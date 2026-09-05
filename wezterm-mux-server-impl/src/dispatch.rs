use crate::sessionhandler::{PduSender, SessionHandler};
use anyhow::Context;
use async_ossl::AsyncSslStream;
use codec::{DecodedPdu, Pdu};
use futures::FutureExt;
use mux::{Mux, MuxNotification};
use smol::prelude::*;
use smol::Async;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context as TaskContext, Poll};
use std::time::{Duration, Instant};
use wezterm_uds::UnixStream;

#[cfg(unix)]
pub trait AsRawDesc: std::os::unix::io::AsRawFd + std::os::fd::AsFd {}
#[cfg(windows)]
pub trait AsRawDesc: std::os::windows::io::AsRawSocket + std::os::windows::io::AsSocket {}

impl AsRawDesc for UnixStream {}
impl AsRawDesc for AsyncSslStream {}

#[derive(Debug)]
enum Item {
    Notif(MuxNotification),
    WritePdu(DecodedPdu),
    Readable,
    LivenessTick,
}

/// Noticing a client that died without closing its socket.
///
/// A socket only reports a peer that went away cleanly. A machine that
/// lost power, a VM paused, a laptop carried out of Wi-Fi range: none of
/// those send a FIN, and through an ssh proxy into a unix socket there is
/// no TCP keepalive to fall back on either. Until now such a client stayed
/// registered until a write to it finally failed, which takes the kernel's
/// retransmit timeout -- a quarter of an hour -- and for that long it kept
/// whatever it held, a TmuxLatest tab's viewport ownership included, so
/// every other client of that tab sat frozen at the dead machine's size.
///
/// So the connection keeps a clock: a client heard from recently is left
/// alone; one silent past `interval` is sent a Ping; one that does not
/// answer within `timeout` is dropped, which unregisters it the ordinary
/// way. Anything the client sends counts as an answer, not only the Pong.
#[derive(Debug)]
pub(crate) struct Liveness {
    interval: Duration,
    timeout: Duration,
    last_inbound: Instant,
    /// When the probe was decided on.
    probe_sent: Option<Instant>,
    /// When it actually reached the kernel. The answer is given `timeout`
    /// from here: the probe rides the same queue as pane output, and a
    /// slow link with a busy pane can take a while to carry it.
    probe_written: Option<Instant>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// Nothing to do yet.
    Fine,
    /// Send a Ping.
    Probe,
    /// The probe went unanswered; the client has been silent this long.
    Dead(Duration),
}

impl Liveness {
    /// Ten seconds of silence earns a probe, and twenty more without an
    /// answer ends the connection: a dead client is gone within half a
    /// minute. Generous, because the answer rides behind whatever pane
    /// output is queued for the client, and a slow link with a busy pane
    /// must not look dead.
    pub const INTERVAL: Duration = Duration::from_secs(10);
    pub const TIMEOUT: Duration = Duration::from_secs(20);

    pub fn new(now: Instant) -> Self {
        Self::with_timing(now, Self::INTERVAL, Self::TIMEOUT)
    }

    pub fn with_timing(now: Instant, interval: Duration, timeout: Duration) -> Self {
        Self {
            interval,
            timeout,
            last_inbound: now,
            probe_sent: None,
            probe_written: None,
        }
    }

    /// The client sent something.
    pub fn heard(&mut self, now: Instant) {
        self.last_inbound = now;
        self.probe_sent = None;
        self.probe_written = None;
    }

    /// The probe has left for the client.
    pub fn probe_written(&mut self, now: Instant) {
        if self.probe_sent.is_some() {
            self.probe_written = Some(now);
        }
    }

    /// When `check` next has something to say.
    pub fn next_check(&self) -> Instant {
        match self.probe_sent {
            Some(sent) => self.probe_written.unwrap_or(sent) + self.timeout,
            None => self.last_inbound + self.interval,
        }
    }

    pub fn check(&mut self, now: Instant) -> Verdict {
        if let Some(sent) = self.probe_sent {
            if now >= self.probe_written.unwrap_or(sent) + self.timeout {
                return Verdict::Dead(now - self.last_inbound);
            }
            return Verdict::Fine;
        }
        if now >= self.last_inbound + self.interval {
            self.probe_sent = Some(now);
            return Verdict::Probe;
        }
        Verdict::Fine
    }
}

/// One client connection, from accept to close. Runs on the connection
/// threads (see `connections`), never on the main thread: everything
/// here that changes the mux hops there explicitly, and everything that
/// does not is answered whatever the main thread is doing.
pub async fn process<T>(stream: T) -> anyhow::Result<()>
where
    T: 'static,
    T: std::io::Read,
    T: std::io::Write,
    T: AsRawDesc,
    T: std::fmt::Debug,
    T: async_io::IoSafe,
{
    let stream = smol::Async::new(stream)?;
    process_async(stream).await
}

pub async fn process_async<T>(stream: Async<T>) -> anyhow::Result<()>
where
    T: 'static,
    T: std::io::Read,
    T: std::io::Write,
    T: std::fmt::Debug,
    T: async_io::IoSafe,
{
    process_async_with(stream, Liveness::new(Instant::now())).await
}

pub(crate) async fn process_async_with<T>(
    mut stream: Async<T>,
    mut liveness: Liveness,
) -> anyhow::Result<()>
where
    T: 'static,
    T: std::io::Read,
    T: std::io::Write,
    T: std::fmt::Debug,
    T: async_io::IoSafe,
{
    log::trace!("process_async called");

    let (item_tx, item_rx) = smol::channel::unbounded::<Item>();

    let pdu_sender = PduSender::with_closed(
        {
            let item_tx = item_tx.clone();
            move |pdu| {
                item_tx
                    .try_send(Item::WritePdu(pdu))
                    .map_err(|e| anyhow::anyhow!("{:?}", e))
            }
        },
        {
            let item_tx = item_tx.clone();
            move || item_tx.is_closed()
        },
    );
    let mut handler = SessionHandler::new(pdu_sender);

    {
        // Notifications take one hop through the main thread's queue before
        // they reach this connection, and are turned into what the loop
        // needs while there. Ordering: a mutation runs on the main thread,
        // notifies in the middle of its work and sends its response at the
        // end; with this loop on its own thread, a notification handed
        // over directly would be on the wire before the response to the
        // request that caused it, where the single-threaded server always
        // sent the response first, and clients rely on that. Locking: what
        // a notification needs from the mux (a pane's status, a window's
        // workspace, the tree and session snapshots) is read on the main
        // thread, so this connection's thread never holds a mux lock while
        // it waits on a busy pane's terminal.
        let mux = Mux::get();
        let tx = item_tx.clone();
        mux.subscribe(move |n| {
            if tx.is_closed() {
                return false;
            }
            log::trace!("notification queued for the connection: {n:?}");
            let tx = tx.clone();
            promise::spawn::spawn_into_main_thread(async move {
                if let Some(item) = item_for_notification(n) {
                    if let Err(err) = tx.try_send(item) {
                        log::trace!("notification not delivered: {err}");
                    }
                }
            })
            .detach();
            true
        });
    }

    // Notification PDUs go through the same WritePdu queue as RPC
    // responses instead of being written inline: the WritePdu arm below is
    // the one place that tolerates a half-closed peer (BrokenPipe), and an
    // inline `?` write here would kill the whole connection — and with it
    // every pane's output push — the moment a notification raced a
    // disconnecting client. A failed try_send means the channel is closed
    // and the connection is already over.
    let send_notif_pdu = {
        let item_tx = item_tx.clone();
        move |pdu: Pdu| {
            let _ = item_tx.try_send(Item::WritePdu(DecodedPdu { serial: 0, pdu }));
        }
    };

    loop {
        let rx_msg = item_rx.recv();
        let wait_for_read = stream.readable().map(|_| Ok(Item::Readable));
        let tick = async {
            smol::Timer::at(liveness.next_check()).await;
            Ok(Item::LivenessTick)
        };

        let item = match smol::future::or(smol::future::or(rx_msg, wait_for_read), tick).await {
            Ok(Item::LivenessTick) => match liveness.check(Instant::now()) {
                Verdict::Fine => continue,
                // Through the write queue like every other push, so a
                // peer that is gone fails the write and ends this the
                // quick way.
                Verdict::Probe => {
                    send_notif_pdu(Pdu::Ping(codec::Ping {}));
                    continue;
                }
                Verdict::Dead(silence) => {
                    // The clock is believed only once the socket has been
                    // looked at. The race above is decided in order, and
                    // a wait for readability just built cannot finish on
                    // its first poll, while a timer already due does: an
                    // answer that arrived during a long write, and has
                    // been sitting in the socket since, would lose to the
                    // tick and the client be dropped as dead with its
                    // answer unread.
                    if !socket_has_data(&stream).await {
                        log::warn!(
                            "client silent for {silence:?} and did not answer a probe; \
                             dropping the connection"
                        );
                        return Ok(());
                    }
                    Ok(Item::Readable)
                }
            },
            other => other,
        };

        match item {
            Ok(Item::Readable) => {
                // A client that dies with a PDU part-way through leaves
                // the read waiting for the rest with no clock running;
                // the clock here runs once the first byte is in, and only
                // while nothing more arrives.
                let moved = AtomicU64::new(0);
                let decoded = smol::future::or(
                    async {
                        let mut counted = Counted {
                            stream: &mut stream,
                            bytes: &moved,
                        };
                        Some(Pdu::decode_async(&mut counted, None).await)
                    },
                    async {
                        stalled(&moved, READ_STALL_LIMIT, true).await;
                        None
                    },
                )
                .await;
                let decoded = match decoded {
                    Some(Ok(data)) => data,
                    Some(Err(err)) => {
                        if let Some(err) = err.root_cause().downcast_ref::<std::io::Error>() {
                            if err.kind() == std::io::ErrorKind::UnexpectedEof {
                                // Client disconnected: no need to make a noise
                                return Ok(());
                            }
                        }
                        return Err(err).context("reading Pdu from client");
                    }
                    None => {
                        log::warn!(
                            "the client stopped part-way through a PDU and sent nothing more \
                             for {READ_STALL_LIMIT:?}; dropping the connection"
                        );
                        return Ok(());
                    }
                };
                liveness.heard(Instant::now());
                handler.process_one(decoded);
            }
            Ok(Item::LivenessTick) => unreachable!("handled above"),
            Ok(Item::WritePdu(decoded)) => {
                log::trace!("write {} serial {}", decoded.pdu.pdu_name(), decoded.serial);
                let is_probe = decoded.serial == 0 && matches!(decoded.pdu, Pdu::Ping(_));
                // A write that makes no progress is the one sure sign of a
                // peer that died with data in flight: the liveness clock
                // cannot run while this arm waits, so the wait itself is
                // bounded. Progress, not completion: a large picture over
                // a slow link takes as long as it takes.
                let moved = AtomicU64::new(0);
                let written = smol::future::or(
                    async {
                        let mut counted = Counted {
                            stream: &mut stream,
                            bytes: &moved,
                        };
                        decoded
                            .pdu
                            .encode_async(&mut counted, decoded.serial)
                            .await
                            .map_err(WriteFailure::Encode)?;
                        counted.flush().await.map_err(WriteFailure::Flush)
                    },
                    async {
                        stalled(&moved, WRITE_STALL_LIMIT, false).await;
                        Err(WriteFailure::Stalled)
                    },
                )
                .await;
                match written {
                    Ok(()) => {
                        if is_probe {
                            liveness.probe_written(Instant::now());
                        }
                    }
                    Err(WriteFailure::Stalled) => {
                        log::warn!(
                            "a write to the client made no progress for \
                             {WRITE_STALL_LIMIT:?}; dropping the connection"
                        );
                        return Ok(());
                    }
                    Err(WriteFailure::Encode(err)) => {
                        if let Some(err) = err.root_cause().downcast_ref::<std::io::Error>() {
                            if err.kind() == std::io::ErrorKind::BrokenPipe {
                                // Client disconnected: no need to make a noise
                                return Ok(());
                            }
                        }
                        return Err(err).context("encoding PDU to client");
                    }
                    Err(WriteFailure::Flush(err)) => {
                        if err.kind() == std::io::ErrorKind::BrokenPipe {
                            // Client disconnected: no need to make a noise
                            return Ok(());
                        }
                        return Err(err).context("flushing PDU to client");
                    }
                }
            }
            Ok(Item::Notif(MuxNotification::PaneOutput(pane_id))) => {
                log::trace!("notification: pane output {pane_id}");
                handler.schedule_pane_push(pane_id);
            }
            Ok(Item::Notif(MuxNotification::Alert { pane_id, alert })) => {
                {
                    let per_pane = handler.per_pane(pane_id);
                    let mut per_pane = per_pane.lock().unwrap();
                    per_pane.notifications.push(alert);
                }
                handler.schedule_pane_push(pane_id);
            }
            Ok(Item::Notif(MuxNotification::PaneRemoved(pane_id))) => {
                handler.forget_pane(pane_id);
                send_notif_pdu(Pdu::PaneRemoved(codec::PaneRemoved { pane_id }));
            }
            Ok(Item::Notif(other)) => {
                log::trace!("notification with nothing for the connection to do: {other:?}");
            }
            Err(err) => {
                log::error!("process_async Err {}", err);
                return Ok(());
            }
        }
    }
}

/// How long a write to the client may go without a single byte leaving
/// for the kernel before the peer is taken for dead, and how long a PDU
/// the client has started sending may go without another byte arriving.
/// Measured as progress, not completion: a large picture over a slow link
/// takes as long as it takes, and the socket's buffers say when the peer
/// has stopped taking anything at all.
const WRITE_STALL_LIMIT: Duration = Duration::from_secs(60);
const READ_STALL_LIMIT: Duration = Duration::from_secs(60);

/// How often the stall clocks look at their counters.
const STALL_CHECK: Duration = Duration::from_secs(1);

/// How long the socket is given to show an answer before a peer the
/// liveness clock calls dead is dropped.
const DEAD_GRACE: Duration = Duration::from_millis(250);

/// Whether the socket has something to read, given a moment to say so.
async fn socket_has_data<T>(stream: &Async<T>) -> bool {
    smol::future::or(async { stream.readable().await.is_ok() }, async {
        smol::Timer::after(DEAD_GRACE).await;
        false
    })
    .await
}

/// Resolves once `bytes` has stood still for `limit`. With `from_first_byte`
/// the clock does not run until something has moved at all: a read is
/// entered on readability, and nothing arriving after that is an idle
/// client, not a stalled one.
async fn stalled(bytes: &AtomicU64, limit: Duration, from_first_byte: bool) {
    let mut seen = bytes.load(Ordering::Relaxed);
    let mut still_since = if from_first_byte {
        None
    } else {
        Some(Instant::now())
    };
    loop {
        smol::Timer::after(STALL_CHECK).await;
        let now = bytes.load(Ordering::Relaxed);
        if now != seen {
            seen = now;
            still_since = Some(Instant::now());
            continue;
        }
        if still_since.is_some_and(|since| since.elapsed() >= limit) {
            return;
        }
    }
}

/// The stream with a count of the bytes that cross it, so a transfer that
/// is merely long can be told from one that has stopped.
#[derive(Debug)]
struct Counted<'a, T> {
    stream: &'a mut Async<T>,
    bytes: &'a AtomicU64,
}

impl<T> AsyncRead for Counted<'_, T>
where
    Async<T>: AsyncRead + Unpin,
{
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &mut [u8],
    ) -> Poll<std::io::Result<usize>> {
        let this = &mut *self;
        let polled = Pin::new(&mut *this.stream).poll_read(cx, buf);
        if let Poll::Ready(Ok(n)) = &polled {
            this.bytes.fetch_add(*n as u64, Ordering::Relaxed);
        }
        polled
    }
}

impl<T> AsyncWrite for Counted<'_, T>
where
    Async<T>: AsyncWrite + Unpin,
{
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let this = &mut *self;
        let polled = Pin::new(&mut *this.stream).poll_write(cx, buf);
        if let Poll::Ready(Ok(n)) = &polled {
            this.bytes.fetch_add(*n as u64, Ordering::Relaxed);
        }
        polled
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut *self.stream).poll_flush(cx)
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut *self.stream).poll_close(cx)
    }
}

enum WriteFailure {
    Encode(anyhow::Error),
    Flush(std::io::Error),
    Stalled,
}

/// What the connection loop needs for a mux notification, prepared on the
/// main thread. Pane output, alerts and pane removal need the handler's
/// own state and go through as they are; everything else becomes the PDU
/// it will be sent as, with whatever the mux has to be asked read here.
fn item_for_notification(n: MuxNotification) -> Option<Item> {
    let write = |pdu: Pdu| Some(Item::WritePdu(DecodedPdu { serial: 0, pdu }));
    match n {
        MuxNotification::PaneOutput(_)
        | MuxNotification::Alert { .. }
        | MuxNotification::PaneRemoved(_) => Some(Item::Notif(n)),
        MuxNotification::AgentStatusChanged(pane_id) => {
            // Read the status at send time so the payload is always the
            // freshest classification, never a queued stale value.
            let status = Mux::get().get_pane(pane_id).and_then(|p| p.agent_status());
            write(Pdu::AgentStatusChanged(codec::AgentStatusChanged {
                pane_id,
                status,
            }))
        }
        MuxNotification::AssignClipboard {
            pane_id,
            selection,
            clipboard,
        } => write(Pdu::SetClipboard(codec::SetClipboard {
            pane_id,
            clipboard,
            selection,
        })),
        MuxNotification::TabAddedToWindow { tab_id, window_id } => {
            write(Pdu::TabAddedToWindow(codec::TabAddedToWindow {
                tab_id,
                window_id,
            }))
        }
        MuxNotification::WindowWorkspaceChanged(window_id) => {
            let workspace = Mux::get()
                .get_window(window_id)
                .map(|w| w.get_workspace().to_string())?;
            write(Pdu::WindowWorkspaceChanged(codec::WindowWorkspaceChanged {
                window_id,
                workspace,
            }))
        }
        MuxNotification::PaneFocused(pane_id) => {
            write(Pdu::PaneFocused(codec::PaneFocused { pane_id }))
        }
        MuxNotification::TabResized(tab_id) => write(Pdu::TabResized(codec::TabResized { tab_id })),
        MuxNotification::TabTitleChanged { tab_id, title } => {
            write(Pdu::TabTitleChanged(codec::TabTitleChanged {
                tab_id,
                title,
            }))
        }
        MuxNotification::WindowTitleChanged { window_id, title } => {
            write(Pdu::WindowTitleChanged(codec::WindowTitleChanged {
                window_id,
                title,
            }))
        }
        MuxNotification::WorkspaceRenamed {
            old_workspace,
            new_workspace,
        } => write(Pdu::RenameWorkspace(codec::RenameWorkspace {
            old_workspace,
            new_workspace,
        })),
        // The tree is small enough to resend whole; this is also the path
        // that tells the client which mutated it that the server accepted
        // the op.
        MuxNotification::ThinkTermTreeChanged => {
            write(Pdu::ThinkTermTreeState(codec::ThinkTermTreeState {
                tree: crate::thinkterm_tree::snapshot(),
            }))
        }
        MuxNotification::ThinkTermSessionChanged => match crate::thinkterm_session::snapshot() {
            Ok(state) => write(Pdu::ThinkTermSessionState(state)),
            Err(err) => {
                log::error!("ThinkTermSessionState snapshot failed: {err:#}");
                None
            }
        },
        MuxNotification::FrontendLeaseChanged(state) => write(Pdu::ClientViewportState(
            crate::sessionhandler::codec_viewport_state(state),
        )),
        MuxNotification::FrontendAccessChanged(state) => write(Pdu::FrontendAccessState(
            crate::sessionhandler::codec_access_state(state),
        )),
        MuxNotification::PaneAdded(_)
        | MuxNotification::SaveToDownloads { .. }
        | MuxNotification::WindowRemoved(_)
        | MuxNotification::WindowCreated(_)
        | MuxNotification::WindowInvalidated(_)
        | MuxNotification::ActiveWorkspaceChanged(_)
        | MuxNotification::Empty => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    const S: Duration = Duration::from_secs(1);

    #[test]
    fn a_quiet_client_is_probed_and_then_dropped() {
        let t0 = Instant::now();
        let mut liveness = Liveness::with_timing(t0, 10 * S, 20 * S);
        assert_eq!(liveness.next_check(), t0 + 10 * S);
        assert_eq!(liveness.check(t0 + 9 * S), Verdict::Fine, "not yet");
        assert_eq!(liveness.check(t0 + 10 * S), Verdict::Probe);
        assert_eq!(
            liveness.next_check(),
            t0 + 30 * S,
            "the probe gets its full timeout"
        );
        assert_eq!(liveness.check(t0 + 29 * S), Verdict::Fine, "still waiting");
        assert_eq!(liveness.check(t0 + 30 * S), Verdict::Dead(30 * S));
    }

    #[test]
    fn anything_heard_cancels_the_probe() {
        let t0 = Instant::now();
        let mut liveness = Liveness::with_timing(t0, 10 * S, 20 * S);
        assert_eq!(liveness.check(t0 + 10 * S), Verdict::Probe);
        liveness.heard(t0 + 12 * S);
        assert_eq!(
            liveness.next_check(),
            t0 + 22 * S,
            "the clock restarts from the answer"
        );
        assert_eq!(
            liveness.check(t0 + 30 * S),
            Verdict::Probe,
            "and it is a fresh probe, not a death"
        );
    }

    #[test]
    fn the_answer_window_opens_when_the_probe_actually_left() {
        let t0 = Instant::now();
        let mut liveness = Liveness::with_timing(t0, 10 * S, 20 * S);
        assert_eq!(liveness.check(t0 + 10 * S), Verdict::Probe);
        // Stuck behind pane output for a while before it was written.
        liveness.probe_written(t0 + 25 * S);
        assert_eq!(liveness.next_check(), t0 + 45 * S);
        assert_eq!(
            liveness.check(t0 + 30 * S),
            Verdict::Fine,
            "not yet: it only just left"
        );
        assert_eq!(liveness.check(t0 + 45 * S), Verdict::Dead(45 * S));
    }

    #[test]
    fn a_probe_is_not_repeated_while_it_is_outstanding() {
        let t0 = Instant::now();
        let mut liveness = Liveness::with_timing(t0, 10 * S, 20 * S);
        assert_eq!(liveness.check(t0 + 10 * S), Verdict::Probe);
        assert_eq!(
            liveness.check(t0 + 21 * S),
            Verdict::Fine,
            "one Ping per silence"
        );
    }

    /// A peer that answers the probe is kept, and hears nothing back for
    /// its Pong. (The first cut replied to the Pong with an ErrorResponse,
    /// which the client could not handle: every client died ten seconds
    /// after connecting.)
    #[cfg(unix)]
    #[test]
    fn a_peer_that_answers_the_probe_is_kept_and_not_answered_back() {
        use std::os::fd::{FromRawFd, IntoRawFd};

        let mux = Arc::new(Mux::new(None));
        Mux::set_mux(&mux);

        let (ours, mut theirs) = std::os::unix::net::UnixStream::pair().unwrap();
        let ours = unsafe { UnixStream::from_raw_fd(ours.into_raw_fd()) };
        let stream = Async::new(ours).unwrap();
        let liveness = Liveness::with_timing(
            Instant::now(),
            Duration::from_millis(100),
            Duration::from_millis(200),
        );

        let peer = std::thread::spawn(move || {
            // Answer every probe for a while. The only things the server
            // may send are further probes: a reply to the Pong would be an
            // ErrorResponse, which a client takes as fatal, and a hangup
            // means it was counted dead regardless.
            theirs
                .set_read_timeout(Some(Duration::from_millis(300)))
                .unwrap();
            let started = Instant::now();
            let mut probes = 0;
            while started.elapsed() < Duration::from_millis(700) {
                match Pdu::decode(&mut theirs) {
                    Ok(decoded) => {
                        assert!(
                            matches!(decoded.pdu, Pdu::Ping(_)),
                            "the server sent {:?} to a peer that answered",
                            decoded.pdu
                        );
                        assert_eq!(decoded.serial, 0);
                        probes += 1;
                        Pdu::Pong(codec::Pong {})
                            .encode(&mut theirs, 0)
                            .expect("the answer is written");
                    }
                    Err(err) => {
                        let io = err.root_cause().downcast_ref::<std::io::Error>();
                        assert!(
                            io.map_or(false, |e| matches!(
                                e.kind(),
                                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                            )),
                            "the server hung up on a peer that answered: {:#}",
                            err
                        );
                    }
                }
            }
            assert!(probes >= 2, "expected repeated probes, got {}", probes);
            theirs
        });

        // The loop must still be running once the probe has been answered
        // and the old timeout has long passed.
        let outcome = smol::block_on(smol::future::or(
            async {
                process_async_with(stream, liveness).await?;
                anyhow::bail!("the loop ended although the peer answered")
            },
            async {
                smol::Timer::after(Duration::from_millis(900)).await;
                Ok(())
            },
        ));
        outcome.expect("the connection stays up");
        peer.join().expect("the peer's expectations hold");
    }

    /// An answer that is already in the socket when the clock runs out
    /// is read, not lost: the peer here answers before it is even asked,
    /// with a clock that calls it dead the moment the probe is out. The
    /// old loop dropped it on the first tick after the probe; now the
    /// answer is read, the clock starts over, and only the second probe
    /// goes unanswered. Two probes on the peer's side say so.
    #[cfg(unix)]
    #[test]
    fn an_answer_already_in_the_socket_is_read_before_the_peer_is_dropped() {
        use std::os::fd::{FromRawFd, IntoRawFd};

        let mux = Arc::new(Mux::new(None));
        Mux::set_mux(&mux);

        let (ours, mut theirs) = std::os::unix::net::UnixStream::pair().unwrap();
        let ours = unsafe { UnixStream::from_raw_fd(ours.into_raw_fd()) };
        let stream = Async::new(ours).unwrap();
        Pdu::Pong(codec::Pong {})
            .encode(&mut theirs, 0)
            .expect("the early answer is written");
        let liveness = Liveness::with_timing(Instant::now(), Duration::ZERO, Duration::ZERO);

        let outcome = smol::block_on(smol::future::or(
            process_async_with(stream, liveness),
            async {
                smol::Timer::after(Duration::from_secs(5)).await;
                anyhow::bail!("the peer was never dropped")
            },
        ));
        outcome.expect("the loop ends on its own once the peer really is silent");

        theirs.set_nonblocking(true).unwrap();
        let mut probes = 0;
        while let Ok(decoded) = Pdu::decode(&mut theirs) {
            assert!(matches!(decoded.pdu, Pdu::Ping(_)), "got {:?}", decoded.pdu);
            probes += 1;
        }
        assert_eq!(
            probes, 2,
            "the first probe found its answer in the socket; the second went unanswered"
        );
    }

    /// The whole loop against a socket whose peer never writes: it must
    /// send a Ping and then give up, returning Ok so the handler drops and
    /// the client is unregistered.
    #[cfg(unix)]
    #[test]
    fn a_silent_peer_ends_the_connection() {
        use std::os::fd::{FromRawFd, IntoRawFd};

        // `process_async` subscribes to the process-wide Mux; give it one.
        // Other tests in this crate build their own instances and never
        // consult the global, so this one is theirs to set.
        let mux = Arc::new(Mux::new(None));
        Mux::set_mux(&mux);

        let (ours, theirs) = std::os::unix::net::UnixStream::pair().unwrap();
        let ours = unsafe { UnixStream::from_raw_fd(ours.into_raw_fd()) };
        let stream = Async::new(ours).unwrap();
        let liveness = Liveness::with_timing(
            Instant::now(),
            Duration::from_millis(100),
            Duration::from_millis(200),
        );

        let started = Instant::now();
        let outcome = smol::block_on(smol::future::or(
            process_async_with(stream, liveness),
            async {
                smol::Timer::after(Duration::from_secs(5)).await;
                anyhow::bail!("the silent peer was never dropped")
            },
        ));
        outcome.expect("the loop ends on its own");
        assert!(
            started.elapsed() >= Duration::from_millis(300),
            "it waited for the probe to go unanswered: {:?}",
            started.elapsed()
        );

        // And the peer was actually asked: a Ping is on its side of the pair.
        use std::io::Read;
        let mut theirs = theirs;
        theirs.set_nonblocking(true).unwrap();
        let mut buf = [0u8; 64];
        let n = theirs.read(&mut buf).expect("the probe was written");
        assert!(n > 0);
    }
}
