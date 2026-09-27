//! Writing a ThinkTerm plugin.
//!
//! A plugin is a program that ThinkTerm starts when the plugin is first
//! used, and talks to over the program's standard input and output (see
//! [`protocol`], and docs/thinkterm/plugins.md for the manifest that goes
//! with it). Implement [`Plugin`] and hand it to [`run`]:
//!
//! ```no_run
//! use serde_json::{json, Value};
//! use thinkterm_plugin_sdk::{Cx, Plugin};
//!
//! struct Hello;
//!
//! impl Plugin for Hello {
//!     fn call(&mut self, body: Value, _cx: &mut Cx) -> anyhow::Result<Value> {
//!         match body["op"].as_str() {
//!             Some("greet") => Ok(json!("hello")),
//!             _ => anyhow::bail!("no such call"),
//!         }
//!     }
//! }
//!
//! fn main() -> std::io::Result<()> {
//!     thinkterm_plugin_sdk::run(Hello)
//! }
//! ```
//!
//! The plugins built into ThinkTerm implement the same trait, and run
//! inside the plugin host instead of in a program of their own.

pub mod protocol;

use anyhow::anyhow;
use protocol::{FromPlugin, ToPlugin};
use serde_json::Value;
use std::io::{self, BufRead};
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;

pub trait Plugin: Send {
    /// Answers a client's call: any JSON, as the plugin defines it. The
    /// default takes no calls.
    fn call(&mut self, body: Value, cx: &mut Cx) -> anyhow::Result<Value> {
        let _ = (body, cx);
        anyhow::bail!("this plugin takes no calls")
    }
}

/// A plugin chosen at run time, among several.
impl<P: Plugin + ?Sized> Plugin for Box<P> {
    fn call(&mut self, body: Value, cx: &mut Cx) -> anyhow::Result<Value> {
        (**self).call(body, cx)
    }
}

/// What a call does besides answering, all of it sent once the answer is.
#[derive(Debug, Default)]
pub struct Cx {
    watch: bool,
    events: Vec<Value>,
}

impl Cx {
    pub fn new() -> Self {
        Self::default()
    }

    /// The caller is sent this plugin's events from now on: a client that
    /// listed something, to hear when it changes.
    pub fn watch(&mut self) {
        self.watch = true;
    }

    /// Sends `event` to every client watching this plugin, the caller
    /// included.
    pub fn emit(&mut self, event: Value) {
        self.events.push(event);
    }

    /// What was asked for, for whatever runs the plugin to carry out.
    pub fn finish(self) -> Effects {
        Effects {
            watch: self.watch,
            events: self.events,
        }
    }
}

#[derive(Debug, Default, PartialEq)]
pub struct Effects {
    pub watch: bool,
    pub events: Vec<Value>,
}

/// Sends events from outside a call: from a thread the plugin keeps to
/// watch something, say.
#[derive(Debug, Clone)]
pub struct Emitter {
    _private: (),
}

impl Emitter {
    pub fn emit(&self, event: Value) -> io::Result<()> {
        send(&FromPlugin::Event { body: event })
    }
}

fn send(message: &FromPlugin) -> io::Result<()> {
    protocol::write_message(&mut io::stdout().lock(), message)
}

/// The directory ThinkTerm made for the plugin to keep its files in.
pub fn data_dir() -> Option<PathBuf> {
    std::env::var_os("THINKTERM_PLUGIN_DATA").map(PathBuf::from)
}

/// Serves `plugin` over standard input and output until ThinkTerm asks it
/// to stop or closes its input, which is when the program is to exit.
/// Standard output belongs to the protocol: print with `eprintln!`, which
/// goes to ThinkTerm's plugin log.
pub fn run(plugin: impl Plugin) -> io::Result<()> {
    run_with(|_| plugin)
}

/// [`run`], for a plugin that sends events of its own accord: `make` is
/// handed what sends them.
pub fn run_with<P: Plugin>(make: impl FnOnce(Emitter) -> P) -> io::Result<()> {
    let mut plugin = make(Emitter { _private: () });
    send(&FromPlugin::Ready { api: protocol::API })?;
    serve(&mut plugin, &mut io::stdin().lock(), &mut |message| {
        send(message)
    })
}

/// Answers what `input` asks until it ends or says stop.
fn serve<P: Plugin>(
    plugin: &mut P,
    input: &mut impl BufRead,
    out: &mut impl FnMut(&FromPlugin) -> io::Result<()>,
) -> io::Result<()> {
    let mut line = Vec::new();
    while protocol::read_line(input, &mut line)? {
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let message: ToPlugin = match serde_json::from_slice(&line) {
            Ok(message) => message,
            Err(err) => {
                // One from a newer ThinkTerm, say. Its caller is told when
                // it has one; anything else is let be.
                let id = serde_json::from_slice::<Value>(&line)
                    .ok()
                    .and_then(|message| message.get("id")?.as_u64());
                if let Some(id) = id {
                    out(&FromPlugin::Error {
                        id,
                        message: format!("not a message this plugin knows: {err}"),
                    })?;
                }
                continue;
            }
        };
        match message {
            ToPlugin::Call { id, body } => {
                let mut cx = Cx::new();
                let answer = guard(|| plugin.call(body, &mut cx));
                answer_with(out, id, answer, cx)?;
            }
            ToPlugin::Stop => break,
        }
    }
    Ok(())
}

/// A plugin that panics answers with an error and goes on serving; the
/// panic itself is on standard error, in the log.
fn guard<T>(work: impl FnOnce() -> anyhow::Result<T>) -> anyhow::Result<T> {
    match std::panic::catch_unwind(AssertUnwindSafe(work)) {
        Ok(result) => result,
        Err(panic) => {
            let why = panic
                .downcast_ref::<&str>()
                .map(|why| why.to_string())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_default();
            Err(anyhow!("the plugin panicked: {why}"))
        }
    }
}

fn answer_with(
    out: &mut impl FnMut(&FromPlugin) -> io::Result<()>,
    id: u64,
    answer: anyhow::Result<Value>,
    cx: Cx,
) -> io::Result<()> {
    let effects = cx.finish();
    match answer {
        Ok(body) => out(&FromPlugin::Ok {
            id,
            body,
            watch: effects.watch,
        })?,
        Err(err) => out(&FromPlugin::Error {
            id,
            message: format!("{err:#}"),
        })?,
    }
    for body in effects.events {
        out(&FromPlugin::Event { body })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Counts calls, and lists as a watcher.
    #[derive(Default)]
    struct Counter {
        calls: u64,
    }

    impl Plugin for Counter {
        fn call(&mut self, body: Value, cx: &mut Cx) -> anyhow::Result<Value> {
            match body["op"].as_str() {
                Some("add") => {
                    self.calls += 1;
                    cx.emit(json!({"event": "changed"}));
                    Ok(json!(self.calls))
                }
                Some("get") => {
                    cx.watch();
                    Ok(json!(self.calls))
                }
                Some("boom") => panic!("boom"),
                _ => anyhow::bail!("unknown op"),
            }
        }
    }

    fn serve_lines(input: &str) -> Vec<Value> {
        let mut plugin = Counter::default();
        let mut sent = Vec::new();
        serve(&mut plugin, &mut input.as_bytes(), &mut |message| {
            sent.push(serde_json::to_value(message).unwrap());
            Ok(())
        })
        .unwrap();
        sent
    }

    #[test]
    fn calls_are_answered_by_id_with_their_events_after() {
        let sent = serve_lines(concat!(
            "{\"type\":\"call\",\"id\":1,\"body\":{\"op\":\"get\"}}\n",
            "\n",
            "{\"type\":\"call\",\"id\":2,\"body\":{\"op\":\"add\"}}\n",
            "{\"type\":\"call\",\"id\":3,\"body\":{\"op\":\"nope\"}}\n",
        ));
        assert_eq!(
            sent,
            [
                json!({"type": "ok", "id": 1, "body": 0, "watch": true}),
                json!({"type": "ok", "id": 2, "body": 1}),
                json!({"type": "event", "body": {"event": "changed"}}),
                json!({"type": "error", "id": 3, "message": "unknown op"}),
            ]
        );
    }

    #[test]
    fn it_stops_when_told_and_outlives_a_panic_and_the_unknown() {
        let sent = serve_lines(concat!(
            "{\"type\":\"call\",\"id\":1,\"body\":{\"op\":\"boom\"}}\n",
            "{\"type\":\"command\",\"id\":2,\"command\":\"decode\"}\n",
            "{\"type\":\"frobnicate\"}\n",
            "not json\n",
            "{\"type\":\"stop\"}\n",
            "{\"type\":\"call\",\"id\":3,\"body\":{\"op\":\"get\"}}\n",
        ));
        assert_eq!(sent.len(), 2, "{sent:?}");
        assert_eq!(sent[0]["id"], 1);
        assert!(sent[0]["message"]
            .as_str()
            .unwrap()
            .contains("panicked: boom"));
        assert_eq!(sent[1]["type"], "error");
        assert_eq!(sent[1]["id"], 2);
    }
}
