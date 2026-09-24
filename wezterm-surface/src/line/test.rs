#![cfg(test)]

use super::*;
use crate::hyperlink::{Hyperlink, Rule};
use crate::line::clusterline::ClusteredLine;
use crate::SEQ_ZERO;
use alloc::sync::Arc;
use k9::assert_equal as assert_eq;
use wezterm_cell::{Cell, CellAttributes};

#[test]
fn grapheme_appends_preserve_gaps_and_wide_overwrites() {
    let plain = CellAttributes::default();
    let mut line = Line::new(1);
    line.set_cell_grapheme(0, "界", 2, plain.clone(), 2);
    line.set_cell_grapheme(2, "e\u{301}", 1, plain.clone(), 3);
    assert_eq!(line.as_str(), "界e\u{301}");
    assert_eq!(line.len(), 3);

    // A default blank beyond the end remains implicit, including metadata.
    let before = line.clone();
    line.set_cell_grapheme(6, " ", 1, plain.clone(), 4);
    assert_eq!(line, before);

    line.set_cell_grapheme(5, "z", 1, plain.clone(), 5);
    assert_eq!(line.as_str(), "界e\u{301}  z");
    assert_eq!(line.len(), 6);
    line.set_cell_grapheme(1, "x", 1, plain.clone(), 6);
    assert_eq!(line.as_str(), " xe\u{301}  z");
    assert_eq!(line.len(), 6);

    // A colored blank must remain visible, including after a storage change.
    for compact in [false, true] {
        let mut line = line.clone();
        if compact {
            line.compress_for_scrollback();
        }
        let mut colored = plain.clone();
        colored.set_background(wezterm_cell::color::ColorAttribute::PaletteIndex(4));
        line.set_cell_grapheme(8, " ", 1, colored.clone(), 7);
        assert_eq!(line.as_str(), " xe\u{301}  z   ");
        assert_eq!(line.len(), 9);
        assert_eq!(line.visible_cells().last().unwrap().attrs(), &colored);
        assert_eq!(line.compute_shape_hash(), {
            let mut expected = Line::from_text(" xe\u{301}  z   ", &plain, 7, None);
            *expected.cells_mut()[8].attrs_mut() = colored;
            expected.compute_shape_hash()
        });
    }
}

#[test]
fn compact_gap_padding_matches_individual_blank_cells() {
    let mut linked = CellAttributes::default();
    linked.set_hyperlink(Some(Arc::new(Hyperlink::new("https://example.org"))));
    for prefix in ["", "ascii", "界🙂e\u{301}"] {
        let mut base = Line::from_text(prefix, &linked, 1, None);
        base.compress_for_scrollback();
        for gap in [0, 1, 7, 63, 64, 127, 128, 129, 65535, 65536] {
            for (text, width, attrs) in [
                ("x", 1, CellAttributes::default()),
                ("界", 2, linked.clone()),
            ] {
                let idx = base.len() + gap;
                let mut expected = base.clone();
                // Contiguous scalar appends do not use the gap-padding path.
                while expected.len() < idx {
                    expected.set_cell_grapheme(
                        expected.len(), " ", 1, CellAttributes::blank(), 42,
                    );
                }
                expected.set_cell_grapheme(idx, text, width, attrs.clone(), 42);
                let mut actual = base.clone();
                actual.set_cell_grapheme(idx, text, width, attrs, 42);
                assert_eq!(actual, expected, "prefix={prefix:?}, gap={gap}");
                assert_eq!(actual.compute_shape_hash(), expected.compute_shape_hash());
            }
        }
    }
}

#[test]
fn scroll_reset_discards_oversized_storage() {
    let mut line = Line::new(1);
    line.set_ascii_cells(0, &"x".repeat(8192), &CellAttributes::default(), 2);
    line.reset_for_scrolling(3, 116);
    assert_eq!(line, Line::new(3));
    match &line.cells {
        super::storage::CellStorage::C(cl) => assert!(cl.text.capacity() <= 232),
        _ => panic!("reset row should use compact storage"),
    }
}

#[cfg(feature = "use_image")]
#[test]
fn scroll_reset_releases_image_references() {
    use wezterm_cell::image::{ImageCell, ImageData, ImageDataType, TextureCoordinate};
    let data = Arc::new(ImageData::with_data(ImageDataType::new_single_frame(
        1,
        1,
        vec![255; 4],
    )));
    for vector in [false, true] {
        let mut attrs = CellAttributes::default();
        attrs.attach_image(Box::new(ImageCell::with_z_index(
            TextureCoordinate::new_f32(0.0, 0.0),
            TextureCoordinate::new_f32(1.0, 1.0),
            Arc::clone(&data),
            0,
            0,
            0,
            0,
            0,
            Some(3),
            Some(7),
        )));
        let mut line = Line::new(1);
        line.set_cell_grapheme(0, "x", 1, attrs, 1);
        if vector {
            line.cells_mut();
        }
        line.reset_for_scrolling(2, 116);
        assert_eq!(Arc::strong_count(&data), 1);
        assert_eq!(line, Line::new(2));
    }
}

#[test]
fn recycled_rows_match_new_rows_and_release_links_and_cached_data() {
    for text in ["x", "界🙂e\u{301}", "https://example.com", ""] {
        for vector in [false, true] {
            let link = Arc::new(Hyperlink::new("https://example.org"));
            let mut attrs = CellAttributes::default();
            attrs.set_hyperlink(Some(Arc::clone(&link)));
            attrs.set_semantic_type(wezterm_cell::SemanticType::Prompt);
            let mut line = Line::new(7);
            for g in finl_unicode::grapheme_clusters::Graphemes::new(text) {
                let cell = Cell::new_grapheme(g, attrs.clone(), None);
                line.set_cell(line.len(), cell, 7);
            }
            drop(attrs);
            if vector {
                line.cells_mut();
            }
            line.set_double_width(7);
            line.set_bidi_info(true, wezterm_bidi::ParagraphDirectionHint::RightToLeft, 7);
            line.semantic_zone_ranges();
            #[cfg(feature = "appdata")]
            let appdata = Arc::new(123u32);
            #[cfg(feature = "appdata")]
            line.set_appdata(Arc::clone(&appdata));
            line.reset_for_scrolling(9, 116);
            assert_eq!(line, Line::new(9));
            assert!(line.semantic_zone_ranges().is_empty());
            assert_eq!(Arc::strong_count(&link), 1);
            #[cfg(feature = "appdata")]
            {
                assert!(line.get_appdata().is_none());
                assert_eq!(Arc::weak_count(&appdata), 0);
            }
            // Previously-wide rows must also behave like fresh rows on reuse.
            line.set_cell_grapheme(1, "y", 1, CellAttributes::default(), 10);
            let mut expected = Line::new(9);
            expected.set_cell_grapheme(1, "y", 1, CellAttributes::default(), 10);
            assert_eq!(line, expected);
        }
    }
}

// Keep a scalar reference so the fast path is checked against the old behavior,
// including less visible state such as hyperlinks, sequence numbers and zones.
fn scalar_range(line: &mut Line, range: core::ops::Range<usize>, cell: &Cell, clear: bool) {
    if clear && line.len() == 0 && *cell == Cell::blank() {
        return;
    }
    for x in range {
        if clear {
            line.set_cell_clearing_image_placements(x, cell.clone(), 42);
        } else {
            line.set_cell(x, cell.clone(), 42);
        }
    }
    if clear {
        line.prune_trailing_blanks(42);
    }
}

fn check_range_against_scalar(
    base: &Line,
    range: core::ops::Range<usize>,
    cell: &Cell,
    clear: bool,
) {
    let mut expected = base.clone();
    scalar_range(&mut expected, range.clone(), cell, clear);
    let mut actual = base.clone();
    if clear {
        actual.fill_range(range.clone(), cell, 42);
    } else {
        actual.set_cell_range(range.clone(), cell, 42);
    }
    assert_eq!(
        actual, expected,
        "range={range:?}, cell={cell:?}, clear={clear}"
    );
    assert_eq!(
        actual.semantic_zone_ranges(),
        expected.semantic_zone_ranges()
    );
    assert_eq!(actual.compute_shape_hash(), expected.compute_shape_hash());
}

#[test]
fn cell_ranges_match_scalar_writes_at_wide_boundaries_and_beyond_end() {
    let mut colored = CellAttributes::default();
    colored.set_background(wezterm_cell::color::ColorAttribute::PaletteIndex(4));
    let cells = [
        Cell::blank(),
        Cell::blank_with_attrs(colored.clone()),
        Cell::new('x', colored),
        Cell::new_grapheme("界", CellAttributes::default(), None),
        Cell::new_grapheme("e\u{301}", CellAttributes::default(), None),
    ];
    for text in ["", "abc   ", "a界b🙂c", "界界", "e\u{301}x", "    "] {
        for compressed in [false, true] {
            let mut base: Line = text.into();
            if compressed {
                base.compress_for_scrollback();
            } else {
                base.coerce_vec_storage();
            }
            for start in 0..base.len() + 3 {
                for end in start..base.len() + 5 {
                    for cell in &cells {
                        for clear in [false, true] {
                            check_range_against_scalar(&base, start..end, cell, clear);
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn cell_ranges_invalidate_implicit_links_and_preserve_explicit_links() {
    let mut base: Line = "https://example.com abc".into();
    base.scan_and_create_hyperlinks(&[Rule::new(r"https://\S+", "$0").unwrap()]);
    let mut attrs = CellAttributes::default();
    attrs.set_hyperlink(Some(Arc::new(Hyperlink::new("https://example.org"))));
    attrs.set_semantic_type(wezterm_cell::SemanticType::Prompt);
    base.semantic_zone_ranges();
    for cell in [Cell::blank(), Cell::new('x', attrs)] {
        for clear in [false, true] {
            check_range_against_scalar(&base, 2..15, &cell, clear);
            check_range_against_scalar(&base, 2..2, &cell, clear);
        }
    }
}

#[test]
fn appending_ranges_keeps_compact_storage() {
    let mut line = Line::new(0);
    line.set_cell_range(0..4096, &Cell::new('x', CellAttributes::default()), 1);
    assert!(line.is_compressed_for_scrollback());
    assert_eq!(line.len(), 4096);
    let mut empty = Line::new(0);
    empty.fill_range(0..usize::MAX, &Cell::blank(), 1);
    assert_eq!(empty.len(), 0);
    assert!(empty.is_compressed_for_scrollback());
}

#[test]
fn erasing_a_default_tail_does_not_materialize_out_of_range_blanks() {
    let mut line: Line = "prefix    tail".into();
    line.coerce_vec_storage();
    let mut expected = line.clone();
    scalar_range(&mut expected, 6..100, &Cell::blank(), true);
    line.fill_range(6..usize::MAX, &Cell::blank(), 42);
    assert_eq!(line, expected);
    assert_eq!(line.as_str(), "prefix");
}

#[cfg(feature = "use_image")]
#[test]
fn cell_ranges_preserve_or_clear_image_placements_like_scalar_writes() {
    use wezterm_cell::image::{ImageCell, ImageData, ImageDataType, TextureCoordinate};
    let data = Arc::new(ImageData::with_data(ImageDataType::new_single_frame(
        1,
        1,
        vec![255; 4],
    )));
    let mut base: Line = "a界b cdef".into();
    for x in 0..base.len() {
        for placement in [None, Some(7)] {
            base.cells_mut()[x]
                .attrs_mut()
                .attach_image(Box::new(ImageCell::with_z_index(
                    TextureCoordinate::new_f32(0.0, 0.0),
                    TextureCoordinate::new_f32(1.0, 1.0),
                    Arc::clone(&data),
                    0,
                    0,
                    0,
                    0,
                    0,
                    Some(3),
                    placement,
                )));
        }
    }
    let image_cell = base.cells_mut()[4].clone();
    for cell in [
        Cell::blank(),
        Cell::new('x', CellAttributes::default()),
        image_cell,
    ] {
        for start in 0..base.len() {
            for end in start..base.len() + 2 {
                for clear in [false, true] {
                    check_range_against_scalar(&base, start..end, &cell, clear);
                }
            }
        }
    }
}

/// There are 4 double-wide graphemes that occupy 2 cells each.
/// When we join the lines, we must preserve the invisible blank
/// that is part of the grapheme otherwise our metrics will be
/// wrong.
/// <https://github.com/wezterm/wezterm/issues/2568>
#[test]
fn append_line() {
    let mut line1: Line = "0123456789".into();
    let line2: Line = "グループaa".into();

    line1.append_line(line2, SEQ_ZERO);

    assert_eq!(line1.len(), 20);
}

#[test]
fn hyperlinks() {
    let text = "❤ 😍🤢 http://example.com \u{1f468}\u{1f3fe}\u{200d}\u{1f9b0} http://example.com";

    let rules = vec![
        Rule::new(r"\b\w+://(?:[\w.-]+)\.[a-z]{2,15}\S*\b", "$0").unwrap(),
        Rule::new(r"\b\w+@[\w-]+(\.[\w-]+)+\b", "mailto:$0").unwrap(),
    ];

    let hyperlink = Arc::new(Hyperlink::new_implicit("http://example.com"));
    let hyperlink_attr = CellAttributes::default()
        .set_hyperlink(Some(hyperlink.clone()))
        .clone();

    let mut line: Line = text.into();
    line.scan_and_create_hyperlinks(&rules);
    assert!(line.has_hyperlink());
    assert_eq!(
        line.coerce_vec_storage().to_vec(),
        vec![
            Cell::new_grapheme("❤", CellAttributes::default(), None),
            Cell::new(' ', CellAttributes::default()), // double width spacer
            Cell::new_grapheme("😍", CellAttributes::default(), None),
            Cell::new(' ', CellAttributes::default()), // double width spacer
            Cell::new_grapheme("🤢", CellAttributes::default(), None),
            Cell::new(' ', CellAttributes::default()), // double width spacer
            Cell::new(' ', CellAttributes::default()),
            Cell::new('h', hyperlink_attr.clone()),
            Cell::new('t', hyperlink_attr.clone()),
            Cell::new('t', hyperlink_attr.clone()),
            Cell::new('p', hyperlink_attr.clone()),
            Cell::new(':', hyperlink_attr.clone()),
            Cell::new('/', hyperlink_attr.clone()),
            Cell::new('/', hyperlink_attr.clone()),
            Cell::new('e', hyperlink_attr.clone()),
            Cell::new('x', hyperlink_attr.clone()),
            Cell::new('a', hyperlink_attr.clone()),
            Cell::new('m', hyperlink_attr.clone()),
            Cell::new('p', hyperlink_attr.clone()),
            Cell::new('l', hyperlink_attr.clone()),
            Cell::new('e', hyperlink_attr.clone()),
            Cell::new('.', hyperlink_attr.clone()),
            Cell::new('c', hyperlink_attr.clone()),
            Cell::new('o', hyperlink_attr.clone()),
            Cell::new('m', hyperlink_attr.clone()),
            Cell::new(' ', CellAttributes::default()),
            Cell::new_grapheme(
                // man: dark skin tone, red hair ZWJ emoji grapheme
                "\u{1f468}\u{1f3fe}\u{200d}\u{1f9b0}",
                CellAttributes::default(),
                None,
            ),
            Cell::new(' ', CellAttributes::default()), // double width spacer
            Cell::new(' ', CellAttributes::default()),
            Cell::new('h', hyperlink_attr.clone()),
            Cell::new('t', hyperlink_attr.clone()),
            Cell::new('t', hyperlink_attr.clone()),
            Cell::new('p', hyperlink_attr.clone()),
            Cell::new(':', hyperlink_attr.clone()),
            Cell::new('/', hyperlink_attr.clone()),
            Cell::new('/', hyperlink_attr.clone()),
            Cell::new('e', hyperlink_attr.clone()),
            Cell::new('x', hyperlink_attr.clone()),
            Cell::new('a', hyperlink_attr.clone()),
            Cell::new('m', hyperlink_attr.clone()),
            Cell::new('p', hyperlink_attr.clone()),
            Cell::new('l', hyperlink_attr.clone()),
            Cell::new('e', hyperlink_attr.clone()),
            Cell::new('.', hyperlink_attr.clone()),
            Cell::new('c', hyperlink_attr.clone()),
            Cell::new('o', hyperlink_attr.clone()),
            Cell::new('m', hyperlink_attr.clone()),
        ]
    );
}

#[test]
fn double_click_range_bounds() {
    let line: Line = "hello".into();
    let r = line.compute_double_click_range(200, |_| true);
    assert_eq!(r, DoubleClickRange::Range(200..200));
}

#[test]
fn cluster_representation_basic() {
    let line: Line = "hello".into();
    let mut compressed = line.clone();
    compressed.compress_for_scrollback();
    k9::snapshot!(
        &compressed.cells,
        r#"
C(
    ClusteredLine {
        text: "hello",
        is_double_wide: None,
        clusters: [
            Cluster {
                cell_width: 5,
                attrs: CellAttributes {
                    attributes: 0,
                    intensity: Normal,
                    underline: None,
                    blink: None,
                    italic: false,
                    reverse: false,
                    strikethrough: false,
                    invisible: false,
                    wrapped: false,
                    overline: false,
                    semantic_type: Output,
                    foreground: Default,
                    background: Default,
                    fat: None,
                },
            },
        ],
        len: 5,
        last_cell_width: Some(
            1,
        ),
    },
)
"#
    );
    compressed.coerce_vec_storage();
    assert_eq!(line, compressed);
}

#[test]
fn cluster_representation_double_width() {
    let line: Line = "❤ 😍🤢he❤ 😍🤢llo❤ 😍🤢".into();
    let mut compressed = line.clone();
    compressed.compress_for_scrollback();
    k9::snapshot!(
        &compressed.cells,
        r#"
C(
    ClusteredLine {
        text: "❤ 😍🤢he❤ 😍🤢llo❤ 😍🤢",
        is_double_wide: Some(
            FixedBitSet {
                data: [
                    2626580,
                ],
                length: 23,
            },
        ),
        clusters: [
            Cluster {
                cell_width: 23,
                attrs: CellAttributes {
                    attributes: 0,
                    intensity: Normal,
                    underline: None,
                    blink: None,
                    italic: false,
                    reverse: false,
                    strikethrough: false,
                    invisible: false,
                    wrapped: false,
                    overline: false,
                    semantic_type: Output,
                    foreground: Default,
                    background: Default,
                    fat: None,
                },
            },
        ],
        len: 23,
        last_cell_width: Some(
            1,
        ),
    },
)
"#
    );
    compressed.coerce_vec_storage();
    assert_eq!(line, compressed);
}

#[test]
fn cluster_representation_empty() {
    let line = Line::from_cells(vec![], SEQ_ZERO);

    let mut compressed = line.clone();
    compressed.compress_for_scrollback();
    k9::snapshot!(
        &compressed.cells,
        r#"
C(
    ClusteredLine {
        text: "",
        is_double_wide: None,
        clusters: [],
        len: 0,
        last_cell_width: None,
    },
)
"#
    );
    compressed.coerce_vec_storage();
    assert_eq!(line, compressed);
}

#[test]
fn cluster_wrap_last() {
    let mut line: Line = "hello".into();
    line.compress_for_scrollback();
    line.set_last_cell_was_wrapped(true, 1);
    k9::snapshot!(
        line,
        r#"
Line {
    cells: C(
        ClusteredLine {
            text: "hello",
            is_double_wide: None,
            clusters: [
                Cluster {
                    cell_width: 4,
                    attrs: CellAttributes {
                        attributes: 0,
                        intensity: Normal,
                        underline: None,
                        blink: None,
                        italic: false,
                        reverse: false,
                        strikethrough: false,
                        invisible: false,
                        wrapped: false,
                        overline: false,
                        semantic_type: Output,
                        foreground: Default,
                        background: Default,
                        fat: None,
                    },
                },
                Cluster {
                    cell_width: 1,
                    attrs: CellAttributes {
                        attributes: 2048,
                        intensity: Normal,
                        underline: None,
                        blink: None,
                        italic: false,
                        reverse: false,
                        strikethrough: false,
                        invisible: false,
                        wrapped: true,
                        overline: false,
                        semantic_type: Output,
                        foreground: Default,
                        background: Default,
                        fat: None,
                    },
                },
            ],
            len: 5,
            last_cell_width: Some(
                1,
            ),
        },
    ),
    zones: [],
    seqno: 1,
    bits: LineBits(
        0x0,
    ),
    appdata: Mutex {
        data: None,
        poisoned: false,
        ..
    },
}
"#
    );
}

fn bold() -> CellAttributes {
    use wezterm_cell::Intensity;
    let mut attr = CellAttributes::default();
    attr.set_intensity(Intensity::Bold);
    attr
}

#[test]
fn cluster_representation_attributes() {
    let line = Line::from_cells(
        vec![
            Cell::new_grapheme("a", CellAttributes::default(), None),
            Cell::new_grapheme("b", bold(), None),
            Cell::new_grapheme("c", CellAttributes::default(), None),
            Cell::new_grapheme("d", bold(), None),
        ],
        SEQ_ZERO,
    );

    let mut compressed = line.clone();
    compressed.compress_for_scrollback();
    k9::snapshot!(
        &compressed.cells,
        r#"
C(
    ClusteredLine {
        text: "abcd",
        is_double_wide: None,
        clusters: [
            Cluster {
                cell_width: 1,
                attrs: CellAttributes {
                    attributes: 0,
                    intensity: Normal,
                    underline: None,
                    blink: None,
                    italic: false,
                    reverse: false,
                    strikethrough: false,
                    invisible: false,
                    wrapped: false,
                    overline: false,
                    semantic_type: Output,
                    foreground: Default,
                    background: Default,
                    fat: None,
                },
            },
            Cluster {
                cell_width: 1,
                attrs: CellAttributes {
                    attributes: 1,
                    intensity: Bold,
                    underline: None,
                    blink: None,
                    italic: false,
                    reverse: false,
                    strikethrough: false,
                    invisible: false,
                    wrapped: false,
                    overline: false,
                    semantic_type: Output,
                    foreground: Default,
                    background: Default,
                    fat: None,
                },
            },
            Cluster {
                cell_width: 1,
                attrs: CellAttributes {
                    attributes: 0,
                    intensity: Normal,
                    underline: None,
                    blink: None,
                    italic: false,
                    reverse: false,
                    strikethrough: false,
                    invisible: false,
                    wrapped: false,
                    overline: false,
                    semantic_type: Output,
                    foreground: Default,
                    background: Default,
                    fat: None,
                },
            },
            Cluster {
                cell_width: 1,
                attrs: CellAttributes {
                    attributes: 1,
                    intensity: Bold,
                    underline: None,
                    blink: None,
                    italic: false,
                    reverse: false,
                    strikethrough: false,
                    invisible: false,
                    wrapped: false,
                    overline: false,
                    semantic_type: Output,
                    foreground: Default,
                    background: Default,
                    fat: None,
                },
            },
        ],
        len: 4,
        last_cell_width: Some(
            1,
        ),
    },
)
"#
    );
    compressed.coerce_vec_storage();
    assert_eq!(line, compressed);
}

#[test]
fn cluster_append() {
    let mut cl = ClusteredLine::new();
    cl.append(Cell::new_grapheme("h", CellAttributes::default(), None));
    cl.append(Cell::new_grapheme("e", CellAttributes::default(), None));
    cl.append(Cell::new_grapheme("l", bold(), None));
    cl.append(Cell::new_grapheme("l", CellAttributes::default(), None));
    cl.append(Cell::new_grapheme("o", CellAttributes::default(), None));
    k9::snapshot!(
        cl,
        r#"
ClusteredLine {
    text: "hello",
    is_double_wide: None,
    clusters: [
        Cluster {
            cell_width: 2,
            attrs: CellAttributes {
                attributes: 0,
                intensity: Normal,
                underline: None,
                blink: None,
                italic: false,
                reverse: false,
                strikethrough: false,
                invisible: false,
                wrapped: false,
                overline: false,
                semantic_type: Output,
                foreground: Default,
                background: Default,
                fat: None,
            },
        },
        Cluster {
            cell_width: 1,
            attrs: CellAttributes {
                attributes: 1,
                intensity: Bold,
                underline: None,
                blink: None,
                italic: false,
                reverse: false,
                strikethrough: false,
                invisible: false,
                wrapped: false,
                overline: false,
                semantic_type: Output,
                foreground: Default,
                background: Default,
                fat: None,
            },
        },
        Cluster {
            cell_width: 2,
            attrs: CellAttributes {
                attributes: 0,
                intensity: Normal,
                underline: None,
                blink: None,
                italic: false,
                reverse: false,
                strikethrough: false,
                invisible: false,
                wrapped: false,
                overline: false,
                semantic_type: Output,
                foreground: Default,
                background: Default,
                fat: None,
            },
        },
    ],
    len: 5,
    last_cell_width: Some(
        1,
    ),
}
"#
    );
}

#[test]
fn cluster_line_new() {
    let mut line = Line::new(1);
    line.set_cell(
        0,
        Cell::new_grapheme("h", CellAttributes::default(), None),
        1,
    );
    line.set_cell(
        1,
        Cell::new_grapheme("e", CellAttributes::default(), None),
        2,
    );
    line.set_cell(2, Cell::new_grapheme("l", bold(), None), 3);
    line.set_cell(
        3,
        Cell::new_grapheme("l", CellAttributes::default(), None),
        4,
    );
    line.set_cell(
        4,
        Cell::new_grapheme("o", CellAttributes::default(), None),
        5,
    );
    k9::snapshot!(
        line,
        r#"
Line {
    cells: C(
        ClusteredLine {
            text: "hello",
            is_double_wide: None,
            clusters: [
                Cluster {
                    cell_width: 2,
                    attrs: CellAttributes {
                        attributes: 0,
                        intensity: Normal,
                        underline: None,
                        blink: None,
                        italic: false,
                        reverse: false,
                        strikethrough: false,
                        invisible: false,
                        wrapped: false,
                        overline: false,
                        semantic_type: Output,
                        foreground: Default,
                        background: Default,
                        fat: None,
                    },
                },
                Cluster {
                    cell_width: 1,
                    attrs: CellAttributes {
                        attributes: 1,
                        intensity: Bold,
                        underline: None,
                        blink: None,
                        italic: false,
                        reverse: false,
                        strikethrough: false,
                        invisible: false,
                        wrapped: false,
                        overline: false,
                        semantic_type: Output,
                        foreground: Default,
                        background: Default,
                        fat: None,
                    },
                },
                Cluster {
                    cell_width: 2,
                    attrs: CellAttributes {
                        attributes: 0,
                        intensity: Normal,
                        underline: None,
                        blink: None,
                        italic: false,
                        reverse: false,
                        strikethrough: false,
                        invisible: false,
                        wrapped: false,
                        overline: false,
                        semantic_type: Output,
                        foreground: Default,
                        background: Default,
                        fat: None,
                    },
                },
            ],
            len: 5,
            last_cell_width: Some(
                1,
            ),
        },
    ),
    zones: [],
    seqno: 5,
    bits: LineBits(
        0x0,
    ),
    appdata: Mutex {
        data: None,
        poisoned: false,
        ..
    },
}
"#
    );
}

fn check_ascii_against_scalar(base: &Line, start: usize, text: &str, attrs: &CellAttributes) {
    let mut expected = base.clone();
    for offset in 0..text.len() {
        expected.set_cell_grapheme(
            start + offset,
            &text[offset..offset + 1],
            1,
            attrs.clone(),
            42,
        );
    }
    let mut actual = base.clone();
    assert!(actual.set_ascii_cells(start, text, attrs, 42));
    assert_eq!(actual, expected, "start={start}, text={text:?}");
    assert_eq!(
        actual.semantic_zone_ranges(),
        expected.semantic_zone_ranges()
    );
    assert_eq!(actual.compute_shape_hash(), expected.compute_shape_hash());
}

#[test]
fn ascii_runs_match_scalar_storage_links_and_wide_boundaries() {
    let mut linked = CellAttributes::default();
    linked.set_hyperlink(Some(Arc::new(Hyperlink::new("https://example.org"))));
    linked.set_semantic_type(wezterm_cell::SemanticType::Prompt);
    for text in [
        "",
        "abc   ",
        "a界b🙂c",
        "界界",
        "e\u{301}x",
        "https://example.com",
    ] {
        for compressed in [false, true] {
            let mut base: Line = text.into();
            base.scan_and_create_hyperlinks(&[Rule::new(r"https://\S+", "$0").unwrap()]);
            if compressed {
                base.compress_for_scrollback();
            } else {
                base.coerce_vec_storage();
            }
            base.semantic_zone_ranges();
            for start in 0..base.len() + 3 {
                for replacement in [
                    "",
                    "x",
                    "   ",
                    " ab ~",
                    "0123456789abcdefghijklmnopqrstuvwxyz",
                ] {
                    for attrs in [CellAttributes::default(), linked.clone()] {
                        check_ascii_against_scalar(&base, start, replacement, &attrs);
                    }
                }
            }
        }
    }
}

#[test]
fn ascii_append_matches_scalar_for_styled_compact_rows() {
    let mut rgb = CellAttributes::default();
    rgb.set_background(
        wezterm_cell::color::ColorAttribute::TrueColorWithDefaultFallback(
            (0.2, 0.4, 0.6, 1.0).into(),
        ),
    );
    let mut linked = rgb.clone();
    linked.set_hyperlink(Some(Arc::new(Hyperlink::new("https://example.org"))));
    linked.set_semantic_type(wezterm_cell::SemanticType::Prompt);
    let styles = [CellAttributes::default(), rgb, linked];
    for prefix in ["", "hello", "界e\u{301}🙂"] {
        for initial in &styles {
            let mut base = Line::from_text(prefix, initial, 7, None);
            base.compress_for_scrollback();
            base.semantic_zone_ranges();
            for attrs in &styles {
                for text in [" ", "abc", "  styled text ~  "] {
                    check_ascii_against_scalar(&base, base.len(), text, attrs);
                }
            }
        }
    }
}

#[test]
fn ascii_append_keeps_compact_storage_and_splits_full_attribute_clusters() {
    let mut base = Line::new(0);
    let attrs = CellAttributes::default();
    for len in [65534, 65535, 65536, 131073] {
        let text = "x".repeat(len);
        check_ascii_against_scalar(&base, base.len(), &text, &attrs);
        assert!(base.set_ascii_cells(base.len(), &text, &attrs, 42));
        assert!(base.is_compressed_for_scrollback());
    }
}

#[test]
fn invalid_ascii_input_is_rejected_without_mutating_the_line() {
    for text in ["a\nb", "a\0b", "a\x7fb", "a界", "e\u{301}"] {
        let mut line: Line = "original".into();
        let original = line.clone();
        assert!(!line.set_ascii_cells(3, text, &CellAttributes::default(), 42));
        assert_eq!(line, original);
    }
}

#[cfg(feature = "use_image")]
#[test]
fn ascii_overwrite_preserves_image_placements() {
    use wezterm_cell::image::{ImageCell, ImageData, ImageDataType, TextureCoordinate};
    let data = Arc::new(ImageData::with_data(ImageDataType::new_single_frame(
        1,
        1,
        vec![255; 4],
    )));
    let mut base: Line = "a界b cdef".into();
    for x in 0..base.len() {
        for placement in [None, Some(7)] {
            base.cells_mut()[x]
                .attrs_mut()
                .attach_image(Box::new(ImageCell::with_z_index(
                    TextureCoordinate::new_f32(0.0, 0.0),
                    TextureCoordinate::new_f32(1.0, 1.0),
                    Arc::clone(&data),
                    0,
                    0,
                    0,
                    0,
                    0,
                    Some(3),
                    placement,
                )));
        }
    }
    for start in 0..base.len() + 2 {
        check_ascii_against_scalar(&base, start, "0123456789", &CellAttributes::default());
    }
}

#[test]
fn blank_fill_to_the_end_matches_scalar_writes_then_prune() {
    // fill_range's Vec fast path: a blank fill from inside the row to at
    // least its end. Its result must equal per-column writes plus pruning.
    let mut styled = CellAttributes::default();
    styled.set_background(wezterm_cell::color::ColorAttribute::PaletteIndex(4));
    for text in ["hello world", "界🙂x", "      ", "ab  cd  "] {
        for attrs in [CellAttributes::default(), styled.clone()] {
            let mut base = Line::from_text(text, &attrs, 1, None);
            base.cells_mut();
            let len = base.len();
            for start in 0..len {
                for end in [len, len + 1, 20] {
                    let mut actual = base.clone();
                    actual.fill_range(start..end, &Cell::blank(), 7);
                    let mut expected = base.clone();
                    for x in start..end {
                        expected.set_cell(x, Cell::blank(), 7);
                    }
                    expected.prune_trailing_blanks(7);
                    assert_eq!(actual, expected, "text={text:?} {start}..{end}");
                }
            }
        }
    }
}

#[test]
fn grapheme_runs_match_one_grapheme_at_a_time() {
    let plain = CellAttributes::default();
    let bold = CellAttributes::default()
        .set_intensity(wezterm_cell::Intensity::Bold)
        .clone();
    let mut linked = CellAttributes::default();
    linked.set_hyperlink(Some(Arc::new(Hyperlink::new("https://example.org"))));
    let long_wide = vec![("界", 2); 60];
    let long_mixed: Vec<(&str, usize)> = (0..40)
        .map(|i| {
            if i % 3 == 0 {
                ("e\u{301}", 1)
            } else {
                ("中", 2)
            }
        })
        .collect();
    // A grapheme longer than a new row's text capacity.
    let marked = format!("e{}", "\u{301}".repeat(45));
    let runs: &[&[(&str, usize)]] = &[
        &[("界", 2)],
        &[("x", 1)],
        &[("中", 2), ("文", 2), ("字", 2)],
        &[
            ("a", 1),
            ("é", 1),
            ("界", 2),
            ("e\u{301}", 1),
            ("🙂", 2),
            ("\u{3000}", 2),
            ("\u{200b}", 1),
        ],
        &long_wide,
        &long_mixed,
        &[("界", 2), (&marked, 1), ("x", 1)],
    ];
    // Starting rows: empty, narrow or wide text in the same or other
    // attributes, one scanned without finding a link (still clustered) and
    // one with an implicit link (expanded to cells, so refused).
    let prefixes: &[&[(&str, usize)]] = &[
        &[],
        &[("a", 1), ("b", 1)],
        &[("界", 2), ("x", 1)],
        &[
            ("h", 1),
            ("t", 1),
            ("t", 1),
            ("p", 1),
            ("s", 1),
            (":", 1),
            ("/", 1),
            ("/", 1),
            ("e", 1),
            (".", 1),
            ("c", 1),
            ("o", 1),
        ],
    ];
    let rules = [Rule::new(r"https://\S+", "$0").unwrap()];
    for prefix in prefixes {
        for prefix_attrs in [&plain, &bold] {
            for (scan, compress) in [(false, false), (true, false), (false, true)] {
                let mut base = Line::new(1);
                let mut end = 0;
                for (g, w) in prefix.iter() {
                    base.set_cell_grapheme(end, g, *w, prefix_attrs.clone(), 2);
                    end += w;
                }
                if scan {
                    base.scan_and_create_hyperlinks(&rules);
                }
                if compress {
                    // Rebuilt compactly, with text capacity cut to its length.
                    base.cells_mut();
                    base.compress_for_scrollback();
                }
                // Text capacities: cut to the length (as cloning leaves it),
                // a few bytes spare, and a new row's.
                for spare in [0, 3, 80] {
                    let copy = |line: &Line| {
                        let mut line = line.clone();
                        if let crate::line::storage::CellStorage::C(cl) = &mut line.cells {
                            cl.text.reserve_exact(spare);
                        }
                        line
                    };
                    for attrs in [&plain, &bold, &linked] {
                        for run in runs {
                            for idx in [end, end + 1] {
                                let mut expected = copy(&base);
                                let mut x = idx;
                                for (g, w) in run.iter() {
                                    expected.set_cell_grapheme(x, g, *w, attrs.clone(), 5);
                                    x += w;
                                }
                                let text: String = run.iter().map(|(g, _)| *g).collect();
                                let cells = run.iter().map(|(_, w)| w).sum();
                                let mut wide = vec![];
                                let mut offset = 0;
                                for (_, w) in run.iter() {
                                    if *w > 1 {
                                        wide.push(offset as u16);
                                    }
                                    offset += w;
                                }
                                let last = run.last().unwrap().1 as u8;
                                let max_len = run.iter().map(|(g, _)| g.len()).max().unwrap();
                                let mut actual = copy(&base);
                                let (len, capacity) = match &actual.cells {
                                    crate::line::storage::CellStorage::C(cl) => {
                                        (cl.text.len(), cl.text.capacity())
                                    }
                                    _ => (0, 0),
                                };
                                let can = actual.can_append_grapheme_run(idx);
                                let appended = actual.append_grapheme_run(
                                    idx, &text, max_len, cells, &wide, last, attrs, 5,
                                );
                                // Refused where the text's growth would depend
                                // on each grapheme's length: one longer than the
                                // capacity, or a capacity under 4 bytes.
                                let predictable = len + text.len() <= capacity
                                    || if capacity == 0 {
                                        max_len <= 8
                                    } else {
                                        capacity >= 4 && max_len <= capacity
                                    };
                                assert_eq!(appended, can && predictable);
                                if appended {
                                    assert_eq!(actual, expected);
                                    assert_eq!(actual.as_str(), expected.as_str());
                                    // The same text capacity is left behind.
                                    match (&actual.cells, &expected.cells) {
                                        (
                                            crate::line::storage::CellStorage::C(a),
                                            crate::line::storage::CellStorage::C(e),
                                        ) => {
                                            assert_eq!(a.text.capacity(), e.text.capacity());
                                        }
                                        _ => panic!("a run is only taken by clustered storage"),
                                    }
                                } else {
                                    assert_eq!(actual, base);
                                }
                                // Only a row ending at `idx` in clustered storage takes a run.
                                assert_eq!(
                                    can,
                                    idx == end
                                        && matches!(
                                            base.cells,
                                            crate::line::storage::CellStorage::C(_)
                                        )
                                );
                            }
                        }
                    }
                }
            }
        }
    }
}
