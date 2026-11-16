//! Where a node is and the machines it sees: what whoever holds a node
//! says of it, to the rest of its process or to a file other processes
//! read.

use serde::{Deserialize, Serialize};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Status {
    /// No node runs in this process.
    #[default]
    Off,
    /// Reaching the control server.
    Starting,
    /// The control server wants a person to approve this machine at `url`.
    NeedsLogin { url: String },
    Connected {
        /// This machine's name in the tailnet.
        name: String,
        address: Option<IpAddr>,
    },
    /// It stopped trying, or is waiting to try again.
    Failed { error: String },
    /// This process should hold the node, but another one on this machine
    /// does; this one takes over when that one lets go.
    HeldElsewhere,
}

/// What a node not let in says (`Status::Failed`): the control server
/// knows the device -- someone logged in for it, or it came with an auth
/// key -- and has not let it in. In a tailnet that approves its devices
/// that is the wait for an admin; with an auth key it may be the key.
/// The node asks again by itself, so one approved meanwhile gets in.
pub const NOT_LET_IN: &str =
    "waiting for this device to be approved in the tailnet's admin console (if it joined with an auth key, the key may be wrong, used up or expired)";

impl Status {
    /// Whether the node waits to be let in (`NOT_LET_IN`): not an error
    /// to whoever looks at it, but something for a person to do.
    pub fn awaits_approval(&self) -> bool {
        matches!(self, Status::Failed { error } if error == NOT_LET_IN)
    }
}

/// Another machine in the tailnet.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Peer {
    /// What a host's address can say to reach it.
    pub name: String,
    /// The name with the tailnet's domain, for when two machines share one.
    pub fqdn: String,
    pub ipv4: Option<Ipv4Addr>,
    pub ipv6: Option<Ipv6Addr>,
    /// None while the control server has not said.
    pub online: Option<bool>,
    /// What the machine says it runs, as Tailscale names it: "macOS",
    /// "iOS", "android", "linux", "windows"... Empty when it does not say,
    /// as in a report from before it was kept.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub os: String,
    /// Its Linux distribution ("ubuntu"), when it says.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub distro: String,
    /// The model of a phone or tablet ("iPad13,4"), when it says.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub model: String,
}

/// Which of a machine's addresses a connection may go to, as AddressFamily
/// in the SSH config says.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Family {
    #[default]
    Any,
    V4,
    V6,
}

impl Family {
    #[cfg(feature = "node")]
    pub fn allows(self, ip: IpAddr) -> bool {
        match self {
            Family::Any => true,
            Family::V4 => ip.is_ipv4(),
            Family::V6 => ip.is_ipv6(),
        }
    }
}

impl Peer {
    /// The address a connection to it goes to.
    pub fn address(&self) -> Option<IpAddr> {
        self.address_in(Family::Any)
    }

    /// The address a connection to it goes to, of those `family` allows.
    pub fn address_in(&self, family: Family) -> Option<IpAddr> {
        let ipv4 = self.ipv4.map(IpAddr::V4);
        let ipv6 = self.ipv6.map(IpAddr::V6);
        match family {
            Family::Any => ipv4.or(ipv6),
            Family::V4 => ipv4,
            Family::V6 => ipv6,
        }
    }

    /// Whether `host`, as a host's address or SSH config spells it, names
    /// this machine.
    pub fn answers_to(&self, host: &str) -> bool {
        let host = host.trim_end_matches('.');
        match host.parse::<IpAddr>() {
            Ok(IpAddr::V4(ip)) => self.ipv4 == Some(ip),
            Ok(IpAddr::V6(ip)) => self.ipv6 == Some(ip),
            Err(_) => self.name.eq_ignore_ascii_case(host) || self.fqdn.eq_ignore_ascii_case(host),
        }
    }

    /// Whether its full name is in `domain`, a tailnet's own: one shared in
    /// from another tailnet keeps that one's.
    pub fn in_domain(&self, domain: &str) -> bool {
        self.fqdn
            .trim_end_matches('.')
            .split_once('.')
            .is_some_and(|(_, rest)| rest.eq_ignore_ascii_case(domain.trim_end_matches('.')))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_node_not_let_in_waits_rather_than_fails() {
        assert!(Status::Failed { error: NOT_LET_IN.into() }.awaits_approval());
        assert!(!Status::Failed { error: "the control server could not be reached".into() }.awaits_approval());
        assert!(!Status::NeedsLogin { url: "https://login.example/a".into() }.awaits_approval());
        assert!(!Status::Starting.awaits_approval());
    }
}
