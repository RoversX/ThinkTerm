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
use crate::quad::{
    HeapQuadAllocator, QuadClipRect, QuadTrait, TripleLayerQuadAllocator,
    TripleLayerQuadAllocatorTrait,
};
use crate::renderstate::RenderState;
use crate::termwindow::render::corners::{
    BOTTOM_LEFT_ROUNDED_CORNER, BOTTOM_LEFT_ROUNDED_CORNER_MASK,
    BOTTOM_LEFT_ROUNDED_CORNER_OUTLINE, BOTTOM_RIGHT_ROUNDED_CORNER,
    BOTTOM_RIGHT_ROUNDED_CORNER_MASK, BOTTOM_RIGHT_ROUNDED_CORNER_OUTLINE, TOP_LEFT_ROUNDED_CORNER,
    TOP_LEFT_ROUNDED_CORNER_MASK, TOP_LEFT_ROUNDED_CORNER_OUTLINE, TOP_RIGHT_ROUNDED_CORNER,
    TOP_RIGHT_ROUNDED_CORNER_MASK, TOP_RIGHT_ROUNDED_CORNER_OUTLINE,
};
use crate::termwindow::ui::icons::{BrandIcon, SvgIcon};
use crate::ui::{UiPalette, UiTokens};
use crate::utilsprites::RenderMetrics;
use std::rc::Rc;
use wezterm_bidi::Direction;
use wezterm_font::LoadedFont;
use window::bitmaps::TextureRect;
use window::color::LinearRgba;
use window::{Dimensions, RectF};

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

    /// The same surface measured with another font's metrics.
    ///
    /// Text lands on the baseline of whichever metrics the *context* carries,
    /// not the font passed to `draw_text` -- so a heading drawn in a larger
    /// font through the body context sits on the body baseline and looks
    /// misaligned. Re-borrow with the heading's metrics instead.
    pub(crate) fn with_metrics<'m>(&self, metrics: &'m RenderMetrics) -> DrawContext<'m>
    where
        'a: 'm,
    {
        DrawContext {
            render_state: self.render_state,
            dimensions: self.dimensions,
            metrics,
        }
    }

    /// Design pixels (2x macOS backing) → this window's backing pixels.
    pub(crate) fn px(&self, value: f32) -> f32 {
        crate::ui::scale_ui_f32(value, self.dimensions.dpi)
    }

    /// Design-pixel -> physical-pixel ratio for this surface. Use it when a
    /// helper needs the raw factor (e.g. to stay unit-testable); prefer
    /// [`Self::px`] for one-off conversions.
    pub(crate) fn scale(&self) -> f32 {
        crate::ui::ui_scale_for_dpi(self.dimensions.dpi)
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

    /// Draw a true GPU-interpolated gradient using a single quad. This keeps
    /// full-window native UI backgrounds smooth without uploading a texture or
    /// exposing visible color bands on large displays.
    pub(crate) fn draw_vertical_gradient(
        &self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        layer_num: usize,
        area: RectF,
        top: LinearRgba,
        bottom: LinearRgba,
    ) -> anyhow::Result<()> {
        if area.size.width <= 0.0 || area.size.height <= 0.0 {
            return Ok(());
        }

        let mut quad = layers.allocate(layer_num)?;
        let left_offset = self.dimensions.pixel_width as f32 / 2.0;
        let top_offset = self.dimensions.pixel_height as f32 / 2.0;
        quad.set_position(
            area.origin.x - left_offset,
            area.origin.y - top_offset,
            area.max_x() - left_offset,
            area.max_y() - top_offset,
        );
        quad.set_texture(self.render_state.util_sprites.filled_box.texture_coords());
        quad.set_is_background();
        quad.set_vertical_gradient(top, bottom);
        quad.set_alt_color_and_mix_value(top, 0.0);
        quad.set_hsv(None);
        Ok(())
    }

    /// Draw a smoothly interpolated four-corner gradient with one GPU quad.
    /// This is intended for restrained native surfaces where a flat vertical
    /// blend is visually too uniform but a texture or blur would be wasteful.
    pub(crate) fn draw_corner_gradient(
        &self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        layer_num: usize,
        area: RectF,
        top_left: LinearRgba,
        top_right: LinearRgba,
        bottom_left: LinearRgba,
        bottom_right: LinearRgba,
    ) -> anyhow::Result<()> {
        if area.size.width <= 0.0 || area.size.height <= 0.0 {
            return Ok(());
        }

        let mut quad = layers.allocate(layer_num)?;
        let left_offset = self.dimensions.pixel_width as f32 / 2.0;
        let top_offset = self.dimensions.pixel_height as f32 / 2.0;
        quad.set_position(
            area.origin.x - left_offset,
            area.origin.y - top_offset,
            area.max_x() - left_offset,
            area.max_y() - top_offset,
        );
        quad.set_texture(self.render_state.util_sprites.filled_box.texture_coords());
        quad.set_is_background();
        quad.set_corner_gradient(top_left, top_right, bottom_left, bottom_right);
        quad.set_alt_color_and_mix_value(top_left, 0.0);
        quad.set_hsv(None);
        Ok(())
    }

    /// Standard elevated surface used by overview cards. Two restrained
    /// layers provide depth without a hard concentric halo.
    /// A soft shadow under a rounded rectangle: the blurred silhouette from
    /// the atlas, drawn as nine slices. `sigma` is the blur's standard
    /// deviation and `offset_y` how far the shadow is dropped, both in
    /// pixels; the colour's alpha is the shadow's peak opacity.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn draw_shadow(
        &self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        layer_num: usize,
        rect: RectF,
        radius: f32,
        sigma: f32,
        offset_y: f32,
        color: LinearRgba,
    ) -> anyhow::Result<()> {
        let Some(snapped) = pixel_snap_rounded_rect(
            rect.origin.x,
            rect.origin.y + offset_y,
            rect.size.width,
            rect.size.height,
            radius,
        ) else {
            return Ok(());
        };
        let sigma = sigma.round().clamp(0.0, 64.0);
        if color.3 <= 0.0 || (sigma <= 0.0 && snapped.radius <= 0.0) {
            return Ok(());
        }
        let key = crate::ui::shadow::ShadowKey {
            radius: snapped.radius.round().clamp(0.0, 128.0) as u16,
            sigma: sigma as u16,
        };
        let layout = key.layout();
        let coords = self
            .render_state
            .glyph_cache
            .borrow_mut()
            .cached_shadow(key)?
            .texture_coords();
        let side = layout.side as f32;
        let u = |px: f32| coords.min_x() + coords.size.width * (px / side);
        let v = |px: f32| coords.min_y() + coords.size.height * (px / side);
        let left_offset = self.dimensions.pixel_width as f32 / 2.0;
        let top_offset = self.dimensions.pixel_height as f32 / 2.0;
        for slice in crate::ui::shadow::shadow_slices(
            snapped.x,
            snapped.y,
            snapped.width,
            snapped.height,
            layout,
        ) {
            let mut quad = layers.allocate(layer_num)?;
            quad.set_position(
                slice.screen_x.0 - left_offset,
                slice.screen_y.0 - top_offset,
                slice.screen_x.1 - left_offset,
                slice.screen_y.1 - top_offset,
            );
            quad.set_texture_discrete(
                u(slice.sprite_x.0),
                u(slice.sprite_x.1),
                v(slice.sprite_y.0),
                v(slice.sprite_y.1),
            );
            quad.set_fg_color(color);
            quad.set_alt_color_and_mix_value(color, 0.0);
            quad.set_hsv(None);
            quad.set_has_color(false);
            quad.set_grayscale();
        }
        Ok(())
    }

    pub(crate) fn draw_elevated_surface(
        &self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        layer_num: usize,
        rect: RectF,
        fill: LinearRgba,
        border: LinearRgba,
        shadow: LinearRgba,
        radius: f32,
    ) -> anyhow::Result<()> {
        // Two shadows, as a lit surface casts: a wide, faint ambient one
        // and a tight, darker one close under the edge.
        self.draw_shadow(
            layers,
            layer_num,
            rect,
            radius,
            self.px(9.0),
            self.px(6.0),
            color_with_alpha(shadow, shadow.3 * 0.42),
        )?;
        self.draw_shadow(
            layers,
            layer_num,
            rect,
            radius,
            self.px(2.0),
            self.px(1.5),
            color_with_alpha(shadow, shadow.3 * 0.28),
        )?;
        self.draw_rounded_frame(
            layers,
            layer_num,
            rect.origin.x,
            rect.origin.y,
            rect.size.width,
            rect.size.height,
            fill,
            border,
            radius,
        )
    }

    /// Cover the four wedges outside a rounded preview after the terminal has
    /// been painted, then redraw its thin outline. The complete chrome is
    /// emitted into a heap buffer and clipped as quads, so a corner crossing a
    /// scroll boundary remains rounded instead of disappearing all at once.
    pub(crate) fn draw_rounded_preview_chrome(
        &self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        layer_num: usize,
        rect: RectF,
        clip: RectF,
        mask: LinearRgba,
        border: LinearRgba,
        radius: f32,
    ) -> anyhow::Result<()> {
        let radius = radius
            .min(rect.size.width / 2.0)
            .min(rect.size.height / 2.0)
            .round()
            .max(0.0);
        if radius <= 0.0 {
            return Ok(());
        }
        let size = euclid::size2(radius, radius);

        let corners = [
            (
                rect.min_x(),
                rect.min_y(),
                TOP_LEFT_ROUNDED_CORNER_MASK,
                TOP_LEFT_ROUNDED_CORNER_OUTLINE,
            ),
            (
                rect.max_x() - radius,
                rect.min_y(),
                TOP_RIGHT_ROUNDED_CORNER_MASK,
                TOP_RIGHT_ROUNDED_CORNER_OUTLINE,
            ),
            (
                rect.min_x(),
                rect.max_y() - radius,
                BOTTOM_LEFT_ROUNDED_CORNER_MASK,
                BOTTOM_LEFT_ROUNDED_CORNER_OUTLINE,
            ),
            (
                rect.max_x() - radius,
                rect.max_y() - radius,
                BOTTOM_RIGHT_ROUNDED_CORNER_MASK,
                BOTTOM_RIGHT_ROUNDED_CORNER_OUTLINE,
            ),
        ];
        let mut heap = HeapQuadAllocator::default();
        {
            let mut clipped_layers = TripleLayerQuadAllocator::Heap(&mut heap);
            for (x, y, mask_poly, outline_poly) in corners {
                self.draw_corner(&mut clipped_layers, layer_num, x, y, mask_poly, size, mask)?;
                self.draw_corner(
                    &mut clipped_layers,
                    layer_num,
                    x,
                    y,
                    outline_poly,
                    size,
                    border,
                )?;
            }

            let stroke = self.px(1.0).max(1.0);
            let edges: [RectF; 4] = [
                euclid::rect(
                    rect.min_x() + radius,
                    rect.min_y(),
                    rect.size.width - radius * 2.0,
                    stroke,
                ),
                euclid::rect(
                    rect.min_x() + radius,
                    rect.max_y() - stroke,
                    rect.size.width - radius * 2.0,
                    stroke,
                ),
                euclid::rect(
                    rect.min_x(),
                    rect.min_y() + radius,
                    stroke,
                    rect.size.height - radius * 2.0,
                ),
                euclid::rect(
                    rect.max_x() - stroke,
                    rect.min_y() + radius,
                    stroke,
                    rect.size.height - radius * 2.0,
                ),
            ];
            for edge in edges {
                self.draw_rect(
                    &mut clipped_layers,
                    layer_num,
                    edge.origin.x,
                    edge.origin.y,
                    edge.size.width,
                    edge.size.height,
                    border,
                )?;
            }
        }
        let clip = QuadClipRect::from_top_left_pixels(
            clip.min_x(),
            clip.min_y(),
            clip.max_x(),
            clip.max_y(),
            &self.dimensions,
        );
        heap.apply_to_clipped(layers, clip, 1.0)
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
        let Some(rect) = pixel_snap_rounded_rect(x, y, width, height, radius) else {
            return Ok(());
        };
        let PixelSnappedRoundedRect {
            x,
            y,
            width,
            height,
            radius,
        } = rect;

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

    /// A rounded rectangle filled from `top` at its top edge to `bottom` at
    /// its bottom one. It is drawn in the same pieces as `draw_rounded_rect`;
    /// each piece takes the colours of the rows it spans, so the GPU's
    /// interpolation joins them into one ramp.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn draw_rounded_rect_vertical_gradient(
        &self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        layer_num: usize,
        rect: RectF,
        radius: f32,
        top: LinearRgba,
        bottom: LinearRgba,
    ) -> anyhow::Result<()> {
        let Some(PixelSnappedRoundedRect {
            x,
            y,
            width,
            height,
            radius,
        }) = pixel_snap_rounded_rect(
            rect.origin.x,
            rect.origin.y,
            rect.size.width,
            rect.size.height,
            radius,
        )
        else {
            return Ok(());
        };
        let at = |row: f32| {
            let t = ((row - y) / height).clamp(0.0, 1.0);
            LinearRgba::with_components(
                top.0 + (bottom.0 - top.0) * t,
                top.1 + (bottom.1 - top.1) * t,
                top.2 + (bottom.2 - top.2) * t,
                top.3 + (bottom.3 - top.3) * t,
            )
        };

        let lower = y + height - radius;
        if radius > 0.0 {
            let size = euclid::size2(radius, radius);
            let left_offset = self.dimensions.pixel_width as f32 / 2.0;
            let top_offset = self.dimensions.pixel_height as f32 / 2.0;
            for (polys, corner_x, corner_y) in [
                (TOP_LEFT_ROUNDED_CORNER, x, y),
                (TOP_RIGHT_ROUNDED_CORNER, x + width - radius, y),
                (BOTTOM_LEFT_ROUNDED_CORNER, x, lower),
                (BOTTOM_RIGHT_ROUNDED_CORNER, x + width - radius, lower),
            ] {
                let sprite = self.corner_sprite(polys, size)?;
                let mut quad = layers.allocate(layer_num)?;
                quad.set_position(
                    corner_x - left_offset,
                    corner_y - top_offset,
                    corner_x + radius - left_offset,
                    corner_y + radius - top_offset,
                );
                quad.set_texture(sprite);
                quad.set_vertical_gradient(at(corner_y), at(corner_y + radius));
                quad.set_alt_color_and_mix_value(at(corner_y), 0.0);
                quad.set_hsv(None);
                quad.set_has_color(false);
                quad.set_grayscale();
            }
        }

        // The column between the corners runs the full height; the bands
        // beside it only between them. Empty ones draw nothing.
        self.draw_vertical_gradient(
            layers,
            layer_num,
            euclid::rect(x + radius, y, width - radius * 2.0, height),
            top,
            bottom,
        )?;
        for band_x in [x, x + width - radius] {
            self.draw_vertical_gradient(
                layers,
                layer_num,
                euclid::rect(band_x, y + radius, radius, lower - (y + radius)),
                at(y + radius),
                at(lower),
            )?;
        }
        Ok(())
    }

    /// A grouped card: the translucent surface a panel's contents sit on.
    /// One call, so every page groups things the same way.
    pub(crate) fn draw_card(
        &self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        layer_num: usize,
        rect: RectF,
        palette: UiPalette,
        tokens: UiTokens,
    ) -> anyhow::Result<()> {
        self.draw_rounded_frame(
            layers,
            layer_num,
            rect.origin.x,
            rect.origin.y,
            rect.size.width,
            rect.size.height,
            palette.card_bg,
            palette.separator,
            tokens.card_radius,
        )
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
        draw_rounded_frame(
            self, layers, layer_num, x, y, width, height, fill, border, radius,
        )
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
        let sprite = self.corner_sprite(polys, size)?;

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

    /// A rounded corner's coverage in the atlas, `size` pixels square.
    fn corner_sprite(
        &self,
        polys: &'static [Poly],
        size: euclid::Size2D<f32, window::PixelUnit>,
    ) -> anyhow::Result<TextureRect> {
        Ok(self
            .render_state
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
            .texture_coords())
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

fn color_with_alpha(color: LinearRgba, alpha: f32) -> LinearRgba {
    LinearRgba(color.0, color.1, color.2, alpha.clamp(0.0, 1.0))
}

/// Rounded corners are rasterized as separate integer-sized sprites. Keep the
/// rectangle that joins those sprites on the same physical-pixel grid or a
/// fractional DPI scale can leave a one-pixel gap between the two halves of a
/// pill (most visibly through the knob of a switch).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct PixelSnappedRoundedRect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    pub radius: f32,
}

pub(crate) fn pixel_snap_rounded_rect(
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    radius: f32,
) -> Option<PixelSnappedRoundedRect> {
    if !x.is_finite()
        || !y.is_finite()
        || !width.is_finite()
        || !height.is_finite()
        || !radius.is_finite()
        || width <= 0.0
        || height <= 0.0
    {
        return None;
    }

    let right = (x + width).round();
    let bottom = (y + height).round();
    let x = x.round();
    let y = y.round();
    let width = right - x;
    let height = bottom - y;
    if width <= 0.0 || height <= 0.0 {
        return None;
    }

    let max_radius = (width / 2.0).floor().min((height / 2.0).floor());
    Some(PixelSnappedRoundedRect {
        x,
        y,
        width,
        height,
        radius: radius.round().clamp(0.0, max_radius),
    })
}

/// The primitives [`draw_rounded_frame`] is built from. Two painters supply
/// them and cannot share a draw path -- [`DrawContext`] for the in-window
/// pages, the settings window for its own -- so they share the frame's
/// geometry through this instead of keeping a copy of it each. The names are
/// deliberately not the painters' own, so that implementing this cannot
/// shadow the inherent methods it forwards to.
pub(crate) trait RoundedFramePainter {
    fn frame_rounded_rect(
        &self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        layer_num: usize,
        x: f32,
        y: f32,
        width: f32,
        height: f32,
        color: LinearRgba,
        radius: f32,
    ) -> anyhow::Result<()>;

    fn frame_rect(
        &self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        layer_num: usize,
        x: f32,
        y: f32,
        width: f32,
        height: f32,
        color: LinearRgba,
    ) -> anyhow::Result<()>;

    fn frame_corner(
        &self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        layer_num: usize,
        x: f32,
        y: f32,
        polys: &'static [Poly],
        size: euclid::Size2D<f32, window::PixelUnit>,
        color: LinearRgba,
    ) -> anyhow::Result<()>;
}

/// A filled rounded rect with a one-pixel ring on its edge.
///
/// Fill first, then the ring. This used to paint the border across the whole
/// rect and cover it with the fill inset by a pixel, which only works when
/// the fill is opaque. Every translucent one -- a card at 0.78 alpha, a
/// control at 0.98 -- let the border colour through across the entire
/// interior, so selecting a card tinted the whole card with the accent
/// instead of just outlining it.
///
/// A border the same colour as the fill is not a border: stroking it anyway
/// draws the antialiased edge twice, which hardens the outline into a visible
/// ring around a filled button and chews the corners of a pill. Passing the
/// fill as the border is how a caller asks for no ring at all.
pub(crate) fn draw_rounded_frame(
    painter: &impl RoundedFramePainter,
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
    let Some(rect) = pixel_snap_rounded_rect(x, y, width, height, radius) else {
        return Ok(());
    };
    let PixelSnappedRoundedRect {
        x,
        y,
        width,
        height,
        radius,
    } = rect;
    if fill.3 > 0.0 {
        painter.frame_rounded_rect(layers, layer_num, x, y, width, height, fill, radius)?;
    }
    if border.3 <= 0.0 || border == fill {
        return Ok(());
    }
    const STROKE: f32 = 1.0;
    if radius > 0.0 {
        let size = euclid::size2(radius, radius);
        for (cx, cy, poly) in [
            (x, y, TOP_LEFT_ROUNDED_CORNER_OUTLINE),
            (x + width - radius, y, TOP_RIGHT_ROUNDED_CORNER_OUTLINE),
            (x, y + height - radius, BOTTOM_LEFT_ROUNDED_CORNER_OUTLINE),
            (
                x + width - radius,
                y + height - radius,
                BOTTOM_RIGHT_ROUNDED_CORNER_OUTLINE,
            ),
        ] {
            painter.frame_corner(layers, layer_num, cx, cy, poly, size, border)?;
        }
    }
    let straight_w = (width - radius * 2.0).max(0.0);
    let straight_h = (height - radius * 2.0).max(0.0);
    painter.frame_rect(layers, layer_num, x + radius, y, straight_w, STROKE, border)?;
    painter.frame_rect(
        layers,
        layer_num,
        x + radius,
        y + height - STROKE,
        straight_w,
        STROKE,
        border,
    )?;
    painter.frame_rect(layers, layer_num, x, y + radius, STROKE, straight_h, border)?;
    painter.frame_rect(
        layers,
        layer_num,
        x + width - STROKE,
        y + radius,
        STROKE,
        straight_h,
        border,
    )?;
    Ok(())
}

impl RoundedFramePainter for DrawContext<'_> {
    fn frame_rounded_rect(
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
        self.draw_rounded_rect(layers, layer_num, x, y, width, height, color, radius)
    }

    fn frame_rect(
        &self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        layer_num: usize,
        x: f32,
        y: f32,
        width: f32,
        height: f32,
        color: LinearRgba,
    ) -> anyhow::Result<()> {
        self.draw_rect(layers, layer_num, x, y, width, height, color)
    }

    fn frame_corner(
        &self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        layer_num: usize,
        x: f32,
        y: f32,
        polys: &'static [Poly],
        size: euclid::Size2D<f32, window::PixelUnit>,
        color: LinearRgba,
    ) -> anyhow::Result<()> {
        self.draw_corner(layers, layer_num, x, y, polys, size, color)
    }
}

#[cfg(test)]
mod tests {
    use super::pixel_snap_rounded_rect;

    #[test]
    fn fractional_dpi_pill_keeps_a_filled_center_strip() {
        // A 36 design-pixel switch knob is 40.5 physical pixels at 225%.
        // The old path rounded each 20.25px corner to 20px but kept the
        // fractional outer width, leaving one uncovered column in the middle.
        let rect = pixel_snap_rounded_rect(1939.0, 1036.0, 40.5, 40.5, 20.25).unwrap();
        assert_eq!(rect.x, 1939.0);
        assert_eq!(rect.y, 1036.0);
        assert_eq!(rect.width, 41.0);
        assert_eq!(rect.height, 41.0);
        assert_eq!(rect.radius, 20.0);
        assert_eq!(rect.width - rect.radius * 2.0, 1.0);
    }

    #[test]
    fn common_windows_scales_produce_integral_geometry() {
        for dpi in [96.0_f32, 120.0, 144.0, 168.0, 192.0, 216.0, 240.0, 288.0] {
            let scale = dpi / 192.0;
            let rect =
                pixel_snap_rounded_rect(117.25, 209.75, 76.0 * scale, 44.0 * scale, 22.0 * scale)
                    .unwrap();

            assert_eq!(rect.x.fract(), 0.0);
            assert_eq!(rect.y.fract(), 0.0);
            assert_eq!(rect.width.fract(), 0.0);
            assert_eq!(rect.height.fract(), 0.0);
            assert_eq!(rect.radius.fract(), 0.0);
            assert!(rect.width - rect.radius * 2.0 >= 0.0);
            assert!(rect.height - rect.radius * 2.0 >= 0.0);
        }
    }
}
