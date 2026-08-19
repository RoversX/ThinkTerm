use crate::PaneId;
use chrono::serde::ts_seconds;
use chrono::{DateTime, Utc};
use serde::*;
use std::sync::Arc;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Hash)]
pub struct ClientId {
    pub hostname: String,
    pub username: String,
    pub pid: u32,
    pub epoch: u64,
    pub id: usize,
    pub ssh_auth_sock: Option<String>,
}

impl ClientId {
    // The constructor that fills these fields in from the running process
    // lives in mux (`mux::client::generate_client_id`); it needs hostname,
    // getpid and the ssh agent proxy, none of which belong in a wire crate.

    /// Whether two ids describe the same logical client, allowing for the
    /// transport-only decoration applied by an SSH mux proxy.
    ///
    /// The server appends ` (via proxy pid N)` to the hostname and replaces
    /// `ssh_auth_sock` so that `list-clients` can explain the route.  Neither
    /// change should make the originating renderer stop recognizing itself.
    pub fn same_logical_client(&self, other: &Self) -> bool {
        proxy_hostname_base(&self.hostname) == proxy_hostname_base(&other.hostname)
            && self.username == other.username
            && self.pid == other.pid
            && self.epoch == other.epoch
            && self.id == other.id
    }
}

fn proxy_hostname_base(hostname: &str) -> &str {
    const PREFIX: &str = " (via proxy pid ";

    let Some((base, suffix)) = hostname.rsplit_once(PREFIX) else {
        return hostname;
    };
    let Some(pid) = suffix.strip_suffix(')') else {
        return hostname;
    };
    if !pid.is_empty() && pid.bytes().all(|byte| byte.is_ascii_digit()) {
        base
    } else {
        hostname
    }
}

#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub struct ClientInfo {
    pub client_id: Arc<ClientId>,
    /// The time this client last connected
    #[serde(with = "ts_seconds")]
    pub connected_at: DateTime<Utc>,
    /// Which workspace is active
    pub active_workspace: Option<String>,
    /// The last time we received input from this client
    #[serde(with = "ts_seconds")]
    pub last_input: DateTime<Utc>,
    /// The currently-focused pane
    pub focused_pane_id: Option<PaneId>,
}

// The methods that stamp the current time are only meaningful on the side
// that owns the session (the server); SystemTime::now panics on
// wasm32-unknown-unknown, and a pure client only ever receives ClientInfo.
#[cfg(not(target_family = "wasm"))]
impl ClientInfo {
    pub fn new(client_id: Arc<ClientId>) -> Self {
        let now = utc_now();
        Self {
            client_id,
            connected_at: now,
            active_workspace: None,
            last_input: now,
            focused_pane_id: None,
        }
    }

    pub fn update_last_input(&mut self) {
        self.last_input = utc_now();
    }

    pub fn update_focused_pane(&mut self, pane_id: PaneId) {
        self.focused_pane_id.replace(pane_id);
    }
}

#[cfg(not(target_family = "wasm"))]
fn utc_now() -> DateTime<Utc> {
    let duration = std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    DateTime::<Utc>::from_timestamp(duration.as_secs() as i64, duration.subsec_nanos())
        .expect("system time should fit chrono timestamp range")
}
