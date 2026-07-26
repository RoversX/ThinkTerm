use crate::client::Client;
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

pub struct ClientInner {
    pub client: Client,
    pub local_domain_id: DomainId,
    pub local_echo_threshold_ms: Option<u64>,
    pub overlay_lag_indicator: bool,
    remote_to_local_window: Mutex<HashMap<WindowId, WindowId>>,
    remote_to_local_tab: Mutex<HashMap<TabId, TabId>>,
    remote_to_local_pane: Mutex<HashMap<PaneId, PaneId>>,
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
    pub async fn reattach(domain_id: DomainId, ui: ConnectionUI) -> anyhow::Result<()> {
        let inner = Self::get_client_inner_for_domain(domain_id)?;

        let panes = inner.client.list_panes().await?;
        Self::process_pane_list(inner, panes, None)?;

        ui.close();
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
            Self::process_pane_list(inner, panes, None)?;
        }
        Ok(())
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
    ) -> anyhow::Result<()> {
        let mux = Mux::get();
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
                    let needs_add = mux
                        .get_window(local_window_id)
                        .expect("no such window!?")
                        .idx_by_id(tab.tab_id())
                        .is_none();
                    if needs_add {
                        // Use add_tab_to_window rather than window.push so
                        // that MuxNotification::TabAddedToWindow reaches the
                        // GUI: it relies on that event to impose the local
                        // window geometry on tabs that arrive via resync
                        // (their wire size is the server's, which sits below
                        // the local size by the pane nav bar reservation).
                        mux.add_tab_to_window(&tab, local_window_id)?;
                    }
                    continue;
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
                let mut window = mux
                    .get_window_mut(local_window_id)
                    .expect("no such window!?");
                window.set_title(&window_title);
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

        Self::process_pane_list(inner, panes, primary_window_id)?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::remote_move_pane_id;

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
        let source_client_pane =
            source_pane
                .downcast_ref::<ClientPane>()
                .ok_or_else(|| {
                    anyhow!("source pane_id {source_local_pane_id} is not a ClientPane")
                })?;
        let target_client_pane =
            target_pane
                .downcast_ref::<ClientPane>()
                .ok_or_else(|| {
                    anyhow!("target pane_id {target_local_pane_id} is not a ClientPane")
                })?;
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
                mux.move_pane_to_split(
                    moved_pane.pane_id(),
                    tab_id,
                    pane_id,
                    split_request,
                )?;
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

        ui.output_str("Attached!\n");
        drop(activity);
        ui.close();
        Ok(())
    }
}
