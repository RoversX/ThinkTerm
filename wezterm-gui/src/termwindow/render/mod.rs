use crate::colorease::ColorEase;
use crate::customglyph::{BlockKey, *};
use crate::glyphcache::{CachedGlyph, GlyphCache};
use crate::quad::{
    HeapQuadAllocator, Quad, QuadAllocator, QuadImpl, QuadTrait, TripleLayerQuadAllocator,
    TripleLayerQuadAllocatorTrait, Vertex,
};
use crate::renderstate::{
    image_fits_dedicated_texture, image_wants_dedicated_texture, RenderContext,
};
use crate::shapecache::*;
use crate::termwindow::render::paint::AllowImage;
use crate::termwindow::render::paint::ImageCompositeSlot;
use crate::termwindow::webgpu::ImageTexture;
use crate::termwindow::{
    BorrowedShapeCacheKey, MouseCapture, RenderState, ShapedInfo, TermWindowNotif,
};
use crate::utilsprites::RenderMetrics;
use ::window::bitmaps::{TextureCoord, TextureRect, TextureSize};
use ::window::{DeadKeyStatus, PointF, RectF, SizeF, WindowOps};
use anyhow::{anyhow, Context};
use config::{
    BoldBrightening, ConfigHandle, DimensionContext, HorizontalWindowContentAlignment, TextStyle,
    VerticalWindowContentAlignment, VisualBellTarget,
};
use euclid::num::Zero;
use mux::pane::{Pane, PaneId};
use mux::renderable::{RenderableDimensions, StableCursorPosition};
use ordered_float::NotNan;
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;
use termwiz::cellcluster::CellCluster;
use termwiz::hyperlink::Hyperlink;
use termwiz::surface::{CursorShape, CursorVisibility, SequenceNo};
use wezterm_font::shaper::PresentationWidth;
use wezterm_font::units::{IntPixelLength, PixelLength};
use wezterm_font::{ClearShapeCache, FontConfiguration, GlyphInfo, LoadedFont};
use wezterm_term::color::{ColorAttribute, ColorPalette};
use wezterm_term::{CellAttributes, Line, StableRowIndex};
use window::color::LinearRgba;

pub mod borders;
pub mod corners;
pub mod draw;
pub mod fancy_tab_bar;
pub mod paint;
pub mod pane;
pub mod screen_line;
pub mod split;
pub mod tab_bar;
pub mod window_buttons;

/// The data that we associate with a line; we use this to cache it shape hash
#[derive(Debug)]
pub struct CachedLineState {
    pub id: u64,
    pub seqno: SequenceNo,
    pub shape_hash: [u8; 16],
}

#[derive(Debug, Hash, Clone, PartialEq, Eq)]
pub struct LineQuadCacheKey {
    pub config_generation: usize,
    pub shape_generation: usize,
    pub quad_generation: usize,
    /// Only set if cursor.y == stable_row
    pub composing: Option<String>,
    pub selection: Range<usize>,
    pub shape_hash: [u8; 16],
    pub font_identity: u64,
    pub top_pixel_y: NotNan<f32>,
    pub left_pixel_x: NotNan<f32>,
    /// The same terminal line can be painted at a different width while a
    /// split divider is being dragged.  Keep that geometry in the key so the
    /// old, wider GPU quads can never be replayed across the new divider.
    pub render_cols: usize,
    pub render_pixel_width: usize,
    pub phys_line_idx: usize,
    pub pane_id: PaneId,
    pub pane_is_active: bool,
    /// A cursor position with the y value fixed at 0.
    /// Only is_some() if the y value matches this row.
    pub cursor: Option<CursorProperties>,
    pub reverse_video: bool,
    pub password_input: bool,
}

pub struct LineQuadCacheValue {
    pub expires: Option<Instant>,
    pub layers: HeapQuadAllocator,
    // Only set if the line contains any hyperlinks, so
    // that we can invalidate when it changes
    pub current_highlight: Option<Arc<Hyperlink>>,
    pub invalidate_on_hover_change: bool,
}

pub struct LineToElementParams<'a> {
    pub line: &'a Line,
    pub config: &'a ConfigHandle,
    pub palette: &'a ColorPalette,
    pub window_is_transparent: bool,
    pub reverse_video: bool,
    pub shape_key: &'a Option<LineToEleShapeCacheKey>,
    pub font: Option<&'a Rc<LoadedFont>>,
    pub style: Option<&'a TextStyle>,
    pub font_config: Option<&'a Rc<FontConfiguration>>,
    pub render_metrics: RenderMetrics,
    pub font_identity: u64,
    pub simple_shaping: bool,
}

#[derive(Debug, Hash, PartialEq, Eq, Clone)]
pub struct LineToEleShapeCacheKey {
    pub shape_hash: [u8; 16],
    pub composing: Option<(usize, String)>,
    pub shape_generation: usize,
    pub font_identity: u64,
}

pub struct LineToElementShapeItem {
    pub expires: Option<Instant>,
    pub shaped: Rc<Vec<LineToElementShape>>,
    // Only set if the line contains any hyperlinks, so
    // that we can invalidate when it changes
    pub current_highlight: Option<Arc<Hyperlink>>,
    pub invalidate_on_hover_change: bool,
}

pub struct LineToElementShape {
    pub underline_tex_rect: TextureRect,
    pub fg_color: LinearRgba,
    pub bg_color: LinearRgba,
    pub underline_color: LinearRgba,
    pub x_pos: f32,
    pub pixel_width: f32,
    pub glyph_info: Rc<Vec<ShapedInfo>>,
    pub cluster: CellCluster,
}

pub struct RenderScreenLineResult {
    pub invalidate_on_hover_change: bool,
}

/// Per-entry bookkeeping cost of the LFU cache itself (the shared `Rc`
/// entry holding the intrusive links), matching the constant used by the
/// UI shape caches in `shapecache.rs`.
const CACHE_ENTRY_FIXED_OVERHEAD: usize = 128;

/// Estimated resident bytes for one `line_state_cache` entry: the
/// `Arc<CachedLineState>` allocation plus the u64 key.
pub const LINE_STATE_ENTRY_BYTES: usize =
    CACHE_ENTRY_FIXED_OVERHEAD + std::mem::size_of::<CachedLineState>() + 24;

/// Estimated resident bytes for one `line_quad_cache` entry. The dominant
/// term is the `HeapQuadAllocator` capacity.
pub fn estimate_line_quad_entry_bytes(key: &LineQuadCacheKey, value: &LineQuadCacheValue) -> usize {
    CACHE_ENTRY_FIXED_OVERHEAD
        .saturating_add(std::mem::size_of::<LineQuadCacheKey>())
        .saturating_add(key.composing.as_ref().map_or(0, |s| s.capacity()))
        .saturating_add(std::mem::size_of::<LineQuadCacheValue>())
        .saturating_add(value.layers.resident_bytes())
}

/// Estimated resident bytes for one `line_to_ele_shape_cache` entry. The
/// `glyph_info` Rc inside each shape is shared with (and counted by) the
/// `shape_cache` entry that minted it, so only each shape's inline size and
/// its cluster's own heap are counted here.
pub fn estimate_line_to_ele_entry_bytes(
    key: &LineToEleShapeCacheKey,
    value: &LineToElementShapeItem,
) -> usize {
    let key_bytes = std::mem::size_of::<LineToEleShapeCacheKey>()
        .saturating_add(key.composing.as_ref().map_or(0, |(_, s)| s.capacity()));
    let shaped_bytes = std::mem::size_of::<Vec<LineToElementShape>>()
        .saturating_add(
            value
                .shaped
                .capacity()
                .saturating_mul(std::mem::size_of::<LineToElementShape>()),
        )
        .saturating_add(
            value
                .shaped
                .iter()
                .map(|shape| shape.cluster.resident_heap_bytes())
                .sum(),
        );
    CACHE_ENTRY_FIXED_OVERHEAD
        .saturating_add(key_bytes)
        .saturating_add(std::mem::size_of::<LineToElementShapeItem>())
        .saturating_add(shaped_bytes)
}

pub struct RenderScreenLineParams<'a> {
    /// zero-based offset from top of the window viewport to the line that
    /// needs to be rendered, measured in pixels
    pub top_pixel_y: f32,
    /// zero-based offset from left of the window viewport to the line that
    /// needs to be rendered, measured in pixels
    pub left_pixel_x: f32,
    pub pixel_width: f32,
    pub stable_line_idx: Option<StableRowIndex>,
    pub line: &'a Line,
    pub selection: Range<usize>,
    pub cursor: &'a StableCursorPosition,
    pub palette: &'a ColorPalette,
    pub dims: &'a RenderableDimensions,
    pub config: &'a ConfigHandle,
    pub pane: Option<&'a Arc<dyn Pane>>,

    pub white_space: TextureRect,
    pub filled_box: TextureRect,

    pub cursor_border_color: LinearRgba,
    pub foreground: LinearRgba,
    pub is_active: bool,

    pub selection_fg: LinearRgba,
    pub selection_bg: LinearRgba,
    pub cursor_fg: LinearRgba,
    pub cursor_bg: LinearRgba,
    pub cursor_is_default_color: bool,

    pub window_is_transparent: bool,
    pub default_bg: LinearRgba,

    /// Override font resolution; useful together with
    /// the resolved title font
    pub font: Option<Rc<LoadedFont>>,
    pub style: Option<&'a TextStyle>,

    /// If true, use the shaper-determined pixel positions,
    /// rather than using monospace cell based positions.
    pub use_pixel_positioning: bool,

    pub render_metrics: RenderMetrics,
    pub font_config: Option<Rc<FontConfiguration>>,
    pub font_identity: u64,
    pub shape_key: Option<LineToEleShapeCacheKey>,
    pub password_input: bool,

    /// Whether terminal image cells should be emitted for this render pass.
    /// Live Overview keeps the source line untouched and suppresses only the
    /// image quads; regular terminal painting enables them.
    pub allow_images: bool,

    /// Shape one cell at a time through the per-grapheme shape cache instead
    /// of handing whole same-attribute runs to the shaper. Ligatures and
    /// kerning are given up, which is invisible at thumbnail cell sizes; what
    /// is bought is a cache that actually hits on content like btop, whose
    /// full-run strings never repeat but whose individual characters always
    /// do. Live Overview previews only.
    pub simple_shaping: bool,
}

#[derive(Debug, Hash, PartialEq, Eq, Clone)]
pub struct CursorProperties {
    pub position: StableCursorPosition,
    pub dead_key_or_leader: bool,
    pub cursor_is_default_color: bool,
    pub cursor_fg: LinearRgba,
    pub cursor_bg: LinearRgba,
    pub cursor_border_color: LinearRgba,
}

pub struct ComputeCellFgBgParams<'a> {
    pub selected: bool,
    pub cursor: Option<&'a StableCursorPosition>,
    pub fg_color: LinearRgba,
    pub bg_color: LinearRgba,
    pub is_active_pane: bool,
    pub config: &'a ConfigHandle,
    pub selection_fg: LinearRgba,
    pub selection_bg: LinearRgba,
    pub cursor_fg: LinearRgba,
    pub cursor_bg: LinearRgba,
    pub cursor_is_default_color: bool,
    pub cursor_border_color: LinearRgba,
    pub pane: Option<&'a Arc<dyn Pane>>,
}

#[derive(Debug)]
pub struct ComputeCellFgBgResult {
    pub fg_color: LinearRgba,
    pub fg_color_alt: LinearRgba,
    pub bg_color: LinearRgba,
    pub bg_color_alt: LinearRgba,
    pub fg_color_mix: f32,
    pub bg_color_mix: f32,
    pub cursor_border_color: LinearRgba,
    pub cursor_border_color_alt: LinearRgba,
    pub cursor_border_mix: f32,
    pub cursor_shape: Option<CursorShape>,
}

/// Basic cache of computed data from prior cluster to avoid doing the same
/// work for space separated clusters with the same style
#[derive(Clone, Debug)]
pub struct ClusterStyleCache<'a> {
    attrs: &'a CellAttributes,
    style: &'a TextStyle,
    underline_tex_rect: TextureRect,
    fg_color: LinearRgba,
    bg_color: LinearRgba,
    underline_color: LinearRgba,
}

impl crate::TermWindow {
    pub fn update_next_frame_time(&self, next_due: Option<Instant>) {
        if next_due.is_some() {
            update_next_frame_time(&mut *self.has_animation.borrow_mut(), next_due);
        }
    }

    fn get_intensity_if_bell_target_ringing(
        &self,
        pane: &Arc<dyn Pane>,
        config: &ConfigHandle,
        target: VisualBellTarget,
    ) -> Option<f32> {
        let mut per_pane = self.pane_state(pane.pane_id());
        if let Some(ringing) = per_pane.bell_start {
            if config.visual_bell.target == target {
                let mut color_ease = ColorEase::new(
                    config.visual_bell.fade_in_duration_ms,
                    config.visual_bell.fade_in_function,
                    config.visual_bell.fade_out_duration_ms,
                    config.visual_bell.fade_out_function,
                    Some(ringing),
                );

                let intensity = color_ease.intensity_one_shot();

                match intensity {
                    None => {
                        per_pane.bell_start.take();
                    }
                    Some((intensity, next)) => {
                        self.update_next_frame_time(Some(next));
                        return Some(intensity);
                    }
                }
            }
        }
        None
    }

    pub fn filled_rectangle<'a>(
        &self,
        layers: &'a mut TripleLayerQuadAllocator,
        layer_num: usize,
        rect: RectF,
        color: LinearRgba,
    ) -> anyhow::Result<QuadImpl<'a>> {
        let mut quad = layers.allocate(layer_num)?;
        let left_offset = self.dimensions.pixel_width as f32 / 2.;
        let top_offset = self.dimensions.pixel_height as f32 / 2.;
        let gl_state = self.render_state.as_ref().unwrap();
        quad.set_position(
            rect.min_x() as f32 - left_offset,
            rect.min_y() as f32 - top_offset,
            rect.max_x() as f32 - left_offset,
            rect.max_y() as f32 - top_offset,
        );
        quad.set_texture(gl_state.util_sprites.filled_box.texture_coords());
        quad.set_is_background();
        quad.set_fg_color(color);
        quad.set_hsv(None);
        Ok(quad)
    }

    pub(crate) fn is_pointer_over_ui_rect(
        &self,
        x: usize,
        y: usize,
        width: usize,
        height: usize,
    ) -> bool {
        if width == 0 || height == 0 {
            return false;
        }
        if !matches!(self.current_mouse_capture, None | Some(MouseCapture::UI)) {
            return false;
        }
        let Some(event) = &self.current_mouse_event else {
            return false;
        };
        let mouse_x = event.coords.x;
        let mouse_y = event.coords.y;
        mouse_x >= x as isize
            && mouse_x < x.saturating_add(width) as isize
            && mouse_y >= y as isize
            && mouse_y < y.saturating_add(height) as isize
    }

    pub(crate) fn is_pointer_pressing_ui_rect(
        &self,
        x: usize,
        y: usize,
        width: usize,
        height: usize,
    ) -> bool {
        self.is_pointer_over_ui_rect(x, y, width, height) && !self.current_mouse_buttons.is_empty()
    }

    pub fn poly_quad<'a>(
        &self,
        layers: &'a mut TripleLayerQuadAllocator,
        layer_num: usize,
        point: PointF,
        polys: &'static [Poly],
        underline_height: IntPixelLength,
        cell_size: SizeF,
        color: LinearRgba,
    ) -> anyhow::Result<QuadImpl<'a>> {
        let left_offset = self.dimensions.pixel_width as f32 / 2.;
        let top_offset = self.dimensions.pixel_height as f32 / 2.;
        let gl_state = self.render_state.as_ref().unwrap();
        let sprite = gl_state
            .glyph_cache
            .borrow_mut()
            .cached_block(
                BlockKey::PolyWithCustomMetrics {
                    polys,
                    underline_height,
                    cell_size: euclid::size2(cell_size.width as isize, cell_size.height as isize),
                },
                &self.render_metrics,
            )?
            .texture_coords();

        let mut quad = layers.allocate(layer_num)?;

        quad.set_position(
            point.x - left_offset,
            point.y - top_offset,
            (point.x + cell_size.width as f32) - left_offset,
            (point.y + cell_size.height as f32) - top_offset,
        );
        quad.set_texture(sprite);
        quad.set_fg_color(color);
        quad.set_alt_color_and_mix_value(color, 0.);
        quad.set_hsv(None);
        quad.set_has_color(false);
        Ok(quad)
    }

    pub fn min_scroll_bar_height(&self) -> f32 {
        self.config
            .min_scroll_bar_height
            .evaluate_as_pixels(DimensionContext {
                dpi: self.dimensions.dpi as f32,
                pixel_max: self.terminal_size.pixel_height as f32,
                pixel_cell: self.render_metrics.cell_size.height as f32,
            })
    }

    pub fn padding_left_top(&self) -> (f32, f32) {
        let h_context = DimensionContext {
            dpi: self.dimensions.dpi as f32,
            pixel_max: self.terminal_size.pixel_width as f32,
            pixel_cell: self.render_metrics.cell_size.width as f32,
        };
        let v_context = DimensionContext {
            dpi: self.dimensions.dpi as f32,
            pixel_max: self.terminal_size.pixel_height as f32,
            pixel_cell: self.render_metrics.cell_size.height as f32,
        };

        let padding_left = self
            .config
            .window_padding
            .left
            .evaluate_as_pixels(h_context)
            + self.workspace_sidebar_width() as f32;
        let padding_right = self.config.window_padding.right;
        let padding_top = self.config.window_padding.top.evaluate_as_pixels(v_context);
        let padding_bottom = self
            .config
            .window_padding
            .bottom
            .evaluate_as_pixels(v_context);

        let horizontal_gap = self.dimensions.pixel_width as f32
            - self.terminal_size.pixel_width as f32
            - padding_left
            - self.right_sidebar_width() as f32
            - if self.show_scroll_bar && padding_right.is_zero() {
                h_context.pixel_cell
            } else {
                padding_right.evaluate_as_pixels(h_context)
            };
        let vertical_gap = self.dimensions.pixel_height as f32
            - self.terminal_size.pixel_height as f32
            - padding_top
            - padding_bottom
            - if self.show_tab_bar {
                self.tab_bar_pixel_height().unwrap_or(0.)
            } else {
                0.
            };
        let left_gap = match self.config.window_content_alignment.horizontal {
            HorizontalWindowContentAlignment::Left => 0.,
            HorizontalWindowContentAlignment::Center => (horizontal_gap / 2.).round(),
            HorizontalWindowContentAlignment::Right => horizontal_gap,
        };
        let top_gap = match self.config.window_content_alignment.vertical {
            VerticalWindowContentAlignment::Top => 0.,
            VerticalWindowContentAlignment::Center => (vertical_gap / 2.).round(),
            VerticalWindowContentAlignment::Bottom => vertical_gap,
        };

        (padding_left + left_gap, padding_top + top_gap)
    }

    fn resolve_lock_glyph(
        &self,
        style: &TextStyle,
        attrs: &CellAttributes,
        font: Option<&Rc<LoadedFont>>,
        font_config: Option<&Rc<FontConfiguration>>,
        gl_state: &RenderState,
        metrics: &RenderMetrics,
        font_identity: u64,
    ) -> anyhow::Result<Rc<CachedGlyph>> {
        let fa_lock = "\u{f023}";
        let line = Line::from_text(fa_lock, attrs, 0, None);
        let cluster = line.cluster(None);
        let shape_info = self.cached_cluster_shape(
            style,
            &cluster[0],
            gl_state,
            font,
            font_config,
            metrics,
            font_identity,
        )?;
        Ok(Rc::clone(&shape_info[0].glyph))
    }

    pub fn populate_block_quad(
        &self,
        block: BlockKey,
        gl_state: &RenderState,
        quads: &mut dyn QuadAllocator,
        pos_x: f32,
        params: &RenderScreenLineParams,
        hsv: Option<config::HsbTransform>,
        glyph_color: LinearRgba,
    ) -> anyhow::Result<()> {
        let sprite = gl_state
            .glyph_cache
            .borrow_mut()
            .cached_block(block, &params.render_metrics)?
            .texture_coords();

        let mut quad = quads.allocate()?;
        let cell_width = params.render_metrics.cell_size.width as f32;
        let cell_height = params.render_metrics.cell_size.height as f32;
        let pos_y = (self.dimensions.pixel_height as f32 / -2.) + params.top_pixel_y;
        quad.set_position(pos_x, pos_y, pos_x + cell_width, pos_y + cell_height);
        quad.set_hsv(hsv);
        quad.set_fg_color(glyph_color);
        quad.set_texture(sprite);
        quad.set_has_color(false);
        Ok(())
    }

    /// Render iTerm2 style image attributes
    pub fn populate_image_quad(
        &self,
        image: &termwiz::image::ImageCell,
        gl_state: &RenderState,
        layers: &mut TripleLayerQuadAllocator,
        layer_num: usize,
        cell_idx: usize,
        params: &RenderScreenLineParams,
        hsv: Option<config::HsbTransform>,
        glyph_color: LinearRgba,
    ) -> anyhow::Result<()> {
        if self.allow_images == AllowImage::No {
            return Ok(());
        }

        let padding = self
            .render_metrics
            .cell_size
            .height
            .max(params.render_metrics.cell_size.width) as usize;
        let padding = if padding.is_power_of_two() {
            padding
        } else {
            padding.next_power_of_two()
        };

        // Large pictures -- streaming kitty frames, icat -- draw from their
        // own texture instead of a slot in the shared atlas. The atlas cannot
        // free a rectangle, so a stream of multi-megabyte frames filled it
        // every few frames and each overflow cleared it: a 64MiB zero image,
        // a 64MiB upload, every glyph re-rasterised and the frame painted a
        // second time. WebGpu only (the composite pass is), static RGBA only
        // (animations advance their frame clock inside cached_image), and
        // never while a content-view transition records or replays the
        // terminal as one heap.
        if let RenderContext::WebGpu(webgpu) = &gl_state.context {
            let atlas_side = gl_state.glyph_cache.borrow().atlas.size();
            let guard = image.image_data().data();
            if let termwiz::image::ImageDataType::Rgba8 {
                data: rgba,
                width,
                height,
                hash,
            } = &*guard
            {
                let (width, height, hash) = (*width, *height, *hash);
                {
                    // Once one picture in a cell has gone to the composite
                    // pass, every picture stacked above it in that cell
                    // goes too, whatever its size: the composite pass runs
                    // after the whole atlas layer, so a small atlas picture
                    // with a higher z would otherwise be painted under the
                    // big one instead of over it.
                    let stacked_on_dedicated =
                        self.dedicated_image_cell.get() == Some((layer_num, cell_idx));
                    let max_side = webgpu.device.limits().max_texture_dimension_2d;
                    let goes_dedicated = image_fits_dedicated_texture(width, height, max_side)
                        && (stacked_on_dedicated
                            || image_wants_dedicated_texture(width, height, padding, atlas_side));
                    // Marked before the transition check: a line painted
                    // through the atlas only because a content-view
                    // transition forbade the dedicated path must not be
                    // cached either, or its replay would pin the picture in
                    // the atlas long after the transition ended.
                    if goes_dedicated {
                        self.dedicated_image_in_line.set(true);
                    }
                    if goes_dedicated && self.dedicated_image_textures_allowed() {
                        let texture = gl_state
                            .dedicated_images
                            .borrow_mut()
                            .get_or_upload(
                                hash,
                                width,
                                height,
                                || ImageTexture::new(width, height, webgpu),
                                |texture| {
                                    texture.upload(rgba);
                                    gl_state.glyph_cache.borrow_mut().note_dedicated_image(
                                        width,
                                        height,
                                        termwiz::image::ImageDataType::is_nonce_key(&hash),
                                    );
                                },
                            )
                            .context("dedicated image texture")?;
                        drop(guard);
                        self.push_dedicated_image_quad(
                            image,
                            &texture,
                            layer_num,
                            cell_idx,
                            params,
                            hsv,
                            glyph_color,
                        );
                        return Ok(());
                    }
                }
            }
        }

        let (sprite, next_due, _load_state) = gl_state
            .glyph_cache
            .borrow_mut()
            .cached_image(image.image_data(), Some(padding), self.allow_images)
            .context("cached_image")?;
        self.update_next_frame_time(next_due);
        let width = sprite.coords.size.width;
        let height = sprite.coords.size.height;

        let top_left = image.top_left();
        let bottom_right = image.bottom_right();

        // We *could* call sprite.texture.to_texture_coords() here,
        // but since that takes integer pixel coordinates, we'd
        // lose precision and end up with visual artifacts.
        // Instead, we compute the texture coords here in floating point.

        let texture_width = sprite.texture.width() as f32;
        let texture_height = sprite.texture.height() as f32;
        let origin = TextureCoord::new(
            (sprite.coords.origin.x as f32 + (*top_left.x * width as f32)) / texture_width,
            (sprite.coords.origin.y as f32 + (*top_left.y * height as f32)) / texture_height,
        );

        let size = TextureSize::new(
            (*bottom_right.x - *top_left.x) * width as f32 / texture_width,
            (*bottom_right.y - *top_left.y) * height as f32 / texture_height,
        );

        let texture_rect = TextureRect::new(origin, size);

        let mut quad = layers.allocate(layer_num)?;
        let cell_width = params.render_metrics.cell_size.width as f32;
        let cell_height = params.render_metrics.cell_size.height as f32;
        let pos_y = (self.dimensions.pixel_height as f32 / -2.) + params.top_pixel_y;

        let pos_x = (self.dimensions.pixel_width as f32 / -2.)
            + params.left_pixel_x
            + (cell_idx as f32 * cell_width);

        let (padding_left, padding_top, padding_right, padding_bottom) = image.padding();

        quad.set_position(
            pos_x + padding_left as f32,
            pos_y + padding_top as f32,
            pos_x + cell_width + padding_left as f32 - padding_right as f32,
            pos_y + cell_height + padding_top as f32 - padding_bottom as f32,
        );
        quad.set_hsv(hsv);
        quad.set_fg_color(glyph_color);
        quad.set_texture(texture_rect);
        quad.set_has_color(true);

        Ok(())
    }

    /// The dedicated-texture twin of the atlas quad above: same cell
    /// geometry, but the texture coordinates are the cell's sub-rectangle
    /// of the picture over a texture that holds exactly that picture, and
    /// the quad goes to the composite batch instead of the layer buffers.
    fn push_dedicated_image_quad(
        &self,
        image: &termwiz::image::ImageCell,
        texture: &Rc<ImageTexture>,
        layer_num: usize,
        cell_idx: usize,
        params: &RenderScreenLineParams,
        hsv: Option<config::HsbTransform>,
        glyph_color: LinearRgba,
    ) {
        let top_left = image.top_left();
        let bottom_right = image.bottom_right();
        let texture_rect = TextureRect::new(
            TextureCoord::new(*top_left.x, *top_left.y),
            TextureSize::new(*bottom_right.x - *top_left.x, *bottom_right.y - *top_left.y),
        );

        let mut vert: [Vertex; 4] = Default::default();
        let mut quad = Quad { vert: &mut vert };
        let cell_width = params.render_metrics.cell_size.width as f32;
        let cell_height = params.render_metrics.cell_size.height as f32;
        let pos_y = (self.dimensions.pixel_height as f32 / -2.) + params.top_pixel_y;
        let pos_x = (self.dimensions.pixel_width as f32 / -2.)
            + params.left_pixel_x
            + (cell_idx as f32 * cell_width);
        let (padding_left, padding_top, padding_right, padding_bottom) = image.padding();
        quad.set_position(
            pos_x + padding_left as f32,
            pos_y + padding_top as f32,
            pos_x + cell_width + padding_left as f32 - padding_right as f32,
            pos_y + cell_height + padding_top as f32 - padding_bottom as f32,
        );
        quad.set_hsv(hsv);
        quad.set_fg_color(glyph_color);
        quad.set_texture(texture_rect);
        quad.set_has_color(true);

        // Sub-buffer 0 is where z<0 pictures went (under the glyphs),
        // sub-buffer 2 where z>=0 went (over them); the composite passes
        // sit at the same two points.
        let slot = if layer_num == 0 {
            ImageCompositeSlot::AfterFills
        } else {
            ImageCompositeSlot::AfterGlyphs
        };
        self.image_composites
            .borrow_mut()
            .push_quad(texture, slot, vert);
        self.dedicated_image_in_line.set(true);
        self.dedicated_image_cell.set(Some((layer_num, cell_idx)));
    }

    fn ensure_min_contrast(&self, fg_color: LinearRgba, bg_color: LinearRgba) -> LinearRgba {
        match self.config.text_min_contrast_ratio {
            Some(ratio) => fg_color
                .ensure_contrast_ratio(&bg_color, ratio)
                .unwrap_or(fg_color),
            None => fg_color,
        }
    }

    pub fn compute_cell_fg_bg(&self, params: ComputeCellFgBgParams) -> ComputeCellFgBgResult {
        let focused_and_active =
            self.focused.is_some() && params.is_active_pane && !self.right_sidebar_has_text_focus();

        if params.cursor.is_some() {
            if let Some(bg_color_mix) = self.get_intensity_if_bell_target_ringing(
                params.pane.expect("cursor only set if pane present"),
                params.config,
                VisualBellTarget::CursorColor,
            ) {
                let (fg_color, bg_color) = if self.use_reverse_video_cursor(&params) {
                    (params.bg_color, params.fg_color)
                } else {
                    (params.cursor_fg, params.cursor_bg)
                };

                let fg_color = self.ensure_min_contrast(fg_color, bg_color);

                // interpolate between the background color
                // and the the target color
                let bg_color_alt = params
                    .config
                    .resolved_palette
                    .visual_bell
                    .map(|c| c.to_linear())
                    .unwrap_or(fg_color);

                return ComputeCellFgBgResult {
                    fg_color,
                    fg_color_alt: fg_color,
                    fg_color_mix: 0.,
                    bg_color,
                    bg_color_alt,
                    bg_color_mix,
                    cursor_shape: Some(CursorShape::Default),
                    cursor_border_color: bg_color,
                    cursor_border_color_alt: bg_color_alt,
                    cursor_border_mix: bg_color_mix,
                };
            }

            let dead_key_or_leader =
                *self.terminal_dead_key_status() != DeadKeyStatus::None || self.leader_is_active();

            if dead_key_or_leader && focused_and_active {
                let (fg_color, bg_color) = if self.use_reverse_video_cursor(&params) {
                    (params.bg_color, params.fg_color)
                } else {
                    (params.cursor_fg, params.cursor_bg)
                };

                let fg_color = self.ensure_min_contrast(fg_color, bg_color);

                let color = params
                    .config
                    .resolved_palette
                    .compose_cursor
                    .map(|c| c.to_linear())
                    .unwrap_or(bg_color);

                return ComputeCellFgBgResult {
                    fg_color,
                    fg_color_alt: fg_color,
                    fg_color_mix: 0.,
                    bg_color,
                    bg_color_alt: bg_color,
                    bg_color_mix: 0.,
                    cursor_shape: Some(CursorShape::Default),
                    cursor_border_color: color,
                    cursor_border_color_alt: color,
                    cursor_border_mix: 0.,
                };
            }
        }

        let (cursor_shape, visibility) = match params.cursor {
            Some(cursor) => (
                params
                    .config
                    .default_cursor_style
                    .effective_shape(cursor.shape),
                cursor.visibility,
            ),
            _ => (CursorShape::default(), CursorVisibility::Hidden),
        };

        let (fg_color, bg_color, cursor_bg) = match (
            params.selected,
            focused_and_active,
            cursor_shape,
            visibility,
        ) {
            // Selected text overrides colors
            (true, _, _, CursorVisibility::Hidden) => (
                params.selection_fg.when_fully_transparent(params.fg_color),
                params.selection_bg,
                params.cursor_bg,
            ),
            // block Cursor cell overrides colors
            (
                _,
                true,
                CursorShape::BlinkingBlock | CursorShape::SteadyBlock,
                CursorVisibility::Visible,
            ) => {
                if self.use_reverse_video_cursor(&params) {
                    (params.bg_color, params.fg_color, params.fg_color)
                } else {
                    (
                        params.cursor_fg.when_fully_transparent(params.fg_color),
                        params.cursor_bg,
                        params.cursor_bg,
                    )
                }
            }
            (
                _,
                true,
                CursorShape::BlinkingUnderline
                | CursorShape::SteadyUnderline
                | CursorShape::BlinkingBar
                | CursorShape::SteadyBar,
                CursorVisibility::Visible,
            ) => {
                if self.use_reverse_video_cursor(&params) {
                    (params.fg_color, params.bg_color, params.fg_color)
                } else {
                    (params.fg_color, params.bg_color, params.cursor_bg)
                }
            }
            // Normally, render the cell as configured (or if the window is unfocused)
            _ => (params.fg_color, params.bg_color, params.cursor_border_color),
        };

        let fg_color = self.ensure_min_contrast(fg_color, bg_color);

        let blinking = params.cursor.is_some()
            && focused_and_active
            && cursor_shape.is_blinking()
            && params.config.cursor_blink_rate != 0;

        let mut fg_color_alt = fg_color;
        let bg_color_alt = bg_color;
        let mut fg_color_mix = 0.;
        let bg_color_mix = 0.;
        let mut cursor_border_color_alt = cursor_bg;
        let mut cursor_border_mix = 0.;

        if blinking {
            let mut color_ease = self.cursor_blink_state.borrow_mut();
            color_ease.update_start(self.prev_cursor.last_cursor_movement());
            let (intensity, next) = color_ease.intensity_continuous();

            cursor_border_mix = intensity;
            cursor_border_color_alt = params.bg_color;

            if matches!(
                cursor_shape,
                CursorShape::BlinkingBlock | CursorShape::SteadyBlock,
            ) {
                fg_color_alt = params.fg_color;
                fg_color_mix = intensity;
            }

            self.update_next_frame_time(Some(next));
        }

        ComputeCellFgBgResult {
            fg_color,
            fg_color_alt,
            bg_color,
            bg_color_alt,
            fg_color_mix,
            bg_color_mix,
            cursor_border_color: cursor_bg,
            cursor_border_color_alt,
            cursor_border_mix,
            cursor_shape: if visibility == CursorVisibility::Visible {
                match cursor_shape {
                    CursorShape::BlinkingBlock | CursorShape::SteadyBlock if focused_and_active => {
                        Some(CursorShape::Default)
                    }
                    // When not focused, convert bar to block to make it more visually
                    // distinct from the focused bar in another pane
                    _shape if !focused_and_active => Some(CursorShape::SteadyBlock),
                    shape => Some(shape),
                }
            } else {
                None
            },
        }
    }

    fn use_reverse_video_cursor(&self, params: &ComputeCellFgBgParams) -> bool {
        self.config.force_reverse_video_cursor
            && params.cursor_is_default_color
            && params.fg_color.contrast_ratio(&params.bg_color)
                >= self.config.reverse_video_cursor_min_contrast
    }

    fn glyph_infos_to_glyphs(
        &self,
        style: &TextStyle,
        glyph_cache: &mut GlyphCache,
        infos: &[GlyphInfo],
        font: &Rc<LoadedFont>,
        metrics: &RenderMetrics,
    ) -> anyhow::Result<Vec<Rc<CachedGlyph>>> {
        let mut glyphs = Vec::with_capacity(infos.len());
        let mut iter = infos.iter().peekable();
        while let Some(info) = iter.next() {
            if self.config.custom_block_glyphs {
                if info.only_char.and_then(BlockKey::from_char).is_some() {
                    // Don't bother rendering the glyph from the font, as it can
                    // have incorrect advance metrics.
                    // Instead, just use our pixel-perfect cell metrics
                    glyphs.push(Rc::new(CachedGlyph {
                        brightness_adjust: 1.0,
                        has_color: false,
                        texture: None,
                        x_advance: PixelLength::new(metrics.cell_size.width as f64),
                        x_offset: PixelLength::zero(),
                        y_offset: PixelLength::zero(),
                        bearing_x: PixelLength::zero(),
                        bearing_y: PixelLength::zero(),
                        scale: 1.0,
                    }));
                    continue;
                }
            }

            let followed_by_space = match iter.peek() {
                Some(next_info) => next_info.is_space,
                None => false,
            };

            glyphs.push(glyph_cache.cached_glyph(
                info,
                &style,
                followed_by_space,
                font,
                metrics,
                info.num_cells,
            )?);
        }
        Ok(glyphs)
    }

    /// Shape the printable text from a cluster
    fn cached_cluster_shape(
        &self,
        style: &TextStyle,
        cluster: &CellCluster,
        gl_state: &RenderState,
        font: Option<&Rc<LoadedFont>>,
        font_config: Option<&Rc<FontConfiguration>>,
        metrics: &RenderMetrics,
        font_identity: u64,
    ) -> anyhow::Result<Rc<Vec<ShapedInfo>>> {
        let shape_resolve_start = Instant::now();
        let key = BorrowedShapeCacheKey {
            font_identity,
            style,
            text: &cluster.text,
        };
        let glyph_info = match self.lookup_cached_shape(&key) {
            Some(Ok(info)) => {
                crate::perf::accum_count("cluster_shape_hit");
                info
            }
            Some(Err(err)) => return Err(err),
            None => {
                let font = match font {
                    Some(f) => Rc::clone(f),
                    None => font_config.unwrap_or(&self.fonts).resolve_font(style)?,
                };
                let window = self.window.as_ref().unwrap().clone();

                let presentation_width = PresentationWidth::with_cluster(&cluster);

                let hb_started = crate::perf::now();
                match font.shape(
                    &cluster.text,
                    move |chars: &[char]| {
                        window.notify(TermWindowNotif::InvalidateShapeCacheForChars(
                            chars.to_vec(),
                        ))
                    },
                    crate::customglyph::filter_out_synthetic,
                    Some(cluster.presentation),
                    cluster.direction,
                    None, // FIXME: need more paragraph context
                    Some(&presentation_width),
                ) {
                    Ok(info) => {
                        crate::perf::accum("cluster_hb_shape", hb_started);
                        let raster_started = crate::perf::now();
                        let glyphs = self.glyph_infos_to_glyphs(
                            &style,
                            &mut gl_state.glyph_cache.borrow_mut(),
                            &info,
                            &font,
                            metrics,
                        )?;
                        crate::perf::accum("glyph_raster", raster_started);
                        let shaped = Rc::new(ShapedInfo::process(&info, &glyphs));

                        let key = key.to_owned();
                        let value = Ok(Rc::clone(&shaped));
                        let weight = crate::shapecache::estimate_shaped_entry_bytes(&key, &value);
                        self.shape_cache
                            .borrow_mut()
                            .put_weighted(key, value, weight);
                        shaped
                    }
                    Err(err) => {
                        if err.root_cause().downcast_ref::<ClearShapeCache>().is_some() {
                            return Err(err);
                        }

                        let res = anyhow!("shaper error: {}", err);
                        let key = key.to_owned();
                        let value = Err(err);
                        let weight = crate::shapecache::estimate_shaped_entry_bytes(&key, &value);
                        self.shape_cache
                            .borrow_mut()
                            .put_weighted(key, value, weight);
                        return Err(res);
                    }
                }
            }
        };
        metrics::histogram!("cached_cluster_shape").record(shape_resolve_start.elapsed());
        log::trace!(
            "shape_resolve for cluster len {} -> elapsed {:?}",
            cluster.text.len(),
            shape_resolve_start.elapsed()
        );
        Ok(glyph_info)
    }

    /// Shape a cluster one cell at a time, each cell's grapheme going through
    /// the shape cache on its own.
    ///
    /// The whole-run `cached_cluster_shape` above keys its cache on the entire
    /// same-attribute string, and against content like btop -- whose braille
    /// graphs mint a new string every refresh -- that cache can never hit:
    /// measured on a Live Overview rebuild, 85% of the cost was HarfBuzz
    /// re-shaping runs it had shaped 100ms earlier. The individual characters,
    /// though, repeat endlessly (braille is 256 codepoints), so per-cell
    /// entries converge on a hit rate of ~1 after the first screen.
    ///
    /// The price is shaping without cross-cell context: no ligatures, no
    /// kerning, and ambiguous-width characters resolved without the cluster's
    /// `PresentationWidth`. At the 2-6px cell sizes previews render at, none
    /// of that is visible, which is why only they take this path.
    fn cached_cluster_shape_by_cell(
        &self,
        style: &TextStyle,
        cluster: &CellCluster,
        gl_state: &RenderState,
        font: Option<&Rc<LoadedFont>>,
        font_config: Option<&Rc<FontConfiguration>>,
        metrics: &RenderMetrics,
        font_identity: u64,
    ) -> anyhow::Result<Rc<Vec<ShapedInfo>>> {
        let text = &cluster.text;
        if text.is_empty() {
            return Ok(Rc::new(vec![]));
        }

        // Cell boundaries in byte offsets. A cell holds one grapheme, so
        // splitting where the byte->cell mapping steps keeps combining
        // sequences intact without a segmentation pass.
        let mut segments: Vec<(usize, usize)> = Vec::new();
        let mut seg_start = 0usize;
        let mut seg_cell = cluster.byte_to_cell_idx(0);
        for (byte_idx, _) in text.char_indices().skip(1) {
            let cell = cluster.byte_to_cell_idx(byte_idx);
            if cell != seg_cell {
                segments.push((seg_start, byte_idx));
                seg_start = byte_idx;
                seg_cell = cell;
            }
        }
        segments.push((seg_start, text.len()));

        let mut resolved_font: Option<Rc<LoadedFont>> = font.cloned();
        let mut merged: Vec<ShapedInfo> = Vec::with_capacity(segments.len());
        for (start, end) in segments {
            let seg = &text[start..end];
            let key = BorrowedShapeCacheKey {
                font_identity,
                style,
                text: seg,
            };
            let infos = match self.lookup_cached_shape(&key) {
                Some(Ok(info)) => {
                    crate::perf::accum_count("cluster_shape_hit");
                    info
                }
                Some(Err(err)) => return Err(err),
                None => {
                    let font = match &resolved_font {
                        Some(font) => Rc::clone(font),
                        None => {
                            let font = font_config.unwrap_or(&self.fonts).resolve_font(style)?;
                            resolved_font = Some(Rc::clone(&font));
                            font
                        }
                    };
                    let window = self.window.as_ref().unwrap().clone();
                    let hb_started = crate::perf::now();
                    match font.shape(
                        seg,
                        move |chars: &[char]| {
                            window.notify(TermWindowNotif::InvalidateShapeCacheForChars(
                                chars.to_vec(),
                            ))
                        },
                        crate::customglyph::filter_out_synthetic,
                        Some(cluster.presentation),
                        cluster.direction,
                        None,
                        None,
                    ) {
                        Ok(info) => {
                            crate::perf::accum("cluster_hb_shape", hb_started);
                            let raster_started = crate::perf::now();
                            let glyphs = self.glyph_infos_to_glyphs(
                                &style,
                                &mut gl_state.glyph_cache.borrow_mut(),
                                &info,
                                &font,
                                metrics,
                            )?;
                            crate::perf::accum("glyph_raster", raster_started);
                            let shaped = Rc::new(ShapedInfo::process(&info, &glyphs));
                            let key = key.to_owned();
                            let value = Ok(Rc::clone(&shaped));
                            let weight =
                                crate::shapecache::estimate_shaped_entry_bytes(&key, &value);
                            self.shape_cache
                                .borrow_mut()
                                .put_weighted(key, value, weight);
                            shaped
                        }
                        Err(err) => {
                            if err.root_cause().downcast_ref::<ClearShapeCache>().is_some() {
                                return Err(err);
                            }
                            let res = anyhow!("shaper error: {}", err);
                            let key = key.to_owned();
                            let value = Err(err);
                            let weight =
                                crate::shapecache::estimate_shaped_entry_bytes(&key, &value);
                            self.shape_cache
                                .borrow_mut()
                                .put_weighted(key, value, weight);
                            return Err(res);
                        }
                    }
                }
            };
            for info in infos.iter() {
                merged.push(ShapedInfo {
                    glyph: Rc::clone(&info.glyph),
                    pos: GlyphPosition {
                        glyph_idx: info.pos.glyph_idx,
                        num_cells: info.pos.num_cells,
                        x_offset: info.pos.x_offset,
                        bearing_x: info.pos.bearing_x,
                        bitmap_pixel_width: info.pos.bitmap_pixel_width,
                    },
                    // Re-anchor from segment-relative to cluster-relative so
                    // the consumer's byte->cell mapping still lands.
                    cluster: start + info.cluster,
                    block_key: info.block_key,
                });
            }
        }
        Ok(Rc::new(merged))
    }

    fn lookup_cached_shape(
        &self,
        key: &dyn ShapeCacheKeyTrait,
    ) -> Option<anyhow::Result<Rc<Vec<ShapedInfo>>>> {
        match self.shape_cache.borrow_mut().get(key) {
            Some(Ok(info)) => Some(Ok(Rc::clone(info))),
            Some(Err(err)) => Some(Err(anyhow!("cached shaper error: {}", err))),
            None => None,
        }
    }

    pub fn recreate_texture_atlas(&mut self, size: Option<usize>) -> anyhow::Result<()> {
        self.shape_generation += 1;
        self.shape_cache.borrow_mut().clear();
        self.ui_shape_caches.borrow_mut().clear_all();
        self.publish_ui_shape_cache_diagnostics();
        self.line_to_ele_shape_cache.borrow_mut().clear();
        if let Some(render_state) = self.render_state.as_mut() {
            render_state.recreate_texture_atlas(&self.fonts, &self.render_metrics, size)?;
        }
        Ok(())
    }

    fn shape_hash_for_line(&mut self, line: &Line) -> [u8; 16] {
        let seqno = line.current_seqno();
        let mut id = None;
        if let Some(cached_arc) = line.get_appdata() {
            if let Some(line_state) = cached_arc.downcast_ref::<CachedLineState>() {
                if line_state.seqno == seqno {
                    // Touch the LRU
                    self.line_state_cache.borrow_mut().get(&line_state.id);
                    return line_state.shape_hash;
                }
                id.replace(line_state.id);
            }
        }

        let id = id.unwrap_or_else(|| {
            let id = self.next_line_state_id;
            self.next_line_state_id += 1;
            id
        });

        let shape_hash = line.compute_shape_hash();

        let state = Arc::new(CachedLineState {
            id,
            seqno,
            shape_hash,
        });

        line.set_appdata(Arc::clone(&state));

        self.line_state_cache
            .borrow_mut()
            .put_weighted(id, state, LINE_STATE_ENTRY_BYTES);
        shape_hash
    }
}

fn resolve_fg_color_attr(
    attrs: &CellAttributes,
    fg: ColorAttribute,
    palette: &ColorPalette,
    config: &ConfigHandle,
    style: &config::TextStyle,
) -> LinearRgba {
    match fg {
        wezterm_term::color::ColorAttribute::Default => {
            if let Some(fg) = style.foreground {
                fg.into()
            } else {
                palette.resolve_fg(attrs.foreground())
            }
        }
        wezterm_term::color::ColorAttribute::PaletteIndex(idx)
            if idx < 8 && config.bold_brightens_ansi_colors != BoldBrightening::No =>
        {
            // For compatibility purposes, switch to a brighter version
            // of one of the standard ANSI colors when Bold is enabled.
            // This lifts black to dark grey.
            let idx = if attrs.intensity() == wezterm_term::Intensity::Bold {
                idx + 8
            } else {
                idx
            };

            palette.resolve_fg(wezterm_term::color::ColorAttribute::PaletteIndex(idx))
        }
        _ => palette.resolve_fg(fg),
    }
    .to_linear()
}

fn update_next_frame_time(storage: &mut Option<Instant>, next_due: Option<Instant>) {
    if let Some(next_due) = next_due {
        match storage.take() {
            None => {
                storage.replace(next_due);
            }
            Some(t) if next_due < t => {
                storage.replace(next_due);
            }
            Some(t) => {
                storage.replace(t);
            }
        }
    }
}

fn same_hyperlink(a: Option<&Arc<Hyperlink>>, b: Option<&Arc<Hyperlink>>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => Arc::ptr_eq(a, b),
        _ => false,
    }
}

#[cfg(test)]
mod line_quad_cache_tests {
    use super::*;

    fn key(render_cols: usize, render_pixel_width: usize) -> LineQuadCacheKey {
        LineQuadCacheKey {
            config_generation: 0,
            shape_generation: 0,
            quad_generation: 0,
            composing: None,
            selection: 0..0,
            shape_hash: [0; 16],
            font_identity: 0,
            top_pixel_y: NotNan::new(0.0).unwrap(),
            left_pixel_x: NotNan::new(0.0).unwrap(),
            render_cols,
            render_pixel_width,
            phys_line_idx: 0,
            pane_id: 1,
            pane_is_active: true,
            cursor: None,
            reverse_video: false,
            password_input: false,
        }
    }

    #[test]
    fn divider_resize_cannot_reuse_line_quads_from_the_old_pane_width() {
        let wide = key(120, 1_200);
        assert_ne!(wide, key(80, 800));
        assert_ne!(wide, key(120, 1_000));
    }
}
