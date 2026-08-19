use crate::client::{ClientId, ClientInfo};
use crate::pane::{CachePolicy, Pane, PaneId};
use crate::ssh_agent::AgentProxy;
use crate::tab::{SplitRequest, Tab, TabId};
use crate::window::{Window, WindowId, WindowUiSurfaceId};
use anyhow::{anyhow, Context, Error};
use config::keyassignment::SpawnTabDomain;
use config::{configuration, ExitBehavior, GuiPosition};
use domain::{Domain, DomainId, DomainState, SplitSource};
use filedescriptor::{poll, pollfd, socketpair, AsRawSocketDescriptor, FileDescriptor, POLLIN};
#[cfg(unix)]
use libc::{c_int, SOL_SOCKET, SO_RCVBUF, SO_SNDBUF};
use log::error;
use metrics::histogram;
use parking_lot::{
    MappedRwLockReadGuard, MappedRwLockWriteGuard, Mutex, RwLock, RwLockReadGuard, RwLockWriteGuard,
};
use percent_encoding::percent_decode_str;
use portable_pty::{CommandBuilder, ExitStatus, PtySize};
use std::collections::{HashMap, HashSet};
use std::convert::TryInto;
use std::io::{Read, Write};
#[cfg(windows)]
use std::os::raw::c_int;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Weak};
use std::thread;
use std::time::{Duration, Instant, SystemTime};
use termwiz::escape::csi::{DecPrivateMode, DecPrivateModeCode, Device, Mode};
use termwiz::escape::{Action, CSI};
use thiserror::*;
use wezterm_term::color::ColorPalette;
use wezterm_term::{
    Clipboard, ClipboardSelection, DownloadHandler, TerminalConfiguration, TerminalSize,
};
#[cfg(windows)]
use winapi::um::winsock2::{SOL_SOCKET, SO_RCVBUF, SO_SNDBUF};

pub mod activity;
pub mod client;
pub mod command_spec;
pub mod connui;
pub mod domain;
pub mod geometrytrace;
pub mod localpane;
pub mod pane;
pub mod renderable;
pub mod ssh;
pub mod ssh_agent;
pub mod tab;
pub mod termwiztermtab;
pub mod tmux;
pub mod tmux_commands;
mod tmux_pty;
pub mod window;

use crate::activity::Activity;

pub const DEFAULT_WORKSPACE: &str = "default";

#[derive(Clone, Debug)]
pub enum MuxNotification {
    PaneOutput(PaneId),
    PaneAdded(PaneId),
    PaneRemoved(PaneId),
    WindowCreated(WindowId),
    WindowRemoved(WindowId),
    WindowInvalidated(WindowId),
    WindowWorkspaceChanged(WindowId),
    ActiveWorkspaceChanged(Arc<ClientId>),
    Alert {
        pane_id: PaneId,
        alert: wezterm_term::Alert,
    },
    Empty,
    AssignClipboard {
        pane_id: PaneId,
        selection: ClipboardSelection,
        clipboard: Option<String>,
    },
    SaveToDownloads {
        name: Option<String>,
        data: Arc<Vec<u8>>,
    },
    TabAddedToWindow {
        tab_id: TabId,
        window_id: WindowId,
    },
    PaneFocused(PaneId),
    TabResized(TabId),
    TabTitleChanged {
        tab_id: TabId,
        title: String,
    },
    WindowTitleChanged {
        window_id: WindowId,
        title: String,
    },
    WorkspaceRenamed {
        old_workspace: String,
        new_workspace: String,
    },
    /// The mux server's ThinkTerm sidebar tree (Space/Project/Thread) changed;
    /// every connection re-sends it. Raised only on the server side — a GUI
    /// mux never owns a tree of its own.
    ThinkTermTreeChanged,
    /// The authoritative server tree or live mux topology changed; thin
    /// frontends should refresh their read-only session snapshot.
    ThinkTermSessionChanged,
    /// The renderer allowed to drive one tab's shared PTY geometry changed,
    /// or that owner published a new canonical viewport.
    FrontendLeaseChanged(FrontendViewportState),
    /// The connection-wide A/B mode or the exclusive handoff owner changed.
    FrontendAccessChanged(FrontendAccessState),
}

static SUB_ID: AtomicUsize = AtomicUsize::new(0);
static PALETTE_SESSION_ID: AtomicUsize = AtomicUsize::new(1);
static CLIENT_REGISTRATION_ID: AtomicUsize = AtomicUsize::new(1);
static SERVER_ID_SEQUENCE: AtomicUsize = AtomicUsize::new(1);

fn new_runtime_server_id() -> String {
    let epoch_nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let host = hostname::get()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "localhost".to_string());
    format!(
        "{host}:{}:{epoch_nanos}:{}",
        std::process::id(),
        SERVER_ID_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    )
}

/// Identifies one live transport registration for a `ClientId`. A reconnect
/// intentionally reuses `ClientId`, so disconnect cleanup and queued viewport
/// work also carry this generation and cannot tear down the replacement.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ClientRegistrationId(usize);

impl ClientRegistrationId {
    fn new() -> Self {
        Self(CLIENT_REGISTRATION_ID.fetch_add(1, Ordering::Relaxed))
    }
}

/// A single mux-server transport connection. `ClientId` identifies the GUI
/// process and can be reused when that process reconnects, so it is not
/// sufficient to reject work queued by an older, disconnected transport.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PaletteSessionId(usize);

#[derive(Clone, Debug, PartialEq)]
pub struct PaletteSelectionChange {
    pub pane_id: PaneId,
    pub palette: Option<ColorPalette>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrontendPaneViewport {
    pub pane_id: PaneId,
    pub size: wezterm_term::TerminalSize,
    pub frame: wezterm_term::TerminalSize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FrontendViewport {
    CellGrid {
        size: wezterm_term::TerminalSize,
    },
    Native {
        size: wezterm_term::TerminalSize,
        panes: Vec<FrontendPaneViewport>,
    },
}

impl FrontendViewport {
    pub fn size(&self) -> wezterm_term::TerminalSize {
        match self {
            Self::CellGrid { size } | Self::Native { size, .. } => *size,
        }
    }
}

/// What the renderer holding a tab's viewport is currently looking at.
///
/// Size alone makes two attached devices the same shape; it does not make them
/// the same view. Without this a phone and a desktop on one tab sit at
/// different points in the same scrollback, which reads as two sessions that
/// happen to share a name.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FrontendView {
    /// Lines above the bottom of each pane's scrollback. A pane that is absent
    /// is following its output.
    pub scroll: Vec<(PaneId, u32)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrontendViewportState {
    pub tab_id: TabId,
    /// Per-tab layout owner used by `TmuxLatest` mode.
    pub owner: Option<ClientId>,
    pub canonical_size: wezterm_term::TerminalSize,
    /// What the owner is looking at, when the owner is a renderer that says.
    /// `None` means nobody has offered one — followers keep their own view
    /// rather than being pulled somewhere arbitrary.
    pub view: Option<FrontendView>,
    pub generation: u64,
    pub access: FrontendAccessState,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FrontendAccessMode {
    TmuxLatest,
    #[default]
    Handoff,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrontendAccessState {
    pub mode: FrontendAccessMode,
    pub owner: Option<ClientId>,
    pub generation: u64,
}

#[derive(Default)]
struct TabFrontendLease {
    owner: Option<ClientId>,
    /// Last geometry advertised by each renderer. Network claims always carry
    /// their geometry and never trust this cache; it remains useful for the
    /// in-process GUI, whose Pane input reaches the mux without a wire PDU.
    viewports: HashMap<ClientId, FrontendViewport>,
    /// Written by the owner alone, and dropped the moment the lease moves, so
    /// it can never describe a renderer that is no longer driving.
    view: Option<FrontendView>,
    generation: u64,
}

/// True when `me` is the only *live* client that has advertised geometry for
/// any tab. A sole renderer cannot be fighting anyone over the lease, so an
/// unclaimed lease is effectively its own. Two or more renderers must take it
/// explicitly — see `unregister_client`, which deliberately elects no
/// successor.
///
/// Handoff's owner is global, so the scan is global: a phone rendering tab B
/// must still stop the desktop from silently assuming tab A. Filtering by
/// `live` registrations guards against a viewport left behind by a client
/// whose transport died without an orderly unregister.
fn is_sole_live_renderer(
    tabs: &HashMap<TabId, TabFrontendLease>,
    live: &HashSet<ClientId>,
    me: &ClientId,
) -> bool {
    !tabs.values().any(|state| {
        state
            .viewports
            .keys()
            .any(|client| client != me && live.contains(client))
    })
}

struct FrontendLeaseState {
    tabs: HashMap<TabId, TabFrontendLease>,
    access_mode: FrontendAccessMode,
    handoff_owner: Option<ClientId>,
    /// Distinguishes initial server startup (first renderer auto-owns) from an
    /// owner disconnect (everyone stays blocked until an explicit takeover).
    handoff_ever_owned: bool,
    /// Clients that have ever advertised or claimed a viewport during their
    /// current registration. Handoff's gate exists to stop two *screens* from
    /// fighting over one terminal; a client that never renders -- `thinkterm
    /// cli send-text` and friends -- is not a screen, so it is exempt from
    /// that gate rather than being told the terminal "is being operated on
    /// another device" by a session it can never see. Sticky, unlike the
    /// per-tab `viewports` maps (which are pruned as tabs close): once a
    /// connection has rendered anything it is a frontend for its whole life.
    ever_rendered: HashSet<ClientId>,
    access_initialized: bool,
    access_generation: u64,
    next_generation: u64,
}

impl Default for FrontendLeaseState {
    fn default() -> Self {
        Self {
            tabs: HashMap::new(),
            access_mode: FrontendAccessMode::Handoff,
            handoff_owner: None,
            handoff_ever_owned: false,
            ever_rendered: HashSet::new(),
            access_initialized: false,
            access_generation: 0,
            next_generation: 1,
        }
    }
}

/// Server-side palette advice is scoped to the client that supplied it.
/// Merely attaching records advice; focus or real input activates it for OSC
/// queries. Application palette overrides are tracked by the terminal and are
/// intentionally not represented here.
#[derive(Default)]
struct PaletteAdvisoryState {
    sessions: HashMap<PaletteSessionId, ClientId>,
    advised: HashMap<(PaletteSessionId, PaneId), ColorPalette>,
    activity: HashMap<(PaletteSessionId, PaneId), u64>,
    owners: HashMap<PaneId, PaletteSessionId>,
    next_activity: u64,
}

impl PaletteAdvisoryState {
    fn register(&mut self, client_id: &ClientId) -> PaletteSessionId {
        let session_id = PaletteSessionId(PALETTE_SESSION_ID.fetch_add(1, Ordering::Relaxed));
        self.sessions.insert(session_id, client_id.clone());
        session_id
    }

    fn advise(
        &mut self,
        session_id: PaletteSessionId,
        pane_id: PaneId,
        palette: ColorPalette,
    ) -> Option<ColorPalette> {
        if !self.sessions.contains_key(&session_id) {
            return None;
        }
        self.advised.insert((session_id, pane_id), palette.clone());
        (self.owners.get(&pane_id) == Some(&session_id)).then_some(palette)
    }

    fn activate(&mut self, session_id: PaletteSessionId, pane_id: PaneId) -> Option<ColorPalette> {
        if !self.sessions.contains_key(&session_id) {
            return None;
        }
        let key = (session_id, pane_id);
        let palette = self.advised.get(&key)?.clone();
        self.next_activity = self.next_activity.saturating_add(1).max(1);
        self.activity.insert(key, self.next_activity);

        if self.owners.get(&pane_id) == Some(&session_id) {
            return None;
        }
        self.owners.insert(pane_id, session_id);
        Some(palette)
    }

    fn remove_session(&mut self, session_id: PaletteSessionId) -> Vec<PaletteSelectionChange> {
        self.sessions.remove(&session_id);
        self.advised.retain(|(id, _), _| *id != session_id);
        self.activity.retain(|(id, _), _| *id != session_id);

        let affected: Vec<PaneId> = self
            .owners
            .iter()
            .filter_map(|(pane_id, owner)| (*owner == session_id).then_some(*pane_id))
            .collect();
        let mut changes = Vec::with_capacity(affected.len());

        for pane_id in affected {
            let replacement = self
                .activity
                .iter()
                .filter(|((candidate_session, candidate_pane), _)| {
                    *candidate_pane == pane_id
                        && self.sessions.contains_key(candidate_session)
                        && self.advised.contains_key(&(*candidate_session, pane_id))
                })
                .max_by_key(|(_, activity)| **activity)
                .map(|((id, _), _)| *id);

            let palette = match replacement {
                Some(id) => {
                    self.owners.insert(pane_id, id);
                    self.advised.get(&(id, pane_id)).cloned()
                }
                None => {
                    self.owners.remove(&pane_id);
                    None
                }
            };
            changes.push(PaletteSelectionChange { pane_id, palette });
        }
        changes
    }

    fn deactivate_session(&mut self, session_id: PaletteSessionId) -> bool {
        self.sessions.remove(&session_id).is_some()
    }

    fn remove_pane(&mut self, pane_id: PaneId) {
        self.advised.retain(|(_, pane), _| *pane != pane_id);
        self.activity.retain(|(_, pane), _| *pane != pane_id);
        self.owners.remove(&pane_id);
    }
}

pub struct Mux {
    runtime_server_id: String,
    tabs: RwLock<HashMap<TabId, Arc<Tab>>>,
    panes: RwLock<HashMap<PaneId, Arc<dyn Pane>>>,
    windows: RwLock<HashMap<WindowId, Window>>,
    default_domain: RwLock<Option<Arc<dyn Domain>>>,
    domains: RwLock<HashMap<DomainId, Arc<dyn Domain>>>,
    domains_by_name: RwLock<HashMap<String, Arc<dyn Domain>>>,
    subscribers: RwLock<HashMap<usize, Box<dyn Fn(MuxNotification) -> bool + Send + Sync>>>,
    banner: RwLock<Option<String>>,
    clients: RwLock<HashMap<ClientId, ClientInfo>>,
    client_registrations: RwLock<HashMap<ClientId, ClientRegistrationId>>,
    frontend_lease: Mutex<FrontendLeaseState>,
    tab_resize_notifications: Mutex<TabResizeNotificationState>,
    #[cfg(test)]
    frontend_geometry_failures: Mutex<std::collections::VecDeque<bool>>,
    palette_advisories: Mutex<PaletteAdvisoryState>,
    identity: RwLock<Option<Arc<ClientId>>>,
    num_panes_by_workspace: RwLock<HashMap<String, usize>>,
    main_thread_id: std::thread::ThreadId,
    agent: Option<AgentProxy>,
}

#[derive(Default)]
struct TabResizeNotificationState {
    depth: HashMap<TabId, usize>,
    pending: HashSet<TabId>,
    aborted: HashSet<TabId>,
}

impl TabResizeNotificationState {
    fn begin(&mut self, tab_id: TabId) {
        if self.depth.get(&tab_id).copied().unwrap_or_default() == 0 {
            self.aborted.remove(&tab_id);
        }
        *self.depth.entry(tab_id).or_default() += 1;
    }

    fn defer(&mut self, tab_id: TabId) -> bool {
        if self.depth.get(&tab_id).copied().unwrap_or_default() == 0 {
            return false;
        }
        self.pending.insert(tab_id);
        true
    }

    fn finish(&mut self, tab_id: TabId, commit: bool) -> bool {
        if !commit {
            self.aborted.insert(tab_id);
        }
        let Some(depth) = self.depth.get_mut(&tab_id) else {
            return false;
        };
        *depth -= 1;
        if *depth != 0 {
            return false;
        }
        self.depth.remove(&tab_id);
        let pending = self.pending.remove(&tab_id);
        let aborted = self.aborted.remove(&tab_id);
        commit && !aborted && pending
    }
}

struct TabGeometryTransaction<'a> {
    mux: &'a Mux,
    tab_id: TabId,
    commit: bool,
}

impl TabGeometryTransaction<'_> {
    fn commit(&mut self) {
        self.commit = true;
    }
}

impl Drop for TabGeometryTransaction<'_> {
    fn drop(&mut self) {
        let publish = self
            .mux
            .tab_resize_notifications
            .lock()
            .finish(self.tab_id, self.commit);
        if publish {
            self.mux
                .notify_immediate(MuxNotification::TabResized(self.tab_id));
        }
    }
}

const BUFSIZE: usize = 1024 * 1024;

/// Size of the per-pane pty read buffer. This lives for the lifetime of the
/// pane's reader thread, so it is sized for what a single read() can actually
/// return (the kernel tty buffer is far smaller than this) rather than
/// BUFSIZE, which would pin 1MB per pane.
const PTY_READ_BUFSIZE: usize = 64 * 1024;

/// This function applies parsed actions to the pane and notifies any
/// mux subscribers about the output event
fn send_actions_to_mux(pane: &Weak<dyn Pane>, dead: &Arc<AtomicBool>, actions: Vec<Action>) {
    let start = Instant::now();
    match pane.upgrade() {
        Some(pane) => {
            pane.perform_actions(actions);
            histogram!("send_actions_to_mux.perform_actions.latency").record(start.elapsed());
            Mux::notify_from_any_thread(MuxNotification::PaneOutput(pane.pane_id()));
        }
        None => {
            // Something else removed the pane from
            // the mux, so signal that we should stop
            // trying to process it in read_from_pane_pty.
            dead.store(true, Ordering::Relaxed);
        }
    }
    histogram!("send_actions_to_mux.rate").record(1.);
}

fn parse_buffered_data(pane: Weak<dyn Pane>, dead: &Arc<AtomicBool>, mut rx: FileDescriptor) {
    let mut buf = vec![0; configuration().mux_output_parser_buffer_size];
    let mut parser = termwiz::escape::parser::Parser::new();
    let mut actions = vec![];
    let mut hold = false;
    let mut action_size = 0;
    let mut delay = Duration::from_millis(configuration().mux_output_parser_coalesce_delay_ms);
    let mut deadline = None;

    loop {
        match rx.read(&mut buf) {
            Ok(size) if size == 0 => {
                dead.store(true, Ordering::Relaxed);
                break;
            }
            Err(_) => {
                dead.store(true, Ordering::Relaxed);
                break;
            }
            Ok(size) => {
                parser.parse(&buf[0..size], |action| {
                    let mut flush = false;
                    match &action {
                        Action::CSI(CSI::Mode(Mode::SetDecPrivateMode(DecPrivateMode::Code(
                            DecPrivateModeCode::SynchronizedOutput,
                        )))) => {
                            hold = true;

                            // Flush prior actions
                            if !actions.is_empty() {
                                send_actions_to_mux(&pane, &dead, std::mem::take(&mut actions));
                                action_size = 0;
                            }
                        }
                        Action::CSI(CSI::Mode(Mode::ResetDecPrivateMode(
                            DecPrivateMode::Code(DecPrivateModeCode::SynchronizedOutput),
                        ))) => {
                            hold = false;
                            flush = true;
                        }
                        Action::CSI(CSI::Device(dev)) if matches!(**dev, Device::SoftReset) => {
                            hold = false;
                            flush = true;
                        }
                        _ => {}
                    };
                    action.append_to(&mut actions);

                    if flush && !actions.is_empty() {
                        send_actions_to_mux(&pane, &dead, std::mem::take(&mut actions));
                        action_size = 0;
                    }
                });
                action_size += size;
                if !actions.is_empty() && !hold {
                    // If we haven't accumulated too much data,
                    // pause for a short while to increase the chances
                    // that we coalesce a full "frame" from an unoptimized
                    // TUI program
                    if action_size < buf.len() {
                        let poll_delay = match deadline {
                            None => {
                                deadline.replace(Instant::now() + delay);
                                Some(delay)
                            }
                            Some(target) => target.checked_duration_since(Instant::now()),
                        };
                        if poll_delay.is_some() {
                            let mut pfd = [pollfd {
                                fd: rx.as_socket_descriptor(),
                                events: POLLIN,
                                revents: 0,
                            }];
                            if let Ok(1) = poll(&mut pfd, poll_delay) {
                                // We can read now without blocking, so accumulate
                                // more data into actions
                                continue;
                            }

                            // Not readable in time: let the data we have flow into
                            // the terminal model
                        }
                    }

                    send_actions_to_mux(&pane, &dead, std::mem::take(&mut actions));
                    deadline = None;
                    action_size = 0;
                }

                let config = configuration();
                buf.resize(config.mux_output_parser_buffer_size, 0);
                delay = Duration::from_millis(config.mux_output_parser_coalesce_delay_ms);
            }
        }
    }

    // Don't forget to send anything that we might have buffered
    // to be displayed before we return from here; this is important
    // for very short lived commands so that we don't forget to
    // display what they displayed.
    if !actions.is_empty() {
        send_actions_to_mux(&pane, &dead, std::mem::take(&mut actions));
    }
}

fn set_socket_buffer(fd: &mut FileDescriptor, option: i32, size: usize) -> anyhow::Result<()> {
    let size = size as c_int;
    let socklen = std::mem::size_of_val(&size);
    unsafe {
        let res = libc::setsockopt(
            fd.as_socket_descriptor(),
            SOL_SOCKET,
            option,
            &size as *const c_int as *const _,
            socklen as _,
        );
        if res == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error()).context("setsockopt")
        }
    }
}

fn allocate_socketpair() -> anyhow::Result<(FileDescriptor, FileDescriptor)> {
    let (mut tx, mut rx) = socketpair().context("socketpair")?;
    set_socket_buffer(&mut tx, SO_SNDBUF, BUFSIZE)
        .context("SO_SNDBUF")
        .ok();
    set_socket_buffer(&mut rx, SO_RCVBUF, BUFSIZE)
        .context("SO_RCVBUF")
        .ok();
    Ok((tx, rx))
}

/// This function is run in a separate thread; its purpose is to perform
/// blocking reads from the pty (non-blocking reads are not portable to
/// all platforms and pty/tty types), parse the escape sequences and
/// relay the actions to the mux thread to apply them to the pane.
fn read_from_pane_pty(
    pane: Weak<dyn Pane>,
    banner: Option<String>,
    mut reader: Box<dyn std::io::Read>,
) {
    let mut buf = vec![0; PTY_READ_BUFSIZE];

    // This is used to signal that an error occurred either in this thread,
    // or in the main mux thread.  If `true`, this thread will terminate.
    let dead = Arc::new(AtomicBool::new(false));

    let (pane_id, exit_behavior) = match pane.upgrade() {
        Some(pane) => (pane.pane_id(), pane.exit_behavior()),
        None => return,
    };

    let (mut tx, rx) = match allocate_socketpair() {
        Ok(pair) => pair,
        Err(err) => {
            log::error!("read_from_pane_pty: Unable to allocate a socketpair: {err:#}");
            localpane::emit_output_for_pane(
                pane_id,
                &format!(
                    "⚠️  ThinkTerm: read_from_pane_pty: \
                    Unable to allocate a socketpair: {err:#}"
                ),
            );
            return;
        }
    };

    std::thread::spawn({
        let dead = Arc::clone(&dead);
        move || parse_buffered_data(pane, &dead, rx)
    });

    if let Some(banner) = banner {
        tx.write_all(banner.as_bytes()).ok();
    }

    while !dead.load(Ordering::Relaxed) {
        match reader.read(&mut buf) {
            Ok(size) if size == 0 => {
                log::trace!("read_pty EOF: pane_id {}", pane_id);
                break;
            }
            Err(err) => {
                error!("read_pty failed: pane {} {:?}", pane_id, err);
                break;
            }
            Ok(size) => {
                histogram!("read_from_pane_pty.bytes.rate").record(size as f64);
                log::trace!("read_pty pane {pane_id} read {size} bytes");
                if let Err(err) = tx.write_all(&buf[..size]) {
                    error!(
                        "read_pty failed to write to parser: pane {} {:?}",
                        pane_id, err
                    );
                    break;
                }
            }
        }
    }

    match exit_behavior.unwrap_or_else(|| configuration().exit_behavior) {
        ExitBehavior::Hold | ExitBehavior::CloseOnCleanExit => {
            // We don't know if we can unilaterally close
            // this pane right now, so don't!
            promise::spawn::spawn_into_main_thread(async move {
                let mux = Mux::get();
                log::trace!("checking for dead windows after EOF on pane {}", pane_id);
                mux.prune_dead_windows();
            })
            .detach();
        }
        ExitBehavior::Close => {
            promise::spawn::spawn_into_main_thread(async move {
                let mux = Mux::get();
                mux.remove_pane(pane_id);
            })
            .detach();
        }
    }

    dead.store(true, Ordering::Relaxed);
}

lazy_static::lazy_static! {
    static ref MUX: Mutex<Option<Arc<Mux>>> = Mutex::new(None);
}

pub struct MuxWindowBuilder {
    window_id: WindowId,
    activity: Option<Activity>,
    notified: bool,
}

struct ExistingWindowSpawnContext {
    size: TerminalSize,
    term_config: Option<Arc<dyn TerminalConfiguration>>,
}

impl MuxWindowBuilder {
    fn notify(&mut self) {
        if self.notified {
            return;
        }
        self.notified = true;
        let activity = self.activity.take().unwrap();
        let window_id = self.window_id;
        let mux = Mux::get();
        if mux.is_main_thread() {
            // If we're already on the mux thread, just send the notification
            // immediately.
            // This is super important for Wayland; if we push it to the
            // spawn queue below then the extra milliseconds of delay
            // causes it to get confused and shutdown the connection!?
            mux.notify(MuxNotification::WindowCreated(window_id));
        } else {
            promise::spawn::spawn_into_main_thread(async move {
                if let Some(mux) = Mux::try_get() {
                    mux.notify(MuxNotification::WindowCreated(window_id));
                    drop(activity);
                }
            })
            .detach();
        }
    }
}

impl Drop for MuxWindowBuilder {
    fn drop(&mut self) {
        self.notify();
    }
}

impl std::ops::Deref for MuxWindowBuilder {
    type Target = WindowId;

    fn deref(&self) -> &WindowId {
        &self.window_id
    }
}

impl Mux {
    pub fn new(default_domain: Option<Arc<dyn Domain>>) -> Self {
        let mut domains = HashMap::new();
        let mut domains_by_name = HashMap::new();
        if let Some(default_domain) = default_domain.as_ref() {
            domains.insert(default_domain.domain_id(), Arc::clone(default_domain));

            domains_by_name.insert(
                default_domain.domain_name().to_string(),
                Arc::clone(default_domain),
            );
        }

        let agent = if config::configuration().mux_enable_ssh_agent {
            Some(AgentProxy::new())
        } else {
            None
        };

        Self {
            runtime_server_id: new_runtime_server_id(),
            tabs: RwLock::new(HashMap::new()),
            panes: RwLock::new(HashMap::new()),
            windows: RwLock::new(HashMap::new()),
            default_domain: RwLock::new(default_domain),
            domains_by_name: RwLock::new(domains_by_name),
            domains: RwLock::new(domains),
            subscribers: RwLock::new(HashMap::new()),
            banner: RwLock::new(None),
            clients: RwLock::new(HashMap::new()),
            client_registrations: RwLock::new(HashMap::new()),
            frontend_lease: Mutex::new(FrontendLeaseState::default()),
            tab_resize_notifications: Mutex::new(TabResizeNotificationState::default()),
            #[cfg(test)]
            frontend_geometry_failures: Mutex::new(std::collections::VecDeque::new()),
            palette_advisories: Mutex::new(PaletteAdvisoryState::default()),
            identity: RwLock::new(None),
            num_panes_by_workspace: RwLock::new(HashMap::new()),
            main_thread_id: std::thread::current().id(),
            agent,
        }
    }

    /// Identity of this running mux process. This is intentionally ephemeral:
    /// today a mux restart cannot preserve any pane whose environment refers
    /// to the old value.
    pub fn runtime_server_id(&self) -> &str {
        &self.runtime_server_id
    }

    fn get_default_workspace(&self) -> String {
        let config = configuration();
        config
            .default_workspace
            .as_deref()
            .unwrap_or(DEFAULT_WORKSPACE)
            .to_string()
    }

    pub fn is_main_thread(&self) -> bool {
        std::thread::current().id() == self.main_thread_id
    }

    fn recompute_pane_count(&self) {
        let mut count = HashMap::new();
        for window in self.windows.read().values() {
            let workspace = window.get_workspace();
            for tab in window.iter() {
                *count.entry(workspace.to_string()).or_insert(0) += match tab.count_panes() {
                    Some(n) => n,
                    None => {
                        // Busy: abort this and we'll retry later
                        return;
                    }
                };
            }
        }
        *self.num_panes_by_workspace.write() = count;
    }

    pub fn client_had_input(&self, client_id: &ClientId) {
        if let Some(info) = self.clients.write().get_mut(client_id) {
            info.update_last_input();
        }
        if let Some(agent) = &self.agent {
            agent.update_target();
        }
    }

    /// Transport-scoped input accounting. Returns false for work from a
    /// connection that has already been superseded by a reconnect.
    pub fn registered_client_had_input(
        &self,
        client_id: &ClientId,
        registration: ClientRegistrationId,
    ) -> bool {
        let registrations = self.client_registrations.read();
        if registrations.get(client_id) != Some(&registration) {
            return false;
        }
        self.client_had_input(client_id);
        true
    }

    fn validate_frontend_viewport(
        &self,
        tab_id: TabId,
        viewport: &FrontendViewport,
    ) -> anyhow::Result<()> {
        let tab = self
            .get_tab(tab_id)
            .ok_or_else(|| anyhow!("no such tab {tab_id}"))?;
        if viewport.size().rows == 0 || viewport.size().cols == 0 {
            anyhow::bail!("viewport for tab {tab_id} has zero rows or columns");
        }
        if let FrontendViewport::Native { panes, .. } = viewport {
            for pane in panes {
                if !tab.contains_pane(pane.pane_id) {
                    anyhow::bail!(
                        "pane {} is not contained by viewport tab {tab_id}",
                        pane.pane_id
                    );
                }
                if pane.size.rows == 0 || pane.size.cols == 0 {
                    anyhow::bail!("viewport for pane {} is empty", pane.pane_id);
                }
            }
            let frames = panes
                .iter()
                .map(|pane| (pane.pane_id, pane.frame))
                .collect::<Vec<_>>();
            tab.validate_frontend_frames(viewport.size(), &frames)?;
        }
        Ok(())
    }

    fn access_state_locked(lease: &FrontendLeaseState) -> FrontendAccessState {
        FrontendAccessState {
            mode: lease.access_mode,
            owner: match lease.access_mode {
                FrontendAccessMode::TmuxLatest => None,
                FrontendAccessMode::Handoff => lease.handoff_owner.clone(),
            },
            generation: lease.access_generation,
        }
    }

    pub fn frontend_access_state(&self) -> FrontendAccessState {
        Self::access_state_locked(&self.frontend_lease.lock())
    }

    /// Install the mode loaded by the mux-server persistence layer. This is
    /// intentionally one-shot so a configuration reload cannot overwrite a
    /// mode selected while this server process is running.
    pub fn initialize_frontend_access_mode(&self, mode: FrontendAccessMode) {
        let mut lease = self.frontend_lease.lock();
        if lease.access_initialized {
            return;
        }
        lease.access_mode = mode;
        lease.access_initialized = true;
    }

    pub fn frontend_access_mode_is_initialized(&self) -> bool {
        self.frontend_lease.lock().access_initialized
    }

    pub fn client_has_frontend_access(&self, client_id: &ClientId) -> bool {
        let lease = self.frontend_lease.lock();
        match lease.access_mode {
            FrontendAccessMode::TmuxLatest => true,
            // Non-rendering clients (never advertised a viewport) pass: the
            // gate arbitrates between screens, and they are not one. This is
            // also what lets a headless `thinkterm cli` work at all in
            // Handoff -- with no owner and `handoff_ever_owned` set, nothing
            // else would ever answer true for it.
            FrontendAccessMode::Handoff => {
                lease.handoff_owner.as_ref() == Some(client_id)
                    || !lease.ever_rendered.contains(client_id)
            }
        }
    }

    /// Check the server-wide interaction gate for one live transport.  This
    /// does not claim anything: chrome actions such as closing a pane must be
    /// rejected for B's opaque followers, but must not silently take A's
    /// layout lease just because they originated outside the terminal area.
    pub fn registered_client_has_frontend_access(
        &self,
        client_id: &ClientId,
        registration: ClientRegistrationId,
    ) -> Option<bool> {
        if self.client_registrations.read().get(client_id) != Some(&registration) {
            return None;
        }
        Some(self.client_has_frontend_access(client_id))
    }

    fn viewport_state(&self, tab_id: TabId) -> Option<FrontendViewportState> {
        let tab = self.get_tab(tab_id)?;
        let lease = self.frontend_lease.lock();
        let state = lease.tabs.get(&tab_id)?;
        Some(FrontendViewportState {
            tab_id,
            owner: state.owner.clone(),
            canonical_size: tab.get_size(),
            view: state.view.clone(),
            generation: state.generation,
            access: Self::access_state_locked(&lease),
        })
    }

    /// Record what the owner of a tab's viewport is looking at.
    ///
    /// Only the owner may write it, for the same reason only the owner sets the
    /// size: a renderer that is not being used does not get to move everyone
    /// else. Returns whether anything changed, so a caller can avoid publishing
    /// a scroll that already matches.
    pub fn set_client_view(&self, client_id: &ClientId, tab_id: TabId, view: FrontendView) -> bool {
        let changed = {
            let mut lease = self.frontend_lease.lock();
            let may_publish = match lease.access_mode {
                FrontendAccessMode::TmuxLatest => {
                    lease
                        .tabs
                        .get(&tab_id)
                        .and_then(|state| state.owner.as_ref())
                        == Some(client_id)
                }
                FrontendAccessMode::Handoff => lease.handoff_owner.as_ref() == Some(client_id),
            };
            if !may_publish {
                return false;
            }
            let Some(state) = lease.tabs.get_mut(&tab_id) else {
                return false;
            };
            if state.view.as_ref() == Some(&view) {
                false
            } else {
                state.view = Some(view);
                true
            }
        };
        if changed {
            self.publish_frontend_viewport_state(tab_id);
        }
        changed
    }

    /// Record one renderer's desired viewport for one tab. A passive resize
    /// never steals an established owner. The first renderer after server
    /// startup seeds B's global handoff owner; after that owner disconnects,
    /// passive reports remain blocked until an explicit claim.
    pub fn set_client_viewport(
        &self,
        client_id: &ClientId,
        tab_id: TabId,
        viewport: FrontendViewport,
    ) -> anyhow::Result<FrontendViewportState> {
        self.validate_frontend_viewport(tab_id, &viewport)?;
        let (should_apply, should_publish, access_changed) = {
            let mut lease = self.frontend_lease.lock();
            lease.ever_rendered.insert(client_id.clone());
            let mut access_changed = false;
            if lease.access_mode == FrontendAccessMode::Handoff
                && lease.handoff_owner.is_none()
                && !lease.handoff_ever_owned
            {
                lease.handoff_owner = Some(client_id.clone());
                lease.handoff_ever_owned = true;
                access_changed = true;
            }
            let mode = lease.access_mode;
            let handoff_owner = lease.handoff_owner.clone();
            let state = lease.tabs.entry(tab_id).or_default();
            let viewport_changed = state.viewports.get(client_id) != Some(&viewport);
            if viewport_changed {
                state.viewports.insert(client_id.clone(), viewport.clone());
            }
            let can_drive = match mode {
                FrontendAccessMode::TmuxLatest => {
                    if state.owner.is_none() {
                        state.owner = Some(client_id.clone());
                    }
                    state.owner.as_ref() == Some(client_id)
                }
                FrontendAccessMode::Handoff => handoff_owner.as_ref() == Some(client_id),
            };
            let owner_changed = can_drive && state.owner.as_ref() != Some(client_id);
            if owner_changed {
                state.owner = Some(client_id.clone());
                state.view = None;
            }
            let should_apply = can_drive && (owner_changed || viewport_changed || access_changed);
            (should_apply, should_apply, access_changed)
        };
        if should_apply {
            if let Err(err) = self.apply_frontend_viewport(tab_id, &viewport) {
                return Err(self.fail_closed_frontend_geometry(tab_id, None, err));
            }
        }
        if access_changed {
            self.publish_frontend_access_state();
        }
        if should_publish {
            self.publish_frontend_viewport_state(tab_id);
        }
        self.viewport_state(tab_id)
            .ok_or_else(|| anyhow!("viewport state for tab {tab_id} disappeared"))
    }

    /// Transport-scoped viewport update. Old handlers can still have work in
    /// the mux queue after a reconnect; reject it instead of applying stale
    /// geometry to the replacement connection.
    pub fn set_registered_client_viewport(
        &self,
        client_id: &ClientId,
        registration: ClientRegistrationId,
        tab_id: TabId,
        viewport: FrontendViewport,
    ) -> anyhow::Result<Option<FrontendViewportState>> {
        let registrations = self.client_registrations.read();
        if registrations.get(client_id) != Some(&registration) {
            return Ok(None);
        }
        drop(registrations);
        self.set_client_viewport(client_id, tab_id, viewport)
            .map(Some)
    }

    pub fn client_owns_frontend_lease(&self, client_id: &ClientId, tab_id: TabId) -> bool {
        let lease = self.frontend_lease.lock();
        match lease.access_mode {
            FrontendAccessMode::TmuxLatest => {
                lease
                    .tabs
                    .get(&tab_id)
                    .and_then(|state| state.owner.as_ref())
                    == Some(client_id)
            }
            FrontendAccessMode::Handoff => lease.handoff_owner.as_ref() == Some(client_id),
        }
    }

    pub fn current_identity_owns_frontend_lease(&self, tab_id: TabId) -> bool {
        // Lock order: registrations first, dropped before the lease lock is
        // taken — the same order `set_registered_client_viewport` uses.
        let identity = self.active_identity();
        let live: HashSet<ClientId> = self.client_registrations.read().keys().cloned().collect();
        let lease = self.frontend_lease.lock();
        match lease.access_mode {
            // Per-tab ownership is cheap to re-take and is re-seeded by the
            // very next passive report (`set_client_viewport` seeds an
            // ownerless tab unconditionally), so an unclaimed tab belongs to
            // whoever asks — symmetric with the wire path.
            FrontendAccessMode::TmuxLatest => {
                match lease
                    .tabs
                    .get(&tab_id)
                    .and_then(|state| state.owner.as_ref())
                {
                    None => true,
                    Some(owner) => identity.as_deref() == Some(owner),
                }
            }
            FrontendAccessMode::Handoff => match lease.handoff_owner.as_ref() {
                Some(owner) => identity.as_deref() == Some(owner),
                // Bootstrap: nobody has ever rendered, nothing to protect.
                None if !lease.handoff_ever_owned => true,
                // Post-revocation. Symmetric with wire clients
                // (`set_client_viewport` seeds only while
                // `!handoff_ever_owned`): survivors must claim explicitly.
                // The one exception is a sole renderer, which recovers on its
                // own instead of sitting behind a takeover surface with
                // nobody to take the terminal from — this preserves the fix
                // for the measured paralysis (split-divider drags leaving
                // TUIs at their old PTY size) without letting two surviving
                // frontends fight over the geometry.
                None => identity
                    .as_deref()
                    .is_some_and(|me| is_sole_live_renderer(&lease.tabs, &live, me)),
            },
        }
    }

    /// Account for an input PDU and enforce B's global input gate. In A, input
    /// is allowed for every renderer and the most recently advertised local
    /// geometry is used as a compatibility fallback; interactive frontends use
    /// the explicit geometry-bearing claim before forwarding the input.
    pub fn registered_client_had_tab_input(
        &self,
        client_id: &ClientId,
        registration: ClientRegistrationId,
        tab_id: TabId,
    ) -> bool {
        let registrations = self.client_registrations.read();
        if registrations.get(client_id) != Some(&registration) {
            return false;
        }
        drop(registrations);
        self.client_had_input(client_id);
        let (mode, handoff_owner, viewport, ever_rendered) = {
            let lease = self.frontend_lease.lock();
            (
                lease.access_mode,
                lease.handoff_owner.clone(),
                lease
                    .tabs
                    .get(&tab_id)
                    .and_then(|state| state.viewports.get(client_id).cloned()),
                lease.ever_rendered.contains(client_id),
            )
        };
        match mode {
            // Same exemption as `client_has_frontend_access`: input from a
            // client that never renders (CLI automation) is not a second
            // screen fighting the owner.
            FrontendAccessMode::Handoff => {
                handoff_owner.as_ref() == Some(client_id) || !ever_rendered
            }
            FrontendAccessMode::TmuxLatest => {
                if let Some(viewport) = viewport {
                    if let Err(err) = self.claim_frontend_viewport(client_id, tab_id, viewport) {
                        log::error!("failed to apply claimed viewport for tab {tab_id}: {err:#}");
                    }
                }
                true
            }
        }
    }

    /// Explicit renderer claim used by terminal-area interaction and pane
    /// layout changes. Geometry is supplied in this request and applied in the
    /// same main-thread operation as the owner change.
    pub fn claim_registered_client_viewport(
        &self,
        client_id: &ClientId,
        registration: ClientRegistrationId,
        tab_id: TabId,
        viewport: FrontendViewport,
    ) -> anyhow::Result<Option<FrontendViewportState>> {
        let registrations = self.client_registrations.read();
        if registrations.get(client_id) != Some(&registration) {
            return Ok(None);
        }
        drop(registrations);
        self.client_had_input(client_id);
        self.claim_frontend_viewport(client_id, tab_id, viewport)
            .map(Some)
    }

    fn claim_frontend_viewport(
        &self,
        client_id: &ClientId,
        tab_id: TabId,
        viewport: FrontendViewport,
    ) -> anyhow::Result<FrontendViewportState> {
        self.validate_frontend_viewport(tab_id, &viewport)?;
        let unchanged = {
            let lease = self.frontend_lease.lock();
            let effective_owner = match lease.access_mode {
                FrontendAccessMode::TmuxLatest => {
                    lease
                        .tabs
                        .get(&tab_id)
                        .and_then(|state| state.owner.as_ref())
                        == Some(client_id)
                }
                FrontendAccessMode::Handoff => lease.handoff_owner.as_ref() == Some(client_id),
            };
            effective_owner
                && lease.tabs.get(&tab_id).is_some_and(|state| {
                    state.owner.as_ref() == Some(client_id)
                        && state.viewports.get(client_id) == Some(&viewport)
                })
        };
        if unchanged {
            return self
                .viewport_state(tab_id)
                .ok_or_else(|| anyhow!("viewport state for tab {tab_id} disappeared"));
        }
        let (access_changed, mut affected_tabs) = {
            let mut lease = self.frontend_lease.lock();
            lease.ever_rendered.insert(client_id.clone());
            let mode = lease.access_mode;
            let mut access_changed = false;
            let mut affected_tabs = vec![tab_id];
            if mode == FrontendAccessMode::Handoff {
                access_changed = lease.handoff_owner.as_ref() != Some(client_id);
                lease.handoff_owner = Some(client_id.clone());
                lease.handoff_ever_owned = true;
                if access_changed {
                    for (affected_tab_id, state) in &mut lease.tabs {
                        if state.view.take().is_some() && *affected_tab_id != tab_id {
                            affected_tabs.push(*affected_tab_id);
                        }
                    }
                }
            }
            let state = lease.tabs.entry(tab_id).or_default();
            state.viewports.insert(client_id.clone(), viewport.clone());
            state.owner = Some(client_id.clone());
            state.view = None;
            (access_changed, affected_tabs)
        };
        if access_changed {
            self.publish_frontend_access_state();
        }
        // Commit and publish B's new owner before Tab::resize can emit a
        // synchronous TabResized notification. Any observer reacting to the
        // new geometry must already mask the prior owner.
        if let Err(err) = self.apply_frontend_viewport(tab_id, &viewport) {
            return Err(self.fail_closed_frontend_geometry(tab_id, None, err));
        }
        affected_tabs.sort_unstable();
        affected_tabs.dedup();
        for affected_tab_id in affected_tabs {
            self.publish_frontend_viewport_state(affected_tab_id);
        }
        self.viewport_state(tab_id)
            .ok_or_else(|| anyhow!("viewport state for tab {tab_id} disappeared"))
    }

    /// In-process GUI equivalent of the wire claim. The GUI identity is
    /// already registered with this mux, but there is no transport generation
    /// involved because the call never crosses a connection.
    pub fn claim_local_frontend_viewport(
        &self,
        client_id: &ClientId,
        tab_id: TabId,
        viewport: FrontendViewport,
    ) -> anyhow::Result<FrontendViewportState> {
        self.client_had_input(client_id);
        self.claim_frontend_viewport(client_id, tab_id, viewport)
    }

    /// Validate a mode transition without mutating state. The server uses this
    /// before atomically persisting the selected mode.
    pub fn validate_registered_frontend_access_mode_change(
        &self,
        client_id: &ClientId,
        registration: ClientRegistrationId,
        mode: FrontendAccessMode,
        tab_id: TabId,
        viewport: &FrontendViewport,
    ) -> anyhow::Result<()> {
        if self.client_registrations.read().get(client_id) != Some(&registration) {
            anyhow::bail!("client connection was superseded");
        }
        self.validate_frontend_access_mode_change(client_id, mode, tab_id, viewport)
    }

    pub fn validate_frontend_access_mode_change(
        &self,
        client_id: &ClientId,
        mode: FrontendAccessMode,
        tab_id: TabId,
        viewport: &FrontendViewport,
    ) -> anyhow::Result<()> {
        self.validate_frontend_viewport(tab_id, viewport)?;
        let lease = self.frontend_lease.lock();
        if lease.access_mode == mode {
            return Ok(());
        }
        let authorized = match lease.access_mode {
            FrontendAccessMode::Handoff => lease.handoff_owner.as_ref() == Some(client_id),
            FrontendAccessMode::TmuxLatest => {
                lease
                    .tabs
                    .get(&tab_id)
                    .and_then(|state| state.owner.as_ref())
                    == Some(client_id)
            }
        };
        if !authorized {
            anyhow::bail!("only the current frontend owner may change access mode");
        }
        Ok(())
    }

    /// Commit a previously validated mode transition. The caller runs this on
    /// the mux main thread after persistence succeeds.
    pub fn set_registered_frontend_access_mode(
        &self,
        client_id: &ClientId,
        registration: ClientRegistrationId,
        mode: FrontendAccessMode,
        tab_id: TabId,
        viewport: FrontendViewport,
    ) -> anyhow::Result<FrontendAccessState> {
        if self.client_registrations.read().get(client_id) != Some(&registration) {
            anyhow::bail!("client connection was superseded");
        }
        self.set_frontend_access_mode(client_id, mode, tab_id, viewport)
    }

    pub fn set_frontend_access_mode(
        &self,
        client_id: &ClientId,
        mode: FrontendAccessMode,
        tab_id: TabId,
        viewport: FrontendViewport,
    ) -> anyhow::Result<FrontendAccessState> {
        self.validate_frontend_access_mode_change(client_id, mode, tab_id, &viewport)?;
        let prior_access = self.frontend_access_state();
        if prior_access.mode == mode {
            return Ok(self.frontend_access_state());
        }

        let affected_tabs = {
            let mut lease = self.frontend_lease.lock();
            lease.access_mode = mode;
            lease.access_initialized = true;
            match mode {
                FrontendAccessMode::TmuxLatest => {
                    lease.handoff_owner = None;
                }
                FrontendAccessMode::Handoff => {
                    lease.handoff_owner = Some(client_id.clone());
                    lease.handoff_ever_owned = true;
                }
            }
            let mut affected = Vec::new();
            for (affected_tab_id, state) in &mut lease.tabs {
                if state.view.take().is_some() || *affected_tab_id == tab_id {
                    affected.push(*affected_tab_id);
                }
            }
            let state = lease.tabs.entry(tab_id).or_default();
            state.viewports.insert(client_id.clone(), viewport.clone());
            state.owner = Some(client_id.clone());
            if !affected.contains(&tab_id) {
                affected.push(tab_id);
            }
            affected
        };
        self.publish_frontend_access_state();
        // As with a direct claim, mode/owner is authoritative before resize
        // notifications expose geometry from the newly selected frontend.
        if let Err(err) = self.apply_frontend_viewport(tab_id, &viewport) {
            return Err(self.fail_closed_frontend_geometry(tab_id, Some(prior_access.mode), err));
        }
        for affected_tab_id in affected_tabs {
            self.publish_frontend_viewport_state(affected_tab_id);
        }
        Ok(self.frontend_access_state())
    }

    /// `None` means the transport generation is stale. `Some(false)` means a
    /// live non-owner submitted a legacy pane resize and it must be a no-op.
    pub fn registered_client_may_resize_tab(
        &self,
        client_id: &ClientId,
        registration: ClientRegistrationId,
        tab_id: TabId,
    ) -> Option<bool> {
        if self.client_registrations.read().get(client_id) != Some(&registration) {
            return None;
        }
        let lease = self.frontend_lease.lock();
        Some(match lease.access_mode {
            FrontendAccessMode::Handoff => lease.handoff_owner.as_ref() == Some(client_id),
            FrontendAccessMode::TmuxLatest => match lease.tabs.get(&tab_id) {
                None => true,
                Some(state) => state.owner.as_ref() == Some(client_id),
            },
        })
    }

    fn publish_frontend_access_state(&self) {
        let state = {
            let mut lease = self.frontend_lease.lock();
            let generation = lease.next_generation;
            lease.next_generation = lease.next_generation.saturating_add(1);
            lease.access_generation = generation;
            Self::access_state_locked(&lease)
        };
        self.notify(MuxNotification::FrontendAccessChanged(state));
    }

    fn publish_frontend_viewport_state(&self, tab_id: TabId) {
        let (generation, owner, view, access) = {
            let mut lease = self.frontend_lease.lock();
            let generation = lease.next_generation;
            lease.next_generation = lease.next_generation.saturating_add(1);
            let Some(state) = lease.tabs.get_mut(&tab_id) else {
                return;
            };
            state.generation = generation;
            let owner = state.owner.clone();
            let view = state.view.clone();
            let access = Self::access_state_locked(&lease);
            (generation, owner, view, access)
        };
        let Some(tab) = self.get_tab(tab_id) else {
            return;
        };
        self.notify(MuxNotification::FrontendLeaseChanged(
            FrontendViewportState {
                tab_id,
                owner,
                canonical_size: tab.get_size(),
                view,
                generation,
                access,
            },
        ));
    }

    fn apply_frontend_viewport(
        &self,
        tab_id: TabId,
        viewport: &FrontendViewport,
    ) -> anyhow::Result<()> {
        let tab = self
            .get_tab(tab_id)
            .ok_or_else(|| anyhow!("no such tab {tab_id}"))?;
        let native_frames = match viewport {
            FrontendViewport::Native { panes, .. } => {
                let frames = panes
                    .iter()
                    .map(|pane| (pane.pane_id, pane.frame))
                    .collect::<Vec<_>>();
                let covers_all_stacks = tab.frontend_frames_cover_all_stacks(&frames)?;
                Some((frames, covers_all_stacks))
            }
            FrontendViewport::CellGrid { .. } => None,
        };
        if crate::geometrytrace::trace_enabled() {
            let incoming = match viewport {
                FrontendViewport::CellGrid { size } => {
                    format!("kind=cellgrid in={}", crate::geometrytrace::size(size))
                }
                FrontendViewport::Native { size, panes } => format!(
                    "kind=native in={} covers_all={} in_panes=[{}]",
                    crate::geometrytrace::size(size),
                    native_frames
                        .as_ref()
                        .is_some_and(|(_, covers_all)| *covers_all),
                    panes
                        .iter()
                        .map(|pane| format!(
                            "{}:pty={} frame={}",
                            pane.pane_id,
                            crate::geometrytrace::size(&pane.size),
                            crate::geometrytrace::size(&pane.frame)
                        ))
                        .collect::<Vec<_>>()
                        .join(" ")
                ),
            };
            crate::zoom_trace!(
                "srv.viewport.recv tab={tab_id} {incoming} | {}",
                tab.geometry_trace()
            );
        }
        let mut transaction = self.begin_tab_geometry_transaction(tab_id);
        let result = (|| {
            let size = viewport.size();
            self.frontend_geometry_step("resizing the tab root")?;
            tab.resize(size);

            if let FrontendViewport::Native { panes, .. } = viewport {
                for pane_viewport in panes {
                    let pane = self
                        .get_pane(pane_viewport.pane_id)
                        .ok_or_else(|| anyhow!("no such pane {}", pane_viewport.pane_id))?;
                    if !tab.contains_pane(pane_viewport.pane_id) {
                        anyhow::bail!("pane {} is not in tab {tab_id}", pane_viewport.pane_id);
                    }
                    self.frontend_geometry_step("resizing a native pane")?;
                    pane.resize(pane_viewport.size)?;
                }
                if let Some((frames, true)) = native_frames.as_ref() {
                    tab.rebuild_splits_sizes_from_frontend_frames(frames)?;
                }
                return Ok(());
            }

            for positioned in tab.iter_panes() {
                let target = pane_size_for_cell_grid(size, positioned.width, positioned.height);
                self.frontend_geometry_step("resizing a cell-grid pane")?;
                positioned.pane.resize(target)?;
            }
            Ok(())
        })();
        if result.is_ok() {
            transaction.commit();
        }
        crate::zoom_trace!(
            "srv.viewport.done tab={tab_id} ok={} | {}",
            result.is_ok(),
            tab.geometry_trace()
        );
        result
    }

    #[cfg(test)]
    fn frontend_geometry_step(&self, what: &str) -> anyhow::Result<()> {
        if self
            .frontend_geometry_failures
            .lock()
            .pop_front()
            .unwrap_or(false)
        {
            anyhow::bail!("injected frontend geometry failure while {what}");
        }
        Ok(())
    }

    #[cfg(not(test))]
    fn frontend_geometry_step(&self, _what: &str) -> anyhow::Result<()> {
        Ok(())
    }

    fn fail_closed_frontend_geometry(
        &self,
        tab_id: TabId,
        restore_mode: Option<FrontendAccessMode>,
        apply_error: anyhow::Error,
    ) -> anyhow::Error {
        let (mode, mut affected_tabs) = {
            let mut lease = self.frontend_lease.lock();
            if let Some(mode) = restore_mode {
                lease.access_mode = mode;
                lease.access_initialized = true;
            }
            let mode = lease.access_mode;
            let mut affected = Vec::new();
            match mode {
                FrontendAccessMode::Handoff => {
                    lease.handoff_owner = None;
                    lease.handoff_ever_owned = true;
                    for (affected_tab_id, state) in &mut lease.tabs {
                        state.owner = None;
                        state.view = None;
                        affected.push(*affected_tab_id);
                    }
                }
                FrontendAccessMode::TmuxLatest => {
                    lease.handoff_owner = None;
                    let state = lease.tabs.entry(tab_id).or_default();
                    state.owner = None;
                    state.view = None;
                    affected.push(tab_id);
                }
            }
            if !lease.tabs.contains_key(&tab_id) {
                lease.tabs.entry(tab_id).or_default();
                affected.push(tab_id);
            }
            (mode, affected)
        };

        affected_tabs.sort_unstable();
        affected_tabs.dedup();
        self.publish_frontend_access_state();
        for affected_tab_id in affected_tabs {
            self.publish_frontend_viewport_state(affected_tab_id);
        }
        self.notify(MuxNotification::TabResized(tab_id));
        anyhow!(
            "applying frontend geometry for tab {tab_id} failed; the {mode:?} lease was revoked: {apply_error:#}"
        )
    }

    pub fn record_input_for_current_identity(&self) {
        let Some(ident) = self.active_identity() else {
            return;
        };
        self.client_had_input(&ident);
        let Some((_domain, _window, tab_id, _pane)) = self.resolve_focused_pane(&ident) else {
            return;
        };
        let viewport = {
            let lease = self.frontend_lease.lock();
            if lease.access_mode != FrontendAccessMode::TmuxLatest {
                return;
            }
            lease
                .tabs
                .get(&tab_id)
                .and_then(|state| state.viewports.get(ident.as_ref()).cloned())
        };
        if let Some(viewport) = viewport {
            if let Err(err) = self.claim_frontend_viewport(&ident, tab_id, viewport) {
                log::error!("failed to claim local frontend viewport for tab {tab_id}: {err:#}");
            }
        }
    }

    pub fn record_focus_for_current_identity(&self, pane_id: PaneId) {
        if let Some(ident) = self.identity.read().as_ref() {
            self.record_focus_for_client(ident, pane_id);
        }
    }

    pub fn resolve_focused_pane(
        &self,
        client_id: &ClientId,
    ) -> Option<(DomainId, WindowId, TabId, PaneId)> {
        let pane_id = self.clients.read().get(client_id)?.focused_pane_id?;
        let (domain, window, tab) = self.resolve_pane_id(pane_id)?;
        Some((domain, window, tab, pane_id))
    }

    pub fn record_focus_for_client(&self, client_id: &ClientId, pane_id: PaneId) {
        let mut prior = None;
        if let Some(info) = self.clients.write().get_mut(client_id) {
            prior = info.focused_pane_id;
            info.update_focused_pane(pane_id);
        }

        if prior == Some(pane_id) {
            return;
        }
        // Synthesize focus events
        if let Some(prior_id) = prior {
            if let Some(pane) = self.get_pane(prior_id) {
                pane.focus_changed(false);
            }
        }
        if let Some(pane) = self.get_pane(pane_id) {
            pane.focus_changed(true);
        }
    }

    /// Called by PaneFocused event handlers to reconcile a remote
    /// pane focus event and apply its effects locally
    pub fn focus_pane_and_containing_tab(&self, pane_id: PaneId) -> anyhow::Result<()> {
        let pane = self
            .get_pane(pane_id)
            .ok_or_else(|| anyhow::anyhow!("pane {pane_id} not found"))?;

        let (_domain, window_id, tab_id) = self
            .resolve_pane_id(pane_id)
            .ok_or_else(|| anyhow::anyhow!("can't find {pane_id} in the mux"))?;

        // Focus/activate the containing tab within its window
        {
            let mut win = self
                .get_window_mut(window_id)
                .ok_or_else(|| anyhow::anyhow!("window_id {window_id} not found"))?;
            let tab_idx = win
                .idx_by_id(tab_id)
                .ok_or_else(|| anyhow::anyhow!("tab {tab_id} not in {window_id}"))?;
            win.save_and_then_set_active(tab_idx);
        }

        // Focus/activate the pane locally
        let tab = self
            .get_tab(tab_id)
            .ok_or_else(|| anyhow::anyhow!("tab {tab_id} not found"))?;

        tab.set_active_pane_silent(&pane);

        Ok(())
    }

    pub fn register_client(&self, client_id: Arc<ClientId>) -> ClientRegistrationId {
        let registration = ClientRegistrationId::new();
        let key = (*client_id).clone();
        {
            // Registration generation and client metadata are one logical
            // replacement. Keep the generation lock until both are visible so
            // a stale Drop cannot remove the newly reconnected client between
            // the two writes.
            let mut registrations = self.client_registrations.write();
            let mut clients = self.clients.write();
            clients.insert(key.clone(), ClientInfo::new(client_id));
            registrations.insert(key.clone(), registration);
        }
        registration
    }

    pub fn iter_clients(&self) -> Vec<ClientInfo> {
        self.clients
            .read()
            .values()
            .map(|info| info.clone())
            .collect()
    }

    /// Returns a list of the unique workspace names known to the mux.
    /// This is taken from all known windows.
    pub fn iter_workspaces(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .windows
            .read()
            .values()
            .map(|w| w.get_workspace().to_string())
            .collect();
        names.sort();
        names.dedup();
        names
    }

    /// Generate a new unique workspace name
    pub fn generate_workspace_name(&self) -> String {
        let used = self.iter_workspaces();
        for candidate in names::Generator::default() {
            if !used.contains(&candidate) {
                return candidate;
            }
        }
        unreachable!();
    }

    /// Returns the effective active workspace name
    pub fn active_workspace(&self) -> String {
        self.identity
            .read()
            .as_ref()
            .and_then(|ident| {
                self.clients
                    .read()
                    .get(&ident)
                    .and_then(|info| info.active_workspace.clone())
            })
            .unwrap_or_else(|| self.get_default_workspace())
    }

    /// Returns the effective active workspace name for a given client
    pub fn active_workspace_for_client(&self, ident: &Arc<ClientId>) -> String {
        self.clients
            .read()
            .get(&ident)
            .and_then(|info| info.active_workspace.clone())
            .unwrap_or_else(|| self.get_default_workspace())
    }

    /// The workspace a request from `client_id` should file new windows under.
    ///
    /// Exactly equivalent to `active_workspace()` evaluated with `client_id`
    /// installed as the global identity, but without touching that global.
    /// Server handlers must resolve workspace attribution with this *before*
    /// they await: an `IdentityHolder` held across a suspension point
    /// publishes the wrong identity to every other main-thread task, and its
    /// restore is not LIFO-safe.
    pub fn active_workspace_for_optional_client(
        &self,
        client_id: Option<&Arc<ClientId>>,
    ) -> String {
        match client_id {
            Some(client_id) => self.active_workspace_for_client(client_id),
            None => self.get_default_workspace(),
        }
    }

    pub fn set_active_workspace_for_client(&self, ident: &Arc<ClientId>, workspace: &str) {
        let mut clients = self.clients.write();
        if let Some(info) = clients.get_mut(&ident) {
            info.active_workspace.replace(workspace.to_string());
            self.notify(MuxNotification::ActiveWorkspaceChanged(ident.clone()));
        }
    }

    /// Assigns the active workspace name for the current identity
    pub fn set_active_workspace(&self, workspace: &str) {
        if let Some(ident) = self.identity.read().clone() {
            self.set_active_workspace_for_client(&ident, workspace);
        }
    }

    pub fn rename_workspace(&self, old_workspace: &str, new_workspace: &str) {
        if old_workspace == new_workspace {
            return;
        }
        self.notify(MuxNotification::WorkspaceRenamed {
            old_workspace: old_workspace.to_string(),
            new_workspace: new_workspace.to_string(),
        });

        for window in self.windows.write().values_mut() {
            if window.get_workspace() == old_workspace {
                window.set_workspace(new_workspace);
            }
        }
        self.recompute_pane_count();
        for client in self.clients.write().values_mut() {
            if client.active_workspace.as_deref() == Some(old_workspace) {
                client.active_workspace.replace(new_workspace.to_string());
                self.notify(MuxNotification::ActiveWorkspaceChanged(
                    client.client_id.clone(),
                ));
            }
        }
    }

    /// Overrides the current client identity.
    /// Returns `IdentityHolder` which will restore the prior identity
    /// when it is dropped.
    /// This can be used to change the identity for the duration of a block.
    ///
    /// # This guard must not be held across an `.await`
    ///
    /// The identity is a single process-global slot and `IdentityHolder`'s
    /// restore is not LIFO-safe across interleaved tasks. Suspending with one
    /// installed publishes the wrong identity to every other main-thread
    /// task, including GUI mux subscribers (which run synchronously from
    /// `notify`) and `record_input_for_current_identity`, which in
    /// `TmuxLatest` mode will *claim* a tab's viewport for whoever happens to
    /// be installed. Resolve what you need synchronously (e.g.
    /// [`Mux::active_workspace_for_optional_client`]) and pass it as data.
    /// `clippy.toml` arms `await_holding_invalid_type` for `IdentityHolder`.
    pub fn with_identity(&self, id: Option<Arc<ClientId>>) -> IdentityHolder {
        let prior = self.replace_identity(id);
        IdentityHolder { prior }
    }

    /// Replace the identity, returning the prior identity
    pub fn replace_identity(&self, id: Option<Arc<ClientId>>) -> Option<Arc<ClientId>> {
        std::mem::replace(&mut *self.identity.write(), id)
    }

    /// Returns the active identity
    pub fn active_identity(&self) -> Option<Arc<ClientId>> {
        self.identity.read().clone()
    }

    /// Remove a client from the ordinary mux client list.
    /// Palette ownership is connection-scoped and is cleaned independently.
    pub fn unregister_client(&self, client_id: &ClientId, registration: ClientRegistrationId) {
        {
            let mut registrations = self.client_registrations.write();
            if registrations.get(client_id) != Some(&registration) {
                return;
            }
            self.clients.write().remove(client_id);
            registrations.remove(client_id);
        }
        let (affected, access_changed) = {
            let mut lease = self.frontend_lease.lock();
            lease.ever_rendered.remove(client_id);
            let mut affected = Vec::new();
            for (tab_id, state) in &mut lease.tabs {
                state.viewports.remove(client_id);
                if state.owner.as_ref() != Some(client_id) {
                    continue;
                }
                state.owner = None;
                state.view = None;
                affected.push(*tab_id);
            }
            let access_changed = lease.access_mode == FrontendAccessMode::Handoff
                && lease.handoff_owner.as_ref() == Some(client_id);
            if access_changed {
                // Do not select a surviving renderer automatically. Every
                // remaining device stays opaque until one explicitly clicks,
                // wheels or swipes the takeover surface.
                lease.handoff_owner = None;
                lease.handoff_ever_owned = true;
            }
            (affected, access_changed)
        };

        if access_changed {
            self.publish_frontend_access_state();
        }
        for tab_id in affected {
            self.publish_frontend_viewport_state(tab_id);
        }
    }

    /// Register one transport connection for palette advice. The returned
    /// token must be captured by queued handlers so work from a disconnected
    /// connection cannot be mistaken for a later connection with the same
    /// `ClientId`.
    pub fn register_palette_session(&self, client_id: &ClientId) -> PaletteSessionId {
        self.palette_advisories.lock().register(client_id)
    }

    /// Remove exactly one transport connection and return any pane palette
    /// selections that need to be applied as a result of owner fallback.
    pub fn unregister_palette_session(
        &self,
        session_id: PaletteSessionId,
    ) -> Vec<PaletteSelectionChange> {
        self.palette_advisories.lock().remove_session(session_id)
    }

    /// Immediately reject further queued work from a disconnected transport.
    /// Final removal and pane fallback are deliberately a separate operation
    /// so the caller can serialize their application on the mux main thread.
    pub fn deactivate_palette_session(&self, session_id: PaletteSessionId) -> bool {
        self.palette_advisories
            .lock()
            .deactivate_session(session_id)
    }

    /// Store a client's configured palette. A return value means that client
    /// currently owns this pane and the server's OSC query base must update.
    pub fn advise_client_palette(
        &self,
        session_id: PaletteSessionId,
        pane_id: PaneId,
        palette: ColorPalette,
    ) -> Option<ColorPalette> {
        self.palette_advisories
            .lock()
            .advise(session_id, pane_id, palette)
    }

    /// Mark focus or input from a palette-capable client. Background clients
    /// that have not supplied advice cannot take ownership.
    pub fn activate_client_palette(
        &self,
        session_id: PaletteSessionId,
        pane_id: PaneId,
    ) -> Option<ColorPalette> {
        self.palette_advisories.lock().activate(session_id, pane_id)
    }

    pub fn subscribe<F>(&self, subscriber: F)
    where
        F: Fn(MuxNotification) -> bool + 'static + Send + Sync,
    {
        let sub_id = SUB_ID.fetch_add(1, Ordering::Relaxed);
        self.subscribers
            .write()
            .insert(sub_id, Box::new(subscriber));
    }

    pub fn notify(&self, notification: MuxNotification) {
        match notification {
            MuxNotification::TabResized(tab_id) => {
                if self.tab_resize_notifications.lock().defer(tab_id) {
                    return;
                }
                self.notify_immediate(MuxNotification::TabResized(tab_id));
            }
            other => self.notify_immediate(other),
        }
    }

    fn notify_immediate(&self, notification: MuxNotification) {
        let mut subscribers = self.subscribers.write();
        subscribers.retain(|_, notify| notify(notification.clone()));
    }

    fn begin_tab_geometry_transaction(&self, tab_id: TabId) -> TabGeometryTransaction<'_> {
        self.tab_resize_notifications.lock().begin(tab_id);
        TabGeometryTransaction {
            mux: self,
            tab_id,
            commit: false,
        }
    }

    pub fn notify_from_any_thread(notification: MuxNotification) {
        if let Some(mux) = Mux::try_get() {
            if mux.is_main_thread() {
                mux.notify(notification);
                return;
            }
        }
        promise::spawn::spawn_into_main_thread(async {
            if let Some(mux) = Mux::try_get() {
                mux.notify(notification);
            }
        })
        .detach();
    }

    pub fn default_domain(&self) -> Arc<dyn Domain> {
        self.default_domain.read().as_ref().map(Arc::clone).unwrap()
    }

    pub fn set_default_domain(&self, domain: &Arc<dyn Domain>) {
        *self.default_domain.write() = Some(Arc::clone(domain));
    }

    pub fn get_domain(&self, id: DomainId) -> Option<Arc<dyn Domain>> {
        self.domains.read().get(&id).cloned()
    }

    pub fn get_domain_by_name(&self, name: &str) -> Option<Arc<dyn Domain>> {
        self.domains_by_name.read().get(name).cloned()
    }

    pub fn add_domain(&self, domain: &Arc<dyn Domain>) {
        if self.default_domain.read().is_none() {
            *self.default_domain.write() = Some(Arc::clone(domain));
        }
        self.domains
            .write()
            .insert(domain.domain_id(), Arc::clone(domain));
        self.domains_by_name
            .write()
            .insert(domain.domain_name().to_string(), Arc::clone(domain));
    }

    pub fn set_mux(mux: &Arc<Mux>) {
        MUX.lock().replace(Arc::clone(mux));
    }

    pub fn shutdown() {
        MUX.lock().take();
    }

    pub fn get() -> Arc<Mux> {
        Self::try_get().unwrap()
    }

    pub fn try_get() -> Option<Arc<Mux>> {
        MUX.lock().as_ref().map(Arc::clone)
    }

    pub fn get_pane(&self, pane_id: PaneId) -> Option<Arc<dyn Pane>> {
        self.panes.read().get(&pane_id).map(Arc::clone)
    }

    pub fn get_tab(&self, tab_id: TabId) -> Option<Arc<Tab>> {
        self.tabs.read().get(&tab_id).map(Arc::clone)
    }

    pub fn add_pane(&self, pane: &Arc<dyn Pane>) -> Result<(), Error> {
        if self.panes.read().contains_key(&pane.pane_id()) {
            return Ok(());
        }

        let clipboard: Arc<dyn Clipboard> = Arc::new(MuxClipboard {
            pane_id: pane.pane_id(),
        });
        pane.set_clipboard(&clipboard);

        let downloader: Arc<dyn DownloadHandler> = Arc::new(MuxDownloader {});
        pane.set_download_handler(&downloader);

        self.panes.write().insert(pane.pane_id(), Arc::clone(pane));
        let pane_id = pane.pane_id();
        if let Some(reader) = pane.reader()? {
            let banner = self.banner.read().clone();
            let pane = Arc::downgrade(pane);
            thread::spawn(move || read_from_pane_pty(pane, banner, reader));
        }
        self.recompute_pane_count();
        self.notify(MuxNotification::PaneAdded(pane_id));
        Ok(())
    }

    pub fn add_tab_no_panes(&self, tab: &Arc<Tab>) {
        self.tabs.write().insert(tab.tab_id(), Arc::clone(tab));
        self.recompute_pane_count();
    }

    pub fn add_tab_and_active_pane(&self, tab: &Arc<Tab>) -> Result<(), Error> {
        self.tabs.write().insert(tab.tab_id(), Arc::clone(tab));
        let pane = tab
            .get_active_pane()
            .ok_or_else(|| anyhow!("tab MUST have an active pane"))?;
        self.add_pane(&pane)
    }

    fn remove_pane_internal(&self, pane_id: PaneId) {
        log::debug!("removing pane {}", pane_id);
        self.palette_advisories.lock().remove_pane(pane_id);
        crate::pane::set_frontend_cell_metrics(pane_id, None);
        let mut changed = false;
        if let Some(pane) = self.panes.write().remove(&pane_id).clone() {
            log::debug!("killing pane {}", pane_id);
            pane.kill();
            self.notify(MuxNotification::PaneRemoved(pane_id));
            changed = true;
        }

        if changed {
            self.recompute_pane_count();
        }
    }

    fn remove_tab_internal(&self, tab_id: TabId) -> Option<Arc<Tab>> {
        log::debug!("remove_tab_internal tab {}", tab_id);

        let tab = self.tabs.write().remove(&tab_id)?;
        self.frontend_lease.lock().tabs.remove(&tab_id);

        if let Some(mut windows) = self.windows.try_write() {
            for w in windows.values_mut() {
                w.remove_by_id(tab_id);
            }
        }

        let mut pane_ids = vec![];
        for pane in tab.iter_all_panes() {
            pane_ids.push(pane.pane_id());
        }
        log::debug!("panes to remove: {pane_ids:?}");
        for pane_id in pane_ids {
            self.remove_pane_internal(pane_id);
        }
        self.recompute_pane_count();

        Some(tab)
    }

    fn remove_window_internal(&self, window_id: WindowId) {
        log::debug!("remove_window_internal {}", window_id);

        let window = self.windows.write().remove(&window_id);
        if let Some(window) = window {
            // Gather all the domains referenced by this window
            let mut domains_of_window = HashSet::new();
            for tab in window.iter() {
                for pane in tab.iter_all_panes() {
                    domains_of_window.insert(pane.domain_id());
                }
            }

            for domain_id in domains_of_window {
                // Detach only when NO surviving window still references the
                // domain. One thread's workspace window going away must not
                // sever a client connection that other threads in the same
                // Space are still displaying — that detach ripples out as
                // "every pane of the domain removed", which empties every
                // window and takes the whole process down with it.
                let still_referenced = self.windows.read().values().any(|win| {
                    win.iter().any(|tab| {
                        tab.iter_all_panes()
                            .iter()
                            .any(|pane| pane.domain_id() == domain_id)
                    })
                });
                if still_referenced {
                    continue;
                }
                if let Some(domain) = self.get_domain(domain_id) {
                    if domain.detachable() {
                        log::info!("detaching domain");
                        if let Err(err) = domain.detach() {
                            log::error!(
                                "while detaching domain {domain_id} {}: {err:#}",
                                domain.domain_name()
                            );
                        }
                    }
                }
            }

            for tab in window.iter() {
                self.remove_tab_internal(tab.tab_id());
            }
            self.notify(MuxNotification::WindowRemoved(window_id));
        }
        self.recompute_pane_count();
    }

    pub fn remove_pane(&self, pane_id: PaneId) {
        self.remove_pane_internal(pane_id);
        self.prune_dead_windows();
    }

    pub fn remove_tab(&self, tab_id: TabId) -> Option<Arc<Tab>> {
        let tab = self.remove_tab_internal(tab_id);
        self.prune_dead_windows();
        tab
    }

    pub fn prune_dead_windows(&self) {
        if Activity::count() > 0 {
            log::trace!("prune_dead_windows: Activity::count={}", Activity::count());
            return;
        }
        let live_tab_ids: Vec<TabId> = self.tabs.read().keys().cloned().collect();
        let mut dead_windows = vec![];
        let dead_tab_ids: Vec<TabId>;

        {
            let mut windows = match self.windows.try_write() {
                Some(w) => w,
                None => {
                    // It's ok if our caller already locked it; we can prune later.
                    log::trace!("prune_dead_windows: self.windows already borrowed");
                    return;
                }
            };
            for (window_id, win) in windows.iter_mut() {
                win.prune_dead_tabs(&live_tab_ids);
                if win.is_empty() {
                    log::trace!("prune_dead_windows: window is now empty");
                    dead_windows.push(*window_id);
                }
            }

            dead_tab_ids = self
                .tabs
                .read()
                .iter()
                .filter_map(|(&id, tab)| if tab.is_dead() { Some(id) } else { None })
                .collect();
        }

        for tab_id in dead_tab_ids {
            log::trace!("tab {} is dead", tab_id);
            self.remove_tab_internal(tab_id);
        }

        for window_id in dead_windows {
            log::trace!("window {} is dead", window_id);
            self.remove_window_internal(window_id);
        }

        if self.is_empty() {
            log::trace!("prune_dead_windows: is_empty, send MuxNotification::Empty");
            self.notify(MuxNotification::Empty);
        } else {
            log::trace!("prune_dead_windows: not empty");
        }
    }

    pub fn kill_window(&self, window_id: WindowId) {
        self.remove_window_internal(window_id);
        self.prune_dead_windows();
    }

    pub fn get_window(&self, window_id: WindowId) -> Option<MappedRwLockReadGuard<'_, Window>> {
        if !self.windows.read().contains_key(&window_id) {
            return None;
        }
        Some(RwLockReadGuard::map(self.windows.read(), |windows| {
            windows.get(&window_id).unwrap()
        }))
    }

    pub fn get_window_mut(
        &self,
        window_id: WindowId,
    ) -> Option<MappedRwLockWriteGuard<'_, Window>> {
        if !self.windows.read().contains_key(&window_id) {
            return None;
        }
        Some(RwLockWriteGuard::map(self.windows.write(), |windows| {
            windows.get_mut(&window_id).unwrap()
        }))
    }

    pub fn get_active_tab_for_window(&self, window_id: WindowId) -> Option<Arc<Tab>> {
        let window = self.get_window(window_id)?;
        window.get_active().map(Arc::clone)
    }

    pub fn new_empty_window(
        &self,
        workspace: Option<String>,
        position: Option<GuiPosition>,
    ) -> MuxWindowBuilder {
        self.new_empty_window_for_domain(workspace, position, None)
    }

    /// Like `new_empty_window`, but tags the window with the domain that is
    /// creating it. The tag is set before the window becomes observable via
    /// mux notifications, so the GUI can reliably distinguish domain-owned
    /// windows (remote mux windows, tmux) from user-initiated ones.
    pub fn new_empty_window_for_domain(
        &self,
        workspace: Option<String>,
        position: Option<GuiPosition>,
        origin_domain: Option<DomainId>,
    ) -> MuxWindowBuilder {
        let window = Window::new(workspace, position, origin_domain);
        let window_id = window.window_id();
        self.windows.write().insert(window_id, window);
        MuxWindowBuilder {
            window_id,
            activity: Some(Activity::new()),
            notified: false,
        }
    }

    pub fn add_tab_to_window(&self, tab: &Arc<Tab>, window_id: WindowId) -> anyhow::Result<()> {
        let tab_id = tab.tab_id();
        {
            let mut window = self
                .get_window_mut(window_id)
                .ok_or_else(|| anyhow!("add_tab_to_window: no such window_id {}", window_id))?;
            window.push(tab);
        }
        self.recompute_pane_count();
        self.notify(MuxNotification::TabAddedToWindow { tab_id, window_id });
        Ok(())
    }

    pub fn register_window_ui_surface(
        &self,
        window_id: WindowId,
        surface_id: WindowUiSurfaceId,
    ) -> bool {
        let changed = {
            let mut windows = self.windows.write();
            let Some(window) = windows.get_mut(&window_id) else {
                return false;
            };
            window.add_ui_surface(surface_id)
        };

        if changed {
            self.notify(MuxNotification::WindowInvalidated(window_id));
        }
        changed
    }

    pub fn unregister_window_ui_surface(&self, window_id: WindowId, surface_id: &str) -> bool {
        let changed = {
            let mut windows = self.windows.write();
            let Some(window) = windows.get_mut(&window_id) else {
                return false;
            };
            window.remove_ui_surface(surface_id)
        };

        if changed {
            self.notify(MuxNotification::WindowInvalidated(window_id));
            self.prune_dead_windows();
        }
        changed
    }

    pub fn window_containing_tab(&self, tab_id: TabId) -> Option<WindowId> {
        for w in self.windows.read().values() {
            for t in w.iter() {
                if t.tab_id() == tab_id {
                    return Some(w.window_id());
                }
            }
        }
        None
    }

    pub fn is_empty(&self) -> bool {
        self.panes.read().is_empty()
            && self
                .windows
                .read()
                .values()
                .all(|window| window.ui_surface_count() == 0)
    }

    pub fn is_workspace_empty(&self, workspace: &str) -> bool {
        *self
            .num_panes_by_workspace
            .read()
            .get(workspace)
            .unwrap_or(&0)
            == 0
    }

    pub fn is_active_workspace_empty(&self) -> bool {
        let workspace = self.active_workspace();
        self.is_workspace_empty(&workspace)
    }

    pub fn iter_panes(&self) -> Vec<Arc<dyn Pane>> {
        self.panes
            .read()
            .iter()
            .map(|(_, v)| Arc::clone(v))
            .collect()
    }

    pub fn iter_windows_in_workspace(&self, workspace: &str) -> Vec<WindowId> {
        let mut windows: Vec<WindowId> = self
            .windows
            .read()
            .iter()
            .filter_map(|(k, w)| {
                if w.get_workspace() == workspace {
                    Some(k)
                } else {
                    None
                }
            })
            .cloned()
            .collect();
        windows.sort();
        windows
    }

    pub fn iter_windows(&self) -> Vec<WindowId> {
        self.windows.read().keys().cloned().collect()
    }

    pub fn iter_domains(&self) -> Vec<Arc<dyn Domain>> {
        self.domains.read().values().cloned().collect()
    }

    pub fn resolve_pane_id(&self, pane_id: PaneId) -> Option<(DomainId, WindowId, TabId)> {
        let mut ids = None;
        for tab in self.tabs.read().values() {
            for pane in tab.iter_all_panes() {
                if pane.pane_id() == pane_id {
                    ids = Some((tab.tab_id(), pane.domain_id()));
                    break;
                }
            }
        }
        let (tab_id, domain_id) = ids?;
        let window_id = self.window_containing_tab(tab_id)?;
        Some((domain_id, window_id, tab_id))
    }

    pub fn pane_stack_tabs(&self, pane_id: PaneId) -> Vec<crate::tab::PaneStackTab> {
        let Some((_domain_id, _window_id, tab_id)) = self.resolve_pane_id(pane_id) else {
            return vec![];
        };
        self.get_tab(tab_id)
            .map(|tab| tab.pane_stack_tabs(pane_id))
            .unwrap_or_default()
    }

    pub fn pane_stack_id(&self, pane_id: PaneId) -> Option<crate::tab::PaneStackId> {
        let (_domain_id, _window_id, tab_id) = self.resolve_pane_id(pane_id)?;
        self.get_tab(tab_id)?.pane_stack_id(pane_id)
    }

    pub fn activate_pane_in_stack(&self, pane_id: PaneId) -> anyhow::Result<()> {
        let (_domain_id, _window_id, tab_id) = self
            .resolve_pane_id(pane_id)
            .ok_or_else(|| anyhow!("pane_id {} invalid", pane_id))?;
        let tab = self
            .get_tab(tab_id)
            .ok_or_else(|| anyhow!("tab_id {} invalid", tab_id))?;
        tab.activate_pane_in_stack(pane_id)?;
        Ok(())
    }

    pub fn domain_was_detached(&self, domain: DomainId) {
        let mut dead_panes = vec![];
        for pane in self.panes.read().values() {
            if pane.domain_id() == domain {
                dead_panes.push(pane.pane_id());
            }
        }

        {
            let mut windows = self.windows.write();
            for (_, win) in windows.iter_mut() {
                for tab in win.iter() {
                    tab.kill_panes_in_domain(domain);
                }
            }
        }

        log::info!("domain detached panes: {:?}", dead_panes);
        for pane_id in dead_panes {
            self.remove_pane_internal(pane_id);
        }

        self.prune_dead_windows();
    }

    pub fn set_banner(&self, banner: Option<String>) {
        *self.banner.write() = banner;
    }

    pub fn resolve_spawn_tab_domain(
        &self,
        // TODO: disambiguate with TabId
        pane_id: Option<PaneId>,
        domain: &config::keyassignment::SpawnTabDomain,
    ) -> anyhow::Result<Arc<dyn Domain>> {
        let domain = match domain {
            SpawnTabDomain::DefaultDomain => self.default_domain(),
            SpawnTabDomain::CurrentPaneDomain => match pane_id {
                Some(pane_id) => {
                    let (pane_domain_id, _window_id, _tab_id) = self
                        .resolve_pane_id(pane_id)
                        .ok_or_else(|| anyhow!("pane_id {} invalid", pane_id))?;
                    self.get_domain(pane_domain_id)
                        .expect("resolve_pane_id to give valid domain_id")
                }
                None => self.default_domain(),
            },
            SpawnTabDomain::DomainId(domain_id) => self
                .get_domain(*domain_id)
                .ok_or_else(|| anyhow!("domain id {} is invalid", domain_id))?,
            SpawnTabDomain::DomainName(name) => {
                self.get_domain_by_name(&name).ok_or_else(|| {
                    let names: Vec<String> = self
                        .domains_by_name
                        .read()
                        .keys()
                        .map(|name| format!("\"{name}\""))
                        .collect();
                    anyhow!(
                        "domain name \"{name}\" is invalid. Possible names are {}.",
                        names.join(", ")
                    )
                })?
            }
        };
        Ok(domain)
    }

    fn resolve_cwd(
        &self,
        command_dir: Option<String>,
        pane: Option<Arc<dyn Pane>>,
        target_domain: DomainId,
        policy: CachePolicy,
    ) -> Option<String> {
        command_dir.or_else(|| {
            match pane {
                Some(pane) if pane.domain_id() == target_domain => pane
                    .get_current_working_dir(policy)
                    .and_then(|url| {
                        percent_decode_str(url.path())
                            .decode_utf8()
                            .ok()
                            .map(|path| path.into_owned())
                    })
                    .map(|path| {
                        // On Windows the file URI can produce a path like:
                        // `/C:\Users` which is valid in a file URI, but the leading slash
                        // is not liked by the windows file APIs, so we strip it off here.
                        let bytes = path.as_bytes();
                        if bytes.len() > 2 && bytes[0] == b'/' && bytes[2] == b':' {
                            path[1..].to_owned()
                        } else {
                            path
                        }
                    }),
                _ => None,
            }
        })
    }

    /// Move an already-registered pane into a new split without ever
    /// leaving it detached if the insertion fails.
    pub fn move_pane_to_split(
        &self,
        src_pane_id: PaneId,
        target_tab_id: TabId,
        target_pane_id: PaneId,
        request: SplitRequest,
    ) -> anyhow::Result<Arc<dyn Pane>> {
        if src_pane_id == target_pane_id {
            anyhow::bail!("cannot split pane {src_pane_id} relative to itself");
        }

        let target_tab = self
            .get_tab(target_tab_id)
            .ok_or_else(|| anyhow!("tab_id {target_tab_id} is invalid"))?;
        let target_index = target_tab
            .pane_index_for_pane(target_pane_id)
            .ok_or_else(|| anyhow!("pane_id {target_pane_id} is not in tab {target_tab_id}"))?;
        target_tab
            .validate_split_request(target_index, request)
            .with_context(|| format!("cannot split pane {target_pane_id}"))?;

        // This is a local topology primitive and does not require either tab
        // to be attached to a window yet.
        let src_tab = self
            .tabs
            .read()
            .values()
            .find(|tab| tab.pane_index_for_pane(src_pane_id).is_some())
            .cloned()
            .ok_or_else(|| anyhow!("pane {src_pane_id} not found in any tab"))?;
        let src_tab_id = src_tab.tab_id();
        let pane = src_tab.remove_pane(src_pane_id).ok_or_else(|| {
            anyhow!("pane {src_pane_id} not found in its containing tab {src_tab_id}")
        })?;

        // Removing a pane can renumber the destination leaves when source
        // and target are in the same tab.
        let final_target_index = match target_tab.pane_index_for_pane(target_pane_id) {
            Some(index) => index,
            None => {
                src_tab.rehome_orphan_pane(&pane);
                anyhow::bail!("target pane {target_pane_id} vanished while moving pane");
            }
        };

        if let Err(err) =
            target_tab.split_and_insert(final_target_index, request, Arc::clone(&pane))
        {
            src_tab.rehome_orphan_pane(&pane);
            return Err(err);
        }

        // split_and_insert only selects the inserted pane when it is the
        // right/bottom half. A move is an explicit user action, so make the
        // moved pane active for all four directions. Doing this here also
        // makes the server's authoritative tree agree with client mirrors
        // before it is serialized for a resync.
        target_tab.set_active_pane(&pane);

        if src_tab.is_dead() {
            self.remove_tab(src_tab_id);
        }

        Ok(pane)
    }

    pub async fn split_pane(
        &self,
        // TODO: disambiguate with TabId
        pane_id: PaneId,
        request: SplitRequest,
        source: SplitSource,
        domain: config::keyassignment::SpawnTabDomain,
    ) -> anyhow::Result<(Arc<dyn Pane>, TerminalSize)> {
        let (_pane_domain_id, window_id, tab_id) = self
            .resolve_pane_id(pane_id)
            .ok_or_else(|| anyhow!("pane_id {} invalid", pane_id))?;

        let domain = self
            .resolve_spawn_tab_domain(Some(pane_id), &domain)
            .context("resolve_spawn_tab_domain")?;

        if domain.state() == DomainState::Detached {
            domain.attach(Some(window_id)).await?;
        }

        let current_pane = self
            .get_pane(pane_id)
            .ok_or_else(|| anyhow!("pane_id {} is invalid", pane_id))?;
        let term_config = current_pane.get_config();

        let source = match source {
            SplitSource::Spawn {
                command,
                command_dir,
            } => SplitSource::Spawn {
                command,
                command_dir: self.resolve_cwd(
                    command_dir,
                    Some(Arc::clone(&current_pane)),
                    domain.domain_id(),
                    CachePolicy::FetchImmediate,
                ),
            },
            other => other,
        };

        let pane = domain.split_pane(source, tab_id, pane_id, request).await?;
        // A moved pane is already registered, so PaneAdded cannot advertise
        // the changed tree to mux clients. Always publish the structural
        // mutation; clients coalesce it into an authoritative resync.
        self.notify(MuxNotification::TabResized(tab_id));
        if let Some(config) = term_config {
            pane.set_config(config);
        }

        // FIXME: clipboard

        let dims = pane.get_dimensions();

        let size = TerminalSize {
            cols: dims.cols,
            rows: dims.viewport_rows,
            pixel_height: 0, // FIXME: split pane pixel dimensions
            pixel_width: 0,
            dpi: dims.dpi,
        };

        Ok((pane, size))
    }

    pub async fn spawn_pane_in_stack(
        &self,
        pane_id: PaneId,
        domain: SpawnTabDomain,
        command: Option<CommandBuilder>,
        command_dir: Option<String>,
        size: TerminalSize,
    ) -> anyhow::Result<Arc<dyn Pane>> {
        let (pane_domain_id, window_id, tab_id) = self
            .resolve_pane_id(pane_id)
            .ok_or_else(|| anyhow!("pane_id {} invalid", pane_id))?;
        let tab = self
            .get_tab(tab_id)
            .ok_or_else(|| anyhow!("tab_id {} invalid", tab_id))?;

        if tab.pane_index_for_pane(pane_id).is_none() {
            anyhow::bail!("pane_id {} is not in tab {}", pane_id, tab_id);
        }

        let domain = self
            .resolve_spawn_tab_domain(Some(pane_id), &domain)
            .context("resolve_spawn_tab_domain")?;

        if domain.state() == DomainState::Detached {
            domain.attach(Some(window_id)).await?;
        }

        let current_pane = self
            .get_pane(pane_id)
            .ok_or_else(|| anyhow!("pane_id {} is invalid", pane_id))?;
        let term_config = current_pane.get_config();
        let cwd = self.resolve_cwd(
            command_dir,
            if pane_domain_id == domain.domain_id() {
                Some(current_pane)
            } else {
                None
            },
            domain.domain_id(),
            CachePolicy::FetchImmediate,
        );

        let pane = domain
            .spawn_pane_in_stack(pane_id, size, command.clone(), cwd.clone())
            .await
            .with_context(|| {
                format!(
                    "Spawning pane in domain `{}`: {size:?} command={command:?} cwd={cwd:?}",
                    domain.domain_name()
                )
            })?;

        if let Some(config) = term_config {
            pane.set_config(config);
        }

        tab.add_pane_to_stack(pane_id, Arc::clone(&pane))?;

        Ok(pane)
    }

    /// Move an existing pane into another pane stack in the same tab.
    /// The domain owns the mutation so proxy domains can update their
    /// authoritative server before the next resync.
    pub async fn move_pane_to_stack(
        &self,
        src_pane_id: PaneId,
        target_pane_id: PaneId,
    ) -> anyhow::Result<Arc<dyn Pane>> {
        if src_pane_id == target_pane_id {
            anyhow::bail!("cannot move pane {src_pane_id} onto itself");
        }

        let (src_domain_id, _src_window_id, src_tab_id) = self
            .resolve_pane_id(src_pane_id)
            .ok_or_else(|| anyhow::anyhow!("pane {src_pane_id} not found"))?;
        let (target_domain_id, _target_window_id, target_tab_id) = self
            .resolve_pane_id(target_pane_id)
            .ok_or_else(|| anyhow::anyhow!("pane {target_pane_id} not found"))?;

        if src_domain_id != target_domain_id {
            anyhow::bail!(
                "cannot move pane {src_pane_id} from domain {src_domain_id} \
                 into domain {target_domain_id}"
            );
        }
        if src_tab_id != target_tab_id {
            anyhow::bail!(
                "cannot move pane {src_pane_id} from tab {src_tab_id} \
                 into stack in tab {target_tab_id}"
            );
        }

        let domain = self
            .get_domain(target_domain_id)
            .ok_or_else(|| anyhow::anyhow!("domain {target_domain_id} not found"))?;
        domain
            .move_pane_to_stack(src_pane_id, target_tab_id, target_pane_id)
            .await
    }

    pub async fn move_pane_to_new_tab(
        &self,
        pane_id: PaneId,
        window_id: Option<WindowId>,
        workspace_for_new_window: Option<String>,
    ) -> anyhow::Result<(Arc<Tab>, WindowId)> {
        let (domain_id, _src_window, src_tab) = self
            .resolve_pane_id(pane_id)
            .ok_or_else(|| anyhow::anyhow!("pane {} not found", pane_id))?;

        let domain = self
            .get_domain(domain_id)
            .ok_or_else(|| anyhow::anyhow!("domain {domain_id} of pane {pane_id} not found"))?;

        if let Some((tab, window_id)) = domain
            .move_pane_to_new_tab(pane_id, window_id, workspace_for_new_window.clone())
            .await?
        {
            return Ok((tab, window_id));
        }

        let src_tab = match self.get_tab(src_tab) {
            Some(t) => t,
            None => anyhow::bail!("Invalid tab id {}", src_tab),
        };

        let window_builder;
        let (window_id, size) = if let Some(window_id) = window_id {
            let window = self
                .get_window_mut(window_id)
                .ok_or_else(|| anyhow!("window_id {} not found on this server", window_id))?;
            let tab = window
                .get_active()
                .ok_or_else(|| anyhow!("window {} has no tabs", window_id))?;
            let size = tab.get_size();

            (window_id, size)
        } else {
            window_builder = self.new_empty_window(workspace_for_new_window, None);
            (*window_builder, src_tab.get_size())
        };

        let pane = src_tab
            .remove_pane(pane_id)
            .ok_or_else(|| anyhow::anyhow!("pane {} wasn't in its containing tab!?", pane_id))?;

        let tab = Arc::new(Tab::new(&size));
        tab.assign_pane(&pane);
        pane.resize(size)?;
        self.add_tab_and_active_pane(&tab)?;
        self.add_tab_to_window(&tab, window_id)?;

        if src_tab.is_dead() {
            self.remove_tab(src_tab.tab_id());
        }

        Ok((tab, window_id))
    }

    pub async fn spawn_tab_or_window(
        &self,
        window_id: Option<WindowId>,
        domain: SpawnTabDomain,
        command: Option<CommandBuilder>,
        command_dir: Option<String>,
        size: TerminalSize,
        current_pane_id: Option<PaneId>,
        workspace_for_new_window: String,
        window_position: Option<GuiPosition>,
    ) -> anyhow::Result<(Arc<Tab>, Arc<dyn Pane>, WindowId)> {
        let domain = self
            .resolve_spawn_tab_domain(current_pane_id, &domain)
            .context("resolve_spawn_tab_domain")?;

        let window_builder;

        let (window_id, size, term_config) = if let Some(window_id) = window_id {
            let context = self.existing_window_spawn_context(window_id, size)?;
            (window_id, context.size, context.term_config)
        } else {
            window_builder = self.new_empty_window(Some(workspace_for_new_window), window_position);
            (*window_builder, size, None)
        };

        if domain.state() == DomainState::Detached {
            domain.attach(Some(window_id)).await?;
        }

        let cwd = self.resolve_cwd(
            command_dir,
            match current_pane_id {
                Some(id) => {
                    // Only use the cwd from the current pane if the domain
                    // is the same as the one we are spawning into
                    let (current_domain_id, _, _) = self
                        .resolve_pane_id(id)
                        .ok_or_else(|| anyhow!("pane_id {} invalid", id))?;
                    if current_domain_id == domain.domain_id() {
                        self.get_pane(id)
                    } else {
                        None
                    }
                }
                None => None,
            },
            domain.domain_id(),
            CachePolicy::FetchImmediate,
        );

        let tab = domain
            .spawn(size, command.clone(), cwd.clone(), window_id)
            .await
            .with_context(|| {
                format!(
                    "Spawning in domain `{}`: {size:?} command={command:?} cwd={cwd:?}",
                    domain.domain_name()
                )
            })?;

        let pane = tab
            .get_active_pane()
            .ok_or_else(|| anyhow!("missing active pane on tab!?"))?;

        if let Some(config) = term_config {
            pane.set_config(config);
        }

        // FIXME: clipboard?

        let mut window = self
            .get_window_mut(window_id)
            .ok_or_else(|| anyhow!("no such window!?"))?;
        if let Some(idx) = window.idx_by_id(tab.tab_id()) {
            window.save_and_then_set_active(idx);
        }

        Ok((tab, pane, window_id))
    }

    fn existing_window_spawn_context(
        &self,
        window_id: WindowId,
        requested_size: TerminalSize,
    ) -> anyhow::Result<ExistingWindowSpawnContext> {
        let window = self
            .get_window_mut(window_id)
            .ok_or_else(|| anyhow!("window_id {} not found on this server", window_id))?;
        let Some(tab) = window.get_active() else {
            return Ok(ExistingWindowSpawnContext {
                size: requested_size,
                term_config: None,
            });
        };
        let size = tab.get_size();
        let term_config = tab.get_active_pane().and_then(|pane| pane.get_config());

        Ok(ExistingWindowSpawnContext { size, term_config })
    }
}

fn pane_size_for_cell_grid(size: TerminalSize, cols: usize, rows: usize) -> TerminalSize {
    let cols = cols.max(1);
    let rows = rows.max(1);
    let cell_width = (size.pixel_width != 0)
        .then(|| size.pixel_width / size.cols.max(1))
        .map(|width| width.max(1));
    let cell_height = (size.pixel_height != 0)
        .then(|| size.pixel_height / size.rows.max(1))
        .map(|height| height.max(1));
    TerminalSize {
        rows,
        cols,
        pixel_width: cell_width.map_or(0, |width| cols.saturating_mul(width)),
        pixel_height: cell_height.map_or(0, |height| rows.saturating_mul(height)),
        dpi: size.dpi,
    }
}

pub struct IdentityHolder {
    prior: Option<Arc<ClientId>>,
}

impl Drop for IdentityHolder {
    fn drop(&mut self) {
        if let Some(mux) = Mux::try_get() {
            mux.replace_identity(self.prior.take());
        }
    }
}

#[derive(Debug, Error)]
#[allow(dead_code)]
pub enum SessionTerminated {
    #[error("Process exited: {:?}", status)]
    ProcessStatus { status: ExitStatus },
    #[error("Error: {:?}", err)]
    Error { err: Error },
    #[error("Window Closed")]
    WindowClosed,
}

pub(crate) fn terminal_size_to_pty_size(size: TerminalSize) -> anyhow::Result<PtySize> {
    Ok(PtySize {
        rows: size.rows.try_into()?,
        cols: size.cols.try_into()?,
        pixel_height: size.pixel_height.try_into()?,
        pixel_width: size.pixel_width.try_into()?,
    })
}

struct MuxClipboard {
    pane_id: PaneId,
}

impl Clipboard for MuxClipboard {
    fn set_contents(
        &self,
        selection: ClipboardSelection,
        clipboard: Option<String>,
    ) -> anyhow::Result<()> {
        let mux =
            Mux::try_get().ok_or_else(|| anyhow::anyhow!("MuxClipboard::set_contents: no Mux?"))?;
        mux.notify(MuxNotification::AssignClipboard {
            pane_id: self.pane_id,
            selection,
            clipboard,
        });
        Ok(())
    }
}

struct MuxDownloader {}

impl wezterm_term::DownloadHandler for MuxDownloader {
    fn save_to_downloads(&self, name: Option<String>, data: Vec<u8>) {
        if let Some(mux) = Mux::try_get() {
            mux.notify(MuxNotification::SaveToDownloads {
                name,
                data: Arc::new(data),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tab_geometry_transaction_coalesces_and_is_tab_scoped() {
        let mut state = TabResizeNotificationState::default();
        state.begin(10);
        state.begin(10);
        state.begin(11);

        assert!(state.defer(10));
        assert!(state.defer(10));
        assert!(state.defer(11));
        assert!(!state.finish(10, true), "nested scope is still open");
        assert!(state.finish(10, true), "tab 10 publishes exactly once");
        assert!(state.finish(11, true), "tab 11 is independent");
        assert!(!state.defer(10), "completed tabs are no longer deferred");
    }

    #[test]
    fn failed_tab_geometry_transaction_discards_partial_notification() {
        let mut state = TabResizeNotificationState::default();
        state.begin(12);
        state.begin(12);
        assert!(state.defer(12));
        assert!(!state.finish(12, false));
        assert!(!state.finish(12, true), "an inner abort poisons the batch");
        assert!(!state.pending.contains(&12));
        assert!(!state.depth.contains_key(&12));
        assert!(!state.aborted.contains(&12));
    }

    #[test]
    fn tab_geometry_transaction_publishes_one_final_notification() {
        let mux = Mux::new(None);
        let notifications = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&notifications);
        mux.subscribe(move |notification| {
            if matches!(notification, MuxNotification::TabResized(21)) {
                observed.fetch_add(1, Ordering::Relaxed);
            }
            true
        });

        {
            let mut transaction = mux.begin_tab_geometry_transaction(21);
            mux.notify(MuxNotification::TabResized(21));
            mux.notify(MuxNotification::TabResized(21));
            assert_eq!(notifications.load(Ordering::Relaxed), 0);
            transaction.commit();
        }
        assert_eq!(notifications.load(Ordering::Relaxed), 1);
    }

    fn client_id(id: usize) -> ClientId {
        ClientId {
            hostname: "test-host".to_string(),
            username: "test-user".to_string(),
            pid: id as u32,
            epoch: 1,
            id,
            ssh_auth_sock: None,
        }
    }

    fn frontend_test_size(cols: usize, rows: usize) -> TerminalSize {
        TerminalSize {
            cols,
            rows,
            pixel_width: cols * 8,
            pixel_height: rows * 16,
            dpi: 96,
        }
    }

    fn frontend_test_viewport(cols: usize, rows: usize) -> FrontendViewport {
        FrontendViewport::CellGrid {
            size: frontend_test_size(cols, rows),
        }
    }

    fn frontend_test_tab(mux: &Mux) -> TabId {
        let tab = Arc::new(Tab::new(&frontend_test_size(80, 24)));
        mux.add_tab_no_panes(&tab);
        tab.tab_id()
    }

    #[test]
    fn sole_live_renderer_ignores_itself_and_dead_clients() {
        let me = client_id(150);
        let other = client_id(151);
        let mut live = HashSet::new();
        live.insert(me.clone());
        let mut tabs: HashMap<TabId, TabFrontendLease> = HashMap::new();

        // No viewports anywhere: sole.
        assert!(is_sole_live_renderer(&tabs, &live, &me));

        // Only my own viewport: still sole.
        tabs.entry(1)
            .or_default()
            .viewports
            .insert(me.clone(), frontend_test_viewport(80, 24));
        assert!(is_sole_live_renderer(&tabs, &live, &me));

        // Another client's viewport, but that client is dead: still sole.
        tabs.entry(2)
            .or_default()
            .viewports
            .insert(other.clone(), frontend_test_viewport(60, 20));
        assert!(is_sole_live_renderer(&tabs, &live, &me));

        // Same viewport once the client is live: no longer sole.
        live.insert(other.clone());
        assert!(!is_sole_live_renderer(&tabs, &live, &me));

        // A live client with no viewport anywhere (a `wezterm cli` style
        // non-rendering client) does not count as a renderer.
        tabs.get_mut(&2).unwrap().viewports.remove(&other);
        assert!(is_sole_live_renderer(&tabs, &live, &me));
    }

    #[test]
    fn unclaimed_handoff_lease_belongs_to_the_first_renderer() {
        let mux = Mux::new(None);
        mux.initialize_frontend_access_mode(FrontendAccessMode::Handoff);
        let tab_id = frontend_test_tab(&mux);

        // Bootstrap: nobody has ever rendered.
        assert!(mux.current_identity_owns_frontend_lease(tab_id));
        mux.replace_identity(Some(Arc::new(client_id(160))));
        assert!(mux.current_identity_owns_frontend_lease(tab_id));
    }

    #[test]
    fn revoked_handoff_lease_stays_unowned_while_two_renderers_are_present() {
        let mux = Mux::new(None);
        mux.initialize_frontend_access_mode(FrontendAccessMode::Handoff);
        let tab_id = frontend_test_tab(&mux);
        let gui = Arc::new(client_id(161));
        let tui = Arc::new(client_id(162));
        mux.register_client(Arc::clone(&gui));
        mux.register_client(Arc::clone(&tui));
        {
            let mut lease = mux.frontend_lease.lock();
            lease.handoff_owner = None;
            lease.handoff_ever_owned = true;
            let state = lease.tabs.entry(tab_id).or_default();
            state
                .viewports
                .insert((*gui).clone(), frontend_test_viewport(120, 40));
            state
                .viewports
                .insert((*tui).clone(), frontend_test_viewport(60, 20));
        }

        // Symmetry is the point: neither survivor silently owns the lease.
        mux.replace_identity(Some(Arc::clone(&gui)));
        assert!(!mux.current_identity_owns_frontend_lease(tab_id));
        mux.replace_identity(Some(Arc::clone(&tui)));
        assert!(!mux.current_identity_owns_frontend_lease(tab_id));
    }

    #[test]
    fn revoked_handoff_lease_returns_to_the_sole_surviving_renderer() {
        let mux = Mux::new(None);
        mux.initialize_frontend_access_mode(FrontendAccessMode::Handoff);
        let tab_id = frontend_test_tab(&mux);
        let gui = Arc::new(client_id(163));
        let tui = Arc::new(client_id(164));
        mux.register_client(Arc::clone(&gui));
        let tui_registration = mux.register_client(Arc::clone(&tui));
        {
            let mut lease = mux.frontend_lease.lock();
            lease.handoff_owner = None;
            lease.handoff_ever_owned = true;
            let state = lease.tabs.entry(tab_id).or_default();
            state
                .viewports
                .insert((*gui).clone(), frontend_test_viewport(120, 40));
            state
                .viewports
                .insert((*tui).clone(), frontend_test_viewport(60, 20));
        }
        mux.replace_identity(Some(Arc::clone(&gui)));
        assert!(!mux.current_identity_owns_frontend_lease(tab_id));

        mux.unregister_client(&tui, tui_registration);
        assert!(mux.current_identity_owns_frontend_lease(tab_id));

        // A viewport left behind by a dead client must not block recovery.
        mux.frontend_lease
            .lock()
            .tabs
            .get_mut(&tab_id)
            .unwrap()
            .viewports
            .insert((*tui).clone(), frontend_test_viewport(60, 20));
        assert!(mux.current_identity_owns_frontend_lease(tab_id));
    }

    #[test]
    fn claimed_handoff_lease_is_not_owned_by_a_non_owner() {
        let mux = Mux::new(None);
        mux.initialize_frontend_access_mode(FrontendAccessMode::Handoff);
        let tab_id = frontend_test_tab(&mux);
        let owner = Arc::new(client_id(165));
        let bystander = Arc::new(client_id(166));
        {
            let mut lease = mux.frontend_lease.lock();
            lease.handoff_owner = Some((*owner).clone());
            lease.handoff_ever_owned = true;
        }

        mux.replace_identity(Some(Arc::clone(&bystander)));
        assert!(!mux.current_identity_owns_frontend_lease(tab_id));
        mux.replace_identity(Some(Arc::clone(&owner)));
        assert!(mux.current_identity_owns_frontend_lease(tab_id));
        mux.replace_identity(None);
        assert!(!mux.current_identity_owns_frontend_lease(tab_id));
    }

    #[test]
    fn unclaimed_tmux_latest_tab_is_owned_by_the_asking_renderer() {
        let mux = Mux::new(None);
        mux.initialize_frontend_access_mode(FrontendAccessMode::TmuxLatest);
        let tab_id = frontend_test_tab(&mux);
        let me = Arc::new(client_id(167));
        let other = Arc::new(client_id(168));
        mux.register_client(Arc::clone(&me));
        mux.register_client(Arc::clone(&other));
        {
            let mut lease = mux.frontend_lease.lock();
            let state = lease.tabs.entry(tab_id).or_default();
            state
                .viewports
                .insert((*me).clone(), frontend_test_viewport(120, 40));
            state
                .viewports
                .insert((*other).clone(), frontend_test_viewport(60, 20));
        }

        // Per-tab claims are cheap and symmetric: an ownerless tab belongs to
        // whoever asks, even with another renderer present.
        mux.replace_identity(Some(Arc::clone(&me)));
        assert!(mux.current_identity_owns_frontend_lease(tab_id));

        mux.frontend_lease
            .lock()
            .tabs
            .get_mut(&tab_id)
            .unwrap()
            .owner = Some((*other).clone());
        assert!(!mux.current_identity_owns_frontend_lease(tab_id));
    }

    #[test]
    fn optional_client_workspace_matches_the_identified_fallback() {
        config::use_test_configuration();
        let mux = Mux::new(None);
        let client = Arc::new(client_id(190));
        mux.register_client(Arc::clone(&client));
        mux.set_active_workspace_for_client(&client, "space-2");

        assert_eq!(
            mux.active_workspace_for_optional_client(Some(&client)),
            "space-2"
        );
        // The None case must resolve to the default workspace — exactly what
        // `active_workspace()` produced with no identity installed.
        assert_eq!(
            mux.active_workspace_for_optional_client(None),
            mux.active_workspace()
        );
        assert_eq!(mux.active_workspace_for_optional_client(None), "default");
    }

    #[test]
    fn failed_first_handoff_geometry_revokes_the_automatic_owner() {
        let mux = Mux::new(None);
        mux.initialize_frontend_access_mode(FrontendAccessMode::Handoff);
        let tab_id = frontend_test_tab(&mux);
        let client = client_id(201);
        mux.frontend_geometry_failures.lock().push_back(true);

        assert!(mux
            .set_client_viewport(&client, tab_id, frontend_test_viewport(120, 40))
            .is_err());
        let lease = mux.frontend_lease.lock();
        assert_eq!(lease.handoff_owner, None);
        assert!(lease.handoff_ever_owned);
        assert_eq!(lease.tabs[&tab_id].owner, None);
    }

    #[test]
    fn failed_handoff_claim_revokes_the_global_owner() {
        let mux = Mux::new(None);
        mux.initialize_frontend_access_mode(FrontendAccessMode::Handoff);
        let tab_id = frontend_test_tab(&mux);
        let first = client_id(211);
        let second = client_id(212);
        {
            let mut lease = mux.frontend_lease.lock();
            lease.handoff_owner = Some(first.clone());
            lease.handoff_ever_owned = true;
            lease.tabs.entry(tab_id).or_default().owner = Some(first);
        }
        mux.frontend_geometry_failures.lock().push_back(true);

        assert!(mux
            .claim_frontend_viewport(&second, tab_id, frontend_test_viewport(120, 40))
            .is_err());
        let lease = mux.frontend_lease.lock();
        assert_eq!(lease.handoff_owner, None);
        assert_eq!(lease.tabs[&tab_id].owner, None);
    }

    #[test]
    fn failed_mode_geometry_restores_the_prior_mode_but_clears_ownership() {
        let mux = Mux::new(None);
        mux.initialize_frontend_access_mode(FrontendAccessMode::Handoff);
        let tab_id = frontend_test_tab(&mux);
        let owner = client_id(221);
        {
            let mut lease = mux.frontend_lease.lock();
            lease.handoff_owner = Some(owner.clone());
            lease.handoff_ever_owned = true;
            lease.tabs.entry(tab_id).or_default().owner = Some(owner.clone());
        }
        mux.frontend_geometry_failures.lock().push_back(true);

        assert!(mux
            .set_frontend_access_mode(
                &owner,
                FrontendAccessMode::TmuxLatest,
                tab_id,
                frontend_test_viewport(120, 40),
            )
            .is_err());
        let access = mux.frontend_access_state();
        assert_eq!(access.mode, FrontendAccessMode::Handoff);
        assert_eq!(access.owner, None);
    }

    fn palette(foreground: f32) -> ColorPalette {
        let mut palette = ColorPalette::default();
        palette.foreground = (foreground, foreground, foreground, 1.0).into();
        palette
    }

    #[test]
    fn palette_advice_requires_interaction_to_take_ownership() {
        let mut state = PaletteAdvisoryState::default();
        let white_client = client_id(1);
        let gray_client = client_id(2);
        let white_session = state.register(&white_client);
        let gray_session = state.register(&gray_client);
        let white = palette(1.0);
        let gray = palette(0.7);

        assert_eq!(state.advise(white_session, 9, white.clone()), None);
        assert_eq!(state.activate(white_session, 9), Some(white.clone()));

        // A background attach records its preference but cannot replace the
        // active client's OSC query base.
        assert_eq!(state.advise(gray_session, 9, gray.clone()), None);
        assert_eq!(state.owners.get(&9), Some(&white_session));

        assert_eq!(state.activate(gray_session, 9), Some(gray));
        assert_eq!(state.owners.get(&9), Some(&gray_session));

        // A config reload by the inactive client is stored without stealing.
        let warmer_white = palette(0.95);
        assert_eq!(state.advise(white_session, 9, warmer_white.clone()), None);
        assert_eq!(state.owners.get(&9), Some(&gray_session));

        // Removing the current owner falls back to the most recently active
        // surviving client and its latest advice.
        assert_eq!(
            state.remove_session(gray_session),
            vec![PaletteSelectionChange {
                pane_id: 9,
                palette: Some(warmer_white),
            }]
        );
        assert_eq!(state.owners.get(&9), Some(&white_session));
    }

    #[test]
    fn disconnect_does_not_promote_a_background_only_advisor() {
        let mut state = PaletteAdvisoryState::default();
        let active = client_id(1);
        let background = client_id(2);
        let active_session = state.register(&active);
        let background_session = state.register(&background);
        assert_eq!(state.advise(active_session, 7, palette(1.0)), None);
        assert!(state.activate(active_session, 7).is_some());
        assert_eq!(state.advise(background_session, 7, palette(0.7)), None);

        assert_eq!(
            state.remove_session(active_session),
            vec![PaletteSelectionChange {
                pane_id: 7,
                palette: None,
            }]
        );
        assert!(!state.owners.contains_key(&7));
    }

    #[test]
    fn active_palette_reload_updates_without_new_interaction() {
        let mut state = PaletteAdvisoryState::default();
        let client = client_id(1);
        let session = state.register(&client);
        assert_eq!(state.advise(session, 4, palette(0.8)), None);
        assert!(state.activate(session, 4).is_some());

        let updated = palette(1.0);
        assert_eq!(state.advise(session, 4, updated.clone()), Some(updated));
        assert_eq!(state.activate(session, 4), None);
    }

    #[test]
    fn stale_client_disconnect_cannot_remove_reconnected_frontend_lease() {
        let mux = Mux::new(None);
        let client = Arc::new(client_id(21));
        let old_registration = mux.register_client(Arc::clone(&client));
        let new_registration = mux.register_client(Arc::clone(&client));

        assert!(!mux.registered_client_had_input(&client, old_registration));
        assert!(mux.registered_client_had_input(&client, new_registration));
        mux.unregister_client(&client, old_registration);

        assert_eq!(mux.iter_clients().len(), 1);
        mux.unregister_client(&client, new_registration);
        assert!(mux.iter_clients().is_empty());
    }

    #[test]
    fn frontend_ownership_is_scoped_per_tab() {
        let mux = Mux::new(None);
        mux.initialize_frontend_access_mode(FrontendAccessMode::TmuxLatest);
        let first = Arc::new(client_id(31));
        let second = Arc::new(client_id(32));
        let mut lease = mux.frontend_lease.lock();
        lease.tabs.entry(10).or_default().owner = Some((*first).clone());
        lease.tabs.entry(11).or_default().owner = Some((*second).clone());
        drop(lease);

        assert!(mux.client_owns_frontend_lease(&first, 10));
        assert!(!mux.client_owns_frontend_lease(&first, 11));
        assert!(mux.client_owns_frontend_lease(&second, 11));
    }

    #[test]
    fn non_rendering_cli_input_does_not_steal_a_tab_viewport() {
        let mux = Mux::new(None);
        mux.initialize_frontend_access_mode(FrontendAccessMode::TmuxLatest);
        let tui = Arc::new(client_id(62));
        let cli = Arc::new(client_id(63));
        mux.register_client(Arc::clone(&tui));
        let cli_registration = mux.register_client(Arc::clone(&cli));
        mux.frontend_lease.lock().tabs.entry(30).or_default().owner = Some((*tui).clone());

        assert!(mux.registered_client_had_tab_input(&cli, cli_registration, 30));
        assert!(mux.client_owns_frontend_lease(&tui, 30));
    }

    /// A view outliving its author would pin every follower to a scrollback
    /// position that the renderer now driving never chose.
    #[test]
    fn only_the_owner_publishes_a_view_and_a_handover_drops_it() {
        fn grid() -> FrontendViewport {
            FrontendViewport::CellGrid {
                size: TerminalSize::default(),
            }
        }
        let mux = Mux::new(None);
        mux.initialize_frontend_access_mode(FrontendAccessMode::TmuxLatest);
        let tab = Arc::new(Tab::new(&TerminalSize::default()));
        mux.add_tab_no_panes(&tab);
        let tab_40 = tab.tab_id();
        let first = Arc::new(client_id(80));
        let second = Arc::new(client_id(81));
        {
            let mut lease = mux.frontend_lease.lock();
            let state = lease.tabs.entry(tab_40).or_default();
            state.owner = Some((*first).clone());
            state.viewports.insert((*first).clone(), grid());
            state.viewports.insert((*second).clone(), grid());
        }

        assert!(mux.set_client_view(
            &first,
            tab_40,
            FrontendView {
                scroll: vec![(7, 120)]
            }
        ));
        assert!(
            !mux.set_client_view(
                &first,
                tab_40,
                FrontendView {
                    scroll: vec![(7, 120)]
                }
            ),
            "saying the same thing twice is not a change worth publishing"
        );
        assert!(
            !mux.set_client_view(
                &second,
                tab_40,
                FrontendView {
                    scroll: vec![(7, 999)]
                }
            ),
            "a renderer nobody is using does not get to move everyone else"
        );
        assert_eq!(
            mux.frontend_lease.lock().tabs[&tab_40].view,
            Some(FrontendView {
                scroll: vec![(7, 120)]
            })
        );

        let _ = mux.claim_frontend_viewport(&second, tab_40, grid());
        assert_eq!(
            mux.frontend_lease.lock().tabs[&tab_40].view,
            None,
            "the new owner publishes its own on its next frame"
        );
    }

    #[test]
    fn different_sized_renderers_converge_on_one_owner_without_oscillation() {
        fn size(cols: usize, rows: usize) -> TerminalSize {
            TerminalSize {
                cols,
                rows,
                pixel_width: cols * 8,
                pixel_height: rows * 16,
                dpi: 96,
            }
        }

        let mux = Mux::new(None);
        mux.initialize_frontend_access_mode(FrontendAccessMode::TmuxLatest);
        let initial = size(132, 40);
        let tab = Arc::new(Tab::new(&initial));
        mux.add_tab_no_panes(&tab);
        let tab_id = tab.tab_id();

        let large = Arc::new(client_id(70));
        let small = Arc::new(client_id(71));
        let large_registration = mux.register_client(Arc::clone(&large));
        let small_registration = mux.register_client(Arc::clone(&small));

        mux.set_registered_client_viewport(
            &large,
            large_registration,
            tab_id,
            FrontendViewport::CellGrid { size: initial },
        )
        .unwrap()
        .unwrap();
        mux.set_registered_client_viewport(
            &small,
            small_registration,
            tab_id,
            FrontendViewport::CellGrid { size: size(80, 24) },
        )
        .unwrap()
        .unwrap();

        assert_eq!(tab.get_size(), initial);
        assert_eq!(
            mux.registered_client_may_resize_tab(&large, large_registration, tab_id),
            Some(true)
        );
        assert_eq!(
            mux.registered_client_may_resize_tab(&small, small_registration, tab_id),
            Some(false)
        );

        assert!(mux.registered_client_had_tab_input(&small, small_registration, tab_id));
        assert_eq!(tab.get_size(), size(80, 24));
        assert!(mux.client_owns_frontend_lease(&small, tab_id));
        assert_eq!(
            mux.registered_client_may_resize_tab(&large, large_registration, tab_id),
            Some(false)
        );
    }

    #[test]
    fn explicit_viewport_claim_uses_request_geometry_atomically() {
        let mux = Mux::new(None);
        let initial = TerminalSize {
            cols: 120,
            rows: 36,
            pixel_width: 960,
            pixel_height: 576,
            dpi: 96,
        };
        let tab = Arc::new(Tab::new(&initial));
        mux.add_tab_no_panes(&tab);
        let tab_id = tab.tab_id();
        let first = Arc::new(client_id(80));
        let second = Arc::new(client_id(81));
        let first_registration = mux.register_client(Arc::clone(&first));
        let second_registration = mux.register_client(Arc::clone(&second));

        mux.set_registered_client_viewport(
            &first,
            first_registration,
            tab_id,
            FrontendViewport::CellGrid { size: initial },
        )
        .unwrap();
        let advertised = TerminalSize {
            cols: 72,
            rows: 20,
            pixel_width: 576,
            pixel_height: 320,
            dpi: 96,
        };
        mux.set_registered_client_viewport(
            &second,
            second_registration,
            tab_id,
            FrontendViewport::CellGrid { size: advertised },
        )
        .unwrap();

        let claimed = TerminalSize {
            cols: 84,
            rows: 26,
            pixel_width: 672,
            pixel_height: 416,
            dpi: 96,
        };

        let state = mux
            .claim_registered_client_viewport(
                &second,
                second_registration,
                tab_id,
                FrontendViewport::CellGrid { size: claimed },
            )
            .unwrap()
            .unwrap();
        assert_eq!(state.owner.as_ref(), Some(second.as_ref()));
        assert_eq!(state.access.owner.as_ref(), Some(second.as_ref()));
        assert_eq!(state.canonical_size, claimed);
        assert_eq!(tab.get_size(), claimed);
        let repeated = mux
            .claim_registered_client_viewport(
                &second,
                second_registration,
                tab_id,
                FrontendViewport::CellGrid { size: claimed },
            )
            .unwrap()
            .unwrap();
        assert_eq!(repeated.generation, state.generation);
    }

    #[test]
    fn handoff_disconnect_has_no_automatic_fallback() {
        let mux = Mux::new(None);
        let tab = Arc::new(Tab::new(&TerminalSize::default()));
        mux.add_tab_no_panes(&tab);
        let tab_id = tab.tab_id();
        let first = Arc::new(client_id(90));
        let second = Arc::new(client_id(91));
        let first_registration = mux.register_client(Arc::clone(&first));
        let second_registration = mux.register_client(Arc::clone(&second));
        let grid = FrontendViewport::CellGrid {
            size: TerminalSize::default(),
        };

        mux.set_registered_client_viewport(&first, first_registration, tab_id, grid.clone())
            .unwrap();
        mux.set_registered_client_viewport(&second, second_registration, tab_id, grid.clone())
            .unwrap();
        assert_eq!(
            mux.frontend_access_state().owner.as_ref(),
            Some(first.as_ref())
        );
        assert_eq!(
            mux.registered_client_has_frontend_access(&first, first_registration),
            Some(true)
        );
        assert_eq!(
            mux.registered_client_has_frontend_access(&second, second_registration),
            Some(false)
        );
        assert!(mux.registered_client_had_tab_input(&first, first_registration, tab_id));
        assert!(
            !mux.registered_client_had_tab_input(&second, second_registration, tab_id),
            "B must reject keyboard/mouse input from the opaque renderer"
        );

        mux.unregister_client(&first, first_registration);
        assert_eq!(
            mux.registered_client_has_frontend_access(&first, first_registration),
            None
        );
        assert_eq!(mux.frontend_access_state().owner, None);
        assert!(!mux.client_has_frontend_access(&second));

        // A passive redraw after the disconnect still must not acquire B.
        mux.set_registered_client_viewport(&second, second_registration, tab_id, grid.clone())
            .unwrap();
        assert_eq!(mux.frontend_access_state().owner, None);

        mux.claim_registered_client_viewport(&second, second_registration, tab_id, grid)
            .unwrap();
        assert_eq!(
            mux.frontend_access_state().owner.as_ref(),
            Some(second.as_ref())
        );
    }

    /// B arbitrates between screens. A connection that never advertises a
    /// viewport -- `thinkterm cli send-text` and friends -- is not a screen,
    /// so its input and chrome mutations pass the gate; and passing must not
    /// itself claim anything from the real owner.
    #[test]
    fn handoff_exempts_clients_that_never_render() {
        let mux = Mux::new(None);
        let tab = Arc::new(Tab::new(&TerminalSize::default()));
        mux.add_tab_no_panes(&tab);
        let tab_id = tab.tab_id();
        let screen = Arc::new(client_id(70));
        let cli = Arc::new(client_id(71));
        let screen_registration = mux.register_client(Arc::clone(&screen));
        let cli_registration = mux.register_client(Arc::clone(&cli));
        let grid = FrontendViewport::CellGrid {
            size: TerminalSize::default(),
        };

        mux.set_registered_client_viewport(&screen, screen_registration, tab_id, grid.clone())
            .unwrap();
        assert_eq!(
            mux.registered_client_has_frontend_access(&screen, screen_registration),
            Some(true)
        );

        assert_eq!(
            mux.registered_client_has_frontend_access(&cli, cli_registration),
            Some(true)
        );
        assert!(mux.registered_client_had_tab_input(&cli, cli_registration, tab_id));
        assert_eq!(
            mux.frontend_access_state().owner.as_ref(),
            Some(screen.as_ref()),
            "the exemption must not steal the screen's lease"
        );

        // The moment the same connection renders, it is a second screen and
        // the gate applies to it like any other.
        mux.set_registered_client_viewport(&cli, cli_registration, tab_id, grid)
            .unwrap();
        assert_eq!(
            mux.registered_client_has_frontend_access(&cli, cli_registration),
            Some(false)
        );
        assert!(!mux.registered_client_had_tab_input(&cli, cli_registration, tab_id));
    }

    /// The headless shape of the same rule: after the owner disconnects
    /// (`handoff_ever_owned` set, no owner elected), surviving screens stay
    /// blocked until an explicit takeover -- but a non-rendering CLI still
    /// passes, because there is no screen it could be fighting.
    #[test]
    fn handoff_exempts_non_renderers_after_owner_disconnect() {
        let mux = Mux::new(None);
        let tab = Arc::new(Tab::new(&TerminalSize::default()));
        mux.add_tab_no_panes(&tab);
        let tab_id = tab.tab_id();
        let screen = Arc::new(client_id(72));
        let survivor = Arc::new(client_id(73));
        let cli = Arc::new(client_id(74));
        let screen_registration = mux.register_client(Arc::clone(&screen));
        let survivor_registration = mux.register_client(Arc::clone(&survivor));
        let cli_registration = mux.register_client(Arc::clone(&cli));
        let grid = FrontendViewport::CellGrid {
            size: TerminalSize::default(),
        };

        mux.set_registered_client_viewport(&screen, screen_registration, tab_id, grid.clone())
            .unwrap();
        mux.set_registered_client_viewport(&survivor, survivor_registration, tab_id, grid)
            .unwrap();
        mux.unregister_client(&screen, screen_registration);

        assert_eq!(
            mux.registered_client_has_frontend_access(&survivor, survivor_registration),
            Some(false),
            "a surviving screen still needs an explicit takeover"
        );
        assert_eq!(
            mux.registered_client_has_frontend_access(&cli, cli_registration),
            Some(true)
        );
        assert!(mux.registered_client_had_tab_input(&cli, cli_registration, tab_id));
    }

    #[test]
    fn mode_changes_require_the_correct_current_owner() {
        let mux = Mux::new(None);
        let tab = Arc::new(Tab::new(&TerminalSize::default()));
        mux.add_tab_no_panes(&tab);
        let tab_id = tab.tab_id();
        let first = Arc::new(client_id(92));
        let second = Arc::new(client_id(93));
        let first_registration = mux.register_client(Arc::clone(&first));
        let second_registration = mux.register_client(Arc::clone(&second));
        let grid = FrontendViewport::CellGrid {
            size: TerminalSize::default(),
        };
        mux.set_registered_client_viewport(&first, first_registration, tab_id, grid.clone())
            .unwrap();
        mux.set_registered_client_viewport(&second, second_registration, tab_id, grid.clone())
            .unwrap();

        assert!(mux
            .set_registered_frontend_access_mode(
                &second,
                second_registration,
                FrontendAccessMode::TmuxLatest,
                tab_id,
                grid.clone(),
            )
            .is_err());
        mux.set_registered_frontend_access_mode(
            &first,
            first_registration,
            FrontendAccessMode::TmuxLatest,
            tab_id,
            grid.clone(),
        )
        .unwrap();

        // In A the active tab's layout owner, not an unrelated renderer, is
        // the one allowed to make itself B's global owner.
        mux.claim_registered_client_viewport(&second, second_registration, tab_id, grid.clone())
            .unwrap();
        assert!(mux
            .set_registered_frontend_access_mode(
                &first,
                first_registration,
                FrontendAccessMode::Handoff,
                tab_id,
                grid.clone(),
            )
            .is_err());
        let access = mux
            .set_registered_frontend_access_mode(
                &second,
                second_registration,
                FrontendAccessMode::Handoff,
                tab_id,
                grid,
            )
            .unwrap();
        assert_eq!(access.mode, FrontendAccessMode::Handoff);
        assert_eq!(access.owner.as_ref(), Some(second.as_ref()));
    }

    #[test]
    fn unknown_cell_grid_pixels_stay_unknown_for_each_pane() {
        let target = pane_size_for_cell_grid(
            TerminalSize {
                cols: 120,
                rows: 40,
                pixel_width: 0,
                pixel_height: 0,
                dpi: 96,
            },
            60,
            20,
        );
        assert_eq!((target.pixel_width, target.pixel_height), (0, 0));

        let known = pane_size_for_cell_grid(
            TerminalSize {
                cols: 120,
                rows: 40,
                pixel_width: 960,
                pixel_height: 640,
                dpi: 96,
            },
            60,
            20,
        );
        assert_eq!((known.pixel_width, known.pixel_height), (480, 320));
    }

    #[test]
    fn late_palette_updates_from_a_disconnected_client_are_rejected() {
        let mut state = PaletteAdvisoryState::default();
        let client = client_id(1);
        let session = state.register(&client);

        assert_eq!(state.advise(session, 4, palette(0.8)), None);
        assert!(state.activate(session, 4).is_some());
        assert!(state.deactivate_session(session));

        // A SessionHandler can already have this work queued when Drop runs.
        // Once disconnect cleanup has happened, that stale work must not be
        // able to recreate advice or ownership for the dead connection.
        assert_eq!(state.advise(session, 4, palette(0.6)), None);
        assert_eq!(state.activate(session, 4), None);
        assert_eq!(state.advised.get(&(session, 4)), Some(&palette(0.8)));
        assert_eq!(state.owners.get(&4), Some(&session));

        state.remove_session(session);
        assert!(!state.owners.contains_key(&4));
    }

    #[test]
    fn old_disconnect_cannot_remove_or_overwrite_a_reconnected_session() {
        let mut state = PaletteAdvisoryState::default();
        let client = client_id(1);
        let old_session = state.register(&client);
        assert_eq!(state.advise(old_session, 4, palette(0.6)), None);
        assert!(state.activate(old_session, 4).is_some());

        // A reconnect legitimately reuses ClientId, but is a distinct
        // transport whose ownership must survive cleanup of the old one.
        let new_session = state.register(&client);
        let new_palette = palette(1.0);
        assert_eq!(state.advise(new_session, 4, new_palette.clone()), None);
        assert_eq!(state.activate(new_session, 4), Some(new_palette.clone()));

        assert!(state.deactivate_session(old_session));
        assert_eq!(state.advise(old_session, 4, palette(0.2)), None);
        assert_eq!(state.owners.get(&4), Some(&new_session));

        // Final cleanup happens later on the mux thread; it must observe that
        // the reconnect has already become owner and leave it untouched.
        assert_eq!(state.remove_session(old_session), vec![]);
        assert_eq!(state.owners.get(&4), Some(&new_session));
        assert_eq!(state.advised.get(&(new_session, 4)), Some(&new_palette));
    }

    #[test]
    fn fallback_never_promotes_an_already_disconnected_session() {
        let mut state = PaletteAdvisoryState::default();
        let first = state.register(&client_id(1));
        let owner = state.register(&client_id(2));

        assert_eq!(state.advise(first, 4, palette(0.6)), None);
        assert!(state.activate(first, 4).is_some());
        assert_eq!(state.advise(owner, 4, palette(1.0)), None);
        assert!(state.activate(owner, 4).is_some());

        // Both drops can deactivate synchronously before either queued final
        // cleanup runs. Removing the owner must not temporarily promote the
        // other disconnected session as the pane's OSC query base.
        assert!(state.deactivate_session(owner));
        assert!(state.deactivate_session(first));
        assert_eq!(
            state.remove_session(owner),
            vec![PaletteSelectionChange {
                pane_id: 4,
                palette: None,
            }]
        );
        assert!(!state.owners.contains_key(&4));
    }

    #[test]
    fn window_ui_surfaces_are_distinct_prune_anchors() {
        let mux = Mux::new(None);
        let window = Window::new(Some("test-workspace".to_string()), None, None);
        let window_id = window.window_id();
        mux.windows.write().insert(window_id, window);

        assert!(mux.is_empty());
        assert!(mux.register_window_ui_surface(window_id, "remote-thread:a".to_string()));
        assert!(mux.register_window_ui_surface(window_id, "ssh-hosts".to_string()));
        assert!(!mux.register_window_ui_surface(window_id, "ssh-hosts".to_string()));
        assert!(!mux.is_empty());

        mux.prune_dead_windows();
        assert!(mux.get_window(window_id).is_some());

        assert!(mux.unregister_window_ui_surface(window_id, "remote-thread:a"));
        assert!(mux.get_window(window_id).is_some());

        assert!(mux.unregister_window_ui_surface(window_id, "ssh-hosts"));
        assert!(mux.get_window(window_id).is_none());
        assert!(mux.is_empty());
    }

    #[test]
    fn existing_window_spawn_context_allows_empty_window() {
        let mux = Mux::new(None);
        let window = Window::new(Some("test-workspace".to_string()), None, None);
        let window_id = window.window_id();
        mux.windows.write().insert(window_id, window);

        let requested_size = TerminalSize {
            rows: 33,
            cols: 111,
            pixel_width: 999,
            pixel_height: 777,
            dpi: 144,
        };
        let context = mux
            .existing_window_spawn_context(window_id, requested_size)
            .unwrap();

        assert_eq!(context.size, requested_size);
        assert!(context.term_config.is_none());
    }
}
