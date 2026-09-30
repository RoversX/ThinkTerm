//! A connection-scoped mailbox for unsolicited, complete Kitty snapshots.
//! Replies and terminal deltas must never pass through this queue.

use crate::KittyFrameSelections;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use crate::kitty_metadata::MAX_METADATA_BYTES as MAX_BYTES;
const MAX_PANES: usize = 1024;

struct Pending {
    snapshot: KittyFrameSelections,
    bytes: usize,
}

#[derive(Default)]
struct State {
    panes: HashMap<usize, Pending>,
    order: VecDeque<usize>,
    bytes: usize,
    scheduled: bool,
    closed: bool,
}

impl State {
    fn post(&mut self, snapshot: KittyFrameSelections) -> anyhow::Result<bool> {
        anyhow::ensure!(!self.closed, "Kitty snapshot mailbox is closed");
        let held = self.panes.get(&snapshot.pane_id);
        if held.is_some_and(|held| held.snapshot.revision >= snapshot.revision) {
            return Ok(false);
        }
        // Account for retained allocations, including spare Vec capacity.
        // Pane count separately bounds the map and FIFO bookkeeping.
        let bytes = snapshot.retained_bytes()?;
        let total = self
            .bytes
            .saturating_sub(held.map_or(0, |held| held.bytes))
            .checked_add(bytes);
        anyhow::ensure!(
            total.is_some_and(|total| total <= MAX_BYTES),
            "Kitty snapshots exceed mailbox byte budget"
        );
        anyhow::ensure!(
            held.is_some() || self.panes.len() < MAX_PANES,
            "Kitty snapshots exceed mailbox pane budget"
        );
        let pane = snapshot.pane_id;
        if self
            .panes
            .insert(pane, Pending { snapshot, bytes })
            .is_none()
        {
            self.order.push_back(pane);
        }
        self.bytes = total.unwrap();
        let wake = !self.scheduled;
        self.scheduled = true;
        Ok(wake)
    }

    fn next(&mut self) -> Option<KittyFrameSelections> {
        let Some(pane) = self.order.pop_front() else {
            self.scheduled = false;
            self.panes = HashMap::new();
            self.order = VecDeque::new();
            return None;
        };
        let held = self.panes.remove(&pane).unwrap();
        self.bytes -= held.bytes;
        Some(held.snapshot)
    }
}

/// Dropping the connection owner releases queued snapshots even if a scheduled
/// consumer still holds a handle. A replacement connection gets its own owner.
#[derive(Default)]
pub struct KittyFrameMailbox(Arc<Mutex<State>>);

impl KittyFrameMailbox {
    pub fn handle(&self) -> KittyFrameMailboxHandle {
        KittyFrameMailboxHandle(Arc::clone(&self.0))
    }
}

impl Drop for KittyFrameMailbox {
    fn drop(&mut self) {
        *self.0.lock().unwrap() = State {
            closed: true,
            ..State::default()
        };
    }
}

#[derive(Clone)]
pub struct KittyFrameMailboxHandle(Arc<Mutex<State>>);

impl KittyFrameMailboxHandle {
    /// True means the caller must schedule one consumer. While it runs, posts
    /// replace pending snapshots without scheduling another consumer.
    pub fn post(&self, snapshot: KittyFrameSelections) -> anyhow::Result<bool> {
        self.0.lock().unwrap().post(snapshot)
    }

    /// The consumer must drain to None, even if processing a snapshot fails.
    /// It may yield between snapshots; other panes keep their FIFO position.
    pub fn next(&self) -> Option<KittyFrameSelections> {
        self.0.lock().unwrap().next()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(pane_id: usize, revision: u64) -> KittyFrameSelections {
        KittyFrameSelections {
            pane_id,
            image_epoch: 0,
            now_ms: 0,
            revision,
            selections: vec![],
        }
    }

    #[test]
    fn a_blocked_consumer_keeps_only_the_latest_snapshot_and_one_wake() {
        let owner = KittyFrameMailbox::default();
        let queue = owner.handle();
        assert!(queue.post(snapshot(1, 1)).unwrap());
        let in_flight = queue.next().unwrap();
        for revision in 2..10_000 {
            assert!(!queue.post(snapshot(1, revision)).unwrap());
        }
        assert_eq!(queue.0.lock().unwrap().panes.len(), 1);
        assert_eq!(in_flight.revision, 1);
        assert_eq!(queue.next().unwrap().revision, 9_999);
        assert!(queue.next().is_none());
        assert!(queue.post(snapshot(1, 10_000)).unwrap());
    }

    #[test]
    fn replacements_do_not_starve_other_panes_or_allow_older_revisions() {
        let mut queue = State::default();
        queue.post(snapshot(1, 1)).unwrap();
        queue.post(snapshot(2, 1)).unwrap();
        queue.post(snapshot(1, 3)).unwrap();
        queue.post(snapshot(1, 2)).unwrap();
        assert_eq!(queue.next().unwrap().revision, 3);
        queue.post(snapshot(1, 4)).unwrap();
        assert_eq!(queue.next().unwrap().pane_id, 2);
        assert_eq!(queue.next().unwrap().revision, 4);
        assert!(queue.next().is_none());
        assert_eq!(
            (queue.bytes, queue.panes.capacity(), queue.order.capacity()),
            (0, 0, 0)
        );
    }

    #[test]
    fn pane_budget_is_bounded_and_replacement_does_not_need_another_slot() {
        let mut queue = State::default();
        for pane in 0..MAX_PANES {
            queue.post(snapshot(pane, 1)).unwrap();
        }
        assert!(queue.post(snapshot(MAX_PANES, 1)).is_err());
        assert!(queue.post(snapshot(0, 2)).is_ok());
        assert_eq!(queue.panes.len(), MAX_PANES);
    }

    #[test]
    fn byte_budget_counts_capacity_and_releases_replaced_payloads() {
        let mut queue = State::default();
        let mut large = snapshot(1, 1);
        large.selections.push(wezterm_term::KittyFrameSelection { relative_placements: Vec::new(), virtual_placements: Vec::new(),
            image_id: 1,
            data_hash: [1; 32],
            data_generation: 0,
            animation: wezterm_term::kitty_animation::KittyAnimation::new([1], 0),
        });
        large.selections[0]
            .animation
            .frame_ends
            .reserve(MAX_BYTES / 8);
        assert!(queue.post(large).is_err());
        assert_eq!(queue.bytes, 0);
        queue.post(snapshot(1, 2)).unwrap();
        assert_eq!(queue.bytes, std::mem::size_of::<KittyFrameSelections>());
        queue.next();
        assert_eq!(queue.bytes, 0);
    }

    #[test]
    fn dropping_a_connection_releases_pending_work_and_invalidates_old_handles() {
        let owner = KittyFrameMailbox::default();
        let old = owner.handle();
        old.post(snapshot(1, 99)).unwrap();
        drop(owner);
        assert!(old.next().is_none());
        assert!(old.post(snapshot(1, 100)).is_err());
        assert_eq!(old.0.lock().unwrap().bytes, 0);
        let replacement = KittyFrameMailbox::default();
        let new = replacement.handle();
        assert!(new.post(snapshot(1, 1)).unwrap());
        assert_eq!(new.next().unwrap().revision, 1);
    }

    #[test]
    fn byte_budget_applies_across_panes_and_replacement_returns_capacity() {
        let large = |pane| {
            let mut state = snapshot(pane, 1);
            let mut animation = wezterm_term::kitty_animation::KittyAnimation::new([1], 0);
            animation.frame_ends = Vec::with_capacity(MAX_BYTES / 16);
            animation.frame_ends.push(1);
            state.selections.push(wezterm_term::KittyFrameSelection { relative_placements: Vec::new(), virtual_placements: Vec::new(),
                image_id: 1,
                data_hash: [1; 32],
                data_generation: 0,
                animation,
            });
            state
        };
        let mut queue = State::default();
        queue.post(large(1)).unwrap();
        let held = queue.bytes;
        assert!(held > MAX_BYTES / 2);
        assert!(queue.post(large(2)).is_err());
        assert_eq!((queue.bytes, queue.panes.len()), (held, 1));
        queue.post(snapshot(1, 2)).unwrap();
        queue.post(large(2)).unwrap();
        assert!(queue.bytes < MAX_BYTES);
    }
}
