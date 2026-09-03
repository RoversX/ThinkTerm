pub(crate) fn rect(x: f32, y: f32, width: f32, height: f32) -> window::RectF {
    euclid::rect(x, y, width.max(0.0), height.max(0.0))
}

pub(crate) fn inset(rect: window::RectF, amount: f32) -> window::RectF {
    euclid::rect(
        rect.origin.x + amount,
        rect.origin.y + amount,
        (rect.size.width - amount * 2.0).max(0.0),
        (rect.size.height - amount * 2.0).max(0.0),
    )
}

pub(crate) fn contains(rect: window::RectF, x: f32, y: f32) -> bool {
    rect.contains(euclid::point2(x, y))
}

pub(crate) fn clamp(value: f32, min: f32, max: f32) -> f32 {
    value.max(min).min(max)
}

/// Layout of a grid of equal-sized cards: how many columns fit, how many rows
/// that makes, and the size of one card.
///
/// The whole grid is width-driven -- pick the columns from the available
/// width, and every other number falls out. That is what lets one page reflow
/// from one column to five without a breakpoint table.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct CardGrid {
    pub columns: usize,
    pub rows: usize,
    pub card_width: f32,
    pub card_height: f32,
}

impl CardGrid {
    pub(crate) const EMPTY: Self = Self {
        columns: 0,
        rows: 0,
        card_width: 0.0,
        card_height: 0.0,
    };

    /// Total height of the laid-out rows, gaps included.
    pub(crate) fn height(self, gap: f32) -> f32 {
        if self.rows == 0 {
            0.0
        } else {
            self.rows as f32 * self.card_height + self.rows.saturating_sub(1) as f32 * gap
        }
    }
}

/// How many cards of at least `minimum` width fit across `width`.
pub(crate) fn grid_columns(width: f32, minimum: f32, gap: f32, maximum: usize) -> usize {
    if maximum == 0 {
        return 0;
    }
    if maximum == 1 || width <= minimum {
        1
    } else {
        (((width + gap) / (minimum + gap)).floor() as usize).clamp(1, maximum)
    }
}

/// Groups whose last row would hold exactly one card. A lone card under a full
/// row reads as a mistake, so callers trade a column away to avoid several of
/// them at once.
pub(crate) fn single_orphan_group_count(counts: &[usize], columns: usize) -> usize {
    if columns == 0 {
        return 0;
    }
    counts
        .iter()
        .filter(|&&count| count > columns && count % columns == 1)
        .count()
}

/// One column count for every group on the page, so groups line up with each
/// other. Drops a column when doing so leaves fewer orphaned last rows and the
/// cards are narrow enough that the extra width is welcome anyway.
pub(crate) fn shared_grid_columns(
    counts: &[usize],
    content_width: f32,
    maximum_columns: usize,
    minimum: f32,
    orphan_comfort_width: f32,
    gap: f32,
) -> usize {
    // Never lay out more columns than the fullest group can fill. A column
    // nothing is ever placed in is not a layout, it is width taken away from
    // the cards that do exist -- which is how three hosts on a wide page end
    // up squeezed to the minimum width with their names truncated and half
    // the page empty beside them.
    let fullest = counts.iter().copied().max().unwrap_or(0).max(1);
    let mut columns = grid_columns(content_width, minimum, gap, maximum_columns).min(fullest);
    if columns == 0 {
        return 0;
    }
    let initial_available =
        (content_width - gap * columns.saturating_sub(1) as f32) / columns as f32;
    if columns > 2 && initial_available < orphan_comfort_width {
        let reduced_columns = columns - 1;
        if single_orphan_group_count(counts, reduced_columns)
            < single_orphan_group_count(counts, columns)
        {
            columns = reduced_columns;
        }
    }
    columns
}

/// Width of one card once `columns` of them share `content_width`, capped so a
/// nearly-empty page does not stretch two cards across a 5K display.
pub(crate) fn grid_card_width(
    content_width: f32,
    columns: usize,
    maximum: f32,
    gap: f32,
) -> f32 {
    if columns == 0 {
        return 0.0;
    }
    let available = (content_width - gap * columns.saturating_sub(1) as f32) / columns as f32;
    available.min(maximum).max(1.0)
}

/// A grid of cards whose height the caller already knows. (Cards whose height
/// follows from their width -- a terminal preview, say -- compute it from
/// [`grid_card_width`] instead.)
pub(crate) fn card_grid(
    count: usize,
    content_width: f32,
    columns: usize,
    maximum_card_width: f32,
    gap: f32,
    card_height: f32,
) -> CardGrid {
    if columns == 0 {
        return CardGrid::EMPTY;
    }
    CardGrid {
        columns,
        rows: (count + columns - 1) / columns,
        card_width: grid_card_width(content_width, columns, maximum_card_width, gap),
        card_height,
    }
}

/// Where a row of cards begins when it does not fill the content width.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RowAlign {
    /// Pin to the left margin. What a list wants: the reader's eye returns to
    /// the same column on every row, and a page with three hosts on it still
    /// starts where a page with thirty does.
    Start,
    /// Centre the row. What a gallery of big tiles wants, where a short last
    /// row hanging off one side reads as a mistake.
    Center,
    /// Pin to both margins: cards keep their width and the width left over
    /// once they hit their cap is shared out between them, so a full row
    /// always reaches the right edge.
    ///
    /// Only full rows are stretched. Spreading a two-card last row across the
    /// whole page -- which is what `space-between` does elsewhere -- puts a
    /// hole in the middle of the grid; the short row keeps the base gap and
    /// starts at the left margin, under the row above it.
    Justify,
}

/// Where card `index` of `count` sits, in the same space `cards_y` is in.
pub(crate) fn card_rect(
    index: usize,
    count: usize,
    grid: CardGrid,
    content_x: f32,
    content_width: f32,
    cards_y: f32,
    gap: f32,
    align: RowAlign,
) -> window::RectF {
    if grid.columns == 0 {
        return rect(content_x, cards_y, 0.0, 0.0);
    }
    let row = index / grid.columns;
    let column = index % grid.columns;
    let row_start = row * grid.columns;
    let row_count = (count - row_start).min(grid.columns);
    let (row_x, column_gap) = match align {
        RowAlign::Start => (content_x, gap),
        RowAlign::Center => {
            let row_width =
                row_count as f32 * grid.card_width + row_count.saturating_sub(1) as f32 * gap;
            (content_x + (content_width - row_width).max(0.0) / 2.0, gap)
        }
        RowAlign::Justify if row_count == grid.columns && grid.columns > 1 => {
            let used = grid.columns as f32 * grid.card_width;
            let spread = (content_width - used) / (grid.columns - 1) as f32;
            // Absorb a modest remainder only. When the cards are capped well
            // below the page -- three of them on a wide window -- sharing out
            // what is left would put half a card's width between neighbours,
            // which reads as a broken layout rather than a filled row. Past
            // double the base gap, leave the slack on the right instead.
            let spread = if spread <= gap * 2.0 {
                spread.max(gap)
            } else {
                gap
            };
            (content_x, spread)
        }
        RowAlign::Justify => (content_x, gap),
    };
    euclid::rect(
        row_x + column as f32 * (grid.card_width + column_gap),
        // Rows always step by the base gap: only the horizontal run has
        // leftover width to absorb.
        cards_y + row as f32 * (grid.card_height + gap),
        grid.card_width,
        grid.card_height,
    )
}

/// Any part of the band `[y, y + height)` inside the viewport.
pub(crate) fn row_visible(y: f32, height: f32, viewport: window::RectF) -> bool {
    y + height > viewport.min_y() && y < viewport.max_y()
}

/// The whole band inside the viewport.
pub(crate) fn row_fully_visible(y: f32, height: f32, viewport: window::RectF) -> bool {
    y >= viewport.min_y() && y + height <= viewport.max_y()
}

/// Visible, or close enough that it is worth keeping ready: one row of
/// overscan stops a scroll from revealing empty cards.
pub(crate) fn card_is_warm(rect: window::RectF, viewport: window::RectF, overscan: f32) -> bool {
    let warm_top = viewport.min_y() - overscan.max(0.0);
    let warm_bottom = viewport.max_y() + overscan.max(0.0);
    rect.max_y() > warm_top && rect.min_y() < warm_bottom
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole point of the grid: more width means more columns, not wider
    /// cards past their cap and not a hole in the middle of the page.
    #[test]
    fn columns_follow_the_available_width() {
        let (min, gap) = (260.0, 14.0);
        assert_eq!(grid_columns(300.0, min, gap, 5), 1);
        assert_eq!(grid_columns(560.0, min, gap, 5), 2);
        assert_eq!(grid_columns(840.0, min, gap, 5), 3);
        // Never past the cap, however wide the window gets.
        assert_eq!(grid_columns(4000.0, min, gap, 5), 5);
        // A window narrower than one card still shows one.
        assert_eq!(grid_columns(10.0, min, gap, 5), 1);
    }

    /// Three cards in a page wide enough for five get three wide columns,
    /// not five narrow ones with two of them empty.
    #[test]
    fn columns_never_exceed_the_fullest_group() {
        let wide = 2000.0;
        assert_eq!(grid_columns(wide, 340.0, 18.0, 5), 5);
        assert_eq!(shared_grid_columns(&[3], wide, 5, 340.0, 400.0, 18.0), 3);
        // Groups still share one count, taken from the fullest of them, so
        // the two grids stay aligned with each other.
        assert_eq!(shared_grid_columns(&[3, 4], wide, 5, 340.0, 400.0, 18.0), 4);
        // Nothing to place: one column, and no panic on an empty page.
        assert_eq!(shared_grid_columns(&[], wide, 5, 340.0, 400.0, 18.0), 1);
        assert_eq!(shared_grid_columns(&[0, 0], wide, 5, 340.0, 400.0, 18.0), 1);
    }

    #[test]
    fn card_width_is_capped_but_never_negative() {
        assert_eq!(grid_card_width(800.0, 2, 380.0, 20.0), 380.0);
        assert_eq!(grid_card_width(500.0, 2, 380.0, 20.0), 240.0);
        assert_eq!(grid_card_width(0.0, 3, 380.0, 20.0), 1.0);
        assert_eq!(grid_card_width(800.0, 0, 380.0, 20.0), 0.0);
    }

    #[test]
    fn rows_round_up_and_measure_with_their_gaps() {
        let grid = card_grid(5, 800.0, 2, 380.0, 20.0, 80.0);
        assert_eq!(grid.columns, 2);
        assert_eq!(grid.rows, 3);
        assert_eq!(grid.height(20.0), 3.0 * 80.0 + 2.0 * 20.0);
        assert_eq!(card_grid(0, 800.0, 2, 380.0, 20.0, 80.0).rows, 0);
        assert_eq!(CardGrid::EMPTY.height(20.0), 0.0);
    }

    /// Every row is centred on the content width, so a last row that cannot
    /// fill it does not hang off to one side. (The full rows are centred too,
    /// which only shows once the cards hit their width cap.)
    #[test]
    fn a_short_last_row_is_centred() {
        let grid = card_grid(3, 800.0, 2, 380.0, 20.0, 80.0);
        assert_eq!(grid.card_width, 380.0, "capped below the available width");
        let full_row = card_rect(0, 3, grid, 0.0, 800.0, 0.0, 20.0, RowAlign::Center);
        let orphan = card_rect(2, 3, grid, 0.0, 800.0, 0.0, 20.0, RowAlign::Center);
        assert_eq!(full_row.origin.x, (800.0 - (380.0 * 2.0 + 20.0)) / 2.0);
        assert_eq!(orphan.origin.x, (800.0 - grid.card_width) / 2.0);
        assert_eq!(orphan.origin.y, grid.card_height + 20.0);
        // Left-aligned, every row starts at the margin regardless of length.
        for index in 0..3 {
            let card = card_rect(index, 3, grid, 40.0, 800.0, 0.0, 20.0, RowAlign::Start);
            assert_eq!(card.origin.x, 40.0 + (index % 2) as f32 * (380.0 + 20.0));
        }
    }

    /// Justified: a full row reaches both margins, and the short row under it
    /// keeps the base gap instead of being spread across the page.
    #[test]
    fn justify_stretches_full_rows_only() {
        let grid = card_grid(3, 800.0, 2, 380.0, 20.0, 80.0);
        let at = |index| card_rect(index, 3, grid, 0.0, 800.0, 0.0, 20.0, RowAlign::Justify).origin.x;
        assert_eq!(at(0), 0.0);
        // 800 - 2 * 380 = 40 of slack, all of it in the single gap.
        assert_eq!(at(1), 380.0 + 40.0);
        assert_eq!(at(1) + 380.0, 800.0, "full row reaches the right margin");
        // Last row: one card, base gap, left margin.
        assert_eq!(at(2), 0.0);
    }

    /// A remainder too large to share out is left on the right rather than
    /// blown into the gaps.
    #[test]
    fn justify_gives_up_when_the_slack_is_absurd() {
        // Three 560-wide cards on a 2900-wide page: 1220 left over, which
        // would be 610 between neighbours.
        let grid = card_grid(3, 2900.0, 3, 560.0, 18.0, 104.0);
        assert_eq!(grid.card_width, 560.0);
        let at = |index| {
            card_rect(index, 3, grid, 0.0, 2900.0, 0.0, 18.0, RowAlign::Justify)
                .origin
                .x
        };
        assert_eq!(at(1), 560.0 + 18.0, "base gap, not a stretched one");
        assert_eq!(at(2), (560.0 + 18.0) * 2.0);
    }

    /// When the cards are not capped there is nothing to share out, so a
    /// justified grid is indistinguishable from a left-aligned one.
    #[test]
    fn justify_is_inert_when_cards_already_fill_the_row() {
        let grid = card_grid(4, 800.0, 2, 10_000.0, 20.0, 80.0);
        for index in 0..4 {
            let justified = card_rect(index, 4, grid, 0.0, 800.0, 0.0, 20.0, RowAlign::Justify);
            let start = card_rect(index, 4, grid, 0.0, 800.0, 0.0, 20.0, RowAlign::Start);
            assert_eq!(justified, start);
        }
    }

    #[test]
    fn warmth_extends_one_overscan_past_the_viewport() {
        let viewport = rect(0.0, 100.0, 500.0, 200.0);
        let above = rect(0.0, 40.0, 100.0, 40.0);
        assert!(!card_is_warm(above, viewport, 0.0));
        assert!(card_is_warm(above, viewport, 80.0));
        assert!(row_visible(90.0, 20.0, viewport));
        assert!(!row_fully_visible(90.0, 20.0, viewport));
        assert!(row_fully_visible(120.0, 20.0, viewport));
    }
}
