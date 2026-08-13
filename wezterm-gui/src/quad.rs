// Clippy hates the implement_vertex macro and won't let me scope
// this warning to its use
#![allow(clippy::unneeded_field_pattern)]

use crate::renderstate::BorrowedLayers;
use ::window::bitmaps::TextureRect;
use ::window::color::LinearRgba;
use ::window::Dimensions;
use config::HsbTransform;

/// Each cell is composed of two triangles built from 4 vertices.
/// The buffer is organized row by row.
pub const VERTICES_PER_CELL: usize = 4;
pub const V_TOP_LEFT: usize = 0;
pub const V_TOP_RIGHT: usize = 1;
pub const V_BOT_LEFT: usize = 2;
pub const V_BOT_RIGHT: usize = 3;

/// a regular monochrome text glyph
const IS_GLYPH: f32 = 0.0;
/// a color emoji glyph
const IS_COLOR_EMOJI: f32 = 1.0;
/// a full color texture attached as the
/// background image of the window
const IS_BG_IMAGE: f32 = 2.0;
/// like 2.0, except that instead of an
/// image, we use the solid bg color
const IS_SOLID_COLOR: f32 = 3.0;
/// Grayscale poly quad for non-aa text render layers
const IS_GRAY_SCALE: f32 = 4.0;

#[repr(C)]
#[derive(Copy, Clone, Default, Debug, bytemuck::Pod, bytemuck::Zeroable)]
pub struct Vertex {
    // Physical position of the corner of the character cell
    pub position: [f32; 2],
    // glyph texture
    pub tex: [f32; 2],
    pub fg_color: [f32; 4],
    pub alt_color: [f32; 4],
    pub hsv: [f32; 3],
    pub has_color: f32,
    pub mix_value: f32,
}
::window::glium::implement_vertex!(
    Vertex, position, tex, fg_color, alt_color, hsv, has_color, mix_value
);

impl Vertex {
    const ATTRIBS: [wgpu::VertexAttribute; 7] = wgpu::vertex_attr_array![
    0 => Float32x2,
    1 => Float32x2,
    2 => Float32x4,
    3 => Float32x4,
    4 => Float32x3,
    5 => Float32,
    6 => Float32,
    ];

    pub fn desc() -> wgpu::VertexBufferLayout<'static> {
        wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<Self>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &Self::ATTRIBS,
        }
    }
}

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

    /// Must be called after set_fg_color
    fn set_alt_color_and_mix_value(&mut self, color: LinearRgba, mix_value: f32);

    fn set_hsv(&mut self, hsv: Option<HsbTransform>);
    fn set_position(&mut self, left: f32, top: f32, right: f32, bottom: f32);
}

pub enum QuadImpl<'a> {
    Vert(Quad<'a>),
    Boxed(&'a mut BoxedQuad),
    Tee(Quad<'a>, &'a mut BoxedQuad),
}

impl<'a> QuadTrait for QuadImpl<'a> {
    fn set_texture_discrete(&mut self, x1: f32, x2: f32, y1: f32, y2: f32) {
        match self {
            Self::Vert(q) => q.set_texture_discrete(x1, x2, y1, y2),
            Self::Boxed(q) => q.set_texture_discrete(x1, x2, y1, y2),
            Self::Tee(gpu, heap) => {
                gpu.set_texture_discrete(x1, x2, y1, y2);
                heap.set_texture_discrete(x1, x2, y1, y2);
            }
        }
    }

    fn set_has_color_impl(&mut self, has_color: f32) {
        match self {
            Self::Vert(q) => q.set_has_color_impl(has_color),
            Self::Boxed(q) => q.set_has_color_impl(has_color),
            Self::Tee(gpu, heap) => {
                gpu.set_has_color_impl(has_color);
                heap.set_has_color_impl(has_color);
            }
        }
    }

    fn set_fg_color(&mut self, color: LinearRgba) {
        match self {
            Self::Vert(q) => q.set_fg_color(color),
            Self::Boxed(q) => q.set_fg_color(color),
            Self::Tee(gpu, heap) => {
                gpu.set_fg_color(color);
                heap.set_fg_color(color);
            }
        }
    }

    fn set_alt_color_and_mix_value(&mut self, color: LinearRgba, mix_value: f32) {
        match self {
            Self::Vert(q) => q.set_alt_color_and_mix_value(color, mix_value),
            Self::Boxed(q) => q.set_alt_color_and_mix_value(color, mix_value),
            Self::Tee(gpu, heap) => {
                gpu.set_alt_color_and_mix_value(color, mix_value);
                heap.set_alt_color_and_mix_value(color, mix_value);
            }
        }
    }

    fn set_hsv(&mut self, hsv: Option<HsbTransform>) {
        match self {
            Self::Vert(q) => q.set_hsv(hsv),
            Self::Boxed(q) => q.set_hsv(hsv),
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
            Self::Tee(gpu, heap) => {
                gpu.set_position(left, top, right, bottom);
                heap.set_position(left, top, right, bottom);
            }
        }
    }
}

/// A helper for updating the 4 vertices that compose a glyph cell
pub struct Quad<'a> {
    pub(crate) vert: &'a mut [Vertex],
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

/// We prefer to allocate a quad at a time for HeapQuadAllocator
/// because we tend to end up with fairly large arrays of Vertex
/// and the total amount of contiguous memory is in the MB range,
/// which is a bit gnarly to reallocate, and can waste several MB
/// in unused capacity
#[derive(Default)]
pub struct BoxedQuad {
    position: (f32, f32, f32, f32),
    fg_color: [f32; 4],
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
        Self {
            tex: (x1, x2, y1, y2),
            position: (left, top, right, bottom),
            has_color: verts[V_TOP_LEFT].has_color,
            alt_color: verts[V_TOP_LEFT].alt_color,
            fg_color: verts[V_TOP_LEFT].fg_color,
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
        quad.set_fg_color(LinearRgba::with_components(
            self.fg_color[0],
            self.fg_color[1],
            self.fg_color[2],
            self.fg_color[3],
        ));
        quad.set_alt_color_and_mix_value(self.alt_color.into(), self.mix_value);

        vert
    }
}

#[derive(Default)]
pub struct HeapQuadAllocator {
    layer0: Vec<Box<BoxedQuad>>,
    layer1: Vec<Box<BoxedQuad>>,
    layer2: Vec<Box<BoxedQuad>>,
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
    /// Where the next quad will land, for later use with [`Self::apply_before`],
    /// [`Self::apply_between`] and [`Self::apply_after`].
    pub fn mark(&self) -> HeapQuadMark {
        HeapQuadMark {
            layer0: self.layer0.len(),
            layer1: self.layer1.len(),
            layer2: self.layer2.len(),
        }
    }

    fn layers(&self) -> [(usize, &Vec<Box<BoxedQuad>>); 3] {
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

    pub fn apply_to(&self, other: &mut TripleLayerQuadAllocator) -> anyhow::Result<()> {
        let start = std::time::Instant::now();
        for (layer_num, quads) in self.layers() {
            for quad in quads {
                other.extend_with(layer_num, &quad.to_vertices());
            }
        }
        metrics::histogram!("quad_buffer_apply").record(start.elapsed());
        Ok(())
    }

    /// Replay everything recorded before `mark`, in place.
    pub fn apply_before(
        &self,
        other: &mut TripleLayerQuadAllocator,
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
        other: &mut TripleLayerQuadAllocator,
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
        other: &mut TripleLayerQuadAllocator,
        start: &HeapQuadMark,
        end: &HeapQuadMark,
        offset_x: f32,
        clip: QuadClipRect,
    ) -> anyhow::Result<()> {
        let started = std::time::Instant::now();
        for (layer_num, quads) in self.layers() {
            let begin = Self::layer_bounds(start, layer_num).min(quads.len());
            let finish = Self::layer_bounds(end, layer_num)
                .min(quads.len())
                .max(begin);
            for quad in &quads[begin..finish] {
                let Some(vertices) = quad.translated_clipped_vertices(offset_x, clip) else {
                    continue;
                };
                other.extend_with(layer_num, &vertices);
            }
        }
        metrics::histogram!("quad_buffer_translated_rect_clip_apply").record(started.elapsed());
        Ok(())
    }
}

impl BoxedQuad {
    fn translated_clipped_vertices(
        &self,
        offset_x: f32,
        clip: QuadClipRect,
    ) -> Option<[Vertex; VERTICES_PER_CELL]> {
        let (clip_left, clip_top, clip_right, clip_bottom) =
            (clip.left(), clip.top(), clip.right(), clip.bottom());
        let (left, top, right, bottom) = self.position;
        let translated_left = left + offset_x;
        let translated_right = right + offset_x;
        if translated_right <= clip_left
            || translated_left >= clip_right
            || bottom <= clip_top
            || top >= clip_bottom
        {
            return None;
        }

        let visible_left = translated_left.max(clip_left);
        let visible_right = translated_right.min(clip_right);
        let visible_top = top.max(clip_top);
        let visible_bottom = bottom.min(clip_bottom);
        if visible_right <= visible_left
            || visible_bottom <= visible_top
            || right <= left
            || bottom <= top
        {
            return None;
        }

        let left_ratio = (visible_left - translated_left) / (translated_right - translated_left);
        let right_ratio = (visible_right - translated_left) / (translated_right - translated_left);
        let top_ratio = (visible_top - top) / (bottom - top);
        let bottom_ratio = (visible_bottom - top) / (bottom - top);
        let (tex_left, tex_right, tex_top, tex_bottom) = self.tex;
        let visible_tex_left = tex_left + (tex_right - tex_left) * left_ratio;
        let visible_tex_right = tex_left + (tex_right - tex_left) * right_ratio;
        let visible_tex_top = tex_top + (tex_bottom - tex_top) * top_ratio;
        let visible_tex_bottom = tex_top + (tex_bottom - tex_top) * bottom_ratio;

        let mut clipped = BoxedQuad {
            position: (visible_left, visible_top, visible_right, visible_bottom),
            fg_color: self.fg_color,
            alt_color: self.alt_color,
            tex: (
                visible_tex_left,
                visible_tex_right,
                visible_tex_top,
                visible_tex_bottom,
            ),
            hsv: self.hsv,
            has_color: self.has_color,
            mix_value: self.mix_value,
        };
        // Preserve a zero-width texture interval exactly for solid-colour
        // quads instead of manufacturing tiny floating-point differences.
        if tex_left == tex_right {
            clipped.tex.0 = tex_left;
            clipped.tex.1 = tex_right;
        }
        if tex_top == tex_bottom {
            clipped.tex.2 = tex_top;
            clipped.tex.3 = tex_bottom;
        }
        Some(clipped.to_vertices())
    }
}

impl TripleLayerQuadAllocatorTrait for HeapQuadAllocator {
    fn allocate(&mut self, layer_num: usize) -> anyhow::Result<QuadImpl<'_>> {
        let quads = match layer_num {
            0 => &mut self.layer0,
            1 => &mut self.layer1,
            2 => &mut self.layer2,
            _ => unreachable!(),
        };

        quads.push(Box::new(BoxedQuad::default()));

        let quad = quads.last_mut().unwrap();
        Ok(QuadImpl::Boxed(quad))
    }

    fn extend_with(&mut self, layer_num: usize, vertices: &[Vertex]) {
        if vertices.is_empty() {
            return;
        }

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
            dest_quads.push(Box::new(BoxedQuad::from_vertices(quad)));
        }
    }
}

pub enum TripleLayerQuadAllocator<'a> {
    Gpu(BorrowedLayers),
    Heap(&'a mut HeapQuadAllocator),
    Tee {
        gpu: BorrowedLayers,
        heap: &'a mut HeapQuadAllocator,
    },
}

impl<'a> TripleLayerQuadAllocator<'a> {
    /// The heap's current position, when there is a heap to record into.
    /// `Gpu` allocators cannot be replayed, so a painter drawing straight to
    /// the GPU has nothing to mark.
    pub fn heap_mark(&self) -> Option<HeapQuadMark> {
        match self {
            Self::Gpu(_) => None,
            Self::Heap(heap) => Some(heap.mark()),
            Self::Tee { heap, .. } => Some(heap.mark()),
        }
    }
}

impl<'a> TripleLayerQuadAllocatorTrait for TripleLayerQuadAllocator<'a> {
    fn allocate(&mut self, layer_num: usize) -> anyhow::Result<QuadImpl<'_>> {
        match self {
            Self::Gpu(b) => b.allocate(layer_num),
            Self::Heap(h) => h.allocate(layer_num),
            Self::Tee { gpu, heap } => {
                let gpu_quad = gpu.allocate(layer_num)?;
                let heap_quad = heap.allocate(layer_num)?;
                match (gpu_quad, heap_quad) {
                    (QuadImpl::Vert(gpu), QuadImpl::Boxed(heap)) => Ok(QuadImpl::Tee(gpu, heap)),
                    _ => unreachable!("tee allocators must pair GPU and heap quads"),
                }
            }
        }
    }

    fn extend_with(&mut self, layer_num: usize, vertices: &[Vertex]) {
        match self {
            Self::Gpu(b) => b.extend_with(layer_num, vertices),
            Self::Heap(h) => h.extend_with(layer_num, vertices),
            Self::Tee { gpu, heap } => {
                gpu.extend_with(layer_num, vertices);
                heap.extend_with(layer_num, vertices);
            }
        }
    }
}

#[cfg(test)]
#[test]
fn size() {
    assert_eq!(std::mem::size_of::<Vertex>() * VERTICES_PER_CELL, 272);
    assert_eq!(std::mem::size_of::<BoxedQuad>(), 84);
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
            .translated_clipped_vertices(10.0, clip)
            .expect("visible clip");

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
            quad.translated_clipped_vertices(0.0, unrebased).is_none(),
            "an un-rebased clip rect drops everything -- this is the failure \
             mode, not a healthy result"
        );

        // Rebased against the real window, the row survives.
        let list =
            QuadClipRect::from_top_left_pixels(0.0, 204.0, 465.0, 1316.0, &window);
        assert!(quad.translated_clipped_vertices(0.0, list).is_some());
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
        fn quad_at(left: f32, top: f32, right: f32, bottom: f32) -> Box<BoxedQuad> {
            let rect = QuadClipRect::from_top_left_pixels(left, top, right, bottom, &WINDOW);
            Box::new(BoxedQuad {
                position: (rect.left(), rect.top(), rect.right(), rect.bottom()),
                ..Default::default()
            })
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
        let mut sink = TripleLayerQuadAllocator::Heap(&mut out);
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
        assert!(quad.translated_clipped_vertices(-100.0, full).is_none());
        assert!(quad.translated_clipped_vertices(0.0, below).is_none());
    }
}
