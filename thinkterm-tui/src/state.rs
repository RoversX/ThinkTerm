use crate::action::{Action, DestructiveAction};
use crate::model::TreeNodeKey;
use mux::pane::{PaneId, SearchResult};
use std::collections::{BTreeSet, HashMap};
use std::time::{Duration, Instant};
use wezterm_term::StableRowIndex;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AppMode {
    #[default]
    Terminal,
    Prefix,
    Navigate,
    Resize,
    Copy,
    Search,
    ContextMenu,
    Prompt,
    Confirm,
    Help,
    Connections,
    Settings,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ViewClass {
    #[default]
    Desktop,
    Compact,
    Narrow,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SelectionPoint {
    pub row: StableRowIndex,
    pub col: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextSelection {
    pub pane_id: PaneId,
    pub anchor: SelectionPoint,
    pub head: SelectionPoint,
    pub finalized: bool,
}

impl TextSelection {
    pub fn ordered(&self) -> (SelectionPoint, SelectionPoint) {
        if (self.anchor.row, self.anchor.col) <= (self.head.row, self.head.col) {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct SearchState {
    pub query: String,
    pub matches: Vec<SearchResult>,
    pub current: Option<usize>,
}

#[derive(Clone, Debug)]
pub struct CopyState {
    pub pane_id: PaneId,
    pub cursor: SelectionPoint,
    pub selection: Option<TextSelection>,
    pub search: SearchState,
}

#[derive(Clone, Debug)]
pub struct PromptState {
    pub title: String,
    pub value: String,
    pub action: PromptAction,
}

#[derive(Clone, Debug)]
pub enum PromptAction {
    RenameNode(TreeNodeKey),
    RenameTab,
    CreateSpace {
        domain_name: String,
    },
    CreateProject {
        domain_name: String,
        space_id: String,
    },
    CreateThread {
        domain_name: String,
        project_id: String,
    },
    Search {
        backwards: bool,
    },
}

#[derive(Clone, Debug)]
pub struct ConfirmationState {
    pub title: String,
    pub detail: String,
    pub action: DestructiveAction,
}

#[derive(Clone, Debug)]
pub struct MenuEntry {
    pub label: String,
    pub action: Action,
    pub destructive: bool,
    pub enabled: bool,
}

#[derive(Clone, Debug)]
pub struct ContextMenuState {
    pub anchor: (u16, u16),
    pub selected: usize,
    pub entries: Vec<MenuEntry>,
}

#[derive(Clone, Debug)]
pub struct PendingOperation {
    pub label: String,
    pub domain_name: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ConnectionStatus {
    #[default]
    Disconnected,
    Connecting,
    Attached,
    Reconnecting,
    Unsupported,
    Failed,
}

#[derive(Clone, Debug)]
pub struct ConnectionItem {
    pub name: String,
    pub label: String,
    pub detail: String,
    pub status: ConnectionStatus,
    pub connectable: bool,
}

#[derive(Clone, Debug)]
pub struct Toast {
    pub message: String,
    pub expires_at: Instant,
}

#[derive(Default)]
pub struct UiState {
    pub mode: AppMode,
    pub sidebar_visible: bool,
    pub sidebar_width: u16,
    /// Mirrors `TuiConfig::narrow_width` so layout can read it without the
    /// whole settings struct.
    pub narrow_width: u16,
    /// Mirrors `TuiConfig::touch_targets`, still undecided until layout knows
    /// how wide the screen turned out to be.
    pub touch_targets: Option<bool>,
    /// Mirrors `TuiConfig::pane_scrollbars`.
    pub pane_scrollbars: bool,
    /// Mirrors `TuiConfig::pane_borders`.
    pub pane_borders: bool,
    /// Mirrors `TuiConfig::pane_nav_bar`.
    pub pane_nav_bar: bool,
    pub sidebar_scroll: usize,
    pub tree_collapsed: BTreeSet<TreeNodeKey>,
    /// Lines above the physical screen for each pane. Zero follows output.
    pub scroll_offsets: HashMap<PaneId, usize>,
    pub selection: Option<TextSelection>,
    pub copy: Option<CopyState>,
    pub prompt: Option<PromptState>,
    pub confirmation: Option<ConfirmationState>,
    pub context_menu: Option<ContextMenuState>,
    pub pending: Option<PendingOperation>,
    pub connections: Vec<ConnectionItem>,
    pub connection_index: usize,
    pub settings_index: usize,
    pub status: String,
    pub toast: Option<Toast>,
    pub exit: bool,
    pub frame_number: u64,
}

impl UiState {
    pub fn new(status: String) -> Self {
        Self {
            sidebar_visible: true,
            sidebar_width: 26,
            narrow_width: 64,
            touch_targets: None,
            pane_scrollbars: false,
            pane_borders: true,
            pane_nav_bar: true,
            status,
            ..Default::default()
        }
    }

    pub fn begin_frame(&mut self) {
        self.frame_number = self.frame_number.wrapping_add(1);
    }

    pub fn set_toast(&mut self, message: impl Into<String>) {
        self.toast = Some(Toast {
            message: message.into(),
            expires_at: Instant::now() + Duration::from_secs(3),
        });
    }

    pub fn expire_toast(&mut self, now: Instant) -> bool {
        if self
            .toast
            .as_ref()
            .is_some_and(|toast| toast.expires_at <= now)
        {
            self.toast = None;
            true
        } else {
            false
        }
    }

    pub fn toast_timeout(&self, now: Instant) -> Option<Duration> {
        self.toast
            .as_ref()
            .map(|toast| toast.expires_at.saturating_duration_since(now))
    }

    pub fn scroll_offset(&self, pane_id: PaneId) -> usize {
        self.scroll_offsets.get(&pane_id).copied().unwrap_or(0)
    }

    pub fn set_scroll_offset(&mut self, pane_id: PaneId, offset: usize) {
        if offset == 0 {
            self.scroll_offsets.remove(&pane_id);
        } else {
            self.scroll_offsets.insert(pane_id, offset);
        }
    }

    pub fn close_overlay(&mut self) {
        self.mode = AppMode::Terminal;
        self.prompt = None;
        self.confirmation = None;
        self.context_menu = None;
        self.copy = None;
    }

    pub fn toggle_collapsed(&mut self, key: TreeNodeKey) {
        if !self.tree_collapsed.remove(&key) {
            self.tree_collapsed.insert(key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toast_expiry_is_time_based_even_without_rendered_frames() {
        let now = Instant::now();
        let mut ui = UiState::new(String::new());
        ui.toast = Some(Toast {
            message: "done".into(),
            expires_at: now + Duration::from_secs(3),
        });

        assert_eq!(ui.toast_timeout(now), Some(Duration::from_secs(3)));
        assert!(!ui.expire_toast(now + Duration::from_secs(2)));
        assert!(ui.expire_toast(now + Duration::from_secs(3)));
        assert!(ui.toast.is_none());
    }
}
