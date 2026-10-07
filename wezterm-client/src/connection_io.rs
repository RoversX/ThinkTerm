use anyhow::{bail, Context};
use smol::prelude::*;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context as TaskContext, Poll};
use std::time::{Duration, Instant};

pub(crate) const IO_STALL_LIMIT: Duration = Duration::from_secs(30);

/// Cancelled reads may have consumed part of a frame. The caller must drop
/// the connection on any error, never start another decode on that stream.
pub(crate) async fn with_progress<T>(
    bytes: &AtomicU64,
    limit: Duration,
    operation: impl std::future::Future<Output = anyhow::Result<T>>,
) -> anyhow::Result<T> {
    smol::future::or(operation, async {
        let mut seen = bytes.load(Ordering::Relaxed);
        let mut last_progress = Instant::now();
        loop {
            smol::Timer::after(limit.min(Duration::from_secs(1))).await;
            let current = bytes.load(Ordering::Relaxed);
            if current != seen {
                seen = current;
                last_progress = Instant::now();
            } else if last_progress.elapsed() >= limit {
                bail!("mux connection made no IO progress for {limit:?}");
            }
        }
    })
    .await
}

#[derive(Debug)]
pub(crate) struct Counted<'a, S: ?Sized> {
    pub stream: &'a mut S,
    pub bytes: &'a AtomicU64,
}

impl<S: AsyncRead + Unpin + ?Sized> AsyncRead for Counted<'_, S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &mut [u8],
    ) -> Poll<std::io::Result<usize>> {
        let this = &mut *self;
        let result = Pin::new(&mut *this.stream).poll_read(cx, buf);
        if let Poll::Ready(Ok(n)) = &result {
            this.bytes.fetch_add(*n as u64, Ordering::Relaxed);
        }
        result
    }
}

impl<S: AsyncWrite + Unpin + ?Sized> AsyncWrite for Counted<'_, S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let this = &mut *self;
        let result = Pin::new(&mut *this.stream).poll_write(cx, buf);
        if let Poll::Ready(Ok(n)) = &result {
            this.bytes.fetch_add(*n as u64, Ordering::Relaxed);
        }
        result
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut *self.stream).poll_flush(cx)
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut *self.stream).poll_close(cx)
    }
}

pub(crate) async fn write_pdu<S: AsyncWrite + Unpin + ?Sized>(
    stream: &mut S,
    pdu: &codec::Pdu,
    serial: u64,
) -> anyhow::Result<()> {
    let bytes = AtomicU64::new(0);
    let mut counted = Counted {
        stream,
        bytes: &bytes,
    };
    with_progress(&bytes, IO_STALL_LIMIT, async {
        pdu.encode_async(&mut counted, serial)
            .await
            .context("encoding PDU")?;
        counted.flush().await.context("flushing PDU")
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stalled_operation_times_out() {
        smol::block_on(async {
            let bytes = AtomicU64::new(0);
            let result: anyhow::Result<()> =
                with_progress(&bytes, Duration::from_millis(15), std::future::pending()).await;
            assert!(result.unwrap_err().to_string().contains("no IO progress"));
        });
    }

    #[test]
    fn a_long_transfer_is_allowed_while_it_makes_progress() {
        smol::block_on(async {
            let bytes = AtomicU64::new(0);
            with_progress(&bytes, Duration::from_millis(40), async {
                for _ in 0..12 {
                    smol::Timer::after(Duration::from_millis(10)).await;
                    bytes.fetch_add(1, Ordering::Relaxed);
                }
                Ok(())
            })
            .await
            .unwrap();
        });
    }
}
