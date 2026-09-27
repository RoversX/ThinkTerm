//! The messages, and the frames that carry them.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::{self, Read, Write};

/// The version of this format. A client that finds a host speaking an
/// older one asks it to quit and starts its own; one that finds a newer
/// host leaves it alone.
pub const PROTOCOL: u32 = 1;

/// The largest frame either side sends or accepts, so a confused peer
/// cannot have the other allocate without bound. A thousand scripts of
/// eight kilobytes each fit twice over.
pub const MAX_FRAME: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToHost {
    /// Ask `plugin` something. Answered by an [`FromHost::Ok`] or an
    /// [`FromHost::Error`] with the same `id`, which the client picks.
    Call {
        id: u64,
        plugin: String,
        body: Value,
    },
    /// Exit now: a newer build is replacing this host.
    Quit,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FromHost {
    /// The first message on every connection.
    Hello {
        protocol: u32,
    },
    Ok {
        id: u64,
        body: Value,
    },
    Error {
        id: u64,
        message: String,
    },
    /// What a plugin tells every connection watching it.
    Event {
        plugin: String,
        body: Value,
    },
}

impl ToHost {
    pub fn encode(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("a message of strings and JSON values always serialises")
    }

    pub fn decode(frame: &[u8]) -> serde_json::Result<Self> {
        serde_json::from_slice(frame)
    }
}

impl FromHost {
    pub fn encode(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("a message of strings and JSON values always serialises")
    }

    pub fn decode(frame: &[u8]) -> serde_json::Result<Self> {
        serde_json::from_slice(frame)
    }
}

/// The length a frame header announces, if it is one this side accepts.
pub fn frame_len(header: [u8; 4]) -> Option<usize> {
    let len = u32::from_le_bytes(header) as usize;
    (len <= MAX_FRAME).then_some(len)
}

/// The header for a frame of `len` bytes, if it is not too long to send.
pub fn frame_header(len: usize) -> Option<[u8; 4]> {
    (len <= MAX_FRAME).then(|| (len as u32).to_le_bytes())
}

fn too_long() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("a plugin frame is limited to {MAX_FRAME} bytes"),
    )
}

/// Writes `payload` as one frame, header and body in a single write so a
/// frame is never split between two writers sharing a socket.
pub fn write_frame(to: &mut impl Write, payload: &[u8]) -> io::Result<()> {
    let header = frame_header(payload.len()).ok_or_else(too_long)?;
    let mut frame = Vec::with_capacity(header.len() + payload.len());
    frame.extend_from_slice(&header);
    frame.extend_from_slice(payload);
    to.write_all(&frame)?;
    to.flush()
}

/// Reads one frame's body.
pub fn read_frame(from: &mut impl Read) -> io::Result<Vec<u8>> {
    let mut header = [0u8; 4];
    from.read_exact(&mut header)?;
    let len = frame_len(header).ok_or_else(too_long)?;
    let mut payload = vec![0u8; len];
    from.read_exact(&mut payload)?;
    Ok(payload)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn messages_are_tagged_json() {
        let call = ToHost::Call {
            id: 7,
            plugin: "snippets".into(),
            body: json!({"op": "watch"}),
        };
        assert_eq!(
            serde_json::to_value(&call).unwrap(),
            json!({"call": {"id": 7, "plugin": "snippets", "body": {"op": "watch"}}})
        );
        assert_eq!(ToHost::decode(&call.encode()).unwrap(), call);
        assert_eq!(serde_json::to_value(ToHost::Quit).unwrap(), json!("quit"));
        let hello = FromHost::Hello { protocol: PROTOCOL };
        assert_eq!(
            serde_json::to_value(&hello).unwrap(),
            json!({"hello": {"protocol": PROTOCOL}})
        );
        assert_eq!(FromHost::decode(&hello.encode()).unwrap(), hello);
    }

    #[test]
    fn frames_round_trip_and_refuse_what_is_too_long() {
        let mut wire = Vec::new();
        write_frame(&mut wire, b"one").unwrap();
        write_frame(&mut wire, b"").unwrap();
        let mut from = wire.as_slice();
        assert_eq!(read_frame(&mut from).unwrap(), b"one");
        assert_eq!(read_frame(&mut from).unwrap(), b"");
        assert_eq!(
            read_frame(&mut from).unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );

        let huge = ((MAX_FRAME + 1) as u32).to_le_bytes();
        assert_eq!(
            read_frame(&mut huge.as_slice()).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert!(frame_header(MAX_FRAME).is_some());
        assert!(frame_header(MAX_FRAME + 1).is_none());
    }
}
