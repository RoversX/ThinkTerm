//! Where the browser is looking in the pane, and the cell under a pixel.

use std::ops::Range;
use thinkterm_proto::RenderableDimensions;
use wezterm_term::StableRowIndex;

/// Rows the page shows: the last `rows` of the screen, moved up into the
/// scrollback by `scroll_from_bottom` lines.
pub fn visible_rows(
    dims: &RenderableDimensions,
    rows: usize,
    scroll_from_bottom: usize,
) -> Range<StableRowIndex> {
    let bottom = dims.physical_top + dims.viewport_rows as StableRowIndex;
    let lowest_top = dims.scrollback_top;
    let top = (bottom - rows as StableRowIndex - scroll_from_bottom as StableRowIndex).max(lowest_top);
    top..top + rows as StableRowIndex
}

/// How far up the view may go: everything above the screen that the
/// server remembers.
pub fn max_scroll(dims: &RenderableDimensions) -> usize {
    (dims.physical_top - dims.scrollback_top).max(0) as usize
}

/// The cell under a canvas pixel (device pixels, origin top-left).
pub fn cell_at(px: f64, py: f64, cell_width: f64, cell_height: f64) -> (usize, usize) {
    let col = (px / cell_width).floor().max(0.0) as usize;
    let row = (py / cell_height).floor().max(0.0) as usize;
    (col, row)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dims(physical_top: StableRowIndex, scrollback_top: StableRowIndex, rows: usize) -> RenderableDimensions {
        RenderableDimensions {
            cols: 80,
            viewport_rows: rows,
            scrollback_rows: (physical_top - scrollback_top) as usize + rows,
            physical_top,
            scrollback_top,
            dpi: 96,
            pixel_width: 0,
            pixel_height: 0,
            reverse_video: false,
        }
    }

    #[test]
    fn following_the_tail_shows_the_screen() {
        assert_eq!(visible_rows(&dims(100, 0, 24), 24, 0), 100..124);
    }

    #[test]
    fn scrolling_moves_up_and_stops_at_the_oldest_row() {
        assert_eq!(visible_rows(&dims(100, 0, 24), 24, 10), 90..114);
        assert_eq!(visible_rows(&dims(100, 95, 24), 24, 10), 95..119);
        assert_eq!(max_scroll(&dims(100, 95, 24)), 5);
    }

    #[test]
    fn a_shorter_page_still_shows_the_bottom_of_the_screen() {
        assert_eq!(visible_rows(&dims(100, 0, 24), 10, 0), 114..124);
    }
}
