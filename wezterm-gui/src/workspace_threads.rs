use anyhow::{ensure, Context, Result};
use chrono::Utc;
use config::keyassignment::{SpawnCommand, SpawnTabDomain};
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
    pub projects: Vec<Project>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Space {
    pub id: SpaceId,
    pub name: String,
    pub active_project_id: Option<ProjectId>,
    #[serde(default)]
    pub is_default: bool,
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
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkspaceThread {
    pub id: WorkspaceThreadId,
    pub name: String,
    pub project_id: ProjectId,
    pub layout: Option<WorkspaceThreadLayoutSnapshot>,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WorkspaceThreadWorkChange {
    changed: bool,
    should_persist: bool,
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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpaceView {
    pub id: SpaceId,
    pub name: String,
    pub is_active: bool,
    pub is_default: bool,
    pub is_occupied_by_other_window: bool,
}

lazy_static::lazy_static! {
    static ref THREAD_STORE: Mutex<WorkspaceThreadStore> =
        Mutex::new(load_workspace_thread_store().unwrap_or_else(|err| {
            log::warn!("failed to load ThinkTerm workspace thread store: {err:#}");
            WorkspaceThreadStore::default()
        }));
    static ref WINDOW_SPACES: Mutex<HashMap<u64, SpaceId>> = Mutex::new(HashMap::new());
    static ref MATERIALIZING_LAYOUT_WORKSPACES: Mutex<HashMap<String, usize>> =
        Mutex::new(HashMap::new());
    static ref WORK_RUNNING_LAST_SEEN: Mutex<HashMap<String, std::time::Instant>> =
        Mutex::new(HashMap::new());
    static ref WORK_STATUS_RECHECK_PENDING: Mutex<std::collections::HashSet<String>> =
        Mutex::new(std::collections::HashSet::new());
}

static THREAD_STORE_PERSIST_SCHEDULED: AtomicBool = AtomicBool::new(false);
static THREAD_STORE_PERSIST_DIRTY: AtomicBool = AtomicBool::new(false);
static NEXT_SPACE_OWNER_ID: AtomicU64 = AtomicU64::new(1);

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

pub fn release_window_space(owner_id: u64) {
    WINDOW_SPACES.lock().remove(&owner_id);
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
    persist_locked(&store);
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
    store
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
        })
        .collect()
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

pub fn create_space(name: Option<String>) -> SpaceId {
    let mut store = THREAD_STORE.lock();
    store.normalize_after_load();
    let name = name.unwrap_or_else(|| next_space_name(&store.spaces));
    let id = store.create_space_record(name);
    store.last_active_space_id = Some(id.clone());
    persist_locked(&store);
    id
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
    let changed = store.rename_space(space_id, name);
    if changed {
        persist_locked(&store);
    }
    changed
}

pub fn delete_space_for_window(
    owner_id: u64,
    space_id: &str,
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

    current_project_for_workspace(space_id, active_workspace).view(live_workspaces)
}

pub fn sync_current_project(space_id: &str, active_workspace: &str) -> bool {
    let mut store = THREAD_STORE.lock();
    let changed = store.sync_current_project(space_id, active_workspace);
    if changed {
        persist_locked(&store);
    }
    changed
}

pub fn create_thread(project_id: &str, name: Option<String>) -> WorkspaceThreadId {
    let mut store = THREAD_STORE.lock();
    let thread_id = store.create_thread(project_id, name);
    persist_locked(&store);
    thread_id
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
    persist_locked(&store);
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
        .is_some_and(is_remote_project)
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

pub fn refresh_thread_work_for_workspace(workspace: &str) -> bool {
    let observed = scan_workspace_work_status(workspace);
    let observed = debounce_work_status(workspace, observed);
    let mut store = THREAD_STORE.lock();
    let Some(change) = store.observe_thread_work_for_workspace(workspace, observed) else {
        return false;
    };
    if change.should_persist {
        schedule_workspace_thread_store_persist();
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
    let mut store = THREAD_STORE.lock();
    let change = store.acknowledge_thread_work_for_workspace(workspace);
    if change.should_persist {
        persist_locked(&store);
    }
    change.changed
}

pub fn acknowledge_thread_work_for_workspace_deferred(workspace: &str) -> bool {
    let mut store = THREAD_STORE.lock();
    let change = store.acknowledge_thread_work_for_workspace(workspace);
    if change.should_persist {
        schedule_workspace_thread_store_persist();
    }
    change.changed
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

    let Some(snapshot) = snapshot_window_layout(window_id, &pane_font_scale) else {
        return;
    };
    if snapshot.tabs.is_empty() {
        return;
    }

    let mut store = THREAD_STORE.lock();
    if store.snapshot_active_space_thread_layout(space_id, workspace, snapshot) {
        persist_locked(&store);
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

    let layout = layout.and_then(|layout| {
        if layout.tabs.is_empty() {
            None
        } else {
            Some(layout)
        }
    });
    if let Some(layout) = layout {
        let _guard = MaterializeThreadLayoutGuard::new(workspace_name.clone());
        materialize_layout(mux, workspace_name, layout, initial_cwd, size, term_config).await
    } else {
        let (_tab, pane, _window_id) = mux
            .spawn_tab_or_window(
                None,
                default_domain,
                None,
                initial_cwd,
                size,
                None,
                workspace_name,
                None,
            )
            .await
            .context("spawn default thread window")?;
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

pub fn project_reveal_path(project_id: &str) -> Option<PathBuf> {
    let store = THREAD_STORE.lock();
    store.project_reveal_path(project_id)
}

pub fn workspace_pane_font_scales(
    workspace: &str,
    window_id: MuxWindowId,
) -> Option<HashMap<PaneId, Option<f64>>> {
    let store = THREAD_STORE.lock();
    store.workspace_pane_font_scales(workspace, window_id)
}

pub fn rename_project(project_id: &str, name: String) -> bool {
    let mut store = THREAD_STORE.lock();
    let changed = store.rename_project(project_id, name);
    if changed {
        persist_locked(&store);
    }
    changed
}

pub fn remove_project(project_id: &str) -> Option<RemovedProject> {
    let mut store = THREAD_STORE.lock();
    let removed = store.remove_project(project_id);
    if removed.is_some() {
        persist_locked(&store);
    }
    removed
}

pub fn rename_thread(thread_id: &str, name: String) -> bool {
    let mut store = THREAD_STORE.lock();
    let changed = store.rename_thread(thread_id, name);
    if changed {
        persist_locked(&store);
    }
    changed
}

pub fn toggle_thread_pinned(thread_id: &str) -> bool {
    let mut store = THREAD_STORE.lock();
    let changed = store.toggle_thread_pinned(thread_id);
    if changed {
        persist_locked(&store);
    }
    changed
}

pub fn mark_thread_unread(thread_id: &str) -> bool {
    let mut store = THREAD_STORE.lock();
    let changed = store.mark_thread_unread(thread_id);
    if changed {
        persist_locked(&store);
    }
    changed
}

#[allow(dead_code)]
pub fn delete_thread(thread_id: &str) -> Option<DeletedWorkspaceThread> {
    let mut store = THREAD_STORE.lock();
    let deleted = store.delete_thread(thread_id);
    if deleted.is_some() {
        persist_locked(&store);
    }
    deleted
}

pub fn end_workspace_thread_record(thread_id: &str) -> EndWorkspaceThreadResult {
    let mut store = THREAD_STORE.lock();
    let result = store.end_workspace_thread_record(thread_id);
    if !matches!(result, EndWorkspaceThreadResult::Noop) {
        persist_locked(&store);
    }
    result
}

pub fn disconnect_workspace_thread_record(
    thread_id: &str,
    live_workspaces: &[String],
) -> Option<DisconnectedWorkspaceThread> {
    let mut store = THREAD_STORE.lock();
    let (disconnected, changed) =
        store.disconnect_workspace_thread_record(thread_id, live_workspaces);
    if changed {
        persist_locked(&store);
    }
    disconnected
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
    fn view(&self, live_workspaces: &[String]) -> WorkspaceThreadsView {
        WorkspaceThreadsView {
            pinned_threads: vec![],
            projects: vec![ProjectView {
                id: self.id.clone(),
                name: self.name.clone(),
                is_active: true,
                threads_collapsed: self.threads_collapsed,
                threads: thread_views_for_project(self, Some(&self.id), live_workspaces),
                is_remote: is_remote_project(self),
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
                is_default: true,
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
        let id = new_id("space");
        self.spaces.push(Space {
            id: id.clone(),
            name,
            active_project_id: None,
            is_default: false,
        });
        id
    }

    fn claim_available_space_id(
        &mut self,
        occupied: &std::collections::HashSet<SpaceId>,
    ) -> SpaceId {
        self.last_active_space_id
            .clone()
            .filter(|space_id| self.has_space(space_id) && !occupied.contains(space_id))
            .or_else(|| {
                self.spaces
                    .iter()
                    .find(|space| !occupied.contains(&space.id))
                    .map(|space| space.id.clone())
            })
            .unwrap_or_else(|| {
                let name = next_space_name(&self.spaces);
                self.create_space_record(name)
            })
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
                let is_remote = is_remote_project(project);
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
        if is_remote_project(project) || !project.path.is_dir() {
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
        for project in &mut self.projects {
            if is_remote_project(project) {
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
            is_remote: is_remote_project(project),
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
                    };
                }
            }
        }
        WorkspaceThreadWorkChange {
            changed: false,
            should_persist: false,
        }
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

        if !is_remote_project(&self.projects[project_index]) {
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

        if !is_remote_project(&self.projects[project_index]) {
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
}

impl WorkspaceThread {
    fn new(project_id: ProjectId, name: String, workspace: Option<String>) -> Self {
        Self {
            id: new_id("thread"),
            name,
            project_id,
            layout: None,
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
                }
            }
            WorkspaceThreadWorkStatus::NeedsAttention => {
                let changed = !self.work_needs_attention;
                self.work_needs_attention = true;
                WorkspaceThreadWorkChange {
                    changed,
                    should_persist: false,
                }
            }
            WorkspaceThreadWorkStatus::Idle | WorkspaceThreadWorkStatus::FinishedUnseen => {
                let was_running = self.work_is_running;
                let had_attention = self.work_needs_attention;
                let mut should_persist = false;
                if was_running {
                    should_persist = !self.work_finished_unseen;
                    self.work_finished_unseen = true;
                }
                self.work_is_running = false;
                self.work_needs_attention = false;
                WorkspaceThreadWorkChange {
                    changed: was_running || had_attention,
                    should_persist,
                }
            }
        }
    }
}

fn valid_font_scale(font_scale: Option<f64>) -> Option<f64> {
    font_scale.filter(|scale| scale.is_finite() && *scale > 0.0)
}

fn persist_locked(store: &WorkspaceThreadStore) {
    if let Err(err) = save_workspace_thread_store(store) {
        log::warn!("failed to save ThinkTerm thread store: {err:#}");
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
            let store = THREAD_STORE.lock().clone();
            persist_locked(&store);
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

async fn materialize_layout(
    mux: Arc<Mux>,
    workspace_name: String,
    layout: WorkspaceThreadLayoutSnapshot,
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
    for tab_value in &layout.tabs {
        let node: PaneNode =
            serde_json::from_value(tab_value.clone()).context("decode thread tab layout")?;
        let first_entry = first_pane_entry(&node);
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
            &node,
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

fn is_remote_project(project: &Project) -> bool {
    project.id.starts_with("ssh-")
        || project.id.starts_with("system-ssh-")
        || project.path.to_string_lossy().starts_with("ssh://")
}

fn thread_has_restorable_workspace(thread: &WorkspaceThread) -> bool {
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
    use tempfile::tempdir;

    fn test_store() -> WorkspaceThreadStore {
        let mut store = WorkspaceThreadStore::default();
        store.normalize_after_load();
        store
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
        }
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
}
