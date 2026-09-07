//! Who owns the tab this browser mirrors, as the server has told us. Pure,
//! so it is tested natively; the WebSocket link keeps one and feeds it.

use thinkterm_proto::{ClientId, TabId};
use wezterm_term::TerminalSize;

/// What the server has told us about who owns the tab we mirror.
#[derive(Debug, Default, Clone)]
pub struct Lease {
    pub tab_id: Option<TabId>,
    pub me: Option<ClientId>,
    pub mode: Option<codec::FrontendAccessMode>,
    /// Connection-wide owner (Handoff mode).
    pub owner: Option<ClientId>,
    /// Per-tab owner (TmuxLatest mode).
    pub tab_owner: Option<ClientId>,
    pub canonical_size: Option<TerminalSize>,
    /// The grid this browser last reported.
    pub reported: Option<TerminalSize>,
}

fn same_client(a: &ClientId, b: &ClientId) -> bool {
    // The server stamps a web client's hostname and username with the
    // token it came in on, so only the parts we chose identify us.
    a.pid == b.pid && a.epoch == b.epoch && a.id == b.id
}

impl Lease {
    pub fn owns_viewport(&self) -> bool {
        let Some(me) = &self.me else {
            return false;
        };
        match self.mode {
            Some(codec::FrontendAccessMode::TmuxLatest) => {
                self.tab_owner.as_ref().is_some_and(|o| same_client(o, me))
            }
            Some(codec::FrontendAccessMode::Handoff) => {
                self.owner.as_ref().is_some_and(|o| same_client(o, me))
            }
            None => false,
        }
    }

    pub fn apply_access(&mut self, access: &codec::FrontendAccessState) {
        self.mode = Some(access.mode);
        self.owner = access.owner.clone();
    }

    pub fn apply_viewport(&mut self, state: &codec::ClientViewportState) {
        if Some(state.tab_id) != self.tab_id {
            return;
        }
        self.tab_owner = state.owner.clone();
        self.canonical_size = Some(state.canonical_size);
        self.apply_access(&state.access);
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: usize) -> ClientId {
        ClientId {
            hostname: "h".into(),
            username: "u".into(),
            pid: 0,
            epoch: 1,
            id: n,
            ssh_auth_sock: None,
        }
    }

    #[test]
    fn ownership_follows_the_mode() {
        let mut lease = Lease {
            me: Some(id(1)),
            tab_id: Some(7),
            ..Lease::default()
        };
        assert!(!lease.owns_viewport(), "unknown mode owns nothing");
        lease.apply_access(&codec::FrontendAccessState {
            mode: codec::FrontendAccessMode::Handoff,
            owner: Some(id(1)),
            generation: 1,
        });
        assert!(lease.owns_viewport());
        // The server stamps hostname and username on web clients; the rest
        // still identifies us.
        let mut stamped = id(1);
        stamped.hostname = "web:laptop".into();
        stamped.username = "server-user".into();
        lease.owner = Some(stamped);
        assert!(lease.owns_viewport());
        lease.owner = Some(id(2));
        assert!(!lease.owns_viewport());
    }

    #[test]
    fn viewport_state_for_another_tab_is_ignored() {
        let mut lease = Lease {
            me: Some(id(1)),
            tab_id: Some(7),
            ..Lease::default()
        };
        let access = codec::FrontendAccessState {
            mode: codec::FrontendAccessMode::TmuxLatest,
            owner: None,
            generation: 1,
        };
        let state = |tab_id, owner| codec::ClientViewportState {
            tab_id,
            owner,
            canonical_size: TerminalSize::default(),
            view: None,
            generation: 1,
            access: access.clone(),
        };
        lease.apply_viewport(&state(8, Some(id(1))));
        assert!(!lease.owns_viewport(), "another tab's owner is not ours");
        lease.apply_viewport(&state(7, Some(id(1))));
        assert!(lease.owns_viewport());
    }
}
