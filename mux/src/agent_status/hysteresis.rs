//! Transition hysteresis for published agent states.
//!
//! TUI agents redraw their screens non-atomically, and an evaluation that
//! lands mid-redraw sees a frame with no matching rule. Publishing that
//! transient (Blocked → Idle → Blocked within 300ms) makes every consumer
//! misfire: the GUI replays its needs-input sound on each re-entry and
//! announces premature completion. Modeled on herdr's pending-idle
//! confirmation (https://github.com/herdrdev/herdr, src/pane/agent_detection.rs),
//! extended to
//! also guard leaving Blocked, which herdr's fixed 300ms sampling merely
//! makes unlikely to flap.
//!
//! Only two transitions require confirmation:
//! - **Blocked → anything**: a waiting form disappearing for one frame is
//!   almost always a redraw, and "still waiting" is the state whose flap
//!   is loudest.
//! - **Working → Idle**: the gap between a turn ending on screen and the
//!   next signal appearing, herdr's original case.
//!
//! Everything else publishes immediately — in particular `* → Blocked`,
//! because entering Blocked is the alert the user is waiting on, and
//! contract-driven transitions, which are explicit reports rather than
//! screen guesses.

use std::time::{Duration, Instant};
use thinkterm_proto::{AgentEvidence, AgentState};

/// Consistent observations required before a guarded transition is
/// published. With the 150ms evaluation cadence this is ~450ms of
/// stability.
pub(super) const RECHECK_CONFIRMATIONS: u8 = 3;
/// Hard ceiling on how long a guarded transition may be held when
/// evaluations are sparse (a quiet pane falls back to the safety tick).
pub(super) const HOLD_CAP: Duration = Duration::from_millis(700);

#[derive(Default)]
pub(super) struct Pending {
    /// The state the observations are converging on.
    target: Option<AgentState>,
    /// When the guarded transition was first observed. Deliberately kept
    /// across target flips so a flapping pane still resolves within
    /// [`HOLD_CAP`].
    first_seen: Option<Instant>,
    confirmations: u8,
}

impl Pending {
    /// Whether a guarded transition is currently being confirmed. Open
    /// panes are re-marked dirty by the drain so confirmation does not
    /// stall waiting for output.
    pub(super) fn is_open(&self) -> bool {
        self.first_seen.is_some()
    }

    fn clear(&mut self) {
        *self = Default::default();
    }

    /// Decide whether to keep publishing `published` instead of `raw`.
    /// Call once per evaluation; `now` is passed in for testability.
    pub(super) fn should_hold(
        &mut self,
        published: AgentState,
        raw: AgentState,
        evidence: AgentEvidence,
        now: Instant,
    ) -> bool {
        if raw == published || evidence == AgentEvidence::Contract {
            self.clear();
            return false;
        }
        let guarded = published == AgentState::Blocked
            || (published == AgentState::Working && raw == AgentState::Idle);
        if !guarded {
            self.clear();
            return false;
        }
        let Some(first_seen) = self.first_seen else {
            self.target = Some(raw);
            self.first_seen = Some(now);
            self.confirmations = 0;
            return true;
        };
        if now.duration_since(first_seen) >= HOLD_CAP {
            self.clear();
            return false;
        }
        if self.target != Some(raw) {
            self.target = Some(raw);
            self.confirmations = 0;
            return true;
        }
        self.confirmations = self.confirmations.saturating_add(1);
        if self.confirmations >= RECHECK_CONFIRMATIONS {
            self.clear();
            return false;
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hold(p: &mut Pending, published: AgentState, raw: AgentState, now: Instant) -> bool {
        p.should_hold(published, raw, AgentEvidence::Screen, now)
    }

    #[test]
    fn same_state_never_holds() {
        let mut p = Pending::default();
        let now = Instant::now();
        assert!(!hold(&mut p, AgentState::Blocked, AgentState::Blocked, now));
        assert!(!p.is_open());
    }

    #[test]
    fn entering_blocked_publishes_immediately() {
        let mut p = Pending::default();
        let now = Instant::now();
        assert!(!hold(&mut p, AgentState::Working, AgentState::Blocked, now));
        assert!(!hold(&mut p, AgentState::Idle, AgentState::Blocked, now));
    }

    #[test]
    fn leaving_blocked_needs_confirmations() {
        let mut p = Pending::default();
        let now = Instant::now();
        assert!(hold(&mut p, AgentState::Blocked, AgentState::Idle, now), "opens");
        assert!(hold(&mut p, AgentState::Blocked, AgentState::Idle, now));
        assert!(hold(&mut p, AgentState::Blocked, AgentState::Idle, now));
        assert!(
            !hold(&mut p, AgentState::Blocked, AgentState::Idle, now),
            "third consistent recheck releases"
        );
        assert!(!p.is_open());
    }

    #[test]
    fn returning_to_published_cancels_the_hold() {
        let mut p = Pending::default();
        let now = Instant::now();
        assert!(hold(&mut p, AgentState::Blocked, AgentState::Idle, now));
        // The redraw finished and the form is back: nothing to publish,
        // nothing pending.
        assert!(!hold(&mut p, AgentState::Blocked, AgentState::Blocked, now));
        assert!(!p.is_open());
    }

    #[test]
    fn working_to_idle_is_guarded_but_working_to_blocked_is_not() {
        let mut p = Pending::default();
        let now = Instant::now();
        assert!(hold(&mut p, AgentState::Working, AgentState::Idle, now));
        assert!(!hold(&mut p, AgentState::Working, AgentState::Blocked, now));
    }

    #[test]
    fn target_flips_keep_the_original_window() {
        let mut p = Pending::default();
        let t0 = Instant::now();
        assert!(hold(&mut p, AgentState::Blocked, AgentState::Idle, t0));
        assert!(hold(&mut p, AgentState::Blocked, AgentState::Working, t0));
        // The cap is measured from the first observation, not the flip.
        assert!(!hold(
            &mut p,
            AgentState::Blocked,
            AgentState::Working,
            t0 + HOLD_CAP
        ));
    }

    #[test]
    fn hold_releases_at_cap() {
        let mut p = Pending::default();
        let t0 = Instant::now();
        assert!(hold(&mut p, AgentState::Blocked, AgentState::Idle, t0));
        assert!(!hold(
            &mut p,
            AgentState::Blocked,
            AgentState::Idle,
            t0 + HOLD_CAP
        ));
        assert!(!p.is_open());
    }

    #[test]
    fn contract_transitions_bypass_hysteresis() {
        let mut p = Pending::default();
        let now = Instant::now();
        assert!(!p.should_hold(
            AgentState::Blocked,
            AgentState::Idle,
            AgentEvidence::Contract,
            now
        ));
        assert!(!p.is_open());
    }
}
