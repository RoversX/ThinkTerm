//! Reusable "content view" framework: a GPU-drawn panel that occupies the
//! terminal content area. Some views behave like synthetic tabs (switch /
//! close / preserve state), while thread-owned views can stay driven by the
//! workspace sidebar instead.

use crate::quad::TripleLayerQuadAllocator;
use crate::termwindow::TermWindow;
use crate::ui::{DrawContext, UiPalette};
use mux::pane::PaneId;
use mux::renderable::{RenderableDimensions, StableCursorPosition};
use mux::tab::PositionedSplit;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;
use wezterm_font::LoadedFont;
use wezterm_term::color::ColorPalette;
use wezterm_term::{KeyCode, KeyModifiers, Line, StableRowIndex, TerminalSize};
use window::{MouseEventKind as WMEK, RectF};

pub(crate) type ContentViewId = u64;

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
    pub dimensions: RenderableDimensions,
    pub palette: ColorPalette,
    pub cursor: StableCursorPosition,
}

#[derive(Clone, Debug)]
pub(crate) struct TerminalPreviewRequest {
    pub snapshot: Arc<TerminalPreviewSnapshot>,
    pub area: RectF,
    pub clip: RectF,
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

    /// Terminal tabs to paint as read-only live thumbnails after the view's
    /// regular UI layers have been prepared.
    fn terminal_previews(&self) -> Vec<TerminalPreviewRequest> {
        Vec::new()
    }

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
        cursor_on: bool,
    ) -> anyhow::Result<()>;

    fn on_mouse(&mut self, x: f32, y: f32, kind: WMEK) -> ContentViewResponse;

    fn on_key(&mut self, key: KeyCode, mods: KeyModifiers) -> ContentViewResponse;

    /// Insert pasted text into the focused field, if any.
    fn on_paste(&mut self, _text: &str) -> ContentViewResponse {
        ContentViewResponse::Redraw
    }

    /// Handle the synthetic tab close button / Escape-style close request.
    fn on_close_requested(&mut self) -> ContentViewResponse {
        ContentViewResponse::Close
    }

    /// Handle a folder selected by a native folder picker that was initiated by
    /// this view.
    fn on_folder_picked(&mut self, _path: PathBuf) -> ContentViewResponse {
        ContentViewResponse::Ignored
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
