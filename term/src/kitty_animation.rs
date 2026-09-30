//! A shared playback timeline. Sampling skips elapsed cycles in constant time
//! and locates a frame by binary search; it never replays missed frames.

pub use wezterm_escape_parser::apc::KittyAnimationState as Playback;

#[cfg_attr(feature = "use_serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KittyAnimation {
    pub frame: u32,
    pub mode: Playback,
    /// Zero means unlimited; otherwise stop before wrapping this many times.
    pub max_loops: u32,
    pub completed_loops: u64,
    pub started_at_ms: u64,
    /// Cumulative frame ends. Equal adjacent values represent gapless frames.
    pub frame_ends: Vec<u64>,
}

/// An optional companion to the legacy terminal snapshot.
#[cfg_attr(feature = "use_serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KittyPlaybackSnapshot {
    pub captured_at_ms: u64,
    pub selections: Vec<KittyPlaybackEntry>,
}

// The checkpoint shape deliberately omits the process-local image generation.
#[cfg_attr(feature = "use_serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KittyPlaybackEntry {
    pub image_id: u32,
    pub data_hash: [u8; 32],
    pub animation: KittyAnimation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sample {
    pub frame: u32,
    pub started_at_ms: u64,
    pub completed_loops: u64,
    pub next_at_ms: Option<u64>,
}

/// The mux's monotonic clock domain. Clients translate it to their own clock;
/// they use `sample` rather than calling this function.
pub fn monotonic_ms() -> u64 {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    START
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_millis()
        .min((u64::MAX - u32::MAX as u64) as u128) as u64
        // Reserve a maximum frame gap before the origin, so a restored
        // partially displayed frame can start before this process did.
        + u32::MAX as u64
}

impl KittyAnimation {
    pub fn rebase(&mut self, captured_at: u64, restored_at: u64) {
        let sample = self.sample(captured_at);
        let elapsed = if self.mode == Playback::Stopped {
            0
        } else {
            captured_at
                .saturating_sub(sample.started_at_ms)
                .min(self.gap(sample.frame as usize))
        };
        self.frame = sample.frame;
        self.completed_loops = sample.completed_loops;
        self.started_at_ms = restored_at.saturating_sub(elapsed);
    }
    pub fn new(gaps: impl IntoIterator<Item = u32>, now: u64) -> Self {
        let mut end = 0;
        let frame_ends = gaps
            .into_iter()
            .map(|gap| {
                end += u64::from(gap);
                end
            })
            .collect();
        Self {
            frame: 0,
            mode: Playback::Stopped,
            max_loops: 0,
            completed_loops: 0,
            started_at_ms: now,
            frame_ends,
        }
    }

    pub fn valid(&self) -> bool {
        !self.frame_ends.is_empty()
            && self.frame_ends.len() <= 4096
            && (self.frame as usize) < self.frame_ends.len()
            && self
                .frame_ends
                .windows(2)
                .all(|pair| pair[0] <= pair[1] && pair[1] - pair[0] <= u32::MAX as u64)
            && self.frame_ends[0] <= u32::MAX as u64
    }

    fn start(&self, frame: usize) -> u64 {
        frame
            .checked_sub(1)
            .map_or(0, |previous| self.frame_ends[previous])
    }

    pub fn gap(&self, frame: usize) -> u64 {
        self.frame_ends[frame] - self.start(frame)
    }

    pub fn sample(&self, now: u64) -> Sample {
        let held = Sample {
            frame: self.frame,
            started_at_ms: self.started_at_ms,
            completed_loops: self.completed_loops,
            next_at_ms: None,
        };
        let cycle = self.frame_ends.last().copied().unwrap_or(0);
        if self.mode == Playback::Stopped
            || self.frame_ends.len() < 2
            || cycle == 0
            || (self.max_loops > 0 && self.completed_loops >= u64::from(self.max_loops))
        {
            return held;
        }
        let now = now.max(self.started_at_ms);
        let position = self
            .start(self.frame as usize)
            .saturating_add(now - self.started_at_ms);
        let loops = position / cycle;
        let completed = self.completed_loops.saturating_add(loops);
        let ended = (self.mode == Playback::Loading && loops > 0)
            || (self.max_loops > 0 && completed >= u64::from(self.max_loops));
        if ended {
            // Trailing gapless frames never replace the last visible frame.
            let frame = self.frame_ends.partition_point(|end| *end < cycle);
            let remaining = if self.mode == Playback::Loading {
                1
            } else {
                u64::from(self.max_loops).saturating_sub(self.completed_loops)
            };
            let until_end = remaining
                .saturating_mul(cycle)
                .saturating_sub(self.start(self.frame as usize));
            let started = self
                .started_at_ms
                .saturating_add(until_end)
                .saturating_sub(self.gap(frame));
            return Sample {
                frame: frame as u32,
                started_at_ms: started,
                completed_loops: if self.mode == Playback::Loading {
                    self.completed_loops
                } else {
                    u64::from(self.max_loops)
                },
                next_at_ms: None,
            };
        }
        let within = position % cycle;
        let frame = self.frame_ends.partition_point(|end| *end <= within);
        let elapsed = within - self.start(frame);
        let started = now.saturating_sub(elapsed);
        Sample {
            frame: frame as u32,
            started_at_ms: started,
            completed_loops: completed,
            next_at_ms: Some(now.saturating_add(self.frame_ends[frame] - within)),
        }
    }

    fn settle(&mut self, now: u64) -> Sample {
        let sampled = self.sample(now);
        self.frame = sampled.frame;
        self.started_at_ms = sampled.started_at_ms;
        self.completed_loops = sampled.completed_loops;
        sampled
    }

    pub fn append(&mut self, gap: u32, now: u64) {
        let waiting = self.mode != Playback::Stopped
            && self.sample(now).next_at_ms.is_none()
            && (self.mode == Playback::Loading
                || self.frame_ends.len() < 2
                || self.frame_ends.last() == Some(&0));
        self.settle(now);
        if waiting && now.saturating_sub(self.started_at_ms) >= self.gap(self.frame as usize) {
            self.frame = self.frame_ends.len() as u32;
            self.started_at_ms = now;
        }
        self.frame_ends
            .push(self.frame_ends.last().copied().unwrap_or(0) + u64::from(gap));
    }

    pub fn set_gap(&mut self, frame: usize, gap: u32, now: u64) -> bool {
        if frame >= self.frame_ends.len() || self.gap(frame) == u64::from(gap) {
            return false;
        }
        self.settle(now);
        let old = self.gap(frame);
        for end in &mut self.frame_ends[frame..] {
            *end = *end - old + u64::from(gap);
        }
        true
    }

    pub fn remove_frame(&mut self, frame: usize, now: u64) -> bool {
        if self.frame_ends.len() <= 1 || frame >= self.frame_ends.len() {
            return false;
        }
        self.settle(now);
        let gap = self.gap(frame);
        self.frame_ends.remove(frame);
        for end in &mut self.frame_ends[frame..] {
            *end -= gap;
        }
        if frame < self.frame as usize {
            self.frame -= 1;
        } else if frame == self.frame as usize {
            self.frame = frame.min(self.frame_ends.len() - 1) as u32;
            self.started_at_ms = now;
        }
        true
    }

    pub fn control(
        &mut self,
        control: &wezterm_escape_parser::apc::KittyImageAnimationControl,
        now: u64,
    ) -> bool {
        let current = control
            .current_frame
            .filter(|frame| *frame > 0 && (*frame as usize) <= self.frame_ends.len());
        let gap = control
            .frame_number
            .filter(|frame| *frame > 0 && (*frame as usize) <= self.frame_ends.len())
            .zip(control.gap_ms.filter(|gap| *gap != 0));
        let loops = control.loops.filter(|loops| *loops > 0);
        if current.is_none() && gap.is_none() && control.state.is_none() && loops.is_none() {
            return false;
        }
        let header = |a: &Self| {
            (
                a.frame,
                a.mode,
                a.max_loops,
                a.completed_loops,
                a.started_at_ms,
            )
        };
        let before = header(self);
        self.settle(now);
        let changed_gap = gap
            .is_some_and(|(frame, gap)| self.set_gap(frame as usize - 1, gap.max(0) as u32, now));
        if let Some(frame) = current {
            if self.frame != frame - 1 {
                self.frame = frame - 1;
                self.started_at_ms = now;
            }
        }
        if let Some(mode) = control.state {
            if self.mode == Playback::Stopped && mode != Playback::Stopped {
                self.started_at_ms = now;
            }
            self.mode = mode;
            self.completed_loops = 0;
        }
        if let Some(loops) = loops {
            self.max_loops = loops - 1;
        }
        changed_gap || header(self) != before
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wezterm_escape_parser::apc::KittyImageAnimationControl;

    fn control() -> KittyImageAnimationControl {
        KittyImageAnimationControl {
            image_id: None,
            image_number: None,
            current_frame: None,
            frame_number: None,
            state: None,
            loops: None,
            gap_ms: None,
        }
    }

    #[test]
    fn frame_deletion_preserves_playback_position_and_removes_the_gap() {
        let mut animation = KittyAnimation::new([20, 30, 40], 100);
        animation.mode = Playback::Running;
        assert!(animation.remove_frame(0, 130));
        assert_eq!(animation.frame_ends, [30, 70]);
        assert_eq!(animation.sample(130).frame, 0);
        assert_eq!(animation.sample(130).next_at_ms, Some(150));
        assert!(animation.remove_frame(0, 140));
        assert_eq!(animation.frame_ends, [40]);
        assert_eq!(animation.started_at_ms, 140);
        assert!(animation.valid());
        assert!(!animation.remove_frame(0, 150));

        let mut stopped = KittyAnimation::new([0, 30, 0], 100);
        stopped.frame = 2;
        assert!(stopped.remove_frame(2, 120));
        assert_eq!(stopped.frame, 1);
        assert_eq!(stopped.frame_ends, [0, 30]);
        assert!(stopped.valid());
    }

    #[test]
    fn elapsed_cycles_are_skipped_and_gapless_frames_are_never_presented() {
        let mut timeline = KittyAnimation::new([0, 40, 0, 60, 0], 100);
        timeline.mode = Playback::Running;
        assert_eq!(timeline.sample(100).frame, 1);
        assert_eq!(timeline.sample(139).next_at_ms, Some(140));
        assert_eq!(timeline.sample(140).frame, 3);
        assert_eq!(timeline.sample(200).frame, 1);
        let sampled = timeline.sample(1_000_000_140);
        assert_eq!(sampled.frame, 3);
        assert_eq!(sampled.completed_loops, 10_000_000);
        assert_eq!(sampled.next_at_ms, Some(1_000_000_200));
    }

    #[test]
    fn finite_loops_hold_the_last_visible_frame() {
        let mut timeline = KittyAnimation::new([0, 40, 60, 0], 100);
        timeline.mode = Playback::Running;
        timeline.max_loops = 2;
        assert_eq!(timeline.sample(299).frame, 2);
        let end = timeline.sample(10_000);
        assert_eq!(end.frame, 2);
        assert_eq!(end.completed_loops, 2);
        assert_eq!(end.next_at_ms, None);
        assert_eq!(end.started_at_ms, 240);
    }

    #[test]
    fn loading_waits_and_a_new_frame_starts_when_it_arrives() {
        let mut timeline = KittyAnimation::new([0, 40], 100);
        timeline.mode = Playback::Loading;
        assert_eq!(timeline.sample(5000).frame, 1);
        assert_eq!(timeline.sample(5000).next_at_ms, None);
        timeline.append(70, 5000);
        assert_eq!(timeline.sample(5000).frame, 2);
        assert_eq!(timeline.sample(5069).next_at_ms, Some(5070));
        assert_eq!(timeline.sample(5070).next_at_ms, None);
    }

    #[test]
    fn stopping_freezes_the_sample_and_resuming_restarts_its_gap() {
        let mut timeline = KittyAnimation::new([20, 40], 100);
        timeline.mode = Playback::Running;
        timeline.control(
            &KittyImageAnimationControl {
                state: Some(Playback::Stopped),
                ..control()
            },
            125,
        );
        assert_eq!(timeline.sample(5000).frame, 1);
        assert_eq!(timeline.sample(5000).next_at_ms, None);
        timeline.control(
            &KittyImageAnimationControl {
                state: Some(Playback::Running),
                loops: Some(2),
                ..control()
            },
            5000,
        );
        assert_eq!(timeline.sample(5039).next_at_ms, Some(5040));
        assert_eq!(timeline.sample(5040).next_at_ms, None);
    }

    #[test]
    fn all_gapless_and_single_frame_images_do_not_spin() {
        for gaps in [vec![0, 0, 0], vec![40]] {
            let mut timeline = KittyAnimation::new(gaps, 100);
            timeline.mode = Playback::Running;
            assert_eq!(timeline.sample(u64::MAX).next_at_ms, None);
        }
        let mut timeline = KittyAnimation::new([0], 100);
        timeline.mode = Playback::Running;
        timeline.append(40, 5000);
        assert_eq!(timeline.sample(5000).frame, 1);
        assert_eq!(timeline.sample(5000).next_at_ms, Some(5040));
    }

    #[test]
    fn appending_before_the_current_gap_expires_keeps_its_remaining_time() {
        let mut timeline = KittyAnimation::new([40], 100);
        timeline.mode = Playback::Running;
        timeline.append(70, 110);
        assert_eq!(timeline.sample(110).frame, 0);
        assert_eq!(timeline.sample(110).next_at_ms, Some(140));
        assert_eq!(timeline.sample(140).frame, 1);
    }

    #[test]
    fn clock_mapping_before_the_anchor_does_not_advance_its_deadline() {
        let mut timeline = KittyAnimation::new([40, 70], 100);
        timeline.mode = Playback::Running;
        assert_eq!(timeline.sample(90).started_at_ms, 100);
        assert_eq!(timeline.sample(90).next_at_ms, Some(140));
    }

    #[test]
    fn restoring_preserves_remaining_gap_and_completed_loops_in_a_new_clock() {
        let mut timeline = KittyAnimation::new([40, 60], 100);
        timeline.mode = Playback::Running;
        timeline.max_loops = 3;
        timeline.rebase(255, 5000);
        assert_eq!(timeline.frame, 1);
        assert_eq!(timeline.completed_loops, 1);
        assert_eq!(timeline.sample(5000).next_at_ms, Some(5045));
        assert_eq!(timeline.sample(5045).frame, 0);
        assert_eq!(timeline.sample(5145).next_at_ms, None);
    }

    #[test]
    fn restoring_loading_wait_and_completed_animation_does_not_restart_them() {
        for mode in [Playback::Loading, Playback::Running] {
            let mut timeline = KittyAnimation::new([40, 60], 100);
            timeline.mode = mode;
            timeline.max_loops = 1;
            timeline.rebase(10_000, 5000);
            assert_eq!(timeline.sample(5000).frame, 1);
            assert_eq!(timeline.sample(5000).next_at_ms, None);
            if mode == Playback::Loading {
                timeline.append(70, 5100);
                assert_eq!(timeline.sample(5100).frame, 2);
                assert_eq!(timeline.sample(5100).next_at_ms, Some(5170));
            }
        }
    }

    #[test]
    fn sampling_matches_a_stepwise_player_for_every_anchor_and_gap_pattern() {
        for gaps in [[0, 1, 3, 0], [4, 0, 2, 5], [1, 1, 1, 1]] {
            for anchor in 0..gaps.len() {
                let mut timeline = KittyAnimation::new(gaps, 100);
                timeline.frame = anchor as u32;
                timeline.mode = Playback::Running;
                let (mut frame, mut start, mut loops) = (anchor, 100, 0);
                for now in 100..1000 {
                    while now - start >= u64::from(gaps[frame]) {
                        start += u64::from(gaps[frame]);
                        frame += 1;
                        if frame == gaps.len() {
                            frame = 0;
                            loops += 1;
                        }
                    }
                    assert_eq!(
                        timeline.sample(now),
                        Sample {
                            frame: frame as u32,
                            started_at_ms: start,
                            completed_loops: loops,
                            next_at_ms: Some(start + u64::from(gaps[frame])),
                        }
                    );
                }
            }
        }
    }
}
