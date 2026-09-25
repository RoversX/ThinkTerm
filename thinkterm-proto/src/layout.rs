use crate::split::SplitDirectionAndSize;
use crate::{TabId, WindowId};
use serde::{Deserialize, Serialize};
use url::Url;
use wezterm_term::{StableRowIndex, TerminalSize};

use crate::renderable::StableCursorPosition;

/// The most cells a pane may span either way. Pane sizes come off the wire
/// and grids, caches, row walks and layouts are built from them; a size past
/// this is a broken or hostile server, not a screen.
pub const MAX_PANE_CELLS: usize = 10_000;

/// The most pixels a pane may span either way: a generous cell at the most
/// cells. Pixel sizes are multiplied and divided in layout, so they are
/// bounded as well.
pub const MAX_PANE_PIXELS: usize = MAX_PANE_CELLS * 1024;

/// Whether a size the server sent is one to build or lay out from.
pub fn terminal_size_is_plausible(size: &TerminalSize) -> bool {
    size.cols <= MAX_PANE_CELLS
        && size.rows <= MAX_PANE_CELLS
        && size.pixel_width <= MAX_PANE_PIXELS
        && size.pixel_height <= MAX_PANE_PIXELS
}

/// Blank every tab whose tree describes a pane or split no screen could be.
/// A tab past the bound is dropped rather than clamped, since a clamped size
/// would disagree with the pane the server actually has; tabs are blanked in
/// place so a list kept beside them (tab titles) still lines up. Returns how
/// many were blanked, for the caller to report.
pub fn blank_implausible_tabs(tabs: &mut [PaneNode]) -> usize {
    let mut blanked = 0;
    for tab in tabs {
        if !tab.is_plausible() {
            *tab = PaneNode::Empty;
            blanked += 1;
        }
    }
    blanked
}

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

    /// Every size in the tree within bounds, splits included, and the two
    /// sides of a split together still a plausible pane. A split's second
    /// side may be empty (a divider dragged to the far edge leaves it no
    /// cells), but its first may not: `SplitDirectionAndSize::size` divides
    /// by it. The whole is checked here without calling `size`, whose
    /// products can overflow a 32-bit `usize` before any bound applies.
    pub fn is_plausible(&self) -> bool {
        match self {
            PaneNode::Empty => true,
            PaneNode::Leaf(entry) => entry.is_plausible(),
            PaneNode::Stack(stack) => stack.panes.iter().all(PaneEntry::is_plausible),
            PaneNode::Split { left, right, node } => {
                let (first, second) = (&node.first, &node.second);
                let sides_ok = terminal_size_is_plausible(first)
                    && terminal_size_is_plausible(second)
                    && first.cols >= 1
                    && first.rows >= 1;
                // Bounded sides, so neither sum below can overflow.
                let whole_ok = || {
                    let (cols, rows) = (node.width(), node.height());
                    let cell_width = first.pixel_width / first.cols;
                    let cell_height = first.pixel_height / first.rows;
                    cols <= MAX_PANE_CELLS
                        && rows <= MAX_PANE_CELLS
                        && cell_width
                            .checked_mul(cols)
                            .is_some_and(|total| total <= MAX_PANE_PIXELS)
                        && cell_height
                            .checked_mul(rows)
                            .is_some_and(|total| total <= MAX_PANE_PIXELS)
                };
                sides_ok && whole_ok() && left.is_plausible() && right.is_plausible()
            }
        }
    }

    /// Every pane in the tree, left to right, stacked ones included.
    pub fn entries(&self) -> Vec<&PaneEntry> {
        match self {
            PaneNode::Empty => vec![],
            PaneNode::Leaf(entry) => vec![entry],
            PaneNode::Stack(stack) => stack.panes.iter().collect(),
            PaneNode::Split { left, right, .. } => {
                let mut all = left.entries();
                all.extend(right.entries());
                all
            }
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

impl PaneEntry {
    /// The listed size within bounds, and its rows reachable from its top.
    pub fn is_plausible(&self) -> bool {
        terminal_size_is_plausible(&self.size)
            && self
                .physical_top
                .checked_add(self.size.rows as StableRowIndex)
                .is_some()
    }
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

#[cfg(test)]
mod test {
    use super::*;
    use crate::renderable::RenderableDimensions;
    use crate::split::SplitDirection;

    fn size(cols: usize, rows: usize) -> TerminalSize {
        TerminalSize {
            rows,
            cols,
            pixel_width: cols * 8,
            pixel_height: rows * 16,
            dpi: 72,
        }
    }

    fn leaf(size: TerminalSize) -> PaneNode {
        PaneNode::Leaf(PaneEntry {
            window_id: 0,
            tab_id: 0,
            pane_id: 0,
            title: String::new(),
            size,
            working_dir: None,
            is_active_pane: true,
            is_zoomed_pane: false,
            alt_screen: false,
            workspace: String::new(),
            cursor_pos: Default::default(),
            physical_top: 0,
            top_row: 0,
            left_col: 0,
            tty_name: None,
        })
    }

    fn split(first: TerminalSize, second: TerminalSize) -> PaneNode {
        PaneNode::Split {
            left: Box::new(leaf(first)),
            right: Box::new(leaf(second)),
            node: SplitDirectionAndSize {
                direction: SplitDirection::Horizontal,
                first,
                second,
            },
        }
    }

    #[test]
    fn ordinary_trees_are_plausible_and_impossible_ones_are_not() {
        assert!(leaf(size(80, 24)).is_plausible());
        assert!(split(size(40, 24), size(39, 24)).is_plausible());
        assert!(!leaf(size(80, MAX_PANE_CELLS + 1)).is_plausible());
        let mut wide_pixels = size(80, 24);
        wide_pixels.pixel_width = usize::MAX;
        assert!(!leaf(wide_pixels).is_plausible());
        // A zero-width first side would divide by zero in `size()`; an empty
        // second one is what a divider dragged to the edge leaves.
        assert!(!split(size(0, 24), size(80, 24)).is_plausible());
        assert!(split(size(79, 24), size(0, 24)).is_plausible());
        // Each pixel size in bounds, the whole not (and on a 32-bit target,
        // not even representable).
        let mut tall_cells = size(1, 24);
        tall_cells.pixel_width = MAX_PANE_PIXELS;
        assert!(!split(tall_cells, size(9_000, 24)).is_plausible());
        // Each side in bounds, the two together not.
        assert!(!split(size(6000, 24), size(6000, 24)).is_plausible());
        let mut tabs = vec![leaf(size(80, 24)), leaf(size(80, 100_000))];
        assert_eq!(blank_implausible_tabs(&mut tabs), 1);
        assert!(matches!(tabs[1], PaneNode::Empty));
    }

    #[test]
    fn dimensions_with_unreachable_rows_are_not_plausible() {
        let dims = RenderableDimensions {
            cols: 80,
            viewport_rows: 24,
            scrollback_rows: 100,
            physical_top: 76,
            scrollback_top: 0,
            dpi: 72,
            pixel_width: 640,
            pixel_height: 384,
            reverse_video: false,
        };
        assert!(dims.is_plausible());
        assert!(!RenderableDimensions {
            scrollback_top: StableRowIndex::MIN,
            ..dims
        }
        .is_plausible());
        assert!(!RenderableDimensions {
            physical_top: StableRowIndex::MAX,
            ..dims
        }
        .is_plausible());
        assert!(!RenderableDimensions {
            scrollback_top: 100,
            ..dims
        }
        .is_plausible());
    }
}
