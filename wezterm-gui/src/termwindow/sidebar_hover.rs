//! Hover-reveal of the collapsed workspace sidebar.
//!
//! A pure state machine, deliberately free of `TermWindow`: it is fed a
//! [`HoverInput`] snapshot once per mouse event and once per frame, and
//! answers with the single [`HoverFrame`] obligation the frame loop owes it.
//! The panel it drives is an OVERLAY — revealing must never reflow the
//! terminal, which is why nothing in here touches `workspace_sidebar_width`
//! or `workspace_sidebar_collapsed`.

use crate::termwindow::ui::tokens::{SIDEBAR_HOVER_DWELL_MS, SIDEBAR_HOVER_GRACE_MS};
use crate::ui::anim::{self, Easing, Timeline};
use std::time::{Duration, Instant};

const DWELL: Duration = Duration::from_millis(SIDEBAR_HOVER_DWELL_MS);
const GRACE: Duration = Duration::from_millis(SIDEBAR_HOVER_GRACE_MS);
/// Both SHORT: the panel is narrow and a hover reveal is asked for casually,
/// so it should feel like a flick, not a page turn.
const REVEAL: Duration = anim::SHORT;
const RETREAT: Duration = anim::SHORT;

/// Where the pointer is, in the only four terms the machine cares about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PointerZone {
    /// Outside the window, or somewhere with no bearing on the panel.
    Away,
    /// In the narrow left-edge strip that arms a reveal.
    HotZone,
    /// Just outside the arming strip but inside the wider sticky band:
    /// close enough to keep a running dwell alive, not close enough to
    /// start one. The hysteresis that stops a one-pixel wobble from
    /// restarting the count.
    NearHotZone,
    /// Inside the panel as presented.
    Panel,
}

/// Everything the machine needs about the frame it is stepped in. `Copy`, so
/// a caller cannot accidentally hold a borrow of the window across a
/// transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HoverInput {
    /// A reveal is possible at all: enabled in settings, the sidebar
    /// collapsed, no content view in the foreground, no menu, modal or
    /// gesture in flight.
    pub(crate) eligible: bool,
    pub(crate) pointer: PointerZone,
    /// A press, capture or drag is live, so retreating would pull the ground
    /// out from under it — and arming would steal a click from the terminal.
    pub(crate) pinned: bool,
}

/// What a step owes the frame loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HoverFrame {
    /// Nothing owed.
    None,
    /// As fast as the display will give it: something is travelling.
    Now,
    /// Exactly one wakeup, at this instant: a dwell or grace is counting
    /// down.
    At(Instant),
}

#[derive(Debug, Clone)]
enum Phase {
    Hidden,
    Arming {
        since: Instant,
    },
    /// `travel` runs 0 -> 1. It is carried through `Departing` so a pointer
    /// that leaves mid-arrival still sees the panel finish arriving before
    /// the grace runs out, rather than freezing part-way.
    Revealing {
        travel: Timeline,
    },
    Revealed,
    Departing {
        travel: Timeline,
        grace_until: Instant,
    },
    Retreating {
        travel: Timeline,
    },
    /// Collapsed by hand while the pointer was still in the strip. Re-arming
    /// from here would undo the click that just closed the panel, so this
    /// waits for the pointer to leave before it will consider anything.
    Suppressed,
}

impl Default for Phase {
    fn default() -> Self {
        Self::Hidden
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct SidebarHoverReveal {
    phase: Phase,
}

impl SidebarHoverReveal {
    /// The whole transition. Call once per frame and once per mouse event.
    pub(crate) fn step(&mut self, input: HoverInput, now: Instant) -> HoverFrame {
        let phase = std::mem::take(&mut self.phase);

        if !input.eligible {
            // Whatever made the machine ineligible (a content view opening, a
            // settings change) may not repaint on its own; if the panel or
            // the arming hint was on screen, one more frame erases it.
            let needs_erase = !matches!(phase, Phase::Hidden | Phase::Suppressed);
            self.phase = Phase::Hidden;
            return if needs_erase {
                HoverFrame::Now
            } else {
                HoverFrame::None
            };
        }

        let (phase, frame) = match phase {
            Phase::Hidden => {
                if input.pointer == PointerZone::HotZone && !input.pinned {
                    (Phase::Arming { since: now }, HoverFrame::At(now + DWELL))
                } else {
                    (Phase::Hidden, HoverFrame::None)
                }
            }
            Phase::Arming { since } => {
                // The hint strip is on screen: every exit owes one erasing
                // frame. Holding tolerates the sticky band; only starting
                // demanded the narrow strip.
                let holding = matches!(
                    input.pointer,
                    PointerZone::HotZone | PointerZone::NearHotZone
                );
                if input.pinned {
                    // A press at the edge belongs to the terminal.
                    (Phase::Hidden, HoverFrame::Now)
                } else if !holding {
                    (Phase::Hidden, HoverFrame::Now)
                } else if now.saturating_duration_since(since) >= DWELL {
                    (
                        Phase::Revealing {
                            travel: Timeline::progress(now, REVEAL, Easing::OutCubic),
                        },
                        HoverFrame::Now,
                    )
                } else {
                    (Phase::Arming { since }, HoverFrame::At(since + DWELL))
                }
            }
            Phase::Revealing { mut travel } => {
                if input.pointer == PointerZone::Away && !input.pinned {
                    (
                        Phase::Departing {
                            travel,
                            grace_until: now + GRACE,
                        },
                        HoverFrame::At(now + GRACE),
                    )
                } else if travel.advance(now) {
                    (Phase::Revealing { travel }, HoverFrame::Now)
                } else {
                    // One last frame lands the end value.
                    (Phase::Revealed, HoverFrame::Now)
                }
            }
            Phase::Revealed => {
                if input.pointer == PointerZone::Away && !input.pinned {
                    (
                        Phase::Departing {
                            travel: Timeline::settled(now, 1.0),
                            grace_until: now + GRACE,
                        },
                        HoverFrame::At(now + GRACE),
                    )
                } else {
                    (Phase::Revealed, HoverFrame::None)
                }
            }
            Phase::Departing {
                mut travel,
                grace_until,
            } => {
                if input.pointer != PointerZone::Away || input.pinned {
                    if travel.is_running() {
                        (Phase::Revealing { travel }, HoverFrame::Now)
                    } else {
                        (Phase::Revealed, HoverFrame::None)
                    }
                } else if now >= grace_until {
                    travel.retarget(now, 0.0, RETREAT, Easing::OutCubic);
                    (Phase::Retreating { travel }, HoverFrame::Now)
                } else if travel.advance(now) {
                    // Still arriving while the grace counts down.
                    (
                        Phase::Departing {
                            travel,
                            grace_until,
                        },
                        HoverFrame::Now,
                    )
                } else {
                    (
                        Phase::Departing {
                            travel,
                            grace_until,
                        },
                        HoverFrame::At(grace_until),
                    )
                }
            }
            Phase::Retreating { mut travel } => {
                if input.pointer != PointerZone::Away {
                    // Departs from where the panel is: no jump.
                    travel.retarget(now, 1.0, REVEAL, Easing::OutCubic);
                    (Phase::Revealing { travel }, HoverFrame::Now)
                } else if travel.advance(now) {
                    (Phase::Retreating { travel }, HoverFrame::Now)
                } else {
                    // One more frame erases the panel.
                    (Phase::Hidden, HoverFrame::Now)
                }
            }
            Phase::Suppressed => {
                if input.pointer == PointerZone::Away {
                    (Phase::Hidden, HoverFrame::None)
                } else {
                    (Phase::Suppressed, HoverFrame::None)
                }
            }
        };
        self.phase = phase;
        frame
    }

    /// `None` = nothing presented; `Some(0.0..=1.0)` = how far out the panel
    /// is.
    pub(crate) fn progress(&self, now: Instant) -> Option<f32> {
        match &self.phase {
            Phase::Revealing { travel }
            | Phase::Departing { travel, .. }
            | Phase::Retreating { travel } => Some(travel.value(now).clamp(0.0, 1.0)),
            Phase::Revealed => Some(1.0),
            Phase::Hidden | Phase::Arming { .. } | Phase::Suppressed => None,
        }
    }

    /// Whether the panel is on screen at all. Clock-free, so geometry code
    /// can ask without a timestamp.
    pub(crate) fn is_presented(&self) -> bool {
        matches!(
            self.phase,
            Phase::Revealing { .. }
                | Phase::Revealed
                | Phase::Departing { .. }
                | Phase::Retreating { .. }
        )
    }

    /// Fully out: real hit targets may be registered.
    pub(crate) fn is_fully_presented(&self, now: Instant) -> bool {
        self.progress(now).is_some_and(|p| p >= 1.0)
    }

    /// The dwell is counting down: the edge hint should be visible.
    pub(crate) fn is_arming(&self) -> bool {
        matches!(self.phase, Phase::Arming { .. })
    }

    /// Whether the machine still owes frames — a travel in flight or a dwell
    /// or grace counting down. Feeds the unfocused-window frame gate.
    pub(crate) fn needs_frames(&self) -> bool {
        !matches!(
            self.phase,
            Phase::Hidden | Phase::Revealed | Phase::Suppressed
        )
    }

    /// A manual collapse with the pointer still at the edge must not re-open
    /// the panel it just closed: wait for the pointer to leave first.
    pub(crate) fn suppress_until_pointer_leaves(&mut self) {
        self.phase = Phase::Suppressed;
    }

    /// Land a travelling panel instead of letting a geometry change make it
    /// jump: a settled overlay survives a resize or DPI change cleanly.
    pub(crate) fn settle_immediately(&mut self) {
        self.phase = match std::mem::take(&mut self.phase) {
            Phase::Revealing { .. } | Phase::Departing { .. } => Phase::Revealed,
            Phase::Retreating { .. } => Phase::Hidden,
            other => other,
        };
    }

    /// Focus loss: drop the panel outright.
    pub(crate) fn cancel_immediately(&mut self) {
        self.phase = Phase::Hidden;
    }
}

/// The left-edge strip that arms a reveal, given the panel's would-be rect.
///
/// Deliberately excludes the top chrome row: at the top-left the pointer is
/// on its way to the traffic lights or to the sidebar toggle, and revealing
/// the panel under it fights the button the user is aiming at.
pub(crate) fn hot_zone(
    x: usize,
    y: usize,
    height: usize,
    hot_zone_width: usize,
    top_chrome_height: usize,
) -> Option<(usize, usize, usize, usize)> {
    if hot_zone_width == 0 {
        return None;
    }
    let top = y.max(top_chrome_height);
    let bottom = y.saturating_add(height);
    if top >= bottom {
        return None;
    }
    Some((x, top, hot_zone_width, bottom - top))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(start: Instant, ms: u64) -> Instant {
        start + Duration::from_millis(ms)
    }

    fn input(zone: PointerZone) -> HoverInput {
        HoverInput {
            eligible: true,
            pointer: zone,
            pinned: false,
        }
    }

    fn pinned(zone: PointerZone) -> HoverInput {
        HoverInput {
            pinned: true,
            ..input(zone)
        }
    }

    /// Step until the machine reports Revealed (progress 1.0), sampling
    /// values so the Timeline clock runs. Returns the time it settled at.
    fn reveal(machine: &mut SidebarHoverReveal, start: Instant) -> Instant {
        machine.step(input(PointerZone::HotZone), start);
        let mut now = at(start, SIDEBAR_HOVER_DWELL_MS);
        machine.step(input(PointerZone::HotZone), now);
        // Sample the opening frame so the clock starts, then run it out.
        for _ in 0..200 {
            let _ = machine.progress(now);
            now += Duration::from_millis(16);
            machine.step(input(PointerZone::HotZone), now);
            if machine.progress(now) == Some(1.0) && !machine.needs_frames() {
                return now;
            }
        }
        panic!("panel never settled");
    }

    #[test]
    fn a_pointer_passing_through_the_edge_never_reveals() {
        let start = Instant::now();
        let mut machine = SidebarHoverReveal::default();
        assert_eq!(
            machine.step(input(PointerZone::HotZone), start),
            HoverFrame::At(at(start, SIDEBAR_HOVER_DWELL_MS))
        );
        // Leaving owes one frame to erase the arming hint, then nothing.
        assert_eq!(
            machine.step(input(PointerZone::Away), at(start, 100)),
            HoverFrame::Now
        );
        assert_eq!(machine.progress(at(start, 100)), None);
        assert!(!machine.is_presented());
        assert!(!machine.is_arming());
    }

    #[test]
    fn a_wobble_into_the_sticky_band_keeps_the_dwell_counting() {
        let start = Instant::now();
        let mut machine = SidebarHoverReveal::default();
        machine.step(input(PointerZone::HotZone), start);
        assert!(machine.is_arming());
        // Drifts just outside the narrow strip: the count survives...
        assert_eq!(
            machine.step(input(PointerZone::NearHotZone), at(start, 60)),
            HoverFrame::At(at(start, SIDEBAR_HOVER_DWELL_MS))
        );
        // ...and still fires on schedule, even from the sticky band.
        assert_eq!(
            machine.step(
                input(PointerZone::NearHotZone),
                at(start, SIDEBAR_HOVER_DWELL_MS)
            ),
            HoverFrame::Now
        );
        assert!(machine.is_presented());
        // But the sticky band never STARTS a dwell.
        let mut fresh = SidebarHoverReveal::default();
        assert_eq!(
            fresh.step(input(PointerZone::NearHotZone), start),
            HoverFrame::None
        );
        assert!(!fresh.is_arming());
    }

    #[test]
    fn a_motionless_pointer_reveals_when_the_dwell_expires() {
        let start = Instant::now();
        let mut machine = SidebarHoverReveal::default();
        machine.step(input(PointerZone::HotZone), start);
        // One tick short: still counting down to the same deadline.
        assert_eq!(
            machine.step(
                input(PointerZone::HotZone),
                at(start, SIDEBAR_HOVER_DWELL_MS - 1)
            ),
            HoverFrame::At(at(start, SIDEBAR_HOVER_DWELL_MS))
        );
        assert_eq!(
            machine.progress(at(start, SIDEBAR_HOVER_DWELL_MS - 1)),
            None
        );
        // Expiry: travelling.
        assert_eq!(
            machine.step(
                input(PointerZone::HotZone),
                at(start, SIDEBAR_HOVER_DWELL_MS)
            ),
            HoverFrame::Now
        );
        assert!(machine
            .progress(at(start, SIDEBAR_HOVER_DWELL_MS))
            .is_some());
        assert!(machine.is_presented());
    }

    #[test]
    fn the_reveal_clock_does_not_start_until_the_first_frame_is_presented() {
        let start = Instant::now();
        let mut machine = SidebarHoverReveal::default();
        machine.step(input(PointerZone::HotZone), start);
        machine.step(input(PointerZone::HotZone), at(start, 250));
        // Opening frame: 0.0, and sampling is what arms the clock.
        assert_eq!(machine.progress(at(start, 250)), Some(0.0));
        // The clock starts on the *next* advance after the sample.
        machine.step(input(PointerZone::HotZone), at(start, 350));
        let p = machine.progress(at(start, 350)).unwrap();
        assert!(p < 0.05, "clock should start at the sampled frame, got {p}");
    }

    #[test]
    fn a_press_in_the_hot_zone_belongs_to_the_terminal() {
        let start = Instant::now();
        let mut machine = SidebarHoverReveal::default();
        machine.step(input(PointerZone::HotZone), start);
        // Cancelling owes one frame to erase the arming hint.
        assert_eq!(
            machine.step(pinned(PointerZone::HotZone), at(start, 100)),
            HoverFrame::Now
        );
        assert!(!machine.is_presented());
        // And the pinned pointer sitting there does not re-arm.
        assert_eq!(
            machine.step(pinned(PointerZone::HotZone), at(start, 150)),
            HoverFrame::None
        );
    }

    #[test]
    fn leaving_starts_a_grace_and_returning_inside_it_cancels_the_retreat() {
        let start = Instant::now();
        let mut machine = SidebarHoverReveal::default();
        let settled = reveal(&mut machine, start);
        assert_eq!(
            machine.step(input(PointerZone::Away), settled),
            HoverFrame::At(settled + GRACE)
        );
        // Comes back just inside the grace: still fully out, no travel.
        machine.step(
            input(PointerZone::Panel),
            at(settled, SIDEBAR_HOVER_GRACE_MS - 1),
        );
        assert_eq!(
            machine.progress(at(settled, SIDEBAR_HOVER_GRACE_MS - 1)),
            Some(1.0)
        );
        assert!(!machine.needs_frames());
    }

    #[test]
    fn the_grace_expiring_retreats_and_the_panel_leaves() {
        let start = Instant::now();
        let mut machine = SidebarHoverReveal::default();
        let settled = reveal(&mut machine, start);
        machine.step(input(PointerZone::Away), settled);
        let mut now = at(settled, SIDEBAR_HOVER_GRACE_MS);
        machine.step(input(PointerZone::Away), now);
        assert!(machine.is_presented(), "retreat should be animated");
        for _ in 0..200 {
            let _ = machine.progress(now);
            now += Duration::from_millis(16);
            machine.step(input(PointerZone::Away), now);
            if !machine.is_presented() {
                return;
            }
        }
        panic!("panel never left");
    }

    #[test]
    fn a_pointer_that_comes_back_mid_retreat_departs_from_where_the_panel_is() {
        let start = Instant::now();
        let mut machine = SidebarHoverReveal::default();
        let settled = reveal(&mut machine, start);
        machine.step(input(PointerZone::Away), settled);
        let mut now = at(settled, SIDEBAR_HOVER_GRACE_MS);
        machine.step(input(PointerZone::Away), now);
        // Run part of the retreat.
        for _ in 0..4 {
            let _ = machine.progress(now);
            now += Duration::from_millis(16);
            machine.step(input(PointerZone::Away), now);
        }
        let mid = machine.progress(now).unwrap();
        assert!(mid < 1.0, "retreat should have moved, got {mid}");
        // Return: the reveal departs from `mid`, monotonically upward.
        machine.step(input(PointerZone::HotZone), now);
        let mut last = machine.progress(now).unwrap();
        assert!((last - mid).abs() < 0.05, "no jump on reversal");
        for _ in 0..200 {
            now += Duration::from_millis(16);
            machine.step(input(PointerZone::HotZone), now);
            let p = machine.progress(now).unwrap();
            assert!(p >= last - 0.001, "progress went backwards: {p} < {last}");
            last = p;
            if p >= 1.0 && !machine.needs_frames() {
                return;
            }
        }
        panic!("panel never re-settled");
    }

    #[test]
    fn leaving_during_the_arrival_still_lets_it_arrive_before_the_grace_runs_out() {
        let start = Instant::now();
        let mut machine = SidebarHoverReveal::default();
        machine.step(input(PointerZone::HotZone), start);
        let mut now = at(start, SIDEBAR_HOVER_DWELL_MS);
        machine.step(input(PointerZone::HotZone), now);
        let _ = machine.progress(now); // present the opening frame
        now += Duration::from_millis(16);
        machine.step(input(PointerZone::HotZone), now);
        // Leave mid-arrival; the grace (400ms) outlasts the travel (220ms).
        machine.step(input(PointerZone::Away), now);
        let mut reached_full = false;
        for _ in 0..200 {
            let _ = machine.progress(now);
            now += Duration::from_millis(16);
            machine.step(input(PointerZone::Away), now);
            match machine.progress(now) {
                Some(p) if p >= 1.0 => reached_full = true,
                Some(_) if !reached_full => {}
                _ => break,
            }
        }
        assert!(
            reached_full,
            "panel should finish arriving before it retreats"
        );
    }

    #[test]
    fn a_drag_out_of_the_panel_pins_it_open() {
        let start = Instant::now();
        let mut machine = SidebarHoverReveal::default();
        let settled = reveal(&mut machine, start);
        assert_eq!(
            machine.step(pinned(PointerZone::Away), at(settled, 50)),
            HoverFrame::None
        );
        assert_eq!(machine.progress(at(settled, 50)), Some(1.0));
    }

    #[test]
    fn collapsing_by_hand_does_not_re_reveal_under_the_pointer() {
        let start = Instant::now();
        let mut machine = SidebarHoverReveal::default();
        machine.suppress_until_pointer_leaves();
        for ms in (0..1000).step_by(100) {
            assert_eq!(
                machine.step(input(PointerZone::HotZone), at(start, ms)),
                HoverFrame::None
            );
            assert_eq!(machine.progress(at(start, ms)), None);
        }
        // Leaving releases the suppression...
        machine.step(input(PointerZone::Away), at(start, 1100));
        // ...and the strip arms again.
        assert_eq!(
            machine.step(input(PointerZone::HotZone), at(start, 1200)),
            HoverFrame::At(at(start, 1200 + SIDEBAR_HOVER_DWELL_MS))
        );
    }

    #[test]
    fn becoming_ineligible_hides_immediately() {
        let start = Instant::now();
        let mut machine = SidebarHoverReveal::default();
        let settled = reveal(&mut machine, start);
        let gone = HoverInput {
            eligible: false,
            ..input(PointerZone::Panel)
        };
        // One erasing frame is owed, then nothing.
        assert_eq!(machine.step(gone, at(settled, 10)), HoverFrame::Now);
        assert_eq!(machine.progress(at(settled, 10)), None);
        assert_eq!(machine.step(gone, at(settled, 20)), HoverFrame::None);
    }

    #[test]
    fn settling_lands_a_travelling_panel_instead_of_deleting_it() {
        let start = Instant::now();
        let mut machine = SidebarHoverReveal::default();
        machine.step(input(PointerZone::HotZone), start);
        machine.step(
            input(PointerZone::HotZone),
            at(start, SIDEBAR_HOVER_DWELL_MS),
        );
        assert!(machine.is_presented());
        machine.settle_immediately();
        assert_eq!(machine.progress(at(start, 300)), Some(1.0));
        assert!(!machine.needs_frames());

        // A retreat settles to gone.
        machine.step(input(PointerZone::Away), at(start, 400));
        machine.step(
            input(PointerZone::Away),
            at(start, 400 + SIDEBAR_HOVER_GRACE_MS),
        );
        machine.settle_immediately();
        assert_eq!(machine.progress(at(start, 900)), None);
    }

    #[test]
    fn only_a_pending_deadline_or_a_travelling_panel_owes_frames() {
        let start = Instant::now();
        let mut machine = SidebarHoverReveal::default();
        assert!(!machine.needs_frames()); // Hidden
        machine.step(input(PointerZone::HotZone), start);
        assert!(machine.needs_frames()); // Arming
        machine.step(
            input(PointerZone::HotZone),
            at(start, SIDEBAR_HOVER_DWELL_MS),
        );
        assert!(machine.needs_frames()); // Revealing
        let settled = reveal(&mut machine, at(start, 1000));
        assert!(!machine.needs_frames()); // Revealed
        machine.step(input(PointerZone::Away), settled);
        assert!(machine.needs_frames()); // Departing
        machine.step(
            input(PointerZone::Away),
            at(settled, SIDEBAR_HOVER_GRACE_MS),
        );
        assert!(machine.needs_frames()); // Retreating
        machine.suppress_until_pointer_leaves();
        assert!(!machine.needs_frames()); // Suppressed
    }

    #[test]
    fn the_hot_zone_excludes_the_tab_bar_row() {
        assert_eq!(hot_zone(0, 0, 800, 6, 52), Some((0, 52, 6, 748)));
        assert_eq!(hot_zone(0, 0, 40, 6, 52), None);
        assert_eq!(hot_zone(0, 0, 800, 0, 52), None);
        // No chrome: the strip spans the full height.
        assert_eq!(hot_zone(0, 10, 700, 6, 0), Some((0, 10, 6, 700)));
    }
}
