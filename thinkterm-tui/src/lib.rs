mod action;
mod backend;
mod clipboard;
mod model;
mod render;
mod settings;
mod state;
mod view;

use action::{Action, DestructiveAction, SplitAxis};
use anyhow::{Context, Result};
use async_channel::{Receiver, Sender};
use backend::TermwizBackend;
use clipboard::{ClipboardProvider, SystemClipboard};
use codec::{ClientViewport, ThinkTermSessionState};
use config::keyassignment::{PaneDirection, SpawnTabDomain};
use config::{ConfigHandle, SshMultiplexing};
use model::{AppModel, ThreadKey, TreeNodeKey};
use mux::client::ClientId;
use mux::connui::ConnectionUI;
use mux::domain::{Domain, DomainState, SplitSource};
use mux::pane::{Pane, PaneId, Pattern};
use mux::tab::{SplitDirection, SplitRequest, SplitSize, Tab, TabId};
use mux::{Mux, MuxNotification};
use ratatui::backend::Backend;
use ratatui::Terminal;
use settings::{TuiConfig, TuiPersistentState};
use state::{
    AppMode, ConfirmationState, ConnectionItem, ConnectionStatus, ContextMenuState, CopyState,
    MenuEntry, PendingOperation, PromptAction, PromptState, SearchState, SelectionPoint,
    TextSelection, UiState, ViewClass,
};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use termwiz::caps::{Capabilities, ProbeHints};
use termwiz::input::{
    InputEvent, KeyCode, KeyEvent, Modifiers, MouseButtons, MouseEvent as TermwizMouseEvent,
};
use termwiz::terminal::{new_terminal, ScreenSize, TerminalWaker};
use uuid::Uuid;
use view::{HitTarget, PaneTool, TabBarControl, TreeAction, ViewLayout};
use wezterm_client::client::Client;
use wezterm_client::domain::{
    ClientDomain, ClientDomainConfig, FrontendRecoverySlot, RemoteFrontendGate,
};
use wezterm_client::pane::ClientPane;
use wezterm_term::input::{MouseButton, MouseEvent, MouseEventKind};
use wezterm_term::TerminalSize;

static EVENT_SENDER: OnceLock<Sender<AppEvent>> = OnceLock::new();
static TERMINAL_WAKER: OnceLock<TerminalWaker> = OnceLock::new();
const INPUT_PROGRESS_WINDOW: Duration = Duration::from_millis(50);
const INPUT_PROGRESS_POLL_INTERVAL: Duration = Duration::from_millis(2);
/// How still the terminal has to be before its size is worth telling the server
/// about. Long enough to swallow a pinch-zoom's intermediate steps, short
/// enough that a deliberate resize still feels immediate.
const RESIZE_SETTLE: Duration = Duration::from_millis(150);
/// How often to ask the panes for new content when nothing has asked to draw.
///
/// A remote pane only asks the server from inside `get_changed_since`, and
/// `RenderableInner::poll` doubles its own interval every time it runs — up to
/// half a minute — resetting only when a response lands. Until now the only
/// caller was `paint_pane`, so a renderer that draws strictly on demand stopped
/// asking, and the screen sat still until something was tapped.
///
/// This asks on a timer instead, and deliberately does *not* draw: content that
/// actually arrives notifies the mux, which marks the frame dirty by the normal
/// route. Drawing on the timer as well was measured at 88% of a core and
/// 3.4 KB/s to the tty on a screen with nothing happening — a cost paid over
/// SSH from a phone, which is the whole audience.
const PANE_POLL_INTERVAL: Duration = Duration::from_millis(150);

/// How long to keep drawing after a resize so the grid the server owes us has a
/// chance to arrive. Comfortably longer than the settle plus a couple of round
/// trips, and bounded so an idle terminal goes back to costing nothing.
const RESIZE_REDRAW_WINDOW: Duration = Duration::from_millis(1500);
/// Pace of those extra draws. Fast enough to look immediate, slow enough that
/// the prefetch throttle refills between attempts.
const RESIZE_REDRAW_INTERVAL: Duration = Duration::from_millis(60);
const TAKEOVER_RESYNC_RETRY_DELAY: Duration = Duration::from_millis(250);
/// Full-screen programs redraw after SIGWINCH asynchronously. Keep the
/// takeover surface opaque until the acknowledged grid and all of its visible
/// rows have stayed ready long enough for that redraw to land.
const TAKEOVER_GEOMETRY_SETTLE: Duration = Duration::from_millis(200);

#[derive(Default)]
struct TakeoverEpochs {
    pending: HashMap<(String, TabId), u64>,
    next: u64,
}

impl TakeoverEpochs {
    fn begin(&mut self, domain_name: &str, tab_id: TabId) -> u64 {
        self.next = self.next.wrapping_add(1).max(1);
        let epoch = self.next;
        self.pending
            .insert((domain_name.to_string(), tab_id), epoch);
        epoch
    }

    fn contains(&self, domain_name: &str, tab_id: TabId) -> bool {
        self.pending
            .contains_key(&(domain_name.to_string(), tab_id))
    }

    fn current(&self, domain_name: &str, tab_id: TabId) -> Option<u64> {
        self.pending
            .get(&(domain_name.to_string(), tab_id))
            .copied()
    }

    fn finish(&mut self, domain_name: &str, tab_id: TabId, epoch: u64) -> bool {
        let key = (domain_name.to_string(), tab_id);
        if self.pending.get(&key) != Some(&epoch) {
            return false;
        }
        self.pending.remove(&key);
        true
    }

    fn clear_domain(&mut self, domain_name: &str) {
        self.pending.retain(|(domain, _), _| domain != domain_name);
    }
}

#[derive(Clone, Debug)]
struct TakeoverGeometryConfirmation {
    epoch: u64,
    root_size: TerminalSize,
    panes: Vec<(PaneId, TerminalSize)>,
    access_generation: u64,
    ready_since: Option<Instant>,
}

fn takeover_geometry_settled(ready: bool, now: Instant, ready_since: &mut Option<Instant>) -> bool {
    if !ready {
        *ready_since = None;
        return false;
    }
    let started = ready_since.get_or_insert(now);
    now.duration_since(*started) >= TAKEOVER_GEOMETRY_SETTLE
}

fn effective_frontend_gate(
    takeover_pending: bool,
    remote_gate: RemoteFrontendGate,
) -> RemoteFrontendGate {
    if takeover_pending {
        RemoteFrontendGate::Syncing
    } else {
        remote_gate
    }
}

fn handoff_geometry_is_pending(
    remote_gate: &RemoteFrontendGate,
    access: Option<&codec::FrontendAccessState>,
    owns: Option<bool>,
    ready_generation: Option<u64>,
) -> bool {
    matches!(remote_gate, RemoteFrontendGate::Visible)
        && owns == Some(true)
        && access
            .filter(|access| access.mode == codec::FrontendAccessMode::Handoff)
            .is_some_and(|access| ready_generation != Some(access.generation))
}

#[derive(Debug, Clone)]
pub struct TuiOptions {
    /// Configured mux client-domain names. If empty, the standard Local mux
    /// resolver is used and other servers remain available in Connections.
    pub domains: Vec<String>,
    /// Window class used by the standard GUI-socket discovery resolver.
    pub class_name: String,
    /// Independent TUI presentation settings. ThinkTerm's Lua config remains
    /// the shared source for connections, shells and terminal behavior.
    pub tui_config_path: Option<PathBuf>,
}

impl Default for TuiOptions {
    fn default() -> Self {
        Self {
            domains: Vec::new(),
            class_name: "com.roversx.thinkterm".to_string(),
            tui_config_path: None,
        }
    }
}

enum AppEvent {
    Mux(MuxNotification),
    Session {
        domain_name: String,
        connection_generation: u64,
        state: ThinkTermSessionState,
    },
    TreeChanged(String),
    Connected {
        domain_name: String,
        connection_generation: u64,
    },
}

/// Whether a wake has been delivered that the loop has not yet acted on.
static WAKE_PENDING: AtomicBool = AtomicBool::new(false);

/// Tell the render loop that something is waiting for it.
///
/// A wake carries no information beyond "look again", so repeating it before
/// the loop has looked buys nothing and costs a write plus a full trip through
/// `poll_input`. Attaching to a busy mux says it more than a thousand times a
/// second — every fetched line is a notification — and the loop spends that
/// second answering the doorbell instead of drawing. Collapsing the repeats
/// leaves exactly one pending wake, which is all the loop can use anyway.
fn wake_terminal() {
    if WAKE_PENDING.swap(true, Ordering::AcqRel) {
        return;
    }
    if let Some(waker) = TERMINAL_WAKER.get() {
        let _ = waker.wake();
    }
}

fn send_event(event: AppEvent) {
    if let Some(sender) = EVENT_SENDER.get() {
        let _ = sender.try_send(event);
    }
    wake_terminal();
}

fn receive_session_state(
    domain_name: &str,
    connection_generation: u64,
    state: ThinkTermSessionState,
) {
    send_event(AppEvent::Session {
        domain_name: domain_name.to_string(),
        connection_generation,
        state,
    });
}

fn receive_tree(domain_name: &str, _tree: codec::ThinkTermTree) {
    send_event(AppEvent::TreeChanged(domain_name.to_string()));
}

fn receive_connection(domain_name: &str, connection_generation: u64) {
    send_event(AppEvent::Connected {
        domain_name: domain_name.to_string(),
        connection_generation,
    });
}

/// Run the terminal UI inside the already initialized `thinkterm` process.
/// This function deliberately does not initialize config, install a second
/// executor or shut down the mux; those lifetimes belong to the host binary.
/// Sends this process's stderr to a file for as long as the TUI owns the screen.
///
/// The logger writes every line to stderr *and* to its own file, and here stderr
/// is the screen being drawn on — so a reconnect, a dropped domain, or any
/// warning paints itself over the interface, which is what every screenshot of
/// this program has had scribbled across it. Nothing is lost by redirecting:
/// the same lines are already going to the log file. Restoring on the way out
/// matters just as much, because an error returned from here is printed by the
/// caller, and that has to land on the stderr the reader can actually see.
#[cfg(unix)]
struct StderrToLogFile(Option<filedescriptor::FileDescriptor>);

#[cfg(unix)]
impl StderrToLogFile {
    fn install() -> Self {
        use filedescriptor::FileDescriptor;
        use std::os::fd::{AsRawFd, IntoRawFd};
        let path =
            config::RUNTIME_DIR.join(format!("thinkterm-tui-stderr-{}.txt", std::process::id()));
        let Ok(file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        else {
            return Self(None);
        };
        let stderr = std::io::stderr();
        let Ok(saved) = FileDescriptor::dup(&stderr.as_raw_fd()) else {
            return Self(None);
        };
        // `dup2` hands back an owning handle to the descriptor it just wrote
        // to. Dropping that handle would close the process's stderr outright —
        // which looks like success, because nothing can be written over the
        // screen any more, and is not, because nothing can be written anywhere.
        // Releasing it leaves descriptor 2 open and pointing at the file.
        match unsafe { FileDescriptor::dup2(&file.as_raw_fd(), stderr.as_raw_fd()) } {
            Ok(installed) => {
                let _ = installed.into_raw_fd();
                Self(Some(saved))
            }
            Err(_) => Self(None),
        }
    }
}

#[cfg(unix)]
impl Drop for StderrToLogFile {
    fn drop(&mut self) {
        use filedescriptor::FileDescriptor;
        use std::os::fd::{AsRawFd, IntoRawFd};
        if let Some(saved) = self.0.take() {
            if let Ok(restored) =
                unsafe { FileDescriptor::dup2(&saved.as_raw_fd(), std::io::stderr().as_raw_fd()) }
            {
                let _ = restored.into_raw_fd();
            }
        }
    }
}

#[cfg(not(unix))]
struct StderrToLogFile;

#[cfg(not(unix))]
impl StderrToLogFile {
    fn install() -> Self {
        Self
    }
}

pub fn run(config: ConfigHandle, options: TuiOptions) -> Result<()> {
    let _stderr = StderrToLogFile::install();
    let executor = promise::spawn::SimpleExecutor::new();
    let (result_tx, result_rx) = std::sync::mpsc::sync_channel(1);
    promise::spawn::spawn(async move {
        let _ = result_tx.send(run_async(config, options).await);
    })
    .detach();
    loop {
        match result_rx.try_recv() {
            Ok(result) => return result,
            Err(std::sync::mpsc::TryRecvError::Empty) => executor.tick()?,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                anyhow::bail!("TUI executor stopped without a result")
            }
        }
    }
}

async fn fetch_current_session(domain: &ClientDomain) -> Result<(u64, ThinkTermSessionState)> {
    if domain.state() != DomainState::Attached {
        anyhow::bail!("domain is not attached");
    }
    let before = domain
        .connection_generation()
        .filter(|generation| *generation != 0)
        .context("domain has no live connection generation")?;
    let snapshot = domain.fetch_thinkterm_session_state().await?;
    let after = domain.connection_generation();
    if domain.state() != DomainState::Attached || after != Some(before) {
        anyhow::bail!("connection changed while fetching the session snapshot");
    }
    Ok((before, snapshot))
}

async fn run_async(config: ConfigHandle, options: TuiOptions) -> Result<()> {
    let tui_config_path = settings::config_path(options.tui_config_path.clone());
    let tui_config = settings::load_config(&tui_config_path)?;
    settings::set_active_theme(tui_config.theme);
    let tui_state_path = settings::state_path();
    let persisted = settings::load_state(&tui_state_path).unwrap_or_else(|err| {
        log::warn!("failed to load TUI state: {err:#}");
        TuiPersistentState::default()
    });
    let catalog = all_client_domain_configs(&config, &options.class_name)?;
    let available = catalog.domains;
    if available.is_empty() {
        anyhow::bail!("no mux client domains or multiplexed ThinkTerm SSH hosts are configured");
    }
    let configs = select_domain_configs(&available, &options.domains, &catalog.local_domain_name)?;
    let has_initial_configs = !configs.is_empty();
    let domain_configs = available
        .iter()
        .cloned()
        .map(|domain| (domain.name().to_string(), domain))
        .collect::<BTreeMap<_, _>>();
    let connection_items = connection_catalog(&available, &catalog.local_domain_name);
    let mux = Arc::new(Mux::new(None));
    Mux::set_mux(&mux);
    let client_id = Arc::new(ClientId::new());
    mux.register_client(Arc::clone(&client_id));
    mux.replace_identity(Some(client_id));

    let (sender, receiver) = async_channel::unbounded();
    EVENT_SENDER
        .set(sender.clone())
        .map_err(|_| anyhow::anyhow!("the TUI event channel was already initialized"))?;
    wezterm_client::domain::set_thinkterm_session_sink(receive_session_state);
    wezterm_client::domain::set_thinkterm_tree_sink(receive_tree);
    wezterm_client::domain::set_thinkterm_connect_sink(receive_connection);
    wezterm_client::domain::set_thinkterm_frontend_wake_sink(wake_terminal);
    mux.subscribe(move |notification| {
        let _ = sender.try_send(AppEvent::Mux(notification));
        wake_terminal();
        true
    });

    let mut domains = BTreeMap::new();
    for domain_config in configs {
        let name = domain_config.name().to_string();
        let domain = Arc::new(ClientDomain::new(domain_config));
        let mux_domain: Arc<dyn Domain> = domain.clone();
        mux.add_domain(&mux_domain);
        domains.insert(name, domain);
    }
    if let Some(domain) = domains.values().next() {
        let mux_domain: Arc<dyn Domain> = domain.clone();
        mux.set_default_domain(&mux_domain);
    }

    let mut model = AppModel::default();
    let mut attach_failures = Vec::new();
    let domain_names: Vec<_> = domains.keys().cloned().collect();
    for name in domain_names {
        let domain = Arc::clone(&domains[&name]);
        match domain
            .attach_with_ui(None, ConnectionUI::new_headless())
            .await
            .with_context(|| format!("attaching to mux domain {name}"))
        {
            Ok(()) => {
                if let Err(err) = guard_against_same_server_nesting(&domain) {
                    domain.perform_detach();
                    attach_failures.push(format!("{name}: {err:#}"));
                    continue;
                }
                match fetch_current_session(&domain).await {
                    Ok((_generation, snapshot)) => {
                        model.apply_snapshot(name.clone(), snapshot);
                    }
                    Err(err) => {
                        domain.perform_detach();
                        attach_failures.push(format!("{name}: session snapshot: {err:#}"));
                    }
                }
            }
            Err(err) => attach_failures.push(format!("{name}: {err:#}")),
        }
    }
    domains.retain(|_, domain| domain.state() == DomainState::Attached);
    restore_persisted_selection(&mut model, &persisted);

    let initial_status = if !has_initial_configs {
        "Choose a mux server to connect".to_string()
    } else if attach_failures.is_empty() {
        String::new()
    } else {
        format!("Some servers failed: {}", attach_failures.join("; "))
    };
    let result = run_terminal(
        domain_configs,
        connection_items,
        domains.clone(),
        receiver,
        model,
        initial_status,
        tui_config,
        tui_config_path,
        tui_state_path,
    )
    .await;
    for domain in domains.values() {
        domain.perform_detach();
    }
    result
}

struct DomainCatalog {
    domains: Vec<ClientDomainConfig>,
    local_domain_name: String,
}

fn all_client_domain_configs(config: &ConfigHandle, class_name: &str) -> Result<DomainCatalog> {
    // Keep the default local entry on exactly the same resolver as `thinkterm
    // cli`: an explicit environment socket wins, then a published GUI socket,
    // and only then the configured daemon socket.  The resolver's synthetic
    // domains have an empty name because the ordinary CLI does not need one;
    // the TUI does, so preserve the configured default name (normally `unix`).
    let mut local = Client::resolve_default_unix_domain(false, class_name)?;
    let local_domain_name = config
        .unix_domains
        .first()
        .map(|domain| domain.name.clone())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "unix".to_string());
    local.name = local_domain_name.clone();

    let mut domains = vec![ClientDomainConfig::Unix(local)];
    domains.extend(
        config
            .unix_domains
            .iter()
            .skip(1)
            .cloned()
            .map(ClientDomainConfig::Unix),
    );
    domains.extend(
        config
            .ssh_domains()
            .into_iter()
            .filter(|domain| domain.multiplexing == SshMultiplexing::WezTerm)
            .map(ClientDomainConfig::Ssh),
    );
    domains.extend(
        config
            .tls_clients
            .iter()
            .cloned()
            .map(ClientDomainConfig::Tls),
    );
    match thinkterm_core::ssh_hosts::list_all_hosts() {
        Ok(entries) => {
            let mut existing = domains
                .iter()
                .map(|domain| domain.name().to_string())
                .collect::<HashSet<_>>();
            for entry in entries {
                if !entry.spec.multiplexing || entry.spec.use_mosh {
                    continue;
                }
                let mut ssh = thinkterm_core::ssh_hosts::build_ssh_domain(&entry.spec);
                ssh.name = entry.spec.label.clone();
                ssh.multiplexing = SshMultiplexing::WezTerm;
                ssh.stored_password = entry
                    .spec
                    .password
                    .as_deref()
                    .map(thinkterm_core::secret::reveal)
                    .filter(|password| !password.is_empty());
                if existing.insert(ssh.name.clone()) {
                    domains.push(ClientDomainConfig::Ssh(ssh));
                }
            }
        }
        Err(err) => log::warn!("failed to read saved ThinkTerm SSH hosts: {err:#}"),
    }
    Ok(DomainCatalog {
        domains,
        local_domain_name,
    })
}

fn select_domain_configs(
    available: &[ClientDomainConfig],
    requested: &[String],
    local_domain_name: &str,
) -> Result<Vec<ClientDomainConfig>> {
    let names = || {
        available
            .iter()
            .map(|domain| domain.name())
            .collect::<Vec<_>>()
            .join(", ")
    };
    if !requested.is_empty() {
        let mut selected = Vec::new();
        let mut seen = HashSet::new();
        for name in requested {
            if !seen.insert(name) {
                continue;
            }
            let domain = available
                .iter()
                .find(|domain| domain.name() == name)
                .cloned()
                .with_context(|| {
                    format!(
                        "no mux client domain named {name:?}; available: {}",
                        names()
                    )
                })?;
            selected.push(domain);
        }
        return Ok(selected);
    }

    // `thinkterm tui` is a standalone local frontend by default. Remote
    // servers are explicit (`thinkterm tui DOMAIN`) or selected later from
    // Connections; silently following GUI auto-connect settings makes startup
    // surprising and can land on a remote host with no local terminal.
    if let Some(local) = available
        .iter()
        .find(|domain| domain.name() == local_domain_name)
    {
        return Ok(vec![local.clone()]);
    }
    if available.len() == 1 {
        return Ok(available.to_vec());
    }
    if available.is_empty() {
        anyhow::bail!("no mux client domains are configured")
    }
    // An interactive frontend can resolve ambiguity itself; start on the
    // connection chooser rather than rejecting `thinkterm tui`.
    Ok(vec![])
}

fn connection_catalog(
    available: &[ClientDomainConfig],
    local_domain_name: &str,
) -> Vec<ConnectionItem> {
    let mut items = available
        .iter()
        .map(|domain| ConnectionItem {
            name: domain.name().to_string(),
            label: if matches!(domain, ClientDomainConfig::Unix(_))
                && domain.name() == local_domain_name
            {
                "Local".to_string()
            } else {
                domain.name().to_string()
            },
            detail: if matches!(domain, ClientDomainConfig::Unix(_))
                && domain.name() == local_domain_name
            {
                "Current ThinkTerm GUI or local mux".to_string()
            } else {
                domain.label()
            },
            status: ConnectionStatus::Disconnected,
            connectable: true,
        })
        .collect::<Vec<_>>();
    if let Ok(hosts) = thinkterm_core::ssh_hosts::list_all_hosts() {
        let known = items
            .iter()
            .map(|item| item.name.clone())
            .collect::<HashSet<_>>();
        for host in hosts {
            if host.spec.multiplexing && !host.spec.use_mosh {
                continue;
            }
            if known.contains(&host.spec.label) {
                continue;
            }
            items.push(ConnectionItem {
                name: host.id,
                label: host.spec.label,
                detail: if host.spec.use_mosh {
                    "Mosh sessions are not mux servers; edit in GUI".into()
                } else {
                    "Enable ThinkTerm Connect in SSH Hosts".into()
                },
                status: ConnectionStatus::Unsupported,
                connectable: false,
            });
        }
    }
    items.sort_by(|a, b| {
        a.label
            .to_ascii_lowercase()
            .cmp(&b.label.to_ascii_lowercase())
    });
    items
}

fn guard_against_same_server_nesting(domain: &ClientDomain) -> Result<()> {
    let origin = std::env::var("THINKTERM_MUX_SERVER_ID").ok();
    let target = domain
        .remote_server_id()
        .context("the target server did not report its runtime identity")?;
    check_nesting(
        origin.as_deref(),
        std::env::var_os("WEZTERM_PANE").is_some(),
        &target,
    )
}

fn check_nesting(origin: Option<&str>, inside_thinkterm_pane: bool, target: &str) -> Result<()> {
    if origin == Some(target) {
        anyhow::bail!("refusing to run ThinkTerm TUI inside a pane owned by the same mux server");
    }
    if inside_thinkterm_pane && origin.is_none() {
        anyhow::bail!(
            "refusing nested TUI: this pane predates server identity support, so a different target cannot be proven"
        );
    }
    Ok(())
}

struct TuiState {
    model: AppModel,
    domain_configs: BTreeMap<String, ClientDomainConfig>,
    domains: BTreeMap<String, Arc<ClientDomain>>,
    ui: UiState,
    dirty: bool,
    layout: ViewLayout,
    last_mouse_buttons: MouseButtons,
    /// Each tab's last advertised viewport, and — through the key alone —
    /// whether this renderer has advertised one at all. Forgetting it re-sends it
    /// on the next frame; the key has to outlive that, because taking control
    /// of a tab requires the server to already hold a viewport for us.
    /// Also carries whether this renderer owned the viewport when it last said
    /// so: a handover changes what the server does with the same numbers, so it
    /// has to count as something new to say.
    last_viewports: HashMap<(String, TabId), Option<(ClientViewport, bool)>>,
    /// B grants visibility for the whole mux, but geometry is still installed
    /// one top-level tab at a time. A tab is safe to reveal only after this TUI
    /// has acknowledged a complete viewport in the current access generation.
    handoff_geometry_ready: HashMap<(String, TabId), u64>,
    /// Keep the release paired with a press on B's takeover surface from
    /// reaching the pane if the claim round-trip completed very quickly.
    handoff_consumed_press: bool,
    takeover_epochs: TakeoverEpochs,
    /// A successful ownership RPC is only the start of a handoff. Keep the
    /// locally previewed pane surfaces pinned until the server has echoed the
    /// same geometry and every visible row has been fetched.
    takeover_geometry_confirmations: HashMap<(String, TabId), TakeoverGeometryConfirmation>,
    /// When the panes were last asked for new content, so the loop keeps asking
    /// even while nothing local has changed.
    last_pane_poll: Instant,
    /// The view last offered to the server, and when, so a flick of the wheel
    /// does not become a round trip per notch.
    shared_views: HashMap<(String, TabId), codec::ClientView>,
    shared_view_at: HashMap<(String, TabId), Instant>,
    /// A view change arrived inside the coalescing window and still has to go
    /// out once that window closes.
    share_view_pending: HashSet<(String, TabId)>,
    /// The view last adopted from the owner, so following it is idempotent and
    /// does not fight this renderer's own scrolling every frame.
    followed_views: HashMap<(String, TabId), codec::ClientView>,
    /// Latest physical cell/pixel information from the TTY. Takeover actions
    /// run between frames, so they use this to build the same native viewport
    /// the most recent frame used.
    screen_size: Option<ScreenSize>,
    /// When the terminal last changed size, plus the time it has to stay still
    /// before that size is worth forwarding. `None` once it has been forwarded.
    resize_settles_at: Option<Instant>,
    /// Deadline until which to keep drawing regardless of whether anything
    /// asked for it, so a pane whose refetch was throttled away still recovers.
    redraw_until: Option<Instant>,
    /// Current transport generation accepted for each domain. Session pushes
    /// from any other generation are stale even when the restarted server has
    /// the same runtime id and a numerically newer snapshot generation.
    connection_generations: HashMap<String, u64>,
    refresh_sessions: HashSet<String>,
    actions: VecDeque<Action>,
    clipboard: Arc<dyn ClipboardProvider>,
    drag: Option<DragState>,
    /// Pane that owns an in-progress mouse sequence passed through to an
    /// application. Releases must go to the same pane even outside its rect.
    forwarded_mouse_pane: Option<PaneId>,
    settings: TuiConfig,
    settings_path: PathBuf,
    state_path: PathBuf,
    animation_epoch: Instant,
}

#[derive(Clone, Debug)]
enum DragState {
    Selection {
        pane_id: usize,
    },
    TreeNode {
        key: TreeNodeKey,
    },
    SidebarResize,
    Scrollbar {
        pane_id: PaneId,
    },
    SplitResize {
        split_index: usize,
        last_x: u16,
        last_y: u16,
    },
}

impl TuiState {
    fn new(
        domain_configs: BTreeMap<String, ClientDomainConfig>,
        connections: Vec<ConnectionItem>,
        domains: BTreeMap<String, Arc<ClientDomain>>,
        model: AppModel,
        status: String,
        settings: TuiConfig,
        settings_path: PathBuf,
        state_path: PathBuf,
    ) -> Self {
        let no_attached_domain = domains.is_empty();
        let connection_generations = domains
            .iter()
            .filter_map(|(name, domain)| {
                domain
                    .connection_generation()
                    .filter(|generation| *generation != 0)
                    .map(|generation| (name.clone(), generation))
            })
            .collect();
        let mut state = Self {
            model,
            domain_configs,
            domains,
            ui: {
                let mut ui = UiState::new(status);
                ui.sidebar_visible = settings.sidebar_visible;
                ui.sidebar_width = settings.sidebar_width;
                ui.narrow_width = settings.narrow_width;
                ui.touch_targets = settings.touch_targets;
                ui.pane_scrollbars = settings.pane_scrollbars;
                ui.pane_borders = settings.pane_borders;
                ui.pane_nav_bar = settings.pane_nav_bar;
                ui.connections = connections;
                if no_attached_domain {
                    ui.mode = AppMode::Connections;
                }
                ui
            },
            dirty: true,
            layout: ViewLayout::default(),
            last_mouse_buttons: MouseButtons::NONE,
            last_viewports: HashMap::new(),
            handoff_geometry_ready: HashMap::new(),
            handoff_consumed_press: false,
            takeover_epochs: TakeoverEpochs::default(),
            takeover_geometry_confirmations: HashMap::new(),
            last_pane_poll: Instant::now(),
            shared_views: HashMap::new(),
            shared_view_at: HashMap::new(),
            share_view_pending: HashSet::new(),
            followed_views: HashMap::new(),
            screen_size: None,
            resize_settles_at: None,
            redraw_until: None,
            connection_generations,
            refresh_sessions: HashSet::new(),
            actions: VecDeque::new(),
            clipboard: Arc::new(SystemClipboard),
            drag: None,
            forwarded_mouse_pane: None,
            settings,
            settings_path,
            state_path,
            animation_epoch: Instant::now(),
        };
        sync_connection_statuses(&mut state);
        state
    }

    fn active_tab(&self) -> Option<(String, Arc<ClientDomain>, Arc<Tab>, TabId)> {
        let remote_tab_id = self.model.selected_tab()?.tab_id;
        let row = self.model.selected_row()?;
        let domain_name = row.key.domain_name.clone();
        let domain = Arc::clone(self.domains.get(&domain_name)?);
        let local_id = domain.remote_to_local_tab_id(remote_tab_id)?;
        let tab = Mux::get().get_tab(local_id)?;
        Some((domain_name, domain, tab, remote_tab_id))
    }

    fn active_view_cache_key(&self) -> Option<(String, TabId)> {
        let remote_tab_id = self.model.selected_tab()?.tab_id;
        let domain_name = self.model.selected_row()?.key.domain_name.clone();
        Some((domain_name, remote_tab_id))
    }

    fn active_pane(&self) -> Option<Arc<dyn Pane>> {
        self.active_tab()?.2.get_active_pane()
    }

    fn clear_selected_viewport_cache(&mut self) {
        let Some(domain_name) = self
            .model
            .selected_row()
            .map(|row| row.key.domain_name.clone())
        else {
            return;
        };
        for ((domain, _), size) in self.last_viewports.iter_mut() {
            if domain == &domain_name {
                *size = None;
            }
        }
    }

    /// False only when the server has told us another renderer holds the lease.
    fn owns_active_viewport(&self) -> bool {
        let Some((_, domain, tab, _)) = self.active_tab() else {
            return true;
        };
        domain.owns_remote_viewport(tab.tab_id()) != Some(false)
    }

    fn active_frontend_access(&self) -> Option<codec::FrontendAccessState> {
        let (_, domain, _, _) = self.active_tab()?;
        domain.remote_access_state()
    }

    fn frontend_gate(&self) -> RemoteFrontendGate {
        let Some((domain_name, domain, tab, remote_tab_id)) = self.active_tab() else {
            return RemoteFrontendGate::Visible;
        };
        let remote_gate = domain.remote_frontend_gate();
        let access = domain.remote_access_state();
        let handoff_geometry_pending = handoff_geometry_is_pending(
            &remote_gate,
            access.as_ref(),
            domain.owns_remote_viewport(tab.tab_id()),
            self.handoff_geometry_ready
                .get(&(domain_name.clone(), remote_tab_id))
                .copied(),
        );
        effective_frontend_gate(
            self.takeover_epochs.contains(&domain_name, remote_tab_id) || handoff_geometry_pending,
            remote_gate,
        )
    }

    /// Start (or reuse) the geometry epoch that keeps an automatically granted
    /// B-mode owner opaque. Explicit claims create their epoch before sending
    /// the RPC; this covers the first renderer, which becomes owner through an
    /// ordinary viewport report instead.
    fn ensure_active_handoff_geometry_epoch(&mut self) -> Option<u64> {
        let (domain_name, domain, tab, remote_tab_id) = self.active_tab()?;
        let access = domain.remote_access_state()?;
        if access.mode != codec::FrontendAccessMode::Handoff
            || domain.owns_remote_viewport(tab.tab_id()) != Some(true)
            || self
                .handoff_geometry_ready
                .get(&(domain_name.clone(), remote_tab_id))
                == Some(&access.generation)
        {
            return None;
        }
        if let Some(epoch) = self.takeover_epochs.current(&domain_name, remote_tab_id) {
            return Some(epoch);
        }
        Some(self.takeover_epochs.begin(&domain_name, remote_tab_id))
    }

    fn frontend_surface_blocked(&self) -> bool {
        self.frontend_gate().obscures_terminal()
    }

    fn frontend_takeover_claimable(&self) -> bool {
        self.frontend_gate().is_claimable()
    }

    fn frontend_overlay_message(&self) -> Option<(String, String)> {
        self.frontend_gate().overlay_message()
    }

    fn shared_grid_is_visible(&self) -> bool {
        self.active_frontend_access()
            .is_some_and(|state| state.mode == codec::FrontendAccessMode::TmuxLatest)
            && !self.owns_active_viewport()
    }

    fn viewport_status(&self) -> Option<String> {
        let (_, domain, tab, _) = self.active_tab()?;
        let access = domain.remote_access_state()?;
        let state = domain.remote_viewport_state(tab.tab_id())?;
        match access.mode {
            codec::FrontendAccessMode::TmuxLatest => {
                if domain.owns_remote_viewport(tab.tab_id()) == Some(false) {
                    Some(format!(
                        "A SHARED · VIEW {}×{}",
                        state.canonical_size.cols, state.canonical_size.rows
                    ))
                } else {
                    Some("A SHARED".to_string())
                }
            }
            codec::FrontendAccessMode::Handoff => {
                if domain.has_remote_access() == Some(true) {
                    Some("B ACTIVE".to_string())
                } else {
                    let owner = access
                        .owner
                        .as_ref()
                        .map(|owner| owner.hostname.as_str())
                        .filter(|hostname| !hostname.trim().is_empty())
                        .unwrap_or("other");
                    Some(format!("B VIEW · {owner}"))
                }
            }
        }
    }

    fn queue(&mut self, action: Action) {
        if action != Action::None {
            self.actions.push_back(action);
        }
    }

    fn pane(&self, pane_id: usize) -> Option<Arc<dyn Pane>> {
        Mux::get().get_pane(pane_id)
    }
}

fn sync_connection_statuses(state: &mut TuiState) {
    for item in &mut state.ui.connections {
        if !item.connectable {
            item.status = ConnectionStatus::Unsupported;
            continue;
        }
        item.status = match state.domains.get(&item.name) {
            Some(domain)
                if domain.state() == DomainState::Attached && domain.is_reconnect_suspended() =>
            {
                ConnectionStatus::Reconnecting
            }
            Some(domain) if domain.state() == DomainState::Attached => ConnectionStatus::Attached,
            Some(domain) if domain.is_attaching() => ConnectionStatus::Connecting,
            Some(domain) if domain.is_reconnect_suspended() => ConnectionStatus::Failed,
            Some(_) => ConnectionStatus::Disconnected,
            None => ConnectionStatus::Disconnected,
        };
    }
    state.ui.connection_index = state
        .ui
        .connection_index
        .min(state.ui.connections.len().saturating_sub(1));
}

async fn run_terminal(
    domain_configs: BTreeMap<String, ClientDomainConfig>,
    connections: Vec<ConnectionItem>,
    domains: BTreeMap<String, Arc<ClientDomain>>,
    receiver: Receiver<AppEvent>,
    model: AppModel,
    initial_status: String,
    settings: TuiConfig,
    settings_path: PathBuf,
    state_path: PathBuf,
) -> Result<()> {
    let caps = tui_terminal_capabilities(ProbeHints::new_from_env())?;
    let backend = TermwizBackend::new(new_terminal(caps).context("opening the terminal")?)?;
    let mut terminal = Terminal::new(backend).context("initializing Ratatui")?;
    let waker = terminal.backend_mut().waker();
    TERMINAL_WAKER
        .set(waker)
        .map_err(|_| anyhow::anyhow!("the TUI terminal was already initialized"))?;
    terminal.clear()?;

    let mut state = TuiState::new(
        domain_configs,
        connections,
        domains,
        model,
        initial_status,
        settings,
        settings_path,
        state_path,
    );
    let area = terminal.size()?;
    // A screen this narrow shows the tree over the whole terminal, so honouring
    // a remembered "visible" opens straight into something covering the thing
    // you came for, every single time. The tree is one tap away on the ≡.
    state.ui.sidebar_visible = opens_with_tree(
        view::classify(area.width, state.ui.narrow_width),
        state.ui.sidebar_visible,
    );
    state.layout = view::compute_view(area.into(), &state.model, &state.ui, None);
    if !state.domains.is_empty() {
        if let Err(err) = ensure_selected_thread_live(&mut state, true, None).await {
            state.ui.pending = None;
            state.ui.status = format!("Opening terminal: {err:#}");
        }
    }
    let mut input_progress_until = None;
    while !state.ui.exit {
        // Cleared before the queue is read, never after: anything that arrives
        // from here on sets it again and rings, so no notification can slip in
        // behind a wake this iteration has already spent.
        WAKE_PENDING.store(false, Ordering::Release);
        drain_events(&receiver, &mut state);
        refresh_sessions(&mut state).await;
        if state.ui.expire_toast(std::time::Instant::now()) {
            state.dirty = true;
        }

        if terminal.backend_mut().check_for_resize()? {
            terminal.resize(terminal.backend().size()?.into())?;
            state.dirty = true;
            state.clear_selected_viewport_cache();
            // Pinching a phone terminal walks through a dozen sizes on the way
            // to the one that was meant. Handing each of them to the pane
            // reflows its contents that many times, and anything a program drew
            // for a width it held for 200ms is destroyed by the next step.
            // Redraw locally at once; tell the server only where it landed.
            state.resize_settles_at = Some(Instant::now() + RESIZE_SETTLE);
            state.redraw_until = Some(Instant::now() + RESIZE_REDRAW_WINDOW);
        }

        // A resize dirties every row at once, which can outrun the client's
        // line-prefetch throttle. A dropped prefetch delivers nothing, so no
        // notification arrives to wake this loop, and `get_lines` — the very
        // call that would retry — only runs while drawing. Left alone, a
        // renderer that draws on demand sits on the pre-resize picture until
        // the next keystroke. Drawing for a moment longer lets it retry.
        match state.redraw_until {
            Some(until) if Instant::now() < until => state.dirty = true,
            Some(_) => state.redraw_until = None,
            None => {}
        }

        if state
            .resize_settles_at
            .is_some_and(|settles_at| Instant::now() >= settles_at)
        {
            state.dirty = true;
        }

        if state.last_pane_poll.elapsed() >= PANE_POLL_INTERVAL {
            poll_panes(&state);
            state.last_pane_poll = Instant::now();
            // The blocked surface is the only TUI view with ambient motion.
            // Piggyback on the existing pane poll instead of adding another
            // timer; normal terminal rendering stays event driven.
            if state.frontend_surface_blocked() {
                state.dirty = true;
            }
        }

        if let Some(cache_key) = state.active_view_cache_key() {
            if state.share_view_pending.contains(&cache_key)
                && share_view_is_due(&state, &cache_key)
            {
                state.dirty = true;
            }
        }

        if state.dirty {
            if let Ok(screen) = terminal.backend_mut().screen_size() {
                state.screen_size = Some(screen);
                let area: ratatui::layout::Rect = terminal.size()?.into();
                let recovery_pending = state.active_tab().is_some_and(|(_, domain, tab, _)| {
                    domain
                        .pending_frontend_recovery(FrontendRecoverySlot::Primary, tab.tab_id())
                        .is_some()
                });
                let preview_epoch = state.ensure_active_handoff_geometry_epoch();
                prepare_active_native_viewport(
                    &mut state,
                    area,
                    screen,
                    recovery_pending,
                    preview_epoch,
                );
            }
            let active = state.active_tab();
            let local_tab = active.as_ref().map(|(_, _, tab, _)| Arc::clone(tab));
            let viewport_status = state.viewport_status();
            let frontend_gate = state.frontend_gate();
            let handoff_message = frontend_gate.overlay_message();
            let handoff_animation =
                render::HandoffAnimationFrame::at(state.animation_epoch.elapsed());
            let shared_grid = state.shared_grid_is_visible();
            let mut rendered = render::RenderResult::default();
            let mut next_layout = ViewLayout::default();
            state.ui.begin_frame();
            terminal.draw(|frame| {
                next_layout =
                    view::compute_view(frame.area(), &state.model, &state.ui, local_tab.as_ref());
                rendered = render::render(
                    frame,
                    &state.model,
                    &state.ui,
                    local_tab.as_ref(),
                    &next_layout,
                    viewport_status.as_deref(),
                    handoff_message.as_ref(),
                    handoff_animation,
                    shared_grid,
                    &state.settings,
                );
            })?;
            state.layout = next_layout;
            // Cleared before the trip to the server, never after — the same
            // rule the wake flag follows, and for the same reason. Adopting the
            // owner's scroll position happens in there, and clearing afterwards
            // would wipe the request to draw it, leaving a follower holding the
            // right position and showing the old one.
            state.dirty = false;
            report_viewport_if_changed(&mut state, rendered.selected_tab, terminal.backend_mut())
                .await;
            advance_takeover_geometry_confirmation(&mut state);
        }

        let now = Instant::now();
        let mut wait = input_progress_wait(state.ui.toast_timeout(now), input_progress_until, now);
        // Nothing external will wake the loop to ask again, so the next ask has
        // to be one of the things the input poll waits on.
        let until_poll =
            PANE_POLL_INTERVAL.saturating_sub(now.saturating_duration_since(state.last_pane_poll));
        wait = Some(wait.map_or(until_poll, |wait| wait.min(until_poll)));
        if let Some(cache_key) = state.active_view_cache_key() {
            if state.share_view_pending.contains(&cache_key) {
                let until_share = state
                    .shared_view_at
                    .get(&cache_key)
                    .map_or(Duration::ZERO, |at| {
                        SHARE_VIEW_INTERVAL.saturating_sub(now.saturating_duration_since(*at))
                    });
                wait = Some(wait.map_or(until_share, |wait| wait.min(until_share)));
            }
        }
        // Nothing else will wake this loop once the terminal stops changing, so
        // the settle deadline has to be one of the things it waits on.
        if let Some(remaining) = state
            .resize_settles_at
            .and_then(|settles_at| settles_at.checked_duration_since(now))
        {
            wait = Some(wait.map_or(remaining, |wait| wait.min(remaining)));
        }
        if let Some(remaining) = state
            .redraw_until
            .and_then(|until| until.checked_duration_since(now))
        {
            let tick = remaining.min(RESIZE_REDRAW_INTERVAL);
            wait = Some(wait.map_or(tick, |wait| wait.min(tick)));
        }
        if let Some(first) = terminal.backend_mut().poll_input(wait)? {
            // Termwiz may decode a single tty read into many key events. Drain
            // that ready queue before drawing so fast typing costs one frame
            // per batch instead of one full Ratatui frame per character.
            let mut next = Some(first);
            let mut had_user_input = false;
            for _ in 0..256 {
                let Some(event) = next.take() else {
                    break;
                };
                had_user_input |= is_user_input(&event);
                handle_input(event, &mut state);
                dispatch_actions(&mut state).await;
                if state.ui.exit {
                    break;
                }
                next = terminal.backend_mut().poll_input(Some(Duration::ZERO))?;
            }
            if had_user_input {
                // Pane input is dispatched through futures scheduled on the
                // same SimpleExecutor as this loop.  Entering an indefinite
                // tty poll immediately after the final key can otherwise
                // starve that send (and its echo) until an unrelated wakeup.
                // Keep the executor moving briefly after real user input,
                // then return to an idle, zero-CPU indefinite poll.
                input_progress_until = Some(Instant::now() + INPUT_PROGRESS_WINDOW);
            }
        }
        smol::future::yield_now().await;
    }
    terminal.clear()?;
    persist_ui_state(&state).ok();
    Ok(())
}

fn is_user_input(event: &InputEvent) -> bool {
    matches!(
        event,
        InputEvent::Key(_)
            | InputEvent::Mouse(_)
            | InputEvent::PixelMouse(_)
            | InputEvent::Paste(_)
    )
}

/// Whether to open with the tree showing.
///
/// A screen narrow enough to draw the tree over the whole terminal opens on the
/// terminal, whatever was remembered: the tree there is a way to get somewhere,
/// and starting inside it means starting on top of the thing you came for. It
/// is one tap away on the ≡.
fn opens_with_tree(class: ViewClass, remembered: bool) -> bool {
    class != ViewClass::Narrow && remembered
}

/// Whether showing or hiding the tree at this width is worth remembering.
///
/// Only where it is a panel beside the terminal. As an overlay it is view
/// state, and writing it to the config makes a phone and a desktop sharing one
/// file fight over a single boolean — whoever toggled last decides how the
/// other one opens.
fn tree_visibility_is_a_preference(class: ViewClass) -> bool {
    class != ViewClass::Narrow
}

fn input_progress_wait(
    ordinary_wait: Option<Duration>,
    progress_until: Option<Instant>,
    now: Instant,
) -> Option<Duration> {
    let Some(remaining) = progress_until
        .and_then(|until| until.checked_duration_since(now))
        .filter(|remaining| !remaining.is_zero())
    else {
        return ordinary_wait;
    };
    let progress_wait = remaining.min(INPUT_PROGRESS_POLL_INTERVAL);
    Some(ordinary_wait.map_or(progress_wait, |wait| wait.min(progress_wait)))
}

fn tui_terminal_capabilities(hints: ProbeHints) -> Result<Capabilities> {
    // Theme selection is an explicit TUI setting and can change while the
    // process is running.  Let the selected theme decide whether to use color;
    // retaining NO_COLOR as a renderer-level capability override would make
    // every named theme silently monochrome.  The two Monochrome themes remain
    // the in-app way to request color-free output.
    Capabilities::new_with_hints(hints.color_level(None)).context("reading terminal capabilities")
}

fn restore_persisted_selection(model: &mut AppModel, state: &TuiPersistentState) {
    let selected = state.last_domain.as_deref().and_then(|domain_name| {
        let thread_id = state.selected_threads.get(domain_name)?;
        model
            .rows()
            .iter()
            .find(|row| row.key.domain_name == domain_name && row.thread.id == *thread_id)
            .map(|row| row.key.clone())
    });
    if let Some(key) = selected {
        model.select_thread(&key);
        let tab_key = format!("{}/{}", key.domain_name, key.thread_id);
        if let Some(tab_id) = state.selected_tabs.get(&tab_key) {
            model.select_tab(*tab_id);
        }
    }
}

fn persist_ui_state(state: &TuiState) -> Result<()> {
    let mut saved = TuiPersistentState::default();
    if let Some(row) = state.model.selected_row() {
        saved.last_domain = Some(row.key.domain_name.clone());
        saved
            .selected_threads
            .insert(row.key.domain_name.clone(), row.thread.id.clone());
        if let Some(tab) = state.model.selected_tab() {
            saved.selected_tabs.insert(
                format!("{}/{}", row.key.domain_name, row.thread.id),
                tab.tab_id,
            );
        }
    }
    settings::save_state(&state.state_path, &saved)
}

fn drain_events(receiver: &Receiver<AppEvent>, state: &mut TuiState) {
    let mut connection_changed = false;
    while let Ok(event) = receiver.try_recv() {
        match event {
            AppEvent::Session {
                domain_name,
                connection_generation,
                state: snapshot,
            } => {
                let current = state.domains.get(&domain_name).is_some_and(|domain| {
                    session_generation_is_current(
                        domain.state() == DomainState::Attached,
                        domain.connection_generation(),
                        state.connection_generations.get(&domain_name).copied(),
                        connection_generation,
                    )
                });
                if current && state.model.apply_snapshot(domain_name.clone(), snapshot) {
                    state
                        .last_viewports
                        .retain(|(domain, _), _| domain != &domain_name);
                    state.dirty = true;
                }
            }
            AppEvent::TreeChanged(domain_name) => {
                state.refresh_sessions.insert(domain_name);
            }
            AppEvent::Connected {
                domain_name,
                connection_generation,
            } => {
                let current = state.domains.get(&domain_name).is_some_and(|domain| {
                    domain.state() == DomainState::Attached
                        && domain.connection_generation() == Some(connection_generation)
                });
                if current {
                    cancel_takeover_geometry_for_domain(state, &domain_name);
                    connection_changed = true;
                    state
                        .connection_generations
                        .insert(domain_name.clone(), connection_generation);
                    state.model.begin_connection_generation(&domain_name);
                    state.refresh_sessions.insert(domain_name.clone());
                    state
                        .last_viewports
                        .retain(|(domain, _), _| domain != &domain_name);
                    state
                        .handoff_geometry_ready
                        .retain(|(domain, _), _| domain != &domain_name);
                    state
                        .shared_views
                        .retain(|(domain, _), _| domain != &domain_name);
                    state
                        .shared_view_at
                        .retain(|(domain, _), _| domain != &domain_name);
                    state
                        .share_view_pending
                        .retain(|(domain, _)| domain != &domain_name);
                    state
                        .followed_views
                        .retain(|(domain, _), _| domain != &domain_name);
                    state.dirty = true;
                }
            }
            AppEvent::Mux(notification) => {
                if notification_refreshes_session(&notification) {
                    state.refresh_sessions.extend(state.domains.keys().cloned());
                }

                if matches!(notification, MuxNotification::Empty) {
                    state.ui.status = "No live panes".to_string();
                }
                state.dirty = true;
            }
        }
    }
    if connection_changed {
        sync_connection_statuses(state);
    }
}

fn session_generation_is_current(
    attached: bool,
    live_generation: Option<u64>,
    accepted_generation: Option<u64>,
    event_generation: u64,
) -> bool {
    attached
        && live_generation == Some(event_generation)
        && accepted_generation == Some(event_generation)
}

fn notification_refreshes_session(notification: &MuxNotification) -> bool {
    matches!(
        notification,
        MuxNotification::PaneAdded(_)
            | MuxNotification::PaneRemoved(_)
            | MuxNotification::WindowCreated(_)
            | MuxNotification::WindowRemoved(_)
            | MuxNotification::WindowWorkspaceChanged(_)
            | MuxNotification::TabAddedToWindow { .. }
            | MuxNotification::TabTitleChanged { .. }
            | MuxNotification::ThinkTermSessionChanged
    )
}

async fn refresh_sessions(state: &mut TuiState) {
    let pending: Vec<_> = state.refresh_sessions.drain().collect();
    for name in pending {
        let Some(domain) = state.domains.get(&name).cloned() else {
            continue;
        };
        if domain.state() != DomainState::Attached {
            state.ui.status = format!("{name}: reconnecting");
            state.dirty = true;
            continue;
        }
        match fetch_current_session(&domain).await {
            Ok((generation, snapshot))
                if state.connection_generations.get(&name) == Some(&generation) =>
            {
                state.model.apply_snapshot(name, snapshot);
                state.ui.status.clear();
                state.dirty = true;
            }
            Ok(_) => {}
            Err(err) => {
                state.ui.status = format!("{name}: session refresh: {err}");
                state.dirty = true;
            }
        }
    }
}

/// Reflow the selected top-level tab to this TUI and derive the pane grids from
/// that same layout pass. `force` is used by an explicit takeover while another
/// renderer still owns the tab; normal frames only preview geometry we already
/// own, so a passive renderer never disturbs the shared local mirror.
fn prepare_active_native_viewport(
    state: &mut TuiState,
    area: ratatui::layout::Rect,
    screen: ScreenSize,
    force: bool,
    preview_epoch: Option<u64>,
) -> Option<ClientViewport> {
    let (_, domain, tab, _) = state.active_tab()?;
    if !force && domain.owns_remote_viewport(tab.tab_id()) != Some(true) {
        return None;
    }

    let shell = view::compute_view(area, &state.model, &state.ui, None);
    if shell.content.width == 0 || shell.content.height == 0 {
        return None;
    }
    let root = terminal_size(
        shell.content.width as usize,
        shell.content.height as usize,
        screen,
    );
    tab.resize(root);

    let layout = view::compute_view(area, &state.model, &state.ui, Some(&tab));
    let mut panes = Vec::new();
    for pane in &layout.panes {
        let handle = Mux::get().get_pane(pane.pane_id)?;
        handle.downcast_ref::<ClientPane>()?;
        let size = terminal_size(pane.rect.width as usize, pane.rect.height as usize, screen);
        panes.push(codec::ClientPaneViewport {
            pane_id: pane.pane_id,
            size,
        });
    }
    // Validate the complete pane set before mutating any surface. A topology
    // notification can remove one pane between the two layout passes; applying
    // half a viewport would recreate the very mixed geometry this path is
    // meant to prevent.
    for pane in &panes {
        let handle = Mux::get().get_pane(pane.pane_id)?;
        let client = handle.downcast_ref::<ClientPane>()?;
        if let Some(epoch) = preview_epoch {
            client.preview_frontend_geometry(epoch, pane.size);
        } else {
            client.adopt_frontend_geometry(pane.size);
        }
    }
    state.layout = layout;
    Some(ClientViewport::Native { size: root, panes })
}

fn forget_native_viewport_geometry(viewport: &ClientViewport) {
    let ClientViewport::Native { panes, .. } = viewport else {
        return;
    };
    for pane in panes {
        if let Some(handle) = Mux::get().get_pane(pane.pane_id) {
            if let Some(client) = handle.downcast_ref::<ClientPane>() {
                client.forget_frontend_geometry(pane.size);
            }
        }
    }
}

fn finish_native_viewport_preview(viewport: &ClientViewport, epoch: u64, succeeded: bool) {
    let ClientViewport::Native { panes, .. } = viewport else {
        return;
    };
    for pane in panes {
        if let Some(handle) = Mux::get().get_pane(pane.pane_id) {
            if let Some(client) = handle.downcast_ref::<ClientPane>() {
                client.finish_frontend_geometry_preview(epoch, pane.size, succeeded);
            }
        }
    }
}

fn begin_takeover_geometry_confirmation(
    state: &mut TuiState,
    cache_key: (String, TabId),
    epoch: u64,
    viewport: &ClientViewport,
    access_generation: u64,
) {
    let ClientViewport::Native { size, panes } = viewport else {
        return;
    };
    state.takeover_geometry_confirmations.insert(
        cache_key,
        TakeoverGeometryConfirmation {
            epoch,
            root_size: *size,
            panes: panes.iter().map(|pane| (pane.pane_id, pane.size)).collect(),
            access_generation,
            ready_since: None,
        },
    );
    state.redraw_until = Some(Instant::now() + RESIZE_REDRAW_WINDOW);
    state.dirty = true;
}

fn cancel_takeover_geometry_confirmation(
    state: &mut TuiState,
    cache_key: &(String, TabId),
    confirmation: &TakeoverGeometryConfirmation,
) {
    let viewport = ClientViewport::Native {
        size: confirmation.root_size,
        panes: confirmation
            .panes
            .iter()
            .map(|(pane_id, size)| codec::ClientPaneViewport {
                pane_id: *pane_id,
                size: *size,
            })
            .collect(),
    };
    finish_native_viewport_preview(&viewport, confirmation.epoch, false);
    state
        .takeover_epochs
        .finish(&cache_key.0, cache_key.1, confirmation.epoch);
    state.takeover_geometry_confirmations.remove(cache_key);
    if state.handoff_geometry_ready.get(cache_key) == Some(&confirmation.access_generation) {
        state.handoff_geometry_ready.remove(cache_key);
    }
    state.dirty = true;
}

fn cancel_takeover_geometry_for_domain(state: &mut TuiState, domain_name: &str) {
    let keys: Vec<_> = state
        .takeover_geometry_confirmations
        .keys()
        .filter(|(domain, _)| domain == domain_name)
        .cloned()
        .collect();
    for key in keys {
        if let Some(confirmation) = state.takeover_geometry_confirmations.get(&key).cloned() {
            cancel_takeover_geometry_confirmation(state, &key, &confirmation);
        }
    }
    state.takeover_epochs.clear_domain(domain_name);
}

fn active_layout_matches_takeover(
    state: &TuiState,
    tab: &Arc<Tab>,
    confirmation: &TakeoverGeometryConfirmation,
) -> bool {
    if tab.get_size() != confirmation.root_size {
        return false;
    }
    let Some(screen) = state.screen_size else {
        return false;
    };
    if terminal_size(
        state.layout.content.width as usize,
        state.layout.content.height as usize,
        screen,
    ) != confirmation.root_size
    {
        return false;
    }
    confirmation.panes.iter().all(|(pane_id, expected)| {
        state
            .layout
            .panes
            .iter()
            .find(|pane| pane.pane_id == *pane_id)
            .is_some_and(|pane| {
                terminal_size(pane.rect.width as usize, pane.rect.height as usize, screen)
                    == *expected
            })
    })
}

/// Confirm the same three layers before revealing a handoff: the local split
/// tree covers this TTY, every pane surface still has the previewed size, and
/// the server has returned a complete snapshot at that size. The preview pins
/// late deltas from the prior GUI until all three agree.
fn advance_takeover_geometry_confirmation(state: &mut TuiState) {
    let Some((domain_name, domain, tab, remote_tab_id)) = state.active_tab() else {
        return;
    };
    let cache_key = (domain_name, remote_tab_id);
    let Some(mut confirmation) = state
        .takeover_geometry_confirmations
        .get(&cache_key)
        .cloned()
    else {
        return;
    };

    let access = domain.remote_access_state();
    let still_owned = domain.owns_remote_viewport(tab.tab_id()) == Some(true)
        && access.as_ref().is_some_and(|access| {
            access.mode == codec::FrontendAccessMode::Handoff
                && access.generation == confirmation.access_generation
        })
        && state.takeover_epochs.current(&cache_key.0, cache_key.1) == Some(confirmation.epoch);
    if !still_owned {
        cancel_takeover_geometry_confirmation(state, &cache_key, &confirmation);
        return;
    }

    let mut ready = active_layout_matches_takeover(state, &tab, &confirmation)
        && !confirmation.panes.is_empty();
    for (pane_id, size) in &confirmation.panes {
        let pane_ready = Mux::get()
            .get_pane(*pane_id)
            .and_then(|pane| {
                pane.downcast_ref::<ClientPane>()
                    .map(|client| client.prime_frontend_geometry(*size))
            })
            .unwrap_or(false);
        ready &= pane_ready;
    }

    if !takeover_geometry_settled(ready, Instant::now(), &mut confirmation.ready_since) {
        state
            .takeover_geometry_confirmations
            .insert(cache_key, confirmation);
        return;
    }

    let viewport = ClientViewport::Native {
        size: confirmation.root_size,
        panes: confirmation
            .panes
            .iter()
            .map(|(pane_id, size)| codec::ClientPaneViewport {
                pane_id: *pane_id,
                size: *size,
            })
            .collect(),
    };
    finish_native_viewport_preview(&viewport, confirmation.epoch, true);
    state
        .takeover_epochs
        .finish(&cache_key.0, cache_key.1, confirmation.epoch);
    state
        .handoff_geometry_ready
        .insert(cache_key.clone(), confirmation.access_generation);
    state.takeover_geometry_confirmations.remove(&cache_key);
    state.dirty = true;
}

async fn report_viewport_if_changed<T: termwiz::terminal::Terminal>(
    state: &mut TuiState,
    remote_tab_id: Option<TabId>,
    backend: &mut TermwizBackend<T>,
) {
    if let Some(settles_at) = state.resize_settles_at {
        if Instant::now() < settles_at {
            // Still mid-gesture. The local redraw already happened; only the
            // trip to the server waits for the size to mean something.
            return;
        }
        state.resize_settles_at = None;
    }
    let Some(remote_tab_id) = remote_tab_id else {
        return;
    };
    let Some(row) = state.model.selected_row() else {
        return;
    };
    let domain_name = row.key.domain_name.clone();
    let Some(domain) = state.domains.get(&domain_name).cloned() else {
        return;
    };
    let Some(local_tab_id) = domain.remote_to_local_tab_id(remote_tab_id) else {
        return;
    };
    let content = state.layout.content;
    if content.width == 0 || content.height == 0 {
        return;
    }
    let screen = match backend.screen_size() {
        Ok(screen) => screen,
        Err(err) => {
            state.ui.status = format!("Viewport: {err}");
            return;
        }
    };
    state.screen_size = Some(screen);
    let size = terminal_size(content.width as usize, content.height as usize, screen);
    // A passive renderer advertises only the screen it could take over with.
    // Once it owns the tab, the pre-draw geometry pass has exact pane grids,
    // including TUI borders/nav bars, so publish them together with the root.
    // This replaces the old CellGrid-then-detached-Resize sequence whose
    // intermediate split trees were visible to a concurrently attached GUI.
    let ownership = domain.owns_remote_viewport(local_tab_id);
    let owns = ownership != Some(false);
    let recovery_generation =
        domain.pending_frontend_recovery(FrontendRecoverySlot::Primary, local_tab_id);
    let viewport = if ownership == Some(true) || recovery_generation.is_some() {
        let panes = state
            .layout
            .panes
            .iter()
            .filter_map(|pane| {
                let handle = Mux::get().get_pane(pane.pane_id)?;
                handle.downcast_ref::<ClientPane>()?;
                Some(codec::ClientPaneViewport {
                    pane_id: pane.pane_id,
                    size: terminal_size(
                        pane.rect.width as usize,
                        pane.rect.height as usize,
                        screen,
                    ),
                })
            })
            .collect();
        ClientViewport::Native { size, panes }
    } else {
        ClientViewport::CellGrid { size }
    };
    let cache_key = (domain_name.clone(), remote_tab_id);
    let handoff_generation = domain
        .remote_access_state()
        .filter(|access| access.mode == codec::FrontendAccessMode::Handoff)
        .map(|access| access.generation);
    let geometry_generation_is_stale = ownership == Some(true)
        && handoff_generation.is_some_and(|generation| {
            state.handoff_geometry_ready.get(&cache_key) != Some(&generation)
        });
    let geometry_confirmation_pending = state
        .takeover_geometry_confirmations
        .contains_key(&cache_key);
    for candidate in state.domains.values() {
        if candidate.domain_id() != domain.domain_id() {
            candidate.clear_frontend_recovery_intent(FrontendRecoverySlot::Primary);
        }
    }
    if let Err(err) =
        domain.set_frontend_recovery_intent(FrontendRecoverySlot::Primary, local_tab_id, &viewport)
    {
        log::trace!("recording TUI frontend recovery intent: {err:#}");
    }

    if recovery_generation.is_some()
        || (geometry_generation_is_stale && !geometry_confirmation_pending)
        || state.last_viewports.get(&cache_key) != Some(&Some((viewport.clone(), owns)))
    {
        match domain
            .set_client_viewport(local_tab_id, viewport.clone())
            .await
        {
            Ok(response) => {
                let owns = domain.owns_remote_viewport(local_tab_id) != Some(false);
                state
                    .last_viewports
                    .insert(cache_key.clone(), Some((viewport.clone(), owns)));
                adopt_local_tab_size(
                    local_tab_id,
                    local_tab_size(&domain, local_tab_id, size, response.canonical_size),
                );
                if let Some(generation) = recovery_generation {
                    if let Err(err) = domain.resync().await {
                        if let Some(epoch) =
                            state.takeover_epochs.current(&cache_key.0, cache_key.1)
                        {
                            finish_native_viewport_preview(&viewport, epoch, false);
                            state
                                .takeover_epochs
                                .finish(&cache_key.0, cache_key.1, epoch);
                            state.takeover_geometry_confirmations.remove(&cache_key);
                            state.handoff_geometry_ready.remove(&cache_key);
                        } else {
                            forget_native_viewport_geometry(&viewport);
                        }
                        domain.fail_frontend_recovery(
                            FrontendRecoverySlot::Primary,
                            local_tab_id,
                            generation,
                            format!("TUI replacement topology resync failed: {err:#}"),
                        );
                        state.ui.status = format!("Restoring terminal: {err:#}");
                        state.dirty = true;
                        return;
                    }
                    let final_local_tab_id = domain
                        .remote_to_local_tab_id(remote_tab_id)
                        .unwrap_or(local_tab_id);
                    adopt_local_tab_size(final_local_tab_id, response.canonical_size);
                    if !domain.acknowledge_frontend_recovery(
                        FrontendRecoverySlot::Primary,
                        local_tab_id,
                        generation,
                    ) {
                        state.dirty = true;
                        return;
                    }
                }
                if owns && matches!(viewport, ClientViewport::Native { .. }) {
                    if response.access.mode == codec::FrontendAccessMode::Handoff {
                        if let Some(epoch) =
                            state.takeover_epochs.current(&cache_key.0, cache_key.1)
                        {
                            begin_takeover_geometry_confirmation(
                                state,
                                cache_key.clone(),
                                epoch,
                                &viewport,
                                response.access.generation,
                            );
                        } else {
                            // An ordinary resize by an already-confirmed owner
                            // does not obscure the terminal. Its access
                            // generation is unchanged and remains ready.
                            let prior = state
                                .handoff_geometry_ready
                                .insert(cache_key.clone(), response.access.generation);
                            if prior != Some(response.access.generation) {
                                state.dirty = true;
                            }
                        }
                    }
                } else if owns {
                    // The first passive advertisement may have made this TUI B's
                    // initial owner. Keep the mask and draw once more so the next
                    // request carries the exact pane geometry as Native.
                    state.dirty = true;
                }
            }
            Err(err) => {
                if let Some(epoch) = state.takeover_epochs.current(&cache_key.0, cache_key.1) {
                    finish_native_viewport_preview(&viewport, epoch, false);
                    state
                        .takeover_epochs
                        .finish(&cache_key.0, cache_key.1, epoch);
                    state.takeover_geometry_confirmations.remove(&cache_key);
                    state.handoff_geometry_ready.remove(&cache_key);
                } else {
                    forget_native_viewport_geometry(&viewport);
                }
                if let Some(generation) = recovery_generation {
                    domain.fail_frontend_recovery(
                        FrontendRecoverySlot::Primary,
                        local_tab_id,
                        generation,
                        format!("TUI replacement viewport failed: {err:#}"),
                    );
                }
                state.ui.status = format!("Viewport: {err}");
                state.dirty = true;
                return;
            }
        }
    }
    share_view_if_changed(state, &domain, local_tab_id, &cache_key).await;
}

/// Ask every pane on screen whether anything changed.
///
/// The answer is discarded: what matters is the side effect, which is that a
/// remote pane forwards the question to the server. Anything that comes back
/// arrives as a mux notification and marks the frame dirty on its own, so this
/// never draws and an idle screen stays idle.
fn poll_panes(state: &TuiState) {
    for pane in &state.layout.panes {
        if let Some(handle) = Mux::get().get_pane(pane.pane_id) {
            let _ = handle.get_changed_since(0..0, handle.get_current_seqno());
        }
    }
}

/// How often at most to tell the server what this renderer is looking at.
///
/// A wheel notch changes the scroll position, and a flick on a phone is a
/// stream of them; one round trip per notch would spend the link on saying the
/// same thing repeatedly. The follower is a person watching another screen, so
/// a tenth of a second is well below what anyone can see.
const SHARE_VIEW_INTERVAL: Duration = Duration::from_millis(100);

/// Whether the coalescing window has closed on a deferred view change.
fn share_view_is_due(state: &TuiState, cache_key: &(String, TabId)) -> bool {
    state
        .shared_view_at
        .get(cache_key)
        .is_none_or(|at| at.elapsed() >= SHARE_VIEW_INTERVAL)
}

/// Offer what this renderer is looking at, so the other renderers on this tab
/// can show the same thing.
///
/// Sharing the size makes two attached devices the same shape; it does not make
/// them the same view, and a phone and a desktop sitting at different points in
/// one scrollback read as two sessions that merely share a name.
///
/// Only what the *owner* sees is worth offering — a renderer nobody is using
/// does not get to move everyone else — and the server enforces that too, so
/// this can be wrong about ownership without any harm beyond a wasted message.
async fn share_view_if_changed(
    state: &mut TuiState,
    domain: &Arc<ClientDomain>,
    local_tab_id: TabId,
    cache_key: &(String, TabId),
) {
    if domain.owns_remote_viewport(local_tab_id) == Some(false) {
        follow_shared_view(state, domain, local_tab_id, cache_key);
        return;
    }
    let mut scroll = Vec::new();
    for pane in &state.layout.panes {
        let offset = state.ui.scroll_offset(pane.pane_id);
        if offset == 0 {
            // Following its output, which is the default a follower already has.
            continue;
        }
        let Some(handle) = Mux::get().get_pane(pane.pane_id) else {
            continue;
        };
        let Some(client_pane) = handle.downcast_ref::<ClientPane>() else {
            continue;
        };
        scroll.push(codec::ClientPaneScroll {
            pane_id: client_pane.remote_pane_id,
            lines_from_bottom: offset.min(u32::MAX as usize) as u32,
        });
    }
    scroll.sort_by_key(|entry| entry.pane_id);
    let view = codec::ClientView { scroll };
    if state.shared_views.get(cache_key) == Some(&view) {
        state.share_view_pending.remove(cache_key);
        return;
    }
    if state
        .shared_view_at
        .get(cache_key)
        .is_some_and(|at| at.elapsed() < SHARE_VIEW_INTERVAL)
    {
        // Deferred, not dropped. Skipping outright leaves the follower one
        // notch behind for good, because the last notch of a flick is exactly
        // the one that lands inside the window and nothing draws afterwards to
        // try again.
        state.share_view_pending.insert(cache_key.clone());
        return;
    }
    state
        .shared_view_at
        .insert(cache_key.clone(), Instant::now());
    state.share_view_pending.remove(cache_key);
    match domain.set_client_view(local_tab_id, view.clone()).await {
        Ok(()) => {
            state.shared_views.insert(cache_key.clone(), view);
        }
        Err(err) => {
            log::trace!("sharing the view of tab {local_tab_id}: {err:#}");
            state.share_view_pending.insert(cache_key.clone());
        }
    }
}

/// Show what the renderer being used is showing.
///
/// The other half of `share_view_if_changed`, and deliberately one-way: a
/// follower never writes back, so two attached devices cannot argue about where
/// the scrollback should sit. When nobody has offered a view — because the
/// owner is a renderer that does not publish one — this leaves the follower
/// exactly where it was rather than pulling it somewhere arbitrary.
fn follow_shared_view(
    state: &mut TuiState,
    domain: &Arc<ClientDomain>,
    local_tab_id: TabId,
    cache_key: &(String, TabId),
) {
    let Some(remote) = domain.remote_viewport_state(local_tab_id) else {
        return;
    };
    let Some(view) = remote.view else {
        return;
    };
    if state.followed_views.get(cache_key) == Some(&view) {
        return;
    }
    for pane in &state.layout.panes {
        let Some(handle) = Mux::get().get_pane(pane.pane_id) else {
            continue;
        };
        let Some(client_pane) = handle.downcast_ref::<ClientPane>() else {
            continue;
        };
        let offset = view
            .scroll
            .iter()
            .find(|entry| entry.pane_id == client_pane.remote_pane_id)
            .map_or(0, |entry| entry.lines_from_bottom as usize);
        // Clamped against this renderer's own scrollback: the two ends agree on
        // how far back the owner is, not on how much history each has fetched.
        let dims = handle.get_dimensions();
        let offset = offset.min(dims.scrollback_rows.saturating_sub(dims.viewport_rows));
        state.ui.set_scroll_offset(pane.pane_id, offset);
    }
    state.followed_views.insert(cache_key.clone(), view);
    state.dirty = true;
}

/// Which size this renderer's own copy of the tab should be laid out at.
///
/// `canonical_size` is the *server's* tab size, and the server derives it back
/// from the pane sizes it was told (`rebuild_splits_sizes_from_contained_panes`).
/// Once a pane's rectangle loses a row to its own nav bar, that shrunken row
/// count is what comes home in the echo — adopting it costs another row on the
/// next layout, and another after that. The GUI never adopts it either: the
/// only use of `canonical_size` on that side is the `VIEW 73×28` title, and
/// `ClientDomain`'s own resync refuses the wire size for the same reason
/// ("would shrink the tab by the chrome height on every resync").
///
/// So the owner lays out at the size it just reported and the server just
/// applied. A renderer that does *not* own the viewport has nothing of its own
/// to lay out at and still needs to draw the grid the panes really have, so it
/// keeps taking the server's answer.
fn choose_local_tab_size(
    owns: Option<bool>,
    reported: TerminalSize,
    canonical: TerminalSize,
) -> TerminalSize {
    match owns {
        Some(false) => canonical,
        _ => reported,
    }
}

fn local_tab_size(
    domain: &Arc<ClientDomain>,
    local_tab_id: TabId,
    reported: TerminalSize,
    canonical: TerminalSize,
) -> TerminalSize {
    choose_local_tab_size(
        domain.owns_remote_viewport(local_tab_id),
        reported,
        canonical,
    )
}

/// Pane rectangles are laid out from the tab's pane geometry, not from the
/// screen, so a tab left at a stale size leaves every pane drawn at whatever it
/// last measured — full-width output arrives and is then clipped to the width
/// the terminal had before it was resized.
fn adopt_local_tab_size(local_tab_id: TabId, size: TerminalSize) {
    if size.rows == 0 || size.cols == 0 {
        return;
    }
    if let Some(tab) = Mux::get().get_tab(local_tab_id) {
        tab.resize(size);
    }
}

async fn dispatch_actions(state: &mut TuiState) {
    while let Some(action) = state.actions.pop_front() {
        if let Err(err) = dispatch_action(state, action).await {
            state.ui.pending = None;
            state.ui.status = format!("{err:#}");
        }
        if let Err(err) = persist_ui_state(state) {
            state.ui.status = format!("Saving TUI state: {err:#}");
        }
        state.dirty = true;
    }
}

async fn claim_active_viewport(state: &mut TuiState) -> Result<Arc<Tab>> {
    let (domain_name, domain, tab, remote_tab_id) =
        state.active_tab().context("no live tab selected")?;
    if !matches!(
        domain.remote_frontend_gate(),
        RemoteFrontendGate::Visible | RemoteFrontendGate::Claimable { .. }
    ) {
        anyhow::bail!("terminal connection is not ready for takeover");
    }
    let local_tab_id = tab.tab_id();
    if domain.owns_remote_viewport(local_tab_id) == Some(true) {
        return Ok(tab);
    }
    let screen = state
        .screen_size
        .context("viewport is not ready; retry after the terminal redraws")?;
    let handoff = domain
        .remote_access_state()
        .is_some_and(|access| access.mode == codec::FrontendAccessMode::Handoff);
    let epoch = state.takeover_epochs.begin(&domain_name, remote_tab_id);
    let viewport = if handoff {
        let area = state.layout.screen;
        match prepare_active_native_viewport(state, area, screen, true, Some(epoch)) {
            Some(viewport) => viewport,
            None => {
                state
                    .takeover_epochs
                    .finish(&domain_name, remote_tab_id, epoch);
                anyhow::bail!("viewport is not ready; retry after the terminal redraws");
            }
        }
    } else {
        match state
            .last_viewports
            .get(&(domain_name.clone(), remote_tab_id))
            .and_then(|entry| entry.as_ref())
            .map(|(viewport, _)| viewport.clone())
        {
            Some(viewport) => viewport,
            None => {
                state
                    .takeover_epochs
                    .finish(&domain_name, remote_tab_id, epoch);
                anyhow::bail!("viewport is not ready; retry after the terminal redraws");
            }
        }
    };
    let reported_size = viewport.size();
    state.dirty = true;
    let claimed = match domain
        .claim_client_viewport(local_tab_id, viewport.clone())
        .await
    {
        Ok(claimed) => claimed,
        Err(err) => {
            if handoff {
                finish_native_viewport_preview(&viewport, epoch, false);
            } else {
                forget_native_viewport_geometry(&viewport);
            }
            state
                .takeover_epochs
                .finish(&domain_name, remote_tab_id, epoch);
            return Err(err);
        }
    };
    if domain.owns_remote_viewport(local_tab_id) != Some(true) {
        if handoff {
            finish_native_viewport_preview(&viewport, epoch, false);
        } else {
            forget_native_viewport_geometry(&viewport);
        }
        state
            .takeover_epochs
            .finish(&domain_name, remote_tab_id, epoch);
        anyhow::bail!("the server did not grant viewport ownership");
    }

    adopt_local_tab_size(local_tab_id, reported_size);
    let resync = domain.resync().await;
    let final_local_tab_id = domain
        .remote_to_local_tab_id(remote_tab_id)
        .unwrap_or(local_tab_id);
    adopt_local_tab_size(final_local_tab_id, reported_size);
    let final_tab = Mux::get().get_tab(final_local_tab_id).unwrap_or(tab);
    if let Err(err) = resync {
        log::warn!(
            "resyncing {domain_name} after TUI viewport takeover: {err:#}; using acknowledged geometry"
        );
        schedule_takeover_resync(domain_name.clone(), Arc::clone(&domain));
    }
    let cache_key = (domain_name, remote_tab_id);
    state
        .last_viewports
        .insert(cache_key.clone(), Some((viewport.clone(), true)));
    if claimed.access.mode == codec::FrontendAccessMode::Handoff {
        begin_takeover_geometry_confirmation(
            state,
            cache_key.clone(),
            epoch,
            &viewport,
            claimed.access.generation,
        );
    } else {
        state
            .takeover_epochs
            .finish(&cache_key.0, cache_key.1, epoch);
    }
    for pane in &state.layout.panes {
        state.ui.set_scroll_offset(pane.pane_id, 0);
    }
    state.shared_views.remove(&cache_key);
    state.shared_view_at.remove(&cache_key);
    state.share_view_pending.remove(&cache_key);
    state.followed_views.remove(&cache_key);
    Ok(final_tab)
}

fn schedule_takeover_resync(domain_name: String, domain: Arc<ClientDomain>) {
    promise::spawn::spawn(async move {
        smol::Timer::after(TAKEOVER_RESYNC_RETRY_DELAY).await;
        if let Err(err) = domain.resync().await {
            log::warn!("retrying {domain_name} topology after TUI takeover: {err:#}");
        }
        wake_terminal();
        Ok::<(), anyhow::Error>(())
    })
    .detach();
}

async fn set_active_frontend_access_mode(
    state: &mut TuiState,
    mode: codec::FrontendAccessMode,
) -> Result<()> {
    let (domain_name, domain, tab, remote_tab_id) =
        state.active_tab().context("no live tab selected")?;
    let local_tab_id = tab.tab_id();
    let screen = state
        .screen_size
        .context("viewport is not ready; retry after the terminal redraws")?;
    let area = state.layout.screen;
    let viewport = prepare_active_native_viewport(state, area, screen, false, None)
        .context("viewport is not ready; retry after the terminal redraws")?;
    let access = domain
        .set_frontend_access_mode(local_tab_id, mode, viewport.clone())
        .await?;
    let cache_key = (domain_name, remote_tab_id);
    state
        .last_viewports
        .insert(cache_key.clone(), Some((viewport, true)));
    if access.mode == codec::FrontendAccessMode::Handoff {
        state
            .handoff_geometry_ready
            .insert(cache_key.clone(), access.generation);
    }
    for pane in &state.layout.panes {
        state.ui.set_scroll_offset(pane.pane_id, 0);
    }
    state.shared_views.remove(&cache_key);
    state.shared_view_at.remove(&cache_key);
    state.share_view_pending.remove(&cache_key);
    state.followed_views.remove(&cache_key);
    state.clear_selected_viewport_cache();
    Ok(())
}

async fn dispatch_action(state: &mut TuiState, action: Action) -> Result<()> {
    match action {
        Action::None => {}
        Action::ClaimFrontendAccess => {
            claim_active_viewport(state).await?;
        }
        Action::SetFrontendAccessMode(mode) => {
            set_active_frontend_access_mode(state, mode).await?;
        }
        Action::Detach => state.ui.exit = true,
        Action::ToggleSidebar => {
            state.ui.sidebar_visible = !state.ui.sidebar_visible;
            // On a narrow screen the tree is not a panel beside the terminal,
            // it is an overlay on top of it — a way to get somewhere, not a
            // thing you leave open. Remembering it as a preference means a
            // phone and a desktop sharing one config fight over a single
            // boolean, and whoever opened it last decides that the other one
            // opens covered.
            if tree_visibility_is_a_preference(state.layout.class) {
                state.settings.sidebar_visible = state.ui.sidebar_visible;
                save_tui_settings(state)?;
            }
            state.clear_selected_viewport_cache();
        }
        Action::ToggleHelp => {
            state.ui.mode = if state.ui.mode == AppMode::Help {
                AppMode::Terminal
            } else {
                AppMode::Help
            };
        }
        Action::OpenNavigator => {
            state.ui.sidebar_visible = true;
            state.ui.mode = AppMode::Navigate;
        }
        Action::OpenConnections => {
            sync_connection_statuses(state);
            state.ui.mode = AppMode::Connections;
        }
        Action::OpenSettings => {
            state.ui.mode = AppMode::Settings;
            state.ui.settings_index = 0;
        }
        Action::AdjustSetting { index, delta } => {
            adjust_setting(state, index, delta)?;
        }
        Action::CloseOverlay | Action::CancelPrompt => state.ui.close_overlay(),
        Action::SelectThread(key) => select_thread(state, &key).await?,
        Action::SelectRelativeThread(delta) => {
            if state.model.select_relative_thread(delta) {
                let key = state.model.selected_key().cloned();
                state.clear_selected_viewport_cache();
                if let Some(key) = key {
                    select_thread(state, &key).await?;
                }
            }
        }
        Action::SelectTab(tab_id) => {
            if state.model.select_tab(tab_id) {
                state.clear_selected_viewport_cache();
            }
        }
        Action::SelectRelativeTab(delta) => {
            if state.model.select_relative_tab(delta) {
                state.clear_selected_viewport_cache();
            }
        }
        Action::SelectNextAttention => {
            if let Some(key) = state.model.next_attention(state.model.selected_key()) {
                select_thread(state, &key).await?;
            }
        }
        Action::FocusPane(direction) => {
            claim_active_viewport(state).await?;
            if let Some((_, _, tab, _)) = state.active_tab() {
                tab.activate_pane_direction(direction);
            }
        }
        Action::FocusPaneId(pane_id) => {
            claim_active_viewport(state).await?;
            if let (Some((_, _, tab, _)), Some(pane)) = (state.active_tab(), state.pane(pane_id)) {
                tab.set_active_pane(&pane);
            }
        }
        Action::CyclePane(delta) => {
            claim_active_viewport(state).await?;
            cycle_pane(state, delta);
        }
        Action::NewTab => spawn_tab_for_selected_thread(state).await?,
        Action::RenameTab => begin_rename_tab(state),
        Action::NewPaneInStack(pane_id) => new_pane_in_stack(state, pane_id).await?,
        Action::SplitPane(axis) => split_active_pane(state, axis).await?,
        Action::ToggleZoom => {
            claim_active_viewport(state).await?;
            if let Some((_, _, tab, _)) = state.active_tab() {
                tab.toggle_zoom();
            }
        }
        Action::EnterResize => state.ui.mode = AppMode::Resize,
        Action::ResizePane(direction, amount) => {
            let tab = claim_active_viewport(state).await?;
            tab.adjust_pane_size(direction, amount.unsigned_abs().max(1));
            state.clear_selected_viewport_cache();
        }
        Action::ResizeSplit { split_index, delta } => {
            let tab = claim_active_viewport(state).await?;
            tab.resize_split_by(split_index, delta);
            state.clear_selected_viewport_cache();
        }
        Action::EnterCopyMode => enter_copy_mode(state),
        Action::LeaveCopyMode => {
            state.ui.copy = None;
            state.ui.mode = AppMode::Terminal;
        }
        Action::ScrollPane { pane_id, lines } => {
            claim_active_viewport(state).await?;
            scroll_pane(state, pane_id, lines);
        }
        Action::ScrollToBottom { pane_id } => {
            claim_active_viewport(state).await?;
            state.ui.set_scroll_offset(pane_id, 0);
        }
        Action::CopySelection => copy_selection(state)?,
        Action::PasteClipboard => paste_clipboard(state)?,
        Action::BeginSearch { backwards } => begin_search(state, backwards),
        Action::SearchNext { backwards } => select_search_match(state, backwards),
        Action::SubmitPrompt => submit_prompt(state).await?,
        Action::NewSpace { domain_name } => begin_prompt(
            state,
            "New Space",
            "",
            PromptAction::CreateSpace { domain_name },
        ),
        Action::NewProject {
            domain_name,
            space_id,
        } => begin_prompt(
            state,
            "New Project",
            "",
            PromptAction::CreateProject {
                domain_name,
                space_id,
            },
        ),
        Action::NewThread {
            domain_name,
            project_id,
        } => begin_prompt(
            state,
            "New Thread",
            "",
            PromptAction::CreateThread {
                domain_name,
                project_id,
            },
        ),
        Action::RenameNode(key) => begin_rename_node(state, key),
        Action::ToggleThreadPinned(key) => toggle_thread_pinned(state, key).await?,
        Action::MoveThread { key, before } => move_thread(state, key, before).await?,
        Action::MoveProject {
            domain_name,
            space_id,
            project_id,
            before,
        } => {
            mutate_authoritative(
                state,
                &domain_name,
                "Moving project",
                vec![codec::TreeOp::MoveProjectBefore {
                    space_id,
                    project_id,
                    before,
                }],
            )
            .await?;
        }
        Action::Confirm(action) => begin_confirmation(state, action),
        Action::AcceptConfirmation => accept_confirmation(state).await?,
        Action::ConnectDomain(name) | Action::RetryDomain(name) => {
            retry_domain(state, &name).await?
        }
        Action::DisconnectDomain(name) => {
            cancel_takeover_geometry_for_domain(state, &name);
            if let Some(domain) = state.domains.get(&name) {
                domain.perform_detach();
            }
            state.model.remove_domain(&name);
            state.connection_generations.remove(&name);
            state.refresh_sessions.remove(&name);
            state
                .last_viewports
                .retain(|(domain, _), _| domain != &name);
            state
                .handoff_geometry_ready
                .retain(|(domain, _), _| domain != &name);
            state.shared_views.retain(|(domain, _), _| domain != &name);
            state
                .shared_view_at
                .retain(|(domain, _), _| domain != &name);
            state
                .share_view_pending
                .retain(|(domain, _)| domain != &name);
            state
                .followed_views
                .retain(|(domain, _), _| domain != &name);
            state.ui.mode = AppMode::Terminal;
            state.ui.status = format!("{name}: disconnected; remote sessions are still running");
            sync_connection_statuses(state);
        }
    }
    Ok(())
}

/// Every row of the settings overlay, in order, as `(section, label)`.
///
/// Drawing, moving the cursor and changing a value all index this one list.
/// They used to agree only by hand-counted index, which is why three settings
/// added since could be changed by nobody: the cursor stopped at four.
pub const SETTINGS: [(&str, &str); 9] = [
    ("Appearance", "Theme"),
    ("Appearance", "Sidebar"),
    ("Appearance", "Pane nav bar"),
    ("Appearance", "Pane borders"),
    ("Appearance", "Pane scrollbars"),
    ("Input", "Application mouse"),
    ("Input", "Copy on select"),
    ("Input", "Touch targets"),
    ("Terminal", "Scroll lines"),
];

/// What the row at `index` currently reads, and whether it is a plain on/off.
pub fn setting_value(settings: &TuiConfig, sidebar_visible: bool, index: usize) -> (String, bool) {
    match index {
        0 => (settings.theme.label().to_string(), false),
        1 => (String::new(), sidebar_visible),
        2 => (String::new(), settings.pane_nav_bar),
        3 => (String::new(), settings.pane_borders),
        4 => (String::new(), settings.pane_scrollbars),
        5 => (String::new(), settings.mouse),
        6 => (String::new(), settings.copy_on_select),
        // Undecided is a real state here: it means "follow the layout", which
        // is neither on nor off and has to say so.
        7 => match settings.touch_targets {
            None => ("Auto".to_string(), false),
            Some(value) => (String::new(), value),
        },
        8 => (settings.scroll_lines.to_string(), false),
        _ => (String::new(), false),
    }
}

fn adjust_setting(state: &mut TuiState, index: usize, delta: isize) -> Result<()> {
    match index {
        0 => {
            state.settings.theme = state.settings.theme.next(delta.signum());
            settings::set_active_theme(state.settings.theme);
        }
        1 => {
            state.ui.sidebar_visible = !state.ui.sidebar_visible;
            state.settings.sidebar_visible = state.ui.sidebar_visible;
            state.clear_selected_viewport_cache();
        }
        2 => {
            state.settings.pane_nav_bar = !state.settings.pane_nav_bar;
            state.ui.pane_nav_bar = state.settings.pane_nav_bar;
            // All three of these hand columns and rows between the chrome and
            // the grid, so the server has to be told the pane changed size.
            state.clear_selected_viewport_cache();
        }
        3 => {
            state.settings.pane_borders = !state.settings.pane_borders;
            state.ui.pane_borders = state.settings.pane_borders;
            state.clear_selected_viewport_cache();
        }
        4 => {
            state.settings.pane_scrollbars = !state.settings.pane_scrollbars;
            state.ui.pane_scrollbars = state.settings.pane_scrollbars;
            state.clear_selected_viewport_cache();
        }
        5 => state.settings.mouse = !state.settings.mouse,
        6 => state.settings.copy_on_select = !state.settings.copy_on_select,
        // Cycles through the third state rather than skipping it: following the
        // layout is the default and has to be reachable again.
        7 => {
            state.settings.touch_targets = match state.settings.touch_targets {
                None => Some(true),
                Some(true) => Some(false),
                Some(false) => None,
            };
            state.ui.touch_targets = state.settings.touch_targets;
            state.clear_selected_viewport_cache();
        }
        8 => {
            state.settings.scroll_lines =
                (state.settings.scroll_lines as isize + delta).clamp(1, 20) as usize;
        }
        _ => return Ok(()),
    }
    save_tui_settings(state)
}

fn save_tui_settings(state: &TuiState) -> Result<()> {
    settings::save_config(&state.settings_path, &state.settings)
}

async fn select_thread(state: &mut TuiState, key: &ThreadKey) -> Result<()> {
    if state.model.select_thread(key) {
        state.clear_selected_viewport_cache();
    }
    // On a screen too narrow to show both, the tree is covering the terminal
    // the reader just asked for. Picking a thread is the whole point of opening
    // it, so getting out of the way is the answer they meant.
    if state.layout.class == ViewClass::Narrow && state.ui.sidebar_visible {
        state.ui.sidebar_visible = false;
        state.clear_selected_viewport_cache();
    }
    mark_thread_seen(state, key).await?;
    ensure_selected_thread_live(state, false, None).await
}

async fn ensure_selected_thread_live(
    state: &mut TuiState,
    force: bool,
    domain_override: Option<&str>,
) -> Result<()> {
    let selected = state
        .model
        .selected_row()
        .filter(|row| domain_override.is_none_or(|domain| row.key.domain_name == domain))
        .cloned();
    if !force
        && selected
            .as_ref()
            .is_some_and(|row| !row.thread.tabs.is_empty())
    {
        return Ok(());
    }
    let domain_name = domain_override
        .map(str::to_string)
        .or_else(|| selected.as_ref().map(|row| row.key.domain_name.clone()))
        .or_else(|| state.domains.keys().next().cloned())
        .context("no attached mux server")?;
    let preferred_thread_id = selected.as_ref().map(|row| row.thread.id.clone());
    let domain = state
        .domains
        .get(&domain_name)
        .cloned()
        .with_context(|| format!("{domain_name}: not attached"))?;
    let generation = domain
        .connection_generation()
        .context("connection has no generation")?;

    state.ui.pending = Some(PendingOperation {
        label: "Opening terminal".into(),
        domain_name: domain_name.clone(),
    });
    state.ui.status = format!("Opening terminal on {domain_name}…");
    let response = domain
        .ensure_thinkterm_thread(preferred_thread_id, materialize_size(state))
        .await
        .with_context(|| format!("materializing a Thread on {domain_name}"))?;
    if domain.state() != DomainState::Attached
        || domain.connection_generation() != Some(generation)
        || state.connection_generations.get(&domain_name) != Some(&generation)
    {
        anyhow::bail!("{domain_name}: connection changed while opening the terminal");
    }
    let (snapshot_generation, snapshot) = fetch_current_session(&domain).await?;
    if snapshot_generation != generation {
        anyhow::bail!("{domain_name}: stale terminal snapshot");
    }
    state.model.apply_snapshot(domain_name.clone(), snapshot);
    let key = state
        .model
        .rows()
        .iter()
        .find(|row| row.key.domain_name == domain_name && row.thread.id == response.thread_id)
        .map(|row| row.key.clone())
        .context("the server landing Thread is missing from its session snapshot")?;
    state.model.select_thread(&key);
    state.clear_selected_viewport_cache();
    state.ui.pending = None;
    state.ui.status.clear();
    Ok(())
}

async fn mark_thread_seen(state: &mut TuiState, key: &ThreadKey) -> Result<()> {
    let Some(row) = state.model.row(key) else {
        return Ok(());
    };
    if !row.thread.is_unread {
        return Ok(());
    }
    let thread_id = row.thread.id.clone();
    let domain_name = row.key.domain_name.clone();
    mutate_authoritative(
        state,
        &domain_name,
        "Marking Thread read",
        vec![codec::TreeOp::SetThreadUnread {
            thread_id,
            unread: false,
        }],
    )
    .await?;
    Ok(())
}

fn cycle_pane(state: &mut TuiState, delta: isize) {
    let Some((_, _, tab, _)) = state.active_tab() else {
        return;
    };
    let panes = tab.iter_panes();
    if panes.is_empty() {
        return;
    }
    let active = tab.get_active_pane().map(|pane| pane.pane_id());
    let current = active
        .and_then(|id| panes.iter().position(|pane| pane.pane.pane_id() == id))
        .unwrap_or(0);
    let next = (current as isize + delta).rem_euclid(panes.len() as isize) as usize;
    tab.set_active_pane(&panes[next].pane);
}

fn enter_copy_mode(state: &mut TuiState) {
    let Some(pane) = state.active_pane() else {
        state.ui.status = "No live pane selected".into();
        return;
    };
    let cursor = pane.get_cursor_position();
    let selection = state
        .ui
        .selection
        .clone()
        .filter(|selection| selection.pane_id == pane.pane_id());
    state.ui.copy = Some(CopyState {
        pane_id: pane.pane_id(),
        cursor: SelectionPoint {
            row: cursor.y,
            col: cursor.x,
        },
        selection,
        search: SearchState::default(),
    });
    state.ui.mode = AppMode::Copy;
}

fn scroll_pane(state: &mut TuiState, pane_id: usize, lines: isize) {
    let Some(pane) = state.pane(pane_id) else {
        return;
    };
    let dims = pane.get_dimensions();
    let current = state.ui.scroll_offset(pane_id);
    let next = if lines >= 0 {
        current.saturating_add(lines as usize)
    } else {
        current.saturating_sub(lines.unsigned_abs())
    }
    .min(dims.scrollback_rows.saturating_sub(dims.viewport_rows));
    state.ui.set_scroll_offset(pane_id, next);
}

fn copy_selection(state: &mut TuiState) -> Result<()> {
    let selection = state
        .ui
        .copy
        .as_ref()
        .and_then(|copy| copy.selection.as_ref())
        .or(state.ui.selection.as_ref())
        .cloned()
        .context("nothing is selected")?;
    let pane = state
        .pane(selection.pane_id)
        .context("the selected pane no longer exists")?;
    let text = selection_text(&pane, &selection);
    state.clipboard.write_text(&text)?;
    // Terminal-mode selections have served their purpose once copied.
    // Retaining one makes a later Ctrl-C look like another copy request
    // instead of interrupting the foreground process. Copy mode owns its
    // separate selection and can keep it for continued navigation.
    state.ui.selection = None;
    if let Some(copy) = state.ui.copy.as_mut() {
        copy.selection = Some(selection);
    }
    state
        .ui
        .set_toast(format!("Copied {} characters", text.chars().count()));
    Ok(())
}

fn selection_text(pane: &Arc<dyn Pane>, selection: &TextSelection) -> String {
    let (start, end) = selection.ordered();
    let (actual_top, lines) = pane.get_lines(start.row..end.row.saturating_add(1));
    let mut result = String::new();
    for (offset, line) in lines.iter().enumerate() {
        let row = actual_top + offset as wezterm_term::StableRowIndex;
        if row < start.row || row > end.row {
            continue;
        }
        let first = if row == start.row { start.col } else { 0 };
        let last = if row == end.row {
            end.col.saturating_add(1)
        } else {
            line.len()
        }
        .min(line.len());
        if first < last {
            let text = line.columns_as_str(first..last);
            let wrapped = line
                .get_cell(last.saturating_sub(1))
                .is_some_and(|cell| cell.attrs().wrapped());
            if wrapped {
                result.push_str(&text);
            } else {
                result.push_str(text.trim_end());
                if row < end.row {
                    result.push('\n');
                }
            }
        } else if row < end.row {
            result.push('\n');
        }
    }
    result
}

fn paste_clipboard(state: &mut TuiState) -> Result<()> {
    let text = state.clipboard.read_text()?;
    let pane = state.active_pane().context("no live pane selected")?;
    pane.send_paste(&text)?;
    state.ui.set_scroll_offset(pane.pane_id(), 0);
    Ok(())
}

fn begin_search(state: &mut TuiState, backwards: bool) {
    let value = state
        .ui
        .copy
        .as_ref()
        .map(|copy| copy.search.query.clone())
        .unwrap_or_default();
    begin_prompt(
        state,
        if backwards {
            "Search backward"
        } else {
            "Search"
        },
        &value,
        PromptAction::Search { backwards },
    );
    state.ui.mode = AppMode::Search;
}

fn select_search_match(state: &mut TuiState, backwards: bool) {
    let Some(copy) = state.ui.copy.as_mut() else {
        return;
    };
    if copy.search.matches.is_empty() {
        return;
    }
    let len = copy.search.matches.len();
    let current = copy
        .search
        .current
        .unwrap_or(if backwards { 0 } else { len - 1 });
    let next = if backwards {
        (current + len - 1) % len
    } else {
        (current + 1) % len
    };
    copy.search.current = Some(next);
    let found = copy.search.matches[next];
    copy.cursor = SelectionPoint {
        row: found.start_y,
        col: found.start_x,
    };
    let pane_id = copy.pane_id;
    let _ = copy;
    if let Some(pane) = state.pane(pane_id) {
        let dims = pane.get_dimensions();
        let offset = dims.physical_top.saturating_sub(found.start_y).max(0) as usize;
        state.ui.set_scroll_offset(pane_id, offset);
    }
}

fn begin_prompt(state: &mut TuiState, title: &str, value: &str, action: PromptAction) {
    state.ui.prompt = Some(PromptState {
        title: title.into(),
        value: value.into(),
        action,
    });
    state.ui.context_menu = None;
    state.ui.mode = AppMode::Prompt;
}

fn begin_rename_node(state: &mut TuiState, key: TreeNodeKey) {
    let value = match &key {
        TreeNodeKey::Domain(name) => name.clone(),
        TreeNodeKey::Space {
            domain_name,
            space_id,
        } => state
            .model
            .domain(domain_name)
            .and_then(|snapshot| snapshot.spaces.iter().find(|space| space.id == *space_id))
            .map(|space| space.name.clone())
            .unwrap_or_default(),
        TreeNodeKey::Project {
            domain_name,
            project_id,
        } => state
            .model
            .domain(domain_name)
            .and_then(|snapshot| {
                snapshot
                    .projects
                    .iter()
                    .find(|project| project.id == *project_id)
            })
            .map(|project| project.name.clone())
            .unwrap_or_default(),
        TreeNodeKey::Thread(key) => state
            .model
            .row(key)
            .map(|row| row.thread.name.clone())
            .unwrap_or_default(),
    };
    begin_prompt(state, "Rename", &value, PromptAction::RenameNode(key));
}

fn begin_rename_tab(state: &mut TuiState) {
    let value = state
        .active_tab()
        .map(|(_, _, tab, _)| tab.get_title())
        .unwrap_or_default();
    begin_prompt(state, "Rename Tab", &value, PromptAction::RenameTab);
}

async fn submit_prompt(state: &mut TuiState) -> Result<()> {
    let Some(prompt) = state.ui.prompt.take() else {
        state.ui.mode = AppMode::Terminal;
        return Ok(());
    };
    let value = prompt.value.trim().to_string();
    if value.is_empty() {
        state.ui.mode = AppMode::Terminal;
        return Ok(());
    }
    state.ui.mode = AppMode::Terminal;

    match prompt.action {
        PromptAction::RenameTab => {
            let (_, _, tab, _) = state.active_tab().context("no live tab selected")?;
            tab.set_title(&value);
        }
        PromptAction::RenameNode(key) => rename_node(state, key, value).await?,
        PromptAction::CreateSpace { domain_name } => {
            create_space(state, domain_name, value).await?
        }
        PromptAction::CreateProject {
            domain_name,
            space_id,
        } => create_project(state, domain_name, space_id, value).await?,
        PromptAction::CreateThread {
            domain_name,
            project_id,
        } => create_thread(state, domain_name, project_id, value).await?,
        PromptAction::Search { backwards } => run_search(state, value, backwards).await?,
    }
    Ok(())
}

async fn rename_node(state: &mut TuiState, key: TreeNodeKey, name: String) -> Result<()> {
    let (domain_name, op) = match key {
        TreeNodeKey::Domain(_) => {
            anyhow::bail!("connection names are edited in ThinkTerm Settings")
        }
        TreeNodeKey::Space {
            domain_name,
            space_id,
        } => (domain_name, codec::TreeOp::RenameSpace { space_id, name }),
        TreeNodeKey::Project {
            domain_name,
            project_id,
        } => (
            domain_name,
            codec::TreeOp::RenameProject { project_id, name },
        ),
        TreeNodeKey::Thread(key) => (
            key.domain_name,
            codec::TreeOp::RenameThread {
                thread_id: key.thread_id,
                name,
                last_active_at: now_timestamp(),
            },
        ),
    };
    mutate_authoritative(state, &domain_name, "Renaming", vec![op]).await?;
    Ok(())
}

async fn create_space(state: &mut TuiState, domain_name: String, name: String) -> Result<()> {
    let space_id = new_id("space");
    let project_id = new_id("project");
    let thread_id = new_id("thread");
    let workspace = workspace_name(&project_id, &thread_id);
    mutate_authoritative(
        state,
        &domain_name,
        "Creating Space",
        vec![
            codec::TreeOp::CreateSpace {
                space_id: space_id.clone(),
                name,
            },
            codec::TreeOp::CreateProject {
                project_id: project_id.clone(),
                space_id,
                name: "Home".into(),
                path: "~".into(),
            },
            codec::TreeOp::CreateThread {
                thread_id: thread_id.clone(),
                project_id,
                name: "main".into(),
                workspace: Some(workspace),
                created_at: now_timestamp(),
            },
        ],
    )
    .await?;
    select_new_thread(state, &domain_name, &thread_id)?;
    ensure_selected_thread_live(state, true, None).await
}

async fn create_project(
    state: &mut TuiState,
    domain_name: String,
    space_id: String,
    name: String,
) -> Result<()> {
    let project_id = new_id("project");
    let thread_id = new_id("thread");
    let workspace = workspace_name(&project_id, &thread_id);
    mutate_authoritative(
        state,
        &domain_name,
        "Creating Project",
        vec![
            codec::TreeOp::CreateProject {
                project_id: project_id.clone(),
                space_id,
                name,
                path: "~".into(),
            },
            codec::TreeOp::CreateThread {
                thread_id: thread_id.clone(),
                project_id,
                name: "main".into(),
                workspace: Some(workspace),
                created_at: now_timestamp(),
            },
        ],
    )
    .await?;
    select_new_thread(state, &domain_name, &thread_id)?;
    ensure_selected_thread_live(state, true, None).await
}

async fn create_thread(
    state: &mut TuiState,
    domain_name: String,
    project_id: String,
    name: String,
) -> Result<()> {
    let thread_id = new_id("thread");
    let workspace = workspace_name(&project_id, &thread_id);
    mutate_authoritative(
        state,
        &domain_name,
        "Creating Thread",
        vec![codec::TreeOp::CreateThread {
            thread_id: thread_id.clone(),
            project_id,
            name,
            workspace: Some(workspace),
            created_at: now_timestamp(),
        }],
    )
    .await?;
    select_new_thread(state, &domain_name, &thread_id)?;
    ensure_selected_thread_live(state, true, None).await
}

fn select_new_thread(state: &mut TuiState, domain_name: &str, thread_id: &str) -> Result<()> {
    let key = state
        .model
        .rows()
        .iter()
        .find(|row| row.key.domain_name == domain_name && row.thread.id == thread_id)
        .map(|row| row.key.clone())
        .context("the server did not accept the new Thread")?;
    state.model.select_thread(&key);
    state.clear_selected_viewport_cache();
    Ok(())
}

async fn toggle_thread_pinned(state: &mut TuiState, key: ThreadKey) -> Result<()> {
    let row = state
        .model
        .row(&key)
        .context("the Thread no longer exists")?;
    let pinned = !row.thread.is_pinned;
    let domain_name = key.domain_name.clone();
    mutate_authoritative(
        state,
        &domain_name,
        if pinned {
            "Pinning Thread"
        } else {
            "Unpinning Thread"
        },
        vec![codec::TreeOp::SetThreadPinned {
            thread_id: key.thread_id,
            pinned,
            last_active_at: now_timestamp(),
        }],
    )
    .await?;
    Ok(())
}

async fn move_thread(state: &mut TuiState, key: ThreadKey, before: Option<String>) -> Result<()> {
    let project_id = state
        .model
        .row(&key)
        .context("the Thread no longer exists")?
        .project
        .id
        .clone();
    let domain_name = key.domain_name.clone();
    mutate_authoritative(
        state,
        &domain_name,
        "Moving Thread",
        vec![codec::TreeOp::MoveThreadBefore {
            project_id,
            thread_id: key.thread_id,
            before,
        }],
    )
    .await?;
    Ok(())
}

async fn run_search(state: &mut TuiState, query: String, backwards: bool) -> Result<()> {
    let pane_id = state
        .ui
        .copy
        .as_ref()
        .map(|copy| copy.pane_id)
        .or_else(|| state.active_pane().map(|pane| pane.pane_id()))
        .context("no live pane selected")?;
    let pane = state.pane(pane_id).context("the pane no longer exists")?;
    let dims = pane.get_dimensions();
    let end = dims.physical_top + dims.viewport_rows as wezterm_term::StableRowIndex;
    let matches = pane
        .search(
            Pattern::CaseInSensitiveString(query.clone()),
            dims.scrollback_top..end,
            None,
        )
        .await?;
    if state.ui.copy.is_none() {
        let cursor = pane.get_cursor_position();
        state.ui.copy = Some(CopyState {
            pane_id,
            cursor: SelectionPoint {
                row: cursor.y,
                col: cursor.x,
            },
            selection: None,
            search: SearchState::default(),
        });
    }
    let copy = state.ui.copy.as_mut().unwrap();
    copy.search = SearchState {
        query,
        current: if matches.is_empty() {
            None
        } else if backwards {
            Some(matches.len() - 1)
        } else {
            Some(0)
        },
        matches,
    };
    state.ui.mode = AppMode::Copy;
    if let Some(index) = copy.search.current {
        let found = copy.search.matches[index];
        copy.cursor = SelectionPoint {
            row: found.start_y,
            col: found.start_x,
        };
        let offset = dims.physical_top.saturating_sub(found.start_y).max(0) as usize;
        state.ui.set_scroll_offset(pane_id, offset);
    } else {
        state.ui.status = "No matches".into();
    }
    Ok(())
}

async fn mutate_authoritative(
    state: &mut TuiState,
    domain_name: &str,
    label: &str,
    ops: Vec<codec::TreeOp>,
) -> Result<ThinkTermSessionState> {
    let domain = state
        .domains
        .get(domain_name)
        .cloned()
        .with_context(|| format!("unknown mux connection {domain_name:?}"))?;
    if domain.state() != DomainState::Attached {
        anyhow::bail!("{domain_name}: offline; no client-side change was queued");
    }
    state.ui.pending = Some(PendingOperation {
        label: label.into(),
        domain_name: domain_name.into(),
    });
    state.ui.status = format!("{label} on server…");

    if let Err(err) = domain.mutate_thinkterm_tree(ops).await {
        if let Ok((generation, snapshot)) = fetch_current_session(&domain).await {
            if state.connection_generations.get(domain_name) == Some(&generation) {
                state.model.apply_snapshot(domain_name, snapshot);
            }
        }
        state.ui.pending = None;
        return Err(err).with_context(|| format!("{label} on {domain_name}"));
    }
    let (generation, snapshot) = fetch_current_session(&domain)
        .await
        .with_context(|| format!("refreshing {domain_name} after {label}"))?;
    if state.connection_generations.get(domain_name) != Some(&generation) {
        anyhow::bail!("{domain_name}: connection changed after {label}");
    }
    state.model.apply_snapshot(domain_name, snapshot.clone());
    state.ui.pending = None;
    state.ui.status.clear();
    Ok(snapshot)
}

async fn spawn_tab_for_selected_thread(state: &mut TuiState) -> Result<()> {
    let row = state
        .model
        .selected_row()
        .cloned()
        .context("no Thread selected")?;
    let domain = state
        .domains
        .get(&row.key.domain_name)
        .cloned()
        .context("the selected server is unavailable")?;
    if domain.state() != DomainState::Attached {
        anyhow::bail!("{}: offline", row.key.domain_name);
    }
    let workspace = row
        .thread
        .materialized_workspace_name
        .clone()
        .or(row.thread.planned_workspace_name.clone())
        .unwrap_or_else(|| workspace_name(&row.project.id, &row.thread.id));
    let existing_window = state
        .model
        .selected_tab()
        .and_then(|tab| domain.remote_to_local_window_id(tab.window_id));
    let mux = Mux::get();
    let tagged_window = existing_window.is_none().then(|| {
        mux.new_empty_window_for_domain(Some(workspace.clone()), None, Some(domain.domain_id()))
    });
    let window_id = existing_window.or_else(|| tagged_window.as_ref().map(|builder| **builder));
    let current_pane = state.active_pane().map(|pane| pane.pane_id());
    let size = spawn_size(state);
    let spawned = mux
        .spawn_tab_or_window(
            window_id,
            SpawnTabDomain::DomainName(row.key.domain_name.clone()),
            None,
            (!row.project.path.trim().is_empty()).then(|| row.project.path.clone()),
            size,
            current_pane,
            workspace.clone(),
            None,
        )
        .await;
    drop(tagged_window);
    let (tab, _, _) = spawned?;
    let remote_tab_id = domain
        .local_to_remote_tab_id(tab.tab_id())
        .context("the server did not map the new tab")?;
    mutate_authoritative(
        state,
        &row.key.domain_name,
        "Binding tab to Thread",
        vec![
            codec::TreeOp::SetThreadWorkspaceName {
                thread_id: row.thread.id.clone(),
                planned: None,
                materialized: Some(workspace),
            },
            codec::TreeOp::TouchThread {
                thread_id: row.thread.id,
                at: now_timestamp(),
            },
        ],
    )
    .await?;
    state.model.select_tab(remote_tab_id);
    Ok(())
}

fn spawn_size(state: &TuiState) -> TerminalSize {
    if let Some(pane) = state.active_pane() {
        let dims = pane.get_dimensions();
        return TerminalSize {
            rows: dims.viewport_rows.max(2),
            cols: dims.cols.max(2),
            pixel_width: dims.pixel_width,
            pixel_height: dims.pixel_height,
            dpi: dims.dpi,
        };
    }
    TerminalSize {
        rows: state.layout.content.height.max(2) as usize,
        cols: state.layout.content.width.max(2) as usize,
        pixel_width: 0,
        pixel_height: 0,
        dpi: 96,
    }
}

fn materialize_size(state: &TuiState) -> TerminalSize {
    TerminalSize {
        rows: state.layout.content.height.max(2) as usize,
        cols: state.layout.content.width.max(2) as usize,
        pixel_width: 0,
        pixel_height: 0,
        dpi: 96,
    }
}

/// Add a terminal to a pane's own stack: same rectangle, one more level-2 tab.
///
/// The size is taken from the pane already sitting there rather than recomputed
/// from the layout, because that pane's grid is what the new one inherits —
/// including the row its nav bar already took.
async fn new_pane_in_stack(state: &mut TuiState, pane_id: PaneId) -> Result<()> {
    let row = state
        .model
        .selected_row()
        .cloned()
        .context("no Thread selected")?;
    let dims = state
        .pane(pane_id)
        .context("no such pane")?
        .get_dimensions();
    let size = TerminalSize {
        rows: dims.viewport_rows,
        cols: dims.cols,
        pixel_width: dims.pixel_width,
        pixel_height: dims.pixel_height,
        dpi: dims.dpi,
    };
    Mux::get()
        .spawn_pane_in_stack(
            pane_id,
            SpawnTabDomain::DomainName(row.key.domain_name.clone()),
            None,
            (!row.project.path.trim().is_empty()).then(|| row.project.path.clone()),
            size,
        )
        .await?;
    state.clear_selected_viewport_cache();
    Ok(())
}

async fn split_active_pane(state: &mut TuiState, axis: SplitAxis) -> Result<()> {
    let row = state
        .model
        .selected_row()
        .cloned()
        .context("no Thread selected")?;
    let pane = state.active_pane().context("no live pane selected")?;
    let request = SplitRequest {
        direction: match axis {
            SplitAxis::Right => SplitDirection::Horizontal,
            SplitAxis::Down => SplitDirection::Vertical,
        },
        target_is_second: true,
        top_level: false,
        size: SplitSize::Percent(50),
    };
    Mux::get()
        .split_pane(
            pane.pane_id(),
            request,
            SplitSource::Spawn {
                command: None,
                command_dir: (!row.project.path.trim().is_empty())
                    .then(|| row.project.path.clone()),
            },
            SpawnTabDomain::DomainName(row.key.domain_name.clone()),
        )
        .await?;
    state.clear_selected_viewport_cache();
    let domain = state.domains[&row.key.domain_name].clone();
    let (generation, snapshot) = fetch_current_session(&domain).await?;
    if state.connection_generations.get(&row.key.domain_name) == Some(&generation) {
        state.model.apply_snapshot(&row.key.domain_name, snapshot);
    }
    Ok(())
}

fn begin_confirmation(state: &mut TuiState, action: DestructiveAction) {
    let (title, detail) = match &action {
        DestructiveAction::ClosePane { .. } => (
            "Close pane",
            "This ends the remote pane on the mux server for every client.".to_string(),
        ),
        DestructiveAction::CloseTab { tab_id } => {
            let count = remote_panes_for_tab(state, *tab_id).len();
            (
                "Close tab",
                format!("End {count} remote pane(s) on the mux server for every client?"),
            )
        }
        DestructiveAction::DeleteThread { key, end_sessions } => {
            let count = remote_panes_for_thread(state, key).len();
            (
                "Delete Thread",
                if *end_sessions {
                    format!("Delete this server-owned Thread and end {count} remote pane(s) for every client?")
                } else {
                    "Delete this server-owned Thread on every client? Its sessions will keep running.".into()
                },
            )
        }
        DestructiveAction::RemoveProject {
            domain_name,
            project_id,
            end_sessions,
        } => {
            let count = remote_panes_for_project(state, domain_name, project_id).len();
            (
                "Remove Project",
                if *end_sessions {
                    format!(
                        "Remove this Project from the server tree and end {count} remote pane(s)?"
                    )
                } else {
                    "Remove this Project from the server tree on every client?".into()
                },
            )
        }
        DestructiveAction::DeleteSpace {
            domain_name,
            space_id,
            end_sessions,
        } => {
            let count = remote_panes_for_space(state, domain_name, space_id).len();
            (
                "Delete Space",
                if *end_sessions {
                    format!("Delete this Space on the authoritative server and end {count} remote pane(s)?")
                } else {
                    "Delete this Space from the authoritative server on every client? Remote sessions keep running.".into()
                },
            )
        }
    };
    state.ui.context_menu = None;
    state.ui.confirmation = Some(ConfirmationState {
        title: title.into(),
        detail,
        action,
    });
    state.ui.mode = AppMode::Confirm;
}

async fn accept_confirmation(state: &mut TuiState) -> Result<()> {
    let Some(confirm) = state.ui.confirmation.take() else {
        state.ui.mode = AppMode::Terminal;
        return Ok(());
    };
    state.ui.mode = AppMode::Terminal;
    execute_destructive(state, confirm.action).await
}

async fn execute_destructive(state: &mut TuiState, action: DestructiveAction) -> Result<()> {
    match action {
        DestructiveAction::ClosePane { pane_id } => {
            end_local_remote_panes(state, &[pane_id]).await?
        }
        DestructiveAction::CloseTab { tab_id } => {
            let panes = remote_panes_for_tab(state, tab_id);
            end_local_remote_panes(state, &panes).await?;
        }
        DestructiveAction::DeleteThread { key, end_sessions } => {
            let panes = remote_panes_for_thread(state, &key);
            mutate_authoritative(
                state,
                &key.domain_name,
                "Deleting Thread",
                vec![codec::TreeOp::DeleteThread {
                    thread_id: key.thread_id,
                }],
            )
            .await?;
            if end_sessions {
                end_local_remote_panes(state, &panes).await?;
            }
        }
        DestructiveAction::RemoveProject {
            domain_name,
            project_id,
            end_sessions,
        } => {
            let panes = remote_panes_for_project(state, &domain_name, &project_id);
            mutate_authoritative(
                state,
                &domain_name,
                "Removing Project",
                vec![codec::TreeOp::RemoveProject { project_id }],
            )
            .await?;
            if end_sessions {
                end_local_remote_panes(state, &panes).await?;
            }
        }
        DestructiveAction::DeleteSpace {
            domain_name,
            space_id,
            end_sessions,
        } => {
            let panes = remote_panes_for_space(state, &domain_name, &space_id);
            mutate_authoritative(
                state,
                &domain_name,
                "Deleting Space",
                vec![codec::TreeOp::DeleteSpace { space_id }],
            )
            .await?;
            if end_sessions {
                end_local_remote_panes(state, &panes).await?;
            }
        }
    }
    Ok(())
}

fn remote_panes_for_tab(state: &TuiState, tab_id: TabId) -> Vec<usize> {
    let Some(row) = state.model.selected_row() else {
        return vec![];
    };
    let ids = row
        .thread
        .tabs
        .iter()
        .find(|tab| tab.tab_id == tab_id)
        .into_iter()
        .flat_map(|tab| tab.pane_ids.iter().copied());
    map_remote_panes(state.domains.get(&row.key.domain_name), ids)
}

fn remote_panes_for_thread(state: &TuiState, key: &ThreadKey) -> Vec<usize> {
    let Some(row) = state.model.row(key) else {
        return vec![];
    };
    map_remote_panes(
        state.domains.get(&key.domain_name),
        row.thread
            .tabs
            .iter()
            .flat_map(|tab| tab.pane_ids.iter().copied()),
    )
}

fn remote_panes_for_project(state: &TuiState, domain_name: &str, project_id: &str) -> Vec<usize> {
    let ids = state
        .model
        .rows()
        .iter()
        .filter(|row| row.key.domain_name == domain_name && row.project.id == project_id)
        .flat_map(|row| &row.thread.tabs)
        .flat_map(|tab| tab.pane_ids.iter().copied());
    map_remote_panes(state.domains.get(domain_name), ids)
}

fn remote_panes_for_space(state: &TuiState, domain_name: &str, space_id: &str) -> Vec<usize> {
    let ids = state
        .model
        .rows()
        .iter()
        .filter(|row| row.key.domain_name == domain_name && row.space.id == space_id)
        .flat_map(|row| &row.thread.tabs)
        .flat_map(|tab| tab.pane_ids.iter().copied());
    map_remote_panes(state.domains.get(domain_name), ids)
}

fn map_remote_panes(
    domain: Option<&Arc<ClientDomain>>,
    remote_ids: impl IntoIterator<Item = usize>,
) -> Vec<usize> {
    let Some(domain) = domain else {
        return vec![];
    };
    let mut result = remote_ids
        .into_iter()
        .filter_map(|remote| domain.remote_to_local_pane_id(remote))
        .collect::<Vec<_>>();
    result.sort_unstable();
    result.dedup();
    result
}

async fn end_local_remote_panes(state: &mut TuiState, pane_ids: &[usize]) -> Result<()> {
    let mut errors = Vec::new();
    for pane_id in pane_ids {
        let Some(pane) = state.pane(*pane_id) else {
            continue;
        };
        if let Some(remote) = pane.downcast_ref::<ClientPane>() {
            if let Err(err) = remote.kill_remote_and_wait().await {
                errors.push(format!("pane {pane_id}: {err:#}"));
            }
        } else {
            pane.kill();
        }
    }
    if !errors.is_empty() {
        anyhow::bail!(
            "some remote panes could not be ended: {}",
            errors.join("; ")
        );
    }
    let domain_names = state.domains.keys().cloned().collect::<Vec<_>>();
    for name in domain_names {
        if let Some(domain) = state.domains.get(&name).cloned() {
            if domain.state() == DomainState::Attached {
                if let Ok((generation, snapshot)) = fetch_current_session(&domain).await {
                    if state.connection_generations.get(&name) == Some(&generation) {
                        state.model.apply_snapshot(name, snapshot);
                    }
                }
            }
        }
    }
    Ok(())
}

async fn retry_domain(state: &mut TuiState, name: &str) -> Result<()> {
    let domain = if let Some(domain) = state.domains.get(name).cloned() {
        domain
    } else {
        let config = state
            .domain_configs
            .get(name)
            .cloned()
            .with_context(|| format!("unknown mux connection {name:?}"))?;
        let domain = Arc::new(ClientDomain::new(config));
        let mux_domain: Arc<dyn Domain> = domain.clone();
        Mux::get().add_domain(&mux_domain);
        if state
            .domains
            .values()
            .all(|domain| domain.state() != DomainState::Attached)
        {
            Mux::get().set_default_domain(&mux_domain);
        }
        state.domains.insert(name.to_string(), Arc::clone(&domain));
        domain
    };
    if domain.state() != DomainState::Attached {
        if let Some(item) = state
            .ui
            .connections
            .iter_mut()
            .find(|item| item.name == name)
        {
            item.status = ConnectionStatus::Connecting;
        }
        state.model.begin_connection_generation(name);
        if let Err(err) = domain
            .attach_with_ui(None, ConnectionUI::new_headless())
            .await
        {
            if let Some(item) = state
                .ui
                .connections
                .iter_mut()
                .find(|item| item.name == name)
            {
                item.status = ConnectionStatus::Failed;
                item.detail = format!("{err:#}");
            }
            return Err(err).with_context(|| format!("connecting to {name}"));
        }
        if let Err(err) = guard_against_same_server_nesting(&domain) {
            domain.perform_detach();
            state.connection_generations.remove(name);
            sync_connection_statuses(state);
            return Err(err);
        }
    }
    let (generation, snapshot) = fetch_current_session(&domain).await?;
    state
        .connection_generations
        .insert(name.to_string(), generation);
    state.model.begin_connection_generation(name);
    state.model.apply_snapshot(name, snapshot);
    if let Some(key) = state
        .model
        .rows()
        .iter()
        .filter(|row| row.key.domain_name == name)
        .find(|row| !row.thread.tabs.is_empty())
        .or_else(|| {
            state
                .model
                .rows()
                .iter()
                .find(|row| row.key.domain_name == name)
        })
        .map(|row| row.key.clone())
    {
        state.model.select_thread(&key);
    }
    sync_connection_statuses(state);
    state.ui.mode = AppMode::Terminal;
    ensure_selected_thread_live(state, true, Some(name)).await?;
    Ok(())
}

fn new_id(prefix: &str) -> String {
    format!("{prefix}-{}", Uuid::new_v4())
}

fn now_millis() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn now_timestamp() -> i64 {
    (now_millis() / 1000).min(i64::MAX as u128) as i64
}

fn workspace_name(project_id: &str, thread_id: &str) -> String {
    format!("thinkterm:{project_id}:{thread_id}")
}

fn terminal_size(cols: usize, rows: usize, screen: ScreenSize) -> TerminalSize {
    #[cfg(unix)]
    let (pixel_width, pixel_height) = (
        scale_total_pixels(screen.xpixel, screen.cols, cols),
        scale_total_pixels(screen.ypixel, screen.rows, rows),
    );
    #[cfg(not(unix))]
    let (pixel_width, pixel_height) = (
        screen.xpixel.saturating_mul(cols),
        screen.ypixel.saturating_mul(rows),
    );
    TerminalSize {
        rows,
        cols,
        pixel_width,
        pixel_height,
        dpi: 96,
    }
}

#[cfg(unix)]
fn scale_total_pixels(total_pixels: usize, source_cells: usize, target_cells: usize) -> usize {
    if total_pixels == 0 || source_cells == 0 {
        0
    } else {
        (total_pixels / source_cells).saturating_mul(target_cells)
    }
}

/// Append one decoded input event to `THINKTERM_LOG_INPUT`, if it is set.
///
/// A touchscreen terminal decides for itself what a swipe means, and the two
/// plausible answers — a wheel notch, or a button press that drags — reach this
/// program as entirely different events that it must treat differently. Which
/// one arrives is not something the code can be read to discover, so this
/// writes down what actually came in. Off unless the variable names a file, and
/// deliberately unbuffered: a session that has to be killed still leaves its
/// evidence on disk.
fn log_input_event(event: &InputEvent) {
    let Some(path) = std::env::var_os("THINKTERM_LOG_INPUT") else {
        return;
    };
    use std::io::Write;
    let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    else {
        return;
    };
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |since| since.as_secs_f64());
    // Formatted whole and written once: `writeln!` straight to the file splits
    // into several `write` calls, and the input threads then interleave inside
    // each other's lines. One append-mode write per line keeps them separable.
    let _ = file.write_all(format!("{stamp:.3} {event:?}\n").as_bytes());
}

fn handle_input(event: InputEvent, state: &mut TuiState) {
    log_input_event(&event);
    match event {
        InputEvent::Key(event) => handle_key(event, state),
        InputEvent::Mouse(event) => handle_mouse(event, state),
        InputEvent::PixelMouse(_) => {}
        InputEvent::Resized { .. } => {
            state.dirty = true;
            state.clear_selected_viewport_cache();
        }
        InputEvent::Paste(text) => {
            match state.ui.mode {
                AppMode::Prompt | AppMode::Search => {
                    if let Some(prompt) = state.ui.prompt.as_mut() {
                        prompt.value.push_str(&text);
                    }
                }
                AppMode::Terminal => {
                    if let Some(pane) = state.active_pane() {
                        if let Err(err) = pane.send_paste(&text) {
                            state.ui.status = format!("Paste: {err}");
                        }
                    }
                }
                _ => state.ui.status = "Paste is unavailable while an overlay is open".into(),
            }
            state.dirty = true;
        }
        InputEvent::Wake => {}
    }
}

fn handle_key(event: KeyEvent, state: &mut TuiState) {
    let modifiers = event.modifiers.remove_positional_mods();
    let key = event.key.normalize_shift_to_upper_case(modifiers);
    let is_prefix = key == KeyCode::Char('b') && modifiers.contains(Modifiers::CTRL);

    match state.ui.mode {
        AppMode::Prompt | AppMode::Search => {
            handle_prompt_key(key, state);
            return;
        }
        AppMode::Confirm => {
            match key {
                KeyCode::Enter | KeyCode::Char('y' | 'Y') => {
                    state.queue(Action::AcceptConfirmation)
                }
                KeyCode::Escape | KeyCode::Char('n' | 'N') => state.queue(Action::CancelPrompt),
                _ => {}
            }
            state.dirty = true;
            return;
        }
        AppMode::ContextMenu => {
            handle_context_menu_key(key, state);
            return;
        }
        AppMode::Help => {
            if matches!(key, KeyCode::Escape | KeyCode::Char('?')) {
                state.queue(Action::ToggleHelp);
            }
            return;
        }
        AppMode::Navigate => {
            handle_navigate_key(key, state);
            return;
        }
        AppMode::Resize => {
            handle_resize_key(key, state);
            return;
        }
        AppMode::Copy => {
            handle_copy_key(key, state);
            return;
        }
        AppMode::Connections => {
            match key {
                KeyCode::Escape => state.queue(Action::CloseOverlay),
                KeyCode::Char('q') => state.queue(Action::Detach),
                KeyCode::UpArrow | KeyCode::Char('k') => {
                    state.ui.connection_index = state.ui.connection_index.saturating_sub(1);
                }
                KeyCode::DownArrow | KeyCode::Char('j') => {
                    state.ui.connection_index = (state.ui.connection_index + 1)
                        .min(state.ui.connections.len().saturating_sub(1));
                }
                KeyCode::Enter | KeyCode::Char('r') => {
                    if let Some(item) = state
                        .ui
                        .connections
                        .get(state.ui.connection_index)
                        .filter(|item| item.connectable)
                    {
                        state.queue(Action::ConnectDomain(item.name.clone()));
                    }
                }
                KeyCode::Char('d') => {
                    if let Some(item) = state
                        .ui
                        .connections
                        .get(state.ui.connection_index)
                        .filter(|item| item.status == ConnectionStatus::Attached)
                    {
                        state.queue(Action::DisconnectDomain(item.name.clone()));
                    }
                }
                _ => {}
            }
            state.dirty = true;
            return;
        }
        AppMode::Settings => {
            match key {
                KeyCode::Escape => state.queue(Action::CloseOverlay),
                KeyCode::UpArrow | KeyCode::Char('k') => {
                    state.ui.settings_index = state.ui.settings_index.saturating_sub(1);
                }
                KeyCode::DownArrow | KeyCode::Char('j') => {
                    state.ui.settings_index = (state.ui.settings_index + 1).min(SETTINGS.len() - 1);
                }
                KeyCode::LeftArrow | KeyCode::Char('h') => {
                    state.queue(Action::AdjustSetting {
                        index: state.ui.settings_index,
                        delta: -1,
                    });
                }
                KeyCode::RightArrow | KeyCode::Char('l') | KeyCode::Enter | KeyCode::Char(' ') => {
                    state.queue(Action::AdjustSetting {
                        index: state.ui.settings_index,
                        delta: 1,
                    });
                }
                _ => {}
            }
            state.dirty = true;
            return;
        }
        AppMode::Prefix => {
            if state.frontend_surface_blocked() {
                state.ui.mode = AppMode::Terminal;
                state.ui.status = state
                    .frontend_overlay_message()
                    .map(|(_, hint)| hint)
                    .unwrap_or_default();
                state.dirty = true;
                return;
            }
            handle_prefix_key(key, modifiers, state);
            return;
        }
        AppMode::Terminal => {}
    }

    if state.frontend_surface_blocked() {
        // B deliberately does not let the keyboard steal control: there is no
        // safe way to distinguish a takeover gesture from text intended for
        // the still-hidden terminal.
        state.ui.status = state
            .frontend_overlay_message()
            .map(|(_, hint)| hint)
            .unwrap_or_default();
        state.dirty = true;
        return;
    }

    if is_prefix {
        state.ui.mode = AppMode::Prefix;
        state.ui.status.clear();
        state.dirty = true;
        return;
    }

    if should_copy_retained_selection(
        key,
        modifiers,
        state.settings.copy_on_select,
        state.ui.selection.as_ref(),
    ) {
        state.queue(Action::CopySelection);
        return;
    }
    send_key_to_pane(key, modifiers, state);
}

fn should_copy_retained_selection(
    key: KeyCode,
    modifiers: Modifiers,
    copy_on_select: bool,
    selection: Option<&TextSelection>,
) -> bool {
    !copy_on_select
        && matches!(key, KeyCode::Char('c' | 'C'))
        && (modifiers.contains(Modifiers::CTRL) || modifiers.contains(Modifiers::SUPER))
        && selection.is_some_and(|selection| selection.finalized)
}

fn handle_prefix_key(key: KeyCode, modifiers: Modifiers, state: &mut TuiState) {
    state.ui.mode = AppMode::Terminal;
    let action = match key {
        KeyCode::Char('b') if modifiers.contains(Modifiers::CTRL) => {
            send_key_to_pane(KeyCode::Char('b'), Modifiers::CTRL, state);
            Action::None
        }
        KeyCode::Char('d' | 'q') => Action::Detach,
        KeyCode::Char('b' | 's') => Action::ToggleSidebar,
        KeyCode::Char('?') => Action::ToggleHelp,
        KeyCode::Char('g' | 'w') => Action::OpenNavigator,
        KeyCode::Char('C') => Action::OpenConnections,
        KeyCode::Char(',') => Action::OpenSettings,
        KeyCode::Char('c') => Action::NewTab,
        KeyCode::Char('n') => Action::SelectRelativeTab(1),
        KeyCode::Char('p') => Action::SelectRelativeTab(-1),
        KeyCode::Char('o') => Action::SelectNextAttention,
        KeyCode::Tab => Action::CyclePane(1),
        KeyCode::UpArrow => Action::SelectRelativeThread(-1),
        KeyCode::DownArrow => Action::SelectRelativeThread(1),
        KeyCode::Char('h') | KeyCode::LeftArrow => Action::FocusPane(PaneDirection::Left),
        KeyCode::Char('j') => Action::FocusPane(PaneDirection::Down),
        KeyCode::Char('k') => Action::FocusPane(PaneDirection::Up),
        KeyCode::Char('l') | KeyCode::RightArrow => Action::FocusPane(PaneDirection::Right),
        KeyCode::Char('v') => Action::SplitPane(SplitAxis::Right),
        KeyCode::Char('-') | KeyCode::Subtract => Action::SplitPane(SplitAxis::Down),
        KeyCode::Char('x') => state
            .active_pane()
            .map(|pane| {
                Action::Confirm(DestructiveAction::ClosePane {
                    pane_id: pane.pane_id(),
                })
            })
            .unwrap_or(Action::None),
        KeyCode::Char('X') => state
            .model
            .selected_tab()
            .map(|tab| Action::Confirm(DestructiveAction::CloseTab { tab_id: tab.tab_id }))
            .unwrap_or(Action::None),
        KeyCode::Char('z') => Action::ToggleZoom,
        KeyCode::Char('r') => Action::EnterResize,
        KeyCode::Char('[') => Action::EnterCopyMode,
        KeyCode::Char(c @ '1'..='9') => {
            let index = c as usize - '1' as usize;
            state
                .model
                .tabs_for_selected_thread()
                .get(index)
                .map(|tab| Action::SelectTab(tab.tab_id))
                .unwrap_or(Action::None)
        }
        _ => {
            state.ui.status = "Unknown prefix command · Ctrl-b ? for help".into();
            Action::None
        }
    };
    state.queue(action);
    state.dirty = true;
}

fn handle_navigate_key(key: KeyCode, state: &mut TuiState) {
    let action = match key {
        KeyCode::Escape | KeyCode::Enter => Action::CloseOverlay,
        KeyCode::UpArrow | KeyCode::Char('k') => Action::SelectRelativeThread(-1),
        KeyCode::DownArrow | KeyCode::Char('j') => Action::SelectRelativeThread(1),
        KeyCode::LeftArrow | KeyCode::Char('h') => Action::FocusPane(PaneDirection::Left),
        KeyCode::RightArrow | KeyCode::Char('l') => Action::FocusPane(PaneDirection::Right),
        KeyCode::Char('n') => Action::SelectRelativeTab(1),
        KeyCode::Char('p') => Action::SelectRelativeTab(-1),
        KeyCode::Char('?') => Action::ToggleHelp,
        _ => Action::None,
    };
    state.queue(action);
    state.dirty = true;
}

fn handle_resize_key(key: KeyCode, state: &mut TuiState) {
    let action = match key {
        KeyCode::Escape | KeyCode::Enter => Action::CloseOverlay,
        KeyCode::LeftArrow | KeyCode::Char('h') => Action::ResizePane(PaneDirection::Left, 1),
        KeyCode::DownArrow | KeyCode::Char('j') => Action::ResizePane(PaneDirection::Down, 1),
        KeyCode::UpArrow | KeyCode::Char('k') => Action::ResizePane(PaneDirection::Up, 1),
        KeyCode::RightArrow | KeyCode::Char('l') => Action::ResizePane(PaneDirection::Right, 1),
        _ => Action::None,
    };
    state.queue(action);
    state.dirty = true;
}

fn handle_copy_key(key: KeyCode, state: &mut TuiState) {
    if state.ui.copy.is_none() {
        state.ui.mode = AppMode::Terminal;
        return;
    }
    let mut moved = false;
    let mut action = Action::None;
    {
        let copy = state.ui.copy.as_mut().unwrap();
        match key {
            KeyCode::Escape => action = Action::LeaveCopyMode,
            KeyCode::LeftArrow | KeyCode::Char('h') => {
                copy.cursor.col = copy.cursor.col.saturating_sub(1);
                moved = true;
            }
            KeyCode::RightArrow | KeyCode::Char('l') => {
                copy.cursor.col = copy.cursor.col.saturating_add(1);
                moved = true;
            }
            KeyCode::UpArrow | KeyCode::Char('k') => {
                copy.cursor.row = copy.cursor.row.saturating_sub(1);
                moved = true;
            }
            KeyCode::DownArrow | KeyCode::Char('j') => {
                copy.cursor.row = copy.cursor.row.saturating_add(1);
                moved = true;
            }
            KeyCode::PageUp => {
                action = Action::ScrollPane {
                    pane_id: copy.pane_id,
                    lines: 10,
                }
            }
            KeyCode::PageDown => {
                action = Action::ScrollPane {
                    pane_id: copy.pane_id,
                    lines: -10,
                }
            }
            KeyCode::End => {
                action = Action::ScrollToBottom {
                    pane_id: copy.pane_id,
                }
            }
            KeyCode::Char(' ') => {
                if let Some(selection) = copy.selection.as_mut() {
                    selection.head = copy.cursor;
                    selection.finalized = true;
                } else {
                    copy.selection = Some(TextSelection {
                        pane_id: copy.pane_id,
                        anchor: copy.cursor,
                        head: copy.cursor,
                        finalized: false,
                    });
                }
            }
            KeyCode::Char('y' | 'Y') | KeyCode::Enter => action = Action::CopySelection,
            KeyCode::Char('/') => action = Action::BeginSearch { backwards: false },
            KeyCode::Char('?') => action = Action::BeginSearch { backwards: true },
            KeyCode::Char('n') => action = Action::SearchNext { backwards: false },
            KeyCode::Char('N') => action = Action::SearchNext { backwards: true },
            _ => {}
        }
    }
    if moved {
        keep_copy_cursor_visible(state);
        if let Some(copy) = state.ui.copy.as_mut() {
            if let Some(selection) = copy
                .selection
                .as_mut()
                .filter(|selection| !selection.finalized)
            {
                selection.head = copy.cursor;
            }
        }
    }
    state.queue(action);
    state.dirty = true;
}

fn keep_copy_cursor_visible(state: &mut TuiState) {
    let Some(copy) = state.ui.copy.as_ref() else {
        return;
    };
    let pane_id = copy.pane_id;
    let cursor = copy.cursor;
    let Some(pane) = state.pane(pane_id) else {
        return;
    };
    let dims = pane.get_dimensions();
    let visible_rows = state
        .layout
        .panes
        .iter()
        .find(|pane| pane.pane_id == pane_id)
        .map(|pane| pane.rect.height as usize)
        .unwrap_or(dims.viewport_rows)
        .max(1);
    let max_row =
        dims.physical_top + dims.viewport_rows.saturating_sub(1) as wezterm_term::StableRowIndex;
    let max_offset = dims.scrollback_rows.saturating_sub(dims.viewport_rows);
    let (cursor, offset) = adjusted_copy_view(
        cursor,
        dims.cols,
        dims.scrollback_top,
        max_row,
        dims.physical_top,
        visible_rows,
        state.ui.scroll_offset(pane_id),
        max_offset,
    );
    if let Some(copy) = state.ui.copy.as_mut() {
        copy.cursor = cursor;
    }
    state.ui.set_scroll_offset(pane_id, offset);
}

#[allow(clippy::too_many_arguments)]
fn adjusted_copy_view(
    mut cursor: SelectionPoint,
    cols: usize,
    earliest_row: wezterm_term::StableRowIndex,
    latest_row: wezterm_term::StableRowIndex,
    physical_top: wezterm_term::StableRowIndex,
    visible_rows: usize,
    current_offset: usize,
    max_offset: usize,
) -> (SelectionPoint, usize) {
    cursor.row = cursor.row.clamp(earliest_row, latest_row);
    cursor.col = cursor.col.min(cols.saturating_sub(1));

    let visible_rows = visible_rows.max(1) as wezterm_term::StableRowIndex;
    let current_offset = current_offset.min(max_offset);
    let visible_top = physical_top
        .saturating_sub(current_offset as wezterm_term::StableRowIndex)
        .max(earliest_row);
    let desired_top = if cursor.row < visible_top {
        cursor.row
    } else if cursor.row >= visible_top + visible_rows {
        cursor.row - visible_rows + 1
    } else {
        visible_top
    };
    let offset = physical_top.saturating_sub(desired_top).max(0) as usize;
    let offset = offset.min(max_offset);
    let final_top = physical_top
        .saturating_sub(offset as wezterm_term::StableRowIndex)
        .max(earliest_row);
    let final_bottom = (final_top + visible_rows - 1).min(latest_row);
    cursor.row = cursor.row.clamp(final_top, final_bottom);
    (cursor, offset)
}

fn handle_prompt_key(key: KeyCode, state: &mut TuiState) {
    let action = match key {
        KeyCode::Escape => Action::CancelPrompt,
        KeyCode::Enter => Action::SubmitPrompt,
        KeyCode::Backspace => {
            if let Some(prompt) = state.ui.prompt.as_mut() {
                prompt.value.pop();
            }
            Action::None
        }
        KeyCode::Char(ch) if !ch.is_control() => {
            if let Some(prompt) = state.ui.prompt.as_mut() {
                prompt.value.push(ch);
            }
            Action::None
        }
        _ => Action::None,
    };
    state.queue(action);
    state.dirty = true;
}

fn handle_context_menu_key(key: KeyCode, state: &mut TuiState) {
    if state.ui.context_menu.is_none() {
        state.ui.mode = AppMode::Terminal;
        return;
    }
    let mut action = Action::None;
    let mut close = false;
    {
        let menu = state.ui.context_menu.as_mut().unwrap();
        match key {
            KeyCode::Escape => action = Action::CloseOverlay,
            KeyCode::UpArrow | KeyCode::Char('k') => {
                menu.selected = menu.selected.saturating_sub(1);
            }
            KeyCode::DownArrow | KeyCode::Char('j') => {
                menu.selected = (menu.selected + 1).min(menu.entries.len().saturating_sub(1));
            }
            KeyCode::Enter => {
                if let Some(entry) = menu
                    .entries
                    .get(menu.selected)
                    .filter(|entry| entry.enabled)
                {
                    action = entry.action.clone();
                }
                close = true;
            }
            _ => {}
        }
    }
    if close {
        state.ui.context_menu = None;
        state.ui.mode = AppMode::Terminal;
    }
    state.queue(action);
    state.dirty = true;
}

fn menu_entry(label: impl Into<String>, action: Action) -> MenuEntry {
    MenuEntry {
        label: label.into(),
        action,
        destructive: false,
        enabled: true,
    }
}

fn destructive_menu_entry(label: impl Into<String>, action: Action) -> MenuEntry {
    MenuEntry {
        label: label.into(),
        action,
        destructive: true,
        enabled: true,
    }
}

fn open_context_menu(state: &mut TuiState, x: u16, y: u16, target: Option<HitTarget>) {
    use crate::model::TreeNodeKey;

    let entries = match target {
        Some(HitTarget::Tree(TreeNodeKey::Domain(domain_name))) => vec![
            menu_entry(
                "New Space",
                Action::NewSpace {
                    domain_name: domain_name.clone(),
                },
            ),
            menu_entry("Retry connection", Action::RetryDomain(domain_name.clone())),
            menu_entry("Disconnect", Action::DisconnectDomain(domain_name)),
        ],
        Some(HitTarget::Tree(TreeNodeKey::Space {
            domain_name,
            space_id,
        })) => vec![
            menu_entry(
                "New Project",
                Action::NewProject {
                    domain_name: domain_name.clone(),
                    space_id: space_id.clone(),
                },
            ),
            menu_entry(
                "Rename Space",
                Action::RenameNode(TreeNodeKey::Space {
                    domain_name: domain_name.clone(),
                    space_id: space_id.clone(),
                }),
            ),
            destructive_menu_entry(
                "Delete from server",
                Action::Confirm(DestructiveAction::DeleteSpace {
                    domain_name: domain_name.clone(),
                    space_id: space_id.clone(),
                    end_sessions: false,
                }),
            ),
            destructive_menu_entry(
                "End sessions and delete",
                Action::Confirm(DestructiveAction::DeleteSpace {
                    domain_name,
                    space_id,
                    end_sessions: true,
                }),
            ),
        ],
        Some(HitTarget::Tree(TreeNodeKey::Project {
            domain_name,
            project_id,
        })) => vec![
            menu_entry(
                "New Thread",
                Action::NewThread {
                    domain_name: domain_name.clone(),
                    project_id: project_id.clone(),
                },
            ),
            menu_entry(
                "Rename Project",
                Action::RenameNode(TreeNodeKey::Project {
                    domain_name: domain_name.clone(),
                    project_id: project_id.clone(),
                }),
            ),
            destructive_menu_entry(
                "End sessions and remove",
                Action::Confirm(DestructiveAction::RemoveProject {
                    domain_name,
                    project_id,
                    end_sessions: true,
                }),
            ),
        ],
        Some(HitTarget::Tree(TreeNodeKey::Thread(key))) => {
            let pinned = state
                .model
                .row(&key)
                .is_some_and(|row| row.thread.is_pinned);
            vec![
                menu_entry("Open", Action::SelectThread(key.clone())),
                menu_entry(
                    "New Thread",
                    Action::NewThread {
                        domain_name: key.domain_name.clone(),
                        project_id: state
                            .model
                            .row(&key)
                            .map(|row| row.project.id.clone())
                            .unwrap_or_default(),
                    },
                ),
                menu_entry(
                    "Rename Thread",
                    Action::RenameNode(TreeNodeKey::Thread(key.clone())),
                ),
                menu_entry(
                    if pinned { "Unpin" } else { "Pin" },
                    Action::ToggleThreadPinned(key.clone()),
                ),
                destructive_menu_entry(
                    "End session and delete",
                    Action::Confirm(DestructiveAction::DeleteThread {
                        key,
                        end_sessions: true,
                    }),
                ),
            ]
        }
        Some(HitTarget::Tab(tab_id)) => vec![
            menu_entry("Open", Action::SelectTab(tab_id)),
            menu_entry("New Tab", Action::NewTab),
            menu_entry("Rename Tab", Action::RenameTab),
            destructive_menu_entry(
                "Close Tab",
                Action::Confirm(DestructiveAction::CloseTab { tab_id }),
            ),
        ],
        Some(HitTarget::Pane(pane_id)) => {
            if let (Some((_, _, tab, _)), Some(pane)) = (state.active_tab(), state.pane(pane_id)) {
                tab.set_active_pane(&pane);
            }
            let mut copy = menu_entry("Copy", Action::CopySelection);
            copy.enabled = state
                .ui
                .selection
                .as_ref()
                .is_some_and(|selection| selection.pane_id == pane_id && selection.finalized);
            vec![
                copy,
                menu_entry("Paste", Action::PasteClipboard),
                menu_entry("Split Right", Action::SplitPane(SplitAxis::Right)),
                menu_entry("Split Down", Action::SplitPane(SplitAxis::Down)),
                menu_entry("Zoom", Action::ToggleZoom),
                destructive_menu_entry(
                    "Close Pane",
                    Action::Confirm(DestructiveAction::ClosePane { pane_id }),
                ),
            ]
        }
        _ => Vec::new(),
    };

    if entries.is_empty() {
        state.ui.context_menu = None;
        state.ui.mode = AppMode::Terminal;
        return;
    }
    state.ui.context_menu = Some(ContextMenuState {
        anchor: (x, y),
        selected: 0,
        entries,
    });
    state.ui.mode = AppMode::ContextMenu;
}

fn open_main_menu(state: &mut TuiState, x: u16, y: u16) {
    let current_mode = state
        .active_frontend_access()
        .map(|state| state.mode)
        .unwrap_or(codec::FrontendAccessMode::Handoff);
    state.ui.context_menu = Some(ContextMenuState {
        anchor: (x, y),
        selected: 0,
        entries: vec![
            menu_entry("New Tab", Action::NewTab),
            menu_entry("Split Right", Action::SplitPane(SplitAxis::Right)),
            menu_entry("Split Down", Action::SplitPane(SplitAxis::Down)),
            menu_entry("Copy Mode", Action::EnterCopyMode),
            menu_entry("Paste", Action::PasteClipboard),
            menu_entry("Connections", Action::OpenConnections),
            menu_entry(
                if current_mode == codec::FrontendAccessMode::TmuxLatest {
                    "✓ A · Shared (tmux-like)"
                } else {
                    "A · Shared (tmux-like)"
                },
                Action::SetFrontendAccessMode(codec::FrontendAccessMode::TmuxLatest),
            ),
            menu_entry(
                if current_mode == codec::FrontendAccessMode::Handoff {
                    "✓ B · Handoff (exclusive)"
                } else {
                    "B · Handoff (exclusive)"
                },
                Action::SetFrontendAccessMode(codec::FrontendAccessMode::Handoff),
            ),
            menu_entry("TUI Settings", Action::OpenSettings),
            menu_entry("Toggle Sidebar", Action::ToggleSidebar),
            menu_entry("Help", Action::ToggleHelp),
            menu_entry("Detach", Action::Detach),
        ],
    });
    state.ui.mode = AppMode::ContextMenu;
}

fn activate_context_menu_entry(state: &mut TuiState, index: usize) {
    let action = state
        .ui
        .context_menu
        .as_ref()
        .and_then(|menu| menu.entries.get(index))
        .filter(|entry| entry.enabled)
        .map(|entry| entry.action.clone());
    state.ui.context_menu = None;
    state.ui.mode = AppMode::Terminal;
    if let Some(action) = action {
        state.queue(action);
    }
}

fn send_key_to_pane(key: KeyCode, modifiers: Modifiers, state: &mut TuiState) {
    if let Some(pane) = state.active_pane() {
        if let Err(err) = pane.key_down(key, modifiers) {
            state.ui.status = format!("Input: {err}");
        } else {
            state.ui.status.clear();
            state.ui.set_scroll_offset(pane.pane_id(), 0);
        }
    } else {
        state.ui.status = "No live pane selected".to_string();
    }
    state.dirty = true;
}

/// Where one wheel notch over a pane belongs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WheelRouting {
    /// The program asked for mouse events, so it gets the notch itself.
    Pane,
    /// Travel through the pane's scrollback.
    Scrollback,
    /// Translate the notch into the arrow keys a full-screen program is
    /// already listening for.
    ArrowKeys,
}

/// The alternate screen keeps no scrollback to travel through, so a notch spent
/// there would otherwise do nothing whatsoever. Spending it on arrow keys is
/// what makes `less` and `git log` readable from a touchscreen, which has no
/// keyboard to offer instead. Shift, and an existing scroll position, both mean
/// the reader deliberately left the live screen, so they keep the scrollback.
fn wheel_routing(
    pane_wants_mouse: bool,
    alt_screen: bool,
    shifted: bool,
    scroll_offset: usize,
) -> WheelRouting {
    if shifted || scroll_offset != 0 {
        WheelRouting::Scrollback
    } else if pane_wants_mouse {
        WheelRouting::Pane
    } else if alt_screen {
        WheelRouting::ArrowKeys
    } else {
        WheelRouting::Scrollback
    }
}

fn handle_mouse(event: TermwizMouseEvent, state: &mut TuiState) {
    let (x, y) = mouse_cell_coordinates(&event);
    let prior = state.last_mouse_buttons.clone();
    let current = event.mouse_buttons.clone();
    let left_pressed = current.contains(MouseButtons::LEFT) && !prior.contains(MouseButtons::LEFT);
    let left_released = !current.contains(MouseButtons::LEFT) && prior.contains(MouseButtons::LEFT);
    let right_pressed =
        current.contains(MouseButtons::RIGHT) && !prior.contains(MouseButtons::RIGHT);
    let middle_pressed =
        current.contains(MouseButtons::MIDDLE) && !prior.contains(MouseButtons::MIDDLE);
    let shifted = event.modifiers.contains(Modifiers::SHIFT);
    let wheel =
        current.contains(MouseButtons::VERT_WHEEL) || current.contains(MouseButtons::HORZ_WHEEL);

    let pointer_release = !current
        .intersects(MouseButtons::LEFT | MouseButtons::RIGHT | MouseButtons::MIDDLE)
        && prior.intersects(MouseButtons::LEFT | MouseButtons::RIGHT | MouseButtons::MIDDLE);
    if state.handoff_consumed_press && pointer_release {
        state.handoff_consumed_press = false;
        state.last_mouse_buttons = current;
        state.dirty = true;
        return;
    }

    if modal_mouse_input(state, x, y, left_pressed) {
        state.last_mouse_buttons = current;
        state.dirty = true;
        return;
    }

    let terminal_surface = state.layout.content.contains((x, y).into());
    let takeover_gesture = left_pressed || right_pressed || middle_pressed || wheel;
    if state.ui.mode == AppMode::Terminal && terminal_surface && state.frontend_surface_blocked() {
        if takeover_gesture && state.frontend_takeover_claimable() {
            state.handoff_consumed_press = left_pressed || right_pressed || middle_pressed;
            state.queue(Action::ClaimFrontendAccess);
        }
        state.last_mouse_buttons = current;
        state.dirty = true;
        return;
    }
    if state.ui.mode == AppMode::Terminal && terminal_surface && takeover_gesture {
        state.queue(Action::ClaimFrontendAccess);
    }

    if let Some(pane_id) = state.forwarded_mouse_pane {
        forward_mouse_to_pane(event, x, y, Some(pane_id), state);
        if first_button(&current).is_none() {
            state.forwarded_mouse_pane = None;
        }
        state.last_mouse_buttons = current;
        state.dirty = true;
        return;
    }

    if (right_pressed && shifted) || middle_pressed {
        if let Some(pane_id) = state.layout.pane_at(x, y).map(|pane| pane.pane_id) {
            state.forwarded_mouse_pane = Some(pane_id);
            forward_mouse_to_pane(event, x, y, Some(pane_id), state);
        }
        state.last_mouse_buttons = current;
        state.dirty = true;
        return;
    }

    if wheel {
        if state
            .layout
            .sidebar_body
            .is_some_and(|body| body.contains((x, y).into()))
        {
            let amount = state.settings.scroll_lines.max(1);
            if current.contains(MouseButtons::WHEEL_POSITIVE) {
                state.ui.sidebar_scroll = state.ui.sidebar_scroll.saturating_sub(amount);
            } else {
                state.ui.sidebar_scroll = state.ui.sidebar_scroll.saturating_add(amount);
            }
            state.last_mouse_buttons = current;
            state.dirty = true;
            return;
        }
        if let Some(pane_view) = state.layout.pane_at(x, y) {
            let pane_id = pane_view.pane_id;
            if let Some(pane) = state.pane(pane_id) {
                let routing = wheel_routing(
                    state.settings.mouse && pane.is_mouse_grabbed(),
                    pane.is_alt_screen_active(),
                    shifted,
                    state.ui.scroll_offset(pane_id),
                );
                let vertical = current.contains(MouseButtons::VERT_WHEEL);
                let up = current.contains(MouseButtons::WHEEL_POSITIVE);
                match routing {
                    WheelRouting::Pane => {
                        forward_mouse_to_pane(event.clone(), x, y, Some(pane_id), state);
                    }
                    WheelRouting::ArrowKeys if vertical => {
                        let key = if up {
                            KeyCode::UpArrow
                        } else {
                            KeyCode::DownArrow
                        };
                        for _ in 0..state.settings.scroll_lines.max(1) {
                            if pane.key_down(key, Modifiers::NONE).is_err() {
                                break;
                            }
                        }
                    }
                    WheelRouting::Scrollback if vertical => {
                        let lines = state.settings.scroll_lines as isize;
                        state.queue(Action::ScrollPane {
                            pane_id,
                            lines: if up { lines } else { -lines },
                        });
                    }
                    _ => {}
                }
            }
        }
        state.last_mouse_buttons = current;
        state.dirty = true;
        return;
    }

    if right_pressed && !shifted {
        let target = state.layout.hit(x, y).cloned();
        open_context_menu(state, x, y, target);
        state.last_mouse_buttons = current;
        state.dirty = true;
        return;
    }

    if left_pressed {
        let target = state.layout.hit(x, y).cloned();
        match target {
            Some(HitTarget::SidebarToggle) => state.queue(Action::ToggleSidebar),
            Some(HitTarget::TabControl(control)) => match control {
                TabBarControl::Menu => open_main_menu(state, x, y),
                TabBarControl::NewTab => state.queue(Action::NewTab),
            },
            // Closing a tab takes the same confirmation the menu entry does.
            Some(HitTarget::CloseTab(tab_id)) => {
                state.queue(Action::Confirm(DestructiveAction::CloseTab { tab_id }))
            }
            Some(HitTarget::Detach) => state.queue(Action::Detach),
            Some(HitTarget::Attention) => state.queue(Action::SelectNextAttention),
            Some(HitTarget::OpenSettings) => state.queue(Action::OpenSettings),
            Some(HitTarget::NewThread) => {
                if let Some(row) = state.model.selected_row() {
                    state.queue(Action::NewThread {
                        domain_name: row.key.domain_name.clone(),
                        project_id: row.project.id.clone(),
                    });
                }
            }
            // The heading has no space of its own, so the new project joins the
            // one the selected thread is already in — which is the space the
            // tree is showing you.
            Some(HitTarget::NewProject) => {
                if let Some(row) = state.model.selected_row() {
                    state.queue(Action::NewProject {
                        domain_name: row.key.domain_name.clone(),
                        space_id: row.space.id.clone(),
                    });
                }
            }
            Some(HitTarget::TreeAction(key, action)) => tree_action(state, x, y, key, action),
            Some(HitTarget::PaneNavTab(pane_id)) => activate_pane_in_stack(state, pane_id),
            // Closing a pane takes the same confirmation the menu entry does.
            Some(HitTarget::PaneNavClose(pane_id)) => {
                state.queue(Action::Confirm(DestructiveAction::ClosePane { pane_id }))
            }
            Some(HitTarget::PaneTool(pane_id, tool)) => pane_tool(state, pane_id, tool),
            Some(HitTarget::Scrollbar(pane_id)) => {
                scroll_to_scrollbar_position(state, pane_id, y);
                state.drag = Some(DragState::Scrollbar { pane_id });
            }
            Some(HitTarget::TreeToggle(key)) => state.ui.toggle_collapsed(key),
            Some(HitTarget::Tree(crate::model::TreeNodeKey::Thread(key))) => {
                state.queue(Action::SelectThread(key.clone()));
                state.drag = Some(DragState::TreeNode {
                    key: TreeNodeKey::Thread(key),
                });
            }
            Some(HitTarget::Tree(key)) => {
                state.drag = Some(DragState::TreeNode { key });
            }
            Some(HitTarget::Tab(tab_id)) => state.queue(Action::SelectTab(tab_id)),
            Some(HitTarget::Pane(pane_id)) => {
                if let Some(pane) = state.pane(pane_id) {
                    if state.settings.mouse && pane.is_mouse_grabbed() && !shifted {
                        state.forwarded_mouse_pane = Some(pane_id);
                        forward_mouse_to_pane(event.clone(), x, y, Some(pane_id), state);
                    } else if let Some(point) = pane_point(state, pane_id, x, y) {
                        state.queue(Action::FocusPaneId(pane_id));
                        state.ui.selection = Some(TextSelection {
                            pane_id,
                            anchor: point,
                            head: point,
                            finalized: false,
                        });
                        state.drag = Some(DragState::Selection { pane_id });
                    }
                }
            }
            Some(HitTarget::SidebarResize) => state.drag = Some(DragState::SidebarResize),
            Some(HitTarget::Split(split_index)) => {
                state.drag = Some(DragState::SplitResize {
                    split_index,
                    last_x: x,
                    last_y: y,
                });
            }
            Some(HitTarget::ContextMenu(index)) => activate_context_menu_entry(state, index),
            Some(HitTarget::Connection(index)) => {
                state.ui.connection_index = index;
                if let Some(item) = state
                    .ui
                    .connections
                    .get(index)
                    .filter(|item| item.connectable)
                {
                    state.queue(Action::ConnectDomain(item.name.clone()));
                }
            }
            Some(HitTarget::DialogConfirm) => state.queue(Action::AcceptConfirmation),
            Some(HitTarget::DialogCancel) => state.queue(Action::CancelPrompt),
            Some(HitTarget::Setting(_)) => {}
            None => {}
        }
    } else if current.contains(MouseButtons::LEFT) {
        match state.drag.clone() {
            Some(DragState::Selection { pane_id }) => {
                if let Some(point) = pane_point(state, pane_id, x, y) {
                    if let Some(selection) = state.ui.selection.as_mut() {
                        selection.head = point;
                    }
                }
            }
            Some(DragState::Scrollbar { pane_id }) => {
                scroll_to_scrollbar_position(state, pane_id, y);
            }
            Some(DragState::SidebarResize) => {
                state.ui.sidebar_width = x.saturating_sub(state.layout.screen.x).clamp(18, 36);
                state.clear_selected_viewport_cache();
            }
            Some(DragState::TreeNode { .. }) => {}
            Some(DragState::SplitResize {
                split_index,
                last_x,
                last_y,
            }) => {
                if state.active_tab().is_some() {
                    if let Some(split) = state
                        .layout
                        .splits
                        .iter()
                        .find(|split| split.index == split_index)
                    {
                        let delta = match split.direction {
                            SplitDirection::Horizontal => x as isize - last_x as isize,
                            SplitDirection::Vertical => y as isize - last_y as isize,
                        };
                        if delta != 0 {
                            state.queue(Action::ResizeSplit { split_index, delta });
                        }
                    }
                }
                state.drag = Some(DragState::SplitResize {
                    split_index,
                    last_x: x,
                    last_y: y,
                });
            }
            None => {
                if state.layout.content.contains((x, y).into()) {
                    forward_mouse_to_pane(event.clone(), x, y, None, state);
                }
            }
        }
    }

    if left_released {
        if matches!(state.drag, Some(DragState::Selection { .. })) {
            let copy = finish_pointer_selection(&mut state.ui.selection);
            if copy && state.settings.copy_on_select {
                state.queue(Action::CopySelection);
            }
        } else if let Some(DragState::TreeNode { key }) = state.drag.clone() {
            if let Some(HitTarget::Tree(target)) = state.layout.hit(x, y).cloned() {
                if let Some(action) = tree_reorder_action(state, key, target) {
                    state.queue(action);
                }
            }
        } else if matches!(state.drag, Some(DragState::SidebarResize)) {
            state.settings.sidebar_width = state.ui.sidebar_width;
            if let Err(err) = save_tui_settings(state) {
                state.ui.status = format!("Saving TUI settings: {err:#}");
            }
        } else if state.layout.content.contains((x, y).into()) {
            forward_mouse_to_pane(event.clone(), x, y, None, state);
        }
        state.drag = None;
    }
    state.last_mouse_buttons = current;
    state.dirty = true;
}

/// How far along one row a pointer must travel before it is selecting rather
/// than pointing. A mouse lands on the cell it was aimed at; a fingertip covers
/// several and reports whichever the touchscreen picked, so a tap that meant
/// "focus this pane" arrives as a one- or two-cell drag. With copy-on-select
/// enabled that tap would take the clipboard with it.
const SELECTION_DRAG_THRESHOLD: usize = 3;

/// A click focuses a pane; only deliberate movement creates a text selection.
///
/// Any movement across rows counts, however small — reaching a different line
/// is unambiguous. Within one row the pointer has to cover
/// [`SELECTION_DRAG_THRESHOLD`] cells, which is more than a fingertip's slop
/// and less than any selection worth making.
fn finish_pointer_selection(selection: &mut Option<TextSelection>) -> bool {
    let Some(current) = selection.as_mut() else {
        return false;
    };
    let same_row = current.anchor.row == current.head.row;
    let travelled = current.anchor.col.abs_diff(current.head.col);
    if same_row && travelled < SELECTION_DRAG_THRESHOLD {
        *selection = None;
        return false;
    }
    current.finalized = true;
    true
}

/// Put the pane where the scrollbar was grabbed.
///
/// The bar maps the whole scrollback onto its own height, so the row a finger
/// lands on names a position directly — the same gesture works as a jump and,
/// held, as a drag.
fn scroll_to_scrollbar_position(state: &mut TuiState, pane_id: PaneId, y: u16) {
    let Some(bar) = state
        .layout
        .panes
        .iter()
        .find(|pane| pane.pane_id == pane_id)
        .and_then(|pane| pane.scrollbar)
    else {
        return;
    };
    let Some(pane) = state.pane(pane_id) else {
        return;
    };
    let dims = pane.get_dimensions();
    let scrollable = dims.scrollback_rows.saturating_sub(dims.viewport_rows);
    if scrollable == 0 || bar.height == 0 {
        return;
    }
    // The top of the bar is the oldest line, so distance from the bottom is how
    // far back the view has been pulled.
    let from_bottom = bar
        .bottom()
        .saturating_sub(1)
        .saturating_sub(y.min(bar.bottom() - 1));
    let offset = (from_bottom as usize * scrollable) / bar.height.max(1) as usize;
    state.ui.set_scroll_offset(pane_id, offset.min(scrollable));
    state.dirty = true;
}

/// Run the command a tree row's own button stands for.
///
/// Every arm here forwards to an `Action` that the row's context menu already
/// offered; the button is a second doorway to it, not a second implementation.
/// `Menu` opens that very menu, so anything not worth its own button stays one
/// tap away.
/// Show a pane that is stacked behind the one currently drawn in its place.
///
/// The server keeps its own idea of which pane in a stack is showing, and the
/// next resync would flip a purely local change back to it — so the change has
/// to be sent as well. The GUI mirrors it for the same reason.
fn activate_pane_in_stack(state: &mut TuiState, pane_id: PaneId) {
    let mux = Mux::get();
    if let Err(err) = mux.activate_pane_in_stack(pane_id) {
        state.ui.status = format!("Show pane: {err:#}");
        return;
    }
    if let Some(pane) = mux.get_pane(pane_id) {
        if let Some(client_pane) = pane.downcast_ref::<ClientPane>() {
            client_pane.activate_in_stack_on_server();
        }
    }
    state.queue(Action::FocusPaneId(pane_id));
}

/// Every pane tool acts on "the" pane, so the pane whose button was pressed has
/// to become that pane first. Two actions rather than one, drained in order.
fn pane_tool(state: &mut TuiState, pane_id: PaneId, tool: PaneTool) {
    state.queue(Action::FocusPaneId(pane_id));
    state.queue(match tool {
        PaneTool::Zoom { .. } => Action::ToggleZoom,
        PaneTool::SplitRight => Action::SplitPane(SplitAxis::Right),
        PaneTool::SplitDown => Action::SplitPane(SplitAxis::Down),
        PaneTool::NewTab => Action::NewPaneInStack(pane_id),
    });
}

fn tree_action(state: &mut TuiState, x: u16, y: u16, key: TreeNodeKey, action: TreeAction) {
    match (action, key) {
        (TreeAction::Menu, key) => open_context_menu(state, x, y, Some(HitTarget::Tree(key))),
        (
            TreeAction::Add,
            TreeNodeKey::Space {
                domain_name,
                space_id,
            },
        ) => state.queue(Action::NewProject {
            domain_name,
            space_id,
        }),
        (
            TreeAction::Add,
            TreeNodeKey::Project {
                domain_name,
                project_id,
            },
        ) => state.queue(Action::NewThread {
            domain_name,
            project_id,
        }),
        _ => {}
    }
}

/// Modal overlays own the entire screen, not just their visible buttons. This
/// prevents clicks and scrolling outside a dialog from reaching hidden panes.
fn modal_mouse_input(state: &mut TuiState, x: u16, y: u16, left_pressed: bool) -> bool {
    match state.ui.mode {
        AppMode::ContextMenu => {
            if left_pressed {
                match state.layout.hit(x, y).cloned() {
                    Some(HitTarget::ContextMenu(index)) => {
                        activate_context_menu_entry(state, index)
                    }
                    _ => state.queue(Action::CloseOverlay),
                }
            }
            true
        }
        AppMode::Confirm => {
            if left_pressed {
                match state.layout.hit(x, y) {
                    Some(HitTarget::DialogConfirm) => state.queue(Action::AcceptConfirmation),
                    Some(HitTarget::DialogCancel) => state.queue(Action::CancelPrompt),
                    _ => {}
                }
            }
            true
        }
        AppMode::Connections => {
            if left_pressed {
                if let Some(HitTarget::Connection(index)) = state.layout.hit(x, y).cloned() {
                    state.ui.connection_index = index;
                    if let Some(item) = state
                        .ui
                        .connections
                        .get(index)
                        .filter(|item| item.connectable)
                    {
                        state.queue(Action::ConnectDomain(item.name.clone()));
                    }
                }
            }
            true
        }
        AppMode::Settings => {
            if left_pressed {
                match state.layout.hit(x, y).cloned() {
                    Some(HitTarget::Setting(index)) => {
                        state.ui.settings_index = index;
                        state.queue(Action::AdjustSetting { index, delta: 1 });
                    }
                    _ => state.queue(Action::CloseOverlay),
                }
            }
            true
        }
        AppMode::Help => {
            if left_pressed {
                state.queue(Action::ToggleHelp);
            }
            true
        }
        AppMode::Prompt | AppMode::Search => true,
        _ => false,
    }
}

fn tree_reorder_action(
    state: &TuiState,
    source: TreeNodeKey,
    target: TreeNodeKey,
) -> Option<Action> {
    match (source, target) {
        (TreeNodeKey::Thread(source), TreeNodeKey::Thread(target)) => {
            let source_row = state.model.row(&source)?;
            let target_row = state.model.row(&target)?;
            (source.domain_name == target.domain_name
                && source_row.project.id == target_row.project.id
                && source != target)
                .then_some(Action::MoveThread {
                    key: source,
                    before: Some(target.thread_id),
                })
        }
        (
            TreeNodeKey::Project {
                domain_name: source_domain,
                project_id: source_project,
            },
            TreeNodeKey::Project {
                domain_name: target_domain,
                project_id: target_project,
            },
        ) => {
            let snapshot = state.model.domain(&source_domain)?;
            let source = snapshot
                .projects
                .iter()
                .find(|item| item.id == source_project)?;
            let target = snapshot
                .projects
                .iter()
                .find(|item| item.id == target_project)?;
            (source_domain == target_domain
                && source.space_id == target.space_id
                && source_project != target_project)
                .then(|| Action::MoveProject {
                    domain_name: source_domain,
                    space_id: source.space_id.clone(),
                    project_id: source_project,
                    before: Some(target_project),
                })
        }
        _ => None,
    }
}

/// Unix terminals report SGR mouse cells using the protocol's one-based
/// coordinates. Ratatui rectangles and mux pane positions are zero-based.
/// The Windows console backend already supplies zero-based coordinates.
fn mouse_cell_coordinates(event: &TermwizMouseEvent) -> (u16, u16) {
    #[cfg(unix)]
    {
        (event.x.saturating_sub(1), event.y.saturating_sub(1))
    }
    #[cfg(not(unix))]
    {
        (event.x, event.y)
    }
}

fn forward_mouse_to_pane(
    event: TermwizMouseEvent,
    screen_x: u16,
    screen_y: u16,
    pane_hint: Option<PaneId>,
    state: &mut TuiState,
) {
    let Some((_, _, tab, _)) = state.active_tab() else {
        return;
    };
    let pane_view = pane_hint
        .and_then(|pane_id| {
            state
                .layout
                .panes
                .iter()
                .find(|pane| pane.pane_id == pane_id)
        })
        .or_else(|| state.layout.pane_at(screen_x, screen_y))
        .cloned();
    let Some(pane_view) = pane_view else {
        return;
    };
    let Some(pane) = state.pane(pane_view.pane_id) else {
        return;
    };

    let prior = &state.last_mouse_buttons;
    let current = &event.mouse_buttons;
    let (kind, button) = mouse_transition(prior, current);

    if kind == MouseEventKind::Press {
        tab.set_active_pane(&pane);
    }
    let mouse = MouseEvent {
        kind,
        x: screen_x
            .saturating_sub(pane_view.rect.x)
            .min(pane_view.rect.width.saturating_sub(1)) as usize,
        y: screen_y
            .saturating_sub(pane_view.rect.y)
            .min(pane_view.rect.height.saturating_sub(1)) as i64,
        x_pixel_offset: 0,
        y_pixel_offset: 0,
        button,
        modifiers: event.modifiers.remove_positional_mods(),
    };
    if let Err(err) = pane.mouse_event(mouse) {
        state.ui.status = format!("Mouse: {err}");
    }
}

fn pane_point(
    state: &TuiState,
    pane_id: usize,
    screen_x: u16,
    screen_y: u16,
) -> Option<SelectionPoint> {
    let view = state
        .layout
        .panes
        .iter()
        .find(|pane| pane.pane_id == pane_id)?;
    let pane = state.pane(pane_id)?;
    let dims = pane.get_dimensions();
    let offset = state.ui.scroll_offset(pane_id).min(dims.scrollback_rows);
    let top = dims
        .physical_top
        .saturating_sub(offset as wezterm_term::StableRowIndex)
        .max(dims.scrollback_top);
    Some(SelectionPoint {
        row: top + screen_y.saturating_sub(view.rect.y) as wezterm_term::StableRowIndex,
        col: screen_x.saturating_sub(view.rect.x) as usize,
    })
}

fn first_button(buttons: &MouseButtons) -> Option<MouseButton> {
    if buttons.contains(MouseButtons::LEFT) {
        Some(MouseButton::Left)
    } else if buttons.contains(MouseButtons::MIDDLE) {
        Some(MouseButton::Middle)
    } else if buttons.contains(MouseButtons::RIGHT) {
        Some(MouseButton::Right)
    } else {
        None
    }
}

fn mouse_transition(prior: &MouseButtons, current: &MouseButtons) -> (MouseEventKind, MouseButton) {
    if current.contains(MouseButtons::VERT_WHEEL) {
        if current.contains(MouseButtons::WHEEL_POSITIVE) {
            (MouseEventKind::Press, MouseButton::WheelUp(1))
        } else {
            (MouseEventKind::Press, MouseButton::WheelDown(1))
        }
    } else if current.contains(MouseButtons::HORZ_WHEEL) {
        if current.contains(MouseButtons::WHEEL_POSITIVE) {
            (MouseEventKind::Press, MouseButton::WheelRight(1))
        } else {
            (MouseEventKind::Press, MouseButton::WheelLeft(1))
        }
    } else if let Some(button) = first_button(current) {
        let kind = if first_button(prior) == Some(button) {
            MouseEventKind::Move
        } else {
            MouseEventKind::Press
        };
        (kind, button)
    } else if let Some(button) = first_button(prior) {
        (MouseEventKind::Release, button)
    } else {
        (MouseEventKind::Move, MouseButton::None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn takeover_epochs_are_scoped_to_domain_and_top_level_tab() {
        let mut epochs = TakeoverEpochs::default();
        let first = epochs.begin("devbox", 7);
        let second = epochs.begin("devbox", 8);

        assert!(epochs.contains("devbox", 7));
        assert!(epochs.contains("devbox", 8));
        assert!(!epochs.finish("devbox", 7, first.wrapping_add(10)));
        assert!(epochs.contains("devbox", 7));
        assert!(epochs.finish("devbox", 7, first));
        assert!(!epochs.contains("devbox", 7));
        assert!(epochs.contains("devbox", 8));
        assert!(epochs.finish("devbox", 8, second));
    }

    #[test]
    fn takeover_pending_forces_the_tui_gate_to_syncing() {
        let claimable = RemoteFrontendGate::Claimable { owner: None };
        assert_eq!(
            effective_frontend_gate(true, claimable.clone()),
            RemoteFrontendGate::Syncing
        );
        assert_eq!(effective_frontend_gate(false, claimable.clone()), claimable);
        assert_eq!(
            effective_frontend_gate(true, RemoteFrontendGate::Visible),
            RemoteFrontendGate::Syncing
        );
    }

    #[test]
    fn takeover_geometry_requires_a_quiet_ready_window() {
        let started = Instant::now();
        let mut ready_since = None;
        assert!(!takeover_geometry_settled(true, started, &mut ready_since));
        assert!(!takeover_geometry_settled(
            true,
            started + TAKEOVER_GEOMETRY_SETTLE / 2,
            &mut ready_since
        ));
        assert!(takeover_geometry_settled(
            true,
            started + TAKEOVER_GEOMETRY_SETTLE,
            &mut ready_since
        ));
        assert!(!takeover_geometry_settled(
            false,
            started + TAKEOVER_GEOMETRY_SETTLE * 2,
            &mut ready_since
        ));
        assert!(ready_since.is_none());
    }

    #[test]
    fn handoff_reveals_each_tab_only_after_its_geometry_generation_is_ready() {
        let access = codec::FrontendAccessState {
            mode: codec::FrontendAccessMode::Handoff,
            owner: None,
            generation: 17,
        };
        assert!(handoff_geometry_is_pending(
            &RemoteFrontendGate::Visible,
            Some(&access),
            Some(true),
            None,
        ));
        assert!(handoff_geometry_is_pending(
            &RemoteFrontendGate::Visible,
            Some(&access),
            Some(true),
            Some(16),
        ));
        assert!(!handoff_geometry_is_pending(
            &RemoteFrontendGate::Visible,
            Some(&access),
            Some(true),
            Some(17),
        ));
        assert!(!handoff_geometry_is_pending(
            &RemoteFrontendGate::Claimable { owner: None },
            Some(&access),
            Some(false),
            None,
        ));
        let collaborative = codec::FrontendAccessState {
            mode: codec::FrontendAccessMode::TmuxLatest,
            ..access
        };
        assert!(!handoff_geometry_is_pending(
            &RemoteFrontendGate::Visible,
            Some(&collaborative),
            Some(true),
            None,
        ));
    }

    fn model_for_reorder() -> AppModel {
        let mut model = AppModel::default();
        model.apply_snapshot(
            "server",
            ThinkTermSessionState {
                server_id: "runtime".into(),
                generation: 1,
                spaces: vec![codec::ThinkTermSessionSpace {
                    id: "space".into(),
                    name: "Space".into(),
                    ..Default::default()
                }],
                projects: vec![codec::ThinkTermSessionProject {
                    id: "project".into(),
                    space_id: "space".into(),
                    name: "Project".into(),
                    threads: ["one", "two"]
                        .into_iter()
                        .map(|id| codec::ThinkTermSessionThread {
                            id: id.into(),
                            project_id: "project".into(),
                            name: id.into(),
                            ..Default::default()
                        })
                        .collect(),
                    ..Default::default()
                }],
                ..Default::default()
            },
        );
        model
    }

    #[test]
    fn nesting_guard_uses_the_transport_target_as_authority() {
        assert!(check_nesting(Some("server-a"), true, "server-a").is_err());
        assert!(check_nesting(Some("server-a"), true, "server-b").is_ok());
        assert!(check_nesting(None, false, "server-b").is_ok());
        assert!(check_nesting(None, true, "server-b").is_err());
    }

    #[test]
    fn terminal_pixels_scale_with_the_cell_grid() {
        let size = terminal_size(
            80,
            24,
            ScreenSize {
                cols: 100,
                rows: 40,
                #[cfg(unix)]
                xpixel: 800,
                #[cfg(unix)]
                ypixel: 640,
                #[cfg(not(unix))]
                xpixel: 8,
                #[cfg(not(unix))]
                ypixel: 16,
            },
        );
        assert_eq!((size.pixel_width, size.pixel_height), (640, 384));
    }

    #[test]
    fn terminal_pixels_remain_unknown_when_the_terminal_does_not_report_them() {
        let size = terminal_size(
            80,
            24,
            ScreenSize {
                cols: 100,
                rows: 40,
                xpixel: 0,
                ypixel: 0,
            },
        );
        assert_eq!((size.pixel_width, size.pixel_height), (0, 0));
    }

    #[test]
    fn terminal_mouse_coordinates_match_ratatui_cells() {
        let event = TermwizMouseEvent {
            x: 23,
            y: 9,
            mouse_buttons: MouseButtons::LEFT,
            modifiers: Modifiers::NONE,
        };
        #[cfg(unix)]
        assert_eq!(mouse_cell_coordinates(&event), (22, 8));
        #[cfg(not(unix))]
        assert_eq!(mouse_cell_coordinates(&event), (23, 9));
    }

    #[test]
    fn tree_drag_uses_stable_thread_ids_not_visual_indices() {
        let model = model_for_reorder();
        let state = TuiState::new(
            BTreeMap::new(),
            vec![],
            BTreeMap::new(),
            model,
            String::new(),
            TuiConfig::default(),
            PathBuf::from("/tmp/test-tui.toml"),
            PathBuf::from("/tmp/test-tui.json"),
        );
        let one = TreeNodeKey::Thread(state.model.rows()[0].key.clone());
        let two = TreeNodeKey::Thread(state.model.rows()[1].key.clone());
        let action = tree_reorder_action(&state, two, one).unwrap();
        let Action::MoveThread { key, before } = action else {
            panic!("thread drag did not produce a server move action")
        };
        assert_eq!(key.thread_id, "two");
        assert_eq!(before.as_deref(), Some("one"));
    }

    #[test]
    fn the_viewport_owner_lays_out_at_the_size_it_reported() {
        let reported = TerminalSize {
            rows: 30,
            cols: 100,
            ..TerminalSize::default()
        };
        // What the server derives back from pane sizes that already lost a row
        // to each pane's nav bar. Adopting it is what used to cost another row
        // per layout.
        let echo = TerminalSize {
            rows: 29,
            cols: 100,
            ..TerminalSize::default()
        };
        assert_eq!(
            choose_local_tab_size(Some(true), reported, echo),
            reported,
            "an owner must not adopt the shrunken echo"
        );
        assert_eq!(
            choose_local_tab_size(None, reported, echo),
            reported,
            "an unknown lease is treated as ours, as it is before the first answer"
        );
        assert_eq!(
            choose_local_tab_size(Some(false), reported, echo),
            echo,
            "a viewer has no geometry of its own and draws what the panes have"
        );
    }

    /// The loop that used to eat a row per layout, run five times.
    ///
    /// Lay out at the tab size, spend a row on each pane's own bar, tell the
    /// server the smaller grid, let the server derive its tab size back from
    /// those panes, and lay out again. Every step is real; the only question is
    /// which size the next layout starts from.
    #[test]
    fn a_pane_bar_costs_its_row_once_rather_than_once_per_layout() {
        fn rows(rows: usize) -> TerminalSize {
            TerminalSize {
                rows,
                cols: 100,
                ..TerminalSize::default()
            }
        }

        let reported = rows(30);
        let mut laid_out = reported;
        let mut grid = 0;
        for _ in 0..5 {
            grid = laid_out.rows - 1;
            laid_out = choose_local_tab_size(Some(true), reported, rows(grid));
        }
        assert_eq!(laid_out.rows, 30, "the tab is still the size we asked for");
        assert_eq!(grid, 29, "and the bar has cost exactly one row, once");

        // A viewer has no size of its own, so it does follow the panes down —
        // and settles, because it never spends the row it was not asked to.
        let mut viewing = reported;
        for _ in 0..5 {
            viewing = choose_local_tab_size(Some(false), reported, rows(29));
        }
        assert_eq!(viewing.rows, 29);
    }

    /// A phone and a desktop on the same machine share one `tui.toml`, so a
    /// remembered "sidebar visible" is a single boolean the two of them fight
    /// over — and on the phone "visible" means covering the terminal entirely.
    #[test]
    fn a_narrow_screen_neither_opens_covered_nor_rewrites_the_shared_preference() {
        assert!(
            !opens_with_tree(ViewClass::Narrow, true),
            "opens on its terminal"
        );
        assert!(!opens_with_tree(ViewClass::Narrow, false));
        assert!(
            opens_with_tree(ViewClass::Compact, true),
            "a panel is remembered"
        );
        assert!(opens_with_tree(ViewClass::Desktop, true));
        assert!(!opens_with_tree(ViewClass::Desktop, false));

        assert!(!tree_visibility_is_a_preference(ViewClass::Narrow));
        assert!(tree_visibility_is_a_preference(ViewClass::Compact));
        assert!(tree_visibility_is_a_preference(ViewClass::Desktop));
    }

    #[test]
    fn generated_tree_ids_are_process_independent_uuids() {
        let ids = (0..1_000).map(|_| new_id("thread")).collect::<HashSet<_>>();
        assert_eq!(ids.len(), 1_000);
        assert!(ids.iter().all(|id| id.starts_with("thread-")));
    }

    #[test]
    fn tab_title_changes_request_a_fresh_server_snapshot() {
        assert!(notification_refreshes_session(
            &MuxNotification::TabTitleChanged {
                tab_id: 7,
                title: "editor".into(),
            }
        ));
        assert!(!notification_refreshes_session(
            &MuxNotification::FrontendLeaseChanged(mux::FrontendViewportState {
                tab_id: 7,
                owner: None,
                canonical_size: TerminalSize::default(),
                view: None,
                generation: 1,
                access: mux::FrontendAccessState {
                    mode: mux::FrontendAccessMode::Handoff,
                    owner: None,
                    generation: 1,
                },
            })
        ));
    }

    #[test]
    fn snapshots_from_superseded_or_detached_connections_are_rejected() {
        assert!(session_generation_is_current(true, Some(9), Some(9), 9));
        assert!(!session_generation_is_current(true, Some(10), Some(10), 9));
        assert!(!session_generation_is_current(true, Some(10), Some(9), 10));
        assert!(!session_generation_is_current(false, Some(9), Some(9), 9));
    }

    #[test]
    fn copy_cursor_scrolls_at_both_viewport_edges_and_stays_in_bounds() {
        let (above, above_offset) = adjusted_copy_view(
            SelectionPoint { row: 8, col: 200 },
            80,
            0,
            119,
            100,
            20,
            80,
            100,
        );
        assert_eq!(above, SelectionPoint { row: 8, col: 79 });
        assert_eq!(above_offset, 92);

        let (below, below_offset) = adjusted_copy_view(
            SelectionPoint { row: 115, col: 1 },
            80,
            0,
            119,
            100,
            10,
            20,
            100,
        );
        assert_eq!(below.row, 109);
        assert_eq!(below_offset, 0);
    }

    #[test]
    fn modal_mouse_clicks_never_fall_through_to_the_tree() {
        let model = model_for_reorder();
        let mut state = TuiState::new(
            BTreeMap::new(),
            vec![],
            BTreeMap::new(),
            model,
            String::new(),
            TuiConfig::default(),
            PathBuf::from("/tmp/test-tui.toml"),
            PathBuf::from("/tmp/test-tui.json"),
        );
        state.ui.mode = AppMode::Help;
        state.layout.hits.push(view::HitRegion {
            rect: ratatui::layout::Rect::new(0, 0, 10, 10),
            target: HitTarget::Tree(TreeNodeKey::Thread(state.model.rows()[0].key.clone())),
        });

        assert!(modal_mouse_input(&mut state, 1, 1, true));
        assert_eq!(state.actions.pop_front(), Some(Action::ToggleHelp));
        assert!(state.actions.is_empty());
    }

    /// Taking control of a tab reads a present key as proof that the server
    /// already holds this renderer's viewport. Forgetting a size is how the
    /// next frame is told to re-send it, and a burst of resize actions all runs
    /// before that frame — so the key has to outlive the forgetting, or every
    /// step of a drag after the first one is refused for lack of a viewport.
    #[test]
    fn forgetting_a_viewport_size_keeps_the_evidence_that_one_was_sent() {
        let model = model_for_reorder();
        let mut state = TuiState::new(
            BTreeMap::new(),
            vec![],
            BTreeMap::new(),
            model,
            String::new(),
            TuiConfig::default(),
            PathBuf::from("/tmp/test-tui.toml"),
            PathBuf::from("/tmp/test-tui.json"),
        );
        let domain_name = state
            .model
            .selected_row()
            .expect("a selected row")
            .key
            .domain_name
            .clone();
        let key = (domain_name, 7);
        state.last_viewports.insert(
            key.clone(),
            Some((
                ClientViewport::CellGrid {
                    size: TerminalSize {
                        rows: 24,
                        cols: 80,
                        ..Default::default()
                    },
                },
                true,
            )),
        );

        state.clear_selected_viewport_cache();

        assert_eq!(state.last_viewports.get(&key), Some(&None));
    }

    #[test]
    fn shared_view_debounce_and_payload_are_scoped_to_each_tab() {
        let model = model_for_reorder();
        let mut state = TuiState::new(
            BTreeMap::new(),
            vec![],
            BTreeMap::new(),
            model,
            String::new(),
            TuiConfig::default(),
            PathBuf::from("/tmp/test-tui.toml"),
            PathBuf::from("/tmp/test-tui.json"),
        );
        let first = ("server".to_string(), 7);
        let second = ("server".to_string(), 8);
        state.shared_view_at.insert(first.clone(), Instant::now());
        state
            .shared_views
            .insert(first.clone(), codec::ClientView::default());

        assert!(!share_view_is_due(&state, &first));
        assert!(share_view_is_due(&state, &second));
        assert!(!state.shared_views.contains_key(&second));
    }

    /// A fingertip covers several cells and the touchscreen picks one, so a tap
    /// meant to focus a pane arrives as a short drag. With copy-on-select on,
    /// that used to hand the clipboard one stray character on every tap.
    #[test]
    fn a_tap_that_slips_a_cell_or_two_is_not_a_selection() {
        fn finish(
            anchor: (wezterm_term::StableRowIndex, usize),
            head: (wezterm_term::StableRowIndex, usize),
        ) -> bool {
            let mut selection = Some(TextSelection {
                pane_id: 1,
                anchor: SelectionPoint {
                    row: anchor.0,
                    col: anchor.1,
                },
                head: SelectionPoint {
                    row: head.0,
                    col: head.1,
                },
                finalized: false,
            });
            finish_pointer_selection(&mut selection)
        }

        assert!(!finish((5, 10), (5, 10)), "a still tap");
        assert!(!finish((5, 10), (5, 11)), "one cell of slip");
        assert!(!finish((5, 10), (5, 8)), "slip backwards counts the same");
        assert!(finish((5, 10), (5, 13)), "a deliberate drag along the row");
        // Reaching another line is unambiguous however short the move.
        assert!(finish((5, 10), (6, 10)), "one row down");
        assert!(finish((5, 10), (4, 11)), "one row up");
    }

    /// A touchscreen has no keyboard, so the wheel is the only way through a
    /// full-screen program — and it must not be spent on a scrollback that the
    /// alternate screen does not keep.
    #[test]
    fn a_wheel_notch_on_the_alternate_screen_reaches_a_program_that_never_asked_for_the_mouse() {
        assert_eq!(
            wheel_routing(false, true, false, 0),
            WheelRouting::ArrowKeys
        );
        // One that did ask for the mouse still receives the notch itself.
        assert_eq!(wheel_routing(true, true, false, 0), WheelRouting::Pane);
        // Shift is the deliberate way back to the scrollback, as is having
        // already scrolled away from the live screen.
        assert_eq!(
            wheel_routing(false, true, true, 0),
            WheelRouting::Scrollback
        );
        assert_eq!(
            wheel_routing(true, true, false, 3),
            WheelRouting::Scrollback
        );
        // The primary screen keeps the scrollback it actually has.
        assert_eq!(
            wheel_routing(false, false, false, 0),
            WheelRouting::Scrollback
        );
    }

    #[test]
    fn paste_is_blocked_by_non_prompt_overlays() {
        let mut state = TuiState::new(
            BTreeMap::new(),
            vec![],
            BTreeMap::new(),
            AppModel::default(),
            String::new(),
            TuiConfig::default(),
            PathBuf::from("/tmp/test-tui.toml"),
            PathBuf::from("/tmp/test-tui.json"),
        );
        state.ui.mode = AppMode::Confirm;
        handle_input(InputEvent::Paste("dangerous command\n".into()), &mut state);
        assert!(state.ui.status.contains("unavailable"));
    }

    #[test]
    fn passed_through_mouse_sequences_include_release_and_horizontal_wheel() {
        assert_eq!(
            mouse_transition(&MouseButtons::NONE, &MouseButtons::RIGHT),
            (MouseEventKind::Press, MouseButton::Right)
        );
        assert_eq!(
            mouse_transition(&MouseButtons::RIGHT, &MouseButtons::RIGHT),
            (MouseEventKind::Move, MouseButton::Right)
        );
        assert_eq!(
            mouse_transition(&MouseButtons::RIGHT, &MouseButtons::NONE),
            (MouseEventKind::Release, MouseButton::Right)
        );
        assert_eq!(
            mouse_transition(
                &MouseButtons::NONE,
                &(MouseButtons::HORZ_WHEEL | MouseButtons::WHEEL_POSITIVE),
            ),
            (MouseEventKind::Press, MouseButton::WheelRight(1))
        );
    }

    #[test]
    fn plain_click_does_not_become_a_copy_selection() {
        let point = SelectionPoint { row: 4, col: 7 };
        let mut selection = Some(TextSelection {
            pane_id: 1,
            anchor: point,
            head: point,
            finalized: false,
        });
        assert!(!finish_pointer_selection(&mut selection));
        assert!(selection.is_none());
    }

    #[test]
    fn pointer_drag_becomes_a_finalized_selection() {
        let selection = TextSelection {
            pane_id: 1,
            anchor: SelectionPoint { row: 4, col: 7 },
            head: SelectionPoint { row: 4, col: 10 },
            finalized: false,
        };
        let mut selection = Some(selection);
        assert!(finish_pointer_selection(&mut selection));
        assert!(selection.is_some_and(|selection| selection.finalized));
    }

    #[test]
    fn ctrl_c_only_copies_a_retained_manual_selection() {
        let selection = TextSelection {
            pane_id: 1,
            anchor: SelectionPoint { row: 4, col: 7 },
            head: SelectionPoint { row: 4, col: 10 },
            finalized: true,
        };
        assert!(should_copy_retained_selection(
            KeyCode::Char('c'),
            Modifiers::CTRL,
            false,
            Some(&selection),
        ));
        assert!(!should_copy_retained_selection(
            KeyCode::Char('c'),
            Modifiers::CTRL,
            true,
            Some(&selection),
        ));
        assert!(!should_copy_retained_selection(
            KeyCode::Char('c'),
            Modifiers::CTRL,
            false,
            None,
        ));
    }

    #[test]
    fn explicit_tui_themes_are_not_forced_monochrome_by_no_color() {
        let caps = tui_terminal_capabilities(
            ProbeHints::default()
                .term(Some("xterm-256color".to_string()))
                .color_level(Some(termwiz::caps::ColorLevel::MonoChrome)),
        )
        .unwrap();

        assert_ne!(caps.color_level(), termwiz::caps::ColorLevel::MonoChrome);
    }

    #[test]
    fn recent_input_keeps_executor_progress_bounded_without_spinning_when_idle() {
        let now = Instant::now();
        assert_eq!(
            input_progress_wait(None, Some(now + INPUT_PROGRESS_WINDOW), now),
            Some(INPUT_PROGRESS_POLL_INTERVAL)
        );
        assert_eq!(
            input_progress_wait(
                Some(Duration::from_millis(1)),
                Some(now + INPUT_PROGRESS_WINDOW),
                now,
            ),
            Some(Duration::from_millis(1))
        );
        assert_eq!(
            input_progress_wait(Some(Duration::from_secs(2)), None, now),
            Some(Duration::from_secs(2))
        );
        assert_eq!(input_progress_wait(None, Some(now), now), None);
    }
}
