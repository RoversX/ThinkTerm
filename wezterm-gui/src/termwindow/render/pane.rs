use crate::quad::{
    HeapQuadAllocator, QuadClipRect, QuadTrait, TripleLayerQuadAllocator,
    TripleLayerQuadAllocatorTrait,
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
    PANE_NAV_ACTION_ICON_SIZE, TAB_CLOSE_HOVER_INSET,
    TAB_CLOSE_HOVER_RADIUS, TAB_CONTENT_INSET, TAB_ICON_SIZE,
    TAB_VERTICAL_PADDING,
};
use crate::termwindow::{PaneNavAction, ScrollHit, ScrollTrack, UIItem, UIItemType};
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

fn clamp_pane_horizontal_span(
    pane_left: f32,
    candidate_right: f32,
    viewport_right: f32,
) -> (f32, f32) {
    let viewport_right = viewport_right.max(0.0);
    if viewport_right <= 0.0 {
        return (0.0, 0.0);
    }

    // Leave a one-pixel span even for corrupt/stale geometry whose left edge
    // is already beyond the viewport. Normal pane layouts never need this,
    // but it keeps every caller's rectangle valid without allowing it to
    // cross into the sidebar.
    let pane_left = pane_left.max(0.0).min((viewport_right - 1.0).max(0.0));
    let pane_right = candidate_right.max(pane_left + 1.0).min(viewport_right);
    (pane_left, (pane_right - pane_left).max(0.0))
}

fn visible_render_columns(
    requested_cols: usize,
    content_left: f32,
    clip_right: f32,
    cell_width: f32,
) -> usize {
    if requested_cols == 0 || cell_width <= 0.0 {
        return 0;
    }
    let visible_width = (clip_right - content_left).max(0.0);
    requested_cols.min((visible_width / cell_width).floor().max(0.0) as usize)
}

#[cfg(test)]
mod pane_geometry_tests {
    use super::{clamp_pane_horizontal_span, visible_render_columns};

    #[test]
    fn stale_pane_geometry_stops_at_the_sidebar_edge() {
        let (left, width) = clamp_pane_horizontal_span(354.0, 1_210.0, 802.0);
        assert_eq!(left, 354.0);
        assert_eq!(left + width, 802.0);
    }

    #[test]
    fn valid_split_spans_still_meet_without_a_gap() {
        let (left_x, left_width) = clamp_pane_horizontal_span(0.0, 354.0, 802.0);
        let (right_x, right_width) = clamp_pane_horizontal_span(354.0, 802.0, 802.0);
        assert_eq!(left_x + left_width, right_x);
        assert_eq!(right_x + right_width, 802.0);
    }

    #[test]
    fn a_pane_starting_beyond_the_viewport_cannot_escape_it() {
        let (left, width) = clamp_pane_horizontal_span(900.0, 1_210.0, 802.0);
        assert!(left >= 0.0);
        assert_eq!(left + width, 802.0);
    }

    #[test]
    fn terminal_columns_are_clipped_to_the_current_pane_while_geometry_is_repaired() {
        assert_eq!(visible_render_columns(100, 354.0, 802.0, 10.0), 44);
        assert_eq!(visible_render_columns(30, 354.0, 802.0, 10.0), 30);
        assert_eq!(visible_render_columns(100, 900.0, 802.0, 10.0), 0);
    }
}

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

/// Inset from the collapsed strip's own edges to its content.
const COLLAPSED_EDGE_PADDING: usize = 8;
/// Gap the collapsed strip leaves between its tabs and its action button.
const COLLAPSED_SECTION_GAP: usize = 10;

/// Most the line quad scratch recorder may keep between lines. A line of
/// ordinary width needs a few tens of KiB across its three layers; one
/// exceptionally wide line grows the buffers past this and they are
/// released rather than pinned for the window's life.
const LINE_QUAD_SCRATCH_MAX_BYTES: usize = 512 * 1024;

impl crate::TermWindow {
    /// Height of the nav bar for this pane: the metric height, clamped so
    /// that at least one terminal row of the pane's cell remains visible.
    pub(crate) fn pane_nav_bar_height_for_pane(&self, pos: &PositionedPane) -> usize {
        self.pane_nav_bar_height().min(
            pos.pixel_height
                .saturating_sub(self.render_metrics.cell_size.height.max(1) as usize),
        )
    }

    /// Icon, action-icon and button sizes for a nav bar of `nav_height`.
    /// The painter and the scroll clamp both size their geometry from these, so
    /// they are derived once.
    fn pane_nav_button_metrics(&self, nav_height: usize) -> (usize, usize, usize) {
        // Fixed chrome sizes, clamped only so a nav bar squeezed by a short
        // pane cannot overflow its own row. The tab icon deliberately matches
        // the window tab row's; it used to be capped at 24 here and computed
        // from the font up there, which left the two rows visibly out of step.
        let icon_size = self.ui_px(TAB_ICON_SIZE).min(nav_height);
        let action_icon_size = self.ui_px(PANE_NAV_ACTION_ICON_SIZE).min(nav_height);
        // Floored by both icons, as the window tab row is: a pane short enough
        // to squeeze the row would otherwise leave the pill shorter than the
        // icon inside it, which then top-aligns and gets its base clipped away.
        let button_size = nav_height
            .saturating_sub(self.ui_px(TAB_VERTICAL_PADDING) * 2)
            .max(action_icon_size)
            .max(icon_size);
        (icon_size, action_icon_size, button_size)
    }

    /// Width the trailing action buttons claim from the nav bar's right edge:
    /// new tab, split down, split right and zoom, plus collapse wherever the
    /// split shape allows it.
    fn pane_nav_action_reserved_width(&self, pos: &PositionedPane, nav_height: usize) -> usize {
        let (_, _, button_size) = self.pane_nav_button_metrics(nav_height);
        let action_count = 4 + usize::from(self.can_collapse_pane_stack(pos));
        action_count
            .saturating_mul(button_size + self.ui_px(PANE_NAV_BUTTON_GAP))
            .saturating_add(self.ui_px(PANE_NAV_INSET))
    }

    /// Horizontal room the *collapsed* strip leaves for its tabs.
    ///
    /// The expanded bar's `pane_nav_tab_viewport_width` does not describe this
    /// one: the collapsed strip insets by its own padding and reserves a single
    /// Expand button where the expanded bar reserves four or five. Clamping the
    /// wheel against the expanded figure left roughly a button-row of dead
    /// scroll the strip never moved through.
    pub(crate) fn collapsed_pane_nav_tab_viewport_width(&self, pos: &PositionedPane) -> f32 {
        let Some(layout) = self.collapsed_pane_layouts.get(&pos.pane_stack_id).copied() else {
            return 0.0;
        };
        if layout.split_direction == SplitDirection::Horizontal {
            // A vertical strip stacks its chips; there is nothing to scroll.
            return 0.0;
        }
        let Ok(pane_rect) = self.pane_frame_rect(pos) else {
            return 0.0;
        };
        let strip_left = pane_rect.origin.x.max(0.0) as usize;
        let strip_right = pane_rect.max_x().max(0.0) as usize;
        let strip_height = pane_rect.size.height.max(1.0) as usize;
        let chrome_height = self.pane_nav_bar_height().min(strip_height);
        let (_, _, button_size) = self.pane_nav_button_metrics(chrome_height);
        let tab_start = strip_left + COLLAPSED_EDGE_PADDING;
        let max_tab_right = strip_right
            .saturating_sub(COLLAPSED_EDGE_PADDING + button_size + COLLAPSED_SECTION_GAP);
        max_tab_right.saturating_sub(tab_start) as f32
    }

    /// Horizontal room the nav bar leaves for its tab strip. Tab width is
    /// derived from this, so the painter and the wheel-scroll clamp have to read
    /// it from the same place or the last tab drifts out of reach.
    pub(crate) fn pane_nav_tab_viewport_width(&self, pos: &PositionedPane) -> f32 {
        let nav_height = self.pane_nav_bar_height_for_pane(pos);
        if nav_height == 0 {
            return 0.0;
        }
        let Ok((pane_x, pane_width)) = self.pane_chrome_span(pos) else {
            return 0.0;
        };
        let tab_start = pane_x.max(0.0) as usize + self.pane_nav_tab_left_inset(pos.left);
        let max_tab_right = ((pane_x + pane_width).max(0.0) as usize)
            .saturating_sub(self.pane_nav_action_reserved_width(pos, nav_height));
        max_tab_right.saturating_sub(tab_start) as f32
    }

    pub(crate) fn terminal_viewport_right(&self) -> f32 {
        let border = self.get_os_border();
        self.dimensions
            .pixel_width
            .saturating_sub(border.right.get() as usize)
            .saturating_sub(self.right_sidebar_width()) as f32
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

    /// Horizontal span (x, width) of a pane's chrome: the cell-aligned
    /// content only covers `cols * cell_width`, so extend outward to the
    /// window/sidebar edges (absorbing padding and the column remainder)
    /// and half-way across split dividers, so that adjacent pane nav bars
    /// tile the full window width without background-colored gaps.
    pub(crate) fn pane_chrome_span(&self, pos: &PositionedPane) -> anyhow::Result<(f32, f32)> {
        let (content_pane_x, _) = self.pane_content_origin(pos)?;
        let cell_w = self.render_metrics.cell_size.width as f32;
        let content_pane_right = content_pane_x + pos.width as f32 * cell_w;
        let pane_x = if pos.left == 0 {
            self.tab_bar_left_edge() as f32
        } else {
            content_pane_x - cell_w / 2.0
        };
        let is_rightmost = pos.left + pos.width >= self.terminal_size.cols;
        let candidate_right = if is_rightmost {
            self.terminal_viewport_right()
        } else {
            content_pane_right + cell_w / 2.0
        };
        Ok(clamp_pane_horizontal_span(
            pane_x,
            candidate_right,
            self.terminal_viewport_right(),
        ))
    }

    pub(crate) fn pane_frame_rect(&self, pos: &PositionedPane) -> anyhow::Result<RectF> {
        let (_, pane_y) = self.pane_content_origin(pos)?;
        let (pane_x, pane_width) = self.pane_chrome_span(pos)?;
        let mut height = (pos.height as f32 * self.render_metrics.cell_size.height as f32).max(1.0);
        if self.collapsed_pane_layouts.contains_key(&pos.pane_stack_id) && pos.top > 0 {
            height = height.max(self.pane_nav_bar_height() as f32);
        }

        Ok(euclid::rect(pane_x, pane_y, pane_width, height))
    }

    fn paint_collapsed_pane_nav_bar(
        &mut self,
        pos: &PositionedPane,
        layers: &mut TripleLayerQuadAllocator,
        layout: CollapsedPaneLayout,
    ) -> anyhow::Result<usize> {
        let pane_rect = self.pane_frame_rect(pos)?;
        let chrome = self.chrome();
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

        const COLLAPSED_BUTTON_GAP: usize = 6;

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
        let (icon_size, action_icon_size, button_size) =
            self.pane_nav_button_metrics(chrome_height);
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
        let tab_width = self.collapsed_pane_tab_width_pixels().ceil() as usize;
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
                chrome.active_tab_surface()
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

            // Reborrow: the surrounding loop cannot move the caller's `&mut`.
            let out_layers = &mut *layers;
            // Content rides in a heap and is replayed clipped to the strip's
            // viewport, recorded in the tab's own space -- left edge at 0 --
            // and shifted onto the strip on replay. See `paint_window_tab`.
            let mut content = HeapQuadAllocator::default();
            let mut content_layers = TripleLayerQuadAllocator::Heap(&mut content);
            let layers = &mut content_layers;

            self.paint_tab_capsule(
                layers,
                1,
                tab_y,
                tab_width,
                tab_height,
                self.ui_f32(PANE_NAV_TAB_RADIUS),
                tab_surface_color,
                tab_border_color,
                selected_tab,
            )
            .context("pane nav tab surface")?;

            let title_icon_x = self.ui_px(TAB_CONTENT_INSET);
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

            let close_slot_reserved = !is_renaming_tab;
            // The button slides to stay inside the tab's visible sliver, so a
            // half-scrolled tab keeps a usable close target instead of letting
            // it scroll off with the rest. The clamp is in the tab's own space
            // and derived from its *true* origin: it used to be computed from
            // an origin clamped to 0, which is what put the button on a
            // neighbouring tab.
            let view_left = (tab_start as f32 - tab_left).max(0.0);
            let view_right = (max_tab_right as f32 - tab_left).min(tab_width as f32);
            let close_x = (self.tab_close_button_x(tab_width, button_size, icon_size) as f32)
                .min(view_right - button_size as f32)
                .max(view_left)
                .max(0.0) as usize;
            let close_screen_x = tab_left + close_x as f32;
            let close_hit_visible = view_right - view_left >= button_size as f32;
            let close_hit_x = close_screen_x.max(0.0) as usize;
            let show_close = close_slot_reserved && (selected_tab || is_hovered);
            if show_close {
                // Hover feedback follows the hit target, or a half-clipped
                // button lights up and then passes the click through.
                let close_hovered = close_hit_visible
                    && self.is_pointer_over_ui_rect(close_hit_x, tab_y, button_size, button_size);
                if close_hovered {
                    let hover_alpha = if self.is_pointer_pressing_ui_rect(
                        close_hit_x,
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
                if close_hit_visible {
                    self.ui_items.push(UIItem {
                        x: close_hit_x,
                        y: tab_y,
                        width: button_size,
                        height: button_size,
                        item_type: UIItemType::PaneNav {
                            pane_id: pos.pane.pane_id(),
                            pane_index: pos.index,
                            action: PaneNavAction::Close(tab.pane_id),
                        },
                    });
                }
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
                // The tab's own close slot, not the slid-out position. Letting
                // the title chase the slide collapsed `text_right` below
                // `text_x` on a narrow sliver and dropped the title whole.
                self.tab_close_button_x(tab_width, button_size, icon_size)
            } else {
                tab_width.saturating_sub(self.ui_px(TAB_CONTENT_INSET))
            };
            let text_width = text_right.saturating_sub(text_x + self.ui_px(PANE_NAV_ICON_GAP));
            let text_y =
                tab_y + ((tab_height.saturating_sub(ui_metrics.cell_size.height as usize)) / 2);
            if text_width > 0 {
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

            drop(content_layers);
            content
                .apply_to_clipped_at(
                    out_layers,
                    tab_left,
                    0.0,
                    QuadClipRect::from_top_left_pixels(
                        tab_start as f32,
                        tab_y as f32,
                        max_tab_right as f32,
                        (tab_y + tab_height) as f32,
                        &self.dimensions,
                    ),
                    1.0,
                )
                .context("collapsed pane nav tab content")?;
        }

        // Dissolve the strip into the bar wherever it runs on past its
        // viewport, so a clipped tab reads as continuing rather than as one
        // sliced in half.
        self.paint_tab_row_fades(
            layers,
            chrome.sidebar_bg,
            tab_y,
            tab_height,
            tab_start,
            max_tab_right,
            scroll_offset > 0.5,
            scroll_offset < max_scroll - 0.5,
        )?;

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

        let (_, pane_y) = self.pane_content_origin(pos)?;
        if pos.width == 0 {
            return Ok(0);
        }
        let (pane_x, pane_width) = self.pane_chrome_span(pos)?;

        let chrome = self.chrome();
        // Between the tab strip and the terminal, so it belongs to the
        // terminal's header rather than to the sidebar -- same reasoning as
        // the tab strip itself. `header_bg` is the chrome's own colour in a
        // dark interface, so nothing moves there.
        let background = chrome.header_bg;
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

        let (icon_size, action_icon_size, button_size) = self.pane_nav_button_metrics(nav_height);
        // Centred, like the collapsed strip and the window tab row above. It
        // used to carry a 4px downward nudge, which left 12px of air over the
        // capsule and 4 under it — the row read as sagging.
        let button_y = pane_y as usize + nav_height.saturating_sub(button_size) / 2;
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
        let tab_gap = self.ui_px(PANE_NAV_TAB_GAP);
        let viewport_width = self.pane_nav_tab_viewport_width(pos) as usize;
        let max_tab_right = tab_start.saturating_add(viewport_width);
        let tab_width = self
            .adaptive_tab_width(tabs.len(), viewport_width as f32, tab_gap as f32)
            .ceil() as usize;
        let tab_step = tab_width + tab_gap;
        let total_tab_width = tabs
            .len()
            .saturating_mul(tab_width)
            .saturating_add(tabs.len().saturating_sub(1).saturating_mul(tab_gap));
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
                chrome.active_tab_surface()
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

            // Reborrow: the surrounding loop cannot move the caller's `&mut`.
            let out_layers = &mut *layers;
            // Content rides in a heap and is replayed clipped to the strip's
            // viewport, recorded in the tab's own space -- left edge at 0 --
            // and shifted onto the strip on replay. See `paint_window_tab`.
            let mut content = HeapQuadAllocator::default();
            let mut content_layers = TripleLayerQuadAllocator::Heap(&mut content);
            let layers = &mut content_layers;

            self.paint_tab_capsule(
                layers,
                1,
                tab_y,
                tab_width,
                tab_height,
                self.ui_f32(PANE_NAV_TAB_RADIUS),
                tab_surface_color,
                tab_border_color,
                selected_tab,
            )
            .context("pane nav tab surface")?;

            let title_icon_x = self.ui_px(TAB_CONTENT_INSET);
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

            let close_slot_reserved = !is_renaming_tab;
            // The button slides to stay inside the tab's visible sliver, so a
            // half-scrolled tab keeps a usable close target instead of letting
            // it scroll off with the rest. The clamp is in the tab's own space
            // and derived from its *true* origin: it used to be computed from
            // an origin clamped to 0, which is what put the button on a
            // neighbouring tab.
            let view_left = (tab_start as f32 - tab_left).max(0.0);
            let view_right = (max_tab_right as f32 - tab_left).min(tab_width as f32);
            let close_x = (self.tab_close_button_x(tab_width, button_size, icon_size) as f32)
                .min(view_right - button_size as f32)
                .max(view_left)
                .max(0.0) as usize;
            let close_screen_x = tab_left + close_x as f32;
            let close_hit_visible = view_right - view_left >= button_size as f32;
            let close_hit_x = close_screen_x.max(0.0) as usize;
            let show_close = close_slot_reserved && (selected_tab || is_hovered);
            if show_close {
                // Hover feedback follows the hit target, or a half-clipped
                // button lights up and then passes the click through.
                let close_hovered = close_hit_visible
                    && self.is_pointer_over_ui_rect(close_hit_x, tab_y, button_size, button_size);
                if close_hovered {
                    let hover_alpha = if self.is_pointer_pressing_ui_rect(
                        close_hit_x,
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
                if close_hit_visible {
                    self.ui_items.push(UIItem {
                        x: close_hit_x,
                        y: tab_y,
                        width: button_size,
                        height: button_size,
                        item_type: UIItemType::PaneNav {
                            pane_id: pos.pane.pane_id(),
                            pane_index: pos.index,
                            action: PaneNavAction::Close(tab.pane_id),
                        },
                    });
                }
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
                // The tab's own close slot, not the slid-out position. Letting
                // the title chase the slide collapsed `text_right` below
                // `text_x` on a narrow sliver and dropped the title whole.
                self.tab_close_button_x(tab_width, button_size, icon_size)
            } else {
                tab_width.saturating_sub(self.ui_px(TAB_CONTENT_INSET))
            };
            let text_width = text_right.saturating_sub(text_x + self.ui_px(PANE_NAV_ICON_GAP));
            let text_y =
                tab_y + ((tab_height.saturating_sub(ui_metrics.cell_size.height as usize)) / 2);
            if text_width > 0 {
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

            drop(content_layers);
            content
                .apply_to_clipped_at(
                    out_layers,
                    tab_left,
                    0.0,
                    QuadClipRect::from_top_left_pixels(
                        tab_start as f32,
                        tab_y as f32,
                        max_tab_right as f32,
                        (tab_y + tab_height) as f32,
                        &self.dimensions,
                    ),
                    1.0,
                )
                .context("pane nav tab content")?;
        }

        // Dissolve the strip into the bar wherever it runs on past its
        // viewport, so a clipped tab reads as continuing rather than as one
        // sliced in half.
        self.paint_tab_row_fades(
            layers,
            background,
            pane_y.max(0.0) as usize,
            nav_height,
            tab_start,
            max_tab_right,
            scroll_offset > 0.5,
            scroll_offset < max_scroll - 0.5,
        )?;

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
            let chrome = self.chrome();
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

    /// The thin, auto-hiding scrollbar over the right edge of a pane's
    /// content: a rounded thumb the height of the visible share of the
    /// scrollback, positioned by the pixel, shown while the view moves and
    /// fading out afterwards. Nothing is reserved for it in the layout.
    /// The hit area is wider than the thumb so a 5px line can be grabbed.
    fn paint_overlay_scrollbar(
        &mut self,
        layers: &mut TripleLayerQuadAllocator,
        pos: &PositionedPane,
        current_viewport: Option<StableRowIndex>,
        viewport_frac: f32,
        content_right: f32,
        content_top: f32,
        content_bottom: f32,
    ) -> anyhow::Result<()> {
        let pane_id = pos.pane.pane_id();
        let dims = pos.pane.get_dimensions();
        // Nothing to indicate while the whole history fits on screen.
        if dims.scrollback_rows <= dims.viewport_rows {
            return Ok(());
        }
        let now = Instant::now();
        let (opacity, next_frame) = self.overlay_scrollbar_opacity(pane_id, now);
        if let Some(due) = next_frame {
            self.update_next_frame_time(Some(due));
        }
        if opacity <= 0.0 {
            return Ok(());
        }

        let tokens = crate::ui::UiTokens::for_dpi(self.dimensions.dpi);
        let ui_palette =
            self.chrome();
        let track_top = content_top + tokens.scrollbar_margin_y;
        let track_height =
            (content_bottom - content_top - 2.0 * tokens.scrollbar_margin_y).max(0.0);
        if track_height < tokens.scrollbar_min_thumb {
            return Ok(());
        }
        let thumb = ScrollHit::thumb_px(
            &*pos.pane,
            current_viewport,
            viewport_frac,
            track_height as usize,
            tokens.scrollbar_min_thumb as usize,
        );
        // Under the pointer the thumb grows leftwards from its resting
        // line, like the macOS overlay scroller, so it is easier to grab.
        let hovered = self.overlay_scrollbar_hovered(pane_id);
        let (expand, next_frame) = self.overlay_scrollbar_expand(pane_id, hovered, now);
        if let Some(due) = next_frame {
            self.update_next_frame_time(Some(due));
        }
        let expand = crate::ui::anim::Easing::Smooth.apply(expand);
        let thumb_width = tokens.scrollbar_width
            * (1.0 + crate::termwindow::OVERLAY_SCROLLBAR_HOVER_GROWTH * expand);
        let thumb_x = content_right - tokens.scrollbar_inset - thumb_width;
        let thumb_top = track_top + thumb.top as f32;

        // Hit areas: a strip three thumbs wide, so the pointer need not
        // land on the line itself. Pushed after the pane's own items, so
        // they win the reverse walk in resolve_ui_item.
        let hit_width = (tokens.scrollbar_width * 3.0).max(12.0);
        let hit_x = (content_right - tokens.scrollbar_inset - hit_width).max(0.0) as usize;
        let track = ScrollTrack {
            pane_id,
            track_top: track_top as usize,
            track_height: track_height as usize,
        };
        self.ui_items.push(UIItem {
            x: hit_x,
            width: hit_width as usize,
            y: track_top as usize,
            height: thumb.top,
            item_type: UIItemType::AboveScrollThumb(track),
        });
        self.ui_items.push(UIItem {
            x: hit_x,
            width: hit_width as usize,
            y: thumb_top as usize,
            height: thumb.height,
            item_type: UIItemType::ScrollThumb(track),
        });
        self.ui_items.push(UIItem {
            x: hit_x,
            width: hit_width as usize,
            y: thumb_top as usize + thumb.height,
            height: (track_height as usize).saturating_sub(thumb.top + thumb.height),
            item_type: UIItemType::BelowScrollThumb(track),
        });

        let gl_state = self.render_state.as_ref().unwrap();
        let ctx = crate::ui::DrawContext::new(gl_state, self.dimensions, &self.render_metrics);
        ctx.draw_rounded_rect(
            layers,
            2,
            thumb_x,
            thumb_top,
            thumb_width,
            thumb.height as f32,
            // A little more solid under the pointer, too.
            ui_palette
                .scrollbar_thumb
                .mul_alpha(opacity * (1.0 + 0.5 * expand)),
            thumb_width / 2.0,
        )
        .context("overlay scrollbar thumb")
    }

    fn paint_pane_box_model(&mut self, pos: &PositionedPane) -> anyhow::Result<()> {
        let pane_id = pos.pane.pane_id();
        let output_generation = self
            .track_pane_output_generations_for_frame
            .then(|| mux::Mux::get().pane_output_generation(pane_id));
        let computed = self.build_pane(pos)?;
        let mut ui_items = computed.ui_items();
        self.ui_items.append(&mut ui_items);
        let gl_state = self.render_state.as_ref().unwrap();
        self.render_element(&computed, gl_state, None)?;
        if let Some(generation) = output_generation {
            self.frame_pane_output_generations
                .insert(pane_id, generation);
        }
        Ok(())
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
        // A wheel notch in smooth mode is paid out over frames; this frame's
        // slice moves the viewport before it is read below.
        self.advance_scroll_glide(&pos.pane);
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
        let content_left = padding_left
            + border.left.get() as f32
            + pos.left as f32 * global_render_metrics.cell_size.width as f32;
        let pane_target_size = self.terminal_size_for_positioned_pane(pos, pane_render_metrics);
        let pane_content_right =
            (content_left + pos.pixel_width as f32).min(self.terminal_viewport_right());
        let visible_cols = visible_render_columns(
            dims.cols,
            content_left,
            pane_content_right,
            pane_render_metrics.cell_size.width.max(1) as f32,
        );
        let mut render_dims = dims;
        render_dims.cols = visible_cols.min(pane_target_size.cols);
        render_dims.viewport_rows = render_dims.viewport_rows.min(pane_target_size.rows);
        render_dims.pixel_width = render_dims
            .cols
            .saturating_mul(pane_render_metrics.cell_size.width.max(1) as usize);
        render_dims.pixel_height = render_dims
            .viewport_rows
            .saturating_mul(pane_render_metrics.cell_size.height.max(1) as usize);

        // A saved viewport can still be overtaken (scrollback trimming
        // passed it, the app erased its scrollback, or a resize renumbered
        // rows faster than the rewrap anchoring compensates).
        // stable_range's fallback would then silently draw physical row 0 —
        // the notorious "terminal jumped to the top". Degrade by one small
        // step instead, routed through set_viewport so the correction gets
        // the same normalization as a user scroll (in particular the
        // conversion to follow-bottom when no scrollback remains) and the
        // copy/quickselect overlays hear about it.
        let current_viewport =
            match Self::normalize_stale_viewport(current_viewport, &dims) {
                Some(fixed) => {
                    self.set_viewport(pane_id, fixed, dims);
                    fixed
                }
                None => current_viewport,
            };

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
            self.chrome().sidebar_bg
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
            let candidate_right = if pos.left + pos.width >= self.terminal_size.cols as usize {
                self.terminal_viewport_right()
            } else {
                x + (pos.width as f32 * cell_width) + width_delta
            };
            let (x, width) =
                clamp_pane_horizontal_span(x, candidate_right, self.terminal_viewport_right());
            euclid::rect(
                x,
                y,
                width,
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

        // The Lua `enable_scroll_bar` gutter: one window-wide bar for the
        // active pane, kept as it was. The per-pane overlay scrollbar is
        // drawn after the lines, at the end of this function.
        if pos.is_active && self.show_scroll_bar {
            let thumb_y_offset = top_bar_height as usize + border.top.get();

            let min_height = self.min_scroll_bar_height();

            let track_height = self.dimensions.pixel_height.saturating_sub(
                thumb_y_offset + border.bottom.get() + bottom_bar_height as usize,
            );
            let info = ScrollHit::thumb(
                &*pos.pane,
                current_viewport,
                track_height,
                min_height as usize,
            );
            let abs_thumb_top = thumb_y_offset + info.top;
            let thumb_size = info.height;
            let color = palette.scrollbar_thumb.to_linear();
            let track = ScrollTrack {
                pane_id: pos.pane.pane_id(),
                track_top: thumb_y_offset,
                track_height,
            };

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
                item_type: UIItemType::AboveScrollThumb(track),
            });
            self.ui_items.push(UIItem {
                x: thumb_x,
                width: padding as usize,
                y: abs_thumb_top,
                height: thumb_size,
                item_type: UIItemType::ScrollThumb(track),
            });
            self.ui_items.push(UIItem {
                x: thumb_x,
                width: padding as usize,
                y: abs_thumb_top + thumb_size,
                height: self
                    .dimensions
                    .pixel_height
                    .saturating_sub(abs_thumb_top + thumb_size),
                item_type: UIItemType::BelowScrollThumb(track),
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
            // A smooth scroll cuts the top row off by `scroll_px`, which
            // uncovers that much of one more row at the bottom; it is drawn
            // too, and the two edge rows are cropped to the pane. The clamp
            // in set_viewport_px keeps the extra row inside the terminal.
            let scroll_px = match current_viewport {
                Some(_) => self.get_viewport_px(pane_id),
                None => 0.0,
            };
            let extra_row = if scroll_px > 0.0 { 1 } else { 0 };
            let stable_range = match current_viewport {
                Some(top) => top..top + (render_dims.viewport_rows + extra_row) as StableRowIndex,
                None => {
                    dims.physical_top
                        ..dims.physical_top + render_dims.viewport_rows as StableRowIndex
                }
            };
            let row_count = (stable_range.end - stable_range.start).max(0) as usize;

            pos.pane
                .apply_hyperlinks(stable_range.clone(), &self.config.hyperlink_rules);

            /// Copy one line's recorded quads into the frame, shifted up by
            /// the smooth-scroll remainder. Only the rows at the pane's top
            /// and bottom can poke out of it, so only those pay for
            /// cropping; the rest are moved as they are. With no remainder
            /// this is the plain replay it always was.
            fn replay_line(
                layers: &mut dyn TripleLayerQuadAllocatorTrait,
                heap: &HeapQuadAllocator,
                scroll_px: f32,
                pane_clip: QuadClipRect,
                line_idx: usize,
                row_count: usize,
            ) -> anyhow::Result<()> {
                if scroll_px == 0.0 {
                    return heap.apply_to(layers);
                }
                let edge = line_idx == 0 || line_idx + 1 >= row_count;
                if edge {
                    heap.apply_to_clipped_at(layers, 0.0, -scroll_px, pane_clip, 1.0)
                } else {
                    heap.apply_to_at(layers, 0.0, -scroll_px)
                }
            }

            struct LineRender<'a, 'b> {
                scroll_px: f32,
                pane_clip: QuadClipRect,
                row_count: usize,
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

            let left_pixel_x = content_left;
            let pane_top_pixel_y = top_pixel_y
                + (pos.top as f32 * global_render_metrics.cell_size.height as f32)
                + pane_nav_height as f32;

            if pos.is_active {
                self.update_text_cursor(
                    &cursor,
                    stable_range.start,
                    &render_dims,
                    left_pixel_x,
                    // The rows are drawn shifted up by the smooth-scroll
                    // remainder; the IME popup follows the drawn cursor.
                    pane_top_pixel_y - scroll_px,
                    pane_render_metrics.cell_size,
                );
            }

            let pane_clip = QuadClipRect::from_top_left_pixels(
                left_pixel_x,
                pane_top_pixel_y,
                left_pixel_x
                    + render_dims.cols as f32 * pane_render_metrics.cell_size.width as f32,
                pane_top_pixel_y
                    + render_dims.viewport_rows as f32
                        * pane_render_metrics.cell_size.height as f32,
                &self.dimensions,
            );
            let mut render = LineRender {
                scroll_px,
                pane_clip,
                row_count,
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
                        render_cols: self.dims.cols,
                        render_pixel_width: self.dims.pixel_width,
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
                            replay_line(
                                self.layers,
                                &cached_quad.layers,
                                self.scroll_px,
                                self.pane_clip,
                                line_idx,
                                self.row_count,
                            )
                            .context("cached_quad.layers.apply_to")?;
                            self.term_window.update_next_frame_time(cached_quad.expires);
                            return Ok(());
                        }
                    }

                    // Recorded into the window's one scratch recorder and
                    // moved out at exact size below, rather than growing a
                    // fresh allocator from zero on every miss. Recycled on
                    // entry as well as on exit, so an early `?` return
                    // leaves nothing stale for the next line.
                    // No painter re-enters this while a line is being
                    // recorded (the only other borrowers run outside a
                    // paint); should one ever do so, that line falls back to
                    // a private allocator rather than panicking mid-paint,
                    // which costs the old doubling churn for that line only.
                    let mut scratch_guard = self.term_window.line_quad_scratch.try_borrow_mut();
                    let mut fallback = HeapQuadAllocator::default();
                    let buf: &mut HeapQuadAllocator = match scratch_guard.as_mut() {
                        Ok(scratch) => {
                            scratch.recycle();
                            &mut **scratch
                        }
                        Err(_) => &mut fallback,
                    };
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

                    self.term_window.dedicated_image_in_line.set(false);
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
                                allow_images: true,
                                simple_shaping: false,
                            },
                            &mut TripleLayerQuadAllocator::Heap(buf),
                        )
                        .context("render_screen_line")?;

                    let expires = self.term_window.has_animation.borrow().as_ref().cloned();
                    self.term_window.update_next_frame_time(next_due);

                    replay_line(
                        self.layers,
                        buf,
                        self.scroll_px,
                        self.pane_clip,
                        line_idx,
                        self.row_count,
                    )
                    .context("HeapQuadAllocator::apply_to")?;

                    // A line that drew a picture from a dedicated texture
                    // emitted composites as a side effect; replaying its
                    // cached heap would repaint the line without the picture.
                    if !self.term_window.dedicated_image_in_line.replace(false) {
                        // Moved out at exactly its length, so the weight
                        // computed from `resident_bytes()` (which counts
                        // capacity) is what the entry really holds, and the
                        // scratch keeps its buffers for the next line.
                        let quad_value = LineQuadCacheValue {
                            layers: buf.take_exact(),
                            expires,
                            invalidate_on_hover_change: render_result.invalidate_on_hover_change,
                            current_highlight: if render_result.invalidate_on_hover_change {
                                self.term_window.current_highlight.clone()
                            } else {
                                None
                            },
                        };
                        let weight = crate::termwindow::render::estimate_line_quad_entry_bytes(
                            &quad_key,
                            &quad_value,
                        );
                        self.term_window
                            .line_quad_cache
                            .borrow_mut()
                            .put_weighted(quad_key, quad_value, weight);
                    } else {
                        // Not cached: leave the scratch empty rather than
                        // holding this line's quads until the next miss.
                        buf.recycle();
                    }
                    // The scratch keeps its buffers between lines on purpose,
                    // but one unusually wide line must not pin them for the
                    // life of the window (only macOS and Windows report the
                    // occlusion that otherwise releases it).
                    if buf.resident_bytes() > LINE_QUAD_SCRATCH_MAX_BYTES {
                        *buf = HeapQuadAllocator::default();
                    }

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

            let output_generation = render
                .term_window
                .track_pane_output_generations_for_frame
                .then(|| mux::Mux::get().pane_output_generation(pane_id));
            // Painters that bypass the line layers (dedicated image
            // composites) read the same shift from here for the duration.
            render.term_window.line_render_y_offset.set(-scroll_px);
            pos.pane.with_lines_mut(stable_range.clone(), &mut render);
            render.term_window.line_render_y_offset.set(0.0);
            if let Some(error) = render.error.take() {
                return Err(error).context("error while calling with_lines_mut");
            }

            // The overlay scrollbar goes on after the text it sits over.
            if !render.term_window.show_scroll_bar && crate::native_settings::overlay_scrollbar() {
                let content_bottom = pane_top_pixel_y
                    + render_dims.viewport_rows as f32
                        * pane_render_metrics.cell_size.height as f32;
                render.term_window.paint_overlay_scrollbar(
                    render.layers,
                    pos,
                    current_viewport,
                    scroll_px / pane_render_metrics.cell_size.height.max(1) as f32,
                    pane_content_right,
                    pane_top_pixel_y,
                    content_bottom,
                )?;
            }
            if let Some(generation) = output_generation {
                render
                    .term_window
                    .frame_pane_output_generations
                    .insert(pane_id, generation);
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

        let candidate_background_right = if pos.left + pos.width >= self.terminal_size.cols as usize
        {
            self.terminal_viewport_right()
        } else {
            x + (pos.width as f32 * cell_width) + width_delta
        };
        let (background_x, background_width) = clamp_pane_horizontal_span(
            x,
            candidate_background_right,
            self.terminal_viewport_right(),
        );
        let background_rect = euclid::rect(
            background_x,
            y,
            background_width,
            // Go all the way to the bottom if we're bottom-most
            if pos.top + pos.height >= self.terminal_size.rows as usize {
                self.dimensions.pixel_height as f32 - y
            } else {
                (pos.height as f32 * cell_height) + height_delta as f32
            },
        );

        // Bounds for the terminal cells
        let content_x = padding_left + border.left.get() as f32 - (cell_width / 2.0)
            + (pos.left as f32 * cell_width);
        let (content_x, content_width) = clamp_pane_horizontal_span(
            content_x,
            content_x + pos.width as f32 * cell_width,
            self.terminal_viewport_right(),
        );
        let content_rect = euclid::rect(
            content_x,
            top_pixel_y + (pos.top as f32 * cell_height) - (cell_height / 2.0),
            content_width,
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
