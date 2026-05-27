use crate::customglyph::BlockKey;
use crate::project_sessions;
use crate::quad::{QuadTrait, TripleLayerQuadAllocator, TripleLayerQuadAllocatorTrait};
use crate::termwindow::render::corners::{
    BOTTOM_LEFT_ROUNDED_CORNER, BOTTOM_RIGHT_ROUNDED_CORNER, TOP_LEFT_ROUNDED_CORNER,
    TOP_RIGHT_ROUNDED_CORNER,
};
use crate::termwindow::ui::icons::SvgIcon;
use crate::termwindow::ui::tokens::{
    CAPSULE_BORDER_WIDTH, MACOS_TITLEBAR_CONTENT_TOP_INSET, SIDEBAR_ICON_GAP, SIDEBAR_INSET,
    SIDEBAR_MAX_WIDTH, SIDEBAR_MIN_WIDTH, SIDEBAR_RESIZE_HANDLE_WIDTH, SIDEBAR_ROW_GAP,
    SIDEBAR_ROW_RADIUS, SIDEBAR_WIDTH_CELLS, WINDOW_TAB_FULLSCREEN_SIDEBAR_BUTTON_SIZE,
    WINDOW_TAB_FULLSCREEN_SIDEBAR_BUTTON_X, WINDOW_TAB_FULLSCREEN_SIDEBAR_ICON_SIZE,
};
use crate::termwindow::{UIItem, UIItemType};
use crate::ui::UiPalette;
use crate::utilsprites::RenderMetrics;
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

pub fn workspace_sidebar_width_for_metrics(render_metrics: &RenderMetrics) -> usize {
    (render_metrics.cell_size.width as usize * SIDEBAR_WIDTH_CELLS).max(SIDEBAR_MIN_WIDTH)
}

impl crate::TermWindow {
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
        view: &project_sessions::ProjectSessionView,
        row_height: usize,
        row_gap: usize,
    ) -> usize {
        let mut height = 0usize;
        if !view.pinned_sessions.is_empty() {
            height = height.saturating_add(row_height + row_gap);
            height = height.saturating_add(
                view.pinned_sessions
                    .len()
                    .saturating_mul(row_height + row_gap),
            );
        }
        for project in &view.projects {
            height = height.saturating_add(row_height + row_gap);
            if !project.sessions_collapsed {
                height = height
                    .saturating_add(project.sessions.len().saturating_mul(row_height + row_gap));
            }
        }
        height.saturating_sub(row_gap)
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
        let panel_margin = 0usize;
        let panel_y = rect.y + panel_margin;
        let panel_height = rect.height.saturating_sub(panel_margin * 2).max(1);
        let settings_footer_top = panel_y
            .saturating_add(panel_height)
            .saturating_sub(SIDEBAR_SETTINGS_FOOTER_HEIGHT);
        let content_bottom = (panel_y + panel_height.saturating_sub(SIDEBAR_INSET))
            .min(settings_footer_top.max(panel_y));
        let list_top = self.workspace_sidebar_content_top(panel_y) + ui_cell_height + SIDEBAR_INSET;
        let viewport_height = content_bottom.saturating_sub(list_top);
        if viewport_height == 0 {
            return 0.0;
        }

        let mux = Mux::get();
        let active_workspace = mux.active_workspace();
        let workspaces = mux.iter_workspaces();
        let view = project_sessions::view_for_current_project(&active_workspace, &workspaces);
        let row_height = (ui_cell_height.max(icon_size) + SIDEBAR_INSET).max(52);
        let row_gap = SIDEBAR_ROW_GAP;
        let total_height = Self::workspace_sidebar_list_height(&view, row_height, row_gap);
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
        let panel_margin = 0usize;
        let panel_x = rect.x + panel_margin;
        let panel_y = rect.y + panel_margin;
        let panel_width = rect.width.saturating_sub(panel_margin * 2).max(1);
        let panel_height = rect.height.saturating_sub(panel_margin * 2).max(1);
        let settings_footer_top = panel_y
            .saturating_add(panel_height)
            .saturating_sub(SIDEBAR_SETTINGS_FOOTER_HEIGHT);
        let content_bottom = (panel_y + panel_height.saturating_sub(SIDEBAR_INSET))
            .min(settings_footer_top.max(panel_y));
        let list_top = self.workspace_sidebar_content_top(panel_y) + ui_cell_height + SIDEBAR_INSET;
        let viewport_height = content_bottom.saturating_sub(list_top);
        if viewport_height == 0 {
            return None;
        }

        let mux = Mux::get();
        let active_workspace = mux.active_workspace();
        let workspaces = mux.iter_workspaces();
        let view = project_sessions::view_for_current_project(&active_workspace, &workspaces);
        let row_height = (ui_cell_height.max(icon_size) + SIDEBAR_INSET).max(52);
        let row_gap = SIDEBAR_ROW_GAP;
        let total_height = Self::workspace_sidebar_list_height(&view, row_height, row_gap);
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
        let track_x = panel_x
            .saturating_add(panel_width)
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
        let selected_bg = chrome.control_bg;
        let selected_border = chrome.control_border;
        let active_fg = chrome.text;
        let muted_fg = chrome.secondary_text;
        let ui_font = self
            .fonts
            .title_font_with_size(crate::native_settings::sidebar_font_size())
            .context("sidebar ui font")?;
        let ui_metrics = RenderMetrics::with_font_metrics(&ui_font.metrics());
        let ui_cell_height = ui_metrics.cell_size.height as usize;
        let icon_size = (ui_cell_height + 12).clamp(30, 36);
        let panel_margin = 0usize;
        let panel_x = rect.x + panel_margin;
        let panel_y = rect.y + panel_margin;
        let panel_width = rect.width.saturating_sub(panel_margin * 2).max(1);
        let panel_height = rect.height.saturating_sub(panel_margin * 2).max(1);
        let mut content_bottom = panel_y + panel_height.saturating_sub(SIDEBAR_INSET);

        if rect.y > 0 {
            self.filled_rectangle(
                layers,
                0,
                euclid::rect(rect.x as f32, 0.0, rect.width as f32, rect.y as f32),
                sidebar_bg,
            )
            .context("sidebar header background")?;
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

        let session_row_height = (ui_cell_height.max(icon_size) + SIDEBAR_INSET).max(52);
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
        let active_workspace = mux.active_workspace();
        let workspaces = mux.iter_workspaces();
        let view = project_sessions::view_for_current_project(&active_workspace, &workspaces);

        let header_icon_size = icon_size.min(32);
        let button_size = (header_icon_size + 8).clamp(32, 40);
        let mut y = self.workspace_sidebar_content_top(panel_y);
        let show_sidebar_toolbar = self.window_state.contains(WindowState::FULL_SCREEN);
        let sidebar_toggle_size = WINDOW_TAB_FULLSCREEN_SIDEBAR_BUTTON_SIZE;
        let sidebar_toggle_x = panel_x + WINDOW_TAB_FULLSCREEN_SIDEBAR_BUTTON_X;
        let sidebar_toggle_y = y;
        let sidebar_toggle_icon_size =
            WINDOW_TAB_FULLSCREEN_SIDEBAR_ICON_SIZE.min(sidebar_toggle_size.saturating_sub(2));
        if show_sidebar_toolbar {
            let toggle_hovered = self.is_pointer_over_ui_rect(
                sidebar_toggle_x,
                sidebar_toggle_y,
                sidebar_toggle_size,
                sidebar_toggle_size,
            );
            self.fill_rounded_rectangle(
                layers,
                1,
                euclid::rect(
                    sidebar_toggle_x as f32,
                    sidebar_toggle_y as f32,
                    sidebar_toggle_size as f32,
                    sidebar_toggle_size as f32,
                ),
                foreground.mul_alpha(if toggle_hovered { 0.14 } else { 0.08 }),
                SIDEBAR_ROW_RADIUS,
            )
            .context("sidebar toolbar toggle button")?;
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
            y += sidebar_toggle_size + SIDEBAR_INSET;
        }
        let section_button_size = button_size.min(34);
        let settings_footer_height = SIDEBAR_SETTINGS_FOOTER_HEIGHT.min(panel_height);
        let settings_footer_y = panel_y
            .saturating_add(panel_height)
            .saturating_sub(settings_footer_height);
        content_bottom = content_bottom.min(settings_footer_y.max(panel_y));
        let section_button_x = item_x
            .saturating_add(item_width)
            .saturating_sub(section_button_size + SIDEBAR_INSET);
        let section_button_y = y.saturating_sub(6);
        self.paint_sidebar_text(
            layers,
            &ui_font,
            ui_metrics,
            "Workspaces",
            item_x + SIDEBAR_INSET,
            y,
            section_button_x.saturating_sub(item_x + SIDEBAR_INSET * 2),
            muted_fg,
        )?;
        self.fill_rounded_rectangle(
            layers,
            1,
            euclid::rect(
                section_button_x as f32,
                section_button_y as f32,
                section_button_size as f32,
                section_button_size as f32,
            ),
            foreground.mul_alpha(0.08),
            SIDEBAR_ROW_RADIUS,
        )
        .context("sidebar new project button")?;
        self.ui_items.push(UIItem {
            x: section_button_x,
            y: section_button_y,
            width: section_button_size,
            height: section_button_size,
            item_type: UIItemType::ProjectNew,
        });
        let section_icon_size = header_icon_size.min(section_button_size.saturating_sub(10));
        self.paint_sidebar_icon(
            layers,
            SvgIcon::FolderPlus,
            section_button_x + ((section_button_size.saturating_sub(section_icon_size)) / 2),
            section_button_y + ((section_button_size.saturating_sub(section_icon_size)) / 2),
            section_icon_size,
            foreground,
        )?;
        y += ui_cell_height + SIDEBAR_INSET;
        let list_top = y;
        let row_gap = SIDEBAR_ROW_GAP;
        let max_scroll = {
            let viewport_height = content_bottom.saturating_sub(list_top);
            let total_height =
                Self::workspace_sidebar_list_height(&view, session_row_height, row_gap);
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

        if !view.pinned_sessions.is_empty() {
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
            let pinned_text_x = pinned_x + SIDEBAR_INSET;
            for session in &view.pinned_sessions {
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
                        item_type: UIItemType::ProjectSession(session.id.clone()),
                    });
                    let is_hovered = !suppress_hover
                        && self.is_pointer_over_ui_rect(
                            pinned_x,
                            y,
                            pinned_width,
                            session_row_height,
                        );
                    let is_renaming_session = self.is_renaming_sidebar_session(&session.id);
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
                        .context("sidebar selected pinned session")?;
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
                            foreground.mul_alpha(0.06),
                            SIDEBAR_ROW_RADIUS + 2.0,
                        )
                        .context("sidebar hovered pinned session")?;
                    }

                    let text_y = y + ((session_row_height.saturating_sub(ui_cell_height)) / 2);
                    let action_size = section_button_size
                        .min(session_row_height.saturating_sub(12))
                        .max(24);
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
                    let title = self.sidebar_session_title(&session.id, &session.name);
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
                        for (x, item_type, icon, context_name) in [
                            (
                                pin_x,
                                UIItemType::ProjectSessionPin(session.id.clone()),
                                SvgIcon::PinOff,
                                "sidebar unpin pinned session button",
                            ),
                            (
                                delete_x,
                                UIItemType::ProjectSessionDelete(session.id.clone()),
                                SvgIcon::Trash2,
                                "sidebar delete pinned session button",
                            ),
                        ] {
                            let hovered =
                                self.is_pointer_over_ui_rect(x, action_y, action_size, action_size);
                            self.fill_rounded_rectangle(
                                layers,
                                1,
                                euclid::rect(
                                    x as f32,
                                    action_y as f32,
                                    action_size as f32,
                                    action_size as f32,
                                ),
                                foreground.mul_alpha(if hovered { 0.14 } else { 0.08 }),
                                SIDEBAR_ROW_RADIUS,
                            )
                            .context(context_name)?;
                            self.ui_items.push(UIItem {
                                x,
                                y: action_y,
                                width: action_size,
                                height: action_size,
                                item_type,
                            });
                            let action_icon_size =
                                header_icon_size.min(action_size.saturating_sub(10));
                            self.paint_sidebar_icon(
                                layers,
                                icon,
                                x + ((action_size.saturating_sub(action_icon_size)) / 2),
                                action_y + ((action_size.saturating_sub(action_icon_size)) / 2),
                                action_icon_size,
                                if hovered { foreground } else { muted_fg },
                            )?;
                        }
                    }
                }

                virtual_y += session_row_height + row_gap;
            }
        }

        for project in &view.projects {
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
            let project_action_size =
                section_button_size.min(session_row_height.saturating_sub(12));
            let project_action_x = item_x
                .saturating_add(item_width)
                .saturating_sub(project_action_size + SIDEBAR_INSET);
            let project_text_right = if project.is_active {
                project_action_x
            } else {
                item_x
                    .saturating_add(item_width)
                    .saturating_sub(SIDEBAR_INSET)
            };

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
                    item_type: UIItemType::ProjectToggleSessions(project.id.clone()),
                });
                self.paint_sidebar_icon(
                    layers,
                    if project.sessions_collapsed {
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
                    if project.sessions_collapsed {
                        SvgIcon::Folder
                    } else {
                        SvgIcon::FolderOpen
                    },
                    project_icon_x,
                    icon_y,
                    icon_size,
                    muted_fg,
                )?;
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

            if row_is_visible && project.is_active {
                self.fill_rounded_rectangle(
                    layers,
                    1,
                    euclid::rect(
                        project_action_x as f32,
                        (y + ((session_row_height.saturating_sub(project_action_size)) / 2)) as f32,
                        project_action_size as f32,
                        project_action_size as f32,
                    ),
                    foreground.mul_alpha(0.08),
                    SIDEBAR_ROW_RADIUS,
                )
                .context("sidebar new session button")?;
                self.ui_items.push(UIItem {
                    x: project_action_x,
                    y: y + ((session_row_height.saturating_sub(project_action_size)) / 2),
                    width: project_action_size,
                    height: project_action_size,
                    item_type: UIItemType::ProjectSessionNew(project.id.clone()),
                });
                let action_icon_size = header_icon_size.min(project_action_size.saturating_sub(10));
                self.paint_sidebar_icon(
                    layers,
                    SvgIcon::Plus,
                    project_action_x + ((project_action_size.saturating_sub(action_icon_size)) / 2),
                    y + ((session_row_height.saturating_sub(project_action_size)) / 2)
                        + ((project_action_size.saturating_sub(action_icon_size)) / 2),
                    action_icon_size,
                    foreground,
                )?;
            }

            virtual_y += session_row_height + row_gap;

            if !project.sessions_collapsed {
                let session_x = item_x + SIDEBAR_INSET * 3;
                let session_width = item_width.saturating_sub(SIDEBAR_INSET * 3);
                let session_icon_x = project_icon_x + SIDEBAR_INSET;
                let session_text_x = session_icon_x + icon_size + SIDEBAR_ICON_GAP;
                let guide_x = disclosure_x + disclosure_size / 2;
                let guide_top =
                    (list_top_f + virtual_y as f32 - scroll_offset).max(list_top_f) as usize;
                let session_start_virtual_y = virtual_y;
                let mut guide_bottom = guide_top;

                for session in &project.sessions {
                    let row_top = list_top_f + virtual_y as f32 - scroll_offset;
                    let row_bottom = row_top + session_row_height as f32;
                    let row_is_visible = row_bottom > list_top_f && row_top < content_bottom_f;
                    let y = row_top.floor().max(0.0) as usize;
                    let hit_y = row_top.max(list_top_f).floor().max(0.0) as usize;
                    let hit_bottom =
                        row_bottom.min(content_bottom_f).ceil().max(hit_y as f32) as usize;
                    let hit_height = hit_bottom.saturating_sub(hit_y).max(1);
                    guide_bottom = y.saturating_add(session_row_height);

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
                        .context("sidebar selected session")?;
                    }

                    let icon_y = y + ((session_row_height.saturating_sub(icon_size)) / 2);
                    let text_y = y + ((session_row_height.saturating_sub(ui_cell_height)) / 2);
                    let status_size = 8usize;
                    let status_x = session_x
                        .saturating_add(session_width)
                        .saturating_sub(SIDEBAR_INSET + status_size);
                    let status_y = y + ((session_row_height.saturating_sub(status_size)) / 2);
                    if row_is_visible {
                        self.ui_items.push(UIItem {
                            x: session_x,
                            y: hit_y,
                            width: session_width,
                            height: hit_height,
                            item_type: UIItemType::ProjectSession(session.id.clone()),
                        });
                        let is_hovered = !suppress_hover
                            && self.is_pointer_over_ui_rect(
                                session_x,
                                y,
                                session_width,
                                session_row_height,
                            );
                        let is_renaming_session = self.is_renaming_sidebar_session(&session.id);
                        let action_size = section_button_size
                            .min(session_row_height.saturating_sub(12))
                            .max(24);
                        let delete_x = session_x
                            .saturating_add(session_width)
                            .saturating_sub(SIDEBAR_INSET + action_size);
                        let pin_x = delete_x.saturating_sub(action_size + 4);
                        let action_y = y + ((session_row_height.saturating_sub(action_size)) / 2);
                        let text_right = if is_hovered && !is_renaming_session {
                            pin_x
                        } else {
                            status_x
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
                                foreground.mul_alpha(0.06),
                                SIDEBAR_ROW_RADIUS + 2.0,
                            )
                            .context("sidebar hovered session")?;
                        }
                        self.paint_sidebar_icon(
                            layers,
                            SvgIcon::SquareTerminal,
                            session_icon_x,
                            icon_y,
                            icon_size,
                            if session.is_active {
                                active_fg
                            } else {
                                muted_fg
                            },
                        )?;
                        let session_title = self.sidebar_session_title(&session.id, &session.name);
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
                            for (x, item_type, icon, context_name) in [
                                (
                                    pin_x,
                                    UIItemType::ProjectSessionPin(session.id.clone()),
                                    pin_icon,
                                    "sidebar pin session button",
                                ),
                                (
                                    delete_x,
                                    UIItemType::ProjectSessionDelete(session.id.clone()),
                                    SvgIcon::Trash2,
                                    "sidebar delete session button",
                                ),
                            ] {
                                let hovered = self.is_pointer_over_ui_rect(
                                    x,
                                    action_y,
                                    action_size,
                                    action_size,
                                );
                                self.fill_rounded_rectangle(
                                    layers,
                                    1,
                                    euclid::rect(
                                        x as f32,
                                        action_y as f32,
                                        action_size as f32,
                                        action_size as f32,
                                    ),
                                    foreground.mul_alpha(if hovered { 0.14 } else { 0.08 }),
                                    SIDEBAR_ROW_RADIUS,
                                )
                                .context(context_name)?;
                                self.ui_items.push(UIItem {
                                    x,
                                    y: action_y,
                                    width: action_size,
                                    height: action_size,
                                    item_type,
                                });
                                let action_icon_size =
                                    header_icon_size.min(action_size.saturating_sub(10));
                                self.paint_sidebar_icon(
                                    layers,
                                    icon,
                                    x + ((action_size.saturating_sub(action_icon_size)) / 2),
                                    action_y + ((action_size.saturating_sub(action_icon_size)) / 2),
                                    action_icon_size,
                                    if hovered { foreground } else { muted_fg },
                                )?;
                            }
                        } else {
                            self.fill_rounded_rectangle(
                                layers,
                                1,
                                euclid::rect(
                                    status_x as f32,
                                    status_y as f32,
                                    status_size as f32,
                                    status_size as f32,
                                ),
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
                                },
                                status_size as f32 / 2.0,
                            )
                            .context("sidebar session status")?;
                        }
                    }

                    virtual_y += session_row_height + row_gap;
                }

                if virtual_y > session_start_virtual_y && guide_bottom > guide_top {
                    self.filled_rectangle(
                        layers,
                        0,
                        euclid::rect(
                            guide_x as f32,
                            guide_top as f32,
                            1.0,
                            guide_bottom
                                .min(content_bottom)
                                .saturating_sub(guide_top + row_gap)
                                as f32,
                        ),
                        foreground.mul_alpha(0.16),
                    )
                    .context("sidebar session guide")?;
                }
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
            self.paint_sidebar_text(
                layers,
                &ui_font,
                ui_metrics,
                "Workspaces",
                item_x + SIDEBAR_INSET,
                y.saturating_sub(ui_cell_height + SIDEBAR_INSET),
                section_button_x.saturating_sub(item_x + SIDEBAR_INSET * 2),
                muted_fg,
            )?;
            if show_sidebar_toolbar {
                let toggle_hovered = self.is_pointer_over_ui_rect(
                    sidebar_toggle_x,
                    sidebar_toggle_y,
                    sidebar_toggle_size,
                    sidebar_toggle_size,
                );
                self.fill_rounded_rectangle(
                    layers,
                    2,
                    euclid::rect(
                        sidebar_toggle_x as f32,
                        sidebar_toggle_y as f32,
                        sidebar_toggle_size as f32,
                        sidebar_toggle_size as f32,
                    ),
                    foreground.mul_alpha(if toggle_hovered { 0.14 } else { 0.08 }),
                    SIDEBAR_ROW_RADIUS,
                )
                .context("sidebar toolbar toggle button repaint")?;
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
            self.fill_rounded_rectangle(
                layers,
                2,
                euclid::rect(
                    section_button_x as f32,
                    section_button_y as f32,
                    section_button_size as f32,
                    section_button_size as f32,
                ),
                foreground.mul_alpha(0.08),
                SIDEBAR_ROW_RADIUS,
            )
            .context("sidebar new project button repaint")?;
            self.paint_sidebar_icon(
                layers,
                SvgIcon::FolderPlus,
                section_button_x + ((section_button_size.saturating_sub(section_icon_size)) / 2),
                section_button_y + ((section_button_size.saturating_sub(section_icon_size)) / 2),
                section_icon_size,
                foreground,
            )?;
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

            let settings_row_x = item_x + SIDEBAR_INSET;
            let settings_row_y = settings_footer_y + 8;
            let settings_row_width = item_width.saturating_sub(SIDEBAR_INSET * 2);
            let settings_row_height = 48usize
                .min(settings_footer_height.saturating_sub(SIDEBAR_INSET))
                .max(1);
            let settings_hovered = self.is_pointer_over_ui_rect(
                settings_row_x,
                settings_row_y,
                settings_row_width,
                settings_row_height,
            );
            if settings_hovered {
                self.fill_rounded_rectangle(
                    layers,
                    2,
                    euclid::rect(
                        settings_row_x as f32,
                        settings_row_y as f32,
                        settings_row_width as f32,
                        settings_row_height as f32,
                    ),
                    foreground.mul_alpha(0.10),
                    SIDEBAR_ROW_RADIUS + 4.0,
                )
                .context("sidebar settings button hover")?;
            }
            self.ui_items.push(UIItem {
                x: settings_row_x,
                y: settings_row_y,
                width: settings_row_width,
                height: settings_row_height,
                item_type: UIItemType::WorkspaceSidebarSettings,
            });
            let settings_icon_size = icon_size.min(settings_row_height.saturating_sub(8));
            let settings_icon_x = settings_row_x + SIDEBAR_INSET + 2;
            let settings_icon_y =
                settings_row_y + ((settings_row_height.saturating_sub(settings_icon_size)) / 2);
            let settings_text_x = settings_icon_x + settings_icon_size + SIDEBAR_ICON_GAP + 4;
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
                settings_row_x
                    .saturating_add(settings_row_width)
                    .saturating_sub(settings_text_x + SIDEBAR_INSET),
                foreground,
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
                foreground.mul_alpha(0.06),
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
                foreground.mul_alpha(0.34),
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
        if radius <= 0.0 {
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
}
