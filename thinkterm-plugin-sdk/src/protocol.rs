//! The plugin protocol: what ThinkTerm and a plugin's program say to each
//! other over the program's standard input and output. Each message is one
//! line of JSON. docs/thinkterm/plugins.md describes it for plugins written
//! in any language.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::{self, BufRead, Read, Write};

/// The version of this protocol: a manifest's `api`, which the program's
/// `ready` repeats.
pub const API: u32 = 1;

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
    /// Exit now. Standard input closes after it.
    Stop,
}

/// What a plugin sends ThinkTerm.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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

    #[test]
    fn messages_are_json_tagged_by_type() {
        assert_eq!(
            to_value(FromPlugin::Ready { api: API }).unwrap(),
            json!({"type": "ready", "api": 1})
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
        let ok: FromPlugin = serde_json::from_value(json!({"type": "ok", "id": 2})).unwrap();
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
        assert!(
            serde_json::from_value::<FromPlugin>(json!({"type": "notify", "title": "x"})).is_err()
        );
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
        assert_eq!(line, br#"{"type":"ready","api":1}"#);
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
