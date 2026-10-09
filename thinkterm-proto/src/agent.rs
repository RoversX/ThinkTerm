//! Agent status wire types.
//!
//! A pane running a coding agent (Claude Code, Codex, …) is classified by
//! the mux that *owns* the pane; the result travels to clients over the
//! mux protocol and mirrors onto their ClientPanes.
//!
//! Varbincode is positional: **field and enum-variant order is the wire
//! contract**. Bump the codec version when changing anything here.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub enum AgentState {
    Working,
    Blocked,
    Idle,
    Unknown,
    /// Stopped on a failure it reported itself (OSC 7501 `error`).
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub enum AgentEvidence {
    /// The agent's own report via the THINKTERM_AGENT user-var contract.
    Contract,
    /// A screen-manifest rule matched the pane's live screen or OSC state.
    Screen,
    /// No rule matched and no fresh contract: the known-agent Idle
    /// fallback (never produces Blocked).
    Fallback,
    /// The program's own OSC 7501 report, which outranks everything else.
    Report,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct AgentStatus {
    /// Manifest/contract agent id, e.g. "claude".
    pub agent_id: String,
    pub state: AgentState,
    pub evidence: AgentEvidence,
    /// The agent's native session id, when its integration reports one.
    pub session_id: Option<String>,
    /// Unix seconds when `state` last changed, on the detecting host's
    /// clock. Cross-host skew makes derived ages approximate; render a
    /// negative age as zero.
    pub since_unix: u64,
    /// The session reported its own end. Reserved on the wire for native
    /// emitters: the current detection pipeline evicts ended sessions
    /// instead of publishing them, so consumers treat a `true` here
    /// defensively (hide from lists, fall back to native signals) but
    /// should not rely on ever observing it.
    pub ended: bool,
    /// What the program said about itself with OSC 7501, when it did.
    pub report: Option<ProgramReport>,
}

/// A program's own account of itself (OSC 7501), as the owning mux read it
/// from the pane: its root record and the records filed under it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ProgramReport {
    pub state: ProgramReportState,
    /// Only while `Blocked`.
    pub kind: Option<ProgramBlockedKind>,
    /// 0 to 100; `None` is indeterminate.
    pub progress: Option<u8>,
    /// Machine-readable program name, such as `claude-code`.
    pub app: Option<String>,
    pub title: Option<String>,
    /// What it is doing, what it finished, or why it stopped.
    pub msg: Option<String>,
    /// Sub-tasks, in id order; at most [`ProgramReport::MAX_CHILDREN`].
    pub children: Vec<ProgramReportChild>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ProgramReportChild {
    pub id: String,
    pub state: ProgramReportState,
    pub kind: Option<ProgramBlockedKind>,
    pub progress: Option<u8>,
    pub title: Option<String>,
    pub msg: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub enum ProgramReportState {
    Idle,
    Working,
    /// Finished, and nobody has looked at the result yet.
    Done,
    Blocked,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub enum ProgramBlockedKind {
    Permission,
    Question,
    Auth,
}

impl ProgramReport {
    /// The protocol lets a terminal keep hundreds of records; an agent's
    /// panel has room for this many, and every status change carries them.
    pub const MAX_CHILDREN: usize = 32;
    /// The protocol's own ceilings on decoded text and names.
    pub const MAX_TITLE_BYTES: usize = 192;
    pub const MAX_MSG_BYTES: usize = 2048;
    pub const MAX_NAME_BYTES: usize = 32;
    pub const MAX_ID_BYTES: usize = 128;

    /// Whether it holds a finished or failed result nobody has looked at
    /// yet -- its own or a sub-task's. What decides that a look at the pane
    /// must be passed on to the mux that keeps the records.
    pub fn has_unseen_result(&self) -> bool {
        let unseen = |state| matches!(state, ProgramReportState::Done | ProgramReportState::Error);
        unseen(self.state) || self.children.iter().any(|child| unseen(child.state))
    }

    pub fn within_budget(&self) -> bool {
        let text_fits = |title: &Option<String>, msg: &Option<String>| {
            title.as_ref().is_none_or(|t| t.len() <= Self::MAX_TITLE_BYTES)
                && msg.as_ref().is_none_or(|m| m.len() <= Self::MAX_MSG_BYTES)
        };
        text_fits(&self.title, &self.msg)
            && self.app.as_ref().is_none_or(|app| app.len() <= Self::MAX_NAME_BYTES)
            && self.children.len() <= Self::MAX_CHILDREN
            && self.children.iter().all(|child| {
                child.id.len() <= Self::MAX_ID_BYTES && text_fits(&child.title, &child.msg)
            })
    }
}

impl AgentStatus {
    /// Metadata budgets do not change identity bytes or the wire layout.
    pub fn within_budget(&self) -> bool {
        self.agent_id.len() <= wezterm_term::agent_contract::MAX_AGENT_ID_BYTES
            && self
                .session_id
                .as_ref()
                .is_none_or(|id| id.len() <= wezterm_term::agent_contract::MAX_AGENT_SESSION_BYTES)
            && self.report.as_ref().is_none_or(ProgramReport::within_budget)
    }
}
