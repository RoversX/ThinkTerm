//! Quads: four vertices per cell, the allocators that hand them out and the
//! heap-recorded surfaces that can be replayed, clipped and shifted later.

use crate::geom::Dimensions;
use crate::vertex::*;
use crate::bitmaps::TextureRect;
use wezterm_color_types::{HsbTransform, LinearRgba};

pub trait QuadTrait {
    /// Assign the texture coordinates
    fn set_texture(&mut self, coords: TextureRect) {
        let x1 = coords.min_x();
        let x2 = coords.max_x();
        let y1 = coords.min_y();
        let y2 = coords.max_y();
        self.set_texture_discrete(x1, x2, y1, y2);
    }
    fn set_texture_discrete(&mut self, x1: f32, x2: f32, y1: f32, y2: f32);
    fn set_has_color_impl(&mut self, has_color: f32);

    /// Set the color glyph "flag"
    fn set_has_color(&mut self, has_color: bool) {
        self.set_has_color_impl(if has_color { IS_COLOR_EMOJI } else { IS_GLYPH });
    }

    /// Mark as a grayscale polyquad; color and alpha will be
    /// multipled with those in the texture
    fn set_grayscale(&mut self) {
        self.set_has_color_impl(IS_GRAY_SCALE);
    }

    /// Mark this quad as a background image.
    /// Mutually exclusive with set_has_color.
    fn set_is_background_image(&mut self) {
        self.set_has_color_impl(IS_BG_IMAGE);
    }

    fn set_is_background(&mut self) {
        self.set_has_color_impl(IS_SOLID_COLOR);
    }

    fn set_fg_color(&mut self, color: LinearRgba);

    /// Set an independently colored corner on each vertex. The GPU
    /// interpolates these colors across the quad, allowing a subtle
    /// two-dimensional native UI gradient without uploading a texture.
    fn set_corner_gradient(
        &mut self,
        top_left: LinearRgba,
        top_right: LinearRgba,
        bottom_left: LinearRgba,
        bottom_right: LinearRgba,
    );

    /// Set a smoothly interpolated foreground color from the top edge to the
    /// bottom edge. Solid-background quads use this for native UI gradients.
    fn set_vertical_gradient(&mut self, top: LinearRgba, bottom: LinearRgba);

    /// Must be called after set_fg_color
    fn set_alt_color_and_mix_value(&mut self, color: LinearRgba, mix_value: f32);

    fn set_hsv(&mut self, hsv: Option<HsbTransform>);
    fn set_position(&mut self, left: f32, top: f32, right: f32, bottom: f32);
}

pub enum QuadImpl<'a> {
    Vert(Quad<'a>),
    Boxed(&'a mut BoxedQuad),
    TransformedBoxed(&'a mut BoxedQuad, QuadPositionTransform),
    Tee(Quad<'a>, &'a mut BoxedQuad),
}

/// Maps quad positions from a naturally rendered source rect into an exact
/// destination rect while the quad is being authored. Live thumbnails use
/// this instead of walking every glyph a second time after paint.
#[derive(Clone, Copy, Debug)]
pub struct QuadPositionTransform {
    scale_x: f32,
    scale_y: f32,
    offset_x: f32,
    offset_y: f32,
}

impl QuadPositionTransform {
    pub fn new(source: QuadClipRect, target: QuadClipRect) -> Option<Self> {
        let source_width = source.width();
        let source_height = source.bottom() - source.top();
        let target_width = target.width();
        let target_height = target.bottom() - target.top();
        if !(source_width > 0.0 && source_height > 0.0 && target_width > 0.0 && target_height > 0.0)
        {
            return None;
        }
        let scale_x = target_width / source_width;
        let scale_y = target_height / source_height;
        let offset_x = target.left() - source.left() * scale_x;
        let offset_y = target.top() - source.top() * scale_y;
        (scale_x.is_finite() && scale_y.is_finite() && offset_x.is_finite() && offset_y.is_finite())
            .then_some(Self {
                scale_x,
                scale_y,
                offset_x,
                offset_y,
            })
    }

    fn map_position(self, left: f32, top: f32, right: f32, bottom: f32) -> (f32, f32, f32, f32) {
        (
            left * self.scale_x + self.offset_x,
            top * self.scale_y + self.offset_y,
            right * self.scale_x + self.offset_x,
            bottom * self.scale_y + self.offset_y,
        )
    }
}

impl<'a> QuadTrait for QuadImpl<'a> {
    fn set_texture_discrete(&mut self, x1: f32, x2: f32, y1: f32, y2: f32) {
        match self {
            Self::Vert(q) => q.set_texture_discrete(x1, x2, y1, y2),
            Self::Boxed(q) | Self::TransformedBoxed(q, _) => q.set_texture_discrete(x1, x2, y1, y2),
            Self::Tee(gpu, heap) => {
                gpu.set_texture_discrete(x1, x2, y1, y2);
                heap.set_texture_discrete(x1, x2, y1, y2);
            }
        }
    }

    fn set_has_color_impl(&mut self, has_color: f32) {
        match self {
            Self::Vert(q) => q.set_has_color_impl(has_color),
            Self::Boxed(q) | Self::TransformedBoxed(q, _) => q.set_has_color_impl(has_color),
            Self::Tee(gpu, heap) => {
                gpu.set_has_color_impl(has_color);
                heap.set_has_color_impl(has_color);
            }
        }
    }

    fn set_fg_color(&mut self, color: LinearRgba) {
        match self {
            Self::Vert(q) => q.set_fg_color(color),
            Self::Boxed(q) | Self::TransformedBoxed(q, _) => q.set_fg_color(color),
            Self::Tee(gpu, heap) => {
                gpu.set_fg_color(color);
                heap.set_fg_color(color);
            }
        }
    }

    fn set_corner_gradient(
        &mut self,
        top_left: LinearRgba,
        top_right: LinearRgba,
        bottom_left: LinearRgba,
        bottom_right: LinearRgba,
    ) {
        match self {
            Self::Vert(q) => q.set_corner_gradient(top_left, top_right, bottom_left, bottom_right),
            Self::Boxed(q) | Self::TransformedBoxed(q, _) => {
                q.set_corner_gradient(top_left, top_right, bottom_left, bottom_right)
            }
            Self::Tee(gpu, heap) => {
                gpu.set_corner_gradient(top_left, top_right, bottom_left, bottom_right);
                heap.set_corner_gradient(top_left, top_right, bottom_left, bottom_right);
            }
        }
    }

    fn set_vertical_gradient(&mut self, top: LinearRgba, bottom: LinearRgba) {
        match self {
            Self::Vert(q) => q.set_vertical_gradient(top, bottom),
            Self::Boxed(q) | Self::TransformedBoxed(q, _) => q.set_vertical_gradient(top, bottom),
            Self::Tee(gpu, heap) => {
                gpu.set_vertical_gradient(top, bottom);
                heap.set_vertical_gradient(top, bottom);
            }
        }
    }

    fn set_alt_color_and_mix_value(&mut self, color: LinearRgba, mix_value: f32) {
        match self {
            Self::Vert(q) => q.set_alt_color_and_mix_value(color, mix_value),
            Self::Boxed(q) | Self::TransformedBoxed(q, _) => {
                q.set_alt_color_and_mix_value(color, mix_value)
            }
            Self::Tee(gpu, heap) => {
                gpu.set_alt_color_and_mix_value(color, mix_value);
                heap.set_alt_color_and_mix_value(color, mix_value);
            }
        }
    }

    fn set_hsv(&mut self, hsv: Option<HsbTransform>) {
        match self {
            Self::Vert(q) => q.set_hsv(hsv),
            Self::Boxed(q) | Self::TransformedBoxed(q, _) => q.set_hsv(hsv),
            Self::Tee(gpu, heap) => {
                gpu.set_hsv(hsv);
                heap.set_hsv(hsv);
            }
        }
    }

    fn set_position(&mut self, left: f32, top: f32, right: f32, bottom: f32) {
        match self {
            Self::Vert(q) => q.set_position(left, top, right, bottom),
            Self::Boxed(q) => q.set_position(left, top, right, bottom),
            Self::TransformedBoxed(q, transform) => {
                let (left, top, right, bottom) = transform.map_position(left, top, right, bottom);
                q.set_position(left, top, right, bottom);
            }
            Self::Tee(gpu, heap) => {
                gpu.set_position(left, top, right, bottom);
                heap.set_position(left, top, right, bottom);
            }
        }
    }
}

/// A helper for updating the 4 vertices that compose a glyph cell
pub struct Quad<'a> {
    pub vert: &'a mut [Vertex],
}

impl<'a> QuadTrait for Quad<'a> {
    fn set_texture_discrete(&mut self, x1: f32, x2: f32, y1: f32, y2: f32) {
        self.vert[V_TOP_LEFT].tex = [x1, y1];
        self.vert[V_TOP_RIGHT].tex = [x2, y1];
        self.vert[V_BOT_LEFT].tex = [x1, y2];
        self.vert[V_BOT_RIGHT].tex = [x2, y2];
    }

    fn set_has_color_impl(&mut self, has_color: f32) {
        for v in self.vert.iter_mut() {
            v.has_color = has_color;
        }
    }

    fn set_fg_color(&mut self, color: LinearRgba) {
        for v in self.vert.iter_mut() {
            v.fg_color = color.into();
        }
        self.set_alt_color_and_mix_value(color, 0.);
    }

    fn set_corner_gradient(
        &mut self,
        top_left: LinearRgba,
        top_right: LinearRgba,
        bottom_left: LinearRgba,
        bottom_right: LinearRgba,
    ) {
        self.vert[V_TOP_LEFT].fg_color = top_left.into();
        self.vert[V_TOP_RIGHT].fg_color = top_right.into();
        self.vert[V_BOT_LEFT].fg_color = bottom_left.into();
        self.vert[V_BOT_RIGHT].fg_color = bottom_right.into();
    }

    fn set_vertical_gradient(&mut self, top: LinearRgba, bottom: LinearRgba) {
        self.set_corner_gradient(top, top, bottom, bottom);
    }

    /// Must be called after set_fg_color
    fn set_alt_color_and_mix_value(&mut self, color: LinearRgba, mix_value: f32) {
        for v in self.vert.iter_mut() {
            v.alt_color = color.into();
            v.mix_value = mix_value;
        }
    }

    fn set_hsv(&mut self, hsv: Option<HsbTransform>) {
        let (h, s, v) = hsv
            .map(|t| (t.hue, t.saturation, t.brightness))
            .unwrap_or((1., 1., 1.));
        for vert in self.vert.iter_mut() {
            vert.hsv = [h, s, v];
        }
    }

    fn set_position(&mut self, left: f32, top: f32, right: f32, bottom: f32) {
        self.vert[V_TOP_LEFT].position = [left, top];
        self.vert[V_TOP_RIGHT].position = [right, top];
        self.vert[V_BOT_LEFT].position = [left, bottom];
        self.vert[V_BOT_RIGHT].position = [right, bottom];
    }
}

pub trait QuadAllocator {
    fn allocate(&mut self) -> anyhow::Result<QuadImpl<'_>>;
    fn extend_with(&mut self, vertices: &[Vertex]);
}

pub trait TripleLayerQuadAllocatorTrait {
    fn allocate(&mut self, layer_num: usize) -> anyhow::Result<QuadImpl<'_>>;
    fn extend_with(&mut self, layer_num: usize, vertices: &[Vertex]);
}

impl<T: TripleLayerQuadAllocatorTrait + ?Sized> TripleLayerQuadAllocatorTrait for &mut T {
    fn allocate(&mut self, layer_num: usize) -> anyhow::Result<QuadImpl<'_>> {
        (**self).allocate(layer_num)
    }

    fn extend_with(&mut self, layer_num: usize, vertices: &[Vertex]) {
        (**self).extend_with(layer_num, vertices)
    }
}

/// We prefer to allocate a quad at a time for HeapQuadAllocator
/// because we tend to end up with fairly large arrays of Vertex
/// and the total amount of contiguous memory is in the MB range,
/// which is a bit gnarly to reallocate, and can waste several MB
/// in unused capacity
#[derive(Clone, Default)]
pub struct BoxedQuad {
    position: (f32, f32, f32, f32),
    fg_color: [f32; 4],
    fg_color_corners: Option<Box<[[f32; 4]; 4]>>,
    alt_color: [f32; 4],
    tex: (f32, f32, f32, f32),
    hsv: [f32; 3],
    has_color: f32,
    mix_value: f32,
}

impl QuadTrait for BoxedQuad {
    fn set_texture_discrete(&mut self, x1: f32, x2: f32, y1: f32, y2: f32) {
        self.tex = (x1, x2, y1, y2);
    }

    fn set_has_color_impl(&mut self, has_color: f32) {
        self.has_color = has_color;
    }

    fn set_fg_color(&mut self, color: LinearRgba) {
        self.fg_color = color.into();
        self.fg_color_corners = None;
    }
    fn set_corner_gradient(
        &mut self,
        top_left: LinearRgba,
        top_right: LinearRgba,
        bottom_left: LinearRgba,
        bottom_right: LinearRgba,
    ) {
        let colors = [
            top_left.into(),
            top_right.into(),
            bottom_left.into(),
            bottom_right.into(),
        ];
        self.fg_color = colors[V_TOP_LEFT];
        self.fg_color_corners = colors
            .iter()
            .skip(1)
            .any(|color| *color != colors[V_TOP_LEFT])
            .then(|| Box::new(colors));
    }
    fn set_vertical_gradient(&mut self, top: LinearRgba, bottom: LinearRgba) {
        self.set_corner_gradient(top, top, bottom, bottom);
    }
    fn set_alt_color_and_mix_value(&mut self, color: LinearRgba, mix_value: f32) {
        self.alt_color = color.into();
        self.mix_value = mix_value;
    }
    fn set_hsv(&mut self, hsv: Option<HsbTransform>) {
        let (h, s, v) = hsv
            .map(|t| (t.hue, t.saturation, t.brightness))
            .unwrap_or((1., 1., 1.));
        self.hsv = [h, s, v];
    }

    fn set_position(&mut self, left: f32, top: f32, right: f32, bottom: f32) {
        self.position = (left, top, right, bottom);
    }
}

impl BoxedQuad {
    fn from_vertices(verts: &[Vertex; VERTICES_PER_CELL]) -> Self {
        let [x1, y1] = verts[V_TOP_LEFT].tex;
        let [x2, y2] = verts[V_BOT_RIGHT].tex;

        let [left, top] = verts[V_TOP_LEFT].position;
        let [right, bottom] = verts[V_BOT_RIGHT].position;
        let fg_color_corners = [
            verts[V_TOP_LEFT].fg_color,
            verts[V_TOP_RIGHT].fg_color,
            verts[V_BOT_LEFT].fg_color,
            verts[V_BOT_RIGHT].fg_color,
        ];
        let fg_color = fg_color_corners[V_TOP_LEFT];
        Self {
            tex: (x1, x2, y1, y2),
            position: (left, top, right, bottom),
            has_color: verts[V_TOP_LEFT].has_color,
            alt_color: verts[V_TOP_LEFT].alt_color,
            fg_color,
            fg_color_corners: fg_color_corners
                .iter()
                .skip(1)
                .any(|color| *color != fg_color)
                .then(|| Box::new(fg_color_corners)),
            hsv: verts[V_TOP_LEFT].hsv,
            mix_value: verts[V_TOP_LEFT].mix_value,
        }
    }

    fn to_vertices(&self) -> [Vertex; VERTICES_PER_CELL] {
        let mut vert: [Vertex; VERTICES_PER_CELL] = Default::default();
        let mut quad = Quad { vert: &mut vert };

        let (x1, x2, y1, y2) = self.tex;
        quad.set_texture_discrete(x1, x2, y1, y2);

        let (left, top, right, bottom) = self.position;
        quad.set_position(left, top, right, bottom);

        quad.set_has_color_impl(self.has_color);
        let [hue, saturation, brightness] = self.hsv;
        quad.set_hsv(Some(HsbTransform {
            hue,
            saturation,
            brightness,
        }));
        if let Some(colors) = self.fg_color_corners.as_deref() {
            quad.set_corner_gradient(
                colors[V_TOP_LEFT].into(),
                colors[V_TOP_RIGHT].into(),
                colors[V_BOT_LEFT].into(),
                colors[V_BOT_RIGHT].into(),
            );
        } else {
            quad.set_fg_color(self.fg_color.into());
        }
        quad.set_alt_color_and_mix_value(self.alt_color.into(), self.mix_value);

        vert
    }

    /// Scale every colour's alpha, for compositing a recorded surface at
    /// partial opacity.
    ///
    /// Every shader branch multiplies by this, so a whole recorded surface --
    /// fills, gradients, UI text, terminal glyphs, colour emoji -- fades as
    /// one picture. `IS_GLYPH` and `IS_COLOR_EMOJI` used to take their alpha
    /// from the glyph texture alone and drop the vertex colour's, which left
    /// terminal text standing solid over everything else as it faded.
    fn with_opacity(mut self, opacity: f32) -> Self {
        self.fg_color[3] *= opacity;
        self.alt_color[3] *= opacity;
        if let Some(corners) = self.fg_color_corners.as_deref_mut() {
            for corner in corners.iter_mut() {
                corner[3] *= opacity;
            }
        }
        self
    }

    /// Translate this quad and crop it to `clip`, preserving texture
    /// coordinates and per-corner colors for the visible portion.
    fn translated_clipped(&self, offset_x: f32, offset_y: f32, clip: QuadClipRect) -> Option<Self> {
        let (left, top, right, bottom) = self.position;
        let translated_left = left + offset_x;
        let translated_top = top + offset_y;
        let translated_right = right + offset_x;
        let translated_bottom = bottom + offset_y;
        self.positioned_clipped(
            translated_left,
            translated_top,
            translated_right,
            translated_bottom,
            clip,
        )
    }

    /// Scale and offset this quad into a destination rect, then crop it.
    ///
    /// Unlike the desktop allocator's heap position transform, which
    /// maps positions as a surface is being authored, this remaps a surface
    /// that was already recorded. That is what lets one captured frame be
    /// replayed at a different size on every frame of a transition instead of
    /// the whole window being painted again each time -- at the cost of the
    /// glyph textures being sampled below their rasterised size, so the text
    /// softens as it shrinks.
    fn transformed_clipped(
        &self,
        transform: QuadPositionTransform,
        clip: QuadClipRect,
    ) -> Option<Self> {
        let (left, top, right, bottom) = self.position;
        let (left, top, right, bottom) = transform.map_position(left, top, right, bottom);
        self.positioned_clipped(left, top, right, bottom, clip)
    }

    /// Crop this quad after its four position edges have already been mapped.
    /// Keeping the mapping outside avoids cloning an intermediate quad for the
    /// scaled-preview path, which touches thousands of tiny glyph quads.
    fn positioned_clipped(
        &self,
        left: f32,
        top: f32,
        right: f32,
        bottom: f32,
        clip: QuadClipRect,
    ) -> Option<Self> {
        if !(right > left
            && bottom > top
            && clip.right() > clip.left()
            && clip.bottom() > clip.top())
        {
            return None;
        }

        let visible_left = left.max(clip.left());
        let visible_top = top.max(clip.top());
        let visible_right = right.min(clip.right());
        let visible_bottom = bottom.min(clip.bottom());
        if visible_right <= visible_left || visible_bottom <= visible_top {
            return None;
        }

        let x0 = (visible_left - left) / (right - left);
        let x1 = (visible_right - left) / (right - left);
        let y0 = (visible_top - top) / (bottom - top);
        let y1 = (visible_bottom - top) / (bottom - top);
        let (u0, u1, v0, v1) = self.tex;
        let source_colors = self
            .fg_color_corners
            .as_deref()
            .copied()
            .unwrap_or([self.fg_color; VERTICES_PER_CELL]);
        let clipped_colors = [
            bilerp_color(source_colors, x0, y0),
            bilerp_color(source_colors, x1, y0),
            bilerp_color(source_colors, x0, y1),
            bilerp_color(source_colors, x1, y1),
        ];
        let fg_color = clipped_colors[V_TOP_LEFT];

        let mut clipped = Self {
            position: (visible_left, visible_top, visible_right, visible_bottom),
            fg_color,
            fg_color_corners: clipped_colors
                .iter()
                .skip(1)
                .any(|color| *color != fg_color)
                .then(|| Box::new(clipped_colors)),
            alt_color: self.alt_color,
            tex: (
                lerp(u0, u1, x0),
                lerp(u0, u1, x1),
                lerp(v0, v1, y0),
                lerp(v0, v1, y1),
            ),
            hsv: self.hsv,
            has_color: self.has_color,
            mix_value: self.mix_value,
        };
        // Solid-colour quads conventionally use zero-width texture intervals.
        // Preserve those intervals exactly rather than introducing tiny
        // floating-point differences while cropping.
        if u0 == u1 {
            clipped.tex.0 = u0;
            clipped.tex.1 = u1;
        }
        if v0 == v1 {
            clipped.tex.2 = v0;
            clipped.tex.3 = v1;
        }
        Some(clipped)
    }
}

fn lerp(start: f32, end: f32, amount: f32) -> f32 {
    start + (end - start) * amount
}

fn lerp_color(start: [f32; 4], end: [f32; 4], amount: f32) -> [f32; 4] {
    std::array::from_fn(|index| lerp(start[index], end[index], amount))
}

fn bilerp_color(corners: [[f32; 4]; 4], x: f32, y: f32) -> [f32; 4] {
    let top = lerp_color(corners[V_TOP_LEFT], corners[V_TOP_RIGHT], x);
    let bottom = lerp_color(corners[V_BOT_LEFT], corners[V_BOT_RIGHT], x);
    lerp_color(top, bottom, y)
}

#[derive(Default, Clone)]
pub struct HeapQuadAllocator {
    // Quads are stored inline: a cached surface with hundreds of thousands of
    // quads would otherwise be that many separate 96-byte allocations, which
    // both fragments the heap and makes every replay a pointer chase. Callers
    // only ever hold the `QuadImpl` borrow from `allocate` until the next
    // allocation, so element addresses do not need to survive vector growth.
    layer0: Vec<BoxedQuad>,
    layer1: Vec<BoxedQuad>,
    layer2: Vec<BoxedQuad>,
    position_transform: Option<QuadPositionTransform>,
}

impl std::fmt::Debug for HeapQuadAllocator {
    fn fmt(&self, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        fmt.debug_struct("HeapQuadAllocator").finish()
    }
}

/// A clip rectangle in the window-centre-relative space that quad positions
/// live in.
///
/// Every painter goes through `filled_rectangle` and friends, which subtract
/// half the window size from each edge, so a quad's `position` is *not* in the
/// same coordinates as the layout rects (`workspace_sidebar_rect`, the list
/// viewport, …) that describe where it was asked to go. Clipping against an
/// un-rebased rect does not fail loudly: every quad simply falls outside and
/// is dropped. That is how the space-swipe transition once drew an entirely
/// empty sidebar while every offset in the logs looked correct.
///
/// The fields are private and [`Self::from_top_left_pixels`] is the only way
/// in, so layout coordinates cannot reach the clipper by accident. Bands
/// carved out of an existing rect go through [`Self::with_vertical`] /
/// [`Self::with_horizontal`], which inherit the already-converted edges.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct QuadClipRect {
    left: f32,
    top: f32,
    right: f32,
    bottom: f32,
}

impl QuadClipRect {
    /// Rebase a top-left pixel rect -- sidebar geometry, a list viewport, a
    /// UI item's bounds -- into the quads' own space.
    ///
    /// Takes the window's own [`Dimensions`] rather than a pair of numbers on
    /// purpose: the conversion is only correct against the surface the quads
    /// were laid out for, and a caller with two loose `usize`s can silently
    /// pass the wrong ones (or zeroes) and get a rect that quietly matches
    /// nothing.
    pub fn from_top_left_pixels(
        left: f32,
        top: f32,
        right: f32,
        bottom: f32,
        dimensions: &Dimensions,
    ) -> Self {
        let dx = dimensions.pixel_width as f32 / 2.0;
        let dy = dimensions.pixel_height as f32 / 2.0;
        Self {
            left: left - dx,
            top: top - dy,
            right: right - dx,
            bottom: bottom - dy,
        }
    }

    pub fn left(&self) -> f32 {
        self.left
    }

    pub fn top(&self) -> f32 {
        self.top
    }

    pub fn right(&self) -> f32 {
        self.right
    }

    pub fn bottom(&self) -> f32 {
        self.bottom
    }

    pub fn width(&self) -> f32 {
        self.right - self.left
    }

    /// A horizontal band of this rect. Both edges are already centre-relative.
    pub fn with_vertical(&self, top: f32, bottom: f32) -> Self {
        Self {
            top,
            bottom,
            ..*self
        }
    }

    /// A vertical band of this rect. Both edges are already centre-relative.
    pub fn with_horizontal(&self, left: f32, right: f32) -> Self {
        Self {
            left,
            right,
            ..*self
        }
    }
}

/// How many quads a [`HeapQuadAllocator`] held at some point during a paint.
///
/// Splitting a recorded frame by screen position cannot distinguish a scrolling
/// row from the chrome drawn over it: the two overlap on purpose, so any
/// horizontal line drawn through them cuts something in half. Recording where
/// the painter *was* instead splits by what was being drawn, which is the
/// question a compositor actually needs answered.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HeapQuadMark {
    layer0: usize,
    layer1: usize,
    layer2: usize,
}

impl HeapQuadAllocator {
    /// Empty the allocator but keep its capacity for whatever is recorded
    /// next. For a caller that records many surfaces in a row -- the overview
    /// draws one per visible card, every frame -- this makes re-recording
    /// allocation-free.
    pub fn recycle(&mut self) {
        // Destructured so that a new field cannot be forgotten here and leak
        // from one recording into the next.
        let Self {
            layer0,
            layer1,
            layer2,
            position_transform,
        } = self;
        layer0.clear();
        layer1.clear();
        layer2.clear();
        *position_transform = None;
    }

    /// Move the recording into a new allocator whose vectors have no spare
    /// capacity, leaving this one empty but keeping its buffers for whatever
    /// is recorded next.
    ///
    /// This is how a scratch recorder hands a finished surface to a
    /// byte-accounted cache. `resident_bytes()` counts capacity, so a buffer
    /// that grew by doubling would charge the cache ~33% more than it holds
    /// and keep that hole resident for the life of the entry. The quads are
    /// moved, not cloned: `BoxedQuad::clone` deep-copies every corner
    /// gradient box, so a clone would trade the doubling slack for an
    /// allocation per gradient quad.
    ///
    /// The position transform is a recording-time setting (applied as quads
    /// are allocated), so it is cleared on both sides: the finished surface
    /// is only ever replayed, and the recorder starts its next line clean.
    pub fn take_exact(&mut self) -> Self {
        fn move_exact(src: &mut Vec<BoxedQuad>) -> Vec<BoxedQuad> {
            // with_capacity + append rather than collect: the exactness is
            // the point, and must not depend on an iterator specialisation.
            let mut out = Vec::with_capacity(src.len());
            out.append(src);
            out
        }
        let Self {
            layer0,
            layer1,
            layer2,
            position_transform,
        } = self;
        *position_transform = None;
        Self {
            layer0: move_exact(layer0),
            layer1: move_exact(layer1),
            layer2: move_exact(layer2),
            position_transform: None,
        }
    }

    /// Estimated resident heap bytes for the recorded quads. Counts vector
    /// capacity, not length: a buffer that grew during recording keeps that
    /// allocation until it is dropped, and capacity is what a byte-accounted
    /// cache needs to know about.
    pub fn resident_bytes(&self) -> usize {
        (self.layer0.capacity() + self.layer1.capacity() + self.layer2.capacity())
            .saturating_mul(std::mem::size_of::<BoxedQuad>())
    }

    /// Where the next quad will land, for later use with [`Self::apply_before`],
    /// [`Self::apply_between`] and [`Self::apply_after`].
    pub fn mark(&self) -> HeapQuadMark {
        HeapQuadMark {
            layer0: self.layer0.len(),
            layer1: self.layer1.len(),
            layer2: self.layer2.len(),
        }
    }

    fn layers(&self) -> [(usize, &Vec<BoxedQuad>); 3] {
        [(0, &self.layer0), (1, &self.layer1), (2, &self.layer2)]
    }

    fn layer_bounds(mark: &HeapQuadMark, layer_num: usize) -> usize {
        match layer_num {
            0 => mark.layer0,
            1 => mark.layer1,
            2 => mark.layer2,
            _ => unreachable!(),
        }
    }

    pub fn apply_to(&self, other: &mut dyn TripleLayerQuadAllocatorTrait) -> anyhow::Result<()> {
        #[cfg(not(target_family = "wasm"))]
        let start = std::time::Instant::now();
        for (layer_num, quads) in self.layers() {
            for quad in quads {
                other.extend_with(layer_num, &quad.to_vertices());
            }
        }
        #[cfg(not(target_family = "wasm"))]
        metrics::histogram!("quad_buffer_apply").record(start.elapsed());
        Ok(())
    }

    /// `apply_to`, with every quad shifted by `offset` on the way out and
    /// nothing else touched: no cropping, no texture arithmetic, no
    /// allocation. The replay for rows that are wholly inside their pane
    /// while the pane is scrolled by a fraction of a row; the two rows at
    /// the edges go through `apply_to_clipped_at` instead.
    pub fn apply_to_at(
        &self,
        other: &mut dyn TripleLayerQuadAllocatorTrait,
        offset_x: f32,
        offset_y: f32,
    ) -> anyhow::Result<()> {
        for (layer_num, quads) in self.layers() {
            for quad in quads {
                let mut vertices = quad.to_vertices();
                for vertex in vertices.iter_mut() {
                    vertex.position[0] += offset_x;
                    vertex.position[1] += offset_y;
                }
                other.extend_with(layer_num, &vertices);
            }
        }
        Ok(())
    }

    /// Copy every recorded quad into `other`, cropped to `clip` and scaled to
    /// `opacity`.
    /// Flatten the recorded quads into a plain vertex stream: sub-layer 0,
    /// then 1, then 2, in recorded order -- the order a replay would draw
    /// them. Used to render a card's picture into its own texture, where
    /// the render pass's projection does the placement and cropping, so the
    /// vertices go out untransformed and unclipped.
    pub fn extract_vertices(&self, out: &mut Vec<Vertex>) {
        for (_layer_num, quads) in self.layers() {
            for quad in quads {
                out.extend_from_slice(&quad.to_vertices());
            }
        }
    }

    pub fn quad_count(&self) -> usize {
        self.layers().iter().map(|(_, quads)| quads.len()).sum()
    }

    /// Position, texture coordinates and per-corner colors are transformed
    /// together by the same primitive used by Space swipe composition.
    ///
    /// See [`BoxedQuad::with_opacity`] for which quads an opacity below 1
    /// actually reaches.
    pub fn apply_to_clipped(
        &self,
        other: &mut dyn TripleLayerQuadAllocatorTrait,
        clip: QuadClipRect,
        opacity: f32,
    ) -> anyhow::Result<()> {
        self.apply_to_clipped_at(other, 0.0, 0.0, clip, opacity)
    }

    /// `apply_to_clipped`, shifted by `offset` on the way out.
    ///
    /// Lets a caller record a surface in its own local space — its left edge
    /// at 0 — and place it on replay. That matters when the surface's real
    /// origin is off-screen: the painters take unsigned pixel coordinates, so
    /// a negative origin cannot be expressed at record time, and clamping it
    /// to 0 lays the contents out from the wrong place.
    pub fn apply_to_clipped_at(
        &self,
        other: &mut dyn TripleLayerQuadAllocatorTrait,
        offset_x: f32,
        offset_y: f32,
        clip: QuadClipRect,
        opacity: f32,
    ) -> anyhow::Result<()> {
        #[cfg(not(target_family = "wasm"))]
        let started = std::time::Instant::now();
        for (layer_num, quads) in self.layers() {
            for quad in quads {
                let Some(clipped) = quad.translated_clipped(offset_x, offset_y, clip) else {
                    continue;
                };
                let clipped = if opacity < 1.0 {
                    clipped.with_opacity(opacity)
                } else {
                    clipped
                };
                other.extend_with(layer_num, &clipped.to_vertices());
            }
        }
        #[cfg(not(target_family = "wasm"))]
        metrics::histogram!("quad_buffer_apply_clipped").record(started.elapsed());
        Ok(())
    }

    /// Replay this whole surface scaled into `target`, cropped to `clip` and
    /// composited at `opacity`.
    ///
    /// `source` is the rect the surface was recorded in; the two together give
    /// the scale and offset. Used to fly a captured window into the card it
    /// becomes.
    pub fn apply_to_scaled(
        &self,
        other: &mut dyn TripleLayerQuadAllocatorTrait,
        source: QuadClipRect,
        target: QuadClipRect,
        clip: QuadClipRect,
        opacity: f32,
    ) -> anyhow::Result<()> {
        #[cfg(not(target_family = "wasm"))]
        let started = std::time::Instant::now();
        let Some(transform) = QuadPositionTransform::new(source, target) else {
            return Ok(());
        };
        for (layer_num, quads) in self.layers() {
            for quad in quads {
                let Some(mapped) = quad.transformed_clipped(transform, clip) else {
                    continue;
                };
                let mapped = if opacity < 1.0 {
                    mapped.with_opacity(opacity)
                } else {
                    mapped
                };
                other.extend_with(layer_num, &mapped.to_vertices());
            }
        }
        #[cfg(not(target_family = "wasm"))]
        metrics::histogram!("quad_buffer_apply_scaled").record(started.elapsed());
        Ok(())
    }

    /// Replay everything recorded before `mark`, in place.
    pub fn apply_before(
        &self,
        other: &mut dyn TripleLayerQuadAllocatorTrait,
        mark: &HeapQuadMark,
    ) -> anyhow::Result<()> {
        for (layer_num, quads) in self.layers() {
            let end = Self::layer_bounds(mark, layer_num).min(quads.len());
            for quad in &quads[..end] {
                other.extend_with(layer_num, &quad.to_vertices());
            }
        }
        Ok(())
    }

    /// Replay everything recorded from `mark` onwards, in place.
    pub fn apply_after(
        &self,
        other: &mut dyn TripleLayerQuadAllocatorTrait,
        mark: &HeapQuadMark,
    ) -> anyhow::Result<()> {
        for (layer_num, quads) in self.layers() {
            let begin = Self::layer_bounds(mark, layer_num).min(quads.len());
            for quad in &quads[begin..] {
                other.extend_with(layer_num, &quad.to_vertices());
            }
        }
        Ok(())
    }

    /// Replay `start..end`, shifted horizontally and clipped.
    ///
    /// The clip is a containment bound -- it stops the shifted quads escaping
    /// the surface they belong to -- and not a way to carve the frame into
    /// moving and stationary parts. Use the marks for that.
    pub fn apply_between(
        &self,
        other: &mut dyn TripleLayerQuadAllocatorTrait,
        start: &HeapQuadMark,
        end: &HeapQuadMark,
        offset_x: f32,
        clip: QuadClipRect,
    ) -> anyhow::Result<()> {
        #[cfg(not(target_family = "wasm"))]
        let started = std::time::Instant::now();
        for (layer_num, quads) in self.layers() {
            let begin = Self::layer_bounds(start, layer_num).min(quads.len());
            let finish = Self::layer_bounds(end, layer_num)
                .min(quads.len())
                .max(begin);
            for quad in &quads[begin..finish] {
                let Some(clipped) = quad.translated_clipped(offset_x, 0.0, clip) else {
                    continue;
                };
                other.extend_with(layer_num, &clipped.to_vertices());
            }
        }
        #[cfg(not(target_family = "wasm"))]
        metrics::histogram!("quad_buffer_translated_rect_clip_apply").record(started.elapsed());
        Ok(())
    }

    /// Replay every recorded quad into ONE sub-layer of `other`, shifted
    /// horizontally and cropped to `clip`.
    ///
    /// The sub-layer a quad was recorded in decides only *when* it is drawn:
    /// each layer's three buffers are submitted 0, 1, 2 whatever order they
    /// were filled in (see `render/draw.rs`). A surface that has to float
    /// over the terminal therefore cannot keep its own sub-layers -- the
    /// terminal's glyphs live in sub-buffer 2 and would draw straight through
    /// a panel background recorded in sub-buffer 0. Collapsing the recording
    /// into the last sub-buffer, in recorded order, keeps the surface's own
    /// back-to-front order and puts all of it above anything drawn earlier:
    /// the same arrangement the context menu gets by painting its panel into
    /// sub-layer 2 by hand.
    pub fn apply_to_single_layer(
        &self,
        other: &mut dyn TripleLayerQuadAllocatorTrait,
        layer_num: usize,
        offset_x: f32,
        clip: QuadClipRect,
    ) -> anyhow::Result<()> {
        #[cfg(not(target_family = "wasm"))]
        let started = std::time::Instant::now();
        for (_recorded_layer, quads) in self.layers() {
            for quad in quads {
                let Some(clipped) = quad.translated_clipped(offset_x, 0.0, clip) else {
                    continue;
                };
                other.extend_with(layer_num, &clipped.to_vertices());
            }
        }
        #[cfg(not(target_family = "wasm"))]
        metrics::histogram!("quad_buffer_single_layer_apply").record(started.elapsed());
        Ok(())
    }

    pub fn set_position_transform(&mut self, transform: Option<QuadPositionTransform>) {
        self.position_transform = transform;
    }
}

impl TripleLayerQuadAllocatorTrait for HeapQuadAllocator {
    fn allocate(&mut self, layer_num: usize) -> anyhow::Result<QuadImpl<'_>> {
        let position_transform = self.position_transform;
        let quads = match layer_num {
            0 => &mut self.layer0,
            1 => &mut self.layer1,
            2 => &mut self.layer2,
            _ => unreachable!(),
        };

        quads.push(BoxedQuad::default());

        let quad = quads.last_mut().unwrap();
        Ok(match position_transform {
            Some(transform) => QuadImpl::TransformedBoxed(quad, transform),
            None => QuadImpl::Boxed(quad),
        })
    }

    fn extend_with(&mut self, layer_num: usize, vertices: &[Vertex]) {
        if vertices.is_empty() {
            return;
        }

        let position_transform = self.position_transform;
        let dest_quads = match layer_num {
            0 => &mut self.layer0,
            1 => &mut self.layer1,
            2 => &mut self.layer2,
            _ => unreachable!(),
        };

        // This is logically equivalent to
        // https://doc.rust-lang.org/std/primitive.slice.html#method.as_chunks_unchecked
        // which is currently nightly-only
        assert_eq!(vertices.len() % VERTICES_PER_CELL, 0);
        let src_quads: &[[Vertex; VERTICES_PER_CELL]] =
            unsafe { std::slice::from_raw_parts(vertices.as_ptr().cast(), vertices.len() / 4) };

        for quad in src_quads {
            let mut quad = BoxedQuad::from_vertices(quad);
            if let Some(transform) = position_transform {
                let (left, top, right, bottom) = quad.position;
                quad.position = transform.map_position(left, top, right, bottom);
            }
            dest_quads.push(quad);
        }
    }
}

#[cfg(test)]
const TEST_ORIGIN: Dimensions = Dimensions {
    pixel_width: 0,
    pixel_height: 0,
    dpi: 96,
};

#[cfg(test)]
#[test]
fn size() {
    assert_eq!(std::mem::size_of::<Vertex>() * VERTICES_PER_CELL, 272);
    assert_eq!(std::mem::size_of::<BoxedQuad>(), 96);
}

#[cfg(test)]
fn record_quads(heap: &mut HeapQuadAllocator, per_layer: [usize; 3]) {
    let layers = heap;
    for (layer_num, count) in per_layer.into_iter().enumerate() {
        for i in 0..count {
            let x = (layer_num * 1000 + i * 10) as f32;
            let mut quad = layers.allocate(layer_num).unwrap();
            quad.set_position(x, 0.0, x + 5.0, 5.0);
        }
    }
}

#[cfg(test)]
#[test]
fn take_exact_produces_exact_capacity_and_keeps_the_scratch_buffers() {
    let mut scratch = HeapQuadAllocator::default();
    record_quads(&mut scratch, [5, 3, 0]);
    let grown = scratch.resident_bytes();

    let out = scratch.take_exact();
    assert_eq!(out.quad_count(), 8);
    assert_eq!(out.resident_bytes(), 8 * std::mem::size_of::<BoxedQuad>());
    assert_eq!(scratch.quad_count(), 0);
    assert_eq!(scratch.resident_bytes(), grown, "the scratch keeps its buffers");
    assert!(grown >= out.resident_bytes());
}

#[cfg(test)]
#[test]
fn take_exact_preserves_layer_assignment_and_recorded_order() {
    let mut scratch = HeapQuadAllocator::default();
    record_quads(&mut scratch, [2, 1, 3]);
    let out = scratch.take_exact();
    assert_eq!(out.layer0.len(), 2);
    assert_eq!(out.layer1.len(), 1);
    assert_eq!(out.layer2.len(), 3);
    assert_eq!(out.layer0[1].position, (10.0, 0.0, 15.0, 5.0));
    assert_eq!(out.layer1[0].position, (1000.0, 0.0, 1005.0, 5.0));
    assert_eq!(out.layer2[2].position, (2020.0, 0.0, 2025.0, 5.0));
}

#[cfg(test)]
#[test]
fn take_exact_moves_corner_gradients_intact() {
    let mut scratch = HeapQuadAllocator::default();
    {
        let layers = &mut scratch;
        let mut quad = layers.allocate(1).unwrap();
        quad.set_corner_gradient(
            LinearRgba::with_components(0.1, 0.0, 0.0, 1.0),
            LinearRgba::with_components(0.2, 0.0, 0.0, 1.0),
            LinearRgba::with_components(0.3, 0.0, 0.0, 1.0),
            LinearRgba::with_components(0.4, 0.0, 0.0, 1.0),
        );
    }
    let out = scratch.take_exact();
    let vertices = out.layer1[0].to_vertices();
    assert_eq!(vertices[V_TOP_LEFT].fg_color, [0.1, 0.0, 0.0, 1.0]);
    assert_eq!(vertices[V_TOP_RIGHT].fg_color, [0.2, 0.0, 0.0, 1.0]);
    assert_eq!(vertices[V_BOT_LEFT].fg_color, [0.3, 0.0, 0.0, 1.0]);
    assert_eq!(vertices[V_BOT_RIGHT].fg_color, [0.4, 0.0, 0.0, 1.0]);
}

#[cfg(test)]
#[test]
fn a_recycled_scratch_records_the_same_surface_as_a_fresh_allocator() {
    let line_a = [4usize, 7, 1];
    let mut fresh = HeapQuadAllocator::default();
    record_quads(&mut fresh, line_a);

    // A wider line first, so the scratch's buffers are larger than line A
    // needs when it is recorded second.
    let mut scratch = HeapQuadAllocator::default();
    record_quads(&mut scratch, [40, 70, 10]);
    let _wide = scratch.take_exact();
    scratch.recycle();
    record_quads(&mut scratch, line_a);
    let reused = scratch.take_exact();

    let (mut expected, mut actual) = (Vec::new(), Vec::new());
    fresh.extract_vertices(&mut expected);
    reused.extract_vertices(&mut actual);
    assert_eq!(actual.len(), expected.len());
    for (a, e) in actual.iter().zip(expected.iter()) {
        assert_eq!(a.position, e.position);
        assert_eq!(a.fg_color, e.fg_color);
        assert_eq!(a.tex, e.tex);
    }
    assert_eq!(reused.quad_count(), fresh.quad_count());
    assert_eq!(reused.resident_bytes(), 12 * std::mem::size_of::<BoxedQuad>());
}

#[cfg(test)]
#[test]
fn take_exact_clears_the_position_transform_on_both_sides() {
    let source = QuadClipRect::from_top_left_pixels(0.0, 0.0, 100.0, 100.0, &TEST_ORIGIN);
    let target = QuadClipRect::from_top_left_pixels(0.0, 0.0, 200.0, 200.0, &TEST_ORIGIN);
    let mut scratch = HeapQuadAllocator::default();
    {
        let layers = &mut scratch;
        layers.set_position_transform(Some(QuadPositionTransform::new(source, target).unwrap()));
        let mut quad = layers.allocate(0).unwrap();
        quad.set_position(10.0, 10.0, 20.0, 20.0);
    }
    let mut out = scratch.take_exact();
    // Recorded under the transform, so mapped.
    assert_eq!(out.layer0[0].position, (20.0, 20.0, 40.0, 40.0));
    // Neither side keeps the transform: a quad recorded into either
    // afterwards lands where it was placed.
    for heap in [&mut scratch, &mut out] {
        let layers = heap;
        let mut quad = layers.allocate(1).unwrap();
        quad.set_position(10.0, 10.0, 20.0, 20.0);
    }
    assert_eq!(scratch.layer1[0].position, (10.0, 10.0, 20.0, 20.0));
    assert_eq!(out.layer1[0].position, (10.0, 10.0, 20.0, 20.0));
}

#[cfg(test)]
#[test]
fn boxed_quad_preserves_vertical_gradient_colors() {
    let top = LinearRgba::with_components(0.1, 0.2, 0.3, 1.0);
    let bottom = LinearRgba::with_components(0.4, 0.5, 0.6, 1.0);
    let mut quad = BoxedQuad::default();
    quad.set_vertical_gradient(top, bottom);
    let vertices = quad.to_vertices();
    let top: [f32; 4] = top.into();
    let bottom: [f32; 4] = bottom.into();
    assert_eq!(vertices[V_TOP_LEFT].fg_color, top);
    assert_eq!(vertices[V_TOP_RIGHT].fg_color, top);
    assert_eq!(vertices[V_BOT_LEFT].fg_color, bottom);
    assert_eq!(vertices[V_BOT_RIGHT].fg_color, bottom);
}

#[cfg(test)]
#[test]
fn boxed_quad_preserves_corner_gradient_colors() {
    let top_left = LinearRgba::with_components(0.1, 0.2, 0.3, 1.0);
    let top_right = LinearRgba::with_components(0.2, 0.3, 0.4, 1.0);
    let bottom_left = LinearRgba::with_components(0.3, 0.4, 0.5, 1.0);
    let bottom_right = LinearRgba::with_components(0.4, 0.5, 0.6, 1.0);
    let mut quad = BoxedQuad::default();
    quad.set_corner_gradient(top_left, top_right, bottom_left, bottom_right);
    let vertices = quad.to_vertices();
    assert_eq!(vertices[V_TOP_LEFT].fg_color, <[f32; 4]>::from(top_left));
    assert_eq!(vertices[V_TOP_RIGHT].fg_color, <[f32; 4]>::from(top_right));
    assert_eq!(vertices[V_BOT_LEFT].fg_color, <[f32; 4]>::from(bottom_left));
    assert_eq!(
        vertices[V_BOT_RIGHT].fg_color,
        <[f32; 4]>::from(bottom_right)
    );
}

#[cfg(test)]
#[test]
fn boxed_quad_keeps_solid_colors_inline() {
    let color = LinearRgba::with_components(0.1, 0.2, 0.3, 1.0);
    let mut quad = BoxedQuad::default();
    quad.set_fg_color(color);
    assert!(quad.fg_color_corners.is_none());
    quad.set_vertical_gradient(color, color);
    assert!(quad.fg_color_corners.is_none());
}

#[cfg(test)]
#[test]
fn boxed_quad_translation_and_clip_crop_position_texture_and_gradient_together() {
    let mut quad = BoxedQuad::default();
    quad.set_position(0.0, 0.0, 100.0, 80.0);
    quad.set_texture_discrete(0.1, 0.9, 0.2, 0.6);
    quad.set_corner_gradient(
        LinearRgba::with_components(0.0, 0.0, 0.0, 1.0),
        LinearRgba::with_components(1.0, 0.0, 0.0, 1.0),
        LinearRgba::with_components(0.0, 1.0, 0.0, 1.0),
        LinearRgba::with_components(1.0, 1.0, 0.0, 1.0),
    );

    let clip = QuadClipRect::from_top_left_pixels(35.0, 25.0, 85.0, 65.0, &TEST_ORIGIN);
    let clipped = quad.translated_clipped(10.0, 5.0, clip).unwrap();
    assert_eq!(clipped.position, (35.0, 25.0, 85.0, 65.0));
    for (actual, expected) in std::iter::zip(
        [clipped.tex.0, clipped.tex.1, clipped.tex.2, clipped.tex.3]
            .iter()
            .copied(),
        [0.3, 0.7, 0.3, 0.5].iter().copied(),
    ) {
        assert!((actual - expected).abs() < 0.000_001);
    }
    let colors = clipped.fg_color_corners.unwrap();
    assert_eq!(colors[V_TOP_LEFT], [0.25, 0.25, 0.0, 1.0]);
    assert_eq!(colors[V_TOP_RIGHT], [0.75, 0.25, 0.0, 1.0]);
    assert_eq!(colors[V_BOT_LEFT], [0.25, 0.75, 0.0, 1.0]);
    assert_eq!(colors[V_BOT_RIGHT], [0.75, 0.75, 0.0, 1.0]);
}

#[cfg(test)]
#[test]
fn heap_position_transform_maps_new_and_cached_quads_in_one_pass() {
    let source = QuadClipRect::from_top_left_pixels(0.0, 0.0, 100.0, 100.0, &TEST_ORIGIN);
    let target = QuadClipRect::from_top_left_pixels(10.0, 20.0, 210.0, 70.0, &TEST_ORIGIN);
    let mut heap = HeapQuadAllocator::default();
    {
        let mut layers = &mut heap;
        layers.set_position_transform(Some(QuadPositionTransform::new(source, target).unwrap()));

        let mut authored = layers.allocate(0).unwrap();
        authored.set_position(20.0, 10.0, 40.0, 50.0);
        drop(authored);

        let mut cached = BoxedQuad::default();
        cached.set_position(20.0, 10.0, 40.0, 50.0);
        layers.extend_with(1, &cached.to_vertices());
        layers.set_position_transform(None);
    }

    assert_eq!(heap.layer0[0].position, (50.0, 25.0, 90.0, 45.0));
    assert_eq!(heap.layer1[0].position, (50.0, 25.0, 90.0, 45.0));
}

#[cfg(test)]
#[test]
fn single_layer_replay_flattens_three_sub_layers_in_recorded_order() {
    let mut heap = HeapQuadAllocator::default();
    {
        let mut layers = &mut heap;
        for (layer_num, x) in [(0usize, 0.0f32), (1, 100.0), (2, 200.0)] {
            let mut quad = layers.allocate(layer_num).unwrap();
            quad.set_position(x, 0.0, x + 50.0, 50.0);
        }
    }

    let mut target_heap = HeapQuadAllocator::default();
    {
        let mut target = &mut target_heap;
        let clip = QuadClipRect::from_top_left_pixels(-500.0, -500.0, 500.0, 500.0, &TEST_ORIGIN);
        heap.apply_to_single_layer(&mut target, 2, -10.0, clip)
            .unwrap();
    }

    assert!(target_heap.layer0.is_empty());
    assert!(target_heap.layer1.is_empty());
    assert_eq!(target_heap.layer2.len(), 3);
    // Recorded order 0 -> 1 -> 2 survives as draw order, shifted by -10.
    assert_eq!(target_heap.layer2[0].position, (-10.0, 0.0, 40.0, 50.0));
    assert_eq!(target_heap.layer2[1].position, (90.0, 0.0, 140.0, 50.0));
    assert_eq!(target_heap.layer2[2].position, (190.0, 0.0, 240.0, 50.0));
}

#[cfg(test)]
#[test]
fn single_layer_replay_drops_quads_the_shift_pushes_out_of_the_clip() {
    let mut heap = HeapQuadAllocator::default();
    {
        let mut layers = &mut heap;
        let mut quad = layers.allocate(0).unwrap();
        quad.set_position(0.0, 0.0, 50.0, 50.0);
    }

    let mut target_heap = HeapQuadAllocator::default();
    {
        let mut target = &mut target_heap;
        let clip = QuadClipRect::from_top_left_pixels(0.0, 0.0, 500.0, 500.0, &TEST_ORIGIN);
        // Shifted fully left of the clip: nothing lands.
        heap.apply_to_single_layer(&mut target, 2, -100.0, clip)
            .unwrap();
    }
    assert_eq!(target_heap.quad_count(), 0);
}

#[cfg(test)]
#[test]
fn boxed_quad_clip_rejects_outside_and_preserves_inside() {
    let mut quad = BoxedQuad::default();
    quad.set_position(10.0, 20.0, 30.0, 40.0);
    quad.set_texture_discrete(0.2, 0.6, 0.3, 0.7);
    let outside = QuadClipRect::from_top_left_pixels(30.0, 0.0, 50.0, 50.0, &TEST_ORIGIN);
    assert!(quad.translated_clipped(0.0, 0.0, outside).is_none());
    let full = QuadClipRect::from_top_left_pixels(0.0, 0.0, 50.0, 50.0, &TEST_ORIGIN);
    let inside = quad.translated_clipped(0.0, 0.0, full).unwrap();
    assert_eq!(inside.position, quad.position);
    assert_eq!(inside.tex, quad.tex);
}

#[cfg(test)]
mod translated_clip_tests {
    use super::*;

    /// Fixtures below that predate the rebase work state their coordinates
    /// already centre-relative, so they convert against a zero-sized window.
    const ORIGIN: Dimensions = Dimensions {
        pixel_width: 0,
        pixel_height: 0,
        dpi: 96,
    };

    #[test]
    fn translated_rect_clip_moves_positions_and_crops_texture_coordinates_together() {
        let quad = BoxedQuad {
            position: (10.0, 2.0, 30.0, 8.0),
            tex: (0.2, 0.6, 0.1, 0.9),
            ..Default::default()
        };
        let clip = QuadClipRect::from_top_left_pixels(25.0, 3.0, 35.0, 7.0, &ORIGIN);
        let vertices = quad
            .translated_clipped(10.0, 0.0, clip)
            .expect("visible clip")
            .to_vertices();

        assert_eq!(vertices[V_TOP_LEFT].position, [25.0, 3.0]);
        assert_eq!(vertices[V_BOT_RIGHT].position, [35.0, 7.0]);
        assert!((vertices[V_TOP_LEFT].tex[0] - 0.3).abs() < 0.0001);
        assert!((vertices[V_TOP_RIGHT].tex[0] - 0.5).abs() < 0.0001);
        assert!((vertices[V_TOP_LEFT].tex[1] - (0.1 + 0.8 / 6.0)).abs() < 0.0001);
        assert!((vertices[V_BOT_LEFT].tex[1] - (0.9 - 0.8 / 6.0)).abs() < 0.0001);
    }

    /// Regression: the swipe transition built its clip rect straight from the
    /// sidebar's top-left layout geometry and handed it to the quad clipper,
    /// whose quads are window-centre relative. Every one of the ~300 quads
    /// failed the bounds test, so the sidebar drew nothing at all for the
    /// whole 220ms -- with correct-looking offsets in every log. Nothing about
    /// that is visible from inside the clipper, so pin the conversion here.
    #[test]
    fn a_clip_rect_is_useless_until_it_is_rebased_into_the_quads_own_space() {
        let window = Dimensions {
            pixel_width: 2560,
            pixel_height: 1600,
            dpi: 144,
        };
        // A window of zero size is the only way to express the old bug now.
        let no_window = Dimensions {
            pixel_width: 0,
            pixel_height: 0,
            dpi: 144,
        };

        // A sidebar row as a painter would emit it: laid out at top-left
        // pixels x 0..465, y 300..320, then written centre-relative.
        let row = QuadClipRect::from_top_left_pixels(0.0, 300.0, 465.0, 320.0, &window);
        let quad = BoxedQuad {
            position: (row.left(), row.top(), row.right(), row.bottom()),
            ..Default::default()
        };

        // The list viewport in the layout's own coordinates. Used as-is, it
        // silently discards a row that is plainly inside it.
        // `QuadClipRect` has private fields, so this is only expressible in a
        // test that reaches for the raw numbers on purpose.
        let unrebased = QuadClipRect::from_top_left_pixels(0.0, 204.0, 465.0, 1316.0, &no_window);
        assert!(
            quad.translated_clipped(0.0, 0.0, unrebased).is_none(),
            "an un-rebased clip rect drops everything -- this is the failure \
             mode, not a healthy result"
        );

        // Rebased against the real window, the row survives.
        let list = QuadClipRect::from_top_left_pixels(0.0, 204.0, 465.0, 1316.0, &window);
        assert!(quad.translated_clipped(0.0, 0.0, list).is_some());
    }

    /// Regression: the transition used to decide what slides by cutting the
    /// sidebar at a y coordinate. It cannot work. The bottom fade and the
    /// settings row are painted *over* the list on purpose, so any horizontal
    /// line drawn between "moving" and "stationary" runs straight through
    /// whichever list row happens to reach that far -- the row's top half slid
    /// away while its bottom half stayed put, already showing the destination.
    /// Splitting by paint order instead asks the question that has an answer.
    #[test]
    fn chrome_painted_over_the_list_stays_put_when_the_list_slides_under_it() {
        const WINDOW: Dimensions = Dimensions {
            pixel_width: 1000,
            pixel_height: 800,
            dpi: 96,
        };
        fn quad_at(left: f32, top: f32, right: f32, bottom: f32) -> BoxedQuad {
            let rect = QuadClipRect::from_top_left_pixels(left, top, right, bottom, &WINDOW);
            BoxedQuad {
                position: (rect.left(), rect.top(), rect.right(), rect.bottom()),
                ..Default::default()
            }
        }
        // Left edge of a top-left `x`, in the centre-relative space quads use.
        let at = |x: f32| x - WINDOW.pixel_width as f32 / 2.0;

        let mut frame = HeapQuadAllocator::default();
        // Chrome above the list.
        frame.layer2.push(quad_at(0.0, 0.0, 400.0, 100.0));

        let list_start = frame.mark();
        // A row reaching down into the band the footer chrome covers. Inset
        // from the sidebar edges so a horizontal shift is visible rather than
        // being cropped away by the containment clip.
        frame.layer2.push(quad_at(100.0, 600.0, 300.0, 660.0));
        let list_end = frame.mark();

        // The fade and the settings row: painted last, and deliberately
        // overlapping the row above.
        frame.layer2.push(quad_at(0.0, 620.0, 400.0, 700.0));

        let mut out = HeapQuadAllocator::default();
        let mut sink = &mut out;
        let clip = QuadClipRect::from_top_left_pixels(0.0, 0.0, 400.0, 800.0, &WINDOW);
        frame.apply_before(&mut sink, &list_start).unwrap();
        frame
            .apply_between(&mut sink, &list_start, &list_end, -50.0, clip)
            .unwrap();
        frame.apply_after(&mut sink, &list_end).unwrap();
        drop(sink);

        let left_edges: Vec<f32> = out.layer2.iter().map(|q| q.position.0).collect();
        assert_eq!(
            left_edges,
            vec![at(0.0), at(50.0), at(0.0)],
            "only the marked list span may move, and the chrome recorded after \
             it must still be replayed last so it keeps masking the row"
        );
    }

    #[test]
    fn translated_rect_clip_discards_quads_outside_the_sidebar_page() {
        let quad = BoxedQuad {
            position: (10.0, 2.0, 30.0, 8.0),
            ..Default::default()
        };
        let full = QuadClipRect::from_top_left_pixels(0.0, 0.0, 40.0, 10.0, &ORIGIN);
        let below = QuadClipRect::from_top_left_pixels(0.0, 20.0, 40.0, 30.0, &ORIGIN);
        assert!(quad.translated_clipped(-100.0, 0.0, full).is_none());
        assert!(quad.translated_clipped(0.0, 0.0, below).is_none());
    }
}
