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
use std::sync::Arc;
use wezterm_term::TerminalConfiguration;
use wezterm_term::TerminalSize;

pub type ProjectId = String;
pub type SessionId = String;

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct SessionStore {
    pub active_project_id: Option<ProjectId>,
    pub projects: Vec<Project>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Project {
    pub id: ProjectId,
    pub name: String,
    pub path: PathBuf,
    pub sessions: Vec<Session>,
    pub active_session_id: Option<SessionId>,
    #[serde(default)]
    pub sessions_collapsed: bool,
    /// When set, this "project" is a remote SSH host rather than a local
    /// folder. Local projects leave this `None`.
    #[serde(default)]
    pub remote: Option<SshHostSpec>,
}

/// Stored SSH host definition. A remote [`Project`] carries one of these; the
/// connection layer (`ssh_hosts.rs`) turns it into a live `config::SshDomain`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SshHostSpec {
    /// Display name shown on the host card / sidebar row.
    pub label: String,
    /// Hostname or IP address of the remote server.
    pub host: String,
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default)]
    pub username: Option<String>,
    /// Path to an SSH identity (private key) file, if any.
    #[serde(default)]
    pub identity_file: Option<String>,
    /// Extra `ssh_config` option overrides (key -> value).
    #[serde(default)]
    pub ssh_options: HashMap<String, String>,
    /// Use WezTerm's multiplexed SSH (persistent, reconnecting) when true,
    /// otherwise connect directly like `ssh`.
    #[serde(default = "default_true")]
    pub multiplexing: bool,
    /// Override the default `ssh:<host>` workspace name.
    #[serde(default)]
    pub default_workspace: Option<String>,
    /// When true, run a one-shot `cat /etc/os-release` after connecting to
    /// detect the distro and pick its icon. User-controlled (opt-in).
    #[serde(default = "default_true")]
    pub detect_os: bool,
    /// `/etc/os-release` `ID` detected after connecting; drives the OS icon.
    #[serde(default)]
    pub detected_distro: Option<String>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Session {
    pub id: SessionId,
    pub name: String,
    pub project_id: ProjectId,
    pub layout: Option<SessionLayoutSnapshot>,
    pub materialized_workspace_name: Option<String>,
    pub last_active_at: i64,
    #[serde(default)]
    pub is_pinned: bool,
    #[serde(default)]
    pub is_unread: bool,
    #[serde(skip)]
    pub work_is_running: bool,
    #[serde(default)]
    pub work_finished_unseen: bool,
    #[serde(default)]
    pub archived: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionLayoutSnapshot {
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
pub struct ProjectSessionView {
    pub pinned_sessions: Vec<SessionView>,
    pub projects: Vec<ProjectView>,
}

#[derive(Debug, Clone)]
pub struct ProjectView {
    pub id: ProjectId,
    pub name: String,
    pub is_active: bool,
    pub sessions_collapsed: bool,
    pub sessions: Vec<SessionView>,
    /// True when this project is a remote SSH host rather than a local folder.
    pub is_remote: bool,
    /// Detected `/etc/os-release` `ID` for remote hosts, used to pick an OS icon.
    pub distro: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SessionView {
    pub id: SessionId,
    pub name: String,
    pub workspace_name: String,
    pub is_active: bool,
    pub is_materialized: bool,
    pub is_pinned: bool,
    pub is_unread: bool,
    pub work_finished_unseen: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionWorkStatus {
    Idle,
    Running,
    FinishedUnseen,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivationPlan {
    pub session_id: SessionId,
    pub workspace_name: String,
    pub project_path: PathBuf,
    pub needs_materialize: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeletedSession {
    pub was_active: bool,
    pub next_session_id: Option<SessionId>,
    pub materialized_workspace_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemovedProject {
    pub was_active: bool,
    pub next_session_id: Option<SessionId>,
    pub materialized_workspace_names: Vec<String>,
}

lazy_static::lazy_static! {
    static ref SESSION_STORE: Mutex<SessionStore> =
        Mutex::new(load_session_store().unwrap_or_else(|err| {
            log::warn!("failed to load ThinkTerm session store: {err:#}");
            SessionStore::default()
        }));
}

pub fn session_store_path() -> PathBuf {
    config::DATA_DIR.join("thinkterm").join("sessions.json")
}

fn legacy_session_store_path() -> PathBuf {
    config::CACHE_DIR.join("thinkterm").join("sessions.json")
}

pub fn load_session_store() -> Result<SessionStore> {
    let path = session_store_path();
    if path.exists() {
        return load_session_store_from_path(&path);
    }

    let legacy_path = legacy_session_store_path();
    if legacy_path.exists() {
        let store = load_session_store_from_path(&legacy_path)?;
        if let Err(err) = save_session_store_to_path(&path, &store) {
            log::warn!(
                "failed to migrate ThinkTerm session store from {} to {}: {err:#}",
                legacy_path.display(),
                path.display()
            );
        }
        return Ok(store);
    }

    Ok(SessionStore::default())
}

pub fn load_session_store_from_path(path: &Path) -> Result<SessionStore> {
    if !path.exists() {
        return Ok(SessionStore::default());
    }
    let file = fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    serde_json::from_reader(file).with_context(|| format!("parse {}", path.display()))
}

pub fn save_session_store(store: &SessionStore) -> Result<()> {
    save_session_store_to_path(&session_store_path(), store)
}

pub fn save_session_store_to_path(path: &Path, store: &SessionStore) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
        let mut file = tempfile::NamedTempFile::new_in(parent)
            .with_context(|| format!("create temporary session store in {}", parent.display()))?;
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
        sessions: vec![],
        active_session_id: None,
        sessions_collapsed: false,
        remote: None,
    }
}

pub fn view_for_current_project(
    active_workspace: &str,
    live_workspaces: &[String],
) -> ProjectSessionView {
    let mut store = SESSION_STORE.lock();
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
    let view = store.view_for_project(&project_id, live_workspaces);
    if changed {
        persist_locked(&store);
    }
    view
}

pub fn create_session(project_id: &str, name: Option<String>) -> SessionId {
    let mut store = SESSION_STORE.lock();
    let session_id = store.create_session(project_id, name);
    persist_locked(&store);
    session_id
}

pub fn create_project_from_path(path: &str) -> Result<SessionId> {
    let path = normalize_project_path(path)?;
    let mut store = SESSION_STORE.lock();
    let session_id = store.create_project_from_path(path);
    persist_locked(&store);
    Ok(session_id)
}

/// Return every stored SSH host (remote project) as `(project_id, spec)`.
pub fn list_hosts() -> Vec<(ProjectId, SshHostSpec)> {
    let store = SESSION_STORE.lock();
    store
        .projects
        .iter()
        .filter_map(|project| {
            project
                .remote
                .clone()
                .map(|spec| (project.id.clone(), spec))
        })
        .collect()
}

/// Fetch the SSH host spec for a remote project, if it is one.
pub fn host_spec(project_id: &str) -> Option<SshHostSpec> {
    let store = SESSION_STORE.lock();
    store
        .projects
        .iter()
        .find(|project| project.id == project_id)
        .and_then(|project| project.remote.clone())
}

/// Create (or update, when host+user+port already exist) an SSH host. Returns
/// the remote project id. The host starts with no sessions; connecting creates
/// them.
pub fn create_host(spec: SshHostSpec) -> ProjectId {
    let mut store = SESSION_STORE.lock();
    let project_id = project_id_for_host(&spec);
    if let Some(project) = store.projects.iter_mut().find(|p| p.id == project_id) {
        project.name = spec.label.clone();
        project.remote = Some(spec);
    } else {
        let path = PathBuf::from(format!("ssh://{}", host_display(&spec)));
        store.projects.push(Project {
            id: project_id.clone(),
            name: spec.label.clone(),
            path,
            sessions: vec![],
            active_session_id: None,
            sessions_collapsed: false,
            remote: Some(spec),
        });
    }
    persist_locked(&store);
    project_id
}

/// Update the spec of an existing remote project in place. Returns false when
/// the project does not exist or is not a remote host.
pub fn update_host(project_id: &str, spec: SshHostSpec) -> bool {
    let mut store = SESSION_STORE.lock();
    let Some(project) = store
        .projects
        .iter_mut()
        .find(|p| p.id == project_id && p.remote.is_some())
    else {
        return false;
    };
    project.name = spec.label.clone();
    project.remote = Some(spec);
    persist_locked(&store);
    true
}

/// Remove an SSH host (and any sessions under it). Delegates to the shared
/// project-removal path so workspace cleanup is handled identically.
pub fn remove_host(project_id: &str) -> Option<RemovedProject> {
    remove_project(project_id)
}

/// Record the detected `/etc/os-release` `ID` for a host so the UI can show the
/// matching OS icon. Returns true when the value changed.
pub fn set_host_distro(project_id: &str, distro_id: &str) -> bool {
    let mut store = SESSION_STORE.lock();
    let Some(project) = store.projects.iter_mut().find(|p| p.id == project_id) else {
        return false;
    };
    let Some(remote) = project.remote.as_mut() else {
        return false;
    };
    let new_value = {
        let trimmed = distro_id.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    };
    if remote.detected_distro == new_value {
        return false;
    }
    remote.detected_distro = new_value;
    persist_locked(&store);
    true
}

/// Create a new session under a remote (SSH) project bound to `workspace_name`,
/// mark it active, and return its id. Returns `None` if the project is missing
/// or not a remote host.
pub fn create_host_session(project_id: &str, workspace_name: &str) -> Option<SessionId> {
    let mut store = SESSION_STORE.lock();
    let project = store
        .projects
        .iter_mut()
        .find(|p| p.id == project_id && p.remote.is_some())?;
    let name = format!("Session {}", project.sessions.len() + 1);
    let session = Session::new(
        project_id.to_string(),
        name,
        Some(workspace_name.to_string()),
    );
    let session_id = session.id.clone();
    project.active_session_id = Some(session_id.clone());
    project.sessions.push(session);
    store.active_project_id = Some(project_id.to_string());
    persist_locked(&store);
    Some(session_id)
}

pub fn activate_session_record(
    session_id: &str,
    live_workspaces: &[String],
) -> Option<ActivationPlan> {
    let mut store = SESSION_STORE.lock();
    let plan = store.activate_session_record(session_id, live_workspaces);
    persist_locked(&store);
    plan
}

pub fn observe_session_work(session_id: &str, is_working: bool) -> Option<SessionWorkStatus> {
    let mut store = SESSION_STORE.lock();
    let (status, changed) = store.observe_session_work(session_id, is_working)?;
    if changed {
        persist_locked(&store);
    }
    Some(status)
}

pub fn acknowledge_session_work_for_workspace(workspace: &str) -> bool {
    let mut store = SESSION_STORE.lock();
    let changed = store.acknowledge_session_work_for_workspace(workspace);
    if changed {
        persist_locked(&store);
    }
    changed
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

    let mut store = SESSION_STORE.lock();
    store.snapshot_workspace_layout(workspace, snapshot);
    persist_locked(&store);
}

pub fn snapshot_active_session_layout_with_font_scales<F>(
    window_id: MuxWindowId,
    pane_font_scale: F,
) where
    F: Fn(PaneId) -> Option<f64>,
{
    let workspace = Mux::get().active_workspace();
    snapshot_workspace_layout_with_font_scales(&workspace, window_id, pane_font_scale);
}

pub async fn materialize_session(
    workspace_name: String,
    layout: Option<SessionLayoutSnapshot>,
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
            .context("spawn default session window")?;
        pane.set_config(term_config);
        let _ = src_window_id;
        Ok(())
    }
}

pub fn session_layout(session_id: &str) -> Option<SessionLayoutSnapshot> {
    let store = SESSION_STORE.lock();
    store
        .projects
        .iter()
        .flat_map(|project| project.sessions.iter())
        .find(|session| session.id == session_id)
        .and_then(|session| session.layout.clone())
}

pub fn session_name(session_id: &str) -> Option<String> {
    let store = SESSION_STORE.lock();
    store
        .projects
        .iter()
        .flat_map(|project| project.sessions.iter())
        .find(|session| session.id == session_id)
        .map(|session| session.name.clone())
}

pub fn session_is_pinned(session_id: &str) -> bool {
    let store = SESSION_STORE.lock();
    store
        .projects
        .iter()
        .flat_map(|project| project.sessions.iter())
        .find(|session| session.id == session_id)
        .is_some_and(|session| session.is_pinned)
}

pub fn project_name(project_id: &str) -> Option<String> {
    let store = SESSION_STORE.lock();
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
    let store = SESSION_STORE.lock();
    store.workspace_pane_font_scales(workspace, window_id)
}

pub fn rename_project(project_id: &str, name: String) -> bool {
    let mut store = SESSION_STORE.lock();
    let changed = store.rename_project(project_id, name);
    if changed {
        persist_locked(&store);
    }
    changed
}

pub fn remove_project(project_id: &str) -> Option<RemovedProject> {
    let mut store = SESSION_STORE.lock();
    let removed = store.remove_project(project_id);
    if removed.is_some() {
        persist_locked(&store);
    }
    removed
}

pub fn rename_session(session_id: &str, name: String) -> bool {
    let mut store = SESSION_STORE.lock();
    let changed = store.rename_session(session_id, name);
    if changed {
        persist_locked(&store);
    }
    changed
}

pub fn toggle_session_pinned(session_id: &str) -> bool {
    let mut store = SESSION_STORE.lock();
    let changed = store.toggle_session_pinned(session_id);
    if changed {
        persist_locked(&store);
    }
    changed
}

pub fn mark_session_unread(session_id: &str) -> bool {
    let mut store = SESSION_STORE.lock();
    let changed = store.mark_session_unread(session_id);
    if changed {
        persist_locked(&store);
    }
    changed
}

pub fn archive_session(session_id: &str) -> bool {
    let mut store = SESSION_STORE.lock();
    let changed = store.archive_session(session_id);
    if changed {
        persist_locked(&store);
    }
    changed
}

pub fn delete_session(session_id: &str) -> Option<DeletedSession> {
    let mut store = SESSION_STORE.lock();
    let deleted = store.delete_session(session_id);
    if deleted.is_some() {
        persist_locked(&store);
    }
    deleted
}

pub fn toggle_project_sessions_collapsed(project_id: &str) -> bool {
    let mut store = SESSION_STORE.lock();
    let changed = store.toggle_project_sessions_collapsed(project_id);
    if changed {
        persist_locked(&store);
    }
    changed
}

fn session_views_for_project(
    project: &Project,
    active_project_id: Option<&str>,
    live_workspaces: &[String],
) -> Vec<SessionView> {
    let project_is_active = active_project_id == Some(project.id.as_str());
    project
        .sessions
        .iter()
        .filter(|session| !session.archived)
        .map(|session| {
            let workspace_name = session
                .materialized_workspace_name
                .clone()
                .unwrap_or_else(|| workspace_name_for_session(&project.id, &session.id));
            let is_materialized = live_workspaces.iter().any(|live| live == &workspace_name);
            SessionView {
                id: session.id.clone(),
                name: session.name.clone(),
                workspace_name,
                is_active: project_is_active
                    && project.active_session_id.as_deref() == Some(&session.id),
                is_materialized,
                is_pinned: session.is_pinned,
                is_unread: session.is_unread,
                work_finished_unseen: session.work_finished_unseen,
            }
        })
        .collect()
}

impl SessionStore {
    fn ensure_current_project(&mut self, active_workspace: &str) -> (ProjectId, bool) {
        let current = current_project_from_cwd();
        let (project_id, mut changed) =
            if let Some(project) = self.projects.iter().find(|p| p.path == current.path) {
                (project.id.clone(), false)
            } else {
                let mut project = current;
                let session = Session::new(
                    project.id.clone(),
                    "main".to_string(),
                    Some(active_workspace.to_string()),
                );
                project.active_session_id = Some(session.id.clone());
                project.sessions.push(session);
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

    fn view_for_project(&self, project_id: &str, live_workspaces: &[String]) -> ProjectSessionView {
        let active_project_id = self.active_project_id.as_deref().unwrap_or(project_id);
        let pinned_sessions = self
            .projects
            .iter()
            .flat_map(|project| {
                session_views_for_project(project, Some(active_project_id), live_workspaces)
                    .into_iter()
                    .filter(|session| session.is_pinned)
            })
            .collect();
        let projects = self
            .projects
            .iter()
            .map(|project| ProjectView {
                id: project.id.clone(),
                name: project.name.clone(),
                is_active: project.id == active_project_id,
                sessions_collapsed: project.sessions_collapsed,
                sessions: session_views_for_project(
                    project,
                    Some(active_project_id),
                    live_workspaces,
                )
                .into_iter()
                .filter(|session| !session.is_pinned)
                .collect(),
                is_remote: project.remote.is_some(),
                distro: project
                    .remote
                    .as_ref()
                    .and_then(|spec| spec.detected_distro.clone()),
            })
            .collect();
        ProjectSessionView {
            pinned_sessions,
            projects,
        }
    }

    fn sync_active_workspace(&mut self, project_id: &str, active_workspace: &str) -> bool {
        let Some(project) = self.projects.iter_mut().find(|p| p.id == project_id) else {
            return false;
        };
        if let Some(session) = project.sessions.iter().find(|session| {
            session.materialized_workspace_name.as_deref() == Some(active_workspace)
        }) {
            if project.active_session_id.as_deref() != Some(&session.id) {
                project.active_session_id = Some(session.id.clone());
                return true;
            }
            return false;
        }

        let active_id = project
            .active_session_id
            .clone()
            .or_else(|| project.sessions.first().map(|session| session.id.clone()));
        if let Some(active_id) = active_id {
            if let Some(session) = project
                .sessions
                .iter_mut()
                .find(|session| session.id == active_id)
            {
                let mut changed = false;
                if session.materialized_workspace_name.as_deref() != Some(active_workspace) {
                    session.materialized_workspace_name = Some(active_workspace.to_string());
                    changed = true;
                }
                if project.active_session_id.as_deref() != Some(&session.id) {
                    project.active_session_id = Some(session.id.clone());
                    changed = true;
                }
                return changed;
            }
        }
        false
    }

    fn create_session(&mut self, project_id: &str, name: Option<String>) -> SessionId {
        let project = self
            .projects
            .iter_mut()
            .find(|project| project.id == project_id)
            .expect("project_id should exist before creating session");
        let name = name.unwrap_or_else(|| format!("Session {}", project.sessions.len() + 1));
        let session = Session::new(project.id.clone(), name, None);
        let id = session.id.clone();
        project.sessions.push(session);
        id
    }

    fn create_project_from_path(&mut self, path: PathBuf) -> SessionId {
        let project_id = project_id_for_path(&path);
        if let Some(existing_project_id) = self
            .projects
            .iter()
            .find(|project| project.path == path)
            .map(|project| project.id.clone())
        {
            if let Some(session_id) = self.active_session_for_project(&existing_project_id) {
                return session_id;
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
            sessions: vec![],
            active_session_id: None,
            sessions_collapsed: false,
            remote: None,
        };
        let session = Session::new(project_id.clone(), "main".to_string(), None);
        let session_id = session.id.clone();
        project.active_session_id = Some(session_id.clone());
        project.sessions.push(session);
        self.active_project_id = Some(project_id);
        self.projects.push(project);
        session_id
    }

    fn project_id_for_workspace(&self, workspace: &str) -> Option<ProjectId> {
        self.projects.iter().find_map(|project| {
            project
                .sessions
                .iter()
                .any(|session| {
                    session.materialized_workspace_name.as_deref() == Some(workspace)
                        || workspace_name_for_session(&project.id, &session.id) == workspace
                })
                .then(|| project.id.clone())
        })
    }

    fn active_session_for_project(&mut self, project_id: &str) -> Option<SessionId> {
        let project = self
            .projects
            .iter_mut()
            .find(|project| project.id == project_id)?;
        if project.active_session_id.as_ref().is_some_and(|active_id| {
            project
                .sessions
                .iter()
                .any(|session| !session.archived && &session.id == active_id)
        }) {
            self.active_project_id = Some(project.id.clone());
            return project.active_session_id.clone();
        }

        let session_id = project
            .sessions
            .iter()
            .find(|session| !session.archived)
            .map(|session| session.id.clone())
            .unwrap_or_else(|| {
                let session = Session::new(project.id.clone(), "main".to_string(), None);
                let session_id = session.id.clone();
                project.sessions.push(session);
                session_id
            });
        project.active_session_id = Some(session_id.clone());
        self.active_project_id = Some(project.id.clone());
        Some(session_id)
    }

    fn activate_session_record(
        &mut self,
        session_id: &str,
        live_workspaces: &[String],
    ) -> Option<ActivationPlan> {
        let project = self.projects.iter_mut().find(|project| {
            project
                .sessions
                .iter()
                .any(|session| session.id == session_id)
        })?;
        let project_id = project.id.clone();
        let session = project
            .sessions
            .iter_mut()
            .find(|session| session.id == session_id)?;
        let workspace_name = session
            .materialized_workspace_name
            .clone()
            .unwrap_or_else(|| workspace_name_for_session(&project_id, &session.id));
        let needs_materialize = !live_workspaces.iter().any(|live| live == &workspace_name);
        session.materialized_workspace_name = Some(workspace_name.clone());
        session.last_active_at = now_ts();
        session.is_unread = false;
        session.work_finished_unseen = false;
        project.active_session_id = Some(session.id.clone());
        self.active_project_id = Some(project.id.clone());
        Some(ActivationPlan {
            session_id: session.id.clone(),
            workspace_name,
            project_path: project.path.clone(),
            needs_materialize,
        })
    }

    fn snapshot_workspace_layout(&mut self, workspace: &str, snapshot: SessionLayoutSnapshot) {
        for project in &mut self.projects {
            for session in &mut project.sessions {
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
            .flat_map(|project| project.sessions.iter())
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

    fn rename_session(&mut self, session_id: &str, name: String) -> bool {
        let name = name.trim();
        if name.is_empty() {
            return false;
        }

        for project in &mut self.projects {
            if let Some(session) = project
                .sessions
                .iter_mut()
                .find(|session| session.id == session_id)
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

    fn toggle_session_pinned(&mut self, session_id: &str) -> bool {
        for project in &mut self.projects {
            if let Some(session) = project
                .sessions
                .iter_mut()
                .find(|session| session.id == session_id)
            {
                session.is_pinned = !session.is_pinned;
                session.last_active_at = now_ts();
                return true;
            }
        }
        false
    }

    fn mark_session_unread(&mut self, session_id: &str) -> bool {
        for project in &mut self.projects {
            if let Some(session) = project
                .sessions
                .iter_mut()
                .find(|session| session.id == session_id)
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

    fn observe_session_work(
        &mut self,
        session_id: &str,
        is_working: bool,
    ) -> Option<(SessionWorkStatus, bool)> {
        for project in &mut self.projects {
            if let Some(session) = project
                .sessions
                .iter_mut()
                .find(|session| session.id == session_id)
            {
                if is_working {
                    let changed = !session.work_is_running || session.work_finished_unseen;
                    session.work_is_running = true;
                    session.work_finished_unseen = false;
                    return Some((SessionWorkStatus::Running, changed));
                }

                let changed = if session.work_is_running {
                    session.work_is_running = false;
                    session.work_finished_unseen = true;
                    true
                } else {
                    false
                };
                let status = if session.work_finished_unseen {
                    SessionWorkStatus::FinishedUnseen
                } else {
                    SessionWorkStatus::Idle
                };
                return Some((status, changed));
            }
        }
        None
    }

    fn acknowledge_session_work_for_workspace(&mut self, workspace: &str) -> bool {
        for project in &mut self.projects {
            let project_id = project.id.clone();
            for session in &mut project.sessions {
                let session_workspace = session
                    .materialized_workspace_name
                    .clone()
                    .unwrap_or_else(|| workspace_name_for_session(&project_id, &session.id));
                if session_workspace == workspace {
                    let changed = session.work_is_running || session.work_finished_unseen;
                    session.work_is_running = false;
                    session.work_finished_unseen = false;
                    return changed;
                }
            }
        }
        false
    }

    fn archive_session(&mut self, session_id: &str) -> bool {
        for project in &mut self.projects {
            if project.active_session_id.as_deref() == Some(session_id) {
                return false;
            }
            if let Some(session) = project
                .sessions
                .iter_mut()
                .find(|session| session.id == session_id)
            {
                if session.archived {
                    return false;
                }
                session.archived = true;
                session.last_active_at = now_ts();
                return true;
            }
        }
        false
    }

    fn delete_session(&mut self, session_id: &str) -> Option<DeletedSession> {
        for project in &mut self.projects {
            let Some(index) = project
                .sessions
                .iter()
                .position(|session| session.id == session_id)
            else {
                continue;
            };
            if project.sessions.len() <= 1 {
                return None;
            }

            let removed = project.sessions.remove(index);
            let was_active = project.active_session_id.as_deref() == Some(session_id);
            let next_session_id = if was_active {
                let next_index = index.saturating_sub(1).min(project.sessions.len() - 1);
                let next_id = project.sessions[next_index].id.clone();
                project.active_session_id = Some(next_id.clone());
                Some(next_id)
            } else {
                None
            };

            return Some(DeletedSession {
                was_active,
                next_session_id,
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
            .sessions
            .iter()
            .filter_map(|session| session.materialized_workspace_name.clone())
            .collect::<Vec<_>>();

        let next_session_id = if was_active {
            let next_index = index.saturating_sub(1).min(self.projects.len() - 1);
            let project = &mut self.projects[next_index];
            self.active_project_id = Some(project.id.clone());
            let session_id = project
                .active_session_id
                .clone()
                .or_else(|| {
                    project
                        .sessions
                        .iter()
                        .find(|session| !session.archived)
                        .map(|session| session.id.clone())
                })
                .unwrap_or_else(|| {
                    let session = Session::new(project.id.clone(), "main".to_string(), None);
                    let session_id = session.id.clone();
                    project.sessions.push(session);
                    session_id
                });
            project.active_session_id = Some(session_id.clone());
            Some(session_id)
        } else {
            None
        };

        Some(RemovedProject {
            was_active,
            next_session_id,
            materialized_workspace_names,
        })
    }

    fn toggle_project_sessions_collapsed(&mut self, project_id: &str) -> bool {
        let Some(project) = self
            .projects
            .iter_mut()
            .find(|project| project.id == project_id)
        else {
            return false;
        };
        project.sessions_collapsed = !project.sessions_collapsed;
        true
    }
}

impl Session {
    fn new(project_id: ProjectId, name: String, workspace: Option<String>) -> Self {
        Self {
            id: new_id("session"),
            name,
            project_id,
            layout: None,
            materialized_workspace_name: workspace,
            last_active_at: now_ts(),
            is_pinned: false,
            is_unread: false,
            work_is_running: false,
            work_finished_unseen: false,
            archived: false,
        }
    }
}

fn valid_font_scale(font_scale: Option<f64>) -> Option<f64> {
    font_scale.filter(|scale| scale.is_finite() && *scale > 0.0)
}

fn persist_locked(store: &SessionStore) {
    if let Err(err) = save_session_store(store) {
        log::warn!("failed to save ThinkTerm session store: {err:#}");
    }
}

fn snapshot_window_layout<F>(
    window_id: MuxWindowId,
    pane_font_scale: &F,
) -> Option<SessionLayoutSnapshot>
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
    Some(SessionLayoutSnapshot {
        active_tab,
        tabs,
        terminal_specs,
    })
}

fn pane_font_scales_for_window(
    layout: &SessionLayoutSnapshot,
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
    layout: SessionLayoutSnapshot,
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
            serde_json::from_value(tab_value.clone()).context("decode session tab layout")?;
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
            .context("spawn session tab")?;
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

fn fnv1a(input: &str) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in input.bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

/// `user@host` (or just `host`) for display / synthetic paths.
fn host_display(spec: &SshHostSpec) -> String {
    match &spec.username {
        Some(user) if !user.is_empty() => format!("{user}@{}", spec.host),
        _ => spec.host.clone(),
    }
}

/// Stable id for a remote project, derived from user@host:port so re-adding the
/// same host maps back to the same project.
fn project_id_for_host(spec: &SshHostSpec) -> ProjectId {
    let key = format!(
        "ssh:{}@{}:{}",
        spec.username.as_deref().unwrap_or(""),
        spec.host,
        spec.port.unwrap_or(22)
    );
    format!("ssh-{:x}", fnv1a(&key))
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

fn workspace_name_for_session(project_id: &str, session_id: &str) -> String {
    format!("thinkterm:{project_id}:{session_id}")
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
    fn session_store_round_trip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sessions.json");
        let mut store = SessionStore::default();
        let project = Project {
            id: "project-1".to_string(),
            name: "thinkterm".to_string(),
            path: PathBuf::from("/tmp/thinkterm"),
            sessions: vec![Session::new(
                "project-1".to_string(),
                "main".to_string(),
                None,
            )],
            active_session_id: None,
            sessions_collapsed: false,
            remote: None,
        };
        store.active_project_id = Some(project.id.clone());
        store.projects.push(project);
        save_session_store_to_path(&path, &store).unwrap();
        let loaded = load_session_store_from_path(&path).unwrap();
        assert_eq!(loaded.projects[0].path, PathBuf::from("/tmp/thinkterm"));
        assert_eq!(loaded.projects[0].sessions[0].name, "main");
    }

    #[test]
    fn running_work_state_is_runtime_only() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sessions.json");
        let mut store = SessionStore::default();
        let mut session = Session::new("project-1".to_string(), "main".to_string(), None);
        session.work_is_running = true;
        store.projects.push(Project {
            id: "project-1".to_string(),
            name: "thinkterm".to_string(),
            path: PathBuf::from("/tmp/thinkterm"),
            sessions: vec![session],
            active_session_id: None,
            sessions_collapsed: false,
            remote: None,
        });

        save_session_store_to_path(&path, &store).unwrap();
        let json = std::fs::read_to_string(&path).unwrap();
        assert!(!json.contains("work_is_running"));

        let legacy_json = json.replace(
            "\"work_finished_unseen\": false",
            "\"work_is_running\": true,\n          \"work_finished_unseen\": false",
        );
        assert_ne!(legacy_json, json);
        std::fs::write(&path, legacy_json).unwrap();

        let loaded = load_session_store_from_path(&path).unwrap();
        assert!(!loaded.projects[0].sessions[0].work_is_running);
        assert!(!loaded.projects[0].sessions[0].work_finished_unseen);
    }

    #[test]
    fn inactive_session_activation_materializes_once() {
        let mut store = SessionStore::default();
        let project = Project {
            id: "project-1".to_string(),
            name: "thinkterm".to_string(),
            path: PathBuf::from("/tmp/thinkterm"),
            sessions: vec![Session::new(
                "project-1".to_string(),
                "main".to_string(),
                None,
            )],
            active_session_id: None,
            sessions_collapsed: false,
            remote: None,
        };
        let session_id = project.sessions[0].id.clone();
        store.projects.push(project);

        let plan = store.activate_session_record(&session_id, &[]).unwrap();
        assert!(plan.needs_materialize);
        let plan = store
            .activate_session_record(&session_id, &[plan.workspace_name.clone()])
            .unwrap();
        assert!(!plan.needs_materialize);
    }

    #[test]
    fn created_project_gets_own_workspace_and_cwd() {
        let dir = tempdir().unwrap();
        let mut store = SessionStore::default();

        let session_id = store.create_project_from_path(dir.path().to_path_buf());
        let project_id = store.active_project_id.clone().unwrap();
        let workspace_name = workspace_name_for_session(&project_id, &session_id);

        assert_eq!(
            store.project_id_for_workspace(&workspace_name),
            Some(project_id.clone())
        );

        let plan = store.activate_session_record(&session_id, &[]).unwrap();
        assert!(plan.needs_materialize);
        assert_eq!(plan.project_path, dir.path());
        assert_eq!(plan.workspace_name, workspace_name);
    }

    #[test]
    fn duplicate_project_path_reuses_existing_session() {
        let dir = tempdir().unwrap();
        let mut store = SessionStore::default();

        let first_session_id = store.create_project_from_path(dir.path().to_path_buf());
        let second_session_id = store.create_project_from_path(dir.path().to_path_buf());

        assert_eq!(second_session_id, first_session_id);
        assert_eq!(store.projects.len(), 1);
        assert_eq!(store.projects[0].sessions.len(), 1);
    }

    #[test]
    fn duplicate_project_path_reuses_stored_project_id() {
        let dir = tempdir().unwrap();
        let mut store = SessionStore::default();

        let project = Project {
            id: "stored-project-id".to_string(),
            name: "existing".to_string(),
            path: dir.path().to_path_buf(),
            sessions: vec![Session::new(
                "stored-project-id".to_string(),
                "main".to_string(),
                None,
            )],
            active_session_id: None,
            sessions_collapsed: false,
            remote: None,
        };
        let session_id = project.sessions[0].id.clone();
        store.projects.push(project);

        let reused_session_id = store.create_project_from_path(dir.path().to_path_buf());

        assert_eq!(reused_session_id, session_id);
        assert_eq!(
            store.active_project_id.as_deref(),
            Some("stored-project-id")
        );
        assert_eq!(store.projects.len(), 1);
    }

    #[test]
    fn snapshot_layout_attaches_to_materialized_session() {
        let mut store = SessionStore::default();
        let mut session = Session::new(
            "project-1".to_string(),
            "main".to_string(),
            Some("ws".to_string()),
        );
        let session_id = session.id.clone();
        let project = Project {
            id: "project-1".to_string(),
            name: "thinkterm".to_string(),
            path: PathBuf::from("/tmp/thinkterm"),
            sessions: vec![session.clone()],
            active_session_id: Some(session_id),
            sessions_collapsed: false,
            remote: None,
        };
        store.projects.push(project);
        store.snapshot_workspace_layout(
            "ws",
            SessionLayoutSnapshot {
                active_tab: 0,
                tabs: vec![serde_json::json!({"kind": "test"})],
                terminal_specs: vec![],
            },
        );
        session = store.projects[0].sessions[0].clone();
        assert_eq!(session.layout.unwrap().tabs.len(), 1);
    }

    #[test]
    fn view_marks_only_global_active_project_session_active() {
        let mut store = SessionStore::default();
        let first = Session::new("project-1".to_string(), "main".to_string(), None);
        let second = Session::new("project-2".to_string(), "current".to_string(), None);
        let first_id = first.id.clone();
        let second_id = second.id.clone();
        store.projects.push(Project {
            id: "project-1".to_string(),
            name: "thinkterm".to_string(),
            path: PathBuf::from("/tmp/thinkterm"),
            sessions: vec![first],
            active_session_id: Some(first_id),
            sessions_collapsed: false,
            remote: None,
        });
        store.projects.push(Project {
            id: "project-2".to_string(),
            name: "agent_dock".to_string(),
            path: PathBuf::from("/tmp/agent_dock"),
            sessions: vec![second],
            active_session_id: Some(second_id),
            sessions_collapsed: false,
            remote: None,
        });
        store.active_project_id = Some("project-2".to_string());

        let view = store.view_for_project("project-2", &[]);
        assert!(!view.projects[0].is_active);
        assert!(!view.projects[0].sessions[0].is_active);
        assert!(view.projects[1].is_active);
        assert!(view.projects[1].sessions[0].is_active);
    }

    #[test]
    fn project_menu_metadata_actions_update_store() {
        let mut store = SessionStore::default();
        let first = Session::new("project-1".to_string(), "main".to_string(), None);
        let second = Session::new("project-2".to_string(), "current".to_string(), None);
        let first_id = first.id.clone();
        let second_id = second.id.clone();
        store.projects.push(Project {
            id: "project-1".to_string(),
            name: "thinkterm".to_string(),
            path: PathBuf::from("/tmp/thinkterm"),
            sessions: vec![first],
            active_session_id: Some(first_id),
            sessions_collapsed: false,
            remote: None,
        });
        store.projects.push(Project {
            id: "project-2".to_string(),
            name: "agent_dock".to_string(),
            path: PathBuf::from("/tmp/agent_dock"),
            sessions: vec![second],
            active_session_id: Some(second_id.clone()),
            sessions_collapsed: false,
            remote: None,
        });
        store.active_project_id = Some("project-2".to_string());

        assert!(store.rename_project("project-2", "Agents".to_string()));
        assert_eq!(store.projects[1].name, "Agents");

        let removed = store.remove_project("project-2").unwrap();
        assert!(removed.was_active);
        assert_eq!(
            removed.next_session_id,
            Some(store.projects[0].sessions[0].id.clone())
        );
        assert_eq!(store.projects.len(), 1);
        assert_eq!(store.active_project_id.as_deref(), Some("project-1"));
        assert!(store.remove_project("project-1").is_none());
    }

    #[test]
    fn session_menu_metadata_actions_update_store() {
        let mut store = SessionStore::default();
        let mut first = Session::new("project-1".to_string(), "main".to_string(), None);
        let second = Session::new("project-1".to_string(), "Session 2".to_string(), None);
        let third = Session::new("project-1".to_string(), "Session 3".to_string(), None);
        let first_id = first.id.clone();
        let second_id = second.id.clone();
        let third_id = third.id.clone();
        first.is_unread = true;
        store.projects.push(Project {
            id: "project-1".to_string(),
            name: "thinkterm".to_string(),
            path: PathBuf::from("/tmp/thinkterm"),
            sessions: vec![first, second, third],
            active_session_id: Some(first_id.clone()),
            sessions_collapsed: false,
            remote: None,
        });

        assert!(store.rename_session(&second_id, "Review".to_string()));
        assert!(store.toggle_session_pinned(&second_id));
        assert!(store.mark_session_unread(&second_id));
        assert!(store.archive_session(&second_id));
        assert!(!store.archive_session(&first_id));
        assert_eq!(
            store
                .delete_session(&third_id)
                .unwrap()
                .materialized_workspace_name,
            None
        );

        let project = &store.projects[0];
        let archived = project
            .sessions
            .iter()
            .find(|session| session.id == second_id)
            .unwrap();
        assert_eq!(archived.name, "Review");
        assert!(archived.is_pinned);
        assert!(archived.is_unread);
        assert!(archived.archived);

        let view = store.view_for_project("project-1", &[]);
        assert_eq!(view.projects[0].sessions.len(), 1);
        assert_eq!(view.projects[0].sessions[0].id, first_id);
    }
}
