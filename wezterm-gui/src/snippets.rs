//! ThinkTerm command snippets shown in the right sidebar: this desktop's copy
//! of the `thinkterm-snippets` store, kept in the GUI's data directory.

use anyhow::Result;
use parking_lot::Mutex;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use thinkterm_snippets::SnippetStore;

pub use thinkterm_snippets::SnippetRecord;

lazy_static::lazy_static! {
    static ref SNIPPET_STORE: Mutex<SnippetStore> =
        Mutex::new(thinkterm_snippets::file::load(&snippets_store_path()).unwrap_or_else(|err| {
            log::warn!("failed to load ThinkTerm snippets store: {err:#}");
            SnippetStore::default()
        }));
}

pub fn snippets_store_path() -> PathBuf {
    crate::native_paths::data_file("snippets.json")
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

/// Whether `path` reads as a snippet store, as an imported backup must.
pub(crate) fn check_snippet_store(path: &Path) -> Result<()> {
    thinkterm_snippets::file::load(path).map(drop)
}

fn save(store: &SnippetStore) -> Result<()> {
    thinkterm_snippets::file::save(&snippets_store_path(), store)
}

/// The snippets the panel shows for `query`; see `SnippetStore::matching`.
pub fn matching_snippets(query: &str) -> Vec<SnippetRecord> {
    SNIPPET_STORE.lock().matching(query).cloned().collect()
}

pub fn matching_snippet_count(query: &str) -> usize {
    SNIPPET_STORE.lock().matching(query).count()
}

pub fn get_snippet(id: &str) -> Option<SnippetRecord> {
    SNIPPET_STORE.lock().get(id).cloned()
}

pub fn create_snippet(title: String, body: String) -> Result<SnippetRecord> {
    let now = now_ms();
    let mut store = SNIPPET_STORE.lock();
    let record = store.create(thinkterm_snippets::new_id(now), &title, body, now);
    save(&store)?;
    Ok(record)
}

pub fn update_snippet(id: &str, title: String, body: String) -> Result<Option<SnippetRecord>> {
    let mut store = SNIPPET_STORE.lock();
    let Some(record) = store.update(id, &title, body, now_ms()) else {
        return Ok(None);
    };
    save(&store)?;
    Ok(Some(record))
}

pub fn delete_snippet(id: &str) -> Result<bool> {
    let mut store = SNIPPET_STORE.lock();
    let changed = store.delete(id, now_ms());
    if changed {
        save(&store)?;
    }
    Ok(changed)
}
