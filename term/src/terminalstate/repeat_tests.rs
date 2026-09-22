//! Differential coverage against the scalar REP implementation.
use super::*;

#[derive(Debug)]
struct TestConfig {
    scrollback: usize,
}
impl TerminalConfiguration for TestConfig {
    fn scrollback_size(&self) -> usize {
        self.scrollback
    }
    fn color_palette(&self) -> ColorPalette {
        ColorPalette::default()
    }
}

impl TerminalState {
    // Reference from before row batching. Keep the per-cell loop independent
    // of set_cell_range so a regression cannot change both sides of the test.
    fn scalar_repeat_for_test(&mut self, n: u32) {
        let seqno = self.seqno;

        let mut y = self.cursor.y;
        let mut x = self.cursor.x;
        let left_and_right_margins = self.left_and_right_margins.clone();
        let top_and_bottom_margins = self.top_and_bottom_margins.clone();

        // Resolve the source cell.  It may be a double-wide character.
        let cell = {
            let screen = self.screen_mut();
            let to_copy = x.saturating_sub(1);
            let line_idx = screen.phys_row(y);
            let line = screen.line_mut(line_idx);

            match line.cells_mut().get(to_copy).cloned() {
                None => Cell::blank(),
                Some(candidate) => {
                    if candidate.str() == " " && to_copy > 0 {
                        // It's a blank.  It may be the second part of
                        // a double-wide pair; look ahead of it.
                        let prior = &line.cells_mut()[to_copy - 1];
                        if prior.width() > 1 {
                            prior.clone()
                        } else {
                            candidate
                        }
                    } else {
                        candidate
                    }
                }
            }
        };

        // Repeats wrap and scroll, so unlike ICH the screen keeps
        // changing -- but only until the viewport and the scrollback
        // hold nothing but the repeated cell. Past that only the cursor
        // column still depends on n, and it cycles with the margin
        // width, so keeping the remainder leaves the cursor exactly
        // where it would have landed and drops the rest. Otherwise n
        // reaches u32::MAX and this loop runs for a minute.
        let width = (left_and_right_margins.end - left_and_right_margins.start).max(1);
        let saturated =
            width.saturating_mul(self.screen().physical_rows + self.config.scrollback_size());
        let n = n as usize;
        let n = if n > saturated {
            saturated + (n % width)
        } else {
            n
        };

        for _ in 0..n {
            {
                let screen = self.screen_mut();
                let line_idx = screen.phys_row(y);
                let line = screen.line_mut(line_idx);

                line.set_cell(x, cell.clone(), seqno);
            }
            x += 1;
            if x > left_and_right_margins.end - 1 {
                x = left_and_right_margins.start;
                if y == top_and_bottom_margins.end - 1 {
                    self.scroll_up(1);
                } else {
                    y += 1;
                    if y > top_and_bottom_margins.end - 1 {
                        y = top_and_bottom_margins.end;
                    }
                }
            }
        }
        self.cursor.x = x;
        self.cursor.y = y;
    }
}

#[test]
fn batched_repeat_matches_scalar_screen_cursor_and_scrollback() {
    for (rows, cols, scrollback) in [(1, 2, 0), (3, 7, 0), (3, 7, 4)] {
        for setup in [
            "abc",
            "a界",
            "e\u{301}",
            "🙂",
            "\x1b[44m ",
            "\x1b[?1049habc",
            "\x1b[?7labc",
            "\x1b[2;3r\x1b[3;6Hq",
            "\x1b[?69h\x1b[2;6s\x1b[3;6Hq",
            "\x1b]8;;https://example.com\x1b\\q",
        ] {
            for n in [0, 1, 2, 7, 8, 19, 50, u32::MAX] {
                let make = || {
                    let mut term = Terminal::new(
                        TerminalSize {
                            rows,
                            cols,
                            pixel_width: cols * 8,
                            pixel_height: rows * 16,
                            dpi: 96,
                        },
                        Arc::new(TestConfig { scrollback }),
                        "test",
                        "test",
                        Box::new(Vec::new()),
                    );
                    term.advance_bytes(setup);
                    term.increment_seqno();
                    term
                };
                let mut expected = make();
                expected.scalar_repeat_for_test(n);
                let mut actual = make();
                actual.perform_csi_edit(Edit::Repeat(n));
                assert_eq!(
                    actual.snapshot(),
                    expected.snapshot(),
                    "rows={rows}, cols={cols}, scrollback={scrollback}, setup={setup:?}, n={n}"
                );
            }
        }
    }
}
