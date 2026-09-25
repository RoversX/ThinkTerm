use crate::workspace_threads::{RemoteFilesSource, RemoteFilesTarget};
use config::SshDomain;
use smol::channel::Sender;
use smol::io::{AsyncReadExt, AsyncWriteExt};
use std::borrow::Cow;
use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};
use wezterm_ssh::{Session, SessionEvent, SftpChannelError, SftpError};

pub(crate) const REMOTE_FILE_TREE_ROW_LIMIT: usize = 2_000;

type RemoteFuture<T> = Pin<Box<dyn Future<Output = Result<T, String>> + Send + 'static>>;

/// A remote Unix path.  It never uses the host OS path implementation, so a
/// Windows client cannot introduce `\` while joining SFTP paths.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RemotePath(String);

impl RemotePath {
    pub(crate) fn from_server_absolute(path: &str) -> Result<Self, String> {
        if !path.starts_with('/') {
            return Err(format!("SFTP returned a non-absolute path: {path}"));
        }
        if path.as_bytes().contains(&0) {
            return Err("remote path contains NUL".to_string());
        }
        let normalized = if path.len() > 1 {
            path.trim_end_matches('/').to_string()
        } else {
            "/".to_string()
        };
        Ok(Self(normalized))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }

    pub(crate) fn file_name(&self) -> &str {
        self.0.rsplit('/').next().unwrap_or(&self.0)
    }

    /// The directory holding this path, or `None` at the root — which has no
    /// parent rather than being its own.
    pub(crate) fn parent(&self) -> Option<Self> {
        if self.0 == "/" {
            return None;
        }
        let (parent, _) = self.0.rsplit_once('/')?;
        Some(Self(if parent.is_empty() {
            "/".to_string()
        } else {
            parent.to_string()
        }))
    }

    pub(crate) fn extension(&self) -> Option<&str> {
        let name = self.file_name();
        let (stem, extension) = name.rsplit_once('.')?;
        (!stem.is_empty() && !extension.is_empty()).then_some(extension)
    }

    pub(crate) fn join_name(&self, name: &str) -> Result<Self, String> {
        if name.is_empty()
            || matches!(name, "." | "..")
            || name.contains('/')
            || name.as_bytes().contains(&0)
        {
            return Err(format!("invalid SFTP directory entry name: {name:?}"));
        }
        let joined = if self.0 == "/" {
            format!("/{name}")
        } else {
            format!("{}/{name}", self.0)
        };
        Ok(Self(joined))
    }

    pub(crate) fn is_descendant_of(&self, parent: &Self) -> bool {
        if parent.0 == "/" {
            self.0.len() > 1 && self.0.starts_with('/')
        } else {
            self.0
                .strip_prefix(&parent.0)
                .is_some_and(|suffix| suffix.starts_with('/'))
        }
    }
}

/// Whether `requested` needs the remote home resolved before it can be used.
/// Absolute roots do not: asking the server to canonicalize `.` first is both
/// pointless and a needless failure point, since a server that cannot answer
/// that request would break a project whose path was fully specified.
fn requested_root_needs_home(requested: &str) -> bool {
    requested == "~" || requested.starts_with("~/")
}

/// The spellings a server may accept when asked to canonicalize the session's
/// starting directory. SFTP has no `~`; `.` is the usual spelling, but not
/// every server answers it, so the empty path is tried before giving up.
/// The liveness probe MUST accept the same spellings as root resolution — a
/// stricter probe declares sessions dead that the panel could actually use,
/// forcing a needless redial on every idle reacquire.
const REMOTE_HOME_SPELLINGS: [&str; 2] = [".", ""];

/// Try each home spelling in order, returning the first success or every
/// failure (in spelling order). Generic over the canonicalize call so the
/// fallback contract stays testable without a server.
async fn canonicalize_remote_home<T, E, F, Fut>(mut canonicalize: F) -> Result<T, Vec<E>>
where
    F: FnMut(&'static str) -> Fut,
    Fut: std::future::Future<Output = Result<T, E>>,
{
    let mut failures = Vec::new();
    for spelling in REMOTE_HOME_SPELLINGS {
        match canonicalize(spelling).await {
            Ok(value) => return Ok(value),
            Err(err) => failures.push(err),
        }
    }
    Err(failures)
}

/// Validate a stored project path for use as a browse root. Kept separate from
/// [`RemotePath::from_server_absolute`] so the diagnostics distinguish "the
/// server answered with something odd" from "this project's stored path cannot
/// be browsed", and so traversal components never reach the server.
fn remote_root_from_absolute(requested: &str) -> Result<RemotePath, String> {
    if !requested.starts_with('/') {
        return Err(format!(
            "Unsupported remote project path {requested:?}: expected an absolute path or ~"
        ));
    }
    if requested.split('/').any(|component| component == "..") {
        return Err(format!(
            "Unsupported remote project path {requested:?}: `..` is not allowed"
        ));
    }
    RemotePath::from_server_absolute(requested)
}

/// Flatten an error and everything it wraps into one line. libssh and ssh2
/// failures surface through `SftpChannelError` as the near-useless
/// "Library-specific error", with the real reason one or more levels down.
fn error_chain(err: &(dyn std::error::Error + 'static)) -> String {
    let mut parts = vec![err.to_string()];
    let mut source = err.source();
    while let Some(inner) = source {
        parts.push(inner.to_string());
        source = inner.source();
    }
    parts.join(": ")
}

/// The deepest, most specific message in the chain — the part worth putting in
/// front of the user when the full chain is too long for a sidebar.
fn error_summary(err: &(dyn std::error::Error + 'static)) -> String {
    let mut deepest = err.to_string();
    let mut source = err.source();
    while let Some(inner) = source {
        let text = inner.to_string();
        if !text.is_empty() {
            deepest = text;
        }
        source = inner.source();
    }
    deepest
}

fn resolve_requested_root(home: RemotePath, requested: &str) -> Result<RemotePath, String> {
    if requested == "~" {
        return Ok(home);
    }
    if let Some(suffix) = requested.strip_prefix("~/") {
        let mut path = home;
        for component in suffix.split('/') {
            if component.is_empty() || component == "." {
                continue;
            }
            path = path.join_name(component)?;
        }
        return Ok(path);
    }
    RemotePath::from_server_absolute(requested)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RemoteFileKind {
    Directory,
    File,
    Symlink,
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RemoteFileEntry {
    pub path: RemotePath,
    pub name: String,
    pub kind: RemoteFileKind,
    pub size: Option<u64>,
}

impl RemoteFileEntry {
    pub(crate) fn is_directory(&self) -> bool {
        self.kind == RemoteFileKind::Directory
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RemoteDirectoryListing {
    pub entries: Vec<RemoteFileEntry>,
    pub truncated: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RemoteFileBytes {
    pub bytes: Vec<u8>,
    pub truncated: bool,
}

/// Bytes moved per round trip. SFTP is request/response over a single
/// channel, so a bigger chunk means fewer round trips; this matches the read
/// path's buffer and still leaves a cancel responsive on a slow link.
pub(crate) const REMOTE_TRANSFER_CHUNK: usize = 256 * 1024;

/// Stands in for "we do not know the size yet". A real file cannot be this
/// large, and it keeps an empty file (a genuine total of 0) distinguishable
/// from a size that has not been established.
const TRANSFER_TOTAL_UNKNOWN: u64 = u64::MAX;

/// Shared, lock-free view of one transfer: the worker publishes progress, the
/// UI reads it every paint, and either side can ask to stop.
///
/// Tracks two scales at once. A single file reports bytes; a folder reports
/// items, because a thousand-file tree's byte total says nothing useful about
/// how far along it looks, and because the per-file byte counter would have to
/// be reset constantly. Whichever scale is populated is the one the UI shows.
#[derive(Clone, Debug)]
pub(crate) struct RemoteTransferProgress {
    transferred: Arc<AtomicU64>,
    total: Arc<AtomicU64>,
    items_done: Arc<AtomicU64>,
    items_total: Arc<AtomicU64>,
    cancel: Arc<AtomicBool>,
}

impl Default for RemoteTransferProgress {
    fn default() -> Self {
        Self {
            transferred: Arc::new(AtomicU64::new(0)),
            total: Arc::new(AtomicU64::new(TRANSFER_TOTAL_UNKNOWN)),
            items_done: Arc::new(AtomicU64::new(0)),
            items_total: Arc::new(AtomicU64::new(TRANSFER_TOTAL_UNKNOWN)),
            cancel: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl RemoteTransferProgress {
    pub(crate) fn transferred(&self) -> u64 {
        self.transferred.load(Ordering::Relaxed)
    }

    pub(crate) fn total(&self) -> Option<u64> {
        match self.total.load(Ordering::Relaxed) {
            TRANSFER_TOTAL_UNKNOWN => None,
            total => Some(total),
        }
    }

    fn set_total(&self, total: u64) {
        // Clamp so a pathological size cannot be mistaken for "unknown".
        self.total
            .store(total.min(TRANSFER_TOTAL_UNKNOWN - 1), Ordering::Relaxed);
    }

    fn advance(&self, bytes: u64) {
        self.transferred.fetch_add(bytes, Ordering::Relaxed);
    }

    pub(crate) fn items(&self) -> Option<(u64, u64)> {
        match self.items_total.load(Ordering::Relaxed) {
            TRANSFER_TOTAL_UNKNOWN => None,
            total => Some((self.items_done.load(Ordering::Relaxed), total)),
        }
    }

    /// Switch this transfer to item-scale reporting. Called once, after the
    /// source tree has been walked and the count is actually known.
    pub(crate) fn set_item_total(&self, total: u64) {
        self.items_total
            .store(total.min(TRANSFER_TOTAL_UNKNOWN - 1), Ordering::Relaxed);
    }

    pub(crate) fn finish_item(&self) {
        self.items_done.fetch_add(1, Ordering::Relaxed);
    }

    /// Per-file byte counters are meaningless once a folder is in flight: the
    /// aggregate is the item count, and letting each file overwrite the byte
    /// total would make the bar jump backwards on every file.
    fn tracks_items(&self) -> bool {
        self.items_total.load(Ordering::Relaxed) != TRANSFER_TOTAL_UNKNOWN
    }

    pub(crate) fn request_cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    pub(crate) fn is_canceled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    /// How far along, for a progress bar. `None` while the size is unknown;
    /// an empty file is complete the moment it is created.
    pub(crate) fn fraction(&self) -> Option<f32> {
        if let Some((done, total)) = self.items() {
            if total == 0 {
                return Some(1.0);
            }
            return Some((done as f64 / total as f64).clamp(0.0, 1.0) as f32);
        }
        let total = self.total()?;
        if total == 0 {
            return Some(1.0);
        }
        Some((self.transferred() as f64 / total as f64).clamp(0.0, 1.0) as f32)
    }
}

/// Reported when a transfer stops because the user asked it to, so callers can
/// tell "you cancelled this" apart from a real failure.
pub(crate) const REMOTE_TRANSFER_CANCELED: &str = "Canceled";

/// Why a transfer stopped, and whether it managed to tidy up.
///
/// A plain error string cannot express "and there is now a half-written file
/// at this path" — which is exactly the case the user has to be told about,
/// because it usually happens when the connection died and took the cleanup
/// with it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TransferFailure {
    pub message: String,
    /// Set when something incomplete was left behind and could not be removed.
    pub leftover: Option<String>,
}

impl TransferFailure {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            leftover: None,
        }
    }
}

/// Transfers report richer failures than the rest of the backend, so they get
/// their own future type rather than widening every method's error.
type TransferFuture = Pin<Box<dyn Future<Output = Result<u64, TransferFailure>> + Send + 'static>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RemoteTransferKind {
    Upload,
    Download,
    /// Removing a remote folder tree. Rides the transfer strip because it has
    /// the same shape — many round trips, progress, a cancel — even though no
    /// bytes move.
    Delete,
    /// Copying between two local directories — no connection involved, but it
    /// wants the same progress row, cancel and retry as the remote ones.
    LocalCopy,
}

impl RemoteTransferKind {
    pub(crate) fn verb(self) -> &'static str {
        match self {
            Self::Upload => "Uploading",
            Self::Download => "Downloading",
            Self::Delete => "Deleting",
            Self::LocalCopy => "Copying",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RemoteTransferStatus {
    Running,
    /// Finished, carrying whatever is worth telling the user afterwards —
    /// for a download, where the file landed.
    Done(String),
    Failed(String),
    /// Failed *and* could not clean up after itself, so something incomplete
    /// is still sitting there. Separate from `Failed` because the user has to
    /// be told where, rather than finding it later and wondering.
    FailedWithLeftover {
        message: String,
        leftover: String,
    },
}

/// What a transfer needs in order to be run again without the user redoing the
/// drag. Kept on the record from the start: retrofitting it after the futures
/// have already consumed the paths is far more invasive.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RemoteTransferSource {
    Upload {
        local: PathBuf,
        remote: RemotePath,
        directory: RemotePath,
    },
    Download {
        remote: RemotePath,
    },
    /// A whole remote directory. Retrying re-walks and re-reserves from
    /// scratch — the tree may have changed, and the old reservation is a
    /// half-written folder the retry must not resume into.
    DownloadFolder {
        remote: RemotePath,
    },
}

/// The Files source an operation was created against.
///
/// Remote paths are only meaningful together with this identity: `/tmp/a` on
/// host A is not the same object as `/tmp/a` on host B.  Keep the source on
/// work that can outlive the currently displayed tree (transfers, confirmation
/// menus and inline edits), then revalidate it immediately before execution.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RemoteOperationOrigin {
    source_key: String,
    connection_key: String,
}

impl RemoteOperationOrigin {
    pub(crate) fn new(source_key: String, connection_key: String) -> Self {
        Self {
            source_key,
            connection_key,
        }
    }

    pub(crate) fn source_key(&self) -> &str {
        &self.source_key
    }

    pub(crate) fn connection_key(&self) -> &str {
        &self.connection_key
    }

    pub(crate) fn matches(&self, source_key: Option<&str>, connection_key: Option<&str>) -> bool {
        source_key == Some(self.source_key()) && connection_key == Some(self.connection_key())
    }
}

/// One transfer in flight, or one that just finished and still has something
/// to report. Lives on the window rather than in [`RemoteFilesState`] because
/// a transfer holds its own lease and must survive the panel switching trees.
#[derive(Clone, Debug)]
pub(crate) struct RemoteTransfer {
    pub id: u64,
    pub kind: RemoteTransferKind,
    pub name: String,
    pub progress: RemoteTransferProgress,
    pub status: RemoteTransferStatus,
    /// Enough to retry. `None` for records that were never a real transfer —
    /// a rejected folder, say — which have nothing to retry.
    pub source: Option<RemoteTransferSource>,
    /// Host identity for remote work. Local copies deliberately leave this
    /// empty; every retryable remote transfer must have both `source` and
    /// `origin`.
    pub origin: Option<RemoteOperationOrigin>,
}

impl RemoteTransfer {
    pub(crate) fn is_running(&self) -> bool {
        matches!(self.status, RemoteTransferStatus::Running)
    }

    pub(crate) fn can_retry(&self) -> bool {
        !self.is_running() && self.source.is_some() && self.origin.is_some()
    }
}

/// How many ` (n)` variants to try before giving up on finding a free name.
const DOWNLOAD_NAME_ATTEMPTS: u32 = 1_000;

/// Reduce a server-supplied name to something that can only ever name a file
/// *inside* the download directory.
///
/// Uses `Path::file_name` rather than splitting on separators by hand, so the
/// host's own path rules apply — which is what catches a Windows drive-
/// relative name like `C:evil.txt`, where there is no separator to split on
/// but `directory.join(..)` would still escape.
fn sanitized_download_name(file_name: &str) -> &str {
    Path::new(file_name)
        .file_name()
        .and_then(|name| name.to_str())
        // `.`/`..`/`/` yield no file name at all; anything else that survives
        // is a single component by construction.
        .filter(|name| !name.is_empty())
        .unwrap_or("download")
}

/// A server-supplied name reduced to something this host can store: a single
/// component that the host's filename rules then accept. Unchanged off
/// Windows, where those rules are the identity.
///
/// The order is load-bearing. `/` and `\` are reserved *characters* under the
/// Windows rules, so sanitizing before taking the basename would rewrite
/// `../../etc/passwd` into one long name on Windows while Linux still landed
/// `passwd` -- one server, two platforms, two different files. Taking the
/// basename first keeps them together, and hands `extension_split_index` a
/// string whose bytes will not shift under it.
///
/// Deliberately not folded into `sanitized_download_name`, and so not into
/// `download_name_candidates`: that is also how an *upload* picks a free name
/// on the remote server, where this host's rules have no business.
fn host_download_name(name: &str) -> Cow<'_, str> {
    crate::termwindow::remote_walk::DownloadNameRules::host()
        .sanitize(sanitized_download_name(name))
}

/// Where a name's extension begins, for collision numbering. The LAST dot,
/// not the first: dots inside a stem are ordinary characters (a macOS
/// screenshot is `… at 12.50.59 PM.png`, and numbering it at the first dot
/// produced `… at 12 (1).50.59 PM.png` — verified live). Known compound
/// archive extensions are the exception and stay whole; a leading dot is a
/// hidden file, not an extension.
fn extension_split_index(name: &str) -> usize {
    const COMPOUND: &[&str] = &[".tar.gz", ".tar.bz2", ".tar.xz", ".tar.zst"];
    let lower = name.to_ascii_lowercase();
    for suffix in COMPOUND {
        if lower.ends_with(suffix) && name.len() > suffix.len() {
            return name.len() - suffix.len();
        }
    }
    match name.rfind('.') {
        Some(0) | None => name.len(),
        Some(index) => index,
    }
}

/// The names a download will try, in order: the file's own name, then
/// ` (1)`, ` (2)`… inserted before the extension the way a browser does.
///
/// Pure, so the naming rule is testable without a filesystem.
pub(crate) fn download_name_candidates(file_name: &str) -> impl Iterator<Item = String> + '_ {
    let sanitized = sanitized_download_name(file_name);
    let (stem, extension) = sanitized.split_at(extension_split_index(sanitized));
    std::iter::once(sanitized.to_string()).chain(
        (1..=DOWNLOAD_NAME_ATTEMPTS).map(move |suffix| format!("{stem} ({suffix}){extension}")),
    )
}

/// The names a downloaded FOLDER will try, in order: the folder's own name,
/// then ` (1)`, ` (2)`… appended at the end. Never the file rule — a folder
/// called `my.folder` has no extension to preserve, and splitting it would
/// produce `my (1).folder`.
pub(crate) fn folder_download_name_candidates(name: &str) -> impl Iterator<Item = String> + '_ {
    let sanitized = sanitized_download_name(name);
    std::iter::once(sanitized.to_string())
        .chain((1..=DOWNLOAD_NAME_ATTEMPTS).map(move |suffix| format!("{sanitized} ({suffix})")))
}

/// Claim a fresh directory for a folder download.
///
/// `reserve` must CREATE the candidate exclusively (`fs::create_dir`, which
/// fails if anything already wears the name) and report whether it won; the
/// successful candidate is both the reservation and the destination, so
/// everything written under it afterwards is by construction new.
pub(crate) fn reserve_download_directory(
    directory: &Path,
    name: &str,
    mut reserve: impl FnMut(&Path) -> bool,
) -> Option<PathBuf> {
    let name = host_download_name(name);
    for candidate in folder_download_name_candidates(&name) {
        let destination = directory.join(&candidate);
        if reserve(&destination) {
            return Some(destination);
        }
    }
    None
}

/// Claim a destination and its staging file together.
///
/// `reserve` must create the staging path *exclusively* and report whether it
/// won the race; a candidate is only taken when its destination is free and
/// its staging file did not already exist. Doing both in one step is what
/// stops two downloads of the same name from sharing a `.part` and stops a
/// pre-existing `X.part` from being truncated and then deleted on cleanup.
pub(crate) fn reserve_download_path(
    directory: &Path,
    file_name: &str,
    exists: impl Fn(&Path) -> bool,
    mut reserve: impl FnMut(&Path) -> bool,
) -> Option<(PathBuf, PathBuf)> {
    let file_name = host_download_name(file_name);
    for name in download_name_candidates(&file_name) {
        let destination = directory.join(&name);
        if exists(&destination) {
            continue;
        }
        let partial = partial_download_path(&destination);
        if reserve(&partial) {
            return Some((destination, partial));
        }
    }
    None
}

pub(crate) trait RemoteFileBackend: Send + Sync {
    fn resolve_root(&self, requested: String) -> RemoteFuture<RemotePath>;
    fn list_directory(
        &self,
        path: RemotePath,
        limit: usize,
    ) -> RemoteFuture<RemoteDirectoryListing>;
    fn read_file(&self, path: RemotePath, limit: usize) -> RemoteFuture<RemoteFileBytes>;

    /// Cheapest possible round-trip, used to confirm a cached connection is
    /// still alive before it is handed out again. The default is a no-op so
    /// test doubles opt in explicitly.
    fn probe(&self) -> RemoteFuture<()> {
        Box::pin(async { Ok(()) })
    }

    /// Send `local` to `remote`, returning the number of bytes written.
    /// Refuses rather than overwriting an existing remote file, and tries to
    /// remove the partial file if it fails or is cancelled — best effort,
    /// since a transfer that died with the connection has no way to clean up
    /// after itself.
    ///
    /// The default refuses: only a backend that can actually write should
    /// claim to, and every test double that does not care about transfers
    /// inherits an honest answer.
    fn upload_file(
        &self,
        local: PathBuf,
        remote: RemotePath,
        progress: RemoteTransferProgress,
        overwrite: bool,
    ) -> TransferFuture {
        let _ = (local, remote, progress, overwrite);
        Box::pin(async { Err(TransferFailure::new("This connection cannot upload files")) })
    }

    /// Create a remote directory, treating "it is already a directory" as
    /// success — merging into an existing folder is normal, and an existing
    /// *file* in its place is the only real clash.
    ///
    /// Checks with `metadata` rather than trusting mkdir's error: libssh wraps
    /// its status codes opaquely, and OpenSSH speaks SFTP v3, whose error set
    /// has no "already exists" — it answers a plain FAILURE that cannot be
    /// told apart from a permission problem.
    fn create_directory(&self, remote: RemotePath) -> RemoteFuture<()> {
        let _ = remote;
        Box::pin(async { Err("This connection cannot create directories".to_string()) })
    }

    /// Fetch `remote` into `local`, returning the number of bytes written.
    fn download_file(
        &self,
        remote: RemotePath,
        local: PathBuf,
        progress: RemoteTransferProgress,
    ) -> TransferFuture {
        let _ = (remote, local, progress);
        Box::pin(async {
            Err(TransferFailure::new(
                "This connection cannot download files",
            ))
        })
    }

    /// Stream `remote` into an already-created local file, returning the bytes
    /// written. Unlike [`Self::download_file`] there is no staging and no
    /// rename: the caller created `sink` exclusively (through its pinned
    /// destination handle) and owns cleaning it up on failure. That is what a
    /// folder download wants — its whole tree is freshly reserved, and every
    /// write must stay behind the caller's directory handle.
    fn download_into(
        &self,
        remote: RemotePath,
        sink: std::fs::File,
        progress: RemoteTransferProgress,
    ) -> TransferFuture {
        let _ = (remote, sink, progress);
        Box::pin(async {
            Err(TransferFailure::new(
                "This connection cannot download files",
            ))
        })
    }

    /// Whether anything currently wears this remote name. Advisory only: a
    /// `false` may be a transport failure or a race, so a caller choosing a
    /// free name must still create exclusively and treat THAT as the truth.
    /// The default says "not there", which makes test doubles optimistic and
    /// keeps the exclusive create as the single honest gate.
    fn exists(&self, remote: RemotePath) -> RemoteFuture<bool> {
        let _ = remote;
        Box::pin(async { Ok(false) })
    }

    /// Delete a single remote file (or symlink). The default refuses, like
    /// `upload_file`: only a backend that can actually delete should claim to.
    fn remove_file(&self, remote: RemotePath) -> RemoteFuture<()> {
        let _ = remote;
        Box::pin(async { Err("This connection cannot delete files".to_string()) })
    }

    /// Delete an EMPTY remote directory. Recursion is the caller's job: the
    /// protocol offers only rmdir, and hiding a walk in here would bury both
    /// its per-directory round trips and its cancellation points.
    fn remove_directory(&self, remote: RemotePath) -> RemoteFuture<()> {
        let _ = remote;
        Box::pin(async { Err("This connection cannot delete folders".to_string()) })
    }

    /// Rename `from` to `to` WITHOUT overwriting: a `to` that already exists
    /// is the server's error to report. Implementations must reserve the
    /// destination atomically rather than relying on an advisory existence
    /// check followed by a potentially-overwriting rename.
    fn rename(&self, from: RemotePath, to: RemotePath) -> RemoteFuture<()> {
        let _ = (from, to);
        Box::pin(async { Err("This connection cannot rename".to_string()) })
    }

    /// Create a remote directory, failing if ANYTHING already has that name.
    /// Unlike [`Self::create_directory`], an existing directory is a failure:
    /// "New Folder" must mint something new, never adopt a neighbour.
    fn create_directory_exclusive(&self, remote: RemotePath) -> RemoteFuture<()> {
        let _ = remote;
        Box::pin(async { Err("This connection cannot create directories".to_string()) })
    }

    /// Every file under `root` for search, listed on the remote host by
    /// `thinkterm list-files` under the rules the local index uses.
    fn list_project_files(
        &self,
        root: RemotePath,
        respect_gitignore: bool,
    ) -> RemoteFuture<RemoteProjectListing> {
        let _ = (root, respect_gitignore);
        Box::pin(async { Ok(RemoteProjectListing::NeedsUpdate) })
    }
}

/// What listing a remote project for search came back with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RemoteProjectListing {
    Listed(thinkterm_file_index::Listing),
    /// The remote host has no `thinkterm` that can list files: none at all, or
    /// one from before `list-files`.
    NeedsUpdate,
}

/// The most a remote listing may send: the entry limit times a generous path
/// length. A listing past this is refused rather than held.
const REMOTE_LISTING_MAX_BYTES: u64 = 64 * 1024 * 1024;
/// How long the remote walk may run before it stops and says it is partial.
const REMOTE_LISTING_TIME_LIMIT_SECS: u64 = 20;
/// How much longer than that the desktop waits for the listing to arrive.
const REMOTE_LISTING_GRACE_SECS: u64 = 15;
/// A listing given up on. The remote may ignore the hangup sent after it, in
/// which case its reader stays blocked until the connection goes, so this is
/// not retried as the query changes.
pub(crate) const REMOTE_LISTING_TIMED_OUT: &str = "The remote file listing timed out";
const REMOTE_LISTING_EXEC_FAILED: &str = "Unable to run the remote file listing";
const REMOTE_LISTING_READ_FAILED: &str = "Reading the remote file listing failed";

/// Whether a listing failure came from the connection itself rather than
/// from what the remote printed. Only those may retire the pooled connection:
/// the remote's own words, a stray "broken pipe" among them, must not.
pub(crate) fn remote_listing_failure_is_transport(message: &str) -> bool {
    message.starts_with(REMOTE_LISTING_EXEC_FAILED)
        || message.starts_with(REMOTE_LISTING_READ_FAILED)
}

/// Quote `value` for a POSIX shell: the command line an ssh exec runs goes
/// through the remote user's shell.
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// The command that lists `root` on the remote host: the host's configured
/// `remote_wezterm_path` when there is one, as the mux proxy runs it; else
/// `thinkterm` from where the proxy and the mobile probe look (an exec's shell
/// does not read the startup files that put it on PATH). The search runs
/// under `sh`, whatever the login shell is, and exits 127 when it finds none.
fn list_files_command(
    root: &RemotePath,
    respect_gitignore: bool,
    remote_command: Option<&str>,
) -> String {
    let mut args = format!("list-files --time-limit {REMOTE_LISTING_TIME_LIMIT_SECS}");
    if respect_gitignore {
        args.push_str(" --respect-gitignore");
    }
    let root = shell_quote(root.as_str());
    if let Some(command) = remote_command {
        return format!("{command} {args} -- {root}");
    }
    // No single quotes or `!` inside: it is single-quoted for every shell.
    let search = format!(
        "for p in \"$HOME/.local/bin/thinkterm\" \"$(command -v thinkterm)\" \
         /usr/local/bin/thinkterm /opt/homebrew/bin/thinkterm \
         /Applications/ThinkTerm.app/Contents/MacOS/thinkterm; do \
         [ -n \"$p\" ] && [ -x \"$p\" ] && exec \"$p\" {args} -- \"$1\"; done; exit 127"
    );
    format!("sh -c '{search}' sh {root}")
}

/// Make sense of what `list_files_command` produced. A missing `thinkterm`
/// (exit 127) or one that does not know `list-files` means the host needs
/// updating; anything else that is not a whole listing is a failure.
fn interpret_remote_listing(
    stdout: &[u8],
    stderr: &str,
    exit_code: Option<u32>,
) -> Result<RemoteProjectListing, String> {
    if let Ok(listing) = thinkterm_file_index::parse_command_output(stdout) {
        return Ok(RemoteProjectListing::Listed(listing));
    }
    if exit_code == Some(127)
        || stderr.contains("unrecognized subcommand")
        || stderr.contains("list-files")
            && (stderr.contains("unexpected") || stderr.contains("invalid"))
    {
        return Ok(RemoteProjectListing::NeedsUpdate);
    }
    let detail = stderr.trim();
    Err(if detail.is_empty() {
        "The remote host did not return a file listing".to_string()
    } else {
        format!("The remote host could not list files: {detail}")
    })
}

/// Run the listing on an exec channel of `session` and read it back. Blocking:
/// call it off the UI thread. stderr drains on a thread of its own, so a
/// remote that writes a lot there cannot stall the listing.
fn run_remote_listing(exec: wezterm_ssh::ExecResult) -> Result<RemoteProjectListing, String> {
    use portable_pty::{Child as _, ChildKiller as _};
    use std::io::Read;
    let wezterm_ssh::ExecResult {
        stdin,
        stdout,
        mut stderr,
        mut child,
    } = exec;
    drop(stdin);
    let stderr = std::thread::spawn(move || {
        let mut err = Vec::new();
        let _ = (&mut stderr).take(16 * 1024).read_to_end(&mut err);
        let _ = std::io::copy(&mut stderr, &mut std::io::sink());
        err
    });
    let mut out = Vec::new();
    let read = stdout
        .take(REMOTE_LISTING_MAX_BYTES + 1)
        .read_to_end(&mut out)
        .map_err(|err| format!("{REMOTE_LISTING_READ_FAILED}: {err}"));
    if read.is_err() || out.len() as u64 > REMOTE_LISTING_MAX_BYTES {
        let _ = child.kill();
        read?;
        return Err("The remote file listing is too large".to_string());
    }
    let err = stderr.join().unwrap_or_default();
    let exit_code = child.wait().ok().map(|status| status.exit_code());
    interpret_remote_listing(&out, &String::from_utf8_lossy(&err), exit_code)
}

#[cfg(test)]
mod listing_tests {
    use super::*;

    #[test]
    fn roots_are_quoted_for_the_shell() {
        assert_eq!(shell_quote("/srv/app"), "'/srv/app'");
        assert_eq!(shell_quote("/srv/it's here"), "'/srv/it'\\''s here'");
        let root = RemotePath::from_server_absolute("/srv/a b;rm -rf ~").unwrap();
        let command = list_files_command(&root, true, None);
        assert!(command.starts_with("sh -c '"));
        assert!(command.ends_with("' sh '/srv/a b;rm -rf ~'"));
        assert!(command.contains("--respect-gitignore -- \"$1\""));
        assert!(!list_files_command(&root, false, None).contains("--respect-gitignore"));
        // The script is one single-quoted word for any shell.
        let script = &command["sh -c '".len()..command.len() - "' sh '/srv/a b;rm -rf ~'".len()];
        assert!(!script.contains('\'') && !script.contains('!'));
        assert_eq!(
            list_files_command(&root, false, Some("/opt/tt/thinkterm")),
            "/opt/tt/thinkterm list-files --time-limit 20 -- '/srv/a b;rm -rf ~'"
        );
    }

    #[test]
    fn listings_and_missing_commands_are_told_apart() {
        let mut listed = vec![];
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "").unwrap();
        thinkterm_file_index::write_listing(
            dir.path(),
            true,
            10,
            Instant::now() + Duration::from_secs(60),
            &mut listed,
        )
        .unwrap();
        match interpret_remote_listing(&listed, "", Some(0)).unwrap() {
            RemoteProjectListing::Listed(listing) => assert_eq!(listing.entries.len(), 1),
            other => panic!("{:?}", other),
        }
        let mut greeted = b"hello from .bashrc\n".to_vec();
        greeted.extend_from_slice(&listed);
        assert!(matches!(
            interpret_remote_listing(&greeted, "", Some(0)).unwrap(),
            RemoteProjectListing::Listed(_)
        ));
        assert_eq!(
            interpret_remote_listing(b"", "", Some(127)).unwrap(),
            RemoteProjectListing::NeedsUpdate
        );
        assert_eq!(
            interpret_remote_listing(b"", "error: unrecognized subcommand 'list-files'", Some(2))
                .unwrap(),
            RemoteProjectListing::NeedsUpdate
        );
        assert!(
            interpret_remote_listing(b"", "listing /x: not a directory", Some(1))
                .unwrap_err()
                .contains("not a directory")
        );
        assert!(interpret_remote_listing(&listed[..listed.len() - 2], "", Some(0)).is_err());
    }
}

pub(crate) trait RemoteFileConnector: Send + Sync {
    fn connect(&self, config: SshDomain) -> RemoteFuture<Arc<dyn RemoteFileBackend>>;
}

pub(crate) fn remote_connection_key(source_key: &str, config: &SshDomain) -> String {
    let mut hasher = DefaultHasher::new();
    source_key.hash(&mut hasher);
    config.remote_address.hash(&mut hasher);
    config.username.hash(&mut hasher);
    config.no_agent_auth.hash(&mut hasher);
    format!("{:?}", config.ssh_backend).hash(&mut hasher);
    let mut options = config.ssh_option.iter().collect::<Vec<_>>();
    options.sort_by(|a, b| a.0.cmp(b.0).then_with(|| a.1.cmp(b.1)));
    options.hash(&mut hasher);
    // Include credentials in the fingerprint without retaining or logging
    // another copy of the secret.
    config.stored_password.hash(&mut hasher);
    format!("{source_key}:{:016x}", hasher.finish())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RemoteRenameEntryKind {
    Directory,
    Other,
}

trait RemoteRenameBackend: Send + Sync {
    fn inspect(&self, path: String) -> RemoteFuture<Option<RemoteRenameEntryKind>>;
    fn reserve(&self, path: String, kind: RemoteRenameEntryKind) -> RemoteFuture<()>;
    fn replace(&self, from: String, to: String) -> RemoteFuture<()>;
    fn release(&self, path: String, kind: RemoteRenameEntryKind) -> RemoteFuture<()>;
}

async fn rename_without_overwrite(
    backend: &dyn RemoteRenameBackend,
    from: &RemotePath,
    to: &RemotePath,
) -> Result<(), String> {
    let source_kind = backend
        .inspect(from.as_str().to_string())
        .await
        .map_err(|err| format!("Unable to inspect {}: {err}", from.as_str()))?
        .ok_or_else(|| format!("{} does not exist", from.as_str()))?;

    if let Err(reserve_error) = backend.reserve(to.as_str().to_string(), source_kind).await {
        return match backend.inspect(to.as_str().to_string()).await {
            Ok(Some(_)) => Err(format!("{} already exists", to.as_str())),
            Ok(None) => Err(format!(
                "Unable to reserve {} for rename: {reserve_error}",
                to.as_str()
            )),
            Err(check) => Err(format!(
                "Unable to verify why {} could not be reserved: {check}",
                to.as_str()
            )),
        };
    }

    if let Err(rename_error) = backend
        .replace(from.as_str().to_string(), to.as_str().to_string())
        .await
    {
        let mut message = format!(
            "Unable to rename {} to {}: {rename_error}",
            from.as_str(),
            to.file_name()
        );
        // A transport error can be ambiguous: the server may have completed
        // the rename before the reply was lost. Only remove the placeholder
        // after proving the source still exists; otherwise `to` may now be
        // the user's actual data.
        match backend.inspect(from.as_str().to_string()).await {
            Ok(Some(_)) => {
                if let Err(cleanup_error) =
                    backend.release(to.as_str().to_string(), source_kind).await
                {
                    message.push_str(&format!(
                        "; the reserved placeholder {} could not be removed: {cleanup_error}",
                        to.as_str()
                    ));
                }
            }
            Ok(None) => {
                message.push_str(&format!(
                    "; the outcome could not be verified, so {} was left untouched",
                    to.as_str()
                ));
            }
            Err(check) => {
                message.push_str(&format!(
                    "; the outcome could not be verified, so {} was left untouched: {check}",
                    to.as_str()
                ));
            }
        }
        return Err(message);
    }
    Ok(())
}

#[derive(Clone)]
struct SftpRemoteRenameBackend {
    sftp: wezterm_ssh::Sftp,
}

impl RemoteRenameBackend for SftpRemoteRenameBackend {
    fn inspect(&self, path: String) -> RemoteFuture<Option<RemoteRenameEntryKind>> {
        let sftp = self.sftp.clone();
        Box::pin(async move {
            match sftp.symlink_metadata(path).await {
                Ok(metadata) => Ok(Some(if metadata.is_dir() {
                    RemoteRenameEntryKind::Directory
                } else {
                    RemoteRenameEntryKind::Other
                })),
                Err(err) if sftp_error_is_missing(&err) => Ok(None),
                Err(err) => Err(err.to_string()),
            }
        })
    }

    fn reserve(&self, path: String, kind: RemoteRenameEntryKind) -> RemoteFuture<()> {
        let sftp = self.sftp.clone();
        Box::pin(async move {
            match kind {
                RemoteRenameEntryKind::Directory => sftp
                    .create_dir(path, 0o700)
                    .await
                    .map_err(|err| err.to_string()),
                RemoteRenameEntryKind::Other => sftp
                    .create_new(path)
                    .await
                    .map(drop)
                    .map_err(|err| err.to_string()),
            }
        })
    }

    fn replace(&self, from: String, to: String) -> RemoteFuture<()> {
        let sftp = self.sftp.clone();
        Box::pin(async move {
            sftp.rename(from, to, wezterm_ssh::RenameOptions::default())
                .await
                .map_err(|err| err.to_string())
        })
    }

    fn release(&self, path: String, kind: RemoteRenameEntryKind) -> RemoteFuture<()> {
        let sftp = self.sftp.clone();
        Box::pin(async move {
            match kind {
                RemoteRenameEntryKind::Directory => sftp.remove_dir(path).await,
                RemoteRenameEntryKind::Other => sftp.remove_file(path).await,
            }
            .map_err(|err| err.to_string())
        })
    }
}

struct SftpRemoteFileBackend {
    // Sftp only retains a request sender.  Keep the Session handle alive for
    // the lifetime of this independent file connection.
    _session: Session,
    sftp: wezterm_ssh::Sftp,
    /// The host's `remote_wezterm_path`, for `list-files`.
    remote_command: Option<String>,
}

impl RemoteFileBackend for SftpRemoteFileBackend {
    fn list_project_files(
        &self,
        root: RemotePath,
        respect_gitignore: bool,
    ) -> RemoteFuture<RemoteProjectListing> {
        // An exec channel on the session this backend already holds: no second
        // login, and it closes when the listing is read.
        let session = self._session.clone();
        let command = list_files_command(&root, respect_gitignore, self.remote_command.as_deref());
        Box::pin(async move {
            let exec = session
                .exec(&command, None)
                .await
                .map_err(|err| format!("{REMOTE_LISTING_EXEC_FAILED}: {err:#}"))?;
            // The remote walk stops itself at its time limit, but only between
            // entries; one stuck on a dead mount must not hold the search.
            let mut killer = portable_pty::ChildKiller::clone_killer(&exec.child);
            let listing = smol::unblock(move || run_remote_listing(exec));
            let timeout = async {
                smol::Timer::after(Duration::from_secs(
                    REMOTE_LISTING_TIME_LIMIT_SECS + REMOTE_LISTING_GRACE_SECS,
                ))
                .await;
                let _ = killer.kill();
                Err(REMOTE_LISTING_TIMED_OUT.to_string())
            };
            smol::future::or(listing, timeout).await
        })
    }

    fn resolve_root(&self, requested: String) -> RemoteFuture<RemotePath> {
        let sftp = self.sftp.clone();
        Box::pin(async move {
            if !requested_root_needs_home(&requested) {
                // Already absolute (or invalid): no server round-trip needed.
                return remote_root_from_absolute(&requested);
            }

            // The home directory is whatever the server canonicalizes the
            // session's starting directory to; see REMOTE_HOME_SPELLINGS.
            match canonicalize_remote_home(|spelling| {
                let sftp = sftp.clone();
                async move { sftp.canonicalize(spelling.to_string()).await }
            })
            .await
            {
                Ok(home) => resolve_requested_root(
                    RemotePath::from_server_absolute(home.as_str())?,
                    &requested,
                ),
                Err(errors) => {
                    let mut failures = Vec::new();
                    for (spelling, err) in REMOTE_HOME_SPELLINGS.iter().zip(&errors) {
                        log::warn!(
                            "remote files: sftp canonicalize({spelling:?}) failed: {}",
                            error_chain(err)
                        );
                        failures.push(error_summary(err));
                    }
                    let detail = failures.last().cloned().unwrap_or_default();
                    log::error!("remote files: unable to resolve remote home: {failures:?}");
                    Err(format!("Can't read the remote home directory: {detail}"))
                }
            }
        })
    }

    fn list_directory(
        &self,
        path: RemotePath,
        limit: usize,
    ) -> RemoteFuture<RemoteDirectoryListing> {
        let sftp = self.sftp.clone();
        Box::pin(async move {
            let dir = sftp
                .open_dir(path.as_str().to_string())
                .await
                .map_err(|err| format!("Unable to open {}: {err}", path.as_str()))?;
            let mut entries = Vec::with_capacity(limit.min(256));
            let mut truncated = false;
            loop {
                // The raw API reports normal end-of-directory as an error.
                // Preserve other errors so a dead shared connection can be
                // invalidated rather than appearing as a partial directory.
                let (raw_name, metadata) = match dir.read_dir().await {
                    Ok(entry) => entry,
                    Err(err) if is_sftp_directory_eof(&err) => break,
                    Err(err) => {
                        return Err(format!(
                            "Unable to read directory {}: {err:#}",
                            path.as_str()
                        ))
                    }
                };
                let name = raw_name.as_str();
                if matches!(name, "." | "..") {
                    continue;
                }
                let Ok(entry_path) = path.join_name(name) else {
                    continue;
                };
                if entries.len() >= limit {
                    truncated = true;
                    break;
                }
                let kind = if metadata.is_dir() {
                    RemoteFileKind::Directory
                } else if metadata.is_file() {
                    RemoteFileKind::File
                } else if metadata.is_symlink() {
                    RemoteFileKind::Symlink
                } else {
                    RemoteFileKind::Other
                };
                entries.push(RemoteFileEntry {
                    path: entry_path,
                    name: name.to_string(),
                    kind,
                    size: metadata.size,
                });
            }
            entries.sort_by(|a, b| {
                b.is_directory()
                    .cmp(&a.is_directory())
                    .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
                    .then_with(|| a.name.cmp(&b.name))
            });
            Ok(RemoteDirectoryListing { entries, truncated })
        })
    }

    fn probe(&self) -> RemoteFuture<()> {
        let sftp = self.sftp.clone();
        Box::pin(async move {
            // Same spellings as resolve_root: a server that only answers the
            // empty path is alive, and declaring it dead here would force a
            // redial on every idle reacquire.
            match canonicalize_remote_home(|spelling| {
                let sftp = sftp.clone();
                async move { sftp.canonicalize(spelling.to_string()).await }
            })
            .await
            {
                Ok(_) => Ok(()),
                Err(errors) => Err(errors
                    .last()
                    .map(|err| error_chain(err))
                    .unwrap_or_default()),
            }
        })
    }

    fn read_file(&self, path: RemotePath, limit: usize) -> RemoteFuture<RemoteFileBytes> {
        let sftp = self.sftp.clone();
        Box::pin(async move {
            let file = sftp
                .open(path.as_str().to_string())
                .await
                .map_err(|err| format!("Unable to open {}: {err}", path.as_str()))?;
            let mut bytes = Vec::with_capacity(limit.saturating_add(1).min(256 * 1024));
            let mut limited = file.take(limit.saturating_add(1) as u64);
            limited
                .read_to_end(&mut bytes)
                .await
                .map_err(|err| format!("Unable to read {}: {err}", path.as_str()))?;
            let truncated = bytes.len() > limit;
            if truncated {
                bytes.truncate(limit);
            }
            Ok(RemoteFileBytes { bytes, truncated })
        })
    }

    fn upload_file(
        &self,
        local: PathBuf,
        remote: RemotePath,
        progress: RemoteTransferProgress,
        overwrite: bool,
    ) -> TransferFuture {
        let sftp = self.sftp.clone();
        Box::pin(async move {
            let mut source = smol::fs::File::open(&local).await.map_err(|err| {
                TransferFailure::new(format!("Unable to open {}: {err}", local.display()))
            })?;
            // Only meaningful for a single file; a folder transfer has already
            // switched the row to item scale, which this must not disturb.
            if !progress.tracks_items() {
                if let Ok(metadata) = source.metadata().await {
                    progress.set_total(metadata.len());
                }
            }

            // Refuse to clobber, and let the *server* decide it. The previous
            // metadata()-then-create left a window in which another writer
            // could win, and cost a round trip per file — which a folder
            // upload would have paid thousands of times.
            //
            // Everything above this point creates nothing, which is what makes
            // the cleanup below safe: it can only ever delete a file this
            // call brought into existence. Do not hoist that cleanup up here —
            // a failure to create means the file was someone else's.
            let created = if overwrite {
                // The user has already been asked and said replace, so the
                // truncating open is what they chose.
                sftp.create(remote.as_str().to_string()).await
            } else {
                sftp.create_new(remote.as_str().to_string()).await
            };
            let mut file = created.map_err(|err| {
                TransferFailure::new(if overwrite {
                    format!("Unable to write {}: {err}", remote.as_str())
                } else {
                    format!(
                        "Unable to create {} (it may already exist): {err}",
                        remote.as_str()
                    )
                })
            })?;

            let outcome = copy_stream(
                &mut source,
                &mut file,
                &progress,
                "the local file",
                "the server",
            )
            .await;

            // Close the handle before touching the path: a half-written file
            // must not survive, and some servers refuse to unlink one that is
            // still open.
            //
            // On an overwrite the original was already destroyed by the
            // truncating open, so removal is not what loses it — the failure
            // is. Removing anyway is still the better of two bad outcomes: a
            // truncated file wearing the original's name reads as intact.
            // (Uploading to a temporary name and renaming over would preserve
            // it, but the libssh backend silently drops rename's overwrite
            // flag, so that route is not dependable here.)
            drop(file);
            match outcome {
                Ok(written) => Ok(written),
                Err(message) => {
                    let mut failure = TransferFailure::new(message);
                    if let Err(err) = sftp.remove_file(remote.as_str().to_string()).await {
                        // Usually because the connection is what failed, so the
                        // cleanup rides the same dead session. Say so: a
                        // half-written file the user does not know about is
                        // worse than one they can go and delete.
                        log::warn!(
                            "remote files: unable to clean up the partial upload {}: {err:#}",
                            remote.as_str()
                        );
                        failure.leftover = Some(remote.as_str().to_string());
                    }
                    Err(failure)
                }
            }
        })
    }

    fn create_directory(&self, remote: RemotePath) -> RemoteFuture<()> {
        let sftp = self.sftp.clone();
        Box::pin(async move {
            if let Ok(metadata) = sftp.metadata(remote.as_str().to_string()).await {
                return if metadata.is_dir() {
                    Ok(())
                } else {
                    Err(format!(
                        "{} already exists and is not a folder",
                        remote.as_str()
                    ))
                };
            }
            // 0o755 as a literal: the mode is masked into permission bits and
            // must never carry anything derived from a local file, whose modes
            // mean nothing on the far side.
            sftp.create_dir(remote.as_str().to_string(), 0o755)
                .await
                .map_err(|err| format!("Unable to create {}: {err}", remote.as_str()))
        })
    }

    fn download_file(
        &self,
        remote: RemotePath,
        local: PathBuf,
        progress: RemoteTransferProgress,
    ) -> TransferFuture {
        let sftp = self.sftp.clone();
        Box::pin(async move {
            // The caller reserved the staging file by creating it exclusively,
            // so from this point on EVERY failure path owes it a cleanup —
            // including the ones before a single byte moves. Leaving one behind
            // is not merely litter: `reserve_download_path` skips candidates
            // whose staging file is taken, so an orphan permanently bumps every
            // later download of that name to ` (1)`, ` (2)`, …
            let partial = partial_download_path(&local);
            let outcome = download_into_partial(&sftp, &remote, &partial, &progress).await;
            finish_download(outcome, &partial, &local).await
        })
    }

    fn download_into(
        &self,
        remote: RemotePath,
        sink: std::fs::File,
        progress: RemoteTransferProgress,
    ) -> TransferFuture {
        let sftp = self.sftp.clone();
        Box::pin(async move {
            let mut file = sftp
                .open(remote.as_str().to_string())
                .await
                .map_err(|err| {
                    TransferFailure::new(format!("Unable to open {}: {err}", remote.as_str()))
                })?;
            let mut sink = smol::fs::File::from(sink);
            copy_stream(
                &mut file,
                &mut sink,
                &progress,
                "the server",
                "the local file",
            )
            .await
            .map_err(TransferFailure::new)
        })
    }

    fn exists(&self, remote: RemotePath) -> RemoteFuture<bool> {
        let sftp = self.sftp.clone();
        Box::pin(async move {
            // SFTP v3 cannot distinguish "not found" from other failures, so a
            // failed stat reads as absent; the exclusive create downstream is
            // what actually guarantees no clobbering.
            Ok(sftp.metadata(remote.as_str().to_string()).await.is_ok())
        })
    }

    fn remove_file(&self, remote: RemotePath) -> RemoteFuture<()> {
        let sftp = self.sftp.clone();
        Box::pin(async move {
            sftp.remove_file(remote.as_str().to_string())
                .await
                .map_err(|err| format!("Unable to delete {}: {err}", remote.as_str()))
        })
    }

    fn remove_directory(&self, remote: RemotePath) -> RemoteFuture<()> {
        let sftp = self.sftp.clone();
        Box::pin(async move {
            sftp.remove_dir(remote.as_str().to_string())
                .await
                .map_err(|err| format!("Unable to delete {}: {err}", remote.as_str()))
        })
    }

    fn rename(&self, from: RemotePath, to: RemotePath) -> RemoteFuture<()> {
        let backend = SftpRemoteRenameBackend {
            sftp: self.sftp.clone(),
        };
        Box::pin(async move {
            // libssh drops RenameOptions and can upgrade a plain rename to
            // posix-rename@openssh.com, which replaces an occupied target.
            // Atomically CREATE the target first, then replace only the empty
            // placeholder owned by this operation. A competing creator either
            // wins the reservation (and we stop) or encounters ours; there is
            // no check-then-rename gap in which its data can be destroyed.
            rename_without_overwrite(&backend, &from, &to).await
        })
    }

    fn create_directory_exclusive(&self, remote: RemotePath) -> RemoteFuture<()> {
        let sftp = self.sftp.clone();
        Box::pin(async move {
            // Unlike create_directory there is no merge tolerance, but the
            // failure still deserves a precise message: SFTP v3 answers a
            // bare FAILURE for "exists", indistinguishable from a permission
            // problem, so ask metadata which one it was.
            match sftp.create_dir(remote.as_str().to_string(), 0o755).await {
                Ok(()) => Ok(()),
                Err(err) => match sftp.symlink_metadata(remote.as_str().to_string()).await {
                    Ok(_) => Err(format!("{} already exists", remote.as_str())),
                    Err(check) if sftp_error_is_missing(&check) => {
                        Err(format!("Unable to create {}: {err}", remote.as_str()))
                    }
                    Err(check) => Err(format!(
                        "Unable to verify why {} could not be created: {check}",
                        remote.as_str()
                    )),
                },
            }
        })
    }
}

/// Put a finished download in place, or clean up after a failed one.
///
/// Every exit from a download funnels through here so the reserved staging
/// file has exactly one disposal site. Split from the transfer itself because
/// this half touches only the filesystem, and is therefore the half that can
/// be tested without a server.
async fn finish_download(
    outcome: Result<u64, String>,
    partial: &Path,
    local: &Path,
) -> Result<u64, TransferFailure> {
    let written = match outcome {
        Ok(written) => written,
        Err(message) => {
            let mut failure = TransferFailure::new(message);
            if let Err(cleanup) = smol::fs::remove_file(partial).await {
                log::warn!(
                    "remote files: unable to clean up the partial download {}: {cleanup:#}",
                    partial.display()
                );
                failure.leftover = Some(partial.display().to_string());
            }
            return Err(failure);
        }
    };

    // A plain rename replaces silently and a preceding exists() check follows
    // symlinks (so a dangling link looks vacant) as well as racing with a
    // creator. TempPath's no-clobber persistence performs the move with
    // destination-exists protection and treats a dangling link as occupied.
    match tempfile::TempPath::from_path(partial).persist_noclobber(local) {
        Ok(()) => Ok(written),
        Err(err) => {
            let message = if local_path_is_occupied(local) {
                format!(
                    "{} appeared while downloading, so it was left untouched",
                    local.display()
                )
            } else {
                format!("Unable to save {}: {}", local.display(), err.error)
            };
            let partial = err.path.to_path_buf();
            // Dropping the TempPath attempts cleanup. Check afterwards so a
            // failed cleanup remains visible in the transfer row.
            drop(err.path);
            let mut failure = TransferFailure::new(message);
            if local_path_is_occupied(&partial) {
                failure.leftover = Some(partial.display().to_string());
            }
            Err(failure)
        }
    }
}

/// `true` unless lstat explicitly says the path does not exist.
///
/// This is deliberately fail-closed: permission and transient filesystem
/// errors cannot be interpreted as permission to overwrite.
pub(crate) fn local_path_is_occupied(path: &Path) -> bool {
    match std::fs::symlink_metadata(path) {
        Ok(_) => true,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => false,
        Err(err) => {
            log::warn!(
                "remote files: unable to verify whether {} is free: {err:#}",
                path.display()
            );
            true
        }
    }
}

fn sftp_error_is_missing(err: &SftpChannelError) -> bool {
    matches!(
        err,
        SftpChannelError::Sftp(SftpError::NoSuchFile | SftpError::NoSuchPath)
    )
}

/// Stream `remote` into the already-reserved `partial`. Split out so the
/// caller has exactly one place to clean up from: every `?` in here becomes a
/// single `Err` the caller handles, rather than an early return that walks
/// past the cleanup.
async fn download_into_partial(
    sftp: &wezterm_ssh::Sftp,
    remote: &RemotePath,
    partial: &Path,
    progress: &RemoteTransferProgress,
) -> Result<u64, String> {
    if let Ok(metadata) = sftp.metadata(remote.as_str().to_string()).await {
        if let Some(size) = metadata.size {
            progress.set_total(size);
        }
    }

    let mut file = sftp
        .open(remote.as_str().to_string())
        .await
        .map_err(|err| format!("Unable to open {}: {err}", remote.as_str()))?;

    // The staging file already exists by reservation; opening it here must not
    // create or truncate anything else, or that reservation would mean nothing.
    let mut sink = smol::fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .create(false)
        .open(partial)
        .await
        .map_err(|err| format!("Unable to write {}: {err}", partial.display()))?;

    let written = copy_stream(
        &mut file,
        &mut sink,
        progress,
        "the server",
        "the local file",
    )
    .await;
    drop(sink);
    written
}

/// Where an in-flight download accumulates. Kept next to the destination so
/// the rename that completes it stays on one filesystem.
fn partial_download_path(local: &Path) -> PathBuf {
    let mut name = local.file_name().unwrap_or_default().to_os_string();
    name.push(".part");
    local.with_file_name(name)
}

/// Stream `source` into `sink`, publishing progress and honouring a cancel
/// between chunks. Generic in both directions because the SFTP file type is
/// private to `wezterm-ssh` and cannot be named here — which is just as well,
/// since upload and download differ only in which end is which.
async fn copy_stream<R, W>(
    source: &mut R,
    sink: &mut W,
    progress: &RemoteTransferProgress,
    source_label: &str,
    sink_label: &str,
) -> Result<u64, String>
where
    R: smol::io::AsyncRead + Unpin,
    W: smol::io::AsyncWrite + Unpin,
{
    let mut buffer = vec![0u8; REMOTE_TRANSFER_CHUNK];
    let mut written = 0u64;
    loop {
        if progress.is_canceled() {
            return Err(REMOTE_TRANSFER_CANCELED.to_string());
        }
        let read = source
            .read(&mut buffer)
            .await
            .map_err(|err| format!("Unable to read from {source_label}: {err}"))?;
        if read == 0 {
            break;
        }
        sink.write_all(&buffer[..read])
            .await
            .map_err(|err| format!("Unable to write to {sink_label}: {err}"))?;
        written += read as u64;
        progress.advance(read as u64);
    }
    sink.flush()
        .await
        .map_err(|err| format!("Unable to finish writing to {sink_label}: {err}"))?;
    Ok(written)
}

fn is_sftp_directory_eof(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        let channel_eof = match cause.downcast_ref::<SftpChannelError>() {
            Some(SftpChannelError::Sftp(SftpError::Eof)) => true,
            Some(SftpChannelError::FileIo(io)) => io.kind() == std::io::ErrorKind::UnexpectedEof,
            _ => false,
        };
        channel_eof
            || cause
                .downcast_ref::<std::io::Error>()
                .is_some_and(|io| io.kind() == std::io::ErrorKind::UnexpectedEof)
    })
}

#[derive(Default)]
pub(crate) struct SshRemoteFileConnector;

impl RemoteFileConnector for SshRemoteFileConnector {
    fn connect(&self, config: SshDomain) -> RemoteFuture<Arc<dyn RemoteFileBackend>> {
        Box::pin(async move {
            let ssh_config =
                mux::ssh::ssh_domain_to_ssh_config(&config).map_err(|err| format!("{err:#}"))?;
            let (session, events) = Session::connect(ssh_config)
                .map_err(|err| format!("Unable to connect to SSH server: {err:#}"))?;
            let mut password = config
                .stored_password
                .filter(|password| !password.is_empty());
            while let Ok(event) = events.recv().await {
                match event {
                    SessionEvent::Banner(_) => {}
                    SessionEvent::HostVerify(verify) => {
                        return Err(format!(
                            "SSH host verification requires interactive confirmation: {}",
                            verify.message
                        ));
                    }
                    SessionEvent::Authenticate(auth) => {
                        let mut answers = Vec::with_capacity(auth.prompts.len());
                        for prompt in &auth.prompts {
                            if prompt.echo {
                                return Err(
                                    "SSH authentication requires an interactive prompt".to_string()
                                );
                            }
                            let Some(stored) = password.take() else {
                                return Err(
                                    "SSH authentication requires a saved password".to_string()
                                );
                            };
                            answers.push(stored);
                        }
                        auth.answer(answers).await.map_err(|err| {
                            format!("Unable to answer SSH authentication prompt: {err:#}")
                        })?;
                    }
                    SessionEvent::HostVerificationFailed(failed) => {
                        return Err(format!("SSH host verification failed: {failed}"));
                    }
                    SessionEvent::Error(err) => return Err(format!("SSH error: {err}")),
                    SessionEvent::Authenticated => {
                        let backend: Arc<dyn RemoteFileBackend> = Arc::new(SftpRemoteFileBackend {
                            sftp: session.sftp(),
                            _session: session,
                            remote_command: config.remote_wezterm_path.clone(),
                        });
                        return Ok(backend);
                    }
                }
            }
            Err("SSH authentication did not complete".to_string())
        })
    }
}

enum ManagedConnection {
    Connecting {
        waiters: Vec<Sender<Result<(), String>>>,
    },
    /// A previously-idle entry whose liveness one acquirer is verifying.
    /// Everyone else queues: handing the backend out before the verdict would
    /// let a caller run operations on a session about to be declared dead —
    /// and that caller's failure report could then tear down the healthy
    /// replacement the prober dials.
    Probing {
        connection_id: u64,
        backend: Arc<dyn RemoteFileBackend>,
        idle_generation: u64,
        waiters: Vec<Sender<Result<(), String>>>,
    },
    Ready {
        connection_id: u64,
        backend: Arc<dyn RemoteFileBackend>,
        leases: usize,
        idle_generation: u64,
        idle_since: Option<Instant>,
    },
}

struct RemoteConnectionManagerInner {
    entries: HashMap<String, ManagedConnection>,
    idle_timeout: Duration,
    next_connection_id: u64,
}

pub(crate) struct RemoteConnectionManager {
    connector: Arc<dyn RemoteFileConnector>,
    inner: Mutex<RemoteConnectionManagerInner>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RemoteAcquireError {
    NotConnected,
    Failed(String),
}

pub(crate) struct RemoteConnectionLease {
    key: String,
    backend: Arc<dyn RemoteFileBackend>,
    manager: Weak<RemoteConnectionManager>,
    connection_id: u64,
}

impl RemoteConnectionLease {
    pub(crate) fn backend(&self) -> Arc<dyn RemoteFileBackend> {
        Arc::clone(&self.backend)
    }

    pub(crate) fn connection_key(&self) -> &str {
        &self.key
    }

    /// Identity of the pooled connection behind this lease. Failure reports
    /// must carry it so they can only invalidate the connection they actually
    /// ran on, never a replacement dialled under the same key since.
    pub(crate) fn connection_id(&self) -> u64 {
        self.connection_id
    }

    /// Whether two leases name the same pooled connection. Identity is the
    /// (key, id) pair: a key dialled with since-changed host settings hashes
    /// differently, and a connection redialled under the same key gets a new
    /// id.
    pub(crate) fn is_same_connection(&self, other: &RemoteConnectionLease) -> bool {
        self.key == other.key && self.connection_id == other.connection_id
    }

    pub(crate) fn operation_lease(&self) -> Option<RemoteConnectionLease> {
        self.manager
            .upgrade()
            .and_then(|manager| manager.retain(&self.key, self.connection_id))
    }
}

impl Drop for RemoteConnectionLease {
    fn drop(&mut self) {
        if let Some(manager) = self.manager.upgrade() {
            manager.release(&self.key, self.connection_id);
        }
    }
}

impl RemoteConnectionManager {
    fn new(
        connector: Arc<dyn RemoteFileConnector>,
        idle_timeout: Duration,
    ) -> Arc<RemoteConnectionManager> {
        Arc::new(Self {
            connector,
            inner: Mutex::new(RemoteConnectionManagerInner {
                entries: HashMap::new(),
                idle_timeout,
                next_connection_id: 0,
            }),
        })
    }

    pub(crate) async fn acquire(
        self: &Arc<Self>,
        key: String,
        config: SshDomain,
        allow_connect: bool,
    ) -> Result<RemoteConnectionLease, RemoteAcquireError> {
        enum Pending {
            Dial,
            Wait(smol::channel::Receiver<Result<(), String>>),
            Probe(Arc<dyn RemoteFileBackend>, u64),
        }
        loop {
            let pending = {
                let mut inner = self.inner.lock().unwrap();
                match inner.entries.get_mut(&key) {
                    Some(ManagedConnection::Ready {
                        connection_id,
                        backend,
                        leases,
                        idle_generation,
                        idle_since,
                    }) => {
                        if idle_since.is_none() {
                            // Actively leased: known good, hand it out.
                            *leases = leases.saturating_add(1);
                            *idle_generation = idle_generation.wrapping_add(1);
                            return Ok(RemoteConnectionLease {
                                key: key.clone(),
                                backend: Arc::clone(backend),
                                manager: Arc::downgrade(self),
                                connection_id: *connection_id,
                            });
                        }
                        // A connection that sat with no leases carried no
                        // traffic, so the server or a NAT may have reaped it
                        // without us noticing. Verify before handing it out,
                        // and park later acquires until the verdict is in.
                        let connection_id = *connection_id;
                        let backend = Arc::clone(backend);
                        let idle_generation = *idle_generation;
                        inner.entries.insert(
                            key.clone(),
                            ManagedConnection::Probing {
                                connection_id,
                                backend: Arc::clone(&backend),
                                idle_generation,
                                waiters: Vec::new(),
                            },
                        );
                        Pending::Probe(backend, connection_id)
                    }
                    Some(ManagedConnection::Connecting { waiters })
                    | Some(ManagedConnection::Probing { waiters, .. }) => {
                        let (tx, rx) = smol::channel::bounded(1);
                        waiters.push(tx);
                        Pending::Wait(rx)
                    }
                    None if !allow_connect => return Err(RemoteAcquireError::NotConnected),
                    None => {
                        inner.entries.insert(
                            key.clone(),
                            ManagedConnection::Connecting { waiters: vec![] },
                        );
                        Pending::Dial
                    }
                }
            };

            match pending {
                Pending::Probe(backend, connection_id) => {
                    let probe_result = backend.probe().await;
                    let (waiters, lease) = {
                        let mut inner = self.inner.lock().unwrap();
                        match inner.entries.get_mut(&key) {
                            Some(ManagedConnection::Probing {
                                connection_id: current,
                                backend,
                                idle_generation,
                                waiters,
                            }) if *current == connection_id => {
                                let waiters = std::mem::take(waiters);
                                match &probe_result {
                                    Ok(()) => {
                                        let backend = Arc::clone(backend);
                                        let idle_generation = idle_generation.wrapping_add(1);
                                        inner.entries.insert(
                                            key.clone(),
                                            ManagedConnection::Ready {
                                                connection_id,
                                                backend: Arc::clone(&backend),
                                                leases: 1,
                                                idle_generation,
                                                idle_since: None,
                                            },
                                        );
                                        (
                                            waiters,
                                            Some(RemoteConnectionLease {
                                                key: key.clone(),
                                                backend,
                                                manager: Arc::downgrade(self),
                                                connection_id,
                                            }),
                                        )
                                    }
                                    Err(_) => {
                                        inner.entries.remove(&key);
                                        (waiters, None)
                                    }
                                }
                            }
                            // The entry moved on without us; whatever owns the
                            // key now decides the next iteration.
                            _ => (Vec::new(), None),
                        }
                    };
                    // Waiters re-examine the map rather than trusting the
                    // verdict second-hand: on success they find Ready, on
                    // failure they find nothing (or the prober's redial).
                    for waiter in waiters {
                        let _ = waiter.try_send(Ok(()));
                    }
                    match probe_result {
                        Ok(()) => {
                            if let Some(lease) = lease {
                                return Ok(lease);
                            }
                            continue;
                        }
                        Err(err) => {
                            // Dead after idling: it is already gone from the
                            // map; dial again on the next turn of the loop
                            // rather than surfacing a failure the user would
                            // have to retry by hand.
                            log::warn!(
                                "remote files: cached connection failed its liveness probe, \
                                 reconnecting: {err}"
                            );
                            continue;
                        }
                    }
                }
                Pending::Wait(waiter) => match waiter.recv().await {
                    Ok(Ok(())) => continue,
                    Ok(Err(err)) => return Err(RemoteAcquireError::Failed(err)),
                    Err(_) => {
                        return Err(RemoteAcquireError::Failed(
                            "Remote Files connection was canceled".to_string(),
                        ))
                    }
                },
                Pending::Dial => {}
            }

            let result = self.connector.connect(config.clone()).await;
            let waiters = {
                let mut inner = self.inner.lock().unwrap();
                let waiters = match inner.entries.remove(&key) {
                    Some(ManagedConnection::Connecting { waiters }) => waiters,
                    _ => Vec::new(),
                };
                if let Ok(backend) = &result {
                    inner.next_connection_id = inner.next_connection_id.wrapping_add(1).max(1);
                    let connection_id = inner.next_connection_id;
                    inner.entries.insert(
                        key.clone(),
                        ManagedConnection::Ready {
                            connection_id,
                            backend: Arc::clone(backend),
                            leases: 0,
                            idle_generation: 0,
                            idle_since: None,
                        },
                    );
                }
                waiters
            };
            let wake = result.as_ref().map(|_| ()).map_err(Clone::clone);
            for waiter in waiters {
                let _ = waiter.try_send(wake.clone());
            }
            match result {
                Ok(_) => continue,
                Err(err) => return Err(RemoteAcquireError::Failed(err)),
            }
        }
    }

    fn retain(
        self: &Arc<Self>,
        key: &str,
        expected_connection_id: u64,
    ) -> Option<RemoteConnectionLease> {
        let mut inner = self.inner.lock().unwrap();
        let ManagedConnection::Ready {
            connection_id,
            backend,
            leases,
            idle_generation,
            idle_since,
        } = inner.entries.get_mut(key)?
        else {
            return None;
        };
        if *connection_id != expected_connection_id {
            return None;
        }
        *leases = leases.saturating_add(1);
        *idle_generation = idle_generation.wrapping_add(1);
        *idle_since = None;
        Some(RemoteConnectionLease {
            key: key.to_string(),
            backend: Arc::clone(backend),
            manager: Arc::downgrade(self),
            connection_id: *connection_id,
        })
    }

    fn release(self: &Arc<Self>, key: &str, connection_id: u64) {
        let Some((generation, timeout)) = self.release_state(key, connection_id, Instant::now())
        else {
            return;
        };
        self.schedule_expiry(key.to_string(), connection_id, generation, timeout);
    }

    fn release_state(
        &self,
        key: &str,
        expected_connection_id: u64,
        now: Instant,
    ) -> Option<(u64, Duration)> {
        {
            let mut inner = self.inner.lock().unwrap();
            let timeout = inner.idle_timeout;
            let Some(ManagedConnection::Ready {
                connection_id,
                leases,
                idle_generation,
                idle_since,
                ..
            }) = inner.entries.get_mut(key)
            else {
                return None;
            };
            if *connection_id != expected_connection_id {
                return None;
            }
            *leases = leases.saturating_sub(1);
            if *leases != 0 {
                return None;
            }
            *idle_generation = idle_generation.wrapping_add(1);
            *idle_since = Some(now);
            Some((*idle_generation, timeout))
        }
    }

    fn schedule_expiry(
        self: &Arc<Self>,
        key: String,
        connection_id: u64,
        generation: u64,
        timeout: Duration,
    ) {
        let manager = Arc::clone(self);
        smol::spawn(async move {
            smol::Timer::after(timeout).await;
            manager.expire_if_idle(&key, connection_id, generation, Instant::now());
        })
        .detach();
    }

    fn expire_if_idle(
        &self,
        key: &str,
        expected_connection_id: u64,
        generation: u64,
        now: Instant,
    ) -> bool {
        let mut inner = self.inner.lock().unwrap();
        let timeout = inner.idle_timeout;
        let should_remove = matches!(
            inner.entries.get(key),
            Some(ManagedConnection::Ready {
                connection_id,
                leases: 0,
                idle_generation,
                idle_since: Some(idle_since),
                ..
            }) if *connection_id == expected_connection_id
                && *idle_generation == generation
                && now.saturating_duration_since(*idle_since) >= timeout
        );
        if should_remove {
            inner.entries.remove(key);
        }
        should_remove
    }

    pub(crate) fn set_idle_timeout(self: &Arc<Self>, timeout: Duration) {
        let now = Instant::now();
        let idle_entries = {
            let mut inner = self.inner.lock().unwrap();
            inner.idle_timeout = timeout;
            inner
                .entries
                .iter_mut()
                .filter_map(|(key, entry)| match entry {
                    ManagedConnection::Ready {
                        connection_id,
                        leases: 0,
                        idle_generation,
                        idle_since,
                        ..
                    } => {
                        *idle_generation = idle_generation.wrapping_add(1);
                        let since = *idle_since.get_or_insert(now);
                        let remaining =
                            timeout.saturating_sub(now.saturating_duration_since(since));
                        Some((key.clone(), *connection_id, *idle_generation, remaining))
                    }
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        for (key, connection_id, generation, remaining) in idle_entries {
            self.schedule_expiry(key, connection_id, generation, remaining);
        }
    }

    #[cfg(test)]
    pub(crate) fn invalidate(&self, key: &str) {
        self.inner.lock().unwrap().entries.remove(key);
    }

    /// Remove `key` only while it still refers to `connection_id`, so a stale
    /// failure report cannot discard a replacement another task already
    /// installed. Deliberately never matches `Connecting`/`Probing`: an
    /// in-flight dial or probe settles on its own verdict, and removing it
    /// here would strand its waiters.
    fn invalidate_connection(&self, key: &str, connection_id: u64) {
        let mut inner = self.inner.lock().unwrap();
        let matches = matches!(
            inner.entries.get(key),
            Some(ManagedConnection::Ready { connection_id: current, .. }) if *current == connection_id
        );
        if matches {
            inner.entries.remove(key);
        }
    }
}

/// Sources the user has explicitly connected at least once in this process.
///
/// Deliberately process-wide, matching [`remote_connection_manager`]: the
/// connection itself is shared between windows, so remembering the click only
/// per-window meant a second window had to ask again for a session that was
/// already open. Never persisted — a restart requires a fresh click, which is
/// what keeps "never dial without being asked" meaningful.
fn authorized_sources() -> &'static Mutex<HashSet<String>> {
    static AUTHORIZED: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    AUTHORIZED.get_or_init(|| Mutex::new(HashSet::new()))
}

pub(crate) fn authorize_remote_source(source_key: &str) {
    authorized_sources()
        .lock()
        .unwrap()
        .insert(source_key.to_string());
}

pub(crate) fn remote_source_is_authorized(source_key: &str) -> bool {
    authorized_sources().lock().unwrap().contains(source_key)
}

pub(crate) fn remote_connection_manager() -> Arc<RemoteConnectionManager> {
    static MANAGER: OnceLock<Arc<RemoteConnectionManager>> = OnceLock::new();
    Arc::clone(MANAGER.get_or_init(|| {
        RemoteConnectionManager::new(
            Arc::new(SshRemoteFileConnector),
            Duration::from_secs(crate::native_settings::remote_sftp_idle_minutes() as u64 * 60),
        )
    }))
}

pub(crate) fn update_remote_connection_idle_timeout(minutes: u32) {
    remote_connection_manager().set_idle_timeout(Duration::from_secs(minutes as u64 * 60));
}

/// Drop a cached connection. Used when a connection fails the checks that
/// bring the panel up (resolving the root, listing it): whatever went wrong, a
/// session that cannot complete its own handshake is worthless, and keeping it
/// means every Retry replays the same failure against the same broken session
/// instead of dialling afresh. Guarded by `connection_id` — the failure must
/// only ever remove the connection it actually happened on, never a healthy
/// replacement dialled under the same key since.
pub(crate) fn invalidate_remote_connection(key: &str, connection_id: u64) {
    remote_connection_manager().invalidate_connection(key, connection_id);
}

pub(crate) fn invalidate_remote_connection_if_dead(
    key: &str,
    connection_id: u64,
    message: &str,
) -> bool {
    let lower = message.to_ascii_lowercase();
    // Matches both the wrapper text (`SftpChannelError`) and the innermost
    // cause, because a concise user-facing message carries only the latter.
    // "closed channel" covers async-channel's own wording on both ends:
    // "sending into a closed channel" and "receiving from an empty and closed
    // channel" — the previous "sending on a closed" never matched either.
    let dead = lower.contains("session is dead")
        || lower.contains("channel is closed")
        || lower.contains("channel closed")
        || lower.contains("closed channel")
        || lower.contains("no connection has been set up")
        || lower.contains("connection, but we lost it")
        || lower.contains("failed to send request")
        || lower.contains("failed to receive response")
        || lower.contains("broken pipe");
    if dead {
        remote_connection_manager().invalidate_connection(key, connection_id);
    }
    dead
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RemoteFilesPhase {
    Disconnected,
    Connecting,
    Connected,
    Failed(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RemoteDirectory {
    listing: RemoteDirectoryListing,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RemoteFileRow {
    pub entry: RemoteFileEntry,
    pub depth: usize,
    pub expanded: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RemotePreviewStatus {
    None,
    Loading(RemotePath),
    Ready(RemotePath),
    Failed(RemotePath, String),
}

#[derive(Clone, Debug)]
pub(crate) struct RemoteFilesState {
    pub target: Option<RemoteFilesTarget>,
    pub phase: RemoteFilesPhase,
    pub root: Option<RemotePath>,
    directories: HashMap<RemotePath, RemoteDirectory>,
    pub expanded: HashSet<RemotePath>,
    loading_directories: HashSet<RemotePath>,
    pub selected: Option<RemotePath>,
    pub preview: RemotePreviewStatus,
    pub error_message: Option<String>,
    pub generation: u64,
    /// True while a tree restored from cache is on screen but the fresh lease
    /// for it has not arrived yet. Effects that need the lease are deferred
    /// (the intent is already recorded in `loading_directories` / `preview`)
    /// and flushed by the `Connected` event, so a click during that round trip
    /// cannot reach the effect layer, find no lease, and tear the restored
    /// tree down as a spurious connection failure.
    awaiting_lease: bool,
    /// Trees kept across target switches so returning to a workspace does not
    /// re-list a directory the connection could still serve from memory.
    /// Keyed by [`Self::tree_cache_key`] — source *and* requested root —
    /// because two projects can share one host while browsing different
    /// directories, and the wrong tree must never appear under a project's
    /// name.
    cached_trees: HashMap<TreeCacheKey, CachedTree>,
    /// LRU order for `cached_trees`, oldest first.
    cached_tree_order: VecDeque<TreeCacheKey>,
}

/// Identity of a browsed tree: the connection source plus the requested root.
/// The source alone is not enough — see `cached_trees`.
type TreeCacheKey = (String, String);

/// Everything needed to put a previously browsed source straight back on
/// screen. Small by design: entries are `{ path, name, kind, size }` and the
/// row budget caps them, so a handful of these cost far less than one decoded
/// image preview.
#[derive(Clone, Debug)]
struct CachedTree {
    root: RemotePath,
    directories: HashMap<RemotePath, RemoteDirectory>,
    expanded: HashSet<RemotePath>,
    selected: Option<RemotePath>,
}

/// How many sources keep a cached tree. Much smaller than the local panel's
/// `FILE_VIEW_STATE_CACHE_CAP` (32) because each entry here holds up to
/// `REMOTE_FILE_TREE_ROW_LIMIT` rows rather than a scroll offset.
const REMOTE_TREE_CACHE_CAP: usize = 4;

impl Default for RemoteFilesState {
    fn default() -> Self {
        Self {
            target: None,
            phase: RemoteFilesPhase::Disconnected,
            root: None,
            directories: HashMap::new(),
            expanded: HashSet::new(),
            loading_directories: HashSet::new(),
            selected: None,
            preview: RemotePreviewStatus::None,
            error_message: None,
            generation: 0,
            awaiting_lease: false,
            cached_trees: HashMap::new(),
            cached_tree_order: VecDeque::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RemoteFilesEffect {
    Connect {
        generation: u64,
        target: RemoteFilesTarget,
        allow_connect: bool,
    },
    ListDirectory {
        generation: u64,
        source_key: String,
        path: RemotePath,
        limit: usize,
    },
    LoadPreview {
        generation: u64,
        source_key: String,
        path: RemotePath,
    },
    ReleaseLease,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RemoteFilesEvent {
    TargetChanged(Option<RemoteFilesTarget>),
    ConnectRequested,
    ResumeRequested,
    Connected {
        generation: u64,
        root: RemotePath,
        listing: RemoteDirectoryListing,
    },
    ConnectionFailed {
        generation: u64,
        message: String,
    },
    ResumeUnavailable {
        generation: u64,
    },
    ToggleDirectory(RemotePath),
    /// A directory's contents changed underneath us (an upload landed there),
    /// so re-list just that directory. Unlike [`RemoteFilesEvent::Refresh`]
    /// this keeps the rest of the tree and its expansion state intact.
    DirectoryInvalidated(RemotePath),
    DirectoryLoaded {
        generation: u64,
        path: RemotePath,
        listing: RemoteDirectoryListing,
    },
    DirectoryFailed {
        generation: u64,
        path: RemotePath,
        message: String,
    },
    SelectFile(RemotePath),
    /// An entry stopped existing under its old path (deleted, or renamed
    /// away): drop its whole cached subtree NOW, not on the next re-list.
    /// Orphaned listings would keep eating the row budget, and a selection
    /// under the old path would keep a preview open for a file that is gone.
    EntryForgotten(RemotePath),
    PreviewFinished {
        generation: u64,
        path: RemotePath,
        error: Option<String>,
    },
    ClosePreview,
    Refresh,
    PanelHidden,
}

impl RemoteFilesState {
    pub(crate) fn source_key(source: &RemoteFilesSource) -> String {
        match source {
            RemoteFilesSource::SshHost(host) => format!("ssh-host:{host}"),
            RemoteFilesSource::ClientDomain(domain) => format!("client-domain:{domain}"),
        }
    }

    pub(crate) fn current_source_key(&self) -> Option<String> {
        self.target
            .as_ref()
            .map(|target| Self::source_key(&target.source))
    }

    fn tree_cache_key(target: &RemoteFilesTarget) -> TreeCacheKey {
        (
            Self::source_key(&target.source),
            target.requested_root.clone(),
        )
    }

    fn current_tree_cache_key(&self) -> Option<TreeCacheKey> {
        self.target.as_ref().map(Self::tree_cache_key)
    }

    pub(crate) fn can_resume_current(&self) -> bool {
        self.current_source_key()
            .is_some_and(|key| remote_source_is_authorized(&key))
    }

    pub(crate) fn transition(&mut self, event: RemoteFilesEvent) -> Vec<RemoteFilesEffect> {
        match event {
            RemoteFilesEvent::TargetChanged(target) => {
                if self.target == target {
                    return Vec::new();
                }
                // Switching away is not a teardown: keep this source's tree so
                // coming back is instant, and drop only what belongs to the
                // outgoing view.
                self.stash_current_tree();
                self.generation = self.generation.wrapping_add(1);
                self.phase = RemoteFilesPhase::Disconnected;
                self.root = None;
                self.directories.clear();
                self.expanded.clear();
                self.loading_directories.clear();
                self.selected = None;
                self.preview = RemotePreviewStatus::None;
                self.error_message = None;
                self.awaiting_lease = false;
                self.target = target;
                // The lease belongs to the old target; the pool keeps the
                // session warm for whoever asks next.
                let mut effects = vec![RemoteFilesEffect::ReleaseLease];
                if self.restore_cached_tree() {
                    // Already authorized and already browsed: the cached tree
                    // goes on screen immediately while a lease is re-taken and
                    // the root re-listed underneath it.
                    self.awaiting_lease = true;
                    effects.extend(self.refresh_restored_tree());
                }
                effects
            }
            RemoteFilesEvent::ConnectRequested => self.begin_connect(true),
            RemoteFilesEvent::ResumeRequested => self.begin_connect(false),
            RemoteFilesEvent::Connected {
                generation,
                root,
                mut listing,
            } => {
                if generation != self.generation || self.target.is_none() {
                    return vec![RemoteFilesEffect::ReleaseLease];
                }
                if let Some(key) = self.current_source_key() {
                    authorize_remote_source(&key);
                }
                let was_deferred = std::mem::take(&mut self.awaiting_lease);
                self.phase = RemoteFilesPhase::Connected;
                self.error_message = None;
                self.root = Some(root.clone());
                self.expanded.insert(root.clone());
                if listing.entries.len() > REMOTE_FILE_TREE_ROW_LIMIT {
                    listing.entries.truncate(REMOTE_FILE_TREE_ROW_LIMIT);
                    listing.truncated = true;
                }
                // A restored tree may hang off a differently-resolved root
                // (e.g. the remote home moved); anything not under the fresh
                // root can never render, so it must not eat the row budget.
                self.directories
                    .retain(|path, _| *path == root || path.is_descendant_of(&root));
                self.expanded
                    .retain(|path| *path == root || path.is_descendant_of(&root));
                self.loading_directories
                    .retain(|path| *path == root || path.is_descendant_of(&root));
                self.directories.insert(root, RemoteDirectory { listing });
                self.enforce_row_budget();
                if !was_deferred {
                    return Vec::new();
                }
                // Flush the operations recorded while the restored tree waited
                // for its lease. `loading_directories` and a `Loading` preview
                // are the queue: they captured the user's intent, and the
                // lease that just arrived is what they were waiting on.
                let mut effects = Vec::new();
                let pending: Vec<RemotePath> = self.loading_directories.iter().cloned().collect();
                for path in pending {
                    effects.extend(self.list_directory_effect(path));
                }
                if let RemotePreviewStatus::Loading(path) = &self.preview {
                    if let Some(source_key) = self.current_source_key() {
                        effects.push(RemoteFilesEffect::LoadPreview {
                            generation: self.generation,
                            source_key,
                            path: path.clone(),
                        });
                    }
                }
                effects
            }
            RemoteFilesEvent::ConnectionFailed {
                generation,
                message,
            } => {
                if generation == self.generation {
                    // A failed source must not leave a cached tree behind:
                    // switching away and back would restore rows belonging to a
                    // connection that is already gone.
                    self.discard_cached_tree_for_current_target();
                    self.awaiting_lease = false;
                    self.phase = RemoteFilesPhase::Failed(message);
                    self.error_message = None;
                    self.root = None;
                    self.directories.clear();
                    self.expanded.clear();
                    self.loading_directories.clear();
                    self.selected = None;
                    self.preview = RemotePreviewStatus::None;
                }
                Vec::new()
            }
            RemoteFilesEvent::ResumeUnavailable { generation } => {
                if generation == self.generation {
                    self.awaiting_lease = false;
                    self.phase = RemoteFilesPhase::Disconnected;
                    self.error_message = None;
                }
                Vec::new()
            }
            RemoteFilesEvent::ToggleDirectory(path) => {
                if !matches!(self.phase, RemoteFilesPhase::Connected) {
                    return Vec::new();
                }
                if self.expanded.remove(&path) {
                    self.remove_descendants(&path);
                    return Vec::new();
                }
                self.expanded.insert(path.clone());
                if self.directories.contains_key(&path)
                    || !self.loading_directories.insert(path.clone())
                {
                    return Vec::new();
                }
                if self.awaiting_lease {
                    // Recorded in `loading_directories`; `Connected` flushes it
                    // once the lease for the restored tree lands.
                    return Vec::new();
                }
                self.list_directory_effect(path)
            }
            RemoteFilesEvent::DirectoryInvalidated(path) => {
                if !matches!(self.phase, RemoteFilesPhase::Connected)
                    || !self.expanded.contains(&path)
                {
                    // Not on screen: whatever changed will be read fresh
                    // whenever this directory is expanded next.
                    return Vec::new();
                }
                // Drop the stale listing first so the re-list is budgeted
                // against the rows that actually remain.
                self.directories.remove(&path);
                if !self.loading_directories.insert(path.clone()) {
                    // Already being fetched; that request will bring the new
                    // contents with it.
                    return Vec::new();
                }
                if self.awaiting_lease {
                    return Vec::new();
                }
                self.list_directory_effect(path)
            }
            RemoteFilesEvent::DirectoryLoaded {
                generation,
                path,
                mut listing,
            } => {
                if generation != self.generation || !self.expanded.contains(&path) {
                    return Vec::new();
                }
                self.loading_directories.remove(&path);
                self.error_message = None;
                let retained_without_current = self.total_entries().saturating_sub(
                    self.directories
                        .get(&path)
                        .map_or(0, |directory| directory.listing.entries.len()),
                );
                let remaining = REMOTE_FILE_TREE_ROW_LIMIT.saturating_sub(retained_without_current);
                if listing.entries.len() > remaining {
                    listing.entries.truncate(remaining);
                    listing.truncated = true;
                }
                self.directories.insert(path, RemoteDirectory { listing });
                Vec::new()
            }
            RemoteFilesEvent::DirectoryFailed {
                generation,
                path,
                message,
            } => {
                if generation == self.generation {
                    self.loading_directories.remove(&path);
                    self.error_message = Some(message);
                }
                Vec::new()
            }
            RemoteFilesEvent::SelectFile(path) => {
                self.selected = Some(path.clone());
                self.preview = RemotePreviewStatus::Loading(path.clone());
                if self.awaiting_lease {
                    // The `Loading` preview is the queue entry; `Connected`
                    // flushes it once the lease lands.
                    return Vec::new();
                }
                let Some(source_key) = self.current_source_key() else {
                    return Vec::new();
                };
                vec![RemoteFilesEffect::LoadPreview {
                    generation: self.generation,
                    source_key,
                    path,
                }]
            }
            RemoteFilesEvent::EntryForgotten(path) => {
                // remove_descendants clears everything BELOW the path plus a
                // selection under it; the path itself needs its own expansion
                // and selection cleared too.
                self.remove_descendants(&path);
                self.expanded.remove(&path);
                if self.selected.as_ref() == Some(&path) {
                    self.selected = None;
                    self.preview = RemotePreviewStatus::None;
                }
                Vec::new()
            }
            RemoteFilesEvent::PreviewFinished {
                generation,
                path,
                error,
            } => {
                if generation != self.generation || self.selected.as_ref() != Some(&path) {
                    return Vec::new();
                }
                self.preview = match error {
                    Some(error) => RemotePreviewStatus::Failed(path, error),
                    None => RemotePreviewStatus::Ready(path),
                };
                Vec::new()
            }
            RemoteFilesEvent::ClosePreview => {
                self.selected = None;
                self.preview = RemotePreviewStatus::None;
                Vec::new()
            }
            RemoteFilesEvent::Refresh => {
                if self.awaiting_lease {
                    // A refresh is already in flight: that is literally what
                    // the restored tree is waiting on. Bumping the generation
                    // here would orphan it and leave the panel stuck.
                    return Vec::new();
                }
                let Some(root) = self.root.clone() else {
                    return Vec::new();
                };
                self.generation = self.generation.wrapping_add(1);
                self.directories.clear();
                self.error_message = None;
                self.expanded.clear();
                self.expanded.insert(root.clone());
                self.loading_directories.clear();
                self.loading_directories.insert(root.clone());
                self.selected = None;
                self.preview = RemotePreviewStatus::None;
                self.list_directory_effect(root)
            }
            RemoteFilesEvent::PanelHidden => {
                // Hiding keeps the structure (it is small) and gives back the
                // lease plus the preview, which is where the memory actually
                // is. `release_cached_trees` frees the rest if the panel stays
                // hidden long enough.
                self.stash_current_tree();
                self.generation = self.generation.wrapping_add(1);
                self.phase = RemoteFilesPhase::Disconnected;
                self.root = None;
                self.directories.clear();
                self.expanded.clear();
                self.loading_directories.clear();
                self.selected = None;
                self.preview = RemotePreviewStatus::None;
                self.error_message = None;
                self.awaiting_lease = false;
                vec![RemoteFilesEffect::ReleaseLease]
            }
        }
    }

    fn begin_connect(&mut self, allow_connect: bool) -> Vec<RemoteFilesEffect> {
        let Some(target) = self.target.clone() else {
            return Vec::new();
        };
        let source_key = Self::source_key(&target.source);
        let authorized = remote_source_is_authorized(&source_key);
        if !allow_connect && !authorized {
            // Never dial a source the user has not asked for.
            return Vec::new();
        }
        if matches!(
            self.phase,
            RemoteFilesPhase::Connecting | RemoteFilesPhase::Connected
        ) {
            return Vec::new();
        }
        self.generation = self.generation.wrapping_add(1);
        self.phase = RemoteFilesPhase::Connecting;
        vec![RemoteFilesEffect::Connect {
            generation: self.generation,
            target,
            // An already-authorized source may redial: once the pool's 15
            // minute idle window lapses, silently reconnecting is better than
            // dropping the user back onto a Connect button they already
            // pressed for this host.
            allow_connect: allow_connect || authorized,
        }]
    }

    /// Re-acquire a lease and refresh the root for a tree that was restored
    /// from cache. Unlike [`Self::begin_connect`] this runs while the phase is
    /// already `Connected`: the user keeps looking at the cached listing
    /// instead of a spinner, and `Connected` swaps in the fresh root when it
    /// lands. A failure here is real (the session is gone) and falls through to
    /// the normal failed state.
    fn refresh_restored_tree(&mut self) -> Vec<RemoteFilesEffect> {
        let Some(target) = self.target.clone() else {
            return Vec::new();
        };
        self.generation = self.generation.wrapping_add(1);
        vec![RemoteFilesEffect::Connect {
            generation: self.generation,
            target,
            allow_connect: true,
        }]
    }

    fn list_directory_effect(&self, path: RemotePath) -> Vec<RemoteFilesEffect> {
        let Some(source_key) = self.current_source_key() else {
            return Vec::new();
        };
        let remaining = REMOTE_FILE_TREE_ROW_LIMIT.saturating_sub(self.total_entries());
        vec![RemoteFilesEffect::ListDirectory {
            generation: self.generation,
            source_key,
            path,
            limit: remaining,
        }]
    }

    fn total_entries(&self) -> usize {
        self.directories
            .values()
            .map(|directory| directory.listing.entries.len())
            .sum()
    }

    fn remove_descendants(&mut self, path: &RemotePath) {
        self.expanded
            .retain(|candidate| !candidate.is_descendant_of(path));
        self.loading_directories
            .retain(|candidate| candidate != path && !candidate.is_descendant_of(path));
        self.directories
            .retain(|candidate, _| candidate != path && !candidate.is_descendant_of(path));
        if self
            .selected
            .as_ref()
            .is_some_and(|selected| selected.is_descendant_of(path))
        {
            self.selected = None;
            self.preview = RemotePreviewStatus::None;
        }
    }

    /// Forget any cached tree for the target currently in view. Used when that
    /// target is known bad, so the cache cannot resurrect it later.
    fn discard_cached_tree_for_current_target(&mut self) {
        let Some(cache_key) = self.current_tree_cache_key() else {
            return;
        };
        self.cached_trees.remove(&cache_key);
        if let Some(index) = self
            .cached_tree_order
            .iter()
            .position(|key| *key == cache_key)
        {
            self.cached_tree_order.remove(index);
        }
    }

    /// Move the loaded tree into the cache instead of dropping it, so switching
    /// back to this target can show it again without re-listing.
    fn stash_current_tree(&mut self) {
        let (Some(cache_key), Some(root)) = (self.current_tree_cache_key(), self.root.clone())
        else {
            return;
        };
        if self.directories.is_empty() {
            return;
        }
        let tree = CachedTree {
            root,
            directories: std::mem::take(&mut self.directories),
            expanded: std::mem::take(&mut self.expanded),
            selected: self.selected.take(),
        };
        if self.cached_trees.insert(cache_key.clone(), tree).is_none() {
            self.cached_tree_order.push_back(cache_key);
        } else {
            self.touch_cached_tree(&cache_key);
        }
        while self.cached_tree_order.len() > REMOTE_TREE_CACHE_CAP {
            if let Some(evicted) = self.cached_tree_order.pop_front() {
                self.cached_trees.remove(&evicted);
            }
        }
    }

    fn touch_cached_tree(&mut self, cache_key: &TreeCacheKey) {
        if let Some(index) = self
            .cached_tree_order
            .iter()
            .position(|key| key == cache_key)
        {
            self.cached_tree_order.remove(index);
        }
        self.cached_tree_order.push_back(cache_key.clone());
    }

    /// Put a cached tree back on screen. Only legitimate for a source the user
    /// already authorized — the connection pool's liveness probe makes reusing
    /// the underlying session safe, and a stale listing is corrected by the
    /// next expand.
    fn restore_cached_tree(&mut self) -> bool {
        let Some(cache_key) = self.current_tree_cache_key() else {
            return false;
        };
        if !remote_source_is_authorized(&cache_key.0) {
            return false;
        }
        let Some(tree) = self.cached_trees.remove(&cache_key) else {
            return false;
        };
        if let Some(index) = self
            .cached_tree_order
            .iter()
            .position(|key| *key == cache_key)
        {
            self.cached_tree_order.remove(index);
        }
        self.root = Some(tree.root);
        self.directories = tree.directories;
        self.expanded = tree.expanded;
        self.selected = tree.selected;
        self.phase = RemoteFilesPhase::Connected;
        self.error_message = None;
        true
    }

    /// Trim the tree back under the global row budget. A restored cache can
    /// hold up to the full budget on its own, and the refreshed root listing
    /// arrives budgeted only against itself, so their union can exceed the
    /// bound. Deepest directories collapse first, keeping the shallow
    /// structure the user is most likely looking at.
    fn enforce_row_budget(&mut self) {
        let Some(root) = self.root.clone() else {
            return;
        };
        while self.total_entries() > REMOTE_FILE_TREE_ROW_LIMIT {
            let Some(victim) = self
                .directories
                .keys()
                .filter(|path| **path != root)
                .max_by_key(|path| path.as_str().matches('/').count())
                .cloned()
            else {
                break;
            };
            self.expanded.remove(&victim);
            self.remove_descendants(&victim);
        }
    }

    /// Forget the selected row and any preview tied to it. Mirrors what
    /// closing the preview does to the local panel's `file_selected`, and is
    /// what stops `right_sidebar_file_preview_active` from keeping the pane
    /// open (and the sidebar wide) after the preview is gone.
    pub(crate) fn clear_selection(&mut self) {
        self.selected = None;
        self.preview = RemotePreviewStatus::None;
    }

    /// Drop every cached tree. Wired into the sidebar's existing idle-release
    /// path so a panel left hidden eventually gives the memory back.
    pub(crate) fn release_cached_trees(&mut self) {
        self.cached_trees.clear();
        self.cached_tree_order.clear();
    }

    pub(crate) fn rows(&self) -> Vec<RemoteFileRow> {
        let Some(root) = self.root.as_ref() else {
            return Vec::new();
        };
        let root_name = self
            .target
            .as_ref()
            .map(|target| target.project_name.clone())
            .unwrap_or_else(|| root.file_name().to_string());
        let mut rows = vec![RemoteFileRow {
            entry: RemoteFileEntry {
                path: root.clone(),
                name: root_name,
                kind: RemoteFileKind::Directory,
                size: None,
            },
            depth: 0,
            expanded: self.expanded.contains(root),
        }];
        self.append_rows(root, 1, &mut rows);
        rows
    }

    fn append_rows(&self, parent: &RemotePath, depth: usize, rows: &mut Vec<RemoteFileRow>) {
        if !self.expanded.contains(parent) {
            return;
        }
        let Some(directory) = self.directories.get(parent) else {
            return;
        };
        for entry in &directory.listing.entries {
            let expanded = entry.is_directory() && self.expanded.contains(&entry.path);
            rows.push(RemoteFileRow {
                entry: entry.clone(),
                depth,
                expanded,
            });
            if entry.is_directory() {
                self.append_rows(&entry.path, depth.saturating_add(1), rows);
            }
        }
    }

    pub(crate) fn has_truncated_directory(&self) -> bool {
        self.directories
            .values()
            .any(|directory| directory.listing.truncated)
    }

    /// Whether this directory currently holds a loaded listing. Used to tell
    /// "the row is not there" apart from "its directory has not loaded yet".
    pub(crate) fn has_listing(&self, path: &RemotePath) -> bool {
        self.directories.contains_key(path)
    }

    pub(crate) fn kind_for_path(&self, path: &RemotePath) -> Option<RemoteFileKind> {
        if self.root.as_ref() == Some(path) {
            return Some(RemoteFileKind::Directory);
        }
        self.directories
            .values()
            .flat_map(|directory| directory.listing.entries.iter())
            .find(|entry| &entry.path == path)
            .map(|entry| entry.kind)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    enum FakeRenameResult {
        #[default]
        Success,
        FailBeforeMove,
        FailAfterMove,
    }

    #[derive(Default)]
    struct FakeRenameState {
        entries: HashMap<String, (RemoteRenameEntryKind, &'static str)>,
        replace_result: FakeRenameResult,
    }

    #[derive(Clone, Default)]
    struct FakeRenameBackend {
        state: Arc<Mutex<FakeRenameState>>,
    }

    impl FakeRenameBackend {
        fn insert(&self, path: &str, kind: RemoteRenameEntryKind, owner: &'static str) {
            self.state
                .lock()
                .unwrap()
                .entries
                .insert(path.to_string(), (kind, owner));
        }

        fn owner(&self, path: &str) -> Option<&'static str> {
            self.state
                .lock()
                .unwrap()
                .entries
                .get(path)
                .map(|(_, owner)| *owner)
        }

        fn set_replace_result(&self, result: FakeRenameResult) {
            self.state.lock().unwrap().replace_result = result;
        }
    }

    impl RemoteRenameBackend for FakeRenameBackend {
        fn inspect(&self, path: String) -> RemoteFuture<Option<RemoteRenameEntryKind>> {
            let state = Arc::clone(&self.state);
            Box::pin(async move {
                Ok(state
                    .lock()
                    .unwrap()
                    .entries
                    .get(&path)
                    .map(|(kind, _)| *kind))
            })
        }

        fn reserve(&self, path: String, kind: RemoteRenameEntryKind) -> RemoteFuture<()> {
            let state = Arc::clone(&self.state);
            Box::pin(async move {
                let mut state = state.lock().unwrap();
                if state.entries.contains_key(&path) {
                    return Err("occupied".to_string());
                }
                state.entries.insert(path, (kind, "reservation"));
                Ok(())
            })
        }

        fn replace(&self, from: String, to: String) -> RemoteFuture<()> {
            let state = Arc::clone(&self.state);
            Box::pin(async move {
                let mut state = state.lock().unwrap();
                if state.replace_result == FakeRenameResult::FailBeforeMove {
                    return Err("rename refused".to_string());
                }
                let source = state
                    .entries
                    .remove(&from)
                    .ok_or_else(|| "source missing".to_string())?;
                state.entries.insert(to, source);
                if state.replace_result == FakeRenameResult::FailAfterMove {
                    Err("reply lost".to_string())
                } else {
                    Ok(())
                }
            })
        }

        fn release(&self, path: String, kind: RemoteRenameEntryKind) -> RemoteFuture<()> {
            let state = Arc::clone(&self.state);
            Box::pin(async move {
                let mut state = state.lock().unwrap();
                match state.entries.get(&path) {
                    Some((reserved_kind, "reservation")) if *reserved_kind == kind => {
                        state.entries.remove(&path);
                        Ok(())
                    }
                    _ => Err("not our reservation".to_string()),
                }
            })
        }
    }

    fn remote_path(path: &str) -> RemotePath {
        RemotePath::from_server_absolute(path).unwrap()
    }

    #[test]
    fn remote_rename_never_replaces_an_existing_target() {
        let backend = FakeRenameBackend::default();
        backend.insert("/from", RemoteRenameEntryKind::Other, "source");
        backend.insert("/to", RemoteRenameEntryKind::Other, "existing");

        let error = smol::block_on(rename_without_overwrite(
            &backend,
            &remote_path("/from"),
            &remote_path("/to"),
        ))
        .expect_err("an occupied target must refuse the rename");

        assert!(error.contains("already exists"));
        assert_eq!(backend.owner("/from"), Some("source"));
        assert_eq!(backend.owner("/to"), Some("existing"));
    }

    #[test]
    fn failed_remote_rename_releases_only_its_own_reservation() {
        let backend = FakeRenameBackend::default();
        backend.insert("/from", RemoteRenameEntryKind::Directory, "source");
        backend.set_replace_result(FakeRenameResult::FailBeforeMove);

        smol::block_on(rename_without_overwrite(
            &backend,
            &remote_path("/from"),
            &remote_path("/to"),
        ))
        .expect_err("the fake rejects the move");

        assert_eq!(backend.owner("/from"), Some("source"));
        assert_eq!(backend.owner("/to"), None);
    }

    #[test]
    fn ambiguous_remote_rename_error_never_deletes_the_moved_source() {
        let backend = FakeRenameBackend::default();
        backend.insert("/from", RemoteRenameEntryKind::Other, "source");
        backend.set_replace_result(FakeRenameResult::FailAfterMove);

        let error = smol::block_on(rename_without_overwrite(
            &backend,
            &remote_path("/from"),
            &remote_path("/to"),
        ))
        .expect_err("the reply was lost");

        assert!(error.contains("left untouched"));
        assert_eq!(backend.owner("/from"), None);
        assert_eq!(backend.owner("/to"), Some("source"));
    }

    /// Records how many times the remote home was asked for, so a test can
    /// prove an absolute root never pays for that round-trip.
    #[derive(Default)]
    struct HomeProbeCounter {
        calls: AtomicUsize,
        home: Option<&'static str>,
    }

    impl HomeProbeCounter {
        /// Mirrors `SftpRemoteFileBackend::resolve_root` without a server: the
        /// closure stands in for `sftp.canonicalize`.
        fn resolve(&self, requested: &str) -> Result<RemotePath, String> {
            if !requested_root_needs_home(requested) {
                return remote_root_from_absolute(requested);
            }
            self.calls.fetch_add(1, Ordering::SeqCst);
            let home = self
                .home
                .ok_or_else(|| "Can't read the remote home directory: denied".to_string())?;
            resolve_requested_root(RemotePath::from_server_absolute(home)?, requested)
        }
    }

    fn with_home() -> HomeProbeCounter {
        HomeProbeCounter {
            calls: AtomicUsize::new(0),
            home: Some("/home/ada"),
        }
    }

    #[test]
    fn absolute_root_never_resolves_the_remote_home() {
        // The bug this pins: `resolve_root` used to canonicalize `.` first, so a
        // project with a fully specified path still broke on any server that
        // could not answer that request.
        let probe = with_home();
        assert_eq!(probe.resolve("/srv/app").unwrap().as_str(), "/srv/app");
        assert_eq!(probe.calls.load(Ordering::SeqCst), 0);

        // Even with no home available at all, an absolute root still works.
        let no_home = HomeProbeCounter::default();
        assert_eq!(no_home.resolve("/srv/app").unwrap().as_str(), "/srv/app");
        assert_eq!(no_home.calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn tilde_resolves_to_the_remote_home() {
        let probe = with_home();
        assert_eq!(probe.resolve("~").unwrap().as_str(), "/home/ada");
        assert_eq!(probe.calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn tilde_child_joins_beneath_the_home() {
        let probe = with_home();
        assert_eq!(
            probe.resolve("~/code/thinkterm").unwrap().as_str(),
            "/home/ada/code/thinkterm"
        );
        // Redundant separators and `.` segments collapse rather than escaping.
        assert_eq!(
            probe.resolve("~/code//./thinkterm").unwrap().as_str(),
            "/home/ada/code/thinkterm"
        );
    }

    #[test]
    fn traversal_is_rejected_on_both_paths() {
        let probe = with_home();
        // `~/..` goes through join_name, which refuses `..`.
        assert!(probe.resolve("~/../etc").is_err());
        // An absolute root is checked before it can ever reach the server.
        let err = probe.resolve("/srv/../etc").unwrap_err();
        assert!(err.contains(".."), "{}", err);
    }

    #[test]
    fn relative_roots_are_rejected_with_a_clear_message() {
        let probe = with_home();
        for bad in ["relative/path", "", "C:/windows"] {
            let err = probe.resolve(bad).unwrap_err();
            assert!(
                err.contains("absolute path or ~"),
                "{:?} produced {:?}",
                bad,
                err
            );
        }
        // The failure is decided locally; no home lookup was attempted.
        assert_eq!(probe.calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn remote_paths_stay_slash_only() {
        // Guards the Windows client: joining must never introduce a `\`, and a
        // backslash is an ordinary character in a POSIX name.
        let root = RemotePath::from_server_absolute("/srv").unwrap();
        let child = root.join_name("a b").unwrap();
        assert_eq!(child.as_str(), "/srv/a b");
        assert_eq!(root.join_name("we\\ird").unwrap().as_str(), "/srv/we\\ird");
        assert!(root.join_name("nested/name").is_err());
        assert!(root.join_name("..").is_err());
        // A trailing slash from the server is normalized away, but root stays "/".
        assert_eq!(
            RemotePath::from_server_absolute("/srv/app/")
                .unwrap()
                .as_str(),
            "/srv/app"
        );
        assert_eq!(RemotePath::from_server_absolute("/").unwrap().as_str(), "/");
    }

    #[test]
    fn an_idle_connection_that_died_is_replaced_without_the_user_retrying() {
        // The reported symptom: the panel occasionally fails with "can't read
        // the remote home" and works on a second attempt. Reusing a cached
        // connection that the server reaped while the panel was closed must
        // redial transparently instead of surfacing that failure.
        let calls = Arc::new(AtomicUsize::new(0));
        let probes = Arc::new(AtomicUsize::new(0));
        let manager = RemoteConnectionManager::new(
            Arc::new(DyingConnector {
                calls: Arc::clone(&calls),
                probes: Arc::clone(&probes),
                first_is_alive: Arc::new(AtomicBool::new(true)),
            }),
            Duration::from_secs(300),
        );
        let key = "host".to_string();

        let lease = smol::block_on(manager.acquire(key.clone(), SshDomain::default(), true))
            .expect("first acquire");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        // A fresh connection is not probed; it was just built.
        assert_eq!(probes.load(Ordering::SeqCst), 0);

        // Panel closed: the connection goes idle and (unknown to us) dies.
        drop(lease);

        // Panel reopened. The cached entry is probed, found dead, and replaced
        // — the caller still gets a usable lease on the first try.
        let lease = smol::block_on(manager.acquire(key.clone(), SshDomain::default(), true))
            .expect("acquire must transparently redial");
        assert_eq!(
            probes.load(Ordering::SeqCst),
            1,
            "cached entry must be probed"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2, "must have redialled");

        // Still leased, so the next acquire skips the probe entirely.
        let second = smol::block_on(manager.acquire(key.clone(), SshDomain::default(), true))
            .expect("concurrent acquire");
        assert_eq!(
            probes.load(Ordering::SeqCst),
            1,
            "live lease must not be probed"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        drop(second);
        drop(lease);
    }

    /// Backend whose probe blocks until the test opens the gate, so the test
    /// can observe what other acquires do while a probe is in flight.
    struct GatedProbeBackend {
        probes: Arc<AtomicUsize>,
        gate: smol::channel::Receiver<()>,
    }

    impl RemoteFileBackend for GatedProbeBackend {
        fn resolve_root(&self, _requested: String) -> RemoteFuture<RemotePath> {
            Box::pin(async { RemotePath::from_server_absolute("/home/test") })
        }

        fn list_directory(
            &self,
            _path: RemotePath,
            _limit: usize,
        ) -> RemoteFuture<RemoteDirectoryListing> {
            Box::pin(async {
                Ok(RemoteDirectoryListing {
                    entries: Vec::new(),
                    truncated: false,
                })
            })
        }

        fn read_file(&self, _path: RemotePath, _limit: usize) -> RemoteFuture<RemoteFileBytes> {
            Box::pin(async {
                Ok(RemoteFileBytes {
                    bytes: Vec::new(),
                    truncated: false,
                })
            })
        }

        fn probe(&self) -> RemoteFuture<()> {
            self.probes.fetch_add(1, Ordering::SeqCst);
            let gate = self.gate.clone();
            Box::pin(async move {
                let _ = gate.recv().await;
                Ok(())
            })
        }
    }

    struct GatedProbeConnector {
        probes: Arc<AtomicUsize>,
        gate: smol::channel::Receiver<()>,
    }

    impl RemoteFileConnector for GatedProbeConnector {
        fn connect(&self, _config: SshDomain) -> RemoteFuture<Arc<dyn RemoteFileBackend>> {
            let backend = GatedProbeBackend {
                probes: Arc::clone(&self.probes),
                gate: self.gate.clone(),
            };
            Box::pin(async move { Ok(Arc::new(backend) as Arc<dyn RemoteFileBackend>) })
        }
    }

    /// While one acquire verifies an idle connection, a concurrent acquire
    /// must wait for the verdict. Handing out the unverified backend meant a
    /// caller could run operations on a session about to be declared dead —
    /// and its failure report could then tear down the healthy replacement.
    #[test]
    fn acquires_during_a_liveness_probe_wait_for_its_verdict() {
        let probes = Arc::new(AtomicUsize::new(0));
        let (open_gate, gate) = smol::channel::unbounded::<()>();
        let manager = RemoteConnectionManager::new(
            Arc::new(GatedProbeConnector {
                probes: Arc::clone(&probes),
                gate,
            }),
            Duration::from_secs(300),
        );
        let key = "host".to_string();

        smol::block_on(async {
            let lease = manager
                .acquire(key.clone(), SshDomain::default(), true)
                .await
                .expect("first acquire");
            drop(lease); // idle: the next acquire must probe

            let prober = {
                let manager = Arc::clone(&manager);
                let key = key.clone();
                smol::spawn(async move { manager.acquire(key, SshDomain::default(), true).await })
            };
            while probes.load(Ordering::SeqCst) == 0 {
                smol::Timer::after(Duration::from_millis(1)).await;
            }

            let (done_tx, done_rx) = smol::channel::bounded::<()>(1);
            let waiter = {
                let manager = Arc::clone(&manager);
                let key = key.clone();
                smol::spawn(async move {
                    let lease = manager.acquire(key, SshDomain::default(), true).await;
                    let _ = done_tx.send(()).await;
                    lease
                })
            };
            smol::Timer::after(Duration::from_millis(50)).await;
            assert!(
                done_rx.try_recv().is_err(),
                "the second acquire must wait for the probe verdict"
            );
            assert_eq!(
                probes.load(Ordering::SeqCst),
                1,
                "the waiter must not start a second probe"
            );

            open_gate.send(()).await.expect("open the probe gate");
            let first = prober.await.expect("probing acquire");
            let second = waiter.await.expect("waiting acquire");
            assert_eq!(
                probes.load(Ordering::SeqCst),
                1,
                "one probe verdict serves every waiter"
            );
            drop(first);
            drop(second);
        });
    }

    /// The liveness probe accepts the same home spellings as root resolution;
    /// a server that only answers the empty path is alive, and treating it as
    /// dead forced a redial on every idle reacquire.
    #[test]
    fn the_home_probe_falls_back_to_the_empty_spelling() {
        let calls = std::cell::RefCell::new(Vec::new());
        let result = smol::block_on(canonicalize_remote_home(|spelling| {
            calls.borrow_mut().push(spelling);
            async move {
                if spelling.is_empty() {
                    Ok("/home/ada".to_string())
                } else {
                    Err("Library-specific error")
                }
            }
        }));
        assert_eq!(result.expect("fallback must succeed"), "/home/ada");
        assert_eq!(*calls.borrow(), vec![".", ""]);

        let result: Result<String, Vec<&str>> =
            smol::block_on(canonicalize_remote_home(|_| async { Err("denied") }));
        assert_eq!(
            result.expect_err("every spelling failed"),
            vec!["denied", "denied"],
            "failures must be reported in spelling order"
        );
    }

    #[test]
    fn a_failed_startup_drops_the_cached_connection_so_retry_redials() {
        // The reported symptom: after one good session, reconnecting fails and
        // Retry keeps failing. Whatever broke the session, the panel must not
        // hand the same one back on the next attempt.
        let calls = Arc::new(AtomicUsize::new(0));
        let manager = RemoteConnectionManager::new(
            Arc::new(FakeConnector {
                calls: Arc::clone(&calls),
            }),
            Duration::from_secs(30),
        );
        let key = "host".to_string();

        let lease = smol::block_on(manager.acquire(key.clone(), SshDomain::default(), true))
            .expect("first acquire");
        drop(lease);
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        // Still cached: a plain reconnect reuses the session without dialling.
        let reused = smol::block_on(manager.acquire(key.clone(), SshDomain::default(), true))
            .expect("cached acquire");
        drop(reused);
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        // Startup failed on that cached session, so it is thrown away and the
        // next attempt dials a fresh one instead of replaying the failure.
        manager.invalidate(&key);
        let fresh = smol::block_on(manager.acquire(key.clone(), SshDomain::default(), true))
            .expect("acquire after invalidation");
        drop(fresh);
        assert_eq!(calls.load(Ordering::SeqCst), 2);

        // And a resume (which must never dial on its own) correctly reports
        // that there is nothing to resume once the entry is gone.
        manager.invalidate(&key);
        assert!(matches!(
            smol::block_on(manager.acquire(key, SshDomain::default(), false)),
            Err(RemoteAcquireError::NotConnected)
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn a_dead_transport_is_detected_from_the_concise_message_too() {
        // `resolve_root` hands the UI only the innermost cause, so invalidation
        // must recognise the transport failure from that alone — otherwise
        // Retry keeps handing back the same dead cached connection.
        for message in [
            "sending into a closed channel",
            "receiving from an empty and closed channel",
            "Failed to send request: sending into a closed channel",
            "session is dead",
            "Broken pipe",
        ] {
            assert!(
                invalidate_remote_connection_if_dead("unused-key", 0, message),
                "{:?} should invalidate",
                message
            );
        }
        // A server-side refusal is not a dead connection: keep the session.
        for message in ["Permission denied", "No such file or directory"] {
            assert!(
                !invalidate_remote_connection_if_dead("unused-key", 0, message),
                "{:?} should not invalidate",
                message
            );
        }
    }

    #[test]
    fn error_chain_keeps_the_specific_cause() {
        // "Library-specific error" alone is what made the UI message useless.
        #[derive(Debug)]
        struct Inner;
        impl std::fmt::Display for Inner {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "Permission denied")
            }
        }
        impl std::error::Error for Inner {}

        #[derive(Debug)]
        struct Outer(Inner);
        impl std::fmt::Display for Outer {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "Library-specific error")
            }
        }
        impl std::error::Error for Outer {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                Some(&self.0)
            }
        }

        let err = Outer(Inner);
        assert_eq!(
            error_chain(&err),
            "Library-specific error: Permission denied"
        );
        assert_eq!(error_summary(&err), "Permission denied");
    }

    /// Yields at most `chunk` bytes per poll, so a test can control how many
    /// times the copy loop goes round.
    struct ChunkReader {
        data: Vec<u8>,
        pos: usize,
        chunk: usize,
    }

    impl smol::io::AsyncRead for ChunkReader {
        fn poll_read(
            mut self: Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            buf: &mut [u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            let remaining = self.data.len() - self.pos;
            let n = remaining.min(self.chunk).min(buf.len());
            let pos = self.pos;
            buf[..n].copy_from_slice(&self.data[pos..pos + n]);
            self.pos += n;
            std::task::Poll::Ready(Ok(n))
        }
    }

    /// Collects everything written, and can trip a cancel after a chosen
    /// number of writes so the abort lands mid-transfer.
    struct VecSink {
        data: Vec<u8>,
        writes: usize,
        flushes: usize,
        cancel_after: Option<(usize, RemoteTransferProgress)>,
    }

    impl VecSink {
        fn new() -> Self {
            Self {
                data: Vec::new(),
                writes: 0,
                flushes: 0,
                cancel_after: None,
            }
        }
    }

    impl smol::io::AsyncWrite for VecSink {
        fn poll_write(
            mut self: Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            buf: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            self.data.extend_from_slice(buf);
            self.writes += 1;
            if let Some((after, progress)) = self.cancel_after.clone() {
                if self.writes >= after {
                    progress.request_cancel();
                }
            }
            std::task::Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(
            mut self: Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            self.flushes += 1;
            std::task::Poll::Ready(Ok(()))
        }

        fn poll_close(
            self: Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    #[test]
    fn a_transfer_moves_every_byte_and_reports_progress() {
        let data: Vec<u8> = (0..10_000u32).map(|index| index as u8).collect();
        let mut source = ChunkReader {
            data: data.clone(),
            pos: 0,
            chunk: 1_000,
        };
        let mut sink = VecSink::new();
        let progress = RemoteTransferProgress::default();
        progress.set_total(data.len() as u64);

        let written = smol::block_on(copy_stream(
            &mut source,
            &mut sink,
            &progress,
            "source",
            "sink",
        ))
        .expect("transfer");

        assert_eq!(written, data.len() as u64);
        assert_eq!(sink.data, data, "the bytes must arrive unchanged");
        assert_eq!(progress.transferred(), data.len() as u64);
        assert_eq!(progress.fraction(), Some(1.0));
        assert_eq!(sink.flushes, 1, "the sink must be flushed exactly once");
    }

    #[test]
    fn canceling_before_the_first_chunk_writes_nothing() {
        let mut source = ChunkReader {
            data: vec![7u8; 4_096],
            pos: 0,
            chunk: 1_024,
        };
        let mut sink = VecSink::new();
        let progress = RemoteTransferProgress::default();
        progress.request_cancel();

        let err = smol::block_on(copy_stream(
            &mut source,
            &mut sink,
            &progress,
            "source",
            "sink",
        ))
        .expect_err("a canceled transfer must fail");

        assert_eq!(err, REMOTE_TRANSFER_CANCELED);
        assert!(sink.data.is_empty(), "nothing may be sent after a cancel");
        assert_eq!(progress.transferred(), 0);
    }

    #[test]
    fn canceling_mid_transfer_stops_at_the_next_chunk() {
        let progress = RemoteTransferProgress::default();
        let mut source = ChunkReader {
            data: vec![3u8; 4_096],
            pos: 0,
            chunk: 1_024,
        };
        let mut sink = VecSink::new();
        // Trip the cancel as the first chunk lands, so the loop aborts on its
        // next pass rather than running to completion.
        sink.cancel_after = Some((1, progress.clone()));

        let err = smol::block_on(copy_stream(
            &mut source,
            &mut sink,
            &progress,
            "source",
            "sink",
        ))
        .expect_err("a canceled transfer must fail");

        assert_eq!(err, REMOTE_TRANSFER_CANCELED);
        assert_eq!(
            sink.data.len(),
            1_024,
            "the chunk already in flight completes, and no more follow"
        );
        assert_eq!(progress.transferred(), 1_024);
        assert_eq!(sink.flushes, 0, "an aborted transfer is never flushed");
    }

    #[test]
    fn transfer_progress_reports_a_usable_fraction() {
        let progress = RemoteTransferProgress::default();
        assert_eq!(progress.total(), None);
        assert_eq!(
            progress.fraction(),
            None,
            "no fraction before the size is known"
        );

        progress.set_total(0);
        assert_eq!(
            progress.fraction(),
            Some(1.0),
            "an empty file is complete once it exists"
        );

        let progress = RemoteTransferProgress::default();
        progress.set_total(200);
        progress.advance(50);
        assert_eq!(progress.fraction(), Some(0.25));
        // A server that reported a stale size must not push the bar past full.
        progress.advance(1_000);
        assert_eq!(progress.fraction(), Some(1.0));
    }

    /// Reserve helper for tests: `taken` stands in for names already on disk,
    /// and every reservation attempt succeeds unless the staging file is one
    /// of them.
    fn reserve_against(taken: &HashSet<PathBuf>) -> impl Fn(&Path) -> bool + '_ {
        move |partial: &Path| !taken.contains(partial)
    }

    #[test]
    fn a_download_never_overwrites_an_existing_file() {
        let dir = Path::new("/home/ada/Downloads");
        let taken: HashSet<PathBuf> = [
            "/home/ada/Downloads/notes.tar.gz",
            "/home/ada/Downloads/notes (1).tar.gz",
            "/home/ada/Downloads/report.pdf",
        ]
        .iter()
        .map(PathBuf::from)
        .collect();
        let exists = |path: &Path| taken.contains(path);
        let taken_path = |dir: &Path, name: &str| dir.join(name);
        // Free name: used as-is.
        assert_eq!(
            reserve_download_path(dir, "fresh.txt", exists, reserve_against(&taken)),
            Some((
                taken_path(dir, "fresh.txt"),
                taken_path(dir, "fresh.txt.part")
            ))
        );
        // Taken once: the first free suffix wins, and a two-part extension
        // stays whole rather than becoming "notes.tar (1).gz".
        assert_eq!(
            reserve_download_path(dir, "notes.tar.gz", exists, reserve_against(&taken))
                .map(|(dest, _)| dest),
            Some(taken_path(dir, "notes (2).tar.gz"))
        );
        assert_eq!(
            reserve_download_path(dir, "report.pdf", exists, reserve_against(&taken))
                .map(|(dest, _)| dest),
            Some(taken_path(dir, "report (1).pdf"))
        );
    }

    /// The staging file is claimed exclusively, so a `.part` that is already
    /// there — another download in flight, or just a file with that name — is
    /// never truncated and never deleted by our cleanup.
    #[test]
    fn a_download_skips_a_name_whose_staging_file_is_taken() {
        let dir = Path::new("/home/ada/Downloads");
        let taken: HashSet<PathBuf> =
            std::iter::once(PathBuf::from("/home/ada/Downloads/ubuntu.iso.part")).collect();
        // The destination itself is free; only its staging file is taken.
        let (destination, partial) =
            reserve_download_path(dir, "ubuntu.iso", |_| false, reserve_against(&taken))
                .expect("a free name exists");
        assert_eq!(destination, dir.join("ubuntu (1).iso"));
        assert_eq!(partial, dir.join("ubuntu (1).iso.part"));
        assert_ne!(
            partial,
            PathBuf::from("/home/ada/Downloads/ubuntu.iso.part"),
            "the pre-existing staging file must be left alone"
        );

        // Nothing free at all: refuse rather than clobber something.
        assert_eq!(
            reserve_download_path(dir, "x.bin", |_| true, |_| true),
            None
        );
    }

    #[test]
    fn a_download_name_cannot_escape_the_download_directory() {
        let dir = Path::new("/home/ada/Downloads");
        // A server controls these names, so a separator, `..`, or a Windows
        // drive prefix must not be able to steer the write anywhere else.
        for hostile in [
            "../../etc/passwd",
            "/etc/passwd",
            "..",
            ".",
            "",
            "C:evil.txt",
            "a/b/c.txt",
        ] {
            let (destination, partial) =
                reserve_download_path(dir, hostile, |_| false, |_| true).expect("a name");
            assert_eq!(
                destination.parent(),
                Some(dir),
                "{hostile:?} escaped to {}",
                destination.display()
            );
            assert_eq!(partial.parent(), Some(dir));
            // And whatever it landed as, this host can actually store it.
            let landed = destination.file_name().and_then(|n| n.to_str()).unwrap();
            assert!(
                crate::termwindow::remote_walk::DownloadNameRules::host().accepts(landed),
                "{:?} landed as {:?}, which this host refuses",
                hostile,
                landed
            );
        }
        // Names this host cannot store are rewritten rather than refused: the
        // transfer is what was asked for, the spelling is incidental. On unix
        // they are ordinary names and come through untouched.
        for reserved in ["report:2024.txt", "why?.txt", "CON", "trailing."] {
            let (destination, _) =
                reserve_download_path(dir, reserved, |_| false, |_| true).expect("a name");
            let landed = destination.file_name().and_then(|n| n.to_str()).unwrap();
            assert!(
                crate::termwindow::remote_walk::DownloadNameRules::host().accepts(landed),
                "{:?} landed as {:?}, which this host refuses",
                reserved,
                landed
            );
            if cfg!(unix) {
                assert_eq!(landed, reserved, "unix has no quarrel with this name");
            }
        }
        // A dotfile keeps its leading dot instead of being read as extension.
        let mut names = download_name_candidates(".bashrc");
        assert_eq!(names.next().as_deref(), Some(".bashrc"));
        assert_eq!(names.next().as_deref(), Some(".bashrc (1)"));

        // Dots inside a stem are ordinary characters: a macOS screenshot must
        // number before `.png`, not in the middle of its timestamp.
        let mut names = download_name_candidates("Screenshot 2026-07-31 at 12.50.59 PM.png");
        names.next();
        assert_eq!(
            names.next().as_deref(),
            Some("Screenshot 2026-07-31 at 12.50.59 PM (1).png")
        );
        let mut names = download_name_candidates("no-extension");
        names.next();
        assert_eq!(names.next().as_deref(), Some("no-extension (1)"));
    }

    /// A folder keeps its dots: the file rule would turn `my.folder` into
    /// `my (1).folder`, naming a sibling that has nothing to do with it.
    #[test]
    fn a_downloaded_folder_is_numbered_at_the_end_of_its_name() {
        let mut names = folder_download_name_candidates("my.folder");
        assert_eq!(names.next().as_deref(), Some("my.folder"));
        assert_eq!(names.next().as_deref(), Some("my.folder (1)"));

        // Server-controlled names are reduced to one component, exactly like
        // file downloads.
        let mut hostile = folder_download_name_candidates("../../etc");
        assert_eq!(hostile.next().as_deref(), Some("etc"));

        let dir = Path::new("/home/ada/Downloads");
        let taken: HashSet<PathBuf> = std::iter::once(dir.join("proj")).collect();
        assert_eq!(
            reserve_download_directory(dir, "proj", |path| !taken.contains(path)),
            Some(dir.join("proj (1)"))
        );
        assert_eq!(
            reserve_download_directory(dir, "notes", |path| !taken.contains(path)),
            Some(dir.join("notes"))
        );
        assert_eq!(
            reserve_download_directory(dir, "x", |_| false),
            None,
            "when nothing can be created, refuse rather than reuse"
        );
    }

    /// Pins the bug this replaced: a download that failed before moving any
    /// bytes used to return past the cleanup, leaving the reserved staging
    /// file behind forever. That orphan is not just litter — reservation skips
    /// names whose staging file is taken, so it permanently bumps every later
    /// download of that name to ` (1)`, ` (2)`, …
    #[test]
    fn a_failed_download_never_leaves_its_staging_file_behind() {
        let dir = tempfile::tempdir().unwrap();
        let local = dir.path().join("report.pdf");
        let partial = partial_download_path(&local);
        std::fs::write(&partial, b"partial").unwrap();

        let err = smol::block_on(finish_download(
            Err("Unable to open /srv/report.pdf: no such file".to_string()),
            &partial,
            &local,
        ))
        .expect_err("a failed download must stay failed");

        assert!(err.message.contains("no such file"), "{}", err.message);
        assert!(
            !partial.exists(),
            "the staging file must not survive a failure"
        );
        assert!(!local.exists(), "and nothing may appear at the destination");
    }

    /// The reservation checked the destination was free, but a download takes
    /// time; `rename` would replace silently, so a file that appears in the
    /// meantime must be left alone — matching the upload side's refusal to
    /// overwrite.
    #[test]
    fn a_download_does_not_clobber_a_file_that_appeared_while_it_ran() {
        let dir = tempfile::tempdir().unwrap();
        let local = dir.path().join("notes.txt");
        let partial = partial_download_path(&local);
        std::fs::write(&partial, b"downloaded").unwrap();
        // Something else got there first.
        std::fs::write(&local, b"do not lose me").unwrap();

        let err = smol::block_on(finish_download(Ok(10), &partial, &local))
            .expect_err("landing on an occupied name must fail");

        assert!(
            err.message.contains("appeared while downloading"),
            "{}",
            err.message
        );
        assert_eq!(
            std::fs::read(&local).unwrap(),
            b"do not lose me",
            "the pre-existing file must be untouched"
        );
        assert!(!partial.exists(), "and the staging file is cleaned up");
    }

    #[cfg(unix)]
    #[test]
    fn a_download_does_not_clobber_a_dangling_symlink() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let local = dir.path().join("notes.txt");
        let partial = partial_download_path(&local);
        std::fs::write(&partial, b"downloaded").unwrap();
        symlink(dir.path().join("missing-target"), &local).unwrap();

        assert!(!local.exists(), "stat follows the link and sees no target");
        assert!(
            local_path_is_occupied(&local),
            "lstat must still reserve the directory entry"
        );
        let err = smol::block_on(finish_download(Ok(10), &partial, &local))
            .expect_err("a dangling link is an occupied destination");

        assert!(
            err.message.contains("appeared while downloading"),
            "{}",
            err.message
        );
        assert!(
            std::fs::symlink_metadata(&local)
                .unwrap()
                .file_type()
                .is_symlink(),
            "the link itself must remain untouched"
        );
        assert!(!partial.exists(), "and the staging file is cleaned up");
    }

    /// When cleanup itself fails — which is the normal case once the
    /// connection is what died — the failure has to carry where the debris is.
    /// A log line is not good enough: the user needs to know a partial file is
    /// sitting there, or they will hit "already exists" on retry with nothing
    /// on screen explaining why.
    #[test]
    fn a_failure_that_could_not_clean_up_says_what_it_left_behind() {
        let dir = tempfile::tempdir().unwrap();
        let local = dir.path().join("big.iso");
        let partial = partial_download_path(&local);
        // No staging file on disk, so the cleanup below cannot succeed —
        // standing in for a removal that fails for any reason.
        let err = smol::block_on(finish_download(
            Err("connection lost".to_string()),
            &partial,
            &local,
        ))
        .expect_err("the transfer failed");

        assert_eq!(err.message, "connection lost");
        assert_eq!(
            err.leftover.as_deref(),
            Some(partial.display().to_string().as_str()),
            "the caller must be able to tell the user where the debris is"
        );

        // A cleanup that works reports no leftover.
        std::fs::write(&partial, b"half").unwrap();
        let err = smol::block_on(finish_download(
            Err("connection lost".to_string()),
            &partial,
            &local,
        ))
        .expect_err("the transfer failed");
        assert_eq!(err.leftover, None);
        assert!(!partial.exists());
    }

    #[test]
    fn a_successful_download_moves_into_place() {
        let dir = tempfile::tempdir().unwrap();
        let local = dir.path().join("archive.tar.gz");
        let partial = partial_download_path(&local);
        std::fs::write(&partial, b"payload").unwrap();

        let written = smol::block_on(finish_download(Ok(7), &partial, &local)).expect("download");
        assert_eq!(written, 7);
        assert_eq!(std::fs::read(&local).unwrap(), b"payload");
        assert!(
            !partial.exists(),
            "the staging file is consumed by the move"
        );
    }

    #[test]
    fn only_explicit_sftp_missing_errors_mean_a_name_is_free() {
        assert!(sftp_error_is_missing(&SftpChannelError::Sftp(
            SftpError::NoSuchFile
        )));
        assert!(sftp_error_is_missing(&SftpChannelError::Sftp(
            SftpError::NoSuchPath
        )));
        assert!(!sftp_error_is_missing(&SftpChannelError::Sftp(
            SftpError::PermissionDenied
        )));
        assert!(!sftp_error_is_missing(&SftpChannelError::Sftp(
            SftpError::Failure
        )));
    }

    #[test]
    fn a_remote_operation_origin_matches_only_its_host_configuration() {
        let origin =
            RemoteOperationOrigin::new("ssh-host:a".to_string(), "ssh-host:a:key-1".to_string());
        assert!(origin.matches(Some("ssh-host:a"), Some("ssh-host:a:key-1")));
        assert!(!origin.matches(Some("ssh-host:b"), Some("ssh-host:a:key-1")));
        assert!(!origin.matches(Some("ssh-host:a"), Some("ssh-host:a:key-2")));
        assert!(!origin.matches(None, Some("ssh-host:a:key-1")));
        assert!(!origin.matches(Some("ssh-host:a"), None));
    }

    /// A folder reports items, not bytes. The trap this pins: a walker that
    /// reuses the single-file upload path lets every file call `set_total`
    /// with its own size, so the bar reads "100%" on each small file and jumps
    /// backwards on the next one. Item scale must win once it is established.
    /// A folder upload rebuilds each destination from the plan's *relative*
    /// path, one component at a time. Joining the whole thing at once would
    /// splice a local separator into a remote path — and on Windows a
    /// backslash is a legal remote filename character, so the mistake would
    /// silently create a file literally called `a\b\c`.
    #[test]
    fn a_relative_path_joins_one_component_at_a_time() {
        let root = RemotePath::from_server_absolute("/srv/app").unwrap();
        let mut built = root.clone();
        for name in ["proj", "src", "main.rs"] {
            built = built.join_name(name).unwrap();
        }
        assert_eq!(built.as_str(), "/srv/app/proj/src/main.rs");

        // The whole-string shortcut is exactly what join_name refuses.
        assert!(root.join_name("proj/src/main.rs").is_err());
        assert!(root.join_name("..").is_err());
        assert!(root.join_name("").is_err());
    }

    #[test]
    fn item_progress_is_not_disturbed_by_per_file_byte_totals() {
        let progress = RemoteTransferProgress::default();
        progress.set_item_total(4);
        assert_eq!(progress.items(), Some((0, 4)));
        assert_eq!(progress.fraction(), Some(0.0));

        // One file of the tree goes by, reporting its own byte size and
        // finishing it. The fraction must follow items, not those bytes.
        progress.set_total(1_000);
        progress.advance(1_000);
        progress.finish_item();
        assert_eq!(progress.items(), Some((1, 4)));
        assert_eq!(
            progress.fraction(),
            Some(0.25),
            "one of four files done, regardless of that file's byte count"
        );

        for _ in 0..3 {
            progress.finish_item();
        }
        assert_eq!(progress.fraction(), Some(1.0));
    }

    #[test]
    fn a_single_file_transfer_still_reports_bytes() {
        let progress = RemoteTransferProgress::default();
        assert_eq!(progress.items(), None, "no item scale unless asked for");
        progress.set_total(200);
        progress.advance(50);
        assert_eq!(progress.fraction(), Some(0.25));
    }

    /// An empty folder is finished the moment it exists, the same way an empty
    /// file is — otherwise the row would sit at 0% forever.
    #[test]
    fn an_empty_folder_reads_as_complete() {
        let progress = RemoteTransferProgress::default();
        progress.set_item_total(0);
        assert_eq!(progress.fraction(), Some(1.0));
    }

    #[test]
    fn a_partial_download_lands_beside_its_destination() {
        // Same directory, so completing the download is a rename rather than a
        // cross-filesystem copy.
        let destination = Path::new("/home/ada/Downloads/notes.tar.gz");
        let path = partial_download_path(destination);
        assert_eq!(path, PathBuf::from("/home/ada/Downloads/notes.tar.gz.part"));
        assert_eq!(path.parent(), destination.parent());
    }

    /// Authorization is a process-wide registry, so tests sharing a source name
    /// would inherit each other's authorization (and race, since they run in
    /// parallel). Every test that cares about the authorized/unauthorized
    /// distinction must mint its own name through this.
    fn unique_source(prefix: &str) -> String {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        format!("{prefix}-{}", NEXT.fetch_add(1, Ordering::SeqCst))
    }

    fn target(source: &str) -> RemoteFilesTarget {
        target_with_root(source, "~")
    }

    fn target_with_root(source: &str, requested_root: &str) -> RemoteFilesTarget {
        RemoteFilesTarget {
            project_id: "project".to_string(),
            project_name: "Project".to_string(),
            source: RemoteFilesSource::ClientDomain(source.to_string()),
            requested_root: requested_root.to_string(),
        }
    }

    fn listing(parent: &RemotePath, names: &[(&str, RemoteFileKind)]) -> RemoteDirectoryListing {
        RemoteDirectoryListing {
            entries: names
                .iter()
                .map(|(name, kind)| RemoteFileEntry {
                    path: parent.join_name(name).unwrap(),
                    name: (*name).to_string(),
                    kind: *kind,
                    size: None,
                })
                .collect(),
            truncated: false,
        }
    }

    struct FakeBackend;

    impl RemoteFileBackend for FakeBackend {
        fn resolve_root(&self, _requested: String) -> RemoteFuture<RemotePath> {
            Box::pin(async { RemotePath::from_server_absolute("/home/test") })
        }

        fn list_directory(
            &self,
            _path: RemotePath,
            _limit: usize,
        ) -> RemoteFuture<RemoteDirectoryListing> {
            Box::pin(async {
                Ok(RemoteDirectoryListing {
                    entries: Vec::new(),
                    truncated: false,
                })
            })
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

    struct FakeConnector {
        calls: Arc<AtomicUsize>,
    }

    /// Backend that answers once and then behaves like a session the server
    /// reaped while it sat idle: every later probe fails.
    struct DiesWhenIdleBackend {
        probes: Arc<AtomicUsize>,
        alive: Arc<AtomicBool>,
    }

    impl RemoteFileBackend for DiesWhenIdleBackend {
        fn resolve_root(&self, _requested: String) -> RemoteFuture<RemotePath> {
            Box::pin(async { RemotePath::from_server_absolute("/home/test") })
        }

        fn list_directory(
            &self,
            _path: RemotePath,
            _limit: usize,
        ) -> RemoteFuture<RemoteDirectoryListing> {
            Box::pin(async {
                Ok(RemoteDirectoryListing {
                    entries: Vec::new(),
                    truncated: false,
                })
            })
        }

        fn read_file(&self, _path: RemotePath, _limit: usize) -> RemoteFuture<RemoteFileBytes> {
            Box::pin(async {
                Ok(RemoteFileBytes {
                    bytes: Vec::new(),
                    truncated: false,
                })
            })
        }

        fn probe(&self) -> RemoteFuture<()> {
            self.probes.fetch_add(1, Ordering::SeqCst);
            let alive = self.alive.load(Ordering::SeqCst);
            Box::pin(async move {
                if alive {
                    Ok(())
                } else {
                    Err("Library-specific error: session is gone".to_string())
                }
            })
        }
    }

    struct DyingConnector {
        calls: Arc<AtomicUsize>,
        probes: Arc<AtomicUsize>,
        /// Flipped false after the first connection is created, so the *next*
        /// dial produces a healthy backend while the cached one is dead.
        first_is_alive: Arc<AtomicBool>,
    }

    impl RemoteFileConnector for DyingConnector {
        fn connect(&self, _config: SshDomain) -> RemoteFuture<Arc<dyn RemoteFileBackend>> {
            let nth = self.calls.fetch_add(1, Ordering::SeqCst);
            let alive = Arc::new(AtomicBool::new(nth > 0));
            if nth == 0 {
                self.first_is_alive.store(false, Ordering::SeqCst);
            }
            let backend = DiesWhenIdleBackend {
                probes: Arc::clone(&self.probes),
                alive,
            };
            Box::pin(async move { Ok(Arc::new(backend) as Arc<dyn RemoteFileBackend>) })
        }
    }

    impl RemoteFileConnector for FakeConnector {
        fn connect(&self, _config: SshDomain) -> RemoteFuture<Arc<dyn RemoteFileBackend>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(Arc::new(FakeBackend) as Arc<dyn RemoteFileBackend>) })
        }
    }

    #[test]
    fn remote_path_is_slash_only_and_treats_backslash_as_a_filename_character() {
        let root = RemotePath::from_server_absolute("/root/app/").unwrap();
        assert_eq!(root.as_str(), "/root/app");
        assert_eq!(
            RemotePath::from_server_absolute("/root/app ")
                .unwrap()
                .as_str(),
            "/root/app "
        );
        assert_eq!(root.join_name("src").unwrap().as_str(), "/root/app/src");
        assert_eq!(
            root.join_name(r"odd\\name").unwrap().as_str(),
            r"/root/app/odd\\name"
        );
        assert!(root.join_name("../escape").is_err());
        assert!(root.join_name("a/b").is_err());
    }

    #[test]
    fn tilde_is_resolved_from_canonical_dot_home() {
        let home = RemotePath::from_server_absolute("/home/alice").unwrap();
        assert_eq!(resolve_requested_root(home.clone(), "~").unwrap(), home);
        assert_eq!(
            resolve_requested_root(home, "~/src/app").unwrap().as_str(),
            "/home/alice/src/app"
        );
        assert!(resolve_requested_root(
            RemotePath::from_server_absolute("/home/alice").unwrap(),
            "~/../etc"
        )
        .is_err());
    }

    #[test]
    fn raw_directory_reader_only_treats_real_eof_as_completion() {
        let protocol_eof = anyhow::Error::new(SftpChannelError::Sftp(SftpError::Eof));
        assert!(is_sftp_directory_eof(&protocol_eof));
        let io_eof = anyhow::Error::new(SftpChannelError::FileIo(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "no more files",
        )));
        assert!(is_sftp_directory_eof(&io_eof));
        let lost = anyhow::Error::new(SftpChannelError::Sftp(SftpError::ConnectionLost));
        assert!(!is_sftp_directory_eof(&lost));
    }

    #[test]
    fn manager_single_flights_and_expires_only_after_last_lease() {
        let calls = Arc::new(AtomicUsize::new(0));
        let manager = RemoteConnectionManager::new(
            Arc::new(FakeConnector {
                calls: Arc::clone(&calls),
            }),
            Duration::from_secs(30),
        );
        let (first, second) = smol::block_on(async {
            futures::future::join(
                manager.acquire("source".to_string(), SshDomain::default(), true),
                manager.acquire("source".to_string(), SshDomain::default(), true),
            )
            .await
        });
        let mut first = first.unwrap();
        let mut second = second.unwrap();
        let connection_id = first.connection_id;
        assert_eq!(second.connection_id, connection_id);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        first.manager = Weak::new();
        second.manager = Weak::new();
        let idle_since = Instant::now();
        assert!(manager
            .release_state("source", connection_id, idle_since)
            .is_none());
        drop(first);
        {
            let inner = manager.inner.lock().unwrap();
            assert!(matches!(
                inner.entries.get("source"),
                Some(ManagedConnection::Ready { leases: 1, .. })
            ));
        }
        let (generation, _) = manager
            .release_state("source", connection_id, idle_since)
            .expect("last lease starts idle timer");
        drop(second);
        let stored_idle_since = {
            let inner = manager.inner.lock().unwrap();
            match inner.entries.get("source") {
                Some(ManagedConnection::Ready {
                    leases: 0,
                    idle_generation,
                    idle_since: Some(idle_since),
                    ..
                }) => {
                    assert_eq!(*idle_generation, generation);
                    *idle_since
                }
                _ => panic!("expected idle connection"),
            }
        };
        assert!(!manager.expire_if_idle(
            "source",
            connection_id,
            generation,
            stored_idle_since + Duration::from_secs(29)
        ));
        assert!(manager.expire_if_idle(
            "source",
            connection_id,
            generation,
            stored_idle_since + Duration::from_secs(30)
        ));
    }

    #[test]
    fn stale_lease_cannot_release_a_replacement_connection() {
        let calls = Arc::new(AtomicUsize::new(0));
        let manager = RemoteConnectionManager::new(
            Arc::new(FakeConnector {
                calls: Arc::clone(&calls),
            }),
            Duration::from_secs(30),
        );
        let old = smol::block_on(manager.acquire("source".to_string(), SshDomain::default(), true))
            .unwrap();
        let old_id = old.connection_id;
        manager.invalidate("source");
        let replacement =
            smol::block_on(manager.acquire("source".to_string(), SshDomain::default(), true))
                .unwrap();
        assert_ne!(old_id, replacement.connection_id);
        drop(old);
        let inner = manager.inner.lock().unwrap();
        assert!(matches!(
            inner.entries.get("source"),
            Some(ManagedConnection::Ready { leases: 1, .. })
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn lease_identity_is_the_key_and_id_pair() {
        let calls = Arc::new(AtomicUsize::new(0));
        let manager = RemoteConnectionManager::new(
            Arc::new(FakeConnector {
                calls: Arc::clone(&calls),
            }),
            Duration::from_secs(30),
        );
        let first =
            smol::block_on(manager.acquire("source".to_string(), SshDomain::default(), true))
                .unwrap();
        let shared =
            smol::block_on(manager.acquire("source".to_string(), SshDomain::default(), true))
                .unwrap();
        assert!(first.is_same_connection(&shared), "one pooled session");

        // What a caller that dialled for itself sees after the pooled session
        // was replaced: the lease it is holding is NOT the live one.
        manager.invalidate("source");
        let redialled =
            smol::block_on(manager.acquire("source".to_string(), SshDomain::default(), true))
                .unwrap();
        assert!(!first.is_same_connection(&redialled));

        // The key half, which the pool cannot produce on its own: changed host
        // settings hash to a different key while the ids stay whatever they
        // were.
        let lease = |key: &str, connection_id| RemoteConnectionLease {
            key: key.to_string(),
            backend: Arc::new(FakeBackend) as Arc<dyn RemoteFileBackend>,
            manager: Weak::new(),
            connection_id,
        };
        assert!(lease("source:aaa", 1).is_same_connection(&lease("source:aaa", 1)));
        assert!(!lease("source:aaa", 1).is_same_connection(&lease("source:bbb", 1)));
    }

    #[test]
    fn connection_key_changes_with_endpoint_options_and_credentials() {
        let mut first = SshDomain::default();
        first.remote_address = "host:22".to_string();
        first.username = Some("alice".to_string());
        let base = remote_connection_key("source", &first);

        let mut changed_host = first.clone();
        changed_host.remote_address = "other:22".to_string();
        assert_ne!(base, remote_connection_key("source", &changed_host));

        let mut changed_option = first.clone();
        changed_option
            .ssh_option
            .insert("proxycommand".to_string(), "ssh jump".to_string());
        assert_ne!(base, remote_connection_key("source", &changed_option));

        let mut changed_password = first;
        changed_password.stored_password = Some("secret".to_string());
        assert_ne!(base, remote_connection_key("source", &changed_password));
    }

    /// Exactly the reported repro: connect, close the sidebar, reopen, connect
    /// again. Every step is driven the way `paint_files_sidebar` drives it,
    /// including the idempotent `TargetChanged` that fires on every frame.
    #[test]
    fn reconnecting_after_hiding_the_panel_reaches_connected_again() {
        let source = unique_source("reconnecting_after_hiding_the_panel_reaches_connected_again");
        let root = RemotePath::from_server_absolute("/home/me").unwrap();
        let mut state = RemoteFilesState::default();

        let connect_once = |state: &mut RemoteFilesState| {
            let effects = state.transition(RemoteFilesEvent::ConnectRequested);
            let generation = match effects.as_slice() {
                [RemoteFilesEffect::Connect { generation, .. }] => *generation,
                other => panic!("expected a Connect effect, got {:?}", other),
            };
            state.transition(RemoteFilesEvent::Connected {
                generation,
                root: root.clone(),
                listing: listing(&root, &[]),
            });
            generation
        };

        state.transition(RemoteFilesEvent::TargetChanged(Some(target(&source))));
        connect_once(&mut state);
        assert_eq!(state.phase, RemoteFilesPhase::Connected);

        // Sidebar closed.
        assert_eq!(
            state.transition(RemoteFilesEvent::PanelHidden),
            vec![RemoteFilesEffect::ReleaseLease]
        );
        assert_eq!(state.phase, RemoteFilesPhase::Disconnected);

        // Reopened: the paint path re-announces the same target every frame and
        // must not disturb anything.
        for _ in 0..3 {
            assert!(state
                .transition(RemoteFilesEvent::TargetChanged(Some(target(&source))))
                .is_empty());
        }
        assert_eq!(state.phase, RemoteFilesPhase::Disconnected);

        // Second connect must behave exactly like the first.
        connect_once(&mut state);
        assert_eq!(state.phase, RemoteFilesPhase::Connected);
        assert_eq!(state.root.as_ref(), Some(&root));
    }

    /// A stale reply from the *first* connection must not be able to knock the
    /// second one back into Failed — the generation guard is what prevents the
    /// "reconnect immediately errors" symptom.
    #[test]
    fn a_late_failure_from_the_previous_attempt_is_ignored() {
        let source = unique_source("a_late_failure_from_the_previous_attempt_is_ignored");
        let root = RemotePath::from_server_absolute("/home/me").unwrap();
        let mut state = RemoteFilesState::default();
        state.transition(RemoteFilesEvent::TargetChanged(Some(target(&source))));

        let first_generation = match state
            .transition(RemoteFilesEvent::ConnectRequested)
            .as_slice()
        {
            [RemoteFilesEffect::Connect { generation, .. }] => *generation,
            other => panic!("expected Connect, got {:?}", other),
        };
        state.transition(RemoteFilesEvent::PanelHidden);

        let second_generation = match state
            .transition(RemoteFilesEvent::ConnectRequested)
            .as_slice()
        {
            [RemoteFilesEffect::Connect { generation, .. }] => *generation,
            other => panic!("expected Connect, got {:?}", other),
        };
        assert_ne!(first_generation, second_generation);

        // The abandoned attempt finally fails; it must not touch the live one.
        state.transition(RemoteFilesEvent::ConnectionFailed {
            generation: first_generation,
            message: "stale".to_string(),
        });
        assert_eq!(state.phase, RemoteFilesPhase::Connecting);

        state.transition(RemoteFilesEvent::Connected {
            generation: second_generation,
            root: root.clone(),
            listing: listing(&root, &[]),
        });
        assert_eq!(state.phase, RemoteFilesPhase::Connected);
    }

    /// Connect one source, switch to another, switch back: the first tree must
    /// come straight back from cache instead of the panel dropping to "Not
    /// connected" and asking for a click while the session is still open.
    /// The preview pane widens the sidebar; leaving `selected` set after the
    /// preview closes keeps `right_sidebar_file_preview_active` true, so the
    /// pane's width is never handed back to the terminal.
    #[test]
    fn clearing_the_selection_also_drops_the_preview() {
        let source = unique_source("clear-selection");
        let root = RemotePath::from_server_absolute("/home/me").unwrap();
        let file = root.join_name("a.txt").unwrap();
        let mut state = RemoteFilesState::default();
        state.transition(RemoteFilesEvent::TargetChanged(Some(target(&source))));
        let generation = match state
            .transition(RemoteFilesEvent::ConnectRequested)
            .as_slice()
        {
            [RemoteFilesEffect::Connect { generation, .. }] => *generation,
            other => panic!("expected Connect, got {:?}", other),
        };
        state.transition(RemoteFilesEvent::Connected {
            generation,
            root: root.clone(),
            listing: listing(&root, &[("a.txt", RemoteFileKind::File)]),
        });
        state.transition(RemoteFilesEvent::SelectFile(file.clone()));
        assert_eq!(state.selected.as_ref(), Some(&file));
        assert!(!matches!(state.preview, RemotePreviewStatus::None));

        state.clear_selection();
        assert!(state.selected.is_none());
        assert!(matches!(state.preview, RemotePreviewStatus::None));
    }

    #[test]
    fn switching_between_sources_restores_each_tree_from_cache() {
        let first_source = unique_source("switch-a");
        let second_source = unique_source("switch-b");
        let root_a = RemotePath::from_server_absolute("/home/a").unwrap();
        let root_b = RemotePath::from_server_absolute("/home/b").unwrap();
        let mut state = RemoteFilesState::default();

        let connect =
            |state: &mut RemoteFilesState, root: &RemotePath, names: &[(&str, RemoteFileKind)]| {
                let generation = match state
                    .transition(RemoteFilesEvent::ConnectRequested)
                    .as_slice()
                {
                    [RemoteFilesEffect::Connect { generation, .. }] => *generation,
                    other => panic!("expected Connect, got {:?}", other),
                };
                state.transition(RemoteFilesEvent::Connected {
                    generation,
                    root: root.clone(),
                    listing: listing(root, names),
                });
            };

        state.transition(RemoteFilesEvent::TargetChanged(Some(target(&first_source))));
        connect(&mut state, &root_a, &[("only-in-a", RemoteFileKind::File)]);
        assert_eq!(state.rows().len(), 2, "root plus its one entry");

        // Switch away: the second source is unauthorized, so it must ask.
        let effects = state.transition(RemoteFilesEvent::TargetChanged(Some(target(
            &second_source,
        ))));
        assert_eq!(effects, vec![RemoteFilesEffect::ReleaseLease]);
        assert_eq!(state.phase, RemoteFilesPhase::Disconnected);
        connect(&mut state, &root_b, &[]);

        // Switch back: cached, authorized, and already connected — no spinner,
        // no button, and a refresh is kicked off underneath.
        let effects =
            state.transition(RemoteFilesEvent::TargetChanged(Some(target(&first_source))));
        assert_eq!(state.phase, RemoteFilesPhase::Connected);
        assert_eq!(state.root.as_ref(), Some(&root_a));
        assert_eq!(state.rows().len(), 2, "the cached tree is back on screen");
        assert!(
            matches!(
                effects.as_slice(),
                [
                    RemoteFilesEffect::ReleaseLease,
                    RemoteFilesEffect::Connect { .. }
                ]
            ),
            "expected release + background refresh, got {:?}",
            effects
        );
    }

    /// A source whose connection failed must not keep a cached tree: coming
    /// back to it should show the failure, not rows from the dead session.
    #[test]
    fn a_failed_connection_forgets_that_sources_cached_tree() {
        let source = unique_source("failed-forgets");
        let root = RemotePath::from_server_absolute("/home/me").unwrap();
        let mut state = RemoteFilesState::default();
        state.transition(RemoteFilesEvent::TargetChanged(Some(target(&source))));
        let generation = match state
            .transition(RemoteFilesEvent::ConnectRequested)
            .as_slice()
        {
            [RemoteFilesEffect::Connect { generation, .. }] => *generation,
            other => panic!("expected Connect, got {:?}", other),
        };
        state.transition(RemoteFilesEvent::Connected {
            generation,
            root: root.clone(),
            listing: listing(&root, &[("f", RemoteFileKind::File)]),
        });

        // Hide (tree goes to cache), come back, then have the refresh fail.
        state.transition(RemoteFilesEvent::PanelHidden);
        state.transition(RemoteFilesEvent::TargetChanged(None));
        let effects = state.transition(RemoteFilesEvent::TargetChanged(Some(target(&source))));
        let refresh_generation = match effects.as_slice() {
            [_, RemoteFilesEffect::Connect { generation, .. }] => *generation,
            other => panic!("expected a background refresh, got {:?}", other),
        };
        state.transition(RemoteFilesEvent::ConnectionFailed {
            generation: refresh_generation,
            message: "connection lost".to_string(),
        });
        assert!(matches!(state.phase, RemoteFilesPhase::Failed(_)));
        assert!(
            state.cached_trees.is_empty(),
            "a dead source must not keep a tree that could be restored later"
        );

        // Switching away and back shows the disconnected state, not old rows.
        state.transition(RemoteFilesEvent::TargetChanged(None));
        state.transition(RemoteFilesEvent::TargetChanged(Some(target(&source))));
        assert_eq!(state.phase, RemoteFilesPhase::Disconnected);
        assert!(state.rows().is_empty());
    }

    #[test]
    fn the_tree_cache_is_bounded_and_evicts_the_oldest_source() {
        let root = RemotePath::from_server_absolute("/home/me").unwrap();
        let mut state = RemoteFilesState::default();
        let sources: Vec<String> = (0..REMOTE_TREE_CACHE_CAP + 1)
            .map(|_| unique_source("lru"))
            .collect();

        for source in &sources {
            state.transition(RemoteFilesEvent::TargetChanged(Some(target(source))));
            let generation = match state
                .transition(RemoteFilesEvent::ConnectRequested)
                .as_slice()
            {
                [RemoteFilesEffect::Connect { generation, .. }] => *generation,
                other => panic!("expected Connect, got {:?}", other),
            };
            state.transition(RemoteFilesEvent::Connected {
                generation,
                root: root.clone(),
                listing: listing(&root, &[("f", RemoteFileKind::File)]),
            });
        }
        // The last one is still on screen; the rest are cached up to the cap.
        state.transition(RemoteFilesEvent::TargetChanged(None));
        assert_eq!(state.cached_trees.len(), REMOTE_TREE_CACHE_CAP);
        assert!(
            !state
                .cached_trees
                .contains_key(&(format!("client-domain:{}", sources[0]), "~".to_string())),
            "the oldest source must have been evicted"
        );
    }

    /// Hiding the panel keeps the structure (it is cheap) — the expensive part
    /// is the preview, which the sidebar releases separately. Only the idle
    /// release gives the trees back.
    #[test]
    fn hiding_keeps_the_tree_but_releasing_drops_it() {
        let source = unique_source("hide-keeps");
        let root = RemotePath::from_server_absolute("/home/me").unwrap();
        let mut state = RemoteFilesState::default();
        state.transition(RemoteFilesEvent::TargetChanged(Some(target(&source))));
        let generation = match state
            .transition(RemoteFilesEvent::ConnectRequested)
            .as_slice()
        {
            [RemoteFilesEffect::Connect { generation, .. }] => *generation,
            other => panic!("expected Connect, got {:?}", other),
        };
        state.transition(RemoteFilesEvent::Connected {
            generation,
            root: root.clone(),
            listing: listing(&root, &[("f", RemoteFileKind::File)]),
        });

        assert_eq!(
            state.transition(RemoteFilesEvent::PanelHidden),
            vec![RemoteFilesEffect::ReleaseLease]
        );
        assert_eq!(state.cached_trees.len(), 1, "hiding must not drop the tree");

        // Reopening the same target restores it without a reconnect prompt.
        state.transition(RemoteFilesEvent::TargetChanged(None));
        state.transition(RemoteFilesEvent::TargetChanged(Some(target(&source))));
        assert_eq!(state.phase, RemoteFilesPhase::Connected);
        assert_eq!(state.rows().len(), 2);

        // The idle release only ever runs while the panel is hidden, i.e. after
        // `PanelHidden` has already moved the live tree into the cache. Model
        // that order: releasing must not yank a tree the user is looking at.
        state.transition(RemoteFilesEvent::PanelHidden);
        assert_eq!(state.cached_trees.len(), 1);
        state.release_cached_trees();
        assert!(state.cached_trees.is_empty());

        // Reopening now has nothing to restore, so it goes back to asking the
        // connection layer instead of showing stale rows.
        state.transition(RemoteFilesEvent::TargetChanged(None));
        state.transition(RemoteFilesEvent::TargetChanged(Some(target(&source))));
        assert_eq!(state.phase, RemoteFilesPhase::Disconnected);
        assert!(state.rows().is_empty());
        // Still authorized, so the paint path's resume will reconnect without
        // making the user press anything.
        assert!(state.can_resume_current());
        assert!(matches!(
            state
                .transition(RemoteFilesEvent::ResumeRequested)
                .as_slice(),
            [RemoteFilesEffect::Connect {
                allow_connect: true,
                ..
            }]
        ));
    }

    /// Two projects can share one host while browsing different roots; the
    /// cache is keyed by (source, requested root) so project B must never be
    /// shown project A's rows just because the connection is the same.
    /// An upload lands in one directory, so only that directory is re-read.
    /// A full `Refresh` would collapse the whole tree the user had opened.
    #[test]
    fn invalidating_a_directory_relists_only_that_directory() {
        let source = unique_source("invalidate");
        let root = RemotePath::from_server_absolute("/home/me").unwrap();
        let dir = root.join_name("src").unwrap();
        let other = root.join_name("docs").unwrap();
        let mut state = RemoteFilesState::default();

        state.transition(RemoteFilesEvent::TargetChanged(Some(target(&source))));
        let generation = match state
            .transition(RemoteFilesEvent::ConnectRequested)
            .as_slice()
        {
            [RemoteFilesEffect::Connect { generation, .. }] => *generation,
            other => panic!("expected Connect, got {:?}", other),
        };
        state.transition(RemoteFilesEvent::Connected {
            generation,
            root: root.clone(),
            listing: listing(
                &root,
                &[
                    ("src", RemoteFileKind::Directory),
                    ("docs", RemoteFileKind::Directory),
                ],
            ),
        });
        for path in [dir.clone(), other.clone()] {
            state.transition(RemoteFilesEvent::ToggleDirectory(path.clone()));
            state.transition(RemoteFilesEvent::DirectoryLoaded {
                generation,
                path: path.clone(),
                listing: listing(&path, &[("old", RemoteFileKind::File)]),
            });
        }
        assert_eq!(state.rows().len(), 5, "root + 2 dirs + 2 files");

        let effects = state.transition(RemoteFilesEvent::DirectoryInvalidated(dir.clone()));
        assert!(
            matches!(
                effects.as_slice(),
                [RemoteFilesEffect::ListDirectory { path, .. }] if *path == dir
            ),
            "expected a re-list of just that directory, got {:?}",
            effects
        );
        // The sibling keeps both its expansion and its rows.
        assert!(state.expanded.contains(&other));
        assert!(state.directories.contains_key(&other));

        state.transition(RemoteFilesEvent::DirectoryLoaded {
            generation,
            path: dir.clone(),
            listing: listing(
                &dir,
                &[("old", RemoteFileKind::File), ("new", RemoteFileKind::File)],
            ),
        });
        assert_eq!(state.rows().len(), 6, "the uploaded file shows up");

        // A directory that is not open has nothing to re-read.
        state.transition(RemoteFilesEvent::ToggleDirectory(other.clone()));
        assert!(state
            .transition(RemoteFilesEvent::DirectoryInvalidated(other))
            .is_empty());
    }

    #[test]
    fn trees_are_cached_per_root_not_per_source() {
        let source = unique_source("per-root");
        let root_a = RemotePath::from_server_absolute("/srv/a").unwrap();
        let mut state = RemoteFilesState::default();

        state.transition(RemoteFilesEvent::TargetChanged(Some(target_with_root(
            &source, "/srv/a",
        ))));
        let generation = match state
            .transition(RemoteFilesEvent::ConnectRequested)
            .as_slice()
        {
            [RemoteFilesEffect::Connect { generation, .. }] => *generation,
            other => panic!("expected Connect, got {:?}", other),
        };
        state.transition(RemoteFilesEvent::Connected {
            generation,
            root: root_a.clone(),
            listing: listing(&root_a, &[("only-in-a", RemoteFileKind::File)]),
        });

        // Same source, different root: authorized, but there is nothing cached
        // for this root, so nothing may be restored.
        let effects = state.transition(RemoteFilesEvent::TargetChanged(Some(target_with_root(
            &source, "/srv/b",
        ))));
        assert_eq!(effects, vec![RemoteFilesEffect::ReleaseLease]);
        assert_eq!(state.phase, RemoteFilesPhase::Disconnected);
        assert!(
            state.rows().is_empty(),
            "must not show the other root's rows"
        );

        // Back on the original root the stash is legitimate again.
        state.transition(RemoteFilesEvent::TargetChanged(Some(target_with_root(
            &source, "/srv/a",
        ))));
        assert_eq!(state.phase, RemoteFilesPhase::Connected);
        assert_eq!(state.rows().len(), 2);
    }

    /// While a restored tree waits for its lease, clicks are recorded, not
    /// executed: an effect reaching the handler before the lease exists used
    /// to be reported as a spurious connection failure that wiped the tree.
    #[test]
    fn clicks_on_a_restored_tree_are_deferred_until_the_lease_arrives() {
        let source = unique_source("deferred");
        let root = RemotePath::from_server_absolute("/home/me").unwrap();
        let dir = root.join_name("src").unwrap();
        let file = root.join_name("readme.md").unwrap();
        let mut state = RemoteFilesState::default();

        state.transition(RemoteFilesEvent::TargetChanged(Some(target(&source))));
        let generation = match state
            .transition(RemoteFilesEvent::ConnectRequested)
            .as_slice()
        {
            [RemoteFilesEffect::Connect { generation, .. }] => *generation,
            other => panic!("expected Connect, got {:?}", other),
        };
        state.transition(RemoteFilesEvent::Connected {
            generation,
            root: root.clone(),
            listing: listing(
                &root,
                &[
                    ("src", RemoteFileKind::Directory),
                    ("readme.md", RemoteFileKind::File),
                ],
            ),
        });

        // Stash and restore: the refresh Connect is now in flight.
        state.transition(RemoteFilesEvent::TargetChanged(None));
        let effects = state.transition(RemoteFilesEvent::TargetChanged(Some(target(&source))));
        let refresh_generation = match effects.as_slice() {
            [_, RemoteFilesEffect::Connect { generation, .. }] => *generation,
            other => panic!("expected a background refresh, got {:?}", other),
        };
        assert_eq!(state.phase, RemoteFilesPhase::Connected);

        // Interactions during the round trip must emit nothing...
        assert!(
            state
                .transition(RemoteFilesEvent::ToggleDirectory(dir.clone()))
                .is_empty(),
            "expanding an uncached directory must wait for the lease"
        );
        assert!(
            state
                .transition(RemoteFilesEvent::SelectFile(file.clone()))
                .is_empty(),
            "opening a preview must wait for the lease"
        );
        assert!(
            state.transition(RemoteFilesEvent::Refresh).is_empty(),
            "a refresh is already what the restored tree is waiting on"
        );

        // ...and be flushed, with the current generation, once it lands.
        let effects = state.transition(RemoteFilesEvent::Connected {
            generation: refresh_generation,
            root: root.clone(),
            listing: listing(
                &root,
                &[
                    ("src", RemoteFileKind::Directory),
                    ("readme.md", RemoteFileKind::File),
                ],
            ),
        });
        assert!(
            effects.iter().any(|effect| matches!(
                effect,
                RemoteFilesEffect::ListDirectory { path, generation, .. }
                    if *path == dir && *generation == refresh_generation
            )),
            "deferred listing must be flushed, got {:?}",
            effects
        );
        assert!(
            effects.iter().any(|effect| matches!(
                effect,
                RemoteFilesEffect::LoadPreview { path, generation, .. }
                    if *path == file && *generation == refresh_generation
            )),
            "deferred preview must be flushed, got {:?}",
            effects
        );
    }

    /// A restored cache can hold up to the whole row budget on its own, and
    /// the refreshed root listing arrives budgeted only against itself — the
    /// union must still respect the global bound.
    #[test]
    fn a_refreshed_root_rebudgets_the_restored_descendants() {
        let source = unique_source("rebudget");
        let root = RemotePath::from_server_absolute("/home/me").unwrap();
        let dir = root.join_name("big").unwrap();
        let mut state = RemoteFilesState::default();

        let wide_listing = |parent: &RemotePath, prefix: &str, count: usize, extra_dir: bool| {
            let mut entries: Vec<RemoteFileEntry> = (0..count)
                .map(|index| {
                    let name = format!("{prefix}-{index}");
                    RemoteFileEntry {
                        path: parent.join_name(&name).unwrap(),
                        name,
                        kind: RemoteFileKind::File,
                        size: None,
                    }
                })
                .collect();
            if extra_dir {
                entries.push(RemoteFileEntry {
                    path: parent.join_name("big").unwrap(),
                    name: "big".to_string(),
                    kind: RemoteFileKind::Directory,
                    size: None,
                });
            }
            RemoteDirectoryListing {
                entries,
                truncated: false,
            }
        };

        state.transition(RemoteFilesEvent::TargetChanged(Some(target(&source))));
        let generation = match state
            .transition(RemoteFilesEvent::ConnectRequested)
            .as_slice()
        {
            [RemoteFilesEffect::Connect { generation, .. }] => *generation,
            other => panic!("expected Connect, got {:?}", other),
        };
        state.transition(RemoteFilesEvent::Connected {
            generation,
            root: root.clone(),
            listing: wide_listing(&root, "root", 999, true),
        });
        state.transition(RemoteFilesEvent::ToggleDirectory(dir.clone()));
        state.transition(RemoteFilesEvent::DirectoryLoaded {
            generation,
            path: dir.clone(),
            listing: wide_listing(&dir, "nested", 1000, false),
        });
        assert_eq!(state.total_entries(), 2000);

        // Stash, restore, and refresh with a root listing that alone fills
        // the budget.
        state.transition(RemoteFilesEvent::TargetChanged(None));
        let effects = state.transition(RemoteFilesEvent::TargetChanged(Some(target(&source))));
        let refresh_generation = match effects.as_slice() {
            [_, RemoteFilesEffect::Connect { generation, .. }] => *generation,
            other => panic!("expected a background refresh, got {:?}", other),
        };
        state.transition(RemoteFilesEvent::Connected {
            generation: refresh_generation,
            root: root.clone(),
            listing: wide_listing(&root, "fresh", REMOTE_FILE_TREE_ROW_LIMIT, false),
        });
        assert!(
            state.total_entries() <= REMOTE_FILE_TREE_ROW_LIMIT,
            "restored descendants must not defeat the row budget: {} rows",
            state.total_entries()
        );
        assert_eq!(
            state
                .directories
                .get(&root)
                .map(|directory| directory.listing.entries.len()),
            Some(REMOTE_FILE_TREE_ROW_LIMIT),
            "the fresh root listing itself must survive the rebudget"
        );
    }

    #[test]
    fn no_click_never_connects_and_resume_requires_prior_success() {
        let source = unique_source("never-clicked");
        let mut state = RemoteFilesState::default();
        assert_eq!(
            state.transition(RemoteFilesEvent::TargetChanged(Some(target(&source)))),
            vec![RemoteFilesEffect::ReleaseLease]
        );
        assert!(state
            .transition(RemoteFilesEvent::ResumeRequested)
            .is_empty());
        assert_eq!(state.phase, RemoteFilesPhase::Disconnected);
        assert!(matches!(
            state
                .transition(RemoteFilesEvent::ConnectRequested)
                .as_slice(),
            [RemoteFilesEffect::Connect {
                allow_connect: true,
                ..
            }]
        ));
    }

    /// Clicking Connect once authorizes that *source* for the whole process,
    /// not just the window that did it — the SSH session is shared between
    /// windows, so making a second window ask again for a session that is
    /// already open is pure friction.
    #[test]
    fn a_successful_connection_authorizes_the_source_for_every_window() {
        let source = unique_source("shared-auth");
        let root = RemotePath::from_server_absolute("/home/me").unwrap();

        let mut second = RemoteFilesState::default();
        second.transition(RemoteFilesEvent::TargetChanged(Some(target(&source))));
        assert!(
            second
                .transition(RemoteFilesEvent::ResumeRequested)
                .is_empty(),
            "an unauthorized source must never connect on its own"
        );

        let mut first = RemoteFilesState::default();
        first.transition(RemoteFilesEvent::TargetChanged(Some(target(&source))));
        let generation = match first
            .transition(RemoteFilesEvent::ConnectRequested)
            .as_slice()
        {
            [RemoteFilesEffect::Connect { generation, .. }] => *generation,
            other => panic!("expected Connect, got {:?}", other),
        };
        first.transition(RemoteFilesEvent::Connected {
            generation,
            root: root.clone(),
            listing: listing(&root, &[]),
        });

        // The other window can now resume the same source without a click, and
        // is allowed to redial: once the pool's idle window lapses, silently
        // reconnecting beats demanding a second click for the same host.
        assert!(matches!(
            second
                .transition(RemoteFilesEvent::ResumeRequested)
                .as_slice(),
            [RemoteFilesEffect::Connect {
                allow_connect: true,
                ..
            }]
        ));

        // A different, untouched source is still locked down.
        let other = unique_source("shared-auth-other");
        let mut third = RemoteFilesState::default();
        third.transition(RemoteFilesEvent::TargetChanged(Some(target(&other))));
        assert!(third
            .transition(RemoteFilesEvent::ResumeRequested)
            .is_empty());
    }

    #[test]
    fn collapsing_directory_releases_descendants() {
        let source = unique_source("collapsing_directory_releases_descendants");
        let root = RemotePath::from_server_absolute("/home/me").unwrap();
        let child = root.join_name("src").unwrap();
        let mut state = RemoteFilesState::default();
        state.transition(RemoteFilesEvent::TargetChanged(Some(target(&source))));
        let generation = match state.transition(RemoteFilesEvent::ConnectRequested)[0] {
            RemoteFilesEffect::Connect { generation, .. } => generation,
            _ => unreachable!(),
        };
        state.transition(RemoteFilesEvent::Connected {
            generation,
            root: root.clone(),
            listing: listing(&root, &[("src", RemoteFileKind::Directory)]),
        });
        state.transition(RemoteFilesEvent::ToggleDirectory(child.clone()));
        state.transition(RemoteFilesEvent::DirectoryLoaded {
            generation,
            path: child.clone(),
            listing: listing(&child, &[("lib.rs", RemoteFileKind::File)]),
        });
        assert_eq!(state.rows().len(), 3);
        state.transition(RemoteFilesEvent::ToggleDirectory(child));
        assert_eq!(state.rows().len(), 2);
        assert_eq!(state.total_entries(), 1);
    }

    /// A deleted (or renamed-away) directory must drop its cached subtree
    /// immediately: orphaned listings keep eating the row budget, and a
    /// selection under the old path keeps a dead preview open.
    #[test]
    fn a_forgotten_entry_takes_its_subtree_and_selection_with_it() {
        let source = unique_source("forgotten");
        let root = RemotePath::from_server_absolute("/home/me").unwrap();
        let dir = root.join_name("src").unwrap();
        let file = dir.join_name("lib.rs").unwrap();
        let mut state = RemoteFilesState::default();
        state.transition(RemoteFilesEvent::TargetChanged(Some(target(&source))));
        let generation = match state.transition(RemoteFilesEvent::ConnectRequested)[0] {
            RemoteFilesEffect::Connect { generation, .. } => generation,
            _ => unreachable!(),
        };
        state.transition(RemoteFilesEvent::Connected {
            generation,
            root: root.clone(),
            listing: listing(&root, &[("src", RemoteFileKind::Directory)]),
        });
        state.transition(RemoteFilesEvent::ToggleDirectory(dir.clone()));
        state.transition(RemoteFilesEvent::DirectoryLoaded {
            generation,
            path: dir.clone(),
            listing: listing(&dir, &[("lib.rs", RemoteFileKind::File)]),
        });
        state.transition(RemoteFilesEvent::SelectFile(file));
        assert_eq!(state.rows().len(), 3);

        assert!(state
            .transition(RemoteFilesEvent::EntryForgotten(dir.clone()))
            .is_empty());
        assert!(!state.expanded.contains(&dir));
        assert!(!state.directories.contains_key(&dir));
        assert!(
            state.selected.is_none(),
            "a selection under the forgotten entry must not survive"
        );
        assert!(matches!(state.preview, RemotePreviewStatus::None));
        assert_eq!(state.total_entries(), 1, "only the root listing remains");

        // Forgetting the selected entry itself clears it too.
        let sibling = root.join_name("src").unwrap();
        state.transition(RemoteFilesEvent::SelectFile(sibling.clone()));
        state.transition(RemoteFilesEvent::EntryForgotten(sibling));
        assert!(state.selected.is_none());
    }

    #[test]
    fn stale_directory_result_is_ignored_after_panel_closes() {
        let source = unique_source("stale_directory_result_is_ignored_after_panel_closes");
        let path = RemotePath::from_server_absolute("/home/me/src").unwrap();
        let mut state = RemoteFilesState::default();
        state.transition(RemoteFilesEvent::TargetChanged(Some(target(&source))));
        let generation = state.generation;
        state.transition(RemoteFilesEvent::PanelHidden);
        state.transition(RemoteFilesEvent::DirectoryLoaded {
            generation,
            path: path.clone(),
            listing: listing(&path, &[("stale", RemoteFileKind::File)]),
        });
        assert!(state.rows().is_empty());
    }

    #[test]
    fn stale_directory_result_does_not_clear_a_new_loading_request() {
        let source = unique_source("stale_directory_result_does_not_clear_a_new_loading_request");
        let root = RemotePath::from_server_absolute("/home/me").unwrap();
        let child = root.join_name("src").unwrap();
        let mut state = RemoteFilesState::default();
        state.transition(RemoteFilesEvent::TargetChanged(Some(target(&source))));
        let first_generation = state.generation;
        state.transition(RemoteFilesEvent::PanelHidden);
        let effect = state.transition(RemoteFilesEvent::ConnectRequested);
        let current_generation = match effect[0] {
            RemoteFilesEffect::Connect { generation, .. } => generation,
            _ => unreachable!(),
        };
        state.transition(RemoteFilesEvent::Connected {
            generation: current_generation,
            root: root.clone(),
            listing: listing(&root, &[("src", RemoteFileKind::Directory)]),
        });
        state.transition(RemoteFilesEvent::ToggleDirectory(child.clone()));
        assert!(state.loading_directories.contains(&child));

        state.transition(RemoteFilesEvent::DirectoryFailed {
            generation: first_generation,
            path: child.clone(),
            message: "stale".to_string(),
        });
        assert!(state.loading_directories.contains(&child));
        assert!(state.error_message.is_none());
    }

    #[test]
    fn resume_does_not_reload_an_already_connected_panel() {
        let source = unique_source("resume_does_not_reload_an_already_connected_panel");
        let root = RemotePath::from_server_absolute("/home/me").unwrap();
        let mut state = RemoteFilesState::default();
        state.transition(RemoteFilesEvent::TargetChanged(Some(target(&source))));
        let generation = match state.transition(RemoteFilesEvent::ConnectRequested)[0] {
            RemoteFilesEffect::Connect { generation, .. } => generation,
            _ => unreachable!(),
        };
        state.transition(RemoteFilesEvent::Connected {
            generation,
            root: root.clone(),
            listing: listing(&root, &[]),
        });

        assert!(state
            .transition(RemoteFilesEvent::ResumeRequested)
            .is_empty());
        assert_eq!(state.phase, RemoteFilesPhase::Connected);
        assert_eq!(state.generation, generation);
    }

    #[test]
    fn directory_results_are_capped_to_the_global_row_budget() {
        let source = unique_source("directory_results_are_capped_to_the_global_row_budget");
        let root = RemotePath::from_server_absolute("/home/me").unwrap();
        let mut state = RemoteFilesState::default();
        state.transition(RemoteFilesEvent::TargetChanged(Some(target(&source))));
        let generation = match state.transition(RemoteFilesEvent::ConnectRequested)[0] {
            RemoteFilesEffect::Connect { generation, .. } => generation,
            _ => unreachable!(),
        };
        let entries = (0..REMOTE_FILE_TREE_ROW_LIMIT + 10)
            .map(|index| RemoteFileEntry {
                path: root.join_name(&format!("file-{index}")).unwrap(),
                name: format!("file-{index}"),
                kind: RemoteFileKind::File,
                size: None,
            })
            .collect();
        state.transition(RemoteFilesEvent::Connected {
            generation,
            root,
            listing: RemoteDirectoryListing {
                entries,
                truncated: true,
            },
        });
        // One synthetic root row plus at most the configured remote entries.
        assert_eq!(state.rows().len(), REMOTE_FILE_TREE_ROW_LIMIT + 1);
        assert!(state.has_truncated_directory());
    }
}
