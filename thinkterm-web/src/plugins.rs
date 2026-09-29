//! The page's side of the plugin channel: calls to the plugin host on the
//! server's machine, carried there and back by the server (`PluginFrame`).
//! A call is answered by the frame naming its id; any other frame is an
//! event, handed on; the host going away fails every call still waiting.

use futures::channel::oneshot;
use serde_json::Value;
use std::collections::HashMap;
use thinkterm_plugin_channel::wire::{FromHost, PanelEvent, ToHost};

/// A call's answer, or why there is none.
pub type Answer = Result<Value, String>;

/// A frame from the host that is not an answer.
#[derive(Debug, PartialEq)]
pub enum Heard {
    /// A plugin's event: its name, and what it said.
    Event(String, Value),
    /// About the page's panel `.0`.
    Panel(u64, PanelEvent),
}

#[derive(Debug, Default)]
pub struct PluginCalls {
    next_id: u64,
    waiting: HashMap<u64, oneshot::Sender<Answer>>,
}

impl PluginCalls {
    /// A call's frame to send, its id, and its answer to wait for.
    pub fn call(&mut self, plugin: &str, body: Value) -> (u64, Vec<u8>, oneshot::Receiver<Answer>) {
        self.next_id += 1;
        let id = self.next_id;
        let frame = ToHost::Call { id, plugin: plugin.to_string(), body }.encode();
        let (tx, rx) = oneshot::channel();
        self.waiting.insert(id, tx);
        (id, frame, rx)
    }

    /// A frame from the host: the answer to a call, delivered, or what
    /// else it is, handed back.
    pub fn heard(&mut self, frame: &[u8]) -> Option<Heard> {
        match FromHost::decode(frame) {
            Ok(FromHost::Ok { id, body }) => self.answer(id, Ok(body)),
            Ok(FromHost::Error { id, message }) => self.answer(id, Err(message)),
            Ok(FromHost::Event { plugin, body }) => return Some(Heard::Event(plugin, body)),
            Ok(FromHost::Panel { view, event }) => return Some(Heard::Panel(view, event)),
            Ok(FromHost::Hello { .. }) => {}
            Err(err) => log::warn!("a frame from the plugin host that does not read: {err}"),
        }
        None
    }

    fn answer(&mut self, id: u64, answer: Answer) {
        if let Some(waiting) = self.waiting.remove(&id) {
            let _ = waiting.send(answer);
        }
    }

    /// The call `id` never reached the host.
    pub fn fail(&mut self, id: u64, why: &str) {
        self.answer(id, Err(why.to_string()));
    }

    /// Whether any call is waiting for its answer: the way to the host is
    /// kept open until none is.
    pub fn waiting(&self) -> bool {
        !self.waiting.is_empty()
    }

    /// The way to the host is gone: nothing still waiting will be answered.
    pub fn lost(&mut self, why: &str) {
        for (_, waiting) in self.waiting.drain() {
            let _ = waiting.send(Err(why.to_string()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn answered(rx: &mut oneshot::Receiver<Answer>) -> Option<Answer> {
        rx.try_recv().expect("not cancelled")
    }

    #[test]
    fn a_call_is_answered_by_its_id_and_an_event_is_handed_on() {
        let mut calls = PluginCalls::default();
        let (first, frame, mut first_rx) = calls.call("snippets", json!({"op": "list"}));
        assert_eq!(
            ToHost::decode(&frame).unwrap(),
            ToHost::Call { id: first, plugin: "snippets".into(), body: json!({"op": "list"}) }
        );
        let (second, _, mut second_rx) = calls.call("snippets", json!({"op": "get"}));
        assert_ne!(first, second);

        let reply = FromHost::Error { id: second, message: "no".into() }.encode();
        assert!(calls.heard(&reply).is_none());
        assert_eq!(answered(&mut second_rx), Some(Err("no".into())));
        assert_eq!(answered(&mut first_rx), None, "still waiting");

        let event = FromHost::Event { plugin: "snippets".into(), body: json!({"event": "changed"}) }.encode();
        assert_eq!(calls.heard(&event), Some(Heard::Event("snippets".into(), json!({"event": "changed"}))));

        let reply = FromHost::Ok { id: first, body: json!([]) }.encode();
        calls.heard(&reply);
        assert_eq!(answered(&mut first_rx), Some(Ok(json!([]))));
    }

    #[test]
    fn nothing_waits_for_a_host_that_went_away() {
        let mut calls = PluginCalls::default();
        let (_, _, mut a) = calls.call("snippets", json!(null));
        let (b_id, _, mut b) = calls.call("snippets", json!(null));
        calls.fail(b_id, "refused");
        assert_eq!(answered(&mut b), Some(Err("refused".into())));
        calls.lost("gone");
        assert_eq!(answered(&mut a), Some(Err("gone".into())));
    }
}
