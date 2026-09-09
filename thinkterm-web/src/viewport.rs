//! Where the browser is looking in the pane, and the cell under a pixel.

use std::ops::Range;
use thinkterm_proto::RenderableDimensions;
use wezterm_term::StableRowIndex;

/// The font size (points) this page should use, chosen from the sizes a
/// quarter point apart between 6 and 72 by measuring each one's cell
/// (`cell`, width and height in device px): the one whose cell is nearest
/// the desktop's (`desktop_cell_h`, device px) so the desktop's pixel
/// geometry maps onto the page one to one -- or the current size when no
/// desktop size is known -- then stepped down until the whole tab
/// (`grid`, with the desktop's padding of a cell each side and half a
/// cell above and below) fits `avail`. A mirror that clips is no mirror.
/// Decided from measurements, not by nudging the current size, so it
/// cannot flap. `None` when the current size is the answer.
pub fn choose_size_pt(
    cell: impl Fn(f64) -> Option<(f64, f64)>,
    current: f64,
    desktop_cell_h: Option<f64>,
    grid: Option<(usize, usize)>,
    avail: (f64, f64),
) -> Option<f64> {
    let steps = || (24..=288).map(|q| q as f64 / 4.0);
    let mut pt = match desktop_cell_h {
        Some(want) if want > 0.0 => steps()
            .filter_map(|pt| cell(pt).map(|(_, h)| (pt, (h - want).abs())))
            .min_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
            .map(|(pt, _)| pt)
            .unwrap_or(current),
        _ => (current * 4.0).round() / 4.0,
    };
    if let Some((cols, rows)) = grid.filter(|(c, r)| *c > 0 && *r > 0) {
        let fits = |pt: f64| {
            cell(pt).is_none_or(|(w, h)| (cols + 2) as f64 * w <= avail.0 && (rows + 1) as f64 * h <= avail.1)
        };
        while pt > 6.0 && !fits(pt) {
            pt -= 0.25;
        }
    }
    let pt = pt.clamp(6.0, 72.0);
    ((pt - current).abs() >= 0.125).then_some(pt)
}

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

#[cfg(test)]
mod font_tests {
    use super::*;

    // A font whose cell grows with the size: 0.6 x 1.2 px per point.
    fn cell(pt: f64) -> Option<(f64, f64)> {
        Some(((pt * 0.6).round(), (pt * 1.2).round()))
    }

    #[test]
    fn the_page_takes_the_desktop_s_cell_size_when_it_fits() {
        // The desktop's cell is 34 px tall: 28 pt is the first size whose cell rounds to 34.
        assert_eq!(choose_size_pt(cell, 12.0, Some(34.0), Some((80, 24)), (4000.0, 4000.0)), Some(28.0));
        assert_eq!(choose_size_pt(cell, 28.0, Some(34.0), Some((80, 24)), (4000.0, 4000.0)), None, "already there");
    }

    #[test]
    fn the_tab_must_fit_the_window_with_its_padding() {
        // 82 cells wide at 17 px would need 1394 px; the window has 1000.
        let pt = choose_size_pt(cell, 12.0, Some(34.0), Some((80, 24)), (1000.0, 4000.0)).unwrap();
        assert!(pt < 28.0);
        let (w, _) = cell(pt).unwrap();
        assert!(82.0 * w <= 1000.0, "{pt} pt: {}", 82.0 * w);
        let (w2, _) = cell(pt + 0.25).unwrap();
        assert!(82.0 * w2 > 1000.0, "the next size up would not fit");
        assert_eq!(choose_size_pt(cell, 6.0, Some(34.0), Some((800, 24)), (100.0, 100.0)), None, "never below 6");
    }

    #[test]
    fn without_a_desktop_size_the_current_one_stays_unless_it_does_not_fit() {
        assert_eq!(choose_size_pt(cell, 12.0, None, Some((80, 24)), (4000.0, 4000.0)), None);
        assert_eq!(choose_size_pt(cell, 12.0, Some(0.0), None, (10.0, 10.0)), None, "no grid to fit");
        assert!(choose_size_pt(cell, 12.0, None, Some((80, 24)), (300.0, 4000.0)).unwrap() < 12.0);
    }
}
