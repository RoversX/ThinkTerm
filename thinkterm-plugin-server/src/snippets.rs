//! The snippets plugin: the one copy of the user's snippets, the file it is
//! kept in, and everything done with it. Clients show what it answers.
//!
//! It is written against thinkterm-plugin-sdk like any installed plugin,
//! and runs inside the host (or out of it, with `--serve-plugin snippets`).

use crate::stamp::{stamp, Stamp};
use anyhow::Context;
use serde_json::Value;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};
use thinkterm_plugin_sdk::{Cx, Plugin};
use thinkterm_snippets::wire::{Event, Request, Row, Saved, Snippet};
use thinkterm_snippets::{file, SnippetRecord, SnippetStore};

pub struct Snippets {
    path: PathBuf,
    /// Read by the first call.
    store: Option<SnippetStore>,
    /// The file as this host last read or wrote it, to notice it being
    /// replaced from outside -- by a restored backup, say.
    seen: Option<Stamp>,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

fn changed() -> Value {
    serde_json::to_value(Event::Changed).expect("an event always serialises")
}

fn row(snippet: &SnippetRecord) -> Row {
    Row {
        id: snippet.id.clone(),
        title: snippet.title.clone(),
        preview: thinkterm_snippets::preview(&snippet.body),
    }
}

impl Snippets {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            store: None,
            seen: None,
        }
    }

    /// The store as the file has it: read on first use and again whenever
    /// the file changed behind this host's back, which is a change for
    /// every client too. A file that does not read is an error, never an
    /// empty store: an empty one would be written over it by the next save.
    fn current(&mut self, cx: &mut Cx) -> anyhow::Result<&mut SnippetStore> {
        let now = stamp(&self.path);
        if self.store.is_none() || now != self.seen {
            let store = file::load(&self.path)?;
            if self.store.is_some() {
                log::info!("{} changed on disk; reloaded it", self.path.display());
                cx.emit(changed());
            }
            self.store = Some(store);
            self.seen = now;
        }
        Ok(self.store.as_mut().expect("just loaded"))
    }

    /// Keeps `next` once it is on disk, and tells the clients.
    fn commit(&mut self, next: SnippetStore, cx: &mut Cx) -> anyhow::Result<()> {
        file::save(&self.path, &next)?;
        self.store = Some(next);
        self.seen = stamp(&self.path);
        cx.emit(changed());
        Ok(())
    }
}

impl Plugin for Snippets {
    fn call(&mut self, body: Value, cx: &mut Cx) -> anyhow::Result<Value> {
        let request: Request =
            serde_json::from_value(body).context("not a request the snippets plugin knows")?;
        let answer = match request {
            Request::List { query } => {
                let store = self.current(cx)?;
                let rows: Vec<Row> = store.matching(&query).map(row).collect();
                cx.watch();
                serde_json::to_value(rows)?
            }
            Request::Get { id } => {
                let store = self.current(cx)?;
                let snippet = store.get(&id).map(|snippet| Snippet {
                    id: snippet.id.clone(),
                    title: snippet.title.clone(),
                    body: snippet.body.clone(),
                });
                serde_json::to_value(snippet)?
            }
            Request::Text { id, run } => {
                let store = self.current(cx)?;
                let text = store.get(&id).and_then(|snippet| {
                    if run {
                        thinkterm_snippets::run_text(&snippet.body)
                    } else {
                        Some(snippet.body.clone())
                    }
                });
                serde_json::to_value(text)?
            }
            Request::Save { id, title, body } => {
                let body = body.trim();
                if body.is_empty() {
                    return Ok(serde_json::to_value(Saved::Empty)?);
                }
                // Saved before it is kept, so a change that did not reach
                // the disk is neither kept nor announced.
                let mut next = self.current(cx)?.clone();
                let now = now_ms();
                let saved = match id {
                    None => {
                        let fresh = thinkterm_snippets::new_id(now);
                        next.create(fresh, &title, body.to_string(), now).id
                    }
                    Some(id) => match next.update(&id, &title, body.to_string(), now) {
                        Some(record) => record.id,
                        None => return Ok(serde_json::to_value(Saved::Gone)?),
                    },
                };
                self.commit(next, cx)?;
                serde_json::to_value(Saved::Saved { id: saved })?
            }
            Request::Delete { id } => {
                let mut next = self.current(cx)?.clone();
                let deleted = next.delete(&id, now_ms());
                if deleted {
                    self.commit(next, cx)?;
                }
                Value::Bool(deleted)
            }
        };
        Ok(answer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use thinkterm_plugin_sdk::Effects;

    fn ask(plugin: &mut Snippets, request: Request) -> (Value, Effects) {
        let mut cx = Cx::new();
        let answer = plugin
            .call(serde_json::to_value(request).unwrap(), &mut cx)
            .unwrap();
        (answer, cx.finish())
    }

    /// Calls the plugin with raw JSON.
    fn call(plugin: &mut Snippets, body: Value) -> anyhow::Result<(Value, Effects)> {
        let mut cx = Cx::new();
        let answer = plugin.call(body, &mut cx)?;
        Ok((answer, cx.finish()))
    }

    fn list(plugin: &mut Snippets, query: &str) -> Vec<Row> {
        let (answer, effects) = ask(
            plugin,
            Request::List {
                query: query.into(),
            },
        );
        assert!(effects.watch, "a list is followed");
        serde_json::from_value(answer).unwrap()
    }

    fn save(plugin: &mut Snippets, id: Option<&str>, title: &str, body: &str) -> Saved {
        let (answer, _) = ask(
            plugin,
            Request::Save {
                id: id.map(str::to_string),
                title: title.into(),
                body: body.into(),
            },
        );
        serde_json::from_value(answer).unwrap()
    }

    fn plugin_in(dir: &tempfile::TempDir) -> Snippets {
        Snippets::new(dir.path().join("snippets.json"))
    }

    #[test]
    fn a_save_is_on_disk_in_the_list_and_announced() {
        let dir = tempfile::tempdir().unwrap();
        let mut plugin = plugin_in(&dir);
        assert!(list(&mut plugin, "").is_empty());

        let (answer, effects) = call(
            &mut plugin,
            json!({"op": "save", "title": "", "body": "  git status  \n"}),
        )
        .unwrap();
        let Saved::Saved { id } = serde_json::from_value(answer).unwrap() else {
            panic!("not saved");
        };
        assert_eq!(effects.events, [changed()]);
        assert!(id.starts_with("snippet-"));
        let rows = list(&mut plugin, "");
        assert_eq!(
            rows,
            [Row {
                id: id.clone(),
                title: "git status".into(),
                preview: "git status".into()
            }]
        );
        let on_disk = file::load(&dir.path().join("snippets.json")).unwrap();
        assert_eq!(on_disk.get(&id).unwrap().body, "git status");
    }

    #[test]
    fn an_edit_a_delete_and_a_search() {
        let dir = tempfile::tempdir().unwrap();
        let mut plugin = plugin_in(&dir);
        let Saved::Saved { id } = save(&mut plugin, None, "Pull", "git pull --rebase") else {
            panic!("not saved");
        };
        save(&mut plugin, None, "", "ls -la");
        assert_eq!(list(&mut plugin, "PULL").len(), 1);
        assert_eq!(list(&mut plugin, "").len(), 2);

        assert_eq!(
            save(&mut plugin, Some(&id), "Fetch", "git fetch"),
            Saved::Saved { id: id.clone() }
        );
        let (answer, _) = ask(&mut plugin, Request::Get { id: id.clone() });
        let snippet: Snippet = serde_json::from_value(answer).unwrap();
        assert_eq!(
            (snippet.title.as_str(), snippet.body.as_str()),
            ("Fetch", "git fetch")
        );

        let (answer, effects) = ask(&mut plugin, Request::Delete { id: id.clone() });
        assert_eq!(answer, json!(true));
        assert_eq!(effects.events, [changed()]);
        let (answer, effects) = ask(&mut plugin, Request::Delete { id: id.clone() });
        assert_eq!(answer, json!(false));
        assert!(effects.events.is_empty(), "nothing changed");
        assert_eq!(save(&mut plugin, Some(&id), "", "git fetch"), Saved::Gone);
        let (answer, _) = ask(&mut plugin, Request::Get { id });
        assert_eq!(answer, Value::Null);
    }

    #[test]
    fn an_empty_script_is_not_saved() {
        let dir = tempfile::tempdir().unwrap();
        let mut plugin = plugin_in(&dir);
        assert_eq!(save(&mut plugin, None, "Title", " \n\t"), Saved::Empty);
        assert!(list(&mut plugin, "").is_empty());
        assert!(!dir.path().join("snippets.json").exists());
    }

    #[test]
    fn the_text_a_pane_is_sent() {
        let dir = tempfile::tempdir().unwrap();
        let mut plugin = plugin_in(&dir);
        let Saved::Saved { id } = save(&mut plugin, None, "", "one\ntwo") else {
            panic!("not saved");
        };
        let text = |plugin: &mut Snippets, run: bool| {
            ask(
                plugin,
                Request::Text {
                    id: id.clone(),
                    run,
                },
            )
            .0
        };
        assert_eq!(text(&mut plugin, false), json!("one\ntwo"));
        assert_eq!(text(&mut plugin, true), json!("one\rtwo\r"));
        let (answer, _) = ask(
            &mut plugin,
            Request::Text {
                id: "gone".into(),
                run: true,
            },
        );
        assert_eq!(answer, Value::Null);
    }

    #[test]
    fn a_file_replaced_from_outside_is_reread_and_announced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("snippets.json");
        let mut plugin = plugin_in(&dir);
        save(&mut plugin, None, "", "ls");

        let mut restored = SnippetStore::default();
        restored.create("b".into(), "", "pwd --physical".into(), 2);
        file::save(&path, &restored).unwrap();

        let (answer, effects) = call(&mut plugin, json!({"op": "list", "query": ""})).unwrap();
        let rows: Vec<Row> = serde_json::from_value(answer).unwrap();
        assert_eq!(
            rows.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            ["b"]
        );
        assert_eq!(effects.events, [changed()]);
    }

    #[test]
    fn a_file_that_does_not_read_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("snippets.json");
        std::fs::write(&path, "not json").unwrap();
        let mut plugin = plugin_in(&dir);
        assert!(call(&mut plugin, json!({"op": "list", "query": ""})).is_err());
        assert!(call(
            &mut plugin,
            json!({"op": "save", "title": "", "body": "ls"})
        )
        .is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "not json");
    }

    #[test]
    fn an_unknown_request_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let mut plugin = plugin_in(&dir);
        assert!(call(&mut plugin, json!({"op": "frobnicate"})).is_err());
    }
}
