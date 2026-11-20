use crate::config::validate_domain_name;
use crate::*;
use luahelper::impl_lua_conversion_dynamic;
use std::fmt::Display;
use std::str::FromStr;
use wezterm_dynamic::{FromDynamic, ToDynamic};

#[derive(Debug, Clone, Copy, FromDynamic, ToDynamic)]
pub enum SshBackend {
    Ssh2,
    LibSsh,
}

impl Default for SshBackend {
    fn default() -> Self {
        Self::LibSsh
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, FromDynamic, ToDynamic)]
pub enum SshMultiplexing {
    WezTerm,
    None,
    // TODO: Tmux-cc in the future?
}

impl Default for SshMultiplexing {
    fn default() -> Self {
        Self::WezTerm
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, FromDynamic, ToDynamic)]
pub enum Shell {
    /// Unknown command shell: no assumptions can be made
    Unknown,

    /// Posix shell compliant, such that `cd DIR ; exec CMD` behaves
    /// as it does in the bourne shell family of shells
    Posix,
    // TODO: Cmd, PowerShell in the future?
}

impl Default for Shell {
    fn default() -> Self {
        Self::Unknown
    }
}

#[derive(Default, Clone, FromDynamic, ToDynamic)]
pub struct SshDomain {
    /// The name of this specific domain.  Must be unique amongst
    /// all types of domain in the configuration file.
    #[dynamic(validate = "validate_domain_name")]
    pub name: String,

    /// identifies the host:port pair of the remote server.
    pub remote_address: String,

    /// Whether agent auth should be disabled
    #[dynamic(default)]
    pub no_agent_auth: bool,

    /// The username to use for authenticating with the remote host
    pub username: Option<String>,

    /// If true, connect to this domain automatically at startup
    #[dynamic(default)]
    pub connect_automatically: bool,

    #[dynamic(default = "default_read_timeout")]
    pub timeout: Duration,

    #[dynamic(default = "default_local_echo_threshold_ms")]
    pub local_echo_threshold_ms: Option<u64>,

    /// Show time since last response when waiting for a response.
    /// It is recommended to use
    /// <https://wezterm.org/config/lua/pane/get_metadata.html#since_last_response_ms>
    /// instead.
    #[dynamic(default)]
    pub overlay_lag_indicator: bool,

    /// The path to a compatible ThinkTerm/WezTerm mux binary on the remote host
    pub remote_wezterm_path: Option<String>,
    /// Override the entire `thinkterm cli proxy` invocation that would otherwise
    /// be computed from remote_wezterm_path and other information.
    pub override_proxy_command: Option<String>,

    pub ssh_backend: Option<SshBackend>,

    /// If false, then don't use a multiplexer connection,
    /// just connect directly using ssh. This doesn't require
    /// that the remote host have a compatible mux binary installed, and is equivalent
    /// to using `thinkterm ssh` to connect.
    #[dynamic(default)]
    pub multiplexing: SshMultiplexing,

    /// ssh_config option values
    #[dynamic(default)]
    pub ssh_option: HashMap<String, String>,

    /// Optional stored password used to auto-answer the first password
    /// (non-echo) auth prompt. Populated at runtime when the domain is
    /// built from a ThinkTerm SSH host entry; not intended for lua config
    /// (prefer key auth there).
    #[dynamic(default)]
    pub stored_password: Option<String>,

    /// Hosts to go through on the way to `remote_address`, nearest first,
    /// as the `ProxyJump` ssh option does: each is reached through the one
    /// before it, and only passes the connection on. Set when the domain
    /// is built from a ThinkTerm SSH host entry reached through another;
    /// in lua config, `ssh_option = { proxyjump = "user@host" }` does the
    /// same. When set, the `proxyjump` and `proxycommand` options for this
    /// host itself are not used.
    #[dynamic(default)]
    pub jump_hosts: Vec<SshJumpHost>,

    pub default_prog: Option<Vec<String>>,

    #[dynamic(default)]
    pub assume_shell: Shell,
}
impl_lua_conversion_dynamic!(SshDomain);

/// A stored password as configuration is shown, in logs among other
/// places: whether there is one, never what it is.
fn redacted(password: &Option<String>) -> Option<&'static str> {
    password.as_ref().map(|_| "<redacted>")
}

impl std::fmt::Debug for SshDomain {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.debug_struct("SshDomain")
            .field("name", &self.name)
            .field("remote_address", &self.remote_address)
            .field("no_agent_auth", &self.no_agent_auth)
            .field("username", &self.username)
            .field("connect_automatically", &self.connect_automatically)
            .field("timeout", &self.timeout)
            .field("local_echo_threshold_ms", &self.local_echo_threshold_ms)
            .field("overlay_lag_indicator", &self.overlay_lag_indicator)
            .field("remote_wezterm_path", &self.remote_wezterm_path)
            .field("override_proxy_command", &self.override_proxy_command)
            .field("ssh_backend", &self.ssh_backend)
            .field("multiplexing", &self.multiplexing)
            .field("ssh_option", &self.ssh_option)
            .field("stored_password", &redacted(&self.stored_password))
            .field("jump_hosts", &self.jump_hosts)
            .field("default_prog", &self.default_prog)
            .field("assume_shell", &self.assume_shell)
            .finish()
    }
}

/// A host an SSH domain is reached through: see `SshDomain::jump_hosts`.
#[derive(Default, Clone, PartialEq, Eq, FromDynamic, ToDynamic)]
pub struct SshJumpHost {
    /// identifies the host:port pair of the jump host.
    pub remote_address: String,

    /// The username to sign in to the jump host with
    pub username: Option<String>,

    /// ssh_config option values for the jump host. `proxycommand` is used
    /// only for the first jump host: the others are reached through it.
    #[dynamic(default)]
    pub ssh_option: HashMap<String, String>,

    /// Like `SshDomain::stored_password`, for the jump host's own password
    /// prompts. A password kept for one host is never sent to another.
    #[dynamic(default)]
    pub stored_password: Option<String>,
}

impl std::fmt::Debug for SshJumpHost {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.debug_struct("SshJumpHost")
            .field("remote_address", &self.remote_address)
            .field("username", &self.username)
            .field("ssh_option", &self.ssh_option)
            .field("stored_password", &redacted(&self.stored_password))
            .finish()
    }
}

#[cfg(test)]
mod debug_tests {
    use super::*;

    #[test]
    fn stored_passwords_never_show() {
        let dom = SshDomain {
            stored_password: Some("host secret".to_string()),
            jump_hosts: vec![SshJumpHost {
                stored_password: Some("jump secret".to_string()),
                ..Default::default()
            }],
            ..Default::default()
        };
        let shown = format!("{dom:?}");
        assert!(!shown.contains("secret"), "{}", shown);
        assert!(shown.contains("<redacted>"), "{}", shown);
    }
}

impl SshDomain {
    pub fn default_domains() -> Vec<Self> {
        let mut config = wezterm_ssh::Config::new();
        config.add_default_config_files();

        let mut plain_ssh = vec![];
        let mut mux_ssh = vec![];
        for host in config.enumerate_hosts() {
            plain_ssh.push(Self {
                name: format!("SSH:{host}"),
                remote_address: host.to_string(),
                multiplexing: SshMultiplexing::None,
                local_echo_threshold_ms: default_local_echo_threshold_ms(),
                ..SshDomain::default()
            });

            mux_ssh.push(Self {
                name: format!("SSHMUX:{host}"),
                remote_address: host.to_string(),
                multiplexing: SshMultiplexing::WezTerm,
                local_echo_threshold_ms: default_local_echo_threshold_ms(),
                ..SshDomain::default()
            });
        }

        plain_ssh.append(&mut mux_ssh);
        plain_ssh
    }
}

#[derive(Clone, Debug)]
pub struct SshParameters {
    pub username: Option<String>,
    pub host_and_port: String,
}

impl Display for SshParameters {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(user) = &self.username {
            write!(f, "{}@{}", user, self.host_and_port)
        } else {
            write!(f, "{}", self.host_and_port)
        }
    }
}

pub fn username_from_env() -> anyhow::Result<String> {
    #[cfg(unix)]
    const USER: &str = "USER";
    #[cfg(windows)]
    const USER: &str = "USERNAME";

    std::env::var(USER).with_context(|| format!("while resolving {} env var", USER))
}

impl FromStr for SshParameters {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let parts: Vec<&str> = s.split('@').collect();

        if parts.len() == 2 {
            Ok(Self {
                username: Some(parts[0].to_string()),
                host_and_port: parts[1].to_string(),
            })
        } else if parts.len() == 1 {
            Ok(Self {
                username: None,
                host_and_port: parts[0].to_string(),
            })
        } else {
            bail!("failed to parse ssh parameters from `{}`", s);
        }
    }
}
