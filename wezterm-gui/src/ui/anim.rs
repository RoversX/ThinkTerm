//! The timing half of an animation: given a start value, an end value and a
//! duration, what should be drawn at this instant.
//!
//! This exists because the arithmetic had been written four times over --
//! ease-out cubic in the Space swipe settle, `t*t*(3-2t)` in onboarding and
//! again, byte for byte, in the SSH host list, plus the exponential friction
//! in [`crate::ui::state::ScrollState`]. Three of those are the same idea with
//! different constants; the fourth is genuinely different (it is a physical
//! response to a finger, not a scripted transition) and deliberately stays
//! where it is.
//!
//! Two behaviours are folded in here rather than left to each caller, because
//! both are invisible until they are wrong and both were only discovered by
//! shipping the Space swipe: the clock does not start until the opening frame
//! has been presented, and retargeting mid-flight departs from where the thing
//! currently *is*. See [`Timeline::advance`] and [`Timeline::retarget`].

use std::cell::Cell;
use std::time::{Duration, Instant};

/// Hover, press and other feedback that only needs the hard edge taken off.
pub(crate) const MICRO: Duration = Duration::from_millis(120);
/// Short travel and opacity: cards closing the gap, chrome fading out.
pub(crate) const SHORT: Duration = Duration::from_millis(180);
/// Page-level movement. Matches the Space swipe settle, which is the one
/// duration in the app that has actually been tuned against a finger.
pub(crate) const STANDARD: Duration = Duration::from_millis(220);
/// Travel across most of the window, where the eye needs longer to follow.
pub(crate) const LONG: Duration = Duration::from_millis(300);

/// How long a timeline will wait for its opening frame before giving up and
/// timing from the present. Without a bound, a timeline attached to something
/// that is never painted -- a card scrolled out of the viewport, say -- would
/// keep asking for frames forever.
const PRESENT_GRACE: Duration = Duration::from_millis(120);

/// How far past its target a move that settles goes, as a share of its
/// way...
pub(crate) const OVERSHOOT: f32 = 0.0125;
/// ...and never further than this, at the design scale: a long way's share
/// would be a lurch.
pub(crate) const MOST_OVERSHOOT: f32 = 4.0;

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Easing {
    /// Decelerate into the target. The default for anything that moves: the
    /// destination is already on screen, so slowing into it reads as the thing
    /// arriving. Accelerating away from the start reads as a stall instead.
    OutCubic,
    /// `OutCubic` that goes a little past the target and comes back, as a
    /// sidebar set sliding does: see [`settle`]. Made by [`Self::settling`].
    Settle(f32),
    /// Symmetric. For opacity, where there is no position for the eye to
    /// track and a hard start is what looks wrong.
    Smooth,
    Linear,
}

impl Easing {
    /// Settling at most `overshoot` of the way past the target.
    pub(crate) fn settling(overshoot: f32) -> Self {
        Self::Settle(tension(overshoot))
    }

    pub(crate) fn apply(self, t: f32) -> f32 {
        let t = t.clamp(0.0, 1.0);
        match self {
            Self::OutCubic => 1.0 - (1.0 - t).powi(3),
            Self::Settle(tension) => settle(t, tension),
            Self::Smooth => t * t * (3.0 - 2.0 * t),
            Self::Linear => t,
        }
    }
}

/// From 0 to 1: quick, then a little past 1 and back. A back-out curve, its
/// tension from `tension`; none is a plain cubic ease out.
pub(crate) fn settle(t: f32, tension: f32) -> f32 {
    let u = t.clamp(0.0, 1.0) - 1.0;
    1.0 + (tension + 1.0) * u.powi(3) + tension * u.powi(2)
}

/// The tension that takes `settle` `overshoot` past 1 at most. It goes
/// 4s³/27(s+1)² past, which grows with s.
pub(crate) fn tension(overshoot: f32) -> f32 {
    let past = |s: f32| 4.0 * s.powi(3) / (27.0 * (s + 1.0).powi(2));
    let (mut low, mut high) = (0.0f32, 4.0f32);
    for _ in 0..24 {
        let mid = (low + high) / 2.0;
        if past(mid) < overshoot {
            low = mid;
        } else {
            high = mid;
        }
    }
    low
}

/// One value travelling from `from` to `to` over `duration`.
///
/// Drive it by calling [`Self::advance`] once per frame and asking for
/// [`Self::value`] while painting.
#[derive(Debug, Clone)]
pub(crate) struct Timeline {
    from: f32,
    to: f32,
    duration: Duration,
    /// How long it was made to take, which [`Self::reshape`] stretches.
    unstretched: Duration,
    easing: Easing,
    /// When the clock actually started, which is later than construction --
    /// see [`Self::advance`]. `None` while still waiting.
    started_at: Option<Instant>,
    /// When the timeline was armed, so the wait above can be bounded.
    armed_at: Instant,
    /// Held at `from` for this long after the clock starts.
    delay: Duration,
    /// Set by [`Self::value`], because being sampled is the only evidence a
    /// timeline has that its opening frame was drawn. A `Cell` so painting can
    /// keep taking `&self`.
    presented: Cell<bool>,
    /// The frame [`Self::pace`] last saw, if it is being paced.
    paced_at: Option<Instant>,
    finished: bool,
}

impl Timeline {
    pub(crate) fn new(now: Instant, from: f32, to: f32, duration: Duration, easing: Easing) -> Self {
        Self {
            from,
            to,
            duration,
            unstretched: duration,
            easing,
            started_at: None,
            armed_at: now,
            delay: Duration::ZERO,
            presented: Cell::new(false),
            paced_at: None,
            finished: from == to || duration.is_zero(),
        }
    }

    /// A timeline that holds at `from` for `delay` before it starts.
    ///
    /// For the second half of a two-part move, where one thing has to be seen
    /// leaving before the other follows it.
    pub(crate) fn delayed(
        now: Instant,
        from: f32,
        to: f32,
        delay: Duration,
        duration: Duration,
        easing: Easing,
    ) -> Self {
        let mut timeline = Self::new(now, from, to, duration, easing);
        timeline.delay = delay;
        timeline
    }

    /// A timeline whose clock is already running.
    ///
    /// For movement that continues something already on screen, where no new
    /// content has to be drawn before it can begin -- a gesture rebounding to
    /// where it started, say. Everything else should use [`Self::new`] and let
    /// the opening frame go uncharged.
    pub(crate) fn running(
        now: Instant,
        from: f32,
        to: f32,
        duration: Duration,
        easing: Easing,
    ) -> Self {
        let mut timeline = Self::new(now, from, to, duration, easing);
        timeline.started_at = Some(now);
        timeline
    }

    /// A timeline from 0 to 1, for callers interpolating something that is not
    /// a single number -- a rectangle, a colour -- from its progress.
    pub(crate) fn progress(now: Instant, duration: Duration, easing: Easing) -> Self {
        Self::new(now, 0.0, 1.0, duration, easing)
    }

    /// A timeline that is already over, sitting at `value`.
    pub(crate) fn settled(now: Instant, value: f32) -> Self {
        Self::new(now, value, value, Duration::ZERO, Easing::Linear)
    }

    /// When the value reaches its target, or reached it: `None` while the
    /// clock has not started.
    pub(crate) fn arrives_at(&self) -> Option<Instant> {
        self.started_at
            .map(|started_at| started_at + self.delay + self.duration)
    }

    /// Advance to `now`; returns whether another frame is still wanted.
    ///
    /// Call this once per frame, before painting. The first call does not
    /// start the clock: the frame that introduces whatever is being animated
    /// is also the frame that pays for rasterising its glyphs and growing the
    /// atlas, which can run well past 100ms. Timing from before that frame
    /// spends a large part of the transition on a picture that has not been
    /// presented yet, and the movement appears to begin already half over.
    pub(crate) fn advance(&mut self, now: Instant) -> bool {
        if self.finished {
            return false;
        }

        let Some(started_at) = self.started_at else {
            if self.presented.get() || now.saturating_duration_since(self.armed_at) >= PRESENT_GRACE
            {
                self.started_at = Some(now);
            }
            return true;
        };

        if now.saturating_duration_since(started_at) >= self.delay + self.duration {
            self.finished = true;
            return false;
        }
        true
    }

    /// Let the clock have run no more than `most` since the last frame:
    /// one that took longer has the rest held back, so the travel takes
    /// longer rather than skip most of its way. A busy window or a build
    /// without optimisation can take several such frames in a row, past the
    /// opening one [`Self::advance`] already waits out.
    ///
    /// Call this once per frame painted, before `advance`.
    pub(crate) fn pace(&mut self, now: Instant, most: Duration) {
        if let (Some(started_at), Some(paced_at)) = (self.started_at.as_mut(), self.paced_at) {
            *started_at += now.saturating_duration_since(paced_at).saturating_sub(most);
        }
        self.paced_at = Some(now);
    }

    pub(crate) fn value(&self, now: Instant) -> f32 {
        self.presented.set(true);
        if self.finished {
            return self.to;
        }
        let Some(started_at) = self.started_at else {
            // Clock not running yet: hold the opening value.
            return self.from;
        };
        let elapsed = now
            .saturating_duration_since(started_at)
            .saturating_sub(self.delay);
        let t = elapsed.as_secs_f32() / self.duration.as_secs_f32().max(f32::EPSILON);
        if t >= 1.0 {
            return self.to;
        }
        self.from + (self.to - self.from) * self.easing.apply(t)
    }

    /// Ease with `easing` instead, taking `by` times as long as it was made
    /// to: for a value whose way is only known once it is set going. Only
    /// before the clock starts, so it does not jump; asked again by then, it
    /// is as if asked once.
    pub(crate) fn reshape(&mut self, easing: Easing, by: f32) {
        if self.started_at.is_some() {
            return;
        }
        self.easing = easing;
        self.duration = self.unstretched.mul_f32(by);
    }

    /// Send the value somewhere else, departing from wherever it is right now.
    ///
    /// The current position is the only correct origin for a redirection. A
    /// hand is always faster than a transition -- close three cards in a row
    /// and the second close lands while the first reflow is still travelling
    /// -- and restarting from the previous target makes the thing jump
    /// backwards before setting off again.
    pub(crate) fn retarget(&mut self, now: Instant, to: f32, duration: Duration, easing: Easing) {
        let from = self.value(now);
        *self = Self::new(now, from, to, duration, easing);
        // The content is already on screen this time, so there is nothing
        // expensive to wait for; but keeping one rule for when the clock
        // starts costs a single frame and removes a way to get this wrong.
    }

    /// Jump to the end. For keyboard-driven changes, which want to be
    /// instant, and for teardown.
    pub(crate) fn finish(&mut self) {
        self.finished = true;
    }

    pub(crate) fn is_running(&self) -> bool {
        !self.finished
    }

    pub(crate) fn target(&self) -> f32 {
        self.to
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(start: Instant, millis: u64) -> Instant {
        start + Duration::from_millis(millis)
    }

    fn armed(start: Instant) -> Timeline {
        Timeline::new(start, 0.0, 100.0, Duration::from_millis(200), Easing::Linear)
    }

    #[test]
    fn the_clock_waits_for_the_frame_that_carries_the_new_content() {
        let start = Instant::now();
        let mut timeline = armed(start);

        // The opening frame: sampled, so it was drawn, but it must not have
        // consumed any of the transition.
        assert!(timeline.advance(at(start, 0)));
        assert_eq!(timeline.value(at(start, 0)), 0.0);

        // That frame took 100ms to rasterise. Timing starts from its end, so
        // the next frame is at the beginning of the travel, not halfway.
        assert!(timeline.advance(at(start, 100)));
        assert_eq!(timeline.value(at(start, 100)), 0.0);

        assert_eq!(timeline.value(at(start, 200)), 50.0);
        assert!(!timeline.advance(at(start, 300)));
        assert_eq!(timeline.value(at(start, 300)), 100.0);
    }

    #[test]
    fn a_timeline_nobody_paints_still_starts_rather_than_asking_for_frames_forever() {
        let start = Instant::now();
        let mut timeline = armed(start);

        // Never sampled -- the card it belongs to is scrolled out of view.
        assert!(timeline.advance(at(start, 0)));
        assert!(timeline.advance(at(start, 60)));
        // Past the grace period the clock starts anyway.
        assert!(timeline.advance(at(start, 130)));
        assert!(!timeline.advance(at(start, 331)));
    }

    #[test]
    fn redirecting_departs_from_where_the_value_is_now() {
        let start = Instant::now();
        let mut timeline = armed(start);
        // A frame is advance-then-paint, so follow that order throughout.
        timeline.advance(at(start, 0));
        assert_eq!(timeline.value(at(start, 0)), 0.0);
        timeline.advance(at(start, 10));
        assert_eq!(timeline.value(at(start, 110)), 50.0);

        // A second change arrives mid-flight and wants to go back to 0.
        timeline.retarget(
            at(start, 110),
            0.0,
            Duration::from_millis(200),
            Easing::Linear,
        );

        // No jump: it leaves from where it was seen last.
        timeline.advance(at(start, 110));
        assert_eq!(timeline.value(at(start, 110)), 50.0);
        timeline.advance(at(start, 120));
        assert_eq!(timeline.value(at(start, 220)), 25.0);
        assert!(!timeline.advance(at(start, 321)));
    }

    #[test]
    fn a_timeline_that_goes_nowhere_never_asks_for_a_frame() {
        let start = Instant::now();
        let mut timeline = Timeline::new(start, 12.0, 12.0, STANDARD, Easing::OutCubic);
        assert!(!timeline.advance(start));
        assert_eq!(timeline.value(at(start, 500)), 12.0);
    }

    #[test]
    fn arrival_is_known_once_the_clock_runs() {
        let start = Instant::now();
        let mut timeline = armed(start);
        timeline.advance(at(start, 0));
        timeline.value(at(start, 0));
        assert_eq!(timeline.arrives_at(), None);
        // The clock starts on the frame after the first presented one.
        timeline.advance(at(start, 100));
        assert_eq!(timeline.arrives_at(), Some(at(start, 300)));
    }

    #[test]
    fn the_tension_found_goes_as_far_past_as_asked() {
        assert_eq!(tension(0.0), 0.0);
        for overshoot in [0.01, OVERSHOOT, 0.1] {
            let tension = tension(overshoot);
            let top = (0..=1000)
                .map(|step| settle(step as f32 / 1000.0, tension))
                .fold(0.0, f32::max);
            assert!(
                (top - 1.0 - overshoot).abs() < 1e-3,
                "{} went {} past",
                overshoot,
                top - 1.0
            );
        }
    }

    #[test]
    fn settling_goes_a_little_past_and_lands_on_the_target() {
        let start = Instant::now();
        let mut timeline = Timeline::new(
            start,
            100.0,
            0.0,
            Duration::from_millis(200),
            Easing::settling(OVERSHOOT),
        );
        timeline.advance(at(start, 0));
        timeline.value(at(start, 0));
        timeline.advance(at(start, 0));
        let seen: Vec<f32> = (0..=200)
            .map(|millis| timeline.value(at(start, millis)))
            .collect();
        let lowest = seen.iter().copied().fold(f32::MAX, f32::min);
        assert!(
            lowest < 0.0 && lowest >= -100.0 * OVERSHOOT - 1e-3,
            "{}",
            lowest
        );
        assert_eq!(seen.last(), Some(&0.0));
    }

    #[test]
    fn reshaped_before_it_starts_it_takes_as_much_longer_and_no_more() {
        let start = Instant::now();
        let mut timeline = armed(start);
        timeline.reshape(Easing::OutCubic, 1.5);
        // Asked again before it starts -- its picture recorded again -- it
        // is not stretched twice.
        timeline.reshape(Easing::OutCubic, 1.5);
        timeline.advance(at(start, 0));
        timeline.value(at(start, 0));
        timeline.advance(at(start, 0));
        assert_eq!(timeline.arrives_at(), Some(at(start, 300)));
        assert!(timeline.value(at(start, 200)) < 100.0);
        // Under way, it is left as it goes.
        let before = timeline.value(at(start, 100));
        timeline.reshape(Easing::Linear, 3.0);
        assert_eq!(timeline.value(at(start, 100)), before);
        assert_eq!(timeline.arrives_at(), Some(at(start, 300)));
    }

    #[test]
    fn paced_a_slow_frame_moves_it_no_further_than_a_quick_one() {
        let start = Instant::now();
        let mut timeline = armed(start);
        let frame = |timeline: &mut Timeline, millis| {
            timeline.pace(at(start, millis), Duration::from_millis(20));
            timeline.advance(at(start, millis));
            timeline.value(at(start, millis))
        };
        let near = |value: f32, expected: f32| (value - expected).abs() < 1e-3;
        assert_eq!(frame(&mut timeline, 0), 0.0);
        assert_eq!(frame(&mut timeline, 10), 0.0);
        // Two frames of 100ms go 20ms each...
        assert!(near(frame(&mut timeline, 110), 10.0));
        assert!(near(frame(&mut timeline, 210), 20.0));
        // ...and quick ones after them carry on from there.
        assert!(near(frame(&mut timeline, 220), 25.0));
        assert_eq!(timeline.arrives_at(), Some(at(start, 370)));
    }

    #[test]
    fn finishing_early_lands_on_the_target() {
        let start = Instant::now();
        let mut timeline = armed(start);
        timeline.advance(start);
        timeline.finish();
        assert!(!timeline.advance(at(start, 1)));
        assert_eq!(timeline.value(at(start, 1)), 100.0);
    }

    #[test]
    fn movement_decelerates_and_opacity_is_symmetric() {
        // Ease-out is past halfway at the halfway point; smoothstep is not.
        assert!(Easing::OutCubic.apply(0.5) > 0.8);
        assert_eq!(Easing::Smooth.apply(0.5), 0.5);
        // Both are pinned at the ends, so nothing overshoots.
        for easing in [Easing::OutCubic, Easing::Smooth, Easing::Linear] {
            assert_eq!(easing.apply(0.0), 0.0);
            assert_eq!(easing.apply(1.0), 1.0);
            assert_eq!(easing.apply(-1.0), 0.0);
            assert_eq!(easing.apply(2.0), 1.0);
        }
    }
}
