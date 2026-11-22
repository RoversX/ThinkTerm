//! Where the node is, as the process holding it -- `thinkterm-tailnet` --
//! last saw. It writes that to a file in the runtime folder, which is how the
//! app and `thinkterm tailnet` learn what the node is doing without asking it.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub use thinkterm_tailnet_engine::{Family, Peer, Status};

/// AddressFamily in the SSH config, as the node takes it.
pub(crate) fn family(family: wezterm_ssh::AddressFamily) -> Family {
    match family {
        wezterm_ssh::AddressFamily::Any => Family::Any,
        wezterm_ssh::AddressFamily::Inet => Family::V4,
        wezterm_ssh::AddressFamily::Inet6 => Family::V6,
    }
}

/// The status file's contents.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Report {
    /// The process that wrote it, to tell a report from a process that has
    /// since gone.
    pub pid: u32,
    pub status: Status,
    pub peers: Vec<Peer>,
    /// The settings' revision ([`crate::Settings::revision`]) the process
    /// had acted on when it wrote this.
    #[serde(default)]
    pub revision: u64,
}

/// Scoped to the build profile like the mux server's socket, so a debug
/// build's report never stands in for the release server's.
pub(crate) fn report_path() -> PathBuf {
    config::RUNTIME_DIR.join(config::runtime_file_name("tailnet.json"))
}

/// The last report, if a process holding a node wrote one.
pub(crate) fn read_report() -> Option<Report> {
    let text = std::fs::read_to_string(report_path()).ok()?;
    serde_json::from_str(&text).ok()
}

/// The last report, if the process that wrote it is still running: one
/// left behind by a process that has since gone describes nothing. One
/// about a node in the tailnet also needs the node's socket to answer: the
/// process's number may have gone to another one since, after a crash or a
/// restart.
pub fn read_live_report() -> Option<Report> {
    read_report().filter(|report| {
        process_is_running(report.pid) && (!holds_node(&report.status) || node_answers())
    })
}

/// Whether only a process holding the node reports `status`.
fn holds_node(status: &Status) -> bool {
    matches!(
        status,
        Status::Starting | Status::NeedsLogin { .. } | Status::Connected { .. }
    )
}

/// Whether the process holding the node listens on its socket (see
/// crate::proxy), which it does for as long as it holds it.
fn node_answers() -> bool {
    #[cfg(unix)]
    {
        crate::settings::socket_path()
            .is_ok_and(|path| std::os::unix::net::UnixStream::connect(path).is_ok())
    }
    #[cfg(windows)]
    {
        crate::settings::socket_path()
            .is_ok_and(|path| wezterm_uds::UnixStream::connect(path).is_ok())
    }
    #[cfg(not(any(unix, windows)))]
    {
        true
    }
}

fn process_is_running(pid: u32) -> bool {
    #[cfg(unix)]
    {
        use nix::errno::Errno;
        use nix::sys::signal::kill;
        use nix::unistd::Pid;
        match i32::try_from(pid) {
            Ok(pid) => matches!(kill(Pid::from_raw(pid), None), Ok(()) | Err(Errno::EPERM)),
            Err(_) => false,
        }
    }
    #[cfg(windows)]
    {
        crate::windows::process_is_running(pid)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        true
    }
}

/// Private to this user: a login link in it would let whoever opens it
/// first decide which tailnet this machine joins. Not waited onto the disk:
/// it is rewritten as the node changes, and means nothing after a restart.
/// On Windows it takes the permissions of the folder it is in, as the
/// settings do (see crate::settings).
#[cfg(all(
    feature = "node",
    any(target_os = "macos", target_os = "linux", target_os = "windows")
))]
pub(crate) fn write_report(report: &Report) {
    let result = serde_json::to_vec_pretty(report)
        .map_err(anyhow::Error::from)
        .and_then(|bytes| crate::settings::write_private(&report_path(), &bytes, false));
    if let Err(err) = result {
        log::warn!("tailnet: writing the status file: {err:#}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    fn peer() -> Peer {
        Peer {
            name: "server-a".into(),
            fqdn: "server-a.example.ts.net".into(),
            ipv4: Some(Ipv4Addr::new(100, 64, 0, 2)),
            ipv6: None,
            online: Some(true),
            ..Default::default()
        }
    }

    #[test]
    fn a_peer_answers_to_its_names_and_addresses() {
        let peer = peer();
        assert!(peer.answers_to("server-a"));
        assert!(peer.answers_to("SERVER-A"));
        assert!(peer.answers_to("server-a.example.ts.net."));
        assert!(peer.answers_to("100.64.0.2"));
        assert!(!peer.answers_to("100.64.0.3"));
        assert!(!peer.answers_to("server-b"));
        assert!(!peer.answers_to("server-a.example.com"));
        assert!(peer.in_domain("example.ts.net"));
        assert!(peer.in_domain("EXAMPLE.ts.net."));
        assert!(!peer.in_domain("other.ts.net"));
        assert!(!peer.in_domain("ts.net"));
    }

    #[test]
    fn report_round_trips_through_json() {
        let report = Report {
            pid: 42,
            status: Status::Connected {
                name: "devbox".into(),
                address: Some(IpAddr::V4(Ipv4Addr::new(100, 64, 0, 1))),
            },
            peers: vec![peer()],
            revision: 7,
        };
        let json = serde_json::to_string(&report).unwrap();
        assert!(json.contains("\"state\":\"connected\""));
        assert_eq!(serde_json::from_str::<Report>(&json).unwrap(), report);
    }

    #[test]
    fn a_machine_keeps_what_it_says_it_is() {
        let phone = Peer {
            os: "iOS".into(),
            model: "iPhone15,2".into(),
            ..peer()
        };
        let json = serde_json::to_string(&phone).unwrap();
        assert_eq!(serde_json::from_str::<Peer>(&json).unwrap(), phone);
        // Written before these were kept: it says nothing about itself.
        let old = r#"{"name":"server-a","fqdn":"server-a.example.ts.net","ipv4":"100.64.0.2","ipv6":null,"online":true}"#;
        assert_eq!(serde_json::from_str::<Peer>(old).unwrap(), peer());
    }
}
