//! The sidebar's model: the server's Spaces, Projects and Threads
//! (`ThinkTermSessionState`, with `ThinkTermTree` for what it leaves
//! out), the agents in their panes, and the windows that belong to no
//! thread. Pure, so it is tested natively; `sidebar.rs` draws it.
//!
//! What a row shows follows the desktop (`termwindow/ui/sidebar.rs`):
//! a thread's status is the worst of what its panes are doing --
//! waiting on someone, then working, then finished unseen -- and its dot
//! says whether it is the one on show, unread, pinned, or open.

use codec::{ListPanesResponse, ThinkTermSessionState, ThinkTermSessionThread, ThinkTermTree, TreeOp};
use std::collections::{HashMap, HashSet};
use thinkterm_proto::{AgentState, AgentStatus, PaneId, TabId, WindowId};

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum Status {
    Idle,
    Running,
    NeedsAttention,
    Done,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum Dot {
    Active,
    Unread,
    Pinned,
    Open,
    Quiet,
}

/// Serialised for the page as `{"kind": …}` rows, the shape the sidebar
/// probe has always read.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Row {
    Space { id: String, name: String },
    NewThread,
    Pinned,
    Workspaces,
    Project { id: String, name: String, path: String, collapsed: bool, archived: bool },
    Thread(ThreadRow),
    Archived { count: usize, open: bool, label: String },
    Others,
    Window {
        #[serde(rename = "id")]
        window_id: WindowId,
        title: String,
        selected: bool,
    },
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct ThreadRow {
    pub id: String,
    #[serde(rename = "project")]
    pub project_id: String,
    pub name: String,
    pub status: Status,
    pub dot: Dot,
    pub pinned: bool,
    pub unread: bool,
    /// The thread has terminals on the server.
    pub live: bool,
    pub selected: bool,
    /// Its delete button was pressed once and waits for the press that means it.
    pub deleting: bool,
}

/// The tree whole: `TreeModel::tree`.
#[derive(Debug, Clone, PartialEq, Default, serde::Serialize)]
pub struct TreeView {
    pub spaces: Vec<TreeSpace>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct TreeSpace {
    pub id: String,
    pub name: String,
    pub current: bool,
    pub projects: Vec<TreeProject>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct TreeProject {
    pub id: String,
    pub name: String,
    pub path: String,
    pub threads: Vec<ThreadRow>,
}

/// A Space as the Space menu lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpaceEntry {
    pub id: String,
    pub name: String,
    pub current: bool,
    pub default: bool,
}

#[derive(Default)]
pub struct TreeModel {
    pub session: Option<ThinkTermSessionState>,
    pub tree: Option<ThinkTermTree>,
    pub agents: HashMap<PaneId, AgentState>,
    /// The agent's id and the pane's title as the server reported them.
    agent_details: HashMap<PaneId, (String, String)>,
    pub collapsed: HashSet<String>,
    pub archived_open: bool,
    /// The Space the sidebar shows; the first one until chosen.
    pub space: Option<String>,
    /// Every window on the server, with its workspace and a title.
    windows: Vec<(WindowId, String, String)>,
}

impl TreeModel {
    /// A newer snapshot from the same server, or any from another.
    pub fn apply_session(&mut self, state: ThinkTermSessionState) -> bool {
        if self
            .session
            .as_ref()
            .is_some_and(|prior| prior.server_id == state.server_id && state.generation <= prior.generation)
        {
            return false;
        }
        self.session = Some(state);
        true
    }

    pub fn apply_tree(&mut self, tree: ThinkTermTree) {
        self.tree = Some(tree);
    }

    /// Every pane an agent runs in: pane, agent id, state, reported title.
    pub fn agent_entries(&self) -> impl Iterator<Item = (PaneId, &str, AgentState, &str)> {
        self.agents.iter().map(|(pane, state)| {
            let (id, title) = self
                .agent_details
                .get(pane)
                .map(|(id, title)| (id.as_str(), title.as_str()))
                .unwrap_or(("", ""));
            (*pane, id, *state, title)
        })
    }

    /// Where a pane is: "Project · Thread" and its window, from the
    /// thread whose tabs hold it; a window's workspace otherwise.
    pub fn place_of_pane(&self, pane: PaneId) -> (String, Option<WindowId>) {
        if let Some(session) = &self.session {
            for project in &session.projects {
                for thread in &project.threads {
                    if let Some(tab) = thread.tabs.iter().find(|t| t.pane_ids.contains(&pane)) {
                        return (format!("{} · {}", project.name, thread.name), Some(tab.window_id));
                    }
                }
            }
        }
        (String::new(), None)
    }

    /// The title the server reported with an agent's status, when it did.
    pub fn record_agent_details(&mut self, pane_id: PaneId, agent_id: &str, title: &str) {
        self.agent_details.insert(pane_id, (agent_id.to_string(), title.to_string()));
    }

    pub fn apply_agent(&mut self, pane_id: PaneId, status: Option<&AgentStatus>) {
        match status {
            Some(s) => {
                self.agents.insert(pane_id, s.state);
                let title = self.agent_details.get(&pane_id).map(|(_, t)| t.clone()).unwrap_or_default();
                self.agent_details.insert(pane_id, (s.agent_id.clone(), title));
            }
            None => {
                self.agents.remove(&pane_id);
                self.agent_details.remove(&pane_id);
            }
        }
    }

    /// The windows the server has, for the ones no thread claims.
    pub fn apply_panes(&mut self, list: &ListPanesResponse) {
        let mut seen = HashSet::new();
        self.windows.clear();
        for node in &list.tabs {
            let leaves = crate::layout::leaves(node);
            let Some(first) = leaves.first() else {
                continue;
            };
            if !seen.insert(first.window_id) {
                continue;
            }
            let title = list
                .window_titles
                .get(&first.window_id)
                .filter(|t| !t.is_empty())
                .cloned()
                .unwrap_or_else(|| {
                    crate::navbar::display_title(&first.title).0
                });
            self.windows.push((first.window_id, first.workspace.clone(), title));
        }
    }

    pub fn thread(&self, id: &str) -> Option<&ThinkTermSessionThread> {
        self.session
            .as_ref()?
            .projects
            .iter()
            .flat_map(|p| p.threads.iter())
            .find(|t| t.id == id)
    }

    /// Every pane the project's threads have on the server.
    pub fn live_panes_of_project(&self, project_id: &str) -> Vec<PaneId> {
        self.session
            .as_ref()
            .and_then(|s| s.projects.iter().find(|p| p.id == project_id))
            .map(|p| p.threads.iter().flat_map(|t| t.tabs.iter().flat_map(|tab| tab.pane_ids.iter().copied())).collect())
            .unwrap_or_default()
    }

    /// How many threads a project has in the tree (archived ones included).
    pub fn thread_count_of_project(&self, project_id: &str) -> usize {
        self.tree
            .as_ref()
            .and_then(|t| t.projects.iter().find(|p| p.id == project_id))
            .map(|p| p.threads.len())
            .unwrap_or(0)
    }

    pub fn archived_count(&self) -> usize {
        self.tree
            .as_ref()
            .map(|t| t.projects.iter().filter(|p| p.archived_at.is_some()).count())
            .unwrap_or(0)
    }

    pub fn project_of(&self, thread_id: &str) -> Option<&codec::ThinkTermSessionProject> {
        self.session
            .as_ref()?
            .projects
            .iter()
            .find(|p| p.threads.iter().any(|t| t.id == thread_id))
    }

    /// The workspace a thread lives in, or would.
    pub fn workspace_of(thread: &ThinkTermSessionThread) -> Option<&str> {
        thread
            .materialized_workspace_name
            .as_deref()
            .or(thread.planned_workspace_name.as_deref())
    }

    fn thread_row(
        &self,
        t: &ThinkTermSessionThread,
        project_id: &str,
        selected: Option<&str>,
        deleting: Option<&str>,
    ) -> ThreadRow {
        let live = !t.tabs.is_empty();
        let is_selected = selected == Some(t.id.as_str());
        let dot = if is_selected {
            Dot::Active
        } else if t.is_unread {
            Dot::Unread
        } else if t.is_pinned {
            Dot::Pinned
        } else if live {
            Dot::Open
        } else {
            Dot::Quiet
        };
        ThreadRow {
            id: t.id.clone(),
            project_id: project_id.to_string(),
            name: t.name.clone(),
            status: self.status_of(t),
            dot,
            pinned: t.is_pinned,
            unread: t.is_unread,
            live,
            selected: is_selected,
            deleting: deleting == Some(t.id.as_str()),
        }
    }

    /// The whole tree, every Space with its projects and their threads,
    /// as the TUI's sidebar shows it; `rows` is one Space of it flattened
    /// the desktop's way.
    pub fn tree(&self, current_tab: TabId, current_workspace: &str, deleting: Option<&str>) -> TreeView {
        let Some(session) = &self.session else {
            return TreeView::default();
        };
        let selected = self.selected_thread(current_tab, current_workspace);
        let current = self.current_space().map(|s| s.id.clone()).unwrap_or_default();
        let spaces = session
            .spaces
            .iter()
            .map(|space| TreeSpace {
                id: space.id.clone(),
                name: space.name.clone(),
                current: space.id == current,
                projects: session
                    .projects
                    .iter()
                    .filter(|p| p.space_id == space.id)
                    .map(|p| TreeProject {
                        id: p.id.clone(),
                        name: p.name.clone(),
                        path: p.path.clone(),
                        threads: p.threads.iter().map(|t| self.thread_row(t, &p.id, selected, deleting)).collect(),
                    })
                    .collect(),
            })
            .collect();
        TreeView { spaces }
    }

    /// The worst of what the thread's panes are doing, the desktop's way:
    /// an agent waiting on someone, or a program that failed, outranks
    /// one still working, which outranks having finished unseen.
    pub fn status_of(&self, thread: &ThinkTermSessionThread) -> Status {
        use codec::ThinkTermSessionWorkStatus as W;
        let agents = thread
            .tabs
            .iter()
            .flat_map(|t| t.pane_ids.iter())
            .filter_map(|p| self.agents.get(p));
        let mut working = false;
        for state in agents {
            match state {
                AgentState::Blocked => return Status::NeedsAttention,
                AgentState::Working => working = true,
                _ => {}
            }
        }
        match thread.work_status {
            W::NeedsAttention => Status::NeedsAttention,
            W::Running => Status::Running,
            _ if working => Status::Running,
            W::FinishedUnseen => Status::Done,
            W::Idle if thread.is_unread => Status::Done,
            W::Idle => Status::Idle,
        }
    }

    /// The thread on show: the one holding the tab on show, else the one
    /// whose workspace the page is in.
    /// The Space on show: the chosen one while the server still has it,
    /// else the first.
    pub fn current_space(&self) -> Option<&codec::ThinkTermSessionSpace> {
        let spaces = &self.session.as_ref()?.spaces;
        self.space
            .as_deref()
            .and_then(|id| spaces.iter().find(|s| s.id == id))
            .or_else(|| spaces.first())
    }

    /// Choose a Space; false when the server has no such Space.
    pub fn set_space(&mut self, id: &str) -> bool {
        let known = self.session.as_ref().is_some_and(|s| s.spaces.iter().any(|s| s.id == id));
        if known {
            self.space = Some(id.to_string());
        }
        known
    }

    /// Every Space, for the Space menu.
    pub fn spaces(&self) -> Vec<SpaceEntry> {
        let current = self.current_space().map(|s| s.id.clone());
        self.session
            .as_ref()
            .map(|s| {
                s.spaces
                    .iter()
                    .map(|space| SpaceEntry {
                        id: space.id.clone(),
                        name: space.name.clone(),
                        current: current.as_deref() == Some(space.id.as_str()),
                        default: space.is_default,
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The project a new thread goes to when none is named: the selected
    /// thread's when it is in the Space on show, else the Space's first.
    pub fn default_project(&self, current_tab: TabId, current_workspace: &str) -> Option<String> {
        let space = self.current_space()?.id.clone();
        let session = self.session.as_ref()?;
        let selected = self
            .selected_thread(current_tab, current_workspace)
            .and_then(|t| self.project_of(t))
            .filter(|p| p.space_id == space)
            .map(|p| p.id.clone());
        selected.or_else(|| session.projects.iter().find(|p| p.space_id == space).map(|p| p.id.clone()))
    }

    pub fn selected_thread(&self, current_tab: TabId, current_workspace: &str) -> Option<&str> {
        let session = self.session.as_ref()?;
        let threads = || session.projects.iter().flat_map(|p| p.threads.iter());
        threads()
            .find(|t| t.tabs.iter().any(|tab| tab.tab_id == current_tab))
            .or_else(|| threads().find(|t| Self::workspace_of(t) == Some(current_workspace)))
            .map(|t| t.id.as_str())
    }

    pub fn rows(&self, current_tab: TabId, current_workspace: &str, current_window: WindowId) -> Vec<Row> {
        self.rows_with(current_tab, current_workspace, current_window, None)
    }

    /// `deleting` is the thread whose delete button waits for its second press.
    pub fn rows_with(
        &self,
        current_tab: TabId,
        current_workspace: &str,
        current_window: WindowId,
        deleting: Option<&str>,
    ) -> Vec<Row> {
        let mut rows = Vec::new();
        let Some(session) = &self.session else {
            return rows;
        };
        let selected = self.selected_thread(current_tab, current_workspace);
        let space = self.current_space();
        let space_id = space.map(|s| s.id.clone()).unwrap_or_default();
        rows.push(Row::Space {
            id: space_id.clone(),
            name: space.map(|s| s.name.clone()).unwrap_or_else(|| "Default".to_string()),
        });
        rows.push(Row::NewThread);
        let in_space = |p: &&codec::ThinkTermSessionProject| p.space_id == space_id;
        let thread_row = |t: &ThinkTermSessionThread, project_id: &str| Row::Thread(self.thread_row(t, project_id, selected, deleting));
        let pinned: Vec<Row> = session
            .projects
            .iter()
            .filter(in_space)
            .flat_map(|p| p.threads.iter().filter(|t| t.is_pinned).map(move |t| thread_row(t, &p.id)))
            .collect();
        if !pinned.is_empty() {
            rows.push(Row::Pinned);
            rows.extend(pinned);
        }
        rows.push(Row::Workspaces);
        for project in session.projects.iter().filter(in_space) {
            let collapsed = self.collapsed.contains(&project.id);
            rows.push(Row::Project {
                id: project.id.clone(),
                name: project.name.clone(),
                path: project.path.clone(),
                collapsed,
                archived: false,
            });
            if !collapsed {
                rows.extend(project.threads.iter().filter(|t| !t.is_pinned).map(|t| thread_row(t, &project.id)));
            }
        }
        // Archived projects are only in the tree.
        let archived: Vec<&codec::TtProject> = self
            .tree
            .as_ref()
            .map(|t| t.projects.iter().filter(|p| p.archived_at.is_some() && p.space_id == space_id).collect())
            .unwrap_or_default();
        if !archived.is_empty() {
            let mut args = thinkterm_i18n::FluentArgs::new();
            args.set("count", archived.len() as i64);
            rows.push(Row::Archived {
                count: archived.len(),
                open: self.archived_open,
                label: thinkterm_i18n::tr_args("menu-show-archived-count", &args),
            });
            if self.archived_open {
                for p in archived {
                    rows.push(Row::Project {
                        id: p.id.clone(),
                        name: p.name.clone(),
                        path: p.path.clone(),
                        collapsed: true,
                        archived: true,
                    });
                }
            }
        }
        // Windows no thread claims, so nothing is out of reach.
        let claimed: HashSet<&str> = session
            .projects
            .iter()
            .flat_map(|p| p.threads.iter())
            .filter_map(Self::workspace_of)
            .collect();
        let others: Vec<Row> = self
            .windows
            .iter()
            .filter(|(_, ws, _)| !claimed.contains(ws.as_str()))
            .map(|(id, _, title)| Row::Window { window_id: *id, title: title.clone(), selected: *id == current_window })
            .collect();
        if !others.is_empty() {
            rows.push(Row::Others);
            rows.extend(others);
        }
        rows
    }
}

/// A write to the tree and what the tree must show afterwards. The
/// server answers a mutation with its tree and nothing else: an op it
/// would not apply (an unknown id, a Space's last project archived) just
/// leaves the tree as it was, so the intent is checked against the
/// answer to turn a silent no-op into a remark.
#[derive(Debug, Clone, PartialEq)]
pub struct Intent {
    pub ops: Vec<TreeOp>,
    pub expect: Expect,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expect {
    ThreadExists(String),
    ThreadNamed(String, String),
    ThreadPinned(String, bool),
    ThreadGone(String),
    ProjectExists(String),
    ProjectNamed(String, String),
    ProjectArchived(String, bool),
    ThreadUnread(String, bool),
    ProjectGone(String),
    SpaceExists(String),
    SpaceNamed(String, String),
    SpaceGone(String),
    /// The unpinned threads of a project, in order.
    ThreadOrder(String, Vec<String>),
    /// The projects of a Space, in order.
    ProjectOrder(String, Vec<String>),
}

impl Expect {
    pub fn check(&self, tree: &ThinkTermTree) -> Result<(), String> {
        let thread = |id: &str| tree.projects.iter().flat_map(|p| p.threads.iter()).find(|t| t.id == id);
        let project = |id: &str| tree.projects.iter().find(|p| p.id == id);
        match self {
            Expect::ThreadExists(id) => thread(id).map(|_| ()).ok_or_else(|| "the server did not create the thread".into()),
            Expect::ThreadNamed(id, name) => match thread(id) {
                Some(t) if &t.name == name => Ok(()),
                // The server keeps names unique within a project: a
                // different name back is its answer, not a refusal.
                Some(t) if t.name.starts_with(name.as_str()) => Ok(()),
                Some(_) => Err("the server kept the old name".into()),
                None => Err("the thread is gone".into()),
            },
            Expect::ThreadPinned(id, pinned) => match thread(id) {
                Some(t) if t.is_pinned == *pinned => Ok(()),
                Some(_) => Err("the server did not change the pin".into()),
                None => Err("the thread is gone".into()),
            },
            Expect::ThreadGone(id) => match thread(id) {
                None => Ok(()),
                Some(_) => Err("the server did not delete the thread".into()),
            },
            Expect::ProjectExists(id) => project(id).map(|_| ()).ok_or_else(|| "the server did not create the project".into()),
            Expect::ProjectNamed(id, name) => match project(id) {
                Some(p) if &p.name == name => Ok(()),
                Some(_) => Err("the server kept the old name".into()),
                None => Err("the project is gone".into()),
            },
            Expect::SpaceExists(id) => tree.spaces.iter().any(|s| &s.id == id).then_some(()).ok_or_else(|| "the server did not create the Space".into()),
            Expect::SpaceNamed(id, name) => match tree.spaces.iter().find(|s| &s.id == id) {
                Some(s) if &s.name == name => Ok(()),
                Some(_) => Err("the server kept the old name".into()),
                None => Err("the Space is gone".into()),
            },
            Expect::SpaceGone(id) => match tree.spaces.iter().find(|s| &s.id == id) {
                None => Ok(()),
                Some(_) => Err("the server did not delete the Space".into()),
            },
            Expect::ThreadUnread(id, unread) => match thread(id) {
                Some(t) if t.is_unread == *unread => Ok(()),
                Some(_) => Err("the server did not change the unread mark".into()),
                None => Err("the thread is gone".into()),
            },
            Expect::ThreadOrder(project_id, order) => match project(project_id) {
                Some(p) => {
                    let now: Vec<&str> = p.threads.iter().filter(|t| !t.is_pinned).map(|t| t.id.as_str()).collect();
                    (now == order.iter().map(String::as_str).collect::<Vec<_>>())
                        .then_some(())
                        .ok_or_else(|| "the server did not move the thread".into())
                }
                None => Err("the project is gone".into()),
            },
            Expect::ProjectOrder(space_id, order) => {
                // Archived projects are not in the session view the
                // wanted order was built from; they hold no place in it.
                let now: Vec<&str> = tree
                    .projects_in_space(space_id)
                    .filter(|p| p.archived_at.is_none())
                    .map(|p| p.id.as_str())
                    .collect();
                (now == order.iter().map(String::as_str).collect::<Vec<_>>())
                    .then_some(())
                    .ok_or_else(|| "the server did not move the project".into())
            }
            Expect::ProjectGone(id) => match project(id) {
                None => Ok(()),
                Some(_) => Err("the server did not remove the project".into()),
            },
            Expect::ProjectArchived(id, archived) => match project(id) {
                Some(p) if p.archived_at.is_some() == *archived => Ok(()),
                Some(_) if *archived => Err("the last project of a Space cannot be archived".into()),
                Some(_) => Err("the server did not restore the project".into()),
                None => Err("the project is gone".into()),
            },
        }
    }
}

/// An id the server adopts: opaque, unique enough, minted here so the
/// thread can be opened without waiting for the answer (the desktop
/// does the same).
pub fn new_id(kind: &str, random: impl Fn() -> u32) -> String {
    format!("{kind}-{:08x}{:08x}{:08x}{:08x}", random(), random(), random(), random())
}

pub fn create_thread(project_id: &str, thread_id: String, now: i64) -> Intent {
    Intent {
        ops: vec![TreeOp::CreateThread {
            thread_id: thread_id.clone(),
            project_id: project_id.to_string(),
            // Empty: the server names it "Thread N".
            name: String::new(),
            workspace: None,
            created_at: now,
        }],
        expect: Expect::ThreadExists(thread_id),
    }
}

/// The directory's own name, as the desktop names a project.
pub fn project_name_for_path(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    let name = trimmed.rsplit('/').next().unwrap_or(trimmed);
    if name.is_empty() || name == "~" {
        "Home".to_string()
    } else {
        name.to_string()
    }
}

/// A project at `path` with its `main` thread, in one round trip; the
/// Space is made too when the tree has none yet.
pub fn create_project(space: Option<&str>, project_id: String, thread_id: String, path: &str, now: i64) -> Intent {
    let mut ops = Vec::new();
    let space_id = match space {
        Some(id) => id.to_string(),
        None => {
            let id = format!("space-{}", &project_id[8.min(project_id.len())..]);
            ops.push(TreeOp::CreateSpace { space_id: id.clone(), name: "Default".into() });
            id
        }
    };
    ops.push(TreeOp::CreateProject {
        project_id: project_id.clone(),
        space_id,
        name: project_name_for_path(path),
        path: path.to_string(),
    });
    ops.push(TreeOp::CreateThread {
        thread_id: thread_id.clone(),
        project_id: project_id.clone(),
        name: "main".into(),
        workspace: None,
        created_at: now,
    });
    Intent { ops, expect: Expect::ThreadExists(thread_id) }
}

pub fn rename_thread(id: &str, name: &str, now: i64) -> Intent {
    Intent {
        ops: vec![TreeOp::RenameThread { thread_id: id.to_string(), name: name.to_string(), last_active_at: now }],
        expect: Expect::ThreadNamed(id.to_string(), name.to_string()),
    }
}

/// Put an unpinned thread before `before` (None: last) among its
/// project's unpinned threads; `order` is that list as it stands.
pub fn move_thread_before(project_id: &str, id: &str, before: Option<&str>, order: &[String]) -> Intent {
    let mut want: Vec<String> = order.iter().filter(|t| t.as_str() != id).cloned().collect();
    let at = before.and_then(|b| want.iter().position(|t| t == b)).unwrap_or(want.len());
    want.insert(at, id.to_string());
    Intent {
        ops: vec![TreeOp::MoveThreadBefore {
            project_id: project_id.to_string(),
            thread_id: id.to_string(),
            before: before.map(str::to_string),
        }],
        expect: Expect::ThreadOrder(project_id.to_string(), want),
    }
}

/// Put a project before `before` (None: last) among its Space's projects.
pub fn move_project_before(space_id: &str, id: &str, before: Option<&str>, order: &[String]) -> Intent {
    let mut want: Vec<String> = order.iter().filter(|p| p.as_str() != id).cloned().collect();
    let at = before.and_then(|b| want.iter().position(|p| p == b)).unwrap_or(want.len());
    want.insert(at, id.to_string());
    Intent {
        ops: vec![TreeOp::MoveProjectBefore {
            space_id: space_id.to_string(),
            project_id: id.to_string(),
            before: before.map(str::to_string),
        }],
        expect: Expect::ProjectOrder(space_id.to_string(), want),
    }
}

pub fn rename_project(id: &str, name: &str) -> Intent {
    Intent {
        ops: vec![TreeOp::RenameProject { project_id: id.to_string(), name: name.to_string() }],
        expect: Expect::ProjectNamed(id.to_string(), name.to_string()),
    }
}

pub fn set_pinned(id: &str, pinned: bool, now: i64) -> Intent {
    Intent {
        ops: vec![TreeOp::SetThreadPinned { thread_id: id.to_string(), pinned, last_active_at: now }],
        expect: Expect::ThreadPinned(id.to_string(), pinned),
    }
}

pub fn delete_thread(id: &str) -> Intent {
    Intent {
        ops: vec![TreeOp::DeleteThread { thread_id: id.to_string() }],
        expect: Expect::ThreadGone(id.to_string()),
    }
}

pub fn create_space(id: String, name: &str) -> Intent {
    Intent {
        ops: vec![TreeOp::CreateSpace { space_id: id.clone(), name: name.to_string() }],
        expect: Expect::SpaceExists(id),
    }
}

pub fn rename_space(id: &str, name: &str) -> Intent {
    Intent {
        ops: vec![TreeOp::RenameSpace { space_id: id.to_string(), name: name.to_string() }],
        expect: Expect::SpaceNamed(id.to_string(), name.to_string()),
    }
}

/// Delete a Space; the server drops its projects and threads with it.
pub fn delete_space(id: &str) -> Intent {
    Intent {
        ops: vec![TreeOp::DeleteSpace { space_id: id.to_string() }],
        expect: Expect::SpaceGone(id.to_string()),
    }
}

pub fn set_unread(id: &str, unread: bool) -> Intent {
    Intent {
        ops: vec![TreeOp::SetThreadUnread { thread_id: id.to_string(), unread }],
        expect: Expect::ThreadUnread(id.to_string(), unread),
    }
}

/// Remove a project and its threads for good.
pub fn remove_project(id: &str) -> Intent {
    Intent {
        ops: vec![TreeOp::RemoveProject { project_id: id.to_string() }],
        expect: Expect::ProjectGone(id.to_string()),
    }
}

pub fn archive_project(id: &str, archived: bool, now: i64) -> Intent {
    Intent {
        ops: vec![TreeOp::SetProjectArchived { project_id: id.to_string(), archived_at: archived.then_some(now) }],
        expect: Expect::ProjectArchived(id.to_string(), archived),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codec::{ThinkTermSessionProject, ThinkTermSessionSpace, ThinkTermSessionTab, ThinkTermSessionWorkStatus};

    #[test]
    fn intents_are_checked_against_the_tree_that_comes_back() {
        let mut tree = ThinkTermTree::default();
        let intent = create_project(None, "project-1".into(), "thread-1".into(), "~/work/site", 5);
        assert_eq!(intent.ops.len(), 3, "space, project, thread");
        for op in &intent.ops {
            assert!(codec::apply_op(&mut tree, op));
        }
        assert_eq!(tree.projects[0].name, "site");
        assert!(intent.expect.check(&tree).is_ok());
        assert!(Expect::ThreadExists("thread-9".into()).check(&tree).is_err());
        // A renamed thread comes back de-duplicated, which is fine.
        codec::apply_op(&mut tree, &rename_thread("thread-1", "work", 6).ops[0]);
        assert!(Expect::ThreadNamed("thread-1".into(), "work".into()).check(&tree).is_ok());
        // Archiving the Space's last project is refused: the tree comes back unchanged.
        let intent = archive_project("project-1", true, 7);
        assert!(!codec::apply_op(&mut tree, &intent.ops[0]));
        assert_eq!(intent.expect.check(&tree).unwrap_err(), "the last project of a Space cannot be archived");
        assert_eq!(project_name_for_path("~"), "Home");
        assert_eq!(project_name_for_path("/srv/app/"), "app");
        assert!(new_id("thread", || 0xabcd).starts_with("thread-0000abcd"));
    }

    #[test]
    fn reorder_intents_expect_the_order_they_asked_for() {
        let mut tree = ThinkTermTree::default();
        for op in create_project(None, "p1".into(), "t1".into(), "~/a", 1).ops {
            assert!(codec::apply_op(&mut tree, &op));
        }
        for id in ["t2", "t3"] {
            assert!(codec::apply_op(&mut tree, &TreeOp::CreateThread {
                thread_id: id.into(), project_id: "p1".into(), name: id.into(), workspace: None, created_at: 2,
            }));
        }
        let order = ["t1", "t2", "t3"].map(String::from);
        let intent = move_thread_before("p1", "t3", Some("t1"), &order);
        assert!(codec::apply_op(&mut tree, &intent.ops[0]));
        assert!(intent.expect.check(&tree).is_ok());
        assert_eq!(intent.expect, Expect::ThreadOrder("p1".into(), ["t3", "t1", "t2"].map(String::from).to_vec()));
        // A move the server did not apply is a refusal.
        assert!(move_thread_before("p1", "t2", None, &order).expect.check(&tree).is_err());
        let space = tree.spaces[0].id.clone();
        assert!(codec::apply_op(&mut tree, &TreeOp::CreateProject {
            project_id: "p2".into(), space_id: space.clone(), name: "b".into(), path: "~/b".into(),
        }));
        let intent = move_project_before(&space, "p2", Some("p1"), &["p1".to_string(), "p2".to_string()]);
        assert!(codec::apply_op(&mut tree, &intent.ops[0]));
        assert!(intent.expect.check(&tree).is_ok());
    }

    fn thread(id: &str, ws: Option<&str>, tabs: Vec<(TabId, Vec<PaneId>)>) -> ThinkTermSessionThread {
        ThinkTermSessionThread {
            id: id.into(),
            project_id: "p1".into(),
            name: id.to_uppercase(),
            planned_workspace_name: None,
            materialized_workspace_name: ws.map(String::from),
            is_pinned: false,
            is_unread: false,
            work_status: ThinkTermSessionWorkStatus::Idle,
            tabs: tabs
                .into_iter()
                .map(|(tab_id, pane_ids)| ThinkTermSessionTab { window_id: 1, tab_id, pane_ids, title: String::new(), is_active: true })
                .collect(),
        }
    }

    fn session(threads: Vec<ThinkTermSessionThread>) -> ThinkTermSessionState {
        ThinkTermSessionState {
            server_id: "s".into(),
            tree_revision: 1,
            generation: 1,
            spaces: vec![ThinkTermSessionSpace { id: "sp".into(), name: "Default".into(), is_default: false, domain: None }],
            projects: vec![ThinkTermSessionProject { id: "p1".into(), space_id: "sp".into(), name: "Home".into(), path: "~".into(), threads }],
        }
    }

    #[test]
    fn rows_follow_the_desktop_s_order_and_pick_the_thread_on_show() {
        let mut m = TreeModel::default();
        let mut pinned = thread("b", Some("ws-b"), vec![]);
        pinned.is_pinned = true;
        assert!(m.apply_session(session(vec![thread("a", Some("ws-a"), vec![(7, vec![70])]), pinned])));
        let rows = m.rows(7, "ws-a", 1);
        let kinds: Vec<&str> = rows
            .iter()
            .map(|r| match r {
                Row::Space { .. } => "space",
                Row::NewThread => "new",
                Row::Pinned => "pinned",
                Row::Workspaces => "workspaces",
                Row::Project { .. } => "project",
                Row::Thread(t) if t.selected => "thread*",
                Row::Thread(_) => "thread",
                _ => "other",
            })
            .collect();
        assert_eq!(kinds, ["space", "new", "pinned", "thread", "workspaces", "project", "thread*"]);
        let Row::Thread(a) = &rows[6] else { panic!() };
        assert_eq!((a.dot, a.live), (Dot::Active, true));
        let Row::Thread(b) = &rows[3] else { panic!() };
        assert_eq!((b.dot, b.live, b.pinned), (Dot::Pinned, false, true));
        // Selection by workspace when the tab is not listed.
        assert_eq!(m.selected_thread(99, "ws-b"), Some("b"));
    }

    #[test]
    fn status_is_the_worst_of_the_panes_and_the_server_s_word() {
        let mut m = TreeModel::default();
        let mut t = thread("a", Some("ws"), vec![(1, vec![10, 11])]);
        assert_eq!(m.status_of(&t), Status::Idle);
        m.agents.insert(10, AgentState::Working);
        assert_eq!(m.status_of(&t), Status::Running);
        m.agents.insert(11, AgentState::Blocked);
        assert_eq!(m.status_of(&t), Status::NeedsAttention);
        m.agents.clear();
        t.work_status = ThinkTermSessionWorkStatus::FinishedUnseen;
        assert_eq!(m.status_of(&t), Status::Done);
        t.work_status = ThinkTermSessionWorkStatus::Idle;
        t.is_unread = true;
        assert_eq!(m.status_of(&t), Status::Done);
    }

    #[test]
    fn a_stale_snapshot_is_ignored_and_orphan_windows_are_listed() {
        let mut m = TreeModel::default();
        let mut s = session(vec![thread("a", Some("thinkterm:p1:a"), vec![])]);
        s.generation = 5;
        assert!(m.apply_session(s.clone()));
        s.generation = 4;
        assert!(!m.apply_session(s.clone()), "older generation from the same server");
        s.server_id = "other".into();
        assert!(m.apply_session(s), "any generation from another server");
        let list = ListPanesResponse {
            tabs: vec![thinkterm_proto::layout::PaneNode::Leaf(thinkterm_proto::layout::PaneEntry {
                window_id: 3,
                tab_id: 30,
                pane_id: 300,
                title: "zsh".into(),
                size: Default::default(),
                working_dir: None,
                is_active_pane: true,
                is_zoomed_pane: false,
                alt_screen: false,
                workspace: "default".into(),
                cursor_pos: Default::default(),
                physical_top: 0,
                top_row: 0,
                left_col: 0,
                tty_name: None,
            })],
            tab_titles: vec![],
            window_titles: Default::default(),
        };
        m.apply_panes(&list);
        let rows = m.rows(30, "default", 3);
        assert!(matches!(rows.last(), Some(Row::Window { window_id: 3, selected: true, title }) if title == "Terminal"));
        assert!(rows.iter().any(|r| matches!(r, Row::Others)));
    }
}
