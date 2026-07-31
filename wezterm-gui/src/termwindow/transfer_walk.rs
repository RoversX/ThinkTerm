//! Turning a dropped folder into a flat list of things to transfer.
//!
//! Deliberately a separate step from the transfer itself. Walking first means
//! the total is known before anything moves — so the progress row can show
//! "12/300" rather than an unbounded spinner — and, more importantly, it means
//! every destination conflict is discovered before the first byte is written,
//! so the user is asked once instead of being interrupted midway.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

/// Above this many entries a folder transfer asks for confirmation first.
///
/// Not a memory limit — the entry list is small. It is a *time* limit: every
/// remote file costs at least one round trip on a single serialized session,
/// so a few thousand small files takes minutes with no way to know that in
/// advance unless we say so.
pub(crate) const TRANSFER_CONFIRM_THRESHOLD: usize = 1_000;

/// A hard stop. Walking a pathological tree should fail cleanly rather than
/// build a list nobody wants.
pub(crate) const TRANSFER_ENTRY_LIMIT: usize = 100_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TransferEntryKind {
    Directory,
    File,
}

/// One thing to create at the destination, named relative to the drop root so
/// the same list works for a local copy and an SFTP upload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TransferEntry {
    pub source: PathBuf,
    /// Path relative to the destination directory, in order: parents always
    /// appear before their children, so a consumer can create as it goes.
    pub relative: PathBuf,
    pub kind: TransferEntryKind,
    pub size: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct TransferPlan {
    pub entries: Vec<TransferEntry>,
    /// Symlinks encountered and deliberately not followed, counted so the user
    /// can be told rather than silently losing them.
    pub skipped_symlinks: usize,
    /// Entries that could not be read at all (permissions, races).
    pub unreadable: usize,
    pub total_bytes: u64,
}

impl TransferPlan {
    pub(crate) fn file_count(&self) -> usize {
        self.entries
            .iter()
            .filter(|entry| entry.kind == TransferEntryKind::File)
            .count()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum TransferPlanError {
    /// The destination is the source, or lives inside it. Copying a directory
    /// into its own subtree makes the walk re-read what it just wrote, which
    /// only ends when the disk is full.
    DestinationInsideSource,
    TooManyEntries(usize),
    Unreadable(String),
}

impl TransferPlanError {
    pub(crate) fn message(&self) -> String {
        match self {
            Self::DestinationInsideSource => {
                "Cannot copy a folder into itself or into one of its own subfolders".to_string()
            }
            Self::TooManyEntries(limit) => {
                format!("That folder holds more than {limit} items")
            }
            Self::Unreadable(path) => format!("Unable to read {path}"),
        }
    }
}

/// Reject a destination that would make the walk eat its own output.
///
/// Compares canonical paths so the check survives symlinks, `..`, and two
/// different spellings of the same directory — the naive string prefix test
/// misses all three.
pub(crate) fn destination_escapes_source(source: &Path, destination_dir: &Path) -> bool {
    let (Ok(source), Ok(destination)) = (source.canonicalize(), destination_dir.canonicalize())
    else {
        // If either side cannot be resolved we cannot prove the copy is safe,
        // and an unprovable copy of a directory into itself is the one that
        // fills the disk. Refuse.
        return true;
    };
    destination == source || destination.starts_with(&source)
}

/// Whether two paths name the same file on disk.
///
/// Matters because `fs::copy` with identical source and destination opens the
/// destination truncating *first*: the shared inode is emptied, zero bytes are
/// then read, and the call reports `Ok(0)`. An 11-byte file becomes 0 bytes
/// and nothing reports an error. Dropping a file back into its own folder is
/// an easy way to trigger it, so it has to be caught before the copy.
pub(crate) fn is_same_file(a: &Path, b: &Path) -> bool {
    // Identity first: hard links give the same file two unrelated names, so
    // their canonical paths differ while a write to either destroys both.
    // Verified: truncating one hard link empties the other.
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if let (Ok(a), Ok(b)) = (a.metadata(), b.metadata()) {
            return a.dev() == b.dev() && a.ino() == b.ino();
        }
    }
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        // A destination that does not exist yet cannot be the source.
        _ => false,
    }
}

/// Confirm a destination really lives under `root` once symlinks are resolved.
///
/// `create_dir_all` happily follows a symlinked directory, so a link named the
/// same as the dropped folder would send the whole tree somewhere else
/// entirely. Checking the resolved parent is what stops that.
///
/// This is a cheap pre-check, not the guarantee: it resolves paths that the
/// write then walks again, so a component swapped in between would slip past.
/// The guarantee comes from [`DestinationRoot`], which descends through
/// directory handles and refuses to traverse a symlink at all. Keep both — the
/// pre-check gives a clear message before any work starts, and the descent is
/// what actually holds.
pub(crate) fn destination_stays_within(root: &Path, destination: &Path) -> bool {
    let Ok(root) = root.canonicalize() else {
        return false;
    };
    // The destination itself may not exist yet; its parent must, and is what
    // decides where the write actually lands.
    let Some(parent) = destination.parent() else {
        return false;
    };
    match parent.canonicalize() {
        Ok(parent) => parent == root || parent.starts_with(&root),
        // Not created yet: it will be made under a parent we already checked
        // on an earlier entry, since entries arrive parents-first.
        Err(_) => true,
    }
}

/// A handle on the directory a copy is allowed to write into.
///
/// Opened once and carried through the whole operation, so every create,
/// rename and delete starts from the *same* verified directory rather than
/// re-resolving a path that could have changed underneath. Each step down
/// uses `O_NOFOLLOW`, so a symlink anywhere in the descent fails instead of
/// redirecting the write.
///
/// This matters wherever the destination is writable by someone other than
/// the user — a shared checkout, a group-writable directory, a network mount.
/// There, planting a link costs an attacker nothing and would have this
/// process write, with *its* privileges, somewhere the attacker cannot reach.
/// "They could edit those files anyway" only holds when the directory belongs
/// solely to the user, which is not an assumption this can make.
pub(crate) struct DestinationRoot {
    path: PathBuf,
    #[cfg(unix)]
    fd: std::os::unix::io::OwnedFd,
}

impl DestinationRoot {
    /// The directory itself, for error messages only — never for reaching the
    /// files, which always goes through the handle.
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(unix)]
mod unix_root {
    use super::DestinationRoot;
    use std::ffi::CString;
    use std::fs::File;
    use std::io::{Error, ErrorKind, Result};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd};
    use std::path::{Component, Path, PathBuf};

    fn component_name(component: Component<'_>) -> Result<CString> {
        match component {
            // Anything but a plain name could leave the subtree; the planner
            // never produces one, and accepting it here would undo the point.
            Component::Normal(name) => CString::new(name.as_bytes())
                .map_err(|_| Error::new(ErrorKind::InvalidInput, "path component contains NUL")),
            _ => Err(Error::new(
                ErrorKind::InvalidInput,
                "path component is not a plain name",
            )),
        }
    }

    fn open_dir_at(parent: &OwnedFd, name: &CString) -> Result<OwnedFd> {
        let fd = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(Error::last_os_error());
        }
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }

    impl DestinationRoot {
        pub(crate) fn open(path: &Path) -> Result<Self> {
            let name = CString::new(path.as_os_str().as_bytes())
                .map_err(|_| Error::new(ErrorKind::InvalidInput, "path contains NUL"))?;
            let fd = unsafe {
                libc::open(
                    name.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
                )
            };
            if fd < 0 {
                return Err(Error::last_os_error());
            }
            Ok(Self {
                path: path.to_path_buf(),
                fd: unsafe { OwnedFd::from_raw_fd(fd) },
            })
        }

        /// Walk to the directory holding `relative`'s last component, starting
        /// from the pinned root, and hand back that directory with the name to
        /// use inside it.
        fn parent_of(&self, relative: &Path) -> Result<(OwnedFd, CString)> {
            let mut components: Vec<_> = relative.components().collect();
            let Some(last) = components.pop() else {
                return Err(Error::new(ErrorKind::InvalidInput, "empty relative path"));
            };
            let mut current = self.fd.try_clone()?;
            for component in components {
                let name = component_name(component)?;
                current = open_dir_at(&current, &name)?;
            }
            Ok((current, component_name(last)?))
        }

        /// Create a directory, treating one that is already there as success
        /// and anything else wearing the name as a clash. `mkdirat` never
        /// follows a symlink at the name it creates.
        pub(crate) fn create_dir(&self, relative: &Path) -> Result<()> {
            let (parent, name) = self.parent_of(relative)?;
            if unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o777) } == 0 {
                return Ok(());
            }
            let err = Error::last_os_error();
            if err.kind() != ErrorKind::AlreadyExists {
                return Err(err);
            }
            let mut stat: libc::stat = unsafe { std::mem::zeroed() };
            if unsafe {
                libc::fstatat(
                    parent.as_raw_fd(),
                    name.as_ptr(),
                    &mut stat,
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            } != 0
            {
                return Err(Error::last_os_error());
            }
            if stat.st_mode & libc::S_IFMT == libc::S_IFDIR {
                Ok(())
            } else {
                Err(Error::new(
                    ErrorKind::AlreadyExists,
                    "a non-folder already has that name",
                ))
            }
        }

        /// Create a directory only if the final name is completely vacant.
        pub(crate) fn create_dir_exclusive(&self, relative: &Path) -> Result<()> {
            let (parent, name) = self.parent_of(relative)?;
            if unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o777) } == 0 {
                Ok(())
            } else {
                Err(Error::last_os_error())
            }
        }

        /// Create a new file, failing if anything already holds that name —
        /// including a symlink, which is never followed.
        pub(crate) fn create_file(&self, relative: &Path) -> Result<File> {
            let (parent, name) = self.parent_of(relative)?;
            let fd = unsafe {
                libc::openat(
                    parent.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_WRONLY
                        | libc::O_CREAT
                        | libc::O_EXCL
                        | libc::O_NOFOLLOW
                        | libc::O_CLOEXEC,
                    0o666 as libc::c_uint,
                )
            };
            if fd < 0 {
                return Err(Error::last_os_error());
            }
            Ok(unsafe { File::from_raw_fd(fd) })
        }

        pub(crate) fn rename(&self, from: &Path, to: &Path) -> Result<()> {
            let (from_dir, from_name) = self.parent_of(from)?;
            let (to_dir, to_name) = self.parent_of(to)?;
            if unsafe {
                libc::renameat(
                    from_dir.as_raw_fd(),
                    from_name.as_ptr(),
                    to_dir.as_raw_fd(),
                    to_name.as_ptr(),
                )
            } == 0
            {
                Ok(())
            } else {
                Err(Error::last_os_error())
            }
        }

        /// Delete through the handle as well. Cleanup runs precisely when
        /// something has already gone wrong, which is the worst moment to
        /// start trusting a path again: a component swapped between the write
        /// and the delete would aim the delete outside the destination.
        pub(crate) fn remove_file(&self, relative: &Path) -> Result<()> {
            let (parent, name) = self.parent_of(relative)?;
            if unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), 0) } == 0 {
                Ok(())
            } else {
                Err(Error::last_os_error())
            }
        }

        pub(crate) fn remove_dir(&self, relative: &Path) -> Result<()> {
            let (parent, name) = self.parent_of(relative)?;
            if unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), libc::AT_REMOVEDIR) } == 0
            {
                Ok(())
            } else {
                Err(Error::last_os_error())
            }
        }

        pub(crate) fn join(&self, relative: &Path) -> PathBuf {
            self.path.join(relative)
        }
    }
}

/// Path-based stand-in for platforms without `openat`.
///
/// Reachable on Windows since remote FOLDER downloads: they start from a
/// context menu, not from the drag-drop path whose missing drop position kept
/// this module dead there. Weaker than the unix version — an intermediate
/// directory component that is a symlink is followed rather than refused —
/// but the folder-download use keeps the risk small: its whole tree starts
/// from a freshly, exclusively created directory, and files are created with
/// `create_new`, which fails if anything (a link included) wears the name.
/// The local drag-drop copy remains unreachable here (Windows and Wayland
/// report no drop position); if that changes, this needs the same
/// handle-based treatment before it can be trusted for arbitrary
/// destinations.
#[cfg(not(unix))]
mod portable_root {
    use super::DestinationRoot;
    use std::fs::{self, File};
    use std::io::{Error, ErrorKind, Result};
    use std::path::{Component, Path, PathBuf};

    fn checked(relative: &Path) -> Result<()> {
        if relative
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
        {
            Ok(())
        } else {
            Err(Error::new(
                ErrorKind::InvalidInput,
                "path component is not a plain name",
            ))
        }
    }

    impl DestinationRoot {
        pub(crate) fn open(path: &Path) -> Result<Self> {
            if path.is_dir() {
                Ok(Self {
                    path: path.to_path_buf(),
                })
            } else {
                Err(Error::new(
                    ErrorKind::NotFound,
                    "destination is not a folder",
                ))
            }
        }

        pub(crate) fn create_dir(&self, relative: &Path) -> Result<()> {
            checked(relative)?;
            let target = self.join(relative);
            match target.symlink_metadata() {
                Ok(meta) if meta.is_dir() => Ok(()),
                Ok(_) => Err(Error::new(
                    ErrorKind::AlreadyExists,
                    "a non-folder already has that name",
                )),
                Err(_) => fs::create_dir_all(&target),
            }
        }

        pub(crate) fn create_dir_exclusive(&self, relative: &Path) -> Result<()> {
            checked(relative)?;
            fs::create_dir(self.join(relative))
        }

        pub(crate) fn create_file(&self, relative: &Path) -> Result<File> {
            checked(relative)?;
            fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(self.join(relative))
        }

        pub(crate) fn rename(&self, from: &Path, to: &Path) -> Result<()> {
            checked(from)?;
            checked(to)?;
            fs::rename(self.join(from), self.join(to))
        }

        pub(crate) fn remove_file(&self, relative: &Path) -> Result<()> {
            checked(relative)?;
            fs::remove_file(self.join(relative))
        }

        pub(crate) fn remove_dir(&self, relative: &Path) -> Result<()> {
            checked(relative)?;
            fs::remove_dir(self.join(relative))
        }

        pub(crate) fn join(&self, relative: &Path) -> PathBuf {
            self.path.join(relative)
        }
    }
}

/// Walk `source` into a flat, parents-first list of entries to create under a
/// destination directory.
///
/// Symlinks are recorded and skipped rather than followed: following them
/// invites loops, and it silently copies data from outside the folder the user
/// pointed at. This matches how the local file indexer already walks.
pub(crate) fn plan_transfer(source: &Path) -> Result<TransferPlan, TransferPlanError> {
    let Some(top_name) = source.file_name() else {
        return Err(TransferPlanError::Unreadable(source.display().to_string()));
    };
    let top = PathBuf::from(top_name);

    let metadata = source
        .symlink_metadata()
        .map_err(|_| TransferPlanError::Unreadable(source.display().to_string()))?;

    let mut plan = TransferPlan::default();
    if metadata.file_type().is_symlink() {
        plan.skipped_symlinks += 1;
        return Ok(plan);
    }
    if metadata.is_file() {
        plan.total_bytes = metadata.len();
        plan.entries.push(TransferEntry {
            source: source.to_path_buf(),
            relative: top,
            kind: TransferEntryKind::File,
            size: metadata.len(),
        });
        return Ok(plan);
    }

    // The folder itself comes first, so consumers can create it before
    // anything is placed inside.
    plan.entries.push(TransferEntry {
        source: source.to_path_buf(),
        relative: top.clone(),
        kind: TransferEntryKind::Directory,
        size: 0,
    });

    for entry in WalkDir::new(source).follow_links(false).min_depth(1) {
        if plan.entries.len() >= TRANSFER_ENTRY_LIMIT {
            return Err(TransferPlanError::TooManyEntries(TRANSFER_ENTRY_LIMIT));
        }
        let Ok(entry) = entry else {
            // A directory that vanished or cannot be read: count it and carry
            // on, so one bad corner does not sink the whole transfer.
            plan.unreadable += 1;
            continue;
        };
        let file_type = entry.file_type();
        if file_type.is_symlink() {
            plan.skipped_symlinks += 1;
            continue;
        }
        let Ok(relative) = entry.path().strip_prefix(source) else {
            plan.unreadable += 1;
            continue;
        };
        let relative = top.join(relative);

        if file_type.is_dir() {
            plan.entries.push(TransferEntry {
                source: entry.path().to_path_buf(),
                relative,
                kind: TransferEntryKind::Directory,
                size: 0,
            });
        } else if file_type.is_file() {
            let size = entry.metadata().map(|meta| meta.len()).unwrap_or(0);
            plan.total_bytes = plan.total_bytes.saturating_add(size);
            plan.entries.push(TransferEntry {
                source: entry.path().to_path_buf(),
                relative,
                kind: TransferEntryKind::File,
                size,
            });
        } else {
            // Sockets, fifos, devices: nothing meaningful to copy.
            plan.unreadable += 1;
        }
    }

    Ok(plan)
}

/// What to do about destinations that already exist.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ConflictChoice {
    Overwrite,
    Skip,
    Cancel,
}

/// How a copy should treat a destination it finds occupied.
///
/// Distinct from [`ConflictChoice`] because "the user said replace" and "we
/// found nothing to replace" must not collapse into the same value: if they
/// did, a file appearing between the check and the write would be destroyed
/// under an authorisation the user never gave.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OverwritePolicy {
    /// Nothing was in the way when we looked, so anything there now arrived
    /// since and must not be touched.
    NoClobber,
    /// The user was shown the clashes and chose to replace them.
    Replace,
    /// The user was shown the clashes and chose to keep them.
    SkipExisting,
}

/// Everything a drop needs decided before any file is written.
#[derive(Debug, Default)]
pub(crate) struct TransferPreflight {
    pub plans: Vec<(PathBuf, TransferPlan)>,
    /// Destination-relative paths that already exist.
    pub conflicts: HashSet<PathBuf>,
    /// Sources rejected outright, as (name, reason).
    pub rejected: Vec<(String, String)>,
    pub skipped_symlinks: usize,
    pub unreadable: usize,
}

impl TransferPreflight {
    /// What to tell the user about things the walk deliberately left out.
    /// Silence here would mean reporting success while quietly omitting data.
    pub(crate) fn omission_note(&self) -> Option<String> {
        let mut parts = Vec::new();
        if self.skipped_symlinks > 0 {
            parts.push(format!("{} symlink(s) skipped", self.skipped_symlinks));
        }
        if self.unreadable > 0 {
            parts.push(format!("{} unreadable item(s)", self.unreadable));
        }
        (!parts.is_empty()).then(|| parts.join(", "))
    }
}

/// Drop the entries a `Skip` choice excludes.
///
/// Directories are never dropped even when they already exist — merging into
/// an existing folder is the whole point of `Skip`, and removing the directory
/// entry would orphan everything underneath it.
pub(crate) fn apply_conflict_choice(
    plan: TransferPlan,
    conflicts: &HashSet<PathBuf>,
    choice: ConflictChoice,
) -> TransferPlan {
    if choice != ConflictChoice::Skip {
        return plan;
    }
    let mut filtered = plan;
    filtered.entries.retain(|entry| {
        entry.kind == TransferEntryKind::Directory || !conflicts.contains(&entry.relative)
    });
    filtered.total_bytes = filtered
        .entries
        .iter()
        .map(|entry| entry.size)
        .fold(0u64, |sum, size| sum.saturating_add(size));
    filtered
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn relatives(plan: &TransferPlan) -> Vec<String> {
        plan.entries
            .iter()
            .map(|entry| entry.relative.to_string_lossy().replace('\\', "/"))
            .collect()
    }

    #[test]
    fn a_folder_walks_into_a_parents_first_list() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("proj");
        fs::create_dir_all(src.join("a/b")).unwrap();
        fs::write(src.join("top.txt"), b"12345").unwrap();
        fs::write(src.join("a/inner.txt"), b"xy").unwrap();
        fs::write(src.join("a/b/deep.txt"), b"z").unwrap();

        let plan = plan_transfer(&src).unwrap();
        let names = relatives(&plan);

        assert!(names.contains(&"proj".to_string()), "{:?}", names);
        assert_eq!(plan.file_count(), 3);
        assert_eq!(plan.total_bytes, 8);

        // Every directory must appear before anything inside it, or a consumer
        // creating as it goes would write into a directory that is not there.
        for (index, name) in names.iter().enumerate() {
            if let Some((parent, _)) = name.rsplit_once('/') {
                let parent_index = names.iter().position(|other| other == parent);
                assert!(
                    parent_index.is_some_and(|parent_index| parent_index < index),
                    "{} must precede {} in {:?}",
                    parent,
                    name,
                    names
                );
            }
        }
    }

    #[test]
    fn a_single_file_plans_as_one_entry() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("notes.txt");
        fs::write(&file, b"hello").unwrap();

        let plan = plan_transfer(&file).unwrap();
        assert_eq!(relatives(&plan), vec!["notes.txt".to_string()]);
        assert_eq!(plan.total_bytes, 5);
        assert_eq!(plan.file_count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_counted_and_not_followed() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("proj");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("real.txt"), b"ok").unwrap();
        // A link pointing back at its own parent: following it would loop.
        std::os::unix::fs::symlink(&src, src.join("loop")).unwrap();

        let plan = plan_transfer(&src).unwrap();
        assert_eq!(plan.skipped_symlinks, 1);
        assert_eq!(plan.file_count(), 1, "only the real file is planned");
        assert!(
            !relatives(&plan).iter().any(|name| name.contains("loop")),
            "the symlink must not be planned"
        );
    }

    /// The disk-filling case: copying a folder into a directory inside itself
    /// makes the walk re-read what it has just written.
    #[test]
    fn a_destination_inside_the_source_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("proj");
        fs::create_dir_all(src.join("vendor")).unwrap();
        let outside = dir.path().join("elsewhere");
        fs::create_dir_all(&outside).unwrap();

        assert!(destination_escapes_source(&src, &src.join("vendor")));
        assert!(
            destination_escapes_source(&src, &src),
            "copying onto itself is equally pointless"
        );
        assert!(!destination_escapes_source(&src, &outside));
    }

    #[cfg(unix)]
    #[test]
    fn the_subtree_check_sees_through_symlinks_and_dot_dot() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("proj");
        fs::create_dir_all(src.join("vendor")).unwrap();
        // A link from outside pointing back inside the source: a plain string
        // prefix test would call this safe.
        let link = dir.path().join("shortcut");
        std::os::unix::fs::symlink(src.join("vendor"), &link).unwrap();
        assert!(destination_escapes_source(&src, &link));

        let round_about = src.join("vendor/../vendor");
        assert!(destination_escapes_source(&src, &round_about));
    }

    /// Pins the worst outcome this module exists to prevent. `fs::copy` with
    /// identical source and destination truncates the shared inode before
    /// reading it, then returns `Ok(0)` — an 11-byte file silently becomes an
    /// empty one and nothing reports a failure. Dropping a file back into its
    /// own folder is the easy way to hit it.
    #[test]
    fn a_file_is_recognised_as_its_own_destination() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        fs::write(&file, b"hello world").unwrap();

        // The exact shape a drop onto the containing folder produces.
        let destination = dir.path().join("a.txt");
        assert!(is_same_file(&file, &destination));

        // A different name in the same folder is a genuine copy.
        assert!(!is_same_file(&file, &dir.path().join("b.txt")));
        // A destination that does not exist cannot be the source.
        assert!(!is_same_file(&file, &dir.path().join("nested/a.txt")));
    }

    /// Hard links defeat a canonical-path comparison: the two names resolve
    /// differently yet share one inode, so truncating the "destination"
    /// empties the source. Verified empirically before this check existed.
    #[cfg(unix)]
    #[test]
    fn a_hard_link_counts_as_the_same_file() {
        let dir = tempfile::tempdir().unwrap();
        let original = dir.path().join("original.txt");
        let link = dir.path().join("hardlink.txt");
        fs::write(&original, b"important data").unwrap();
        fs::hard_link(&original, &link).unwrap();

        assert_ne!(
            original.canonicalize().unwrap(),
            link.canonicalize().unwrap(),
            "the canonical paths differ, which is exactly why this needs inodes"
        );
        assert!(is_same_file(&original, &link));

        // An ordinary copy of the same bytes is still two different files.
        let separate = dir.path().join("separate.txt");
        fs::write(&separate, b"important data").unwrap();
        assert!(!is_same_file(&original, &separate));
    }

    #[cfg(unix)]
    #[test]
    fn the_same_file_is_seen_through_a_symlinked_directory() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        fs::create_dir_all(&real).unwrap();
        let file = real.join("a.txt");
        fs::write(&file, b"data").unwrap();
        // Reaching the same file by another name must not look like a copy.
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert!(is_same_file(&file, &link.join("a.txt")));
    }

    /// The guarantee behind the path pre-check: descending through directory
    /// handles refuses to traverse a symlink at all, so a component swapped
    /// after any path was resolved cannot redirect the write. This is what
    /// matters when the destination is writable by someone else — planting a
    /// link would otherwise borrow this process's privileges to write where
    /// the planter cannot reach.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_parent_component_cannot_be_traversed() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("dest");
        let outside = dir.path().join("outside");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("sub")).unwrap();
        let opened = DestinationRoot::open(&root).unwrap();

        // Writing "through" the link must fail rather than land outside.
        let err = opened
            .create_file(Path::new("sub/payload.txt"))
            .expect_err("a symlinked component must not be traversed");
        assert!(
            matches!(
                err.raw_os_error(),
                Some(libc::ELOOP) | Some(libc::ENOTDIR) | Some(libc::EMLINK)
            ),
            "unexpected error: {err} ({:?})",
            err.raw_os_error()
        );
        assert!(
            !outside.join("payload.txt").exists(),
            "nothing may be written outside the destination"
        );

        // Creating a directory through it is refused for the same reason.
        assert!(opened.create_dir(Path::new("sub/nested")).is_err());
        assert!(!outside.join("nested").exists());

        // A real directory underneath works normally.
        opened.create_dir(Path::new("real")).unwrap();
        let file = opened.create_file(Path::new("real/ok.txt")).unwrap();
        drop(file);
        assert!(root.join("real/ok.txt").is_file());
    }

    /// Cleanup runs precisely when something has already gone wrong, which is
    /// the worst moment to start trusting a path again: a component swapped
    /// between the write and the delete would aim the delete outside the
    /// destination. Deleting through the handle cannot be steered that way.
    #[cfg(unix)]
    #[test]
    fn deleting_goes_through_the_handle_too() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("dest");
        let outside = dir.path().join("outside");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&outside).unwrap();
        let bystander = outside.join("keep.txt");
        fs::write(&bystander, b"not yours to delete").unwrap();

        let opened = DestinationRoot::open(&root).unwrap();
        // A file we did create is removed normally.
        drop(opened.create_file(Path::new("mine.txt")).unwrap());
        opened.remove_file(Path::new("mine.txt")).unwrap();
        assert!(!root.join("mine.txt").exists());

        // Now the destination sprouts a link to somewhere else, the way it
        // might between a failed write and its cleanup.
        std::os::unix::fs::symlink(&outside, root.join("sub")).unwrap();
        assert!(
            opened.remove_file(Path::new("sub/keep.txt")).is_err(),
            "a delete must not be routed through a symlinked component"
        );
        assert!(
            bystander.exists(),
            "the file outside the destination must survive"
        );
    }

    /// The root is opened once and held. Re-opening it per operation would
    /// leave the root's own ancestors free to be swapped in between.
    #[cfg(unix)]
    #[test]
    fn the_root_handle_survives_its_path_being_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        let decoy = dir.path().join("decoy");
        fs::create_dir_all(&real).unwrap();
        fs::create_dir_all(&decoy).unwrap();

        let link = dir.path().join("root");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let opened = DestinationRoot::open(&link).unwrap();

        // Repoint the name the root was opened by.
        fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(&decoy, &link).unwrap();

        drop(opened.create_file(Path::new("written.txt")).unwrap());
        assert!(
            real.join("written.txt").is_file(),
            "the write must follow the handle taken at the start"
        );
        assert!(
            !decoy.join("written.txt").exists(),
            "and must not follow the name to its new target"
        );
    }

    #[cfg(unix)]
    #[test]
    fn creating_a_directory_that_is_already_there_is_success() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let opened = DestinationRoot::open(&root).unwrap();
        opened.create_dir(Path::new("shared")).unwrap();
        // Merging into it again is normal.
        opened.create_dir(Path::new("shared")).unwrap();

        // A file wearing the name is a genuine clash.
        fs::write(root.join("taken"), b"x").unwrap();
        assert!(opened.create_dir(Path::new("taken")).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_destination_reached_through_a_symlink_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("dest");
        let outside = dir.path().join("outside");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&outside).unwrap();
        // A link inside the destination pointing elsewhere: create_dir_all
        // follows it without complaint, so anything written beneath would
        // escape the folder the user chose.
        std::os::unix::fs::symlink(&outside, root.join("escape")).unwrap();

        assert!(destination_stays_within(&root, &root.join("normal.txt")));
        assert!(!destination_stays_within(
            &root,
            &root.join("escape/payload.txt")
        ));
    }

    #[test]
    fn skipping_conflicts_drops_files_but_keeps_directories() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("proj");
        fs::create_dir_all(src.join("a")).unwrap();
        fs::write(src.join("a/keep.txt"), b"aa").unwrap();
        fs::write(src.join("a/clash.txt"), b"bbbb").unwrap();

        let plan = plan_transfer(&src).unwrap();
        let before = plan.total_bytes;
        assert_eq!(before, 6);

        let conflicts: HashSet<PathBuf> =
            std::iter::once(PathBuf::from("proj/a/clash.txt")).collect();
        let skipped = apply_conflict_choice(plan.clone(), &conflicts, ConflictChoice::Skip);
        let names = relatives(&skipped);
        assert!(!names.iter().any(|name| name.ends_with("clash.txt")));
        assert!(names.iter().any(|name| name.ends_with("keep.txt")));
        assert!(
            names.contains(&"proj/a".to_string()),
            "the directory itself must survive so its kept children have a home"
        );
        assert_eq!(skipped.total_bytes, 2, "the total follows what remains");

        // Overwrite keeps everything untouched.
        let overwrite = apply_conflict_choice(plan, &conflicts, ConflictChoice::Overwrite);
        assert_eq!(overwrite.total_bytes, before);
    }
}
