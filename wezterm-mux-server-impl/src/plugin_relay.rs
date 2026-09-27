//! The plugin channel through the mux: a client's frames to the plugin host
//! on this machine, and the host's frames back, carried unread.
//!
//! The mux runs terminals and nothing else; the plugins run in a process of
//! their own (see thinkterm-plugin-channel). A browser can only reach the mux, so
//! this is its way there. Each client that sends a frame gets a connection
//! of its own to the host, opened at its first frame and closed when the
//! client asks or goes away; to the host it is one more client.

use crate::sessionhandler::PduSender;
use anyhow::anyhow;
use codec::{DecodedPdu, Pdu, PluginFrame, UnitResponse};
use smol::channel::{bounded, Receiver, Sender, TrySendError};
use smol::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use smol::Async;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use thinkterm_plugin_channel::client::{Connection, Host};
use thinkterm_plugin_channel::wire::{frame_header, frame_len};

/// Frames one client may have on their way to the host at once; a client
/// with more is not waiting for its answers.
const QUEUE: usize = 64;

pub(crate) type Respond = Box<dyn FnOnce(anyhow::Result<Pdu>) + Send>;

/// One client's connection to the host. Dropping it ends the connection
/// once what the client already sent has left: a change sent just before
/// the client closed the panel, or went away, is still made.
pub(crate) struct Pipe {
    frames: Sender<(Vec<u8>, Respond)>,
    carrier: Option<smol::Task<()>>,
    /// Set once the client has closed it, or gone: it is not told the
    /// connection ended. Told, a client that had opened another meanwhile
    /// would take the word for that one's.
    let_go: Arc<AtomicBool>,
}

impl Drop for Pipe {
    fn drop(&mut self) {
        self.let_go.store(true, Ordering::Release);
        // The carrier drains the queue, finds it closed, and ends.
        self.frames.close();
        if let Some(carrier) = self.carrier.take() {
            carrier.detach();
        }
    }
}

impl Pipe {
    /// A connection to the host this build ships, started if need be.
    pub(crate) fn open(to_client: PduSender) -> Self {
        Self::open_with(to_client, || Host::for_this_build()?.connect())
    }

    fn open_with(
        to_client: PduSender,
        connect: impl FnOnce() -> anyhow::Result<Connection> + Send + 'static,
    ) -> Self {
        let (frames, queued) = bounded(QUEUE);
        let let_go = Arc::new(AtomicBool::new(false));
        Self {
            frames,
            carrier: Some(crate::connections::spawn_task(carry(
                queued,
                to_client,
                connect,
                Arc::clone(&let_go),
            ))),
            let_go,
        }
    }

    /// Connected, or still connecting.
    pub(crate) fn is_open(&self) -> bool {
        !self.frames.is_closed()
    }

    /// Sends `data` on to the host; `respond` is told once it has left.
    pub(crate) fn send(&self, data: Vec<u8>, respond: Respond) {
        match self.frames.try_send((data, respond)) {
            Ok(()) => {}
            Err(TrySendError::Full((_, respond))) => respond(Err(anyhow!(
                "too many frames are already on their way to the plugin host"
            ))),
            Err(TrySendError::Closed((_, respond))) => {
                respond(Err(anyhow!("the connection to the plugin host has closed")))
            }
        }
    }
}

async fn carry(
    queued: Receiver<(Vec<u8>, Respond)>,
    to_client: PduSender,
    connect: impl FnOnce() -> anyhow::Result<Connection> + Send + 'static,
    let_go: Arc<AtomicBool>,
) {
    // Off the connection threads: reaching the host may mean starting it.
    let stream = match smol::unblock(move || -> anyhow::Result<_> {
        Ok(Async::new(connect()?.into_stream())?)
    })
    .await
    {
        Ok(stream) => stream,
        Err(err) => {
            let reason = format!("{err:#}");
            log::warn!("cannot reach the plugin host: {reason}");
            refuse_waiting(&queued, &format!("no plugin host here: {reason}"));
            return;
        }
    };
    let (mut from_host, mut to_host) = smol::io::split(stream);
    let outbound = async {
        while let Ok((data, respond)) = queued.recv().await {
            match write_frame(&mut to_host, &data).await {
                Ok(()) => respond(Ok(Pdu::UnitResponse(UnitResponse {}))),
                Err(err) => {
                    respond(Err(anyhow!("sending to the plugin host: {err}")));
                    return;
                }
            }
        }
    };
    let inbound = async {
        while let Ok(data) = read_frame(&mut from_host).await {
            let frame = DecodedPdu {
                serial: 0,
                pdu: Pdu::PluginFrame(PluginFrame { data }),
            };
            if to_client.send(frame).is_err() {
                return;
            }
        }
    };
    smol::future::or(outbound, inbound).await;
    refuse_waiting(&queued, "the connection to the plugin host has closed");
    if let_go.load(Ordering::Acquire) {
        return;
    }
    // An empty frame: the host went, and the client may open another
    // connection when it wants.
    let _ = to_client.send(DecodedPdu {
        serial: 0,
        pdu: Pdu::PluginFrame(PluginFrame { data: vec![] }),
    });
}

/// Closes the queue, answering whatever was still in it.
fn refuse_waiting(queued: &Receiver<(Vec<u8>, Respond)>, reason: &str) {
    queued.close();
    while let Ok((_, respond)) = queued.try_recv() {
        respond(Err(anyhow!("{reason}")));
    }
}

fn too_long() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "plugin frame too long")
}

async fn write_frame(to: &mut (impl AsyncWrite + Unpin), payload: &[u8]) -> io::Result<()> {
    let header = frame_header(payload.len()).ok_or_else(too_long)?;
    let mut frame = Vec::with_capacity(header.len() + payload.len());
    frame.extend_from_slice(&header);
    frame.extend_from_slice(payload);
    to.write_all(&frame).await?;
    to.flush().await
}

async fn read_frame(from: &mut (impl AsyncRead + Unpin)) -> io::Result<Vec<u8>> {
    let mut header = [0u8; 4];
    from.read_exact(&mut header).await?;
    let len = frame_len(header).ok_or_else(too_long)?;
    let mut payload = vec![0u8; len];
    from.read_exact(&mut payload).await?;
    Ok(payload)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::sync::mpsc;
    use std::time::Duration;
    use thinkterm_plugin_channel::wire::{self, FromHost, PROTOCOL};

    const WAIT: Duration = Duration::from_secs(10);

    fn host_at(dir: &Path) -> Host {
        Host {
            socket: dir.join("sock"),
            lock: dir.join("lock"),
            data_dir: dir.join("data"),
            log: dir.join("log"),
            program: dir.join("no-such-program"),
        }
    }

    fn client() -> (PduSender, mpsc::Receiver<Pdu>) {
        let (tx, rx) = mpsc::channel();
        let sender = PduSender::new(move |decoded: DecodedPdu| {
            assert_eq!(decoded.serial, 0, "pushes are unasked");
            tx.send(decoded.pdu).map_err(|err| anyhow!("{err}"))
        });
        (sender, rx)
    }

    fn send(pipe: &Pipe, data: &[u8]) -> anyhow::Result<Pdu> {
        let (tx, rx) = mpsc::channel();
        pipe.send(
            data.to_vec(),
            Box::new(move |result| {
                let _ = tx.send(result);
            }),
        );
        rx.recv_timeout(WAIT).expect("an answer")
    }

    fn pushed(rx: &mpsc::Receiver<Pdu>) -> Vec<u8> {
        match rx.recv_timeout(WAIT).expect("a push") {
            Pdu::PluginFrame(PluginFrame { data }) => data,
            other => panic!("expected a plugin frame, got {other:?}"),
        }
    }

    #[test]
    fn frames_go_both_ways_as_they_are() {
        let dir = tempfile::tempdir().unwrap();
        let host = host_at(dir.path());
        // A host that answers each frame with its bytes reversed, whatever
        // they are, and hangs up when told to.
        let listener = wezterm_uds::UnixListener::bind(&host.socket).unwrap();
        let fake = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let hello = FromHost::Hello { protocol: PROTOCOL }.encode();
            wire::write_frame(&mut stream, &hello).unwrap();
            loop {
                let frame = wire::read_frame(&mut stream).unwrap();
                if frame == b"bye" {
                    return;
                }
                let reversed: Vec<u8> = frame.into_iter().rev().collect();
                wire::write_frame(&mut stream, &reversed).unwrap();
            }
        });

        let (sender, pushes) = client();
        let pipe = Pipe::open_with(sender, move || host.connect());
        assert!(matches!(
            send(&pipe, b"not json at all"),
            Ok(Pdu::UnitResponse(_))
        ));
        assert_eq!(pushed(&pushes), b"lla ta nosj ton");
        assert!(matches!(send(&pipe, b"bye"), Ok(Pdu::UnitResponse(_))));
        fake.join().unwrap();
        // The host hung up: the client is told with an empty frame, and the
        // pipe is closed for good.
        assert_eq!(pushed(&pushes), b"");
        assert!(!pipe.is_open());
        assert!(send(&pipe, b"more").is_err());
    }

    #[test]
    fn what_was_sent_before_the_pipe_closed_still_arrives() {
        let dir = tempfile::tempdir().unwrap();
        let host = host_at(dir.path());
        let listener = wezterm_uds::UnixListener::bind(&host.socket).unwrap();
        let fake = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let hello = FromHost::Hello { protocol: PROTOCOL }.encode();
            wire::write_frame(&mut stream, &hello).unwrap();
            let mut got = Vec::new();
            while let Ok(frame) = wire::read_frame(&mut stream) {
                got.push(frame);
            }
            got
        });
        let (sender, _pushes) = client();
        let pipe = Pipe::open_with(sender, move || host.connect());
        for frame in [&b"one"[..], b"two", b"three"] {
            pipe.send(frame.to_vec(), Box::new(|_| {}));
        }
        // Closed while the connection may still be coming up.
        drop(pipe);
        assert_eq!(
            fake.join().unwrap(),
            [b"one".to_vec(), b"two".to_vec(), b"three".to_vec()]
        );
    }

    #[test]
    fn a_pipe_the_client_closed_is_not_announced_closed() {
        let dir = tempfile::tempdir().unwrap();
        let host = host_at(dir.path());
        let listener = wezterm_uds::UnixListener::bind(&host.socket).unwrap();
        let fake = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let hello = FromHost::Hello { protocol: PROTOCOL }.encode();
            wire::write_frame(&mut stream, &hello).unwrap();
            while wire::read_frame(&mut stream).is_ok() {}
        });
        let (sender, pushes) = client();
        let pipe = Pipe::open_with(sender, move || host.connect());
        assert!(matches!(send(&pipe, b"one"), Ok(Pdu::UnitResponse(_))));
        drop(pipe);
        fake.join().unwrap();
        // The client asked: an empty frame now would read as the end of a
        // connection it opened since.
        assert!(pushes.recv_timeout(Duration::from_millis(300)).is_err());
    }

    #[test]
    fn a_frame_with_no_host_to_reach_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let host = host_at(dir.path());
        let (sender, _pushes) = client();
        let pipe = Pipe::open_with(sender, move || host.connect());
        let err = send(&pipe, b"{}").unwrap_err();
        assert!(
            format!("{err:#}").contains("no plugin host here"),
            "{err:#}"
        );
        assert!(!pipe.is_open());
    }
}
