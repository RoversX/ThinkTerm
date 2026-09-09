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
    /// The grid this page could show: its canvas in cells. Never sent as
    /// a viewport unless `fit` is on; see `claim_size`.
    pub reported: Option<TerminalSize>,
    /// The canonical size this page last told the server about. The
    /// server claims a client's last report for it when it types, so
    /// the report must be kept current or a keystroke would reshape the
    /// tab to a size the desktop has since left.
    pub reported_canonical: Option<TerminalSize>,
    /// The page wants the tab at its own grid rather than the desktop's.
    /// Cleared when a push shows the tab in someone else's hands.
    pub fit: bool,
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
        if self.fit && !self.owns_viewport() {
            self.fit = false;
        }
    }

    /// Whether the tab (TmuxLatest) or the connection (Handoff) has no
    /// owner at all.
    pub fn ownerless(&self) -> bool {
        match self.mode {
            Some(codec::FrontendAccessMode::TmuxLatest) => self.tab_owner.is_none(),
            Some(codec::FrontendAccessMode::Handoff) => self.owner.is_none(),
            None => true,
        }
    }

    /// Whether this page's input will be accepted. In TmuxLatest anyone
    /// may type (the server hands the tab to whoever does); in Handoff
    /// only the owner, or anyone when there is none.
    pub fn may_type(&self) -> bool {
        match self.mode {
            Some(codec::FrontendAccessMode::Handoff) => self.owns_viewport() || self.owner.is_none(),
            _ => true,
        }
    }

    /// The grid to claim the tab with. The desktop's own (canonical) size
    /// unless this page asked to fit the tab to itself or nobody holds
    /// the tab yet -- so typing here does not reshape what the desktop
    /// shows. Echoed verbatim: the server's resize is a no-op only when
    /// every field matches.
    pub fn claim_size(&self) -> Option<TerminalSize> {
        if self.fit || self.ownerless() {
            self.reported.or(self.canonical_size)
        } else {
            self.canonical_size.or(self.reported)
        }
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

#[cfg(test)]
mod claim_tests {
    use super::*;

    fn id(n: usize) -> ClientId {
        ClientId {
            hostname: "h".into(),
            username: "u".into(),
            pid: 1,
            epoch: 1,
            id: n,
            ssh_auth_sock: None,
        }
    }
    fn size(cols: usize, rows: usize) -> TerminalSize {
        TerminalSize { cols, rows, pixel_width: cols * 7, pixel_height: rows * 15, dpi: 96 }
    }
    fn lease(mode: codec::FrontendAccessMode) -> Lease {
        Lease {
            tab_id: Some(1),
            me: Some(id(1)),
            mode: Some(mode),
            owner: None,
            tab_owner: None,
            canonical_size: Some(size(120, 40)),
            reported: Some(size(96, 30)),
            reported_canonical: None,
            fit: false,
        }
    }

    #[test]
    fn a_tab_someone_else_holds_is_claimed_at_its_own_size() {
        let mut l = lease(codec::FrontendAccessMode::TmuxLatest);
        l.tab_owner = Some(id(2));
        assert_eq!(l.claim_size(), Some(size(120, 40)), "the desktop's grid, verbatim");
        assert!(l.may_type(), "TmuxLatest: typing hands the tab over, at that size");
    }

    #[test]
    fn an_ownerless_tab_and_a_fit_take_the_page_s_own_grid() {
        let mut l = lease(codec::FrontendAccessMode::TmuxLatest);
        assert_eq!(l.claim_size(), Some(size(96, 30)));
        l.tab_owner = Some(id(2));
        l.fit = true;
        assert_eq!(l.claim_size(), Some(size(96, 30)));
    }

    #[test]
    fn a_tab_this_page_holds_keeps_the_size_it_was_claimed_at() {
        let mut l = lease(codec::FrontendAccessMode::TmuxLatest);
        l.tab_owner = Some(id(1));
        assert_eq!(l.claim_size(), Some(size(120, 40)));
    }

    #[test]
    fn handoff_lets_only_the_owner_type_and_a_push_ends_a_fit() {
        let mut l = lease(codec::FrontendAccessMode::Handoff);
        assert!(l.may_type(), "nobody holds the connection");
        l.owner = Some(id(2));
        assert!(!l.may_type());
        assert_eq!(l.claim_size(), Some(size(120, 40)));
        l.owner = Some(id(1));
        assert!(l.may_type());
        l.fit = true;
        let state = codec::ClientViewportState {
            tab_id: 1,
            owner: Some(id(2)),
            canonical_size: size(100, 50),
            view: None,
            generation: 3,
            access: codec::FrontendAccessState { mode: codec::FrontendAccessMode::Handoff, owner: Some(id(2)), generation: 3 },
        };
        l.apply_viewport(&state);
        assert!(!l.fit, "the tab is in someone else's hands");
        assert_eq!(l.canonical_size, Some(size(100, 50)));
    }
}
