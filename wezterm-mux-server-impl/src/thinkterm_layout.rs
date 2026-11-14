//! Durable server-side layout snapshots for remote ThinkTerm Threads.
//!
//! The mux server owns remote terminal topology.  This module records enough
//! structure to rebuild tabs, splits and pane-local stacks after the server
//! process disappears.  It deliberately does not attempt to preserve PTYs,
//! scrollback or running programs.

use anyhow::{Context, Result};
use config::keyassignment::SpawnTabDomain;
use futures::future::LocalBoxFuture;
use mux::domain::{LocalDomain, SplitSource};
use mux::kept_output::{self, Owner};
use mux::pane::{Pane, PaneId};
use mux::tab::{PaneEntry, PaneNode, SplitDirection, SplitRequest, SplitSize};
use mux::window::WindowId;
use mux::{Mux, MuxNotification};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use wezterm_term::TerminalSize;

const LAYOUT_VERSION: u32 = 1;
const SNAPSHOT_DEBOUNCE: Duration = Duration::from_millis(180);

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
struct StoredLayouts {
    version: u32,
    layouts: HashMap<String, StoredThreadLayout>,
}

impl Default for StoredLayouts {
    fn default() -> Self {
        Self {
            version: LAYOUT_VERSION,
            layouts: HashMap::new(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub(crate) struct StoredThreadLayout {
    workspace: String,
    active_tab: usize,
    /// PaneNode is also a wire type.  Keeping each tree as JSON allows an old
    /// server to leave a newer snapshot untouched when it cannot decode it.
    tabs: Vec<serde_json::Value>,
    #[serde(default)]
    terminal_specs: Vec<StoredTerminalSpec>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
struct StoredTerminalSpec {
    pane_id: PaneId,
    cwd: Option<String>,
    domain: Option<String>,
    /// Older snapshots written while pane titles were intentionally omitted
    /// do not contain this field. Titles are not used to restore the layout,
    /// but continuing to write them keeps snapshots readable by older server
    /// binaries after a downgrade.
    #[serde(default)]
    title: String,
    /// The label the pane was given; it is put back on the pane that
    /// replaces it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    label: Option<String>,
    /// The plugin's page the pane held in place of a terminal: the pane
    /// that replaces it holds the page again, and runs nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    page: Option<thinkterm_proto::PageSpec>,
}

#[derive(Debug)]
struct StoreState {
    stored: StoredLayouts,
    load_error: Option<String>,
    restoring_workspaces: HashSet<String>,
    /// Workspaces seen live in the preceding snapshot.  If one subsequently
    /// disappears, the user closed its final pane and the old layout must not
    /// come back on the next EnsureThinkTermThread.
    live_workspaces: HashSet<String>,
    /// A failed restore leaves one fallback shell.  Preserve the original
    /// snapshot while that fallback has the same structure; focus and resize
    /// notifications must not silently replace it.  A later real structural
    /// edit is allowed to become the new snapshot.
    protected_failed_layouts: HashMap<String, Option<u64>>,
}

lazy_static::lazy_static! {
    static ref STORE: Mutex<StoreState> = Mutex::new(load_state(&layout_path()));
}

static LISTENER_INSTALLED: AtomicBool = AtomicBool::new(false);
static CHANGE_GENERATION: AtomicU64 = AtomicU64::new(0);

/// The file's writes, in the order their snapshots were taken. A write
/// waits for the disk -- on macOS `sync_all` is a full flush, 5 to 30 ms --
/// so the debounced snapshot, taken on the main thread after every focus
/// change, hands its write to a thread of its own; `flush_now` still writes
/// in place. A write older than one already on disk is dropped.
static WRITE_SEQUENCE: AtomicU64 = AtomicU64::new(0);
/// A write handed off did not land: the next snapshot writes even when
/// nothing changed since.
static BACKGROUND_WRITE_FAILED: AtomicBool = AtomicBool::new(false);

lazy_static::lazy_static! {
    /// The newest write done, per file.
    static ref WRITTEN: Mutex<HashMap<PathBuf, u64>> = Mutex::new(HashMap::new());
    /// The newest write handed off and not yet started, per file.
    static ref PENDING_WRITE: Mutex<PendingWrite> = Mutex::new(PendingWrite::default());
}

#[derive(Default)]
struct PendingWrite {
    next: HashMap<PathBuf, (u64, StoredLayouts)>,
    writer_running: bool,
}

pub fn layout_path() -> PathBuf {
    config::DATA_DIR.join("thinkterm_layout.json")
}

fn load_state(path: &Path) -> StoreState {
    match load_from_path(path) {
        Ok(stored) => StoreState {
            stored,
            load_error: None,
            restoring_workspaces: HashSet::new(),
            live_workspaces: HashSet::new(),
            protected_failed_layouts: HashMap::new(),
        },
        Err(err) => {
            log::error!(
                "failed to load ThinkTerm layouts from {}: {err:#}; \
                 keeping the file untouched and opening one shell per Thread",
                path.display()
            );
            StoreState {
                stored: StoredLayouts::default(),
                load_error: Some(format!("{err:#}")),
                restoring_workspaces: HashSet::new(),
                live_workspaces: HashSet::new(),
                protected_failed_layouts: HashMap::new(),
            }
        }
    }
}

fn load_from_path(path: &Path) -> Result<StoredLayouts> {
    if !path.exists() {
        return Ok(StoredLayouts::default());
    }
    let file = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let stored: StoredLayouts = serde_json::from_reader(std::io::BufReader::new(file))
        .with_context(|| format!("parse {}", path.display()))?;
    if stored.version != LAYOUT_VERSION {
        anyhow::bail!(
            "unsupported ThinkTerm layout file version {} in {}",
            stored.version,
            path.display()
        );
    }
    Ok(stored)
}

fn save_to_path(path: &Path, stored: &StoredLayouts) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("{} has no parent directory", path.display()))?;
    std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    let mut file = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("create temporary layout file in {}", parent.display()))?;
    serde_json::to_writer_pretty(&mut file, stored)
        .with_context(|| format!("write {}", path.display()))?;
    file.flush()
        .with_context(|| format!("flush {}", path.display()))?;
    file.as_file()
        .sync_all()
        .with_context(|| format!("sync {}", path.display()))?;
    file.persist(path)
        .with_context(|| format!("replace {}", path.display()))?;
    Ok(())
}

fn commit_candidate(
    path: &Path,
    current: &mut StoredLayouts,
    candidate: StoredLayouts,
) -> Result<bool> {
    // What was handed to the writer is on disk before this returns: a
    // caller writing in place -- `flush_now`, just before the process
    // exits -- must not leave it to a thread about to die.
    finish_handed_off_writes();
    if candidate == *current && !BACKGROUND_WRITE_FAILED.load(Ordering::Acquire) {
        return Ok(false);
    }
    let sequence = WRITE_SEQUENCE.fetch_add(1, Ordering::AcqRel) + 1;
    save_in_order(path, &candidate, sequence)?;
    BACKGROUND_WRITE_FAILED.store(false, Ordering::Release);
    *current = candidate;
    Ok(true)
}

/// As `commit_candidate`, but the write happens on another thread: the
/// snapshot in memory changes now, the file as soon as the disk allows.
fn commit_candidate_in_background(
    path: &Path,
    current: &mut StoredLayouts,
    candidate: StoredLayouts,
) -> bool {
    if candidate == *current && !BACKGROUND_WRITE_FAILED.load(Ordering::Acquire) {
        return false;
    }
    let sequence = WRITE_SEQUENCE.fetch_add(1, Ordering::AcqRel) + 1;
    BACKGROUND_WRITE_FAILED.store(false, Ordering::Release);
    *current = candidate.clone();

    let mut pending = PENDING_WRITE.lock().unwrap();
    pending
        .next
        .insert(path.to_path_buf(), (sequence, candidate));
    if pending.writer_running {
        return true;
    }
    pending.writer_running = true;
    drop(pending);
    if let Err(err) = std::thread::Builder::new()
        .name("layout-writer".to_string())
        .spawn(write_pending)
    {
        log::warn!("starting the layout writer: {err:#}; writing in place");
        write_pending();
    }
    true
}

/// Write what was handed off, newest first, until nothing is left.
fn write_pending() {
    loop {
        let mut pending = PENDING_WRITE.lock().unwrap();
        let Some(path) = pending.next.keys().next().cloned() else {
            pending.writer_running = false;
            return;
        };
        let Some((sequence, stored)) = pending.next.remove(&path) else {
            continue;
        };
        // The file is held before the slot is let go: a commit finishing
        // the handed-off writes finds either the write still in the slot
        // or this one under way, and waits for it.
        let mut written = WRITTEN.lock().unwrap();
        drop(pending);
        write_handed_off(&mut written, &path, &stored, sequence);
    }
}

/// Write here what was handed off and not started yet, after the write
/// under way, if any.
fn finish_handed_off_writes() {
    let mut pending = PENDING_WRITE.lock().unwrap();
    let handed_off = std::mem::take(&mut pending.next);
    let mut written = WRITTEN.lock().unwrap();
    drop(pending);
    for (path, (sequence, stored)) in handed_off {
        write_handed_off(&mut written, &path, &stored, sequence);
    }
}

fn write_handed_off(
    written: &mut HashMap<PathBuf, u64>,
    path: &Path,
    stored: &StoredLayouts,
    sequence: u64,
) {
    if let Err(err) = save_in_order_locked(written, path, stored, sequence) {
        // Marked while the file is held, so a commit waiting on it sees it.
        BACKGROUND_WRITE_FAILED.store(true, Ordering::Release);
        log::error!("persisting ThinkTerm layouts: {err:#}");
    }
}

/// Write `stored` unless a newer snapshot of the file is already on disk.
fn save_in_order(path: &Path, stored: &StoredLayouts, sequence: u64) -> Result<()> {
    save_in_order_locked(&mut WRITTEN.lock().unwrap(), path, stored, sequence)
}

fn save_in_order_locked(
    written: &mut HashMap<PathBuf, u64>,
    path: &Path,
    stored: &StoredLayouts,
    sequence: u64,
) -> Result<()> {
    if written.get(path).is_some_and(|done| *done >= sequence) {
        return Ok(());
    }
    save_to_path(path, stored)?;
    written.insert(path.to_path_buf(), sequence);
    Ok(())
}

fn prune_orphaned_layouts(
    stored: &StoredLayouts,
    valid_thread_ids: &HashSet<String>,
) -> StoredLayouts {
    let mut candidate = stored.clone();
    candidate
        .layouts
        .retain(|thread_id, _| valid_thread_ids.contains(thread_id));
    candidate
}

pub fn initialize_mux(mux: &Mux) {
    if let Err(err) = reconcile_with_tree(&crate::thinkterm_tree::snapshot()) {
        log::error!("reconciling ThinkTerm layouts at startup: {err:#}");
    }
    if LISTENER_INSTALLED.swap(true, Ordering::AcqRel) {
        return;
    }
    mux.subscribe(|notification| {
        if notification_changes_layout(&notification) {
            schedule_snapshot();
        }
        true
    });
}

fn notification_changes_layout(notification: &MuxNotification) -> bool {
    matches!(
        notification,
        MuxNotification::PaneAdded(_)
            | MuxNotification::PaneRemoved(_)
            | MuxNotification::WindowCreated(_)
            | MuxNotification::WindowRemoved(_)
            | MuxNotification::WindowWorkspaceChanged(_)
            | MuxNotification::TabAddedToWindow { .. }
            | MuxNotification::PaneFocused(_)
            | MuxNotification::TabResized(_)
            | MuxNotification::WorkspaceRenamed { .. }
            | MuxNotification::PaneLabelChanged { .. }
            | MuxNotification::PageChanged(_)
            | MuxNotification::Empty
    )
}

fn schedule_snapshot() {
    let generation = CHANGE_GENERATION.fetch_add(1, Ordering::AcqRel) + 1;
    promise::spawn::spawn(async move {
        smol::Timer::after(SNAPSHOT_DEBOUNCE).await;
        if CHANGE_GENERATION.load(Ordering::Acquire) != generation {
            return;
        }
        promise::spawn::spawn_into_main_thread(async move {
            if CHANGE_GENERATION.load(Ordering::Acquire) != generation {
                return;
            }
            if let Err(err) = snapshot_live_layouts(generation, true) {
                log::error!("persisting ThinkTerm layouts: {err:#}");
            }
        })
        .detach();
    })
    .detach();
}

/// Persist the latest complete mux topology synchronously. The mux-server
/// calls this before controlled termination so the debounce window cannot
/// discard the user's final split/tab/focus change.
pub fn flush_now() -> Result<()> {
    let generation = CHANGE_GENERATION.fetch_add(1, Ordering::AcqRel) + 1;
    snapshot_live_layouts(generation, false)
}

/// Before a controlled exit, after `flush_now`: what each Thread's panes
/// show, kept by workspace until its layout is restored, when this machine
/// has that on. Only the panes the saved layout records, under its ids.
pub fn keep_pane_output() {
    if !kept_output::enabled() {
        kept_output::discard(Owner::Server);
        return;
    }
    let Some(mux) = Mux::try_get() else {
        return;
    };
    let records = thread_records(&crate::thinkterm_tree::snapshot());
    let recorded: Vec<(String, HashSet<PaneId>)> = {
        let store = STORE.lock().unwrap();
        records
            .iter()
            // A restore in flight has not replaced the saved layout yet, and
            // one that failed is kept as it was: in both the saved ids are
            // not the live panes', and a record under them would land in
            // the wrong pane.
            .filter(|(_, workspace)| {
                !store.restoring_workspaces.contains(workspace)
                    && !store.protected_failed_layouts.contains_key(workspace)
            })
            .filter_map(|(thread_id, workspace)| {
                let layout = store.stored.layouts.get(thread_id)?;
                let ids = layout
                    .terminal_specs
                    .iter()
                    .map(|spec| spec.pane_id)
                    .collect();
                Some((workspace.clone(), ids))
            })
            .collect()
    };
    for (workspace, ids) in recorded {
        let panes: Vec<(PaneId, Arc<dyn Pane>)> = mux
            .iter_windows_in_workspace(&workspace)
            .into_iter()
            .filter_map(|window_id| {
                let window = mux.get_window(window_id)?;
                Some(
                    window
                        .iter()
                        .flat_map(|tab| tab.iter_all_panes())
                        .collect::<Vec<_>>(),
                )
            })
            .flatten()
            .filter(|pane| ids.contains(&pane.pane_id()))
            .map(|pane| (pane.pane_id(), pane))
            .collect();
        kept_output::save(Owner::Server, &workspace, &panes);
    }
    let workspaces = records
        .into_iter()
        .map(|(_, workspace)| workspace)
        .collect();
    kept_output::prune(Owner::Server, &workspaces);
}

fn thread_records(tree: &codec::ThinkTermTree) -> Vec<(String, String)> {
    tree.projects
        .iter()
        .flat_map(|project| {
            project.threads.iter().filter_map(|thread| {
                thread
                    .materialized_workspace_name
                    .as_ref()
                    .or(thread.planned_workspace_name.as_ref())
                    .map(|workspace| (thread.id.clone(), workspace.clone()))
            })
        })
        .collect()
}

fn snapshot_live_layouts(_generation: u64, in_background: bool) -> Result<()> {
    let tree = crate::thinkterm_tree::snapshot();
    let records = thread_records(&tree);
    // An archived project's stored layouts are frozen: archiving kills its
    // panes on purpose, and the stored layout is exactly what unarchiving
    // restores from. Neither the live-empty prune nor a mid-teardown
    // partial snapshot may touch them.
    let archived_thread_ids: HashSet<String> = tree
        .projects
        .iter()
        .filter(|project| project.archived_at.is_some())
        .flat_map(|project| project.threads.iter().map(|thread| thread.id.clone()))
        .collect();
    let valid_thread_ids = records
        .iter()
        .map(|(thread_id, _)| thread_id.clone())
        .collect::<HashSet<_>>();
    let mux = Mux::get();
    let mut live = HashMap::new();
    let mut current_live_workspaces = HashSet::new();

    for (thread_id, workspace) in &records {
        let has_live_window = mux
            .iter_windows_in_workspace(workspace)
            .into_iter()
            .filter_map(|window_id| mux.get_window(window_id))
            .any(|window| window.iter().any(|tab| !tab.iter_all_panes().is_empty()));
        if has_live_window {
            current_live_workspaces.insert(workspace.clone());
        }
        if let Some(layout) = snapshot_workspace(&mux, workspace)? {
            live.insert(thread_id.clone(), layout);
        }
    }

    let mut store = STORE.lock().unwrap();
    if store.load_error.is_some() {
        return Ok(());
    }

    let mut candidate = prune_orphaned_layouts(&store.stored, &valid_thread_ids);

    for (thread_id, workspace) in &records {
        if archived_thread_ids.contains(thread_id) {
            continue;
        }
        if store.restoring_workspaces.contains(workspace) {
            continue;
        }
        if let Some(protected) = store.protected_failed_layouts.get(workspace) {
            let live_fingerprint = live.get(thread_id).and_then(layout_structure_fingerprint);
            if live_fingerprint == *protected {
                continue;
            }
            store.protected_failed_layouts.remove(workspace);
        }
        if let Some(layout) = live.remove(thread_id) {
            candidate.layouts.insert(thread_id.clone(), layout);
        } else if store.live_workspaces.contains(workspace) {
            candidate.layouts.remove(thread_id);
        }
    }

    let mut next_live = current_live_workspaces;
    for workspace in &store.restoring_workspaces {
        if store.live_workspaces.contains(workspace) {
            next_live.insert(workspace.clone());
        }
    }

    if in_background {
        commit_candidate_in_background(&layout_path(), &mut store.stored, candidate);
    } else {
        commit_candidate(&layout_path(), &mut store.stored, candidate)?;
    }
    store.live_workspaces = next_live;
    Ok(())
}

fn snapshot_workspace(mux: &Mux, workspace: &str) -> Result<Option<StoredThreadLayout>> {
    let mut window_ids = mux.iter_windows_in_workspace(workspace);
    window_ids.sort_unstable();
    let mut windows = Vec::new();

    for window_id in window_ids {
        let Some(window) = mux.get_window(window_id) else {
            continue;
        };
        if !window.iter().any(|tab| !tab.iter_all_panes().is_empty()) {
            continue;
        }
        let mut terminal_specs = Vec::new();
        let mut tabs = Vec::new();
        for tab in window.iter() {
            let node = tab.codec_pane_tree();
            collect_terminal_specs(mux, &node, &mut terminal_specs);
            tabs.push(serde_json::to_value(node).context("serialize pane tree")?);
        }
        windows.push((window.get_active_idx(), tabs, terminal_specs));
    }

    Ok(merge_workspace_snapshots(workspace, windows))
}

fn merge_workspace_snapshots(
    workspace: &str,
    windows: Vec<(usize, Vec<serde_json::Value>, Vec<StoredTerminalSpec>)>,
) -> Option<StoredThreadLayout> {
    let mut active_tab = 0;
    let mut saw_live_window = false;
    let mut terminal_specs = Vec::new();
    let mut tabs = Vec::new();
    for (window_active_tab, window_tabs, window_specs) in windows {
        if window_tabs.is_empty() {
            continue;
        }
        if !saw_live_window {
            active_tab = tabs.len() + window_active_tab.min(window_tabs.len() - 1);
            saw_live_window = true;
        }
        tabs.extend(window_tabs);
        terminal_specs.extend(window_specs);
    }
    if tabs.is_empty() {
        return None;
    }
    Some(StoredThreadLayout {
        workspace: workspace.to_string(),
        active_tab: active_tab.min(tabs.len() - 1),
        tabs,
        terminal_specs,
    })
}

fn collect_terminal_specs(
    mux: &Mux,
    node: &PaneNode,
    terminal_specs: &mut Vec<StoredTerminalSpec>,
) {
    match node {
        PaneNode::Empty => {}
        PaneNode::Leaf(entry) => collect_terminal_spec(mux, entry, terminal_specs),
        PaneNode::Stack(stack) => {
            for entry in &stack.panes {
                collect_terminal_spec(mux, entry, terminal_specs);
            }
        }
        PaneNode::Split { left, right, .. } => {
            collect_terminal_specs(mux, left, terminal_specs);
            collect_terminal_specs(mux, right, terminal_specs);
        }
    }
}

fn collect_terminal_spec(
    mux: &Mux,
    entry: &PaneEntry,
    terminal_specs: &mut Vec<StoredTerminalSpec>,
) {
    let pane = mux.get_pane(entry.pane_id);
    let domain = pane
        .as_ref()
        .and_then(|pane| mux.get_domain(pane.domain_id()))
        .map(|domain| domain.domain_name().to_string());
    terminal_specs.push(StoredTerminalSpec {
        pane_id: entry.pane_id,
        cwd: working_dir_from_entry(entry),
        domain,
        title: entry.title.clone(),
        label: mux.pane_label(entry.pane_id),
        page: pane.and_then(|pane| pane.page()),
    });
}

fn working_dir_from_entry(entry: &PaneEntry) -> Option<String> {
    entry.working_dir.as_ref().map(|url| {
        url.url
            .to_file_path()
            .ok()
            .and_then(|path| path.to_str().map(str::to_owned))
            .unwrap_or_else(|| url.url.path().to_string())
    })
}

pub(crate) fn reconcile_with_tree(tree: &codec::ThinkTermTree) -> Result<()> {
    // Archived projects deliberately keep their layouts: their threads stay
    // in the tree, so `valid` still covers them and unarchiving can rebuild
    // the shells from what is stored here.
    let valid = tree
        .projects
        .iter()
        .flat_map(|project| project.threads.iter().map(|thread| thread.id.clone()))
        .collect::<HashSet<_>>();
    let mut store = STORE.lock().unwrap();
    if let Some(err) = &store.load_error {
        anyhow::bail!(
            "refusing to overwrite unreadable {}: {err}",
            layout_path().display()
        );
    }
    let candidate = prune_orphaned_layouts(&store.stored, &valid);
    commit_candidate(&layout_path(), &mut store.stored, candidate)?;
    Ok(())
}

pub(crate) fn begin_restore(workspace: &str) {
    STORE
        .lock()
        .unwrap()
        .restoring_workspaces
        .insert(workspace.to_string());
}

pub(crate) fn finish_restore(workspace: &str, succeeded: bool) {
    let failed_fingerprint = if succeeded {
        None
    } else {
        snapshot_workspace(&Mux::get(), workspace)
            .ok()
            .flatten()
            .and_then(|layout| layout_structure_fingerprint(&layout))
    };
    {
        let mut store = STORE.lock().unwrap();
        store.restoring_workspaces.remove(workspace);
        if !succeeded {
            store
                .protected_failed_layouts
                .insert(workspace.to_string(), failed_fingerprint);
        }
    }
    if succeeded {
        schedule_snapshot();
    }
}

fn saved_layout(thread_id: &str) -> Option<StoredThreadLayout> {
    let store = STORE.lock().unwrap();
    if store.load_error.is_some() {
        return None;
    }
    store.stored.layouts.get(thread_id).cloned()
}

pub(crate) async fn restore_thread_layout(
    thread_id: &str,
    workspace: &str,
    project_path: &str,
    size: TerminalSize,
) -> Result<bool> {
    let Some(layout) = saved_layout(thread_id) else {
        return Ok(false);
    };
    if layout.workspace != workspace {
        log::warn!(
            "ignoring stale ThinkTerm layout workspace {} for thread {thread_id}; expected {workspace}",
            layout.workspace
        );
        return Ok(false);
    }
    let tabs = decode_layout_tabs(&layout)?;
    if tabs.is_empty() {
        return Ok(false);
    }

    let mux = Mux::get();
    let mut created_window = None;
    let result = restore_decoded_layout(
        &mux,
        workspace,
        project_path,
        size,
        &layout,
        &tabs,
        &mut created_window,
    )
    .await;
    if result.is_err() {
        if let Some(window_id) = created_window {
            mux.kill_window(window_id);
        }
    } else {
        kept_output::finish(workspace);
    }
    result.map(|()| true)
}

fn decode_layout_tabs(layout: &StoredThreadLayout) -> Result<Vec<PaneNode>> {
    layout
        .tabs
        .iter()
        .map(|tab| {
            serde_json::from_value::<PaneNode>(tab.clone())
                .context("decode saved ThinkTerm pane tree")
        })
        .collect()
}

async fn restore_decoded_layout(
    mux: &Mux,
    workspace: &str,
    project_path: &str,
    size: TerminalSize,
    layout: &StoredThreadLayout,
    tabs: &[PaneNode],
    created_window: &mut Option<WindowId>,
) -> Result<()> {
    let specs = layout
        .terminal_specs
        .iter()
        .map(|spec| (spec.pane_id, spec.clone()))
        .collect::<HashMap<_, _>>();
    let mut spawned_tabs = Vec::new();

    for node in tabs {
        let first = first_pane_entry(node);
        let cwd = first.and_then(|entry| cwd_for_entry(entry, &specs, project_path));
        let domain = first
            .map(|entry| domain_for_entry(mux, entry, &specs, true))
            .unwrap_or(SpawnTabDomain::DefaultDomain);
        let command = first.and_then(|entry| kept_command(mux, workspace, entry, &domain, None));
        // A tab that began with a page begins with it again: no shell is
        // started in a page's place.
        let (tab, pane, window_id) = match first.and_then(|entry| page_for_entry(entry, &specs)) {
            Some(page) => {
                mux.spawn_page_tab(
                    *created_window,
                    domain,
                    page,
                    size,
                    None,
                    workspace.to_string(),
                )
                .await
            }
            None => {
                mux.spawn_tab_or_window(
                    *created_window,
                    domain,
                    command,
                    cwd,
                    size,
                    None,
                    workspace.to_string(),
                    None,
                )
                .await
            }
        }
        .context("restore ThinkTerm tab")?;
        *created_window = Some(window_id);
        let mut pane_map = HashMap::new();
        restore_node(
            mux,
            pane.pane_id(),
            node,
            &specs,
            project_path,
            &mut pane_map,
            workspace,
        )
        .await?;
        for (saved, restored) in &pane_map {
            if let Some(label) = specs.get(saved).and_then(|spec| spec.label.as_deref()) {
                mux.set_pane_label(*restored, Some(label));
            }
        }
        if let Some(active) = active_pane_entry(node)
            .and_then(|entry| pane_map.get(&entry.pane_id))
            .and_then(|pane_id| mux.get_pane(*pane_id))
        {
            tab.set_active_pane_silent(&active);
        }
        spawned_tabs.push(tab);
    }

    if let Some(window_id) = *created_window {
        if let Some(mut window) = mux.get_window_mut(window_id) {
            window.set_active_without_saving(layout.active_tab.min(spawned_tabs.len() - 1));
        }
    }
    Ok(())
}

fn restore_node<'a>(
    mux: &'a Mux,
    base_pane_id: PaneId,
    node: &'a PaneNode,
    specs: &'a HashMap<PaneId, StoredTerminalSpec>,
    project_path: &'a str,
    pane_map: &'a mut HashMap<PaneId, PaneId>,
    workspace: &'a str,
) -> LocalBoxFuture<'a, Result<()>> {
    Box::pin(async move {
        match node {
            PaneNode::Empty => {}
            PaneNode::Leaf(entry) => {
                pane_map.insert(entry.pane_id, base_pane_id);
            }
            PaneNode::Stack(stack) => {
                if let Some(first) = stack.panes.first() {
                    pane_map.insert(first.pane_id, base_pane_id);
                }
                let base = mux.get_pane(base_pane_id).ok_or_else(|| {
                    anyhow::anyhow!("restored base pane {base_pane_id} disappeared")
                })?;
                let dims = base.get_dimensions();
                let stack_size = TerminalSize {
                    rows: dims.viewport_rows,
                    cols: dims.cols,
                    pixel_width: dims.pixel_width,
                    pixel_height: dims.pixel_height,
                    dpi: dims.dpi,
                };
                for entry in stack.panes.iter().skip(1) {
                    let domain = domain_for_entry(mux, entry, specs, false);
                    let command = kept_command(mux, workspace, entry, &domain, Some(base_pane_id));
                    let pane = match page_for_entry(entry, specs) {
                        Some(page) => {
                            mux.spawn_page_in_stack(base_pane_id, domain, page, stack_size)
                                .await
                        }
                        None => {
                            mux.spawn_pane_in_stack(
                                base_pane_id,
                                domain,
                                command,
                                cwd_for_entry(entry, specs, project_path),
                                stack_size,
                            )
                            .await
                        }
                    }
                    .context("restore ThinkTerm pane stack")?;
                    pane_map.insert(entry.pane_id, pane.pane_id());
                }
                if let Some(entry) = stack.panes.get(stack.active) {
                    if let Some(pane_id) = pane_map.get(&entry.pane_id) {
                        mux.activate_pane_in_stack(*pane_id)?;
                    }
                }
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
                let domain = right_entry
                    .map(|entry| domain_for_entry(mux, entry, specs, false))
                    .unwrap_or(SpawnTabDomain::CurrentPaneDomain);
                let command = right_entry.and_then(|entry| {
                    kept_command(mux, workspace, entry, &domain, Some(base_pane_id))
                });
                // A page, as a terminal, is of the domain it was of.
                let source = match right_entry.and_then(|entry| page_for_entry(entry, specs)) {
                    Some(page) => SplitSource::Page(page),
                    None => SplitSource::Spawn {
                        command,
                        command_dir: right_entry
                            .and_then(|entry| cwd_for_entry(entry, specs, project_path)),
                    },
                };
                let (right_pane, _) = mux
                    .split_pane(base_pane_id, request, source, domain)
                    .await
                    .context("restore ThinkTerm split")?;
                restore_node(
                    mux,
                    base_pane_id,
                    left,
                    specs,
                    project_path,
                    pane_map,
                    workspace,
                )
                .await?;
                restore_node(
                    mux,
                    right_pane.pane_id(),
                    right,
                    specs,
                    project_path,
                    pane_map,
                    workspace,
                )
                .await?;
            }
        }
        Ok(())
    })
}

/// The command that starts `entry`'s replacement with what it showed at the
/// last exit, when that was kept and a local domain here spawns it: the only
/// kind that reads the record (`mux::kept_output`).
fn kept_command(
    mux: &Mux,
    workspace: &str,
    entry: &PaneEntry,
    domain: &SpawnTabDomain,
    current: Option<PaneId>,
) -> Option<portable_pty::CommandBuilder> {
    let local = mux
        .resolve_spawn_tab_domain(current, domain)
        .is_ok_and(|domain| domain.downcast_ref::<LocalDomain>().is_some());
    if !local {
        return None;
    }
    kept_output::with_seed(Owner::Server, workspace, entry.pane_id, None)
}

fn first_pane_entry(node: &PaneNode) -> Option<&PaneEntry> {
    match node {
        PaneNode::Empty => None,
        PaneNode::Leaf(entry) => Some(entry),
        PaneNode::Stack(stack) => stack.panes.first(),
        PaneNode::Split { left, right, .. } => {
            first_pane_entry(left).or_else(|| first_pane_entry(right))
        }
    }
}

fn active_pane_entry(node: &PaneNode) -> Option<&PaneEntry> {
    match node {
        PaneNode::Empty => None,
        PaneNode::Leaf(entry) => entry.is_active_pane.then_some(entry),
        PaneNode::Stack(stack) => stack
            .panes
            .iter()
            .find(|entry| entry.is_active_pane)
            .or_else(|| stack.panes.get(stack.active)),
        PaneNode::Split { left, right, .. } => {
            active_pane_entry(left).or_else(|| active_pane_entry(right))
        }
    }
}

/// The plugin's page the saved pane held, when it held one, as it is
/// restored: with no terminal, whose id was another server's.
fn page_for_entry(
    entry: &PaneEntry,
    specs: &HashMap<PaneId, StoredTerminalSpec>,
) -> Option<thinkterm_proto::PageSpec> {
    Some(specs.get(&entry.pane_id)?.page.clone()?.restored())
}

fn cwd_for_entry(
    entry: &PaneEntry,
    specs: &HashMap<PaneId, StoredTerminalSpec>,
    project_path: &str,
) -> Option<String> {
    specs
        .get(&entry.pane_id)
        .and_then(|spec| usable_directory(spec.cwd.as_deref()))
        .or_else(|| usable_directory(working_dir_from_entry(entry).as_deref()))
        .or_else(|| usable_directory(Some(project_path)))
}

fn usable_directory(path: Option<&str>) -> Option<String> {
    let path = path?.trim();
    if path.is_empty() || path.starts_with("wezterm-mux://") {
        return None;
    }
    let expanded = if path == "~" {
        config::HOME_DIR.to_path_buf()
    } else if let Some(rest) = path.strip_prefix("~/") {
        config::HOME_DIR.join(rest)
    } else {
        PathBuf::from(path)
    };
    expanded
        .is_dir()
        .then(|| expanded.to_string_lossy().to_string())
}

fn domain_for_entry(
    mux: &Mux,
    entry: &PaneEntry,
    specs: &HashMap<PaneId, StoredTerminalSpec>,
    root: bool,
) -> SpawnTabDomain {
    specs
        .get(&entry.pane_id)
        .and_then(|spec| spec.domain.as_deref())
        // The name a client recorded is the client's name for *its*
        // connection to this server, and a config shared between server
        // and client gives the server a client domain of that same name,
        // pointing back at its own socket. Spawning through it mirrored
        // the server into itself without end. A client domain that leads
        // elsewhere is as good as any other.
        .filter(|domain| {
            mux.get_domain_by_name(domain).is_some_and(|found| {
                let ours = leads_back_here(&found);
                if ours {
                    log::debug!(
                        "restoring a layout: domain {domain} leads back to this server; \
                         using the default domain"
                    );
                }
                !ours
            })
        })
        .map(|domain| SpawnTabDomain::DomainName(domain.to_string()))
        .unwrap_or(if root {
            SpawnTabDomain::DefaultDomain
        } else {
            SpawnTabDomain::CurrentPaneDomain
        })
}

/// Whether `domain` is a client domain for one of the unix sockets this
/// server itself listens on.
fn leads_back_here(domain: &Arc<dyn mux::domain::Domain>) -> bool {
    let Some(client) = domain.downcast_ref::<wezterm_client::domain::ClientDomain>() else {
        return false;
    };
    let Some(socket) = client.unix_socket_path() else {
        return false;
    };
    config::configuration()
        .unix_domains
        .iter()
        .any(|ours| ours.socket_path() == socket)
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

fn layout_structure_fingerprint(layout: &StoredThreadLayout) -> Option<u64> {
    let mut hasher = DefaultHasher::new();
    layout.tabs.len().hash(&mut hasher);
    for tab in &layout.tabs {
        let node = serde_json::from_value::<PaneNode>(tab.clone()).ok()?;
        hash_node_structure(&node, &mut hasher);
    }
    Some(hasher.finish())
}

fn hash_node_structure<H: Hasher>(node: &PaneNode, hasher: &mut H) {
    match node {
        PaneNode::Empty => 0u8.hash(hasher),
        PaneNode::Leaf(entry) => {
            1u8.hash(hasher);
            entry.pane_id.hash(hasher);
        }
        PaneNode::Stack(stack) => {
            2u8.hash(hasher);
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
            hash_node_structure(left, hasher);
            hash_node_structure(right, hasher);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mux::renderable::StableCursorPosition;

    #[test]
    fn a_saved_pane_says_whether_it_held_a_page() {
        let mut layout = sample_layout("workspace-1");
        let page = thinkterm_proto::PageSpec {
            plugin: "board".to_string(),
            args: "{\"board\":\"release\"}".to_string(),
            title: "Board".to_string(),
            id: "0f3a9c2e5b7d1846".to_string(),
            cwd: "/home/user/project".to_string(),
            terminal: Some(7),
            ..Default::default()
        };
        let written = serde_json::to_value(&layout).unwrap();
        assert!(
            written["terminal_specs"][0].get("page").is_none(),
            "a terminal's record is as it was"
        );
        layout.terminal_specs[0].page = Some(page.clone());
        let written = serde_json::to_string(&layout).unwrap();
        let read: StoredThreadLayout = serde_json::from_str(&written).unwrap();
        assert_eq!(read.terminal_specs[0].page.as_ref(), Some(&page));
        let entry = pane_entry(7, TerminalSize::default(), true);
        let specs = HashMap::from([(7, read.terminal_specs[0].clone())]);
        // Restored with all it was but its terminal, whose id was another
        // server's.
        assert_eq!(page_for_entry(&entry, &specs), Some(page.clone().restored()));
        assert_eq!(page_for_entry(&entry, &specs).unwrap().id, page.id);
        assert_eq!(page_for_entry(&pane_entry(8, TerminalSize::default(), true), &specs), None);
    }

    fn sample_layout(workspace: &str) -> StoredThreadLayout {
        StoredThreadLayout {
            workspace: workspace.to_string(),
            active_tab: 1,
            tabs: vec![serde_json::to_value(PaneNode::Empty).unwrap()],
            terminal_specs: vec![StoredTerminalSpec {
                pane_id: 7,
                cwd: Some("/tmp".to_string()),
                domain: Some("local".to_string()),
                title: "shell".to_string(),
                label: None,
                page: None,
            }],
        }
    }

    #[test]
    fn missing_file_defaults_and_versioned_layout_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("thinkterm_layout.json");
        assert_eq!(load_from_path(&path).unwrap(), StoredLayouts::default());
        let mut stored = StoredLayouts::default();
        stored
            .layouts
            .insert("thread-1".to_string(), sample_layout("workspace-1"));
        save_to_path(&path, &stored).unwrap();
        assert_eq!(load_from_path(&path).unwrap(), stored);
    }

    #[test]
    fn snapshot_without_terminal_title_remains_readable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("thinkterm_layout.json");
        let mut stored = StoredLayouts::default();
        stored
            .layouts
            .insert("thread-1".to_string(), sample_layout("workspace-1"));

        let mut json = serde_json::to_value(&stored).unwrap();
        json["layouts"]["thread-1"]["terminal_specs"][0]
            .as_object_mut()
            .unwrap()
            .remove("title");
        std::fs::write(&path, serde_json::to_vec_pretty(&json).unwrap()).unwrap();

        let loaded = load_from_path(&path).unwrap();
        assert_eq!(loaded.layouts["thread-1"].terminal_specs[0].title, "");

        // Keep emitting the field so a later downgrade to a server that still
        // requires it can read the snapshot produced by this version.
        save_to_path(&path, &loaded).unwrap();
        let rewritten: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            rewritten["layouts"]["thread-1"]["terminal_specs"][0]["title"],
            ""
        );
    }

    #[test]
    fn malformed_or_unknown_layout_file_is_not_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let malformed = dir.path().join("malformed.json");
        std::fs::write(&malformed, b"not json").unwrap();
        assert!(load_from_path(&malformed).is_err());
        assert_eq!(std::fs::read(&malformed).unwrap(), b"not json");

        let unknown = dir.path().join("unknown.json");
        std::fs::write(&unknown, br#"{"version":99,"layouts":{}}"#).unwrap();
        assert!(load_from_path(&unknown).is_err());
        assert!(String::from_utf8(std::fs::read(&unknown).unwrap())
            .unwrap()
            .contains("\"version\":99"));
    }

    #[test]
    fn failed_atomic_replace_does_not_change_the_in_memory_candidate() {
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, b"file").unwrap();
        let path = blocker.join("thinkterm_layout.json");
        let mut current = StoredLayouts::default();
        current
            .layouts
            .insert("thread-1".to_string(), sample_layout("workspace-1"));
        let before = current.clone();
        assert!(commit_candidate(&path, &mut current, StoredLayouts::default()).is_err());
        assert_eq!(current, before);
    }

    #[test]
    fn an_older_write_never_lands_over_a_newer_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("thinkterm_layout.json");
        let mut newer = StoredLayouts::default();
        newer
            .layouts
            .insert("thread-1".to_string(), sample_layout("workspace-1"));
        save_in_order(&path, &newer, 2).unwrap();
        save_in_order(&path, &StoredLayouts::default(), 1).unwrap();
        assert_eq!(load_from_path(&path).unwrap(), newer);
    }

    #[test]
    fn a_write_handed_off_lands_while_memory_changes_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("thinkterm_layout.json");
        let mut current = StoredLayouts::default();
        let mut candidate = StoredLayouts::default();
        candidate
            .layouts
            .insert("thread-1".to_string(), sample_layout("workspace-1"));
        assert!(commit_candidate_in_background(
            &path,
            &mut current,
            candidate.clone()
        ));
        assert_eq!(current, candidate);
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while load_from_path(&path).ok().as_ref() != Some(&candidate) {
            assert!(
                std::time::Instant::now() < deadline,
                "the write never landed"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn writing_in_place_first_finishes_what_was_handed_off() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("thinkterm_layout.json");
        let mut current = StoredLayouts::default();
        let mut candidate = StoredLayouts::default();
        candidate
            .layouts
            .insert("thread-1".to_string(), sample_layout("workspace-1"));
        commit_candidate_in_background(&path, &mut current, candidate.clone());
        // As `flush_now` before an exit, with nothing new to write: what
        // the writer had is on disk by the time it returns.
        assert!(!commit_candidate(&path, &mut current, candidate.clone()).unwrap());
        assert_eq!(load_from_path(&path).unwrap(), candidate);
    }

    #[test]
    fn deleting_tree_entries_prunes_only_their_layouts() {
        let mut stored = StoredLayouts::default();
        stored
            .layouts
            .insert("keep".to_string(), sample_layout("workspace-keep"));
        stored
            .layouts
            .insert("delete".to_string(), sample_layout("workspace-delete"));
        let valid = HashSet::from(["keep".to_string()]);
        let pruned = prune_orphaned_layouts(&stored, &valid);
        assert!(pruned.layouts.contains_key("keep"));
        assert!(!pruned.layouts.contains_key("delete"));
    }

    #[test]
    fn split_ratio_uses_saved_proportions_not_absolute_size() {
        let first = TerminalSize {
            rows: 40,
            cols: 30,
            pixel_width: 0,
            pixel_height: 0,
            dpi: 96,
        };
        let second = TerminalSize {
            rows: 40,
            cols: 70,
            pixel_width: 0,
            pixel_height: 0,
            dpi: 96,
        };
        assert_eq!(
            split_second_percent(SplitDirection::Horizontal, first, second),
            70
        );
    }

    #[test]
    fn output_notifications_do_not_schedule_layout_persistence() {
        assert!(!notification_changes_layout(&MuxNotification::PaneOutput(
            7
        )));
        assert!(notification_changes_layout(&MuxNotification::TabResized(9)));
        assert!(notification_changes_layout(&MuxNotification::PaneFocused(
            7
        )));
    }

    fn pane_entry(pane_id: PaneId, size: TerminalSize, active: bool) -> PaneEntry {
        PaneEntry {
            window_id: 1,
            tab_id: 2,
            pane_id,
            title: "shell".to_string(),
            size,
            working_dir: None,
            is_active_pane: active,
            is_zoomed_pane: false,
            alt_screen: false,
            workspace: "workspace-1".to_string(),
            cursor_pos: StableCursorPosition::default(),
            physical_top: 0,
            top_row: 0,
            left_col: 0,
            tty_name: None,
        }
    }

    #[test]
    fn failed_restore_protection_ignores_focus_and_size_but_not_structure() {
        let size_a = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 600,
            dpi: 96,
        };
        let size_b = TerminalSize {
            rows: 60,
            cols: 160,
            pixel_width: 1600,
            pixel_height: 1200,
            dpi: 144,
        };
        let one = StoredThreadLayout {
            workspace: "workspace-1".to_string(),
            active_tab: 0,
            tabs: vec![serde_json::to_value(PaneNode::Leaf(pane_entry(7, size_a, true))).unwrap()],
            terminal_specs: vec![],
        };
        let resized = StoredThreadLayout {
            workspace: "workspace-1".to_string(),
            active_tab: 0,
            tabs: vec![serde_json::to_value(PaneNode::Leaf(pane_entry(7, size_b, false))).unwrap()],
            terminal_specs: vec![],
        };
        let extra_tab = StoredThreadLayout {
            workspace: "workspace-1".to_string(),
            active_tab: 1,
            tabs: vec![
                serde_json::to_value(PaneNode::Leaf(pane_entry(7, size_b, false))).unwrap(),
                serde_json::to_value(PaneNode::Leaf(pane_entry(8, size_b, true))).unwrap(),
            ],
            terminal_specs: vec![],
        };
        assert_eq!(
            layout_structure_fingerprint(&one),
            layout_structure_fingerprint(&resized)
        );
        assert_ne!(
            layout_structure_fingerprint(&one),
            layout_structure_fingerprint(&extra_tab)
        );
    }

    #[test]
    fn multiple_workspace_windows_are_merged_in_stable_order() {
        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
            dpi: 96,
        };
        let tab = |pane_id| {
            serde_json::to_value(PaneNode::Leaf(pane_entry(pane_id, size, false))).unwrap()
        };
        let spec = |pane_id| StoredTerminalSpec {
            pane_id,
            cwd: None,
            domain: Some("local".to_string()),
            title: "shell".to_string(),
            label: None,
            page: None,
        };
        let merged = merge_workspace_snapshots(
            "workspace-1",
            vec![
                (1, vec![tab(10), tab(11)], vec![spec(10), spec(11)]),
                (0, vec![tab(20)], vec![spec(20)]),
            ],
        )
        .unwrap();

        assert_eq!(merged.active_tab, 1);
        assert_eq!(merged.tabs.len(), 3);
        assert_eq!(merged.terminal_specs.len(), 3);
        let panes = merged
            .tabs
            .iter()
            .map(
                |tab| match serde_json::from_value::<PaneNode>(tab.clone()).unwrap() {
                    PaneNode::Leaf(entry) => entry.pane_id,
                    other => panic!("unexpected tab node: {:?}", other),
                },
            )
            .collect::<Vec<_>>();
        assert_eq!(panes, vec![10, 11, 20]);
    }

    #[test]
    fn unreadable_pane_tree_rejects_restore_without_mutating_the_snapshot() {
        let mut layout = sample_layout("workspace-1");
        layout.tabs = vec![serde_json::json!({"future_node": {"field": true}})];
        let before = layout.clone();
        assert!(decode_layout_tabs(&layout).is_err());
        assert_eq!(layout, before);
    }
}
