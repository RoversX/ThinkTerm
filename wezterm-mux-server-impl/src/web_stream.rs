//! A browser's WebSocket presented to the connection loop as the ordered
//! byte stream the codec wants.
//!
//! The codec's framing is self-delimiting, so WebSocket message boundaries
//! carry no meaning: inbound messages are appended to one byte queue and
//! outbound bytes are cut into binary messages of a fixed size, however
//! the codec chose to write them. Two tasks on the connection executor
//! own the socket halves; the stream itself talks to them through bounded
//! channels, which is what gives the dispatch loop's stall clocks
//! something to measure -- a peer that stops reading fills the outbound
//! channel and `poll_write` stops making progress, exactly as a full
//! kernel buffer would.
//!
//! The socket's lifetime is the stream's: the reader task is cancelled
//! when the stream drops (soketto only closes the socket once both halves
//! are gone), and the writer says goodbye with a close frame but is given
//! a bounded time to do so, since the peer it is closing may be dead.

use crate::dispatch::ConnectionStream;
use futures::channel::oneshot;
use futures::io::{AsyncRead, AsyncWrite};
use smol::channel::{Receiver, Sender};
use soketto::connection::{Builder, Error as WsError, Mode};
use soketto::Data;
use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

/// Outbound bytes leave in messages of at most this size; the client
/// concatenates, so the number only bounds the writer's staging.
const OUTBOUND_CHUNK: usize = 64 * 1024;
/// Messages the reader may run ahead of the codec, and PDUs the codec may
/// run ahead of the socket, before either side is made to wait.
const CHANNEL_DEPTH: usize = 16;
/// Largest single WebSocket message accepted from a browser. What a browser
/// sends is keys, pastes and small requests; soketto allocates the declared
/// length up front, so this is the most one frame header can make the
/// server allocate. (PDU lengths are bounded separately, by the codec:
/// a PDU may span messages.)
const MAX_INBOUND_MESSAGE: usize = 8 * 1024 * 1024;
/// How long the writer waits for its close frame to leave once the
/// stream is gone. A dead peer never drains the socket; the fd must not
/// wait on it.
const CLOSE_GRACE: Duration = Duration::from_secs(5);

/// Sync as well as Send: the dispatch loop holds `&WebStream` across an
/// await, so the stream has to be shareable for its future to be Send.
type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send + Sync>>;

/// What the stream hands the writer task.
enum Outbound {
    Data(Vec<u8>),
    /// Flush the socket and say when it is done.
    Flush(oneshot::Sender<()>),
}

/// The byte stream side of an upgraded WebSocket.
pub struct WebStream {
    /// Bytes received but not yet read. Behind a mutex only so that
    /// `wait_for_readable`, which takes `&self`, can stash the message it
    /// pulled; the loop that owns the stream is a single task.
    inbound: Mutex<VecDeque<u8>>,
    /// Behind `Arc` because a channel end is not `Unpin` and the stream
    /// has to be.
    rx_in: Arc<Receiver<Vec<u8>>>,
    recv: Option<BoxFuture<Option<Vec<u8>>>>,
    /// `None` once closed.
    tx_out: Option<Arc<Sender<Outbound>>>,
    /// A send the writer channel has not yet accepted, with the number of
    /// bytes it carries; `poll_write` reports that count once it lands.
    send: Option<(BoxFuture<bool>, usize)>,
    /// A flush the writer has not yet acknowledged.
    flush: Option<BoxFuture<()>>,
    /// The reader task; dropped with the stream, which cancels it.
    _reader: smol::Task<()>,
}

impl std::fmt::Debug for WebStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebStream")
            .field("buffered", &self.inbound.lock().map(|q| q.len()).unwrap_or(0))
            .field("closed", &self.tx_out.is_none())
            .finish()
    }
}

impl WebStream {
    /// Take over `socket` right after a successful HTTP upgrade and run the
    /// WebSocket protocol over it.
    pub fn new<T>(socket: T) -> Self
    where
        T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let mut builder = Builder::new(socket, Mode::Server);
        builder.set_max_message_size(MAX_INBOUND_MESSAGE);
        builder.set_max_frame_size(MAX_INBOUND_MESSAGE);
        let (mut sender, mut receiver) = builder.finish();

        let (tx_in, rx_in) = smol::channel::bounded::<Vec<u8>>(CHANNEL_DEPTH);
        let (tx_out, rx_out) = smol::channel::bounded::<Outbound>(CHANNEL_DEPTH);

        let reader = crate::connections::spawn_task(async move {
            let mut message = Vec::new();
            loop {
                message.clear();
                match receiver.receive_data(&mut message).await {
                    Ok(Data::Binary(_)) => {
                        // An empty message carries nothing to deliver, and
                        // must not wake a reader that would then find the
                        // queue empty.
                        if message.is_empty() {
                            continue;
                        }
                        if tx_in.send(std::mem::take(&mut message)).await.is_err() {
                            // The stream is gone; nothing to deliver to.
                            return;
                        }
                    }
                    Ok(Data::Text(_)) => {
                        // The codec is bytes; a text frame has been through
                        // UTF-8 validation and cannot be what a client of
                        // ours sent. Treat it as end of stream.
                        log::warn!("web client sent a text frame; closing its connection");
                        return;
                    }
                    Err(WsError::Closed) => return,
                    Err(err) => {
                        log::debug!("web socket read ended: {err}");
                        return;
                    }
                }
            }
            // tx_in drops here: the reader side sees end of stream once the
            // queue drains.
        });

        crate::connections::spawn(async move {
            while let Ok(out) = rx_out.recv().await {
                let result = match out {
                    Outbound::Data(chunk) => match sender.send_binary(&chunk).await {
                        Ok(()) => sender.flush().await,
                        Err(err) => Err(err),
                    },
                    Outbound::Flush(ack) => {
                        let flushed = sender.flush().await;
                        let _ = ack.send(());
                        flushed
                    }
                };
                if let Err(err) = result {
                    log::debug!("web socket write failed: {err}");
                    return;
                }
            }
            // The stream closed: say goodbye properly so the browser sees a
            // clean close rather than a reset -- for as long as a live
            // peer needs, no longer.
            smol::future::or(
                async {
                    let _ = sender.close().await;
                },
                async {
                    smol::Timer::after(CLOSE_GRACE).await;
                },
            )
            .await;
        });

        Self {
            inbound: Mutex::new(VecDeque::new()),
            rx_in: Arc::new(rx_in),
            recv: None,
            tx_out: Some(Arc::new(tx_out)),
            send: None,
            flush: None,
            _reader: reader,
        }
    }
}

impl AsyncRead for WebStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<std::io::Result<usize>> {
        loop {
            {
                let mut queue = self.inbound.lock().unwrap_or_else(|e| e.into_inner());
                if !queue.is_empty() {
                    let n = queue.len().min(buf.len());
                    for (dst, src) in buf.iter_mut().zip(queue.drain(..n)) {
                        *dst = src;
                    }
                    return Poll::Ready(Ok(n));
                }
            }
            let this = &mut *self;
            let rx_in = &this.rx_in;
            let recv = this.recv.get_or_insert_with(|| {
                let rx = Arc::clone(rx_in);
                Box::pin(async move { rx.recv().await.ok() })
            });
            match recv.as_mut().poll(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(None) => {
                    self.recv = None;
                    return Poll::Ready(Ok(0));
                }
                Poll::Ready(Some(message)) => {
                    self.recv = None;
                    self.inbound
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .extend(message);
                }
            }
        }
    }
}

impl AsyncWrite for WebStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        if self.send.is_none() {
            let Some(tx) = self.tx_out.clone() else {
                return Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into()));
            };
            if buf.is_empty() {
                return Poll::Ready(Ok(0));
            }
            let chunk = buf[..buf.len().min(OUTBOUND_CHUNK)].to_vec();
            let len = chunk.len();
            self.send = Some((
                Box::pin(async move { tx.send(Outbound::Data(chunk)).await.is_ok() }),
                len,
            ));
        }
        let (fut, len) = self.send.as_mut().expect("just set");
        let len = *len;
        match fut.as_mut().poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(true) => {
                self.send = None;
                Poll::Ready(Ok(len))
            }
            Poll::Ready(false) => {
                self.send = None;
                Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into()))
            }
        }
    }

    /// Resolves once everything queued before it has reached the socket:
    /// the liveness clock stamps a probe as written from here, so it must
    /// not report a Ping still sitting behind pane output.
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        if self.flush.is_none() {
            let Some(tx) = self.tx_out.clone() else {
                return Poll::Ready(Ok(()));
            };
            self.flush = Some(Box::pin(async move {
                let (ack, done) = oneshot::channel();
                if tx.send(Outbound::Flush(ack)).await.is_ok() {
                    let _ = done.await;
                }
            }));
        }
        let fut = self.flush.as_mut().expect("just set");
        match fut.as_mut().poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(()) => {
                self.flush = None;
                Poll::Ready(Ok(()))
            }
        }
    }

    fn poll_close(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        // Dropping the sender ends the writer task, which sends the close
        // frame.
        self.tx_out = None;
        self.send = None;
        self.flush = None;
        Poll::Ready(Ok(()))
    }
}

impl ConnectionStream for WebStream {
    fn wait_for_readable(&self) -> impl Future<Output = std::io::Result<()>> + Send + '_ {
        // Only these two fields are captured: the pending-future slots are
        // not Sync, and a future holding `&self` would not be Send.
        let inbound = &self.inbound;
        let rx_in = &self.rx_in;
        async move {
            loop {
                if inbound.lock().map(|q| !q.is_empty()).unwrap_or(false) {
                    return Ok(());
                }
                // Park until a message with something in it arrives, then
                // keep it for the read that follows. A closed channel is
                // readable too: the read returns end of stream.
                match rx_in.recv().await {
                    Ok(message) if message.is_empty() => continue,
                    Ok(message) => {
                        inbound
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .extend(message);
                        return Ok(());
                    }
                    Err(_) => return Ok(()),
                }
            }
        }
    }
}

/// A stream with some bytes already taken off it: the HTTP layer reads the
/// request head in one gulp, and whatever followed it belongs to the
/// WebSocket protocol.
#[derive(Debug)]
pub struct Prefixed<T> {
    prefix: VecDeque<u8>,
    inner: T,
}

impl<T> Prefixed<T> {
    pub fn new(prefix: Vec<u8>, inner: T) -> Self {
        Self {
            prefix: prefix.into(),
            inner,
        }
    }
}

impl<T: AsyncRead + Unpin> AsyncRead for Prefixed<T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<std::io::Result<usize>> {
        if !self.prefix.is_empty() {
            let n = self.prefix.len().min(buf.len());
            for (dst, src) in buf.iter_mut().zip(self.prefix.drain(..n)) {
                *dst = src;
            }
            return Poll::Ready(Ok(n));
        }
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for Prefixed<T> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_close(cx)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use codec::Pdu;
    use futures::io::AsyncReadExt;
    use smol::Async;
    use std::os::fd::{FromRawFd, IntoRawFd};
    use std::os::unix::net::UnixStream;
    use std::time::Duration;

    /// A browser stand-in: the client half of a WebSocket over a socket
    /// pair, speaking soketto's client mode to our server-mode stream.
    fn pair() -> (
        WebStream,
        soketto::connection::Sender<Async<UnixStream>>,
        soketto::connection::Receiver<Async<UnixStream>>,
    ) {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let ours = unsafe { UnixStream::from_raw_fd(ours.into_raw_fd()) };
        let theirs = unsafe { UnixStream::from_raw_fd(theirs.into_raw_fd()) };
        let server = WebStream::new(Async::new(ours).unwrap());
        let (tx, rx) = Builder::new(Async::new(theirs).unwrap(), Mode::Client).finish();
        (server, tx, rx)
    }

    fn encoded(pdu: Pdu, serial: u64) -> Vec<u8> {
        let mut out = Vec::new();
        pdu.encode(&mut out, serial).unwrap();
        out
    }

    #[test]
    fn a_pdu_split_across_three_messages_decodes_whole() {
        let (mut server, mut client, _rx) = pair();
        let bytes = encoded(Pdu::Ping(codec::Ping {}), 7);
        let cut = bytes.len() / 3;
        smol::block_on(async {
            client.send_binary(&bytes[..cut]).await.unwrap();
            client.send_binary(&bytes[cut..2 * cut]).await.unwrap();
            client.send_binary(&bytes[2 * cut..]).await.unwrap();
            client.flush().await.unwrap();
            let decoded = Pdu::decode_async(&mut server, None).await.unwrap();
            assert_eq!(decoded.serial, 7);
            assert!(matches!(decoded.pdu, Pdu::Ping(_)));
        });
    }

    #[test]
    fn three_pdus_in_one_message_decode_one_by_one() {
        let (mut server, mut client, _rx) = pair();
        let mut bytes = encoded(Pdu::Ping(codec::Ping {}), 1);
        bytes.extend(encoded(Pdu::Pong(codec::Pong {}), 2));
        bytes.extend(encoded(Pdu::GetCodecVersion(codec::GetCodecVersion {}), 3));
        smol::block_on(async {
            client.send_binary(&bytes).await.unwrap();
            client.flush().await.unwrap();
            for expected in 1..=3 {
                let decoded = Pdu::decode_async(&mut server, None).await.unwrap();
                assert_eq!(decoded.serial, expected);
            }
        });
    }

    #[test]
    fn readability_waits_for_bytes_and_survives_into_the_read() {
        let (mut server, mut client, _rx) = pair();
        smol::block_on(async {
            let idle = smol::future::or(
                async {
                    server.wait_for_readable().await.unwrap();
                    true
                },
                async {
                    smol::Timer::after(Duration::from_millis(150)).await;
                    false
                },
            )
            .await;
            assert!(!idle, "readable with nothing sent");

            client.send_binary(b"abc").await.unwrap();
            client.flush().await.unwrap();
            server.wait_for_readable().await.unwrap();
            // The wait pulled the message; a second wait is immediate and
            // the read gets the bytes the wait saw.
            server.wait_for_readable().await.unwrap();
            let mut buf = [0u8; 8];
            let n = server.read(&mut buf).await.unwrap();
            assert_eq!(&buf[..n], b"abc");
        });
    }

    #[test]
    fn writes_arrive_as_binary_messages_that_concatenate() {
        let (mut server, _client, mut rx) = pair();
        // Incompressible, or the codec's zstd would shrink it to one message.
        let mut noise = vec![0u8; 150_000];
        getrandom::fill(&mut noise).unwrap();
        use base64::Engine;
        let big = Pdu::SendPaste(codec::SendPaste {
            pane_id: 1,
            data: base64::engine::general_purpose::STANDARD.encode(&noise),
        });
        let bytes = encoded(big, 9);
        assert!(bytes.len() > 2 * OUTBOUND_CHUNK, "{} bytes", bytes.len());
        smol::block_on(async {
            use futures::io::AsyncWriteExt;
            // Read while writing: the flush is honest now and waits for
            // the socket, which a peer that never reads would never drain.
            let write = async {
                server.write_all(&bytes).await.unwrap();
                server.flush().await.unwrap();
            };
            let read = async {
                let mut got = Vec::new();
                let mut messages = 0;
                while got.len() < bytes.len() {
                    let mut m = Vec::new();
                    match rx.receive_data(&mut m).await.unwrap() {
                        Data::Binary(_) => got.extend(m),
                        Data::Text(_) => panic!("server sent a text frame"),
                    }
                    messages += 1;
                }
                (got, messages)
            };
            let ((), (got, messages)) = futures::future::join(write, read).await;
            assert_eq!(got, bytes);
            assert!(messages > 1, "a 200 KB PDU should leave in several messages");
        });
    }

    #[test]
    fn a_peer_that_closes_mid_pdu_ends_the_stream() {
        let (mut server, mut client, _rx) = pair();
        let bytes = encoded(Pdu::GetCodecVersion(codec::GetCodecVersion {}), 4);
        smol::block_on(async {
            client.send_binary(&bytes[..2]).await.unwrap();
            client.flush().await.unwrap();
            client.close().await.unwrap();
            let err = Pdu::decode_async(&mut server, None)
                .await
                .expect_err("half a PDU cannot decode");
            let io = err
                .root_cause()
                .downcast_ref::<std::io::Error>()
                .expect("an io error");
            assert_eq!(io.kind(), std::io::ErrorKind::UnexpectedEof);
        });
    }

    #[test]
    fn closing_the_stream_sends_a_close_frame() {
        let (mut server, _client, mut rx) = pair();
        smol::block_on(async {
            use futures::io::AsyncWriteExt;
            server.close().await.unwrap();
            let mut m = Vec::new();
            let err = rx.receive_data(&mut m).await.expect_err("closed");
            assert!(matches!(err, WsError::Closed), "{}", err);
        });
    }

    /// An empty binary frame is nothing, not a wake: a reader woken by it
    /// would enter a decode on an empty queue and sit there with the
    /// stall clock never started.
    #[test]
    fn an_empty_frame_does_not_report_readable() {
        let (mut server, mut client, _rx) = pair();
        smol::block_on(async {
            client.send_binary(b"").await.unwrap();
            client.flush().await.unwrap();
            let woke = smol::future::or(
                async {
                    server.wait_for_readable().await.unwrap();
                    true
                },
                async {
                    smol::Timer::after(Duration::from_millis(200)).await;
                    false
                },
            )
            .await;
            assert!(!woke, "an empty frame woke the reader");
            // And a real message after it still gets through.
            client.send_binary(b"xyz").await.unwrap();
            client.flush().await.unwrap();
            server.wait_for_readable().await.unwrap();
            let mut buf = [0u8; 8];
            let n = server.read(&mut buf).await.unwrap();
            assert_eq!(&buf[..n], b"xyz");
        });
    }

    /// Dropping the stream closes the socket: the reader task is
    /// cancelled and the writer's goodbye is bounded, so a peer that never
    /// answers cannot keep the descriptor.
    #[test]
    fn dropping_the_stream_closes_the_socket() {
        use std::os::fd::AsRawFd;
        let (ours, theirs) = UnixStream::pair().unwrap();
        let fd = ours.as_raw_fd();
        let ours = unsafe { UnixStream::from_raw_fd(ours.into_raw_fd()) };
        let server = WebStream::new(Async::new(ours).unwrap());
        // The peer never reads or writes; it is only kept alive.
        let _theirs = theirs;
        let open = |fd: i32| unsafe { libc::fcntl(fd, libc::F_GETFD) } != -1;
        assert!(open(fd));
        drop(server);
        let started = std::time::Instant::now();
        while open(fd) {
            assert!(
                started.elapsed() < CLOSE_GRACE + Duration::from_secs(5),
                "the socket stayed open after the stream was dropped"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// A flush resolves only once the writer has put everything before it
    /// on the wire, so a probe stamped as written really has left.
    #[test]
    fn a_flush_waits_for_the_writer() {
        let (mut server, _client, mut rx) = pair();
        smol::block_on(async {
            use futures::io::AsyncWriteExt;
            server.write_all(b"before").await.unwrap();
            server.flush().await.unwrap();
            let mut m = Vec::new();
            // Already there: no waiting on the peer to see it.
            let got = smol::future::or(
                async { rx.receive_data(&mut m).await.is_ok() },
                async {
                    smol::Timer::after(Duration::from_millis(500)).await;
                    false
                },
            )
            .await;
            assert!(got, "the flushed bytes had not been written");
            assert_eq!(m, b"before");
        });
    }
}
