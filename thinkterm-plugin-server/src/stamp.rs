//! Telling that a file changed without reading it: its length and the time
//! it was last written. What the host keeps (the snippets, the switches)
//! and what it runs (manifests, programs) are compared by stamp before use,
//! so a change made from outside is picked up without watching for it.
//!
//! A script's plugin is stamped whole ([`tree_stamp`]): the script may load
//! any file beside it, so a change to any of them is a change to what runs.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Stamp {
    len: u64,
    modified: Option<SystemTime>,
    /// For a stamp of several files, what their stamps fold to.
    folded: u64,
}

/// The file's stamp; `None` when there is no such file.
pub fn stamp(path: &Path) -> Option<Stamp> {
    let meta = std::fs::metadata(path).ok()?;
    Some(Stamp {
        len: meta.len(),
        modified: meta.modified().ok(),
        folded: 0,
    })
}

impl Stamp {
    /// This stamp and `other` as one, which changes when either does.
    pub fn with(self, other: Option<Stamp>) -> Stamp {
        let mut hasher = DefaultHasher::new();
        self.hash(&mut hasher);
        other.hash(&mut hasher);
        Stamp {
            len: 0,
            modified: None,
            folded: hasher.finish(),
        }
    }
}

/// The most files and directories one tree's stamp looks at: a plugin's
/// directory with more holds something it does not run -- a build, an
/// environment -- that is not walked on every use.
const TREE_LIMIT: usize = 2000;
/// How deep it goes.
const TREE_DEPTH: usize = 8;
/// Directories a tree's stamp leaves out, besides hidden ones: what tools
/// write beside a plugin's files, not the plugin.
const UNSTAMPED: [&str; 4] = ["__pycache__", "node_modules", "target", "venv"];

/// The trees found too big to stamp whole, said once each.
static TOO_BIG: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

/// The stamp of every file under `dir` -- each with the extension `only`,
/// when given -- folded into one: what changes when any of them is
/// written, added, removed or renamed. Hidden files and directories, and
/// [`UNSTAMPED`] ones, are left out; a link is followed to a file, not into
/// a directory. Past [`TREE_LIMIT`] entries the rest are left out too, and
/// the log says so once. `None` when `dir` cannot be read.
pub fn tree_stamp(dir: &Path, only: Option<&str>) -> Option<Stamp> {
    let mut found: Vec<(PathBuf, u64, Option<SystemTime>)> = Vec::new();
    let mut seen = 0usize;
    let mut cut = false;
    let mut pending = vec![(dir.to_path_buf(), 0usize)];
    let mut first = true;
    while let Some((at, depth)) = pending.pop() {
        let entries = match std::fs::read_dir(&at) {
            Ok(entries) => entries,
            Err(_) if first => return None,
            Err(_) => continue,
        };
        first = false;
        for entry in entries.filter_map(Result::ok) {
            seen += 1;
            if seen > TREE_LIMIT {
                cut = true;
                break;
            }
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with('.') || UNSTAMPED.contains(&name.as_ref()) {
                continue;
            }
            let path = entry.path();
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                if depth + 1 < TREE_DEPTH {
                    pending.push((path, depth + 1));
                } else {
                    cut = true;
                }
                continue;
            }
            if only.is_some_and(|only| path.extension().is_none_or(|ext| ext != only)) {
                continue;
            }
            // A link's target, when that is a file.
            let Ok(meta) = std::fs::metadata(&path) else {
                continue;
            };
            if meta.is_file() {
                let relative = path.strip_prefix(dir).unwrap_or(&path).to_path_buf();
                found.push((relative, meta.len(), meta.modified().ok()));
            }
        }
        if cut && seen > TREE_LIMIT {
            break;
        }
    }
    if cut {
        let mut said = TOO_BIG
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !said.iter().any(|known| known == dir) {
            said.push(dir.to_path_buf());
            log::warn!(
                "{} has more than {TREE_LIMIT} files or goes deeper than {TREE_DEPTH} \
                 directories: a change past that does not start its plugin again",
                dir.display()
            );
        }
    }
    found.sort();
    let mut hasher = DefaultHasher::new();
    found.hash(&mut hasher);
    cut.hash(&mut hasher);
    Some(Stamp {
        len: found.len() as u64,
        modified: found.iter().filter_map(|(_, _, modified)| *modified).max(),
        folded: hasher.finish(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_trees_stamp_changes_with_any_file_but_what_tools_leave() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("main.py"), "print(1)\n").unwrap();
        std::fs::create_dir(root.join("lib")).unwrap();
        std::fs::write(root.join("lib").join("util.py"), "x = 1\n").unwrap();
        let before = tree_stamp(root, None).unwrap();
        assert_eq!(
            tree_stamp(root, None),
            Some(before),
            "the same files stamp the same"
        );

        // Written over: longer.
        std::fs::write(root.join("lib").join("util.py"), "x = 22\n").unwrap();
        let edited = tree_stamp(root, None).unwrap();
        assert_ne!(edited, before);

        // What a tool writes beside the plugin changes nothing.
        for skipped in ["__pycache__", ".git", "node_modules"] {
            std::fs::create_dir_all(root.join(skipped)).unwrap();
            std::fs::write(root.join(skipped).join("x"), "y").unwrap();
        }
        std::fs::write(root.join(".hidden"), "y").unwrap();
        assert_eq!(tree_stamp(root, None), Some(edited));

        // Added, then renamed.
        std::fs::write(root.join("new.py"), "").unwrap();
        let added = tree_stamp(root, None).unwrap();
        assert_ne!(added, edited);
        std::fs::rename(root.join("new.py"), root.join("other.py")).unwrap();
        assert_ne!(tree_stamp(root, None).unwrap(), added);

        assert_eq!(tree_stamp(&root.join("nothing-here"), None), None);
    }

    #[test]
    fn a_trees_stamp_of_one_kind_of_file_leaves_the_others_out() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("main.py"), "print(1)\n").unwrap();
        let before = tree_stamp(root, Some("py")).unwrap();
        // What the plugin writes as it runs.
        std::fs::write(root.join("state.json"), "{}").unwrap();
        std::fs::write(root.join("log.txt"), "x").unwrap();
        assert_eq!(tree_stamp(root, Some("py")), Some(before));
        std::fs::write(root.join("main.py"), "print(22)\n").unwrap();
        assert_ne!(tree_stamp(root, Some("py")).unwrap(), before);
    }

    #[test]
    fn one_stamp_with_another_changes_when_either_does() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (dir.path().join("a"), dir.path().join("b"));
        std::fs::write(&a, "1").unwrap();
        std::fs::write(&b, "1").unwrap();
        let both = stamp(&a).unwrap().with(stamp(&b));
        assert_eq!(stamp(&a).unwrap().with(stamp(&b)), both);
        std::fs::write(&b, "22").unwrap();
        assert_ne!(stamp(&a).unwrap().with(stamp(&b)), both);
        assert_ne!(stamp(&a).unwrap().with(None), both);
    }
}
