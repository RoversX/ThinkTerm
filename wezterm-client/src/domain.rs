use crate::client::{Client, ClientConnectionPhase};
use crate::pane::ClientPane;
use anyhow::{anyhow, bail, Context};
use async_trait::async_trait;
use codec::{ListPanesResponse, SpawnV2, SplitPane};
use config::keyassignment::SpawnTabDomain;
use config::{SshDomain, TlsDomainClient, UnixDomain};
use mux::connui::{ConnectionUI, ConnectionUIParams};
use mux::domain::{alloc_domain_id, Domain, DomainId, DomainState, SplitSource};
use mux::pane::{Pane, PaneId};
use mux::tab::{SplitRequest, Tab, TabId};
use mux::window::WindowId;
use mux::{Mux, MuxNotification};
use portable_pty::CommandBuilder;
use promise::spawn::spawn_into_new_thread;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use wezterm_term::TerminalSize;

fn accepts_generation(prior: Option<u64>, incoming: u64) -> bool {
    prior.is_none_or(|prior| incoming > prior)
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
    /// Authoritative per-remote-tab viewport ownership pushed by the server.
    remote_viewports: Mutex<HashMap<TabId, codec::ClientViewportState>>,
    /// Connection-wide A/B mode and exclusive handoff owner.
    remote_access: Mutex<Option<codec::FrontendAccessState>>,
    /// Latest geometry this renderer actually reported for each remote tab.
    /// Explicit claims copy this geometry into the same PDU as the owner move.
    reported_viewports: Mutex<HashMap<TabId, codec::ClientViewport>>,
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
}

/// RAII scope for a structure-mutating RPC; defers resyncs for its lifetime
/// and schedules the catch-up resync when the last in-flight mutation ends.
pub(crate) struct StructureMutationGuard {
    inner: Arc<ClientInner>,
}

impl Drop for StructureMutationGuard {
    fn drop(&mut self) {
        use std::sync::atomic::Ordering;
        if self
            .inner
            .mutations_in_flight
            .fetch_sub(1, Ordering::SeqCst)
            == 1
            && self.inner.resync_deferred.swap(false, Ordering::SeqCst)
        {
            let domain_id = self.inner.local_domain_id;
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
                if let Err(err) = domain.resync().await {
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
        let map = self.remote_to_local_tab.lock().unwrap();
        for (remote, local) in map.iter() {
            if *local == local_tab_id {
                return Some(*remote);
            }
        }
        None
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

    pub fn remote_to_local_pane_id(&self, remote_pane_id: PaneId) -> Option<TabId> {
        let mut pane_map = self.remote_to_local_pane.lock().unwrap();

        if let Some(id) = pane_map.get(&remote_pane_id) {
            return Some(*id);
        }

        let mux = Mux::get();

        for pane in mux.iter_panes() {
            if pane.domain_id() != self.local_domain_id {
                continue;
            }
            if let Some(pane) = pane.downcast_ref::<ClientPane>() {
                if pane.remote_pane_id() == remote_pane_id {
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
        Self {
            client,
            local_domain_id,
            local_echo_threshold_ms,
            overlay_lag_indicator,
            remote_to_local_window: Mutex::new(HashMap::new()),
            remote_to_local_tab: Mutex::new(HashMap::new()),
            remote_to_local_pane: Mutex::new(HashMap::new()),
            remote_viewports: Mutex::new(HashMap::new()),
            remote_access: Mutex::new(None),
            reported_viewports: Mutex::new(HashMap::new()),
            frontend_claim_locks: Mutex::new(HashMap::new()),
            remote_to_local_stack: Mutex::new(HashMap::new()),
            focused_remote_pane_id: Mutex::new(None),
            focus_advised_at: Mutex::new(None),
            mutations_in_flight: std::sync::atomic::AtomicUsize::new(0),
            resync_deferred: std::sync::atomic::AtomicBool::new(false),
        }
    }
}

impl ClientInner {
    fn begin_structure_mutation(self: &Arc<Self>) -> StructureMutationGuard {
        self.mutations_in_flight
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        StructureMutationGuard {
            inner: Arc::clone(self),
        }
    }

    fn structure_mutation_in_flight(&self) -> bool {
        self.mutations_in_flight
            .load(std::sync::atomic::Ordering::SeqCst)
            > 0
    }

    fn defer_resync(&self) {
        self.resync_deferred
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

pub struct ClientDomain {
    config: ClientDomainConfig,
    label: String,
    inner: Mutex<Option<Arc<ClientInner>>>,
    /// True while an attach is in flight (state() stays Detached until
    /// finish_attach installs the inner, so state alone can't dedupe).
    attaching: std::sync::atomic::AtomicBool,
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

lazy_static::lazy_static! {
    static ref THINKTERM_TREE_SINK: Mutex<Option<ThinkTermTreeSink>> = Mutex::new(None);
    static ref THINKTERM_CONNECT_SINK: Mutex<Option<ThinkTermConnectSink>> = Mutex::new(None);
    static ref THINKTERM_SESSION_SINK: Mutex<Option<ThinkTermSessionSink>> = Mutex::new(None);
    static ref THINKTERM_FRONTEND_WAKE_SINK: Mutex<Option<ThinkTermFrontendWakeSink>> = Mutex::new(None);
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

pub(crate) fn wake_thinkterm_frontend() {
    let sink = *THINKTERM_FRONTEND_WAKE_SINK.lock().unwrap();
    if let Some(sink) = sink {
        sink();
    }
}

pub(crate) fn deliver_thinkterm_tree(domain_name: &str, tree: codec::ThinkTermTree) {
    let sink = *THINKTERM_TREE_SINK.lock().unwrap();
    if let Some(sink) = sink {
        sink(domain_name, tree);
    }
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
    pub fn new(config: ClientDomainConfig) -> Self {
        let local_domain_id = alloc_domain_id();
        let label = config.label();
        Mux::get().subscribe(move |notif| mux_notify_client_domain(local_domain_id, notif));
        Self {
            config,
            label,
            inner: Mutex::new(None),
            attaching: std::sync::atomic::AtomicBool::new(false),
            local_domain_id,
        }
    }

    fn inner(&self) -> Option<Arc<ClientInner>> {
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
        let Some(inner) = self.inner() else {
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
    ) -> anyhow::Result<()> {
        let inner = Self::get_client_inner_for_domain(domain_id)?;
        if inner.client.connection_generation() != connection_generation {
            bail!("generation {connection_generation} was superseded before reattach began");
        }
        inner.begin_remote_generation();
        let domain = Mux::get()
            .get_domain(domain_id)
            .ok_or_else(|| anyhow!("domain {domain_id} disappeared during reattach"))?;

        ui.output_str("Checking server version and restoring client identity\n");
        inner.client.verify_version_compat(&ui).await?;
        if inner.client.connection_generation() != connection_generation {
            bail!("generation {connection_generation} was superseded during registration");
        }

        // A reconnect begins a new connection generation. The revision
        // baseline, any old connection-scoped presentation overlay, and the
        // Spaces a local disconnect hid all belong to the connection that just
        // died — and this is the "next connect" the Disconnect menu item
        // promises. Announced first so the tree below, and any push that
        // overtakes it, is measured against this connection; a push that beats
        // the announcement is at worst dropped as stale and put right by that
        // same fetch.
        deliver_thinkterm_connected(domain.domain_name(), connection_generation);

        let panes = inner.client.list_panes().await?;
        Self::process_pane_list(Arc::clone(&inner), panes, None, true)?;

        // Pull the tree exactly as a first attach does. Pushes only carry what
        // changes from now on, so without this the sidebar would keep showing
        // whatever it held when the link dropped. Ordinary RPCs remain behind
        // the registration barrier until SetClientId has been acknowledged.
        let client = domain
            .downcast_ref::<ClientDomain>()
            .ok_or_else(|| anyhow!("domain {domain_id} changed type during reattach"))?;

        // SetClientId deliberately does not restore ownership.  Re-advertise
        // the exact geometry this frontend rendered before the outage so the
        // server can return current access state for the new live session.
        // SetClientViewport is non-claiming: if another device took over while
        // we were offline, it remains the owner.
        for (remote_tab_id, viewport) in inner.reported_viewports_for_live_tabs() {
            let state = inner
                .client
                .set_client_viewport(codec::SetClientViewport {
                    tab_id: remote_tab_id,
                    viewport,
                })
                .await
                .with_context(|| {
                    format!("restoring viewport and access state for remote tab {remote_tab_id}")
                })?;
            client.process_remote_viewport_state(state);
        }
        if !inner.remote_to_local_tab.lock().unwrap().is_empty()
            && inner.remote_access_state().is_none()
        {
            bail!(
                "the reconnected mux session did not return frontend access state; \
                 refusing to mark generation {connection_generation} ready"
            );
        }

        client.fetch_thinkterm_tree().await.with_context(|| {
            format!(
                "fetching the ThinkTerm tree after reconnecting to {}",
                client.config.name()
            )
        })?;
        if inner.client.connection_generation() != connection_generation {
            bail!("generation {connection_generation} was superseded during topology sync");
        }
        Ok(())
    }

    pub async fn resync(&self) -> anyhow::Result<()> {
        if let Some(inner) = self.inner() {
            // A spawn/split response is about to install the mappings for
            // the very structures this resync would otherwise see as
            // unmapped (and duplicate). Defer; the mutation's guard runs a
            // catch-up resync when it completes. Checked again after the
            // round-trip because a mutation may have started while the
            // ListPanes request was in flight.
            if inner.structure_mutation_in_flight() {
                inner.defer_resync();
                return Ok(());
            }
            let panes = inner.client.list_panes().await?;
            if inner.structure_mutation_in_flight() {
                inner.defer_resync();
                return Ok(());
            }
            Self::process_pane_list(inner, panes, None, false)?;
        }
        Ok(())
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
        deliver_thinkterm_tree(self.config.name(), tree.clone());
        Ok(tree)
    }

    /// Pull the server's tree and hand it to the sink. Used on attach and
    /// whenever a client wants to force a resync of the sidebar structure.
    pub async fn fetch_thinkterm_tree(&self) -> anyhow::Result<()> {
        let inner = self
            .inner()
            .ok_or_else(|| anyhow!("domain is not attached"))?;
        let response = inner.client.get_thinkterm_tree().await?;
        deliver_thinkterm_tree(self.config.name(), response.tree);
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
                    });
                }
                codec::ClientViewport::Native {
                    size,
                    panes: remote_panes,
                }
            }
        })
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
        let state = inner
            .client
            .set_client_viewport(codec::SetClientViewport {
                tab_id: remote_tab_id,
                viewport,
            })
            .await?;
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
        let state = inner
            .client
            .claim_client_viewport(codec::ClaimClientViewport {
                tab_id: remote_tab_id,
                viewport,
            })
            .await?;
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

        for (mut tabroot, tab_title) in panes.tabs.into_iter().zip(panes.tab_titles.iter()) {
            // Translate remote stack ids into stable local ids BEFORE the
            // tree rebuild, so that GUI state keyed by pane_stack_id
            // (collapse layouts, level-2 tab bar scroll) survives resyncs.
            inner.translate_remote_stack_ids(&mut tabroot);

            let root_size = match tabroot.root_size() {
                Some(size) => size,
                None => continue,
            };

            if let Some((remote_window_id, remote_tab_id)) = tabroot.window_and_tab_ids() {
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
                tab.sync_with_pane_tree(sync_size, tabroot, |entry| {
                    workspace.replace(entry.workspace.clone());
                    remote_panes_to_forget.remove(&entry.pane_id);
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
                        primary_window_id.take();
                        continue;
                    }
                }
                log::debug!(
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
                inner.record_remote_to_local_window_mapping(remote_window_id, *local_window_id);
                mux.add_tab_to_window(&tab, *local_window_id)?;
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

        Self::process_pane_list(inner, panes, primary_window_id, false)?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        accepts_generation, owns_remote_viewport_from_states, remote_frontend_gate_from_state,
        remote_move_pane_id, RemoteFrontendGate,
    };
    use crate::client::ClientConnectionPhase;

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
                command,
                command_dir,
                domain: SpawnTabDomain::CurrentPaneDomain,
            })
            .await?;

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
                command,
                command_dir,
                workspace,
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
                command,
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
        let ui = ConnectionUI::with_params(ConnectionUIParams {
            window_id,
            ..Default::default()
        });
        self.attach_with_ui(window_id, ui).await
    }

    fn detachable(&self) -> bool {
        true
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
                let mut cloned_ui = ui.clone();
                let client = spawn_into_new_thread(move || match &config {
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
                    ClientDomainConfig::Tls(tls) => Client::new_tls(domain_id, tls, &mut cloned_ui),
                    ClientDomainConfig::Ssh(ssh) => Client::new_ssh(domain_id, ssh, &mut cloned_ui),
                })
                .await?;

                ui.output_str("Checking server version\n");
                client.verify_version_compat(&ui).await?;

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
        // shows no rows until the next push or reconnect.
        if let Err(err) = self.fetch_thinkterm_tree().await {
            log::warn!(
                "failed to fetch the ThinkTerm tree from {}: {err:#}",
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
