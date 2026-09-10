//! The right-hand Agents panel as data: one row per pane an agent runs
//! in, as the desktop's panel lists them (`agent_panel.rs`), and the
//! one-line summary above them.

use crate::tree::TreeModel;
use serde::Serialize;
use thinkterm_i18n::{tr, tr_args, FluentArgs};
use thinkterm_proto::{AgentState, PaneId, WindowId};

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AgentRow {
    pub pane: PaneId,
    pub window: Option<WindowId>,
    /// The pane is in the window this page shows.
    pub here: bool,
    pub agent_id: String,
    pub name: String,
    pub title: String,
    pub state: &'static str,
    pub state_label: String,
    /// "Project · Thread", or the workspace when no thread claims it.
    pub place: String,
    /// The brand or fallback icon the page draws.
    pub icon: &'static str,
}

/// One tab of the right panel's selector, as the desktop lists them
/// (`RightSidebarMode::ALL`: Files, Notes, Code, Agents). Which exist and
/// which the browser can open is decided here, not in the display layer.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PanelTab {
    pub id: &'static str,
    /// A lucide name, as the menus name theirs.
    pub icon: &'static str,
    pub label: String,
    /// Off for the panels whose APIs the browser has not got; the tip
    /// says so, and a press does nothing.
    pub available: bool,
    pub tip: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AgentsView {
    pub rows: Vec<AgentRow>,
    pub summary: String,
    /// The selector above the panel, and which of its tabs is showing.
    pub tabs: Vec<PanelTab>,
    pub active: &'static str,
}

/// The panel's sections, in the desktop's order, but only those the page
/// can open: a button for a feature the browser lacks is noise. Files,
/// Notes and Code join here when they get a wire.
pub fn panel_tabs() -> Vec<PanelTab> {
    [("agents", "bot", "right-mode-agents")]
        .into_iter()
        .map(|(id, icon, key)| {
            let label = tr(key);
            PanelTab { id, icon, label: label.clone(), available: true, tip: label }
        })
        .collect()
}

#[cfg(test)]
mod panel_tests {
    #[test]
    fn only_agents_is_available_in_a_browser() {
        let tabs = super::panel_tabs();
        assert!(tabs.iter().map(|t| t.id).eq(["agents"]));
        assert!(tabs.iter().all(|t| t.available));
    }
}

/// The curated names the desktop uses; anything else is its id, title-cased.
pub fn display_name(agent_id: &str) -> String {
    match agent_id {
        "claude" => "Claude Code".into(),
        "codex" => "Codex".into(),
        "copilot" => "Copilot CLI".into(),
        "cursor" => "Cursor Agent".into(),
        "pi" => "Pi".into(),
        "opencode" => "OpenCode".into(),
        "kimi" => "Kimi Code".into(),
        "omp" => "OMP".into(),
        "soul" => "Soul".into(),
        other => {
            let mut chars = other.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => "Agent".into(),
            }
        }
    }
}

/// Brand icons the page has (simple-icons: claude, githubcopilot); `bot` otherwise.
pub fn icon(agent_id: &str) -> &'static str {
    match agent_id {
        "claude" => "brand-claude",
        "copilot" => "brand-copilot",
        _ => "bot",
    }
}

fn state_key(state: AgentState) -> (&'static str, &'static str) {
    match state {
        AgentState::Working => ("working", "right-agents-state-working"),
        AgentState::Blocked => ("blocked", "right-agents-state-blocked"),
        AgentState::Idle => ("idle", "right-agents-state-idle"),
        AgentState::Unknown => ("unknown", "right-agents-state-unknown"),
    }
}

/// The rows, sorted as the desktop sorts them: by place, agent, pane --
/// never floating the current window, so a switch does not reorder them.
pub fn rows(tree: &TreeModel, current_window: WindowId, title_of: impl Fn(PaneId) -> Option<String>) -> Vec<AgentRow> {
    let mut rows: Vec<AgentRow> = tree
        .agent_entries()
        .map(|(pane, agent_id, state, title)| {
            let (place, window) = tree.place_of_pane(pane);
            let (state_name, state_label) = state_key(state);
            AgentRow {
                pane,
                window,
                here: window == Some(current_window),
                name: display_name(agent_id),
                title: title_of(pane).unwrap_or_else(|| crate::navbar::display_title(title).0),
                state: state_name,
                state_label: tr(state_label),
                place,
                icon: icon(agent_id),
                agent_id: agent_id.to_string(),
            }
        })
        .collect();
    rows.sort_by(|a, b| {
        a.place
            .to_lowercase()
            .cmp(&b.place.to_lowercase())
            .then(a.agent_id.cmp(&b.agent_id))
            .then(a.pane.cmp(&b.pane))
    });
    rows
}

/// "2 working · 1 needs input", else idle and unknown counted apart,
/// else "No agents detected".
pub fn summary(rows: &[AgentRow]) -> String {
    let count = |s: &str| rows.iter().filter(|r| r.state == s).count();
    let (working, blocked, idle, unknown) = (count("working"), count("blocked"), count("idle"), count("unknown"));
    if rows.is_empty() {
        return tr("right-agents-none");
    }
    let part = |key: &str, n: usize| {
        let mut args = FluentArgs::new();
        args.set("count", n as i64);
        tr_args(key, &args)
    };
    let mut parts = Vec::new();
    if working > 0 {
        parts.push(part("right-agents-count-working", working));
    }
    if blocked > 0 {
        parts.push(part("right-agents-count-blocked", blocked));
    }
    if parts.is_empty() {
        if idle > 0 {
            parts.push(part("right-agents-count-idle", idle));
        }
        if unknown > 0 {
            parts.push(part("right-agents-count-unknown", unknown));
        }
    }
    parts.join(" · ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(pane: PaneId, agent: &str, state: &'static str, place: &str) -> AgentRow {
        AgentRow {
            pane,
            window: None,
            here: false,
            agent_id: agent.into(),
            name: display_name(agent),
            title: String::new(),
            state,
            state_label: String::new(),
            place: place.into(),
            icon: icon(agent),
        }
    }

    #[test]
    fn names_and_icons_follow_the_desktop() {
        assert_eq!(display_name("claude"), "Claude Code");
        assert_eq!(display_name("gemini"), "Gemini");
        assert_eq!(icon("claude"), "brand-claude");
        assert_eq!(icon("gemini"), "bot");
    }

    #[test]
    fn the_summary_counts_like_the_desktop() {
        assert_eq!(summary(&[]), "No agents detected");
        let rows = vec![row(1, "claude", "working", "a"), row(2, "codex", "working", "a"), row(3, "pi", "blocked", "b")];
        assert_eq!(summary(&rows), "2 working · 1 needs input");
        let quiet = vec![row(1, "claude", "idle", "a"), row(2, "codex", "unknown", "a"), row(3, "pi", "unknown", "a")];
        assert_eq!(summary(&quiet), "1 idle · 2 unknown");
    }
}
