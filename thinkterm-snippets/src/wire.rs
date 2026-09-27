//! The snippets plugin's messages: JSON, carried in the plugin channel as a
//! call's body, its answer and the events a client is sent.
//!
//! The plugin keeps the snippets and does everything with them: searching,
//! saving, deleting, turning one into what a pane is sent. A client shows
//! what it is given and says what the user did; it keeps no copy of its own.

use crate::SnippetId;
use serde::{Deserialize, Serialize};

/// The name calls address and events carry.
pub const PLUGIN: &str = "snippets";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    /// The snippets matching `query`, as a list shows them, newest first.
    /// Answered with `Vec<Row>`. The caller is told of every change after
    /// it, and asks again.
    List { query: String },
    /// One snippet whole, for an editor. Answered with a `Snippet`, or
    /// `null` when there is no such snippet.
    Get { id: SnippetId },
    /// What a pane is sent for a snippet: its body as a paste, or with
    /// `run` typed and run line by line. Answered with a string, or `null`
    /// when there is no such snippet or nothing in it to run.
    Text { id: SnippetId, run: bool },
    /// Save what an editor holds, a new snippet when `id` is absent.
    /// Answered with `Saved`.
    Save {
        #[serde(default)]
        id: Option<SnippetId>,
        title: String,
        body: String,
    },
    /// Answered with whether there was such a snippet to delete.
    Delete { id: SnippetId },
}

/// One snippet as a list shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Row {
    pub id: SnippetId,
    pub title: String,
    /// The first line with text in it, shortened.
    pub preview: String,
}

/// One snippet whole, for an editor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snippet {
    pub id: SnippetId,
    pub title: String,
    pub body: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum Saved {
    /// Saved, under this id: a new snippet's, just given.
    Saved { id: SnippetId },
    /// Nothing was saved: the script is empty.
    Empty,
    /// Nothing was saved: the snippet being edited was deleted meanwhile.
    Gone,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    /// The snippets changed, here or from outside: a list on show is to be
    /// asked for again.
    Changed,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, to_value};

    #[test]
    fn requests_answers_and_events_are_tagged_json() {
        assert_eq!(
            to_value(Request::List {
                query: "git".into()
            })
            .unwrap(),
            json!({"op": "list", "query": "git"})
        );
        // A new snippet's save names no id.
        let save: Request =
            serde_json::from_value(json!({"op": "save", "title": "", "body": "ls"})).unwrap();
        assert_eq!(
            save,
            Request::Save {
                id: None,
                title: String::new(),
                body: "ls".into()
            }
        );
        assert_eq!(
            to_value(Saved::Saved { id: "a".into() }).unwrap(),
            json!({"outcome": "saved", "id": "a"})
        );
        assert_eq!(to_value(Saved::Empty).unwrap(), json!({"outcome": "empty"}));
        assert_eq!(
            to_value(Event::Changed).unwrap(),
            json!({"event": "changed"})
        );
    }
}
