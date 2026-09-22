//! Shared SSH host catalog and mux-domain translation.

use anyhow::{Context, Result};
use config::{SshDomain, SshMultiplexing};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::Path;

pub type SshHostId = String;
pub const DEFAULT_MOSH_SERVER_COMMAND: &str = "mosh-server new -s -l LANG=en_US.UTF-8";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SshHostSpec {
    pub label: String,
    pub host: String,
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub identity_file: Option<String>,
    #[serde(default)]
    pub password: Option<String>,
    #[serde(default)]
    pub ssh_options: HashMap<String, String>,
    #[serde(default)]
    pub multiplexing: bool,
    #[serde(default)]
    pub default_workspace: Option<String>,
    #[serde(default = "default_true")]
    pub detect_os: bool,
    #[serde(default)]
    pub detected_distro: Option<String>,
    #[serde(default)]
    pub use_mosh: bool,
    #[serde(default = "default_mosh_server_command")]
    pub mosh_server_command: String,
}

pub fn default_true() -> bool {
    true
}

pub fn default_mosh_server_command() -> String {
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

#[derive(Debug, Clone, Deserialize)]
struct SshHostRecord {
    id: SshHostId,
    spec: SshHostSpec,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct SshHostStore {
    #[allow(dead_code)]
    #[serde(default)]
    version: u32,
    #[serde(default)]
    hosts: Vec<SshHostRecord>,
}

pub fn saved_hosts_path() -> Result<std::path::PathBuf> {
    Ok(crate::credential_data_dir()?.join("ssh_hosts.json"))
}

pub fn list_saved_hosts() -> Result<Vec<SshHostEntry>> {
    list_saved_hosts_from_path(&saved_hosts_path()?)
}

pub fn list_saved_hosts_from_path(path: &Path) -> Result<Vec<SshHostEntry>> {
    if !path.exists() {
        return Ok(vec![]);
    }
    let file = fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let store: SshHostStore =
        serde_json::from_reader(file).with_context(|| format!("parse {}", path.display()))?;
    Ok(store
        .hosts
        .into_iter()
        .map(|record| SshHostEntry {
            id: record.id,
            source: SshHostSource::ThinkTerm,
            spec: record.spec,
        })
        .collect())
}

pub fn list_system_hosts() -> Result<Vec<SshHostEntry>> {
    list_system_hosts_from_path(&config::HOME_DIR.join(".ssh").join("config"))
}

pub fn list_system_hosts_from_path(path: &Path) -> Result<Vec<SshHostEntry>> {
    if !path.exists() {
        return Ok(vec![]);
    }
    let content = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    Ok(parse_system_ssh_config(&content))
}

pub fn list_all_hosts() -> Result<Vec<SshHostEntry>> {
    let mut entries = list_saved_hosts()?;
    entries.extend(list_system_hosts()?);
    entries.sort_by(|a, b| {
        a.spec
            .label
            .to_ascii_lowercase()
            .cmp(&b.spec.label.to_ascii_lowercase())
    });
    entries.dedup_by(|a, b| a.id == b.id);
    Ok(entries)
}

#[derive(Default)]
struct SystemHostBlock {
    aliases: Vec<String>,
    user: Option<String>,
    port: Option<u16>,
    identity_file: Option<String>,
}

pub fn parse_system_ssh_config(content: &str) -> Vec<SshHostEntry> {
    fn flush(block: &mut Option<SystemHostBlock>, out: &mut Vec<SshHostEntry>) {
        let Some(block) = block.take() else { return };
        for alias in block.aliases {
            if alias.starts_with('!') || alias.contains('*') || alias.contains('?') {
                continue;
            }
            out.push(SshHostEntry {
                id: system_host_id(&alias),
                source: SshHostSource::System,
                spec: SshHostSpec {
                    label: alias.clone(),
                    host: alias,
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
                },
            });
        }
    }

    let mut entries = Vec::new();
    let mut current: Option<SystemHostBlock> = None;
    for raw in content.lines() {
        let line = raw.split_once('#').map_or(raw, |(head, _)| head).trim();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.split_whitespace();
        let Some(key) = parts.next() else { continue };
        match key.to_ascii_lowercase().as_str() {
            "host" => {
                flush(&mut current, &mut entries);
                current = Some(SystemHostBlock {
                    aliases: parts.map(str::to_string).collect(),
                    ..Default::default()
                });
            }
            "user" => {
                if let Some(block) = current.as_mut() {
                    block.user = parts.next().map(str::to_string);
                }
            }
            "port" => {
                if let Some(block) = current.as_mut() {
                    block.port = parts.next().and_then(|value| value.parse().ok());
                }
            }
            "identityfile" => {
                if let Some(block) = current.as_mut() {
                    block.identity_file = parts.next().map(expand_home);
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

fn expand_home(value: &str) -> String {
    if value == "~" {
        config::HOME_DIR.to_string_lossy().into_owned()
    } else if let Some(rest) = value.strip_prefix("~/") {
        config::HOME_DIR.join(rest).to_string_lossy().into_owned()
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

fn system_host_id(alias: &str) -> SshHostId {
    format!("system-ssh-{:x}", fnv1a(&format!("system:{alias}")))
}

pub fn endpoint(spec: &SshHostSpec) -> String {
    let host = match spec.port {
        Some(port) if port != 22 => format!("{}:{port}", spec.host),
        _ => spec.host.clone(),
    };
    match &spec.username {
        Some(user) if !user.is_empty() => format!("{user}@{host}"),
        _ => host,
    }
}

pub fn ssh_domain_name(spec: &SshHostSpec) -> String {
    format!("ssh:{}", endpoint(spec))
}

pub fn build_ssh_domain(spec: &SshHostSpec) -> SshDomain {
    let mut ssh_option = spec
        .ssh_options
        .iter()
        .map(|(key, value)| (key.to_lowercase(), value.clone()))
        .collect::<HashMap<_, _>>();
    if let Some(identity) = &spec.identity_file {
        if !identity.trim().is_empty() {
            ssh_option.insert("identityfile".into(), identity.clone());
        }
    }
    ssh_option
        .entry("connecttimeout".into())
        .or_insert_with(|| "10".into());
    SshDomain {
        name: ssh_domain_name(spec),
        remote_address: match spec.port {
            Some(port) => format!("{}:{port}", spec.host),
            None => spec.host.clone(),
        },
        username: spec.username.clone(),
        multiplexing: if spec.multiplexing {
            SshMultiplexing::WezTerm
        } else {
            SshMultiplexing::None
        },
        ssh_option,
        local_echo_threshold_ms: config::default_local_echo_threshold_ms(),
        timeout: config::default_read_timeout(),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_only_concrete_system_hosts() {
        let entries = parse_system_ssh_config(
            "Host prod *.internal\n User deploy\n Port 2202\n IdentityFile ~/.ssh/prod\n",
        );
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].spec.host, "prod");
        assert_eq!(entries[0].spec.username.as_deref(), Some("deploy"));
        assert_eq!(entries[0].spec.port, Some(2202));
    }

    #[test]
    fn saved_catalog_schema_is_frontend_neutral() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ssh_hosts.json");
        std::fs::write(
            &path,
            r#"{"version":1,"hosts":[{"id":"h","spec":{"label":"AM2","host":"host","multiplexing":true}}]}"#,
        )
        .unwrap();
        let hosts = list_saved_hosts_from_path(&path).unwrap();
        assert_eq!(hosts[0].spec.label, "AM2");
        assert!(hosts[0].spec.multiplexing);
    }

    #[test]
    fn explicit_default_port_keeps_the_legacy_domain_name() {
        let spec = SshHostSpec {
            label: "server".into(),
            host: "example.test".into(),
            port: Some(22),
            username: Some("root".into()),
            identity_file: None,
            password: None,
            ssh_options: HashMap::new(),
            multiplexing: true,
            default_workspace: None,
            detect_os: true,
            detected_distro: None,
            use_mosh: false,
            mosh_server_command: default_mosh_server_command(),
        };
        assert_eq!(ssh_domain_name(&spec), "ssh:root@example.test");
        assert_eq!(build_ssh_domain(&spec).remote_address, "example.test:22");
    }
}
