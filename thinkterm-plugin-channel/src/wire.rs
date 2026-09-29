//! The messages, and the frames that carry them.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use serde_json::Value;
use std::fmt;
use std::io::{self, Read, Write};

/// The version of this format and of what the host answers. A client that
/// finds a host speaking an older one asks it to quit and starts its own;
/// one that finds a newer host leaves it alone. 2 added the host's own API
/// (`registry`) and installed plugins; 3 took commands and notifications
/// out of it; 4 added plugins' panels; 5 what a panel's plugin asks of
/// another machine, and how long plugins run unused.
pub const PROTOCOL: u32 = 5;

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
    /// About a panel the client shows, `view` being its own number for it.
    Panel { view: u64, request: PanelRequest },
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
    /// About the client's panel `view`.
    Panel {
        view: u64,
        event: PanelEvent,
    },
}

/// What a client asks of a plugin's panel it shows. What the panel's
/// messages say -- the `thinkterm-plugin-panel` types, as JSON -- is
/// between the client and the plugin: the host carries them unread.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PanelRequest {
    /// Show plugin `plugin`'s panel for an `Env`: its program is started
    /// if it is not running. An open under a number the client already
    /// uses replaces that panel. With `extends`, the client's number for a
    /// panel of the same plugin whose frame asked for it (`Frame::extend`),
    /// it is that panel's extended view instead, and closes with it.
    Open {
        plugin: String,
        env: Raw,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        extends: Option<u64>,
    },
    /// Its size, fonts or theme changed: an `Env`.
    Env { env: Raw },
    /// What the user did in it: an `Input`.
    Input { input: Raw },
    /// Rows of one of its lists: a `RowsWanted`.
    Rows { wanted: Raw },
    /// The client has taken in the last frame, and can take the next. A
    /// panel is sent one frame at a time: the newest is held back until
    /// the client has the one before, and any older one is dropped, so a
    /// plugin drawing faster than a client keeps up never queues frames.
    Shown,
    /// It went off show.
    Close,
    /// What came of what the plugin asked, [`PanelEvent::Remote`] `id`: an
    /// `Answer`.
    Answer { id: u64, answer: Raw },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PanelEvent {
    /// The plugin drew the panel: a `Frame`.
    Frame { frame: Raw },
    /// Rows the client asked for: a `Rows`.
    Rows { rows: Raw },
    /// The panel is no longer served: its plugin stopped, went off, or
    /// could not start. With `again`, opening it anew may work -- the
    /// plugin was restarted, or was on its way out -- and the client may
    /// try after a pause.
    Closed { reason: String, again: bool },
    /// The plugin asks something of `machine`, the one the client told it
    /// the terminal beside the panel runs on (`Env::remote`): an `Ask`,
    /// answered with [`PanelRequest::Answer`] and the same `id`. The client
    /// does it only while the terminal is still on that machine.
    Remote { id: u64, machine: String, ask: Raw },
}

/// JSON passed along unread: parsed only far enough to know where it ends,
/// and written out as it came.
#[derive(Clone, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Raw(Box<RawValue>);

impl Raw {
    pub fn new<T: Serialize + ?Sized>(value: &T) -> Self {
        Self(serde_json::value::to_raw_value(value).expect("a panel message always serialises"))
    }

    /// A copy of JSON borrowed from what it was read out of.
    pub fn copy_of(raw: &RawValue) -> Self {
        Self(raw.to_owned())
    }

    /// What it says, read as a `T`.
    pub fn read<T: DeserializeOwned>(&self) -> serde_json::Result<T> {
        serde_json::from_str(self.0.get())
    }

    pub fn get(&self) -> &str {
        self.0.get()
    }
}

impl PartialEq for Raw {
    fn eq(&self, other: &Self) -> bool {
        self.get() == other.get()
    }
}

impl fmt::Debug for Raw {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.get())
    }
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
    fn a_panel_message_is_carried_as_it_was_written() {
        let frame =
            br#"{"panel":{"view":3,"event":{"frame":{"frame":{"items":[ {"rect":{"x":1}} ]}}}}}"#;
        let decoded = FromHost::decode(frame).unwrap();
        let FromHost::Panel {
            view: 3,
            event: PanelEvent::Frame { frame: raw },
        } = &decoded
        else {
            panic!("{decoded:?}")
        };
        assert_eq!(
            raw.get(),
            r#"{"items":[ {"rect":{"x":1}} ]}"#,
            "unread, spaces and all"
        );
        assert_eq!(
            raw.read::<Value>().unwrap(),
            json!({"items": [{"rect": {"x": 1}}]})
        );
        assert_eq!(FromHost::decode(&decoded.encode()).unwrap(), decoded);

        let open = ToHost::Panel {
            view: 1,
            request: PanelRequest::Open {
                plugin: "stocks".into(),
                env: Raw::new(&json!({"width": 300})),
                extends: None,
            },
        };
        assert_eq!(
            serde_json::to_value(&open).unwrap(),
            json!({"panel": {"view": 1, "request": {"open": {"plugin": "stocks", "env": {"width": 300}}}}})
        );
        let extended = ToHost::decode(
            br#"{"panel": {"view": 2, "request": {"open": {"plugin": "diff", "env": {}, "extends": 1}}}}"#,
        )
        .unwrap();
        assert!(
            matches!(
                extended,
                ToHost::Panel {
                    view: 2,
                    request: PanelRequest::Open {
                        extends: Some(1),
                        ..
                    }
                }
            ),
            "{extended:?}"
        );
        let shown = ToHost::Panel {
            view: 1,
            request: PanelRequest::Shown,
        };
        assert_eq!(
            serde_json::to_value(&shown).unwrap(),
            json!({"panel": {"view": 1, "request": "shown"}})
        );
        assert_eq!(ToHost::decode(&shown.encode()).unwrap(), shown);

        let asked = FromHost::decode(
            br#"{"panel": {"view": 4, "event": {"remote": {"id": 9, "machine": "m1", "ask": {"op": "stat", "path": "/x"}}}}}"#,
        )
        .unwrap();
        let FromHost::Panel {
            view: 4,
            event:
                PanelEvent::Remote {
                    id: 9,
                    machine,
                    ask,
                },
        } = &asked
        else {
            panic!("{asked:?}")
        };
        assert_eq!(machine, "m1");
        assert_eq!(ask.get(), r#"{"op": "stat", "path": "/x"}"#);
        let answer = ToHost::Panel {
            view: 4,
            request: PanelRequest::Answer {
                id: 9,
                answer: Raw::new(&json!({"result": "failed", "why": "no"})),
            },
        };
        assert_eq!(
            serde_json::to_value(&answer).unwrap(),
            json!({"panel": {"view": 4, "request": {"answer": {"id": 9, "answer": {"result": "failed", "why": "no"}}}}})
        );
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
