//! Optional animation metadata for viewers behind a proxy mux. Ordinary desktop
//! panes never create a relay or subscribe to this extension.
use crate::domain::ClientInner;
use codec::{GetKittyFrameSelections, KittyFrameSelections, Pdu};
use mux::pane::{KittyFrameSubscription, PaneId};
use mux::{Mux, MuxNotification};
use parking_lot::Mutex;
use std::sync::{Arc, Weak};
use wezterm_term::kitty_animation::monotonic_ms;
use wezterm_term::KittyFrameSelection;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Request {
    generation: u64,
    epoch: u64,
    subscribe: bool,
}

#[derive(Default)]
struct State {
    listeners: usize,
    generation: u64,
    epoch: u64,
    delivered: bool,
    possibly_subscribed: bool,
    delivered_epoch: u64,
    failed: Option<Request>,
    worker: bool,
    revision: u64,
    remote_revision: Option<u64>,
    captured_at: u64,
    image_epoch: u64,
    remote_image_epoch: Option<u64>,
    offset: Option<i128>,
    best_rtt: Option<u64>,
    selections: Vec<KittyFrameSelection>,
}

impl State {
    fn clear(&mut self) {
        self.selections = Vec::new();
        self.image_epoch += 1;
        self.remote_image_epoch = None;
        self.remote_revision = None;
        self.offset = None;
        self.best_rtt = None;
        self.revision += 1;
    }

    fn reconnect(&mut self, generation: u64) -> bool {
        if self.generation == generation {
            return false;
        }
        self.generation = generation;
        self.epoch += 1;
        // An in-flight request can cross the reconnect queue. Reconcile even
        // an unwanted subscription once that request has settled.
        self.delivered = self.worker;
        self.possibly_subscribed = self.worker;
        self.failed = None;
        self.clear();
        true
    }

    fn add_listener(&mut self) {
        if self.listeners == 0 {
            self.epoch += 1;
        }
        self.listeners += 1;
    }

    fn remove_listener(&mut self) {
        self.listeners -= 1;
        if self.listeners == 0 {
            self.epoch += 1;
            self.clear();
        }
    }

    fn next(&self) -> Option<Request> {
        let subscribe = self.listeners > 0;
        if self.delivered == subscribe
            && (if subscribe {
                self.delivered_epoch == self.epoch
            } else {
                !self.possibly_subscribed
            })
        {
            return None;
        }
        let request = Request {
            generation: self.generation,
            epoch: self.epoch,
            subscribe,
        };
        (self.failed != Some(request)).then_some(request)
    }

    fn delivered(&mut self, request: Request) {
        if request.generation == self.generation {
            self.delivered = request.subscribe;
            self.possibly_subscribed = request.subscribe;
            self.delivered_epoch = request.epoch;
            self.failed = None;
        }
    }

    fn accept(
        &mut self,
        generation: u64,
        reply: KittyFrameSelections,
        received: u64,
        sent: Option<u64>,
    ) -> anyhow::Result<bool> {
        if generation != self.generation || self.listeners == 0 {
            return Ok(false);
        }
        reply.validate()?;
        let mut changed = false;
        if let Some(sent) = sent {
            let rtt = received.saturating_sub(sent);
            if self.best_rtt.is_none_or(|best| rtt < best) {
                let offset = reply.now_ms as i128 - (sent as i128 + received as i128) / 2;
                changed = self.offset != Some(offset);
                self.offset = Some(offset);
                self.best_rtt = Some(rtt);
            }
        } else if self.offset.is_none() {
            self.offset = Some(reply.now_ms as i128 - received as i128);
            changed = true;
        }
        if self
            .remote_revision
            .is_none_or(|revision| reply.revision > revision)
        {
            if self
                .remote_image_epoch
                .is_some_and(|epoch| epoch != reply.image_epoch)
            {
                self.image_epoch += 1;
            }
            self.remote_image_epoch = Some(reply.image_epoch);
            self.remote_revision = Some(reply.revision);
            self.captured_at = reply.now_ms;
            self.selections = reply.selections;
            changed = true;
        }
        if changed {
            self.revision += 1;
        }
        Ok(changed)
    }

    fn snapshot(&self, known: Option<u64>) -> Option<(u64, u64, Vec<KittyFrameSelection>)> {
        if known == Some(self.revision) {
            return None;
        }
        let local_capture =
            (self.captured_at as i128 - self.offset.unwrap_or(0)).clamp(0, u64::MAX as i128) as u64;
        let mut selections = self.selections.clone();
        for selection in &mut selections {
            selection.animation.rebase(self.captured_at, local_capture);
        }
        Some((self.revision, self.image_epoch, selections))
    }
}

pub(super) struct Relay {
    client: Weak<ClientInner>,
    local_pane: PaneId,
    remote_pane: PaneId,
    state: Mutex<State>,
}

impl Relay {
    pub fn new(client: &Arc<ClientInner>, local_pane: PaneId, remote_pane: PaneId) -> Arc<Self> {
        Arc::new(Self {
            client: Arc::downgrade(client),
            local_pane,
            remote_pane,
            state: Mutex::new(State {
                generation: client.client.connection_generation(),
                ..State::default()
            }),
        })
    }

    pub fn subscribe(self: &Arc<Self>) -> Box<dyn KittyFrameSubscription> {
        self.state.lock().add_listener();
        self.reconnected();
        self.kick();
        Box::new(Subscription(Arc::clone(self)))
    }

    pub fn reconnected(self: &Arc<Self>) {
        if let Some(client) = self.client.upgrade() {
            let changed = self
                .state
                .lock()
                .reconnect(client.client.connection_generation());
            if changed {
                self.notify();
                self.kick();
            }
        }
    }

    pub fn snapshot(&self, known: Option<u64>) -> Option<(u64, u64, Vec<KittyFrameSelection>)> {
        self.state.lock().snapshot(known)
    }

    pub fn receive(&self, reply: KittyFrameSelections) -> anyhow::Result<()> {
        if let Some(client) = self.client.upgrade() {
            let changed = self.state.lock().accept(
                client.client.connection_generation(),
                reply,
                monotonic_ms(),
                None,
            )?;
            if changed {
                self.notify();
            }
        }
        Ok(())
    }

    fn notify(&self) {
        Mux::get().notify(MuxNotification::PaneOutput(self.local_pane));
    }

    fn kick(self: &Arc<Self>) {
        {
            let mut state = self.state.lock();
            if state.worker || state.next().is_none() {
                return;
            }
            state.worker = true;
        }
        let relay = Arc::clone(self);
        promise::spawn::spawn_into_main_thread(async move {
            let mut failures = (None, 0);
            loop {
                let request = {
                    let mut state = relay.state.lock();
                    let Some(request) = state.next() else {
                        state.worker = false;
                        return;
                    };
                    state.possibly_subscribed |= request.subscribe;
                    request
                };
                let Some(client) = relay.client.upgrade() else {
                    let mut state = relay.state.lock();
                    state.worker = false;
                    state.clear();
                    return;
                };
                let sent = monotonic_ms();
                let response = client
                    .client
                    .send_pdu(Pdu::GetKittyFrameSelections(GetKittyFrameSelections {
                        pane_id: relay.remote_pane,
                        subscribe: request.subscribe,
                    }))
                    .await;
                let received = monotonic_ms();
                relay.reconnected();
                let result = match response {
                    Ok(Pdu::KittyFrameSelections(reply))
                        if request.subscribe && reply.pane_id == relay.remote_pane =>
                    {
                        let mut state = relay.state.lock();
                        let result = if request.epoch == state.epoch {
                            state.accept(request.generation, reply, received, Some(sent))
                        } else {
                            Ok(false)
                        };
                        if result.is_ok() {
                            state.delivered(request);
                        }
                        result
                    }
                    Ok(Pdu::UnitResponse(_)) if !request.subscribe => {
                        relay.state.lock().delivered(request);
                        Ok(false)
                    }
                    Ok(Pdu::ErrorResponse(err)) => Err(anyhow::anyhow!("{}", err.reason)),
                    Ok(_) => Err(anyhow::anyhow!("unexpected Kitty subscription response")),
                    Err(err) => Err(err),
                };
                match result {
                    Ok(changed) => {
                        failures = (None, 0);
                        if changed {
                            relay.notify();
                        }
                    }
                    Err(err) => {
                        failures = (
                            Some(request),
                            if failures.0 == Some(request) {
                                failures.1 + 1
                            } else {
                                1
                            },
                        );
                        if failures.1 >= 3 {
                            relay.state.lock().failed = Some(request);
                            log::warn!(
                                "Kitty subscription for remote pane {} failed: {err:#}",
                                relay.remote_pane
                            );
                        } else {
                            smol::Timer::after(std::time::Duration::from_secs(2)).await;
                        }
                    }
                }
            }
        })
        .detach();
    }
}

pub(super) async fn fetch_image(
    client: Arc<ClientInner>,
    relay: Option<Arc<Relay>>,
    remote_pane: PaneId,
    mut request: codec::GetKittyImage,
) -> anyhow::Result<codec::GetImageCellResponse> {
    let generation = client.client.connection_generation();
    let local_epoch = if let Some(relay) = &relay {
        let state = relay.state.lock();
        anyhow::ensure!(
            request
                .image_epoch
                .is_none_or(|epoch| epoch == state.image_epoch),
            "Kitty image epoch changed"
        );
        request.image_epoch = state.remote_image_epoch;
        Some(state.image_epoch)
    } else {
        anyhow::ensure!(
            request.image_epoch.is_none(),
            "Kitty image subscription expired"
        );
        None
    };
    let local_pane = request.pane_id;
    request.pane_id = remote_pane;
    let response = client.client.send_pdu(Pdu::GetKittyImage(request)).await?;
    anyhow::ensure!(
        client.client.connection_generation() == generation,
        "Kitty image connection changed"
    );
    if let Some(relay) = &relay {
        anyhow::ensure!(
            Some(relay.state.lock().image_epoch) == local_epoch,
            "Kitty image epoch changed"
        );
    }
    match response {
        Pdu::GetImageCellResponse(mut response) => {
            anyhow::ensure!(response.pane_id == remote_pane, "Kitty image pane mismatch");
            response.pane_id = local_pane;
            Ok(response)
        }
        Pdu::ErrorResponse(error) => anyhow::bail!("{}", error.reason),
        _ => anyhow::bail!("unexpected Kitty image response"),
    }
}

struct Subscription(Arc<Relay>);
impl std::fmt::Debug for Subscription {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("KittySubscription")
            .field(&self.0.remote_pane)
            .finish()
    }
}
impl KittyFrameSubscription for Subscription {}
impl Drop for Subscription {
    fn drop(&mut self) {
        self.0.state.lock().remove_listener();
        self.0.kick();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wezterm_term::kitty_animation::{KittyAnimation, Playback};

    fn reply(revision: u64, now_ms: u64) -> KittyFrameSelections {
        let mut animation = KittyAnimation::new([100, 200], 1000);
        animation.mode = Playback::Running;
        KittyFrameSelections {
            pane_id: 7,
            image_epoch: 0,
            now_ms,
            revision,
            selections: vec![KittyFrameSelection { relative_placements: Vec::new(), virtual_placements: Vec::new(),
                data_generation: 0,
                image_id: 1,
                data_hash: [3; 32],
                animation,
            }],
        }
    }

    #[test]
    fn listeners_share_one_subscription_and_release_metadata_with_the_last_viewer() {
        let mut state = State::default();
        assert_eq!(state.next(), None);
        state.add_listener();
        let request = state.next().unwrap();
        state.add_listener();
        assert_eq!(state.next(), Some(request));
        state.delivered(request);
        state.accept(0, reply(1, 1050), 5050, Some(5030)).unwrap();
        assert_eq!(state.next(), None);
        state.remove_listener();
        assert_eq!(state.next(), None);
        assert_eq!(state.selections.len(), 1);
        state.remove_listener();
        assert_eq!(state.selections.capacity(), 0);
        assert_eq!(state.remote_revision, None);
        assert!(!state.next().unwrap().subscribe);
        state.delivered(state.next().unwrap());
        assert_eq!(state.next(), None);
    }

    #[test]
    fn abandoning_an_inflight_subscription_still_unsubscribes_after_its_ack() {
        let mut state = State::default();
        state.add_listener();
        let subscribe = state.next().unwrap();
        state.remove_listener();
        state.delivered(subscribe);
        assert!(!state.accept(0, reply(1, 1050), 5050, Some(5030)).unwrap());
        let unsubscribe = state.next().unwrap();
        assert!(!unsubscribe.subscribe);
        state.add_listener();
        state.delivered(unsubscribe);
        assert!(state.next().unwrap().subscribe);
    }

    #[test]
    fn a_cancelled_subscribe_is_cleaned_up_even_when_the_server_rejects_its_snapshot() {
        let mut state = State::default();
        state.add_listener();
        // The server subscribes before waiting for the terminal lock. A busy
        // snapshot can fail after that subscription has already taken effect.
        state.possibly_subscribed = true;
        state.remove_listener();
        let cleanup = state.next().unwrap();
        assert!(!cleanup.subscribe);
        state.delivered(cleanup);
        assert_eq!(state.next(), None);
    }

    #[test]
    fn a_viewer_returning_before_unsubscribe_is_sent_refreshes_the_cleared_snapshot() {
        let mut state = State::default();
        state.add_listener();
        state.delivered(state.next().unwrap());
        state.remove_listener();
        state.add_listener();
        assert!(state.next().unwrap().subscribe);
    }

    #[test]
    fn snapshots_translate_clocks_and_a_late_rpc_cannot_rewind_a_push() {
        let mut state = State::default();
        state.add_listener();
        state.accept(0, reply(2, 1150), 5150, None).unwrap();
        // This older response improves the clock, but preserves revision 2.
        state.accept(0, reply(1, 1050), 5050, Some(5030)).unwrap();
        assert_eq!(state.remote_revision, Some(2));
        let (revision, _, selections) = state.snapshot(None).unwrap();
        let original = reply(2, 1150).selections.remove(0).animation;
        for local in [5140, 5190, 5290, 5990, 100_000] {
            let expected = original.sample(local - 3990);
            let actual = selections[0].animation.sample(local);
            assert_eq!(actual.frame, expected.frame);
            assert_eq!(actual.completed_loops, expected.completed_loops);
            assert_eq!(actual.next_at_ms, expected.next_at_ms.map(|t| t + 3990));
        }
        assert!(state.snapshot(Some(revision)).is_none());
        assert!(!state.accept(0, reply(1, 1050), 6000, Some(5030)).unwrap());
    }

    #[test]
    fn reconnect_resets_remote_revisions_and_rejects_old_connection_replies() {
        let mut state = State::default();
        state.add_listener();
        state.delivered(state.next().unwrap());
        state.accept(0, reply(99, 1050), 5050, None).unwrap();
        let previous = state.revision;
        assert!(state.reconnect(1));
        assert!(!state.accept(0, reply(100, 1200), 5200, None).unwrap());
        assert!(state.selections.is_empty());
        assert!(state.next().unwrap().subscribe);
        assert!(state.accept(1, reply(0, 1050), 5050, None).unwrap());
        assert!(state.revision > previous);
        assert_eq!(state.remote_revision, Some(0));
    }

    #[test]
    fn reconnect_cleans_a_cancelled_request_that_could_cross_the_transport_queue() {
        let mut state = State::default();
        state.add_listener();
        state.worker = true;
        let old = state.next().unwrap();
        state.remove_listener();
        state.reconnect(1);
        state.delivered(old);
        let cleanup = state.next().unwrap();
        assert!(!cleanup.subscribe);
        assert_eq!(cleanup.generation, 1);
        state.delivered(cleanup);
        assert_eq!(state.next(), None);
    }

    #[test]
    fn malformed_or_over_budget_metadata_does_not_replace_a_valid_snapshot() {
        let mut state = State::default();
        state.add_listener();
        state.accept(0, reply(1, 1050), 5050, None).unwrap();
        let previous = state.snapshot(None);
        let mut duplicate = reply(2, 1050);
        duplicate.selections.push(duplicate.selections[0].clone());
        assert!(state.accept(0, duplicate, 5050, None).is_err());
        let mut invalid = reply(2, 1050);
        invalid.selections[0].animation.frame = 3;
        assert!(state.accept(0, invalid, 5050, None).is_err());
        let mut oversized = reply(2, 1050);
        let selection = KittyFrameSelection { relative_placements: Vec::new(), virtual_placements: Vec::new(),
            data_generation: 0,
            image_id: 1,
            data_hash: [0; 32],
            animation: KittyAnimation::new(std::iter::repeat_n(1, 4096), 1000),
        };
        oversized.selections = (0..257)
            .map(|id| KittyFrameSelection { relative_placements: Vec::new(),
                data_generation: 0,
                image_id: id,
                ..selection.clone()
            })
            .collect();
        assert!(state.accept(0, oversized, 5050, None).is_err());
        assert_eq!(state.snapshot(None), previous);
    }

    #[test]
    fn an_upstream_proxy_epoch_change_propagates_without_a_transport_reconnect() {
        let mut state = State::default();
        state.add_listener();
        state.accept(0, reply(1, 1050), 5050, None).unwrap();
        let before = state.image_epoch;
        let mut restored = reply(2, 1100);
        restored.image_epoch = 9;
        state.accept(0, restored, 5100, None).unwrap();
        assert!(state.image_epoch > before);
        assert_eq!(state.remote_image_epoch, Some(9));
        let epoch = state.image_epoch;
        state.accept(0, reply(1, 1050), 5150, Some(5030)).unwrap();
        assert_eq!(state.image_epoch, epoch);
        assert_eq!(state.remote_image_epoch, Some(9));
        assert_eq!(state.snapshot(None).unwrap().1, epoch);
    }

    #[test]
    fn failed_delivery_does_not_spin_and_reconnect_can_retry() {
        let mut state = State::default();
        state.add_listener();
        state.failed = state.next();
        assert_eq!(state.next(), None);
        state.reconnect(1);
        assert!(state.next().unwrap().subscribe);
    }
}
