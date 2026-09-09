//! The left sidebar: the desktop's Spaces/Projects/Threads panel
//! (`termwindow/ui/sidebar.rs`) as HTML. The markup is built from
//! `tree::Row`s here, on every target; only mounting and clicks are wasm.

use crate::tree::{Dot, Row, Status};
use thinkterm_proto::WindowId;

fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            c => out.push(c),
        }
    }
    out
}

/// A name or path being typed into the panel.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Editing {
    #[default]
    None,
    NewProject,
    Thread(String),
    Project(String),
}

/// The panel's markup.
pub fn html(rows: &[Row], editing: &Editing) -> String {
    let icon = crate::icons::svg;
    let mut h = String::from("<div class=\"handle\"></div><div class=\"list\">");
    let field = |value: &str| format!("<input class=\"rename\" value=\"{}\" spellcheck=\"false\">", escape(value));
    for row in rows {
        match row {
            Row::Space { name } => h.push_str(&format!(
                "<div class=\"row space\">{}<span class=\"t\">{}</span><span class=\"act\" data-action=\"space-menu\" title=\"Space\">{}</span></div>",
                icon("layers"),
                escape(name),
                icon("ellipsis")
            )),
            Row::NewThread => h.push_str(&format!(
                "<div class=\"pill\" data-action=\"new-thread\" title=\"New thread in the project on show\">{}<span>New Thread</span></div>",
                icon("circle-plus")
            )),
            Row::Pinned => h.push_str(&format!("<div class=\"hdr\">{}<span>Pinned</span></div>", icon("pin"))),
            Row::Workspaces => {
                h.push_str(&format!(
                    "<div class=\"hdr\"><span>Workspaces</span><span class=\"act\" data-action=\"new-project\" title=\"New project: a directory on the server\">{}</span></div>",
                    icon("folder-plus")
                ));
                if *editing == Editing::NewProject {
                    h.push_str("<div class=\"path\"><input placeholder=\"~/project or /path on the server\" spellcheck=\"false\"></div>");
                }
            }
            Row::Project { id, name, path, collapsed, archived } => {
                if *archived {
                    h.push_str(&format!(
                        "<div class=\"row project archived\" data-project=\"{}\" title=\"{}\">{}<span class=\"t\">{}</span><span class=\"hov\"><span class=\"act\" data-action=\"unarchive\" data-project=\"{}\" title=\"Restore\">{}</span></span></div>",
                        escape(id), escape(path), icon("archive"), escape(name), escape(id), icon("archive-restore")
                    ));
                } else {
                    let (chev, folder) = if *collapsed { ("chevron-right", "folder") } else { ("chevron-down", "folder-open") };
                    let title = if *editing == Editing::Project(id.clone()) {
                        field(name)
                    } else {
                        format!("<span class=\"t\" data-action=\"rename-project\" data-project=\"{}\">{}</span>", escape(id), escape(name))
                    };
                    h.push_str(&format!(
                        "<div class=\"row project\" data-project=\"{}\" title=\"{}\"><span class=\"chev\">{}</span>{}{title}<span class=\"hov\"><span class=\"act\" data-action=\"new-thread\" data-project=\"{}\" title=\"New thread here\">{}</span><span class=\"act\" data-action=\"archive\" data-project=\"{}\" title=\"Archive this project (ends its programs)\">{}</span></span></div>",
                        escape(id), escape(path), icon(chev), icon(folder), escape(id), icon("plus"), escape(id), icon("archive")
                    ));
                }
            }
            Row::Thread(t) => {
                let status = match t.status {
                    Status::Running => format!("<span class=\"st spin\">{}</span>", icon("loader-circle")),
                    Status::NeedsAttention => format!("<span class=\"st alert\">{}</span>", icon("circle-alert")),
                    Status::Done => format!("<span class=\"st done\">{}</span>", icon("circle-check")),
                    Status::Idle => {
                        let dot = match t.dot {
                            Dot::Active => "active",
                            Dot::Unread => "unread",
                            Dot::Pinned => "pinned",
                            Dot::Open => "open",
                            Dot::Quiet => "quiet",
                        };
                        format!("<span class=\"st dot {dot}\"></span>")
                    }
                };
                let (pin_action, pin_icon, pin_title) = if t.pinned { ("unpin", "pin-off", "Unpin") } else { ("pin", "pin", "Pin") };
                let class = if t.selected { "row thread selected" } else { "row thread" };
                let title = if *editing == Editing::Thread(t.id.clone()) {
                    field(&t.name)
                } else {
                    format!("<span class=\"t\" data-action=\"rename-thread\" data-thread=\"{}\">{}</span>", escape(&t.id), escape(&t.name))
                };
                let (del_class, del_body) = if t.deleting { ("act danger", "delete?".to_string()) } else { ("act", icon("trash-2").to_string()) };
                h.push_str(&format!(
                    "<div class=\"{class}\" data-thread=\"{}\">{status}{title}<span class=\"hov\"><span class=\"act\" data-action=\"{pin_action}\" data-thread=\"{}\" title=\"{pin_title}\">{}</span><span class=\"{del_class}\" data-action=\"delete\" data-thread=\"{}\" title=\"Delete this thread and end its programs. Asks twice.\">{del_body}</span></span></div>",
                    escape(&t.id), escape(&t.id), icon(pin_icon), escape(&t.id)
                ));
            }
            Row::Archived { count, open } => h.push_str(&format!(
                "<div class=\"hdr sub\" data-action=\"archived\"><span class=\"chev\">{}</span>{}<span>Archived ({count})</span></div>",
                icon(if *open { "chevron-down" } else { "chevron-right" }),
                icon("archive")
            )),
            Row::Others => h.push_str("<div class=\"hdr\"><span>Other windows</span></div>"),
            Row::Window { window_id, title, selected } => {
                let class = if *selected { "row thread selected" } else { "row thread" };
                h.push_str(&format!(
                    "<div class=\"{class}\" data-window=\"{window_id}\"><span class=\"st dot {}\"></span><span class=\"t\">{}</span></div>",
                    if *selected { "active" } else { "open" },
                    escape(title)
                ));
            }
        }
    }
    h.push_str("</div>");
    h
}

/// What a click in the panel meant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SideClick {
    Thread(String),
    Window(WindowId),
    ToggleProject(String),
    ToggleArchived,
    NewThread(Option<String>),
    NewProject,
    Pin(String, bool),
    Delete(String),
    RenameThread(String),
    RenameProject(String),
    Archive(String),
    Unarchive(String),
    SpaceMenu,
}

#[cfg(target_arch = "wasm32")]
mod dom {
    use super::SideClick;
    use wasm_bindgen::JsCast;

    pub struct Sidebar {
        root: web_sys::Element,
    }

    impl Sidebar {
        pub fn mount(id: &str) -> Option<Self> {
            let root = web_sys::window()?.document()?.get_element_by_id(id)?;
            Some(Self { root })
        }

        pub fn element(&self) -> &web_sys::Element {
            &self.root
        }

        pub fn set(&self, html: &str) {
            self.root.set_inner_html(html);
        }

        /// What a click landed on. A click on a row's text or dot is the
        /// row; the small buttons and the headers' actions are their own.
        pub fn click_target(ev: &web_sys::MouseEvent) -> Option<SideClick> {
            let target: web_sys::Element = ev.target()?.dyn_into().ok()?;
            let hit = target.closest("[data-action],[data-thread],[data-window],[data-project]").ok()??;
            let thread = || hit.get_attribute("data-thread");
            let project = || hit.get_attribute("data-project");
            if let Some(action) = hit.get_attribute("data-action") {
                return match action.as_str() {
                    "new-thread" => Some(SideClick::NewThread(project())),
                    "new-project" => Some(SideClick::NewProject),
                    "archived" => Some(SideClick::ToggleArchived),
                    "pin" => thread().map(|t| SideClick::Pin(t, true)),
                    "unpin" => thread().map(|t| SideClick::Pin(t, false)),
                    "delete" => thread().map(SideClick::Delete),
                    "archive" => project().map(SideClick::Archive),
                    "unarchive" => project().map(SideClick::Unarchive),
                    "space-menu" => Some(SideClick::SpaceMenu),
                    // A single click on a name is the row; renaming is a double click.
                    "rename-thread" if ev.detail() >= 2 => thread().map(SideClick::RenameThread),
                    "rename-project" if ev.detail() >= 2 => project().map(SideClick::RenameProject),
                    "rename-thread" => thread().map(SideClick::Thread),
                    "rename-project" => project().map(SideClick::ToggleProject),
                    _ => None,
                };
            }
            if let Some(t) = thread() {
                return Some(SideClick::Thread(t));
            }
            if let Some(w) = hit.get_attribute("data-window") {
                return w.parse().ok().map(SideClick::Window);
            }
            project().map(SideClick::ToggleProject)
        }
    }

    /// The panel's width, kept per browser.
    pub fn stored_width() -> Option<f64> {
        let storage = web_sys::window()?.local_storage().ok()??;
        storage.get_item("thinkterm.sidebar").ok()??.parse().ok()
    }

    pub fn store_width(px: f64) {
        if let Some(storage) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) {
            let _ = storage.set_item("thinkterm.sidebar", &format!("{px:.0}"));
        }
    }
}

#[cfg(target_arch = "wasm32")]
pub use dom::{store_width, stored_width, Sidebar};

/// The desktop's bounds for the panel, in CSS px.
pub const MIN_WIDTH: f64 = 127.0;
pub const MAX_WIDTH: f64 = 260.0;
pub const DEFAULT_WIDTH: f64 = 220.0;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::ThreadRow;

    #[test]
    fn the_markup_names_what_a_click_lands_on() {
        let rows = vec![
            Row::Space { name: "Default".into() },
            Row::NewThread,
            Row::Workspaces,
            Row::Project { id: "p1".into(), name: "Home".into(), path: "~".into(), collapsed: false, archived: false },
            Row::Thread(ThreadRow {
                id: "t1".into(),
                project_id: "p1".into(),
                name: "main <1>".into(),
                status: Status::Idle,
                dot: Dot::Active,
                pinned: false,
                unread: false,
                live: true,
                selected: true,
                deleting: false,
            }),
        ];
        let h = html(&rows, &Editing::None);
        assert!(h.contains("data-thread=\"t1\""));
        assert!(!h.contains("<input"));
        let h = html(&rows, &Editing::Thread("t1".into()));
        assert!(h.contains("<input class=\"rename\" value=\"main &lt;1&gt;\""));
        assert!(html(&rows, &Editing::NewProject).contains("class=\"path\""));
        assert!(h.contains("main &lt;1&gt;"), "titles are escaped");
        assert!(h.contains("class=\"row thread selected\""));
        assert!(h.contains("data-action=\"new-project\""));
        assert!(h.contains("data-project=\"p1\""));
    }
}
