//! Input for a remote pane: one ordered queue per pane, drained onto the
//! wire in the order the user gave it, each request sent the moment the
//! one before it is on the wire.
use crate::host::{LinkError, PduLink};
use crate::Lock;
use codec::{
    EraseScrollbackRequest, InputSerial, Pdu, SendKeyDown, SendMouseEvent, SendPaste, WriteToPane,
};
use futures_util::stream::{FuturesUnordered, StreamExt};
use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use termwiz::input::KeyEvent;
use thinkterm_proto::{PaneId, ScrollbackEraseMode, TabId};
use wezterm_term::MouseEvent;

/// A boxed future for a link whose futures are `Send` (the desktop).
pub type SendFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;
/// A boxed future for a link whose futures are not (a browser).
pub type LocalFuture<T> = Pin<Box<dyn Future<Output = T> + 'static>>;

/// One thing the user gave a remote pane.
///
/// Keys, pastes and mouse reports travel as their own requests; the pty
/// bytes the GUI encodes itself (kitty-protocol and win32-input-mode keys,
/// text from the input method, SendString) go as WriteToPane. The four
/// used to leave by separate paths, and the byte path waited for each
/// answer before sending more, so a key pressed while a write was still
/// unanswered overtook the text queued behind that write: Enter before
/// the sentence it was meant to end. Everything now goes through one
/// queue per pane, in the order it was given.
pub enum PaneInput {
    Bytes(Vec<u8>),
    Key {
        event: KeyEvent,
        input_serial: InputSerial,
    },
    Paste(String),
    Mouse(MouseEvent),
    EraseScrollback(ScrollbackEraseMode),
}

/// What an input weighs against `INPUT_QUEUE_LIMIT` when it is all
/// bookkeeping.
pub const INPUT_ITEM_FLOOR: usize = 64;

impl PaneInput {
    pub fn weight(&self) -> usize {
        match self {
            PaneInput::Bytes(data) => data.len().max(INPUT_ITEM_FLOOR),
            PaneInput::Paste(text) => text.len().max(INPUT_ITEM_FLOOR),
            PaneInput::Key { .. } | PaneInput::Mouse(_) | PaneInput::EraseScrollback(_) => {
                INPUT_ITEM_FLOOR
            }
        }
    }

    pub fn describe(&self) -> String {
        match self {
            PaneInput::Bytes(data) => format!("{} bytes", data.len()),
            PaneInput::Key { .. } => "a key".to_string(),
            PaneInput::Paste(text) => format!("a paste of {} bytes", text.len()),
            PaneInput::Mouse(_) => "a mouse report".to_string(),
            PaneInput::EraseScrollback(_) => "a scrollback erase".to_string(),
        }
    }

    /// Fold `next` into this one when the two can travel as a single
    /// request: bytes after bytes, a mouse report over the one before it.
    /// Nothing folds across a key or a paste, so the order the user gave
    /// is the order the pty sees. Hands `next` back when it has to stay
    /// its own request.
    pub fn absorb(&mut self, next: PaneInput) -> Option<PaneInput> {
        match (self, next) {
            (PaneInput::Bytes(data), PaneInput::Bytes(more)) => {
                data.extend_from_slice(&more);
                None
            }
            (PaneInput::Mouse(last), PaneInput::Mouse(event)) => {
                crate::mouse::coalesce(last, event).map(PaneInput::Mouse)
            }
            (_, next) => Some(next),
        }
    }

    pub fn into_pdu(self, pane_id: PaneId) -> Pdu {
        match self {
            PaneInput::Bytes(data) => Pdu::WriteToPane(WriteToPane { pane_id, data }),
            PaneInput::Key {
                event,
                input_serial,
            } => Pdu::SendKeyDown(SendKeyDown {
                pane_id,
                event,
                input_serial,
            }),
            PaneInput::Paste(data) => Pdu::SendPaste(SendPaste { pane_id, data }),
            PaneInput::Mouse(event) => Pdu::SendMouseEvent(SendMouseEvent { pane_id, event }),
            PaneInput::EraseScrollback(erase_mode) => {
                Pdu::EraseScrollbackRequest(EraseScrollbackRequest {
                    pane_id,
                    erase_mode,
                })
            }
        }
    }
}

/// Input for a remote pane the server has not answered: what waits to be
/// sent, and how much is on the wire.
#[derive(Default)]
pub struct InputQueue {
    pending: VecDeque<PaneInput>,
    /// The weight of `pending`.
    queued: usize,
    /// The weight sent and not yet answered.
    in_flight: usize,
    draining: bool,
}

/// How much input may wait for the link, queued or sent and unanswered,
/// before more is refused: a paste, not a runaway.
const INPUT_QUEUE_LIMIT: usize = 4 * 1024 * 1024;

/// The queue is full: the link has answered nothing for a while and
/// `INPUT_QUEUE_LIMIT` bytes of input already wait for it.
#[derive(Debug)]
pub struct InputQueueFull {
    waiting: usize,
}

impl std::fmt::Display for InputQueueFull {
    fn fmt(&self, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            fmt,
            "{} bytes of input are already waiting for the link to the remote pane",
            self.waiting
        )
    }
}

impl std::error::Error for InputQueueFull {}

impl InputQueue {
    /// Queue `input` behind what already waits; true when the caller has
    /// to start the drain. Over the limit the input is refused rather than
    /// dropped and reported as sent: the caller is told, and what reaches
    /// the remote pane is either all of an input or none of it.
    pub fn push(&mut self, input: PaneInput) -> Result<bool, InputQueueFull> {
        let waiting = self.queued + self.in_flight;
        if waiting + input.weight() > INPUT_QUEUE_LIMIT {
            return Err(InputQueueFull { waiting });
        }
        let input = match self.pending.back_mut() {
            Some(last) => {
                let before = last.weight();
                let left = last.absorb(input);
                self.queued = self.queued - before + last.weight();
                left
            }
            None => Some(input),
        };
        if let Some(input) = input {
            self.queued += input.weight();
            self.pending.push_back(input);
        }
        Ok(!std::mem::replace(&mut self.draining, true))
    }

    /// The next input to send, counted as in flight from here on, or None
    /// once nothing waits: the drain is over and the next push starts a
    /// new one. The check and the hand-back happen under the one lock, so
    /// no push can slip between them and find the queue neither drained
    /// nor draining.
    pub fn take(&mut self) -> Option<PaneInput> {
        match self.pending.pop_front() {
            Some(input) => {
                let weight = input.weight();
                self.queued -= weight;
                self.in_flight += weight;
                Some(input)
            }
            None => {
                self.draining = false;
                None
            }
        }
    }

    /// `weight` of input was answered, or never went: it no longer waits.
    pub fn settle(&mut self, weight: usize) {
        self.in_flight = self.in_flight.saturating_sub(weight);
    }
}

/// Marks the drain over if its task ends before it reaches the end of the
/// queue (a panic caught by the executor, a scheduler torn down with it
/// queued); every later input would otherwise wait for a drain that never
/// comes. A drain that did reach the end handed the queue back itself and
/// disarms this, so it cannot undo a drain the next push already started.
struct InputDraining {
    queue: Arc<Lock<InputQueue>>,
    done: bool,
}

impl Drop for InputDraining {
    fn drop(&mut self) {
        if !self.done {
            self.queue.lock().draining = false;
        }
    }
}

/// Settles an input's weight when its answer arrives, or when the answer
/// is given up on: dropped with the future, it settles all the same.
struct Settling {
    queue: Arc<Lock<InputQueue>>,
    weight: usize,
}

impl Drop for Settling {
    fn drop(&mut self) {
        self.queue.lock().settle(self.weight);
    }
}

/// The wire as the input drain needs it: `PduLink` plus the per-tab gate.
/// The future is an associated type so a desktop link can hand out `Send`
/// futures and a browser link local ones, with no `Send` bound written
/// here (see `SendFuture` and `LocalFuture`).
pub trait PaneLink: PduLink {
    type Prepare: Future<Output = anyhow::Result<bool>> + 'static;

    /// Whether this client may drive the tab right now; a claim may have
    /// to reach the server first.
    fn prepare(&self, remote_tab_id: TabId) -> Self::Prepare;
}

/// Sends the pane's queued input in order, each request the moment the
/// one before it is on the wire, then waits for the answers. No request
/// waits for an answer before the next is sent: the wire keeps the order,
/// and the server applies one connection's input to a pane in the order
/// it arrives. The answers only settle what counts against
/// `INPUT_QUEUE_LIMIT`, so a stalled link ends in refused input, not in
/// input sent out of order.
pub async fn drain_pane_inputs<L: PaneLink>(
    link: &L,
    remote_pane_id: PaneId,
    remote_tab_id: Arc<AtomicUsize>,
    queue: Arc<Lock<InputQueue>>,
) {
    let mut answers = FuturesUnordered::new();
    let mut draining = InputDraining {
        queue: Arc::clone(&queue),
        done: false,
    };
    loop {
        let Some(input) = queue.lock().take() else {
            draining.done = true;
            break;
        };
        let what = input.describe();
        let settling = Settling {
            queue: Arc::clone(&queue),
            weight: input.weight(),
        };
        let remote_tab_id = remote_tab_id.load(Ordering::Relaxed);
        match link.prepare(remote_tab_id).await {
            Ok(true) => {}
            Ok(false) => {
                log::warn!(
                    "dropping {what} for remote pane {remote_pane_id}: this client may \
                     not drive the tab right now"
                );
                continue;
            }
            Err(err) => {
                log::error!("dropping {what} for remote pane {remote_pane_id}: {err:#}");
                continue;
            }
        }
        let answer = link.request(input.into_pdu(remote_pane_id));
        answers.push(async move {
            let _settling = settling;
            if let Err(err) = answered(answer.await) {
                log::error!("sending {what} to remote pane {remote_pane_id}: {err:#}");
            }
        });
    }
    drop(draining);
    while answers.next().await.is_some() {}
}

/// Queue `input` behind everything given before it; true when the caller
/// has to start the drain.
pub fn push_input(queue: &Lock<InputQueue>, input: PaneInput) -> Result<bool, InputQueueFull> {
    queue.lock().push(input)
}

fn answered(answer: Result<Pdu, LinkError>) -> anyhow::Result<()> {
    match answer? {
        Pdu::UnitResponse(_) => Ok(()),
        Pdu::ErrorResponse(err) => anyhow::bail!(err.reason),
        other => anyhow::bail!("unexpected response {other:?}"),
    }
}

#[cfg(test)]
mod input_queue_tests {
    use super::*;
    use codec::UnitResponse;
    use std::sync::atomic::AtomicBool;
    use wezterm_term::{KeyCode, KeyModifiers, MouseButton, MouseEventKind};

    fn bytes(text: &str) -> PaneInput {
        PaneInput::Bytes(text.as_bytes().to_vec())
    }

    fn enter() -> PaneInput {
        PaneInput::Key {
            event: KeyEvent {
                key: KeyCode::Enter,
                modifiers: KeyModifiers::NONE,
            },
            input_serial: InputSerial::empty(),
        }
    }

    fn mouse(kind: MouseEventKind, button: MouseButton, x: usize) -> PaneInput {
        PaneInput::Mouse(MouseEvent {
            kind,
            x,
            y: 0,
            x_pixel_offset: 0,
            y_pixel_offset: 0,
            button,
            modifiers: KeyModifiers::NONE,
        })
    }

    fn tag(input: &PaneInput) -> String {
        match input {
            PaneInput::Bytes(data) => format!("bytes:{}", String::from_utf8_lossy(data)),
            PaneInput::Key { .. } => "key".to_string(),
            PaneInput::Paste(text) => format!("paste:{text}"),
            PaneInput::EraseScrollback(_) => "erase".to_string(),
            PaneInput::Mouse(event) => match (&event.kind, &event.button) {
                (MouseEventKind::Move, _) => format!("mouse:move@{}", event.x),
                (_, MouseButton::WheelDown(n)) => format!("mouse:wheeldown{n}"),
                _ => "mouse".to_string(),
            },
        }
    }

    fn pdu_tag(pdu: &Pdu) -> String {
        match pdu {
            Pdu::WriteToPane(write) => {
                format!("bytes:{}", String::from_utf8_lossy(&write.data))
            }
            Pdu::SendKeyDown(_) => "key".to_string(),
            Pdu::SendPaste(paste) => format!("paste:{}", paste.data),
            Pdu::SendMouseEvent(_) => "mouse".to_string(),
            other => format!("{other:?}"),
        }
    }

    /// A link that records what went on the wire and answers only when
    /// the test says so.
    #[derive(Default)]
    struct RecordingLink {
        wire: Lock<Vec<String>>,
        answers: Lock<Vec<async_channel::Sender<Result<Pdu, LinkError>>>>,
        refuse: AtomicBool,
    }

    impl RecordingLink {
        fn answer_everything(&self) {
            for answer in self.answers.lock().drain(..) {
                answer
                    .try_send(Ok(Pdu::UnitResponse(UnitResponse {})))
                    .unwrap();
            }
        }
    }

    impl PduLink for RecordingLink {
        type Request = LocalFuture<Result<Pdu, LinkError>>;

        fn request(&self, pdu: Pdu) -> Self::Request {
            self.wire.lock().push(pdu_tag(&pdu));
            let (tx, rx) = async_channel::bounded(1);
            self.answers.lock().push(tx);
            Box::pin(async move {
                rx.recv().await.map_err(|_| LinkError {
                    message: "the answer was dropped".into(),
                    retryable: false,
                })?
            })
        }

        fn is_reconnectable(&self) -> bool {
            false
        }

        fn connection_generation(&self) -> u64 {
            1
        }
    }

    impl PaneLink for RecordingLink {
        type Prepare = LocalFuture<anyhow::Result<bool>>;

        fn prepare(&self, _remote_tab_id: TabId) -> Self::Prepare {
            let allowed = !self.refuse.load(Ordering::SeqCst);
            Box::pin(async move { Ok(allowed) })
        }
    }

    /// The scenario that was wrong: text from the input method is on the
    /// wire and unanswered, more text arrives, then Enter. Enter must leave
    /// after that text, and nothing may wait for the first answer.
    #[test]
    fn enter_leaves_behind_the_text_queued_before_it_without_waiting_for_an_answer() {
        let link = Arc::new(RecordingLink::default());
        let queue: Arc<Lock<InputQueue>> = Default::default();
        let tab = Arc::new(AtomicUsize::new(0));
        let ex = async_executor::LocalExecutor::new();
        let drain = || {
            ex.spawn(drain_pane_inputs(
                &*link,
                7,
                Arc::clone(&tab),
                Arc::clone(&queue),
            ))
        };

        assert!(queue.lock().push(bytes("ni")).unwrap());
        let first = drain();
        while ex.try_tick() {}
        assert_eq!(*link.wire.lock(), ["bytes:ni"], "sent at once");
        assert_eq!(queue.lock().in_flight, INPUT_ITEM_FLOOR, "and unanswered");
        assert!(
            !queue.lock().draining,
            "an unanswered request does not hold the drain"
        );

        assert!(
            queue.lock().push(bytes("hao")).unwrap(),
            "a new drain starts"
        );
        assert!(
            !queue.lock().push(enter()).unwrap(),
            "and Enter rides along"
        );
        let second = drain();
        while ex.try_tick() {}
        assert_eq!(
            *link.wire.lock(),
            ["bytes:ni", "bytes:hao", "key"],
            "Enter left after the text, while ni is still unanswered"
        );
        assert_eq!(queue.lock().in_flight, 3 * INPUT_ITEM_FLOOR);

        link.answer_everything();
        futures_lite::future::block_on(ex.run(async {
            first.await;
            second.await;
        }));
        assert_eq!(queue.lock().in_flight, 0, "every answer settled its input");
    }

    #[test]
    fn input_this_client_may_not_send_is_dropped_and_settled() {
        let link = Arc::new(RecordingLink::default());
        link.refuse.store(true, Ordering::SeqCst);
        let queue: Arc<Lock<InputQueue>> = Default::default();
        assert!(queue.lock().push(enter()).unwrap());
        let ex = async_executor::LocalExecutor::new();
        futures_lite::future::block_on(ex.run(drain_pane_inputs(
            &*link,
            7,
            Arc::new(AtomicUsize::new(0)),
            Arc::clone(&queue),
        )));
        assert!(link.wire.lock().is_empty(), "nothing went");
        let queue = queue.lock();
        assert_eq!(queue.in_flight, 0, "and nothing is counted as waiting");
        assert!(!queue.draining, "the drain is over");
    }

    #[test]
    fn only_neighbours_of_the_same_kind_travel_together() {
        let mut queue = InputQueue::default();
        queue.push(bytes("a")).unwrap();
        queue.push(bytes("b")).unwrap();
        queue.push(enter()).unwrap();
        queue.push(bytes("c")).unwrap();
        queue
            .push(mouse(MouseEventKind::Move, MouseButton::None, 1))
            .unwrap();
        queue
            .push(mouse(MouseEventKind::Move, MouseButton::None, 2))
            .unwrap();
        queue.push(PaneInput::Paste("p".to_string())).unwrap();
        queue
            .push(mouse(MouseEventKind::Press, MouseButton::WheelDown(3), 0))
            .unwrap();
        queue
            .push(mouse(MouseEventKind::Press, MouseButton::WheelDown(1), 0))
            .unwrap();
        assert_eq!(
            queue.queued,
            queue.pending.iter().map(PaneInput::weight).sum::<usize>(),
            "folding keeps the account right"
        );
        let sent: Vec<String> = std::iter::from_fn(|| queue.take())
            .map(|input| tag(&input))
            .collect();
        assert_eq!(
            sent,
            [
                "bytes:ab",
                "key",
                "bytes:c",
                "mouse:move@2",
                "paste:p",
                "mouse:wheeldown4"
            ]
        );
    }

    #[test]
    fn input_on_the_wire_counts_until_it_is_answered() {
        let mut queue = InputQueue::default();
        assert!(
            queue
                .push(PaneInput::Bytes(vec![b'x'; INPUT_QUEUE_LIMIT]))
                .is_ok(),
            "the limit itself fits"
        );
        let refused = queue
            .push(enter())
            .expect_err("a key on top of it does not");
        assert_eq!(refused.waiting, INPUT_QUEUE_LIMIT);
        assert_eq!(
            queue.take().map(|input| input.weight()),
            Some(INPUT_QUEUE_LIMIT),
            "what was queued is intact; the refused key is not in it"
        );
        assert!(
            queue.push(enter()).is_err(),
            "on the wire and unanswered, it still counts"
        );
        queue.settle(INPUT_QUEUE_LIMIT);
        assert!(queue.push(enter()).is_ok(), "answered, it makes room");
    }

    #[test]
    fn a_drain_that_ends_early_gives_the_queue_back() {
        let queue: Arc<Lock<InputQueue>> = Default::default();
        assert!(queue.lock().push(bytes("a")).unwrap());
        drop(InputDraining {
            queue: Arc::clone(&queue),
            done: false,
        });
        assert!(!queue.lock().draining, "the slot is free again");
        assert_eq!(
            tag(&queue.lock().take().unwrap()),
            "bytes:a",
            "and the bytes still wait for the next drain"
        );
    }

    #[test]
    fn a_drain_that_reached_the_end_does_not_undo_the_next_one() {
        let queue: Arc<Lock<InputQueue>> = Default::default();
        assert!(queue.lock().push(bytes("a")).unwrap());
        assert!(queue.lock().take().is_some());
        assert!(queue.lock().take().is_none(), "handed back under the lock");
        assert!(
            queue.lock().push(bytes("b")).unwrap(),
            "the next push starts a new drain"
        );
        drop(InputDraining {
            queue: Arc::clone(&queue),
            done: true,
        });
        assert!(
            queue.lock().draining,
            "the finished drain's guard is disarmed"
        );
    }
}
