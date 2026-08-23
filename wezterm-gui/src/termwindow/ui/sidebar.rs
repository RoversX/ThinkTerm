use crate::quad::{QuadTrait, TripleLayerQuadAllocator, TripleLayerQuadAllocatorTrait};
use crate::termwindow::render::corners::{
    BOTTOM_LEFT_ROUNDED_CORNER, BOTTOM_RIGHT_ROUNDED_CORNER, TOP_LEFT_ROUNDED_CORNER,
    TOP_RIGHT_ROUNDED_CORNER,
};
use crate::termwindow::ui::icons::{distro_to_icon, BrandIcon, MaterialIcon, SvgIcon};
use crate::termwindow::ui::platform_chrome;
use crate::termwindow::ui::status_icon::UiStatusKind;
use crate::termwindow::ui::tokens::{
    CAPSULE_BORDER_WIDTH, SIDEBAR_ICON_GAP, SIDEBAR_INSET, SIDEBAR_MAX_WIDTH, SIDEBAR_MIN_WIDTH,
    SIDEBAR_RESIZE_HANDLE_WIDTH, SIDEBAR_ROW_GAP, SIDEBAR_ROW_RADIUS, SIDEBAR_WIDTH_CELLS,
    WINDOW_TAB_FULLSCREEN_NEW_SESSION_EXTRA_HEIGHT, WINDOW_TAB_FULLSCREEN_NEW_SESSION_Y_OFFSET,
    WINDOW_TAB_FULLSCREEN_SIDEBAR_BUTTON_X, WINDOW_TAB_FULLSCREEN_SIDEBAR_BUTTON_Y_OFFSET,
    WINDOW_TAB_TOP_SPACER,
};
use crate::termwindow::{UIItem, UIItemType};
use crate::ui::{scale_ui_f32, scale_ui_usize, unscale_ui_usize, UiPalette};
use crate::utilsprites::RenderMetrics;
use crate::workspace_threads;
use anyhow::Context;
use mux::Mux;
use std::borrow::Cow;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::{Duration, Instant};
use wezterm_font::LoadedFont;
use window::color::LinearRgba;
use window::{MouseEventKind as WMEK, RectF};

const SIDEBAR_SCROLLBAR_VISIBLE_MS: u64 = 900;
const SIDEBAR_SETTINGS_FOOTER_HEIGHT: usize = 72;
const SIDEBAR_SETTINGS_FADE_HEIGHT: usize = 52;
const SIDEBAR_LIST_BOTTOM_PADDING: usize = 65;
const SIDEBAR_SETTINGS_ROW_TOP_PADDING: usize = 4;
const SIDEBAR_SETTINGS_ROW_BOTTOM_PADDING: usize = 12;
const SIDEBAR_SETTINGS_ROW_SIDE_PADDING: usize = 16;
const SIDEBAR_SETTINGS_ICON_EXTRA_INSET: usize = 18;
const SIDEBAR_SETTINGS_ROW_LIFT: usize = 18;
const SIDEBAR_TOP_FADE_HEIGHT: usize = 32;
const WORKSPACE_GROUP_EXTRA_GAP: usize = 8;
const WORKSPACE_SECTION_LABEL_GAP: usize = 12;
const SESSION_ROW_MIN_HEIGHT: usize = 66;
const SESSION_ROW_SIDE_PADDING: usize = 10;
const SESSION_ACTION_MIN_SIZE: usize = 48;
const SESSION_ACTION_MAX_SIZE: usize = 52;
const SESSION_ACTION_ICON_INSET: usize = 12;
const SESSION_STATUS_DOT_SIZE: usize = 10;
const SESSION_STATUS_ICON_SIZE: usize = 20;
const SESSION_STATUS_ACTIVE_ICON_SIZE: usize = 26;
const SESSION_STATUS_DONE_COLOR: LinearRgba = LinearRgba::with_components(0.20, 0.78, 0.36, 1.0);
const SESSION_STATUS_OPEN_COLOR: LinearRgba = LinearRgba::with_components(0.12, 0.48, 1.0, 1.0);
const SPACE_DISCONNECTED_COLOR: LinearRgba = LinearRgba::with_components(0.86, 0.45, 0.12, 1.0);
const NOTIFICATION_BADGE_COLOR: LinearRgba = LinearRgba::with_components(0.96, 0.16, 0.22, 1.0);
const NOTIFICATION_BADGE_PULSE_DURATION: Duration = Duration::from_millis(1800);
const NOTIFICATION_BADGE_PULSE_COUNT: f32 = 3.0;
const NOTIFICATION_BADGE_FRAME_MS: u64 = 33;
const SIDEBAR_SECTION_ACTION_SIZE: usize = 48;
const SIDEBAR_SECTION_ACTION_ICON_INSET: usize = 6;

/// Connection health of the active Space's mux client domain, as shown by
/// the sidebar indicator. Local Spaces are always Connected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SpaceConnectionState {
    Connected,
    /// An attach is in flight (initial connect or manual re-attach); not an
    /// error state, so no alert icon and no Reconnect button.
    Connecting,
    /// Transport lost; the automatic retry loop is running.
    Reconnecting,
    /// Not connected and nothing is retrying (retry loop parked after
    /// sustained failure, domain detached, or never connected); the
    /// sidebar offers a Reconnect button.
    Disconnected,
}

#[derive(Debug, Clone, Copy)]
pub struct WorkspaceSidebarRect {
    pub x: usize,
    pub y: usize,
    pub width: usize,
    pub height: usize,
}

#[derive(Debug, Clone, Copy)]
pub struct WorkspaceSidebarScrollGeometry {
    pub track_x: usize,
    pub track_y: usize,
    pub track_width: usize,
    pub track_height: usize,
    pub thumb_y: f32,
    pub thumb_height: f32,
    pub max_scroll: f32,
}

#[derive(Debug, Clone, Copy)]
struct WorkspaceSidebarLayout {
    panel_x: usize,
    panel_y: usize,
    panel_width: usize,
    panel_height: usize,
    content_bottom: usize,
    settings_footer_y: usize,
    settings_footer_height: usize,
    row_height: usize,
    show_sidebar_toolbar: bool,
    space_menu_y: usize,
    space_menu_height: usize,
    /// Height of the "Reconnect" row below the Space menu; non-zero only
    /// while the Space's mux domain is Disconnected (a stable state — the
    /// row never flickers in and out on transient lag).
    reconnect_row_height: usize,
    top_action_y_offset: usize,
    top_action_height: usize,
    list_top: usize,
}

pub fn workspace_sidebar_width_for_metrics(render_metrics: &RenderMetrics, dpi: usize) -> usize {
    let min_width = scale_ui_usize(SIDEBAR_MIN_WIDTH, dpi);
    let max_width = scale_ui_usize(SIDEBAR_MAX_WIDTH, dpi);
    let default_width =
        (render_metrics.cell_size.width as usize * SIDEBAR_WIDTH_CELLS).max(min_width);
    crate::native_settings::workspace_sidebar_width()
        .map(|width| scale_ui_usize(width, dpi))
        .unwrap_or(default_width)
        .clamp(min_width, max_width)
}

fn centered_inner_start(origin: usize, outer: usize, inner: usize) -> usize {
    if inner <= outer {
        origin + (outer - inner) / 2
    } else {
        origin.saturating_sub((inner - outer) / 2)
    }
}

impl crate::TermWindow {
    pub(crate) fn acknowledge_active_workspace_thread_work(&self) -> bool {
        let mux = Mux::get();
        let Some(window) = mux.get_window(self.mux_window_id) else {
            return false;
        };
        workspace_threads::acknowledge_thread_work_for_workspace(window.get_workspace())
    }

    pub(crate) fn acknowledge_active_workspace_thread_work_deferred(&self) -> bool {
        let mux = Mux::get();
        let Some(window) = mux.get_window(self.mux_window_id) else {
            return false;
        };
        workspace_threads::acknowledge_thread_work_for_workspace_deferred(window.get_workspace())
    }

    /// The docked width, clamped. Not "the width right now": ask
    /// `workspace_sidebar_width` for what the terminal is laid out against
    /// and `workspace_sidebar_presented_width` for what is on screen.
    fn workspace_sidebar_docked_width(&self) -> usize {
        let min_width = scale_ui_usize(SIDEBAR_MIN_WIDTH, self.dimensions.dpi);
        self.workspace_sidebar_width
            .clamp(min_width, self.workspace_sidebar_max_width())
    }

    /// The width the TERMINAL is laid out against. Zero whenever the sidebar
    /// is collapsed, hover reveal or not: a hover must never reflow a PTY.
    /// Every consumer that decides where the terminal, the tab bar or a pane
    /// lives asks this one.
    pub fn workspace_sidebar_width(&self) -> usize {
        if self.workspace_sidebar_collapsed {
            0
        } else {
            self.workspace_sidebar_docked_width()
        }
    }

    /// The width the panel is DRAWN and HIT-TESTED at. Equal to
    /// `workspace_sidebar_width` while docked; the full docked width while a
    /// hover reveal is on screen, at which point the terminal behind it has
    /// not moved. Presentation and pointer routing only.
    pub(crate) fn workspace_sidebar_presented_width(&self) -> usize {
        if self.workspace_sidebar_collapsed {
            if self.workspace_sidebar_hover.is_presented() {
                self.workspace_sidebar_docked_width()
            } else {
                0
            }
        } else {
            self.workspace_sidebar_docked_width()
        }
    }

    /// Whether the panel is on screen at all, by either route.
    pub(crate) fn workspace_sidebar_is_presented(&self) -> bool {
        !self.workspace_sidebar_collapsed || self.workspace_sidebar_hover.is_presented()
    }

    pub fn workspace_sidebar_max_width(&self) -> usize {
        let min_width = scale_ui_usize(SIDEBAR_MIN_WIDTH, self.dimensions.dpi);
        scale_ui_usize(SIDEBAR_MAX_WIDTH, self.dimensions.dpi)
            .min((self.dimensions.pixel_width / 2).max(min_width))
    }

    pub fn set_workspace_sidebar_width(&mut self, width: usize) {
        let min_width = scale_ui_usize(SIDEBAR_MIN_WIDTH, self.dimensions.dpi);
        self.workspace_sidebar_width = width.clamp(min_width, self.workspace_sidebar_max_width());
    }

    pub fn persist_workspace_sidebar_width(&self) {
        let min_width = scale_ui_usize(SIDEBAR_MIN_WIDTH, self.dimensions.dpi);
        let max_width = scale_ui_usize(SIDEBAR_MAX_WIDTH, self.dimensions.dpi);
        let width = self.workspace_sidebar_width.clamp(min_width, max_width);
        let width = unscale_ui_usize(width, self.dimensions.dpi);
        if let Err(err) = crate::native_settings::save_workspace_sidebar_width(width) {
            log::warn!("failed to save workspace sidebar width: {err:#}");
        }
    }

    /// Toggle the panel, and remember the result so new windows inherit it.
    ///
    /// Reads [`Self::workspace_sidebar_is_presented`] rather than the collapsed
    /// flag, because during a hover reveal the two disagree: the panel is on
    /// screen while `workspace_sidebar_collapsed` is still true. The tab bar's
    /// button is labelled from the presented state
    /// ([`Self::workspace_sidebar_toggle_icon`]), so it reads "close" during a
    /// reveal — and toggling the flag from there *docked* the panel instead of
    /// closing it. Harmless while nothing persisted; once this writes the
    /// setting, brushing the left edge and clicking "close" would re-dock the
    /// sidebar for every window from then on.
    pub fn toggle_workspace_sidebar(&mut self) {
        let shown = workspace_sidebar_toggle_target(
            self.workspace_sidebar_collapsed,
            self.workspace_sidebar_hover.is_presented(),
        );
        self.workspace_sidebar_swipe.cancel_immediately();
        self.clear_workspace_space_swipe_frame_transition();
        self.set_workspace_sidebar_shown(shown);
    }

    pub fn expand_workspace_sidebar(&mut self) {
        self.set_workspace_sidebar_shown(true);
    }

    /// Dock or collapse the panel and persist that choice.
    ///
    /// The write is skipped when nothing changed, which matters because the
    /// resize drag calls this on every pointer move.
    pub(crate) fn set_workspace_sidebar_shown(&mut self, shown: bool) {
        let collapsed = !shown;
        if self.workspace_sidebar_collapsed != collapsed {
            self.workspace_sidebar_collapsed = collapsed;
            crate::native_settings::save_workspace_sidebar_shown(shown);
        }
        // Either way the hover machine must not act on the pointer still
        // sitting where it was: collapsing must not instantly re-reveal, and
        // docking makes the reveal moot.
        self.workspace_sidebar_hover.suppress_until_pointer_leaves();
    }

    pub(crate) fn workspace_sidebar_toggle_icon(&self) -> SvgIcon {
        if self.workspace_sidebar_is_presented() {
            SvgIcon::PanelLeftClose
        } else {
            SvgIcon::PanelLeftOpen
        }
    }

    pub fn tab_bar_left_edge(&self) -> usize {
        let border = self.get_os_border();
        border.left.get() as usize + self.workspace_sidebar_width()
    }

    /// The panel as presented — a hover reveal is included. Anything
    /// deriving terminal geometry wants `workspace_sidebar_width` instead.
    pub fn workspace_sidebar_rect(&self) -> Option<WorkspaceSidebarRect> {
        self.workspace_sidebar_rect_for_width(self.workspace_sidebar_presented_width())
    }

    fn workspace_sidebar_rect_for_width(&self, width: usize) -> Option<WorkspaceSidebarRect> {
        let border = self.get_os_border();
        let bottom_tab_bar_height = if self.config.tab_bar_at_bottom && self.show_tab_bar {
            self.tab_bar_pixel_height().unwrap_or(0.0).ceil() as usize
        } else {
            0
        };

        let x = border.left.get() as usize;
        let y = border.top.get() as usize;
        let width = width.min(
            self.dimensions
                .pixel_width
                .saturating_sub((border.left + border.right).get() as usize),
        );
        let height = self
            .dimensions
            .pixel_height
            .saturating_sub(y + border.bottom.get() as usize + bottom_tab_bar_height);

        if width == 0 || height == 0 {
            return None;
        }

        Some(WorkspaceSidebarRect {
            x,
            y,
            width,
            height,
        })
    }

    /// The left-edge strip that arms a hover reveal, as (x, y, w, h) in
    /// window pixels. Computed against the docked rect so it exists while
    /// the panel does not.
    pub(crate) fn workspace_sidebar_hover_hot_zone(&self) -> Option<(usize, usize, usize, usize)> {
        let rect = self.workspace_sidebar_rect_for_width(self.workspace_sidebar_docked_width())?;
        let top_tab_bar_height = if self.show_tab_bar && !self.config.tab_bar_at_bottom {
            self.tab_bar_pixel_height().unwrap_or(0.0).ceil() as usize
        } else {
            0
        };
        crate::termwindow::sidebar_hover::hot_zone(
            rect.x,
            rect.y,
            rect.height,
            self.ui_px(crate::termwindow::ui::tokens::SIDEBAR_HOVER_HOT_ZONE_WIDTH),
            top_tab_bar_height,
        )
    }

    fn workspace_sidebar_list_height(
        view: &workspace_threads::WorkspaceThreadsView,
        row_height: usize,
        row_gap: usize,
    ) -> usize {
        let mut height = 0usize;
        if !view.pinned_threads.is_empty() {
            height = height.saturating_add(row_height + row_gap);
            height = height.saturating_add(
                view.pinned_threads
                    .len()
                    .saturating_mul(row_height + row_gap),
            );
        }
        if !view.projects.is_empty() {
            if !view.pinned_threads.is_empty() {
                height = height.saturating_add(WORKSPACE_SECTION_LABEL_GAP);
            }
            height = height.saturating_add(row_height + row_gap);
        }
        for project in &view.projects {
            height = height.saturating_add(row_height + row_gap);
            if !project.threads_collapsed {
                height = height
                    .saturating_add(project.threads.len().saturating_mul(row_height + row_gap));
            }
        }
        height = height.saturating_add(
            view.projects
                .len()
                .saturating_sub(1)
                .saturating_mul(WORKSPACE_GROUP_EXTRA_GAP),
        );
        if !view.ref_groups.is_empty() {
            if !view.pinned_threads.is_empty() || !view.projects.is_empty() {
                height = height.saturating_add(WORKSPACE_GROUP_EXTRA_GAP);
            }
            for (group_idx, group) in view.ref_groups.iter().enumerate() {
                if group_idx > 0 {
                    height = height.saturating_add(WORKSPACE_GROUP_EXTRA_GAP);
                }
                height = height.saturating_add(row_height + row_gap);
                if !group.collapsed {
                    height = height
                        .saturating_add(group.threads.len().saturating_mul(row_height + row_gap));
                }
            }
        }
        height.saturating_sub(row_gap)
    }

    fn workspace_sidebar_scroll_height(
        view: &workspace_threads::WorkspaceThreadsView,
        row_height: usize,
        row_gap: usize,
        viewport_height: usize,
    ) -> usize {
        let height = Self::workspace_sidebar_list_height(view, row_height, row_gap);
        if height > viewport_height {
            height.saturating_add(SIDEBAR_LIST_BOTTOM_PADDING)
        } else {
            height
        }
    }

    /// Apply the persisted view-options status filter. Must run everywhere a
    /// `WorkspaceThreadsView` feeds the sidebar (paint, scroll height, hit
    /// testing) or row math diverges from the painted rows.
    fn apply_workspace_thread_status_filter(
        &self,
        mut view: workspace_threads::WorkspaceThreadsView,
    ) -> workspace_threads::WorkspaceThreadsView {
        // This is the single choke point every sidebar consumer of the view
        // passes through (paint, scroll height, hit testing), so the runtime
        // collapse state of reference groups is stamped here — the three
        // row-math sites must all see the same collapsed flags.
        let space_id = self.workspace_sidebar_space_id().to_string();
        for group in &mut view.ref_groups {
            group.collapsed = self
                .thread_ref_groups_collapsed
                .contains(&format!("{space_id}::{}", group.key));
        }
        // Thread refs are a hand-curated list and are not filtered; the
        // filter below only touches the Space's own pinned/project rows.
        let hidden = crate::native_settings::workspace_sidebar_hidden_statuses()
            .iter()
            .filter_map(|key| workspace_threads::WorkspaceThreadWorkStatus::from_settings_key(key))
            .collect::<Vec<_>>();
        if !hidden.is_empty() {
            workspace_threads::filter_threads_view_by_status(&mut view, &hidden);
        }
        view
    }

    fn sidebar_thread_status_kind(
        &self,
        session: &workspace_threads::WorkspaceThreadView,
    ) -> Option<UiStatusKind> {
        match session.work_status {
            workspace_threads::WorkspaceThreadWorkStatus::Running => Some(UiStatusKind::Running),
            workspace_threads::WorkspaceThreadWorkStatus::NeedsAttention => {
                Some(UiStatusKind::NeedsAttention)
            }
            workspace_threads::WorkspaceThreadWorkStatus::FinishedUnseen => {
                Some(UiStatusKind::Done)
            }
            workspace_threads::WorkspaceThreadWorkStatus::Idle => None,
        }
    }

    /// Update this window's notification snapshot and return the expanding
    /// halo phase for a newly arrived notification. An initial snapshot does
    /// not pulse: notifications restored at startup are unread, but not new.
    fn workspace_notification_badge_pulse(
        &mut self,
        notifications: &[workspace_threads::ThreadWorkNotification],
    ) -> Option<f32> {
        let current = notifications
            .iter()
            .map(|notification| (notification.thread_id.clone(), notification.status))
            .collect::<HashMap<_, _>>();

        let has_new_notification = notification_snapshot_has_new_entry(
            self.workspace_notification_snapshot.as_ref(),
            &current,
        );
        let has_current_notifications = !current.is_empty();
        self.workspace_notification_snapshot = Some(current);

        if !has_current_notifications {
            self.workspace_notification_pulse_started_at = None;
            return None;
        }

        let now = Instant::now();
        if has_new_notification {
            self.workspace_notification_pulse_started_at = Some(now);
        }

        let started_at = self.workspace_notification_pulse_started_at?;
        let Some(phase) = notification_badge_pulse_phase(now.saturating_duration_since(started_at))
        else {
            self.workspace_notification_pulse_started_at = None;
            return None;
        };

        self.update_next_frame_time(Some(
            now + Duration::from_millis(NOTIFICATION_BADGE_FRAME_MS),
        ));
        Some(phase)
    }

    fn paint_workspace_notification_badge(
        &self,
        layers: &mut TripleLayerQuadAllocator,
        layer: usize,
        action_x: usize,
        action_y: usize,
        action_size: usize,
        pulse_phase: Option<f32>,
    ) -> anyhow::Result<()> {
        let badge_size = self.ui_px(9).max(4);
        let badge_x = (action_x + action_size).saturating_sub(badge_size + self.ui_px(6));
        let badge_y = action_y + self.ui_px(6);

        if let Some(phase) = pulse_phase {
            // Keep the core visible and radiate a short, fading halo three
            // times. This attracts attention without a continuously blinking
            // control once the animation ends.
            let expansion = self.ui_f32(10.0) * phase;
            let halo_size = badge_size as f32 + expansion;
            let halo_x = badge_x as f32 + badge_size as f32 / 2.0 - halo_size / 2.0;
            let halo_y = badge_y as f32 + badge_size as f32 / 2.0 - halo_size / 2.0;
            self.fill_rounded_rectangle(
                layers,
                layer,
                euclid::rect(halo_x, halo_y, halo_size, halo_size),
                NOTIFICATION_BADGE_COLOR.mul_alpha(0.48 * (1.0 - phase)),
                halo_size / 2.0,
            )
            .context("sidebar notification badge pulse")?;
        }

        self.fill_rounded_rectangle(
            layers,
            layer,
            euclid::rect(
                badge_x as f32,
                badge_y as f32,
                badge_size as f32,
                badge_size as f32,
            ),
            NOTIFICATION_BADGE_COLOR,
            badge_size as f32 / 2.0,
        )
        .context("sidebar notification badge")
    }

    /// Connection health of a client-domain Space. While the Space is
    /// remote we keep a slow self-driven repaint tick going: a dead
    /// connection produces no output, so nothing else would repaint the
    /// indicator when the state flips.
    pub(crate) fn space_connection_state(&self, space_id: &str) -> SpaceConnectionState {
        const SUSTAINED_LAG_MS: u64 = 5000;
        let Some(domain_name) = workspace_threads::client_domain_for_space(space_id) else {
            return SpaceConnectionState::Connected;
        };
        self.update_next_frame_time(Some(
            std::time::Instant::now() + std::time::Duration::from_secs(1),
        ));
        let mux = Mux::get();
        let Some(domain) = mux.get_domain_by_name(&domain_name) else {
            // Not even registered: never connected in this session.
            return SpaceConnectionState::Disconnected;
        };
        if let Some(client) = domain.downcast_ref::<wezterm_client::domain::ClientDomain>() {
            // First connect (or manual re-attach) in progress: state() reads
            // Detached until finish_attach, which used to surface as
            // "Disconnected" with a Reconnect button while actively
            // connecting.
            if client.is_attaching() {
                return SpaceConnectionState::Connecting;
            }
            // The retry engine is between attempts. The backoff gap used to
            // read as Disconnected, so a VPN that wakes on the first packet
            // flashed a failure on every entry before attempt two succeeded.
            if client.is_attach_retrying() {
                return SpaceConnectionState::Connecting;
            }
            // The retry loop parked itself after two minutes of failures;
            // it waits for the sidebar Reconnect button.
            if client.is_reconnect_suspended() {
                return SpaceConnectionState::Disconnected;
            }
            // The reconnect loop knows immediately when the transport died
            // — pane tardiness below only trips after something is SENT on
            // a pane, so on its own it misses idle disconnects entirely.
            if client.is_reconnecting() {
                return SpaceConnectionState::Reconnecting;
            }
        }
        if domain.state() != mux::domain::DomainState::Attached {
            return SpaceConnectionState::Disconnected;
        }
        // Attached but silent despite outstanding requests: transport not
        // (yet) declared dead. Panes report tardy after ~3s, which ordinary
        // latency spikes trip all the time; require a longer sustained
        // silence before alarming the user.
        let domain_id = domain.domain_id();
        if mux.iter_panes().iter().any(|pane| {
            pane.domain_id() == domain_id
                && crate::termwindow::render::pane::client_pane_lag_ms(pane.as_ref())
                    .map_or(false, |ms| ms >= SUSTAINED_LAG_MS)
        }) {
            SpaceConnectionState::Reconnecting
        } else {
            SpaceConnectionState::Connected
        }
    }

    fn is_workspace_sidebar_thread_selected(
        &self,
        session: &workspace_threads::WorkspaceThreadView,
    ) -> bool {
        if let Some(thread_id) = self.workspace_sidebar_pending_thread_selection.as_deref() {
            thread_id == session.id.as_str()
        } else {
            session.is_active
        }
    }

    fn sidebar_thread_dot_color(
        &self,
        session: &workspace_threads::WorkspaceThreadView,
        chrome: &UiPalette,
        foreground: LinearRgba,
    ) -> LinearRgba {
        if session.is_active {
            LinearRgba::with_components(0.20, 0.78, 0.36, 1.0)
        } else if session.is_unread {
            chrome.selected_bg
        } else if session.is_pinned {
            foreground.mul_alpha(0.68)
        } else if session.is_materialized {
            SESSION_STATUS_OPEN_COLOR
        } else {
            foreground.mul_alpha(0.16)
        }
    }

    fn sidebar_thread_status_color(
        &self,
        session: &workspace_threads::WorkspaceThreadView,
        status: UiStatusKind,
        chrome: &UiPalette,
        foreground: LinearRgba,
    ) -> LinearRgba {
        if status == UiStatusKind::Done {
            SESSION_STATUS_DONE_COLOR
        } else if session.is_active {
            foreground
        } else {
            self.sidebar_thread_dot_color(session, chrome, foreground)
        }
    }

    pub(crate) fn ui_px(&self, value: usize) -> usize {
        scale_ui_usize(value, self.dimensions.dpi)
    }

    pub(crate) fn ui_f32(&self, value: f32) -> f32 {
        scale_ui_f32(value, self.dimensions.dpi)
    }

    fn paint_sidebar_thread_status(
        &self,
        layers: &mut TripleLayerQuadAllocator,
        session: &workspace_threads::WorkspaceThreadView,
        x: usize,
        y: usize,
        row_height: usize,
        chrome: &UiPalette,
        foreground: LinearRgba,
    ) -> anyhow::Result<()> {
        let icon_y = y + ((row_height.saturating_sub(self.ui_px(SESSION_STATUS_ICON_SIZE))) / 2);
        if let Some(status) = self.sidebar_thread_status_kind(session) {
            let status_size = if matches!(status, UiStatusKind::Running | UiStatusKind::Done) {
                self.ui_px(SESSION_STATUS_ACTIVE_ICON_SIZE)
            } else {
                self.ui_px(SESSION_STATUS_ICON_SIZE)
            };
            return self.paint_status_icon(
                layers,
                2,
                status,
                centered_inner_start(x, self.ui_px(SESSION_STATUS_ICON_SIZE), status_size),
                centered_inner_start(y, row_height, status_size),
                status_size,
                self.sidebar_thread_status_color(session, status, chrome, foreground),
            );
        }

        let dot_offset = (self
            .ui_px(SESSION_STATUS_ICON_SIZE)
            .saturating_sub(self.ui_px(SESSION_STATUS_DOT_SIZE)))
            / 2;
        let dot_x = x + dot_offset;
        let dot_y = icon_y + dot_offset;
        self.fill_rounded_rectangle(
            layers,
            1,
            euclid::rect(
                dot_x as f32,
                dot_y as f32,
                self.ui_px(SESSION_STATUS_DOT_SIZE) as f32,
                self.ui_px(SESSION_STATUS_DOT_SIZE) as f32,
            ),
            self.sidebar_thread_dot_color(session, chrome, foreground),
            self.ui_px(SESSION_STATUS_DOT_SIZE) as f32 / 2.0,
        )
        .context("sidebar thread status dot")
    }

    /// Visual size of the expanded-sidebar toggle button; shares
    /// sidebar_toggle_size_px with the collapsed-state painter and the tab
    /// layout reservation so the control never changes size when the
    /// sidebar opens or closes.
    fn workspace_sidebar_toggle_visual_size(&self) -> usize {
        self.window_tab_chrome_params().sidebar_toggle_button_size()
    }

    fn workspace_sidebar_content_top(&self, panel_y: usize) -> usize {
        let tab_row_height = self.tab_bar_pixel_height().unwrap_or(0.0).ceil() as usize;
        platform_chrome::workspace_sidebar_content_top(
            panel_y,
            self.ui_px(SIDEBAR_INSET),
            tab_row_height,
            self.window_state,
            self.dimensions.dpi,
        )
    }

    fn workspace_sidebar_layout(
        &self,
        rect: WorkspaceSidebarRect,
        ui_cell_height: usize,
        icon_size: usize,
    ) -> WorkspaceSidebarLayout {
        let panel_margin = 0usize;
        let panel_x = rect.x + panel_margin;
        let panel_y = rect.y + panel_margin;
        let panel_width = rect.width.saturating_sub(panel_margin * 2).max(1);
        let panel_height = rect.height.saturating_sub(panel_margin * 2).max(1);
        let settings_footer_height = self.ui_px(SIDEBAR_SETTINGS_FOOTER_HEIGHT).min(panel_height);
        let settings_footer_y = panel_y
            .saturating_add(panel_height)
            .saturating_sub(settings_footer_height);
        let content_bottom = (panel_y + panel_height.saturating_sub(self.ui_px(SIDEBAR_INSET)))
            .min(settings_footer_y.max(panel_y));
        let show_sidebar_toolbar =
            platform_chrome::workspace_sidebar_shows_toolbar(self.window_state);
        let sidebar_toolbar_uses_fullscreen_style =
            platform_chrome::workspace_sidebar_toolbar_uses_fullscreen_style(self.window_state);
        let row_height = (ui_cell_height.max(icon_size) + self.ui_px(SIDEBAR_INSET))
            .max(self.ui_px(SESSION_ROW_MIN_HEIGHT));
        let mut y = self.workspace_sidebar_content_top(panel_y);
        if show_sidebar_toolbar {
            y += self.workspace_sidebar_toggle_visual_size() + self.ui_px(SIDEBAR_INSET);
        }
        let top_action_height = row_height
            .min(self.ui_px(48))
            .max(ui_cell_height + self.ui_px(SIDEBAR_INSET))
            + if sidebar_toolbar_uses_fullscreen_style {
                self.ui_px(WINDOW_TAB_FULLSCREEN_NEW_SESSION_EXTRA_HEIGHT)
            } else {
                0
            };
        let top_action_y_offset = if sidebar_toolbar_uses_fullscreen_style {
            self.ui_px(WINDOW_TAB_FULLSCREEN_NEW_SESSION_Y_OFFSET)
        } else {
            0
        };
        let space_menu_y = y;
        let space_menu_height = top_action_height + self.ui_px(6);
        y += space_menu_height + self.ui_px(SIDEBAR_INSET);
        // Connecting keeps the row (as a spinner) so the whole retry
        // sequence paints one stable row instead of flickering between
        // "row while disconnected" and "no row while an attempt runs".
        let reconnect_row_height = if matches!(
            self.space_connection_state(self.workspace_sidebar_space_id()),
            SpaceConnectionState::Disconnected | SpaceConnectionState::Connecting
        ) {
            ui_cell_height + self.ui_px(SIDEBAR_INSET)
        } else {
            0
        };
        if reconnect_row_height > 0 {
            y += reconnect_row_height + self.ui_px(SIDEBAR_INSET);
        }
        let list_top = y + top_action_y_offset + top_action_height + self.ui_px(SIDEBAR_INSET);

        WorkspaceSidebarLayout {
            panel_x,
            panel_y,
            panel_width,
            panel_height,
            content_bottom,
            settings_footer_y,
            settings_footer_height,
            row_height,
            show_sidebar_toolbar,
            space_menu_y,
            space_menu_height,
            reconnect_row_height,
            top_action_y_offset,
            top_action_height,
            list_top,
        }
    }

    /// The row cell height the sidebar actually PAINTS with: the title font
    /// at the user's sidebar font size. Every consumer of
    /// `workspace_sidebar_layout` must use this — a plain `title_font()`
    /// here made scroll geometry and drag hot zones drift from the pixels
    /// whenever the sidebar font size was customized.
    fn workspace_sidebar_cell_height(&self) -> usize {
        self.fonts
            .title_font_with_size(crate::native_settings::sidebar_font_size())
            .map(|font| {
                RenderMetrics::with_font_metrics(&font.metrics())
                    .cell_size
                    .height as usize
            })
            .unwrap_or(self.render_metrics.cell_size.height as usize)
    }

    pub fn workspace_sidebar_scroll_max(&self) -> f32 {
        let Some(rect) = self.workspace_sidebar_rect() else {
            return 0.0;
        };
        if !self.workspace_sidebar_is_presented() {
            return 0.0;
        }

        let ui_cell_height = self.workspace_sidebar_cell_height();
        let icon_size = (ui_cell_height + self.ui_px(12)).clamp(self.ui_px(30), self.ui_px(36));
        let layout = self.workspace_sidebar_layout(rect, ui_cell_height, icon_size);
        let content_bottom = layout.content_bottom;
        let list_top = layout.list_top;
        let viewport_height = content_bottom.saturating_sub(list_top);
        if viewport_height == 0 {
            return 0.0;
        }

        let mux = Mux::get();
        // Use THIS window's own workspace, not the global active one: with
        // multiple windows open, a non-focused window must still highlight the
        // thread it is actually showing.
        let active_workspace = self
            .current_mux_workspace()
            .unwrap_or_else(|| mux.active_workspace());
        let workspaces = mux.iter_workspaces();
        let view =
            self.apply_workspace_thread_status_filter(workspace_threads::view_for_current_project(
                self.workspace_sidebar_space_id(),
                &active_workspace,
                &workspaces,
            ));
        let row_gap = self.ui_px(SIDEBAR_ROW_GAP);
        let total_height = Self::workspace_sidebar_scroll_height(
            &view,
            layout.row_height,
            row_gap,
            viewport_height,
        );
        total_height.saturating_sub(viewport_height) as f32
    }

    /// The vertical extent of the scrollable thread list itself — between
    /// the toolbar above and the footer below. This is the area row drags
    /// hit-test and autoscroll against; the panel background also covers
    /// the header strip and footer, so it must not be used for that.
    pub(crate) fn workspace_sidebar_list_viewport(&self) -> Option<(isize, isize)> {
        let rect = self.workspace_sidebar_rect()?;
        if !self.workspace_sidebar_is_presented() {
            return None;
        }
        let ui_cell_height = self.workspace_sidebar_cell_height();
        let icon_size = (ui_cell_height + self.ui_px(12)).clamp(self.ui_px(30), self.ui_px(36));
        let layout = self.workspace_sidebar_layout(rect, ui_cell_height, icon_size);
        if layout.content_bottom <= layout.list_top {
            return None;
        }
        Some((layout.list_top as isize, layout.content_bottom as isize))
    }

    pub fn workspace_sidebar_scroll_geometry(&self) -> Option<WorkspaceSidebarScrollGeometry> {
        let rect = self.workspace_sidebar_rect()?;
        if !self.workspace_sidebar_is_presented() {
            return None;
        }

        let ui_cell_height = self.workspace_sidebar_cell_height();
        let icon_size = (ui_cell_height + self.ui_px(12)).clamp(self.ui_px(30), self.ui_px(36));
        let layout = self.workspace_sidebar_layout(rect, ui_cell_height, icon_size);
        let content_bottom = layout.content_bottom;
        let list_top = layout.list_top;
        let viewport_height = content_bottom.saturating_sub(list_top);
        if viewport_height == 0 {
            return None;
        }

        let mux = Mux::get();
        // Use THIS window's own workspace, not the global active one: with
        // multiple windows open, a non-focused window must still highlight the
        // thread it is actually showing.
        let active_workspace = self
            .current_mux_workspace()
            .unwrap_or_else(|| mux.active_workspace());
        let workspaces = mux.iter_workspaces();
        let view =
            self.apply_workspace_thread_status_filter(workspace_threads::view_for_current_project(
                self.workspace_sidebar_space_id(),
                &active_workspace,
                &workspaces,
            ));
        let row_gap = self.ui_px(SIDEBAR_ROW_GAP);
        let total_height = Self::workspace_sidebar_scroll_height(
            &view,
            layout.row_height,
            row_gap,
            viewport_height,
        );
        let max_scroll = total_height.saturating_sub(viewport_height) as f32;
        if max_scroll <= 0.0 || total_height == 0 {
            return None;
        }

        let track_width = 4usize;
        let track_height = viewport_height.max(1);
        let thumb_height = ((viewport_height as f32 / total_height as f32) * track_height as f32)
            .clamp(28.0, track_height as f32);
        let travel = (track_height as f32 - thumb_height).max(1.0);
        let scroll_offset = self.workspace_sidebar_scroll_offset.clamp(0.0, max_scroll);
        let thumb_y = list_top as f32 + (scroll_offset / max_scroll) * travel;
        let track_x = layout
            .panel_x
            .saturating_add(layout.panel_width)
            .saturating_sub(self.ui_px(SIDEBAR_INSET) / 2 + track_width);

        Some(WorkspaceSidebarScrollGeometry {
            track_x,
            track_y: list_top,
            track_width,
            track_height,
            thumb_y,
            thumb_height,
            max_scroll,
        })
    }

    /// The "Disconnected — Reconnect" row under the Space menu. Painted
    /// twice per frame like the rest of the header: once in the normal
    /// header pass, and once in the post-list header repaint that covers
    /// scrolled content (which would otherwise mask it — everything in the
    /// first pass sits below the header scroll mask).
    #[allow(clippy::too_many_arguments)]
    fn paint_space_reconnect_row(
        &mut self,
        layers: &mut TripleLayerQuadAllocator,
        layer: usize,
        row_x: usize,
        y: usize,
        row_width: usize,
        row_height: usize,
        ui_font: &Rc<LoadedFont>,
        ui_metrics: RenderMetrics,
        ui_cell_height: usize,
        register_ui_item: bool,
    ) -> anyhow::Result<()> {
        // The domain-level retry flag covers every attach entry point; the
        // local set still bridges the click-to-spawn gap for the button.
        let reconnect_in_flight = self.space_connection_state(self.workspace_sidebar_space_id())
            == SpaceConnectionState::Connecting
            || workspace_threads::client_domain_for_space(self.workspace_sidebar_space_id())
                .map_or(false, |name| {
                    self.space_reconnects_in_flight.contains(&name)
                });
        let hovered =
            !reconnect_in_flight && self.is_pointer_over_ui_rect(row_x, y, row_width, row_height);
        self.fill_rounded_rectangle(
            layers,
            layer,
            euclid::rect(row_x as f32, y as f32, row_width as f32, row_height as f32),
            SPACE_DISCONNECTED_COLOR.mul_alpha(if hovered { 0.30 } else { 0.18 }),
            self.ui_f32(SIDEBAR_ROW_RADIUS),
        )
        .context("sidebar reconnect row")?;
        let row_icon_size = ui_cell_height.min(row_height.saturating_sub(6));
        let row_icon_x = row_x + self.ui_px(SIDEBAR_INSET);
        let row_icon_y = y + ((row_height.saturating_sub(row_icon_size)) / 2);
        if reconnect_in_flight {
            // The spinning painter also schedules the next repaint; the
            // static one would freeze the loader on frame zero.
            self.paint_spinning_ui_icon(
                layers,
                2,
                SvgIcon::LoaderCircle,
                row_icon_x,
                row_icon_y,
                row_icon_size,
                SPACE_DISCONNECTED_COLOR,
            )?;
        } else {
            self.paint_sidebar_icon(
                layers,
                SvgIcon::RotateCcw,
                row_icon_x,
                row_icon_y,
                row_icon_size,
                SPACE_DISCONNECTED_COLOR,
            )?;
        }
        let row_text_x = row_icon_x + row_icon_size + self.ui_px(SIDEBAR_ICON_GAP);
        let row_text_max =
            (row_x + row_width).saturating_sub(row_text_x + self.ui_px(SIDEBAR_INSET));
        let reconnect_label = if reconnect_in_flight {
            crate::i18n::tr("sidebar-connecting")
        } else {
            crate::i18n::tr("sidebar-reconnect")
        };
        let row_label = self.ellipsize_ui_text(ui_font, &reconnect_label, row_text_max)?;
        let row_label = row_label.into_owned();
        self.paint_sidebar_text(
            layers,
            ui_font,
            ui_metrics,
            &row_label,
            row_text_x,
            y + ((row_height.saturating_sub(ui_cell_height)) / 2),
            row_text_max,
            SPACE_DISCONNECTED_COLOR,
        )?;
        if register_ui_item && !reconnect_in_flight {
            self.ui_items.push(UIItem {
                x: row_x,
                y,
                width: row_width,
                height: row_height,
                item_type: UIItemType::SpaceReconnect,
            });
        }
        Ok(())
    }

    pub fn paint_workspace_sidebar(
        &mut self,
        layers: &mut TripleLayerQuadAllocator,
    ) -> anyhow::Result<()> {
        // Clearing here is what keeps the recording honest: every exit below
        // this point either reached the row loop and recorded a real span, or
        // left this `None`. It can never describe an earlier paint.
        self.workspace_sidebar_list_quads = None;
        let rect = match self.workspace_sidebar_rect() {
            Some(rect) => rect,
            None => return Ok(()),
        };

        let chrome = UiPalette::for_appearance(crate::native_settings::effective_appearance());
        let foreground = chrome.text;
        let sidebar_bg = chrome.workspace_sidebar_bg;
        let sidebar_separator = chrome.separator;
        let selected_bg = chrome.sidebar_row_active_bg;
        let selected_border = chrome.sidebar_row_active_border;
        let active_fg = chrome.text;
        let muted_fg = chrome.secondary_text;
        let ui_font = self
            .fonts
            .title_font_with_size(crate::native_settings::sidebar_font_size())
            .context("sidebar ui font")?;
        let ui_metrics = RenderMetrics::with_font_metrics(&ui_font.metrics());
        let ui_cell_height = ui_metrics.cell_size.height as usize;
        let icon_size = (ui_cell_height + self.ui_px(12)).clamp(self.ui_px(30), self.ui_px(36));
        let layout = self.workspace_sidebar_layout(rect, ui_cell_height, icon_size);
        let panel_x = layout.panel_x;
        let panel_y = layout.panel_y;
        let panel_width = layout.panel_width;
        let panel_height = layout.panel_height;
        let content_bottom = layout.content_bottom;
        let settings_footer_y = layout.settings_footer_y;
        let settings_footer_height = layout.settings_footer_height;

        if rect.y > 0 {
            self.filled_rectangle(
                layers,
                0,
                euclid::rect(rect.x as f32, 0.0, rect.width as f32, rect.y as f32),
                sidebar_bg,
            )
            .context("sidebar header background")?;
            self.ui_items.push(UIItem {
                x: rect.x,
                y: 0,
                width: rect.width,
                height: rect.y,
                item_type: UIItemType::WorkspaceSidebarBackground,
            });
        }
        self.filled_rectangle(
            layers,
            0,
            euclid::rect(
                panel_x as f32,
                panel_y as f32,
                panel_width as f32,
                panel_height as f32,
            ),
            sidebar_bg,
        )
        .context("sidebar background")?;
        self.ui_items.push(UIItem {
            x: rect.x,
            y: rect.y,
            width: rect.width,
            height: rect.height,
            item_type: UIItemType::WorkspaceSidebarBackground,
        });

        let session_row_height = layout.row_height;
        let item_x = panel_x + self.ui_px(SIDEBAR_INSET);
        let item_width = panel_width.saturating_sub(self.ui_px(SIDEBAR_INSET) * 2 + 1);
        self.ui_items.push(UIItem {
            x: rect
                .x
                .saturating_add(rect.width)
                .saturating_sub(self.ui_px(SIDEBAR_RESIZE_HANDLE_WIDTH) / 2),
            y: rect.y,
            width: self.ui_px(SIDEBAR_RESIZE_HANDLE_WIDTH),
            height: rect.height,
            item_type: UIItemType::WorkspaceSidebarResize,
        });

        if !self.workspace_sidebar_is_presented() {
            return Ok(());
        }

        let mux = Mux::get();
        // Use THIS window's own workspace, not the global active one: with
        // multiple windows open, a non-focused window must still highlight the
        // thread it is actually showing.
        let active_workspace = self
            .current_mux_workspace()
            .unwrap_or_else(|| mux.active_workspace());
        let workspaces = mux.iter_workspaces();
        let view =
            self.apply_workspace_thread_status_filter(workspace_threads::view_for_current_project(
                self.workspace_sidebar_space_id(),
                &active_workspace,
                &workspaces,
            ));

        let header_icon_size = icon_size.min(self.ui_px(32));
        let button_size = (header_icon_size + self.ui_px(8)).clamp(self.ui_px(32), self.ui_px(40));
        let header_y = self.workspace_sidebar_content_top(panel_y);
        let show_sidebar_toolbar = layout.show_sidebar_toolbar;
        let sidebar_toolbar_uses_fullscreen_style =
            platform_chrome::workspace_sidebar_toolbar_uses_fullscreen_style(self.window_state);
        let sidebar_toggle_size = self.workspace_sidebar_toggle_visual_size();
        // Mirror the collapsed-state toggle exactly: same left inset from
        // the window content edge (independent of the sidebar width — the
        // tab-bar path's leading_action_start_pixels returns 0 while the
        // sidebar is open, so it must not be used here) and the same
        // centering within the tab bar's content row.
        let border = self.get_os_border();
        let sidebar_toggle_x = if sidebar_toolbar_uses_fullscreen_style {
            panel_x + self.ui_px(WINDOW_TAB_FULLSCREEN_SIDEBAR_BUTTON_X)
        } else {
            border.left.get() as usize
                + platform_chrome::sidebar_toggle_left_inset_px(
                    self.dimensions.dpi,
                    self.window_state,
                )
        };
        let top_fancy_row =
            if self.show_tab_bar && self.config.use_fancy_tab_bar && !self.config.tab_bar_at_bottom
            {
                self.tab_bar_pixel_height().ok().map(|h| h.ceil() as usize)
            } else {
                None
            };
        let sidebar_toggle_y = if sidebar_toolbar_uses_fullscreen_style {
            header_y + self.ui_px(WINDOW_TAB_FULLSCREEN_SIDEBAR_BUTTON_Y_OFFSET)
        } else if let Some(row_h) = top_fancy_row {
            // Same formula as the collapsed painter: centered within the
            // content row (full row minus the top spacer).
            let spacer = self.ui_px(WINDOW_TAB_TOP_SPACER).min(row_h);
            border.top.get() as usize
                + spacer
                + (row_h - spacer).saturating_sub(sidebar_toggle_size) / 2
        } else {
            header_y
        };
        let sidebar_toggle_icon_size = self
            .ui_px(platform_chrome::workspace_sidebar_toolbar_icon_size(
                self.window_state,
            ))
            .min(sidebar_toggle_size.saturating_sub(2));
        if show_sidebar_toolbar {
            let toggle_hovered = self.is_pointer_over_ui_rect(
                sidebar_toggle_x,
                sidebar_toggle_y,
                sidebar_toggle_size,
                sidebar_toggle_size,
            );
            if toggle_hovered {
                self.fill_rounded_rectangle(
                    layers,
                    1,
                    euclid::rect(
                        sidebar_toggle_x as f32,
                        sidebar_toggle_y as f32,
                        sidebar_toggle_size as f32,
                        sidebar_toggle_size as f32,
                    ),
                    chrome.sidebar_button_hover_bg,
                    self.ui_f32(SIDEBAR_ROW_RADIUS),
                )
                .context("sidebar toolbar toggle button")?;
            }
            self.ui_items.push(UIItem {
                x: sidebar_toggle_x,
                y: sidebar_toggle_y,
                width: sidebar_toggle_size,
                height: sidebar_toggle_size,
                item_type: UIItemType::WorkspaceSidebarToggle,
            });
            self.paint_sidebar_icon(
                layers,
                self.workspace_sidebar_toggle_icon(),
                sidebar_toggle_x
                    + ((sidebar_toggle_size.saturating_sub(sidebar_toggle_icon_size)) / 2),
                sidebar_toggle_y
                    + ((sidebar_toggle_size.saturating_sub(sidebar_toggle_icon_size)) / 2),
                sidebar_toggle_icon_size,
                if toggle_hovered { foreground } else { muted_fg },
            )?;
        }
        let section_button_size = self
            .ui_px(SIDEBAR_SECTION_ACTION_SIZE)
            .max(button_size)
            .min(session_row_height.saturating_sub(8));
        let active_project_id = view
            .projects
            .iter()
            .find(|project| project.is_active)
            .map(|project| project.id.clone());
        let top_action_y_offset = layout.top_action_y_offset;
        let top_action_total_width = item_width.saturating_sub(self.ui_px(SIDEBAR_INSET) * 2);
        let top_action_gap = self.ui_px(SIDEBAR_ICON_GAP) + 4;
        let top_action_height = layout.top_action_height;
        let space_menu_x = item_x + self.ui_px(SIDEBAR_INSET);
        let space_menu_y = layout.space_menu_y;
        let mut push_header_blank = |x: usize, y: usize, width: usize, height: usize| {
            if width > 0 && height > 0 {
                self.ui_items.push(UIItem {
                    x,
                    y,
                    width,
                    height,
                    item_type: UIItemType::WorkspaceSidebarHeaderBlank,
                });
            }
        };
        if space_menu_y > panel_y {
            if show_sidebar_toolbar {
                let header_bottom = space_menu_y;
                let toggle_bottom = sidebar_toggle_y.saturating_add(sidebar_toggle_size);
                push_header_blank(
                    panel_x,
                    panel_y,
                    panel_width,
                    sidebar_toggle_y.saturating_sub(panel_y),
                );
                push_header_blank(
                    panel_x,
                    toggle_bottom,
                    panel_width,
                    header_bottom.saturating_sub(toggle_bottom),
                );
                let toggle_band_y = sidebar_toggle_y.max(panel_y);
                let toggle_band_bottom = toggle_bottom.min(header_bottom);
                let toggle_band_height = toggle_band_bottom.saturating_sub(toggle_band_y);
                push_header_blank(
                    panel_x,
                    toggle_band_y,
                    sidebar_toggle_x.saturating_sub(panel_x),
                    toggle_band_height,
                );
                push_header_blank(
                    sidebar_toggle_x.saturating_add(sidebar_toggle_size),
                    toggle_band_y,
                    panel_x
                        .saturating_add(panel_width)
                        .saturating_sub(sidebar_toggle_x.saturating_add(sidebar_toggle_size)),
                    toggle_band_height,
                );
            } else {
                push_header_blank(panel_x, panel_y, panel_width, space_menu_y - panel_y);
            }
        }
        let space_menu_width = top_action_total_width;
        let space_menu_height = layout.space_menu_height;
        let space_menu_hovered = self.is_pointer_over_ui_rect(
            space_menu_x,
            space_menu_y,
            space_menu_width,
            space_menu_height,
        );
        if space_menu_hovered {
            self.fill_rounded_rectangle(
                layers,
                1,
                euclid::rect(
                    space_menu_x as f32,
                    space_menu_y as f32,
                    space_menu_width as f32,
                    space_menu_height as f32,
                ),
                chrome.sidebar_button_hover_bg,
                self.ui_f32(SIDEBAR_ROW_RADIUS + 4.0),
            )
            .context("sidebar space menu button")?;
        }
        self.ui_items.push(UIItem {
            x: space_menu_x,
            y: space_menu_y,
            width: space_menu_width,
            height: space_menu_height,
            item_type: UIItemType::SpaceMenu,
        });
        let space_icon_size = header_icon_size.min(space_menu_height.saturating_sub(14));
        let space_icon_x = space_menu_x + self.ui_px(SIDEBAR_INSET);
        let space_icon_y = space_menu_y + ((space_menu_height.saturating_sub(space_icon_size)) / 2);
        let space_action_icon_size = space_icon_size.min(22);
        let space_action_icon_x = space_menu_x
            .saturating_add(space_menu_width)
            .saturating_sub(self.ui_px(SIDEBAR_INSET) + space_action_icon_size);
        let space_action_icon_y =
            space_menu_y + ((space_menu_height.saturating_sub(space_action_icon_size)) / 2);
        let space_text_right = space_menu_x
            .saturating_add(space_menu_width)
            .saturating_sub(self.ui_px(SIDEBAR_INSET) + space_action_icon_size + top_action_gap);
        let space_text_x = space_icon_x + space_icon_size + top_action_gap;
        let space_name =
            crate::workspace_threads::active_space_name(self.workspace_sidebar_space_id())
                .unwrap_or_else(|| crate::i18n::tr("sidebar-default-space"));
        let space_title = self.sidebar_space_title(self.workspace_sidebar_space_id(), &space_name);
        let space_label = self.ellipsize_ui_text(
            &ui_font,
            &space_title,
            space_text_right.saturating_sub(space_text_x),
        )?;
        let connection_state = self.space_connection_state(self.workspace_sidebar_space_id());
        // Swap the icon in place rather than adding text: the indicator
        // must not change the row's width or height, so transient lag
        // spikes can't make the sidebar layout jump.
        let (space_icon, space_icon_color) = match connection_state {
            SpaceConnectionState::Reconnecting | SpaceConnectionState::Disconnected => {
                (SvgIcon::CircleAlert, SPACE_DISCONNECTED_COLOR)
            }
            SpaceConnectionState::Connecting => (
                SvgIcon::LoaderCircle,
                if space_menu_hovered {
                    foreground
                } else {
                    muted_fg
                },
            ),
            SpaceConnectionState::Connected => (
                SvgIcon::Layers,
                if space_menu_hovered {
                    foreground
                } else {
                    muted_fg
                },
            ),
        };
        if connection_state == SpaceConnectionState::Connecting {
            self.paint_spinning_ui_icon(
                layers,
                2,
                space_icon,
                space_icon_x,
                space_icon_y,
                space_icon_size,
                space_icon_color,
            )?;
        } else {
            self.paint_sidebar_icon(
                layers,
                space_icon,
                space_icon_x,
                space_icon_y,
                space_icon_size,
                space_icon_color,
            )?;
        }
        self.paint_sidebar_text(
            layers,
            &ui_font,
            ui_metrics,
            space_label.as_ref(),
            space_text_x,
            space_menu_y + ((space_menu_height.saturating_sub(ui_cell_height)) / 2),
            space_text_right.saturating_sub(space_text_x),
            foreground,
        )?;
        self.paint_sidebar_icon(
            layers,
            SvgIcon::Ellipsis,
            space_action_icon_x,
            space_action_icon_y,
            space_action_icon_size,
            if space_menu_hovered {
                foreground
            } else {
                muted_fg
            },
        )?;
        let mut y = space_menu_y + space_menu_height + self.ui_px(SIDEBAR_INSET);
        if layout.reconnect_row_height > 0 {
            self.paint_space_reconnect_row(
                layers,
                1,
                item_x + self.ui_px(SIDEBAR_INSET),
                y,
                item_width.saturating_sub(self.ui_px(SIDEBAR_INSET) * 2),
                layout.reconnect_row_height,
                &ui_font,
                ui_metrics,
                ui_cell_height,
                true,
            )?;
            y += layout.reconnect_row_height + self.ui_px(SIDEBAR_INSET);
        }
        let top_action_x = item_x + self.ui_px(SIDEBAR_INSET);
        let top_action_y = y + top_action_y_offset;
        let notification_action_size = top_action_height;
        let notification_action_x = top_action_x
            .saturating_add(top_action_total_width)
            .saturating_sub(notification_action_size);
        let notification_action_y = top_action_y;
        let top_action_width =
            top_action_total_width.saturating_sub(notification_action_size + top_action_gap);
        let top_action_hovered = active_project_id.is_some()
            && self.is_pointer_over_ui_rect(
                top_action_x,
                top_action_y,
                top_action_width,
                top_action_height,
            );
        let notification_action_hovered = self.is_pointer_over_ui_rect(
            notification_action_x,
            notification_action_y,
            notification_action_size,
            notification_action_size,
        );
        self.fill_rounded_rectangle(
            layers,
            1,
            euclid::rect(
                top_action_x as f32,
                top_action_y as f32,
                top_action_width as f32,
                top_action_height as f32,
            ),
            if top_action_hovered {
                chrome.sidebar_button_hover_bg
            } else {
                chrome.sidebar_button_bg
            },
            self.ui_f32(SIDEBAR_ROW_RADIUS + 4.0),
        )
        .context("sidebar add thread button")?;
        self.fill_rounded_rectangle(
            layers,
            1,
            euclid::rect(
                notification_action_x as f32,
                notification_action_y as f32,
                notification_action_size as f32,
                notification_action_size as f32,
            ),
            if notification_action_hovered {
                chrome.sidebar_button_hover_bg
            } else {
                chrome.sidebar_button_bg
            },
            self.ui_f32(SIDEBAR_ROW_RADIUS + 4.0),
        )
        .context("sidebar notifications button")?;
        if let Some(project_id) = active_project_id.clone() {
            self.ui_items.push(UIItem {
                x: top_action_x,
                y: top_action_y,
                width: top_action_width,
                height: top_action_height,
                item_type: UIItemType::WorkspaceThreadNew(project_id),
            });
        }
        self.ui_items.push(UIItem {
            x: notification_action_x,
            y: notification_action_y,
            width: notification_action_size,
            height: notification_action_size,
            item_type: UIItemType::WorkspaceSidebarNotifications,
        });
        let top_action_icon_size = header_icon_size.min(top_action_height.saturating_sub(14));
        let top_action_text_max_width = top_action_width
            .saturating_sub(top_action_icon_size + top_action_gap + self.ui_px(SIDEBAR_INSET) * 2);
        let top_action_label_text = crate::i18n::tr("sidebar-new-thread");
        let top_action_label =
            self.ellipsize_ui_text(&ui_font, &top_action_label_text, top_action_text_max_width)?;
        let top_action_text_width = self
            .sidebar_text_width(&ui_font, top_action_label.as_ref())?
            .ceil()
            .max(0.0) as usize;
        let top_action_content_width =
            top_action_icon_size + top_action_gap + top_action_text_width;
        let top_action_content_x =
            top_action_x + top_action_width.saturating_sub(top_action_content_width) / 2;
        let top_action_icon_x = top_action_content_x;
        let top_action_icon_y =
            top_action_y + ((top_action_height.saturating_sub(top_action_icon_size)) / 2);
        let top_action_text_x = top_action_icon_x + top_action_icon_size + top_action_gap;
        let top_action_text_y =
            top_action_y + ((top_action_height.saturating_sub(ui_cell_height)) / 2);
        let top_action_fg = if active_project_id.is_some() {
            foreground
        } else {
            muted_fg
        };
        self.paint_sidebar_icon(
            layers,
            SvgIcon::CirclePlus,
            top_action_icon_x,
            top_action_icon_y,
            top_action_icon_size,
            top_action_fg,
        )?;
        self.paint_sidebar_text(
            layers,
            &ui_font,
            ui_metrics,
            top_action_label.as_ref(),
            top_action_text_x,
            top_action_text_y,
            top_action_text_width,
            top_action_fg,
        )?;
        let notification_icon_size =
            top_action_icon_size.min(notification_action_size.saturating_sub(14));
        let notification_count = workspace_threads::pending_work_notification_count();
        let notifications = workspace_threads::pending_work_notifications();
        // Treat either snapshot as authoritative for this frame if work state
        // changes between the two short store reads.
        let has_notifications = notification_count > 0 || !notifications.is_empty();
        let notification_badge_pulse = self.workspace_notification_badge_pulse(&notifications);
        self.paint_sidebar_icon(
            layers,
            SvgIcon::Bell,
            notification_action_x
                + ((notification_action_size.saturating_sub(notification_icon_size)) / 2),
            notification_action_y
                + ((notification_action_size.saturating_sub(notification_icon_size)) / 2),
            notification_icon_size,
            if notification_action_hovered {
                foreground
            } else {
                muted_fg
            },
        )?;
        if has_notifications {
            self.paint_workspace_notification_badge(
                layers,
                2,
                notification_action_x,
                notification_action_y,
                notification_action_size,
                notification_badge_pulse,
            )
            .context("sidebar notification badge initial paint")?;
        }
        let list_top = layout.list_top;
        let row_gap = self.ui_px(SIDEBAR_ROW_GAP);
        let max_scroll = {
            let viewport_height = content_bottom.saturating_sub(list_top);
            let total_height = Self::workspace_sidebar_scroll_height(
                &view,
                session_row_height,
                row_gap,
                viewport_height,
            );
            total_height.saturating_sub(viewport_height) as f32
        };
        let scroll_offset = self.workspace_sidebar_scroll_offset.clamp(0.0, max_scroll);
        if self.workspace_sidebar_preview_space_id.is_none() {
            // Writing the clamp back is what stops the sidebar staying scrolled
            // past the end after a list shrinks. It must not happen while
            // previewing a neighbouring Space: `max_scroll` then describes
            // *that* Space's extent, and a shorter one would drag the live
            // sidebar upward mid-gesture.
            self.workspace_sidebar_scroll_offset = scroll_offset;
        }
        let list_top_f = list_top as f32;
        let content_bottom_f = content_bottom as f32;
        let suppress_hover = self
            .current_mouse_event
            .as_ref()
            .is_some_and(|event| matches!(event.kind, WMEK::VertWheel(_) | WMEK::HorzWheel(_)));
        let mut virtual_y = 0usize;
        // Everything from here until the end of the project loop scrolls with
        // `virtual_y`. A space-swipe slides exactly this stretch and leaves the
        // chrome painted before and after it standing.
        let list_quads_start = layers.heap_mark();

        if !view.pinned_threads.is_empty() {
            let label_top = list_top_f + virtual_y as f32 - scroll_offset;
            let label_bottom = label_top + session_row_height as f32;
            let label_is_visible = label_bottom > list_top_f && label_top < content_bottom_f;
            if label_is_visible {
                let label_y = label_top.floor().max(0.0) as usize;
                let label_icon_size = header_icon_size.min(ui_cell_height).max(16);
                let label_icon_x = item_x + self.ui_px(SIDEBAR_INSET);
                let label_icon_y =
                    label_y + ((session_row_height.saturating_sub(label_icon_size)) / 2);
                let label_text_x = label_icon_x + label_icon_size + self.ui_px(SIDEBAR_ICON_GAP);
                self.paint_sidebar_icon(
                    layers,
                    SvgIcon::Pin,
                    label_icon_x,
                    label_icon_y,
                    label_icon_size,
                    muted_fg,
                )?;
                let section_label = crate::i18n::tr("sidebar-pinned");
                self.paint_sidebar_text(
                    layers,
                    &ui_font,
                    ui_metrics,
                    &section_label,
                    label_text_x,
                    label_y + ((session_row_height.saturating_sub(ui_cell_height)) / 2),
                    item_x
                        .saturating_add(item_width)
                        .saturating_sub(label_text_x + self.ui_px(SIDEBAR_INSET)),
                    muted_fg,
                )?;
            }
            virtual_y += session_row_height + row_gap;

            let pinned_x = item_x + self.ui_px(SIDEBAR_INSET);
            let pinned_width = item_width.saturating_sub(self.ui_px(SIDEBAR_INSET) * 2);
            let pinned_status_x = pinned_x + self.ui_px(SIDEBAR_INSET);
            let pinned_text_x = pinned_status_x
                + self.ui_px(SESSION_STATUS_ICON_SIZE)
                + self.ui_px(SIDEBAR_ICON_GAP)
                + 6;
            for session in &view.pinned_threads {
                let row_top = list_top_f + virtual_y as f32 - scroll_offset;
                let row_bottom = row_top + session_row_height as f32;
                let row_is_visible = row_bottom > list_top_f && row_top < content_bottom_f;
                let y = row_top.floor().max(0.0) as usize;
                let hit_y = row_top.max(list_top_f).floor().max(0.0) as usize;
                let hit_bottom = row_bottom.min(content_bottom_f).ceil().max(hit_y as f32) as usize;
                let hit_height = hit_bottom.saturating_sub(hit_y).max(1);

                if row_is_visible {
                    self.ui_items.push(UIItem {
                        x: pinned_x,
                        y: hit_y,
                        width: pinned_width,
                        height: hit_height,
                        item_type: UIItemType::WorkspaceThread(session.id.clone()),
                    });
                    let is_hovered = !suppress_hover
                        && self.is_pointer_over_ui_rect(
                            pinned_x,
                            y,
                            pinned_width,
                            session_row_height,
                        );
                    let is_renaming_session = self.is_renaming_sidebar_thread(&session.id);
                    let is_selected = self.is_workspace_sidebar_thread_selected(session);
                    if is_selected {
                        self.fill_rounded_rectangle_with_border(
                            layers,
                            0,
                            euclid::rect(
                                pinned_x as f32,
                                y as f32,
                                pinned_width as f32,
                                session_row_height as f32,
                            ),
                            selected_bg,
                            selected_border,
                            self.ui_f32(SIDEBAR_ROW_RADIUS) + 2.0,
                            CAPSULE_BORDER_WIDTH,
                        )
                        .context("sidebar selected pinned thread")?;
                    } else if is_hovered && !is_renaming_session {
                        self.fill_rounded_rectangle(
                            layers,
                            0,
                            euclid::rect(
                                pinned_x as f32,
                                y as f32,
                                pinned_width as f32,
                                session_row_height as f32,
                            ),
                            chrome.sidebar_row_hover_bg,
                            self.ui_f32(SIDEBAR_ROW_RADIUS) + 2.0,
                        )
                        .context("sidebar hovered pinned thread")?;
                    }

                    let text_y = y + ((session_row_height.saturating_sub(ui_cell_height)) / 2);
                    let action_size = session_row_height.saturating_sub(8).clamp(
                        self.ui_px(SESSION_ACTION_MIN_SIZE),
                        self.ui_px(SESSION_ACTION_MAX_SIZE),
                    );
                    let delete_x = pinned_x
                        .saturating_add(pinned_width)
                        .saturating_sub(self.ui_px(SIDEBAR_INSET) + action_size);
                    let pin_x = delete_x.saturating_sub(action_size + 4);
                    let action_y = y + ((session_row_height.saturating_sub(action_size)) / 2);
                    let text_right = if is_hovered && !is_renaming_session {
                        pin_x
                    } else {
                        pinned_x
                            .saturating_add(pinned_width)
                            .saturating_sub(self.ui_px(SIDEBAR_INSET))
                    };
                    self.paint_sidebar_thread_status(
                        layers,
                        session,
                        pinned_status_x,
                        y,
                        session_row_height,
                        &chrome,
                        foreground,
                    )?;
                    let title = self.sidebar_thread_title(&session.id, &session.name);
                    self.paint_sidebar_text(
                        layers,
                        &ui_font,
                        ui_metrics,
                        &title,
                        pinned_text_x,
                        text_y,
                        text_right.saturating_sub(pinned_text_x + self.ui_px(SIDEBAR_INSET)),
                        if is_selected { active_fg } else { foreground },
                    )?;

                    if is_hovered && !is_renaming_session {
                        let affordances = vec![
                            (
                                pin_x,
                                UIItemType::WorkspaceThreadPin(session.id.clone()),
                                SvgIcon::PinOff,
                                "sidebar unpin pinned thread button",
                            ),
                            (
                                delete_x,
                                UIItemType::WorkspaceThreadDelete(session.id.clone()),
                                SvgIcon::Trash2,
                                "sidebar delete pinned thread button",
                            ),
                        ];
                        for (x, item_type, icon, _context_name) in affordances {
                            let hovered =
                                self.is_pointer_over_ui_rect(x, action_y, action_size, action_size);
                            self.ui_items.push(UIItem {
                                x,
                                y: action_y,
                                width: action_size,
                                height: action_size,
                                item_type,
                            });
                            let action_icon_size = action_size
                                .saturating_sub(self.ui_px(SESSION_ACTION_ICON_INSET))
                                .max(header_icon_size);
                            self.paint_sidebar_icon(
                                layers,
                                icon,
                                x + ((action_size.saturating_sub(action_icon_size)) / 2),
                                action_y + ((action_size.saturating_sub(action_icon_size)) / 2),
                                action_icon_size,
                                if hovered {
                                    foreground
                                } else {
                                    muted_fg.mul_alpha(0.88)
                                },
                            )?;
                        }
                    }
                }

                virtual_y += session_row_height + row_gap;
            }
        }

        if !view.projects.is_empty() {
            if !view.pinned_threads.is_empty() {
                virtual_y += self.ui_px(WORKSPACE_SECTION_LABEL_GAP);
            }
            let label_top = list_top_f + virtual_y as f32 - scroll_offset;
            let label_bottom = label_top + session_row_height as f32;
            let label_is_visible = label_bottom > list_top_f && label_top < content_bottom_f;
            if label_is_visible {
                let label_y = label_top.floor().max(0.0) as usize;
                let button_y =
                    label_y + ((session_row_height.saturating_sub(section_button_size)) / 2);
                let button_x = item_x
                    .saturating_add(item_width)
                    .saturating_sub(section_button_size + self.ui_px(SIDEBAR_INSET));
                let button_hovered = !suppress_hover
                    && self.is_pointer_over_ui_rect(
                        button_x,
                        button_y,
                        section_button_size,
                        section_button_size,
                    );
                self.paint_sidebar_text(
                    layers,
                    &ui_font,
                    ui_metrics,
                    &crate::i18n::tr("sidebar-workspaces"),
                    item_x + self.ui_px(SIDEBAR_INSET),
                    label_y + ((session_row_height.saturating_sub(ui_cell_height)) / 2),
                    button_x.saturating_sub(item_x + self.ui_px(SIDEBAR_INSET) * 2),
                    muted_fg,
                )?;
                self.fill_rounded_rectangle(
                    layers,
                    1,
                    euclid::rect(
                        button_x as f32,
                        button_y as f32,
                        section_button_size as f32,
                        section_button_size as f32,
                    ),
                    if button_hovered {
                        chrome.sidebar_button_hover_bg
                    } else {
                        chrome.sidebar_button_bg
                    },
                    self.ui_f32(SIDEBAR_ROW_RADIUS),
                )
                .context("sidebar new project button")?;
                self.ui_items.push(UIItem {
                    x: button_x,
                    y: button_y,
                    width: section_button_size,
                    height: section_button_size,
                    item_type: UIItemType::ProjectNew,
                });
                let section_icon_size = (header_icon_size + 4).min(
                    section_button_size
                        .saturating_sub(self.ui_px(SIDEBAR_SECTION_ACTION_ICON_INSET)),
                );
                self.paint_sidebar_icon(
                    layers,
                    SvgIcon::FolderPlus,
                    button_x + ((section_button_size.saturating_sub(section_icon_size)) / 2),
                    button_y + ((section_button_size.saturating_sub(section_icon_size)) / 2),
                    section_icon_size,
                    if button_hovered { foreground } else { muted_fg },
                )?;
            }
            virtual_y += session_row_height + row_gap;
        }

        for (project_idx, project) in view.projects.iter().enumerate() {
            let row_top = list_top_f + virtual_y as f32 - scroll_offset;
            let row_bottom = row_top + session_row_height as f32;
            let row_is_visible = row_bottom > list_top_f && row_top < content_bottom_f;
            let y = row_top.floor().max(0.0) as usize;
            let hit_y = row_top.max(list_top_f).floor().max(0.0) as usize;
            let hit_bottom = row_bottom.min(content_bottom_f).ceil().max(hit_y as f32) as usize;
            let hit_height = hit_bottom.saturating_sub(hit_y).max(1);

            let disclosure_size = icon_size.min(22);
            let disclosure_x = item_x + self.ui_px(SIDEBAR_INSET);
            let project_icon_x = disclosure_x + disclosure_size + 4;
            let project_text_x = project_icon_x + icon_size + self.ui_px(SIDEBAR_ICON_GAP);
            let project_action_size = section_button_size.min(session_row_height.saturating_sub(8));
            let project_action_x = item_x
                .saturating_add(item_width)
                .saturating_sub(project_action_size + self.ui_px(SIDEBAR_INSET));
            let project_text_right = project_action_x;

            if row_is_visible {
                self.ui_items.push(UIItem {
                    x: item_x,
                    y: hit_y,
                    width: item_width,
                    height: hit_height,
                    item_type: UIItemType::Project(project.id.clone()),
                });
            }

            let icon_y = y + ((session_row_height.saturating_sub(icon_size)) / 2);
            let disclosure_y = y + ((session_row_height.saturating_sub(disclosure_size)) / 2);
            let text_y = y + ((session_row_height.saturating_sub(ui_cell_height)) / 2);
            if row_is_visible {
                self.ui_items.push(UIItem {
                    x: disclosure_x,
                    y: disclosure_y,
                    width: disclosure_size,
                    height: disclosure_size,
                    item_type: UIItemType::ProjectToggleThreads(project.id.clone()),
                });
                self.paint_sidebar_icon(
                    layers,
                    if project.threads_collapsed {
                        SvgIcon::ChevronRight
                    } else {
                        SvgIcon::ChevronDown
                    },
                    disclosure_x,
                    disclosure_y,
                    disclosure_size,
                    muted_fg,
                )?;
                let remote_brand = if project.is_remote {
                    project.distro.as_deref().and_then(distro_to_icon)
                } else {
                    None
                };
                if let Some(brand) = remote_brand {
                    // Detected remote OS: show its brand logo in full color.
                    self.paint_sidebar_brand_icon(
                        layers,
                        brand,
                        project_icon_x,
                        icon_y,
                        icon_size,
                    )?;
                } else {
                    let project_icon = if project.is_remote {
                        // Remote host without a detected OS: a globe marks it as
                        // distinct from local folder projects in the mixed list.
                        SvgIcon::Globe
                    } else if project.threads_collapsed {
                        SvgIcon::Folder
                    } else {
                        SvgIcon::FolderOpen
                    };
                    self.paint_sidebar_icon(
                        layers,
                        project_icon,
                        project_icon_x,
                        icon_y,
                        icon_size,
                        muted_fg,
                    )?;
                }
                let project_title = self.sidebar_project_title(&project.id, &project.name);
                self.paint_sidebar_text(
                    layers,
                    &ui_font,
                    ui_metrics,
                    &project_title,
                    project_text_x,
                    text_y,
                    project_text_right.saturating_sub(project_text_x + self.ui_px(SIDEBAR_INSET)),
                    muted_fg,
                )?;
            }

            if row_is_visible {
                let project_action_y =
                    y + ((session_row_height.saturating_sub(project_action_size)) / 2);
                let project_action_hovered = !suppress_hover
                    && self.is_pointer_over_ui_rect(
                        project_action_x,
                        project_action_y,
                        project_action_size,
                        project_action_size,
                    );
                self.fill_rounded_rectangle(
                    layers,
                    1,
                    euclid::rect(
                        project_action_x as f32,
                        project_action_y as f32,
                        project_action_size as f32,
                        project_action_size as f32,
                    ),
                    if project_action_hovered {
                        chrome.sidebar_button_hover_bg
                    } else {
                        chrome.sidebar_button_bg
                    },
                    self.ui_f32(SIDEBAR_ROW_RADIUS),
                )
                .context("sidebar new thread button")?;
                self.ui_items.push(UIItem {
                    x: project_action_x,
                    y: project_action_y,
                    width: project_action_size,
                    height: project_action_size,
                    item_type: UIItemType::WorkspaceThreadNew(project.id.clone()),
                });
                let action_icon_size = (header_icon_size + 4).min(
                    project_action_size
                        .saturating_sub(self.ui_px(SIDEBAR_SECTION_ACTION_ICON_INSET)),
                );
                self.paint_sidebar_icon(
                    layers,
                    SvgIcon::Plus,
                    project_action_x + ((project_action_size.saturating_sub(action_icon_size)) / 2),
                    project_action_y + ((project_action_size.saturating_sub(action_icon_size)) / 2),
                    action_icon_size,
                    if project.is_active || project_action_hovered {
                        foreground
                    } else {
                        muted_fg
                    },
                )?;
            }

            virtual_y += session_row_height + row_gap;

            if !project.threads_collapsed {
                let session_x =
                    item_x + self.ui_px(SIDEBAR_INSET) * 3 + self.ui_px(SESSION_ROW_SIDE_PADDING);
                let session_width = item_width.saturating_sub(
                    self.ui_px(SIDEBAR_INSET) * 3 + self.ui_px(SESSION_ROW_SIDE_PADDING) * 2,
                );
                let session_status_x = session_x + self.ui_px(SIDEBAR_INSET) + 2;
                let session_text_x = session_status_x
                    + self.ui_px(SESSION_STATUS_ICON_SIZE)
                    + self.ui_px(SIDEBAR_ICON_GAP)
                    + 6;

                for session in &project.threads {
                    let row_top = list_top_f + virtual_y as f32 - scroll_offset;
                    let row_bottom = row_top + session_row_height as f32;
                    let row_is_visible = row_bottom > list_top_f && row_top < content_bottom_f;
                    let y = row_top.floor().max(0.0) as usize;
                    let hit_y = row_top.max(list_top_f).floor().max(0.0) as usize;
                    let hit_bottom =
                        row_bottom.min(content_bottom_f).ceil().max(hit_y as f32) as usize;
                    let hit_height = hit_bottom.saturating_sub(hit_y).max(1);

                    let is_selected = self.is_workspace_sidebar_thread_selected(session);
                    if row_is_visible && is_selected {
                        self.fill_rounded_rectangle_with_border(
                            layers,
                            0,
                            euclid::rect(
                                session_x as f32,
                                y as f32,
                                session_width as f32,
                                session_row_height as f32,
                            ),
                            selected_bg,
                            selected_border,
                            self.ui_f32(SIDEBAR_ROW_RADIUS) + 2.0,
                            CAPSULE_BORDER_WIDTH,
                        )
                        .context("sidebar selected thread")?;
                    }

                    let text_y = y + ((session_row_height.saturating_sub(ui_cell_height)) / 2);
                    if row_is_visible {
                        self.ui_items.push(UIItem {
                            x: session_x,
                            y: hit_y,
                            width: session_width,
                            height: hit_height,
                            item_type: UIItemType::WorkspaceThread(session.id.clone()),
                        });
                        let is_hovered = !suppress_hover
                            && self.is_pointer_over_ui_rect(
                                session_x,
                                y,
                                session_width,
                                session_row_height,
                            );
                        let is_renaming_session = self.is_renaming_sidebar_thread(&session.id);
                        let action_size = session_row_height.saturating_sub(8).clamp(
                            self.ui_px(SESSION_ACTION_MIN_SIZE),
                            self.ui_px(SESSION_ACTION_MAX_SIZE),
                        );
                        let delete_x = session_x
                            .saturating_add(session_width)
                            .saturating_sub(self.ui_px(SIDEBAR_INSET) + action_size);
                        let pin_x = delete_x.saturating_sub(action_size + 4);
                        let action_y = y + ((session_row_height.saturating_sub(action_size)) / 2);
                        let text_right = if is_hovered && !is_renaming_session {
                            pin_x
                        } else {
                            session_x
                                .saturating_add(session_width)
                                .saturating_sub(self.ui_px(SIDEBAR_INSET))
                        };
                        let text_width =
                            text_right.saturating_sub(session_text_x + self.ui_px(SIDEBAR_INSET));
                        if is_hovered && !is_selected && !is_renaming_session {
                            self.fill_rounded_rectangle(
                                layers,
                                0,
                                euclid::rect(
                                    session_x as f32,
                                    y as f32,
                                    session_width as f32,
                                    session_row_height as f32,
                                ),
                                chrome.sidebar_row_hover_bg,
                                self.ui_f32(SIDEBAR_ROW_RADIUS) + 2.0,
                            )
                            .context("sidebar hovered thread")?;
                        }
                        self.paint_sidebar_thread_status(
                            layers,
                            session,
                            session_status_x,
                            y,
                            session_row_height,
                            &chrome,
                            foreground,
                        )?;
                        let session_title = self.sidebar_thread_title(&session.id, &session.name);
                        self.paint_sidebar_text(
                            layers,
                            &ui_font,
                            ui_metrics,
                            &session_title,
                            session_text_x,
                            text_y,
                            text_width,
                            if is_selected { active_fg } else { foreground },
                        )?;
                        if is_hovered && !is_renaming_session {
                            let pin_icon = if session.is_pinned {
                                SvgIcon::PinOff
                            } else {
                                SvgIcon::Pin
                            };
                            for (x, item_type, icon, _context_name) in [
                                (
                                    pin_x,
                                    UIItemType::WorkspaceThreadPin(session.id.clone()),
                                    pin_icon,
                                    "sidebar pin thread button",
                                ),
                                (
                                    delete_x,
                                    UIItemType::WorkspaceThreadDelete(session.id.clone()),
                                    SvgIcon::Trash2,
                                    "sidebar delete thread button",
                                ),
                            ] {
                                let hovered = self.is_pointer_over_ui_rect(
                                    x,
                                    action_y,
                                    action_size,
                                    action_size,
                                );
                                self.ui_items.push(UIItem {
                                    x,
                                    y: action_y,
                                    width: action_size,
                                    height: action_size,
                                    item_type,
                                });
                                let action_icon_size = action_size
                                    .saturating_sub(self.ui_px(SESSION_ACTION_ICON_INSET))
                                    .max(header_icon_size);
                                self.paint_sidebar_icon(
                                    layers,
                                    icon,
                                    x + ((action_size.saturating_sub(action_icon_size)) / 2),
                                    action_y + ((action_size.saturating_sub(action_icon_size)) / 2),
                                    action_icon_size,
                                    if hovered {
                                        foreground
                                    } else {
                                        muted_fg.mul_alpha(0.88)
                                    },
                                )?;
                            }
                        }
                    }

                    virtual_y += session_row_height + row_gap;
                }
            }

            if project_idx + 1 < view.projects.len() {
                virtual_y += self.ui_px(WORKSPACE_GROUP_EXTRA_GAP);
            }
        }

        if !view.ref_groups.is_empty() {
            if !view.pinned_threads.is_empty() || !view.projects.is_empty() {
                virtual_y += self.ui_px(WORKSPACE_GROUP_EXTRA_GAP);
            }
            // Reference groups render exactly like project folders: chevron,
            // machine icon, machine name, then indented thread rows. The
            // link icon in the header is what tells a reference group apart
            // from a real project group with the same machine name.
            let group_space_id = self.workspace_sidebar_space_id().to_string();
            for (group_idx, group) in view.ref_groups.iter().enumerate() {
                if group_idx > 0 {
                    virtual_y += self.ui_px(WORKSPACE_GROUP_EXTRA_GAP);
                }
                let row_top = list_top_f + virtual_y as f32 - scroll_offset;
                let row_bottom = row_top + session_row_height as f32;
                let row_is_visible = row_bottom > list_top_f && row_top < content_bottom_f;
                let y = row_top.floor().max(0.0) as usize;

                let disclosure_size = icon_size.min(22);
                let disclosure_x = item_x + self.ui_px(SIDEBAR_INSET);
                let group_icon_x = disclosure_x + disclosure_size + 4;
                let group_text_x = group_icon_x + icon_size + self.ui_px(SIDEBAR_ICON_GAP);
                let icon_y = y + ((session_row_height.saturating_sub(icon_size)) / 2);
                let disclosure_y = y + ((session_row_height.saturating_sub(disclosure_size)) / 2);
                let text_y = y + ((session_row_height.saturating_sub(ui_cell_height)) / 2);
                let group_action_size =
                    section_button_size.min(session_row_height.saturating_sub(8));
                let group_action_x = item_x
                    .saturating_add(item_width)
                    .saturating_sub(group_action_size + self.ui_px(SIDEBAR_INSET));
                if row_is_visible {
                    let group_key = format!("{group_space_id}::{}", group.key);
                    self.ui_items.push(UIItem {
                        x: item_x,
                        y,
                        width: item_width,
                        height: session_row_height,
                        item_type: UIItemType::ThreadRefGroupToggle(group_key),
                    });
                    self.paint_sidebar_icon(
                        layers,
                        if group.collapsed {
                            SvgIcon::ChevronRight
                        } else {
                            SvgIcon::ChevronDown
                        },
                        disclosure_x,
                        disclosure_y,
                        disclosure_size,
                        muted_fg,
                    )?;
                    self.paint_sidebar_icon(
                        layers,
                        SvgIcon::Link2,
                        group_icon_x,
                        icon_y,
                        icon_size,
                        if group.attached {
                            muted_fg
                        } else {
                            muted_fg.mul_alpha(0.6)
                        },
                    )?;
                    // Header: origin-Space name, with the machine as a
                    // right-aligned badge. The name is the primary info, so
                    // when both cannot fit the badge is dropped first and
                    // the name keeps the full width (ellipsized as needed).
                    let header_fg = if group.attached {
                        muted_fg
                    } else {
                        muted_fg.mul_alpha(0.6)
                    };
                    let text_avail =
                        group_action_x.saturating_sub(group_text_x + self.ui_px(SIDEBAR_INSET));
                    let badge_pad = self.ui_px(6);
                    let badge_gap = self.ui_px(8);
                    // Badge typography: clearly smaller than the header name
                    // so it reads as metadata, matching the height of the
                    // "+" button beside it.
                    let badge_font = self
                        .fonts
                        .title_font_with_size(
                            crate::native_settings::sidebar_font_size() * 0.82,
                        )
                        .ok();
                    // Local needs no badge: the badge answers "which
                    // machine", and this machine is the answer by default.
                    let show_badge = group.machine_label != group.label
                        && !group.machine_label.is_empty()
                        && group.machine_label != "Local";
                    let badge = match (&badge_font, show_badge) {
                        (Some(badge_font), true) => {
                            let badge_metrics =
                                RenderMetrics::with_font_metrics(&badge_font.metrics());
                            (|| {
                                let text_w = self
                                    .cached_ui_text_advance(
                                        badge_font,
                                        &badge_metrics,
                                        &group.machine_label,
                                    )
                                    .ok()?
                                    .ceil() as usize;
                                let name_w = self
                                    .cached_ui_text_advance(
                                        &ui_font,
                                        &ui_metrics,
                                        &group.label,
                                    )
                                    .ok()?
                                    .ceil() as usize;
                                let badge_w = text_w + badge_pad * 2;
                                (name_w + badge_gap + badge_w <= text_avail)
                                    .then_some((badge_w, badge_metrics))
                            })()
                        }
                        _ => None,
                    };
                    let name_avail = match &badge {
                        Some((badge_w, _)) => {
                            text_avail.saturating_sub(badge_w + badge_gap)
                        }
                        None => text_avail,
                    };
                    self.paint_sidebar_text(
                        layers,
                        &ui_font,
                        ui_metrics,
                        &group.label,
                        group_text_x,
                        text_y,
                        name_avail,
                        header_fg,
                    )?;
                    if let (Some((badge_w, badge_metrics)), Some(badge_font)) =
                        (badge, &badge_font)
                    {
                        // Same height as the "+" button so the trailing
                        // cluster reads as one aligned row of controls.
                        let badge_h = group_action_size;
                        let badge_x = group_action_x
                            .saturating_sub(self.ui_px(SIDEBAR_INSET) / 2 + badge_w);
                        let badge_y =
                            y + ((session_row_height.saturating_sub(badge_h)) / 2);
                        self.fill_rounded_rectangle(
                            layers,
                            1,
                            euclid::rect(
                                badge_x as f32,
                                badge_y as f32,
                                badge_w as f32,
                                badge_h as f32,
                            ),
                            chrome.sidebar_button_bg,
                            self.ui_f32(SIDEBAR_ROW_RADIUS),
                        )
                        .context("sidebar ref group machine badge")?;
                        let badge_cell_h = badge_metrics.cell_size.height as usize;
                        self.paint_sidebar_text(
                            layers,
                            badge_font,
                            badge_metrics,
                            &group.machine_label,
                            badge_x + badge_pad,
                            badge_y + ((badge_h.saturating_sub(badge_cell_h)) / 2),
                            badge_w.saturating_sub(badge_pad),
                            header_fg,
                        )?;
                    }

                    // The group "+": create a thread in the origin project
                    // and auto-reference it here — same affordance as the
                    // project "+", inert (and dimmed) while the origin
                    // machine is unreachable.
                    let group_action_y =
                        y + ((session_row_height.saturating_sub(group_action_size)) / 2);
                    let group_action_hovered = group.attached
                        && !suppress_hover
                        && self.is_pointer_over_ui_rect(
                            group_action_x,
                            group_action_y,
                            group_action_size,
                            group_action_size,
                        );
                    self.fill_rounded_rectangle(
                        layers,
                        1,
                        euclid::rect(
                            group_action_x as f32,
                            group_action_y as f32,
                            group_action_size as f32,
                            group_action_size as f32,
                        ),
                        if group_action_hovered {
                            chrome.sidebar_button_hover_bg
                        } else if group.attached {
                            chrome.sidebar_button_bg
                        } else {
                            chrome.sidebar_button_bg.mul_alpha(0.4)
                        },
                        self.ui_f32(SIDEBAR_ROW_RADIUS),
                    )
                    .context("sidebar ref group new thread button")?;
                    if group.attached {
                        self.ui_items.push(UIItem {
                            x: group_action_x,
                            y: group_action_y,
                            width: group_action_size,
                            height: group_action_size,
                            item_type: UIItemType::ThreadRefGroupNewThread(group.key.clone()),
                        });
                    }
                    let action_icon_size = (header_icon_size + 4).min(
                        group_action_size
                            .saturating_sub(self.ui_px(SIDEBAR_SECTION_ACTION_ICON_INSET)),
                    );
                    self.paint_sidebar_icon(
                        layers,
                        SvgIcon::Plus,
                        group_action_x
                            + ((group_action_size.saturating_sub(action_icon_size)) / 2),
                        group_action_y
                            + ((group_action_size.saturating_sub(action_icon_size)) / 2),
                        action_icon_size,
                        if group_action_hovered {
                            foreground
                        } else if group.attached {
                            muted_fg
                        } else {
                            muted_fg.mul_alpha(0.5)
                        },
                    )?;
                }
                virtual_y += session_row_height + row_gap;

                if group.collapsed {
                    continue;
                }
                let session_x =
                    item_x + self.ui_px(SIDEBAR_INSET) * 3 + self.ui_px(SESSION_ROW_SIDE_PADDING);
                let session_width = item_width.saturating_sub(
                    self.ui_px(SIDEBAR_INSET) * 3 + self.ui_px(SESSION_ROW_SIDE_PADDING) * 2,
                );
                let session_status_x = session_x + self.ui_px(SIDEBAR_INSET) + 2;
                let session_text_x = session_status_x
                    + self.ui_px(SESSION_STATUS_ICON_SIZE)
                    + self.ui_px(SIDEBAR_ICON_GAP)
                    + 6;
                for reference in &group.threads {
                    let session = &reference.thread;
                    let row_top = list_top_f + virtual_y as f32 - scroll_offset;
                    let row_bottom = row_top + session_row_height as f32;
                    let row_is_visible = row_bottom > list_top_f && row_top < content_bottom_f;
                    let y = row_top.floor().max(0.0) as usize;
                    let hit_y = row_top.max(list_top_f).floor().max(0.0) as usize;
                    let hit_bottom =
                        row_bottom.min(content_bottom_f).ceil().max(hit_y as f32) as usize;
                    let hit_height = hit_bottom.saturating_sub(hit_y).max(1);

                    if row_is_visible {
                        // A dangling ref has nothing to activate; it paints
                        // (greyed, removable) but registers no activation
                        // target.
                        if !reference.dangling {
                            self.ui_items.push(UIItem {
                                x: session_x,
                                y: hit_y,
                                width: session_width,
                                height: hit_height,
                                item_type: UIItemType::WorkspaceThread(session.id.clone()),
                            });
                        }
                        let is_hovered = !suppress_hover
                            && self.is_pointer_over_ui_rect(
                                session_x,
                                y,
                                session_width,
                                session_row_height,
                            );
                        let is_selected = self.is_workspace_sidebar_thread_selected(session);
                        let dimmed = reference.dangling || !reference.origin_domain_attached;
                        if is_selected {
                            self.fill_rounded_rectangle_with_border(
                                layers,
                                0,
                                euclid::rect(
                                    session_x as f32,
                                    y as f32,
                                    session_width as f32,
                                    session_row_height as f32,
                                ),
                                selected_bg,
                                selected_border,
                                self.ui_f32(SIDEBAR_ROW_RADIUS) + 2.0,
                                CAPSULE_BORDER_WIDTH,
                            )
                            .context("sidebar selected thread ref")?;
                        } else if is_hovered {
                            self.fill_rounded_rectangle(
                                layers,
                                0,
                                euclid::rect(
                                    session_x as f32,
                                    y as f32,
                                    session_width as f32,
                                    session_row_height as f32,
                                ),
                                chrome.sidebar_row_hover_bg,
                                self.ui_f32(SIDEBAR_ROW_RADIUS) + 2.0,
                            )
                            .context("sidebar hovered thread ref")?;
                        }

                        let text_y =
                            y + ((session_row_height.saturating_sub(ui_cell_height)) / 2);
                        let action_size = session_row_height.saturating_sub(8).clamp(
                            self.ui_px(SESSION_ACTION_MIN_SIZE),
                            self.ui_px(SESSION_ACTION_MAX_SIZE),
                        );
                        let delete_x = session_x
                            .saturating_add(session_width)
                            .saturating_sub(self.ui_px(SIDEBAR_INSET) + action_size);
                        let action_y =
                            y + ((session_row_height.saturating_sub(action_size)) / 2);
                        let text_right = if is_hovered {
                            delete_x
                        } else {
                            session_x
                                .saturating_add(session_width)
                                .saturating_sub(self.ui_px(SIDEBAR_INSET))
                        };
                        if !dimmed {
                            self.paint_sidebar_thread_status(
                                layers,
                                session,
                                session_status_x,
                                y,
                                session_row_height,
                                &chrome,
                                foreground,
                            )?;
                        }
                        let text_fg = if dimmed {
                            muted_fg.mul_alpha(0.7)
                        } else if is_selected {
                            active_fg
                        } else {
                            foreground
                        };
                        self.paint_sidebar_text(
                            layers,
                            &ui_font,
                            ui_metrics,
                            &session.name,
                            session_text_x,
                            text_y,
                            text_right
                                .saturating_sub(session_text_x + self.ui_px(SIDEBAR_INSET)),
                            text_fg,
                        )?;

                        if is_hovered {
                            // An X, not a trash can: removing a reference
                            // leaves the origin thread running untouched.
                            let hovered = self.is_pointer_over_ui_rect(
                                delete_x,
                                action_y,
                                action_size,
                                action_size,
                            );
                            self.ui_items.push(UIItem {
                                x: delete_x,
                                y: action_y,
                                width: action_size,
                                height: action_size,
                                item_type: UIItemType::WorkspaceThreadDelete(session.id.clone()),
                            });
                            let action_icon_size = action_size
                                .saturating_sub(self.ui_px(SESSION_ACTION_ICON_INSET))
                                .max(header_icon_size);
                            self.paint_sidebar_icon(
                                layers,
                                SvgIcon::X,
                                delete_x
                                    + ((action_size.saturating_sub(action_icon_size)) / 2),
                                action_y
                                    + ((action_size.saturating_sub(action_icon_size)) / 2),
                                action_icon_size,
                                if hovered {
                                    foreground
                                } else {
                                    muted_fg.mul_alpha(0.88)
                                },
                            )?;
                        }
                    }

                    virtual_y += session_row_height + row_gap;
                }
            }
        }

        self.workspace_sidebar_list_quads = list_quads_start.zip(layers.heap_mark());

        let header_mask_height = list_top.saturating_sub(panel_y);
        if header_mask_height > 0 {
            self.filled_rectangle(
                layers,
                2,
                euclid::rect(
                    panel_x as f32,
                    panel_y as f32,
                    panel_width as f32,
                    header_mask_height as f32,
                ),
                sidebar_bg,
            )
            .context("sidebar header scroll mask")?;
            if show_sidebar_toolbar {
                let toggle_hovered = self.is_pointer_over_ui_rect(
                    sidebar_toggle_x,
                    sidebar_toggle_y,
                    sidebar_toggle_size,
                    sidebar_toggle_size,
                );
                if toggle_hovered {
                    self.fill_rounded_rectangle(
                        layers,
                        2,
                        euclid::rect(
                            sidebar_toggle_x as f32,
                            sidebar_toggle_y as f32,
                            sidebar_toggle_size as f32,
                            sidebar_toggle_size as f32,
                        ),
                        chrome.sidebar_button_hover_bg,
                        self.ui_f32(SIDEBAR_ROW_RADIUS),
                    )
                    .context("sidebar toolbar toggle button repaint")?;
                }
                self.paint_sidebar_icon(
                    layers,
                    self.workspace_sidebar_toggle_icon(),
                    sidebar_toggle_x
                        + ((sidebar_toggle_size.saturating_sub(sidebar_toggle_icon_size)) / 2),
                    sidebar_toggle_y
                        + ((sidebar_toggle_size.saturating_sub(sidebar_toggle_icon_size)) / 2),
                    sidebar_toggle_icon_size,
                    if toggle_hovered { foreground } else { muted_fg },
                )?;
            }
            let space_menu_hovered = self.is_pointer_over_ui_rect(
                space_menu_x,
                space_menu_y,
                space_menu_width,
                space_menu_height,
            );
            if space_menu_hovered {
                self.fill_rounded_rectangle(
                    layers,
                    2,
                    euclid::rect(
                        space_menu_x as f32,
                        space_menu_y as f32,
                        space_menu_width as f32,
                        space_menu_height as f32,
                    ),
                    chrome.sidebar_button_hover_bg,
                    self.ui_f32(SIDEBAR_ROW_RADIUS + 4.0),
                )
                .context("sidebar space menu button repaint")?;
            }
            if connection_state == SpaceConnectionState::Connecting {
                self.paint_spinning_ui_icon(
                    layers,
                    2,
                    space_icon,
                    space_icon_x,
                    space_icon_y,
                    space_icon_size,
                    space_icon_color,
                )?;
            } else {
                self.paint_sidebar_icon(
                    layers,
                    space_icon,
                    space_icon_x,
                    space_icon_y,
                    space_icon_size,
                    space_icon_color,
                )?;
            }
            self.paint_sidebar_text(
                layers,
                &ui_font,
                ui_metrics,
                space_label.as_ref(),
                space_text_x,
                space_menu_y + ((space_menu_height.saturating_sub(ui_cell_height)) / 2),
                space_text_right.saturating_sub(space_text_x),
                foreground,
            )?;
            self.paint_sidebar_icon(
                layers,
                SvgIcon::Ellipsis,
                space_action_icon_x,
                space_action_icon_y,
                space_action_icon_size,
                if space_menu_hovered {
                    foreground
                } else {
                    muted_fg
                },
            )?;
            if layout.reconnect_row_height > 0 {
                self.paint_space_reconnect_row(
                    layers,
                    2,
                    item_x + self.ui_px(SIDEBAR_INSET),
                    space_menu_y + space_menu_height + self.ui_px(SIDEBAR_INSET),
                    item_width.saturating_sub(self.ui_px(SIDEBAR_INSET) * 2),
                    layout.reconnect_row_height,
                    &ui_font,
                    ui_metrics,
                    ui_cell_height,
                    false,
                )?;
            }
            let top_action_hovered = active_project_id.is_some()
                && self.is_pointer_over_ui_rect(
                    top_action_x,
                    top_action_y,
                    top_action_width,
                    top_action_height,
                );
            self.fill_rounded_rectangle(
                layers,
                2,
                euclid::rect(
                    top_action_x as f32,
                    top_action_y as f32,
                    top_action_width as f32,
                    top_action_height as f32,
                ),
                if top_action_hovered {
                    chrome.sidebar_button_hover_bg
                } else {
                    chrome.sidebar_button_bg
                },
                self.ui_f32(SIDEBAR_ROW_RADIUS + 4.0),
            )
            .context("sidebar add thread button repaint")?;
            let notification_action_hovered = self.is_pointer_over_ui_rect(
                notification_action_x,
                notification_action_y,
                notification_action_size,
                notification_action_size,
            );
            self.fill_rounded_rectangle(
                layers,
                2,
                euclid::rect(
                    notification_action_x as f32,
                    notification_action_y as f32,
                    notification_action_size as f32,
                    notification_action_size as f32,
                ),
                if notification_action_hovered {
                    chrome.sidebar_button_hover_bg
                } else {
                    chrome.sidebar_button_bg
                },
                self.ui_f32(SIDEBAR_ROW_RADIUS + 4.0),
            )
            .context("sidebar notifications button repaint")?;
            let top_action_fg = if active_project_id.is_some() {
                foreground
            } else {
                muted_fg
            };
            self.paint_sidebar_icon(
                layers,
                SvgIcon::CirclePlus,
                top_action_icon_x,
                top_action_icon_y,
                top_action_icon_size,
                top_action_fg,
            )?;
            self.paint_sidebar_text(
                layers,
                &ui_font,
                ui_metrics,
                top_action_label.as_ref(),
                top_action_text_x,
                top_action_text_y,
                top_action_text_width,
                top_action_fg,
            )?;
            self.paint_sidebar_icon(
                layers,
                SvgIcon::Bell,
                notification_action_x
                    + ((notification_action_size.saturating_sub(notification_icon_size)) / 2),
                notification_action_y
                    + ((notification_action_size.saturating_sub(notification_icon_size)) / 2),
                notification_icon_size,
                if notification_action_hovered {
                    foreground
                } else {
                    muted_fg
                },
            )?;
            if has_notifications {
                // The layer-2 header mask above covers the initial header
                // paint. Repaint the badge here too, after the button and bell,
                // so the unread indicator is actually visible.
                self.paint_workspace_notification_badge(
                    layers,
                    2,
                    notification_action_x,
                    notification_action_y,
                    notification_action_size,
                    notification_badge_pulse,
                )
                .context("sidebar notification badge repaint")?;
            }
        }

        if max_scroll > 0.0 && scroll_offset > 0.0 {
            let fade_height = self
                .ui_px(SIDEBAR_TOP_FADE_HEIGHT)
                .min(content_bottom.saturating_sub(list_top));
            for step in 0..fade_height {
                let progress = step as f32 / fade_height as f32;
                let alpha = 1.0 - progress * progress * (3.0 - 2.0 * progress);
                self.filled_rectangle(
                    layers,
                    2,
                    euclid::rect(
                        panel_x as f32,
                        (list_top + step) as f32,
                        panel_width as f32,
                        1.0,
                    ),
                    sidebar_bg.mul_alpha(alpha),
                )
                .context("sidebar top scroll fade")?;
            }
        }

        let bottom_mask_height = panel_y
            .saturating_add(panel_height)
            .saturating_sub(content_bottom);
        if bottom_mask_height > 0 {
            self.filled_rectangle(
                layers,
                2,
                euclid::rect(
                    panel_x as f32,
                    content_bottom as f32,
                    panel_width as f32,
                    bottom_mask_height as f32,
                ),
                sidebar_bg,
            )
            .context("sidebar bottom scroll mask")?;
        }

        if settings_footer_height > 0 {
            let fade_height = self
                .ui_px(SIDEBAR_SETTINGS_FADE_HEIGHT)
                .min(settings_footer_y.saturating_sub(panel_y))
                .min(content_bottom.saturating_sub(panel_y));
            if max_scroll > 0.0 && fade_height > 0 {
                for step in 0..fade_height {
                    let progress = (step + 1) as f32 / fade_height as f32;
                    self.filled_rectangle(
                        layers,
                        2,
                        euclid::rect(
                            panel_x as f32,
                            (settings_footer_y.saturating_sub(fade_height) + step) as f32,
                            panel_width as f32,
                            1.0,
                        ),
                        sidebar_bg.mul_alpha(progress * progress * (3.0 - 2.0 * progress)),
                    )
                    .context("sidebar settings fade")?;
                }
            }

            self.filled_rectangle(
                layers,
                2,
                euclid::rect(
                    panel_x as f32,
                    settings_footer_y as f32,
                    panel_width as f32,
                    settings_footer_height as f32,
                ),
                sidebar_bg,
            )
            .context("sidebar settings footer background")?;

            let settings_row_x = item_x + self.ui_px(SIDEBAR_SETTINGS_ROW_SIDE_PADDING);
            let settings_row_y = (settings_footer_y + self.ui_px(SIDEBAR_SETTINGS_ROW_TOP_PADDING))
                .saturating_sub(self.ui_px(SIDEBAR_SETTINGS_ROW_LIFT));
            let settings_row_width =
                item_width.saturating_sub(self.ui_px(SIDEBAR_SETTINGS_ROW_SIDE_PADDING) * 2);
            let settings_row_height = settings_footer_height
                .saturating_sub(
                    self.ui_px(SIDEBAR_SETTINGS_ROW_TOP_PADDING)
                        + self.ui_px(SIDEBAR_SETTINGS_ROW_BOTTOM_PADDING),
                )
                .max(1);
            let settings_action_size = settings_row_height.max(1);
            let settings_action_x = settings_row_x
                .saturating_add(settings_row_width)
                .saturating_sub(settings_action_size);
            let settings_action_y = settings_row_y;
            // SSH hosts sits left of view options; Live Overview sits left of
            // SSH so the settings label remains the primary footer action.
            let ssh_action_x = settings_action_x
                .saturating_sub(settings_action_size + self.ui_px(SIDEBAR_INSET) / 2);
            let ssh_action_y = settings_row_y;
            let overview_action_x =
                ssh_action_x.saturating_sub(settings_action_size + self.ui_px(SIDEBAR_INSET) / 2);
            let overview_action_y = settings_row_y;
            let ssh_action_hovered = self.is_pointer_over_ui_rect(
                ssh_action_x,
                ssh_action_y,
                settings_action_size,
                settings_action_size,
            );
            let overview_action_hovered = self.is_pointer_over_ui_rect(
                overview_action_x,
                overview_action_y,
                settings_action_size,
                settings_action_size,
            );
            let settings_body_width = overview_action_x
                .saturating_sub(settings_row_x)
                .saturating_sub(self.ui_px(SIDEBAR_INSET) / 2);
            let settings_hovered = self.is_pointer_over_ui_rect(
                settings_row_x,
                settings_row_y,
                settings_body_width,
                settings_row_height,
            );
            let settings_options_hovered = self.is_pointer_over_ui_rect(
                settings_action_x,
                settings_action_y,
                settings_action_size,
                settings_action_size,
            );
            if settings_hovered {
                self.fill_rounded_rectangle(
                    layers,
                    2,
                    euclid::rect(
                        settings_row_x as f32,
                        settings_row_y as f32,
                        settings_body_width as f32,
                        settings_row_height as f32,
                    ),
                    chrome.sidebar_row_hover_bg,
                    self.ui_f32(SIDEBAR_ROW_RADIUS + 4.0),
                )
                .context("sidebar settings button hover")?;
            }
            self.ui_items.push(UIItem {
                x: settings_row_x,
                y: settings_row_y,
                width: settings_body_width,
                height: settings_row_height,
                item_type: UIItemType::WorkspaceSidebarSettings,
            });
            if settings_options_hovered {
                self.fill_rounded_rectangle(
                    layers,
                    2,
                    euclid::rect(
                        settings_action_x as f32,
                        settings_action_y as f32,
                        settings_action_size as f32,
                        settings_action_size as f32,
                    ),
                    chrome.sidebar_button_hover_bg,
                    self.ui_f32(SIDEBAR_ROW_RADIUS + 4.0),
                )
                .context("sidebar settings view options hover")?;
            }
            self.ui_items.push(UIItem {
                x: settings_action_x,
                y: settings_action_y,
                width: settings_action_size,
                height: settings_action_size,
                item_type: UIItemType::WorkspaceSidebarViewOptions,
            });
            if ssh_action_hovered {
                self.fill_rounded_rectangle(
                    layers,
                    2,
                    euclid::rect(
                        ssh_action_x as f32,
                        ssh_action_y as f32,
                        settings_action_size as f32,
                        settings_action_size as f32,
                    ),
                    chrome.sidebar_button_hover_bg,
                    self.ui_f32(SIDEBAR_ROW_RADIUS + 4.0),
                )
                .context("sidebar ssh hosts hover")?;
            }
            self.ui_items.push(UIItem {
                x: ssh_action_x,
                y: ssh_action_y,
                width: settings_action_size,
                height: settings_action_size,
                item_type: UIItemType::WorkspaceSidebarSshHosts,
            });
            {
                let ssh_icon_size = icon_size.min(settings_row_height.saturating_sub(8));
                self.paint_sidebar_icon(
                    layers,
                    SvgIcon::Link2,
                    ssh_action_x + ((settings_action_size.saturating_sub(ssh_icon_size)) / 2),
                    ssh_action_y + ((settings_action_size.saturating_sub(ssh_icon_size)) / 2),
                    ssh_icon_size,
                    if ssh_action_hovered {
                        foreground
                    } else {
                        muted_fg
                    },
                )?;
            }
            if overview_action_hovered {
                self.fill_rounded_rectangle(
                    layers,
                    2,
                    euclid::rect(
                        overview_action_x as f32,
                        overview_action_y as f32,
                        settings_action_size as f32,
                        settings_action_size as f32,
                    ),
                    chrome.sidebar_button_hover_bg,
                    self.ui_f32(SIDEBAR_ROW_RADIUS + 4.0),
                )
                .context("sidebar live overview hover")?;
            }
            self.ui_items.push(UIItem {
                x: overview_action_x,
                y: overview_action_y,
                width: settings_action_size,
                height: settings_action_size,
                item_type: UIItemType::WorkspaceSidebarLiveOverview,
            });
            {
                let overview_icon_size = icon_size.min(settings_row_height.saturating_sub(8));
                self.paint_sidebar_icon(
                    layers,
                    SvgIcon::Grid2x2,
                    overview_action_x
                        + ((settings_action_size.saturating_sub(overview_icon_size)) / 2),
                    overview_action_y
                        + ((settings_action_size.saturating_sub(overview_icon_size)) / 2),
                    overview_icon_size,
                    if overview_action_hovered {
                        foreground
                    } else {
                        muted_fg
                    },
                )?;
            }
            let settings_icon_size = icon_size.min(settings_row_height.saturating_sub(8));
            let settings_icon_x = settings_row_x + self.ui_px(SIDEBAR_SETTINGS_ICON_EXTRA_INSET);
            let settings_icon_y =
                settings_row_y + ((settings_row_height.saturating_sub(settings_icon_size)) / 2);
            let settings_text_x =
                settings_icon_x + settings_icon_size + self.ui_px(SIDEBAR_ICON_GAP) + 8;
            let settings_text_y =
                settings_row_y + ((settings_row_height.saturating_sub(ui_cell_height)) / 2);
            self.paint_sidebar_icon(
                layers,
                SvgIcon::Settings,
                settings_icon_x,
                settings_icon_y,
                settings_icon_size,
                foreground,
            )?;
            self.paint_sidebar_text(
                layers,
                &ui_font,
                ui_metrics,
                &crate::i18n::tr("sidebar-settings"),
                settings_text_x,
                settings_text_y,
                settings_body_width
                    .saturating_sub(settings_text_x.saturating_sub(settings_row_x))
                    .saturating_sub(self.ui_px(SIDEBAR_INSET)),
                foreground,
            )?;
            let settings_action_icon_size = settings_icon_size;
            self.paint_sidebar_icon(
                layers,
                SvgIcon::SlidersVertical,
                settings_action_x
                    + ((settings_action_size.saturating_sub(settings_action_icon_size)) / 2),
                settings_action_y
                    + ((settings_action_size.saturating_sub(settings_action_icon_size)) / 2),
                settings_action_icon_size,
                if settings_options_hovered {
                    foreground
                } else {
                    muted_fg
                },
            )?;
        }

        if self.workspace_sidebar_scrollbar_visible() {
            self.update_next_frame_time(self.workspace_sidebar_scrollbar_visible_until);
        }

        if self.workspace_sidebar_scrollbar_visible() {
            let Some(scroll) = self.workspace_sidebar_scroll_geometry() else {
                return Ok(());
            };

            let track_radius = scroll.track_width as f32 / 2.0;
            self.fill_rounded_rectangle(
                layers,
                2,
                euclid::rect(
                    scroll.track_x as f32,
                    scroll.track_y as f32,
                    scroll.track_width as f32,
                    scroll.track_height as f32,
                ),
                chrome.separator,
                track_radius,
            )
            .context("sidebar scroll track")?;

            self.fill_rounded_rectangle(
                layers,
                2,
                euclid::rect(
                    scroll.track_x as f32,
                    scroll.thumb_y,
                    scroll.track_width as f32,
                    scroll.thumb_height,
                ),
                chrome.scrollbar_thumb,
                track_radius,
            )
            .context("sidebar scroll thumb")?;

            let hit_slop = 6usize;
            self.ui_items.push(UIItem {
                x: scroll.track_x.saturating_sub(hit_slop),
                y: scroll.track_y,
                width: scroll.track_width + hit_slop * 2,
                height: scroll.track_height,
                item_type: UIItemType::WorkspaceSidebarScrollTrack,
            });
            self.ui_items.push(UIItem {
                x: scroll.track_x.saturating_sub(hit_slop),
                y: scroll.thumb_y.round().max(0.0) as usize,
                width: scroll.track_width + hit_slop * 2,
                height: scroll.thumb_height.round().max(1.0) as usize,
                item_type: UIItemType::WorkspaceSidebarScrollThumb,
            });
        }

        self.filled_rectangle(
            layers,
            2,
            euclid::rect(
                rect.x.saturating_add(rect.width).saturating_sub(1) as f32,
                0.0,
                1.0,
                rect.y.saturating_add(rect.height) as f32,
            ),
            sidebar_separator,
        )
        .context("sidebar right separator")?;

        Ok(())
    }

    pub(crate) fn show_workspace_sidebar_scrollbar(&mut self) {
        self.workspace_sidebar_scrollbar_visible_until = Some(
            std::time::Instant::now()
                + std::time::Duration::from_millis(SIDEBAR_SCROLLBAR_VISIBLE_MS),
        );
    }

    fn workspace_sidebar_scrollbar_visible(&self) -> bool {
        let actively_dragging = self.dragging.as_ref().is_some_and(|(item, _)| {
            matches!(
                item.item_type,
                UIItemType::WorkspaceSidebarScrollTrack | UIItemType::WorkspaceSidebarScrollThumb
            )
        });
        actively_dragging
            || self
                .workspace_sidebar_scrollbar_visible_until
                .is_some_and(|until| until > std::time::Instant::now())
    }

    pub(crate) fn fill_rounded_rectangle(
        &self,
        layers: &mut TripleLayerQuadAllocator,
        layer_num: usize,
        rect: RectF,
        color: LinearRgba,
        radius: f32,
    ) -> anyhow::Result<()> {
        self.fill_vertically_rounded_rectangle(layers, layer_num, rect, color, radius, true, true)
    }

    /// Fill a rectangle whose top and bottom corner pairs can be rounded
    /// independently. Scrollable surfaces use this to preserve the real edge's
    /// corners while keeping a viewport-created cut edge square.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn fill_vertically_rounded_rectangle(
        &self,
        layers: &mut TripleLayerQuadAllocator,
        layer_num: usize,
        rect: RectF,
        color: LinearRgba,
        radius: f32,
        round_top: bool,
        round_bottom: bool,
    ) -> anyhow::Result<()> {
        let radius = snapped_rounded_corner_radius(rect, radius);
        // Sub-pixel radii round down to a 0px corner sprite (which panics when
        // building its pixmap), so fall back to a plain rectangle below ~1px.
        if !(radius >= 1.0) {
            self.filled_rectangle(layers, layer_num, rect, color)?;
            return Ok(());
        }

        let corner_size = euclid::size2(radius, radius);
        if round_top {
            self.poly_quad(
                layers,
                layer_num,
                euclid::point2(rect.min_x(), rect.min_y()),
                TOP_LEFT_ROUNDED_CORNER,
                0,
                corner_size,
                color,
            )?
            .set_grayscale();
            self.poly_quad(
                layers,
                layer_num,
                euclid::point2(rect.max_x() - radius, rect.min_y()),
                TOP_RIGHT_ROUNDED_CORNER,
                0,
                corner_size,
                color,
            )?
            .set_grayscale();
        } else {
            self.filled_rectangle(
                layers,
                layer_num,
                euclid::rect(rect.min_x(), rect.min_y(), radius, radius),
                color,
            )?;
            self.filled_rectangle(
                layers,
                layer_num,
                euclid::rect(rect.max_x() - radius, rect.min_y(), radius, radius),
                color,
            )?;
        }
        if round_bottom {
            self.poly_quad(
                layers,
                layer_num,
                euclid::point2(rect.min_x(), rect.max_y() - radius),
                BOTTOM_LEFT_ROUNDED_CORNER,
                0,
                corner_size,
                color,
            )?
            .set_grayscale();
            self.poly_quad(
                layers,
                layer_num,
                euclid::point2(rect.max_x() - radius, rect.max_y() - radius),
                BOTTOM_RIGHT_ROUNDED_CORNER,
                0,
                corner_size,
                color,
            )?
            .set_grayscale();
        } else {
            self.filled_rectangle(
                layers,
                layer_num,
                euclid::rect(rect.min_x(), rect.max_y() - radius, radius, radius),
                color,
            )?;
            self.filled_rectangle(
                layers,
                layer_num,
                euclid::rect(rect.max_x() - radius, rect.max_y() - radius, radius, radius),
                color,
            )?;
        }

        self.filled_rectangle(
            layers,
            layer_num,
            euclid::rect(
                rect.min_x() + radius,
                rect.min_y(),
                rect.width() - radius * 2.0,
                rect.height(),
            ),
            color,
        )?;
        self.filled_rectangle(
            layers,
            layer_num,
            euclid::rect(
                rect.min_x(),
                rect.min_y() + radius,
                radius,
                rect.height() - radius * 2.0,
            ),
            color,
        )?;
        self.filled_rectangle(
            layers,
            layer_num,
            euclid::rect(
                rect.max_x() - radius,
                rect.min_y() + radius,
                radius,
                rect.height() - radius * 2.0,
            ),
            color,
        )?;

        Ok(())
    }

    pub(crate) fn fill_rounded_rectangle_with_border(
        &self,
        layers: &mut TripleLayerQuadAllocator,
        layer_num: usize,
        rect: RectF,
        fill: LinearRgba,
        border: LinearRgba,
        radius: f32,
        border_width: f32,
    ) -> anyhow::Result<()> {
        let border_width = border_width
            .max(0.0)
            .min(rect.width() / 2.0)
            .min(rect.height() / 2.0);
        if border_width <= 0.0 {
            return self.fill_rounded_rectangle(layers, layer_num, rect, fill, radius);
        }

        self.fill_rounded_rectangle(layers, layer_num, rect, border, radius)?;
        self.fill_rounded_rectangle(
            layers,
            layer_num,
            euclid::rect(
                rect.min_x() + border_width,
                rect.min_y() + border_width,
                (rect.width() - border_width * 2.0).max(0.0),
                (rect.height() - border_width * 2.0).max(0.0),
            ),
            fill,
            (radius - border_width).max(0.0),
        )
    }

    pub(crate) fn paint_sidebar_text(
        &self,
        layers: &mut TripleLayerQuadAllocator,
        font: &Rc<LoadedFont>,
        metrics: RenderMetrics,
        text: &str,
        x: usize,
        y: usize,
        width: usize,
        foreground: LinearRgba,
    ) -> anyhow::Result<()> {
        if text.is_empty() || width == 0 {
            return Ok(());
        }

        let text = self.ellipsize_ui_text(font, text, width)?;
        if text.is_empty() {
            return Ok(());
        }

        self.paint_ui_title_text_cached(layers, font, &metrics, &text, x, y, width, foreground)?;
        Ok(())
    }

    pub(crate) fn ellipsize_ui_text<'a>(
        &self,
        font: &Rc<LoadedFont>,
        text: &'a str,
        width: usize,
    ) -> anyhow::Result<Cow<'a, str>> {
        let max_width = width as f32;
        if max_width <= 0.0 {
            return Ok(Cow::Borrowed(""));
        }

        // Shape the whole string ONCE via the shared shape cache, then measure
        // and cut from the cached glyph run. The previous implementation
        // re-measured a growing prefix grapheme-by-grapheme, each re-shaping the
        // string (`sidebar_text_width`), which was O(N^2) harfbuzz work per row
        // every frame and made the search list (long `display_path` names) crawl.
        let metrics = RenderMetrics::with_font_metrics(&font.metrics());
        let (shaped, _) = self.cached_ui_shape(font, &metrics, text)?;
        let total: f32 = shaped
            .iter()
            .map(|info| info.glyph.x_advance.get() as f32)
            .sum();
        if total <= max_width {
            return Ok(Cow::Borrowed(text));
        }

        const ELLIPSIS: &str = "...";
        let ellipsis_w = self.sidebar_text_width(font, ELLIPSIS)?;
        if ellipsis_w > max_width {
            // Not even the ellipsis fits; emit as many dots as do (matches the
            // previous degenerate fallback).
            let dot_w = self.sidebar_text_width(font, ".")?;
            let dots = if dot_w > 0.0 {
                ((max_width / dot_w).floor() as usize).min(ELLIPSIS.len())
            } else {
                0
            };
            return Ok(Cow::Owned(".".repeat(dots)));
        }

        // Keep the largest prefix (at a glyph-cluster boundary) that still
        // leaves room for the ellipsis.
        let budget = max_width - ellipsis_w;
        let glyphs: Vec<(f32, usize)> = shaped
            .iter()
            .map(|info| (info.glyph.x_advance.get() as f32, info.cluster))
            .collect();
        let cut_byte = ellipsize_cut_byte(&glyphs, text.len(), budget).min(text.len());

        let mut output = String::with_capacity(cut_byte + ELLIPSIS.len());
        output.push_str(&text[..cut_byte]);
        output.push_str(ELLIPSIS);
        Ok(Cow::Owned(output))
    }

    pub(crate) fn sidebar_text_width(
        &self,
        font: &Rc<LoadedFont>,
        text: &str,
    ) -> anyhow::Result<f32> {
        // Route through the shared shape cache so repeated width queries (the
        // ellipsize fit-check, button sizing, etc.) become cache hits and stay
        // consistent with the cached painters. Metrics are derived from the font
        // exactly as the sidebar paint path does
        // (`RenderMetrics::with_font_metrics(&ui_font.metrics())`).
        let metrics = RenderMetrics::with_font_metrics(&font.metrics());
        self.cached_ui_text_advance(font, &metrics, text)
    }

    pub(crate) fn paint_sidebar_icon(
        &self,
        layers: &mut TripleLayerQuadAllocator,
        icon: SvgIcon,
        x: usize,
        y: usize,
        size: usize,
        color: LinearRgba,
    ) -> anyhow::Result<()> {
        let left_offset = self.dimensions.pixel_width as f32 / 2.0;
        let top_offset = self.dimensions.pixel_height as f32 / 2.0;
        let gl_state = self.render_state.as_ref().unwrap();
        let sprite = gl_state
            .glyph_cache
            .borrow_mut()
            .cached_svg_icon(icon, size)?
            .texture_coords();

        let mut quad = layers.allocate(2)?;
        quad.set_position(
            x as f32 - left_offset,
            y as f32 - top_offset,
            x as f32 + size as f32 - left_offset,
            y as f32 + size as f32 - top_offset,
        );
        quad.set_texture(sprite);
        quad.set_fg_color(color);
        quad.set_alt_color_and_mix_value(color, 0.0);
        quad.set_hsv(None);
        quad.set_grayscale();

        Ok(())
    }

    /// Paint a full-color brand/OS logo (the brand color is baked into the
    /// sprite, so unlike [`Self::paint_sidebar_icon`] it is not tinted).
    fn paint_sidebar_brand_icon(
        &self,
        layers: &mut TripleLayerQuadAllocator,
        icon: BrandIcon,
        x: usize,
        y: usize,
        size: usize,
    ) -> anyhow::Result<()> {
        let left_offset = self.dimensions.pixel_width as f32 / 2.0;
        let top_offset = self.dimensions.pixel_height as f32 / 2.0;
        let gl_state = self.render_state.as_ref().unwrap();
        let sprite = gl_state
            .glyph_cache
            .borrow_mut()
            .cached_brand_icon(icon, size)?
            .texture_coords();

        let mut quad = layers.allocate(2)?;
        quad.set_position(
            x as f32 - left_offset,
            y as f32 - top_offset,
            x as f32 + size as f32 - left_offset,
            y as f32 + size as f32 - top_offset,
        );
        quad.set_texture(sprite);
        quad.set_hsv(None);
        quad.set_has_color(true);
        quad.set_fg_color(LinearRgba::with_components(1.0, 1.0, 1.0, 1.0));

        Ok(())
    }

    pub(crate) fn paint_sidebar_material_icon(
        &self,
        layers: &mut TripleLayerQuadAllocator,
        icon: MaterialIcon,
        x: usize,
        y: usize,
        size: usize,
    ) -> anyhow::Result<()> {
        let left_offset = self.dimensions.pixel_width as f32 / 2.0;
        let top_offset = self.dimensions.pixel_height as f32 / 2.0;
        let gl_state = self.render_state.as_ref().unwrap();
        let sprite = gl_state
            .glyph_cache
            .borrow_mut()
            .cached_material_icon(icon, size)?
            .texture_coords();

        let mut quad = layers.allocate(2)?;
        quad.set_position(
            x as f32 - left_offset,
            y as f32 - top_offset,
            x as f32 + size as f32 - left_offset,
            y as f32 + size as f32 - top_offset,
        );
        quad.set_texture(sprite);
        quad.set_hsv(None);
        quad.set_has_color(true);
        quad.set_fg_color(LinearRgba::with_components(1.0, 1.0, 1.0, 1.0));

        Ok(())
    }
}

/// Largest byte prefix (at a glyph-cluster boundary) whose cumulative advance
/// fits in `budget`. `glyphs` are `(advance_px, cluster)` pairs in visual order,
/// where `cluster` is the source byte offset of each glyph. Cutting at the
/// *next* glyph's cluster guarantees we never split a multi-byte character or a
/// shaped cluster. Used by `ellipsize_ui_text` to truncate in a single
/// shaping pass instead of re-shaping a growing prefix per grapheme.
fn ellipsize_cut_byte(glyphs: &[(f32, usize)], text_len: usize, budget: f32) -> usize {
    let mut acc = 0.0f32;
    let mut cut = 0usize;
    for idx in 0..glyphs.len() {
        let advance = glyphs[idx].0;
        if acc + advance > budget {
            break;
        }
        acc += advance;
        cut = glyphs.get(idx + 1).map(|next| next.1).unwrap_or(text_len);
    }
    cut
}

/// Corner sprites are cached at integral physical-pixel sizes. Keep the quad
/// geometry on that same grid so fractional radii cannot expose the joins
/// between the four corners and the center rectangles on 1x displays.
fn snapped_rounded_corner_radius(rect: RectF, radius: f32) -> f32 {
    radius
        .min(rect.width() / 2.0)
        .min(rect.height() / 2.0)
        .floor()
}

fn notification_snapshot_has_new_entry(
    previous: Option<&HashMap<String, workspace_threads::WorkspaceThreadWorkStatus>>,
    current: &HashMap<String, workspace_threads::WorkspaceThreadWorkStatus>,
) -> bool {
    let Some(previous) = previous else {
        return false;
    };
    current
        .iter()
        .any(|(thread_id, status)| previous.get(thread_id) != Some(status))
}

fn notification_badge_pulse_phase(elapsed: Duration) -> Option<f32> {
    if elapsed >= NOTIFICATION_BADGE_PULSE_DURATION {
        return None;
    }
    let total_progress = elapsed.as_secs_f32() / NOTIFICATION_BADGE_PULSE_DURATION.as_secs_f32();
    Some((total_progress * NOTIFICATION_BADGE_PULSE_COUNT).fract())
}

/// The `shown` state a sidebar toggle should move to.
///
/// Split out from [`TermWindow::toggle_workspace_sidebar`] because its two
/// inputs disagree during a hover reveal — the panel is on screen while
/// `collapsed` is still true — and that disagreement is exactly where this went
/// wrong: reading `collapsed` alone turned a click on a button labelled "close"
/// into a dock.
fn workspace_sidebar_toggle_target(collapsed: bool, hover_presented: bool) -> bool {
    let presented = !collapsed || hover_presented;
    !presented
}

#[cfg(test)]
mod tests {
    use super::{
        ellipsize_cut_byte, notification_badge_pulse_phase, notification_snapshot_has_new_entry,
        snapped_rounded_corner_radius, workspace_sidebar_toggle_target,
        NOTIFICATION_BADGE_PULSE_DURATION,
    };
    use crate::workspace_threads::WorkspaceThreadWorkStatus;
    use std::collections::HashMap;
    use std::time::Duration;

    /// Clicking the tab bar's toggle acts on what the user sees. During a hover
    /// reveal the panel is presented while `collapsed` is still true, and the
    /// button reads "close" — so it must collapse, not dock. Docking there also
    /// persisted `show_left_sidebar_by_default`, permanently re-docking the
    /// panel for every future window.
    #[test]
    fn toggling_a_hover_revealed_sidebar_collapses_it() {
        // (collapsed, hover_presented) -> shown
        assert!(
            !workspace_sidebar_toggle_target(false, false),
            "docked panel should collapse"
        );
        assert!(
            workspace_sidebar_toggle_target(true, false),
            "collapsed panel should dock"
        );
        assert!(
            !workspace_sidebar_toggle_target(true, true),
            "hover-revealed panel should collapse, not dock"
        );
        assert!(
            !workspace_sidebar_toggle_target(false, true),
            "docked panel under a reveal should still collapse"
        );
    }

    // Build (advance, cluster) pairs for an ASCII or per-char string where every
    // char is one glyph of `advance` px and the cluster is its byte offset.
    fn glyphs_per_char(text: &str, advance: f32) -> Vec<(f32, usize)> {
        text.char_indices()
            .map(|(byte, _)| (advance, byte))
            .collect()
    }

    #[test]
    fn cut_keeps_whole_prefix_when_everything_fits() {
        let text = "abcdef";
        let glyphs = glyphs_per_char(text, 10.0);
        // budget large enough for all 6 glyphs.
        assert_eq!(ellipsize_cut_byte(&glyphs, text.len(), 1000.0), text.len());
    }

    #[test]
    fn cut_stops_at_budget() {
        let text = "abcdef";
        let glyphs = glyphs_per_char(text, 10.0);
        // budget for ~3.5 glyphs -> keep 3 (bytes 0..3).
        assert_eq!(ellipsize_cut_byte(&glyphs, text.len(), 35.0), 3);
    }

    #[test]
    fn cut_never_splits_multibyte_chars() {
        // "ab你好cd": bytes a=0 b=1 你=2..5 好=5..8 c=8 d=9 (你/好 are 3 bytes each).
        let text = "ab你好cd";
        let glyphs = glyphs_per_char(text, 10.0);
        // Budget for 3 glyphs (a, b, 你) -> cut at byte 5 (start of 好), the
        // boundary AFTER the full multi-byte char, never inside it.
        let cut = ellipsize_cut_byte(&glyphs, text.len(), 35.0);
        assert_eq!(cut, 5);
        assert!(text.is_char_boundary(cut));
        assert_eq!(&text[..cut], "ab你");
    }

    #[test]
    fn cut_is_zero_when_nothing_fits() {
        let text = "abc";
        let glyphs = glyphs_per_char(text, 10.0);
        assert_eq!(ellipsize_cut_byte(&glyphs, text.len(), 5.0), 0);
    }

    #[test]
    fn rounded_corner_radius_matches_integral_corner_sprite_size() {
        let odd_height = euclid::rect(0.0, 0.0, 215.0, 33.0);
        let even_height = euclid::rect(0.0, 0.0, 215.0, 34.0);

        assert_eq!(snapped_rounded_corner_radius(odd_height, 999.0), 16.0);
        assert_eq!(snapped_rounded_corner_radius(even_height, 999.0), 17.0);
        assert_eq!(snapped_rounded_corner_radius(even_height, 10.75), 10.0);
    }

    #[test]
    fn notification_snapshot_ignores_initial_state_and_detects_replacements() {
        let current = HashMap::from([(
            "thread-a".to_string(),
            WorkspaceThreadWorkStatus::FinishedUnseen,
        )]);
        assert!(!notification_snapshot_has_new_entry(None, &current));
        assert!(!notification_snapshot_has_new_entry(
            Some(&current),
            &current
        ));

        // Total count is still one, but a different thread completed.
        let replacement = HashMap::from([(
            "thread-b".to_string(),
            WorkspaceThreadWorkStatus::FinishedUnseen,
        )]);
        assert!(notification_snapshot_has_new_entry(
            Some(&current),
            &replacement
        ));
    }

    #[test]
    fn notification_snapshot_detects_status_transition() {
        let attention = HashMap::from([(
            "thread-a".to_string(),
            WorkspaceThreadWorkStatus::NeedsAttention,
        )]);
        let finished = HashMap::from([(
            "thread-a".to_string(),
            WorkspaceThreadWorkStatus::FinishedUnseen,
        )]);
        assert!(notification_snapshot_has_new_entry(
            Some(&attention),
            &finished
        ));
    }

    #[test]
    fn notification_badge_pulses_three_times_then_stops() {
        assert_eq!(notification_badge_pulse_phase(Duration::ZERO), Some(0.0));
        assert_eq!(
            notification_badge_pulse_phase(NOTIFICATION_BADGE_PULSE_DURATION),
            None
        );

        // Halfway through each 600ms cycle, the expanding halo is halfway out.
        for elapsed_ms in [300, 900, 1500] {
            let phase = notification_badge_pulse_phase(Duration::from_millis(elapsed_ms)).unwrap();
            assert!((phase - 0.5).abs() < 0.001);
        }
    }
}
