//! Image slices share the terminal's CPU pixels and keep only visible textures
//! on the GPU. They are drawn between the text renderer's layers.

use crate::emit::LineParams;
use crate::gpu::Gpu;
use std::collections::HashMap;
use std::ops::Range;
use std::rc::Rc;
use termwiz::image::{ImageData, ImageDataType};
use thinkterm_render::pipeline::GpuTexture;
use thinkterm_render::vertex::{Vertex, IS_BG_IMAGE};

const MAX_TEXTURE_BYTES: usize = 256 * 1024 * 1024;
const MAX_TEXTURES: usize = 64;
const MAX_ATLAS_IMAGE_SIDE: u32 = 254;
const MAX_ATLAS_SIDE: u32 = 1024;
const MAX_FRAGMENTS: usize = 128 * 1024;
const UPLOAD_STRIP_BYTES: usize = 1024 * 1024;
type ImageKey = [u8; 32];

/// One clock mapping per mux connection. Prefer the least-delayed round trip;
/// a push can seed it before the subscription response arrives.
#[derive(Default)]
pub(crate) struct AnimationClock {
    offset: Option<f64>,
    best_rtt: Option<f64>,
}

impl AnimationClock {
    pub fn observe(&mut self, server: u64, received: f64, sent: Option<f64>) {
        if let Some(sent) = sent {
            let rtt = (received - sent).max(0.0);
            if self.best_rtt.is_none_or(|best| rtt < best) {
                self.offset = Some(server as f64 - (sent + received) / 2.0);
                self.best_rtt = Some(rtt);
            }
        } else if self.offset.is_none() {
            self.offset = Some(server as f64 - received);
        }
    }

    pub fn server_now(&self, local: f64) -> u64 {
        (local + self.offset.unwrap_or(0.0)).max(0.0) as u64
    }

    pub fn local_time(&self, server: u64) -> f64 {
        server as f64 - self.offset.unwrap_or(0.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct TextureKey {
    image: ImageKey,
    owner: Option<(usize, u32)>,
    x: u32,
    y: u32,
}

struct Source {
    size: (u32, u32),
    generation: u64,
    seen: bool,
}

fn pixels(data: &ImageDataType, frame: u32) -> Option<(u32, u32, &[u8], ImageKey)> {
    match data {
        ImageDataType::Rgba8 {
            width,
            height,
            data,
            hash,
        } if frame == 0 => Some((*width, *height, data, *hash)),
        ImageDataType::AnimRgba8 {
            width,
            height,
            frames,
            hashes,
            ..
        } => Some((
            *width,
            *height,
            frames.get(frame as usize)?,
            *hashes.get(frame as usize)?,
        )),
        _ => None,
    }
}

/// Bounds use exclusive right/bottom edges. A one-pixel halo lets linear
/// filtering cross tile boundaries without clamping to each tile's edge.
fn tile_bounds(size: (u32, u32), side: u32, x: u32, y: u32) -> ([u32; 4], [u32; 4]) {
    let core = [
        x,
        y,
        x.saturating_add(side).min(size.0),
        y.saturating_add(side).min(size.1),
    ];
    let upload = [
        x.saturating_sub(1),
        y.saturating_sub(1),
        core[2].saturating_add(1).min(size.0),
        core[3].saturating_add(1).min(size.1),
    ];
    (core, upload)
}

struct Texture {
    gpu: Rc<GpuTexture>,
    allocation: Option<guillotiere::Allocation>,
    source_size: (u32, u32),
    size: (u32, u32),
    bytes: usize,
    generation: u64,
    frame: u32,
    content: ImageKey,
    seen: bool,
}

struct ImageAtlas {
    gpu: Rc<GpuTexture>,
    allocator: guillotiere::AtlasAllocator,
    side: u32,
    live: usize,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Slice {
    rect: [f32; 4],
    uv: [f32; 4],
}

pub(crate) struct RelativeViewport {
    pub first: i64,
    pub seqno: u64,
    pub alternate: bool,
    pub origin: (f32, f32),
    pub cell: (f32, f32),
    pub source_cell: (u32, u32),
    pub clip: [f32; 4],
}

#[derive(Clone, Copy)]
pub(crate) struct RelativeSlice(Slice);

pub(crate) fn relative_slice(view: &wezterm_term::kitty_relative::RelativeView, viewport: &RelativeViewport) -> Option<RelativeSlice> {
    if view.source_seqno > viewport.seqno || view.anchor.alt_screen != viewport.alternate
        || viewport.cell.0 <= 0.0 || viewport.cell.1 <= 0.0
        || ![viewport.origin.0, viewport.origin.1, viewport.cell.0, viewport.cell.1].iter()
            .chain(&viewport.clip).all(|n| n.is_finite()) { return None; }
    let layout = view.geometry.layout(view.image_size, viewport.source_cell)?;
    let sx = f64::from(viewport.cell.0) / f64::from(viewport.source_cell.0);
    let sy = f64::from(viewport.cell.1) / f64::from(viewport.source_cell.1);
    let x = f64::from(viewport.origin.0) + view.anchor.column as f64 * f64::from(viewport.cell.0);
    let y = f64::from(viewport.origin.1) + (view.anchor.row as f64 - viewport.first as f64) * f64::from(viewport.cell.1);
    let rect = [x + layout.rect[0] * sx, y + layout.rect[1] * sy, x + layout.rect[2] * sx, y + layout.rect[3] * sy];
    let clip = [rect[0].max(f64::from(viewport.clip[0])), rect[1].max(f64::from(viewport.clip[1])),
        rect[2].min(f64::from(viewport.clip[2])), rect[3].min(f64::from(viewport.clip[3]))];
    if clip[2] <= clip[0] || clip[3] <= clip[1] { return None; }
    let uv = [
        layout.uv[0] + (layout.uv[2] - layout.uv[0]) * (clip[0] - rect[0]) / (rect[2] - rect[0]),
        layout.uv[1] + (layout.uv[3] - layout.uv[1]) * (clip[1] - rect[1]) / (rect[3] - rect[1]),
        layout.uv[0] + (layout.uv[2] - layout.uv[0]) * (clip[2] - rect[0]) / (rect[2] - rect[0]),
        layout.uv[1] + (layout.uv[3] - layout.uv[1]) * (clip[3] - rect[1]) / (rect[3] - rect[1]),
    ];
    Some(RelativeSlice(Slice { rect: clip.map(|v| v as f32), uv: uv.map(|v| v as f32) }))
}

impl Slice {
    fn clipped(self, clip: [f32; 4]) -> Option<Self> {
        if !self
            .rect
            .iter()
            .chain(&self.uv)
            .chain(&clip)
            .all(|n| n.is_finite())
        {
            return None;
        }
        let [x0, y0, x1, y1] = self.rect;
        if x1 <= x0 || y1 <= y0 {
            return None;
        }
        let rect = [
            x0.max(clip[0]),
            y0.max(clip[1]),
            x1.min(clip[2]),
            y1.min(clip[3]),
        ];
        if rect[2] <= rect[0] || rect[3] <= rect[1] {
            return None;
        }
        let [u0, v0, u1, v1] = self.uv;
        Some(Self {
            rect,
            uv: [
                u0 + (u1 - u0) * (rect[0] - x0) / (x1 - x0),
                v0 + (v1 - v0) * (rect[1] - y0) / (y1 - y0),
                u0 + (u1 - u0) * (rect[2] - x0) / (x1 - x0),
                v0 + (v1 - v0) * (rect[3] - y0) / (y1 - y0),
            ],
        })
    }

    fn in_tile(self, size: (u32, u32), core: [u32; 4], upload: [u32; 4]) -> Option<Self> {
        let (w, h) = (size.0 as f32, size.1 as f32);
        let source = Self {
            rect: self.uv,
            uv: self.rect,
        }
        .clipped([
            core[0] as f32 / w,
            core[1] as f32 / h,
            core[2] as f32 / w,
            core[3] as f32 / h,
        ])?;
        let tw = (upload[2] - upload[0]) as f32;
        let th = (upload[3] - upload[1]) as f32;
        Some(Self {
            rect: source.uv,
            uv: [
                (source.rect[0] * w - upload[0] as f32) / tw,
                (source.rect[1] * h - upload[1] as f32) / th,
                (source.rect[2] * w - upload[0] as f32) / tw,
                (source.rect[3] * h - upload[1] as f32) / th,
            ],
        })
    }

    fn joined(self, next: Self, vertical: bool) -> Option<Self> {
        let (along, across) = if vertical { (1, 0) } else { (0, 1) };
        if self.rect[across] != next.rect[across]
            || self.rect[across + 2] != next.rect[across + 2]
            || self.uv[across] != next.uv[across]
            || self.uv[across + 2] != next.uv[across + 2]
            || (self.rect[along + 2] - next.rect[along]).abs() > 0.001
            || (self.uv[along + 2] - next.uv[along]).abs() > 0.000001
        {
            return None;
        }
        // Equal endpoints alone do not imply equal scale: separate cropped
        // placements can touch while sampling at different magnifications.
        let scale =
            (self.uv[along + 2] - self.uv[along]) / (self.rect[along + 2] - self.rect[along]);
        let next_scale =
            (next.uv[along + 2] - next.uv[along]) / (next.rect[along + 2] - next.rect[along]);
        if (scale - next_scale).abs() > 0.000001 {
            return None;
        }
        let mut joined = self;
        joined.rect[along + 2] = next.rect[along + 2];
        joined.uv[along + 2] = next.uv[along + 2];
        Some(joined)
    }
}

#[derive(Clone, Copy)]
struct Fragment {
    key: TextureKey,
    z: i32,
    image_id: u32,
    placement_id: u32,
    slice: Slice,
    hsv: [f32; 3],
}

impl Fragment {
    fn order(&self) -> (i32, u32, u32, TextureKey) {
        (self.z, self.image_id, self.placement_id, self.key)
    }
}

struct Batch {
    key: TextureKey,
    layer: usize,
    vertices: Range<usize>,
}

struct Selection {
    hash: ImageKey,
    animation: wezterm_term::kitty_animation::KittyAnimation,
    sample: Option<wezterm_term::kitty_animation::Sample>,
    virtual_placements: Vec<wezterm_term::kitty_virtual::VirtualPlacement>,
}

#[derive(Default)]
pub struct Graphics {
    textures: HashMap<TextureKey, Texture>,
    atlases: HashMap<usize, ImageAtlas>,
    sources: HashMap<ImageKey, Source>,
    texture_bytes: usize,
    texture_count: usize,
    fragments: Vec<Fragment>,
    vertices: Vec<Vertex>,
    batches: Vec<Batch>,
    uploads: u64,
    upload_scratch: Vec<u8>,
    selections: HashMap<(usize, u32), Selection>,
    animation_now: u64,
}

fn layer(z: i32) -> usize {
    if z < i32::MIN / 2 {
        0
    } else if z < 0 {
        1
    } else {
        2
    }
}

impl Graphics {
    pub fn set_selections(
        &mut self,
        pane: usize,
        selections: &[wezterm_term::KittyFrameSelection],
    ) {
        self.selections.retain(|(owner, _), _| *owner != pane);
        for selection in selections {
            if selection.animation.valid()
                && wezterm_term::kitty_virtual::valid(&selection.virtual_placements)
            {
                self.selections.insert(
                    (pane, selection.image_id),
                    Selection {
                        hash: selection.data_hash,
                        animation: selection.animation.clone(),
                        sample: None,
                        virtual_placements: selection.virtual_placements.clone(),
                    },
                );
            }
        }
        if self.selections.is_empty() {
            self.selections = HashMap::new();
        }
    }

    pub fn next_animation_at(&self) -> Option<u64> {
        self.textures
            .iter()
            .filter(|(_, texture)| texture.seen)
            .filter_map(|(key, _)| {
                let selection = self.selections.get(&key.owner?)?;
                if selection.hash != key.image {
                    return None;
                }
                selection.sample?.next_at_ms
            })
            .min()
    }

    pub fn begin_frame(&mut self, animation_now: u64) {
        self.animation_now = animation_now;
        for selection in self.selections.values_mut() {
            selection.sample = None;
        }
        for source in self.sources.values_mut() {
            source.seen = false;
        }
        for texture in self.textures.values_mut() {
            texture.seen = false;
        }
        self.fragments.clear();
        self.vertices.clear();
        self.batches.clear();
    }

    fn remove(&mut self, gpu: &mut Gpu, key: &TextureKey) {
        if let Some(texture) = self.textures.remove(key) {
            Self::release_texture(
                gpu,
                &mut self.atlases,
                &mut self.texture_bytes,
                &mut self.texture_count,
                &texture,
            );
        }
    }

    fn release_texture(
        gpu: &mut Gpu,
        atlases: &mut HashMap<usize, ImageAtlas>,
        bytes: &mut usize,
        count: &mut usize,
        texture: &Texture,
    ) {
        let id = texture.gpu.id();
        if let Some(allocation) = &texture.allocation {
            let atlas = atlases.get_mut(&id).expect("image atlas exists");
            atlas.allocator.deallocate(allocation.id);
            atlas.live -= 1;
            if atlas.live != 0 {
                return;
            }
            *bytes -= atlas.side as usize * atlas.side as usize * 4;
            atlases.remove(&id);
        } else {
            *bytes -= texture.bytes;
        }
        *count -= 1;
        gpu.forget_texture(id);
    }

    fn allocate_texture(
        &mut self,
        gpu: &mut Gpu,
        width: u32,
        height: u32,
    ) -> anyhow::Result<Option<(Rc<GpuTexture>, Option<guillotiere::Allocation>)>> {
        let packed = width <= MAX_ATLAS_IMAGE_SIDE && height <= MAX_ATLAS_IMAGE_SIDE;
        loop {
            if packed {
                for atlas in self.atlases.values_mut() {
                    if let Some(allocation) = atlas
                        .allocator
                        .allocate(guillotiere::size2(width as i32 + 2, height as i32 + 2))
                    {
                        atlas.live += 1;
                        return Ok(Some((Rc::clone(&atlas.gpu), Some(allocation))));
                    }
                }
            }
            let side = if packed {
                let previous = self
                    .atlases
                    .values()
                    .map(|atlas| atlas.side)
                    .max()
                    .unwrap_or(4);
                previous
                    .saturating_mul(2)
                    .min(MAX_ATLAS_SIDE)
                    .max((width + 2).max(height + 2).next_power_of_two())
            } else {
                0
            };
            let (w, h) = if packed {
                (side, side)
            } else {
                (width, height)
            };
            let bytes = w as usize * h as usize * 4;
            if self.texture_bytes + bytes > MAX_TEXTURE_BYTES || self.texture_count >= MAX_TEXTURES
            {
                let Some(old) = self
                    .textures
                    .iter()
                    .find_map(|(key, texture)| (!texture.seen).then_some(*key))
                else {
                    return Ok(None);
                };
                self.remove(gpu, &old);
                continue;
            }
            let texture = Rc::new(GpuTexture::new(&gpu.device, gpu.queue.clone(), w, h)?);
            self.texture_bytes += bytes;
            self.texture_count += 1;
            let allocation = if packed {
                let mut allocator =
                    guillotiere::AtlasAllocator::new(guillotiere::size2(side as i32, side as i32));
                let allocation = allocator
                    .allocate(guillotiere::size2(width as i32 + 2, height as i32 + 2))
                    .expect("new atlas fits its image");
                self.atlases.insert(
                    texture.id(),
                    ImageAtlas {
                        gpu: Rc::clone(&texture),
                        allocator,
                        side,
                        live: 1,
                    },
                );
                Some(allocation)
            } else {
                None
            };
            return Ok(Some((texture, allocation)));
        }
    }

    fn source_size(&mut self, data: &ImageData) -> Option<(u32, u32)> {
        let key = data.hash();
        let generation = data.generation();
        if let Some(source) = self.sources.get_mut(&key) {
            if source.generation == generation {
                source.seen = true;
                return Some(source.size);
            }
        }
        let payload = data.data();
        let (width, height, pixels, _) = pixels(&payload, 0)?;
        let bytes = (width as usize)
            .checked_mul(height as usize)?
            .checked_mul(4)?;
        if bytes == 0 || bytes > MAX_TEXTURE_BYTES || pixels.len() != bytes {
            return None;
        }
        if !self.sources.contains_key(&key) && self.sources.len() >= MAX_TEXTURES {
            let old = self
                .sources
                .iter()
                .find_map(|(key, source)| (!source.seen).then_some(*key))
                .or_else(|| self.sources.keys().next().copied())?;
            self.sources.remove(&old);
        }
        self.sources.insert(
            key,
            Source {
                size: (width, height),
                generation,
                seen: true,
            },
        );
        Some((width, height))
    }

    fn prepare(
        &mut self,
        gpu: &mut Gpu,
        data: &ImageData,
        key: TextureKey,
        upload: [u32; 4],
        frame: u32,
    ) -> anyhow::Result<bool> {
        let generation = data.generation();
        let (width, height) = (upload[2] - upload[0], upload[3] - upload[1]);
        if let Some(texture) = self.textures.get_mut(&key) {
            if texture.size == (width, height)
                && texture.frame == frame
                && (texture.seen || texture.generation == generation)
            {
                texture.seen = true;
                return Ok(true);
            }
        }
        let payload = data.data();
        let Some((source_width, source_height, pixels, content)) = pixels(&payload, frame) else {
            return Ok(false);
        };
        if upload[2] > source_width || upload[3] > source_height {
            return Ok(false);
        }
        let bytes = width as usize * height as usize * 4;
        if bytes == 0 || bytes > MAX_TEXTURE_BYTES {
            return Ok(false);
        }
        if self
            .textures
            .get(&key)
            .is_some_and(|texture| texture.size != (width, height))
        {
            self.remove(gpu, &key);
        }
        if !self.textures.contains_key(&key) {
            if self.textures.len() >= MAX_FRAGMENTS {
                let Some(old) = self
                    .textures
                    .iter()
                    .find_map(|(key, texture)| (!texture.seen).then_some(*key))
                else {
                    return Ok(false);
                };
                self.remove(gpu, &old);
            }
            let Some((texture, allocation)) = self.allocate_texture(gpu, width, height)? else {
                return Ok(false);
            };
            self.textures.insert(
                key,
                Texture {
                    gpu: texture,
                    allocation,
                    source_size: (source_width, source_height),
                    size: (width, height),
                    bytes,
                    generation,
                    frame,
                    content,
                    seen: false,
                },
            );
            self.upload(gpu, &key, pixels, source_width, upload);
        } else if self.textures[&key].content != content {
            self.upload(gpu, &key, pixels, source_width, upload);
        }
        let texture = self.textures.get_mut(&key).expect("prepared texture");
        texture.source_size = (source_width, source_height);
        texture.generation = generation;
        texture.frame = frame;
        texture.content = content;
        texture.seen = true;
        Ok(true)
    }

    fn upload(
        &mut self,
        gpu: &Gpu,
        key: &TextureKey,
        pixels: &[u8],
        source_width: u32,
        upload: [u32; 4],
    ) {
        let (width, height) = (upload[2] - upload[0], upload[3] - upload[1]);
        let offset = (upload[1] as usize * source_width as usize + upload[0] as usize) * 4;
        if let Some(allocation) = &self.textures[key].allocation {
            // Duplicate the edge pixels into the gutter so linear filtering
            // cannot sample an adjacent image, including after slot reuse.
            let stride = (width as usize + 2) * 4;
            let needed = stride * (height as usize + 2);
            debug_assert!(needed <= UPLOAD_STRIP_BYTES);
            self.upload_scratch
                .reserve_exact(needed.saturating_sub(self.upload_scratch.len()));
            self.upload_scratch.resize(needed, 0);
            for (row, target) in self.upload_scratch.chunks_exact_mut(stride).enumerate() {
                let source_row = row.saturating_sub(1).min(height as usize - 1);
                let start = offset + source_row * source_width as usize * 4;
                let source = &pixels[start..start + width as usize * 4];
                target[..4].copy_from_slice(&source[..4]);
                target[4..stride - 4].copy_from_slice(source);
                target[stride - 4..].copy_from_slice(&source[source.len() - 4..]);
            }
            gpu.queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: self.textures[key].gpu.texture(),
                    mip_level: 0,
                    origin: wgpu::Origin3d {
                        x: allocation.rectangle.min.x as u32,
                        y: allocation.rectangle.min.y as u32,
                        z: 0,
                    },
                    aspect: wgpu::TextureAspect::All,
                },
                &self.upload_scratch,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(stride as u32),
                    rows_per_image: Some(height + 2),
                },
                wgpu::Extent3d {
                    width: width + 2,
                    height: height + 2,
                    depth_or_array_layers: 1,
                },
            );
            self.uploads += 1;
            return;
        }
        let texture = &self.textures[key].gpu;
        let write = |bytes: &[u8], y: u32, rows: u32, stride: u32| {
            debug_assert!(bytes.len() <= UPLOAD_STRIP_BYTES);
            gpu.queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: texture.texture(),
                    mip_level: 0,
                    origin: wgpu::Origin3d { x: 0, y, z: 0 },
                    aspect: wgpu::TextureAspect::All,
                },
                bytes,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(stride),
                    rows_per_image: Some(rows),
                },
                wgpu::Extent3d {
                    width,
                    height: rows,
                    depth_or_array_layers: 1,
                },
            );
        };
        let row_bytes = width as usize * 4;
        let source_stride = source_width as usize * 4;
        let strip_rows = (UPLOAD_STRIP_BYTES / row_bytes).max(1);
        if width == source_width || height == 1 {
            for y in (0..height as usize).step_by(strip_rows) {
                let rows = strip_rows.min(height as usize - y);
                let start = offset + y * source_stride;
                let len = (rows - 1) * source_stride + row_bytes;
                write(
                    &pixels[start..start + len],
                    y as u32,
                    rows as u32,
                    source_width * 4,
                );
            }
        } else {
            // wgpu's Web backend copies the entire supplied slice into JS.
            // Pack narrow tiles in bounded strips instead of copying all the
            // unused source columns (or keeping a second full image).
            for y in (0..height as usize).step_by(strip_rows) {
                let rows = strip_rows.min(height as usize - y);
                let needed = rows * row_bytes;
                self.upload_scratch
                    .reserve_exact(needed.saturating_sub(self.upload_scratch.len()));
                self.upload_scratch.resize(needed, 0);
                for (row, target) in self.upload_scratch.chunks_exact_mut(row_bytes).enumerate() {
                    let start = offset + (y + row) * source_stride;
                    target.copy_from_slice(&pixels[start..start + row_bytes]);
                }
                write(&self.upload_scratch, y as u32, rows as u32, width * 4);
            }
        }
        self.uploads += 1;
    }

    /// `clip` is the stationary pane content box; the row's origin already
    /// includes fractional scrolling. Padding scales with the server's cells.
    pub fn collect_line(
        &mut self,
        gpu: &mut Gpu,
        pane: usize,
        p: &LineParams<'_>,
        cell: (f32, f32),
        padding_scale: (f32, f32),
        clip: [f32; 4],
    ) -> anyhow::Result<()> {
        if p.line.is_double_height_bottom() || !p.line.has_images() {
            return Ok(());
        }
        let cw = cell.0 * if p.line.is_single_width() { 1.0 } else { 2.0 };
        let ch = cell.1
            * if p.line.is_double_height_top() {
                2.0
            } else {
                1.0
            };
        let y = p.origin.1 + p.top_pixel_y;
        let mut placeholders = crate::placeholders::Decoder::default();
        for cell in p.line.visible_cells() {
            let placeholder = placeholders.push(
                cell.cell_index(),
                cell.str(),
                cell.attrs().foreground(),
                cell.attrs().underline_color(),
            );
            let x = p.origin.0 + cell.cell_index() as f32 * cw;
            if x >= clip[2] {
                break;
            }
            for image in cell.attrs().image_attachments() {
                if self.fragments.len() >= MAX_FRAGMENTS {
                    return Ok(());
                }
                let (left, top, right, bottom) = image.padding();
                let tl = image.top_left();
                let br = image.bottom_right();
                let mut slice = Slice {
                    rect: [
                        x + left as f32 * padding_scale.0,
                        y + top as f32 * padding_scale.1,
                        x + cw - right as f32 * padding_scale.0,
                        y + ch - bottom as f32 * padding_scale.1,
                    ],
                    uv: [
                        tl.x.into_inner(),
                        tl.y.into_inner(),
                        br.x.into_inner(),
                        br.y.into_inner(),
                    ],
                };
                if let Some(placeholder) =
                    placeholder.filter(|p| Some(p.image_id) == image.image_id())
                {
                    let placement = self
                        .selections
                        .get(&(pane, placeholder.image_id))
                        .filter(|s| s.hash == image.image_data().hash())
                        .and_then(|s| {
                            wezterm_term::kitty_virtual::find(
                                &s.virtual_placements,
                                placeholder.placement_id,
                            )
                        });
                    if let Some(placement) =
                        placement.filter(|p| Some(p.placement_id) == image.placement_id())
                    {
                        let Some(size) = self.source_size(image.image_data()) else {
                            continue;
                        };
                        let Some(part) = crate::placeholders::fit_cell(
                            size,
                            (placement.columns, placement.rows),
                            (cw, ch),
                            placeholder.row,
                            placeholder.column,
                        ) else {
                            continue;
                        };
                        slice = Slice {
                            rect: [
                                x + part.rect[0],
                                y + part.rect[1],
                                x + part.rect[2],
                                y + part.rect[3],
                            ],
                            uv: part.uv,
                        };
                    }
                }
                let Some(slice) = slice.clipped(clip) else {
                    continue;
                };
                let hsv = p.hsv.unwrap_or_default();
                self.collect_slice(gpu, image.image_data(), image.image_id(), image.placement_id().unwrap_or(0),
                    image.z_index(), slice, [hsv.hue, hsv.saturation, hsv.brightness], pane)?;
            }
        }
        Ok(())
    }

    fn collect_slice(
        &mut self, gpu: &mut Gpu, image: &std::sync::Arc<ImageData>, image_id: Option<u32>,
        placement_id: u32, z: i32, slice: Slice, hsv: [f32; 3], pane: usize,
    ) -> anyhow::Result<()> {
        if self.fragments.len() >= MAX_FRAGMENTS { return Ok(()); }
        let selected = image_id.and_then(|id| {
            self.selections
                .get_mut(&(pane, id))
                .filter(|selection| selection.hash == image.hash())
                .map(|selection| {
                    let sample = selection.sample.get_or_insert_with(|| {
                        selection.animation.sample(self.animation_now)
                    });
                    (id, sample.frame)
                })
        });
        let frame = selected.map_or(0, |(_, frame)| frame);

        let key = TextureKey {
            image: image.hash(),
            owner: selected.map(|(id, _)| (pane, id)),
            x: 0,
            y: 0,
        };
        let fragment = Fragment {
            key,
            slice,
            z,
            image_id: image_id.unwrap_or(0),
            placement_id,
            hsv,
        };
        if let Some(texture) = self.textures.get_mut(&key) {
            if texture.size == texture.source_size
                && texture.frame == frame
                && (texture.seen || texture.generation == image.generation())
            {
                texture.seen = true;
                self.fragments.push(fragment);
                return Ok(());
            }
        }
        let Some(size) = self.source_size(image) else {
            return Ok(());
        };
        let limit = gpu
            .max_texture_dimension()
            .min((UPLOAD_STRIP_BYTES / 4) as u32);
        if size.0 <= limit && size.1 <= limit {
            if self.prepare(gpu, image, key, [0, 0, size.0, size.1], frame)? {
                self.fragments.push(fragment);
            }
            return Ok(());
        }
        let side = limit
            .saturating_sub(2)
            .min((UPLOAD_STRIP_BYTES / 4 - 2) as u32)
            .max(1);
        let x0 = (slice.uv[0].max(0.0) * size.0 as f32).floor() as u32 / side * side;
        let y0 = (slice.uv[1].max(0.0) * size.1 as f32).floor() as u32 / side * side;
        let x1 = ((slice.uv[2].min(1.0) * size.0 as f32).ceil() as u32).min(size.0);
        let y1 = ((slice.uv[3].min(1.0) * size.1 as f32).ceil() as u32).min(size.1);
        for y in (y0..y1).step_by(side as usize) {
            for x in (x0..x1).step_by(side as usize) {
                if self.fragments.len() >= MAX_FRAGMENTS {
                    return Ok(());
                }
                let (core, upload) = tile_bounds(size, side, x, y);
                let Some(slice) = slice.in_tile(size, core, upload) else {
                    continue;
                };
                let key = TextureKey {
                    image: image.hash(),
                    owner: key.owner,
                    x,
                    y,
                };
                if !self.prepare(gpu, image, key, upload, frame)? {
                    continue;
                }
                self.fragments.push(Fragment {
                    key,
                    slice,
                    ..fragment
                });
            }
        }
        Ok(())
    }

    pub(crate) fn collect_relative(
        &mut self, gpu: &mut Gpu, pane: usize, image: &std::sync::Arc<ImageData>, image_id: u32,
        view: &wezterm_term::kitty_relative::RelativeView, slice: RelativeSlice, hsv: [f32; 3],
    ) -> anyhow::Result<()> {
        if self.source_size(image) != Some(view.image_size) { return Ok(()); }
        let placement_id = u32::try_from(view.placement_id).unwrap_or(0);
        self.collect_slice(gpu, image, Some(image_id), placement_id, view.geometry.z_index, slice.0, hsv, pane)
    }

    pub fn finish_frame(&mut self, gpu: &mut Gpu, surface: (f32, f32)) {
        self.sources.retain(|_, source| source.seen);
        self.textures.retain(|_, texture| {
            if !texture.seen {
                Self::release_texture(
                    gpu,
                    &mut self.atlases,
                    &mut self.texture_bytes,
                    &mut self.texture_count,
                    texture,
                );
            }
            texture.seen
        });
        if !self
            .textures
            .values()
            .any(|texture| texture.source_size != texture.size || texture.allocation.is_some())
        {
            self.upload_scratch = Vec::new();
        }
        self.fragments.sort_unstable_by(|a, b| {
            a.order()
                .cmp(&b.order())
                .then_with(|| a.slice.rect[1].total_cmp(&b.slice.rect[1]))
                .then_with(|| a.slice.rect[0].total_cmp(&b.slice.rect[0]))
        });
        // Turn cell slices into rows, then rectangles. A full-screen image
        // normally needs one quad, not four vertices for every terminal cell.
        for vertical in [false, true] {
            let mut kept = 0;
            for read in 0..self.fragments.len() {
                let next = self.fragments[read];
                if kept > 0 {
                    let last = &mut self.fragments[kept - 1];
                    if last.order() == next.order() && last.hsv == next.hsv {
                        if let Some(joined) = last.slice.joined(next.slice, vertical) {
                            last.slice = joined;
                            continue;
                        }
                    }
                }
                self.fragments[kept] = next;
                kept += 1;
            }
            self.fragments.truncate(kept);
        }
        for fragment in &self.fragments {
            let start = self.vertices.len();
            let [x0, y0, x1, y1] = fragment.slice.rect;
            let [mut u0, mut v0, mut u1, mut v1] = fragment.slice.uv;
            let texture = &self.textures[&fragment.key];
            if let Some(allocation) = &texture.allocation {
                let side = self.atlases[&texture.gpu.id()].side as f32;
                let x = (allocation.rectangle.min.x + 1) as f32;
                let y = (allocation.rectangle.min.y + 1) as f32;
                u0 = (x + u0 * texture.size.0 as f32) / side;
                u1 = (x + u1 * texture.size.0 as f32) / side;
                v0 = (y + v0 * texture.size.1 as f32) / side;
                v1 = (y + v1 * texture.size.1 as f32) / side;
            }
            for (x, y, u, v) in [
                (x0, y0, u0, v0),
                (x1, y0, u1, v0),
                (x0, y1, u0, v1),
                (x1, y1, u1, v1),
            ] {
                self.vertices.push(Vertex {
                    position: [x - surface.0 / 2.0, y - surface.1 / 2.0],
                    tex: [u, v],
                    fg_color: [1.0; 4],
                    hsv: fragment.hsv,
                    has_color: IS_BG_IMAGE,
                    ..Default::default()
                });
            }
            let layer = layer(fragment.z);
            if let Some(last) = self
                .batches
                .last_mut()
                .filter(|b| self.textures[&b.key].gpu.id() == texture.gpu.id() && b.layer == layer)
            {
                last.vertices.end = self.vertices.len();
            } else {
                self.batches.push(Batch {
                    key: fragment.key,
                    layer,
                    vertices: start..self.vertices.len(),
                });
            }
        }
        if self.fragments.is_empty() {
            // A clear or tab switch releases the high-water geometry as well
            // as the textures; a transient large image leaves no idle cache.
            self.sources = HashMap::new();
            self.fragments = Vec::new();
            self.vertices = Vec::new();
            self.batches = Vec::new();
            if self.textures.is_empty() {
                self.textures = HashMap::new();
                self.atlases = HashMap::new();
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.batches.is_empty()
    }

    pub fn clear_images(&mut self, gpu: &mut Gpu) {
        let selections = std::mem::take(&mut self.selections);
        self.clear(gpu);
        self.selections = selections;
    }

    pub fn clear(&mut self, gpu: &mut Gpu) {
        for atlas in self.atlases.values() {
            gpu.forget_texture(atlas.gpu.id());
        }
        for texture in self
            .textures
            .values()
            .filter(|texture| texture.allocation.is_none())
        {
            gpu.forget_texture(texture.gpu.id());
        }
        *self = Self::default();
    }

    #[cfg(target_arch = "wasm32")]
    pub(crate) fn stats(&self) -> (usize, usize, u64) {
        (self.texture_count, self.texture_bytes, self.uploads)
    }

    #[cfg(target_arch = "wasm32")]
    pub(crate) fn staging_bytes(&self) -> usize {
        self.upload_scratch.capacity()
    }

    pub fn append_batches<'a>(
        &'a self,
        layer: usize,
        out: &mut Vec<(&'a [Vertex], &'a GpuTexture)>,
    ) {
        for batch in &self.batches {
            if batch.layer == layer {
                out.push((
                    &self.vertices[batch.vertices.clone()],
                    &self.textures[&batch.key].gpu,
                ));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_crops_follow_fractional_viewports_and_wait_for_terminal_updates() {
        use wezterm_term::kitty_relative::{Anchor, PlacementGeometry, RelativeView};
        let mut view = RelativeView { placement_id: 1, source_seqno: 9,
            anchor: Anchor { column: 2, row: 99, alt_screen: false },
            geometry: PlacementGeometry { x: 20, y: 10, width: 40, height: 20, columns: 4, rows: 2, ..Default::default() },
            image_size: (80, 40) };
        let mut viewport = RelativeViewport { first: 100, seqno: 8, alternate: false,
            origin: (10.0, 3.5), cell: (10.0, 20.0), source_cell: (8, 16), clip: [35.0, 0.0, 60.0, 20.0] };
        assert!(relative_slice(&view, &viewport).is_none());
        viewport.seqno = 9;
        let slice = relative_slice(&view, &viewport).unwrap().0;
        assert_eq!(slice.rect, [35.0, 0.0, 60.0, 20.0]);
        let expected = [0.3125, 0.45625, 0.625, 0.70625];
        for (actual, expected) in slice.uv.into_iter().zip(expected) { assert!((actual - expected).abs() < 0.00001); }
        viewport.alternate = true;
        assert!(relative_slice(&view, &viewport).is_none());
        viewport.alternate = false;
        view.anchor.row = i64::MAX;
        assert!(relative_slice(&view, &viewport).is_none());
        view.anchor.row = i64::MIN;
        assert!(relative_slice(&view, &viewport).is_none());
        view.anchor.row = 99;
        viewport.cell.0 = f32::NAN;
        assert!(relative_slice(&view, &viewport).is_none());
    }

    #[test]
    fn tiled_slices_keep_geometry_and_include_filtering_neighbours() {
        let size = (12, 4);
        let whole = Slice {
            rect: [20.0, 10.0, 44.0, 18.0],
            uv: [0.0, 0.0, 1.0, 1.0],
        };
        let (core, upload) = tile_bounds(size, 6, 0, 0);
        assert_eq!(upload, [0, 0, 7, 4]);
        assert_eq!(
            whole.in_tile(size, core, upload),
            Some(Slice {
                rect: [20.0, 10.0, 32.0, 18.0],
                uv: [0.0, 0.0, 6.0 / 7.0, 1.0],
            })
        );
        let (core, upload) = tile_bounds(size, 6, 6, 0);
        assert_eq!(upload, [5, 0, 12, 4]);
        assert_eq!(
            whole.in_tile(size, core, upload),
            Some(Slice {
                rect: [32.0, 10.0, 44.0, 18.0],
                uv: [1.0 / 7.0, 0.0, 1.0, 1.0],
            })
        );
        let outside = Slice {
            uv: [0.0, 0.0, 0.25, 1.0],
            ..whole
        };
        assert_eq!(outside.in_tile(size, core, upload), None);
        let (core, upload) = tile_bounds((20, 20), 6, 6, 6);
        assert_eq!(core, [6, 6, 12, 12]);
        assert_eq!(upload, [5, 5, 13, 13]);
    }

    #[test]
    fn fractional_scroll_and_split_clip_interpolate_source_crop() {
        let slice = Slice {
            rect: [20.0, -2.5, 40.0, 17.5],
            uv: [0.25, 0.5, 0.75, 1.0],
        };
        assert_eq!(
            slice.clipped([25.0, 0.0, 35.0, 10.0]),
            Some(Slice {
                rect: [25.0, 0.0, 35.0, 10.0],
                uv: [0.375, 0.5625, 0.625, 0.8125],
            })
        );
        assert_eq!(slice.clipped([40.0, 0.0, 50.0, 20.0]), None);
        assert_eq!(
            Slice {
                rect: [0.0; 4],
                ..slice
            }
            .clipped([0.0, 0.0, 1.0, 1.0]),
            None
        );
        assert_eq!(
            Slice {
                uv: [f32::NAN; 4],
                ..slice
            }
            .clipped([0.0, 0.0, 50.0, 50.0]),
            None
        );
    }

    #[test]
    fn clock_mapping_uses_fastest_response_and_ignores_delayed_pushes() {
        let mut clock = super::AnimationClock::default();
        clock.observe(1000, 500.0, None);
        assert_eq!(clock.server_now(510.0), 1010);
        clock.observe(1020, 530.0, Some(510.0));
        assert_eq!(clock.server_now(540.0), 1040);
        assert_eq!(clock.local_time(1100), 600.0);
        clock.observe(2000, 800.0, Some(600.0));
        clock.observe(9000, 800.0, None);
        assert_eq!(clock.server_now(800.0), 1300);
        clock.observe(1400, 901.0, Some(899.0));
        assert_eq!(clock.server_now(910.0), 1410);
    }

    #[test]
    fn protocol_layers_keep_the_boundary_below_text() {
        assert_eq!(layer(i32::MIN), 0);
        assert_eq!(layer(i32::MIN / 2 - 1), 0);
        assert_eq!(layer(i32::MIN / 2), 1);
        assert_eq!(layer(-1), 1);
        assert_eq!(layer(0), 2);
        assert_eq!(layer(i32::MAX), 2);
    }

    #[test]
    fn joining_slices_preserves_padding_and_scale_boundaries() {
        let left = Slice {
            rect: [0.0, 0.0, 10.0, 20.0],
            uv: [0.0, 0.0, 0.5, 1.0],
        };
        let right = Slice {
            rect: [10.0, 0.0, 20.0, 20.0],
            uv: [0.5, 0.0, 1.0, 1.0],
        };
        assert_eq!(
            left.joined(right, false),
            Some(Slice {
                rect: [0.0, 0.0, 20.0, 20.0],
                uv: [0.0, 0.0, 1.0, 1.0],
            })
        );
        assert_eq!(
            left.joined(
                Slice {
                    rect: [11.0, 0.0, 20.0, 20.0],
                    ..right
                },
                false
            ),
            None
        );
        assert_eq!(
            left.joined(
                Slice {
                    rect: [10.0, 0.0, 30.0, 20.0],
                    ..right
                },
                false
            ),
            None
        );
        assert_eq!(left.joined(right, true), None);
    }
}
