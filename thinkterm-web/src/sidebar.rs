//! The left sidebar: the desktop's Spaces/Projects/Threads panel
//! (`termwindow/ui/sidebar.rs`) as HTML. The markup is built from
//! `tree::Row`s here, on every target; only mounting and clicks are wasm.

use thinkterm_proto::WindowId;

#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize)]
#[serde(tag = "kind", content = "id", rename_all = "kebab-case")]
pub enum Editing {
    #[default]
    None,
    NewProject,
    Thread(String),
    Project(String),
    Space(String),
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

pub const MIN_WIDTH: f64 = 127.0;
pub const MAX_WIDTH: f64 = 260.0;
pub const DEFAULT_WIDTH: f64 = 220.0;
