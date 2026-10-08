//! The search palette as data: what can be found (threads, tabs, panes,
//! Spaces, commands), how a query ranks it, and the id a pick hands back.
//! The ranking is the desktop's (`command_palette.rs` `compute_order`):
//! nucleo scores, an exact title first, results regrouped under their
//! section with the best section first.

use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};
use serde::Serialize;
use thinkterm_i18n::tr;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Group {
    Threads,
    Tabs,
    Panes,
    Spaces,
    Commands,
}

impl Group {
    fn label(self) -> String {
        match self {
            Group::Threads => tr("command-palette-group-thread"),
            Group::Tabs => tr("web-palette-tabs"),
            Group::Panes => tr("web-palette-panes"),
            Group::Spaces => tr("command-palette-group-space"),
            Group::Commands => tr("web-palette-commands"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Entry {
    /// `thread:<id>`, `tab:<id>`, `pane:<id>`, `space:<id>`, `cmd:<name>`, `lang:<tag>`.
    pub id: String,
    pub title: String,
    pub subtitle: String,
    /// A lucide icon name.
    pub icon: &'static str,
    /// Right-hand text: a shortcut, a Space, a state.
    pub accessory: String,
    pub group: Group,
    /// Extra words a query may hit (a project's path, a Space's name).
    #[serde(skip)]
    pub terms: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Section {
    pub group: Group,
    pub label: String,
    pub entries: Vec<Entry>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Results {
    pub sections: Vec<Section>,
    pub placeholder: String,
    pub empty: String,
}

/// The fixed commands, with their ids and the keys of their labels.
const COMMANDS: &[(&str, &str, &str, &str)] = &[
    // id, label key, icon, shortcut
    ("new-thread", "menu-new-thread", "circle-plus", ""),
    ("new-tab", "web-cmd-new-tab", "plus", ""),
    ("split-right", "menu-split-right", "square-split-horizontal", ""),
    ("split-down", "menu-split-down", "square-split-vertical", ""),
    ("zoom", "menu-zoom-pane", "maximize-2", ""),
    ("close-pane", "web-cmd-close-pane", "x", ""),
    ("take-over", "web-cmd-take-over", "square-terminal", "Ctrl+Shift+T"),
    ("follow", "web-cmd-follow", "eye", ""),
    ("toggle-sidebar", "web-cmd-toggle-sidebar", "panel-left", ""),
    ("settings", "sidebar-settings", "settings", ""),
    // The desktop's Shell › Remote Hosts… and Add Remote Host…: the page's
    // own, as Settings is.
    ("remote-hosts", "web-machines-title", "network", ""),
    ("add-remote-host", "menu-add-remote-host", "plus", ""),
    ("font-up", "web-cmd-font-up", "zoom-in", "Cmd+="),
    ("font-down", "web-cmd-font-down", "zoom-out", "Cmd+-"),
    ("font-reset", "web-cmd-font-reset", "rotate-ccw", "Cmd+0"),
];

pub fn commands() -> Vec<Entry> {
    let out: Vec<Entry> = COMMANDS
        .iter()
        .map(|(id, key, icon, keys)| Entry {
            id: format!("cmd:{id}"),
            title: tr(key),
            subtitle: String::new(),
            icon,
            accessory: keys.to_string(),
            group: Group::Commands,
            terms: id.replace('-', " "),
        })
        .collect();
    out
}

/// The last two components of a project path, as the desktop indexes it
/// (`search_path_terms`): "users"/"documents" must not hit every project.
pub fn path_terms(path: &str) -> String {
    let parts: Vec<&str> = path.trim_end_matches('/').rsplit('/').filter(|p| !p.is_empty()).take(2).collect();
    parts.join(" ")
}

fn haystack(entry: &Entry) -> String {
    format!("{} {} {} {}", entry.title, entry.subtitle, entry.accessory, entry.terms)
}

/// Rank `entries` for `query`: nucleo scores, an exact title match first,
/// then regrouped under their sections with the best section first.
/// An empty query lists everything in source order, the `recent` ids
/// (most recent first) ahead of the rest of their section.
pub fn rank(query: &str, entries: Vec<Entry>, recent: &[String]) -> Vec<Section> {
    let order = [Group::Threads, Group::Tabs, Group::Panes, Group::Spaces, Group::Commands];
    let query = query.trim();
    let scored: Vec<(u64, Entry)> = if query.is_empty() {
        entries
            .into_iter()
            .map(|e| {
                let recency = recent.iter().position(|r| *r == e.id).map(|p| u32::MAX as u64 - p as u64).unwrap_or(1);
                (recency, e)
            })
            .collect()
    } else {
        let mut matcher = Matcher::new(Config::DEFAULT);
        let pattern = Pattern::parse(query, CaseMatching::Ignore, Normalization::Smart);
        let mut buf = Vec::new();
        entries
            .into_iter()
            .filter_map(|e| {
                if e.title.eq_ignore_ascii_case(query) {
                    return Some((u64::MAX, e));
                }
                let hay = haystack(&e);
                pattern.score(Utf32Str::new(&hay, &mut buf), &mut matcher).map(|s| (s as u64, e))
            })
            .collect()
    };
    let mut sections: Vec<(u64, Section)> = Vec::new();
    for group in order {
        let mut members: Vec<(u64, Entry)> = scored.iter().filter(|(_, e)| e.group == group).cloned().collect();
        if members.is_empty() {
            continue;
        }
        // Stable: equal scores keep their source order.
        members.sort_by(|a, b| b.0.cmp(&a.0));
        let best = members[0].0;
        sections.push((best, Section { group, label: group.label(), entries: members.into_iter().map(|(_, e)| e).collect() }));
    }
    if !query.is_empty() {
        sections.sort_by(|a, b| b.0.cmp(&a.0));
    }
    sections.into_iter().map(|(_, s)| s).collect()
}

pub fn results(query: &str, entries: Vec<Entry>, recent: &[String]) -> Results {
    Results {
        sections: rank(query, entries, recent),
        placeholder: tr("command-palette-placeholder"),
        empty: tr("command-palette-empty"),
    }
}

/// Remember a pick: most recent first, without duplicates, capped.
pub fn remember(recent: &mut Vec<String>, id: &str) {
    recent.retain(|r| r != id);
    recent.insert(0, id.to_string());
    recent.truncate(50);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn thread(id: &str, name: &str, project: &str, path: &str) -> Entry {
        Entry {
            id: format!("thread:{id}"),
            title: name.into(),
            subtitle: project.into(),
            icon: "square-terminal",
            accessory: "Default".into(),
            group: Group::Threads,
            terms: path_terms(path),
        }
    }

    #[test]
    fn path_terms_keep_the_last_two_components() {
        assert_eq!(path_terms("/Users/me/Documents/GitHub/thinkterm/"), "thinkterm GitHub");
        assert_eq!(path_terms("~"), "~");
    }

    #[test]
    fn a_typed_thread_name_ranks_its_section_first() {
        let entries = vec![
            thread("a", "main", "thinkterm", "/Users/me/GitHub/thinkterm"),
            thread("b", "notes", "diary", "/Users/me/diary"),
        ]
        .into_iter()
        .chain(commands())
        .collect();
        let sections = rank("note", entries, &[]);
        assert_eq!(sections[0].group, Group::Threads);
        assert_eq!(sections[0].entries[0].id, "thread:b");
        // A project's path finds its threads; "users" alone does not.
        let sections = rank("github", vec![thread("a", "main", "thinkterm", "/Users/me/GitHub/thinkterm")], &[]);
        assert_eq!(sections.len(), 1);
        assert!(rank("users", vec![thread("a", "main", "thinkterm", "/Users/me/GitHub/thinkterm")], &[]).is_empty());
    }

    #[test]
    fn an_exact_title_beats_a_fuzzy_one_and_recents_lead_an_empty_query() {
        let entries = vec![thread("a", "zoom lens", "p", ""), thread("b", "zoom", "p", "")];
        let sections = rank("zoom", entries.clone(), &[]);
        assert_eq!(sections[0].entries[0].id, "thread:b");
        let sections = rank("", entries, &["thread:b".into()]);
        assert_eq!(sections[0].entries[0].id, "thread:b");
        let mut recent = vec!["cmd:zoom".to_string()];
        remember(&mut recent, "thread:b");
        remember(&mut recent, "cmd:zoom");
        assert_eq!(recent, ["cmd:zoom", "thread:b"]);
    }

    #[test]
    fn commands_are_the_desktop_s_where_it_has_them() {
        let cmds = commands();
        assert!(cmds.iter().any(|e| e.id == "cmd:take-over" && e.title == "Take Over the Terminal"));
        assert!(cmds.iter().any(|e| e.id == "cmd:font-reset" && e.title == "Reset font size"));
        assert!(!cmds.iter().any(|e| e.id.starts_with("lang:")), "the language is a setting, not a command");
        assert_eq!(rank("", cmds, &[]).len(), 1);
    }
}
