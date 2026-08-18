//! Durable selection of ThinkTerm's server-wide frontend access mode.
//!
//! Only the mode is durable. Owners and generations identify live transports
//! and must be rebuilt after each mux-server restart. A malformed existing file
//! is never silently overwritten: the server starts safely in Handoff mode but
//! refuses mode mutations until the operator repairs or removes that file.
//!
//! # Single writer per file
//!
//! `STORE` is a per-process cache read once at startup. Both wezterm-gui
//! (which also calls `persist_mode` in-process from its own frontend-mode UI)
//! and wezterm-mux-server cache independently, and neither re-reads on a
//! schedule: mux mode is initialized exactly once per process
//! (`initialize_mux`, `Mux::initialize_frontend_access_mode`). Concurrent
//! writers are therefore last-writer-wins; `persist_mode` re-reads the file
//! before each write so a concurrent change is *reported* (and the write is
//! based on the on-disk truth), but modes are not reconciled across live
//! processes — that would need owner-validated runtime handoff and is out of
//! scope here.

use anyhow::{Context, Result};
use mux::{FrontendAccessMode, Mux};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

#[derive(Clone, Debug, Deserialize, Serialize)]
struct StoredAccess {
    version: u32,
    mode: StoredMode,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum StoredMode {
    TmuxLatest,
    Handoff,
}

impl From<StoredMode> for FrontendAccessMode {
    fn from(value: StoredMode) -> Self {
        match value {
            StoredMode::TmuxLatest => Self::TmuxLatest,
            StoredMode::Handoff => Self::Handoff,
        }
    }
}

impl From<FrontendAccessMode> for StoredMode {
    fn from(value: FrontendAccessMode) -> Self {
        match value {
            FrontendAccessMode::TmuxLatest => Self::TmuxLatest,
            FrontendAccessMode::Handoff => Self::Handoff,
        }
    }
}

#[derive(Debug)]
struct StoreState {
    mode: FrontendAccessMode,
    load_error: Option<String>,
}

lazy_static::lazy_static! {
    static ref STORE: Mutex<StoreState> = Mutex::new(load_state(&access_path()));
}

pub fn access_path() -> PathBuf {
    config::DATA_DIR.join("thinkterm_access.json")
}

fn load_state(path: &Path) -> StoreState {
    match load_from_path(path) {
        Ok(mode) => StoreState {
            mode,
            load_error: None,
        },
        Err(err) => {
            log::error!(
                "failed to load ThinkTerm access mode from {}: {err:#}; \
                 starting in Handoff and refusing to overwrite the file",
                path.display()
            );
            StoreState {
                mode: FrontendAccessMode::Handoff,
                load_error: Some(format!("{err:#}")),
            }
        }
    }
}

fn load_from_path(path: &Path) -> Result<FrontendAccessMode> {
    if !path.exists() {
        return Ok(FrontendAccessMode::Handoff);
    }
    let file = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let stored: StoredAccess = serde_json::from_reader(std::io::BufReader::new(file))
        .with_context(|| format!("parse {}", path.display()))?;
    if stored.version != 1 {
        anyhow::bail!(
            "unsupported ThinkTerm access file version {} in {}",
            stored.version,
            path.display()
        );
    }
    Ok(stored.mode.into())
}

fn save_to_path(path: &Path, mode: FrontendAccessMode) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("{} has no parent directory", path.display()))?;
    std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    let mut file = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("create temporary access file in {}", parent.display()))?;
    serde_json::to_writer_pretty(
        &mut file,
        &StoredAccess {
            version: 1,
            mode: mode.into(),
        },
    )
    .with_context(|| format!("write {}", path.display()))?;
    file.flush()
        .with_context(|| format!("flush {}", path.display()))?;
    file.as_file()
        .sync_all()
        .with_context(|| format!("sync {}", path.display()))?;
    file.persist(path)
        .with_context(|| format!("replace {}", path.display()))?;
    Ok(())
}

pub fn initialize_mux(mux: &Mux) {
    if mux.frontend_access_mode_is_initialized() {
        return;
    }
    mux.initialize_frontend_access_mode(STORE.lock().unwrap().mode);
}

/// Persist before the caller commits the mux state. `StoreState` changes only
/// after the atomic replace succeeds.
pub fn persist_mode(mode: FrontendAccessMode) -> Result<()> {
    persist_mode_at(&access_path(), &mut STORE.lock().unwrap(), mode)
}

fn persist_mode_at(path: &Path, store: &mut StoreState, mode: FrontendAccessMode) -> Result<()> {
    if let Some(err) = &store.load_error {
        anyhow::bail!("refusing to overwrite unreadable {}: {err}", path.display());
    }
    // Another process (GUI-embedded server vs standalone mux-server) may have
    // written the file since this process cached it. Decide against the
    // on-disk truth, not the cache: the stale-cache early return below used
    // to skip a write the file actually needed.
    match load_from_path(path) {
        Ok(on_disk) => {
            if on_disk != store.mode {
                log::warn!(
                    "{} was changed out from under this process (on disk {on_disk:?}, \
                     this process last knew {:?}); writing {mode:?} anyway — last \
                     writer wins",
                    path.display(),
                    store.mode
                );
                store.mode = on_disk;
            }
        }
        Err(err) => {
            log::warn!(
                "refusing to overwrite {}: it became unreadable since startup: {err:#}",
                path.display()
            );
            return Err(err);
        }
    }
    if store.mode == mode {
        return Ok(());
    }
    save_to_path(path, mode)?;
    store.mode = mode;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_defaults_to_handoff_and_round_trips_both_modes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("thinkterm_access.json");
        assert_eq!(load_from_path(&path).unwrap(), FrontendAccessMode::Handoff);

        save_to_path(&path, FrontendAccessMode::TmuxLatest).unwrap();
        assert_eq!(
            load_from_path(&path).unwrap(),
            FrontendAccessMode::TmuxLatest
        );
        save_to_path(&path, FrontendAccessMode::Handoff).unwrap();
        assert_eq!(load_from_path(&path).unwrap(), FrontendAccessMode::Handoff);
    }

    #[test]
    fn malformed_file_is_an_error_instead_of_becoming_a_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("thinkterm_access.json");
        std::fs::write(&path, b"not json").unwrap();
        assert!(load_from_path(&path).is_err());
        assert!(path.exists());
    }

    #[test]
    fn a_concurrent_mode_change_is_reported_and_the_last_writer_wins() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("thinkterm_access.json");
        // Another process wrote TmuxLatest while this process still believes
        // its own last write of Handoff is current.
        save_to_path(&path, FrontendAccessMode::TmuxLatest).unwrap();
        let mut store = StoreState {
            mode: FrontendAccessMode::Handoff,
            load_error: None,
        };

        // The stale-cache early return used to skip this write entirely,
        // leaving the file saying TmuxLatest while this process believed
        // Handoff. Deciding against the on-disk truth fixes that.
        persist_mode_at(&path, &mut store, FrontendAccessMode::Handoff).unwrap();
        assert_eq!(load_from_path(&path).unwrap(), FrontendAccessMode::Handoff);
        assert_eq!(store.mode, FrontendAccessMode::Handoff);
    }

    #[test]
    fn an_unreadable_file_is_not_overwritten_even_when_the_cache_looks_clean() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("thinkterm_access.json");
        std::fs::write(&path, b"not json").unwrap();
        let mut store = StoreState {
            mode: FrontendAccessMode::Handoff,
            load_error: None,
        };

        assert!(persist_mode_at(&path, &mut store, FrontendAccessMode::TmuxLatest).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"not json");
        assert_eq!(store.mode, FrontendAccessMode::Handoff);
    }
}
