//! Shared SSH host catalog and mux-domain translation.

use anyhow::{Context, Result};
use config::{SshDomain, SshMultiplexing};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::Path;

pub type SshHostId = String;

/// The ssh option carrying a host's other addresses to the connection:
/// wezterm-ssh's `OTHER_ADDRESSES`.
pub const OTHER_ADDRESSES_OPTION: &str = "thinkterm_other_addresses";
pub const NETWORK_OPTION: &str = "thinkterm_network";
pub const DEFAULT_MOSH_SERVER_COMMAND: &str = "mosh-server new -s -l LANG=en_US.UTF-8";

#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum SshNetwork {
    #[default]
    System,
    Tailnet,
}

impl SshNetwork {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Tailnet => "tailnet",
        }
    }
}

/// The picture a host is shown by where servers are listed, such as the
/// Space menu: what kind of machine it is, picked by the person.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HostIcon {
    Server,
    Cloud,
    Globe,
    Laptop,
    MacMini,
    Desktop,
    /// A small board: a Raspberry Pi and the like.
    Chip,
    Container,
}

impl HostIcon {
    /// Every icon, in the order the picker offers them.
    pub const ALL: [HostIcon; 8] = [
        Self::Server,
        Self::Cloud,
        Self::Globe,
        Self::Laptop,
        Self::MacMini,
        Self::Desktop,
        Self::Chip,
        Self::Container,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Server => "server",
            Self::Cloud => "cloud",
            Self::Globe => "globe",
            Self::Laptop => "laptop",
            Self::MacMini => "mac_mini",
            Self::Desktop => "desktop",
            Self::Chip => "chip",
            Self::Container => "container",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|icon| icon.as_str() == name)
    }
}

/// `SshHostSpec::icon` as written: by name, so that one a newer version
/// added reads back as no icon rather than failing the whole host list.
mod host_icon_serde {
    use super::HostIcon;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(icon: &Option<HostIcon>, s: S) -> Result<S::Ok, S::Error> {
        match icon {
            Some(icon) => s.serialize_some(icon.as_str()),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<HostIcon>, D::Error> {
        Ok(Option::<String>::deserialize(d)?
            .as_deref()
            .and_then(HostIcon::from_name))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SshHostSpec {
    pub label: String,
    pub host: String,
    #[serde(default)]
    pub network: SshNetwork,
    #[serde(default)]
    pub port: Option<u16>,
    /// Other addresses the host answers at -- a LAN name, a Tailscale name,
    /// a public address with a forwarded port -- tried after `host`, in
    /// order, for whichever answers first. Each is `host` or `host:port`;
    /// without a port, `port` applies. The host key is checked against
    /// `host` whichever one carries the connection.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub other_addresses: Vec<String>,
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
    /// Whether the host's shell should report each command it runs, so a tab
    /// can show it over plain ssh: ThinkTerm installs its shell integration
    /// there (`mux::shell_integration`).
    #[serde(default)]
    pub shell_integration: bool,
    /// The `shell_integration` setting as last carried out on the host: a
    /// change is made at the next connection and only recorded here once it
    /// has been.
    #[serde(default)]
    pub shell_integration_applied: bool,
    /// The login shell that setup found the integration has no hooks for, in
    /// which case it left nothing on the host.
    #[serde(default)]
    pub shell_integration_unsupported: Option<String>,
    /// Why the last connection could not carry `shell_integration` out, kept
    /// until one does; the next connection tries again.
    #[serde(default)]
    pub shell_integration_failure: Option<ShellIntegrationFailure>,
    /// Whether the right sidebar's Files panel connects to this host by
    /// itself, beside a terminal there, rather than once the user presses
    /// Connect.
    #[serde(default)]
    pub files_auto_connect: bool,
    /// The kind of machine it is, as picked; none shows the plain server.
    #[serde(
        default,
        with = "host_icon_serde",
        skip_serializing_if = "Option::is_none"
    )]
    pub icon: Option<HostIcon>,
}

/// Why setting a host's shell integration up failed.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ShellIntegrationFailure {
    /// A startup file the host would not let it write, from the home as
    /// `~/...` when it is in there.
    Unwritable(String),
    /// Anything else, as the error put it.
    Other(String),
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
                    icon: None,
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
    match spec.network {
        SshNetwork::System => format!("ssh:{}", endpoint(spec)),
        SshNetwork::Tailnet => format!("ssh:tailnet:{}", endpoint(spec)),
    }
}

/// The host's other addresses as they are tried: trimmed, without blanks,
/// repeats or the host's own address.
pub fn other_addresses(spec: &SshHostSpec) -> Vec<String> {
    let mut addresses: Vec<String> = Vec::new();
    for address in &spec.other_addresses {
        let address = address.trim();
        if address.is_empty() || address == spec.host || addresses.iter().any(|a| a == address) {
            continue;
        }
        addresses.push(address.to_string());
    }
    addresses
}

/// Other addresses as a person types them: separated by commas or spaces.
pub fn parse_other_addresses(text: &str) -> Vec<String> {
    text.split(|c: char| c == ',' || c.is_whitespace())
        .filter(|address| !address.is_empty())
        .map(str::to_string)
        .collect()
}

pub fn build_ssh_domain(spec: &SshHostSpec) -> SshDomain {
    let mut ssh_option = spec
        .ssh_options
        .iter()
        .map(|(key, value)| (key.to_lowercase(), value.clone()))
        .collect::<HashMap<_, _>>();
    ssh_option.insert(NETWORK_OPTION.into(), spec.network.as_str().into());
    if let Some(identity) = &spec.identity_file {
        if !identity.trim().is_empty() {
            ssh_option.insert("identityfile".into(), identity.clone());
        }
    }
    ssh_option
        .entry("connecttimeout".into())
        .or_insert_with(|| "10".into());
    let other_addresses = other_addresses(spec);
    if !other_addresses.is_empty() {
        ssh_option.insert(OTHER_ADDRESSES_OPTION.into(), other_addresses.join(" "));
    }
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
    fn a_saved_host_selects_one_network_for_all_its_addresses() {
        assert_eq!(NETWORK_OPTION, wezterm_ssh::NETWORK);
        let mut spec: SshHostSpec = serde_json::from_value(serde_json::json!({
            "label": "server-a", "host": "100.64.0.2"
        }))
        .unwrap();
        assert_eq!(spec.network, SshNetwork::System);
        let system = build_ssh_domain(&spec);
        assert_eq!(system.ssh_option[NETWORK_OPTION], "system");
        spec.network = SshNetwork::Tailnet;
        spec.other_addresses.push("server-a.example.ts.net".into());
        spec.ssh_options
            .insert(NETWORK_OPTION.into(), "system".into());
        let tailnet = build_ssh_domain(&spec);
        assert_eq!(tailnet.ssh_option[NETWORK_OPTION], "tailnet");
        assert_eq!(
            tailnet.ssh_option[OTHER_ADDRESSES_OPTION],
            "server-a.example.ts.net"
        );
        assert_ne!(system.name, tailnet.name);
        let saved = serde_json::to_string(&spec).unwrap();
        assert_eq!(
            serde_json::from_str::<SshHostSpec>(&saved).unwrap().network,
            SshNetwork::Tailnet
        );
    }

    #[test]
    fn the_option_is_the_one_the_connection_reads() {
        assert_eq!(OTHER_ADDRESSES_OPTION, wezterm_ssh::OTHER_ADDRESSES);
    }

    #[test]
    fn other_addresses_reach_the_connection_cleaned_up() {
        let mut spec: SshHostSpec =
            serde_json::from_value(serde_json::json!({"label": "myhost", "host": "myhost"}))
                .unwrap();
        assert!(
            spec.other_addresses.is_empty(),
            "a host saved before them has none"
        );
        assert!(!build_ssh_domain(&spec)
            .ssh_option
            .contains_key(OTHER_ADDRESSES_OPTION));
        assert!(!serde_json::to_string(&spec)
            .unwrap()
            .contains("other_addresses"));

        spec.other_addresses =
            parse_other_addresses("myhost.local, 203.0.113.7:2200  myhost ,myhost.local");
        assert_eq!(
            spec.other_addresses,
            vec!["myhost.local", "203.0.113.7:2200", "myhost", "myhost.local"]
        );
        assert_eq!(
            build_ssh_domain(&spec)
                .ssh_option
                .get(OTHER_ADDRESSES_OPTION)
                .map(String::as_str),
            Some("myhost.local 203.0.113.7:2200")
        );
    }

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
        assert!(
            !hosts[0].spec.files_auto_connect,
            "saved before there was one: Files waits to be asked"
        );
    }

    #[test]
    fn host_icon_round_trips_and_an_unknown_one_reads_as_none() {
        let mut spec: SshHostSpec =
            serde_json::from_str(r#"{"label":"server-a","host":"example.test"}"#).unwrap();
        assert_eq!(spec.icon, None);
        assert!(!serde_json::to_string(&spec).unwrap().contains("icon"));

        spec.icon = Some(HostIcon::MacMini);
        let saved = serde_json::to_string(&spec).unwrap();
        assert!(saved.contains(r#""icon":"mac_mini""#), "{}", saved);
        assert_eq!(
            serde_json::from_str::<SshHostSpec>(&saved).unwrap().icon,
            Some(HostIcon::MacMini)
        );

        let newer = r#"{"label":"server-a","host":"example.test","icon":"toaster"}"#;
        assert_eq!(serde_json::from_str::<SshHostSpec>(newer).unwrap().icon, None);
    }

    #[test]
    fn explicit_default_port_keeps_the_legacy_domain_name() {
        let spec = SshHostSpec {
            label: "server".into(),
            host: "example.test".into(),
            port: Some(22),
            other_addresses: Vec::new(),
            network: Default::default(),
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
            shell_integration: false,
            shell_integration_applied: false,
            shell_integration_unsupported: None,
            shell_integration_failure: None,
            files_auto_connect: false,
            icon: None,
        };
        assert_eq!(ssh_domain_name(&spec), "ssh:root@example.test");
        assert_eq!(build_ssh_domain(&spec).remote_address, "example.test:22");
    }
}
