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

    /// Rows the list itself has just moved by `by`, a row taken away or
    /// added above them: drawn where they were, going on from there.
    pub fn glide(&mut self, id: &str, by: f32) {
        self.shifts
            .entry(id.to_string())
            .or_insert(Shift {
                drawn: 0.0,
                going: 0.0,
            })
            .drawn += by;
    }

    /// Nothing is moving aside or settling.
    pub fn is_idle(&self) -> bool {
        self.shifts.is_empty()
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
                // Landing counts: the frame that shows it landed is owed.
                moving |= shift.drawn != shift.going;
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

/// Where the list laid out each of its rows in the paint before, so that a
/// row the list itself moves -- one taken away or added above it, or the row
/// itself pinned or put back -- can be drawn going there rather than
/// jumping. Holds the rows of one Space as last painted, updated in place, so
/// it is no bigger than the list and a frame allocates only for rows it has
/// not seen.
#[derive(Debug, Default)]
pub(crate) struct RowPlaces {
    space: String,
    places: HashMap<String, Place>,
    /// Counts the paints.
    paint: u64,
    /// Whether the list was painted in the frame under way.
    painted: bool,
    /// Whether this paint has nothing before it to compare with.
    fresh: bool,
    /// Where the list was scrolled to in the last paint.
    scroll: f32,
    /// Whether this paint's list is scrolled as the last one's was.
    steady: bool,
    /// Whether a row has appeared in this paint, above the rows still to
    /// be noted.
    grown: bool,
}

#[derive(Debug, Clone)]
struct Place {
    /// The part of the list `was` and `is` are counted in.
    within: String,
    was: f32,
    is: f32,
    /// The same two counted from the top of the list.
    was_in_list: f32,
    is_in_list: f32,
    paint: u64,
}

impl RowPlaces {
    /// A paint of `space`'s list, scrolled to `scroll`, starts. Rows the
    /// last paint did not lay out are let go of. Nothing is compared in a
    /// paint scrolled from where the last one was: a list that shrank under
    /// its scrolled end is pulled down by as much as its rows go up, so on
    /// screen they have not moved.
    pub fn begin(&mut self, space: &str, scroll: f32) {
        self.fresh = self.space != space || self.places.is_empty();
        if self.space != space {
            self.space.clear();
            self.space.push_str(space);
            self.places.clear();
        }
        let last = self.paint;
        self.places.retain(|_, place| place.paint == last);
        self.paint += 1;
        self.painted = true;
        self.steady = (scroll - self.scroll).abs() < NEAR;
        self.scroll = scroll;
        self.grown = false;
    }

    /// A frame is done. One that did not paint the list (the sidebar away,
    /// say) lets go of every row: the list may move while nobody watches.
    pub fn end_frame(&mut self) {
        if !std::mem::take(&mut self.painted) {
            self.places.clear();
        }
    }

    /// `id` is laid out at `place` in this paint, counted from the top of
    /// `within` (a folder, say), and at `in_list` from the top of the list.
    /// Returns how far up it is to be drawn from there, as far up as the
    /// list has moved it since the paint before. `carried` is how far its
    /// part of the list was just sent the same way, which a row staying in
    /// it shares and a row new to it does not.
    ///
    /// After a row new to the list, the rows going down to make room for it
    /// would slide over it, so they and the rest are put in place at once:
    /// all but one come from another part of the list, which is going far.
    pub fn note(
        &mut self,
        id: &str,
        within: &str,
        place: f32,
        in_list: f32,
        carried: f32,
    ) -> Option<f32> {
        let paint = self.paint;
        let Some(known) = self.places.get_mut(id) else {
            self.places.insert(
                id.to_string(),
                Place {
                    within: within.to_string(),
                    was: place,
                    is: place,
                    was_in_list: in_list,
                    is_in_list: in_list,
                    paint,
                },
            );
            self.grown |= !self.fresh;
            return None;
        };
        if known.paint != paint {
            known.was = known.is;
            known.was_in_list = known.is_in_list;
            known.paint = paint;
        }
        known.is = place;
        known.is_in_list = in_list;
        let crossed = known.within != within;
        let by = if crossed {
            known.within.clear();
            known.within.push_str(within);
            known.was_in_list - in_list - carried
        } else {
            known.was - place
        };
        let crowded = self.grown && !crossed;
        (self.steady && !crowded && by.abs() >= NEAR).then_some(by)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One frame painting the list once, noting `rows` as (id, within,
    /// place, place in the list, carried).
    fn paint(
        places: &mut RowPlaces,
        scroll: f32,
        rows: &[(&str, &str, f32, f32, f32)],
    ) -> Vec<Option<f32>> {
        places.begin("s", scroll);
        let moved = rows
            .iter()
            .map(|(id, within, place, in_list, carried)| {
                places.note(id, within, *place, *in_list, *carried)
            })
            .collect();
        places.end_frame();
        moved
    }

    #[test]
    fn a_row_the_list_moves_is_reported_once() {
        let mut places = RowPlaces::default();
        assert_eq!(
            paint(&mut places, 0.0, &[("b", "", 40.0, 40.0, 0.0)]),
            [None]
        );
        assert_eq!(
            paint(&mut places, 0.0, &[("b", "", 20.0, 20.0, 0.0)]),
            [Some(20.0)]
        );
        assert_eq!(
            paint(&mut places, 0.0, &[("b", "", 20.0, 20.0, 0.0)]),
            [None]
        );
        assert_eq!(
            paint(&mut places, 0.0, &[("b", "", 60.0, 60.0, 0.0)]),
            [Some(-40.0)]
        );
    }

    #[test]
    fn a_row_painted_twice_in_a_paint_is_reported_both_times() {
        let mut places = RowPlaces::default();
        paint(&mut places, 0.0, &[("b", "", 40.0, 40.0, 0.0)]);
        let row = ("b", "", 20.0, 20.0, 0.0);
        assert_eq!(
            paint(&mut places, 0.0, &[row, row]),
            [Some(20.0), Some(20.0)]
        );
    }

    #[test]
    fn rows_of_another_space_are_not_compared() {
        let mut places = RowPlaces::default();
        paint(&mut places, 0.0, &[("b", "", 40.0, 40.0, 0.0)]);
        places.begin("t", 0.0);
        assert_eq!(places.note("b", "", 20.0, 20.0, 0.0), None);
    }

    #[test]
    fn a_frame_without_the_list_lets_go_of_its_rows() {
        let mut places = RowPlaces::default();
        paint(&mut places, 0.0, &[("b", "", 40.0, 40.0, 0.0)]);
        places.end_frame();
        assert_eq!(
            paint(&mut places, 0.0, &[("b", "", 20.0, 20.0, 0.0)]),
            [None]
        );
    }

    #[test]
    fn a_row_pinned_goes_from_where_it_was_in_the_list() {
        let mut places = RowPlaces::default();
        paint(&mut places, 0.0, &[("b", "folder", 20.0, 100.0, 0.0)]);
        assert_eq!(
            paint(&mut places, 0.0, &[("b", "", 20.0, 20.0, 0.0)]),
            [Some(80.0)]
        );
        // Put back into a folder sent 10 up itself: the row's own share is
        // the rest of the way.
        assert_eq!(
            paint(&mut places, 0.0, &[("b", "folder", 20.0, 110.0, 10.0)]),
            [Some(-100.0)]
        );
    }

    #[test]
    fn a_list_scrolled_since_the_paint_before_reports_no_move() {
        let mut places = RowPlaces::default();
        paint(&mut places, 40.0, &[("b", "", 60.0, 60.0, 0.0)]);
        assert_eq!(
            paint(&mut places, 20.0, &[("b", "", 40.0, 40.0, 0.0)]),
            [None]
        );
        assert_eq!(
            paint(&mut places, 20.0, &[("b", "", 40.0, 40.0, 0.0)]),
            [None]
        );
    }

    #[test]
    fn rows_after_a_new_row_are_not_pushed_down_over_it() {
        let mut places = RowPlaces::default();
        paint(
            &mut places,
            0.0,
            &[("a", "", 0.0, 0.0, 0.0), ("c", "", 20.0, 20.0, 0.0)],
        );
        assert_eq!(
            paint(
                &mut places,
                0.0,
                &[
                    ("a", "", 0.0, 0.0, 0.0),
                    ("b", "", 20.0, 20.0, 0.0),
                    ("c", "", 40.0, 40.0, 0.0)
                ],
            ),
            [None, None, None]
        );
    }

    #[test]
    fn the_first_pin_sends_the_pinned_row_alone() {
        let mut places = RowPlaces::default();
        paint(
            &mut places,
            0.0,
            &[
                ("label", "", 0.0, 0.0, 0.0),
                ("folder", "", 20.0, 20.0, 0.0),
                ("c", "folder", 20.0, 40.0, 0.0),
                ("d", "folder", 40.0, 60.0, 0.0),
            ],
        );
        // The Pinned heading appears and c goes under it: the rest is put
        // where it goes, and c alone comes up from its folder.
        assert_eq!(
            paint(
                &mut places,
                0.0,
                &[
                    ("pinned", "", 0.0, 0.0, 0.0),
                    ("c", "", 20.0, 20.0, 0.0),
                    ("label", "", 40.0, 40.0, 0.0),
                    ("folder", "", 60.0, 60.0, 0.0),
                    ("d", "folder", 20.0, 80.0, 0.0),
                ],
            ),
            [None, Some(20.0), None, None, None]
        );
    }

    #[test]
    fn a_glided_row_settles_home() {
        let mut way = SidebarMakeWay::default();
        way.glide("b", 20.0);
        assert_eq!(way.shift_of("b"), 20.0);
        settle(&mut way);
        assert!(way.is_idle());
    }

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
