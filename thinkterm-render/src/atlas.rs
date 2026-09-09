//! The glyph/sprite atlas: guillotiere packing over a `Texture2d`.

use crate::bitmaps::{BitmapImage, Image, Texture2d, TextureRect};
use crate::geom::{Point, Rect, Size};
use anyhow::{ensure, Result as Fallible};
use guillotiere::{SimpleAtlasAllocator, Size as AtlasSize};
use std::convert::TryInto;
use std::rc::Rc;
use thiserror::*;

const PADDING: i32 = 1;

/// How many rows of the texture are zeroed at a time.
///
/// A side-sized `Image` is `side * side * 4` bytes -- 268 MB at the 8192 a
/// WebGPU device commonly allows, on a heap that never gives memory back --
/// and both `new` and `clear` used to build one. A strip is 2 MB at that
/// size and the uploads are the same total work.
const ZERO_STRIP_ROWS: usize = 64;

/// Blank the whole texture without holding a full-size copy of it.
fn zero(texture: &Rc<dyn Texture2d>, side: usize) {
    let mut rows = ZERO_STRIP_ROWS.min(side);
    let mut strip = Image::new(side, rows);
    let mut y = 0;
    while y < side {
        if side - y < rows {
            rows = side - y;
            strip = Image::new(side, rows);
        }
        texture.write(
            Rect::new(
                Point::new(0, y as isize),
                Size::new(side as isize, rows as isize),
            ),
            &strip,
        );
        y += rows;
    }
}

#[derive(Debug, Error)]
#[error("Texture Size exceeded, need {:?}", size)]
pub struct OutOfTextureSpace {
    pub size: Option<usize>,
    pub current_size: usize,
}

/// Who asked for the sprite. The atlas cannot tell a glyph from a kitty
/// frame, so callers tag their allocations and the usage report splits
/// the packed area by tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AtlasTag {
    Glyph,
    Image,
    Other,
}

impl AtlasTag {
    fn index(self) -> usize {
        match self {
            AtlasTag::Glyph => 0,
            AtlasTag::Image => 1,
            AtlasTag::Other => 2,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AtlasTagUsage {
    pub allocated_px: u64,
    pub allocations: u32,
}

/// Packing accounting since the last `clear()`. `allocated_px` is the
/// padded reservation area and only ever grows: the allocator cannot
/// release a single rectangle, so this is the high-water mark of what
/// was packed, not the live working set. `max_rect` is the largest single
/// reservation (padded, width x height), the number that decides whether
/// a smaller atlas could have held the same content at all.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AtlasUsage {
    pub allocated_px: u64,
    pub allocations: u32,
    pub per_tag: [AtlasTagUsage; 3],
    pub max_rect: (i32, i32),
    pub failures: u32,
}

impl AtlasUsage {
    pub fn tag(&self, tag: AtlasTag) -> AtlasTagUsage {
        self.per_tag[tag.index()]
    }

    fn record_success(&mut self, tag: AtlasTag, width: i32, height: i32) {
        let area = width as u64 * height as u64;
        self.allocated_px += area;
        self.allocations += 1;
        let slot = &mut self.per_tag[tag.index()];
        slot.allocated_px += area;
        slot.allocations += 1;
        // Largest by longest side, not by area: the longest side is what
        // sets the smallest square atlas that could hold the reservation
        // (a 3000x1 strip needs a 4096 atlas; a 100x100 square does not).
        let side = width.max(height);
        let current_side = self.max_rect.0.max(self.max_rect.1);
        let current_area = self.max_rect.0 as u64 * self.max_rect.1 as u64;
        if side > current_side || (side == current_side && area > current_area) {
            self.max_rect = (width, height);
        }
    }
}

/// Atlases are bitmaps of srgba data that are sized as a power of 2.
/// We allocate sprites out of the available space, using AtlasAllocator
/// to manage the available rectangles.
pub struct Atlas {
    texture: Rc<dyn Texture2d>,

    allocator: SimpleAtlasAllocator,

    /// Dimensions of the texture
    side: usize,

    usage: AtlasUsage,
}

impl Atlas {
    pub fn new(texture: &Rc<dyn Texture2d>) -> Fallible<Self> {
        ensure!(
            texture.width() == texture.height(),
            "texture must be square!"
        );
        let side = texture.width();
        // Everything that can fail happens before the texture is touched.
        // `Atlas::new` is called on a texture that is already on screen --
        // that is how the web client clears a full atlas -- and wiping it
        // and then returning `Err` leaves the caller holding a cache whose
        // every sprite points at blank pixels: a terminal that draws
        // nothing, with no way back.
        let allocator =
            SimpleAtlasAllocator::new(AtlasSize::new(side.try_into()?, side.try_into()?));
        zero(texture, side);
        Ok(Self {
            texture: Rc::clone(texture),
            side,
            allocator,
            usage: AtlasUsage::default(),
        })
    }

    #[inline]
    pub fn texture(&self) -> Rc<dyn Texture2d> {
        Rc::clone(&self.texture)
    }

    /// Reserve space for a sprite of the given size
    pub fn allocate(&mut self, im: &dyn BitmapImage) -> Result<Sprite, OutOfTextureSpace> {
        self.allocate_with_padding(im, None, None)
    }

    pub fn allocate_with_padding(
        &mut self,
        im: &dyn BitmapImage,
        padding: Option<usize>,
        scale_down: Option<usize>,
    ) -> Result<Sprite, OutOfTextureSpace> {
        self.allocate_tagged(im, padding, scale_down, AtlasTag::Other)
    }

    /// Reserve space for a sprite, attributing the packed area to `tag`
    /// in the usage report.
    pub fn allocate_tagged(
        &mut self,
        im: &dyn BitmapImage,
        padding: Option<usize>,
        scale_down: Option<usize>,
        tag: AtlasTag,
    ) -> Result<Sprite, OutOfTextureSpace> {
        let (width, height) = im.image_dimensions();

        if let Some(scale_down) = scale_down {
            let mut copied = Image::new(width, height);
            copied.draw_image(Point::new(0, 0), None, im);

            // A source smaller than the divisor must not become 0x0: the
            // sprite's dimensions feed texture-coordinate divisions.
            let scaled = copied.resize((width / scale_down).max(1), (height / scale_down).max(1));

            return self.allocate_tagged(&scaled, padding, None, tag);
        }

        // If we can't convert the sizes to i32, then we'll never
        // be able to store this image
        let reserve_width: i32 = width.try_into().map_err(|_| OutOfTextureSpace {
            size: None,
            current_size: self.side,
        })?;
        let reserve_height: i32 = height.try_into().map_err(|_| OutOfTextureSpace {
            size: None,
            current_size: self.side,
        })?;

        // We pad each sprite reservation with blank space to avoid
        // surprising and unexpected artifacts when the texture is
        // interpolated on to the render surface.
        let reserve_width = reserve_width + padding.unwrap_or(0) as i32 + PADDING * 2;
        let reserve_height = reserve_height + padding.unwrap_or(0) as i32 + PADDING * 2;

        #[cfg(not(target_family = "wasm"))]
        let start = std::time::Instant::now();
        let res = if let Some(allocation) = self
            .allocator
            .allocate(AtlasSize::new(reserve_width, reserve_height))
        {
            let left = allocation.min.x;
            let top = allocation.min.y;
            let rect = Rect::new(
                Point::new((left + PADDING) as isize, (top + PADDING) as isize),
                Size::new(width as isize, height as isize),
            );

            self.texture.write(rect, im);
            self.usage
                .record_success(tag, reserve_width, reserve_height);

            metrics::histogram!("window.atlas.allocate.success.rate").record(1.);
            Ok(Sprite {
                texture: Rc::clone(&self.texture),
                coords: rect,
            })
        } else {
            // It's not possible to satisfy that request
            let size = (reserve_width.max(reserve_height) as usize).next_power_of_two();
            self.usage.failures += 1;
            metrics::histogram!("window.atlas.allocate.failure.rate").record(1.);
            Err(OutOfTextureSpace {
                size: Some((self.side * 2).max(size)),
                current_size: self.side,
            })
        };
        #[cfg(not(target_family = "wasm"))]
        metrics::histogram!("window.atlas.allocate.latency").record(start.elapsed());

        res
    }

    pub fn size(&self) -> usize {
        self.side
    }

    /// Packing accounting since the last `clear()`.
    pub fn usage(&self) -> AtlasUsage {
        self.usage
    }

    /// Zero out the texture, and forget all allocated regions
    pub fn clear(&mut self) {
        zero(&self.texture, self.side);
        self.allocator.clear();
        self.usage = AtlasUsage::default();
    }
}

pub struct Sprite {
    pub texture: Rc<dyn Texture2d>,
    pub coords: Rect,
}

impl std::fmt::Debug for Sprite {
    fn fmt(&self, fmt: &mut std::fmt::Formatter) -> std::result::Result<(), std::fmt::Error> {
        fmt.debug_struct("Sprite")
            .field("coords", &self.coords)
            .field("texture_width", &self.texture.width())
            .field("texture_height", &self.texture.height())
            .finish()
    }
}

impl Clone for Sprite {
    fn clone(&self) -> Self {
        Self {
            texture: Rc::clone(&self.texture),
            coords: self.coords,
        }
    }
}

impl Sprite {
    /// Returns the texture coordinates of the sprite
    pub fn texture_coords(&self) -> TextureRect {
        self.texture.to_texture_coords(self.coords)
    }
}

#[cfg(test)]
mod usage_tests {
    use super::{Atlas, AtlasTag, AtlasUsage, PADDING};
    use crate::bitmaps::{Image, ImageTexture, Texture2d};
    use std::rc::Rc;

    fn atlas(side: usize) -> Atlas {
        let texture: Rc<dyn Texture2d> = Rc::new(ImageTexture::new(side, side));
        Atlas::new(&texture).unwrap()
    }

    fn padded(w: usize, h: usize) -> (i32, i32) {
        (w as i32 + PADDING * 2, h as i32 + PADDING * 2)
    }

    #[test]
    fn accounts_padded_area_per_tag_and_tracks_the_largest_rect() {
        let mut atlas = atlas(64);
        let glyph = Image::new(10, 10);
        let frame = Image::new(20, 5);
        atlas
            .allocate_tagged(&glyph, None, None, AtlasTag::Glyph)
            .unwrap();
        atlas
            .allocate_tagged(&frame, None, None, AtlasTag::Image)
            .unwrap();
        atlas.allocate(&glyph).unwrap();

        let (gw, gh) = padded(10, 10);
        let (fw, fh) = padded(20, 5);
        let glyph_px = (gw * gh) as u64;
        let frame_px = (fw * fh) as u64;

        let usage = atlas.usage();
        assert_eq!(usage.allocations, 3);
        assert_eq!(usage.allocated_px, glyph_px * 2 + frame_px);
        assert_eq!(usage.tag(AtlasTag::Glyph).allocations, 1);
        assert_eq!(usage.tag(AtlasTag::Glyph).allocated_px, glyph_px);
        assert_eq!(usage.tag(AtlasTag::Image).allocated_px, frame_px);
        assert_eq!(usage.tag(AtlasTag::Other).allocated_px, glyph_px);
        assert_eq!(usage.max_rect, (fw, fh));
        assert_eq!(usage.failures, 0);
    }

    #[test]
    fn max_rect_follows_the_longest_side_not_the_area() {
        let mut atlas = atlas(256);
        atlas
            .allocate_tagged(&Image::new(200, 1), None, None, AtlasTag::Image)
            .unwrap();
        atlas
            .allocate_tagged(&Image::new(50, 50), None, None, AtlasTag::Glyph)
            .unwrap();
        // 52x52 has the larger area, but 202x3 is what forces the atlas side.
        assert_eq!(atlas.usage().max_rect, padded(200, 1));
    }

    #[test]
    fn scaled_allocations_account_the_scaled_rect() {
        let mut atlas = atlas(64);
        let big = Image::new(40, 40);
        atlas
            .allocate_tagged(&big, None, Some(2), AtlasTag::Image)
            .unwrap();
        let (w, h) = padded(20, 20);
        assert_eq!(atlas.usage().max_rect, (w, h));
        assert_eq!(
            atlas.usage().tag(AtlasTag::Image).allocated_px,
            (w * h) as u64
        );
    }

    #[test]
    fn failures_count_and_clear_resets_everything() {
        let mut atlas = atlas(32);
        atlas.allocate(&Image::new(8, 8)).unwrap();
        assert!(atlas.allocate(&Image::new(100, 100)).is_err());
        assert_eq!(atlas.usage().failures, 1);
        assert_eq!(atlas.usage().allocations, 1);

        atlas.clear();
        assert_eq!(atlas.usage(), AtlasUsage::default());
    }
}
