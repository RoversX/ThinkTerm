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
}

impl AgentStatus {
    /// Metadata budgets do not change identity bytes or the wire layout.
    pub fn within_budget(&self) -> bool {
        self.agent_id.len() <= wezterm_term::agent_contract::MAX_AGENT_ID_BYTES
            && self
                .session_id
                .as_ref()
                .is_none_or(|id| id.len() <= wezterm_term::agent_contract::MAX_AGENT_SESSION_BYTES)
    }
}
