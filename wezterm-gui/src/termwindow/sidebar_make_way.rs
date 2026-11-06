//! Sidebar rows moving out of the way of one being dragged through them.
//!
//! Only the picture moves. A row keeps its place in the list and its hit
//! rect, which the drop is aimed by, until the drop: where a row is drawn
//! never feeds back into where the gap goes, so the rows cannot chase it.

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Time constant of the rows' ease: most of the way in three of these.
const TAU: Duration = Duration::from_millis(50);
/// Close enough to where it is going, in pixels.
const NEAR: f32 = 0.5;

/// Which rows give way for `source` to go before `before`, the end of the
/// list when None, and by how much: `by` is what `source` takes up. Rows
/// between where it was and where it goes close up behind it.
pub(crate) fn shifts_for_move(
    list: &[&str],
    source: &str,
    before: Option<&str>,
    by: f32,
) -> Vec<(String, f32)> {
    let Some(from) = list.iter().position(|id| *id == source) else {
        return vec![];
    };
    // Not in the list: a row the status filter hides, or the other side of
    // it. Either way past everything this list shows.
    let to = before
        .and_then(|before| list.iter().position(|id| *id == before))
        .unwrap_or(list.len());
    let (passed, by) = if to > from {
        (&list[from + 1..to], -by)
    } else {
        (&list[to..from], by)
    };
    passed.iter().map(|id| (id.to_string(), by)).collect()
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Shift {
    drawn: f32,
    going: f32,
}

/// How far from their places rows are drawn, by row or folder id. Holds only
/// rows on the move or moved aside, so it empties when a drag has settled.
#[derive(Debug, Default)]
pub(crate) struct SidebarMakeWay {
    shifts: HashMap<String, Shift>,
    last_tick: Option<Instant>,
}

impl SidebarMakeWay {
    pub fn shift_of(&self, id: &str) -> f32 {
        self.shifts.get(id).map_or(0.0, |shift| shift.drawn)
    }

    /// Send the rows in `going` that far from their places, the rest home.
    pub fn aim(&mut self, going: Vec<(String, f32)>) {
        for shift in self.shifts.values_mut() {
            shift.going = 0.0;
        }
        for (id, by) in going {
            self.shifts
                .entry(id)
                .or_insert(Shift {
                    drawn: 0.0,
                    going: 0.0,
                })
                .going = by;
        }
    }

    /// The drag is over. With `moved`, the list has just changed to what the
    /// rows were heading for, so each is now at home in a new place, still
    /// drawn where it was: it goes on from there rather than jumping. Without
    /// it, they go back.
    pub fn finish(&mut self, moved: bool) {
        for shift in self.shifts.values_mut() {
            if moved {
                shift.drawn -= shift.going;
            }
            shift.going = 0.0;
        }
    }

    /// Advance the rows. Returns whether any is still moving.
    pub fn step(&mut self, now: Instant) -> bool {
        // A starved frame must not land as one huge jump.
        let dt = self.last_tick.map_or(Duration::from_millis(16), |last| {
            now.saturating_duration_since(last)
                .min(Duration::from_millis(100))
        });
        let k = 1.0 - (-(dt.as_secs_f32() / TAU.as_secs_f32())).exp();
        let mut moving = false;
        for shift in self.shifts.values_mut() {
            if (shift.going - shift.drawn).abs() <= NEAR {
                shift.drawn = shift.going;
            } else {
                shift.drawn += (shift.going - shift.drawn) * k;
                moving = true;
            }
        }
        self.shifts
            .retain(|_, shift| shift.drawn != 0.0 || shift.going != 0.0);
        self.last_tick = moving.then_some(now);
        moving
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIST: [&str; 4] = ["a", "b", "c", "d"];

    fn shifts(source: &str, before: Option<&str>) -> Vec<(String, f32)> {
        shifts_for_move(&LIST, source, before, 10.0)
    }

    fn pairs(want: &[(&str, f32)]) -> Vec<(String, f32)> {
        want.iter().map(|(id, by)| (id.to_string(), *by)).collect()
    }

    #[test]
    fn rows_a_downward_drag_passes_close_up_behind_it() {
        assert_eq!(shifts("a", Some("c")), pairs(&[("b", -10.0)]));
        assert_eq!(shifts("a", Some("d")), pairs(&[("b", -10.0), ("c", -10.0)]));
    }

    #[test]
    fn rows_an_upward_drag_passes_step_down_for_it() {
        assert_eq!(shifts("d", Some("c")), pairs(&[("c", 10.0)]));
        assert_eq!(shifts("c", Some("a")), pairs(&[("a", 10.0), ("b", 10.0)]));
    }

    #[test]
    fn the_end_of_the_list_is_past_every_row() {
        assert_eq!(shifts("c", None), pairs(&[("d", -10.0)]));
        // Hidden by the status filter: as far as this list shows, the end.
        assert_eq!(shifts("c", Some("hidden")), pairs(&[("d", -10.0)]));
    }

    #[test]
    fn nothing_moves_for_a_row_over_its_own_place() {
        assert!(shifts("b", Some("b")).is_empty());
        assert!(shifts("b", Some("c")).is_empty());
        assert!(shifts("d", None).is_empty());
    }

    #[test]
    fn a_row_not_in_the_list_moves_nothing() {
        assert!(shifts("elsewhere", Some("a")).is_empty());
    }

    fn settle(way: &mut SidebarMakeWay) {
        let mut now = Instant::now();
        for _ in 0..200 {
            now += Duration::from_millis(16);
            if !way.step(now) {
                return;
            }
        }
        panic!("the rows never settled");
    }

    #[test]
    fn rows_stay_aside_until_sent_home_and_are_then_forgotten() {
        let mut way = SidebarMakeWay::default();
        way.aim(pairs(&[("b", -10.0)]));
        settle(&mut way);
        assert_eq!(way.shift_of("b"), -10.0);

        way.aim(vec![]);
        settle(&mut way);
        assert_eq!(way.shift_of("b"), 0.0);
        assert!(way.shifts.is_empty());
    }

    #[test]
    fn a_row_changes_course_from_where_it_is_drawn() {
        let mut way = SidebarMakeWay::default();
        way.aim(pairs(&[("b", -10.0)]));
        way.step(Instant::now());
        let drawn = way.shift_of("b");
        assert!(drawn < 0.0 && drawn > -10.0);

        way.aim(pairs(&[("b", 10.0)]));
        assert_eq!(way.shift_of("b"), drawn);
    }

    #[test]
    fn a_drop_leaves_rows_drawn_where_they_were() {
        let mut way = SidebarMakeWay::default();
        way.aim(pairs(&[("b", -10.0)]));
        way.step(Instant::now());
        let drawn = way.shift_of("b");

        // The row's place is now ten pixels up: drawn at the same spot, it
        // is that much less far from it.
        way.finish(true);
        assert_eq!(way.shift_of("b"), drawn + 10.0);
        settle(&mut way);
        assert!(way.shifts.is_empty());
    }

    #[test]
    fn a_cancelled_drag_sends_rows_home_from_where_they_are() {
        let mut way = SidebarMakeWay::default();
        way.aim(pairs(&[("b", -10.0)]));
        way.step(Instant::now());
        let drawn = way.shift_of("b");

        way.finish(false);
        assert_eq!(way.shift_of("b"), drawn);
        settle(&mut way);
        assert!(way.shifts.is_empty());
    }
}
