//! The right-hand Agents panel as data: one row per pane an agent runs
//! in, as the desktop's panel lists them (`agent_panel.rs`), and the
//! one-line summary above them.

use crate::tree::TreeModel;
use serde::Serialize;
use thinkterm_i18n::{tr, tr_args, FluentArgs};
use thinkterm_proto::{
    AgentState, PaneId, ProgramBlockedKind, ProgramReportChild, ProgramReportState, WindowId,
};

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
    /// What the program said about itself (OSC 7501): what it waits for,
    /// what it is doing or why it stopped, how far it got, its sub-tasks.
    pub kind: Option<String>,
    pub said: Option<String>,
    pub progress: Option<u8>,
    /// "7 sub-tasks · 5 working": the one line the sub-tasks fold into,
    /// as on the desktop; empty when there are none.
    pub subtasks_summary: String,
    /// Whether that line is open, listing them.
    pub subtasks_open: bool,
    /// While it is open: the sub-tasks listed, and how many more there are.
    pub subtasks: Vec<AgentSubtask>,
    pub more_subtasks: usize,
}

/// One of a program's sub-tasks, as the desktop's panel lists them.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AgentSubtask {
    pub state: &'static str,
    pub label: String,
}

/// Sub-tasks a row lists before it says how many more there are, as the
/// desktop's `AGENT_SUBTASKS_SHOWN`.
const SUBTASKS_SHOWN: usize = 5;

/// The line a row's sub-tasks fold into, worded as the desktop's panel
/// words it: how many, and how many of them are working.
fn subtasks_summary(children: &[ProgramReportChild]) -> String {
    if children.is_empty() {
        return String::new();
    }
    let count = |key: &str, n: usize| {
        let mut args = FluentArgs::new();
        args.set("count", n as i64);
        tr_args(key, &args)
    };
    let all = count("right-agents-subtasks", children.len());
    match children.iter().filter(|child| child.state == ProgramReportState::Working).count() {
        0 => all,
        working => format!("{all} · {}", count("right-agents-count-working", working)),
    }
}

fn kind_label(kind: ProgramBlockedKind) -> String {
    tr(match kind {
        ProgramBlockedKind::Permission => "agent-kind-permission",
        ProgramBlockedKind::Question => "agent-kind-question",
        ProgramBlockedKind::Auth => "agent-kind-auth",
    })
}

fn report_state_name(state: ProgramReportState) -> &'static str {
    match state {
        ProgramReportState::Working => "working",
        ProgramReportState::Blocked => "blocked",
        ProgramReportState::Error => "error",
        ProgramReportState::Done => "done",
        ProgramReportState::Idle => "idle",
    }
}

/// One tab of the right panel's selector, as the desktop lists them
/// (`RightSidebarMode::ALL`: Files, Notes, Code, Agents, then the plugins'
/// panels). Which exist and which the browser can open is decided here,
/// not in the display layer.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PanelTab {
    /// `snippets`, `agents`, or `plugin:` and the plugin's id.
    pub id: String,
    /// A lucide name, as the menus name theirs.
    pub icon: String,
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
    pub active: String,
    /// Whether the active tab carries its label ([`labeled`]).
    pub labeled: bool,
}

/// The most tabs the selector names the active one of: past this its label
/// has no room, and every tab is its icon alone, the active one told by its
/// pill -- as the desktop's selector does (`right_sidebar.rs`,
/// RIGHT_SIDEBAR_LABELED_MODES).
pub const LABELED_TABS: usize = 5;

/// Whether the selector over `tabs` names the active one.
pub fn labeled(tabs: &[PanelTab]) -> bool {
    tabs.len() <= LABELED_TABS
}

/// A panel a plugin adds, as the plugin list names it.
#[derive(Debug, Clone, PartialEq)]
pub struct PluginPanel {
    pub id: String,
    pub name: String,
    /// A Lucide icon's name, one of `thinkterm_plugin_panel::ICONS`.
    pub icon: String,
}

/// The tab id of plugin `id`'s panel.
pub fn plugin_tab(id: &str) -> String {
    format!("plugin:{id}")
}

/// The panel's sections, in the desktop's order, but only those the page
/// can open: a button for a feature the browser lacks is noise. Files and
/// Notes join here when they get a wire. Snippets is there while its
/// plugin is on, as on the desktop, and the plugins' panels after the
/// rest.
pub fn panel_tabs(snippets: bool, plugins: &[PluginPanel]) -> Vec<PanelTab> {
    let own = [
        ("snippets", "code-xml", "right-mode-snippets"),
        ("agents", "bot", "right-mode-agents"),
    ]
        .into_iter()
        .filter(|(id, _, _)| snippets || *id != "snippets")
        .map(|(id, icon, key)| {
            let label = tr(key);
            PanelTab { id: id.to_string(), icon: icon.to_string(), label: label.clone(), available: true, tip: label }
        });
    let plugins = plugins.iter().map(|panel| PanelTab {
        id: plugin_tab(&panel.id),
        icon: panel.icon.clone(),
        label: panel.name.clone(),
        available: true,
        tip: panel.name.clone(),
    });
    own.chain(plugins).collect()
}

#[cfg(test)]
mod panel_tests {
    use super::*;

    #[test]
    fn snippets_agents_and_the_plugins_panels_are_available_in_a_browser() {
        let stocks = PluginPanel { id: "stocks".into(), name: "Stocks".into(), icon: "chart-line".into() };
        let tabs = panel_tabs(true, std::slice::from_ref(&stocks));
        assert!(tabs.iter().map(|t| t.id.as_str()).eq(["snippets", "agents", "plugin:stocks"]));
        assert!(tabs.iter().all(|t| t.available));
        assert_eq!((tabs[2].label.as_str(), tabs[2].icon.as_str()), ("Stocks", "chart-line"));
        let tabs = panel_tabs(false, &[]);
        assert!(tabs.iter().map(|t| t.id.as_str()).eq(["agents"]), "the plugin is off");
    }

    #[test]
    fn past_five_tabs_the_active_one_is_its_icon_alone() {
        let panel = |id: &str| PluginPanel { id: id.into(), name: id.into(), icon: "chart-line".into() };
        let three: Vec<PluginPanel> = ["a", "b", "c"].into_iter().map(panel).collect();
        assert!(labeled(&panel_tabs(true, &three)), "five: named");
        let four: Vec<PluginPanel> = ["a", "b", "c", "d"].into_iter().map(panel).collect();
        assert!(!labeled(&panel_tabs(true, &four)), "six: no room");
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

/// The mark the page draws for an agent, as the desktop picks it
/// (`agent_status::brand_icon`, lobe-icons); `bot` for one with no logo.
pub fn icon(agent_id: &str) -> &'static str {
    match agent_id {
        "claude" => "brand-claude",
        "copilot" => "brand-copilot",
        "kimi" => "brand-kimi",
        "codex" => "brand-codex",
        "cursor" => "brand-cursor",
        "opencode" => "brand-opencode",
        "pi" => "brand-pi",
        _ => "bot",
    }
}

fn state_key(state: AgentState) -> (&'static str, &'static str) {
    match state {
        AgentState::Working => ("working", "right-agents-state-working"),
        AgentState::Blocked => ("blocked", "right-agents-state-blocked"),
        AgentState::Idle => ("idle", "right-agents-state-idle"),
        AgentState::Unknown => ("unknown", "right-agents-state-unknown"),
        AgentState::Error => ("error", "right-agents-state-error"),
    }
}

/// The rows, sorted as the desktop sorts them: by place, agent, pane --
/// never floating the current window, so a switch does not reorder them.
/// `open` says whose sub-task lists are open; the rest stay folded.
pub fn rows(
    tree: &TreeModel,
    current_window: WindowId,
    title_of: impl Fn(PaneId) -> Option<String>,
    open: impl Fn(PaneId) -> bool,
) -> Vec<AgentRow> {
    let mut rows: Vec<AgentRow> = tree
        .agent_entries()
        .map(|(pane, agent_id, state, title)| {
            let (place, window) = tree.place_of_pane(pane);
            let report = tree.agent_report(pane);
            // A result nobody has seen is its own word, not merely idle.
            let (state_name, state_label) = match (state, report.map(|r| r.state)) {
                (AgentState::Idle, Some(ProgramReportState::Done)) => ("done", "right-agents-state-done"),
                _ => state_key(state),
            };
            let children = report.map(|r| r.children.as_slice()).unwrap_or_default();
            let subtasks_open = !children.is_empty() && open(pane);
            let listed = if subtasks_open { children } else { &[] };
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
                kind: report.and_then(|r| r.kind).map(kind_label),
                said: report.and_then(|r| r.msg.clone()),
                progress: report.and_then(|r| r.progress),
                subtasks_summary: subtasks_summary(children),
                subtasks_open,
                subtasks: listed
                    .iter()
                    .take(SUBTASKS_SHOWN)
                    .map(|child| {
                        let name = child.title.as_deref().or(child.msg.as_deref()).unwrap_or(&child.id);
                        AgentSubtask {
                            state: report_state_name(child.state),
                            label: match child.kind {
                                Some(kind) => format!("{name} · {}", kind_label(kind)),
                                None => name.to_string(),
                            },
                        }
                    })
                    .collect(),
                more_subtasks: listed.len().saturating_sub(SUBTASKS_SHOWN),
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
    // A finished result is idle to the counts, as the desktop's tally has it.
    let (working, blocked, idle, unknown) =
        (count("working"), count("blocked"), count("idle") + count("done"), count("unknown"));
    let errored = count("error");
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
    if errored > 0 {
        parts.push(part("right-agents-count-error", errored));
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
            kind: None,
            said: None,
            progress: None,
            subtasks_summary: String::new(),
            subtasks_open: false,
            subtasks: Vec::new(),
            more_subtasks: 0,
        }
    }

    #[test]
    fn names_and_icons_follow_the_desktop() {
        assert_eq!(display_name("claude"), "Claude Code");
        assert_eq!(display_name("gemini"), "Gemini");
        assert_eq!(icon("claude"), "brand-claude");
        assert_eq!(icon("codex"), "brand-codex");
        assert_eq!(icon("kimi"), "brand-kimi");
        assert_eq!(icon("gemini"), "bot");
    }

    /// Sub-tasks fold to the one line that counts them, as on the desktop;
    /// only an open list carries them.
    #[test]
    fn sub_tasks_fold_to_one_line_until_opened() {
        use thinkterm_proto::{AgentEvidence, AgentStatus, ProgramReport};
        let child = |n: usize, state| ProgramReportChild {
            id: format!("task-{n}"),
            state,
            kind: None,
            progress: None,
            title: None,
            msg: None,
        };
        let mut children: Vec<_> = (0..5).map(|n| child(n, ProgramReportState::Working)).collect();
        children.extend((5..7).map(|n| child(n, ProgramReportState::Done)));
        let mut tree = TreeModel::default();
        tree.apply_agent(
            5,
            Some(&AgentStatus {
                agent_id: "claude".into(),
                state: AgentState::Working,
                evidence: AgentEvidence::Report,
                session_id: None,
                since_unix: 0,
                ended: false,
                report: Some(ProgramReport {
                    state: ProgramReportState::Working,
                    kind: None,
                    progress: None,
                    app: None,
                    title: None,
                    msg: None,
                    children,
                }),
            }),
        );

        let folded = rows(&tree, 1, |_| None, |_| false).remove(0);
        assert!(!folded.subtasks_open);
        assert!(folded.subtasks.is_empty() && folded.more_subtasks == 0);
        let line = &folded.subtasks_summary;
        assert!(line.contains('7') && line.contains('5'), "{line}");

        let open = rows(&tree, 1, |_| None, |pane| pane == 5).remove(0);
        assert!(open.subtasks_open);
        assert_eq!((open.subtasks.len(), open.more_subtasks), (5, 2));
        assert_eq!(open.subtasks_summary, folded.subtasks_summary);
    }

    #[test]
    fn the_summary_counts_like_the_desktop() {
        assert_eq!(summary(&[]), "No agents detected");
        let rows = vec![row(1, "claude", "working", "a"), row(2, "codex", "working", "a"), row(3, "pi", "blocked", "b")];
        assert_eq!(summary(&rows), "2 working · 1 needs input");
        let quiet = vec![row(1, "claude", "idle", "a"), row(2, "codex", "unknown", "a"), row(3, "pi", "unknown", "a")];
        assert_eq!(summary(&quiet), "1 idle · 2 unknown");
        // A finished result counts as idle; an error is named.
        let done = vec![row(1, "claude", "done", "a")];
        assert_eq!(summary(&done), "1 idle");
        let failed = vec![row(1, "claude", "error", "a"), row(2, "codex", "working", "a")];
        assert_eq!(summary(&failed), "1 working · 1 error");
    }
}
