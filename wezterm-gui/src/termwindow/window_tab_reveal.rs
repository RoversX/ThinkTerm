//! The window tab row when it auto-hides: out of the layout, and brought
//! down from the top edge while the pointer rests there.
//!
//! Away, the row gives its height back to the terminal. Brought down, it
//! comes in over the window's top and pushes the terminal area down by the
//! same amount -- painted lower, never resized, so nothing reflows and the
//! last line or two drop out of view until it goes again. The hover is the
//! workspace sidebar's machine (`SidebarHoverReveal`): the same dwell, grace
//! and travel, fed a strip along the top instead of the left edge.
//!
//! macOS keeps its traffic lights while the row is away. Elsewhere the row
//! holds the window's own buttons and the sidebars' toggles, so the pane
//! bars along the top hold them instead, and their blank moves the window.

use crate::native_settings::NativeWindowTabs;
use crate::quad::{HeapQuadAllocator, QuadClipRect, TripleLayerQuadAllocator};
use crate::termwindow::sidebar_hover::{HoverFrame, HoverInput, PointerZone, SidebarHoverReveal};
use crate::termwindow::ui::tokens::{
    WINDOW_RESIZE_EDGE_HEIGHT, WINDOW_TAB_REVEAL_EDGE_OVER_CONTROLS_HEIGHT,
    WINDOW_TAB_REVEAL_HOT_ZONE_HEIGHT, WINDOW_TAB_REVEAL_STICKY_ZONE_HEIGHT,
};
use crate::termwindow::{
    InlineTabRenameTarget, PaneNavAction, TermWindow, TermWindowNotif, UIItem, UIItemType,
};
use mux::tab::PositionedPane;
use mux::Mux;
use std::time::Instant;
use window::{MouseEvent, WindowDecorations, WindowOps, WindowState};

#[derive(Debug, Clone, Default)]
pub(crate) struct WindowTabReveal {
    mode: NativeWindowTabs,
    pinned: bool,
    /// A thread's own window: the row hides whatever Settings say, so the
    /// pane bar shares the traffic lights' row.
    always_hiding: bool,
    hover: SidebarHoverReveal,
    /// How far out the row is, 0 to 1, as of the last step: what geometry
    /// reads between steps, so a frame and the clicks on it agree.
    shown: f32,
    /// How far the dwell has counted, likewise.
    arming: Option<f32>,
    /// Where the pane bars along the top held what the row would, in the
    /// last frame, and so far in this one.
    held: PaneBarsHeld,
    holding: PaneBarsHeld,
}

impl WindowTabReveal {
    pub(crate) fn new() -> Self {
        let (mode, pinned) = crate::native_settings::window_tabs_choice();
        Self {
            mode,
            pinned,
            ..Default::default()
        }
    }

    pub(crate) fn always_hiding() -> Self {
        Self {
            mode: NativeWindowTabs::AutoHide,
            pinned: false,
            always_hiding: true,
            ..Default::default()
        }
    }

    /// The row keeps its place in the layout.
    pub(crate) fn in_layout(&self) -> bool {
        row_in_layout(self.mode, self.pinned)
    }

    /// The row carries its pin: it auto-hides, pinned or not.
    pub(crate) fn shows_pin(&self) -> bool {
        self.mode == NativeWindowTabs::AutoHide && !self.always_hiding
    }

    pub(crate) fn pinned(&self) -> bool {
        self.pinned
    }

    /// How far out the row is, 0 to 1.
    pub(crate) fn shown(&self) -> f32 {
        self.shown
    }

    /// A dwell, a grace or a travel is under way.
    pub(crate) fn needs_frames(&self) -> bool {
        self.hover.needs_frames()
    }

    /// How far the dwell has counted while the pointer rests in the strip,
    /// 0 to 1, as of the last step.
    pub(crate) fn arming(&self) -> Option<f32> {
        self.arming
    }

    /// Focus loss: put the row away without a travel.
    pub(crate) fn cancel(&mut self) {
        self.hover.cancel_immediately();
        self.shown = 0.0;
        self.arming = None;
    }

    /// A paint pass begins: what its pane bars hold is gathered afresh, and
    /// the pointer goes by the last one's until it is done.
    pub(crate) fn begin_paint(&mut self) {
        self.holding = PaneBarsHeld::default();
    }

    /// A paint pass is done: the pointer goes by what its pane bars held.
    pub(crate) fn finish_paint(&mut self) {
        self.held = self.holding;
    }

    /// The top-right pane bar holds the row's buttons from `left` to its
    /// end, down to `bottom`.
    pub(crate) fn note_window_chrome(&mut self, left: usize, bottom: usize) {
        self.holding.window_chrome = Some((left, bottom));
    }

    /// The top-left pane bar holds the sidebar's toggle up to `right`, down
    /// to `bottom`.
    pub(crate) fn note_sidebar_toggle(&mut self, right: usize, bottom: usize) {
        self.holding.sidebar_toggle = Some((right, bottom));
    }
}

/// Where the pane bars along the window's top hold what the row would, in
/// window pixels.
#[derive(Debug, Clone, Copy, Default)]
struct PaneBarsHeld {
    window_chrome: Option<(usize, usize)>,
    sidebar_toggle: Option<(usize, usize)>,
}

impl PaneBarsHeld {
    /// The pointer at `x`, `y` is over what they hold: on its way to a
    /// button, not to the row.
    fn covers(self, x: isize, y: isize) -> bool {
        if x < 0 || y < 0 {
            return false;
        }
        let (x, y) = (x as usize, y as usize);
        self.window_chrome
            .is_some_and(|(left, bottom)| x >= left && y < bottom)
            || self
                .sidebar_toggle
                .is_some_and(|(right, bottom)| x < right && y < bottom)
    }
}

/// What the top-right pane bar takes over from a row that is away.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct PaneBarWindowChrome {
    pub(crate) right_sidebar_toggle: bool,
    pub(crate) window_buttons: bool,
}

impl PaneBarWindowChrome {
    pub(crate) fn any(self) -> bool {
        self.right_sidebar_toggle || self.window_buttons
    }
}

/// Whether the row takes its height in the layout: shown, or auto-hiding
/// with its pin in.
fn row_in_layout(mode: NativeWindowTabs, pinned: bool) -> bool {
    match mode {
        NativeWindowTabs::Show => true,
        NativeWindowTabs::AutoHide => pinned,
    }
}

impl TermWindow {
    /// The height the window tab row takes in the layout: all of it while it
    /// keeps its place, none while it hides. A revealed row takes none
    /// either -- see `window_tab_reveal_offset`.
    pub(crate) fn window_tab_row_layout_height(&self) -> f32 {
        if self.window_tab_reveal.in_layout() {
            self.tab_bar_pixel_height().unwrap_or(0.0)
        } else {
            0.0
        }
    }

    /// How far a row brought down from the top edge has pushed the terminal
    /// area down. Painting and pointer mapping add it; the terminal's size
    /// never does.
    pub(crate) fn window_tab_reveal_offset(&self) -> f32 {
        if self.window_tab_reveal.in_layout() {
            return 0.0;
        }
        self.window_tab_reveal.shown * self.tab_bar_pixel_height().unwrap_or(0.0)
    }

    /// How much of the window's top the row covers as it stands: all of its
    /// height while it keeps its place, as far as it has come down otherwise.
    pub(crate) fn window_tab_row_drawn_height(&self) -> f32 {
        self.window_tab_row_layout_height() + self.window_tab_reveal_offset()
    }

    /// The row is drawn where it always is, with live buttons: it keeps its
    /// place, or it has come all the way down.
    pub(crate) fn window_tab_row_in_place(&self) -> bool {
        self.window_tab_reveal.in_layout() || self.window_tab_reveal.shown >= 1.0
    }

    /// The row is drawn at all this frame: in place, or travelling.
    pub(crate) fn window_tab_row_drawn(&self) -> bool {
        self.window_tab_row_in_place() || self.window_tab_reveal.shown > 0.0
    }

    /// While the row travels: how far above its place it is drawn, as a
    /// negative shift. `None` when it is in place or away.
    pub(crate) fn window_tab_row_travel_shift(&self) -> Option<f32> {
        let shown = self.window_tab_reveal.shown;
        if self.window_tab_row_in_place() || shown <= 0.0 {
            return None;
        }
        Some((shown - 1.0) * self.tab_bar_pixel_height().unwrap_or(0.0))
    }

    /// Paint the row `shift` pixels above its place, cropped to the window:
    /// recorded where it sits, since the painters take unsigned coordinates,
    /// and moved on replay. Nothing on it takes a click until it lands, and
    /// the one target returned keeps a click there from falling through to
    /// the terminal's first row instead.
    pub(crate) fn paint_window_tab_row_travelling(
        &mut self,
        layers: &mut TripleLayerQuadAllocator,
        shift: f32,
    ) -> anyhow::Result<Vec<UIItem>> {
        let mut recorded = HeapQuadAllocator::default();
        let mut row_layers = TripleLayerQuadAllocator::Heap(&mut recorded);
        self.paint_fancy_tab_bar(&mut row_layers, false)?;
        drop(row_layers);
        let clip = QuadClipRect::from_top_left_pixels(
            0.0,
            0.0,
            self.dimensions.pixel_width as f32,
            self.dimensions.pixel_height as f32,
            &self.dimensions,
        );
        recorded.apply_to_clipped_at(layers, 0.0, shift, clip, 1.0)?;
        let left = self.tab_bar_left_edge();
        Ok(vec![UIItem {
            x: left,
            y: self.get_os_border().top.get() as usize,
            width: (self.terminal_viewport_right() as usize).saturating_sub(left),
            height: self.window_tab_reveal_offset().ceil() as usize,
            item_type: UIItemType::WindowTabsTravelling,
        }])
    }

    /// Off macOS, while the row auto-hides, the pane bars along the window's
    /// top keep room for what it holds that has nowhere else to be -- the
    /// window's buttons and the sidebars' toggles -- as macOS keeps its
    /// traffic lights: all of it while the row is away, none once it is
    /// down, in step as it comes. 0 where they keep none.
    pub(crate) fn pane_bars_stand_in_share(&self) -> f32 {
        if cfg!(target_os = "macos")
            || self.window_tab_reveal.in_layout()
            || self.tab_bar_pixel_height().unwrap_or(0.0) <= 0.0
        {
            return 0.0;
        }
        1.0 - self.window_tab_reveal.shown
    }

    /// The row is away and the pane bars along the top draw what they keep
    /// room for. Once it starts down, it carries its own.
    pub(crate) fn pane_bars_stand_in(&self) -> bool {
        self.pane_bars_stand_in_share() >= 1.0
    }

    /// As `pane_bars_stand_in_share`, for `pos`'s bar starting with the
    /// workspace sidebar's toggle: the one at the top left, wherever the row
    /// would draw one.
    pub(crate) fn pane_bar_sidebar_toggle_share(&self, pos: &PositionedPane) -> f32 {
        let share = self.pane_bars_stand_in_share();
        if share > 0.0
            && pos.left == 0
            && pos.top == 0
            && self.window_tab_shows_sidebar_toggle_action()
        {
            share
        } else {
            0.0
        }
    }

    /// What `pos`'s bar ends with, as the bar at the top right standing in
    /// for the row: the right sidebar's toggle, and the window's own
    /// buttons unless an open sidebar holds them. A bar too narrow for them
    /// all holds none, and the row has to come down for them.
    pub(crate) fn pane_bar_window_chrome(&self, pos: &PositionedPane) -> PaneBarWindowChrome {
        let top_right = pos.top == 0 && pos.left + pos.width >= self.terminal_size.cols;
        if !top_right || self.pane_bars_stand_in_share() <= 0.0 {
            return PaneBarWindowChrome::default();
        }
        let parts = PaneBarWindowChrome {
            right_sidebar_toggle: self.right_sidebar_has_panels(),
            window_buttons: self.fancy_tab_bar_shows_window_buttons()
                && self.right_sidebar_width() == 0,
        };
        let fits = self
            .pane_chrome_span(pos)
            .is_ok_and(|(_, width)| self.pane_bar_fits_window_chrome(pos, parts, width));
        if parts.any() && fits {
            parts
        } else {
            PaneBarWindowChrome::default()
        }
    }

    /// How far down from the window's top its chrome reaches at the right:
    /// the row as it stands, or the pane bar holding what the row would.
    pub(crate) fn window_top_right_chrome_height(&self) -> f32 {
        let bar = self.window_tab_reveal.held.window_chrome;
        self.window_tab_row_drawn_height().max(self.below_window_top(bar))
    }

    /// As `window_top_right_chrome_height`, at the left.
    pub(crate) fn window_top_left_chrome_height(&self) -> f32 {
        let bar = self.window_tab_reveal.held.sidebar_toggle;
        self.window_tab_row_drawn_height().max(self.below_window_top(bar))
    }

    fn below_window_top(&self, held: Option<(usize, usize)>) -> f32 {
        held.map_or(0.0, |(_, bottom)| {
            bottom.saturating_sub(self.get_os_border().top.get() as usize) as f32
        })
    }

    /// `item` is the blank of a pane bar along the window's top, and stands
    /// in for the row's as the title bar: not where the system draws one,
    /// nor in full screen, where there is no window to move, nor on the
    /// strip of a collapsed stack, which runs the window's height.
    pub(crate) fn pane_bar_blank_is_title_bar(&self, item: &UIItem) -> bool {
        let UIItemType::PaneNav {
            pane_id,
            action: PaneNavAction::Background,
            ..
        } = item.item_type
        else {
            return false;
        };
        (self.pane_bars_stand_in() || self.window_tab_reveal.always_hiding)
            && !self.config.window_decorations.contains(WindowDecorations::TITLE)
            && !self
                .window_state
                .intersects(WindowState::FULL_SCREEN | WindowState::SERVER_DECORATED)
            && item.y <= self.get_os_border().top.get() as usize
            && !Mux::get()
                .get_active_tab_for_window(self.mux_window_id)
                .and_then(|tab| tab.pane_stack_id(pane_id))
                .is_some_and(|stack_id| self.collapsed_pane_layouts.contains_key(&stack_id))
    }

    /// A tab of the row is being renamed in place, which needs the row out.
    fn renaming_window_tab(&self) -> bool {
        self.inline_tab_rename
            .as_ref()
            .is_some_and(|rename| matches!(rename.target, InlineTabRenameTarget::WindowTab(_)))
    }

    /// Off macOS, the top edge the system keeps for resizing a window that
    /// draws its own frame. The pointer there never reaches the window, so
    /// the strip that brings the row down reaches past it.
    fn window_tab_resize_edge(&self) -> usize {
        if cfg!(target_os = "macos")
            || self.config.window_decorations.contains(WindowDecorations::TITLE)
            || !self.window_state.can_resize()
        {
            return 0;
        }
        self.ui_px(WINDOW_RESIZE_EDGE_HEIGHT)
    }

    fn window_tab_hover_input(&self) -> HoverInput {
        let overlay_open = self.menu_or_overlay_open();
        let presented = self.window_tab_reveal.hover.is_presented();
        // A thread's own window with one tab has nothing to bring down, and
        // the top of it is what moves the window.
        let lone_tab = self.window_tab_reveal.always_hiding
            && Mux::get()
                .get_window(self.mux_window_id)
                .is_some_and(|window| window.len() < 2);
        // A row out for a rename goes back the usual way once it is done,
        // whatever the mode. Unlike the sidebars, a terminal that is
        // connecting or offline does not hold the row back: it is the way
        // to another tab.
        let eligible = (!self.window_tab_reveal.in_layout()
            || self.renaming_window_tab()
            || presented)
            && !lone_tab
            && self.tab_bar_pixel_height().unwrap_or(0.0) > 0.0
            && !self.content_view_is_full_window()
            && (!overlay_open || presented);
        let pointer = match &self.current_mouse_event {
            None => PointerZone::Away,
            Some(event) => {
                let x = event.coords.x;
                let y = event.coords.y;
                let top = self.get_os_border().top.get() as isize;
                let left = self.tab_bar_left_edge() as isize;
                let right = self.terminal_viewport_right() as isize;
                let row_bottom = top + self.window_tab_reveal_offset().ceil() as isize;
                let edge = top + self.window_tab_resize_edge() as isize;
                let below = |height: usize| y < edge + self.ui_px(height) as isize;
                if x < left || x >= right {
                    PointerZone::Away
                } else if presented && y < row_bottom {
                    PointerZone::Panel
                } else if self.window_tab_reveal.held.covers(x, y)
                    || !below(WINDOW_TAB_REVEAL_STICKY_ZONE_HEIGHT)
                {
                    PointerZone::Away
                } else if self.pointer_on_pane_bar_control(event) {
                    if below(WINDOW_TAB_REVEAL_EDGE_OVER_CONTROLS_HEIGHT) {
                        PointerZone::HotZone
                    } else {
                        PointerZone::Away
                    }
                } else if below(WINDOW_TAB_REVEAL_HOT_ZONE_HEIGHT) {
                    PointerZone::HotZone
                } else {
                    PointerZone::NearHotZone
                }
            }
        };
        let pinned = !self.current_mouse_buttons.is_empty()
            || self.current_mouse_capture.is_some()
            || self.dragging.is_some()
            || self.pane_tab_drag.is_some()
            || overlay_open;
        HoverInput {
            eligible,
            pointer,
            pinned,
        }
    }

    /// The pointer is over something of a pane bar's it may well be there
    /// for rather than for the row, as the last frame laid them out: a tab,
    /// a button, or a blank standing in for the title bar.
    fn pointer_on_pane_bar_control(&self, event: &MouseEvent) -> bool {
        self.resolve_ui_item(event).is_some_and(|item| match item.item_type {
            UIItemType::PaneNav {
                action: PaneNavAction::Background,
                ..
            } => self.pane_bar_blank_is_title_bar(&item),
            UIItemType::PaneNav { .. } => true,
            _ => false,
        })
    }

    /// Step the row's hover machine and keep its position for the frame.
    /// From the frame loop, so a resting pointer still brings it down at
    /// dwell expiry and a departed one still sends it back at grace expiry.
    pub(crate) fn advance_window_tab_reveal(&mut self, now: Instant) {
        let input = self.window_tab_hover_input();
        let frame = self.window_tab_reveal.hover.step(input, now);
        self.window_tab_reveal.shown = self.window_tab_reveal.hover.progress(now).unwrap_or(0.0);
        self.window_tab_reveal.arming = self.window_tab_reveal.hover.arming_progress(now);
        match frame {
            HoverFrame::None => {}
            HoverFrame::Now => {
                if let Some(window) = self.window.as_ref() {
                    window.invalidate();
                }
            }
            HoverFrame::At(due) => self.update_next_frame_time(Some(due)),
        }
        // The hint along the top fades up as the dwell counts, which takes
        // every frame of it: a pointer only passing through barely shows it.
        if self.window_tab_reveal.arming.is_some() {
            if let Some(window) = self.window.as_ref() {
                window.invalidate();
            }
        }
    }

    /// Feed a pointer event to the row's hover machine. Never consumes it.
    pub(crate) fn update_window_tab_reveal(&mut self, context: &dyn WindowOps) {
        let input = self.window_tab_hover_input();
        if self.window_tab_reveal.hover.step(input, Instant::now()) != HoverFrame::None {
            // As with the sidebar: the frame's advance registers deadlines.
            context.invalidate();
        }
    }

    /// Take a new mode or pin from Settings: the terminal gets the row's
    /// height back, or gives it up, once.
    pub(crate) fn refresh_window_tabs(&mut self) {
        if self.window_tab_reveal.always_hiding {
            return;
        }
        let before = self.window_tab_reveal.in_layout();
        let (mode, pinned) = crate::native_settings::window_tabs_choice();
        self.window_tab_reveal.mode = mode;
        self.window_tab_reveal.pinned = pinned;
        if self.window_tab_reveal.in_layout() {
            self.window_tab_reveal.cancel();
        }
        if before != self.window_tab_reveal.in_layout() {
            self.invalidate_fancy_tab_bar();
            if let Some(window) = self.window.as_ref().cloned() {
                let dimensions = self.dimensions;
                self.apply_dimensions(&dimensions, None, &window);
            }
        }
        if let Some(window) = self.window.as_ref() {
            window.invalidate();
        }
    }

    /// A row tab renamed from the keyboard or the palette while the row is
    /// away: bring the row down for the edit, which holds it out until done.
    pub(crate) fn reveal_window_tab_row_for_rename(&mut self) {
        if self.window_tab_reveal.in_layout() || !self.renaming_window_tab() {
            return;
        }
        self.window_tab_reveal.hover.reveal_now(Instant::now());
        if let Some(window) = self.window.as_ref() {
            window.invalidate();
        }
    }

    /// The pin: keep an auto-hiding row out, or let it go again. Saved, so
    /// every window follows and the next launch remembers.
    pub(crate) fn toggle_window_tabs_pinned(&mut self) {
        let pinned = !self.window_tab_reveal.pinned;
        if let Err(err) = crate::native_settings::save_window_tabs_pinned(pinned) {
            log::warn!("failed to save the window tab row's pin: {err:#}");
            return;
        }
        // This window at once: a frame painted before the notice below
        // reaches it would still go by the old pin.
        self.refresh_window_tabs();
        if !pinned {
            // Unpinned under the pointer: it stays out until the pointer
            // leaves, as a row brought down by hover would.
            self.window_tab_reveal.hover.present_immediately();
            self.window_tab_reveal.shown = 1.0;
        }
        if let Some(front_end) = crate::frontend::try_front_end() {
            for gui_window in front_end.gui_windows() {
                gui_window.window.notify(TermWindowNotif::Apply(Box::new(|term_window| {
                    term_window.refresh_window_tabs();
                })));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_buttons_a_pane_bar_holds_do_not_bring_the_row_down() {
        let held = PaneBarsHeld {
            window_chrome: Some((600, 30)),
            sidebar_toggle: Some((40, 30)),
        };
        assert!(held.covers(700, 10));
        assert!(held.covers(20, 10));
        assert!(!held.covers(300, 10));
        assert!(!held.covers(700, 30));
        assert!(!held.covers(-1, 10));
        assert!(!PaneBarsHeld::default().covers(700, 10));
    }

    #[test]
    fn only_a_shown_or_pinned_row_keeps_its_place() {
        assert!(row_in_layout(NativeWindowTabs::Show, false));
        assert!(row_in_layout(NativeWindowTabs::AutoHide, true));
        assert!(!row_in_layout(NativeWindowTabs::AutoHide, false));
    }
}
