//! Authoritative ThinkTerm session snapshots built by the mux server.
//!
//! The persisted tree supplies Space/Project/Thread identity and ordering;
//! the live mux supplies the exact window/tab/pane mapping. No client cache or
//! GUI projection participates in this view.

use codec::{
    ThinkTermSessionProject, ThinkTermSessionSpace, ThinkTermSessionState, ThinkTermSessionTab,
    ThinkTermSessionThread, ThinkTermSessionWorkStatus,
};
use mux::{Mux, MuxNotification};
use std::sync::atomic::{AtomicU64, Ordering};
use wezterm_term::Progress;

static GENERATION: AtomicU64 = AtomicU64::new(1);

pub fn snapshot() -> anyhow::Result<ThinkTermSessionState> {
    let tree = crate::thinkterm_tree::snapshot();
    let mux = Mux::get();

    let mut spaces: Vec<ThinkTermSessionSpace> = tree
        .spaces
        .iter()
        .map(|space| ThinkTermSessionSpace {
            id: space.id.clone(),
            name: space.name.clone(),
            is_default: false,
            domain: None,
        })
        .collect();

    let mut projects: Vec<ThinkTermSessionProject> = tree
        .projects
        .iter()
        // Archived projects are hidden everywhere; the session projection
        // (TUI sidebar, CLI listings, attach pickers) must not resurface
        // them. Their panes are gone, so nothing live is lost.
        .filter(|project| project.archived_at.is_none())
        .map(|project| ThinkTermSessionProject {
            id: project.id.clone(),
            space_id: project.space_id.clone(),
            name: project.name.clone(),
            path: project.path.clone(),
            threads: project
                .threads
                .iter()
                .map(|thread| {
                    let (tabs, running, needs_attention) = match thread
                        .materialized_workspace_name
                        .as_deref()
                        .or(thread.planned_workspace_name.as_deref())
                    {
                        Some(workspace) => workspace_tabs(&mux, workspace),
                        None => (Vec::new(), false, false),
                    };
                    ThinkTermSessionThread {
                        id: thread.id.clone(),
                        project_id: thread.project_id.clone(),
                        name: thread.name.clone(),
                        planned_workspace_name: thread.planned_workspace_name.clone(),
                        materialized_workspace_name: thread.materialized_workspace_name.clone(),
                        is_pinned: thread.is_pinned,
                        is_unread: thread.is_unread,
                        work_status: work_status_of(
                            thread.is_unread,
                            needs_attention,
                            running,
                        ),
                        tabs,
                    }
                })
                .collect(),
        })
        .collect();

    // Windows no thread of the tree claims -- the server's own startup
    // `default`, a CLI `spawn`, an older version's names -- run on this
    // server while the projection above walks only the tree, so every thin
    // client (browser, mobile, TUI, CLI listings) learns nothing about them.
    // The desktop adopts its own rows for these (see the GUI's
    // `adopt_orphan_remote_thread_windows`); the server lists them here
    // instead, so the rest can see and open them.
    let mut server_windows = Vec::new();
    for workspace in unclaimed_plain_workspaces(&tree, &mux.iter_workspaces()) {
        let (tabs, running, needs_attention) = workspace_tabs(&mux, &workspace);
        // A workspace whose every window is empty holds GUI bookkeeping on
        // some client, not a session; a row for it would be a dead click.
        if tabs.is_empty() {
            continue;
        }
        server_windows.push(server_windows_thread(
            &workspace,
            tabs,
            running,
            needs_attention,
        ));
    }
    if !server_windows.is_empty() {
        let space_id = match spaces.first() {
            Some(first) => first.id.clone(),
            // A server whose tree has no Spaces yet still has its startup
            // terminals; give their rows somewhere to sit.
            None => {
                let id = SERVER_WINDOWS_SPACE_ID.to_string();
                spaces.push(ThinkTermSessionSpace {
                    id: id.clone(),
                    name: "Server".to_string(),
                    is_default: false,
                    domain: None,
                });
                id
            }
        };
        projects.push(ThinkTermSessionProject {
            id: SERVER_WINDOWS_PROJECT_ID.to_string(),
            space_id,
            name: "Server windows".to_string(),
            path: String::new(),
            threads: server_windows,
        });
    }

    Ok(ThinkTermSessionState {
        server_id: mux.runtime_server_id().to_string(),
        tree_revision: tree.revision,
        generation: GENERATION.fetch_add(1, Ordering::AcqRel),
        spaces,
        projects,
    })
}

/// The prefix a desktop's session-server handoff parks replaced windows
/// under before re-adopting or killing them (see `wezterm-gui`'s
/// `local_sessions`). Transitional zombies must not get a row.
const DEAD_SESSION_WORKSPACE_PREFIX: &str = "dead-session:";

/// The id of the projection's one project for windows no thread claims, and
/// of the Space holding it when the tree has none. Never a tree row: tree
/// ops that name them apply to nothing on the server, which is exactly the
/// guard thin clients need against mutating a projection row.
const SERVER_WINDOWS_PROJECT_ID: &str = "session-windows";
const SERVER_WINDOWS_SPACE_ID: &str = "session-windows-space";

/// One workspace's live tabs, with the work signals its panes report.
fn workspace_tabs(mux: &Mux, workspace: &str) -> (Vec<ThinkTermSessionTab>, bool, bool) {
    let mut tabs = Vec::new();
    let mut running = false;
    let mut needs_attention = false;
    for window_id in mux.iter_windows_in_workspace(workspace) {
        let Some(window) = mux.get_window(window_id) else {
            continue;
        };
        let active_tab = window.get_active().map(|tab| tab.tab_id());
        tabs.extend(window.iter().map(|tab| {
            let panes = tab.iter_all_panes();
            for pane in &panes {
                match pane.get_progress() {
                    Progress::None => {}
                    Progress::Percentage(_) | Progress::Indeterminate => {
                        running = true;
                    }
                    Progress::Error(_) => needs_attention = true,
                }
            }
            ThinkTermSessionTab {
                window_id,
                tab_id: tab.tab_id(),
                pane_ids: panes.into_iter().map(|pane| pane.pane_id()).collect(),
                title: tab.get_title(),
                is_active: active_tab == Some(tab.tab_id()),
            }
        }));
    }
    (tabs, running, needs_attention)
}

fn work_status_of(
    is_unread: bool,
    needs_attention: bool,
    running: bool,
) -> ThinkTermSessionWorkStatus {
    if is_unread {
        ThinkTermSessionWorkStatus::FinishedUnseen
    } else if needs_attention {
        ThinkTermSessionWorkStatus::NeedsAttention
    } else if running {
        ThinkTermSessionWorkStatus::Running
    } else {
        ThinkTermSessionWorkStatus::Idle
    }
}

/// The live-mux workspaces no thread of the tree claims, in stable order.
/// Claims are counted across every project, the archived ones included:
/// their panes are on the way out, and a dying claim must not double-list
/// a workspace under a projection row. A handoff's parked zombies and the
/// empty name never qualify.
fn unclaimed_plain_workspaces(tree: &codec::ThinkTermTree, live: &[String]) -> Vec<String> {
    let claimed = |workspace: &str| {
        tree.projects.iter().any(|project| {
            project.threads.iter().any(|thread| {
                thread
                    .materialized_workspace_name
                    .as_deref()
                    .or(thread.planned_workspace_name.as_deref())
                    == Some(workspace)
            })
        })
    };
    let mut out: Vec<String> = live
        .iter()
        .filter(|workspace| {
            !workspace.is_empty()
                && !workspace.starts_with(DEAD_SESSION_WORKSPACE_PREFIX)
                && !claimed(workspace)
        })
        .cloned()
        .collect();
    out.sort();
    out.dedup();
    out
}

/// The projection row for one unclaimed workspace: named after it, keyed by
/// it, materialized under it, so thin clients find the same row across
/// snapshots and open its tabs by their own ids.
fn server_windows_thread(
    workspace: &str,
    tabs: Vec<ThinkTermSessionTab>,
    running: bool,
    needs_attention: bool,
) -> ThinkTermSessionThread {
    ThinkTermSessionThread {
        id: format!("session-window:{workspace}"),
        project_id: SERVER_WINDOWS_PROJECT_ID.to_string(),
        name: workspace.to_string(),
        planned_workspace_name: None,
        materialized_workspace_name: Some(workspace.to_string()),
        is_pinned: false,
        is_unread: false,
        work_status: work_status_of(false, needs_attention, running),
        tabs,
    }
}

/// Kept as the notification bridge for topology/tree callers. The payload is
/// always rebuilt from server-owned state when dispatch sends it.
pub fn publish_changed() {
    Mux::notify_from_any_thread(MuxNotification::ThinkTermSessionChanged);
}

#[cfg(test)]
mod tests {
    use super::*;
    use codec::{ThinkTermTree, TtProject, TtSpace, TtThread};

    fn thread(id: &str, materialized: Option<&str>, planned: Option<&str>) -> TtThread {
        TtThread {
            id: id.to_string(),
            project_id: "p1".to_string(),
            name: id.to_string(),
            planned_workspace_name: planned.map(str::to_string),
            materialized_workspace_name: materialized.map(str::to_string),
            last_active_at: 0,
            is_pinned: false,
            is_unread: false,
        }
    }

    fn project(archived_at: Option<i64>, threads: Vec<TtThread>) -> TtProject {
        TtProject {
            id: "p1".to_string(),
            space_id: "s1".to_string(),
            name: "Work".to_string(),
            path: String::new(),
            threads,
            archived_at,
        }
    }

    fn tree(projects: Vec<TtProject>) -> ThinkTermTree {
        ThinkTermTree {
            spaces: vec![TtSpace {
                id: "s1".to_string(),
                name: "Work".to_string(),
            }],
            projects,
            revision: 3,
        }
    }

    /// Only the plain names no thread claims are listed: materialized and
    /// planned claims both count, an archived project's claim still holds,
    /// and a handoff's parked zombie never shows.
    #[test]
    fn unclaimed_plain_workspaces_skip_claims_pages_and_parking() {
        let tree = tree(vec![
            project(
                None,
                vec![
                    thread("t1", Some("thinkterm:p1:t1"), None),
                    thread("t2", None, Some("planned-ws")),
                ],
            ),
            project(
                Some(9),
                vec![thread("t3", Some("an-archived-claim"), None)],
            ),
        ]);
        let live = vec![
            "default".to_string(),
            "thinkterm:p1:t1".to_string(),
            "planned-ws".to_string(),
            "an-archived-claim".to_string(),
            "dead-session:9:default".to_string(),
            String::new(),
            "cli-workspace".to_string(),
        ];
        assert_eq!(
            unclaimed_plain_workspaces(&tree, &live),
            vec!["cli-workspace".to_string(), "default".to_string()]
        );
    }

    /// The row for one unclaimed workspace is named and keyed by it, so
    /// clients find the same row across snapshots.
    #[test]
    fn a_server_window_row_is_named_and_keyed_by_its_workspace() {
        let row = server_windows_thread("default", Vec::new(), true, false);
        assert_eq!(row.id, "session-window:default");
        assert_eq!(row.project_id, SERVER_WINDOWS_PROJECT_ID);
        assert_eq!(row.name, "default");
        assert_eq!(row.materialized_workspace_name.as_deref(), Some("default"));
        assert_eq!(row.planned_workspace_name, None);
        assert_eq!(row.work_status, ThinkTermSessionWorkStatus::Running);
    }
}
