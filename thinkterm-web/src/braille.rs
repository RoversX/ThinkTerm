//! Braille dot patterns, drawn rather than looked up.
//!
//! U+2800..=U+28FF put the pattern in the low byte of the codepoint, so the
//! character *is* the bitmap: eight dots in two columns of four. Programs
//! that draw graphs in a terminal -- btop, macmon, gitui -- use them as a
//! 2x4 pixel grid, so a face without them turns every graph into a row of
//! missing-glyph boxes, which is what the bundled faces do: neither covers
//! a single one of the 256.
//!
//! The desktop does not consult a font for these either; `customglyph.rs`
//! draws them, and this is the same arithmetic, squares and all. Keeping a
//! second copy is deliberate: it is two dozen lines of pure geometry, and
//! sharing it would mean reaching into the desktop's glyph cache.

use thinkterm_render::bitmaps::{BitmapImage, Image};
use thinkterm_render::geom::{Point, Rect, Size};
use wezterm_color_types::SrgbaPixel;

/// The dot pattern this character stands for, if it is Braille at all.
pub fn dots(c: char) -> Option<u8> {
    let n = c as u32;
    (0x2800..=0x28ff).contains(&n).then_some((n & 0xff) as u8)
}

/// Which dot each bit lights, as a column and a row of the 2x4 grid:
///
/// ```text
///   1 4      bit 0  bit 3
///   2 5      bit 1  bit 4
///   3 6      bit 2  bit 5
///   7 8      bit 6  bit 7
/// ```
///
/// The last row is the two dots Unicode added to the six-dot alphabet, so
/// its bits are the high ones rather than the ones that follow the column.
const GRID: [(u8, isize, isize); 8] = [
    (1 << 0, 0, 0),
    (1 << 1, 0, 1),
    (1 << 2, 0, 2),
    (1 << 3, 1, 0),
    (1 << 4, 1, 1),
    (1 << 5, 1, 2),
    (1 << 6, 0, 3),
    (1 << 7, 1, 3),
];

/// The squares to fill for a pattern, in a cell of this size.
///
/// Each dot gets half the cell's width and a quarter of its height, and is
/// a square half that width centred in it. The arithmetic is done in
/// floating point and rounded per dot: at small sizes a cell is only a few
/// pixels across, and dividing integers twice would drop a column or leave
/// the rows unevenly spaced. A dot is never smaller than one pixel, so a
/// pattern never disappears entirely.
pub fn rects(dots: u8, cell: Size) -> Vec<Rect> {
    let area_width = cell.width as f64 / 2.0;
    let area_height = cell.height as f64 / 4.0;
    let side = (area_width / 2.0).round().max(1.0);
    GRID.iter()
        .filter(|(bit, _, _)| dots & bit != 0)
        .map(|(_, col, row)| {
            let x = (*col as f64 * area_width + (area_width - side) / 2.0).round();
            let y = (*row as f64 * area_height + (area_height - side) / 2.0).round();
            // Never past the cell: a rounded-up square in the last row or
            // column would be clipped by the atlas rather than by us.
            let w = side.min(cell.width as f64 - x).max(1.0);
            let h = side.min(cell.height as f64 - y).max(1.0);
            Rect::new(
                Point::new(x as isize, y as isize),
                Size::new(w as isize, h as isize),
            )
        })
        .collect()
}

/// A cell-sized image with the pattern's dots in white on transparent.
/// The glyph is coloured by the shader like any other monochrome one.
pub fn image(dots: u8, cell: Size) -> Image {
    let mut buffer = Image::new(cell.width as usize, cell.height as usize);
    buffer.clear_rect(
        Rect::new(Point::new(0, 0), cell),
        SrgbaPixel::rgba(0, 0, 0, 0),
    );
    for rect in rects(dots, cell) {
        buffer.clear_rect(rect, SrgbaPixel::rgba(0xff, 0xff, 0xff, 0xff));
    }
    buffer
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cell() -> Size {
        Size::new(16, 32)
    }

    fn lit(image: &Image) -> usize {
        image.pixels().iter().filter(|p| **p != 0).count()
    }

    #[test]
    fn only_the_braille_block_has_a_pattern() {
        assert_eq!(dots('\u{2800}'), Some(0x00));
        assert_eq!(dots('\u{28ff}'), Some(0xff));
        assert_eq!(dots('⣀'), Some(0xc0));
        assert_eq!(dots('\u{27ff}'), None);
        assert_eq!(dots('\u{2900}'), None);
        assert_eq!(dots('a'), None);
    }

    #[test]
    fn a_blank_pattern_draws_nothing_and_a_full_one_draws_eight_dots() {
        assert!(rects(0x00, cell()).is_empty());
        assert_eq!(lit(&image(0x00, cell())), 0, "U+2800 must be transparent");
        let full = rects(0xff, cell());
        assert_eq!(full.len(), 8);
        assert_eq!(
            lit(&image(0xff, cell())),
            full.iter()
                .map(|r| (r.size.width * r.size.height) as usize)
                .sum::<usize>(),
            "the eight dots must not overlap"
        );
    }

    #[test]
    fn every_dot_lands_in_its_own_quarter_of_its_own_column() {
        // bit -> (which half across, which quarter down)
        for (bit, col, row) in GRID {
            let rect = rects(bit, cell()).pop().expect("one dot");
            let (x, y) = (rect.origin.x, rect.origin.y);
            let (right, bottom) = (x + rect.size.width, y + rect.size.height);
            assert!(x >= 0 && y >= 0, "bit {bit:#04x} left the cell");
            assert!(
                right <= cell().width && bottom <= cell().height,
                "bit {bit:#04x} ran past the cell"
            );
            let half = cell().width / 2;
            let quarter = cell().height / 4;
            assert!(
                x >= col * half && right <= (col + 1) * half,
                "bit {bit:#04x} is not in column {col}"
            );
            assert!(
                y >= row * quarter && bottom <= (row + 1) * quarter,
                "bit {bit:#04x} is not in row {row}"
            );
        }
    }

    #[test]
    fn each_pattern_draws_its_own_dots_and_no_others() {
        // The cache is keyed by this byte, so two patterns that drew the
        // same pixels would be indistinguishable on screen: btop's graph
        // would hold one shape while the data moved.
        let mut seen: std::collections::HashMap<Vec<u32>, u8> = std::collections::HashMap::new();
        for pattern in 0u8..=0xff {
            let drawn = image(pattern, cell());
            assert_eq!(
                rects(pattern, cell()).len(),
                pattern.count_ones() as usize,
                "{pattern:#04x} drew the wrong number of dots"
            );
            if let Some(other) = seen.insert(drawn.pixels().to_vec(), pattern) {
                panic!("{pattern:#04x} draws the same pixels as {other:#04x}");
            }
        }
    }

    #[test]
    fn a_cell_too_small_to_divide_still_draws_something() {
        // A four-pixel-wide cell leaves a one-pixel dot; it must not round
        // to nothing, or a whole graph vanishes at small sizes.
        for cell in [Size::new(4, 8), Size::new(5, 9), Size::new(3, 6)] {
            let full = rects(0xff, cell);
            assert_eq!(full.len(), 8, "{cell:?} lost a dot");
            for rect in &full {
                assert!(rect.size.width >= 1 && rect.size.height >= 1, "{cell:?}");
                assert!(
                    rect.origin.x + rect.size.width <= cell.width
                        && rect.origin.y + rect.size.height <= cell.height,
                    "{cell:?} drew outside the cell"
                );
            }
            assert!(lit(&image(0xff, cell)) > 0, "{cell:?} drew nothing");
        }
    }
}
