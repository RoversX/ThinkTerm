use crate::domain::{ClientDomain, ClientDomainConfig};
use crate::pane::ClientPane;
use anyhow::{anyhow, bail, Context};
use async_ossl::AsyncSslStream;
use async_trait::async_trait;
use codec::*;
use config::{configuration, SshDomain, TlsDomainClient, UnixDomain, UnixTarget};
use filedescriptor::FileDescriptor;
use futures::FutureExt;
use mux::client::ClientId;
use mux::connui::ConnectionUI;
use mux::domain::{Domain, DomainId};
use mux::pane::PaneId;
use mux::ssh::ssh_connect_with_ui_and_password;
use mux::Mux;
use openssl::ssl::{SslConnector, SslFiletype, SslMethod};
use openssl::x509::X509;
use portable_pty::Child;
use smol::channel::{bounded, unbounded, Receiver, Sender};
use smol::prelude::*;
use smol::{block_on, Async};
use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::marker::Unpin;
use std::net::TcpStream;
#[cfg(unix)]
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, RawFd};
#[cfg(unix)]
use std::os::unix::process::CommandExt;
#[cfg(windows)]
use std::os::windows::io::{AsRawSocket, AsSocket, BorrowedSocket, RawSocket};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::mpsc::channel;
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use std::time::Duration;
use thinkterm_session::connection::{
    describe_handshake_failure, describe_server_build_mismatch, leads_back_to_this_process,
    ChannelSendError, RegistrationBarrier,
};
use thiserror::Error;
use wezterm_uds::UnixStream;

pub use thinkterm_session::connection::ConnectionPhase as ClientConnectionPhase;
pub use thinkterm_session::connection::VersionHandshakeStalled;

static NEXT_CONNECTION_GENERATION: AtomicU64 = AtomicU64::new(1);

#[derive(Error, Debug)]
#[error("Timeout")]
struct Timeout;

enum ReaderMessage {
    SendPdu {
        pdu: Pdu,
        promise: Sender<anyhow::Result<Pdu>>,
        registration_required: bool,
    },
    /// `SetClientId` was acknowledged for this connection generation.  Only
    /// now may ordinary RPCs queued during reconnect reach the new session.
    RegistrationComplete {
        connection_generation: u64,
    },
    /// Tear down a generation whose transport connected but whose topology
    /// could not be restored. The normal reconnect loop will make a fresh
    /// transport and repeat the complete bootstrap.
    AbortGeneration {
        connection_generation: u64,
        reason: String,
    },
    Readable,
    /// The connection has been idle for a while: send a keepalive ping,
    /// or declare the transport dead if the previous ping went unanswered.
    KeepaliveTick,
}

struct PduRegistrationBarrier {
    inner: RegistrationBarrier<(Pdu, Sender<anyhow::Result<Pdu>>)>,
}

impl PduRegistrationBarrier {
    fn new() -> Self {
        Self {
            inner: RegistrationBarrier::new(),
        }
    }

    fn submit(
        &mut self,
        item: (Pdu, Sender<anyhow::Result<Pdu>>),
        registration_required: bool,
    ) -> Option<(Pdu, Sender<anyhow::Result<Pdu>>)> {
        self.inner.submit(item, registration_required)
    }

    fn complete(&mut self) -> VecDeque<(Pdu, Sender<anyhow::Result<Pdu>>)> {
        self.inner.complete()
    }

    fn is_complete(&self) -> bool {
        self.inner.is_complete()
    }

    fn fail_deferred(&mut self, reason: &str) {
        for (_, promise) in self.inner.drain() {
            let _ = promise.try_send(Err(anyhow!(reason.to_string())));
        }
    }
}

impl Drop for PduRegistrationBarrier {
    fn drop(&mut self) {
        self.fail_deferred("reconnect generation ended before mux client registration completed");
    }
}

#[derive(Clone)]
pub struct Client {
    sender: Sender<ReaderMessage>,
    local_domain_id: Option<DomainId>,
    pub client_id: ClientId,
    client_domain_config: ClientDomainConfig,
    pub is_reconnectable: bool,
    pub is_local: bool,
    /// Authoritative lifecycle of the current transport generation. A socket
    /// becoming writable is not Ready: the mux identity and topology must be
    /// restored first.
    connection_phase: Arc<AtomicU8>,
    resume_reconnect_tx: std::sync::mpsc::Sender<()>,
    remote_server_id: Arc<RwLock<Option<String>>>,
    /// The distro id the server reported when we shook hands, if any.
    remote_os_release: Arc<RwLock<Option<String>>>,
    /// Process-unique identity of the currently attached transport.  A
    /// reconnect gets a fresh value so queued unilateral messages from the
    /// superseded reader cannot be mistaken for current server state.
    connection_generation: Arc<AtomicU64>,
    /// Set when a reconnect cycle hits an error that retrying can never fix
    /// (a codec version mismatch). The transport reconnects fine in that
    /// state, so without this the loop would cycle "Reconnecting..."
    /// forever, showing the real error only to a headless UI. The reconnect
    /// loop checks it after every dead session, surfaces it in a visible
    /// window, and stops.
    fatal_connection_error: Arc<Mutex<Option<String>>>,
}

impl Client {
    pub fn connection_phase(&self) -> ClientConnectionPhase {
        ClientConnectionPhase::from_u8(self.connection_phase.load(Ordering::Acquire))
    }

    fn set_connection_phase(&self, phase: ClientConnectionPhase) {
        self.connection_phase.store(phase as u8, Ordering::Release);
        crate::domain::wake_thinkterm_frontend();
    }

    pub fn is_reconnecting(&self) -> bool {
        matches!(
            self.connection_phase(),
            ClientConnectionPhase::Registering
                | ClientConnectionPhase::Syncing
                | ClientConnectionPhase::Reconnecting
        )
    }

    pub fn reconnect_is_suspended(&self) -> bool {
        self.connection_phase() == ClientConnectionPhase::Suspended
    }

    /// Wake a parked reconnect loop for another round of retries.
    pub fn resume_reconnect(&self) {
        let _ = self.resume_reconnect_tx.send(());
    }

    pub fn remote_server_id(&self) -> Option<String> {
        self.remote_server_id.read().unwrap().clone()
    }

    /// What the server said it is running on, once the handshake completed.
    pub fn remote_os_release(&self) -> Option<String> {
        self.remote_os_release.read().unwrap().clone()
    }

    pub fn connection_generation(&self) -> u64 {
        self.connection_generation.load(Ordering::Acquire)
    }

    pub(crate) fn mark_ready(&self) {
        self.set_connection_phase(ClientConnectionPhase::Ready);
    }

    fn mark_registration_complete(&self) -> anyhow::Result<()> {
        let connection_generation = self.connection_generation();
        self.sender
            .try_send(ReaderMessage::RegistrationComplete {
                connection_generation,
            })
            .map_err(|_| ChannelSendError)
            .context("marking mux client registration complete")?;
        self.set_connection_phase(ClientConnectionPhase::Syncing);
        Ok(())
    }

    pub(crate) fn abort_connection_generation(&self, connection_generation: u64, reason: String) {
        let _ = self.sender.try_send(ReaderMessage::AbortGeneration {
            connection_generation,
            reason,
        });
    }
}

#[derive(Error, Debug, Clone, PartialEq, Eq)]
#[error(
    "Please install a compatible ThinkTerm/WezTerm remote mux binary on the server!\n\
     The server version is {} (codec version {}),\n\
     which is not compatible with our version \n\
     {} (codec version {}).",
    version,
    codec_vers,
    config::wezterm_version(),
    CODEC_VERSION
)]
pub struct IncompatibleVersionError {
    pub version: String,
    pub codec_vers: usize,
}

/// The server answered an RPC with a refusal. The transport carried the
/// request and the reply; only this request was rejected. Callers that
/// keep going after a refusal downcast to this so that a transport
/// failure, which returns a different error, still stops them.
#[derive(Error, Debug, Clone, PartialEq, Eq)]
#[error("{reason}")]
pub struct RemoteRpcError {
    pub reason: String,
}

impl RemoteRpcError {
    pub fn is_cause_of(err: &anyhow::Error) -> bool {
        err.chain()
            .any(|cause| cause.downcast_ref::<RemoteRpcError>().is_some())
    }
}

macro_rules! rpc {
    ($method_name:ident, $request_type:ident, $response_type:ident) => {
        pub async fn $method_name(&self, pdu: $request_type) -> anyhow::Result<$response_type> {
            let start = std::time::Instant::now();
            let result = self.send_pdu(Pdu::$request_type(pdu)).await;
            let elapsed = start.elapsed();
            metrics::histogram!("rpc", "method" => stringify!($method_name)).record(elapsed);
            metrics::counter!("rpc.count", "method" => stringify!($method_name)).increment(1);
            match result {
                Ok(Pdu::$response_type(res)) => Ok(res),
                Ok(Pdu::ErrorResponse(err)) => Err(RemoteRpcError { reason: err.reason }.into()),
                Ok(_) => bail!("unexpected response {:?}", result),
                Err(err) => Err(err),
            }
        }
    };

    // This variant allows omitting the request parameter; this is useful
    // in the case where the struct is empty and present only for the purpose
    // of typing the request.
    ($method_name:ident, $request_type:ident=(), $response_type:ident) => {
        #[allow(dead_code)]
        pub async fn $method_name(&self) -> anyhow::Result<$response_type> {
            let start = std::time::Instant::now();
            let result = self.send_pdu(Pdu::$request_type($request_type{})).await;
            let elapsed = start.elapsed();
            metrics::histogram!("rpc", "method" => stringify!($method_name)).record(elapsed);
            metrics::counter!("rpc.count", "method" => stringify!($method_name)).increment(1);
            match result {
                Ok(Pdu::$response_type(res)) => Ok(res),
                Ok(Pdu::ErrorResponse(err)) => Err(RemoteRpcError { reason: err.reason }.into()),
                Ok(_) => bail!("unexpected response {:?}", result),
                Err(err) => Err(err),
            }
        }
    };
}

fn process_unilateral_inner(
    pane_id: PaneId,
    local_domain_id: DomainId,
    connection_generation: u64,
    decoded: DecodedPdu,
) {
    promise::spawn::spawn(async move {
        process_unilateral_inner_async(pane_id, local_domain_id, connection_generation, decoded)
            .await?;
        Ok::<(), anyhow::Error>(())
    })
    .detach();
}

async fn process_unilateral_inner_async(
    pane_id: PaneId,
    local_domain_id: DomainId,
    connection_generation: u64,
    decoded: DecodedPdu,
) -> anyhow::Result<()> {
    let mux = match Mux::try_get() {
        Some(mux) => mux,
        None => {
            // This can happen for some client scenarios; it is ok to ignore it.
            return Ok(());
        }
    };

    let client_domain = mux
        .get_domain(local_domain_id)
        .ok_or_else(|| anyhow!("no such domain {}", local_domain_id))?;
    let client_domain = client_domain
        .downcast_ref::<ClientDomain>()
        .ok_or_else(|| anyhow!("domain {} is not a ClientDomain instance", local_domain_id))?;
    if client_domain.connection_generation() != Some(connection_generation) {
        return Ok(());
    }

    // Agent statuses routinely arrive before their pane's local mirror
    // exists (all through an attach). Record every one into the domain's
    // remote-keyed snapshot first — a mirror materializing later seeds
    // itself from it — and then still deliver live to a mapped pane below.
    // PaneRemoved must prune that snapshot here: the server deliberately
    // never publishes an eviction status (its PaneRemoved handler is
    // evict-only), and a retained entry would seed a *reused* remote pane
    // id with the dead pane's agent.
    // Foreground programs are retained the same way, for the same reason.
    match &decoded.pdu {
        Pdu::AgentStatusChanged(codec::AgentStatusChanged { pane_id, status }) => {
            client_domain.record_remote_agent_status(*pane_id, status.clone());
        }
        Pdu::ForegroundProgramChanged(codec::ForegroundProgramChanged { pane_id, program }) => {
            client_domain.record_remote_foreground_program(*pane_id, program.clone());
        }
        Pdu::PaneRemoved(_) => {
            client_domain.record_remote_agent_status(pane_id, None);
            client_domain.record_remote_foreground_program(pane_id, None);
        }
        _ => {}
    }

    // If we get a push for a pane that we don't yet know about,
    // it means that some other client has manipulated the mux
    // topology; we need to re-sync.
    let local_pane_id = match client_domain.remote_to_local_pane_id(pane_id) {
        Some(p) => p,
        None => {
            // Not for agent status or foreground programs though: the
            // server publishes those for every pane it owns, other
            // workspaces included, so unmapped is routine and implies no
            // topology change worth a resync. The value was recorded into
            // the snapshot above; the mirror seeds from it whenever it
            // materializes.
            if matches!(
                decoded.pdu,
                Pdu::AgentStatusChanged(_) | Pdu::ForegroundProgramChanged(_)
            ) {
                return Ok(());
            }
            log::debug!("got {decoded:?}, pane not found locally, resync");
            client_domain.resync_for_remote_pane(pane_id).await?;
            if client_domain.connection_generation() != Some(connection_generation) {
                return Ok(());
            }
            client_domain
                .remote_to_local_pane_id(pane_id)
                .ok_or_else(|| {
                    anyhow!("remote pane id {} does not have a local pane id", pane_id)
                })?
        }
    };

    let pane = match mux.get_pane(local_pane_id) {
        Some(p) => p,
        None => {
            log::debug!("got {decoded:?}, but local pane {local_pane_id} no longer exists; resync");
            client_domain.resync_for_remote_pane(pane_id).await?;
            if client_domain.connection_generation() != Some(connection_generation) {
                return Ok(());
            }

            let local_pane_id =
                client_domain
                    .remote_to_local_pane_id(pane_id)
                    .ok_or_else(|| {
                        anyhow!("remote pane id {} does not have a local pane id", pane_id)
                    })?;

            mux.get_pane(local_pane_id)
                .ok_or_else(|| anyhow!("local pane {local_pane_id} not found"))?
        }
    };
    let client_pane = pane.downcast_ref::<ClientPane>().ok_or_else(|| {
        log::error!(
            "received unilateral PDU for pane {} which is \
                     not an instance of ClientPane: {:?}",
            local_pane_id,
            decoded.pdu
        );
        anyhow!(
            "received unilateral PDU for pane {} which is \
                     not an instance of ClientPane: {:?}",
            local_pane_id,
            decoded.pdu
        )
    })?;
    client_pane.process_unilateral(decoded.pdu).await
}

fn process_unilateral(
    local_domain_id: Option<DomainId>,
    connection_generation: u64,
    decoded: DecodedPdu,
    kitty_pending: &codec::kitty_queue::KittyFrameMailboxHandle,
) -> anyhow::Result<()> {
    let local_domain_id = match local_domain_id {
        Some(id) => id,
        None => {
            // FIXME: We currently get a bunch of these; we'll need
            // to do something to advise the server when we want them.
            // For now, we just ignore them.
            log::trace!(
                "client doesn't have a real local domain, \
                 so unilateral message cannot be processed by it"
            );
            return Ok(());
        }
    };
    if let Pdu::KittyFrameSelections(snapshot) = decoded.pdu {
        if kitty_pending.post(snapshot)? {
            let pending = kitty_pending.clone();
            promise::spawn::spawn_into_main_thread(async move {
                while let Some(snapshot) = pending.next() {
                    let pane = snapshot.pane_id;
                    let decoded = DecodedPdu { serial: 0, pdu: Pdu::KittyFrameSelections(snapshot) };
                    if let Err(err) = process_unilateral_inner_async(
                        pane, local_domain_id, connection_generation, decoded,
                    ).await {
                        log::error!("processing Kitty snapshot: {err:#}");
                    }
                    smol::future::yield_now().await;
                }
            }).detach();
        }
        return Ok(());
    }
    match &decoded.pdu {
        Pdu::WindowWorkspaceChanged(WindowWorkspaceChanged {
            window_id,
            workspace,
        }) => {
            let window_id = *window_id;
            let workspace = workspace.to_string();
            promise::spawn::spawn_into_main_thread(async move {
                let mux = Mux::try_get().ok_or_else(|| anyhow!("no more mux"))?;
                let client_domain = mux
                    .get_domain(local_domain_id)
                    .ok_or_else(|| anyhow!("no such domain {}", local_domain_id))?;
                let client_domain =
                    client_domain
                        .downcast_ref::<ClientDomain>()
                        .ok_or_else(|| {
                            anyhow!("domain {} is not a ClientDomain instance", local_domain_id)
                        })?;
                if client_domain.connection_generation() != Some(connection_generation) {
                    return Ok(());
                }

                let local_window_id = client_domain
                    .remote_to_local_window_id(window_id)
                    .ok_or_else(|| anyhow!("no local window for remote window id {}", window_id))?;
                if let Some(mut window) = mux.get_window_mut(local_window_id) {
                    window.set_workspace(&workspace);
                }

                anyhow::Result::<()>::Ok(())
            })
            .detach();

            return Ok(());
        }
        Pdu::WindowTitleChanged(WindowTitleChanged { window_id, title }) => {
            let title = title.to_string();
            let window_id = *window_id;
            promise::spawn::spawn_into_main_thread(async move {
                let mux = Mux::try_get().ok_or_else(|| anyhow!("no more mux"))?;
                let client_domain = mux
                    .get_domain(local_domain_id)
                    .ok_or_else(|| anyhow!("no such domain {}", local_domain_id))?;
                let client_domain =
                    client_domain
                        .downcast_ref::<ClientDomain>()
                        .ok_or_else(|| {
                            anyhow!("domain {} is not a ClientDomain instance", local_domain_id)
                        })?;
                if client_domain.connection_generation() != Some(connection_generation) {
                    return Ok(());
                }

                client_domain.process_remote_window_title_change(window_id, title);
                anyhow::Result::<()>::Ok(())
            })
            .detach();
            return Ok(());
        }
        Pdu::RenameWorkspace(RenameWorkspace {
            old_workspace,
            new_workspace,
        }) => {
            let old_workspace = old_workspace.to_string();
            let new_workspace = new_workspace.to_string();
            promise::spawn::spawn_into_main_thread(async move {
                let mux = Mux::try_get().ok_or_else(|| anyhow!("no more mux"))?;
                let domain = mux
                    .get_domain(local_domain_id)
                    .ok_or_else(|| anyhow!("no such domain {}", local_domain_id))?;
                let domain = domain.downcast_ref::<ClientDomain>().ok_or_else(|| {
                    anyhow!("domain {} is not a ClientDomain instance", local_domain_id)
                })?;
                if domain.connection_generation() != Some(connection_generation) {
                    return Ok(());
                }
                log::debug!("got a rename {old_workspace} -> {new_workspace}");
                mux.rename_workspace(&old_workspace, &new_workspace);
                anyhow::Result::<()>::Ok(())
            })
            .detach();
            return Ok(());
        }
        Pdu::TabTitleChanged(TabTitleChanged { tab_id, title }) => {
            let title = title.to_string();
            let tab_id = *tab_id;
            promise::spawn::spawn_into_main_thread(async move {
                let mux = Mux::try_get().ok_or_else(|| anyhow!("no more mux"))?;
                let client_domain = mux
                    .get_domain(local_domain_id)
                    .ok_or_else(|| anyhow!("no such domain {}", local_domain_id))?;
                let client_domain =
                    client_domain
                        .downcast_ref::<ClientDomain>()
                        .ok_or_else(|| {
                            anyhow!("domain {} is not a ClientDomain instance", local_domain_id)
                        })?;
                if client_domain.connection_generation() != Some(connection_generation) {
                    return Ok(());
                }

                client_domain.process_remote_tab_title_change(tab_id, title);
                anyhow::Result::<()>::Ok(())
            })
            .detach();
            return Ok(());
        }
        Pdu::ThinkTermTreeState(_) => {
            // Another device (or this one) changed the server's sidebar tree.
            // Hand the whole thing to the GUI, which merges it with this
            // device's own view state.
            let Pdu::ThinkTermTreeState(state) = decoded.pdu else {
                unreachable!("matched ThinkTermTreeState above");
            };
            promise::spawn::spawn_into_main_thread(async move {
                let mux = Mux::try_get().ok_or_else(|| anyhow!("no more mux"))?;
                let domain = mux
                    .get_domain(local_domain_id)
                    .ok_or_else(|| anyhow!("no such domain {}", local_domain_id))?;
                let domain = domain
                    .downcast_ref::<ClientDomain>()
                    .ok_or_else(|| anyhow!("domain {} is not a ClientDomain", local_domain_id))?;
                if domain.connection_generation() != Some(connection_generation) {
                    return Ok(());
                }
                crate::domain::deliver_thinkterm_tree(domain.client_domain_config(), state.tree);
                anyhow::Result::<()>::Ok(())
            })
            .detach();
            return Ok(());
        }
        Pdu::ThinkTermSessionState(_) => {
            let Pdu::ThinkTermSessionState(state) = decoded.pdu else {
                unreachable!("matched ThinkTermSessionState above");
            };
            promise::spawn::spawn_into_main_thread(async move {
                let mux = Mux::try_get().ok_or_else(|| anyhow!("no more mux"))?;
                let domain = mux
                    .get_domain(local_domain_id)
                    .ok_or_else(|| anyhow!("no such domain {}", local_domain_id))?;
                let domain = domain
                    .downcast_ref::<ClientDomain>()
                    .ok_or_else(|| anyhow!("domain {} is not a ClientDomain", local_domain_id))?;
                if domain.connection_generation() != Some(connection_generation) {
                    return Ok(());
                }
                crate::domain::deliver_thinkterm_session(
                    domain.domain_name(),
                    connection_generation,
                    state,
                );
                anyhow::Result::<()>::Ok(())
            })
            .detach();
            return Ok(());
        }
        Pdu::ClientViewportState(_) => {
            let Pdu::ClientViewportState(state) = decoded.pdu else {
                unreachable!("matched ClientViewportState above");
            };
            promise::spawn::spawn_into_main_thread(async move {
                let mux = Mux::try_get().ok_or_else(|| anyhow!("no more mux"))?;
                let domain = mux
                    .get_domain(local_domain_id)
                    .ok_or_else(|| anyhow!("no such domain {}", local_domain_id))?;
                let domain = domain
                    .downcast_ref::<ClientDomain>()
                    .ok_or_else(|| anyhow!("domain {} is not a ClientDomain", local_domain_id))?;
                // Not the strict generation check of the other arms: this
                // state is pushed right after the handshake, before the
                // attach has built the inner that would carry a generation.
                if !domain.accepts_connection_generation(connection_generation) {
                    return Ok(());
                }
                domain.process_remote_viewport_state(state);
                anyhow::Result::<()>::Ok(())
            })
            .detach();
            return Ok(());
        }
        Pdu::FrontendAccessState(_) => {
            let Pdu::FrontendAccessState(state) = decoded.pdu else {
                unreachable!("matched FrontendAccessState above");
            };
            promise::spawn::spawn_into_main_thread(async move {
                let mux = Mux::try_get().ok_or_else(|| anyhow!("no more mux"))?;
                let domain = mux
                    .get_domain(local_domain_id)
                    .ok_or_else(|| anyhow!("no such domain {}", local_domain_id))?;
                let domain = domain
                    .downcast_ref::<ClientDomain>()
                    .ok_or_else(|| anyhow!("domain {} is not a ClientDomain", local_domain_id))?;
                // Not the strict generation check of the other arms: this
                // state is pushed right after the handshake, before the
                // attach has built the inner that would carry a generation.
                if !domain.accepts_connection_generation(connection_generation) {
                    return Ok(());
                }
                domain.process_remote_access_state(state);
                anyhow::Result::<()>::Ok(())
            })
            .detach();
            return Ok(());
        }
        // A GUI client renders from its own configuration; the server's
        // resolved palette is only for clients that have none of their own.
        Pdu::DefaultPalette(_) => {
            return Ok(());
        }
        Pdu::TabResized(_) | Pdu::TabAddedToWindow(_) => {
            log::trace!("resync due to {:?}", decoded.pdu);
            promise::spawn::spawn_into_main_thread(async move {
                let mux = Mux::try_get().ok_or_else(|| anyhow!("no more mux"))?;
                let client_domain = mux
                    .get_domain(local_domain_id)
                    .ok_or_else(|| anyhow!("no such domain {}", local_domain_id))?;
                let client_domain =
                    client_domain
                        .downcast_ref::<ClientDomain>()
                        .ok_or_else(|| {
                            anyhow!("domain {} is not a ClientDomain instance", local_domain_id)
                        })?;
                if client_domain.connection_generation() != Some(connection_generation) {
                    return Ok(());
                }

                // Throttled: a burst of tab-geometry pushes (every client
                // resize fans one out per tab) must not cost one full
                // ListPanes walk per PDU.
                client_domain.resync_throttled().await
            })
            .detach();

            return Ok(());
        }
        _ => {}
    }

    if let Some(pane_id) = decoded.pdu.pane_id() {
        promise::spawn::spawn_into_main_thread(async move {
            process_unilateral_inner(pane_id, local_domain_id, connection_generation, decoded)
        })
        .detach();
    } else {
        bail!("don't know how to handle {:?}", decoded);
    }
    Ok(())
}

#[derive(Error, Debug, Clone, PartialEq, Eq)]
enum NotReconnectableError {
    #[error("Client was destroyed")]
    ClientWasDestroyed,
}

/// Did this failure come from the user dismissing an auth prompt rather than
/// from a wrong password or an unreachable host? Retrying would only ask the
/// same question again, so we park instead.
///
/// Walks the whole chain rather than just the root cause, so that adding a
/// `.context()` anywhere on the way up cannot silently disable it.
fn is_auth_cancelled(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause
            .downcast_ref::<mux::ssh::AuthCancelledError>()
            .is_some()
    })
}

fn client_thread(
    reconnectable: &mut Reconnectable,
    local_domain_id: Option<DomainId>,
    connection_generation: u64,
    rx: &mut Receiver<ReaderMessage>,
) -> anyhow::Result<()> {
    block_on(client_thread_async(
        reconnectable,
        local_domain_id,
        connection_generation,
        rx,
    ))
}

async fn client_thread_async(
    reconnectable: &mut Reconnectable,
    local_domain_id: Option<DomainId>,
    connection_generation: u64,
    rx: &mut Receiver<ReaderMessage>,
) -> anyhow::Result<()> {
    let mut next_serial = 1u64;
    let mut registration = PduRegistrationBarrier::new();
    let mut deferred_unilateral = VecDeque::<DecodedPdu>::new();
    let mut deferred_kitty: Option<codec::kitty_queue::KittyFrameMailbox> = None;
    let kitty_frames = codec::kitty_queue::KittyFrameMailbox::default();
    let kitty_pending = kitty_frames.handle();

    struct Promises {
        map: HashMap<u64, Sender<anyhow::Result<Pdu>>>,
    }

    impl Promises {
        fn fail_all(&mut self, reason: &str) {
            log::trace!("failing all promises: {}", reason);
            for (_, promise) in self.map.drain() {
                let _ = promise.try_send(Err(anyhow!("{}", reason)));
            }
        }
    }

    impl Drop for Promises {
        fn drop(&mut self) {
            self.fail_all("Client was destroyed");
        }
    }
    let mut promises = Promises {
        map: HashMap::new(),
    };

    let mut stream = reconnectable.take_stream().unwrap();

    // Application-level keepalive: a transport that died without an RST
    // (VPN egress rotation, sleepy NAT) otherwise hangs silently until the
    // next write. Ping after this much idle time and require the pong to
    // arrive before the following tick.
    const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(15);
    let mut pending_ping: Option<(u64, std::time::Instant)> = None;
    // Measured from the last byte *received*. Measuring from the last loop
    // event, as this used to, let every outbound request push the deadline
    // back, so a client that kept asking and never got answered would
    // never notice.
    let mut keepalive_deadline = std::time::Instant::now() + KEEPALIVE_INTERVAL;

    loop {
        let rx_msg = rx.recv();
        let wait_for_read = stream
            .wait_for_readable()
            .map(|_| Ok(ReaderMessage::Readable));
        let keepalive = async {
            smol::Timer::at(keepalive_deadline).await;
            Ok(ReaderMessage::KeepaliveTick)
        };

        match smol::future::or(smol::future::or(rx_msg, wait_for_read), keepalive).await {
            Ok(ReaderMessage::SendPdu {
                pdu,
                promise,
                registration_required,
            }) => {
                let Some((pdu, promise)) =
                    registration.submit((pdu, promise), registration_required)
                else {
                    continue;
                };
                let serial = next_serial;
                next_serial += 1;
                promises.map.insert(serial, promise);

                pdu.encode_async(&mut stream, serial)
                    .await
                    .context("encoding a PDU to send to the server")?;
                stream.flush().await.context("flushing PDU to server")?;
            }
            Ok(ReaderMessage::RegistrationComplete {
                connection_generation: completed_generation,
            }) => {
                if completed_generation != connection_generation {
                    log::debug!(
                        "ignoring registration completion for generation {completed_generation}; \
                         current generation is {connection_generation}"
                    );
                    continue;
                }
                while let Some(decoded) = deferred_unilateral.pop_front() {
                    process_unilateral(local_domain_id, connection_generation, decoded, &kitty_pending)
                        .context("processing unilateral PDU buffered during registration")?;
                }
                if let Some(deferred) = deferred_kitty.take() {
                    let pending = deferred.handle();
                    while let Some(snapshot) = pending.next() {
                        process_unilateral(local_domain_id, connection_generation,
                            DecodedPdu { serial: 0, pdu: Pdu::KittyFrameSelections(snapshot) },
                            &kitty_pending)?;
                    }
                }
                for (pdu, promise) in registration.complete() {
                    let serial = next_serial;
                    next_serial += 1;
                    promises.map.insert(serial, promise);
                    pdu.encode_async(&mut stream, serial)
                        .await
                        .context("encoding a deferred PDU after client registration")?;
                }
                stream
                    .flush()
                    .await
                    .context("flushing deferred PDUs after client registration")?;
            }
            Ok(ReaderMessage::AbortGeneration {
                connection_generation: aborted_generation,
                reason,
            }) => {
                if aborted_generation != connection_generation {
                    log::debug!(
                        "ignoring abort for generation {aborted_generation}; \
                         current generation is {connection_generation}: {reason}"
                    );
                    continue;
                }
                promises.fail_all(&reason);
                registration.fail_deferred(&reason);
                anyhow::bail!("reattach failed for generation {connection_generation}: {reason}");
            }
            Ok(ReaderMessage::KeepaliveTick) => {
                if let Some((serial, sent)) = pending_ping.take() {
                    let reason = format!(
                        "keepalive: no response to ping serial {serial} after {:?}; \
                         transport presumed dead",
                        sent.elapsed()
                    );
                    promises.fail_all(&reason);
                    anyhow::bail!("{reason}");
                }
                let serial = next_serial;
                next_serial += 1;
                pending_ping = Some((serial, std::time::Instant::now()));
                keepalive_deadline = std::time::Instant::now() + KEEPALIVE_INTERVAL;
                Pdu::Ping(Ping {})
                    .encode_async(&mut stream, serial)
                    .await
                    .context("encoding keepalive ping")?;
                stream.flush().await.context("flushing keepalive ping")?;
            }
            Ok(ReaderMessage::Readable) => {
                match Pdu::decode_async(&mut stream, Some(next_serial)).await {
                    Ok(decoded) => {
                        crate::domain::wake_thinkterm_frontend();
                        // Traffic postpones the next ping, but not the
                        // verdict on one already sent: a server that keeps
                        // pushing output while never answering is still
                        // one that never answers.
                        if pending_ping.is_none() {
                            keepalive_deadline = std::time::Instant::now() + KEEPALIVE_INTERVAL;
                        }
                        log::debug!(
                            "decoded serial {} {}",
                            decoded.serial,
                            decoded.pdu.pdu_name()
                        );
                        // The server asking whether we are still here. Answered
                        // right here, before registration and off the main
                        // thread, so a stalled GUI never looks like a dead
                        // client.
                        if matches!(decoded.pdu, Pdu::Ping(_)) && decoded.serial == 0 {
                            Pdu::Pong(Pong {})
                                .encode_async(&mut stream, 0)
                                .await
                                .context("encoding pong")?;
                            stream.flush().await.context("flushing pong")?;
                            continue;
                        }
                        if pending_ping.map_or(false, |(serial, _)| serial == decoded.serial) {
                            pending_ping = None;
                            keepalive_deadline = std::time::Instant::now() + KEEPALIVE_INTERVAL;
                        } else if decoded.serial == 0 {
                            if registration.is_complete() {
                                process_unilateral(local_domain_id, connection_generation, decoded, &kitty_pending)
                                    .context("processing unilateral PDU from server")
                                    .map_err(|e| {
                                        log::error!("process_unilateral: {:?}", e);
                                        e
                                    })?;
                            } else {
                                match decoded.pdu {
                                    Pdu::KittyFrameSelections(snapshot) => {
                                        deferred_kitty.get_or_insert_with(Default::default)
                                            .handle().post(snapshot)?;
                                    }
                                    _ => deferred_unilateral.push_back(decoded),
                                }
                            }
                        } else if let Some(promise) = promises.map.remove(&decoded.serial) {
                            if promise.try_send(Ok(decoded.pdu)).is_err() {
                                // The requester stopped waiting — e.g. a
                                // bootstrap RPC timed out and dropped its
                                // receiver. That abandons one request, not
                                // the client: killing the connection here
                                // turned a transient handshake stall into a
                                // permanent detach. Client teardown proper is
                                // detected by the channel close below.
                                log::debug!(
                                    "response for serial {} arrived after its \
                                     requester gave up; discarding",
                                    decoded.serial
                                );
                            }
                        } else {
                            let reason =
                                format!("got serial {:?} without a corresponding promise", decoded);
                            promises.fail_all(&reason);
                            anyhow::bail!("{}", reason);
                        }
                    }
                    Err(err) => {
                        let reason = format!("Error while decoding response pdu: {:#}", err);
                        log::error!("{}", reason);
                        promises.fail_all(&reason);
                        return Err(err).context("Error while decoding response pdu");
                    }
                }
            }
            Err(_) => {
                return Err(NotReconnectableError::ClientWasDestroyed.into());
            }
        }
    }
}

/// Connect to a server this process has just spawned: it answers the moment
/// it binds, so watch for that rather than sleeping a guessed interval. The
/// deadline is generous because overrunning it is worse than waiting -- the
/// GUI then runs this launch's terminals in process, and only logs it.
fn connect_to_spawned_server(path: &Path) -> anyhow::Result<UnixStream> {
    const POLL: Duration = Duration::from_millis(15);
    const DEADLINE: Duration = Duration::from_secs(5);

    let started = std::time::Instant::now();
    loop {
        match UnixStream::connect(path) {
            Ok(stream) => return Ok(stream),
            Err(err) => {
                // Waiting only mends a socket that is missing or unattended.
                // A name too long to be an address, a directory we may not
                // enter, refuse as firmly in five seconds as they do now.
                let mends_itself = matches!(
                    err.kind(),
                    std::io::ErrorKind::NotFound
                        | std::io::ErrorKind::ConnectionRefused
                        | std::io::ErrorKind::Interrupted
                );
                if !mends_itself || started.elapsed() >= DEADLINE {
                    let waited = started.elapsed();
                    return Err(err).with_context(|| {
                        format!(
                            "connecting to {} after spawning the server ({waited:.1?} elapsed)",
                            path.display()
                        )
                    });
                }
                std::thread::sleep(POLL);
            }
        }
    }
}

pub fn unix_connect_with_retry(
    target: &UnixTarget,
    just_spawned: bool,
    max_attempts: Option<u64>,
) -> anyhow::Result<UnixStream> {
    let mut error = None;

    if just_spawned {
        // A proxy command is not a server that comes up by itself: the
        // loop below runs it afresh each pass and decides for itself.
        if let UnixTarget::Socket(path) = target {
            return connect_to_spawned_server(path);
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }

    let max_attempts = max_attempts.unwrap_or(10);

    for iter in 0..max_attempts {
        if iter > 0 {
            std::thread::sleep(std::time::Duration::from_millis(iter * 50));
        }
        match target {
            UnixTarget::Socket(path) => match UnixStream::connect(path) {
                Ok(stream) => return Ok(stream),
                Err(err) => {
                    error =
                        Some(Err(err).with_context(|| format!("connecting to {}", path.display())))
                }
            },
            UnixTarget::Proxy(argv) => {
                let mut cmd = std::process::Command::new(&argv[0]);
                cmd.args(&argv[1..]);

                let (a, b) = filedescriptor::socketpair()?;

                cmd.stdin(b.as_stdio()?);
                cmd.stdout(b.as_stdio()?);
                cmd.stderr(std::process::Stdio::inherit());
                let mut child = cmd
                    .spawn()
                    .with_context(|| format!("spawning proxy command {:?}", cmd))?;

                error.take();

                // Grace period to detect whether connection failed
                for _ in 0..5 {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                    match child.try_wait() {
                        Ok(Some(status)) => {
                            error = Some(Err(anyhow!(
                                "{:?} exited already with status {:?}",
                                cmd,
                                status
                            )));
                            continue;
                        }
                        Ok(None) => {
                            error.take();
                        }
                        Err(err) => {
                            error =
                                Some(Err(err).context(format!("spawning proxy command {:?}", cmd)));
                            continue;
                        }
                    }
                }

                if error.is_none() {
                    #[cfg(unix)]
                    unsafe {
                        use std::os::unix::io::{FromRawFd, IntoRawFd};
                        return Ok(UnixStream::from_raw_fd(a.into_raw_fd()));
                    }
                    #[cfg(windows)]
                    unsafe {
                        use std::os::windows::io::{FromRawSocket, IntoRawSocket};
                        return Ok(UnixStream::from_raw_socket(a.into_raw_socket()));
                    }
                }
            }
        }
    }

    error.expect("only get here after at least one unix fail")
}

#[async_trait(?Send)]
pub trait AsyncReadAndWrite: Unpin + AsyncRead + AsyncWrite + std::fmt::Debug + Send {
    async fn wait_for_readable(&self) -> anyhow::Result<()>;
}

#[async_trait(?Send)]
impl<T> AsyncReadAndWrite for Async<T>
where
    T: std::fmt::Debug,
    T: std::io::Write,
    T: std::io::Read,
    T: Send,
    T: async_io::IoSafe,
{
    async fn wait_for_readable(&self) -> anyhow::Result<()> {
        Ok(self.readable().await?)
    }
}

/// Which rule produced the default unix domain.  Only the GUI-socket branch
/// is a guess: it names whatever a GUI last published, which may have died
/// between the liveness check and our connect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolvedVia {
    Env,
    GuiSock,
    ConfiguredDomain,
}

#[derive(Debug)]
struct Reconnectable {
    config: ClientDomainConfig,
    stream: Option<Box<dyn AsyncReadAndWrite>>,
    tls_creds: Option<GetTlsCredsResponse>,
}

struct SshStream {
    stdin: FileDescriptor,
    stdout: FileDescriptor,
}

unsafe impl async_io::IoSafe for SshStream {}

impl std::fmt::Debug for SshStream {
    fn fmt(&self, fmt: &mut std::fmt::Formatter<'_>) -> std::result::Result<(), std::fmt::Error> {
        write!(fmt, "SshStream {{...}}")
    }
}

#[cfg(unix)]
impl AsFd for SshStream {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.stdout.as_fd()
    }
}

#[cfg(unix)]
impl AsRawFd for SshStream {
    fn as_raw_fd(&self) -> RawFd {
        self.stdout.as_raw_fd()
    }
}

#[cfg(windows)]
impl AsRawSocket for SshStream {
    fn as_raw_socket(&self) -> RawSocket {
        self.stdout.as_raw_socket()
    }
}

#[cfg(windows)]
impl AsSocket for SshStream {
    fn as_socket(&self) -> BorrowedSocket {
        self.stdout.as_socket()
    }
}

impl Read for SshStream {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, std::io::Error> {
        self.stdout.read(buf)
    }
}

impl Write for SshStream {
    fn write(&mut self, buf: &[u8]) -> Result<usize, std::io::Error> {
        self.stdin.write(buf)
    }
    fn flush(&mut self) -> Result<(), std::io::Error> {
        self.stdin.flush()
    }
}

impl Reconnectable {
    fn new(config: ClientDomainConfig, stream: Option<Box<dyn AsyncReadAndWrite>>) -> Self {
        Self {
            config,
            stream,
            tls_creds: None,
        }
    }

    fn tls_creds_path(&self) -> anyhow::Result<PathBuf> {
        let path = config::pki_dir()?.join(self.config.name());
        std::fs::create_dir_all(&path)?;
        Ok(path)
    }

    fn tls_creds_ca_path(&self) -> anyhow::Result<PathBuf> {
        Ok(self.tls_creds_path()?.join("ca.pem"))
    }

    fn tls_creds_cert_path(&self) -> anyhow::Result<PathBuf> {
        Ok(self.tls_creds_path()?.join("cert.pem"))
    }

    fn take_stream(&mut self) -> Option<Box<dyn AsyncReadAndWrite>> {
        self.stream.take()
    }

    fn is_local(&mut self) -> bool {
        matches!(&self.config, ClientDomainConfig::Unix(_))
    }

    fn is_local_session_host(&self) -> bool {
        self.config.is_local_session_host()
    }

    fn reconnectable(&mut self) -> bool {
        match &self.config {
            // The session server of this machine: it exits when a newer
            // build takes its panes over (same runtime id, the client
            // rebinds) and it can crash (a replacement, restored from the
            // saved layout). Any other unix domain only goes away when its
            // server dies, and respawning one would not bring the tabs back.
            ClientDomainConfig::Unix(unix) => unix.local_session_host,
            ClientDomainConfig::Tls(_) => true,
            // An ssh transport dies whenever the network path changes
            // (VPN egress rotation, sleep/wake, flaky wifi); the remote mux
            // server is typically still alive, so reconnect and reattach.
            // A user-initiated detach surfaces as ClientWasDestroyed rather
            // than an IO error, so it never reaches the reconnect loop, and
            // any auth prompts are handled by the ConnectionUI just like the
            // initial connect.
            ClientDomainConfig::Ssh(_) => true,
        }
    }

    /// A dropped ssh transport surfaces as a plain EOF on the channel, which
    /// is indistinguishable from a deliberate server-side close; since the
    /// common case by far is a network-level drop, we still reconnect. The
    /// reconnect UI is cancellable for the rare deliberate-shutdown case.
    fn reconnect_on_eof(&self) -> bool {
        match &self.config {
            ClientDomainConfig::Ssh(_) => true,
            // EOF is how a handoff or a crash of the local session server
            // shows up.
            ClientDomainConfig::Unix(unix) => unix.local_session_host,
            ClientDomainConfig::Tls(_) => false,
        }
    }

    fn connect(
        &mut self,
        initial: bool,
        ui: &mut ConnectionUI,
        no_auto_start: bool,
    ) -> anyhow::Result<()> {
        match self.config.clone() {
            ClientDomainConfig::Unix(unix_dom) => {
                self.unix_connect(unix_dom, initial, ui, no_auto_start)
            }
            ClientDomainConfig::Tls(tls) => self.tls_connect(tls, initial, ui),
            ClientDomainConfig::Ssh(ssh) => self.ssh_connect(ssh, initial, ui, no_auto_start),
        }
    }

    /// Build a command that runs a compatible mux binary on the remote system.
    /// We can't simply derive this from the current executable because
    /// we are being asked to produce a path for the remote system and
    /// we don't really know anything about it.
    /// `path` comes from the SshDoman::remote_wezterm_path option; if set
    /// then the user has told us where to look.
    /// Otherwise, use `wezterm` for compatibility with existing remote hosts.
    /// This string is passed directly to SSH exec, so avoid shell-specific
    /// fallback logic here; users can set remote_wezterm_path or
    /// override_proxy_command when their remote binary has another name.
    ///
    /// `~/.local/bin` is tried first: it is where install.sh (and the remote
    /// update that runs it) puts the binary, and a non-interactive ssh shell
    /// does not source the startup files that would add it to PATH. A copy
    /// the person installed there for their own user is also the more
    /// deliberate choice over a system-wide one.
    fn remote_mux_command(path: &Option<String>, args: &str) -> String {
        match path {
            Some(path) => format!("{path} {args}"),
            None => format!(
                "if [ -x \"$HOME/.local/bin/thinkterm\" ]; \
                 then exec \"$HOME/.local/bin/thinkterm\" {args}; \
                 elif command -v thinkterm >/dev/null 2>&1; \
                 then exec thinkterm {args}; else exec wezterm {args}; fi"
            ),
        }
    }

    fn ssh_connect(
        &mut self,
        ssh_dom: SshDomain,
        initial: bool,
        ui: &mut ConnectionUI,
        no_auto_start: bool,
    ) -> anyhow::Result<()> {
        let ssh_config = mux::ssh::ssh_domain_to_ssh_config(&ssh_dom)?;

        let sess =
            ssh_connect_with_ui_and_password(ssh_config, ui, ssh_dom.stored_password.clone())?;
        let cmd = if let Some(cmd) = ssh_dom.override_proxy_command.clone() {
            cmd
        } else if initial || !no_auto_start {
            Self::remote_mux_command(&ssh_dom.remote_wezterm_path, "cli --prefer-mux proxy")
        } else {
            Self::remote_mux_command(
                &ssh_dom.remote_wezterm_path,
                "cli --prefer-mux --no-auto-start proxy",
            )
        };
        ui.output_str(&format!("Running: {}\n", cmd));
        log::debug!("going to run {}", cmd);

        let exec = smol::block_on(sess.exec(&cmd, None))?;

        let mut stderr = exec.stderr;
        std::thread::spawn(move || {
            let mut buf = [0u8; 1024];
            while let Ok(len) = stderr.read(&mut buf) {
                if len == 0 {
                    break;
                } else {
                    let stderr = &buf[0..len];
                    log::error!("ssh stderr: {}", String::from_utf8_lossy(stderr));
                }
            }
        });

        // This is a bit gross, but it helps to surface errors in running
        // the proxy, and prevents us from hanging forever after the process
        // has died
        let mut child = exec.child;
        std::thread::spawn(move || match child.wait() {
            Err(err) => log::error!("waiting on {} failed: {:#}", cmd, err),
            Ok(status) if !status.success() => log::error!("{}: {}", cmd, status),
            _ => {}
        });

        let stream: Box<dyn AsyncReadAndWrite> = Box::new(Async::new(SshStream {
            stdin: exec.stdin,
            stdout: exec.stdout,
        })?);
        self.stream.replace(stream);
        Ok(())
    }

    fn unix_connect(
        &mut self,
        unix_dom: UnixDomain,
        initial: bool,
        ui: &mut ConnectionUI,
        no_auto_start: bool,
    ) -> anyhow::Result<()> {
        let target = unix_dom.target();
        ui.output_str(&format!("Connect to {:?}\n", target));
        log::trace!("connect to {:?}", target);

        // A socket nobody is about to bind does not begin answering while we
        // wait, so ask it once: failing here goes on to spawn the server, and
        // a domain that may not do that has nothing to wait for either. The
        // ladder is for reconnects, which someone else may be reviving.
        let will_spawn_on_failure = !no_auto_start
            && !unix_dom.no_serve_automatically
            && (initial || unix_dom.local_session_host);
        let max_attempts = match &target {
            UnixTarget::Socket(_) if initial || no_auto_start || will_spawn_on_failure => Some(1),
            _ => {
                if no_auto_start {
                    Some(1)
                } else {
                    None
                }
            }
        };

        let stream = match unix_connect_with_retry(&target, false, max_attempts) {
            Ok(stream) => stream,
            Err(e) => {
                if !will_spawn_on_failure {
                    bail!("failed to connect to {:?}: {}", target, e);
                }
                // A host of this machine that stops answering is gone for
                // good; the replacement started here restores its layout.
                log::warn!(
                    "While connecting to {:?}: {}.  Will try spawning the server.",
                    target,
                    e
                );
                ui.output_str(&format!("Error: {}.  Will try spawning server.\n", e));

                let argv = unix_dom.serve_command()?;

                let mut cmd = std::process::Command::new(&argv[0]);
                cmd.args(&argv[1..]);

                #[cfg(unix)]
                if let Some(mask) = umask::UmaskSaver::saved_umask() {
                    unsafe {
                        cmd.pre_exec(move || {
                            libc::umask(mask);
                            Ok(())
                        });
                    }
                }

                // The server is a console-subsystem binary; without this a
                // console window flashes every time the GUI starts it.
                #[cfg(windows)]
                {
                    use std::os::windows::process::CommandExt as _;
                    cmd.creation_flags(winapi::um::winbase::CREATE_NO_WINDOW);
                }

                log::warn!("Running: {:?}", cmd);
                ui.output_str(&format!("Running: {:?}\n", cmd));

                let child = cmd
                    .spawn()
                    .with_context(|| format!("while spawning {:?}", cmd))?;
                if unix_dom.local_session_host {
                    crate::local_update::note_local_session_host_started();
                }
                std::thread::spawn(move || match child.wait_with_output() {
                    Ok(out) => {
                        // `--daemonize` returns once it has forked, so a
                        // failing status is a server that never started at
                        // all: it lost the pid lock, or could not bind.
                        if !out.status.success() {
                            log::warn!("the server we spawned exited with {}", out.status);
                        }
                        if let Ok(stdout) = std::str::from_utf8(&out.stdout) {
                            if !stdout.is_empty() {
                                log::warn!("stdout: {}", stdout);
                            }
                        }
                        if let Ok(stderr) = std::str::from_utf8(&out.stderr) {
                            if !stderr.is_empty() {
                                log::warn!("stderr: {}", stderr);
                            }
                        }
                    }
                    Err(err) => {
                        log::error!("spawn: {:#}", err);
                    }
                });

                unix_connect_with_retry(&target, true, None).with_context(|| {
                    format!("(after spawning server) failed to connect to {:?}", target)
                })?
            }
        };

        ui.output_str("Connected!\n");
        stream.set_read_timeout(Some(unix_dom.read_timeout))?;
        stream.set_write_timeout(Some(unix_dom.write_timeout))?;
        let stream: Box<dyn AsyncReadAndWrite> = Box::new(Async::new(stream)?);
        self.stream.replace(stream);
        Ok(())
    }

    pub fn tls_connect(
        &mut self,
        tls_client: TlsDomainClient,
        _initial: bool,
        ui: &mut ConnectionUI,
    ) -> anyhow::Result<()> {
        openssl::init();

        let remote_address = &tls_client.remote_address;

        let remote_host_name = remote_address.split(':').next().ok_or_else(|| {
            anyhow!(
                "expected mux_server_remote_address to have the form 'host:port', but have {}",
                remote_address
            )
        })?;

        // If we are reconnecting and already bootstrapped via SSH, let's see if
        // we can connect using those same credentials and avoid running through
        // the SSH authentication flow.
        if let Some(Ok(_)) = tls_client.ssh_parameters() {
            match self.try_connect(&tls_client, ui, &remote_address, remote_host_name) {
                Ok(stream) => {
                    self.stream.replace(stream);
                    return Ok(());
                }
                Err(err) => {
                    if let Some(ioerr) = err.root_cause().downcast_ref::<std::io::Error>() {
                        match ioerr.kind() {
                            std::io::ErrorKind::ConnectionRefused => {
                                // Server isn't up yet; let's proceed with bootstrap
                            }
                            _ => {
                                // If it is an IO error that implies that we had an issue
                                // reaching or otherwise talking to the remote host.
                                // Re-attempting the SSH bootstrap most likely will not
                                // succeed so we let this bubble up.
                                return Err(err);
                            }
                        }
                    }
                    ui.output_str(&format!(
                        "Failed to reuse creds: {:?}\nWill retry bootstrap via SSH\n",
                        err
                    ));
                }
            }
        }

        if let Some(Ok(ssh_params)) = tls_client.ssh_parameters() {
            if self.tls_creds.is_none() {
                // We need to bootstrap via an ssh session

                let mut ssh_config = wezterm_ssh::Config::new();
                ssh_config.add_default_config_files();

                let mut fields = ssh_params.host_and_port.split(':');
                let host = fields
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("no host component somehow"))?;
                let port = fields.next();

                let mut ssh_config = ssh_config.for_host(host);
                if let Some(username) = &ssh_params.username {
                    ssh_config.insert("user".to_string(), username.to_string());
                }
                if let Some(port) = port {
                    ssh_config.insert("port".to_string(), port.to_string());
                }

                let sess = ssh_connect_with_ui_and_password(ssh_config, ui, None)?;

                let creds = ui.run_and_log_error(|| {
                    // The `tlscreds` command will start the server if needed and then
                    // obtain client credentials that we can use for tls.
                    let cmd =
                        Self::remote_mux_command(&tls_client.remote_wezterm_path, "cli tlscreds");

                    ui.output_str(&format!("Running: {}\n", cmd));
                    let mut exec = smol::block_on(sess.exec(&cmd, None))
                        .with_context(|| format!("executing `{}` on remote host", cmd))?;

                    log::debug!("waiting for command to finish");
                    let status = exec.child.wait()?;
                    if !status.success() {
                        anyhow::bail!("{} failed", cmd);
                    }

                    drop(exec.stdin);

                    let mut stderr = exec.stderr;
                    thread::spawn(move || {
                        // stderr is ideally empty
                        let mut err = String::new();
                        let _ = stderr.read_to_string(&mut err);
                        if !err.is_empty() {
                            log::error!("remote: `{}` stderr -> `{}`", cmd, err);
                        }
                    });

                    let creds = match Pdu::decode(exec.stdout)
                        .context("reading tlscreds response")?
                        .pdu
                    {
                        Pdu::GetTlsCredsResponse(creds) => creds,
                        _ => bail!("unexpected response to tlscreds"),
                    };

                    // Save the credentials to disk, as that is currently the easiest
                    // way to get them into openssl.  Ideally we'd keep these entirely
                    // in memory.
                    std::fs::write(&self.tls_creds_ca_path()?, creds.ca_cert_pem.as_bytes())?;
                    std::fs::write(
                        &self.tls_creds_cert_path()?,
                        creds.client_cert_pem.as_bytes(),
                    )?;
                    log::info!("got TLS creds");
                    Ok(creds)
                })?;
                self.tls_creds.replace(creds);
            }
        }

        let cloned_ui = ui.clone();
        let stream = cloned_ui.run_and_log_error({
            || self.try_connect(&tls_client, ui, &remote_address, remote_host_name)
        })?;
        self.stream.replace(stream);
        Ok(())
    }

    fn try_connect(
        &mut self,
        tls_client: &TlsDomainClient,
        ui: &mut ConnectionUI,
        remote_address: &str,
        remote_host_name: &str,
    ) -> anyhow::Result<Box<dyn AsyncReadAndWrite>> {
        let mut connector = SslConnector::builder(SslMethod::tls())?;

        let cert_file = match tls_client.pem_cert.clone() {
            Some(cert) => cert,
            None => self.tls_creds_cert_path()?,
        };

        connector
            .set_certificate_file(&cert_file, SslFiletype::PEM)
            .context(format!(
                "set_certificate_file to {} for TLS client",
                cert_file.display()
            ))?;

        if let Some(chain_file) = tls_client.pem_ca.as_ref() {
            connector
                .set_certificate_chain_file(&chain_file)
                .context(format!(
                    "set_certificate_chain_file to {} for TLS client",
                    chain_file.display()
                ))?;
        }

        let key_file = match tls_client.pem_private_key.clone() {
            Some(key) => key,
            None => self.tls_creds_cert_path()?,
        };
        connector
            .set_private_key_file(&key_file, SslFiletype::PEM)
            .context(format!(
                "set_private_key_file to {} for TLS client",
                key_file.display()
            ))?;

        fn load_cert(name: &Path) -> anyhow::Result<X509> {
            let cert_bytes = std::fs::read(name)?;
            log::trace!("loaded {}", name.display());
            Ok(X509::from_pem(&cert_bytes)?)
        }
        for name in &tls_client.pem_root_certs {
            if name.is_dir() {
                for entry in std::fs::read_dir(name)? {
                    if let Ok(cert) = load_cert(&entry?.path()) {
                        connector.cert_store_mut().add_cert(cert).ok();
                    }
                }
            } else {
                connector.cert_store_mut().add_cert(load_cert(name)?)?;
            }
        }

        if let Ok(ca_path) = self.tls_creds_ca_path() {
            if ca_path.exists() {
                connector.cert_store_mut().add_cert(load_cert(&ca_path)?)?;
            }
        }

        let connector = connector.build();
        let connector = connector
            .configure()?
            .verify_hostname(!tls_client.accept_invalid_hostnames);

        ui.output_str(&format!("Connecting to {} using TLS\n", remote_address));
        let stream = TcpStream::connect(remote_address)
            .with_context(|| format!("connecting to {}", remote_address))?;
        stream.set_nodelay(true)?;
        stream.set_write_timeout(Some(tls_client.write_timeout))?;
        stream.set_read_timeout(Some(tls_client.read_timeout))?;

        let stream = Box::new(Async::new(AsyncSslStream::new(
            connector
                .connect(
                    tls_client
                        .expected_cn
                        .as_deref()
                        .unwrap_or(remote_host_name),
                    stream,
                )
                .with_context(|| {
                    format!(
                        "SslConnector for {} with host name {}",
                        remote_address, remote_host_name,
                    )
                })?,
        ))?);
        ui.output_str("TLS Connected!\n");
        Ok(stream)
    }
}

impl Client {
    fn new(local_domain_id: Option<DomainId>, mut reconnectable: Reconnectable) -> Self {
        let client_domain_config = reconnectable.config.clone();
        let is_reconnectable = reconnectable.reconnectable();
        let is_local = reconnectable.is_local();
        let (sender, mut receiver) = unbounded();
        let client_id = mux::client::generate_client_id();
        let reader_client_id = client_id.clone();
        let connection_phase = Arc::new(AtomicU8::new(ClientConnectionPhase::Registering as u8));
        let reader_connection_phase = Arc::clone(&connection_phase);
        let (resume_reconnect_tx, resume_reconnect_rx) = channel::<()>();
        let remote_server_id = Arc::new(RwLock::new(None));
        let remote_os_release = Arc::new(RwLock::new(None));
        let connection_generation = Arc::new(AtomicU64::new(0));
        let reader_connection_generation = Arc::clone(&connection_generation);
        let fatal_connection_error = Arc::new(Mutex::new(None));
        let reader_fatal_connection_error = Arc::clone(&fatal_connection_error);

        thread::spawn(move || {
            const BASE_INTERVAL: Duration = Duration::from_secs(1);
            const MAX_INTERVAL: Duration = Duration::from_secs(10);
            // A connection that dies this quickly after a "successful"
            // connect never really worked. The classic shape is the ssh hop
            // coming up while the remote mux server is gone (the host
            // rebooted): the proxy spawns fine and then exits on its first
            // read, so connect() reports success and the session dies within
            // a second.
            const SHORT_SESSION: Duration = Duration::from_secs(15);
            // After this much continuous failure, stop hammering the
            // network and park until the user asks for another round (the
            // sidebar Reconnect button). Nothing is torn down: the domain
            // stays attached and every window and pane survives, ready for
            // the next attempt.
            const GIVE_UP_AFTER: Duration = Duration::from_secs(120);

            let mut backoff = BASE_INTERVAL;
            // When the outage in progress began, or None while the session
            // is healthy. It is kept across rounds: a transport that comes
            // back every time only for the restored session to be refused
            // is as much an outage as a network that stays down, and a
            // clock restarted on every round could never reach the give-up.
            let mut outage_started: Option<std::time::Instant> = None;
            // Consecutive sessions that died within SHORT_SESSION of
            // connecting. Only established-then-dead sessions count; a
            // connect() that fails outright (network still down) does not.
            let mut short_sessions = 0usize;
            // One reconnect UI for the lifetime of this client, reused across
            // reconnect cycles. It is closed by a successful reattach or when
            // we give up; creating one per cycle piles up dead relays while
            // the remote is unhealthy. It starts with no window at all and
            // grows one only if a prompt needs answering, so window placement
            // and Space drift are the relay's problem, not ours.
            let mut reconnect_ui: Option<ConnectionUI> = None;

            let mut pending_reattach_ui: Option<ConnectionUI> = None;
            'client: loop {
                let session_started = std::time::Instant::now();
                let generation = NEXT_CONNECTION_GENERATION.fetch_add(1, Ordering::AcqRel);
                reader_connection_generation.store(generation, Ordering::Release);
                if let (Some(reattach_ui), Some(local_domain_id)) =
                    (pending_reattach_ui.take(), local_domain_id)
                {
                    let phase = Arc::clone(&reader_connection_phase);
                    promise::spawn::spawn_into_main_thread(async move {
                        match ClientDomain::reattach(
                            local_domain_id,
                            generation,
                            reattach_ui.clone(),
                        )
                        .await
                        {
                            Ok(mark_ready) => {
                                if mark_ready {
                                    phase.store(
                                        ClientConnectionPhase::Ready as u8,
                                        Ordering::Release,
                                    );
                                    log::info!("Reconnected and restored generation {generation}");
                                } else {
                                    log::info!(
                                        "Replacement mux topology restored for generation \
                                         {generation}; waiting for frontend geometry"
                                    );
                                }
                                reattach_ui.close();
                                crate::domain::wake_thinkterm_frontend();
                            }
                            Err(err) => {
                                log::error!("reattach failed for generation {generation}: {err:#}");
                                if let Ok(inner) =
                                    ClientDomain::get_client_inner_for_domain(local_domain_id)
                                {
                                    inner.client.abort_connection_generation(
                                        generation,
                                        format!("{err:#}"),
                                    );
                                }
                            }
                        }
                    })
                    .detach();
                }
                if let Err(e) = client_thread(
                    &mut reconnectable,
                    local_domain_id,
                    generation,
                    &mut receiver,
                ) {
                    if !reconnectable.reconnectable() || local_domain_id.is_none() {
                        log::debug!("client thread ended: {}", e);
                        break;
                    }

                    let local_domain_id = local_domain_id.expect("checked above");

                    if let Some(ioerr) = e.root_cause().downcast_ref::<std::io::Error>() {
                        if let std::io::ErrorKind::UnexpectedEof = ioerr.kind() {
                            if !reconnectable.reconnect_on_eof() {
                                // Don't reconnect for a simple EOF
                                log::error!("server closed connection ({})", e);
                                break;
                            }
                        }
                    }

                    if let Some(err) = e.root_cause().downcast_ref::<NotReconnectableError>() {
                        log::error!("{}; won't try to reconnect", err);
                        break;
                    }

                    // A fatal error reported by the reattach task (codec
                    // version mismatch). The transport itself reconnects
                    // fine, so retrying would cycle forever with the real
                    // error visible only to a headless UI; show it in a
                    // window of its own and stop.
                    if let Some(reason) = reader_fatal_connection_error.lock().unwrap().take() {
                        log::error!("{reason}; won't try to reconnect");
                        let ui = ConnectionUI::new_with_no_close_delay();
                        ui.title("ThinkTerm: connection failed");
                        ui.output_str(&format!("{reason}\n"));
                        break;
                    }

                    // Whether the session that just ended had been restored
                    // and marked usable; one that died while registering,
                    // or whose reattach was refused, never was.
                    let was_ready = reader_connection_phase.load(Ordering::Acquire)
                        == ClientConnectionPhase::Ready as u8;
                    reader_connection_phase
                        .store(ClientConnectionPhase::Reconnecting as u8, Ordering::Release);
                    crate::domain::wake_thinkterm_frontend();

                    let session_lasted = session_started.elapsed() >= SHORT_SESSION;
                    if session_lasted {
                        // The previous connection genuinely worked; restart
                        // the retry schedule from scratch.
                        short_sessions = 0;
                        backoff = BASE_INTERVAL;
                    } else {
                        short_sessions += 1;
                        // A session that dies at once is retried with the
                        // same backoff as a transport that never comes up;
                        // otherwise a refused restore was retried every
                        // BASE_INTERVAL for as long as the process lived.
                        backoff = (backoff + backoff).min(MAX_INTERVAL);
                    }
                    // A session that was restored ends the outage, however
                    // briefly it lived: a flapping link keeps being retried,
                    // as it always was. One that never became usable keeps
                    // the clock running -- unless it stayed up longer than
                    // the whole give-up window, which no refused restore
                    // does; that is a connection that worked without ever
                    // being marked so, and its drop is a new outage, not the
                    // hours-old one parking us on the first try.
                    outage_started = outage_clock_after_session(
                        outage_started,
                        was_ready,
                        session_started.elapsed(),
                        GIVE_UP_AFTER,
                    );
                    let outage_started = outage_started.get_or_insert_with(std::time::Instant::now);

                    // A successful reattach closes the UI behind our back;
                    // detect that so we build a fresh one when needed.
                    if reconnect_ui.as_ref().map_or(false, |ui| !ui.test_alive()) {
                        reconnect_ui = None;
                    }

                    loop {
                        // Reconnect silently. The outage is already on screen
                        // four ways — an opaque overlay across every pane of
                        // this domain, the orange sidebar Space icon, the
                        // sidebar Reconnect row once we suspend, and the Space
                        // menu group header — so a progress UI here was only
                        // ever a fifth copy, and one that stole the user's
                        // active tab or opened a window over a frozen session.
                        // The lazy UI grows a window only if ssh genuinely
                        // needs a password or a host key confirmed.
                        let mut ui = match &reconnect_ui {
                            Some(ui) => ui.clone(),
                            None => {
                                let ui = ConnectionUI::new_lazy(mux::connui::ConnectionUIParams {
                                    host_domain_id: Some(local_domain_id),
                                    ..Default::default()
                                });
                                ui.title("ThinkTerm: Reconnecting...");
                                reconnect_ui = Some(ui.clone());
                                ui
                            }
                        };

                        // Set when we should stop retrying and park, rather
                        // than keep going or tear the domain down.
                        let mut suspend: Option<String> = None;

                        // Judged before the attempt, on how long the domain
                        // has been unusable: the network staying down and a
                        // transport that reconnects only for the restore to
                        // fail both count.
                        if outage_started.elapsed() >= GIVE_UP_AFTER {
                            if reconnectable.is_local_session_host() {
                                // The local session host keeps trying at the
                                // capped interval: its Space is a local one
                                // with no Reconnect row, so a parked retry
                                // could never be resumed. And a server that
                                // kept dying at once gets started again after
                                // this long; nothing else would ever try.
                                log::warn!(
                                    "the session server has been unreachable for \
                                     {GIVE_UP_AFTER:?}; starting it again if nothing answers"
                                );
                                short_sessions = 0;
                                *outage_started = std::time::Instant::now();
                            } else {
                                suspend =
                                    Some(format!("unable to reconnect for {GIVE_UP_AFTER:?}"));
                            }
                        }

                        if suspend.is_some() {
                            // Parked below; no further attempt this round.
                        } else if ui
                            .sleep_with_reason(
                                &format!("client disconnected {}; will reconnect", e),
                                backoff,
                            )
                            .is_err()
                        {
                            // Only reachable once a prompt window exists and
                            // the user dismissed it; the silent path always
                            // answers a sleep with Ok.
                            suspend = Some("reconnect prompt dismissed".to_string());
                        } else {
                            let initial = false;
                            // Normally a reconnect must not auto-start a
                            // server: during a network blip the server is
                            // alive and would be fought by a second instance.
                            // But when freshly established sessions keep dying
                            // instantly, the ssh hop is fine and it is the
                            // remote mux server that is gone (e.g. the host
                            // rebooted); refusing auto-start then loops
                            // forever without ever converging. Let the proxy
                            // revive the server after a couple of instant
                            // deaths — it still only spawns one if connecting
                            // to the existing socket fails.
                            // The local session host is started again
                            // whenever nothing answers -- unless what was
                            // started keeps dying at once, which no restart
                            // fixes; then the backoff and the give-up apply.
                            let no_auto_start = if reconnectable.is_local_session_host() {
                                short_sessions >= 3
                            } else {
                                short_sessions < 2
                            };
                            match reconnectable.connect(initial, &mut ui, no_auto_start) {
                                Ok(_) => {
                                    log::info!("Transport reconnected; restoring mux session");
                                    reader_connection_phase.store(
                                        ClientConnectionPhase::Registering as u8,
                                        Ordering::Release,
                                    );
                                    pending_reattach_ui = Some(ui.clone());
                                    crate::domain::wake_thinkterm_frontend();
                                    break;
                                }
                                Err(err) => {
                                    backoff = (backoff + backoff).min(MAX_INTERVAL);
                                    ui.output_str(&format!(
                                        "problem reconnecting: {}; will reconnect in {:?}\n",
                                        err, backoff
                                    ));
                                    if is_auth_cancelled(&err) {
                                        // We asked and they declined; another
                                        // attempt would only ask again.
                                        suspend = Some("authentication was declined".to_string());
                                    }
                                }
                            }
                        }

                        if let Some(reason) = suspend {
                            log::error!("{reason}; suspending retries until requested");
                            if let Some(ui) = reconnect_ui.take() {
                                ui.close();
                            }
                            reader_connection_phase
                                .store(ClientConnectionPhase::Suspended as u8, Ordering::Release);
                            crate::domain::wake_thinkterm_frontend();
                            match resume_reconnect_rx.recv() {
                                Ok(()) => {
                                    reader_connection_phase.store(
                                        ClientConnectionPhase::Reconnecting as u8,
                                        Ordering::Release,
                                    );
                                    *outage_started = std::time::Instant::now();
                                    backoff = BASE_INTERVAL;
                                    short_sessions = 0;
                                }
                                Err(_) => {
                                    // Every Client handle is gone; nobody can
                                    // ever resume us.
                                    log::error!(
                                        "reconnect suspended and client dropped; detaching"
                                    );
                                    break 'client;
                                }
                            }
                        }
                    }
                } else {
                    log::error!("client_thread returned without any error condition");
                    break;
                }
            }

            // Whatever ended the loop (not reconnectable, or every Client
            // handle gone), don't leave the reconnect UI behind. The domain
            // detaches below, so we are no longer "reconnecting".
            reader_connection_phase.store(ClientConnectionPhase::Detached as u8, Ordering::Release);
            if let Some(ui) = reconnect_ui.take() {
                ui.close();
            }

            async fn detach(local_domain_id: DomainId, client_id: ClientId) -> anyhow::Result<()> {
                if let Some(mux) = Mux::try_get() {
                    let client_domain = mux
                        .get_domain(local_domain_id)
                        .ok_or_else(|| anyhow!("no such domain {}", local_domain_id))?;
                    let client_domain =
                        client_domain
                            .downcast_ref::<ClientDomain>()
                            .ok_or_else(|| {
                                anyhow!("domain {} is not a ClientDomain instance", local_domain_id)
                            })?;
                    // A client that was replaced while it was still alive --
                    // the attach path does that after updating a remote
                    // server -- must not tear down the domain its successor
                    // now owns.
                    let owned = client_domain
                        .inner()
                        .map(|inner| inner.client.client_id == client_id)
                        .unwrap_or(true);
                    if owned {
                        client_domain.perform_detach();
                    } else {
                        log::info!(
                            "domain {} is now attached through another client; \
                             the superseded client leaves it alone",
                            local_domain_id
                        );
                    }
                }
                Ok(())
            }
            if let Some(domain_id) = local_domain_id {
                let client_id = reader_client_id;
                promise::spawn::spawn_into_main_thread(async move {
                    detach(domain_id, client_id).await.ok();
                })
                .detach();
            }
        });

        Self {
            sender,
            local_domain_id,
            is_reconnectable,
            is_local,
            client_id,
            client_domain_config,
            connection_phase,
            resume_reconnect_tx,
            remote_server_id,
            remote_os_release,
            connection_generation,
            fatal_connection_error,
        }
    }

    /// Mark the connection as failed in a way no amount of retrying can fix.
    /// The reconnect loop surfaces the reason and stops after the current
    /// session dies.
    pub fn set_fatal_connection_error(&self, reason: String) {
        self.fatal_connection_error.lock().unwrap().replace(reason);
    }

    /// See `ClientDomainConfig::is_local_session_host`.
    pub fn is_local_session_host(&self) -> bool {
        self.client_domain_config.is_local_session_host()
    }

    pub fn into_client_domain_config(self) -> ClientDomainConfig {
        self.client_domain_config
    }

    pub async fn verify_version_compat(
        &self,
        ui: &ConnectionUI,
    ) -> anyhow::Result<GetCodecVersionResponse> {
        match self
            .send_bootstrap_pdu(Pdu::GetCodecVersion(GetCodecVersion {}))
            .or(async {
                smol::Timer::after(Duration::from_secs(60)).await;
                Err(Timeout).context("Timeout")
            })
            .await
        {
            Ok(Pdu::GetCodecVersionResponse(info)) if info.codec_vers == CODEC_VERSION => {
                log::trace!(
                    "Server version is {} (codec version {})",
                    info.version_string,
                    info.codec_vers
                );
                // A domain whose socket leads back to this very process. A
                // config shared between a mux server and its clients lists
                // the unix domain the server serves, and the server holds a
                // client domain for it too. Attaching it would mirror every
                // window this mux has as a new window in the same mux, which
                // the mirror then mirrors again, without end.
                let own_id = mux::Mux::try_get().map(|mux| mux.runtime_server_id().to_string());
                if leads_back_to_this_process(own_id.as_deref(), &info.server_id) {
                    anyhow::bail!(
                        "refusing to attach: this connection leads back to this very process \
                         (server id {})",
                        info.server_id
                    );
                }
                if let Some(mismatch) =
                    describe_server_build_mismatch(config::wezterm_version(), &info.version_string)
                {
                    log::warn!("{mismatch}");
                }
                match self
                    .send_bootstrap_pdu(Pdu::SetClientId(SetClientId {
                        client_id: self.client_id.clone(),
                        is_proxy: false,
                    }))
                    .or(async {
                        // Without this, a server that answers the version
                        // query and then wedges leaves the attach hanging
                        // forever.
                        smol::Timer::after(Duration::from_secs(60)).await;
                        Err(anyhow::Error::new(VersionHandshakeStalled {
                            timeout_secs: 60,
                        }))
                        .context("timed out waiting for SetClientId acknowledgement")
                    })
                    .await?
                {
                    Pdu::UnitResponse(_) => {}
                    Pdu::ErrorResponse(err) => anyhow::bail!(err.reason),
                    response => anyhow::bail!(
                        "unexpected SetClientId response during bootstrap: {response:?}"
                    ),
                }
                *self.remote_server_id.write().unwrap() = Some(info.server_id.clone());
                // Best effort, and its own request: see GetServerOsRelease for
                // why this cannot ride the version response. A server that
                // does not answer simply leaves the distro unknown.
                if let Ok(Pdu::GetServerOsReleaseResponse(os)) = self
                    .send_bootstrap_pdu(Pdu::GetServerOsRelease(GetServerOsRelease {}))
                    .or(async {
                        smol::Timer::after(Duration::from_secs(10)).await;
                        Err(Timeout).context("Timeout")
                    })
                    .await
                {
                    *self.remote_os_release.write().unwrap() = os.os_release_id;
                }
                self.mark_registration_complete()?;
                Ok(info)
            }
            Ok(Pdu::GetCodecVersionResponse(info)) => {
                let err = IncompatibleVersionError {
                    version: info.version_string,
                    codec_vers: info.codec_vers,
                };
                ui.output_str(&err.to_string());
                log::error!("{:?}", err);
                return Err(err.into());
            }
            Ok(Pdu::ErrorResponse(err)) => anyhow::bail!(err.reason),
            Ok(response) => {
                anyhow::bail!("unexpected GetCodecVersion response during bootstrap: {response:?}")
            }
            Err(err) => {
                log::trace!("{:?}", err);
                // A timeout stays typed so the reattach path can classify
                // it as transient (retryable) rather than treating it like
                // an incompatibility.
                if err.root_cause().is::<Timeout>() {
                    let stalled = VersionHandshakeStalled { timeout_secs: 60 };
                    let msg = stalled.to_string();
                    ui.output_str(&msg);
                    log::error!("{msg}");
                    return Err(anyhow::Error::new(stalled));
                }
                let msg = describe_handshake_failure(&err);
                ui.output_str(&msg);
                bail!("{}", msg);
            }
        }
    }

    #[allow(dead_code)]
    pub fn local_domain_id(&self) -> Option<DomainId> {
        self.local_domain_id
    }

    pub fn resolve_default_unix_domain(
        prefer_mux: bool,
        class_name: &str,
    ) -> anyhow::Result<(config::UnixDomain, ResolvedVia)> {
        match std::env::var_os("WEZTERM_UNIX_SOCKET") {
            Some(path) if !path.is_empty() => Ok((
                config::UnixDomain {
                    socket_path: Some(path.into()),
                    ..Default::default()
                },
                ResolvedVia::Env,
            )),
            Some(_) | None => {
                if !prefer_mux {
                    if let Ok(gui) = crate::discovery::resolve_gui_sock_path(class_name) {
                        return Ok((
                            config::UnixDomain {
                                socket_path: Some(gui),
                                no_serve_automatically: true,
                                ..Default::default()
                            },
                            ResolvedVia::GuiSock,
                        ));
                    }
                }

                let config = configuration();
                Ok((
                    config
                        .unix_domains
                        .first()
                        .ok_or_else(|| {
                            anyhow!(
                                "no default unix domain is configured and WEZTERM_UNIX_SOCKET \
                                 is not set in the environment"
                            )
                        })?
                        .clone(),
                    ResolvedVia::ConfiguredDomain,
                ))
            }
        }
    }

    pub fn new_default_unix_domain(
        initial: bool,
        ui: &mut ConnectionUI,
        no_auto_start: bool,
        prefer_mux: bool,
        class_name: &str,
    ) -> anyhow::Result<Self> {
        let (unix_dom, via) = Self::resolve_default_unix_domain(prefer_mux, class_name)?;
        let gui_err = match Self::new_unix_domain(None, &unix_dom, initial, ui, no_auto_start) {
            Ok(client) => return Ok(client),
            // An explicit WEZTERM_UNIX_SOCKET, or the domain the user
            // configured, is the answer whether or not it answers: only the
            // published GUI socket is a guess we are allowed to walk back.
            Err(err) if via != ResolvedVia::GuiSock => return Err(err),
            Err(err) => err,
        };

        log::warn!(
            "the published GUI socket {:?} did not answer: {gui_err:#}; \
             falling back to the configured mux server",
            unix_dom.socket_path
        );
        ui.output_str(
            "The published GUI socket did not answer; \
             trying the configured mux server.\n",
        );

        let (fallback, _) = Self::resolve_default_unix_domain(true, class_name)?;
        Self::new_unix_domain(None, &fallback, initial, ui, no_auto_start)
            .with_context(|| format!("after the published GUI socket failed ({gui_err:#})"))
    }

    pub fn new_unix_domain(
        local_domain_id: Option<DomainId>,
        unix_dom: &UnixDomain,
        initial: bool,
        ui: &mut ConnectionUI,
        no_auto_start: bool,
    ) -> anyhow::Result<Self> {
        let mut reconnectable =
            Reconnectable::new(ClientDomainConfig::Unix(unix_dom.clone()), None);
        reconnectable.connect(initial, ui, no_auto_start)?;
        Ok(Self::new(local_domain_id, reconnectable))
    }

    pub fn new_tls(
        local_domain_id: DomainId,
        tls_client: &TlsDomainClient,
        ui: &mut ConnectionUI,
    ) -> anyhow::Result<Self> {
        let mut reconnectable =
            Reconnectable::new(ClientDomainConfig::Tls(tls_client.clone()), None);
        let no_auto_start = true;
        reconnectable.connect(true, ui, no_auto_start)?;
        Ok(Self::new(Some(local_domain_id), reconnectable))
    }

    pub fn new_ssh(
        local_domain_id: DomainId,
        ssh_dom: &SshDomain,
        ui: &mut ConnectionUI,
    ) -> anyhow::Result<Self> {
        let mut reconnectable = Reconnectable::new(ClientDomainConfig::Ssh(ssh_dom.clone()), None);
        let no_auto_start = true;
        reconnectable.connect(true, ui, no_auto_start)?;
        Ok(Self::new(Some(local_domain_id), reconnectable))
    }

    pub async fn send_pdu(&self, pdu: Pdu) -> anyhow::Result<Pdu> {
        self.send_pdu_with_registration_requirement(pdu, true).await
    }

    async fn send_bootstrap_pdu(&self, pdu: Pdu) -> anyhow::Result<Pdu> {
        self.send_pdu_with_registration_requirement(pdu, false)
            .await
    }

    /// Put `pdu` on the wire now, behind everything sent before it, and
    /// hand back its answer to await later. `send_pdu` waits for the answer
    /// in place; a caller that has to keep several requests in order cannot
    /// wait for each answer before sending the next, so this splits the two:
    /// the send is done when this returns.
    pub fn send_pdu_pipelined(
        &self,
        pdu: Pdu,
    ) -> impl std::future::Future<Output = anyhow::Result<Pdu>> + Send + 'static {
        log::trace!("send_pdu_pipelined {}", pdu.pdu_name());
        let (promise, rx) = bounded(1);
        // The channel is unbounded: try_send fails only once the reader is
        // gone, which is the same failure `send_pdu` reports.
        let sent = self
            .sender
            .try_send(ReaderMessage::SendPdu {
                pdu,
                promise,
                registration_required: true,
            })
            .map_err(|_| ChannelSendError)
            .context("send_pdu send");
        async move {
            sent?;
            rx.recv().await.context("send_pdu recv")?
        }
    }

    async fn send_pdu_with_registration_requirement(
        &self,
        pdu: Pdu,
        registration_required: bool,
    ) -> anyhow::Result<Pdu> {
        log::trace!("send_pdu {}", pdu.pdu_name());
        let (promise, rx) = bounded(1);
        self.sender
            .send(ReaderMessage::SendPdu {
                pdu,
                promise,
                registration_required,
            })
            .await
            .map_err(|_| ChannelSendError)
            .context("send_pdu send")?;
        rx.recv().await.context("send_pdu recv")?
    }

    pub async fn resolve_pane_id(&self, pane_id: Option<PaneId>) -> anyhow::Result<PaneId> {
        let pane_id: PaneId = match pane_id {
            Some(p) => p,
            None => {
                if let Ok(pane) = std::env::var("WEZTERM_PANE") {
                    pane.parse()?
                } else {
                    let mut clients = self.list_clients().await?.clients;
                    clients.retain(|client| client.focused_pane_id.is_some());
                    clients.sort_by(|a, b| b.last_input.cmp(&a.last_input));
                    if clients.is_empty() {
                        anyhow::bail!(
                            "--pane-id was not specified and $WEZTERM_PANE
                         is not set in the environment, and I couldn't
                         determine which pane was currently focused"
                        );
                    }

                    clients[0]
                        .focused_pane_id
                        .expect("to have filtered out above")
                }
            }
        };
        Ok(pane_id)
    }

    rpc!(ping, Ping = (), Pong);
    rpc!(list_panes, ListPanes = (), ListPanesResponse);
    rpc!(
        get_agent_statuses,
        GetAgentStatuses = (),
        GetAgentStatusesResponse
    );
    rpc!(
        get_foreground_programs,
        GetForegroundPrograms = (),
        GetForegroundProgramsResponse
    );
    rpc!(spawn_v2, SpawnV2, SpawnResponse);
    rpc!(split_pane, SplitPane, SpawnResponse);
    rpc!(spawn_pane_in_stack, SpawnPaneInStack, SpawnResponse);
    rpc!(activate_pane_in_stack, ActivatePaneInStack, UnitResponse);
    rpc!(move_pane_to_stack, MovePaneToStack, UnitResponse);
    rpc!(
        move_pane_to_new_tab,
        MovePaneToNewTab,
        MovePaneToNewTabResponse
    );
    rpc!(
        get_thinkterm_tree,
        GetThinkTermTree = (),
        ThinkTermTreeState
    );
    rpc!(
        mutate_thinkterm_tree,
        MutateThinkTermTree,
        ThinkTermTreeState
    );
    rpc!(
        get_thinkterm_session_state,
        GetThinkTermSessionState = (),
        ThinkTermSessionState
    );
    rpc!(
        ensure_thinkterm_thread,
        EnsureThinkTermThread,
        EnsureThinkTermThreadResponse
    );
    rpc!(set_client_viewport, SetClientViewport, ClientViewportState);
    rpc!(set_client_view, SetClientView, UnitResponse);
    rpc!(
        claim_client_viewport,
        ClaimClientViewport,
        ClientViewportState
    );
    rpc!(
        set_frontend_access_mode,
        SetFrontendAccessMode,
        FrontendAccessState
    );
    rpc!(write_to_pane, WriteToPane, UnitResponse);
    rpc!(send_paste, SendPaste, UnitResponse);
    rpc!(key_down, SendKeyDown, UnitResponse);
    rpc!(mouse_event, SendMouseEvent, UnitResponse);
    rpc!(resize, Resize, UnitResponse);
    rpc!(set_zoomed, SetPaneZoomed, UnitResponse);
    rpc!(move_tab, MoveTab, UnitResponse);
    rpc!(activate_pane_direction, ActivatePaneDirection, UnitResponse);
    rpc!(
        get_pane_render_changes,
        GetPaneRenderChanges,
        LivenessResponse
    );
    rpc!(get_lines, GetLines, GetLinesResponse);
    rpc!(
        get_dimensions,
        GetPaneRenderableDimensions,
        GetPaneRenderableDimensionsResponse
    );
    rpc!(get_codec_version, GetCodecVersion, GetCodecVersionResponse);
    rpc!(get_tls_creds, GetTlsCreds = (), GetTlsCredsResponse);
    rpc!(web_token_mint, WebTokenMint, WebTokenMintResponse);
    rpc!(web_token_list, WebTokenList = (), WebTokenListResponse);
    rpc!(web_token_revoke, WebTokenRevoke, WebTokenRevokeResponse);
    rpc!(get_web_server_status, GetWebServerStatus = (), WebServerStatus);
    rpc!(set_web_server, SetWebServer, WebServerStatus);
    rpc!(
        search_scrollback,
        SearchScrollbackRequest,
        SearchScrollbackResponse
    );
    rpc!(kill_pane, KillPane, UnitResponse);
    rpc!(set_client_id, SetClientId, UnitResponse);
    rpc!(list_clients, GetClientList = (), GetClientListResponse);
    rpc!(set_window_workspace, SetWindowWorkspace, UnitResponse);
    rpc!(set_focused_pane_id, SetFocusedPane, UnitResponse);
    rpc!(get_image_cell, GetImageCell, GetImageCellResponse);
    rpc!(set_configured_palette_for_pane, SetPalette, UnitResponse);
    rpc!(set_tab_title, TabTitleChanged, UnitResponse);
    rpc!(set_window_title, WindowTitleChanged, UnitResponse);
    rpc!(rename_workspace, RenameWorkspace, UnitResponse);
    rpc!(erase_scrollback, EraseScrollbackRequest, UnitResponse);
    rpc!(
        get_pane_direction,
        GetPaneDirection,
        GetPaneDirectionResponse
    );
    rpc!(adjust_pane_size, AdjustPaneSize, UnitResponse);
}

/// The outage clock to carry into the next reconnect round, given how the
/// session that just ended fared. `None` means no outage is in progress.
fn outage_clock_after_session(
    prior: Option<std::time::Instant>,
    was_ready: bool,
    session_length: Duration,
    give_up_after: Duration,
) -> Option<std::time::Instant> {
    if was_ready || session_length >= give_up_after {
        None
    } else {
        prior
    }
}

#[cfg(test)]
mod tests {
    use super::{is_auth_cancelled, outage_clock_after_session, Reconnectable};
    use crate::domain::ClientDomainConfig;
    use std::time::{Duration, Instant};

    #[test]
    fn a_restored_session_ends_the_outage() {
        let old = Some(Instant::now() - Duration::from_secs(3600));
        assert_eq!(
            outage_clock_after_session(old, true, Duration::from_secs(3), Duration::from_secs(120)),
            None
        );
    }

    #[test]
    fn a_short_refused_session_keeps_the_clock_running() {
        let old = Some(Instant::now() - Duration::from_secs(3600));
        assert_eq!(
            outage_clock_after_session(old, false, Duration::from_secs(3), Duration::from_secs(120)),
            old
        );
    }

    #[test]
    fn a_long_session_that_was_never_marked_ready_still_ends_the_outage() {
        let old = Some(Instant::now() - Duration::from_secs(3600));
        assert_eq!(
            outage_clock_after_session(
                old,
                false,
                Duration::from_secs(600),
                Duration::from_secs(120)
            ),
            None
        );
    }

    #[test]
    fn only_the_local_session_host_reconnects_over_a_unix_socket() {
        let plain = Reconnectable::new(
            ClientDomainConfig::Unix(config::UnixDomain::default()),
            None,
        );
        let mut plain = plain;
        assert!(!plain.reconnectable());
        assert!(!plain.reconnect_on_eof());
        assert!(!plain.is_local_session_host());

        let mut host = Reconnectable::new(
            ClientDomainConfig::Unix(config::UnixDomain {
                local_session_host: true,
                ..Default::default()
            }),
            None,
        );
        assert!(host.reconnectable());
        assert!(host.reconnect_on_eof());
        assert!(host.is_local_session_host());
    }

    #[test]
    fn auth_cancellation_survives_being_wrapped_in_context() {
        // The reconnect loop parks instead of retrying when it sees this, so
        // it has to keep matching however far up the stack it is re-wrapped.
        let err = anyhow::Error::new(mux::ssh::AuthCancelledError)
            .context("ssh_connect_with_ui_and_password")
            .context("reconnecting");
        assert!(is_auth_cancelled(&err));
    }

    #[test]
    fn an_ordinary_failure_is_not_auth_cancellation() {
        let err = anyhow::anyhow!("Connecting to host within 10s: timed out");
        assert!(!is_auth_cancelled(&err));
    }

    #[test]
    fn remote_mux_command_uses_configured_path() {
        assert_eq!(
            Reconnectable::remote_mux_command(
                &Some("/opt/bin/wezterm".to_string()),
                "cli tlscreds"
            ),
            "/opt/bin/wezterm cli tlscreds"
        );
    }

    #[test]
    fn remote_mux_command_defaults_to_thinkterm_with_wezterm_fallback() {
        let cmd = Reconnectable::remote_mux_command(&None, "cli --prefer-mux proxy");
        assert!(cmd.contains("thinkterm cli --prefer-mux proxy"), "{}", cmd);
        assert!(cmd.contains("wezterm cli --prefer-mux proxy"), "{}", cmd);
    }

    /// A refusal stays recognisable under the context a caller adds,
    /// and a transport failure is not mistaken for one.
    #[test]
    fn a_server_refusal_is_told_apart_from_a_transport_failure() {
        use anyhow::Context;
        let refused: anyhow::Result<()> = Err(super::RemoteRpcError {
            reason: "Error: pane 9 is not contained by viewport tab 6".to_string(),
        }
        .into());
        let refused = refused
            .with_context(|| "restoring viewport and access state for remote tab 6")
            .unwrap_err();
        assert!(super::RemoteRpcError::is_cause_of(&refused));
        assert_eq!(
            format!("{refused:#}"),
            "restoring viewport and access state for remote tab 6: \
             Error: pane 9 is not contained by viewport tab 6"
        );

        let dropped = anyhow::anyhow!("EOF while reading leb128 encoded value")
            .context("decoding a PDU");
        assert!(!super::RemoteRpcError::is_cause_of(&dropped));
    }
}
