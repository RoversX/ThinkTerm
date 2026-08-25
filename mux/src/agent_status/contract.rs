//! The `THINKTERM_AGENT` user-var contract.
//!
//! Agents (or hook scripts acting for them) report identity and state by
//! emitting `OSC 1337 ; SetUserVar=THINKTERM_AGENT=<base64 value> ST` into
//! their own pane. The terminal layer stores the decoded value in the
//! pane's user vars (mirrored across mux domains), and this module parses
//! it.
//!
//! Value grammar, version 1:
//!
//! ```text
//! v1;agent=claude;state=working;session=<id>;pid=<n>;ts=<unix-seconds>[;ended=1]
//! ```
//!
//! One variable rather than several so state and session can never be
//! observed torn. Unknown keys are ignored; an unknown *state* parses as
//! `Unknown`; an unrecognized major version invalidates the whole value.

use thinkterm_proto::AgentState;

pub const USER_VAR_NAME: &str = "THINKTERM_AGENT";

/// How old a non-idle contract may be before it is disregarded. Long on
/// purpose: it only exists to shed reports from long-dead sessions on
/// mux-server panes that survive GUI restarts.
pub(crate) const CONTRACT_MAX_AGE_SECS: u64 = 6 * 60 * 60;
/// Clock skew tolerated before a future timestamp stops counting as fresh.
const FUTURE_SKEW_ALLOWANCE_SECS: u64 = 300;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentContract {
    pub agent_id: String,
    pub state: AgentState,
    pub session_id: Option<String>,
    /// Unix seconds at emission time, from the reporter's clock.
    pub ts: Option<u64>,
    /// The session announced its own end. Reserved for native emitters;
    /// today's detection evicts an ended session's pane instead of
    /// publishing it, so a surviving `ended=1` only reaches consumers
    /// while the emitting process is still alive (where it is ignored).
    pub ended: bool,
}

pub fn parse(value: &str) -> Option<AgentContract> {
    let mut fields = value.split(';');
    let version = fields.next()?.trim();
    // "v1" or a future minor like "v1.1" stays parseable; any other major
    // means a new incompatible grammar and the value is ignored.
    if version != "v1" && !version.starts_with("v1.") {
        return None;
    }

    let mut agent_id = None;
    let mut state = None;
    let mut session_id = None;
    let mut ts = None;
    let mut ended = false;
    for field in fields {
        let Some((key, val)) = field.split_once('=') else {
            continue;
        };
        match key.trim() {
            "agent" => agent_id = Some(val.trim().to_string()),
            "state" => {
                state = Some(match val.trim() {
                    "working" => AgentState::Working,
                    "idle" => AgentState::Idle,
                    "blocked" => AgentState::Blocked,
                    _ => AgentState::Unknown,
                })
            }
            "session" => {
                let val = val.trim();
                if !val.is_empty() {
                    session_id = Some(val.to_string());
                }
            }
            "ts" => ts = val.trim().parse::<u64>().ok(),
            "ended" => ended = val.trim() == "1",
            // pid= and unknown future keys are diagnostic only.
            _ => {}
        }
    }

    let agent_id = agent_id.filter(|id| !id.is_empty())?;
    let state = state?;
    Some(AgentContract {
        agent_id,
        state,
        session_id,
        ts,
        ended,
    })
}

/// A non-idle report older than `CONTRACT_MAX_AGE_SECS` no longer counts as
/// a state authority (identity is still honored by the caller).
pub fn state_is_fresh(contract: &AgentContract, now_unix: u64) -> bool {
    if contract.state == AgentState::Idle {
        return true;
    }
    match contract.ts {
        // A timestamp from the future (broken clock, crafted var) must
        // not be eternally fresh; allow modest skew only.
        Some(ts) if ts > now_unix + FUTURE_SKEW_ALLOWANCE_SECS => false,
        Some(ts) => now_unix.saturating_sub(ts) <= CONTRACT_MAX_AGE_SECS,
        // No timestamp: trust it; pane-death and screen contradiction are
        // the remaining guards.
        None => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_value_parses() {
        let parsed =
            parse("v1;agent=claude;state=working;session=abc-123;pid=42;ts=1700000000").unwrap();
        assert_eq!(parsed.agent_id, "claude");
        assert_eq!(parsed.state, AgentState::Working);
        assert_eq!(parsed.session_id.as_deref(), Some("abc-123"));
        assert_eq!(parsed.ts, Some(1_700_000_000));
        assert!(!parsed.ended);
    }

    #[test]
    fn empty_session_and_unknown_keys_are_tolerated() {
        let parsed = parse("v1;agent=soul;state=idle;session=;future_key=zzz;ended=1").unwrap();
        assert_eq!(parsed.agent_id, "soul");
        assert_eq!(parsed.state, AgentState::Idle);
        assert_eq!(parsed.session_id, None);
        assert!(parsed.ended);
    }

    #[test]
    fn unknown_state_is_unknown_but_valid() {
        let parsed = parse("v1;agent=claude;state=thinking").unwrap();
        assert_eq!(parsed.state, AgentState::Unknown);
    }

    #[test]
    fn wrong_major_version_or_missing_fields_reject() {
        assert_eq!(parse("v2;agent=claude;state=working"), None);
        assert_eq!(parse("v1;state=working"), None);
        assert_eq!(parse("v1;agent=claude"), None);
        assert_eq!(parse("garbage"), None);
        assert_eq!(parse(""), None);
    }

    #[test]
    fn freshness_gate() {
        let now = 2_000_000_000u64;
        let stale = parse("v1;agent=claude;state=working;ts=1700000000").unwrap();
        assert!(!state_is_fresh(&stale, now));
        let fresh = parse(&format!("v1;agent=claude;state=working;ts={}", now - 60)).unwrap();
        assert!(state_is_fresh(&fresh, now));
        // Idle never goes stale; no ts means trusted.
        let idle_stale = parse("v1;agent=claude;state=idle;ts=1700000000").unwrap();
        assert!(state_is_fresh(&idle_stale, now));
        let no_ts = parse("v1;agent=claude;state=working").unwrap();
        assert!(state_is_fresh(&no_ts, now));

        // A future timestamp beyond skew allowance must not be eternally
        // fresh: a broken clock or crafted var would otherwise pin the
        // reported state forever.
        let mut future = fresh.clone();
        future.ts = Some(now + 10_000);
        assert!(!state_is_fresh(&future, now));
        let mut skewed = fresh.clone();
        skewed.ts = Some(now + 60);
        assert!(state_is_fresh(&skewed, now));
    }
}
