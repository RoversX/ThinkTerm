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

/// How far up this view may go, including screen rows hidden by a shorter
/// client viewport as well as the server's scrollback.
pub fn max_scroll(dims: &RenderableDimensions, rows: usize) -> usize {
    dims.physical_top
        .saturating_add(dims.viewport_rows as StableRowIndex)
        .saturating_sub(rows as StableRowIndex)
        .saturating_sub(dims.scrollback_top)
        .max(0) as usize
}

/// The rows a fractionally scrolled view draws: `visible_rows` with one
/// more row at the bottom, to fill what shifting the rows up by `px`
/// leaves empty -- but never past the newest row the server has.
pub fn visible_rows_px(
    dims: &RenderableDimensions,
    rows: usize,
    scroll_from_bottom: usize,
    px: f32,
) -> Range<StableRowIndex> {
    let range = visible_rows(dims, rows, scroll_from_bottom);
    let bottom = dims.physical_top + dims.viewport_rows as StableRowIndex;
    if px > 0.0 && range.end < bottom {
        range.start..range.end + 1
    } else {
        range
    }
}

/// Where a scroll of `delta_px` device pixels lands, keeping the part of a
/// row the view is through.
///
/// `scroll_from_bottom` counts whole rows up into the scrollback and `px`,
/// in `[0, cell_h)`, is how far the top visible row is cut off at its top:
/// the view sits `px` pixels *below* the row `scroll_from_bottom` names, so
/// it is `scroll_from_bottom - px / cell_h` rows above the tail. A positive
/// `delta_px` scrolls down, towards the newest row, as a wheel pushed away
/// from the hand does.
///
/// The floor keeps the two directions symmetric: the same distance back and
/// forth returns to the same pixel. Both ends clamp onto a whole row -- the
/// tail cannot show half a row that has not been written yet, and neither
/// can the oldest row the server kept.
pub fn normalize_scroll_px(
    scroll_from_bottom: usize,
    px: f32,
    delta_px: f32,
    cell_h: f32,
    max: usize,
) -> (usize, f32) {
    if !(cell_h > 0.0) || !px.is_finite() || !delta_px.is_finite() {
        return (scroll_from_bottom.min(max), 0.0);
    }
    let total = px + delta_px;
    let mut rows = (total / cell_h).floor();
    let mut px = total - rows * cell_h;
    // The division rounds; a remainder that landed outside the row it
    // belongs to is on a boundary, so put it on the boundary rather than
    // carrying a negative offset or a whole extra row of one.
    if px >= cell_h {
        rows += 1.0;
        px = 0.0;
    } else if px < 0.0 {
        px = 0.0;
    }
    // A delta wider than the whole scrollback can still only reach an end,
    // and `as isize` on a large float saturates rather than wrapping;
    // holding the row count inside the reachable range says the same thing
    // in numbers the arithmetic below can carry.
    let reach = (max as f32) + (scroll_from_bottom as f32) + 1.0;
    let rows = rows.clamp(-reach, reach) as isize;
    let scroll = scroll_from_bottom as isize - rows;
    if scroll <= 0 {
        // The tail: there is nothing below the last row to show.
        return (0, 0.0);
    }
    let scroll = scroll as usize;
    if scroll > max {
        // The oldest row the server kept, whole.
        return (max, 0.0);
    }
    (scroll, px)
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
        assert_eq!(max_scroll(&dims(100, 95, 24), 24), 5);
    }

    #[test]
    fn a_shorter_page_still_shows_the_bottom_of_the_screen() {
        assert_eq!(visible_rows(&dims(100, 0, 24), 10, 0), 114..124);
    }

    #[test]
    fn a_shorter_view_can_reach_hidden_screen_rows_without_server_scrollback() {
        let server = dims(0, 0, 25);
        assert_eq!(visible_rows(&server, 22, 0), 3..25);
        assert_eq!(max_scroll(&server, 22), 3);
        assert_eq!(visible_rows(&server, 22, max_scroll(&server, 22)), 0..22);
        assert_eq!(max_scroll(&server, 30), 0);
        assert_eq!(max_scroll(&dims(100, 0, 24), 10), 114);
        assert_eq!(max_scroll(&dims(100, 95, 24), 30), 0);
    }

    #[test]
    fn a_fractional_view_draws_one_more_row_but_never_past_the_newest() {
        let d = dims(100, 0, 24);
        assert_eq!(visible_rows_px(&d, 24, 10, 0.0), 90..114, "a whole row draws what it always did");
        assert_eq!(visible_rows_px(&d, 24, 10, 4.0), 90..115);
        // At the tail there is no row below to borrow, and the convention
        // forbids a fraction there anyway.
        assert_eq!(visible_rows_px(&d, 24, 0, 4.0), 100..124);
        // Clamped at the oldest row, the extra one is still inside the screen.
        let d = dims(100, 95, 24);
        let range = visible_rows_px(&d, 24, 10, 4.0);
        assert_eq!(range, 95..120);
        assert!(range.end <= d.physical_top + d.viewport_rows as StableRowIndex);
    }
}

#[cfg(test)]
mod scroll_px_tests {
    use super::*;

    const CELL: f32 = 20.0;

    #[test]
    fn a_delta_short_of_a_row_moves_only_the_offset() {
        // Up (negative) from the tail: a quarter of a row back.
        assert_eq!(normalize_scroll_px(0, 0.0, -5.0, CELL, 100), (1, 15.0));
        // And down again lands exactly where it started.
        assert_eq!(normalize_scroll_px(1, 15.0, 5.0, CELL, 100), (0, 0.0));
    }

    #[test]
    fn whole_rows_leave_no_offset_behind() {
        assert_eq!(normalize_scroll_px(0, 0.0, -CELL, CELL, 100), (1, 0.0));
        assert_eq!(normalize_scroll_px(3, 0.0, -3.0 * CELL, CELL, 100), (6, 0.0));
        assert_eq!(normalize_scroll_px(6, 0.0, 2.0 * CELL, CELL, 100), (4, 0.0));
        // A whole row on top of an offset keeps the offset.
        assert_eq!(normalize_scroll_px(4, 7.0, -CELL, CELL, 100), (5, 7.0));
    }

    #[test]
    fn the_same_distance_back_and_forth_returns_to_the_same_pixel() {
        for delta in [1.0f32, 3.5, 19.0, 20.0, 47.25, 137.0] {
            let start = (7usize, 11.0f32);
            let up = normalize_scroll_px(start.0, start.1, -delta, CELL, 100);
            let back = normalize_scroll_px(up.0, up.1, delta, CELL, 100);
            assert_eq!(back, start, "{delta} px up and down again");
        }
    }

    #[test]
    fn the_offset_stays_inside_one_row() {
        let mut at = (10usize, 0.0f32);
        for step in 0..200 {
            let delta = if step % 3 == 0 { 7.5 } else { -3.25 };
            at = normalize_scroll_px(at.0, at.1, delta, CELL, 40);
            assert!((0.0..CELL).contains(&at.1), "{at:?} after {delta}");
            assert!(at.0 <= 40, "{at:?}");
            assert!(at.0 > 0 || at.1 == 0.0, "the tail is always a whole row: {at:?}");
        }
    }

    #[test]
    fn both_ends_land_on_a_whole_row() {
        // Past the tail.
        assert_eq!(normalize_scroll_px(1, 4.0, 5.0 * CELL, CELL, 100), (0, 0.0));
        assert_eq!(normalize_scroll_px(0, 0.0, 3.0, CELL, 100), (0, 0.0));
        // Past the oldest row the server kept.
        assert_eq!(normalize_scroll_px(9, 4.0, -5.0 * CELL, CELL, 10), (10, 0.0));
        // Stopping exactly on it keeps the offset: the oldest row may be cut.
        assert_eq!(normalize_scroll_px(8, 0.0, -CELL - 6.0, CELL, 10), (10, 14.0));
        // Nothing to scroll at all.
        assert_eq!(normalize_scroll_px(0, 0.0, -100.0, CELL, 0), (0, 0.0));
    }

    #[test]
    fn nonsense_input_falls_back_to_a_whole_row() {
        assert_eq!(normalize_scroll_px(5, 3.0, -10.0, 0.0, 100), (5, 0.0));
        assert_eq!(normalize_scroll_px(5, 3.0, f32::NAN, CELL, 100), (5, 0.0));
        assert_eq!(normalize_scroll_px(5, f32::INFINITY, -1.0, CELL, 100), (5, 0.0));
        assert_eq!(normalize_scroll_px(5, 3.0, -1e30, CELL, 10), (10, 0.0));
        assert_eq!(normalize_scroll_px(5, 3.0, 1e30, CELL, 10), (0, 0.0));
        assert_eq!(normalize_scroll_px(50, 0.0, 0.0, CELL, 10), (10, 0.0), "a stale row is clamped");
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
