use crate::*;
use std::path::PathBuf;
use wezterm_dynamic::{FromDynamic, ToDynamic};

/// A web entry to the multiplexer: one TCP port that serves the browser
/// client's bundle over HTTP and accepts its WebSocket. Every WebSocket
/// must present a web token minted with `thinkterm cli web-token mint`;
/// the token equals full access as the server's user, exactly like the
/// unix socket and the TLS credentials.
///
/// Like `tls_servers`, these are read when the server starts; a config
/// reload does not open or close web ports.
#[derive(Debug, Clone, FromDynamic, ToDynamic)]
pub struct WebServer {
    /// The address:port to listen on. Loopback by default: browsers only
    /// expose WebGPU to secure contexts, and `http://localhost` is one
    /// while a plain `http://` to any other host is not.
    #[dynamic(default = "default_web_bind_address")]
    pub bind_address: String,

    /// The path to an x509 PEM encoded private key file. Setting this and
    /// `pem_cert` turns the port into `https`/`wss`.
    pub pem_private_key: Option<PathBuf>,

    /// The path to an x509 PEM encoded certificate file.
    pub pem_cert: Option<PathBuf>,

    /// The path to an x509 PEM encoded CA chain file.
    pub pem_ca: Option<PathBuf>,

    /// Where the browser client's `index.html`, JavaScript and wasm live.
    /// Defaults to the `share/thinkterm/web` directory next to the
    /// installed executable.
    pub static_dir: Option<PathBuf>,

    /// Where minted tokens are kept (as digests, never the tokens
    /// themselves). Unset means memory only: a restart of the server
    /// forgets every token.
    pub token_file: Option<PathBuf>,

    /// Origins allowed to open the WebSocket. Empty means "this
    /// listener's own address", which is what a page served from the same
    /// port sends. A page served from anywhere else must be listed.
    #[dynamic(default)]
    pub allowed_origins: Vec<String>,

    /// Refuse to start without TLS when `bind_address` is not loopback.
    /// Off, the port still works, but a browser reaching it over plain
    /// `http://` has no WebGPU and shows nothing. Leave this on and use
    /// `ssh -L` to reach a remote server instead.
    #[dynamic(default = "default_true")]
    pub require_tls_off_loopback: bool,
}

fn default_web_bind_address() -> String {
    "127.0.0.1:8088".to_string()
}

impl Default for WebServer {
    fn default() -> Self {
        Self {
            bind_address: default_web_bind_address(),
            pem_private_key: None,
            pem_cert: None,
            pem_ca: None,
            static_dir: None,
            token_file: None,
            allowed_origins: vec![],
            require_tls_off_loopback: true,
        }
    }
}

/// The host and port of an authority (`host`, `host:port`, `[v6]:port`),
/// host lowercase with any brackets removed. `None` when it is not one.
pub fn split_authority(authority: &str) -> Option<(String, Option<u16>)> {
    let authority = authority.trim();
    if authority.is_empty() || authority.contains(['/', '?', '#', '@', ' ']) {
        return None;
    }
    let (host, port) = if let Some(rest) = authority.strip_prefix('[') {
        let (v6, after) = rest.split_once(']')?;
        match after {
            "" => (v6, None),
            p => (v6, Some(p.strip_prefix(':')?.parse::<u16>().ok()?)),
        }
    } else {
        match authority.rsplit_once(':') {
            // A bare IPv6 literal has several colons and no brackets.
            Some((host, port)) if !host.contains(':') => {
                (host, Some(port.parse::<u16>().ok()?))
            }
            _ => (authority, None),
        }
    };
    if host.is_empty() {
        return None;
    }
    Some((host.to_ascii_lowercase(), port))
}

/// The port a URL without one means.
pub fn default_port(scheme: &str) -> Option<u16> {
    match scheme {
        "http" | "ws" => Some(80),
        "https" | "wss" => Some(443),
        _ => None,
    }
}

/// A name that can only ever mean this machine.
pub fn is_loopback_host(host: &str) -> bool {
    match host.parse::<std::net::IpAddr>() {
        Ok(ip) => ip.is_loopback(),
        Err(_) => host.eq_ignore_ascii_case("localhost"),
    }
}

/// An origin taken apart: scheme, host and the port it means (the
/// scheme's default when the string carries none).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Origin {
    pub scheme: String,
    pub host: String,
    pub port: Option<u16>,
    /// The port as written, if any: a `Host` header names the same
    /// authority without a scheme to default from.
    pub port_given: Option<u16>,
}

impl Origin {
    pub fn parse(origin: &str) -> Option<Self> {
        let origin = origin.trim().trim_end_matches('/');
        let (scheme, authority) = origin.split_once("://")?;
        let scheme = scheme.to_ascii_lowercase();
        let (host, port) = split_authority(authority)?;
        Some(Self {
            port: port.or(default_port(&scheme)),
            port_given: port,
            scheme,
            host,
        })
    }

    /// The same origin, however its port was spelled.
    pub fn same_as(&self, other: &Origin) -> bool {
        self.scheme == other.scheme && self.host == other.host && self.port == other.port
    }
}

impl WebServer {
    pub fn uses_tls(&self) -> bool {
        self.pem_cert.is_some() && self.pem_private_key.is_some()
    }

    /// A certificate without its key, or the reverse.
    pub fn tls_half_configured(&self) -> bool {
        self.pem_cert.is_some() != self.pem_private_key.is_some()
    }

    pub fn scheme(&self) -> &'static str {
        if self.uses_tls() {
            "https"
        } else {
            "http"
        }
    }

    /// The host and port parts of `bind_address`.
    pub fn host_and_port(&self) -> (String, Option<u16>) {
        match self.bind_address.rsplit_once(':') {
            Some((host, port)) if !host.is_empty() => {
                let host = host.trim_start_matches('[').trim_end_matches(']');
                (host.to_string(), port.parse().ok())
            }
            _ => (self.bind_address.clone(), None),
        }
    }

    pub fn is_loopback(&self) -> bool {
        let (host, _) = self.host_and_port();
        match host.parse::<std::net::IpAddr>() {
            Ok(ip) => ip.is_loopback(),
            Err(_) => host == "localhost",
        }
    }

    /// The `host[:port]` values a browser will send in `Host:` and use in
    /// a URL. A loopback listener answers to both spellings of itself.
    /// A scheme's default port is left out, as a browser leaves it out.
    pub fn url_hosts(&self) -> Vec<String> {
        let (host, port) = self.host_and_port();
        let shown = port.filter(|p| Some(*p) != default_port(self.scheme()));
        let with_port = |h: &str| match shown {
            Some(p) => format!("{h}:{p}"),
            None => h.to_string(),
        };
        let mut hosts = vec![];
        let ip = host.parse::<std::net::IpAddr>().ok();
        match ip {
            Some(ip) if ip.is_loopback() => {
                hosts.push(with_port("127.0.0.1"));
                hosts.push(with_port("localhost"));
                hosts.push(with_port("[::1]"));
            }
            Some(ip) if ip.is_unspecified() => {
                // Bound everywhere: any name reaches it, and the listener
                // cannot enumerate them. Loopback spellings at least.
                hosts.push(with_port("127.0.0.1"));
                hosts.push(with_port("localhost"));
                hosts.push(with_port("[::1]"));
            }
            Some(std::net::IpAddr::V6(v6)) => hosts.push(with_port(&format!("[{v6}]"))),
            _ => hosts.push(with_port(&host)),
        }
        hosts
    }

    /// The origins the upgrade accepts: the configured list, or every
    /// spelling of this listener's own address.
    pub fn effective_allowed_origins(&self) -> Vec<String> {
        if !self.allowed_origins.is_empty() {
            return self.allowed_origins.clone();
        }
        let scheme = self.scheme();
        self.url_hosts()
            .into_iter()
            .map(|host| format!("{scheme}://{host}"))
            .collect()
    }

    /// The URLs a person can open to reach this listener: the configured
    /// origins when there are any (that is where the page is reached),
    /// else every spelling of the bind address.
    pub fn urls(&self) -> Vec<String> {
        if !self.allowed_origins.is_empty() {
            return self
                .allowed_origins
                .iter()
                .map(|origin| format!("{}/", origin.trim_end_matches('/')))
                .collect();
        }
        let scheme = self.scheme();
        self.url_hosts()
            .into_iter()
            .map(|host| format!("{scheme}://{host}/"))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_listener_answers_to_every_spelling_of_itself() {
        let server = WebServer::default();
        assert!(server.is_loopback());
        assert_eq!(
            server.effective_allowed_origins(),
            vec![
                "http://127.0.0.1:8088",
                "http://localhost:8088",
                "http://[::1]:8088"
            ]
        );
    }

    #[test]
    fn a_named_host_with_tls_is_https_and_only_itself() {
        let server = WebServer {
            bind_address: "mux.example.net:8443".into(),
            pem_cert: Some("/c.pem".into()),
            pem_private_key: Some("/k.pem".into()),
            ..WebServer::default()
        };
        assert!(!server.is_loopback());
        assert_eq!(server.urls(), vec!["https://mux.example.net:8443/"]);
        assert_eq!(
            server.effective_allowed_origins(),
            vec!["https://mux.example.net:8443"]
        );
    }

    #[test]
    fn configured_origins_win_and_name_the_urls() {
        let server = WebServer {
            allowed_origins: vec!["https://app.example.net".into()],
            ..WebServer::default()
        };
        assert_eq!(
            server.effective_allowed_origins(),
            vec!["https://app.example.net"]
        );
        assert_eq!(server.urls(), vec!["https://app.example.net/"]);
    }

    #[test]
    fn default_ports_are_left_out_as_browsers_leave_them_out() {
        let server = WebServer {
            bind_address: "mux.example.net:443".into(),
            pem_cert: Some("/c.pem".into()),
            pem_private_key: Some("/k.pem".into()),
            ..WebServer::default()
        };
        assert_eq!(server.urls(), vec!["https://mux.example.net/"]);
        assert_eq!(
            server.effective_allowed_origins(),
            vec!["https://mux.example.net"]
        );
    }

    #[test]
    fn authorities_and_origins_take_apart() {
        assert_eq!(split_authority("Localhost:8088"), Some(("localhost".into(), Some(8088))));
        assert_eq!(split_authority("localhost"), Some(("localhost".into(), None)));
        assert_eq!(split_authority("[::1]:9000"), Some(("::1".into(), Some(9000))));
        assert_eq!(split_authority("[::1]"), Some(("::1".into(), None)));
        assert_eq!(split_authority("::1"), Some(("::1".into(), None)));
        assert_eq!(split_authority("host:x"), None);
        assert_eq!(split_authority("a/b"), None);
        assert_eq!(split_authority(""), None);
        assert_eq!(
            Origin::parse("HTTPS://App.Example.net/"),
            Some(Origin {
                scheme: "https".into(),
                host: "app.example.net".into(),
                port: Some(443),
                port_given: None
            })
        );
        assert_eq!(
            Origin::parse("http://localhost:9000"),
            Some(Origin {
                scheme: "http".into(),
                host: "localhost".into(),
                port: Some(9000),
                port_given: Some(9000)
            })
        );
        assert_eq!(Origin::parse("null"), None);
        assert!(is_loopback_host("127.0.0.1"));
        assert!(is_loopback_host("::1"));
        assert!(is_loopback_host("LOCALHOST"));
        assert!(!is_loopback_host("10.0.0.5"));
        assert!(!is_loopback_host("localhost.example"));
    }
}
