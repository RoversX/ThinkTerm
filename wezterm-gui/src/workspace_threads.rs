use anyhow::{ensure, Context, Result};
use chrono::Utc;
use config::keyassignment::SpawnTabDomain;
use futures::future::LocalBoxFuture;
use mux::domain::SplitSource;
use mux::pane::PaneId;
use mux::tab::{PaneEntry, PaneNode, PaneStackEntry, SplitDirection, SplitRequest, SplitSize};
use mux::window::WindowId as MuxWindowId;
use mux::Mux;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use wezterm_term::Progress;
use wezterm_term::TerminalConfiguration;
use wezterm_term::TerminalSize;

pub type ProjectId = String;
pub type WorkspaceThreadId = String;

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct WorkspaceThreadStore {
    pub active_project_id: Option<ProjectId>,
    pub projects: Vec<Project>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Project {
    pub id: ProjectId,
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

lazy_static::lazy_static! {
    static ref THREAD_STORE: Mutex<WorkspaceThreadStore> =
        Mutex::new(load_workspace_thread_store().unwrap_or_else(|err| {
            log::warn!("failed to load ThinkTerm workspace thread store: {err:#}");
            WorkspaceThreadStore::default()
        }));
}

static THREAD_STORE_PERSIST_SCHEDULED: AtomicBool = AtomicBool::new(false);
static THREAD_STORE_PERSIST_DIRTY: AtomicBool = AtomicBool::new(false);

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
    serde_json::from_value(value).with_context(|| format!("parse {}", path.display()))
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

pub fn current_project_from_cwd() -> Project {
    let path = std::env::current_dir().unwrap_or_else(|_| config::HOME_DIR.to_path_buf());
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("Home")
        .to_string();
    let id = project_id_for_path(&path);
    Project {
        id,
        name,
        path,
        threads: vec![],
        active_thread_id: None,
        threads_collapsed: false,
    }
}

fn current_project_for_workspace(active_workspace: &str) -> Project {
    let mut project = current_project_from_cwd();
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
    active_workspace: &str,
    live_workspaces: &[String],
) -> WorkspaceThreadsView {
    let store = THREAD_STORE.lock();
    if let Some(project_id) = store
        .project_id_for_workspace(active_workspace)
        .or_else(|| store.active_project_id.clone())
        .filter(|project_id| {
            store
                .projects
                .iter()
                .any(|project| &project.id == project_id)
        })
    {
        return store.view_for_project(&project_id, live_workspaces);
    }

    current_project_for_workspace(active_workspace).view(live_workspaces)
}

pub fn sync_current_project(active_workspace: &str) -> bool {
    let mut store = THREAD_STORE.lock();
    let (project_id, mut changed) =
        if let Some(project_id) = store.project_id_for_workspace(active_workspace) {
            if store.active_project_id.as_deref() != Some(&project_id) {
                store.active_project_id = Some(project_id.clone());
                (project_id, true)
            } else {
                (project_id, false)
            }
        } else {
            store.ensure_current_project(active_workspace)
        };
    changed |= store.sync_active_workspace(&project_id, active_workspace);
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

pub fn create_project_from_path(path: &str) -> Result<WorkspaceThreadId> {
    let path = normalize_project_path(path)?;
    let mut store = THREAD_STORE.lock();
    let thread_id = store.create_project_from_path(path);
    persist_locked(&store);
    Ok(thread_id)
}

/// Create a new thread under a remote (SSH) project bound to `workspace_name`,
/// mark it active, and return its id. Remote host connection details live in
/// `ssh_hosts.json`; the project record here is layout/sidebar state only.
pub fn create_remote_host_thread(
    project_id: &str,
    label: &str,
    path: PathBuf,
    workspace_name: &str,
) -> WorkspaceThreadId {
    let mut store = THREAD_STORE.lock();
    if let Some(project) = store.projects.iter_mut().find(|p| p.id == project_id) {
        project.name = label.to_string();
        project.path = path.clone();
    } else {
        store.projects.push(Project {
            id: project_id.to_string(),
            name: label.to_string(),
            path,
            threads: vec![],
            active_thread_id: None,
            threads_collapsed: false,
        });
    }

    let project = store
        .projects
        .iter_mut()
        .find(|p| p.id == project_id)
        .expect("remote project was just inserted");
    let name = format!("Thread {}", project.threads.len() + 1);
    let session = WorkspaceThread::new(
        project_id.to_string(),
        name,
        Some(workspace_name.to_string()),
    );
    let thread_id = session.id.clone();
    project.active_thread_id = Some(thread_id.clone());
    project.threads.push(session);
    store.active_project_id = Some(project_id.to_string());
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

pub fn refresh_thread_work_for_workspace(workspace: &str) -> bool {
    let observed = scan_workspace_work_status(workspace);
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

pub fn snapshot_workspace_layout_with_font_scales<F>(
    workspace: &str,
    window_id: MuxWindowId,
    pane_font_scale: F,
) where
    F: Fn(PaneId) -> Option<f64>,
{
    let Some(snapshot) = snapshot_window_layout(window_id, &pane_font_scale) else {
        return;
    };

    let mut store = THREAD_STORE.lock();
    store.snapshot_workspace_layout(workspace, snapshot);
    persist_locked(&store);
}

pub fn snapshot_active_thread_layout_with_font_scales<F>(window_id: MuxWindowId, pane_font_scale: F)
where
    F: Fn(PaneId) -> Option<f64>,
{
    let workspace = Mux::get().active_workspace();
    snapshot_workspace_layout_with_font_scales(&workspace, window_id, pane_font_scale);
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

pub fn delete_thread(thread_id: &str) -> Option<DeletedWorkspaceThread> {
    let mut store = THREAD_STORE.lock();
    let deleted = store.delete_thread(thread_id);
    if deleted.is_some() {
        persist_locked(&store);
    }
    deleted
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
    fn ensure_current_project(&mut self, active_workspace: &str) -> (ProjectId, bool) {
        let current = current_project_from_cwd();
        let (project_id, mut changed) =
            if let Some(project) = self.projects.iter().find(|p| p.path == current.path) {
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
        if self.active_project_id.as_deref() != Some(&project_id) {
            self.active_project_id = Some(project_id.clone());
            changed = true;
        }
        (project_id, changed)
    }

    fn view_for_project(
        &self,
        project_id: &str,
        live_workspaces: &[String],
    ) -> WorkspaceThreadsView {
        let active_project_id = self.active_project_id.as_deref().unwrap_or(project_id);
        let pinned_threads = self
            .projects
            .iter()
            .flat_map(|project| {
                thread_views_for_project(project, Some(active_project_id), live_workspaces)
                    .into_iter()
                    .filter(|session| session.is_pinned)
            })
            .collect();
        let projects = self
            .projects
            .iter()
            .map(|project| {
                let is_remote = is_remote_project(project);
                ProjectView {
                    id: project.id.clone(),
                    name: project.name.clone(),
                    is_active: project.id == active_project_id,
                    threads_collapsed: project.threads_collapsed,
                    threads: thread_views_for_project(
                        project,
                        Some(active_project_id),
                        live_workspaces,
                    )
                    .into_iter()
                    .filter(|session| !session.is_pinned)
                    .collect(),
                    is_remote,
                    distro: if is_remote {
                        crate::ssh_hosts::host_spec(&project.id)
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

    fn sync_active_workspace(&mut self, project_id: &str, active_workspace: &str) -> bool {
        let Some(project) = self.projects.iter_mut().find(|p| p.id == project_id) else {
            return false;
        };
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

    fn create_thread(&mut self, project_id: &str, name: Option<String>) -> WorkspaceThreadId {
        let project = self
            .projects
            .iter_mut()
            .find(|project| project.id == project_id)
            .expect("project_id should exist before creating session");
        let name = name.unwrap_or_else(|| format!("Thread {}", project.threads.len() + 1));
        let session = WorkspaceThread::new(project.id.clone(), name, None);
        let id = session.id.clone();
        project.threads.push(session);
        id
    }

    fn create_project_from_path(&mut self, path: PathBuf) -> WorkspaceThreadId {
        let project_id = project_id_for_path(&path);
        if let Some(existing_project_id) = self
            .projects
            .iter()
            .find(|project| project.path == path)
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
        self.active_project_id = Some(project_id);
        self.projects.push(project);
        thread_id
    }

    fn project_id_for_workspace(&self, workspace: &str) -> Option<ProjectId> {
        self.projects.iter().find_map(|project| {
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
        let project = self
            .projects
            .iter_mut()
            .find(|project| project.id == project_id)?;
        if project.active_thread_id.as_ref().is_some_and(|active_id| {
            project
                .threads
                .iter()
                .any(|session| &session.id == active_id)
        }) {
            self.active_project_id = Some(project.id.clone());
            return project.active_thread_id.clone();
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
        self.active_project_id = Some(project.id.clone());
        Some(thread_id)
    }

    fn activate_thread_record(
        &mut self,
        thread_id: &str,
        live_workspaces: &[String],
    ) -> Option<ActivationPlan> {
        let project = self.projects.iter_mut().find(|project| {
            project
                .threads
                .iter()
                .any(|session| session.id == thread_id)
        })?;
        let project_id = project.id.clone();
        let session = project
            .threads
            .iter_mut()
            .find(|session| session.id == thread_id)?;
        let workspace_name = session
            .materialized_workspace_name
            .clone()
            .unwrap_or_else(|| workspace_name_for_thread(&project_id, &session.id));
        let needs_materialize = !live_workspaces.iter().any(|live| live == &workspace_name);
        session.materialized_workspace_name = Some(workspace_name.clone());
        session.last_active_at = now_ts();
        session.is_unread = false;
        session.work_finished_unseen = false;
        project.active_thread_id = Some(session.id.clone());
        self.active_project_id = Some(project.id.clone());
        Some(ActivationPlan {
            project_id,
            thread_id: session.id.clone(),
            workspace_name,
            project_path: project.path.clone(),
            needs_materialize,
        })
    }

    fn snapshot_workspace_layout(
        &mut self,
        workspace: &str,
        snapshot: WorkspaceThreadLayoutSnapshot,
    ) {
        for project in &mut self.projects {
            for session in &mut project.threads {
                if session.materialized_workspace_name.as_deref() == Some(workspace) {
                    session.layout = Some(snapshot);
                    session.last_active_at = now_ts();
                    return;
                }
            }
        }
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
            if let Some(session) = project
                .threads
                .iter_mut()
                .find(|session| session.id == thread_id)
            {
                if session.name == name {
                    return false;
                }
                session.name = name.to_string();
                session.last_active_at = now_ts();
                return true;
            }
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

    fn remove_project(&mut self, project_id: &str) -> Option<RemovedProject> {
        if self.projects.len() <= 1 {
            return None;
        }

        let index = self
            .projects
            .iter()
            .position(|project| project.id == project_id)?;
        let removed = self.projects.remove(index);
        let was_active = self.active_project_id.as_deref() == Some(project_id);
        let materialized_workspace_names = removed
            .threads
            .iter()
            .filter_map(|session| session.materialized_workspace_name.clone())
            .collect::<Vec<_>>();

        let next_thread_id = if was_active {
            let next_index = index.saturating_sub(1).min(self.projects.len() - 1);
            let project = &mut self.projects[next_index];
            self.active_project_id = Some(project.id.clone());
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

fn project_id_for_path(path: &Path) -> ProjectId {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in path.as_os_str().to_string_lossy().bytes() {
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
    Ok(canonical)
}

fn workspace_name_for_thread(project_id: &str, thread_id: &str) -> String {
    format!("thinkterm:{project_id}:{thread_id}")
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

    #[test]
    fn workspace_thread_store_round_trip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("workspace_threads.json");
        let mut store = WorkspaceThreadStore::default();
        let project = Project {
            id: "project-1".to_string(),
            name: "thinkterm".to_string(),
            path: PathBuf::from("/tmp/thinkterm"),
            threads: vec![WorkspaceThread::new(
                "project-1".to_string(),
                "main".to_string(),
                None,
            )],
            active_thread_id: None,
            threads_collapsed: false,
        };
        store.active_project_id = Some(project.id.clone());
        store.projects.push(project);
        save_workspace_thread_store_to_path(&path, &store).unwrap();
        let loaded = load_workspace_thread_store_from_path(&path).unwrap();
        assert_eq!(loaded.projects[0].path, PathBuf::from("/tmp/thinkterm"));
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
        let mut store = WorkspaceThreadStore::default();
        let mut session = WorkspaceThread::new("project-1".to_string(), "main".to_string(), None);
        session.work_is_running = true;
        store.projects.push(Project {
            id: "project-1".to_string(),
            name: "thinkterm".to_string(),
            path: PathBuf::from("/tmp/thinkterm"),
            threads: vec![session],
            active_thread_id: None,
            threads_collapsed: false,
        });

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
        let workspace = "workspace-1";
        let synthetic = current_project_for_workspace(workspace);
        let synthetic_thread_id = synthetic.threads[0].id.clone();

        let mut store = WorkspaceThreadStore::default();
        let (project_id, changed) = store.ensure_current_project(workspace);

        assert!(changed);
        assert_eq!(project_id, synthetic.id);
        assert_eq!(store.projects[0].threads[0].id, synthetic_thread_id);
        assert_eq!(
            store.projects[0].threads[0]
                .materialized_workspace_name
                .as_deref(),
            Some(workspace)
        );
    }

    #[test]
    fn workspace_work_observation_transitions_to_finished_unseen() {
        let mut store = WorkspaceThreadStore::default();
        let session = WorkspaceThread::new(
            "project-1".to_string(),
            "main".to_string(),
            Some("workspace-1".to_string()),
        );
        store.projects.push(Project {
            id: "project-1".to_string(),
            name: "thinkterm".to_string(),
            path: PathBuf::from("/tmp/thinkterm"),
            threads: vec![session],
            active_thread_id: None,
            threads_collapsed: false,
        });

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
        let mut store = WorkspaceThreadStore::default();
        let mut session = WorkspaceThread::new(
            "project-1".to_string(),
            "main".to_string(),
            Some("workspace-1".to_string()),
        );
        session.work_finished_unseen = true;
        store.projects.push(Project {
            id: "project-1".to_string(),
            name: "thinkterm".to_string(),
            path: PathBuf::from("/tmp/thinkterm"),
            threads: vec![session],
            active_thread_id: None,
            threads_collapsed: false,
        });

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
        let mut store = WorkspaceThreadStore::default();
        let project = Project {
            id: "project-1".to_string(),
            name: "thinkterm".to_string(),
            path: PathBuf::from("/tmp/thinkterm"),
            threads: vec![WorkspaceThread::new(
                "project-1".to_string(),
                "main".to_string(),
                None,
            )],
            active_thread_id: None,
            threads_collapsed: false,
        };
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
        let dir = tempdir().unwrap();
        let mut store = WorkspaceThreadStore::default();

        let thread_id = store.create_project_from_path(dir.path().to_path_buf());
        let project_id = store.active_project_id.clone().unwrap();
        let workspace_name = workspace_name_for_thread(&project_id, &thread_id);

        assert_eq!(
            store.project_id_for_workspace(&workspace_name),
            Some(project_id.clone())
        );

        let plan = store.activate_thread_record(&thread_id, &[]).unwrap();
        assert!(plan.needs_materialize);
        assert_eq!(plan.project_path, dir.path());
        assert_eq!(plan.workspace_name, workspace_name);
    }

    #[test]
    fn duplicate_project_path_reuses_existing_thread() {
        let dir = tempdir().unwrap();
        let mut store = WorkspaceThreadStore::default();

        let first_thread_id = store.create_project_from_path(dir.path().to_path_buf());
        let second_thread_id = store.create_project_from_path(dir.path().to_path_buf());

        assert_eq!(second_thread_id, first_thread_id);
        assert_eq!(store.projects.len(), 1);
        assert_eq!(store.projects[0].threads.len(), 1);
    }

    #[test]
    fn duplicate_project_path_reuses_stored_project_id() {
        let dir = tempdir().unwrap();
        let mut store = WorkspaceThreadStore::default();

        let project = Project {
            id: "stored-project-id".to_string(),
            name: "existing".to_string(),
            path: dir.path().to_path_buf(),
            threads: vec![WorkspaceThread::new(
                "stored-project-id".to_string(),
                "main".to_string(),
                None,
            )],
            active_thread_id: None,
            threads_collapsed: false,
        };
        let thread_id = project.threads[0].id.clone();
        store.projects.push(project);

        let reused_thread_id = store.create_project_from_path(dir.path().to_path_buf());

        assert_eq!(reused_thread_id, thread_id);
        assert_eq!(
            store.active_project_id.as_deref(),
            Some("stored-project-id")
        );
        assert_eq!(store.projects.len(), 1);
    }

    #[test]
    fn snapshot_layout_attaches_to_materialized_thread() {
        let mut store = WorkspaceThreadStore::default();
        let mut session = WorkspaceThread::new(
            "project-1".to_string(),
            "main".to_string(),
            Some("ws".to_string()),
        );
        let thread_id = session.id.clone();
        let project = Project {
            id: "project-1".to_string(),
            name: "thinkterm".to_string(),
            path: PathBuf::from("/tmp/thinkterm"),
            threads: vec![session.clone()],
            active_thread_id: Some(thread_id),
            threads_collapsed: false,
        };
        store.projects.push(project);
        store.snapshot_workspace_layout(
            "ws",
            WorkspaceThreadLayoutSnapshot {
                active_tab: 0,
                tabs: vec![serde_json::json!({"kind": "test"})],
                terminal_specs: vec![],
            },
        );
        session = store.projects[0].threads[0].clone();
        assert_eq!(session.layout.unwrap().tabs.len(), 1);
    }

    #[test]
    fn view_marks_only_global_active_workspace_thread_active() {
        let mut store = WorkspaceThreadStore::default();
        let first = WorkspaceThread::new("project-1".to_string(), "main".to_string(), None);
        let second = WorkspaceThread::new("project-2".to_string(), "current".to_string(), None);
        let first_id = first.id.clone();
        let second_id = second.id.clone();
        store.projects.push(Project {
            id: "project-1".to_string(),
            name: "thinkterm".to_string(),
            path: PathBuf::from("/tmp/thinkterm"),
            threads: vec![first],
            active_thread_id: Some(first_id),
            threads_collapsed: false,
        });
        store.projects.push(Project {
            id: "project-2".to_string(),
            name: "agent_dock".to_string(),
            path: PathBuf::from("/tmp/agent_dock"),
            threads: vec![second],
            active_thread_id: Some(second_id),
            threads_collapsed: false,
        });
        store.active_project_id = Some("project-2".to_string());

        let view = store.view_for_project("project-2", &[]);
        assert!(!view.projects[0].is_active);
        assert!(!view.projects[0].threads[0].is_active);
        assert!(view.projects[1].is_active);
        assert!(view.projects[1].threads[0].is_active);
    }

    #[test]
    fn project_menu_metadata_actions_update_store() {
        let mut store = WorkspaceThreadStore::default();
        let first = WorkspaceThread::new("project-1".to_string(), "main".to_string(), None);
        let second = WorkspaceThread::new("project-2".to_string(), "current".to_string(), None);
        let first_id = first.id.clone();
        let second_id = second.id.clone();
        store.projects.push(Project {
            id: "project-1".to_string(),
            name: "thinkterm".to_string(),
            path: PathBuf::from("/tmp/thinkterm"),
            threads: vec![first],
            active_thread_id: Some(first_id),
            threads_collapsed: false,
        });
        store.projects.push(Project {
            id: "project-2".to_string(),
            name: "agent_dock".to_string(),
            path: PathBuf::from("/tmp/agent_dock"),
            threads: vec![second],
            active_thread_id: Some(second_id.clone()),
            threads_collapsed: false,
        });
        store.active_project_id = Some("project-2".to_string());

        assert!(store.rename_project("project-2", "Agents".to_string()));
        assert_eq!(store.projects[1].name, "Agents");

        let removed = store.remove_project("project-2").unwrap();
        assert!(removed.was_active);
        assert_eq!(
            removed.next_thread_id,
            Some(store.projects[0].threads[0].id.clone())
        );
        assert_eq!(store.projects.len(), 1);
        assert_eq!(store.active_project_id.as_deref(), Some("project-1"));
        assert!(store.remove_project("project-1").is_none());
    }

    #[test]
    fn thread_menu_metadata_actions_update_store() {
        let mut store = WorkspaceThreadStore::default();
        let mut first = WorkspaceThread::new("project-1".to_string(), "main".to_string(), None);
        let second = WorkspaceThread::new("project-1".to_string(), "Thread 2".to_string(), None);
        let third = WorkspaceThread::new("project-1".to_string(), "Thread 3".to_string(), None);
        let first_id = first.id.clone();
        let second_id = second.id.clone();
        let third_id = third.id.clone();
        first.is_unread = true;
        store.projects.push(Project {
            id: "project-1".to_string(),
            name: "thinkterm".to_string(),
            path: PathBuf::from("/tmp/thinkterm"),
            threads: vec![first, second, third],
            active_thread_id: Some(first_id.clone()),
            threads_collapsed: false,
        });

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

        let view = store.view_for_project("project-1", &[]);
        assert_eq!(view.pinned_threads.len(), 1);
        assert_eq!(view.pinned_threads[0].id, second_id);
        assert_eq!(view.projects[0].threads.len(), 1);
        assert_eq!(view.projects[0].threads[0].id, first_id);
    }
}
