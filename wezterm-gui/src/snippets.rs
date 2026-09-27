//! ThinkTerm command snippets shown in the right sidebar.
//!
//! The snippets live in the Snippets plugin, in the plugin host (see
//! `crate::plugins`), which keeps them and does everything with them:
//! searching, saving, deleting, making the text a pane is sent (see
//! thinkterm-snippets `wire`). A window's panel is only a view of it: each
//! window keeps its own search and the rows sent for it (thinkterm-snippets
//! `view`), and forwards what the user does. A browser's panel is another
//! view of the same.
//!
//! The windows that painted the panel are told when the snippets change,
//! and when the session with the host is let go, their rows with it; the
//! next paint asks again.

use crate::termwindow::{TermWindow, TermWindowNotif};
use anyhow::Result;
use serde::de::DeserializeOwned;
use std::path::{Path, PathBuf};
use thinkterm_snippets::wire::{Event, Request, PLUGIN};
use window::{Window, WindowOps};

pub use thinkterm_snippets::wire::{Row, Saved, Snippet};

/// What a window's panel has to show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Availability {
    Ready,
    /// The host has not answered yet.
    Loading,
    /// The host cannot be reached; why.
    Unavailable(String),
}

pub fn snippets_store_path() -> PathBuf {
    crate::native_paths::data_file("snippets.json")
}

/// Whether `path` reads as a snippet store, as an imported backup must.
pub(crate) fn check_snippet_store(path: &Path) -> Result<()> {
    thinkterm_snippets::file::load(path).map(drop)
}

/// `window` paints the panel: it is told when the snippets change, and the
/// session is kept.
pub fn in_use(window: &Window) {
    crate::plugins::window_in_use(window);
}

/// Why the host cannot be reached, while it cannot.
pub fn trouble() -> Option<String> {
    crate::plugins::trouble()
}

/// What a plugin other than the host itself told its watchers.
pub(crate) fn on_event(plugin: &str, body: serde_json::Value) {
    if plugin != PLUGIN {
        return;
    }
    if let Ok(Event::Changed) = serde_json::from_value(body) {
        crate::plugins::tell_windows(TermWindow::snippets_changed);
    }
}

fn to_body(request: &Request) -> serde_json::Value {
    serde_json::to_value(request).expect("a snippets request always serialises")
}

/// Asks the host something on behalf of `window`, and hands the answer, or
/// why there is none, to `apply` on that window alone.
fn ask<T: DeserializeOwned + Send + Sync + 'static>(
    request: Request,
    window: Window,
    apply: impl FnOnce(&mut TermWindow, std::result::Result<T, String>) + Send + Sync + 'static,
) {
    crate::plugins::call(PLUGIN, to_body(&request), move |answer| {
        let answer = answer.and_then(crate::plugins::from_body);
        promise::spawn::spawn_into_main_thread(async move {
            let repaint = window.clone();
            window.notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                apply(term_window, answer);
                repaint.invalidate();
            })));
        })
        .detach();
    });
}

/// The rows matching `query`, for `window`'s panel.
pub fn list(
    query: String,
    window: Window,
    apply: impl FnOnce(&mut TermWindow, String, std::result::Result<Vec<Row>, String>)
        + Send
        + Sync
        + 'static,
) {
    let request = Request::List {
        query: query.clone(),
    };
    ask(request, window, move |term_window, rows| {
        apply(term_window, query, rows)
    });
}

/// One snippet whole, for the editor.
pub fn open(
    id: &str,
    window: Window,
    apply: impl FnOnce(&mut TermWindow, Option<Snippet>) + Send + Sync + 'static,
) {
    let request = Request::Get { id: id.to_string() };
    ask(request, window, move |term_window, snippet| match snippet {
        Ok(snippet) => apply(term_window, snippet),
        Err(why) => log::error!("failed to open snippet: {why}"),
    });
}

/// What a pane is sent for a snippet: pasted, or with `run` typed and run.
pub fn text(
    id: &str,
    run: bool,
    window: Window,
    apply: impl FnOnce(&mut TermWindow, String) + Send + Sync + 'static,
) {
    let request = Request::Text {
        id: id.to_string(),
        run,
    };
    ask(request, window, move |term_window, text| match text {
        Ok(Some(text)) => apply(term_window, text),
        Ok(None) => {}
        Err(why) => log::error!("failed to fetch snippet text: {why}"),
    });
}

/// Saves what an editor holds, a new snippet when `id` is `None`. The
/// answer, or why there is none, goes to `apply`.
pub fn save(
    id: Option<String>,
    title: String,
    body: String,
    window: Window,
    apply: impl FnOnce(&mut TermWindow, std::result::Result<Saved, String>) + Send + Sync + 'static,
) {
    ask(Request::Save { id, title, body }, window, apply);
}

pub fn delete(id: &str) {
    let request = Request::Delete { id: id.to_string() };
    let id = id.to_string();
    crate::plugins::call(PLUGIN, to_body(&request), move |answer| {
        if let Err(why) = answer {
            log::error!("failed to delete snippet {id}: {why}");
        }
    });
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_host_keeps_snippets_where_the_backup_looks_for_them() {
        assert_eq!(
            thinkterm_plugin_channel::paths::data_dir().join("snippets.json"),
            super::snippets_store_path()
        );
    }
}
