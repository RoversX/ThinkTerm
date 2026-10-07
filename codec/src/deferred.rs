//! Bounded, serialized storage for messages waiting for mux registration.
//! Terminal lines must not be materialized until a connection can use them.
use crate::{Decoded, DecodedPdu, Payload, Pdu, PAYLOAD_READ_STEP};
use anyhow::{ensure, Context};
use futures_lite::prelude::*;
use std::io::{Read, Write};

#[derive(Debug)]
pub struct DeferredPdu(Decoded);

impl DeferredPdu {
    pub async fn read_async<R: AsyncRead + Unpin + std::fmt::Debug>(
        reader: &mut R,
        max_serial: Option<u64>,
        push_budget: usize,
    ) -> anyhow::Result<Self> {
        let mut decoded =
            crate::decode_raw_async_limited(reader, max_serial, Some(push_budget)).await?;
        // Count decompressed bytes, not compressed wire bytes. A highly
        // compressible push must not bypass the registration budget.
        if decoded.serial == 0 && decoded.ident != 1 && decoded.is_compressed {
            #[cfg(not(target_family = "wasm"))]
            let reader = zstd::stream::read::Decoder::new(decoded.data.reader())?;
            #[cfg(target_family = "wasm")]
            let reader = ruzstd::decoding::StreamingDecoder::new(decoded.data.reader())
                .map_err(|err| anyhow::anyhow!("decompressing deferred PDU: {err}"))?;
            let data = read_bounded(reader, push_budget)?;
            decoded.data = data;
            decoded.is_compressed = false;
        }
        Ok(Self(decoded))
    }

    pub fn is_push(&self) -> bool {
        self.0.serial == 0 && self.0.ident != 1
    }

    pub fn retained_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + self.0.data.pieces.capacity() * std::mem::size_of::<Vec<u8>>()
            + self.0.data.pieces.iter().map(Vec::capacity).sum::<usize>()
    }

    pub fn decode(self) -> anyhow::Result<DecodedPdu> {
        Pdu::from_decoded(self.0)
    }

    pub(crate) fn from_payload(ident: u64, data: Vec<u8>) -> Self {
        let mut payload = Payload::default();
        payload.push(data);
        Self(Decoded {
            ident,
            serial: 0,
            data: payload,
            is_compressed: false,
        })
    }
}

fn read_bounded(mut reader: impl Read, budget: usize) -> anyhow::Result<Payload> {
    let mut payload = Payload::default();
    loop {
        let remaining = budget.saturating_sub(payload.len());
        let mut piece = vec![0; remaining.min(PAYLOAD_READ_STEP).max(1)];
        let n = reader
            .read(&mut piece)
            .context("decompressing deferred PDU")?;
        if n == 0 {
            return Ok(payload);
        }
        ensure!(
            n <= remaining,
            "registration push exceeds decompressed byte budget"
        );
        piece.truncate(n);
        // No spare megabyte for the final short read.
        piece.shrink_to_fit();
        payload.push(piece);
    }
}

/// A serializer must stop before allocating an oversized deferred request.
pub(crate) struct LimitedWriter {
    pub data: Vec<u8>,
    pub limit: usize,
}

impl Write for LimitedWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let total = self
            .data
            .len()
            .checked_add(bytes.len())
            .filter(|n| *n <= self.limit)
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::Other,
                    "registration request exceeds byte budget",
                )
            })?;
        if total > self.data.capacity() {
            let capacity = total.saturating_add(4095).min(self.limit) & !4095;
            self.data
                .reserve_exact(capacity.max(total) - self.data.len());
        }
        self.data.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decompressed_data_cannot_exceed_its_budget() {
        assert!(read_bounded(&b"12345"[..], 4).is_err());
        assert_eq!(read_bounded(&b"1234"[..], 4).unwrap().len(), 4);
    }

    #[test]
    fn serialization_stops_at_the_budget() {
        let mut writer = LimitedWriter {
            data: vec![],
            limit: 4,
        };
        writer.write_all(b"1234").unwrap();
        assert!(writer.write_all(b"5").is_err());
        assert_eq!(writer.data, b"1234");
        assert!(writer.data.capacity() <= 4);
    }
}

#[cfg(test)]
mod protocol_tests {
    use super::*;
    use crate::{Ping, WindowTitleChanged};

    #[test]
    fn raw_and_compressed_pushes_round_trip_in_order() {
        futures_lite::future::block_on(async {
            for link in [crate::Link::SameMachine, crate::Link::Network] {
                let pdu = Pdu::WindowTitleChanged(WindowTitleChanged {
                    window_id: 2,
                    title: "content".repeat(200),
                });
                let mut wire = Vec::new();
                pdu.encode_async_over(&mut wire, 0, link).await.unwrap();
                let deferred = DeferredPdu::read_async(&mut &wire[..], None, 8192)
                    .await
                    .unwrap();
                assert!(deferred.is_push());
                assert!(deferred.retained_bytes() < 8192);
                assert_eq!(deferred.decode().unwrap().pdu, pdu);
            }
        });
    }

    #[test]
    fn a_compressed_push_cannot_hide_its_expanded_size() {
        futures_lite::future::block_on(async {
            let pdu = Pdu::WindowTitleChanged(WindowTitleChanged {
                window_id: 0,
                title: "x".repeat(256 * 1024),
            });
            let mut wire = Vec::new();
            pdu.encode_async(&mut wire, 0).await.unwrap();
            assert!(wire.len() < 1024);
            let error = DeferredPdu::read_async(&mut &wire[..], None, 1024)
                .await
                .unwrap_err();
            assert!(format!("{error:#}").contains("decompressed byte budget"));
        });
    }

    #[test]
    fn an_oversized_push_is_refused_before_reading_its_payload() {
        futures_lite::future::block_on(async {
            let header = crate::encode_header(25, 0, 4096, false, 0).unwrap();
            let error = DeferredPdu::read_async(&mut &header[..], None, 128)
                .await
                .unwrap_err();
            assert!(error.to_string().contains("byte budget"));
        });
    }

    #[test]
    fn ping_works_even_when_the_push_budget_is_exhausted() {
        futures_lite::future::block_on(async {
            let mut wire = Vec::new();
            Pdu::Ping(Ping {}).encode_async(&mut wire, 0).await.unwrap();
            let deferred = DeferredPdu::read_async(&mut &wire[..], None, 0)
                .await
                .unwrap();
            assert!(!deferred.is_push());
            assert!(matches!(deferred.decode().unwrap().pdu, Pdu::Ping(_)));
        });
    }
}
