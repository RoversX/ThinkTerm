//! Turning a remote directory into a flat list of things to download or
//! delete, before anything moves.
//!
//! Deliberately the remote sibling of [`super::transfer_walk`]: walking first
//! means the item total is known before the first byte (or unlink), every
//! problem the tree holds — a name the local system cannot store, a listing
//! too big to trust — surfaces as one refusal up front instead of a
//! half-processed tree, and the confirmation threshold has a number to ask
//! about. The walk itself is priced differently though: every directory here
//! costs a round trip on a single serialized channel, which is why its budget
//! is a tenth of the local one.

use super::remote_files::{
    RemoteFileBackend, RemoteFileKind, RemotePath, RemoteTransferProgress, REMOTE_TRANSFER_CANCELED,
};
use std::borrow::Cow;
use std::collections::{HashSet, VecDeque};
use std::path::{Component, Path};

/// Total entries a remote walk will accept before refusing. Each directory is
/// one round trip, so a tree at this limit already takes real time just to
/// list — anything bigger deserves a shell, not a sidebar.
pub(crate) const REMOTE_WALK_ENTRY_LIMIT: usize = 10_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RemoteWalkMode {
    /// Building a download: symlinks and special files are skipped and
    /// counted, matching what the local planner does on the way up.
    Download,
    /// Building a delete: symlinks and special files become unlink entries
    /// instead — leaving them behind would make every parent rmdir fail.
    Delete,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RemoteWalkEntry {
    pub path: RemotePath,
    /// This entry's name and those of its ancestors below the walk root, in
    /// order. Kept as separate components — never pre-joined — so the local
    /// side assembles them one at a time under its own path rules.
    pub components: Vec<String>,
    pub kind: RemoteFileKind,
    pub size: u64,
}

impl RemoteWalkEntry {
    pub(crate) fn is_directory(&self) -> bool {
        self.kind == RemoteFileKind::Directory
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct RemoteWalkPlan {
    /// Parents always precede their children, so a consumer can create as it
    /// goes — or delete as it goes by walking this in reverse.
    pub entries: Vec<RemoteWalkEntry>,
    /// Symlinks and special files deliberately left out (Download mode only),
    /// counted so the user can be told rather than silently losing them.
    pub skipped: usize,
    pub total_bytes: u64,
}

/// Whether a server-supplied name can be stored as exactly one path component
/// on THIS machine. Host semantics on purpose: `we\ird` is an ordinary Linux
/// file name and must stay downloadable there, while on Windows the same name
/// would split into two components (and `C:x` would grow a drive prefix) and
/// so is refused here, by name, before anything is written.
fn name_is_single_host_component(name: &str) -> bool {
    let mut components = Path::new(name).components();
    matches!(
        (components.next(), components.next()),
        (Some(Component::Normal(_)), None)
    )
}

/// Filename semantics of the destination that will materialize a download.
///
/// Kept explicit rather than hidden behind `cfg` so macOS and Windows
/// behavior can be exercised by the ordinary Linux test suite.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DownloadNameRules {
    Unix,
    MacOs,
    Windows,
}

impl DownloadNameRules {
    pub(crate) fn host() -> Self {
        if cfg!(target_os = "windows") {
            Self::Windows
        } else if cfg!(target_os = "macos") {
            // Standard APFS/HFS+ volumes preserve case but compare names
            // without it. Conservatively reject those collisions up front;
            // this can reject a valid tree on an explicitly case-sensitive
            // macOS volume, but never leaves a partial download on the
            // overwhelmingly common default.
            Self::MacOs
        } else {
            Self::Unix
        }
    }

    pub(crate) fn accepts(self, name: &str) -> bool {
        match self {
            Self::Unix | Self::MacOs => name_is_single_host_component(name),
            Self::Windows => windows_download_name_is_valid(name),
        }
    }

    /// `name` rewritten until [`Self::accepts`] takes it, borrowed unchanged
    /// when it already does -- for a destination whose name is incidental,
    /// like a download, where refusing the transfer over a character is
    /// worse than landing it under a near-miss of the server's name.
    ///
    /// Only the Windows rules rewrite anything. `:`, `?` and the rest are
    /// ordinary characters elsewhere, and renaming a file a user asked for
    /// by name is its own kind of wrong.
    ///
    /// Expects a single component: run it after the name has been reduced to
    /// a basename, never before, or `/` and `\` are rewritten into the name
    /// instead of splitting it and the two platforms land different files.
    pub(crate) fn sanitize(self, name: &str) -> Cow<'_, str> {
        match self {
            Self::Unix | Self::MacOs => Cow::Borrowed(name),
            Self::Windows => windows_download_name(name),
        }
    }

    fn collision_key(self, name: &str) -> Option<String> {
        match self {
            Self::Unix => None,
            // A conservative Unicode fold catches the common APFS/HFS+ and
            // NTFS collision class without making Linux downloads
            // case-insensitive.
            Self::MacOs | Self::Windows => {
                Some(name.chars().flat_map(char::to_uppercase).collect())
            }
        }
    }
}

/// A character Win32 refuses in a path component, plus the control range.
fn windows_reserved_char(ch: char) -> bool {
    ch <= '\u{1f}' || matches!(ch, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*')
}

/// Whether the name's stem is a reserved device. They remain reserved with
/// an extension (`CON.txt` is `CON`), and Windows also recognizes the
/// ISO-8859-1 superscript forms of COM/LPT 1-3.
fn windows_reserved_stem(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or(name).to_uppercase();
    matches!(
        stem.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) || ["COM", "LPT"].iter().any(|prefix| {
        stem.strip_prefix(prefix).is_some_and(|suffix| {
            matches!(
                suffix,
                "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
            )
        })
    })
}

fn windows_download_name_is_valid(name: &str) -> bool {
    !name.is_empty()
        && !matches!(name, "." | "..")
        && !name.ends_with([' ', '.'])
        && name.encode_utf16().count() <= 255
        && !name.chars().any(windows_reserved_char)
        && !windows_reserved_stem(name)
}

/// Drop the trailing dots and spaces Windows would drop anyway. Left in
/// place they are how one name silently becomes another: `report.` and
/// `report` are the same file, and only one of them is what was asked for.
fn trim_trailing_dots_and_spaces(name: &mut String) {
    while name.ends_with([' ', '.']) {
        name.pop();
    }
}

/// `name` rewritten until [`windows_download_name_is_valid`] takes it.
///
/// Reserved characters become `_`, a reserved device stem is pushed out of
/// the way with a prefix, and the length is cut on a character boundary.
/// A name that survives none of that is `download`.
fn windows_download_name(name: &str) -> Cow<'_, str> {
    if windows_download_name_is_valid(name) {
        return Cow::Borrowed(name);
    }
    let mut out: String = name
        .chars()
        .map(|ch| if windows_reserved_char(ch) { '_' } else { ch })
        .collect();
    trim_trailing_dots_and_spaces(&mut out);
    // Length before the device check, not after. The limit counts UTF-16
    // units, so the cut walks characters rather than slicing bytes, and the
    // trim has to run again because the cut can expose a dot or a space --
    // which is exactly how the stem changes underneath: `CON` followed by
    // 252 spaces is not a device name until the cut and the trim have taken
    // the spaces away, and then it is.
    cut_to_255_utf16_units(&mut out);
    if windows_reserved_stem(&out) {
        out.insert(0, '_');
        // The prefix can put it back over the limit. Cutting again is safe
        // to do once: the stem now begins with `_`, so no amount of taking
        // from the end can make it a device name a second time.
        cut_to_255_utf16_units(&mut out);
    }
    if out.is_empty() {
        out.push_str("download");
    }
    Cow::Owned(out)
}

fn cut_to_255_utf16_units(name: &mut String) {
    if name.encode_utf16().count() <= 255 {
        return;
    }
    let mut units = 0usize;
    let mut end = name.len();
    for (index, ch) in name.char_indices() {
        if units + ch.len_utf16() > 255 {
            end = index;
            break;
        }
        units += ch.len_utf16();
    }
    name.truncate(end);
    trim_trailing_dots_and_spaces(name);
}

/// Walk `root` into a flat, parents-first plan.
///
/// Refusals are total: a listing that comes back truncated, a tree over the
/// entry budget, or (for downloads) a name the local system cannot store all
/// fail the whole plan rather than shrinking it. A partial plan silently
/// executed is exactly the "reported success while omitting data" outcome the
/// planning step exists to prevent.
pub(crate) async fn plan_remote_walk(
    backend: &dyn RemoteFileBackend,
    root: RemotePath,
    mode: RemoteWalkMode,
    progress: &RemoteTransferProgress,
) -> Result<RemoteWalkPlan, String> {
    plan_remote_walk_with_rules(backend, root, mode, progress, DownloadNameRules::host()).await
}

async fn plan_remote_walk_with_rules(
    backend: &dyn RemoteFileBackend,
    root: RemotePath,
    mode: RemoteWalkMode,
    progress: &RemoteTransferProgress,
    name_rules: DownloadNameRules,
) -> Result<RemoteWalkPlan, String> {
    let mut plan = RemoteWalkPlan::default();
    let mut pending: VecDeque<(RemotePath, Vec<String>)> = VecDeque::new();
    if mode == RemoteWalkMode::Download
        && !root.file_name().is_empty()
        && !name_rules.accepts(root.file_name())
    {
        return Err(cannot_store_locally(&root));
    }
    pending.push_back((root, Vec::new()));

    while let Some((dir, prefix)) = pending.pop_front() {
        // Per-directory rather than per-entry: one round trip is also the
        // granularity at which a cancel can actually take effect.
        if progress.is_canceled() {
            return Err(REMOTE_TRANSFER_CANCELED.to_string());
        }
        // Skipped entries cost the same round trip as planned ones, so they
        // spend the budget too: counting only `entries` would let a tree of
        // symlinks list millions of items while still claiming to stop at the
        // advertised limit.
        let seen = plan.entries.len().saturating_add(plan.skipped);
        let remaining = REMOTE_WALK_ENTRY_LIMIT.saturating_sub(seen);
        // One extra so "exactly filled the budget" and "went past it" are
        // distinguishable without trusting the truncation flag alone.
        let listing = backend
            .list_directory(dir, remaining.saturating_add(1))
            .await?;
        if listing.truncated || listing.entries.len() > remaining {
            return Err(format!(
                "That folder holds more than {REMOTE_WALK_ENTRY_LIMIT} items"
            ));
        }
        let mut materialized_names = HashSet::new();
        for entry in listing.entries {
            let mut components = prefix.clone();
            components.push(entry.name.clone());
            let materializes =
                matches!(entry.kind, RemoteFileKind::Directory | RemoteFileKind::File);
            if mode == RemoteWalkMode::Download && materializes {
                if !name_rules.accepts(&entry.name) {
                    return Err(cannot_store_locally(&entry.path));
                }
                if let Some(key) = name_rules.collision_key(&entry.name) {
                    if !materialized_names.insert(key) {
                        return Err(format!(
                            "The names in {} collide on this computer",
                            entry
                                .path
                                .parent()
                                .unwrap_or_else(|| entry.path.clone())
                                .as_str()
                        ));
                    }
                }
            }
            match entry.kind {
                RemoteFileKind::Directory => {
                    pending.push_back((entry.path.clone(), components.clone()));
                    plan.entries.push(RemoteWalkEntry {
                        path: entry.path,
                        components,
                        kind: RemoteFileKind::Directory,
                        size: 0,
                    });
                }
                RemoteFileKind::File => {
                    let size = entry.size.unwrap_or(0);
                    plan.total_bytes = plan.total_bytes.saturating_add(size);
                    plan.entries.push(RemoteWalkEntry {
                        path: entry.path,
                        components,
                        kind: RemoteFileKind::File,
                        size,
                    });
                }
                RemoteFileKind::Symlink | RemoteFileKind::Other => match mode {
                    RemoteWalkMode::Download => plan.skipped += 1,
                    // No local name is ever built for these, so no host name
                    // check: the delete talks only in remote paths.
                    RemoteWalkMode::Delete => plan.entries.push(RemoteWalkEntry {
                        path: entry.path,
                        components,
                        kind: entry.kind,
                        size: 0,
                    }),
                },
            }
        }
    }
    Ok(plan)
}

fn cannot_store_locally(path: &RemotePath) -> String {
    format!(
        "The name of {} cannot be used on this computer",
        path.as_str()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::termwindow::remote_files::{
        RemoteDirectoryListing, RemoteFileBytes, RemoteFileEntry,
    };
    use std::collections::HashMap;
    use std::future::Future;
    use std::pin::Pin;

    type RemoteFuture<T> = Pin<Box<dyn Future<Output = Result<T, String>> + Send + 'static>>;

    /// Serves listings from a map; anything absent is an empty directory.
    struct MapBackend {
        directories: HashMap<String, Vec<RemoteFileEntry>>,
        truncate_all: bool,
    }

    impl MapBackend {
        fn new(directories: &[(&str, Vec<RemoteFileEntry>)]) -> Self {
            Self {
                directories: directories
                    .iter()
                    .map(|(path, entries)| ((*path).to_string(), entries.clone()))
                    .collect(),
                truncate_all: false,
            }
        }
    }

    fn entry(parent: &str, name: &str, kind: RemoteFileKind, size: u64) -> RemoteFileEntry {
        let parent = RemotePath::from_server_absolute(parent).unwrap();
        RemoteFileEntry {
            path: parent.join_name(name).unwrap(),
            name: name.to_string(),
            kind,
            size: Some(size),
        }
    }

    impl RemoteFileBackend for MapBackend {
        fn resolve_root(&self, _requested: String) -> RemoteFuture<RemotePath> {
            Box::pin(async { RemotePath::from_server_absolute("/srv") })
        }

        fn list_directory(
            &self,
            path: RemotePath,
            limit: usize,
        ) -> RemoteFuture<RemoteDirectoryListing> {
            let mut entries = self
                .directories
                .get(path.as_str())
                .cloned()
                .unwrap_or_default();
            let mut truncated = self.truncate_all;
            if entries.len() > limit {
                entries.truncate(limit);
                truncated = true;
            }
            Box::pin(async move { Ok(RemoteDirectoryListing { entries, truncated }) })
        }

        fn read_file(&self, _path: RemotePath, _limit: usize) -> RemoteFuture<RemoteFileBytes> {
            Box::pin(async {
                Ok(RemoteFileBytes {
                    bytes: Vec::new(),
                    truncated: false,
                })
            })
        }
    }

    fn root() -> RemotePath {
        RemotePath::from_server_absolute("/srv/proj").unwrap()
    }

    #[test]
    fn a_tree_walks_into_a_parents_first_component_list() {
        let backend = MapBackend::new(&[
            (
                "/srv/proj",
                vec![
                    entry("/srv/proj", "src", RemoteFileKind::Directory, 0),
                    entry("/srv/proj", "top.txt", RemoteFileKind::File, 5),
                ],
            ),
            (
                "/srv/proj/src",
                vec![entry("/srv/proj/src", "lib.rs", RemoteFileKind::File, 7)],
            ),
        ]);
        let progress = RemoteTransferProgress::default();
        let plan = smol::block_on(plan_remote_walk(
            &backend,
            root(),
            RemoteWalkMode::Download,
            &progress,
        ))
        .expect("plan");

        let flattened: Vec<String> = plan
            .entries
            .iter()
            .map(|entry| entry.components.join("/"))
            .collect();
        assert_eq!(flattened, vec!["src", "top.txt", "src/lib.rs"]);
        assert_eq!(plan.total_bytes, 12);
        let files = plan
            .entries
            .iter()
            .filter(|entry| !entry.is_directory())
            .count();
        assert_eq!(files, 2);
        // Every directory precedes everything inside it.
        for (index, name) in flattened.iter().enumerate() {
            if let Some((parent, _)) = name.rsplit_once('/') {
                let parent_index = flattened.iter().position(|other| other == parent);
                assert!(
                    parent_index.is_some_and(|parent_index| parent_index < index),
                    "{} must precede {} in {:?}",
                    parent,
                    name,
                    flattened
                );
            }
        }
    }

    #[test]
    fn download_mode_skips_links_but_delete_mode_unlinks_them() {
        let backend = MapBackend::new(&[(
            "/srv/proj",
            vec![
                entry("/srv/proj", "real.txt", RemoteFileKind::File, 1),
                entry("/srv/proj", "link", RemoteFileKind::Symlink, 0),
                entry("/srv/proj", "socket", RemoteFileKind::Other, 0),
            ],
        )]);
        let progress = RemoteTransferProgress::default();

        let download = smol::block_on(plan_remote_walk(
            &backend,
            root(),
            RemoteWalkMode::Download,
            &progress,
        ))
        .expect("download plan");
        assert_eq!(download.entries.len(), 1, "only the real file is planned");
        assert_eq!(download.skipped, 2);

        // A delete that skipped them would strand every parent rmdir.
        let delete = smol::block_on(plan_remote_walk(
            &backend,
            root(),
            RemoteWalkMode::Delete,
            &progress,
        ))
        .expect("delete plan");
        assert_eq!(delete.entries.len(), 3);
        assert_eq!(delete.skipped, 0);
    }

    #[test]
    fn a_truncated_listing_fails_the_whole_plan() {
        let mut backend = MapBackend::new(&[(
            "/srv/proj",
            vec![entry("/srv/proj", "a.txt", RemoteFileKind::File, 1)],
        )]);
        backend.truncate_all = true;
        let progress = RemoteTransferProgress::default();
        let err = smol::block_on(plan_remote_walk(
            &backend,
            root(),
            RemoteWalkMode::Download,
            &progress,
        ))
        .expect_err("an incomplete listing must not become a partial plan");
        assert!(err.contains("more than"), "{}", err);
    }

    #[test]
    fn a_tree_over_the_entry_budget_is_refused() {
        let wide: Vec<RemoteFileEntry> = (0..REMOTE_WALK_ENTRY_LIMIT + 1)
            .map(|index| {
                entry(
                    "/srv/proj",
                    &format!("file-{index}"),
                    RemoteFileKind::File,
                    1,
                )
            })
            .collect();
        let backend = MapBackend::new(&[("/srv/proj", wide)]);
        let progress = RemoteTransferProgress::default();
        let err = smol::block_on(plan_remote_walk(
            &backend,
            root(),
            RemoteWalkMode::Download,
            &progress,
        ))
        .expect_err("over budget must refuse");
        assert!(
            err.contains(&REMOTE_WALK_ENTRY_LIMIT.to_string()),
            "{}",
            err
        );
    }

    #[test]
    fn skipped_entries_still_consume_the_walk_budget() {
        // Two directories of symlinks: a download plans none of them, but each
        // one still cost a round trip, and together they are over the budget.
        // Counting only what lands in the plan would let this walk run for as
        // long as the tree is wide while still claiming a 10,000-item stop.
        let half = REMOTE_WALK_ENTRY_LIMIT / 2;
        let links = |parent: &str| -> Vec<RemoteFileEntry> {
            (0..half)
                .map(|index| entry(parent, &format!("link-{index}"), RemoteFileKind::Symlink, 0))
                .collect()
        };
        let backend = MapBackend::new(&[
            (
                "/srv/proj",
                vec![
                    entry("/srv/proj", "a", RemoteFileKind::Directory, 0),
                    entry("/srv/proj", "b", RemoteFileKind::Directory, 0),
                ],
            ),
            ("/srv/proj/a", links("/srv/proj/a")),
            ("/srv/proj/b", links("/srv/proj/b")),
        ]);
        let progress = RemoteTransferProgress::default();
        let err = smol::block_on(plan_remote_walk(
            &backend,
            root(),
            RemoteWalkMode::Download,
            &progress,
        ))
        .expect_err("skipped entries must spend the budget too");
        assert!(
            err.contains(&REMOTE_WALK_ENTRY_LIMIT.to_string()),
            "{}",
            err
        );
    }

    #[test]
    fn a_canceled_walk_stops_before_listing() {
        let backend = MapBackend::new(&[(
            "/srv/proj",
            vec![entry("/srv/proj", "a.txt", RemoteFileKind::File, 1)],
        )]);
        let progress = RemoteTransferProgress::default();
        progress.request_cancel();
        let err = smol::block_on(plan_remote_walk(
            &backend,
            root(),
            RemoteWalkMode::Download,
            &progress,
        ))
        .expect_err("canceled");
        assert_eq!(err, REMOTE_TRANSFER_CANCELED);
    }

    /// Host semantics, pinned for the Unix half: a backslash is an ordinary
    /// file name character here and must stay downloadable. The Windows
    /// rejection branch of `name_is_single_host_component` cannot execute on
    /// this platform — it is covered by the cross-compile check instead.
    #[cfg(unix)]
    #[test]
    fn a_backslash_name_stays_downloadable_on_unix() {
        let backend = MapBackend::new(&[(
            "/srv/proj",
            vec![entry("/srv/proj", r"we\ird.txt", RemoteFileKind::File, 1)],
        )]);
        let progress = RemoteTransferProgress::default();
        let plan = smol::block_on(plan_remote_walk(
            &backend,
            root(),
            RemoteWalkMode::Download,
            &progress,
        ))
        .expect("host semantics accept it");
        assert_eq!(plan.entries.len(), 1);
        assert!(name_is_single_host_component(r"we\ird.txt"));
    }

    #[test]
    fn windows_rejects_names_that_cannot_be_created() {
        for name in [
            "report?.txt",
            "foo:bar",
            "bad<name",
            "bad|name",
            "trailing.",
            "trailing ",
            "CON",
            "con.txt",
            "AUX.log",
            "COM1",
            "LPT9.data",
            "COM¹",
            "CONIN$",
        ] {
            assert!(
                !windows_download_name_is_valid(name),
                "{:?} must be rejected",
                name
            );
        }
        for name in ["report.txt", "console.txt", "COM10", "LPT0", "résumé.md"] {
            assert!(
                windows_download_name_is_valid(name),
                "{:?} should remain usable",
                name
            );
        }
    }

    /// The invariant the download path leans on: whatever `sanitize` returns
    /// is a name the same rules accept. Without it the edges are easy to get
    /// half-right -- a trailing dot removed but the reserved stem it exposes
    /// left alone, a length cut that re-exposes a dot.
    #[test]
    fn sanitizing_a_windows_name_always_yields_one_windows_accepts() {
        let long = "a".repeat(300);
        let long_tail = format!("{}.", "b".repeat(255));
        let wide = "😀".repeat(200);
        // Over the limit, and what the cut leaves behind once its trailing
        // spaces go is a device name again.
        let device_after_cut = format!("CON{}abc", " ".repeat(252));
        let port_after_cut = format!("COM1{}x", " ".repeat(300));
        for name in [
            "report:2024.txt",
            "a<b>c|d?e*f\"g",
            "trailing.",
            "trailing ",
            "CON",
            "con.txt",
            "AUX.log",
            "COM1",
            "LPT9.data",
            "COM¹",
            "CONIN$",
            "CON.",
            "..",
            ".",
            "....",
            ".. ",
            "",
            "\u{1}\u{2}",
            &long,
            &long_tail,
            &wide,
            &device_after_cut,
            &port_after_cut,
        ] {
            let sanitized = DownloadNameRules::Windows.sanitize(name);
            assert!(
                DownloadNameRules::Windows.accepts(&sanitized),
                "sanitize({:?}) produced {:?}, which is still refused",
                name,
                sanitized
            );
        }
        // A name that is already fine is handed back untouched, not copied.
        assert!(matches!(
            DownloadNameRules::Windows.sanitize("report.txt"),
            Cow::Borrowed("report.txt")
        ));
        // Elsewhere the name is the user's, and stays theirs.
        assert_eq!(
            DownloadNameRules::Unix.sanitize("report:2024.txt"),
            "report:2024.txt"
        );
    }

    #[test]
    fn case_insensitive_targets_reject_collisions_before_download() {
        let backend = MapBackend::new(&[(
            "/srv/proj",
            vec![
                entry("/srv/proj", "Readme.md", RemoteFileKind::File, 1),
                entry("/srv/proj", "README.MD", RemoteFileKind::File, 1),
            ],
        )]);

        for rules in [DownloadNameRules::MacOs, DownloadNameRules::Windows] {
            let err = smol::block_on(plan_remote_walk_with_rules(
                &backend,
                root(),
                RemoteWalkMode::Download,
                &RemoteTransferProgress::default(),
                rules,
            ))
            .expect_err("case variants cannot share a default macOS or Windows directory");
            assert!(err.contains("collide"), "{}", err);
        }

        let unix = smol::block_on(plan_remote_walk_with_rules(
            &backend,
            root(),
            RemoteWalkMode::Download,
            &RemoteTransferProgress::default(),
            DownloadNameRules::Unix,
        ))
        .expect("Unix keeps case-distinct names");
        assert_eq!(unix.entries.len(), 2);
    }

    #[test]
    fn windows_validates_the_download_root_before_listing() {
        let backend = MapBackend::new(&[]);
        let progress = RemoteTransferProgress::default();
        let root = RemotePath::from_server_absolute("/srv/CON").unwrap();
        let err = smol::block_on(plan_remote_walk_with_rules(
            &backend,
            root,
            RemoteWalkMode::Download,
            &progress,
            DownloadNameRules::Windows,
        ))
        .expect_err("a reserved root name must fail before destination creation");
        assert!(err.contains("/srv/CON"), "{}", err);
    }
}
