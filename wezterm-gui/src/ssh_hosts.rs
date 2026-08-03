//! ThinkTerm SSH host address book and runtime host -> mux domain plumbing.
//!
//! `workspace_threads.json` owns workspace/thread layout only. User-created SSH hosts
//! live in `ssh_hosts.json`; system `~/.ssh/config` hosts are exposed as
//! read-only entries. Runtime domains are still registered lazily with the mux
//! via [`mux::Mux::add_domain`].

use anyhow::{bail, Context, Result};
use config::keyassignment::{SpawnCommand, SpawnTabDomain};
use config::{SshDomain, SshMultiplexing};
use filedescriptor::FileDescriptor;
use mux::domain::Domain;
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

pub type SshHostId = String;

pub const DEFAULT_MOSH_SERVER_COMMAND: &str = "mosh-server new -s -l LANG=en_US.UTF-8";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SshHostSpec {
    /// Display name shown on the host card / sidebar row.
    pub label: String,
    /// Hostname or IP address of the remote server.
    pub host: String,
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default)]
    pub username: Option<String>,
    /// Path to an SSH identity (private key) file, if any.
    #[serde(default)]
    pub identity_file: Option<String>,
    /// Optional stored password used to auto-answer the password prompt on
    /// connect. New values are stored encrypted (`enc:v1:...`); legacy
    /// plaintext values are still accepted by the connection layer.
    #[serde(default)]
    pub password: Option<String>,
    /// Extra `ssh_config` option overrides (key -> value).
    #[serde(default)]
    pub ssh_options: HashMap<String, String>,
    /// Use WezTerm's multiplexed SSH (persistent, reconnecting) when true.
    /// That requires `wezterm` installed on the remote; default to direct
    /// `ssh` (like `thinkterm ssh`) so password auth + the shell work anywhere.
    #[serde(default)]
    pub multiplexing: bool,
    /// Override the default `ssh:<host>` workspace name.
    #[serde(default)]
    pub default_workspace: Option<String>,
    /// When true, run a one-shot `cat /etc/os-release` after connecting to
    /// detect the distro and pick its icon. User-controlled (opt-in).
    #[serde(default = "default_true")]
    pub detect_os: bool,
    /// `/etc/os-release` `ID` detected after connecting; drives the OS icon.
    #[serde(default)]
    pub detected_distro: Option<String>,
    /// When true, connect with Mosh instead of WezTerm's SSH domain. ThinkTerm
    /// first tries an integrated SSH bootstrap (`mosh-server new ...`) so the
    /// stored password can be used; if that fails, it falls back to the
    /// external `mosh` wrapper for manual interaction.
    #[serde(default)]
    pub use_mosh: bool,
    /// Remote command used by an integrated mosh bootstrap path to start
    /// `mosh-server`. Kept configurable because different servers may need
    /// environment overrides or a non-default binary path.
    #[serde(default = "default_mosh_server_command")]
    pub mosh_server_command: String,
}

fn default_true() -> bool {
    true
}

fn default_mosh_server_command() -> String {
    DEFAULT_MOSH_SERVER_COMMAND.to_string()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SshHostSource {
    ThinkTerm,
    System,
}

#[derive(Debug, Clone)]
pub struct SshHostEntry {
    pub id: SshHostId,
    pub source: SshHostSource,
    pub spec: SshHostSpec,
}

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
    static ref SSH_HOST_STORE: Mutex<SshHostStore> =
        Mutex::new(load_ssh_host_store().unwrap_or_else(|err| {
            log::warn!("failed to load ThinkTerm SSH host store: {err:#}");
            SshHostStore::default()
        }));
}

pub fn ssh_hosts_store_path() -> PathBuf {
    crate::native_paths::data_file("ssh_hosts.json")
}

fn load_ssh_host_store() -> Result<SshHostStore> {
    load_ssh_host_store_from_path(&ssh_hosts_store_path())
}

fn load_ssh_host_store_from_path(path: &Path) -> Result<SshHostStore> {
    if !path.exists() {
        return Ok(SshHostStore::default());
    }
    let file = fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    serde_json::from_reader(file).with_context(|| format!("parse {}", path.display()))
}

fn save_ssh_host_store(store: &SshHostStore) -> Result<()> {
    save_ssh_host_store_to_path(&ssh_hosts_store_path(), store)
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
    let store = SSH_HOST_STORE.lock();
    store
        .hosts
        .iter()
        .map(|record| (record.id.clone(), record.spec.clone()))
        .collect()
}

pub fn list_all_hosts() -> Vec<SshHostEntry> {
    let mut entries: Vec<_> = list_hosts()
        .into_iter()
        .map(|(id, spec)| SshHostEntry {
            id,
            source: SshHostSource::ThinkTerm,
            spec,
        })
        .collect();
    entries.extend(list_system_hosts());
    entries
}

pub fn host_spec(host_id: &str) -> Option<SshHostSpec> {
    {
        let store = SSH_HOST_STORE.lock();
        if let Some(record) = store.hosts.iter().find(|record| record.id == host_id) {
            return Some(record.spec.clone());
        }
    }
    list_system_hosts()
        .into_iter()
        .find(|entry| entry.id == host_id)
        .map(|entry| entry.spec)
}

pub fn try_create_host(spec: SshHostSpec) -> Result<SshHostId> {
    let (host_id, spec) = upsert_host(spec)?;
    register_ssh_domain(&spec)?;
    Ok(host_id)
}

pub fn try_import_legacy_host(spec: SshHostSpec) -> Result<SshHostId> {
    let mut store = SSH_HOST_STORE.lock();
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
    let mut store = SSH_HOST_STORE.lock();
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

pub fn try_update_host(host_id: &str, spec: SshHostSpec) -> Result<bool> {
    if is_system_host_id(host_id) {
        return Ok(false);
    }
    let mut store = SSH_HOST_STORE.lock();
    let Some(record) = store.hosts.iter_mut().find(|record| record.id == host_id) else {
        return Ok(false);
    };
    record.spec = spec;
    save_ssh_host_store(&store)?;
    let spec = store
        .hosts
        .iter()
        .find(|record| record.id == host_id)
        .map(|record| record.spec.clone());
    drop(store);
    if let Some(spec) = spec {
        register_ssh_domain(&spec)?;
    }
    Ok(true)
}

pub fn try_remove_host(host_id: &str) -> Result<bool> {
    if is_system_host_id(host_id) {
        return Ok(false);
    }
    let mut store = SSH_HOST_STORE.lock();
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
    let mut store = SSH_HOST_STORE.lock();
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

pub fn is_system_host_id(host_id: &str) -> bool {
    host_id.starts_with("system-ssh-")
}

/// `user@host` (or `host`, with `:port` when non-default) used for the
/// human-readable domain / workspace identifiers.
fn endpoint(spec: &SshHostSpec) -> String {
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

pub fn build_mosh_fallback_spawn(spec: &SshHostSpec) -> SpawnCommand {
    SpawnCommand {
        label: Some(format!("mosh {}", spec.label)),
        args: Some(build_mosh_args(spec)),
        domain: SpawnTabDomain::DomainName("local".to_string()),
        ..Default::default()
    }
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
        domain: SpawnTabDomain::DomainName("local".to_string()),
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
    let ssh_config = mux::ssh::ssh_domain_to_ssh_config(&dom).context("build SSH config")?;
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
    format!("ssh:{}", endpoint(spec))
}

/// Translate a stored host spec into a `config::SshDomain`.
pub fn build_ssh_domain(spec: &SshHostSpec) -> SshDomain {
    let mut ssh_option = HashMap::new();
    for (key, value) in &spec.ssh_options {
        ssh_option.insert(key.to_lowercase(), value.clone());
    }
    if let Some(identity) = &spec.identity_file {
        if !identity.trim().is_empty() {
            ssh_option.insert("identityfile".to_string(), identity.clone());
        }
    }
    // Default connect timeout so an unreachable host fails fast instead of
    // hanging on the OS default TCP timeout. Users can override via ssh_options.
    ssh_option
        .entry("connecttimeout".to_string())
        .or_insert_with(|| "10".to_string());

    let remote_address = match spec.port {
        Some(port) => format!("{}:{port}", spec.host),
        None => spec.host.clone(),
    };

    SshDomain {
        name: ssh_domain_name(spec),
        remote_address,
        username: spec.username.clone(),
        multiplexing: if spec.multiplexing {
            SshMultiplexing::WezTerm
        } else {
            SshMultiplexing::None
        },
        ssh_option,
        // The derived Default is all-zero here: the documented Some(100ms)
        // predictive-echo threshold (and the read timeout) only apply when
        // the domain is deserialized from lua config. Without them, mux
        // sessions built from the host store never show local-echo
        // predictions, no matter how laggy the link.
        local_echo_threshold_ms: config::default_local_echo_threshold_ms(),
        timeout: config::default_read_timeout(),
        ..Default::default()
    }
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

pub fn list_system_hosts() -> Vec<SshHostEntry> {
    let path = config::HOME_DIR.join(".ssh").join("config");
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

fn host_id_for_host(spec: &SshHostSpec) -> SshHostId {
    let key = format!(
        "ssh:{}@{}:{}",
        spec.username.as_deref().unwrap_or(""),
        spec.host,
        spec.port.unwrap_or(22)
    );
    format!("ssh-{:x}", fnv1a(&key))
}

fn system_host_id(alias: &str) -> SshHostId {
    format!("system-ssh-{:x}", fnv1a(&format!("system:{alias}")))
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(spec
            .identity_file
            .as_deref()
            .unwrap()
            .ends_with("/.ssh/prod"));
    }

    fn spec_with_options(ssh_options: HashMap<String, String>) -> SshHostSpec {
        SshHostSpec {
            label: "t".to_string(),
            host: "example.com".to_string(),
            port: None,
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
