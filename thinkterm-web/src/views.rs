//! What the page's display layer reads: plain values, serialised as JSON
//! by the bridge, one per part of the chrome. The types they carry are
//! the models' own (`tree::Row`, `navbar::NavView`, `chrome::TabView`),
//! so the JSON is the model, not a second description of it.

use crate::chrome::{Controls, TabView};
use crate::navbar::NavView;
use crate::sidebar::Editing;
use crate::tree::Row;
use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SidebarView {
    pub rows: Vec<Row>,
    pub editing: Editing,
    /// The Space on show, for the page to remember.
    pub space: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TabsView {
    pub tabs: Vec<TabView>,
    pub controls: Controls,
}

pub type NavsView = Vec<NavView>;

/// A passing remark (`sticky` while the page is disconnected, where it is
/// the state rather than a remark), the desktop's takeover card, and the
/// one-line summary probes read.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StatusView {
    pub toast: Option<Toast>,
    pub card: Option<Card>,
    pub summary: String,
}

/// What a menu row's action came to: `copy` is text for the page to put
/// on the clipboard, `paste` asks the page to read the clipboard.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MenuOutcome {
    pub handled: bool,
    pub copy: Option<String>,
    pub paste: bool,
}

/// What a palette pick came to: `page` names something only the page
/// does (`toggle-sidebar`, `settings`, `lang:<tag>`), `recent` is the
/// updated list of picks for the page to keep.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PaletteOutcome {
    pub handled: bool,
    pub page: Option<String>,
    pub recent: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Toast {
    pub text: String,
    pub sticky: bool,
    /// When it was raised, in the page's monotonic milliseconds.
    pub at: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Card {
    pub title: String,
    pub hint: String,
}

/// The static labels the page draws itself (tooltips, headers, the
/// confirmation words), keyed by their catalogue id, in the active locale.
pub const STRING_KEYS: &[&str] = &[
    "sidebar-new-thread",
    "sidebar-pinned",
    "sidebar-workspaces",
    "web-sidebar-other-windows",
    "web-path-placeholder",
    "web-tip-space",
    "web-tip-new-thread",
    "web-tip-new-thread-here",
    "web-tip-new-project",
    "web-tip-restore",
    "web-tip-archive",
    "web-tip-pin",
    "web-tip-unpin",
    "web-tip-delete-thread",
    "web-confirm-delete",
    "web-confirm-close",
    "web-tip-close-pane",
    "web-tip-close-tab",
    "web-tip-new-tab",
    "web-tip-split-down",
    "web-tip-split-right",
    "web-tip-zoom",
    "web-tip-unzoom",
    "web-tip-sidebar",
    "web-fitted",
    "web-tip-fitted",
    "web-tip-sidebar-options",
    "tooltip-sidebar-notifications",
    "tooltip-sidebar-thread-search",
    "tooltip-sidebar-settings",
    "web-tip-search",
    "web-tip-agents",
    "web-tip-settings",
    "web-agents-title",
    "sidebar-settings",
    "settings-language",
    "web-settings-theme",
    "web-theme-dark",
    "web-theme-light",
    "web-theme-system",
    "web-settings-font",
    "web-font-follow",
    "web-font-pinned",
    "web-settings-hover-reveal",
    "web-settings-agents-panel",
    "web-settings-palette-hotkey",
    "web-settings-sidebar-reset",
    "web-settings-about",
    "web-settings-close",
    "language-system",
    "settings-section-general",
    "settings-section-appearance",
    "settings-section-sidebar",
    "settings-section-agents",
    "settings-section-about",
    "web-settings-search",
    "web-settings-build",
    "web-settings-language-description",
    "web-settings-hotkey-description",
    "web-settings-theme-description",
    "web-settings-font-description",
    "web-settings-hover-reveal-description",
    "web-settings-agents-panel-description",
    "web-settings-sidebar-reset-description",
    "web-cmd-font-reset",
    "common-reset",
];

pub fn strings() -> std::collections::BTreeMap<&'static str, String> {
    STRING_KEYS.iter().map(|key| (*key, thinkterm_i18n::tr(key))).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::{Dot, Status, ThreadRow};
    use serde_json::{json, to_value};

    #[test]
    fn rows_serialise_as_kinded_objects() {
        let rows = vec![
            Row::Space { id: "space-1".into(), name: "Default".into() },
            Row::NewThread,
            Row::Thread(ThreadRow {
                id: "thread-1".into(),
                project_id: "project-1".into(),
                name: "main".into(),
                status: Status::Running,
                dot: Dot::Active,
                pinned: false,
                unread: true,
                live: true,
                selected: true,
                deleting: false,
            }),
            Row::Window { window_id: 3, title: "Terminal".into(), selected: false },
        ];
        let v = to_value(SidebarView { rows, editing: Editing::Thread("thread-1".into()), space: Some("space-1".into()) }).unwrap();
        assert_eq!(v["rows"][0], json!({"kind": "space", "id": "space-1", "name": "Default"}));
        assert_eq!(v["space"], "space-1");
        assert_eq!(v["rows"][1], json!({"kind": "new-thread"}));
        let t = &v["rows"][2];
        assert_eq!(t["kind"], "thread");
        assert_eq!(t["project"], "project-1");
        assert_eq!(t["status"], "Running");
        assert_eq!(t["dot"], "Active");
        assert_eq!(t["unread"], true);
        assert_eq!(v["rows"][3], json!({"kind": "window", "id": 3, "title": "Terminal", "selected": false}));
        assert_eq!(v["editing"], json!({"kind": "thread", "id": "thread-1"}));
        assert_eq!(to_value(Editing::None).unwrap(), json!({"kind": "none"}));
    }

    #[test]
    fn every_page_string_is_defined() {
        for key in STRING_KEYS {
            assert!(thinkterm_i18n::has_key(key), "{key}");
        }
        assert_eq!(strings()["web-fitted"], "fitted");
    }

    #[test]
    fn tabs_and_status_shapes() {
        let v = to_value(TabsView {
            tabs: vec![TabView {
                tab_id: 7,
                window_id: 0,
                title: "zsh".into(),
                label: "Terminal".into(),
                target: 9,
                current: true,
                panes: vec![],
            }],
            controls: Controls { following: true, fit: false, closing_tab: Some(7), clipped: Some((80, 24)) },
        })
        .unwrap();
        assert_eq!(v["tabs"][0]["tab"], 7);
        assert_eq!(v["tabs"][0]["window"], 0);
        assert_eq!(v["tabs"][0]["target"], 9);
        assert_eq!(v["controls"], json!({"following": true, "fit": false, "closing": 7, "clipped": [80, 24]}));
        let s = to_value(StatusView {
            toast: Some(Toast { text: "hi".into(), sticky: false, at: 12.5 }),
            card: Some(Card { title: "Terminal is available".into(), hint: "Click or scroll to take control".into() }),
            summary: "zsh".into(),
        })
        .unwrap();
        assert_eq!(s["toast"]["at"], 12.5);
        assert_eq!(s["card"]["hint"], "Click or scroll to take control");
        assert_eq!(to_value(StatusView { toast: None, card: None, summary: String::new() }).unwrap()["card"], serde_json::Value::Null);
    }
}
