//! Server-side ownership of the ThinkTerm sidebar tree.
//!
//! The mux server is the single source of truth for Space/Project/Thread on a
//! remote host: any number of devices can attach and they all see the same
//! rows. Clients send one `TreeOp` at a time, the op is applied here with
//! `codec::apply_op` — the same function the client ran locally — and the
//! resulting tree is broadcast to every connection.
//!
//! Storage is a single JSON file. The server is one process, so all writes are
//! already serialized through this mutex; there is nothing a database would
//! buy at this size (tens of KB) that an atomic file replace does not.

use anyhow::{Context, Result};
use codec::{apply_op, ensure_unique_thread_names, ThinkTermTree, TreeOp};
use mux::{Mux, MuxNotification};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

lazy_static::lazy_static! {
    static ref TREE: Mutex<ThinkTermTree> = Mutex::new(load_or_default());
}

pub fn tree_path() -> PathBuf {
    config::DATA_DIR.join("thinkterm_tree.json")
}

fn load_or_default() -> ThinkTermTree {
    let path = tree_path();
    match load_from_path(&path) {
        Ok(mut tree) => {
            if let Err(err) = repair_loaded_tree(&path, &mut tree) {
                log::error!(
                    "failed to persist repaired ThinkTerm tree to {}: {err:#}",
                    path.display()
                );
            }
            tree
        }
        Err(err) => {
            // A corrupt or unreadable tree must not stop the server from
            // serving terminals; the client will re-seed an empty one.
            log::warn!(
                "failed to load ThinkTerm tree from {}: {err:#}; starting empty",
                path.display()
            );
            ThinkTermTree::default()
        }
    }
}

#[cfg(test)]
fn load_repaired_from_path(path: &Path) -> Result<ThinkTermTree> {
    let mut tree = load_from_path(path)?;
    repair_loaded_tree(path, &mut tree)?;
    Ok(tree)
}

fn repair_loaded_tree(path: &Path, tree: &mut ThinkTermTree) -> Result<()> {
    if ensure_unique_thread_names(tree) {
        tree.revision = tree.revision.saturating_add(1);
        save_to_path(path, &tree)
            .with_context(|| format!("persist repaired ThinkTerm tree to {}", path.display()))?;
    }
    Ok(())
}

fn load_from_path(path: &Path) -> Result<ThinkTermTree> {
    if !path.exists() {
        return Ok(ThinkTermTree::default());
    }
    let file = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    serde_json::from_reader(std::io::BufReader::new(file))
        .with_context(|| format!("parse {}", path.display()))
}

/// Replace the file atomically so that a crash mid-write can never leave a
/// half-serialized tree behind. Same shape as the GUI's own store writer.
fn save_to_path(path: &Path, tree: &ThinkTermTree) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("{} has no parent directory", path.display()))?;
    std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;

    let mut file = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("create temporary tree file in {}", parent.display()))?;
    serde_json::to_writer_pretty(&mut file, tree)
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

/// The tree as it stands right now.
pub fn snapshot() -> ThinkTermTree {
    TREE.lock().unwrap().clone()
}

fn apply_and_persist(
    current: &ThinkTermTree,
    ops: &[TreeOp],
    path: &Path,
) -> Result<(ThinkTermTree, bool)> {
    let mut candidate = current.clone();
    let mut changed = false;
    for op in ops {
        changed |= apply_op(&mut candidate, op);
    }
    if !changed {
        return Ok((candidate, false));
    }
    candidate.revision = candidate.revision.saturating_add(1);
    save_to_path(path, &candidate)
        .with_context(|| format!("persist authoritative ThinkTerm tree to {}", path.display()))?;
    Ok((candidate, true))
}

/// Apply `ops` in order and return the resulting tree.
///
/// When nothing changed (unknown ids, no-op moves, a racing delete from
/// another device, or a seed batch the server already has) neither the file
/// nor the other clients are touched; the caller still gets the current tree
/// so its own RPC can reconcile.
pub fn mutate(ops: &[TreeOp]) -> Result<ThinkTermTree> {
    // The file is written while the lock is still held. Dropping it first and
    // saving afterwards lets two connections race: the one that applied the
    // *older* revision can reach the disk last and leave the file behind what
    // every client has already been shown, so a delete or rename would come
    // back from the dead at the next server restart. The tree is tens of KB,
    // so serializing the writes costs nothing worth measuring.
    let (tree, changed) = {
        let mut guard = TREE.lock().unwrap();
        let path = tree_path();
        let (candidate, changed) = apply_and_persist(&guard, ops, &path)?;
        if changed {
            *guard = candidate;
        }
        (guard.clone(), changed)
    };

    if changed {
        Mux::notify_from_any_thread(MuxNotification::ThinkTermTreeChanged);
    }

    Ok(tree)
}

#[cfg(test)]
mod test {
    use super::*;
    use codec::TreeOp;

    fn sample() -> ThinkTermTree {
        let mut tree = ThinkTermTree::default();
        assert!(apply_op(
            &mut tree,
            &TreeOp::CreateSpace {
                space_id: "s1".into(),
                name: "Work".into()
            }
        ));
        assert!(apply_op(
            &mut tree,
            &TreeOp::CreateProject {
                project_id: "p1".into(),
                space_id: "s1".into(),
                name: "thinkterm".into(),
                path: "/home/x/github/ThinkTerm".into(),
            }
        ));
        assert!(apply_op(
            &mut tree,
            &TreeOp::CreateThread {
                thread_id: "t1".into(),
                project_id: "p1".into(),
                name: "main".into(),
                workspace: Some("thinkterm:p1:t1".into()),
                created_at: 42,
            }
        ));
        tree
    }

    #[test]
    fn a_saved_tree_reloads_identically() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("thinkterm_tree.json");
        let tree = sample();

        save_to_path(&path, &tree).unwrap();
        assert_eq!(load_from_path(&path).unwrap(), tree);
    }

    /// The revision is what lets a client tell "never written to" from "the
    /// user deleted the last Space", so it has to outlive a server restart
    /// just as the rows do.
    #[test]
    fn the_revision_survives_a_reload() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("thinkterm_tree.json");
        let mut tree = sample();
        tree.revision = 12;
        save_to_path(&path, &tree).unwrap();

        let reloaded = load_from_path(&path).unwrap();
        assert_eq!(reloaded.revision, 12);
        assert!(!reloaded.is_uninitialized());
    }

    #[test]
    fn a_missing_file_loads_as_an_empty_tree() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("absent.json");
        assert_eq!(load_from_path(&path).unwrap(), ThinkTermTree::default());
    }

    #[test]
    fn a_corrupt_file_is_reported_rather_than_silently_emptied() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("corrupt.json");
        std::fs::write(&path, b"{ this is not json").unwrap();
        // load_from_path surfaces the error; load_or_default is what decides
        // to fall back, and it logs when it does.
        assert!(load_from_path(&path).is_err());
    }

    #[test]
    fn saving_replaces_rather_than_appends() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("thinkterm_tree.json");
        let mut tree = sample();
        save_to_path(&path, &tree).unwrap();

        assert!(apply_op(
            &mut tree,
            &TreeOp::DeleteThread {
                thread_id: "t1".into()
            }
        ));
        save_to_path(&path, &tree).unwrap();

        let reloaded = load_from_path(&path).unwrap();
        assert_eq!(reloaded, tree);
        assert!(reloaded.thread("t1").is_none());
    }

    #[test]
    fn loading_repairs_and_persists_duplicate_thread_names() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("thinkterm_tree.json");
        let mut tree = sample();
        let duplicate = tree.projects[0].threads[0].clone();
        let mut duplicate = duplicate;
        duplicate.id = "t2".into();
        tree.projects[0].threads.push(duplicate);
        tree.revision = 7;
        save_to_path(&path, &tree).unwrap();

        let repaired = load_repaired_from_path(&path).unwrap();
        assert_eq!(repaired.revision, 8);
        assert_eq!(repaired.projects[0].threads[0].name, "main");
        assert_eq!(repaired.projects[0].threads[1].name, "main 2");
        assert_eq!(load_from_path(&path).unwrap(), repaired);
    }

    #[test]
    fn failed_persistence_does_not_commit_the_candidate_tree() {
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("not-a-directory");
        std::fs::write(&blocker, b"file").unwrap();
        let path = blocker.join("thinkterm_tree.json");
        let current = sample();
        let op = TreeOp::RenameSpace {
            space_id: "s1".into(),
            name: "Renamed".into(),
        };

        assert!(apply_and_persist(&current, &[op], &path).is_err());
        assert_eq!(current.space("s1").unwrap().name, "Work");
        assert_eq!(current.revision, 0);
    }
}
