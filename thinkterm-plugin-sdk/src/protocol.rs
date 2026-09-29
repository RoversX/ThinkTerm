//! The plugin protocol: what ThinkTerm and a plugin's program say to each
//! other over the program's standard input and output. Each message is one
//! line of JSON. docs/thinkterm/plugins.md describes it for plugins written
//! in any language.

use serde::de::Error as _;
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use serde_json::Value;
use std::borrow::Cow;
use std::io::{self, BufRead, Read, Write};
use thinkterm_plugin_channel::wire::Raw;
use thinkterm_plugin_panel::{Answer, Env, Input, RowsWanted};

/// The version of this protocol: a manifest's `api`, which the program's
/// `ready` repeats. 2 added panels; a program speaking 1 still runs, and
/// has none.
pub const API: u32 = 2;
/// The first version a panel is drawn in.
pub const PANEL_API: u32 = 2;

/// The longest line either side sends or accepts, so that neither can make
/// the other hold an input without bound. The same as a client's frame.
pub const MAX_LINE: usize = thinkterm_plugin_channel::wire::MAX_FRAME;

/// What ThinkTerm sends a plugin.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToPlugin {
    /// A client's call. Answered by an `ok` or an `error` with the same id.
    Call {
        id: u64,
        #[serde(default)]
        body: Value,
    },
    /// A panel of the plugin came on show, as `view`: to be drawn with a
    /// `frame`. With `extends`, it is the extended view of that panel,
    /// which asked for one; it is closed before the panel is.
    Open {
        view: u64,
        env: Env,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        extends: Option<u64>,
    },
    /// Its size, fonts or theme changed.
    Env { view: u64, env: Env },
    /// The user did something in it.
    Input { view: u64, input: Input },
    /// Rows of one of its lists: answered with `rows`.
    Rows { view: u64, wanted: RowsWanted },
    /// It went off show.
    Close { view: u64 },
    /// What came of the plugin's `ask` `id` for panel `view`.
    Answer { view: u64, id: u64, answer: Answer },
    /// Exit now. Standard input closes after it.
    Stop,
}

/// What a plugin sends ThinkTerm. A panel's frames and rows are carried
/// as they were written, unread, for ThinkTerm passes them on to the
/// client showing the panel: read a line with [`FromPlugin::read`].
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FromPlugin {
    /// The first line: the program is ready, and speaks this protocol.
    Ready {
        api: u32,
    },
    Ok {
        id: u64,
        #[serde(default)]
        body: Value,
        /// The caller is sent the plugin's events from now on.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        watch: bool,
    },
    Error {
        id: u64,
        message: String,
    },
    /// For every client watching the plugin.
    Event {
        body: Value,
    },
    /// Panel `view` drawn anew: a `Frame`.
    Frame {
        view: u64,
        frame: Raw,
    },
    /// Rows panel `view` asked for: a `Rows`.
    Rows {
        view: u64,
        rows: Raw,
    },
    /// Something for ThinkTerm to do on `machine`, the one the terminal
    /// beside panel `view` runs on (`Env::remote`): an `Ask`, answered
    /// with an `answer` of the same `id`, which the plugin picks. Done only
    /// while the terminal is still on that machine.
    Ask {
        view: u64,
        id: u64,
        machine: String,
        ask: Raw,
    },
}

/// The kinds of message a plugin sends, for the error naming them.
const KINDS: &[&str] = &["ready", "ok", "error", "event", "frame", "rows", "ask"];

impl FromPlugin {
    /// Reads one line from a plugin, going over it once: a panel's frame,
    /// rows or ask is kept as it was written, for ThinkTerm passes those on
    /// unread, and a payload kept unread cannot be read through a tag.
    pub fn read(line: &[u8]) -> serde_json::Result<Self> {
        #[derive(Deserialize)]
        struct Line<'a> {
            #[serde(rename = "type", borrow)]
            kind: Cow<'a, str>,
            api: Option<u32>,
            id: Option<u64>,
            body: Option<Value>,
            #[serde(default)]
            watch: bool,
            message: Option<String>,
            view: Option<u64>,
            #[serde(borrow)]
            machine: Option<Cow<'a, str>>,
            #[serde(borrow)]
            frame: Option<&'a RawValue>,
            #[serde(borrow)]
            rows: Option<&'a RawValue>,
            #[serde(borrow)]
            ask: Option<&'a RawValue>,
        }
        fn given<T>(field: Option<T>, name: &'static str) -> serde_json::Result<T> {
            field.ok_or_else(|| serde_json::Error::missing_field(name))
        }
        let line: Line = serde_json::from_slice(line)?;
        Ok(match &*line.kind {
            "ready" => Self::Ready {
                api: given(line.api, "api")?,
            },
            "ok" => Self::Ok {
                id: given(line.id, "id")?,
                body: line.body.unwrap_or_default(),
                watch: line.watch,
            },
            "error" => Self::Error {
                id: given(line.id, "id")?,
                message: given(line.message, "message")?,
            },
            "event" => Self::Event {
                body: line.body.unwrap_or_default(),
            },
            "frame" => Self::Frame {
                view: given(line.view, "view")?,
                frame: Raw::copy_of(given(line.frame, "frame")?),
            },
            "rows" => Self::Rows {
                view: given(line.view, "view")?,
                rows: Raw::copy_of(given(line.rows, "rows")?),
            },
            "ask" => Self::Ask {
                view: given(line.view, "view")?,
                id: given(line.id, "id")?,
                machine: given(line.machine, "machine")?.into_owned(),
                ask: Raw::copy_of(given(line.ask, "ask")?),
            },
            other => return Err(serde_json::Error::unknown_variant(other, KINDS)),
        })
    }
}

/// Writes `message` as one line, in a single write, so lines written from
/// two threads never mix.
pub fn write_message(to: &mut impl Write, message: &impl Serialize) -> io::Result<()> {
    let mut line = serde_json::to_vec(message)?;
    line.push(b'\n');
    to.write_all(&line)?;
    to.flush()
}

/// Reads one line into `line`, without its ending; false at the end of the
/// input. A line longer than [`MAX_LINE`] is an error: what follows it
/// cannot be told apart from it.
pub fn read_line(from: &mut impl BufRead, line: &mut Vec<u8>) -> io::Result<bool> {
    line.clear();
    let read = Read::take(&mut *from, MAX_LINE as u64 + 1).read_until(b'\n', line)?;
    if read == 0 {
        return Ok(false);
    }
    if line.last() == Some(&b'\n') {
        line.pop();
        if line.last() == Some(&b'\r') {
            line.pop();
        }
    } else if line.len() > MAX_LINE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("a plugin message is limited to {MAX_LINE} bytes"),
        ));
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, to_value};

    fn read(value: Value) -> serde_json::Result<FromPlugin> {
        FromPlugin::read(&serde_json::to_vec(&value).unwrap())
    }

    #[test]
    fn messages_are_json_tagged_by_type() {
        assert_eq!(
            to_value(FromPlugin::Ready { api: API }).unwrap(),
            json!({"type": "ready", "api": 2})
        );
        assert_eq!(
            to_value(FromPlugin::Ok {
                id: 1,
                body: json!("hi"),
                watch: false
            })
            .unwrap(),
            json!({"type": "ok", "id": 1, "body": "hi"})
        );
        let ok = read(json!({"type": "ok", "id": 2})).unwrap();
        assert_eq!(
            ok,
            FromPlugin::Ok {
                id: 2,
                body: Value::Null,
                watch: false
            }
        );
        let call: ToPlugin =
            serde_json::from_value(json!({"type": "call", "id": 3, "body": {"op": "uuid"}}))
                .unwrap();
        assert_eq!(
            call,
            ToPlugin::Call {
                id: 3,
                body: json!({"op": "uuid"})
            }
        );
        assert_eq!(to_value(ToPlugin::Stop).unwrap(), json!({"type": "stop"}));
        // What a plugin can no longer send is not a message.
        assert!(read(json!({"type": "notify", "title": "x"})).is_err());
    }

    #[test]
    fn a_frame_is_read_without_reading_what_it_draws() {
        let line = br#"{"type":"frame","view":4,"frame":{"items":[{"rect": {"x":0}}]}}"#;
        let FromPlugin::Frame { view, frame } = FromPlugin::read(line).unwrap() else {
            panic!()
        };
        assert_eq!(view, 4);
        assert_eq!(frame.get(), r#"{"items":[{"rect": {"x":0}}]}"#);
        let mut wire = Vec::new();
        write_message(&mut wire, &FromPlugin::Frame { view, frame }).unwrap();
        assert_eq!(wire, [&line[..], b"\n"].concat(), "written as it came");

        let rows = FromPlugin::read(br#"{"view":1,"rows":[],"type":"rows"}"#).unwrap();
        assert!(matches!(rows, FromPlugin::Rows { view: 1, .. }), "{rows:?}");
        assert!(FromPlugin::read(br#"{"type":"frame","view":1}"#).is_err());
        assert!(
            FromPlugin::read(br#"{"type":"error","id":1}"#).is_err(),
            "no message"
        );

        let asked = br#"{"type":"ask","view":2,"id":5,"machine":"m1","ask":{"op":"read","path":"/a b","limit":9}}"#;
        let FromPlugin::Ask {
            view,
            id,
            machine,
            ask,
        } = FromPlugin::read(asked).unwrap()
        else {
            panic!()
        };
        assert_eq!((view, id, machine.as_str()), (2, 5, "m1"));
        assert_eq!(
            ask.get(),
            r#"{"op":"read","path":"/a b","limit":9}"#,
            "unread"
        );
        let mut wire = Vec::new();
        write_message(
            &mut wire,
            &FromPlugin::Ask {
                view,
                id,
                machine,
                ask,
            },
        )
        .unwrap();
        assert_eq!(wire, [&asked[..], b"\n"].concat());
        assert!(
            FromPlugin::read(br#"{"type":"ask","view":2,"id":5,"ask":{"op":"stat","path":"/"}}"#)
                .is_err(),
            "an ask names the machine it is for"
        );

        let answer: ToPlugin = serde_json::from_value(json!({
            "type": "answer", "view": 2, "id": 5,
            "answer": {"result": "read", "bytes": "aGk=", "cut": false}
        }))
        .unwrap();
        let ToPlugin::Answer {
            answer: thinkterm_plugin_panel::Answer::Read { bytes, cut: false },
            ..
        } = answer
        else {
            panic!("{answer:?}")
        };
        assert_eq!(bytes.0, b"hi");

        let open: ToPlugin = serde_json::from_value(json!({
            "type": "open",
            "view": 2,
            "env": {
                "width": 300, "height": 500, "scale": 2, "dark": true,
                "small": {"size": 11, "line": 15},
                "body": {"size": 13, "line": 18},
                "title": {"size": 15, "line": 20},
                "mono": {"size": 12, "line": 17, "advance": 7.2}
            }
        }))
        .unwrap();
        let ToPlugin::Open {
            view: 2,
            env,
            extends: None,
        } = open
        else {
            panic!("{open:?}")
        };
        assert_eq!(env.mono.advance, 7.2);
        let extended = ToPlugin::Open {
            view: 3,
            env,
            extends: Some(2),
        };
        assert_eq!(to_value(&extended).unwrap()["extends"], 2);
    }

    #[test]
    fn lines_round_trip_and_refuse_what_is_too_long() {
        let mut wire = Vec::new();
        write_message(&mut wire, &FromPlugin::Ready { api: API }).unwrap();
        wire.extend_from_slice(b"{\"type\":\"stop\"}\r\n");
        wire.extend_from_slice(b"last");
        let mut from = wire.as_slice();
        let mut line = Vec::new();
        assert!(read_line(&mut from, &mut line).unwrap());
        assert_eq!(line, br#"{"type":"ready","api":2}"#);
        assert!(read_line(&mut from, &mut line).unwrap());
        assert_eq!(line, br#"{"type":"stop"}"#);
        assert!(read_line(&mut from, &mut line).unwrap());
        assert_eq!(line, b"last", "a last line may lack its ending");
        assert!(!read_line(&mut from, &mut line).unwrap());

        let huge = vec![b'x'; MAX_LINE + 1];
        let err = read_line(&mut huge.as_slice(), &mut line).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        let mut fits = vec![b'x'; MAX_LINE];
        fits.push(b'\n');
        assert!(read_line(&mut fits.as_slice(), &mut line).unwrap());
        assert_eq!(line.len(), MAX_LINE);
    }
}
