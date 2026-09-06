//! The transport-neutral half of a connection: what may be sent before
//! the server knows who we are, the phases a connection goes through, how
//! answers find their requests, when a silent link is declared dead, and
//! how a failed version handshake is described. The desktop client drives
//! these from its socket loop; a browser client from a WebSocket.
use crate::clock::Timestamp;
use codec::{CorruptResponse, Pdu, CODEC_VERSION};
use std::collections::{HashMap, VecDeque};
use std::time::Duration;
use thiserror::Error;

#[derive(Error, Debug)]
#[error("ChannelSendError")]
pub struct ChannelSendError;

pub struct RegistrationBarrier<T> {
    complete: bool,
    deferred: VecDeque<T>,
}

impl<T> RegistrationBarrier<T> {
    pub fn new() -> Self {
        Self {
            complete: false,
            deferred: VecDeque::new(),
        }
    }

    pub fn submit(&mut self, item: T, registration_required: bool) -> Option<T> {
        if registration_required && !self.complete {
            self.deferred.push_back(item);
            None
        } else {
            Some(item)
        }
    }

    pub fn complete(&mut self) -> VecDeque<T> {
        self.complete = true;
        std::mem::take(&mut self.deferred)
    }

    pub fn drain(&mut self) -> VecDeque<T> {
        std::mem::take(&mut self.deferred)
    }

    pub fn is_complete(&self) -> bool {
        self.complete
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ConnectionPhase {
    Connecting = 0,
    Registering = 1,
    Syncing = 2,
    Ready = 3,
    Reconnecting = 4,
    Suspended = 5,
    Detached = 6,
}

impl ConnectionPhase {
    pub fn from_u8(value: u8) -> Self {
        match value {
            0 => Self::Connecting,
            1 => Self::Registering,
            2 => Self::Syncing,
            3 => Self::Ready,
            4 => Self::Reconnecting,
            5 => Self::Suspended,
            _ => Self::Detached,
        }
    }
}

/// The server accepted the connection but never answered the version
/// handshake. Unlike [`IncompatibleVersionError`] this is a transient
/// condition — a wedged or overloaded server (e.g. one pane consuming all
/// of its resources) or a stalled link — and retrying can succeed, so it
/// must never be classified as fatal.
#[derive(Error, Debug, Clone, PartialEq, Eq)]
#[error(
    "The server did not answer the version handshake within {timeout_secs} \
     seconds. The server may be overloaded or wedged, or the link may have \
     stalled. This is transient — it is NOT a version mismatch — and \
     reconnecting can succeed."
)]
pub struct VersionHandshakeStalled {
    pub timeout_secs: u64,
}

/// Human-readable description of a failed version handshake, for errors
/// that are not a timeout. The wording deliberately does not claim a
/// version mismatch: a real mismatch is detected from an actual response
/// ([`IncompatibleVersionError`]); landing here means no usable answer
/// arrived at all, which is most often a transport or server-health
/// problem.
pub fn describe_handshake_failure(err: &anyhow::Error) -> String {
    if err.root_cause().is::<CorruptResponse>() {
        "Received an implausible and likely corrupt response from \
         the server. This can happen if the remote host outputs \
         to stdout prior to running commands. \
         Check your shell startup!"
            .to_string()
    } else if err.root_cause().is::<ChannelSendError>() {
        "Internal channel was closed prior to sending request. \
         This may indicate that the remote host output invalid data \
         to stdout prior to running the requested command. \
         Check your shell startup!"
            .to_string()
    } else {
        format!(
            "The version handshake with the server failed: '{err}'. \
             Possible causes: the connection or the server stalled before \
             answering; the remote host printed to stdout during shell \
             startup (check your shell startup files); or the server build \
             is too old to answer at all. An actual version mismatch is \
             reported explicitly, so do not assume one from this message."
        )
    }
}

/// Describe a server whose build differs from ours, or `None` when they match.
///
/// [`CODEC_VERSION`] guards the wire format, not behaviour: two builds that
/// differ only by a behaviour change still carry the same codec version and
/// handshake without complaint. That is how a mux server left running from a
/// days-old build goes on serving live shells while looking healthy to a
/// freshly built client -- the failure this reports is invisible otherwise.
///
/// Reported through the log rather than the connection UI on purpose. A
/// *remote* server legitimately runs its own build, and nagging on every
/// connect would train the warning away before it ever caught the local case
/// it exists for.
pub fn describe_server_build_mismatch(local: &str, remote: &str) -> Option<String> {
    if local == remote {
        return None;
    }
    Some(format!(
        "mux server is running {remote}, this client is {local}. \
         Codec version {CODEC_VERSION} matches, so they interoperate, but the \
         server may be serving behaviour from an older build; restart it if \
         that is not deliberate."
    ))
}

/// Whether the server answering the handshake is the process asking.
pub fn leads_back_to_this_process(own_server_id: Option<&str>, server_id: &str) -> bool {
    own_server_id.is_some_and(|own| own == server_id)
}

/// Answers matched to the requests waiting for them, by serial. Serial 0
/// is the server speaking unasked and never enters the table.
pub struct SerialTable<W> {
    next_serial: u64,
    waiting: HashMap<u64, W>,
}

/// What became of an answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    /// Handed to the request that waited for it.
    Delivered,
    /// The request stopped waiting (a bootstrap RPC that timed out and
    /// dropped its receiver). That abandons one request, not the
    /// connection: killing it here turned a transient handshake stall into
    /// a permanent detach.
    Discarded,
    /// Nothing ever asked with this serial: the stream is out of step and
    /// the connection cannot be trusted.
    Unmatched,
}

impl<W> Default for SerialTable<W> {
    fn default() -> Self {
        Self::new()
    }
}

impl<W> SerialTable<W> {
    pub fn new() -> Self {
        Self {
            next_serial: 1,
            waiting: HashMap::new(),
        }
    }

    /// The serial the next request goes out with.
    pub fn next_serial(&self) -> u64 {
        self.next_serial
    }

    /// Take a serial for a request; `waiter` gets its answer.
    pub fn register(&mut self, waiter: W) -> u64 {
        let serial = self.next_serial;
        self.next_serial += 1;
        self.waiting.insert(serial, waiter);
        serial
    }

    /// Take a serial for a request nothing waits on (a keepalive ping).
    pub fn allocate(&mut self) -> u64 {
        let serial = self.next_serial;
        self.next_serial += 1;
        serial
    }

    /// Route `pdu`, answered on `serial`. `deliver` hands it to the waiter
    /// and says whether anyone was still listening.
    pub fn answer(
        &mut self,
        serial: u64,
        pdu: Pdu,
        deliver: impl FnOnce(W, Pdu) -> bool,
    ) -> Answer {
        match self.waiting.remove(&serial) {
            Some(waiter) => {
                if deliver(waiter, pdu) {
                    Answer::Delivered
                } else {
                    Answer::Discarded
                }
            }
            None => Answer::Unmatched,
        }
    }

    /// Every waiter, for failing them all when the connection ends.
    pub fn drain(&mut self) -> Vec<W> {
        self.waiting.drain().map(|(_, waiter)| waiter).collect()
    }

    pub fn is_empty(&self) -> bool {
        self.waiting.is_empty()
    }
}

/// Application-level keepalive: a transport that died without a reset
/// (VPN egress rotation, a sleepy NAT) otherwise hangs silently until the
/// next write. After `interval` without a byte received, ping; the pong
/// must arrive before the following tick.
pub struct Keepalive {
    interval: Duration,
    deadline: Timestamp,
    pending_ping: Option<(u64, Timestamp)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeepaliveAction {
    /// Put a ping with this serial on the wire.
    SendPing(u64),
    /// The ping with this serial went unanswered for `waited`: the
    /// transport is presumed dead.
    DeclareDead { serial: u64, waited: Duration },
}

impl Keepalive {
    pub fn new(interval: Duration, now: Timestamp) -> Self {
        Self {
            interval,
            deadline: now + interval,
            pending_ping: None,
        }
    }

    /// When the next tick is due.
    pub fn deadline(&self) -> Timestamp {
        self.deadline
    }

    /// A byte arrived. Traffic postpones the next ping, but not the
    /// verdict on one already sent: a server that keeps pushing output
    /// while never answering is still one that never answers.
    pub fn received(&mut self, now: Timestamp) {
        if self.pending_ping.is_none() {
            self.deadline = now + self.interval;
        }
    }

    /// An answer arrived on `serial`; true when it was the pending ping's.
    pub fn answered(&mut self, serial: u64, now: Timestamp) -> bool {
        match self.pending_ping {
            Some((pending, _)) if pending == serial => {
                self.pending_ping = None;
                self.deadline = now + self.interval;
                true
            }
            _ => false,
        }
    }

    /// The deadline passed. `serial` allocates the ping's serial when one
    /// is to be sent.
    pub fn tick(&mut self, now: Timestamp, serial: impl FnOnce() -> u64) -> KeepaliveAction {
        if let Some((serial, sent)) = self.pending_ping.take() {
            return KeepaliveAction::DeclareDead {
                serial,
                waited: now.saturating_duration_since(sent),
            };
        }
        let serial = serial();
        self.pending_ping = Some((serial, now));
        self.deadline = now + self.interval;
        KeepaliveAction::SendPing(serial)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_identical_build_is_not_worth_warning_about() {
        assert_eq!(
            describe_server_build_mismatch("20260814-011230-aaef9bfb", "20260814-011230-aaef9bfb"),
            None
        );
    }

    #[test]
    fn a_server_from_another_build_names_both_sides() {
        let mismatch =
            describe_server_build_mismatch("20260815-150100-eab2bf7c", "20260813-012155-816da4db")
                .expect("differing builds are reported");
        // Which side is which is the whole point of the message: the same two
        // version strings in the wrong order sends the user to restart the
        // wrong process.
        assert!(mismatch.contains("server is running 20260813-012155-816da4db"));
        assert!(mismatch.contains("client is 20260815-150100-eab2bf7c"));
    }

    #[test]
    fn registration_barrier_sends_bootstrap_before_deferred_rpcs() {
        let mut barrier = RegistrationBarrier::new();
        assert_eq!(barrier.submit("palette", true), None);
        assert_eq!(barrier.submit("viewport", true), None);
        assert_eq!(
            barrier.submit("GetCodecVersion", false),
            Some("GetCodecVersion")
        );
        assert_eq!(barrier.submit("SetClientId", false), Some("SetClientId"));
        assert_eq!(
            barrier.complete().into_iter().collect::<Vec<_>>(),
            vec!["palette", "viewport"]
        );
        assert_eq!(barrier.submit("focus", true), Some("focus"));
    }

    fn pong() -> Pdu {
        Pdu::Pong(codec::Pong {})
    }

    #[test]
    fn an_answer_nobody_waits_for_is_discarded_not_fatal() {
        let mut table: SerialTable<Option<()>> = SerialTable::new();
        let serial = table.register(Some(()));
        // the waiter is gone: delivery finds nobody listening
        assert_eq!(
            table.answer(serial, pong(), |_, _| false),
            Answer::Discarded
        );
        assert!(table.is_empty());
    }

    #[test]
    fn an_answer_for_an_unknown_serial_is_fatal() {
        let mut table: SerialTable<()> = SerialTable::new();
        let serial = table.register(());
        assert_eq!(table.answer(serial, pong(), |_, _| true), Answer::Delivered);
        assert_eq!(table.answer(serial, pong(), |_, _| true), Answer::Unmatched);
        assert_eq!(table.answer(99, pong(), |_, _| true), Answer::Unmatched);
        assert_eq!(
            table.allocate(),
            2,
            "a ping takes the next serial without waiting"
        );
        assert_eq!(table.next_serial(), 3);
    }

    fn at(secs: u64) -> Timestamp {
        Timestamp::from_micros(secs * 1_000_000)
    }

    #[test]
    fn traffic_postpones_the_next_ping_but_not_the_verdict_on_one_sent() {
        let mut keepalive = Keepalive::new(Duration::from_secs(15), at(0));
        assert_eq!(keepalive.deadline(), at(15));
        keepalive.received(at(10));
        assert_eq!(keepalive.deadline(), at(25), "a byte pushed the tick back");
        let mut serials = 5u64..;
        assert_eq!(
            keepalive.tick(at(25), || serials.next().unwrap()),
            KeepaliveAction::SendPing(5)
        );
        keepalive.received(at(30));
        assert_eq!(
            keepalive.deadline(),
            at(40),
            "output during a pending ping does not postpone its verdict"
        );
        assert!(
            !keepalive.answered(4, at(31)),
            "some other answer is not the pong"
        );
        assert!(keepalive.answered(5, at(31)));
        assert_eq!(
            keepalive.deadline(),
            at(46),
            "the pong restarts the idle clock"
        );
    }

    #[test]
    fn a_missed_pong_declares_the_transport_dead_on_the_next_tick() {
        let mut keepalive = Keepalive::new(Duration::from_secs(15), at(0));
        assert_eq!(keepalive.tick(at(15), || 7), KeepaliveAction::SendPing(7));
        assert_eq!(
            keepalive.tick(at(30), || 8),
            KeepaliveAction::DeclareDead {
                serial: 7,
                waited: Duration::from_secs(15)
            }
        );
    }
}

#[cfg(test)]
mod handshake_classification_tests {
    use super::*;

    #[test]
    fn unknown_failure_does_not_claim_a_version_mismatch() {
        let err = anyhow::anyhow!("Client was destroyed");
        let msg = describe_handshake_failure(&err);
        assert!(!msg.contains("install a compatible"), "{msg}");
        assert!(msg.contains("Client was destroyed"), "{msg}");
    }

    #[test]
    fn channel_send_error_keeps_its_specific_guidance() {
        let err = anyhow::Error::new(ChannelSendError).context("send_pdu");
        let msg = describe_handshake_failure(&err);
        assert!(msg.contains("Internal channel was closed"), "{msg}");
    }

    #[test]
    fn stalled_handshake_reads_as_transient() {
        let msg = VersionHandshakeStalled { timeout_secs: 60 }.to_string();
        assert!(msg.contains("transient"), "{msg}");
        assert!(msg.contains("NOT a version mismatch"), "{msg}");
    }

    #[test]
    fn a_server_with_our_own_id_is_never_attached() {
        assert!(super::leads_back_to_this_process(Some("abc"), "abc"));
        assert!(!super::leads_back_to_this_process(Some("abc"), "def"));
        assert!(
            !super::leads_back_to_this_process(None, "abc"),
            "a process without a mux of its own cannot be the server"
        );
    }
}
