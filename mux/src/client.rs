use crate::PaneId;
use chrono::serde::ts_seconds;
use chrono::{DateTime, Utc};
use serde::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::SystemTime;

static CLIENT_ID: AtomicUsize = AtomicUsize::new(0);
lazy_static::lazy_static! {
    static ref EPOCH: u64 = SystemTime::now()
                                .duration_since(SystemTime::UNIX_EPOCH)
                                .unwrap().as_secs();
}

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
    pub fn new() -> Self {
        let id = CLIENT_ID.fetch_add(1, Ordering::Relaxed);
        Self {
            hostname: hostname::get()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|_| "localhost".to_string()),
            username: config::username_from_env().unwrap_or_else(|_| "somebody".to_string()),
            pid: unsafe { libc::getpid() as u32 },
            epoch: *EPOCH,
            id,
            ssh_auth_sock: crate::AgentProxy::default_ssh_auth_sock(),
        }
    }

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

fn utc_now() -> DateTime<Utc> {
    let duration = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    DateTime::<Utc>::from_timestamp(duration.as_secs() as i64, duration.subsec_nanos())
        .expect("system time should fit chrono timestamp range")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client_id(hostname: &str, id: usize, ssh_auth_sock: Option<&str>) -> ClientId {
        ClientId {
            hostname: hostname.to_string(),
            username: "test-user".to_string(),
            pid: 42,
            epoch: 123,
            id,
            ssh_auth_sock: ssh_auth_sock.map(str::to_string),
        }
    }

    #[test]
    fn proxied_identity_matches_its_originating_client() {
        let local = client_id("myhost.local", 7, Some("/local/agent"));
        let proxied = client_id(
            "myhost.local (via proxy pid 190892)",
            7,
            Some("/remote/agent"),
        );

        assert!(local.same_logical_client(&proxied));
        assert!(proxied.same_logical_client(&local));
    }

    #[test]
    fn proxy_decoration_does_not_merge_distinct_clients() {
        let local = client_id("myhost.local", 7, None);
        let other = client_id("myhost.local (via proxy pid 190892)", 8, None);

        assert!(!local.same_logical_client(&other));
    }

    #[test]
    fn malformed_proxy_decoration_is_part_of_the_hostname() {
        let local = client_id("myhost.local", 7, None);
        let malformed = client_id("myhost.local (via proxy pid unknown)", 7, None);

        assert!(!local.same_logical_client(&malformed));
    }
}
