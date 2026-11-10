//! The terminals of other machines a window is connected to, as the server
//! a window runs for the command line hands them to the programs that ask
//! (`thinkterm cli`, a plugin): only as far as the window's Settings let
//! them. A program here asking after them is not the person at the window,
//! and what they reach is a shell on another machine, logged in as that
//! person. A server without a window to say otherwise has none to keep.

use codec::{PaneMachine, Pdu};
use mux::domain::DomainId;
use mux::pane::PaneId;
use mux::tab::TabId;
use mux::{Mux, MuxNotification};
use std::sync::OnceLock;
use thinkterm_proto::SpawnTabDomain;

/// How far the programs on this machine may reach the terminals of the
/// machines a window is connected to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reach {
    /// Not at all: they are not listed, read or typed into.
    Off,
    /// Listed and read, not typed into.
    Read,
    /// Listed, read and typed into, as the person at the window would.
    Type,
}

/// What a window says of the terminals of other machines.
pub struct Policy {
    /// How far the programs here may reach them, asked as each asks.
    pub reach: fn() -> Reach,
    /// Whether a domain is a connection to another machine. Asked of every
    /// pane's output, so it must be quick.
    pub foreign: fn(DomainId) -> bool,
    /// What the window calls the machine such a domain connects to.
    pub machine: fn(DomainId) -> String,
}

static POLICY: OnceLock<Policy> = OnceLock::new();

/// Set by a window's process, once, for the server it runs for the
/// command line.
pub fn install(policy: Policy) {
    let _ = POLICY.set(policy);
}

/// What a request does to a pane: reads it, or acts on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Use {
    Read,
    Act,
}

/// What a request is about.
#[derive(Clone, Debug, PartialEq)]
enum Subject {
    Pane(PaneId),
    Tab(TabId),
    Domain(SpawnTabDomain, Option<PaneId>),
}

/// What `pdu` reads or acts on, and which: nothing for what is about no one
/// pane -- a list, a version, the window's own business.
fn subject(pdu: &Pdu) -> Vec<(Subject, Use)> {
    use Subject::{Domain, Pane, Tab};
    use Use::{Act, Read};
    match pdu {
        Pdu::GetPaneRenderChanges(req) => vec![(Pane(req.pane_id), Read)],
        Pdu::GetPaneRenderableDimensions(req) => vec![(Pane(req.pane_id), Read)],
        Pdu::GetLines(req) => vec![(Pane(req.pane_id), Read)],
        Pdu::SearchScrollbackRequest(req) => vec![(Pane(req.pane_id), Read)],
        Pdu::GetPaneRunState(req) => vec![(Pane(req.pane_id), Read)],
        Pdu::GetPaneDirection(req) => vec![(Pane(req.pane_id), Read)],
        Pdu::GetKittyImage(req) => vec![(Pane(req.pane_id), Read)],
        Pdu::ReadRemoteFile(req) => vec![(Pane(req.pane_id), Read)],
        Pdu::GetKittyFrameSelections(req) => vec![(Pane(req.pane_id), Read)],
        Pdu::GetImageCell(req) => vec![(Pane(req.pane_id), Read)],
        Pdu::WriteToPane(req) => vec![(Pane(req.pane_id), Act)],
        Pdu::SendKeyDown(req) => vec![(Pane(req.pane_id), Act)],
        Pdu::SendMouseEvent(req) => vec![(Pane(req.pane_id), Act)],
        Pdu::SendPaste(req) => vec![(Pane(req.pane_id), Act)],
        Pdu::KillPane(req) => vec![(Pane(req.pane_id), Act)],
        Pdu::EraseScrollbackRequest(req) => vec![(Pane(req.pane_id), Act)],
        Pdu::MarkProgramStatusSeen(req) => vec![(Pane(req.pane_id), Act)],
        Pdu::ActivatePaneDirection(req) => vec![(Pane(req.pane_id), Act)],
        Pdu::AdjustPaneSize(req) => vec![(Pane(req.pane_id), Act)],
        Pdu::SetFocusedPane(req) => vec![(Pane(req.pane_id), Act)],
        Pdu::PaneLabelChanged(req) => vec![(Pane(req.pane_id), Act)],
        Pdu::ActivatePaneInStack(req) => vec![(Pane(req.pane_id), Act)],
        Pdu::MovePaneToNewTab(req) => vec![(Pane(req.pane_id), Act)],
        Pdu::SetPaneZoomed(req) => vec![(Pane(req.pane_id), Act)],
        Pdu::Resize(req) => vec![(Pane(req.pane_id), Act)],
        Pdu::SplitPane(req) => {
            let mut subjects = vec![
                (Pane(req.pane_id), Act),
                (Domain(req.domain.clone(), Some(req.pane_id)), Act),
            ];
            if let Some(pane_id) = req.move_pane_id {
                subjects.push((Pane(pane_id), Act));
            }
            subjects
        }
        Pdu::SpawnPaneInStack(req) => {
            vec![
                (Pane(req.pane_id), Act),
                (Domain(req.domain.clone(), Some(req.pane_id)), Act),
            ]
        }
        Pdu::MovePaneToStack(req) => vec![
            (Pane(req.source_pane_id), Act),
            (Pane(req.target_pane_id), Act),
        ],
        Pdu::SwapPanes(req) => vec![(Pane(req.pane_id), Act), (Pane(req.other_pane_id), Act)],
        Pdu::SpawnV2(req) => vec![(Domain(req.domain.clone(), None), Act)],
        Pdu::MoveTab(req) => vec![(Tab(req.tab_id), Act)],
        Pdu::TabTitleChanged(req) => vec![(Tab(req.tab_id), Act)],
        Pdu::SetClientViewport(req) => vec![(Tab(req.tab_id), Act)],
        Pdu::SetClientView(req) => vec![(Tab(req.tab_id), Act)],
        Pdu::ClaimClientViewport(req) => vec![(Tab(req.tab_id), Act)],
        _ => vec![],
    }
}

/// The domain of the other machine `subject` is on, when it is on one.
fn foreign_domain(policy: &Policy, subject: &Subject) -> Option<DomainId> {
    foreign_domain_in(&Mux::get(), policy, subject)
}

fn foreign_domain_in(mux: &Mux, policy: &Policy, subject: &Subject) -> Option<DomainId> {
    let domain = match subject {
        Subject::Pane(pane_id) => mux.get_pane(*pane_id)?.domain_id(),
        Subject::Tab(tab_id) => mux.get_tab(*tab_id)?.get_active_pane()?.domain_id(),
        Subject::Domain(domain, pane_id) => mux
            .resolve_spawn_tab_domain(*pane_id, domain)
            .ok()?
            .domain_id(),
    };
    (policy.foreign)(domain).then_some(domain)
}

/// Whether `reach` lets a program here `what` a terminal of another
/// machine; why not, if it does not.
fn allows(reach: Reach, what: Use, machine: &str) -> Result<(), String> {
    match (reach, what) {
        (Reach::Type, _) | (Reach::Read, Use::Read) => Ok(()),
        (Reach::Read, Use::Act) => Err(format!(
            "the terminals on {machine} can only be read from this machine: \
             let programs here type into them too in ThinkTerm's \
             Settings › Agents"
        )),
        (Reach::Off, _) => Err(format!(
            "the terminals on {machine} are not reachable from this machine: \
             let programs here reach them in ThinkTerm's Settings › Agents"
        )),
    }
}

/// Refuse what `pdu` asks of a terminal of another machine that the
/// window's Settings do not let programs here have.
pub fn gate(pdu: &Pdu) -> anyhow::Result<()> {
    let Some(policy) = POLICY.get() else {
        return Ok(());
    };
    gate_with_policy(&Mux::get(), policy, pdu)
}

fn gate_with_policy(mux: &Mux, policy: &Policy, pdu: &Pdu) -> anyhow::Result<()> {
    let subjects = subject(pdu);
    if subjects.is_empty() {
        return Ok(());
    }
    let reach = (policy.reach)();
    let types = matches!(
        pdu,
        Pdu::WriteToPane(_) | Pdu::SendPaste(_) | Pdu::SendKeyDown(_) | Pdu::SendMouseEvent(_)
    );
    for (subject, what) in subjects {
        if let Some(domain) = foreign_domain_in(mux, policy, &subject) {
            let machine = (policy.machine)(domain);
            allows(reach, what, &machine).map_err(anyhow::Error::msg)?;
            // What would be dropped without a word, typed into a terminal
            // the window may not type into just now: why, instead.
            if let (Subject::Pane(pane_id), true) = (&subject, types) {
                if let Some(why) = held_back(*pane_id, &machine) {
                    anyhow::bail!(why);
                }
            }
        }
    }
    Ok(())
}

/// Why what is sent to `pane_id`, on `machine`, would not reach it; `None`
/// when it would.
fn held_back(pane_id: PaneId, machine: &str) -> Option<String> {
    use wezterm_client::domain::RemoteFrontendGate;
    let pane = Mux::get().get_pane(pane_id)?;
    let remote = pane.downcast_ref::<wezterm_client::pane::ClientPane>()?;
    match remote.frontend_gate() {
        RemoteFrontendGate::Visible => None,
        RemoteFrontendGate::Claimable { owner: Some(_) } => Some(format!(
            "the terminal on {machine} is in use on another device just now: \
             take it back in the ThinkTerm window first"
        )),
        RemoteFrontendGate::Claimable { owner: None } => Some(format!(
            "the ThinkTerm window has not taken up the terminals on {machine} yet: \
             open one of them there first"
        )),
        RemoteFrontendGate::Connecting
        | RemoteFrontendGate::Reconnecting
        | RemoteFrontendGate::Syncing => Some(format!(
            "the ThinkTerm window is still connecting to {machine}: try again in a moment"
        )),
        RemoteFrontendGate::Offline => Some(format!(
            "the ThinkTerm window is not connected to {machine} just now"
        )),
    }
}

/// Whether `pane_id` is on another machine and not to be shown to the
/// programs here at all.
pub fn hidden(pane_id: PaneId) -> bool {
    let Some(policy) = POLICY.get() else {
        return false;
    };
    (policy.reach)() == Reach::Off && foreign_domain(policy, &Subject::Pane(pane_id)).is_some()
}

/// Remove hidden panes without losing local siblings in splits or stacks.
pub(crate) fn visible_tree(
    tree: thinkterm_proto::PaneNode,
    hidden: &impl Fn(PaneId) -> bool,
) -> thinkterm_proto::PaneNode {
    use thinkterm_proto::PaneNode;
    match tree {
        PaneNode::Leaf(entry) if hidden(entry.pane_id) => PaneNode::Empty,
        PaneNode::Stack(mut stack) => {
            let active = stack.panes.get(stack.active).map(|pane| pane.pane_id);
            stack.panes.retain(|pane| !hidden(pane.pane_id));
            stack.active = stack
                .panes
                .iter()
                .position(|pane| Some(pane.pane_id) == active)
                .unwrap_or(0);
            if stack.panes.is_empty() {
                PaneNode::Empty
            } else {
                PaneNode::Stack(stack)
            }
        }
        PaneNode::Split { left, right, node } => {
            let left = visible_tree(*left, hidden);
            let right = visible_tree(*right, hidden);
            match (left, right) {
                (PaneNode::Empty, right) => right,
                (left, PaneNode::Empty) => left,
                (left, right) => PaneNode::Split {
                    left: Box::new(left),
                    right: Box::new(right),
                    node,
                },
            }
        }
        tree => tree,
    }
}

/// Whether `notification` tells of another machine's terminal that the
/// programs here are not to hear of at all: what a program there runs, what
/// it copies, what it is called, as much as what it prints.
pub fn hides(notification: &MuxNotification) -> bool {
    let Some(policy) = POLICY.get() else {
        return false;
    };
    if (policy.reach)() != Reach::Off {
        return false;
    }
    let subject = match notification {
        MuxNotification::PaneOutput(pane_id)
        | MuxNotification::Alert { pane_id, .. }
        | MuxNotification::PaneAdded(pane_id)
        | MuxNotification::PaneExited { pane_id, .. }
        | MuxNotification::PaneRemoved(pane_id)
        | MuxNotification::AgentStatusChanged(pane_id)
        | MuxNotification::ForegroundProgramChanged(pane_id)
        | MuxNotification::AssignClipboard { pane_id, .. }
        | MuxNotification::PaneFocused(pane_id)
        | MuxNotification::PaneLabelChanged { pane_id, .. } => Subject::Pane(*pane_id),
        MuxNotification::TabAddedToWindow { tab_id, .. }
        | MuxNotification::TabResized(tab_id)
        | MuxNotification::TabTitleChanged { tab_id, .. } => Subject::Tab(*tab_id),
        _ => return false,
    };
    foreign_domain(policy, &subject).is_some()
}

/// Where `pane_id` is, if on another machine whose terminals the programs
/// here may see: the machine, and its id there -- the id the session server
/// there gives it, or over a plain SSH session, which has none, its id here.
pub fn machine(pane_id: PaneId) -> Option<PaneMachine> {
    let policy = POLICY.get()?;
    if (policy.reach)() == Reach::Off {
        return None;
    }
    on_its_machine(pane_id)
}

/// `machine`, whether or not the programs here may see the pane: how the
/// command line names it once they may, for the window to show.
pub fn on_its_machine(pane_id: PaneId) -> Option<PaneMachine> {
    let policy = POLICY.get()?;
    let domain = foreign_domain(policy, &Subject::Pane(pane_id))?;
    let pane = Mux::get().get_pane(pane_id)?;
    let there = pane
        .downcast_ref::<wezterm_client::pane::ClientPane>()
        .map_or(pane_id, |remote| remote.remote_pane_id);
    Some(PaneMachine {
        machine: (policy.machine)(domain),
        pane_id: there,
    })
}

/// Where the cursor of `pane_id` is and how far its rows run, as its own
/// machine has them, when it is another one's: a window keeps them only as
/// fresh as what it has shown. `None` for a pane of this machine's.
pub async fn dimensions_from_its_machine(
    pane_id: PaneId,
) -> Option<anyhow::Result<codec::GetPaneRenderableDimensionsResponse>> {
    let policy = POLICY.get()?;
    foreign_domain(policy, &Subject::Pane(pane_id))?;
    let pane = Mux::get().get_pane(pane_id)?;
    let remote = pane.downcast_ref::<wezterm_client::pane::ClientPane>()?;
    Some(remote.fetch_dimensions().await.map(|response| {
        codec::GetPaneRenderableDimensionsResponse {
            pane_id,
            ..response
        }
    }))
}

/// The rows of `pane_id` as its own machine has them, when it is another
/// one's: a window holds only the rows it has shown, and the rest come back
/// blank until fetched. `None` for a pane of this machine's.
pub async fn lines_from_its_machine(
    pane_id: PaneId,
    lines: Vec<std::ops::Range<wezterm_term::StableRowIndex>>,
) -> Option<anyhow::Result<codec::SerializedLines>> {
    let policy = POLICY.get()?;
    foreign_domain(policy, &Subject::Pane(pane_id))?;
    let pane = Mux::get().get_pane(pane_id)?;
    let remote = pane.downcast_ref::<wezterm_client::pane::ClientPane>()?;
    Some(remote.fetch_lines(lines).await)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_policy(reach: fn() -> Reach) -> Policy {
        Policy {
            reach,
            foreign: |_| true,
            machine: |_| "devbox".to_string(),
        }
    }

    #[test]
    fn implicit_remote_domains_obey_the_same_policy_as_explicit_ones() {
        use mux::domain::{Domain, LocalDomain};
        use std::sync::Arc;
        let mux = Mux::new(None);
        let domain: Arc<dyn Domain> = Arc::new(LocalDomain::new("devbox").unwrap());
        mux.add_domain(&domain);
        mux.set_default_domain(&domain);
        for domain in [
            SpawnTabDomain::DefaultDomain,
            SpawnTabDomain::CurrentPaneDomain,
            SpawnTabDomain::DomainId(domain.domain_id()),
            SpawnTabDomain::DomainName("devbox".to_string()),
        ] {
            let spawn = Pdu::SpawnV2(codec::SpawnV2 {
                domain,
                window_id: None,
                command: None,
                command_dir: None,
                size: wezterm_term::TerminalSize::default(),
                workspace: "default".to_string(),
            });
            assert!(gate_with_policy(&mux, &test_policy(|| Reach::Off), &spawn).is_err());
            assert!(gate_with_policy(&mux, &test_policy(|| Reach::Read), &spawn).is_err());
            assert!(gate_with_policy(&mux, &test_policy(|| Reach::Type), &spawn).is_ok());
        }
    }

    #[test]
    fn a_split_move_checks_the_source_as_a_mutation() {
        let split = Pdu::SplitPane(codec::SplitPane {
            pane_id: 1,
            split_request: Default::default(),
            command: None,
            command_dir: None,
            domain: SpawnTabDomain::CurrentPaneDomain,
            move_pane_id: Some(2),
        });
        assert!(subject(&split).contains(&(Subject::Pane(2), Use::Act)));
    }

    fn entry(pane_id: PaneId) -> thinkterm_proto::PaneEntry {
        thinkterm_proto::PaneEntry {
            window_id: 1,
            tab_id: 1,
            pane_id,
            title: "shell".to_string(),
            size: wezterm_term::TerminalSize::default(),
            working_dir: None,
            is_active_pane: false,
            is_zoomed_pane: false,
            alt_screen: false,
            workspace: "default".to_string(),
            cursor_pos: Default::default(),
            physical_top: 0,
            top_row: 0,
            left_col: 0,
            tty_name: None,
        }
    }

    #[test]
    fn filtering_remote_leaves_keeps_local_splits_and_stack_members() {
        use thinkterm_proto::{PaneNode, PaneStackEntry, SplitDirection, SplitDirectionAndSize};
        let tree = PaneNode::Split {
            left: Box::new(PaneNode::Leaf(entry(1))),
            right: Box::new(PaneNode::Stack(PaneStackEntry {
                active: 2,
                panes: vec![entry(2), entry(3), entry(4)],
                pane_stack_id: Some(7),
            })),
            node: SplitDirectionAndSize {
                direction: SplitDirection::Horizontal,
                first: Default::default(),
                second: Default::default(),
            },
        };
        let tree = visible_tree(tree, &|id| id == 1 || id == 2);
        assert_eq!(
            tree.entries().iter().map(|p| p.pane_id).collect::<Vec<_>>(),
            vec![3, 4]
        );
        assert!(
            matches!(&tree, PaneNode::Stack(stack) if stack.active == 1 && stack.pane_stack_id == Some(7))
        );
        let tree = visible_tree(tree, &|id| id == 4);
        assert!(matches!(&tree, PaneNode::Stack(stack) if stack.active == 0));
        assert!(matches!(visible_tree(tree, &|_| true), PaneNode::Empty));
        assert_eq!(
            visible_tree(PaneNode::Leaf(entry(5)), &|_| false),
            PaneNode::Leaf(entry(5))
        );
    }

    #[test]
    fn reading_is_one_thing_and_typing_another() {
        assert!(allows(Reach::Read, Use::Read, "devbox").is_ok());
        assert!(allows(Reach::Read, Use::Act, "devbox").is_err());
        assert!(allows(Reach::Type, Use::Act, "devbox").is_ok());
        assert!(allows(Reach::Off, Use::Read, "devbox").is_err());
        let why = allows(Reach::Off, Use::Read, "devbox").unwrap_err();
        assert!(why.contains("devbox") && why.contains("Settings"), "{why}");
    }

    #[test]
    fn what_reads_and_what_acts() {
        let reads = Pdu::GetLines(codec::GetLines {
            pane_id: 4,
            lines: vec![],
        });
        assert_eq!(subject(&reads), vec![(Subject::Pane(4), Use::Read)]);
        let types = Pdu::WriteToPane(codec::WriteToPane {
            pane_id: 4,
            data: b"ls\r".to_vec(),
        });
        assert_eq!(subject(&types), vec![(Subject::Pane(4), Use::Act)]);
        let lists = Pdu::ListPanes(codec::ListPanes {});
        assert!(subject(&lists).is_empty());
    }

    #[test]
    fn with_no_window_nothing_is_kept_from_anyone() {
        // No policy installed in a test process.
        let types = Pdu::WriteToPane(codec::WriteToPane {
            pane_id: 4,
            data: b"ls\r".to_vec(),
        });
        assert!(gate(&types).is_ok());
        assert!(!hidden(4));
        assert!(machine(4).is_none());
    }
}
