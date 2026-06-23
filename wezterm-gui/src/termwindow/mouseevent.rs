use crate::frontend::front_end;
use crate::tabbar::TabBarItem;
use crate::termwindow::content_view::ContentViewId;
use crate::termwindow::ui::pane_nav_bar_height_for_metrics;
use crate::termwindow::ui::platform_chrome::WindowTabChromeParams;
use crate::termwindow::ui::tokens::{
    PANE_NAV_BUTTON_GAP, PANE_NAV_INSET, PANE_NAV_TAB_GAP, TAB_ROW_START_PADDING,
    TAB_VERTICAL_PADDING, WINDOW_TAB_ACTION_RESERVED_WIDTH, WINDOW_TAB_GAP,
    WINDOW_TAB_LEADING_ACTION_BUTTON_SIZE, WINDOW_TAB_LEADING_ACTION_GAP,
};
use crate::termwindow::{
    GuiWin, MouseCapture, PaneNavAction, PositionedSplit, ScrollHit, TabWheelSurface,
    TermWindowNotif, UIItem, UIItemType, TMB,
};
use ::window::{
    ContextMenuItem, IntegratedTitleButtonStyle, MouseButtons as WMB, MouseCursor, MouseEvent,
    MouseEventKind as WMEK, MousePress, WindowDecorations, WindowOps, WindowState,
};
use config::keyassignment::{
    ClipboardCopyDestination, ClipboardPasteSource, KeyAssignment, MouseEventTrigger,
    PaneDirection, SpawnCommand, SpawnTabDomain, SplitPane, SplitSize,
};
use config::{MouseEventAltScreen, TermConfig};
use mux::pane::{Pane, WithPaneLines};
use mux::ssh::{RemoteSshDomain, SshConnectionStatus};
use mux::tab::{PositionedPane, SplitDirection};
use mux::window::WindowId as MuxWindowId;
use mux::Mux;
use mux_lua::MuxPane;
use std::convert::TryInto;
use std::ops::Sub;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use termwiz::hyperlink::Hyperlink;
use termwiz::surface::Line;
use wezterm_dynamic::ToDynamic;
use wezterm_term::input::{MouseButton, MouseEventKind as TMEK};
use wezterm_term::{ClickPosition, LastMouseClick, StableRowIndex};

const TAB_WHEEL_SURFACE_LOCK_MS: u64 = 700;
const TAB_WHEEL_DIRECTION_LOCK_MS: u64 = 140;
/// Overall ceiling on the "Connecting…" phase, as a backstop for the case where
/// the TCP connect succeeds but the SSH banner/handshake then stalls (the
/// per-connect `connecttimeout` only bounds the TCP connect itself).
const REMOTE_CONNECT_OVERALL_TIMEOUT_SECS: u64 = 20;

impl super::TermWindow {
    pub(crate) fn collapsed_pane_min_cells(&self) -> usize {
        let nav_height = pane_nav_bar_height_for_metrics(self.render_metrics);
        let cell_height = self.render_metrics.cell_size.height.max(1) as usize;

        nav_height.div_ceil(cell_height).max(2)
    }

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

    fn window_tab_chrome_params(&self) -> WindowTabChromeParams {
        WindowTabChromeParams {
            use_fancy_tab_bar: self.config.use_fancy_tab_bar,
            workspace_sidebar_width: self.workspace_sidebar_width(),
            window_state: self.window_state,
            window_decorations: self.config.window_decorations,
            integrated_title_button_alignment: self.config.integrated_title_button_alignment,
            integrated_title_button_style: self.config.integrated_title_button_style,
            cell_width: self.render_metrics.cell_size.width.max(1) as f32,
        }
    }

    pub(super) fn window_tab_leading_action_slot_count(&self) -> usize {
        self.window_tab_chrome_params().leading_action_slot_count()
    }

    pub(super) fn window_tab_shows_sidebar_toggle_action(&self) -> bool {
        self.window_tab_chrome_params()
            .shows_sidebar_toggle_action()
    }

    pub(super) fn window_tab_sidebar_toggle_uses_fullscreen_style(&self) -> bool {
        self.window_tab_chrome_params()
            .sidebar_toggle_uses_fullscreen_style()
    }

    pub(super) fn window_tab_sidebar_toggle_button_size(&self) -> usize {
        self.window_tab_chrome_params().sidebar_toggle_button_size()
    }

    pub(super) fn window_tab_sidebar_toggle_icon_size(&self) -> usize {
        self.window_tab_chrome_params().sidebar_toggle_icon_size()
    }

    pub(super) fn window_tab_leading_action_start_pixels(&self) -> f32 {
        self.window_tab_chrome_params()
            .leading_action_start_pixels()
    }

    pub(super) fn window_tab_left_padding_pixels(&self) -> f32 {
        self.window_tab_chrome_params().left_padding_pixels()
    }

    pub(super) fn window_tab_trailing_action_reserved_width(&self) -> usize {
        let sidebar_actions_width = if self.right_sidebar_width() > 0 {
            WINDOW_TAB_ACTION_RESERVED_WIDTH
                + if !cfg!(target_os = "macos") {
                    WINDOW_TAB_LEADING_ACTION_BUTTON_SIZE + WINDOW_TAB_LEADING_ACTION_GAP
                } else {
                    0
                }
        } else {
            WINDOW_TAB_ACTION_RESERVED_WIDTH
                + WINDOW_TAB_LEADING_ACTION_BUTTON_SIZE
                + WINDOW_TAB_LEADING_ACTION_GAP
        };

        sidebar_actions_width
            + if self.right_sidebar_width() == 0
                && self
                    .config
                    .window_decorations
                    .contains(WindowDecorations::INTEGRATED_BUTTONS)
                && self.config.integrated_title_button_style
                    != IntegratedTitleButtonStyle::MacOsNative
            {
                self.config.integrated_title_buttons.len()
                    * (WINDOW_TAB_LEADING_ACTION_BUTTON_SIZE + WINDOW_TAB_LEADING_ACTION_GAP / 2)
                    + WINDOW_TAB_LEADING_ACTION_GAP
            } else {
                0
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
            .saturating_sub(self.right_sidebar_width())
            .saturating_sub(border.right.get() as usize)
            .saturating_sub(left_padding.max(0.0) as usize)
            .saturating_sub(if self.config.use_fancy_tab_bar {
                TAB_ROW_START_PADDING + self.window_tab_trailing_action_reserved_width()
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
        let window_tab_count = if self.active_content_view_is_remote_thread() {
            0
        } else {
            window.len()
        };
        let tab_count = window_tab_count + self.content_view_count();
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
            WMEK::VertWheel(amount) => Some(amount),
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

    fn mouse_wheel_right_sidebar(&mut self, event: &MouseEvent, context: &dyn WindowOps) -> bool {
        if self.right_sidebar_mode == super::RightSidebarMode::Chat {
            if let Some(rect) = self.right_sidebar_file_preview_rect() {
                let x = event.coords.x;
                let y = event.coords.y;
                if x >= rect.x as isize
                    && x < rect.x.saturating_add(rect.width) as isize
                    && y >= rect.y as isize
                    && y < rect.y.saturating_add(rect.height) as isize
                {
                    let changed = match event.kind {
                        WMEK::VertWheel(amount) if amount != 0 => {
                            self.scroll_right_sidebar_file_preview(amount)
                        }
                        WMEK::HorzWheel(amount) if amount != 0 => {
                            self.scroll_right_sidebar_file_preview_horizontal(amount)
                        }
                        WMEK::VertWheel(_) | WMEK::HorzWheel(_) => false,
                        _ => return false,
                    };
                    if changed {
                        context.invalidate();
                    }
                    return true;
                }
            }
        }

        let Some(rect) = self.right_sidebar_rect() else {
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
            WMEK::HorzWheel(_) => return true,
            _ => return false,
        };
        if amount == 0 {
            return true;
        }

        if self.right_sidebar_mode == super::RightSidebarMode::Chat {
            if self.scroll_right_sidebar_files(amount) {
                context.invalidate();
            }
        } else {
            let was_visible = self.right_sidebar_snippet_scrollbar_visible_until;
            self.show_right_sidebar_snippet_scrollbar();
            if self.scroll_right_sidebar_snippets(amount)
                || was_visible != self.right_sidebar_snippet_scrollbar_visible_until
            {
                context.invalidate();
            }
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
            | UIItemType::SpaceMenu
            | UIItemType::ProjectToggleThreads(_)
            | UIItemType::Project(_)
            | UIItemType::WorkspaceThread(_)
            | UIItemType::WorkspaceThreadPin(_)
            | UIItemType::WorkspaceThreadDelete(_)
            | UIItemType::WorkspaceThreadNew(_)
            | UIItemType::WorkspaceSidebarToggle
            | UIItemType::WorkspaceSidebarScrollTrack
            | UIItemType::WorkspaceSidebarScrollThumb
            | UIItemType::WorkspaceSidebarHeaderBlank
            | UIItemType::WorkspaceSidebarBackground
            | UIItemType::WorkspaceSidebarResize
            | UIItemType::WorkspaceSidebarSettings
            | UIItemType::WorkspaceSidebarViewOptions
            | UIItemType::WorkspaceSidebarSshHosts
            | UIItemType::WorkspaceSidebarNotifications
            | UIItemType::RightSidebarToggle
            | UIItemType::RightSidebarMode(_)
            | UIItemType::RightSidebarBackground
            | UIItemType::RightSidebarResize
            | UIItemType::RightSidebarFilePreviewResize
            | UIItemType::RightSidebarSnippetNew
            | UIItemType::RightSidebarSnippetBack
            | UIItemType::RightSidebarSnippetSave
            | UIItemType::RightSidebarSnippetSearch
            | UIItemType::RightSidebarSnippetTitle
            | UIItemType::RightSidebarSnippetBody
            | UIItemType::RightSidebarSnippetEdit(_)
            | UIItemType::RightSidebarSnippetPaste(_)
            | UIItemType::RightSidebarSnippetRun(_)
            | UIItemType::RightSidebarSnippetDelete(_)
            | UIItemType::RightSidebarSnippetScrollTrack
            | UIItemType::RightSidebarSnippetScrollThumb
            | UIItemType::RightSidebarFilePreviewScrollTrack
            | UIItemType::RightSidebarFilePreviewScrollThumb
            | UIItemType::RightSidebarFilePreviewHorizontalScrollTrack
            | UIItemType::RightSidebarFilePreviewHorizontalScrollThumb
            | UIItemType::RightSidebarFilePreviewText
            | UIItemType::RightSidebarFileFilter
            | UIItemType::RightSidebarFileRow(_)
            | UIItemType::RightSidebarFileBack
            | UIItemType::RightSidebarFileOpen
            | UIItemType::RightSidebarFileOpenMenu
            | UIItemType::RightSidebarFileReveal
            | UIItemType::RightSidebarFileCopyText
            | UIItemType::ContextMenuBackdrop
            | UIItemType::ContextMenuItem(_)
            | UIItemType::AboveScrollThumb
            | UIItemType::BelowScrollThumb
            | UIItemType::ScrollThumb
            | UIItemType::Split(_)
            | UIItemType::ContentViewClose(_) => {}
        }
    }

    fn enter_ui_item(&mut self, item: &UIItem) {
        match item.item_type {
            UIItemType::TabBar(_) => {}
            UIItemType::CloseTab(_)
            | UIItemType::PaneNav { .. }
            | UIItemType::ProjectNew
            | UIItemType::SpaceMenu
            | UIItemType::ProjectToggleThreads(_)
            | UIItemType::Project(_)
            | UIItemType::WorkspaceThread(_)
            | UIItemType::WorkspaceThreadPin(_)
            | UIItemType::WorkspaceThreadDelete(_)
            | UIItemType::WorkspaceThreadNew(_)
            | UIItemType::WorkspaceSidebarToggle
            | UIItemType::WorkspaceSidebarScrollTrack
            | UIItemType::WorkspaceSidebarScrollThumb
            | UIItemType::WorkspaceSidebarHeaderBlank
            | UIItemType::WorkspaceSidebarBackground
            | UIItemType::WorkspaceSidebarResize
            | UIItemType::WorkspaceSidebarSettings
            | UIItemType::WorkspaceSidebarViewOptions
            | UIItemType::WorkspaceSidebarSshHosts
            | UIItemType::WorkspaceSidebarNotifications
            | UIItemType::RightSidebarToggle
            | UIItemType::RightSidebarMode(_)
            | UIItemType::RightSidebarBackground
            | UIItemType::RightSidebarResize
            | UIItemType::RightSidebarFilePreviewResize
            | UIItemType::RightSidebarSnippetNew
            | UIItemType::RightSidebarSnippetBack
            | UIItemType::RightSidebarSnippetSave
            | UIItemType::RightSidebarSnippetSearch
            | UIItemType::RightSidebarSnippetTitle
            | UIItemType::RightSidebarSnippetBody
            | UIItemType::RightSidebarSnippetEdit(_)
            | UIItemType::RightSidebarSnippetPaste(_)
            | UIItemType::RightSidebarSnippetRun(_)
            | UIItemType::RightSidebarSnippetDelete(_)
            | UIItemType::RightSidebarSnippetScrollTrack
            | UIItemType::RightSidebarSnippetScrollThumb
            | UIItemType::RightSidebarFilePreviewScrollTrack
            | UIItemType::RightSidebarFilePreviewScrollThumb
            | UIItemType::RightSidebarFilePreviewHorizontalScrollTrack
            | UIItemType::RightSidebarFilePreviewHorizontalScrollThumb
            | UIItemType::RightSidebarFilePreviewText
            | UIItemType::RightSidebarFileFilter
            | UIItemType::RightSidebarFileRow(_)
            | UIItemType::RightSidebarFileBack
            | UIItemType::RightSidebarFileOpen
            | UIItemType::RightSidebarFileOpenMenu
            | UIItemType::RightSidebarFileReveal
            | UIItemType::RightSidebarFileCopyText
            | UIItemType::ContextMenuBackdrop
            | UIItemType::ContextMenuItem(_)
            | UIItemType::AboveScrollThumb
            | UIItemType::BelowScrollThumb
            | UIItemType::ScrollThumb
            | UIItemType::Split(_)
            | UIItemType::ContentViewClose(_) => {}
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
        let pane = self.get_active_pane_or_overlay();

        self.current_mouse_event.replace(event.clone());

        if self.consume_context_menu_suppressed_release(&event) {
            return;
        }

        if let Some(pane) = pane.as_ref() {
            if self.mouse_event_context_menu(&event, pane, context) {
                return;
            }
        } else if matches!(
            self.current_mouse_capture,
            Some(MouseCapture::TerminalPane(_))
        ) {
            self.current_mouse_capture = None;
        }

        if pane.is_none()
            && matches!(
                self.current_mouse_capture,
                None | Some(MouseCapture::TerminalPane(_))
            )
            && !self.content_view_foreground()
        {
            self.current_mouse_capture = None;
        }

        if self.mouse_wheel_right_sidebar(&event, context) {
            return;
        }

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
        let pane_mouse_grabbed = pane.as_ref().is_some_and(|pane| pane.is_mouse_grabbed());
        let x = if !pane_mouse_grabbed {
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
                    if completed_drag.as_ref().is_some_and(|(item, _)| {
                        item.item_type == UIItemType::RightSidebarResize
                            && self.right_sidebar_file_preview_rect().is_none()
                    }) {
                        self.persist_right_sidebar_width();
                    }
                    if completed_drag.as_ref().is_some_and(|(item, _)| {
                        item.item_type == UIItemType::RightSidebarResize
                            && self.right_sidebar_file_preview_rect().is_some()
                    }) {
                        self.persist_right_sidebar_file_preview_width();
                    }
                    if completed_drag.as_ref().is_some_and(|(item, _)| {
                        item.item_type == UIItemType::RightSidebarFilePreviewResize
                    }) {
                        self.persist_right_sidebar_file_preview_width();
                    }
                    if completed_drag
                        .as_ref()
                        .is_some_and(|(item, _)| matches!(item.item_type, UIItemType::Split(_)))
                    {
                        self.persist_workspace_layout_after_mutation("split drag released");
                    }
                    if completed_drag.as_ref().is_some_and(|(item, _)| {
                        item.item_type == UIItemType::RightSidebarFilePreviewText
                    }) {
                        self.update_right_sidebar_file_preview_selection(
                            event.coords.x,
                            event.coords.y,
                        );
                        context.invalidate();
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
            if let Some(pane) = pane.clone() {
                self.mouse_event_ui_item(item, pane, y, event, context);
            } else {
                self.mouse_event_ui_item_without_pane(item, event, context);
            }
        } else if self.content_view_foreground() {
            // The content view owns the content area; route by pixel coords.
            context.set_cursor(Some(MouseCursor::Arrow));
            let px = event.coords.x as f32;
            let py = event.coords.y as f32;
            let area = self.content_view_area();
            if px >= area.min_x() && px < area.max_x() && py >= area.min_y() && py < area.max_y() {
                let resp = self
                    .active_content_view_mut()
                    .map(|v| v.on_mouse(px, py, event.kind));
                if let Some(resp) = resp {
                    self.handle_content_response(resp);
                }
            }
        } else if matches!(
            self.current_mouse_capture,
            None | Some(MouseCapture::TerminalPane(_))
        ) {
            let Some(pane) = pane else {
                context.set_cursor(Some(MouseCursor::Arrow));
                context.invalidate();
                if prior_ui_item != ui_item {
                    self.update_title_post_status();
                }
                return;
            };
            if event.kind == WMEK::Press(MousePress::Left) && self.right_sidebar_has_text_focus() {
                self.clear_right_sidebar_text_focus();
                context.invalidate();
            }
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
        let preserve_cursor = self.current_mouse_event.as_ref().is_some_and(|event| {
            event.coords.x >= 0
                && event.coords.y >= 0
                && event.coords.x as usize <= self.dimensions.pixel_width
                && event.coords.y as usize <= self.dimensions.pixel_height
        });
        self.current_mouse_event = None;
        self.update_title();
        if !preserve_cursor {
            context.set_cursor(Some(MouseCursor::Arrow));
        }
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

    fn set_right_sidebar_snippet_scroll_from_thumb_top(
        &mut self,
        thumb_top: f32,
        context: &dyn WindowOps,
    ) {
        let Some(scroll) = self.right_sidebar_snippet_scroll_geometry() else {
            return;
        };
        let travel = (scroll.track_height as f32 - scroll.thumb_height).max(1.0);
        let relative_top = (thumb_top - scroll.track_y as f32).clamp(0.0, travel);
        let offset = (relative_top / travel * scroll.max_scroll).clamp(0.0, scroll.max_scroll);
        self.show_right_sidebar_snippet_scrollbar();
        if (offset - self.right_sidebar_snippet_scroll_offset).abs() > f32::EPSILON {
            self.right_sidebar_snippet_scroll_offset = offset;
        }
        context.invalidate();
    }

    fn drag_right_sidebar_snippet_scroll_thumb(
        &mut self,
        item: UIItem,
        start_event: MouseEvent,
        event: MouseEvent,
        context: &dyn WindowOps,
    ) {
        let from_top = start_event.coords.y.saturating_sub(item.y as isize) as f32;
        let thumb_top = event.coords.y as f32 - from_top;
        self.set_right_sidebar_snippet_scroll_from_thumb_top(thumb_top, context);
        context.set_cursor(Some(MouseCursor::Hand));
        self.dragging.replace((item, start_event));
    }

    fn set_right_sidebar_file_preview_scroll_from_thumb_top(
        &mut self,
        thumb_top: f32,
        context: &dyn WindowOps,
    ) {
        let Some(scroll) = self.right_sidebar_file_preview_scroll_geometry() else {
            return;
        };
        let travel = (scroll.track_height as f32 - scroll.thumb_height).max(1.0);
        let relative_top = (thumb_top - scroll.track_y as f32).clamp(0.0, travel);
        let offset = (relative_top / travel * scroll.max_scroll).clamp(0.0, scroll.max_scroll);
        if (offset - self.right_sidebar_file_preview_scroll_offset).abs() > f32::EPSILON {
            self.right_sidebar_file_preview_scroll_offset = offset;
        }
        context.invalidate();
    }

    fn drag_right_sidebar_file_preview_scroll_thumb(
        &mut self,
        item: UIItem,
        start_event: MouseEvent,
        event: MouseEvent,
        context: &dyn WindowOps,
    ) {
        let from_top = start_event.coords.y.saturating_sub(item.y as isize) as f32;
        let thumb_top = event.coords.y as f32 - from_top;
        self.set_right_sidebar_file_preview_scroll_from_thumb_top(thumb_top, context);
        context.set_cursor(Some(MouseCursor::Hand));
        self.dragging.replace((item, start_event));
    }

    fn set_right_sidebar_file_preview_horizontal_scroll_from_thumb_left(
        &mut self,
        thumb_left: f32,
        context: &dyn WindowOps,
    ) {
        let Some(scroll) = self.right_sidebar_file_preview_horizontal_scroll_geometry() else {
            return;
        };
        let travel = (scroll.track_width as f32 - scroll.thumb_width).max(1.0);
        let relative_left = (thumb_left - scroll.track_x as f32).clamp(0.0, travel);
        let offset = (relative_left / travel * scroll.max_scroll as f32)
            .round()
            .clamp(0.0, scroll.max_scroll as f32) as usize;
        if offset != self.right_sidebar_file_preview_horizontal_offset {
            self.right_sidebar_file_preview_horizontal_offset = offset;
        }
        context.invalidate();
    }

    fn drag_right_sidebar_file_preview_horizontal_scroll_thumb(
        &mut self,
        item: UIItem,
        start_event: MouseEvent,
        event: MouseEvent,
        context: &dyn WindowOps,
    ) {
        let from_left = start_event.coords.x.saturating_sub(item.x as isize) as f32;
        let thumb_left = event.coords.x as f32 - from_left;
        self.set_right_sidebar_file_preview_horizontal_scroll_from_thumb_left(thumb_left, context);
        context.set_cursor(Some(MouseCursor::Hand));
        self.dragging.replace((item, start_event));
    }

    fn drag_right_sidebar_file_preview_text_selection(
        &mut self,
        item: UIItem,
        start_event: MouseEvent,
        event: MouseEvent,
        context: &dyn WindowOps,
    ) {
        self.update_right_sidebar_file_preview_selection(event.coords.x, event.coords.y);
        context.set_cursor(Some(MouseCursor::Text));
        context.invalidate();
        self.dragging.replace((item, start_event));
    }

    fn drag_right_sidebar_input_selection(
        &mut self,
        item: UIItem,
        start_event: MouseEvent,
        event: MouseEvent,
        context: &dyn WindowOps,
    ) {
        self.position_right_sidebar_input_caret(&item.item_type, event.coords.x, true);
        context.set_cursor(Some(MouseCursor::Text));
        context.invalidate();
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
            UIItemType::RightSidebarResize => {
                self.drag_right_sidebar_resize(item, start_event, event, context);
            }
            UIItemType::RightSidebarFilePreviewResize => {
                self.drag_right_sidebar_file_preview_resize(item, start_event, event, context);
            }
            UIItemType::RightSidebarSnippetScrollThumb => {
                self.drag_right_sidebar_snippet_scroll_thumb(item, start_event, event, context);
            }
            UIItemType::RightSidebarFilePreviewScrollThumb => {
                self.drag_right_sidebar_file_preview_scroll_thumb(
                    item,
                    start_event,
                    event,
                    context,
                );
            }
            UIItemType::RightSidebarFilePreviewHorizontalScrollThumb => {
                self.drag_right_sidebar_file_preview_horizontal_scroll_thumb(
                    item,
                    start_event,
                    event,
                    context,
                );
            }
            UIItemType::RightSidebarFilePreviewText => {
                self.drag_right_sidebar_file_preview_text_selection(
                    item,
                    start_event,
                    event,
                    context,
                );
            }
            UIItemType::RightSidebarFileFilter
            | UIItemType::RightSidebarSnippetSearch
            | UIItemType::RightSidebarSnippetTitle => {
                self.drag_right_sidebar_input_selection(item, start_event, event, context);
            }
            UIItemType::WorkspaceSidebarScrollThumb => {
                self.drag_workspace_sidebar_scroll_thumb(item, start_event, event, context);
            }
            UIItemType::ContextMenuBackdrop | UIItemType::ContextMenuItem(_) => {}
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

    fn drag_right_sidebar_resize(
        &mut self,
        item: UIItem,
        start_event: MouseEvent,
        event: MouseEvent,
        context: &dyn WindowOps,
    ) {
        let right_edge =
            self.dimensions
                .pixel_width
                .saturating_sub(self.get_os_border().right.get() as usize) as isize;
        let width = right_edge.saturating_sub(event.coords.x).max(0) as usize;
        self.expand_right_sidebar();
        if self.right_sidebar_file_preview_rect().is_some() {
            self.set_right_sidebar_file_preview_total_width(width);
        } else {
            self.set_right_sidebar_width(width);
        }
        self.reflow_right_sidebar(context);
        context.set_cursor(Some(MouseCursor::SizeLeftRight));
        self.dragging.replace((item, start_event));
    }

    fn drag_right_sidebar_file_preview_resize(
        &mut self,
        item: UIItem,
        start_event: MouseEvent,
        event: MouseEvent,
        context: &dyn WindowOps,
    ) {
        if self.set_right_sidebar_file_preview_split_x(event.coords.x) {
            context.invalidate();
        }
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
            UIItemType::TabBar(TabBarItem::NewTabButton { .. })
                if self.active_content_view_is_remote_thread() =>
            {
                self.mouse_event_disabled_new_tab_button(context);
            }
            UIItemType::TabBar(TabBarItem::NewTabButton { .. })
                if self.content_view_foreground() =>
            {
                self.mouse_event_local_new_tab_button(event, context);
            }
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
            UIItemType::SpaceMenu => {
                self.mouse_event_space_menu(item, event, context);
            }
            UIItemType::ProjectToggleThreads(project_id) => {
                self.mouse_event_project_toggle_threads(project_id, event, context);
            }
            UIItemType::Project(project_id) => {
                self.mouse_event_project(project_id, event, context);
            }
            UIItemType::WorkspaceThread(thread_id) => {
                self.mouse_event_workspace_thread(thread_id, event, context);
            }
            UIItemType::WorkspaceThreadPin(thread_id) => {
                self.mouse_event_workspace_thread_pin(thread_id, event, context);
            }
            UIItemType::WorkspaceThreadDelete(thread_id) => {
                self.mouse_event_workspace_thread_delete(thread_id, event, context);
            }
            UIItemType::WorkspaceThreadNew(project_id) => {
                self.mouse_event_workspace_thread_new(project_id, event, context);
            }
            UIItemType::WorkspaceSidebarToggle => {
                self.mouse_event_workspace_sidebar_toggle(event, context);
            }
            UIItemType::WorkspaceSidebarHeaderBlank => {
                self.mouse_event_workspace_sidebar_header_blank(event, context);
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
            UIItemType::RightSidebarToggle => {
                self.mouse_event_right_sidebar_toggle(event, context);
            }
            UIItemType::RightSidebarMode(mode) => {
                self.mouse_event_right_sidebar_mode(mode, event, context);
            }
            UIItemType::RightSidebarBackground => {
                context.set_cursor(Some(MouseCursor::Arrow));
                if event.kind == WMEK::Press(MousePress::Left) {
                    self.clear_right_sidebar_text_focus();
                    context.invalidate();
                }
            }
            UIItemType::RightSidebarResize => {
                self.mouse_event_right_sidebar_resize(item, event, context);
            }
            UIItemType::RightSidebarFilePreviewResize => {
                self.mouse_event_right_sidebar_file_preview_resize(item, event, context);
            }
            UIItemType::RightSidebarSnippetScrollTrack => {
                self.mouse_event_right_sidebar_snippet_scroll_track(item, event, context);
            }
            UIItemType::RightSidebarSnippetScrollThumb => {
                self.mouse_event_right_sidebar_snippet_scroll_thumb(item, event, context);
            }
            UIItemType::RightSidebarFilePreviewScrollTrack => {
                self.mouse_event_right_sidebar_file_preview_scroll_track(item, event, context);
            }
            UIItemType::RightSidebarFilePreviewScrollThumb => {
                self.mouse_event_right_sidebar_file_preview_scroll_thumb(item, event, context);
            }
            UIItemType::RightSidebarFilePreviewHorizontalScrollTrack => {
                self.mouse_event_right_sidebar_file_preview_horizontal_scroll_track(
                    item, event, context,
                );
            }
            UIItemType::RightSidebarFilePreviewHorizontalScrollThumb => {
                self.mouse_event_right_sidebar_file_preview_horizontal_scroll_thumb(
                    item, event, context,
                );
            }
            UIItemType::RightSidebarFilePreviewText => {
                self.mouse_event_right_sidebar_file_preview_text(item, event, context);
            }
            UIItemType::RightSidebarSnippetNew
            | UIItemType::RightSidebarSnippetBack
            | UIItemType::RightSidebarSnippetSave
            | UIItemType::RightSidebarSnippetSearch
            | UIItemType::RightSidebarSnippetTitle
            | UIItemType::RightSidebarSnippetBody
            | UIItemType::RightSidebarSnippetEdit(_)
            | UIItemType::RightSidebarSnippetPaste(_)
            | UIItemType::RightSidebarSnippetRun(_)
            | UIItemType::RightSidebarSnippetDelete(_) => {
                self.mouse_event_right_sidebar_snippet(item.clone(), event, context);
            }
            UIItemType::RightSidebarFileFilter
            | UIItemType::RightSidebarFileRow(_)
            | UIItemType::RightSidebarFileBack
            | UIItemType::RightSidebarFileOpen
            | UIItemType::RightSidebarFileOpenMenu
            | UIItemType::RightSidebarFileReveal
            | UIItemType::RightSidebarFileCopyText => {
                self.mouse_event_right_sidebar_file(item.clone(), event, context);
            }
            UIItemType::WorkspaceSidebarSettings => {
                self.mouse_event_workspace_sidebar_settings(event, context);
            }
            UIItemType::WorkspaceSidebarSshHosts => {
                context.set_cursor(Some(MouseCursor::Hand));
                if event.kind == WMEK::Press(MousePress::Left) {
                    self.toggle_ssh_hosts_view();
                }
            }
            UIItemType::ContentViewClose(id) => {
                context.set_cursor(Some(MouseCursor::Hand));
                if event.kind == WMEK::Press(MousePress::Left) {
                    self.request_close_content_view_by_id(id);
                }
            }
            UIItemType::WorkspaceSidebarViewOptions => {
                self.mouse_event_workspace_sidebar_view_options(item, event, context);
            }
            UIItemType::WorkspaceSidebarNotifications => {
                context.set_cursor(Some(MouseCursor::Hand));
            }
            UIItemType::ContextMenuBackdrop => {
                context.set_cursor(Some(MouseCursor::Arrow));
            }
            UIItemType::ContextMenuItem(_) => {
                context.set_cursor(Some(MouseCursor::Hand));
            }
        }
    }

    fn mouse_event_ui_item_without_pane(
        &mut self,
        item: UIItem,
        event: MouseEvent,
        context: &dyn WindowOps,
    ) {
        self.last_ui_item.replace(item.clone());
        match item.item_type {
            UIItemType::TabBar(TabBarItem::NewTabButton { .. })
                if self.active_content_view_is_remote_thread() =>
            {
                self.mouse_event_disabled_new_tab_button(context);
            }
            UIItemType::TabBar(TabBarItem::NewTabButton { .. }) => {
                self.mouse_event_local_new_tab_button(event, context);
            }
            UIItemType::TabBar(item) => {
                self.mouse_event_tab_bar(item, event, context);
            }
            UIItemType::CloseTab(idx) => {
                self.mouse_event_close_tab(idx, event, context);
            }
            UIItemType::ProjectNew => {
                self.mouse_event_project_new(event, context);
            }
            UIItemType::SpaceMenu => {
                self.mouse_event_space_menu(item, event, context);
            }
            UIItemType::ProjectToggleThreads(project_id) => {
                self.mouse_event_project_toggle_threads(project_id, event, context);
            }
            UIItemType::Project(project_id) => {
                self.mouse_event_project(project_id, event, context);
            }
            UIItemType::WorkspaceThread(thread_id) => {
                self.mouse_event_workspace_thread(thread_id, event, context);
            }
            UIItemType::WorkspaceThreadPin(thread_id) => {
                self.mouse_event_workspace_thread_pin(thread_id, event, context);
            }
            UIItemType::WorkspaceThreadDelete(thread_id) => {
                self.mouse_event_workspace_thread_delete(thread_id, event, context);
            }
            UIItemType::WorkspaceThreadNew(project_id) => {
                self.mouse_event_workspace_thread_new(project_id, event, context);
            }
            UIItemType::WorkspaceSidebarToggle => {
                self.mouse_event_workspace_sidebar_toggle(event, context);
            }
            UIItemType::WorkspaceSidebarHeaderBlank => {
                self.mouse_event_workspace_sidebar_header_blank(event, context);
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
            UIItemType::WorkspaceSidebarSshHosts => {
                context.set_cursor(Some(MouseCursor::Hand));
                if event.kind == WMEK::Press(MousePress::Left) {
                    self.toggle_ssh_hosts_view();
                }
            }
            UIItemType::WorkspaceSidebarViewOptions => {
                self.mouse_event_workspace_sidebar_view_options(item, event, context);
            }
            UIItemType::WorkspaceSidebarNotifications => {
                context.set_cursor(Some(MouseCursor::Hand));
            }
            UIItemType::RightSidebarToggle => {
                self.mouse_event_right_sidebar_toggle(event, context);
            }
            UIItemType::RightSidebarMode(mode) => {
                self.mouse_event_right_sidebar_mode(mode, event, context);
            }
            UIItemType::RightSidebarBackground => {
                context.set_cursor(Some(MouseCursor::Arrow));
                if event.kind == WMEK::Press(MousePress::Left) {
                    self.clear_right_sidebar_text_focus();
                    context.invalidate();
                }
            }
            UIItemType::RightSidebarResize => {
                self.mouse_event_right_sidebar_resize(item, event, context);
            }
            UIItemType::RightSidebarFilePreviewResize => {
                self.mouse_event_right_sidebar_file_preview_resize(item, event, context);
            }
            UIItemType::RightSidebarSnippetScrollTrack => {
                self.mouse_event_right_sidebar_snippet_scroll_track(item, event, context);
            }
            UIItemType::RightSidebarSnippetScrollThumb => {
                self.mouse_event_right_sidebar_snippet_scroll_thumb(item, event, context);
            }
            UIItemType::RightSidebarFilePreviewScrollTrack => {
                self.mouse_event_right_sidebar_file_preview_scroll_track(item, event, context);
            }
            UIItemType::RightSidebarFilePreviewScrollThumb => {
                self.mouse_event_right_sidebar_file_preview_scroll_thumb(item, event, context);
            }
            UIItemType::RightSidebarFilePreviewHorizontalScrollTrack => {
                self.mouse_event_right_sidebar_file_preview_horizontal_scroll_track(
                    item, event, context,
                );
            }
            UIItemType::RightSidebarFilePreviewHorizontalScrollThumb => {
                self.mouse_event_right_sidebar_file_preview_horizontal_scroll_thumb(
                    item, event, context,
                );
            }
            UIItemType::RightSidebarFilePreviewText => {
                self.mouse_event_right_sidebar_file_preview_text(item, event, context);
            }
            UIItemType::RightSidebarSnippetNew
            | UIItemType::RightSidebarSnippetBack
            | UIItemType::RightSidebarSnippetSave
            | UIItemType::RightSidebarSnippetSearch
            | UIItemType::RightSidebarSnippetTitle
            | UIItemType::RightSidebarSnippetBody
            | UIItemType::RightSidebarSnippetEdit(_)
            | UIItemType::RightSidebarSnippetPaste(_)
            | UIItemType::RightSidebarSnippetRun(_)
            | UIItemType::RightSidebarSnippetDelete(_) => {
                self.mouse_event_right_sidebar_snippet(item.clone(), event, context);
            }
            UIItemType::RightSidebarFileFilter
            | UIItemType::RightSidebarFileRow(_)
            | UIItemType::RightSidebarFileBack
            | UIItemType::RightSidebarFileOpen
            | UIItemType::RightSidebarFileOpenMenu
            | UIItemType::RightSidebarFileReveal
            | UIItemType::RightSidebarFileCopyText => {
                self.mouse_event_right_sidebar_file(item.clone(), event, context);
            }
            UIItemType::ContentViewClose(id) => {
                context.set_cursor(Some(MouseCursor::Hand));
                if event.kind == WMEK::Press(MousePress::Left) {
                    self.request_close_content_view_by_id(id);
                }
            }
            UIItemType::ContextMenuBackdrop => {
                context.set_cursor(Some(MouseCursor::Arrow));
            }
            UIItemType::ContextMenuItem(_) => {
                context.set_cursor(Some(MouseCursor::Hand));
            }
            UIItemType::AboveScrollThumb
            | UIItemType::ScrollThumb
            | UIItemType::BelowScrollThumb
            | UIItemType::Split(_)
            | UIItemType::PaneNav { .. } => {
                context.set_cursor(Some(MouseCursor::Arrow));
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

    fn reflow_right_sidebar(&mut self, context: &dyn WindowOps) {
        if let Some(window) = self.window.as_ref().cloned() {
            let dimensions = self.dimensions;
            self.apply_dimensions(&dimensions, None, &window);
        }
        context.invalidate();
    }

    fn invalidate_or_reflow_right_sidebar(
        &mut self,
        previous_width: usize,
        context: &dyn WindowOps,
    ) {
        if self.right_sidebar_width() != previous_width {
            self.reflow_right_sidebar(context);
        } else {
            context.invalidate();
        }
    }

    pub fn mouse_event_workspace_sidebar_resize(
        &mut self,
        item: UIItem,
        event: MouseEvent,
        context: &dyn WindowOps,
    ) {
        context.set_cursor(Some(MouseCursor::SizeLeftRight));
        if matches!(event.kind, WMEK::Press(MousePress::Left)) {
            self.dragging.replace((item, event));
        }
    }

    pub fn mouse_event_workspace_sidebar_header_blank(
        &mut self,
        event: MouseEvent,
        context: &dyn WindowOps,
    ) {
        self.mouse_event_window_header_blank(event, context);
    }

    fn mouse_event_window_header_blank(&mut self, event: MouseEvent, context: &dyn WindowOps) {
        context.set_cursor(Some(MouseCursor::Arrow));
        if event.kind != WMEK::Press(MousePress::Left) {
            return;
        }

        let maximized = self
            .window_state
            .intersects(WindowState::MAXIMIZED | WindowState::FULL_SCREEN);
        if self.last_mouse_click.as_ref().map(|c| c.streak) == Some(2) {
            if self.window_state.contains(WindowState::FULL_SCREEN) {
                return;
            }
            if let Some(ref window) = self.window {
                if self.window_state.contains(WindowState::MAXIMIZED) {
                    window.restore();
                } else {
                    window.maximize();
                }
            }
            return;
        }

        if !maximized {
            #[cfg(target_os = "macos")]
            {
                context.request_drag_move();
            }
            #[cfg(not(target_os = "macos"))]
            self.window_drag_position.replace(event);
            #[cfg(not(target_os = "macos"))]
            context.request_drag_move();
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

    pub fn mouse_event_right_sidebar_resize(
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

    pub fn mouse_event_right_sidebar_file_preview_resize(
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

    pub fn mouse_event_right_sidebar_snippet_scroll_thumb(
        &mut self,
        item: UIItem,
        event: MouseEvent,
        context: &dyn WindowOps,
    ) {
        context.set_cursor(Some(MouseCursor::Hand));
        self.show_right_sidebar_snippet_scrollbar();
        if event.kind == WMEK::Press(MousePress::Left) {
            self.dragging.replace((item, event));
            context.invalidate();
        }
    }

    pub fn mouse_event_right_sidebar_snippet_scroll_track(
        &mut self,
        _item: UIItem,
        event: MouseEvent,
        context: &dyn WindowOps,
    ) {
        context.set_cursor(Some(MouseCursor::Hand));
        self.show_right_sidebar_snippet_scrollbar();
        if event.kind == WMEK::Press(MousePress::Left) {
            if let Some(scroll) = self.right_sidebar_snippet_scroll_geometry() {
                let thumb_top = event.coords.y as f32 - scroll.thumb_height / 2.0;
                self.set_right_sidebar_snippet_scroll_from_thumb_top(thumb_top, context);
            }
            context.invalidate();
        }
    }

    pub fn mouse_event_right_sidebar_file_preview_scroll_thumb(
        &mut self,
        item: UIItem,
        event: MouseEvent,
        context: &dyn WindowOps,
    ) {
        context.set_cursor(Some(MouseCursor::Hand));
        if event.kind == WMEK::Press(MousePress::Left) {
            self.dragging.replace((item, event));
            context.invalidate();
        }
    }

    pub fn mouse_event_right_sidebar_file_preview_scroll_track(
        &mut self,
        _item: UIItem,
        event: MouseEvent,
        context: &dyn WindowOps,
    ) {
        context.set_cursor(Some(MouseCursor::Hand));
        if event.kind == WMEK::Press(MousePress::Left) {
            if let Some(scroll) = self.right_sidebar_file_preview_scroll_geometry() {
                let thumb_top = event.coords.y as f32 - scroll.thumb_height / 2.0;
                self.set_right_sidebar_file_preview_scroll_from_thumb_top(thumb_top, context);
            }
            context.invalidate();
        }
    }

    pub fn mouse_event_right_sidebar_file_preview_horizontal_scroll_thumb(
        &mut self,
        item: UIItem,
        event: MouseEvent,
        context: &dyn WindowOps,
    ) {
        context.set_cursor(Some(MouseCursor::Hand));
        if event.kind == WMEK::Press(MousePress::Left) {
            self.dragging.replace((item, event));
            context.invalidate();
        }
    }

    pub fn mouse_event_right_sidebar_file_preview_horizontal_scroll_track(
        &mut self,
        _item: UIItem,
        event: MouseEvent,
        context: &dyn WindowOps,
    ) {
        context.set_cursor(Some(MouseCursor::Hand));
        if event.kind == WMEK::Press(MousePress::Left) {
            if let Some(scroll) = self.right_sidebar_file_preview_horizontal_scroll_geometry() {
                let thumb_left = event.coords.x as f32 - scroll.thumb_width / 2.0;
                self.set_right_sidebar_file_preview_horizontal_scroll_from_thumb_left(
                    thumb_left, context,
                );
            }
            context.invalidate();
        }
    }

    pub fn mouse_event_right_sidebar_file_preview_text(
        &mut self,
        item: UIItem,
        event: MouseEvent,
        context: &dyn WindowOps,
    ) {
        context.set_cursor(Some(MouseCursor::Text));
        match event.kind {
            WMEK::Press(MousePress::Left)
                if self
                    .begin_right_sidebar_file_preview_selection(event.coords.x, event.coords.y) =>
            {
                self.dragging.replace((item, event));
                context.invalidate();
            }
            WMEK::Press(MousePress::Right) => {
                self.show_right_sidebar_file_open_with_menu(context, event.coords);
            }
            _ => {}
        }
    }

    pub fn mouse_event_right_sidebar_toggle(&mut self, event: MouseEvent, context: &dyn WindowOps) {
        context.set_cursor(Some(MouseCursor::Hand));
        if event.kind == WMEK::Press(MousePress::Left) {
            self.toggle_right_sidebar();
            self.reflow_right_sidebar(context);
        }
    }

    pub fn mouse_event_right_sidebar_mode(
        &mut self,
        mode: super::RightSidebarMode,
        event: MouseEvent,
        context: &dyn WindowOps,
    ) {
        context.set_cursor(Some(MouseCursor::Hand));
        if event.kind == WMEK::Press(MousePress::Left) {
            let previous_width = self.right_sidebar_width();
            self.right_sidebar_mode = mode;
            // Leaving the file view (e.g. switching to Snippets/Tasks) makes the
            // file index idle; schedule it for release if nothing reopens it.
            if !self.right_sidebar_file_view_active() {
                self.schedule_right_sidebar_file_memory_release();
            }
            self.invalidate_or_reflow_right_sidebar(previous_width, context);
        }
    }

    pub fn mouse_event_right_sidebar_snippet(
        &mut self,
        item: UIItem,
        event: MouseEvent,
        context: &dyn WindowOps,
    ) {
        let item_type = item.item_type.clone();
        let is_input = matches!(
            item_type,
            UIItemType::RightSidebarSnippetSearch
                | UIItemType::RightSidebarSnippetTitle
                | UIItemType::RightSidebarSnippetBody
        );
        context.set_cursor(Some(if is_input {
            MouseCursor::Text
        } else {
            MouseCursor::Hand
        }));
        if event.kind != WMEK::Press(MousePress::Left) {
            return;
        }

        let double_click = self
            .last_mouse_click
            .as_ref()
            .is_some_and(|c| c.streak >= 2);
        match item_type {
            UIItemType::RightSidebarSnippetNew => {
                self.clear_right_sidebar_text_focus();
                self.open_new_snippet_editor();
            }
            UIItemType::RightSidebarSnippetBack => self.close_snippet_editor(),
            UIItemType::RightSidebarSnippetSave => self.save_snippet_editor(),
            UIItemType::RightSidebarSnippetSearch => {
                self.right_sidebar_snippet_focus = Some(super::RightSidebarSnippetField::Search);
                if double_click {
                    self.right_sidebar_snippet_search.caret_select_all();
                } else {
                    self.position_right_sidebar_input_caret(
                        &UIItemType::RightSidebarSnippetSearch,
                        event.coords.x,
                        false,
                    );
                    self.dragging.replace((item, event));
                }
            }
            UIItemType::RightSidebarSnippetTitle => {
                self.right_sidebar_snippet_focus = Some(super::RightSidebarSnippetField::Title);
                if double_click {
                    self.right_sidebar_snippet_title.caret_select_all();
                } else {
                    self.position_right_sidebar_input_caret(
                        &UIItemType::RightSidebarSnippetTitle,
                        event.coords.x,
                        false,
                    );
                    self.dragging.replace((item, event));
                }
            }
            UIItemType::RightSidebarSnippetBody => {
                self.right_sidebar_snippet_focus = Some(super::RightSidebarSnippetField::Body);
                if double_click {
                    self.right_sidebar_snippet_body.caret_select_all();
                }
            }
            UIItemType::RightSidebarSnippetEdit(id) => {
                self.clear_right_sidebar_text_focus();
                self.open_existing_snippet_editor(&id);
            }
            UIItemType::RightSidebarSnippetPaste(id) => {
                self.clear_right_sidebar_text_focus();
                self.paste_snippet_to_active_pane(&id, false);
            }
            UIItemType::RightSidebarSnippetRun(id) => {
                self.clear_right_sidebar_text_focus();
                self.paste_snippet_to_active_pane(&id, true);
            }
            UIItemType::RightSidebarSnippetDelete(id) => self.delete_snippet(&id),
            _ => {}
        }
        context.invalidate();
    }

    pub fn mouse_event_right_sidebar_file(
        &mut self,
        item: UIItem,
        event: MouseEvent,
        context: &dyn WindowOps,
    ) {
        let item_type = item.item_type.clone();
        let is_input = matches!(item_type, UIItemType::RightSidebarFileFilter);
        context.set_cursor(Some(if is_input {
            MouseCursor::Text
        } else {
            MouseCursor::Hand
        }));
        if !matches!(
            event.kind,
            WMEK::Press(MousePress::Left) | WMEK::Press(MousePress::Right)
        ) {
            return;
        }

        let previous_width = self.right_sidebar_width();
        match (item_type, event.kind.clone()) {
            (UIItemType::RightSidebarFileFilter, WMEK::Press(MousePress::Left)) => {
                self.right_sidebar_file_focus = Some(super::RightSidebarFileField::Filter);
                let double_click = self
                    .last_mouse_click
                    .as_ref()
                    .is_some_and(|c| c.streak >= 2);
                if double_click {
                    self.right_sidebar_file_filter.caret_select_all();
                } else {
                    self.position_right_sidebar_input_caret(
                        &UIItemType::RightSidebarFileFilter,
                        event.coords.x,
                        false,
                    );
                    self.dragging.replace((item, event));
                }
            }
            (UIItemType::RightSidebarFileRow(path), WMEK::Press(MousePress::Left)) => {
                self.clear_right_sidebar_text_focus();
                self.open_right_sidebar_file_path(path);
            }
            (UIItemType::RightSidebarFileRow(path), WMEK::Press(MousePress::Right)) => {
                self.clear_right_sidebar_text_focus();
                self.right_sidebar_file_selected = Some(path);
                self.show_right_sidebar_file_open_with_menu(context, event.coords);
            }
            (UIItemType::RightSidebarFileBack, WMEK::Press(MousePress::Left)) => {
                self.clear_right_sidebar_text_focus();
                self.close_right_sidebar_file_preview();
            }
            (UIItemType::RightSidebarFileOpen, WMEK::Press(MousePress::Left)) => {
                self.clear_right_sidebar_text_focus();
                self.open_right_sidebar_selected_file_with_current_app();
            }
            (UIItemType::RightSidebarFileOpenMenu, WMEK::Press(MousePress::Left))
            | (UIItemType::RightSidebarFileOpenMenu, WMEK::Press(MousePress::Right)) => {
                self.clear_right_sidebar_text_focus();
                let coords = window::Point::new(
                    item.x as isize,
                    item.y.saturating_add(item.height) as isize,
                );
                self.show_right_sidebar_file_open_with_menu(context, coords);
            }
            (UIItemType::RightSidebarFileReveal, WMEK::Press(MousePress::Left)) => {
                self.clear_right_sidebar_text_focus();
                self.reveal_right_sidebar_selected_file();
            }
            (UIItemType::RightSidebarFileCopyText, WMEK::Press(MousePress::Left)) => {
                self.clear_right_sidebar_text_focus();
                self.copy_right_sidebar_selected_file_preview_text()
            }
            _ => {}
        }
        self.invalidate_or_reflow_right_sidebar(previous_width, context);
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
            self.show_term_context_menu(
                context,
                coords,
                self.workspace_sidebar_view_options_menu_items(),
            );
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

    pub fn mouse_event_workspace_thread_new(
        &mut self,
        project_id: String,
        event: MouseEvent,
        context: &dyn WindowOps,
    ) {
        if let WMEK::Press(MousePress::Left) = event.kind {
            self.create_workspace_thread(&project_id, context);
        }
        context.set_cursor(Some(MouseCursor::Arrow));
    }

    pub fn mouse_event_project_new(&mut self, event: MouseEvent, context: &dyn WindowOps) {
        if let WMEK::Press(MousePress::Left) = event.kind {
            self.prompt_create_project(context);
        }
        context.set_cursor(Some(MouseCursor::Arrow));
    }

    pub fn mouse_event_space_menu(
        &mut self,
        item: UIItem,
        event: MouseEvent,
        context: &dyn WindowOps,
    ) {
        context.set_cursor(Some(MouseCursor::Hand));
        match event.kind {
            WMEK::Press(MousePress::Left) => {
                let coords = window::Point::new(
                    item.x as isize,
                    item.y.saturating_add(item.height) as isize,
                );
                self.show_term_context_menu(context, coords, self.space_menu_items());
            }
            WMEK::Press(MousePress::Right) => {
                self.show_term_context_menu(context, event.coords, self.space_menu_items());
            }
            _ => {}
        }
    }

    pub fn mouse_event_project(
        &mut self,
        project_id: String,
        event: MouseEvent,
        context: &dyn WindowOps,
    ) {
        match event.kind {
            WMEK::Press(MousePress::Left) => {
                crate::workspace_threads::toggle_project_threads_collapsed(&project_id);
                context.invalidate();
            }
            WMEK::Press(MousePress::Right) => {
                self.show_term_context_menu(
                    context,
                    event.coords,
                    self.project_context_menu_items(&project_id),
                );
            }
            _ => {}
        }
        context.set_cursor(Some(MouseCursor::Arrow));
    }

    pub fn mouse_event_project_toggle_threads(
        &mut self,
        project_id: String,
        event: MouseEvent,
        context: &dyn WindowOps,
    ) {
        match event.kind {
            WMEK::Press(MousePress::Left) => {
                crate::workspace_threads::toggle_project_threads_collapsed(&project_id);
                context.invalidate();
            }
            WMEK::Press(MousePress::Right) => {
                self.show_term_context_menu(
                    context,
                    event.coords,
                    self.project_context_menu_items(&project_id),
                );
            }
            _ => {}
        }
        context.set_cursor(Some(MouseCursor::Arrow));
    }

    pub fn mouse_event_workspace_thread(
        &mut self,
        thread_id: String,
        event: MouseEvent,
        context: &dyn WindowOps,
    ) {
        match event.kind {
            WMEK::Press(MousePress::Left) => {
                if !self.open_remote_workspace_thread_without_connecting(&thread_id, context) {
                    self.activate_workspace_thread(thread_id, context);
                }
            }
            WMEK::Press(MousePress::Right) => {
                self.show_term_context_menu(
                    context,
                    event.coords,
                    self.workspace_thread_context_menu_items(&thread_id),
                );
            }
            _ => {}
        }
        context.set_cursor(Some(MouseCursor::Arrow));
    }

    pub fn mouse_event_workspace_thread_pin(
        &mut self,
        thread_id: String,
        event: MouseEvent,
        context: &dyn WindowOps,
    ) {
        match event.kind {
            WMEK::Press(MousePress::Left) => {
                crate::workspace_threads::toggle_thread_pinned(&thread_id);
                context.invalidate();
            }
            WMEK::Press(MousePress::Right) => {
                self.show_term_context_menu(
                    context,
                    event.coords,
                    self.workspace_thread_context_menu_items(&thread_id),
                );
            }
            _ => {}
        }
        context.set_cursor(Some(MouseCursor::Arrow));
    }

    pub fn mouse_event_workspace_thread_delete(
        &mut self,
        thread_id: String,
        event: MouseEvent,
        context: &dyn WindowOps,
    ) {
        match event.kind {
            WMEK::Press(MousePress::Left) => {
                self.end_workspace_thread(&thread_id, Some(context));
            }
            WMEK::Press(MousePress::Right) => {
                self.show_term_context_menu(
                    context,
                    event.coords,
                    self.workspace_thread_context_menu_items(&thread_id),
                );
            }
            _ => {}
        }
        context.set_cursor(Some(MouseCursor::Arrow));
    }

    pub(crate) fn end_workspace_thread(
        &mut self,
        thread_id: &str,
        context: Option<&dyn WindowOps>,
    ) {
        match crate::workspace_threads::end_workspace_thread_record(thread_id) {
            crate::workspace_threads::EndWorkspaceThreadResult::DeletedThread(deleted) => {
                self.finish_deleted_workspace_thread(deleted, context);
            }
            crate::workspace_threads::EndWorkspaceThreadResult::RemovedProject(removed) => {
                self.finish_removed_project(removed, context);
            }
            crate::workspace_threads::EndWorkspaceThreadResult::Noop => {
                if let Some(context) = context {
                    context.invalidate();
                }
            }
        }
    }

    pub(crate) fn disconnect_workspace_thread(
        &mut self,
        thread_id: &str,
        context: Option<&dyn WindowOps>,
    ) {
        let live_workspaces = Mux::get().iter_workspaces();
        let Some(disconnected) = crate::workspace_threads::disconnect_workspace_thread_record(
            thread_id,
            &live_workspaces,
        ) else {
            if let Some(context) = context {
                context.invalidate();
            }
            return;
        };

        let cleanup_workspaces = vec![disconnected.workspace_name];
        if disconnected.was_active {
            if let (Some(next_thread_id), Some(context)) = (disconnected.next_thread_id, context) {
                self.activate_workspace_thread_with_cleanup(
                    next_thread_id,
                    context,
                    cleanup_workspaces,
                );
                return;
            }
            if context.is_none() {
                return;
            }
        }

        kill_workspace_windows(&cleanup_workspaces, None);

        if let Some(context) = context {
            context.invalidate();
        }
    }

    fn finish_deleted_workspace_thread(
        &mut self,
        deleted: crate::workspace_threads::DeletedWorkspaceThread,
        context: Option<&dyn WindowOps>,
    ) {
        let cleanup_workspaces = deleted
            .materialized_workspace_name
            .into_iter()
            .collect::<Vec<_>>();
        if deleted.was_active {
            if let (Some(next_thread_id), Some(context)) = (deleted.next_thread_id, context) {
                self.activate_workspace_thread_with_cleanup(
                    next_thread_id,
                    context,
                    cleanup_workspaces,
                );
                return;
            }
            if context.is_none() {
                return;
            }
        }

        kill_workspace_windows(&cleanup_workspaces, None);

        if let Some(context) = context {
            context.invalidate();
        }
    }

    fn finish_removed_project(
        &mut self,
        removed: crate::workspace_threads::RemovedProject,
        context: Option<&dyn WindowOps>,
    ) {
        let cleanup_workspaces = removed.materialized_workspace_names;
        if removed.was_active {
            if let (Some(next_thread_id), Some(context)) = (removed.next_thread_id, context) {
                self.activate_workspace_thread_with_cleanup(
                    next_thread_id,
                    context,
                    cleanup_workspaces,
                );
                return;
            }
            if context.is_none() {
                return;
            }
        }

        kill_workspace_windows(&cleanup_workspaces, None);

        if let Some(context) = context {
            context.invalidate();
        }
    }

    fn project_context_menu_items(&self, project_id: &str) -> Vec<ContextMenuItem> {
        let project_id = project_id.to_string();
        let mut reveal_item = ContextMenuItem::item_with_icon(
            "Reveal in Folder",
            "folder",
            KeyAssignment::RevealProjectInFolder(project_id.clone()),
        );
        if crate::workspace_threads::project_reveal_path(&project_id).is_none() {
            reveal_item = reveal_item.disabled();
        }

        vec![
            ContextMenuItem::item_with_icon(
                "Rename Workspace...",
                "pencil",
                KeyAssignment::PromptRenameProject(project_id.clone()),
            ),
            reveal_item,
            ContextMenuItem::item_with_icon(
                "New Thread",
                "plus.square",
                KeyAssignment::CreateWorkspaceThread(project_id.clone()),
            ),
            ContextMenuItem::item_with_icon(
                "Collapse / Expand Threads",
                "chevron.right",
                KeyAssignment::ToggleWorkspaceThreadsCollapsed(project_id.clone()),
            ),
            ContextMenuItem::Separator,
            ContextMenuItem::item_with_icon(
                "Remove Workspace",
                "folder.badge.minus",
                KeyAssignment::RemoveProject(project_id),
            ),
        ]
    }

    fn space_menu_items(&self) -> Vec<ContextMenuItem> {
        let spaces = crate::workspace_threads::spaces_for_window(self.space_owner_id);
        let mut items = vec![];
        for space in &spaces {
            let mut item = ContextMenuItem::item_with_icon(
                if space.is_occupied_by_other_window {
                    format!("{} (occupied)", space.name)
                } else {
                    space.name.clone()
                },
                if space.is_default {
                    "house"
                } else {
                    "square.stack"
                },
                KeyAssignment::SwitchSpace(space.id.to_string()),
            )
            .checked(space.is_active);
            if space.is_occupied_by_other_window {
                item = item.disabled();
            }
            items.push(item);
        }

        items.push(ContextMenuItem::Separator);
        items.push(ContextMenuItem::item_with_icon(
            "New Space",
            "plus.square",
            KeyAssignment::CreateSpace,
        ));
        items.push(ContextMenuItem::item_with_icon(
            spaces
                .iter()
                .find(|space| space.is_active)
                .map(|space| format!("Rename \"{}\"...", space.name))
                .unwrap_or_else(|| "Rename Space...".to_string()),
            "pencil",
            KeyAssignment::PromptRenameSpace(self.active_space_id.clone()),
        ));
        let active_space_id = spaces
            .iter()
            .find(|space| space.is_active)
            .map(|space| space.id.clone());
        let delete_item = |space: crate::workspace_threads::SpaceView| {
            ContextMenuItem::item_with_icon(
                format!("Delete \"{}\"", space.name),
                "trash",
                KeyAssignment::DeleteSpace(space.id),
            )
        };
        let delete_candidates = spaces
            .iter()
            .filter(|space| !space.is_default && !space.is_occupied_by_other_window)
            .cloned()
            .collect::<Vec<_>>();
        let active_delete = delete_candidates
            .iter()
            .find(|space| active_space_id.as_deref() == Some(space.id.as_str()))
            .cloned();
        if let Some(space) = active_delete {
            items.push(delete_item(space));
        }

        let other_delete_candidates = delete_candidates
            .into_iter()
            .filter(|space| active_space_id.as_deref() != Some(space.id.as_str()))
            .map(delete_item)
            .collect::<Vec<_>>();
        if !other_delete_candidates.is_empty() {
            items.push(ContextMenuItem::submenu(
                "Delete Other Space",
                other_delete_candidates,
            ));
        }
        items
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
            ContextMenuItem::item_with_icon("Pinned", "pin", KeyAssignment::Nop)
                .checked(true)
                .disabled(),
            ContextMenuItem::Separator,
            ContextMenuItem::item("Collapse All", KeyAssignment::Nop).disabled(),
            ContextMenuItem::item("Mark All Read", KeyAssignment::Nop).disabled(),
        ]
    }

    fn open_remote_workspace_thread_without_connecting(
        &mut self,
        thread_id: &str,
        context: &dyn WindowOps,
    ) -> bool {
        self.open_remote_workspace_thread_view(thread_id, context, false)
    }

    fn open_remote_workspace_thread_view(
        &mut self,
        thread_id: &str,
        context: &dyn WindowOps,
        force_disconnected: bool,
    ) -> bool {
        let live_workspaces = Mux::get().iter_workspaces();
        let Some(mut state) =
            crate::workspace_threads::thread_connection_state(thread_id, &live_workspaces)
        else {
            return false;
        };

        if !state.is_remote || (!force_disconnected && state.is_live) {
            if !state.is_remote {
                return false;
            }

            let key = format!(
                "{}{}",
                crate::termwindow::remote_thread_view::REMOTE_THREAD_CONTENT_VIEW_KEY_PREFIX,
                thread_id
            );
            if let Some(view_id) = self.content_view_id_for_key(&key) {
                self.snapshot_active_workspace_thread_layout();
                self.workspace_sidebar_pending_thread_selection = Some(thread_id.to_string());
                self.activate_content_view(view_id);
                context.invalidate();
                return true;
            }

            return false;
        }
        if force_disconnected {
            state.is_live = false;
        }

        self.snapshot_active_workspace_thread_layout();
        self.workspace_sidebar_pending_thread_selection = Some(thread_id.to_string());
        self.open_content_view(Box::new(
            crate::termwindow::remote_thread_view::RemoteThreadView::new(state),
        ));
        context.invalidate();
        true
    }

    fn workspace_thread_context_menu_items(&self, thread_id: &str) -> Vec<ContextMenuItem> {
        let thread_id = thread_id.to_string();
        let is_pinned = crate::workspace_threads::thread_is_pinned(&thread_id);
        let live_workspaces = Mux::get().iter_workspaces();
        let connection =
            crate::workspace_threads::thread_connection_state(&thread_id, &live_workspaces);
        let mut items = vec![];

        if let Some(connection) = connection.as_ref() {
            let remote_host_exists = connection.is_remote
                && crate::ssh_hosts::host_spec(
                    crate::workspace_threads::remote_host_id_for_project_id(&connection.project_id),
                )
                .is_some();
            if connection.is_remote && connection.is_live {
                items.push(ContextMenuItem::item_with_icon(
                    "Disconnect Thread",
                    "unlink",
                    KeyAssignment::DisconnectWorkspaceThread(thread_id.clone()),
                ));
                items.push(ContextMenuItem::Separator);
            } else if connection.is_remote && remote_host_exists {
                items.push(ContextMenuItem::item_with_icon(
                    "Connect Thread",
                    "link",
                    KeyAssignment::ConnectWorkspaceThread(thread_id.clone()),
                ));
                items.push(ContextMenuItem::Separator);
            }
        }

        items.extend([
            ContextMenuItem::item_with_icon(
                if is_pinned {
                    "Unpin Thread"
                } else {
                    "Pin Thread"
                },
                if is_pinned { "pin.slash" } else { "pin" },
                KeyAssignment::ToggleWorkspaceThreadPinned(thread_id.clone()),
            ),
            ContextMenuItem::item_with_icon(
                "Rename Thread...",
                "pencil",
                KeyAssignment::PromptRenameWorkspaceThread(thread_id.clone()),
            ),
            ContextMenuItem::item_with_icon(
                "Delete Thread",
                "trash",
                KeyAssignment::DeleteWorkspaceThread(thread_id.clone()),
            ),
            ContextMenuItem::item_with_icon(
                "Mark as Unread",
                "envelope.badge",
                KeyAssignment::MarkWorkspaceThreadUnread(thread_id),
            ),
        ]);
        items
    }

    pub(crate) fn activate_workspace_thread(&mut self, thread_id: String, context: &dyn WindowOps) {
        self.activate_workspace_thread_impl(thread_id, context, None, Vec::new());
    }

    fn activate_workspace_thread_with_cleanup(
        &mut self,
        thread_id: String,
        context: &dyn WindowOps,
        workspaces_to_kill_after_adopt: Vec<String>,
    ) {
        self.activate_workspace_thread_impl(
            thread_id,
            context,
            None,
            workspaces_to_kill_after_adopt,
        );
    }

    pub(crate) fn create_workspace_thread(&mut self, project_id: &str, context: &dyn WindowOps) {
        let thread_id = crate::workspace_threads::create_thread(project_id, None);
        if !self.open_remote_workspace_thread_without_connecting(&thread_id, context) {
            self.activate_workspace_thread(thread_id, context);
        }
    }

    pub(crate) fn open_ssh_host_thread_without_connecting(
        &mut self,
        host_id: String,
        context: &dyn WindowOps,
    ) -> bool {
        let Some(spec) = crate::ssh_hosts::host_spec(&host_id) else {
            context.invalidate();
            return false;
        };

        self.snapshot_active_workspace_thread_layout();
        let use_mosh = spec.use_mosh;
        let thread_id = crate::workspace_threads::create_disconnected_remote_host_thread(
            &self.active_space_id,
            &host_id,
            &spec.label,
            crate::ssh_hosts::host_project_path(&spec),
            spec.default_workspace.clone(),
        );
        if !self.open_remote_workspace_thread_without_connecting(&thread_id, context) {
            context.invalidate();
            return false;
        }
        if use_mosh {
            let _ = self.begin_open_remote_thread_connection(thread_id, context, None);
        }
        true
    }

    fn mosh_spec_for_project_id(project_id: &str) -> Option<crate::ssh_hosts::SshHostSpec> {
        let host_id = crate::workspace_threads::remote_host_id_for_project_id(project_id);
        crate::ssh_hosts::host_spec(host_id).filter(|spec| spec.use_mosh)
    }

    /// Bootstrap `mosh-server` over ThinkTerm's SSH stack, then launch the local
    /// `mosh-client`. If the integrated bootstrap can't proceed (missing stored
    /// password, host verification prompt, command failure, etc.), fall back to
    /// the external `mosh` wrapper so the user can still interact manually.
    fn begin_mosh_thread_connection_impl(
        &mut self,
        thread_id: String,
        view_id: Option<ContentViewId>,
        context: &dyn WindowOps,
        orphan_candidate_window_id: Option<MuxWindowId>,
        workspaces_to_kill_after_adopt: Vec<String>,
    ) -> bool {
        let mux = Mux::get();
        let live_workspaces = mux.iter_workspaces();
        let Some(plan) =
            crate::workspace_threads::activation_plan_for_thread(&thread_id, &live_workspaces)
        else {
            if let Some(view_id) = view_id {
                self.fail_remote_connect(view_id, "This thread could not be activated.");
            } else {
                context.invalidate();
            }
            return true;
        };

        if !plan.needs_materialize {
            if let Some(view_id) = view_id {
                self.remote_connects.remove(&view_id);
                self.close_content_view_by_id(view_id);
            }
            self.activate_workspace_thread_impl(
                thread_id,
                context,
                orphan_candidate_window_id,
                workspaces_to_kill_after_adopt,
            );
            return true;
        }

        let remote_host_id =
            crate::workspace_threads::remote_host_id_for_project_id(&plan.project_id).to_string();
        let Some(spec) = crate::ssh_hosts::host_spec(&remote_host_id).filter(|spec| spec.use_mosh)
        else {
            return false;
        };

        self.snapshot_active_workspace_thread_layout();
        if view_id.is_none() {
            self.workspace_sidebar_pending_thread_selection = None;
            self.set_content_view_active(false);
        }
        if let Some(view_id) = view_id {
            if let Some(view) = self.content_view_mut_by_id(view_id) {
                view.on_remote_connect_phase(
                    crate::termwindow::content_view::RemoteConnectPhase::Connecting,
                );
            }
        }

        let workspace_name = plan.workspace_name.clone();
        let (mosh_generation, cancel_token) = if let Some(view_id) = view_id {
            let generation = self.next_mosh_connect_generation;
            self.next_mosh_connect_generation =
                self.next_mosh_connect_generation.saturating_add(1).max(1);
            let token = Arc::new(AtomicBool::new(false));
            if let Some(previous) = self.mosh_connects.insert(
                view_id,
                super::MoshConnectState {
                    generation,
                    workspace_name: workspace_name.clone(),
                    canceled: Arc::clone(&token),
                },
            ) {
                previous.canceled.store(true, Ordering::SeqCst);
                self.kill_remote_connect_workspace(&previous.workspace_name);
            }
            (Some(generation), Some(token))
        } else {
            (None, None)
        };
        let size = self.config.initial_size(
            self.dimensions.dpi as u32,
            crate::cell_pixel_dims(&self.config, self.dimensions.dpi as f64).ok(),
        );
        let term_config: Arc<dyn wezterm_term::TerminalConfiguration> =
            Arc::new(TermConfig::with_config(self.config.clone()));
        let window = self.window.clone();
        let thread_id_for_task = thread_id.clone();
        let label = spec.label.clone();
        let fallback_spawn = crate::ssh_hosts::build_mosh_fallback_spawn(&spec);
        let bootstrap_spec = spec.clone();
        let cancel_token_for_task = cancel_token.clone();

        promise::spawn::spawn(async move {
            let spawn_result = promise::spawn::spawn_into_new_thread(move || {
                promise::spawn::block_on(crate::ssh_hosts::build_integrated_mosh_spawn(
                    &bootstrap_spec,
                ))
            })
            .await;
            let spawn = match spawn_result {
                Ok(spawn) => spawn,
                Err(err) => {
                    log::warn!(
                        "integrated mosh bootstrap failed for {:?}: {err:#}; falling back to external mosh",
                        label
                    );
                    fallback_spawn
                }
            };

            if cancel_token_for_task
                .as_ref()
                .is_some_and(|token| token.load(Ordering::SeqCst))
            {
                return;
            }

            front_end().set_switching_workspaces(true);
            let materialized = match crate::workspace_threads::materialize_thread_spawn(
                workspace_name.clone(),
                spawn,
                size,
                term_config,
            )
            .await
            {
                Ok(()) => true,
                Err(err) => {
                    log::error!("failed to materialize Mosh thread: {err:#}");
                    false
                }
            };

            if let Some(window) = window {
                window.notify(TermWindowNotif::Apply(Box::new(move |tw| {
                    tw.finish_mosh_thread_connection(
                        thread_id_for_task,
                        workspace_name,
                        view_id,
                        mosh_generation,
                        cancel_token,
                        materialized,
                        orphan_candidate_window_id,
                        workspaces_to_kill_after_adopt,
                    );
                })));
            } else {
                if cancel_token_for_task
                    .as_ref()
                    .is_some_and(|token| token.load(Ordering::SeqCst))
                    && materialized
                {
                    kill_workspace_windows(&[workspace_name], None);
                }
                front_end().set_switching_workspaces(false);
                reconcile_workspace_layout_after_materialize(None);
            }
        })
        .detach();

        context.invalidate();
        true
    }

    fn finish_mosh_thread_connection(
        &mut self,
        thread_id: String,
        workspace_name: String,
        view_id: Option<ContentViewId>,
        mosh_generation: Option<u64>,
        cancel_token: Option<Arc<AtomicBool>>,
        materialized: bool,
        orphan_candidate_window_id: Option<MuxWindowId>,
        workspaces_to_kill_after_adopt: Vec<String>,
    ) {
        let token_cancelled = cancel_token
            .as_ref()
            .is_some_and(|token| token.load(Ordering::SeqCst));
        let mut cancelled = token_cancelled;
        let mut reveal_now = view_id.is_none();
        let mut close_view_after_success = None;

        if let Some(view_id) = view_id {
            let state_matches = self.mosh_connects.get(&view_id).is_some_and(|state| {
                Some(state.generation) == mosh_generation
                    && cancel_token
                        .as_ref()
                        .is_some_and(|token| Arc::ptr_eq(&state.canceled, token))
            });

            if !state_matches {
                cancelled = true;
            }

            if cancelled {
                if let Some(state) = self.mosh_connects.remove(&view_id) {
                    state.canceled.store(true, Ordering::SeqCst);
                }
            } else if materialized {
                self.mosh_connects.remove(&view_id);
                reveal_now = self.active_content_view_id == Some(view_id);
                self.workspace_sidebar_pending_thread_selection = None;
                close_view_after_success = Some(view_id);
            } else {
                self.mosh_connects.remove(&view_id);
                self.fail_remote_connect(view_id, "Failed to start Mosh.");
            }
        }

        if materialized {
            if cancelled {
                self.kill_remote_connect_workspace(&workspace_name);
            } else if reveal_now {
                let live_workspaces = Mux::get().iter_workspaces();
                let _ =
                    crate::workspace_threads::activate_thread_record(&thread_id, &live_workspaces);
                self.adopt_workspace_in_this_window(&workspace_name);
                cleanup_orphaned_mux_window(orphan_candidate_window_id);
                kill_workspace_windows(&workspaces_to_kill_after_adopt, Some(&workspace_name));
            }
        }

        if let Some(view_id) = close_view_after_success {
            self.close_content_view_by_id(view_id);
        }

        front_end().set_switching_workspaces(false);
        reconcile_workspace_layout_after_materialize(self.window.clone());
        self.invalidate_window();
    }

    pub(crate) fn activate_workspace_thread_for_new_window(
        &mut self,
        thread_id: String,
        context: &dyn WindowOps,
        startup_window_id: MuxWindowId,
    ) {
        if self.open_remote_workspace_thread_without_connecting(&thread_id, context) {
            // Do not auto-connect remote threads while restoring a new window.
            // Remote hosts may be unavailable or expensive to wake; let the
            // disconnected view make the connection an explicit user action.
            return;
        }

        self.activate_workspace_thread_impl(
            thread_id,
            context,
            Some(startup_window_id),
            Vec::new(),
        );
    }

    fn activate_workspace_thread_impl(
        &mut self,
        thread_id: String,
        context: &dyn WindowOps,
        orphan_candidate_window_id: Option<MuxWindowId>,
        workspaces_to_kill_after_adopt: Vec<String>,
    ) {
        if crate::workspace_threads::thread_space_id(&thread_id).as_deref()
            != Some(self.active_space_id.as_str())
        {
            context.invalidate();
            return;
        }

        let mux = Mux::get();
        let live_workspaces = mux.iter_workspaces();
        if let Some(state) =
            crate::workspace_threads::thread_connection_state(&thread_id, &live_workspaces)
        {
            if state.is_remote && !state.is_live {
                if Self::mosh_spec_for_project_id(&state.project_id).is_some() {
                    let _ = self.begin_mosh_thread_connection_impl(
                        thread_id,
                        None,
                        context,
                        orphan_candidate_window_id,
                        workspaces_to_kill_after_adopt,
                    );
                    return;
                }
            }
        }

        self.snapshot_active_workspace_thread_layout();
        self.workspace_sidebar_pending_thread_selection = None;
        self.set_content_view_active(false);

        let Some(plan) =
            crate::workspace_threads::activate_thread_record(&thread_id, &live_workspaces)
        else {
            context.invalidate();
            return;
        };

        if !plan.needs_materialize {
            self.adopt_workspace_in_this_window(&plan.workspace_name);
            // Symmetric with the materialize branch below: if this switch
            // orphaned a startup mux window, tidy it up. cleanup_orphaned_mux_window
            // is a no-op when the window is still shown or has a thread binding.
            cleanup_orphaned_mux_window(orphan_candidate_window_id);
            kill_workspace_windows(&workspaces_to_kill_after_adopt, Some(&plan.workspace_name));
            context.invalidate();
            return;
        }

        let workspace_name = plan.workspace_name.clone();
        let remote_host_id =
            crate::workspace_threads::remote_host_id_for_project_id(&plan.project_id).to_string();
        let remote_spec = crate::ssh_hosts::host_spec(&remote_host_id);
        if remote_spec.is_none() && crate::workspace_threads::project_is_remote(&plan.project_id) {
            log::warn!(
                "refusing to connect remote thread {:?}: SSH host {:?} no longer exists",
                plan.thread_id,
                remote_host_id
            );
            context.invalidate();
            return;
        }
        let mut detect_remote_os = None;
        let (initial_cwd, default_domain) = if let Some(spec) = remote_spec.as_ref() {
            match crate::ssh_hosts::ensure_ssh_domain_registered(spec) {
                Ok(domain_name) => {
                    if spec.detect_os {
                        detect_remote_os = Some((domain_name.clone(), remote_host_id));
                    }
                    (
                        None,
                        config::keyassignment::SpawnTabDomain::DomainName(domain_name),
                    )
                }
                Err(err) => {
                    log::error!(
                        "failed to register SSH domain for session {:?}: {err:#}",
                        spec.label
                    );
                    context.invalidate();
                    return;
                }
            }
        } else {
            (
                plan.project_path.to_str().map(|path| path.to_string()),
                config::keyassignment::SpawnTabDomain::DefaultDomain,
            )
        };
        let layout = crate::workspace_threads::thread_layout(&plan.thread_id);
        let dpi = self.dimensions.dpi as u32;
        let size = self.config.initial_size(
            dpi,
            crate::cell_pixel_dims(&self.config, self.dimensions.dpi as f64).ok(),
        );
        let term_config: Arc<dyn wezterm_term::TerminalConfiguration> =
            Arc::new(TermConfig::with_config(self.config.clone()));
        // Suppress the additive reconcile while we materialize the target
        // workspace's mux window; we adopt it into THIS window afterwards so
        // no duplicate window is spawned and no other window is disturbed.
        front_end().set_switching_workspaces(true);
        mux.set_active_workspace(&workspace_name);

        let reconcile_window = self.window.clone();
        let adopt_workspace = workspace_name.clone();
        let detect_window = self.window.clone();
        let cleanup_workspaces = workspaces_to_kill_after_adopt;
        promise::spawn::spawn(async move {
            let materialized = match crate::workspace_threads::materialize_thread(
                workspace_name,
                layout,
                initial_cwd,
                size,
                None,
                term_config,
                default_domain,
            )
            .await
            {
                Ok(()) => true,
                Err(err) => {
                    log::error!("failed to materialize ThinkTerm thread: {err:#}");
                    false
                }
            };
            adopt_workspace_into_window(&reconcile_window, &adopt_workspace);
            front_end().set_switching_workspaces(false);
            if materialized {
                cleanup_orphaned_mux_window(orphan_candidate_window_id);
                kill_workspace_windows(&cleanup_workspaces, Some(&adopt_workspace));
            }
            reconcile_workspace_layout_after_materialize(reconcile_window);

            if materialized {
                if let Some((detect_domain, detect_project)) = detect_remote_os {
                    if let Some(domain) = Mux::get().get_domain_by_name(&detect_domain) {
                        if let Some(ssh) = domain.as_ref().downcast_ref::<RemoteSshDomain>() {
                            if let Some(distro) = ssh.detect_os_release().await {
                                if crate::ssh_hosts::set_host_distro(&detect_project, &distro) {
                                    if let Some(win) = detect_window.as_ref() {
                                        win.invalidate();
                                    }
                                }
                            }
                        }
                    }
                }
            }
        })
        .detach();

        context.invalidate();
    }

    fn failed_remote_thread_for_pane(
        &self,
        pane_id: mux::pane::PaneId,
    ) -> Option<(String, String)> {
        let mux = Mux::get();
        let pane = mux.get_pane(pane_id)?;
        let domain = mux.get_domain(pane.domain_id())?;
        let ssh = domain.as_ref().downcast_ref::<RemoteSshDomain>()?;
        let SshConnectionStatus::Failed(message) = ssh.connection_status() else {
            return None;
        };

        let workspace = self.current_mux_workspace()?;
        let thread_id =
            crate::workspace_threads::thread_id_for_workspace(&self.active_space_id, &workspace)?;
        let live_workspaces = mux.iter_workspaces();
        let state =
            crate::workspace_threads::thread_connection_state(&thread_id, &live_workspaces)?;
        state.is_remote.then_some((thread_id, message))
    }

    pub(crate) fn redirect_failed_remote_tab_spawn_to_thread_view(
        &mut self,
        pane_id: mux::pane::PaneId,
    ) -> bool {
        let Some((thread_id, message)) = self.failed_remote_thread_for_pane(pane_id) else {
            return false;
        };
        let Some(window) = self.window.clone() else {
            return false;
        };

        if self.open_remote_workspace_thread_view(&thread_id, &window, true) {
            let key = format!(
                "{}{}",
                crate::termwindow::remote_thread_view::REMOTE_THREAD_CONTENT_VIEW_KEY_PREFIX,
                thread_id
            );
            if let Some(view_id) = self.content_view_id_for_key(&key) {
                if let Some(view) = self.content_view_mut_by_id(view_id) {
                    view.on_remote_connect_phase(
                        crate::termwindow::content_view::RemoteConnectPhase::Failed { message },
                    );
                }
            }
        } else {
            window.invalidate();
        }
        true
    }

    /// Single entry point for "connect this thread" (context menu, etc). For a
    /// disconnected remote thread it routes through the `RemoteThreadView`
    /// "Connecting…" UI; otherwise it activates the thread directly.
    pub(crate) fn connect_remote_thread(&mut self, thread_id: String, context: &dyn WindowOps) {
        let live_workspaces = Mux::get().iter_workspaces();
        let is_disconnected_remote =
            crate::workspace_threads::thread_connection_state(&thread_id, &live_workspaces)
                .map(|state| state.is_remote && !state.is_live)
                .unwrap_or(false);

        if is_disconnected_remote
            && self.open_remote_workspace_thread_without_connecting(&thread_id, context)
        {
            if self.begin_open_remote_thread_connection(thread_id.clone(), context, None) {
                return;
            }
        }

        self.activate_workspace_thread(thread_id, context);
    }

    fn begin_open_remote_thread_connection(
        &mut self,
        thread_id: String,
        context: &dyn WindowOps,
        orphan_candidate_window_id: Option<MuxWindowId>,
    ) -> bool {
        let key = format!(
            "{}{}",
            crate::termwindow::remote_thread_view::REMOTE_THREAD_CONTENT_VIEW_KEY_PREFIX,
            thread_id
        );
        let Some(view_id) = self.content_view_id_for_key(&key) else {
            return false;
        };

        // Show the spinner immediately, then start the connection.
        if let Some(view) = self.content_view_mut_by_id(view_id) {
            view.on_remote_connect_phase(
                crate::termwindow::content_view::RemoteConnectPhase::Connecting,
            );
        }
        self.begin_remote_thread_connection_impl(
            thread_id,
            view_id,
            context,
            orphan_candidate_window_id,
        );
        true
    }

    /// Start connecting a remote thread while keeping its `RemoteThreadView`
    /// foreground as the "Connecting…" UI. The SSH workspace is materialized in
    /// the background (we do not switch this window's active workspace, so the
    /// additive reconcile leaves the new mux window alone); a poll loop adopts
    /// it once authenticated, or kills it on failure.
    pub(crate) fn begin_remote_thread_connection(
        &mut self,
        thread_id: String,
        view_id: ContentViewId,
        context: &dyn WindowOps,
    ) {
        self.begin_remote_thread_connection_impl(thread_id, view_id, context, None);
    }

    fn begin_remote_thread_connection_impl(
        &mut self,
        thread_id: String,
        view_id: ContentViewId,
        context: &dyn WindowOps,
        orphan_candidate_window_id: Option<MuxWindowId>,
    ) {
        let mux = Mux::get();
        let live_workspaces = mux.iter_workspaces();
        let Some(plan) =
            crate::workspace_threads::activation_plan_for_thread(&thread_id, &live_workspaces)
        else {
            self.fail_remote_connect(view_id, "This thread could not be activated.");
            return;
        };

        // Already live (or a local thread that needs no session): just reveal.
        if !plan.needs_materialize {
            self.remote_connects.remove(&view_id);
            let window = self.window.as_ref().cloned();
            self.close_content_view_by_id(view_id);
            if let Some(window) = window {
                self.activate_workspace_thread_impl(
                    thread_id,
                    &window,
                    orphan_candidate_window_id,
                    Vec::new(),
                );
            }
            return;
        }

        let remote_host_id =
            crate::workspace_threads::remote_host_id_for_project_id(&plan.project_id).to_string();
        let Some(spec) = crate::ssh_hosts::host_spec(&remote_host_id) else {
            self.fail_remote_connect(view_id, "The saved SSH host no longer exists.");
            return;
        };
        if spec.use_mosh {
            let _ = self.begin_mosh_thread_connection_impl(
                thread_id,
                Some(view_id),
                context,
                orphan_candidate_window_id,
                Vec::new(),
            );
            return;
        }
        let domain_name = match crate::ssh_hosts::ensure_ssh_domain_registered(&spec) {
            Ok(name) => name,
            Err(err) => {
                self.fail_remote_connect(view_id, &format!("{err:#}"));
                return;
            }
        };

        self.snapshot_active_workspace_thread_layout();

        let workspace_name = plan.workspace_name.clone();
        let layout = crate::workspace_threads::thread_layout(&plan.thread_id);
        let size = self.config.initial_size(
            self.dimensions.dpi as u32,
            crate::cell_pixel_dims(&self.config, self.dimensions.dpi as f64).ok(),
        );
        let term_config: Arc<dyn wezterm_term::TerminalConfiguration> =
            Arc::new(TermConfig::with_config(self.config.clone()));

        let generation = self.next_remote_connect_generation;
        self.next_remote_connect_generation =
            self.next_remote_connect_generation.saturating_add(1).max(1);
        if let Some(previous) = self.remote_connects.insert(
            view_id,
            super::RemoteConnectState {
                generation,
                thread_id,
                workspace_name: workspace_name.clone(),
                domain_name: domain_name.clone(),
                started: Instant::now(),
                orphan_candidate_window_id,
                detect_os: spec
                    .detect_os
                    .then(|| (domain_name.clone(), remote_host_id)),
            },
        ) {
            self.kill_remote_connect_workspace(&previous.workspace_name);
        }

        let materialize_workspace = workspace_name;
        promise::spawn::spawn(async move {
            if let Err(err) = crate::workspace_threads::materialize_thread(
                materialize_workspace,
                layout,
                None,
                size,
                None,
                term_config,
                config::keyassignment::SpawnTabDomain::DomainName(domain_name),
            )
            .await
            {
                log::error!("failed to materialize connecting SSH thread: {err:#}");
            }
        })
        .detach();

        self.schedule_remote_connect_poll(view_id, generation);
        context.invalidate();
    }

    fn schedule_remote_connect_poll(&self, view_id: ContentViewId, generation: u64) {
        let Some(window) = self.window.clone() else {
            return;
        };
        promise::spawn::spawn(async move {
            smol::Timer::after(Duration::from_millis(200)).await;
            window.notify(TermWindowNotif::Apply(Box::new(move |tw| {
                tw.poll_remote_connect(view_id, generation);
            })));
        })
        .detach();
    }

    fn poll_remote_connect(&mut self, view_id: ContentViewId, generation: u64) {
        let Some(state) = self.remote_connects.get(&view_id) else {
            return;
        };
        if state.generation != generation {
            return; // superseded by a newer attempt or cancelled
        }
        let domain_name = state.domain_name.clone();
        let workspace_name = state.workspace_name.clone();
        let elapsed = state.started.elapsed();

        let status = Mux::get()
            .get_domain_by_name(&domain_name)
            .and_then(|domain| {
                domain
                    .as_ref()
                    .downcast_ref::<RemoteSshDomain>()
                    .map(|ssh| ssh.connection_status())
            });

        match status {
            Some(SshConnectionStatus::Authenticating) | Some(SshConnectionStatus::Connected) => {
                self.reveal_remote_connect(view_id, generation);
            }
            Some(SshConnectionStatus::Failed(message)) => {
                self.kill_remote_connect_workspace(&workspace_name);
                self.fail_remote_connect(view_id, &message);
            }
            _ => {
                if elapsed >= Duration::from_secs(REMOTE_CONNECT_OVERALL_TIMEOUT_SECS) {
                    self.kill_remote_connect_workspace(&workspace_name);
                    self.fail_remote_connect(view_id, "Connection timed out.");
                    return;
                }
                if let Some(view) = self.content_view_mut_by_id(view_id) {
                    view.on_remote_connect_phase(
                        crate::termwindow::content_view::RemoteConnectPhase::Connecting,
                    );
                }
                self.schedule_remote_connect_poll(view_id, generation);
                self.invalidate_window();
            }
        }
    }

    fn reveal_remote_connect(&mut self, view_id: ContentViewId, generation: u64) {
        let Some(state) = self.remote_connects.remove(&view_id) else {
            return;
        };
        if state.generation != generation {
            self.remote_connects.insert(view_id, state);
            return;
        }
        let detect = state.detect_os;
        let orphan_candidate_window_id = state.orphan_candidate_window_id;
        let window = self.window.as_ref().cloned();
        let reveal_now = self.active_content_view_id == Some(view_id);
        self.close_content_view_by_id(view_id);
        if reveal_now {
            if let Some(window) = window.clone() {
                self.activate_workspace_thread_impl(
                    state.thread_id,
                    &window,
                    orphan_candidate_window_id,
                    Vec::new(),
                );
            }
        }

        if let Some((detect_domain, detect_project)) = detect {
            let detect_window = window;
            promise::spawn::spawn(async move {
                if let Some(domain) = Mux::get().get_domain_by_name(&detect_domain) {
                    if let Some(ssh) = domain.as_ref().downcast_ref::<RemoteSshDomain>() {
                        if let Some(distro) = ssh.detect_os_release().await {
                            if crate::ssh_hosts::set_host_distro(&detect_project, &distro) {
                                if let Some(win) = detect_window.as_ref() {
                                    win.invalidate();
                                }
                            }
                        }
                    }
                }
            })
            .detach();
        }
    }

    fn fail_remote_connect(&mut self, view_id: ContentViewId, message: &str) {
        self.remote_connects.remove(&view_id);
        if let Some(view) = self.content_view_mut_by_id(view_id) {
            view.on_remote_connect_phase(
                crate::termwindow::content_view::RemoteConnectPhase::Failed {
                    message: message.to_string(),
                },
            );
        }
        self.invalidate_window();
    }

    /// Cancel an in-flight connection started from `view_id` and tear down any
    /// background workspace it already materialized. The view has already reset
    /// itself to Idle.
    pub(crate) fn cancel_remote_thread_connection(&mut self, view_id: ContentViewId) {
        if let Some(state) = self.remote_connects.remove(&view_id) {
            self.kill_remote_connect_workspace(&state.workspace_name);
        }
        if let Some(state) = self.mosh_connects.remove(&view_id) {
            state.canceled.store(true, Ordering::SeqCst);
            self.kill_remote_connect_workspace(&state.workspace_name);
        }
        self.invalidate_window();
    }

    fn kill_remote_connect_workspace(&self, workspace_name: &str) {
        let mux = Mux::get();
        for window_id in mux.iter_windows_in_workspace(workspace_name) {
            mux.kill_window(window_id);
        }
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
            WMEK::VertWheel(amount) => {
                self.lock_pane_nav_tab_wheel_surface(pane_id);
                self.scroll_pane_nav_tabs(pane_id, amount, context);
                return;
            }
            _ => {}
        }

        if event.kind == WMEK::Press(MousePress::Right) {
            if let PaneNavAction::Activate(target_pane_id) = action {
                self.show_term_context_menu(
                    context,
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
                    if tab
                        .pane_stack_id(pane_id)
                        .is_some_and(|stack_id| self.collapsed_pane_layouts.contains_key(&stack_id))
                    {
                        context.invalidate();
                        return;
                    }
                    tab.set_active_idx(pane_index);
                }
                if self.last_mouse_click.as_ref().map(|c| c.streak) == Some(2) {
                    self.spawn_pane_nav_tab(pane_id, pane_index);
                }
            }
            PaneNavAction::Activate(target_pane_id) => {
                if let Some(tab) = mux.get_active_tab_for_window(self.mux_window_id) {
                    if let Some(stack_id) = tab.pane_stack_id(target_pane_id) {
                        if let Some(layout) = self.collapsed_pane_layouts.remove(&stack_id) {
                            if !tab.restore_collapsed_pane(layout) {
                                self.collapsed_pane_layouts.insert(stack_id, layout);
                                context.invalidate();
                                return;
                            }
                        }
                    }
                }
                if let Err(err) = mux.activate_pane_in_stack(target_pane_id) {
                    log::error!("pane nav activate failed: {err:#}");
                }
            }
            PaneNavAction::Close(target_pane_id) => {
                if let Some(tab) = mux.get_active_tab_for_window(self.mux_window_id) {
                    if tab.pane_stack_tabs(target_pane_id).len() <= 1 {
                        if let Some(stack_id) = tab.pane_stack_id(target_pane_id) {
                            self.collapsed_pane_layouts.remove(&stack_id);
                        }
                    }
                }
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
            PaneNavAction::ToggleZoom => {
                if let Some(tab) = mux.get_active_tab_for_window(self.mux_window_id) {
                    tab.set_active_idx(pane_index);
                    tab.toggle_zoom();
                }
            }
            PaneNavAction::ToggleCollapse => {
                if let Some(tab) = mux.get_active_tab_for_window(self.mux_window_id) {
                    let Some(stack_id) = tab.pane_stack_id(pane_id) else {
                        context.invalidate();
                        return;
                    };
                    if let Some(layout) = self.collapsed_pane_layouts.remove(&stack_id) {
                        if !tab.restore_collapsed_pane(layout) {
                            self.collapsed_pane_layouts.insert(stack_id, layout);
                            context.invalidate();
                            return;
                        }
                        tab.set_active_idx(pane_index);
                    } else {
                        tab.set_active_idx(pane_index);
                        let can_collapse_direction = tab.pane_split_direction_by_index(pane_index)
                            == Some(SplitDirection::Vertical);
                        if can_collapse_direction {
                            let panes = tab.iter_panes_ignoring_zoom();
                            let has_collapsed_stack = panes.iter().any(|pane| {
                                self.collapsed_pane_layouts
                                    .contains_key(&pane.pane_stack_id)
                            });
                            let has_another_visible_stack =
                                panes.into_iter().any(|pane| pane.pane_stack_id != stack_id);
                            if !has_collapsed_stack && has_another_visible_stack {
                                if let Some(layout) = tab.collapse_pane_by_index(
                                    pane_index,
                                    self.collapsed_pane_min_cells(),
                                ) {
                                    let collapsed_stack_id = layout.pane_stack_id;
                                    self.collapsed_pane_layouts
                                        .insert(collapsed_stack_id, layout);
                                    if let Some(visible_pane) =
                                        tab.iter_panes_ignoring_zoom().into_iter().find(|pane| {
                                            pane.pane_stack_id != collapsed_stack_id
                                                && !self
                                                    .collapsed_pane_layouts
                                                    .contains_key(&pane.pane_stack_id)
                                        })
                                    {
                                        tab.set_active_idx(visible_pane.index);
                                    }
                                }
                            }
                        }
                    }
                }
            }
            PaneNavAction::SplitRight | PaneNavAction::SplitDown => {
                if let Some(tab) = mux.get_active_tab_for_window(self.mux_window_id) {
                    if tab
                        .pane_stack_id(pane_id)
                        .is_some_and(|stack_id| self.collapsed_pane_layouts.contains_key(&stack_id))
                    {
                        context.invalidate();
                        return;
                    }
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
        if self.redirect_failed_remote_tab_spawn_to_thread_view(pane_id) {
            return;
        }

        let mux = Mux::get();
        let tab = match mux.get_active_tab_for_window(self.mux_window_id) {
            Some(tab) => tab,
            None => return,
        };
        if tab
            .pane_stack_id(pane_id)
            .is_some_and(|stack_id| self.collapsed_pane_layouts.contains_key(&stack_id))
        {
            return;
        }
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

    fn mouse_event_disabled_new_tab_button(&mut self, context: &dyn WindowOps) {
        context.set_cursor(Some(MouseCursor::Arrow));
    }

    fn mouse_event_local_new_tab_button(&mut self, event: MouseEvent, context: &dyn WindowOps) {
        if event.kind == WMEK::Press(MousePress::Left) {
            self.spawn_command(
                &SpawnCommand {
                    domain: SpawnTabDomain::DomainName("local".to_string()),
                    ..SpawnCommand::default()
                },
                crate::spawn::SpawnWhere::NewTab,
            );
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
        if self.active_content_view_is_remote_thread()
            && matches!(
                item,
                TabBarItem::Tab { .. } | TabBarItem::NewTabButton { .. }
            )
        {
            context.set_cursor(Some(MouseCursor::Arrow));
            return;
        }

        match event.kind {
            WMEK::Press(MousePress::Left) => match item {
                TabBarItem::Tab { tab_idx, .. } => {
                    self.set_content_view_active(false);
                    self.activate_tab(tab_idx as isize).ok();
                }
                TabBarItem::ContentView { id } => {
                    self.activate_content_view(id);
                }
                TabBarItem::NewTabButton { .. } => {
                    self.do_new_tab_button_click(MousePress::Left);
                }
                TabBarItem::None | TabBarItem::LeftStatus | TabBarItem::RightStatus => {
                    self.mouse_event_window_header_blank(event.clone(), context);
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
                | TabBarItem::ContentView { .. }
                | TabBarItem::WindowButton(_) => {}
            },
            WMEK::Press(MousePress::Right) => match item {
                TabBarItem::Tab { tab_idx, .. } => {
                    self.show_term_context_menu(
                        context,
                        event.coords,
                        self.tab_context_menu_items(tab_idx),
                    );
                }
                TabBarItem::NewTabButton { .. } => {
                    self.do_new_tab_button_click(MousePress::Right);
                }
                TabBarItem::None
                | TabBarItem::LeftStatus
                | TabBarItem::RightStatus
                | TabBarItem::ContentView { .. }
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
                | TabBarItem::ContentView { .. }
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
                "Copy",
                "doc.on.doc",
                KeyAssignment::CopyTo(ClipboardCopyDestination::Clipboard),
            ),
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

        if matches!(event.kind, WMEK::Press(_)) && self.acknowledge_active_workspace_thread_work() {
            context.invalidate();
        }

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
            self.show_term_context_menu(context, event.coords, self.terminal_context_menu_items());
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

/// After a thread has finished materializing its saved layout, the materialize
/// guard suppressed any layout saves (so we never persist a half-built tree).
/// That means a structural change the user made *during* materialization (e.g.
/// splitting an extra pane while the thread was still rebuilding) was skipped
/// and never re-captured, and the per-window structure fingerprint is stale.
/// Reconcile once here: snapshot whatever is actually live now and re-baseline
/// the fingerprint so subsequent change detection is correct.
fn reconcile_workspace_layout_after_materialize(window: Option<::window::Window>) {
    if let Some(window) = window {
        window.notify(TermWindowNotif::Apply(Box::new(|tw| {
            tw.snapshot_active_workspace_thread_layout();
            tw.remember_workspace_layout_structure_fingerprint();
        })));
    }
}

/// After a target workspace has been materialized in a background task, adopt
/// its mux window into `window` (the window that initiated the switch) in
/// place, without disturbing any other window. Rebinding the frontend mapping
/// immediately closes the race where the additive reconcile might otherwise
/// spawn a duplicate window for the freshly materialized mux window.
pub(crate) fn adopt_workspace_into_window(window: &Option<::window::Window>, workspace: &str) {
    let mux = Mux::get();
    let Some(target) = mux.iter_windows_in_workspace(workspace).first().copied() else {
        return;
    };
    mux.set_active_workspace(workspace);
    if let Some(window) = window.as_ref() {
        front_end().rebind_known_window(window, target);
        window.notify(TermWindowNotif::SwitchToMuxWindow(target));
    }
}

fn cleanup_orphaned_mux_window(window_id: Option<MuxWindowId>) {
    let Some(window_id) = window_id else {
        return;
    };
    if front_end().has_mux_window(window_id) {
        return;
    }

    let mux = Mux::get();
    let Some(window) = mux.get_window(window_id) else {
        return;
    };
    let workspace = window.get_workspace().to_string();
    drop(window);

    if crate::workspace_threads::workspace_has_thread_binding(&workspace) {
        return;
    }

    log::trace!("clean up unbound startup mux window {window_id} in workspace {workspace:?}");
    mux.kill_window(window_id);
}

fn kill_workspace_windows(workspaces: &[String], skip_workspace: Option<&str>) {
    let mux = Mux::get();
    for workspace in workspaces {
        if skip_workspace == Some(workspace.as_str()) {
            continue;
        }
        for window_id in mux.iter_windows_in_workspace(workspace) {
            mux.kill_window(window_id);
        }
    }
}
