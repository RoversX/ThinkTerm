use crate::quad::{
    HeapQuadAllocator, QuadTrait, TripleLayerQuadAllocator, TripleLayerQuadAllocatorTrait,
};
use crate::selection::SelectionRange;
use crate::termwindow::box_model::*;
use crate::termwindow::render::{
    same_hyperlink, CursorProperties, LineQuadCacheKey, LineQuadCacheValue, LineToEleShapeCacheKey,
    RenderScreenLineParams,
};
use crate::termwindow::ui::icons::SvgIcon;
use crate::termwindow::ui::status_icon::{split_leading_legacy_progress_marker, UiStatusKind};
use crate::termwindow::ui::tokens::{
    CAPSULE_BORDER_WIDTH, ICON_BUTTON_BORDER_WIDTH, PANE_NAV_ACTION_BUTTON_RADIUS,
    PANE_NAV_BUTTON_GAP, PANE_NAV_ICON_GAP, PANE_NAV_INSET, PANE_NAV_TAB_GAP, PANE_NAV_TAB_RADIUS,
    PANE_NAV_TAB_TOP_OFFSET, TAB_CLOSE_HOVER_INSET, TAB_CLOSE_HOVER_RADIUS, TAB_CLOSE_RIGHT_GAP,
    TAB_VERTICAL_PADDING,
};
use crate::termwindow::{PaneNavAction, ScrollHit, UIItem, UIItemType};
use crate::ui::UiPalette;
use crate::utilsprites::RenderMetrics;
use ::window::bitmaps::TextureRect;
use ::window::{DeadKeyStatus, RectF};
use anyhow::Context;
use config::VisualBellTarget;
use mux::pane::{PaneId, WithPaneLines};
use mux::renderable::{RenderableDimensions, StableCursorPosition};
use mux::tab::{CollapsedPaneLayout, PositionedPane, SplitDirection};
use mux::Mux;
use ordered_float::NotNan;
use std::rc::Rc;
use std::time::Instant;
use wezterm_dynamic::Value;
use wezterm_font::{FontConfiguration, LoadedFont};
use wezterm_term::color::{ColorAttribute, ColorPalette};
use wezterm_term::{Line, StableRowIndex};
use window::color::LinearRgba;
use window::MouseEventKind as WMEK;

/// Milliseconds since the mux server last responded, when the pane's
/// connection looks unhealthy (a reconnect is pending or in progress).
/// None for local panes and for healthy client panes.
pub(crate) fn client_pane_lag_ms(pane: &dyn mux::pane::Pane) -> Option<u64> {
    let Value::Object(map) = pane.get_metadata() else {
        return None;
    };
    match map.get(&Value::String("is_tardy".to_string())) {
        Some(Value::Bool(true)) => {}
        _ => return None,
    }
    match map.get(&Value::String("since_last_response_ms".to_string())) {
        Some(Value::U64(ms)) => Some(*ms),
        _ => Some(0),
    }
}

impl crate::TermWindow {
    /// Height of the nav bar for this pane: the metric height, clamped so
    /// that at least one terminal row of the pane's cell remains visible.
    pub(crate) fn pane_nav_bar_height_for_pane(&self, pos: &PositionedPane) -> usize {
        self.pane_nav_bar_height().min(
            pos.pixel_height
                .saturating_sub(self.render_metrics.cell_size.height.max(1) as usize),
        )
    }

    fn pane_content_origin(&self, pos: &PositionedPane) -> anyhow::Result<(f32, f32)> {
        let tab_bar_height = if self.show_tab_bar {
            self.tab_bar_pixel_height()
                .context("tab_bar_pixel_height")?
        } else {
            0.
        };
        let top_bar_height = if self.config.tab_bar_at_bottom {
            0.0
        } else {
            tab_bar_height
        };
        let border = self.get_os_border();
        let (padding_left, _) = self.padding_left_top();
        Ok((
            padding_left
                + border.left.get() as f32
                + (pos.left as f32 * self.render_metrics.cell_size.width as f32),
            top_bar_height
                + border.top.get() as f32
                + (pos.top as f32 * self.render_metrics.cell_size.height as f32),
        ))
    }

    pub(crate) fn pane_frame_rect(&self, pos: &PositionedPane) -> anyhow::Result<RectF> {
        let (content_pane_x, pane_y) = self.pane_content_origin(pos)?;
        let content_pane_width = pos.width as f32 * self.render_metrics.cell_size.width as f32;
        let content_pane_right = content_pane_x + content_pane_width;
        let pane_x = if pos.left == 0 && self.workspace_sidebar_width() > 0 {
            self.tab_bar_left_edge() as f32
        } else {
            content_pane_x
        };
        let mut height = (pos.height as f32 * self.render_metrics.cell_size.height as f32).max(1.0);
        if self.collapsed_pane_layouts.contains_key(&pos.pane_stack_id) && pos.top > 0 {
            height = height.max(self.pane_nav_bar_height() as f32);
        }

        Ok(euclid::rect(
            pane_x,
            pane_y,
            (content_pane_right - pane_x).max(1.0),
            height,
        ))
    }

    fn paint_collapsed_pane_nav_bar(
        &mut self,
        pos: &PositionedPane,
        layers: &mut TripleLayerQuadAllocator,
        layout: CollapsedPaneLayout,
    ) -> anyhow::Result<usize> {
        let pane_rect = self.pane_frame_rect(pos)?;
        let chrome = UiPalette::for_appearance(crate::native_settings::effective_appearance());
        let foreground = chrome.text;
        let muted_fg = chrome.secondary_text;

        self.filled_rectangle(layers, 0, pane_rect, chrome.sidebar_bg)
            .context("collapsed pane background")?;

        self.ui_items.push(UIItem {
            x: pane_rect.origin.x.max(0.0) as usize,
            y: pane_rect.origin.y.max(0.0) as usize,
            width: pane_rect.size.width.max(0.0) as usize,
            height: pane_rect.size.height.max(0.0) as usize,
            item_type: UIItemType::PaneNav {
                pane_id: pos.pane.pane_id(),
                pane_index: pos.index,
                action: PaneNavAction::Background,
            },
        });

        const COLLAPSED_EDGE_PADDING: usize = 8;
        const COLLAPSED_BUTTON_GAP: usize = 6;
        const COLLAPSED_SECTION_GAP: usize = 10;

        let tabs = Mux::get().pane_stack_tabs(pos.pane.pane_id());
        let active_tab = tabs.iter().find(|tab| tab.is_active);

        let is_vertical_strip = layout.split_direction == SplitDirection::Horizontal;
        if is_vertical_strip {
            let strip_left = pane_rect.origin.x.max(0.0) as usize;
            let strip_top = pane_rect.origin.y.max(0.0) as usize;
            let strip_width = pane_rect.size.width.max(0.0) as usize;
            let strip_bottom = pane_rect.max_y().max(0.0) as usize;
            let button_size = (pane_rect.size.width as usize)
                .saturating_sub(COLLAPSED_EDGE_PADDING * 2)
                .clamp(24, 30);
            let icon_size = button_size.saturating_sub(8).clamp(16, 22);
            let x = strip_left + (strip_width.saturating_sub(button_size) / 2);
            let mut y = strip_top + COLLAPSED_EDGE_PADDING;

            if let Some(tab) = &active_tab {
                if y.saturating_add(button_size) <= strip_bottom {
                    self.fill_rounded_rectangle_with_border(
                        layers,
                        1,
                        euclid::rect(x as f32, y as f32, button_size as f32, button_size as f32),
                        chrome.control_bg,
                        chrome.control_border,
                        self.ui_f32(PANE_NAV_TAB_RADIUS),
                        CAPSULE_BORDER_WIDTH,
                    )
                    .context("collapsed vertical pane tab chip")?;
                    self.ui_items.push(UIItem {
                        x,
                        y,
                        width: button_size,
                        height: button_size,
                        item_type: UIItemType::PaneNav {
                            pane_id: pos.pane.pane_id(),
                            pane_index: pos.index,
                            action: PaneNavAction::Activate(tab.pane_id),
                        },
                    });
                    self.paint_pane_nav_icon(
                        layers,
                        SvgIcon::SquareTerminal,
                        x + ((button_size.saturating_sub(icon_size)) / 2),
                        y + ((button_size.saturating_sub(icon_size)) / 2),
                        icon_size,
                        foreground,
                    )?;
                    y = y.saturating_add(button_size + COLLAPSED_SECTION_GAP);
                }
            }

            for (icon, action) in [(SvgIcon::Expand, PaneNavAction::ToggleCollapse)] {
                if y.saturating_add(button_size) > strip_bottom {
                    break;
                }
                self.paint_pane_nav_icon_button(
                    layers,
                    icon,
                    x,
                    y,
                    button_size,
                    icon_size,
                    muted_fg,
                    foreground,
                    pos,
                    action,
                )?;
                y = y.saturating_add(button_size + COLLAPSED_BUTTON_GAP);
            }
            return Ok(pane_rect.size.height.max(0.0) as usize);
        }

        let strip_left = pane_rect.origin.x.max(0.0) as usize;
        let strip_right = pane_rect.max_x().max(0.0) as usize;
        let strip_height = pane_rect.size.height.max(1.0) as usize;
        // Draw height is always the metric height: for remote mux panes the
        // bar overlays the content (it steals no viewport rows), for local
        // panes the viewport was already shrunk to make room.
        let chrome_height = self.pane_nav_bar_height().min(strip_height);
        let icon_size = chrome_height
            .saturating_sub(self.ui_px(PANE_NAV_INSET) * 2)
            .clamp(self.ui_px(20), self.ui_px(24));
        let action_icon_size = icon_size
            .saturating_add(self.ui_px(2))
            .clamp(icon_size, self.ui_px(26));
        let button_size = chrome_height
            .saturating_sub(self.ui_px(TAB_VERTICAL_PADDING) * 2)
            .max(action_icon_size);
        let button_y =
            pane_rect.origin.y.max(0.0) as usize + (chrome_height.saturating_sub(button_size) / 2);
        let mut button_x = strip_right.saturating_sub(COLLAPSED_EDGE_PADDING);
        let actions = [(SvgIcon::Expand, PaneNavAction::ToggleCollapse)];
        let action_count = actions.len();
        for (idx, (icon, action)) in actions.iter().copied().enumerate() {
            button_x = button_x.saturating_sub(button_size);
            self.paint_pane_nav_icon_button(
                layers,
                icon,
                button_x,
                button_y,
                button_size,
                action_icon_size,
                muted_fg,
                foreground,
                pos,
                action,
            )?;
            if idx + 1 < action_count {
                button_x = button_x.saturating_sub(COLLAPSED_BUTTON_GAP);
            }
        }

        let tab_start = strip_left + COLLAPSED_EDGE_PADDING;
        let tab_width = self.window_tab_width_pixels().ceil() as usize;
        let tab_step = tab_width + self.ui_px(PANE_NAV_TAB_GAP);
        let total_tab_width = tabs.len().saturating_mul(tab_width).saturating_add(
            tabs.len()
                .saturating_sub(1)
                .saturating_mul(self.ui_px(PANE_NAV_TAB_GAP)),
        );
        let max_tab_right = button_x.saturating_sub(COLLAPSED_SECTION_GAP);
        let viewport_width = max_tab_right.saturating_sub(tab_start);
        let max_scroll = total_tab_width.saturating_sub(viewport_width) as f32;
        let scroll_offset = self
            .pane_nav_tab_scroll_offsets
            .get(&pos.pane_stack_id)
            .copied()
            .unwrap_or(0.0)
            .clamp(0.0, max_scroll.max(0.0));
        let suppress_hover = self
            .current_mouse_event
            .as_ref()
            .is_some_and(|event| matches!(event.kind, WMEK::VertWheel(_) | WMEK::HorzWheel(_)))
            || self.tab_wheel_scroll_active();
        let tab_height = button_size;
        let tab_y = button_y;
        let tab_fg = if pos.is_active { foreground } else { muted_fg };
        for (idx, tab) in tabs.into_iter().enumerate() {
            if tab_width <= icon_size + self.ui_px(PANE_NAV_ICON_GAP) + button_size
                || viewport_width == 0
            {
                break;
            }

            let virtual_x = idx.saturating_mul(tab_step) as f32;
            let tab_left = tab_start as f32 + virtual_x - scroll_offset;
            let tab_right = tab_left + tab_width as f32;
            if tab_right <= tab_start as f32 || tab_left >= max_tab_right as f32 {
                continue;
            }

            let visible_left = tab_left.max(tab_start as f32);
            let visible_right = tab_right.min(max_tab_right as f32);
            let visible_width = (visible_right - visible_left).max(0.0);
            if visible_width <= 1.0 {
                continue;
            }

            let selected_tab = tab.is_active;
            let is_renaming_tab = self.is_renaming_pane_nav_tab(tab.pane_id);
            let this_tab_fg = if selected_tab { tab_fg } else { muted_fg };
            let hover_x = visible_left.max(0.0) as usize;
            let hover_width = visible_width.max(0.0) as usize;
            let is_hovered = !suppress_hover
                && self.is_pointer_over_ui_rect(hover_x, tab_y, hover_width, tab_height);
            let tab_surface_color = if selected_tab {
                chrome.control_bg
            } else if is_hovered && !is_renaming_tab {
                chrome.control_hover_bg
            } else {
                foreground.mul_alpha(0.025)
            };
            let tab_border_color = if selected_tab {
                chrome.control_border
            } else {
                LinearRgba::TRANSPARENT
            };
            self.fill_rounded_rectangle_with_border(
                layers,
                1,
                euclid::rect(visible_left, tab_y as f32, visible_width, tab_height as f32),
                tab_surface_color,
                tab_border_color,
                self.ui_f32(PANE_NAV_TAB_RADIUS),
                CAPSULE_BORDER_WIDTH,
            )
            .context("collapsed pane nav tab surface")?;

            self.ui_items.push(UIItem {
                x: visible_left.max(0.0) as usize,
                y: tab_y,
                width: visible_width.max(0.0) as usize,
                height: tab_height,
                item_type: UIItemType::PaneNav {
                    pane_id: pos.pane.pane_id(),
                    pane_index: pos.index,
                    action: PaneNavAction::Activate(tab.pane_id),
                },
            });

            let draw_tab_x = tab_left.max(0.0) as usize;
            let title_icon_x = draw_tab_x + self.ui_px(PANE_NAV_INSET);
            let title_icon_y = tab_y + ((tab_height.saturating_sub(icon_size)) / 2);
            let raw_title = self.pane_nav_tab_title(tab.pane_id, &tab.title);
            let (title, status) =
                if let Some(title) = split_leading_legacy_progress_marker(&raw_title) {
                    (
                        if title.is_empty() { "Terminal" } else { title },
                        Some(UiStatusKind::Running),
                    )
                } else {
                    (raw_title.as_str(), None)
                };
            if title_icon_x >= tab_start && title_icon_x.saturating_add(icon_size) <= max_tab_right
            {
                if let Some(status) = status {
                    self.paint_status_icon(
                        layers,
                        2,
                        status,
                        title_icon_x,
                        title_icon_y,
                        icon_size,
                        this_tab_fg,
                    )?;
                } else {
                    self.paint_pane_nav_icon(
                        layers,
                        SvgIcon::SquareTerminal,
                        title_icon_x,
                        title_icon_y,
                        icon_size,
                        this_tab_fg,
                    )?;
                }
            }

            let raw_close_x = draw_tab_x
                .saturating_add(tab_width)
                .saturating_sub(button_size + self.ui_px(TAB_CLOSE_RIGHT_GAP));
            let close_slot_reserved = !is_renaming_tab;
            let close_view_left = draw_tab_x.max(tab_start);
            let close_view_right = draw_tab_x.saturating_add(tab_width).min(max_tab_right);
            let close_x = raw_close_x
                .min(close_view_right.saturating_sub(button_size))
                .max(close_view_left);
            let show_close = close_slot_reserved
                && (selected_tab || is_hovered)
                && close_view_right.saturating_sub(close_view_left) >= button_size;
            if show_close {
                let close_hovered =
                    self.is_pointer_over_ui_rect(close_x, tab_y, button_size, button_size);
                if close_hovered {
                    let hover_alpha = if self.is_pointer_pressing_ui_rect(
                        close_x,
                        tab_y,
                        button_size,
                        button_size,
                    ) {
                        0.20
                    } else {
                        0.12
                    };
                    let hover_inset = self.ui_px(TAB_CLOSE_HOVER_INSET).min(button_size / 2);
                    let hover_size = button_size.saturating_sub(hover_inset * 2);
                    self.fill_rounded_rectangle(
                        layers,
                        1,
                        euclid::rect(
                            (close_x + hover_inset) as f32,
                            (tab_y + hover_inset) as f32,
                            hover_size as f32,
                            hover_size as f32,
                        ),
                        foreground.mul_alpha(hover_alpha),
                        TAB_CLOSE_HOVER_RADIUS,
                    )
                    .context("collapsed pane nav close hover")?;
                }
                self.ui_items.push(UIItem {
                    x: close_x,
                    y: tab_y,
                    width: button_size,
                    height: button_size,
                    item_type: UIItemType::PaneNav {
                        pane_id: pos.pane.pane_id(),
                        pane_index: pos.index,
                        action: PaneNavAction::Close(tab.pane_id),
                    },
                });
                self.paint_pane_nav_icon(
                    layers,
                    SvgIcon::X,
                    close_x + ((button_size.saturating_sub(icon_size)) / 2),
                    title_icon_y,
                    icon_size,
                    if close_hovered { foreground } else { muted_fg },
                )?;
            }

            let ui_font = self
                .fonts
                .title_font_with_size(crate::native_settings::pane_header_font_size())
                .context("collapsed pane nav title font")?;
            let ui_metrics = RenderMetrics::with_font_metrics(&ui_font.metrics());
            let text_x = title_icon_x + icon_size + self.ui_px(PANE_NAV_ICON_GAP);
            let text_right = if close_slot_reserved {
                if show_close {
                    close_x
                } else {
                    raw_close_x
                }
            } else {
                draw_tab_x + tab_width - self.ui_px(PANE_NAV_INSET)
            };
            let text_width = text_right
                .min(max_tab_right)
                .saturating_sub(text_x + self.ui_px(PANE_NAV_ICON_GAP));
            let text_y =
                tab_y + ((tab_height.saturating_sub(ui_metrics.cell_size.height as usize)) / 2);
            if text_x >= tab_start && text_x < max_tab_right && text_width > 0 {
                let text_fg = if is_renaming_tab {
                    self.fill_rounded_rectangle(
                        layers,
                        1,
                        euclid::rect(
                            text_x.saturating_sub(3) as f32,
                            text_y as f32,
                            text_width.saturating_add(6) as f32,
                            ui_metrics.cell_size.height as f32,
                        ),
                        chrome.selected_bg,
                        4.0,
                    )
                    .context("collapsed pane nav rename selection")?;
                    chrome.selected_text
                } else {
                    this_tab_fg
                };
                self.paint_pane_nav_text(
                    layers, &ui_font, ui_metrics, title, text_x, text_y, text_width, text_fg,
                )?;
            }
        }

        Ok(strip_height)
    }

    fn can_collapse_pane_stack(&self, pos: &PositionedPane) -> bool {
        let Some(tab) = Mux::get().get_active_tab_for_window(self.mux_window_id) else {
            return false;
        };

        if tab.pane_split_direction_by_index(pos.index) != Some(SplitDirection::Vertical) {
            return false;
        }

        let panes = tab.iter_panes_ignoring_zoom();
        if panes.iter().any(|pane| {
            self.collapsed_pane_layouts
                .contains_key(&pane.pane_stack_id)
        }) {
            return false;
        }

        panes
            .into_iter()
            .any(|pane| pane.pane_stack_id != pos.pane_stack_id)
    }

    fn paint_pane_nav_bar(
        &mut self,
        pos: &PositionedPane,
        layers: &mut TripleLayerQuadAllocator,
        _palette: &ColorPalette,
    ) -> anyhow::Result<usize> {
        if let Some(layout) = self.collapsed_pane_layouts.get(&pos.pane_stack_id).copied() {
            return self.paint_collapsed_pane_nav_bar(pos, layers, layout);
        }

        let nav_height = self.pane_nav_bar_height_for_pane(pos);
        if nav_height == 0 {
            return Ok(0);
        }

        let (content_pane_x, pane_y) = self.pane_content_origin(pos)?;
        let content_pane_width = pos.width as f32 * self.render_metrics.cell_size.width as f32;
        if content_pane_width <= 0.0 {
            return Ok(0);
        }
        let content_pane_right = content_pane_x + content_pane_width;
        let pane_x = if pos.left == 0 && self.workspace_sidebar_width() > 0 {
            self.tab_bar_left_edge() as f32
        } else {
            content_pane_x
        };
        let pane_width = (content_pane_right - pane_x).max(1.0);

        let chrome = UiPalette::for_appearance(crate::native_settings::effective_appearance());
        let background = chrome.sidebar_bg;
        let foreground = chrome.text;
        let muted_fg = chrome.secondary_text;
        let tab_fg = if pos.is_active { foreground } else { muted_fg };

        self.filled_rectangle(
            layers,
            0,
            euclid::rect(pane_x, pane_y, pane_width, nav_height as f32),
            background,
        )
        .context("pane nav background")?;

        self.ui_items.push(UIItem {
            x: pane_x.max(0.0) as usize,
            y: pane_y.max(0.0) as usize,
            width: pane_width.max(0.0) as usize,
            height: nav_height,
            item_type: UIItemType::PaneNav {
                pane_id: pos.pane.pane_id(),
                pane_index: pos.index,
                action: PaneNavAction::Background,
            },
        });

        let icon_size = nav_height
            .saturating_sub(self.ui_px(PANE_NAV_INSET) * 2)
            .clamp(self.ui_px(20), self.ui_px(24));
        let action_icon_size = icon_size
            .saturating_add(self.ui_px(2))
            .clamp(icon_size, self.ui_px(26));
        let button_size = nav_height
            .saturating_sub(self.ui_px(TAB_VERTICAL_PADDING) * 2)
            .max(action_icon_size);
        let button_y = pane_y as usize
            + (nav_height.saturating_sub(button_size) / 2 + self.ui_px(PANE_NAV_TAB_TOP_OFFSET))
                .min(nav_height.saturating_sub(button_size));
        let mut button_x = (pane_x + pane_width) as usize;
        let mut actions = vec![
            (SvgIcon::Plus, PaneNavAction::NewTab),
            (SvgIcon::SplitVertical, PaneNavAction::SplitDown),
            (SvgIcon::SplitHorizontal, PaneNavAction::SplitRight),
        ];
        if self.can_collapse_pane_stack(pos) {
            actions.push((SvgIcon::Shrink, PaneNavAction::ToggleCollapse));
        }
        actions.push((
            if pos.is_zoomed {
                SvgIcon::Minimize2
            } else {
                SvgIcon::Maximize2
            },
            PaneNavAction::ToggleZoom,
        ));
        for (icon, action) in actions {
            button_x = button_x.saturating_sub(button_size + self.ui_px(PANE_NAV_BUTTON_GAP));
            // Stop once a button would spill past the pane's left edge (happens
            // when the pane is too narrow to hold all the action buttons).
            if (button_x as f32) < pane_x {
                break;
            }
            self.paint_pane_nav_icon_button(
                layers,
                icon,
                button_x,
                button_y,
                button_size,
                action_icon_size,
                muted_fg,
                foreground,
                pos,
                action,
            )?;
        }

        let tabs = Mux::get().pane_stack_tabs(pos.pane.pane_id());
        let tab_start = pane_x as usize + self.pane_nav_tab_left_inset(pos.left);
        let tab_width = self.window_tab_width_pixels().ceil() as usize;
        let tab_step = tab_width + self.ui_px(PANE_NAV_TAB_GAP);
        let total_tab_width = tabs.len().saturating_mul(tab_width).saturating_add(
            tabs.len()
                .saturating_sub(1)
                .saturating_mul(self.ui_px(PANE_NAV_TAB_GAP)),
        );
        let max_tab_right = button_x.saturating_sub(self.ui_px(PANE_NAV_INSET));
        let viewport_width = max_tab_right.saturating_sub(tab_start);
        let max_scroll = total_tab_width.saturating_sub(viewport_width) as f32;
        let scroll_offset = self
            .pane_nav_tab_scroll_offsets
            .get(&pos.pane_stack_id)
            .copied()
            .unwrap_or(0.0)
            .clamp(0.0, max_scroll.max(0.0));
        let suppress_hover = self
            .current_mouse_event
            .as_ref()
            .is_some_and(|event| matches!(event.kind, WMEK::VertWheel(_) | WMEK::HorzWheel(_)))
            || self.tab_wheel_scroll_active();
        let tab_height = button_size;
        let tab_y = button_y;
        for (idx, tab) in tabs.into_iter().enumerate() {
            if tab_width <= icon_size + self.ui_px(PANE_NAV_ICON_GAP) + button_size
                || viewport_width == 0
            {
                break;
            }

            let virtual_x = idx.saturating_mul(tab_step) as f32;
            let tab_left = tab_start as f32 + virtual_x - scroll_offset;
            let tab_right = tab_left + tab_width as f32;
            if tab_right <= tab_start as f32 || tab_left >= max_tab_right as f32 {
                continue;
            }

            let visible_left = tab_left.max(tab_start as f32);
            let visible_right = tab_right.min(max_tab_right as f32);
            let visible_width = (visible_right - visible_left).max(0.0);
            if visible_width <= 1.0 {
                continue;
            }

            let selected_tab = tab.is_active;
            let is_renaming_tab = self.is_renaming_pane_nav_tab(tab.pane_id);
            let this_tab_fg = if selected_tab { tab_fg } else { muted_fg };
            let hover_x = visible_left.max(0.0) as usize;
            let hover_width = visible_width.max(0.0) as usize;
            let is_hovered = !suppress_hover
                && self.is_pointer_over_ui_rect(hover_x, tab_y, hover_width, tab_height);
            let tab_surface_color = if selected_tab {
                chrome.control_bg
            } else if is_hovered && !is_renaming_tab {
                chrome.control_hover_bg
            } else {
                foreground.mul_alpha(0.025)
            };
            let tab_border_color = if selected_tab {
                chrome.control_border
            } else {
                LinearRgba::TRANSPARENT
            };
            self.fill_rounded_rectangle_with_border(
                layers,
                1,
                euclid::rect(visible_left, tab_y as f32, visible_width, tab_height as f32),
                tab_surface_color,
                tab_border_color,
                self.ui_f32(PANE_NAV_TAB_RADIUS),
                CAPSULE_BORDER_WIDTH,
            )
            .context("pane nav tab surface")?;
            self.ui_items.push(UIItem {
                x: visible_left.max(0.0) as usize,
                y: tab_y,
                width: visible_width.max(0.0) as usize,
                height: tab_height,
                item_type: UIItemType::PaneNav {
                    pane_id: pos.pane.pane_id(),
                    pane_index: pos.index,
                    action: PaneNavAction::Activate(tab.pane_id),
                },
            });

            let draw_tab_x = tab_left.max(0.0) as usize;
            let title_icon_x = draw_tab_x + self.ui_px(PANE_NAV_INSET);
            let title_icon_y = tab_y + ((tab_height.saturating_sub(icon_size)) / 2);
            let raw_title = self.pane_nav_tab_title(tab.pane_id, &tab.title);
            let (title, status) =
                if let Some(title) = split_leading_legacy_progress_marker(&raw_title) {
                    (
                        if title.is_empty() { "Terminal" } else { title },
                        Some(UiStatusKind::Running),
                    )
                } else {
                    (raw_title.as_str(), None)
                };
            if title_icon_x >= tab_start && title_icon_x.saturating_add(icon_size) <= max_tab_right
            {
                if let Some(status) = status {
                    self.paint_status_icon(
                        layers,
                        2,
                        status,
                        title_icon_x,
                        title_icon_y,
                        icon_size,
                        this_tab_fg,
                    )?;
                } else {
                    self.paint_pane_nav_icon(
                        layers,
                        SvgIcon::SquareTerminal,
                        title_icon_x,
                        title_icon_y,
                        icon_size,
                        this_tab_fg,
                    )?;
                }
            }

            let raw_close_x = draw_tab_x
                .saturating_add(tab_width)
                .saturating_sub(button_size + self.ui_px(TAB_CLOSE_RIGHT_GAP));
            let close_slot_reserved = !is_renaming_tab;
            let close_view_left = draw_tab_x.max(tab_start);
            let close_view_right = draw_tab_x.saturating_add(tab_width).min(max_tab_right);
            let close_x = raw_close_x
                .min(close_view_right.saturating_sub(button_size))
                .max(close_view_left);
            let show_close = close_slot_reserved
                && (selected_tab || is_hovered)
                && close_view_right.saturating_sub(close_view_left) >= button_size;
            if show_close {
                let close_hovered =
                    self.is_pointer_over_ui_rect(close_x, tab_y, button_size, button_size);
                if close_hovered {
                    let hover_alpha = if self.is_pointer_pressing_ui_rect(
                        close_x,
                        tab_y,
                        button_size,
                        button_size,
                    ) {
                        0.20
                    } else {
                        0.12
                    };
                    let hover_inset = self.ui_px(TAB_CLOSE_HOVER_INSET).min(button_size / 2);
                    let hover_size = button_size.saturating_sub(hover_inset * 2);
                    self.fill_rounded_rectangle(
                        layers,
                        1,
                        euclid::rect(
                            (close_x + hover_inset) as f32,
                            (tab_y + hover_inset) as f32,
                            hover_size as f32,
                            hover_size as f32,
                        ),
                        foreground.mul_alpha(hover_alpha),
                        TAB_CLOSE_HOVER_RADIUS,
                    )
                    .context("pane nav close hover")?;
                }
                self.ui_items.push(UIItem {
                    x: close_x,
                    y: tab_y,
                    width: button_size,
                    height: button_size,
                    item_type: UIItemType::PaneNav {
                        pane_id: pos.pane.pane_id(),
                        pane_index: pos.index,
                        action: PaneNavAction::Close(tab.pane_id),
                    },
                });
                self.paint_pane_nav_icon(
                    layers,
                    SvgIcon::X,
                    close_x + ((button_size.saturating_sub(icon_size)) / 2),
                    title_icon_y,
                    icon_size,
                    if close_hovered { foreground } else { muted_fg },
                )?;
            }

            let ui_font = self
                .fonts
                .title_font_with_size(crate::native_settings::pane_header_font_size())
                .context("pane nav title font")?;
            let ui_metrics = RenderMetrics::with_font_metrics(&ui_font.metrics());
            let text_x = title_icon_x + icon_size + self.ui_px(PANE_NAV_ICON_GAP);
            let text_right = if close_slot_reserved {
                if show_close {
                    close_x
                } else {
                    raw_close_x
                }
            } else {
                draw_tab_x + tab_width - self.ui_px(PANE_NAV_INSET)
            };
            let text_width = text_right
                .min(max_tab_right)
                .saturating_sub(text_x + self.ui_px(PANE_NAV_ICON_GAP));
            let text_y =
                tab_y + ((tab_height.saturating_sub(ui_metrics.cell_size.height as usize)) / 2);
            if text_x >= tab_start && text_x < max_tab_right && text_width > 0 {
                let text_fg = if is_renaming_tab {
                    self.fill_rounded_rectangle(
                        layers,
                        1,
                        euclid::rect(
                            text_x.saturating_sub(3) as f32,
                            text_y as f32,
                            text_width.saturating_add(6) as f32,
                            ui_metrics.cell_size.height as f32,
                        ),
                        chrome.selected_bg,
                        4.0,
                    )
                    .context("pane nav rename selection")?;
                    chrome.selected_text
                } else {
                    this_tab_fg
                };
                self.paint_pane_nav_text(
                    layers, &ui_font, ui_metrics, title, text_x, text_y, text_width, text_fg,
                )?;
            }
        }

        Ok(nav_height)
    }

    fn paint_pane_nav_icon_button(
        &mut self,
        layers: &mut TripleLayerQuadAllocator,
        icon: SvgIcon,
        x: usize,
        y: usize,
        button_size: usize,
        icon_size: usize,
        color: LinearRgba,
        hover_color: LinearRgba,
        pos: &PositionedPane,
        action: PaneNavAction,
    ) -> anyhow::Result<()> {
        self.ui_items.push(UIItem {
            x,
            y,
            width: button_size,
            height: button_size,
            item_type: UIItemType::PaneNav {
                pane_id: pos.pane.pane_id(),
                pane_index: pos.index,
                action,
            },
        });

        let hovered = self.is_pointer_over_ui_rect(x, y, button_size, button_size);
        let pressed = hovered && self.is_pointer_pressing_ui_rect(x, y, button_size, button_size);
        let press_inset = if pressed { 1 } else { 0 };
        let visual_size = button_size.saturating_sub(press_inset * 2);
        if hovered {
            let chrome = UiPalette::for_appearance(crate::native_settings::effective_appearance());
            let fill = if pressed {
                chrome.control_pressed_bg
            } else {
                chrome.control_hover_bg
            };
            let border_alpha = if pressed { 0.52 } else { 0.38 };
            self.fill_rounded_rectangle_with_border(
                layers,
                1,
                euclid::rect(
                    (x + press_inset) as f32,
                    (y + press_inset) as f32,
                    visual_size as f32,
                    visual_size as f32,
                ),
                fill,
                color.mul_alpha(border_alpha),
                PANE_NAV_ACTION_BUTTON_RADIUS,
                ICON_BUTTON_BORDER_WIDTH,
            )
            .context("pane nav button hover")?;
        }

        let icon_size = if pressed {
            icon_size.saturating_sub(1).max(1)
        } else {
            icon_size
        };
        self.paint_pane_nav_icon(
            layers,
            icon,
            x + press_inset + ((visual_size.saturating_sub(icon_size)) / 2),
            y + press_inset + ((visual_size.saturating_sub(icon_size)) / 2),
            icon_size,
            if hovered { hover_color } else { color },
        )
    }

    fn paint_pane_nav_text(
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
        if width == 0 {
            return Ok(());
        }

        let cell_width = (metrics.cell_size.width as usize).max(1);
        if width < cell_width {
            return Ok(());
        }

        let text = self.ellipsize_ui_text(font, text, width)?;
        self.paint_ui_title_text(layers, font, &metrics, &text, x, y, width, foreground)
    }

    fn paint_pane_nav_icon(
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

    fn paint_pane_box_model(&mut self, pos: &PositionedPane) -> anyhow::Result<()> {
        let computed = self.build_pane(pos)?;
        let mut ui_items = computed.ui_items();
        self.ui_items.append(&mut ui_items);
        let gl_state = self.render_state.as_ref().unwrap();
        self.render_element(&computed, gl_state, None)
    }

    pub fn paint_pane(
        &mut self,
        pos: &PositionedPane,
        layers: &mut TripleLayerQuadAllocator,
    ) -> anyhow::Result<()> {
        if self.config.use_box_model_render {
            return self.paint_pane_box_model(pos);
        }

        self.check_for_dirty_lines_and_invalidate_selection(&pos.pane);
        /*
        let zone = {
            let dims = pos.pane.get_dimensions();
            let position = self
                .get_viewport(pos.pane.pane_id())
                .unwrap_or(dims.physical_top);

            let zones = self.get_semantic_zones(&pos.pane);
            let idx = match zones.binary_search_by(|zone| zone.start_y.cmp(&position)) {
                Ok(idx) | Err(idx) => idx,
            };
            let idx = ((idx as isize) - 1).max(0) as usize;
            zones.get(idx).cloned()
        };
        */

        let global_cursor_fg = self.palette().cursor_fg;
        let global_cursor_bg = self.palette().cursor_bg;
        let config = self.config.clone();
        let palette = pos.pane.palette();

        let (padding_left, padding_top) = self.padding_left_top();

        let tab_bar_height = if self.show_tab_bar {
            self.tab_bar_pixel_height()
                .context("tab_bar_pixel_height")?
        } else {
            0.
        };
        let (top_bar_height, bottom_bar_height) = if self.config.tab_bar_at_bottom {
            (0.0, tab_bar_height)
        } else {
            (tab_bar_height, 0.0)
        };

        let border = self.get_os_border();
        let top_pixel_y = top_bar_height + padding_top + border.top.get() as f32;

        let cursor = pos.pane.get_cursor_position();
        if pos.is_active {
            self.prev_cursor.update(&cursor);
        }

        let pane_id = pos.pane.pane_id();
        let current_viewport = self.get_viewport(pane_id);
        let dims = pos.pane.get_dimensions();
        let global_render_metrics = self.render_metrics;
        let pane_font_scale = self.pane_font_scale(pane_id);
        let (pane_font_config, pane_render_metrics) =
            if pane_font_scale.to_bits() == self.fonts.get_font_scale().to_bits() {
                (None, global_render_metrics)
            } else {
                let (fonts, metrics) = self.pane_font_resources(pane_font_scale)?;
                (Some(fonts), metrics)
            };
        let render_dims = dims;

        let gl_state = self.render_state.as_ref().unwrap();

        let cursor_border_color = palette.cursor_border.to_linear();
        let foreground = palette.foreground.to_linear();
        let white_space = gl_state.util_sprites.white_space.texture_coords();
        let filled_box = gl_state.util_sprites.filled_box.texture_coords();

        let window_is_transparent =
            !self.window_background.is_empty() || config.window_background_opacity != 1.0;

        let dark_chrome_background = matches!(
            crate::native_settings::effective_appearance(),
            window::Appearance::Dark | window::Appearance::DarkHighContrast
        )
        .then(|| {
            UiPalette::for_appearance(crate::native_settings::effective_appearance()).sidebar_bg
        });
        let default_bg = dark_chrome_background
            .unwrap_or_else(|| palette.resolve_bg(ColorAttribute::Default).to_linear())
            .mul_alpha(if window_is_transparent {
                0.
            } else {
                config.text_background_opacity
            });

        let cell_width = self.render_metrics.cell_size.width as f32;
        let cell_height = self.render_metrics.cell_size.height as f32;
        let background_rect = {
            // We want to fill out to the edges of the splits
            let (x, width_delta) = if pos.left == 0 {
                (
                    0.,
                    padding_left + border.left.get() as f32 + (cell_width / 2.0),
                )
            } else {
                (
                    padding_left + border.left.get() as f32 - (cell_width / 2.0)
                        + (pos.left as f32 * cell_width),
                    cell_width,
                )
            };

            let (y, height_delta) = if pos.top == 0 {
                (
                    (top_pixel_y - padding_top),
                    padding_top + (cell_height / 2.0),
                )
            } else {
                (
                    top_pixel_y + (pos.top as f32 * cell_height) - (cell_height / 2.0),
                    cell_height,
                )
            };
            euclid::rect(
                x,
                y,
                // Go all the way to the right edge if we're right-most
                if pos.left + pos.width >= self.terminal_size.cols as usize {
                    self.dimensions.pixel_width as f32 - x
                } else {
                    (pos.width as f32 * cell_width) + width_delta
                },
                // Go all the way to the bottom if we're bottom-most
                if pos.top + pos.height >= self.terminal_size.rows as usize {
                    self.dimensions.pixel_height as f32 - y
                } else {
                    (pos.height as f32 * cell_height) + height_delta as f32
                },
            )
        };

        if self.window_background.is_empty() {
            // Per-pane, palette-specified background

            let mut quad = self
                .filled_rectangle(
                    layers,
                    0,
                    background_rect,
                    dark_chrome_background
                        .unwrap_or_else(|| palette.background.to_linear())
                        .mul_alpha(config.window_background_opacity),
                )
                .context("filled_rectangle")?;
            quad.set_hsv(if pos.is_active {
                None
            } else {
                Some(config.inactive_pane_hsb)
            });
        }

        {
            // If the bell is ringing, we draw another background layer over the
            // top of this in the configured bell color
            if let Some(intensity) = self.get_intensity_if_bell_target_ringing(
                &pos.pane,
                &config,
                VisualBellTarget::BackgroundColor,
            ) {
                // target background color
                let LinearRgba(r, g, b, _) = config
                    .resolved_palette
                    .visual_bell
                    .as_deref()
                    .unwrap_or(&palette.foreground)
                    .to_linear();

                let background = if window_is_transparent {
                    // for transparent windows, we fade in the target color
                    // by adjusting its alpha
                    LinearRgba::with_components(r, g, b, intensity)
                } else {
                    // otherwise We'll interpolate between the background color
                    // and the the target color
                    let (r1, g1, b1, a) = palette
                        .background
                        .to_linear()
                        .mul_alpha(config.window_background_opacity)
                        .tuple();
                    LinearRgba::with_components(
                        r1 + (r - r1) * intensity,
                        g1 + (g - g1) * intensity,
                        b1 + (b - b1) * intensity,
                        a,
                    )
                };
                log::trace!("bell color is {:?}", background);

                let mut quad = self
                    .filled_rectangle(layers, 0, background_rect, background)
                    .context("filled_rectangle")?;

                quad.set_hsv(if pos.is_active {
                    None
                } else {
                    Some(config.inactive_pane_hsb)
                });
            }
        }

        let pane_nav_height = self
            .paint_pane_nav_bar(pos, layers, &palette)
            .context("paint_pane_nav_bar")?;
        if self.collapsed_pane_layouts.contains_key(&pos.pane_stack_id) {
            return Ok(());
        }

        // TODO: we only have a single scrollbar in a single position.
        // We only update it for the active pane, but we should probably
        // do a per-pane scrollbar.  That will require more extensive
        // changes to ScrollHit, mouse positioning, PositionedPane
        // and tab size calculation.
        if pos.is_active && self.show_scroll_bar {
            let thumb_y_offset = top_bar_height as usize + border.top.get();

            let min_height = self.min_scroll_bar_height();

            let info = ScrollHit::thumb(
                &*pos.pane,
                current_viewport,
                self.dimensions.pixel_height.saturating_sub(
                    thumb_y_offset + border.bottom.get() + bottom_bar_height as usize,
                ),
                min_height as usize,
            );
            let abs_thumb_top = thumb_y_offset + info.top;
            let thumb_size = info.height;
            let color = palette.scrollbar_thumb.to_linear();

            // Adjust the scrollbar thumb position
            let config = &self.config;
            let padding = self.effective_right_padding(&config) as f32;

            let thumb_x = self.dimensions.pixel_width - padding as usize - border.right.get();

            // Register the scroll bar location
            self.ui_items.push(UIItem {
                x: thumb_x,
                width: padding as usize,
                y: thumb_y_offset,
                height: info.top,
                item_type: UIItemType::AboveScrollThumb,
            });
            self.ui_items.push(UIItem {
                x: thumb_x,
                width: padding as usize,
                y: abs_thumb_top,
                height: thumb_size,
                item_type: UIItemType::ScrollThumb,
            });
            self.ui_items.push(UIItem {
                x: thumb_x,
                width: padding as usize,
                y: abs_thumb_top + thumb_size,
                height: self
                    .dimensions
                    .pixel_height
                    .saturating_sub(abs_thumb_top + thumb_size),
                item_type: UIItemType::BelowScrollThumb,
            });

            self.filled_rectangle(
                layers,
                2,
                euclid::rect(
                    thumb_x as f32,
                    abs_thumb_top as f32,
                    padding,
                    thumb_size as f32,
                ),
                color,
            )
            .context("filled_rectangle")?;
        }

        let (selrange, rectangular) = {
            let sel = self.selection(pos.pane.pane_id());
            (sel.range.clone(), sel.rectangular)
        };

        let start = Instant::now();
        let selection_fg = palette.selection_fg.to_linear();
        let selection_bg = palette.selection_bg.to_linear();
        let cursor_fg = palette.cursor_fg.to_linear();
        let cursor_bg = palette.cursor_bg.to_linear();
        let cursor_is_default_color =
            palette.cursor_fg == global_cursor_fg && palette.cursor_bg == global_cursor_bg;

        {
            let stable_range = match current_viewport {
                Some(top) => top..top + render_dims.viewport_rows as StableRowIndex,
                None => {
                    dims.physical_top
                        ..dims.physical_top + render_dims.viewport_rows as StableRowIndex
                }
            };

            pos.pane
                .apply_hyperlinks(stable_range.clone(), &self.config.hyperlink_rules);

            struct LineRender<'a, 'b> {
                term_window: &'a mut crate::TermWindow,
                selrange: Option<SelectionRange>,
                rectangular: bool,
                dims: RenderableDimensions,
                top_pixel_y: f32,
                left_pixel_x: f32,
                render_metrics: RenderMetrics,
                font_config: Option<Rc<FontConfiguration>>,
                font_identity: u64,
                pos: &'a PositionedPane,
                pane_id: PaneId,
                cursor: &'a StableCursorPosition,
                palette: &'a ColorPalette,
                default_bg: LinearRgba,
                cursor_border_color: LinearRgba,
                selection_fg: LinearRgba,
                selection_bg: LinearRgba,
                cursor_fg: LinearRgba,
                cursor_bg: LinearRgba,
                foreground: LinearRgba,
                cursor_is_default_color: bool,
                white_space: TextureRect,
                filled_box: TextureRect,
                window_is_transparent: bool,
                layers: &'a mut TripleLayerQuadAllocator<'b>,
                error: Option<anyhow::Error>,
            }

            let left_pixel_x = padding_left
                + border.left.get() as f32
                + (pos.left as f32 * global_render_metrics.cell_size.width as f32);
            let pane_top_pixel_y = top_pixel_y
                + (pos.top as f32 * global_render_metrics.cell_size.height as f32)
                + pane_nav_height as f32;

            let mut render = LineRender {
                term_window: self,
                selrange,
                rectangular,
                dims: render_dims,
                top_pixel_y: pane_top_pixel_y,
                left_pixel_x,
                render_metrics: pane_render_metrics,
                font_config: pane_font_config,
                font_identity: pane_font_scale.to_bits(),
                pos,
                pane_id,
                cursor: &cursor,
                palette: &palette,
                cursor_border_color,
                selection_fg,
                selection_bg,
                cursor_fg,
                default_bg,
                cursor_bg,
                foreground,
                cursor_is_default_color,
                white_space,
                filled_box,
                window_is_transparent,
                layers,
                error: None,
            };

            impl<'a, 'b> LineRender<'a, 'b> {
                fn render_line(
                    &mut self,
                    stable_top: StableRowIndex,
                    line_idx: usize,
                    line: &&mut Line,
                ) -> anyhow::Result<()> {
                    let stable_row = stable_top + line_idx as StableRowIndex;
                    let selrange = self
                        .selrange
                        .map_or(0..0, |sel| sel.cols_for_row(stable_row, self.rectangular));
                    // Constrain to the pane width!
                    let selrange = selrange.start..selrange.end.min(self.dims.cols);

                    let (cursor, composing, password_input) = if self.cursor.y == stable_row {
                        (
                            Some(CursorProperties {
                                position: StableCursorPosition {
                                    y: 0,
                                    ..*self.cursor
                                },
                                dead_key_or_leader: *self.term_window.terminal_dead_key_status()
                                    != DeadKeyStatus::None
                                    || self.term_window.leader_is_active(),
                                cursor_fg: self.cursor_fg,
                                cursor_bg: self.cursor_bg,
                                cursor_border_color: self.cursor_border_color,
                                cursor_is_default_color: self.cursor_is_default_color,
                            }),
                            match (
                                self.pos.is_active,
                                self.term_window.terminal_dead_key_status(),
                            ) {
                                (true, DeadKeyStatus::Composing(composing)) => {
                                    Some(composing.to_string())
                                }
                                _ => None,
                            },
                            if self.term_window.config.detect_password_input {
                                match self.pos.pane.get_metadata() {
                                    Value::Object(obj) => {
                                        match obj.get(&Value::String("password_input".to_string()))
                                        {
                                            Some(Value::Bool(b)) => *b,
                                            _ => false,
                                        }
                                    }
                                    _ => false,
                                }
                            } else {
                                false
                            },
                        )
                    } else {
                        (None, None, false)
                    };

                    let shape_hash = self.term_window.shape_hash_for_line(line);

                    let quad_key = LineQuadCacheKey {
                        pane_id: self.pane_id,
                        password_input,
                        pane_is_active: self.pos.is_active,
                        config_generation: self.term_window.config.generation(),
                        shape_generation: self.term_window.shape_generation,
                        quad_generation: self.term_window.quad_generation,
                        font_identity: self.font_identity,
                        composing: composing.clone(),
                        selection: selrange.clone(),
                        cursor,
                        shape_hash,
                        top_pixel_y: NotNan::new(self.top_pixel_y).unwrap()
                            + line_idx as f32 * self.render_metrics.cell_size.height as f32,
                        left_pixel_x: NotNan::new(self.left_pixel_x).unwrap(),
                        phys_line_idx: line_idx,
                        reverse_video: self.dims.reverse_video,
                    };

                    if let Some(cached_quad) =
                        self.term_window.line_quad_cache.borrow_mut().get(&quad_key)
                    {
                        let expired = cached_quad
                            .expires
                            .map(|i| Instant::now() >= i)
                            .unwrap_or(false);
                        let hover_changed = if cached_quad.invalidate_on_hover_change {
                            !same_hyperlink(
                                cached_quad.current_highlight.as_ref(),
                                self.term_window.current_highlight.as_ref(),
                            )
                        } else {
                            false
                        };
                        if !expired && !hover_changed {
                            cached_quad
                                .layers
                                .apply_to(self.layers)
                                .context("cached_quad.layers.apply_to")?;
                            self.term_window.update_next_frame_time(cached_quad.expires);
                            return Ok(());
                        }
                    }

                    let mut buf = HeapQuadAllocator::default();
                    let next_due = self.term_window.has_animation.borrow_mut().take();

                    let shape_key = LineToEleShapeCacheKey {
                        shape_hash,
                        shape_generation: quad_key.shape_generation,
                        font_identity: self.font_identity,
                        composing: if self.cursor.y == stable_row && self.pos.is_active {
                            if let DeadKeyStatus::Composing(composing) =
                                self.term_window.terminal_dead_key_status()
                            {
                                Some((self.cursor.x, composing.to_string()))
                            } else {
                                None
                            }
                        } else {
                            None
                        },
                    };

                    let render_result = self
                        .term_window
                        .render_screen_line(
                            RenderScreenLineParams {
                                top_pixel_y: *quad_key.top_pixel_y,
                                left_pixel_x: self.left_pixel_x,
                                pixel_width: self.dims.cols as f32
                                    * self.render_metrics.cell_size.width as f32,
                                stable_line_idx: Some(stable_row),
                                line: &line,
                                selection: selrange.clone(),
                                cursor: &self.cursor,
                                palette: &self.palette,
                                dims: &self.dims,
                                config: &self.term_window.config,
                                cursor_border_color: self.cursor_border_color,
                                foreground: self.foreground,
                                is_active: self.pos.is_active,
                                pane: Some(&self.pos.pane),
                                selection_fg: self.selection_fg,
                                selection_bg: self.selection_bg,
                                cursor_fg: self.cursor_fg,
                                cursor_bg: self.cursor_bg,
                                cursor_is_default_color: self.cursor_is_default_color,
                                white_space: self.white_space,
                                filled_box: self.filled_box,
                                window_is_transparent: self.window_is_transparent,
                                default_bg: self.default_bg,
                                font: None,
                                style: None,
                                use_pixel_positioning: self
                                    .term_window
                                    .config
                                    .experimental_pixel_positioning,
                                render_metrics: self.render_metrics,
                                font_config: self.font_config.clone(),
                                font_identity: self.font_identity,
                                shape_key: Some(shape_key),
                                password_input,
                            },
                            &mut TripleLayerQuadAllocator::Heap(&mut buf),
                        )
                        .context("render_screen_line")?;

                    let expires = self.term_window.has_animation.borrow().as_ref().cloned();
                    self.term_window.update_next_frame_time(next_due);

                    buf.apply_to(self.layers)
                        .context("HeapQuadAllocator::apply_to")?;

                    let quad_value = LineQuadCacheValue {
                        layers: buf,
                        expires,
                        invalidate_on_hover_change: render_result.invalidate_on_hover_change,
                        current_highlight: if render_result.invalidate_on_hover_change {
                            self.term_window.current_highlight.clone()
                        } else {
                            None
                        },
                    };

                    self.term_window
                        .line_quad_cache
                        .borrow_mut()
                        .put(quad_key, quad_value);

                    Ok(())
                }
            }

            impl<'a, 'b> WithPaneLines for LineRender<'a, 'b> {
                fn with_lines_mut(&mut self, stable_top: StableRowIndex, lines: &mut [&mut Line]) {
                    for (line_idx, line) in lines.iter().enumerate() {
                        if let Err(err) = self.render_line(stable_top, line_idx, line) {
                            self.error.replace(err);
                            return;
                        }
                    }
                }
            }

            pos.pane.with_lines_mut(stable_range.clone(), &mut render);
            if let Some(error) = render.error.take() {
                return Err(error).context("error while calling with_lines_mut");
            }
        }

        /*
        if let Some(zone) = zone {
            // TODO: render a thingy to jump to prior prompt
        }
        */
        metrics::histogram!("paint_pane.lines").record(start.elapsed());
        log::trace!("lines elapsed {:?}", start.elapsed());

        Ok(())
    }

    pub fn build_pane(&mut self, pos: &PositionedPane) -> anyhow::Result<ComputedElement> {
        // First compute the bounds for the pane background

        let cell_width = self.render_metrics.cell_size.width as f32;
        let cell_height = self.render_metrics.cell_size.height as f32;
        let (padding_left, padding_top) = self.padding_left_top();
        let tab_bar_height = if self.show_tab_bar {
            self.tab_bar_pixel_height()?
        } else {
            0.
        };
        let (top_bar_height, _bottom_bar_height) = if self.config.tab_bar_at_bottom {
            (0.0, tab_bar_height)
        } else {
            (tab_bar_height, 0.0)
        };

        let border = self.get_os_border();
        let top_pixel_y = top_bar_height + padding_top + border.top.get() as f32;

        // We want to fill out to the edges of the splits
        let (x, width_delta) = if pos.left == 0 {
            (
                0.,
                padding_left + border.left.get() as f32 + (cell_width / 2.0),
            )
        } else {
            (
                padding_left + border.left.get() as f32 - (cell_width / 2.0)
                    + (pos.left as f32 * cell_width),
                cell_width,
            )
        };

        let (y, height_delta) = if pos.top == 0 {
            (
                (top_pixel_y - padding_top),
                padding_top + (cell_height / 2.0),
            )
        } else {
            (
                top_pixel_y + (pos.top as f32 * cell_height) - (cell_height / 2.0),
                cell_height,
            )
        };

        let background_rect = euclid::rect(
            x,
            y,
            // Go all the way to the right edge if we're right-most
            if pos.left + pos.width >= self.terminal_size.cols as usize {
                self.dimensions.pixel_width as f32 - x
            } else {
                (pos.width as f32 * cell_width) + width_delta
            },
            // Go all the way to the bottom if we're bottom-most
            if pos.top + pos.height >= self.terminal_size.rows as usize {
                self.dimensions.pixel_height as f32 - y
            } else {
                (pos.height as f32 * cell_height) + height_delta as f32
            },
        );

        // Bounds for the terminal cells
        let content_rect = euclid::rect(
            padding_left + border.left.get() as f32 - (cell_width / 2.0)
                + (pos.left as f32 * cell_width),
            top_pixel_y + (pos.top as f32 * cell_height) - (cell_height / 2.0),
            pos.width as f32 * cell_width,
            pos.height as f32 * cell_height,
        );

        let palette = pos.pane.palette();

        // TODO: visual bell background layer
        // TODO: scrollbar

        Ok(ComputedElement {
            item_type: None,
            zindex: 0,
            bounds: background_rect,
            border: PixelDimension::default(),
            border_rect: background_rect,
            border_corners: None,
            colors: ElementColors {
                border: BorderColor::default(),
                bg: if self.window_background.is_empty() {
                    palette
                        .background
                        .to_linear()
                        .mul_alpha(self.config.window_background_opacity)
                        .into()
                } else {
                    InheritableColor::Inherited
                },
                text: InheritableColor::Inherited,
            },
            hover_colors: None,
            padding: background_rect,
            content_rect,
            baseline: 1.0,
            content: ComputedElementContent::Children(vec![]),
        })
    }
}
