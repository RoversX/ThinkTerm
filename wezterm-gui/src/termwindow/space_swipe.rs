use crate::ui::anim::{Easing, Timeline};
use std::time::{Duration, Instant};

const HORIZONTAL_LOCK_DISTANCE: f32 = 8.0;
const VERTICAL_LOCK_DISTANCE: f32 = 18.0;
const AXIS_LOCK_RATIO: f32 = 1.15;
const AXIS_LOCK_FALLBACK_DISTANCE: f32 = 32.0;
const EDGE_RESISTANCE: f32 = 0.25;
const EDGE_MAX_OFFSET: f32 = 48.0;
const COMMIT_DISTANCE_RATIO: f32 = 0.28;
const COMMIT_MIN_FLING_DISTANCE: f32 = 16.0;
const COMMIT_VELOCITY: f32 = 600.0;
const SETTLE_DURATION: Duration = Duration::from_millis(220);

pub(crate) fn sidebar_page_push_offsets(
    visual_offset: f32,
    gesture_extent: f32,
    page_width: f32,
    direction: f32,
) -> (f32, f32) {
    if gesture_extent <= 0.0 || page_width <= 0.0 || direction == 0.0 {
        return (0.0, 0.0);
    }

    let direction = direction.signum();
    let progress = (visual_offset.abs() / gesture_extent).clamp(0.0, 1.0);
    let source = direction * page_width * progress;
    let target = source - direction * page_width;
    (source, target)
}

/// Atlas recreation invalidates captured quad UVs, but a fast committed flick
/// can still be waiting for its first source capture and therefore has no
/// source frame to invalidate. Keep that semantic commit alive so the retry
/// can capture the source and finish the switch; any cached neighbour is
/// discarded and repainted separately.
pub(crate) fn preserve_pending_source_capture_after_atlas_recreation(
    source_frame_captured: bool,
    capture_source: bool,
    pending_commit: bool,
) -> bool {
    !source_frame_captured && capture_source && pending_commit
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum SidebarSpaceSwipeUpdate {
    Pending,
    Horizontal,
    Vertical(f32),
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum SidebarSpaceSwipeFinish {
    None,
    Switch(String),
    AnimateBack,
    /// The gesture ended before either axis locked, so its vertical travel was
    /// only ever accumulated. Scroll by it now: the state machine consumed the
    /// events, so nothing else will.
    FlushVertical(f32),
}

/// Where a committed transition should begin its travel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SettleOpening {
    /// The pages were already following the finger, so carry on from where it
    /// let go. Restarting at rest would snap them backwards first.
    WhereTheFingerLeftIt,
    /// Nothing was ever composited for this gesture -- a flick that began and
    /// ended inside a single frame -- so the pages are still sitting at rest
    /// no matter what the gesture's arithmetic says. Travelling the whole way
    /// is what makes the switch visible rather than an instant cut.
    AtRest,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SidebarSpaceSwipeVisual {
    pub(crate) source_space_id: String,
    pub(crate) target_space_id: Option<String>,
    pub(crate) source_scroll_offset: f32,
    pub(crate) offset: f32,
}

#[derive(Debug, Clone)]
struct Gesture {
    source_space_id: String,
    previous_space_id: Option<String>,
    next_space_id: Option<String>,
    source_scroll_offset: f32,
    raw_x: f32,
    raw_y: f32,
    velocity_x: f32,
    last_sample_at: Instant,
}

impl Gesture {
    fn new(
        source_space_id: String,
        previous_space_id: Option<String>,
        next_space_id: Option<String>,
        source_scroll_offset: f32,
        now: Instant,
    ) -> Self {
        Self {
            source_space_id,
            previous_space_id,
            next_space_id,
            source_scroll_offset,
            raw_x: 0.0,
            raw_y: 0.0,
            velocity_x: 0.0,
            last_sample_at: now,
        }
    }

    fn update(&mut self, delta_x: f32, delta_y: f32, now: Instant) {
        let dt = now
            .saturating_duration_since(self.last_sample_at)
            .as_secs_f32()
            .clamp(1.0 / 240.0, 1.0 / 20.0);
        self.last_sample_at = now;
        self.raw_x += delta_x;
        self.raw_y += delta_y;

        let instantaneous = delta_x / dt;
        self.velocity_x = self.velocity_x * 0.65 + instantaneous * 0.35;
    }

    fn target_space_id(&self) -> Option<&String> {
        if self.raw_x < 0.0 {
            self.next_space_id.as_ref()
        } else if self.raw_x > 0.0 {
            self.previous_space_id.as_ref()
        } else {
            None
        }
    }

    fn display_offset(&self, sidebar_width: f32) -> f32 {
        if self.raw_x < 0.0 {
            if self.next_space_id.is_some() {
                self.raw_x.max(-sidebar_width)
            } else {
                (self.raw_x * EDGE_RESISTANCE).max(-EDGE_MAX_OFFSET)
            }
        } else if self.raw_x > 0.0 {
            if self.previous_space_id.is_some() {
                self.raw_x.min(sidebar_width)
            } else {
                (self.raw_x * EDGE_RESISTANCE).min(EDGE_MAX_OFFSET)
            }
        } else {
            0.0
        }
    }

    fn target_offset(&self, sidebar_width: f32) -> f32 {
        if self.raw_x < 0.0 {
            -sidebar_width
        } else {
            sidebar_width
        }
    }

    fn should_commit(&self, sidebar_width: f32) -> bool {
        if self.target_space_id().is_none() {
            return false;
        }

        let distance_commit =
            self.display_offset(sidebar_width).abs() >= sidebar_width * COMMIT_DISTANCE_RATIO;
        let velocity_toward_target = if self.raw_x < 0.0 {
            -self.velocity_x
        } else {
            self.velocity_x
        };
        let fling_commit = self.raw_x.abs() >= COMMIT_MIN_FLING_DISTANCE
            && velocity_toward_target >= COMMIT_VELOCITY;
        distance_commit || fling_commit
    }
}

#[derive(Debug, Clone)]
struct Settle {
    source_space_id: String,
    target_space_id: Option<String>,
    source_scroll_offset: f32,
    /// The sidebar offset travelling to its resting place. A committed switch
    /// leaves its opening frame uncharged, because that frame also adopts the
    /// destination Space and pays for its glyph rasterisation and atlas
    /// growth -- routinely 100ms+, roughly half the transition.
    travel: Timeline,
    committed: bool,
}

#[derive(Debug, Clone)]
enum Phase {
    Idle,
    Candidate(Gesture),
    Vertical,
    Tracking(Gesture),
    AwaitingCommit(Gesture),
    Settling(Settle),
}

impl Default for Phase {
    fn default() -> Self {
        Self::Idle
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct SidebarSpaceSwipeState {
    phase: Phase,
    suppress_momentum: bool,
}

impl SidebarSpaceSwipeState {
    pub(crate) fn begin(
        &mut self,
        source_space_id: String,
        previous_space_id: Option<String>,
        next_space_id: Option<String>,
        source_scroll_offset: f32,
        now: Instant,
    ) -> bool {
        if !matches!(self.phase, Phase::Idle) {
            return false;
        }
        self.suppress_momentum = false;
        self.phase = Phase::Candidate(Gesture::new(
            source_space_id,
            previous_space_id,
            next_space_id,
            source_scroll_offset,
            now,
        ));
        true
    }

    pub(crate) fn update(
        &mut self,
        delta_x: f32,
        delta_y: f32,
        now: Instant,
    ) -> SidebarSpaceSwipeUpdate {
        let phase = std::mem::take(&mut self.phase);
        match phase {
            Phase::Candidate(mut gesture) => {
                gesture.update(delta_x, delta_y, now);
                let x = gesture.raw_x.abs();
                let y = gesture.raw_y.abs();
                let max_axis = x.max(y);

                if x >= HORIZONTAL_LOCK_DISTANCE && x > y * AXIS_LOCK_RATIO {
                    self.phase = Phase::Tracking(gesture);
                    SidebarSpaceSwipeUpdate::Horizontal
                } else if y >= VERTICAL_LOCK_DISTANCE && y > x * AXIS_LOCK_RATIO {
                    self.phase = Phase::Vertical;
                    SidebarSpaceSwipeUpdate::Vertical(gesture.raw_y)
                } else if max_axis >= AXIS_LOCK_FALLBACK_DISTANCE && x >= y {
                    self.phase = Phase::Tracking(gesture);
                    SidebarSpaceSwipeUpdate::Horizontal
                } else if max_axis >= AXIS_LOCK_FALLBACK_DISTANCE {
                    self.phase = Phase::Vertical;
                    SidebarSpaceSwipeUpdate::Vertical(gesture.raw_y)
                } else {
                    self.phase = Phase::Candidate(gesture);
                    SidebarSpaceSwipeUpdate::Pending
                }
            }
            Phase::Tracking(mut gesture) => {
                gesture.update(delta_x, delta_y, now);
                self.phase = Phase::Tracking(gesture);
                SidebarSpaceSwipeUpdate::Horizontal
            }
            Phase::Vertical => {
                self.phase = Phase::Vertical;
                SidebarSpaceSwipeUpdate::Vertical(delta_y)
            }
            other => {
                self.phase = other;
                SidebarSpaceSwipeUpdate::Pending
            }
        }
    }

    pub(crate) fn finish(&mut self, now: Instant, sidebar_width: f32) -> SidebarSpaceSwipeFinish {
        let phase = std::mem::take(&mut self.phase);
        match phase {
            Phase::Tracking(gesture) if gesture.should_commit(sidebar_width) => {
                self.suppress_momentum = true;
                let target = gesture.target_space_id().cloned().expect("checked above");
                self.phase = Phase::AwaitingCommit(gesture);
                SidebarSpaceSwipeFinish::Switch(target)
            }
            Phase::Tracking(gesture) => {
                self.suppress_momentum = true;
                let from = gesture.display_offset(sidebar_width);
                let target_space_id = gesture.target_space_id().cloned();
                self.phase = Phase::Settling(Settle {
                    source_space_id: gesture.source_space_id,
                    target_space_id,
                    source_scroll_offset: gesture.source_scroll_offset,
                    // The pages are already where the finger left them, so
                    // there is nothing new to draw before travelling back.
                    travel: Timeline::running(now, from, 0.0, SETTLE_DURATION, Easing::OutCubic),
                    committed: false,
                });
                SidebarSpaceSwipeFinish::AnimateBack
            }
            Phase::Candidate(gesture) => {
                // Below both lock distances the gesture is still undecided, so
                // `update` has been swallowing every delta to keep the sidebar
                // from twitching while the axis is in doubt. A stroke that ends
                // here never reached the `Vertical` arm that flushes them, so
                // hand the accumulation over now or a gentle nudge scrolls
                // nothing at all.
                self.phase = Phase::Idle;
                self.suppress_momentum = false;
                if gesture.raw_y.abs() > f32::EPSILON {
                    SidebarSpaceSwipeFinish::FlushVertical(gesture.raw_y)
                } else {
                    SidebarSpaceSwipeFinish::None
                }
            }
            Phase::Vertical => {
                self.phase = Phase::Idle;
                self.suppress_momentum = false;
                SidebarSpaceSwipeFinish::None
            }
            other => {
                self.phase = other;
                SidebarSpaceSwipeFinish::None
            }
        }
    }

    pub(crate) fn resolve_switch(
        &mut self,
        switched: bool,
        now: Instant,
        sidebar_width: f32,
        opening: SettleOpening,
    ) {
        let phase = std::mem::take(&mut self.phase);
        match phase {
            Phase::AwaitingCommit(gesture) => {
                let from = match opening {
                    SettleOpening::WhereTheFingerLeftIt => gesture.display_offset(sidebar_width),
                    SettleOpening::AtRest => 0.0,
                };
                let target_space_id = gesture.target_space_id().cloned();
                let target_offset = gesture.target_offset(sidebar_width);
                self.phase = Phase::Settling(Settle {
                    source_space_id: gesture.source_space_id,
                    target_space_id,
                    source_scroll_offset: gesture.source_scroll_offset,
                    // A committed switch defers its clock to the next frame;
                    // a rejected one has nothing expensive to wait for.
                    travel: {
                        let to = if switched { target_offset } else { 0.0 };
                        if switched {
                            Timeline::new(now, from, to, SETTLE_DURATION, Easing::OutCubic)
                        } else {
                            Timeline::running(now, from, to, SETTLE_DURATION, Easing::OutCubic)
                        }
                    },
                    committed: switched,
                });
            }
            other => self.phase = other,
        }
    }

    pub(crate) fn cancel_immediately(&mut self) {
        self.phase = Phase::Idle;
        self.suppress_momentum = false;
    }

    pub(crate) fn advance(&mut self, now: Instant) -> bool {
        let phase = std::mem::take(&mut self.phase);
        let Phase::Settling(mut settle) = phase else {
            self.phase = phase;
            return false;
        };

        if !settle.travel.advance(now) {
            self.phase = Phase::Idle;
            return false;
        }

        self.phase = Phase::Settling(settle);
        true
    }

    pub(crate) fn visual(
        &self,
        now: Instant,
        sidebar_width: f32,
    ) -> Option<SidebarSpaceSwipeVisual> {
        match &self.phase {
            Phase::Tracking(gesture) | Phase::AwaitingCommit(gesture) => {
                Some(SidebarSpaceSwipeVisual {
                    source_space_id: gesture.source_space_id.clone(),
                    target_space_id: gesture.target_space_id().cloned(),
                    source_scroll_offset: gesture.source_scroll_offset,
                    offset: gesture.display_offset(sidebar_width),
                })
            }
            Phase::Settling(settle) => Some(SidebarSpaceSwipeVisual {
                source_space_id: settle.source_space_id.clone(),
                target_space_id: settle.target_space_id.clone(),
                source_scroll_offset: settle.source_scroll_offset,
                offset: settle.travel.value(now),
            }),
            _ => None,
        }
    }

    pub(crate) fn is_active(&self) -> bool {
        !matches!(self.phase, Phase::Idle)
    }

    pub(crate) fn is_committing_or_committed(&self) -> bool {
        matches!(
            &self.phase,
            Phase::AwaitingCommit(_)
                | Phase::Settling(Settle {
                    committed: true,
                    ..
                })
        )
    }

    pub(crate) fn pending_switch_target(&self) -> Option<&str> {
        match &self.phase {
            Phase::AwaitingCommit(gesture) => gesture.target_space_id().map(String::as_str),
            _ => None,
        }
    }

    pub(crate) fn consume_momentum(&mut self, ended: bool) -> bool {
        let consume = self.suppress_momentum || self.is_active();
        if ended {
            self.suppress_momentum = false;
        }
        consume
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(start: Instant, millis: u64) -> Instant {
        start + Duration::from_millis(millis)
    }

    #[test]
    fn sidebar_page_push_tiles_source_and_target_across_the_list_viewport() {
        assert_eq!(
            sidebar_page_push_offsets(-150.0, 300.0, 300.0, -1.0),
            (-150.0, 150.0)
        );
        assert_eq!(
            sidebar_page_push_offsets(300.0, 300.0, 300.0, 1.0),
            (300.0, 0.0)
        );
    }

    #[test]
    fn horizontal_gesture_tracks_then_commits_by_distance() {
        let start = Instant::now();
        let mut swipe = SidebarSpaceSwipeState::default();
        assert!(swipe.begin(
            "source".into(),
            Some("previous".into()),
            Some("next".into()),
            0.0,
            start,
        ));
        assert_eq!(
            swipe.update(-4.0, 1.0, at(start, 8)),
            SidebarSpaceSwipeUpdate::Pending
        );
        assert_eq!(
            swipe.update(-92.0, 1.0, at(start, 24)),
            SidebarSpaceSwipeUpdate::Horizontal
        );
        assert_eq!(
            swipe.visual(at(start, 24), 300.0),
            Some(SidebarSpaceSwipeVisual {
                source_space_id: "source".into(),
                target_space_id: Some("next".into()),
                source_scroll_offset: 0.0,
                offset: -96.0,
            })
        );
        assert_eq!(
            swipe.finish(at(start, 30), 300.0),
            SidebarSpaceSwipeFinish::Switch("next".into())
        );
        swipe.resolve_switch(
            true,
            at(start, 30),
            300.0,
            SettleOpening::WhereTheFingerLeftIt,
        );
        // The frame that commits renders the opening position -- the finger's
        // own -96 -- and `advance`, run at the top of each paint, starts the
        // clock on the frame after it.
        assert_eq!(swipe.visual(at(start, 30), 300.0).unwrap().offset, -96.0);
        assert!(swipe.advance(at(start, 100)));
        assert!(swipe.visual(at(start, 200), 300.0).unwrap().offset < -96.0);
        assert!(!swipe.advance(at(start, 340)));
    }

    #[test]
    fn vertical_axis_stays_vertical_for_the_whole_gesture() {
        let start = Instant::now();
        let mut swipe = SidebarSpaceSwipeState::default();
        swipe.begin("source".into(), None, Some("next".into()), 0.0, start);
        assert_eq!(
            swipe.update(2.0, -20.0, at(start, 16)),
            SidebarSpaceSwipeUpdate::Vertical(-20.0)
        );
        assert_eq!(
            swipe.update(-40.0, 1.0, at(start, 32)),
            SidebarSpaceSwipeUpdate::Vertical(1.0)
        );
        assert_eq!(swipe.visual(at(start, 32), 300.0), None);
        assert_eq!(
            swipe.finish(at(start, 40), 300.0),
            SidebarSpaceSwipeFinish::None
        );
    }

    #[test]
    fn the_pages_report_an_offset_while_the_finger_is_still_down() {
        // The whole point of following the finger: an offset exists from the
        // moment the axis locks, not only once the gesture has been released.
        let start = Instant::now();
        let mut swipe = SidebarSpaceSwipeState::default();
        swipe.begin("source".into(), None, Some("next".into()), 0.0, start);
        assert_eq!(
            swipe.update(-20.0, 0.0, at(start, 16)),
            SidebarSpaceSwipeUpdate::Horizontal
        );
        assert_eq!(swipe.visual(at(start, 16), 300.0).unwrap().offset, -20.0);
        swipe.update(-30.0, 0.0, at(start, 32));
        assert_eq!(swipe.visual(at(start, 32), 300.0).unwrap().offset, -50.0);

        // Reversing under the finger walks it back, rather than latching.
        swipe.update(45.0, 0.0, at(start, 48));
        assert_eq!(swipe.visual(at(start, 48), 300.0).unwrap().offset, -5.0);
    }

    #[test]
    fn reversing_past_the_start_retargets_the_neighbour_on_the_other_side() {
        // A drag that crosses back over its origin is now approaching the
        // *other* neighbour. The renderer captures whichever Space this
        // reports, so getting it wrong composites a page against the Space it
        // is sliding away from.
        let start = Instant::now();
        let mut swipe = SidebarSpaceSwipeState::default();
        swipe.begin(
            "source".into(),
            Some("previous".into()),
            Some("next".into()),
            0.0,
            start,
        );
        swipe.update(-40.0, 0.0, at(start, 16));
        assert_eq!(
            swipe.visual(at(start, 16), 300.0).unwrap().target_space_id,
            Some("next".into())
        );

        swipe.update(70.0, 0.0, at(start, 32));
        let visual = swipe.visual(at(start, 32), 300.0).unwrap();
        assert_eq!(visual.target_space_id, Some("previous".into()));
        assert_eq!(visual.offset, 30.0);
    }

    #[test]
    fn a_rebound_travels_back_instead_of_snapping() {
        // A gesture too short to commit has still moved the pages, so it owes
        // them a trip home. Before the pages tracked the finger there was
        // nothing on screen to return and the settle was simply cancelled.
        let start = Instant::now();
        let mut swipe = SidebarSpaceSwipeState::default();
        swipe.begin("source".into(), None, Some("next".into()), 0.0, start);
        swipe.update(-20.0, 0.0, at(start, 16));
        assert_eq!(
            swipe.finish(at(start, 24), 300.0),
            SidebarSpaceSwipeFinish::AnimateBack
        );

        // Still animating, and starting from where the finger let go.
        assert_eq!(swipe.visual(at(start, 24), 300.0).unwrap().offset, -20.0);
        assert!(swipe.advance(at(start, 30)));
        let midway = swipe.visual(at(start, 130), 300.0).unwrap().offset;
        assert!(
            midway > -20.0 && midway < 0.0,
            "expected travel back toward rest, got {}",
            midway
        );
        assert_eq!(swipe.visual(at(start, 300), 300.0).unwrap().offset, 0.0);
    }

    #[test]
    fn a_nudge_too_small_to_lock_an_axis_still_scrolls_by_what_it_travelled() {
        // Under both lock distances `update` reports Pending and keeps the
        // deltas to itself, so the sidebar cannot twitch while the axis is
        // undecided. Nothing downstream sees those events -- the swipe handler
        // has already claimed them -- so ending here has to hand them back.
        let start = Instant::now();
        let mut swipe = SidebarSpaceSwipeState::default();
        swipe.begin("source".into(), None, Some("next".into()), 0.0, start);
        for frame in 1..=3 {
            assert_eq!(
                swipe.update(0.0, -4.0, at(start, frame * 16)),
                SidebarSpaceSwipeUpdate::Pending,
                "12 pixels is under VERTICAL_LOCK_DISTANCE, so nothing scrolls yet"
            );
        }
        assert_eq!(
            swipe.finish(at(start, 64), 300.0),
            SidebarSpaceSwipeFinish::FlushVertical(-12.0)
        );

        // A gesture that never moved has nothing to hand back, and must not
        // pass a zero scroll to the sidebar.
        let mut swipe = SidebarSpaceSwipeState::default();
        swipe.begin("source".into(), None, Some("next".into()), 0.0, start);
        assert_eq!(
            swipe.finish(at(start, 16), 300.0),
            SidebarSpaceSwipeFinish::None
        );
    }

    #[test]
    fn early_vertical_noise_does_not_steal_a_horizontal_swipe() {
        let start = Instant::now();
        let mut swipe = SidebarSpaceSwipeState::default();
        swipe.begin("source".into(), None, Some("next".into()), 0.0, start);

        assert_eq!(
            swipe.update(-1.0, 5.0, at(start, 8)),
            SidebarSpaceSwipeUpdate::Pending
        );
        assert_eq!(
            swipe.update(-2.0, 5.0, at(start, 16)),
            SidebarSpaceSwipeUpdate::Pending
        );
        assert_eq!(
            swipe.update(-12.0, 2.0, at(start, 24)),
            SidebarSpaceSwipeUpdate::Horizontal
        );
        assert_eq!(swipe.visual(at(start, 24), 300.0).unwrap().offset, -15.0);
    }

    #[test]
    fn short_slow_drag_animates_back_without_switching() {
        let start = Instant::now();
        let mut swipe = SidebarSpaceSwipeState::default();
        swipe.begin("source".into(), None, Some("next".into()), 0.0, start);
        swipe.update(-20.0, 0.0, at(start, 100));
        assert_eq!(
            swipe.finish(at(start, 200), 300.0),
            SidebarSpaceSwipeFinish::AnimateBack
        );
        let before = swipe.visual(at(start, 200), 300.0).unwrap().offset;
        let after = swipe.visual(at(start, 300), 300.0).unwrap().offset;
        assert!(before < 0.0);
        assert!(after > before);
    }

    #[test]
    fn quick_flick_commits_before_the_distance_threshold() {
        let start = Instant::now();
        let mut swipe = SidebarSpaceSwipeState::default();
        swipe.begin("source".into(), None, Some("next".into()), 0.0, start);
        swipe.update(-10.0, 0.0, at(start, 8));
        swipe.update(-10.0, 0.0, at(start, 16));
        assert_eq!(
            swipe.finish(at(start, 20), 300.0),
            SidebarSpaceSwipeFinish::Switch("next".into())
        );
    }

    #[test]
    fn unavailable_edge_is_resisted_and_never_commits() {
        let start = Instant::now();
        let mut swipe = SidebarSpaceSwipeState::default();
        swipe.begin("source".into(), None, Some("next".into()), 0.0, start);
        swipe.update(400.0, 0.0, at(start, 16));
        let visual = swipe.visual(at(start, 16), 300.0).unwrap();
        assert_eq!(visual.target_space_id, None);
        assert_eq!(visual.offset, 48.0);
        assert_eq!(
            swipe.finish(at(start, 20), 300.0),
            SidebarSpaceSwipeFinish::AnimateBack
        );
    }

    #[test]
    fn momentum_is_suppressed_after_a_completed_gesture() {
        let start = Instant::now();
        let mut swipe = SidebarSpaceSwipeState::default();
        swipe.begin("source".into(), None, Some("next".into()), 0.0, start);
        swipe.update(-20.0, 0.0, at(start, 16));
        swipe.finish(at(start, 20), 300.0);
        assert!(swipe.consume_momentum(false));
        assert!(swipe.consume_momentum(true));
        swipe.cancel_immediately();
        assert!(!swipe.consume_momentum(false));
    }

    #[test]
    fn a_rejected_switch_returns_to_the_source_and_cannot_commit_twice() {
        let start = Instant::now();
        let mut swipe = SidebarSpaceSwipeState::default();
        swipe.begin("source".into(), None, Some("next".into()), 23.0, start);
        swipe.update(-120.0, 0.0, at(start, 16));
        assert_eq!(
            swipe.finish(at(start, 20), 300.0),
            SidebarSpaceSwipeFinish::Switch("next".into())
        );
        assert_eq!(
            swipe.finish(at(start, 21), 300.0),
            SidebarSpaceSwipeFinish::None
        );

        swipe.resolve_switch(
            false,
            at(start, 24),
            300.0,
            SettleOpening::WhereTheFingerLeftIt,
        );
        let visual = swipe.visual(at(start, 100), 300.0).unwrap();
        assert_eq!(visual.source_scroll_offset, 23.0);
        assert!(visual.offset > -120.0);
        assert!(visual.offset < 0.0);
    }

    #[test]
    fn committed_settle_survives_layout_sync_until_the_animation_finishes() {
        let start = Instant::now();
        let mut swipe = SidebarSpaceSwipeState::default();
        swipe.begin("source".into(), None, Some("next".into()), 0.0, start);
        swipe.update(-120.0, 0.0, at(start, 16));
        assert_eq!(
            swipe.finish(at(start, 20), 300.0),
            SidebarSpaceSwipeFinish::Switch("next".into())
        );
        swipe.resolve_switch(
            true,
            at(start, 20),
            300.0,
            SettleOpening::WhereTheFingerLeftIt,
        );

        assert!(swipe.is_committing_or_committed());
        // The opening frame: painted at rest, and not charged to the
        // transition -- it is the frame that adopts the destination Space.
        assert!(swipe.advance(at(start, 30)));
        assert_eq!(swipe.visual(at(start, 30), 300.0).unwrap().offset, -120.0);
        assert!(swipe.is_committing_or_committed());

        // Timing runs from the frame after that one.
        assert!(swipe.advance(at(start, 46)));
        assert!(swipe.advance(at(start, 200)));
        assert!(swipe.is_committing_or_committed());
        assert!(!swipe.advance(at(start, 280)));
        assert!(!swipe.is_committing_or_committed());
    }

    #[test]
    fn awaiting_commit_is_preserved_during_the_switch_layout_sync() {
        let start = Instant::now();
        let mut swipe = SidebarSpaceSwipeState::default();
        swipe.begin("source".into(), None, Some("next".into()), 0.0, start);
        swipe.update(-120.0, 0.0, at(start, 16));
        assert_eq!(
            swipe.finish(at(start, 20), 300.0),
            SidebarSpaceSwipeFinish::Switch("next".into())
        );

        // switch_space_to_thread can synchronously cause a Resized event
        // before resolve_switch records the successful commit.
        assert!(swipe.is_committing_or_committed());
        swipe.resolve_switch(
            true,
            at(start, 20),
            300.0,
            SettleOpening::WhereTheFingerLeftIt,
        );
        assert!(swipe.is_committing_or_committed());
    }

    #[test]
    fn committed_animation_clock_starts_after_slow_space_adoption() {
        let start = Instant::now();
        let mut swipe = SidebarSpaceSwipeState::default();
        swipe.begin("source".into(), None, Some("next".into()), 0.0, start);
        swipe.update(-120.0, 0.0, at(start, 16));
        assert_eq!(
            swipe.finish(at(start, 20), 300.0),
            SidebarSpaceSwipeFinish::Switch("next".into())
        );

        // Model a destination adoption that takes much longer than the visual
        // transition. Resolution supplies a fresh clock after that work.
        let adoption_finished = at(start, 1_000);
        swipe.resolve_switch(
            true,
            adoption_finished,
            300.0,
            SettleOpening::WhereTheFingerLeftIt,
        );
        assert!(swipe.advance(adoption_finished));
        assert_eq!(
            swipe.visual(adoption_finished, 300.0).unwrap().offset,
            -120.0
        );
        // The full transition is still ahead: the second of adoption before it
        // has been spent on work, not on the animation.
        assert!(swipe.advance(at(start, 1_100)));
        assert!(swipe.advance(at(start, 1_250)));
        assert!(!swipe.advance(at(start, 1_330)));
    }

    #[test]
    fn unpainted_full_distance_swipe_still_has_a_visible_commit_animation() {
        let start = Instant::now();
        let mut swipe = SidebarSpaceSwipeState::default();
        swipe.begin("source".into(), None, Some("next".into()), 0.0, start);
        swipe.update(-500.0, 0.0, at(start, 16));
        assert_eq!(
            swipe.finish(at(start, 20), 300.0),
            SidebarSpaceSwipeFinish::Switch("next".into())
        );
        // A flick this fast is over before a frame is composited, so the pages
        // never left rest. Opening where the finger "left off" would put them
        // at the full -300 immediately and the switch would be an instant cut.
        swipe.resolve_switch(true, at(start, 1_000), 300.0, SettleOpening::AtRest);

        let first = swipe.visual(at(start, 1_000), 300.0).unwrap().offset;
        assert_eq!(first, 0.0);
        assert!(swipe.advance(at(start, 1_000)));
        let middle = swipe.visual(at(start, 1_100), 300.0).unwrap().offset;
        let last = swipe.visual(at(start, 1_220), 300.0).unwrap().offset;
        assert!(middle < first && middle > -300.0);
        assert_eq!(last, -300.0);
    }

    /// Regression: macOS routinely delivers a trailing `Ended` after momentum
    /// has already ended the gesture, so `finish` runs a second time moments
    /// after a commit. That second call has nothing to settle and returns
    /// `None` -- which the caller read as "no gesture in flight" and used to
    /// tear down the frame transition, destroying the just-committed animation
    /// before its first frame. `None` must therefore leave an in-flight settle
    /// completely untouched, and `is_active` must stay true so the caller can
    /// tell "nothing to do" apart from "nothing here".
    #[test]
    fn a_second_finish_after_committing_reports_none_without_disturbing_the_settle() {
        let start = Instant::now();
        let mut swipe = SidebarSpaceSwipeState::default();
        swipe.begin("source".into(), None, Some("next".into()), 0.0, start);
        swipe.update(-120.0, 0.0, at(start, 16));
        assert_eq!(
            swipe.finish(at(start, 20), 300.0),
            SidebarSpaceSwipeFinish::Switch("next".into())
        );

        // Trailing Ended, still in AwaitingCommit.
        assert_eq!(
            swipe.finish(at(start, 21), 300.0),
            SidebarSpaceSwipeFinish::None
        );
        assert!(swipe.is_active());
        assert!(swipe.is_committing_or_committed());
        assert_eq!(swipe.pending_switch_target(), Some("next"));

        swipe.resolve_switch(
            true,
            at(start, 22),
            300.0,
            SettleOpening::WhereTheFingerLeftIt,
        );
        assert!(swipe.advance(at(start, 30)));

        // And again once the settle is running.
        assert_eq!(
            swipe.finish(at(start, 40), 300.0),
            SidebarSpaceSwipeFinish::None
        );
        assert!(swipe.is_active());
        assert!(swipe.is_committing_or_committed());
        let mid = swipe
            .visual(at(start, 120), 300.0)
            .expect("settle still live");
        assert!(mid.offset < 0.0 && mid.offset > -300.0);

        // An idle machine is the only case where `None` really means empty.
        swipe.cancel_immediately();
        assert_eq!(
            swipe.finish(at(start, 50), 300.0),
            SidebarSpaceSwipeFinish::None
        );
        assert!(!swipe.is_active());
    }

    /// The commit frame adopts the destination Space, so it can take 100ms+ of
    /// glyph rasterisation before it reaches the screen. That cost must not be
    /// billed to the 220ms transition, or the first visible motion jumps
    /// straight to ~87% and the swipe reads as an instant cut.
    #[test]
    fn a_slow_commit_frame_does_not_consume_the_transition() {
        let start = Instant::now();
        let mut swipe = SidebarSpaceSwipeState::default();
        swipe.begin("source".into(), None, Some("next".into()), 0.0, start);
        swipe.update(-120.0, 0.0, at(start, 16));
        assert_eq!(
            swipe.finish(at(start, 20), 300.0),
            SidebarSpaceSwipeFinish::Switch("next".into())
        );
        swipe.resolve_switch(true, at(start, 20), 300.0, SettleOpening::AtRest);

        // That first frame takes 500ms to present.
        let after_slow_frame = at(start, 520);
        assert!(swipe.advance(after_slow_frame));
        assert_eq!(swipe.visual(after_slow_frame, 300.0).unwrap().offset, 0.0);

        // A full 220ms of travel is still ahead, measured from that point.
        let mid = swipe.visual(at(start, 520 + 110), 300.0).unwrap().offset;
        assert!(mid < 0.0 && mid > -300.0);
        assert!(swipe.advance(at(start, 520 + 200)));
        assert!(!swipe.advance(at(start, 520 + 240)));
    }

    #[test]
    fn a_committed_push_opens_where_the_finger_let_go() {
        let start = Instant::now();
        let mut swipe = SidebarSpaceSwipeState::default();
        swipe.begin("source".into(), None, Some("next".into()), 0.0, start);
        swipe.update(-40.0, 0.0, at(start, 16));
        swipe.update(-40.0, 0.0, at(start, 48));
        swipe.update(-40.0, 0.0, at(start, 80));
        assert_eq!(
            swipe.finish(at(start, 90), 300.0),
            SidebarSpaceSwipeFinish::Switch("next".into())
        );
        swipe.resolve_switch(
            true,
            at(start, 100),
            300.0,
            SettleOpening::WhereTheFingerLeftIt,
        );
        // The pages have been sitting at -120 under the finger. Opening the
        // settle anywhere else -- at rest, most temptingly -- yanks them
        // backwards for one frame before they travel on.
        assert_eq!(swipe.visual(at(start, 100), 300.0).unwrap().offset, -120.0);
    }

    #[test]
    fn atlas_recreation_preserves_only_a_commit_waiting_for_its_source_capture() {
        assert!(preserve_pending_source_capture_after_atlas_recreation(
            false, true, true
        ));

        assert!(!preserve_pending_source_capture_after_atlas_recreation(
            true, true, true
        ));
        assert!(!preserve_pending_source_capture_after_atlas_recreation(
            false, false, true
        ));
        assert!(!preserve_pending_source_capture_after_atlas_recreation(
            false, true, false
        ));
    }
}
