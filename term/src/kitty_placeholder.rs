//! Kitty Unicode placeholder decoding for terminal cores and renderers.
//! The protocol table is independent of the host's Unicode/font version.

use crate::color::ColorAttribute;
use crate::Line;

pub const PLACEHOLDER: char = '\u{10eeee}';

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Placeholder {
    pub image_id: u32,
    pub placement_id: u32,
    pub row: u32,
    pub column: u32,
}

/// One decoder per physical text row. Only the immediately preceding cell can
/// supply missing coordinates; a gap, ordinary text or a color change breaks it.
#[derive(Default)]
pub struct Decoder {
    previous: Option<(usize, Placeholder)>,
}

fn color_id(color: ColorAttribute) -> u32 {
    match color {
        ColorAttribute::Default => 0,
        ColorAttribute::PaletteIndex(index) => u32::from(index),
        ColorAttribute::TrueColorWithDefaultFallback(rgb)
        | ColorAttribute::TrueColorWithPaletteFallback(rgb, _) => {
            let (r, g, b, _) = rgb.to_srgb_u8();
            (u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b)
        }
    }
}

fn diacritic(c: char) -> Option<u32> {
    DIACRITICS
        .binary_search(&(c as u32))
        .ok()
        .map(|index| index as u32)
}

impl Decoder {
    pub fn push(
        &mut self,
        column: usize,
        text: &str,
        foreground: ColorAttribute,
        underline: ColorAttribute,
    ) -> Option<Placeholder> {
        let mut chars = text.chars();
        if chars.next() != Some(PLACEHOLDER) {
            self.previous = None;
            return None;
        }
        let row = chars.next().and_then(diacritic);
        let col = chars.next().and_then(diacritic);
        let upper = chars.next().and_then(diacritic).map(|n| n as u8);
        let lower = color_id(foreground);
        let placement_id = color_id(underline);
        let previous = self
            .previous
            .filter(|(at, image)| {
                at.checked_add(1) == Some(column)
                    && image.image_id & 0xffffff == lower
                    && image.placement_id == placement_id
                    && row.is_none_or(|row| row == image.row)
                    && col.is_none_or(|col| image.column.checked_add(1) == Some(col))
                    && upper.is_none_or(|upper| u32::from(upper) == image.image_id >> 24)
            })
            .map(|(_, image)| image);
        let image = Placeholder {
            image_id: lower
                | (u32::from(
                    upper.unwrap_or_else(|| previous.map_or(0, |p| (p.image_id >> 24) as u8)),
                ) << 24),
            placement_id,
            row: row.unwrap_or_else(|| previous.map_or(0, |p| p.row)),
            column: col
                .unwrap_or_else(|| previous.and_then(|p| p.column.checked_add(1)).unwrap_or(0)),
        };
        self.previous = Some((column, image));
        Some(image)
    }
}

/// Visit without allocating a decoded-cell list or resolving palette colors.
pub fn visit(line: &Line, mut emit: impl FnMut(usize, Placeholder)) {
    // Wire rows already store contiguous text. Avoid decoding their graphemes
    // and attributes when no placeholder is present; as_str borrows this form.
    if line.is_compressed_for_scrollback() && !line.as_str().contains(PLACEHOLDER) {
        return;
    }
    let mut decoder = Decoder::default();
    for cell in line.visible_cells() {
        if let Some(image) = decoder.push(
            cell.cell_index(),
            cell.str(),
            cell.attrs().foreground(),
            cell.attrs().underline_color(),
        ) {
            emit(cell.cell_index(), image);
        }
    }
}

/// A single placeholder cell's piece of an aspect-preserving, centered image.
/// `rect` is local to the cell, and `uv` is normalized to the complete image.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Slice {
    pub rect: [f32; 4],
    pub uv: [f32; 4],
}

pub fn fit_cell(
    image: (u32, u32),
    grid: (u32, u32),
    cell: (f32, f32),
    row: u32,
    column: u32,
) -> Option<Slice> {
    let (iw, ih) = (f64::from(image.0), f64::from(image.1));
    let (cw, ch) = (f64::from(cell.0), f64::from(cell.1));
    if iw == 0.0 || ih == 0.0 || !cw.is_finite() || !ch.is_finite() || cw <= 0.0 || ch <= 0.0 {
        return None;
    }
    // A missing grid dimension uses the natural image size, as Kitty does.
    let cols = if grid.0 == 0 {
        (iw / cw).ceil()
    } else {
        f64::from(grid.0)
    };
    let rows = if grid.1 == 0 {
        (ih / ch).ceil()
    } else {
        f64::from(grid.1)
    };
    if f64::from(column) >= cols || f64::from(row) >= rows {
        return None;
    }
    let scale = (cols * cw / iw).min(rows * ch / ih);
    let (w, h) = (iw * scale, ih * scale);
    let (left, top) = ((cols * cw - w) / 2.0, (rows * ch - h) / 2.0);
    let (x, y) = (f64::from(column) * cw, f64::from(row) * ch);
    let (x1, y1, x2, y2) = (
        x.max(left),
        y.max(top),
        (x + cw).min(left + w),
        (y + ch).min(top + h),
    );
    if x2 <= x1 || y2 <= y1 {
        return None;
    }
    Some(Slice {
        rect: [
            (x1 - x) as f32,
            (y1 - y) as f32,
            (x2 - x) as f32,
            (y2 - y) as f32,
        ],
        uv: [
            ((x1 - left) / w) as f32,
            ((y1 - top) / h) as f32,
            ((x2 - left) / w) as f32,
            ((y2 - top) / h) as f32,
        ],
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Cell, CellAttributes};

    const P: &str = "\u{10eeee}";
    const FG: ColorAttribute = ColorAttribute::PaletteIndex(42);
    const DEFAULT: ColorAttribute = ColorAttribute::Default;

    #[test]
    fn explicit_and_omitted_coordinates_match_the_protocol_examples() {
        for row in 0..2 {
            let mut decoder = Decoder::default();
            let first = format!("{P}{}", char::from_u32(DIACRITICS[row]).unwrap());
            for (column, text) in [first.as_str(), P, P].into_iter().enumerate() {
                assert_eq!(
                    decoder.push(column, text, FG, DEFAULT),
                    Some(Placeholder {
                        image_id: 42,
                        placement_id: 0,
                        row: row as u32,
                        column: column as u32,
                    })
                );
            }
        }
    }

    #[test]
    fn high_byte_inherits_only_with_compatible_row_column_and_colors() {
        let mut decoder = Decoder::default();
        let start = format!("{P}\u{305}\u{305}\u{30e}");
        assert_eq!(
            decoder.push(0, &start, FG, DEFAULT).unwrap().image_id,
            33554474
        );
        assert_eq!(
            decoder
                .push(1, &format!("{P}\u{305}\u{30d}"), FG, DEFAULT)
                .unwrap()
                .image_id,
            33554474
        );
        let broken = decoder
            .push(2, &format!("{P}\u{30d}"), FG, DEFAULT)
            .unwrap();
        assert_eq!((broken.image_id, broken.row, broken.column), (42, 1, 0));
        decoder.push(3, &start, FG, DEFAULT);
        let broken = decoder
            .push(4, P, FG, ColorAttribute::PaletteIndex(3))
            .unwrap();
        assert_eq!(
            (broken.image_id, broken.placement_id, broken.column),
            (42, 3, 0)
        );
    }

    #[test]
    fn ordinary_text_and_non_adjacent_cells_break_inheritance() {
        let mut decoder = Decoder::default();
        let start = format!("{P}\u{30e}\u{30e}\u{30e}");
        decoder.push(0, &start, FG, DEFAULT);
        assert_eq!(decoder.push(1, "x", FG, DEFAULT), None);
        assert_eq!(decoder.push(2, P, FG, DEFAULT).unwrap().column, 0);
        decoder.push(3, &start, FG, DEFAULT);
        let next = decoder.push(5, P, FG, DEFAULT).unwrap();
        assert_eq!((next.image_id, next.row, next.column), (42, 0, 0));
    }

    #[test]
    fn raw_color_bytes_encode_ids_without_palette_resolution() {
        let rgb = ColorAttribute::TrueColorWithDefaultFallback(crate::color::SrgbaTuple(
            18.0 / 255.0,
            52.0 / 255.0,
            86.0 / 255.0,
            1.0,
        ));
        let image = Decoder::default()
            .push(0, P, rgb, ColorAttribute::PaletteIndex(7))
            .unwrap();
        assert_eq!((image.image_id, image.placement_id), (0x123456, 7));
        let mut attrs = CellAttributes::default();
        attrs.set_foreground(FG);
        let mut line = Line::from_cells(
            vec![
                Cell::new_grapheme_with_width(&format!("{P}\u{305}"), 1, attrs.clone()),
                Cell::new_grapheme_with_width(P, 1, attrs),
            ],
            1,
        );
        for compressed in [false, true] {
            if compressed {
                line.compress_for_scrollback();
            }
            let mut seen = vec![];
            visit(&line, |at, image| seen.push((at, image.column)));
            assert_eq!(seen, [(0, 0), (1, 1)]);
        }
    }

    #[test]
    fn table_covers_high_byte_and_non_bmp_coordinates() {
        assert!(DIACRITICS.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(DIACRITICS.len() > 256);
        assert_eq!(&DIACRITICS[..3], &[0x305, 0x30d, 0x30e]);
        for (index, &code) in DIACRITICS.iter().enumerate() {
            assert_eq!(diacritic(char::from_u32(code).unwrap()), Some(index as u32));
        }
        assert_eq!(diacritic('\u{301}'), None);
    }

    #[test]
    fn letterboxing_clips_each_cell_without_stretching_the_image() {
        let upper = fit_cell((200, 100), (2, 2), (10.0, 10.0), 0, 0).unwrap();
        assert_eq!(upper.rect, [0.0, 5.0, 10.0, 10.0]);
        assert_eq!(upper.uv, [0.0, 0.0, 0.5, 0.5]);
        let lower = fit_cell((200, 100), (2, 2), (10.0, 10.0), 1, 1).unwrap();
        assert_eq!(lower.rect, [0.0, 0.0, 10.0, 5.0]);
        assert_eq!(lower.uv, [0.5, 0.5, 1.0, 1.0]);
        assert!(fit_cell((200, 10), (2, 4), (10.0, 10.0), 0, 0).is_none());
        assert!(fit_cell((200, 100), (2, 2), (10.0, 10.0), 2, 0).is_none());
    }

    #[test]
    fn natural_grid_and_extreme_geometry_remain_finite() {
        assert_eq!(
            fit_cell((20, 20), (0, 0), (10.0, 10.0), 1, 1).unwrap().uv,
            [0.5, 0.5, 1.0, 1.0]
        );
        for cell in [(0.0, 1.0), (1.0, f32::NAN), (f32::INFINITY, 1.0)] {
            assert!(fit_cell((20, 20), (2, 2), cell, 0, 0).is_none());
        }
        assert!(fit_cell((0, 20), (2, 2), (10.0, 10.0), 0, 0).is_none());
        if let Some(slice) = fit_cell(
            (u32::MAX, u32::MAX),
            (u32::MAX, u32::MAX),
            (100.0, 100.0),
            100,
            100,
        ) {
            assert!(slice
                .rect
                .iter()
                .chain(slice.uv.iter())
                .all(|v| v.is_finite()));
            assert!(slice.uv.iter().all(|v| (0.0..=1.0).contains(v)));
        }
    }
}

// Protocol rowcolumn-diacritics.txt, derived from Unicode 6.0 combining marks.
// https://sw.kovidgoyal.net/kitty/graphics-protocol/#unicode-placeholders
const DIACRITICS: &[u32] = &[
    0x305, 0x30d, 0x30e, 0x310, 0x312, 0x33d, 0x33e, 0x33f, 0x346, 0x34a, 0x34b, 0x34c, 0x350,
    0x351, 0x352, 0x357, 0x35b, 0x363, 0x364, 0x365, 0x366, 0x367, 0x368, 0x369, 0x36a, 0x36b,
    0x36c, 0x36d, 0x36e, 0x36f, 0x483, 0x484, 0x485, 0x486, 0x487, 0x592, 0x593, 0x594, 0x595,
    0x597, 0x598, 0x599, 0x59c, 0x59d, 0x59e, 0x59f, 0x5a0, 0x5a1, 0x5a8, 0x5a9, 0x5ab, 0x5ac,
    0x5af, 0x5c4, 0x610, 0x611, 0x612, 0x613, 0x614, 0x615, 0x616, 0x617, 0x657, 0x658, 0x659,
    0x65a, 0x65b, 0x65d, 0x65e, 0x6d6, 0x6d7, 0x6d8, 0x6d9, 0x6da, 0x6db, 0x6dc, 0x6df, 0x6e0,
    0x6e1, 0x6e2, 0x6e4, 0x6e7, 0x6e8, 0x6eb, 0x6ec, 0x730, 0x732, 0x733, 0x735, 0x736, 0x73a,
    0x73d, 0x73f, 0x740, 0x741, 0x743, 0x745, 0x747, 0x749, 0x74a, 0x7eb, 0x7ec, 0x7ed, 0x7ee,
    0x7ef, 0x7f0, 0x7f1, 0x7f3, 0x816, 0x817, 0x818, 0x819, 0x81b, 0x81c, 0x81d, 0x81e, 0x81f,
    0x820, 0x821, 0x822, 0x823, 0x825, 0x826, 0x827, 0x829, 0x82a, 0x82b, 0x82c, 0x82d, 0x951,
    0x953, 0x954, 0xf82, 0xf83, 0xf86, 0xf87, 0x135d, 0x135e, 0x135f, 0x17dd, 0x193a, 0x1a17,
    0x1a75, 0x1a76, 0x1a77, 0x1a78, 0x1a79, 0x1a7a, 0x1a7b, 0x1a7c, 0x1b6b, 0x1b6d, 0x1b6e, 0x1b6f,
    0x1b70, 0x1b71, 0x1b72, 0x1b73, 0x1cd0, 0x1cd1, 0x1cd2, 0x1cda, 0x1cdb, 0x1ce0, 0x1dc0, 0x1dc1,
    0x1dc3, 0x1dc4, 0x1dc5, 0x1dc6, 0x1dc7, 0x1dc8, 0x1dc9, 0x1dcb, 0x1dcc, 0x1dd1, 0x1dd2, 0x1dd3,
    0x1dd4, 0x1dd5, 0x1dd6, 0x1dd7, 0x1dd8, 0x1dd9, 0x1dda, 0x1ddb, 0x1ddc, 0x1ddd, 0x1dde, 0x1ddf,
    0x1de0, 0x1de1, 0x1de2, 0x1de3, 0x1de4, 0x1de5, 0x1de6, 0x1dfe, 0x20d0, 0x20d1, 0x20d4, 0x20d5,
    0x20d6, 0x20d7, 0x20db, 0x20dc, 0x20e1, 0x20e7, 0x20e9, 0x20f0, 0x2cef, 0x2cf0, 0x2cf1, 0x2de0,
    0x2de1, 0x2de2, 0x2de3, 0x2de4, 0x2de5, 0x2de6, 0x2de7, 0x2de8, 0x2de9, 0x2dea, 0x2deb, 0x2dec,
    0x2ded, 0x2dee, 0x2def, 0x2df0, 0x2df1, 0x2df2, 0x2df3, 0x2df4, 0x2df5, 0x2df6, 0x2df7, 0x2df8,
    0x2df9, 0x2dfa, 0x2dfb, 0x2dfc, 0x2dfd, 0x2dfe, 0x2dff, 0xa66f, 0xa67c, 0xa67d, 0xa6f0, 0xa6f1,
    0xa8e0, 0xa8e1, 0xa8e2, 0xa8e3, 0xa8e4, 0xa8e5, 0xa8e6, 0xa8e7, 0xa8e8, 0xa8e9, 0xa8ea, 0xa8eb,
    0xa8ec, 0xa8ed, 0xa8ee, 0xa8ef, 0xa8f0, 0xa8f1, 0xaab0, 0xaab2, 0xaab3, 0xaab7, 0xaab8, 0xaabe,
    0xaabf, 0xaac1, 0xfe20, 0xfe21, 0xfe22, 0xfe23, 0xfe24, 0xfe25, 0xfe26, 0x10a0f, 0x10a38,
    0x1d185, 0x1d186, 0x1d187, 0x1d188, 0x1d189, 0x1d1aa, 0x1d1ab, 0x1d1ac, 0x1d1ad, 0x1d242,
    0x1d243, 0x1d244,
];
