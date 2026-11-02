//! ThinkTerm SSH host address book and runtime host -> mux domain plumbing.
//!
//! `workspace_threads.json` owns workspace/thread layout only. User-created SSH hosts
//! live in `ssh_hosts.json`; system `~/.ssh/config` hosts are exposed as
//! read-only entries. Runtime domains are still registered lazily with the mux
//! via [`mux::Mux::add_domain`].

use anyhow::{bail, Context, Result};
use config::keyassignment::{SpawnCommand, SpawnTabDomain};
use config::SshDomain;
use filedescriptor::FileDescriptor;
use mux::domain::Domain;
use mux::shell_integration::{NotConnected, ShellIntegrationOutcome, UnwritableStartupFile};
use mux::ssh::RemoteSshDomain;
use mux::Mux;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use wezterm_ssh::{Session, SessionEvent};

pub use thinkterm_core::ssh_hosts::{
    default_mosh_server_command, parse_other_addresses, ShellIntegrationFailure, SshHostEntry,
    SshHostId, SshHostSource, SshHostSpec, SshNetwork, DEFAULT_MOSH_SERVER_COMMAND,
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct SshHostRecord {
    id: SshHostId,
    spec: SshHostSpec,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct SshHostStore {
    #[serde(default = "store_version")]
    version: u32,
    #[serde(default)]
    hosts: Vec<SshHostRecord>,
}

impl Default for SshHostStore {
    fn default() -> Self {
        Self {
            version: store_version(),
            hosts: vec![],
        }
    }
}

fn store_version() -> u32 {
    1
}

lazy_static::lazy_static! {
    static ref SSH_HOST_STORE: Mutex<Option<SshHostStore>> = Mutex::new(None);
    /// Renames saved while connected under the old name, as (old name, new
    /// name): the host's Spaces stay with that connection while it lasts
    /// (`connect_name_after_rename`). One entry per host at most, gone once
    /// its Spaces have moved.
    static ref RENAMED_WHILE_CONNECTED: Mutex<Vec<(String, String)>> = Mutex::new(vec![]);
}

fn ensure_store_loaded(
    cached: &mut Option<SshHostStore>,
    load: impl FnOnce() -> Result<SshHostStore>,
) -> Result<()> {
    if cached.is_none() {
        *cached = Some(load()?);
    }
    Ok(())
}

fn host_store() -> Result<parking_lot::MappedMutexGuard<'static, SshHostStore>> {
    let mut store = SSH_HOST_STORE.lock();
    ensure_store_loaded(&mut store, load_ssh_host_store).map_err(|err| {
        log::warn!("failed to load ThinkTerm SSH host store: {err:#}");
        err
    })?;
    Ok(parking_lot::MutexGuard::map(store, |store| store.as_mut().unwrap()))
}

pub fn ssh_hosts_store_path() -> Result<PathBuf> {
    thinkterm_core::ssh_hosts::saved_hosts_path()
}

fn load_ssh_host_store() -> Result<SshHostStore> {
    load_ssh_host_store_from_path(&ssh_hosts_store_path()?)
}

/// Whether `path` reads as a host catalog, as an imported backup must.
pub(crate) fn check_ssh_host_store(path: &Path) -> Result<()> {
    load_ssh_host_store_from_path(path).map(drop)
}

fn load_ssh_host_store_from_path(path: &Path) -> Result<SshHostStore> {
    if !path.exists() {
        return Ok(SshHostStore::default());
    }
    let text = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))
}

fn save_ssh_host_store(store: &SshHostStore) -> Result<()> {
    save_ssh_host_store_to_path(&ssh_hosts_store_path()?, store)
}

fn save_ssh_host_store_to_path(path: &Path, store: &SshHostStore) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
        let mut file = tempfile::NamedTempFile::new_in(parent)
            .with_context(|| format!("create temporary SSH host store in {}", parent.display()))?;
        serde_json::to_writer_pretty(&mut file, store)
            .with_context(|| format!("write {}", path.display()))?;
        file.flush()
            .with_context(|| format!("flush {}", path.display()))?;
        file.as_file()
            .sync_all()
            .with_context(|| format!("sync {}", path.display()))?;
        file.persist(path)
            .with_context(|| format!("replace {}", path.display()))?;
        return Ok(());
    }

    let mut file = fs::File::create(path).with_context(|| format!("create {}", path.display()))?;
    serde_json::to_writer_pretty(&mut file, store)
        .with_context(|| format!("write {}", path.display()))?;
    file.flush()
        .with_context(|| format!("flush {}", path.display()))?;
    file.sync_all()
        .with_context(|| format!("sync {}", path.display()))
}

pub fn list_hosts() -> Vec<(SshHostId, SshHostSpec)> {
    let Ok(store) = host_store() else { return vec![]; };
    store
        .hosts
        .iter()
        .map(|record| (record.id.clone(), record.spec.clone()))
        .collect()
}

pub fn list_all_hosts() -> Vec<SshHostEntry> {
    list_all_hosts_with(list_system_hosts())
}

/// [`list_all_hosts`] around a [`list_system_hosts`] the caller kept.
pub fn list_all_hosts_with(system_hosts: Vec<SshHostEntry>) -> Vec<SshHostEntry> {
    let mut entries: Vec<_> = list_hosts()
        .into_iter()
        .map(|(id, spec)| SshHostEntry {
            id,
            source: SshHostSource::ThinkTerm,
            spec,
        })
        .collect();
    entries.extend(system_hosts);
    entries
}

pub fn host_spec(host_id: &str) -> Option<SshHostSpec> {
    if let Ok(store) = host_store() {
        if let Some(record) = store.hosts.iter().find(|record| record.id == host_id) {
            return Some(record.spec.clone());
        }
    }
    list_system_hosts()
        .into_iter()
        .find(|entry| entry.id == host_id)
        .map(|entry| entry.spec)
}

/// The saved record of `host_id` as the loaded store holds it now -- what
/// another window saved since a list of hosts was read -- without loading
/// the store when it is not.
pub fn saved_host_spec_if_loaded(host_id: &str) -> Option<SshHostSpec> {
    let store = SSH_HOST_STORE.lock();
    store
        .as_ref()?
        .hosts
        .iter()
        .find(|record| record.id == host_id)
        .map(|record| record.spec.clone())
}

pub fn host_exists(host_id: &str) -> bool {
    host_store().map(|store| store.hosts.iter().any(|record| record.id == host_id)).unwrap_or(false)
}

pub fn try_create_host(spec: SshHostSpec) -> Result<SshHostId> {
    // Refuse rather than replace. `upsert_host` matches on an id derived from
    // the endpoint, so saving a *new* host that dials an endpoint some other
    // host already uses used to overwrite that host's whole record -- its
    // saved password included -- and leave one card behind wearing the new
    // name.
    if host_store()?.hosts.iter().any(|record| record.id == host_id_for_host(&spec)) {
        bail!("a saved host already uses {}", endpoint(&spec));
    }
    let (host_id, spec) = upsert_host(spec)?;
    register_ssh_domain(&spec)?;
    Ok(host_id)
}

pub fn try_import_legacy_host(spec: SshHostSpec) -> Result<SshHostId> {
    let mut store = host_store()?;
    let host_id = host_id_for_host(&spec);
    if store.hosts.iter().any(|record| record.id == host_id) {
        return Ok(host_id);
    }
    store.hosts.push(SshHostRecord {
        id: host_id.clone(),
        spec,
    });
    save_ssh_host_store(&store)?;
    Ok(host_id)
}

fn upsert_host(spec: SshHostSpec) -> Result<(SshHostId, SshHostSpec)> {
    let mut store = host_store()?;
    let host_id = host_id_for_host(&spec);
    if let Some(record) = store.hosts.iter_mut().find(|record| record.id == host_id) {
        record.spec = spec;
    } else {
        store.hosts.push(SshHostRecord {
            id: host_id.clone(),
            spec,
        });
    }
    save_ssh_host_store(&store)?;
    let saved_spec = store
        .hosts
        .iter()
        .find(|record| record.id == host_id)
        .map(|record| record.spec.clone())
        .expect("host was just inserted");
    Ok((host_id, saved_spec))
}

/// Save an edited host. Returns the record's id, which is *not* always the
/// one passed in: the id is derived from the endpoint, so an edit that moves
/// the host to a different `user@host:port` has to re-key the record or it
/// stops matching its own spec. `None` means there was no such editable
/// record.
pub fn try_update_host(host_id: &str, mut spec: SshHostSpec) -> Result<Option<SshHostId>> {
    if is_system_host_id(host_id) {
        return Ok(None);
    }
    let mut store = host_store()?;
    // The same invariant `try_create_host` enforces, and for the same reason:
    // one record per endpoint. Editing a host onto an endpoint another record
    // already owns would leave two records dialing it, both registering a
    // domain under the single name `ssh_domain_name` derives.
    let new_id = host_id_for_host(&spec);
    if new_id != host_id && store.hosts.iter().any(|record| record.id == new_id) {
        bail!("a saved host already uses {}", endpoint(&spec));
    }
    let Some(record) = store.hosts.iter_mut().find(|record| record.id == host_id) else {
        return Ok(None);
    };
    keep_what_connections_found(&record.spec, &mut spec);
    let reached_as_before = same_ssh_domain(&record.spec, &spec);
    record.id = new_id.clone();
    record.spec = spec;
    save_ssh_host_store(&store)?;
    let spec = store
        .hosts
        .iter()
        .find(|record| record.id == new_id)
        .map(|record| record.spec.clone());
    drop(store);
    if let Some(spec) = spec {
        // Reached the way it was, the host has its domain already: the mux
        // keeps every domain it is given, so another would only show up
        // beside it, in the command palette for one.
        if reached_as_before {
            ensure_ssh_domain_registered(&spec)?;
        } else {
            register_ssh_domain(&spec)?;
        }
    }
    Ok(Some(new_id))
}

/// What connections found on the host is the record's to keep, not the copy
/// the form took when it opened -- unless the host moved to another
/// endpoint, which ThinkTerm has set nothing up on yet. Over the other
/// network, it is the same host.
fn keep_what_connections_found(record: &SshHostSpec, spec: &mut SshHostSpec) {
    if endpoint(spec) == endpoint(record) {
        spec.shell_integration_applied = record.shell_integration_applied;
        spec.shell_integration_unsupported = record.shell_integration_unsupported.clone();
        spec.shell_integration_failure = record.shell_integration_failure.clone();
    } else {
        spec.shell_integration_applied = false;
        spec.shell_integration_unsupported = None;
        spec.shell_integration_failure = None;
    }
}

pub fn try_remove_host(host_id: &str) -> Result<bool> {
    if is_system_host_id(host_id) {
        return Ok(false);
    }
    let mut store = host_store()?;
    let before = store.hosts.len();
    store.hosts.retain(|record| record.id != host_id);
    if store.hosts.len() == before {
        return Ok(false);
    }
    save_ssh_host_store(&store)?;
    Ok(true)
}

pub fn set_host_distro(host_id: &str, distro_id: &str) -> bool {
    if is_system_host_id(host_id) {
        return false;
    }
    let Ok(mut store) = host_store() else { return false; };
    let Some(record) = store.hosts.iter_mut().find(|record| record.id == host_id) else {
        return false;
    };
    let new_value = {
        let trimmed = distro_id.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    };
    if record.spec.detected_distro == new_value {
        return false;
    }
    record.spec.detected_distro = new_value;
    if let Err(err) = save_ssh_host_store(&store) {
        log::warn!("failed to persist SSH host distro for {host_id}: {err:#}");
    }
    true
}

/// The shell integration change a host's settings still wait on: `Some`
/// with whether to install it, when the setting and what was last carried
/// out differ. Only a direct ssh connection can carry it out; ThinkTerm
/// Connect sees the host's processes itself, and Mosh has no session for it.
pub fn pending_shell_integration(spec: &SshHostSpec) -> Option<bool> {
    (!spec.use_mosh
        && !spec.multiplexing
        && spec.shell_integration != spec.shell_integration_applied)
        .then_some(spec.shell_integration)
}

/// Record what setting up the host's shell integration came to, so the next
/// connection does not carry it out again. Returns whether the record
/// changed.
pub fn record_shell_integration(host_id: &str, outcome: &ShellIntegrationOutcome) -> bool {
    if is_system_host_id(host_id) {
        return false;
    }
    let Ok(mut store) = host_store() else {
        return false;
    };
    let Some(record) = store.hosts.iter_mut().find(|record| record.id == host_id) else {
        return false;
    };
    // A shell without hooks counts as carried out too: nothing was left on
    // the host, and the next connection would only find the same shell.
    let (applied, unsupported) = match outcome {
        ShellIntegrationOutcome::Installed(_) => (true, None),
        ShellIntegrationOutcome::Removed => (false, None),
        ShellIntegrationOutcome::Unsupported(shell) => (true, Some(shell.clone())),
    };
    if record.spec.shell_integration_applied == applied
        && record.spec.shell_integration_unsupported == unsupported
        && record.spec.shell_integration_failure.is_none()
    {
        return false;
    }
    record.spec.shell_integration_applied = applied;
    record.spec.shell_integration_unsupported = unsupported;
    record.spec.shell_integration_failure = None;
    if let Err(err) = save_ssh_host_store(&store) {
        log::warn!("failed to persist SSH host shell integration for {host_id}: {err:#}");
    }
    true
}

/// Record why carrying out the host's shell integration setting failed, for
/// its settings to show until a connection gets it done. Returns whether the
/// record changed.
pub fn record_shell_integration_failure(host_id: &str, err: &anyhow::Error) -> bool {
    if is_system_host_id(host_id) {
        return false;
    }
    let Some(failure) = shell_integration_failure(err) else {
        return false;
    };
    let Ok(mut store) = host_store() else {
        return false;
    };
    let Some(record) = store.hosts.iter_mut().find(|record| record.id == host_id) else {
        return false;
    };
    if record.spec.shell_integration_failure.as_ref() == Some(&failure) {
        return false;
    }
    record.spec.shell_integration_failure = Some(failure);
    if let Err(err) = save_ssh_host_store(&store) {
        log::warn!("failed to persist SSH host shell integration for {host_id}: {err:#}");
    }
    true
}

/// How a failed setup is kept: a file the host would not let it write as
/// such, anything else in one line short enough for a few lines of the form.
/// Nothing when it never ran -- a host that never connected has nothing to
/// show for it.
fn shell_integration_failure(err: &anyhow::Error) -> Option<ShellIntegrationFailure> {
    if err.downcast_ref::<NotConnected>().is_some() {
        return None;
    }
    if let Some(file) = err.downcast_ref::<UnwritableStartupFile>() {
        return Some(ShellIntegrationFailure::Unwritable(file.0.clone()));
    }
    let message = format!("{err:#}")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let mut short: String = message.chars().take(240).collect();
    if short.len() < message.len() {
        short.push('…');
    }
    Some(ShellIntegrationFailure::Other(short))
}

/// What connections have found about a host's shell integration so far, as
/// `record_shell_integration` and `record_shell_integration_failure` keep it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ShellIntegrationRecord {
    /// The setting as last carried out.
    pub applied: bool,
    /// The login shell the integration has no hooks for.
    pub unsupported: Option<String>,
    /// Why the last connection could not carry the setting out.
    pub failure: Option<ShellIntegrationFailure>,
}

/// What setting up the host's shell integration has come to so far.
pub fn shell_integration_record(host_id: &str) -> ShellIntegrationRecord {
    let Ok(store) = host_store() else {
        return ShellIntegrationRecord::default();
    };
    store
        .hosts
        .iter()
        .find(|record| record.id == host_id)
        .map(|record| ShellIntegrationRecord {
            applied: record.spec.shell_integration_applied,
            unsupported: record.spec.shell_integration_unsupported.clone(),
            failure: record.spec.shell_integration_failure.clone(),
        })
        .unwrap_or_default()
}

pub fn is_system_host_id(host_id: &str) -> bool {
    host_id.starts_with("system-ssh-")
}

/// `user@host` (or `host`, with `:port` when non-default) used for the
/// human-readable domain / workspace identifiers.
pub(crate) fn endpoint(spec: &SshHostSpec) -> String {
    let host = match spec.port {
        Some(port) if port != 22 => format!("{}:{port}", spec.host),
        _ => spec.host.clone(),
    };
    match &spec.username {
        Some(user) if !user.is_empty() => format!("{user}@{host}"),
        _ => host,
    }
}

pub fn host_project_path(spec: &SshHostSpec) -> PathBuf {
    PathBuf::from(format!("ssh://{}", endpoint(spec)))
}

/// Build the fallback command line for connecting to `spec` via the `mosh` wrapper:
/// `mosh [--ssh="ssh -p <port> -i <identity>"] <user>@<host>`. mosh performs the
/// SSH bootstrap itself, so port/identity go into its `--ssh` option and the
/// target is a bare `user@host` (no `:port`). This fallback only carries port +
/// identity; arbitrary `ssh_options` and stored passwords are not plumbed
/// through because the wrapper's SSH bootstrap is interactive.
pub fn build_mosh_args(spec: &SshHostSpec) -> Vec<String> {
    let mut ssh = String::from("ssh");
    if let Some(port) = spec.port {
        if port != 22 {
            ssh.push_str(&format!(" -p {port}"));
        }
    }
    if let Some(identity) = spec.identity_file.as_deref().filter(|s| !s.is_empty()) {
        let identity = shlex::try_quote(identity)
            .unwrap_or_else(|_| identity.into())
            .into_owned();
        ssh.push_str(&format!(" -i {identity}"));
    }

    let mut args = vec!["mosh".to_string()];
    if ssh != "ssh" {
        args.push(format!("--ssh={ssh}"));
    }

    let target = match spec.username.as_deref().filter(|s| !s.is_empty()) {
        Some(user) => format!("{user}@{}", spec.host),
        None => spec.host.clone(),
    };
    args.push(target);
    args
}

fn check_mosh_network(spec: &SshHostSpec) -> Result<()> {
    if spec.network == SshNetwork::Tailnet {
        bail!(crate::i18n::tr("ssh-error-tailnet-mosh"));
    }
    Ok(())
}

pub fn build_mosh_fallback_spawn(spec: &SshHostSpec) -> Result<SpawnCommand> {
    check_mosh_network(spec)?;
    Ok(SpawnCommand {
        label: Some(format!("mosh {}", spec.label)),
        args: Some(build_mosh_args(spec)),
        domain: crate::local_sessions::local_spawn_domain(),
        ..Default::default()
    })
}

fn parse_mosh_connect(output: &str) -> Option<(u16, String)> {
    for line in output.lines() {
        let mut parts = line.split_whitespace();
        if parts.next() != Some("MOSH") || parts.next() != Some("CONNECT") {
            continue;
        }
        let Some(port) = parts.next().and_then(|port| port.parse::<u16>().ok()) else {
            continue;
        };
        let Some(key) = parts.next().filter(|key| !key.is_empty()) else {
            continue;
        };
        let key = key.to_string();
        if !key.is_empty() {
            return Some((port, key));
        }
    }
    None
}

fn build_mosh_client_spawn(
    spec: &SshHostSpec,
    host: String,
    port: u16,
    key: String,
) -> SpawnCommand {
    let mut env = HashMap::new();
    env.insert("MOSH_KEY".to_string(), key);
    SpawnCommand {
        label: Some(format!("mosh {}", spec.label)),
        args: Some(vec!["mosh-client".to_string(), host, port.to_string()]),
        set_environment_variables: env,
        domain: crate::local_sessions::local_spawn_domain(),
        ..Default::default()
    }
}

fn read_fd_to_string(mut fd: FileDescriptor) -> std::io::Result<String> {
    let mut buf = Vec::new();
    fd.read_to_end(&mut buf)?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

async fn connect_mosh_bootstrap_session(spec: &SshHostSpec) -> Result<(Session, String)> {
    let dom = build_ssh_domain(spec);
    let mut ssh_config = mux::ssh::ssh_domain_to_ssh_config(&dom).context("build SSH config")?;
    // mosh-client goes to the host's own address over UDP, so the
    // bootstrap that starts mosh-server there must not land elsewhere.
    ssh_config.remove(wezterm_ssh::OTHER_ADDRESSES);
    let mosh_client_host = ssh_config
        .get("hostname")
        .cloned()
        .unwrap_or_else(|| spec.host.clone());
    let (session, events) = Session::connect(ssh_config).context("connect to SSH server")?;
    let mut password = spec
        .password
        .as_deref()
        .map(crate::secret::reveal)
        .filter(|p| !p.is_empty());

    while let Ok(event) = events.recv().await {
        match event {
            SessionEvent::Banner(_) => {}
            SessionEvent::HostVerify(verify) => {
                bail!(
                    "SSH host verification requires interactive confirmation: {}",
                    verify.message
                );
            }
            SessionEvent::Authenticate(auth) => {
                let mut answers = vec![];
                for prompt in &auth.prompts {
                    if prompt.echo {
                        bail!("SSH authentication requires an interactive prompt");
                    }
                    let Some(stored) = password.take() else {
                        bail!("SSH authentication requires a saved password");
                    };
                    answers.push(stored);
                }
                auth.answer(answers)
                    .await
                    .context("answer SSH authentication prompt")?;
            }
            SessionEvent::HostVerificationFailed(failed) => {
                bail!("SSH host verification failed: {}", failed);
            }
            SessionEvent::Error(err) => bail!("SSH error: {}", err),
            SessionEvent::Authenticated => return Ok((session, mosh_client_host)),
        }
    }

    bail!("SSH authentication did not complete")
}

pub async fn build_integrated_mosh_spawn(spec: &SshHostSpec) -> Result<SpawnCommand> {
    check_mosh_network(spec)?;
    let command = spec.mosh_server_command.trim();
    if command.is_empty() {
        bail!("Mosh server command is empty");
    }

    let (session, host) = connect_mosh_bootstrap_session(spec).await?;
    let exec = session
        .exec(command, None)
        .await
        .context("run mosh-server command")?;
    let stdout = exec.stdout;
    let stderr = exec.stderr;
    let mut child = exec.child;
    let stdout_reader = std::thread::spawn(move || read_fd_to_string(stdout));
    let stderr_reader = std::thread::spawn(move || read_fd_to_string(stderr));
    let status = child.async_wait().await.context("wait for mosh-server")?;
    let stdout = stdout_reader
        .join()
        .map_err(|_| anyhow::anyhow!("mosh-server stdout reader panicked"))?
        .context("read mosh-server stdout")?;
    let stderr = stderr_reader
        .join()
        .map_err(|_| anyhow::anyhow!("mosh-server stderr reader panicked"))?
        .context("read mosh-server stderr")?;
    let output = format!("{stdout}\n{stderr}");

    let Some((port, key)) = parse_mosh_connect(&output) else {
        if status.success() {
            bail!("mosh-server output did not include MOSH CONNECT");
        }
        bail!("mosh-server exited with status {}", status.exit_code());
    };

    Ok(build_mosh_client_spawn(spec, host, port, key))
}

/// Deterministic mux domain name for a host. Stable across restarts so that a
/// snapshotted SSH layout (which records the pane's domain name) can be
/// re-materialized after [`register_saved_hosts`] re-registers the domain.
pub fn ssh_domain_name(spec: &SshHostSpec) -> String {
    thinkterm_core::ssh_hosts::ssh_domain_name(spec)
}

/// Translate a stored host spec into a `config::SshDomain`.
pub fn build_ssh_domain(spec: &SshHostSpec) -> SshDomain {
    thinkterm_core::ssh_hosts::build_ssh_domain(spec)
}

fn register_ssh_domain(spec: &SshHostSpec) -> Result<()> {
    let dom = build_ssh_domain(spec);
    let password = spec
        .password
        .as_deref()
        .map(crate::secret::reveal)
        .filter(|p| !p.is_empty());
    let domain: Arc<dyn Domain> = Arc::new(RemoteSshDomain::with_ssh_domain_and_password(
        &dom, password,
    )?);
    Mux::get().add_domain(&domain);
    Ok(())
}

/// Whether two domains made from a host dial it the same way: everything
/// `build_ssh_domain` takes from the host's record, and its password.
pub fn same_dialing(a: &SshDomain, b: &SshDomain) -> bool {
    a.name == b.name
        && a.remote_address == b.remote_address
        && a.username == b.username
        && a.multiplexing == b.multiplexing
        && a.ssh_option == b.ssh_option
        && a.stored_password == b.stored_password
}

/// Whether `a` and `b` make the same ssh domain. Most of a host's record --
/// how its shell is set up, whether its OS is detected, Mosh -- goes into
/// none of its domains.
fn same_ssh_domain(a: &SshHostSpec, b: &SshHostSpec) -> bool {
    same_dialing(&build_ssh_domain(a), &build_ssh_domain(b)) && a.password == b.password
}

/// Ensure the host's SSH domain is registered with the mux, building and adding
/// it on first use. Returns the domain name to spawn into. The actual SSH
/// connection is established lazily by the domain's `spawn_pane`, so this is
/// cheap and does not block on the network.
pub fn ensure_ssh_domain_registered(spec: &SshHostSpec) -> Result<String> {
    let name = ssh_domain_name(spec);
    let mux = Mux::get();
    if mux.get_domain_by_name(&name).is_none() {
        register_ssh_domain(spec)?;
    }
    Ok(name)
}

/// Register every saved SSH host as a mux domain. Called once at startup so
/// that re-activating a previously-connected remote session (whose saved layout
/// references the domain by name) works without re-opening the host manager.
pub fn register_saved_hosts() {
    for entry in list_all_hosts() {
        if let Err(err) = ensure_ssh_domain_registered(&entry.spec) {
            log::warn!(
                "failed to register saved SSH host {:?}: {err:#}",
                entry.spec.label
            );
        }
    }
}

/// What a host on the embedded tailnet names its ThinkTerm Connect domain
/// after: its address, so that saving it on the other network makes a new
/// domain rather than finding the old one, connected the old way.
const TAILNET_CONNECT_PREFIX: &str = "tailnet:";

pub fn connect_domain_name(spec: &SshHostSpec) -> String {
    match spec.network {
        SshNetwork::System => spec.label.clone(),
        SshNetwork::Tailnet => format!("{TAILNET_CONNECT_PREFIX}{}", endpoint(spec)),
    }
}

/// What a ThinkTerm Connect domain is called on screen: the name of the host
/// it belongs to. Only a domain of the embedded tailnet is named otherwise,
/// and only a host saved here can be on it -- `~/.ssh/config` has none.
pub fn connect_domain_label(domain_name: &str) -> String {
    if domain_name.starts_with(TAILNET_CONNECT_PREFIX) {
        if let Ok(store) = host_store() {
            let specs = store.hosts.iter().map(|record| &record.spec);
            if let Some(label) = label_for_connect_domain(specs, domain_name) {
                return label;
            }
        }
    }
    domain_name.to_string()
}

/// What a domain that is a connection to another machine is called where
/// the command line names it (`--pane MACHINE/ID`): the label of the saved
/// host it reaches -- over a session server there, or a plain SSH session --
/// or, for one that is not saved, its own name.
pub fn machine_label(domain_name: &str) -> String {
    if let Ok(store) = host_store() {
        let label = store
            .hosts
            .iter()
            .map(|record| &record.spec)
            .find(|spec| {
                connect_domain_name(spec) == domain_name || ssh_domain_name(spec) == domain_name
            })
            .map(|spec| spec.label.trim().to_string())
            .filter(|label| !label.is_empty());
        if let Some(label) = label {
            return label;
        }
    }
    connect_domain_label(domain_name)
}

fn label_for_connect_domain<'a>(
    mut specs: impl Iterator<Item = &'a SshHostSpec>,
    domain_name: &str,
) -> Option<String> {
    specs
        .find(|spec| connect_domain_name(spec) == domain_name)
        .map(|spec| spec.label.trim().to_string())
        .filter(|label| !label.is_empty())
}

/// The saved host a ThinkTerm Connect domain name stands for. The name a
/// Space keeps for its host is tried first: a host on the system network and
/// one on the embedded tailnet may share a label, and only that name tells
/// them apart. Then a record id, and a bare label (as typed for `thinkterm
/// connect`) last.
pub fn host_for_connect_domain<'a>(
    hosts: &'a [SshHostEntry],
    name: &str,
) -> Option<&'a SshHostEntry> {
    hosts
        .iter()
        .find(|entry| connect_domain_name(&entry.spec) == name)
        .or_else(|| hosts.iter().find(|entry| entry.id == name))
        .or_else(|| hosts.iter().find(|entry| entry.spec.label == name))
}

/// Every name this host can be connected under.
///
/// A ThinkTerm Connect host attaches its persistent mux domain under the
/// user's label (on the embedded tailnet, under its address: see
/// `connect_domain_name`); a direct host registers as `ssh:user@host`. A Space mirrored
/// from that machine is tagged with whichever one it arrived through.
pub fn domain_names_for_host(spec: &SshHostSpec) -> Vec<String> {
    let mut names = vec![ssh_domain_name(spec)];
    let name = connect_domain_name(spec);
    let name = name.trim();
    if !name.is_empty() && !names.iter().any(|other| other == name) {
        names.push(name.to_string());
    }
    names
}

/// Whether a mux domain of this name can still be reached: a lua-configured
/// or already-attached domain, or a saved host that is connected under this
/// name or its record id. Remote Hosts once connected hosts under their ids;
/// a Space left under one stays, for the host's connection under its own
/// name to take over (see `ingest_remote_tree`).
fn domain_name_is_reachable(domain_name: &str) -> bool {
    if Mux::get().get_domain_by_name(domain_name).is_some() {
        return true;
    }
    list_all_hosts()
        .into_iter()
        .any(|entry| connect_domain_name(&entry.spec) == domain_name || entry.id == domain_name)
}

/// The connection name an edit moved a host from, and the one it goes by
/// now: when it was renamed or moved to the other network, and no other
/// saved host goes by the old name as well.
pub fn connect_name_change(old: &SshHostSpec, new: &SshHostSpec) -> Option<(String, String)> {
    // The same name: no need to read every host, `~/.ssh/config`'s too.
    if connect_domain_name(old) == connect_domain_name(new) {
        return None;
    }
    connect_name_change_among(old, new, &list_all_hosts())
}

fn connect_name_change_among(
    old: &SshHostSpec,
    new: &SshHostSpec,
    hosts: &[SshHostEntry],
) -> Option<(String, String)> {
    let before = connect_domain_name(old);
    let after = connect_domain_name(new);
    let still_used = hosts
        .iter()
        .any(|entry| connect_domain_name(&entry.spec) == before);
    (before != after && !still_used).then_some((before, after))
}

/// Take a host's Spaces from the connection name an edit moved it off to
/// the one it goes by now (see `connect_name_change`). Not while connected
/// under the old name, even if that connection has dropped and is coming
/// back: their windows show its terminals, and moved out from under it they
/// would read as disconnected and be connected a second time beside it.
/// They stay with it, as its address and password do, until it is gone.
/// One that is only trying to connect has nothing on screen to keep, and
/// gives them up at once.
pub fn move_spaces_after_rename(from: String, to: String) {
    let moved = rename_moves_now(&mut RENAMED_WHILE_CONNECTED.lock(), from, to, connected_under);
    if let Some((from, to)) = moved {
        crate::workspace_threads::move_spaces_to_domain(&from, &to);
    }
}

/// The name to connect by for a host that goes by, or went by, `name`.
/// Renamed while connected the old way, that is the old name as long as the
/// connection lasts -- connecting lands in the windows already on it -- and
/// the new one after, the Spaces moved over to it first.
pub fn connect_name_after_rename(name: &str) -> String {
    let (name, moved) = rename_settles(&mut RENAMED_WHILE_CONNECTED.lock(), name, connected_under);
    if let Some((from, to)) = moved {
        crate::workspace_threads::move_spaces_to_domain(&from, &to);
    }
    name
}

/// Whether a connection under `name` was made and has not been let go:
/// connected, or reconnecting after a drop.
fn connected_under(name: &str) -> bool {
    Mux::get()
        .get_domain_by_name(name)
        .is_some_and(|domain| domain.state() == mux::domain::DomainState::Attached)
}

/// The Spaces a rename from `from` to `to` moves now, as (from, to): none
/// while `connected` under the old name, the rename waiting in `renamed`
/// instead.
fn rename_moves_now(
    renamed: &mut Vec<(String, String)>,
    from: String,
    to: String,
    connected: impl Fn(&str) -> bool,
) -> Option<(String, String)> {
    // Renamed again before the first rename moved anything: one move, from
    // the name the Spaces are under to the newest.
    let from = match renamed.iter().position(|(_, new)| *new == from) {
        Some(index) => renamed.remove(index).0,
        None => from,
    };
    if from == to {
        return None;
    }
    if connected(&from) {
        renamed.push((from, to));
        return None;
    }
    Some((from, to))
}

/// The name to connect by for `name` given the renames waiting in
/// `renamed`, and the Spaces to move before connecting by it.
fn rename_settles(
    renamed: &mut Vec<(String, String)>,
    name: &str,
    connected: impl Fn(&str) -> bool,
) -> (String, Option<(String, String)>) {
    let Some(index) = renamed
        .iter()
        .position(|(old, new)| old == name || new == name)
    else {
        return (name.to_string(), None);
    };
    if connected(&renamed[index].0) {
        return (renamed[index].0.clone(), None);
    }
    let (old, new) = renamed.remove(index);
    (new.clone(), Some((old, new)))
}

/// The names saved hosts are connected under as things stand.
pub fn connect_domain_names() -> std::collections::HashSet<String> {
    list_all_hosts()
        .iter()
        .map(|entry| connect_domain_name(&entry.spec))
        .collect()
}

/// Move the Spaces left under a saved host's record id to the name the host
/// is connected under. Remote Hosts once connected hosts under their ids, so
/// a Space made then reconnected under the id and never under the host's
/// own name. At startup, before anything is connected.
pub fn move_spaces_off_record_ids() {
    for entry in list_all_hosts() {
        crate::workspace_threads::move_spaces_to_domain(
            &entry.id,
            &connect_domain_name(&entry.spec),
        );
    }
}

/// Drop this device's copy of every Space whose mux server it can no longer
/// name.
///
/// Deleting a host used to leave its Spaces in the sidebar forever: they
/// cannot be connected to (nothing resolves the domain), and they cannot be
/// renamed or removed through the server either, because every mutation is
/// refused while the domain is unattached. Removal is local — the server keeps
/// its Spaces and other devices never notice — so re-adding the host brings
/// them straight back.
///
/// Call once at startup, after both lua domains and saved hosts are
/// registered and before any window exists.
pub fn forget_spaces_of_deleted_hosts() {
    let unreachable = crate::workspace_threads::remote_space_domains()
        .into_iter()
        .filter(|domain| !domain_name_is_reachable(domain))
        .collect::<Vec<_>>();
    if unreachable.is_empty() {
        return;
    }
    for space_id in crate::workspace_threads::space_ids_for_domains(&unreachable) {
        match crate::workspace_threads::forget_space_locally(&space_id) {
            Ok(_) => log::info!(
                "removed Space {space_id} from this device: the host its mux \
                 server was reached through is no longer saved. The server \
                 keeps it; re-adding the host brings it back."
            ),
            Err(err) => log::warn!("cannot remove unreachable Space {space_id}: {err:?}"),
        }
    }
    // Startup catch-all for references into those machines (a host deleted
    // while the app was closed, or an interrupted delete): with no host
    // record left to resolve them, the refs would grey out forever.
    crate::workspace_threads::purge_thread_refs_for_machines(&unreachable);
}

/// When `~/.ssh/config` last changed and how long it is: enough for a caller
/// that keeps [`list_system_hosts`] to tell when to read it again.
pub type SshConfigStamp = (std::time::SystemTime, u64);

/// `None` when there is no file, or its time cannot be read.
pub fn system_ssh_config_stamp() -> Option<SshConfigStamp> {
    let meta = fs::metadata(system_ssh_config_path()).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

fn system_ssh_config_path() -> PathBuf {
    config::HOME_DIR.join(".ssh").join("config")
}

pub fn list_system_hosts() -> Vec<SshHostEntry> {
    let path = system_ssh_config_path();
    match parse_system_ssh_config(&path) {
        Ok(hosts) => hosts,
        Err(err) => {
            log::debug!(
                "failed to read system SSH config {}: {err:#}",
                path.display()
            );
            vec![]
        }
    }
}

fn parse_system_ssh_config(path: &Path) -> Result<Vec<SshHostEntry>> {
    if !path.exists() {
        return Ok(vec![]);
    }
    let content = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    Ok(parse_system_ssh_config_str(&content))
}

#[derive(Default)]
struct SystemHostBlock {
    aliases: Vec<String>,
    user: Option<String>,
    port: Option<u16>,
    identity_file: Option<String>,
}

fn parse_system_ssh_config_str(content: &str) -> Vec<SshHostEntry> {
    fn flush(block: &mut Option<SystemHostBlock>, out: &mut Vec<SshHostEntry>) {
        let Some(block) = block.take() else {
            return;
        };
        for alias in block.aliases {
            if !is_concrete_host_alias(&alias) {
                continue;
            }
            let spec = SshHostSpec {
                label: alias.clone(),
                // Preserve the alias as the remote address so ssh_config
                // alias-scoped options such as ProxyJump still apply.
                host: alias.clone(),
                port: block.port,
                other_addresses: Vec::new(),
                network: Default::default(),
                username: block.user.clone(),
                identity_file: block.identity_file.clone(),
                password: None,
                ssh_options: HashMap::new(),
                multiplexing: false,
                default_workspace: None,
                detect_os: true,
                detected_distro: None,
                use_mosh: false,
                mosh_server_command: default_mosh_server_command(),
                shell_integration: false,
                shell_integration_applied: false,
                shell_integration_unsupported: None,
                shell_integration_failure: None,
                files_auto_connect: false,
            };
            out.push(SshHostEntry {
                id: system_host_id(&alias),
                source: SshHostSource::System,
                spec,
            });
        }
    }

    let mut entries = Vec::new();
    let mut current: Option<SystemHostBlock> = None;

    for raw_line in content.lines() {
        let Some(line) = strip_ssh_config_comment(raw_line) else {
            continue;
        };
        let mut parts = line.split_whitespace();
        let Some(key) = parts.next() else {
            continue;
        };
        let key = key.to_ascii_lowercase();
        match key.as_str() {
            "host" => {
                flush(&mut current, &mut entries);
                let aliases = parts.map(|part| part.to_string()).collect::<Vec<_>>();
                current = Some(SystemHostBlock {
                    aliases,
                    ..Default::default()
                });
            }
            "user" => {
                if let Some(block) = current.as_mut() {
                    block.user = parts.next().map(|value| value.to_string());
                }
            }
            "port" => {
                if let Some(block) = current.as_mut() {
                    block.port = parts.next().and_then(|value| value.parse::<u16>().ok());
                }
            }
            "identityfile" => {
                if let Some(block) = current.as_mut() {
                    block.identity_file = parts.next().map(expand_system_ssh_value);
                }
            }
            _ => {}
        }
    }
    flush(&mut current, &mut entries);
    entries.sort_by(|a, b| {
        a.spec
            .label
            .to_ascii_lowercase()
            .cmp(&b.spec.label.to_ascii_lowercase())
    });
    entries.dedup_by(|a, b| a.id == b.id);
    entries
}

fn strip_ssh_config_comment(line: &str) -> Option<&str> {
    let line = line.split_once('#').map(|(head, _)| head).unwrap_or(line);
    let line = line.trim();
    (!line.is_empty()).then_some(line)
}

fn is_concrete_host_alias(alias: &str) -> bool {
    !alias.starts_with('!') && !alias.contains('*') && !alias.contains('?')
}

fn expand_system_ssh_value(value: &str) -> String {
    if value == "~" {
        config::HOME_DIR.to_string_lossy().to_string()
    } else if let Some(rest) = value.strip_prefix("~/") {
        config::HOME_DIR.join(rest).to_string_lossy().to_string()
    } else {
        value.to_string()
    }
}

fn fnv1a(input: &str) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in input.bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

/// The record id for a host, derived from its network and `user@host:port`: two
/// hosts that dial the same endpoint are the same record no matter what they
/// are named. Both save paths lean on that: see `try_create_host` and
/// `try_update_host`.
pub fn host_id_for_host(spec: &SshHostSpec) -> SshHostId {
    let key = format!(
        "ssh:{}@{}:{}",
        spec.username.as_deref().unwrap_or(""),
        spec.host,
        spec.port.unwrap_or(22)
    );
    let key = match spec.network {
        SshNetwork::System => key,
        SshNetwork::Tailnet => format!("tailnet:{key}"),
    };
    format!("ssh-{:x}", fnv1a(&key))
}

fn system_host_id(alias: &str) -> SshHostId {
    format!("system-ssh-{:x}", fnv1a(&format!("system:{alias}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_same_address_on_two_networks_has_distinct_records_and_domains() {
        let mut spec = spec_with_options(HashMap::new());
        spec.label = "server-a".into();
        spec.host = "100.64.0.2".into();
        let id = host_id_for_host(&spec);
        let domains = domain_names_for_host(&spec);
        spec.network = SshNetwork::Tailnet;
        assert_ne!(host_id_for_host(&spec), id);
        assert!(domain_names_for_host(&spec)
            .iter()
            .all(|name| !domains.contains(name)));
        assert!(build_mosh_fallback_spawn(&spec).is_err());
    }

    #[test]
    fn a_tailnet_connect_domain_is_shown_by_its_hosts_name() {
        let mut spec = spec_with_options(HashMap::new());
        spec.label = "server-a".into();
        spec.host = "100.64.0.2".into();
        spec.network = SshNetwork::Tailnet;
        let domain = connect_domain_name(&spec);
        assert_ne!(domain, "server-a");
        assert_eq!(
            label_for_connect_domain(std::iter::once(&spec), &domain).as_deref(),
            Some("server-a")
        );
        assert_eq!(
            label_for_connect_domain(std::iter::once(&spec), "tailnet:server-b"),
            None
        );
        // Any other domain is already called what it is named.
        assert_eq!(connect_domain_label("server-b"), "server-b");
    }

    #[test]
    fn a_label_on_both_networks_finds_the_host_its_space_was_connected_through() {
        let mut system = spec_with_options(HashMap::new());
        system.label = "server-a".into();
        system.host = "192.0.2.10".into();
        let mut tailnet = system.clone();
        tailnet.host = "100.64.0.2".into();
        tailnet.network = SshNetwork::Tailnet;
        let entry = |spec: &SshHostSpec| SshHostEntry {
            id: host_id_for_host(spec),
            source: SshHostSource::ThinkTerm,
            spec: spec.clone(),
        };
        let network = |hosts: &[SshHostEntry], name: &str| {
            host_for_connect_domain(hosts, name).map(|entry| entry.spec.network)
        };
        // Listed first, the tailnet host shares the label but must not
        // answer for the Spaces of the one on the system network.
        let hosts = [entry(&tailnet), entry(&system)];
        assert_eq!(network(&hosts, "server-a"), Some(SshNetwork::System));
        assert_eq!(
            network(&hosts, &connect_domain_name(&tailnet)),
            Some(SshNetwork::Tailnet)
        );
        assert_eq!(
            network(&hosts, &host_id_for_host(&tailnet)),
            Some(SshNetwork::Tailnet)
        );
        // With nothing else called that, the label still finds its host.
        let hosts = [entry(&tailnet)];
        assert_eq!(network(&hosts, "server-a"), Some(SshNetwork::Tailnet));
        assert_eq!(network(&hosts, "server-b"), None);
    }

    #[test]
    fn a_rename_or_a_move_to_the_other_network_changes_the_connection_name() {
        let entry = |spec: &SshHostSpec| SshHostEntry {
            id: host_id_for_host(spec),
            source: SshHostSource::ThinkTerm,
            spec: spec.clone(),
        };
        let mut old = spec_with_options(HashMap::new());
        old.label = "server-a".into();
        old.host = "100.64.0.2".into();
        let change = |new: &SshHostSpec| connect_name_change_among(&old, new, &[entry(new)]);

        let mut new = old.clone();
        new.multiplexing = true;
        new.detect_os = false;
        assert_eq!(change(&new), None);

        new.label = "server-b".into();
        assert_eq!(
            change(&new),
            Some(("server-a".to_string(), "server-b".to_string()))
        );

        let mut moved = old.clone();
        moved.network = SshNetwork::Tailnet;
        assert_eq!(
            change(&moved),
            Some(("server-a".to_string(), connect_domain_name(&moved)))
        );

        // A name another saved host still goes by stays that host's.
        let mut other = old.clone();
        other.host = "192.0.2.10".into();
        assert_eq!(
            connect_name_change_among(&old, &new, &[entry(&new), entry(&other)]),
            None
        );
    }

    #[test]
    fn a_rename_with_nothing_connected_moves_the_spaces_at_once() {
        let mut renamed = vec![];
        assert_eq!(
            rename_moves_now(&mut renamed, "server-a".into(), "server-b".into(), |_| false),
            Some(("server-a".to_string(), "server-b".to_string()))
        );
        assert!(renamed.is_empty());
        assert_eq!(
            rename_settles(&mut renamed, "server-b", |_| false),
            ("server-b".to_string(), None)
        );
    }

    #[test]
    fn a_rename_while_connected_leaves_the_spaces_on_that_connection_until_it_ends() {
        let mut renamed = vec![];
        let connected = |name: &str| name == "server-a";
        assert_eq!(
            rename_moves_now(&mut renamed, "server-a".into(), "server-b".into(), connected),
            None
        );
        // Connecting by either name lands on the connection still up.
        assert_eq!(
            rename_settles(&mut renamed, "server-b", connected),
            ("server-a".to_string(), None)
        );
        assert_eq!(
            rename_settles(&mut renamed, "server-a", connected),
            ("server-a".to_string(), None)
        );
        // Renamed again meanwhile: still one move, from where the Spaces are.
        assert_eq!(
            rename_moves_now(&mut renamed, "server-b".into(), "server-c".into(), connected),
            None
        );
        // Once it is down, connecting moves them over and goes by the new name.
        assert_eq!(
            rename_settles(&mut renamed, "server-c", |_| false),
            (
                "server-c".to_string(),
                Some(("server-a".to_string(), "server-c".to_string()))
            )
        );
        assert!(renamed.is_empty());
    }

    #[test]
    fn renaming_back_while_connected_leaves_nothing_to_move() {
        let mut renamed = vec![];
        let connected = |name: &str| name == "server-a";
        rename_moves_now(&mut renamed, "server-a".into(), "server-b".into(), connected);
        assert_eq!(
            rename_moves_now(&mut renamed, "server-b".into(), "server-a".into(), connected),
            None
        );
        assert!(renamed.is_empty());
    }

    #[test]
    fn a_failed_catalog_load_is_retried_before_editing() {
        let mut cached = None;
        assert!(ensure_store_loaded(&mut cached, || anyhow::bail!("migration cleanup temporarily failed")).is_err());
        assert!(cached.is_none());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ssh_hosts.json");
        let original = SshHostRecord { id: "original".into(), spec: spec_with_options(HashMap::new()) };
        save_ssh_host_store_to_path(&path, &SshHostStore { version: 1, hosts: vec![original.clone()] }).unwrap();
        ensure_store_loaded(&mut cached, || load_ssh_host_store_from_path(&path)).unwrap();
        let store = cached.as_mut().unwrap();
        store.hosts.push(SshHostRecord { id: "new".into(), ..original });
        save_ssh_host_store_to_path(&path, store).unwrap();
        let saved = load_ssh_host_store_from_path(&path).unwrap();
        assert_eq!(saved.hosts.len(), 2);
        assert_eq!(saved.hosts[0].id, "original");
    }

    #[test]
    fn system_hosts_preserve_ssh_config_alias_as_remote_address() {
        let entries = parse_system_ssh_config_str(
            r#"
Host prod *.internal
  HostName 10.0.0.1
  User deploy
  Port 2202
  IdentityFile ~/.ssh/prod
  ProxyJump bastion
"#,
        );

        assert_eq!(entries.len(), 1);
        let spec = &entries[0].spec;
        assert_eq!(spec.label, "prod");
        assert_eq!(spec.host, "prod");
        assert_eq!(spec.username.as_deref(), Some("deploy"));
        assert_eq!(spec.port, Some(2202));
        // `~` expands to this machine's home, which on Windows is spelled
        // with backslashes; compare components rather than text.
        let identity = std::path::Path::new(spec.identity_file.as_deref().unwrap());
        assert!(
            identity.ends_with(std::path::Path::new(".ssh").join("prod")),
            "{}",
            identity.display()
        );
    }

    /// The two names one host can attach a mux domain under. A Space is tagged
    /// with whichever one it arrived through, so deleting the host has to look
    /// for both or it leaves half of them stranded.
    #[test]
    fn a_host_is_reachable_under_both_its_label_and_its_endpoint() {
        let mut spec = spec_with_options(HashMap::new());
        spec.label = "DO SYD".to_string();
        spec.username = Some("x".to_string());

        assert_eq!(
            domain_names_for_host(&spec),
            vec!["ssh:x@example.com".to_string(), "DO SYD".to_string()]
        );

        // A host left unlabelled must not contribute an empty name, which
        // would match nothing and read as a bug at the call site.
        spec.label = "  ".to_string();
        assert_eq!(
            domain_names_for_host(&spec),
            vec!["ssh:x@example.com".to_string()]
        );

        // And a label that is already the endpoint is not worth saying twice.
        spec.label = "ssh:x@example.com".to_string();
        assert_eq!(
            domain_names_for_host(&spec),
            vec!["ssh:x@example.com".to_string()]
        );
    }

    #[test]
    fn a_connection_that_never_came_up_is_no_failure_of_the_setup() {
        let refused = anyhow::Error::new(NotConnected("connection refused".to_string()));
        assert_eq!(shell_integration_failure(&refused), None);
    }

    #[test]
    fn a_failed_setup_is_kept_as_the_file_or_in_one_short_line() {
        let unwritable = anyhow::Error::new(UnwritableStartupFile("~/.zshrc".to_string()));
        assert_eq!(
            shell_integration_failure(&unwritable),
            Some(ShellIntegrationFailure::Unwritable("~/.zshrc".to_string()))
        );
        // Still the file under a context.
        let wrapped = unwritable.context("setting up example");
        assert_eq!(
            shell_integration_failure(&wrapped),
            Some(ShellIntegrationFailure::Unwritable("~/.zshrc".to_string()))
        );

        let lost = anyhow::anyhow!("the setup on the host failed:\n  Exited with code 2");
        assert_eq!(
            shell_integration_failure(&lost),
            Some(ShellIntegrationFailure::Other(
                "the setup on the host failed: Exited with code 2".to_string()
            ))
        );
        let long = anyhow::anyhow!("{}", "x".repeat(300));
        match shell_integration_failure(&long) {
            Some(ShellIntegrationFailure::Other(text)) => {
                assert_eq!(text.chars().count(), 241);
                assert!(text.ends_with('…'));
            }
            other => panic!("{:?}", other),
        }
    }

    #[test]
    fn only_a_changed_setting_on_plain_ssh_waits_on_a_connection() {
        let mut spec = spec_with_options(HashMap::new());
        assert_eq!(pending_shell_integration(&spec), None);
        spec.shell_integration = true;
        assert_eq!(pending_shell_integration(&spec), Some(true));
        spec.shell_integration_applied = true;
        assert_eq!(pending_shell_integration(&spec), None);
        spec.shell_integration = false;
        assert_eq!(pending_shell_integration(&spec), Some(false));

        // Connect sees the host's processes itself, and Mosh has no session
        // to carry the change out on.
        spec.multiplexing = true;
        assert_eq!(pending_shell_integration(&spec), None);
        spec.multiplexing = false;
        spec.use_mosh = true;
        assert_eq!(pending_shell_integration(&spec), None);
    }

    #[test]
    fn the_other_network_keeps_what_connections_found_on_the_host() {
        let mut record = spec_with_options(HashMap::new());
        record.shell_integration_applied = true;
        record.shell_integration_unsupported = Some("nu".to_string());
        // The form's copy, taken before the last connection.
        let mut moved = spec_with_options(HashMap::new());
        moved.network = SshNetwork::Tailnet;
        keep_what_connections_found(&record, &mut moved);
        assert!(moved.shell_integration_applied);
        assert_eq!(moved.shell_integration_unsupported.as_deref(), Some("nu"));

        // Another endpoint has nothing set up on it yet.
        let mut elsewhere = record.clone();
        elsewhere.port = Some(2222);
        keep_what_connections_found(&record, &mut elsewhere);
        assert!(!elsewhere.shell_integration_applied);
        assert_eq!(elsewhere.shell_integration_unsupported, None);
    }

    #[test]
    fn only_how_a_host_is_reached_makes_it_another_domain() {
        let saved = spec_with_options(HashMap::new());
        let mut settled = saved.clone();
        settled.detect_os = true;
        settled.shell_integration = true;
        settled.shell_integration_applied = true;
        settled.files_auto_connect = true;
        settled.use_mosh = true;
        assert!(same_ssh_domain(&saved, &settled));

        let mut tailnet = saved.clone();
        tailnet.network = SshNetwork::Tailnet;
        let mut connect = saved.clone();
        connect.multiplexing = true;
        let mut port = saved.clone();
        port.port = Some(2222);
        let mut key = saved.clone();
        key.identity_file = Some("/home/user/.ssh/id_example".to_string());
        let mut password = saved.clone();
        password.password = Some("sealed".to_string());
        for changed in [tailnet, connect, port, key, password] {
            assert!(!same_ssh_domain(&saved, &changed), "{changed:?}");
        }
    }

    fn spec_with_options(ssh_options: HashMap<String, String>) -> SshHostSpec {
        SshHostSpec {
            label: "t".to_string(),
            host: "example.com".to_string(),
            port: None,
            other_addresses: Vec::new(),
            network: Default::default(),
            username: None,
            identity_file: None,
            password: None,
            ssh_options,
            multiplexing: false,
            default_workspace: None,
            detect_os: false,
            detected_distro: None,
            use_mosh: false,
            mosh_server_command: default_mosh_server_command(),
            shell_integration: false,
            shell_integration_applied: false,
            shell_integration_unsupported: None,
            shell_integration_failure: None,
            files_auto_connect: false,
        }
    }

    #[test]
    fn build_ssh_domain_injects_default_connect_timeout() {
        let dom = build_ssh_domain(&spec_with_options(HashMap::new()));
        assert_eq!(
            dom.ssh_option.get("connecttimeout").map(String::as_str),
            Some("10"),
            "an unreachable host should fail fast via a default connect timeout"
        );
    }

    #[test]
    fn build_ssh_domain_preserves_user_connect_timeout() {
        let mut options = HashMap::new();
        // User-provided key with mixed case; build_ssh_domain lowercases it.
        options.insert("ConnectTimeout".to_string(), "3".to_string());
        let dom = build_ssh_domain(&spec_with_options(options));
        assert_eq!(
            dom.ssh_option.get("connecttimeout").map(String::as_str),
            Some("3"),
            "an explicit connecttimeout override must win over the default"
        );
    }

    #[test]
    fn build_mosh_args_plain_host() {
        let spec = spec_with_options(HashMap::new());
        // host-only (no user/port/identity): just `mosh host`.
        assert_eq!(build_mosh_args(&spec), vec!["mosh", "example.com"]);
    }

    #[test]
    fn build_mosh_args_includes_user() {
        let mut spec = spec_with_options(HashMap::new());
        spec.username = Some("deploy".to_string());
        assert_eq!(build_mosh_args(&spec), vec!["mosh", "deploy@example.com"]);
    }

    #[test]
    fn build_mosh_args_maps_port_and_identity_into_ssh() {
        let mut spec = spec_with_options(HashMap::new());
        spec.username = Some("deploy".to_string());
        spec.port = Some(2222);
        spec.identity_file = Some("/home/me/.ssh/prod".to_string());
        assert_eq!(
            build_mosh_args(&spec),
            vec![
                "mosh",
                "--ssh=ssh -p 2222 -i /home/me/.ssh/prod",
                "deploy@example.com",
            ]
        );
    }

    #[test]
    fn build_mosh_args_quotes_identity_with_spaces() {
        let mut spec = spec_with_options(HashMap::new());
        spec.identity_file = Some("/home/me/.ssh/prod key".to_string());
        assert_eq!(
            build_mosh_args(&spec),
            vec![
                "mosh",
                "--ssh=ssh -i '/home/me/.ssh/prod key'",
                "example.com",
            ]
        );
    }

    #[test]
    fn build_mosh_args_omits_default_port_22() {
        let mut spec = spec_with_options(HashMap::new());
        spec.port = Some(22);
        // Port 22 is the default, so no `--ssh` is needed.
        assert_eq!(build_mosh_args(&spec), vec!["mosh", "example.com"]);
    }

    #[test]
    fn parse_mosh_connect_finds_line_in_noisy_output() {
        assert_eq!(
            parse_mosh_connect("banner\nMOSH CONNECT 60001 abc123\nready"),
            Some((60001, "abc123".to_string()))
        );
    }

    #[test]
    fn parse_mosh_connect_rejects_invalid_output() {
        assert_eq!(parse_mosh_connect("MOSH CONNECT nope abc123"), None);
        assert_eq!(parse_mosh_connect("MOSH CONNECT 60001"), None);
        assert_eq!(parse_mosh_connect("nothing useful"), None);
    }

    #[test]
    fn parse_mosh_connect_skips_malformed_connect_lines() {
        assert_eq!(
            parse_mosh_connect("MOSH CONNECT nope abc123\nMOSH CONNECT 60001 good-key"),
            Some((60001, "good-key".to_string()))
        );
        assert_eq!(
            parse_mosh_connect("MOSH CONNECT 60001\nMOSH CONNECT 60002 better-key"),
            Some((60002, "better-key".to_string()))
        );
    }

    #[test]
    fn build_mosh_client_spawn_puts_key_only_in_environment() {
        let spec = spec_with_options(HashMap::new());
        let spawn = build_mosh_client_spawn(
            &spec,
            "example.com".to_string(),
            60001,
            "abc123".to_string(),
        );
        assert_eq!(
            spawn.args,
            Some(vec![
                "mosh-client".to_string(),
                "example.com".to_string(),
                "60001".to_string(),
            ])
        );
        assert_eq!(
            spawn
                .set_environment_variables
                .get("MOSH_KEY")
                .map(String::as_str),
            Some("abc123")
        );
        assert!(!spawn
            .args
            .as_ref()
            .unwrap()
            .iter()
            .any(|arg| arg.contains("abc123")));
        assert_eq!(
            spawn.domain,
            SpawnTabDomain::DomainName("local".to_string())
        );
    }

    #[test]
    fn use_mosh_defaults_false_when_absent() {
        // Existing saved hosts (serialized before `use_mosh` existed) must load
        // with mosh off.
        let spec: SshHostSpec =
            serde_json::from_str(r#"{"label":"t","host":"example.com"}"#).unwrap();
        assert!(!spec.use_mosh);
        assert_eq!(spec.mosh_server_command, DEFAULT_MOSH_SERVER_COMMAND);
    }
}
