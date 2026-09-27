//! ThinkTerm command snippets shown in the right sidebar.
//!
//! The snippets live in the plugin host, the thinkterm-plugin-server process,
//! which keeps them and does everything with them: searching, saving,
//! deleting, making the text a pane is sent (see thinkterm-snippets
//! `wire`). A window's panel is only a view of it: each window keeps its
//! own search and the rows sent for it (thinkterm-snippets `view`), and
//! forwards what the user does. This holds what the windows share: the one
//! session with the host, and the windows to tell when the snippets change.
//! A browser's panel is another view of the same.
//!
//! The session is let go once no window has painted the panel for a while,
//! and the windows' rows with it; the next paint asks again.

use crate::termwindow::{TermWindow, TermWindowNotif};
use anyhow::Result;
use parking_lot::{Mutex, MutexGuard};
use serde::de::DeserializeOwned;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use thinkterm_plugin_channel::client::{Host, Notice, Session};
use thinkterm_snippets::wire::{Event, Request, PLUGIN};
use window::{Window, WindowOps};

pub use thinkterm_snippets::wire::{Row, Saved, Snippet};

/// How long the panel may go unpainted before the session is let go.
const RELEASE_AFTER: Duration = Duration::from_secs(120);
/// How often that is looked at.
const RELEASE_CHECK: Duration = Duration::from_secs(15);

struct Shared {
    session: Option<Session>,
    /// Counts sessions, so a release check stops with the one it watched.
    generation: u64,
    /// The windows that have painted the panel since the session started:
    /// the ones told when the snippets change, or the rows are let go. A
    /// handful at most; one that has closed ignores what it is told.
    windows: Vec<Window>,
    /// Why the host cannot be reached, while it cannot.
    trouble: Option<String>,
    last_used: Option<Instant>,
}

static SHARED: Mutex<Shared> = parking_lot::const_mutex(Shared {
    session: None,
    generation: 0,
    windows: Vec::new(),
    trouble: None,
    last_used: None,
});

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

/// The shared state, marked as in use, with a session to the host. On the
/// GUI thread: it may start the release check.
fn shared_in_use() -> MutexGuard<'static, Shared> {
    let mut shared = SHARED.lock();
    shared.last_used = Some(Instant::now());
    if shared.session.is_none() {
        let started = Host::for_this_build().and_then(|host| Session::start(host, on_notice));
        match started {
            Ok(session) => {
                shared.session = Some(session);
                shared.generation += 1;
                release_when_idle(shared.generation);
            }
            Err(err) => shared.trouble = Some(format!("{err:#}")),
        }
    }
    shared
}

/// `window` paints the panel: it is told when the snippets change, and the
/// session is kept.
pub fn in_use(window: &Window) {
    let mut shared = shared_in_use();
    if !shared.windows.contains(window) {
        shared.windows.push(window.clone());
    }
}

/// Why the host cannot be reached, while it cannot.
pub fn trouble() -> Option<String> {
    SHARED.lock().trouble.clone()
}

/// Has every window that shows the panel apply `change` to itself.
fn tell_windows(change: fn(&mut TermWindow)) {
    let windows: Vec<Window> = SHARED.lock().windows.iter().cloned().collect();
    promise::spawn::spawn_into_main_thread(async move {
        for window in windows {
            let repaint = window.clone();
            window.notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                change(term_window);
                repaint.invalidate();
            })));
        }
    })
    .detach();
}

fn on_notice(notice: Notice) {
    match notice {
        // Whatever the host said before, it is asked again.
        Notice::Connected => {
            SHARED.lock().trouble = None;
            tell_windows(TermWindow::snippets_changed);
        }
        Notice::Event { plugin, body } if plugin == PLUGIN => {
            if let Ok(Event::Changed) = serde_json::from_value(body) {
                tell_windows(TermWindow::snippets_changed);
            }
        }
        Notice::Event { .. } => {}
        Notice::Trouble(why) => {
            SHARED.lock().trouble = Some(why);
            // Rows already on show stay; an empty panel says why.
            tell_windows(|_| {});
        }
    }
}

/// Lets the session go once no window has painted the panel for a while,
/// and the windows' rows with it -- without asking them to paint, which
/// would only ask for them again.
fn release_when_idle(generation: u64) {
    promise::spawn::spawn(async move {
        loop {
            smol::Timer::after(RELEASE_CHECK).await;
            let mut shared = SHARED.lock();
            if shared.generation != generation || shared.session.is_none() {
                return;
            }
            let idle = shared
                .last_used
                .map_or(true, |used| used.elapsed() >= RELEASE_AFTER);
            if !idle {
                continue;
            }
            shared.session = None;
            shared.trouble = None;
            for window in shared.windows.drain(..) {
                window.notify(TermWindowNotif::Apply(Box::new(|term_window| {
                    term_window.snippets_released();
                })));
            }
            return;
        }
    })
    .detach();
}

fn to_body(request: &Request) -> serde_json::Value {
    serde_json::to_value(request).expect("a snippets request always serialises")
}

fn from_body<T: DeserializeOwned>(body: serde_json::Value) -> std::result::Result<T, String> {
    serde_json::from_value(body).map_err(|err| format!("an answer that does not read: {err}"))
}

/// Asks the host something on behalf of `window`, and hands the answer, or
/// why there is none, to `apply` on that window alone.
fn ask<T: DeserializeOwned + Send + Sync + 'static>(
    request: Request,
    window: Window,
    apply: impl FnOnce(&mut TermWindow, std::result::Result<T, String>) + Send + Sync + 'static,
) {
    let shared = shared_in_use();
    let deliver = move |answer: std::result::Result<T, String>| {
        promise::spawn::spawn_into_main_thread(async move {
            let repaint = window.clone();
            window.notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                apply(term_window, answer);
                repaint.invalidate();
            })));
        })
        .detach();
    };
    match &shared.session {
        Some(session) => session.call(PLUGIN, to_body(&request), move |answer| {
            deliver(answer.and_then(from_body));
        }),
        None => deliver(Err(shared
            .trouble
            .clone()
            .unwrap_or_else(|| "the plugin host cannot be reached".into()))),
    }
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
    let shared = shared_in_use();
    let Some(session) = &shared.session else {
        log::error!("failed to delete snippet {id}: the plugin host cannot be reached");
        return;
    };
    let request = Request::Delete { id: id.to_string() };
    let id = id.to_string();
    session.call(PLUGIN, to_body(&request), move |answer| {
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
