//! Where the panes of a tab go, from the server's own listing.
//!
//! The desktop lays a tab out from the split tree the server sends; this
//! does the same walk so the page draws the same rectangles. Pure, and
//! tested natively: the only inputs are `PaneNode`s.
//!
//! The rules, from `mux::tab::pane_tree` and `thinkterm_proto::split`:
//! a split's second child starts one cell past the end of the first, and
//! that gap cell is where the divider is drawn; a stack shows only its
//! active member; a zoomed pane is drawn alone at the tab's size, and its
//! listed position is stale.

use codec::ListPanesResponse;
use thinkterm_proto::layout::{PaneEntry, PaneNode};
use thinkterm_proto::split::SplitDirection;
use thinkterm_proto::{PaneId, TabId, WindowId};
use wezterm_term::StableRowIndex;
use wezterm_term::TerminalSize;

/// Cells, from the tab's top-left.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub left: usize,
    pub top: usize,
    pub cols: usize,
    pub rows: usize,
}

impl Rect {
    pub fn contains(&self, col: usize, row: usize) -> bool {
        col >= self.left && col < self.left + self.cols && row >= self.top && row < self.top + self.rows
    }
}

/// One pane to draw.
#[derive(Debug, Clone, PartialEq)]
pub struct PanePlacement {
    pub pane_id: PaneId,
    pub tab_id: TabId,
    pub window_id: WindowId,
    /// The rectangle the split tree gives the pane.
    pub frame: Rect,
    /// The grid the pane's terminal actually has, from `PaneEntry.size`:
    /// on the desktop smaller than the frame by its per-pane chrome.
    pub content: (usize, usize),
    pub is_active: bool,
    pub is_zoomed: bool,
    pub title: String,
    pub alt_screen: bool,
    pub physical_top: StableRowIndex,
    pub size: TerminalSize,
    pub workspace: String,
}

/// The gap cell between two panes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Divider {
    /// A vertical line at `col`, from `top` for `rows` cells.
    Col { col: usize, top: usize, rows: usize },
    /// A horizontal line at `row`, from `left` for `cols` cells.
    Row { row: usize, left: usize, cols: usize },
}

#[derive(Debug, Clone, PartialEq)]
pub struct TabLayout {
    pub tab_id: TabId,
    pub window_id: WindowId,
    pub workspace: String,
    /// The tab's own grid, `PaneNode::root_size()`.
    pub cols: usize,
    pub rows: usize,
    pub size: TerminalSize,
    /// The panes to draw, in tree order.
    pub panes: Vec<PanePlacement>,
    /// Empty while a pane is zoomed.
    pub dividers: Vec<Divider>,
    pub zoomed: Option<PaneId>,
    /// Panes that exist but are not drawn: inactive stack members, and
    /// everything but the zoomed pane while one is.
    pub hidden: Vec<PaneId>,
}

/// Every pane in a tab, left to right, top to bottom, stacks included.
pub fn leaves(node: &PaneNode) -> Vec<&PaneEntry> {
    match node {
        PaneNode::Empty => vec![],
        PaneNode::Leaf(entry) => vec![entry],
        PaneNode::Stack(stack) => stack.panes.iter().collect(),
        PaneNode::Split { left, right, .. } => {
            let mut all = leaves(left);
            all.extend(leaves(right));
            all
        }
    }
}

/// The tab that holds `pane_id`.
pub fn tab_containing(list: &ListPanesResponse, pane_id: PaneId) -> Option<&PaneNode> {
    list.tabs
        .iter()
        .find(|tab| leaves(tab).iter().any(|e| e.pane_id == pane_id))
}

fn placement(entry: &PaneEntry, frame: Rect) -> PanePlacement {
    PanePlacement {
        pane_id: entry.pane_id,
        tab_id: entry.tab_id,
        window_id: entry.window_id,
        frame,
        content: (entry.size.cols, entry.size.rows),
        is_active: entry.is_active_pane,
        is_zoomed: entry.is_zoomed_pane,
        title: entry.title.clone(),
        alt_screen: entry.alt_screen,
        physical_top: entry.physical_top,
        size: entry.size,
        workspace: entry.workspace.clone(),
    }
}

fn walk(
    node: &PaneNode,
    frame: Rect,
    panes: &mut Vec<PanePlacement>,
    dividers: &mut Vec<Divider>,
    hidden: &mut Vec<PaneId>,
) {
    match node {
        PaneNode::Empty => {}
        PaneNode::Leaf(entry) => panes.push(placement(entry, frame)),
        PaneNode::Stack(stack) => {
            let active = stack.active.min(stack.panes.len().saturating_sub(1));
            for (i, entry) in stack.panes.iter().enumerate() {
                if i == active {
                    panes.push(placement(entry, frame));
                } else {
                    hidden.push(entry.pane_id);
                }
            }
        }
        PaneNode::Split { left, right, node } => {
            let first = Rect {
                left: frame.left,
                top: frame.top,
                cols: node.first.cols,
                rows: node.first.rows,
            };
            let second = Rect {
                left: frame.left + node.left_of_second(),
                top: frame.top + node.top_of_second(),
                cols: node.second.cols,
                rows: node.second.rows,
            };
            match node.direction {
                SplitDirection::Horizontal => dividers.push(Divider::Col {
                    col: frame.left + node.first.cols,
                    top: frame.top,
                    rows: node.height(),
                }),
                SplitDirection::Vertical => dividers.push(Divider::Row {
                    row: frame.top + node.first.rows,
                    left: frame.left,
                    cols: node.width(),
                }),
            }
            walk(left, first, panes, dividers, hidden);
            walk(right, second, panes, dividers, hidden);
        }
    }
}

/// Lay a tab out. `None` for an empty tab.
pub fn layout(node: &PaneNode) -> Option<TabLayout> {
    let size = node.root_size()?;
    let (window_id, tab_id) = node.window_and_tab_ids()?;
    let root = Rect {
        left: 0,
        top: 0,
        cols: size.cols,
        rows: size.rows,
    };
    let workspace = leaves(node)
        .first()
        .map(|e| e.workspace.clone())
        .unwrap_or_default();
    let mut panes = Vec::new();
    let mut dividers = Vec::new();
    let mut hidden = Vec::new();
    walk(node, root, &mut panes, &mut dividers, &mut hidden);
    // A zoomed pane fills the tab; the rest are there but not drawn. Its
    // listed position is where it sat before the zoom.
    let zoomed = panes.iter().find(|p| p.is_zoomed).map(|p| p.pane_id);
    if let Some(zoomed_id) = zoomed {
        let mut kept = Vec::new();
        for p in panes.drain(..) {
            if p.pane_id == zoomed_id {
                kept.push(PanePlacement { frame: root, ..p });
            } else {
                hidden.push(p.pane_id);
            }
        }
        panes = kept;
        dividers.clear();
    }
    Some(TabLayout {
        tab_id,
        window_id,
        workspace,
        cols: size.cols,
        rows: size.rows,
        size,
        panes,
        dividers,
        zoomed,
        hidden,
    })
}

/// The pane under a cell of the tab, if any: divider cells and the space
/// past the tab belong to nobody.
pub fn hit(layout: &TabLayout, col: usize, row: usize) -> Option<&PanePlacement> {
    layout.panes.iter().find(|p| p.frame.contains(col, row))
}

#[cfg(test)]
mod tests {
    use super::*;
    use thinkterm_proto::layout::PaneStackEntry;
    use thinkterm_proto::split::SplitDirectionAndSize;

    fn size(cols: usize, rows: usize) -> TerminalSize {
        TerminalSize {
            cols,
            rows,
            pixel_width: cols * 8,
            pixel_height: rows * 16,
            dpi: 96,
        }
    }

    fn pane(id: PaneId, cols: usize, rows: usize, left: usize, top: usize, active: bool) -> PaneEntry {
        PaneEntry {
            window_id: 0,
            tab_id: 1,
            pane_id: id,
            title: format!("pane {id}"),
            size: size(cols, rows),
            working_dir: None,
            is_active_pane: active,
            is_zoomed_pane: false,
            alt_screen: false,
            workspace: "default".into(),
            cursor_pos: Default::default(),
            physical_top: 0,
            top_row: top,
            left_col: left,
            tty_name: None,
        }
    }

    fn split(direction: SplitDirection, first: TerminalSize, second: TerminalSize, l: PaneNode, r: PaneNode) -> PaneNode {
        PaneNode::Split {
            left: Box::new(l),
            right: Box::new(r),
            node: SplitDirectionAndSize { direction, first, second },
        }
    }

    /// The server's own arithmetic: the listing's `left_col/top_row` and
    /// the walk from the split sizes must agree on every leaf.
    fn assert_positions_agree(node: &PaneNode, layout: &TabLayout) {
        for entry in leaves(node) {
            if layout.hidden.contains(&entry.pane_id) {
                continue;
            }
            let p = layout.panes.iter().find(|p| p.pane_id == entry.pane_id).unwrap();
            assert_eq!((p.frame.left, p.frame.top), (entry.left_col, entry.top_row), "pane {}", entry.pane_id);
        }
    }

    #[test]
    fn a_lone_pane_fills_the_tab() {
        let node = PaneNode::Leaf(pane(1, 80, 24, 0, 0, true));
        let l = layout(&node).unwrap();
        assert_eq!((l.cols, l.rows), (80, 24));
        assert_eq!(l.panes.len(), 1);
        assert_eq!(l.panes[0].frame, Rect { left: 0, top: 0, cols: 80, rows: 24 });
        assert!(l.dividers.is_empty());
        assert_positions_agree(&node, &l);
    }

    #[test]
    fn a_side_by_side_split_leaves_one_gap_cell_for_the_divider() {
        // 40 | 39 = 80 cols, the divider on column 40.
        let node = split(
            SplitDirection::Horizontal,
            size(40, 24),
            size(39, 24),
            PaneNode::Leaf(pane(1, 40, 24, 0, 0, false)),
            PaneNode::Leaf(pane(2, 39, 24, 41, 0, true)),
        );
        let l = layout(&node).unwrap();
        assert_eq!((l.cols, l.rows), (80, 24), "first + second + 1");
        assert_eq!(l.panes[1].frame.left, l.panes[0].frame.cols + 1);
        assert_eq!(l.dividers, vec![Divider::Col { col: 40, top: 0, rows: 24 }]);
        assert_positions_agree(&node, &l);
        assert_eq!(hit(&l, 39, 5).map(|p| p.pane_id), Some(1));
        assert_eq!(hit(&l, 40, 5), None, "the divider belongs to nobody");
        assert_eq!(hit(&l, 41, 5).map(|p| p.pane_id), Some(2));
        assert_eq!(hit(&l, 80, 5), None, "past the tab");
    }

    #[test]
    fn a_stacked_split_and_a_nested_one() {
        // top: 80x10; bottom: (30 | 49) x 13; total 80x24 with two dividers.
        let bottom = split(
            SplitDirection::Horizontal,
            size(30, 13),
            size(49, 13),
            PaneNode::Leaf(pane(2, 30, 13, 0, 11, false)),
            PaneNode::Leaf(pane(3, 49, 13, 31, 11, true)),
        );
        let node = split(
            SplitDirection::Vertical,
            size(80, 10),
            size(80, 13),
            PaneNode::Leaf(pane(1, 80, 10, 0, 0, false)),
            bottom,
        );
        let l = layout(&node).unwrap();
        assert_eq!((l.cols, l.rows), (80, 24));
        assert_eq!(l.panes.iter().map(|p| p.pane_id).collect::<Vec<_>>(), vec![1, 2, 3]);
        assert_eq!(
            l.dividers,
            vec![
                Divider::Row { row: 10, left: 0, cols: 80 },
                Divider::Col { col: 30, top: 11, rows: 13 },
            ]
        );
        assert_positions_agree(&node, &l);
        assert_eq!(hit(&l, 50, 20).map(|p| p.pane_id), Some(3));
        assert_eq!(hit(&l, 50, 10), None);
    }

    #[test]
    fn a_stack_shows_only_its_active_member() {
        let node = PaneNode::Stack(PaneStackEntry {
            active: 1,
            panes: vec![pane(1, 80, 24, 0, 0, false), pane(2, 80, 24, 0, 0, true)],
            pane_stack_id: None,
        });
        let l = layout(&node).unwrap();
        assert_eq!(l.panes.iter().map(|p| p.pane_id).collect::<Vec<_>>(), vec![2]);
        assert_eq!(l.hidden, vec![1]);
    }

    #[test]
    fn a_zoomed_pane_is_drawn_alone_at_the_tab_size() {
        let mut zoomed = pane(2, 80, 24, 41, 0, true);
        zoomed.is_zoomed_pane = true;
        let node = split(
            SplitDirection::Horizontal,
            size(40, 24),
            size(39, 24),
            PaneNode::Leaf(pane(1, 40, 24, 0, 0, false)),
            PaneNode::Leaf(zoomed),
        );
        let l = layout(&node).unwrap();
        assert_eq!(l.zoomed, Some(2));
        assert_eq!(l.panes.len(), 1);
        assert_eq!(l.panes[0].frame, Rect { left: 0, top: 0, cols: 80, rows: 24 });
        assert!(l.dividers.is_empty());
        assert_eq!(l.hidden, vec![1]);
    }

    #[test]
    fn content_can_be_smaller_than_the_frame() {
        // The desktop reserves a nav bar row: the pane's grid is 23 rows in
        // a 24-row frame.
        let node = PaneNode::Leaf(pane(1, 80, 23, 0, 0, true));
        let mut l = layout(&node).unwrap();
        l.panes[0].frame.rows = 24;
        assert_eq!(l.panes[0].content, (80, 23));
    }

    #[test]
    fn the_tab_holding_a_pane_is_found() {
        let list = ListPanesResponse {
            tabs: vec![
                PaneNode::Leaf(pane(1, 80, 24, 0, 0, true)),
                PaneNode::Leaf(pane(7, 80, 24, 0, 0, true)),
            ],
            tab_titles: vec![],
            window_titles: Default::default(),
        };
        assert!(matches!(tab_containing(&list, 7), Some(PaneNode::Leaf(e)) if e.pane_id == 7));
        assert!(tab_containing(&list, 9).is_none());
    }
}
