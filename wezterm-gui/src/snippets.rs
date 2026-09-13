//! ThinkTerm command snippets shown in the right sidebar.
//!
//! Snippets are intentionally stored as ordinary user data rather than as
//! secrets. They are command templates, not a password vault.

use anyhow::{Context, Result};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

pub type SnippetId = String;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SnippetRecord {
    pub id: SnippetId,
    pub title: String,
    pub body: String,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct SnippetStore {
    #[serde(default = "store_version")]
    version: u32,
    #[serde(default)]
    snippets: Vec<SnippetRecord>,
}

impl Default for SnippetStore {
    fn default() -> Self {
        Self {
            version: store_version(),
            snippets: vec![],
        }
    }
}

fn store_version() -> u32 {
    1
}

static NEXT_SNIPPET_COUNTER: AtomicU64 = AtomicU64::new(1);

lazy_static::lazy_static! {
    static ref SNIPPET_STORE: Mutex<SnippetStore> =
        Mutex::new(load_snippet_store().unwrap_or_else(|err| {
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

pub fn title_from_body(body: &str) -> String {
    body.lines()
        .find_map(|line| {
            let trimmed = line.trim();
            (!trimmed.is_empty()).then_some(trimmed)
        })
        .unwrap_or("Untitled snippet")
        .chars()
        .take(48)
        .collect()
}

fn load_snippet_store() -> Result<SnippetStore> {
    load_snippet_store_from_path(&snippets_store_path())
}

fn load_snippet_store_from_path(path: &Path) -> Result<SnippetStore> {
    if !path.exists() {
        return Ok(SnippetStore::default());
    }
    let text = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))
}

fn save_snippet_store(store: &SnippetStore) -> Result<()> {
    save_snippet_store_to_path(&snippets_store_path(), store)
}

fn save_snippet_store_to_path(path: &Path, store: &SnippetStore) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
        let mut file = tempfile::NamedTempFile::new_in(parent)
            .with_context(|| format!("create temporary snippets store in {}", parent.display()))?;
        serde_json::to_writer_pretty(&mut file, store)
            .with_context(|| format!("write {}", path.display()))?;
        file.flush()
            .with_context(|| format!("flush {}", path.display()))?;
        file.as_file()
            .sync_all()
            .with_context(|| format!("sync {}", path.display()))?;
        file.persist(path)
            .with_context(|| format!("replace {}", path.display()))?;
        set_user_private_permissions(path);
        return Ok(());
    }

    let mut file = fs::File::create(path).with_context(|| format!("create {}", path.display()))?;
    serde_json::to_writer_pretty(&mut file, store)
        .with_context(|| format!("write {}", path.display()))?;
    file.flush()
        .with_context(|| format!("flush {}", path.display()))?;
    file.sync_all()
        .with_context(|| format!("sync {}", path.display()))?;
    set_user_private_permissions(path);
    Ok(())
}

#[cfg(unix)]
fn set_user_private_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;

    if let Err(err) = fs::set_permissions(path, fs::Permissions::from_mode(0o600)) {
        log::warn!(
            "failed to set private permissions on snippets store {}: {err:#}",
            path.display()
        );
    }
}

#[cfg(not(unix))]
fn set_user_private_permissions(_path: &Path) {}

pub fn list_snippets() -> Vec<SnippetRecord> {
    SNIPPET_STORE.lock().snippets.clone()
}

pub fn get_snippet(id: &str) -> Option<SnippetRecord> {
    SNIPPET_STORE
        .lock()
        .snippets
        .iter()
        .find(|snippet| snippet.id == id)
        .cloned()
}

pub fn create_snippet(title: String, body: String) -> Result<SnippetRecord> {
    let now = now_ms();
    let title = normalized_title(title, &body);
    let record = SnippetRecord {
        id: format!(
            "snippet-{}-{}",
            now,
            NEXT_SNIPPET_COUNTER.fetch_add(1, Ordering::Relaxed)
        ),
        title,
        body,
        created_at_ms: now,
        updated_at_ms: now,
    };
    let mut store = SNIPPET_STORE.lock();
    store.snippets.insert(0, record.clone());
    save_snippet_store(&store)?;
    Ok(record)
}

pub fn update_snippet(id: &str, title: String, body: String) -> Result<Option<SnippetRecord>> {
    let mut store = SNIPPET_STORE.lock();
    let Some(index) = store.snippets.iter().position(|snippet| snippet.id == id) else {
        return Ok(None);
    };
    let mut record = store.snippets[index].clone();
    record.title = normalized_title(title, &body);
    record.body = body;
    record.updated_at_ms = now_ms();
    store.snippets[index] = record.clone();
    save_snippet_store(&store)?;
    Ok(Some(record))
}

pub fn delete_snippet(id: &str) -> Result<bool> {
    let mut store = SNIPPET_STORE.lock();
    let before = store.snippets.len();
    store.snippets.retain(|snippet| snippet.id != id);
    let changed = store.snippets.len() != before;
    if changed {
        save_snippet_store(&store)?;
    }
    Ok(changed)
}

fn normalized_title(title: String, body: &str) -> String {
    let title = title.trim();
    if title.is_empty() {
        title_from_body(body)
    } else {
        title.chars().take(80).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn title_falls_back_to_first_non_empty_body_line() {
        assert_eq!(title_from_body("\n  ls -la\npwd"), "ls -la");
    }

    #[test]
    fn missing_store_loads_empty() {
        let dir = tempfile::tempdir().unwrap();
        let store = load_snippet_store_from_path(&dir.path().join("snippets.json")).unwrap();
        assert!(store.snippets.is_empty());
    }

    #[test]
    fn store_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("snippets.json");
        let store = SnippetStore {
            version: 1,
            snippets: vec![SnippetRecord {
                id: "snippet-1".to_string(),
                title: "List".to_string(),
                body: "ls -la".to_string(),
                created_at_ms: 1,
                updated_at_ms: 2,
            }],
        };
        save_snippet_store_to_path(&path, &store).unwrap();
        assert_eq!(load_snippet_store_from_path(&path).unwrap(), store);
    }
}
