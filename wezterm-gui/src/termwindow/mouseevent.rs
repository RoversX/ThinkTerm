use crate::frontend::front_end;
use crate::tabbar::TabBarItem;
use crate::termwindow::ui::pane_nav_bar_height_for_metrics;
use crate::termwindow::ui::tokens::{
    MACOS_TRAFFIC_LIGHT_CLEARANCE_WIDTH, MACOS_WINDOW_TAB_RESERVED_ACTION_SLOTS,
    PANE_NAV_BUTTON_GAP, PANE_NAV_INSET, PANE_NAV_TAB_GAP, TAB_ROW_START_PADDING,
    TAB_VERTICAL_PADDING, WINDOW_TAB_ACTION_RESERVED_WIDTH,
    WINDOW_TAB_FULLSCREEN_SIDEBAR_BUTTON_SIZE, WINDOW_TAB_FULLSCREEN_SIDEBAR_BUTTON_X,
    WINDOW_TAB_GAP, WINDOW_TAB_LEADING_ACTION_BUTTON_SIZE, WINDOW_TAB_LEADING_ACTION_GAP,
};
use crate::termwindow::{
    GuiWin, MouseCapture, PaneNavAction, PositionedSplit, ScrollHit, TabWheelSurface,
    TermWindowNotif, UIItem, UIItemType, TMB,
};
use ::window::{
    ContextMenuItem, MouseButtons as WMB, MouseCursor, MouseEvent, MouseEventKind as WMEK,
    MousePress, WindowDecorations, WindowOps, WindowState,
};
use config::keyassignment::{
    ClipboardPasteSource, KeyAssignment, MouseEventTrigger, PaneDirection, SpawnCommand,
    SpawnTabDomain, SplitPane, SplitSize,
};
use config::{MouseEventAltScreen, TermConfig};
use mux::pane::{Pane, WithPaneLines};
use mux::tab::{PositionedPane, SplitDirection};
use mux::Mux;
use mux_lua::MuxPane;
use std::convert::TryInto;
use std::ops::Sub;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};
use termwiz::hyperlink::Hyperlink;
use termwiz::surface::Line;
use wezterm_dynamic::ToDynamic;
use wezterm_term::input::{MouseButton, MouseEventKind as TMEK};
use wezterm_term::{ClickPosition, LastMouseClick, StableRowIndex};

const TAB_WHEEL_SURFACE_LOCK_MS: u64 = 700;
const TAB_WHEEL_DIRECTION_LOCK_MS: u64 = 140;

impl super::TermWindow {
    fn tab_scroll_pixels(amount: i16) -> f32 {
        let steps = amount.unsigned_abs().max(1) as f32;
        let delta = (steps * 56.0).min(280.0);
        if amount < 0 {
            delta
        } else {
            -delta
        }
    }

    fn sidebar_scroll_pixels(amount: i16) -> f32 {
        let steps = amount.unsigned_abs().max(1) as f32;
        let delta = (steps * 6.0).min(42.0);
        if amount < 0 {
            delta
        } else {
            -delta
        }
    }

    pub(super) fn window_tab_width_pixels(&self) -> f32 {
        let cell_width = self.render_metrics.cell_size.width.max(1) as f32;
        (self.config.tab_max_width as f32 * cell_width)
            .max(cell_width * 15.0)
            .max(176.0)
            .ceil()
    }

    pub(super) fn window_tab_leading_action_slot_count(&self) -> usize {
        if !self.config.use_fancy_tab_bar || self.workspace_sidebar_width() > 0 {
            return 0;
        }

        if self.window_state.contains(WindowState::FULL_SCREEN) {
            return 1;
        }

        if cfg!(target_os = "macos") {
            return MACOS_WINDOW_TAB_RESERVED_ACTION_SLOTS;
        }

        0
    }

    pub(super) fn window_tab_shows_sidebar_toggle_action(&self) -> bool {
        self.config.use_fancy_tab_bar
            && self.workspace_sidebar_width() == 0
            && self.window_state.contains(WindowState::FULL_SCREEN)
    }

    pub(super) fn window_tab_leading_action_start_pixels(&self) -> f32 {
        if self.window_tab_leading_action_slot_count() == 0 {
            0.0
        } else if self.window_state.contains(WindowState::FULL_SCREEN) {
            WINDOW_TAB_FULLSCREEN_SIDEBAR_BUTTON_X as f32
        } else if cfg!(target_os = "macos") {
            MACOS_TRAFFIC_LIGHT_CLEARANCE_WIDTH as f32
        } else {
            0.0
        }
    }

    pub(super) fn window_tab_leading_action_area_width_pixels(&self) -> f32 {
        let count = self.window_tab_leading_action_slot_count();
        if count == 0 {
            0.0
        } else {
            let button_size = if self.window_tab_shows_sidebar_toggle_action() {
                WINDOW_TAB_FULLSCREEN_SIDEBAR_BUTTON_SIZE
            } else {
                WINDOW_TAB_LEADING_ACTION_BUTTON_SIZE
            };
            (count * (button_size + WINDOW_TAB_LEADING_ACTION_GAP)) as f32
        }
    }

    pub(super) fn window_tab_left_padding_pixels(&self) -> f32 {
        if self.workspace_sidebar_width() > 0 {
            return 0.0;
        }

        let cell_width = self.render_metrics.cell_size.width.max(1) as f32;
        let leading_action_slot_count = self.window_tab_leading_action_slot_count();
        let leading_action_padding = if leading_action_slot_count > 0 {
            self.window_tab_leading_action_start_pixels()
                + self.window_tab_leading_action_area_width_pixels()
        } else {
            0.0
        };
        if cfg!(target_os = "macos") && !self.window_state.contains(WindowState::FULL_SCREEN) {
            return leading_action_padding.max(MACOS_TRAFFIC_LIGHT_CLEARANCE_WIDTH as f32);
        }
        if self
            .config
            .window_decorations
            .contains(WindowDecorations::INTEGRATED_BUTTONS)
            && (self.config.integrated_title_button_alignment
                == window::IntegratedTitleButtonAlignment::Left
                || self.config.integrated_title_button_style
                    == window::IntegratedTitleButtonStyle::MacOsNative)
        {
            if self.config.integrated_title_button_style
                == window::IntegratedTitleButtonStyle::MacOsNative
            {
                if self.window_state.contains(WindowState::FULL_SCREEN) {
                    leading_action_padding + cell_width * 0.5
                } else {
                    70.0
                }
            } else {
                leading_action_padding
            }
        } else {
            leading_action_padding + cell_width * 0.5
        }
    }

    pub(super) fn pane_nav_tab_left_inset(&self, pane_left: usize) -> usize {
        if pane_left == 0 && self.workspace_sidebar_width() > 0 {
            TAB_ROW_START_PADDING
        } else {
            PANE_NAV_INSET
        }
    }

    pub(super) fn window_tab_viewport_width(&self) -> f32 {
        let border = self.get_os_border();
        let left_padding = self.window_tab_left_padding_pixels();

        self.dimensions
            .pixel_width
            .saturating_sub(self.tab_bar_left_edge())
            .saturating_sub(border.right.get() as usize)
            .saturating_sub(left_padding.max(0.0) as usize)
            .saturating_sub(if self.config.use_fancy_tab_bar {
                TAB_ROW_START_PADDING + WINDOW_TAB_ACTION_RESERVED_WIDTH
            } else {
                0
            })
            .max(1) as f32
    }

    pub(super) fn max_window_tab_scroll_offset(&self) -> f32 {
        let mux = Mux::get();
        let Some(window) = mux.get_window(self.mux_window_id) else {
            return 0.0;
        };
        let tab_count = window.len();
        if tab_count <= 1 {
            return 0.0;
        }

        let tab_width = self.window_tab_width_pixels();
        let tab_gap = if self.config.use_fancy_tab_bar {
            WINDOW_TAB_GAP as f32
        } else {
            0.0
        };
        let total_width =
            tab_count as f32 * tab_width + tab_count.saturating_sub(1) as f32 * tab_gap;
        let available_width = self.window_tab_viewport_width();

        (total_width - available_width).max(0.0)
    }

    fn scroll_window_tab_bar(&mut self, amount: i16, context: &dyn WindowOps) {
        let max_offset = self.max_window_tab_scroll_offset();
        let offset =
            (self.tab_bar_scroll_target + Self::tab_scroll_pixels(amount)).clamp(0.0, max_offset);
        if (offset - self.tab_bar_scroll_target).abs() > f32::EPSILON
            || (offset - self.tab_bar_scroll_offset).abs() > f32::EPSILON
        {
            self.tab_bar_scroll_target = offset;
            self.tab_bar_scroll_offset = offset;
            self.invalidate_fancy_tab_bar();
            context.invalidate();
        }
    }

    fn max_pane_nav_tab_scroll_offset_for_stack(&self, stack_id: mux::tab::PaneStackId) -> f32 {
        let Some(tab) = Mux::get().get_active_tab_for_window(self.mux_window_id) else {
            return 0.0;
        };

        let Some(pos) = tab
            .iter_panes_ignoring_zoom()
            .into_iter()
            .find(|pos| pos.pane_stack_id == stack_id)
        else {
            return 0.0;
        };

        let tab_count = Mux::get().pane_stack_tabs(pos.pane.pane_id()).len();
        if tab_count <= 1 {
            return 0.0;
        }

        let cell_width = self.render_metrics.cell_size.width as f32;
        let content_width = pos.width as f32 * cell_width;
        let pane_width = if pos.left == 0 && self.workspace_sidebar_width() > 0 {
            content_width
                + (self.padding_left_top().0 - self.workspace_sidebar_width() as f32).max(0.0)
        } else {
            content_width
        };
        let pane_left = pos.left;
        if pane_width <= 0.0 {
            return 0.0;
        }

        let nav_height = pane_nav_bar_height_for_metrics(self.render_metrics);
        let icon_size = nav_height.saturating_sub(PANE_NAV_INSET * 2).clamp(20, 24);
        let button_size = nav_height
            .saturating_sub(TAB_VERTICAL_PADDING * 2)
            .max(icon_size);
        let controls_width = (button_size + PANE_NAV_BUTTON_GAP)
            .saturating_mul(2)
            .saturating_add(PANE_NAV_INSET)
            .saturating_add(self.pane_nav_tab_left_inset(pane_left));
        let viewport_width = (pane_width as usize).saturating_sub(controls_width).max(1) as f32;
        let tab_width = self.window_tab_width_pixels();
        let total_width = tab_count as f32 * tab_width
            + tab_count.saturating_sub(1) as f32 * PANE_NAV_TAB_GAP as f32;

        (total_width - viewport_width).max(0.0)
    }

    fn set_tab_wheel_surface_lock(&mut self, surface: TabWheelSurface) {
        self.tab_wheel_surface_lock = Some((
            surface,
            std::time::Instant::now() + std::time::Duration::from_millis(TAB_WHEEL_SURFACE_LOCK_MS),
        ));
    }

    fn locked_tab_wheel_surface(&self) -> Option<TabWheelSurface> {
        let (surface, until) = self.tab_wheel_surface_lock?;
        (until > Instant::now()).then_some(surface)
    }

    fn should_ignore_tab_wheel_amount(&mut self, surface: TabWheelSurface, amount: i16) -> bool {
        let sign = amount.signum();
        if sign == 0 {
            return true;
        }

        let now = Instant::now();
        if let Some((locked_surface, locked_sign, until)) = self.tab_wheel_direction_lock {
            if locked_surface == surface
                && until > now
                && locked_sign != sign
                && amount.unsigned_abs() <= 1
            {
                return true;
            }
        }

        self.tab_wheel_direction_lock = Some((
            surface,
            sign,
            now + Duration::from_millis(TAB_WHEEL_DIRECTION_LOCK_MS),
        ));
        false
    }

    pub(crate) fn tab_wheel_scroll_active(&self) -> bool {
        self.locked_tab_wheel_surface().is_some()
            || self
                .pane_nav_tab_scroll_targets
                .iter()
                .any(|(stack_id, target)| {
                    let current = self
                        .pane_nav_tab_scroll_offsets
                        .get(stack_id)
                        .copied()
                        .unwrap_or(0.0);
                    (current - target).abs() > 0.75
                })
    }

    fn scroll_pane_nav_stack_tabs(
        &mut self,
        stack_key: mux::tab::PaneStackId,
        amount: i16,
        context: &dyn WindowOps,
    ) {
        let max_offset = self.max_pane_nav_tab_scroll_offset_for_stack(stack_key);
        let stored_offset = self
            .pane_nav_tab_scroll_offsets
            .get(&stack_key)
            .copied()
            .unwrap_or(0.0);
        let current_offset = stored_offset.clamp(0.0, max_offset.max(0.0));
        let needs_offset_clamp = (current_offset - stored_offset).abs() > f32::EPSILON;
        let stored_target = self
            .pane_nav_tab_scroll_targets
            .get(&stack_key)
            .copied()
            .unwrap_or(current_offset);
        let current_target = stored_target.clamp(0.0, max_offset.max(0.0));
        let needs_target_clamp = (current_target - stored_target).abs() > f32::EPSILON;
        let offset =
            (current_target + Self::tab_scroll_pixels(amount)).clamp(0.0, max_offset.max(0.0));
        if (offset - current_target).abs() > f32::EPSILON
            || needs_offset_clamp
            || needs_target_clamp
        {
            self.pane_nav_tab_scroll_targets.insert(stack_key, offset);
            if needs_offset_clamp || !self.pane_nav_tab_scroll_offsets.contains_key(&stack_key) {
                self.pane_nav_tab_scroll_offsets
                    .insert(stack_key, current_offset);
            }
            self.update_next_frame_time(Some(Instant::now() + Duration::from_millis(16)));
            context.invalidate();
        }
    }

    fn scroll_pane_nav_tabs(
        &mut self,
        pane_id: mux::pane::PaneId,
        amount: i16,
        context: &dyn WindowOps,
    ) {
        if let Some(stack_key) = Mux::get().pane_stack_id(pane_id) {
            self.scroll_tab_wheel_surface(TabWheelSurface::PaneStack(stack_key), amount, context);
        }
    }

    fn lock_pane_nav_tab_wheel_surface(
        &mut self,
        pane_id: mux::pane::PaneId,
    ) -> Option<mux::tab::PaneStackId> {
        let stack_key = Mux::get().pane_stack_id(pane_id)?;
        self.set_tab_wheel_surface_lock(TabWheelSurface::PaneStack(stack_key));
        Some(stack_key)
    }

    fn scroll_tab_wheel_surface(
        &mut self,
        surface: TabWheelSurface,
        amount: i16,
        context: &dyn WindowOps,
    ) {
        if self.should_ignore_tab_wheel_amount(surface, amount) {
            context.invalidate();
            return;
        }

        match surface {
            TabWheelSurface::Window => self.scroll_window_tab_bar(amount, context),
            TabWheelSurface::PaneStack(stack_id) => {
                self.scroll_pane_nav_stack_tabs(stack_id, amount, context)
            }
        }
    }

    fn wheel_amount(event: &MouseEvent) -> Option<i16> {
        match event.kind {
            WMEK::HorzWheel(amount) => Some(amount),
            _ => None,
        }
    }

    fn tab_wheel_surface_at_event(&self, event: &MouseEvent) -> Option<TabWheelSurface> {
        if let Some(UIItem {
            item_type: UIItemType::PaneNav { pane_id, .. },
            ..
        }) = self.resolve_ui_item(event)
        {
            return Mux::get()
                .pane_stack_id(pane_id)
                .map(TabWheelSurface::PaneStack);
        }

        let Some(tab) = Mux::get().get_active_tab_for_window(self.mux_window_id) else {
            return None;
        };

        let border = self.get_os_border();
        let top_bar_height = if self.show_tab_bar && !self.config.tab_bar_at_bottom {
            self.tab_bar_pixel_height().unwrap_or(0.)
        } else {
            0.0
        };
        let (padding_left, _) = self.padding_left_top();
        let nav_height = pane_nav_bar_height_for_metrics(self.render_metrics) as f32;
        let cell_width = self.render_metrics.cell_size.width as f32;
        let cell_height = self.render_metrics.cell_size.height as f32;
        let x = event.coords.x as f32;
        let y = event.coords.y as f32;

        for pos in tab.iter_panes_ignoring_zoom() {
            let content_pane_x =
                padding_left + border.left.get() as f32 + pos.left as f32 * cell_width;
            let pane_x = if pos.left == 0 && self.workspace_sidebar_width() > 0 {
                self.tab_bar_left_edge() as f32
            } else {
                content_pane_x
            };
            let pane_y = top_bar_height + border.top.get() as f32 + pos.top as f32 * cell_height;
            let pane_width = (content_pane_x + pos.width as f32 * cell_width - pane_x).max(1.0);
            if x >= pane_x && x < pane_x + pane_width && y >= pane_y && y < pane_y + nav_height {
                return Some(TabWheelSurface::PaneStack(pos.pane_stack_id));
            }
        }

        if self.show_tab_bar {
            let tab_bar_height = self.tab_bar_pixel_height().unwrap_or(0.);
            let tab_bar_y = if self.config.tab_bar_at_bottom {
                self.dimensions.pixel_height as f32 - tab_bar_height - border.bottom.get() as f32
            } else {
                border.top.get() as f32
            };
            if event.coords.x >= self.tab_bar_left_edge() as isize
                && y >= tab_bar_y
                && y < tab_bar_y + tab_bar_height
            {
                return Some(TabWheelSurface::Window);
            }
        }

        None
    }

    fn vertical_wheel_tab_surface_passthrough(&self, event: &MouseEvent, item: &UIItem) -> bool {
        if !matches!(event.kind, WMEK::VertWheel(_)) {
            return false;
        }

        matches!(
            item.item_type,
            UIItemType::PaneNav { .. } | UIItemType::TabBar(_)
        )
    }

    fn mouse_wheel_tab_surfaces(&mut self, event: &MouseEvent, context: &dyn WindowOps) -> bool {
        if !matches!(event.kind, WMEK::VertWheel(_) | WMEK::HorzWheel(_)) {
            return false;
        }

        let hovered_surface = self.tab_wheel_surface_at_event(event);

        if let Some(amount) = Self::wheel_amount(event) {
            match hovered_surface {
                Some(surface) => {
                    self.set_tab_wheel_surface_lock(surface);
                    self.scroll_tab_wheel_surface(surface, amount, context);
                    true
                }
                None => {
                    self.tab_wheel_surface_lock = None;
                    false
                }
            }
        } else {
            false
        }
    }

    fn mouse_wheel_workspace_sidebar(
        &mut self,
        event: &MouseEvent,
        context: &dyn WindowOps,
    ) -> bool {
        let Some(rect) = self.workspace_sidebar_rect() else {
            return false;
        };

        let x = event.coords.x;
        let y = event.coords.y;
        if x < rect.x as isize
            || x >= rect.x.saturating_add(rect.width) as isize
            || y < rect.y as isize
            || y >= rect.y.saturating_add(rect.height) as isize
        {
            return false;
        }

        let amount = match event.kind {
            WMEK::VertWheel(amount) => amount,
            // Trackpads often emit a little horizontal inertia while the user is
            // vertically scrolling. Consume it inside the sidebar so it doesn't
            // leak to tab or terminal wheel handlers at the scroll bounds.
            WMEK::HorzWheel(_) => return true,
            _ => return false,
        };
        if amount == 0 {
            return true;
        }

        let max_offset = self.workspace_sidebar_scroll_max();
        if max_offset <= 0.0 {
            return true;
        }

        self.show_workspace_sidebar_scrollbar();
        let offset = (self.workspace_sidebar_scroll_offset + Self::sidebar_scroll_pixels(amount))
            .clamp(0.0, max_offset);
        if (offset - self.workspace_sidebar_scroll_offset).abs() > f32::EPSILON {
            self.workspace_sidebar_scroll_offset = offset;
            context.invalidate();
        } else {
            context.invalidate();
        }
        true
    }

    fn resolve_ui_item(&self, event: &MouseEvent) -> Option<UIItem> {
        let x = event.coords.x;
        let y = event.coords.y;
        self.ui_items
            .iter()
            .rev()
            .find(|item| item.hit_test(x, y))
            .cloned()
    }

    fn leave_ui_item(&mut self, item: &UIItem) {
        match item.item_type {
            UIItemType::TabBar(_) => {
                self.update_title_post_status();
            }
            UIItemType::CloseTab(_)
            | UIItemType::PaneNav { .. }
            | UIItemType::ProjectNew
            | UIItemType::ProjectToggleSessions(_)
            | UIItemType::Project(_)
            | UIItemType::ProjectSession(_)
            | UIItemType::ProjectSessionPin(_)
            | UIItemType::ProjectSessionDelete(_)
            | UIItemType::ProjectSessionNew(_)
            | UIItemType::WorkspaceSidebarToggle
            | UIItemType::WorkspaceSidebarScrollTrack
            | UIItemType::WorkspaceSidebarScrollThumb
            | UIItemType::WorkspaceSidebarBackground
            | UIItemType::WorkspaceSidebarResize
            | UIItemType::WorkspaceSidebarSettings
            | UIItemType::WorkspaceSidebarViewOptions
            | UIItemType::AboveScrollThumb
            | UIItemType::BelowScrollThumb
            | UIItemType::ScrollThumb
            | UIItemType::Split(_) => {}
        }
    }

    fn enter_ui_item(&mut self, item: &UIItem) {
        match item.item_type {
            UIItemType::TabBar(_) => {}
            UIItemType::CloseTab(_)
            | UIItemType::PaneNav { .. }
            | UIItemType::ProjectNew
            | UIItemType::ProjectToggleSessions(_)
            | UIItemType::Project(_)
            | UIItemType::ProjectSession(_)
            | UIItemType::ProjectSessionPin(_)
            | UIItemType::ProjectSessionDelete(_)
            | UIItemType::ProjectSessionNew(_)
            | UIItemType::WorkspaceSidebarToggle
            | UIItemType::WorkspaceSidebarScrollTrack
            | UIItemType::WorkspaceSidebarScrollThumb
            | UIItemType::WorkspaceSidebarBackground
            | UIItemType::WorkspaceSidebarResize
            | UIItemType::WorkspaceSidebarSettings
            | UIItemType::WorkspaceSidebarViewOptions
            | UIItemType::AboveScrollThumb
            | UIItemType::BelowScrollThumb
            | UIItemType::ScrollThumb
            | UIItemType::Split(_) => {}
        }
    }

    fn click_position_for_pane(
        &self,
        event: &MouseEvent,
        pane: &Arc<dyn Pane>,
        pos: &PositionedPane,
    ) -> ClickPosition {
        let border = self.get_os_border();
        let first_line_offset = if self.show_tab_bar && !self.config.tab_bar_at_bottom {
            self.tab_bar_pixel_height().unwrap_or(0.) as isize
        } else {
            0
        } + border.top.get() as isize;
        let (padding_left, padding_top) = self.padding_left_top();

        let global_cell_size = self.render_metrics.cell_size;
        let pane_font_scale = self.pane_font_scale(pos.pane.pane_id());
        let pane_cell_size = if pane_font_scale.to_bits() == self.fonts.get_font_scale().to_bits() {
            global_cell_size
        } else {
            self.pane_font_resources(pane_font_scale)
                .map(|(_, metrics)| metrics.cell_size)
                .unwrap_or(global_cell_size)
        };

        let pane_left = (padding_left + border.left.get() as f32) as isize
            + (pos.left as isize * global_cell_size.width);
        let pane_top =
            padding_top as isize + first_line_offset + (pos.top as isize * global_cell_size.height);
        let pane_nav_height = pane_nav_bar_height_for_metrics(self.render_metrics) as isize;

        let local_x = event.coords.x.sub(pane_left);
        let local_y = event.coords.y.sub(pane_top + pane_nav_height);

        let x = (local_x.max(0) as f32) / pane_cell_size.width.max(1) as f32;
        let column = if !pane.is_mouse_grabbed() {
            x.round()
        } else {
            x
        }
        .trunc() as usize;

        let row = (local_y.max(0) / pane_cell_size.height.max(1)) as i64;

        let x_pixel_offset = if column > 0 {
            local_x.max(0) % pane_cell_size.width.max(1)
        } else {
            local_x
        };
        let y_pixel_offset = if row > 0 {
            local_y.max(0) % pane_cell_size.height.max(1)
        } else {
            local_y
        };

        ClickPosition {
            column,
            row,
            x_pixel_offset,
            y_pixel_offset,
        }
    }

    pub fn mouse_event_impl(&mut self, event: MouseEvent, context: &dyn WindowOps) {
        log::trace!("{:?}", event);
        let pane = match self.get_active_pane_or_overlay() {
            Some(pane) => pane,
            None => return,
        };

        self.current_mouse_event.replace(event.clone());

        if self.mouse_wheel_workspace_sidebar(&event, context) {
            return;
        }

        if self.mouse_wheel_tab_surfaces(&event, context) {
            return;
        }

        let border = self.get_os_border();

        let first_line_offset = if self.show_tab_bar && !self.config.tab_bar_at_bottom {
            self.tab_bar_pixel_height().unwrap_or(0.) as isize
        } else {
            0
        } + border.top.get() as isize;

        let (padding_left, padding_top) = self.padding_left_top();

        let y = (event
            .coords
            .y
            .sub(padding_top as isize)
            .sub(first_line_offset)
            .max(0)
            / self.render_metrics.cell_size.height) as i64;

        let x = (event
            .coords
            .x
            .sub((padding_left + border.left.get() as f32) as isize)
            .max(0) as f32)
            / self.render_metrics.cell_size.width as f32;
        let x = if !pane.is_mouse_grabbed() {
            // Round the x coordinate so that we're a bit more forgiving of
            // the horizontal position when selecting cells
            x.round()
        } else {
            x
        }
        .trunc() as usize;

        let mut y_pixel_offset = event
            .coords
            .y
            .sub(padding_top as isize)
            .sub(first_line_offset);
        if y > 0 {
            y_pixel_offset = y_pixel_offset.max(0) % self.render_metrics.cell_size.height;
        }

        let mut x_pixel_offset = event
            .coords
            .x
            .sub((padding_left + border.left.get() as f32) as isize);
        if x > 0 {
            x_pixel_offset = x_pixel_offset.max(0) % self.render_metrics.cell_size.width;
        }

        self.last_mouse_coords = (x, y);

        let mut capture_mouse = false;

        match event.kind {
            WMEK::Release(ref press) => {
                self.current_mouse_capture = None;
                self.current_mouse_buttons.retain(|p| p != press);
                if press == &MousePress::Left && self.window_drag_position.take().is_some() {
                    // Completed a window drag
                    return;
                }
                if press == &MousePress::Left {
                    let completed_drag = self.dragging.take();
                    if completed_drag.as_ref().is_some_and(|(item, _)| {
                        item.item_type == UIItemType::WorkspaceSidebarResize
                    }) {
                        self.persist_workspace_sidebar_width();
                    }
                    if completed_drag.is_some() {
                        // Completed a drag
                        return;
                    }
                }
            }

            WMEK::Press(ref press) => {
                capture_mouse = true;

                // Perform click counting
                let button = mouse_press_to_tmb(press);

                let click_position = ClickPosition {
                    column: x,
                    row: y,
                    x_pixel_offset,
                    y_pixel_offset,
                };

                let click = match self.last_mouse_click.take() {
                    None => LastMouseClick::new(button, click_position),
                    Some(click) => click.add(button, click_position),
                };
                self.last_mouse_click = Some(click);
                self.current_mouse_buttons.retain(|p| p != press);
                self.current_mouse_buttons.push(*press);
            }

            WMEK::Move => {
                if let Some(start) = self.window_drag_position.as_ref() {
                    // Dragging the window
                    // Compute the distance since the initial event
                    let delta_x = start.screen_coords.x - event.screen_coords.x;
                    let delta_y = start.screen_coords.y - event.screen_coords.y;

                    // Now compute a new window position.
                    // We don't have a direct way to get the position,
                    // but we can infer it by comparing the mouse coords
                    // with the screen coords in the initial event.
                    // This computes the original top_left position,
                    // and applies the total drag delta to it.
                    let top_left = ::window::ScreenPoint::new(
                        (start.screen_coords.x - start.coords.x) - delta_x,
                        (start.screen_coords.y - start.coords.y) - delta_y,
                    );
                    // and now tell the window to go there
                    context.set_window_position(top_left);
                    return;
                }

                if let Some((item, start_event)) = self.dragging.take() {
                    self.drag_ui_item(item, start_event, x, y, event, context);
                    return;
                }
            }
            _ => {}
        }

        let prior_ui_item = self.last_ui_item.clone();

        let ui_item = if matches!(self.current_mouse_capture, None | Some(MouseCapture::UI)) {
            let ui_item = self
                .resolve_ui_item(&event)
                .filter(|item| !self.vertical_wheel_tab_surface_passthrough(&event, item));

            match (self.last_ui_item.take(), &ui_item) {
                (Some(prior), Some(item)) => {
                    if prior != *item || !self.config.use_fancy_tab_bar {
                        self.leave_ui_item(&prior);
                        self.enter_ui_item(item);
                        context.invalidate();
                    }
                }
                (Some(prior), None) => {
                    self.leave_ui_item(&prior);
                    context.invalidate();
                }
                (None, Some(item)) => {
                    self.enter_ui_item(item);
                    context.invalidate();
                }
                (None, None) => {}
            }

            ui_item
        } else {
            None
        };

        if let Some(item) = ui_item.clone() {
            if capture_mouse {
                self.current_mouse_capture = Some(MouseCapture::UI);
            }
            self.mouse_event_ui_item(item, pane, y, event, context);
        } else if matches!(
            self.current_mouse_capture,
            None | Some(MouseCapture::TerminalPane(_))
        ) {
            self.mouse_event_terminal(
                pane,
                ClickPosition {
                    column: x,
                    row: y,
                    x_pixel_offset,
                    y_pixel_offset,
                },
                event,
                context,
                capture_mouse,
            );
        }

        if prior_ui_item != ui_item {
            self.update_title_post_status();
        }
    }

    pub fn mouse_leave_impl(&mut self, context: &dyn WindowOps) {
        self.current_mouse_event = None;
        self.update_title();
        context.set_cursor(Some(MouseCursor::Arrow));
        context.invalidate();
    }

    fn drag_split(
        &mut self,
        mut item: UIItem,
        split: PositionedSplit,
        start_event: MouseEvent,
        x: usize,
        y: i64,
        context: &dyn WindowOps,
    ) {
        let mux = Mux::get();
        let tab = match mux.get_active_tab_for_window(self.mux_window_id) {
            Some(tab) => tab,
            None => return,
        };
        let delta = match split.direction {
            SplitDirection::Horizontal => (x as isize).saturating_sub(split.left as isize),
            SplitDirection::Vertical => (y as isize).saturating_sub(split.top as isize),
        };

        if delta != 0 {
            tab.resize_split_by(split.index, delta);
            if let Some(split) = tab.iter_splits().into_iter().nth(split.index) {
                item.item_type = UIItemType::Split(split);
                context.invalidate();
            }
        }
        self.dragging.replace((item, start_event));
    }

    fn drag_scroll_thumb(
        &mut self,
        item: UIItem,
        start_event: MouseEvent,
        event: MouseEvent,
        context: &dyn WindowOps,
    ) {
        let pane = match self.get_active_pane_or_overlay() {
            Some(pane) => pane,
            None => return,
        };

        let dims = pane.get_dimensions();
        let current_viewport = self.get_viewport(pane.pane_id());

        let tab_bar_height = if self.show_tab_bar {
            self.tab_bar_pixel_height().unwrap_or(0.)
        } else {
            0.
        };
        let (top_bar_height, bottom_bar_height) = if self.config.tab_bar_at_bottom {
            (0.0, tab_bar_height)
        } else {
            (tab_bar_height, 0.0)
        };

        let border = self.get_os_border();
        let y_offset = top_bar_height + border.top.get() as f32;

        let from_top = start_event.coords.y.saturating_sub(item.y as isize);
        let effective_thumb_top = event
            .coords
            .y
            .saturating_sub(y_offset as isize + from_top)
            .max(0) as usize;

        // Convert thumb top into a row index by reversing the math
        // in ScrollHit::thumb
        let row = ScrollHit::thumb_top_to_scroll_top(
            effective_thumb_top,
            &*pane,
            current_viewport,
            self.dimensions.pixel_height.saturating_sub(
                y_offset as usize + border.bottom.get() + bottom_bar_height as usize,
            ),
            self.min_scroll_bar_height() as usize,
        );
        self.set_viewport(pane.pane_id(), Some(row), dims);
        context.invalidate();
        self.dragging.replace((item, start_event));
    }

    fn set_workspace_sidebar_scroll_from_thumb_top(
        &mut self,
        thumb_top: f32,
        context: &dyn WindowOps,
    ) {
        let Some(scroll) = self.workspace_sidebar_scroll_geometry() else {
            return;
        };
        let travel = (scroll.track_height as f32 - scroll.thumb_height).max(1.0);
        let relative_top = (thumb_top - scroll.track_y as f32).clamp(0.0, travel);
        let offset = (relative_top / travel * scroll.max_scroll).clamp(0.0, scroll.max_scroll);
        self.show_workspace_sidebar_scrollbar();
        if (offset - self.workspace_sidebar_scroll_offset).abs() > f32::EPSILON {
            self.workspace_sidebar_scroll_offset = offset;
            context.invalidate();
        } else {
            context.invalidate();
        }
    }

    fn drag_workspace_sidebar_scroll_thumb(
        &mut self,
        item: UIItem,
        start_event: MouseEvent,
        event: MouseEvent,
        context: &dyn WindowOps,
    ) {
        let from_top = start_event.coords.y.saturating_sub(item.y as isize) as f32;
        let thumb_top = event.coords.y as f32 - from_top;
        self.set_workspace_sidebar_scroll_from_thumb_top(thumb_top, context);
        context.set_cursor(Some(MouseCursor::Hand));
        self.dragging.replace((item, start_event));
    }

    fn drag_ui_item(
        &mut self,
        item: UIItem,
        start_event: MouseEvent,
        x: usize,
        y: i64,
        event: MouseEvent,
        context: &dyn WindowOps,
    ) {
        match item.item_type {
            UIItemType::Split(split) => {
                self.drag_split(item, split, start_event, x, y, context);
            }
            UIItemType::ScrollThumb => {
                self.drag_scroll_thumb(item, start_event, event, context);
            }
            UIItemType::WorkspaceSidebarResize => {
                self.drag_workspace_sidebar_resize(item, start_event, event, context);
            }
            UIItemType::WorkspaceSidebarScrollThumb => {
                self.drag_workspace_sidebar_scroll_thumb(item, start_event, event, context);
            }
            UIItemType::PaneNav { .. } => {}
            _ => {
                log::error!("drag not implemented for {:?}", item);
            }
        }
    }

    fn drag_workspace_sidebar_resize(
        &mut self,
        item: UIItem,
        start_event: MouseEvent,
        event: MouseEvent,
        context: &dyn WindowOps,
    ) {
        let left_edge = self.get_os_border().left.get() as isize;
        let width = event.coords.x.saturating_sub(left_edge).max(0) as usize;
        self.expand_workspace_sidebar();
        self.set_workspace_sidebar_width(width);
        self.reflow_workspace_sidebar(context);
        context.set_cursor(Some(MouseCursor::SizeLeftRight));
        self.dragging.replace((item, start_event));
    }

    fn mouse_event_ui_item(
        &mut self,
        item: UIItem,
        pane: Arc<dyn Pane>,
        _y: i64,
        event: MouseEvent,
        context: &dyn WindowOps,
    ) {
        self.last_ui_item.replace(item.clone());
        match item.item_type {
            UIItemType::TabBar(item) => {
                self.mouse_event_tab_bar(item, event, context);
            }
            UIItemType::AboveScrollThumb => {
                self.mouse_event_above_scroll_thumb(item, pane, event, context);
            }
            UIItemType::ScrollThumb => {
                self.mouse_event_scroll_thumb(item, pane, event, context);
            }
            UIItemType::BelowScrollThumb => {
                self.mouse_event_below_scroll_thumb(item, pane, event, context);
            }
            UIItemType::Split(split) => {
                self.mouse_event_split(item, split, event, context);
            }
            UIItemType::CloseTab(idx) => {
                self.mouse_event_close_tab(idx, event, context);
            }
            UIItemType::PaneNav {
                pane_id,
                pane_index,
                action,
            } => {
                self.mouse_event_pane_nav(pane_id, pane_index, action, event, context);
            }
            UIItemType::ProjectNew => {
                self.mouse_event_project_new(event, context);
            }
            UIItemType::ProjectToggleSessions(project_id) => {
                self.mouse_event_project_toggle_sessions(project_id, event, context);
            }
            UIItemType::Project(project_id) => {
                self.mouse_event_project(project_id, event, context);
            }
            UIItemType::ProjectSession(session_id) => {
                self.mouse_event_project_session(session_id, event, context);
            }
            UIItemType::ProjectSessionPin(session_id) => {
                self.mouse_event_project_session_pin(session_id, event, context);
            }
            UIItemType::ProjectSessionDelete(session_id) => {
                self.mouse_event_project_session_delete(session_id, event, context);
            }
            UIItemType::ProjectSessionNew(project_id) => {
                self.mouse_event_project_session_new(project_id, event, context);
            }
            UIItemType::WorkspaceSidebarToggle => {
                self.mouse_event_workspace_sidebar_toggle(event, context);
            }
            UIItemType::WorkspaceSidebarBackground => {
                context.set_cursor(Some(MouseCursor::Arrow));
            }
            UIItemType::WorkspaceSidebarScrollTrack => {
                self.mouse_event_workspace_sidebar_scroll_track(item, event, context);
            }
            UIItemType::WorkspaceSidebarScrollThumb => {
                self.mouse_event_workspace_sidebar_scroll_thumb(item, event, context);
            }
            UIItemType::WorkspaceSidebarResize => {
                self.mouse_event_workspace_sidebar_resize(item, event, context);
            }
            UIItemType::WorkspaceSidebarSettings => {
                self.mouse_event_workspace_sidebar_settings(event, context);
            }
            UIItemType::WorkspaceSidebarViewOptions => {
                self.mouse_event_workspace_sidebar_view_options(item, event, context);
            }
        }
    }

    fn reflow_workspace_sidebar(&mut self, context: &dyn WindowOps) {
        if let Some(window) = self.window.as_ref().cloned() {
            let dimensions = self.dimensions;
            self.apply_dimensions(&dimensions, None, &window);
        }
        context.invalidate();
    }

    pub fn mouse_event_workspace_sidebar_resize(
        &mut self,
        item: UIItem,
        event: MouseEvent,
        context: &dyn WindowOps,
    ) {
        context.set_cursor(Some(MouseCursor::SizeLeftRight));
        if event.kind == WMEK::Press(MousePress::Left) {
            self.dragging.replace((item, event));
        }
    }

    pub fn mouse_event_workspace_sidebar_toggle(
        &mut self,
        event: MouseEvent,
        context: &dyn WindowOps,
    ) {
        context.set_cursor(Some(MouseCursor::Hand));
        if event.kind == WMEK::Press(MousePress::Left) {
            self.toggle_workspace_sidebar();
            self.reflow_workspace_sidebar(context);
        }
    }

    pub fn mouse_event_workspace_sidebar_settings(
        &mut self,
        event: MouseEvent,
        context: &dyn WindowOps,
    ) {
        context.set_cursor(Some(MouseCursor::Hand));
        if event.kind == WMEK::Press(MousePress::Left) {
            crate::settings_window::show();
        }
    }

    pub fn mouse_event_workspace_sidebar_view_options(
        &mut self,
        item: UIItem,
        event: MouseEvent,
        context: &dyn WindowOps,
    ) {
        context.set_cursor(Some(MouseCursor::Hand));
        if event.kind == WMEK::Press(MousePress::Left) {
            let coords = window::Point::new(
                item.x.saturating_add(item.width) as isize,
                item.y.saturating_add(item.height / 2) as isize,
            );
            context.show_context_menu(coords, self.workspace_sidebar_view_options_menu_items());
        }
    }

    pub fn mouse_event_workspace_sidebar_scroll_thumb(
        &mut self,
        item: UIItem,
        event: MouseEvent,
        context: &dyn WindowOps,
    ) {
        context.set_cursor(Some(MouseCursor::Hand));
        self.show_workspace_sidebar_scrollbar();
        if event.kind == WMEK::Press(MousePress::Left) {
            self.dragging.replace((item, event));
            context.invalidate();
        }
    }

    pub fn mouse_event_workspace_sidebar_scroll_track(
        &mut self,
        _item: UIItem,
        event: MouseEvent,
        context: &dyn WindowOps,
    ) {
        context.set_cursor(Some(MouseCursor::Hand));
        self.show_workspace_sidebar_scrollbar();
        if event.kind == WMEK::Press(MousePress::Left) {
            if let Some(scroll) = self.workspace_sidebar_scroll_geometry() {
                let thumb_top = event.coords.y as f32 - scroll.thumb_height / 2.0;
                self.set_workspace_sidebar_scroll_from_thumb_top(thumb_top, context);
            }
            context.invalidate();
        }
    }

    pub fn mouse_event_project_session_new(
        &mut self,
        project_id: String,
        event: MouseEvent,
        context: &dyn WindowOps,
    ) {
        if let WMEK::Press(MousePress::Left) = event.kind {
            let session_id = crate::project_sessions::create_session(&project_id, None);
            self.activate_project_session(session_id, context);
        }
        context.set_cursor(Some(MouseCursor::Arrow));
    }

    pub fn mouse_event_project_new(&mut self, event: MouseEvent, context: &dyn WindowOps) {
        if let WMEK::Press(MousePress::Left) = event.kind {
            self.prompt_create_project(context);
        }
        context.set_cursor(Some(MouseCursor::Arrow));
    }

    pub fn mouse_event_project(
        &mut self,
        project_id: String,
        event: MouseEvent,
        context: &dyn WindowOps,
    ) {
        match event.kind {
            WMEK::Press(MousePress::Left) => {
                crate::project_sessions::toggle_project_sessions_collapsed(&project_id);
                context.invalidate();
            }
            WMEK::Press(MousePress::Right) => {
                context
                    .show_context_menu(event.coords, self.project_context_menu_items(&project_id));
            }
            _ => {}
        }
        context.set_cursor(Some(MouseCursor::Arrow));
    }

    pub fn mouse_event_project_toggle_sessions(
        &mut self,
        project_id: String,
        event: MouseEvent,
        context: &dyn WindowOps,
    ) {
        match event.kind {
            WMEK::Press(MousePress::Left) => {
                crate::project_sessions::toggle_project_sessions_collapsed(&project_id);
                context.invalidate();
            }
            WMEK::Press(MousePress::Right) => {
                context
                    .show_context_menu(event.coords, self.project_context_menu_items(&project_id));
            }
            _ => {}
        }
        context.set_cursor(Some(MouseCursor::Arrow));
    }

    pub fn mouse_event_project_session(
        &mut self,
        session_id: String,
        event: MouseEvent,
        context: &dyn WindowOps,
    ) {
        match event.kind {
            WMEK::Press(MousePress::Left) => {
                self.activate_project_session(session_id, context);
            }
            WMEK::Press(MousePress::Right) => {
                context.show_context_menu(
                    event.coords,
                    self.project_session_context_menu_items(&session_id),
                );
            }
            _ => {}
        }
        context.set_cursor(Some(MouseCursor::Arrow));
    }

    pub fn mouse_event_project_session_pin(
        &mut self,
        session_id: String,
        event: MouseEvent,
        context: &dyn WindowOps,
    ) {
        match event.kind {
            WMEK::Press(MousePress::Left) => {
                crate::project_sessions::toggle_session_pinned(&session_id);
                context.invalidate();
            }
            WMEK::Press(MousePress::Right) => {
                context.show_context_menu(
                    event.coords,
                    self.project_session_context_menu_items(&session_id),
                );
            }
            _ => {}
        }
        context.set_cursor(Some(MouseCursor::Arrow));
    }

    pub fn mouse_event_project_session_delete(
        &mut self,
        session_id: String,
        event: MouseEvent,
        context: &dyn WindowOps,
    ) {
        match event.kind {
            WMEK::Press(MousePress::Left) => {
                if let Some(deleted) = crate::project_sessions::delete_session(&session_id) {
                    if deleted.was_active {
                        if let Some(next_session_id) = deleted.next_session_id {
                            self.activate_project_session(next_session_id, context);
                        }
                    } else if let Some(workspace) = deleted.materialized_workspace_name {
                        let mux = Mux::get();
                        for window_id in mux.iter_windows_in_workspace(&workspace) {
                            mux.kill_window(window_id);
                        }
                    }
                    context.invalidate();
                }
            }
            WMEK::Press(MousePress::Right) => {
                context.show_context_menu(
                    event.coords,
                    self.project_session_context_menu_items(&session_id),
                );
            }
            _ => {}
        }
        context.set_cursor(Some(MouseCursor::Arrow));
    }

    fn project_context_menu_items(&self, project_id: &str) -> Vec<ContextMenuItem> {
        let project_id = project_id.to_string();
        vec![
            ContextMenuItem::item_with_icon(
                "Rename Workspace...",
                "pencil",
                KeyAssignment::PromptRenameProject(project_id.clone()),
            ),
            ContextMenuItem::item_with_icon(
                "New Session",
                "plus.square",
                KeyAssignment::CreateProjectSession(project_id.clone()),
            ),
            ContextMenuItem::item_with_icon(
                "Collapse / Expand Sessions",
                "chevron.right",
                KeyAssignment::ToggleProjectSessionsCollapsed(project_id.clone()),
            ),
            ContextMenuItem::Separator,
            ContextMenuItem::item_with_icon(
                "Remove Workspace",
                "folder.badge.minus",
                KeyAssignment::RemoveProject(project_id),
            ),
        ]
    }

    fn workspace_sidebar_view_options_menu_items(&self) -> Vec<ContextMenuItem> {
        use config::keyassignment::KeyAssignment;

        vec![
            ContextMenuItem::item("Group by", KeyAssignment::Nop).disabled(),
            ContextMenuItem::item_with_icon("Workspace", "folder", KeyAssignment::Nop)
                .checked(true)
                .disabled(),
            ContextMenuItem::Separator,
            ContextMenuItem::item("Show", KeyAssignment::Nop).disabled(),
            ContextMenuItem::submenu(
                "Status",
                vec![
                    ContextMenuItem::item_with_icon(
                        "Running",
                        "arrow.triangle.2.circlepath",
                        KeyAssignment::Nop,
                    )
                    .checked(true)
                    .disabled(),
                    ContextMenuItem::item_with_icon(
                        "Needs Attention",
                        "exclamationmark.circle",
                        KeyAssignment::Nop,
                    )
                    .checked(true)
                    .disabled(),
                    ContextMenuItem::item_with_icon("Done", "checkmark.circle", KeyAssignment::Nop)
                        .checked(true)
                        .disabled(),
                ],
            ),
            ContextMenuItem::item_with_icon("Unread", "envelope.badge", KeyAssignment::Nop)
                .checked(true)
                .disabled(),
            ContextMenuItem::item_with_icon("Archived", "archivebox", KeyAssignment::Nop)
                .disabled(),
            ContextMenuItem::item_with_icon("Pinned", "pin", KeyAssignment::Nop)
                .checked(true)
                .disabled(),
            ContextMenuItem::Separator,
            ContextMenuItem::item("Collapse All", KeyAssignment::Nop).disabled(),
            ContextMenuItem::item("Mark All Read", KeyAssignment::Nop).disabled(),
        ]
    }

    fn project_session_context_menu_items(&self, session_id: &str) -> Vec<ContextMenuItem> {
        let session_id = session_id.to_string();
        let is_pinned = crate::project_sessions::session_is_pinned(&session_id);
        vec![
            ContextMenuItem::item_with_icon(
                if is_pinned {
                    "Unpin Session"
                } else {
                    "Pin Session"
                },
                if is_pinned { "pin.slash" } else { "pin" },
                KeyAssignment::ToggleProjectSessionPinned(session_id.clone()),
            ),
            ContextMenuItem::item_with_icon(
                "Rename Session...",
                "pencil",
                KeyAssignment::PromptRenameProjectSession(session_id.clone()),
            ),
            ContextMenuItem::item_with_icon(
                "Archive Session",
                "archivebox",
                KeyAssignment::ArchiveProjectSession(session_id.clone()),
            ),
            ContextMenuItem::item_with_icon(
                "Delete Session",
                "trash",
                KeyAssignment::DeleteProjectSession(session_id.clone()),
            ),
            ContextMenuItem::item_with_icon(
                "Mark as Unread",
                "envelope.badge",
                KeyAssignment::MarkProjectSessionUnread(session_id),
            ),
        ]
    }

    pub(crate) fn activate_project_session(&mut self, session_id: String, context: &dyn WindowOps) {
        self.snapshot_active_project_session_layout();

        let mux = Mux::get();
        let live_workspaces = mux.iter_workspaces();
        let Some(plan) =
            crate::project_sessions::activate_session_record(&session_id, &live_workspaces)
        else {
            context.invalidate();
            return;
        };

        if !plan.needs_materialize {
            if mux.active_workspace() != plan.workspace_name {
                front_end().switch_workspace(&plan.workspace_name);
            }
            context.invalidate();
            return;
        }

        let workspace_name = plan.workspace_name.clone();
        let initial_cwd = plan.project_path.to_str().map(|path| path.to_string());
        let layout = crate::project_sessions::session_layout(&plan.session_id);
        let dpi = self.dimensions.dpi as u32;
        let size = self.config.initial_size(
            dpi,
            crate::cell_pixel_dims(&self.config, self.dimensions.dpi as f64).ok(),
        );
        let term_config: Arc<dyn wezterm_term::TerminalConfiguration> =
            Arc::new(TermConfig::with_config(self.config.clone()));
        let switcher = crate::frontend::WorkspaceSwitcher::new(&workspace_name);
        mux.set_active_workspace(&workspace_name);

        promise::spawn::spawn(async move {
            if let Err(err) = crate::project_sessions::materialize_session(
                workspace_name,
                layout,
                initial_cwd,
                size,
                None,
                term_config,
            )
            .await
            {
                log::error!("failed to materialize ThinkTerm session: {err:#}");
            }
            switcher.do_switch();
        })
        .detach();

        context.invalidate();
    }

    fn pane_nav_tab_context_menu_items(&self, pane_id: mux::pane::PaneId) -> Vec<ContextMenuItem> {
        vec![ContextMenuItem::item_with_icon(
            "Rename Tab...",
            "pencil",
            KeyAssignment::PromptRenamePaneTab(pane_id),
        )]
    }

    pub fn mouse_event_pane_nav(
        &mut self,
        pane_id: mux::pane::PaneId,
        pane_index: usize,
        action: PaneNavAction,
        event: MouseEvent,
        context: &dyn WindowOps,
    ) {
        context.set_cursor(Some(MouseCursor::Arrow));

        match event.kind {
            WMEK::HorzWheel(amount) => {
                self.lock_pane_nav_tab_wheel_surface(pane_id);
                self.scroll_pane_nav_tabs(pane_id, amount, context);
                return;
            }
            WMEK::VertWheel(_) => {
                self.lock_pane_nav_tab_wheel_surface(pane_id);
                return;
            }
            _ => {}
        }

        if event.kind == WMEK::Press(MousePress::Right) {
            if let PaneNavAction::Activate(target_pane_id) = action {
                context.show_context_menu(
                    event.coords,
                    self.pane_nav_tab_context_menu_items(target_pane_id),
                );
            }
            return;
        }

        if event.kind != WMEK::Press(MousePress::Left) {
            return;
        }

        let mux = Mux::get();

        match action {
            PaneNavAction::Background => {
                if let Some(tab) = mux.get_active_tab_for_window(self.mux_window_id) {
                    tab.set_active_idx(pane_index);
                }
                if self.last_mouse_click.as_ref().map(|c| c.streak) == Some(2) {
                    self.spawn_pane_nav_tab(pane_id, pane_index);
                }
            }
            PaneNavAction::Activate(target_pane_id) => {
                if let Err(err) = mux.activate_pane_in_stack(target_pane_id) {
                    log::error!("pane nav activate failed: {err:#}");
                }
            }
            PaneNavAction::Close(target_pane_id) => {
                if let Some(pane) = mux.get_pane(target_pane_id) {
                    self.close_pane(pane, true);
                }
            }
            PaneNavAction::NewTab => {
                if let Some(tab) = mux.get_active_tab_for_window(self.mux_window_id) {
                    tab.set_active_idx(pane_index);
                }
                self.spawn_pane_nav_tab(pane_id, pane_index);
            }
            PaneNavAction::SplitRight | PaneNavAction::SplitDown => {
                if let Some(tab) = mux.get_active_tab_for_window(self.mux_window_id) {
                    tab.set_active_idx(pane_index);
                }
                let pane = match mux.get_pane(pane_id) {
                    Some(pane) => pane,
                    None => return,
                };
                let direction = match action {
                    PaneNavAction::SplitRight => PaneDirection::Right,
                    PaneNavAction::SplitDown => PaneDirection::Down,
                    _ => unreachable!(),
                };
                let assignment = KeyAssignment::SplitPane(SplitPane {
                    direction,
                    size: SplitSize::Percent(50),
                    command: SpawnCommand::default(),
                    top_level: false,
                });
                if let Err(err) = self.perform_key_assignment(&pane, &assignment) {
                    log::error!("pane nav action failed: {err:#}");
                }
            }
        }

        context.invalidate();
    }

    fn spawn_pane_nav_tab(&mut self, pane_id: mux::pane::PaneId, pane_index: usize) {
        let mux = Mux::get();
        let tab = match mux.get_active_tab_for_window(self.mux_window_id) {
            Some(tab) => tab,
            None => return,
        };
        let panes = tab.iter_panes_ignoring_zoom();
        let pos = panes
            .iter()
            .find(|pos| pos.pane.pane_id() == pane_id)
            .or_else(|| panes.get(pane_index))
            .cloned();
        let pos = match pos {
            Some(pos) => pos,
            None => return,
        };
        let size = self.terminal_size_for_positioned_pane(&pos, self.render_metrics);
        let window = GuiWin::new(self);

        promise::spawn::spawn(async move {
            if let Err(err) = Mux::get()
                .spawn_pane_in_stack(pane_id, SpawnTabDomain::CurrentPaneDomain, None, None, size)
                .await
            {
                log::error!("pane nav new terminal failed: {err:#}");
                return;
            }
            window
                .window
                .notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                    term_window.update_title();
                    if let Some(window) = term_window.window.as_ref() {
                        window.invalidate();
                    }
                })));
        })
        .detach();
    }

    pub fn mouse_event_close_tab(
        &mut self,
        idx: usize,
        event: MouseEvent,
        context: &dyn WindowOps,
    ) {
        match event.kind {
            WMEK::Press(MousePress::Left) => {
                log::debug!("Should close tab {}", idx);
                self.close_specific_tab(idx, true);
            }
            _ => {}
        }
        context.set_cursor(Some(MouseCursor::Arrow));
    }

    fn do_new_tab_button_click(&mut self, button: MousePress) {
        let pane = match self.get_active_pane_or_overlay() {
            Some(pane) => pane,
            None => return,
        };
        let action = match button {
            MousePress::Left => Some(KeyAssignment::SpawnTab(SpawnTabDomain::CurrentPaneDomain)),
            MousePress::Right => Some(KeyAssignment::ShowLauncher),
            MousePress::Middle => None,
        };

        async fn dispatch_new_tab_button(
            lua: Option<Rc<mlua::Lua>>,
            window: GuiWin,
            pane: MuxPane,
            button: MousePress,
            action: Option<KeyAssignment>,
        ) -> anyhow::Result<()> {
            let default_action = match lua {
                Some(lua) => {
                    let args = lua.pack_multi((
                        window.clone(),
                        pane,
                        format!("{button:?}"),
                        action.clone(),
                    ))?;
                    config::lua::emit_event(&lua, ("new-tab-button-click".to_string(), args))
                        .await
                        .map_err(|e| {
                            log::error!("while processing new-tab-button-click event: {:#}", e);
                            e
                        })?
                }
                None => true,
            };
            if let (true, Some(assignment)) = (default_action, action) {
                window.window.notify(TermWindowNotif::PerformAssignment {
                    pane_id: pane.0,
                    assignment,
                    tx: None,
                });
            }
            Ok(())
        }
        let window = GuiWin::new(self);
        let pane = MuxPane(pane.pane_id());
        promise::spawn::spawn(config::with_lua_config_on_main_thread(move |lua| {
            dispatch_new_tab_button(lua, window, pane, button, action)
        }))
        .detach();
    }

    fn spawn_window_tab_from_active_pane(&mut self) {
        let pane = match self.get_active_pane_or_overlay() {
            Some(pane) => pane,
            None => return,
        };
        let assignment = KeyAssignment::SpawnTab(SpawnTabDomain::CurrentPaneDomain);
        if let Err(err) = self.perform_key_assignment(&pane, &assignment) {
            log::error!("tab bar double-click new tab failed: {err:#}");
        }
    }

    fn tab_context_action(tab_idx: usize, action: KeyAssignment) -> KeyAssignment {
        KeyAssignment::Multiple(vec![KeyAssignment::ActivateTab(tab_idx as isize), action])
    }

    fn close_tabs_to_left_action(tab_idx: usize) -> Option<KeyAssignment> {
        if tab_idx == 0 {
            return None;
        }

        let mut actions = Vec::with_capacity(tab_idx * 2);
        for _ in 0..tab_idx {
            actions.push(KeyAssignment::ActivateTab(0));
            actions.push(KeyAssignment::CloseCurrentTab { confirm: false });
        }
        Some(KeyAssignment::Multiple(actions))
    }

    fn close_tabs_to_right_action(tab_idx: usize, tab_count: usize) -> Option<KeyAssignment> {
        if tab_idx + 1 >= tab_count {
            return None;
        }

        let mut actions = Vec::with_capacity((tab_count - tab_idx - 1) * 2);
        for _ in tab_idx + 1..tab_count {
            actions.push(KeyAssignment::ActivateTab((tab_idx + 1) as isize));
            actions.push(KeyAssignment::CloseCurrentTab { confirm: false });
        }
        Some(KeyAssignment::Multiple(actions))
    }

    fn close_other_tabs_action(tab_idx: usize, tab_count: usize) -> Option<KeyAssignment> {
        if tab_count <= 1 {
            return None;
        }

        let mut actions = vec![];
        if let Some(KeyAssignment::Multiple(mut right)) =
            Self::close_tabs_to_right_action(tab_idx, tab_count)
        {
            actions.append(&mut right);
        }
        if let Some(KeyAssignment::Multiple(mut left)) = Self::close_tabs_to_left_action(tab_idx) {
            actions.append(&mut left);
        }
        Some(KeyAssignment::Multiple(actions))
    }

    fn tab_context_menu_items(&self, tab_idx: usize) -> Vec<ContextMenuItem> {
        let tab_count = Mux::get()
            .get_window(self.mux_window_id)
            .map(|window| window.len())
            .unwrap_or(0);
        if tab_count == 0 || tab_idx >= tab_count {
            return vec![];
        }

        let mut items = vec![ContextMenuItem::item_with_icon(
            "Rename Tab...",
            "pencil",
            Self::tab_context_action(tab_idx, KeyAssignment::PromptRenameTab),
        )];

        let mut close_items = vec![];
        if let Some(action) = Self::close_tabs_to_left_action(tab_idx) {
            close_items.push(ContextMenuItem::item("Close Tabs to Left", action));
        }
        if let Some(action) = Self::close_tabs_to_right_action(tab_idx, tab_count) {
            close_items.push(ContextMenuItem::item("Close Tabs to Right", action));
        }
        if let Some(action) = Self::close_other_tabs_action(tab_idx, tab_count) {
            close_items.push(ContextMenuItem::item("Close Other Tabs", action));
        }
        if !close_items.is_empty() {
            items.push(ContextMenuItem::Separator);
            items.append(&mut close_items);
        }

        let mut move_items = vec![];
        if tab_idx > 0 {
            move_items.push(ContextMenuItem::item(
                "Move Tab Left",
                Self::tab_context_action(tab_idx, KeyAssignment::MoveTab(tab_idx - 1)),
            ));
        }
        if tab_idx + 1 < tab_count {
            move_items.push(ContextMenuItem::item(
                "Move Tab Right",
                Self::tab_context_action(tab_idx, KeyAssignment::MoveTab(tab_idx + 1)),
            ));
        }
        if !move_items.is_empty() {
            items.push(ContextMenuItem::Separator);
            items.append(&mut move_items);
        }

        items.push(ContextMenuItem::Separator);
        items.push(ContextMenuItem::item_with_icon(
            "New Terminal Tab to Right",
            "plus.square",
            Self::tab_context_action(
                tab_idx,
                KeyAssignment::SpawnTabToRight(SpawnTabDomain::CurrentPaneDomain),
            ),
        ));
        items.push(ContextMenuItem::Separator);
        items.push(ContextMenuItem::item(
            "Zoom Pane",
            Self::tab_context_action(tab_idx, KeyAssignment::TogglePaneZoomState),
        ));

        items
    }

    pub fn mouse_event_tab_bar(
        &mut self,
        item: TabBarItem,
        event: MouseEvent,
        context: &dyn WindowOps,
    ) {
        match event.kind {
            WMEK::Press(MousePress::Left) => match item {
                TabBarItem::Tab { tab_idx, .. } => {
                    self.activate_tab(tab_idx as isize).ok();
                }
                TabBarItem::NewTabButton { .. } => {
                    self.do_new_tab_button_click(MousePress::Left);
                }
                TabBarItem::None | TabBarItem::LeftStatus | TabBarItem::RightStatus => {
                    if self.last_mouse_click.as_ref().map(|c| c.streak) == Some(2) {
                        self.window_drag_position.take();
                        self.spawn_window_tab_from_active_pane();
                        context.invalidate();
                        return;
                    }

                    let maximized = self
                        .window_state
                        .intersects(WindowState::MAXIMIZED | WindowState::FULL_SCREEN);
                    if let Some(ref window) = self.window {
                        if self.config.window_decorations
                            == WindowDecorations::INTEGRATED_BUTTONS | WindowDecorations::RESIZE
                        {
                            if self.last_mouse_click.as_ref().map(|c| c.streak) == Some(2) {
                                if maximized {
                                    window.restore();
                                } else {
                                    window.maximize();
                                }
                            }
                        }
                    }
                    // Potentially starting a drag by the tab bar
                    if !maximized {
                        self.window_drag_position.replace(event.clone());
                    }
                    context.request_drag_move();
                }
                TabBarItem::WindowButton(button) => {
                    use window::IntegratedTitleButton as Button;
                    if let Some(ref window) = self.window {
                        match button {
                            Button::Hide => window.hide(),
                            Button::Maximize => {
                                let maximized = self
                                    .window_state
                                    .intersects(WindowState::MAXIMIZED | WindowState::FULL_SCREEN);
                                if maximized {
                                    window.restore();
                                } else {
                                    window.maximize();
                                }
                            }
                            Button::Close => self.close_requested(&window.clone()),
                        }
                    }
                }
            },
            WMEK::Press(MousePress::Middle) => match item {
                TabBarItem::Tab { tab_idx, .. } => {
                    self.close_specific_tab(tab_idx, true);
                }
                TabBarItem::NewTabButton { .. } => {
                    self.do_new_tab_button_click(MousePress::Middle);
                }
                TabBarItem::None
                | TabBarItem::LeftStatus
                | TabBarItem::RightStatus
                | TabBarItem::WindowButton(_) => {}
            },
            WMEK::Press(MousePress::Right) => match item {
                TabBarItem::Tab { tab_idx, .. } => {
                    context.show_context_menu(event.coords, self.tab_context_menu_items(tab_idx));
                }
                TabBarItem::NewTabButton { .. } => {
                    self.do_new_tab_button_click(MousePress::Right);
                }
                TabBarItem::None
                | TabBarItem::LeftStatus
                | TabBarItem::RightStatus
                | TabBarItem::WindowButton(_) => {}
            },
            WMEK::Move => match item {
                TabBarItem::None | TabBarItem::LeftStatus | TabBarItem::RightStatus => {
                    context.set_window_drag_position(event.screen_coords);
                }
                TabBarItem::WindowButton(window::IntegratedTitleButton::Maximize) => {
                    let item = self.last_ui_item.clone().unwrap();
                    let bounds: ::window::ScreenRect = euclid::rect(
                        item.x as isize - (event.coords.x as isize - event.screen_coords.x),
                        item.y as isize - (event.coords.y as isize - event.screen_coords.y),
                        item.width as isize,
                        item.height as isize,
                    );
                    context.set_maximize_button_position(bounds);
                }
                TabBarItem::WindowButton(_)
                | TabBarItem::Tab { .. }
                | TabBarItem::NewTabButton { .. } => {}
            },
            WMEK::HorzWheel(amount) => {
                self.scroll_window_tab_bar(amount, context);
            }
            WMEK::VertWheel(_) => {}
            _ => {}
        }
        context.set_cursor(Some(MouseCursor::Arrow));
    }

    pub fn mouse_event_above_scroll_thumb(
        &mut self,
        _item: UIItem,
        pane: Arc<dyn Pane>,
        event: MouseEvent,
        context: &dyn WindowOps,
    ) {
        if let WMEK::Press(MousePress::Left) = event.kind {
            let dims = pane.get_dimensions();
            let current_viewport = self.get_viewport(pane.pane_id());
            // Page up
            self.set_viewport(
                pane.pane_id(),
                Some(
                    current_viewport
                        .unwrap_or(dims.physical_top)
                        .saturating_sub(self.terminal_size.rows.try_into().unwrap()),
                ),
                dims,
            );
            context.invalidate();
        }
        context.set_cursor(Some(MouseCursor::Arrow));
    }

    pub fn mouse_event_below_scroll_thumb(
        &mut self,
        _item: UIItem,
        pane: Arc<dyn Pane>,
        event: MouseEvent,
        context: &dyn WindowOps,
    ) {
        if let WMEK::Press(MousePress::Left) = event.kind {
            let dims = pane.get_dimensions();
            let current_viewport = self.get_viewport(pane.pane_id());
            // Page down
            self.set_viewport(
                pane.pane_id(),
                Some(
                    current_viewport
                        .unwrap_or(dims.physical_top)
                        .saturating_add(self.terminal_size.rows.try_into().unwrap()),
                ),
                dims,
            );
            context.invalidate();
        }
        context.set_cursor(Some(MouseCursor::Arrow));
    }

    pub fn mouse_event_scroll_thumb(
        &mut self,
        item: UIItem,
        _pane: Arc<dyn Pane>,
        event: MouseEvent,
        context: &dyn WindowOps,
    ) {
        if let WMEK::Press(MousePress::Left) = event.kind {
            // Start a scroll drag
            // self.scroll_drag_start = Some(from_top);
            self.dragging = Some((item, event));
        }
        context.set_cursor(Some(MouseCursor::Arrow));
    }

    pub fn mouse_event_split(
        &mut self,
        item: UIItem,
        split: PositionedSplit,
        event: MouseEvent,
        context: &dyn WindowOps,
    ) {
        context.set_cursor(Some(match &split.direction {
            SplitDirection::Horizontal => MouseCursor::SizeLeftRight,
            SplitDirection::Vertical => MouseCursor::SizeUpDown,
        }));

        if event.kind == WMEK::Press(MousePress::Left) {
            self.dragging.replace((item, event));
        }
    }

    fn terminal_context_menu_items(&self) -> Vec<ContextMenuItem> {
        fn split_item(label: &str, icon: &str, direction: PaneDirection) -> ContextMenuItem {
            ContextMenuItem::item_with_icon(
                label,
                icon,
                KeyAssignment::SplitPane(SplitPane {
                    direction,
                    size: SplitSize::Percent(50),
                    command: SpawnCommand::default(),
                    top_level: false,
                }),
            )
        }

        vec![
            ContextMenuItem::item_with_icon(
                "Paste",
                "doc.on.clipboard",
                KeyAssignment::PasteFrom(ClipboardPasteSource::Clipboard),
            ),
            ContextMenuItem::Separator,
            split_item("Split Right", "rectangle.split.2x1", PaneDirection::Right),
            split_item("Split Left", "rectangle.split.2x1", PaneDirection::Left),
            split_item("Split Down", "rectangle.split.1x2", PaneDirection::Down),
            split_item("Split Up", "rectangle.split.1x2", PaneDirection::Up),
            ContextMenuItem::Separator,
            ContextMenuItem::item_with_icon(
                "Reset Terminal",
                "arrow.clockwise",
                KeyAssignment::ResetTerminal,
            ),
        ]
    }

    fn mouse_event_terminal(
        &mut self,
        mut pane: Arc<dyn Pane>,
        position: ClickPosition,
        event: MouseEvent,
        context: &dyn WindowOps,
        capture_mouse: bool,
    ) {
        let mut is_click_to_focus_pane = false;

        let ClickPosition {
            mut column,
            mut row,
            mut x_pixel_offset,
            mut y_pixel_offset,
        } = position;

        let is_already_captured = matches!(
            self.current_mouse_capture,
            Some(MouseCapture::TerminalPane(_))
        );

        for pos in self.get_panes_to_render() {
            if !is_already_captured
                && row >= pos.top as i64
                && row <= (pos.top + pos.height) as i64
                && column >= pos.left
                && column <= pos.left + pos.width
            {
                if pane.pane_id() != pos.pane.pane_id() {
                    // We're over a pane that isn't active
                    match &event.kind {
                        WMEK::Press(_) => {
                            let mux = Mux::get();
                            mux.get_active_tab_for_window(self.mux_window_id)
                                .map(|tab| tab.set_active_idx(pos.index));

                            pane = Arc::clone(&pos.pane);
                            is_click_to_focus_pane = true;
                        }
                        WMEK::Move => {
                            // ThinkTerm keeps pane focus explicit. The upstream
                            // focus-follows-mouse behavior makes split panes
                            // appear to switch while scrolling pane-local tabs.
                        }
                        WMEK::Release(_) | WMEK::HorzWheel(_) => {}
                        WMEK::VertWheel(_) => {
                            // Let wheel events route to the hovered pane,
                            // even if it doesn't have focus
                            pane = Arc::clone(&pos.pane);
                            context.invalidate();
                        }
                    }
                }
                let position = self.click_position_for_pane(&event, &pane, &pos);
                column = position.column;
                row = position.row;
                x_pixel_offset = position.x_pixel_offset;
                y_pixel_offset = position.y_pixel_offset;
                break;
            } else if is_already_captured && pane.pane_id() == pos.pane.pane_id() {
                let position = self.click_position_for_pane(&event, &pane, &pos);
                column = position.column;
                row = position.row;
                x_pixel_offset = position.x_pixel_offset;
                y_pixel_offset = position.y_pixel_offset;
                break;
            }
        }

        if capture_mouse {
            self.current_mouse_capture = Some(MouseCapture::TerminalPane(pane.pane_id()));
        }

        let is_focused = if let Some(focused) = self.focused.as_ref() {
            !self.config.swallow_mouse_click_on_window_focus
                || (focused.elapsed() > Duration::from_millis(200))
        } else {
            false
        };

        if self.focused.is_some() && !is_focused {
            if matches!(&event.kind, WMEK::Press(_))
                && self.config.swallow_mouse_click_on_window_focus
            {
                // Entering click to focus state
                self.is_click_to_focus_window = true;
                context.invalidate();
                log::trace!("enter click to focus");
                return;
            }
        }
        if self.is_click_to_focus_window && matches!(&event.kind, WMEK::Release(_)) {
            // Exiting click to focus state
            self.is_click_to_focus_window = false;
            context.invalidate();
            log::trace!("exit click to focus");
            return;
        }

        let allow_action = if self.is_click_to_focus_window || !is_focused {
            matches!(&event.kind, WMEK::VertWheel(_) | WMEK::HorzWheel(_))
        } else {
            true
        };

        log::trace!(
            "is_focused={} allow_action={} event={:?}",
            is_focused,
            allow_action,
            event
        );

        let dims = pane.get_dimensions();
        let stable_row = self
            .get_viewport(pane.pane_id())
            .unwrap_or(dims.physical_top)
            + row as StableRowIndex;

        self.pane_state(pane.pane_id())
            .mouse_terminal_coords
            .replace((
                ClickPosition {
                    column,
                    row,
                    x_pixel_offset,
                    y_pixel_offset,
                },
                stable_row,
            ));

        pane.apply_hyperlinks(stable_row..stable_row + 1, &self.config.hyperlink_rules);

        struct FindCurrentLink {
            current: Option<Arc<Hyperlink>>,
            stable_row: StableRowIndex,
            column: usize,
        }

        impl WithPaneLines for FindCurrentLink {
            fn with_lines_mut(&mut self, stable_top: StableRowIndex, lines: &mut [&mut Line]) {
                if stable_top == self.stable_row {
                    if let Some(line) = lines.get(0) {
                        if let Some(cell) = line.get_cell(self.column) {
                            self.current = cell.attrs().hyperlink().cloned();
                        }
                    }
                }
            }
        }

        let mut find_link = FindCurrentLink {
            current: None,
            stable_row,
            column,
        };
        pane.with_lines_mut(stable_row..stable_row + 1, &mut find_link);
        let new_highlight = find_link.current;

        match (self.current_highlight.as_ref(), new_highlight) {
            (Some(old_link), Some(new_link)) if Arc::ptr_eq(&old_link, &new_link) => {
                // Unchanged
            }
            (None, None) => {
                // Unchanged
            }
            (_, rhs) => {
                // We're hovering over a different URL, so invalidate and repaint
                // so that we render the underline correctly
                self.current_highlight = rhs;
                context.invalidate();
            }
        };

        let outside_window = event.coords.x < 0
            || event.coords.x as usize > self.dimensions.pixel_width
            || event.coords.y < 0
            || event.coords.y as usize > self.dimensions.pixel_height;

        context.set_cursor(Some(if self.current_highlight.is_some() {
            // When hovering over a hyperlink, show an appropriate
            // mouse cursor to give the cue that it is clickable
            MouseCursor::Hand
        } else if pane.is_mouse_grabbed() || outside_window {
            MouseCursor::Arrow
        } else {
            MouseCursor::Text
        }));

        let event_trigger_type = match &event.kind {
            WMEK::Press(press) => {
                let press = mouse_press_to_tmb(press);
                match self.last_mouse_click.as_ref() {
                    Some(LastMouseClick { streak, button, .. }) if *button == press => {
                        Some(MouseEventTrigger::Down {
                            streak: *streak,
                            button: press,
                        })
                    }
                    _ => None,
                }
            }
            WMEK::Release(press) => {
                let press = mouse_press_to_tmb(press);
                match self.last_mouse_click.as_ref() {
                    Some(LastMouseClick { streak, button, .. }) if *button == press => {
                        Some(MouseEventTrigger::Up {
                            streak: *streak,
                            button: press,
                        })
                    }
                    _ => None,
                }
            }
            WMEK::Move => {
                if !self.current_mouse_buttons.is_empty() {
                    if let Some(LastMouseClick { streak, button, .. }) =
                        self.last_mouse_click.as_ref()
                    {
                        if Some(*button)
                            == self.current_mouse_buttons.last().map(mouse_press_to_tmb)
                        {
                            Some(MouseEventTrigger::Drag {
                                streak: *streak,
                                button: *button,
                            })
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                } else {
                    None
                }
            }
            WMEK::VertWheel(amount) => Some(match *amount {
                0 => return,
                1.. => MouseEventTrigger::Down {
                    streak: 1,
                    button: MouseButton::WheelUp(*amount as usize),
                },
                _ => MouseEventTrigger::Down {
                    streak: 1,
                    button: MouseButton::WheelDown(-amount as usize),
                },
            }),
            WMEK::HorzWheel(amount) => Some(match *amount {
                0 => return,
                1.. => MouseEventTrigger::Down {
                    streak: 1,
                    button: MouseButton::WheelLeft(*amount as usize),
                },
                _ => MouseEventTrigger::Down {
                    streak: 1,
                    button: MouseButton::WheelRight(-amount as usize),
                },
            }),
        };

        if allow_action {
            if let Some(mut event_trigger_type) = event_trigger_type {
                self.current_event = Some(event_trigger_type.to_dynamic());
                let mut modifiers = event.modifiers;

                // Since we use shift to force assessing the mouse bindings, pretend
                // that shift is not one of the mods when the mouse is grabbed.
                let mut mouse_reporting = pane.is_mouse_grabbed();
                if mouse_reporting {
                    if modifiers.contains(self.config.bypass_mouse_reporting_modifiers) {
                        modifiers.remove(self.config.bypass_mouse_reporting_modifiers);
                        mouse_reporting = false;
                    }
                }

                if mouse_reporting {
                    // If they were scrolled back prior to launching an
                    // application that captures the mouse, then mouse based
                    // scrolling assignments won't have any effect.
                    // Ensure that we scroll to the bottom if they try to
                    // use the mouse so that things are less surprising
                    self.scroll_to_bottom(&pane);
                }

                // normalize delta and streak to make mouse assignment
                // easier to wrangle
                match event_trigger_type {
                    MouseEventTrigger::Down {
                        ref mut streak,
                        button:
                            MouseButton::WheelUp(ref mut delta)
                            | MouseButton::WheelDown(ref mut delta)
                            | MouseButton::WheelLeft(ref mut delta)
                            | MouseButton::WheelRight(ref mut delta),
                    }
                    | MouseEventTrigger::Up {
                        ref mut streak,
                        button:
                            MouseButton::WheelUp(ref mut delta)
                            | MouseButton::WheelDown(ref mut delta)
                            | MouseButton::WheelLeft(ref mut delta)
                            | MouseButton::WheelRight(ref mut delta),
                    }
                    | MouseEventTrigger::Drag {
                        ref mut streak,
                        button:
                            MouseButton::WheelUp(ref mut delta)
                            | MouseButton::WheelDown(ref mut delta)
                            | MouseButton::WheelLeft(ref mut delta)
                            | MouseButton::WheelRight(ref mut delta),
                    } => {
                        *streak = 1;
                        *delta = 1;
                    }
                    _ => {}
                };

                let mouse_mods = config::MouseEventTriggerMods {
                    mods: modifiers,
                    mouse_reporting,
                    alt_screen: if pane.is_alt_screen_active() {
                        MouseEventAltScreen::True
                    } else {
                        MouseEventAltScreen::False
                    },
                };

                if let Some(action) = self.input_map.lookup_mouse(event_trigger_type, mouse_mods) {
                    self.perform_key_assignment(&pane, &action).ok();
                    return;
                }
            }
        }

        if allow_action
            && matches!(event.kind, WMEK::Release(MousePress::Right))
            && !pane.is_mouse_grabbed()
        {
            context.show_context_menu(event.coords, self.terminal_context_menu_items());
            return;
        }

        let mouse_event = wezterm_term::MouseEvent {
            kind: match event.kind {
                WMEK::Move => TMEK::Move,
                WMEK::VertWheel(_) | WMEK::HorzWheel(_) | WMEK::Press(_) => TMEK::Press,
                WMEK::Release(_) => TMEK::Release,
            },
            button: match event.kind {
                WMEK::Release(ref press) | WMEK::Press(ref press) => mouse_press_to_tmb(press),
                WMEK::Move => {
                    if event.mouse_buttons == WMB::LEFT {
                        TMB::Left
                    } else if event.mouse_buttons == WMB::RIGHT {
                        TMB::Right
                    } else if event.mouse_buttons == WMB::MIDDLE {
                        TMB::Middle
                    } else {
                        TMB::None
                    }
                }
                WMEK::VertWheel(amount) => {
                    if amount > 0 {
                        TMB::WheelUp(amount as usize)
                    } else {
                        TMB::WheelDown((-amount) as usize)
                    }
                }
                WMEK::HorzWheel(amount) => {
                    if amount > 0 {
                        TMB::WheelLeft(amount as usize)
                    } else {
                        TMB::WheelRight((-amount) as usize)
                    }
                }
            },
            x: column,
            y: row,
            x_pixel_offset,
            y_pixel_offset,
            modifiers: event.modifiers,
        };

        if allow_action
            && !(self.config.swallow_mouse_click_on_pane_focus && is_click_to_focus_pane)
        {
            pane.mouse_event(mouse_event).ok();
        }

        match event.kind {
            WMEK::Move => {}
            _ => {
                context.invalidate();
            }
        }
    }
}

fn mouse_press_to_tmb(press: &MousePress) -> TMB {
    match press {
        MousePress::Left => TMB::Left,
        MousePress::Right => TMB::Right,
        MousePress::Middle => TMB::Middle,
    }
}
