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

/// A pane in a stack, for the stack's row of capsules.
#[derive(Debug, Clone, PartialEq)]
pub struct StackMember {
    pub pane_id: PaneId,
    pub title: String,
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
    /// Every pane sharing this frame, the drawn one included, in order.
    pub stack: Vec<StackMember>,
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

impl TabLayout {
    /// What the desktop would claim for this layout: every drawn pane in
    /// the frame the split tree gives it, with `nav_rows` of the frame
    /// kept for the bar above the pane, as the desktop keeps them for
    /// its own (`ClientViewport::Native`). With the bar the desktop's
    /// height this is the layout the desktop already has, and claiming
    /// it resizes nothing; a `CellGrid` claim would grow each pane to
    /// its frame.
    pub fn viewport(&self, nav_rows: usize) -> Vec<codec::ClientPaneViewport> {
        let cell_w = self.size.pixel_width.checked_div(self.cols).unwrap_or(0);
        let cell_h = self.size.pixel_height.checked_div(self.rows).unwrap_or(0);
        self.panes
            .iter()
            .map(|p| codec::ClientPaneViewport {
                pane_id: p.pane_id,
                size: TerminalSize {
                    rows: p.frame.rows.saturating_sub(nav_rows).max(1),
                    cols: p.frame.cols,
                    pixel_width: p.frame.cols * cell_w,
                    pixel_height: p.frame.rows.saturating_sub(nav_rows).max(1) * cell_h,
                    dpi: self.size.dpi,
                },
                frame: TerminalSize {
                    rows: p.frame.rows,
                    cols: p.frame.cols,
                    pixel_width: p.frame.cols * cell_w,
                    pixel_height: p.frame.rows * cell_h,
                    dpi: self.size.dpi,
                },
            })
            .collect()
    }
}

/// The layout with divider `idx` moved `cells` (right/down positive):
/// the panes ending at it along its extent grow, the ones starting past
/// it shrink, and the move is clamped so each keeps `nav_rows` + 1 rows
/// (or one column). `cells` is a displacement from `layout`, so zero
/// (asked, or clamped to) is `layout` itself: a drag back to where it
/// started has to restore what it started from. `None` for a divider
/// that is not there or has nothing on one side.
pub fn with_divider_moved(layout: &TabLayout, idx: usize, cells: isize, nav_rows: usize) -> Option<TabLayout> {
    let divider = *layout.dividers.get(idx)?;
    let mut moved = layout.clone();
    let (before, after): (Vec<usize>, Vec<usize>) = match divider {
        Divider::Col { col, top, rows } => {
            let spans = |p: &PanePlacement| p.frame.top < top + rows && p.frame.top + p.frame.rows > top;
            let b = moved.panes.iter().enumerate().filter(|(_, p)| spans(p) && p.frame.left + p.frame.cols == col).map(|(i, _)| i).collect();
            let a = moved.panes.iter().enumerate().filter(|(_, p)| spans(p) && p.frame.left == col + 1).map(|(i, _)| i).collect();
            (b, a)
        }
        Divider::Row { row, left, cols } => {
            let spans = |p: &PanePlacement| p.frame.left < left + cols && p.frame.left + p.frame.cols > left;
            let b = moved.panes.iter().enumerate().filter(|(_, p)| spans(p) && p.frame.top + p.frame.rows == row).map(|(i, _)| i).collect();
            let a = moved.panes.iter().enumerate().filter(|(_, p)| spans(p) && p.frame.top == row + 1).map(|(i, _)| i).collect();
            (b, a)
        }
    };
    if before.is_empty() || after.is_empty() {
        return None;
    }
    let horizontal = matches!(divider, Divider::Col { .. });
    let min = if horizontal { 1 } else { nav_rows + 1 };
    let extent = |p: &PanePlacement| if horizontal { p.frame.cols } else { p.frame.rows } as isize;
    // Room to shrink on whichever side loses cells.
    let room = |side: &[usize]| side.iter().map(|&i| extent(&moved.panes[i]) - min as isize).min().unwrap_or(0).max(0);
    let cells = if cells > 0 { cells.min(room(&after)) } else { cells.max(-room(&before)) };
    if cells == 0 {
        return Some(moved);
    }
    for &i in &before {
        let f = &mut moved.panes[i].frame;
        if horizontal { f.cols = (f.cols as isize + cells) as usize } else { f.rows = (f.rows as isize + cells) as usize }
    }
    for &i in &after {
        let f = &mut moved.panes[i].frame;
        if horizontal {
            f.left = (f.left as isize + cells) as usize;
            f.cols = (f.cols as isize - cells) as usize;
        } else {
            f.top = (f.top as isize + cells) as usize;
            f.rows = (f.rows as isize - cells) as usize;
        }
    }
    match &mut moved.dividers[idx] {
        Divider::Col { col, .. } => *col = (*col as isize + cells) as usize,
        Divider::Row { row, .. } => *row = (*row as isize + cells) as usize,
    }
    Some(moved)
}

/// The layout with every frame scaled to `size`'s grid, the dividers
/// kept to one cell: pane edges next to a divider are put at the
/// divider's new place, so the frames still compose to the grid as the
/// server requires. Near enough to claim pane by pane. `None` when a
/// pane would be left without a cell: padding one out would break the
/// composition, and the server would refuse the lot.
pub fn scaled(layout: &TabLayout, size: TerminalSize) -> Option<TabLayout> {
    let (cols, rows) = (size.cols, size.rows);
    let sx = |c: usize| (c * cols + layout.cols / 2) / layout.cols;
    let sy = |r: usize| (r * rows + layout.rows / 2) / layout.rows;
    let mut scaled = layout.clone();
    scaled.cols = cols;
    scaled.rows = rows;
    scaled.size = size;
    for p in scaled.panes.iter_mut() {
        let old = p.frame;
        let (mut l, mut t) = (sx(old.left), sy(old.top));
        let (mut r, mut b) = (sx(old.left + old.cols), sy(old.top + old.rows));
        // Edges on a divider follow it: the gap stays one cell.
        for d in &layout.dividers {
            match *d {
                Divider::Col { col, top, rows: n } if old.top < top + n && old.top + old.rows > top => {
                    if old.left + old.cols == col {
                        r = sx(col);
                    }
                    if old.left == col + 1 {
                        l = sx(col) + 1;
                    }
                }
                Divider::Row { row, left, cols: n } if old.left < left + n && old.left + old.cols > left => {
                    if old.top + old.rows == row {
                        b = sy(row);
                    }
                    if old.top == row + 1 {
                        t = sy(row) + 1;
                    }
                }
                _ => {}
            }
        }
        if r <= l || b <= t {
            return None;
        }
        p.frame = Rect { left: l, top: t, cols: r - l, rows: b - t };
    }
    for d in scaled.dividers.iter_mut() {
        match d {
            Divider::Col { col, top, rows: n } => {
                let (t, b) = (sy(*top), sy(*top + *n));
                *col = sx(*col);
                *top = t;
                *n = b.saturating_sub(t).max(1);
            }
            Divider::Row { row, left, cols: n } => {
                let (l, r) = (sx(*left), sx(*left + *n));
                *row = sy(*row);
                *left = l;
                *n = r.saturating_sub(l).max(1);
            }
        }
    }
    Some(scaled)
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

fn placement(entry: &PaneEntry, frame: Rect, stack: Vec<StackMember>) -> PanePlacement {
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
        stack,
    }
}

fn member(entry: &PaneEntry) -> StackMember {
    StackMember { pane_id: entry.pane_id, title: entry.title.clone() }
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
        PaneNode::Leaf(entry) => panes.push(placement(entry, frame, vec![member(entry)])),
        PaneNode::Stack(stack) => {
            let active = stack.active.min(stack.panes.len().saturating_sub(1));
            let members: Vec<StackMember> = stack.panes.iter().map(member).collect();
            for (i, entry) in stack.panes.iter().enumerate() {
                if i == active {
                    panes.push(placement(entry, frame, members.clone()));
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
    layout_in(node, None, 0)
}

/// Lay a tab out, knowing the tab's own size when the server has said
/// it. A split's sizes are its frames and compose to the tab; a lone
/// pane (or stack) only lists the pane's grid, which is the frame less
/// the bar above it. Without the server's word the frame is taken to be
/// that grid plus `nav_rows`, as whoever laid it out with a bar would
/// have left it -- never the grid itself, since claiming a frame the
/// size of the pane would shrink the pane by the bar's rows each time.
pub fn layout_in(node: &PaneNode, tab_size: Option<TerminalSize>, nav_rows: usize) -> Option<TabLayout> {
    let listed = node.root_size()?;
    let framed = |grid: TerminalSize| {
        tab_size.unwrap_or_else(|| {
            let rows = grid.rows + nav_rows;
            TerminalSize {
                rows,
                pixel_height: grid.pixel_height.checked_div(grid.rows).unwrap_or(0) * rows,
                ..grid
            }
        })
    };
    // A zoomed pane's own size is the tab's: the split tree it hides is
    // not resized while it is zoomed, so the root the server lists is
    // wherever the tab stood at the zoom.
    let zoomed_grid = leaves(node).into_iter().find(|e| e.is_zoomed_pane).map(|e| e.size);
    let size = match (node, zoomed_grid) {
        (PaneNode::Split { .. }, None) => listed,
        (_, Some(grid)) => framed(grid),
        _ => framed(listed),
    };
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
    #[test]
    fn moving_a_divider_shifts_the_panes_on_both_sides_within_bounds() {
        let place = |id: usize, top: usize, rows: usize| PanePlacement {
            pane_id: id, tab_id: 1, window_id: 1,
            frame: Rect { left: 0, top, cols: 80, rows },
            content: (80, rows.saturating_sub(2)), is_active: id == 1, is_zoomed: false,
            title: String::new(), alt_screen: false, physical_top: 0,
            size: TerminalSize { rows: rows.saturating_sub(2), cols: 80, pixel_width: 800, pixel_height: 0, dpi: 96 },
            workspace: "w".into(), stack: vec![],
        };
        let layout = TabLayout {
            tab_id: 1, window_id: 1, workspace: "w".into(), cols: 80, rows: 41,
            size: TerminalSize { rows: 41, cols: 80, pixel_width: 800, pixel_height: 820, dpi: 96 },
            panes: vec![place(1, 0, 20), place(2, 21, 20)],
            dividers: vec![Divider::Row { row: 20, left: 0, cols: 80 }],
            zoomed: None, hidden: vec![],
        };
        let moved = with_divider_moved(&layout, 0, 5, 2).unwrap();
        assert_eq!((moved.panes[0].frame.rows, moved.panes[1].frame.top, moved.panes[1].frame.rows), (25, 26, 15));
        assert!(matches!(moved.dividers[0], Divider::Row { row: 25, .. }));
        // Clamped: the lower pane keeps its bar and one row.
        let moved = with_divider_moved(&layout, 0, 30, 2).unwrap();
        assert_eq!(moved.panes[1].frame.rows, 3);
        // Back at the origin, or pushed against the clamp from the other
        // side: the starting layout, not nothing.
        assert_eq!(with_divider_moved(&layout, 0, 0, 2).unwrap(), layout);
        assert_eq!(with_divider_moved(&layout, 0, -40, 2).unwrap().panes[0].frame.rows, 3);
        assert!(with_divider_moved(&layout, 1, 3, 2).is_none());
    }
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
    fn the_viewport_claims_each_pane_at_its_own_grid_in_its_frame() {
        // The desktop keeps 3 rows of each frame for its nav bar: the
        // listed size is 21 rows in a 24-row frame.
        let mut a = pane(1, 40, 24, 0, 0, false);
        a.size = size(40, 21);
        let mut b = pane(2, 39, 24, 41, 0, true);
        b.size = size(39, 21);
        let node = split(
            SplitDirection::Horizontal,
            size(40, 24),
            size(39, 24),
            PaneNode::Leaf(a),
            PaneNode::Leaf(b),
        );
        let l = layout(&node).unwrap();
        let v = l.viewport(3);
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].size, size(40, 21), "the frame less the bar's rows: the grid the pane has");
        assert_eq!(l.viewport(2)[0].size, size(40, 22), "a shorter bar would give the pane a row");
        assert_eq!(v[0].frame, size(40, 24), "the frame the split gives it");
        assert_eq!(v[1].frame, size(39, 24));
        assert_eq!(v[0].frame.cols + v[1].frame.cols + 1, l.cols, "frames compose to the tab");
        assert_eq!(v[0].frame.pixel_width, 40 * 8, "pixels from the tab's cell");
    }

    #[test]
    fn a_lone_pane_s_frame_is_its_tab_not_its_grid() {
        let node = PaneNode::Leaf(pane(1, 80, 21, 0, 0, true));
        let told = layout_in(&node, Some(size(80, 24)), 3).unwrap();
        assert_eq!((told.rows, told.panes[0].frame.rows, told.panes[0].content.1), (24, 24, 21));
        let guessed = layout_in(&node, None, 3).unwrap();
        assert_eq!(guessed.rows, 24, "the grid plus the bar's rows");
        assert_eq!(guessed.size.pixel_height, 24 * 16);
        assert_eq!(guessed.viewport(3)[0].size, size(80, 21), "so a claim leaves the pane as it is");
        // A split's frames are its own word.
        let split = split(
            SplitDirection::Horizontal,
            size(40, 24),
            size(39, 24),
            PaneNode::Leaf(pane(1, 40, 21, 0, 0, false)),
            PaneNode::Leaf(pane(2, 39, 21, 41, 0, true)),
        );
        assert_eq!(layout_in(&split, Some(size(80, 30)), 3).unwrap().rows, 24);
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

    /// The server's tree keeps the pre-zoom sizes while a pane is zoomed;
    /// the zoomed pane's own size, resized since, is the tab's.
    #[test]
    fn a_zoomed_pane_resized_since_the_zoom_sizes_the_tab() {
        let mut zoomed = pane(2, 165, 22, 41, 0, true);
        zoomed.is_zoomed_pane = true;
        let node = split(
            SplitDirection::Horizontal,
            size(70, 36),
            size(39, 36),
            PaneNode::Leaf(pane(1, 70, 36, 0, 0, false)),
            PaneNode::Leaf(zoomed),
        );
        let l = layout_in(&node, None, 3).unwrap();
        assert_eq!((l.cols, l.rows), (165, 25));
        assert_eq!(l.panes[0].frame, Rect { left: 0, top: 0, cols: 165, rows: 25 });
        // With the server's word for the tab, that.
        let l = layout_in(&node, Some(size(165, 25)), 3).unwrap();
        assert_eq!((l.cols, l.rows), (165, 25));
    }

    /// Doubling an 80-column tab split 40 | 39 gives 80 | 79 with the
    /// divider still one cell, not 80 | 78 with a two-cell gap the
    /// server would refuse.
    #[test]
    fn scaling_keeps_dividers_one_cell_wide() {
        let mut zoomed = pane(2, 39, 24, 41, 0, true);
        zoomed.is_zoomed_pane = false;
        let node = split(
            SplitDirection::Horizontal,
            size(40, 24),
            size(39, 24),
            PaneNode::Leaf(pane(1, 40, 24, 0, 0, false)),
            PaneNode::Leaf(zoomed),
        );
        let l = layout(&node).unwrap();
        let big = scaled(&l, size(160, 24)).unwrap();
        let frames: Vec<(usize, usize)> = big.panes.iter().map(|p| (p.frame.left, p.frame.cols)).collect();
        assert_eq!(frames, vec![(0, 80), (81, 79)]);
        assert!(matches!(big.dividers[0], Divider::Col { col: 80, .. }));
        // And back down, still composing.
        let small = scaled(&big, size(60, 24)).unwrap();
        let frames: Vec<(usize, usize)> = small.panes.iter().map(|p| (p.frame.left, p.frame.cols)).collect();
        assert_eq!(frames[0].0 + frames[0].1 + 1, frames[1].0);
        assert_eq!(frames[1].0 + frames[1].1, 60);
        // A one-column pane has nowhere to go at 3 columns: no layout,
        // rather than one a column too wide.
        assert!(scaled(&l, size(3, 24)).is_none());
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
