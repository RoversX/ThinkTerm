use anyhow::{ensure, Context, Result};
use chrono::Utc;
use config::keyassignment::{SpawnCommand, SpawnTabDomain};
use fluent_bundle::FluentArgs;
use futures::future::LocalBoxFuture;
use mux::domain::SplitSource;
use mux::pane::PaneId;
use mux::tab::{PaneEntry, PaneNode, PaneStackEntry, SplitDirection, SplitRequest, SplitSize};
use mux::window::WindowId as MuxWindowId;
use mux::Mux;
use parking_lot::Mutex;
use portable_pty::CommandBuilder;
use serde::{Deserialize, Serialize};
use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::fs;
use std::hash::{Hash, Hasher};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use wezterm_term::Progress;
use wezterm_term::TerminalConfiguration;
use wezterm_term::TerminalSize;

pub type SpaceId = String;
pub type ProjectId = String;
pub type WorkspaceThreadId = String;

const DEFAULT_SPACE_ID: &str = "space-default";
const DEFAULT_SPACE_NAME: &str = "Default";
const REMOTE_PROJECT_SPACE_SEPARATOR: &str = "::space::";

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct WorkspaceThreadStore {
    #[serde(default)]
    pub spaces: Vec<Space>,
    #[serde(default)]
    pub last_active_space_id: Option<SpaceId>,
    #[serde(default, skip_serializing)]
    pub active_project_id: Option<ProjectId>,
    /// Which Space this device was last looking at on each mux domain. A
    /// server can host several Spaces, so reconnecting has to pick one; this
    /// is per-device by nature and never travels to the server.
    #[serde(default)]
    pub last_space_per_domain: HashMap<String, SpaceId>,
    pub projects: Vec<Project>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Space {
    pub id: SpaceId,
    pub name: String,
    pub active_project_id: Option<ProjectId>,
    /// The local Obsidian-compatible Vault shared by every Project in this
    /// Space.  The Vault contents remain ordinary user-owned files; ThinkTerm
    /// only persists this binding in its own workspace store.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note_vault: Option<SpaceVaultBinding>,
    #[serde(default)]
    pub is_default: bool,
    /// When set, this Space is dedicated to a wezterm mux client domain
    /// (`thinkterm connect <name>`). Such Spaces are found-or-created by domain
    /// name, are never claimed by startup/Dock windows, and never persist
    /// layout locally (the remote mux server owns the layout truth).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_domain: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SpaceVaultBinding {
    pub root: PathBuf,
    #[serde(default)]
    pub managed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Project {
    pub id: ProjectId,
    #[serde(default)]
    pub space_id: SpaceId,
    pub name: String,
    pub path: PathBuf,
    #[serde(default)]
    pub threads: Vec<WorkspaceThread>,
    pub active_thread_id: Option<WorkspaceThreadId>,
    #[serde(default)]
    pub threads_collapsed: bool,
    /// Vault-relative Markdown path last opened for this Project. Projects in
    /// the same Space share a Vault but intentionally remember independent
    /// active notes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_note_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkspaceThread {
    pub id: WorkspaceThreadId,
    pub name: String,
    pub project_id: ProjectId,
    pub layout: Option<WorkspaceThreadLayoutSnapshot>,
    /// Per-pane font scales for mux-domain threads, keyed by the REMOTE
    /// pane id (stable across resyncs for the life of the server). Local
    /// threads keep font scales inside `layout`; mux threads must never
    /// persist layout (the server owns it), but font scale is client-side
    /// presentation state, so it is persisted separately here.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub remote_font_scales: HashMap<PaneId, f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub planned_workspace_name: Option<String>,
    pub materialized_workspace_name: Option<String>,
    pub last_active_at: i64,
    #[serde(default)]
    pub is_pinned: bool,
    #[serde(default)]
    pub is_unread: bool,
    #[serde(skip)]
    pub work_is_running: bool,
    #[serde(skip)]
    pub work_needs_attention: bool,
    #[serde(default)]
    pub work_finished_unseen: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkspaceThreadLayoutSnapshot {
    pub active_tab: usize,
    pub tabs: Vec<serde_json::Value>,
    #[serde(default)]
    pub terminal_specs: Vec<TerminalSpecEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TerminalSpecEntry {
    pub pane_id: PaneId,
    pub spec: TerminalSpawnSpec,
    #[serde(default)]
    pub font_scale: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TerminalSpawnSpec {
    pub cwd: Option<String>,
    pub domain: Option<String>,
    pub title: String,
}

#[derive(Debug, Clone)]
pub struct WorkspaceThreadsView {
    pub pinned_threads: Vec<WorkspaceThreadView>,
    pub projects: Vec<ProjectView>,
}

#[derive(Debug, Clone)]
pub struct ProjectView {
    pub id: ProjectId,
    pub name: String,
    pub is_active: bool,
    pub threads_collapsed: bool,
    pub threads: Vec<WorkspaceThreadView>,
    /// True when this project is a remote SSH host rather than a local folder.
    pub is_remote: bool,
    /// Detected `/etc/os-release` `ID` for remote hosts, used to pick an OS icon.
    pub distro: Option<String>,
}

#[derive(Debug, Clone)]
pub struct WorkspaceThreadView {
    pub id: WorkspaceThreadId,
    pub name: String,
    pub is_active: bool,
    pub is_materialized: bool,
    pub is_pinned: bool,
    pub is_unread: bool,
    pub work_status: WorkspaceThreadWorkStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceThreadWorkStatus {
    Idle,
    Running,
    NeedsAttention,
    FinishedUnseen,
}

impl WorkspaceThreadWorkStatus {
    pub const ALL: [Self; 4] = [
        Self::Idle,
        Self::Running,
        Self::NeedsAttention,
        Self::FinishedUnseen,
    ];

    /// Stable identifier used to persist sidebar status filters.
    pub fn settings_key(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Running => "running",
            Self::NeedsAttention => "needs-attention",
            Self::FinishedUnseen => "finished",
        }
    }

    pub fn from_settings_key(key: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|status| status.settings_key() == key)
    }
}

impl From<WorkspaceThreadWorkStatus> for codec::ThinkTermSessionWorkStatus {
    fn from(status: WorkspaceThreadWorkStatus) -> Self {
        match status {
            WorkspaceThreadWorkStatus::Idle => Self::Idle,
            WorkspaceThreadWorkStatus::Running => Self::Running,
            WorkspaceThreadWorkStatus::NeedsAttention => Self::NeedsAttention,
            WorkspaceThreadWorkStatus::FinishedUnseen => Self::FinishedUnseen,
        }
    }
}

/// A thread whose work finished without being seen or needs attention,
/// across every Space; feeds the sidebar notification bell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadWorkNotification {
    pub space_id: SpaceId,
    pub space_name: String,
    pub project_name: String,
    pub thread_id: WorkspaceThreadId,
    pub thread_name: String,
    pub status: WorkspaceThreadWorkStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WorkspaceThreadWorkChange {
    changed: bool,
    should_persist: bool,
    /// What this transition is worth saying out loud, if anything. Set only on
    /// the observation that first crosses into the state, so re-scanning a
    /// thread that has not moved stays quiet without any extra bookkeeping.
    announce: Option<WorkAnnouncement>,
}

/// A transition is at most one of these, so a single field makes it impossible
/// to announce two things about the same moment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkAnnouncement {
    /// Work that was running has stopped.
    Finished,
    /// Something is waiting on the user.
    NeedsInput,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivationPlan {
    pub project_id: ProjectId,
    pub thread_id: WorkspaceThreadId,
    pub workspace_name: String,
    pub project_path: PathBuf,
    pub needs_materialize: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadConnectionState {
    pub space_id: SpaceId,
    pub project_id: ProjectId,
    pub thread_id: WorkspaceThreadId,
    pub project_name: String,
    pub thread_name: String,
    pub workspace_name: String,
    pub is_remote: bool,
    pub is_live: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeletedWorkspaceThread {
    pub was_active: bool,
    pub next_thread_id: Option<WorkspaceThreadId>,
    pub materialized_workspace_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemovedProject {
    pub was_active: bool,
    pub next_thread_id: Option<WorkspaceThreadId>,
    pub materialized_workspace_names: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EndWorkspaceThreadResult {
    DeletedThread(DeletedWorkspaceThread),
    RemovedProject(RemovedProject),
    Noop,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisconnectedWorkspaceThread {
    pub was_active: bool,
    pub next_thread_id: Option<WorkspaceThreadId>,
    pub workspace_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeletedSpace {
    pub materialized_workspace_names: Vec<String>,
    pub fallback_space_id: SpaceId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteSpaceError {
    NotFound,
    DefaultSpace,
    LastSpace,
    Occupied,
    RemoteUnavailable,
    ServerRejected,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpaceView {
    pub id: SpaceId,
    pub name: String,
    pub is_active: bool,
    pub is_default: bool,
    pub is_occupied_by_other_window: bool,
    /// True for a Space that lives on a remote mux client domain
    /// (`thinkterm connect`); the UI labels these distinctly and their
    /// "disconnect" is local-only (the remote server keeps running).
    pub is_remote: bool,
    /// The mux domain hosting this Space, when it is remote. One server can
    /// host several Spaces, so the Space menu groups by this.
    pub domain: Option<String>,
    /// True while the hosting domain is attached and not mid-(re)connect.
    /// Always false for local Spaces; consult `is_swipe_reachable` instead of
    /// reading this directly.
    pub is_domain_attached: bool,
}

impl SpaceView {
    /// Whether a Space swipe may pass through or land on this Space.
    ///
    /// A swipe commit adopts the destination the moment the finger lifts, so
    /// the destination must be paintable and adoptable right now: local
    /// Spaces always are, remote Spaces only while their domain is attached.
    /// A disconnected domain keeps its click-to-connect flow (with its
    /// connection UI and error reporting) instead of parking the gesture on a
    /// page that may take ten seconds to exist or never arrive.
    pub fn is_swipe_reachable(&self) -> bool {
        !self.is_occupied_by_other_window && (!self.is_remote || self.is_domain_attached)
    }
}

pub fn adjacent_swipe_space_ids(
    spaces: &[SpaceView],
    active_space_id: &str,
) -> (Option<SpaceId>, Option<SpaceId>) {
    let reachable = spaces
        .iter()
        .filter(|space| space.is_swipe_reachable())
        .collect::<Vec<_>>();
    let Some(index) = reachable
        .iter()
        .position(|space| space.id == active_space_id)
    else {
        return (None, None);
    };

    let previous = index
        .checked_sub(1)
        .and_then(|previous| reachable.get(previous))
        .map(|space| space.id.clone());
    let next = reachable.get(index + 1).map(|space| space.id.clone());
    (previous, next)
}

lazy_static::lazy_static! {
    static ref THREAD_STORE: Mutex<WorkspaceThreadStore> =
        Mutex::new(load_workspace_thread_store().unwrap_or_else(|err| {
            log::error!("cannot read the ThinkTerm workspace store: {err:#}");
            preserve_unreadable_workspace_thread_store();
            STORE_IS_UNREADABLE.store(true, Ordering::Release);
            WorkspaceThreadStore::default()
        }));
    static ref WINDOW_SPACES: Mutex<HashMap<u64, SpaceId>> = Mutex::new(HashMap::new());
    static ref MATERIALIZING_LAYOUT_WORKSPACES: Mutex<HashMap<String, usize>> =
        Mutex::new(HashMap::new());
    /// Workspaces whose saved layout this build could not decode. They opened
    /// as a single terminal, so snapshotting them would write that one pane
    /// over the arrangement we failed to read — turning a version skew into
    /// permanent data loss. Held in memory only: a build that can read the
    /// file again starts with an empty set and resumes saving normally.
    static ref UNREADABLE_LAYOUT_WORKSPACES: Mutex<std::collections::HashSet<String>> =
        Mutex::new(std::collections::HashSet::new());
    static ref WORK_RUNNING_LAST_SEEN: Mutex<HashMap<String, std::time::Instant>> =
        Mutex::new(HashMap::new());
    static ref WORK_SOUND_TIMING: Mutex<WorkSoundTiming> = Mutex::new(WorkSoundTiming::default());
    static ref WORK_STATUS_RECHECK_PENDING: Mutex<std::collections::HashSet<String>> =
        Mutex::new(std::collections::HashSet::new());
    /// The sidebar tree each mux server last told us it had, keyed by domain
    /// name. A reconcile diffs against this: rows that still match it are left
    /// alone (another device may have changed them since), and rows it has
    /// that we no longer do are the ones to delete.
    static ref LAST_KNOWN_REMOTE_TREES: Mutex<HashMap<String, codec::ThinkTermTree>> =
        Mutex::new(HashMap::new());
    /// Online mutations currently in flight, keyed by domain name.  This is a
    /// short-lived presentation overlay only: it is never persisted or
    /// replayed after a disconnect.  A remote edit is rejected while the
    /// domain is unavailable, and a reconnect always starts from the server's
    /// tree rather than from client intent left over from the old connection.
    static ref IN_FLIGHT_TREE_OPS: Mutex<HashMap<String, Vec<codec::TreeOp>>> =
        Mutex::new(HashMap::new());
    /// Remote Spaces this device has disconnected from, keyed by domain name.
    /// The server still has them and other devices still see them; this device
    /// filters them out of every push until it reconnects to that server.
    static ref LOCALLY_HIDDEN_SPACES: Mutex<HashMap<String, std::collections::HashSet<SpaceId>>> =
        Mutex::new(HashMap::new());
    /// Servers we have just connected to and not yet heard a tree from. The
    /// first tree of a connection is the one that catches this device up, and
    /// it is not necessarily the answer to our own fetch — a broadcast can
    /// overtake it.
    static ref FIRST_TREE_PENDING: Mutex<std::collections::HashSet<String>> =
        Mutex::new(std::collections::HashSet::new());
}

static THREAD_STORE_PERSIST_SCHEDULED: AtomicBool = AtomicBool::new(false);
static THREAD_STORE_PERSIST_DIRTY: AtomicBool = AtomicBool::new(false);
/// Orders writes of the store file and discards snapshots that have been
/// overtaken.
///
/// The debounced writer copies the store, releases the store lock, and only
/// then spends ~88ms serializing and fsyncing. A synchronous save landing
/// inside that window writes newer data first and is then overwritten when
/// the older copy finishes its rename -- silently undoing whatever the user
/// had just done. Serializing the writes alone does not help: the stale copy
/// would still be the one to land last. Stamping each snapshot as it is taken
/// is what lets the loser be dropped instead of applied.
struct StoreWriteGate {
    next_seq: AtomicU64,
    last_written: Mutex<u64>,
}

impl StoreWriteGate {
    const fn new() -> Self {
        Self {
            next_seq: AtomicU64::new(0),
            last_written: Mutex::new(0),
        }
    }

    /// Stamp a snapshot. Callers hold `THREAD_STORE`, so the numbers come out
    /// in the same order as the mutations they describe.
    fn claim(&self) -> u64 {
        self.next_seq.fetch_add(1, Ordering::AcqRel) + 1
    }

    /// Run `write` unless a later snapshot already reached disk. Returns
    /// whether it ran. Writes are serialized against each other, so `write`
    /// never races another copy of the file.
    fn write_if_newest(&self, seq: u64, write: impl FnOnce() -> bool) -> bool {
        let mut last_written = self.last_written.lock();
        if *last_written >= seq {
            return false;
        }
        if !write() {
            return false;
        }
        *last_written = seq;
        true
    }
}

static THREAD_STORE_WRITES: StoreWriteGate = StoreWriteGate::new();
/// Set when the store on disk exists but could not be read.
///
/// Nothing may be written while it is set. The in-memory store is empty in
/// that case, so saving would replace a file this build could not parse with
/// one describing nothing at all — every Space, Project and Thread the user
/// has, and the evidence needed to work out why, gone in a single atomic
/// rename. The same shape as a layout snapshot the renderer cannot decode,
/// and the same answer: read failures must not be allowed to write.
static STORE_IS_UNREADABLE: AtomicBool = AtomicBool::new(false);
static NEXT_SPACE_OWNER_ID: AtomicU64 = AtomicU64::new(1);
fn publish_thinkterm_session_changed() {
    wezterm_mux_server_impl::thinkterm_session::publish_changed();
}

pub fn workspace_thread_store_path() -> PathBuf {
    crate::native_paths::data_file("workspace_threads.json")
}

pub fn load_workspace_thread_store() -> Result<WorkspaceThreadStore> {
    load_workspace_thread_store_from_path(&workspace_thread_store_path())
}

pub fn load_workspace_thread_store_from_path(path: &Path) -> Result<WorkspaceThreadStore> {
    if !path.exists() {
        return Ok(WorkspaceThreadStore::default());
    }
    let file = fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut value: serde_json::Value =
        serde_json::from_reader(file).with_context(|| format!("parse {}", path.display()))?;
    migrate_legacy_remote_hosts(&value);
    let removed = drop_legacy_archived_threads(&mut value);
    if removed > 0 {
        log::info!("dropped {removed} legacy archived ThinkTerm workspace threads");
    }
    let mut store: WorkspaceThreadStore =
        serde_json::from_value(value).with_context(|| format!("parse {}", path.display()))?;
    store.normalize_after_load();
    Ok(store)
}

fn drop_legacy_archived_threads(value: &mut serde_json::Value) -> usize {
    let Some(projects) = value
        .get_mut("projects")
        .and_then(|projects| projects.as_array_mut())
    else {
        return 0;
    };

    let mut removed = 0usize;
    for project in projects {
        let active_thread_id = project
            .get("active_thread_id")
            .and_then(|id| id.as_str())
            .map(str::to_string);

        let active_thread_update = {
            let Some(threads) = project
                .get_mut("threads")
                .and_then(|threads| threads.as_array_mut())
            else {
                continue;
            };

            let mut removed_active = false;
            threads.retain(|thread| {
                let archived = thread
                    .get("archived")
                    .and_then(|archived| archived.as_bool())
                    .unwrap_or(false);
                if archived {
                    removed += 1;
                    if active_thread_id.as_deref() == thread.get("id").and_then(|id| id.as_str()) {
                        removed_active = true;
                    }
                }
                !archived
            });

            if removed_active {
                Some(first_legacy_thread_id(threads))
            } else if active_thread_id.as_deref().is_some_and(|active_id| {
                !threads
                    .iter()
                    .any(|thread| thread.get("id").and_then(|id| id.as_str()) == Some(active_id))
            }) {
                Some(first_legacy_thread_id(threads))
            } else {
                None
            }
        };

        if let Some(active_thread_update) = active_thread_update {
            project["active_thread_id"] = active_thread_update;
        }
    }

    removed
}

fn first_legacy_thread_id(threads: &[serde_json::Value]) -> serde_json::Value {
    threads
        .iter()
        .find_map(|thread| thread.get("id").and_then(|id| id.as_str()))
        .map(|id| serde_json::Value::String(id.to_string()))
        .unwrap_or(serde_json::Value::Null)
}

fn migrate_legacy_remote_hosts(value: &serde_json::Value) {
    let Some(projects) = value
        .get("projects")
        .and_then(|projects| projects.as_array())
    else {
        return;
    };

    for project in projects {
        let Some(remote) = project.get("remote") else {
            continue;
        };
        let project_label = project
            .get("name")
            .and_then(|name| name.as_str())
            .unwrap_or("SSH Host");
        let project_id = project
            .get("id")
            .and_then(|id| id.as_str())
            .unwrap_or("<unknown>");

        let mut spec = match serde_json::from_value::<crate::ssh_hosts::SshHostSpec>(remote.clone())
        {
            Ok(spec) => spec,
            Err(err) => {
                log::warn!(
                    "failed to migrate legacy SSH host from workspace project {project_id}: {err:#}"
                );
                continue;
            }
        };

        if spec.label.trim().is_empty() {
            spec.label = project_label.to_string();
        }

        if let Some(password) = spec.password.clone() {
            if !password.is_empty() && !crate::secret::is_encrypted(&password) {
                match crate::secret::encrypt(&password) {
                    Ok(encrypted) => spec.password = Some(encrypted),
                    Err(err) => {
                        log::warn!(
                            "failed to encrypt legacy SSH password for workspace project {project_id}: {err:#}"
                        );
                        continue;
                    }
                }
            }
        }

        if let Err(err) = crate::ssh_hosts::try_import_legacy_host(spec) {
            log::warn!(
                "failed to migrate legacy SSH host from workspace project {project_id}: {err:#}"
            );
        }
    }
}

/// Copy a store that could not be read to a name that will not be reopened.
///
/// Refusing to write already keeps the original safe; this makes it safe from
/// the user too, who has no reason to suspect the untouched-looking file is
/// the only copy of their workspace and may well delete it while trying to get
/// a working app back.
fn preserve_unreadable_workspace_thread_store() {
    let path = workspace_thread_store_path();
    let Some(name) = path.file_stem().and_then(|stem| stem.to_str()) else {
        return;
    };
    let preserved = path.with_file_name(format!("{name}.unreadable-{}.json", now_ts()));
    if preserved.exists() {
        return;
    }
    match fs::copy(&path, &preserved) {
        Ok(_) => log::error!(
            "kept a copy of the unreadable workspace store at {}. \
             ThinkTerm will not save over the original, so nothing further \
             is lost; this session's changes are not being saved either.",
            preserved.display()
        ),
        Err(err) => log::error!(
            "cannot copy the unreadable workspace store to {}: {err:#}",
            preserved.display()
        ),
    }
}

pub fn save_workspace_thread_store(store: &WorkspaceThreadStore) -> Result<()> {
    save_workspace_thread_store_to_path(&workspace_thread_store_path(), store)
}

pub fn save_workspace_thread_store_to_path(
    path: &Path,
    store: &WorkspaceThreadStore,
) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
        let mut file = tempfile::NamedTempFile::new_in(parent)
            .with_context(|| format!("create temporary thread store in {}", parent.display()))?;
        serde_json::to_writer_pretty(&mut file, store)
            .with_context(|| format!("write {}", path.display()))?;
        file.flush()
            .with_context(|| format!("flush {}", path.display()))?;
        file.as_file()
            .sync_all()
            .with_context(|| format!("sync {}", path.display()))?;
        file.persist(path)
            .with_context(|| format!("replace {}", path.display()))?;
        return Ok(());
    }

    let mut file = fs::File::create(path).with_context(|| format!("create {}", path.display()))?;
    serde_json::to_writer_pretty(&mut file, store)
        .with_context(|| format!("write {}", path.display()))?;
    file.flush()
        .with_context(|| format!("flush {}", path.display()))?;
    file.sync_all()
        .with_context(|| format!("sync {}", path.display()))
}

pub fn default_space_id() -> SpaceId {
    DEFAULT_SPACE_ID.to_string()
}

pub fn next_space_owner_id() -> u64 {
    NEXT_SPACE_OWNER_ID.fetch_add(1, Ordering::Relaxed)
}

pub fn space_for_window(owner_id: u64) -> SpaceId {
    if let Some(space_id) = WINDOW_SPACES.lock().get(&owner_id).cloned() {
        return space_id;
    }
    claim_initial_space_for_window(owner_id)
}

pub fn claim_initial_space_for_window(owner_id: u64) -> SpaceId {
    let occupied = WINDOW_SPACES
        .lock()
        .iter()
        .filter_map(|(owner, space)| (*owner != owner_id).then(|| space.clone()))
        .collect::<std::collections::HashSet<_>>();

    let mut store = THREAD_STORE.lock();
    let changed = store.normalize_after_load();
    let space_id = store.claim_available_space_id(&occupied);
    if store.last_active_space_id.as_deref() != Some(&space_id) {
        store.last_active_space_id = Some(space_id.clone());
        persist_locked(&store);
    } else if changed {
        persist_locked(&store);
    }
    drop(store);

    WINDOW_SPACES.lock().insert(owner_id, space_id.clone());
    space_id
}

pub fn claim_space_for_new_window(owner_id: u64) -> SpaceId {
    claim_initial_space_for_window(owner_id)
}

/// Claim a Space for a window the user did not explicitly open (domain-owned
/// windows spawned by the reconcile, e.g. a reconnect/auth prompt window).
/// Picks a free Space like [`claim_initial_space_for_window`] but never
/// records it as the user's last active Space: these windows are incidental
/// and must not affect which Space the next startup or Dock window restores.
pub fn claim_space_for_incidental_window(owner_id: u64) -> SpaceId {
    let occupied = WINDOW_SPACES
        .lock()
        .iter()
        .filter_map(|(owner, space)| (*owner != owner_id).then(|| space.clone()))
        .collect::<std::collections::HashSet<_>>();

    let mut store = THREAD_STORE.lock();
    let changed = store.normalize_after_load();
    let space_id = store.claim_available_space_id(&occupied);
    if changed {
        persist_locked(&store);
    }
    drop(store);

    WINDOW_SPACES.lock().insert(owner_id, space_id.clone());
    space_id
}

pub fn release_window_space(owner_id: u64) {
    WINDOW_SPACES.lock().remove(&owner_id);
}

fn window_owner_for_space_in(assignments: &HashMap<u64, SpaceId>, space_id: &str) -> Option<u64> {
    assignments
        .iter()
        .find_map(|(owner_id, active_space)| (active_space == space_id).then_some(*owner_id))
}

/// Return the native-window owner currently displaying `space_id`, if any.
/// Space ownership remains exclusive; this is a read-only lookup used by
/// global UI such as Live Overview to focus the window that already owns it.
pub fn window_owner_for_space(space_id: &str) -> Option<u64> {
    window_owner_for_space_in(&WINDOW_SPACES.lock(), space_id)
}

pub fn switch_window_space(owner_id: u64, space_id: &str) -> bool {
    if WINDOW_SPACES
        .lock()
        .iter()
        .any(|(owner, active_space)| *owner != owner_id && active_space == space_id)
    {
        return false;
    }

    let mut store = THREAD_STORE.lock();
    if !store.has_space(space_id) {
        return false;
    }
    store.last_active_space_id = Some(space_id.to_string());
    store.remember_space_for_domain(space_id);
    schedule_workspace_thread_store_persist();
    drop(store);

    WINDOW_SPACES.lock().insert(owner_id, space_id.to_string());
    true
}

pub fn spaces_for_window(owner_id: u64) -> Vec<SpaceView> {
    let active_space_id = space_for_window(owner_id);
    let occupied = WINDOW_SPACES.lock().clone();
    let mut store = THREAD_STORE.lock();
    if store.normalize_after_load() {
        persist_locked(&store);
    }
    let mut views: Vec<SpaceView> = store
        .spaces
        .iter()
        .map(|space| SpaceView {
            id: space.id.clone(),
            name: space.name.clone(),
            is_active: space.id == active_space_id,
            is_default: space.is_default,
            is_occupied_by_other_window: occupied
                .iter()
                .any(|(owner, active_space)| *owner != owner_id && active_space == &space.id),
            is_remote: space.client_domain.is_some(),
            domain: space.client_domain.clone(),
            is_domain_attached: false,
        })
        .collect();
    // Resolved after the store lock is released: the mux takes its own locks
    // and must not nest inside ours.
    drop(store);
    for view in &mut views {
        if let Some(domain) = view.domain.as_deref() {
            view.is_domain_attached = remote_tree_domain_is_attached(domain);
        }
    }
    views
}

pub fn active_space_name(space_id: &str) -> Option<String> {
    let mut store = THREAD_STORE.lock();
    if store.normalize_after_load() {
        persist_locked(&store);
    }
    store
        .spaces
        .iter()
        .find(|space| space.id == space_id)
        .map(|space| space.name.clone())
}

/// Return the Vault binding for a Space. A Space owns exactly one Vault and
/// every Project in that Space sees the same root.
pub fn space_note_vault(space_id: &str) -> Option<SpaceVaultBinding> {
    let mut store = THREAD_STORE.lock();
    if store.normalize_after_load() {
        persist_locked(&store);
    }
    store
        .spaces
        .iter()
        .find(|space| space.id == space_id)
        .and_then(|space| space.note_vault.clone())
}

/// Bind a Space to a local Vault. Managed Vaults are allowed to be created;
/// existing Vaults must already be directories. Canonicalizing here gives the
/// document registry one stable key even when a picker returns a symlinked
/// path.
pub fn set_space_note_vault(
    space_id: &str,
    root: PathBuf,
    managed: bool,
) -> Result<SpaceVaultBinding> {
    if managed {
        fs::create_dir_all(&root)
            .with_context(|| format!("create note vault {}", root.display()))?;
    }
    ensure!(
        root.is_dir(),
        "note vault is not a directory: {}",
        root.display()
    );
    let root = root
        .canonicalize()
        .with_context(|| format!("resolve note vault {}", root.display()))?;
    let binding = SpaceVaultBinding { root, managed };

    let mut store = THREAD_STORE.lock();
    store.normalize_after_load();
    let space = store
        .spaces
        .iter_mut()
        .find(|space| space.id == space_id)
        .with_context(|| format!("unknown Space {space_id}"))?;
    if space.note_vault.as_ref() != Some(&binding) {
        space.note_vault = Some(binding.clone());
        // Folder picking runs its completion on the UI thread.  Serializing and
        // fsyncing the complete workspace store here made selecting a Vault
        // visibly stall before Note could even start loading.  The shared
        // coalescing worker snapshots the latest store, so rapid changes still
        // persist in order without blocking input.
        schedule_workspace_thread_store_persist();
    }
    Ok(binding)
}

pub fn create_space(name: Option<String>) -> SpaceId {
    let mut store = THREAD_STORE.lock();
    store.normalize_after_load();
    let name = name.unwrap_or_else(|| next_space_name(&store.spaces));
    let id = store.create_space_record(name);
    store.last_active_space_id = Some(id.clone());
    persist_locked(&store);
    id
}

/// Add another Space to a remote server.
///
/// The Space lives on the server like any other, so it is created locally with
/// a fresh id and the same id is sent up; every other device attached to that
/// server sees it appear. The domain's own project (the one carrying the
/// `wezterm-mux://` sentinel path) and its first thread come along, because a
/// Space with nothing in it has nowhere to put a terminal.
pub fn create_space_on_domain(domain_name: &str, name: Option<String>) -> Result<SpaceId> {
    ensure!(
        remote_tree_mutation_allowed(Some(domain_name)),
        "{domain_name} is disconnected"
    );
    let mut store = THREAD_STORE.lock();
    store.normalize_after_load();
    let name = name.unwrap_or_else(|| next_space_name(&store.spaces));
    let space_id = store.create_space_record_for_domain(name, Some(domain_name.to_string()));

    let project_id = remote_project_id_for_space(&space_id, &mux_domain_host_id(domain_name));
    let thread = WorkspaceThread::new(project_id.clone(), "main".to_string(), None);
    let thread_id = thread.id.clone();
    store.projects.push(Project {
        id: project_id.clone(),
        space_id: space_id.clone(),
        name: domain_name.to_string(),
        path: PathBuf::from(format!("wezterm-mux://{domain_name}")),
        threads: vec![thread],
        active_thread_id: Some(thread_id),
        threads_collapsed: false,
        active_note_path: None,
    });
    if let Some(space) = store.spaces.iter_mut().find(|space| space.id == space_id) {
        space.active_project_id = Some(project_id);
    }
    store.last_active_space_id = Some(space_id.clone());
    persist_locked(&store);
    drop(store);

    reconcile_remote_subtree(domain_name);
    Ok(space_id)
}

pub fn ensure_space_named(name: &str) -> SpaceId {
    let name = name.trim();
    let name = if name.is_empty() { "Default" } else { name };
    let mut store = THREAD_STORE.lock();
    let mut changed = store.normalize_after_load();
    let id = store
        .spaces
        .iter()
        .find(|space| space.name.eq_ignore_ascii_case(name))
        .map(|space| space.id.clone())
        .unwrap_or_else(|| {
            changed = true;
            store.create_space_record(name.to_string())
        });
    if store.last_active_space_id.as_deref() != Some(&id) {
        store.last_active_space_id = Some(id.clone());
        changed = true;
    }
    if changed {
        persist_locked(&store);
    }
    id
}

pub fn rename_space(space_id: &str, name: String) -> bool {
    let mut store = THREAD_STORE.lock();
    let domain = tree_domain_for_space(&store, space_id);
    if !remote_tree_mutation_allowed(domain.as_deref()) {
        return false;
    }
    let changed = store.rename_space(space_id, name.clone());
    if changed {
        if let Some(domain) = domain {
            submit_tree_op(
                domain,
                codec::TreeOp::RenameSpace {
                    space_id: space_id.to_string(),
                    name,
                },
            );
        }
        persist_locked(&store);
    }
    changed
}

/// What removing a Space from this window should mean.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpaceRemoval {
    /// Drop this device's copy only. For a remote Space the rows are a cache
    /// of the server's tree, so the Space comes back on the next connect and
    /// other devices never notice — this is "disconnect", not "delete".
    Local,
    /// Delete the Space on the server that hosts it, so it disappears
    /// everywhere. Falls back to a local removal for a local Space.
    Everywhere,
}

/// Every mux domain this device currently mirrors Spaces from.
pub fn remote_space_domains() -> Vec<String> {
    THREAD_STORE.lock().remote_space_domains()
}

/// The Spaces this device mirrored from a mux server it reaches under any of
/// `domain_names`.
///
/// A host is connected under more than one name — the label for a ThinkTerm
/// Connect domain, `ssh:user@host` for a direct one — and the Space records
/// whichever one it arrived through, so a caller asking "what did this host
/// bring in" has to ask about all of them.
pub fn space_ids_for_domains(domain_names: &[String]) -> Vec<SpaceId> {
    THREAD_STORE.lock().space_ids_for_domains(domain_names)
}

/// Owner id no live window can have: they are handed out from 1.
const NO_WINDOW_OWNER: u64 = 0;

/// Drop this device's copy of `space_id` with no window in the picture.
///
/// The windowed path ([`TermWindow::start_delete_space`]) exists because a
/// window showing the Space has to be moved off it first. At startup there are
/// no windows yet, so there is nothing to move.
pub fn forget_space_locally(space_id: &str) -> Result<DeletedSpace, DeleteSpaceError> {
    delete_space_for_window_local(NO_WINDOW_OWNER, space_id, SpaceRemoval::Local)
}

fn delete_space_for_window_local(
    owner_id: u64,
    space_id: &str,
    removal: SpaceRemoval,
) -> Result<DeletedSpace, DeleteSpaceError> {
    let window_was_active = WINDOW_SPACES
        .lock()
        .get(&owner_id)
        .is_some_and(|active_space| active_space == space_id);
    let occupied_by_other = WINDOW_SPACES
        .lock()
        .iter()
        .filter_map(|(owner, active_space)| (*owner != owner_id).then(|| active_space.clone()))
        .collect::<std::collections::HashSet<_>>();
    if occupied_by_other.contains(space_id) {
        return Err(DeleteSpaceError::Occupied);
    }

    let mut store = THREAD_STORE.lock();
    // Resolve the owning server before the Space record is gone.
    let domain = tree_domain_for_space(&store, space_id);
    let mut deleted = store.delete_space(space_id)?;
    if window_was_active {
        let fallback_space_id = store
            .spaces
            .iter()
            .find(|space| space.is_default && !occupied_by_other.contains(&space.id))
            .or_else(|| {
                store
                    .spaces
                    .iter()
                    .find(|space| !occupied_by_other.contains(&space.id))
            })
            .map(|space| space.id.clone())
            .unwrap_or_else(|| {
                let name = next_space_name(&store.spaces);
                store.create_space_record(name)
            });
        store.last_active_space_id = Some(fallback_space_id.clone());
        deleted.fallback_space_id = fallback_space_id;
    }
    persist_locked(&store);
    drop(store);

    if window_was_active {
        WINDOW_SPACES
            .lock()
            .insert(owner_id, deleted.fallback_space_id.clone());
    }

    if let Some(domain) = domain {
        match removal {
            SpaceRemoval::Everywhere => {
                submit_tree_op(
                    domain,
                    codec::TreeOp::DeleteSpace {
                        space_id: space_id.to_string(),
                    },
                );
            }
            SpaceRemoval::Local => {
                // The server keeps the Space and other devices keep seeing it,
                // so its next push still carries it. Remember to filter it out
                // here until this device connects to that server again, which
                // is when the menu says it comes back.
                hide_space_locally(&domain, space_id);
            }
        }
    }

    Ok(deleted)
}

/// Delete a Space, waiting for the owning mux server before changing any
/// local ownership or tearing down mirror windows.  Keeping the windows alive
/// across the await also keeps the last ClientDomain reference attached, so a
/// sole-Space delete cannot strand its request in a detached client.
pub async fn delete_space_for_window(
    owner_id: u64,
    space_id: &str,
    removal: SpaceRemoval,
    end_remote_sessions: bool,
) -> Result<DeletedSpace, DeleteSpaceError> {
    let domain_name = {
        let store = THREAD_STORE.lock();
        tree_domain_for_space(&store, space_id)
    };

    if removal != SpaceRemoval::Everywhere || domain_name.is_none() {
        return delete_space_for_window_local(owner_id, space_id, removal);
    }

    // Validate without changing the real store, and capture the remote
    // workspaces while the authoritative response still has their rows.
    let (window_was_active, occupied_by_other, mut deleted) = {
        let window_was_active = WINDOW_SPACES
            .lock()
            .get(&owner_id)
            .is_some_and(|active_space| active_space == space_id);
        let occupied_by_other = WINDOW_SPACES
            .lock()
            .iter()
            .filter_map(|(owner, active_space)| (*owner != owner_id).then(|| active_space.clone()))
            .collect::<std::collections::HashSet<_>>();
        if occupied_by_other.contains(space_id) {
            return Err(DeleteSpaceError::Occupied);
        }
        let store = THREAD_STORE.lock();
        let mut staged = store.clone();
        let deleted = staged.delete_space(space_id)?;
        (window_was_active, occupied_by_other, deleted)
    };

    let domain_name = domain_name.unwrap();
    if !remote_tree_domain_is_attached(&domain_name) {
        notify_remote_tree_mutation_unavailable(&domain_name);
        return Err(DeleteSpaceError::RemoteUnavailable);
    }
    let domain = Mux::get()
        .get_domain_by_name(&domain_name)
        .ok_or(DeleteSpaceError::RemoteUnavailable)?;
    let client = domain
        .downcast_ref::<wezterm_client::domain::ClientDomain>()
        .ok_or(DeleteSpaceError::RemoteUnavailable)?;
    // Keep empty mirror windows from being pruned while the final remote
    // panes and tree row are removed. Without this guard the last KillPane
    // notification can detach the domain before DeleteSpace reaches it.
    let _delete_activity = mux::activity::Activity::new();
    if end_remote_sessions {
        let mut panes = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for workspace in &deleted.materialized_workspace_names {
            for window_id in Mux::get().iter_windows_in_workspace(workspace) {
                if let Some(window) = Mux::get().get_window(window_id) {
                    for pane in window.iter().flat_map(|tab| tab.iter_all_panes()) {
                        if pane.domain_id() == domain.domain_id()
                            && pane
                                .downcast_ref::<wezterm_client::pane::ClientPane>()
                                .is_some()
                            && seen.insert(pane.pane_id())
                        {
                            panes.push(pane);
                        }
                    }
                }
            }
        }
        for pane in panes {
            let remote = pane
                .downcast_ref::<wezterm_client::pane::ClientPane>()
                .expect("filtered ClientPane");
            if let Err(err) = remote.kill_remote_and_wait().await {
                log::error!(
                    "failed to end pane {} while deleting Space {space_id} on {domain_name}: {err:#}",
                    pane.pane_id()
                );
                notify_remote_tree_mutation_unavailable(&domain_name);
                return Err(DeleteSpaceError::RemoteUnavailable);
            }
        }
    }

    let tree = match client
        .mutate_thinkterm_tree(vec![codec::TreeOp::DeleteSpace {
            space_id: space_id.to_string(),
        }])
        .await
    {
        Ok(tree) => tree,
        Err(err) => {
            log::error!("failed to delete Space {space_id} on {domain_name}: {err:#}");
            notify_remote_tree_mutation_unavailable(&domain_name);
            return Err(DeleteSpaceError::RemoteUnavailable);
        }
    };
    if tree.space(space_id).is_some() {
        log::error!("{domain_name} rejected deletion of ThinkTerm Space {space_id}");
        return Err(DeleteSpaceError::ServerRejected);
    }

    // `mutate_thinkterm_tree` synchronously delivered the response through
    // ingest_remote_tree, so the shared row is already gone.  Only per-device
    // selection state remains to be committed here.
    if window_was_active {
        let mut store = THREAD_STORE.lock();
        let fallback_space_id = store
            .spaces
            .iter()
            .find(|space| space.is_default && !occupied_by_other.contains(&space.id))
            .or_else(|| {
                store
                    .spaces
                    .iter()
                    .find(|space| !occupied_by_other.contains(&space.id))
            })
            .map(|space| space.id.clone())
            .unwrap_or_else(|| {
                let name = next_space_name(&store.spaces);
                store.create_space_record(name)
            });
        store.last_active_space_id = Some(fallback_space_id.clone());
        persist_locked(&store);
        deleted.fallback_space_id = fallback_space_id.clone();
        WINDOW_SPACES.lock().insert(owner_id, fallback_space_id);
    }

    Ok(deleted)
}

pub fn ensure_active_thread_for_space(space_id: &str) -> Option<WorkspaceThreadId> {
    let mut store = THREAD_STORE.lock();
    let mut changed = store.normalize_after_load();
    if !store.has_space(space_id) {
        return None;
    }
    let project_id = store
        .active_project_id_for_space(space_id)
        .filter(|project_id| {
            store
                .projects
                .iter()
                .any(|project| project.space_id == space_id && &project.id == project_id)
        })
        .or_else(|| {
            store
                .projects
                .iter()
                .find(|project| project.space_id == space_id)
                .map(|project| project.id.clone())
        })
        .unwrap_or_else(|| {
            let mut project = default_project_for_space(space_id);
            let session = WorkspaceThread::new(project.id.clone(), "main".to_string(), None);
            let thread_id = session.id.clone();
            project.active_thread_id = Some(thread_id);
            let project_id = project.id.clone();
            project.threads.push(session);
            store.projects.push(project);
            changed = true;
            project_id
        });
    let thread_id = store.active_thread_for_project(&project_id);
    if changed {
        persist_locked(&store);
    }
    thread_id
}

pub fn thread_to_restore_for_space(space_id: &str) -> Option<WorkspaceThreadId> {
    let mut store = THREAD_STORE.lock();
    let mut changed = store.normalize_after_load();
    let (thread_id, selected_changed) = store.thread_to_restore_for_space(space_id);
    changed |= selected_changed;
    if changed {
        persist_locked(&store);
    }
    thread_id
}

pub fn workspace_has_thread_binding(workspace: &str) -> bool {
    let mut store = THREAD_STORE.lock();
    if store.normalize_after_load() {
        persist_locked(&store);
    }
    store.workspace_space_id(workspace).is_some()
}

pub fn thread_id_for_workspace(space_id: &str, workspace: &str) -> Option<WorkspaceThreadId> {
    let store = THREAD_STORE.lock();
    store.thread_id_for_workspace(space_id, workspace)
}

/// A brand-new / empty Space has no project yet. Give it a stable default
/// project rooted at the user's home directory. Previously this used the
/// process launch cwd (`std::env::current_dir()`), which is arbitrary — `/`
/// when launched from Finder, or whatever directory the binary was started
/// from — and made a new Space appear to "inherit" the previous window's path.
pub fn default_project_for_space(space_id: &str) -> Project {
    let path = config::HOME_DIR.clone();
    let id = project_id_for_path(space_id, &path);
    Project {
        id,
        space_id: space_id.to_string(),
        name: "Home".to_string(),
        path,
        threads: vec![],
        active_thread_id: None,
        threads_collapsed: false,
        active_note_path: None,
    }
}

fn current_project_for_workspace(space_id: &str, active_workspace: &str) -> Project {
    let mut project = default_project_for_space(space_id);
    let session = WorkspaceThread::new_initial(
        project.id.clone(),
        "main".to_string(),
        Some(active_workspace.to_string()),
    );
    project.active_thread_id = Some(session.id.clone());
    project.threads.push(session);
    project
}

pub fn view_for_current_project(
    space_id: &str,
    active_workspace: &str,
    live_workspaces: &[String],
) -> WorkspaceThreadsView {
    let mut store = THREAD_STORE.lock();
    if store.normalize_after_load() {
        persist_locked(&store);
    }
    if let Some(project_id) = store
        .project_id_for_workspace(space_id, active_workspace)
        .or_else(|| store.active_project_id_for_space(space_id))
        .filter(|project_id| {
            store
                .projects
                .iter()
                .any(|project| project.space_id == space_id && &project.id == project_id)
        })
    {
        return store.view_for_project(space_id, &project_id, live_workspaces);
    }

    let is_remote = store.is_client_domain_space(space_id);
    current_project_for_workspace(space_id, active_workspace).view(live_workspaces, is_remote)
}

pub fn sync_current_project(space_id: &str, active_workspace: &str) -> bool {
    let mut store = THREAD_STORE.lock();
    let changed = store.sync_current_project(space_id, active_workspace);
    if changed {
        persist_locked(&store);
        if let Some(thread_id) = store.thread_id_for_workspace(space_id, active_workspace) {
            submit_thread_state(&store, &thread_id);
        }
    }
    changed
}

pub fn create_thread(project_id: &str, name: Option<String>) -> Result<WorkspaceThreadId> {
    let mut store = THREAD_STORE.lock();
    let domain = tree_domain_for_project(&store, project_id);
    ensure!(
        remote_tree_mutation_allowed(domain.as_deref()),
        "remote project is disconnected"
    );
    let thread_id = store.create_thread(project_id, name);
    persist_locked(&store);
    // The id is generated here rather than by the server so that callers can
    // go straight on to activating the thread; the server just adopts it.
    let created = store
        .thread_record(&thread_id)
        .map(|thread| (thread.name.clone(), thread.last_active_at));
    drop(store);
    if let (Some(domain), Some((name, created_at))) = (domain, created) {
        submit_tree_op(
            domain,
            codec::TreeOp::CreateThread {
                thread_id: thread_id.clone(),
                project_id: project_id.to_string(),
                name,
                workspace: None,
                created_at,
            },
        );
    }
    Ok(thread_id)
}

/// Pick the thread a GUI window should fall back to after the mux window
/// showing its active thread died (e.g. `exit` in the thread's only pane):
/// the most recently used OTHER thread of the Space, or a brand-new thread
/// in the active project when no other thread is left. Returns None when
/// the Space (or any project to create a thread in) no longer exists.
pub fn thread_to_recover_after_window_death(space_id: &str) -> Option<WorkspaceThreadId> {
    let mut store = THREAD_STORE.lock();
    store.normalize_after_load();
    if !store.has_space(space_id) {
        return None;
    }
    let active_project_id = store.active_project_id_for_space(space_id);
    let dead_thread_id = active_project_id
        .as_ref()
        .and_then(|project_id| store.projects.iter().find(|p| &p.id == project_id))
        .and_then(|project| project.active_thread_id.clone());

    let mut best: Option<(i64, WorkspaceThreadId)> = None;
    for project in store.projects.iter().filter(|p| p.space_id == space_id) {
        for thread in &project.threads {
            if dead_thread_id.as_deref() == Some(thread.id.as_str()) {
                continue;
            }
            if best
                .as_ref()
                .map_or(true, |(ts, _)| thread.last_active_at > *ts)
            {
                best = Some((thread.last_active_at, thread.id.clone()));
            }
        }
    }
    if let Some((_, thread_id)) = best {
        return Some(thread_id);
    }

    let project_id = active_project_id
        .filter(|project_id| {
            store
                .projects
                .iter()
                .any(|p| p.space_id == space_id && &p.id == project_id)
        })
        .or_else(|| {
            store
                .projects
                .iter()
                .find(|p| p.space_id == space_id)
                .map(|p| p.id.clone())
        })?;
    let thread_id = store.create_thread(&project_id, None);
    persist_locked(&store);
    Some(thread_id)
}

pub fn create_project_from_path(space_id: &str, path: &str) -> Result<WorkspaceThreadId> {
    let path = normalize_project_path(path)?;
    let mut store = THREAD_STORE.lock();
    store.normalize_after_load();
    let thread_id = store.create_project_from_path(space_id, path);
    persist_locked(&store);
    Ok(thread_id)
}

pub fn create_disconnected_remote_host_thread(
    space_id: &str,
    host_id: &str,
    label: &str,
    path: PathBuf,
    workspace_override: Option<String>,
) -> WorkspaceThreadId {
    let mut store = THREAD_STORE.lock();
    store.normalize_after_load();
    let thread_id = store.create_disconnected_remote_host_thread(
        space_id,
        host_id,
        label,
        path,
        workspace_override,
    );
    persist_locked(&store);
    thread_id
}

pub fn activate_thread_record(
    thread_id: &str,
    live_workspaces: &[String],
) -> Option<ActivationPlan> {
    let mut store = THREAD_STORE.lock();
    let plan = store.activate_thread_record(thread_id, live_workspaces);
    schedule_workspace_thread_store_persist();
    if plan.is_some() {
        submit_thread_state(&store, thread_id);
    }
    plan
}

pub fn activation_plan_for_thread(
    thread_id: &str,
    live_workspaces: &[String],
) -> Option<ActivationPlan> {
    let store = THREAD_STORE.lock();
    store.activation_plan_for_thread(thread_id, live_workspaces)
}

pub fn thread_connection_state(
    thread_id: &str,
    live_workspaces: &[String],
) -> Option<ThreadConnectionState> {
    let store = THREAD_STORE.lock();
    store.thread_connection_state(thread_id, live_workspaces)
}

pub fn thread_space_id(thread_id: &str) -> Option<SpaceId> {
    let store = THREAD_STORE.lock();
    store.thread_space_id(thread_id)
}

pub fn project_is_remote(project_id: &str) -> bool {
    let store = THREAD_STORE.lock();
    store
        .projects
        .iter()
        .find(|project| project.id == project_id)
        .is_some_and(|project| is_remote_project(project, &store.spaces))
}

pub fn refresh_thread_work_for_pane(pane_id: PaneId) -> bool {
    let mux = Mux::get();
    let Some((_domain_id, window_id, _tab_id)) = mux.resolve_pane_id(pane_id) else {
        return false;
    };
    let Some(window) = mux.get_window(window_id) else {
        return false;
    };
    let workspace = window.get_workspace().to_string();
    drop(window);
    refresh_thread_work_for_workspace(&workspace)
}

/// The Running signal derives from pane titles / progress escapes that can
/// flicker off for a few frames while the user types, which used to strobe
/// the sidebar spinner (and spuriously latch work_finished_unseen). Hold
/// Running through short Idle observations; a deferred re-check settles the
/// state to Idle once the grace period truly elapses.
const WORK_RUNNING_FALL_DEBOUNCE: Duration = Duration::from_millis(800);
const MIN_FINISHED_SOUND_DURATION: Duration = Duration::from_secs(5);

#[derive(Debug)]
struct WorkRunTiming {
    started_at: std::time::Instant,
    first_idle_at: Option<std::time::Instant>,
}

#[derive(Debug, Default)]
struct WorkSoundTiming {
    runs: HashMap<String, WorkRunTiming>,
}

impl WorkSoundTiming {
    fn observe(
        &mut self,
        workspace: &str,
        observed: WorkspaceThreadWorkStatus,
        now: std::time::Instant,
    ) {
        match observed {
            WorkspaceThreadWorkStatus::Running => {
                let run = self
                    .runs
                    .entry(workspace.to_string())
                    .or_insert(WorkRunTiming {
                        started_at: now,
                        first_idle_at: None,
                    });
                // A brief false Idle is already tolerated by the status
                // debounce. If Running returns, that candidate was not the
                // task's completion and must not determine its duration.
                run.first_idle_at = None;
            }
            WorkspaceThreadWorkStatus::Idle | WorkspaceThreadWorkStatus::FinishedUnseen => {
                if let Some(run) = self.runs.get_mut(workspace) {
                    run.first_idle_at.get_or_insert(now);
                }
            }
            WorkspaceThreadWorkStatus::NeedsAttention => {
                if let Some(run) = self.runs.get_mut(workspace) {
                    run.first_idle_at = None;
                }
            }
        }
    }

    fn take_finished_duration(&mut self, workspace: &str) -> Option<Duration> {
        let run = self.runs.remove(workspace)?;
        let finished_at = run.first_idle_at.unwrap_or_else(std::time::Instant::now);
        Some(finished_at.saturating_duration_since(run.started_at))
    }

    fn forget(&mut self, workspace: &str) {
        self.runs.remove(workspace);
    }
}

fn debounce_work_status(
    workspace: &str,
    observed: WorkspaceThreadWorkStatus,
) -> WorkspaceThreadWorkStatus {
    let mut last_seen = WORK_RUNNING_LAST_SEEN.lock();
    match observed {
        WorkspaceThreadWorkStatus::Running => {
            last_seen.insert(workspace.to_string(), std::time::Instant::now());
            observed
        }
        WorkspaceThreadWorkStatus::Idle => {
            let Some(last) = last_seen.get(workspace) else {
                return observed;
            };
            let elapsed = last.elapsed();
            if elapsed < WORK_RUNNING_FALL_DEBOUNCE {
                schedule_work_status_recheck(
                    workspace.to_string(),
                    WORK_RUNNING_FALL_DEBOUNCE - elapsed,
                );
                WorkspaceThreadWorkStatus::Running
            } else {
                last_seen.remove(workspace);
                observed
            }
        }
        WorkspaceThreadWorkStatus::NeedsAttention | WorkspaceThreadWorkStatus::FinishedUnseen => {
            last_seen.remove(workspace);
            observed
        }
    }
}

fn schedule_work_status_recheck(workspace: String, delay: Duration) {
    if !WORK_STATUS_RECHECK_PENDING.lock().insert(workspace.clone()) {
        return;
    }
    std::thread::spawn(move || {
        std::thread::sleep(delay);
        WORK_STATUS_RECHECK_PENDING.lock().remove(&workspace);
        promise::spawn::spawn_into_main_thread(async move {
            if refresh_thread_work_for_workspace(&workspace) {
                if let Some(front_end) = crate::frontend::try_front_end() {
                    front_end.invalidate_all_windows();
                }
            }
        })
        .detach();
    });
}

/// The bundled prompts. Regenerate them with `assets/sounds/synth.py`; they are
/// synthesised rather than sourced so nothing here is licensed from anyone.
static DONE_WAV: &[u8] = include_bytes!("../../assets/sounds/done.wav");
static NEEDS_INPUT_WAV: &[u8] = include_bytes!("../../assets/sounds/needs-input.wav");

/// Silences every prompt regardless of the setting. Test runners have no
/// business making noise, and neither does a machine someone is presenting on.
const DISABLE_SOUND_ENV: &str = "THINKTERM_DISABLE_SOUND";

fn should_play_work_sound(
    announcement: WorkAnnouncement,
    finished_after: Option<Duration>,
) -> bool {
    match announcement {
        WorkAnnouncement::Finished => finished_after
            .map(|duration| duration >= MIN_FINISHED_SOUND_DURATION)
            .unwrap_or(true),
        WorkAnnouncement::NeedsInput => true,
    }
}

fn announce_work(announcement: WorkAnnouncement, finished_after: Option<Duration>) {
    if std::env::var_os(DISABLE_SOUND_ENV).is_some() {
        return;
    }
    if !crate::native_settings::notification_sounds_enabled() {
        return;
    }
    if !should_play_work_sound(announcement, finished_after) {
        return;
    }
    let wav = match announcement {
        WorkAnnouncement::Finished => DONE_WAV,
        WorkAnnouncement::NeedsInput => NEEDS_INPUT_WAV,
    };
    // The connection is main-thread only. Every caller of
    // `refresh_thread_work_for_workspace` is on it today, and staying silent is
    // the right answer if that ever stops being true.
    use ::window::ConnectionOps;
    match ::window::Connection::get() {
        Some(connection) => connection.play_sound(wav),
        None => log::debug!("no window connection on this thread; notification stays silent"),
    }
}

pub fn refresh_thread_work_for_workspace(workspace: &str) -> bool {
    let raw_observed = scan_workspace_work_status(workspace);
    let observed = debounce_work_status(workspace, raw_observed);
    let change = {
        let mut store = THREAD_STORE.lock();
        let Some(change) = store.observe_thread_work_for_workspace(workspace, observed) else {
            return false;
        };
        change
    };
    let finished_after = {
        let mut timing = WORK_SOUND_TIMING.lock();
        timing.observe(workspace, raw_observed, std::time::Instant::now());
        if change.announce == Some(WorkAnnouncement::Finished) {
            timing.take_finished_duration(workspace)
        } else {
            None
        }
    };
    if let Some(announcement) = change.announce {
        announce_work(announcement, finished_after);
    }
    if change.should_persist {
        schedule_workspace_thread_store_persist();
    }
    if change.changed {
        publish_thinkterm_session_changed();
    }
    change.changed
}

pub fn refresh_all_thread_work() -> bool {
    let workspaces = {
        let store = THREAD_STORE.lock();
        store.thread_workspace_names()
    };
    let mut changed = false;
    for workspace in workspaces {
        changed |= refresh_thread_work_for_workspace(&workspace);
    }
    changed
}

pub fn acknowledge_thread_work_for_workspace(workspace: &str) -> bool {
    WORK_SOUND_TIMING.lock().forget(workspace);
    let mut store = THREAD_STORE.lock();
    let change = store.acknowledge_thread_work_for_workspace(workspace);
    if change.should_persist {
        persist_locked(&store);
    }
    if change.changed && !change.should_persist {
        publish_thinkterm_session_changed();
    }
    change.changed
}

/// Clear the unseen/attention flags of one thread by id (used when a
/// notification entry is activated from the bell menu).
pub fn acknowledge_thread_work_for_thread(thread_id: &str) -> bool {
    let mut store = THREAD_STORE.lock();
    let change = store.acknowledge_thread_work_for_thread(thread_id);
    if change.should_persist {
        persist_locked(&store);
    }
    if change.changed && !change.should_persist {
        publish_thinkterm_session_changed();
    }
    change.changed
}

/// Threads across every Space whose work needs attention or finished without
/// being seen. NeedsAttention entries sort first.
pub fn pending_work_notifications() -> Vec<ThreadWorkNotification> {
    let mut store = THREAD_STORE.lock();
    if store.normalize_after_load() {
        persist_locked(&store);
    }
    store.pending_work_notifications()
}

pub fn pending_work_notification_count() -> usize {
    let store = THREAD_STORE.lock();
    store.pending_work_notification_count()
}

/// Drop threads whose status is hidden by the sidebar view options. The
/// active thread is always kept so the row the user is looking at cannot
/// vanish from under them.
pub fn filter_threads_view_by_status(
    view: &mut WorkspaceThreadsView,
    hidden: &[WorkspaceThreadWorkStatus],
) {
    let keep =
        |thread: &WorkspaceThreadView| thread.is_active || !hidden.contains(&thread.work_status);
    view.pinned_threads.retain(keep);
    for project in &mut view.projects {
        project.threads.retain(keep);
    }
}

/// Guard for the view-options menu: refuse a toggle that would hide every
/// status and leave the sidebar inexplicably empty.
pub fn hidden_statuses_cover_all(hidden: &[WorkspaceThreadWorkStatus]) -> bool {
    WorkspaceThreadWorkStatus::ALL
        .iter()
        .all(|status| hidden.contains(status))
}

pub fn acknowledge_thread_work_for_workspace_deferred(workspace: &str) -> bool {
    WORK_SOUND_TIMING.lock().forget(workspace);
    let mut store = THREAD_STORE.lock();
    let change = store.acknowledge_thread_work_for_workspace(workspace);
    if change.should_persist {
        schedule_workspace_thread_store_persist();
    }
    if change.changed {
        publish_thinkterm_session_changed();
    }
    change.changed
}

fn mark_layout_unreadable(workspace: &str) {
    UNREADABLE_LAYOUT_WORKSPACES
        .lock()
        .insert(workspace.to_string());
}

fn layout_is_unreadable(workspace: &str) -> bool {
    UNREADABLE_LAYOUT_WORKSPACES.lock().contains(workspace)
}

pub fn is_materializing_thread_layout(workspace: &str) -> bool {
    MATERIALIZING_LAYOUT_WORKSPACES
        .lock()
        .get(workspace)
        .copied()
        .unwrap_or(0)
        > 0
}

struct MaterializeThreadLayoutGuard {
    workspace: String,
}

impl MaterializeThreadLayoutGuard {
    fn new(workspace: String) -> Self {
        *MATERIALIZING_LAYOUT_WORKSPACES
            .lock()
            .entry(workspace.clone())
            .or_insert(0) += 1;
        Self { workspace }
    }
}

impl Drop for MaterializeThreadLayoutGuard {
    fn drop(&mut self) {
        let mut workspaces = MATERIALIZING_LAYOUT_WORKSPACES.lock();
        if let Some(depth) = workspaces.get_mut(&self.workspace) {
            *depth = depth.saturating_sub(1);
            if *depth == 0 {
                workspaces.remove(&self.workspace);
            }
        }
    }
}

pub fn snapshot_active_space_thread_layout_with_font_scales<F>(
    space_id: &str,
    workspace: &str,
    window_id: MuxWindowId,
    pane_font_scale: F,
) where
    F: Fn(PaneId) -> Option<f64>,
{
    if is_materializing_thread_layout(workspace) {
        return;
    }

    // The window standing in for a layout we could not read describes one
    // empty pane, and saving that would destroy the arrangement it stands in
    // for. Keep the file as it is; a build that can read it restores it.
    if layout_is_unreadable(workspace) {
        return;
    }

    // Domain-owned windows (remote mux windows, tmux) must never have
    // their LAYOUT snapshotted locally: the remote mux server owns the
    // layout truth, and a local snapshot would fight it on restore. Font
    // scale however is client-side presentation state, so persist that
    // part keyed by the remote pane id: the local pane ids die with the
    // mirror window on switch-away and are reissued by the resync on
    // switch-back.
    //
    // The window's origin tag alone is not enough to recognize one. A thread
    // materialized after the connect gets its mux window from the generic
    // spawn path, which does not tag it, and the tag never appears later
    // because the resync binds the remote window to that same untagged one.
    // Such a window used to fall through to the local branch, which then
    // refused it for being a client-domain Space — so the scale was written
    // nowhere and every switch away lost it. Asking the same question the
    // store asks (`is_client_domain_space`, below in
    // `snapshot_active_space_thread_layout`) keeps the two in step whatever
    // the tag says; the tag is still honoured so tmux keeps its behaviour.
    let space_is_remote = client_domain_for_space(space_id).is_some();
    if space_is_remote
        || Mux::get()
            .get_window(window_id)
            .map_or(false, |w| w.origin_domain().is_some())
    {
        let mut scales: HashMap<PaneId, f64> = HashMap::new();
        let mut remote_panes = 0usize;
        if let Some(window) = Mux::get().get_window(window_id) {
            for tab in window.iter() {
                for pos in tab.iter_panes_ignoring_zoom() {
                    if let Some(client_pane) =
                        pos.pane.downcast_ref::<wezterm_client::pane::ClientPane>()
                    {
                        remote_panes += 1;
                        if let Some(scale) = pane_font_scale(pos.pane.pane_id()) {
                            scales.insert(client_pane.remote_pane_id, scale);
                        }
                    }
                }
            }
        }
        if remote_panes == 0 {
            // The mirror panes may simply not have folded in yet (attach
            // or resync still in flight); saving now would clobber the
            // stored scales with an empty map before restore ever ran.
            return;
        }
        let mut store = THREAD_STORE.lock();
        if store.snapshot_remote_thread_font_scales(space_id, workspace, scales) {
            schedule_workspace_thread_store_persist();
        }
        return;
    }

    let Some(snapshot) = snapshot_window_layout(window_id, &pane_font_scale) else {
        return;
    };
    if snapshot.tabs.is_empty() {
        return;
    }

    let mut store = THREAD_STORE.lock();
    if store.snapshot_active_space_thread_layout(space_id, workspace, snapshot) {
        // Debounced rather than written through. A Space switch calls this,
        // `switch_window_space` and `activate_thread_record` back to back, and
        // each one used to fsync the whole store: three ~88ms stalls on the
        // main thread for three copies of the same file, which is longer than
        // the switch animation they were blocking. The background writer
        // collapses them into one.
        schedule_workspace_thread_store_persist();
    }
}

pub async fn materialize_thread(
    workspace_name: String,
    layout: Option<WorkspaceThreadLayoutSnapshot>,
    initial_cwd: Option<String>,
    size: TerminalSize,
    src_window_id: Option<MuxWindowId>,
    term_config: Arc<dyn TerminalConfiguration>,
    default_domain: SpawnTabDomain,
) -> Result<()> {
    let mux = Mux::get();
    if !mux.iter_windows_in_workspace(&workspace_name).is_empty() {
        return Ok(());
    }

    // A remote mux owns both its ThinkTerm tree and terminal topology.  Use
    // its dedicated, serialized materialization RPC instead of sending an
    // ordinary SpawnV2 from sidebar chrome.  In handoff mode an ordinary
    // spawn is correctly rejected for a non-owner, but that used to leave a
    // cold Thread with no terminal surface on which the user could click to
    // take control.  EnsureThinkTermThread creates only the missing backing
    // terminal and deliberately does not claim the frontend lease; after the
    // resync below, the opaque terminal surface can perform the real claim.
    if let Some((_project_id, thread_id)) = parse_thread_workspace_name(&workspace_name) {
        if let Ok(domain) = mux.resolve_spawn_tab_domain(None, &default_domain) {
            if let Some(client_domain) =
                domain.downcast_ref::<wezterm_client::domain::ClientDomain>()
            {
                let response = client_domain
                    .ensure_thinkterm_thread(Some(thread_id), size)
                    .await
                    .context("ensure remote ThinkTerm thread")?;
                if response.workspace != workspace_name {
                    anyhow::bail!(
                        "remote selected workspace {}, expected {} for thread {}",
                        response.workspace,
                        workspace_name,
                        response.thread_id
                    );
                }
                if mux.iter_windows_in_workspace(&workspace_name).is_empty() {
                    anyhow::bail!(
                        "remote materialized thread {} but resync installed no window in {}",
                        response.thread_id,
                        workspace_name
                    );
                }
                return Ok(());
            }
        }
    }

    // Decoding before the branch, rather than inside `materialize_layout`, is
    // what makes a layout this build cannot read a lost *arrangement* instead
    // of a Thread that will not open: an undecodable snapshot falls through to
    // the plain spawn below, which still lands in the project directory.
    let layout = layout.and_then(|layout| {
        if layout.tabs.is_empty() {
            return None;
        }
        let tabs = decode_layout_tabs(&workspace_name, &layout)?;
        Some((layout, tabs))
    });
    if let Some((layout, tabs)) = layout {
        let _guard = MaterializeThreadLayoutGuard::new(workspace_name.clone());
        materialize_layout(
            mux,
            workspace_name,
            layout,
            tabs,
            initial_cwd,
            size,
            term_config,
        )
        .await
    } else {
        // A thread whose Space lives on a mux server needs its window tagged
        // with that domain. `spawn_tab_or_window` makes untagged windows, and
        // the resync then binds the remote window to this one for good, so a
        // tag missed here never appears later. Everything that asks "which
        // server owns this window" reads that tag: font-scale persistence,
        // re-connect focus, orphan adoption, and the frontend's
        // local-vs-domain-owned split.
        //
        // Only client domains are tagged. Tagging whatever domain the pane
        // happens to come from would mark ordinary local windows as
        // domain-owned, which is a far worse misclassification than the one
        // being fixed.
        let origin = mux
            .resolve_spawn_tab_domain(None, &default_domain)
            .ok()
            .and_then(|domain| {
                domain
                    .downcast_ref::<wezterm_client::domain::ClientDomain>()
                    .map(|_| domain.domain_id())
            });

        // The builder carries an Activity, and that Activity is the only thing
        // stopping a still-empty window from being pruned. Spawning into a
        // remote domain is a round trip, so the builder has to outlive the
        // await — dropping it early can prune the window before its first tab
        // lands. `connect_domain_into_space` holds a `connect_activity` across
        // its own attach+spawn for exactly this reason.
        let tagged_window = origin.map(|domain_id| {
            mux.new_empty_window_for_domain(Some(workspace_name.clone()), None, Some(domain_id))
        });
        let window_id = tagged_window.as_ref().map(|builder| **builder);
        if let (Some(window_id), Some(domain_id)) = (window_id, origin) {
            log::debug!(
                "materialize_thread: window {window_id} for {workspace_name} \
                 tagged with client domain {domain_id}"
            );
        }

        let spawned = mux
            .spawn_tab_or_window(
                window_id,
                default_domain,
                None,
                initial_cwd,
                size,
                None,
                workspace_name,
                None,
            )
            .await;
        drop(tagged_window);

        let (_tab, pane, _window_id) = spawned.context("spawn default thread window")?;
        pane.set_config(term_config);
        let _ = src_window_id;
        Ok(())
    }
}

pub async fn materialize_thread_spawn(
    workspace_name: String,
    spawn: SpawnCommand,
    size: TerminalSize,
    term_config: Arc<dyn TerminalConfiguration>,
) -> Result<()> {
    let mux = Mux::get();
    if !mux.iter_windows_in_workspace(&workspace_name).is_empty() {
        return Ok(());
    }

    let cwd = if let Some(cwd) = spawn.cwd.as_ref() {
        Some(
            cwd.to_str()
                .map(|s| s.to_string())
                .with_context(|| format!("convert cwd {:?} to unicode", cwd))?,
        )
    } else {
        None
    };

    let command = match (
        spawn.args.as_ref(),
        spawn.cwd.as_ref(),
        spawn.set_environment_variables.is_empty(),
    ) {
        (None, None, true) => None,
        _ => {
            let mut builder = spawn
                .args
                .as_ref()
                .map(|args| CommandBuilder::from_argv(args.iter().map(Into::into).collect()))
                .unwrap_or_else(CommandBuilder::new_default_prog);
            for (key, value) in spawn.set_environment_variables.iter() {
                builder.env(key, value);
            }
            if let Some(cwd) = spawn.cwd.as_ref() {
                builder.cwd(cwd);
            }
            Some(builder)
        }
    };

    let (_tab, pane, _window_id) = mux
        .spawn_tab_or_window(
            None,
            spawn.domain,
            command,
            cwd,
            size,
            None,
            workspace_name,
            spawn.position,
        )
        .await
        .context("spawn command in thread workspace")?;
    pane.set_config(term_config);
    Ok(())
}

pub fn thread_layout(thread_id: &str) -> Option<WorkspaceThreadLayoutSnapshot> {
    let store = THREAD_STORE.lock();
    store
        .projects
        .iter()
        .flat_map(|project| project.threads.iter())
        .find(|session| session.id == thread_id)
        .and_then(|session| session.layout.clone())
}

pub fn thread_name(thread_id: &str) -> Option<String> {
    let store = THREAD_STORE.lock();
    store
        .projects
        .iter()
        .flat_map(|project| project.threads.iter())
        .find(|session| session.id == thread_id)
        .map(|session| session.name.clone())
}

pub fn thread_is_pinned(thread_id: &str) -> bool {
    let store = THREAD_STORE.lock();
    store
        .projects
        .iter()
        .flat_map(|project| project.threads.iter())
        .find(|session| session.id == thread_id)
        .is_some_and(|session| session.is_pinned)
}

pub fn project_name(project_id: &str) -> Option<String> {
    let store = THREAD_STORE.lock();
    store
        .projects
        .iter()
        .find(|project| project.id == project_id)
        .map(|project| project.name.clone())
}

pub fn project_id_for_thread(thread_id: &str) -> Option<ProjectId> {
    let store = THREAD_STORE.lock();
    store
        .projects
        .iter()
        .find(|project| {
            project
                .threads
                .iter()
                .any(|session| session.id == thread_id)
        })
        .map(|project| project.id.clone())
}

/// A Space's project ids in render order — the full logical list, not just
/// what fits in the sidebar's viewport.
pub fn ordered_project_ids(space_id: &str) -> Vec<ProjectId> {
    let store = THREAD_STORE.lock();
    store
        .projects
        .iter()
        .filter(|project| project.space_id == space_id)
        .map(|project| project.id.clone())
        .collect()
}

/// Every thread in a project in its persisted render order. Unlike
/// [`unpinned_thread_ids`], this includes pinned threads because global views
/// such as Live Overview group by Space/Project rather than duplicating the
/// sidebar's pinned section.
pub fn ordered_thread_ids(project_id: &str) -> Vec<WorkspaceThreadId> {
    let store = THREAD_STORE.lock();
    store
        .projects
        .iter()
        .find(|project| project.id == project_id)
        .map(|project| {
            project
                .threads
                .iter()
                .map(|session| session.id.clone())
                .collect()
        })
        .unwrap_or_default()
}

/// The thread ids a project shows under its own row, in render order —
/// pinned threads live in the sidebar's separate top section.
pub fn unpinned_thread_ids(project_id: &str) -> Vec<WorkspaceThreadId> {
    let store = THREAD_STORE.lock();
    store
        .projects
        .iter()
        .find(|project| project.id == project_id)
        .map(|project| {
            project
                .threads
                .iter()
                .filter(|session| !session.is_pinned)
                .map(|session| session.id.clone())
                .collect()
        })
        .unwrap_or_default()
}

pub fn active_project_id_for_space(space_id: &str) -> Option<ProjectId> {
    let mut store = THREAD_STORE.lock();
    if store.normalize_after_load() {
        persist_locked(&store);
    }
    store.active_project_id_for_space(space_id)
}

pub fn project_active_note_path(project_id: &str) -> Option<String> {
    let store = THREAD_STORE.lock();
    store
        .projects
        .iter()
        .find(|project| project.id == project_id)
        .and_then(|project| project.active_note_path.clone())
}

/// Remember a Project's active note independently from its threads. Paths are
/// persisted relative to the Space Vault so moving a Vault does not invalidate
/// every Project selection.
pub fn set_project_active_note_path(project_id: &str, path: Option<&str>) -> Result<bool> {
    let normalized = path.map(normalize_vault_markdown_path).transpose()?;
    let mut store = THREAD_STORE.lock();
    let project = store
        .projects
        .iter_mut()
        .find(|project| project.id == project_id)
        .with_context(|| format!("unknown Project {project_id}"))?;
    if project.active_note_path == normalized {
        return Ok(false);
    }
    project.active_note_path = normalized;
    // Note selection is presentation state and can change several times while
    // the user moves through a Vault.  Coalesce those writes off the UI thread
    // while keeping the in-memory value immediately authoritative.
    schedule_workspace_thread_store_persist();
    Ok(true)
}

pub fn normalize_vault_markdown_path(path: &str) -> Result<String> {
    use std::path::Component;

    let trimmed = path.trim();
    ensure!(!trimmed.is_empty(), "note path is empty");
    let path = Path::new(trimmed);
    ensure!(
        path.extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| extension.eq_ignore_ascii_case("md")),
        "note path must end in .md: {trimmed}"
    );
    let mut components = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => components.push(part.to_string_lossy().into_owned()),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                anyhow::bail!("note path must remain inside the Vault: {trimmed}")
            }
        }
    }
    ensure!(!components.is_empty(), "note path is empty");
    Ok(components.join("/"))
}

pub fn project_reveal_path(project_id: &str) -> Option<PathBuf> {
    let store = THREAD_STORE.lock();
    store.project_reveal_path(project_id)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RemoteFilesSource {
    SshHost(String),
    ClientDomain(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteFilesTarget {
    pub project_id: String,
    pub project_name: String,
    pub source: RemoteFilesSource,
    /// Remote spelling as stored by the workspace model.  This is deliberately
    /// a String rather than PathBuf: joining is performed by the slash-only
    /// RemotePath type in the SFTP layer.
    pub requested_root: String,
}

/// Resolve the remote file source and root for the active project without
/// consulting the local filesystem.
pub fn remote_files_target(space_id: &str, project_id: &str) -> Option<RemoteFilesTarget> {
    let store = THREAD_STORE.lock();
    store.remote_files_target(space_id, project_id)
}

impl WorkspaceThreadStore {
    fn remote_files_target(&self, space_id: &str, project_id: &str) -> Option<RemoteFilesTarget> {
        let project = self
            .projects
            .iter()
            .find(|project| project.space_id == space_id && project.id == project_id)?;
        if !is_remote_project(project, &self.spaces) {
            return None;
        }

        let source = if let Some(domain) = self
            .spaces
            .iter()
            .find(|space| space.id == space_id)
            .and_then(|space| space.client_domain.clone())
        {
            RemoteFilesSource::ClientDomain(domain)
        } else {
            RemoteFilesSource::SshHost(remote_host_id_for_project_id(&project.id).to_string())
        };
        let stored = project.path.to_string_lossy();
        let requested_root = if stored.starts_with("wezterm-mux://") || stored.starts_with("ssh://")
        {
            "~".to_string()
        } else {
            stored.into_owned()
        };

        Some(RemoteFilesTarget {
            project_id: project.id.clone(),
            project_name: project.name.clone(),
            source,
            requested_root,
        })
    }
}

pub fn workspace_pane_font_scales(
    workspace: &str,
    window_id: MuxWindowId,
) -> Option<HashMap<PaneId, Option<f64>>> {
    let (local, remote) = {
        let store = THREAD_STORE.lock();
        (
            store.workspace_pane_font_scales(workspace, window_id),
            store.remote_thread_font_scales(workspace),
        )
    };
    log::debug!(
        "workspace_pane_font_scales: ws={workspace} window={window_id} local={:?} remote={:?}",
        local,
        remote
    );
    if local.is_some() {
        return local;
    }
    // Mux-domain threads store scales keyed by remote pane id; translate
    // through the live panes of the window (their local ids are reissued
    // by every re-fold, the remote ids are stable).
    let remote = remote?;
    let mux = Mux::get();
    let window = mux.get_window(window_id)?;
    let mut scales: HashMap<PaneId, Option<f64>> = HashMap::new();
    for tab in window.iter() {
        for pos in tab.iter_panes_ignoring_zoom() {
            if let Some(client_pane) = pos.pane.downcast_ref::<wezterm_client::pane::ClientPane>() {
                scales.insert(
                    pos.pane.pane_id(),
                    remote.get(&client_pane.remote_pane_id).copied(),
                );
            }
        }
    }
    if scales.is_empty() {
        None
    } else {
        Some(scales)
    }
}

pub fn rename_project(project_id: &str, name: String) -> bool {
    let mut store = THREAD_STORE.lock();
    let domain = tree_domain_for_project(&store, project_id);
    if !remote_tree_mutation_allowed(domain.as_deref()) {
        return false;
    }
    let changed = store.rename_project(project_id, name.clone());
    if changed {
        if let Some(domain) = domain {
            submit_tree_op(
                domain,
                codec::TreeOp::RenameProject {
                    project_id: project_id.to_string(),
                    name,
                },
            );
        }
        persist_locked(&store);
    }
    changed
}

pub fn remove_project(project_id: &str) -> Option<RemovedProject> {
    let mut store = THREAD_STORE.lock();
    // Resolve the owner before the record is gone.
    let domain = tree_domain_for_project(&store, project_id);
    if !remote_tree_mutation_allowed(domain.as_deref()) {
        return None;
    }
    let removed = store.remove_project(project_id);
    if removed.is_some() {
        if let Some(domain) = domain {
            submit_tree_op(
                domain,
                codec::TreeOp::RemoveProject {
                    project_id: project_id.to_string(),
                },
            );
        }
        persist_locked(&store);
    }
    removed
}

pub fn rename_thread(thread_id: &str, name: String) -> bool {
    let mut store = THREAD_STORE.lock();
    let domain = tree_domain_for_thread(&store, thread_id);
    if !remote_tree_mutation_allowed(domain.as_deref()) {
        return false;
    }
    let requested_name = name.clone();
    let changed = store.rename_thread(thread_id, name);
    if changed {
        // The local store predicts a unique name for immediate feedback, but
        // the op carries the user's request. The server re-settles it against
        // the current authoritative project and its response wins.
        if let (Some(domain), Some(thread)) = (domain, store.thread_record(thread_id)) {
            submit_tree_op(
                domain,
                codec::TreeOp::RenameThread {
                    thread_id: thread_id.to_string(),
                    name: requested_name,
                    last_active_at: thread.last_active_at,
                },
            );
        }
        persist_locked(&store);
    }
    changed
}

pub fn toggle_thread_pinned(thread_id: &str) -> bool {
    let mut store = THREAD_STORE.lock();
    let domain = tree_domain_for_thread(&store, thread_id);
    if !remote_tree_mutation_allowed(domain.as_deref()) {
        return false;
    }
    let changed = store.toggle_thread_pinned(thread_id);
    if changed {
        if let (Some(domain), Some(thread)) = (domain, store.thread_record(thread_id)) {
            submit_tree_op(
                domain,
                codec::TreeOp::SetThreadPinned {
                    thread_id: thread_id.to_string(),
                    pinned: thread.is_pinned,
                    last_active_at: thread.last_active_at,
                },
            );
        }
        persist_locked(&store);
    }
    changed
}

pub fn mark_thread_unread(thread_id: &str) -> bool {
    let mut store = THREAD_STORE.lock();
    let domain = tree_domain_for_thread(&store, thread_id);
    if !remote_tree_mutation_allowed(domain.as_deref()) {
        return false;
    }
    let changed = store.mark_thread_unread(thread_id);
    if changed {
        if let Some(domain) = domain {
            submit_tree_op(
                domain,
                codec::TreeOp::SetThreadUnread {
                    thread_id: thread_id.to_string(),
                    unread: true,
                },
            );
        }
        persist_locked(&store);
    }
    changed
}

#[allow(dead_code)]
pub fn delete_thread(thread_id: &str) -> Option<DeletedWorkspaceThread> {
    let mut store = THREAD_STORE.lock();
    let domain = tree_domain_for_thread(&store, thread_id);
    if !remote_tree_mutation_allowed(domain.as_deref()) {
        return None;
    }
    let deleted = store.delete_thread(thread_id);
    if deleted.is_some() {
        if let Some(domain) = domain {
            submit_tree_op(
                domain,
                codec::TreeOp::DeleteThread {
                    thread_id: thread_id.to_string(),
                },
            );
        }
        persist_locked(&store);
    }
    deleted
}

pub fn end_workspace_thread_record(thread_id: &str) -> EndWorkspaceThreadResult {
    let mut store = THREAD_STORE.lock();
    let domain = tree_domain_for_thread(&store, thread_id);
    if !remote_tree_mutation_allowed(domain.as_deref()) {
        return EndWorkspaceThreadResult::Noop;
    }
    let result = store.end_workspace_thread_record(thread_id);
    if !matches!(result, EndWorkspaceThreadResult::Noop) {
        persist_locked(&store);
    }
    drop(store);
    // Ending the last thread of a Space's last project also creates a
    // replacement project and removes the old one; restating the subtree is
    // more reliable than tracking that sequence op by op.
    if let Some(domain) = domain {
        if !matches!(result, EndWorkspaceThreadResult::Noop) {
            reconcile_remote_subtree(&domain);
        }
    }
    result
}

pub fn disconnect_workspace_thread_record(
    thread_id: &str,
    live_workspaces: &[String],
) -> Option<DisconnectedWorkspaceThread> {
    let mut store = THREAD_STORE.lock();
    let domain = tree_domain_for_thread(&store, thread_id);
    if !remote_tree_mutation_allowed(domain.as_deref()) {
        return None;
    }
    let (disconnected, changed) =
        store.disconnect_workspace_thread_record(thread_id, live_workspaces);
    if changed {
        persist_locked(&store);
    }
    drop(store);
    // Disconnecting a Space's active thread can also mint a replacement
    // project and its first thread to land on. Restating the subtree covers
    // that, where a targeted op for the thread being disconnected would leave
    // the new rows unknown to the server — and the next push would delete them
    // out from under the activation that is about to use them.
    if changed {
        if let Some(domain) = domain {
            reconcile_remote_subtree(&domain);
        }
    }
    disconnected
}

/// Route `op` to the mux server that owns `domain_name`.
///
/// The caller has already applied the equivalent mutation to the local store,
/// so the UI is already correct; this tells the server (and through it every
/// other attached device). The server's reply and broadcast both come back
/// through `ingest_remote_tree`, which is what corrects us if the server saw
/// things differently — a racing delete from another device, say.
fn submit_tree_op(domain_name: String, op: codec::TreeOp) {
    submit_tree_ops(domain_name, vec![op]);
}

fn submit_tree_ops(domain_name: String, ops: Vec<codec::TreeOp>) {
    if ops.is_empty() {
        return;
    }
    if !remote_tree_domain_is_attached(&domain_name) {
        // This is the transport layer, not necessarily a user action.  Space
        // disconnect, window re-home and reconnect bookkeeping can all leave
        // a harmless late state-sync here after the domain has deliberately
        // detached.  Roll it back to the server baseline without telling the
        // user to undo the disconnect they just requested.  Explicit editing
        // commands use `remote_tree_mutation_allowed` before changing the
        // store, and that higher layer owns the user-facing notification.
        log::warn!(
            "dropping {} late ThinkTerm tree ops for {domain_name}: domain is unavailable",
            ops.len()
        );
        schedule_restore_last_authoritative_tree(domain_name);
        return;
    }
    queue_in_flight_tree_ops(&domain_name, &ops);
    send_tree_ops(domain_name, ops);
}

/// Put an already-approved online mutation on the wire.  A failure retires the
/// presentation overlay instead of turning it into an offline command: the
/// next authoritative tree is allowed to restore what the server actually has.
fn send_tree_ops(domain_name: String, ops: Vec<codec::TreeOp>) {
    promise::spawn::spawn_into_main_thread(async move {
        let Some(domain) = Mux::get().get_domain_by_name(&domain_name) else {
            retire_sent_tree_ops(&domain_name, &ops);
            log::warn!("cannot mutate ThinkTerm tree for {domain_name}: domain disappeared");
            restore_last_authoritative_tree(&domain_name);
            return;
        };
        let Some(client) = domain.downcast_ref::<wezterm_client::domain::ClientDomain>() else {
            log::warn!("{domain_name} is not a mux client domain; dropping ThinkTerm tree ops");
            retire_sent_tree_ops(&domain_name, &ops);
            restore_last_authoritative_tree(&domain_name);
            return;
        };
        if !remote_tree_domain_is_attached(&domain_name) {
            retire_sent_tree_ops(&domain_name, &ops);
            log::warn!("cannot mutate ThinkTerm tree for {domain_name}: domain detached");
            restore_last_authoritative_tree(&domain_name);
            return;
        }
        match client.mutate_thinkterm_tree(ops.clone()).await {
            Ok(tree) => {
                // The domain delivers the reply to the tree sink before this
                // future resumes.  At that point this batch is deliberately
                // still part of the presentation overlay, so a server-side
                // canonicalization (for example de-duplicating a Thread
                // name) can be hidden by the client's requested value.  Drop
                // the acknowledged intent and ingest the same reply once
                // more: the server's result is the final word, while any
                // genuinely later batch remains overlaid until its own ack.
                retire_sent_tree_ops(&domain_name, &ops);
                ingest_remote_tree(&domain_name, tree);
            }
            Err(err) => {
                retire_sent_tree_ops(&domain_name, &ops);
                log::error!("failed to send ThinkTerm tree ops to {domain_name}: {err:#}");
                restore_last_authoritative_tree(&domain_name);
            }
        }
    })
    .detach();
}

fn restore_last_authoritative_tree(domain_name: &str) {
    let tree = LAST_KNOWN_REMOTE_TREES.lock().get(domain_name).cloned();
    if let Some(tree) = tree {
        ingest_remote_tree(domain_name, tree);
    }
}

fn schedule_restore_last_authoritative_tree(domain_name: String) {
    promise::spawn::spawn_into_main_thread(async move {
        restore_last_authoritative_tree(&domain_name);
    })
    .detach();
}

/// Cap connection-scoped in-flight presentation overlays. This is not an
/// offline queue and is cleared whenever the connection generation changes.
const MAX_IN_FLIGHT_TREE_OPS: usize = 1024;

/// Remember `ops` only until this connection's response shows the server has
/// them or the connection is replaced.
fn queue_in_flight_tree_ops(domain_name: &str, ops: &[codec::TreeOp]) {
    let mut in_flight = IN_FLIGHT_TREE_OPS.lock();
    let queue = in_flight.entry(domain_name.to_string()).or_default();
    if queue.len() >= MAX_IN_FLIGHT_TREE_OPS {
        log::warn!(
            "dropping {} ThinkTerm tree ops for {domain_name}: {} already waiting to be acked",
            ops.len(),
            queue.len()
        );
        return;
    }
    queue.extend(ops.iter().cloned());
}

fn take_in_flight_tree_ops(domain_name: &str) -> Vec<codec::TreeOp> {
    IN_FLIGHT_TREE_OPS
        .lock()
        .remove(domain_name)
        .unwrap_or_default()
}

/// Put back the ops an arriving tree did not account for, bypassing the cap:
/// this is the same set coming round again, not new growth.
fn restore_in_flight_tree_ops(domain_name: &str, ops: Vec<codec::TreeOp>) {
    IN_FLIGHT_TREE_OPS
        .lock()
        .insert(domain_name.to_string(), ops);
}

/// Drop the ops the server has just told us it processed.
///
/// One occurrence each, by value: a batch that creates a row and deletes it
/// again is settled the moment the server has run both, and re-deriving that
/// from the resulting tree is impossible — the net effect is invisible there.
fn retire_sent_tree_ops(domain_name: &str, sent: &[codec::TreeOp]) {
    let mut in_flight = IN_FLIGHT_TREE_OPS.lock();
    let Some(queue) = in_flight.get_mut(domain_name) else {
        return;
    };
    for op in sent {
        if let Some(index) = queue.iter().position(|queued| queued == op) {
            queue.remove(index);
        }
    }
    if queue.is_empty() {
        in_flight.remove(domain_name);
    }
}

/// Drop `space_id` from this device's view of `domain_name` without touching
/// the server's copy.
///
/// It is removed from the last-known tree as well, so the reconcile that
/// follows neither deletes it on the server nor offers to recreate it: as far
/// as diffing is concerned this device has simply never heard of it.
fn hide_space_locally(domain_name: &str, space_id: &str) {
    LOCALLY_HIDDEN_SPACES
        .lock()
        .entry(domain_name.to_string())
        .or_default()
        .insert(space_id.to_string());
    if let Some(tree) = LAST_KNOWN_REMOTE_TREES.lock().get_mut(domain_name) {
        let mut just_this_one = std::collections::HashSet::new();
        just_this_one.insert(space_id.to_string());
        strip_hidden_spaces(tree, &just_this_one);
    }
}

fn strip_hidden_spaces(
    tree: &mut codec::ThinkTermTree,
    hidden: &std::collections::HashSet<SpaceId>,
) {
    if hidden.is_empty() {
        return;
    }
    tree.spaces.retain(|space| !hidden.contains(&space.id));
    tree.projects
        .retain(|project| !hidden.contains(&project.space_id));
}

/// The mux domain owning `space_id`, when it is a remote Space.
fn tree_domain_for_space(store: &WorkspaceThreadStore, space_id: &str) -> Option<String> {
    store
        .spaces
        .iter()
        .find(|space| space.id == space_id)
        .and_then(|space| space.client_domain.clone())
}

fn tree_domain_for_project(store: &WorkspaceThreadStore, project_id: &str) -> Option<String> {
    let space_id = store
        .projects
        .iter()
        .find(|project| project.id == project_id)
        .map(|project| project.space_id.clone())?;
    tree_domain_for_space(store, &space_id)
}

fn tree_domain_for_thread(store: &WorkspaceThreadStore, thread_id: &str) -> Option<String> {
    let project_id = store
        .projects
        .iter()
        .find(|project| project.threads.iter().any(|thread| thread.id == thread_id))
        .map(|project| project.id.clone())?;
    tree_domain_for_project(store, &project_id)
}

fn remote_tree_domain_is_attached(domain_name: &str) -> bool {
    Mux::get()
        .get_domain_by_name(domain_name)
        .is_some_and(|domain| {
            let Some(client) = domain.downcast_ref::<wezterm_client::domain::ClientDomain>() else {
                return false;
            };
            remote_tree_connection_can_mutate(
                domain.state(),
                client.is_attaching(),
                client.is_reconnecting(),
                client.is_reconnect_suspended(),
            )
        })
}

fn remote_tree_connection_can_mutate(
    state: mux::domain::DomainState,
    attaching: bool,
    reconnecting: bool,
    reconnect_suspended: bool,
) -> bool {
    state == mux::domain::DomainState::Attached
        && !attaching
        && !reconnecting
        && !reconnect_suspended
}

/// Report an unavailable server for an explicit user edit. Background state
/// synchronization must stay silent and only restore the authoritative tree.
fn notify_remote_tree_mutation_unavailable(domain_name: &str) {
    log::warn!(
        "ThinkTerm tree mutation for {domain_name} was rejected: reconnect to the server first"
    );
    if crate::frontend::try_front_end().is_some() {
        let mut args = FluentArgs::new();
        args.set("name", domain_name.to_string());
        wezterm_toast_notification::persistent_toast_notification(
            "ThinkTerm",
            &crate::i18n::tr_args("remote-tree-mutation-offline", &args),
        );
    }
}

/// Shared remote rows are not an offline-editable cache.  Call this before
/// changing the store; a missing domain means the row is local and remains
/// immediately editable.
fn remote_tree_mutation_allowed(domain_name: Option<&str>) -> bool {
    let Some(domain_name) = domain_name else {
        return true;
    };
    if remote_tree_domain_is_attached(domain_name) {
        true
    } else {
        notify_remote_tree_mutation_unavailable(domain_name);
        false
    }
}

/// Adopt a mux server's sidebar tree.
///
/// The server owns which Spaces/Projects/Threads exist, their names, order and
/// pins; this device owns where it is looking and how it is displayed. So the
/// remote records are rebuilt from the tree while every per-device field is
/// carried across by id.
pub fn ingest_remote_tree(domain_name: &str, tree: codec::ThinkTermTree) {
    let mut tree = tree;
    if let Some(hidden) = LOCALLY_HIDDEN_SPACES.lock().get(domain_name) {
        strip_hidden_spaces(&mut tree, hidden);
    }

    // Before anything else, and before the emptiness below is given any
    // meaning: a tree older than the one we hold is stale news, whichever
    // route it took. Trees arrive both as RPC answers and as broadcasts, on
    // schedules that can swap them over, and a reply cut before a push must
    // not undo it — least of all by looking "uninitialized" and sending us
    // back round the seeding path.
    let known_revision = LAST_KNOWN_REMOTE_TREES
        .lock()
        .get(domain_name)
        .map(|known| known.revision);
    if tree_is_stale(tree.revision, known_revision) {
        log::debug!(
            "ignoring ThinkTerm tree from {domain_name}: revision {} is older than \
             the {known_revision:?} we hold",
            tree.revision
        );
        return;
    }

    // The first tree of a connection is the only place allowed to invent a
    // Space when the server has none.  Online mutations from the previous
    // connection are deliberately not carried across this boundary.
    let first_of_connection = FIRST_TREE_PENDING.lock().remove(domain_name);

    // A server nobody has ever written to answers with an empty tree, and that
    // emptiness says "unset", not "the user deleted everything" — seed it from
    // what we hold. Once it has a revision an empty tree is the truth, and a
    // device with a stale cache must not resurrect the deleted rows.
    if tree.is_uninitialized() {
        // An uninitialized server is the one explicit bootstrap exception:
        // it has no authoritative rows yet, so the cached subtree may seed it.
        take_in_flight_tree_ops(domain_name);
        LAST_KNOWN_REMOTE_TREES
            .lock()
            .insert(domain_name.to_string(), tree);
        reconcile_remote_subtree(domain_name);
        return;
    }

    // A push can overtake an online RPC. Fold only this connection's in-flight
    // presentation overlay into that push so the row does not flicker while
    // the request is on the wire.  These ops are never persisted separately,
    // never accepted while offline and never re-sent after reconnect.
    //
    // An op that no longer changes the server's tree is one the server has
    // accounted for, so it retires here; the rest go back on the queue. The
    // last-known tree deliberately stays the server's own copy: until it says
    // otherwise these ops are still a local difference.
    //
    let mut unacked = take_in_flight_tree_ops(domain_name);
    let mut patched = tree.clone();
    unacked.retain(|op| codec::apply_op(&mut patched, op));
    if !unacked.is_empty() {
        restore_in_flight_tree_ops(domain_name, unacked.clone());
    }
    let tree_for_store = patched;

    let mut store = THREAD_STORE.lock();
    // Which Spaces belonged to this server *before* it spoke. Only a window
    // parked on one of these can have been orphaned by this push, and the set
    // has to be taken now because the ingest below is what removes them.
    let owned_before = store.space_ids_for_domain(domain_name);
    let mut changed = store.ingest_remote_tree(domain_name, &tree_for_store);
    // A server can legitimately have no Spaces left — its last one was deleted
    // from here or from another device. A device that is connecting to it
    // right now still needs somewhere on *that server* to land, so build one
    // before the re-home below has to reach for a fallback and drop the window
    // onto an unrelated local Space. Only on the connection's first tree: a
    // broadcast reaching several attached devices at once would otherwise have
    // every one of them invent a Space.
    let minted_space =
        if first_of_connection && store.preferred_space_for_domain(domain_name).is_none() {
            let plan = store.ensure_mux_domain_space(domain_name);
            log::info!(
                "{domain_name} has no Spaces left; created {}",
                plan.space_id
            );
            changed = true;
            true
        } else {
            false
        };
    if changed {
        persist_locked(&store);
    }
    // A window can be left sitting on a Space the server does not have: the
    // connect flow has to pick a Space before it can authenticate, so a device
    // connecting for the first time invents one and only learns the real ones
    // here.
    let rehome = orphaned_window_spaces(&store, domain_name, &owned_before);
    drop(store);

    LAST_KNOWN_REMOTE_TREES
        .lock()
        .insert(domain_name.to_string(), tree);

    if minted_space {
        // The replacement Space exists only here so far; the server is the one
        // place it has to reach for the other devices to see it too.
        reconcile_remote_subtree(domain_name);
    }

    if changed {
        if let Some(front_end) = crate::frontend::try_front_end() {
            front_end.invalidate_all_windows();
        }
    }
    if !rehome.is_empty() {
        rehome_orphaned_windows(rehome);
    }
}

/// A fresh connection to `domain_name`, announced before anything is asked of
/// it.
///
/// Everything this device believes about that server's tree describes the
/// connection that just ended, so it is dropped here — once — and whatever
/// arrives next sets the baseline afresh. That is what lets a server which
/// lost or rolled back its own copy be re-adopted instead of having every one
/// of its trees dismissed as out of date, without exempting a whole class of
/// arrival from the ordering check.
///
/// Reconnecting is also when Spaces this device disconnected from come back,
/// which is exactly what the menu promises.
pub fn note_remote_connected(domain_name: &str) {
    LAST_KNOWN_REMOTE_TREES.lock().remove(domain_name);
    IN_FLIGHT_TREE_OPS.lock().remove(domain_name);
    LOCALLY_HIDDEN_SPACES.lock().remove(domain_name);
    FIRST_TREE_PENDING.lock().insert(domain_name.to_string());
}

/// Whether an arriving tree is older news than what we already hold.
///
/// Trees arrive by two routes — the answer to an RPC and the server's own
/// broadcast — that are resumed on different schedules, so a reply cut before
/// a push can still be handed over after it. Adopting it would walk the
/// sidebar backwards. Revisions only climb within one connection, so a lower
/// one is stale by definition; a server that restarted lower announces itself
/// through `note_remote_connected`, which clears the baseline rather than
/// asking this to make an exception.
fn tree_is_stale(arriving: u64, known: Option<u64>) -> bool {
    known.map_or(false, |known| arriving < known)
}

/// Windows whose recorded Space vanished, paired with where they should go.
///
/// `owned_before` is the set of Spaces this server had immediately before the
/// arriving tree was applied, and a window only qualifies if it was sitting on
/// one of them. "Any Space the store no longer has" looks equivalent but is
/// not: a re-home is a notification the destination window has yet to act on,
/// so a window orphaned by server A is still on A's dead Space when B's next
/// push lands, and B would drag it onto one of *its* Spaces.
fn orphaned_window_spaces(
    store: &WorkspaceThreadStore,
    domain_name: &str,
    owned_before: &std::collections::HashSet<SpaceId>,
) -> Vec<(u64, SpaceId)> {
    let fallback = store
        .preferred_space_for_domain(domain_name)
        // The server can end up with no Spaces at all — the last one was
        // deleted on another device. A window parked on one of them still has
        // to land somewhere that exists, so fall back to a local Space rather
        // than leaving it pointing at a row nothing can draw.
        .or_else(|| {
            store
                .spaces
                .iter()
                .find(|space| space.is_default)
                .or_else(|| store.spaces.first())
                .map(|space| space.id.clone())
        });
    let Some(fallback) = fallback else {
        return vec![];
    };
    WINDOW_SPACES
        .lock()
        .iter()
        .filter(|(_, space_id)| owned_before.contains(*space_id) && !store.has_space(space_id))
        .map(|(owner_id, _)| (*owner_id, fallback.clone()))
        .collect()
}

/// Move each orphaned window onto a Space that exists.
///
/// `switch_space` is the sanctioned path — it snapshots the outgoing layout,
/// updates the window's own notion of its Space and activates a thread — so
/// the windows are asked to do it themselves rather than having the map
/// rewritten underneath them.
fn rehome_orphaned_windows(rehome: Vec<(u64, SpaceId)>) {
    let Some(front_end) = crate::frontend::try_front_end() else {
        return;
    };
    use ::window::WindowOps;
    for gui_window in front_end.gui_windows() {
        let rehome = rehome.clone();
        gui_window
            .window
            .notify(crate::termwindow::TermWindowNotif::Apply(Box::new(
                move |term_window| {
                    term_window.rehome_if_space_vanished(&rehome);
                },
            )));
    }
}

/// Bring the server's copy of `domain_name`'s subtree in line with ours.
///
/// Called from the paths that build or tear down rows in more than one step,
/// where restating the result is more reliable than enumerating the edits.
/// See `WorkspaceThreadStore::reconcile_ops_for_domain`.
pub fn reconcile_remote_subtree(domain_name: &str) {
    let last_known = LAST_KNOWN_REMOTE_TREES
        .lock()
        .get(domain_name)
        .cloned()
        .unwrap_or_default();
    let ops = {
        let store = THREAD_STORE.lock();
        store.reconcile_ops_for_domain(domain_name, &last_known)
    };
    submit_tree_ops(domain_name.to_string(), ops);
}

/// Push a thread's shared scalar state to its server.
///
/// Activating a thread binds it to a mux workspace, stamps its recency and
/// clears its unread mark all at once; sending the resulting values together
/// keeps the two copies identical without one op per field at each call site.
/// A no-op for local threads.
fn submit_thread_state(store: &WorkspaceThreadStore, thread_id: &str) {
    let Some(domain) = tree_domain_for_thread(store, thread_id) else {
        return;
    };
    let Some(thread) = store.thread_record(thread_id) else {
        return;
    };
    submit_tree_ops(
        domain,
        vec![
            codec::TreeOp::SetThreadWorkspaceName {
                thread_id: thread_id.to_string(),
                planned: thread.planned_workspace_name.clone(),
                materialized: thread.materialized_workspace_name.clone(),
            },
            codec::TreeOp::TouchThread {
                thread_id: thread_id.to_string(),
                at: thread.last_active_at,
            },
            codec::TreeOp::SetThreadUnread {
                thread_id: thread_id.to_string(),
                unread: thread.is_unread,
            },
        ],
    );
}

pub fn move_project_before(space_id: &str, project_id: &str, before: Option<&str>) -> bool {
    let mut store = THREAD_STORE.lock();
    let domain = tree_domain_for_space(&store, space_id);
    if !remote_tree_mutation_allowed(domain.as_deref()) {
        return false;
    }
    let changed = store.move_project_before(space_id, project_id, before);
    if changed {
        if let Some(domain) = domain {
            submit_tree_op(
                domain,
                codec::TreeOp::MoveProjectBefore {
                    space_id: space_id.to_string(),
                    project_id: project_id.to_string(),
                    before: before.map(|id| id.to_string()),
                },
            );
        }
        persist_locked(&store);
    }
    changed
}

pub fn move_thread_before(project_id: &str, thread_id: &str, before: Option<&str>) -> bool {
    let mut store = THREAD_STORE.lock();
    let domain = tree_domain_for_project(&store, project_id);
    if !remote_tree_mutation_allowed(domain.as_deref()) {
        return false;
    }
    let changed = store.move_thread_before(project_id, thread_id, before);
    if changed {
        if let Some(domain) = domain {
            submit_tree_op(
                domain,
                codec::TreeOp::MoveThreadBefore {
                    project_id: project_id.to_string(),
                    thread_id: thread_id.to_string(),
                    before: before.map(|id| id.to_string()),
                },
            );
        }
        persist_locked(&store);
    }
    changed
}

pub fn toggle_project_threads_collapsed(project_id: &str) -> bool {
    let mut store = THREAD_STORE.lock();
    let changed = store.toggle_project_threads_collapsed(project_id);
    if changed {
        persist_locked(&store);
    }
    changed
}

fn thread_views_for_project(
    project: &Project,
    active_project_id: Option<&str>,
    live_workspaces: &[String],
) -> Vec<WorkspaceThreadView> {
    let project_is_active = active_project_id == Some(project.id.as_str());
    project
        .threads
        .iter()
        .map(|session| {
            let workspace_name = session
                .materialized_workspace_name
                .clone()
                .unwrap_or_else(|| workspace_name_for_thread(&project.id, &session.id));
            let is_materialized = live_workspaces.iter().any(|live| live == &workspace_name);
            WorkspaceThreadView {
                id: session.id.clone(),
                name: session.name.clone(),
                is_active: project_is_active
                    && project.active_thread_id.as_deref() == Some(&session.id),
                is_materialized,
                is_pinned: session.is_pinned,
                is_unread: session.is_unread,
                work_status: session.work_status(),
            }
        })
        .collect()
}

impl Project {
    fn view(&self, live_workspaces: &[String], is_remote: bool) -> WorkspaceThreadsView {
        WorkspaceThreadsView {
            pinned_threads: vec![],
            projects: vec![ProjectView {
                id: self.id.clone(),
                name: self.name.clone(),
                is_active: true,
                threads_collapsed: self.threads_collapsed,
                threads: thread_views_for_project(self, Some(&self.id), live_workspaces),
                is_remote,
                distro: None,
            }],
        }
    }
}

impl WorkspaceThreadStore {
    fn normalize_after_load(&mut self) -> bool {
        let mut changed = false;
        if self.spaces.is_empty() {
            self.spaces.push(Space {
                id: DEFAULT_SPACE_ID.to_string(),
                name: DEFAULT_SPACE_NAME.to_string(),
                active_project_id: self.active_project_id.clone(),
                note_vault: None,
                is_default: true,
                client_domain: None,
            });
            changed = true;
        }

        let default_space_id = self
            .spaces
            .iter()
            .find(|space| space.is_default)
            .map(|space| space.id.clone())
            .unwrap_or_else(|| {
                self.spaces[0].is_default = true;
                self.spaces[0].id.clone()
            });

        let known_spaces = self
            .spaces
            .iter()
            .map(|space| space.id.clone())
            .collect::<std::collections::HashSet<_>>();
        for project in &mut self.projects {
            let shell_path = shell_compatible_project_path(project.path.clone());
            if shell_path != project.path {
                project.path = shell_path;
                changed = true;
            }
            if project.space_id.is_empty() || !known_spaces.contains(&project.space_id) {
                project.space_id = default_space_id.clone();
                changed = true;
            }
        }

        if let Some(active_project_id) = self.active_project_id.take() {
            if let Some(default_space) = self
                .spaces
                .iter_mut()
                .find(|space| space.id == default_space_id)
            {
                if default_space.active_project_id.is_none() {
                    default_space.active_project_id = Some(active_project_id);
                    changed = true;
                }
            }
        }

        for index in 0..self.spaces.len() {
            let space_id = self.spaces[index].id.clone();
            let active_project_id = self.spaces[index].active_project_id.clone();
            let active_is_valid = active_project_id.as_ref().is_some_and(|project_id| {
                self.projects
                    .iter()
                    .any(|project| project.space_id == space_id && &project.id == project_id)
            });
            if !active_is_valid {
                let next_active_project_id = self
                    .projects
                    .iter()
                    .find(|project| project.space_id == space_id)
                    .map(|project| project.id.clone());
                if self.spaces[index].active_project_id != next_active_project_id {
                    self.spaces[index].active_project_id = next_active_project_id;
                    changed = true;
                }
            }
        }

        if self
            .last_active_space_id
            .as_ref()
            .is_none_or(|space_id| !self.has_space(space_id))
        {
            self.last_active_space_id = Some(default_space_id);
            changed = true;
        }
        changed |= self.repair_cross_space_local_workspace_bindings();
        changed |= self.ensure_unique_thread_names();
        changed
    }

    fn has_space(&self, space_id: &str) -> bool {
        self.spaces.iter().any(|space| space.id == space_id)
    }

    fn create_space_record(&mut self, name: String) -> SpaceId {
        self.create_space_record_for_domain(name, None)
    }

    fn create_space_record_for_domain(
        &mut self,
        name: String,
        client_domain: Option<String>,
    ) -> SpaceId {
        let id = new_id("space");
        self.spaces.push(Space {
            id: id.clone(),
            name,
            active_project_id: None,
            note_vault: None,
            is_default: false,
            client_domain,
        });
        id
    }

    fn claim_available_space_id(
        &mut self,
        occupied: &std::collections::HashSet<SpaceId>,
    ) -> SpaceId {
        // Mux-domain Spaces belong to `thinkterm connect`; startup and Dock
        // "New Window" must never claim them (they would materialize a local
        // shell into a remote-owned workspace).
        self.last_active_space_id
            .clone()
            .filter(|space_id| {
                self.has_space(space_id)
                    && !occupied.contains(space_id)
                    && !self.is_client_domain_space(space_id)
            })
            .or_else(|| {
                self.spaces
                    .iter()
                    .find(|space| !occupied.contains(&space.id) && space.client_domain.is_none())
                    .map(|space| space.id.clone())
            })
            .unwrap_or_else(|| {
                let name = next_space_name(&self.spaces);
                self.create_space_record(name)
            })
    }

    /// The Space to open on `domain_name`: the one this device last used if it
    /// is still there, otherwise the domain's first Space in tree order.
    /// `None` when the domain has no Spaces at all.
    fn preferred_space_for_domain(&self, domain_name: &str) -> Option<SpaceId> {
        let belongs = |space_id: &str| {
            self.spaces.iter().any(|space| {
                space.id == space_id && space.client_domain.as_deref() == Some(domain_name)
            })
        };
        self.last_space_per_domain
            .get(domain_name)
            .filter(|space_id| belongs(space_id))
            .cloned()
            .or_else(|| {
                self.spaces
                    .iter()
                    .find(|space| space.client_domain.as_deref() == Some(domain_name))
                    .map(|space| space.id.clone())
            })
    }

    /// Remember which Space this device is using on a remote server, so the
    /// next connect comes back to it instead of the server's first one.
    fn remember_space_for_domain(&mut self, space_id: &str) -> bool {
        let Some(domain_name) = self
            .spaces
            .iter()
            .find(|space| space.id == space_id)
            .and_then(|space| space.client_domain.clone())
        else {
            return false;
        };
        if self
            .last_space_per_domain
            .get(&domain_name)
            .map(String::as_str)
            == Some(space_id)
        {
            return false;
        }
        self.last_space_per_domain
            .insert(domain_name, space_id.to_string());
        true
    }

    fn is_client_domain_space(&self, space_id: &str) -> bool {
        self.spaces
            .iter()
            .any(|space| space.id == space_id && space.client_domain.is_some())
    }

    fn rename_space(&mut self, space_id: &str, name: String) -> bool {
        let name = name.trim();
        if name.is_empty() {
            return false;
        }
        let Some(space) = self.spaces.iter_mut().find(|space| space.id == space_id) else {
            return false;
        };
        // Space ids must be stable: project ids for new projects are derived from
        // space_id, so renaming a Space may only change the display name.
        if space.name == name {
            return false;
        }
        space.name = name.to_string();
        true
    }

    fn delete_space(&mut self, space_id: &str) -> Result<DeletedSpace, DeleteSpaceError> {
        let index = self
            .spaces
            .iter()
            .position(|space| space.id == space_id)
            .ok_or(DeleteSpaceError::NotFound)?;
        if self.spaces[index].is_default {
            return Err(DeleteSpaceError::DefaultSpace);
        }
        if self.spaces.len() <= 1 {
            return Err(DeleteSpaceError::LastSpace);
        }

        self.spaces.remove(index);
        let materialized_workspace_names = self
            .projects
            .iter()
            .filter(|project| project.space_id == space_id)
            .flat_map(|project| {
                project
                    .threads
                    .iter()
                    .filter_map(|thread| thread.materialized_workspace_name.clone())
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        self.projects.retain(|project| project.space_id != space_id);

        let fallback_space_id = self
            .spaces
            .iter()
            .find(|space| space.is_default)
            .or_else(|| self.spaces.first())
            .map(|space| space.id.clone())
            .unwrap_or_else(default_space_id);
        if self.last_active_space_id.as_deref() == Some(space_id) {
            self.last_active_space_id = Some(fallback_space_id.clone());
        }
        Ok(DeletedSpace {
            materialized_workspace_names,
            fallback_space_id,
        })
    }

    /// The Spaces currently attributed to `domain_name`.
    fn space_ids_for_domain(&self, domain_name: &str) -> std::collections::HashSet<SpaceId> {
        self.spaces
            .iter()
            .filter(|space| space.client_domain.as_deref() == Some(domain_name))
            .map(|space| space.id.clone())
            .collect()
    }

    /// The same question across several names, in sidebar order because the
    /// caller removes them one at a time and a nondeterministic order would
    /// make which Space a window lands on depend on hashing.
    fn space_ids_for_domains(&self, domain_names: &[String]) -> Vec<SpaceId> {
        self.spaces
            .iter()
            .filter(|space| {
                space
                    .client_domain
                    .as_ref()
                    .is_some_and(|domain| domain_names.iter().any(|name| name == domain))
            })
            .map(|space| space.id.clone())
            .collect()
    }

    fn remote_space_domains(&self) -> Vec<String> {
        let mut domains: Vec<String> = Vec::new();
        for space in &self.spaces {
            if let Some(domain) = space.client_domain.as_ref() {
                if !domains.iter().any(|known| known == domain) {
                    domains.push(domain.clone());
                }
            }
        }
        domains
    }

    #[cfg(test)]
    fn materialized_workspaces_for_space(&self, space_id: &str) -> Vec<String> {
        self.projects
            .iter()
            .filter(|project| project.space_id == space_id)
            .flat_map(|project| project.threads.iter())
            .filter_map(|thread| thread.materialized_workspace_name.clone())
            .collect()
    }

    fn active_project_id_for_space(&self, space_id: &str) -> Option<ProjectId> {
        self.spaces
            .iter()
            .find(|space| space.id == space_id)
            .and_then(|space| space.active_project_id.clone())
    }

    fn set_active_project_for_space(&mut self, space_id: &str, project_id: ProjectId) -> bool {
        let Some(space) = self.spaces.iter_mut().find(|space| space.id == space_id) else {
            return false;
        };
        if space.active_project_id.as_deref() == Some(&project_id) {
            return false;
        }
        space.active_project_id = Some(project_id);
        true
    }

    fn ensure_current_project(
        &mut self,
        space_id: &str,
        active_workspace: &str,
    ) -> (ProjectId, bool) {
        let current = default_project_for_space(space_id);
        let (project_id, mut changed) = if let Some(project) = self
            .projects
            .iter()
            .find(|p| p.space_id == space_id && p.path == current.path)
        {
            (project.id.clone(), false)
        } else {
            let mut project = current;
            let session = WorkspaceThread::new_initial(
                project.id.clone(),
                "main".to_string(),
                Some(active_workspace.to_string()),
            );
            project.active_thread_id = Some(session.id.clone());
            project.threads.push(session);
            let project_id = project.id.clone();
            self.projects.push(project);
            (project_id, true)
        };
        changed |= self.set_active_project_for_space(space_id, project_id.clone());
        (project_id, changed)
    }

    fn view_for_project(
        &self,
        space_id: &str,
        project_id: &str,
        live_workspaces: &[String],
    ) -> WorkspaceThreadsView {
        let active_project_id = self
            .active_project_id_for_space(space_id)
            .unwrap_or_else(|| project_id.to_string());
        let pinned_threads = self
            .projects
            .iter()
            .filter(|project| project.space_id == space_id)
            .flat_map(|project| {
                thread_views_for_project(project, Some(&active_project_id), live_workspaces)
                    .into_iter()
                    .filter(|session| session.is_pinned)
            })
            .collect();
        let projects = self
            .projects
            .iter()
            .filter(|project| project.space_id == space_id)
            .map(|project| {
                let is_remote = is_remote_project(project, &self.spaces);
                ProjectView {
                    id: project.id.clone(),
                    name: project.name.clone(),
                    is_active: project.id == active_project_id,
                    threads_collapsed: project.threads_collapsed,
                    threads: thread_views_for_project(
                        project,
                        Some(&active_project_id),
                        live_workspaces,
                    )
                    .into_iter()
                    .filter(|session| !session.is_pinned)
                    .collect(),
                    is_remote,
                    distro: if is_remote {
                        crate::ssh_hosts::host_spec(remote_host_id_for_project_id(&project.id))
                            .and_then(|spec| spec.detected_distro.clone())
                    } else {
                        None
                    },
                }
            })
            .collect();
        WorkspaceThreadsView {
            pinned_threads,
            projects,
        }
    }

    fn project_reveal_path(&self, project_id: &str) -> Option<PathBuf> {
        let project = self
            .projects
            .iter()
            .find(|project| project.id == project_id)?;
        if is_remote_project(project, &self.spaces) || !project.path.is_dir() {
            return None;
        }
        Some(project.path.clone())
    }

    fn sync_active_workspace(
        &mut self,
        space_id: &str,
        project_id: &str,
        active_workspace: &str,
    ) -> bool {
        let Some(project) = self.projects.iter_mut().find(|p| p.id == project_id) else {
            return false;
        };
        if project.space_id != space_id {
            return false;
        }
        if let Some(session) = project.threads.iter().find(|session| {
            session.materialized_workspace_name.as_deref() == Some(active_workspace)
        }) {
            if project.active_thread_id.as_deref() != Some(&session.id) {
                project.active_thread_id = Some(session.id.clone());
                return true;
            }
            return false;
        }

        let active_id = project
            .active_thread_id
            .clone()
            .or_else(|| project.threads.first().map(|session| session.id.clone()));
        if let Some(active_id) = active_id {
            if let Some(session) = project
                .threads
                .iter_mut()
                .find(|session| session.id == active_id)
            {
                let mut changed = false;
                if session.materialized_workspace_name.as_deref() != Some(active_workspace) {
                    session.materialized_workspace_name = Some(active_workspace.to_string());
                    changed = true;
                }
                if project.active_thread_id.as_deref() != Some(&session.id) {
                    project.active_thread_id = Some(session.id.clone());
                    changed = true;
                }
                return changed;
            }
        }
        false
    }

    fn sync_current_project(&mut self, space_id: &str, active_workspace: &str) -> bool {
        let mut changed = self.normalize_after_load();
        if self.workspace_belongs_to_other_space(space_id, active_workspace) {
            return changed;
        }
        let (project_id, project_changed) =
            if let Some(project_id) = self.project_id_for_workspace(space_id, active_workspace) {
                if self.set_active_project_for_space(space_id, project_id.clone()) {
                    (project_id, true)
                } else {
                    (project_id, false)
                }
            } else if self.is_client_domain_space(space_id) {
                // A mux-domain Space's connect window lives in the default
                // mux workspace, which is not bound to any thread. Resolve to
                // the Space's mux project instead of manufacturing a local
                // Home project.
                let Some(project_id) = self
                    .projects
                    .iter()
                    .find(|p| p.space_id == space_id && is_mux_domain_project_id(&p.id))
                    .map(|p| p.id.clone())
                else {
                    return changed;
                };
                let set = self.set_active_project_for_space(space_id, project_id.clone());
                (project_id, set)
            } else {
                self.ensure_current_project(space_id, active_workspace)
            };
        changed |= project_changed;
        changed |= self.sync_active_workspace(space_id, &project_id, active_workspace);
        changed
    }

    fn workspace_space_id(&self, workspace: &str) -> Option<SpaceId> {
        self.projects.iter().find_map(|project| {
            project
                .threads
                .iter()
                .any(|session| {
                    session.materialized_workspace_name.as_deref() == Some(workspace)
                        || workspace_name_for_thread(&project.id, &session.id) == workspace
                })
                .then(|| project.space_id.clone())
        })
    }

    fn thread_id_for_workspace(
        &self,
        space_id: &str,
        workspace: &str,
    ) -> Option<WorkspaceThreadId> {
        self.projects.iter().find_map(|project| {
            if project.space_id != space_id {
                return None;
            }
            project
                .threads
                .iter()
                .find(|session| {
                    session.materialized_workspace_name.as_deref() == Some(workspace)
                        || workspace_name_for_thread(&project.id, &session.id) == workspace
                })
                .map(|session| session.id.clone())
        })
    }

    fn workspace_belongs_to_other_space(&self, space_id: &str, workspace: &str) -> bool {
        self.workspace_space_id(workspace)
            .is_some_and(|owner_space_id| owner_space_id != space_id)
    }

    fn repair_cross_space_local_workspace_bindings(&mut self) -> bool {
        let mut changed = false;
        let spaces = &self.spaces;
        for project in &mut self.projects {
            if is_remote_project(project, spaces) {
                continue;
            }
            for session in &mut project.threads {
                let expected_workspace = workspace_name_for_thread(&project.id, &session.id);
                let Some(materialized_workspace) = session.materialized_workspace_name.as_deref()
                else {
                    continue;
                };

                // Space ids are part of new local project ids. A local thread
                // pointing at another `thinkterm:*` workspace would make two
                // Spaces share one live terminal and corrupt layout snapshots.
                if materialized_workspace.starts_with("thinkterm:")
                    && materialized_workspace != expected_workspace
                {
                    session.materialized_workspace_name = Some(expected_workspace);
                    changed = true;
                }
            }
        }
        changed
    }

    fn ensure_unique_thread_names(&mut self) -> bool {
        let mut changed = false;
        for project in &mut self.projects {
            let mut used = Vec::<String>::new();
            let mut next_index = project.threads.len().saturating_add(1).max(1);

            for session in &mut project.threads {
                let trimmed = session.name.trim();
                if !trimmed.is_empty() && !used.iter().any(|name| name == trimmed) {
                    if session.name != trimmed {
                        session.name = trimmed.to_string();
                        changed = true;
                    }
                    used.push(session.name.clone());
                    continue;
                }

                loop {
                    let candidate = format!("Thread {next_index}");
                    next_index += 1;
                    if !used.iter().any(|name| name == &candidate) {
                        session.name = candidate.clone();
                        used.push(candidate);
                        changed = true;
                        break;
                    }
                }
            }
        }
        changed
    }

    fn create_thread(&mut self, project_id: &str, name: Option<String>) -> WorkspaceThreadId {
        let project = self
            .projects
            .iter_mut()
            .find(|project| project.id == project_id)
            .expect("project_id should exist before creating session");
        let name = name
            .map(|name| unique_thread_name(project, None, name.trim()))
            .unwrap_or_else(|| next_thread_name(project));
        let session = WorkspaceThread::new(project.id.clone(), name, None);
        let id = session.id.clone();
        project.threads.push(session);
        id
    }

    fn create_disconnected_remote_host_thread(
        &mut self,
        space_id: &str,
        host_id: &str,
        label: &str,
        path: PathBuf,
        workspace_override: Option<String>,
    ) -> WorkspaceThreadId {
        let project_id = if self
            .projects
            .iter()
            .any(|project| project.id == host_id && project.space_id == space_id)
        {
            host_id.to_string()
        } else {
            remote_project_id_for_space(space_id, host_id)
        };

        if let Some(project) = self.projects.iter_mut().find(|p| p.id == project_id) {
            project.space_id = space_id.to_string();
            project.name = label.to_string();
            project.path = path.clone();
        } else {
            self.projects.push(Project {
                id: project_id.clone(),
                space_id: space_id.to_string(),
                name: label.to_string(),
                path,
                threads: vec![],
                active_thread_id: None,
                threads_collapsed: false,
                active_note_path: None,
            });
        }

        let project = self
            .projects
            .iter_mut()
            .find(|p| p.id == project_id)
            .expect("remote project was just inserted");
        let name = next_thread_name(project);
        let mut session = WorkspaceThread::new(project_id.clone(), name, None);
        if let Some(workspace) = workspace_override.as_deref().map(str::trim) {
            if !workspace.is_empty() {
                session.planned_workspace_name = Some(workspace_name_for_remote_default(
                    &project_id,
                    &session.id,
                    workspace,
                ));
            }
        }
        let thread_id = session.id.clone();
        project.threads.push(session);
        thread_id
    }

    fn create_project_from_path(&mut self, space_id: &str, path: PathBuf) -> WorkspaceThreadId {
        // New project ids include the stable Space id, but migration must never
        // recompute old project ids; existing Project.id values remain valid.
        let project_id = project_id_for_path(space_id, &path);
        if let Some(existing_project_id) = self
            .projects
            .iter()
            .find(|project| project.space_id == space_id && project.path == path)
            .map(|project| project.id.clone())
        {
            if let Some(thread_id) = self.active_thread_for_project(&existing_project_id) {
                return thread_id;
            }
        }

        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| !name.is_empty())
            .unwrap_or("Project")
            .to_string();
        let mut project = Project {
            id: project_id.clone(),
            space_id: space_id.to_string(),
            name,
            path,
            threads: vec![],
            active_thread_id: None,
            threads_collapsed: false,
            active_note_path: None,
        };
        let session = WorkspaceThread::new(project_id.clone(), "main".to_string(), None);
        let thread_id = session.id.clone();
        project.active_thread_id = Some(thread_id.clone());
        project.threads.push(session);
        self.set_active_project_for_space(space_id, project_id);
        self.projects.push(project);
        thread_id
    }

    fn project_id_for_workspace(&self, space_id: &str, workspace: &str) -> Option<ProjectId> {
        self.projects.iter().find_map(|project| {
            if project.space_id != space_id {
                return None;
            }
            project
                .threads
                .iter()
                .any(|session| {
                    session.materialized_workspace_name.as_deref() == Some(workspace)
                        || workspace_name_for_thread(&project.id, &session.id) == workspace
                })
                .then(|| project.id.clone())
        })
    }

    fn active_thread_for_project(&mut self, project_id: &str) -> Option<WorkspaceThreadId> {
        let project_index = self
            .projects
            .iter()
            .position(|project| project.id == project_id)?;
        let project = &mut self.projects[project_index];
        if project.active_thread_id.as_ref().is_some_and(|active_id| {
            project
                .threads
                .iter()
                .any(|session| &session.id == active_id)
        }) {
            let space_id = project.space_id.clone();
            let project_id = project.id.clone();
            let active_thread_id = project.active_thread_id.clone();
            let _ = project;
            self.set_active_project_for_space(&space_id, project_id);
            return active_thread_id;
        }

        let thread_id = project
            .threads
            .iter()
            .map(|session| session.id.clone())
            .next()
            .unwrap_or_else(|| {
                let session = WorkspaceThread::new(project.id.clone(), "main".to_string(), None);
                let thread_id = session.id.clone();
                project.threads.push(session);
                thread_id
            });
        project.active_thread_id = Some(thread_id.clone());
        let space_id = project.space_id.clone();
        let project_id = project.id.clone();
        let _ = project;
        self.set_active_project_for_space(&space_id, project_id);
        Some(thread_id)
    }

    fn thread_to_restore_for_space(&mut self, space_id: &str) -> (Option<WorkspaceThreadId>, bool) {
        if !self.has_space(space_id) {
            return (None, false);
        }

        let project_indices = self
            .active_project_id_for_space(space_id)
            .and_then(|active_project_id| {
                self.projects.iter().position(|project| {
                    project.space_id == space_id && project.id == active_project_id
                })
            })
            .into_iter()
            .chain(
                self.projects
                    .iter()
                    .enumerate()
                    .filter_map(|(index, project)| (project.space_id == space_id).then_some(index)),
            )
            .collect::<Vec<_>>();

        for project_index in project_indices {
            let Some(thread_id) = self.restorable_thread_id_for_project(project_index) else {
                continue;
            };
            let project = &mut self.projects[project_index];
            let mut changed = false;
            if project.active_thread_id.as_deref() != Some(&thread_id) {
                project.active_thread_id = Some(thread_id.clone());
                changed = true;
            }
            let project_id = project.id.clone();
            let _ = project;
            changed |= self.set_active_project_for_space(space_id, project_id);
            return (Some(thread_id), changed);
        }

        (None, false)
    }

    fn restorable_thread_id_for_project(&self, project_index: usize) -> Option<WorkspaceThreadId> {
        let project = self.projects.get(project_index)?;
        if self.is_client_domain_space(&project.space_id) {
            return None;
        }
        project
            .active_thread_id
            .as_ref()
            .and_then(|active_thread_id| {
                project
                    .threads
                    .iter()
                    .find(|thread| {
                        &thread.id == active_thread_id && thread_has_restorable_workspace(thread)
                    })
                    .map(|thread| thread.id.clone())
            })
            .or_else(|| {
                project
                    .threads
                    .iter()
                    .find(|thread| thread_has_restorable_workspace(thread))
                    .map(|thread| thread.id.clone())
            })
    }

    fn activate_thread_record(
        &mut self,
        thread_id: &str,
        live_workspaces: &[String],
    ) -> Option<ActivationPlan> {
        let plan = self.activation_plan_for_thread(thread_id, live_workspaces)?;
        let project = self
            .projects
            .iter_mut()
            .find(|project| project.id == plan.project_id)?;
        let session = project
            .threads
            .iter_mut()
            .find(|session| session.id == plan.thread_id)?;
        session.materialized_workspace_name = Some(plan.workspace_name.clone());
        session.last_active_at = now_ts();
        session.is_unread = false;
        session.work_finished_unseen = false;
        project.active_thread_id = Some(plan.thread_id.clone());
        let space_id = project.space_id.clone();
        let active_project_id = project.id.clone();
        let _ = project;
        self.set_active_project_for_space(&space_id, active_project_id);
        Some(plan)
    }

    fn activation_plan_for_thread(
        &self,
        thread_id: &str,
        live_workspaces: &[String],
    ) -> Option<ActivationPlan> {
        let project = self.projects.iter().find(|project| {
            project
                .threads
                .iter()
                .any(|session| session.id == thread_id)
        })?;
        let session = project
            .threads
            .iter()
            .find(|session| session.id == thread_id)?;
        let workspace_name = session
            .materialized_workspace_name
            .clone()
            .or_else(|| session.planned_workspace_name.clone())
            .unwrap_or_else(|| workspace_name_for_thread(&project.id, &session.id));
        let needs_materialize = !live_workspaces.iter().any(|live| live == &workspace_name);
        Some(ActivationPlan {
            project_id: project.id.clone(),
            thread_id: session.id.clone(),
            workspace_name,
            project_path: project.path.clone(),
            needs_materialize,
        })
    }

    fn thread_connection_state(
        &self,
        thread_id: &str,
        live_workspaces: &[String],
    ) -> Option<ThreadConnectionState> {
        let project = self.projects.iter().find(|project| {
            project
                .threads
                .iter()
                .any(|session| session.id == thread_id)
        })?;
        let session = project
            .threads
            .iter()
            .find(|session| session.id == thread_id)?;
        let workspace_name = session
            .materialized_workspace_name
            .clone()
            .or_else(|| session.planned_workspace_name.clone())
            .unwrap_or_else(|| workspace_name_for_thread(&project.id, &session.id));
        Some(ThreadConnectionState {
            space_id: project.space_id.clone(),
            project_id: project.id.clone(),
            thread_id: session.id.clone(),
            project_name: project.name.clone(),
            thread_name: session.name.clone(),
            workspace_name: workspace_name.clone(),
            is_remote: is_remote_project(project, &self.spaces),
            is_live: live_workspaces.iter().any(|live| live == &workspace_name),
        })
    }

    fn thread_space_id(&self, thread_id: &str) -> Option<SpaceId> {
        self.projects.iter().find_map(|project| {
            project
                .threads
                .iter()
                .any(|session| session.id == thread_id)
                .then(|| project.space_id.clone())
        })
    }

    fn snapshot_active_space_thread_layout(
        &mut self,
        space_id: &str,
        workspace: &str,
        snapshot: WorkspaceThreadLayoutSnapshot,
    ) -> bool {
        let Some(project_id) = self.active_project_id_for_space(space_id) else {
            return false;
        };
        // Never write a local layout for mux-domain threads; the remote mux
        // server owns the layout truth (second layer under the window-origin
        // tag guard in snapshot_active_space_thread_layout_with_font_scales).
        if self.is_client_domain_space(space_id) || is_mux_domain_project_id(&project_id) {
            return false;
        }
        let Some(project) = self
            .projects
            .iter_mut()
            .find(|project| project.space_id == space_id && project.id == project_id)
        else {
            return false;
        };
        let project_id = project.id.clone();
        let active_thread_id = project
            .active_thread_id
            .clone()
            .or_else(|| project.threads.first().map(|session| session.id.clone()));
        let Some(active_thread_id) = active_thread_id else {
            return false;
        };
        let Some(session) = project
            .threads
            .iter_mut()
            .find(|session| session.id == active_thread_id)
        else {
            return false;
        };

        let expected_workspace = session
            .materialized_workspace_name
            .clone()
            .unwrap_or_else(|| workspace_name_for_thread(&project_id, &session.id));
        if expected_workspace != workspace {
            return false;
        }

        session.materialized_workspace_name = Some(workspace.to_string());
        session.layout = Some(snapshot);
        session.last_active_at = now_ts();
        true
    }

    fn workspace_pane_font_scales(
        &self,
        workspace: &str,
        window_id: MuxWindowId,
    ) -> Option<HashMap<PaneId, Option<f64>>> {
        self.projects
            .iter()
            .flat_map(|project| project.threads.iter())
            .find(|session| session.materialized_workspace_name.as_deref() == Some(workspace))
            .and_then(|session| session.layout.as_ref())
            .and_then(|layout| pane_font_scales_for_window(layout, window_id))
    }

    /// Store remote-pane-id-keyed font scales for the active thread of the
    /// given mux-domain Space. Counterpart of snapshot_active_space_thread_layout
    /// for the one piece of state that IS client-owned on remote threads.
    fn snapshot_remote_thread_font_scales(
        &mut self,
        space_id: &str,
        workspace: &str,
        scales: HashMap<PaneId, f64>,
    ) -> bool {
        let Some(project_id) = self.active_project_id_for_space(space_id) else {
            return false;
        };
        let Some(project) = self
            .projects
            .iter_mut()
            .find(|project| project.space_id == space_id && project.id == project_id)
        else {
            return false;
        };
        let project_id = project.id.clone();
        let active_thread_id = project
            .active_thread_id
            .clone()
            .or_else(|| project.threads.first().map(|session| session.id.clone()));
        let Some(active_thread_id) = active_thread_id else {
            return false;
        };
        let Some(session) = project
            .threads
            .iter_mut()
            .find(|session| session.id == active_thread_id)
        else {
            return false;
        };

        let expected_workspace = session
            .materialized_workspace_name
            .clone()
            .unwrap_or_else(|| workspace_name_for_thread(&project_id, &session.id));
        if expected_workspace != workspace {
            return false;
        }

        if session.remote_font_scales == scales {
            return false;
        }
        session.remote_font_scales = scales;
        true
    }

    fn remote_thread_font_scales(&self, workspace: &str) -> Option<HashMap<PaneId, f64>> {
        self.projects
            .iter()
            .flat_map(|project| project.threads.iter())
            .find(|session| session.materialized_workspace_name.as_deref() == Some(workspace))
            .map(|session| session.remote_font_scales.clone())
            .filter(|scales| !scales.is_empty())
    }

    /// Ops that make the server's copy of this domain's subtree match ours.
    ///
    /// Every mutation with a simple shape sends its own targeted op; this is
    /// for the compound ones (ending a thread, adopting an orphaned window,
    /// first connect) where enumerating the individual edits would be more
    /// error-prone than restating the result.
    ///
    /// `last_known` is the tree the server last told us about, and every op
    /// here is a *difference* from it. That is what keeps a reconcile from
    /// trampling another device: if a row still looks exactly as the server
    /// last described it we have nothing to say about that row, so a rename
    /// that landed there after our snapshot is left alone instead of being
    /// reverted to our stale copy. Only rows we actually changed — and rows
    /// the server has that we no longer do — produce ops.
    ///
    /// Row *order* is not reconciled: reorders have their own ops, and the
    /// paths that call this only ever append on both sides.
    fn reconcile_ops_for_domain(
        &self,
        domain_name: &str,
        last_known: &codec::ThinkTermTree,
    ) -> Vec<codec::TreeOp> {
        let space_ids: Vec<SpaceId> = self
            .spaces
            .iter()
            .filter(|space| space.client_domain.as_deref() == Some(domain_name))
            .map(|space| space.id.clone())
            .collect();

        let mut ops = vec![];
        let mut live_projects: std::collections::HashSet<&str> = Default::default();
        let mut live_threads: std::collections::HashSet<&str> = Default::default();

        for space in self
            .spaces
            .iter()
            .filter(|space| space.client_domain.as_deref() == Some(domain_name))
        {
            match last_known.space(&space.id) {
                None => ops.push(codec::TreeOp::CreateSpace {
                    space_id: space.id.clone(),
                    name: space.name.clone(),
                }),
                Some(known) if known.name != space.name => ops.push(codec::TreeOp::RenameSpace {
                    space_id: space.id.clone(),
                    name: space.name.clone(),
                }),
                Some(_) => {}
            }
        }

        for project in self
            .projects
            .iter()
            .filter(|project| space_ids.contains(&project.space_id))
        {
            live_projects.insert(project.id.as_str());
            let known_project = last_known.project(&project.id);
            match known_project {
                None => ops.push(codec::TreeOp::CreateProject {
                    project_id: project.id.clone(),
                    space_id: project.space_id.clone(),
                    name: project.name.clone(),
                    path: project.path.to_string_lossy().to_string(),
                }),
                Some(known) if known.name != project.name => {
                    ops.push(codec::TreeOp::RenameProject {
                        project_id: project.id.clone(),
                        name: project.name.clone(),
                    })
                }
                Some(_) => {}
            }
            for thread in &project.threads {
                live_threads.insert(thread.id.as_str());
                let Some(known) = last_known.thread(&thread.id) else {
                    ops.push(codec::TreeOp::CreateThread {
                        thread_id: thread.id.clone(),
                        project_id: project.id.clone(),
                        name: thread.name.clone(),
                        workspace: thread.materialized_workspace_name.clone(),
                        created_at: thread.last_active_at,
                    });
                    if thread.planned_workspace_name.is_some() {
                        ops.push(codec::TreeOp::SetThreadWorkspaceName {
                            thread_id: thread.id.clone(),
                            planned: thread.planned_workspace_name.clone(),
                            materialized: thread.materialized_workspace_name.clone(),
                        });
                    }
                    if thread.is_pinned {
                        ops.push(codec::TreeOp::SetThreadPinned {
                            thread_id: thread.id.clone(),
                            pinned: true,
                            last_active_at: thread.last_active_at,
                        });
                    }
                    if thread.is_unread {
                        ops.push(codec::TreeOp::SetThreadUnread {
                            thread_id: thread.id.clone(),
                            unread: true,
                        });
                    }
                    continue;
                };

                // `SetThreadPinned` and `RenameThread` carry the recency stamp
                // because toggling a pin and renaming both bump it locally.
                // Sending a bare `TouchThread` when neither fired avoids
                // restating a name we may no longer own.
                let mut stamp_sent = false;
                if known.is_pinned != thread.is_pinned {
                    ops.push(codec::TreeOp::SetThreadPinned {
                        thread_id: thread.id.clone(),
                        pinned: thread.is_pinned,
                        last_active_at: thread.last_active_at,
                    });
                    stamp_sent = true;
                }
                if known.name != thread.name {
                    ops.push(codec::TreeOp::RenameThread {
                        thread_id: thread.id.clone(),
                        name: thread.name.clone(),
                        last_active_at: thread.last_active_at,
                    });
                    stamp_sent = true;
                }
                if !stamp_sent && known.last_active_at != thread.last_active_at {
                    ops.push(codec::TreeOp::TouchThread {
                        thread_id: thread.id.clone(),
                        at: thread.last_active_at,
                    });
                }
                if known.planned_workspace_name != thread.planned_workspace_name
                    || known.materialized_workspace_name != thread.materialized_workspace_name
                {
                    ops.push(codec::TreeOp::SetThreadWorkspaceName {
                        thread_id: thread.id.clone(),
                        planned: thread.planned_workspace_name.clone(),
                        materialized: thread.materialized_workspace_name.clone(),
                    });
                }
                if known.is_unread != thread.is_unread {
                    ops.push(codec::TreeOp::SetThreadUnread {
                        thread_id: thread.id.clone(),
                        unread: thread.is_unread,
                    });
                }
            }
        }

        // Threads and projects first: deleting their Space would take them
        // with it, and we want the narrower removal when only a row went away.
        for project in &last_known.projects {
            for thread in &project.threads {
                if !live_threads.contains(thread.id.as_str()) {
                    ops.push(codec::TreeOp::DeleteThread {
                        thread_id: thread.id.clone(),
                    });
                }
            }
            if !live_projects.contains(project.id.as_str()) {
                ops.push(codec::TreeOp::RemoveProject {
                    project_id: project.id.clone(),
                });
            }
        }
        for space in &last_known.spaces {
            if !space_ids.contains(&space.id) {
                ops.push(codec::TreeOp::DeleteSpace {
                    space_id: space.id.clone(),
                });
            }
        }

        ops
    }

    /// Rebuild this domain's Spaces/Projects/Threads from the server's tree.
    ///
    /// The server owns what exists, what it is called, its order and its pins.
    /// This device owns where it is looking (`active_*`), how it is displayed
    /// (`threads_collapsed`, font scales), its Vault/Note bindings, and the
    /// locally-derived agent work status. Those are carried across by id, so a
    /// push from another device cannot collapse your rows or move your cursor.
    ///
    /// Remote records keep being written to the local JSON file, but purely as
    /// a cache: it lets the sidebar draw something before the attach finishes,
    /// and this function overwrites it wholesale when the truth arrives.
    fn ingest_remote_tree(&mut self, domain_name: &str, tree: &codec::ThinkTermTree) -> bool {
        let owned_space_ids: std::collections::HashSet<SpaceId> = self
            .spaces
            .iter()
            .filter(|space| space.client_domain.as_deref() == Some(domain_name))
            .map(|space| space.id.clone())
            .chain(tree.spaces.iter().map(|space| space.id.clone()))
            .collect();

        let mut space_state: HashMap<SpaceId, (Option<ProjectId>, Option<SpaceVaultBinding>)> =
            HashMap::new();
        for space in &self.spaces {
            if space.client_domain.as_deref() == Some(domain_name) {
                space_state.insert(
                    space.id.clone(),
                    (space.active_project_id.clone(), space.note_vault.clone()),
                );
            }
        }

        struct ProjectViewState {
            active_thread_id: Option<WorkspaceThreadId>,
            threads_collapsed: bool,
            active_note_path: Option<String>,
        }
        struct ThreadViewState {
            remote_font_scales: HashMap<PaneId, f64>,
            work_is_running: bool,
            work_needs_attention: bool,
            work_finished_unseen: bool,
        }

        let mut project_state: HashMap<ProjectId, ProjectViewState> = HashMap::new();
        let mut thread_state: HashMap<WorkspaceThreadId, ThreadViewState> = HashMap::new();
        for project in &self.projects {
            if !owned_space_ids.contains(&project.space_id) {
                continue;
            }
            project_state.insert(
                project.id.clone(),
                ProjectViewState {
                    active_thread_id: project.active_thread_id.clone(),
                    threads_collapsed: project.threads_collapsed,
                    active_note_path: project.active_note_path.clone(),
                },
            );
            for thread in &project.threads {
                thread_state.insert(
                    thread.id.clone(),
                    ThreadViewState {
                        remote_font_scales: thread.remote_font_scales.clone(),
                        work_is_running: thread.work_is_running,
                        work_needs_attention: thread.work_needs_attention,
                        work_finished_unseen: thread.work_finished_unseen,
                    },
                );
            }
        }

        let new_spaces: Vec<Space> = tree
            .spaces
            .iter()
            .map(|space| {
                let (active_project_id, note_vault) =
                    space_state.get(&space.id).cloned().unwrap_or((None, None));
                Space {
                    id: space.id.clone(),
                    name: space.name.clone(),
                    active_project_id,
                    note_vault,
                    is_default: false,
                    client_domain: Some(domain_name.to_string()),
                }
            })
            .collect();

        let new_projects: Vec<Project> = tree
            .projects
            .iter()
            .map(|project| {
                let view = project_state.get(&project.id);
                let threads: Vec<WorkspaceThread> = project
                    .threads
                    .iter()
                    .map(|thread| {
                        let state = thread_state.get(&thread.id);
                        WorkspaceThread {
                            id: thread.id.clone(),
                            name: thread.name.clone(),
                            project_id: thread.project_id.clone(),
                            // A remote thread's layout belongs to the server;
                            // the client never persists one for it.
                            layout: None,
                            remote_font_scales: state
                                .map(|state| state.remote_font_scales.clone())
                                .unwrap_or_default(),
                            planned_workspace_name: thread.planned_workspace_name.clone(),
                            materialized_workspace_name: thread.materialized_workspace_name.clone(),
                            last_active_at: thread.last_active_at,
                            is_pinned: thread.is_pinned,
                            is_unread: thread.is_unread,
                            work_is_running: state.map_or(false, |state| state.work_is_running),
                            work_needs_attention: state
                                .map_or(false, |state| state.work_needs_attention),
                            work_finished_unseen: state
                                .map_or(false, |state| state.work_finished_unseen),
                        }
                    })
                    .collect();
                Project {
                    id: project.id.clone(),
                    space_id: project.space_id.clone(),
                    name: project.name.clone(),
                    path: PathBuf::from(&project.path),
                    // Drop a remembered active thread that the server no
                    // longer has, rather than pointing at a deleted row.
                    active_thread_id: view.and_then(|view| {
                        view.active_thread_id
                            .clone()
                            .filter(|id| threads.iter().any(|thread| &thread.id == id))
                    }),
                    threads,
                    threads_collapsed: view.map_or(false, |view| view.threads_collapsed),
                    active_note_path: view.and_then(|view| view.active_note_path.clone()),
                }
            })
            .collect();

        // Splice the rebuilt block in where the old one started so that the
        // Space menu and sidebar do not reshuffle on every push.
        let spaces = splice_in_place(&self.spaces, new_spaces, |space| {
            space.client_domain.as_deref() == Some(domain_name)
        });
        let projects = splice_in_place(&self.projects, new_projects, |project| {
            owned_space_ids.contains(&project.space_id)
        });

        if spaces == self.spaces && projects == self.projects {
            return false;
        }
        self.spaces = spaces;
        self.projects = projects;
        true
    }

    fn thread_record(&self, thread_id: &str) -> Option<&WorkspaceThread> {
        self.projects
            .iter()
            .flat_map(|project| project.threads.iter())
            .find(|thread| thread.id == thread_id)
    }

    fn rename_project(&mut self, project_id: &str, name: String) -> bool {
        let name = name.trim();
        if name.is_empty() {
            return false;
        }

        let Some(project) = self
            .projects
            .iter_mut()
            .find(|project| project.id == project_id)
        else {
            return false;
        };
        if project.name == name {
            return false;
        }
        project.name = name.to_string();
        true
    }

    fn rename_thread(&mut self, thread_id: &str, name: String) -> bool {
        let name = name.trim();
        if name.is_empty() {
            return false;
        }

        for project in &mut self.projects {
            let Some(index) = project
                .threads
                .iter()
                .position(|session| session.id == thread_id)
            else {
                continue;
            };

            let unique_name = unique_thread_name(project, Some(thread_id), name);
            let session = &mut project.threads[index];
            if session.name == unique_name {
                return false;
            }
            session.name = unique_name;
            session.last_active_at = now_ts();
            return true;
        }
        false
    }

    fn toggle_thread_pinned(&mut self, thread_id: &str) -> bool {
        for project in &mut self.projects {
            if let Some(session) = project
                .threads
                .iter_mut()
                .find(|session| session.id == thread_id)
            {
                session.is_pinned = !session.is_pinned;
                session.last_active_at = now_ts();
                return true;
            }
        }
        false
    }

    fn mark_thread_unread(&mut self, thread_id: &str) -> bool {
        for project in &mut self.projects {
            if let Some(session) = project
                .threads
                .iter_mut()
                .find(|session| session.id == thread_id)
            {
                if session.is_unread {
                    return false;
                }
                session.is_unread = true;
                return true;
            }
        }
        false
    }

    fn observe_thread_work_for_workspace(
        &mut self,
        workspace: &str,
        observed: WorkspaceThreadWorkStatus,
    ) -> Option<WorkspaceThreadWorkChange> {
        for project in &mut self.projects {
            let project_id = project.id.clone();
            for session in &mut project.threads {
                let session_workspace = session
                    .materialized_workspace_name
                    .clone()
                    .unwrap_or_else(|| workspace_name_for_thread(&project_id, &session.id));
                if session_workspace == workspace {
                    return Some(session.observe_work_status(observed));
                }
            }
        }
        None
    }

    fn acknowledge_thread_work_for_workspace(
        &mut self,
        workspace: &str,
    ) -> WorkspaceThreadWorkChange {
        for project in &mut self.projects {
            let project_id = project.id.clone();
            for session in &mut project.threads {
                let session_workspace = session
                    .materialized_workspace_name
                    .clone()
                    .unwrap_or_else(|| workspace_name_for_thread(&project_id, &session.id));
                if session_workspace == workspace {
                    let should_persist = session.work_finished_unseen;
                    let changed = session.work_is_running
                        || session.work_needs_attention
                        || session.work_finished_unseen;
                    session.work_is_running = false;
                    session.work_needs_attention = false;
                    session.work_finished_unseen = false;
                    return WorkspaceThreadWorkChange {
                        changed,
                        should_persist,
                        announce: None,
                    };
                }
            }
        }
        WorkspaceThreadWorkChange {
            changed: false,
            should_persist: false,
            announce: None,
        }
    }

    fn acknowledge_thread_work_for_thread(&mut self, thread_id: &str) -> WorkspaceThreadWorkChange {
        for project in &mut self.projects {
            for session in &mut project.threads {
                if session.id == thread_id {
                    let should_persist = session.work_finished_unseen;
                    let changed = session.work_needs_attention || session.work_finished_unseen;
                    session.work_needs_attention = false;
                    session.work_finished_unseen = false;
                    return WorkspaceThreadWorkChange {
                        changed,
                        should_persist,
                        announce: None,
                    };
                }
            }
        }
        WorkspaceThreadWorkChange {
            changed: false,
            should_persist: false,
            announce: None,
        }
    }

    fn pending_work_notifications(&self) -> Vec<ThreadWorkNotification> {
        let mut notifications = Vec::new();
        for project in &self.projects {
            let space_name = self
                .spaces
                .iter()
                .find(|space| space.id == project.space_id)
                .map(|space| space.name.clone())
                .unwrap_or_default();
            for session in &project.threads {
                let status = session.work_status();
                if matches!(
                    status,
                    WorkspaceThreadWorkStatus::NeedsAttention
                        | WorkspaceThreadWorkStatus::FinishedUnseen
                ) {
                    notifications.push(ThreadWorkNotification {
                        space_id: project.space_id.clone(),
                        space_name: space_name.clone(),
                        project_name: project.name.clone(),
                        thread_id: session.id.clone(),
                        thread_name: session.name.clone(),
                        status,
                    });
                }
            }
        }
        notifications.sort_by_key(|notification| match notification.status {
            WorkspaceThreadWorkStatus::NeedsAttention => 0,
            _ => 1,
        });
        notifications
    }

    fn pending_work_notification_count(&self) -> usize {
        self.projects
            .iter()
            .flat_map(|project| project.threads.iter())
            .filter(|session| {
                matches!(
                    session.work_status(),
                    WorkspaceThreadWorkStatus::NeedsAttention
                        | WorkspaceThreadWorkStatus::FinishedUnseen
                )
            })
            .count()
    }

    fn thread_workspace_names(&self) -> Vec<String> {
        let mut names = Vec::new();
        for project in &self.projects {
            for session in &project.threads {
                names.push(
                    session
                        .materialized_workspace_name
                        .clone()
                        .unwrap_or_else(|| workspace_name_for_thread(&project.id, &session.id)),
                );
            }
        }
        names
    }

    fn delete_thread(&mut self, thread_id: &str) -> Option<DeletedWorkspaceThread> {
        for project in &mut self.projects {
            let Some(index) = project
                .threads
                .iter()
                .position(|session| session.id == thread_id)
            else {
                continue;
            };
            if project.threads.len() <= 1 {
                return None;
            }

            let removed = project.threads.remove(index);
            let was_active = project.active_thread_id.as_deref() == Some(thread_id);
            let next_thread_id = if was_active {
                let next_index = index.saturating_sub(1).min(project.threads.len() - 1);
                let next_id = project.threads[next_index].id.clone();
                project.active_thread_id = Some(next_id.clone());
                Some(next_id)
            } else {
                None
            };

            return Some(DeletedWorkspaceThread {
                was_active,
                next_thread_id,
                materialized_workspace_name: removed.materialized_workspace_name,
            });
        }
        None
    }

    fn end_workspace_thread_record(&mut self, thread_id: &str) -> EndWorkspaceThreadResult {
        if let Some(deleted) = self.delete_thread(thread_id) {
            return EndWorkspaceThreadResult::DeletedThread(deleted);
        }

        let Some(project_index) = self.projects.iter().position(|project| {
            project
                .threads
                .iter()
                .any(|session| session.id == thread_id)
        }) else {
            return EndWorkspaceThreadResult::Noop;
        };

        if !is_remote_project(&self.projects[project_index], &self.spaces) {
            return EndWorkspaceThreadResult::Noop;
        }

        let project_id = self.projects[project_index].id.clone();
        let space_id = self.projects[project_index].space_id.clone();
        let project_count = self
            .projects
            .iter()
            .filter(|project| project.space_id == space_id)
            .count();

        if project_count <= 1 {
            let mut project = default_project_for_space(&space_id);
            let session = WorkspaceThread::new(project.id.clone(), "main".to_string(), None);
            project.active_thread_id = Some(session.id.clone());
            project.threads.push(session);
            self.projects.push(project);
            self.set_active_project_for_space(&space_id, project_id.clone());
        }

        self.remove_project(&project_id)
            .map(EndWorkspaceThreadResult::RemovedProject)
            .unwrap_or(EndWorkspaceThreadResult::Noop)
    }

    fn disconnect_workspace_thread_record(
        &mut self,
        thread_id: &str,
        live_workspaces: &[String],
    ) -> (Option<DisconnectedWorkspaceThread>, bool) {
        let Some(project_index) = self.projects.iter().position(|project| {
            project
                .threads
                .iter()
                .any(|session| session.id == thread_id)
        }) else {
            return (None, false);
        };

        if !is_remote_project(&self.projects[project_index], &self.spaces) {
            return (None, false);
        }

        let project_id = self.projects[project_index].id.clone();
        let space_id = self.projects[project_index].space_id.clone();
        let workspace_name = self.projects[project_index]
            .threads
            .iter()
            .find(|session| session.id == thread_id)
            .and_then(|session| session.materialized_workspace_name.clone())
            .unwrap_or_else(|| workspace_name_for_thread(&project_id, thread_id));

        if !live_workspaces.iter().any(|live| live == &workspace_name) {
            return (None, false);
        }

        let was_active = self.active_project_id_for_space(&space_id).as_deref()
            == Some(project_id.as_str())
            && self.projects[project_index].active_thread_id.as_deref() == Some(thread_id);
        let mut changed = false;
        let next_thread_id = if was_active {
            let default_project_id = default_project_for_space(&space_id).id;
            if self
                .projects
                .iter()
                .all(|project| project.id != default_project_id || project.space_id != space_id)
            {
                let mut project = default_project_for_space(&space_id);
                let session = WorkspaceThread::new(project.id.clone(), "main".to_string(), None);
                let thread_id = session.id.clone();
                project.active_thread_id = Some(thread_id);
                project.threads.push(session);
                self.projects.push(project);
                changed = true;
            }

            let next_thread_id = {
                let project = self
                    .projects
                    .iter_mut()
                    .find(|project| {
                        project.id == default_project_id && project.space_id == space_id
                    })
                    .expect("default project should exist");
                project.active_thread_id.clone().unwrap_or_else(|| {
                    let session =
                        WorkspaceThread::new(project.id.clone(), "main".to_string(), None);
                    let thread_id = session.id.clone();
                    project.active_thread_id = Some(thread_id.clone());
                    project.threads.push(session);
                    changed = true;
                    thread_id
                })
            };
            changed |= self.set_active_project_for_space(&space_id, default_project_id);
            Some(next_thread_id)
        } else {
            None
        };

        (
            Some(DisconnectedWorkspaceThread {
                was_active,
                next_thread_id,
                workspace_name,
            }),
            changed,
        )
    }

    fn remove_project(&mut self, project_id: &str) -> Option<RemovedProject> {
        let index = self
            .projects
            .iter()
            .position(|project| project.id == project_id)?;
        let space_id = self.projects[index].space_id.clone();
        if self
            .projects
            .iter()
            .filter(|project| project.space_id == space_id)
            .count()
            <= 1
        {
            return None;
        }
        let removed = self.projects.remove(index);
        let was_active = self.active_project_id_for_space(&space_id).as_deref() == Some(project_id);
        let materialized_workspace_names = removed
            .threads
            .iter()
            .filter_map(|session| session.materialized_workspace_name.clone())
            .collect::<Vec<_>>();

        let next_thread_id = if was_active {
            let Some(next_index) = self
                .projects
                .iter()
                .enumerate()
                .filter(|(_, project)| project.space_id == space_id)
                .map(|(idx, _)| idx)
                .find(|idx| *idx >= index)
                .or_else(|| {
                    self.projects
                        .iter()
                        .enumerate()
                        .rev()
                        .find(|(_, project)| project.space_id == space_id)
                        .map(|(idx, _)| idx)
                })
            else {
                return None;
            };
            let project = &mut self.projects[next_index];
            let next_project_id = project.id.clone();
            let thread_id = project
                .active_thread_id
                .clone()
                .or_else(|| {
                    project
                        .threads
                        .iter()
                        .map(|session| session.id.clone())
                        .next()
                })
                .unwrap_or_else(|| {
                    let session =
                        WorkspaceThread::new(project.id.clone(), "main".to_string(), None);
                    let thread_id = session.id.clone();
                    project.threads.push(session);
                    thread_id
                });
            project.active_thread_id = Some(thread_id.clone());
            let _ = project;
            self.set_active_project_for_space(&space_id, next_project_id);
            Some(thread_id)
        } else {
            None
        };

        Some(RemovedProject {
            was_active,
            next_thread_id,
            materialized_workspace_names,
        })
    }

    fn toggle_project_threads_collapsed(&mut self, project_id: &str) -> bool {
        let Some(project) = self
            .projects
            .iter_mut()
            .find(|project| project.id == project_id)
        else {
            return false;
        };
        project.threads_collapsed = !project.threads_collapsed;
        true
    }

    /// Move a project so it renders directly before `before` in its Space
    /// (or last when `before` is None). The projects Vec interleaves every
    /// Space; only the relative order within this Space changes. Returns
    /// false — and leaves the store untouched — for unknown ids, a `before`
    /// from another Space, or a drop on the current position.
    fn move_project_before(
        &mut self,
        space_id: &str,
        project_id: &str,
        before: Option<&str>,
    ) -> bool {
        let Some(from) = self
            .projects
            .iter()
            .position(|project| project.id == project_id && project.space_id == space_id)
        else {
            return false;
        };
        let to =
            match before {
                Some(before_id) => {
                    if before_id == project_id {
                        return false;
                    }
                    let Some(index) = self.projects.iter().position(|project| {
                        project.id == before_id && project.space_id == space_id
                    }) else {
                        return false;
                    };
                    index
                }
                None => {
                    let Some(last) = self
                        .projects
                        .iter()
                        .rposition(|project| project.space_id == space_id)
                    else {
                        return false;
                    };
                    last + 1
                }
            };
        if to == from || to == from + 1 {
            return false;
        }
        let project = self.projects.remove(from);
        let to = if to > from { to - 1 } else { to };
        self.projects.insert(to, project);
        true
    }

    /// Move a thread within its project so it renders directly before
    /// `before` (or last when `before` is None). Pinned threads live in the
    /// sidebar's separate cross-project section, so they are neither movable
    /// here nor valid anchors; inserting before an unpinned sibling's Vec
    /// position keeps the visible (pinned-filtered) order correct even with
    /// pinned entries interleaved in the Vec.
    fn move_thread_before(
        &mut self,
        project_id: &str,
        thread_id: &str,
        before: Option<&str>,
    ) -> bool {
        let Some(project) = self
            .projects
            .iter_mut()
            .find(|project| project.id == project_id)
        else {
            return false;
        };
        let Some(from) = project
            .threads
            .iter()
            .position(|thread| thread.id == thread_id)
        else {
            return false;
        };
        if project.threads[from].is_pinned {
            return false;
        }
        let to = match before {
            Some(before_id) => {
                if before_id == thread_id {
                    return false;
                }
                let Some(index) = project
                    .threads
                    .iter()
                    .position(|thread| thread.id == before_id && !thread.is_pinned)
                else {
                    return false;
                };
                index
            }
            None => project.threads.len(),
        };
        if to == from || to == from + 1 {
            return false;
        }
        let thread = project.threads.remove(from);
        let to = if to > from { to - 1 } else { to };
        project.threads.insert(to, thread);
        true
    }
}

impl WorkspaceThread {
    fn new(project_id: ProjectId, name: String, workspace: Option<String>) -> Self {
        Self {
            id: new_id("thread"),
            name,
            project_id,
            layout: None,
            remote_font_scales: HashMap::new(),
            planned_workspace_name: None,
            materialized_workspace_name: workspace,
            last_active_at: now_ts(),
            is_pinned: false,
            is_unread: false,
            work_is_running: false,
            work_needs_attention: false,
            work_finished_unseen: false,
        }
    }

    fn new_initial(project_id: ProjectId, name: String, workspace: Option<String>) -> Self {
        let id = workspace
            .as_deref()
            .map(|workspace| initial_thread_id_for_workspace(&project_id, workspace))
            .unwrap_or_else(|| new_id("thread"));
        Self {
            id,
            name,
            project_id,
            layout: None,
            remote_font_scales: HashMap::new(),
            planned_workspace_name: None,
            materialized_workspace_name: workspace,
            last_active_at: now_ts(),
            is_pinned: false,
            is_unread: false,
            work_is_running: false,
            work_needs_attention: false,
            work_finished_unseen: false,
        }
    }

    fn work_status(&self) -> WorkspaceThreadWorkStatus {
        if self.work_needs_attention {
            WorkspaceThreadWorkStatus::NeedsAttention
        } else if self.work_is_running {
            WorkspaceThreadWorkStatus::Running
        } else if self.work_finished_unseen {
            WorkspaceThreadWorkStatus::FinishedUnseen
        } else {
            WorkspaceThreadWorkStatus::Idle
        }
    }

    fn observe_work_status(
        &mut self,
        observed: WorkspaceThreadWorkStatus,
    ) -> WorkspaceThreadWorkChange {
        match observed {
            WorkspaceThreadWorkStatus::Running => {
                let should_persist = self.work_finished_unseen;
                let changed =
                    !self.work_is_running || self.work_needs_attention || self.work_finished_unseen;
                self.work_is_running = true;
                self.work_needs_attention = false;
                self.work_finished_unseen = false;
                WorkspaceThreadWorkChange {
                    changed,
                    should_persist,
                    announce: None,
                }
            }
            WorkspaceThreadWorkStatus::NeedsAttention => {
                let changed = !self.work_needs_attention;
                self.work_needs_attention = true;
                WorkspaceThreadWorkChange {
                    changed,
                    should_persist: false,
                    // `changed` is already "this is the first time we have seen
                    // it waiting", which is exactly when it is worth saying.
                    announce: changed.then_some(WorkAnnouncement::NeedsInput),
                }
            }
            WorkspaceThreadWorkStatus::Idle | WorkspaceThreadWorkStatus::FinishedUnseen => {
                let was_running = self.work_is_running;
                let had_attention = self.work_needs_attention;
                let mut should_persist = false;
                let mut finished_now = false;
                if was_running {
                    // The same condition that decides whether this is worth
                    // writing to disk decides whether it is worth announcing:
                    // both mean "it had not already finished".
                    finished_now = !self.work_finished_unseen;
                    should_persist = finished_now;
                    self.work_finished_unseen = true;
                }
                self.work_is_running = false;
                self.work_needs_attention = false;
                WorkspaceThreadWorkChange {
                    changed: was_running || had_attention,
                    should_persist,
                    announce: finished_now.then_some(WorkAnnouncement::Finished),
                }
            }
        }
    }
}

fn valid_font_scale(font_scale: Option<f64>) -> Option<f64> {
    font_scale.filter(|scale| scale.is_finite() && *scale > 0.0)
}

/// Write the store through. Callers hold `THREAD_STORE`, so the snapshot is
/// current and can be stamped here.
fn persist_locked(store: &WorkspaceThreadStore) {
    persist_snapshot(store, THREAD_STORE_WRITES.claim());
}

fn persist_snapshot(store: &WorkspaceThreadStore, seq: u64) {
    // See `STORE_IS_UNREADABLE`: this write is the one that would destroy the
    // file, so it is the one that has to be refused.
    if STORE_IS_UNREADABLE.load(Ordering::Acquire) {
        return;
    }
    let wrote =
        THREAD_STORE_WRITES.write_if_newest(seq, || match save_workspace_thread_store(store) {
            Ok(()) => true,
            Err(err) => {
                log::warn!("failed to save ThinkTerm thread store: {err:#}");
                false
            }
        });
    if wrote {
        publish_thinkterm_session_changed();
    }
}

fn schedule_workspace_thread_store_persist() {
    THREAD_STORE_PERSIST_DIRTY.store(true, Ordering::Release);
    if THREAD_STORE_PERSIST_SCHEDULED.swap(true, Ordering::AcqRel) {
        return;
    }

    std::thread::spawn(|| loop {
        std::thread::sleep(Duration::from_millis(50));

        if THREAD_STORE_PERSIST_DIRTY.swap(false, Ordering::AcqRel) {
            // Stamp the copy while the store is still held: the number has to
            // describe this snapshot, not the moment the write gets around to
            // running.
            let (store, seq) = {
                let store = THREAD_STORE.lock();
                (store.clone(), THREAD_STORE_WRITES.claim())
            };
            persist_snapshot(&store, seq);
            continue;
        }

        THREAD_STORE_PERSIST_SCHEDULED.store(false, Ordering::Release);
        if THREAD_STORE_PERSIST_DIRTY.load(Ordering::Acquire)
            && !THREAD_STORE_PERSIST_SCHEDULED.swap(true, Ordering::AcqRel)
        {
            continue;
        }

        break;
    });
}

fn snapshot_window_layout<F>(
    window_id: MuxWindowId,
    pane_font_scale: &F,
) -> Option<WorkspaceThreadLayoutSnapshot>
where
    F: Fn(PaneId) -> Option<f64>,
{
    let mux = Mux::get();
    let window = mux.get_window(window_id)?;
    let active_tab = window.get_active_idx();
    let mut terminal_specs = vec![];
    let tabs = window
        .iter()
        .filter_map(|tab| {
            let tree = tab.codec_pane_tree();
            collect_terminal_specs(&mux, &tree, &mut terminal_specs, pane_font_scale);
            serde_json::to_value(tree).ok()
        })
        .collect::<Vec<_>>();
    Some(WorkspaceThreadLayoutSnapshot {
        active_tab,
        tabs,
        terminal_specs,
    })
}

pub fn window_layout_structure_fingerprint(window_id: MuxWindowId) -> Option<u64> {
    let mux = Mux::get();
    let window = mux.get_window(window_id)?;
    let mut hasher = DefaultHasher::new();
    window.get_active_idx().hash(&mut hasher);

    let mut tab_count = 0usize;
    for tab in window.iter() {
        tab_count += 1;
        let tree = tab.codec_pane_tree();
        hash_pane_node_structure(&tree, &mut hasher);
    }
    tab_count.hash(&mut hasher);

    Some(hasher.finish())
}

fn hash_pane_node_structure<H: Hasher>(node: &PaneNode, hasher: &mut H) {
    match node {
        PaneNode::Empty => {
            0u8.hash(hasher);
        }
        PaneNode::Leaf(entry) => {
            1u8.hash(hasher);
            entry.pane_id.hash(hasher);
        }
        PaneNode::Stack(stack) => {
            2u8.hash(hasher);
            stack.active.hash(hasher);
            stack.panes.len().hash(hasher);
            for entry in &stack.panes {
                entry.pane_id.hash(hasher);
            }
        }
        PaneNode::Split { left, right, node } => {
            3u8.hash(hasher);
            match node.direction {
                SplitDirection::Horizontal => 0u8.hash(hasher),
                SplitDirection::Vertical => 1u8.hash(hasher),
            }
            hash_pane_node_structure(left, hasher);
            hash_pane_node_structure(right, hasher);
        }
    }
}

fn pane_font_scales_for_window(
    layout: &WorkspaceThreadLayoutSnapshot,
    window_id: MuxWindowId,
) -> Option<HashMap<PaneId, Option<f64>>> {
    let mux = Mux::get();
    let window = mux.get_window(window_id)?;
    let spec_scales = layout
        .terminal_specs
        .iter()
        .map(|entry| (entry.pane_id, valid_font_scale(entry.font_scale)))
        .collect::<HashMap<_, _>>();
    let mut font_scales = HashMap::new();
    let mut decoded_tabs = 0usize;

    for (stored_tab, live_tab) in layout.tabs.iter().zip(window.iter()) {
        let Ok(stored_node) = serde_json::from_value::<PaneNode>(stored_tab.clone()) else {
            continue;
        };
        decoded_tabs += 1;
        let live_node = live_tab.codec_pane_tree();
        collect_matching_font_scales(&stored_node, &live_node, &spec_scales, &mut font_scales);
    }

    if decoded_tabs == 0 && !layout.tabs.is_empty() {
        return None;
    }

    for tab in window.iter() {
        for pos in tab.iter_panes_ignoring_zoom() {
            let pane_id = pos.pane.pane_id();
            if let Some(font_scale) = spec_scales.get(&pane_id) {
                font_scales.insert(pane_id, *font_scale);
            }
        }
    }

    Some(font_scales)
}

fn collect_matching_font_scales(
    stored_node: &PaneNode,
    live_node: &PaneNode,
    spec_scales: &HashMap<PaneId, Option<f64>>,
    font_scales: &mut HashMap<PaneId, Option<f64>>,
) {
    match (stored_node, live_node) {
        (PaneNode::Leaf(stored), PaneNode::Leaf(live)) => {
            font_scales.insert(
                live.pane_id,
                spec_scales.get(&stored.pane_id).copied().unwrap_or(None),
            );
        }
        (PaneNode::Stack(stored), PaneNode::Stack(live)) => {
            for (stored, live) in stored.panes.iter().zip(&live.panes) {
                font_scales.insert(
                    live.pane_id,
                    spec_scales.get(&stored.pane_id).copied().unwrap_or(None),
                );
            }
        }
        (
            PaneNode::Split {
                left: stored_left,
                right: stored_right,
                ..
            },
            PaneNode::Split {
                left: live_left,
                right: live_right,
                ..
            },
        ) => {
            collect_matching_font_scales(stored_left, live_left, spec_scales, font_scales);
            collect_matching_font_scales(stored_right, live_right, spec_scales, font_scales);
        }
        _ => {
            let mut stored_panes = vec![];
            let mut live_panes = vec![];
            collect_pane_entries(stored_node, &mut stored_panes);
            collect_pane_entries(live_node, &mut live_panes);
            for (stored, live) in stored_panes.into_iter().zip(live_panes) {
                font_scales.insert(
                    live.pane_id,
                    spec_scales.get(&stored.pane_id).copied().unwrap_or(None),
                );
            }
        }
    }
}

fn collect_pane_entries<'a>(node: &'a PaneNode, entries: &mut Vec<&'a PaneEntry>) {
    match node {
        PaneNode::Empty => {}
        PaneNode::Leaf(entry) => entries.push(entry),
        PaneNode::Stack(stack) => entries.extend(stack.panes.iter()),
        PaneNode::Split { left, right, .. } => {
            collect_pane_entries(left, entries);
            collect_pane_entries(right, entries);
        }
    }
}

/// Decode a saved layout's tabs, or `None` if this build cannot read it.
///
/// `PaneNode` is the mux's wire type as well as this file's on-disk format.
/// Adding a field to it is free on the wire — the codec version rises and both
/// ends move together — but every layout already on disk was written by an
/// older shape and stays that way forever. So a decode failure here is a
/// routine forward-compatibility event, not a bug in the file, and it must not
/// take the whole Thread down with it.
fn decode_layout_tabs(
    workspace_name: &str,
    layout: &WorkspaceThreadLayoutSnapshot,
) -> Option<Vec<PaneNode>> {
    let mut tabs = Vec::with_capacity(layout.tabs.len());
    for tab_value in &layout.tabs {
        match serde_json::from_value::<PaneNode>(tab_value.clone()) {
            Ok(node) => tabs.push(node),
            Err(err) => {
                log::error!(
                    "cannot read the saved layout for {workspace_name}: {err:#}. \
                     Opening a single terminal instead; the saved layout is left \
                     on disk untouched."
                );
                mark_layout_unreadable(workspace_name);
                return None;
            }
        }
    }
    Some(tabs)
}

async fn materialize_layout(
    mux: Arc<Mux>,
    workspace_name: String,
    layout: WorkspaceThreadLayoutSnapshot,
    tabs: Vec<PaneNode>,
    initial_cwd: Option<String>,
    size: TerminalSize,
    term_config: Arc<dyn TerminalConfiguration>,
) -> Result<()> {
    let terminal_specs = layout
        .terminal_specs
        .iter()
        .map(|entry| (entry.pane_id, entry.spec.clone()))
        .collect::<HashMap<_, _>>();
    let mut window_id = None;
    let mut spawned_tabs = 0usize;
    for node in &tabs {
        let first_entry = first_pane_entry(node);
        let cwd = first_entry
            .and_then(|entry| working_dir_for_entry(entry, &terminal_specs))
            .or_else(|| initial_cwd.clone());
        let domain = first_entry
            .map(|entry| spawn_domain_for_entry(&mux, entry, &terminal_specs, true))
            .unwrap_or(SpawnTabDomain::DefaultDomain);
        let (_tab, pane, win_id) = mux
            .spawn_tab_or_window(
                window_id,
                domain,
                None,
                cwd,
                node.root_size().unwrap_or(size),
                None,
                workspace_name.clone(),
                None,
            )
            .await
            .context("spawn thread tab")?;
        pane.set_config(Arc::clone(&term_config));
        window_id = Some(win_id);
        restore_node(
            Arc::clone(&mux),
            pane.pane_id(),
            node,
            &terminal_specs,
            Arc::clone(&term_config),
        )
        .await?;
        spawned_tabs += 1;
    }

    if let Some(win_id) = window_id {
        if let Some(mut window) = mux.get_window_mut(win_id) {
            if spawned_tabs > 0 {
                window.set_active_without_saving(layout.active_tab.min(spawned_tabs - 1));
            }
        }
    }

    Ok(())
}

fn restore_node<'a>(
    mux: Arc<Mux>,
    base_pane_id: PaneId,
    node: &'a PaneNode,
    terminal_specs: &'a HashMap<PaneId, TerminalSpawnSpec>,
    term_config: Arc<dyn TerminalConfiguration>,
) -> LocalBoxFuture<'a, Result<()>> {
    Box::pin(async move {
        match node {
            PaneNode::Empty | PaneNode::Leaf(_) => {}
            PaneNode::Stack(stack) => {
                restore_stack(mux, base_pane_id, stack, terminal_specs, term_config).await?;
            }
            PaneNode::Split { left, right, node } => {
                let right_entry = first_pane_entry(right);
                let request = SplitRequest {
                    direction: node.direction,
                    target_is_second: true,
                    top_level: false,
                    size: SplitSize::Percent(split_second_percent(
                        node.direction,
                        node.first,
                        node.second,
                    )),
                };
                let (right_pane, _size) = mux
                    .split_pane(
                        base_pane_id,
                        request,
                        SplitSource::Spawn {
                            command: None,
                            command_dir: right_entry
                                .and_then(|entry| working_dir_for_entry(entry, terminal_specs)),
                        },
                        right_entry
                            .map(|entry| spawn_domain_for_entry(&mux, entry, terminal_specs, false))
                            .unwrap_or(SpawnTabDomain::CurrentPaneDomain),
                    )
                    .await
                    .context("restore split pane")?;
                right_pane.set_config(Arc::clone(&term_config));
                restore_node(
                    Arc::clone(&mux),
                    base_pane_id,
                    left,
                    terminal_specs,
                    Arc::clone(&term_config),
                )
                .await?;
                restore_node(
                    mux,
                    right_pane.pane_id(),
                    right,
                    terminal_specs,
                    term_config,
                )
                .await?;
            }
        }
        Ok(())
    })
}

async fn restore_stack(
    mux: Arc<Mux>,
    base_pane_id: PaneId,
    stack: &PaneStackEntry,
    terminal_specs: &HashMap<PaneId, TerminalSpawnSpec>,
    term_config: Arc<dyn TerminalConfiguration>,
) -> Result<()> {
    let mut pane_ids = vec![base_pane_id];
    for entry in stack.panes.iter().skip(1) {
        let pane = mux
            .spawn_pane_in_stack(
                base_pane_id,
                spawn_domain_for_entry(&mux, entry, terminal_specs, false),
                None,
                working_dir_for_entry(entry, terminal_specs),
                entry.size,
            )
            .await
            .context("restore pane-local tab")?;
        pane.set_config(Arc::clone(&term_config));
        pane_ids.push(pane.pane_id());
    }

    if let Some(pane_id) = pane_ids.get(stack.active).copied() {
        let _ = mux.activate_pane_in_stack(pane_id);
    }
    Ok(())
}

fn split_second_percent(
    direction: SplitDirection,
    first: TerminalSize,
    second: TerminalSize,
) -> u8 {
    let (first, second) = match direction {
        SplitDirection::Horizontal => (first.cols, second.cols),
        SplitDirection::Vertical => (first.rows, second.rows),
    };
    let total = first.saturating_add(second).max(1);
    ((second.saturating_mul(100) / total).clamp(5, 95)) as u8
}

fn working_dir_from_entry(entry: &PaneEntry) -> Option<String> {
    entry
        .working_dir
        .as_ref()
        .and_then(|url| url.url.to_file_path().ok())
        .and_then(|path| path.to_str().map(|path| path.to_string()))
}

fn collect_terminal_specs<F>(
    mux: &Mux,
    node: &PaneNode,
    terminal_specs: &mut Vec<TerminalSpecEntry>,
    pane_font_scale: &F,
) where
    F: Fn(PaneId) -> Option<f64>,
{
    match node {
        PaneNode::Empty => {}
        PaneNode::Leaf(entry) => collect_terminal_spec(mux, entry, terminal_specs, pane_font_scale),
        PaneNode::Stack(stack) => {
            for entry in &stack.panes {
                collect_terminal_spec(mux, entry, terminal_specs, pane_font_scale);
            }
        }
        PaneNode::Split { left, right, .. } => {
            collect_terminal_specs(mux, left, terminal_specs, pane_font_scale);
            collect_terminal_specs(mux, right, terminal_specs, pane_font_scale);
        }
    }
}

fn collect_terminal_spec<F>(
    mux: &Mux,
    entry: &PaneEntry,
    terminal_specs: &mut Vec<TerminalSpecEntry>,
    pane_font_scale: &F,
) where
    F: Fn(PaneId) -> Option<f64>,
{
    let domain = mux
        .get_pane(entry.pane_id)
        .and_then(|pane| mux.get_domain(pane.domain_id()))
        .map(|domain| domain.domain_name().to_string());
    terminal_specs.push(TerminalSpecEntry {
        pane_id: entry.pane_id,
        spec: TerminalSpawnSpec {
            cwd: working_dir_from_entry(entry),
            domain,
            title: entry.title.clone(),
        },
        font_scale: valid_font_scale(pane_font_scale(entry.pane_id)),
    });
}

fn first_pane_entry(node: &PaneNode) -> Option<&PaneEntry> {
    match node {
        PaneNode::Empty => None,
        PaneNode::Leaf(entry) => Some(entry),
        PaneNode::Stack(stack) => stack.panes.first(),
        PaneNode::Split { left, .. } => first_pane_entry(left),
    }
}

fn working_dir_for_entry(
    entry: &PaneEntry,
    terminal_specs: &HashMap<PaneId, TerminalSpawnSpec>,
) -> Option<String> {
    terminal_specs
        .get(&entry.pane_id)
        .and_then(|spec| spec.cwd.clone())
        .or_else(|| working_dir_from_entry(entry))
}

fn spawn_domain_for_entry(
    mux: &Mux,
    entry: &PaneEntry,
    terminal_specs: &HashMap<PaneId, TerminalSpawnSpec>,
    default_if_missing: bool,
) -> SpawnTabDomain {
    terminal_specs
        .get(&entry.pane_id)
        .and_then(|spec| spec.domain.as_deref())
        .filter(|domain| mux.get_domain_by_name(domain).is_some())
        .map(|domain| SpawnTabDomain::DomainName(domain.to_string()))
        .unwrap_or(if default_if_missing {
            SpawnTabDomain::DefaultDomain
        } else {
            SpawnTabDomain::CurrentPaneDomain
        })
}

fn project_id_for_path(space_id: &str, path: &Path) -> ProjectId {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in space_id
        .bytes()
        .chain(std::iter::once(0))
        .chain(path.as_os_str().to_string_lossy().bytes())
    {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("project-{hash:x}")
}

fn initial_thread_id_for_workspace(project_id: &str, workspace: &str) -> WorkspaceThreadId {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in project_id
        .bytes()
        .chain(std::iter::once(0))
        .chain(workspace.bytes())
    {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("thread-{hash:x}")
}

fn scan_workspace_work_status(workspace: &str) -> WorkspaceThreadWorkStatus {
    let mux = Mux::get();
    let mut running = false;
    let mut needs_attention = false;
    for window_id in mux.iter_windows_in_workspace(workspace) {
        let Some(window) = mux.get_window(window_id) else {
            continue;
        };
        let tabs = (0..window.len())
            .filter_map(|idx| window.get_by_idx(idx).cloned())
            .collect::<Vec<_>>();
        drop(window);

        for tab in tabs {
            for pane in tab.iter_all_panes() {
                match pane.get_progress() {
                    Progress::None => {}
                    Progress::Percentage(_) | Progress::Indeterminate => running = true,
                    Progress::Error(_) => needs_attention = true,
                }
                if crate::termwindow::ui::status_icon::split_leading_legacy_progress_marker(
                    &pane.get_title(),
                )
                .is_some()
                {
                    running = true;
                }
            }
        }
    }

    if needs_attention {
        WorkspaceThreadWorkStatus::NeedsAttention
    } else if running {
        WorkspaceThreadWorkStatus::Running
    } else {
        WorkspaceThreadWorkStatus::Idle
    }
}

fn is_remote_project(project: &Project, spaces: &[Space]) -> bool {
    project.id.starts_with("ssh-")
        || project.id.starts_with("system-ssh-")
        || project.path.to_string_lossy().starts_with("ssh://")
        || spaces
            .iter()
            .any(|space| space.id == project.space_id && space.client_domain.is_some())
}

fn thread_has_restorable_workspace(thread: &WorkspaceThread) -> bool {
    // Mux-domain threads have no local layout to restore: their content lives
    // on the remote mux server and only materializes through `thinkterm connect`.
    if is_mux_domain_project_id(&thread.project_id) {
        return false;
    }
    thread.layout.is_some() || thread.materialized_workspace_name.is_some()
}

fn normalize_project_path(path: &str) -> Result<PathBuf> {
    let trimmed = path.trim();
    ensure!(!trimmed.is_empty(), "project path is empty");

    let expanded = if trimmed == "~" {
        config::HOME_DIR.to_path_buf()
    } else if let Some(rest) = trimmed.strip_prefix("~/") {
        config::HOME_DIR.join(rest)
    } else {
        let path = PathBuf::from(trimmed);
        if path.is_absolute() {
            path
        } else {
            std::env::current_dir()
                .unwrap_or_else(|_| config::HOME_DIR.to_path_buf())
                .join(path)
        }
    };

    let canonical =
        fs::canonicalize(&expanded).with_context(|| format!("open {}", expanded.display()))?;
    ensure!(
        canonical.is_dir(),
        "project path is not a directory: {}",
        canonical.display()
    );
    Ok(shell_compatible_project_path(canonical))
}

#[cfg(windows)]
fn shell_compatible_project_path(path: PathBuf) -> PathBuf {
    let stripped = {
        let text = path.to_string_lossy();
        strip_windows_verbatim_prefix_text(&text)
    };
    stripped.map(PathBuf::from).unwrap_or(path)
}

#[cfg(not(windows))]
fn shell_compatible_project_path(path: PathBuf) -> PathBuf {
    path
}

#[cfg(any(test, windows))]
fn strip_windows_verbatim_prefix_text(path: &str) -> Option<String> {
    let rest = path.strip_prefix(r"\\?\")?;
    let bytes = rest.as_bytes();
    if bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'\\' | b'/')
    {
        return Some(rest.to_string());
    }
    rest.strip_prefix(r"UNC\")
        .map(|unc_path| format!(r"\\{unc_path}"))
}

fn workspace_name_for_thread(project_id: &str, thread_id: &str) -> String {
    format!("thinkterm:{project_id}:{thread_id}")
}

fn workspace_name_for_remote_default(project_id: &str, thread_id: &str, workspace: &str) -> String {
    format!(
        "{}:{}",
        workspace_name_for_thread(project_id, thread_id),
        workspace
    )
}

pub fn remote_project_id_for_space(space_id: &str, host_id: &str) -> ProjectId {
    format!("{host_id}{REMOTE_PROJECT_SPACE_SEPARATOR}{space_id}")
}

/// The Space a remote project id was minted for, if it carries one. Lets a
/// Space tell a sibling Space's rows on the same server apart from genuinely
/// orphaned ones.
fn space_id_from_remote_project_id(project_id: &str) -> Option<&str> {
    project_id
        .split_once(REMOTE_PROJECT_SPACE_SEPARATOR)
        .map(|(_host, space_id)| space_id)
        .filter(|space_id| !space_id.is_empty())
}

/// Prefix distinguishing mux-client-domain pseudo host ids from the ssh_hosts
/// store's `ssh-`/`system-ssh-` ids, so none of the sidebar SSH machinery
/// fires on them.
const MUX_DOMAIN_HOST_PREFIX: &str = "muxdomain-";

pub fn mux_domain_host_id(domain_name: &str) -> String {
    format!("{MUX_DOMAIN_HOST_PREFIX}{domain_name}")
}

pub fn is_mux_domain_project_id(project_id: &str) -> bool {
    remote_host_id_for_project_id(project_id).starts_with(MUX_DOMAIN_HOST_PREFIX)
}

/// Paths of existing projects in a Space (excluding the mux domain's own
/// `wezterm-mux://` sentinel). Used to offer previously-added remote paths
/// as picker candidates.
pub fn project_paths_for_space(space_id: &str) -> Vec<String> {
    let store = THREAD_STORE.lock();
    store
        .projects
        .iter()
        .filter(|p| p.space_id == space_id)
        .filter_map(|p| {
            let path = p.path.to_string_lossy();
            if path.starts_with("wezterm-mux://") {
                None
            } else {
                Some(path.to_string())
            }
        })
        .collect()
}

/// The mux client domain a Space is dedicated to, if any. Every thread in
/// such a Space targets the remote server: new panes spawn into this domain
/// and project paths refer to the remote filesystem.
pub fn client_domain_for_space(space_id: &str) -> Option<String> {
    let store = THREAD_STORE.lock();
    store
        .spaces
        .iter()
        .find(|space| space.id == space_id)
        .and_then(|space| space.client_domain.clone())
}

/// Parse a thread workspace name (`thinkterm:<project-id>:<thread-id>`,
/// optionally with a `:<remote-workspace>` suffix) back into its identity.
/// Thread ids never contain `:`, project ids may (`::space::`).
fn parse_thread_workspace_name(workspace: &str) -> Option<(String, String)> {
    let rest = workspace.strip_prefix("thinkterm:")?;
    let idx = rest.find(":thread-")?;
    let project_id = &rest[..idx];
    let thread_id = rest[idx + 1..].split(':').next()?;
    if project_id.is_empty() {
        return None;
    }
    Some((project_id.to_string(), thread_id.to_string()))
}

/// Whether the workspace is managed by ThinkTerm's Space/thread store.
///
/// Client domains mirror every server-side workspace into the local mux,
/// including implementation-detail workspaces such as the mux server's
/// startup default window. Keep the recognition rule in one place so GUI
/// reconciliation cannot mistake one of those background mirrors for a
/// ThinkTerm thread window.
pub(crate) fn is_thread_workspace_name(workspace: &str) -> bool {
    parse_thread_workspace_name(workspace).is_some()
}

/// Rebuild sidebar records for live mux windows of this Space's client
/// domain whose thread workspace has no local record. The remote mux server
/// is the source of truth for a mux-domain Space: when the local store lost
/// (or never had) the project/thread entries — a different Space identity
/// for the same server, a reinstalled client, a lost store — the running
/// remote terminals would otherwise be invisible and unreachable from the
/// sidebar. Workspace names embed their identity, so the records can be
/// reconstructed exactly. A project that exists in ANOTHER Space is left
/// alone: its own Space already shows it.
pub fn adopt_orphan_remote_thread_windows(space_id: &str) -> bool {
    let Some(domain_name) = client_domain_for_space(space_id) else {
        return false;
    };
    let mux = Mux::get();
    let Some(domain) = mux.get_domain_by_name(&domain_name) else {
        return false;
    };
    let domain_id = domain.domain_id();

    // Gather candidates outside the store lock.
    let mut candidates: Vec<(String, String, String, Option<String>)> = Vec::new();
    for window_id in mux.iter_windows() {
        let Some(window) = mux.get_window(window_id) else {
            continue;
        };
        if window.origin_domain() != Some(domain_id) {
            continue;
        }
        let workspace = window.get_workspace().to_string();
        let Some((project_id, thread_id)) = parse_thread_workspace_name(&workspace) else {
            continue;
        };
        if candidates.iter().any(|(_, _, ws, _)| ws == &workspace) {
            continue;
        }
        let cwd = window.iter().next().and_then(|tab| {
            tab.iter_panes_ignoring_zoom().first().and_then(|pos| {
                pos.pane
                    .get_current_working_dir(mux::pane::CachePolicy::AllowStale)
                    .map(|url| url.path().to_string())
            })
        });
        candidates.push((project_id, thread_id, workspace, cwd));
    }
    if candidates.is_empty() {
        return false;
    }

    let mux_project_id = remote_project_id_for_space(space_id, &mux_domain_host_id(&domain_name));

    let mut store = THREAD_STORE.lock();
    let mut changed = false;
    for (project_id, thread_id, workspace, cwd) in candidates {
        let workspace_known = store.projects.iter().any(|project| {
            project.threads.iter().any(|thread| {
                thread.materialized_workspace_name.as_deref() == Some(workspace.as_str())
                    || workspace_name_for_thread(&project.id, &thread.id) == workspace
            })
        });
        if workspace_known {
            continue;
        }
        // One server can host several Spaces. A workspace whose embedded Space
        // id names a sibling Space we already know about belongs to that
        // Space, which will show it; adopting it here would move a live
        // terminal out from under it.
        let belongs_to_sibling_space = space_id_from_remote_project_id(&project_id)
            .is_some_and(|sibling| sibling != space_id && store.has_space(sibling));
        match store
            .projects
            .iter_mut()
            .find(|project| project.id == project_id)
        {
            Some(project) => {
                if project.space_id != space_id {
                    continue;
                }
                let mut thread =
                    WorkspaceThread::new(project_id, "main".to_string(), Some(workspace));
                thread.id = thread_id;
                project.threads.push(thread);
                changed = true;
            }
            // Mux-domain workspace names embed a Space id. When that identity
            // no longer matches any local project (the Space was recreated,
            // the store was lost, the domain was relabelled), the session
            // would otherwise keep running invisibly on the server forever.
            // Re-home it as a thread of THIS Space's mux-domain project; the
            // materialized workspace name keeps routing to the live remote
            // workspace.
            //
            None if project_id.contains(REMOTE_PROJECT_SPACE_SEPARATOR)
                && !belongs_to_sibling_space =>
            {
                let thread_id_taken = store
                    .projects
                    .iter()
                    .any(|project| project.threads.iter().any(|thread| thread.id == thread_id));
                if !store
                    .projects
                    .iter()
                    .any(|project| project.id == mux_project_id)
                {
                    store.projects.push(Project {
                        id: mux_project_id.clone(),
                        space_id: space_id.to_string(),
                        name: domain_name.clone(),
                        path: PathBuf::from(format!("wezterm-mux://{domain_name}")),
                        threads: vec![],
                        active_thread_id: None,
                        threads_collapsed: false,
                        active_note_path: None,
                    });
                }
                let project = store
                    .projects
                    .iter_mut()
                    .find(|project| project.id == mux_project_id)
                    .expect("mux domain project was just ensured");
                let mut thread = WorkspaceThread::new(
                    mux_project_id.clone(),
                    "main".to_string(),
                    Some(workspace),
                );
                if !thread_id_taken {
                    thread.id = thread_id;
                }
                project.threads.push(thread);
                changed = true;
            }
            None => {
                let path = cwd
                    .filter(|p| !p.is_empty())
                    .unwrap_or_else(|| "~".to_string());
                let name = Path::new(&path)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .filter(|n| !n.is_empty())
                    .unwrap_or("Recovered")
                    .to_string();
                let mut thread =
                    WorkspaceThread::new(project_id.clone(), "main".to_string(), Some(workspace));
                thread.id = thread_id.clone();
                store.projects.push(Project {
                    id: project_id,
                    space_id: space_id.to_string(),
                    name,
                    path: PathBuf::from(path),
                    threads: vec![thread],
                    active_thread_id: Some(thread_id),
                    threads_collapsed: false,
                    active_note_path: None,
                });
                changed = true;
            }
        }
    }
    if changed {
        store.ensure_unique_thread_names();
        persist_locked(&store);
    }
    drop(store);
    if changed {
        // Rows rebuilt from live remote windows are new to the server too
        // whenever it was the server's tree that went missing.
        reconcile_remote_subtree(&domain_name);
    }
    changed
}

/// Create a project in a mux-domain Space. The path names a directory on the
/// remote server, so it must not be resolved or canonicalized locally.
pub fn create_remote_project_from_path(space_id: &str, path: &str) -> Result<WorkspaceThreadId> {
    let trimmed = path.trim();
    ensure!(!trimmed.is_empty(), "project path is empty");
    ensure!(
        trimmed == "~" || trimmed.starts_with("~/") || trimmed.starts_with('/'),
        "remote project path must be absolute or start with ~: {trimmed}"
    );
    let mut store = THREAD_STORE.lock();
    let domain = tree_domain_for_space(&store, space_id)
        .with_context(|| format!("Space {space_id} is not a remote Space"))?;
    ensure!(
        remote_tree_mutation_allowed(Some(&domain)),
        "{domain} is disconnected"
    );
    store.normalize_after_load();
    let thread_id = store.create_project_from_path(space_id, PathBuf::from(trimmed));
    persist_locked(&store);
    drop(store);
    // create_project_from_path either adds a project and its first thread or
    // reuses an existing project for the same path; reconciling covers both.
    reconcile_remote_subtree(&domain);
    Ok(thread_id)
}

/// Everything `thinkterm connect` needs to route a client-domain attach into
/// its dedicated Space.
#[derive(Debug, Clone)]
pub struct MuxDomainSpacePlan {
    pub space_id: SpaceId,
    pub project_id: ProjectId,
    pub thread_id: WorkspaceThreadId,
}

/// Find-or-create the dedicated Space for a mux client domain, along with its
/// single project and "main" thread. Idempotent: reconnecting converges on the
/// same records.
pub fn ensure_mux_domain_space(domain_name: &str) -> MuxDomainSpacePlan {
    let mut store = THREAD_STORE.lock();
    let plan = store.ensure_mux_domain_space(domain_name);
    persist_locked(&store);
    plan
}

impl WorkspaceThreadStore {
    fn ensure_mux_domain_space(&mut self, domain_name: &str) -> MuxDomainSpacePlan {
        let store = self;

        // A server can host several Spaces. Land on the one this device was last
        // using; failing that its first Space; and only create one when we have
        // never connected (or the server told us it has none).
        let space_id = store
            .preferred_space_for_domain(domain_name)
            .unwrap_or_else(|| {
                store.create_space_record_for_domain(
                    domain_name.to_string(),
                    Some(domain_name.to_string()),
                )
            });

        let host_id = mux_domain_host_id(domain_name);
        let project_id = remote_project_id_for_space(&space_id, &host_id);
        if !store.projects.iter().any(|p| p.id == project_id) {
            store.projects.push(Project {
                id: project_id.clone(),
                space_id: space_id.clone(),
                name: domain_name.to_string(),
                path: PathBuf::from(format!("wezterm-mux://{domain_name}")),
                threads: vec![],
                active_thread_id: None,
                threads_collapsed: false,
                active_note_path: None,
            });
        }

        let project = store
            .projects
            .iter_mut()
            .find(|p| p.id == project_id)
            .expect("mux domain project was just ensured");
        if project.threads.is_empty() {
            project.threads.push(WorkspaceThread::new(
                project_id.clone(),
                "main".to_string(),
                None,
            ));
        }
        let thread_id = project
            .active_thread_id
            .clone()
            .filter(|id| project.threads.iter().any(|t| &t.id == id))
            .unwrap_or_else(|| project.threads[0].id.clone());
        project.active_thread_id = Some(thread_id.clone());

        if let Some(space) = store.spaces.iter_mut().find(|space| space.id == space_id) {
            space.active_project_id = Some(project_id.clone());
        }

        MuxDomainSpacePlan {
            space_id,
            project_id,
            thread_id,
        }
    }
}

/// Resolve (and record) the workspace that a mux-domain Space's thread
/// lives in. The connect window is created directly in this workspace so
/// that thread switching can find it again by name instead of
/// materializing a duplicate; recording it as materialized keeps
/// reconnects and switch-backs symmetric.
pub fn ensure_mux_thread_workspace(plan: &MuxDomainSpacePlan) -> String {
    let mut store = THREAD_STORE.lock();
    let mut name = None;
    if let Some(project) = store
        .projects
        .iter_mut()
        .find(|project| project.id == plan.project_id)
    {
        if let Some(thread) = project
            .threads
            .iter_mut()
            .find(|thread| thread.id == plan.thread_id)
        {
            let workspace = thread
                .materialized_workspace_name
                .clone()
                .or_else(|| thread.planned_workspace_name.clone())
                .unwrap_or_else(|| workspace_name_for_thread(&plan.project_id, &plan.thread_id));
            thread.materialized_workspace_name = Some(workspace.clone());
            name = Some(workspace);
        }
    }
    persist_locked(&store);
    submit_thread_state(&store, &plan.thread_id);
    name.unwrap_or_else(|| workspace_name_for_thread(&plan.project_id, &plan.thread_id))
}

pub fn remote_host_id_for_project_id(project_id: &str) -> &str {
    project_id
        .split_once(REMOTE_PROJECT_SPACE_SEPARATOR)
        .map(|(host_id, _)| host_id)
        .unwrap_or(project_id)
}

fn next_space_name(spaces: &[Space]) -> String {
    let mut index = spaces.len() + 1;
    loop {
        let name = format!("Space {index}");
        if !spaces.iter().any(|space| space.name == name) {
            return name;
        }
        index += 1;
    }
}

fn thread_name_in_use(project: &Project, ignored_thread_id: Option<&str>, name: &str) -> bool {
    project
        .threads
        .iter()
        .any(|thread| ignored_thread_id != Some(thread.id.as_str()) && thread.name == name)
}

fn next_thread_name(project: &Project) -> String {
    let mut index = project.threads.len().saturating_add(1).max(1);
    loop {
        let name = format!("Thread {index}");
        if !thread_name_in_use(project, None, &name) {
            return name;
        }
        index += 1;
    }
}

fn unique_thread_name(
    project: &Project,
    ignored_thread_id: Option<&str>,
    requested: &str,
) -> String {
    let requested = requested.trim();
    if requested.is_empty() {
        return next_thread_name(project);
    }
    if !thread_name_in_use(project, ignored_thread_id, requested) {
        return requested.to_string();
    }

    let mut index = 2;
    loop {
        let name = format!("{requested} {index}");
        if !thread_name_in_use(project, ignored_thread_id, &name) {
            return name;
        }
        index += 1;
    }
}

/// Replace every element matching `is_replaced` with `replacement`, as one
/// block positioned where the first match was. Used to swap a mux domain's
/// rows for the server's version without disturbing the ordering of anything
/// around them (other Spaces, local projects).
fn splice_in_place<T: Clone>(
    current: &[T],
    replacement: Vec<T>,
    is_replaced: impl Fn(&T) -> bool,
) -> Vec<T> {
    let mut out = Vec::with_capacity(current.len() + replacement.len());
    let mut spliced = false;
    for item in current {
        if is_replaced(item) {
            if !spliced {
                out.extend(replacement.iter().cloned());
                spliced = true;
            }
            continue;
        }
        out.push(item.clone());
    }
    if !spliced {
        out.extend(replacement);
    }
    out
}

fn new_id(prefix: &str) -> String {
    format!(
        "{prefix}-{}-{:08x}",
        Utc::now().timestamp_millis(),
        fastrand::u32(..)
    )
}

fn now_ts() -> i64 {
    Utc::now().timestamp()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_owner_lookup_tracks_claims_without_changing_them() {
        let assignments = HashMap::from([(11, "space-a".to_string()), (22, "space-b".to_string())]);
        assert_eq!(window_owner_for_space_in(&assignments, "space-a"), Some(11));
        assert_eq!(window_owner_for_space_in(&assignments, "space-b"), Some(22));
        assert_eq!(window_owner_for_space_in(&assignments, "space-c"), None);
        assert_eq!(assignments.len(), 2);
    }
    use tempfile::tempdir;

    fn test_store() -> WorkspaceThreadStore {
        let mut store = WorkspaceThreadStore::default();
        store.normalize_after_load();
        store
    }

    fn space_view(id: &str, is_remote: bool, is_occupied_by_other_window: bool) -> SpaceView {
        SpaceView {
            id: id.to_string(),
            name: id.to_string(),
            is_active: false,
            is_default: false,
            is_occupied_by_other_window,
            is_remote,
            domain: is_remote.then(|| "server".to_string()),
            is_domain_attached: false,
        }
    }

    fn attached_remote_space_view(id: &str) -> SpaceView {
        SpaceView {
            is_domain_attached: true,
            ..space_view(id, true, false)
        }
    }

    /// Regression: the debounced writer copies the store, releases the store
    /// lock, then spends ~88ms writing. A synchronous save inside that window
    /// used to be silently undone -- the older copy finished last and its
    /// rename replaced the newer file, so a project rename made just after a
    /// Space switch was gone on restart.
    #[test]
    fn a_snapshot_overtaken_before_it_reaches_disk_is_dropped_not_applied() {
        let gate = StoreWriteGate::new();
        let mut written = vec![];

        // The debounced writer stamps its copy...
        let stale = gate.claim();
        // ...then a rename mutates the store and writes through, beating it.
        let fresh = gate.claim();
        assert!(gate.write_if_newest(fresh, || {
            written.push(fresh);
            true
        }));
        // The older copy must not now replace it.
        assert!(
            !gate.write_if_newest(stale, || {
                written.push(stale);
                true
            }),
            "the stale snapshot would overwrite the rename that beat it to disk"
        );
        assert_eq!(written, vec![fresh]);

        // A later snapshot still gets through.
        let later = gate.claim();
        assert!(gate.write_if_newest(later, || {
            written.push(later);
            true
        }));
        assert_eq!(written, vec![fresh, later]);
    }

    #[test]
    fn adjacent_space_swipe_targets_keep_local_order_and_skip_unavailable_spaces() {
        let spaces = vec![
            space_view("local-1", false, false),
            space_view("remote", true, false),
            space_view("occupied", false, true),
            space_view("local-2", false, false),
            space_view("local-3", false, false),
        ];

        assert_eq!(
            adjacent_swipe_space_ids(&spaces, "local-2"),
            (Some("local-1".into()), Some("local-3".into()))
        );
        assert_eq!(
            adjacent_swipe_space_ids(&spaces, "local-1"),
            (None, Some("local-2".into()))
        );
        assert_eq!(
            adjacent_swipe_space_ids(&spaces, "local-3"),
            (Some("local-2".into()), None)
        );
        assert_eq!(adjacent_swipe_space_ids(&spaces, "remote"), (None, None));
    }

    #[test]
    fn adjacent_space_swipe_targets_include_attached_remote_spaces() {
        let spaces = vec![
            space_view("local-1", false, false),
            attached_remote_space_view("attached"),
            space_view("offline", true, false),
            space_view("local-2", false, false),
        ];

        // The attached domain sits in the swipe order; the offline one is
        // skipped over as if it were not there.
        assert_eq!(
            adjacent_swipe_space_ids(&spaces, "local-1"),
            (None, Some("attached".into()))
        );
        assert_eq!(
            adjacent_swipe_space_ids(&spaces, "attached"),
            (Some("local-1".into()), Some("local-2".into()))
        );
        assert_eq!(
            adjacent_swipe_space_ids(&spaces, "local-2"),
            (Some("attached".into()), None)
        );
        // Standing on an unreachable Space starts nothing.
        assert_eq!(adjacent_swipe_space_ids(&spaces, "offline"), (None, None));
    }

    /// A store that fails to parse must not come back as an empty one that
    /// then overwrites the file it could not read. The parse is strict, so the
    /// next field added to `Space`, `Project` or `WorkspaceThread` without a
    /// default lands here — and unlike a layout snapshot, what it would take
    /// with it is every Space, Project and Thread the user has.
    #[test]
    fn a_store_that_cannot_be_parsed_is_an_error_rather_than_an_empty_store() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("workspace_threads.json");

        // A thread as an older build wrote it, missing a field a later one
        // made required.
        std::fs::write(
            &path,
            serde_json::json!({
                "spaces": [{"id": "space-1", "name": "Default", "active_project_id": null}],
                "projects": [{
                    "id": "project-1",
                    "space_id": "space-1",
                    "name": "thinkterm",
                    "path": "/tmp/thinkterm",
                    "active_thread_id": null,
                    "threads": [{
                        "id": "thread-1",
                        "name": "main",
                        "project_id": "project-1",
                        "layout": null,
                        "materialized_workspace_name": null
                        // last_active_at, which the struct requires, is absent
                    }]
                }]
            })
            .to_string(),
        )
        .unwrap();

        let err = load_workspace_thread_store_from_path(&path)
            .expect_err("a store missing a required field must not parse as empty");
        assert!(
            format!("{err:#}").contains("last_active_at"),
            "the error should name the field: {err:#}"
        );

        // And the file is still there to be recovered from.
        assert!(path.exists());
    }

    /// `WorkspaceThreadLayoutSnapshot::tabs` holds serialized `PaneNode`s, so
    /// that type is this file's on-disk format as much as it is the mux's wire
    /// format. A field added to it without a default invalidates every layout
    /// a user has ever saved: `materialize_layout` fails to decode, the Thread
    /// comes up as a single empty pane, and the next snapshot writes that over
    /// the layout it could not read. This is the shape of a tab saved before
    /// `PaneEntry::alt_screen` existed.
    #[test]
    fn layout_snapshot_saved_before_alt_screen_still_decodes() {
        let stored = serde_json::json!({
            "Leaf": {
                "window_id": 1,
                "tab_id": 1,
                "pane_id": 6,
                "title": "zsh",
                "size": {"rows": 24, "cols": 80, "pixel_width": 0, "pixel_height": 0, "dpi": 0},
                "working_dir": "file:///Users/someone/project",
                "is_active_pane": true,
                "is_zoomed_pane": false,
                "workspace": "default",
                "cursor_pos": {"x": 0, "y": 0, "shape": "Default", "visibility": "Visible"},
                "physical_top": 0,
                "top_row": 0,
                "left_col": 0,
                "tty_name": null
            }
        });

        let node: PaneNode =
            serde_json::from_value(stored).expect("decode a layout saved before alt_screen");
        let PaneNode::Leaf(entry) = node else {
            panic!("expected a leaf");
        };
        assert_eq!(entry.pane_id, 6);
        assert!(!entry.alt_screen);
    }

    /// The next field added to `PaneEntry` without a default will land here
    /// again. When it does, the Thread must still open — and the arrangement
    /// we could not read must still be on disk afterwards, so that a build
    /// which can read it gets it back.
    #[test]
    fn an_undecodable_layout_is_refused_rather_than_overwritten() {
        let workspace = "test-workspace-undecodable-layout";
        let layout = WorkspaceThreadLayoutSnapshot {
            active_tab: 0,
            tabs: vec![serde_json::json!({"Leaf": {"a field from the future": true}})],
            terminal_specs: vec![],
        };

        assert!(!layout_is_unreadable(workspace));
        // No tabs to spawn from, so `materialize_thread` falls through to the
        // plain project-directory spawn instead of failing the Thread.
        assert!(decode_layout_tabs(workspace, &layout).is_none());
        // And the single pane that spawn produces must not be saved over it.
        assert!(layout_is_unreadable(workspace));
    }

    fn test_project(id: &str, name: &str, path: PathBuf, threads: Vec<WorkspaceThread>) -> Project {
        test_project_in_space(&default_space_id(), id, name, path, threads)
    }

    fn test_project_in_space(
        space_id: &str,
        id: &str,
        name: &str,
        path: PathBuf,
        threads: Vec<WorkspaceThread>,
    ) -> Project {
        Project {
            id: id.to_string(),
            space_id: space_id.to_string(),
            name: name.to_string(),
            path,
            threads,
            active_thread_id: None,
            threads_collapsed: false,
            active_note_path: None,
        }
    }

    #[test]
    fn vault_binding_and_project_note_paths_round_trip() {
        let vault = SpaceVaultBinding {
            root: PathBuf::from("/tmp/ThinkTerm Notes"),
            managed: false,
        };
        let mut store = test_store();
        let space_id = store.spaces[0].id.clone();
        store.spaces[0].note_vault = Some(vault.clone());
        let mut first = test_project_in_space(
            &space_id,
            "project-first",
            "First",
            PathBuf::from("/tmp/first"),
            vec![],
        );
        first.active_note_path = Some("Design/Overview.md".to_string());
        let mut second = test_project_in_space(
            &space_id,
            "project-second",
            "Second",
            PathBuf::from("/tmp/second"),
            vec![],
        );
        second.active_note_path = Some("Daily/Today.md".to_string());
        store.projects.extend([first, second]);

        let encoded = serde_json::to_string(&store).expect("serialize store");
        let decoded: WorkspaceThreadStore =
            serde_json::from_str(&encoded).expect("deserialize store");

        assert_eq!(decoded.spaces[0].note_vault.as_ref(), Some(&vault));
        assert_eq!(
            decoded.projects[0].active_note_path.as_deref(),
            Some("Design/Overview.md")
        );
        assert_eq!(
            decoded.projects[1].active_note_path.as_deref(),
            Some("Daily/Today.md")
        );
    }

    #[test]
    fn vault_markdown_paths_are_portable_and_cannot_escape() {
        assert_eq!(
            normalize_vault_markdown_path("./Design/Overview.MD").unwrap(),
            "Design/Overview.MD"
        );
        assert!(normalize_vault_markdown_path("../outside.md").is_err());
        assert!(normalize_vault_markdown_path("/absolute.md").is_err());
        assert!(normalize_vault_markdown_path("image.png").is_err());
    }

    #[test]
    fn normalize_makes_thread_names_unique_per_project() {
        let mut store = test_store();
        store.projects.push(test_project(
            "project-1",
            "Project",
            PathBuf::from("/tmp/project"),
            vec![
                WorkspaceThread::new("project-1".to_string(), "Thread 2".to_string(), None),
                WorkspaceThread::new("project-1".to_string(), "Thread 2".to_string(), None),
                WorkspaceThread::new("project-1".to_string(), "Thread 3".to_string(), None),
            ],
        ));

        assert!(store.normalize_after_load());
        let names = store.projects[0]
            .threads
            .iter()
            .map(|thread| thread.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["Thread 2", "Thread 4", "Thread 3"]);
    }

    #[test]
    fn create_thread_skips_existing_default_thread_names() {
        let mut store = test_store();
        store.projects.push(test_project(
            "project-1",
            "Project",
            PathBuf::from("/tmp/project"),
            vec![
                WorkspaceThread::new("project-1".to_string(), "Thread 1".to_string(), None),
                WorkspaceThread::new("project-1".to_string(), "Thread 3".to_string(), None),
            ],
        ));

        let thread_id = store.create_thread("project-1", None);
        let thread = store.projects[0]
            .threads
            .iter()
            .find(|thread| thread.id == thread_id)
            .expect("created thread");
        assert_eq!(thread.name, "Thread 4");
    }

    #[test]
    fn rename_thread_keeps_names_unique_in_project() {
        let mut store = test_store();
        let first = WorkspaceThread::new("project-1".to_string(), "Build".to_string(), None);
        let second = WorkspaceThread::new("project-1".to_string(), "Review".to_string(), None);
        let second_id = second.id.clone();
        store.projects.push(test_project(
            "project-1",
            "Project",
            PathBuf::from("/tmp/project"),
            vec![first, second],
        ));

        assert!(store.rename_thread(&second_id, "Build".to_string()));
        let renamed = store.projects[0]
            .threads
            .iter()
            .find(|thread| thread.id == second_id)
            .expect("renamed thread");
        assert_eq!(renamed.name, "Build 2");
    }

    #[test]
    fn normalize_empty_space_without_active_project_is_stable() {
        let mut store = test_store();
        let space_id = store.create_space_record("Empty".to_string());

        assert!(!store.normalize_after_load());
        assert_eq!(
            store
                .spaces
                .iter()
                .find(|space| space.id == space_id)
                .unwrap()
                .active_project_id,
            None
        );
    }

    #[test]
    fn materializing_layout_guard_is_scoped_to_workspace() {
        let workspace = new_id("restoring-workspace");
        let other_workspace = new_id("other-workspace");

        assert!(!is_materializing_thread_layout(&workspace));
        {
            let _guard = MaterializeThreadLayoutGuard::new(workspace.clone());
            assert!(is_materializing_thread_layout(&workspace));
            assert!(!is_materializing_thread_layout(&other_workspace));

            {
                let _nested_guard = MaterializeThreadLayoutGuard::new(workspace.clone());
                assert!(is_materializing_thread_layout(&workspace));
                assert!(!is_materializing_thread_layout(&other_workspace));
            }

            assert!(is_materializing_thread_layout(&workspace));
        }
        assert!(!is_materializing_thread_layout(&workspace));
    }

    #[test]
    fn claim_available_space_prefers_unoccupied_existing_space() {
        let mut store = test_store();
        let default_space = default_space_id();
        let second_space = store.create_space_record("Second".to_string());
        let third_space = store.create_space_record("Third".to_string());
        store.last_active_space_id = Some(second_space.clone());
        let occupied: std::collections::HashSet<SpaceId> =
            vec![default_space, third_space].into_iter().collect();

        let claimed = store.claim_available_space_id(&occupied);

        assert_eq!(claimed, second_space);
        assert_eq!(store.spaces.len(), 3);
    }

    #[test]
    fn claim_available_space_creates_when_all_spaces_are_occupied() {
        let mut store = test_store();
        let default_space = default_space_id();
        let second_space = store.create_space_record("Second".to_string());
        let occupied: std::collections::HashSet<SpaceId> =
            vec![default_space, second_space].into_iter().collect();

        let claimed = store.claim_available_space_id(&occupied);

        assert!(!occupied.contains(&claimed));
        assert!(store.has_space(&claimed));
        assert_eq!(store.spaces.len(), 3);
    }

    #[test]
    fn empty_space_has_no_thread_to_restore() {
        let mut store = test_store();
        let space_id = store.create_space_record("Empty".to_string());

        let (thread_id, changed) = store.thread_to_restore_for_space(&space_id);

        assert_eq!(thread_id, None);
        assert!(!changed);
        assert!(store
            .projects
            .iter()
            .all(|project| project.space_id != space_id));
    }

    #[test]
    fn unbound_thread_without_layout_is_not_restored() {
        let mut store = test_store();
        let space_id = store.create_space_record("Fresh".to_string());
        let thread = WorkspaceThread::new("project-fresh".to_string(), "main".to_string(), None);
        let thread_id = thread.id.clone();
        let mut project = test_project_in_space(
            &space_id,
            "project-fresh",
            "Fresh",
            PathBuf::from("/tmp/fresh"),
            vec![thread],
        );
        project.active_thread_id = Some(thread_id);
        store.projects.push(project);
        store.set_active_project_for_space(&space_id, "project-fresh".to_string());

        let (thread_id, changed) = store.thread_to_restore_for_space(&space_id);

        assert_eq!(thread_id, None);
        assert!(!changed);
    }

    #[test]
    fn saved_layout_thread_is_restored() {
        let mut store = test_store();
        let space_id = store.create_space_record("Saved".to_string());
        let mut thread =
            WorkspaceThread::new("project-saved".to_string(), "main".to_string(), None);
        let thread_id = thread.id.clone();
        thread.layout = Some(WorkspaceThreadLayoutSnapshot {
            active_tab: 0,
            tabs: vec![serde_json::json!({"kind": "saved"})],
            terminal_specs: vec![],
        });
        let project = test_project_in_space(
            &space_id,
            "project-saved",
            "Saved",
            PathBuf::from("/tmp/saved"),
            vec![thread],
        );
        store.projects.push(project);

        let (restored_thread_id, changed) = store.thread_to_restore_for_space(&space_id);

        assert_eq!(restored_thread_id.as_deref(), Some(thread_id.as_str()));
        assert!(changed);
        assert_eq!(
            store.active_project_id_for_space(&space_id).as_deref(),
            Some("project-saved")
        );
        assert_eq!(
            store.projects[0].active_thread_id.as_deref(),
            Some(thread_id.as_str())
        );
    }

    #[test]
    fn thread_id_for_workspace_matches_materialized_or_expected_name() {
        let mut store = test_store();
        let space_id = store.create_space_record("Remote".to_string());
        let mut thread = WorkspaceThread::new("ssh-host".to_string(), "main".to_string(), None);
        let thread_id = thread.id.clone();
        let expected_workspace = workspace_name_for_thread("ssh-host", &thread_id);
        thread.materialized_workspace_name = Some("custom-remote".to_string());
        store.projects.push(test_project_in_space(
            &space_id,
            "ssh-host",
            "Remote",
            PathBuf::from("ssh://user@example.com"),
            vec![thread],
        ));

        assert_eq!(
            store
                .thread_id_for_workspace(&space_id, "custom-remote")
                .as_deref(),
            Some(thread_id.as_str())
        );
        assert_eq!(
            store
                .thread_id_for_workspace(&space_id, &expected_workspace)
                .as_deref(),
            Some(thread_id.as_str())
        );
        assert_eq!(
            store.thread_id_for_workspace("other-space", "custom-remote"),
            None
        );
    }

    #[test]
    fn workspace_thread_store_round_trip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("workspace_threads.json");
        let mut store = test_store();
        let project = test_project(
            "project-1",
            "thinkterm",
            PathBuf::from("/tmp/thinkterm"),
            vec![WorkspaceThread::new(
                "project-1".to_string(),
                "main".to_string(),
                None,
            )],
        );
        store.set_active_project_for_space(&default_space_id(), project.id.clone());
        store.projects.push(project);
        save_workspace_thread_store_to_path(&path, &store).unwrap();
        let loaded = load_workspace_thread_store_from_path(&path).unwrap();
        assert_eq!(
            loaded
                .active_project_id_for_space(&default_space_id())
                .as_deref(),
            Some("project-1")
        );
        assert_eq!(loaded.projects[0].path, PathBuf::from("/tmp/thinkterm"));
        assert_eq!(loaded.projects[0].space_id, default_space_id());
        assert_eq!(loaded.projects[0].threads[0].name, "main");
    }

    #[test]
    fn legacy_archived_threads_are_dropped_on_load() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("workspace_threads.json");
        let legacy = serde_json::json!({
            "active_project_id": "project-1",
            "projects": [
                {
                    "id": "project-1",
                    "name": "thinkterm",
                    "path": "/tmp/thinkterm",
                    "threads": [
                        {
                            "id": "thread-archived",
                            "name": "Archived Thread",
                            "project_id": "project-1",
                            "layout": null,
                            "materialized_workspace_name": null,
                            "last_active_at": 1,
                            "is_pinned": false,
                            "is_unread": false,
                            "work_finished_unseen": false,
                            "archived": true
                        },
                        {
                            "id": "thread-live",
                            "name": "Live Thread",
                            "project_id": "project-1",
                            "layout": null,
                            "materialized_workspace_name": null,
                            "last_active_at": 2,
                            "is_pinned": false,
                            "is_unread": false,
                            "work_finished_unseen": false,
                            "archived": false
                        }
                    ],
                    "active_thread_id": "thread-archived",
                    "threads_collapsed": false
                }
            ]
        });
        std::fs::write(&path, serde_json::to_string_pretty(&legacy).unwrap()).unwrap();

        let loaded = load_workspace_thread_store_from_path(&path).unwrap();
        assert_eq!(loaded.projects[0].threads.len(), 1);
        assert_eq!(loaded.projects[0].space_id, default_space_id());
        assert_eq!(loaded.projects[0].threads[0].id, "thread-live");
        assert_eq!(
            loaded.projects[0].active_thread_id.as_deref(),
            Some("thread-live")
        );

        save_workspace_thread_store_to_path(&path, &loaded).unwrap();
        let json = std::fs::read_to_string(&path).unwrap();
        assert!(!json.contains("archived"));
    }

    #[test]
    fn running_work_state_is_runtime_only() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("workspace_threads.json");
        let mut store = test_store();
        let mut session = WorkspaceThread::new("project-1".to_string(), "main".to_string(), None);
        session.work_is_running = true;
        store.projects.push(test_project(
            "project-1",
            "thinkterm",
            PathBuf::from("/tmp/thinkterm"),
            vec![session],
        ));

        save_workspace_thread_store_to_path(&path, &store).unwrap();
        let json = std::fs::read_to_string(&path).unwrap();
        assert!(!json.contains("work_is_running"));

        let legacy_json = json.replace(
            "\"work_finished_unseen\": false",
            "\"work_is_running\": true,\n          \"work_finished_unseen\": false",
        );
        assert_ne!(legacy_json, json);
        std::fs::write(&path, legacy_json).unwrap();

        let loaded = load_workspace_thread_store_from_path(&path).unwrap();
        assert!(!loaded.projects[0].threads[0].work_is_running);
        assert!(!loaded.projects[0].threads[0].work_finished_unseen);
    }

    #[test]
    fn synthetic_current_project_matches_ensured_project_ids() {
        let space_id = default_space_id();
        let workspace = "workspace-1";
        let synthetic = current_project_for_workspace(&space_id, workspace);
        let synthetic_thread_id = synthetic.threads[0].id.clone();

        let mut store = test_store();
        let (project_id, changed) = store.ensure_current_project(&space_id, workspace);

        assert!(changed);
        assert_eq!(project_id, synthetic.id);
        assert_eq!(store.projects[0].space_id, space_id);
        assert_eq!(store.projects[0].threads[0].id, synthetic_thread_id);
        assert_eq!(
            store.projects[0].threads[0]
                .materialized_workspace_name
                .as_deref(),
            Some(workspace)
        );
    }

    #[test]
    fn remote_thread_connection_state_does_not_select_or_materialize() {
        let space_id = default_space_id();
        let mut store = test_store();
        let thread = WorkspaceThread::new("ssh-host".to_string(), "main".to_string(), None);
        let thread_id = thread.id.clone();
        store.projects.push(Project {
            id: "ssh-host".to_string(),
            space_id: space_id.clone(),
            name: "Remote".to_string(),
            path: PathBuf::from("ssh://user@example.com"),
            threads: vec![thread],
            active_thread_id: None,
            threads_collapsed: false,
            active_note_path: None,
        });

        let state = store
            .thread_connection_state(&thread_id, &[])
            .expect("remote thread state");
        assert_eq!(state.space_id, space_id);
        assert!(state.is_remote);
        assert!(!state.is_live);
        assert_eq!(
            state.workspace_name,
            workspace_name_for_thread("ssh-host", &thread_id)
        );
        assert!(store.projects[0].active_thread_id.is_none());
        assert!(store.spaces[0].active_project_id.is_none());
        assert!(store.projects[0].threads[0]
            .materialized_workspace_name
            .is_none());
        assert_eq!(store.thread_space_id(&thread_id), Some(space_id));
    }

    #[test]
    fn mux_domain_path_project_is_remote_even_when_path_exists_locally() {
        let space_id = default_space_id();
        let dir = tempdir().unwrap();
        let mut store = test_store();
        store.spaces[0].client_domain = Some("remote-mux".to_string());

        let materialized_workspace = "thinkterm:other-project:thread-other".to_string();
        let thread = WorkspaceThread::new(
            "project-path".to_string(),
            "main".to_string(),
            Some(materialized_workspace.clone()),
        );
        let thread_id = thread.id.clone();
        let mut project = test_project(
            "project-path",
            "Remote path",
            dir.path().to_path_buf(),
            vec![thread],
        );
        project.active_thread_id = Some(thread_id.clone());
        store.projects.push(project);
        store.set_active_project_for_space(&space_id, "project-path".to_string());

        assert_eq!(
            store
                .remote_files_target(&space_id, "project-path")
                .expect("remote Files target"),
            RemoteFilesTarget {
                project_id: "project-path".to_string(),
                project_name: "Remote path".to_string(),
                source: RemoteFilesSource::ClientDomain("remote-mux".to_string()),
                requested_root: dir.path().to_string_lossy().into_owned(),
            }
        );
        let view = store.view_for_project(&space_id, "project-path", &[]);
        assert!(view.projects[0].is_remote);
        assert!(store.project_reveal_path("project-path").is_none());
        assert!(!store.snapshot_active_space_thread_layout(
            &space_id,
            &materialized_workspace,
            WorkspaceThreadLayoutSnapshot {
                active_tab: 0,
                tabs: vec![serde_json::json!({"kind": "must-not-be-saved"})],
                terminal_specs: vec![],
            },
        ));
        assert!(store.projects[0].threads[0].layout.is_none());

        let state = store
            .thread_connection_state(&thread_id, &[])
            .expect("mux-domain thread state");
        assert!(state.is_remote);
        assert_eq!(state.space_id, space_id);
        assert_eq!(store.thread_to_restore_for_space(&space_id), (None, false));

        assert!(!store.repair_cross_space_local_workspace_bindings());
        assert_eq!(
            store.projects[0].threads[0]
                .materialized_workspace_name
                .as_deref(),
            Some(materialized_workspace.as_str())
        );
    }

    #[test]
    fn activation_plan_for_thread_does_not_select_or_materialize() {
        let space_id = default_space_id();
        let mut store = test_store();
        let thread = WorkspaceThread::new("ssh-host".to_string(), "main".to_string(), None);
        let thread_id = thread.id.clone();
        store.projects.push(Project {
            id: "ssh-host".to_string(),
            space_id: space_id.clone(),
            name: "Remote".to_string(),
            path: PathBuf::from("ssh://user@example.com"),
            threads: vec![thread],
            active_thread_id: None,
            threads_collapsed: false,
            active_note_path: None,
        });

        let plan = store
            .activation_plan_for_thread(&thread_id, &[])
            .expect("activation plan");
        assert!(plan.needs_materialize);
        assert_eq!(
            plan.workspace_name,
            workspace_name_for_thread("ssh-host", &thread_id)
        );
        assert!(store.projects[0].active_thread_id.is_none());
        assert!(store.spaces[0].active_project_id.is_none());
        assert!(store.projects[0].threads[0]
            .materialized_workspace_name
            .is_none());
        assert_eq!(store.thread_to_restore_for_space(&space_id), (None, false));

        let plan = store.activate_thread_record(&thread_id, &[]).unwrap();
        assert!(plan.needs_materialize);
        assert_eq!(
            store.projects[0].active_thread_id.as_deref(),
            Some(thread_id.as_str())
        );
        assert_eq!(
            store.spaces[0].active_project_id.as_deref(),
            Some("ssh-host")
        );
        assert!(thread_has_restorable_workspace(
            &store.projects[0].threads[0]
        ));
        assert_eq!(
            store.thread_to_restore_for_space(&space_id),
            (Some(thread_id), false)
        );
    }

    #[test]
    fn disconnected_remote_host_thread_preserves_workspace_override() {
        let space_id = default_space_id();
        let mut store = test_store();
        let thread_id = store.create_disconnected_remote_host_thread(
            &space_id,
            "ssh-host",
            "Remote",
            PathBuf::from("ssh://user@example.com"),
            Some("custom-remote-workspace".to_string()),
        );

        let project = store
            .projects
            .iter()
            .find(|project| project.threads.iter().any(|thread| thread.id == thread_id))
            .expect("remote project");
        let thread = project
            .threads
            .iter()
            .find(|thread| thread.id == thread_id)
            .expect("remote thread");
        let expected_workspace =
            workspace_name_for_remote_default(&project.id, &thread_id, "custom-remote-workspace");
        assert_eq!(
            thread.planned_workspace_name.as_deref(),
            Some(expected_workspace.as_str())
        );
        assert!(thread.materialized_workspace_name.is_none());
        assert!(!thread_has_restorable_workspace(thread));

        let state = store
            .thread_connection_state(&thread_id, &["custom-remote-workspace".to_string()])
            .expect("remote thread state");
        assert_eq!(state.space_id, space_id);
        assert_eq!(state.workspace_name, expected_workspace);
        assert!(!state.is_live);

        let plan = store
            .activate_thread_record(&thread_id, &[])
            .expect("activation plan");
        assert_eq!(plan.workspace_name, expected_workspace);
    }

    #[test]
    fn workspace_work_observation_transitions_to_finished_unseen() {
        let mut store = test_store();
        let session = WorkspaceThread::new(
            "project-1".to_string(),
            "main".to_string(),
            Some("workspace-1".to_string()),
        );
        store.projects.push(test_project(
            "project-1",
            "thinkterm",
            PathBuf::from("/tmp/thinkterm"),
            vec![session],
        ));

        let change = store
            .observe_thread_work_for_workspace("workspace-1", WorkspaceThreadWorkStatus::Running)
            .unwrap();
        assert!(change.changed);
        assert!(!change.should_persist);
        assert_eq!(
            store.projects[0].threads[0].work_status(),
            WorkspaceThreadWorkStatus::Running
        );

        let change = store
            .observe_thread_work_for_workspace("workspace-1", WorkspaceThreadWorkStatus::Idle)
            .unwrap();
        assert!(change.changed);
        assert!(change.should_persist);
        assert_eq!(
            store.projects[0].threads[0].work_status(),
            WorkspaceThreadWorkStatus::FinishedUnseen
        );
    }

    fn work_store_with_one_thread() -> WorkspaceThreadStore {
        let mut store = test_store();
        let session = WorkspaceThread::new(
            "project-1".to_string(),
            "main".to_string(),
            Some("workspace-1".to_string()),
        );
        store.projects.push(test_project(
            "project-1",
            "thinkterm",
            PathBuf::from("/tmp/thinkterm"),
            vec![session],
        ));
        store
    }

    fn observe(
        store: &mut WorkspaceThreadStore,
        status: WorkspaceThreadWorkStatus,
    ) -> Option<WorkAnnouncement> {
        store
            .observe_thread_work_for_workspace("workspace-1", status)
            .unwrap()
            .announce
    }

    /// The status is re-scanned on a timer, so the same finished thread is
    /// observed over and over. Announcing every observation would turn one
    /// completed job into a stream of dings.
    #[test]
    fn finishing_announces_once_per_run_not_once_per_scan() {
        let mut store = work_store_with_one_thread();

        assert_eq!(
            observe(&mut store, WorkspaceThreadWorkStatus::Running),
            None
        );
        assert_eq!(
            observe(&mut store, WorkspaceThreadWorkStatus::Idle),
            Some(WorkAnnouncement::Finished)
        );
        // Still finished, still idle — and now silent.
        assert_eq!(observe(&mut store, WorkspaceThreadWorkStatus::Idle), None);
        assert_eq!(observe(&mut store, WorkspaceThreadWorkStatus::Idle), None);

        // A second run of work earns a second announcement.
        assert_eq!(
            observe(&mut store, WorkspaceThreadWorkStatus::Running),
            None
        );
        assert_eq!(
            observe(&mut store, WorkspaceThreadWorkStatus::Idle),
            Some(WorkAnnouncement::Finished)
        );
    }

    #[test]
    fn short_finished_work_is_silent_but_five_seconds_plays() {
        assert!(!should_play_work_sound(
            WorkAnnouncement::Finished,
            Some(Duration::from_millis(4_999))
        ));
        assert!(should_play_work_sound(
            WorkAnnouncement::Finished,
            Some(Duration::from_secs(5))
        ));
        assert!(should_play_work_sound(
            WorkAnnouncement::NeedsInput,
            Some(Duration::from_millis(1))
        ));
    }

    #[test]
    fn finished_sound_duration_excludes_the_idle_confirmation_delay() {
        let mut timing = WorkSoundTiming::default();
        let started_at = std::time::Instant::now();
        timing.observe(
            "workspace-1",
            WorkspaceThreadWorkStatus::Running,
            started_at,
        );
        timing.observe(
            "workspace-1",
            WorkspaceThreadWorkStatus::Idle,
            started_at + Duration::from_millis(2_500),
        );

        assert_eq!(
            timing.take_finished_duration("workspace-1"),
            Some(Duration::from_millis(2_500)),
            "the 800ms status debounce must not turn a short task into an audible one"
        );
    }

    #[test]
    fn transient_idle_does_not_end_the_timed_run() {
        let mut timing = WorkSoundTiming::default();
        let started_at = std::time::Instant::now();
        timing.observe(
            "workspace-1",
            WorkspaceThreadWorkStatus::Running,
            started_at,
        );
        timing.observe(
            "workspace-1",
            WorkspaceThreadWorkStatus::Idle,
            started_at + Duration::from_secs(1),
        );
        timing.observe(
            "workspace-1",
            WorkspaceThreadWorkStatus::Running,
            started_at + Duration::from_millis(1_100),
        );
        timing.observe(
            "workspace-1",
            WorkspaceThreadWorkStatus::Idle,
            started_at + Duration::from_secs(4),
        );

        assert_eq!(
            timing.take_finished_duration("workspace-1"),
            Some(Duration::from_secs(4))
        );
    }

    /// Going idle without ever having run is a thread that did nothing, not a
    /// thread that finished something.
    #[test]
    fn going_idle_without_running_announces_nothing() {
        let mut store = work_store_with_one_thread();
        assert_eq!(observe(&mut store, WorkspaceThreadWorkStatus::Idle), None);
    }

    #[test]
    fn needing_input_announces_once_until_it_clears() {
        let mut store = work_store_with_one_thread();

        assert_eq!(
            observe(&mut store, WorkspaceThreadWorkStatus::NeedsAttention),
            Some(WorkAnnouncement::NeedsInput)
        );
        assert_eq!(
            observe(&mut store, WorkspaceThreadWorkStatus::NeedsAttention),
            None
        );

        // Work resumes, so the next time it gets stuck it is news again.
        assert_eq!(
            observe(&mut store, WorkspaceThreadWorkStatus::Running),
            None
        );
        assert_eq!(
            observe(&mut store, WorkspaceThreadWorkStatus::NeedsAttention),
            Some(WorkAnnouncement::NeedsInput)
        );
    }

    /// Acknowledging is the user saying they already know; it must not double
    /// as news.
    #[test]
    fn acknowledging_announces_nothing() {
        let mut store = work_store_with_one_thread();
        observe(&mut store, WorkspaceThreadWorkStatus::Running);
        observe(&mut store, WorkspaceThreadWorkStatus::Idle);

        assert_eq!(
            store
                .acknowledge_thread_work_for_workspace("workspace-1")
                .announce,
            None
        );
    }

    #[test]
    fn pending_notifications_list_attention_first_and_skip_idle_running() {
        let mut store = test_store();
        let mut finished = WorkspaceThread::new(
            "project-1".to_string(),
            "done".to_string(),
            Some("workspace-1".to_string()),
        );
        finished.work_finished_unseen = true;
        let mut attention = WorkspaceThread::new(
            "project-1".to_string(),
            "stuck".to_string(),
            Some("workspace-2".to_string()),
        );
        attention.work_needs_attention = true;
        let mut running = WorkspaceThread::new(
            "project-1".to_string(),
            "busy".to_string(),
            Some("workspace-3".to_string()),
        );
        running.work_is_running = true;
        let idle = WorkspaceThread::new(
            "project-1".to_string(),
            "quiet".to_string(),
            Some("workspace-4".to_string()),
        );
        store.projects.push(test_project(
            "project-1",
            "thinkterm",
            PathBuf::from("/tmp/thinkterm"),
            vec![finished, attention, running, idle],
        ));

        let notifications = store.pending_work_notifications();
        assert_eq!(notifications.len(), 2);
        assert_eq!(notifications[0].thread_name, "stuck");
        assert_eq!(
            notifications[0].status,
            WorkspaceThreadWorkStatus::NeedsAttention
        );
        assert_eq!(notifications[1].thread_name, "done");
        assert_eq!(notifications[0].project_name, "thinkterm");
        assert_eq!(notifications[0].space_id, default_space_id());
        assert!(!notifications[0].space_name.is_empty());
        assert_eq!(store.pending_work_notification_count(), 2);

        let thread_id = notifications[1].thread_id.clone();
        let change = store.acknowledge_thread_work_for_thread(&thread_id);
        assert!(change.changed);
        assert!(change.should_persist);
        assert_eq!(store.pending_work_notification_count(), 1);
        // Running state is runtime truth and must survive acknowledgement.
        assert!(store.projects[0].threads[2].work_is_running);
    }

    #[test]
    fn status_filter_hides_threads_but_never_the_active_one() {
        let hidden = [WorkspaceThreadWorkStatus::Idle];
        let make = |name: &str, status, is_active| WorkspaceThreadView {
            id: name.to_string(),
            name: name.to_string(),
            is_active,
            is_materialized: true,
            is_pinned: false,
            is_unread: false,
            work_status: status,
        };
        let mut view = WorkspaceThreadsView {
            pinned_threads: vec![make("pinned-idle", WorkspaceThreadWorkStatus::Idle, false)],
            projects: vec![ProjectView {
                id: "p".to_string(),
                name: "p".to_string(),
                is_active: true,
                threads_collapsed: false,
                threads: vec![
                    make("active-idle", WorkspaceThreadWorkStatus::Idle, true),
                    make("idle", WorkspaceThreadWorkStatus::Idle, false),
                    make("running", WorkspaceThreadWorkStatus::Running, false),
                ],
                is_remote: false,
                distro: None,
            }],
        };
        filter_threads_view_by_status(&mut view, &hidden);
        assert!(view.pinned_threads.is_empty());
        let names: Vec<_> = view.projects[0]
            .threads
            .iter()
            .map(|thread| thread.name.as_str())
            .collect();
        assert_eq!(names, ["active-idle", "running"]);

        assert!(!hidden_statuses_cover_all(&hidden));
        assert!(hidden_statuses_cover_all(&WorkspaceThreadWorkStatus::ALL));
        for status in WorkspaceThreadWorkStatus::ALL {
            assert_eq!(
                WorkspaceThreadWorkStatus::from_settings_key(status.settings_key()),
                Some(status)
            );
        }
    }

    #[test]
    fn acknowledged_finished_work_persists_but_runtime_running_state_does_not() {
        let mut store = test_store();
        let mut session = WorkspaceThread::new(
            "project-1".to_string(),
            "main".to_string(),
            Some("workspace-1".to_string()),
        );
        session.work_finished_unseen = true;
        store.projects.push(test_project(
            "project-1",
            "thinkterm",
            PathBuf::from("/tmp/thinkterm"),
            vec![session],
        ));

        let change = store.acknowledge_thread_work_for_workspace("workspace-1");
        assert!(change.changed);
        assert!(change.should_persist);
        assert!(!store.projects[0].threads[0].work_finished_unseen);

        store.projects[0].threads[0].work_is_running = true;
        let change = store.acknowledge_thread_work_for_workspace("workspace-1");
        assert!(change.changed);
        assert!(!change.should_persist);
        assert!(!store.projects[0].threads[0].work_is_running);
    }

    #[test]
    fn inactive_thread_activation_materializes_once() {
        let mut store = test_store();
        let project = test_project(
            "project-1",
            "thinkterm",
            PathBuf::from("/tmp/thinkterm"),
            vec![WorkspaceThread::new(
                "project-1".to_string(),
                "main".to_string(),
                None,
            )],
        );
        let thread_id = project.threads[0].id.clone();
        store.projects.push(project);

        let plan = store.activate_thread_record(&thread_id, &[]).unwrap();
        assert!(plan.needs_materialize);
        let plan = store
            .activate_thread_record(&thread_id, &[plan.workspace_name.clone()])
            .unwrap();
        assert!(!plan.needs_materialize);
    }

    #[test]
    fn created_project_gets_own_workspace_and_cwd() {
        let space_id = default_space_id();
        let dir = tempdir().unwrap();
        let mut store = test_store();

        let thread_id = store.create_project_from_path(&space_id, dir.path().to_path_buf());
        let project_id = store.active_project_id_for_space(&space_id).unwrap();
        let workspace_name = workspace_name_for_thread(&project_id, &thread_id);

        assert_eq!(
            store.project_id_for_workspace(&space_id, &workspace_name),
            Some(project_id.clone())
        );

        let plan = store.activate_thread_record(&thread_id, &[]).unwrap();
        assert!(plan.needs_materialize);
        assert_eq!(plan.project_path, dir.path());
        assert_eq!(plan.workspace_name, workspace_name);
    }

    #[test]
    fn strips_windows_verbatim_drive_prefix() {
        assert_eq!(
            strip_windows_verbatim_prefix_text(r"\\?\C:\Users\Dev\Documents\GitHub").as_deref(),
            Some(r"C:\Users\Dev\Documents\GitHub")
        );
    }

    #[test]
    fn strips_windows_verbatim_unc_prefix() {
        assert_eq!(
            strip_windows_verbatim_prefix_text(r"\\?\UNC\server\share\project").as_deref(),
            Some(r"\\server\share\project")
        );
    }

    #[test]
    fn duplicate_project_path_reuses_existing_thread() {
        let space_id = default_space_id();
        let dir = tempdir().unwrap();
        let mut store = test_store();

        let first_thread_id = store.create_project_from_path(&space_id, dir.path().to_path_buf());
        let second_thread_id = store.create_project_from_path(&space_id, dir.path().to_path_buf());

        assert_eq!(second_thread_id, first_thread_id);
        assert_eq!(store.projects.len(), 1);
        assert_eq!(store.projects[0].threads.len(), 1);
    }

    #[test]
    fn duplicate_project_path_reuses_stored_project_id() {
        let space_id = default_space_id();
        let dir = tempdir().unwrap();
        let mut store = test_store();

        let project = test_project(
            "stored-project-id",
            "existing",
            dir.path().to_path_buf(),
            vec![WorkspaceThread::new(
                "stored-project-id".to_string(),
                "main".to_string(),
                None,
            )],
        );
        let thread_id = project.threads[0].id.clone();
        store.projects.push(project);

        let reused_thread_id = store.create_project_from_path(&space_id, dir.path().to_path_buf());

        assert_eq!(reused_thread_id, thread_id);
        assert_eq!(
            store.active_project_id_for_space(&space_id).as_deref(),
            Some("stored-project-id")
        );
        assert_eq!(store.projects.len(), 1);
    }

    #[test]
    fn same_path_in_different_spaces_gets_distinct_projects() {
        let dir = tempdir().unwrap();
        let mut store = test_store();
        let default_space = default_space_id();
        let second_space = store.create_space_record("Second".to_string());

        let first_thread_id =
            store.create_project_from_path(&default_space, dir.path().to_path_buf());
        let first_project_id = store.active_project_id_for_space(&default_space).unwrap();
        let second_thread_id =
            store.create_project_from_path(&second_space, dir.path().to_path_buf());
        let second_project_id = store.active_project_id_for_space(&second_space).unwrap();

        assert_ne!(first_project_id, second_project_id);
        assert_ne!(first_thread_id, second_thread_id);
        assert_eq!(store.projects.len(), 2);
        assert_eq!(
            store
                .projects
                .iter()
                .filter(|project| project.path == dir.path())
                .count(),
            2
        );
    }

    #[test]
    fn current_workspace_owned_by_another_space_is_not_rebound() {
        let mut store = test_store();
        let default_space = default_space_id();
        let second_space = store.create_space_record("Second".to_string());
        let mut default_thread = WorkspaceThread::new(
            "project-default".to_string(),
            "main".to_string(),
            Some("workspace-default".to_string()),
        );
        let default_thread_id = default_thread.id.clone();
        default_thread.layout = Some(WorkspaceThreadLayoutSnapshot {
            active_tab: 0,
            tabs: vec![serde_json::json!({"kind": "default"})],
            terminal_specs: vec![],
        });
        let mut default_project = test_project_in_space(
            &default_space,
            "project-default",
            "Default",
            PathBuf::from("/tmp/default"),
            vec![default_thread],
        );
        default_project.active_thread_id = Some(default_thread_id);
        store.projects.push(default_project);
        store.set_active_project_for_space(&default_space, "project-default".to_string());

        store.sync_current_project(&second_space, "workspace-default");
        assert!(store
            .projects
            .iter()
            .filter(|project| project.space_id == second_space)
            .all(|project| project.threads.iter().all(|thread| {
                thread.materialized_workspace_name.as_deref() != Some("workspace-default")
            })));
    }

    #[test]
    fn normalize_repairs_local_thread_bound_to_another_thinkterm_workspace() {
        let mut store = test_store();
        let default_space = default_space_id();
        let second_space = store.create_space_record("Second".to_string());
        let first = WorkspaceThread::new(
            "project-default".to_string(),
            "main".to_string(),
            Some("thinkterm:project-default:thread-default".to_string()),
        );
        let mut second = WorkspaceThread::new(
            "project-second".to_string(),
            "main".to_string(),
            Some("thinkterm:project-default:thread-default".to_string()),
        );
        second.id = "thread-second".to_string();
        store.projects.push(test_project_in_space(
            &default_space,
            "project-default",
            "Default",
            PathBuf::from("/tmp/default"),
            vec![first],
        ));
        store.projects.push(test_project_in_space(
            &second_space,
            "project-second",
            "Second",
            PathBuf::from("/tmp/second"),
            vec![second],
        ));

        assert!(store.normalize_after_load());
        let repaired = &store.projects[1].threads[0];
        assert_eq!(
            repaired.materialized_workspace_name.as_deref(),
            Some("thinkterm:project-second:thread-second")
        );
    }

    #[test]
    fn rename_space_keeps_id_and_project_ids_stable() {
        let dir = tempdir().unwrap();
        let mut store = test_store();
        let space_id = store.create_space_record("Coding".to_string());
        store.create_project_from_path(&space_id, dir.path().to_path_buf());
        let project_id = store.active_project_id_for_space(&space_id).unwrap();

        assert!(store.rename_space(&space_id, "Renamed Coding".to_string()));

        let renamed = store
            .spaces
            .iter()
            .find(|space| space.id == space_id)
            .unwrap();
        assert_eq!(renamed.name, "Renamed Coding");
        assert_eq!(
            store.active_project_id_for_space(&space_id).as_deref(),
            Some(project_id.as_str())
        );
        assert!(store
            .projects
            .iter()
            .any(|project| { project.id == project_id && project.space_id == space_id }));
    }

    #[test]
    fn legacy_store_migration_keeps_project_ids() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("workspace_threads.json");
        let legacy = serde_json::json!({
            "active_project_id": "legacy-project-id",
            "projects": [
                {
                    "id": "legacy-project-id",
                    "name": "thinkterm",
                    "path": "/tmp/thinkterm",
                    "threads": [
                        {
                            "id": "legacy-thread-id",
                            "name": "main",
                            "project_id": "legacy-project-id",
                            "layout": null,
                            "materialized_workspace_name": "legacy-workspace",
                            "last_active_at": 1,
                            "is_pinned": false,
                            "is_unread": false,
                            "work_finished_unseen": false
                        }
                    ],
                    "active_thread_id": "legacy-thread-id",
                    "threads_collapsed": false
                }
            ]
        });
        std::fs::write(&path, serde_json::to_string_pretty(&legacy).unwrap()).unwrap();

        let loaded = load_workspace_thread_store_from_path(&path).unwrap();

        assert_eq!(loaded.projects[0].id, "legacy-project-id");
        assert_eq!(loaded.projects[0].space_id, default_space_id());
        assert_eq!(
            loaded
                .active_project_id_for_space(&default_space_id())
                .as_deref(),
            Some("legacy-project-id")
        );
    }

    #[test]
    fn delete_space_removes_owned_projects_and_returns_live_workspaces() {
        let mut store = test_store();
        let space_id = store.create_space_record("Scratch".to_string());
        let mut thread = WorkspaceThread::new(
            "project-space".to_string(),
            "main".to_string(),
            Some("live-space-workspace".to_string()),
        );
        thread.materialized_workspace_name = Some("live-space-workspace".to_string());
        store.projects.push(test_project_in_space(
            &space_id,
            "project-space",
            "scratch",
            PathBuf::from("/tmp/scratch"),
            vec![thread],
        ));

        let deleted = store.delete_space(&space_id).unwrap();

        assert_eq!(
            deleted.materialized_workspace_names,
            vec!["live-space-workspace".to_string()]
        );
        assert_eq!(deleted.fallback_space_id, default_space_id());
        assert!(!store.spaces.iter().any(|space| space.id == space_id));
        assert!(!store
            .projects
            .iter()
            .any(|project| project.space_id == space_id));
    }

    #[test]
    fn default_space_cannot_be_deleted() {
        let mut store = test_store();
        assert_eq!(
            store.delete_space(&default_space_id()),
            Err(DeleteSpaceError::DefaultSpace)
        );
    }

    #[test]
    fn snapshot_layout_attaches_to_materialized_thread() {
        let mut store = test_store();
        let space_id = default_space_id();
        let mut session = WorkspaceThread::new(
            "project-1".to_string(),
            "main".to_string(),
            Some("ws".to_string()),
        );
        let thread_id = session.id.clone();
        let mut project = test_project(
            "project-1",
            "thinkterm",
            PathBuf::from("/tmp/thinkterm"),
            vec![session.clone()],
        );
        project.active_thread_id = Some(thread_id);
        store.projects.push(project);
        store.set_active_project_for_space(&space_id, "project-1".to_string());
        assert!(store.snapshot_active_space_thread_layout(
            &space_id,
            "ws",
            WorkspaceThreadLayoutSnapshot {
                active_tab: 0,
                tabs: vec![serde_json::json!({"kind": "test"})],
                terminal_specs: vec![],
            },
        ));
        session = store.projects[0].threads[0].clone();
        assert_eq!(session.layout.unwrap().tabs.len(), 1);
    }

    #[test]
    fn snapshot_active_space_layout_does_not_attach_to_other_space_workspace() {
        let mut store = test_store();
        let default_space = default_space_id();
        let second_space = store.create_space_record("Second".to_string());
        let default_thread = WorkspaceThread::new(
            "project-default".to_string(),
            "main".to_string(),
            Some("workspace-default".to_string()),
        );
        let second_thread = WorkspaceThread::new(
            "project-second".to_string(),
            "main".to_string(),
            Some("workspace-second".to_string()),
        );
        let second_thread_id = second_thread.id.clone();
        let mut default_project = test_project_in_space(
            &default_space,
            "project-default",
            "Default",
            PathBuf::from("/tmp/default"),
            vec![default_thread],
        );
        default_project.active_thread_id = default_project
            .threads
            .first()
            .map(|thread| thread.id.clone());
        let mut second_project = test_project_in_space(
            &second_space,
            "project-second",
            "Second",
            PathBuf::from("/tmp/second"),
            vec![second_thread],
        );
        second_project.active_thread_id = Some(second_thread_id);
        store.projects.push(default_project);
        store.projects.push(second_project);
        store.set_active_project_for_space(&second_space, "project-second".to_string());

        let snapshot = WorkspaceThreadLayoutSnapshot {
            active_tab: 0,
            tabs: vec![serde_json::json!({"kind": "wrong"})],
            terminal_specs: vec![],
        };
        assert!(!store.snapshot_active_space_thread_layout(
            &second_space,
            "workspace-default",
            snapshot
        ));
        assert!(store.projects[1].threads[0].layout.is_none());

        let snapshot = WorkspaceThreadLayoutSnapshot {
            active_tab: 0,
            tabs: vec![serde_json::json!({"kind": "right"})],
            terminal_specs: vec![],
        };
        assert!(store.snapshot_active_space_thread_layout(
            &second_space,
            "workspace-second",
            snapshot
        ));
        assert_eq!(
            store.projects[1].threads[0]
                .layout
                .as_ref()
                .unwrap()
                .tabs
                .len(),
            1
        );
    }

    #[test]
    fn view_marks_only_global_active_workspace_thread_active() {
        let space_id = default_space_id();
        let mut store = test_store();
        let first = WorkspaceThread::new("project-1".to_string(), "main".to_string(), None);
        let second = WorkspaceThread::new("project-2".to_string(), "current".to_string(), None);
        let first_id = first.id.clone();
        let second_id = second.id.clone();
        let mut first_project = test_project(
            "project-1",
            "thinkterm",
            PathBuf::from("/tmp/thinkterm"),
            vec![first],
        );
        first_project.active_thread_id = Some(first_id);
        store.projects.push(first_project);
        let mut second_project = test_project(
            "project-2",
            "agent_dock",
            PathBuf::from("/tmp/agent_dock"),
            vec![second],
        );
        second_project.active_thread_id = Some(second_id);
        store.projects.push(second_project);
        store.set_active_project_for_space(&space_id, "project-2".to_string());

        let view = store.view_for_project(&space_id, "project-2", &[]);
        assert!(!view.projects[0].is_active);
        assert!(!view.projects[0].threads[0].is_active);
        assert!(view.projects[1].is_active);
        assert!(view.projects[1].threads[0].is_active);
    }

    #[test]
    fn project_menu_metadata_actions_update_store() {
        let space_id = default_space_id();
        let mut store = test_store();
        let first = WorkspaceThread::new("project-1".to_string(), "main".to_string(), None);
        let second = WorkspaceThread::new("project-2".to_string(), "current".to_string(), None);
        let first_id = first.id.clone();
        let second_id = second.id.clone();
        let mut first_project = test_project(
            "project-1",
            "thinkterm",
            PathBuf::from("/tmp/thinkterm"),
            vec![first],
        );
        first_project.active_thread_id = Some(first_id);
        store.projects.push(first_project);
        let mut second_project = test_project(
            "project-2",
            "agent_dock",
            PathBuf::from("/tmp/agent_dock"),
            vec![second],
        );
        second_project.active_thread_id = Some(second_id.clone());
        store.projects.push(second_project);
        store.set_active_project_for_space(&space_id, "project-2".to_string());

        assert!(store.rename_project("project-2", "Agents".to_string()));
        assert_eq!(store.projects[1].name, "Agents");

        let removed = store.remove_project("project-2").unwrap();
        assert!(removed.was_active);
        assert_eq!(
            removed.next_thread_id,
            Some(store.projects[0].threads[0].id.clone())
        );
        assert_eq!(store.projects.len(), 1);
        assert_eq!(
            store.active_project_id_for_space(&space_id).as_deref(),
            Some("project-1")
        );
        assert!(store.remove_project("project-1").is_none());
    }

    #[test]
    fn end_workspace_thread_deletes_thread_when_project_has_fallback_thread() {
        let space_id = default_space_id();
        let mut store = test_store();
        let first = WorkspaceThread::new("project-1".to_string(), "main".to_string(), None);
        let second = WorkspaceThread::new("project-1".to_string(), "Thread 2".to_string(), None);
        let first_id = first.id.clone();
        let second_id = second.id.clone();
        let mut project = test_project(
            "project-1",
            "thinkterm",
            PathBuf::from("/tmp/thinkterm"),
            vec![first, second],
        );
        project.active_thread_id = Some(second_id.clone());
        store.projects.push(project);
        store.set_active_project_for_space(&space_id, "project-1".to_string());

        let result = store.end_workspace_thread_record(&second_id);
        let EndWorkspaceThreadResult::DeletedThread(deleted) = result else {
            panic!("expected deleted thread result");
        };
        assert!(deleted.was_active);
        assert_eq!(deleted.next_thread_id, Some(first_id.clone()));
        assert_eq!(store.projects[0].threads.len(), 1);
        assert_eq!(
            store.projects[0].active_thread_id.as_deref(),
            Some(first_id.as_str())
        );
    }

    #[test]
    fn end_workspace_thread_removes_remote_project_when_space_has_fallback_project() {
        let space_id = default_space_id();
        let mut store = test_store();
        let local = WorkspaceThread::new("project-local".to_string(), "main".to_string(), None);
        let local_id = local.id.clone();
        let mut local_project = test_project(
            "project-local",
            "Home",
            PathBuf::from("/tmp/home"),
            vec![local],
        );
        local_project.active_thread_id = Some(local_id.clone());
        store.projects.push(local_project);

        let remote = WorkspaceThread::new("ssh-host".to_string(), "Session 1".to_string(), None);
        let remote_id = remote.id.clone();
        let mut remote_project = test_project(
            "ssh-host",
            "Remote",
            PathBuf::from("ssh://root@example.com"),
            vec![remote],
        );
        remote_project.active_thread_id = Some(remote_id.clone());
        store.projects.push(remote_project);
        store.set_active_project_for_space(&space_id, "ssh-host".to_string());

        let result = store.end_workspace_thread_record(&remote_id);
        let EndWorkspaceThreadResult::RemovedProject(removed) = result else {
            panic!("expected removed project result");
        };
        assert!(removed.was_active);
        assert_eq!(removed.next_thread_id, Some(local_id.clone()));
        assert_eq!(store.projects.len(), 1);
        assert_eq!(store.projects[0].id, "project-local");
        assert_eq!(
            store.active_project_id_for_space(&space_id).as_deref(),
            Some("project-local")
        );
    }

    #[test]
    fn end_workspace_thread_seeds_default_project_before_removing_last_remote_project() {
        let space_id = default_space_id();
        let mut store = test_store();
        let remote = WorkspaceThread::new("ssh-host".to_string(), "Session 1".to_string(), None);
        let remote_id = remote.id.clone();
        let mut remote_project = test_project(
            "ssh-host",
            "Remote",
            PathBuf::from("ssh://root@example.com"),
            vec![remote],
        );
        remote_project.active_thread_id = Some(remote_id.clone());
        store.projects.push(remote_project);
        store.set_active_project_for_space(&space_id, "ssh-host".to_string());

        let result = store.end_workspace_thread_record(&remote_id);
        let EndWorkspaceThreadResult::RemovedProject(removed) = result else {
            panic!("expected removed project result");
        };
        assert!(removed.was_active);
        let next_thread_id = removed
            .next_thread_id
            .clone()
            .expect("new default thread id");
        assert_eq!(store.projects.len(), 1);
        assert_eq!(store.projects[0].name, "Home");
        assert_eq!(store.projects[0].threads.len(), 1);
        assert_eq!(store.projects[0].threads[0].name, "main");
        assert_eq!(store.projects[0].threads[0].id, next_thread_id);
        assert_eq!(store.projects[0].active_thread_id, Some(next_thread_id));
        assert_eq!(
            store.active_project_id_for_space(&space_id).as_deref(),
            Some(store.projects[0].id.as_str())
        );
    }

    #[test]
    fn end_workspace_thread_removes_mux_domain_path_project() {
        let space_id = default_space_id();
        let mut store = test_store();
        store.spaces[0].client_domain = Some("remote-mux".to_string());

        let thread =
            WorkspaceThread::new("project-path".to_string(), "Session 1".to_string(), None);
        let thread_id = thread.id.clone();
        let mut project = test_project(
            "project-path",
            "Remote path",
            PathBuf::from("/remote/project"),
            vec![thread],
        );
        project.active_thread_id = Some(thread_id.clone());
        store.projects.push(project);
        store.set_active_project_for_space(&space_id, "project-path".to_string());

        let result = store.end_workspace_thread_record(&thread_id);
        let EndWorkspaceThreadResult::RemovedProject(removed) = result else {
            panic!("expected mux-domain project removal");
        };
        assert!(removed.was_active);
        assert!(store
            .projects
            .iter()
            .all(|project| project.id != "project-path"));
    }

    #[test]
    fn end_workspace_thread_noops_for_last_local_thread() {
        let space_id = default_space_id();
        let mut store = test_store();
        let thread = WorkspaceThread::new("project-local".to_string(), "main".to_string(), None);
        let thread_id = thread.id.clone();
        let mut project = test_project(
            "project-local",
            "Home",
            PathBuf::from("/tmp/home"),
            vec![thread],
        );
        project.active_thread_id = Some(thread_id.clone());
        store.projects.push(project);
        store.set_active_project_for_space(&space_id, "project-local".to_string());

        assert_eq!(
            store.end_workspace_thread_record(&thread_id),
            EndWorkspaceThreadResult::Noop
        );
        assert_eq!(store.projects.len(), 1);
        assert_eq!(store.projects[0].threads.len(), 1);
        assert_eq!(
            store.active_project_id_for_space(&space_id).as_deref(),
            Some("project-local")
        );
    }

    #[test]
    fn disconnect_active_remote_thread_keeps_record_and_switches_to_default_project() {
        let space_id = default_space_id();
        let mut store = test_store();
        let remote = WorkspaceThread::new("ssh-host".to_string(), "Session 1".to_string(), None);
        let remote_id = remote.id.clone();
        let remote_workspace = workspace_name_for_thread("ssh-host", &remote_id);
        let mut remote_project = test_project(
            "ssh-host",
            "Remote",
            PathBuf::from("ssh://root@example.com"),
            vec![remote],
        );
        remote_project.active_thread_id = Some(remote_id.clone());
        store.projects.push(remote_project);
        store.set_active_project_for_space(&space_id, "ssh-host".to_string());

        let (disconnected, changed) =
            store.disconnect_workspace_thread_record(&remote_id, &[remote_workspace.clone()]);
        let disconnected = disconnected.expect("disconnected remote thread");
        assert!(changed);
        assert!(disconnected.was_active);
        assert_eq!(disconnected.workspace_name, remote_workspace);
        assert!(disconnected.next_thread_id.is_some());
        assert!(store.projects.iter().any(|project| project.id == "ssh-host"
            && project.threads.iter().any(|thread| thread.id == remote_id)));
        assert_eq!(
            store.active_project_id_for_space(&space_id).as_deref(),
            Some(store.projects[1].id.as_str())
        );
    }

    #[test]
    fn disconnect_inactive_remote_thread_keeps_active_project() {
        let space_id = default_space_id();
        let mut store = test_store();
        let local = WorkspaceThread::new("project-local".to_string(), "main".to_string(), None);
        let local_id = local.id.clone();
        let mut local_project = test_project(
            "project-local",
            "Home",
            PathBuf::from("/tmp/home"),
            vec![local],
        );
        local_project.active_thread_id = Some(local_id);
        store.projects.push(local_project);

        let remote = WorkspaceThread::new("ssh-host".to_string(), "Session 1".to_string(), None);
        let remote_id = remote.id.clone();
        let remote_workspace = workspace_name_for_thread("ssh-host", &remote_id);
        let mut remote_project = test_project(
            "ssh-host",
            "Remote",
            PathBuf::from("ssh://root@example.com"),
            vec![remote],
        );
        remote_project.active_thread_id = Some(remote_id.clone());
        store.projects.push(remote_project);
        store.set_active_project_for_space(&space_id, "project-local".to_string());

        let (disconnected, changed) =
            store.disconnect_workspace_thread_record(&remote_id, &[remote_workspace.clone()]);
        let disconnected = disconnected.expect("disconnected remote thread");
        assert!(!changed);
        assert!(!disconnected.was_active);
        assert_eq!(disconnected.next_thread_id, None);
        assert_eq!(disconnected.workspace_name, remote_workspace);
        assert_eq!(
            store.active_project_id_for_space(&space_id).as_deref(),
            Some("project-local")
        );
    }

    #[test]
    fn disconnect_non_live_remote_thread_noops() {
        let mut store = test_store();
        let remote = WorkspaceThread::new("ssh-host".to_string(), "Session 1".to_string(), None);
        let remote_id = remote.id.clone();
        let mut remote_project = test_project(
            "ssh-host",
            "Remote",
            PathBuf::from("ssh://root@example.com"),
            vec![remote],
        );
        remote_project.active_thread_id = Some(remote_id.clone());
        store.projects.push(remote_project);

        assert_eq!(
            store.disconnect_workspace_thread_record(&remote_id, &[]),
            (None, false)
        );
    }

    #[test]
    fn disconnect_mux_domain_path_thread_uses_remote_flow() {
        let space_id = default_space_id();
        let mut store = test_store();
        store.spaces[0].client_domain = Some("remote-mux".to_string());

        let thread =
            WorkspaceThread::new("project-path".to_string(), "Session 1".to_string(), None);
        let thread_id = thread.id.clone();
        let workspace = workspace_name_for_thread("project-path", &thread_id);
        let mut project = test_project(
            "project-path",
            "Remote path",
            PathBuf::from("/remote/project"),
            vec![thread],
        );
        project.active_thread_id = Some(thread_id.clone());
        store.projects.push(project);
        store.set_active_project_for_space(&space_id, "project-path".to_string());

        let (disconnected, changed) =
            store.disconnect_workspace_thread_record(&thread_id, &[workspace.clone()]);
        let disconnected = disconnected.expect("disconnected mux-domain thread");
        assert!(changed);
        assert!(disconnected.was_active);
        assert_eq!(disconnected.workspace_name, workspace);
        assert_ne!(
            store.active_project_id_for_space(&space_id).as_deref(),
            Some("project-path")
        );
    }

    #[test]
    fn project_reveal_path_requires_existing_local_directory() {
        let mut store = test_store();
        let dir = tempdir().unwrap();
        store.projects.push(test_project(
            "project-local",
            "Local",
            dir.path().to_path_buf(),
            vec![],
        ));
        store.projects.push(test_project(
            "project-missing",
            "Missing",
            dir.path().join("missing"),
            vec![],
        ));
        store.projects.push(test_project(
            "ssh-host",
            "Remote",
            PathBuf::from("ssh://example/home"),
            vec![],
        ));

        assert_eq!(
            store.project_reveal_path("project-local"),
            Some(dir.path().to_path_buf())
        );
        assert!(store.project_reveal_path("project-missing").is_none());
        assert!(store.project_reveal_path("ssh-host").is_none());
        assert!(store.project_reveal_path("project-unknown").is_none());
    }

    #[test]
    fn thread_menu_metadata_actions_update_store() {
        let space_id = default_space_id();
        let mut store = test_store();
        let mut first = WorkspaceThread::new("project-1".to_string(), "main".to_string(), None);
        let second = WorkspaceThread::new("project-1".to_string(), "Thread 2".to_string(), None);
        let third = WorkspaceThread::new("project-1".to_string(), "Thread 3".to_string(), None);
        let first_id = first.id.clone();
        let second_id = second.id.clone();
        let third_id = third.id.clone();
        first.is_unread = true;
        let mut project = test_project(
            "project-1",
            "thinkterm",
            PathBuf::from("/tmp/thinkterm"),
            vec![first, second, third],
        );
        project.active_thread_id = Some(first_id.clone());
        store.projects.push(project);
        store.set_active_project_for_space(&space_id, "project-1".to_string());

        assert!(store.rename_thread(&second_id, "Review".to_string()));
        assert!(store.toggle_thread_pinned(&second_id));
        assert!(store.mark_thread_unread(&second_id));
        assert_eq!(
            store
                .delete_thread(&third_id)
                .unwrap()
                .materialized_workspace_name,
            None
        );

        let project = &store.projects[0];
        let updated = project
            .threads
            .iter()
            .find(|session| session.id == second_id)
            .unwrap();
        assert_eq!(updated.name, "Review");
        assert!(updated.is_pinned);
        assert!(updated.is_unread);

        let view = store.view_for_project(&space_id, "project-1", &[]);
        assert_eq!(view.pinned_threads.len(), 1);
        assert_eq!(view.pinned_threads[0].id, second_id);
        assert_eq!(view.projects[0].threads.len(), 1);
        assert_eq!(view.projects[0].threads[0].id, first_id);
    }
    fn ordered_project_ids(store: &WorkspaceThreadStore, space_id: &str) -> Vec<String> {
        store
            .projects
            .iter()
            .filter(|project| project.space_id == space_id)
            .map(|project| project.id.clone())
            .collect()
    }

    #[test]
    fn a_project_drags_before_a_sibling_and_to_the_end() {
        let mut store = test_store();
        let space_id = store.spaces[0].id.clone();
        for id in ["p1", "p2", "p3"] {
            store.projects.push(test_project_in_space(
                &space_id,
                id,
                id,
                PathBuf::from(format!("/tmp/{id}")),
                vec![],
            ));
        }

        assert!(store.move_project_before(&space_id, "p3", Some("p1")));
        assert_eq!(ordered_project_ids(&store, &space_id), ["p3", "p1", "p2"]);

        assert!(store.move_project_before(&space_id, "p3", None));
        assert_eq!(ordered_project_ids(&store, &space_id), ["p1", "p2", "p3"]);

        // Dropping where the project already sits must not dirty the store.
        assert!(!store.move_project_before(&space_id, "p3", None));
        assert!(!store.move_project_before(&space_id, "p2", Some("p3")));
        assert!(!store.move_project_before(&space_id, "p2", Some("p2")));
        assert!(!store.move_project_before(&space_id, "missing", Some("p1")));
        assert!(!store.move_project_before("other-space", "p1", Some("p2")));
    }

    /// The projects Vec interleaves every Space; moving within one Space
    /// must not disturb another Space's relative order.
    #[test]
    fn a_project_move_stays_inside_its_space() {
        let mut store = test_store();
        let space_id = store.spaces[0].id.clone();
        store.projects.push(test_project_in_space(
            &space_id,
            "a1",
            "a1",
            PathBuf::from("/tmp/a1"),
            vec![],
        ));
        store.projects.push(test_project_in_space(
            "space-b",
            "b1",
            "b1",
            PathBuf::from("/tmp/b1"),
            vec![],
        ));
        store.projects.push(test_project_in_space(
            &space_id,
            "a2",
            "a2",
            PathBuf::from("/tmp/a2"),
            vec![],
        ));

        assert!(store.move_project_before(&space_id, "a2", Some("a1")));
        assert_eq!(ordered_project_ids(&store, &space_id), ["a2", "a1"]);
        assert_eq!(ordered_project_ids(&store, "space-b"), ["b1"]);
        // An anchor from another Space is refused outright.
        assert!(!store.move_project_before(&space_id, "a1", Some("b1")));
    }

    #[test]
    fn a_thread_drags_within_its_project_and_pinned_rows_stay_put() {
        let mut store = test_store();
        let space_id = store.spaces[0].id.clone();
        let threads: Vec<WorkspaceThread> = ["t1", "t2", "t3"]
            .iter()
            .map(|name| WorkspaceThread::new("project-1".to_string(), name.to_string(), None))
            .collect();
        let ids: Vec<String> = threads.iter().map(|thread| thread.id.clone()).collect();
        store.projects.push(test_project_in_space(
            &space_id,
            "project-1",
            "P",
            PathBuf::from("/tmp/p"),
            threads,
        ));

        let order = |store: &WorkspaceThreadStore| -> Vec<String> {
            store.projects[0]
                .threads
                .iter()
                .map(|thread| thread.name.clone())
                .collect()
        };

        assert!(store.move_thread_before("project-1", &ids[2], Some(&ids[0])));
        assert_eq!(order(&store), ["t3", "t1", "t2"]);
        assert!(store.move_thread_before("project-1", &ids[2], None));
        assert_eq!(order(&store), ["t1", "t2", "t3"]);
        assert!(!store.move_thread_before("project-1", &ids[2], None));
        assert!(!store.move_thread_before("project-1", &ids[1], Some(&ids[2])));

        // Pinned threads belong to the sidebar's separate section: neither
        // draggable nor a valid anchor.
        assert!(store.toggle_thread_pinned(&ids[0]));
        assert!(!store.move_thread_before("project-1", &ids[0], None));
        assert!(!store.move_thread_before("project-1", &ids[1], Some(&ids[0])));
        // Moving an unpinned thread before an unpinned sibling still works
        // with the pinned entry interleaved in the Vec.
        assert!(store.move_thread_before("project-1", &ids[2], Some(&ids[1])));
        assert_eq!(order(&store), ["t1", "t3", "t2"]);
    }

    /// A store holding one remote Space for `domain`, plus one purely local
    /// project, so that ingestion can be checked for collateral damage.
    fn remote_test_store(domain: &str) -> WorkspaceThreadStore {
        let mut store = test_store();
        let local_space = store.spaces[0].id.clone();
        store.projects.push(test_project_in_space(
            &local_space,
            "local-1",
            "local",
            PathBuf::from("/tmp/local"),
            vec![],
        ));
        store.spaces.push(Space {
            id: "space-remote".to_string(),
            name: "syd".to_string(),
            active_project_id: None,
            note_vault: None,
            is_default: false,
            client_domain: Some(domain.to_string()),
        });
        store
    }

    fn tree_thread(id: &str, project_id: &str, name: &str) -> codec::TtThread {
        codec::TtThread {
            id: id.to_string(),
            project_id: project_id.to_string(),
            name: name.to_string(),
            planned_workspace_name: None,
            materialized_workspace_name: Some(format!("thinkterm:{project_id}:{id}")),
            last_active_at: 10,
            is_pinned: false,
            is_unread: false,
        }
    }

    fn sample_tree() -> codec::ThinkTermTree {
        codec::ThinkTermTree {
            spaces: vec![codec::TtSpace {
                id: "space-remote".to_string(),
                name: "syd".to_string(),
            }],
            projects: vec![codec::TtProject {
                id: "rp1".to_string(),
                space_id: "space-remote".to_string(),
                name: "thinkterm".to_string(),
                path: "/home/x/github/ThinkTerm".to_string(),
                threads: vec![
                    tree_thread("rt1", "rp1", "main"),
                    tree_thread("rt2", "rp1", "build"),
                ],
            }],
            revision: 1,
        }
    }

    #[test]
    fn ingesting_a_tree_adopts_shared_rows_and_leaves_local_spaces_alone() {
        let mut store = remote_test_store("syd");
        let local_projects_before = store.projects.clone();

        assert!(store.ingest_remote_tree("syd", &sample_tree()));

        let remote: Vec<&Project> = store
            .projects
            .iter()
            .filter(|project| project.space_id == "space-remote")
            .collect();
        assert_eq!(remote.len(), 1);
        assert_eq!(remote[0].id, "rp1");
        assert_eq!(remote[0].path, PathBuf::from("/home/x/github/ThinkTerm"));
        assert_eq!(
            remote[0]
                .threads
                .iter()
                .map(|thread| thread.name.as_str())
                .collect::<Vec<_>>(),
            ["main", "build"]
        );
        // The client's own client_domain tag is re-applied, not taken from the
        // wire: the tree describes rows, not which connection carries them.
        assert_eq!(
            store
                .spaces
                .iter()
                .find(|space| space.id == "space-remote")
                .and_then(|space| space.client_domain.clone()),
            Some("syd".to_string())
        );
        // The local project is untouched and still ahead of the remote block.
        assert_eq!(store.projects[0], local_projects_before[0]);

        // Ingesting the same tree twice reports no change, so no repaint and
        // no write.
        assert!(!store.ingest_remote_tree("syd", &sample_tree()));
    }

    #[test]
    fn ingesting_keeps_this_devices_own_view_state() {
        let mut store = remote_test_store("syd");
        assert!(store.ingest_remote_tree("syd", &sample_tree()));

        // Things this device decided for itself.
        let project = store
            .projects
            .iter_mut()
            .find(|project| project.id == "rp1")
            .unwrap();
        project.threads_collapsed = true;
        project.active_thread_id = Some("rt2".to_string());
        project.active_note_path = Some("notes/rp1.md".to_string());
        project.threads[0].remote_font_scales.insert(7, 1.5);
        project.threads[0].work_finished_unseen = true;
        let space = store
            .spaces
            .iter_mut()
            .find(|space| space.id == "space-remote")
            .unwrap();
        space.note_vault = Some(SpaceVaultBinding {
            root: PathBuf::from("/home/x/vault"),
            managed: false,
        });

        // Another device renames a thread; the push carries only shared state.
        let mut tree = sample_tree();
        tree.projects[0].threads[1].name = "release".to_string();
        assert!(store.ingest_remote_tree("syd", &tree));

        let project = store
            .projects
            .iter()
            .find(|project| project.id == "rp1")
            .unwrap();
        assert_eq!(project.threads[1].name, "release");
        assert!(project.threads_collapsed);
        assert_eq!(project.active_thread_id.as_deref(), Some("rt2"));
        assert_eq!(project.active_note_path.as_deref(), Some("notes/rp1.md"));
        assert_eq!(project.threads[0].remote_font_scales.get(&7), Some(&1.5));
        assert!(project.threads[0].work_finished_unseen);
        assert!(store
            .spaces
            .iter()
            .find(|space| space.id == "space-remote")
            .unwrap()
            .note_vault
            .is_some());
    }

    #[test]
    fn ingesting_drops_rows_the_server_no_longer_has() {
        let mut store = remote_test_store("syd");
        assert!(store.ingest_remote_tree("syd", &sample_tree()));

        // This device was looking at the thread another device just deleted.
        store
            .projects
            .iter_mut()
            .find(|project| project.id == "rp1")
            .unwrap()
            .active_thread_id = Some("rt2".to_string());

        let mut tree = sample_tree();
        tree.projects[0].threads.pop();
        assert!(store.ingest_remote_tree("syd", &tree));

        let project = store
            .projects
            .iter()
            .find(|project| project.id == "rp1")
            .unwrap();
        assert_eq!(project.threads.len(), 1);
        // Rather than leaving the sidebar pointing at a row that is gone.
        assert_eq!(project.active_thread_id, None);
    }

    #[test]
    fn a_reconcile_creates_what_is_new_and_deletes_only_what_it_knew_about() {
        let mut store = remote_test_store("syd");
        let last_known = sample_tree();
        assert!(store.ingest_remote_tree("syd", &last_known));

        // This device deleted one thread locally...
        assert!(store.delete_thread("rt2").is_some());
        // ...and another device added one we have not been told about yet.
        let ops = store.reconcile_ops_for_domain("syd", &last_known);

        assert!(ops.contains(&codec::TreeOp::DeleteThread {
            thread_id: "rt2".to_string()
        }));
        assert!(!ops.iter().any(|op| matches!(
            op,
            codec::TreeOp::DeleteThread { thread_id } if thread_id == "rt1"
        )));
        // Nothing outside this domain is ever mentioned.
        assert!(!ops.iter().any(|op| matches!(
            op,
            codec::TreeOp::CreateProject { project_id, .. } if project_id == "local-1"
        )));

        // Replaying the reconcile against the server's copy reproduces ours.
        let mut server = last_known.clone();
        for op in &ops {
            codec::apply_op(&mut server, op);
        }
        assert_eq!(
            server.project("rp1").unwrap().threads.len(),
            store
                .projects
                .iter()
                .find(|project| project.id == "rp1")
                .unwrap()
                .threads
                .len()
        );
    }

    /// A reconcile restates only what *this* device changed. Anything still
    /// matching the server's last word is left alone, so a compound path here
    /// cannot revert an edit another device made in the meantime.
    #[test]
    fn a_reconcile_does_not_revert_what_another_device_changed() {
        let mut store = remote_test_store("syd");
        let last_known = sample_tree();
        assert!(store.ingest_remote_tree("syd", &last_known));

        // Nothing has changed on either side: there is nothing to say.
        assert!(store
            .reconcile_ops_for_domain("syd", &last_known)
            .is_empty());

        // This device renames one thread.
        assert!(store.rename_thread("rt1", "shell".to_string()));
        let ops = store.reconcile_ops_for_domain("syd", &last_known);

        // Meanwhile another device renamed the Space and the *other* thread,
        // and we have not been told yet.
        let mut server = last_known.clone();
        assert!(codec::apply_op(
            &mut server,
            &codec::TreeOp::RenameSpace {
                space_id: "space-remote".to_string(),
                name: "sydney".to_string(),
            }
        ));
        assert!(codec::apply_op(
            &mut server,
            &codec::TreeOp::RenameThread {
                thread_id: "rt2".to_string(),
                name: "tests".to_string(),
                last_active_at: 99,
            }
        ));

        for op in &ops {
            codec::apply_op(&mut server, op);
        }

        assert_eq!(server.thread("rt1").unwrap().name, "shell");
        assert_eq!(server.space("space-remote").unwrap().name, "sydney");
        assert_eq!(server.thread("rt2").unwrap().name, "tests");
        assert_eq!(server.thread("rt2").unwrap().last_active_at, 99);
    }

    /// Disconnecting a remote Space leaves it on the server for the other
    /// devices, so every later push still carries it; this device filters it
    /// out until it connects to that server again.
    #[test]
    fn a_locally_disconnected_space_is_filtered_out_of_later_pushes() {
        let mut pushed = sample_tree();
        assert!(codec::apply_op(
            &mut pushed,
            &codec::TreeOp::CreateSpace {
                space_id: "space-remote-2".to_string(),
                name: "side".to_string(),
            }
        ));
        assert!(codec::apply_op(
            &mut pushed,
            &codec::TreeOp::CreateProject {
                project_id: "rp2".to_string(),
                space_id: "space-remote-2".to_string(),
                name: "side project".to_string(),
                path: "/srv/side".to_string(),
            }
        ));

        let mut hidden = std::collections::HashSet::new();
        hidden.insert("space-remote-2".to_string());
        strip_hidden_spaces(&mut pushed, &hidden);

        assert!(pushed.space("space-remote-2").is_none());
        // Its projects go with it, or they would be orphan rows pointing at a
        // Space this device cannot draw.
        assert!(pushed.project("rp2").is_none());
        assert!(pushed.space("space-remote").is_some());
        assert!(pushed.project("rp1").is_some());
    }

    /// A push cut just before an online RPC lands may carry the previous tree.
    /// The connection-scoped overlay avoids a visual rollback, but is retired
    /// as soon as the server response accounts for it.
    #[test]
    fn in_flight_ops_survive_an_overtaking_push_only_until_ack() {
        let domain = "in-flight-test-domain";
        take_in_flight_tree_ops(domain);

        queue_in_flight_tree_ops(
            domain,
            &[codec::TreeOp::CreateSpace {
                space_id: "space-offline".to_string(),
                name: "Space 2".to_string(),
            }],
        );
        queue_in_flight_tree_ops(
            domain,
            &[codec::TreeOp::RenameSpace {
                space_id: "space-offline".to_string(),
                name: "Frontend".to_string(),
            }],
        );

        let mut unacked = take_in_flight_tree_ops(domain);
        assert_eq!(unacked.len(), 2);
        // Taking them empties the queue: they must not be replayed twice.
        assert!(take_in_flight_tree_ops(domain).is_empty());

        // A tree cut before either op reached the server: both are replayed on
        // top of it, and both stay queued because it does not show them.
        let mut stale = sample_tree();
        assert!(stale.space("space-offline").is_none());
        unacked.retain(|op| codec::apply_op(&mut stale, op));
        assert_eq!(stale.space("space-offline").unwrap().name, "Frontend");
        assert_eq!(unacked.len(), 2);

        // The tree that comes back once the server has applied them retires
        // both: re-applying an op the server already has changes nothing, and
        // that is exactly what an acknowledgement looks like here.
        let mut acked = stale.clone();
        unacked.retain(|op| codec::apply_op(&mut acked, op));
        assert!(unacked.is_empty());
        assert_eq!(acked, stale);

        // An op the server answered differently — it deleted the Space rather
        // than accepting the rename — also retires, instead of being replayed
        // forever against a row that is gone.
        let mut rename_only = vec![codec::TreeOp::RenameSpace {
            space_id: "space-offline".to_string(),
            name: "Frontend".to_string(),
        }];
        let mut without_it = sample_tree();
        rename_only.retain(|op| codec::apply_op(&mut without_it, op));
        assert!(rename_only.is_empty());
    }

    #[test]
    fn shared_remote_mutations_require_a_healthy_attached_connection() {
        use mux::domain::DomainState;

        assert!(remote_tree_connection_can_mutate(
            DomainState::Attached,
            false,
            false,
            false
        ));
        assert!(!remote_tree_connection_can_mutate(
            DomainState::Detached,
            false,
            false,
            false
        ));
        assert!(!remote_tree_connection_can_mutate(
            DomainState::Attached,
            true,
            false,
            false
        ));
        assert!(!remote_tree_connection_can_mutate(
            DomainState::Attached,
            false,
            true,
            false
        ));
        assert!(!remote_tree_connection_can_mutate(
            DomainState::Attached,
            false,
            false,
            true
        ));
    }

    /// A tree that predates the one we hold is stale news, whichever route it
    /// took — an RPC answer is checked exactly as a broadcast is, or a reply
    /// cut before a push and delivered after it would undo the push.
    #[test]
    fn a_tree_that_predates_what_we_hold_is_ignored() {
        assert!(tree_is_stale(4, Some(5)));
        assert!(!tree_is_stale(5, Some(5)));
        assert!(!tree_is_stale(6, Some(5)));
        // Nothing held yet: the first word on the subject is not stale.
        assert!(!tree_is_stale(0, None));
        // Including revision 0, which would otherwise read as "never written
        // to" and send this device back round the seeding path.
        assert!(tree_is_stale(0, Some(9)));
    }

    /// Connecting is the one thing that clears the baseline, so a server that
    /// came back having lost or rolled back its own copy is re-adopted rather
    /// than ignored for the rest of the session. It also un-hides the Spaces
    /// this device disconnected from, which is what the menu promises.
    #[test]
    fn connecting_clears_the_baseline_a_restarted_server_would_be_measured_against() {
        let domain = "reconnect-test-domain";
        LAST_KNOWN_REMOTE_TREES.lock().insert(
            domain.to_string(),
            codec::ThinkTermTree {
                revision: 9,
                ..Default::default()
            },
        );
        let mut hidden = std::collections::HashSet::new();
        hidden.insert("space-hidden".to_string());
        LOCALLY_HIDDEN_SPACES
            .lock()
            .insert(domain.to_string(), hidden);
        queue_in_flight_tree_ops(
            domain,
            &[codec::TreeOp::RenameSpace {
                space_id: "space-old-generation".to_string(),
                name: "stale intent".to_string(),
            }],
        );

        // While that baseline stands, a server restarting at revision 1 is
        // rightly dismissed as stale.
        let known = LAST_KNOWN_REMOTE_TREES
            .lock()
            .get(domain)
            .map(|tree| tree.revision);
        assert!(tree_is_stale(1, known));

        note_remote_connected(domain);

        assert!(LAST_KNOWN_REMOTE_TREES.lock().get(domain).is_none());
        assert!(take_in_flight_tree_ops(domain).is_empty());
        assert!(LOCALLY_HIDDEN_SPACES.lock().get(domain).is_none());
        assert!(FIRST_TREE_PENDING.lock().contains(domain));
        let known = LAST_KNOWN_REMOTE_TREES
            .lock()
            .get(domain)
            .map(|tree| tree.revision);
        assert!(!tree_is_stale(1, known));

        // The catch-up is a one-shot and may mint a Space, but never flushes
        // an intent left over from the connection that just ended.
        assert!(FIRST_TREE_PENDING.lock().remove(domain));
        assert!(!FIRST_TREE_PENDING.lock().remove(domain));
    }

    /// A re-home is a notification the target window has yet to act on, so a
    /// window orphaned by one server is still parked on that server's dead
    /// Space when the next server speaks. Only the server that owned the Space
    /// may move it.
    #[test]
    fn one_servers_push_does_not_claim_another_servers_orphan() {
        let mut store = remote_test_store("syd");
        assert!(store.ingest_remote_tree("syd", &sample_tree()));
        store.spaces.push(Space {
            id: "space-ams".to_string(),
            name: "ams".to_string(),
            active_project_id: None,
            note_vault: None,
            is_default: false,
            client_domain: Some("ams".to_string()),
        });

        let owned_by_syd = store.space_ids_for_domain("syd");
        let owned_by_ams = store.space_ids_for_domain("ams");
        assert!(owned_by_syd.contains("space-remote"));
        assert!(owned_by_ams.contains("space-ams"));

        // A window is sitting on syd's Space, and syd has just dropped it.
        let owner_id = next_space_owner_id();
        WINDOW_SPACES
            .lock()
            .insert(owner_id, "space-remote".to_string());
        store.spaces.retain(|space| space.id != "space-remote");

        // ams speaking about its own rows must not touch it...
        let by_ams = orphaned_window_spaces(&store, "ams", &owned_by_ams);
        assert!(
            !by_ams.iter().any(|(owner, _)| *owner == owner_id),
            "ams claimed a window orphaned by syd: {:?}",
            by_ams
        );

        // ...but syd, whose Space it was, re-homes it onto a Space it has.
        let by_syd = orphaned_window_spaces(&store, "syd", &owned_by_syd);
        assert!(
            by_syd.iter().any(|(owner, _)| *owner == owner_id),
            "syd did not re-home the window it orphaned: {:?}",
            by_syd
        );

        WINDOW_SPACES.lock().remove(&owner_id);
    }

    /// Deleting a host has to find every Space that host brought in, and a
    /// host is connectable under more than one name: the label for a ThinkTerm
    /// Connect domain, `ssh:user@host` for a direct one. Matching only one of
    /// them leaves the other's Spaces stranded — visible, unconnectable, and
    /// refusing every rename or delete because those go through a server this
    /// device can no longer name.
    #[test]
    fn a_hosts_spaces_are_found_under_every_name_it_connects_under() {
        let mut store = remote_test_store("DO SYD");
        store.spaces.push(Space {
            id: "space-direct".to_string(),
            name: "direct".to_string(),
            active_project_id: None,
            note_vault: None,
            is_default: false,
            client_domain: Some("ssh:x@203.0.113.9".to_string()),
        });
        store.spaces.push(Space {
            id: "space-elsewhere".to_string(),
            name: "another server".to_string(),
            active_project_id: None,
            note_vault: None,
            is_default: false,
            client_domain: Some("DO AMS".to_string()),
        });

        let names = vec!["ssh:x@203.0.113.9".to_string(), "DO SYD".to_string()];
        assert_eq!(
            store.space_ids_for_domains(&names),
            vec!["space-remote".to_string(), "space-direct".to_string()]
        );

        // Another server's Spaces, and the purely local one, stay put.
        assert!(!store
            .space_ids_for_domains(&names)
            .contains(&"space-elsewhere".to_string()));

        let mut domains = store.remote_space_domains();
        domains.sort();
        assert_eq!(domains, vec!["DO AMS", "DO SYD", "ssh:x@203.0.113.9"]);
    }

    /// Ending the sessions of one Space must not reach into its siblings on
    /// the same server, which the domain id alone cannot distinguish.
    #[test]
    fn a_spaces_workspaces_do_not_include_its_siblings_on_the_same_server() {
        let mut store = remote_test_store("syd");
        assert!(store.ingest_remote_tree("syd", &sample_tree()));
        store.spaces.push(Space {
            id: "space-remote-2".to_string(),
            name: "side".to_string(),
            active_project_id: None,
            note_vault: None,
            is_default: false,
            client_domain: Some("syd".to_string()),
        });
        store.projects.push(Project {
            id: "rp2".to_string(),
            space_id: "space-remote-2".to_string(),
            name: "side project".to_string(),
            path: PathBuf::from("/srv/side"),
            threads: vec![WorkspaceThread {
                materialized_workspace_name: Some("thinkterm:rp2:rt9".to_string()),
                ..WorkspaceThread::new("rp2".to_string(), "main".to_string(), None)
            }],
            active_thread_id: None,
            threads_collapsed: false,
            active_note_path: None,
        });

        let mine = store.materialized_workspaces_for_space("space-remote");
        assert_eq!(mine, ["thinkterm:rp1:rt1", "thinkterm:rp1:rt2"]);
        assert!(!mine.iter().any(|ws| ws == "thinkterm:rp2:rt9"));
        assert_eq!(
            store.materialized_workspaces_for_space("space-remote-2"),
            ["thinkterm:rp2:rt9"]
        );
    }

    #[test]
    fn a_server_can_host_several_spaces_and_reconnect_returns_to_the_last_one() {
        let mut store = remote_test_store("syd");
        store.spaces.push(Space {
            id: "space-remote-2".to_string(),
            name: "side".to_string(),
            active_project_id: None,
            note_vault: None,
            is_default: false,
            client_domain: Some("syd".to_string()),
        });

        // With nothing remembered, connecting lands on the server's first.
        assert_eq!(
            store.preferred_space_for_domain("syd").as_deref(),
            Some("space-remote")
        );

        assert!(store.remember_space_for_domain("space-remote-2"));
        assert_eq!(
            store.preferred_space_for_domain("syd").as_deref(),
            Some("space-remote-2")
        );
        // Recording the same Space twice is not a change worth persisting.
        assert!(!store.remember_space_for_domain("space-remote-2"));

        // A remembered Space the server has since dropped falls back rather
        // than stranding the connection.
        store.spaces.retain(|space| space.id != "space-remote-2");
        assert_eq!(
            store.preferred_space_for_domain("syd").as_deref(),
            Some("space-remote")
        );

        // Another server's Space is never offered for this one, and a domain
        // with no Spaces reports none.
        assert!(store.preferred_space_for_domain("other").is_none());
        // Local Spaces are not "remembered per domain" at all.
        let local = store.spaces[0].id.clone();
        assert!(!store.remember_space_for_domain(&local));
    }

    /// A live remote window belonging to a sibling Space on the same server
    /// must not be adopted by whichever Space happens to be active: that would
    /// move a running terminal out from under the Space that owns it.
    #[test]
    fn a_sibling_spaces_workspace_is_not_mistaken_for_an_orphan() {
        let host = mux_domain_host_id("syd");
        let mine = remote_project_id_for_space("space-remote", &host);
        let sibling = remote_project_id_for_space("space-remote-2", &host);

        assert_eq!(space_id_from_remote_project_id(&mine), Some("space-remote"));
        assert_eq!(
            space_id_from_remote_project_id(&sibling),
            Some("space-remote-2")
        );
        // A plain local project id carries no Space, so the orphan path still
        // applies to it.
        assert_eq!(space_id_from_remote_project_id("project-1"), None);

        let mut store = remote_test_store("syd");
        assert!(store.has_space("space-remote"));
        assert!(!store.has_space("space-remote-2"));
        // With the sibling Space unknown, its rows really are orphaned.
        assert!(!space_id_from_remote_project_id(&sibling)
            .is_some_and(|s| s != "space-remote" && store.has_space(s)));

        store.spaces.push(Space {
            id: "space-remote-2".to_string(),
            name: "side".to_string(),
            active_project_id: None,
            note_vault: None,
            is_default: false,
            client_domain: Some("syd".to_string()),
        });
        // Once it is known, they are hands off.
        assert!(space_id_from_remote_project_id(&sibling)
            .is_some_and(|s| s != "space-remote" && store.has_space(s)));
        // Our own Space's rows are never "a sibling's".
        assert!(!space_id_from_remote_project_id(&mine)
            .is_some_and(|s| s != "space-remote" && store.has_space(s)));
    }

    /// Two Spaces on one server must not collide: the mux-domain project id
    /// embeds the Space id, and the thread workspace name embeds the project.
    #[test]
    fn two_spaces_on_one_server_get_distinct_projects_and_workspaces() {
        let host = mux_domain_host_id("syd");
        let a = remote_project_id_for_space("space-remote", &host);
        let b = remote_project_id_for_space("space-remote-2", &host);
        assert_ne!(a, b);
        assert_eq!(remote_host_id_for_project_id(&a), host);
        assert_eq!(remote_host_id_for_project_id(&b), host);

        let ws_a = workspace_name_for_thread(&a, "thread-1");
        let ws_b = workspace_name_for_thread(&b, "thread-1");
        assert_ne!(ws_a, ws_b);
        assert_eq!(
            parse_thread_workspace_name(&ws_a),
            Some((a, "thread-1".to_string()))
        );
    }

    /// The local store and `codec::apply_op` implement the same row-ordering
    /// and pin rules in two different places. If they ever drift, a drag would
    /// look right until the server's push snapped it back.
    #[test]
    fn store_and_wire_ordering_rules_agree() {
        let mut store = remote_test_store("syd");
        let mut tree = sample_tree();
        tree.projects.push(codec::TtProject {
            id: "rp2".to_string(),
            space_id: "space-remote".to_string(),
            name: "notes".to_string(),
            path: "/srv/notes".to_string(),
            threads: vec![tree_thread("rt3", "rp2", "main")],
        });
        assert!(store.ingest_remote_tree("syd", &tree));

        let cases: Vec<codec::TreeOp> = vec![
            codec::TreeOp::MoveProjectBefore {
                space_id: "space-remote".to_string(),
                project_id: "rp2".to_string(),
                before: Some("rp1".to_string()),
            },
            codec::TreeOp::MoveProjectBefore {
                space_id: "space-remote".to_string(),
                project_id: "rp2".to_string(),
                before: None,
            },
            codec::TreeOp::MoveThreadBefore {
                project_id: "rp1".to_string(),
                thread_id: "rt2".to_string(),
                before: Some("rt1".to_string()),
            },
            codec::TreeOp::MoveThreadBefore {
                project_id: "rp1".to_string(),
                thread_id: "rt2".to_string(),
                before: None,
            },
            // A pinned row is refused as both subject and anchor on both sides.
            codec::TreeOp::SetThreadPinned {
                thread_id: "rt1".to_string(),
                pinned: true,
                last_active_at: 10,
            },
            codec::TreeOp::MoveThreadBefore {
                project_id: "rp1".to_string(),
                thread_id: "rt1".to_string(),
                before: None,
            },
            codec::TreeOp::MoveThreadBefore {
                project_id: "rp1".to_string(),
                thread_id: "rt2".to_string(),
                before: Some("rt1".to_string()),
            },
        ];

        for op in &cases {
            let wire_changed = codec::apply_op(&mut tree, op);
            let store_changed = match op {
                codec::TreeOp::MoveProjectBefore {
                    space_id,
                    project_id,
                    before,
                } => store.move_project_before(space_id, project_id, before.as_deref()),
                codec::TreeOp::MoveThreadBefore {
                    project_id,
                    thread_id,
                    before,
                } => store.move_thread_before(project_id, thread_id, before.as_deref()),
                codec::TreeOp::SetThreadPinned { thread_id, .. } => {
                    store.toggle_thread_pinned(thread_id)
                }
                other => panic!("unhandled case {:?}", other),
            };
            assert_eq!(wire_changed, store_changed, "differing verdict on {op:?}");

            let wire_projects: Vec<&str> = tree
                .projects_in_space("space-remote")
                .map(|project| project.id.as_str())
                .collect();
            let store_projects: Vec<&str> = store
                .projects
                .iter()
                .filter(|project| project.space_id == "space-remote")
                .map(|project| project.id.as_str())
                .collect();
            assert_eq!(wire_projects, store_projects, "project order after {op:?}");

            for project_id in ["rp1", "rp2"] {
                let wire_threads: Vec<&str> = tree
                    .project(project_id)
                    .unwrap()
                    .threads
                    .iter()
                    .map(|thread| thread.id.as_str())
                    .collect();
                let store_threads: Vec<&str> = store
                    .projects
                    .iter()
                    .find(|project| project.id == project_id)
                    .unwrap()
                    .threads
                    .iter()
                    .map(|thread| thread.id.as_str())
                    .collect();
                assert_eq!(
                    wire_threads, store_threads,
                    "thread order in {project_id} after {op:?}"
                );
            }
        }
    }
}
