use crate::client::{Client, ClientConnectionPhase};
use crate::pane::{remote_server_identity_matches, ClientPane};
use anyhow::{anyhow, bail, Context};
use async_trait::async_trait;
use codec::{ListPanesResponse, SpawnV2, SplitPane};
use config::keyassignment::SpawnTabDomain;
use config::{SshDomain, TlsDomainClient, UnixDomain};
use futures::channel::oneshot;
use mux::command_spec::{CommandSpec, CommandSpecExt};
use mux::connui::{ConnectionUI, ConnectionUIParams};
use mux::domain::{alloc_domain_id, Domain, DomainId, DomainState, SplitSource};
use mux::pane::{Pane, PaneId};
use mux::tab::{SplitRequest, Tab, TabId};
use mux::window::{Window, WindowId};
use mux::{Mux, MuxNotification};
use portable_pty::CommandBuilder;
use promise::spawn::spawn_into_new_thread;
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use wezterm_term::TerminalSize;

const MIN_PUSH_RESYNC_GAP: Duration = Duration::from_millis(500);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ResyncOutcome {
    Applied,
    /// A structure mutation owns the authoritative mapping update and will
    /// enqueue its own catch-up resync when its guard is dropped.
    Deferred,
    NoClient,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SharedResyncCompletion {
    generation: u64,
    outcome: ResyncOutcome,
}

type SharedResyncResult = Result<SharedResyncCompletion, Arc<str>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ResyncRequestKind {
    Immediate,
    FreshAfter(u64),
    MutationCatchup,
    Background,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct QueuedResync {
    generation: u64,
    not_before: Instant,
    run_after_deferred: bool,
}

struct ResyncWaiter {
    generation: u64,
    sender: oneshot::Sender<SharedResyncResult>,
}

struct ResyncTicket {
    joined_existing: bool,
    receiver: oneshot::Receiver<SharedResyncResult>,
}

enum ResyncDriverAction {
    Stop,
    Wait {
        delay: Duration,
        wake: oneshot::Receiver<()>,
    },
    Run(u64),
}

struct ResyncCoordinatorState {
    next_generation: u64,
    active_generation: Option<u64>,
    queued: Option<QueuedResync>,
    last_started_at: Option<Instant>,
    driver_running: bool,
    timer_wake: Option<oneshot::Sender<()>>,
    waiters: Vec<ResyncWaiter>,
}

impl Default for ResyncCoordinatorState {
    fn default() -> Self {
        Self {
            next_generation: 1,
            active_generation: None,
            queued: None,
            last_started_at: None,
            driver_running: false,
            timer_wake: None,
            waiters: Vec::new(),
        }
    }
}

impl ResyncCoordinatorState {
    fn allocate(&mut self, not_before: Instant, run_after_deferred: bool) -> u64 {
        let generation = self.next_generation;
        self.next_generation = self.next_generation.wrapping_add(1).max(1);
        self.queued = Some(QueuedResync {
            generation,
            not_before,
            run_after_deferred,
        });
        generation
    }

    fn request(
        &mut self,
        kind: ResyncRequestKind,
        now: Instant,
    ) -> (ResyncTicket, bool, Option<oneshot::Sender<()>>) {
        let mut joined_existing = false;
        let mut wake = None;
        let generation = match kind {
            ResyncRequestKind::Immediate => {
                if let Some(generation) = self.active_generation {
                    joined_existing = true;
                    generation
                } else if let Some(queued) = self.queued.as_mut() {
                    if queued.not_before > now {
                        queued.not_before = now;
                        wake = self.timer_wake.take();
                    }
                    queued.generation
                } else {
                    self.allocate(now, false)
                }
            }
            ResyncRequestKind::FreshAfter(after) => {
                if let Some(generation) = self.active_generation.filter(|g| *g > after) {
                    generation
                } else if let Some(queued) = self.queued.as_mut().filter(|q| q.generation > after) {
                    if queued.not_before > now {
                        queued.not_before = now;
                        wake = self.timer_wake.take();
                    }
                    queued.generation
                } else {
                    self.allocate(now, false)
                }
            }
            ResyncRequestKind::MutationCatchup => {
                if let Some(queued) = self.queued.as_mut() {
                    queued.not_before = now;
                    queued.run_after_deferred = true;
                    wake = self.timer_wake.take();
                    queued.generation
                } else {
                    self.allocate(now, true)
                }
            }
            ResyncRequestKind::Background => {
                if let Some(queued) = self.queued {
                    queued.generation
                } else {
                    let not_before = self
                        .last_started_at
                        .map(|started| started + MIN_PUSH_RESYNC_GAP)
                        .unwrap_or(now)
                        .max(now);
                    self.allocate(not_before, false)
                }
            }
        };

        let (sender, receiver) = oneshot::channel();
        self.waiters.push(ResyncWaiter { generation, sender });
        let start_driver = !self.driver_running;
        self.driver_running = true;
        (
            ResyncTicket {
                joined_existing,
                receiver,
            },
            start_driver,
            wake,
        )
    }

    fn next_action(&mut self, now: Instant) -> ResyncDriverAction {
        debug_assert!(self.active_generation.is_none());
        let Some(queued) = self.queued else {
            self.driver_running = false;
            self.timer_wake = None;
            return ResyncDriverAction::Stop;
        };
        if queued.not_before > now {
            let (wake, receiver) = oneshot::channel();
            self.timer_wake = Some(wake);
            return ResyncDriverAction::Wait {
                delay: queued.not_before.saturating_duration_since(now),
                wake: receiver,
            };
        }

        self.queued = None;
        self.timer_wake = None;
        self.active_generation = Some(queued.generation);
        self.last_started_at = Some(now);
        ResyncDriverAction::Run(queued.generation)
    }

    fn finish(&mut self, generation: u64, result: SharedResyncResult, now: Instant) {
        debug_assert_eq!(self.active_generation, Some(generation));
        self.active_generation = None;

        let deferred = matches!(
            result,
            Ok(SharedResyncCompletion {
                outcome: ResyncOutcome::Deferred,
                ..
            })
        );
        let no_client = matches!(
            result,
            Ok(SharedResyncCompletion {
                outcome: ResyncOutcome::NoClient,
                ..
            })
        );
        let keep_mutation_catchup =
            deferred && self.queued.is_some_and(|queued| queued.run_after_deferred);
        let terminal = no_client || (deferred && !keep_mutation_catchup);
        let failed = result.is_err();
        let mut remaining = Vec::new();
        for waiter in self.waiters.drain(..) {
            if terminal || waiter.generation <= generation {
                let _ = waiter.sender.send(result.clone());
            } else {
                remaining.push(waiter);
            }
        }
        self.waiters = remaining;

        if terminal {
            self.queued = None;
        } else if keep_mutation_catchup {
            if let Some(queued) = self.queued.as_mut() {
                queued.not_before = now;
            }
        } else if failed {
            self.last_started_at = None;
            if let Some(queued) = self.queued.as_mut() {
                queued.not_before = now;
            }
        }
    }

    fn abort(&mut self, reason: Arc<str>) {
        self.active_generation = None;
        self.queued = None;
        self.last_started_at = None;
        self.driver_running = false;
        if let Some(wake) = self.timer_wake.take() {
            let _ = wake.send(());
        }
        for waiter in self.waiters.drain(..) {
            let _ = waiter.sender.send(Err(Arc::clone(&reason)));
        }
    }
}

struct ResyncDriverGuard {
    domain_id: DomainId,
    armed: bool,
}

impl Drop for ResyncDriverGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let Some(mux) = Mux::try_get() else { return };
        let Some(domain) = mux.get_domain(self.domain_id) else {
            return;
        };
        let Some(domain) = domain.downcast_ref::<ClientDomain>() else {
            return;
        };
        domain.resync_coordinator.lock().unwrap().abort(Arc::from(
            "resync coordinator driver stopped before completing",
        ));
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttachRetryOutcome {
    Attached,
    /// `keep_going` said stop before the domain attached. The ConnectionUI
    /// has been closed and the domain remains detached.
    Cancelled,
}

#[derive(Clone, Copy, Debug)]
struct AttachRetryTiming {
    initial_backoff: Duration,
    max_backoff: Duration,
    cancellation_poll: Duration,
}

impl Default for AttachRetryTiming {
    fn default() -> Self {
        Self {
            initial_backoff: Duration::from_secs(1),
            max_backoff: Duration::from_secs(10),
            cancellation_poll: Duration::from_millis(100),
        }
    }
}

/// Errors that retrying cannot fix. Walk the complete error chain so adding
/// context at any layer cannot accidentally turn a fatal error transient.
pub fn is_fatal_attach_error(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause
            .downcast_ref::<mux::ssh::AuthCancelledError>()
            .is_some()
            || cause
                .downcast_ref::<crate::client::IncompatibleVersionError>()
                .is_some()
            || cause
                .downcast_ref::<wezterm_ssh::HostVerificationFailed>()
                .is_some()
    })
}

async fn wait_attach_backoff(
    keep_going: &mut impl FnMut() -> bool,
    duration: Duration,
    cancellation_poll: Duration,
) -> bool {
    let deadline = Instant::now() + duration;
    loop {
        if !keep_going() {
            return false;
        }
        let now = Instant::now();
        if now >= deadline {
            return true;
        }
        smol::Timer::after(
            deadline
                .saturating_duration_since(now)
                .min(cancellation_poll),
        )
        .await;
    }
}

fn next_attach_backoff(current: Duration, maximum: Duration) -> Duration {
    (current * 2).min(maximum)
}

async fn attach_with_retry_loop<Attempt, AttemptFuture, KeepGoing>(
    domain_name: &str,
    ui: ConnectionUI,
    mut attempt: Attempt,
    mut keep_going: KeepGoing,
    max_total: Option<Duration>,
    timing: AttachRetryTiming,
) -> anyhow::Result<AttachRetryOutcome>
where
    Attempt: FnMut() -> AttemptFuture,
    AttemptFuture: Future<Output = anyhow::Result<()>>,
    KeepGoing: FnMut() -> bool,
{
    let start = Instant::now();
    let mut backoff = timing.initial_backoff;
    loop {
        if !keep_going() {
            ui.close();
            return Ok(AttachRetryOutcome::Cancelled);
        }
        match attempt().await {
            Ok(()) => {
                // ClientDomain::attach_with_ui already closes on success, but
                // owning that invariant here keeps the retry runner correct
                // for tests and any future single-attempt implementation.
                ui.close();
                return Ok(AttachRetryOutcome::Attached);
            }
            Err(err) if is_fatal_attach_error(&err) => return Err(err),
            Err(err) => {
                if max_total.is_some_and(|limit| start.elapsed() + backoff >= limit) {
                    return Err(err);
                }
                log::error!("attaching {domain_name} failed: {err:#}; retrying in {backoff:?}");
                ui.output_str(&format!("Will retry in {backoff:?}...\n"));
                if !wait_attach_backoff(&mut keep_going, backoff, timing.cancellation_poll).await {
                    ui.close();
                    return Ok(AttachRetryOutcome::Cancelled);
                }
                backoff = next_attach_backoff(backoff, timing.max_backoff);
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum FrontendRecoverySlot {
    /// A thin frontend with a single selected terminal, currently the TUI.
    Primary,
    /// A stable native GUI window identity.  This is deliberately not a mux
    /// WindowId: the mux id changes when the native window switches Threads.
    Window(u64),
}

#[derive(Clone, Debug)]
struct ReconnectRecoveryTarget {
    slot: FrontendRecoverySlot,
    workspace: String,
    size: TerminalSize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ThinkTermFrontendRecoveryTarget {
    pub slot: FrontendRecoverySlot,
    pub window_id: WindowId,
    pub tab_id: TabId,
}

/// How long a recovery barrier may wait for the frontend before it is
/// released unacknowledged.
const FRONTEND_RECOVERY_DEADLINE: Duration = Duration::from_secs(20);

#[derive(Debug)]
struct FrontendRecoveryBarrier {
    generation: u64,
    pending: HashMap<FrontendRecoverySlot, TabId>,
    /// When the barrier was armed, so the recovery log can say how long
    /// the frontend took to publish its geometry.
    started_at: Instant,
    /// The reattach's hold on push-driven resyncs, kept until the frontend
    /// has published the recovered geometry: a resync in between resized
    /// the very tabs whose geometry the frontend was confirming, and the
    /// confirmation was lost to the newer epoch. Its drop runs whatever
    /// resync was deferred meanwhile.
    _structure: Option<StructureMutationGuard>,
}

impl FrontendRecoveryBarrier {
    fn new(
        generation: u64,
        pending: HashMap<FrontendRecoverySlot, TabId>,
        structure: Option<StructureMutationGuard>,
    ) -> Self {
        Self {
            generation,
            pending,
            started_at: Instant::now(),
            _structure: structure,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FrontendRecoveryAck {
    Ignored,
    Pending,
    Complete,
}

fn acknowledge_recovery_target(
    barrier: &mut Option<FrontendRecoveryBarrier>,
    slot: FrontendRecoverySlot,
    local_tab_id: TabId,
    generation: u64,
    current_generation: u64,
) -> FrontendRecoveryAck {
    let Some(active) = barrier.as_mut() else {
        return FrontendRecoveryAck::Ignored;
    };
    if active.generation != generation
        || generation != current_generation
        || active.pending.get(&slot) != Some(&local_tab_id)
    {
        return FrontendRecoveryAck::Ignored;
    }
    active.pending.remove(&slot);
    if active.pending.is_empty() {
        *barrier = None;
        FrontendRecoveryAck::Complete
    } else {
        FrontendRecoveryAck::Pending
    }
}

#[derive(Default)]
struct ServerReplacementMirrors {
    windows_by_workspace: HashMap<String, WindowId>,
    old_tabs: Vec<TabId>,
    /// Local windows this attempt made for remote windows that had no
    /// retained window to rebind into. An attempt that fails after this
    /// point takes them back; the next attempt makes its own.
    created_windows: Vec<WindowId>,
}

/// Take back the local windows one replacement attempt made, when the
/// attempt fails after making them. Nobody else would: they hold tabs, so
/// `prune_dead_windows` keeps them, and a reconnect that took three
/// attempts ended with three copies of every window the retained ones did
/// not cover. The panes in them mirror live panes on the current server,
/// so the local windows go and the remote panes stay.
fn discard_replacement_windows(inner: &ClientInner, windows: &[WindowId]) {
    let mux = Mux::get();
    for window_id in windows {
        let tabs = match mux.get_window(*window_id) {
            Some(window) => window.iter().cloned().collect::<Vec<_>>(),
            None => continue,
        };
        for tab in tabs {
            for pane in tab.iter_all_panes() {
                if pane.domain_id() != inner.local_domain_id {
                    continue;
                }
                if let Some(client_pane) = pane.downcast_ref::<ClientPane>() {
                    client_pane.ignore_next_kill();
                }
            }
        }
        log::info!("discarding local window {window_id} made by a failed reattach attempt");
        mux.kill_window(*window_id);
    }
}

fn pane_node_workspace(node: &mux::tab::PaneNode) -> Option<&str> {
    match node {
        mux::tab::PaneNode::Empty => None,
        mux::tab::PaneNode::Leaf(entry) => Some(entry.workspace.as_str()),
        mux::tab::PaneNode::Stack(stack) => stack
            .panes
            .get(stack.active)
            .or_else(|| stack.panes.first())
            .map(|entry| entry.workspace.as_str()),
        mux::tab::PaneNode::Split { left, right, .. } => {
            pane_node_workspace(left).or_else(|| pane_node_workspace(right))
        }
    }
}

fn thread_id_for_workspace(tree: &codec::ThinkTermTree, workspace: &str) -> Option<String> {
    tree.projects.iter().find_map(|project| {
        project.threads.iter().find_map(|thread| {
            let thread_workspace = thread
                .materialized_workspace_name
                .as_deref()
                .or(thread.planned_workspace_name.as_deref());
            (thread_workspace == Some(workspace)).then(|| thread.id.clone())
        })
    })
}

fn server_runtime_replaced(ready_server_id: Option<&str>, observed_server_id: &str) -> bool {
    ready_server_id.is_some_and(|ready| ready != observed_server_id)
}

fn active_remote_tabs_by_workspace(state: &codec::ThinkTermSessionState) -> HashMap<String, TabId> {
    let mut active = HashMap::new();
    for project in &state.projects {
        for thread in &project.threads {
            let Some(workspace) = thread
                .materialized_workspace_name
                .as_deref()
                .or(thread.planned_workspace_name.as_deref())
            else {
                continue;
            };
            if let Some(tab) = thread
                .tabs
                .iter()
                .find(|tab| tab.is_active)
                .or_else(|| thread.tabs.first())
            {
                active.insert(workspace.to_string(), tab.tab_id);
            }
        }
    }
    active
}

fn accepts_generation(prior: Option<u64>, incoming: u64) -> bool {
    prior.is_none_or(|prior| incoming > prior)
}

fn consistent_remote_tab_id(ids: impl IntoIterator<Item = TabId>) -> Option<TabId> {
    let mut ids = ids.into_iter();
    let first = ids.next()?;
    ids.all(|id| id == first).then_some(first)
}

/// Where a locally mirrored pane came from, as far as the stale-mirror
/// reap in `process_pane_list` is concerned.
#[derive(Clone, Debug, PartialEq, Eq)]
enum MirrorOrigin {
    /// A `ClientPane` of the domain being swept, tagged with the mux
    /// runtime that allocated its remote pane id (`None` when the
    /// connection had not learned a server identity at construction).
    Mirror(Option<String>),
    /// A pane the reap must never touch: another domain's pane, or a
    /// local pane sharing a tab with mirrors.
    Foreign,
}

/// Mirrors whose allocating mux runtime is gone. Their remote pane ids
/// name nothing on the connected server -- worse, a replacement server
/// allocates ids from scratch, so a stale id can collide with someone
/// else's live pane.
///
/// Only a *committed* runtime may condemn anything, which is why both ids
/// are required and must agree. `Client::remote_server_id` flips as soon
/// as version bootstrap succeeds, long before the replacement topology
/// exists; a server push during the replacement's own Ensure calls spawns
/// a resync that reaches here past a connection-generation guard that
/// already matches. Judging against the connected id there would condemn
/// every mirror of the session that is still being rebuilt, and judging
/// against the committed id would condemn the replacement's fresh panes
/// instead. While the two disagree both runtimes' mirrors coexist by
/// design, so the sweep stands down and the replacement machinery owns
/// the teardown.
///
/// Absence of evidence never condemns either: an unidentified runtime on
/// either side keeps the pane, and a response that described no mirrors
/// at all reaps nothing, so a half-started server cannot condemn the
/// whole session.
fn stale_mirrors_to_reap(
    panes: impl IntoIterator<Item = (PaneId, MirrorOrigin)>,
    committed_server_id: Option<&str>,
    connected_server_id: Option<&str>,
    live_mirrors_in_response: usize,
) -> Vec<PaneId> {
    if live_mirrors_in_response == 0
        || committed_server_id.is_none()
        || !remote_server_identity_matches(committed_server_id, connected_server_id)
    {
        return vec![];
    }
    panes
        .into_iter()
        .filter_map(|(pane_id, origin)| match origin {
            MirrorOrigin::Mirror(Some(created))
                if !remote_server_identity_matches(Some(&created), committed_server_id) =>
            {
                Some(pane_id)
            }
            _ => None,
        })
        .collect()
}

fn remote_owner_matches_client(
    owner: Option<&mux::client::ClientId>,
    client_id: &mux::client::ClientId,
) -> bool {
    owner.is_some_and(|owner| owner.same_logical_client(client_id))
}

fn owns_remote_viewport_from_states(
    client_id: &mux::client::ClientId,
    access: Option<&codec::FrontendAccessState>,
    viewport: Option<&codec::ClientViewportState>,
) -> Option<bool> {
    let access = access?;
    match access.mode {
        // Handoff is connection-wide.  A claim made while another tab is
        // active does not publish a fresh viewport snapshot for every tab, so
        // consulting `viewport.access` here can leave those tabs believing an
        // old owner indefinitely.
        codec::FrontendAccessMode::Handoff => Some(remote_owner_matches_client(
            access.owner.as_ref(),
            client_id,
        )),
        // Collaborative mode deliberately keeps one layout owner per tab.
        codec::FrontendAccessMode::TmuxLatest => {
            viewport.map(|state| remote_owner_matches_client(state.owner.as_ref(), client_id))
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteFrontendGate {
    Visible,
    Claimable {
        owner: Option<mux::client::ClientId>,
    },
    Connecting,
    Reconnecting,
    Offline,
    Syncing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutomaticRemotePaneResize {
    Live,
    OnRelease,
}

const VIEWPORT_RTT_LIVE_MS: f64 = 80.0;
const VIEWPORT_RTT_ON_RELEASE_MS: f64 = 140.0;
const VIEWPORT_RTT_MIN_SAMPLES: u32 = 3;
const VIEWPORT_RTT_EWMA_ALPHA: f64 = 0.25;
const VIEWPORT_FAILURE_COOLDOWN: Duration = Duration::from_secs(30);

#[derive(Debug)]
struct ViewportLatencyState {
    ewma_ms: Option<f64>,
    successful_samples: u32,
    selected: AutomaticRemotePaneResize,
    degraded_until: Option<Instant>,
}

impl Default for ViewportLatencyState {
    fn default() -> Self {
        Self {
            ewma_ms: None,
            successful_samples: 0,
            selected: AutomaticRemotePaneResize::Live,
            degraded_until: None,
        }
    }
}

impl ViewportLatencyState {
    fn record_success(&mut self, elapsed: Duration) {
        let elapsed_ms = elapsed.as_secs_f64() * 1_000.0;
        self.ewma_ms = Some(match self.ewma_ms {
            Some(previous) => {
                previous * (1.0 - VIEWPORT_RTT_EWMA_ALPHA) + elapsed_ms * VIEWPORT_RTT_EWMA_ALPHA
            }
            None => elapsed_ms,
        });
        self.successful_samples = self.successful_samples.saturating_add(1);
    }

    fn record_failure(&mut self, now: Instant) {
        self.selected = AutomaticRemotePaneResize::OnRelease;
        self.degraded_until = now.checked_add(VIEWPORT_FAILURE_COOLDOWN);
    }

    fn choose(&mut self, now: Instant, ready: bool, tardy: bool) -> AutomaticRemotePaneResize {
        if !ready || tardy {
            return AutomaticRemotePaneResize::OnRelease;
        }
        if self.degraded_until.is_some_and(|until| now < until) {
            return AutomaticRemotePaneResize::OnRelease;
        }
        self.degraded_until = None;
        if self.successful_samples < VIEWPORT_RTT_MIN_SAMPLES {
            return AutomaticRemotePaneResize::Live;
        }
        match self.ewma_ms {
            Some(ewma) if ewma <= VIEWPORT_RTT_LIVE_MS => {
                self.selected = AutomaticRemotePaneResize::Live;
            }
            Some(ewma) if ewma >= VIEWPORT_RTT_ON_RELEASE_MS => {
                self.selected = AutomaticRemotePaneResize::OnRelease;
            }
            _ => {}
        }
        self.selected
    }
}

impl RemoteFrontendGate {
    pub fn obscures_terminal(&self) -> bool {
        !matches!(self, Self::Visible)
    }

    pub fn is_claimable(&self) -> bool {
        matches!(self, Self::Claimable { .. })
    }

    pub fn overlay_message(&self) -> Option<(String, String)> {
        match self {
            Self::Visible => None,
            Self::Claimable { owner: Some(owner) } => {
                let hostname = owner.hostname.trim();
                let title = if hostname.is_empty() {
                    "Terminal is being used on another device".to_string()
                } else {
                    format!("Terminal is being used on {hostname}")
                };
                Some((title, "Click or scroll to continue".to_string()))
            }
            Self::Claimable { owner: None } => Some((
                "Terminal is available".to_string(),
                "Click or scroll to take control".to_string(),
            )),
            Self::Connecting => Some((
                "Connecting to terminal…".to_string(),
                "Please wait".to_string(),
            )),
            Self::Reconnecting => Some((
                "Connection lost — Reconnecting…".to_string(),
                "Terminal will resume automatically".to_string(),
            )),
            Self::Offline => Some((
                "Connection offline".to_string(),
                "Reconnect from the sidebar".to_string(),
            )),
            Self::Syncing => Some((
                "Restoring terminal state…".to_string(),
                "Please wait".to_string(),
            )),
        }
    }
}

fn remote_frontend_gate_from_state(
    phase: ClientConnectionPhase,
    access: Option<&codec::FrontendAccessState>,
    client_id: &mux::client::ClientId,
) -> RemoteFrontendGate {
    match phase {
        ClientConnectionPhase::Connecting => RemoteFrontendGate::Connecting,
        ClientConnectionPhase::Registering | ClientConnectionPhase::Reconnecting => {
            RemoteFrontendGate::Reconnecting
        }
        ClientConnectionPhase::Syncing => RemoteFrontendGate::Syncing,
        ClientConnectionPhase::Suspended | ClientConnectionPhase::Detached => {
            RemoteFrontendGate::Offline
        }
        ClientConnectionPhase::Ready => match access {
            None => RemoteFrontendGate::Syncing,
            Some(access) if access.mode == codec::FrontendAccessMode::TmuxLatest => {
                RemoteFrontendGate::Visible
            }
            Some(access) if remote_owner_matches_client(access.owner.as_ref(), client_id) => {
                RemoteFrontendGate::Visible
            }
            Some(access) => RemoteFrontendGate::Claimable {
                owner: access.owner.clone(),
            },
        },
    }
}

pub struct ClientInner {
    pub client: Client,
    pub local_domain_id: DomainId,
    pub local_echo_threshold_ms: Option<u64>,
    pub overlay_lag_indicator: bool,
    remote_to_local_window: Mutex<HashMap<WindowId, WindowId>>,
    remote_to_local_tab: Mutex<HashMap<TabId, TabId>>,
    remote_to_local_pane: Mutex<HashMap<PaneId, PaneId>>,
    /// The latest agent status the server pushed or served for each
    /// *remote* pane id, retained whether or not a local mirror exists
    /// yet. A mirror that materializes later seeds itself from here
    /// (`ClientPane::new`), which closes the attach-time hole where the
    /// cold-start fetch ran before the panes existed and the push path
    /// had nothing to deliver to — each side assumed the other covered it.
    remote_agent_statuses: Mutex<HashMap<PaneId, thinkterm_proto::AgentStatus>>,
    /// The same, for the program leading each remote pane's terminal.
    remote_foreground_programs: Mutex<HashMap<PaneId, thinkterm_proto::ForegroundProgram>>,
    /// Remote panes whose KillPane is on its way. A resync that lands in
    /// between must not mirror them back: the local mirror is gone already,
    /// and a fresh one would get a window of its own.
    pending_kills: Mutex<HashSet<PaneId>>,
    /// Authoritative per-remote-tab viewport ownership pushed by the server.
    remote_viewports: Mutex<HashMap<TabId, codec::ClientViewportState>>,
    /// Connection-wide A/B mode and exclusive handoff owner.
    remote_access: Mutex<Option<codec::FrontendAccessState>>,
    /// Latest geometry this renderer actually reported for each remote tab.
    /// Explicit claims copy this geometry into the same PDU as the owner move.
    reported_viewports: Mutex<HashMap<TabId, codec::ClientViewport>>,
    /// Frontend-owned recovery intents. GUI windows use independent stable
    /// slots; the TUI overwrites its single Primary slot as selection changes.
    /// This keeps every visible GUI Thread while avoiding eager restoration of
    /// every Thread a TUI visited earlier in its lifetime.
    frontend_recovery_intents: Mutex<HashMap<FrontendRecoverySlot, ReconnectRecoveryTarget>>,
    /// Claims on a tab are coalesced behind one request-scoped async lock. A
    /// second input rechecks ownership after the first request completes.
    frontend_claim_locks: Mutex<HashMap<TabId, Arc<futures::lock::Mutex<()>>>>,
    /// Remote pane-stack id -> stable local pane-stack id. Remote and local
    /// stack ids live in different id spaces; translating (rather than
    /// adopting) avoids collisions with locally-created stacks while keeping
    /// each remote stack's local id stable across resyncs.
    remote_to_local_stack: Mutex<HashMap<usize, usize>>,
    pub focused_remote_pane_id: Mutex<Option<PaneId>>,
    /// When we last advised the server of a focus change. Used to discard
    /// stale PaneFocused echoes that would otherwise yank the active tab
    /// and stack away from a newer local selection.
    pub focus_advised_at: Mutex<Option<std::time::Instant>>,
    /// Number of structure-mutating RPCs (spawn/split/stack-tab/move) in
    /// flight. Their responses install the remote<->local mappings; a resync
    /// racing them (the server broadcasts TabAddedToWindow/TabResized while
    /// handling the request) sees the new remote window/tab/pane as unmapped
    /// and materializes a duplicate local mirror of the same remote pane.
    mutations_in_flight: std::sync::atomic::AtomicUsize,
    /// A resync arrived while a mutation was in flight; run one when the
    /// last mutation completes.
    resync_deferred: std::sync::atomic::AtomicBool,
    /// A replacement mux runtime has installed new topology but one or more
    /// frontend slots have not yet confirmed their final local geometry.
    /// Ordinary viewport RPCs never modify this barrier.
    frontend_recovery_barrier: Mutex<Option<FrontendRecoveryBarrier>>,
    /// The mux runtime whose topology and frontend geometry were last fully
    /// committed. `Client::remote_server_id` changes as soon as version
    /// bootstrap succeeds, which is too early: a later Ensure/ListPanes/
    /// viewport failure must continue to be treated as a replacement runtime
    /// on the next retry.
    ready_server_id: Mutex<Option<String>>,
    /// Recovery intent survives a failed replacement attempt even after the
    /// old remote-id maps have been cleared. It is released only when the
    /// frontend acknowledges geometry for the replacement topology.
    pending_recovery_targets: Mutex<HashMap<FrontendRecoverySlot, ReconnectRecoveryTarget>>,
    /// Stable frontend windows retained across replacement retries. Once the
    /// old remote-id maps are cleared, this is what prevents a failed retry
    /// from materializing duplicate native windows on its next attempt.
    pending_recovery_windows: Mutex<HashMap<String, WindowId>>,
    /// Measured end-to-end latency of complete viewport RPCs. GUI Auto mode
    /// consults this once at drag start; it never changes policy mid-drag.
    viewport_latency: Mutex<ViewportLatencyState>,
}

/// RAII scope for a structure-mutating RPC; defers resyncs for its lifetime
/// and schedules the catch-up resync when the last in-flight mutation ends.
#[derive(Debug)]
pub(crate) struct StructureMutationGuard {
    /// Weak, since a guard can live inside the inner it counts for (the
    /// frontend recovery barrier holds one).
    inner: std::sync::Weak<ClientInner>,
}

impl Drop for StructureMutationGuard {
    fn drop(&mut self) {
        use std::sync::atomic::Ordering;
        let Some(inner) = self.inner.upgrade() else {
            return;
        };
        if inner.mutations_in_flight.fetch_sub(1, Ordering::SeqCst) == 1
            && inner.resync_deferred.swap(false, Ordering::SeqCst)
        {
            let domain_id = inner.local_domain_id;
            promise::spawn::spawn_into_main_thread(async move {
                let Some(mux) = Mux::try_get() else {
                    return;
                };
                let Some(domain) = mux.get_domain(domain_id) else {
                    return;
                };
                let Some(domain) = domain.downcast_ref::<ClientDomain>() else {
                    return;
                };
                if let Err(err) = domain.resync_after_mutation().await {
                    log::error!("deferred resync for domain {domain_id}: {err:#}");
                }
            })
            .detach();
        }
    }
}

fn remote_move_pane_id(
    target_local_pane_id: PaneId,
    target_remote_pane_id: PaneId,
    source_local_pane_id: PaneId,
    source_remote_pane_id: PaneId,
    expected_domain_id: DomainId,
    source_domain_id: DomainId,
) -> anyhow::Result<PaneId> {
    if source_local_pane_id == target_local_pane_id {
        bail!("cannot split pane {source_local_pane_id} relative to itself");
    }
    if source_domain_id != expected_domain_id {
        bail!(
            "cannot move pane {source_local_pane_id} from domain {source_domain_id} \
             into domain {expected_domain_id}"
        );
    }
    if source_remote_pane_id == target_remote_pane_id {
        bail!(
            "local panes {source_local_pane_id} and {target_local_pane_id} \
             both refer to remote pane {source_remote_pane_id}"
        );
    }
    Ok(source_remote_pane_id)
}

impl ClientInner {
    fn remote_owner_is_self(&self, owner: Option<&mux::client::ClientId>) -> bool {
        remote_owner_matches_client(owner, &self.client.client_id)
    }

    pub fn remote_frontend_gate(&self) -> RemoteFrontendGate {
        let access = self.remote_access_state();
        remote_frontend_gate_from_state(
            self.client.connection_phase(),
            access.as_ref(),
            &self.client.client_id,
        )
    }

    fn remote_to_local_window(&self, remote_window_id: WindowId) -> Option<WindowId> {
        let map = self.remote_to_local_window.lock().unwrap();
        map.get(&remote_window_id).cloned()
    }

    pub(crate) fn expire_stale_mappings(&self) {
        let mux = Mux::get();

        self.remote_to_local_pane
            .lock()
            .unwrap()
            .retain(|_remote_pane_id, local_pane_id| mux.get_pane(*local_pane_id).is_some());

        self.remote_to_local_tab
            .lock()
            .unwrap()
            .retain(
                |remote_tab_id, local_tab_id| match mux.get_tab(*local_tab_id) {
                    Some(tab) => {
                        for pane in tab.iter_all_panes() {
                            if pane.domain_id() == self.local_domain_id {
                                return true;
                            }
                        }
                        log::trace!(
                            "expire_stale_mappings: domain: {}. will remove \
                            {remote_tab_id} -> {local_tab_id} tab mapping \
                            because tab contains no panes from this domain",
                            self.local_domain_id,
                        );
                        false
                    }
                    None => false,
                },
            );

        self.remote_to_local_window
            .lock()
            .unwrap()
            .retain(
                |_remote_window_id, local_window_id| match mux.get_window(*local_window_id) {
                    Some(w) => {
                        for tab in w.iter() {
                            for pane in tab.iter_all_panes() {
                                if pane.domain_id() == self.local_domain_id {
                                    return true;
                                }
                            }
                        }
                        false
                    }
                    None => false,
                },
            );
    }

    fn record_remote_to_local_window_mapping(
        &self,
        remote_window_id: WindowId,
        local_window_id: WindowId,
    ) {
        let mut map = self.remote_to_local_window.lock().unwrap();
        map.insert(remote_window_id, local_window_id);
        log::trace!(
            "record_remote_to_local_window_mapping: {} -> {}",
            remote_window_id,
            local_window_id
        );
    }

    fn local_to_remote_tab(&self, local_tab_id: TabId) -> Option<TabId> {
        {
            let map = self.remote_to_local_tab.lock().unwrap();
            for (remote, local) in map.iter() {
                if *local == local_tab_id {
                    return Some(*remote);
                }
            }
        }

        // A replacement runtime installs fresh ClientPanes before the GUI is
        // allowed to publish its recovery viewport.  If a concurrent topology
        // sweep dropped only the tab map, the panes still carry an
        // unambiguous, generation-bound remote tab id. Reconstruct that
        // derived index instead of leaving the frontend permanently stuck in
        // Syncing with `tab ... has no remote mapping`.
        let tab = Mux::get().get_tab(local_tab_id)?;
        let remote_server_id = self.client.remote_server_id();
        let remote_tab_id =
            consistent_remote_tab_id(tab.iter_all_panes().into_iter().filter_map(|pane| {
                pane.downcast_ref::<ClientPane>()
                    .filter(|pane| {
                        pane.domain_id() == self.local_domain_id
                            && pane.belongs_to_remote_server(remote_server_id.as_deref())
                    })
                    .map(ClientPane::remote_tab_id)
            }))?;

        self.record_remote_to_local_tab_mapping(remote_tab_id, local_tab_id);
        log::info!(
            "recovered missing remote tab mapping {remote_tab_id} -> {local_tab_id} from live panes"
        );
        Some(remote_tab_id)
    }

    fn local_to_remote_window(&self, local_window_id: WindowId) -> Option<WindowId> {
        let map = self.remote_to_local_window.lock().unwrap();
        for (remote, local) in map.iter() {
            if *local == local_window_id {
                return Some(*remote);
            }
        }
        None
    }

    /// Record one status into the remote-keyed snapshot (`None` clears).
    /// Called for every AgentStatusChanged push, mapped or not, so a
    /// mirror that materializes later can seed itself; the `None` path is
    /// driven by PaneRemoved on the client side — the server never pushes
    /// an eviction status of its own.
    pub fn record_remote_agent_status(
        &self,
        remote_pane_id: PaneId,
        status: Option<thinkterm_proto::AgentStatus>,
    ) {
        let mut map = self.remote_agent_statuses.lock().unwrap();
        match status.filter(|s| s.within_budget()) {
            Some(status) => {
                map.insert(remote_pane_id, status);
            }
            None => {
                map.remove(&remote_pane_id);
            }
        }
    }

    /// The last status the server reported for a remote pane, if any.
    pub fn remote_agent_status(
        &self,
        remote_pane_id: PaneId,
    ) -> Option<thinkterm_proto::AgentStatus> {
        self.remote_agent_statuses
            .lock()
            .unwrap()
            .get(&remote_pane_id)
            .cloned()
    }

    /// Record one foreground program into the remote-keyed snapshot, on
    /// the same terms as [`Self::record_remote_agent_status`].
    pub fn record_remote_foreground_program(
        &self,
        remote_pane_id: PaneId,
        program: Option<thinkterm_proto::ForegroundProgram>,
    ) {
        let mut map = self.remote_foreground_programs.lock().unwrap();
        match program.filter(|program| program.within_budget()) {
            Some(program) => {
                map.insert(remote_pane_id, program);
            }
            None => {
                map.remove(&remote_pane_id);
            }
        }
    }

    /// The last foreground program the server reported for a remote pane.
    pub fn remote_foreground_program(
        &self,
        remote_pane_id: PaneId,
    ) -> Option<thinkterm_proto::ForegroundProgram> {
        self.remote_foreground_programs
            .lock()
            .unwrap()
            .get(&remote_pane_id)
            .cloned()
    }

    pub fn remote_to_local_pane_id(&self, remote_pane_id: PaneId) -> Option<TabId> {
        let mut pane_map = self.remote_to_local_pane.lock().unwrap();
        let remote_server_id = self.client.remote_server_id();

        if let Some(id) = pane_map.get(&remote_pane_id).copied() {
            let mapping_is_current = Mux::get().get_pane(id).is_some_and(|pane| {
                pane.downcast_ref::<ClientPane>().is_some_and(|pane| {
                    pane.domain_id() == self.local_domain_id
                        && pane.belongs_to_remote_server(remote_server_id.as_deref())
                })
            });
            if mapping_is_current {
                return Some(id);
            }
            log::debug!(
                "discarding stale remote pane mapping {remote_pane_id} -> {id} after mux runtime change"
            );
            pane_map.remove(&remote_pane_id);
        }

        let mux = Mux::get();

        for pane in mux.iter_panes() {
            if pane.domain_id() != self.local_domain_id {
                continue;
            }
            if let Some(pane) = pane.downcast_ref::<ClientPane>() {
                if pane.remote_pane_id() == remote_pane_id
                    && pane.belongs_to_remote_server(remote_server_id.as_deref())
                {
                    let local_pane_id = pane.pane_id();
                    pane_map.insert(remote_pane_id, local_pane_id);
                    return Some(local_pane_id);
                }
            }
        }
        None
    }
    pub fn remove_old_pane_mapping(&self, remote_pane_id: PaneId) {
        let mut pane_map = self.remote_to_local_pane.lock().unwrap();
        pane_map.remove(&remote_pane_id);
    }

    /// Rewrite the stack ids in a remote pane tree from the server's id
    /// space into stable local ids (allocating on first sight). Keeping the
    /// local id stable across resyncs is what lets stack-keyed GUI state
    /// survive `sync_with_pane_tree` rebuilds.
    pub fn translate_remote_stack_ids(&self, node: &mut mux::tab::PaneNode) {
        use mux::tab::PaneNode;
        match node {
            PaneNode::Split { left, right, .. } => {
                self.translate_remote_stack_ids(left);
                self.translate_remote_stack_ids(right);
            }
            PaneNode::Stack(entry) => {
                if let Some(remote_id) = entry.pane_stack_id {
                    let mut map = self.remote_to_local_stack.lock().unwrap();
                    let local_id = *map
                        .entry(remote_id)
                        .or_insert_with(mux::tab::alloc_pane_stack_id);
                    entry.pane_stack_id = Some(local_id);
                }
            }
            PaneNode::Leaf(_) | PaneNode::Empty => {}
        }
    }

    pub fn remove_old_tab_mapping(&self, remote_tab_id: TabId) {
        let mut tab_map = self.remote_to_local_tab.lock().unwrap();
        let old = tab_map.remove(&remote_tab_id);
        log::trace!("remove_old_tab_mapping: {remote_tab_id} -> {old:?}");
    }

    fn record_remote_to_local_tab_mapping(&self, remote_tab_id: TabId, local_tab_id: TabId) {
        let mut map = self.remote_to_local_tab.lock().unwrap();
        let prior = map.insert(remote_tab_id, local_tab_id);
        log::trace!(
            "record_remote_to_local_tab_mapping: {} -> {} \
             (prior={prior:?}, domain={})",
            remote_tab_id,
            local_tab_id,
            self.local_domain_id,
        );
    }

    pub fn remote_to_local_tab_id(&self, remote_tab_id: TabId) -> Option<TabId> {
        let map = self.remote_to_local_tab.lock().unwrap();
        map.get(&remote_tab_id).copied()
    }

    /// Lease generations are a server process's own counter. A new
    /// connection may be to a new process (a takeover keeps the runtime id
    /// and every pane, a crash brings a replacement), whose counter starts
    /// again below what this client remembers; kept, that memory would
    /// reject every lease update from it, including the one that makes
    /// this client the owner, and the terminal would stay claimable for
    /// good. Forgotten at every (re)connection instead.
    fn forget_lease_generations(&self) {
        self.remote_access.lock().unwrap().take();
        self.remote_viewports.lock().unwrap().clear();
    }

    fn update_remote_viewport(&self, state: codec::ClientViewportState) -> bool {
        let mut states = self.remote_viewports.lock().unwrap();
        if !accepts_generation(
            states.get(&state.tab_id).map(|prior| prior.generation),
            state.generation,
        ) {
            return false;
        }
        states.insert(state.tab_id, state);
        true
    }

    pub fn remote_viewport_state(
        &self,
        remote_tab_id: TabId,
    ) -> Option<codec::ClientViewportState> {
        self.remote_viewports
            .lock()
            .unwrap()
            .get(&remote_tab_id)
            .cloned()
    }

    pub fn owns_remote_viewport(&self, remote_tab_id: TabId) -> Option<bool> {
        let access = self.remote_access_state();
        let viewport = self.remote_viewport_state(remote_tab_id);
        owns_remote_viewport_from_states(&self.client.client_id, access.as_ref(), viewport.as_ref())
    }

    /// Drop connection-scoped state before registering a new transport
    /// generation.  Keep the latest locally-rendered geometry: replaying it
    /// after SetClientId is how the new live session obtains authoritative
    /// access/owner state without claiming ownership or waiting for an
    /// incidental GUI resize.
    fn begin_remote_generation(&self) {
        self.remote_viewports.lock().unwrap().clear();
        *self.remote_access.lock().unwrap() = None;
        self.frontend_claim_locks.lock().unwrap().clear();
        *self.frontend_recovery_barrier.lock().unwrap() = None;
    }

    /// Capture only workspaces for which this frontend actually advertised a
    /// viewport. Every mux client mirrors the server's complete topology, so
    /// using every mirrored window here would eagerly restart background
    /// Threads that this device was not displaying.
    fn reconnect_recovery_targets(&self) -> Vec<ReconnectRecoveryTarget> {
        let mut targets = self
            .frontend_recovery_intents
            .lock()
            .unwrap()
            .values()
            .cloned()
            .collect::<Vec<_>>();
        targets.sort_by_key(|target| target.slot);
        targets
    }

    /// The runtime these bindings named is gone and nothing on the
    /// replacement corresponds to them: forget them before anything is
    /// asked of the new server by an old id. The local mirrors stay until
    /// `reap_dead_runtime_mirrors`.
    fn forget_dead_runtime_bindings(&self) {
        self.pending_recovery_windows.lock().unwrap().clear();
        self.remote_to_local_window.lock().unwrap().clear();
        self.remote_to_local_tab.lock().unwrap().clear();
        self.remote_to_local_pane.lock().unwrap().clear();
        self.remote_to_local_stack.lock().unwrap().clear();
        self.reported_viewports.lock().unwrap().clear();
        self.remote_agent_statuses.lock().unwrap().clear();
        self.remote_foreground_programs.lock().unwrap().clear();
    }

    /// Kill every local mirror of this domain: the terminals they showed
    /// died with the old runtime. Nothing is sent to the server (the ids
    /// mean nothing there), and the windows left empty are pruned, which
    /// is what makes the GUI rebuild them from its layout store.
    fn reap_dead_runtime_mirrors(&self) {
        let mux = Mux::get();
        let mut dead = Vec::new();
        for pane in mux.iter_panes() {
            if pane.domain_id() != self.local_domain_id {
                continue;
            }
            if let Some(client_pane) = pane.downcast_ref::<ClientPane>() {
                client_pane.ignore_next_kill();
            }
            dead.push(pane.pane_id());
        }
        log::info!(
            "dropping {} mirrors of the replaced session server's terminals",
            dead.len()
        );
        mux.domain_was_detached(self.local_domain_id);
    }

    /// Prepare to bind a fresh mux runtime into the local windows that were
    /// already displaying it. The old tabs stay alive and opaque until the
    /// replacement topology has been installed; they are removed only after
    /// new tabs exist, so the native GUI window is never pruned mid-recovery.
    fn prepare_server_replacement(&self) -> ServerReplacementMirrors {
        let mux = Mux::get();
        let local_windows = self
            .remote_to_local_window
            .lock()
            .unwrap()
            .values()
            .copied()
            .collect::<HashSet<_>>();
        let mut mirrors = ServerReplacementMirrors {
            windows_by_workspace: self.pending_recovery_windows.lock().unwrap().clone(),
            old_tabs: Vec::new(),
            created_windows: Vec::new(),
        };
        for window_id in local_windows {
            let Some(window) = mux.get_window(window_id) else {
                continue;
            };
            mirrors
                .windows_by_workspace
                .entry(window.get_workspace().to_string())
                .or_insert(window_id);
        }
        // Recompute this from all retained windows so a partial prior attempt
        // is cleaned up together with the original mirror after a successful
        // retry.
        for window_id in mirrors.windows_by_workspace.values().copied() {
            if let Some(window) = mux.get_window(window_id) {
                mirrors
                    .old_tabs
                    .extend(window.iter().map(|tab| tab.tab_id()));
            }
        }
        *self.pending_recovery_windows.lock().unwrap() = mirrors.windows_by_workspace.clone();

        self.remote_to_local_window.lock().unwrap().clear();
        self.remote_to_local_tab.lock().unwrap().clear();
        self.remote_to_local_pane.lock().unwrap().clear();
        self.remote_to_local_stack.lock().unwrap().clear();
        self.reported_viewports.lock().unwrap().clear();
        // The replacement server allocates pane ids from scratch, so the
        // retained statuses describe panes that no longer exist. Left in
        // place they would seed the replacement's mirrors (created before
        // the post-replacement fetch) with the dead server's agents, and
        // its programs.
        self.remote_agent_statuses.lock().unwrap().clear();
        self.remote_foreground_programs.lock().unwrap().clear();
        mirrors
    }

    fn reported_viewports_for_live_tabs(&self) -> Vec<(TabId, codec::ClientViewport)> {
        let live_tabs: HashSet<TabId> = self
            .remote_to_local_tab
            .lock()
            .unwrap()
            .keys()
            .copied()
            .collect();
        let mut reported = self.reported_viewports.lock().unwrap();
        reported.retain(|tab_id, _| live_tabs.contains(tab_id));
        reported
            .iter()
            .map(|(tab_id, viewport)| (*tab_id, viewport.clone()))
            .collect()
    }

    fn update_remote_access(&self, state: codec::FrontendAccessState) -> bool {
        let mut prior = self.remote_access.lock().unwrap();
        if !accepts_generation(
            prior.as_ref().map(|prior| prior.generation),
            state.generation,
        ) {
            return false;
        }
        *prior = Some(state);
        true
    }

    pub fn remote_access_state(&self) -> Option<codec::FrontendAccessState> {
        self.remote_access.lock().unwrap().clone()
    }

    pub fn has_remote_access(&self) -> Option<bool> {
        let state = self.remote_access_state()?;
        Some(match state.mode {
            codec::FrontendAccessMode::TmuxLatest => true,
            codec::FrontendAccessMode::Handoff => self.remote_owner_is_self(state.owner.as_ref()),
        })
    }

    fn remember_reported_viewport(&self, tab_id: TabId, viewport: codec::ClientViewport) {
        self.reported_viewports
            .lock()
            .unwrap()
            .insert(tab_id, viewport);
    }

    fn forget_reported_viewport(&self, tab_id: TabId) {
        self.reported_viewports.lock().unwrap().remove(&tab_id);
    }

    /// Re-advertise one viewport after reconnecting to the runtime that
    /// last accepted it. A refusal the server explains is not a failure
    /// of the reattach: the transport is up and the refused geometry is
    /// simply out of date. The entry is retried without its panes, and
    /// if that is refused too it is dropped; the frontend reports the
    /// tab again from its live layout. `Ok(None)` is such a drop. A
    /// transport failure is returned as it is, so the caller still
    /// abandons the generation.
    async fn restore_reported_viewport(
        &self,
        remote_tab_id: TabId,
        viewport: codec::ClientViewport,
        live_panes: Option<&HashSet<PaneId>>,
        server_name: &str,
    ) -> anyhow::Result<Option<codec::ClientViewportState>> {
        let Some(live_panes) = live_panes else {
            log::warn!(
                "not restoring the viewport of remote tab {remote_tab_id} on {server_name}: \
                 the server no longer lists that tab"
            );
            self.forget_reported_viewport(remote_tab_id);
            return Ok(None);
        };
        let paneless = codec::ClientViewport::Native {
            size: viewport.size(),
            panes: Vec::new(),
        };
        // The recorded viewport first, then the paneless one as a last
        // resort; one already known to be stale skips straight to that.
        let attempts = match reattach_viewport_fallback(&viewport, live_panes) {
            Some(fallback) => {
                log::warn!(
                    "restoring remote tab {remote_tab_id} on {server_name} without its pane \
                     layout: it names a pane the tab no longer contains"
                );
                vec![fallback]
            }
            None if viewport_has_panes(&viewport) => vec![viewport.clone(), paneless],
            None => vec![viewport.clone()],
        };
        for attempt in attempts {
            match self
                .client
                .set_client_viewport(codec::SetClientViewport {
                    tab_id: remote_tab_id,
                    viewport: attempt.clone(),
                })
                .await
            {
                Ok(state) => {
                    if attempt != viewport {
                        self.remember_reported_viewport(remote_tab_id, attempt);
                    }
                    return Ok(Some(state));
                }
                Err(err) if crate::client::RemoteRpcError::is_cause_of(&err) => {
                    log::warn!(
                        "{server_name} refused the restored viewport of remote tab \
                         {remote_tab_id}: {err:#}"
                    );
                }
                Err(err) => {
                    return Err(err).with_context(|| {
                        format!("restoring viewport and access state for remote tab {remote_tab_id}")
                    });
                }
            }
        }
        log::warn!(
            "dropping the recorded viewport of remote tab {remote_tab_id} on {server_name}; \
             the frontend will report it again"
        );
        self.forget_reported_viewport(remote_tab_id);
        Ok(None)
    }

    fn claim_lock(&self, tab_id: TabId) -> Arc<futures::lock::Mutex<()>> {
        self.frontend_claim_locks
            .lock()
            .unwrap()
            .entry(tab_id)
            .or_insert_with(|| Arc::new(futures::lock::Mutex::new(())))
            .clone()
    }

    /// Return false only for B's opaque non-owner state. In A, serialize an
    /// atomic claim before forwarding the terminal input.
    pub(crate) async fn prepare_remote_tab_input(
        &self,
        remote_tab_id: TabId,
    ) -> anyhow::Result<bool> {
        // The local session host's terminals are this machine's: the lease
        // decides who paints, not whether a layout may be restored into it.
        // Gating here refused the splits of a saved layout while the first
        // viewport was still being settled, and the thread stayed unbuilt.
        if self.client.is_local_session_host() {
            return Ok(true);
        }
        if self.remote_frontend_gate().obscures_terminal() {
            return Ok(false);
        }
        let Some(access) = self.remote_access_state() else {
            return Ok(false);
        };
        match access.mode {
            codec::FrontendAccessMode::Handoff => {
                return Ok(self.remote_owner_is_self(access.owner.as_ref()));
            }
            codec::FrontendAccessMode::TmuxLatest => {}
        }
        if self
            .remote_viewport_state(remote_tab_id)
            .is_some_and(|state| self.remote_owner_is_self(state.owner.as_ref()))
        {
            return Ok(true);
        }
        let lock = self.claim_lock(remote_tab_id);
        let _guard = lock.lock().await;
        if self
            .remote_viewport_state(remote_tab_id)
            .is_some_and(|state| self.remote_owner_is_self(state.owner.as_ref()))
        {
            return Ok(true);
        }
        let viewport = self
            .reported_viewports
            .lock()
            .unwrap()
            .get(&remote_tab_id)
            .cloned()
            .ok_or_else(|| anyhow!("viewport for remote tab {remote_tab_id} is not ready"))?;
        let state = self
            .client
            .claim_client_viewport(codec::ClaimClientViewport {
                tab_id: remote_tab_id,
                viewport,
            })
            .await?;
        let owns = self.remote_owner_is_self(state.owner.as_ref());
        self.update_remote_access(state.access.clone());
        self.update_remote_viewport(state);
        Ok(owns)
    }

    pub(crate) fn remote_tab_input_is_blocked(&self) -> bool {
        self.remote_frontend_gate().obscures_terminal()
    }

    pub fn is_local(&self) -> bool {
        self.client.is_local
    }
}

#[derive(Clone, Debug)]
pub enum ClientDomainConfig {
    Unix(UnixDomain),
    Tls(TlsDomainClient),
    Ssh(SshDomain),
}

impl ClientDomainConfig {
    pub fn name(&self) -> &str {
        match self {
            ClientDomainConfig::Unix(unix) => &unix.name,
            ClientDomainConfig::Tls(tls) => &tls.name,
            ClientDomainConfig::Ssh(ssh) => &ssh.name,
        }
    }

    /// The session server of the machine the GUI runs on; see
    /// `UnixDomain::local_session_host`.
    pub fn is_local_session_host(&self) -> bool {
        matches!(self, ClientDomainConfig::Unix(unix) if unix.local_session_host)
    }

    pub fn local_echo_threshold_ms(&self) -> Option<u64> {
        match self {
            ClientDomainConfig::Unix(unix) => unix.local_echo_threshold_ms,
            ClientDomainConfig::Tls(tls) => tls.local_echo_threshold_ms,
            ClientDomainConfig::Ssh(ssh) => ssh.local_echo_threshold_ms,
        }
    }

    pub fn overlay_lag_indicator(&self) -> bool {
        match self {
            ClientDomainConfig::Unix(unix) => unix.overlay_lag_indicator,
            ClientDomainConfig::Tls(tls) => tls.overlay_lag_indicator,
            ClientDomainConfig::Ssh(ssh) => ssh.overlay_lag_indicator,
        }
    }

    pub fn label(&self) -> String {
        match self {
            ClientDomainConfig::Unix(unix) => format!("unix mux {}", unix.socket_path().display()),
            ClientDomainConfig::Tls(tls) => format!("TLS mux {}", tls.remote_address),
            ClientDomainConfig::Ssh(ssh) => {
                if let Some(user) = &ssh.username {
                    format!("SSH mux {}@{}", user, ssh.remote_address)
                } else {
                    format!("SSH mux {}", ssh.remote_address)
                }
            }
        }
    }

    pub fn connect_automatically(&self) -> bool {
        match self {
            ClientDomainConfig::Unix(unix) => unix.connect_automatically,
            ClientDomainConfig::Tls(tls) => tls.connect_automatically,
            ClientDomainConfig::Ssh(ssh) => ssh.connect_automatically,
        }
    }
}

impl ClientInner {
    pub fn new(
        local_domain_id: DomainId,
        client: Client,
        local_echo_threshold_ms: Option<u64>,
        overlay_lag_indicator: bool,
    ) -> Self {
        let ready_server_id = client.remote_server_id();
        Self {
            client,
            local_domain_id,
            local_echo_threshold_ms,
            overlay_lag_indicator,
            remote_to_local_window: Mutex::new(HashMap::new()),
            remote_to_local_tab: Mutex::new(HashMap::new()),
            remote_to_local_pane: Mutex::new(HashMap::new()),
            remote_agent_statuses: Mutex::new(HashMap::new()),
            remote_foreground_programs: Mutex::new(HashMap::new()),
            pending_kills: Mutex::new(HashSet::new()),
            remote_viewports: Mutex::new(HashMap::new()),
            remote_access: Mutex::new(None),
            reported_viewports: Mutex::new(HashMap::new()),
            frontend_recovery_intents: Mutex::new(HashMap::new()),
            frontend_claim_locks: Mutex::new(HashMap::new()),
            remote_to_local_stack: Mutex::new(HashMap::new()),
            focused_remote_pane_id: Mutex::new(None),
            focus_advised_at: Mutex::new(None),
            mutations_in_flight: std::sync::atomic::AtomicUsize::new(0),
            resync_deferred: std::sync::atomic::AtomicBool::new(false),
            frontend_recovery_barrier: Mutex::new(None),
            ready_server_id: Mutex::new(ready_server_id),
            pending_recovery_targets: Mutex::new(HashMap::new()),
            pending_recovery_windows: Mutex::new(HashMap::new()),
            viewport_latency: Mutex::new(ViewportLatencyState::default()),
        }
    }
}

impl ClientInner {
    pub(crate) fn note_pending_kill(&self, remote_pane_id: PaneId) {
        self.pending_kills.lock().unwrap().insert(remote_pane_id);
    }

    pub(crate) fn forget_pending_kill(&self, remote_pane_id: PaneId) {
        self.pending_kills.lock().unwrap().remove(&remote_pane_id);
    }

    /// Whether every pane of this tab is one we asked the server to kill.
    fn tab_is_being_killed(&self, tabroot: &mux::tab::PaneNode) -> bool {
        let mut pane_ids = Vec::new();
        collect_pane_ids(tabroot, &mut pane_ids);
        if pane_ids.is_empty() {
            return false;
        }
        let pending = self.pending_kills.lock().unwrap();
        pane_ids.iter().all(|pane_id| pending.contains(pane_id))
    }
}

fn collect_pane_ids(node: &mux::tab::PaneNode, out: &mut Vec<PaneId>) {
    match node {
        mux::tab::PaneNode::Empty => {}
        mux::tab::PaneNode::Split { left, right, .. } => {
            collect_pane_ids(left, out);
            collect_pane_ids(right, out);
        }
        mux::tab::PaneNode::Leaf(entry) => out.push(entry.pane_id),
        mux::tab::PaneNode::Stack(stack) => {
            out.extend(stack.panes.iter().map(|entry| entry.pane_id))
        }
    }
}

/// Keep a mirrored window's tabs in the order the server lists them: move
/// `tab_id` to `*slot`, the next listed position, and move that on. A tab the
/// window does not hold takes no position.
fn place_in_listed_order(window: &mut Window, slot: &mut usize, tab_id: TabId) {
    let Some(at) = window.idx_by_id(tab_id) else {
        return;
    };
    if at != *slot && *slot < window.len() {
        let active = window.get_active().map(|tab| tab.tab_id());
        let moved = window.remove_by_idx(at);
        window.insert(*slot, &moved);
        if let Some(active) = active.and_then(|id| window.idx_by_id(id)) {
            window.set_active_without_saving(active);
        }
    }
    *slot += 1;
}

/// `place_in_listed_order` for `window_id`, with the positions `placed` has
/// handed out in it so far.
fn place_listed_tab(
    mux: &Mux,
    placed: &mut HashMap<WindowId, usize>,
    window_id: WindowId,
    tab_id: TabId,
) {
    if let Some(mut window) = mux.get_window_mut(window_id) {
        place_in_listed_order(&mut window, placed.entry(window_id).or_insert(0), tab_id);
    }
}

/// The panes the server lists under each remote tab. A tab whose tree is
/// empty has no entry.
fn remote_tab_panes(tabs: &[mux::tab::PaneNode]) -> HashMap<TabId, HashSet<PaneId>> {
    fn walk(node: &mux::tab::PaneNode, out: &mut HashMap<TabId, HashSet<PaneId>>) {
        match node {
            mux::tab::PaneNode::Empty => {}
            mux::tab::PaneNode::Split { left, right, .. } => {
                walk(left, out);
                walk(right, out);
            }
            mux::tab::PaneNode::Leaf(entry) => {
                out.entry(entry.tab_id).or_default().insert(entry.pane_id);
            }
            mux::tab::PaneNode::Stack(stack) => {
                for entry in &stack.panes {
                    out.entry(entry.tab_id).or_default().insert(entry.pane_id);
                }
            }
        }
    }
    let mut out = HashMap::new();
    for node in tabs {
        walk(node, &mut out);
    }
    out
}

/// A viewport recorded before an outage, checked against the panes the
/// server still holds in its tab. `None` when it can be replayed as it
/// is. One that names a pane the tab no longer contains would be refused
/// outright, so it comes back with its panes removed: the tab keeps its
/// size and the server's own split tree, and the frontend describes the
/// layout again from what it really shows.
fn reattach_viewport_fallback(
    viewport: &codec::ClientViewport,
    tab_panes: &HashSet<PaneId>,
) -> Option<codec::ClientViewport> {
    match viewport {
        codec::ClientViewport::CellGrid { .. } => None,
        codec::ClientViewport::Native { size, panes } => {
            if panes.iter().all(|pane| tab_panes.contains(&pane.pane_id)) {
                None
            } else {
                Some(codec::ClientViewport::Native {
                    size: *size,
                    panes: Vec::new(),
                })
            }
        }
    }
}

/// Whether a viewport carries pane geometry that a refusal could be about.
fn viewport_has_panes(viewport: &codec::ClientViewport) -> bool {
    matches!(viewport, codec::ClientViewport::Native { panes, .. } if !panes.is_empty())
}

impl ClientInner {
    fn begin_structure_mutation(self: &Arc<Self>) -> StructureMutationGuard {
        self.mutations_in_flight
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        StructureMutationGuard {
            inner: Arc::downgrade(self),
        }
    }

    fn structure_mutation_in_flight(&self) -> bool {
        self.release_stale_frontend_recovery_barrier();
        self.mutations_in_flight
            .load(std::sync::atomic::Ordering::SeqCst)
            > 0
    }

    fn defer_resync(&self) {
        self.resync_deferred
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    fn begin_frontend_recovery(
        &self,
        generation: u64,
        targets: impl IntoIterator<Item = (FrontendRecoverySlot, TabId)>,
        structure: Option<StructureMutationGuard>,
    ) {
        let pending = targets.into_iter().collect::<HashMap<_, _>>();
        if generation != self.client.connection_generation() {
            // A newer attempt already owns the slot; a barrier for this
            // one would never be acknowledged and would hold resyncs back
            // for good.
            log::info!(
                "not arming the frontend recovery barrier for mux generation {generation}: \
                 the connection is at generation {}",
                self.client.connection_generation()
            );
            return;
        }
        *self.frontend_recovery_barrier.lock().unwrap() =
            Some(FrontendRecoveryBarrier::new(generation, pending, structure));
    }

    /// Drop a recovery barrier the frontend is never going to acknowledge:
    /// one from a superseded generation, or one older than
    /// `FRONTEND_RECOVERY_DEADLINE` (a tab the user switched away from never
    /// publishes its geometry). Its hold on push-driven resyncs goes with it.
    fn release_stale_frontend_recovery_barrier(&self) {
        let stale = {
            let mut barrier = self.frontend_recovery_barrier.lock().unwrap();
            let Some(armed) = barrier.as_ref() else {
                return;
            };
            let superseded = armed.generation != self.client.connection_generation();
            let overdue = armed.started_at.elapsed() > FRONTEND_RECOVERY_DEADLINE;
            if !superseded && !overdue {
                return;
            }
            barrier.take().map(|b| (b.generation, superseded, b.started_at.elapsed()))
        };
        if let Some((generation, superseded, waited)) = stale {
            log::warn!(
                "releasing the frontend recovery barrier for mux generation {generation} after {waited:?}: {}",
                if superseded { "superseded" } else { "never acknowledged" }
            );
            // An overdue barrier of the live generation was the last thing
            // between this connection and Ready: the reattach returned early
            // to wait for it, and nothing else marks the session usable.
            // Left as it was, the panes sat behind "Restoring terminal
            // state…" for as long as the connection lived, and the
            // reconnect loop later read that as an outage still running.
            if !superseded {
                self.mark_server_recovered();
                self.client.mark_ready();
                wake_thinkterm_frontend();
            }
        }
    }

    fn ready_server_id(&self) -> Option<String> {
        self.ready_server_id.lock().unwrap().clone()
    }

    fn mark_server_recovered(&self) {
        *self.ready_server_id.lock().unwrap() = self.client.remote_server_id();
        self.pending_recovery_targets.lock().unwrap().clear();
        self.pending_recovery_windows.lock().unwrap().clear();
    }

    fn recovery_targets(&self) -> Vec<ReconnectRecoveryTarget> {
        let pending = self.pending_recovery_targets.lock().unwrap();
        if pending.is_empty() {
            drop(pending);
            self.reconnect_recovery_targets()
        } else {
            let mut targets = pending.values().cloned().collect::<Vec<_>>();
            targets.sort_by_key(|target| target.slot);
            targets
        }
    }

    fn remember_recovery_targets(&self, targets: &[ReconnectRecoveryTarget]) {
        let mut pending = self.pending_recovery_targets.lock().unwrap();
        if pending.is_empty() {
            pending.extend(targets.iter().cloned().map(|target| (target.slot, target)));
        }
    }

    fn pending_frontend_recovery(
        &self,
        slot: FrontendRecoverySlot,
        local_tab_id: TabId,
    ) -> Option<u64> {
        let barrier = self.frontend_recovery_barrier.lock().unwrap();
        let barrier = barrier.as_ref()?;
        (barrier.generation == self.client.connection_generation()
            && barrier.pending.get(&slot) == Some(&local_tab_id))
        .then_some(barrier.generation)
    }

    fn acknowledge_frontend_recovery(
        &self,
        slot: FrontendRecoverySlot,
        local_tab_id: TabId,
        generation: u64,
    ) -> bool {
        let (ack, waited) = {
            let mut barrier = self.frontend_recovery_barrier.lock().unwrap();
            let started_at = barrier.as_ref().map(|barrier| barrier.started_at);
            let ack = acknowledge_recovery_target(
                &mut barrier,
                slot,
                local_tab_id,
                generation,
                self.client.connection_generation(),
            );
            (ack, started_at.map(|started_at| started_at.elapsed()))
        };
        if ack == FrontendRecoveryAck::Complete {
            self.mark_server_recovered();
            self.client.mark_ready();
            log::info!(
                "frontend geometry restored for mux generation {generation} after {:?}",
                waited.unwrap_or_default()
            );
            wake_thinkterm_frontend();
        }
        ack != FrontendRecoveryAck::Ignored
    }

    fn fail_frontend_recovery(
        &self,
        slot: FrontendRecoverySlot,
        local_tab_id: TabId,
        generation: u64,
        reason: String,
    ) -> bool {
        if self.pending_frontend_recovery(slot, local_tab_id) != Some(generation) {
            return false;
        }
        self.client.abort_connection_generation(generation, reason);
        true
    }
}

/// Lease state the server pushed before this domain had an inner to keep
/// it in. A server tells a registering client where the frontend lease
/// stands right after the handshake, which is before the attach has built
/// the inner; thrown away, that state never came again for a tab nobody
/// touched, and the frontend sat behind "Restoring terminal state".
#[derive(Default)]
struct EarlyRemoteState {
    access: Option<codec::FrontendAccessState>,
    viewports: Vec<codec::ClientViewportState>,
}

pub struct ClientDomain {
    config: ClientDomainConfig,
    label: String,
    inner: Mutex<Option<Arc<ClientInner>>>,
    early_remote_state: Mutex<EarlyRemoteState>,
    /// True while an attach is in flight (state() stays Detached until
    /// finish_attach installs the inner, so state alone can't dedupe).
    attaching: std::sync::atomic::AtomicBool,
    /// Count of attach retry loops currently running, backoff gaps included.
    /// `attaching` covers only a single attempt; the UI needs to know the
    /// engine has not given up between attempts, or every backoff gap paints
    /// as a disconnect.
    attach_retries: std::sync::atomic::AtomicUsize,
    /// Serializes topology snapshots. Immediate callers join an active
    /// generation; background pushes coalesce behind one trailing pass.
    resync_coordinator: Mutex<ResyncCoordinatorState>,
    local_domain_id: DomainId,
}

async fn update_remote_workspace(
    local_domain_id: DomainId,
    pdu: codec::SetWindowWorkspace,
) -> anyhow::Result<()> {
    let inner = ClientDomain::get_client_inner_for_domain(local_domain_id)?;
    inner.client.set_window_workspace(pdu).await?;
    Ok(())
}

fn mux_notify_client_domain(local_domain_id: DomainId, notif: MuxNotification) -> bool {
    let mux = Mux::get();
    let domain = match mux.get_domain(local_domain_id) {
        Some(domain) => domain,
        None => return false,
    };
    let client_domain = match domain.downcast_ref::<ClientDomain>() {
        Some(c) => c,
        None => return false,
    };

    match notif {
        MuxNotification::ActiveWorkspaceChanged(_client_id) => {
            // TODO: advice remote host of interesting workspaces
        }
        MuxNotification::WorkspaceRenamed {
            old_workspace,
            new_workspace,
        } => {
            if let Some(inner) = client_domain.inner() {
                let workspaces = Mux::get().iter_workspaces();
                if workspaces.contains(&old_workspace) {
                    promise::spawn::spawn(async move {
                        inner
                            .client
                            .rename_workspace(codec::RenameWorkspace {
                                old_workspace,
                                new_workspace,
                            })
                            .await
                    })
                    .detach();
                }
            }
        }
        MuxNotification::WindowWorkspaceChanged(window_id) => {
            // Mux::get_window() may trigger a borrow error if called
            // immediately; defer the bulk of this work.
            // <https://github.com/wezterm/wezterm/issues/2638>
            promise::spawn::spawn_into_main_thread(async move {
                let mux = Mux::get();
                let domain = match mux.get_domain(local_domain_id) {
                    Some(domain) => domain,
                    None => return,
                };
                let domain = match domain.downcast_ref::<ClientDomain>() {
                    Some(domain) => domain,
                    None => return,
                };
                if let Some(remote_window_id) = domain.local_to_remote_window_id(window_id) {
                    if let Some(workspace) = mux
                        .get_window(window_id)
                        .map(|w| w.get_workspace().to_string())
                    {
                        promise::spawn::spawn_into_main_thread(async move {
                            let request = codec::SetWindowWorkspace {
                                window_id: remote_window_id,
                                workspace,
                            };
                            let _ = update_remote_workspace(local_domain_id, request).await;
                        })
                        .detach();
                    }
                } else {
                    log::debug!(
                        "local window id {window_id} has no known remote window \
                        id while reconciling a local WindowWorkspaceChanged event"
                    );
                }
            })
            .detach();
        }
        MuxNotification::TabTitleChanged { tab_id, title } => {
            if let Some(remote_tab_id) = client_domain.local_to_remote_tab_id(tab_id) {
                if let Some(inner) = client_domain.inner() {
                    promise::spawn::spawn(async move {
                        inner
                            .client
                            .set_tab_title(codec::TabTitleChanged {
                                tab_id: remote_tab_id,
                                title,
                            })
                            .await
                    })
                    .detach();
                }
            }
        }
        MuxNotification::WindowTitleChanged {
            window_id,
            title: _,
        } => {
            if let Some(remote_window_id) = client_domain.local_to_remote_window_id(window_id) {
                if let Some(inner) = client_domain.inner() {
                    promise::spawn::spawn_into_main_thread(async move {
                        // De-bounce the title propagation.
                        // There is a bit of a race condition with these async
                        // updates that can trigger a cycle of WindowTitleChanged
                        // PDUs being exchanged between client and server if the
                        // title is changed twice in quick succession.
                        // To avoid that, here on the client, we wait a second
                        // and then report the now-current name of the window, rather
                        // than propagating the title encoded in the MuxNotification.
                        smol::Timer::after(std::time::Duration::from_secs(1)).await;
                        if let Some(mux) = Mux::try_get() {
                            let title = mux
                                .get_window(window_id)
                                .map(|win| win.get_title().to_string());
                            if let Some(title) = title {
                                inner
                                    .client
                                    .set_window_title(codec::WindowTitleChanged {
                                        window_id: remote_window_id,
                                        title,
                                    })
                                    .await?;
                            }
                        }
                        anyhow::Result::<()>::Ok(())
                    })
                    .detach();
                }
            }
        }
        _ => {}
    }
    true
}

/// Receives a mux server's ThinkTerm sidebar tree whenever it arrives, either
/// as the answer to a fetch or as a server push.
///
/// `wezterm-client` has no way to reach into the GUI's store, and the GUI is
/// the only thing that knows how to merge a shared tree with this device's own
/// state, so it registers a sink here at startup. Headless clients (the CLI,
/// the mux server's own client domains) simply never register one and the
/// trees are dropped.
pub type ThinkTermTreeSink = fn(domain_name: &str, tree: codec::ThinkTermTree);

/// Receives the authoritative tree-plus-live-topology view exported by one
/// mux server. Thin frontends keep it separate per client-domain name.
pub type ThinkTermSessionSink =
    fn(domain_name: &str, connection_generation: u64, state: codec::ThinkTermSessionState);

/// Announces a fresh connection to a server, before anything is asked of it.
///
/// Trees do not carry which connection they belong to, and the client has to
/// know: everything it believes about a server's tree describes the previous
/// connection, and a server that comes back having lost or rolled back its own
/// copy must be able to say so rather than be dismissed as out of date.
pub type ThinkTermConnectSink = fn(domain_name: &str, connection_generation: u64);

/// Wakes a blocking thin frontend as soon as the transport reader receives
/// anything.  The actual PDU is still processed on the main-thread executor;
/// this hook only breaks the frontend out of `poll_input(None)` so that the
/// executor can run without a fixed polling timer.
pub type ThinkTermFrontendWakeSink = fn();

/// Requests one geometry publication after a replacement mux topology has
/// been installed and its recovery barrier is armed.  TabAddedToWindow is
/// emitted while the replacement is still being assembled, so a GUI can
/// otherwise publish too early and leave the barrier waiting forever when no
/// later resize or tab switch happens.
pub type ThinkTermFrontendRecoverySink = fn(
    domain_id: DomainId,
    connection_generation: u64,
    recovery_targets: Vec<ThinkTermFrontendRecoveryTarget>,
);

lazy_static::lazy_static! {
    static ref THINKTERM_TREE_SINK: Mutex<Option<ThinkTermTreeSink>> = Mutex::new(None);
    static ref THINKTERM_CONNECT_SINK: Mutex<Option<ThinkTermConnectSink>> = Mutex::new(None);
    static ref THINKTERM_SESSION_SINK: Mutex<Option<ThinkTermSessionSink>> = Mutex::new(None);
    static ref THINKTERM_FRONTEND_WAKE_SINK: Mutex<Option<ThinkTermFrontendWakeSink>> = Mutex::new(None);
    static ref THINKTERM_FRONTEND_RECOVERY_SINK: Mutex<Option<ThinkTermFrontendRecoverySink>> = Mutex::new(None);
}

pub fn set_thinkterm_tree_sink(sink: ThinkTermTreeSink) {
    THINKTERM_TREE_SINK.lock().unwrap().replace(sink);
}

pub fn set_thinkterm_connect_sink(sink: ThinkTermConnectSink) {
    THINKTERM_CONNECT_SINK.lock().unwrap().replace(sink);
}

pub fn set_thinkterm_session_sink(sink: ThinkTermSessionSink) {
    THINKTERM_SESSION_SINK.lock().unwrap().replace(sink);
}

pub fn set_thinkterm_frontend_wake_sink(sink: ThinkTermFrontendWakeSink) {
    THINKTERM_FRONTEND_WAKE_SINK.lock().unwrap().replace(sink);
}

pub fn set_thinkterm_frontend_recovery_sink(sink: ThinkTermFrontendRecoverySink) {
    THINKTERM_FRONTEND_RECOVERY_SINK
        .lock()
        .unwrap()
        .replace(sink);
}

pub(crate) fn wake_thinkterm_frontend() {
    let sink = *THINKTERM_FRONTEND_WAKE_SINK.lock().unwrap();
    if let Some(sink) = sink {
        sink();
    }
}

fn request_thinkterm_frontend_recovery(
    domain_id: DomainId,
    connection_generation: u64,
    recovery_targets: Vec<ThinkTermFrontendRecoveryTarget>,
) {
    let sink = *THINKTERM_FRONTEND_RECOVERY_SINK.lock().unwrap();
    if let Some(sink) = sink {
        sink(domain_id, connection_generation, recovery_targets);
    }
}

pub(crate) fn deliver_thinkterm_tree(config: &ClientDomainConfig, tree: codec::ThinkTermTree) {
    // The local session host's tree mirrors the local Spaces; the sink
    // merges it like any other server's.
    let sink = *THINKTERM_TREE_SINK.lock().unwrap();
    if let Some(sink) = sink {
        sink(config.name(), tree);
    }
}

/// Told when the local session host came back as a different, empty
/// server: every mirror of this domain shows a terminal that no longer
/// exists. The GUI rebuilds each affected window's thread from its layout
/// store; without a sink the mirrors are simply dropped.
pub type LocalSessionHostReplacedSink = fn(DomainId);

lazy_static::lazy_static! {
    static ref LOCAL_SESSION_HOST_REPLACED_SINK: Mutex<Option<LocalSessionHostReplacedSink>> =
        Mutex::new(None);
}

pub fn set_local_session_host_replaced_sink(sink: LocalSessionHostReplacedSink) {
    LOCAL_SESSION_HOST_REPLACED_SINK.lock().unwrap().replace(sink);
}

pub(crate) fn deliver_thinkterm_connected(domain_name: &str, connection_generation: u64) {
    let sink = *THINKTERM_CONNECT_SINK.lock().unwrap();
    if let Some(sink) = sink {
        sink(domain_name, connection_generation);
    }
}

pub(crate) fn deliver_thinkterm_session(
    domain_name: &str,
    connection_generation: u64,
    state: codec::ThinkTermSessionState,
) {
    let sink = *THINKTERM_SESSION_SINK.lock().unwrap();
    if let Some(sink) = sink {
        sink(domain_name, connection_generation, state);
    }
}

impl ClientDomain {
    /// Whether this domain is the session server of the machine the GUI
    /// runs on: its panes are local terminals kept out of process, not a
    /// remote host, and the GUI treats them as local everywhere it would
    /// otherwise treat a client domain as remote.
    pub fn is_local_session_host(&self) -> bool {
        self.config.is_local_session_host()
    }

    pub fn client_domain_config(&self) -> &ClientDomainConfig {
        &self.config
    }

    /// The command to ask the server for. The session server of this
    /// machine builds its commands in its own process, where the shell the
    /// user chose in the GUI is not known: a spawn that asks for the
    /// default program names that shell explicitly.
    fn command_spec_for(&self, command: Option<&CommandBuilder>) -> Option<CommandSpec> {
        let wants_default_prog = command.map_or(true, CommandBuilder::is_default_prog);
        let chosen = if self.is_local_session_host() && wants_default_prog {
            mux::default_prog::preferred_argv()
        } else {
            None
        };
        let Some(argv) = chosen else {
            return command.map(CommandSpec::from_command_builder);
        };
        // Applied the way the in-process domain applies it: a bare shell
        // travels as `SHELL`, so the server still runs it as the login
        // shell; a shell with arguments is taken literally.
        let mut cmd = command
            .cloned()
            .unwrap_or_else(CommandBuilder::new_default_prog);
        match mux::default_prog::shell_application(&argv, cfg!(windows)) {
            Some(mux::default_prog::ShellApplication::ShellEnv(shell)) => {
                // The builder's base environment already carries the login
                // SHELL; the choice replaces it.
                cmd.env("SHELL", shell);
            }
            Some(mux::default_prog::ShellApplication::Argv(argv)) => {
                *cmd.get_argv_mut() = argv.into_iter().map(Into::into).collect();
            }
            None => {}
        }
        Some(CommandSpec::from_command_builder(&cmd))
    }

    /// The unix socket this domain connects to, if it is a unix domain
    /// that connects to a socket at all. A mux server uses it to tell a
    /// client domain that leads back to its own socket from one that leads
    /// elsewhere. A domain that goes through a proxy command leads
    /// wherever the proxy does; its configured socket path is not used and
    /// defaults to the very path a server listens on.
    pub fn unix_socket_path(&self) -> Option<std::path::PathBuf> {
        match &self.config {
            ClientDomainConfig::Unix(unix) if unix.proxy_command.is_none() => {
                Some(unix.socket_path())
            }
            _ => None,
        }
    }

    /// Whether a PDU from connection `generation` is for this domain as it
    /// stands. While no inner exists (the first attach is in flight) there
    /// is no generation to hold it against, and what the server pushes
    /// during the handshake -- where the frontend lease stands -- is the
    /// state the attach is waiting to keep.
    pub fn accepts_connection_generation(&self, generation: u64) -> bool {
        match self.connection_generation() {
            Some(current) => current == generation,
            None => true,
        }
    }

    pub fn new(config: ClientDomainConfig) -> Self {
        let local_domain_id = alloc_domain_id();
        let label = config.label();
        Mux::get().subscribe(move |notif| mux_notify_client_domain(local_domain_id, notif));
        Self {
            config,
            label,
            inner: Mutex::new(None),
            early_remote_state: Mutex::new(EarlyRemoteState::default()),
            attaching: std::sync::atomic::AtomicBool::new(false),
            attach_retries: std::sync::atomic::AtomicUsize::new(0),
            resync_coordinator: Mutex::new(ResyncCoordinatorState::default()),
            local_domain_id,
        }
    }

    /// The distro id the remote mux reported at handshake. `None` until the
    /// domain is attached, or when the server is not on a machine with an
    /// `/etc/os-release`.
    pub fn remote_os_release(&self) -> Option<String> {
        self.inner()?.client.remote_os_release()
    }

    pub(crate) fn inner(&self) -> Option<Arc<ClientInner>> {
        self.inner.lock().unwrap().as_ref().map(Arc::clone)
    }

    pub fn connect_automatically(&self) -> bool {
        self.config.connect_automatically()
    }

    /// Return the SSH transport configuration for callers that need to open a
    /// separate, non-mux SSH channel (for example, ThinkTerm's opt-in SFTP
    /// file browser).  Unix and TLS client domains intentionally return None.
    ///
    /// The clone keeps `ClientDomain`'s live transport private: consumers
    /// cannot accidentally share the mux Session and stall terminal traffic.
    pub fn ssh_domain_config(&self) -> Option<SshDomain> {
        match &self.config {
            ClientDomainConfig::Ssh(ssh) => Some(ssh.clone()),
            ClientDomainConfig::Unix(_) | ClientDomainConfig::Tls(_) => None,
        }
    }

    /// The transport died and the background reconnect loop is trying to
    /// get back; authoritative connection-health signal for indicators
    /// (pane tardiness only trips after something is sent on the pane).
    pub fn is_reconnecting(&self) -> bool {
        self.inner()
            .map_or(false, |inner| inner.client.is_reconnecting())
    }

    pub fn automatic_remote_pane_resize(&self, pane_is_tardy: bool) -> AutomaticRemotePaneResize {
        let Some(inner) = self.inner() else {
            return AutomaticRemotePaneResize::OnRelease;
        };
        let choice = inner.viewport_latency.lock().unwrap().choose(
            Instant::now(),
            inner.client.connection_phase() == ClientConnectionPhase::Ready,
            pane_is_tardy,
        );
        choice
    }

    /// Automatic reconnection failed for long enough that the retry loop
    /// parked itself; nothing is torn down, and resume_reconnect() starts
    /// another round.
    pub fn is_reconnect_suspended(&self) -> bool {
        self.inner()
            .map_or(false, |inner| inner.client.reconnect_is_suspended())
    }

    pub fn resume_reconnect(&self) {
        if let Some(inner) = self.inner() {
            inner.client.resume_reconnect();
        }
    }

    /// An attach is in flight (initial connect or a manual re-attach);
    /// state() still reads Detached until it completes.
    pub fn is_attaching(&self) -> bool {
        self.attaching.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// True for the whole of an attach retry sequence, backoff gaps
    /// included; `is_attaching` is per-attempt and reads false while the
    /// engine waits between attempts.
    pub fn is_attach_retrying(&self) -> bool {
        self.attach_retries
            .load(std::sync::atomic::Ordering::SeqCst)
            > 0
    }

    pub fn perform_detach(&self) {
        log::info!("detached domain {}", self.local_domain_id);
        self.inner.lock().unwrap().take();
        let mux = Mux::get();
        mux.domain_was_detached(self.local_domain_id);
    }

    pub fn remote_to_local_pane_id(&self, remote_pane_id: TabId) -> Option<TabId> {
        let inner = self.inner()?;
        inner.remote_to_local_pane_id(remote_pane_id)
    }

    /// See [`ClientInner::record_remote_agent_status`].
    pub fn record_remote_agent_status(
        &self,
        remote_pane_id: PaneId,
        status: Option<thinkterm_proto::AgentStatus>,
    ) {
        if let Some(inner) = self.inner() {
            inner.record_remote_agent_status(remote_pane_id, status);
        }
    }

    /// See [`ClientInner::record_remote_foreground_program`].
    pub fn record_remote_foreground_program(
        &self,
        remote_pane_id: PaneId,
        program: Option<thinkterm_proto::ForegroundProgram>,
    ) {
        if let Some(inner) = self.inner() {
            inner.record_remote_foreground_program(remote_pane_id, program);
        }
    }

    pub fn remote_to_local_window_id(&self, remote_window_id: WindowId) -> Option<WindowId> {
        let inner = self.inner()?;
        inner.remote_to_local_window(remote_window_id)
    }

    pub fn local_to_remote_window_id(&self, local_window_id: WindowId) -> Option<WindowId> {
        let inner = self.inner()?;
        inner.local_to_remote_window(local_window_id)
    }

    pub fn local_to_remote_tab_id(&self, local_tab_id: TabId) -> Option<TabId> {
        let inner = self.inner()?;
        inner.local_to_remote_tab(local_tab_id)
    }

    pub fn remote_to_local_tab_id(&self, remote_tab_id: TabId) -> Option<TabId> {
        let inner = self.inner()?;
        inner.remote_to_local_tab_id(remote_tab_id)
    }

    pub fn remote_server_id(&self) -> Option<String> {
        self.inner()?.client.remote_server_id()
    }

    pub fn connection_generation(&self) -> Option<u64> {
        Some(self.inner()?.client.connection_generation())
    }

    /// Record which terminal this frontend slot is actively presenting. This
    /// is local reconnect metadata only; it is not sent over the wire.
    pub fn set_frontend_recovery_intent(
        &self,
        slot: FrontendRecoverySlot,
        local_tab_id: TabId,
        viewport: &codec::ClientViewport,
    ) -> anyhow::Result<()> {
        let inner = self
            .inner()
            .ok_or_else(|| anyhow!("domain is not attached"))?;
        inner
            .local_to_remote_tab(local_tab_id)
            .ok_or_else(|| anyhow!("tab {local_tab_id} has no remote mapping"))?;
        let mux = Mux::get();
        let window_id = mux
            .window_containing_tab(local_tab_id)
            .ok_or_else(|| anyhow!("tab {local_tab_id} is not attached to a window"))?;
        let workspace = mux
            .get_window(window_id)
            .map(|window| window.get_workspace().to_string())
            .ok_or_else(|| anyhow!("window {window_id} disappeared"))?;
        inner.frontend_recovery_intents.lock().unwrap().insert(
            slot,
            ReconnectRecoveryTarget {
                slot,
                workspace,
                size: viewport.size(),
            },
        );
        Ok(())
    }

    pub fn clear_frontend_recovery_intent(&self, slot: FrontendRecoverySlot) {
        if let Some(inner) = self.inner() {
            inner
                .frontend_recovery_intents
                .lock()
                .unwrap()
                .remove(&slot);
        }
    }

    pub fn pending_frontend_recovery(
        &self,
        slot: FrontendRecoverySlot,
        local_tab_id: TabId,
    ) -> Option<u64> {
        self.inner()?.pending_frontend_recovery(slot, local_tab_id)
    }

    pub fn acknowledge_frontend_recovery(
        &self,
        slot: FrontendRecoverySlot,
        local_tab_id: TabId,
        generation: u64,
    ) -> bool {
        self.inner().is_some_and(|inner| {
            inner.acknowledge_frontend_recovery(slot, local_tab_id, generation)
        })
    }

    pub fn fail_frontend_recovery(
        &self,
        slot: FrontendRecoverySlot,
        local_tab_id: TabId,
        generation: u64,
        reason: impl Into<String>,
    ) -> bool {
        self.inner().is_some_and(|inner| {
            inner.fail_frontend_recovery(slot, local_tab_id, generation, reason.into())
        })
    }

    pub fn remote_viewport_state(&self, local_tab_id: TabId) -> Option<codec::ClientViewportState> {
        let inner = self.inner()?;
        let remote_tab_id = inner.local_to_remote_tab(local_tab_id)?;
        inner.remote_viewport_state(remote_tab_id)
    }

    pub fn owns_remote_viewport(&self, local_tab_id: TabId) -> Option<bool> {
        let inner = self.inner()?;
        let remote_tab_id = inner.local_to_remote_tab(local_tab_id)?;
        inner.owns_remote_viewport(remote_tab_id)
    }

    pub fn remote_access_state(&self) -> Option<codec::FrontendAccessState> {
        self.inner()?.remote_access_state()
    }

    pub fn has_remote_access(&self) -> Option<bool> {
        self.inner()?.has_remote_access()
    }

    pub fn remote_frontend_gate(&self) -> RemoteFrontendGate {
        if self.is_attaching() {
            return RemoteFrontendGate::Connecting;
        }
        self.inner()
            .map(|inner| inner.remote_frontend_gate())
            .unwrap_or(RemoteFrontendGate::Offline)
    }

    pub fn process_remote_access_state(&self, state: codec::FrontendAccessState) {
        log::debug!(
            "frontend access from {}: mode={:?} owner={:?} generation={} (this client: {:?})",
            self.config.name(),
            state.mode,
            state.owner.as_ref().map(|owner| (owner.hostname.as_str(), owner.pid, owner.id)),
            state.generation,
            self.inner()
                .map(|inner| (inner.client.client_id.pid, inner.client.client_id.id))
        );
        let Some(inner) = self.inner() else {
            self.early_remote_state.lock().unwrap().access = Some(state);
            return;
        };
        if !inner.update_remote_access(state.clone()) {
            return;
        }
        Mux::get().notify(MuxNotification::FrontendAccessChanged(
            mux::FrontendAccessState {
                mode: match state.mode {
                    codec::FrontendAccessMode::TmuxLatest => mux::FrontendAccessMode::TmuxLatest,
                    codec::FrontendAccessMode::Handoff => mux::FrontendAccessMode::Handoff,
                },
                owner: state.owner,
                generation: state.generation,
            },
        ));
    }

    pub fn process_remote_viewport_state(&self, state: codec::ClientViewportState) {
        let Some(inner) = self.inner() else {
            self.early_remote_state
                .lock()
                .unwrap()
                .viewports
                .push(state);
            return;
        };
        self.process_remote_access_state(state.access.clone());
        if !inner.update_remote_viewport(state.clone()) {
            return;
        }
        if let Some(local_tab_id) = inner.remote_to_local_tab_id(state.tab_id) {
            Mux::get().notify(MuxNotification::FrontendLeaseChanged(
                mux::FrontendViewportState {
                    tab_id: local_tab_id,
                    owner: state.owner,
                    canonical_size: state.canonical_size,
                    view: state.view.map(|view| mux::FrontendView {
                        scroll: view
                            .scroll
                            .into_iter()
                            .map(|entry| (entry.pane_id, entry.lines_from_bottom))
                            .collect(),
                    }),
                    generation: state.generation,
                    access: mux::FrontendAccessState {
                        mode: match state.access.mode {
                            codec::FrontendAccessMode::TmuxLatest => {
                                mux::FrontendAccessMode::TmuxLatest
                            }
                            codec::FrontendAccessMode::Handoff => mux::FrontendAccessMode::Handoff,
                        },
                        owner: state.access.owner,
                        generation: state.access.generation,
                    },
                },
            ));
        }
    }

    pub fn get_client_inner_for_domain(domain_id: DomainId) -> anyhow::Result<Arc<ClientInner>> {
        let mux = Mux::get();
        let domain = mux
            .get_domain(domain_id)
            .ok_or_else(|| anyhow!("invalid domain id {}", domain_id))?;
        let domain = domain
            .downcast_ref::<Self>()
            .ok_or_else(|| anyhow!("domain {} is not a ClientDomain", domain_id))?;

        if let Some(inner) = domain.inner() {
            Ok(inner)
        } else {
            bail!("domain has no assigned client");
        }
    }

    /// The reader in the mux may have decided to give up on one or
    /// more tabs at the time that a disconnect was detected, and
    /// it's also possible that another client connected and adjusted
    /// the set of tabs since we were connected, so we need to re-sync.
    /// Take up a connection that was re-established after an outage.
    ///
    /// This is the funnel every automatic reconnect passes through, so it has
    /// to do everything a first attach does and not only re-pull the panes.
    pub async fn reattach(
        domain_id: DomainId,
        connection_generation: u64,
        ui: ConnectionUI,
    ) -> anyhow::Result<bool> {
        let inner = Self::get_client_inner_for_domain(domain_id)?;
        if inner.client.connection_generation() != connection_generation {
            bail!("generation {connection_generation} was superseded before reattach began");
        }
        // Held for the whole reattach. The reader releases the pushes it
        // buffered during registration the moment SetClientId is answered,
        // and a replacement server sends plenty (its restore adds tabs);
        // each used to run a resync that made a local window for every
        // remote window *before* this function rebound them into the
        // windows it had retained, and then this made its own set on top:
        // duplicates nobody removed. Deferred, and run once at the end --
        // or, for a replaced server, once the frontend has confirmed the
        // recovered geometry (the barrier takes the guard over).
        let mut structure = Some(inner.begin_structure_mutation());
        let prior_server_id = inner.ready_server_id();
        let recovery_targets = inner.recovery_targets();
        inner.begin_remote_generation();
        let domain = Mux::get()
            .get_domain(domain_id)
            .ok_or_else(|| anyhow!("domain {domain_id} disappeared during reattach"))?;

        ui.output_str("Checking server version and restoring client identity\n");
        let server = match inner.client.verify_version_compat(&ui).await {
            Ok(server) => server,
            Err(err) => {
                // A version mismatch cannot be fixed by reconnecting: the
                // transport comes up fine every time and this check fails
                // every time. Tell the reconnect loop to surface it and
                // stop, instead of cycling "Reconnecting..." forever.
                if err
                    .downcast_ref::<crate::client::IncompatibleVersionError>()
                    .is_some()
                {
                    inner.client.set_fatal_connection_error(format!("{err:#}"));
                }
                return Err(err);
            }
        };
        let server_replaced =
            server_runtime_replaced(prior_server_id.as_deref(), &server.server_id);
        if inner.client.connection_generation() != connection_generation {
            bail!("generation {connection_generation} was superseded during registration");
        }
        inner.forget_lease_generations();

        // A reconnect begins a new connection generation. The revision
        // baseline, any old connection-scoped presentation overlay, and the
        // Spaces a local disconnect hid all belong to the connection that just
        // died — and this is the "next connect" the Disconnect menu item
        // promises. Announced first so the tree below, and any push that
        // overtakes it, is measured against this connection; a push that beats
        // the announcement is at worst dropped as stale and put right by that
        // same fetch.
        deliver_thinkterm_connected(domain.domain_name(), connection_generation);

        // Pull the tree exactly as a first attach does. Pushes only carry what
        // changes from now on, so without this the sidebar would keep showing
        // whatever it held when the link dropped. Ordinary RPCs remain behind
        // the registration barrier until SetClientId has been acknowledged.
        let client = domain
            .downcast_ref::<ClientDomain>()
            .ok_or_else(|| anyhow!("domain {domain_id} changed type during reattach"))?;
        let tree = inner
            .client
            .get_thinkterm_tree()
            .await
            .with_context(|| {
                format!(
                    "fetching the ThinkTerm tree after reconnecting to {}",
                    client.config.name()
                )
            })?
            .tree;
        deliver_thinkterm_tree(&client.config, tree.clone());

        // The session server of this machine has no layout of its own to
        // restore from: a replacement is empty, and the terminals the old
        // one held are gone. Their mirrors are dropped once this attach is
        // through, and the GUI rebuilds the visible Space from its layout
        // store, as it does for any window whose mux window died.
        let host_replaced = server_replaced && client.is_local_session_host();
        if host_replaced {
            log::warn!(
                "session server {} was replaced ({:?} -> {}); its terminals are gone, \
                 the visible Space is rebuilt from the local layout store",
                client.config.name(),
                prior_server_id,
                server.server_id
            );
            inner.forget_dead_runtime_bindings();
        }
        let mut restored_targets = Vec::new();
        if server_replaced && !host_replaced {
            inner.remember_recovery_targets(&recovery_targets);
            log::info!(
                "mux runtime changed from {:?} to {}; restoring {} visible workspaces",
                prior_server_id,
                server.server_id,
                recovery_targets.len()
            );
            let mut used_fallback = false;
            let mut restored_workspaces = HashMap::<String, String>::new();
            for target in &recovery_targets {
                if let Some(restored_workspace) = restored_workspaces.get(&target.workspace) {
                    restored_targets.push(ReconnectRecoveryTarget {
                        slot: target.slot,
                        workspace: restored_workspace.clone(),
                        size: target.size,
                    });
                    continue;
                }
                let preferred_thread_id = thread_id_for_workspace(&tree, &target.workspace);
                if preferred_thread_id.is_none() && used_fallback {
                    continue;
                }
                used_fallback |= preferred_thread_id.is_none();
                let response = inner
                    .client
                    .ensure_thinkterm_thread(codec::EnsureThinkTermThread {
                        preferred_thread_id,
                        size: target.size,
                    })
                    .await
                    .with_context(|| {
                        format!(
                            "restoring visible ThinkTerm workspace {} on replacement mux",
                            target.workspace
                        )
                    })?;
                restored_workspaces.insert(target.workspace.clone(), response.workspace.clone());
                restored_targets.push(ReconnectRecoveryTarget {
                    slot: target.slot,
                    workspace: response.workspace,
                    size: target.size,
                });
            }
        }

        let replacement_session = if server_replaced {
            let state = inner
                .client
                .get_thinkterm_session_state()
                .await
                .context("fetching replacement mux topology snapshot")?;
            deliver_thinkterm_session(client.config.name(), connection_generation, state.clone());
            Some(state)
        } else {
            None
        };
        let panes = inner.client.list_panes().await?;
        let live_tab_panes = remote_tab_panes(&panes.tabs);
        if server_replaced && !restored_targets.is_empty() {
            let live_workspaces = panes
                .tabs
                .iter()
                .filter_map(pane_node_workspace)
                .collect::<HashSet<_>>();
            for target in &restored_targets {
                if !live_workspaces.contains(target.workspace.as_str()) {
                    bail!(
                        "replacement mux did not materialize restored workspace {}",
                        target.workspace
                    );
                }
            }
        }
        let mut replacement = None;
        if server_replaced && !host_replaced {
            if restored_targets.is_empty() {
                bail!(
                    "replacement mux has no visible frontend workspace to restore for generation {connection_generation}"
                );
            }
            // Do not discard the old id maps until every Ensure and the first
            // authoritative ListPanes have succeeded. A failure before this
            // point must leave enough information for the retry to identify
            // the frontend's selected Thread and current size.
            replacement = Some(inner.prepare_server_replacement());
        }
        if let Err(err) =
            Self::process_pane_list(Arc::clone(&inner), panes, None, true, replacement.as_mut())
        {
            // Windows this attempt made before failing would otherwise
            // outlive it, as the ones a push-driven resync made used to.
            if let Some(replacement) = replacement.as_ref() {
                discard_replacement_windows(&inner, &replacement.created_windows);
            }
            return Err(err);
        }

        let created_windows = replacement
            .as_ref()
            .map(|state| state.created_windows.clone())
            .unwrap_or_default();
        if let Some(replacement) = replacement {
            {
                let _activity = mux::activity::Activity::new();
                let mux = Mux::get();
                for tab_id in replacement.old_tabs {
                    let Some(tab) = mux.get_tab(tab_id) else {
                        continue;
                    };
                    // Removing a tab calls Pane::kill on every pane in it,
                    // and ClientPane::kill reports the remote id it recorded
                    // -- an id from the DEAD runtime. The replacement
                    // allocates ids from scratch, so that id can name an
                    // unrelated live pane on the new server. These mirrors
                    // are being discarded, not closed: keep the server out
                    // of it.
                    //
                    // Only mirrors of a *replaced* runtime qualify. A retried
                    // replacement can leave current-runtime panes in an old
                    // tab -- process_pane_list re-adopts those same Arcs into
                    // the new topology -- and the latch is one-shot, cleared
                    // only by a kill that actually reaches the pane. Arming
                    // one of those would swallow the user's next deliberate
                    // close and leak the remote pane for the session.
                    let connected_server_id = inner.client.remote_server_id();
                    for pane in tab.iter_all_panes() {
                        if pane.domain_id() != inner.local_domain_id {
                            continue;
                        }
                        if mux.get_pane(pane.pane_id()).is_none() {
                            continue;
                        }
                        if let Some(client_pane) = pane.downcast_ref::<ClientPane>() {
                            if !client_pane.belongs_to_remote_server(connected_server_id.as_deref())
                            {
                                client_pane.ignore_next_kill();
                            }
                        }
                    }
                    mux.remove_tab(tab_id);
                }
            }
            Mux::get().prune_dead_windows();
        }

        // Before the server-replaced early return below: both reconnect
        // shapes must reconcile agent statuses, or everything that changed
        // while disconnected (agent finished, exited, started) stays wrong
        // until the pane next changes state.
        if let Err(err) = client.fetch_agent_statuses().await {
            log::warn!("failed to fetch agent statuses on reattach: {err:#}");
        }
        if let Err(err) = client.fetch_foreground_programs().await {
            log::warn!("failed to fetch foreground programs on reattach: {err:#}");
        }

        if server_replaced && !host_replaced {
            let active_remote_tabs = replacement_session
                .as_ref()
                .map(active_remote_tabs_by_workspace)
                .unwrap_or_default();
            // The old and replacement tabs briefly coexist in the retained
            // local window. Resolve both the retained local window and its
            // authoritative replacement tab only after the old mirror has
            // been removed. Passing this exact pair to the GUI is important:
            // its cached active tab can still name the removed mirror for one
            // notification turn, and merely asking "is your active tab in this
            // set?" can otherwise miss the only post-barrier geometry request.
            let mux = Mux::get();
            let mut frontend_targets = Vec::new();
            for target in &restored_targets {
                let authoritative_tab = active_remote_tabs
                    .get(&target.workspace)
                    .and_then(|remote_tab_id| inner.remote_to_local_tab_id(*remote_tab_id));
                let fallback_tab = || {
                    mux.iter_windows_in_workspace(&target.workspace)
                        .into_iter()
                        .filter_map(|window_id| mux.get_window(window_id))
                        .flat_map(|window| window.iter().cloned().collect::<Vec<_>>())
                        .find(|tab| inner.local_to_remote_tab(tab.tab_id()).is_some())
                        .map(|tab| tab.tab_id())
                };
                let Some(local_tab_id) = authoritative_tab.or_else(fallback_tab) else {
                    continue;
                };
                let Some(window_id) = mux.window_containing_tab(local_tab_id) else {
                    continue;
                };
                let Some(mut window) = mux.get_window_mut(window_id) else {
                    continue;
                };
                if window.get_workspace() != target.workspace {
                    continue;
                }
                let Some(index) = window.idx_by_id(local_tab_id) else {
                    continue;
                };
                window.save_and_then_set_active(index);
                frontend_targets.push(ThinkTermFrontendRecoveryTarget {
                    slot: target.slot,
                    window_id,
                    tab_id: local_tab_id,
                });
            }
            if frontend_targets.is_empty() {
                discard_replacement_windows(&inner, &created_windows);
                bail!(
                    "replacement mux restored no active frontend tab for generation {connection_generation}"
                );
            }
            frontend_targets
                .sort_unstable_by_key(|target| (target.slot, target.window_id, target.tab_id));
            frontend_targets.dedup();
            let recovery_targets = frontend_targets
                .iter()
                .map(|target| (target.slot, target.tab_id));
            inner.begin_frontend_recovery(
                connection_generation,
                recovery_targets,
                structure.take(),
            );
            if inner.client.connection_generation() != connection_generation {
                discard_replacement_windows(&inner, &created_windows);
                bail!("generation {connection_generation} was superseded during topology sync");
            }
            // TabAddedToWindow is emitted while process_pane_list is still
            // assembling the replacement. A fast GUI can therefore publish
            // before begin_frontend_recovery and have that ACK ignored. Make
            // one explicit post-barrier request using the replacement local
            // ids; the generation check in the frontend rejects late work.
            request_thinkterm_frontend_recovery(domain_id, connection_generation, frontend_targets);
            // The TUI uses the ordinary wake hook to break out of its blocking
            // input poll and publish its current CellGrid.
            wake_thinkterm_frontend();
            return Ok(false);
        }

        // SetClientId deliberately does not restore ownership.  Re-advertise
        // the exact geometry this frontend rendered before the outage so the
        // server can return current access state for the new live session.
        // SetClientViewport is non-claiming: if another device took over while
        // we were offline, it remains the owner.
        let reported = inner.reported_viewports_for_live_tabs();
        log::debug!(
            "re-reporting {} viewports after reconnecting to {} (live tabs {})",
            reported.len(),
            client.config.name(),
            inner.remote_to_local_tab.lock().unwrap().len()
        );
        // A recorded viewport can be out of date: a pane it names may
        // have gone while the transport was down. Replayed as it is, the
        // server refuses it every time, and treating that refusal as a
        // failed reattach made the client reconnect and replay the same
        // entry without end, until the process was restarted.
        for (remote_tab_id, viewport) in reported {
            if let Some(state) = inner
                .restore_reported_viewport(
                    remote_tab_id,
                    viewport,
                    live_tab_panes.get(&remote_tab_id),
                    client.config.name(),
                )
                .await?
            {
                client.process_remote_viewport_state(state);
            }
        }
        if !inner.remote_to_local_tab.lock().unwrap().is_empty()
            && inner.remote_access_state().is_none()
        {
            bail!(
                "the reconnected mux session did not return frontend access state; \
                 refusing to mark generation {connection_generation} ready"
            );
        }

        if inner.client.connection_generation() != connection_generation {
            bail!("generation {connection_generation} was superseded during topology sync");
        }
        inner.mark_server_recovered();
        if host_replaced {
            let sink = *LOCAL_SESSION_HOST_REPLACED_SINK.lock().unwrap();
            match sink {
                Some(sink) => sink(domain_id),
                None => inner.reap_dead_runtime_mirrors(),
            }
        }
        Ok(true)
    }

    fn enqueue_resync(&self, kind: ResyncRequestKind) -> ResyncTicket {
        let (ticket, start_driver, wake) = self
            .resync_coordinator
            .lock()
            .unwrap()
            .request(kind, Instant::now());
        if let Some(wake) = wake {
            let _ = wake.send(());
        }
        if start_driver {
            let domain_id = self.local_domain_id;
            promise::spawn::spawn_into_main_thread(async move {
                ClientDomain::run_resync_driver(domain_id).await
            })
            .detach();
        }
        ticket
    }

    async fn await_resync_ticket(
        &self,
        ticket: ResyncTicket,
    ) -> anyhow::Result<(SharedResyncCompletion, bool)> {
        let result = ticket
            .receiver
            .await
            .map_err(|_| anyhow!("resync coordinator stopped before completing"))?;
        let completion = result.map_err(|message| anyhow!(message.to_string()))?;
        Ok((completion, ticket.joined_existing))
    }

    async fn coordinated_resync(
        &self,
        kind: ResyncRequestKind,
    ) -> anyhow::Result<(SharedResyncCompletion, bool)> {
        let ticket = self.enqueue_resync(kind);
        self.await_resync_ticket(ticket).await
    }

    async fn run_resync_driver(domain_id: DomainId) {
        let mut guard = ResyncDriverGuard {
            domain_id,
            armed: true,
        };
        loop {
            let action = {
                let Some(mux) = Mux::try_get() else { return };
                let Some(domain) = mux.get_domain(domain_id) else {
                    return;
                };
                let Some(domain) = domain.downcast_ref::<ClientDomain>() else {
                    return;
                };
                let action = domain
                    .resync_coordinator
                    .lock()
                    .unwrap()
                    .next_action(Instant::now());
                action
            };

            match action {
                ResyncDriverAction::Stop => {
                    guard.armed = false;
                    return;
                }
                ResyncDriverAction::Wait { delay, wake } => {
                    smol::future::or(
                        async move {
                            smol::Timer::after(delay).await;
                        },
                        async move {
                            let _ = wake.await;
                        },
                    )
                    .await;
                }
                ResyncDriverAction::Run(generation) => {
                    let result = {
                        let domain = Mux::try_get().and_then(|mux| mux.get_domain(domain_id));
                        match domain {
                            Some(domain) => match domain.downcast_ref::<ClientDomain>() {
                                Some(domain) => domain.resync_once().await,
                                None => Ok(ResyncOutcome::NoClient),
                            },
                            None => Ok(ResyncOutcome::NoClient),
                        }
                    };
                    let shared = result
                        .map(|outcome| SharedResyncCompletion {
                            generation,
                            outcome,
                        })
                        .map_err(|err| Arc::<str>::from(format!("{err:#}")));
                    if let Err(err) = &shared {
                        log::warn!("resync generation {generation} failed: {err}");
                    }

                    let Some(mux) = Mux::try_get() else { return };
                    let Some(domain) = mux.get_domain(domain_id) else {
                        return;
                    };
                    let Some(domain) = domain.downcast_ref::<ClientDomain>() else {
                        return;
                    };
                    domain.resync_coordinator.lock().unwrap().finish(
                        generation,
                        shared,
                        Instant::now(),
                    );
                }
            }
        }
    }

    /// Immediate single-flight resync. Concurrent callers join the active
    /// generation instead of issuing parallel ListPanes snapshots.
    pub async fn resync(&self) -> anyhow::Result<()> {
        self.coordinated_resync(ResyncRequestKind::Immediate)
            .await?;
        Ok(())
    }

    async fn resync_after_mutation(&self) -> anyhow::Result<()> {
        self.coordinated_resync(ResyncRequestKind::MutationCatchup)
            .await?;
        Ok(())
    }

    /// Resolve a pane-scoped push without losing it behind an older in-flight
    /// snapshot. If that generation still did not materialize the pane, all
    /// such callers share exactly one fresh trailing pass. A structure
    /// mutation can deliberately defer both passes; its guard owns the later
    /// catch-up, so that existing narrow window can still leave this push
    /// unmapped.
    pub(crate) async fn resync_for_remote_pane(&self, pane_id: PaneId) -> anyhow::Result<()> {
        let (completion, joined_existing) = self
            .coordinated_resync(ResyncRequestKind::Immediate)
            .await?;
        if completion.outcome == ResyncOutcome::Applied
            && joined_existing
            && !self.remote_pane_is_materialized(pane_id)
        {
            self.coordinated_resync(ResyncRequestKind::FreshAfter(completion.generation))
                .await?;
        }
        Ok(())
    }

    fn remote_pane_is_materialized(&self, remote_pane_id: PaneId) -> bool {
        self.remote_to_local_pane_id(remote_pane_id)
            .and_then(|local_pane_id| Mux::try_get()?.get_pane(local_pane_id))
            .is_some()
    }

    /// Rate-limited trailing resync for topology-only push notifications.
    /// The coordinator serializes it with every active pass and coalesces a
    /// burst into one generation; immediate mapping recovery can wake and
    /// upgrade a pending 500ms timer.
    pub async fn resync_throttled(&self) -> anyhow::Result<()> {
        self.coordinated_resync(ResyncRequestKind::Background)
            .await?;
        Ok(())
    }

    async fn resync_once(&self) -> anyhow::Result<ResyncOutcome> {
        if let Some(inner) = self.inner() {
            // A spawn/split response is about to install the mappings for
            // the very structures this resync would otherwise see as
            // unmapped (and duplicate). Defer; the mutation's guard runs a
            // catch-up resync when it completes. Checked again after the
            // round-trip because a mutation may have started while the
            // ListPanes request was in flight.
            if inner.structure_mutation_in_flight() {
                inner.defer_resync();
                return Ok(ResyncOutcome::Deferred);
            }
            let panes = inner.client.list_panes().await?;
            if inner.structure_mutation_in_flight() {
                inner.defer_resync();
                return Ok(ResyncOutcome::Deferred);
            }
            Self::process_pane_list(inner, panes, None, false, None)?;
            // Catch-up only: steady-state updates arrive as pushed
            // AgentStatusChanged PDUs. With detection off on this side
            // there is no consumer for the answer, so skip the RPC --
            // it walks the server's whole pane list on its main thread.
            if mux::agent_status::detection_enabled() {
                if let Err(err) = self.fetch_agent_statuses().await {
                    log::warn!(
                        "failed to refresh agent statuses from {}: {err:#}",
                        self.config.name()
                    );
                }
            }
            // The same for tab icons, on their own switch.
            if mux::foreground_program::enabled() {
                if let Err(err) = self.fetch_foreground_programs().await {
                    log::warn!(
                        "failed to refresh foreground programs from {}: {err:#}",
                        self.config.name()
                    );
                }
            }
            return Ok(ResyncOutcome::Applied);
        }
        Ok(ResyncOutcome::NoClient)
    }

    /// Send mutations of the server's ThinkTerm sidebar tree, hand the
    /// authoritative result to the sink and return it to the initiating
    /// workflow.  Returning the tree lets destructive callers wait for an
    /// explicit server acknowledgement before tearing down the transport that
    /// carries the request.
    pub async fn mutate_thinkterm_tree(
        &self,
        ops: Vec<codec::TreeOp>,
    ) -> anyhow::Result<codec::ThinkTermTree> {
        let inner = self
            .inner()
            .ok_or_else(|| anyhow!("domain is not attached"))?;
        let response = inner
            .client
            .mutate_thinkterm_tree(codec::MutateThinkTermTree { ops })
            .await?;
        let tree = response.tree;
        deliver_thinkterm_tree(&self.config, tree.clone());
        Ok(tree)
    }

    /// Reconcile the mirrored agent statuses with the server's. Late-
    /// attaching clients would otherwise only learn of *future* status
    /// changes; this closes the cold-start gap on attach, resync and
    /// reconnect. The snapshot is authoritative in both directions: a
    /// mirror the response does not mention is cleared, so an agent that
    /// exited while this client was disconnected does not survive as a
    /// phantom.
    ///
    /// Ordering caveat: this snapshot can interleave with live pushes in
    /// either direction and briefly apply an older value. It converges
    /// because every server-side mutation of a published status enqueues
    /// its own trailing AgentStatusChanged on the same dispatch channel
    /// (the one deliberate exception, the PaneRemoved eviction, is always
    /// followed by the PaneRemoved PDU itself). Any future silent registry
    /// mutation without a paired PDU would make a stale snapshot permanent.
    pub async fn fetch_agent_statuses(&self) -> anyhow::Result<()> {
        let inner = self
            .inner()
            .ok_or_else(|| anyhow!("domain is not attached"))?;
        let response = inner.client.get_agent_statuses().await?;
        // Retain the snapshot keyed by *remote* pane id before any local
        // mapping is attempted: on attach this fetch typically runs while
        // the mirrors are still being materialized, and a status that
        // cannot map yet must wait for its pane (`ClientPane::new` seeds
        // from this map), not evaporate.
        let remote: std::collections::HashMap<PaneId, thinkterm_proto::AgentStatus> = response
            .statuses
            .into_iter()
            .map(|entry| (entry.pane_id, entry.status))
            .collect();
        *inner.remote_agent_statuses.lock().unwrap() = remote.clone();
        let mut desired: std::collections::HashMap<PaneId, thinkterm_proto::AgentStatus> = remote
            .into_iter()
            .filter_map(|(remote_pane, status)| {
                inner
                    .remote_to_local_pane_id(remote_pane)
                    .map(|local| (local, status))
            })
            .collect();
        let mux = Mux::get();
        let mut changed = Vec::new();
        for pane in mux.iter_panes() {
            if pane.domain_id() != self.local_domain_id {
                continue;
            }
            let Some(client_pane) = pane.downcast_ref::<ClientPane>() else {
                continue;
            };
            let target = desired.remove(&pane.pane_id());
            if pane.agent_status() != target {
                client_pane.set_agent_status(target);
                changed.push(pane.pane_id());
            }
        }
        // This runs in an ordinary spawned task, never inside a Mux
        // subscriber callback, so notifying here is safe.
        for pane_id in changed {
            Mux::notify_from_any_thread(MuxNotification::AgentStatusChanged(pane_id));
        }
        Ok(())
    }

    /// Reconcile the mirrored foreground programs with the server's, on the
    /// same terms as [`Self::fetch_agent_statuses`]: the snapshot is kept by
    /// remote pane id for mirrors not yet made, and is authoritative both
    /// ways for the ones that exist. It converges against live pushes for
    /// the same reason: every change the server publishes is followed by
    /// its own ForegroundProgramChanged on the same channel.
    pub async fn fetch_foreground_programs(&self) -> anyhow::Result<()> {
        let inner = self
            .inner()
            .ok_or_else(|| anyhow!("domain is not attached"))?;
        let response = inner.client.get_foreground_programs().await?;
        let remote: std::collections::HashMap<PaneId, thinkterm_proto::ForegroundProgram> =
            response
                .programs
                .into_iter()
                .map(|entry| (entry.pane_id, entry.program))
                .collect();
        *inner.remote_foreground_programs.lock().unwrap() = remote.clone();
        let mut desired: std::collections::HashMap<PaneId, thinkterm_proto::ForegroundProgram> =
            remote
                .into_iter()
                .filter_map(|(remote_pane, program)| {
                    inner
                        .remote_to_local_pane_id(remote_pane)
                        .map(|local| (local, program))
                })
                .collect();
        let mux = Mux::get();
        let mut changed = Vec::new();
        for pane in mux.iter_panes() {
            if pane.domain_id() != self.local_domain_id {
                continue;
            }
            let Some(client_pane) = pane.downcast_ref::<ClientPane>() else {
                continue;
            };
            let target = desired.remove(&pane.pane_id());
            if pane.foreground_program() != target {
                client_pane.set_foreground_program(target);
                changed.push(pane.pane_id());
            }
        }
        for pane_id in changed {
            Mux::notify_from_any_thread(MuxNotification::ForegroundProgramChanged(pane_id));
        }
        Ok(())
    }

    /// Pull the server's tree and hand it to the sink. Used on attach and
    /// whenever a client wants to force a resync of the sidebar structure.
    pub async fn fetch_thinkterm_tree(&self) -> anyhow::Result<()> {
        let inner = self
            .inner()
            .ok_or_else(|| anyhow!("domain is not attached"))?;
        let response = inner.client.get_thinkterm_tree().await?;
        deliver_thinkterm_tree(&self.config, response.tree);
        Ok(())
    }

    /// Pull the mux server's authoritative tree-plus-live-topology view.
    pub async fn fetch_thinkterm_session_state(
        &self,
    ) -> anyhow::Result<codec::ThinkTermSessionState> {
        let inner = self
            .inner()
            .ok_or_else(|| anyhow!("domain is not attached"))?;
        let state = inner.client.get_thinkterm_session_state().await?;
        deliver_thinkterm_session(
            self.config.name(),
            inner.client.connection_generation(),
            state.clone(),
        );
        Ok(state)
    }

    /// Ask the authoritative mux server to choose (or create) a landing
    /// Thread and ensure that its workspace contains a live terminal.  The
    /// following resync installs the remote-to-local tab and pane mappings so
    /// callers can immediately render the returned Thread.
    pub async fn ensure_thinkterm_thread(
        &self,
        preferred_thread_id: Option<codec::TtThreadId>,
        size: wezterm_term::TerminalSize,
    ) -> anyhow::Result<codec::EnsureThinkTermThreadResponse> {
        let inner = self
            .inner()
            .ok_or_else(|| anyhow!("domain is not attached"))?;
        let response = inner
            .client
            .ensure_thinkterm_thread(codec::EnsureThinkTermThread {
                preferred_thread_id,
                size,
            })
            .await?;
        self.resync().await?;
        Ok(response)
    }

    fn translate_client_viewport(
        &self,
        viewport: codec::ClientViewport,
    ) -> anyhow::Result<codec::ClientViewport> {
        Ok(match viewport {
            codec::ClientViewport::CellGrid { size } => codec::ClientViewport::CellGrid { size },
            codec::ClientViewport::Native { size, panes } => {
                let mut remote_panes = Vec::with_capacity(panes.len());
                for pane in panes {
                    let local = Mux::get()
                        .get_pane(pane.pane_id)
                        .ok_or_else(|| anyhow!("no such local pane {}", pane.pane_id))?;
                    let remote = local
                        .downcast_ref::<ClientPane>()
                        .filter(|client| client.domain_id() == self.local_domain_id)
                        .ok_or_else(|| {
                            anyhow!("pane {} is not owned by this domain", pane.pane_id)
                        })?
                        .remote_pane_id();
                    remote_panes.push(codec::ClientPaneViewport {
                        pane_id: remote,
                        size: pane.size,
                        frame: pane.frame,
                    });
                }
                codec::ClientViewport::Native {
                    size,
                    panes: remote_panes,
                }
            }
        })
    }

    /// Summarize an outgoing viewport for the `zoomtrace` log target, using
    /// the remote pane ids so a GUI record can be lined up with the mux
    /// server's `srv.viewport.recv` for the same PDU.
    fn viewport_trace(viewport: &codec::ClientViewport) -> String {
        match viewport {
            codec::ClientViewport::CellGrid { size } => {
                format!("kind=cellgrid out={}", mux::geometrytrace::size(size))
            }
            codec::ClientViewport::Native { size, panes } => format!(
                "kind=native out={} out_panes=[{}]",
                mux::geometrytrace::size(size),
                panes
                    .iter()
                    .map(|pane| format!(
                        "r{}:pty={} frame={}",
                        pane.pane_id,
                        mux::geometrytrace::size(&pane.size),
                        mux::geometrytrace::size(&pane.frame)
                    ))
                    .collect::<Vec<_>>()
                    .join(" ")
            ),
        }
    }

    fn local_tab_geometry_trace(local_tab_id: TabId) -> String {
        Mux::get()
            .get_tab(local_tab_id)
            .map(|tab| tab.geometry_trace())
            .unwrap_or_else(|| "<no local tab>".to_string())
    }

    /// Report the viewport for a locally mirrored tab to the frontend mux.
    pub async fn set_client_viewport(
        &self,
        local_tab_id: TabId,
        viewport: codec::ClientViewport,
    ) -> anyhow::Result<codec::ClientViewportState> {
        let inner = self
            .inner()
            .ok_or_else(|| anyhow!("domain is not attached"))?;
        let remote_tab_id = inner
            .local_to_remote_tab(local_tab_id)
            .ok_or_else(|| anyhow!("tab {local_tab_id} has no remote mapping"))?;
        let viewport = self.translate_client_viewport(viewport)?;
        let reported = viewport.clone();
        let started_at = Instant::now();
        if mux::geometrytrace::trace_enabled() {
            let summary = Self::viewport_trace(&viewport);
            let geometry = Self::local_tab_geometry_trace(local_tab_id);
            mux::zoom_trace!(
                "gui.viewport.send rpc=set tab={local_tab_id}/r{remote_tab_id} gen={} \
                 {summary} | {geometry}",
                inner.client.connection_generation()
            );
        }
        let result = inner
            .client
            .set_client_viewport(codec::SetClientViewport {
                tab_id: remote_tab_id,
                viewport,
            })
            .await;
        mux::zoom_trace!(
            "gui.viewport.ack rpc=set tab={local_tab_id}/r{remote_tab_id} ok={} elapsed={:?}",
            result.is_ok(),
            started_at.elapsed()
        );
        let state = match result {
            Ok(state) => {
                inner
                    .viewport_latency
                    .lock()
                    .unwrap()
                    .record_success(started_at.elapsed());
                state
            }
            Err(err) => {
                inner
                    .viewport_latency
                    .lock()
                    .unwrap()
                    .record_failure(Instant::now());
                return Err(err);
            }
        };
        inner.remember_reported_viewport(remote_tab_id, reported);
        self.process_remote_viewport_state(state.clone());
        Ok(state)
    }

    /// Offer what this renderer is looking at, for other renderers on the same
    /// tab to follow. The server ignores it unless this client owns the tab's
    /// viewport, so it is safe to send whenever the view changes.
    /// `view` must already carry pane ids in the server's namespace — see
    /// `ClientPane::remote_pane_id`, which the caller has in hand while it is
    /// reading each pane's scroll position anyway.
    pub async fn set_client_view(
        &self,
        local_tab_id: TabId,
        view: codec::ClientView,
    ) -> anyhow::Result<()> {
        let inner = self
            .inner()
            .ok_or_else(|| anyhow!("domain is not attached"))?;
        let remote_tab_id = inner
            .local_to_remote_tab(local_tab_id)
            .ok_or_else(|| anyhow!("tab {local_tab_id} has no remote mapping"))?;
        inner
            .client
            .set_client_view(codec::SetClientView {
                tab_id: remote_tab_id,
                view,
            })
            .await?;
        Ok(())
    }

    /// Atomically claim the current access/layout lease and install the exact
    /// geometry used for the interaction.
    pub async fn claim_client_viewport(
        &self,
        local_tab_id: TabId,
        viewport: codec::ClientViewport,
    ) -> anyhow::Result<codec::ClientViewportState> {
        let inner = self
            .inner()
            .ok_or_else(|| anyhow!("domain is not attached"))?;
        let remote_tab_id = inner
            .local_to_remote_tab(local_tab_id)
            .ok_or_else(|| anyhow!("tab {local_tab_id} has no remote mapping"))?;
        let viewport = self.translate_client_viewport(viewport)?;
        let reported = viewport.clone();
        let started_at = Instant::now();
        if mux::geometrytrace::trace_enabled() {
            let summary = Self::viewport_trace(&viewport);
            let geometry = Self::local_tab_geometry_trace(local_tab_id);
            mux::zoom_trace!(
                "gui.viewport.send rpc=claim tab={local_tab_id}/r{remote_tab_id} gen={} \
                 {summary} | {geometry}",
                inner.client.connection_generation()
            );
        }
        let result = inner
            .client
            .claim_client_viewport(codec::ClaimClientViewport {
                tab_id: remote_tab_id,
                viewport,
            })
            .await;
        mux::zoom_trace!(
            "gui.viewport.ack rpc=claim tab={local_tab_id}/r{remote_tab_id} ok={} elapsed={:?}",
            result.is_ok(),
            started_at.elapsed()
        );
        let state = match result {
            Ok(state) => {
                inner
                    .viewport_latency
                    .lock()
                    .unwrap()
                    .record_success(started_at.elapsed());
                state
            }
            Err(err) => {
                inner
                    .viewport_latency
                    .lock()
                    .unwrap()
                    .record_failure(Instant::now());
                return Err(err);
            }
        };
        inner.remember_reported_viewport(remote_tab_id, reported);
        self.process_remote_viewport_state(state.clone());
        Ok(state)
    }

    pub async fn set_frontend_access_mode(
        &self,
        local_tab_id: TabId,
        mode: codec::FrontendAccessMode,
        viewport: codec::ClientViewport,
    ) -> anyhow::Result<codec::FrontendAccessState> {
        let inner = self
            .inner()
            .ok_or_else(|| anyhow!("domain is not attached"))?;
        let remote_tab_id = inner
            .local_to_remote_tab(local_tab_id)
            .ok_or_else(|| anyhow!("tab {local_tab_id} has no remote mapping"))?;
        let viewport = self.translate_client_viewport(viewport)?;
        let reported = viewport.clone();
        let state = inner
            .client
            .set_frontend_access_mode(codec::SetFrontendAccessMode {
                mode,
                tab_id: remote_tab_id,
                viewport,
            })
            .await?;
        inner.remember_reported_viewport(remote_tab_id, reported);
        self.process_remote_access_state(state.clone());
        Ok(state)
    }

    pub fn process_remote_window_title_change(&self, remote_window_id: WindowId, title: String) {
        if let Some(inner) = self.inner() {
            if let Some(local_window_id) = inner.remote_to_local_window(remote_window_id) {
                if let Some(mut window) = Mux::get().get_window_mut(local_window_id) {
                    window.set_title(&title);
                }
            }
        }
    }

    pub fn process_remote_tab_title_change(&self, remote_tab_id: TabId, title: String) {
        if let Some(inner) = self.inner() {
            if let Some(local_tab_id) = inner.remote_to_local_tab_id(remote_tab_id) {
                if let Some(tab) = Mux::get().get_tab(local_tab_id) {
                    tab.set_title(&title);
                }
            }
        }
    }

    fn process_pane_list(
        inner: Arc<ClientInner>,
        panes: ListPanesResponse,
        mut primary_window_id: Option<WindowId>,
        resend_palette: bool,
        mut replacement: Option<&mut ServerReplacementMirrors>,
    ) -> anyhow::Result<()> {
        let mux = Mux::get();
        // A native/mux window can disappear while an attach or structural RPC
        // is in flight. Never trust mappings retained by the prior resync:
        // remove entries whose local objects no longer exist before using
        // them below.
        inner.expire_stale_mappings();
        log::debug!(
            "domain {}: ListPanes result {:#?}",
            inner.local_domain_id,
            panes
        );

        // "Mark" the current set of known remote ids, so that we can "Sweep"
        // any unreferenced ids at the bottom, garbage collection style
        let mut remote_windows_to_forget: HashSet<WindowId> = inner
            .remote_to_local_window
            .lock()
            .unwrap()
            .keys()
            .copied()
            .collect();
        let mut remote_tabs_to_forget: HashSet<WindowId> = inner
            .remote_to_local_tab
            .lock()
            .unwrap()
            .keys()
            .copied()
            .collect();
        let mut remote_panes_to_forget: HashSet<WindowId> = inner
            .remote_to_local_pane
            .lock()
            .unwrap()
            .keys()
            .copied()
            .collect();

        // How many mirror panes the authoritative response actually
        // described. The stale-mirror reap below refuses to act on an
        // empty answer: a half-started server must not condemn the whole
        // session.
        let mut live_mirrors_in_response = 0usize;

        // Where each local window's next tab goes: the wire lists a
        // window's tabs in its order, and a mirror keeps that order.
        let mut placed: HashMap<WindowId, usize> = HashMap::new();
        for (mut tabroot, tab_title) in panes.tabs.into_iter().zip(panes.tab_titles.iter()) {
            // Sizes off the wire build grids and layouts below. A tab listed
            // with sizes no screen could have is left as it was: not built
            // from, and not swept either, since it has not gone away.
            if !tabroot.is_plausible() {
                log::warn!(
                    "domain {}: ignoring a tab listed with impossible pane sizes",
                    inner.local_domain_id
                );
                if let Some((remote_window_id, remote_tab_id)) = tabroot.window_and_tab_ids() {
                    remote_windows_to_forget.remove(&remote_window_id);
                    remote_tabs_to_forget.remove(&remote_tab_id);
                    // Its mirror stays in the window, so it keeps its place:
                    // the tabs listed after it must not move in ahead of it.
                    if let (Some(window_id), Some(tab_id)) = (
                        inner.remote_to_local_window(remote_window_id),
                        inner.remote_to_local_tab_id(remote_tab_id),
                    ) {
                        place_listed_tab(&mux, &mut placed, window_id, tab_id);
                    }
                }
                for entry in tabroot.entries() {
                    remote_panes_to_forget.remove(&entry.pane_id);
                }
                continue;
            }
            // Translate remote stack ids into stable local ids BEFORE the
            // tree rebuild, so that GUI state keyed by pane_stack_id
            // (collapse layouts, level-2 tab bar scroll) survives resyncs.
            inner.translate_remote_stack_ids(&mut tabroot);

            let root_size = match tabroot.root_size() {
                Some(size) => size,
                None => continue,
            };

            if let Some((remote_window_id, remote_tab_id)) = tabroot.window_and_tab_ids() {
                if inner.tab_is_being_killed(&tabroot) {
                    log::debug!(
                        "domain {}: remote tab {remote_tab_id} is being killed; not mirroring it",
                        inner.local_domain_id
                    );
                    continue;
                }
                let tab;
                // For a tab we already track, the locally-held size (driven
                // by the GUI window geometry) is authoritative; the wire
                // size reflects the server's pane dimensions, which are
                // smaller than the local cells whenever the GUI reserves
                // per-pane chrome (pane nav bar). Adopting the wire size
                // here would shrink the tab by the chrome height on every
                // resync. Only brand-new tabs take the wire size, until a
                // GUI window adopts and resizes them.
                let mut sync_size = root_size;

                remote_windows_to_forget.remove(&remote_window_id);
                remote_tabs_to_forget.remove(&remote_tab_id);

                if let Some(tab_id) = inner.remote_to_local_tab_id(remote_tab_id) {
                    match mux.get_tab(tab_id) {
                        Some(t) => {
                            let local_size = t.get_size();
                            if local_size.rows > 0 && local_size.cols > 0 {
                                sync_size = local_size;
                            }
                            tab = t;
                        }
                        None => {
                            // We likely decided that we hit EOF on the tab and
                            // removed it from the mux.  Let's add it back, but
                            // with a new id.
                            log::trace!(
                                "we had remote_to_local_tab_id mapping of \
                                 {remote_tab_id} -> {tab_id}, but the local \
                                 tab is not in the mux, make a new tab"
                            );
                            inner.remove_old_tab_mapping(remote_tab_id);
                            tab = Arc::new(Tab::new(&root_size));
                            inner.record_remote_to_local_tab_mapping(remote_tab_id, tab.tab_id());
                            mux.add_tab_no_panes(&tab);
                        }
                    };
                } else {
                    tab = Arc::new(Tab::new(&root_size));
                    mux.add_tab_no_panes(&tab);
                    inner.record_remote_to_local_tab_mapping(remote_tab_id, tab.tab_id());
                }

                tab.set_title(tab_title);

                log::debug!("domain: {} tree: {:#?}", inner.local_domain_id, tabroot);
                let mut workspace = None;
                // The local window is the geometry authority only while
                // this client holds the tab; a follower draws whatever
                // splits the owner made, which only the wire carries.
                let keep_local_geometry = inner.owns_remote_viewport(remote_tab_id).unwrap_or(true);
                // A follower shows the owner's grid whole (the GUI paints
                // it at the canonical size): the wire root is its size,
                // so the sync is a no-op resize rather than a fresh
                // TabResized per push, and the splits keep their ratios.
                if !keep_local_geometry {
                    sync_size = root_size;
                }
                tab.sync_with_pane_tree_keeping(sync_size, tabroot, keep_local_geometry, |entry| {
                    workspace.replace(entry.workspace.clone());
                    remote_panes_to_forget.remove(&entry.pane_id);
                    live_mirrors_in_response += 1;
                    if let Some(pane_id) = inner.remote_to_local_pane_id(entry.pane_id) {
                        match mux.get_pane(pane_id) {
                            Some(pane) => {
                                if let Some(client_pane) = pane.downcast_ref::<ClientPane>() {
                                    client_pane.set_remote_tab_id(entry.tab_id);
                                    if resend_palette {
                                        // A reattach can land on a fresh
                                        // server process that never received
                                        // our palette, and its bare defaults
                                        // are what OSC color queries would
                                        // otherwise keep answering. Ordinary
                                        // structural resyncs do not need the
                                        // extra RPC.
                                        client_pane.resend_palette_to_server();
                                        client_pane.reconnect_kitty_frames();
                                    }
                                }
                                pane
                            }
                            None => {
                                // We likely decided that we hit EOF on the tab and
                                // removed it from the mux.  Let's add it back, but
                                // with a new id.
                                inner.remove_old_pane_mapping(entry.pane_id);
                                let pane: Arc<dyn Pane> = Arc::new(ClientPane::new(
                                    &inner,
                                    entry.tab_id,
                                    entry.pane_id,
                                    entry.size,
                                    &entry.title,
                                    entry.alt_screen,
                                ));
                                mux.add_pane(&pane).expect("failed to add pane to mux");
                                pane
                            }
                        }
                    } else {
                        let pane: Arc<dyn Pane> = Arc::new(ClientPane::new(
                            &inner,
                            entry.tab_id,
                            entry.pane_id,
                            entry.size,
                            &entry.title,
                            entry.alt_screen,
                        ));
                        log::debug!(
                            "domain: {} attaching to remote pane {:?} -> local pane_id {}",
                            inner.local_domain_id,
                            entry,
                            pane.pane_id()
                        );
                        mux.add_pane(&pane).expect("failed to add pane to mux");
                        pane
                    }
                });

                if let Some(local_window_id) = inner.remote_to_local_window(remote_window_id) {
                    log::debug!(
                        "domain: {} adding tab to existing local window {}",
                        inner.local_domain_id,
                        local_window_id
                    );
                    if let Some(window) = mux.get_window(local_window_id) {
                        let needs_add = window.idx_by_id(tab.tab_id()).is_none();
                        drop(window);
                        if needs_add {
                            // Use add_tab_to_window rather than window.push so
                            // that MuxNotification::TabAddedToWindow reaches
                            // the GUI: it relies on that event to impose the
                            // local window geometry on tabs that arrive via
                            // resync (their wire size is the server's, which
                            // sits below the local size by the pane nav bar
                            // reservation).
                            mux.add_tab_to_window(&tab, local_window_id)?;
                        }
                        place_listed_tab(&mux, &mut placed, local_window_id, tab.tab_id());
                        continue;
                    }
                    // The mapping went stale after the initial sweep. Fall
                    // through and adopt/create a live local window instead of
                    // panicking on an asynchronous resync.
                    inner
                        .remote_to_local_window
                        .lock()
                        .unwrap()
                        .remove(&remote_window_id);
                }

                if let (Some(workspace_name), Some(replacements)) =
                    (workspace.as_ref(), replacement.as_deref_mut())
                {
                    if let Some(local_window_id) =
                        replacements.windows_by_workspace.remove(workspace_name)
                    {
                        if mux.get_window(local_window_id).is_some() {
                            log::info!(
                                "rebinding fresh remote window {} into local window {} for {}",
                                remote_window_id,
                                local_window_id,
                                workspace_name
                            );
                            inner.record_remote_to_local_window_mapping(
                                remote_window_id,
                                local_window_id,
                            );
                            mux.add_tab_to_window(&tab, local_window_id)?;
                            place_listed_tab(&mux, &mut placed, local_window_id, tab.tab_id());
                            continue;
                        }
                    }
                }

                if let Some(local_window_id) = primary_window_id {
                    // Adopt the remote window into the local primary window
                    // only when the workspaces agree. Adopting on the
                    // origin-domain claim alone used to grab whichever
                    // remote window the server listed FIRST — typically the
                    // server's own startup window, or another thread's
                    // window — surfacing an unrelated old terminal as a tab
                    // and renaming its workspace on the server, merging it
                    // into the wrong thread for good.
                    let workspace_matches = {
                        let window = mux
                            .get_window(local_window_id)
                            .expect("primary window to be valid");
                        Some(window.get_workspace()) == workspace.as_deref()
                    };
                    if workspace_matches {
                        // Yes! We can use this window
                        log::debug!(
                            "adding remote window {} as tab to local window {}",
                            remote_window_id,
                            local_window_id
                        );
                        inner.record_remote_to_local_window_mapping(
                            remote_window_id,
                            local_window_id,
                        );
                        mux.add_tab_to_window(&tab, local_window_id)?;
                        place_listed_tab(&mux, &mut placed, local_window_id, tab.tab_id());
                        primary_window_id.take();
                        continue;
                    }
                }
                log::info!(
                    "making new local window for remote {} in workspace {:?}",
                    remote_window_id,
                    workspace
                );
                let position = None;
                let local_window_id = mux.new_empty_window_for_domain(
                    workspace.take(),
                    position,
                    Some(inner.local_domain_id),
                );
                if let Some(replacement) = replacement.as_deref_mut() {
                    replacement.created_windows.push(*local_window_id);
                }
                inner.record_remote_to_local_window_mapping(remote_window_id, *local_window_id);
                mux.add_tab_to_window(&tab, *local_window_id)?;
                place_listed_tab(&mux, &mut placed, *local_window_id, tab.tab_id());
            }
        }

        for (remote_window_id, window_title) in panes.window_titles {
            if let Some(local_window_id) = inner.remote_to_local_window(remote_window_id) {
                if let Some(mut window) = mux.get_window_mut(local_window_id) {
                    window.set_title(&window_title);
                } else {
                    inner
                        .remote_to_local_window
                        .lock()
                        .unwrap()
                        .remove(&remote_window_id);
                }
            }
        }

        // "Sweep" away our mapping for ids that are no longer present in the
        // latest sync
        log::debug!(
            "after sync, remote_windows_to_forget={remote_windows_to_forget:?}, \
                    remote_tabs_to_forget={remote_tabs_to_forget:?}, \
                    remote_panes_to_forget={remote_panes_to_forget:?}"
        );
        if !remote_windows_to_forget.is_empty() {
            let mut windows = inner.remote_to_local_window.lock().unwrap();
            for w in remote_windows_to_forget {
                windows.remove(&w);
            }
        }
        if !remote_tabs_to_forget.is_empty() {
            let mut tabs = inner.remote_to_local_tab.lock().unwrap();
            for t in remote_tabs_to_forget {
                tabs.remove(&t);
            }
        }
        if !remote_panes_to_forget.is_empty() {
            let mut panes = inner.remote_to_local_pane.lock().unwrap();
            for p in remote_panes_to_forget {
                panes.remove(&p);
            }
        }

        // The sweep above heals the *maps*. It cannot see a mirror that
        // lost its map entries entirely: prepare_server_replacement clears
        // every remote<->local map, so a replacement attempt that failed
        // partway leaves whole tabs with no mapping at all, permanently
        // frozen on their last frame. Identity survives on the panes
        // themselves -- ClientPane records the runtime that allocated its
        // remote id -- so reap every mirror still addressed to a previous
        // mux runtime. When the runtime did not change this collects
        // nothing, so an ordinary reconnect is untouched.
        let committed_server_id = inner.ready_server_id();
        let connected_server_id = inner.client.remote_server_id();
        // Settle the scalar gate before walking the mux: this runs on every
        // resync, and server pushes spawn one per TabResized/TabAddedToWindow.
        let doomed = if committed_server_id.is_some()
            && committed_server_id == connected_server_id
            && live_mirrors_in_response > 0
        {
            stale_mirrors_to_reap(
                mux.iter_panes().into_iter().map(|pane| {
                    let origin = pane
                        .downcast_ref::<ClientPane>()
                        .filter(|_| pane.domain_id() == inner.local_domain_id)
                        .map(|client_pane| {
                            MirrorOrigin::Mirror(
                                client_pane.created_remote_server_id().map(str::to_string),
                            )
                        })
                        .unwrap_or(MirrorOrigin::Foreign);
                    (pane.pane_id(), origin)
                }),
                committed_server_id.as_deref(),
                connected_server_id.as_deref(),
                live_mirrors_in_response,
            )
        } else {
            vec![]
        };
        if !doomed.is_empty() {
            log::info!(
                "domain {}: reaping {} mirror pane(s) {:?} addressed to a replaced \
                 mux runtime (committed runtime {:?})",
                inner.local_domain_id,
                doomed.len(),
                doomed,
                committed_server_id
            );
            {
                // Keep the frontend alive while windows are momentarily
                // empty, exactly like the replacement removal in reattach.
                let _activity = mux::activity::Activity::new();
                for pane_id in &doomed {
                    if let Some(pane) = mux.get_pane(*pane_id) {
                        if let Some(client_pane) = pane.downcast_ref::<ClientPane>() {
                            // Removal calls Pane::kill; the stale remote id
                            // could name an unrelated live pane on the new
                            // server. Discard locally, tell it nothing.
                            client_pane.ignore_next_kill();
                        }
                    }
                    mux.remove_pane(*pane_id);
                }
            }
            mux.prune_dead_windows();
            // The reap can empty a retained recovery window. Left behind,
            // its dead id survives into the next replacement attempt, whose
            // rebind path skips it (`mux.get_window` is None) and mints a
            // brand-new native window instead -- the duplicate this map
            // exists to prevent.
            inner
                .pending_recovery_windows
                .lock()
                .unwrap()
                .retain(|_workspace, window_id| mux.get_window(*window_id).is_some());
        }

        Ok(())
    }

    fn finish_attach(
        domain_id: DomainId,
        client: Client,
        panes: ListPanesResponse,
        primary_window_id: Option<WindowId>,
    ) -> anyhow::Result<()> {
        let mux = Mux::get();
        let domain = mux
            .get_domain(domain_id)
            .ok_or_else(|| anyhow!("invalid domain id {}", domain_id))?;
        let domain = domain
            .downcast_ref::<Self>()
            .ok_or_else(|| anyhow!("domain {} is not a ClientDomain", domain_id))?;
        let threshold = domain.config.local_echo_threshold_ms();
        let overlay_lag_indicator = domain.config.overlay_lag_indicator();

        // The server behind this attach may be a new process: its image
        // generations start over, and a copy kept from the old one would
        // pass for current.
        crate::pane::forget_images_for_domain(domain_id);
        let inner = Arc::new(ClientInner::new(
            domain_id,
            client,
            threshold,
            overlay_lag_indicator,
        ));
        {
            let mut guard = domain.inner.lock().unwrap();
            if guard.is_some() {
                // A concurrent attach already installed an inner. Replacing
                // it would orphan every existing local window/pane while the
                // fresh (empty) remote<->local maps re-materialize duplicates
                // of all remote windows.
                log::warn!(
                    "domain {domain_id} is already attached; dropping duplicate attach result"
                );
                return Ok(());
            }
            guard.replace(Arc::clone(&inner));
        }

        Self::process_pane_list(Arc::clone(&inner), panes, primary_window_id, false, None)?;

        // What the server said about the lease before there was an inner
        // to hold it: applied now that the tabs it names exist here.
        let early = std::mem::take(&mut *domain.early_remote_state.lock().unwrap());
        if let Some(access) = early.access {
            domain.process_remote_access_state(access);
        }
        for viewport in early.viewports {
            domain.process_remote_viewport_state(viewport);
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        accepts_generation, acknowledge_recovery_target, active_remote_tabs_by_workspace,
        attach_with_retry_loop, consistent_remote_tab_id, is_fatal_attach_error,
        next_attach_backoff, owns_remote_viewport_from_states, place_in_listed_order,
        remote_frontend_gate_from_state, remote_move_pane_id, server_runtime_replaced,
        stale_mirrors_to_reap, thread_id_for_workspace, AttachRetryOutcome, AttachRetryTiming,
        AutomaticRemotePaneResize, FrontendRecoveryAck, FrontendRecoveryBarrier,
        FrontendRecoverySlot, MirrorOrigin, RemoteFrontendGate, ResyncCoordinatorState,
        ResyncDriverAction, ResyncOutcome, ResyncRequestKind, SharedResyncCompletion,
        ViewportLatencyState,
    };
    use crate::client::ClientConnectionPhase;
    use mux::connui::ConnectionUI;
    use std::cell::Cell;
    use std::collections::HashMap;
    use std::rc::Rc;
    use std::time::{Duration, Instant};

    fn client_id(hostname: &str, id: usize) -> mux::client::ClientId {
        mux::client::ClientId {
            hostname: hostname.to_string(),
            username: "test-user".to_string(),
            pid: 42,
            epoch: 123,
            id,
            ssh_auth_sock: None,
        }
    }

    fn fast_retry_timing() -> AttachRetryTiming {
        AttachRetryTiming {
            initial_backoff: Duration::from_millis(1),
            max_backoff: Duration::from_millis(2),
            cancellation_poll: Duration::from_millis(1),
        }
    }

    fn start_resync_generation(state: &mut ResyncCoordinatorState, now: Instant) -> u64 {
        match state.next_action(now) {
            ResyncDriverAction::Run(generation) => generation,
            ResyncDriverAction::Stop => panic!("expected a queued resync, got stop"),
            ResyncDriverAction::Wait { .. } => panic!("expected an immediate resync, got timer"),
        }
    }

    fn finish_resync_generation(
        state: &mut ResyncCoordinatorState,
        generation: u64,
        outcome: ResyncOutcome,
        now: Instant,
    ) {
        state.finish(
            generation,
            Ok(SharedResyncCompletion {
                generation,
                outcome,
            }),
            now,
        );
    }

    #[test]
    fn a_tab_listed_with_impossible_sizes_keeps_its_place() {
        // Window methods notify the process-wide Mux; nothing else here uses it.
        let _ = promise::spawn::SimpleExecutor::new();
        mux::Mux::set_mux(&std::sync::Arc::new(mux::Mux::new(None)));
        let size = wezterm_term::TerminalSize::default();
        let tabs: Vec<_> = (0..3)
            .map(|_| std::sync::Arc::new(mux::tab::Tab::new(&size)))
            .collect();
        let ids: Vec<_> = tabs.iter().map(|tab| tab.tab_id()).collect();
        let mut window = mux::window::Window::new(None, None, None);
        for tab in &tabs {
            window.push(tab);
        }
        let order = |window: &mux::window::Window| {
            window.iter().map(|tab| tab.tab_id()).collect::<Vec<_>>()
        };

        // Listed A, X, B with X's sizes impossible: X is not rebuilt, but its
        // mirror still takes its turn, so B does not move in ahead of it.
        let mut slot = 0;
        for id in [ids[0], ids[1], ids[2]] {
            place_in_listed_order(&mut window, &mut slot, id);
        }
        assert_eq!(order(&window), [ids[0], ids[1], ids[2]]);

        // Listed in another order, the window follows; a tab it does not hold
        // takes no place.
        let elsewhere = mux::tab::Tab::new(&size).tab_id();
        let mut slot = 0;
        for id in [ids[2], elsewhere, ids[0], ids[1]] {
            place_in_listed_order(&mut window, &mut slot, id);
        }
        assert_eq!(slot, 3);
        assert_eq!(order(&window), [ids[2], ids[0], ids[1]]);
    }

    #[test]
    fn resync_single_flight_fans_one_completion_out_to_all_joiners() {
        let now = Instant::now();
        let mut state = ResyncCoordinatorState::default();
        let (leader, start_driver, _) = state.request(ResyncRequestKind::Immediate, now);
        assert!(start_driver);
        assert!(!leader.joined_existing);
        let generation = start_resync_generation(&mut state, now);

        let mut joiners = Vec::new();
        for _ in 0..100 {
            let (ticket, start_driver, wake) = state.request(ResyncRequestKind::Immediate, now);
            assert!(!start_driver);
            assert!(wake.is_none());
            assert!(ticket.joined_existing);
            joiners.push(ticket);
        }

        finish_resync_generation(&mut state, generation, ResyncOutcome::Applied, now);
        let leader_completion = smol::block_on(leader.receiver)
            .expect("leader sender remains live")
            .expect("resync succeeds");
        assert_eq!(leader_completion.generation, generation);
        for ticket in joiners {
            let completion = smol::block_on(ticket.receiver)
                .expect("joiner sender remains live")
                .expect("joined resync succeeds");
            assert_eq!(completion, leader_completion);
        }
    }

    #[test]
    fn missing_mapping_after_a_join_coalesces_one_fresh_generation() {
        let now = Instant::now();
        let mut state = ResyncCoordinatorState::default();
        let (_leader, _, _) = state.request(ResyncRequestKind::Immediate, now);
        let first = start_resync_generation(&mut state, now);
        let (joined, _, _) = state.request(ResyncRequestKind::Immediate, now);
        assert!(joined.joined_existing);
        finish_resync_generation(&mut state, first, ResyncOutcome::Applied, now);
        let joined_completion = smol::block_on(joined.receiver)
            .expect("joined sender remains live")
            .expect("first resync succeeds");

        let mut trailing = Vec::new();
        for _ in 0..100 {
            let (ticket, _, _) = state.request(
                ResyncRequestKind::FreshAfter(joined_completion.generation),
                now,
            );
            trailing.push(ticket);
        }
        let second = start_resync_generation(&mut state, now);
        assert_eq!(second, first + 1);
        finish_resync_generation(&mut state, second, ResyncOutcome::Applied, now);
        for ticket in trailing {
            let completion = smol::block_on(ticket.receiver)
                .expect("trailing sender remains live")
                .expect("trailing resync succeeds");
            assert_eq!(completion.generation, second);
        }
    }

    #[test]
    fn background_burst_waits_for_the_active_resync_and_runs_once() {
        let started = Instant::now();
        let mut state = ResyncCoordinatorState::default();
        let (_leader, _, _) = state.request(ResyncRequestKind::Immediate, started);
        let first = start_resync_generation(&mut state, started);
        let mut background = Vec::new();
        for _ in 0..20 {
            let (ticket, _, _) = state.request(
                ResyncRequestKind::Background,
                started + Duration::from_millis(100),
            );
            background.push(ticket);
        }
        assert_eq!(
            state.queued.map(|queued| queued.generation),
            Some(first + 1)
        );

        let completed = started + Duration::from_millis(800);
        finish_resync_generation(&mut state, first, ResyncOutcome::Applied, completed);
        let trailing = start_resync_generation(&mut state, completed);
        assert_eq!(trailing, first + 1);
        finish_resync_generation(&mut state, trailing, ResyncOutcome::Applied, completed);
        for ticket in background {
            let completion = smol::block_on(ticket.receiver)
                .expect("background sender remains live")
                .expect("trailing resync succeeds");
            assert_eq!(completion.generation, trailing);
        }
    }

    #[test]
    fn immediate_resync_wakes_and_upgrades_a_background_timer() {
        let started = Instant::now();
        let mut state = ResyncCoordinatorState::default();
        state.last_started_at = Some(started);
        let (_background, start_driver, _) = state.request(
            ResyncRequestKind::Background,
            started + Duration::from_millis(100),
        );
        assert!(start_driver);
        let wake_receiver = match state.next_action(started + Duration::from_millis(100)) {
            ResyncDriverAction::Wait { delay, wake } => {
                assert_eq!(delay, Duration::from_millis(400));
                wake
            }
            _ => panic!("background request should wait for its gap"),
        };

        let (immediate, _, wake) = state.request(
            ResyncRequestKind::Immediate,
            started + Duration::from_millis(110),
        );
        assert!(!immediate.joined_existing);
        wake.expect("the active timer is interruptible")
            .send(())
            .expect("driver still holds its receiver");
        smol::block_on(wake_receiver).expect("timer wake is delivered");
        assert!(matches!(
            state.next_action(started + Duration::from_millis(110)),
            ResyncDriverAction::Run(_)
        ));
    }

    #[test]
    fn resync_failure_releases_the_gap_and_deferred_stops_trailing_work() {
        let now = Instant::now();
        let mut failed = ResyncCoordinatorState::default();
        let (leader, _, _) = failed.request(ResyncRequestKind::Immediate, now);
        let first = start_resync_generation(&mut failed, now);
        let (joiner, _, _) = failed.request(ResyncRequestKind::Immediate, now);
        let (trailing, _, _) = failed.request(ResyncRequestKind::Background, now);
        let shared_error = std::sync::Arc::from("list panes failed");
        failed.finish(first, Err(std::sync::Arc::clone(&shared_error)), now);
        let leader_error = smol::block_on(leader.receiver)
            .expect("leader receives the failure")
            .expect_err("leader sees the RPC failure");
        let joiner_error = smol::block_on(joiner.receiver)
            .expect("joiner receives the failure")
            .expect_err("joiner sees the RPC failure");
        assert!(std::sync::Arc::ptr_eq(&leader_error, &shared_error));
        assert!(std::sync::Arc::ptr_eq(&joiner_error, &shared_error));
        assert_eq!(start_resync_generation(&mut failed, now), first + 1);
        finish_resync_generation(&mut failed, first + 1, ResyncOutcome::Applied, now);
        assert!(smol::block_on(trailing.receiver)
            .expect("trailing sender remains live")
            .is_ok());

        let mut deferred = ResyncCoordinatorState::default();
        let (leader, _, _) = deferred.request(ResyncRequestKind::Immediate, now);
        let first = start_resync_generation(&mut deferred, now);
        let (trailing, _, _) = deferred.request(ResyncRequestKind::Background, now);
        finish_resync_generation(&mut deferred, first, ResyncOutcome::Deferred, now);
        assert_eq!(
            smol::block_on(leader.receiver)
                .expect("leader sender remains live")
                .expect("defer is not an RPC error")
                .outcome,
            ResyncOutcome::Deferred
        );
        assert_eq!(
            smol::block_on(trailing.receiver)
                .expect("trailing sender is completed")
                .expect("defer is not an RPC error")
                .outcome,
            ResyncOutcome::Deferred
        );
        assert!(matches!(
            deferred.next_action(now),
            ResyncDriverAction::Stop
        ));
    }

    #[test]
    fn mutation_catchup_survives_a_deferred_active_generation() {
        let now = Instant::now();
        let mut state = ResyncCoordinatorState::default();
        let (leader, _, _) = state.request(ResyncRequestKind::Immediate, now);
        let first = start_resync_generation(&mut state, now);

        // The last mutation guard can drop after resync_once has decided to
        // defer but before the coordinator records that completion.
        let (catchup, _, _) = state.request(ResyncRequestKind::MutationCatchup, now);
        finish_resync_generation(&mut state, first, ResyncOutcome::Deferred, now);
        assert_eq!(
            smol::block_on(leader.receiver)
                .expect("leader sender remains live")
                .expect("defer is not an RPC error")
                .outcome,
            ResyncOutcome::Deferred
        );

        let second = start_resync_generation(&mut state, now);
        assert_eq!(second, first + 1);
        finish_resync_generation(&mut state, second, ResyncOutcome::Applied, now);
        assert_eq!(
            smol::block_on(catchup.receiver)
                .expect("catchup sender remains live")
                .expect("catchup resync succeeds")
                .generation,
            second
        );
    }

    #[test]
    fn attach_backoff_doubles_and_caps() {
        let maximum = Duration::from_secs(10);
        let mut delay = Duration::from_secs(1);
        let mut observed = vec![delay];
        for _ in 0..5 {
            delay = next_attach_backoff(delay, maximum);
            observed.push(delay);
        }
        assert_eq!(
            observed,
            vec![
                Duration::from_secs(1),
                Duration::from_secs(2),
                Duration::from_secs(4),
                Duration::from_secs(8),
                Duration::from_secs(10),
                Duration::from_secs(10),
            ]
        );
    }

    #[test]
    fn transient_attach_failures_retry_until_success() {
        let attempts = Rc::new(Cell::new(0));
        let attempt_counter = Rc::clone(&attempts);
        let outcome = smol::block_on(attach_with_retry_loop(
            "retry-test",
            ConnectionUI::new_headless(),
            move || {
                let attempt = attempt_counter.get() + 1;
                attempt_counter.set(attempt);
                async move {
                    if attempt < 3 {
                        Err(anyhow::anyhow!("temporary routing failure"))
                    } else {
                        Ok(())
                    }
                }
            },
            || true,
            None,
            fast_retry_timing(),
        ))
        .expect("transient failures should eventually attach");
        assert_eq!(outcome, AttachRetryOutcome::Attached);
        assert_eq!(attempts.get(), 3);
    }

    #[test]
    fn cancellation_stops_before_a_second_attempt() {
        let attempts = Rc::new(Cell::new(0));
        let attempt_counter = Rc::clone(&attempts);
        let keep_checks = Rc::new(Cell::new(0));
        let keep_counter = Rc::clone(&keep_checks);
        let outcome = smol::block_on(attach_with_retry_loop(
            "cancel-test",
            ConnectionUI::new_headless(),
            move || {
                attempt_counter.set(attempt_counter.get() + 1);
                async { Err(anyhow::anyhow!("temporary routing failure")) }
            },
            move || {
                let check = keep_counter.get();
                keep_counter.set(check + 1);
                check == 0
            },
            None,
            fast_retry_timing(),
        ))
        .expect("cancellation is not an attach error");
        assert_eq!(outcome, AttachRetryOutcome::Cancelled);
        assert_eq!(attempts.get(), 1);
    }

    #[test]
    fn retry_budget_stops_after_the_current_failure() {
        let attempts = Rc::new(Cell::new(0));
        let attempt_counter = Rc::clone(&attempts);
        let err = smol::block_on(attach_with_retry_loop(
            "budget-test",
            ConnectionUI::new_headless(),
            move || {
                attempt_counter.set(attempt_counter.get() + 1);
                async { Err(anyhow::anyhow!("still unavailable")) }
            },
            || true,
            Some(Duration::ZERO),
            fast_retry_timing(),
        ))
        .expect_err("an exhausted retry budget returns the last error");
        assert!(err.to_string().contains("still unavailable"));
        assert_eq!(attempts.get(), 1);
    }

    #[test]
    fn fatal_attach_errors_do_not_retry_even_when_wrapped() {
        let fatal_errors = [
            anyhow::Error::new(mux::ssh::AuthCancelledError).context("ssh setup"),
            anyhow::Error::new(crate::client::IncompatibleVersionError {
                version: "20990101-000000-abcdef".to_string(),
                codec_vers: 1,
            })
            .context("version check"),
            anyhow::Error::new(wezterm_ssh::HostVerificationFailed {
                remote_address: "example.test:22".to_string(),
                key: "SHA256:test".to_string(),
                file: None,
            })
            .context("host verification"),
        ];
        for err in fatal_errors {
            assert!(is_fatal_attach_error(&err), "{}", format!("{err:#}"));
        }
    }

    #[test]
    fn retry_loop_stops_after_one_fatal_attempt() {
        let attempts = Rc::new(Cell::new(0));
        let attempt_counter = Rc::clone(&attempts);
        let err = smol::block_on(attach_with_retry_loop(
            "fatal-test",
            ConnectionUI::new_headless(),
            move || {
                attempt_counter.set(attempt_counter.get() + 1);
                async { Err(anyhow::Error::new(mux::ssh::AuthCancelledError)) }
            },
            || true,
            None,
            fast_retry_timing(),
        ))
        .expect_err("fatal failures must be returned immediately");
        assert!(is_fatal_attach_error(&err));
        assert_eq!(attempts.get(), 1);
    }

    #[test]
    fn routing_and_stalled_handshake_errors_remain_transient() {
        let stalled =
            anyhow::Error::new(crate::client::VersionHandshakeStalled { timeout_secs: 60 })
                .context("Checking server version");
        assert!(!is_fatal_attach_error(&stalled));
        assert!(!is_fatal_attach_error(&anyhow::anyhow!(
            "Connection failed: No route to host (os error 65)"
        )));
    }

    fn access(
        mode: codec::FrontendAccessMode,
        owner: Option<mux::client::ClientId>,
        generation: u64,
    ) -> codec::FrontendAccessState {
        codec::FrontendAccessState {
            mode,
            owner,
            generation,
        }
    }

    fn viewport(
        tab_id: mux::tab::TabId,
        owner: Option<mux::client::ClientId>,
        access: codec::FrontendAccessState,
    ) -> codec::ClientViewportState {
        codec::ClientViewportState {
            tab_id,
            owner,
            canonical_size: wezterm_term::TerminalSize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
                dpi: 96,
            },
            view: None,
            generation: 1,
            access,
        }
    }

    #[test]
    fn equal_or_older_frontend_generations_are_ignored() {
        assert!(accepts_generation(None, 0));
        assert!(accepts_generation(Some(9), 10));
        assert!(!accepts_generation(Some(9), 9));
        assert!(!accepts_generation(Some(9), 8));
    }

    #[test]
    fn automatic_remote_resize_uses_samples_and_hysteresis() {
        let now = Instant::now();
        let mut state = ViewportLatencyState::default();
        assert_eq!(
            state.choose(now, true, false),
            AutomaticRemotePaneResize::Live
        );

        for _ in 0..3 {
            state.record_success(Duration::from_millis(50));
        }
        assert_eq!(
            state.choose(now, true, false),
            AutomaticRemotePaneResize::Live
        );

        state.record_success(Duration::from_millis(500));
        assert_eq!(
            state.choose(now, true, false),
            AutomaticRemotePaneResize::OnRelease
        );

        // The 80-140ms band retains the previous decision instead of
        // oscillating at a threshold while the connection jitters.
        state.ewma_ms = Some(110.0);
        assert_eq!(
            state.choose(now, true, false),
            AutomaticRemotePaneResize::OnRelease
        );
        state.ewma_ms = Some(70.0);
        assert_eq!(
            state.choose(now, true, false),
            AutomaticRemotePaneResize::Live
        );
    }

    #[test]
    fn automatic_remote_resize_degrades_for_failure_tardy_and_reconnect() {
        let now = Instant::now();
        let mut state = ViewportLatencyState::default();
        for _ in 0..3 {
            state.record_success(Duration::from_millis(40));
        }
        state.record_failure(now);
        assert_eq!(
            state.choose(now, true, false),
            AutomaticRemotePaneResize::OnRelease
        );

        state.degraded_until = Some(now - Duration::from_millis(1));
        state.ewma_ms = Some(40.0);
        assert_eq!(
            state.choose(now, true, false),
            AutomaticRemotePaneResize::Live
        );
        assert_eq!(
            state.choose(now, true, true),
            AutomaticRemotePaneResize::OnRelease
        );
        assert_eq!(
            state.choose(now, false, false),
            AutomaticRemotePaneResize::OnRelease
        );
    }

    #[test]
    fn handoff_ownership_uses_latest_connection_state_across_tabs() {
        let mac = client_id("mac", 1);
        let vm = client_id("vm", 2);
        let stale_tab = viewport(
            7,
            Some(mac.clone()),
            access(codec::FrontendAccessMode::Handoff, Some(mac.clone()), 1),
        );

        // The VM claimed globally while another tab was active.  This tab's
        // viewport snapshot is stale, but it must immediately become blocked.
        let vm_owns = access(codec::FrontendAccessMode::Handoff, Some(vm.clone()), 2);
        assert_eq!(
            owns_remote_viewport_from_states(&mac, Some(&vm_owns), Some(&stale_tab)),
            Some(false)
        );

        // After the Mac claims its currently selected tab, it must immediately
        // use its full local size even before that tab receives another lease
        // publication.
        let mac_owns = access(codec::FrontendAccessMode::Handoff, Some(mac.clone()), 3);
        assert_eq!(
            owns_remote_viewport_from_states(&mac, Some(&mac_owns), Some(&stale_tab)),
            Some(true)
        );
        assert_eq!(
            owns_remote_viewport_from_states(&mac, Some(&mac_owns), None),
            Some(true)
        );
    }

    #[test]
    fn collaborative_ownership_remains_per_tab() {
        let mac = client_id("mac", 1);
        let vm = client_id("vm", 2);
        let shared = access(codec::FrontendAccessMode::TmuxLatest, Some(vm.clone()), 4);
        let mac_tab = viewport(7, Some(mac.clone()), shared.clone());
        let vm_tab = viewport(8, Some(vm), shared.clone());

        assert_eq!(
            owns_remote_viewport_from_states(&mac, Some(&shared), Some(&mac_tab)),
            Some(true)
        );
        assert_eq!(
            owns_remote_viewport_from_states(&mac, Some(&shared), Some(&vm_tab)),
            Some(false)
        );
    }

    #[test]
    fn frontend_gate_separates_connection_health_from_handoff_ownership() {
        let mac = client_id("mac", 1);
        let vm = client_id("vm", 2);
        let mac_owns = access(codec::FrontendAccessMode::Handoff, Some(mac.clone()), 1);
        let vm_owns = access(codec::FrontendAccessMode::Handoff, Some(vm.clone()), 2);
        let unowned = access(codec::FrontendAccessMode::Handoff, None, 3);
        let shared = access(codec::FrontendAccessMode::TmuxLatest, Some(vm), 4);

        assert_eq!(
            remote_frontend_gate_from_state(ClientConnectionPhase::Ready, Some(&mac_owns), &mac,),
            RemoteFrontendGate::Visible
        );
        assert_eq!(
            remote_frontend_gate_from_state(ClientConnectionPhase::Ready, Some(&vm_owns), &mac,),
            RemoteFrontendGate::Claimable {
                owner: vm_owns.owner.clone(),
            }
        );
        assert_eq!(
            remote_frontend_gate_from_state(ClientConnectionPhase::Ready, Some(&unowned), &mac,),
            RemoteFrontendGate::Claimable { owner: None }
        );
        assert_eq!(
            remote_frontend_gate_from_state(ClientConnectionPhase::Ready, Some(&shared), &mac,),
            RemoteFrontendGate::Visible
        );
        assert_eq!(
            remote_frontend_gate_from_state(ClientConnectionPhase::Ready, None, &mac),
            RemoteFrontendGate::Syncing
        );

        for (phase, expected) in [
            (
                ClientConnectionPhase::Connecting,
                RemoteFrontendGate::Connecting,
            ),
            (
                ClientConnectionPhase::Registering,
                RemoteFrontendGate::Reconnecting,
            ),
            (
                ClientConnectionPhase::Reconnecting,
                RemoteFrontendGate::Reconnecting,
            ),
            (ClientConnectionPhase::Syncing, RemoteFrontendGate::Syncing),
            (
                ClientConnectionPhase::Suspended,
                RemoteFrontendGate::Offline,
            ),
            (ClientConnectionPhase::Detached, RemoteFrontendGate::Offline),
        ] {
            assert_eq!(
                remote_frontend_gate_from_state(phase, Some(&vm_owns), &mac),
                expected
            );
        }

        assert_eq!(
            RemoteFrontendGate::Reconnecting.overlay_message(),
            Some((
                "Connection lost — Reconnecting…".to_string(),
                "Terminal will resume automatically".to_string(),
            ))
        );
        assert_eq!(
            RemoteFrontendGate::Claimable { owner: None }.overlay_message(),
            Some((
                "Terminal is available".to_string(),
                "Click or scroll to take control".to_string(),
            ))
        );
    }

    #[test]
    fn remote_move_uses_the_remote_source_id() {
        assert_eq!(remote_move_pane_id(41, 901, 42, 7, 3, 3).unwrap(), 7);
    }

    #[test]
    fn remote_move_rejects_self_and_cross_domain_moves() {
        assert!(remote_move_pane_id(42, 901, 42, 7, 3, 3).is_err());
        assert!(remote_move_pane_id(41, 901, 42, 7, 3, 4).is_err());
    }

    #[test]
    fn remote_move_rejects_duplicate_local_mirrors() {
        assert!(remote_move_pane_id(41, 7, 42, 7, 3, 3).is_err());
    }

    #[test]
    fn replacement_is_compared_with_the_last_fully_recovered_runtime() {
        assert!(!server_runtime_replaced(None, "server-b"));
        assert!(!server_runtime_replaced(Some("server-b"), "server-b"));
        assert!(server_runtime_replaced(Some("server-a"), "server-b"));
        // A failed recovery must keep comparing against server-a on its next
        // attempt even though the transport has already observed server-b.
        assert!(server_runtime_replaced(Some("server-a"), "server-b"));
    }

    #[test]
    fn recovery_resolves_materialized_then_planned_thread_workspaces() {
        let tree = codec::ThinkTermTree {
            projects: vec![codec::TtProject {
                id: "project".to_string(),
                threads: vec![
                    codec::TtThread {
                        id: "materialized".to_string(),
                        planned_workspace_name: Some("planned-old".to_string()),
                        materialized_workspace_name: Some("live-workspace".to_string()),
                        ..Default::default()
                    },
                    codec::TtThread {
                        id: "planned".to_string(),
                        planned_workspace_name: Some("planned-workspace".to_string()),
                        ..Default::default()
                    },
                ],
                ..Default::default()
            }],
            ..Default::default()
        };
        assert_eq!(
            thread_id_for_workspace(&tree, "live-workspace").as_deref(),
            Some("materialized")
        );
        assert_eq!(
            thread_id_for_workspace(&tree, "planned-workspace").as_deref(),
            Some("planned")
        );
        assert_eq!(thread_id_for_workspace(&tree, "missing"), None);
    }

    #[test]
    fn recovery_uses_the_authoritative_active_top_level_tab() {
        let state = codec::ThinkTermSessionState {
            projects: vec![codec::ThinkTermSessionProject {
                threads: vec![codec::ThinkTermSessionThread {
                    materialized_workspace_name: Some("thread-workspace".to_string()),
                    tabs: vec![
                        codec::ThinkTermSessionTab {
                            tab_id: 10,
                            is_active: false,
                            ..Default::default()
                        },
                        codec::ThinkTermSessionTab {
                            tab_id: 11,
                            is_active: true,
                            ..Default::default()
                        },
                    ],
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        };
        assert_eq!(
            active_remote_tabs_by_workspace(&state).get("thread-workspace"),
            Some(&11)
        );
    }

    #[test]
    fn missing_tab_mapping_is_recoverable_only_from_consistent_live_panes() {
        assert_eq!(consistent_remote_tab_id([11, 11, 11]), Some(11));
        assert_eq!(consistent_remote_tab_id([]), None);
        assert_eq!(consistent_remote_tab_id([11, 12]), None);
    }

    #[test]
    fn replacement_barrier_waits_for_every_frontend_slot() {
        let mut barrier = Some(FrontendRecoveryBarrier::new(
            9,
            HashMap::from([
                (FrontendRecoverySlot::Window(1), 41),
                (FrontendRecoverySlot::Window(2), 42),
            ]),
            None,
        ));
        assert_eq!(
            acknowledge_recovery_target(&mut barrier, FrontendRecoverySlot::Window(1), 41, 9, 9,),
            FrontendRecoveryAck::Pending
        );
        assert!(barrier.is_some());
        assert_eq!(
            acknowledge_recovery_target(&mut barrier, FrontendRecoverySlot::Window(2), 42, 9, 9,),
            FrontendRecoveryAck::Complete
        );
        assert!(barrier.is_none());
    }

    #[test]
    fn replacement_barrier_rejects_stale_generation_and_wrong_tab() {
        let mut barrier = Some(FrontendRecoveryBarrier::new(
            9,
            HashMap::from([(FrontendRecoverySlot::Primary, 41)]),
            None,
        ));
        assert_eq!(
            acknowledge_recovery_target(&mut barrier, FrontendRecoverySlot::Primary, 42, 9, 9,),
            FrontendRecoveryAck::Ignored
        );
        assert_eq!(
            acknowledge_recovery_target(&mut barrier, FrontendRecoverySlot::Primary, 41, 8, 9,),
            FrontendRecoveryAck::Ignored
        );
        assert!(barrier.is_some());
    }

    fn mirror(server: &str) -> MirrorOrigin {
        MirrorOrigin::Mirror(Some(server.to_string()))
    }

    #[test]
    fn stale_mirrors_from_a_replaced_runtime_are_reaped() {
        // The observed zombie: prepare_server_replacement wiped every map,
        // so nothing here consults a mapping -- identity alone condemns.
        assert_eq!(
            stale_mirrors_to_reap(
                [
                    (1, mirror("srv-a")),
                    (2, mirror("srv-a")),
                    (3, mirror("srv-a"))
                ],
                Some("srv-b"),
                Some("srv-b"),
                2,
            ),
            vec![1, 2, 3]
        );
    }

    #[test]
    fn mirrors_of_the_current_runtime_are_never_reaped() {
        // An ordinary reconnect keeps the same runtime; the reap must be a
        // strict no-op there.
        assert!(stale_mirrors_to_reap(
            [(1, mirror("srv-b")), (2, mirror("srv-b"))],
            Some("srv-b"),
            Some("srv-b"),
            2,
        )
        .is_empty());
    }

    #[test]
    fn a_replacement_in_flight_condemns_nothing() {
        // Client::remote_server_id flips at version bootstrap, long before
        // the replacement topology exists, and a server push during the
        // replacement's own Ensure calls spawns a resync that lands here
        // past a connection-generation guard that already matches. Judging
        // against the connected runtime there would condemn every mirror of
        // the session being rebuilt; judging against the committed one
        // would condemn the replacement's fresh panes. Stand down instead.
        let mirrors = [(1, mirror("srv-a")), (2, mirror("srv-b"))];
        assert!(stale_mirrors_to_reap(mirrors.clone(), Some("srv-a"), Some("srv-b"), 2).is_empty());
        assert!(stale_mirrors_to_reap(mirrors, Some("srv-b"), Some("srv-a"), 2).is_empty());
    }

    #[test]
    fn foreign_and_local_panes_sharing_a_tab_survive_the_sweep() {
        assert_eq!(
            stale_mirrors_to_reap(
                [
                    (1, MirrorOrigin::Foreign),
                    (2, mirror("srv-a")),
                    (3, MirrorOrigin::Foreign)
                ],
                Some("srv-b"),
                Some("srv-b"),
                1,
            ),
            vec![2]
        );
    }

    #[test]
    fn an_empty_response_condemns_nothing() {
        // A half-started server that answered with no panes must not take
        // the whole session down with it.
        assert!(
            stale_mirrors_to_reap([(1, mirror("srv-a"))], Some("srv-b"), Some("srv-b"), 0)
                .is_empty()
        );
    }

    #[test]
    fn mirrors_with_no_recorded_runtime_are_left_alone() {
        // Absence of evidence, in either direction, never condemns.
        assert!(stale_mirrors_to_reap(
            [(1, MirrorOrigin::Mirror(None))],
            Some("srv-b"),
            Some("srv-b"),
            1
        )
        .is_empty());
        assert!(stale_mirrors_to_reap([(1, mirror("srv-a"))], None, None, 1).is_empty());
    }
}

#[async_trait(?Send)]
impl Domain for ClientDomain {
    fn domain_id(&self) -> DomainId {
        self.local_domain_id
    }

    fn domain_name(&self) -> &str {
        self.config.name()
    }

    async fn domain_label(&self) -> String {
        self.label.to_string()
    }

    async fn spawn_pane(
        &self,
        _size: TerminalSize,
        _command: Option<CommandBuilder>,
        _command_dir: Option<String>,
    ) -> anyhow::Result<Arc<dyn Pane>> {
        anyhow::bail!("spawn_pane not implemented for ClientDomain")
    }

    /// Level-2 tab in a remote pane's stack: ask the server to spawn the
    /// pane and insert it into ITS stack, then mirror it locally as a
    /// ClientPane. The caller (Mux::spawn_pane_in_stack) performs the local
    /// stack insertion; the next resync converges both sides via the stable
    /// translated stack id.
    async fn spawn_pane_in_stack(
        &self,
        base_pane_id: PaneId,
        _size: TerminalSize,
        command: Option<CommandBuilder>,
        command_dir: Option<String>,
    ) -> anyhow::Result<Arc<dyn Pane>> {
        let inner = self
            .inner()
            .ok_or_else(|| anyhow!("domain is not attached"))?;
        let _mutation = inner.begin_structure_mutation();

        let mux = Mux::get();
        let local_pane = mux
            .get_pane(base_pane_id)
            .ok_or_else(|| anyhow!("pane_id {} is invalid", base_pane_id))?;
        let pane = local_pane
            .downcast_ref::<ClientPane>()
            .ok_or_else(|| anyhow!("pane_id {} is not a ClientPane", base_pane_id))?;

        if !inner.prepare_remote_tab_input(pane.remote_tab_id()).await? {
            bail!("terminal is being operated on another device");
        }

        let result = inner
            .client
            .spawn_pane_in_stack(codec::SpawnPaneInStack {
                pane_id: pane.remote_pane_id,
                command: self.command_spec_for(command.as_ref()),
                command_dir,
                domain: SpawnTabDomain::CurrentPaneDomain,
            })
            .await?;

        if !thinkterm_proto::layout::terminal_size_is_plausible(&result.size) {
            bail!("the server described the new pane with an impossible size");
        }
        let pane: Arc<dyn Pane> = Arc::new(ClientPane::new(
            &inner,
            result.tab_id,
            result.pane_id,
            result.size,
            "thinkterm",
            false,
        ));
        mux.add_pane(&pane)?;

        Ok(pane)
    }

    async fn move_pane_to_stack(
        &self,
        source_local_pane_id: PaneId,
        target_tab_id: TabId,
        target_local_pane_id: PaneId,
    ) -> anyhow::Result<Arc<dyn Pane>> {
        let inner = self
            .inner()
            .ok_or_else(|| anyhow!("domain is not attached"))?;
        let _mutation = inner.begin_structure_mutation();
        let mux = Mux::get();

        let tab = mux
            .get_tab(target_tab_id)
            .ok_or_else(|| anyhow!("tab_id {target_tab_id} is invalid"))?;
        if tab.pane_index_for_pane(source_local_pane_id).is_none()
            || tab.pane_index_for_pane(target_local_pane_id).is_none()
        {
            bail!(
                "remote stack move requires source pane {source_local_pane_id} and \
                 target pane {target_local_pane_id} to be in tab {target_tab_id}"
            );
        }

        let source_pane = mux
            .get_pane(source_local_pane_id)
            .ok_or_else(|| anyhow!("source pane_id {source_local_pane_id} is invalid"))?;
        let target_pane = mux
            .get_pane(target_local_pane_id)
            .ok_or_else(|| anyhow!("target pane_id {target_local_pane_id} is invalid"))?;
        let source_client_pane = source_pane
            .downcast_ref::<ClientPane>()
            .ok_or_else(|| anyhow!("source pane_id {source_local_pane_id} is not a ClientPane"))?;
        let target_client_pane = target_pane
            .downcast_ref::<ClientPane>()
            .ok_or_else(|| anyhow!("target pane_id {target_local_pane_id} is not a ClientPane"))?;
        if !source_client_pane.belongs_to_client(&inner)
            || !target_client_pane.belongs_to_client(&inner)
        {
            bail!("remote stack move panes belong to different client connections");
        }

        let source_remote_pane_id = remote_move_pane_id(
            target_local_pane_id,
            target_client_pane.remote_pane_id(),
            source_local_pane_id,
            source_client_pane.remote_pane_id(),
            inner.local_domain_id,
            source_pane.domain_id(),
        )?;
        let target_remote_pane_id = target_client_pane.remote_pane_id();
        let target_remote_tab_id = target_client_pane.remote_tab_id();
        if source_client_pane.remote_tab_id() != target_remote_tab_id {
            inner.defer_resync();
            bail!(
                "remote stack move panes do not belong to the same remote tab: \
                 source={}, target={target_remote_tab_id}",
                source_client_pane.remote_tab_id()
            );
        }

        if !inner.prepare_remote_tab_input(target_remote_tab_id).await? {
            bail!("terminal is being operated on another device");
        }

        // This feature is restricted to one top-level tab. Apply the
        // already-validated move locally before the network round-trip so
        // dropping the drag preview does not reveal the old layout for one
        // RTT. A failed RPC schedules an authoritative resync, which restores
        // the server tree.
        tab.move_pane_to_stack(source_local_pane_id, target_local_pane_id)?;

        if let Err(err) = inner
            .client
            .move_pane_to_stack(codec::MovePaneToStack {
                source_pane_id: source_remote_pane_id,
                target_pane_id: target_remote_pane_id,
            })
            .await
        {
            inner.defer_resync();
            return Err(err).context("moving remote pane into stack");
        }

        source_client_pane.set_remote_tab_id(target_remote_tab_id);
        inner.defer_resync();
        Ok(source_pane)
    }

    /// Forward the request to the remote; we need to translate the local ids
    /// to those that match the remote for the request, resync the changed
    /// structure, and then translate the results back to local
    async fn move_pane_to_new_tab(
        &self,
        pane_id: PaneId,
        window_id: Option<WindowId>,
        workspace_for_new_window: Option<String>,
    ) -> anyhow::Result<Option<(Arc<Tab>, WindowId)>> {
        let inner = self
            .inner()
            .ok_or_else(|| anyhow!("domain is not attached"))?;
        let _mutation = inner.begin_structure_mutation();

        let local_pane = Mux::get()
            .get_pane(pane_id)
            .ok_or_else(|| anyhow!("pane_id {} is invalid", pane_id))?;
        let pane = local_pane
            .downcast_ref::<ClientPane>()
            .ok_or_else(|| anyhow!("pane_id {} is not a ClientPane", pane_id))?;

        if !inner.prepare_remote_tab_input(pane.remote_tab_id()).await? {
            bail!("terminal is being operated on another device");
        }

        let remote_window_id =
            window_id.and_then(|local_window| self.local_to_remote_window_id(local_window));

        let result = inner
            .client
            .move_pane_to_new_tab(codec::MovePaneToNewTab {
                pane_id: pane.remote_pane_id,
                window_id: remote_window_id,
                workspace_for_new_window,
            })
            .await?;

        self.resync().await?;

        let local_tab_id = inner
            .remote_to_local_tab_id(result.tab_id)
            .ok_or_else(|| anyhow!("remote tab {} didn't resolve after resync", result.tab_id))?;

        let local_win_id = self
            .remote_to_local_window_id(result.window_id)
            .ok_or_else(|| {
                anyhow!(
                    "remote window {} didn't resolve after resync",
                    result.window_id
                )
            })?;

        let tab = Mux::get()
            .get_tab(local_tab_id)
            .ok_or_else(|| anyhow!("local tab {local_tab_id} is invalid"))?;

        Ok(Some((tab, local_win_id)))
    }

    async fn spawn(
        &self,
        size: TerminalSize,
        command: Option<CommandBuilder>,
        command_dir: Option<String>,
        window: WindowId,
    ) -> anyhow::Result<Arc<Tab>> {
        let inner = self
            .inner()
            .ok_or_else(|| anyhow!("domain is not attached"))?;
        let _mutation = inner.begin_structure_mutation();

        // File the remote window under the workspace of the local window we
        // are spawning into, NOT the globally active workspace: with several
        // Spaces open the active workspace routinely belongs to a different
        // (even local) thread, and a remote window misfiled under that name
        // folds into the wrong window on every later attach.
        let workspace = Mux::get()
            .get_window(window)
            .map(|w| w.get_workspace().to_string())
            .unwrap_or_else(|| Mux::get().active_workspace());

        let result = inner
            .client
            .spawn_v2(SpawnV2 {
                domain: SpawnTabDomain::DefaultDomain,
                window_id: inner.local_to_remote_window(window),
                size,
                command: self.command_spec_for(command.as_ref()),
                command_dir,
                workspace: workspace.clone(),
            })
            .await?;

        inner.record_remote_to_local_window_mapping(result.window_id, window);

        let pane: Arc<dyn Pane> = Arc::new(ClientPane::new(
            &inner,
            result.tab_id,
            result.pane_id,
            size,
            "thinkterm",
            false,
        ));
        let tab = Arc::new(Tab::new(&size));
        tab.assign_pane(&pane);
        inner.remove_old_tab_mapping(result.tab_id);
        inner.record_remote_to_local_tab_mapping(result.tab_id, tab.tab_id());

        let mux = Mux::get();
        mux.add_tab_and_active_pane(&tab)?;
        // The window was empty while the server answered, and an empty
        // window is what a pane removal's prune sweeps away; the tab then
        // needs a window of its own rather than a fatal error.
        let window = if mux.get_window(window).is_some() {
            window
        } else {
            let replacement = *mux.new_empty_window_for_domain(
                Some(workspace.clone()),
                None,
                Some(self.local_domain_id),
            );
            log::warn!(
                "local window {window} for the new tab was pruned while spawning; using {replacement}"
            );
            inner.record_remote_to_local_window_mapping(result.window_id, replacement);
            replacement
        };
        mux.add_tab_to_window(&tab, window)?;

        Ok(tab)
    }

    async fn split_pane(
        &self,
        source: SplitSource,
        tab_id: TabId,
        pane_id: PaneId,
        split_request: SplitRequest,
    ) -> anyhow::Result<Arc<dyn Pane>> {
        let inner = self
            .inner()
            .ok_or_else(|| anyhow!("domain is not attached"))?;
        let _mutation = inner.begin_structure_mutation();

        let mux = Mux::get();

        let tab = mux
            .get_tab(tab_id)
            .ok_or_else(|| anyhow!("tab_id {} is invalid", tab_id))?;
        let local_pane = mux
            .get_pane(pane_id)
            .ok_or_else(|| anyhow!("pane_id {} is invalid", pane_id))?;
        let pane = local_pane
            .downcast_ref::<ClientPane>()
            .ok_or_else(|| anyhow!("pane_id {} is not a ClientPane", pane_id))?;
        if !pane.belongs_to_client(&inner) {
            bail!("pane_id {pane_id} belongs to a stale client connection");
        }
        let target_remote_pane_id = pane.remote_pane_id();
        let target_remote_tab_id = pane.remote_tab_id();

        if !inner.prepare_remote_tab_input(target_remote_tab_id).await? {
            bail!("terminal is being operated on another device");
        }

        let (command, command_dir, move_pane_id, moved_local_pane, source_tab_id) = match source {
            SplitSource::Spawn {
                command,
                command_dir,
            } => (command, command_dir, None, None, None),
            SplitSource::MovePane(source_local_pane_id) => {
                let source_pane = mux
                    .get_pane(source_local_pane_id)
                    .ok_or_else(|| anyhow!("source pane_id {source_local_pane_id} is invalid"))?;
                let source_client_pane =
                    source_pane.downcast_ref::<ClientPane>().ok_or_else(|| {
                        anyhow!("source pane_id {source_local_pane_id} is not a ClientPane")
                    })?;
                if !source_client_pane.belongs_to_client(&inner) {
                    bail!(
                        "source pane_id {source_local_pane_id} belongs to a different client connection"
                    );
                }
                let remote_source_pane_id = remote_move_pane_id(
                    pane_id,
                    target_remote_pane_id,
                    source_local_pane_id,
                    source_client_pane.remote_pane_id(),
                    inner.local_domain_id,
                    source_pane.domain_id(),
                )?;

                let target_index = tab
                    .pane_index_for_pane(pane_id)
                    .ok_or_else(|| anyhow!("pane_id {pane_id} is not in tab {tab_id}"))?;
                tab.validate_split_request(target_index, split_request)
                    .context("remote MovePane split preflight failed")?;
                let (_, _, source_tab_id) = mux
                    .resolve_pane_id(source_local_pane_id)
                    .ok_or_else(|| anyhow!("source pane_id {source_local_pane_id} is invalid"))?;

                (
                    None,
                    None,
                    Some(remote_source_pane_id),
                    Some((source_pane, remote_source_pane_id)),
                    Some(source_tab_id),
                )
            }
        };

        // Pane-tab dragging is currently restricted to one top-level tab.
        // Mirror that common case before waiting for the RPC so the old
        // layout is never exposed between the drag overlay disappearing and
        // the server response. Cross-tab callers retain the conservative
        // post-response update below.
        let optimistic_move_applied = if let Some((moved_pane, _)) = moved_local_pane.as_ref() {
            if source_tab_id == Some(tab_id) {
                mux.move_pane_to_split(moved_pane.pane_id(), tab_id, pane_id, split_request)?;
                true
            } else {
                false
            }
        } else {
            false
        };

        let result = match inner
            .client
            .split_pane(SplitPane {
                domain: SpawnTabDomain::CurrentPaneDomain,
                pane_id: target_remote_pane_id,
                split_request,
                command: self.command_spec_for(command.as_ref()),
                command_dir,
                move_pane_id,
            })
            .await
        {
            Ok(result) => result,
            Err(err) => {
                // The transport may have failed after the server committed
                // the mutation. Always fetch the authoritative tree instead
                // of assuming that an RPC error means no structural change.
                inner.defer_resync();
                return Err(err);
            }
        };

        if let Some((moved_pane, expected_remote_pane_id)) = moved_local_pane {
            // The server must return the identity of the pane that it moved.
            // Treat anything else as a protocol/topology mismatch rather
            // than creating a second local mirror for that remote pane.
            if result.pane_id != expected_remote_pane_id {
                inner.defer_resync();
                bail!(
                    "remote MovePane returned pane {}, expected {}",
                    result.pane_id,
                    expected_remote_pane_id
                );
            }
            if result.tab_id != target_remote_tab_id {
                inner.defer_resync();
                bail!(
                    "remote MovePane returned tab {}, expected {}",
                    result.tab_id,
                    target_remote_tab_id
                );
            }

            let moved_client_pane = moved_pane
                .downcast_ref::<ClientPane>()
                .expect("MovePane source was validated as ClientPane");
            moved_client_pane.set_remote_tab_id(result.tab_id);

            if !optimistic_move_applied {
                if let Err(err) =
                    mux.move_pane_to_split(moved_pane.pane_id(), tab_id, pane_id, split_request)
                {
                    // The server has already committed the move. The mux
                    // helper guarantees that the local pane remains attached;
                    // force an authoritative tree sync to converge on the
                    // remote result.
                    inner.defer_resync();
                    return Err(err).context("mirroring remote MovePane locally");
                }
            }

            // Always converge with the server tree after the optimistic
            // local update, even if its TabResized notification was delayed.
            inner.defer_resync();
            return Ok(moved_pane);
        }

        if !thinkterm_proto::layout::terminal_size_is_plausible(&result.size) {
            bail!("the server described the new pane with an impossible size");
        }
        let pane: Arc<dyn Pane> = Arc::new(ClientPane::new(
            &inner,
            result.tab_id,
            result.pane_id,
            result.size,
            "thinkterm",
            false,
        ));

        let pane_index = match tab
            .iter_panes()
            .iter()
            .find(|p| p.pane.pane_id() == pane_id)
        {
            Some(p) => p.index,
            None => anyhow::bail!("invalid pane id {}", pane_id),
        };

        if let Err(err) = tab.split_and_insert(pane_index, split_request, Arc::clone(&pane)) {
            inner.defer_resync();
            return Err(err).context("mirroring remote split locally");
        }

        mux.add_pane(&pane)?;

        Ok(pane)
    }

    async fn attach(&self, window_id: Option<WindowId>) -> anyhow::Result<()> {
        // The local session host answers over a unix socket with nothing to
        // ask; a progress tab in the window being spawned into reads as an
        // error, and closing it leaves that window with no tab. Attach it
        // silently; remote domains keep the tab for prompts and failures.
        let ui = if self.is_local_session_host() {
            log::debug!("attaching the local session host without a progress tab");
            ConnectionUI::new_headless()
        } else {
            ConnectionUI::with_params(ConnectionUIParams {
                window_id,
                ..Default::default()
            })
        };
        let outcome = self
            .attach_with_ui_retry(
                window_id,
                ui,
                move || {
                    window_id.is_none_or(|window_id| Mux::get().get_window(window_id).is_some())
                },
                Some(Duration::from_secs(60)),
            )
            .await?;
        match outcome {
            AttachRetryOutcome::Attached => Ok(()),
            AttachRetryOutcome::Cancelled => bail!("attach cancelled because its window closed"),
        }
    }

    /// The local session host is never detached: not when its last window
    /// closes (the mux would drop it, and the next window spawns into it),
    /// and not by a detach action. Closing the GUI is what ends this
    /// connection.
    fn detachable(&self) -> bool {
        !self.is_local_session_host()
    }

    fn detach(&self) -> anyhow::Result<()> {
        self.perform_detach();
        Ok(())
    }

    fn state(&self) -> DomainState {
        if self.inner.lock().unwrap().is_some() {
            DomainState::Attached
        } else {
            DomainState::Detached
        }
    }
}

impl ClientDomain {
    /// Attach with one ConnectionUI shared by every attempt. Transient
    /// failures use 1s..10s exponential backoff; fatal failures return
    /// immediately. `None` retries until `keep_going` cancels the operation.
    pub async fn attach_with_ui_retry(
        &self,
        window_id: Option<WindowId>,
        ui: ConnectionUI,
        keep_going: impl FnMut() -> bool,
        max_total: Option<Duration>,
    ) -> anyhow::Result<AttachRetryOutcome> {
        struct RetryFlag<'a>(&'a std::sync::atomic::AtomicUsize);
        impl Drop for RetryFlag<'_> {
            fn drop(&mut self) {
                self.0.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            }
        }
        self.attach_retries
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let _in_flight = RetryFlag(&self.attach_retries);

        let attempt_ui = ui.clone();
        attach_with_retry_loop(
            self.domain_name(),
            ui,
            move || self.attach_with_ui(window_id, attempt_ui.clone()),
            keep_going,
            max_total,
            AttachRetryTiming::default(),
        )
        .await
    }

    /// The body of Domain::attach, with a caller-supplied ConnectionUI so
    /// that a retrying caller can funnel every attempt into one UI tab
    /// instead of leaving a dead tab behind per attempt. On failure the UI
    /// is left open (the caller either retries into it or lets it linger to
    /// show the error); on success it is closed.
    pub async fn attach_with_ui(
        &self,
        window_id: Option<WindowId>,
        ui: ConnectionUI,
    ) -> anyhow::Result<()> {
        use std::sync::atomic::Ordering;

        if self.state() == DomainState::Attached {
            // Already attached
            ui.close();
            return Ok(());
        }

        // Connecting takes seconds (ssh handshake, auth, pane list) while
        // state() still reads Detached. A second attach started in that
        // window would run to finish_attach and replace the first one's
        // ClientInner with a fresh one whose remote<->local maps are empty,
        // duplicating every remote window as a second local mirror. Only one
        // attach may run; latecomers wait for its outcome.
        if self.attaching.swap(true, Ordering::SeqCst) {
            while self.attaching.load(Ordering::SeqCst) {
                smol::Timer::after(std::time::Duration::from_millis(100)).await;
            }
            if self.state() == DomainState::Attached {
                ui.close();
                return Ok(());
            }
            anyhow::bail!("a concurrent attach attempt for this domain failed");
        }

        let result = self.attach_with_ui_impl(window_id, ui).await;
        self.attaching.store(false, Ordering::SeqCst);
        result
    }

    async fn attach_with_ui_impl(
        &self,
        window_id: Option<WindowId>,
        ui: ConnectionUI,
    ) -> anyhow::Result<()> {
        let domain_id = self.local_domain_id;
        let config = self.config.clone();

        let activity = mux::activity::Activity::new();
        ui.title("ThinkTerm: Connecting...");

        ui.async_run_and_log_error({
            let ui = ui.clone();
            async move {
                let connect = |config: ClientDomainConfig, ui: ConnectionUI| {
                    spawn_into_new_thread(move || {
                        let mut cloned_ui = ui;
                        match &config {
                            ClientDomainConfig::Unix(unix) => {
                                let initial = true;
                                let no_auto_start = false;
                                Client::new_unix_domain(
                                    Some(domain_id),
                                    unix,
                                    initial,
                                    &mut cloned_ui,
                                    no_auto_start,
                                )
                            }
                            ClientDomainConfig::Tls(tls) => {
                                Client::new_tls(domain_id, tls, &mut cloned_ui)
                            }
                            ClientDomainConfig::Ssh(ssh) => {
                                Client::new_ssh(domain_id, ssh, &mut cloned_ui)
                            }
                        }
                    })
                };
                let mut client = connect(config.clone(), ui.clone()).await?;

                ui.output_str("Checking server version\n");
                let verified = client.verify_version_compat(&ui).await;
                // The session server of this machine is brought to this
                // client's build whenever the two differ, codec mismatch or
                // not: the running one hands its panes over and exits. A
                // failed takeover leaves a compatible server serving and
                // an incompatible one refused as before.
                if let (ClientDomainConfig::Unix(unix), true) =
                    (&config, config.is_local_session_host())
                {
                    let needs_takeover = match &verified {
                        Ok(info) => info.version_string != config::wezterm_version(),
                        Err(err)
                            if err
                                .downcast_ref::<crate::client::IncompatibleVersionError>()
                                .is_some() =>
                        {
                            true
                        }
                        // A stalled handshake or a refused registration is
                        // not something a takeover repairs.
                        Err(_) => return Err(verified.unwrap_err()),
                    };
                    if needs_takeover && !cfg!(unix) {
                        // No handoff on this platform (it passes descriptors).
                        // A server that still speaks our codec keeps serving;
                        // one that does not is stopped, and the reconnect
                        // starts this build's server. Its terminals end, which
                        // is what an update without a handoff means here.
                        match &verified {
                            Ok(info) => log::info!(
                                "the session server runs ThinkTerm {} and this is {}; it keeps \
                                 serving, since this platform has no in-place handoff",
                                info.version_string,
                                config::wezterm_version()
                            ),
                            Err(err) => {
                                ui.output_str(&format!(
                                    "The running session server cannot be used ({err:#}) and this \
                                     platform cannot hand its sessions over; stopping it and \
                                     starting this build's server. Its terminals end.\n"
                                ));
                                let pid_file = config::configuration().daemon_options.pid_file();
                                let socket = unix.socket_path();
                                let outcome = spawn_into_new_thread(move || {
                                    mux::session_server::stop(
                                        &pid_file,
                                        &socket,
                                        std::time::Duration::from_secs(10),
                                    )
                                })
                                .await;
                                match outcome {
                                    Ok(mux::session_server::StopOutcome::NotRunning) => {
                                        // A server left by a build that wrote no pid
                                        // file cannot be found; only a person can
                                        // stop that one.
                                        ui.output_str(
                                            "Nothing holds the session server's pid file, so it \
                                             cannot be stopped from here: stop thinkterm-mux-server \
                                             by hand, then start ThinkTerm again.\n",
                                        );
                                        verified?;
                                    }
                                    Ok(_) => {
                                        ui.output_str("Reconnecting to a fresh server\n");
                                        *self.early_remote_state.lock().unwrap() =
                                            EarlyRemoteState::default();
                                        client = connect(config.clone(), ui.clone()).await?;
                                        client.verify_version_compat(&ui).await?;
                                    }
                                    Err(stop_err) => {
                                        ui.output_str(&format!(
                                            "Could not stop the session server: {stop_err:#}\n"
                                        ));
                                        verified?;
                                    }
                                }
                            }
                        }
                    } else if needs_takeover {
                        let outcome = {
                            let unix = unix.clone();
                            let ui = ui.clone();
                            spawn_into_new_thread(move || {
                                crate::local_update::take_over_local_server(&unix, &ui)
                            })
                            .await
                        };
                        match outcome {
                            Ok(true) => {
                                ui.output_str("Reconnecting to the updated server\n");
                                // The lease state stashed from the old server
                                // carries its generations; the successor's
                                // start over.
                                *self.early_remote_state.lock().unwrap() =
                                    EarlyRemoteState::default();
                                client = connect(config.clone(), ui.clone()).await?;
                                client.verify_version_compat(&ui).await?;
                            }
                            Ok(false) => {
                                verified?;
                            }
                            Err(takeover_err) => {
                                ui.output_str(&format!("Local update failed: {takeover_err:#}\n"));
                                verified?;
                            }
                        }
                    }
                } else if let Err(err) = verified {
                    // A codec mismatch over ssh is the one connect failure
                    // this side can repair: the installer runs over the same
                    // hop, at this client's version. Anything else, and a
                    // declined offer, keeps the original error.
                    let mismatch = err.downcast_ref::<crate::client::IncompatibleVersionError>();
                    let (ClientDomainConfig::Ssh(ssh), Some(mismatch)) = (&config, mismatch) else {
                        return Err(err);
                    };
                    let outcome = {
                        let ssh = ssh.clone();
                        let ui = ui.clone();
                        let mismatch = mismatch.clone();
                        spawn_into_new_thread(move || {
                            crate::remote_update::offer_remote_update(&ssh, &ui, &mismatch)
                        })
                        .await
                    };
                    match outcome {
                        Ok(crate::remote_update::RemoteUpdateOutcome::Updated {
                            restarted: true,
                        }) => {
                            ui.output_str("Reconnecting to the updated server\n");
                            client = connect(config.clone(), ui.clone()).await?;
                            client.verify_version_compat(&ui).await?;
                        }
                        Ok(_) => return Err(err),
                        Err(update_err) => {
                            ui.output_str(&format!("Remote update failed: {update_err:#}\n"));
                            return Err(err);
                        }
                    }
                }

                ui.output_str("Version check OK!  Requesting pane list...\n");
                let panes = client.list_panes().await?;
                ui.output_str(&format!(
                    "Server has {} tabs.  Attaching to local UI...\n",
                    panes.tabs.len()
                ));
                ClientDomain::finish_attach(domain_id, client, panes, window_id)
            }
        })
        .await
        .map_err(|e| {
            ui.output_str(&format!("Error during attach: {:#}\n", e));
            e
        })?;

        // Announce the new connection before asking it anything, so that the
        // answer — and any push that overtakes it — is measured against this
        // connection rather than the last one.
        let connection_generation = self
            .connection_generation()
            .ok_or_else(|| anyhow!("domain detached immediately after attach"))?;
        deliver_thinkterm_connected(self.config.name(), connection_generation);

        // The sidebar structure lives on the server. Fetching it is not worth
        // failing an otherwise-good attach over: without it the Space simply
        // shows no rows until the next push or reconnect. For the local
        // session host the fetch is what starts the mirror of the local Spaces.
        if let Err(err) = self.fetch_thinkterm_tree().await {
            log::warn!(
                "failed to fetch the ThinkTerm tree from {}: {err:#}",
                self.config.name()
            );
        }

        // Same failure policy: agent statuses are a nicety, not worth
        // failing an attach over; the push path catches us up on the
        // next state change regardless.
        if let Err(err) = self.fetch_agent_statuses().await {
            log::warn!(
                "failed to fetch agent statuses from {}: {err:#}",
                self.config.name()
            );
        }
        if let Err(err) = self.fetch_foreground_programs().await {
            log::warn!(
                "failed to fetch foreground programs from {}: {err:#}",
                self.config.name()
            );
        }

        if let Some(inner) = self.inner() {
            inner.client.mark_ready();
        }

        ui.output_str("Attached!\n");
        drop(activity);
        ui.close();
        Ok(())
    }
}

#[cfg(test)]
mod reattach_viewport_tests {
    use super::{reattach_viewport_fallback, remote_tab_panes};
    use mux::pane::PaneId;
    use mux::tab::{PaneEntry, PaneNode, PaneStackEntry, SplitDirection, SplitDirectionAndSize};
    use mux::renderable::StableCursorPosition;
    use std::collections::HashSet;
    use wezterm_term::TerminalSize;

    fn size(rows: usize, cols: usize) -> TerminalSize {
        TerminalSize {
            rows,
            cols,
            pixel_width: cols * 8,
            pixel_height: rows * 16,
            dpi: 96,
        }
    }

    fn entry(tab_id: usize, pane_id: PaneId) -> PaneEntry {
        PaneEntry {
            window_id: 1,
            tab_id,
            pane_id,
            title: format!("pane {pane_id}"),
            size: size(24, 80),
            working_dir: None,
            is_active_pane: false,
            is_zoomed_pane: false,
            alt_screen: false,
            workspace: "default".to_string(),
            cursor_pos: StableCursorPosition::default(),
            physical_top: 0,
            top_row: 0,
            left_col: 0,
            tty_name: None,
        }
    }

    fn pane_viewport(pane_id: PaneId) -> codec::ClientPaneViewport {
        codec::ClientPaneViewport {
            pane_id,
            size: size(24, 40),
            frame: size(24, 40),
        }
    }

    #[test]
    fn remote_tab_panes_groups_leaves_splits_and_stacks_by_tab() {
        let tabs = vec![
            PaneNode::Split {
                left: Box::new(PaneNode::Leaf(entry(6, 9))),
                right: Box::new(PaneNode::Stack(PaneStackEntry {
                    active: 0,
                    panes: vec![entry(6, 12), entry(6, 13)],
                    pane_stack_id: None,
                })),
                node: SplitDirectionAndSize {
                    direction: SplitDirection::Horizontal,
                    first: size(24, 40),
                    second: size(24, 40),
                },
            },
            PaneNode::Leaf(entry(7, 20)),
            PaneNode::Empty,
        ];
        let panes = remote_tab_panes(&tabs);
        assert_eq!(panes.len(), 2);
        assert_eq!(panes[&6], HashSet::from([9, 12, 13]));
        assert_eq!(panes[&7], HashSet::from([20]));
    }

    #[test]
    fn a_viewport_whose_panes_are_all_present_is_replayed_as_it_is() {
        let viewport = codec::ClientViewport::Native {
            size: size(24, 80),
            panes: vec![pane_viewport(9), pane_viewport(12)],
        };
        assert_eq!(
            reattach_viewport_fallback(&viewport, &HashSet::from([9, 12, 13])),
            None
        );
    }

    #[test]
    fn a_viewport_naming_a_missing_pane_falls_back_to_the_tab_size_alone() {
        // The shape of the 2026-09-16 outage: pane 9 left tab 6 while the
        // transport was down and the recorded viewport still named it.
        let viewport = codec::ClientViewport::Native {
            size: size(50, 200),
            panes: vec![pane_viewport(9), pane_viewport(12)],
        };
        assert_eq!(
            reattach_viewport_fallback(&viewport, &HashSet::from([12, 13])),
            Some(codec::ClientViewport::Native {
                size: size(50, 200),
                panes: Vec::new(),
            })
        );
    }

    #[test]
    fn paneless_and_cell_grid_viewports_never_need_a_fallback() {
        let empty = HashSet::new();
        let paneless = codec::ClientViewport::Native {
            size: size(24, 80),
            panes: Vec::new(),
        };
        assert_eq!(reattach_viewport_fallback(&paneless, &empty), None);
        let grid = codec::ClientViewport::CellGrid { size: size(24, 80) };
        assert_eq!(reattach_viewport_fallback(&grid, &empty), None);
    }
}
