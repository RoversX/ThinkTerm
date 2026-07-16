use crate::customglyph::BlockKey;
use crate::quad::{QuadTrait, TripleLayerQuadAllocator, TripleLayerQuadAllocatorTrait};
use crate::shapecache::{BorrowedShapeCacheKey, ShapedInfo};
use crate::tabbar::{TabBarItem, TabEntry};
use crate::termwindow::ui::icons::SvgIcon;
use crate::termwindow::ui::tokens::{
    CAPSULE_BORDER_WIDTH, ICON_BUTTON_BORDER_WIDTH, SIDEBAR_INSET, TAB_CLOSE_HOVER_INSET,
    TAB_CLOSE_HOVER_RADIUS, TAB_CLOSE_RIGHT_GAP, TAB_ROW_START_PADDING, TAB_VERTICAL_PADDING,
    WINDOW_TAB_ADD_BUTTON_RADIUS, WINDOW_TAB_FULLSCREEN_SIDEBAR_BUTTON_Y_OFFSET, WINDOW_TAB_GAP,
    WINDOW_TAB_LEADING_ACTION_BUTTON_SIZE, WINDOW_TAB_LEADING_ACTION_GAP,
    WINDOW_TAB_LEADING_ACTION_ICON_SIZE, WINDOW_TAB_RADIUS, WINDOW_TAB_TOP_SPACER,
};
use crate::termwindow::{TermWindowNotif, UIItem, UIItemType, UiShapeCacheLookup};
use crate::ui::UiPalette;
use crate::utilsprites::RenderMetrics;
use anyhow::{anyhow, Context};
use std::rc::Rc;
use termwiz::cell::CellAttributes;
use wezterm_bidi::Direction;
use wezterm_font::{ClearShapeCache, LoadedFont};
use wezterm_term::Line;
use window::color::LinearRgba;
use window::WindowOps;
use window::{IntegratedTitleButton, IntegratedTitleButtonStyle, WindowDecorations, WindowState};

const WINDOW_TAB_INSET: usize = 8;
const WINDOW_TAB_ICON_GAP: usize = 8;
const WINDOW_TAB_MIN_TEXT_COLS: usize = 3;
const UI_SHAPE_CACHE_FONT_IDENTITY_BIT: u64 = 1u64 << 63;

impl crate::TermWindow {
    pub fn invalidate_fancy_tab_bar(&mut self) {
        self.fancy_tab_bar.take();
    }

    pub fn paint_fancy_tab_bar(
        &mut self,
        layers: &mut TripleLayerQuadAllocator,
    ) -> anyhow::Result<Vec<UIItem>> {
        let chrome = UiPalette::for_appearance(crate::native_settings::effective_appearance());
        let row_height = self.tab_bar_pixel_height()?.ceil() as usize;
        if row_height == 0 {
            return Ok(vec![]);
        }

        let font = self
            .fonts
            .title_font_with_size(crate::native_settings::tab_font_size())?;
        let metrics = RenderMetrics::with_font_metrics(&font.metrics());
        let tab_count = self.window_tab_count_for_layout();
        let tab_width = self.adaptive_window_tab_width_pixels(tab_count) as usize;

        let background = chrome.sidebar_bg;
        let foreground = chrome.text;
        let muted_fg = chrome.secondary_text;

        let border = self.get_os_border();
        let row_x = self.tab_bar_left_edge();
        let row_y = if self.config.tab_bar_at_bottom {
            self.dimensions
                .pixel_height
                .saturating_sub(row_height + border.bottom.get() as usize)
        } else {
            border.top.get() as usize
        };
        let content_top_spacer = if self.config.tab_bar_at_bottom {
            0
        } else {
            self.ui_px(WINDOW_TAB_TOP_SPACER).min(row_height)
        };
        let content_row_y = row_y + content_top_spacer;
        let content_row_height = row_height.saturating_sub(content_top_spacer);
        let icon_size = fancy_tab_icon_size(&metrics, content_row_height as f32) as usize;
        let button_size = content_row_height
            .saturating_sub(self.ui_px(TAB_VERTICAL_PADDING) * 2)
            .max(icon_size);
        let row_width = self
            .dimensions
            .pixel_width
            .saturating_sub(row_x + border.right.get() as usize)
            .saturating_sub(self.right_sidebar_width())
            .max(1);
        let row_right = row_x + row_width;
        let viewport_left = (row_x as f32 + self.window_tab_left_padding_pixels()).ceil() as usize
            + self.ui_px(TAB_ROW_START_PADDING);
        let viewport_right =
            row_right.saturating_sub(self.window_tab_trailing_action_reserved_width());
        let viewport_width = viewport_right.saturating_sub(viewport_left);

        self.filled_rectangle(
            layers,
            0,
            euclid::rect(
                row_x as f32,
                row_y as f32,
                row_width as f32,
                row_height as f32,
            ),
            background,
        )
        .context("fancy tab bar background")?;

        let mut ui_items = vec![UIItem {
            x: row_x,
            y: row_y,
            width: row_width,
            height: row_height,
            item_type: UIItemType::TabBar(TabBarItem::None),
        }];

        self.paint_window_tab_leading_actions(
            layers,
            &mut ui_items,
            row_x,
            content_row_y,
            content_row_height,
            foreground,
            muted_fg,
        )?;

        let show_pane_layer_divider = mux::Mux::get()
            .get_active_tab_for_window(self.mux_window_id)
            .is_some_and(|tab| tab.iter_panes_ignoring_zoom().len() > 1);
        if show_pane_layer_divider {
            let divider_y = if self.config.tab_bar_at_bottom {
                row_y
            } else {
                row_y + row_height
            };
            self.filled_rectangle(
                layers,
                1,
                euclid::rect(row_x as f32, divider_y as f32, row_width as f32, 1.0),
                foreground.mul_alpha(0.14),
            )
            .context("fancy tab bar pane layer divider")?;
        }

        if viewport_width == 0 || tab_width == 0 {
            return Ok(ui_items);
        }

        let max_scroll = self.max_window_tab_scroll_offset();
        let scroll_offset = self.tab_bar_scroll_offset.clamp(0.0, max_scroll.max(0.0));
        let inline_rename_tab_idx = self.inline_window_tab_rename_tab_id().and_then(|tab_id| {
            mux::Mux::get()
                .get_window(self.mux_window_id)
                .and_then(|window| window.idx_by_id(tab_id))
        });

        // Remote thread connection views are selected from the sidebar and must
        // not expose the underlying mux tabs here; clicking those stale tabs
        // would leave the remote thread UI and jump to the prior terminal.
        let active_content_view_id = self.active_content_view_id;
        let cv_active = self.active_content_view_shown_in_tab_bar();
        let show_mux_tabs = !self.active_content_view_is_remote_thread();

        let tab_step = tab_width + self.ui_px(WINDOW_TAB_GAP);
        let mut tab_sequence_idx = 0usize;
        if show_mux_tabs {
            for item in self.tab_bar.items() {
                let TabBarItem::Tab { tab_idx, active } = item.item else {
                    continue;
                };
                let active = active && !cv_active;

                let virtual_left = tab_sequence_idx as f32 * tab_step as f32 - scroll_offset;
                tab_sequence_idx += 1;

                let tab_left = viewport_left as f32 + virtual_left;
                let tab_right = tab_left + tab_width as f32;
                if tab_right <= viewport_left as f32 || tab_left >= viewport_right as f32 {
                    continue;
                }

                let visible_left = tab_left.max(viewport_left as f32);
                let visible_right = tab_right.min(viewport_right as f32);
                let visible_width = (visible_right - visible_left).max(0.0);
                if visible_width <= 1.0 {
                    continue;
                }

                self.paint_window_tab(
                    layers,
                    &mut ui_items,
                    item,
                    tab_idx,
                    active,
                    inline_rename_tab_idx == Some(tab_idx),
                    tab_left,
                    visible_left,
                    visible_width,
                    viewport_left,
                    viewport_right,
                    content_row_y,
                    content_row_height,
                    tab_width,
                    button_size,
                    icon_size,
                    &font,
                    metrics,
                    chrome,
                    background,
                    if active { foreground } else { muted_fg },
                )?;
            }
        }

        // Synthetic content-view tabs (e.g. SSH Hosts), placed after the mux tabs.
        for content_tab in &self.content_views {
            if !self.content_view_shown_in_tab_bar(content_tab) {
                continue;
            }
            let virtual_left = tab_sequence_idx as f32 * tab_step as f32 - scroll_offset;
            tab_sequence_idx += 1;
            let tab_left = viewport_left as f32 + virtual_left;
            let tab_right = tab_left + tab_width as f32;
            if tab_right > viewport_left as f32 && tab_left < viewport_right as f32 {
                let visible_left = tab_left.max(viewport_left as f32);
                let visible_right = tab_right.min(viewport_right as f32);
                let visible_width = (visible_right - visible_left).max(0.0);
                if visible_width > 1.0 {
                    let active = active_content_view_id == Some(content_tab.id);
                    self.paint_content_view_tab(
                        layers,
                        &mut ui_items,
                        content_tab.id,
                        &content_tab.view.title(),
                        active,
                        tab_left,
                        visible_left,
                        visible_width,
                        viewport_left,
                        viewport_right,
                        content_row_y,
                        content_row_height,
                        tab_width,
                        button_size,
                        icon_size,
                        &font,
                        metrics,
                        chrome,
                        background,
                        if active { foreground } else { muted_fg },
                    )?;
                }
            }
        }

        let mut action_right = row_right.saturating_sub(self.ui_px(WINDOW_TAB_INSET) + 2);
        if self.fancy_tab_bar_shows_window_buttons() {
            let window_button_right = if self.right_sidebar_width() > 0 {
                self.dimensions
                    .pixel_width
                    .saturating_sub(border.right.get() as usize)
                    .saturating_sub(self.ui_px(WINDOW_TAB_INSET) + 2)
            } else {
                action_right
            };
            let window_button_left = self.paint_window_tab_window_buttons(
                layers,
                &mut ui_items,
                window_button_right,
                content_row_y,
                content_row_height,
                button_size,
                icon_size,
                foreground,
                muted_fg,
            )?;
            if self.right_sidebar_width() == 0 {
                action_right = window_button_left;
            }
        }

        if self.right_sidebar_width() > 0 {
            let action_button_x = action_right.saturating_sub(button_size);
            if cfg!(target_os = "macos") {
                self.paint_window_tab_new_button(
                    layers,
                    &mut ui_items,
                    action_button_x,
                    content_row_y,
                    content_row_height,
                    button_size,
                    icon_size,
                    foreground,
                    muted_fg,
                )?;
            } else {
                let right_sidebar_toggle_x = action_button_x;
                let new_button_x = right_sidebar_toggle_x
                    .saturating_sub(self.ui_px(WINDOW_TAB_LEADING_ACTION_GAP) + button_size);
                self.paint_window_tab_new_button(
                    layers,
                    &mut ui_items,
                    new_button_x,
                    content_row_y,
                    content_row_height,
                    button_size,
                    icon_size,
                    foreground,
                    muted_fg,
                )?;
                self.paint_window_tab_right_sidebar_toggle_button(
                    layers,
                    &mut ui_items,
                    right_sidebar_toggle_x,
                    content_row_y,
                    content_row_height,
                    button_size,
                    icon_size,
                    foreground,
                    muted_fg,
                )?;
            }
        } else {
            let right_sidebar_toggle_x = action_right.saturating_sub(button_size);
            let new_button_x = right_sidebar_toggle_x
                .saturating_sub(self.ui_px(WINDOW_TAB_LEADING_ACTION_GAP) + button_size);
            self.paint_window_tab_new_button(
                layers,
                &mut ui_items,
                new_button_x,
                content_row_y,
                content_row_height,
                button_size,
                icon_size,
                foreground,
                muted_fg,
            )?;
            self.paint_window_tab_right_sidebar_toggle_button(
                layers,
                &mut ui_items,
                right_sidebar_toggle_x,
                content_row_y,
                content_row_height,
                button_size,
                icon_size,
                foreground,
                muted_fg,
            )?;
        }

        Ok(ui_items)
    }

    fn fancy_tab_bar_shows_window_buttons(&self) -> bool {
        self.config
            .window_decorations
            .contains(WindowDecorations::INTEGRATED_BUTTONS)
            && self.config.integrated_title_button_style != IntegratedTitleButtonStyle::MacOsNative
            && !self.config.integrated_title_buttons.is_empty()
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_window_tab_leading_actions(
        &self,
        layers: &mut TripleLayerQuadAllocator,
        ui_items: &mut Vec<UIItem>,
        row_x: usize,
        row_y: usize,
        row_height: usize,
        foreground: LinearRgba,
        muted_fg: LinearRgba,
    ) -> anyhow::Result<()> {
        let action_slot_count = self.window_tab_leading_action_slot_count();
        if action_slot_count == 0 {
            return Ok(());
        }

        let mut button_x = row_x + self.window_tab_leading_action_start_pixels().ceil() as usize;
        for action_idx in 0..action_slot_count {
            let is_sidebar_toggle =
                action_idx == 0 && self.window_tab_shows_sidebar_toggle_action();
            let is_fullscreen_sidebar_toggle =
                is_sidebar_toggle && self.window_tab_sidebar_toggle_uses_fullscreen_style();
            let action_button_size = if is_sidebar_toggle {
                self.window_tab_sidebar_toggle_button_size()
            } else {
                self.ui_px(WINDOW_TAB_LEADING_ACTION_BUTTON_SIZE)
            };
            let action_icon_size = if is_sidebar_toggle {
                self.window_tab_sidebar_toggle_icon_size()
            } else {
                self.ui_px(WINDOW_TAB_LEADING_ACTION_ICON_SIZE)
            };
            let button_size = if is_fullscreen_sidebar_toggle {
                action_button_size
            } else {
                action_button_size.min(row_height.saturating_sub(4)).max(1)
            };
            let icon_size = action_icon_size.min(button_size.saturating_sub(2));

            if is_sidebar_toggle {
                let button_y = if self.config.tab_bar_at_bottom {
                    row_y + (row_height.saturating_sub(button_size) / 2)
                } else if is_fullscreen_sidebar_toggle {
                    row_y.saturating_sub(self.ui_px(WINDOW_TAB_TOP_SPACER))
                        + self.ui_px(SIDEBAR_INSET)
                        + self.ui_px(WINDOW_TAB_FULLSCREEN_SIDEBAR_BUTTON_Y_OFFSET)
                } else if !cfg!(target_os = "macos") {
                    row_y.saturating_sub(self.ui_px(WINDOW_TAB_TOP_SPACER))
                        + self.ui_px(SIDEBAR_INSET)
                } else {
                    row_y + (row_height.saturating_sub(button_size) / 2)
                };
                self.paint_window_sidebar_toggle_button(
                    layers,
                    ui_items,
                    button_x,
                    button_y,
                    button_size,
                    icon_size,
                    foreground,
                    muted_fg,
                )?;
            }
            button_x += action_button_size + self.ui_px(WINDOW_TAB_LEADING_ACTION_GAP);
        }

        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_window_sidebar_toggle_button(
        &self,
        layers: &mut TripleLayerQuadAllocator,
        ui_items: &mut Vec<UIItem>,
        button_x: usize,
        button_y: usize,
        button_size: usize,
        icon_size: usize,
        foreground: LinearRgba,
        muted_fg: LinearRgba,
    ) -> anyhow::Result<()> {
        let hovered = self.is_pointer_over_ui_rect(button_x, button_y, button_size, button_size);
        if hovered {
            self.fill_rounded_rectangle(
                layers,
                1,
                euclid::rect(
                    button_x as f32,
                    button_y as f32,
                    button_size as f32,
                    button_size as f32,
                ),
                foreground.mul_alpha(
                    if self.is_pointer_pressing_ui_rect(
                        button_x,
                        button_y,
                        button_size,
                        button_size,
                    ) {
                        0.20
                    } else {
                        0.12
                    },
                ),
                self.ui_px(SIDEBAR_INSET) as f32,
            )
            .context("window sidebar toggle hover")?;
        }

        ui_items.push(UIItem {
            x: button_x,
            y: button_y,
            width: button_size,
            height: button_size,
            item_type: UIItemType::WorkspaceSidebarToggle,
        });

        let icon_size = icon_size.min(button_size.saturating_sub(2)).max(1);
        self.paint_fancy_tab_icon(
            layers,
            self.workspace_sidebar_toggle_icon(),
            button_x + (button_size.saturating_sub(icon_size) / 2),
            button_y + (button_size.saturating_sub(icon_size) / 2),
            icon_size,
            if hovered { foreground } else { muted_fg },
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_window_tab_window_buttons(
        &self,
        layers: &mut TripleLayerQuadAllocator,
        ui_items: &mut Vec<UIItem>,
        mut action_right: usize,
        row_y: usize,
        row_height: usize,
        button_size: usize,
        icon_size: usize,
        foreground: LinearRgba,
        muted_fg: LinearRgba,
    ) -> anyhow::Result<usize> {
        let gap = self.ui_px(WINDOW_TAB_LEADING_ACTION_GAP) / 2;
        for button in self.config.integrated_title_buttons.iter().rev() {
            action_right = action_right.saturating_sub(button_size);
            self.paint_window_tab_window_button(
                layers,
                ui_items,
                *button,
                action_right,
                row_y,
                row_height,
                button_size,
                icon_size,
                foreground,
                muted_fg,
            )?;
            action_right = action_right.saturating_sub(gap);
        }

        Ok(action_right.saturating_sub(self.ui_px(WINDOW_TAB_LEADING_ACTION_GAP) / 2))
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_window_tab_window_button(
        &self,
        layers: &mut TripleLayerQuadAllocator,
        ui_items: &mut Vec<UIItem>,
        button: IntegratedTitleButton,
        button_x: usize,
        row_y: usize,
        row_height: usize,
        button_size: usize,
        icon_size: usize,
        foreground: LinearRgba,
        muted_fg: LinearRgba,
    ) -> anyhow::Result<()> {
        let button_y = row_y + (row_height.saturating_sub(button_size) / 2);
        let hovered = self.is_pointer_over_ui_rect(button_x, button_y, button_size, button_size);
        let pressed = hovered
            && self.is_pointer_pressing_ui_rect(button_x, button_y, button_size, button_size);
        let press_inset = if pressed { 1 } else { 0 };
        let visual_size = button_size.saturating_sub(press_inset * 2);
        let close_button = button == IntegratedTitleButton::Close;

        if hovered {
            let chrome = UiPalette::for_appearance(crate::native_settings::effective_appearance());
            let fill = if close_button {
                let mut red = LinearRgba::with_srgba(232, 17, 35, 255);
                if pressed {
                    red.3 = 0.82;
                }
                red
            } else if pressed {
                chrome.control_pressed_bg
            } else {
                chrome.control_hover_bg
            };
            let border = if close_button {
                LinearRgba::TRANSPARENT
            } else {
                foreground.mul_alpha(if pressed { 0.52 } else { 0.38 })
            };
            self.fill_rounded_rectangle_with_border(
                layers,
                1,
                euclid::rect(
                    (button_x + press_inset) as f32,
                    (button_y + press_inset) as f32,
                    visual_size as f32,
                    visual_size as f32,
                ),
                fill,
                border,
                WINDOW_TAB_ADD_BUTTON_RADIUS,
                ICON_BUTTON_BORDER_WIDTH,
            )
            .context("window tab title button hover")?;
        }

        ui_items.push(UIItem {
            x: button_x,
            y: button_y,
            width: button_size,
            height: button_size,
            item_type: UIItemType::TabBar(TabBarItem::WindowButton(button)),
        });

        let maximized = self
            .window_state
            .intersects(WindowState::MAXIMIZED | WindowState::FULL_SCREEN);
        let icon = match button {
            IntegratedTitleButton::Hide => SvgIcon::Minus,
            IntegratedTitleButton::Maximize if maximized => SvgIcon::Copy,
            IntegratedTitleButton::Maximize => SvgIcon::Square,
            IntegratedTitleButton::Close => SvgIcon::X,
        };
        let icon_size = if pressed {
            icon_size.saturating_sub(1).max(1)
        } else {
            icon_size
        };
        let icon_color = if close_button && hovered {
            LinearRgba(1.0, 1.0, 1.0, 1.0)
        } else if hovered {
            foreground
        } else {
            muted_fg
        };
        self.paint_fancy_tab_icon(
            layers,
            icon,
            button_x + press_inset + (visual_size.saturating_sub(icon_size) / 2),
            button_y + press_inset + (visual_size.saturating_sub(icon_size) / 2),
            icon_size,
            icon_color,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_window_tab(
        &self,
        layers: &mut TripleLayerQuadAllocator,
        ui_items: &mut Vec<UIItem>,
        item: &TabEntry,
        tab_idx: usize,
        active: bool,
        is_renaming: bool,
        tab_left: f32,
        visible_left: f32,
        visible_width: f32,
        viewport_left: usize,
        viewport_right: usize,
        row_y: usize,
        row_height: usize,
        tab_width: usize,
        button_size: usize,
        icon_size: usize,
        font: &Rc<LoadedFont>,
        metrics: RenderMetrics,
        chrome: UiPalette,
        background: LinearRgba,
        foreground: LinearRgba,
    ) -> anyhow::Result<()> {
        self.filled_rectangle(
            layers,
            1,
            euclid::rect(visible_left, row_y as f32, visible_width, row_height as f32),
            background,
        )
        .context("window tab background")?;

        let hover_x = visible_left.max(0.0) as usize;
        let hover_width = visible_width.max(0.0) as usize;
        let is_hovered = self.is_pointer_over_ui_rect(hover_x, row_y, hover_width, row_height);
        let tab_surface_color = if active {
            chrome.control_bg
        } else if is_hovered && !is_renaming {
            chrome.control_hover_bg
        } else {
            background
        };
        let tab_surface_y = row_y + (row_height.saturating_sub(button_size) / 2);
        self.fill_rounded_rectangle_with_border(
            layers,
            1,
            euclid::rect(
                visible_left,
                tab_surface_y as f32,
                visible_width,
                button_size as f32,
            ),
            tab_surface_color,
            if active {
                chrome.control_border
            } else {
                LinearRgba::TRANSPARENT
            },
            self.ui_f32(WINDOW_TAB_RADIUS),
            CAPSULE_BORDER_WIDTH,
        )
        .context("window tab surface")?;

        ui_items.push(UIItem {
            x: visible_left.max(0.0) as usize,
            y: row_y,
            width: visible_width.max(0.0) as usize,
            height: row_height,
            item_type: UIItemType::TabBar(TabBarItem::Tab { tab_idx, active }),
        });

        let tab_left = tab_left.max(0.0) as usize;
        let icon_x = tab_left + self.ui_px(WINDOW_TAB_INSET);
        let icon_y = row_y + (row_height.saturating_sub(icon_size) / 2);
        if icon_x >= viewport_left && icon_x.saturating_add(icon_size) <= viewport_right {
            self.paint_fancy_tab_icon(
                layers,
                SvgIcon::SquareTerminal,
                icon_x,
                icon_y,
                icon_size,
                foreground,
            )?;
        }

        let close_x = tab_left
            .saturating_add(tab_width)
            .saturating_sub(button_size + self.ui_px(TAB_CLOSE_RIGHT_GAP));
        let close_y = row_y + (row_height.saturating_sub(button_size) / 2);
        let close_slot_reserved = self.config.show_close_tab_button_in_tabs && !is_renaming;
        let show_close = close_slot_reserved && (active || is_hovered);
        if show_close
            && close_x >= viewport_left
            && close_x.saturating_add(button_size) <= viewport_right
        {
            let close_hovered =
                self.is_pointer_over_ui_rect(close_x, close_y, button_size, button_size);
            if close_hovered {
                let hover_alpha =
                    if self.is_pointer_pressing_ui_rect(close_x, close_y, button_size, button_size)
                    {
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
                        (close_y + hover_inset) as f32,
                        hover_size as f32,
                        hover_size as f32,
                    ),
                    foreground.mul_alpha(hover_alpha),
                    TAB_CLOSE_HOVER_RADIUS,
                )
                .context("window tab close hover")?;
            }
            ui_items.push(UIItem {
                x: close_x,
                y: close_y,
                width: button_size,
                height: button_size,
                item_type: UIItemType::CloseTab(tab_idx),
            });
            let close_fg = if close_hovered {
                foreground
            } else {
                foreground.mul_alpha(0.82)
            };
            self.paint_fancy_tab_icon(
                layers,
                SvgIcon::X,
                close_x + (button_size.saturating_sub(icon_size) / 2),
                icon_y,
                icon_size,
                close_fg,
            )?;
        }

        let mut text_x = icon_x + icon_size + self.ui_px(WINDOW_TAB_ICON_GAP);
        if let Some(status) = item.status {
            let status_x = text_x;
            if status_x >= viewport_left && status_x.saturating_add(icon_size) <= viewport_right {
                self.paint_status_icon(layers, 2, status, status_x, icon_y, icon_size, foreground)
                    .context("window tab status icon")?;
            }
            text_x = status_x + icon_size + self.ui_px(WINDOW_TAB_ICON_GAP);
        }
        let text_right = if close_slot_reserved {
            close_x.saturating_sub(self.ui_px(WINDOW_TAB_ICON_GAP))
        } else {
            tab_left
                .saturating_add(tab_width)
                .saturating_sub(self.ui_px(WINDOW_TAB_INSET))
        }
        .min(viewport_right);
        let text_width = text_right.saturating_sub(text_x);
        let text_y = row_y + (row_height.saturating_sub(metrics.cell_size.height as usize) / 2);
        if text_x >= viewport_left && text_x < viewport_right {
            let text_fg = if is_renaming {
                self.filled_rectangle(
                    layers,
                    1,
                    euclid::rect(
                        text_x.saturating_sub(3) as f32,
                        text_y as f32,
                        text_width.saturating_add(6) as f32,
                        metrics.cell_size.height as f32,
                    ),
                    chrome.selected_bg,
                )
                .context("window tab rename selection")?;
                chrome.selected_text
            } else {
                foreground
            };
            self.paint_fancy_tab_text(
                layers,
                font,
                &item.title,
                item.status.is_some(),
                text_x,
                text_y,
                text_width,
                text_fg,
            )?;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_content_view_tab(
        &self,
        layers: &mut TripleLayerQuadAllocator,
        ui_items: &mut Vec<UIItem>,
        id: crate::termwindow::content_view::ContentViewId,
        title: &str,
        active: bool,
        tab_left: f32,
        visible_left: f32,
        visible_width: f32,
        viewport_left: usize,
        viewport_right: usize,
        row_y: usize,
        row_height: usize,
        tab_width: usize,
        button_size: usize,
        icon_size: usize,
        font: &Rc<LoadedFont>,
        metrics: RenderMetrics,
        chrome: UiPalette,
        background: LinearRgba,
        foreground: LinearRgba,
    ) -> anyhow::Result<()> {
        let is_hovered = self.is_pointer_over_ui_rect(
            visible_left.max(0.0) as usize,
            row_y,
            visible_width.max(0.0) as usize,
            row_height,
        );
        let surface = if active {
            chrome.control_bg
        } else if is_hovered {
            chrome.control_hover_bg
        } else {
            background
        };
        let tab_surface_y = row_y + (row_height.saturating_sub(button_size) / 2);
        self.fill_rounded_rectangle_with_border(
            layers,
            1,
            euclid::rect(
                visible_left,
                tab_surface_y as f32,
                visible_width,
                button_size as f32,
            ),
            surface,
            if active {
                chrome.control_border
            } else {
                LinearRgba::TRANSPARENT
            },
            self.ui_f32(WINDOW_TAB_RADIUS),
            CAPSULE_BORDER_WIDTH,
        )
        .context("content view tab surface")?;

        ui_items.push(UIItem {
            x: visible_left.max(0.0) as usize,
            y: row_y,
            width: visible_width.max(0.0) as usize,
            height: row_height,
            item_type: UIItemType::TabBar(TabBarItem::ContentView { id }),
        });

        let tab_left = tab_left.max(0.0) as usize;
        let icon_x = tab_left + self.ui_px(WINDOW_TAB_INSET);
        let icon_y = row_y + (row_height.saturating_sub(icon_size) / 2);
        if icon_x >= viewport_left && icon_x.saturating_add(icon_size) <= viewport_right {
            self.paint_fancy_tab_icon(
                layers,
                SvgIcon::Link2,
                icon_x,
                icon_y,
                icon_size,
                foreground,
            )?;
        }

        let close_x = tab_left
            .saturating_add(tab_width)
            .saturating_sub(button_size + self.ui_px(TAB_CLOSE_RIGHT_GAP));
        let close_y = row_y + (row_height.saturating_sub(button_size) / 2);
        let close_slot_visible =
            close_x >= viewport_left && close_x.saturating_add(button_size) <= viewport_right;
        let show_close = close_slot_visible && (active || is_hovered);
        if show_close {
            let close_hovered =
                self.is_pointer_over_ui_rect(close_x, close_y, button_size, button_size);
            ui_items.push(UIItem {
                x: close_x,
                y: close_y,
                width: button_size,
                height: button_size,
                item_type: UIItemType::ContentViewClose(id),
            });
            self.paint_fancy_tab_icon(
                layers,
                SvgIcon::X,
                close_x + (button_size.saturating_sub(icon_size) / 2),
                icon_y,
                icon_size,
                if close_hovered {
                    foreground
                } else {
                    foreground.mul_alpha(0.82)
                },
            )?;
        }

        let text_x = icon_x + icon_size + self.ui_px(WINDOW_TAB_ICON_GAP);
        let text_right = if close_slot_visible {
            close_x.saturating_sub(self.ui_px(WINDOW_TAB_ICON_GAP))
        } else {
            tab_left
                .saturating_add(tab_width)
                .saturating_sub(self.ui_px(WINDOW_TAB_INSET))
        }
        .min(viewport_right);
        let text_width = text_right.saturating_sub(text_x);
        let text_y = row_y + (row_height.saturating_sub(metrics.cell_size.height as usize) / 2);
        if text_x >= viewport_left && text_x < viewport_right {
            let line = Line::from_text(title, &CellAttributes::blank(), 0, None);
            self.paint_fancy_tab_text(
                layers, font, &line, false, text_x, text_y, text_width, foreground,
            )?;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_window_tab_new_button(
        &self,
        layers: &mut TripleLayerQuadAllocator,
        ui_items: &mut Vec<UIItem>,
        button_x: usize,
        row_y: usize,
        row_height: usize,
        button_size: usize,
        icon_size: usize,
        foreground: LinearRgba,
        muted_fg: LinearRgba,
    ) -> anyhow::Result<()> {
        let button_y = row_y + (row_height.saturating_sub(button_size) / 2);
        let hovered = self.is_pointer_over_ui_rect(button_x, button_y, button_size, button_size);
        let pressed = hovered
            && self.is_pointer_pressing_ui_rect(button_x, button_y, button_size, button_size);
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
                    (button_x + press_inset) as f32,
                    (button_y + press_inset) as f32,
                    visual_size as f32,
                    visual_size as f32,
                ),
                fill,
                foreground.mul_alpha(border_alpha),
                WINDOW_TAB_ADD_BUTTON_RADIUS,
                ICON_BUTTON_BORDER_WIDTH,
            )
            .context("window tab new button hover")?;
        }

        ui_items.push(UIItem {
            x: button_x,
            y: button_y,
            width: button_size,
            height: button_size,
            item_type: UIItemType::TabBar(TabBarItem::NewTabButton),
        });

        let icon_size = if pressed {
            icon_size.saturating_sub(1).max(1)
        } else {
            icon_size
        };
        self.paint_fancy_tab_icon(
            layers,
            SvgIcon::Plus,
            button_x + press_inset + (visual_size.saturating_sub(icon_size) / 2),
            button_y + press_inset + (visual_size.saturating_sub(icon_size) / 2),
            icon_size,
            if hovered { foreground } else { muted_fg },
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_window_tab_right_sidebar_toggle_button(
        &self,
        layers: &mut TripleLayerQuadAllocator,
        ui_items: &mut Vec<UIItem>,
        button_x: usize,
        row_y: usize,
        row_height: usize,
        button_size: usize,
        icon_size: usize,
        foreground: LinearRgba,
        muted_fg: LinearRgba,
    ) -> anyhow::Result<()> {
        let button_y = row_y + (row_height.saturating_sub(button_size) / 2);
        let hovered = self.is_pointer_over_ui_rect(button_x, button_y, button_size, button_size);
        let pressed = hovered
            && self.is_pointer_pressing_ui_rect(button_x, button_y, button_size, button_size);
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
                    (button_x + press_inset) as f32,
                    (button_y + press_inset) as f32,
                    visual_size as f32,
                    visual_size as f32,
                ),
                fill,
                foreground.mul_alpha(border_alpha),
                WINDOW_TAB_ADD_BUTTON_RADIUS,
                ICON_BUTTON_BORDER_WIDTH,
            )
            .context("window tab right sidebar toggle hover")?;
        }

        ui_items.push(UIItem {
            x: button_x,
            y: button_y,
            width: button_size,
            height: button_size,
            item_type: UIItemType::RightSidebarToggle,
        });

        let icon_size = if pressed {
            icon_size.saturating_sub(1).max(1)
        } else {
            icon_size
        };
        self.paint_fancy_tab_icon(
            layers,
            self.right_sidebar_toggle_icon(),
            button_x + press_inset + (visual_size.saturating_sub(icon_size) / 2),
            button_y + press_inset + (visual_size.saturating_sub(icon_size) / 2),
            icon_size,
            if hovered { foreground } else { muted_fg },
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_fancy_tab_text(
        &self,
        layers: &mut TripleLayerQuadAllocator,
        font: &Rc<LoadedFont>,
        title: &Line,
        strip_leading_progress: bool,
        x: usize,
        y: usize,
        width: usize,
        foreground: LinearRgba,
    ) -> anyhow::Result<()> {
        let metrics = RenderMetrics::with_font_metrics(&font.metrics());
        let cell_width = (metrics.cell_size.width as usize).max(1);
        let min_width = WINDOW_TAB_MIN_TEXT_COLS * cell_width;
        if width < min_width {
            return Ok(());
        }

        let mut text = String::new();
        let mut trimming_legacy_progress = strip_leading_progress;
        for cell in title.visible_cells() {
            let value = cell.str();
            if trimming_legacy_progress {
                if is_legacy_progress_marker(value) {
                    continue;
                }
                if text.is_empty() && value.trim().is_empty() {
                    continue;
                }
                trimming_legacy_progress = false;
            }
            text.push_str(value);
        }
        let text = self.ellipsize_ui_text(font, &text, width)?;
        self.paint_ui_title_text(layers, font, &metrics, &text, x, y, width, foreground)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn paint_ui_title_text(
        &self,
        layers: &mut TripleLayerQuadAllocator,
        font: &Rc<LoadedFont>,
        metrics: &RenderMetrics,
        text: &str,
        x: usize,
        y: usize,
        width: usize,
        foreground: LinearRgba,
    ) -> anyhow::Result<()> {
        self.paint_ui_title_text_with_advance(layers, font, metrics, text, x, y, width, foreground)
            .map(|_| ())
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn paint_ui_title_text_with_advance(
        &self,
        layers: &mut TripleLayerQuadAllocator,
        font: &Rc<LoadedFont>,
        metrics: &RenderMetrics,
        text: &str,
        x: usize,
        y: usize,
        width: usize,
        foreground: LinearRgba,
    ) -> anyhow::Result<f32> {
        if text.is_empty() || width == 0 {
            return Ok(0.0);
        }
        // Shape once via the shared cache and draw through the single emission
        // helper, so tab-bar/pane/sidebar title text stops re-shaping every frame
        // (it used to run harfbuzz inline on each paint).
        let (shaped, _) = self.cached_ui_shape(font, metrics, text)?;
        self.paint_cached_ui_shape(layers, metrics, &shaped, x, y, width, |_| foreground)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn paint_ui_title_text_cached(
        &self,
        layers: &mut TripleLayerQuadAllocator,
        font: &Rc<LoadedFont>,
        metrics: &RenderMetrics,
        text: &str,
        x: usize,
        y: usize,
        width: usize,
        foreground: LinearRgba,
    ) -> anyhow::Result<UiShapeCacheLookup> {
        self.paint_ui_title_text_cached_with_advance(
            layers, font, metrics, text, x, y, width, foreground,
        )
        .map(|(_, lookup)| lookup)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn paint_ui_title_text_cached_with_advance(
        &self,
        layers: &mut TripleLayerQuadAllocator,
        font: &Rc<LoadedFont>,
        metrics: &RenderMetrics,
        text: &str,
        x: usize,
        y: usize,
        width: usize,
        foreground: LinearRgba,
    ) -> anyhow::Result<(f32, UiShapeCacheLookup)> {
        if text.is_empty() || width == 0 {
            return Ok((0.0, UiShapeCacheLookup::Skipped));
        }

        let (shaped, lookup) = self.cached_ui_shape(font, metrics, text)?;
        let advance =
            self.paint_cached_ui_shape(layers, metrics, &shaped, x, y, width, |_| foreground)?;
        Ok((advance, lookup))
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn paint_ui_colored_text_cached(
        &self,
        layers: &mut TripleLayerQuadAllocator,
        font: &Rc<LoadedFont>,
        metrics: &RenderMetrics,
        text: &str,
        char_colors: &[LinearRgba],
        default_color: LinearRgba,
        x: usize,
        y: usize,
        width: usize,
    ) -> anyhow::Result<UiShapeCacheLookup> {
        if text.is_empty() || width == 0 {
            return Ok(UiShapeCacheLookup::Skipped);
        }

        let (shaped, lookup) = self.cached_ui_shape(font, metrics, text)?;

        // Map each byte offset to its char index so a glyph's cluster can look
        // up the colour of the character it came from. This stays outside the
        // shape cache because colours are applied at paint time.
        let mut byte_to_char = vec![0usize; text.len() + 1];
        for (char_idx, (byte_idx, ch)) in text.char_indices().enumerate() {
            for byte in byte_idx..byte_idx + ch.len_utf8() {
                byte_to_char[byte] = char_idx;
            }
        }
        if let Some(last) = byte_to_char.last_mut() {
            *last = char_colors.len().saturating_sub(1);
        }

        self.paint_cached_ui_shape(layers, metrics, &shaped, x, y, width, |info| {
            let char_idx = byte_to_char.get(info.cluster).copied().unwrap_or(0);
            char_colors.get(char_idx).copied().unwrap_or(default_color)
        })?;

        Ok(lookup)
    }

    pub(crate) fn cached_ui_shape(
        &self,
        font: &Rc<LoadedFont>,
        metrics: &RenderMetrics,
        text: &str,
    ) -> anyhow::Result<(Rc<Vec<ShapedInfo>>, UiShapeCacheLookup)> {
        let font_identity = UI_SHAPE_CACHE_FONT_IDENTITY_BIT | font.id() as u64;
        let style = font.style();
        let key = BorrowedShapeCacheKey {
            font_identity,
            style,
            text,
        };

        if let Some(cached) = self.lookup_cached_shape(&key) {
            return cached.map(|shaped| (shaped, UiShapeCacheLookup::Hit));
        }

        let Some(window) = self.window.as_ref().cloned() else {
            return Ok((Rc::new(Vec::new()), UiShapeCacheLookup::Skipped));
        };
        let Some(gl_state) = self.render_state.as_ref() else {
            return Ok((Rc::new(Vec::new()), UiShapeCacheLookup::Skipped));
        };

        let infos = match font.shape(
            text,
            move || window.notify(TermWindowNotif::InvalidateShapeCache),
            BlockKey::filter_out_synthetic,
            None,
            Direction::LeftToRight,
            None,
            None,
        ) {
            Ok(infos) => infos,
            Err(err) => {
                if err.root_cause().downcast_ref::<ClearShapeCache>().is_some() {
                    return Err(err);
                }

                let res = anyhow!("shaper error: {}", err);
                self.shape_cache.borrow_mut().put(key.to_owned(), Err(err));
                return Err(res);
            }
        };

        let glyphs = {
            let mut glyph_cache = gl_state.glyph_cache.borrow_mut();
            self.glyph_infos_to_glyphs(style, &mut glyph_cache, &infos, font, metrics)?
        };
        let shaped = Rc::new(ShapedInfo::process(&infos, &glyphs));
        self.shape_cache
            .borrow_mut()
            .put(key.to_owned(), Ok(Rc::clone(&shaped)));
        Ok((shaped, UiShapeCacheLookup::Miss))
    }

    /// Total advance width (px) of `text` in `font`, via the shared shape cache.
    /// Reuses the same cached glyph run as the cached painters, so width queries
    /// (ellipsize, button sizing) become cache hits after the first shape and
    /// stay consistent with what is painted.
    pub(crate) fn cached_ui_text_advance(
        &self,
        font: &Rc<LoadedFont>,
        metrics: &RenderMetrics,
        text: &str,
    ) -> anyhow::Result<f32> {
        if text.is_empty() {
            return Ok(0.0);
        }
        let (shaped, _) = self.cached_ui_shape(font, metrics, text)?;
        Ok(shaped
            .iter()
            .map(|info| info.glyph.x_advance.get() as f32)
            .sum())
    }

    fn paint_cached_ui_shape<F>(
        &self,
        layers: &mut TripleLayerQuadAllocator,
        metrics: &RenderMetrics,
        shaped: &[ShapedInfo],
        x: usize,
        y: usize,
        width: usize,
        color_for: F,
    ) -> anyhow::Result<f32>
    where
        F: FnMut(&ShapedInfo) -> LinearRgba,
    {
        self.paint_cached_ui_shape_clipped(
            layers,
            metrics,
            shaped,
            x as f32,
            y as f32,
            x as f32,
            (x + width) as f32,
            color_for,
        )
    }

    /// Draw a cached glyph run starting at pixel `start_x` (which may be left of
    /// `clip_left`, e.g. for a horizontally-scrolled line), clipping to
    /// `[clip_left, clip_right]`: glyphs fully left of `clip_left` advance the
    /// pen but draw nothing, and drawing stops at `clip_right`. With
    /// `start_x == clip_left` this is identical to the previous fixed-origin
    /// painter, so existing callers are unaffected.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn paint_cached_ui_shape_clipped<F>(
        &self,
        layers: &mut TripleLayerQuadAllocator,
        metrics: &RenderMetrics,
        shaped: &[ShapedInfo],
        start_x: f32,
        y: f32,
        clip_left: f32,
        clip_right: f32,
        mut color_for: F,
    ) -> anyhow::Result<f32>
    where
        F: FnMut(&ShapedInfo) -> LinearRgba,
    {
        if shaped.is_empty() || clip_right <= clip_left {
            return Ok(0.0);
        }

        let Some(gl_state) = self.render_state.as_ref() else {
            return Ok(0.0);
        };
        let mut glyph_cache = gl_state.glyph_cache.borrow_mut();
        let left_offset = self.dimensions.pixel_width as f32 / -2.0;
        let top_offset = self.dimensions.pixel_height as f32 / -2.0;
        let baseline = metrics.cell_size.height as f32 + metrics.descender.get() as f32;
        let mut x_pos = start_x;

        for info in shaped {
            let advance = info.glyph.x_advance.get() as f32;
            // Fully left of the viewport: advance the pen, draw nothing.
            if x_pos + advance <= clip_left {
                x_pos += advance;
                continue;
            }
            let color = color_for(info);

            if let Some(key) = info.block_key {
                if x_pos + advance > clip_right {
                    break;
                }
                let sprite = glyph_cache.cached_block(key, metrics)?;
                let mut quad = layers.allocate(2)?;
                quad.set_position(
                    x_pos + left_offset,
                    y + top_offset,
                    x_pos + left_offset + advance,
                    y + top_offset + metrics.cell_size.height as f32,
                );
                quad.set_texture(sprite.texture_coords());
                quad.set_fg_color(color);
                quad.set_alt_color_and_mix_value(color, 0.0);
                quad.set_hsv(None);
                x_pos += advance;
                continue;
            }

            let glyph = &info.glyph;
            if x_pos + advance > clip_right {
                break;
            }

            if let Some(texture) = glyph.texture.as_ref() {
                let glyph_x = x_pos + (glyph.x_offset + glyph.bearing_x).get() as f32;
                let glyph_y = y - (glyph.y_offset + glyph.bearing_y).get() as f32 + baseline;
                let glyph_width = texture.coords.size.width as f32 * glyph.scale as f32;
                let glyph_height = texture.coords.size.height as f32 * glyph.scale as f32;
                if glyph_x + glyph_width > clip_right {
                    break;
                }
                let mut quad = layers.allocate(2)?;
                quad.set_position(
                    glyph_x + left_offset,
                    glyph_y + top_offset,
                    glyph_x + left_offset + glyph_width,
                    glyph_y + top_offset + glyph_height,
                );
                quad.set_texture(texture.texture_coords());
                quad.set_fg_color(color);
                quad.set_alt_color_and_mix_value(color, 0.0);
                quad.set_has_color(glyph.has_color);
                quad.set_hsv(None);
            }

            x_pos += advance;
        }

        Ok((x_pos - start_x).max(0.0))
    }

    fn paint_fancy_tab_icon(
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

fn fancy_tab_icon_size(metrics: &RenderMetrics, tab_bar_height: f32) -> f32 {
    let cell_height = metrics.cell_size.height.max(1) as f32;
    let from_bar = (tab_bar_height - cell_height * 0.45).max(1.0);
    let from_font = metrics.cell_size.height as f32 * 0.95;
    from_bar.min(from_font).max(1.0).floor()
}

fn is_legacy_progress_marker(value: &str) -> bool {
    let mut chars = value.chars();
    let Some(ch) = chars.next() else {
        return false;
    };
    if chars.next().is_some() {
        return false;
    }

    matches!(
        ch as u32,
        0x2800..=0x28ff | 0xf0130 | 0xf0a9e..=0xf0aa5 | 0xee00..=0xee0b
    )
}
