//! The store as a JSON file. Saving writes a new file beside the old one and
//! renames it into place, so a crash mid-write leaves the old file whole.

use crate::SnippetStore;
use anyhow::{Context, Result};
use std::fs;
use std::io::Write;
use std::path::Path;

/// Reads the store at `path`; a missing file is an empty store.
pub fn load(path: &Path) -> Result<SnippetStore> {
    if !path.exists() {
        return Ok(SnippetStore::default());
    }
    let text = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))
}

pub fn save(path: &Path, store: &SnippetStore) -> Result<()> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_store_loads_empty() {
        let dir = tempfile::tempdir().unwrap();
        let store = load(&dir.path().join("snippets.json")).unwrap();
        assert!(store.snippets().is_empty());
    }

    #[test]
    fn store_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("snippets.json");
        let mut store = SnippetStore::default();
        store.create("snippet-1".into(), "List", "ls -la".into(), 1);
        store.create("snippet-2".into(), "", "pwd".into(), 2);
        store.delete("snippet-2", 3);
        save(&path, &store).unwrap();
        assert_eq!(load(&path).unwrap(), store);
    }

    #[test]
    fn new_ids_do_not_repeat_within_a_millisecond() {
        assert_ne!(crate::new_id(7), crate::new_id(7));
    }
}
