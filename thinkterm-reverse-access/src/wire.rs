use crate::budget::{Reservation, RouteBudget};
use crate::clock::Tick;
use crate::{Error, Result, MAX_INPUT_BYTES, MAX_READ_BYTES};
use std::fmt;
use std::time::Duration;

pub const HEADER_SIZE: usize = 20;
pub const MAX_FRAME: usize = 32 * 1024;
pub const MAX_BLOCK: usize = 16 * 1024;
const RECEIVE_TIMEOUT: Duration = Duration::from_secs(5);
const MAGIC: &[u8; 4] = b"TTCR";
const VERSION: u8 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Kind {
    Hello = 1,
    Bound = 2,
    Request = 3,
    InputBlock = 4,
    Reply = 5,
    TextBlock = 6,
    Cancel = 7,
    Withdraw = 8,
}

impl Kind {
    fn parse(value: u8) -> Result<Self> {
        match value {
            1 => Ok(Self::Hello),
            2 => Ok(Self::Bound),
            3 => Ok(Self::Request),
            4 => Ok(Self::InputBlock),
            5 => Ok(Self::Reply),
            6 => Ok(Self::TextBlock),
            7 => Ok(Self::Cancel),
            8 => Ok(Self::Withdraw),
            _ => Err(Error::Protocol),
        }
    }

    fn limit(self) -> usize {
        match self {
            Self::Hello | Self::Bound | Self::Cancel | Self::Withdraw => 1024,
            Self::InputBlock | Self::TextBlock => MAX_BLOCK + 4,
            Self::Request | Self::Reply => MAX_FRAME - HEADER_SIZE,
        }
    }

    fn check_sequence(self, sequence: u64) -> Result<()> {
        let connection_message = matches!(self, Self::Hello | Self::Bound | Self::Withdraw);
        if connection_message != (sequence == 0) {
            return Err(Error::Protocol);
        }
        Ok(())
    }
}

pub struct Frame {
    kind: Kind,
    sequence: u64,
    body: Vec<u8>,
}

impl fmt::Debug for Frame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Frame")
            .field("kind", &self.kind)
            .field("sequence", &self.sequence)
            .field("bytes", &self.body.len())
            .finish()
    }
}

impl Frame {
    pub fn new(kind: Kind, sequence: u64, body: &[u8]) -> Result<Self> {
        kind.check_sequence(sequence)?;
        if body.len() > kind.limit() {
            return Err(Error::TooLarge);
        }
        Ok(Self {
            kind,
            sequence,
            body: body.to_vec(),
        })
    }

    pub fn kind(&self) -> Kind {
        self.kind
    }
    pub fn sequence(&self) -> u64 {
        self.sequence
    }
    pub fn body(&self) -> &[u8] {
        &self.body
    }

    pub fn header(&self) -> [u8; HEADER_SIZE] {
        let mut header = [0; HEADER_SIZE];
        header[..4].copy_from_slice(MAGIC);
        header[4] = VERSION;
        header[5] = self.kind as u8;
        // Flags/reserved bytes stay zero. Compression is not supported.
        header[8..16].copy_from_slice(&self.sequence.to_be_bytes());
        header[16..].copy_from_slice(&(self.body.len() as u32).to_be_bytes());
        header
    }
}

#[derive(Default)]
pub struct Decoder {
    header: [u8; HEADER_SIZE],
    header_len: usize,
    pending: Option<Frame>,
    body_len: usize,
    started: Option<Tick>,
    failed: bool,
}

impl Decoder {
    /// Feed at most one frame. The caller retains a frame-sized receive
    /// reservation while the decoder is alive. Lengths are checked before
    /// body allocation; a trickle never resets the absolute deadline.
    pub fn push(&mut self, bytes: &[u8], now: Tick) -> Result<(usize, Option<Frame>)> {
        if self.failed {
            return Err(Error::Protocol);
        }
        let result = self.push_inner(bytes, now);
        if result.is_err() {
            self.failed = true;
            self.pending = None;
        }
        result
    }

    fn push_inner(&mut self, bytes: &[u8], now: Tick) -> Result<(usize, Option<Frame>)> {
        if let Some(started) = self.started {
            if now.elapsed_since(started)? >= RECEIVE_TIMEOUT {
                return Err(Error::Timeout);
            }
        } else if !bytes.is_empty() {
            self.started = Some(now);
        }
        let mut used = 0;
        if self.header_len < HEADER_SIZE {
            let count = bytes.len().min(HEADER_SIZE - self.header_len);
            self.header[self.header_len..self.header_len + count].copy_from_slice(&bytes[..count]);
            self.header_len += count;
            used += count;
            if self.header_len != HEADER_SIZE {
                return Ok((used, None));
            }
            if &self.header[..4] != MAGIC
                || self.header[4] != VERSION
                || self.header[6] != 0
                || self.header[7] != 0
            {
                return Err(Error::Protocol);
            }
            let kind = Kind::parse(self.header[5])?;
            let sequence = u64::from_be_bytes(self.header[8..16].try_into().unwrap());
            kind.check_sequence(sequence)?;
            self.body_len = u32::from_be_bytes(self.header[16..20].try_into().unwrap()) as usize;
            if self.body_len > kind.limit() {
                return Err(Error::TooLarge);
            }
            self.pending = Some(Frame {
                kind,
                sequence,
                body: Vec::with_capacity(self.body_len),
            });
        }
        let pending = self.pending.as_mut().ok_or(Error::Protocol)?;
        let count = (self.body_len - pending.body.len()).min(bytes.len() - used);
        pending.body.extend_from_slice(&bytes[used..used + count]);
        used += count;
        if pending.body.len() == self.body_len {
            let frame = self.pending.take();
            self.header_len = 0;
            self.started = None;
            Ok((used, frame))
        } else {
            Ok((used, None))
        }
    }

    pub fn is_idle(&self) -> bool {
        self.started.is_none() && !self.failed
    }
}

/// A request is assembled completely before any input can be submitted.
/// The reservation travels with the completed payload through its reply.
pub struct Assembly {
    total: usize,
    bytes: Vec<u8>,
    reservation: Reservation,
}

impl Assembly {
    pub fn input(total: usize, budget: &RouteBudget) -> Result<Self> {
        Self::new(total, MAX_INPUT_BYTES, budget)
    }

    pub fn text(total: usize, budget: &RouteBudget) -> Result<Self> {
        Self::new(total, MAX_READ_BYTES, budget)
    }

    /// A reply's bytes join its request's reservation. The request already
    /// paid the rate limit and holds a slot, so only its size can be refused.
    pub fn reply_text(total: usize, mut reservation: Reservation) -> Result<Self> {
        if total > MAX_READ_BYTES {
            return Err(Error::TooLarge);
        }
        reservation.grow(total)?;
        Ok(Self {
            total,
            bytes: Vec::with_capacity(total),
            reservation,
        })
    }

    fn new(total: usize, limit: usize, budget: &RouteBudget) -> Result<Self> {
        if total > limit {
            return Err(Error::TooLarge);
        }
        let reservation = budget.acquire(total)?;
        Ok(Self {
            total,
            bytes: Vec::with_capacity(total),
            reservation,
        })
    }

    pub fn append(&mut self, offset: usize, bytes: &[u8]) -> Result<()> {
        if offset != self.bytes.len()
            || bytes.is_empty()
            || bytes.len() > MAX_BLOCK
            || bytes.len() > self.total - self.bytes.len()
        {
            return Err(Error::Protocol);
        }
        self.bytes.extend_from_slice(bytes);
        Ok(())
    }

    pub fn finish(self) -> Result<Payload> {
        if self.bytes.len() != self.total {
            return Err(Error::Protocol);
        }
        std::str::from_utf8(&self.bytes).map_err(|_| Error::Protocol)?;
        Ok(Payload {
            bytes: self.bytes,
            reservation: self.reservation,
        })
    }
}

pub struct Payload {
    bytes: Vec<u8>,
    reservation: Reservation,
}

impl Payload {
    pub fn text(&self) -> &str {
        // Validated once at assembly completion; bytes are never mutated.
        std::str::from_utf8(&self.bytes).expect("validated Connect text")
    }

    pub fn reservation(&mut self) -> &mut Reservation {
        &mut self.reservation
    }
}

impl fmt::Debug for Payload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Payload")
            .field("bytes", &self.bytes.len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::budget::Budgets;
    use crate::Id;

    #[test]
    fn every_split_decodes_without_losing_the_next_frame() {
        let original = Frame::new(Kind::InputBlock, 12, b"example payload").unwrap();
        let bytes = [original.header().as_slice(), original.body()].concat();
        for split in 0..=bytes.len() {
            let mut decoder = Decoder::default();
            let (first, frame) = decoder.push(&bytes[..split], Tick::seconds(0)).unwrap();
            assert_eq!(first, split);
            let frame = match frame {
                Some(frame) => frame,
                None => decoder
                    .push(&bytes[split..], Tick::seconds(1))
                    .unwrap()
                    .1
                    .unwrap(),
            };
            assert_eq!(frame.body(), original.body());
            assert!(decoder.is_idle());
        }
    }

    #[test]
    fn oversize_or_compressed_headers_fail_before_a_body_is_allocated() {
        for compressed in [false, true] {
            let mut header = Frame::new(Kind::Request, 1, b"").unwrap().header();
            if compressed {
                header[6] = 1;
            } else {
                header[16..].copy_from_slice(&u32::MAX.to_be_bytes());
            }
            let mut decoder = Decoder::default();
            assert!(decoder.push(&header, Tick::seconds(0)).is_err());
            assert!(decoder.pending.is_none());
            assert!(decoder.push(&[], Tick::seconds(1)).is_err());
        }
    }

    #[test]
    fn trickling_progress_does_not_extend_frame_lifetime() {
        let header = Frame::new(Kind::Request, 1, b"example").unwrap().header();
        let mut decoder = Decoder::default();
        for second in 0..5 {
            assert!(decoder
                .push(&header[second..second + 1], Tick::seconds(second as u64))
                .is_ok());
        }
        assert!(matches!(
            decoder.push(&header[5..], Tick::seconds(5)),
            Err(Error::Timeout)
        ));
    }

    #[test]
    fn incomplete_reordered_or_invalid_utf8_input_is_never_released() {
        let budgets = Budgets::new().unwrap();
        let route = budgets.open_route(Id::from_bytes([1; 16])).unwrap();
        let mut input = Assembly::input(4, &route).unwrap();
        assert_eq!(input.append(1, b"x"), Err(Error::Protocol));
        input.append(0, b"x").unwrap();
        assert!(input.finish().is_err());
        let mut input = Assembly::input(1, &route).unwrap();
        input.append(0, &[0xff]).unwrap();
        assert!(input.finish().is_err());
        assert_eq!(budgets.retained().unwrap(), (0, 0));
    }

    #[test]
    fn unicode_can_cross_block_boundaries_and_debug_is_redacted() {
        let budgets = Budgets::new().unwrap();
        let route = budgets.open_route(Id::from_bytes([1; 16])).unwrap();
        let text = "example-\u{1f980}";
        let mut input = Assembly::input(text.len(), &route).unwrap();
        input.append(0, &text.as_bytes()[..10]).unwrap();
        input.append(10, &text.as_bytes()[10..]).unwrap();
        let payload = input.finish().unwrap();
        assert_eq!(payload.text(), text);
        assert!(!format!("{payload:?}").contains("example"));
        let frame = Frame::new(Kind::InputBlock, 1, text.as_bytes()).unwrap();
        assert!(!format!("{frame:?}").contains("example"));
    }
}
