//! Low-level GPU drawing primitives shared by native ThinkTerm UI surfaces
//! (the settings window and the in-window SSH hosts view).
//!
//! A [`DrawContext`] bundles the three things every primitive needs — the
//! window's [`RenderState`], its [`Dimensions`], and [`RenderMetrics`] — so the
//! same drawing code works regardless of which window/surface owns them. The
//! method bodies are lifted verbatim from `settings_window.rs` (which keeps its
//! own copies for now); the only change is `self.render_state.as_ref().unwrap()`
//! becomes the borrowed `self.render_state` field.

use crate::customglyph::{BlockKey, Poly};
use crate::quad::{QuadTrait, TripleLayerQuadAllocator, TripleLayerQuadAllocatorTrait};
use crate::renderstate::RenderState;
use crate::termwindow::render::corners::{
    BOTTOM_LEFT_ROUNDED_CORNER, BOTTOM_RIGHT_ROUNDED_CORNER, TOP_LEFT_ROUNDED_CORNER,
    TOP_RIGHT_ROUNDED_CORNER,
};
use crate::termwindow::ui::icons::{BrandIcon, SvgIcon};
use crate::utilsprites::RenderMetrics;
use std::rc::Rc;
use wezterm_bidi::Direction;
use wezterm_font::LoadedFont;
use window::color::LinearRgba;
use window::Dimensions;

/// Borrowed handles needed by every primitive. Cheap to build each frame.
pub(crate) struct DrawContext<'a> {
    pub render_state: &'a RenderState,
    pub dimensions: Dimensions,
    pub metrics: &'a RenderMetrics,
}

impl<'a> DrawContext<'a> {
    pub(crate) fn new(
        render_state: &'a RenderState,
        dimensions: Dimensions,
        metrics: &'a RenderMetrics,
    ) -> Self {
        Self {
            render_state,
            dimensions,
            metrics,
        }
    }

    pub(crate) fn draw_rect(
        &self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        layer_num: usize,
        x: f32,
        y: f32,
        width: f32,
        height: f32,
        color: LinearRgba,
    ) -> anyhow::Result<()> {
        if width <= 0.0 || height <= 0.0 {
            return Ok(());
        }

        let render_state = self.render_state;
        let mut quad = layers.allocate(layer_num)?;
        let left_offset = self.dimensions.pixel_width as f32 / 2.0;
        let top_offset = self.dimensions.pixel_height as f32 / 2.0;
        quad.set_position(
            x - left_offset,
            y - top_offset,
            x + width - left_offset,
            y + height - top_offset,
        );
        quad.set_texture(render_state.util_sprites.filled_box.texture_coords());
        quad.set_is_background();
        quad.set_fg_color(color);
        quad.set_hsv(None);
        Ok(())
    }

    pub(crate) fn draw_rounded_rect(
        &self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        layer_num: usize,
        x: f32,
        y: f32,
        width: f32,
        height: f32,
        color: LinearRgba,
        radius: f32,
    ) -> anyhow::Result<()> {
        if width <= 0.0 || height <= 0.0 {
            return Ok(());
        }

        let radius = radius.min(width / 2.0).min(height / 2.0).round().max(0.0);
        if radius <= 0.0 {
            return self.draw_rect(layers, layer_num, x, y, width, height, color);
        }

        let corner_size = euclid::size2(radius, radius);
        self.draw_corner(
            layers,
            layer_num,
            x,
            y,
            TOP_LEFT_ROUNDED_CORNER,
            corner_size,
            color,
        )?;
        self.draw_corner(
            layers,
            layer_num,
            x + width - radius,
            y,
            TOP_RIGHT_ROUNDED_CORNER,
            corner_size,
            color,
        )?;
        self.draw_corner(
            layers,
            layer_num,
            x,
            y + height - radius,
            BOTTOM_LEFT_ROUNDED_CORNER,
            corner_size,
            color,
        )?;
        self.draw_corner(
            layers,
            layer_num,
            x + width - radius,
            y + height - radius,
            BOTTOM_RIGHT_ROUNDED_CORNER,
            corner_size,
            color,
        )?;

        self.draw_rect(
            layers,
            layer_num,
            x + radius,
            y,
            width - radius * 2.0,
            height,
            color,
        )?;
        self.draw_rect(
            layers,
            layer_num,
            x,
            y + radius,
            radius,
            height - radius * 2.0,
            color,
        )?;
        self.draw_rect(
            layers,
            layer_num,
            x + width - radius,
            y + radius,
            radius,
            height - radius * 2.0,
            color,
        )?;

        Ok(())
    }

    pub(crate) fn draw_rounded_frame(
        &self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        layer_num: usize,
        x: f32,
        y: f32,
        width: f32,
        height: f32,
        fill: LinearRgba,
        border: LinearRgba,
        radius: f32,
    ) -> anyhow::Result<()> {
        self.draw_rounded_rect(layers, layer_num, x, y, width, height, border, radius)?;
        self.draw_rounded_rect(
            layers,
            layer_num,
            x + 1.0,
            y + 1.0,
            width - 2.0,
            height - 2.0,
            fill,
            (radius - 1.0).max(0.0),
        )?;
        Ok(())
    }

    pub(crate) fn draw_corner(
        &self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        layer_num: usize,
        x: f32,
        y: f32,
        polys: &'static [Poly],
        size: euclid::Size2D<f32, window::PixelUnit>,
        color: LinearRgba,
    ) -> anyhow::Result<()> {
        let render_state = self.render_state;
        let sprite = render_state
            .glyph_cache
            .borrow_mut()
            .cached_block(
                BlockKey::PolyWithCustomMetrics {
                    polys,
                    underline_height: self.metrics.underline_height,
                    cell_size: euclid::size2(size.width as isize, size.height as isize),
                },
                self.metrics,
            )?
            .texture_coords();

        let mut quad = layers.allocate(layer_num)?;
        let left_offset = self.dimensions.pixel_width as f32 / 2.0;
        let top_offset = self.dimensions.pixel_height as f32 / 2.0;
        quad.set_position(
            x - left_offset,
            y - top_offset,
            x + size.width - left_offset,
            y + size.height - top_offset,
        );
        quad.set_texture(sprite);
        quad.set_fg_color(color);
        quad.set_alt_color_and_mix_value(color, 0.0);
        quad.set_hsv(None);
        quad.set_has_color(false);
        quad.set_grayscale();
        Ok(())
    }

    pub(crate) fn draw_svg_icon(
        &self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        icon: SvgIcon,
        x: f32,
        y: f32,
        size: f32,
        color: LinearRgba,
    ) -> anyhow::Result<()> {
        if size <= 0.0 {
            return Ok(());
        }

        let render_state = self.render_state;
        let sprite = render_state
            .glyph_cache
            .borrow_mut()
            .cached_svg_icon(icon, size.round() as usize)?
            .texture_coords();
        let mut quad = layers.allocate(2)?;
        let left_offset = self.dimensions.pixel_width as f32 / 2.0;
        let top_offset = self.dimensions.pixel_height as f32 / 2.0;
        quad.set_position(
            x - left_offset,
            y - top_offset,
            x + size - left_offset,
            y + size - top_offset,
        );
        quad.set_texture(sprite);
        quad.set_fg_color(color);
        quad.set_alt_color_and_mix_value(color, 0.0);
        quad.set_hsv(None);
        quad.set_has_color(false);
        quad.set_grayscale();
        Ok(())
    }

    /// Brand/OS logos bake their color into the sprite, so they render
    /// full-color rather than being tinted (unlike [`Self::draw_svg_icon`]).
    pub(crate) fn draw_brand_icon(
        &self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        icon: BrandIcon,
        x: f32,
        y: f32,
        size: f32,
    ) -> anyhow::Result<()> {
        if size <= 0.0 {
            return Ok(());
        }

        let render_state = self.render_state;
        let sprite = render_state
            .glyph_cache
            .borrow_mut()
            .cached_brand_icon(icon, size.round() as usize)?
            .texture_coords();
        let mut quad = layers.allocate(2)?;
        let left_offset = self.dimensions.pixel_width as f32 / 2.0;
        let top_offset = self.dimensions.pixel_height as f32 / 2.0;
        quad.set_position(
            x - left_offset,
            y - top_offset,
            x + size - left_offset,
            y + size - top_offset,
        );
        quad.set_texture(sprite);
        quad.set_hsv(None);
        quad.set_has_color(true);
        quad.set_fg_color(LinearRgba::with_components(1.0, 1.0, 1.0, 1.0));
        Ok(())
    }

    pub(crate) fn draw_text(
        &self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        font: &Rc<LoadedFont>,
        x: f32,
        y: f32,
        text: &str,
        color: LinearRgba,
        max_width: f32,
    ) -> anyhow::Result<()> {
        self.draw_text_on_layer(layers, 1, font, x, y, text, color, max_width)
    }

    pub(crate) fn draw_text_on_layer(
        &self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        layer_num: usize,
        font: &Rc<LoadedFont>,
        x: f32,
        y: f32,
        text: &str,
        color: LinearRgba,
        max_width: f32,
    ) -> anyhow::Result<()> {
        if text.is_empty() || max_width <= 0.0 {
            return Ok(());
        }

        let display_text = self.text_with_ellipsis(font, text, max_width);
        if display_text.is_empty() {
            return Ok(());
        }

        let infos = font.blocking_shape(&display_text, None, Direction::LeftToRight, None, None)?;
        let render_state = self.render_state;
        let mut glyph_cache = render_state.glyph_cache.borrow_mut();
        let style = font.style();
        let mut pos_x = x;
        let baseline = self.metrics.cell_size.height as f32 + self.metrics.descender.get() as f32;
        let left_offset = self.dimensions.pixel_width as f32 / 2.0;
        let top_offset = self.dimensions.pixel_height as f32 / 2.0;
        let right_edge = x + max_width;

        for info in infos {
            let glyph = glyph_cache.cached_glyph(&info, style, false, font, self.metrics, 1)?;
            if let Some(texture) = glyph.texture.as_ref() {
                let glyph_x = (pos_x + (glyph.x_offset + glyph.bearing_x).get() as f32).round();
                let glyph_y =
                    (y - (glyph.y_offset + glyph.bearing_y).get() as f32 + baseline).round();
                let width = texture.coords.size.width as f32 * glyph.scale as f32;
                let height = texture.coords.size.height as f32 * glyph.scale as f32;

                if glyph_x + width > right_edge {
                    break;
                }

                let mut quad = layers.allocate(layer_num)?;
                quad.set_position(
                    glyph_x - left_offset,
                    glyph_y - top_offset,
                    glyph_x + width - left_offset,
                    glyph_y + height - top_offset,
                );
                quad.set_texture(texture.texture_coords());
                quad.set_has_color(glyph.has_color);
                quad.set_fg_color(color);
                quad.set_hsv(None);
            }
            pos_x += glyph.x_advance.get() as f32;
            if pos_x > right_edge {
                break;
            }
        }

        Ok(())
    }

    pub(crate) fn measure_text_width(&self, font: &Rc<LoadedFont>, text: &str) -> f32 {
        if text.is_empty() {
            return 0.0;
        }
        let Ok(infos) = font.blocking_shape(text, None, Direction::LeftToRight, None, None) else {
            return 0.0;
        };
        let render_state = self.render_state;
        let mut glyph_cache = render_state.glyph_cache.borrow_mut();
        let style = font.style();
        infos
            .into_iter()
            .filter_map(|info| {
                glyph_cache
                    .cached_glyph(&info, style, false, font, self.metrics, 1)
                    .ok()
                    .map(|glyph| glyph.x_advance.get() as f32)
            })
            .sum()
    }

    pub(crate) fn text_with_ellipsis(
        &self,
        font: &Rc<LoadedFont>,
        text: &str,
        max_width: f32,
    ) -> String {
        if max_width <= 0.0 || text.is_empty() {
            return String::new();
        }

        if self.measure_text_width(font, text) <= max_width {
            return text.to_string();
        }

        let ellipsis = "...";
        let ellipsis_width = self.measure_text_width(font, ellipsis);
        if ellipsis_width > max_width {
            return String::new();
        }

        let mut boundaries = text
            .char_indices()
            .map(|(idx, _)| idx)
            .chain(std::iter::once(text.len()))
            .collect::<Vec<_>>();
        boundaries.dedup();

        let mut low = 0;
        let mut high = boundaries.len().saturating_sub(1);
        let mut best = 0;
        while low <= high {
            let mid = (low + high) / 2;
            let candidate = &text[..boundaries[mid]];
            let width = self.measure_text_width(font, candidate) + ellipsis_width;
            if width <= max_width {
                best = mid;
                low = mid + 1;
            } else if mid == 0 {
                break;
            } else {
                high = mid - 1;
            }
        }

        format!("{}{}", text[..boundaries[best]].trim_end(), ellipsis)
    }
}
