//! What the page's display layer reads: plain values, serialised as JSON
//! by the bridge, one per part of the chrome. The types they carry are
//! the models' own (`tree::Row`, `navbar::NavView`, `chrome::TabView`),
//! so the JSON is the model, not a second description of it.

use crate::chrome::{Controls, TabView};
use crate::navbar::NavView;
use crate::sidebar::{Editing, FooterAction, Reveal};
use crate::tree::Row;
use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SidebarView {
    pub rows: Vec<Row>,
    pub editing: Editing,
    /// The Space on show, for the page to remember.
    pub space: Option<String>,
    /// Why the last "add workspace" path was refused, shown next to the
    /// field it was typed into rather than as a passing remark.
    pub new_project_error: Option<String>,
    /// What a hover over the window's left edge does with the panel put
    /// away, and the footer the panel ends with. Rules, not drawing: a
    /// native shell reads the same values.
    pub reveal: Reveal,
    pub footer: Vec<FooterAction>,
    pub footer_label_min_width: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TabsView {
    pub tabs: Vec<TabView>,
    pub controls: Controls,
    /// The card heading every window tab: the terminal's, as on the
    /// desktop (`tab_icons`); none with tab icons off.
    pub icon: Option<String>,
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
    /// `busy` (another device holds it), `free` (nobody does), `taking`
    /// (this page asked), `refused` (the server kept it there).
    pub state: &'static str,
    /// The button's words; a press asks for the terminal.
    pub action: String,
}

/// The static labels the page draws itself (tooltips, headers, the
/// confirmation words), keyed by their catalogue id, in the active locale.
pub const STRING_KEYS: &[&str] = &[
    "web-machines-title",
    "web-machines-off",
    "web-machines-off-hint",
    "web-machines-failed-off",
    "ssh-group-hosts",
    "web-machines-here",
    "web-machines-connected",
    "web-machines-empty",
    "web-machines-connect",
    "web-machines-show",
    "web-machines-disconnect",
    "web-machines-forget",
    "web-machines-retry",
    "web-machines-plain-ssh",
    "web-machines-name",
    "web-machines-host",
    "web-machines-port",
    "web-machines-user",
    "web-machines-password",
    "web-machines-optional",
    "web-machines-add-connect",
    "web-machines-cancel",
    "web-machines-from-ssh-config",
    "web-machines-from-desktop",
    "web-machines-from-web",
    "web-machines-password-kept",
    "web-machines-step-connecting",
    "web-machines-step-authenticating",
    "web-machines-step-checking",
    "web-machines-step-installing",
    "web-machines-step-updating",
    "web-machines-step-starting",
    "web-machines-reconnecting",
    "web-machines-needs-you",
    "web-machines-not-connected",
    "web-machines-ask-host-key",
    "web-machines-ask-host-key-hint",
    "web-machines-ask-password",
    "web-machines-ask-code",
    "web-machines-ask-answer",
    "web-machines-ask-install",
    "web-machines-ask-install-hint",
    "web-machines-ask-replace",
    "web-machines-ask-replace-hint",
    "web-machines-ask-stop",
    "web-machines-ask-stop-hint",
    "web-machines-remember",
    "web-machines-trust",
    "web-machines-install",
    "web-machines-replace",
    "web-machines-stop",
    "web-machines-not-now",
    "web-machines-continue",
    "web-machines-failed-unreachable",
    "web-machines-failed-host-key",
    "web-machines-failed-auth",
    "web-machines-failed-cancelled",
    "web-machines-failed-declined",
    "web-machines-failed-cannot-install",
    "web-machines-failed-same-server",
    "web-machines-failed-failed",
    "web-machines-failed-desktop",
    "web-machines-failed-gone",
    "web-machines-bad-port",
    "web-machines-details",
    // The add form's card, as the desktop's host form heads it.
    "ssh-group-connection",
    // The page's toolbar, as the desktop's Remote Hosts page has it.
    "ssh-new-host",
    "ssh-search",
    // The desktop's Tab Icons switch.
    "settings-tab-icons-enabled",
    "settings-tab-icons-enabled-description",
    "menu-add-remote-host",
    "menu-new-space-here",
    "sidebar-new-thread",
    "sidebar-pinned",
    "sidebar-workspaces",
    "web-sidebar-other-windows",
    "web-path-placeholder",
    "web-add-workspace",
    "web-add-workspace-hint",
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
    "web-tip-sidebar-options",
    "tooltip-sidebar-notifications",
    "web-tip-search",
    "web-tip-agents",
    "web-tip-settings",
    "web-agents-title",
    "settings-language",
    "web-settings-theme",
    "web-theme-dark",
    "web-theme-light",
    "web-theme-system",
    "web-settings-scroll-mode",
    "web-settings-scroll-mode-description",
    "web-scroll-mode-stepped",
    "web-scroll-mode-smooth",
    "web-settings-font",
    "web-font-follow",
    "web-font-pinned",
    "web-settings-hover-reveal",
    "web-settings-agents-panel",
    "web-settings-palette-hotkey",
    "web-settings-sidebar-reset",
    "web-settings-about",
    "web-settings-close",
    // The way back to the sections, on a phone.
    "sidebar-settings",
    "language-system",
    "settings-section-general",
    "settings-section-appearance",
    "settings-section-sidebar",
    "settings-section-agents",
    "settings-section-about",
    "web-settings-search",
    "web-settings-build",
    // Settings › About, in the desktop's words.
    "settings-about-version",
    "settings-about-license",
    "settings-about-copy",
    "settings-about-copied",
    "ssh-field-host",
    "web-settings-language-description",
    "web-settings-hotkey-description",
    "web-settings-theme-description",
    "web-settings-font-description",
    "web-settings-scheme",
    "web-settings-scheme-description",
    "web-scheme-desktop",
    "web-scheme-search",
    "web-scheme-preview",
    "web-settings-hover-reveal-description",
    "web-settings-agents-panel-description",
    "web-settings-sidebar-reset-description",
    "web-cmd-font-reset",
    "common-reset",
    "web-tip-new-pane",
    "command-palette-empty",
    "right-new-snippet",
    "right-edit-snippet",
    "right-search",
    "right-run",
    "menu-paste",
    "right-save",
    "right-cancel",
    "right-action-description",
    "right-action-description-placeholder",
    "right-script-required",
    "right-script-placeholder",
    "web-tip-delete-snippet",
    "settings-sidebar-description",
    "right-mode-snippets",
    "right-plugin-starting",
    "settings-sidebar-snippets-description",
    "settings-section-plugins",
    "settings-plugins-description",
    "settings-plugins-reload",
    "settings-plugins-reload-description",
    "settings-plugins-reload-button",
    "settings-plugins-unusable",
    "settings-plugins-allow",
    "settings-plugins-background-always",
    "settings-plugins-background-briefly",
    "settings-plugins-background-never",
];

pub fn strings() -> std::collections::BTreeMap<&'static str, String> {
    let mut strings: std::collections::BTreeMap<&'static str, String> =
        STRING_KEYS.iter().map(|key| (*key, thinkterm_i18n::tr(key))).collect();
    let mut args = thinkterm_i18n::FluentArgs::new();
    args.set("count", COUNT_SLOT);
    for key in COUNTED_KEYS {
        strings.insert(*key, thinkterm_i18n::tr_args(key, &args));
    }
    strings
}

/// Labels with a number in them: sent with `COUNT_SLOT` where it goes, for
/// the page to fill in (`sCount` in client.svelte.ts).
pub const COUNTED_KEYS: &[&str] = &["ssh-system-hosts", "right-agents-more-subtasks"];
pub const COUNT_SLOT: &str = "{count}";

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
        let v = to_value(SidebarView {
            rows,
            editing: Editing::Thread("thread-1".into()),
            space: Some("space-1".into()),
            new_project_error: None,
            reveal: crate::sidebar::REVEAL,
            footer: vec![],
            footer_label_min_width: crate::sidebar::FOOTER_LABEL_MIN_WIDTH,
        })
        .unwrap();
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
        // The rules the display layer reads rather than decides.
        assert_eq!(v["reveal"], json!({"edge": 6.0, "dwell_ms": 150, "retreat_ms": 250}));
        assert_eq!(v["footer_label_min_width"], 168.0);
    }

    #[test]
    fn every_page_string_is_defined() {
        for key in STRING_KEYS {
            assert!(thinkterm_i18n::has_key(key), "{key}");
        }
        assert_eq!(strings()["web-tip-sidebar"].is_empty(), false);
    }

    /// A key the page's sources name in quotes -- in a call, or in a list
    /// of choices -- reaches the page, which shows the key itself otherwise.
    #[test]
    fn every_string_the_page_names_is_sent_to_it() {
        let ui = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("ui/src");
        let mut missing = Vec::new();
        for entry in std::fs::read_dir(&ui).unwrap() {
            let path = entry.unwrap().path();
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            if !(name.ends_with(".svelte") || name.ends_with(".ts")) {
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap();
            let bytes = text.as_bytes();
            let mut at = 0;
            while at < bytes.len() {
                let quote = bytes[at];
                at += 1;
                if !matches!(quote, b'\'' | b'"' | b'`') {
                    continue;
                }
                let Some(len) = bytes[at..].iter().position(|&b| b == quote) else {
                    continue;
                };
                let quoted = &text[at..at + len];
                let key_like = quoted.starts_with(|c: char| c.is_ascii_lowercase())
                    && quoted.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
                if key_like && thinkterm_i18n::has_key(quoted) {
                    if !STRING_KEYS.contains(&quoted) && !COUNTED_KEYS.contains(&quoted) {
                        missing.push(format!("{name}: {quoted}"));
                    }
                    at += len + 1;
                }
            }
        }
        assert!(missing.is_empty(), "named by the page and not sent to it: {missing:?}");
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
            icon: Some("terminal".into()),
        })
        .unwrap();
        assert_eq!(v["icon"], "terminal");
        assert_eq!(v["tabs"][0]["tab"], 7);
        assert_eq!(v["tabs"][0]["window"], 0);
        assert_eq!(v["tabs"][0]["target"], 9);
        assert_eq!(v["controls"], json!({"following": true, "fit": false, "closing": 7, "clipped": [80, 24]}));
        let s = to_value(StatusView {
            toast: Some(Toast { text: "hi".into(), sticky: false, at: 12.5 }),
            card: Some(Card { title: "Terminal is available".into(), hint: "Click or scroll to take control".into(), state: "free", action: "Take control".into() }),
            summary: "zsh".into(),
        })
        .unwrap();
        assert_eq!(s["toast"]["at"], 12.5);
        assert_eq!(s["card"]["hint"], "Click or scroll to take control");
        assert_eq!(to_value(StatusView { toast: None, card: None, summary: String::new() }).unwrap()["card"], serde_json::Value::Null);
    }
}
