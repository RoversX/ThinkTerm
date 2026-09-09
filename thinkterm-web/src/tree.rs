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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Idle,
    Running,
    NeedsAttention,
    Done,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dot {
    Active,
    Unread,
    Pinned,
    Open,
    Quiet,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Row {
    Space { name: String },
    NewThread,
    Pinned,
    Workspaces,
    Project { id: String, name: String, path: String, collapsed: bool, archived: bool },
    Thread(ThreadRow),
    Archived { count: usize, open: bool },
    Others,
    Window { window_id: WindowId, title: String, selected: bool },
}

#[derive(Debug, Clone, PartialEq)]
pub struct ThreadRow {
    pub id: String,
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

#[derive(Default)]
pub struct TreeModel {
    pub session: Option<ThinkTermSessionState>,
    pub tree: Option<ThinkTermTree>,
    pub agents: HashMap<PaneId, AgentState>,
    pub collapsed: HashSet<String>,
    pub archived_open: bool,
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

    pub fn apply_agent(&mut self, pane_id: PaneId, status: Option<&AgentStatus>) {
        match status {
            Some(s) => {
                self.agents.insert(pane_id, s.state);
            }
            None => {
                self.agents.remove(&pane_id);
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
                    let (title, _) = crate::navbar::display_title(&first.title);
                    title.to_string()
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
        let space_name = session
            .spaces
            .first()
            .map(|s| s.name.clone())
            .unwrap_or_else(|| "Default".to_string());
        rows.push(Row::Space { name: space_name });
        rows.push(Row::NewThread);
        let thread_row = |t: &ThinkTermSessionThread, project_id: &str| {
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
            Row::Thread(ThreadRow {
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
            })
        };
        let pinned: Vec<Row> = session
            .projects
            .iter()
            .flat_map(|p| p.threads.iter().filter(|t| t.is_pinned).map(move |t| thread_row(t, &p.id)))
            .collect();
        if !pinned.is_empty() {
            rows.push(Row::Pinned);
            rows.extend(pinned);
        }
        rows.push(Row::Workspaces);
        for project in &session.projects {
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
            .map(|t| t.projects.iter().filter(|p| p.archived_at.is_some()).collect())
            .unwrap_or_default();
        if !archived.is_empty() {
            rows.push(Row::Archived { count: archived.len(), open: self.archived_open });
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
