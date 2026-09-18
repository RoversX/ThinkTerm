//! Reusable "content view" framework: a GPU-drawn panel that occupies the
//! terminal content area. Some views behave like synthetic tabs (switch /
//! close / preserve state), while thread-owned views can stay driven by the
//! workspace sidebar instead.

use crate::quad::TripleLayerQuadAllocator;
use crate::termwindow::TermWindow;
use crate::ui::{DrawContext, UiPalette};
use mux::pane::PaneId;
use mux::renderable::{RenderableDimensions, StableCursorPosition};
use mux::tab::{PositionedSplit, TabId};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};
use wezterm_font::LoadedFont;
use wezterm_term::color::ColorPalette;
use wezterm_term::{KeyCode, KeyModifiers, Line, StableRowIndex, TerminalSize};
use window::{MouseEvent, MouseEventKind as WMEK, RectF};

pub(crate) type ContentViewId = u64;

/// A full-window ContentView arriving or leaving, composited over the terminal
/// while it travels.
///
/// The terminal keeps being painted for the duration -- normally a foreground
/// ContentView suppresses it entirely -- so the two are on screen together and
/// the view can be faded against something rather than against nothing.
pub(crate) struct ContentViewFade {
    /// How present the view is: rising as it arrives, falling as it leaves.
    pub(crate) opacity: crate::ui::anim::Timeline,
    /// How far the terminal has travelled towards the place the view keeps for
    /// it: 0 is the whole window, 1 is that place. Separate from `opacity`
    /// because movement decelerates into its target while opacity does not.
    pub(crate) travel: crate::ui::anim::Timeline,
    /// The outgoing view's last painted frame.
    ///
    /// A closing view is torn down immediately: deferring that would mean
    /// keeping something alive that still answers to input and to the mux
    /// while it is on its way out. Keeping its final quads instead lets the
    /// picture leave without the view having to.
    pub(crate) ghost: Option<crate::quad::HeapQuadAllocator>,
    /// The terminal, recorded once, and where it is going.
    pub(crate) flight: Option<ContentViewFlight>,
    /// How far the window frame has moved off its own edges: 0 in place, 1
    /// fully gone. Separate from the terminal's travel so the two can be
    /// staggered -- the frame is meant to be seen leaving before the terminal
    /// starts crossing, and to come back after it has landed.
    pub(crate) chrome_travel: crate::ui::anim::Timeline,
    /// The window frame, recorded once, in the three pieces that leave in
    /// three different directions.
    pub(crate) chrome: Option<ContentViewChrome>,
    /// Destination captured before the view was torn down.
    ///
    /// A closing view cannot be asked where the terminal belongs -- it is
    /// already gone by the time the flight is resolved -- so a close records
    /// the answer while it still can.
    pub(crate) pending_destination: Option<RectF>,
    /// The travelling terminal dissolving into the card's own thumbnail of it,
    /// started once the two are close enough in size to overlap. `None` until
    /// then, and for the whole of a closing transition -- growing back out of
    /// a card, there is nothing on the far side to dissolve into.
    pub(crate) landing: Option<crate::ui::anim::Timeline>,
    /// When this transition began, in wall-clock time.
    ///
    /// Only used to bound how long input may be held; the animation itself is
    /// paced by the timelines above, which run on a deferred clock.
    pub(crate) started_at: Instant,
}

/// How long a running transition may hold input before it is assumed stuck.
///
/// The longest piece of the animation is 260ms, and its clock does not start
/// until a frame has been presented. This ceiling is well clear of that while
/// still bounded, because the fade is only ever cleared by painting: a window
/// that stops painting mid-transition -- occluded, or on a display that has
/// gone to sleep -- would otherwise hold input forever.
const TRANSITION_INPUT_HOLD: Duration = Duration::from_millis(1000);

/// Whether a transition that began `elapsed` ago still owns the keyboard and
/// the pointer.
///
/// While a transition runs, what is on screen is a recording: the terminal on
/// its way behind an arriving view, or a view's last frame on its way out.
/// Both look interactive and neither is.
pub(crate) fn transition_holds_input(elapsed: Duration) -> bool {
    elapsed < TRANSITION_INPUT_HOLD
}

#[cfg(test)]
mod flight_rect_tests {
    use super::*;

    fn source() -> RectF {
        euclid::rect(0.0, 40.0, 1600.0, 900.0)
    }

    fn destination() -> RectF {
        euclid::rect(300.0, 600.0, 400.0, 225.0)
    }

    #[test]
    fn the_journey_starts_where_the_terminal_was_recorded() {
        assert_eq!(flight_rect_at(source(), destination(), 0.0), source());
    }

    #[test]
    fn the_journey_ends_in_the_card() {
        assert_eq!(flight_rect_at(source(), destination(), 1.0), destination());
    }

    #[test]
    fn travel_is_clamped_at_both_ends() {
        assert_eq!(flight_rect_at(source(), destination(), -0.5), source());
        assert_eq!(flight_rect_at(source(), destination(), 4.0), destination());
    }

    #[test]
    fn halfway_is_halfway_on_every_edge() {
        let mid = flight_rect_at(source(), destination(), 0.5);
        assert_eq!(mid.min_x(), 150.0);
        assert_eq!(mid.min_y(), 320.0);
        assert_eq!(mid.size.width, 1000.0);
        assert_eq!(mid.size.height, 562.5);
    }

    #[test]
    fn the_dissolve_waits_until_the_sizes_nearly_match() {
        // Shrinking 1600 wide into a 400 wide card: every 1% of journey left
        // is 3% of oversize, so the bound is reached at travel 0.96.
        let (src, dst) = (source(), destination());
        assert!(!flight_is_landing(src, dst, 0.0));
        assert!(!flight_is_landing(src, dst, 0.95));
        assert!(flight_is_landing(src, dst, 0.96));
        assert!(flight_is_landing(src, dst, 1.0));
    }

    #[test]
    fn a_smaller_card_has_to_be_approached_more_closely() {
        // Same journey into a card half the size: the journey left that was
        // 12% of oversize is 28% of this one, so 0.96 is no longer close
        // enough. The window stays roughly the same length in time because
        // the travel eases out.
        let src = source();
        let small = euclid::rect(300.0, 600.0, 200.0, 112.5);
        assert!(!flight_is_landing(src, small, 0.96));
        assert!(flight_is_landing(src, small, 0.99));
    }

    #[test]
    fn a_card_that_moved_mid_flight_retargets_from_the_same_source() {
        // The overview reflowed underneath the terminal: same recording, new
        // landing rectangle. The arriving picture has to follow it rather than
        // finish at the old one and jump.
        let moved = euclid::rect(900.0, 100.0, 400.0, 225.0);
        assert_eq!(flight_rect_at(source(), moved, 1.0), moved);
        assert_eq!(flight_rect_at(source(), moved, 0.0), source());
    }
}

#[cfg(test)]
mod transition_input_tests {
    use super::*;

    #[test]
    fn input_is_held_for_the_length_of_the_animation() {
        // The travel is 260ms and its clock waits for a presented frame.
        assert!(transition_holds_input(Duration::ZERO));
        assert!(transition_holds_input(Duration::from_millis(260)));
        assert!(transition_holds_input(Duration::from_millis(500)));
    }

    #[test]
    fn a_transition_that_stopped_painting_gives_input_back() {
        assert!(!transition_holds_input(TRANSITION_INPUT_HOLD));
        assert!(!transition_holds_input(Duration::from_secs(30)));
    }
}

/// The window frame recorded per edge, because each piece leaves towards the
/// edge it is anchored to rather than all of them fading in place.
#[derive(Default)]
pub(crate) struct ContentViewChrome {
    pub(crate) left: crate::quad::HeapQuadAllocator,
    pub(crate) right: crate::quad::HeapQuadAllocator,
    pub(crate) top: crate::quad::HeapQuadAllocator,
}

/// The terminal as it looked when a transition began, travelling to the place
/// the arriving view keeps for it.
///
/// Recorded once rather than repainted per frame: the terminal is not
/// interactive during the transition, so there is nothing for a live repaint
/// to show that a still frame cannot. The cost is that its glyphs are sampled
/// below the size they were rasterised at, so the text softens as it shrinks
/// -- which is what a window pulling away from you looks like anyway.
pub(crate) struct ContentViewFlight {
    pub(crate) surface: crate::quad::HeapQuadAllocator,
    /// Where the terminal was when `surface` was recorded.
    ///
    /// Frozen rather than re-read each frame. These quads carry the positions
    /// they were authored at, so remapping them from anywhere but the geometry
    /// they were authored under scales and offsets them wrongly -- a window
    /// resized, or a sidebar toggled, mid-flight used to stretch the picture.
    pub(crate) source: RectF,
    /// Destination in window pixels. Equal to the whole window when the view
    /// has no place for this terminal, which turns the flight into a plain
    /// hold and leaves the transition to the fade alone.
    ///
    /// A fallback rather than the answer: an arriving view is asked again on
    /// every frame, because its layout keeps moving while the terminal is on
    /// its way there. This value is what a *departing* view left behind, and
    /// there is nobody left to ask.
    pub(crate) destination: RectF,
    /// Which terminal is travelling, so the arriving view can be re-asked
    /// where it now keeps a place for it.
    pub(crate) tab_id: Option<TabId>,
}

/// How much larger than its card the travelling terminal may still be when the
/// two are first allowed to overlap.
///
/// Loose, because the overlap itself is cheap: the eased opacity is still flat
/// at 1.0 for the first quarter of the dissolve, and by the time the recording
/// is visibly translucent the mismatch is already under 1%. What made the
/// earlier attempt read as two pictures was not the mismatch on its own but
/// the flat panel colour showing between them -- the card drew no thumbnail
/// while its terminal was in flight, so there was nothing to dissolve into.
/// With an aligned copy of the same picture underneath, a few percent of size
/// difference is not what the eye is looking at.
///
/// Tightening this is what made the first version invisible: 3% left about two
/// usable frames, which is a cut with extra steps.
const FLIGHT_LANDING_OVERSIZE: f32 = 0.12;

/// Whether the travelling terminal has closed to within touching distance of
/// the card it is landing in.
///
/// Measured in size, not in time and not in distance already covered. The
/// travel eases out, so a window written in either of those spends most of
/// itself at a size that does not match the card -- a threshold at "the last
/// tenth of the distance" is nearly half of the duration, which is exactly how
/// the previous attempt ended up translucent for seven frames at a size 3-18%
/// off. Bounding the mismatch instead makes the window short as a
/// *consequence*: 40-65ms across the range of card sizes, and self-correcting,
/// because a smaller card has to be approached more closely to reach the same
/// proportional error.
pub(crate) fn flight_is_landing(source: RectF, destination: RectF, travel: f32) -> bool {
    let rect = flight_rect_at(source, destination, travel);
    let mismatch = |reached: f32, target: f32| {
        if target <= 0.0 {
            f32::INFINITY
        } else {
            (reached - target).abs() / target
        }
    };
    mismatch(rect.size.width, destination.size.width)
        .max(mismatch(rect.size.height, destination.size.height))
        <= FLIGHT_LANDING_OVERSIZE
}

/// Where the travelling terminal sits at `travel`, between the rectangle it
/// was recorded in and the one it is heading for.
pub(crate) fn flight_rect_at(source: RectF, destination: RectF, travel: f32) -> RectF {
    let travel = travel.clamp(0.0, 1.0);
    let lerp = |from: f32, to: f32| from + (to - from) * travel;
    euclid::rect(
        lerp(source.min_x(), destination.min_x()),
        lerp(source.min_y(), destination.min_y()),
        lerp(source.size.width, destination.size.width),
        lerp(source.size.height, destination.size.height),
    )
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum ContentViewPresentation {
    #[default]
    ContentArea,
    FullWindow,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum ContentViewTypography {
    #[default]
    Default,
    Overview,
}

/// Immutable terminal state captured once by the owning ContentView.  The
/// renderer consumes this value without looking the tab up in the mux again,
/// so a transient detach cannot turn a card into a blank rectangle halfway
/// through a frame.
#[derive(Clone, Debug)]
pub(crate) struct TerminalPreviewSnapshot {
    pub tab_size: TerminalSize,
    pub panes: Vec<TerminalPreviewPaneSnapshot>,
    pub splits: Vec<PositionedSplit>,
    /// Whether every row copied was a row the pane had. A pane that fetches
    /// its rows on demand hands out blank placeholders for the ones still
    /// in flight, and a picture holding any of those is not the terminal
    /// yet: it is kept only until one that is can be taken.
    pub complete: bool,
}

impl TerminalPreviewSnapshot {
    /// Whether anything in the picture would be visible: at least one row
    /// with more than whitespace on it.
    pub fn has_content(&self) -> bool {
        self.panes
            .iter()
            .any(|pane| pane.lines.iter().any(|line| !line.is_whitespace()))
    }
}

#[derive(Clone, Debug)]
pub(crate) struct TerminalPreviewPaneSnapshot {
    pub pane_id: PaneId,
    pub is_active: bool,
    pub left: usize,
    pub top: usize,
    pub width: usize,
    pub height: usize,
    pub cols: usize,
    pub rows: usize,
    pub resolved_top: StableRowIndex,
    pub lines: Vec<Line>,
    /// Height of the pane's whole box, nav bar included. `dimensions` covers
    /// the grid alone, so the difference is what the real terminal reserves
    /// above the first row -- which a thumbnail has to reserve too, or it
    /// draws the same rows into a taller space.
    pub box_pixel_height: usize,
    pub dimensions: RenderableDimensions,
    pub palette: ColorPalette,
    pub cursor: StableCursorPosition,
}

#[derive(Clone, Debug)]
pub(crate) struct TerminalPreviewRequest {
    /// Which card this thumbnail belongs to. The renderer keeps one cached
    /// quad heap per card and needs a stable identity to find it again.
    pub tab_id: TabId,
    pub snapshot: Arc<TerminalPreviewSnapshot>,
    pub area: RectF,
    pub clip: RectF,
    /// The card is being resized right now, so hold whatever font scale was
    /// already chosen rather than picking one for this exact size. Scales are
    /// bucketed to bound how many `FontConfiguration`s exist, but a drag sweeps
    /// the card through a bucket every few pixels, and each new bucket builds a
    /// font and rasterises a glyph set. Holding still for the duration costs a
    /// thumbnail that is briefly a little small.
    pub hold_scale: bool,
    /// How opaque to draw it, 0 to 1. A card changing tab draws the old
    /// picture fading out under the new one fading in.
    pub opacity: f32,
}

/// Progress of an SSH connection that a content view kicked off, pushed by the
/// owning `TermWindow` while it polls the remote domain. Only the remote-thread
/// view consumes it; `Authenticating`/`Connected` are not pushed because at that
/// point the view is closed and the real terminal is revealed instead.
#[derive(Clone, Debug)]
pub(crate) enum RemoteConnectPhase {
    Connecting,
    Failed { message: String },
}

/// Result of re-checking a Project directory that a content view reported as
/// unusable, pushed by the owning `TermWindow` once the probe comes back off
/// the worker thread. Only the project-root view consumes it.
#[derive(Clone, Debug)]
pub(crate) enum ProjectRootProbe {
    /// The directory lists again: the user granted the access, remounted the
    /// volume, or put the folder back.
    Available,
    /// Still unusable, with a freshly classified reason -- which may differ
    /// from the original one.
    Blocked(crate::termwindow::ui::folder_problem::ProjectRootUnavailable),
    /// The user picked some *other* folder in the re-authorization panel;
    /// the Project is never repointed on the strength of that, the view just
    /// says what happened.
    OtherFolderChosen,
}

/// What `TermWindow` should do after a content view handled an input event.
pub(crate) enum ContentViewResponse {
    /// Event not consumed by the view.
    Ignored,
    /// Repaint the window.
    Redraw,
    /// Close the content view (remove its synthetic tab).
    Close,
    /// Run a callback back on the owning `TermWindow` (e.g. connect SSH).
    Run(Box<dyn FnOnce(&mut TermWindow)>),
}

/// One entry of the Remote Hosts page's card menu. Lives here rather than in
/// the page because the window is what dispatches it: a native menu reports
/// back to the window, not to the view that asked for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RemoteHostCommand {
    Connect,
    Edit,
    Duplicate,
    Delete,
}

pub(crate) trait ContentView {
    /// Title shown on the synthetic tab.
    fn title(&self) -> String;

    /// Whether this view should appear in the window's top tab bar.
    fn show_in_tab_bar(&self) -> bool {
        true
    }

    /// How much of the native window this view owns while it is foreground.
    /// Full-window views preserve the OS title bar but temporarily suppress
    /// ThinkTerm's tab bar and sidebars without changing their saved state.
    fn presentation(&self) -> ContentViewPresentation {
        ContentViewPresentation::ContentArea
    }

    /// Paint the background for the complete client surface before the view's
    /// safe content area is laid out. Full-window views can use this to blend
    /// through the native title-bar region without placing controls beneath
    /// the traffic lights.
    fn paint_surface_background(
        &mut self,
        _ctx: &DrawContext,
        _layers: &mut TripleLayerQuadAllocator<'_>,
        _surface: RectF,
        _palette: UiPalette,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    /// Select a native chrome font role rather than inventing view-local font
    /// sizes.  Overview uses the same sidebar, pane-header and settings roles
    /// as the rest of ThinkTerm.
    fn typography(&self) -> ContentViewTypography {
        ContentViewTypography::Default
    }

    /// Stable key used to activate an existing tab instead of opening a duplicate.
    fn tab_key(&self) -> Option<String> {
        None
    }

    /// Space id that owns this synthetic tab. `None` means the view is global
    /// and remains visible while switching Spaces.
    fn space_id(&self) -> Option<&str> {
        None
    }

    /// Called when a deduplicated or background synthetic tab is explicitly
    /// brought back to the foreground by user action.
    fn on_reactivated(&mut self) -> ContentViewResponse {
        ContentViewResponse::Ignored
    }

    /// Push SSH connection progress into a view that initiated a connection.
    /// Default is a no-op; only the remote-thread view reacts.
    fn on_remote_connect_phase(&mut self, _phase: RemoteConnectPhase) {}

    /// Send an already-open hosts page straight to its blank host form.
    /// The menu entry that leads here is worded as an action, so landing on
    /// the list would leave the user a click short of what they asked for.
    /// Default is a no-op; only the SSH hosts view reacts.
    fn begin_new_remote_host(&mut self) {}

    /// Run one entry of that page's card menu. Default is a no-op; only the
    /// remote hosts page reacts.
    fn run_remote_host_command(
        &mut self,
        _host_id: &str,
        _command: RemoteHostCommand,
    ) -> ContentViewResponse {
        ContentViewResponse::Ignored
    }

    /// Gate a re-authorization round trip for the project-root page and
    /// report whether it may start. Default is a no-op refusal; only the
    /// project-root view participates.
    fn begin_project_root_reauthorize(&mut self) -> bool {
        false
    }

    /// Push the outcome of that round trip back into the view that asked.
    fn on_project_root_reauthorize(&mut self, _outcome: ProjectRootProbe) {}

    /// Re-point an existing project-root page at a newer refusal, instead of
    /// opening a second page for the same Thread. Returns false for any other
    /// view type.
    fn replace_project_root_problem(
        &mut self,
        _space_id: String,
        _display_name: String,
        _failure: crate::termwindow::ui::folder_problem::ProjectRootUnavailable,
    ) -> bool {
        false
    }

    /// Whether the view currently wants the cursor-blink animation running
    /// (true only while a text field is focused, to avoid needless repaints).
    fn wants_cursor_blink(&self) -> bool {
        false
    }

    /// Schedule a follow-up repaint for view-local animations.
    fn next_frame_time(&self) -> Option<Instant> {
        None
    }

    /// Inform the active view that the native window is in an interactive
    /// resize. Returning true means the state changed and a final repaint is
    /// required even if the last resize event repeats the same dimensions.
    fn set_live_resizing(&mut self, _live_resizing: bool) -> bool {
        false
    }

    /// Tell the view the shape of the terminal area it is standing in front
    /// of, so a thumbnail can hold the same proportions as the thing it is a
    /// picture of. Pushed every frame rather than fixed at construction: the
    /// window can be resized, a sidebar opened or the tab bar toggled while
    /// the view is up, and a card frozen at the shape the terminal happened to
    /// have when it opened stops matching the terminal it came from.
    fn set_host_preview_aspect(&mut self, _aspect: f32) {}

    /// Terminal tabs to paint as read-only live thumbnails after the view's
    /// regular UI layers have been prepared.
    fn terminal_previews(&self) -> Vec<TerminalPreviewRequest> {
        Vec::new()
    }

    /// Where this view shows `tab_id`'s terminal, if it shows it at all.
    ///
    /// A view that gives a terminal a place of its own can have that terminal
    /// travel to it rather than being replaced by it. `None` means there is no
    /// destination and the view should simply arrive.
    ///
    /// Only meaningful after a paint: the answer comes from a layout, and the
    /// layout is computed while drawing.
    fn terminal_landing_rect(&self, _tab_id: TabId) -> Option<RectF> {
        None
    }

    /// A terminal that is currently travelling to or from its place here, and
    /// so must not also be drawn in it.
    ///
    /// Without this the view shows its own copy of the terminal underneath the
    /// one flying towards it, and the transition reads as two pictures of the
    /// same thing rather than one thing moving.
    fn set_terminal_in_flight(&mut self, _tab_id: Option<TabId>) {}

    /// The opening frame of a full-window transition records the terminal and
    /// grows the atlas; defer thumbnail capture until the next frame so that
    /// work is not stacked onto the click that triggered the transition.
    fn set_defer_preview_captures(&mut self, _defer: bool) {}

    /// Paint masks and chrome that must sit above terminal preview glyphs.
    /// Most ContentViews do not embed terminal snapshots and need no second
    /// pass.
    #[allow(clippy::too_many_arguments)]
    fn paint_after_terminal_previews(
        &mut self,
        _ctx: &DrawContext,
        _layers: &mut TripleLayerQuadAllocator<'_>,
        _area: RectF,
        _palette: UiPalette,
        _font: &Rc<LoadedFont>,
        _title_font: &Rc<LoadedFont>,
        _section_font: &Rc<LoadedFont>,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    /// Whether output from this pane should invalidate the view. Normal
    /// content views do not display panes; Live Overview opts in only for the
    /// previews currently intersecting its viewport.
    fn wants_pane_output(&self, _pane_id: PaneId) -> bool {
        false
    }

    /// Paint the view into `area` (the terminal content rect). `cursor_on` is
    /// the current blink phase for any focused caret.
    fn paint(
        &mut self,
        ctx: &DrawContext,
        layers: &mut TripleLayerQuadAllocator<'_>,
        area: RectF,
        palette: UiPalette,
        font: &Rc<LoadedFont>,
        title_font: &Rc<LoadedFont>,
        section_font: &Rc<LoadedFont>,
        caption_font: &Rc<LoadedFont>,
        cursor_on: bool,
    ) -> anyhow::Result<()>;

    fn on_mouse(&mut self, x: f32, y: f32, kind: WMEK) -> ContentViewResponse;

    /// Wheel events, with the parts of the event that [`WMEK`] cannot carry.
    ///
    /// A trackpad reports pixels: macOS sends `VertWheel(0)` alongside a
    /// precise delta whenever the gesture has not yet accumulated a whole
    /// line, so a view that reads only the kind either stands still or moves
    /// in whole notches. The momentum phase matters for the same reason --
    /// the system is already supplying the glide, and a view that adds its
    /// own would be integrating it twice.
    ///
    /// Views that do not scroll by pixels can ignore this and keep handling
    /// the coarse event.
    fn on_wheel(&mut self, x: f32, y: f32, event: &MouseEvent) -> ContentViewResponse {
        self.on_mouse(x, y, event.kind.clone())
    }

    fn on_key(&mut self, key: KeyCode, mods: KeyModifiers) -> ContentViewResponse;

    /// Insert pasted text into the focused field, if any.
    fn on_paste(&mut self, _text: &str) -> ContentViewResponse {
        ContentViewResponse::Redraw
    }

    /// Handle the synthetic tab close button / Escape-style close request.
    fn on_close_requested(&mut self) -> ContentViewResponse {
        ContentViewResponse::Close
    }

    /// Text to copy to the clipboard for ⌘C (e.g. the focused field), if any.
    fn copy_text(&self) -> Option<String> {
        None
    }

    /// Text to cut to the clipboard for ⌘X. Views that only support whole-field
    /// selection should return text only when that selection is active.
    fn cut_text(&mut self) -> Option<String> {
        None
    }
}

/// ContentViews defer mux geometry while they own the foreground.  Consume the
/// deferred resize only on the transition back to terminal content; opening
/// and closing a view without an intervening size change is a strict no-op.
pub(crate) fn take_deferred_mux_resize_on_exit(
    deferred: &mut bool,
    was_foreground: bool,
    is_foreground: bool,
) -> bool {
    if was_foreground && !is_foreground && *deferred {
        *deferred = false;
        true
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::take_deferred_mux_resize_on_exit;

    #[test]
    fn deferred_mux_resize_flushes_once_on_terminal_return() {
        let mut deferred = true;
        assert!(!take_deferred_mux_resize_on_exit(&mut deferred, true, true));
        assert!(deferred);
        assert!(take_deferred_mux_resize_on_exit(&mut deferred, true, false));
        assert!(!deferred);
        assert!(!take_deferred_mux_resize_on_exit(
            &mut deferred,
            true,
            false
        ));
    }

    #[test]
    fn closing_unresized_content_view_does_not_touch_mux_geometry() {
        let mut deferred = false;
        assert!(!take_deferred_mux_resize_on_exit(
            &mut deferred,
            true,
            false
        ));
    }
}
