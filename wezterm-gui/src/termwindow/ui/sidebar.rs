use crate::customglyph::BlockKey;
use crate::quad::{QuadTrait, TripleLayerQuadAllocator, TripleLayerQuadAllocatorTrait};
use crate::termwindow::render::corners::{
    BOTTOM_LEFT_ROUNDED_CORNER, BOTTOM_RIGHT_ROUNDED_CORNER, TOP_LEFT_ROUNDED_CORNER,
    TOP_RIGHT_ROUNDED_CORNER,
};
use crate::termwindow::ui::icons::{distro_to_icon, BrandIcon, SvgIcon};
use crate::termwindow::ui::status_icon::UiStatusKind;
use crate::termwindow::ui::tokens::{
    CAPSULE_BORDER_WIDTH, MACOS_TITLEBAR_CONTENT_TOP_INSET, SIDEBAR_ICON_GAP, SIDEBAR_INSET,
    SIDEBAR_MAX_WIDTH, SIDEBAR_MIN_WIDTH, SIDEBAR_RESIZE_HANDLE_WIDTH, SIDEBAR_ROW_GAP,
    SIDEBAR_ROW_RADIUS, SIDEBAR_WIDTH_CELLS, WINDOW_TAB_FULLSCREEN_NEW_SESSION_EXTRA_HEIGHT,
    WINDOW_TAB_FULLSCREEN_NEW_SESSION_Y_OFFSET, WINDOW_TAB_FULLSCREEN_SIDEBAR_BUTTON_SIZE,
    WINDOW_TAB_FULLSCREEN_SIDEBAR_BUTTON_X, WINDOW_TAB_FULLSCREEN_SIDEBAR_BUTTON_Y_OFFSET,
    WINDOW_TAB_FULLSCREEN_SIDEBAR_ICON_SIZE,
};
use crate::termwindow::{UIItem, UIItemType};
use crate::ui::UiPalette;
use crate::utilsprites::RenderMetrics;
use crate::workspace_threads;
use anyhow::Context;
use finl_unicode::grapheme_clusters::Graphemes;
use mux::Mux;
use std::borrow::Cow;
use std::rc::Rc;
use wezterm_bidi::Direction;
use wezterm_font::LoadedFont;
use window::color::LinearRgba;
use window::{MouseEventKind as WMEK, RectF, WindowOps, WindowState};

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
const SIDEBAR_SECTION_ACTION_SIZE: usize = 48;
const SIDEBAR_SECTION_ACTION_ICON_INSET: usize = 6;

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
    top_action_y_offset: usize,
    top_action_height: usize,
    list_top: usize,
}

pub fn workspace_sidebar_width_for_metrics(render_metrics: &RenderMetrics) -> usize {
    let default_width =
        (render_metrics.cell_size.width as usize * SIDEBAR_WIDTH_CELLS).max(SIDEBAR_MIN_WIDTH);
    crate::native_settings::workspace_sidebar_width()
        .unwrap_or(default_width)
        .clamp(SIDEBAR_MIN_WIDTH, SIDEBAR_MAX_WIDTH)
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

    pub fn workspace_sidebar_width(&self) -> usize {
        if self.workspace_sidebar_collapsed {
            0
        } else {
            self.workspace_sidebar_width
                .clamp(SIDEBAR_MIN_WIDTH, self.workspace_sidebar_max_width())
        }
    }

    pub fn workspace_sidebar_max_width(&self) -> usize {
        SIDEBAR_MAX_WIDTH.min((self.dimensions.pixel_width / 2).max(SIDEBAR_MIN_WIDTH))
    }

    pub fn set_workspace_sidebar_width(&mut self, width: usize) {
        self.workspace_sidebar_width =
            width.clamp(SIDEBAR_MIN_WIDTH, self.workspace_sidebar_max_width());
    }

    pub fn persist_workspace_sidebar_width(&self) {
        let width = self
            .workspace_sidebar_width
            .clamp(SIDEBAR_MIN_WIDTH, SIDEBAR_MAX_WIDTH);
        if let Err(err) = crate::native_settings::save_workspace_sidebar_width(width) {
            log::warn!("failed to save workspace sidebar width: {err:#}");
        }
    }

    pub fn toggle_workspace_sidebar(&mut self) {
        self.workspace_sidebar_collapsed = !self.workspace_sidebar_collapsed;
    }

    pub fn expand_workspace_sidebar(&mut self) {
        self.workspace_sidebar_collapsed = false;
    }

    pub(crate) fn workspace_sidebar_toggle_icon(&self) -> SvgIcon {
        if self.workspace_sidebar_collapsed {
            SvgIcon::PanelLeftOpen
        } else {
            SvgIcon::PanelLeftClose
        }
    }

    pub fn tab_bar_left_edge(&self) -> usize {
        let border = self.get_os_border();
        border.left.get() as usize + self.workspace_sidebar_width()
    }

    pub fn workspace_sidebar_rect(&self) -> Option<WorkspaceSidebarRect> {
        let border = self.get_os_border();
        let bottom_tab_bar_height = if self.config.tab_bar_at_bottom && self.show_tab_bar {
            self.tab_bar_pixel_height().unwrap_or(0.0).ceil() as usize
        } else {
            0
        };

        let x = border.left.get() as usize;
        let y = border.top.get() as usize;
        let width = self.workspace_sidebar_width().min(
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
            foreground.mul_alpha(0.45)
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
        let icon_y = y + ((row_height.saturating_sub(SESSION_STATUS_ICON_SIZE)) / 2);
        if let Some(status) = self.sidebar_thread_status_kind(session) {
            let status_size = if matches!(status, UiStatusKind::Running | UiStatusKind::Done) {
                SESSION_STATUS_ACTIVE_ICON_SIZE
            } else {
                SESSION_STATUS_ICON_SIZE
            };
            return self.paint_status_icon(
                layers,
                2,
                status,
                centered_inner_start(x, SESSION_STATUS_ICON_SIZE, status_size),
                centered_inner_start(y, row_height, status_size),
                status_size,
                self.sidebar_thread_status_color(session, status, chrome, foreground),
            );
        }

        let dot_offset = (SESSION_STATUS_ICON_SIZE.saturating_sub(SESSION_STATUS_DOT_SIZE)) / 2;
        let dot_x = x + dot_offset;
        let dot_y = icon_y + dot_offset;
        self.fill_rounded_rectangle(
            layers,
            1,
            euclid::rect(
                dot_x as f32,
                dot_y as f32,
                SESSION_STATUS_DOT_SIZE as f32,
                SESSION_STATUS_DOT_SIZE as f32,
            ),
            self.sidebar_thread_dot_color(session, chrome, foreground),
            SESSION_STATUS_DOT_SIZE as f32 / 2.0,
        )
        .context("sidebar thread status dot")
    }

    fn workspace_sidebar_content_top(&self, panel_y: usize) -> usize {
        let base_top = panel_y + SIDEBAR_INSET;
        if cfg!(target_os = "macos") && !self.window_state.contains(WindowState::FULL_SCREEN) {
            let tab_row_height = self.tab_bar_pixel_height().unwrap_or(0.0).ceil() as usize;
            panel_y + MACOS_TITLEBAR_CONTENT_TOP_INSET.max(tab_row_height)
        } else {
            base_top
        }
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
        let settings_footer_height = SIDEBAR_SETTINGS_FOOTER_HEIGHT.min(panel_height);
        let settings_footer_y = panel_y
            .saturating_add(panel_height)
            .saturating_sub(settings_footer_height);
        let content_bottom = (panel_y + panel_height.saturating_sub(SIDEBAR_INSET))
            .min(settings_footer_y.max(panel_y));
        let show_sidebar_toolbar = self.window_state.contains(WindowState::FULL_SCREEN);
        let row_height =
            (ui_cell_height.max(icon_size) + SIDEBAR_INSET).max(SESSION_ROW_MIN_HEIGHT);
        let mut y = self.workspace_sidebar_content_top(panel_y);
        if show_sidebar_toolbar {
            y += WINDOW_TAB_FULLSCREEN_SIDEBAR_BUTTON_SIZE + SIDEBAR_INSET;
        }
        let top_action_height = row_height.min(48).max(ui_cell_height + SIDEBAR_INSET)
            + if show_sidebar_toolbar {
                WINDOW_TAB_FULLSCREEN_NEW_SESSION_EXTRA_HEIGHT
            } else {
                0
            };
        let top_action_y_offset = if show_sidebar_toolbar {
            WINDOW_TAB_FULLSCREEN_NEW_SESSION_Y_OFFSET
        } else {
            0
        };
        let space_menu_y = y;
        let space_menu_height = top_action_height + 6;
        y += space_menu_height + SIDEBAR_INSET;
        let list_top = y + top_action_y_offset + top_action_height + SIDEBAR_INSET;

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
            top_action_y_offset,
            top_action_height,
            list_top,
        }
    }

    pub fn workspace_sidebar_scroll_max(&self) -> f32 {
        let Some(rect) = self.workspace_sidebar_rect() else {
            return 0.0;
        };
        if self.workspace_sidebar_collapsed {
            return 0.0;
        }

        let ui_cell_height = self
            .fonts
            .title_font()
            .map(|font| {
                RenderMetrics::with_font_metrics(&font.metrics())
                    .cell_size
                    .height as usize
            })
            .unwrap_or(self.render_metrics.cell_size.height as usize);
        let icon_size = (ui_cell_height + 12).clamp(30, 36);
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
        let view = workspace_threads::view_for_current_project(
            &self.active_space_id,
            &active_workspace,
            &workspaces,
        );
        let row_gap = SIDEBAR_ROW_GAP;
        let total_height = Self::workspace_sidebar_scroll_height(
            &view,
            layout.row_height,
            row_gap,
            viewport_height,
        );
        total_height.saturating_sub(viewport_height) as f32
    }

    pub fn workspace_sidebar_scroll_geometry(&self) -> Option<WorkspaceSidebarScrollGeometry> {
        let rect = self.workspace_sidebar_rect()?;
        if self.workspace_sidebar_collapsed {
            return None;
        }

        let ui_cell_height = self
            .fonts
            .title_font()
            .map(|font| {
                RenderMetrics::with_font_metrics(&font.metrics())
                    .cell_size
                    .height as usize
            })
            .unwrap_or(self.render_metrics.cell_size.height as usize);
        let icon_size = (ui_cell_height + 12).clamp(30, 36);
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
        let view = workspace_threads::view_for_current_project(
            &self.active_space_id,
            &active_workspace,
            &workspaces,
        );
        let row_gap = SIDEBAR_ROW_GAP;
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
            .saturating_sub(SIDEBAR_INSET / 2 + track_width);

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

    pub fn paint_workspace_sidebar(
        &mut self,
        layers: &mut TripleLayerQuadAllocator,
    ) -> anyhow::Result<()> {
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
        let icon_size = (ui_cell_height + 12).clamp(30, 36);
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
        let item_x = panel_x + SIDEBAR_INSET;
        let item_width = panel_width.saturating_sub(SIDEBAR_INSET * 2 + 1);
        self.ui_items.push(UIItem {
            x: rect
                .x
                .saturating_add(rect.width)
                .saturating_sub(SIDEBAR_RESIZE_HANDLE_WIDTH / 2),
            y: rect.y,
            width: SIDEBAR_RESIZE_HANDLE_WIDTH,
            height: rect.height,
            item_type: UIItemType::WorkspaceSidebarResize,
        });

        if self.workspace_sidebar_collapsed {
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
        let view = workspace_threads::view_for_current_project(
            &self.active_space_id,
            &active_workspace,
            &workspaces,
        );

        let header_icon_size = icon_size.min(32);
        let button_size = (header_icon_size + 8).clamp(32, 40);
        let header_y = self.workspace_sidebar_content_top(panel_y);
        let show_sidebar_toolbar = layout.show_sidebar_toolbar;
        let sidebar_toggle_size = WINDOW_TAB_FULLSCREEN_SIDEBAR_BUTTON_SIZE;
        let sidebar_toggle_x = panel_x + WINDOW_TAB_FULLSCREEN_SIDEBAR_BUTTON_X;
        let sidebar_toggle_y = header_y + WINDOW_TAB_FULLSCREEN_SIDEBAR_BUTTON_Y_OFFSET;
        let sidebar_toggle_icon_size =
            WINDOW_TAB_FULLSCREEN_SIDEBAR_ICON_SIZE.min(sidebar_toggle_size.saturating_sub(2));
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
                    SIDEBAR_ROW_RADIUS,
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
        let section_button_size = SIDEBAR_SECTION_ACTION_SIZE
            .max(button_size)
            .min(session_row_height.saturating_sub(8));
        let active_project_id = view
            .projects
            .iter()
            .find(|project| project.is_active)
            .map(|project| project.id.clone());
        let top_action_y_offset = layout.top_action_y_offset;
        let top_action_total_width = item_width.saturating_sub(SIDEBAR_INSET * 2);
        let top_action_gap = SIDEBAR_ICON_GAP + 4;
        let top_action_height = layout.top_action_height;
        let space_menu_x = item_x + SIDEBAR_INSET;
        let space_menu_y = layout.space_menu_y;
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
                SIDEBAR_ROW_RADIUS + 4.0,
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
        let space_icon_x = space_menu_x + SIDEBAR_INSET;
        let space_icon_y = space_menu_y + ((space_menu_height.saturating_sub(space_icon_size)) / 2);
        let space_action_icon_size = space_icon_size.min(22);
        let space_action_icon_x = space_menu_x
            .saturating_add(space_menu_width)
            .saturating_sub(SIDEBAR_INSET + space_action_icon_size);
        let space_action_icon_y =
            space_menu_y + ((space_menu_height.saturating_sub(space_action_icon_size)) / 2);
        let space_text_right = space_menu_x
            .saturating_add(space_menu_width)
            .saturating_sub(SIDEBAR_INSET + space_action_icon_size + top_action_gap);
        let space_text_x = space_icon_x + space_icon_size + top_action_gap;
        let space_name = crate::workspace_threads::active_space_name(&self.active_space_id)
            .unwrap_or_else(|| "Default".to_string());
        let space_title = self.sidebar_space_title(&self.active_space_id, &space_name);
        let space_label = self.ellipsize_sidebar_text(
            &ui_font,
            &space_title,
            space_text_right.saturating_sub(space_text_x),
        )?;
        self.paint_sidebar_icon(
            layers,
            SvgIcon::Layers,
            space_icon_x,
            space_icon_y,
            space_icon_size,
            if space_menu_hovered {
                foreground
            } else {
                muted_fg
            },
        )?;
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
        let y = space_menu_y + space_menu_height + SIDEBAR_INSET;
        let top_action_x = item_x + SIDEBAR_INSET;
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
            SIDEBAR_ROW_RADIUS + 4.0,
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
            SIDEBAR_ROW_RADIUS + 4.0,
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
            .saturating_sub(top_action_icon_size + top_action_gap + SIDEBAR_INSET * 2);
        let top_action_label =
            self.ellipsize_sidebar_text(&ui_font, "New Thread", top_action_text_max_width)?;
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
        let list_top = layout.list_top;
        let row_gap = SIDEBAR_ROW_GAP;
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
        self.workspace_sidebar_scroll_offset =
            self.workspace_sidebar_scroll_offset.clamp(0.0, max_scroll);
        let scroll_offset = self.workspace_sidebar_scroll_offset;
        let list_top_f = list_top as f32;
        let content_bottom_f = content_bottom as f32;
        let suppress_hover = self
            .current_mouse_event
            .as_ref()
            .is_some_and(|event| matches!(event.kind, WMEK::VertWheel(_) | WMEK::HorzWheel(_)));
        let mut virtual_y = 0usize;

        if !view.pinned_threads.is_empty() {
            let label_top = list_top_f + virtual_y as f32 - scroll_offset;
            let label_bottom = label_top + session_row_height as f32;
            let label_is_visible = label_bottom > list_top_f && label_top < content_bottom_f;
            if label_is_visible {
                let label_y = label_top.floor().max(0.0) as usize;
                let label_icon_size = header_icon_size.min(ui_cell_height).max(16);
                let label_icon_x = item_x + SIDEBAR_INSET;
                let label_icon_y =
                    label_y + ((session_row_height.saturating_sub(label_icon_size)) / 2);
                let label_text_x = label_icon_x + label_icon_size + SIDEBAR_ICON_GAP;
                self.paint_sidebar_icon(
                    layers,
                    SvgIcon::Pin,
                    label_icon_x,
                    label_icon_y,
                    label_icon_size,
                    muted_fg,
                )?;
                self.paint_sidebar_text(
                    layers,
                    &ui_font,
                    ui_metrics,
                    "Pinned",
                    label_text_x,
                    label_y + ((session_row_height.saturating_sub(ui_cell_height)) / 2),
                    item_x
                        .saturating_add(item_width)
                        .saturating_sub(label_text_x + SIDEBAR_INSET),
                    muted_fg,
                )?;
            }
            virtual_y += session_row_height + row_gap;

            let pinned_x = item_x + SIDEBAR_INSET;
            let pinned_width = item_width.saturating_sub(SIDEBAR_INSET * 2);
            let pinned_status_x = pinned_x + SIDEBAR_INSET;
            let pinned_text_x = pinned_status_x + SESSION_STATUS_ICON_SIZE + SIDEBAR_ICON_GAP + 6;
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
                    if session.is_active {
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
                            SIDEBAR_ROW_RADIUS + 2.0,
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
                            SIDEBAR_ROW_RADIUS + 2.0,
                        )
                        .context("sidebar hovered pinned thread")?;
                    }

                    let text_y = y + ((session_row_height.saturating_sub(ui_cell_height)) / 2);
                    let action_size = session_row_height
                        .saturating_sub(8)
                        .clamp(SESSION_ACTION_MIN_SIZE, SESSION_ACTION_MAX_SIZE);
                    let delete_x = pinned_x
                        .saturating_add(pinned_width)
                        .saturating_sub(SIDEBAR_INSET + action_size);
                    let pin_x = delete_x.saturating_sub(action_size + 4);
                    let action_y = y + ((session_row_height.saturating_sub(action_size)) / 2);
                    let text_right = if is_hovered && !is_renaming_session {
                        pin_x
                    } else {
                        pinned_x
                            .saturating_add(pinned_width)
                            .saturating_sub(SIDEBAR_INSET)
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
                        text_right.saturating_sub(pinned_text_x + SIDEBAR_INSET),
                        if session.is_active {
                            active_fg
                        } else {
                            foreground
                        },
                    )?;

                    if is_hovered && !is_renaming_session {
                        for (x, item_type, icon, _context_name) in [
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
                        ] {
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
                                .saturating_sub(SESSION_ACTION_ICON_INSET)
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
                virtual_y += WORKSPACE_SECTION_LABEL_GAP;
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
                    .saturating_sub(section_button_size + SIDEBAR_INSET);
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
                    "Workspaces",
                    item_x + SIDEBAR_INSET,
                    label_y + ((session_row_height.saturating_sub(ui_cell_height)) / 2),
                    button_x.saturating_sub(item_x + SIDEBAR_INSET * 2),
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
                    SIDEBAR_ROW_RADIUS,
                )
                .context("sidebar new project button")?;
                self.ui_items.push(UIItem {
                    x: button_x,
                    y: button_y,
                    width: section_button_size,
                    height: section_button_size,
                    item_type: UIItemType::ProjectNew,
                });
                let section_icon_size = (header_icon_size + 4)
                    .min(section_button_size.saturating_sub(SIDEBAR_SECTION_ACTION_ICON_INSET));
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
            let disclosure_x = item_x + SIDEBAR_INSET;
            let project_icon_x = disclosure_x + disclosure_size + 4;
            let project_text_x = project_icon_x + icon_size + SIDEBAR_ICON_GAP;
            let project_action_size = section_button_size.min(session_row_height.saturating_sub(8));
            let project_action_x = item_x
                .saturating_add(item_width)
                .saturating_sub(project_action_size + SIDEBAR_INSET);
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
                    project_text_right.saturating_sub(project_text_x + SIDEBAR_INSET),
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
                    SIDEBAR_ROW_RADIUS,
                )
                .context("sidebar new thread button")?;
                self.ui_items.push(UIItem {
                    x: project_action_x,
                    y: project_action_y,
                    width: project_action_size,
                    height: project_action_size,
                    item_type: UIItemType::WorkspaceThreadNew(project.id.clone()),
                });
                let action_icon_size = (header_icon_size + 4)
                    .min(project_action_size.saturating_sub(SIDEBAR_SECTION_ACTION_ICON_INSET));
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
                let session_x = item_x + SIDEBAR_INSET * 3 + SESSION_ROW_SIDE_PADDING;
                let session_width =
                    item_width.saturating_sub(SIDEBAR_INSET * 3 + SESSION_ROW_SIDE_PADDING * 2);
                let session_status_x = session_x + SIDEBAR_INSET + 2;
                let session_text_x =
                    session_status_x + SESSION_STATUS_ICON_SIZE + SIDEBAR_ICON_GAP + 6;

                for session in &project.threads {
                    let row_top = list_top_f + virtual_y as f32 - scroll_offset;
                    let row_bottom = row_top + session_row_height as f32;
                    let row_is_visible = row_bottom > list_top_f && row_top < content_bottom_f;
                    let y = row_top.floor().max(0.0) as usize;
                    let hit_y = row_top.max(list_top_f).floor().max(0.0) as usize;
                    let hit_bottom =
                        row_bottom.min(content_bottom_f).ceil().max(hit_y as f32) as usize;
                    let hit_height = hit_bottom.saturating_sub(hit_y).max(1);

                    if row_is_visible && session.is_active {
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
                            SIDEBAR_ROW_RADIUS + 2.0,
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
                        let action_size = session_row_height
                            .saturating_sub(8)
                            .clamp(SESSION_ACTION_MIN_SIZE, SESSION_ACTION_MAX_SIZE);
                        let delete_x = session_x
                            .saturating_add(session_width)
                            .saturating_sub(SIDEBAR_INSET + action_size);
                        let pin_x = delete_x.saturating_sub(action_size + 4);
                        let action_y = y + ((session_row_height.saturating_sub(action_size)) / 2);
                        let text_right = if is_hovered && !is_renaming_session {
                            pin_x
                        } else {
                            session_x
                                .saturating_add(session_width)
                                .saturating_sub(SIDEBAR_INSET)
                        };
                        let text_width = text_right.saturating_sub(session_text_x + SIDEBAR_INSET);
                        if is_hovered && !session.is_active && !is_renaming_session {
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
                                SIDEBAR_ROW_RADIUS + 2.0,
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
                            if session.is_active {
                                active_fg
                            } else {
                                foreground
                            },
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
                                    .saturating_sub(SESSION_ACTION_ICON_INSET)
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
                virtual_y += WORKSPACE_GROUP_EXTRA_GAP;
            }
        }

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
                        SIDEBAR_ROW_RADIUS,
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
                    SIDEBAR_ROW_RADIUS + 4.0,
                )
                .context("sidebar space menu button repaint")?;
            }
            self.paint_sidebar_icon(
                layers,
                SvgIcon::Layers,
                space_icon_x,
                space_icon_y,
                space_icon_size,
                if space_menu_hovered {
                    foreground
                } else {
                    muted_fg
                },
            )?;
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
                SIDEBAR_ROW_RADIUS + 4.0,
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
                SIDEBAR_ROW_RADIUS + 4.0,
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
        }

        if max_scroll > 0.0 && scroll_offset > 0.0 {
            let fade_height = SIDEBAR_TOP_FADE_HEIGHT.min(content_bottom.saturating_sub(list_top));
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
            let fade_height = SIDEBAR_SETTINGS_FADE_HEIGHT
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

            let settings_row_x = item_x + SIDEBAR_SETTINGS_ROW_SIDE_PADDING;
            let settings_row_y = (settings_footer_y + SIDEBAR_SETTINGS_ROW_TOP_PADDING)
                .saturating_sub(SIDEBAR_SETTINGS_ROW_LIFT);
            let settings_row_width =
                item_width.saturating_sub(SIDEBAR_SETTINGS_ROW_SIDE_PADDING * 2);
            let settings_row_height = settings_footer_height
                .saturating_sub(
                    SIDEBAR_SETTINGS_ROW_TOP_PADDING + SIDEBAR_SETTINGS_ROW_BOTTOM_PADDING,
                )
                .max(1);
            let settings_action_size = settings_row_height.max(1);
            let settings_action_x = settings_row_x
                .saturating_add(settings_row_width)
                .saturating_sub(settings_action_size);
            let settings_action_y = settings_row_y;
            // SSH hosts (link-2) button sits just left of the view-options button.
            let ssh_action_x =
                settings_action_x.saturating_sub(settings_action_size + SIDEBAR_INSET / 2);
            let ssh_action_y = settings_row_y;
            let ssh_action_hovered = self.is_pointer_over_ui_rect(
                ssh_action_x,
                ssh_action_y,
                settings_action_size,
                settings_action_size,
            );
            let settings_body_width = ssh_action_x
                .saturating_sub(settings_row_x)
                .saturating_sub(SIDEBAR_INSET / 2);
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
                    SIDEBAR_ROW_RADIUS + 4.0,
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
                    SIDEBAR_ROW_RADIUS + 4.0,
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
                    SIDEBAR_ROW_RADIUS + 4.0,
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
            let settings_icon_size = icon_size.min(settings_row_height.saturating_sub(8));
            let settings_icon_x = settings_row_x + SIDEBAR_SETTINGS_ICON_EXTRA_INSET;
            let settings_icon_y =
                settings_row_y + ((settings_row_height.saturating_sub(settings_icon_size)) / 2);
            let settings_text_x = settings_icon_x + settings_icon_size + SIDEBAR_ICON_GAP + 8;
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
                "Settings",
                settings_text_x,
                settings_text_y,
                settings_action_x.saturating_sub(settings_text_x + SIDEBAR_INSET),
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
        let radius = radius.min(rect.width() / 2.0).min(rect.height() / 2.0);
        // Sub-pixel radii round down to a 0px corner sprite (which panics when
        // building its pixmap), so fall back to a plain rectangle below ~1px.
        if !(radius >= 1.0) {
            self.filled_rectangle(layers, layer_num, rect, color)?;
            return Ok(());
        }

        let corner_size = euclid::size2(radius, radius);
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

    fn paint_sidebar_text(
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

        let text = self.ellipsize_sidebar_text(font, text, width)?;
        if text.is_empty() {
            return Ok(());
        }

        self.paint_ui_title_text(layers, font, &metrics, &text, x, y, width, foreground)
    }

    fn ellipsize_sidebar_text<'a>(
        &self,
        font: &Rc<LoadedFont>,
        text: &'a str,
        width: usize,
    ) -> anyhow::Result<Cow<'a, str>> {
        let max_width = width as f32;
        if max_width <= 0.0 {
            return Ok(Cow::Borrowed(""));
        }

        if self.sidebar_text_width(font, text)? <= max_width {
            return Ok(Cow::Borrowed(text));
        }

        const ELLIPSIS: &str = "...";
        if self.sidebar_text_width(font, ELLIPSIS)? > max_width {
            let mut fallback = String::new();
            for dot_count in 1..=ELLIPSIS.len() {
                let candidate = ".".repeat(dot_count);
                if self.sidebar_text_width(font, &candidate)? > max_width {
                    break;
                }
                fallback = candidate;
            }
            return Ok(Cow::Owned(fallback));
        }

        let mut output = String::new();
        for grapheme in Graphemes::new(text) {
            let mut candidate = output.clone();
            candidate.push_str(grapheme);
            candidate.push_str(ELLIPSIS);
            if self.sidebar_text_width(font, &candidate)? > max_width {
                break;
            }
            output.push_str(grapheme);
        }
        output.push_str(ELLIPSIS);
        Ok(Cow::Owned(output))
    }

    fn sidebar_text_width(&self, font: &Rc<LoadedFont>, text: &str) -> anyhow::Result<f32> {
        let Some(window) = self.window.as_ref().cloned() else {
            return Ok(0.0);
        };
        let infos = font.shape(
            text,
            move || window.notify(crate::termwindow::TermWindowNotif::InvalidateShapeCache),
            BlockKey::filter_out_synthetic,
            None,
            Direction::LeftToRight,
            None,
            None,
        )?;
        Ok(infos.iter().map(|info| info.x_advance.get() as f32).sum())
    }

    fn paint_sidebar_icon(
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
}
