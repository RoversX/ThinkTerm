use crate::split::SplitDirectionAndSize;
use crate::{TabId, WindowId};
use serde::{Deserialize, Serialize};
use url::Url;
use wezterm_term::{StableRowIndex, TerminalSize};

use crate::renderable::StableCursorPosition;

/// This type is used directly by the codec, take care to bump
/// the codec version if you change this
#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub enum PaneNode {
    Empty,
    Split {
        left: Box<PaneNode>,
        right: Box<PaneNode>,
        node: SplitDirectionAndSize,
    },
    Leaf(PaneEntry),
    Stack(PaneStackEntry),
}

impl PaneNode {
    pub fn into_tree(self) -> bintree::Tree<PaneStackEntry, SplitDirectionAndSize> {
        match self {
            PaneNode::Empty => bintree::Tree::Empty,
            PaneNode::Split { left, right, node } => bintree::Tree::Node {
                left: Box::new((*left).into_tree()),
                right: Box::new((*right).into_tree()),
                data: Some(node),
            },
            PaneNode::Leaf(e) => bintree::Tree::Leaf(PaneStackEntry {
                active: 0,
                panes: vec![e],
                pane_stack_id: None,
            }),
            PaneNode::Stack(stack) => bintree::Tree::Leaf(stack),
        }
    }

    pub fn root_size(&self) -> Option<TerminalSize> {
        match self {
            PaneNode::Empty => None,
            PaneNode::Split { node, .. } => Some(node.size()),
            PaneNode::Leaf(entry) => Some(entry.size),
            PaneNode::Stack(stack) => stack
                .panes
                .get(stack.active)
                .or_else(|| stack.panes.first())
                .map(|entry| entry.size),
        }
    }

    pub fn window_and_tab_ids(&self) -> Option<(WindowId, TabId)> {
        match self {
            PaneNode::Empty => None,
            PaneNode::Split { left, right, .. } => match left.window_and_tab_ids() {
                Some(res) => Some(res),
                None => right.window_and_tab_ids(),
            },
            PaneNode::Leaf(entry) => Some((entry.window_id, entry.tab_id)),
            PaneNode::Stack(stack) => stack
                .panes
                .get(stack.active)
                .or_else(|| stack.panes.first())
                .map(|entry| (entry.window_id, entry.tab_id)),
        }
    }
}

/// This type is used directly by the codec, take care to bump
/// the codec version if you change this
#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub struct PaneStackEntry {
    pub active: usize,
    pub panes: Vec<PaneEntry>,
    /// Stable identity of the stack on the side that owns it (the mux
    /// server). Clients translate this to a stable local id so that GUI
    /// state keyed by stack id survives resyncs. Optional for backwards
    /// compatibility with older layout snapshots.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane_stack_id: Option<usize>,
}

/// This type is used directly by the codec, take care to bump
/// the codec version if you change this
#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub struct PaneEntry {
    pub window_id: WindowId,
    pub tab_id: TabId,
    pub pane_id: crate::PaneId,
    pub title: String,
    pub size: TerminalSize,
    pub working_dir: Option<SerdeUrl>,
    pub is_active_pane: bool,
    pub is_zoomed_pane: bool,
    /// Whether the pane is showing the alternate screen. Carried here as well
    /// as in render changes so that a renderer knows it the moment it learns
    /// the pane exists, rather than only once something in it next changes.
    ///
    /// Defaulted for the same reason as `PaneStackEntry::pane_stack_id`: this
    /// type is the on-disk format of a Thread's saved layout, and every
    /// snapshot written before the field existed omits it. Without a default,
    /// adding it made all of them fail to decode, which meant every Thread
    /// opened as a single empty pane and then overwrote its own saved layout.
    #[serde(default)]
    pub alt_screen: bool,
    pub workspace: String,
    pub cursor_pos: StableCursorPosition,
    pub physical_top: StableRowIndex,
    pub top_row: usize,
    pub left_col: usize,
    pub tty_name: Option<String>,
}

#[derive(Deserialize, Clone, Serialize, PartialEq, Debug)]
#[serde(try_from = "String", into = "String")]
pub struct SerdeUrl {
    pub url: Url,
}

impl std::convert::TryFrom<String> for SerdeUrl {
    type Error = url::ParseError;
    fn try_from(s: String) -> Result<SerdeUrl, url::ParseError> {
        let url = Url::parse(&s)?;
        Ok(SerdeUrl { url })
    }
}

impl From<Url> for SerdeUrl {
    fn from(url: Url) -> SerdeUrl {
        SerdeUrl { url }
    }
}

impl Into<Url> for SerdeUrl {
    fn into(self) -> Url {
        self.url
    }
}

impl Into<String> for SerdeUrl {
    fn into(self) -> String {
        self.url.as_str().into()
    }
}
