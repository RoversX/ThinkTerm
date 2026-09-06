//! Render pushes for a pane, applied one at a time in the order they
//! came, and what a push that a newer one has overtaken may skip.
use codec::GetPaneRenderChangesResponse;
use std::ops::Range;
use wezterm_term::StableRowIndex;

#[derive(Default)]
pub struct RenderDeltaQueue {
    pending: std::collections::VecDeque<GetPaneRenderChangesResponse>,
    draining: bool,
}

/// The rows named by the pushes still waiting in the queue: as bonus rows,
/// or as dirty ranges the client will fetch on its own.
#[derive(Default)]
pub struct RowsCarried {
    rows: std::collections::HashSet<StableRowIndex>,
    ranges: Vec<Range<StableRowIndex>>,
}

impl RowsCarried {
    pub fn contains(&self, row: StableRowIndex) -> bool {
        self.rows.contains(&row) || self.ranges.iter().any(|range| range.contains(&row))
    }
}

impl RenderDeltaQueue {
    /// Queue `delta`; true when the caller has to start the drain.
    pub fn push(&mut self, delta: GetPaneRenderChangesResponse) -> bool {
        self.pending.push_back(delta);
        !std::mem::replace(&mut self.draining, true)
    }

    /// The next push to apply, with the rows the pushes behind it carry
    /// when there are any, or None once the drain is over.
    pub fn take(&mut self) -> Option<(GetPaneRenderChangesResponse, Option<RowsCarried>)> {
        let delta = match self.pending.pop_front() {
            Some(delta) => delta,
            None => {
                self.draining = false;
                return None;
            }
        };
        if self.pending.is_empty() {
            return Some((delta, None));
        }
        let mut carried = RowsCarried::default();
        for newer in &self.pending {
            carried.rows.extend(newer.bonus_lines.rows());
            carried.ranges.extend(newer.dirty_lines.iter().cloned());
        }
        Some((delta, Some(carried)))
    }

    /// The drain's task ended without reaching the end of the queue (it
    /// was dropped); the next push must start a new one.
    pub fn end_drain(&mut self) {
        self.draining = false;
    }
}

#[cfg(test)]
mod render_delta_queue_tests {
    #[test]
    fn render_pushes_are_taken_in_order_and_know_when_another_waits() {
        let mut queue = super::RenderDeltaQueue::default();
        let delta = |seqno| codec::GetPaneRenderChangesResponse {
            pane_id: 1,
            mouse_grabbed: false,
            alt_screen: false,
            keyboard_encoding: Default::default(),
            cursor_position: Default::default(),
            dimensions: Default::default(),
            dirty_lines: vec![],
            title: String::new(),
            working_dir: None,
            bonus_lines: Vec::new().into(),
            input_serial: None,
            seqno,
        };
        assert!(queue.push(delta(1)), "the first push starts the drain");
        assert!(!queue.push(delta(2)), "the second rides along");
        let (first, carried) = queue.take().unwrap();
        assert_eq!(first.seqno, 1);
        assert!(
            carried.is_some(),
            "so the first is applied without fetching pictures"
        );
        let (second, carried) = queue.take().unwrap();
        assert_eq!(second.seqno, 2);
        assert!(carried.is_none(), "the latest one fetches");
        assert!(queue.take().is_none(), "and the drain ends");
    }

    /// A push applied without pictures leaves rows out; the rows the
    /// pushes behind it name are the ones something else will bring.
    #[test]
    fn a_push_applied_without_pictures_knows_which_rows_the_newer_ones_carry() {
        use termwiz::surface::Line;
        let mut queue = super::RenderDeltaQueue::default();
        let delta =
            |seqno,
             bonus: Vec<wezterm_term::StableRowIndex>,
             dirty: Vec<std::ops::Range<wezterm_term::StableRowIndex>>| {
                codec::GetPaneRenderChangesResponse {
                    pane_id: 1,
                    mouse_grabbed: false,
                    alt_screen: false,
                    keyboard_encoding: Default::default(),
                    cursor_position: Default::default(),
                    dimensions: Default::default(),
                    dirty_lines: dirty,
                    title: String::new(),
                    working_dir: None,
                    bonus_lines: bonus
                        .into_iter()
                        .map(|row| (row, Line::with_width(1, 0)))
                        .collect::<Vec<_>>()
                        .into(),
                    input_serial: None,
                    seqno,
                }
            };
        queue.push(delta(1, vec![3, 4], vec![]));
        queue.push(delta(2, vec![4], vec![10..12]));

        queue.push(delta(3, vec![7], vec![]));
        let (_, carried) = queue.take().unwrap();
        let carried = carried.expect("two pushes wait behind the first");
        assert!(carried.contains(4), "a bonus row of a later push");
        assert!(carried.contains(7));
        assert!(carried.contains(11), "a dirty row of a later push");
        assert!(
            !carried.contains(3),
            "row 3 is only in the push being applied: left out, it must be marked dirty"
        );
        assert!(!carried.contains(12), "ranges are half-open");
    }
}
