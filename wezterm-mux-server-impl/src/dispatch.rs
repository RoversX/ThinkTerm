use crate::sessionhandler::{PduSender, SessionHandler};
use anyhow::Context;
use async_ossl::AsyncSslStream;
use codec::{DecodedPdu, Pdu};
use futures::FutureExt;
use mux::{Mux, MuxNotification};
use smol::prelude::*;
use smol::Async;
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
    probe_sent: Option<Instant>,
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
        }
    }

    /// The client sent something.
    pub fn heard(&mut self, now: Instant) {
        self.last_inbound = now;
        self.probe_sent = None;
    }

    /// When `check` next has something to say.
    pub fn next_check(&self) -> Instant {
        match self.probe_sent {
            Some(sent) => sent + self.timeout,
            None => self.last_inbound + self.interval,
        }
    }

    pub fn check(&mut self, now: Instant) -> Verdict {
        if let Some(sent) = self.probe_sent {
            if now >= sent + self.timeout {
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

    let pdu_sender = PduSender::new({
        let item_tx = item_tx.clone();
        move |pdu| {
            item_tx
                .try_send(Item::WritePdu(pdu))
                .map_err(|e| anyhow::anyhow!("{:?}", e))
        }
    });
    let mut handler = SessionHandler::new(pdu_sender);

    {
        // Notifications take one hop through the main thread's queue before
        // they reach this connection. A mutation runs on the main thread,
        // notifies in the middle of its work and sends its response at the
        // end; with this loop on its own thread, a notification handed
        // over directly would be on the wire before the response to the
        // request that caused it, where the single-threaded server always
        // sent the response first. Clients rely on that: a state push that
        // overtakes its response leaves the response looking stale, and a
        // frontend waits for state that has, as far as it is concerned,
        // never arrived. The hop lands the notification after the response
        // has been queued, restoring the order.
        let mux = Mux::get();
        let tx = item_tx.clone();
        mux.subscribe(move |n| {
            if tx.is_closed() {
                return false;
            }
            log::trace!("notification queued for the connection: {n:?}");
            let tx = tx.clone();
            promise::spawn::spawn_into_main_thread(async move {
                if let Err(err) = tx.try_send(Item::Notif(n)) {
                    log::trace!("notification not delivered: {err}");
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

        match smol::future::or(smol::future::or(rx_msg, wait_for_read), tick).await {
            Ok(Item::Readable) => {
                let decoded = match Pdu::decode_async(&mut stream, None).await {
                    Ok(data) => data,
                    Err(err) => {
                        if let Some(err) = err.root_cause().downcast_ref::<std::io::Error>() {
                            if err.kind() == std::io::ErrorKind::UnexpectedEof {
                                // Client disconnected: no need to make a noise
                                return Ok(());
                            }
                        }
                        return Err(err).context("reading Pdu from client");
                    }
                };
                liveness.heard(Instant::now());
                handler.process_one(decoded);
            }
            Ok(Item::LivenessTick) => match liveness.check(Instant::now()) {
                Verdict::Fine => {}
                // Through the write queue like every other push, so a
                // peer that is gone fails the write and ends this the
                // quick way.
                Verdict::Probe => send_notif_pdu(Pdu::Ping(codec::Ping {})),
                Verdict::Dead(silence) => {
                    log::warn!(
                        "client silent for {silence:?} and did not answer a probe; \
                         dropping the connection"
                    );
                    return Ok(());
                }
            },
            Ok(Item::WritePdu(decoded)) => {
                log::trace!("write {} serial {}", decoded.pdu.pdu_name(), decoded.serial);
                match decoded.pdu.encode_async(&mut stream, decoded.serial).await {
                    Ok(()) => {}
                    Err(err) => {
                        if let Some(err) = err.root_cause().downcast_ref::<std::io::Error>() {
                            if err.kind() == std::io::ErrorKind::BrokenPipe {
                                // Client disconnected: no need to make a noise
                                return Ok(());
                            }
                        }
                        return Err(err).context("encoding PDU to client");
                    }
                };
                match stream.flush().await {
                    Ok(()) => {}
                    Err(err) => {
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
            Ok(Item::Notif(MuxNotification::PaneAdded(_pane_id))) => {}
            Ok(Item::Notif(MuxNotification::AgentStatusChanged(pane_id))) => {
                // Read the status at send time so the payload is always the
                // freshest classification, never a queued stale value.
                let status = Mux::get().get_pane(pane_id).and_then(|p| p.agent_status());
                send_notif_pdu(Pdu::AgentStatusChanged(codec::AgentStatusChanged {
                    pane_id,
                    status,
                }));
            }
            Ok(Item::Notif(MuxNotification::PaneRemoved(pane_id))) => {
                send_notif_pdu(Pdu::PaneRemoved(codec::PaneRemoved { pane_id }));
            }
            Ok(Item::Notif(MuxNotification::Alert { pane_id, alert })) => {
                {
                    let per_pane = handler.per_pane(pane_id);
                    let mut per_pane = per_pane.lock().unwrap();
                    per_pane.notifications.push(alert);
                }
                handler.schedule_pane_push(pane_id);
            }
            Ok(Item::Notif(MuxNotification::SaveToDownloads { .. })) => {}
            Ok(Item::Notif(MuxNotification::AssignClipboard {
                pane_id,
                selection,
                clipboard,
            })) => {
                send_notif_pdu(Pdu::SetClipboard(codec::SetClipboard {
                    pane_id,
                    clipboard,
                    selection,
                }));
            }
            Ok(Item::Notif(MuxNotification::TabAddedToWindow { tab_id, window_id })) => {
                send_notif_pdu(Pdu::TabAddedToWindow(codec::TabAddedToWindow {
                    tab_id,
                    window_id,
                }));
            }
            Ok(Item::Notif(MuxNotification::WindowRemoved(_window_id))) => {}
            Ok(Item::Notif(MuxNotification::WindowCreated(_window_id))) => {}
            Ok(Item::Notif(MuxNotification::WindowInvalidated(_window_id))) => {}
            Ok(Item::Notif(MuxNotification::WindowWorkspaceChanged(window_id))) => {
                let workspace = {
                    let mux = Mux::get();
                    mux.get_window(window_id)
                        .map(|w| w.get_workspace().to_string())
                };
                if let Some(workspace) = workspace {
                    send_notif_pdu(Pdu::WindowWorkspaceChanged(codec::WindowWorkspaceChanged {
                        window_id,
                        workspace,
                    }));
                }
            }
            Ok(Item::Notif(MuxNotification::PaneFocused(pane_id))) => {
                send_notif_pdu(Pdu::PaneFocused(codec::PaneFocused { pane_id }));
            }
            Ok(Item::Notif(MuxNotification::TabResized(tab_id))) => {
                send_notif_pdu(Pdu::TabResized(codec::TabResized { tab_id }));
            }
            Ok(Item::Notif(MuxNotification::TabTitleChanged { tab_id, title })) => {
                send_notif_pdu(Pdu::TabTitleChanged(codec::TabTitleChanged {
                    tab_id,
                    title,
                }));
            }
            Ok(Item::Notif(MuxNotification::WindowTitleChanged { window_id, title })) => {
                send_notif_pdu(Pdu::WindowTitleChanged(codec::WindowTitleChanged {
                    window_id,
                    title,
                }));
            }
            Ok(Item::Notif(MuxNotification::WorkspaceRenamed {
                old_workspace,
                new_workspace,
            })) => {
                send_notif_pdu(Pdu::RenameWorkspace(codec::RenameWorkspace {
                    old_workspace,
                    new_workspace,
                }));
            }
            Ok(Item::Notif(MuxNotification::ThinkTermTreeChanged)) => {
                // The tree is small enough to resend whole; this is also the
                // path that tells the client which mutated it that the server
                // accepted the op.
                send_notif_pdu(Pdu::ThinkTermTreeState(codec::ThinkTermTreeState {
                    tree: crate::thinkterm_tree::snapshot(),
                }));
            }
            Ok(Item::Notif(MuxNotification::ThinkTermSessionChanged)) => {
                // A snapshot failure must not kill the connection — that
                // would stop every pane's pushes for this client.
                match crate::thinkterm_session::snapshot() {
                    Ok(state) => send_notif_pdu(Pdu::ThinkTermSessionState(state)),
                    Err(err) => {
                        log::error!("ThinkTermSessionState snapshot failed: {err:#}")
                    }
                }
            }
            Ok(Item::Notif(MuxNotification::FrontendLeaseChanged(state))) => {
                send_notif_pdu(Pdu::ClientViewportState(
                    crate::sessionhandler::codec_viewport_state(state),
                ));
            }
            Ok(Item::Notif(MuxNotification::FrontendAccessChanged(state))) => {
                send_notif_pdu(Pdu::FrontendAccessState(
                    crate::sessionhandler::codec_access_state(state),
                ));
            }
            Ok(Item::Notif(MuxNotification::ActiveWorkspaceChanged(_))) => {}
            Ok(Item::Notif(MuxNotification::Empty)) => {}
            Err(err) => {
                log::error!("process_async Err {}", err);
                return Ok(());
            }
        }
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
