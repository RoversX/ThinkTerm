//! User-initiated reverse Connect grants. No configuration, Lua or mux PDU
//! entry point invokes this controller.

use mux::domain::DomainId;
use mux::Mux;
use parking_lot::Mutex;
use std::collections::BTreeMap;
use std::time::Duration;
use thinkterm_reverse_access::{Error, GrantScope, Permission, Result};
use wezterm_client::connect::{ExportState, Exporter, HostBinding};
use wezterm_client::domain::ClientDomain;
use window::Window;

pub use mux::connect::service::Control;

pub struct Source {
    pub id: DomainId,
    pub name: String,
    pub eligible: bool,
}
pub struct HostChoice {
    pub id: Option<DomainId>,
    pub name: String,
}

pub fn sources() -> Vec<Source> {
    let mut sources = Vec::new();
    for domain in Mux::get().iter_domains() {
        let Some(client_domain) = domain.downcast_ref::<ClientDomain>() else {
            continue;
        };
        if client_domain.ssh_domain_config().is_none() {
            continue;
        }
        // A domain exists for every host in the SSH configuration.
        let Some(client) = client_domain.connect_client() else {
            continue;
        };
        let eligible = client.verified_connect_session().is_some();
        sources.push(Source {
            id: domain.domain_id(),
            name: thinkterm_reverse_access::protocol::display_text(domain.domain_name(), 96),
            eligible,
        });
    }
    sources.sort_by_key(|source| source.id);
    sources
}

pub fn hosts() -> Vec<HostChoice> {
    let mut hosts = Vec::new();
    for domain in Mux::get().iter_domains() {
        let Some(client_domain) = domain.downcast_ref::<ClientDomain>() else {
            continue;
        };
        if client_domain.is_local_session_host()
            && client_domain.unix_socket_path().is_some()
            && client_domain.connect_client().is_some()
        {
            hosts.push(HostChoice {
                id: Some(domain.domain_id()),
                name: crate::i18n::tr("settings-reverse-local-sessions"),
            });
        }
    }
    hosts.sort_by_key(|host| host.id);
    hosts.push(HostChoice {
        id: None,
        name: crate::i18n::tr("settings-reverse-this-window"),
    });
    hosts
}

struct Entry {
    source_id: DomainId,
    host_id: Option<DomainId>,
    source: String,
    host: String,
    exporter: Option<Exporter>,
    error: Option<Error>,
    starting: bool,
    initial: Option<Control>,
}

#[derive(Default)]
struct State {
    next: u64,
    entries: BTreeMap<u64, Entry>,
}
static STATE: Mutex<Option<State>> = Mutex::new(None);
static POLL: crate::page_poll::PollTimer = crate::page_poll::PollTimer::new();

fn with_state<T>(action: impl FnOnce(&mut State) -> T) -> T {
    action(STATE.lock().get_or_insert_with(State::default))
}

#[derive(Clone)]
pub struct View {
    pub id: u64,
    pub source_id: DomainId,
    pub source: String,
    pub host: String,
    pub state: ExportState,
    pub starting: bool,
}

pub fn views() -> Vec<View> {
    with_state(|state| {
        state
            .entries
            .iter()
            .map(|(id, entry)| {
                let mut view = entry
                    .exporter
                    .as_ref()
                    .map(Exporter::state)
                    .unwrap_or_default();
                if entry.error.is_some() {
                    view.error = entry.error;
                }
                View {
                    id: *id,
                    source_id: entry.source_id,
                    source: entry.source.clone(),
                    host: entry.host.clone(),
                    state: view,
                    starting: entry.starting,
                }
            })
            .collect()
    })
}

pub fn arm(window: Window) {
    if with_state(|state| !state.entries.is_empty()) {
        POLL.arm(window, Duration::from_secs(1));
    }
}

pub fn begin(source_id: DomainId, host_id: Option<DomainId>) -> Result<u64> {
    begin_scoped(source_id, host_id, GrantScope::SelectedTerminals, None)
}

pub fn begin_all_local(source_id: DomainId, permission: Permission, seconds: u64) -> Result<u64> {
    let default_id = Mux::get().default_domain().domain_id();
    let hosts = hosts();
    let host_id = hosts
        .iter()
        .find(|host| host.id == Some(default_id))
        .or_else(|| hosts.first())
        .and_then(|host| host.id);
    begin_scoped(
        source_id,
        host_id,
        GrantScope::AllLocalTerminals,
        Some(Control::ApproveAllLocal {
            permission,
            seconds,
            renewal: None,
        }),
    )
}

fn begin_scoped(
    source_id: DomainId,
    host_id: Option<DomainId>,
    scope: GrantScope,
    initial: Option<Control>,
) -> Result<u64> {
    if let Some(id) = with_state(|state| {
        state.entries.iter().find_map(|(id, entry)| {
            (entry.source_id == source_id
                && entry.host_id == host_id
                && (entry.starting
                    || entry
                        .exporter
                        .as_ref()
                        .is_some_and(|exporter| !exporter.state().closed)))
            .then_some(*id)
        })
    }) {
        return Ok(id);
    }
    let mux = Mux::get();
    let domain = mux.get_domain(source_id).ok_or(Error::Unavailable)?;
    let session = domain
        .downcast_ref::<ClientDomain>()
        .and_then(ClientDomain::connect_client)
        .and_then(|client| client.verified_connect_session())
        .ok_or(Error::Unavailable)?;
    let source = thinkterm_reverse_access::protocol::display_text(domain.domain_name(), 96);
    let (host_client, host_name) = match host_id {
        Some(id) => {
            let domain = mux.get_domain(id).ok_or(Error::Unavailable)?;
            let domain = domain
                .downcast_ref::<ClientDomain>()
                .ok_or(Error::Unavailable)?;
            if !domain.is_local_session_host() || domain.unix_socket_path().is_none() {
                return Err(Error::Unavailable);
            }
            (
                Some(domain.connect_client().ok_or(Error::Unavailable)?),
                crate::i18n::tr("settings-reverse-local-sessions"),
            )
        }
        None => (None, crate::i18n::tr("settings-reverse-this-window")),
    };
    let id = with_state(|state| {
        if state.entries.len() >= thinkterm_reverse_access::MAX_ROUTES {
            return Err(Error::Busy);
        }
        state.next = state.next.checked_add(1).ok_or(Error::Busy)?;
        state.entries.insert(
            state.next,
            Entry {
                source_id,
                host_id,
                source,
                host: host_name,
                exporter: None,
                error: None,
                starting: true,
                initial,
            },
        );
        Ok(state.next)
    })?;
    promise::spawn::spawn_into_main_thread(async move {
        let result = async {
            let binding = match host_client {
                Some(client) => HostBinding::local_session(client).await?,
                None => HostBinding::in_process(&mux)?,
            };
            let language = crate::i18n::current_locale().to_string();
            let exporter = match scope {
                GrantScope::SelectedTerminals => Exporter::start(session, binding, language).await,
                GrantScope::AllLocalTerminals => {
                    Exporter::start_all_local(session, binding, language).await
                }
            }?;
            let initial =
                with_state(|state| state.entries.get_mut(&id).map(|entry| entry.initial.take()))
                    .ok_or(Error::Unavailable)?;
            if let Some(command) = initial {
                exporter.command(command)?;
            }
            Ok(exporter)
        }
        .await;
        with_state(|state| {
            if let Some(entry) = state.entries.get_mut(&id) {
                entry.starting = false;
                match result {
                    Ok(exporter) => entry.exporter = Some(exporter),
                    Err(error) => entry.error = Some(error),
                }
            }
        });
        crate::settings_window::invalidate_open_settings_window();
    })
    .detach();
    Ok(id)
}

pub fn command(id: u64, command: Control) -> Result<()> {
    with_state(|state| {
        let entry = state.entries.get_mut(&id).ok_or(Error::Unavailable)?;
        if entry.starting && matches!(command, Control::Cancel) {
            entry.initial = None;
            return Ok(());
        }
        entry
            .exporter
            .as_ref()
            .ok_or(Error::Unavailable)?
            .command(command)
    })
}

pub fn close(id: u64) {
    with_state(|state| {
        state.entries.remove(&id);
    });
}

/// Leaving the editor cancels its unfinished authentication. Active grants
/// stay visible in the process-wide list until expiry or explicit revocation.
pub fn leave(id: u64) {
    with_state(|state| {
        if let Some(entry) = state.entries.get(&id) {
            if let Some(exporter) = &entry.exporter {
                if !exporter.state().status.grants.is_empty() {
                    let _ = exporter.command(Control::Cancel);
                    return;
                }
            }
        }
        state.entries.remove(&id);
    });
}
