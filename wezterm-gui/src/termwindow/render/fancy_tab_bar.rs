use crate::customglyph::BlockKey;
use crate::quad::{QuadTrait, TripleLayerQuadAllocator, TripleLayerQuadAllocatorTrait};
use crate::tabbar::{TabBarItem, TabEntry};
use crate::termwindow::ui::icons::SvgIcon;
use crate::termwindow::{TermWindowNotif, UIItem, UIItemType};
use crate::utilsprites::RenderMetrics;
use anyhow::Context;
use finl_unicode::grapheme_clusters::Graphemes;
use std::rc::Rc;
use termwiz::cell::grapheme_column_width;
use wezterm_bidi::Direction;
use wezterm_font::LoadedFont;
use wezterm_term::Line;
use wezterm_term::color::{ColorAttribute, ColorPalette};
use window::WindowOps;
use window::color::LinearRgba;

const WINDOW_TAB_INSET: usize = 8;
const WINDOW_TAB_ICON_GAP: usize = 8;
const WINDOW_TAB_BUTTON_GAP: usize = 6;
const WINDOW_TAB_MIN_TEXT_COLS: usize = 3;

impl crate::TermWindow {
    pub fn invalidate_fancy_tab_bar(&mut self) {
        self.fancy_tab_bar.take();
    }

    pub fn paint_fancy_tab_bar(
        &mut self,
        layers: &mut TripleLayerQuadAllocator,
    ) -> anyhow::Result<Vec<UIItem>> {
        let palette = self.palette().clone();
        let row_height = self.tab_bar_pixel_height()?.ceil() as usize;
        if row_height == 0 {
            return Ok(vec![]);
        }

        let font = self.fonts.title_font()?;
        let metrics = RenderMetrics::with_font_metrics(&font.metrics());
        let icon_size = fancy_tab_icon_size(&metrics, row_height as f32) as usize;
        let button_size = (icon_size + 10)
            .min(row_height.saturating_sub(6))
            .max(icon_size);
        let tab_width = self.window_tab_width_pixels().ceil() as usize;

        let background = palette
            .resolve_bg(ColorAttribute::Default)
            .to_linear()
            .mul_alpha(self.config.window_background_opacity);
        let foreground = palette.foreground.to_linear();
        let muted_fg = foreground.mul_alpha(0.66);
        let divider = foreground.mul_alpha(0.14);
        let active_edge = foreground.mul_alpha(0.20);
        let accent = palette.selection_bg.to_linear();

        let border = self.get_os_border();
        let row_x = self.tab_bar_left_edge();
        let row_y = if self.config.tab_bar_at_bottom {
            self.dimensions
                .pixel_height
                .saturating_sub(row_height + border.bottom.get() as usize)
        } else {
            border.top.get() as usize
        };
        let row_width = self
            .dimensions
            .pixel_width
            .saturating_sub(row_x + border.right.get() as usize)
            .max(1);
        let row_right = row_x + row_width;
        let viewport_left = (row_x as f32 + self.window_tab_left_padding_pixels()).ceil() as usize;
        let viewport_right = row_right;
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
        self.filled_rectangle(
            layers,
            1,
            euclid::rect(
                row_x as f32,
                (row_y + row_height.saturating_sub(1)) as f32,
                row_width as f32,
                1.0,
            ),
            divider,
        )
        .context("fancy tab bar divider")?;

        let mut ui_items = vec![UIItem {
            x: row_x,
            y: row_y,
            width: row_width,
            height: row_height,
            item_type: UIItemType::TabBar(TabBarItem::None),
        }];

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

        let mut tab_sequence_idx = 0usize;
        for item in self.tab_bar.items() {
            let TabBarItem::Tab { tab_idx, active } = item.item else {
                continue;
            };

            let virtual_left = tab_sequence_idx as f32 * tab_width as f32 - scroll_offset;
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
                row_y,
                row_height,
                tab_width,
                button_size,
                icon_size,
                &font,
                metrics,
                &palette,
                background,
                if active { foreground } else { muted_fg },
                active_edge,
                accent,
            )?;
        }

        self.paint_fancy_tab_bar_overflow(
            layers,
            row_y,
            row_height,
            viewport_left,
            viewport_width,
            scroll_offset,
            max_scroll,
            background,
            divider,
        )?;

        Ok(ui_items)
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
        palette: &ColorPalette,
        background: LinearRgba,
        foreground: LinearRgba,
        active_edge: LinearRgba,
        accent: LinearRgba,
    ) -> anyhow::Result<()> {
        self.filled_rectangle(
            layers,
            1,
            euclid::rect(visible_left, row_y as f32, visible_width, row_height as f32),
            background,
        )
        .context("window tab background")?;

        if active {
            self.filled_rectangle(
                layers,
                1,
                euclid::rect(visible_left, row_y as f32, 1.0, row_height as f32),
                active_edge,
            )
            .context("window tab active left edge")?;
            self.filled_rectangle(
                layers,
                1,
                euclid::rect(
                    (visible_left + visible_width - 1.0).max(visible_left),
                    row_y as f32,
                    1.0,
                    row_height as f32,
                ),
                active_edge,
            )
            .context("window tab active right edge")?;
            self.filled_rectangle(
                layers,
                1,
                euclid::rect(
                    visible_left,
                    row_y as f32 + row_height.saturating_sub(3) as f32,
                    visible_width,
                    3.0,
                ),
                accent,
            )
            .context("window tab active indicator")?;
        } else {
            self.filled_rectangle(
                layers,
                1,
                euclid::rect(
                    (visible_left + visible_width - 1.0).max(visible_left),
                    row_y as f32 + 7.0,
                    1.0,
                    row_height.saturating_sub(14) as f32,
                ),
                active_edge.mul_alpha(0.55),
            )
            .context("window tab separator")?;
        }

        ui_items.push(UIItem {
            x: visible_left.max(0.0) as usize,
            y: row_y,
            width: visible_width.max(0.0) as usize,
            height: row_height,
            item_type: UIItemType::TabBar(TabBarItem::Tab { tab_idx, active }),
        });

        let tab_left = tab_left.max(0.0) as usize;
        let icon_x = tab_left + WINDOW_TAB_INSET;
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
            .saturating_sub(button_size + WINDOW_TAB_BUTTON_GAP);
        let show_close = self.config.show_close_tab_button_in_tabs && !is_renaming;
        if show_close
            && close_x >= viewport_left
            && close_x.saturating_add(button_size) <= viewport_right
        {
            ui_items.push(UIItem {
                x: close_x,
                y: row_y + (row_height.saturating_sub(button_size) / 2),
                width: button_size,
                height: button_size,
                item_type: UIItemType::CloseTab(tab_idx),
            });
            self.paint_fancy_tab_icon(
                layers,
                SvgIcon::X,
                close_x + (button_size.saturating_sub(icon_size) / 2),
                icon_y,
                icon_size,
                foreground.mul_alpha(0.82),
            )?;
        }

        let text_x = icon_x + icon_size + WINDOW_TAB_ICON_GAP;
        let text_right = if show_close {
            close_x.saturating_sub(WINDOW_TAB_ICON_GAP)
        } else {
            tab_left
                .saturating_add(tab_width)
                .saturating_sub(WINDOW_TAB_INSET)
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
                    palette.selection_bg.to_linear(),
                )
                .context("window tab rename selection")?;
                palette.selection_fg.to_linear()
            } else {
                foreground
            };
            self.paint_fancy_tab_text(
                layers,
                font,
                &item.title,
                text_x,
                text_y,
                text_width,
                text_fg,
            )?;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_fancy_tab_bar_overflow(
        &self,
        layers: &mut TripleLayerQuadAllocator,
        row_y: usize,
        row_height: usize,
        viewport_left: usize,
        viewport_width: usize,
        scroll_offset: f32,
        max_scroll: f32,
        background: LinearRgba,
        divider: LinearRgba,
    ) -> anyhow::Result<()> {
        let fade_width = 22usize.min(viewport_width / 3);
        if fade_width <= 1 {
            return Ok(());
        }

        if scroll_offset > 0.0 {
            self.filled_rectangle(
                layers,
                1,
                euclid::rect(
                    viewport_left as f32,
                    row_y as f32,
                    fade_width as f32,
                    row_height as f32,
                ),
                background.mul_alpha(0.94),
            )
            .context("window tab left overflow shade")?;
            self.filled_rectangle(
                layers,
                1,
                euclid::rect(
                    (viewport_left + fade_width.saturating_sub(2)) as f32,
                    row_y as f32,
                    2.0,
                    row_height as f32,
                ),
                divider,
            )
            .context("window tab left overflow edge")?;
        }

        if scroll_offset < max_scroll {
            let x = viewport_left + viewport_width.saturating_sub(fade_width);
            self.filled_rectangle(
                layers,
                1,
                euclid::rect(x as f32, row_y as f32, fade_width as f32, row_height as f32),
                background.mul_alpha(0.94),
            )
            .context("window tab right overflow shade")?;
            self.filled_rectangle(
                layers,
                1,
                euclid::rect(x as f32, row_y as f32, 2.0, row_height as f32),
                divider,
            )
            .context("window tab right overflow edge")?;
        }

        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_fancy_tab_text(
        &self,
        layers: &mut TripleLayerQuadAllocator,
        font: &Rc<LoadedFont>,
        title: &Line,
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
        for cell in title.visible_cells() {
            text.push_str(cell.str());
        }
        self.paint_ui_title_text(layers, font, &metrics, &text, x, y, width, foreground)
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn paint_ui_title_text(
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
        if text.is_empty() || width == 0 {
            return Ok(());
        }

        let Some(window) = self.window.as_ref().cloned() else {
            return Ok(());
        };
        let gl_state = self.render_state.as_ref().unwrap();
        let infos = font.shape(
            text,
            move || window.notify(TermWindowNotif::InvalidateShapeCache),
            BlockKey::filter_out_synthetic,
            None,
            Direction::LeftToRight,
            None,
            None,
        )?;
        let mut glyph_cache = gl_state.glyph_cache.borrow_mut();
        let left_offset = self.dimensions.pixel_width as f32 / -2.0;
        let top_offset = self.dimensions.pixel_height as f32 / -2.0;
        let baseline = metrics.cell_size.height as f32 + metrics.descender.get() as f32;
        let max_x = x as f32 + width as f32;
        let mut x_pos = x as f32;
        let y = y as f32;
        let style = font.style();

        for info in infos {
            let cell_start = &text[info.cluster as usize..];
            let mut iter = Graphemes::new(cell_start).peekable();
            let Some(grapheme) = iter.next() else {
                continue;
            };

            if let Some(key) = BlockKey::from_str(grapheme) {
                let advance = metrics.cell_size.width as f32;
                if x_pos + advance > max_x {
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
                quad.set_fg_color(foreground);
                quad.set_alt_color_and_mix_value(foreground, 0.0);
                quad.set_hsv(None);
                x_pos += advance;
                continue;
            }

            let next_grapheme = iter.peek().copied();
            let followed_by_space = next_grapheme == Some(" ");
            let num_cells = grapheme_column_width(grapheme, None).max(1) as u8;
            let glyph = glyph_cache.cached_glyph(
                &info,
                &style,
                followed_by_space,
                font,
                metrics,
                num_cells,
            )?;
            let advance = glyph.x_advance.get() as f32;
            if x_pos + advance > max_x {
                break;
            }

            if let Some(texture) = glyph.texture.as_ref() {
                let glyph_x = x_pos + (glyph.x_offset + glyph.bearing_x).get() as f32;
                let glyph_y = y - (glyph.y_offset + glyph.bearing_y).get() as f32 + baseline;
                let glyph_width = texture.coords.size.width as f32 * glyph.scale as f32;
                let glyph_height = texture.coords.size.height as f32 * glyph.scale as f32;
                if glyph_x + glyph_width > max_x {
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
                quad.set_fg_color(foreground);
                quad.set_alt_color_and_mix_value(foreground, 0.0);
                quad.set_has_color(glyph.has_color);
                quad.set_hsv(None);
            }

            x_pos += advance;
        }

        Ok(())
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
    let from_bar = (tab_bar_height - 14.0).max(1.0);
    let from_font = metrics.cell_size.height as f32 * 0.95;
    from_bar.min(from_font).clamp(18.0, 24.0).floor()
}
