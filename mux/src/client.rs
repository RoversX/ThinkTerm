use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::SystemTime;

// The types moved to thinkterm-proto (they travel on the wire); this module
// keeps the process-facing constructor, which needs hostname, getpid and the
// ssh agent proxy -- none of which belong in the wire crate.
pub use thinkterm_proto::{ClientId, ClientInfo};

static CLIENT_ID: AtomicUsize = AtomicUsize::new(0);
lazy_static::lazy_static! {
    static ref EPOCH: u64 = SystemTime::now()
                                .duration_since(SystemTime::UNIX_EPOCH)
                                .unwrap().as_secs();
}

/// Identify this process as a client. Formerly `ClientId::new()`; it cannot
/// live on the type any more because the type moved out of this crate.
pub fn generate_client_id() -> ClientId {
    let id = CLIENT_ID.fetch_add(1, Ordering::Relaxed);
    ClientId {
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
