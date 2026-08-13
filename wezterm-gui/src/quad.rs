// Clippy hates the implement_vertex macro and won't let me scope
// this warning to its use
#![allow(clippy::unneeded_field_pattern)]

use crate::renderstate::BorrowedLayers;
use ::window::bitmaps::TextureRect;
use ::window::color::LinearRgba;
use ::window::RectF;
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
}

impl<'a> QuadTrait for QuadImpl<'a> {
    fn set_texture_discrete(&mut self, x1: f32, x2: f32, y1: f32, y2: f32) {
        match self {
            Self::Vert(q) => q.set_texture_discrete(x1, x2, y1, y2),
            Self::Boxed(q) => q.set_texture_discrete(x1, x2, y1, y2),
        }
    }

    fn set_has_color_impl(&mut self, has_color: f32) {
        match self {
            Self::Vert(q) => q.set_has_color_impl(has_color),
            Self::Boxed(q) => q.set_has_color_impl(has_color),
        }
    }

    fn set_fg_color(&mut self, color: LinearRgba) {
        match self {
            Self::Vert(q) => q.set_fg_color(color),
            Self::Boxed(q) => q.set_fg_color(color),
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
            Self::Boxed(q) => q.set_corner_gradient(top_left, top_right, bottom_left, bottom_right),
        }
    }

    fn set_vertical_gradient(&mut self, top: LinearRgba, bottom: LinearRgba) {
        match self {
            Self::Vert(q) => q.set_vertical_gradient(top, bottom),
            Self::Boxed(q) => q.set_vertical_gradient(top, bottom),
        }
    }

    fn set_alt_color_and_mix_value(&mut self, color: LinearRgba, mix_value: f32) {
        match self {
            Self::Vert(q) => q.set_alt_color_and_mix_value(color, mix_value),
            Self::Boxed(q) => q.set_alt_color_and_mix_value(color, mix_value),
        }
    }

    fn set_hsv(&mut self, hsv: Option<HsbTransform>) {
        match self {
            Self::Vert(q) => q.set_hsv(hsv),
            Self::Boxed(q) => q.set_hsv(hsv),
        }
    }

    fn set_position(&mut self, left: f32, top: f32, right: f32, bottom: f32) {
        match self {
            Self::Vert(q) => q.set_position(left, top, right, bottom),
            Self::Boxed(q) => q.set_position(left, top, right, bottom),
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

    fn clipped_to(&self, clip: (f32, f32, f32, f32)) -> Option<Self> {
        let (left, top, right, bottom) = self.position;
        let (clip_left, clip_top, clip_right, clip_bottom) = clip;
        if !(right > left && bottom > top && clip_right > clip_left && clip_bottom > clip_top) {
            return None;
        }

        let visible_left = left.max(clip_left);
        let visible_top = top.max(clip_top);
        let visible_right = right.min(clip_right);
        let visible_bottom = bottom.min(clip_bottom);
        if visible_right <= visible_left || visible_bottom <= visible_top {
            return None;
        }
        if visible_left == left
            && visible_top == top
            && visible_right == right
            && visible_bottom == bottom
        {
            return Some(self.clone());
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

        Some(Self {
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
        })
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

impl HeapQuadAllocator {
    pub fn apply_to(&self, other: &mut TripleLayerQuadAllocator) -> anyhow::Result<()> {
        let start = std::time::Instant::now();
        for (layer_num, quads) in [(0, &self.layer0), (1, &self.layer1), (2, &self.layer2)] {
            for quad in quads {
                other.extend_with(layer_num, &quad.to_vertices());
            }
        }
        metrics::histogram!("quad_buffer_apply").record(start.elapsed());
        Ok(())
    }

    /// Copy these heap-backed quads into `other`, hard-clipped to a surface
    /// pixel rectangle. Position, texture coordinates and per-corner colors are
    /// cropped together, so glyphs and gradients retain their original shape.
    pub fn apply_to_clipped(
        &self,
        other: &mut TripleLayerQuadAllocator,
        clip: RectF,
        surface_width: f32,
        surface_height: f32,
    ) -> anyhow::Result<()> {
        if clip.size.width <= 0.0 || clip.size.height <= 0.0 {
            return Ok(());
        }
        let left_offset = surface_width / 2.0;
        let top_offset = surface_height / 2.0;
        let centered_clip = (
            clip.min_x() - left_offset,
            clip.min_y() - top_offset,
            clip.max_x() - left_offset,
            clip.max_y() - top_offset,
        );
        let start = std::time::Instant::now();
        for (layer_num, quads) in [(0, &self.layer0), (1, &self.layer1), (2, &self.layer2)] {
            for quad in quads {
                if let Some(clipped) = quad.clipped_to(centered_clip) {
                    other.extend_with(layer_num, &clipped.to_vertices());
                }
            }
        }
        metrics::histogram!("quad_buffer_apply_clipped").record(start.elapsed());
        Ok(())
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
}

impl<'a> TripleLayerQuadAllocatorTrait for TripleLayerQuadAllocator<'a> {
    fn allocate(&mut self, layer_num: usize) -> anyhow::Result<QuadImpl<'_>> {
        match self {
            Self::Gpu(b) => b.allocate(layer_num),
            Self::Heap(h) => h.allocate(layer_num),
        }
    }

    fn extend_with(&mut self, layer_num: usize, vertices: &[Vertex]) {
        match self {
            Self::Gpu(b) => b.extend_with(layer_num, vertices),
            Self::Heap(h) => h.extend_with(layer_num, vertices),
        }
    }
}

#[cfg(test)]
#[test]
fn size() {
    assert_eq!(std::mem::size_of::<Vertex>() * VERTICES_PER_CELL, 272);
    assert_eq!(std::mem::size_of::<BoxedQuad>(), 96);
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
fn boxed_quad_clip_crops_position_texture_and_gradient_together() {
    let mut quad = BoxedQuad::default();
    quad.set_position(0.0, 0.0, 100.0, 80.0);
    quad.set_texture_discrete(0.1, 0.9, 0.2, 0.6);
    quad.set_corner_gradient(
        LinearRgba::with_components(0.0, 0.0, 0.0, 1.0),
        LinearRgba::with_components(1.0, 0.0, 0.0, 1.0),
        LinearRgba::with_components(0.0, 1.0, 0.0, 1.0),
        LinearRgba::with_components(1.0, 1.0, 0.0, 1.0),
    );

    let clipped = quad.clipped_to((25.0, 20.0, 75.0, 60.0)).unwrap();
    assert_eq!(clipped.position, (25.0, 20.0, 75.0, 60.0));
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
fn boxed_quad_clip_rejects_outside_and_preserves_inside() {
    let mut quad = BoxedQuad::default();
    quad.set_position(10.0, 20.0, 30.0, 40.0);
    quad.set_texture_discrete(0.2, 0.6, 0.3, 0.7);
    assert!(quad.clipped_to((30.0, 0.0, 50.0, 50.0)).is_none());
    let inside = quad.clipped_to((0.0, 0.0, 50.0, 50.0)).unwrap();
    assert_eq!(inside.position, quad.position);
    assert_eq!(inside.tex, quad.tex);
}
