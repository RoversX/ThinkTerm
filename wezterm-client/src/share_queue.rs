//! A slow share viewer keeps the latest screen update and control state.

use codec::{GetPaneRenderChangesResponse, Pdu, ShareStateChanged};
use smol::channel::{RecvError, TryRecvError};
use std::collections::BTreeMap;
use std::ops::Range;
use std::sync::{Arc, Mutex};
use wezterm_term::{Line, StableRowIndex};

/// Rows that skipped updates brought, kept for a viewer that fell behind;
/// past this, the furthest back count as changed instead.
const CARRIED_ROWS: usize = 1000;
/// Separate runs of changed rows kept; past this, one run spans them all.
const CHANGED_RUNS: usize = 64;

pub struct RenderUpdate {
    pub changes: Box<GetPaneRenderChangesResponse>,
    /// Rows that updates merged into this one brought and it does not:
    /// they go in after its dirty rows go, before its bonus rows.
    pub carried: BTreeMap<StableRowIndex, Line>,
    pub screen_changed: bool,
    pub generation: u64,
}

impl RenderUpdate {
    pub fn new(changes: GetPaneRenderChangesResponse, generation: u64) -> anyhow::Result<Self> {
        anyhow::ensure!(
            changes.dimensions.is_plausible(),
            "the share sent invalid terminal dimensions"
        );
        Ok(Self {
            changes: Box::new(changes),
            carried: BTreeMap::new(),
            screen_changed: false,
            generation,
        })
    }

    /// Keep the newer payload as if the server had sent both as one: rows
    /// only this one brought are carried, rows it changed without bringing
    /// stay changed, and the rows between are left alone.
    pub fn merge(&mut self, mut newer: Self) {
        if newer.screen_changed || self.changes.alt_screen != newer.changes.alt_screen {
            // The other screen numbers its rows afresh: nothing said of
            // this one's rows holds there.
            newer.screen_changed = true;
            *self = newer;
            return;
        }
        newer.screen_changed = self.screen_changed;
        let (brought, _) = std::mem::take(&mut self.changes.bonus_lines).extract_data();
        let mut carried = std::mem::take(&mut self.carried);
        carried.extend(brought);
        // What the newer one brings or changed is newer than these.
        for row in newer.changes.bonus_lines.rows() {
            carried.remove(&row);
        }
        for range in &newer.changes.dirty_lines {
            if carried.is_empty() {
                break;
            }
            if range.is_empty() {
                continue;
            }
            let stale: Vec<_> = carried.range(range.clone()).map(|(row, _)| *row).collect();
            for row in stale {
                carried.remove(&row);
            }
        }
        carried.append(&mut newer.carried);
        let mut changed = std::mem::take(&mut self.changes.dirty_lines);
        while carried.len() > CARRIED_ROWS {
            if let Some((row, _)) = carried.pop_first() {
                changed.push(row..row.saturating_add(1));
            }
        }
        changed.append(&mut newer.changes.dirty_lines);
        newer.changes.dirty_lines = runs(changed);
        newer.carried = carried;
        *self = newer;
    }
}

/// `ranges` in order, with overlapping and touching ones joined and empty
/// ones gone; more than `CHANGED_RUNS` become one from first to last.
fn runs(mut ranges: Vec<Range<StableRowIndex>>) -> Vec<Range<StableRowIndex>> {
    ranges.retain(|range| range.start < range.end);
    ranges.sort_unstable_by_key(|range| range.start);
    let mut runs: Vec<Range<StableRowIndex>> = Vec::with_capacity(ranges.len());
    for range in ranges {
        match runs.last_mut() {
            Some(last) if range.start <= last.end => last.end = last.end.max(range.end),
            _ => runs.push(range),
        }
    }
    if runs.len() > CHANGED_RUNS {
        let span = runs[0].start..runs[runs.len() - 1].end;
        runs = vec![span];
    }
    runs
}

pub enum Update {
    Render(RenderUpdate),
    State(ShareStateChanged),
    Ended(String),
}

#[derive(Default)]
struct Pending {
    render: Option<RenderUpdate>,
    state: Option<ShareStateChanged>,
    ended: Option<String>,
    terminal: bool,
    closed: bool,
    generation: u64,
}

impl Pending {
    fn end(&mut self, reason: String) {
        if !self.terminal {
            self.render = None;
            self.state = None;
            self.ended = Some(reason);
            self.terminal = true;
        }
    }

    fn take(&mut self) -> Option<Update> {
        if let Some(reason) = self.ended.take() {
            return Some(Update::Ended(reason));
        }
        if let Some(state) = self.state.take() {
            return Some(Update::State(state));
        }
        self.render.take().map(Update::Render)
    }
}

pub(crate) struct Sender {
    pending: Arc<Mutex<Pending>>,
    wake: smol::channel::Sender<()>,
}

pub(crate) struct Receiver {
    pending: Arc<Mutex<Pending>>,
    wake: smol::channel::Receiver<()>,
}

pub(crate) fn channel() -> (Sender, Receiver) {
    let (wake_tx, wake_rx) = smol::channel::bounded(1);
    let pending = Arc::new(Mutex::new(Pending::default()));
    (
        Sender {
            pending: Arc::clone(&pending),
            wake: wake_tx,
        },
        Receiver {
            pending,
            wake: wake_rx,
        },
    )
}

impl Sender {
    pub(crate) fn same_channel(&self, receiver: &Receiver) -> bool {
        Arc::ptr_eq(&self.pending, &receiver.pending)
    }

    pub(crate) fn send(&self, pdu: Pdu) -> anyhow::Result<()> {
        let mut pending = self.pending.lock().unwrap();
        anyhow::ensure!(!pending.closed, "the viewer closed");
        if pending.terminal {
            return Ok(());
        }
        let result = match pdu {
            Pdu::GetPaneRenderChangesResponse(changes) => {
                let generation = pending.generation.checked_add(1);
                match generation {
                    Some(generation) => {
                        pending.generation = generation;
                        match RenderUpdate::new(changes, generation) {
                            Ok(update) => match pending.render.as_mut() {
                                Some(previous) => previous.merge(update),
                                None => pending.render = Some(update),
                            },
                            Err(err) => {
                                pending.end(err.to_string());
                                self.wake.close();
                                return Err(err);
                            }
                        }
                        Ok(())
                    }
                    None => {
                        pending.end("the share sent too many screen updates".into());
                        self.wake.close();
                        anyhow::bail!("the share sent too many screen updates")
                    }
                }
            }
            Pdu::ShareStateChanged(mut state) => {
                if let Some(reason) = state.ended.take() {
                    pending.end(reason);
                } else {
                    pending.state = Some(state);
                }
                Ok(())
            }
            Pdu::PaneRemoved(_) | Pdu::PaneExited(_) => {
                pending.end(codec::share_end::CLOSED.into());
                Ok(())
            }
            _ => return Ok(()),
        };
        if pending.terminal {
            self.wake.close();
        } else {
            // A wake carries no payload and never blocks the network reader.
            let _ = self.wake.try_send(());
        }
        result
    }
}

impl Receiver {
    #[cfg(test)]
    pub(crate) fn generation(&self) -> u64 {
        self.pending.lock().unwrap().generation
    }

    pub(crate) fn try_recv(&self) -> Result<Update, TryRecvError> {
        let mut pending = self.pending.lock().unwrap();
        if let Some(update) = pending.take() {
            return Ok(update);
        }
        if pending.closed || pending.terminal || self.wake.is_closed() {
            Err(TryRecvError::Closed)
        } else {
            Err(TryRecvError::Empty)
        }
    }

    pub(crate) async fn recv(&self) -> Result<Update, RecvError> {
        loop {
            match self.try_recv() {
                Ok(update) => return Ok(update),
                Err(TryRecvError::Closed) => return Err(RecvError),
                Err(TryRecvError::Empty) => {}
            }
            // After a close, inspect the slots once more for the final reason.
            let _ = self.wake.recv().await;
        }
    }

    pub(crate) fn close(&self) {
        let mut pending = self.pending.lock().unwrap();
        pending.closed = true;
        pending.render = None;
        pending.state = None;
        pending.ended = None;
        self.wake.close();
    }
}

impl Drop for Receiver {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mux::renderable::{RenderableDimensions, StableCursorPosition};
    use wezterm_term::Line;

    fn changes(alt_screen: bool, row: StableRowIndex) -> GetPaneRenderChangesResponse {
        GetPaneRenderChangesResponse {
            pane_id: 1,
            mouse_grabbed: false,
            alt_screen,
            keyboard_encoding: Default::default(),
            bracketed_paste: false,
            cursor_position: StableCursorPosition::default(),
            dimensions: RenderableDimensions {
                cols: 80,
                viewport_rows: 24,
                ..Default::default()
            },
            dirty_lines: vec![],
            title: row.to_string(),
            bonus_lines: vec![(row, Line::from_text("screen", &Default::default(), 0, None))]
                .into(),
            working_dir: None,
            input_serial: None,
            seqno: 0,
        }
    }

    fn state(controlling: bool, ended: Option<&str>) -> Pdu {
        Pdu::ShareStateChanged(ShareStateChanged {
            share_id: 1,
            controlling,
            requested: false,
            controller: None,
            ended: ended.map(str::to_string),
        })
    }

    async fn send_over_wire(sender: &Sender, pdu: Pdu) {
        let mut bytes = Vec::new();
        pdu.encode(&mut bytes, 0).unwrap();
        let length = bytes.len() as u64;
        let mut reader = futures::io::Cursor::new(bytes);
        let decoded =
            Pdu::decode_async_bounded(&mut reader, Some(1), crate::share::MAX_RESPONSE_BYTES)
                .await
                .unwrap();
        assert_eq!(decoded.serial, 0);
        assert_eq!(reader.position(), length);
        sender.send(decoded.pdu).unwrap();
    }

    #[test]
    #[allow(clippy::single_range_in_vec_init)]
    fn decoded_output_bursts_preserve_the_latest_screen_revoke_and_end() {
        smol::block_on(async {
            let (sender, receiver) = channel();
            send_over_wire(&sender, state(true, None)).await;
            for row in 0..4096 {
                let mut update = changes((1024..2048).contains(&row), row);
                update.dimensions.physical_top = row.saturating_sub(23).max(0);
                update.cursor_position.y = row;
                update.dirty_lines = vec![row - 1..row];
                send_over_wire(&sender, Pdu::GetPaneRenderChangesResponse(update)).await;
            }
            send_over_wire(&sender, state(false, None)).await;

            assert_eq!(receiver.generation(), 4096);
            assert_eq!(sender.wake.len(), 1);
            assert!(!sender.wake.is_closed());
            assert!(matches!(
                receiver.try_recv(),
                Ok(Update::State(state)) if !state.controlling
            ));
            let Ok(Update::Render(update)) = receiver.try_recv() else {
                panic!("expected the latest decoded screen");
            };
            assert_eq!(update.changes.title, "4095");
            assert_eq!(update.changes.dimensions.physical_top, 4072);
            // Each row was changed by the next update; those before the last
            // trip to the other screen are gone with it.
            assert_eq!(update.changes.dirty_lines, vec![2047..4095]);
            assert!(update.carried.is_empty());
            assert!(update.screen_changed);
            let (rows, _) = update.changes.bonus_lines.extract_data();
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].0, 4095);
            assert_eq!(rows[0].1.as_str(), "screen");

            send_over_wire(
                &sender,
                Pdu::GetPaneRenderChangesResponse(changes(false, 4096)),
            )
            .await;
            send_over_wire(&sender, state(false, Some(codec::share_end::STOPPED))).await;
            send_over_wire(&sender, state(true, None)).await;
            send_over_wire(
                &sender,
                Pdu::GetPaneRenderChangesResponse(changes(false, 4097)),
            )
            .await;

            {
                let pending = sender.pending.lock().unwrap();
                assert!(pending.render.is_none());
                assert!(pending.state.is_none());
            }
            assert!(matches!(
                receiver.recv().await,
                Ok(Update::Ended(reason)) if reason == codec::share_end::STOPPED
            ));
            assert!(matches!(receiver.try_recv(), Err(TryRecvError::Closed)));
        });
    }

    #[test]
    #[allow(clippy::single_range_in_vec_init)]
    fn thousands_of_updates_keep_one_payload_and_the_latest_control_state() {
        let (sender, receiver) = channel();
        for row in 0..5000 {
            sender
                .send(Pdu::GetPaneRenderChangesResponse(changes(false, row)))
                .unwrap();
            sender.send(state(row % 2 == 0, None)).unwrap();
        }
        let pending = sender.pending.lock().unwrap();
        assert!(pending.render.is_some());
        assert!(pending.state.is_some());
        assert!(pending.ended.is_none());
        assert_eq!(sender.wake.len(), 1);
        drop(pending);
        assert_eq!(receiver.generation(), 5000);
        assert!(matches!(receiver.try_recv(), Ok(Update::State(state)) if !state.controlling));
        let Ok(Update::Render(update)) = receiver.try_recv() else {
            panic!("expected the latest screen");
        };
        assert_eq!(update.changes.title, "4999");
        // The rows they brought, as many as are kept; those further back
        // count as changed.
        assert_eq!(update.carried.len(), CARRIED_ROWS);
        assert_eq!(update.carried.keys().next(), Some(&3999));
        assert_eq!(update.carried[&4998].as_str(), "screen");
        assert_eq!(update.changes.dirty_lines, vec![0..3999]);
        assert_eq!(update.generation, 5000);
        assert!(matches!(receiver.try_recv(), Err(TryRecvError::Empty)));
    }

    #[test]
    #[allow(clippy::reversed_empty_ranges, clippy::single_range_in_vec_init)]
    fn merging_keeps_changed_runs_and_carried_rows_without_overflow() {
        let mut first = changes(false, StableRowIndex::MAX);
        first.dirty_lines = vec![StableRowIndex::MIN..-4, 10..10, 30..20];
        let mut update = RenderUpdate::new(first, 1).unwrap();
        let mut newer = changes(false, 7);
        newer.dirty_lines = vec![-3..10];
        update.merge(RenderUpdate::new(newer, 2).unwrap());
        assert_eq!(
            update.changes.dirty_lines,
            vec![StableRowIndex::MIN..-4, -3..10]
        );
        assert_eq!(
            update.carried.keys().copied().collect::<Vec<_>>(),
            vec![StableRowIndex::MAX]
        );
        assert_eq!(
            update.changes.bonus_lines.rows().collect::<Vec<_>>(),
            vec![7]
        );
        assert_eq!(update.generation, 2);

        // Too many separate runs become one.
        let runs = CHANGED_RUNS as StableRowIndex + 1;
        let mut sparse = changes(false, 0);
        sparse.dirty_lines = (0..runs).map(|run| run * 2..run * 2 + 1).collect();
        let mut update = RenderUpdate::new(sparse, 3).unwrap();
        update.merge(RenderUpdate::new(changes(false, 1000), 4).unwrap());
        assert_eq!(update.changes.dirty_lines, vec![0..runs * 2 - 1]);
    }

    fn line(text: &str) -> Line {
        Line::from_text(text, &Default::default(), 0, None)
    }

    #[test]
    fn rows_between_those_skipped_updates_changed_are_left_alone() {
        // A clock on the top row and the cursor's row at the bottom.
        let screen = |generation: u64, clock: &str| {
            let mut update = changes(false, 0);
            update.bonus_lines = vec![(0, line(clock)), (23, line("$ "))].into();
            RenderUpdate::new(update, generation).unwrap()
        };
        let mut update = screen(1, "12:00:01");
        for generation in 2..6 {
            update.merge(screen(generation, "12:00:02"));
        }
        assert!(update.changes.dirty_lines.is_empty());
        assert!(update.carried.is_empty());
    }

    #[test]
    #[allow(clippy::single_range_in_vec_init)]
    fn rows_skipped_updates_brought_are_kept_until_changed() {
        let screen = |generation: u64, top: StableRowIndex, row: StableRowIndex, text: &str| {
            let mut update = changes(false, 0);
            update.dimensions.physical_top = top;
            update.bonus_lines = vec![(row, line(text))].into();
            RenderUpdate::new(update, generation).unwrap()
        };
        // The cursor at the top, at the bottom, and then the screen moves
        // on a row: the top row is history now, and was seen.
        let mut update = screen(1, 100, 100, "seen");
        update.merge(screen(2, 100, 123, "$ "));
        update.merge(screen(3, 101, 124, "new"));
        assert!(update.changes.dirty_lines.is_empty());
        assert_eq!(
            update.carried.keys().copied().collect::<Vec<_>>(),
            vec![100, 123]
        );
        assert_eq!(update.carried[&100].as_str(), "seen");

        let mut changed = screen(4, 101, 125, "later");
        changed.changes.dirty_lines = vec![100..101];
        update.merge(changed);
        assert_eq!(
            update.carried.keys().copied().collect::<Vec<_>>(),
            vec![123, 124]
        );
        assert_eq!(update.changes.dirty_lines, vec![100..101]);
    }

    #[test]
    fn a_round_trip_through_the_other_screen_stays_recorded() {
        let mut update = RenderUpdate::new(changes(false, 1), 1).unwrap();
        update.merge(RenderUpdate::new(changes(true, 0), 2).unwrap());
        update.merge(RenderUpdate::new(changes(false, 1), 3).unwrap());
        assert!(!update.changes.alt_screen);
        assert!(update.screen_changed);
    }

    #[test]
    fn an_end_dominates_queued_and_later_screen_and_control_updates() {
        let (sender, receiver) = channel();
        sender
            .send(Pdu::GetPaneRenderChangesResponse(changes(false, 1)))
            .unwrap();
        sender.send(state(true, None)).unwrap();
        sender.send(state(false, Some("removed"))).unwrap();
        sender.send(state(true, None)).unwrap();
        sender
            .send(Pdu::GetPaneRenderChangesResponse(changes(false, 2)))
            .unwrap();
        assert!(matches!(receiver.try_recv(), Ok(Update::Ended(reason)) if reason == "removed"));
        assert!(matches!(receiver.try_recv(), Err(TryRecvError::Closed)));
        let pending = sender.pending.lock().unwrap();
        assert!(pending.render.is_none());
        assert!(pending.state.is_none());
    }

    #[test]
    fn a_bad_frame_cannot_be_erased_by_a_later_valid_frame() {
        let (sender, receiver) = channel();
        let mut bad = changes(false, 1);
        bad.dimensions.viewport_rows = usize::MAX;
        assert!(sender.send(Pdu::GetPaneRenderChangesResponse(bad)).is_err());
        sender
            .send(Pdu::GetPaneRenderChangesResponse(changes(false, 2)))
            .unwrap();
        assert!(
            matches!(receiver.try_recv(), Ok(Update::Ended(reason)) if reason.contains("dimensions"))
        );
        assert!(matches!(receiver.try_recv(), Err(TryRecvError::Closed)));
    }

    #[test]
    fn closing_releases_unread_payloads_and_wakes_a_waiter() {
        let (sender, receiver) = channel();
        let allocation = Arc::downgrade(&sender.pending);
        sender
            .send(Pdu::GetPaneRenderChangesResponse(changes(false, 1)))
            .unwrap();
        sender.send(state(true, None)).unwrap();
        drop(receiver);
        let pending = sender.pending.lock().unwrap();
        assert!(pending.render.is_none());
        assert!(pending.state.is_none());
        assert!(pending.ended.is_none());
        drop(pending);
        assert!(sender.send(state(false, None)).is_err());
        drop(sender);
        assert!(allocation.upgrade().is_none());

        let (sender, receiver) = channel();
        drop(sender);
        assert!(smol::block_on(receiver.recv()).is_err());
    }
}
