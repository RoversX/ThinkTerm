use crate::frontend::front_end;
use crate::tabbar::TabBarItem;
use crate::termwindow::content_view::ContentViewId;
#[cfg(target_os = "macos")]
use crate::termwindow::space_swipe::{SidebarSpaceSwipeFinish, SidebarSpaceSwipeUpdate};
use crate::termwindow::ui::platform_chrome::WindowTabChromeParams;
use crate::termwindow::ui::sidebar::SpaceConnectionState;
use crate::termwindow::ui::tokens::{
    PANE_NAV_INSET, PANE_NAV_TAB_GAP, TAB_MIN_WIDTH, TAB_ROW_START_PADDING, TAB_TARGET_WIDTH,
    TAB_VERTICAL_PADDING, WINDOW_TAB_ACTION_RESERVED_WIDTH, WINDOW_TAB_GAP,
    WINDOW_TAB_LEADING_ACTION_BUTTON_SIZE, WINDOW_TAB_LEADING_ACTION_GAP, WINDOW_TAB_TOP_SPACER,
};
use crate::termwindow::{
    pane_drop_action, pane_drop_zone, zone_split_request, GuiWin, MouseCapture, PaneDropKind,
    PaneDropZone, PaneNavAction, PaneTabDragState, PaneTabDropTarget, PositionedSplit, ScrollHit,
    TabWheelSurface, TermWindowNotif, UIItem, UIItemType, TMB,
};
#[cfg(target_os = "macos")]
use ::window::ScrollPhase;
use ::window::{
    ContextMenuIcon, ContextMenuItem, IntegratedTitleButtonStyle, MouseButtons as WMB, MouseCursor,
    MouseEvent, MouseEventKind as WMEK, MousePress, WindowOps, WindowState,
};
use config::keyassignment::{
    ClipboardCopyDestination, ClipboardPasteSource, KeyAssignment, MouseEventTrigger,
    PaneDirection, SpawnCommand, SpawnTabDomain, SplitPane, SplitSize,
};
use config::{MouseEventAltScreen, TermConfig};
use fluent_bundle::FluentArgs;
use mux::domain::SplitSource;
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

/// Returns an in-flight startup-fallback repair to the pending state if its
/// detached future is cancelled before it can report an outcome.
struct DeniedProjectFallbackRepair {
    workspace: Option<String>,
}

impl DeniedProjectFallbackRepair {
    fn begin(workspace: String) -> Option<Self> {
        crate::workspace_threads::begin_denied_project_fallback_repair(&workspace).then_some(Self {
            workspace: Some(workspace),
        })
    }

    fn complete(mut self) {
        if let Some(workspace) = self.workspace.take() {
            crate::workspace_threads::complete_denied_project_fallback_repair(&workspace);
        }
    }

    fn retry(mut self) {
        if let Some(workspace) = self.workspace.take() {
            crate::workspace_threads::retry_denied_project_fallback_repair(&workspace);
        }
    }
}

impl Drop for DeniedProjectFallbackRepair {
    fn drop(&mut self) {
        if let Some(workspace) = self.workspace.take() {
            crate::workspace_threads::retry_denied_project_fallback_repair(&workspace);
        }
    }
}

fn tr_with_name(id: &'static str, name: &str) -> String {
    let mut args = FluentArgs::new();
    args.set("name", name.to_string());
    crate::i18n::tr_args(id, &args)
}

/// The entries that get rid of the active Space.
///
/// A remote Space has up to three of them and each one wants the Space named to
/// be unambiguous, which made the menu four lines of the same name. They fold
/// into one submenu instead: the name is said once, on the parent, and the
/// children say only what they do. A local Space has a single action, so it
/// stays flat — a submenu holding one item is just an extra click.
fn space_destructive_menu_items(
    space: &crate::workspace_threads::SpaceView,
    offer_remote_kill: bool,
) -> Vec<ContextMenuItem> {
    if !space.is_remote {
        return vec![ContextMenuItem::item_with_icon(
            tr_with_name("menu-delete-space", &space.name),
            ContextMenuIcon::Delete,
            KeyAssignment::DeleteSpace(space.id.clone()),
        )];
    }

    // A remote Space lives on its server, so removing it here only drops this
    // device's copy: the Space is untouched for everyone else and returns on
    // the next connect. Deleting it for real is a separate, more-consequential
    // entry offered alongside.
    let mut children = vec![
        ContextMenuItem::item_with_icon(
            crate::i18n::tr("menu-space-disconnect-short"),
            ContextMenuIcon::Disconnect,
            KeyAssignment::DeleteSpace(space.id.clone()),
        ),
        ContextMenuItem::item_with_icon(
            crate::i18n::tr("menu-space-delete-everywhere-short"),
            ContextMenuIcon::Delete,
            KeyAssignment::DeleteSpaceEverywhere(space.id.clone()),
        ),
    ];
    if offer_remote_kill {
        children.push(ContextMenuItem::item_with_icon(
            crate::i18n::tr("menu-space-delete-and-sessions-short"),
            ContextMenuIcon::Delete,
            KeyAssignment::DeleteSpaceAndRemoteSessions(space.id.clone()),
        ));
    }

    vec![ContextMenuItem::submenu_with_icon(
        tr_with_name("menu-remove-named-space", &space.name),
        ContextMenuIcon::Delete,
        children,
    )]
}

fn note_drag_scroll_delta(
    pointer_y: f32,
    top: f32,
    bottom: f32,
    edge: f32,
    max_step: f32,
) -> Option<f32> {
    let edge = edge.min((bottom - top).max(0.0) / 3.0).max(1.0);
    if pointer_y < top + edge {
        let intensity = ((top + edge - pointer_y) / edge).clamp(0.2, 1.0);
        Some(-max_step * intensity)
    } else if pointer_y > bottom - edge {
        let intensity = ((pointer_y - (bottom - edge)) / edge).clamp(0.2, 1.0);
        Some(max_step * intensity)
    } else {
        None
    }
}

fn note_horizontal_scroll_offset(current: f32, delta: f32, maximum: f32) -> f32 {
    (current + delta).clamp(0.0, maximum.max(0.0))
}

fn trailing_action_reserved_width(
    fixed_clearance: usize,
    action_button_count: usize,
    action_button_size: usize,
    action_button_gap: usize,
    window_button_count: usize,
) -> usize {
    fixed_clearance
        .saturating_add(action_button_count.saturating_mul(action_button_size))
        .saturating_add(
            action_button_count
                .saturating_sub(1)
                .saturating_mul(action_button_gap),
        )
        .saturating_add(if window_button_count > 0 {
            window_button_count
                .saturating_mul(action_button_size + action_button_gap / 2)
                .saturating_add(action_button_gap)
        } else {
            0
        })
}

fn remote_connect_can_reveal(
    status: Option<&SshConnectionStatus>,
    workspace_has_window: bool,
) -> bool {
    workspace_has_window
        && matches!(
            status,
            Some(SshConnectionStatus::Authenticating | SshConnectionStatus::Connected)
        )
}

fn remote_thread_uses_ssh_connection_view(is_remote: bool, space_has_client_domain: bool) -> bool {
    is_remote && !space_has_client_domain
}

fn missing_ssh_host_blocks_activation(
    project_is_remote: bool,
    has_ssh_host: bool,
    space_has_client_domain: bool,
) -> bool {
    project_is_remote && !has_ssh_host && !space_has_client_domain
}

/// Divide a tab row of `viewport_width` among `tab_count` tabs.
///
/// Everything is already in device pixels, which keeps the rule testable
/// without a live window. `target` is the width a tab wants; `floor` is the
/// narrowest it stays readable at. Once the tabs no longer fit even at `floor`
/// the row overflows and its caller scrolls.
fn adaptive_tab_width_pixels(
    tab_count: usize,
    viewport_width: f32,
    gap: f32,
    target: f32,
    floor: f32,
) -> f32 {
    let floor = floor.min(target);
    if tab_count <= 1 {
        return target;
    }
    let count = tab_count as f32;
    let usable = (viewport_width - gap * (count - 1.0)).max(0.0);
    // floor(), never ceil(): at the shrink threshold the tabs plus their gaps
    // have to add up to at most the viewport, or every row would carry a pixel
    // of phantom scroll.
    (usable / count).floor().clamp(floor, target)
}

#[cfg(test)]
mod window_tab_layout_tests {
    use super::{
        missing_ssh_host_blocks_activation, note_drag_scroll_delta, note_horizontal_scroll_offset,
        remote_connect_can_reveal, remote_thread_uses_ssh_connection_view,
        trailing_action_reserved_width,
    };
    use mux::ssh::SshConnectionStatus;

    #[test]
    fn trailing_actions_keep_tabs_before_the_new_tab_button() {
        assert_eq!(trailing_action_reserved_width(26, 2, 34, 8, 0), 102);
        assert_eq!(trailing_action_reserved_width(26, 2, 64, 8, 0), 162);
    }

    #[test]
    fn trailing_actions_include_integrated_window_buttons() {
        assert_eq!(trailing_action_reserved_width(26, 2, 34, 8, 3), 224);
    }

    #[test]
    fn note_drag_edge_scroll_has_direction_and_safe_zone() {
        assert_eq!(note_drag_scroll_delta(50.0, 0.0, 200.0, 40.0, 28.0), None);
        assert_eq!(
            note_drag_scroll_delta(0.0, 0.0, 200.0, 40.0, 28.0),
            Some(-28.0)
        );
        assert_eq!(
            note_drag_scroll_delta(200.0, 0.0, 200.0, 40.0, 28.0),
            Some(28.0)
        );
        assert!(
            note_drag_scroll_delta(30.0, 0.0, 200.0, 40.0, 28.0).expect("top edge delta") < 0.0
        );
    }

    #[test]
    fn note_horizontal_scroll_is_clamped_to_table_extent() {
        assert_eq!(note_horizontal_scroll_offset(20.0, 15.0, 100.0), 35.0);
        assert_eq!(note_horizontal_scroll_offset(95.0, 15.0, 100.0), 100.0);
        assert_eq!(note_horizontal_scroll_offset(5.0, -15.0, 100.0), 0.0);
        assert_eq!(note_horizontal_scroll_offset(5.0, 15.0, 0.0), 0.0);
    }

    #[test]
    fn remote_connect_reveals_only_after_the_target_workspace_exists() {
        assert!(!remote_connect_can_reveal(
            Some(&SshConnectionStatus::Authenticating),
            false
        ));
        assert!(remote_connect_can_reveal(
            Some(&SshConnectionStatus::Authenticating),
            true
        ));
        assert!(!remote_connect_can_reveal(
            Some(&SshConnectionStatus::Connected),
            false
        ));
        assert!(remote_connect_can_reveal(
            Some(&SshConnectionStatus::Connected),
            true
        ));
        assert!(!remote_connect_can_reveal(
            Some(&SshConnectionStatus::Connecting),
            true
        ));
        assert!(!remote_connect_can_reveal(
            Some(&SshConnectionStatus::Failed("failed".to_string())),
            true
        ));
    }

    #[test]
    fn mux_domain_threads_bypass_the_ssh_connection_view() {
        assert!(remote_thread_uses_ssh_connection_view(true, false));
        assert!(!remote_thread_uses_ssh_connection_view(true, true));
        assert!(!remote_thread_uses_ssh_connection_view(false, false));
    }

    #[test]
    fn mux_domain_threads_do_not_require_an_ssh_host_record() {
        assert!(missing_ssh_host_blocks_activation(true, false, false));
        assert!(!missing_ssh_host_blocks_activation(true, false, true));
        assert!(!missing_ssh_host_blocks_activation(true, true, false));
        assert!(!missing_ssh_host_blocks_activation(false, false, false));
    }
}

/// Overall ceiling on the "Connecting…" phase, as a backstop for the case where
/// the TCP connect succeeds but the SSH banner/handshake then stalls (the
/// per-connect `connecttimeout` only bounds the TCP connect itself).
const REMOTE_CONNECT_OVERALL_TIMEOUT_SECS: u64 = 20;

impl super::TermWindow {
    pub(crate) fn collapsed_pane_min_cells(&self) -> usize {
        let nav_height = self.pane_nav_bar_height();
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

    /// How far one wheel notch moves a sidebar surface, in physical pixels.
    ///
    /// This deliberately goes through the same helper the settings window
    /// uses, because the two have to agree: a notch used to be a flat 6px here
    /// (capped at 42px no matter how many notches an event carried) while
    /// settings scrolled 34 *design* pixels per notch. Rows and preview lines
    /// are laid out in dpi-scaled pixels, so the flat version covered half as
    /// much ground on a 200% display as on a 100% one - on Windows at 200% a
    /// notch moved less than half a row while the same notch in settings moved
    /// three rows.
    fn sidebar_scroll_pixels(&self, amount: i16) -> f32 {
        crate::ui::wheel_delta_pixels(amount, crate::ui::ui_scale_for_dpi(self.dimensions.dpi))
    }

    fn sidebar_vertical_scroll_delta(&self, event: &MouseEvent) -> Option<f32> {
        if !matches!(event.kind, WMEK::VertWheel(_)) {
            return None;
        }
        if let Some(delta) = event.precise_scroll_delta {
            if delta.y.abs() > f32::EPSILON {
                return Some(-delta.y);
            }
        }
        match event.kind {
            WMEK::VertWheel(amount) if amount != 0 => Some(self.sidebar_scroll_pixels(amount)),
            WMEK::VertWheel(_) => Some(0.0),
            _ => None,
        }
    }

    fn sidebar_horizontal_scroll_delta(&self, event: &MouseEvent) -> Option<f32> {
        if !matches!(event.kind, WMEK::HorzWheel(_)) {
            return None;
        }
        if let Some(delta) = event.precise_scroll_delta {
            if delta.x.abs() > f32::EPSILON {
                return Some(-delta.x);
            }
        }
        match event.kind {
            WMEK::HorzWheel(amount) if amount != 0 => {
                Some(self.sidebar_scroll_pixels(amount) * 2.0)
            }
            WMEK::HorzWheel(_) => Some(0.0),
            _ => None,
        }
    }

    /// Width of one tab surface in a row that holds `tab_count` of them.
    ///
    /// Tabs are chrome, so the terminal font plays no part: the row's own width
    /// and how many tabs share it decide everything. See `TAB_TARGET_WIDTH`.
    pub(super) fn adaptive_tab_width(&self, tab_count: usize, viewport_width: f32, gap: f32) -> f32 {
        // Whole pixels: the painters round the width up before laying tabs out,
        // and the scroll clamps use it raw. A fractional ui scale would
        // otherwise leave them disagreeing by up to a pixel per tab.
        adaptive_tab_width_pixels(
            tab_count,
            viewport_width,
            gap,
            self.ui_f32(TAB_TARGET_WIDTH).floor(),
            self.ui_f32(TAB_MIN_WIDTH).floor(),
        )
    }

    /// The gap the window tab row leaves between neighbouring tabs.
    pub(super) fn window_tab_gap_pixels(&self) -> f32 {
        if self.config.use_fancy_tab_bar {
            self.ui_px(WINDOW_TAB_GAP) as f32
        } else {
            0.0
        }
    }

    /// How many surfaces the window tab row paints: the mux tabs plus the
    /// synthetic content-view tabs. Counted off `tab_bar` rather than the mux
    /// window so this matches `paint_fancy_tab_bar` frame for frame — tab width
    /// now depends on the count, so a stale one would resize every tab.
    pub(super) fn window_tab_bar_item_count(&self) -> usize {
        let mux_tabs = if self.active_content_view_is_remote_thread() {
            0
        } else {
            self.tab_bar
                .items()
                .iter()
                .filter(|item| matches!(item.item, TabBarItem::Tab { .. }))
                .count()
        };
        mux_tabs + self.content_view_count()
    }

    pub(super) fn window_tab_width_pixels(&self) -> f32 {
        self.adaptive_tab_width(
            self.window_tab_bar_item_count(),
            self.window_tab_viewport_width(),
            self.window_tab_gap_pixels(),
        )
    }

    /// The collapsed pane strip still sizes its chips off the terminal cell
    /// grid; only the window tab row and the pane nav bar moved to
    /// `adaptive_tab_width`.
    pub(super) fn collapsed_pane_tab_width_pixels(&self) -> f32 {
        let cell_width = self.render_metrics.cell_size.width.max(1) as f32;
        (self.config.tab_max_width as f32 * cell_width)
            .max(cell_width * 15.0)
            .max(self.ui_f32(176.0))
            .ceil()
    }

    pub(crate) fn window_tab_chrome_params(&self) -> WindowTabChromeParams {
        WindowTabChromeParams {
            use_fancy_tab_bar: self.config.use_fancy_tab_bar,
            workspace_sidebar_width: self.workspace_sidebar_width(),
            window_state: self.window_state,
            window_decorations: self.config.window_decorations,
            integrated_title_button_alignment: self.config.integrated_title_button_alignment,
            integrated_title_button_style: self.config.integrated_title_button_style,
            cell_width: self.render_metrics.cell_size.width.max(1) as f32,
            dpi: self.dimensions.dpi,
            top_fancy_row_height: if self.show_tab_bar
                && self.config.use_fancy_tab_bar
                && !self.config.tab_bar_at_bottom
            {
                self.tab_bar_pixel_height()
                    .ok()
                    .map(|h| h.ceil() as usize)
                    .filter(|h| *h > 0)
            } else {
                None
            },
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
        let configured_button_size = self.ui_px(WINDOW_TAB_LEADING_ACTION_BUTTON_SIZE);
        let action_button_size = if self.config.use_fancy_tab_bar {
            let row_height = self
                .tab_bar_pixel_height()
                .unwrap_or(configured_button_size as f32)
                .ceil() as usize;
            let content_top_spacer = if self.config.tab_bar_at_bottom {
                0
            } else {
                self.ui_px(WINDOW_TAB_TOP_SPACER).min(row_height)
            };
            let content_height = row_height.saturating_sub(content_top_spacer);
            let tab_font_cell_height_upper_bound = content_height.div_ceil(2);
            content_height
                .saturating_sub(self.ui_px(TAB_VERTICAL_PADDING) * 2)
                .max(tab_font_cell_height_upper_bound)
                .max(1)
                .max(configured_button_size)
        } else {
            configured_button_size
        };
        let action_button_count = if !self.right_sidebar_has_panels() {
            // Every sidebar panel is off, so fancy_tab_bar paints no toggle;
            // reserving its width anyway just shortens the tab strip.
            1
        } else if self.right_sidebar_width() > 0 && cfg!(target_os = "macos") {
            1
        } else {
            2
        };
        let window_button_count = if self.right_sidebar_width() == 0
            && self
                .window_tab_chrome_params()
                .uses_integrated_window_buttons()
            && self.config.integrated_title_button_style != IntegratedTitleButtonStyle::MacOsNative
        {
            self.config.integrated_title_buttons.len()
        } else {
            0
        };
        let fixed_clearance = self
            .ui_px(WINDOW_TAB_ACTION_RESERVED_WIDTH)
            .saturating_sub(configured_button_size);

        trailing_action_reserved_width(
            fixed_clearance,
            action_button_count,
            action_button_size,
            self.ui_px(WINDOW_TAB_LEADING_ACTION_GAP),
            window_button_count,
        )
    }

    pub(super) fn pane_nav_tab_left_inset(&self, pane_left: usize) -> usize {
        if pane_left == 0 && self.workspace_sidebar_width() > 0 {
            self.ui_px(TAB_ROW_START_PADDING)
        } else {
            self.ui_px(PANE_NAV_INSET)
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
            // ceil, matching `paint_fancy_tab_bar`'s viewport_left: tab width is
            // derived from this number, so a one-pixel disagreement would clip
            // the last tab in a row that otherwise fits exactly.
            .saturating_sub(left_padding.max(0.0).ceil() as usize)
            .saturating_sub(if self.config.use_fancy_tab_bar {
                self.ui_px(TAB_ROW_START_PADDING) + self.window_tab_trailing_action_reserved_width()
            } else {
                0
            })
            .max(1) as f32
    }

    pub(super) fn max_window_tab_scroll_offset(&self) -> f32 {
        let tab_count = self.window_tab_bar_item_count();
        if tab_count <= 1 {
            return 0.0;
        }

        let tab_width = self.window_tab_width_pixels();
        let tab_gap = self.window_tab_gap_pixels();
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

        // Share the painter's viewport rather than re-deriving one here: it
        // used to assume two trailing action buttons where the painter draws
        // four or five, and now that tab width follows the row, any drift would
        // strand the last tab out of scroll range.
        let tab_gap = self.ui_px(PANE_NAV_TAB_GAP) as f32;
        // A collapsed stack has its own strip geometry and its own (legacy,
        // cell-derived) tab width; clamping it against the expanded bar's
        // figures left it with a stretch of dead scroll.
        let (viewport_width, tab_width) = if self.collapsed_pane_layouts.contains_key(&stack_id) {
            (
                self.collapsed_pane_nav_tab_viewport_width(&pos),
                self.collapsed_pane_tab_width_pixels().ceil(),
            )
        } else {
            let viewport_width = self.pane_nav_tab_viewport_width(&pos);
            (
                viewport_width,
                self.adaptive_tab_width(tab_count, viewport_width, tab_gap),
            )
        };
        let total_width =
            tab_count as f32 * tab_width + tab_count.saturating_sub(1) as f32 * tab_gap;

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
        let cell_height = self.render_metrics.cell_size.height as f32;
        let x = event.coords.x as f32;
        let y = event.coords.y as f32;

        let nav_height = self.pane_nav_bar_height() as f32;
        // iter_panes (NOT ignoring_zoom): the hit test must cover exactly
        // the panes being painted. With a pane zoomed, the unzoomed
        // layout's header strips would otherwise remain as invisible
        // wheel-consuming bands inside the zoomed pane, at every hidden
        // pane's old header position.
        for pos in tab.iter_panes() {
            let Ok((pane_x, pane_width)) = self.pane_chrome_span(&pos) else {
                continue;
            };
            let pane_y = top_bar_height + border.top.get() as f32 + pos.top as f32 * cell_height;
            if x >= pane_x && x < pane_x + pane_width && y >= pane_y && y < pane_y + nav_height {
                // The zoomed branch of iter_panes fills pane_stack_id with
                // the pane's own id, not the real stack id; resolve through
                // the mux like the ui-item branch above so the wheel
                // scrolls the stack actually under the pointer.
                let stack_id = Mux::get()
                    .pane_stack_id(pos.pane.pane_id())
                    .unwrap_or(pos.pane_stack_id);
                return Some(TabWheelSurface::PaneStack(stack_id));
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

    /// Feed the pointer to the hover-reveal machine. Called for every
    /// pointer event, before any dispatch, and never consumes one.
    fn update_workspace_sidebar_hover(&mut self, context: &dyn WindowOps) {
        let input = self.workspace_sidebar_hover_input();
        if self
            .workspace_sidebar_hover
            .step(input, std::time::Instant::now())
            != crate::termwindow::sidebar_hover::HoverFrame::None
        {
            // Deadlines are registered by the paint pass, not here:
            // `update_next_frame_time` writes into `has_animation`, which
            // `paint_impl` clears on entry. Asking for a frame is enough --
            // that frame's advance registers the wakeup.
            context.invalidate();
        }
    }

    pub(crate) fn workspace_sidebar_hover_input(
        &self,
    ) -> crate::termwindow::sidebar_hover::HoverInput {
        use crate::termwindow::sidebar_hover::{HoverInput, PointerZone};
        let eligible = crate::native_settings::workspace_sidebar_hover_reveal_enabled()
            && self.workspace_sidebar_collapsed
            && !self.content_view_foreground()
            && !self.content_view_transition_running()
            && !self.frontend_surface_blocked()
            && self.modal.borrow().is_none()
            && self.context_menu.is_none()
            && !self.native_context_menu_open
            && self.inline_tab_rename.is_none();
        // Panel is checked before the hot zone on purpose: once revealed,
        // the strip is inside the panel.
        let pointer = match &self.current_mouse_event {
            None => PointerZone::Away,
            Some(event) => {
                let x = event.coords.x;
                let y = event.coords.y;
                let in_panel = self.workspace_sidebar_rect().is_some_and(|rect| {
                    x >= rect.x as isize
                        && x < rect.x.saturating_add(rect.width) as isize
                        && y >= rect.y as isize
                        && y < rect.y.saturating_add(rect.height) as isize
                });
                let zone = self
                    .workspace_sidebar_hover_hot_zone()
                    .and_then(|(zx, zy, zw, zh)| {
                        if y < zy as isize || y >= zy.saturating_add(zh) as isize || x < zx as isize
                        {
                            return None;
                        }
                        if x < zx.saturating_add(zw) as isize {
                            return Some(PointerZone::HotZone);
                        }
                        // The sticky band shares the strip's vertical extent
                        // and only widens it: close enough to keep a running
                        // dwell alive, not close enough to start one.
                        let sticky = self
                            .ui_px(crate::termwindow::ui::tokens::SIDEBAR_HOVER_STICKY_ZONE_WIDTH);
                        (x < zx.saturating_add(sticky) as isize).then_some(PointerZone::NearHotZone)
                    });
                // The sidebar toggle button is a hot zone of its own:
                // pointing at it already says "I want the sidebar", so the
                // collapsed panel reveals on hover there too (the same dwell
                // applies, and set_workspace_sidebar_shown's suppression
                // keeps a collapse click from instantly re-revealing).
                let over_toggle = self.ui_items.iter().any(|item| {
                    matches!(item.item_type, UIItemType::WorkspaceSidebarToggle)
                        && x >= item.x as isize
                        && x < item.x.saturating_add(item.width) as isize
                        && y >= item.y as isize
                        && y < item.y.saturating_add(item.height) as isize
                });
                if in_panel {
                    PointerZone::Panel
                } else if over_toggle {
                    PointerZone::HotZone
                } else {
                    zone.unwrap_or(PointerZone::Away)
                }
            }
        };
        // The native macOS titlebar button lives outside our view, so it is
        // invisible to current_mouse_event; its hover arrives as a window
        // event and overrides the pointer zone here.
        let pointer = if self.titlebar_sidebar_button_hovered {
            PointerZone::HotZone
        } else {
            pointer
        };
        let pinned = !self.current_mouse_buttons.is_empty()
            || self.current_mouse_capture.is_some()
            || self.dragging.is_some()
            || self.sidebar_row_drag.is_some()
            || self.right_sidebar_file_drag.is_some()
            || self.pane_tab_drag.is_some()
            // A Space swipe on the revealed panel pins it open: retreating
            // mid-gesture would slide the ground out from under the pages.
            || self.workspace_sidebar_swipe.is_active();
        HoverInput {
            eligible,
            pointer,
            pinned,
        }
    }

    fn mouse_wheel_workspace_sidebar(
        &mut self,
        event: &MouseEvent,
        context: &dyn WindowOps,
    ) -> bool {
        #[cfg(target_os = "macos")]
        if let Some(handled) = self.mouse_wheel_workspace_sidebar_space_swipe(event, context) {
            return handled;
        }

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

        let delta = match event.kind {
            WMEK::VertWheel(_) => self.sidebar_vertical_scroll_delta(event).unwrap_or(0.0),
            // Trackpads often emit a little horizontal inertia while the user is
            // vertically scrolling. Consume it inside the sidebar so it doesn't
            // leak to tab or terminal wheel handlers at the scroll bounds.
            WMEK::HorzWheel(_) => return true,
            _ => return false,
        };
        if delta.abs() <= f32::EPSILON {
            return true;
        }

        self.scroll_workspace_sidebar_by(delta, context);
        true
    }

    fn scroll_workspace_sidebar_by(&mut self, delta: f32, context: &dyn WindowOps) {
        if delta.abs() <= f32::EPSILON {
            return;
        }
        let max_offset = self.workspace_sidebar_scroll_max();
        if max_offset <= 0.0 {
            return;
        }

        self.show_workspace_sidebar_scrollbar();
        let offset = (self.workspace_sidebar_scroll_offset + delta).clamp(0.0, max_offset);
        if (offset - self.workspace_sidebar_scroll_offset).abs() > f32::EPSILON {
            self.workspace_sidebar_scroll_offset = offset;
            context.invalidate();
        } else {
            context.invalidate();
        }
    }

    #[cfg(target_os = "macos")]
    fn mouse_wheel_workspace_sidebar_space_swipe(
        &mut self,
        event: &MouseEvent,
        context: &dyn WindowOps,
    ) -> Option<bool> {
        if !matches!(event.kind, WMEK::VertWheel(_) | WMEK::HorzWheel(_)) {
            return None;
        }

        if matches!(event.momentum_phase, Some(ScrollPhase::Began)) && event.scroll_phase.is_none()
        {
            // Some macOS trackpads transition directly from Changed into
            // momentum without emitting a scroll-phase Ended event. Treat the
            // beginning of momentum as the finger-up boundary so a horizontal
            // gesture can commit instead of remaining stuck in Tracking.
            self.finish_workspace_sidebar_space_swipe(Instant::now(), context);
        }

        if let Some(momentum_phase) = event.momentum_phase {
            let ended = matches!(momentum_phase, ScrollPhase::Ended | ScrollPhase::Cancelled);
            if self.workspace_sidebar_swipe.consume_momentum(ended) {
                return Some(true);
            }
        }

        let scroll_phase = event.scroll_phase?;
        let (delta_x, delta_y) = event
            .precise_scroll_delta
            .map(|delta| (delta.x, delta.y))
            .unwrap_or((0.0, 0.0));
        let inside_sidebar = self.workspace_sidebar_rect().is_some_and(|rect| {
            let x = event.coords.x;
            let y = event.coords.y;
            x >= rect.x as isize
                && x < rect.x.saturating_add(rect.width) as isize
                && y >= rect.y as isize
                && y < rect.y.saturating_add(rect.height) as isize
        });
        if !inside_sidebar && !self.workspace_sidebar_swipe.is_active() {
            return None;
        }

        // Docked, or hover-revealed and fully arrived: a panel still sliding
        // out must not host a second animation.
        let panel_ready_for_swipe = !self.workspace_sidebar_collapsed
            || self
                .workspace_sidebar_hover
                .is_fully_presented(Instant::now());
        let can_begin_without_space_lookup = inside_sidebar
            && panel_ready_for_swipe
            && self.context_menu.is_none()
            && self.modal.borrow().is_none()
            && self.inline_tab_rename.is_none()
            && self.current_mouse_capture.is_none()
            && self.dragging.is_none()
            && self.sidebar_row_drag.is_none()
            && self.right_sidebar_file_drag.is_none()
            && self.pane_tab_drag.is_none();
        let now = Instant::now();

        if matches!(scroll_phase, ScrollPhase::MayBegin) {
            if !can_begin_without_space_lookup {
                return None;
            }
            let active_space_is_local =
                crate::workspace_threads::spaces_for_window(self.space_owner_id)
                    .iter()
                    .any(|space| space.id == self.active_space_id && !space.is_remote);
            if active_space_is_local {
                self.prepare_workspace_space_swipe_source_capture();
                context.invalidate();
                return Some(true);
            }
            return None;
        }

        let needs_begin = matches!(scroll_phase, ScrollPhase::Began)
            || (matches!(scroll_phase, ScrollPhase::Changed)
                && !self.workspace_sidebar_swipe.is_active());
        if needs_begin {
            if !can_begin_without_space_lookup {
                return None;
            }
            if matches!(scroll_phase, ScrollPhase::Began)
                && self.workspace_sidebar_swipe.is_active()
            {
                // A fresh gesture supersedes any unfinished/settling visual
                // from the preceding gesture.
                self.workspace_sidebar_swipe.cancel_immediately();
                self.clear_workspace_space_swipe_frame_transition();
            }
            let spaces = crate::workspace_threads::spaces_for_window(self.space_owner_id);
            if !spaces
                .iter()
                .any(|space| space.id == self.active_space_id && space.is_swipe_reachable())
            {
                return None;
            }
            let (previous, next) =
                crate::workspace_threads::adjacent_swipe_space_ids(&spaces, &self.active_space_id);
            if self.workspace_space_swipe_source_frame.is_none() {
                self.prepare_workspace_space_swipe_source_capture();
            }
            // Whether the pages tracked is a fact about *this* gesture. The
            // previous one having tracked must not decide how this one opens.
            self.workspace_space_swipe_tracked = false;
            self.workspace_sidebar_swipe.begin(
                self.active_space_id.clone(),
                previous,
                next,
                self.workspace_sidebar_scroll_offset,
                now,
            );
        }

        if matches!(scroll_phase, ScrollPhase::Began | ScrollPhase::Changed)
            && (delta_x.abs() > f32::EPSILON || delta_y.abs() > f32::EPSILON)
        {
            let update = self.workspace_sidebar_swipe.update(delta_x, delta_y, now);
            match update {
                SidebarSpaceSwipeUpdate::Pending => {}
                SidebarSpaceSwipeUpdate::Horizontal => context.invalidate(),
                SidebarSpaceSwipeUpdate::Vertical(delta_y) => {
                    self.clear_workspace_space_swipe_frame_transition();
                    self.scroll_workspace_sidebar_by(-delta_y, context);
                }
            }
        }

        match scroll_phase {
            ScrollPhase::Ended => {
                self.finish_workspace_sidebar_space_swipe(now, context);
            }
            ScrollPhase::Cancelled => {
                self.workspace_sidebar_swipe.cancel_immediately();
                self.clear_workspace_space_swipe_frame_transition();
                context.invalidate();
            }
            _ => {}
        }

        Some(true)
    }

    #[cfg(target_os = "macos")]
    fn finish_workspace_sidebar_space_swipe(
        &mut self,
        now: Instant,
        context: &dyn WindowOps,
    ) -> bool {
        let width = self.workspace_sidebar_presented_width() as f32;
        let finish = self.workspace_sidebar_swipe.finish(now, width);
        match finish {
            SidebarSpaceSwipeFinish::Switch(target) => {
                let direction = self
                    .workspace_sidebar_swipe
                    .visual(now, width)
                    .map(|visual| visual.offset.signum())
                    .unwrap_or(0.0);
                self.workspace_space_swipe_pending_commit = Some((target, direction));
                if self.workspace_space_swipe_source_frame.is_none() {
                    // A very fast flick can end before AppKit presents even
                    // one source-sidebar paint. Defer the actual Space adoption
                    // until paint_pass has mirrored the still-current sidebar;
                    // this keeps its list-page transition from degrading into
                    // an immediate destination flash.
                    self.workspace_space_swipe_capture_source = true;
                } else {
                    self.complete_workspace_space_swipe_switch();
                }
                context.invalidate();
                true
            }
            SidebarSpaceSwipeFinish::AnimateBack => {
                // The pages already followed the finger out, so they have to
                // travel back rather than blink into place. `finish` has put
                // the state machine into a settle that does exactly that;
                // leave the captured neighbour alive to be composited until
                // `advance` retires it.
                context.invalidate();
                true
            }
            SidebarSpaceSwipeFinish::FlushVertical(delta_y) => {
                self.clear_workspace_space_swipe_frame_transition();
                self.scroll_workspace_sidebar_by(-delta_y, context);
                true
            }
            SidebarSpaceSwipeFinish::None => {
                // `None` means "this call had no gesture to settle". That is
                // true both when nothing was in flight and when a previous
                // call already moved the state machine into AwaitingCommit or
                // Settling -- macOS routinely delivers a trailing Ended after
                // momentum has already ended the gesture, so this runs a
                // second time right after a commit. Only tear down the frame
                // transition when the state machine really is idle; otherwise
                // the just-committed push is destroyed before its first frame.
                if !self.workspace_sidebar_swipe.is_active() {
                    self.clear_workspace_space_swipe_frame_transition();
                }
                false
            }
        }
    }

    #[cfg(target_os = "macos")]
    pub(super) fn complete_workspace_space_swipe_switch(&mut self) {
        let Some((target, direction)) = self.workspace_space_swipe_pending_commit.take() else {
            return;
        };
        let width = self.workspace_sidebar_presented_width() as f32;
        let now = Instant::now();
        let source_is_current = self
            .workspace_sidebar_swipe
            .visual(now, width)
            .is_some_and(|visual| visual.source_space_id == self.active_space_id);
        let target_is_available = crate::workspace_threads::spaces_for_window(self.space_owner_id)
            .iter()
            .any(|space| space.id == target && space.is_swipe_reachable());
        let gui_window = self.window.clone();
        let switched = source_is_current
            && target_is_available
            && gui_window
                .as_ref()
                .is_some_and(|window| self.switch_space_to_thread(target, None, window));

        self.workspace_space_swipe_push_active =
            switched && direction != 0.0 && self.workspace_space_swipe_source_frame.is_some();
        self.workspace_space_swipe_direction = if self.workspace_space_swipe_push_active {
            direction
        } else {
            0.0
        };
        self.workspace_space_swipe_capture_source = false;
        // The Space this held is the one now adopted, so it is the live paint
        // from here on; keeping the capture would only leave the compositor a
        // stale copy to choose.
        self.workspace_space_swipe_target_frame = None;
        self.workspace_space_swipe_needs_settle_start = self.workspace_space_swipe_push_active;
        if !switched {
            self.workspace_sidebar_swipe.resolve_switch(
                false,
                Instant::now(),
                width,
                crate::termwindow::space_swipe::SettleOpening::WhereTheFingerLeftIt,
            );
        }
        if !self.workspace_space_swipe_push_active {
            self.workspace_sidebar_swipe.cancel_immediately();
            self.workspace_space_swipe_source_frame = None;
        }
        if let Some(window) = gui_window {
            window.invalidate();
        }
    }

    #[cfg(target_os = "macos")]
    fn prepare_workspace_space_swipe_source_capture(&mut self) {
        self.workspace_space_swipe_source_frame = None;
        self.workspace_space_swipe_capture_source = true;
        self.workspace_space_swipe_push_active = false;
        self.workspace_space_swipe_direction = 0.0;
        self.workspace_space_swipe_pending_commit = None;
        self.workspace_space_swipe_needs_settle_start = false;
    }

    fn scroll_right_sidebar_note_table_at(&mut self, x: f32, y: f32, delta: f32) -> bool {
        let Some(layout) = self
            .right_sidebar_note_table_layouts
            .iter()
            .rev()
            .find(|layout| {
                x >= layout.x
                    && x < layout.x + layout.width
                    && y >= layout.y
                    && y < layout.y + layout.height
            })
            .copied()
        else {
            return false;
        };
        if layout.max_horizontal_scroll <= 0.0 {
            return false;
        }
        let offset = self
            .right_sidebar_note_table_horizontal_offsets
            .entry(layout.source_start)
            .or_default();
        let next = note_horizontal_scroll_offset(*offset, delta, layout.max_horizontal_scroll);
        let changed = (next - *offset).abs() > f32::EPSILON;
        *offset = next;
        changed
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
                    let vert_delta = self.sidebar_vertical_scroll_delta(event).unwrap_or(0.0);
                    let changed = match event.kind {
                        WMEK::VertWheel(_) => self.scroll_right_sidebar_file_preview_by(vert_delta),
                        WMEK::HorzWheel(amount) if amount != 0 => {
                            self.scroll_right_sidebar_file_preview_horizontal(amount)
                        }
                        WMEK::HorzWheel(_) => false,
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

        if self.right_sidebar_mode == super::RightSidebarMode::Tasks {
            let shift_vertical = matches!(event.kind, WMEK::VertWheel(_))
                && event.modifiers.contains(::window::Modifiers::SHIFT);
            if matches!(event.kind, WMEK::HorzWheel(_)) || shift_vertical {
                let delta = if shift_vertical {
                    self.sidebar_vertical_scroll_delta(event).unwrap_or(0.0)
                } else {
                    self.sidebar_horizontal_scroll_delta(event).unwrap_or(0.0)
                };
                if delta.abs() > f32::EPSILON {
                    let changed = self.scroll_right_sidebar_note_table_at(
                        event.coords.x as f32,
                        event.coords.y as f32,
                        delta,
                    ) || self.right_sidebar_note.scroll_code_block_at(
                        event.coords.x as f32,
                        event.coords.y as f32,
                        delta,
                    );
                    if changed {
                        context.invalidate();
                    }
                }
                return true;
            }
        }

        let delta = match event.kind {
            WMEK::VertWheel(_) => self.sidebar_vertical_scroll_delta(event).unwrap_or(0.0),
            WMEK::HorzWheel(_) => return true,
            _ => return false,
        };
        if delta.abs() <= f32::EPSILON {
            return true;
        }

        match self.right_sidebar_mode {
            super::RightSidebarMode::Chat => {
                if self.scroll_right_sidebar_files_by(delta) {
                    context.invalidate();
                }
            }
            super::RightSidebarMode::Tasks => {
                let over_tree = matches!(
                    self.resolve_ui_item(event).map(|item| item.item_type),
                    Some(UIItemType::RightSidebarNoteTreeRow(_))
                        | Some(UIItemType::RightSidebarNoteTreeBack)
                        | Some(UIItemType::RightSidebarNoteTreeToggle)
                ) || self.right_sidebar_note_view
                    == crate::termwindow::RightSidebarNoteView::Tree;
                let changed = if over_tree {
                    let old = self.right_sidebar_note_tree_scroll_offset;
                    self.right_sidebar_note_tree_scroll_offset =
                        (self.right_sidebar_note_tree_scroll_offset + delta).max(0.0);
                    (old - self.right_sidebar_note_tree_scroll_offset).abs() > f32::EPSILON
                } else {
                    self.right_sidebar_note.reveal_caret = false;
                    self.right_sidebar_note.scroll_by(delta)
                };
                if changed {
                    context.invalidate();
                }
            }
            super::RightSidebarMode::Snippets => {
                let was_visible = self.right_sidebar_snippet_scrollbar_visible_until;
                self.show_right_sidebar_snippet_scrollbar();
                let amount = match event.kind {
                    WMEK::VertWheel(amount) => amount,
                    _ => 0,
                };
                if self.scroll_right_sidebar_snippets(amount)
                    || was_visible != self.right_sidebar_snippet_scrollbar_visible_until
                {
                    context.invalidate();
                }
            }
            super::RightSidebarMode::Agents => {
                let old = self.right_sidebar_agents_scroll;
                self.right_sidebar_agents_scroll =
                    (self.right_sidebar_agents_scroll + delta).max(0.0);
                if (old - self.right_sidebar_agents_scroll).abs() > f32::EPSILON {
                    context.invalidate();
                }
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
            | UIItemType::SpaceReconnect
            | UIItemType::ProjectToggleThreads(_)
            | UIItemType::ThreadRefGroupToggle { .. }
            | UIItemType::ThreadRefGroupNewThread(_)
            | UIItemType::Project(_)
            | UIItemType::ArchivedProject(_)
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
            | UIItemType::WorkspaceSidebarThreadSearch
            | UIItemType::WorkspaceSidebarViewOptions
            | UIItemType::WorkspaceSidebarSshHosts
            | UIItemType::WorkspaceSidebarLiveOverview
            | UIItemType::WorkspaceSidebarNotifications
            | UIItemType::RightSidebarToggle
            | UIItemType::RightSidebarMode(_)
            | UIItemType::RightSidebarBackground
            | UIItemType::RightSidebarResize
            | UIItemType::RightSidebarFilePreviewResize
            | UIItemType::RightSidebarNotePaneResize
            | UIItemType::RightSidebarNotePaneToggle
            | UIItemType::RightSidebarAgent(_)
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
            | UIItemType::RightSidebarNoteMenu
            | UIItemType::RightSidebarNoteChooseVault
            | UIItemType::RightSidebarNoteCreateVault
            | UIItemType::RightSidebarNoteRetry
            | UIItemType::RightSidebarNoteTreeToggle
            | UIItemType::RightSidebarNoteTreeBack
            | UIItemType::RightSidebarNoteTreeRow(_)
            | UIItemType::RightSidebarNoteCodeToggle(_)
            | UIItemType::RightSidebarNoteCodeCopy(_)
            | UIItemType::RightSidebarNoteBody
            | UIItemType::RightSidebarFilePreviewScrollTrack
            | UIItemType::RightSidebarFilePreviewScrollThumb
            | UIItemType::RightSidebarFilePreviewHorizontalScrollTrack
            | UIItemType::RightSidebarFilePreviewHorizontalScrollThumb
            | UIItemType::RightSidebarFilePreviewText
            | UIItemType::RightSidebarFileFilter
            | UIItemType::RightSidebarFileRefresh
            | UIItemType::RightSidebarFileRow(_)
            | UIItemType::RightSidebarFileBack
            | UIItemType::RightSidebarFileOpen
            | UIItemType::RightSidebarFileOpenMenu
            | UIItemType::RightSidebarFileReveal
            | UIItemType::RightSidebarFileCopyText
            | UIItemType::RightSidebarRemoteFileConnect
            | UIItemType::RightSidebarRemoteFileRefresh
            | UIItemType::RightSidebarRemoteFileRow(_)
            | UIItemType::RightSidebarRemoteFileBack
            | UIItemType::RightSidebarRemoteFileCopyText
            | UIItemType::RightSidebarRemoteTransfer(_)
            | UIItemType::ContextMenuBackdrop
            | UIItemType::ContextMenuItem(_)
            | UIItemType::CommandPalette
            | UIItemType::AboveScrollThumb
            | UIItemType::BelowScrollThumb
            | UIItemType::ScrollThumb
            | UIItemType::Split(_)
            | UIItemType::ContentViewClose(_) => {}
        }
    }

    /// Track which tagged item the pointer is resting on.
    ///
    /// Keyed by item *type*, not by the UIItem: the sidebar rebuilds its items
    /// every frame and a button's rect can shift by a pixel, which must not
    /// restart the delay and leave the tag forever one frame away.
    ///
    /// Only records when the pointer arrived. The wakeup that makes the tag
    /// appear is registered by `paint_hover_tooltip`, because `paint_impl`
    /// clears `has_animation` on entry and would discard a deadline set here.
    fn update_hover_tooltip(&mut self, item: Option<&UIItem>) {
        let labelled = item
            .filter(|item| crate::termwindow::tooltip_label_for(&item.item_type).is_some())
            // The dynamic half of the rule: no tag over a live rename
            // editor, and none for a file row whose label already reads in
            // full. `paint_hover_tooltip` re-checks the same predicate.
            .filter(|item| self.hover_tooltip_allowed(&item.item_type));
        match labelled {
            None => self.hover_tooltip = None,
            Some(item) => match self.hover_tooltip.as_mut() {
                Some(hover) if hover.item.item_type == item.item_type => {
                    hover.item = item.clone();
                }
                _ => {
                    self.hover_tooltip = Some(crate::termwindow::HoverTooltip {
                        item: item.clone(),
                        since: Instant::now(),
                    });
                }
            },
        }
    }

    fn enter_ui_item(&mut self, item: &UIItem) {
        match item.item_type {
            UIItemType::TabBar(_) => {}
            UIItemType::CloseTab(_)
            | UIItemType::PaneNav { .. }
            | UIItemType::ProjectNew
            | UIItemType::SpaceMenu
            | UIItemType::SpaceReconnect
            | UIItemType::ProjectToggleThreads(_)
            | UIItemType::ThreadRefGroupToggle { .. }
            | UIItemType::ThreadRefGroupNewThread(_)
            | UIItemType::Project(_)
            | UIItemType::ArchivedProject(_)
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
            | UIItemType::WorkspaceSidebarThreadSearch
            | UIItemType::WorkspaceSidebarViewOptions
            | UIItemType::WorkspaceSidebarSshHosts
            | UIItemType::WorkspaceSidebarLiveOverview
            | UIItemType::WorkspaceSidebarNotifications
            | UIItemType::RightSidebarToggle
            | UIItemType::RightSidebarMode(_)
            | UIItemType::RightSidebarBackground
            | UIItemType::RightSidebarResize
            | UIItemType::RightSidebarFilePreviewResize
            | UIItemType::RightSidebarNotePaneResize
            | UIItemType::RightSidebarNotePaneToggle
            | UIItemType::RightSidebarAgent(_)
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
            | UIItemType::RightSidebarNoteMenu
            | UIItemType::RightSidebarNoteChooseVault
            | UIItemType::RightSidebarNoteCreateVault
            | UIItemType::RightSidebarNoteRetry
            | UIItemType::RightSidebarNoteTreeToggle
            | UIItemType::RightSidebarNoteTreeBack
            | UIItemType::RightSidebarNoteTreeRow(_)
            | UIItemType::RightSidebarNoteCodeToggle(_)
            | UIItemType::RightSidebarNoteCodeCopy(_)
            | UIItemType::RightSidebarNoteBody
            | UIItemType::RightSidebarFilePreviewScrollTrack
            | UIItemType::RightSidebarFilePreviewScrollThumb
            | UIItemType::RightSidebarFilePreviewHorizontalScrollTrack
            | UIItemType::RightSidebarFilePreviewHorizontalScrollThumb
            | UIItemType::RightSidebarFilePreviewText
            | UIItemType::RightSidebarFileFilter
            | UIItemType::RightSidebarFileRefresh
            | UIItemType::RightSidebarFileRow(_)
            | UIItemType::RightSidebarFileBack
            | UIItemType::RightSidebarFileOpen
            | UIItemType::RightSidebarFileOpenMenu
            | UIItemType::RightSidebarFileReveal
            | UIItemType::RightSidebarFileCopyText
            | UIItemType::RightSidebarRemoteFileConnect
            | UIItemType::RightSidebarRemoteFileRefresh
            | UIItemType::RightSidebarRemoteFileRow(_)
            | UIItemType::RightSidebarRemoteFileBack
            | UIItemType::RightSidebarRemoteFileCopyText
            | UIItemType::RightSidebarRemoteTransfer(_)
            | UIItemType::ContextMenuBackdrop
            | UIItemType::ContextMenuItem(_)
            | UIItemType::CommandPalette
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
        let pane_nav_height = self.pane_nav_bar_height() as isize;

        let local_x = event.coords.x.sub(pane_left);
        // A smooth scroll draws the top row cut off by `viewport_px`, so
        // the row under the pointer is that much further down the grid.
        let scroll_px = self.get_viewport_px(pane.pane_id()).round() as isize;
        let local_y = event.coords.y.sub(pane_top + pane_nav_height) + scroll_px;

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

    /// Release the pointer-ownership bookkeeping for a `Release` that is
    /// about to be consumed by an early return.
    ///
    /// `mouse_event_impl` normally clears these in its `Release` arm, but
    /// three paths return before reaching it: the suppressed release that
    /// follows a menu choice, any release delivered while a menu is open, and
    /// any release that lands while a full-window view is transitioning.
    /// Suppressing the *action* is intended; forgetting that the button
    /// physically came up is not. Left armed, `current_mouse_capture` and
    /// `current_mouse_buttons` stay set for the rest of the session and
    /// silently veto every guard that tests them -- which disabled the
    /// sidebar space swipe permanently after any context-menu use.
    fn release_pointer_ownership(&mut self, event: &MouseEvent) {
        if let WMEK::Release(ref press) = event.kind {
            self.current_mouse_capture = None;
            self.current_mouse_buttons.retain(|p| p != press);
        }
    }

    /// Same bookkeeping, for the case where the release will never arrive at
    /// all: a native AppKit menu tracks in a nested event loop that consumes
    /// the mouse-up outright. See the call site in `show_term_context_menu`.
    pub(crate) fn release_pointer_ownership_for_native_menu(&mut self) {
        self.current_mouse_capture = None;
        self.current_mouse_buttons.clear();
    }

    pub fn mouse_event_impl(&mut self, event: MouseEvent, context: &dyn WindowOps) {
        log::trace!("{:?}", event);
        // See `key_event_impl`: nothing on screen during a transition is live,
        // including the card layout a click would be aiming at, which is still
        // moving.
        if self.content_view_transition_running() {
            // Finger-driven wheels are exempt from the hold: a scroll
            // cannot "click the moving card", and the hold lasts up to a
            // full second — long enough that swipes made right after
            // closing a page were silently discarded and scrolling felt
            // dead. Momentum events are NOT exempt: they belong to a
            // gesture that started on the page being left, and letting
            // them through would scroll whatever lands underneath.
            if !matches!(event.kind, WMEK::VertWheel(_) | WMEK::HorzWheel(_))
                || event.momentum_phase.is_some()
            {
                // Suppressing the action is the point; forgetting that the button
                // physically came up is not. A press held across the start of a
                // transition -- holding a card and hitting Escape, say -- would
                // otherwise leave the pointer bookkeeping armed, and every guard
                // that tests it reads as if the button were still down.
                self.release_pointer_ownership(&event);
                return;
            }
        }
        let pane = self.get_active_pane_or_overlay();

        if self.frontend_handoff_consumed_press && matches!(event.kind, WMEK::Release(_)) {
            self.frontend_handoff_consumed_press = false;
            return;
        }

        // Hit-test first. Tab bar, sidebars, menus and OS chrome stay usable
        // without silently taking a terminal away from another device.
        let area = self.content_view_area();
        let point_is_in_terminal_area = !self.content_view_foreground()
            && (event.coords.x as f32) >= area.min_x()
            && (event.coords.x as f32) < area.max_x()
            && (event.coords.y as f32) >= area.min_y()
            && (event.coords.y as f32) < area.max_y();
        let terminal_ui_item = self.resolve_ui_item(&event).is_some_and(|item| {
            matches!(
                item.item_type,
                UIItemType::PaneNav { .. }
                    | UIItemType::AboveScrollThumb
                    | UIItemType::ScrollThumb
                    | UIItemType::BelowScrollThumb
                    | UIItemType::Split(_)
            )
        });
        let terminal_surface = point_is_in_terminal_area
            && (self.resolve_ui_item(&event).is_none() || terminal_ui_item);
        let wheel_has_motion = match event.kind {
            WMEK::VertWheel(amount) | WMEK::HorzWheel(amount) => {
                amount != 0
                    || event.precise_scroll_delta.is_some_and(|delta| {
                        delta.x.abs() > f32::EPSILON || delta.y.abs() > f32::EPSILON
                    })
            }
            _ => false,
        };
        let takeover_gesture = matches!(event.kind, WMEK::Press(_)) || wheel_has_motion;

        if terminal_surface && self.frontend_surface_blocked() {
            if takeover_gesture && self.frontend_takeover_claimable() {
                self.frontend_handoff_consumed_press = matches!(event.kind, WMEK::Press(_));
                self.claim_frontend_viewport_for_interaction();
                context.invalidate();
            }
            // The opaque handoff surface consumes the triggering gesture and
            // all pointer traffic while blocked; nothing leaks to the pane.
            return;
        }
        if terminal_surface && takeover_gesture {
            // In shared mode this updates the per-tab layout owner. The event
            // itself may continue; ClientPane serializes forwarded input behind
            // the same geometry-bearing claim.
            self.claim_frontend_viewport_for_interaction();
        }

        self.current_mouse_event.replace(event.clone());
        // Not while the command palette's scrim covers the window: the
        // hover-reveal machinery would slide the sidebar out underneath it.
        if self.command_palette.is_none() {
            self.update_workspace_sidebar_hover(context);
        }

        if self.consume_context_menu_suppressed_release(&event) {
            self.release_pointer_ownership(&event);
            return;
        }

        // The command palette owns all pointer traffic while open. Dispatched
        // before the pane-dependent context-menu branch so it also works when
        // there is no active pane (content view foreground).
        if self.mouse_event_command_palette(&event, context) {
            self.release_pointer_ownership(&event);
            return;
        }

        if let Some(pane) = pane.as_ref() {
            if self.mouse_event_context_menu(&event, pane, context) {
                self.release_pointer_ownership(&event);
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

        if self.mouse_wheel_right_sidebar(&event, context)
            || self.mouse_wheel_workspace_sidebar(&event, context)
            || self.mouse_wheel_tab_surfaces(&event, context)
        {
            // The wheel moved content under a stationary pointer, so an
            // armed hover tag now names whatever row USED to be under it.
            // These early returns never reach `update_hover_tooltip`, so the
            // stale tag would otherwise fire over the wrong row once its
            // delay elapses. It re-arms on the next real pointer move.
            self.hover_tooltip = None;
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
                    if completed_drag.as_ref().is_some_and(|(item, _)| {
                        item.item_type == UIItemType::RightSidebarNotePaneResize
                            || (item.item_type == UIItemType::RightSidebarResize
                                && self.right_sidebar_note_pane_rect().is_some())
                    }) {
                        self.persist_right_sidebar_note_pane_width();
                    }
                    if completed_drag
                        .as_ref()
                        .is_some_and(|(item, _)| matches!(item.item_type, UIItemType::Split(_)))
                    {
                        self.finish_remote_split_drag();
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
                    if completed_drag
                        .as_ref()
                        .is_some_and(|(item, _)| item.item_type == UIItemType::RightSidebarNoteBody)
                    {
                        if self.right_sidebar_note.drag_selection_active {
                            self.update_right_sidebar_note_drag_selection(
                                event.coords.x,
                                event.coords.y,
                            );
                        }
                        self.right_sidebar_note.drag_selection_active = false;
                        self.right_sidebar_note.drag_selection_base = None;
                        context.invalidate();
                    }
                    if completed_drag.as_ref().is_some_and(|(item, _)| {
                        matches!(
                            item.item_type,
                            UIItemType::RightSidebarFileRow(_)
                                | UIItemType::RightSidebarRemoteFileRow(_)
                        )
                    }) {
                        if let Some(state) = self.right_sidebar_file_drag.take() {
                            if state.active {
                                self.drop_right_sidebar_file_drag(state, &event, x, y);
                                context.invalidate();
                            } else {
                                // Never crossed the drag threshold: this was a
                                // plain click, so open the file (moved here
                                // from the press handler). Opening a preview
                                // widens the sidebar, so reflow like the old
                                // press path did or the preview overlaps the
                                // terminal.
                                let previous_width = self.right_sidebar_width();
                                match state.payload {
                                    super::FileDragPayload::Local(path) => {
                                        self.open_right_sidebar_file_path(path)
                                    }
                                    super::FileDragPayload::Remote { path, origin } => {
                                        if self.remote_operation_origin_matches(&origin) {
                                            self.open_right_sidebar_remote_file(path)
                                        }
                                    }
                                }
                                self.invalidate_or_reflow_right_sidebar(previous_width, context);
                            }
                        }
                        return;
                    }
                    if completed_drag.as_ref().is_some_and(|(item, _)| {
                        matches!(
                            item.item_type,
                            UIItemType::Project(_)
                                | UIItemType::WorkspaceThread(_)
                                | UIItemType::ThreadRefGroupToggle { .. }
                        )
                    }) {
                        // Release coordinates can differ from the last move
                        // event; recompute the drop gap from where the button
                        // actually went up.
                        if self
                            .sidebar_row_drag
                            .as_ref()
                            .is_some_and(|state| state.active)
                        {
                            self.update_sidebar_row_drag_target(event.coords);
                        }
                        if let Some(state) = self.sidebar_row_drag.take() {
                            if state.active {
                                self.drop_sidebar_row_drag(state, context);
                            } else {
                                // Never crossed the drag threshold: this was
                                // a plain click (moved here from the press
                                // handler).
                                match state.kind {
                                    super::SidebarRowKind::Project(project_id) => {
                                        crate::workspace_threads::toggle_project_threads_collapsed(
                                            &project_id,
                                        );
                                    }
                                    super::SidebarRowKind::RefGroup(raw_key) => {
                                        let collapse_key =
                                            format!("{}::{raw_key}", self.active_space_id);
                                        if !self.thread_ref_groups_collapsed.remove(&collapse_key) {
                                            self.thread_ref_groups_collapsed.insert(collapse_key);
                                        }
                                    }
                                    super::SidebarRowKind::Thread { thread_id, .. } => {
                                        if !self.open_remote_workspace_thread_without_connecting(
                                            &thread_id, context,
                                        ) {
                                            self.activate_workspace_thread(thread_id, context);
                                        }
                                    }
                                }
                            }
                            context.invalidate();
                        }
                        return;
                    }
                    if completed_drag.as_ref().is_some_and(|(item, _)| {
                        matches!(item.item_type, UIItemType::PaneNav { .. })
                    }) {
                        // Release coordinates can differ from the last move
                        // event (coalesced/fast motion); recompute the drop
                        // target from where the button actually went up.
                        if self
                            .pane_tab_drag
                            .as_ref()
                            .is_some_and(|state| state.active)
                        {
                            self.update_pane_tab_drag_target(&event);
                        }
                        if let Some(state) = self.pane_tab_drag.take() {
                            if state.active {
                                self.drop_pane_tab_drag(state);
                            }
                            // A plain click (never crossed the threshold)
                            // was already handled on press.
                            context.invalidate();
                        }
                        return;
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

        // Also clears while a drag holds the capture: a tag hovering next to
        // the pointer mid-drag is noise.
        self.update_hover_tooltip(ui_item.as_ref());

        // A press anywhere other than the row being renamed commits a pending
        // inline rename (Enter semantics); otherwise the editor stays armed
        // and keeps swallowing keyboard input after the user clicked away.
        if matches!(event.kind, WMEK::Press(_))
            && self.inline_tab_rename.is_some()
            && !self.ui_item_hosts_inline_rename(ui_item.as_ref().map(|item| &item.item_type))
        {
            self.finish_inline_tab_rename(true);
            context.invalidate();
        }

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
                let resp = self.active_content_view_mut().map(|v| {
                    if matches!(event.kind, WMEK::VertWheel(_) | WMEK::HorzWheel(_)) {
                        // Pixel deltas and scroll phases do not survive the
                        // trip through `MouseEventKind`; hand the view the
                        // whole event so it can scroll like a trackpad.
                        v.on_wheel(px, py, &event)
                    } else {
                        v.on_mouse(px, py, event.kind)
                    }
                });
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
        // The machine reads `Away` now: cancels an Arming, starts a grace.
        self.update_workspace_sidebar_hover(context);
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
            // Split of the per-event drag cost: the pre-c9e3300 path was
            // resize_split_by alone; preview_active_tab_geometry_now is the
            // frontend-geometry machinery added since. Which of the two the
            // time goes to decides where the fast-path cut lands.
            let event_started = crate::perf::now();
            let split_started = crate::perf::now();
            tab.resize_split_by(split.index, delta);
            crate::perf::log_duration("drag_resize_split_by", split_started);
            let preview_started = crate::perf::now();
            self.preview_active_tab_geometry_now();
            crate::perf::log_duration("drag_preview_geometry", preview_started);
            if let Some(split) = tab.iter_splits().into_iter().nth(split.index) {
                item.item_type = UIItemType::Split(split);
                context.invalidate();
            }
            crate::perf::log_duration("drag_split_event", event_started);
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

    fn drag_right_sidebar_note_selection(
        &mut self,
        item: UIItem,
        start_event: MouseEvent,
        event: MouseEvent,
        context: &dyn WindowOps,
    ) {
        self.right_sidebar_note.drag_selection_active = true;
        self.update_right_sidebar_note_drag_selection(event.coords.x, event.coords.y);
        if self
            .right_sidebar_note_drag_scroll_delta(&item, &event)
            .is_some()
        {
            self.schedule_right_sidebar_note_drag_autoscroll();
        }
        context.set_cursor(Some(MouseCursor::Text));
        context.invalidate();
        self.dragging.replace((item, start_event));
    }

    fn update_right_sidebar_note_drag_selection(&mut self, x: isize, y: isize) {
        let position = self
            .right_sidebar_note
            .source_position_for_point(x as f32, y as f32);
        self.right_sidebar_note.extend_selection_to(position);
        self.right_sidebar_note.reveal_caret = false;
        self.right_sidebar_note.refresh_projection();
    }

    fn right_sidebar_note_drag_scroll_delta(
        &self,
        item: &UIItem,
        event: &MouseEvent,
    ) -> Option<f32> {
        let top = item.y as f32;
        let bottom = item.y.saturating_add(item.height) as f32;
        note_drag_scroll_delta(
            event.coords.y as f32,
            top,
            bottom,
            self.ui_f32(40.0),
            self.ui_f32(28.0),
        )
    }

    fn schedule_right_sidebar_note_drag_autoscroll(&mut self) {
        if self.right_sidebar_note.drag_autoscroll_scheduled {
            return;
        }
        let Some(window) = self.window.as_ref().cloned() else {
            return;
        };
        self.right_sidebar_note.drag_autoscroll_scheduled = true;
        promise::spawn::spawn(async move {
            smol::Timer::after(Duration::from_millis(32)).await;
            window.notify(TermWindowNotif::Apply(Box::new(|term_window| {
                term_window.right_sidebar_note.drag_autoscroll_scheduled = false;
                if term_window.step_right_sidebar_note_drag_autoscroll() {
                    term_window.invalidate_window();
                    term_window.schedule_right_sidebar_note_drag_autoscroll();
                }
            })));
        })
        .detach();
    }

    fn step_right_sidebar_note_drag_autoscroll(&mut self) -> bool {
        if !self.right_sidebar_note.drag_selection_active {
            return false;
        }
        let Some((item, _)) = self.dragging.as_ref() else {
            return false;
        };
        if item.item_type != UIItemType::RightSidebarNoteBody {
            return false;
        }
        let Some(event) = self.current_mouse_event.clone() else {
            return false;
        };
        let Some(delta) = self.right_sidebar_note_drag_scroll_delta(item, &event) else {
            return false;
        };
        let changed = self.right_sidebar_note.scroll_by(delta);
        if changed {
            self.update_right_sidebar_note_drag_selection(event.coords.x, event.coords.y);
        }
        changed
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
            UIItemType::RightSidebarNotePaneResize => {
                self.drag_right_sidebar_note_pane_resize(item, start_event, event, context);
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
            UIItemType::RightSidebarNoteBody => {
                self.drag_right_sidebar_note_selection(item, start_event, event, context);
            }
            UIItemType::RightSidebarFileFilter
            | UIItemType::RightSidebarSnippetSearch
            | UIItemType::RightSidebarSnippetTitle => {
                self.drag_right_sidebar_input_selection(item, start_event, event, context);
            }
            UIItemType::WorkspaceSidebarScrollThumb => {
                self.drag_workspace_sidebar_scroll_thumb(item, start_event, event, context);
            }
            UIItemType::ContextMenuBackdrop
            | UIItemType::ContextMenuItem(_)
            | UIItemType::CommandPalette => {}
            UIItemType::PaneNav { .. } => {
                self.drag_pane_nav_tab(item, start_event, event, context);
            }
            UIItemType::Project(_)
            | UIItemType::WorkspaceThread(_)
            | UIItemType::ThreadRefGroupToggle { .. } => {
                self.drag_sidebar_row(item, start_event, event, context);
            }
            UIItemType::RightSidebarFileRow(ref path) => {
                if self.is_renaming_sidebar_file(path) {
                    self.drag_right_sidebar_input_selection(item, start_event, event, context);
                } else {
                    self.drag_right_sidebar_file_row(item, start_event, event, context);
                }
            }
            UIItemType::RightSidebarRemoteFileRow(ref path) => {
                if self.is_renaming_sidebar_remote_file(path) {
                    self.drag_right_sidebar_input_selection(item, start_event, event, context);
                } else {
                    self.drag_right_sidebar_file_row(item, start_event, event, context);
                }
            }
            _ => {
                log::error!("drag not implemented for {:?}", item);
            }
        }
    }

    fn drag_right_sidebar_file_row(
        &mut self,
        item: UIItem,
        start_event: MouseEvent,
        event: MouseEvent,
        context: &dyn WindowOps,
    ) {
        if let Some(state) = self.right_sidebar_file_drag.as_mut() {
            if !state.active {
                let dx = event.coords.x - state.start.x;
                let dy = event.coords.y - state.start.y;
                // ~5px of travel turns the pending click into a drag
                if dx * dx + dy * dy >= 25 {
                    state.active = true;
                }
            }
            state.current = event.coords;
            if state.active {
                context.set_cursor(Some(MouseCursor::Hand));
                context.invalidate();
            }
        }
        // drag_ui_item takes `dragging` on every move; keep the drag armed
        self.dragging.replace((item, start_event));
    }

    fn drop_right_sidebar_file_drag(
        &mut self,
        state: super::FileDragState,
        event: &MouseEvent,
        column: usize,
        row: i64,
    ) {
        // Only a drop over bare terminal ground counts: any UI chrome under
        // the pointer (sidebars, tab bar, splits) or an active content view
        // cancels silently.
        if self.resolve_ui_item(event).is_some() || self.content_view_foreground() {
            return;
        }
        let px = event.coords.x as f32;
        let py = event.coords.y as f32;
        let area = self.content_view_area();
        if px < area.min_x() || px >= area.max_x() || py < area.min_y() || py >= area.max_y() {
            return;
        }

        // Paste into the pane under the drop point (splits!), falling back
        // to the active pane. Focus is deliberately left unchanged.
        let mut target = None;
        for pos in self.get_panes_to_render() {
            if row >= pos.top as i64
                && row <= (pos.top + pos.height) as i64
                && column >= pos.left
                && column <= pos.left + pos.width
            {
                target = Some(pos.pane);
                break;
            }
        }
        let Some(pane) = target.or_else(|| self.get_active_pane_or_overlay()) else {
            return;
        };

        match state.payload {
            super::FileDragPayload::Local(path) => {
                let mut text = self
                    .config
                    .quote_dropped_files
                    .escape(&path.to_string_lossy());
                text.push(' ');
                if let Err(err) = pane.send_paste(&text) {
                    log::error!("failed to paste dropped file path: {err:#}");
                }
            }
            super::FileDragPayload::Remote { path, origin } => {
                // The tree this path was picked from has to still be the tree
                // on screen: the same absolute path on another host names a
                // different object, and a drag that outlived the switch has no
                // way to know that. Cancelling silently matches a drop onto
                // any other piece of chrome.
                if !self.remote_operation_origin_matches_pane(&origin, pane.pane_id()) {
                    return;
                }
                // POSIX-quoted regardless of `quote_dropped_files` — that
                // setting is about the local shell, and this path is on a
                // server by construction.
                self.paste_remote_path_to_pane(pane.pane_id(), &path);
            }
        }
    }

    /// Whether a pane can participate in a drag move/split. Remote mux
    /// panes are supported by ClientDomain's translated MovePane path;
    /// tmux panes remain excluded because that domain ignores
    /// SplitSource::MovePane and would spawn instead.
    fn pane_tab_is_movable(pane: &Arc<dyn Pane>) -> bool {
        if let Some(domain) = Mux::get().get_domain(pane.domain_id()) {
            if domain.downcast_ref::<mux::tmux::TmuxDomain>().is_some() {
                return false;
            }
        }
        true
    }

    /// Prime a level-2 pane tab for a potential move/split drag. It only
    /// becomes a real drag once the pointer travels past the threshold, so
    /// plain clicks keep their press-time activation behavior.
    fn arm_sidebar_row_drag(
        &mut self,
        item: UIItem,
        kind: super::SidebarRowKind,
        title: String,
        draggable: bool,
        event: MouseEvent,
    ) {
        self.sidebar_row_drag = Some(super::SidebarRowDragState {
            kind,
            title,
            start: event.coords,
            current: event.coords,
            active: false,
            draggable,
            autoscroll_scheduled: false,
            target: None,
        });
        self.dragging.replace((item, event));
    }

    fn drag_sidebar_row(
        &mut self,
        item: UIItem,
        start_event: MouseEvent,
        event: MouseEvent,
        context: &dyn WindowOps,
    ) {
        let mut dragging_active = false;
        if let Some(state) = self.sidebar_row_drag.as_mut() {
            if state.draggable && !state.active {
                let dx = event.coords.x - state.start.x;
                let dy = event.coords.y - state.start.y;
                // ~5px of travel turns the pending click into a drag
                if dx * dx + dy * dy >= 25 {
                    state.active = true;
                }
            }
            state.current = event.coords;
            dragging_active = state.active;
        }
        if dragging_active {
            self.autoscroll_sidebar_for_row_drag(event.coords.y);
            self.update_sidebar_row_drag_target(event.coords);
            context.set_cursor(Some(MouseCursor::Hand));
            context.invalidate();
        }
        // drag_ui_item takes `dragging` on every move; keep the drag armed
        self.dragging.replace((item, start_event));
    }

    /// Dragging near the list's top or bottom edge scrolls it so off-screen
    /// rows can be reached. One call applies one step; a stationary pointer
    /// produces no further move events, so each applied step queues a timer
    /// tick that repeats the check from the last known pointer position
    /// until the pointer leaves the hot zone, the scroll hits its limit, or
    /// the drag ends.
    fn autoscroll_sidebar_for_row_drag(&mut self, pointer_y: isize) {
        // The list viewport proper: the panel background also covers the
        // toolbar and footer (and a header strip when there is a top
        // border), which is exactly the wrong area to hot-zone against.
        let Some((top, bottom)) = self.workspace_sidebar_list_viewport() else {
            return;
        };
        // Roughly a row's worth of hot zone, with a floor so it stays
        // usable at scale factors that shrink ui_px results.
        let hot = (self.ui_px(32) as isize).max(24);
        let step = (self.ui_px(16) as f32).max(8.0);
        let delta = if pointer_y < top + hot {
            -step
        } else if pointer_y > bottom - hot {
            step
        } else {
            return;
        };
        let max = self.workspace_sidebar_scroll_max();
        let next = (self.workspace_sidebar_scroll_offset + delta).clamp(0.0, max);
        if (next - self.workspace_sidebar_scroll_offset).abs() < f32::EPSILON {
            // Already at the end this direction scrolls toward: stop the
            // timer chain instead of ticking forever.
            return;
        }
        self.workspace_sidebar_scroll_offset = next;
        self.schedule_sidebar_drag_autoscroll_tick();
    }

    fn schedule_sidebar_drag_autoscroll_tick(&mut self) {
        let Some(state) = self.sidebar_row_drag.as_mut() else {
            return;
        };
        if state.autoscroll_scheduled {
            return;
        }
        state.autoscroll_scheduled = true;
        let Some(window) = self.window.as_ref().cloned() else {
            return;
        };
        promise::spawn::spawn(async move {
            smol::Timer::after(Duration::from_millis(40)).await;
            window.notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                let (pointer, active) = match term_window.sidebar_row_drag.as_mut() {
                    Some(state) => {
                        state.autoscroll_scheduled = false;
                        (state.current, state.active)
                    }
                    None => return,
                };
                if !active {
                    return;
                }
                term_window.autoscroll_sidebar_for_row_drag(pointer.y);
                term_window.update_sidebar_row_drag_target(pointer);
                term_window.invalidate_window();
            })));
        })
        .detach();
    }

    /// Recompute which gap the drag would drop into. The candidate rows are
    /// this frame's rendered rects: same-Space Project rows for a project
    /// drag, the same project's (unpinned) thread rows for a thread drag —
    /// the pinned section and other projects' threads are not targets.
    fn update_sidebar_row_drag_target(&mut self, coords: ::window::Point) {
        let Some(kind) = self
            .sidebar_row_drag
            .as_ref()
            .map(|state| state.kind.clone())
        else {
            return;
        };
        let sidebar_x_ok = self
            .ui_items
            .iter()
            .find(|candidate| candidate.item_type == UIItemType::WorkspaceSidebarBackground)
            .is_some_and(|bg| coords.x >= bg.x as isize && coords.x < (bg.x + bg.width) as isize);
        let rows: Vec<(String, isize, isize)> = match &kind {
            super::SidebarRowKind::Project(_) | super::SidebarRowKind::RefGroup(_) => {
                folder_block_extents(&self.ui_items, &self.active_space_id)
            }
            super::SidebarRowKind::Thread {
                thread_id,
                project_id,
            } => {
                // A dragged reference row reorders within the Space's ref
                // list, not within the origin project's threads.
                let siblings = if self.sidebar_row_is_thread_ref(thread_id) {
                    crate::workspace_threads::space_thread_refs(&self.active_space_id)
                } else {
                    crate::workspace_threads::unpinned_thread_ids(project_id)
                };
                self.ui_items
                    .iter()
                    .filter_map(|candidate| match &candidate.item_type {
                        UIItemType::WorkspaceThread(id) if siblings.contains(id) => Some((
                            id.clone(),
                            candidate.y as isize,
                            (candidate.y + candidate.height) as isize,
                        )),
                        _ => None,
                    })
                    .collect()
            }
        };
        // A thread pointer that leaves its own list's vertical extent (plus
        // one row of slack) is not aiming at any of its gaps: dropping there
        // cancels instead of snapping to the nearest end, which is what
        // keeps a thread dragged onto ANOTHER project a no-op. A project
        // has no foreign list to stray into — the whole sidebar is its drop
        // zone, so anywhere below the last block simply means "the end".
        let within_span = match &kind {
            super::SidebarRowKind::Project(_) | super::SidebarRowKind::RefGroup(_) => {
                !rows.is_empty()
            }
            super::SidebarRowKind::Thread { .. } => {
                rows.first().zip(rows.last()).is_some_and(|(first, last)| {
                    let slack = (first.2 - first.1).max(0);
                    coords.y >= first.1 - slack && coords.y <= last.2 + slack
                })
            }
        };
        let target = if sidebar_x_ok && within_span {
            sidebar_insert_position(&rows, coords.y).map(|(before, line_y)| {
                // The rendered rows are only the VISIBLE ones: "after the
                // last visible row" is the end of the viewport, not of the
                // list. Anchor to the next logical sibling instead, so a
                // scrolled-away tail keeps its place; None remains reserved
                // for the true end.
                let logical = match &kind {
                    super::SidebarRowKind::Project(_) | super::SidebarRowKind::RefGroup(_) => {
                        crate::workspace_threads::ordered_sidebar_folder_keys(&self.active_space_id)
                    }
                    super::SidebarRowKind::Thread {
                        thread_id,
                        project_id,
                    } => {
                        if self.sidebar_row_is_thread_ref(thread_id) {
                            crate::workspace_threads::space_thread_refs(&self.active_space_id)
                        } else {
                            crate::workspace_threads::unpinned_thread_ids(project_id)
                        }
                    }
                };
                let before = clamp_end_anchor_to_next_logical_sibling(
                    before,
                    rows.last().map(|(id, _, _)| id.as_str()),
                    &logical,
                );
                super::SidebarInsertTarget { before, line_y }
            })
        } else {
            None
        };
        if let Some(state) = self.sidebar_row_drag.as_mut() {
            state.target = target;
        }
    }

    fn drop_sidebar_row_drag(
        &mut self,
        state: super::SidebarRowDragState,
        context: &dyn WindowOps,
    ) {
        let Some(target) = state.target else {
            return;
        };
        let changed = match &state.kind {
            super::SidebarRowKind::Project(project_id) => {
                let space_id = self.active_space_id.clone();
                // Local Spaces order projects and ref groups in one merged
                // list; move_sidebar_folder_before refuses domain-owned
                // Spaces, which keep the server-synced project path.
                crate::workspace_threads::move_sidebar_folder_before(
                    &space_id,
                    project_id,
                    target.before.as_deref(),
                ) || crate::workspace_threads::move_project_before(
                    &space_id,
                    project_id,
                    target.before.as_deref(),
                )
            }
            super::SidebarRowKind::RefGroup(raw_key) => {
                let space_id = self.active_space_id.clone();
                crate::workspace_threads::move_sidebar_folder_before(
                    &space_id,
                    raw_key,
                    target.before.as_deref(),
                )
            }
            super::SidebarRowKind::Thread {
                thread_id,
                project_id,
            } => {
                let space_id = self.active_space_id.clone();
                if self.sidebar_row_is_thread_ref(thread_id) {
                    // Reference order is purely local; nothing travels to
                    // any server.
                    crate::workspace_threads::move_thread_ref_before(
                        &space_id,
                        thread_id,
                        target.before.as_deref(),
                    )
                } else {
                    crate::workspace_threads::move_thread_before(
                        project_id,
                        thread_id,
                        target.before.as_deref(),
                    )
                }
            }
        };
        if changed {
            context.invalidate();
        }
    }

    fn arm_pane_tab_drag(&mut self, item: UIItem, pane_id: mux::pane::PaneId, event: MouseEvent) {
        let mux = Mux::get();
        let Some(tab) = mux.get_active_tab_for_window(self.mux_window_id) else {
            return;
        };
        // A zoomed tab shows a single pane: there is nowhere to drop.
        if tab.get_zoomed_pane().is_some() {
            return;
        }
        if tab
            .pane_stack_id(pane_id)
            .is_some_and(|stack_id| self.collapsed_pane_layouts.contains_key(&stack_id))
        {
            return;
        }
        let Some(pane) = mux.get_pane(pane_id) else {
            return;
        };
        if !Self::pane_tab_is_movable(&pane) {
            return;
        }
        let title = self.pane_nav_tab_title(pane_id, &pane.get_title());
        self.pane_tab_drag = Some(PaneTabDragState {
            pane_id,
            title,
            start: event.coords,
            current: event.coords,
            active: false,
            target: None,
        });
        self.dragging.replace((item, event));
    }

    fn drag_pane_nav_tab(
        &mut self,
        item: UIItem,
        start_event: MouseEvent,
        event: MouseEvent,
        context: &dyn WindowOps,
    ) {
        let mut dragging_active = false;
        if let Some(state) = self.pane_tab_drag.as_mut() {
            if !state.active {
                let dx = event.coords.x - state.start.x;
                let dy = event.coords.y - state.start.y;
                // ~5px of travel turns the pending click into a drag
                if dx * dx + dy * dy >= 25 {
                    state.active = true;
                }
            }
            state.current = event.coords;
            dragging_active = state.active;
        }
        if dragging_active {
            self.update_pane_tab_drag_target(&event);
            context.set_cursor(Some(MouseCursor::Hand));
            context.invalidate();
        }
        // drag_ui_item takes `dragging` on every move; keep the drag armed
        self.dragging.replace((item, start_event));
    }

    /// Hit-test the pointer against the rendered panes and record which
    /// drop (move-into-stack or directional split) releasing here would
    /// perform, along with the preview rectangle to paint.
    fn update_pane_tab_drag_target(&mut self, event: &MouseEvent) {
        let Some(src_pane_id) = self.pane_tab_drag.as_ref().map(|state| state.pane_id) else {
            return;
        };
        let mut target = None;
        let mux = Mux::get();
        if let Some(tab) = mux.get_active_tab_for_window(self.mux_window_id) {
            let x = event.coords.x as f32;
            let y = event.coords.y as f32;
            for pos in self.get_panes_to_render() {
                if self.collapsed_pane_layouts.contains_key(&pos.pane_stack_id) {
                    continue;
                }
                let Ok(rect) = self.pane_frame_rect(&pos) else {
                    continue;
                };
                if x < rect.min_x() || x >= rect.max_x() || y < rect.min_y() || y >= rect.max_y() {
                    continue;
                }
                // TmuxDomain cannot move existing panes; remote mux panes
                // are supported by ClientDomain.
                if !Self::pane_tab_is_movable(&pos.pane) {
                    break;
                }
                let stack_tabs = tab.pane_stack_tabs(pos.pane.pane_id());
                let src_in_target_stack =
                    stack_tabs.iter().any(|entry| entry.pane_id == src_pane_id);
                let fx = (x - rect.min_x()) / rect.size.width.max(1.0);
                let fy = (y - rect.min_y()) / rect.size.height.max(1.0);
                // Hovering the target pane's own level-2 tab bar reads as
                // "join this stack", like dropping a browser tab onto
                // another window's tab strip.
                let nav_bar_h = self.pane_nav_bar_height_for_pane(&pos) as f32;
                let zone = if y < rect.min_y() + nav_bar_h {
                    PaneDropZone::Center
                } else {
                    pane_drop_zone(fx, fy)
                };
                let Some(kind) = pane_drop_action(zone, src_in_target_stack, stack_tabs.len())
                else {
                    break;
                };
                let preview = match zone {
                    // Joining the stack highlights the target's level-2 tab
                    // bar, like dropping a browser tab onto a tab strip.
                    PaneDropZone::Center => euclid::rect(
                        rect.origin.x,
                        rect.origin.y,
                        rect.size.width,
                        nav_bar_h.max(1.0),
                    ),
                    PaneDropZone::Left => euclid::rect(
                        rect.origin.x,
                        rect.origin.y,
                        rect.size.width / 2.0,
                        rect.size.height,
                    ),
                    PaneDropZone::Right => euclid::rect(
                        rect.origin.x + rect.size.width / 2.0,
                        rect.origin.y,
                        rect.size.width / 2.0,
                        rect.size.height,
                    ),
                    PaneDropZone::Top => euclid::rect(
                        rect.origin.x,
                        rect.origin.y,
                        rect.size.width,
                        rect.size.height / 2.0,
                    ),
                    PaneDropZone::Bottom => euclid::rect(
                        rect.origin.x,
                        rect.origin.y + rect.size.height / 2.0,
                        rect.size.width,
                        rect.size.height / 2.0,
                    ),
                };
                target = Some(PaneTabDropTarget {
                    target_pane_id: pos.pane.pane_id(),
                    zone,
                    kind,
                    rect: preview,
                });
                break;
            }
        }
        if let Some(state) = self.pane_tab_drag.as_mut() {
            state.target = target;
        }
    }

    fn drop_pane_tab_drag(&mut self, state: PaneTabDragState) {
        let Some(target) = state.target else {
            return;
        };
        let src_pane_id = state.pane_id;
        let mux = Mux::get();
        let Some(tab) = mux.get_active_tab_for_window(self.mux_window_id) else {
            return;
        };
        // Re-validate against the live tree: the target was computed on a
        // prior frame and panes may have closed or collapsed since.
        if tab.get_zoomed_pane().is_some() {
            return;
        }
        if tab.pane_index_for_pane(src_pane_id).is_none()
            || tab.pane_index_for_pane(target.target_pane_id).is_none()
        {
            return;
        }
        if tab
            .pane_stack_id(target.target_pane_id)
            .is_some_and(|stack_id| self.collapsed_pane_layouts.contains_key(&stack_id))
        {
            return;
        }

        match target.kind {
            PaneDropKind::MoveIntoStack => {
                let target_pane_id = target.target_pane_id;
                let dest_tab_id = tab.tab_id();
                let workspace = self.current_mux_workspace();
                let window = GuiWin::new(self);
                promise::spawn::spawn(async move {
                    match Mux::get()
                        .move_pane_to_stack(src_pane_id, target_pane_id)
                        .await
                    {
                        Ok(moved) => {
                            window.window.notify(TermWindowNotif::Apply(Box::new(
                                move |term_window| {
                                    if let Some(tab) = Mux::get().get_tab(dest_tab_id) {
                                        tab.set_active_pane(&moved);
                                    }
                                    if term_window.active_tab_is(dest_tab_id) {
                                        term_window.sync_active_tab_geometry_now();
                                    }
                                    if term_window.current_mux_workspace() == workspace {
                                        term_window.persist_workspace_layout_after_mutation(
                                            "pane tab drop move",
                                        );
                                    }
                                    term_window.update_title();
                                    if let Some(window) = term_window.window.as_ref() {
                                        window.invalidate();
                                    }
                                },
                            )));
                        }
                        Err(err) => log::error!("pane tab drop move failed: {err:#}"),
                    }
                })
                .detach();
            }
            PaneDropKind::Split => {
                let stack_tabs = tab.pane_stack_tabs(target.target_pane_id);
                let src_in_target_stack =
                    stack_tabs.iter().any(|entry| entry.pane_id == src_pane_id);
                // Splitting relative to the source's own stack must target
                // another pane in that stack: MovePane removes the source
                // first and would then fail to find it, orphaning the pane.
                let effective_target = if src_in_target_stack {
                    match stack_tabs.iter().find(|entry| entry.pane_id != src_pane_id) {
                        Some(other) => other.pane_id,
                        None => return,
                    }
                } else {
                    target.target_pane_id
                };
                let request = zone_split_request(target.zone);
                // Preflight the geometry: compute_split_size hands back
                // zero-sized halves for tiny panes without complaining, and
                // split_and_insert would then reject the split only after
                // MovePane has already detached the source.
                let Some(target_index) = tab.pane_index_for_pane(effective_target) else {
                    return;
                };
                match tab.compute_split_size(target_index, request) {
                    Some(split)
                        if split.first.rows > 0
                            && split.first.cols > 0
                            && split.second.rows > 0
                            && split.second.cols > 0 => {}
                    _ => {
                        log::debug!(
                            "pane tab drop split: no room to split pane {effective_target}"
                        );
                        return;
                    }
                }
                let dest_tab_id = tab.tab_id();
                let workspace = self.current_mux_workspace();
                let window = GuiWin::new(self);
                promise::spawn::spawn(async move {
                    match Mux::get()
                        .split_pane(
                            effective_target,
                            request,
                            SplitSource::MovePane(src_pane_id),
                            SpawnTabDomain::CurrentPaneDomain,
                        )
                        .await
                    {
                        Ok((moved, _size)) => {
                            window.window.notify(TermWindowNotif::Apply(Box::new(
                                move |term_window| {
                                    // Look up the destination tab by id: the
                                    // window's active tab may have changed
                                    // while the split was in flight.
                                    if let Some(tab) = Mux::get().get_tab(dest_tab_id) {
                                        // split_and_insert only activates
                                        // right/bottom targets; make all four
                                        // directions end focused on the moved
                                        // pane.
                                        tab.set_active_pane(&moved);
                                    }
                                    if term_window.active_tab_is(dest_tab_id) {
                                        term_window.sync_active_tab_geometry_now();
                                    }
                                    if term_window.current_mux_workspace() == workspace {
                                        term_window.persist_workspace_layout_after_mutation(
                                            "pane tab drop split",
                                        );
                                    }
                                    term_window.update_title();
                                    if let Some(window) = term_window.window.as_ref() {
                                        window.invalidate();
                                    }
                                },
                            )));
                        }
                        Err(err) => log::error!("pane tab drop split failed: {err:#}"),
                    }
                })
                .detach();
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
        } else if self.right_sidebar_note_pane_rect().is_some() {
            self.set_right_sidebar_note_pane_total_width(width);
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

    fn drag_right_sidebar_note_pane_resize(
        &mut self,
        item: UIItem,
        start_event: MouseEvent,
        event: MouseEvent,
        context: &dyn WindowOps,
    ) {
        if self.set_right_sidebar_note_pane_split_x(event.coords.x) {
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
                self.mouse_event_pane_nav(item, pane_id, pane_index, action, event, context);
            }
            UIItemType::ProjectNew => {
                self.mouse_event_project_new(event, context);
            }
            UIItemType::SpaceMenu => {
                self.mouse_event_space_menu(item, event, context);
            }
            UIItemType::SpaceReconnect => {
                if let WMEK::Press(MousePress::Left) = event.kind {
                    self.reconnect_space_domain();
                }
                context.set_cursor(Some(MouseCursor::Arrow));
            }
            UIItemType::ProjectToggleThreads(project_id) => {
                self.mouse_event_project_toggle_threads(project_id, event, context);
            }
            UIItemType::ThreadRefGroupToggle {
                key: ref group_key, ..
            } => {
                let group_key = group_key.clone();
                if let WMEK::Press(MousePress::Left) = event.kind {
                    // Arm a potential folder-reorder drag; the collapse
                    // toggle fires on release when the pointer never crossed
                    // the drag threshold — same contract as project headers.
                    let raw_key = group_key
                        .strip_prefix(&format!("{}::", self.active_space_id))
                        .unwrap_or(&group_key)
                        .to_string();
                    let title =
                        crate::workspace_threads::ref_group_title(&self.active_space_id, &raw_key)
                            .unwrap_or_else(|| raw_key.clone());
                    self.arm_sidebar_row_drag(
                        item,
                        super::SidebarRowKind::RefGroup(raw_key),
                        title,
                        true,
                        event,
                    );
                }
                context.set_cursor(Some(MouseCursor::Arrow));
            }
            UIItemType::ThreadRefGroupNewThread(group_key) => {
                if let WMEK::Press(MousePress::Left) = event.kind {
                    self.create_thread_in_ref_group(&group_key, context);
                }
                context.set_cursor(Some(MouseCursor::Arrow));
            }
            UIItemType::Project(ref project_id) => {
                let project_id = project_id.clone();
                self.mouse_event_project(item, project_id, event, context);
            }
            UIItemType::ArchivedProject(ref project_id) => {
                let project_id = project_id.clone();
                self.mouse_event_archived_project(project_id, event, context);
            }
            UIItemType::WorkspaceThread(ref thread_id) => {
                let thread_id = thread_id.clone();
                self.mouse_event_workspace_thread(item, thread_id, event, context);
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
            UIItemType::RightSidebarNotePaneResize => {
                self.mouse_event_right_sidebar_note_pane_resize(item, event, context);
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
            UIItemType::RightSidebarAgent(_) => {
                self.mouse_event_right_sidebar_agent(item.clone(), event, context);
            }
            UIItemType::RightSidebarNoteMenu
            | UIItemType::RightSidebarNoteChooseVault
            | UIItemType::RightSidebarNoteCreateVault
            | UIItemType::RightSidebarNoteRetry
            | UIItemType::RightSidebarNoteTreeToggle
            | UIItemType::RightSidebarNoteTreeBack
            | UIItemType::RightSidebarNoteTreeRow(_)
            | UIItemType::RightSidebarNoteCodeToggle(_)
            | UIItemType::RightSidebarNoteCodeCopy(_)
            | UIItemType::RightSidebarNoteBody
            | UIItemType::RightSidebarNotePaneToggle => {
                self.mouse_event_right_sidebar_note(item.clone(), event, context);
            }
            UIItemType::RightSidebarFileFilter
            | UIItemType::RightSidebarFileRefresh
            | UIItemType::RightSidebarFileRow(_)
            | UIItemType::RightSidebarFileBack
            | UIItemType::RightSidebarFileOpen
            | UIItemType::RightSidebarFileOpenMenu
            | UIItemType::RightSidebarFileReveal
            | UIItemType::RightSidebarFileCopyText => {
                self.mouse_event_right_sidebar_file(item.clone(), event, context);
            }
            UIItemType::RightSidebarRemoteFileConnect
            | UIItemType::RightSidebarRemoteFileRefresh
            | UIItemType::RightSidebarRemoteFileRow(_)
            | UIItemType::RightSidebarRemoteFileBack
            | UIItemType::RightSidebarRemoteFileCopyText
            | UIItemType::RightSidebarRemoteTransfer(_) => {
                self.mouse_event_right_sidebar_remote_file(item.clone(), event, context);
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
            UIItemType::WorkspaceSidebarLiveOverview => {
                context.set_cursor(Some(MouseCursor::Hand));
                if event.kind == WMEK::Press(MousePress::Left) {
                    self.toggle_live_overview_view();
                }
            }
            UIItemType::ContentViewClose(id) => {
                context.set_cursor(Some(MouseCursor::Hand));
                if event.kind == WMEK::Press(MousePress::Left) {
                    self.request_close_content_view_by_id(id);
                }
            }
            UIItemType::WorkspaceSidebarThreadSearch => {
                context.set_cursor(Some(MouseCursor::Hand));
                if event.kind == WMEK::Press(MousePress::Left) {
                    self.open_thread_search();
                }
            }
            UIItemType::WorkspaceSidebarViewOptions => {
                self.mouse_event_workspace_sidebar_view_options(item, event, context);
            }
            UIItemType::WorkspaceSidebarNotifications => {
                self.mouse_event_workspace_sidebar_notifications(item.clone(), event, context);
            }
            UIItemType::ContextMenuBackdrop | UIItemType::CommandPalette => {
                context.set_cursor(Some(MouseCursor::Arrow));
            }
            UIItemType::ContextMenuItem(_) => {
                context.set_cursor(Some(MouseCursor::Hand));
            }
        }
    }

    fn mouse_event_right_sidebar_note(
        &mut self,
        item: UIItem,
        event: MouseEvent,
        context: &dyn WindowOps,
    ) {
        match item.item_type.clone() {
            UIItemType::RightSidebarNoteMenu => {
                context.set_cursor(Some(MouseCursor::Hand));
                if event.kind == WMEK::Press(MousePress::Left) {
                    self.show_right_sidebar_note_menu(context, event.coords);
                    context.invalidate();
                }
            }
            UIItemType::RightSidebarNoteChooseVault => {
                context.set_cursor(Some(MouseCursor::Hand));
                if event.kind == WMEK::Press(MousePress::Left) {
                    self.perform_right_sidebar_note_command(
                        crate::termwindow::NoteEditorCommand::ChooseVault { managed: false },
                    );
                }
            }
            UIItemType::RightSidebarNoteRetry => {
                context.set_cursor(Some(MouseCursor::Hand));
                if event.kind == WMEK::Press(MousePress::Left) {
                    self.retry_right_sidebar_note_vault();
                }
            }
            UIItemType::RightSidebarNoteCreateVault => {
                context.set_cursor(Some(MouseCursor::Hand));
                if event.kind == WMEK::Press(MousePress::Left) {
                    self.perform_right_sidebar_note_command(
                        crate::termwindow::NoteEditorCommand::ChooseVault { managed: true },
                    );
                }
            }
            UIItemType::RightSidebarNoteTreeToggle => {
                context.set_cursor(Some(MouseCursor::Hand));
                if event.kind == WMEK::Press(MousePress::Left) {
                    self.toggle_right_sidebar_note_vault_tree();
                    self.right_sidebar_note.view.focused = false;
                    context.invalidate();
                }
            }
            UIItemType::RightSidebarNotePaneToggle => {
                context.set_cursor(Some(MouseCursor::Hand));
                if event.kind == WMEK::Press(MousePress::Left) {
                    self.toggle_right_sidebar_note_pane();
                    context.invalidate();
                }
            }
            UIItemType::RightSidebarNoteTreeBack => {
                context.set_cursor(Some(MouseCursor::Hand));
                if event.kind == WMEK::Press(MousePress::Left) {
                    self.right_sidebar_note_view = crate::termwindow::RightSidebarNoteView::Editor;
                    context.invalidate();
                }
            }
            UIItemType::RightSidebarNoteTreeRow(relative_path) => {
                context.set_cursor(Some(MouseCursor::Hand));
                if event.kind == WMEK::Press(MousePress::Left) {
                    self.activate_right_sidebar_note_tree_path(&relative_path);
                    context.invalidate();
                }
            }
            UIItemType::RightSidebarNoteCodeToggle(source_start) => {
                context.set_cursor(Some(MouseCursor::Hand));
                if event.kind == WMEK::Press(MousePress::Left) {
                    let code =
                        self.right_sidebar_note
                            .projection
                            .objects
                            .iter()
                            .find_map(|object| match object {
                                crate::markdown_editor::ProjectedObject::CodeBlock(code)
                                    if code.source.start == source_start =>
                                {
                                    Some(code.clone())
                                }
                                _ => None,
                            });
                    let collapsed = self.right_sidebar_note.toggle_code_block(source_start);
                    if collapsed {
                        if let Some(code) = code {
                            let caret = self.right_sidebar_note.view.selection.focus.byte;
                            if code.content.start <= caret && caret <= code.content.end {
                                if let Some(session) = self.right_sidebar_note.session.clone() {
                                    let target = {
                                        let session = session.lock();
                                        let source = session.source();
                                        let end = code.source.end.min(source.len());
                                        if source[end..].starts_with('\n') {
                                            (end + 1).min(source.len())
                                        } else {
                                            end
                                        }
                                    };
                                    session.lock().set_caret(
                                        &mut self.right_sidebar_note.view,
                                        target,
                                        false,
                                    );
                                }
                            }
                        }
                    }
                    self.right_sidebar_note.reveal_caret = true;
                    self.right_sidebar_note.refresh_projection();
                    context.invalidate();
                }
            }
            UIItemType::RightSidebarNoteCodeCopy(source_start) => {
                context.set_cursor(Some(MouseCursor::Hand));
                if event.kind == WMEK::Press(MousePress::Left) {
                    if let Some(text) =
                        self.right_sidebar_note
                            .projection
                            .objects
                            .iter()
                            .find_map(|object| match object {
                                crate::markdown_editor::ProjectedObject::CodeBlock(code)
                                    if code.source.start == source_start =>
                                {
                                    Some(code.text.clone())
                                }
                                _ => None,
                            })
                    {
                        self.copy_to_clipboard(ClipboardCopyDestination::Clipboard, text);
                    }
                    context.invalidate();
                }
            }
            UIItemType::RightSidebarNoteBody => {
                let external_link_target = self
                    .right_sidebar_note
                    .external_link_target_for_point(event.coords.x as f32, event.coords.y as f32);
                context.set_cursor(Some(if external_link_target.is_some() {
                    MouseCursor::Hand
                } else {
                    MouseCursor::Text
                }));
                if event.kind == WMEK::Press(MousePress::Right) {
                    self.right_sidebar_note.begin_live_editing();
                    let position = self
                        .right_sidebar_note
                        .source_position_for_point(event.coords.x as f32, event.coords.y as f32);
                    self.right_sidebar_note.view.focused = true;
                    let selected = self.right_sidebar_note.view.selection.range();
                    if selected.is_empty()
                        || position.byte < selected.start
                        || position.byte >= selected.end
                    {
                        self.right_sidebar_note.begin_selection(
                            position,
                            crate::markdown_editor::SelectionGranularity::Word,
                            false,
                        );
                    }
                    self.right_sidebar_note.reveal_caret = true;
                    self.right_sidebar_note.refresh_projection();
                    let items = self.right_sidebar_note_context_menu_items(event.coords);
                    self.show_term_context_menu(context, event.coords, items);
                    context.invalidate();
                    return;
                }
                if event.kind == WMEK::Press(MousePress::Left) {
                    let click_streak = self
                        .last_mouse_click
                        .as_ref()
                        .map(|click| click.streak)
                        .unwrap_or(1);
                    let extend = event.modifiers.contains(::window::Modifiers::SHIFT);
                    if click_streak == 1 && !extend {
                        if let Some(target) = external_link_target {
                            wezterm_open_url::open_url(&target);
                            context.invalidate();
                            return;
                        }
                    }
                    let atomic_range = self
                        .right_sidebar_note
                        .atomic_source_for_point(event.coords.x as f32, event.coords.y as f32);
                    if let Some(range) = atomic_range.as_ref() {
                        let wiki_link =
                            self.right_sidebar_note
                                .projection
                                .objects
                                .iter()
                                .find_map(|object| match object {
                                    crate::markdown_editor::ProjectedObject::WikiLink {
                                        source,
                                        target,
                                        resolved_path,
                                        ambiguous_paths,
                                        ..
                                    } if source == range => Some((
                                        target.clone(),
                                        resolved_path.clone(),
                                        ambiguous_paths.clone(),
                                    )),
                                    _ => None,
                                });
                        if click_streak == 1 && !extend {
                            if let Some((target, resolved_path, ambiguous_paths)) = wiki_link {
                                if ambiguous_paths.len() > 1 {
                                    self.show_right_sidebar_note_wiki_link_choices(
                                        context,
                                        event.coords,
                                        ambiguous_paths,
                                    );
                                } else {
                                    self.activate_or_create_right_sidebar_wiki_link(
                                        &target,
                                        resolved_path.as_deref(),
                                    );
                                }
                                context.invalidate();
                                return;
                            }
                        }
                        let replacement =
                            self.right_sidebar_note
                                .session
                                .as_ref()
                                .and_then(|session| {
                                    let session = session.lock();
                                    session.source().get(range.clone()).and_then(|source| {
                                        let trimmed = source.trim();
                                        if trimmed.eq_ignore_ascii_case("[x]") {
                                            Some("[ ]")
                                        } else if trimmed == "[ ]" {
                                            Some("[x]")
                                        } else {
                                            None
                                        }
                                    })
                                });
                        if click_streak == 1 && !extend {
                            if let Some(replacement) = replacement {
                                self.right_sidebar_note.view.focused = true;
                                self.right_sidebar_note.view.selection =
                                    crate::markdown_editor::SourceSelection {
                                        anchor: crate::markdown_editor::SourcePosition::new(
                                            range.start,
                                        ),
                                        focus: crate::markdown_editor::SourcePosition::new(
                                            range.end,
                                        ),
                                    };
                                self.push_right_sidebar_text(replacement);
                                context.invalidate();
                                return;
                            }
                        }
                    }
                    self.right_sidebar_note.begin_live_editing();
                    let position = self
                        .right_sidebar_note
                        .source_position_for_point(event.coords.x as f32, event.coords.y as f32);
                    self.right_sidebar_note.view.focused = true;
                    let granularity = if click_streak >= 3 {
                        crate::markdown_editor::SelectionGranularity::MarkdownBlock
                    } else if click_streak == 2 {
                        crate::markdown_editor::SelectionGranularity::Word
                    } else {
                        crate::markdown_editor::SelectionGranularity::Character
                    };
                    if click_streak == 2 && !extend {
                        if let Some(range) = atomic_range {
                            self.right_sidebar_note.selection_granularity = granularity;
                            self.right_sidebar_note.view.selection =
                                crate::markdown_editor::SourceSelection {
                                    anchor: crate::markdown_editor::SourcePosition::new(
                                        range.start,
                                    ),
                                    focus: crate::markdown_editor::SourcePosition::new(range.end),
                                };
                            self.right_sidebar_note.drag_selection_base = Some(range);
                        } else {
                            self.right_sidebar_note
                                .begin_selection(position, granularity, extend);
                        }
                    } else {
                        self.right_sidebar_note
                            .begin_selection(position, granularity, extend);
                    }
                    self.right_sidebar_note.reveal_caret = true;
                    self.right_sidebar_note.drag_selection_active = false;
                    self.right_sidebar_note.refresh_projection();
                    self.dragging.replace((item, event));
                    context.invalidate();
                }
            }
            _ => {}
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
            UIItemType::SpaceReconnect => {
                if let WMEK::Press(MousePress::Left) = event.kind {
                    self.reconnect_space_domain();
                }
                context.set_cursor(Some(MouseCursor::Arrow));
            }
            UIItemType::ProjectToggleThreads(project_id) => {
                self.mouse_event_project_toggle_threads(project_id, event, context);
            }
            UIItemType::ThreadRefGroupToggle {
                key: ref group_key, ..
            } => {
                let group_key = group_key.clone();
                if let WMEK::Press(MousePress::Left) = event.kind {
                    // Arm a potential folder-reorder drag; the collapse
                    // toggle fires on release when the pointer never crossed
                    // the drag threshold — same contract as project headers.
                    let raw_key = group_key
                        .strip_prefix(&format!("{}::", self.active_space_id))
                        .unwrap_or(&group_key)
                        .to_string();
                    let title =
                        crate::workspace_threads::ref_group_title(&self.active_space_id, &raw_key)
                            .unwrap_or_else(|| raw_key.clone());
                    self.arm_sidebar_row_drag(
                        item,
                        super::SidebarRowKind::RefGroup(raw_key),
                        title,
                        true,
                        event,
                    );
                }
                context.set_cursor(Some(MouseCursor::Arrow));
            }
            UIItemType::ThreadRefGroupNewThread(group_key) => {
                if let WMEK::Press(MousePress::Left) = event.kind {
                    self.create_thread_in_ref_group(&group_key, context);
                }
                context.set_cursor(Some(MouseCursor::Arrow));
            }
            UIItemType::Project(ref project_id) => {
                let project_id = project_id.clone();
                self.mouse_event_project(item, project_id, event, context);
            }
            UIItemType::ArchivedProject(ref project_id) => {
                let project_id = project_id.clone();
                self.mouse_event_archived_project(project_id, event, context);
            }
            UIItemType::WorkspaceThread(ref thread_id) => {
                let thread_id = thread_id.clone();
                self.mouse_event_workspace_thread(item, thread_id, event, context);
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
            UIItemType::WorkspaceSidebarLiveOverview => {
                context.set_cursor(Some(MouseCursor::Hand));
                if event.kind == WMEK::Press(MousePress::Left) {
                    self.toggle_live_overview_view();
                }
            }
            UIItemType::WorkspaceSidebarThreadSearch => {
                context.set_cursor(Some(MouseCursor::Hand));
                if event.kind == WMEK::Press(MousePress::Left) {
                    self.open_thread_search();
                }
            }
            UIItemType::WorkspaceSidebarViewOptions => {
                self.mouse_event_workspace_sidebar_view_options(item, event, context);
            }
            UIItemType::WorkspaceSidebarNotifications => {
                self.mouse_event_workspace_sidebar_notifications(item.clone(), event, context);
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
            UIItemType::RightSidebarNotePaneResize => {
                self.mouse_event_right_sidebar_note_pane_resize(item, event, context);
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
            UIItemType::RightSidebarAgent(_) => {
                self.mouse_event_right_sidebar_agent(item.clone(), event, context);
            }
            UIItemType::RightSidebarNoteMenu
            | UIItemType::RightSidebarNoteChooseVault
            | UIItemType::RightSidebarNoteCreateVault
            | UIItemType::RightSidebarNoteRetry
            | UIItemType::RightSidebarNoteTreeToggle
            | UIItemType::RightSidebarNoteTreeBack
            | UIItemType::RightSidebarNoteTreeRow(_)
            | UIItemType::RightSidebarNoteCodeToggle(_)
            | UIItemType::RightSidebarNoteCodeCopy(_)
            | UIItemType::RightSidebarNoteBody
            | UIItemType::RightSidebarNotePaneToggle => {
                self.mouse_event_right_sidebar_note(item.clone(), event, context);
            }
            UIItemType::RightSidebarFileFilter
            | UIItemType::RightSidebarFileRefresh
            | UIItemType::RightSidebarFileRow(_)
            | UIItemType::RightSidebarFileBack
            | UIItemType::RightSidebarFileOpen
            | UIItemType::RightSidebarFileOpenMenu
            | UIItemType::RightSidebarFileReveal
            | UIItemType::RightSidebarFileCopyText => {
                self.mouse_event_right_sidebar_file(item.clone(), event, context);
            }
            UIItemType::RightSidebarRemoteFileConnect
            | UIItemType::RightSidebarRemoteFileRefresh
            | UIItemType::RightSidebarRemoteFileRow(_)
            | UIItemType::RightSidebarRemoteFileBack
            | UIItemType::RightSidebarRemoteFileCopyText
            | UIItemType::RightSidebarRemoteTransfer(_) => {
                self.mouse_event_right_sidebar_remote_file(item.clone(), event, context);
            }
            UIItemType::ContentViewClose(id) => {
                context.set_cursor(Some(MouseCursor::Hand));
                if event.kind == WMEK::Press(MousePress::Left) {
                    self.request_close_content_view_by_id(id);
                }
            }
            UIItemType::ContextMenuBackdrop | UIItemType::CommandPalette => {
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
            {
                // X11's _NET_WM_MOVERESIZE request needs the root coordinates
                // from this press. The last hover/move position can be stale
                // and causes the WM to start the drag from the wrong anchor.
                context.set_window_drag_position(event.screen_coords);
                self.window_drag_position.replace(event);
            }
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

    pub fn mouse_event_right_sidebar_note_pane_resize(
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
            // A remote preview has no local file to hand to another
            // application, so "Open With" is meaningless there; it gets its
            // own menu of things that do apply instead.
            WMEK::Press(MousePress::Right)
                if self.right_sidebar_remote_files.selected.is_some() =>
            {
                self.show_right_sidebar_remote_file_preview_context_menu(context, event.coords);
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
            let previous_mode = self.right_sidebar_mode;
            if previous_mode == super::RightSidebarMode::Tasks
                && mode != super::RightSidebarMode::Tasks
            {
                self.clear_right_sidebar_text_focus();
            }
            self.right_sidebar_mode = mode;
            if self.right_sidebar_mode == super::RightSidebarMode::Tasks {
                self.right_sidebar_note_memory_release_token =
                    self.right_sidebar_note_memory_release_token.wrapping_add(1);
            } else if previous_mode == super::RightSidebarMode::Tasks {
                self.schedule_right_sidebar_note_memory_release();
            }
            // Leaving the file view (e.g. switching to Snippets/Tasks) makes the
            // file index idle; schedule it for release. Entering it refreshes +
            // (re)starts the periodic re-scan.
            if self.right_sidebar_file_view_active() {
                self.kick_right_sidebar_file_rescan_cycle();
                self.request_right_sidebar_remote_files_connect(false);
            } else {
                self.schedule_right_sidebar_file_memory_release();
                self.release_right_sidebar_remote_files_if_hidden();
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
        let is_input = matches!(item_type, UIItemType::RightSidebarFileFilter)
            || matches!(&item_type, UIItemType::RightSidebarFileRow(path) if self.is_renaming_sidebar_file(path));
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
                // Start walking the project the moment the box takes focus, so
                // the index is ready by the time a query is typed.
                self.prime_right_sidebar_file_index();
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
            (UIItemType::RightSidebarFileRefresh, WMEK::Press(MousePress::Left)) => {
                self.clear_right_sidebar_text_focus();
                self.force_right_sidebar_file_rescan();
            }
            (UIItemType::RightSidebarFileRow(path), WMEK::Press(MousePress::Left)) => {
                if self.is_renaming_sidebar_file(&path) {
                    let double_click = self
                        .last_mouse_click
                        .as_ref()
                        .is_some_and(|click| click.streak >= 2);
                    if double_click {
                        if let Some(input) = self.right_sidebar_input_for_item_mut(
                            &UIItemType::RightSidebarFileRow(path),
                        ) {
                            input.caret_select_all();
                        }
                    } else {
                        self.position_right_sidebar_input_caret(
                            &UIItemType::RightSidebarFileRow(path),
                            event.coords.x,
                            false,
                        );
                        self.dragging.replace((item, event));
                    }
                    context.set_cursor(Some(MouseCursor::Text));
                    context.invalidate();
                    return;
                }
                self.clear_right_sidebar_text_focus();
                // Don't open yet: arm a potential drag toward the terminal.
                // If the pointer never crosses the threshold, the release
                // handler treats it as a click and opens the file.
                self.right_sidebar_file_drag = Some(super::FileDragState {
                    payload: super::FileDragPayload::Local(path),
                    start: event.coords,
                    current: event.coords,
                    active: false,
                });
                self.dragging.replace((item.clone(), event));
            }
            (UIItemType::RightSidebarFileRow(path), WMEK::Press(MousePress::Right)) => {
                self.clear_right_sidebar_text_focus();
                self.show_right_sidebar_file_context_menu(context, event.coords, path);
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

    pub fn mouse_event_right_sidebar_remote_file(
        &mut self,
        item: UIItem,
        event: MouseEvent,
        context: &dyn WindowOps,
    ) {
        let rename_input = matches!(
            &item.item_type,
            UIItemType::RightSidebarRemoteFileRow(path)
                if self.is_renaming_sidebar_remote_file(path)
        );
        context.set_cursor(Some(if rename_input {
            MouseCursor::Text
        } else {
            MouseCursor::Hand
        }));
        if let (UIItemType::RightSidebarRemoteFileRow(path), WMEK::Press(MousePress::Right)) =
            (&item.item_type, &event.kind)
        {
            self.show_right_sidebar_remote_file_context_menu(context, event.coords, path.clone());
            return;
        }
        if event.kind != WMEK::Press(MousePress::Left) {
            return;
        }
        let previous_width = self.right_sidebar_width();
        match item.item_type {
            UIItemType::RightSidebarRemoteFileConnect => {
                self.request_right_sidebar_remote_files_connect(true);
            }
            UIItemType::RightSidebarRemoteTransfer(id) => {
                self.remote_transfer_row_clicked(id, event.coords);
            }
            UIItemType::RightSidebarRemoteFileRefresh => {
                self.refresh_right_sidebar_remote_files();
            }
            UIItemType::RightSidebarRemoteFileRow(ref path) => {
                if self.is_renaming_sidebar_remote_file(path) {
                    let double_click = self
                        .last_mouse_click
                        .as_ref()
                        .is_some_and(|click| click.streak >= 2);
                    if double_click {
                        if let Some(input) = self.right_sidebar_input_for_item_mut(
                            &UIItemType::RightSidebarRemoteFileRow(path.clone()),
                        ) {
                            input.caret_select_all();
                        }
                    } else {
                        self.position_right_sidebar_input_caret(
                            &UIItemType::RightSidebarRemoteFileRow(path.clone()),
                            event.coords.x,
                            false,
                        );
                        self.dragging.replace((item, event));
                    }
                    context.invalidate();
                    return;
                }
                // Don't open yet: arm a potential drag toward the terminal,
                // exactly like the local rows. The release handler opens the
                // row when the pointer never crossed the threshold. Without a
                // source to pin the path to there is nothing safe to drag, so
                // fall back to opening on press.
                match self.current_remote_operation_origin() {
                    Some(origin) => {
                        self.right_sidebar_file_drag = Some(super::FileDragState {
                            payload: super::FileDragPayload::Remote {
                                path: path.clone(),
                                origin,
                            },
                            start: event.coords,
                            current: event.coords,
                            active: false,
                        });
                        self.dragging.replace((item.clone(), event));
                    }
                    None => self.open_right_sidebar_remote_file(path.clone()),
                }
            }
            UIItemType::RightSidebarRemoteFileBack => {
                self.close_right_sidebar_remote_file_preview();
            }
            UIItemType::RightSidebarRemoteFileCopyText => {
                self.copy_right_sidebar_selected_file_preview_text();
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
            let items = self.workspace_sidebar_view_options_menu_items();
            self.show_term_context_menu(context, coords, items);
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
                let items = self.space_menu_items();
                self.show_term_context_menu(context, coords, items);
            }
            WMEK::Press(MousePress::Right) => {
                let items = self.space_menu_items();
                self.show_term_context_menu(context, event.coords, items);
            }
            _ => {}
        }
    }

    /// A revealed archived row is inert on purpose: the only ways out are
    /// the explicit menu items, so a stray click cannot half-restore it.
    pub fn mouse_event_archived_project(
        &mut self,
        project_id: String,
        event: MouseEvent,
        context: &dyn WindowOps,
    ) {
        context.set_cursor(Some(MouseCursor::Arrow));
        if let WMEK::Press(MousePress::Right) = event.kind {
            let items = self.archived_project_context_menu_items(&project_id);
            self.show_term_context_menu(context, event.coords, items);
        }
    }

    pub fn mouse_event_project(
        &mut self,
        item: UIItem,
        project_id: String,
        event: MouseEvent,
        context: &dyn WindowOps,
    ) {
        match event.kind {
            WMEK::Press(MousePress::Left) => {
                // Arm a potential reorder drag; the collapse toggle fires on
                // release when the pointer never crossed the drag threshold.
                let title = crate::workspace_threads::project_name(&project_id)
                    .unwrap_or_else(|| project_id.clone());
                self.arm_sidebar_row_drag(
                    item,
                    super::SidebarRowKind::Project(project_id),
                    title,
                    true,
                    event,
                );
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
        item: UIItem,
        thread_id: String,
        event: MouseEvent,
        context: &dyn WindowOps,
    ) {
        match event.kind {
            WMEK::Press(MousePress::Left) => {
                // Arm a potential reorder drag; switching to the thread fires
                // on release when the pointer never crossed the threshold.
                // Rows in the cross-project pinned section arm too (their
                // click must keep working) but never activate as drags.
                let Some(project_id) = crate::workspace_threads::project_id_for_thread(&thread_id)
                else {
                    return;
                };
                // Reference rows reorder the local reference list, so the
                // origin thread's pinned state is irrelevant there.
                let draggable = self.sidebar_row_is_thread_ref(&thread_id)
                    || !crate::workspace_threads::thread_is_pinned(&thread_id);
                let title = crate::workspace_threads::thread_name(&thread_id)
                    .unwrap_or_else(|| thread_id.clone());
                self.arm_sidebar_row_drag(
                    item,
                    super::SidebarRowKind::Thread {
                        thread_id,
                        project_id,
                    },
                    title,
                    draggable,
                    event,
                );
            }
            WMEK::Press(MousePress::Right) => {
                let items = self.workspace_thread_context_menu_items(&thread_id);
                self.show_term_context_menu(context, event.coords, items);
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
                let items = self.workspace_thread_context_menu_items(&thread_id);
                self.show_term_context_menu(context, event.coords, items);
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
                // On a reference row the X removes the reference; the
                // thread itself keeps living in its origin Space.
                let space_id = self.active_space_id.clone();
                if self.sidebar_row_is_thread_ref(&thread_id) {
                    if crate::workspace_threads::remove_thread_ref(&space_id, &thread_id) {
                        context.invalidate();
                    }
                } else {
                    self.end_workspace_thread(&thread_id, Some(context));
                }
            }
            WMEK::Press(MousePress::Right) => {
                let items = self.workspace_thread_context_menu_items(&thread_id);
                self.show_term_context_menu(context, event.coords, items);
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
            crate::termwindow::ui::context_menu::reveal_in_folder_label(),
            ContextMenuIcon::Folder,
            KeyAssignment::RevealProjectInFolder(project_id.clone()),
        );
        if crate::workspace_threads::project_reveal_path(&project_id).is_none() {
            reveal_item = reveal_item.disabled();
        }
        // macOS only: the picker it runs is what grants access there.
        let mut grant_item = ContextMenuItem::item_with_icon(
            crate::i18n::tr("menu-grant-folder-access"),
            ContextMenuIcon::Folder,
            KeyAssignment::GrantProjectFolderAccess(project_id.clone()),
        );
        if crate::workspace_threads::local_project_path(&project_id).is_none() {
            grant_item = grant_item.disabled();
        }

        let mut items = vec![
            ContextMenuItem::item_with_icon(
                crate::i18n::tr("menu-rename-project"),
                ContextMenuIcon::Edit,
                KeyAssignment::PromptRenameProject(project_id.clone()),
            ),
            reveal_item,
            ContextMenuItem::item_with_icon(
                crate::i18n::tr("menu-new-thread"),
                ContextMenuIcon::New,
                KeyAssignment::CreateWorkspaceThread(project_id.clone()),
            ),
            ContextMenuItem::item_with_icon(
                crate::i18n::tr("menu-toggle-threads"),
                ContextMenuIcon::Collapse,
                KeyAssignment::ToggleWorkspaceThreadsCollapsed(project_id.clone()),
            ),
            ContextMenuItem::Separator,
            {
                // The only unrecoverable part of archiving is killing live
                // panes, so the second-gesture gate (the submenu standing in
                // for a confirm dialog, like Space delete's) appears only
                // when panes would actually die. A quiet project archives in
                // one click -- unarchiving is lossless.
                let mux = Mux::get();
                let live_panes: usize =
                    crate::workspace_threads::materialized_workspace_names_for_project(&project_id)
                        .iter()
                        .flat_map(|workspace| mux.iter_windows_in_workspace(workspace))
                        .filter_map(|window_id| mux.get_window(window_id))
                        .flat_map(|window| {
                            window
                                .iter()
                                .map(|tab| tab.iter_all_panes().len())
                                .collect::<Vec<_>>()
                        })
                        .sum();
                if live_panes == 0 {
                    ContextMenuItem::item_with_icon(
                        crate::i18n::tr("menu-archive-project-quiet"),
                        ContextMenuIcon::Archive,
                        KeyAssignment::ArchiveProject(project_id.clone()),
                    )
                } else {
                    ContextMenuItem::submenu_with_icon(
                        crate::i18n::tr("menu-archive-project"),
                        ContextMenuIcon::Archive,
                        vec![
                            ContextMenuItem::section_header({
                                let mut args = FluentArgs::new();
                                args.set("panes", live_panes as i64);
                                crate::i18n::tr_args("menu-archive-project-explain", &args)
                            }),
                            ContextMenuItem::item_with_icon(
                                crate::i18n::tr("menu-archive-project-confirm"),
                                ContextMenuIcon::Archive,
                                KeyAssignment::ArchiveProject(project_id.clone()),
                            ),
                        ],
                    )
                }
            },
            ContextMenuItem::item_with_icon(
                crate::i18n::tr("menu-remove-project"),
                ContextMenuIcon::FolderRemove,
                KeyAssignment::RemoveProject(project_id),
            ),
        ];
        if cfg!(target_os = "macos") {
            items.insert(2, grant_item);
        }
        items
    }

    /// The menu for a revealed archived row: restore it, or delete it for
    /// good through the same second-gesture gate the live menu uses.
    fn archived_project_context_menu_items(&self, project_id: &str) -> Vec<ContextMenuItem> {
        let project_id = project_id.to_string();
        vec![
            ContextMenuItem::item_with_icon(
                crate::i18n::tr("menu-unarchive-project"),
                ContextMenuIcon::ArchiveRestore,
                KeyAssignment::UnarchiveProject(project_id.clone()),
            ),
            ContextMenuItem::Separator,
            ContextMenuItem::submenu_with_icon(
                crate::i18n::tr("menu-delete-project-permanently"),
                ContextMenuIcon::Delete,
                vec![
                    ContextMenuItem::section_header({
                        let mut args = FluentArgs::new();
                        args.set(
                            "threads",
                            crate::workspace_threads::ordered_thread_ids(&project_id).len() as i64,
                        );
                        crate::i18n::tr_args("menu-delete-project-permanently-explain", &args)
                    }),
                    ContextMenuItem::item_with_icon(
                        crate::i18n::tr("menu-delete-project-permanently-confirm"),
                        ContextMenuIcon::Delete,
                        KeyAssignment::RemoveProject(project_id),
                    ),
                ],
            ),
        ]
    }

    /// One row per Space, grouped by the server hosting it.
    ///
    /// A remote server is a *connection*, not a Space: it can host several, and
    /// they all go up and down together. So it appears as a disabled group
    /// header carrying the connection's state, with its Spaces listed beneath
    /// and a "New Space Here" entry of its own. Local Spaces stay at the top,
    /// ungrouped.
    fn space_menu_items(&mut self) -> Vec<ContextMenuItem> {
        self.begin_context_menu_application_actions();
        let spaces = crate::workspace_threads::spaces_for_window(self.space_owner_id);

        let space_item = |space: &crate::workspace_threads::SpaceView| {
            let label = if space.is_occupied_by_other_window {
                tr_with_name("menu-space-occupied", &space.name)
            } else {
                space.name.clone()
            };
            let mut item = ContextMenuItem::item_with_icon(
                label,
                if space.is_default {
                    ContextMenuIcon::Home
                } else {
                    ContextMenuIcon::Stack
                },
                KeyAssignment::SwitchSpace(space.id.to_string()),
            )
            .checked(space.is_active);
            if space.is_occupied_by_other_window {
                item = item.disabled();
            }
            item
        };

        let mut items = vec![];
        for space in spaces.iter().filter(|space| space.domain.is_none()) {
            items.push(space_item(space));
        }

        // Keep the servers in the order their Spaces appear rather than
        // sorting: that order is the user's.
        let mut domains: Vec<&str> = vec![];
        for domain in spaces.iter().filter_map(|space| space.domain.as_deref()) {
            if !domains.contains(&domain) {
                domains.push(domain);
            }
        }

        for domain in domains {
            items.push(ContextMenuItem::Separator);
            let state = spaces
                .iter()
                .find(|space| space.domain.as_deref() == Some(domain))
                .map(|space| self.space_connection_state(&space.id))
                .unwrap_or(SpaceConnectionState::Connected);
            let header = match state {
                SpaceConnectionState::Connected => tr_with_name("menu-space-server-group", domain),
                SpaceConnectionState::Connecting => {
                    tr_with_name("menu-space-server-group-connecting", domain)
                }
                // Distinct from disconnected: the transport is being retried,
                // and calling that "disconnected" reads as though the Spaces
                // below are unreachable for good.
                SpaceConnectionState::Reconnecting => {
                    tr_with_name("menu-space-server-group-reconnecting", domain)
                }
                SpaceConnectionState::Disconnected => {
                    tr_with_name("menu-space-server-group-offline", domain)
                }
            };
            // A server is a heading, not something to switch to. Saying that
            // with a section header rather than a disabled row is what makes it
            // *look* like a heading: the platform styles it as a caption and
            // never highlights it under the pointer.
            items.push(ContextMenuItem::section_header(header));
            for space in spaces
                .iter()
                .filter(|space| space.domain.as_deref() == Some(domain))
            {
                items.push(space_item(space));
            }
            items.push(ContextMenuItem::item_with_icon(
                crate::i18n::tr("menu-new-space-here"),
                ContextMenuIcon::New,
                KeyAssignment::CreateSpaceOnDomain(domain.to_string()),
            ));
        }

        // The servers listed above are what this adds another of. Kept out of
        // the Space block below because a remote host is a connection, not a
        // Space: one host can carry several. It stays visible with no servers
        // configured at all -- then it is the only path to the first one.
        items.push(ContextMenuItem::Separator);
        items.push(ContextMenuItem::item_with_icon(
            crate::i18n::tr("menu-add-remote-host"),
            ContextMenuIcon::Network,
            KeyAssignment::AddRemoteHost,
        ));

        items.push(ContextMenuItem::Separator);
        items.push(ContextMenuItem::item_with_icon(
            crate::i18n::tr("menu-new-space"),
            ContextMenuIcon::New,
            KeyAssignment::CreateSpace,
        ));
        items.push(ContextMenuItem::item_with_icon(
            spaces
                .iter()
                .find(|space| space.is_active)
                .map(|space| tr_with_name("menu-rename-named-space", &space.name))
                .unwrap_or_else(|| crate::i18n::tr("menu-rename-space")),
            ContextMenuIcon::Edit,
            KeyAssignment::PromptRenameSpace(self.active_space_id.clone()),
        ));
        let active_space_id = spaces
            .iter()
            .find(|space| space.is_active)
            .map(|space| space.id.clone());
        // For a Space other than the active one there is only ever the one
        // action, so it is named in full here; the active Space's several
        // actions are grouped by `space_destructive_menu_items` instead.
        let delete_item = |space: crate::workspace_threads::SpaceView| {
            ContextMenuItem::item_with_icon(
                if space.is_remote {
                    tr_with_name("menu-disconnect-space", &space.name)
                } else {
                    tr_with_name("menu-delete-space", &space.name)
                },
                if space.is_remote {
                    ContextMenuIcon::Disconnect
                } else {
                    ContextMenuIcon::Delete
                },
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
            // Ending the sessions too only makes sense while we are actually
            // connected to the server.
            let offer_remote_kill = space.is_remote
                && crate::workspace_threads::client_domain_for_space(&space.id)
                    .and_then(|name| Mux::get().get_domain_by_name(&name))
                    .map_or(false, |domain| {
                        domain.state() == mux::domain::DomainState::Attached
                    });
            items.extend(space_destructive_menu_items(&space, offer_remote_kill));
        }

        let other_delete_candidates = delete_candidates
            .into_iter()
            .filter(|space| active_space_id.as_deref() != Some(space.id.as_str()))
            .map(delete_item)
            .collect::<Vec<_>>();
        if !other_delete_candidates.is_empty() {
            items.push(ContextMenuItem::submenu_with_icon(
                crate::i18n::tr("menu-remove-other-space"),
                ContextMenuIcon::Delete,
                other_delete_candidates,
            ));
        }
        items
    }

    fn workspace_sidebar_view_options_menu_items(&mut self) -> Vec<ContextMenuItem> {
        use crate::workspace_threads::WorkspaceThreadWorkStatus;
        use config::keyassignment::KeyAssignment;

        self.begin_context_menu_application_actions();
        let hidden = crate::native_settings::workspace_sidebar_hidden_statuses();
        let mut status_items = Vec::new();
        for (label, icon, status) in [
            (
                crate::i18n::tr("menu-status-running"),
                ContextMenuIcon::Refresh,
                WorkspaceThreadWorkStatus::Running,
            ),
            (
                crate::i18n::tr("menu-status-attention"),
                ContextMenuIcon::Warning,
                WorkspaceThreadWorkStatus::NeedsAttention,
            ),
            (
                crate::i18n::tr("menu-status-done"),
                ContextMenuIcon::Check,
                WorkspaceThreadWorkStatus::FinishedUnseen,
            ),
            (
                crate::i18n::tr("menu-status-idle"),
                ContextMenuIcon::Info,
                WorkspaceThreadWorkStatus::Idle,
            ),
        ] {
            let visible = !hidden.iter().any(|key| key == status.settings_key());
            status_items.push(
                self.context_menu_application_item_with_icon(
                    label,
                    icon,
                    crate::termwindow::ContextMenuApplicationAction::ToggleWorkspaceStatusFilter(
                        status,
                    ),
                    true,
                )
                .checked(visible),
            );
        }

        vec![
            ContextMenuItem::section_header(crate::i18n::tr("menu-group-by")),
            // Not a heading: it is the one grouping mode there is, shown ticked
            // and unclickable because there is nothing to switch it to.
            ContextMenuItem::item_with_icon(
                crate::i18n::tr("menu-workspace"),
                ContextMenuIcon::Folder,
                KeyAssignment::Nop,
            )
            .checked(true)
            .disabled(),
            ContextMenuItem::Separator,
            ContextMenuItem::section_header(crate::i18n::tr("menu-show")),
            ContextMenuItem::submenu_with_icon(
                crate::i18n::tr("menu-status"),
                ContextMenuIcon::Check,
                status_items,
            ),
            {
                let archived_count = crate::workspace_threads::archived_project_count(
                    self.workspace_sidebar_space_id(),
                );
                let label = if archived_count > 0 {
                    let mut args = FluentArgs::new();
                    args.set("count", archived_count as i64);
                    crate::i18n::tr_args("menu-show-archived-count", &args)
                } else {
                    crate::i18n::tr("menu-show-archived")
                };
                // Disabled at zero -- an inert toggle would look broken --
                // but never while the reveal is on, or the flag could latch
                // with no way to untoggle it.
                self.context_menu_application_item_with_icon(
                    label,
                    ContextMenuIcon::Archive,
                    crate::termwindow::ContextMenuApplicationAction::ToggleWorkspaceShowArchived,
                    archived_count > 0 || self.workspace_sidebar_show_archived,
                )
                .checked(self.workspace_sidebar_show_archived)
            },
        ]
    }

    /// Toggle a status's visibility in the workspace sidebar, refusing the
    /// toggle that would hide every status and leave the list empty.
    pub(crate) fn toggle_workspace_sidebar_status_filter(
        &mut self,
        status: crate::workspace_threads::WorkspaceThreadWorkStatus,
    ) {
        let mut hidden = crate::native_settings::workspace_sidebar_hidden_statuses();
        let key = status.settings_key();
        if let Some(index) = hidden.iter().position(|entry| entry == key) {
            hidden.remove(index);
        } else {
            hidden.push(key.to_string());
            let parsed = hidden
                .iter()
                .filter_map(|entry| {
                    crate::workspace_threads::WorkspaceThreadWorkStatus::from_settings_key(entry)
                })
                .collect::<Vec<_>>();
            if crate::workspace_threads::hidden_statuses_cover_all(&parsed) {
                return;
            }
        }
        if let Err(err) = crate::native_settings::save_workspace_sidebar_hidden_statuses(hidden) {
            log::warn!("failed to save sidebar status filter: {err:#}");
        }
        self.invalidate_window();
    }

    /// The notification bell: pending finished/attention threads across every
    /// Space; activating an entry jumps to that thread.
    pub fn mouse_event_workspace_sidebar_notifications(
        &mut self,
        item: UIItem,
        event: MouseEvent,
        context: &dyn WindowOps,
    ) {
        use config::keyassignment::KeyAssignment;

        context.set_cursor(Some(MouseCursor::Hand));
        if event.kind != WMEK::Press(MousePress::Left) {
            return;
        }
        let coords = window::Point::new(
            item.x.saturating_add(item.width) as isize,
            item.y.saturating_add(item.height / 2) as isize,
        );
        let notifications = crate::workspace_threads::pending_work_notifications();
        self.begin_context_menu_application_actions();
        let items = if notifications.is_empty() {
            vec![ContextMenuItem::item_with_icon(
                crate::i18n::tr("menu-no-notifications"),
                ContextMenuIcon::Notification,
                KeyAssignment::Nop,
            )
            .disabled()]
        } else {
            notifications
                .into_iter()
                .map(|notification| {
                    let icon = match notification.status {
                        crate::workspace_threads::WorkspaceThreadWorkStatus::NeedsAttention => {
                            ContextMenuIcon::Warning
                        }
                        _ => ContextMenuIcon::Check,
                    };
                    let label = format!(
                        "{} — {} · {}",
                        notification.thread_name,
                        notification.project_name,
                        notification.space_name
                    );
                    self.context_menu_application_item_with_icon(
                        label,
                        icon,
                        crate::termwindow::ContextMenuApplicationAction::ActivateWorkspaceThread {
                            space_id: notification.space_id,
                            thread_id: notification.thread_id,
                        },
                        true,
                    )
                })
                .collect()
        };
        self.show_term_context_menu(context, coords, items);
    }

    pub(crate) fn open_remote_workspace_thread_without_connecting(
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
        let space_has_client_domain =
            crate::workspace_threads::client_domain_for_space(&state.space_id).is_some();
        if !remote_thread_uses_ssh_connection_view(state.is_remote, space_has_client_domain) {
            // A mux-domain Space already has its transport. Its threads are
            // materialized directly through that ClientDomain and must never
            // be sent through the disconnected SSH-host content view.
            return false;
        }

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

    fn workspace_thread_context_menu_items(&mut self, thread_id: &str) -> Vec<ContextMenuItem> {
        self.begin_context_menu_application_actions();
        let thread_id = thread_id.to_string();
        let is_pinned = crate::workspace_threads::thread_is_pinned(&thread_id);
        let active_space = self.active_space_id.clone();
        // The row is a reference when this Space holds a ref to a thread
        // whose home is elsewhere.
        let is_thread_ref = crate::workspace_threads::thread_ref_exists(&active_space, &thread_id)
            && crate::workspace_threads::thread_space_id(&thread_id).as_deref()
                != Some(active_space.as_str());
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
                    crate::i18n::tr("menu-disconnect-thread"),
                    ContextMenuIcon::Close,
                    KeyAssignment::DisconnectWorkspaceThread(thread_id.clone()),
                ));
                items.push(ContextMenuItem::Separator);
            } else if connection.is_remote && remote_host_exists {
                items.push(ContextMenuItem::item_with_icon(
                    crate::i18n::tr("menu-connect-thread"),
                    ContextMenuIcon::ExternalLink,
                    KeyAssignment::ConnectWorkspaceThread(thread_id.clone()),
                ));
                items.push(ContextMenuItem::Separator);
            }
        }

        // On a reference row the pin toggle and Delete would mutate the
        // origin thread while reading like row actions — the reference is
        // removed via "Remove from This Space" below instead. Rename and
        // Mark Unread act on the real thread on purpose: a reference is a
        // shortcut to it, not a copy.
        if !is_thread_ref {
            items.push(ContextMenuItem::item_with_icon(
                if is_pinned {
                    crate::i18n::tr("menu-unpin-thread")
                } else {
                    crate::i18n::tr("menu-pin-thread")
                },
                if is_pinned {
                    ContextMenuIcon::Unpin
                } else {
                    ContextMenuIcon::Pin
                },
                KeyAssignment::ToggleWorkspaceThreadPinned(thread_id.clone()),
            ));
        }
        items.push(ContextMenuItem::item_with_icon(
            crate::i18n::tr("menu-rename-thread"),
            ContextMenuIcon::Edit,
            KeyAssignment::PromptRenameWorkspaceThread(thread_id.clone()),
        ));
        if !is_thread_ref {
            items.push(ContextMenuItem::item_with_icon(
                crate::i18n::tr("menu-delete-thread"),
                ContextMenuIcon::Delete,
                KeyAssignment::DeleteWorkspaceThread(thread_id.clone()),
            ));
        }
        items.push(ContextMenuItem::item_with_icon(
            crate::i18n::tr("menu-mark-unread"),
            ContextMenuIcon::Notification,
            KeyAssignment::MarkWorkspaceThreadUnread(thread_id.clone()),
        ));

        // Reference section. On a reference row the ref can be removed here
        // or traced back to its origin; on a thread's own row it can be
        // added to any eligible Space (local, not its home, not already
        // holding it).
        if is_thread_ref {
            items.push(ContextMenuItem::Separator);
            let remove_item = self.context_menu_application_item_with_icon(
                crate::i18n::tr("menu-remove-thread-ref"),
                ContextMenuIcon::Close,
                crate::termwindow::ContextMenuApplicationAction::RemoveThreadFromCollection {
                    collection_space_id: active_space.clone(),
                    thread_id: thread_id.clone(),
                },
                true,
            );
            items.push(remove_item);
            // Move the REFERENCE elsewhere: gone from this Space's list,
            // added to the target's. ref_host_candidates already excludes
            // the origin and every Space holding it — this one included.
            let move_targets: Vec<ContextMenuItem> =
                crate::workspace_threads::ref_host_candidates(&thread_id)
                    .into_iter()
                    .map(|(space_id, name)| {
                        self.context_menu_application_item_with_icon(
                            name,
                            ContextMenuIcon::ExternalLink,
                            crate::termwindow::ContextMenuApplicationAction::MoveThreadRefToSpace {
                                from_space_id: active_space.clone(),
                                space_id,
                                thread_id: thread_id.clone(),
                            },
                            true,
                        )
                    })
                    .collect();
            if !move_targets.is_empty() {
                items.push(ContextMenuItem::submenu_with_icon(
                    crate::i18n::tr("menu-move-to-space"),
                    ContextMenuIcon::ExternalLink,
                    move_targets,
                ));
            }
            if let Some(origin_space) = crate::workspace_threads::thread_space_id(&thread_id) {
                let origin_item = self.context_menu_application_item_with_icon(
                    crate::i18n::tr("menu-go-to-origin-space"),
                    ContextMenuIcon::ExternalLink,
                    crate::termwindow::ContextMenuApplicationAction::ActivateWorkspaceThread {
                        space_id: origin_space,
                        thread_id: thread_id.clone(),
                    },
                    true,
                );
                items.push(origin_item);
            }
        } else {
            // "Add to" references the thread elsewhere; "Move to" re-homes
            // it (local threads only — a remote thread executes on its
            // machine and can only be referenced). Each folds its Space
            // list into one submenu so a long Space roster stays one row.
            let add_targets: Vec<ContextMenuItem> =
                crate::workspace_threads::ref_host_candidates(&thread_id)
                    .into_iter()
                    .map(|(space_id, name)| {
                        self.context_menu_application_item_with_icon(
                            name,
                            ContextMenuIcon::Pin,
                            crate::termwindow::ContextMenuApplicationAction::AddThreadToCollection {
                                collection_space_id: Some(space_id),
                                thread_id: thread_id.clone(),
                            },
                            true,
                        )
                    })
                    .collect();
            let move_targets: Vec<ContextMenuItem> =
                crate::workspace_threads::move_host_candidates(&thread_id)
                    .into_iter()
                    .map(|(space_id, name)| {
                        self.context_menu_application_item_with_icon(
                            name,
                            ContextMenuIcon::ExternalLink,
                            crate::termwindow::ContextMenuApplicationAction::MoveThreadToSpace {
                                space_id,
                                thread_id: thread_id.clone(),
                            },
                            true,
                        )
                    })
                    .collect();
            if !add_targets.is_empty() || !move_targets.is_empty() {
                items.push(ContextMenuItem::Separator);
            }
            if !add_targets.is_empty() {
                items.push(ContextMenuItem::submenu_with_icon(
                    crate::i18n::tr("menu-add-to-space"),
                    ContextMenuIcon::Pin,
                    add_targets,
                ));
            }
            if !move_targets.is_empty() {
                items.push(ContextMenuItem::submenu_with_icon(
                    crate::i18n::tr("menu-move-to-space"),
                    ContextMenuIcon::ExternalLink,
                    move_targets,
                ));
            }
        }
        items
    }

    pub(crate) fn activate_workspace_thread(&mut self, thread_id: String, context: &dyn WindowOps) {
        self.activate_workspace_thread_impl(thread_id, context, None, Vec::new());
    }

    pub(crate) fn activate_workspace_thread_with_cleanup(
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

    /// The reference group "+": create a thread in the group's origin
    /// project — exactly the project-"+" flow — and auto-add a reference to
    /// it in the current Space so it appears (and activates) right here.
    pub(crate) fn create_thread_in_ref_group(&mut self, group_key: &str, context: &dyn WindowOps) {
        let Some(project_id) =
            crate::workspace_threads::ref_group_origin_project(&self.active_space_id, group_key)
        else {
            context.invalidate();
            return;
        };
        let thread_id = match crate::workspace_threads::create_thread(&project_id, None) {
            Ok(thread_id) => thread_id,
            Err(err) => {
                log::warn!("failed to create thread in reference group: {err:#}");
                context.invalidate();
                return;
            }
        };
        // Reference it before activating: the activation gate only lets this
        // window open threads its Space owns or references.
        crate::workspace_threads::add_thread_ref(&self.active_space_id, &thread_id);
        if !self.open_remote_workspace_thread_without_connecting(&thread_id, context) {
            self.activate_workspace_thread(thread_id, context);
        }
    }

    pub(crate) fn create_workspace_thread(&mut self, project_id: &str, context: &dyn WindowOps) {
        let thread_id = match crate::workspace_threads::create_thread(project_id, None) {
            Ok(thread_id) => thread_id,
            Err(err) => {
                log::warn!("failed to create ThinkTerm thread: {err:#}");
                context.invalidate();
                return;
            }
        };
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

        // A ThinkTerm Connect host attaches the persistent remote mux domain
        // in its dedicated Space — the same flow as `thinkterm connect
        // <name>` — rather than opening a direct-ssh remote thread.
        if spec.multiplexing && !spec.use_mosh {
            let name = spec.label.clone();
            // These hosts used to skip OS detection entirely: the branch
            // returned before anything read `detect_os`, and both detection
            // call sites downcast to `RemoteSshDomain`, which a mux client
            // domain is not. There is no ssh session here to borrow for an
            // `exec` either -- it is buried in the client's transport -- so
            // the server reports its own distro in the version handshake and
            // this just reads what it said.
            let detect = spec.detect_os && spec.detected_distro.is_none();
            let detect_host_id = host_id.clone();
            let detect_window = self.window.as_ref().cloned();
            promise::spawn::spawn(async move {
                let domain = match Mux::get().get_domain_by_name(&name) {
                    Some(domain) => domain,
                    None => match crate::connect_domain_from_ssh_host(&name) {
                        Ok(domain) => domain,
                        Err(err) => {
                            log::error!("connect {name}: {err:#}");
                            return;
                        }
                    },
                };
                let attached = Arc::clone(&domain);
                if let Err(err) = crate::connect_domain_into_space(None, domain).await {
                    log::error!("connect {name}: {err:#}");
                    return;
                }
                if detect {
                    if let Some(distro) = attached
                        .as_ref()
                        .downcast_ref::<wezterm_client::domain::ClientDomain>()
                        .and_then(|client| client.remote_os_release())
                    {
                        if crate::ssh_hosts::set_host_distro(&detect_host_id, &distro) {
                            if let Some(window) = detect_window.as_ref() {
                                window.invalidate();
                            }
                        }
                    }
                }
            })
            .detach();
            return true;
        }

        self.snapshot_active_workspace_thread_layout();
        let use_mosh = spec.use_mosh;
        // Any Space can own projects now (a migrated collection simply
        // becomes a mixed Space), so the host project lands right here.
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
        let size = self.terminal_size;
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
                let _ = crate::workspace_threads::activate_thread_record(
                    &thread_id,
                    &live_workspaces,
                    true,
                );
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

    /// Whether this sidebar row shows a thread REFERENCE: the active Space
    /// holds a ref to it and its home is elsewhere (or unknown — a dangling
    /// ref). Row affordances differ: references are removed, own threads
    /// are deleted.
    fn sidebar_row_is_thread_ref(&self, thread_id: &str) -> bool {
        crate::workspace_threads::thread_ref_exists(&self.active_space_id, thread_id)
            && crate::workspace_threads::thread_space_id(thread_id).as_deref()
                != Some(self.active_space_id.as_str())
    }

    fn activate_workspace_thread_impl(
        &mut self,
        thread_id: String,
        context: &dyn WindowOps,
        orphan_candidate_window_id: Option<MuxWindowId>,
        workspaces_to_kill_after_adopt: Vec<String>,
    ) {
        let thread_home_space = crate::workspace_threads::thread_space_id(&thread_id);
        let in_this_space = thread_home_space.as_deref() == Some(self.active_space_id.as_str());
        // A window may activate any thread its Space references: the
        // reference opens in place while the thread keeps belonging to its
        // origin Space.
        let via_thread_ref = !in_this_space
            && crate::workspace_threads::thread_ref_exists(&self.active_space_id, &thread_id);
        if !in_this_space && !via_thread_ref {
            context.invalidate();
            return;
        }
        // Any valid selection supersedes an older detached local
        // materialization, including a selection that resolves synchronously
        // or takes a remote path. The old task may still finish, but it no
        // longer owns this window's visible state.
        let activation_generation = self.next_local_thread_activation_generation;
        self.next_local_thread_activation_generation = self
            .next_local_thread_activation_generation
            .saturating_add(1)
            .max(1);
        let activation_space_id = self.active_space_id.clone();
        self.local_thread_activation = None;
        if via_thread_ref {
            // Remember the choice so switching back to this Space restores
            // the reference, like a normal Space restores its active thread.
            crate::workspace_threads::note_thread_active_ref(&self.active_space_id, &thread_id);
        } else {
            // Activating one of the Space's own threads means the next
            // switch back should follow the normal pointers, not jump to a
            // previously opened reference.
            crate::workspace_threads::clear_thread_active_ref(&self.active_space_id);
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

        // Two GUI windows sharing one mux window fight over geometry (both
        // resize the same tabs). When another window is already showing the
        // target workspace — a collection window that opened it in place, the
        // origin-Space window while a collection shows it, or a connect
        // window displaying a workspace of a Space it did not claim — hand
        // the click to that window instead of adopting. Checked before any
        // record mutation so a handed-off click leaves no trace here.
        if let Some(plan) =
            crate::workspace_threads::activation_plan_for_thread(&thread_id, &live_workspaces)
        {
            if !plan.needs_materialize {
                let target =
                    crate::workspace_threads::window_to_show_in_workspace(&plan.workspace_name);
                if let Some(target) = target {
                    if target != self.mux_window_id {
                        if let Some(other) =
                            crate::frontend::front_end().gui_window_for_mux_window(target)
                        {
                            let workspace = plan.workspace_name.clone();
                            let project_path = plan.project_path.clone();
                            let project_id = plan.project_id.clone();
                            let thread_id = plan.thread_id.clone();
                            other.window.notify(TermWindowNotif::Apply(Box::new(
                                move |term_window| {
                                    term_window.open_project_terminal_in_live_workspace(
                                        workspace,
                                        project_path,
                                        project_id,
                                        thread_id,
                                        term_window.active_space_id.clone(),
                                    );
                                },
                            )));
                            other.window.focus();
                            context.invalidate();
                            return;
                        }
                    }
                }
            }
        }

        self.snapshot_active_workspace_thread_layout();
        self.workspace_sidebar_pending_thread_selection = None;
        self.set_content_view_active(false);

        let Some(plan) = crate::workspace_threads::activate_thread_record(
            &thread_id,
            &live_workspaces,
            !via_thread_ref,
        ) else {
            context.invalidate();
            return;
        };

        if !plan.needs_materialize {
            self.adopt_workspace_in_this_window(&plan.workspace_name);
            // This workspace may be live only because startup fell back to a
            // `$HOME` shell for a refused Project; add the terminal that was
            // actually asked for. Added rather than swapped: the fallback
            // shell may have been used, and killing its mux window would take
            // this GUI window down with it.
            self.open_project_terminal_in_live_workspace(
                plan.workspace_name.clone(),
                plan.project_path.clone(),
                plan.project_id.clone(),
                plan.thread_id.clone(),
                self.active_space_id.clone(),
            );
            // Symmetric with the materialize branch below: if this switch
            // orphaned a startup mux window, tidy it up. cleanup_orphaned_mux_window
            // is a no-op when the window is still shown or has a thread binding.
            cleanup_orphaned_mux_window(orphan_candidate_window_id);
            kill_workspace_windows(&workspaces_to_kill_after_adopt, Some(&plan.workspace_name));
            context.invalidate();
            return;
        }

        // In a mux-client-domain Space every thread targets the remote
        // server: panes spawn into the client domain, never a local shell.
        // That requires the domain to be attached first. For a collection
        // reference the thread's own Space decides the domain — the
        // collection Space itself is local and has none.
        let domain_space_id = if via_thread_ref {
            thread_home_space
                .clone()
                .unwrap_or_else(|| self.active_space_id.clone())
        } else {
            self.active_space_id.clone()
        };
        let space_client_domain =
            crate::workspace_threads::client_domain_for_space(&domain_space_id);
        if let Some(domain_name) = space_client_domain.as_deref() {
            let attached = mux
                .get_domain_by_name(domain_name)
                .map_or(false, |d| d.state() == mux::domain::DomainState::Attached);
            if !attached {
                // Attach on demand (auth prompts go through the ConnectionUI),
                // then re-run this activation once the domain is live.
                let Some(window) = self.window.clone() else {
                    context.invalidate();
                    return;
                };
                let domain_name = domain_name.to_string();
                let mux_window_id = self.mux_window_id;
                // Bridge the frames between this switch and the retry engine
                // raising its own in-flight flag: without it the sidebar
                // paints "Disconnected — Reconnect" for the gap.
                self.space_reconnects_in_flight.insert(domain_name.clone());
                promise::spawn::spawn(async move {
                    let result = async {
                        let mux = Mux::get();
                        // Host-store mux domains are only registered on
                        // demand (sshhost connect / CLI `connect`); build
                        // one from the SSH host store when switching into
                        // the Space cold, instead of failing the whole
                        // activation.
                        let domain = match mux.get_domain_by_name(&domain_name) {
                            Some(domain) => domain,
                            None => crate::connect_domain_from_ssh_host(&domain_name)?,
                        };
                        let ui = mux::connui::ConnectionUI::with_params(
                            mux::connui::ConnectionUIParams {
                                window_id: Some(mux_window_id),
                                ..Default::default()
                            },
                        );
                        let outcome = crate::attach_domain_with_retry(
                            domain,
                            Some(mux_window_id),
                            ui,
                            move || Mux::get().get_window(mux_window_id).is_some(),
                            Some(std::time::Duration::from_secs(60)),
                        )
                        .await?;
                        anyhow::Ok(matches!(outcome, crate::AttachRetryOutcome::Attached))
                    }
                    .await;
                    let cleanup_name = domain_name.clone();
                    match result {
                        // Cancelled (the hosting window went away mid-retry)
                        // leaves nothing to activate.
                        Ok(false) => {}
                        Ok(true) => {
                            window.notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                                if let Some(win) = term_window.window.clone() {
                                    term_window.activate_workspace_thread_impl(
                                        thread_id,
                                        &win,
                                        orphan_candidate_window_id,
                                        workspaces_to_kill_after_adopt,
                                    );
                                }
                            })));
                        }
                        Err(err) => {
                            log::error!("attaching {domain_name} for thread switch: {err:#}");
                        }
                    }
                    window.notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                        term_window.space_reconnects_in_flight.remove(&cleanup_name);
                        if let Some(win) = term_window.window.as_ref() {
                            win.invalidate();
                        }
                    })));
                })
                .detach();
                context.invalidate();
                return;
            }
        }

        let workspace_name = plan.workspace_name.clone();
        let remote_host_id =
            crate::workspace_threads::remote_host_id_for_project_id(&plan.project_id).to_string();
        let remote_spec = crate::ssh_hosts::host_spec(&remote_host_id);
        if missing_ssh_host_blocks_activation(
            crate::workspace_threads::project_is_remote(&plan.project_id),
            remote_spec.is_some(),
            space_client_domain.is_some(),
        ) {
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
        } else if let Some(domain_name) = space_client_domain {
            // Thread in a mux-domain Space: spawn on the remote server, with
            // the project path (a remote directory) as the starting cwd. The
            // domain's own "main" project carries a wezterm-mux:// sentinel
            // path, which is not a cwd.
            let cwd = plan
                .project_path
                .to_str()
                .filter(|path| !path.starts_with("wezterm-mux://"))
                .map(|path| path.to_string());
            (
                cwd,
                config::keyassignment::SpawnTabDomain::DomainName(domain_name),
            )
        } else {
            (
                plan.project_path.to_str().map(|path| path.to_string()),
                config::keyassignment::SpawnTabDomain::DefaultDomain,
            )
        };
        let layout = crate::workspace_threads::thread_layout(&plan.thread_id);
        let size = self.terminal_size;
        let term_config: Arc<dyn wezterm_term::TerminalConfiguration> =
            Arc::new(TermConfig::with_config(self.config.clone()));
        // Suppress the additive reconcile while we materialize the target
        // workspace's mux window; we adopt it into THIS window afterwards so
        // no duplicate window is spawned and no other window is disturbed.
        front_end().set_switching_workspaces(true);
        mux.set_active_workspace(&workspace_name);

        let cleanup_workspaces = workspaces_to_kill_after_adopt;
        let problem_thread_id = plan.thread_id.clone();
        let problem_display_name =
            crate::workspace_threads::thread_display_name(&plan.project_id, &plan.thread_id);
        self.local_thread_activation = Some(super::LocalThreadActivationState {
            generation: activation_generation,
            thread_id: plan.thread_id.clone(),
            space_id: activation_space_id.clone(),
            workspace_name: workspace_name.clone(),
        });
        let completion_window = self.window.clone();
        let completion_workspace = workspace_name.clone();
        promise::spawn::spawn(async move {
            let result = crate::workspace_threads::materialize_thread(
                workspace_name,
                layout,
                initial_cwd,
                size,
                None,
                term_config,
                default_domain,
                true,
            )
            .await;
            // Released inside the completion below, after the adopt.
            // Clearing it here reopens the additive-reconcile race: a
            // MuxNotification handler would find the active workspace's mux
            // window bound to no GUI window and spawn a second window.
            let Some(window) = completion_window else {
                front_end().set_switching_workspaces(false);
                return;
            };
            {
                window.notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                    term_window.finish_local_thread_activation(
                        activation_generation,
                        problem_thread_id,
                        activation_space_id,
                        completion_workspace,
                        result,
                        problem_display_name,
                        orphan_candidate_window_id,
                        cleanup_workspaces,
                        detect_remote_os,
                    );
                })));
            }
        })
        .detach();

        context.invalidate();
    }

    /// Add a terminal rooted in the Project to a workspace that is already
    /// live, and make it active.
    ///
    /// Strict about its directory: this exists precisely because the Project
    /// was refused once, so silently landing in `$HOME` again -- the very
    /// thing being repaired -- must fail loudly instead. A refusal reopens the
    /// recovery page with the current classification.
    fn open_project_terminal_in_live_workspace(
        &mut self,
        workspace: String,
        project_path: std::path::PathBuf,
        project_id: String,
        thread_id: String,
        space_id: String,
    ) {
        let Some(cwd) = project_path.to_str().map(str::to_string) else {
            return;
        };
        let mux = Mux::get();
        let Some(mux_window_id) = mux.iter_windows_in_workspace(&workspace).into_iter().next()
        else {
            return;
        };
        let Some(repair) = DeniedProjectFallbackRepair::begin(workspace.clone()) else {
            return;
        };
        let display_name = crate::workspace_threads::thread_display_name(&project_id, &thread_id);
        let size = self.terminal_size;
        let term_config: Arc<dyn wezterm_term::TerminalConfiguration> =
            Arc::new(TermConfig::with_config(self.config.clone()));
        let window = self.window.clone();
        promise::spawn::spawn(async move {
            let mut command = portable_pty::CommandBuilder::new_default_prog();
            command.set_require_cwd(true);
            let result = Mux::get()
                .spawn_tab_or_window(
                    Some(mux_window_id),
                    config::keyassignment::SpawnTabDomain::DefaultDomain,
                    Some(command),
                    Some(cwd),
                    size,
                    None,
                    workspace.clone(),
                    None,
                )
                .await;
            match result {
                Ok((_tab, pane, _window_id)) => {
                    pane.set_config(term_config);
                    repair.complete();
                    if let Some(window) = window {
                        window.notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                            term_window.invalidate_window();
                        })));
                    }
                }
                Err(err) => {
                    log::error!("open Project terminal in live workspace: {err:#}");
                    let failure = crate::termwindow::ui::folder_problem::ProjectRootUnavailable::from_error_chain(&err);
                    repair.retry();
                    if let Some(window) = window {
                        window.notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                            if let Some(failure) = failure {
                                term_window.show_project_root_problem(
                                    thread_id,
                                    space_id,
                                    display_name,
                                    failure,
                                );
                            } else {
                                term_window.invalidate_window();
                            }
                        })));
                    }
                }
            }
        })
        .detach();
    }

    #[allow(clippy::too_many_arguments)]
    fn finish_local_thread_activation(
        &mut self,
        generation: u64,
        thread_id: String,
        space_id: String,
        workspace_name: String,
        result: anyhow::Result<crate::workspace_threads::ThreadMaterializationOutcome>,
        display_name: String,
        orphan_candidate_window_id: Option<MuxWindowId>,
        workspaces_to_kill_after_adopt: Vec<String>,
        detect_remote_os: Option<(String, String)>,
    ) {
        let is_current = self.local_thread_activation.as_ref().is_some_and(|state| {
            local_thread_activation_matches(
                state,
                generation,
                &thread_id,
                &space_id,
                &workspace_name,
                &self.active_space_id,
            )
        });
        // Every exit below releases the switching guard and re-baselines the
        // layout; leaving either undone on a failed or overtaken activation
        // leaves a stuck guard and a stale fingerprint behind.
        let release = |term_window: &mut Self| {
            front_end().set_switching_workspaces(false);
            reconcile_workspace_layout_after_materialize(term_window.window.clone());
            term_window.invalidate_window();
        };

        if !is_current {
            // The materialized workspace is intentionally left live in the
            // background. A newer click owns the visible window now -- but the
            // cleanup this activation was carrying still has to happen, or the
            // orphan window and the workspaces it was told to kill leak for
            // the life of the process.
            cleanup_orphaned_mux_window(orphan_candidate_window_id);
            kill_workspace_windows(&workspaces_to_kill_after_adopt, None);
            release(self);
            return;
        }
        let state = self.local_thread_activation.take().unwrap();
        let workspace = state.workspace_name;

        if let Err(err) = result {
            log::error!("failed to materialize ThinkTerm thread: {err:#}");
            // The mux-level backstop refuses *inside* `spawn_tab_or_window`,
            // which leaves a tab-less window behind; that would make the
            // workspace look materialized and stop any retry.
            let mux = Mux::get();
            for window_id in mux.iter_windows_in_workspace(&workspace) {
                if mux
                    .get_window(window_id)
                    .is_some_and(|window| window.is_empty())
                {
                    mux.kill_window(window_id);
                }
            }
            if let Some(failure) =
                crate::termwindow::ui::folder_problem::ProjectRootUnavailable::from_error_chain(
                    &err,
                )
            {
                self.show_project_root_problem(thread_id, space_id, display_name, failure);
            }
            // The workspaces this activation was told to kill are dead
            // already (mirrors of a replaced server); a failed rebuild must
            // not keep them for the life of the process.
            kill_workspace_windows(&workspaces_to_kill_after_adopt, None);
            release(self);
            return;
        }

        if !self.adopt_workspace_in_this_window(&workspace) {
            log::error!("materialized ThinkTerm workspace {workspace:?} has no window to adopt");
            kill_workspace_windows(&workspaces_to_kill_after_adopt, None);
            release(self);
            return;
        }
        cleanup_orphaned_mux_window(orphan_candidate_window_id);
        kill_workspace_windows(&workspaces_to_kill_after_adopt, Some(&workspace));
        front_end().set_switching_workspaces(false);
        self.snapshot_active_workspace_thread_layout();
        self.remember_workspace_layout_structure_fingerprint();
        reconcile_workspace_layout_after_materialize(self.window.clone());
        self.invalidate_window();

        if let Some((detect_domain, detect_project)) = detect_remote_os {
            let detect_window = self.window.clone();
            promise::spawn::spawn(async move {
                if let Some(domain) = Mux::get().get_domain_by_name(&detect_domain) {
                    if let Some(ssh) = domain.as_ref().downcast_ref::<RemoteSshDomain>() {
                        if let Some(distro) = ssh.detect_os_release().await {
                            if crate::ssh_hosts::set_host_distro(&detect_project, &distro) {
                                if let Some(window) = detect_window {
                                    window.invalidate();
                                }
                            }
                        }
                    }
                }
            })
            .detach();
        }
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
        let size = self.terminal_size;
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
        let materialize_workspace_for_error = materialize_workspace.clone();
        let materialize_window = self.window.clone();
        promise::spawn::spawn(async move {
            if let Err(err) = crate::workspace_threads::materialize_thread(
                materialize_workspace,
                layout,
                None,
                size,
                None,
                term_config,
                config::keyassignment::SpawnTabDomain::DomainName(domain_name),
                true,
            )
            .await
            {
                log::error!("failed to materialize connecting SSH thread: {err:#}");
                let message = format!("Unable to open the remote terminal: {err:#}");
                if let Some(window) = materialize_window {
                    window.notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                        let is_current = term_window
                            .remote_connects
                            .get(&view_id)
                            .is_some_and(|state| state.generation == generation);
                        if !is_current {
                            return;
                        }
                        term_window.kill_remote_connect_workspace(&materialize_workspace_for_error);
                        term_window.fail_remote_connect(view_id, &message);
                    })));
                }
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
        let workspace_has_window = !Mux::get()
            .iter_windows_in_workspace(&workspace_name)
            .is_empty();
        let can_reveal = remote_connect_can_reveal(status.as_ref(), workspace_has_window);

        match status {
            Some(SshConnectionStatus::Authenticating | SshConnectionStatus::Connected)
                if can_reveal =>
            {
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
            ContextMenuIcon::Edit,
            KeyAssignment::PromptRenamePaneTab(pane_id),
        )]
    }

    pub fn mouse_event_pane_nav(
        &mut self,
        item: UIItem,
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
                } else if let Some(pane) = mux.get_pane(target_pane_id) {
                    // For a remote mux pane, mirror the activation on the
                    // server so its notion of the stack's visible pane
                    // matches ours; otherwise the next resync would flip
                    // the local stack back to the server's stale value.
                    if let Some(client_pane) =
                        pane.downcast_ref::<wezterm_client::pane::ClientPane>()
                    {
                        client_pane.activate_in_stack_on_server();
                    }
                }
                self.arm_pane_tab_drag(item, target_pane_id, event);
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
                self.claim_frontend_viewport_for_interaction();
                if let Some(tab) = mux.get_active_tab_for_window(self.mux_window_id) {
                    mux::zoom_trace!(
                        "gui.gesture site=PaneNavToggleZoom tab={} pane={pane_id} \
                         index={pane_index} zoom={}",
                        tab.tab_id(),
                        tab.get_zoomed_pane().is_some()
                    );
                    tab.set_active_idx(pane_index);
                    tab.toggle_zoom();
                }
                self.sync_active_tab_geometry_now();
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
                self.sync_active_tab_geometry_now();
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
                    term_window.sync_active_tab_geometry_now();
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
                    domain: crate::local_sessions::local_spawn_domain(),
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
            crate::i18n::tr("menu-rename-tab"),
            ContextMenuIcon::Edit,
            Self::tab_context_action(tab_idx, KeyAssignment::PromptRenameTab),
        )];

        let mut close_items = vec![];
        if let Some(action) = Self::close_tabs_to_left_action(tab_idx) {
            close_items.push(ContextMenuItem::item_with_icon(
                crate::i18n::tr("menu-close-tabs-left"),
                ContextMenuIcon::Close,
                action,
            ));
        }
        if let Some(action) = Self::close_tabs_to_right_action(tab_idx, tab_count) {
            close_items.push(ContextMenuItem::item_with_icon(
                crate::i18n::tr("menu-close-tabs-right"),
                ContextMenuIcon::Close,
                action,
            ));
        }
        if let Some(action) = Self::close_other_tabs_action(tab_idx, tab_count) {
            close_items.push(ContextMenuItem::item_with_icon(
                crate::i18n::tr("menu-close-other-tabs"),
                ContextMenuIcon::Close,
                action,
            ));
        }
        if !close_items.is_empty() {
            items.push(ContextMenuItem::Separator);
            items.append(&mut close_items);
        }

        let mut move_items = vec![];
        if tab_idx > 0 {
            move_items.push(ContextMenuItem::item_with_icon(
                crate::i18n::tr("menu-move-tab-left"),
                ContextMenuIcon::MoveLeft,
                Self::tab_context_action(tab_idx, KeyAssignment::MoveTab(tab_idx - 1)),
            ));
        }
        if tab_idx + 1 < tab_count {
            move_items.push(ContextMenuItem::item_with_icon(
                crate::i18n::tr("menu-move-tab-right"),
                ContextMenuIcon::MoveRight,
                Self::tab_context_action(tab_idx, KeyAssignment::MoveTab(tab_idx + 1)),
            ));
        }
        if !move_items.is_empty() {
            items.push(ContextMenuItem::Separator);
            items.append(&mut move_items);
        }

        items.push(ContextMenuItem::Separator);
        items.push(ContextMenuItem::item_with_icon(
            crate::i18n::tr("menu-new-terminal-tab-right"),
            ContextMenuIcon::Terminal,
            Self::tab_context_action(
                tab_idx,
                KeyAssignment::SpawnTabToRight(SpawnTabDomain::CurrentPaneDomain),
            ),
        ));
        items.push(ContextMenuItem::Separator);
        items.push(ContextMenuItem::item_with_icon(
            crate::i18n::tr("menu-zoom-pane"),
            ContextMenuIcon::Expand,
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

    fn terminal_context_menu_items(&mut self) -> Vec<ContextMenuItem> {
        fn split_item(
            label: String,
            icon: ContextMenuIcon,
            direction: PaneDirection,
        ) -> ContextMenuItem {
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

        self.begin_context_menu_application_actions();
        let mut items = vec![
            ContextMenuItem::item_with_icon(
                crate::i18n::tr("menu-copy"),
                ContextMenuIcon::Copy,
                KeyAssignment::CopyTo(ClipboardCopyDestination::Clipboard),
            ),
            ContextMenuItem::item_with_icon(
                crate::i18n::tr("menu-paste"),
                ContextMenuIcon::Paste,
                KeyAssignment::PasteFrom(ClipboardPasteSource::Clipboard),
            ),
            ContextMenuItem::Separator,
            split_item(
                crate::i18n::tr("menu-split-right"),
                ContextMenuIcon::SplitHorizontal,
                PaneDirection::Right,
            ),
            split_item(
                crate::i18n::tr("menu-split-left"),
                ContextMenuIcon::SplitHorizontal,
                PaneDirection::Left,
            ),
            split_item(
                crate::i18n::tr("menu-split-down"),
                ContextMenuIcon::SplitVertical,
                PaneDirection::Down,
            ),
            split_item(
                crate::i18n::tr("menu-split-up"),
                ContextMenuIcon::SplitVertical,
                PaneDirection::Up,
            ),
            ContextMenuItem::Separator,
            ContextMenuItem::item_with_icon(
                crate::i18n::tr("menu-reset-terminal"),
                ContextMenuIcon::Refresh,
                KeyAssignment::ResetTerminal,
            ),
        ];
        let current_mode = self
            .active_frontend_access_state()
            .map(|state| state.mode)
            .unwrap_or(mux::FrontendAccessMode::Handoff);
        let modes = [
            (
                "A · Shared (tmux-like)",
                codec::FrontendAccessMode::TmuxLatest,
                mux::FrontendAccessMode::TmuxLatest,
            ),
            (
                "B · Handoff (exclusive)",
                codec::FrontendAccessMode::Handoff,
                mux::FrontendAccessMode::Handoff,
            ),
        ];
        let children = modes
            .iter()
            .copied()
            .map(|(label, codec_mode, mux_mode)| {
                self.context_menu_application_item_with_icon(
                    label,
                    if current_mode == mux_mode {
                        ContextMenuIcon::Check
                    } else {
                        ContextMenuIcon::Terminal
                    },
                    crate::termwindow::ContextMenuApplicationAction::SetFrontendAccessMode(
                        codec_mode,
                    ),
                    true,
                )
            })
            .collect();
        items.push(ContextMenuItem::Separator);
        items.push(ContextMenuItem::submenu_with_icon(
            "Frontend access",
            ContextMenuIcon::Terminal,
            children,
        ));
        items
    }

    fn right_sidebar_note_context_menu_items(
        &mut self,
        anchor: window::Point,
    ) -> Vec<ContextMenuItem> {
        use crate::termwindow::{ContextMenuApplicationAction, NoteEditorCommand};

        let Some(session) = self.right_sidebar_note.session.clone() else {
            return vec![];
        };
        let session = session.lock();
        let editable =
            self.right_sidebar_note.view.mode != crate::markdown_editor::EditorMode::ReadOnly;
        let has_selection = !self.right_sidebar_note.view.selection.is_caret();
        let has_text = !session.source().is_empty();
        let can_undo = editable && session.can_undo();
        let can_redo = editable && session.can_redo();
        let revision = session.revision();
        let selected_range = self.right_sidebar_note.view.selection.range();
        let spelling_issue = self
            .right_sidebar_note
            .spelling_issues
            .iter()
            .find(|issue| {
                issue.source.start < selected_range.end && selected_range.start < issue.source.end
            })
            .cloned();
        let lookup_text = session
            .selected_text(&self.right_sidebar_note.view)
            .map(str::to_string)
            .filter(|text| !text.trim().is_empty());
        drop(session);

        self.begin_context_menu_application_actions();
        let mut items = Vec::new();
        if let Some(issue) = spelling_issue {
            for suggestion in issue.suggestions.iter().take(5) {
                items.push(self.context_menu_application_item_with_icon(
                    suggestion.clone(),
                    ContextMenuIcon::Spellcheck,
                    ContextMenuApplicationAction::Note(NoteEditorCommand::ReplaceSpelling {
                        revision,
                        range: issue.source.clone(),
                        replacement: suggestion.clone(),
                    }),
                    editable,
                ));
            }
            if !issue.suggestions.is_empty() {
                items.push(ContextMenuItem::Separator);
            }
            items.push(self.context_menu_application_item_with_icon(
                crate::i18n::tr("menu-ignore-spelling"),
                ContextMenuIcon::Spellcheck,
                ContextMenuApplicationAction::Note(NoteEditorCommand::IgnoreSpelling {
                    word: issue.word.clone(),
                }),
                true,
            ));
            items.push(self.context_menu_application_item_with_icon(
                crate::i18n::tr("menu-learn-spelling"),
                ContextMenuIcon::Spellcheck,
                ContextMenuApplicationAction::Note(NoteEditorCommand::LearnSpelling {
                    word: issue.word,
                }),
                true,
            ));
            items.push(ContextMenuItem::Separator);
        }
        if let Some(text) = lookup_text {
            items.push(self.context_menu_application_item_with_icon(
                crate::i18n::tr("menu-look-up"),
                ContextMenuIcon::Search,
                ContextMenuApplicationAction::Note(NoteEditorCommand::LookUp {
                    text,
                    anchor: window::Rect::new(anchor, window::Size::new(1, 1)),
                }),
                true,
            ));
            items.push(ContextMenuItem::Separator);
        }
        items.push(self.context_menu_application_item_with_icon(
            crate::i18n::tr("menu-undo"),
            ContextMenuIcon::Undo,
            ContextMenuApplicationAction::Note(NoteEditorCommand::Undo),
            can_undo,
        ));
        items.push(self.context_menu_application_item_with_icon(
            crate::i18n::tr("menu-redo"),
            ContextMenuIcon::Redo,
            ContextMenuApplicationAction::Note(NoteEditorCommand::Redo),
            can_redo,
        ));
        items.push(ContextMenuItem::Separator);
        items.push(self.context_menu_application_item_with_icon(
            crate::i18n::tr("menu-cut"),
            ContextMenuIcon::Cut,
            ContextMenuApplicationAction::Note(NoteEditorCommand::Cut),
            editable && has_selection,
        ));
        items.push(self.context_menu_application_item_with_icon(
            crate::i18n::tr("menu-copy"),
            ContextMenuIcon::Copy,
            ContextMenuApplicationAction::Note(NoteEditorCommand::Copy),
            has_selection,
        ));
        items.push(self.context_menu_application_item_with_icon(
            crate::i18n::tr("menu-paste"),
            ContextMenuIcon::Paste,
            ContextMenuApplicationAction::Note(NoteEditorCommand::Paste),
            editable,
        ));
        items.push(self.context_menu_application_item_with_icon(
            crate::i18n::tr("menu-delete"),
            ContextMenuIcon::Delete,
            ContextMenuApplicationAction::Note(NoteEditorCommand::Delete),
            editable && has_selection,
        ));
        items.push(ContextMenuItem::Separator);
        items.push(self.context_menu_application_item_with_icon(
            crate::i18n::tr("menu-select-all"),
            ContextMenuIcon::Check,
            ContextMenuApplicationAction::Note(NoteEditorCommand::SelectAll),
            has_text,
        ));
        items
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
                // Less than a line of travel, but the device said how much:
                // smooth scrolling moves the grid by exactly that, so the
                // wheel binding still has to fire. An application that
                // reports the mouse gets whole notches only, as before.
                0 if !pane.is_mouse_grabbed() && sub_line_wheel_direction(&event) != 0 => {
                    MouseEventTrigger::Down {
                        streak: 1,
                        button: if sub_line_wheel_direction(&event) > 0 {
                            MouseButton::WheelUp(1)
                        } else {
                            MouseButton::WheelDown(1)
                        },
                    }
                }
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
            let items = self.terminal_context_menu_items();
            self.show_term_context_menu(context, event.coords, items);
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

fn local_thread_activation_matches(
    state: &super::LocalThreadActivationState,
    generation: u64,
    thread_id: &str,
    space_id: &str,
    workspace_name: &str,
    active_space_id: &str,
) -> bool {
    state.generation == generation
        && state.thread_id == thread_id
        && state.space_id == space_id
        && state.workspace_name == workspace_name
        && state.space_id == active_space_id
}

#[cfg(test)]
mod local_thread_activation_tests {
    use super::local_thread_activation_matches;
    use crate::termwindow::LocalThreadActivationState;

    fn state() -> LocalThreadActivationState {
        LocalThreadActivationState {
            generation: 7,
            thread_id: "thread-a".to_string(),
            space_id: "space-a".to_string(),
            workspace_name: "workspace-a".to_string(),
        }
    }

    #[test]
    fn only_the_exact_current_activation_may_complete() {
        let state = state();
        assert!(local_thread_activation_matches(
            &state,
            7,
            "thread-a",
            "space-a",
            "workspace-a",
            "space-a"
        ));
        assert!(!local_thread_activation_matches(
            &state,
            6,
            "thread-a",
            "space-a",
            "workspace-a",
            "space-a"
        ));
        assert!(!local_thread_activation_matches(
            &state,
            7,
            "thread-b",
            "space-a",
            "workspace-a",
            "space-a"
        ));
        assert!(!local_thread_activation_matches(
            &state,
            7,
            "thread-a",
            "space-a",
            "workspace-a",
            "space-b"
        ));
        assert!(!local_thread_activation_matches(
            &state,
            7,
            "thread-a",
            "space-a",
            "workspace-b",
            "space-a"
        ));
    }
}

/// Adopt a materialized workspace into the window that initiated an operation
/// without disturbing any other GUI window already bound to the mux.
pub(crate) fn adopt_workspace_into_window(window: &Option<::window::Window>, workspace: &str) {
    let mux = Mux::get();
    let Some(target) = crate::workspace_threads::window_to_show_in_workspace(workspace) else {
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

/// A project's draggable extent is its whole BLOCK: the header row plus
/// every thread row rendered under it (nothing when collapsed). Gaps then
/// sit between whole workspaces, so "after X" paints below X's last thread
/// instead of between X's header and its threads. Relies on the sidebar
/// pushing items in visual order: each Project row followed by its rows,
/// with the cross-project pinned section preceding the first Project.
pub(crate) fn folder_block_extents(
    items: &[UIItem],
    space_id: &str,
) -> Vec<(String, isize, isize)> {
    let collapse_prefix = format!("{space_id}::");
    let mut blocks: Vec<(String, isize, isize)> = vec![];
    for item in items {
        match &item.item_type {
            UIItemType::Project(id) => {
                blocks.push((id.clone(), item.y as isize, (item.y + item.height) as isize))
            }
            UIItemType::ThreadRefGroupToggle { key, .. } => {
                let raw = key.strip_prefix(&collapse_prefix).unwrap_or(key.as_str());
                blocks.push((
                    raw.to_string(),
                    item.y as isize,
                    (item.y + item.height) as isize,
                ))
            }
            UIItemType::WorkspaceThread(_)
            | UIItemType::WorkspaceThreadNew(_)
            | UIItemType::ThreadRefGroupNewThread(_) => {
                if let Some(block) = blocks.last_mut() {
                    block.2 = block.2.max((item.y + item.height) as isize);
                }
            }
            _ => {}
        }
    }
    blocks
}

/// "After the last VISIBLE row" only means "the end of the list" when that
/// row is also the logically last sibling. With the list scrolled, rendered
/// rows stop at the viewport edge while the store's order continues, so the
/// end-of-viewport gap must anchor to the first off-screen sibling instead.
pub(crate) fn clamp_end_anchor_to_next_logical_sibling(
    before: Option<String>,
    last_visible: Option<&str>,
    logical: &[String],
) -> Option<String> {
    if before.is_some() {
        return before;
    }
    last_visible.and_then(|last| {
        logical
            .iter()
            .position(|id| id == last)
            .and_then(|index| logical.get(index + 1).cloned())
    })
}

/// Which gap of a row list a pointer at `pointer_y` selects: insert before
/// the returned id, or at the end when None. The second value is where the
/// insert indicator line paints. Rows are (id, top, bottom) in render order;
/// the upper half of a row aims before it, the lower half after it.
pub(crate) fn sidebar_insert_position(
    rows: &[(String, isize, isize)],
    pointer_y: isize,
) -> Option<(Option<String>, isize)> {
    for (id, top, bottom) in rows {
        let mid = (top + bottom) / 2;
        if pointer_y < mid {
            return Some((Some(id.clone()), *top));
        }
    }
    rows.last().map(|(_, _, bottom)| (None, *bottom))
}

#[cfg(test)]
mod sidebar_drag_tests {
    use super::{
        clamp_end_anchor_to_next_logical_sibling, folder_block_extents, sidebar_insert_position,
    };
    use crate::termwindow::{UIItem, UIItemType};

    /// The scrolled-list regression: dropping after the last VISIBLE row
    /// while more siblings sit below the viewport must land before those
    /// siblings, not at the absolute end of the list.
    #[test]
    fn an_end_of_viewport_drop_anchors_before_offscreen_siblings() {
        let logical: Vec<String> = ["a", "b", "c"].iter().map(|s| s.to_string()).collect();
        // c is scrolled out of view: the gap below b anchors to c.
        assert_eq!(
            clamp_end_anchor_to_next_logical_sibling(None, Some("b"), &logical),
            Some("c".to_string())
        );
        // b IS the true end: None stays None.
        assert_eq!(
            clamp_end_anchor_to_next_logical_sibling(None, Some("c"), &logical),
            None
        );
        // A concrete anchor passes through untouched.
        assert_eq!(
            clamp_end_anchor_to_next_logical_sibling(Some("b".to_string()), Some("a"), &logical),
            Some("b".to_string())
        );
    }

    fn rows() -> Vec<(String, isize, isize)> {
        vec![
            ("a".to_string(), 0, 20),
            ("b".to_string(), 24, 44),
            ("c".to_string(), 48, 68),
        ]
    }

    fn item(item_type: UIItemType, y: usize, height: usize) -> UIItem {
        UIItem {
            x: 0,
            y,
            width: 200,
            height,
            item_type,
        }
    }

    /// The regression from live testing: with expanded threads, "after a
    /// project" must mean below its LAST THREAD, not below its header row —
    /// and the pinned section above the first project is no block at all.
    #[test]
    fn a_project_block_runs_through_its_threads() {
        let items = vec![
            item(UIItemType::WorkspaceThread("pinned".into()), 40, 20),
            item(UIItemType::Project("alpha".into()), 100, 20),
            item(UIItemType::WorkspaceThread("a1".into()), 124, 20),
            item(UIItemType::WorkspaceThread("a2".into()), 148, 20),
            item(UIItemType::Project("beta".into()), 176, 20),
            // collapsed: no thread rows follow
            item(UIItemType::WorkspaceSidebarBackground, 0, 600),
        ];
        assert_eq!(
            folder_block_extents(&items, "space-x"),
            vec![
                ("alpha".to_string(), 100, 168),
                ("beta".to_string(), 176, 196),
            ]
        );
        // Below beta: the end, with the line under the last block.
        assert_eq!(
            sidebar_insert_position(&folder_block_extents(&items, "space-x"), 500),
            Some((None, 196))
        );
    }

    /// A ref-group header opens a block exactly like a project header, its
    /// key stripped of the collapse-state space prefix; the group's thread
    /// rows and "+" extend the block just like a project's do.
    #[test]
    fn a_ref_group_block_interleaves_with_project_blocks() {
        let items = vec![
            item(UIItemType::Project("alpha".into()), 100, 20),
            item(UIItemType::WorkspaceThread("a1".into()), 124, 20),
            item(
                UIItemType::ThreadRefGroupToggle {
                    key: "space-x::origin-project".into(),
                    origin: String::new(),
                },
                176,
                20,
            ),
            item(UIItemType::WorkspaceThread("r1".into()), 200, 20),
            item(UIItemType::Project("beta".into()), 252, 20),
        ];
        assert_eq!(
            folder_block_extents(&items, "space-x"),
            vec![
                ("alpha".to_string(), 100, 144),
                ("origin-project".to_string(), 176, 220),
                ("beta".to_string(), 252, 272),
            ]
        );
    }

    #[test]
    fn a_pointer_picks_the_gap_nearest_to_it() {
        // Above everything and in the first row's upper half: before "a".
        assert_eq!(
            sidebar_insert_position(&rows(), -10),
            Some((Some("a".to_string()), 0))
        );
        assert_eq!(
            sidebar_insert_position(&rows(), 4),
            Some((Some("a".to_string()), 0))
        );
        // Lower half of "a" aims after it = before "b".
        assert_eq!(
            sidebar_insert_position(&rows(), 15),
            Some((Some("b".to_string()), 24))
        );
        // Lower half of the last row, and anywhere below: the end.
        assert_eq!(sidebar_insert_position(&rows(), 60), Some((None, 68)));
        assert_eq!(sidebar_insert_position(&rows(), 500), Some((None, 68)));
        // No rows: nowhere to drop.
        assert_eq!(sidebar_insert_position(&[], 10), None);
    }
}

#[cfg(test)]
mod space_menu_tests {
    use super::space_destructive_menu_items;
    use crate::workspace_threads::SpaceView;
    use config::keyassignment::KeyAssignment;
    use window::ContextMenuAction;
    use window::ContextMenuItem;

    fn space(is_remote: bool) -> SpaceView {
        SpaceView {
            id: "s1".to_string(),
            name: "Remote 2".to_string(),
            is_active: true,
            is_default: false,
            is_occupied_by_other_window: false,
            is_remote,
            domain: is_remote.then(|| "DO SYD X user".to_string()),
            is_domain_attached: false,
        }
    }

    /// The submenu's shape is the contract; the labels come from whichever
    /// locale happens to be active, so asserting on them would be asserting on
    /// the test machine's settings.
    fn actions(items: &[ContextMenuItem]) -> Vec<KeyAssignment> {
        items
            .iter()
            .map(|item| match item {
                ContextMenuItem::Item {
                    action: ContextMenuAction::KeyAssignment(action),
                    ..
                } => action.clone(),
                other => panic!("expected a key assignment item, got {:?}", other),
            })
            .collect()
    }

    fn submenu_of(item: &ContextMenuItem) -> &[ContextMenuItem] {
        match item {
            ContextMenuItem::Item { submenu, .. } => submenu,
            other => panic!("expected an item, got {:?}", other),
        }
    }

    /// One action does not deserve a submenu: a local Space keeps the single
    /// named row it has always had.
    #[test]
    fn a_local_space_stays_flat() {
        let items = space_destructive_menu_items(&space(false), false);
        assert_eq!(items.len(), 1);
        assert!(submenu_of(&items[0]).is_empty());
        assert_eq!(
            actions(&items),
            vec![KeyAssignment::DeleteSpace("s1".to_string())]
        );
    }

    /// The four lines that prompted this: one parent row carrying the name,
    /// with the actions underneath.
    #[test]
    fn a_remote_space_folds_its_actions_into_one_submenu() {
        let items = space_destructive_menu_items(&space(true), true);
        assert_eq!(items.len(), 1);
        assert_eq!(
            actions(submenu_of(&items[0])),
            vec![
                KeyAssignment::DeleteSpace("s1".to_string()),
                KeyAssignment::DeleteSpaceEverywhere("s1".to_string()),
                KeyAssignment::DeleteSpaceAndRemoteSessions("s1".to_string()),
            ]
        );
    }

    /// Ending the remote sessions needs a live connection to end them over, so
    /// a detached domain offers only the two that work offline.
    #[test]
    fn a_detached_remote_space_omits_the_session_kill() {
        let items = space_destructive_menu_items(&space(true), false);
        assert_eq!(
            actions(submenu_of(&items[0])),
            vec![
                KeyAssignment::DeleteSpace("s1".to_string()),
                KeyAssignment::DeleteSpaceEverywhere("s1".to_string()),
            ]
        );
    }
}

#[cfg(test)]
mod adaptive_tab_width_tests {
    use super::adaptive_tab_width_pixels;

    const TARGET: f32 = 300.0;
    const FLOOR: f32 = 160.0;
    const GAP: f32 = 18.0;

    fn width(count: usize, viewport: f32) -> f32 {
        adaptive_tab_width_pixels(count, viewport, GAP, TARGET, FLOOR)
    }

    /// A roomy row does not stretch its tabs to fill it: two tabs in a wide
    /// window stay at the target width, the way browser tab strips behave.
    #[test]
    fn a_roomy_row_parks_every_tab_at_the_target_width() {
        assert_eq!(width(1, 2000.0), TARGET);
        assert_eq!(width(2, 2000.0), TARGET);
        assert_eq!(width(5, 2000.0), TARGET);
    }

    /// Past the point where targets fit, the tabs give up width together.
    #[test]
    fn crowding_shrinks_the_tabs_evenly() {
        let w = width(8, 1600.0);
        assert!(w < TARGET, "{w} should be under the target");
        assert!(w > FLOOR, "{w} should still be over the floor");
        // 8 tabs and 7 gaps have to land inside the row.
        assert!(8.0 * w + 7.0 * GAP <= 1600.0);
    }

    /// Exactly at the fit boundary the row must not leave a stray pixel of
    /// scroll behind — that is what the floor() in the divide is for.
    #[test]
    fn a_row_that_just_fits_leaves_no_phantom_scroll() {
        for count in 2..=40usize {
            for viewport in [640.0f32, 977.0, 1280.0, 1441.0, 2560.0] {
                let w = width(count, viewport);
                let total = count as f32 * w + (count - 1) as f32 * GAP;
                if w > FLOOR {
                    assert!(
                        total <= viewport,
                        "count={count} viewport={viewport} width={w} total={total}"
                    );
                }
            }
        }
    }

    /// Below the floor the tabs stop shrinking and the row overflows instead,
    /// which is the signal for the caller to scroll.
    #[test]
    fn the_floor_holds_and_hands_the_overflow_to_the_scroller() {
        let w = width(40, 1200.0);
        assert_eq!(w, FLOOR);
        assert!(40.0 * w + 39.0 * GAP > 1200.0);
    }

    /// A window narrower than one tab (or with no room at all) still reports a
    /// usable width rather than 0 or a negative.
    #[test]
    fn a_starved_row_still_reports_the_floor() {
        assert_eq!(width(3, 0.0), FLOOR);
        assert_eq!(width(3, -50.0), FLOOR);
        assert_eq!(width(0, 800.0), TARGET);
    }

    /// The floor is advisory: a design that set it above the target must not
    /// produce a tab wider than the target.
    #[test]
    fn a_floor_above_the_target_never_wins() {
        assert_eq!(
            adaptive_tab_width_pixels(6, 400.0, GAP, 100.0, 250.0),
            100.0
        );
    }
}

/// Which way a wheel event that travelled less than a whole line was
/// going, from the finer data the device supplied: positive is up (towards
/// older rows), 0 when there is no such data.
fn sub_line_wheel_direction(event: &::window::MouseEvent) -> i8 {
    let travel = event
        .precise_scroll_delta
        .map(|delta| delta.y)
        .filter(|y| *y != 0.0)
        .or(event.precise_wheel_lines.filter(|lines| *lines != 0.0));
    match travel {
        Some(value) if value > 0.0 => 1,
        Some(_) => -1,
        None => 0,
    }
}
