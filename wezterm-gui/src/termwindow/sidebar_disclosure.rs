//! The chevron beside a sidebar folder's add button: there only while the
//! pointer is over the folder's row, and turning as the folder opens and
//! closes.
//!
//! Only folders painted since the last step are kept, so the table is no
//! bigger than the folders on screen and lets go of the rest at once.

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// The rotation frames the chevron's sprite is cut into, a full turn's worth:
/// a quarter turn, from pointing right to pointing down, takes a quarter of
/// them.
pub(crate) const CHEVRON_FRAMES: u8 = 48;
/// Time constant of the ease: most of the way in three of these.
const TAU: Duration = Duration::from_millis(50);
/// Close enough to where it is going.
const NEAR: f32 = 0.01;

/// Which list a folder's id is from: a project's and a reference group's
/// need not differ.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FolderKind {
    Project,
    RefGroup,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Chevron {
    shown: f32,
    show_to: f32,
    open: f32,
    open_to: f32,
    /// The step it was last painted before.
    painted: u64,
}

#[derive(Debug, Default)]
pub(crate) struct FolderChevrons {
    projects: HashMap<String, Chevron>,
    groups: HashMap<String, Chevron>,
    /// Counts the steps; a chevron painted since the last one carries it.
    step: u64,
    last_tick: Option<Instant>,
    /// Until the next step, every folder is drawn where it is going.
    landing: bool,
}

impl FolderChevrons {
    fn of_kind(&mut self, kind: FolderKind) -> &mut HashMap<String, Chevron> {
        match kind {
            FolderKind::Project => &mut self.projects,
            FolderKind::RefGroup => &mut self.groups,
        }
    }

    /// Where `id`'s chevron is in the frame being painted, heading for
    /// `shown` and `open`: how far it has faded in and how far it has turned
    /// towards open, each 0 to 1. A folder not met before starts where it is
    /// going, so nothing turns as the sidebar first appears.
    pub fn aim(&mut self, kind: FolderKind, id: &str, shown: bool, open: bool) -> (f32, f32) {
        let show_to = if shown { 1.0 } else { 0.0 };
        let open_to = if open { 1.0 } else { 0.0 };
        let step = self.step;
        let landing = self.landing;
        let chevrons = self.of_kind(kind);
        if let Some(chevron) = chevrons.get_mut(id) {
            chevron.show_to = show_to;
            chevron.open_to = open_to;
            chevron.painted = step;
            if landing {
                chevron.open = open_to;
            }
            return (chevron.shown, chevron.open);
        }
        chevrons.insert(
            id.to_string(),
            Chevron {
                shown: show_to,
                show_to,
                open: open_to,
                open_to,
                painted: step,
            },
        );
        (show_to, open_to)
    }

    /// How far open `id` is drawn, if it was painted lately.
    pub fn open_of(&self, kind: FolderKind, id: &str) -> Option<f32> {
        if self.landing {
            return None;
        }
        let chevrons = match kind {
            FolderKind::Project => &self.projects,
            FolderKind::RefGroup => &self.groups,
        };
        chevrons.get(id).map(|chevron| chevron.open)
    }

    /// Draw every folder where it is going in the next frame, without
    /// the turn: for a jump to a row inside one, which needs the list as it
    /// will be.
    pub fn land(&mut self) {
        self.landing = true;
    }

    /// A folder is opening or closing: its rows, and the list after them,
    /// are on their way somewhere.
    pub fn folding(&self) -> bool {
        self.projects
            .values()
            .chain(self.groups.values())
            .any(|chevron| chevron.open != chevron.open_to)
    }

    /// Advance the chevrons painted since the last step and forget the rest.
    /// Returns whether any is still moving.
    pub fn step(&mut self, now: Instant) -> bool {
        self.landing = false;
        let step = self.step;
        self.step += 1;
        self.projects.retain(|_, chevron| chevron.painted == step);
        self.groups.retain(|_, chevron| chevron.painted == step);
        // A starved frame must not land as one huge jump.
        let dt = self.last_tick.map_or(Duration::from_millis(16), |last| {
            now.saturating_duration_since(last)
                .min(Duration::from_millis(100))
        });
        let k = 1.0 - (-(dt.as_secs_f32() / TAU.as_secs_f32())).exp();
        let mut moving = false;
        for chevron in self.projects.values_mut().chain(self.groups.values_mut()) {
            moving |= ease(&mut chevron.shown, chevron.show_to, k);
            moving |= ease(&mut chevron.open, chevron.open_to, k);
        }
        self.last_tick = moving.then_some(now);
        moving
    }
}

/// The rotation frame for a chevron turned `open` of the way to open.
pub(crate) fn chevron_frame(open: f32) -> u8 {
    let quarter = f32::from(CHEVRON_FRAMES / 4);
    (open.clamp(0.0, 1.0) * quarter).round() as u8
}

/// Whether `value` changed: the step that lands it on `to` counts, so that
/// the frame showing it there is still asked for.
fn ease(value: &mut f32, to: f32, k: f32) -> bool {
    if *value == to {
        return false;
    }
    if (to - *value).abs() <= NEAR {
        *value = to;
    } else {
        *value += (to - *value) * k;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settle(chevrons: &mut FolderChevrons, id: &str, shown: bool, open: bool) {
        let mut now = Instant::now();
        for _ in 0..200 {
            chevrons.aim(FolderKind::Project, id, shown, open);
            now += Duration::from_millis(16);
            if !chevrons.step(now) {
                return;
            }
        }
        panic!("the chevron never settled");
    }

    #[test]
    fn a_folder_met_for_the_first_time_does_not_move() {
        let mut chevrons = FolderChevrons::default();
        assert_eq!(
            chevrons.aim(FolderKind::Project, "a", false, true),
            (0.0, 1.0)
        );
        assert!(!chevrons.step(Instant::now()));
    }

    #[test]
    fn a_chevron_fades_in_under_the_pointer_and_out_again() {
        let mut chevrons = FolderChevrons::default();
        chevrons.aim(FolderKind::Project, "a", false, true);
        chevrons.step(Instant::now());
        chevrons.aim(FolderKind::Project, "a", true, true);
        assert!(chevrons.step(Instant::now()));
        let (shown, _) = chevrons.aim(FolderKind::Project, "a", true, true);
        assert!(shown > 0.0 && shown < 1.0);
        settle(&mut chevrons, "a", true, true);
        assert_eq!(chevrons.aim(FolderKind::Project, "a", true, true).0, 1.0);
        settle(&mut chevrons, "a", false, true);
        assert_eq!(chevrons.aim(FolderKind::Project, "a", false, true).0, 0.0);
    }

    #[test]
    fn closing_a_folder_turns_its_chevron_back_to_pointing_right() {
        let mut chevrons = FolderChevrons::default();
        chevrons.aim(FolderKind::Project, "a", true, true);
        chevrons.step(Instant::now());
        settle(&mut chevrons, "a", true, false);
        assert_eq!(
            chevron_frame(chevrons.aim(FolderKind::Project, "a", true, false).1),
            0
        );
        settle(&mut chevrons, "a", true, true);
        assert_eq!(
            chevron_frame(chevrons.aim(FolderKind::Project, "a", true, true).1),
            CHEVRON_FRAMES / 4
        );
    }

    #[test]
    fn the_last_frame_asked_for_shows_the_folder_where_it_was_going() {
        let mut chevrons = FolderChevrons::default();
        chevrons.aim(FolderKind::Project, "a", true, true);
        chevrons.step(Instant::now());
        let mut now = Instant::now();
        for _ in 0..200 {
            let painted = chevrons.aim(FolderKind::Project, "a", false, false);
            now += Duration::from_millis(16);
            if !chevrons.step(now) {
                assert_eq!(painted, (0.0, 0.0));
                return;
            }
        }
        panic!("the chevron never settled");
    }

    #[test]
    fn a_landed_folder_is_drawn_open_at_once() {
        let mut chevrons = FolderChevrons::default();
        chevrons.aim(FolderKind::Project, "a", true, false);
        chevrons.step(Instant::now());
        chevrons.land();
        assert_eq!(chevrons.open_of(FolderKind::Project, "a"), None);
        assert_eq!(chevrons.aim(FolderKind::Project, "a", true, true).1, 1.0);
        chevrons.step(Instant::now());
        assert_eq!(chevrons.open_of(FolderKind::Project, "a"), Some(1.0));
    }

    #[test]
    fn a_folder_no_longer_painted_is_forgotten() {
        let mut chevrons = FolderChevrons::default();
        chevrons.aim(FolderKind::Project, "a", true, true);
        chevrons.step(Instant::now());
        chevrons.step(Instant::now());
        assert!(chevrons.projects.is_empty());
    }
}
