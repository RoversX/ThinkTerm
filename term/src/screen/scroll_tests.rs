//! Compare recycled rows with the pre-optimization scrolling implementation.
use super::*;
use crate::color::{ColorAttribute, ColorPalette};
use wezterm_bidi::ParagraphDirectionHint;

#[derive(Debug)]
struct Config;
impl TerminalConfiguration for Config {
    fn scrollback_size(&self) -> usize {
        5
    }
    fn color_palette(&self) -> ColorPalette {
        ColorPalette::default()
    }
}

impl Screen {
    fn reference_scroll_up(
        &mut self,
        scroll_region: &Range<VisibleRowIndex>,
        num_rows: usize,
        seqno: SequenceNo,
        blank_attr: CellAttributes,
        bidi_mode: BidiMode,
    ) {
        let phys_scroll = self.phys_range(scroll_region);
        let num_rows = num_rows.min(phys_scroll.end - phys_scroll.start);
        let scrollback_ok = scroll_region.start == 0 && self.allow_scrollback;
        let insert_at_end = scroll_region.end as usize == self.physical_rows;

        debug!(
            "scroll_up {:?} num_rows={} phys_scroll={:?}",
            scroll_region, num_rows, phys_scroll
        );
        // Invalidate the lines that will move before they move so that
        // the indices of the lines are stable (we may remove lines below)
        // We only need invalidate if the StableRowIndex of the row would be
        // changed by the scroll operation.  For normal newline at the bottom
        // of the screen based scrolling, the StableRowIndex does not change,
        // so we use the scroll region bounds to gate the invalidation.
        if !scrollback_ok {
            for y in phys_scroll.clone() {
                self.line_mut(y).update_last_change_seqno(seqno);
            }
        }

        // if we're going to remove lines due to lack of scrollback capacity,
        // remember how many so that we can adjust our insertion point later.
        let lines_removed = if !scrollback_ok {
            // No scrollback available for these;
            // Remove the scrolled lines
            num_rows
        } else {
            let max_allowed = self.physical_rows + self.scrollback_size();
            if self.lines.len() + num_rows >= max_allowed {
                (self.lines.len() + num_rows) - max_allowed
            } else {
                0
            }
        };

        if scroll_region.start == 0 {
            for y in self.phys_range(&(0..num_rows as VisibleRowIndex)) {
                self.line_mut(y).compress_for_scrollback();
            }
        }

        let remove_idx = if scroll_region.start == 0 {
            0
        } else {
            phys_scroll.start
        };

        let default_blank = CellAttributes::blank();
        // To avoid thrashing the heap, prefer to move lines that were
        // scrolled off the top and re-use them at the bottom.
        let to_move = lines_removed.min(num_rows);
        let (to_remove, to_add) = {
            for _ in 0..to_move {
                let mut line = self.lines.remove(remove_idx).unwrap();
                let line = if default_blank == blank_attr {
                    Line::new(seqno)
                } else {
                    // Make the line like a new one of the appropriate width
                    line.resize_and_clear(self.physical_cols, seqno, blank_attr.clone());
                    line.update_last_change_seqno(seqno);
                    line
                };
                if insert_at_end {
                    self.lines.push_back(line);
                } else {
                    self.lines.insert(phys_scroll.end - 1, line);
                }
            }
            // We may still have some lines to add at the bottom, so
            // return revised counts for remove/add
            (lines_removed - to_move, num_rows - to_move)
        };

        // Perform the removal
        for _ in 0..to_remove {
            self.lines.remove(remove_idx);
        }

        if remove_idx == 0 && scrollback_ok {
            self.stable_row_index_offset += lines_removed;
        }

        for _ in 0..to_add {
            let mut line = if default_blank == blank_attr {
                Line::new(seqno)
            } else {
                Line::with_width_and_cell(
                    self.physical_cols,
                    Cell::blank_with_attrs(blank_attr.clone()),
                    seqno,
                )
            };
            bidi_mode.apply_to_line(&mut line, seqno);
            if insert_at_end {
                self.lines.push_back(line);
            } else {
                self.lines.insert(phys_scroll.end, line);
            }
        }

        // If we have invalidated the StableRowIndex, mark all subsequent lines as dirty
        if to_remove > 0 || (to_add > 0 && !insert_at_end) {
            for y in self.phys_range(&(scroll_region.end..self.physical_rows as VisibleRowIndex)) {
                self.line_mut(y).update_last_change_seqno(seqno);
            }
        }
    }
}

#[test]
fn recycled_scrolling_matches_original_rows_and_stable_indices() {
    let config: Arc<dyn TerminalConfiguration> = Arc::new(Config);
    for rows in [1, 3, 8] {
        for allow_scrollback in [false, true] {
            for bidi in [false, true] {
                let mode = BidiMode {
                    enabled: bidi,
                    hint: ParagraphDirectionHint::LeftToRight,
                };
                let size = TerminalSize {
                    rows,
                    cols: 12,
                    pixel_width: 120,
                    pixel_height: rows * 16,
                    dpi: 96,
                };
                let mut base = Screen::new(size, &config, allow_scrollback, 1, mode);
                // Exercise both occupied scrollback and a wrapped VecDeque.
                for _ in 0..rows + 9 {
                    base.line_mut(base.lines.len() - 1).set_cell_grapheme(
                        0,
                        "x",
                        1,
                        CellAttributes::default(),
                        2,
                    );
                    base.reference_scroll_up(
                        &(0..rows as i64),
                        1,
                        2,
                        CellAttributes::default(),
                        mode,
                    );
                }
                for row in 0..rows {
                    let idx = base.phys_row(row as i64);
                    let line = base.line_mut(idx);
                    let mut attrs = CellAttributes::default();
                    attrs.set_hyperlink(Some(Arc::new(termwiz::hyperlink::Hyperlink::new(
                        "https://example.com",
                    ))));
                    attrs.set_semantic_type(SemanticType::Prompt);
                    line.set_cell(
                        0,
                        Cell::new_grapheme(["界", "🙂", "e\u{301}"][row % 3], attrs, None),
                        3,
                    );
                    if row % 4 == 3 {
                        line.set_ascii_cells(
                            0,
                            &"x".repeat(size.cols),
                            &CellAttributes::default(),
                            3,
                        );
                    }
                    if row % 2 == 0 {
                        line.cells_mut();
                    }
                    line.set_double_width(3);
                    line.semantic_zone_ranges();
                }
                for start in 0..rows {
                    for end in start + 1..=rows {
                        for count in [0, 1, 2, rows, rows + 2] {
                            for styled in [false, true] {
                                let mut blank = CellAttributes::default();
                                if styled {
                                    blank.set_background(ColorAttribute::PaletteIndex(4));
                                }
                                let mut expected = base.clone();
                                let mut actual = base.clone();
                                let region = start as i64..end as i64;
                                for seqno in [4, 4, 5, 5] {
                                    expected.reference_scroll_up(
                                        &region,
                                        count,
                                        seqno,
                                        blank.clone(),
                                        mode,
                                    );
                                    actual.scroll_up(&region, count, seqno, blank.clone(), mode);
                                    assert_eq!(actual.lines, expected.lines, "rows={rows} region={region:?} count={count} styled={styled} scrollback={allow_scrollback} bidi={bidi}");
                                    assert_eq!(
                                        actual.stable_row_index_offset,
                                        expected.stable_row_index_offset
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}
