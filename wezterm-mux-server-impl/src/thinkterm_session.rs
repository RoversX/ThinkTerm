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

    let spaces = tree
        .spaces
        .iter()
        .map(|space| ThinkTermSessionSpace {
            id: space.id.clone(),
            name: space.name.clone(),
            is_default: false,
            domain: None,
        })
        .collect();

    let projects = tree
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
                    let workspace = thread
                        .materialized_workspace_name
                        .as_deref()
                        .or(thread.planned_workspace_name.as_deref());
                    let mut tabs = Vec::new();
                    let mut running = false;
                    let mut needs_attention = false;
                    if let Some(workspace) = workspace {
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
                                    pane_ids: panes
                                        .into_iter()
                                        .map(|pane| pane.pane_id())
                                        .collect(),
                                    title: tab.get_title(),
                                    is_active: active_tab == Some(tab.tab_id()),
                                }
                            }));
                        }
                    }
                    ThinkTermSessionThread {
                        id: thread.id.clone(),
                        project_id: thread.project_id.clone(),
                        name: thread.name.clone(),
                        planned_workspace_name: thread.planned_workspace_name.clone(),
                        materialized_workspace_name: thread.materialized_workspace_name.clone(),
                        is_pinned: thread.is_pinned,
                        is_unread: thread.is_unread,
                        work_status: if thread.is_unread {
                            ThinkTermSessionWorkStatus::FinishedUnseen
                        } else if needs_attention {
                            ThinkTermSessionWorkStatus::NeedsAttention
                        } else if running {
                            ThinkTermSessionWorkStatus::Running
                        } else {
                            ThinkTermSessionWorkStatus::Idle
                        },
                        tabs,
                    }
                })
                .collect(),
        })
        .collect();

    Ok(ThinkTermSessionState {
        server_id: mux.runtime_server_id().to_string(),
        tree_revision: tree.revision,
        generation: GENERATION.fetch_add(1, Ordering::AcqRel),
        spaces,
        projects,
    })
}

/// Kept as the notification bridge for topology/tree callers. The payload is
/// always rebuilt from server-owned state when dispatch sends it.
pub fn publish_changed() {
    Mux::notify_from_any_thread(MuxNotification::ThinkTermSessionChanged);
}
