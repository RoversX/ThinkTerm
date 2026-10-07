//! Execute validated import plans in their terminal owner. Source adapters
//! register outside this crate and retain their own discovery and protocols.
mod layout;
mod pending;
mod pty;
mod receipt;
#[cfg(test)]
mod tests;

use anyhow::{ensure, Context, Result};
use codec::{ImportSessionRequest, ImportSessionResponse, ThinkTermTree, TreeOp};
use layout::{layout_tree, minimum_size};
use mux::localpane::LocalPane;
use mux::pane::{alloc_pane_id, Pane, PaneId};
use mux::tab::Tab;
use mux::window::Window;
use mux::Mux;
pub(crate) use pending::ensure_not_pending;
pub use pending::wait_for_workspace;
use portable_pty::{CommandBuilder, MasterPty};
use promise::spawn::spawn_into_new_thread;
use std::collections::HashMap;
use std::sync::Arc;
use thinkterm_import::{Direction, Handoff, ImportContext, ImportPlan, Layout, TerminalSeed};
use thinkterm_proto::{PaneEntry, PaneNode, SplitDirection, SplitDirectionAndSize};
use wezterm_term::{Terminal, TerminalSize};

static IMPORT: smol::lock::Mutex<()> = smol::lock::Mutex::new(());

struct LivePane {
    id: PaneId,
    pid: u32,
    terminal: Terminal,
    master: pty::ImportedPty,
}

struct Prepared {
    plan: ImportPlan,
    transfer: Option<Box<dyn Handoff>>,
    panes: HashMap<u32, LivePane>,
}

/// Layouts are available before the live handoff commits, including panes
/// that have not yet been attached to the mux or displayed by a frontend.
pub struct ImportedThreadLayout {
    pub thread_id: String,
    pub active_tab: usize,
    pub tabs: Vec<PaneNode>,
}

pub fn context() -> Result<ImportContext> {
    Ok(ImportContext {
        home: config::HOME_DIR.to_path_buf(),
        config_home: std::env::var_os("XDG_CONFIG_HOME").map(Into::into),
        executable: std::env::current_exe()?,
    })
}

pub fn discover(request: codec::ListImportSessions) -> Result<codec::ListImportSessionsResponse> {
    ensure!(
        thinkterm_import::valid_source_id(&request.source),
        "Invalid import source"
    );
    let source = thinkterm_import::source(&request.source)?;
    Ok(codec::ListImportSessionsResponse {
        sessions: source.discover(&context()?)?,
        notes: source.notes(None).into_iter().map(str::to_owned).collect(),
    })
}

pub fn preview(
    request: codec::PreviewImportSession,
) -> Result<codec::PreviewImportSessionResponse> {
    ensure!(
        thinkterm_import::valid_source_id(&request.source),
        "Invalid import source"
    );
    ensure!(
        !request.session.is_empty() && request.session.len() <= 4096,
        "Invalid import session"
    );
    let source = thinkterm_import::source(&request.source)?;
    let preview = source.preview(&context()?, &request.session)?;
    let mode = if preview.live {
        thinkterm_import::ImportMode::Live
    } else {
        thinkterm_import::ImportMode::Layout
    };
    Ok(codec::PreviewImportSessionResponse {
        notes: source
            .notes(Some(mode))
            .into_iter()
            .map(str::to_owned)
            .collect(),
        preview,
    })
}

fn initial_size(seed: &TerminalSeed) -> TerminalSize {
    TerminalSize {
        rows: seed.rows as usize,
        cols: seed.cols as usize,
        pixel_width: seed.cols as usize * seed.cell_width_px.min(1024) as usize,
        pixel_height: seed.rows as usize * seed.cell_height_px.min(1024) as usize,
        dpi: 96,
    }
}

fn seed_config(config: config::ConfigHandle) -> Arc<config::TermConfig> {
    Arc::new(config::TermConfig::with_config(
        config.adjusted(|config| config.enable_kitty_keyboard = true),
    ))
}

fn prepare(request: &thinkterm_import::ImportRequest) -> Result<Prepared> {
    let imported = thinkterm_import::prepare(&context()?, request)?;
    let saved: HashMap<_, _> = imported
        .plan
        .tabs()
        .flat_map(|tab| tab.panes.iter())
        .collect();
    let mut panes = HashMap::new();
    for live in imported.terminals {
        let master = pty::ImportedPty::prepare(live.pty).context("Prepare imported PTY")?;
        let size = initial_size(&live.seed);
        // Restoring saved output cannot send a reply to a process still owned
        // by the source. Only the final terminal receives the live writer.
        let mut seed_terminal = Terminal::new(
            size,
            seed_config(config::configuration()),
            "ThinkTerm",
            config::wezterm_version(),
            Box::new(std::io::sink()),
        );
        seed_terminal.advance_bytes(live.seed.ansi.as_bytes());
        let mut state = seed_terminal.snapshot();
        state.identity.title = saved[&live.pane_id]
            .title
            .clone()
            .or(live.seed.title)
            .unwrap_or_default();
        let mut terminal = Terminal::new(
            size,
            Arc::new(config::TermConfig::new()),
            "ThinkTerm",
            config::wezterm_version(),
            master.take_writer()?,
        );
        terminal
            .restore(state)
            .context("Prepare imported terminal state")?;
        panes.insert(
            live.pane_id,
            LivePane {
                id: alloc_pane_id(),
                pid: live.seed.child_pid,
                terminal,
                master,
            },
        );
    }
    Ok(Prepared {
        plan: imported.plan,
        transfer: imported.handoff,
        panes,
    })
}

pub fn tree_ops(tree: &ThinkTermTree, add: bool) -> Vec<TreeOp> {
    let mut ops = Vec::new();
    for space in &tree.spaces {
        ops.push(if add {
            TreeOp::CreateSpace {
                space_id: space.id.clone(),
                name: space.name.clone(),
            }
        } else {
            TreeOp::DeleteSpace {
                space_id: space.id.clone(),
            }
        });
    }
    if add {
        for project in &tree.projects {
            ops.push(TreeOp::CreateProject {
                project_id: project.id.clone(),
                space_id: project.space_id.clone(),
                name: project.name.clone(),
                path: project.path.clone(),
            });
            for thread in &project.threads {
                ops.push(TreeOp::CreateThread {
                    thread_id: thread.id.clone(),
                    project_id: project.id.clone(),
                    name: thread.name.clone(),
                    workspace: thread.materialized_workspace_name.clone(),
                    created_at: thread.last_active_at,
                });
            }
        }
    }
    ops
}

fn make_tree(plan: &ImportPlan, name: &str) -> ThinkTermTree {
    let space_id = uuid::Uuid::new_v4().to_string();
    let mut tree = ThinkTermTree::default();
    tree.spaces.push(codec::TtSpace {
        id: space_id.clone(),
        name: name.into(),
    });
    for project in &plan.projects {
        let project_id = format!("project-{}", uuid::Uuid::new_v4());
        let threads = project
            .threads
            .iter()
            .map(|thread| {
                let thread_id = format!("thread-{}", uuid::Uuid::new_v4());
                let workspace = format!("thinkterm:{}:{}", project_id, thread_id);
                codec::TtThread {
                    id: thread_id,
                    project_id: project_id.clone(),
                    name: thread.name.clone(),
                    planned_workspace_name: Some(workspace.clone()),
                    materialized_workspace_name: Some(workspace),
                    last_active_at: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs() as i64,
                    is_pinned: false,
                    is_unread: false,
                }
            })
            .collect();
        tree.projects.push(codec::TtProject {
            id: project_id,
            space_id: space_id.clone(),
            name: project.name.clone(),
            path: project.directory.clone(),
            archived_at: None,
            threads,
        });
    }
    tree
}

/// Receipts take a file lock, so the lookup runs on a blocking thread.
pub async fn status(request_id: String) -> Result<codec::ImportSessionStatus> {
    smol::unblock(move || receipt::Store::owner().status(&request_id)).await
}

/// The owner persists the new rows and their layouts before committing the
/// handoff. Failed preparation or commit removes those rows and newly spawned
/// shells without touching any existing session.
pub async fn execute<F>(request: ImportSessionRequest, persist: F) -> Result<ImportSessionResponse>
where
    F: FnMut(&ThinkTermTree, bool, &[ImportedThreadLayout]) -> Result<()>,
{
    let _import = IMPORT.lock().await;
    let started = {
        let request = request.clone();
        smol::unblock(move || receipt::Store::owner().start(request)).await?
    };
    let attempt = match started {
        receipt::Started::Completed(result) => return Ok(result),
        receipt::Started::New(attempt) => attempt,
    };
    let result = execute_new(request.request, persist, &attempt).await;
    match result {
        Ok(result) => {
            attempt.complete().await.context(
                "Save completed import receipt; query the import result before retrying",
            )?;
            Ok(result)
        }
        Err(err) => {
            attempt
                .fail(&format!("{err:#}"))
                .await
                .context("Save failed import receipt; query the import result before retrying")?;
            Err(err)
        }
    }
}

async fn execute_new<F>(
    request: thinkterm_import::ImportRequest,
    mut persist: F,
    attempt: &receipt::Attempt,
) -> Result<ImportSessionResponse>
where
    F: FnMut(&ThinkTermTree, bool, &[ImportedThreadLayout]) -> Result<()>,
{
    let _activity = mux::activity::Activity::new();
    request.validate()?;
    let mux = Mux::get();
    let domain = mux
        .get_domain_by_name("local")
        .context("Local terminal domain is unavailable")?;
    ensure!(
        domain
            .downcast_ref::<mux::domain::LocalDomain>()
            .is_some_and(|d| d.is_plain_local()),
        "Session import requires a local terminal owner"
    );
    let prepare_request = request.clone();
    let mut prepared = spawn_into_new_thread(move || prepare(&prepare_request)).await?;
    let tree = make_tree(&prepared.plan, request.space_name.trim());
    let selected = prepared.plan.active;
    let response = ImportSessionResponse {
        space_id: tree.spaces[0].id.clone(),
        workspace: tree.projects[selected.project].threads[selected.thread]
            .materialized_workspace_name
            .clone()
            .unwrap(),
        live: request.mode.is_live(),
        pane_count: prepared.plan.pane_count(),
        tree: tree.clone(),
    };
    attempt
        .destination(response.clone())
        .await
        .context("Save import destination before handoff")?;

    let mut panes: HashMap<u32, Arc<dyn Pane>> = HashMap::new();
    if prepared.transfer.is_none() {
        for tab in prepared.plan.tabs() {
            for (id, saved) in &tab.panes {
                // default_prog may be an agent or other saved command. A
                // layout import only starts a login shell at the saved cwd.
                let shell = CommandBuilder::new_default_prog().get_shell();
                let mut command = CommandBuilder::new(&shell);
                command.arg("-l");
                command.env("SHELL", &shell);
                command.set_require_cwd(true);
                command.cwd(&saved.cwd);
                match domain
                    .spawn_pane(
                        TerminalSize::default(),
                        Some(command),
                        Some(saved.cwd.clone()),
                    )
                    .await
                {
                    Ok(pane) => {
                        if let Some(title) = &saved.title {
                            pane.perform_actions(vec![
                                termwiz::escape::Action::OperatingSystemCommand(Box::new(
                                    termwiz::escape::OperatingSystemCommand::SetWindowTitle(
                                        title.clone(),
                                    ),
                                )),
                            ]);
                        }
                        panes.insert(*id, pane);
                    }
                    Err(err) => {
                        for pane in panes.values() {
                            mux.remove_pane(pane.pane_id());
                        }
                        return Err(err.context("Create imported shells"));
                    }
                }
            }
        }
    }
    let mut windows = Vec::new();
    let mut layouts = Vec::new();
    for (saved_project, project) in prepared.plan.projects.iter().zip(&tree.projects) {
        for (saved_thread, thread) in saved_project.threads.iter().zip(&project.threads) {
            let workspace = thread.materialized_workspace_name.as_ref().unwrap();
            let window = Window::new(Some(workspace.clone()), None, None);
            let mut tabs = Vec::new();
            let mut roots = Vec::new();
            for saved_tab in &saved_thread.tabs {
                let (min_cols, min_rows) = minimum_size(&saved_tab.layout);
                let size = TerminalSize {
                    cols: 120.max(min_cols),
                    rows: 36.max(min_rows),
                    ..TerminalSize::default()
                };
                let tab = Arc::new(Tab::new(&size));
                if let Some(title) = &saved_tab.name {
                    tab.set_title(title);
                }
                let focused = saved_tab
                    .focused
                    .unwrap_or_else(|| saved_tab.layout.first_pane());
                let entries = saved_tab
                    .panes
                    .iter()
                    .map(|(id, saved)| {
                        let pane_id = prepared
                            .panes
                            .get(id)
                            .map(|p| p.id)
                            .unwrap_or_else(|| panes[id].pane_id());
                        (
                            *id,
                            PaneEntry {
                                window_id: window.window_id(),
                                tab_id: tab.tab_id(),
                                pane_id,
                                title: saved.title.clone().unwrap_or_default(),
                                size,
                                working_dir: url::Url::from_directory_path(&saved.cwd)
                                    .ok()
                                    .map(Into::into),
                                is_active_pane: *id == focused,
                                is_zoomed_pane: saved_tab.zoomed && *id == focused,
                                alt_screen: false,
                                workspace: workspace.clone(),
                                cursor_pos: Default::default(),
                                physical_top: 0,
                                top_row: 0,
                                left_col: 0,
                                tty_name: None,
                            },
                        )
                    })
                    .collect();
                let root = layout_tree(&saved_tab.layout, size, &entries);
                roots.push(root);
                tabs.push((tab, size));
            }
            layouts.push(ImportedThreadLayout {
                thread_id: thread.id.clone(),
                active_tab: saved_thread.active_tab,
                tabs: roots,
            });
            windows.push((window, tabs, saved_thread.active_tab));
        }
    }
    let pending = pending::PendingImport::new(
        tree.projects
            .iter()
            .flat_map(|project| &project.threads)
            .map(|thread| thread.materialized_workspace_name.clone().unwrap())
            .collect(),
    );
    if let Err(err) = persist(&tree, true, &layouts) {
        for pane in panes.values() {
            mux.remove_pane(pane.pane_id());
        }
        return Err(err.context("Save imported Space before handoff"));
    }
    if prepared.transfer.is_some() {
        let (returned, committed) = spawn_into_new_thread(move || {
            let result = prepared.transfer.as_mut().unwrap().commit();
            if result.is_ok() {
                let sizes: Vec<_> = prepared
                    .panes
                    .values()
                    .filter_map(|pane| pane.master.get_size().ok().map(|size| (pane, size)))
                    .collect();
                for (pane, size) in &sizes {
                    let mut smaller = *size;
                    if smaller.rows > 2 {
                        smaller.rows -= 1;
                    } else if smaller.cols > 2 {
                        smaller.cols -= 1;
                    }
                    let _ = pane.master.resize(smaller);
                }
                std::thread::sleep(std::time::Duration::from_millis(30));
                for (pane, size) in sizes {
                    let _ = pane.master.resize(size);
                }
            }
            Ok((prepared, result))
        })
        .await?;
        prepared = returned;
        let transfer = prepared.transfer.take().unwrap();
        if let Err(err) = committed {
            drop(transfer);
            return Err(match persist(&tree, false, &layouts) {
                Ok(()) => err.context("Session handoff did not commit"),
                Err(rollback) => err.context(format!("Session handoff did not commit; removing the empty imported Space also failed: {rollback:#}")),
            });
        }
        for (id, live) in prepared.panes.drain() {
            let writer = live.terminal.writer_handle();
            let pane: Arc<dyn Pane> = Arc::new(LocalPane::new(
                live.id,
                live.terminal,
                portable_pty::adopted_child(live.pid),
                Box::new(live.master),
                writer,
                domain.domain_id(),
                "Imported terminal".into(),
            ));
            mux.add_pane(&pane)
                .expect("reserved imported terminal reader");
            panes.insert(id, pane);
        }
        transfer.finish();
    }
    let by_id: HashMap<_, _> = panes
        .values()
        .map(|pane| (pane.pane_id(), Arc::clone(pane)))
        .collect();
    for ((window, tabs, active), layout) in windows.into_iter().zip(layouts) {
        let builder = mux.insert_window(window);
        for ((tab, size), root) in tabs.into_iter().zip(layout.tabs) {
            for entry in root.entries() {
                let target = if entry.is_zoomed_pane {
                    size
                } else {
                    entry.size
                };
                if let Err(err) = by_id[&entry.pane_id].resize(target) {
                    log::warn!("resizing an imported terminal: {err:#}");
                }
            }
            tab.sync_with_pane_tree(size, root, |entry| Arc::clone(&by_id[&entry.pane_id]));
            mux.add_tab_no_panes(&tab);
            mux.add_tab_to_window(&tab, *builder)
                .expect("new imported window");
        }
        if let Some(mut window) = mux.get_window_mut(*builder) {
            let active = active.min(window.len().saturating_sub(1));
            window.set_active_without_saving(active);
        }
    }
    pending.finish();
    Ok(response)
}
