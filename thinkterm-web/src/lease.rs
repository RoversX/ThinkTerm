//! Who owns the tab this browser mirrors, as the server has told us. Pure,
//! so it is tested natively; the WebSocket link keeps one and feeds it.

use std::collections::HashMap;
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
    /// The viewport this page last told the server about. The server
    /// claims a client's last report for it when it types, so the report
    /// must be kept current or a keystroke would reshape the tab to a
    /// shape the desktop has since left.
    pub reported_viewport: Option<codec::ClientViewport>,
    /// The drawn layout as the desktop would claim it (`TabLayout::viewport`);
    /// empty only before the tab is first listed. Kept across a canonical
    /// size push: the listing that follows it replaces the panes, and a
    /// stale pane-by-pane claim is refused and retried as the grid,
    /// whereas a bare grid claim would resize every pane to its frame.
    pub native: Vec<codec::ClientPaneViewport>,
    /// The tab size `native`'s frames compose to: the listing's. A claim
    /// at any other size goes as a bare grid, since frames that do not
    /// compose to the root are refused.
    pub native_root: Option<TerminalSize>,
    /// The page's own grid is what it claims: set when an interaction
    /// here takes the terminal, as the desktop's own window shape wins
    /// when it takes it. Cleared when a push shows the tab in someone
    /// else's hands, and the page follows the desktop's size again.
    pub fit: bool,
    /// Every tab's own size the server has told us, whichever tab it
    /// was for. A lone pane's listing does not say how big its tab is
    /// (the pane is smaller by its bar), and a report with the wrong
    /// root would resize the tab to it.
    pub tab_sizes: HashMap<TabId, TerminalSize>,
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
        self.tab_sizes.insert(state.tab_id, state.canonical_size);
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

    /// Whether a terminal-area interaction must claim before it acts. In
    /// Handoff the server refuses even a focus change from a renderer that
    /// does not hold the terminal -- also when nobody does, since a page
    /// that has reported a viewport is a screen, not a CLI. In TmuxLatest
    /// input claims on its own and focus changes are always accepted.
    pub fn needs_claim(&self) -> bool {
        matches!(self.mode, Some(codec::FrontendAccessMode::Handoff)) && !self.owns_viewport()
    }

    /// The panes as the desktop would claim them, from a listing.
    pub fn set_native(&mut self, layout: &crate::layout::TabLayout, nav_rows: usize) {
        self.native = layout.viewport(nav_rows);
        self.native_root = Some(layout.size);
    }

    /// The viewport to claim the tab with: the page's own grid when an
    /// interaction here holds the terminal (`fit`), else the desktop's
    /// canonical size, so a follower's report changes nothing. Pane by
    /// pane when the listed frames compose to that size -- then every
    /// pane keeps the rows above it for its bar -- and as a bare grid
    /// otherwise (the size just changed; the next listing catches up).
    pub fn claim_viewport(&self) -> Option<codec::ClientViewport> {
        let size = if self.fit || (self.ownerless() && self.canonical_size.is_none()) {
            self.reported.or(self.canonical_size)?
        } else {
            self.canonical_size.or(self.reported)?
        };
        let composes = !self.native.is_empty()
            && self.native_root.is_some_and(|r| r.cols == size.cols && r.rows == size.rows);
        if composes {
            Some(codec::ClientViewport::Native { size, panes: self.native.clone() })
        } else {
            Some(codec::ClientViewport::CellGrid { size })
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
        assert!(lease.tab_sizes.contains_key(&8), "but its size is remembered");
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
            reported_viewport: None,
            native: vec![],
            native_root: None,
            fit: false,
            tab_sizes: HashMap::new(),
        }
    }
    fn grid(l: &Lease) -> Option<TerminalSize> {
        match l.claim_viewport()? {
            codec::ClientViewport::CellGrid { size } => Some(size),
            codec::ClientViewport::Native { .. } => None,
        }
    }
    fn pane(pane_id: usize, cols: usize, rows: usize) -> codec::ClientPaneViewport {
        codec::ClientPaneViewport { pane_id, size: size(cols, rows - 3), frame: size(cols, rows) }
    }

    #[test]
    fn a_tab_someone_else_holds_is_claimed_at_its_own_size() {
        let mut l = lease(codec::FrontendAccessMode::TmuxLatest);
        l.tab_owner = Some(id(2));
        assert_eq!(grid(&l), Some(size(120, 40)), "the desktop's grid, verbatim");
        assert!(l.may_type(), "TmuxLatest: typing hands the tab over, at that size");
    }

    #[test]
    fn only_a_fit_or_an_unsized_ownerless_tab_takes_the_page_s_own_grid() {
        let mut l = lease(codec::FrontendAccessMode::TmuxLatest);
        assert_eq!(grid(&l), Some(size(120, 40)), "ownerless but sized: the tab's own");
        l.canonical_size = None;
        assert_eq!(grid(&l), Some(size(96, 30)), "nothing known: the page's");
        l.canonical_size = Some(size(120, 40));
        l.tab_owner = Some(id(2));
        l.fit = true;
        assert_eq!(grid(&l), Some(size(96, 30)));
    }

    #[test]
    fn a_tab_this_page_holds_keeps_the_size_it_was_claimed_at() {
        let mut l = lease(codec::FrontendAccessMode::TmuxLatest);
        l.tab_owner = Some(id(1));
        assert_eq!(grid(&l), Some(size(120, 40)));
    }

    #[test]
    fn a_listed_tab_is_claimed_pane_by_pane_until_its_size_moves() {
        let mut l = lease(codec::FrontendAccessMode::TmuxLatest);
        l.tab_owner = Some(id(2));
        l.native = vec![pane(1, 59, 40), pane(2, 60, 40)];
        l.native_root = Some(size(120, 40));
        match l.claim_viewport() {
            Some(codec::ClientViewport::Native { size: root, panes }) => {
                assert_eq!(root, size(120, 40));
                assert_eq!(panes.len(), 2);
                assert_eq!(panes[0].size, size(59, 37), "the pane's grid, not its frame");
            }
            other => panic!("expected a native claim, got {other:?}"),
        }
        l.fit = true;
        assert_eq!(grid(&l), Some(size(96, 30)), "a fit is always the page's grid");
        l.fit = false;
        let state = codec::ClientViewportState {
            tab_id: 1,
            owner: Some(id(2)),
            canonical_size: size(100, 50),
            view: None,
            generation: 3,
            access: codec::FrontendAccessState { mode: codec::FrontendAccessMode::TmuxLatest, owner: None, generation: 3 },
        };
        l.apply_viewport(&state);
        assert_eq!(l.native.len(), 2, "the panes wait for the next listing");
        assert_eq!(
            l.claim_viewport(),
            Some(codec::ClientViewport::CellGrid { size: size(100, 50) }),
            "frames of the old size would not compose: the bare grid until the next listing"
        );
    }

    #[test]
    fn handoff_lets_only_the_owner_type_and_a_push_ends_a_fit() {
        let mut l = lease(codec::FrontendAccessMode::Handoff);
        assert!(l.may_type(), "nobody holds the connection");
        l.owner = Some(id(2));
        assert!(!l.may_type());
        assert_eq!(grid(&l), Some(size(120, 40)));
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
