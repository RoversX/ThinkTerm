use crate::budget::Reservation;
use crate::clock::{Clock, SystemClock, Tick};
use crate::wire::{Decoder, Frame, Kind, MAX_FRAME};
use crate::{Error, Result};
use futures::future::poll_fn;
use futures::io::{AsyncRead, AsyncReadExt, AsyncWrite};
use serde::{de::DeserializeOwned, Serialize};
use smol::future::FutureExt;
use std::io::{self, Write};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

#[derive(Clone)]
pub struct Deadline {
    clock: Arc<dyn Clock>,
    end: Tick,
    last: Arc<Mutex<Tick>>,
}

/// Kept back at each forwarding stage, so that whatever a request ran out of
/// time on can still be answered: a serial channel needs its reply.
pub const REPLY_MARGIN: Duration = Duration::from_millis(500);

impl Deadline {
    pub fn after(duration: Duration) -> Result<Self> {
        let clock = SystemClock::shared();
        let now = clock.now()?;
        Ok(Self {
            clock,
            end: now.after(duration)?,
            last: Arc::new(Mutex::new(now)),
        })
    }

    pub fn request() -> Result<Self> {
        Self::after(Duration::from_secs(crate::REQUEST_TIMEOUT_SECS))
    }

    /// This deadline, `margin` sooner.
    pub fn earlier(&self, margin: Duration) -> Result<Self> {
        Self::after(
            self.remaining()?
                .checked_sub(margin)
                .filter(|left| !left.is_zero())
                .ok_or(Error::Timeout)?,
        )
    }

    pub fn remaining(&self) -> Result<Duration> {
        let now = self.clock.now()?;
        let mut last = self.last.lock().map_err(|_| Error::Clock)?;
        if now < *last {
            return Err(Error::Clock);
        }
        *last = now;
        if now >= self.end {
            return Err(Error::Timeout);
        }
        self.end.elapsed_since(now)
    }

    pub async fn run<T>(&self, future: impl std::future::Future<Output = Result<T>>) -> Result<T> {
        let remaining = self.remaining()?;
        let result = future
            .or(async {
                smol::Timer::after(remaining).await;
                Err(Error::Timeout)
            })
            .await;
        self.remaining()?;
        result
    }

    /// Busy means this attempt did no work. Yield before retrying the
    /// guarded operation, without restarting the request's deadline.
    pub async fn retry_busy<T>(
        &self,
        mut operation: impl FnMut(&mut Context<'_>) -> Poll<Result<T>>,
    ) -> Result<T> {
        self.run(async {
            loop {
                let result = poll_fn(|cx| {
                    if let Err(error) = self.remaining() {
                        return Poll::Ready(Err(error));
                    }
                    operation(cx)
                })
                .await;
                match result {
                    Err(Error::Busy) => {
                        smol::Timer::after(Duration::from_millis(1)).await;
                    }
                    result => return result,
                }
            }
        })
        .await
    }
}

/// A single bounded receive buffer and frame decoder. The reservation also
/// accounts for JSON collections and outgoing frame copies. It survives
/// every read and write for this connection's lifetime.
pub struct Channel<S> {
    stream: SharedStream<S>,
    decoder: Decoder,
    buffer: [u8; 8192],
    start: usize,
    end: usize,
    frame_deadline: Option<Deadline>,
    clock: Arc<dyn Clock>,
    _reservation: Reservation,
}

impl<S: AsyncRead + AsyncWrite + Unpin> Channel<S> {
    pub fn new(stream: S, reservation: Reservation) -> Result<Self> {
        if reservation.bytes() < 1024 * 1024 {
            return Err(Error::Protocol);
        }
        Ok(Self {
            stream: SharedStream(Arc::new(Mutex::new(stream))),
            decoder: Decoder::default(),
            buffer: [0; 8192],
            start: 0,
            end: 0,
            frame_deadline: None,
            clock: SystemClock::shared(),
            _reservation: reservation,
        })
    }

    /// With no request deadline, an entirely idle channel sleeps until its
    /// first byte. A partial frame always has a five-second total deadline.
    pub async fn read(&mut self, deadline: Option<&Deadline>) -> Result<Frame> {
        loop {
            if let Some(deadline) = deadline {
                deadline.remaining()?;
            }
            if let Some(frame) = &self.frame_deadline {
                frame.remaining()?;
            }
            if self.start == self.end {
                let stream = &mut self.stream;
                let buffer = &mut self.buffer;
                let read = async { stream.read(buffer).await.map_err(|_| Error::Unavailable) };
                let request_remaining = deadline.map(Deadline::remaining).transpose()?;
                let frame_remaining = self
                    .frame_deadline
                    .as_ref()
                    .map(Deadline::remaining)
                    .transpose()?;
                let timeout = match (request_remaining, frame_remaining) {
                    (Some(a), Some(b)) => Some(a.min(b)),
                    (a, b) => a.or(b),
                };
                self.end = if let Some(timeout) = timeout {
                    read.or(async {
                        smol::Timer::after(timeout).await;
                        Err(Error::Timeout)
                    })
                    .await?
                } else {
                    read.await?
                };
                self.start = 0;
                if self.end == 0 {
                    return Err(Error::Unavailable);
                }
            }
            if self.decoder.is_idle() && self.frame_deadline.is_none() {
                self.frame_deadline = Some(Deadline::after(Duration::from_secs(5))?);
            }
            if let Some(deadline) = deadline {
                deadline.remaining()?;
            }
            self.frame_deadline
                .as_ref()
                .ok_or(Error::Protocol)?
                .remaining()?;
            let (used, frame) = self
                .decoder
                .push(&self.buffer[self.start..self.end], self.clock.now()?)?;
            self.start += used;
            if let Some(frame) = frame {
                self.frame_deadline = None;
                return Ok(frame);
            }
        }
    }

    pub async fn write(&mut self, frame: &Frame, deadline: &Deadline) -> Result<()> {
        self.write_guarded(frame, deadline, |write| Ok(write()))
            .await
    }

    /// The authorization guard encloses each nonblocking poll, including
    /// retries after Pending. It never authorizes an unchecked future write.
    pub async fn write_guarded<G>(
        &mut self,
        frame: &Frame,
        deadline: &Deadline,
        mut guard: G,
    ) -> Result<()>
    where
        G: FnMut(&mut dyn FnMut() -> Poll<io::Result<usize>>) -> Result<Poll<io::Result<usize>>>,
    {
        let header = frame.header();
        for part in [&header[..], frame.body()] {
            let mut offset = 0;
            while offset < part.len() {
                let count = deadline
                    .retry_busy(|cx| {
                        match guard(&mut || {
                            Pin::new(&mut self.stream).poll_write(cx, &part[offset..])
                        }) {
                            Ok(Poll::Pending) => Poll::Pending,
                            Ok(Poll::Ready(Ok(count))) if count > 0 => Poll::Ready(Ok(count)),
                            Ok(Poll::Ready(_)) => Poll::Ready(Err(Error::Unavailable)),
                            Err(error) => Poll::Ready(Err(error)),
                        }
                    })
                    .await?;
                if count > part.len() - offset {
                    return Err(Error::Protocol);
                }
                offset += count;
            }
        }
        Ok(())
    }

    pub async fn write_json<T: Serialize>(
        &mut self,
        kind: Kind,
        sequence: u64,
        value: &T,
        deadline: &Deadline,
    ) -> Result<()> {
        self.write(&json_frame(kind, sequence, value)?, deadline)
            .await
    }

    /// Business exchanges are serial. After receiving the complete request,
    /// any further byte is a protocol violation; EOF cancels the operation.
    pub fn watch_peer(&self) -> PeerWatch<S> {
        PeerWatch {
            stream: SharedStream(Arc::clone(&self.stream.0)),
            buffered: self.start != self.end,
        }
    }
}

struct SharedStream<S>(Arc<Mutex<S>>);

impl<S: AsyncRead + Unpin> AsyncRead for SharedStream<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        bytes: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        match self.0.lock() {
            Ok(mut stream) => Pin::new(&mut *stream).poll_read(cx, bytes),
            Err(_) => Poll::Ready(Err(io::ErrorKind::BrokenPipe.into())),
        }
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for SharedStream<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.0.lock() {
            Ok(mut stream) => Pin::new(&mut *stream).poll_write(cx, bytes),
            Err(_) => Poll::Ready(Err(io::ErrorKind::BrokenPipe.into())),
        }
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<io::Result<()>> {
        match self.0.lock() {
            Ok(mut stream) => Pin::new(&mut *stream).poll_flush(cx),
            Err(_) => Poll::Ready(Err(io::ErrorKind::BrokenPipe.into())),
        }
    }
    fn poll_close(self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<io::Result<()>> {
        match self.0.lock() {
            Ok(mut stream) => Pin::new(&mut *stream).poll_close(cx),
            Err(_) => Poll::Ready(Err(io::ErrorKind::BrokenPipe.into())),
        }
    }
}

pub struct PeerWatch<S> {
    stream: SharedStream<S>,
    buffered: bool,
}

impl<S: AsyncRead + Unpin> std::future::Future for PeerWatch<S> {
    type Output = Result<()>;
    fn poll(mut self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<Self::Output> {
        if self.buffered {
            return Poll::Ready(Err(Error::Protocol));
        }
        match Pin::new(&mut self.stream).poll_read(cx, &mut [0; 1]) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Ok(count)) if count > 0 => Poll::Ready(Err(Error::Protocol)),
            Poll::Ready(_) => Poll::Ready(Err(Error::Unavailable)),
        }
    }
}

pub fn json_frame<T: Serialize>(kind: Kind, sequence: u64, value: &T) -> Result<Frame> {
    struct Bounded(Vec<u8>);
    impl Write for Bounded {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if bytes.len() > MAX_FRAME - self.0.len() {
                return Err(io::ErrorKind::InvalidInput.into());
            }
            self.0.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut output = Bounded(Vec::with_capacity(MAX_FRAME));
    serde_json::to_writer(&mut output, value).map_err(|_| Error::TooLarge)?;
    Frame::new(kind, sequence, &output.0)
}

pub fn parse<T: DeserializeOwned>(frame: &Frame, kind: Kind, sequence: u64) -> Result<T> {
    if frame.kind() != kind || frame.sequence() != sequence {
        return Err(Error::Protocol);
    }
    serde_json::from_slice(frame.body()).map_err(|_| Error::Protocol)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::budget::Budgets;
    use crate::Id;

    struct Partial(Vec<u8>);
    impl AsyncRead for Partial {
        fn poll_read(
            self: Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
            _: &mut [u8],
        ) -> Poll<io::Result<usize>> {
            Poll::Pending
        }
    }
    impl AsyncWrite for Partial {
        fn poll_write(
            mut self: Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
            bytes: &[u8],
        ) -> Poll<io::Result<usize>> {
            self.0.push(bytes[0]);
            Poll::Ready(Ok(1))
        }
        fn poll_flush(
            self: Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_close(
            self: Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }
    fn channel() -> Channel<Partial> {
        let budgets = Budgets::new().unwrap();
        let route = budgets.open_route(Id::from_bytes([1; 16])).unwrap();
        Channel::new(Partial(vec![]), route.acquire(1024 * 1024).unwrap()).unwrap()
    }

    #[test]
    fn an_earlier_deadline_ends_sooner_and_never_outlives_its_own() {
        let deadline = Deadline::after(Duration::from_secs(10)).unwrap();
        let earlier = deadline.earlier(Duration::from_secs(4)).unwrap();
        assert!(earlier.remaining().unwrap() <= Duration::from_secs(6));
        assert!(earlier.remaining().unwrap() < deadline.remaining().unwrap());
        assert!(matches!(
            deadline.earlier(Duration::from_secs(11)),
            Err(Error::Timeout)
        ));
    }

    #[test]
    fn transient_guard_contention_resumes_partial_frames_without_duplication() {
        let mut channel = channel();
        let frame = Frame::new(Kind::Reply, 1, b"example text").unwrap();
        let mut checks = 0;
        let result =
            smol::block_on(
                channel.write_guarded(&frame, &Deadline::request().unwrap(), |write| {
                    checks += 1;
                    if [1, 5, 25].contains(&checks) {
                        Err(Error::Busy)
                    } else {
                        Ok(write())
                    }
                }),
            );
        assert_eq!(result, Ok(()));
        let mut expected = frame.header().to_vec();
        expected.extend_from_slice(frame.body());
        assert_eq!(channel.stream.0.lock().unwrap().0, expected);
        assert_eq!(checks, expected.len() + 3);
    }

    #[test]
    fn a_busy_guard_rechecks_revocation_and_other_terminal_failures() {
        for error in [
            Error::Revoked,
            Error::Unavailable,
            Error::Clock,
            Error::Timeout,
        ] {
            let mut channel = channel();
            let frame = Frame::new(Kind::Reply, 1, b"example text").unwrap();
            let mut checks = 0;
            let result = smol::block_on(channel.write_guarded(
                &frame,
                &Deadline::request().unwrap(),
                |write| {
                    checks += 1;
                    match checks {
                        1 | 2 => Ok(write()),
                        3 => Err(Error::Busy),
                        _ => Err(error),
                    }
                },
            ));
            assert_eq!(result, Err(error));
            assert_eq!(channel.stream.0.lock().unwrap().0, &frame.header()[..2]);
            assert_eq!(checks, 4);
        }
    }

    #[test]
    fn repeated_contention_cannot_restart_the_request_deadline() {
        use std::sync::atomic::{AtomicU64, Ordering};
        struct TestClock(AtomicU64);
        impl Clock for TestClock {
            fn now(&self) -> Result<Tick> {
                Ok(Tick::seconds(self.0.load(Ordering::Acquire)))
            }
        }
        let clock = Arc::new(TestClock(AtomicU64::new(0)));
        let deadline = Deadline {
            clock: clock.clone(),
            end: Tick::seconds(10),
            last: Arc::new(Mutex::new(Tick::seconds(0))),
        };
        let mut channel = channel();
        let frame = Frame::new(Kind::Reply, 1, b"example text").unwrap();
        let mut checks = 0;
        let result = smol::block_on(channel.write_guarded(&frame, &deadline, |_| {
            checks += 1;
            clock.0.store(8 + checks, Ordering::Release);
            Err(Error::Busy)
        }));
        assert_eq!(result, Err(Error::Timeout));
        assert_eq!(checks, 2);
        assert!(channel.stream.0.lock().unwrap().0.is_empty());
    }

    #[test]
    fn revocation_guard_runs_before_every_partial_transport_write() {
        let mut channel = channel();
        let frame = Frame::new(Kind::Reply, 1, b"private text").unwrap();
        let mut checks = 0;
        let result =
            smol::block_on(
                channel.write_guarded(&frame, &Deadline::request().unwrap(), |write| {
                    checks += 1;
                    if checks > 3 {
                        Err(Error::Revoked)
                    } else {
                        Ok(write())
                    }
                }),
            );
        assert_eq!(result, Err(Error::Revoked));
        assert_eq!(channel.stream.0.lock().unwrap().0.len(), 3);
        assert_eq!(checks, 4);
    }
}
