//! Drawing the glyphs the bundled faces cannot, on any platform.
//!
//! The page ships two font files and does its own shaping, so it has no
//! system fallback chain: CJK, Hangul, emoji and a scattering of symbols
//! have no glyph at all. The platform, however, has every font the machine
//! has installed. So the missing graphemes are painted by it -- a 2D canvas
//! in the browser, CoreText on iOS, `android.graphics` on Android -- read
//! back as pixels and put into the same atlas as everything else.
//!
//! Only the painting primitive is platform code ([`TextPainter`]). The
//! procedure around it -- paint twice in fills that differ in every
//! channel to learn whether the glyph carries its own colour, find the
//! ink, crop it, decide the bearing -- is here, and the arithmetic it
//! leans on is in `fallback.rs`, where it is tested.

use crate::fallback::{self, Ink, Placement};
use anyhow::Result;
use std::rc::Rc;
use thinkterm_render::bitmaps::Image;
use thinkterm_render::geom::Size;

/// The widest glyph we will draw. Wider than any terminal cluster we expect
/// and cheap to be generous about: the scratch is a few hundred kilobytes
/// at the largest cell size.
pub const MAX_FALLBACK_CELLS: u8 = 4;

/// Two fills that differ in **every** channel.
///
/// A glyph that takes our colour looks different under the two; one that
/// carries its own looks the same. Sharing a channel between them -- white
/// and red both have full red -- is what made the first measurements call
/// every Chinese character a colour glyph.
pub const FILL_A: [u8; 3] = [0xff, 0x00, 0x00];
pub const FILL_B: [u8; 3] = [0x00, 0xff, 0xff];

/// The scratch surface a painter draws into: its size in pixels, where
/// the pen starts and where the baseline sits, and the font size.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScratchGeometry {
    pub width: usize,
    pub height: usize,
    pub pen_x: f64,
    pub baseline: f64,
    /// Font size in pixels, already rounded.
    pub px: f64,
}

impl ScratchGeometry {
    /// Room for `MAX_FALLBACK_CELLS` cells with a cell of margin on either
    /// side and half a row above and below, so ink that overhangs is kept
    /// rather than cut.
    pub fn for_cell(cell: Size, descender: f64, px: f64) -> Self {
        let cell_w = cell.width.max(1) as usize;
        let cell_h = cell.height.max(1) as usize;
        let row_top = cell_h as f64 / 2.0;
        Self {
            width: (MAX_FALLBACK_CELLS as usize + 2) * cell_w,
            height: 2 * cell_h,
            pen_x: cell_w as f64,
            baseline: row_top + cell_h as f64 + descender,
            px: px.max(1.0).round(),
        }
    }
}

/// What the platform must do: paint `text` in one solid colour at the
/// geometry it was built for and hand back the whole scratch as RGBA,
/// `width * height * 4` bytes, non-premultiplied, transparent where nothing
/// was painted.
pub trait TextPainter {
    fn paint(&self, text: &str, fill: [u8; 3]) -> Result<Vec<u8>>;
}

/// What a glyph cache needs from its platform: a clock that does not step
/// (for the per-frame budget and the retry backoff) and painters.
pub trait GlyphPlatform {
    /// Milliseconds from a monotonic clock.
    fn now_ms(&self) -> f64;
    /// A painter for one scratch geometry, drawing with `families` (a CSS
    /// font stack on the web; other platforms may ignore it).
    fn painter(&self, geometry: &ScratchGeometry, families: &str) -> Result<Box<dyn TextPainter>>;
}

pub type Platform = Rc<dyn GlyphPlatform>;

/// A glyph the platform drew for us.
pub struct Drawn {
    /// The ink, cropped, in the cell's coordinate space.
    pub image: Image,
    pub placement: Placement,
    /// Whether it carries its own colour (an emoji) or takes the cell's.
    pub has_color: bool,
    /// The ink touched the edge of the scratch: it was cut, and the caller
    /// should not trust its width.
    pub clipped: bool,
}

/// The scratch and the procedure; the painter is the only platform part.
pub struct Scratch {
    painter: Box<dyn TextPainter>,
    geometry: ScratchGeometry,
    cell_width: f64,
}

impl Scratch {
    pub fn new(
        platform: &dyn GlyphPlatform,
        cell: Size,
        descender: f64,
        px: f64,
        families: &str,
    ) -> Result<Self> {
        let geometry = ScratchGeometry::for_cell(cell, descender, px);
        let painter = platform.painter(&geometry, families)?;
        Ok(Self {
            painter,
            geometry,
            cell_width: cell.width.max(1) as f64,
        })
    }

    fn render(&self, text: &str) -> Result<Option<(Ink, Vec<u8>, bool)>> {
        let (width, height) = (self.geometry.width, self.geometry.height);
        let first = self.painter.paint(text, FILL_A)?;
        anyhow::ensure!(
            first.len() == width * height * 4,
            "the painter returned {} bytes for a {width}x{height} scratch",
            first.len()
        );
        let Some(ink) = fallback::ink_box(&first, width, height) else {
            return Ok(None);
        };
        let cropped = fallback::crop(&first, width, &ink);
        let second_full = self.painter.paint(text, FILL_B)?;
        anyhow::ensure!(
            second_full.len() == width * height * 4,
            "the painter returned {} bytes for a {width}x{height} scratch",
            second_full.len()
        );
        let second = fallback::crop(&second_full, width, &ink);
        let has_color = fallback::carries_own_colour(&cropped, &second);
        Ok(Some((ink, cropped, has_color)))
    }

    fn max_width(&self, cells: u8) -> f64 {
        self.cell_width * (cells.max(1) as f64 + 0.25)
    }

    fn limit(&self, cells: u8, has_color: bool) -> f64 {
        if has_color {
            f64::INFINITY
        } else {
            self.max_width(cells)
        }
    }

    pub fn glyph(&self, text: &str, cells: u8) -> Result<Option<Drawn>> {
        let Some((ink, cropped, has_color)) = self.render(text)? else {
            return Ok(None);
        };
        Ok(Some(Drawn {
            image: Image::from_raw(ink.width, ink.height, cropped),
            placement: fallback::place(
                &ink,
                self.geometry.pen_x,
                self.geometry.baseline,
                self.limit(cells, has_color),
            ),
            has_color,
            clipped: ink.clipped,
        }))
    }

    pub fn measure(&self, text: &str, cells: u8) -> Result<Option<(Ink, Placement, bool)>> {
        let Some((ink, _, has_color)) = self.render(text)? else {
            return Ok(None);
        };
        let placement = fallback::place(
            &ink,
            self.geometry.pen_x,
            self.geometry.baseline,
            self.limit(cells, has_color),
        );
        Ok(Some((ink, placement, has_color)))
    }

    pub fn geometry(&self) -> ScratchGeometry {
        self.geometry
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use thinkterm_render::bitmaps::BitmapImage;

    /// A painter that draws a filled box for any text, in the fill it is
    /// given, so the colour test sees a glyph that takes our colour.
    struct BoxPainter(ScratchGeometry);

    impl TextPainter for BoxPainter {
        fn paint(&self, text: &str, fill: [u8; 3]) -> Result<Vec<u8>> {
            let g = self.0;
            let mut out = vec![0u8; g.width * g.height * 4];
            if text.is_empty() {
                return Ok(out);
            }
            let (x0, y0, w, h) = (g.pen_x as usize, g.baseline as usize - 10, 8usize, 10usize);
            for y in y0..y0 + h {
                for x in x0..x0 + w {
                    let i = (y * g.width + x) * 4;
                    out[i..i + 3].copy_from_slice(&fill);
                    out[i + 3] = 255;
                }
            }
            Ok(out)
        }
    }

    struct Fake;
    impl GlyphPlatform for Fake {
        fn now_ms(&self) -> f64 {
            0.0
        }
        fn painter(&self, geometry: &ScratchGeometry, _: &str) -> Result<Box<dyn TextPainter>> {
            Ok(Box::new(BoxPainter(*geometry)))
        }
    }

    #[test]
    fn a_box_is_a_monochrome_glyph_at_the_pen() {
        let scratch = Scratch::new(&Fake, Size::new(9, 20), -4.0, 16.0, "").unwrap();
        let drawn = scratch.glyph("x", 1).unwrap().expect("ink");
        assert!(!drawn.has_color);
        assert!(!drawn.clipped);
        assert_eq!(drawn.image.image_dimensions(), (8, 10));
        assert_eq!(drawn.placement.bearing_x, 0.0);
    }

    #[test]
    fn nothing_painted_is_no_glyph() {
        let scratch = Scratch::new(&Fake, Size::new(9, 20), -4.0, 16.0, "").unwrap();
        assert!(scratch.glyph("", 1).unwrap().is_none());
    }
}
