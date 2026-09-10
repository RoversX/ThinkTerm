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

/// The hover reveal's rules, held here rather than in the display layer:
/// a native shell reads them from the same JSON. Lengths are CSS pixels
/// (the desktop's design pixels are a 2x grid).
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
pub struct Reveal {
    /// How far into the window the left-edge trigger reaches
    /// (`SIDEBAR_HOVER_HOT_ZONE_WIDTH` 12 design px).
    pub edge: f64,
    /// How long the pointer rests there before the panel comes back.
    pub dwell_ms: u32,
    /// How long a departed pointer has to come back before it leaves.
    pub retreat_ms: u32,
}

/// The desktop dwells 180ms and waits 400ms (`SIDEBAR_HOVER_DWELL_MS`,
/// `SIDEBAR_HOVER_GRACE_MS`); a pointer over a page is quicker to place,
/// so the reveal is quicker in both directions.
pub const REVEAL: Reveal = Reveal { edge: 6.0, dwell_ms: 150, retreat_ms: 250 };

/// One action in the panel's footer. `label` is drawn beside the icon
/// while the panel is wide enough for it; `min_label_width` is that width.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct FooterAction {
    /// What a press means, as `SideClick`/the page's own panels read it.
    pub id: String,
    /// A lucide name, as the menus name theirs.
    pub icon: String,
    pub label: Option<String>,
    pub tip: String,
    /// Off for what the desktop has and the browser has not; the tip says
    /// so, and a press does nothing.
    pub enabled: bool,
    /// The desktop keeps its settings row at the left inset and its other
    /// actions against the right one.
    pub trailing: bool,
}

/// Below this the label cannot sit beside the gear and the trailing
/// actions, and the footer shows the gear alone -- what `sidebar.rs`'s
/// `settings_label_fits` decides on the desktop.
pub const FOOTER_LABEL_MIN_WIDTH: f64 = 168.0;

pub const MIN_WIDTH: f64 = 127.0;
pub const MAX_WIDTH: f64 = 260.0;
pub const DEFAULT_WIDTH: f64 = 220.0;
