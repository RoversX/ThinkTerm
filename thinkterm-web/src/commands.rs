//! The display layer's commands as strings, turned into the App's enums.
//! Shared by the page's bridge and the phone's FFI so both name a click
//! the same way.

use crate::chrome::Click;
use crate::sidebar::SideClick;
use thinkterm_proto::{PaneId, SplitDirection, TabId};

pub fn side_click(kind: &str, id: Option<String>, flag: Option<bool>) -> Option<SideClick> {
    Some(match (kind, id) {
        ("thread", Some(id)) => SideClick::Thread(id),
        ("window", Some(id)) => SideClick::Window(id.parse().ok()?),
        ("toggle-project", Some(id)) => SideClick::ToggleProject(id),
        ("toggle-archived", _) => SideClick::ToggleArchived,
        ("new-thread", project) => SideClick::NewThread(project),
        ("new-project", _) => SideClick::NewProject,
        ("pin", Some(id)) => SideClick::Pin(id, flag.unwrap_or(true)),
        ("delete", Some(id)) => SideClick::Delete(id),
        ("rename-thread", Some(id)) => SideClick::RenameThread(id),
        ("rename-project", Some(id)) => SideClick::RenameProject(id),
        ("archive", Some(id)) => SideClick::Archive(id),
        ("unarchive", Some(id)) => SideClick::Unarchive(id),
        ("space-menu", _) => SideClick::SpaceMenu,
        _ => return None,
    })
}

pub fn chrome_click(action: &str, pane: Option<PaneId>, tab: Option<TabId>) -> Option<Click> {
    Some(match (action, pane, tab) {
        ("pane", Some(p), _) => Click::Pane(p),
        ("follow", _, _) => Click::Follow,
        ("new-tab", _, _) => Click::NewTab,
        ("new-pane", Some(p), _) => Click::NewInStack(p),
        ("split-right", p, _) => Click::SplitRight(p),
        ("split-below", p, _) => Click::SplitBelow(p),
        ("zoom", p, _) => Click::Zoom(p),
        ("close", _, _) => Click::Close,
        ("close-pane", Some(p), _) => Click::ClosePane(p),
        ("close-tab", _, Some(t)) => Click::CloseTab(t),
        _ => return None,
    })
}

/// A drop's target edge, as the page names it.
pub fn drop_edge(edge: &str) -> Option<(SplitDirection, bool)> {
    Some(match edge {
        "left" => (SplitDirection::Horizontal, false),
        "right" => (SplitDirection::Horizontal, true),
        "top" => (SplitDirection::Vertical, false),
        "bottom" => (SplitDirection::Vertical, true),
        _ => return None,
    })
}
