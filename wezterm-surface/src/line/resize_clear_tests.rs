use super::*;
use crate::hyperlink::Hyperlink;
use alloc::sync::Arc;
use k9::assert_equal as assert_eq;

fn clear_rgb_attrs(red: f32) -> CellAttributes {
    use wezterm_cell::color::ColorAttribute;
    let mut attrs = CellAttributes::default();
    attrs.set_background(ColorAttribute::TrueColorWithDefaultFallback(
        (red, 0.25, 0.5, 1.0).into(),
    ));
    attrs.set_underline_color(ColorAttribute::TrueColorWithDefaultFallback(
        (0.2, red, 0.7, 1.0).into(),
    ));
    attrs
}

#[test]
fn resize_and_clear_produces_blank_rows_across_storage_styles_and_sizes() {
    let mut linked = clear_rgb_attrs(0.3);
    linked.set_hyperlink(Some(Arc::new(Hyperlink::new("https://example.org"))));
    linked.set_semantic_type(wezterm_cell::SemanticType::Prompt);
    let mut palette = CellAttributes::default();
    palette.set_background(wezterm_cell::color::ColorAttribute::PaletteIndex(4));
    let mut foreground = CellAttributes::default();
    foreground.set_foreground(
        wezterm_cell::color::ColorAttribute::TrueColorWithDefaultFallback(
            (0.3, 0.6, 0.9, 1.0).into(),
        ),
    );
    let styles = [
        linked,
        clear_rgb_attrs(0.1),
        clear_rgb_attrs(0.8),
        palette,
        foreground,
        CellAttributes::default(),
    ];
    let long_grapheme = format!("a{}", "\u{301}".repeat(256));
    for text in ["", "abc   ", "a界🙂b", "👩🏿‍🤝‍👩🏿e\u{301}", &long_grapheme] {
        for vector in [false, true] {
            for initial_attrs in &styles {
                for width in [0, 1, 2, 12, 116] {
                    let mut actual = Line::from_text(text, initial_attrs, 7, None);
                    if !vector {
                        actual.compress_for_scrollback();
                        assert!(matches!(actual.cells, CellStorage::C(_)));
                    } else {
                        assert!(matches!(actual.cells, CellStorage::V(_)));
                    }
                    actual.set_double_height_top(7);
                    actual.set_bidi_info(
                        true,
                        wezterm_bidi::ParagraphDirectionHint::RightToLeft,
                        7,
                    );
                    actual.semantic_zone_ranges();
                    #[cfg(feature = "appdata")]
                    let cached = {
                        let data = Arc::new(123u32);
                        actual.set_appdata(Arc::clone(&data));
                        actual.get_appdata().unwrap()
                    };
                    let mut expected_seqno = 7;
                    // Equal-size clears, growth, shrinkage and zero-width clears
                    // also cover the multi-line paste-prediction call site.
                    for next_width in [width, width, width + 3, 1, 0, width] {
                        for (n, attrs) in styles.iter().enumerate() {
                            if !vector {
                                actual.compress_for_scrollback();
                                assert!(matches!(actual.cells, CellStorage::C(_)));
                            }
                            let seqno = [3, 9, 8, 10, 10, 3][n];
                            expected_seqno = expected_seqno.max(seqno);
                            let mut expected = Line::with_width_and_cell(
                                next_width,
                                Cell::blank_with_attrs(attrs.clone()),
                                expected_seqno,
                            );
                            actual.resize_and_clear(next_width, seqno, attrs.clone());
                            assert_eq!(actual, expected);
                            assert_eq!(actual.len(), next_width);
                            assert_eq!(actual.compute_shape_hash(), expected.compute_shape_hash());
                            assert_eq!(actual.zones, expected.zones);
                            assert_eq!(
                                actual.semantic_zone_ranges(),
                                expected.semantic_zone_ranges()
                            );
                            assert_eq!(
                                actual.coerce_vec_storage().capacity(),
                                expected.coerce_vec_storage().capacity()
                            );
                            #[cfg(feature = "appdata")]
                            assert!(Arc::ptr_eq(&actual.get_appdata().unwrap(), &cached));
                        }
                    }
                }
            }
        }
    }
}

#[cfg(feature = "use_image")]
#[test]
fn resize_and_clear_releases_replaced_images_and_links() {
    use wezterm_cell::image::{ImageCell, ImageData, ImageDataType, TextureCoordinate};
    let image = Arc::new(ImageData::with_data(ImageDataType::new_single_frame(
        1,
        1,
        vec![255; 4],
    )));
    let link = Arc::new(Hyperlink::new("https://example.org"));
    for vector in [false, true] {
        for width in [0, 1, 4, 12] {
            for blank in [CellAttributes::default(), clear_rgb_attrs(0.8)] {
                let mut attrs = clear_rgb_attrs(0.2);
                attrs.set_hyperlink(Some(Arc::clone(&link)));
                attrs.attach_image(Box::new(ImageCell::with_z_index(
                    TextureCoordinate::new_f32(0.0, 0.0),
                    TextureCoordinate::new_f32(1.0, 1.0),
                    Arc::clone(&image),
                    0,
                    0,
                    0,
                    0,
                    0,
                    Some(3),
                    Some(7),
                )));
                let mut actual = Line::from_text("abcd", &attrs, 1, None);
                drop(attrs);
                if !vector {
                    actual.compress_for_scrollback();
                    assert!(matches!(actual.cells, CellStorage::C(_)));
                } else {
                    assert!(matches!(actual.cells, CellStorage::V(_)));
                }
                let expected =
                    Line::with_width_and_cell(width, Cell::blank_with_attrs(blank.clone()), 2);
                actual.resize_and_clear(width, 2, blank);
                assert_eq!(actual, expected);
                assert_eq!(Arc::strong_count(&image), 1);
                assert_eq!(Arc::strong_count(&link), 1);
            }
        }
    }
}
