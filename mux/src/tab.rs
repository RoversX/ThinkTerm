use crate::domain::DomainId;
use crate::pane::*;
use crate::renderable::StableCursorPosition;
use crate::{Mux, MuxNotification, WindowId};
use bintree::PathBranch;
use config::configuration;
use config::keyassignment::PaneDirection;
use parking_lot::Mutex;
use rangeset::intersects_range;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::convert::TryInto;
use std::sync::Arc;
use url::Url;
use wezterm_term::{StableRowIndex, TerminalSize};

pub type PaneStackId = usize;
pub type Tree = bintree::Tree<PaneStack, SplitDirectionAndSize>;
pub type Cursor = bintree::Cursor<PaneStack, SplitDirectionAndSize>;

static TAB_ID: ::std::sync::atomic::AtomicUsize = ::std::sync::atomic::AtomicUsize::new(0);
static PANE_STACK_ID: ::std::sync::atomic::AtomicUsize = ::std::sync::atomic::AtomicUsize::new(0);

/// Allocate a fresh local pane-stack id. Used by mux clients to mint stable
/// local ids for stacks arriving from a remote server (whose ids live in a
/// different id space and must not collide with locally-created stacks).
pub fn alloc_pane_stack_id() -> PaneStackId {
    PANE_STACK_ID.fetch_add(1, ::std::sync::atomic::Ordering::Relaxed)
}
pub type TabId = usize;

#[derive(Default)]
struct Recency {
    count: usize,
    by_idx: HashMap<usize, usize>,
}

impl Recency {
    fn tag(&mut self, idx: usize) {
        self.by_idx.insert(idx, self.count);
        self.count += 1;
    }

    fn score(&self, idx: usize) -> usize {
        self.by_idx.get(&idx).copied().unwrap_or(0)
    }
}

#[derive(Clone)]
pub struct PaneStack {
    id: PaneStackId,
    panes: Vec<Arc<dyn Pane>>,
    active: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaneStackTab {
    pub pane_id: PaneId,
    pub title: String,
    pub is_active: bool,
}

impl PaneStack {
    fn new(pane: Arc<dyn Pane>) -> Self {
        Self::from_panes(vec![pane], 0)
    }

    fn from_panes(panes: Vec<Arc<dyn Pane>>, active: usize) -> Self {
        Self::from_panes_with_id(panes, active, None)
    }

    /// Like `from_panes`, but reuses a previously-assigned stack id when one
    /// is known (rebuilding a tab from a remote pane tree). Keeping the id
    /// stable across rebuilds is what lets GUI state keyed by stack id
    /// (collapse layouts, level-2 tab bar scroll) survive resyncs.
    fn from_panes_with_id(
        panes: Vec<Arc<dyn Pane>>,
        active: usize,
        id: Option<PaneStackId>,
    ) -> Self {
        Self {
            id: id.unwrap_or_else(|| {
                PANE_STACK_ID.fetch_add(1, ::std::sync::atomic::Ordering::Relaxed)
            }),
            active: active.min(panes.len().saturating_sub(1)),
            panes,
        }
    }

    pub fn id(&self) -> PaneStackId {
        self.id
    }

    fn len(&self) -> usize {
        self.panes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.panes.is_empty()
    }

    fn active_index(&self) -> usize {
        self.active.min(self.panes.len().saturating_sub(1))
    }

    fn active_pane(&self) -> Option<Arc<dyn Pane>> {
        self.panes.get(self.active_index()).map(Arc::clone)
    }

    fn contains_pane(&self, pane_id: PaneId) -> bool {
        self.panes.iter().any(|pane| pane.pane_id() == pane_id)
    }

    fn pane_index(&self, pane_id: PaneId) -> Option<usize> {
        self.panes.iter().position(|pane| pane.pane_id() == pane_id)
    }

    fn set_active_pane(&mut self, pane_id: PaneId) -> bool {
        match self.pane_index(pane_id) {
            Some(index) => {
                self.active = index;
                true
            }
            None => false,
        }
    }

    fn push_and_activate(&mut self, pane: Arc<dyn Pane>) {
        self.panes.push(pane);
        self.active = self.panes.len().saturating_sub(1);
    }

    fn resize(&self, size: TerminalSize) -> anyhow::Result<()> {
        for pane in &self.panes {
            // Remote mirror panes are sized by the GUI layer, which
            // subtracts per-pane chrome (the pane nav bar) from the cell
            // size; forcing them to the raw cell size here would undo that
            // and bounce Resize PDUs back and forth with the server.
            if pane.is_remote_mirror() {
                continue;
            }
            pane.resize(size)?;
        }
        Ok(())
    }

    fn tabs(&self) -> Vec<PaneStackTab> {
        let active = self.active_index();
        self.panes
            .iter()
            .enumerate()
            .map(|(idx, pane)| PaneStackTab {
                pane_id: pane.pane_id(),
                title: pane.get_title(),
                is_active: idx == active,
            })
            .collect()
    }

    fn remove_matching<F>(
        &mut self,
        pane_index: usize,
        f: &F,
        zoomed_pane: Option<PaneId>,
    ) -> (Vec<Arc<dyn Pane>>, bool)
    where
        F: Fn(usize, &Arc<dyn Pane>) -> bool,
    {
        let mut removed = vec![];
        let mut idx = 0;
        let active = self.active_index();
        let mut removed_before_active = 0;
        let mut removed_active = false;

        self.panes.retain(|pane| {
            let should_remove = f(pane_index, pane);
            if should_remove {
                if idx < active {
                    removed_before_active += 1;
                }
                if idx == active || Some(pane.pane_id()) == zoomed_pane {
                    removed_active = true;
                }
                removed.push(Arc::clone(pane));
            }
            idx += 1;
            !should_remove
        });

        if self.panes.is_empty() {
            self.active = 0;
        } else if removed_active {
            self.active = self
                .active_index()
                .saturating_sub(1)
                .min(self.panes.len().saturating_sub(1));
        } else {
            self.active = active
                .saturating_sub(removed_before_active)
                .min(self.panes.len().saturating_sub(1));
        }

        let became_empty = self.panes.is_empty();
        (removed, became_empty)
    }
}

struct TabInner {
    id: TabId,
    pane: Option<Tree>,
    size: TerminalSize,
    size_before_zoom: TerminalSize,
    active: usize,
    zoomed: Option<Arc<dyn Pane>>,
    title: String,
    recency: Recency,
}

/// A Tab is a container of Panes
pub struct Tab {
    inner: Mutex<TabInner>,
    tab_id: TabId,
}

#[derive(Clone)]
pub struct PositionedPane {
    /// Stable identifier for the pane stack that owns this pane.
    pub pane_stack_id: PaneStackId,
    /// The topological pane index that can be used to reference this pane
    pub index: usize,
    /// true if this is the active pane at the time the position was computed
    pub is_active: bool,
    /// true if this pane is zoomed
    pub is_zoomed: bool,
    /// The offset from the top left corner of the containing tab to the top
    /// left corner of this pane, in cells.
    pub left: usize,
    /// The offset from the top left corner of the containing tab to the top
    /// left corner of this pane, in cells.
    pub top: usize,
    /// The width of this pane in cells
    pub width: usize,
    pub pixel_width: usize,
    /// The height of this pane in cells
    pub height: usize,
    pub pixel_height: usize,
    /// The pane instance
    pub pane: Arc<dyn Pane>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CollapsedPaneLayout {
    pub pane_stack_id: PaneStackId,
    pub split_direction: SplitDirection,
    pub active_is_second: bool,
    pub active_cells_before: usize,
}

impl std::fmt::Debug for PositionedPane {
    fn fmt(&self, fmt: &mut std::fmt::Formatter) -> std::result::Result<(), std::fmt::Error> {
        fmt.debug_struct("PositionedPane")
            .field("pane_stack_id", &self.pane_stack_id)
            .field("index", &self.index)
            .field("is_active", &self.is_active)
            .field("left", &self.left)
            .field("top", &self.top)
            .field("width", &self.width)
            .field("height", &self.height)
            .field("pane_id", &self.pane.pane_id())
            .finish()
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
pub enum SplitDirection {
    Horizontal,
    Vertical,
}

/// The size is of the (first, second) child of the split
#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
pub struct SplitDirectionAndSize {
    pub direction: SplitDirection,
    pub first: TerminalSize,
    pub second: TerminalSize,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
pub enum SplitSize {
    Cells(usize),
    Percent(u8),
}

impl Default for SplitSize {
    fn default() -> Self {
        Self::Percent(50)
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
pub struct SplitRequest {
    pub direction: SplitDirection,
    /// Whether the newly created item will be in the second part
    /// of the split (right/bottom)
    pub target_is_second: bool,
    /// Split across the top of the tab rather than the active pane
    pub top_level: bool,
    /// The size of the new item
    pub size: SplitSize,
}

impl Default for SplitRequest {
    fn default() -> Self {
        Self {
            direction: SplitDirection::Horizontal,
            target_is_second: true,
            top_level: false,
            size: SplitSize::default(),
        }
    }
}

impl SplitDirectionAndSize {
    fn top_of_second(&self) -> usize {
        match self.direction {
            SplitDirection::Horizontal => 0,
            SplitDirection::Vertical => self.first.rows as usize + 1,
        }
    }

    fn left_of_second(&self) -> usize {
        match self.direction {
            SplitDirection::Horizontal => self.first.cols as usize + 1,
            SplitDirection::Vertical => 0,
        }
    }

    pub fn width(&self) -> usize {
        if self.direction == SplitDirection::Horizontal {
            self.first.cols + self.second.cols + 1
        } else {
            self.first.cols
        }
    }

    pub fn height(&self) -> usize {
        if self.direction == SplitDirection::Vertical {
            self.first.rows + self.second.rows + 1
        } else {
            self.first.rows
        }
    }

    pub fn size(&self) -> TerminalSize {
        let cell_width = self.first.pixel_width / self.first.cols;
        let cell_height = self.first.pixel_height / self.first.rows;

        let rows = self.height();
        let cols = self.width();

        TerminalSize {
            rows,
            cols,
            pixel_height: cell_height * rows,
            pixel_width: cell_width * cols,
            dpi: self.first.dpi,
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct PositionedSplit {
    /// The topological node index that can be used to reference this split
    pub index: usize,
    pub direction: SplitDirection,
    /// The offset from the top left corner of the containing tab to the top
    /// left corner of this split, in cells.
    pub left: usize,
    /// The offset from the top left corner of the containing tab to the top
    /// left corner of this split, in cells.
    pub top: usize,
    /// For Horizontal splits, how tall the split should be, for Vertical
    /// splits how wide it should be
    pub size: usize,
}

fn is_pane(pane: &Arc<dyn Pane>, other: &Option<&Arc<dyn Pane>>) -> bool {
    if let Some(other) = other {
        other.pane_id() == pane.pane_id()
    } else {
        false
    }
}

fn pane_tree(
    tree: &Tree,
    tab_id: TabId,
    window_id: WindowId,
    active: Option<&Arc<dyn Pane>>,
    zoomed: Option<&Arc<dyn Pane>>,
    workspace: &str,
    left_col: usize,
    top_row: usize,
) -> PaneNode {
    fn pane_entry(
        pane: &Arc<dyn Pane>,
        tab_id: TabId,
        window_id: WindowId,
        active: Option<&Arc<dyn Pane>>,
        zoomed: Option<&Arc<dyn Pane>>,
        workspace: &str,
        left_col: usize,
        top_row: usize,
    ) -> PaneEntry {
        let dims = pane.get_dimensions();
        let working_dir = pane.get_current_working_dir(CachePolicy::AllowStale);
        let cursor_pos = pane.get_cursor_position();

        PaneEntry {
            window_id,
            tab_id,
            pane_id: pane.pane_id(),
            title: pane.get_title(),
            is_active_pane: is_pane(pane, &active),
            is_zoomed_pane: is_pane(pane, &zoomed),
            size: TerminalSize {
                cols: dims.cols,
                rows: dims.viewport_rows,
                pixel_height: dims.pixel_height,
                pixel_width: dims.pixel_width,
                dpi: dims.dpi,
            },
            working_dir: working_dir.map(Into::into),
            workspace: workspace.to_string(),
            cursor_pos,
            physical_top: dims.physical_top,
            left_col,
            top_row,
            tty_name: pane.tty_name(),
        }
    }

    match tree {
        Tree::Empty => PaneNode::Empty,
        Tree::Node { left, right, data } => {
            let data = data.unwrap();
            PaneNode::Split {
                left: Box::new(pane_tree(
                    &*left, tab_id, window_id, active, zoomed, workspace, left_col, top_row,
                )),
                right: Box::new(pane_tree(
                    &*right,
                    tab_id,
                    window_id,
                    active,
                    zoomed,
                    workspace,
                    if data.direction == SplitDirection::Vertical {
                        left_col
                    } else {
                        left_col + data.left_of_second()
                    },
                    if data.direction == SplitDirection::Horizontal {
                        top_row
                    } else {
                        top_row + data.top_of_second()
                    },
                )),
                node: data,
            }
        }
        Tree::Leaf(stack) => {
            let entries: Vec<_> = stack
                .panes
                .iter()
                .map(|pane| {
                    pane_entry(
                        pane, tab_id, window_id, active, zoomed, workspace, left_col, top_row,
                    )
                })
                .collect();

            if entries.len() == 1 {
                PaneNode::Leaf(entries.into_iter().next().unwrap())
            } else {
                PaneNode::Stack(PaneStackEntry {
                    active: stack.active_index(),
                    panes: entries,
                    pane_stack_id: Some(stack.id()),
                })
            }
        }
    }
}

fn build_from_pane_tree<F>(
    tree: bintree::Tree<PaneStackEntry, SplitDirectionAndSize>,
    active: &mut Option<Arc<dyn Pane>>,
    zoomed: &mut Option<Arc<dyn Pane>>,
    make_pane: &mut F,
) -> Tree
where
    F: FnMut(PaneEntry) -> Arc<dyn Pane>,
{
    match tree {
        bintree::Tree::Empty => Tree::Empty,
        bintree::Tree::Node { left, right, data } => Tree::Node {
            left: Box::new(build_from_pane_tree(*left, active, zoomed, make_pane)),
            right: Box::new(build_from_pane_tree(*right, active, zoomed, make_pane)),
            data,
        },
        bintree::Tree::Leaf(entry) => {
            let active_index = entry.active.min(entry.panes.len().saturating_sub(1));
            let stack_id = entry.pane_stack_id;
            let mut panes = vec![];

            for pane_entry in entry.panes {
                let is_zoomed_pane = pane_entry.is_zoomed_pane;
                let is_active_pane = pane_entry.is_active_pane;
                let pane = make_pane(pane_entry);
                if is_zoomed_pane {
                    zoomed.replace(Arc::clone(&pane));
                }
                if is_active_pane {
                    active.replace(Arc::clone(&pane));
                }
                panes.push(pane);
            }

            if panes.is_empty() {
                Tree::Empty
            } else {
                Tree::Leaf(PaneStack::from_panes_with_id(
                    panes,
                    active_index,
                    stack_id,
                ))
            }
        }
    }
}

/// Computes the minimum (x, y) size based on the panes in this portion
/// of the tree.
fn compute_min_size(tree: &mut Tree) -> (usize, usize) {
    match tree {
        Tree::Node { data: None, .. } | Tree::Empty => (1, 1),
        Tree::Node {
            left,
            right,
            data: Some(data),
        } => {
            let (left_x, left_y) = compute_min_size(&mut *left);
            let (right_x, right_y) = compute_min_size(&mut *right);
            match data.direction {
                SplitDirection::Vertical => (left_x.max(right_x), left_y + right_y + 1),
                SplitDirection::Horizontal => (left_x + right_x + 1, left_y.max(right_y)),
            }
        }
        Tree::Leaf(_) => (1, 1),
    }
}

fn adjust_x_size(tree: &mut Tree, mut x_adjust: isize, cell_dimensions: &TerminalSize) {
    let (min_x, _) = compute_min_size(tree);
    while x_adjust != 0 {
        match tree {
            Tree::Empty | Tree::Leaf(_) => return,
            Tree::Node { data: None, .. } => return,
            Tree::Node {
                left,
                right,
                data: Some(data),
            } => {
                data.first.dpi = cell_dimensions.dpi;
                data.second.dpi = cell_dimensions.dpi;
                match data.direction {
                    SplitDirection::Vertical => {
                        let new_cols = (data.first.cols as isize)
                            .saturating_add(x_adjust)
                            .max(min_x as isize);
                        x_adjust = new_cols.saturating_sub(data.first.cols as isize);

                        if x_adjust != 0 {
                            adjust_x_size(&mut *left, x_adjust, cell_dimensions);
                            data.first.cols = new_cols.try_into().unwrap();
                            data.first.pixel_width =
                                data.first.cols.saturating_mul(cell_dimensions.pixel_width);

                            adjust_x_size(&mut *right, x_adjust, cell_dimensions);
                            data.second.cols = data.first.cols;
                            data.second.pixel_width = data.first.pixel_width;
                        }
                        return;
                    }
                    SplitDirection::Horizontal if x_adjust > 0 => {
                        adjust_x_size(&mut *left, 1, cell_dimensions);
                        data.first.cols += 1;
                        data.first.pixel_width =
                            data.first.cols.saturating_mul(cell_dimensions.pixel_width);
                        x_adjust -= 1;

                        if x_adjust > 0 {
                            adjust_x_size(&mut *right, 1, cell_dimensions);
                            data.second.cols += 1;
                            data.second.pixel_width =
                                data.second.cols.saturating_mul(cell_dimensions.pixel_width);
                            x_adjust -= 1;
                        }
                    }
                    SplitDirection::Horizontal => {
                        // x_adjust is negative
                        if data.first.cols > 1 {
                            adjust_x_size(&mut *left, -1, cell_dimensions);
                            data.first.cols -= 1;
                            data.first.pixel_width =
                                data.first.cols.saturating_mul(cell_dimensions.pixel_width);
                            x_adjust += 1;
                        }
                        if x_adjust < 0 && data.second.cols > 1 {
                            adjust_x_size(&mut *right, -1, cell_dimensions);
                            data.second.cols -= 1;
                            data.second.pixel_width =
                                data.second.cols.saturating_mul(cell_dimensions.pixel_width);
                            x_adjust += 1;
                        }
                    }
                }
            }
        }
    }
}

fn adjust_y_size(tree: &mut Tree, mut y_adjust: isize, cell_dimensions: &TerminalSize) {
    let (_, min_y) = compute_min_size(tree);
    while y_adjust != 0 {
        match tree {
            Tree::Empty | Tree::Leaf(_) => return,
            Tree::Node { data: None, .. } => return,
            Tree::Node {
                left,
                right,
                data: Some(data),
            } => {
                data.first.dpi = cell_dimensions.dpi;
                data.second.dpi = cell_dimensions.dpi;
                match data.direction {
                    SplitDirection::Horizontal => {
                        let new_rows = (data.first.rows as isize)
                            .saturating_add(y_adjust)
                            .max(min_y as isize);
                        y_adjust = new_rows.saturating_sub(data.first.rows as isize);

                        if y_adjust != 0 {
                            adjust_y_size(&mut *left, y_adjust, cell_dimensions);
                            data.first.rows = new_rows.try_into().unwrap();
                            data.first.pixel_height =
                                data.first.rows.saturating_mul(cell_dimensions.pixel_height);

                            adjust_y_size(&mut *right, y_adjust, cell_dimensions);
                            data.second.rows = data.first.rows;
                            data.second.pixel_height = data.first.pixel_height;
                        }
                        return;
                    }
                    SplitDirection::Vertical if y_adjust > 0 => {
                        adjust_y_size(&mut *left, 1, cell_dimensions);
                        data.first.rows += 1;
                        data.first.pixel_height =
                            data.first.rows.saturating_mul(cell_dimensions.pixel_height);
                        y_adjust -= 1;
                        if y_adjust > 0 {
                            adjust_y_size(&mut *right, 1, cell_dimensions);
                            data.second.rows += 1;
                            data.second.pixel_height = data
                                .second
                                .rows
                                .saturating_mul(cell_dimensions.pixel_height);
                            y_adjust -= 1;
                        }
                    }
                    SplitDirection::Vertical => {
                        // y_adjust is negative
                        if data.first.rows > 1 {
                            adjust_y_size(&mut *left, -1, cell_dimensions);
                            data.first.rows -= 1;
                            data.first.pixel_height =
                                data.first.rows.saturating_mul(cell_dimensions.pixel_height);
                            y_adjust += 1;
                        }
                        if y_adjust < 0 && data.second.rows > 1 {
                            adjust_y_size(&mut *right, -1, cell_dimensions);
                            data.second.rows -= 1;
                            data.second.pixel_height = data
                                .second
                                .rows
                                .saturating_mul(cell_dimensions.pixel_height);
                            y_adjust += 1;
                        }
                    }
                }
            }
        }
    }
}

fn apply_sizes_from_splits(tree: &Tree, size: &TerminalSize) -> anyhow::Result<()> {
    match tree {
        Tree::Empty => return Ok(()),
        Tree::Node { data: None, .. } => return Ok(()),
        Tree::Node {
            left,
            right,
            data: Some(data),
        } => {
            apply_sizes_from_splits(&*left, &data.first)?;
            apply_sizes_from_splits(&*right, &data.second)?;
        }
        Tree::Leaf(stack) => {
            stack.resize(*size)?;
        }
    }
    Ok(())
}

/// Recompute split node sizes bottom-up from the contained panes'
/// current dimensions, returning the aggregate size of the tree.
fn compute_tree_size_from_panes(node: &mut Tree) -> Option<TerminalSize> {
    match node {
        Tree::Empty => None,
        Tree::Leaf(stack) => {
            let pane = stack.active_pane()?;
            let dims = pane.get_dimensions();
            let size = TerminalSize {
                cols: dims.cols,
                rows: dims.viewport_rows,
                pixel_height: dims.pixel_height,
                pixel_width: dims.pixel_width,
                dpi: dims.dpi,
            };
            Some(size)
        }
        Tree::Node { left, right, data } => {
            if let Some(data) = data {
                if let Some(first) = compute_tree_size_from_panes(left) {
                    data.first = first;
                }
                if let Some(second) = compute_tree_size_from_panes(right) {
                    data.second = second;
                }
                Some(data.size())
            } else {
                None
            }
        }
    }
}

fn cell_dimensions(size: &TerminalSize) -> TerminalSize {
    TerminalSize {
        rows: 1,
        cols: 1,
        pixel_width: size.pixel_width / size.cols,
        pixel_height: size.pixel_height / size.rows,
        dpi: size.dpi,
    }
}

impl Tab {
    pub fn new(size: &TerminalSize) -> Self {
        let inner = TabInner::new(size);
        let tab_id = inner.id;
        Self {
            inner: Mutex::new(inner),
            tab_id,
        }
    }

    pub fn get_title(&self) -> String {
        self.inner.lock().title.clone()
    }

    pub fn set_title(&self, title: &str) {
        let mut inner = self.inner.lock();
        if inner.title != title {
            inner.title = title.to_string();
            Mux::try_get().map(|mux| {
                mux.notify(MuxNotification::TabTitleChanged {
                    tab_id: inner.id,
                    title: title.to_string(),
                })
            });
        }
    }

    /// Called by the multiplexer client when building a local tab to
    /// mirror a remote tab.  The supplied `root` is the information
    /// about our counterpart in the the remote server.
    /// This method builds a local tree based on the remote tree which
    /// then replaces the local tree structure.
    ///
    /// The `make_pane` function is provided by the caller, and its purpose
    /// is to lookup an existing Pane that corresponds to the provided
    /// PaneEntry, or to create a new Pane from that entry.
    /// make_pane is expected to add the pane to the mux if it creates
    /// a new pane, otherwise the pane won't poll/update in the GUI.
    pub fn sync_with_pane_tree<F>(&self, size: TerminalSize, root: PaneNode, make_pane: F)
    where
        F: FnMut(PaneEntry) -> Arc<dyn Pane>,
    {
        self.inner.lock().sync_with_pane_tree(size, root, make_pane)
    }

    pub fn codec_pane_tree(&self) -> PaneNode {
        self.inner.lock().codec_pane_tree()
    }

    /// Returns a count of how many panes are in this tab
    pub fn count_panes(&self) -> Option<usize> {
        self.inner.try_lock().map(|mut inner| inner.count_panes())
    }

    /// Sets the zoom state, returns the prior state
    pub fn set_zoomed(&self, zoomed: bool) -> bool {
        self.inner.lock().set_zoomed(zoomed)
    }

    pub fn toggle_zoom(&self) {
        self.inner.lock().toggle_zoom()
    }

    pub fn contains_pane(&self, pane: PaneId) -> bool {
        self.inner.lock().contains_pane(pane)
    }

    pub fn iter_panes(&self) -> Vec<PositionedPane> {
        self.inner.lock().iter_panes()
    }

    pub fn iter_panes_ignoring_zoom(&self) -> Vec<PositionedPane> {
        self.inner.lock().iter_panes_ignoring_zoom()
    }

    pub fn iter_all_panes(&self) -> Vec<Arc<dyn Pane>> {
        self.inner.lock().iter_all_panes()
    }

    pub fn pane_stack_tabs(&self, pane_id: PaneId) -> Vec<PaneStackTab> {
        self.inner.lock().pane_stack_tabs(pane_id)
    }

    pub fn pane_stack_id(&self, pane_id: PaneId) -> Option<PaneStackId> {
        self.inner.lock().pane_stack_id(pane_id)
    }

    pub fn pane_split_direction_by_index(&self, pane_index: usize) -> Option<SplitDirection> {
        self.inner.lock().pane_split_direction_by_index(pane_index)
    }

    pub fn pane_index_for_pane(&self, pane_id: PaneId) -> Option<usize> {
        self.inner.lock().pane_index_for_pane(pane_id)
    }

    pub fn rotate_counter_clockwise(&self) {
        self.inner.lock().rotate_counter_clockwise()
    }

    pub fn rotate_clockwise(&self) {
        self.inner.lock().rotate_clockwise()
    }

    pub fn iter_splits(&self) -> Vec<PositionedSplit> {
        self.inner.lock().iter_splits()
    }

    pub fn tab_id(&self) -> TabId {
        self.tab_id
    }

    pub fn get_size(&self) -> TerminalSize {
        self.inner.lock().get_size()
    }

    /// Apply the new size of the tab to the panes contained within.
    /// The delta between the current and the new size is computed,
    /// and is distributed between the splits.  For small resizes
    /// this algorithm biases towards adjusting the left/top nodes
    /// first.  For large resizes this tends to proportionally adjust
    /// the relative sizes of the elements in a split.
    pub fn resize(&self, size: TerminalSize) {
        self.inner.lock().resize(size)
    }

    /// Called when running in the mux server after an individual pane
    /// has been resized.
    /// Because the split manipulation happened on the GUI we "lost"
    /// the information that would have allowed us to call resize_split_by()
    /// and instead need to back-infer the split size information.
    /// We rely on the client to have resized (or be in the process
    /// of resizing) affected panes consistently with its own Tab
    /// tree model.
    /// This method does a simple tree walk to the leaves to back-propagate
    /// the size of the panes up to their containing node split data.
    /// Without this step, disconnecting and reconnecting would cause
    /// the GUI to use stale size information for the window it spawns
    /// to attach this tab.
    pub fn rebuild_splits_sizes_from_contained_panes(&self) {
        self.inner
            .lock()
            .rebuild_splits_sizes_from_contained_panes()
    }

    /// Given split_index, the topological index of a split returned by
    /// iter_splits() as PositionedSplit::index, revised the split position
    /// by the provided delta; positive values move the split to the right/bottom,
    /// and negative values to the left/top.
    /// The adjusted size is propogated downwards to contained children and
    /// their panes are resized accordingly.
    pub fn resize_split_by(&self, split_index: usize, delta: isize) {
        self.inner.lock().resize_split_by(split_index, delta)
    }

    pub fn collapse_pane_by_index(
        &self,
        pane_index: usize,
        min_cells: usize,
    ) -> Option<CollapsedPaneLayout> {
        self.inner
            .lock()
            .collapse_pane_by_index(pane_index, min_cells)
    }

    pub fn restore_collapsed_pane(&self, layout: CollapsedPaneLayout) -> bool {
        self.inner.lock().restore_collapsed_pane(layout)
    }

    pub fn reapply_collapsed_pane(&self, layout: CollapsedPaneLayout, min_cells: usize) -> bool {
        self.inner.lock().reapply_collapsed_pane(layout, min_cells)
    }

    /// Adjusts the size of the active pane in the specified direction
    /// by the specified amount.
    pub fn adjust_pane_size(&self, direction: PaneDirection, amount: usize) {
        self.inner.lock().adjust_pane_size(direction, amount)
    }

    /// Activate an adjacent pane in the specified direction.
    /// In cases where there are multiple adjacent panes in the
    /// intended direction, we take the pane that has the largest
    /// edge intersection.
    pub fn activate_pane_direction(&self, direction: PaneDirection) {
        self.inner.lock().activate_pane_direction(direction)
    }

    /// Returns an adjacent pane in the specified direction.
    /// In cases where there are multiple adjacent panes in the
    /// intended direction, we take the pane that has the largest
    /// edge intersection.
    pub fn get_pane_direction(&self, direction: PaneDirection, ignore_zoom: bool) -> Option<usize> {
        self.inner.lock().get_pane_direction(direction, ignore_zoom)
    }

    pub fn prune_dead_panes(&self) -> bool {
        self.inner.lock().prune_dead_panes()
    }

    pub fn kill_pane(&self, pane_id: PaneId) -> bool {
        self.inner.lock().kill_pane(pane_id)
    }

    pub fn kill_panes_in_domain(&self, domain: DomainId) -> bool {
        self.inner.lock().kill_panes_in_domain(domain)
    }

    /// Remove pane from tab.
    /// The pane is still live in the mux; the intent is for the pane to
    /// be added to a different tab.
    pub fn remove_pane(&self, pane_id: PaneId) -> Option<Arc<dyn Pane>> {
        self.inner.lock().remove_pane(pane_id)
    }

    pub fn can_close_without_prompting(&self, reason: CloseReason) -> bool {
        self.inner.lock().can_close_without_prompting(reason)
    }

    pub fn is_dead(&self) -> bool {
        self.inner.lock().is_dead()
    }

    pub fn get_active_pane(&self) -> Option<Arc<dyn Pane>> {
        self.inner.lock().get_active_pane()
    }

    #[allow(unused)]
    pub fn get_active_idx(&self) -> usize {
        self.inner.lock().get_active_idx()
    }

    pub fn set_active_pane(&self, pane: &Arc<dyn Pane>) {
        self.inner.lock().set_active_pane(pane)
    }

    pub fn set_active_pane_silent(&self, pane: &Arc<dyn Pane>) {
        self.inner.lock().set_active_pane_silent(pane)
    }

    pub fn set_active_idx(&self, pane_index: usize) {
        self.inner.lock().set_active_idx(pane_index)
    }

    pub fn add_pane_to_stack(
        &self,
        base_pane_id: PaneId,
        pane: Arc<dyn Pane>,
    ) -> anyhow::Result<usize> {
        self.inner.lock().add_pane_to_stack(base_pane_id, pane)
    }

    pub fn activate_pane_in_stack(&self, pane_id: PaneId) -> anyhow::Result<usize> {
        self.inner.lock().activate_pane_in_stack(pane_id)
    }

    /// Assigns the root pane.
    /// This is suitable when creating a new tab and then assigning
    /// the initial pane
    pub fn assign_pane(&self, pane: &Arc<dyn Pane>) {
        self.inner.lock().assign_pane(pane)
    }

    /// Swap the active pane with the specified pane_index
    pub fn swap_active_with_index(&self, pane_index: usize, keep_focus: bool) -> Option<()> {
        self.inner
            .lock()
            .swap_active_with_index(pane_index, keep_focus)
    }

    /// Computes the size of the pane that would result if the specified
    /// pane was split in a particular direction.
    /// The intent is to call this prior to spawning the new pane so that
    /// you can create it with the correct size.
    /// May return None if the specified pane_index is invalid.
    pub fn compute_split_size(
        &self,
        pane_index: usize,
        request: SplitRequest,
    ) -> Option<SplitDirectionAndSize> {
        self.inner.lock().compute_split_size(pane_index, request)
    }

    /// Split the pane that has pane_index in the given direction and assign
    /// the right/bottom pane of the newly created split to the provided Pane
    /// instance.  Returns the resultant index of the newly inserted pane.
    /// Both the split and the inserted pane will be resized.
    pub fn split_and_insert(
        &self,
        pane_index: usize,
        request: SplitRequest,
        pane: Arc<dyn Pane>,
    ) -> anyhow::Result<usize> {
        self.inner
            .lock()
            .split_and_insert(pane_index, request, pane)
    }

    pub fn get_zoomed_pane(&self) -> Option<Arc<dyn Pane>> {
        self.inner.lock().get_zoomed_pane()
    }
}

impl TabInner {
    fn new(size: &TerminalSize) -> Self {
        Self {
            id: TAB_ID.fetch_add(1, ::std::sync::atomic::Ordering::Relaxed),
            pane: Some(Tree::new()),
            size: *size,
            size_before_zoom: *size,
            active: 0,
            zoomed: None,
            title: String::new(),
            recency: Recency::default(),
        }
    }

    fn sync_with_pane_tree<F>(&mut self, size: TerminalSize, root: PaneNode, mut make_pane: F)
    where
        F: FnMut(PaneEntry) -> Arc<dyn Pane>,
    {
        let mut active = None;
        let mut zoomed = None;

        log::debug!("sync_with_pane_tree with size {:?}", size);

        let t = build_from_pane_tree(root.into_tree(), &mut active, &mut zoomed, &mut make_pane);
        let mut cursor = t.cursor();

        self.active = 0;
        if let Some(active) = active {
            // Resolve the active pane to its index
            let mut index = 0;
            loop {
                if let Some(stack) = cursor.leaf_mut() {
                    if stack.contains_pane(active.pane_id()) {
                        // Found it
                        self.active = index;
                        self.recency.tag(index);
                        break;
                    }
                    index += 1;
                }
                match cursor.preorder_next() {
                    Ok(c) => cursor = c,
                    Err(c) => {
                        // Didn't find it
                        cursor = c;
                        break;
                    }
                }
            }
        }
        self.pane.replace(cursor.tree());
        self.zoomed = zoomed;

        // The rebuilt tree carries the peer's cell geometry (derived from
        // its pane dimensions, which for client tabs are smaller than the
        // local cells because the GUI reserves per-pane chrome). Recompute
        // self.size from that tree so that the resize below starts from an
        // accurate value, then re-impose the target size: for client tabs
        // the target is the locally (window-)derived size, which stays
        // authoritative over whatever round-tripped through the server.
        // resize() is a no-op (and emits no TabResized) when the sizes
        // already agree.
        if let Some(root) = self.pane.as_mut() {
            if let Some(tree_size) = compute_tree_size_from_panes(root) {
                self.size = tree_size;
            }
        }
        self.resize(size);

        log::debug!(
            "sync tab: {:#?} zoomed: {} {:#?}",
            size,
            self.zoomed.is_some(),
            self.iter_panes()
        );
        assert!(self.pane.is_some());
    }

    fn codec_pane_tree(&mut self) -> PaneNode {
        let mux = Mux::get();
        let tab_id = self.id;
        let window_id = match mux.window_containing_tab(tab_id) {
            Some(w) => w,
            None => {
                log::error!("no window contains tab {}", tab_id);
                return PaneNode::Empty;
            }
        };

        let workspace = match mux
            .get_window(window_id)
            .map(|w| w.get_workspace().to_string())
        {
            Some(ws) => ws,
            None => {
                log::error!("window id {} doesn't have a window!?", window_id);
                return PaneNode::Empty;
            }
        };

        let active = self.get_active_pane();
        let zoomed = self.zoomed.as_ref();
        if let Some(root) = self.pane.as_ref() {
            pane_tree(
                root,
                tab_id,
                window_id,
                active.as_ref(),
                zoomed,
                &workspace,
                0,
                0,
            )
        } else {
            PaneNode::Empty
        }
    }

    /// Returns a count of how many panes are in this tab
    fn count_panes(&mut self) -> usize {
        let mut count = 0;
        let mut cursor = self.pane.take().unwrap().cursor();

        loop {
            if cursor.is_leaf() {
                count += cursor.leaf_mut().unwrap().len();
            }
            match cursor.preorder_next() {
                Ok(c) => cursor = c,
                Err(c) => {
                    self.pane.replace(c.tree());
                    return count;
                }
            }
        }
    }

    /// Sets the zoom state, returns the prior state
    fn set_zoomed(&mut self, zoomed: bool) -> bool {
        if self.zoomed.is_some() == zoomed {
            // Current zoom state matches intended zoom state,
            // so we have nothing to do.
            return zoomed;
        }
        self.toggle_zoom();
        !zoomed
    }

    fn toggle_zoom(&mut self) {
        let size = self.size;
        if self.zoomed.take().is_some() {
            // We were zoomed, but now we are not.
            // Re-apply the size to the panes
            if let Some(pane) = self.get_active_pane() {
                pane.set_zoomed(false);
            }
            self.size = self.size_before_zoom;
            self.resize(size);
        } else {
            // We weren't zoomed, but now we want to zoom.
            // Locate the active pane
            self.size_before_zoom = size;
            if let Some(pane) = self.get_active_pane() {
                pane.set_zoomed(true);
                if let Err(err) = pane.resize(size) {
                    log::error!("failed to resize zoomed pane: {err:#}");
                }
                self.zoomed.replace(pane);
            }
        }
        Mux::try_get().map(|mux| mux.notify(MuxNotification::TabResized(self.id)));
    }

    fn contains_pane(&self, pane: PaneId) -> bool {
        fn contains(tree: &Tree, pane: PaneId) -> bool {
            match tree {
                Tree::Empty => false,
                Tree::Node { left, right, .. } => contains(left, pane) || contains(right, pane),
                Tree::Leaf(stack) => stack.contains_pane(pane),
            }
        }
        match &self.pane {
            Some(root) => contains(root, pane),
            None => false,
        }
    }

    /// Walks the pane tree to produce the topologically ordered flattened
    /// list of PositionedPane instances along with their positioning information.
    fn iter_panes(&mut self) -> Vec<PositionedPane> {
        self.iter_panes_impl(true)
    }

    /// Like iter_panes, except that it will include all panes, regardless of
    /// whether one of them is currently zoomed.
    fn iter_panes_ignoring_zoom(&mut self) -> Vec<PositionedPane> {
        self.iter_panes_impl(false)
    }

    fn iter_all_panes(&mut self) -> Vec<Arc<dyn Pane>> {
        let mut panes = vec![];
        let mut cursor = self.pane.take().unwrap().cursor();

        loop {
            if cursor.is_leaf() {
                panes.extend(cursor.leaf_mut().unwrap().panes.iter().cloned());
            }

            match cursor.preorder_next() {
                Ok(c) => cursor = c,
                Err(c) => {
                    self.pane.replace(c.tree());
                    break;
                }
            }
        }

        panes
    }

    fn pane_stack_tabs(&mut self, pane_id: PaneId) -> Vec<PaneStackTab> {
        let mut tabs = vec![];
        let mut cursor = self.pane.take().unwrap().cursor();

        loop {
            if cursor.is_leaf() {
                let stack = cursor.leaf_mut().unwrap();
                if stack.contains_pane(pane_id) {
                    tabs = stack.tabs();
                }
            }

            match cursor.preorder_next() {
                Ok(c) if tabs.is_empty() => cursor = c,
                Ok(c) | Err(c) => {
                    self.pane.replace(c.tree());
                    break;
                }
            }
        }

        tabs
    }

    fn pane_stack_id(&mut self, pane_id: PaneId) -> Option<PaneStackId> {
        let mut pane_stack_id = None;
        let mut cursor = self.pane.take().unwrap().cursor();

        loop {
            if cursor.is_leaf() {
                let stack = cursor.leaf_mut().unwrap();
                if stack.contains_pane(pane_id) {
                    pane_stack_id = Some(stack.id());
                }
            }

            match cursor.preorder_next() {
                Ok(c) if pane_stack_id.is_none() => cursor = c,
                Ok(c) | Err(c) => {
                    self.pane.replace(c.tree());
                    break;
                }
            }
        }

        pane_stack_id
    }

    fn pane_split_direction_by_index(&mut self, pane_index: usize) -> Option<SplitDirection> {
        if self.zoomed.is_some() {
            return None;
        }

        let mut cursor = self.pane.take()?.cursor();
        let mut index = 0;

        loop {
            if cursor.is_leaf() {
                if index == pane_index {
                    break;
                }
                index += 1;
            }

            match cursor.preorder_next() {
                Ok(c) => cursor = c,
                Err(c) => {
                    self.pane.replace(c.tree());
                    return None;
                }
            }
        }

        let direction = {
            let mut path = cursor.path_to_root();
            path.next()
                .and_then(|(_, parent)| parent.map(|node| node.direction))
        };
        self.pane.replace(cursor.tree());
        direction
    }

    fn pane_index_for_pane(&mut self, pane_id: PaneId) -> Option<usize> {
        let mut cursor = self.pane.take().unwrap().cursor();
        let mut pane_index = 0;
        let mut found = None;

        loop {
            if cursor.is_leaf() {
                if cursor.leaf_mut().unwrap().contains_pane(pane_id) {
                    found = Some(pane_index);
                }
                pane_index += 1;
            }

            match cursor.preorder_next() {
                Ok(c) if found.is_none() => cursor = c,
                Ok(c) | Err(c) => {
                    self.pane.replace(c.tree());
                    break;
                }
            }
        }

        found
    }

    fn iter_stacks(&mut self) -> Vec<PaneStack> {
        let mut stacks = vec![];
        let mut cursor = self.pane.take().unwrap().cursor();

        loop {
            if cursor.is_leaf() {
                stacks.push(cursor.leaf_mut().unwrap().clone());
            }

            match cursor.preorder_next() {
                Ok(c) => cursor = c,
                Err(c) => {
                    self.pane.replace(c.tree());
                    break;
                }
            }
        }

        stacks
    }

    fn rotate_counter_clockwise(&mut self) {
        let stacks = self.iter_stacks();
        if stacks.is_empty() {
            // Shouldn't happen, but we check for this here so that the
            // expect below cannot trigger a panic
            return;
        }
        let mut stack_to_swap = stacks.first().cloned().expect("at least one pane");

        let mut cursor = self.pane.take().unwrap().cursor();

        loop {
            if cursor.is_leaf() {
                std::mem::swap(&mut stack_to_swap, cursor.leaf_mut().unwrap());
            }

            match cursor.postorder_next() {
                Ok(c) => cursor = c,
                Err(c) => {
                    self.pane.replace(c.tree());
                    let size = self.size;
                    if let Err(err) = apply_sizes_from_splits(self.pane.as_mut().unwrap(), &size) {
                        log::error!("failed to resize panes after rotation: {err:#}");
                    }
                    break;
                }
            }
        }
    }

    fn rotate_clockwise(&mut self) {
        let stacks = self.iter_stacks();
        if stacks.is_empty() {
            // Shouldn't happen, but we check for this here so that the
            // expect below cannot trigger a panic
            return;
        }
        let mut stack_to_swap = stacks.last().cloned().expect("at least one pane");

        let mut cursor = self.pane.take().unwrap().cursor();

        loop {
            if cursor.is_leaf() {
                std::mem::swap(&mut stack_to_swap, cursor.leaf_mut().unwrap());
            }

            match cursor.preorder_next() {
                Ok(c) => cursor = c,
                Err(c) => {
                    self.pane.replace(c.tree());
                    let size = self.size;
                    if let Err(err) = apply_sizes_from_splits(self.pane.as_mut().unwrap(), &size) {
                        log::error!("failed to resize panes after rotation: {err:#}");
                    }
                    break;
                }
            }
        }
        Mux::try_get().map(|mux| mux.notify(MuxNotification::TabResized(self.id)));
    }

    fn iter_panes_impl(&mut self, respect_zoom_state: bool) -> Vec<PositionedPane> {
        let mut panes = vec![];

        if respect_zoom_state {
            if let Some(zoomed) = self.zoomed.as_ref() {
                let size = self.size;
                panes.push(PositionedPane {
                    pane_stack_id: zoomed.pane_id(),
                    index: 0,
                    is_active: true,
                    is_zoomed: true,
                    left: 0,
                    top: 0,
                    width: size.cols.into(),
                    pixel_width: size.pixel_width.into(),
                    height: size.rows.into(),
                    pixel_height: size.pixel_height.into(),
                    pane: Arc::clone(zoomed),
                });
                return panes;
            }
        }

        let active_idx = self.active;
        let zoomed_id = self.zoomed.as_ref().map(|p| p.pane_id());
        let root_size = self.size;
        let mut cursor = self.pane.take().unwrap().cursor();

        loop {
            if cursor.is_leaf() {
                let index = panes.len();
                let mut left = 0usize;
                let mut top = 0usize;
                let mut parent_size = None;
                for (branch, node) in cursor.path_to_root() {
                    if let Some(node) = node {
                        if parent_size.is_none() {
                            parent_size.replace(if branch == PathBranch::IsRight {
                                node.second
                            } else {
                                node.first
                            });
                        }
                        if branch == PathBranch::IsRight {
                            top += node.top_of_second();
                            left += node.left_of_second();
                        }
                    }
                }

                if let Some(pane) = cursor.leaf_mut().unwrap().active_pane() {
                    let dims = parent_size.unwrap_or_else(|| root_size);
                    let pane_stack_id = cursor.leaf_mut().unwrap().id();

                    panes.push(PositionedPane {
                        pane_stack_id,
                        index,
                        is_active: index == active_idx,
                        is_zoomed: zoomed_id == Some(pane.pane_id()),
                        left,
                        top,
                        width: dims.cols as _,
                        height: dims.rows as _,
                        pixel_width: dims.pixel_width as _,
                        pixel_height: dims.pixel_height as _,
                        pane,
                    });
                }
            }

            match cursor.preorder_next() {
                Ok(c) => cursor = c,
                Err(c) => {
                    self.pane.replace(c.tree());
                    break;
                }
            }
        }

        panes
    }

    fn iter_splits(&mut self) -> Vec<PositionedSplit> {
        let mut dividers = vec![];
        if self.zoomed.is_some() {
            return dividers;
        }

        let mut cursor = self.pane.take().unwrap().cursor();
        let mut index = 0;

        loop {
            if !cursor.is_leaf() {
                let mut left = 0usize;
                let mut top = 0usize;
                for (branch, p) in cursor.path_to_root() {
                    if let Some(p) = p {
                        if branch == PathBranch::IsRight {
                            left += p.left_of_second();
                            top += p.top_of_second();
                        }
                    }
                }
                if let Ok(Some(node)) = cursor.node_mut() {
                    match node.direction {
                        SplitDirection::Horizontal => left += node.first.cols as usize,
                        SplitDirection::Vertical => top += node.first.rows as usize,
                    }

                    dividers.push(PositionedSplit {
                        index,
                        direction: node.direction,
                        left,
                        top,
                        size: if node.direction == SplitDirection::Horizontal {
                            node.height() as usize
                        } else {
                            node.width() as usize
                        },
                    })
                }
                index += 1;
            }

            match cursor.preorder_next() {
                Ok(c) => cursor = c,
                Err(c) => {
                    self.pane.replace(c.tree());
                    break;
                }
            }
        }

        dividers
    }

    fn get_size(&self) -> TerminalSize {
        self.size
    }

    fn resize(&mut self, size: TerminalSize) {
        if size.rows == 0 || size.cols == 0 {
            // Ignore "impossible" resize requests
            return;
        }

        // No-op resizes must not emit TabResized: for mux client tabs the
        // notification round-trips through the server and triggers a resync
        // (which itself calls resize), so an unconditional notify turns any
        // transient client/server size disagreement into an endless
        // resize/resync storm that visibly flickers the window contents.
        if size == self.size {
            return;
        }

        if let Some(zoomed) = &self.zoomed {
            self.size = size;
            if !zoomed.is_remote_mirror() {
                if let Err(err) = zoomed.resize(size) {
                    log::error!("failed to resize zoomed pane: {err:#}");
                }
            }
        } else {
            let dims = cell_dimensions(&size);
            let (min_x, min_y) = compute_min_size(self.pane.as_mut().unwrap());
            let current_size = self.size;

            // Constrain the new size to the minimum possible dimensions
            let cols = size.cols.max(min_x);
            let rows = size.rows.max(min_y);
            let size = TerminalSize {
                rows,
                cols,
                pixel_width: cols * dims.pixel_width,
                pixel_height: rows * dims.pixel_height,
                dpi: dims.dpi,
            };

            // Update the split nodes with adjusted sizes
            adjust_x_size(
                self.pane.as_mut().unwrap(),
                cols as isize - current_size.cols as isize,
                &dims,
            );
            adjust_y_size(
                self.pane.as_mut().unwrap(),
                rows as isize - current_size.rows as isize,
                &dims,
            );

            self.size = size;

            // And then resize the individual panes to match
            if let Err(err) = apply_sizes_from_splits(self.pane.as_mut().unwrap(), &size) {
                log::error!("failed to resize panes after split adjustment: {err:#}");
            }
        }

        Mux::try_get().map(|mux| mux.notify(MuxNotification::TabResized(self.id)));
    }

    fn apply_pane_size(&mut self, pane_size: TerminalSize, cursor: &mut Cursor) {
        let cell_width = pane_size
            .pixel_width
            .checked_div(pane_size.cols)
            .unwrap_or(1);
        let cell_height = pane_size
            .pixel_height
            .checked_div(pane_size.rows)
            .unwrap_or(1);
        if let Ok(Some(node)) = cursor.node_mut() {
            // Adjust the size of the node; we preserve the size of the first
            // child and adjust the second, so if we are split down the middle
            // and the window is made wider, the right column will grow in
            // size, leaving the left at its current width.
            if node.direction == SplitDirection::Horizontal {
                node.first.rows = pane_size.rows;
                node.second.rows = pane_size.rows;

                // Clamp the preserved first branch to what actually fits;
                // collapse/expand can shrink a branch below its child's
                // remembered size, and an unclamped first here makes this
                // node wider than its parent (overlapping pane geometry).
                node.first.cols = node
                    .first
                    .cols
                    .min(pane_size.cols.saturating_sub(2))
                    .max(1);
                node.second.cols = pane_size.cols.saturating_sub(1 + node.first.cols);
            } else {
                node.first.cols = pane_size.cols;
                node.second.cols = pane_size.cols;

                node.first.rows = node
                    .first
                    .rows
                    .min(pane_size.rows.saturating_sub(2))
                    .max(1);
                node.second.rows = pane_size.rows.saturating_sub(1 + node.first.rows);
            }
            node.first.pixel_width = node.first.cols * cell_width;
            node.first.pixel_height = node.first.rows * cell_height;

            node.second.pixel_width = node.second.cols * cell_width;
            node.second.pixel_height = node.second.rows * cell_height;
        }
    }

    fn rebuild_splits_sizes_from_contained_panes(&mut self) {
        if self.zoomed.is_some() {
            return;
        }

        if let Some(root) = self.pane.as_mut() {
            if let Some(size) = compute_tree_size_from_panes(root) {
                self.size = size;
            }
        }
        Mux::try_get().map(|mux| mux.notify(MuxNotification::TabResized(self.id)));
    }

    fn resize_split_by(&mut self, split_index: usize, delta: isize) {
        if self.zoomed.is_some() {
            return;
        }

        let mut cursor = self.pane.take().unwrap().cursor();
        let mut index = 0;

        // Position cursor on the specified split
        loop {
            if !cursor.is_leaf() {
                if index == split_index {
                    // Found it
                    break;
                }
                index += 1;
            }
            match cursor.preorder_next() {
                Ok(c) => cursor = c,
                Err(c) => {
                    // Didn't find it
                    self.pane.replace(c.tree());
                    return;
                }
            }
        }

        // Now cursor is looking at the split
        self.adjust_node_at_cursor(&mut cursor, delta);
        self.cascade_size_from_cursor(cursor);
        Mux::try_get().map(|mux| mux.notify(MuxNotification::TabResized(self.id)));
    }

    fn adjust_node_at_cursor(&mut self, cursor: &mut Cursor, delta: isize) {
        let cell_dimensions = self.cell_dimensions();
        if let Ok(Some(node)) = cursor.node_mut() {
            match node.direction {
                SplitDirection::Horizontal => {
                    let width = node.width();

                    let mut cols = node.first.cols as isize;
                    cols = cols
                        .saturating_add(delta)
                        .max(1)
                        .min((width as isize).saturating_sub(2));
                    node.first.cols = cols as usize;
                    node.first.pixel_width =
                        node.first.cols.saturating_mul(cell_dimensions.pixel_width);

                    node.second.cols = width.saturating_sub(node.first.cols.saturating_add(1));
                    node.second.pixel_width =
                        node.second.cols.saturating_mul(cell_dimensions.pixel_width);
                }
                SplitDirection::Vertical => {
                    let height = node.height();

                    let mut rows = node.first.rows as isize;
                    rows = rows
                        .saturating_add(delta)
                        .max(1)
                        .min((height as isize).saturating_sub(2));
                    node.first.rows = rows as usize;
                    node.first.pixel_height =
                        node.first.rows.saturating_mul(cell_dimensions.pixel_height);

                    node.second.rows = height.saturating_sub(node.first.rows.saturating_add(1));
                    node.second.pixel_height = node
                        .second
                        .rows
                        .saturating_mul(cell_dimensions.pixel_height);
                }
            }
        }
    }

    fn split_branch_cells(node: &SplitDirectionAndSize, active_is_second: bool) -> usize {
        match (node.direction, active_is_second) {
            (SplitDirection::Horizontal, false) => node.first.cols,
            (SplitDirection::Horizontal, true) => node.second.cols,
            (SplitDirection::Vertical, false) => node.first.rows,
            (SplitDirection::Vertical, true) => node.second.rows,
        }
    }

    fn set_split_branch_cells(
        node: &mut SplitDirectionAndSize,
        active_is_second: bool,
        active_cells: usize,
        cell_dimensions: TerminalSize,
    ) {
        let total = match node.direction {
            SplitDirection::Horizontal => node.width(),
            SplitDirection::Vertical => node.height(),
        };
        if total < 3 {
            return;
        }

        let active_cells = active_cells.max(1).min(total.saturating_sub(2));
        let other_cells = total.saturating_sub(active_cells.saturating_add(1)).max(1);

        match (node.direction, active_is_second) {
            (SplitDirection::Horizontal, false) => {
                node.first.cols = active_cells;
                node.second.cols = other_cells;
                node.first.pixel_width = active_cells.saturating_mul(cell_dimensions.pixel_width);
                node.second.pixel_width = other_cells.saturating_mul(cell_dimensions.pixel_width);
            }
            (SplitDirection::Horizontal, true) => {
                node.first.cols = other_cells;
                node.second.cols = active_cells;
                node.first.pixel_width = other_cells.saturating_mul(cell_dimensions.pixel_width);
                node.second.pixel_width = active_cells.saturating_mul(cell_dimensions.pixel_width);
            }
            (SplitDirection::Vertical, false) => {
                node.first.rows = active_cells;
                node.second.rows = other_cells;
                node.first.pixel_height = active_cells.saturating_mul(cell_dimensions.pixel_height);
                node.second.pixel_height = other_cells.saturating_mul(cell_dimensions.pixel_height);
            }
            (SplitDirection::Vertical, true) => {
                node.first.rows = other_cells;
                node.second.rows = active_cells;
                node.first.pixel_height = other_cells.saturating_mul(cell_dimensions.pixel_height);
                node.second.pixel_height =
                    active_cells.saturating_mul(cell_dimensions.pixel_height);
            }
        }
    }

    fn collapse_pane_by_index(
        &mut self,
        pane_index: usize,
        min_cells: usize,
    ) -> Option<CollapsedPaneLayout> {
        if self.zoomed.is_some() {
            return None;
        }

        let mut cursor = self.pane.take()?.cursor();
        let mut index = 0;
        loop {
            if cursor.is_leaf() {
                if index == pane_index {
                    break;
                }
                index += 1;
            }
            match cursor.preorder_next() {
                Ok(c) => cursor = c,
                Err(c) => {
                    self.pane.replace(c.tree());
                    return None;
                }
            }
        }

        let pane_stack_id = match cursor.leaf_mut().map(|stack| stack.id()) {
            Some(pane_stack_id) => pane_stack_id,
            None => {
                self.pane.replace(cursor.tree());
                return None;
            }
        };
        let (branch, parent_node) = match cursor.path_to_root().next() {
            Some((branch, Some(parent_node))) => (branch, *parent_node),
            _ => {
                self.pane.replace(cursor.tree());
                return None;
            }
        };
        let active_is_second = branch == PathBranch::IsRight;
        let active_cells_before = Self::split_branch_cells(&parent_node, active_is_second);

        match cursor.go_up() {
            Ok(mut parent_cursor) => {
                let cell_dimensions = self.cell_dimensions();
                if let Ok(Some(node)) = parent_cursor.node_mut() {
                    let layout = CollapsedPaneLayout {
                        pane_stack_id,
                        split_direction: node.direction,
                        active_is_second,
                        active_cells_before,
                    };
                    Self::set_split_branch_cells(
                        node,
                        active_is_second,
                        min_cells,
                        cell_dimensions,
                    );
                    self.cascade_size_from_cursor(parent_cursor);
                    Some(layout)
                } else {
                    self.pane.replace(parent_cursor.tree());
                    None
                }
            }
            Err(c) => {
                self.pane.replace(c.tree());
                None
            }
        }
    }

    fn restore_collapsed_pane(&mut self, layout: CollapsedPaneLayout) -> bool {
        if self.zoomed.is_some() {
            return false;
        }

        let mut cursor = match self.pane.take() {
            Some(tree) => tree.cursor(),
            None => return false,
        };
        loop {
            if cursor.is_leaf() {
                let contains_pane = cursor
                    .leaf_mut()
                    .is_some_and(|stack| stack.id() == layout.pane_stack_id);
                if contains_pane {
                    break;
                }
            }
            match cursor.preorder_next() {
                Ok(c) => cursor = c,
                Err(c) => {
                    self.pane.replace(c.tree());
                    return false;
                }
            }
        }

        match cursor.go_up() {
            Ok(mut parent_cursor) => {
                let cell_dimensions = self.cell_dimensions();
                if let Ok(Some(node)) = parent_cursor.node_mut() {
                    if node.direction != layout.split_direction {
                        self.pane.replace(parent_cursor.tree());
                        return false;
                    }
                    Self::set_split_branch_cells(
                        node,
                        layout.active_is_second,
                        layout.active_cells_before,
                        cell_dimensions,
                    );
                    self.cascade_size_from_cursor(parent_cursor);
                    true
                } else {
                    self.pane.replace(parent_cursor.tree());
                    false
                }
            }
            Err(c) => {
                self.pane.replace(c.tree());
                false
            }
        }
    }

    fn reapply_collapsed_pane(&mut self, layout: CollapsedPaneLayout, min_cells: usize) -> bool {
        if self.zoomed.is_some() {
            return false;
        }

        let mut cursor = match self.pane.take() {
            Some(tree) => tree.cursor(),
            None => return false,
        };
        loop {
            if cursor.is_leaf() {
                let contains_pane = cursor
                    .leaf_mut()
                    .is_some_and(|stack| stack.id() == layout.pane_stack_id);
                if contains_pane {
                    break;
                }
            }
            match cursor.preorder_next() {
                Ok(c) => cursor = c,
                Err(c) => {
                    self.pane.replace(c.tree());
                    return false;
                }
            }
        }

        match cursor.go_up() {
            Ok(mut parent_cursor) => {
                let cell_dimensions = self.cell_dimensions();
                if let Ok(Some(node)) = parent_cursor.node_mut() {
                    if node.direction != layout.split_direction {
                        self.pane.replace(parent_cursor.tree());
                        return false;
                    }
                    Self::set_split_branch_cells(
                        node,
                        layout.active_is_second,
                        min_cells,
                        cell_dimensions,
                    );
                    self.cascade_size_from_cursor(parent_cursor);
                    true
                } else {
                    self.pane.replace(parent_cursor.tree());
                    false
                }
            }
            Err(c) => {
                self.pane.replace(c.tree());
                false
            }
        }
    }

    fn cascade_size_from_cursor(&mut self, mut cursor: Cursor) {
        // Now we need to cascade this down to children
        match cursor.preorder_next() {
            Ok(c) => cursor = c,
            Err(c) => {
                self.pane.replace(c.tree());
                return;
            }
        }
        let root_size = self.size;

        loop {
            // Figure out the available size by looking at our immediate parent node.
            // If we are the root, look at the provided new size
            let pane_size = if let Some((branch, Some(parent))) = cursor.path_to_root().next() {
                if branch == PathBranch::IsRight {
                    parent.second
                } else {
                    parent.first
                }
            } else {
                root_size
            };

            if cursor.is_leaf() {
                // Apply our size to the tty
                if let Some(stack) = cursor.leaf_mut() {
                    if let Err(err) = stack.resize(pane_size) {
                        log::error!("failed to resize pane stack: {err:#}");
                    }
                }
            } else {
                self.apply_pane_size(pane_size, &mut cursor);
            }
            match cursor.preorder_next() {
                Ok(c) => cursor = c,
                Err(c) => {
                    self.pane.replace(c.tree());
                    break;
                }
            }
        }
        Mux::try_get().map(|mux| mux.notify(MuxNotification::TabResized(self.id)));
    }

    fn adjust_pane_size(&mut self, direction: PaneDirection, amount: usize) {
        if self.zoomed.is_some() {
            return;
        }
        let active_index = self.active;
        let mut cursor = self.pane.take().unwrap().cursor();
        let mut index = 0;

        // Position cursor on the active leaf
        loop {
            if cursor.is_leaf() {
                if index == active_index {
                    // Found it
                    break;
                }
                index += 1;
            }
            match cursor.preorder_next() {
                Ok(c) => cursor = c,
                Err(c) => {
                    // Didn't find it
                    self.pane.replace(c.tree());
                    return;
                }
            }
        }

        // We are on the active leaf.
        // Now we go up until we find the parent node that is
        // aligned with the desired direction.
        let split_direction = match direction {
            PaneDirection::Left | PaneDirection::Right => SplitDirection::Horizontal,
            PaneDirection::Up | PaneDirection::Down => SplitDirection::Vertical,
            PaneDirection::Next | PaneDirection::Prev => unreachable!(),
        };
        let delta = match direction {
            PaneDirection::Down | PaneDirection::Right => amount as isize,
            PaneDirection::Up | PaneDirection::Left => -(amount as isize),
            PaneDirection::Next | PaneDirection::Prev => unreachable!(),
        };
        loop {
            match cursor.go_up() {
                Ok(mut c) => {
                    if let Ok(Some(node)) = c.node_mut() {
                        if node.direction == split_direction {
                            self.adjust_node_at_cursor(&mut c, delta);
                            self.cascade_size_from_cursor(c);
                            return;
                        }
                    }

                    cursor = c;
                }

                Err(c) => {
                    self.pane.replace(c.tree());
                    return;
                }
            }
        }
    }

    fn activate_pane_direction(&mut self, direction: PaneDirection) {
        if self.zoomed.is_some() {
            if !configuration().unzoom_on_switch_pane {
                return;
            }
            self.toggle_zoom();
        }
        if let Some(panel_idx) = self.get_pane_direction(direction, false) {
            self.set_active_idx(panel_idx);
        }
        let mux = Mux::get();
        if let Some(window_id) = mux.window_containing_tab(self.id) {
            mux.notify(MuxNotification::WindowInvalidated(window_id));
        }
    }

    fn get_pane_direction(&mut self, direction: PaneDirection, ignore_zoom: bool) -> Option<usize> {
        let panes = if ignore_zoom {
            self.iter_panes_ignoring_zoom()
        } else {
            self.iter_panes()
        };

        let active = match panes.iter().find(|pane| pane.is_active) {
            Some(p) => p,
            None => {
                // No active pane somehow...
                return Some(0);
            }
        };

        if matches!(direction, PaneDirection::Next | PaneDirection::Prev) {
            let max_pane_id = panes.iter().map(|p| p.index).max().unwrap_or(active.index);

            return Some(if direction == PaneDirection::Next {
                if active.index == max_pane_id {
                    0
                } else {
                    active.index + 1
                }
            } else {
                if active.index == 0 {
                    max_pane_id
                } else {
                    active.index - 1
                }
            });
        }

        let mut best = None;

        let recency = &self.recency;

        fn edge_intersects(
            active_start: usize,
            active_size: usize,
            current_start: usize,
            current_size: usize,
        ) -> bool {
            intersects_range(
                &(active_start..active_start + active_size),
                &(current_start..current_start + current_size),
            )
        }

        for pane in &panes {
            let score = match direction {
                PaneDirection::Right => {
                    if pane.left == active.left + active.width + 1
                        && edge_intersects(active.top, active.height, pane.top, pane.height)
                    {
                        1 + recency.score(pane.index)
                    } else {
                        0
                    }
                }
                PaneDirection::Left => {
                    if pane.left + pane.width + 1 == active.left
                        && edge_intersects(active.top, active.height, pane.top, pane.height)
                    {
                        1 + recency.score(pane.index)
                    } else {
                        0
                    }
                }
                PaneDirection::Up => {
                    if pane.top + pane.height + 1 == active.top
                        && edge_intersects(active.left, active.width, pane.left, pane.width)
                    {
                        1 + recency.score(pane.index)
                    } else {
                        0
                    }
                }
                PaneDirection::Down => {
                    if active.top + active.height + 1 == pane.top
                        && edge_intersects(active.left, active.width, pane.left, pane.width)
                    {
                        1 + recency.score(pane.index)
                    } else {
                        0
                    }
                }
                PaneDirection::Next | PaneDirection::Prev => unreachable!(),
            };

            if score > 0 {
                let target = match best.take() {
                    Some((best_score, best_pane)) if best_score > score => (best_score, best_pane),
                    _ => (score, pane),
                };
                best.replace(target);
            }
        }

        if let Some((_, target)) = best.take() {
            return Some(target.index);
        }
        None
    }

    fn prune_dead_panes(&mut self) -> bool {
        let mux = Mux::get();
        !self
            .remove_pane_if(
                |_, pane| {
                    // If the pane is no longer known to the mux, then its liveness
                    // state isn't guaranteed to be monitored or updated, so let's
                    // consider the pane effectively dead if it isn't in the mux.
                    // <https://github.com/wezterm/wezterm/issues/4030>
                    let in_mux = mux.get_pane(pane.pane_id()).is_some();
                    let dead = pane.is_dead();
                    log::trace!(
                        "prune_dead_panes: pane_id={} dead={} in_mux={}",
                        pane.pane_id(),
                        dead,
                        in_mux
                    );
                    dead || !in_mux
                },
                true,
            )
            .is_empty()
    }

    fn kill_pane(&mut self, pane_id: PaneId) -> bool {
        !self
            .remove_pane_if(|_, pane| pane.pane_id() == pane_id, true)
            .is_empty()
    }

    fn kill_panes_in_domain(&mut self, domain: DomainId) -> bool {
        !self
            .remove_pane_if(|_, pane| pane.domain_id() == domain, true)
            .is_empty()
    }

    fn remove_pane(&mut self, pane_id: PaneId) -> Option<Arc<dyn Pane>> {
        let panes = self.remove_pane_if(|_, pane| pane.pane_id() == pane_id, false);
        for pane in panes {
            return Some(pane);
        }
        None
    }

    fn remove_pane_if<F>(&mut self, f: F, kill: bool) -> Vec<Arc<dyn Pane>>
    where
        F: Fn(usize, &Arc<dyn Pane>) -> bool,
    {
        let mut dead_panes = vec![];
        let zoomed_pane = self.zoomed.as_ref().map(|p| p.pane_id());
        let prior = self.get_active_pane();

        {
            let root_size = self.size;
            let mut cursor = self.pane.take().unwrap().cursor();
            let mut pane_index = 0;
            let mut removed_indices = vec![];
            let cell_dims = self.cell_dimensions();

            loop {
                // Figure out the available size by looking at our immediate parent node.
                // If we are the root, look at the tab size
                let pane_size = if let Some((branch, Some(parent))) = cursor.path_to_root().next() {
                    if branch == PathBranch::IsRight {
                        parent.second
                    } else {
                        parent.first
                    }
                } else {
                    root_size
                };

                if cursor.is_leaf() {
                    let (removed, stack_became_empty) =
                        cursor
                            .leaf_mut()
                            .unwrap()
                            .remove_matching(pane_index, &f, zoomed_pane);
                    if !removed.is_empty() {
                        if removed
                            .iter()
                            .any(|pane| Some(pane.pane_id()) == zoomed_pane)
                        {
                            // If we removed the zoomed pane, un-zoom our state!
                            self.zoomed.take();
                        }
                        dead_panes.extend(removed);
                    }

                    if stack_became_empty {
                        removed_indices.push(pane_index);
                        let parent;
                        match cursor.unsplit_leaf() {
                            Ok((c, dead, p)) => {
                                dead_panes.extend(dead.panes);
                                parent = p.unwrap();
                                cursor = c;
                            }
                            Err(c) => {
                                // We might be the root, for example
                                if c.is_top() && c.is_leaf() {
                                    self.pane.replace(Tree::Empty);
                                } else {
                                    self.pane.replace(c.tree());
                                }
                                break;
                            }
                        };

                        // Now we need to increase the size of the current node
                        // and propagate the revised size to its children.
                        let size = TerminalSize {
                            rows: parent.height(),
                            cols: parent.width(),
                            pixel_width: cell_dims.pixel_width * parent.width(),
                            pixel_height: cell_dims.pixel_height * parent.height(),
                            dpi: cell_dims.dpi,
                        };

                        if let Some(stack) = cursor.leaf_mut() {
                            if let Err(err) = stack.resize(size) {
                                log::error!("failed to resize pane stack after removal: {err:#}");
                            }
                        } else {
                            self.apply_pane_size(size, &mut cursor);
                        }
                    } else if !dead_panes.is_empty() {
                        // Apply our revised size to the tty
                        if let Some(stack) = cursor.leaf_mut() {
                            if let Err(err) = stack.resize(pane_size) {
                                log::error!("failed to resize pane stack after removal: {err:#}");
                            }
                        }
                    }

                    pane_index += 1;
                } else if !dead_panes.is_empty() {
                    self.apply_pane_size(pane_size, &mut cursor);
                }
                match cursor.preorder_next() {
                    Ok(c) => cursor = c,
                    Err(c) => {
                        self.pane.replace(c.tree());
                        break;
                    }
                }
            }

            // Figure out which pane should now be active.
            // If panes earlier than the active pane were closed, then we
            // need to shift the active pane down
            let active_idx = self.active;
            removed_indices.retain(|&idx| idx <= active_idx);
            self.active = active_idx.saturating_sub(removed_indices.len());
        }

        if !dead_panes.is_empty() {
            self.advise_focus_change(prior);
        }

        if !dead_panes.is_empty() && kill {
            let to_kill: Vec<_> = dead_panes.iter().map(|p| p.pane_id()).collect();
            promise::spawn::spawn_into_main_thread(async move {
                let mux = Mux::get();
                for pane_id in to_kill.into_iter() {
                    mux.remove_pane(pane_id);
                }
            })
            .detach();
        }
        dead_panes
    }

    fn can_close_without_prompting(&mut self, reason: CloseReason) -> bool {
        let panes = self.iter_all_panes();
        for pane in &panes {
            if !pane.can_close_without_prompting(reason) {
                return false;
            }
        }
        true
    }

    fn is_dead(&mut self) -> bool {
        // Make sure we account for all panes, so that we don't
        // kill the whole tab if the zoomed pane is dead!
        let panes = self.iter_all_panes();
        let mut dead_count = 0;
        for pane in &panes {
            if pane.is_dead() {
                dead_count += 1;
            }
        }
        dead_count == panes.len()
    }

    fn get_active_pane(&mut self) -> Option<Arc<dyn Pane>> {
        if let Some(zoomed) = self.zoomed.as_ref() {
            return Some(Arc::clone(zoomed));
        }

        self.iter_panes_ignoring_zoom()
            .iter()
            .nth(self.active)
            .map(|p| Arc::clone(&p.pane))
    }

    fn get_active_idx(&self) -> usize {
        self.active
    }

    fn set_active_pane(&mut self, pane: &Arc<dyn Pane>) {
        self.set_active_pane_impl(pane, true);
    }

    fn set_active_pane_silent(&mut self, pane: &Arc<dyn Pane>) {
        self.set_active_pane_impl(pane, false);
    }

    fn set_active_pane_impl(&mut self, pane: &Arc<dyn Pane>, notify_focus: bool) {
        let prior = self.get_active_pane();

        if is_pane(pane, &prior.as_ref()) {
            return;
        }

        if self.zoomed.is_some() {
            if !configuration().unzoom_on_switch_pane {
                return;
            }
            self.toggle_zoom();
        }

        if let Some(pane_index) = self.activate_pane_in_stack_impl(pane.pane_id()) {
            self.active = pane_index;
            self.recency.tag(pane_index);
            self.advise_focus_change_impl(prior, notify_focus);
        }
    }

    fn activate_pane_in_stack(&mut self, pane_id: PaneId) -> anyhow::Result<usize> {
        if self.zoomed.is_some() {
            return self.activate_zoomed_pane_in_stack(pane_id);
        }

        let prior = self.get_active_pane();
        let pane_index = self
            .activate_pane_in_stack_impl(pane_id)
            .ok_or_else(|| anyhow::anyhow!("pane {} not found in tab", pane_id))?;
        self.active = pane_index;
        self.recency.tag(pane_index);
        // Switching the visible pane of a stack is a focus event, not a size
        // event: advise_focus_change already emits PaneFocused, which the GUI
        // uses to repaint. Emitting TabResized here made every level-2 tab
        // switch on a mux client tab round-trip through the server and kick
        // off a resync (resize/resync storm).
        self.advise_focus_change(prior);

        Ok(pane_index)
    }

    fn activate_zoomed_pane_in_stack(&mut self, pane_id: PaneId) -> anyhow::Result<usize> {
        let prior = self.get_active_pane();
        let Some(zoomed_pane_id) = prior.as_ref().map(|pane| pane.pane_id()) else {
            anyhow::bail!("cannot switch pane tab while zoomed");
        };
        let mut cursor = self.pane.take().unwrap().cursor();
        let mut pane_index = 0;
        let mut target = None;

        loop {
            if cursor.is_leaf() {
                let stack = cursor.leaf_mut().unwrap();
                if stack.contains_pane(zoomed_pane_id) {
                    if stack.set_active_pane(pane_id) {
                        target = stack.active_pane().map(|pane| (pane_index, pane));
                    }
                }
                pane_index += 1;
            }

            match cursor.preorder_next() {
                Ok(c) if target.is_none() => cursor = c,
                Ok(c) | Err(c) => {
                    self.pane.replace(c.tree());
                    break;
                }
            }
        }

        let Some((pane_index, target_pane)) = target else {
            anyhow::bail!("cannot switch pane tab while zoomed");
        };

        self.active = pane_index;
        self.recency.tag(pane_index);

        if !is_pane(&target_pane, &prior.as_ref()) {
            if let Some(prior) = prior.as_ref() {
                prior.set_zoomed(false);
            }
            target_pane.set_zoomed(true);
            if let Err(err) = target_pane.resize(self.size) {
                log::error!("failed to resize zoomed pane: {err:#}");
            }
        }
        self.zoomed.replace(target_pane);
        self.advise_focus_change(prior);
        Mux::try_get().map(|mux| mux.notify(MuxNotification::TabResized(self.id)));

        Ok(pane_index)
    }

    fn activate_pane_in_stack_impl(&mut self, pane_id: PaneId) -> Option<usize> {
        let mut cursor = self.pane.take().unwrap().cursor();
        let mut pane_index = 0;
        let mut found = None;

        loop {
            if cursor.is_leaf() {
                if cursor.leaf_mut().unwrap().set_active_pane(pane_id) {
                    found = Some(pane_index);
                }
                pane_index += 1;
            }

            match cursor.preorder_next() {
                Ok(c) if found.is_none() => cursor = c,
                Ok(c) | Err(c) => {
                    self.pane.replace(c.tree());
                    break;
                }
            }
        }

        found
    }

    fn advise_focus_change(&mut self, prior: Option<Arc<dyn Pane>>) {
        self.advise_focus_change_impl(prior, true);
    }

    fn advise_focus_change_impl(&mut self, prior: Option<Arc<dyn Pane>>, notify_focus: bool) {
        let mux = Mux::get();
        let current = self.get_active_pane();
        match (prior, current) {
            (Some(prior), Some(current)) if prior.pane_id() != current.pane_id() => {
                prior.focus_changed(false);
                current.focus_changed(true);
                if notify_focus {
                    mux.notify(MuxNotification::PaneFocused(current.pane_id()));
                }
            }
            (None, Some(current)) => {
                current.focus_changed(true);
                if notify_focus {
                    mux.notify(MuxNotification::PaneFocused(current.pane_id()));
                }
            }
            (Some(prior), None) => {
                prior.focus_changed(false);
            }
            (Some(_), Some(_)) | (None, None) => {
                // no change
            }
        }
    }

    fn set_active_idx(&mut self, pane_index: usize) {
        let prior = self.get_active_pane();
        self.active = pane_index;
        self.recency.tag(pane_index);
        self.advise_focus_change(prior);
    }

    fn add_pane_to_stack(
        &mut self,
        base_pane_id: PaneId,
        pane: Arc<dyn Pane>,
    ) -> anyhow::Result<usize> {
        if self.zoomed.is_some() {
            anyhow::bail!("cannot create pane tab while zoomed");
        }

        let prior = self.get_active_pane();
        let mut cursor = self.pane.take().unwrap().cursor();
        let mut pane_index = 0;
        let mut found = false;

        loop {
            if cursor.is_leaf() {
                let stack = cursor.leaf_mut().unwrap();
                if stack.contains_pane(base_pane_id) {
                    if let Some(base) = stack.active_pane() {
                        let dims = base.get_dimensions();
                        pane.resize(TerminalSize {
                            rows: dims.viewport_rows,
                            cols: dims.cols,
                            pixel_height: dims.pixel_height,
                            pixel_width: dims.pixel_width,
                            dpi: dims.dpi,
                        })?;
                    }
                    stack.push_and_activate(Arc::clone(&pane));
                    found = true;
                }
                if !found {
                    pane_index += 1;
                }
            }

            match cursor.preorder_next() {
                Ok(c) if !found => cursor = c,
                Ok(c) | Err(c) => {
                    self.pane.replace(c.tree());
                    break;
                }
            }
        }

        if !found {
            anyhow::bail!("pane {} not found in tab", base_pane_id);
        }

        self.active = pane_index;
        self.recency.tag(pane_index);
        self.advise_focus_change(prior);
        Mux::try_get().map(|mux| mux.notify(MuxNotification::TabResized(self.id)));

        Ok(pane_index)
    }

    fn assign_pane(&mut self, pane: &Arc<dyn Pane>) {
        match Tree::new()
            .cursor()
            .assign_top(PaneStack::new(Arc::clone(pane)))
        {
            Ok(c) => self.pane = Some(c.tree()),
            Err(_) => panic!("tried to assign root pane to non-empty tree"),
        }
    }

    fn cell_dimensions(&self) -> TerminalSize {
        cell_dimensions(&self.size)
    }

    fn swap_active_with_index(&mut self, pane_index: usize, keep_focus: bool) -> Option<()> {
        let active_idx = self.get_active_idx();
        let prior = self.get_active_pane()?;
        let mut stack = self.iter_stacks().into_iter().nth(active_idx)?;
        log::trace!(
            "swap_active_with_index: pane_index {} active {}",
            pane_index,
            active_idx
        );

        {
            let mut cursor = self.pane.take().unwrap().cursor();

            // locate the requested index
            match cursor.go_to_nth_leaf(pane_index) {
                Ok(c) => cursor = c,
                Err(c) => {
                    log::trace!("didn't find pane {pane_index}");
                    self.pane.replace(c.tree());
                    return None;
                }
            };

            std::mem::swap(&mut stack, cursor.leaf_mut().unwrap());

            // re-position to the root
            cursor = cursor.tree().cursor();

            // and now go and update the active idx
            match cursor.go_to_nth_leaf(active_idx) {
                Ok(c) => cursor = c,
                Err(c) => {
                    self.pane.replace(c.tree());
                    log::trace!("didn't find active {active_idx}");
                    return None;
                }
            };

            std::mem::swap(&mut stack, cursor.leaf_mut().unwrap());
            self.pane.replace(cursor.tree());

            // Advise the panes of their new sizes
            let size = self.size;
            if let Err(err) = apply_sizes_from_splits(self.pane.as_mut().unwrap(), &size) {
                log::error!("failed to resize panes after moving active pane: {err:#}");
            }
        }

        // And update focus
        if keep_focus {
            self.set_active_idx(pane_index);
        } else {
            self.advise_focus_change(Some(prior));
        }
        None
    }

    fn compute_split_size(
        &mut self,
        pane_index: usize,
        request: SplitRequest,
    ) -> Option<SplitDirectionAndSize> {
        let cell_dims = self.cell_dimensions();

        fn split_dimension(dim: usize, request: SplitRequest) -> (usize, usize) {
            let target_size = match request.size {
                SplitSize::Cells(n) => n,
                SplitSize::Percent(n) => (dim * (n as usize)) / 100,
            }
            .max(1);

            let remain = dim.saturating_sub(target_size + 1);

            if request.target_is_second {
                (remain, target_size)
            } else {
                (target_size, remain)
            }
        }

        if request.top_level {
            let size = self.size;

            let ((width1, width2), (height1, height2)) = match request.direction {
                SplitDirection::Horizontal => (
                    split_dimension(size.cols as usize, request),
                    (size.rows as usize, size.rows as usize),
                ),
                SplitDirection::Vertical => (
                    (size.cols as usize, size.cols as usize),
                    split_dimension(size.rows as usize, request),
                ),
            };

            return Some(SplitDirectionAndSize {
                direction: request.direction,
                first: TerminalSize {
                    rows: height1 as _,
                    cols: width1 as _,
                    pixel_height: cell_dims.pixel_height * height1,
                    pixel_width: cell_dims.pixel_width * width1,
                    dpi: cell_dims.dpi,
                },
                second: TerminalSize {
                    rows: height2 as _,
                    cols: width2 as _,
                    pixel_height: cell_dims.pixel_height * height2,
                    pixel_width: cell_dims.pixel_width * width2,
                    dpi: cell_dims.dpi,
                },
            });
        }

        // Ensure that we're not zoomed, otherwise we'll end up in
        // a bogus split state (https://github.com/wezterm/wezterm/issues/723)
        self.set_zoomed(false);

        self.iter_panes().iter().nth(pane_index).map(|pos| {
            let ((width1, width2), (height1, height2)) = match request.direction {
                SplitDirection::Horizontal => (
                    split_dimension(pos.width, request),
                    (pos.height, pos.height),
                ),
                SplitDirection::Vertical => {
                    ((pos.width, pos.width), split_dimension(pos.height, request))
                }
            };

            SplitDirectionAndSize {
                direction: request.direction,
                first: TerminalSize {
                    rows: height1 as _,
                    cols: width1 as _,
                    pixel_height: cell_dims.pixel_height * height1,
                    pixel_width: cell_dims.pixel_width * width1,
                    dpi: cell_dims.dpi,
                },
                second: TerminalSize {
                    rows: height2 as _,
                    cols: width2 as _,
                    pixel_height: cell_dims.pixel_height * height2,
                    pixel_width: cell_dims.pixel_width * width2,
                    dpi: cell_dims.dpi,
                },
            }
        })
    }

    fn split_and_insert(
        &mut self,
        pane_index: usize,
        request: SplitRequest,
        pane: Arc<dyn Pane>,
    ) -> anyhow::Result<usize> {
        if self.zoomed.is_some() {
            anyhow::bail!("cannot split while zoomed");
        }

        {
            let split_info = self
                .compute_split_size(pane_index, request)
                .ok_or_else(|| {
                    anyhow::anyhow!("invalid pane_index {}; cannot split!", pane_index)
                })?;

            let tab_size = self.size;
            if split_info.first.rows == 0
                || split_info.first.cols == 0
                || split_info.second.rows == 0
                || split_info.second.cols == 0
                || split_info.top_of_second() + split_info.second.rows > tab_size.rows
                || split_info.left_of_second() + split_info.second.cols > tab_size.cols
            {
                log::error!(
                    "No space for split!!! {:#?} height={} width={} top_of_second={} left_of_second={} tab_size={:?}",
                    split_info,
                    split_info.height(),
                    split_info.width(),
                    split_info.top_of_second(),
                    split_info.left_of_second(),
                    tab_size
                );
                anyhow::bail!("No space for split!");
            }

            let needs_resize = if request.top_level {
                self.pane.as_ref().unwrap().num_leaves() > 1
            } else {
                false
            };

            if needs_resize {
                // Pre-emptively resize the tab contents down to
                // match the target size; it's easier to reuse
                // existing resize logic that way
                if request.target_is_second {
                    self.resize(split_info.first.clone());
                } else {
                    self.resize(split_info.second.clone());
                }
            }

            let mut cursor = self.pane.take().unwrap().cursor();

            if request.top_level && !cursor.is_leaf() {
                let new_stack = PaneStack::new(Arc::clone(&pane));
                let result = if request.target_is_second {
                    cursor.split_node_and_insert_right(new_stack)
                } else {
                    cursor.split_node_and_insert_left(new_stack)
                };
                cursor = match result {
                    Ok(c) => {
                        cursor = match c.assign_node(Some(split_info)) {
                            Err(c) | Ok(c) => c,
                        };

                        self.pane.replace(cursor.tree());

                        let pane_index = if request.target_is_second {
                            self.pane.as_ref().unwrap().num_leaves().saturating_sub(1)
                        } else {
                            0
                        };

                        self.active = pane_index;
                        self.recency.tag(pane_index);
                        return Ok(pane_index);
                    }
                    Err(cursor) => cursor,
                };
            }

            match cursor.go_to_nth_leaf(pane_index) {
                Ok(c) => cursor = c,
                Err(c) => {
                    self.pane.replace(c.tree());
                    anyhow::bail!("invalid pane_index {}; cannot split!", pane_index);
                }
            };

            let existing_stack = cursor.leaf_mut().unwrap().clone();
            let new_stack = PaneStack::new(pane);

            let (stack1, stack2) = if request.target_is_second {
                (existing_stack, new_stack)
            } else {
                (new_stack, existing_stack)
            };

            stack1.resize(split_info.first)?;
            stack2.resize(split_info.second)?;

            *cursor.leaf_mut().unwrap() = stack1;

            match cursor.split_leaf_and_insert_right(stack2) {
                Ok(c) => cursor = c,
                Err(c) => {
                    self.pane.replace(c.tree());
                    anyhow::bail!("invalid pane_index {}; cannot split!", pane_index);
                }
            };

            // cursor now points to the newly created split node;
            // we need to populate its split information
            match cursor.assign_node(Some(split_info)) {
                Err(c) | Ok(c) => self.pane.replace(c.tree()),
            };

            if request.target_is_second {
                self.active = pane_index + 1;
                self.recency.tag(pane_index + 1);
            }
        }

        log::debug!("split info after split: {:#?}", self.iter_splits());
        log::debug!("pane info after split: {:#?}", self.iter_panes());

        Ok(if request.target_is_second {
            pane_index + 1
        } else {
            pane_index
        })
    }

    fn get_zoomed_pane(&self) -> Option<Arc<dyn Pane>> {
        self.zoomed.clone()
    }
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
    pub pane_id: PaneId,
    pub title: String,
    pub size: TerminalSize,
    pub working_dir: Option<SerdeUrl>,
    pub is_active_pane: bool,
    pub is_zoomed_pane: bool,
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

#[cfg(test)]
mod test {
    use super::*;
    use crate::renderable::*;
    use parking_lot::{MappedMutexGuard, Mutex};
    use rangeset::RangeSet;
    use std::ops::Range;
    use termwiz::surface::SequenceNo;
    use url::Url;
    use wezterm_term::color::ColorPalette;
    use wezterm_term::{KeyCode, KeyModifiers, Line, MouseEvent, StableRowIndex};

    struct FakePane {
        id: PaneId,
        size: Mutex<TerminalSize>,
    }

    impl FakePane {
        fn new(id: PaneId, size: TerminalSize) -> Arc<dyn Pane> {
            Arc::new(Self {
                id,
                size: Mutex::new(size),
            })
        }
    }

    impl Pane for FakePane {
        fn pane_id(&self) -> PaneId {
            self.id
        }

        fn get_cursor_position(&self) -> StableCursorPosition {
            StableCursorPosition::default()
        }

        fn get_current_seqno(&self) -> SequenceNo {
            unimplemented!();
        }

        fn get_changed_since(
            &self,
            _lines: Range<StableRowIndex>,
            _: SequenceNo,
        ) -> RangeSet<StableRowIndex> {
            unimplemented!();
        }

        fn with_lines_mut(
            &self,
            _stable_range: Range<StableRowIndex>,
            _with_lines: &mut dyn WithPaneLines,
        ) {
            unimplemented!();
        }

        fn for_each_logical_line_in_stable_range_mut(
            &self,
            _lines: Range<StableRowIndex>,
            _for_line: &mut dyn ForEachPaneLogicalLine,
        ) {
            unimplemented!();
        }

        fn get_lines(&self, _lines: Range<StableRowIndex>) -> (StableRowIndex, Vec<Line>) {
            unimplemented!();
        }

        fn get_logical_lines(&self, _lines: Range<StableRowIndex>) -> Vec<LogicalLine> {
            unimplemented!();
        }

        fn get_dimensions(&self) -> RenderableDimensions {
            let size = *self.size.lock();
            RenderableDimensions {
                cols: size.cols,
                viewport_rows: size.rows,
                scrollback_rows: size.rows,
                physical_top: 0,
                scrollback_top: 0,
                dpi: size.dpi,
                pixel_width: size.pixel_width,
                pixel_height: size.pixel_height,
                reverse_video: false,
            }
        }

        fn get_title(&self) -> String {
            format!("pane {}", self.id)
        }
        fn send_paste(&self, _text: &str) -> anyhow::Result<()> {
            unimplemented!()
        }
        fn reader(&self) -> anyhow::Result<Option<Box<dyn std::io::Read + Send>>> {
            Ok(None)
        }
        fn writer(&self) -> MappedMutexGuard<'_, dyn std::io::Write> {
            unimplemented!()
        }
        fn resize(&self, size: TerminalSize) -> anyhow::Result<()> {
            *self.size.lock() = size;
            Ok(())
        }

        fn key_down(&self, _key: KeyCode, _mods: KeyModifiers) -> anyhow::Result<()> {
            unimplemented!()
        }
        fn key_up(&self, _: KeyCode, _: KeyModifiers) -> anyhow::Result<()> {
            unimplemented!()
        }
        fn mouse_event(&self, _event: MouseEvent) -> anyhow::Result<()> {
            unimplemented!()
        }
        fn is_dead(&self) -> bool {
            false
        }
        fn palette(&self) -> ColorPalette {
            unimplemented!()
        }
        fn domain_id(&self) -> DomainId {
            1
        }
        fn is_mouse_grabbed(&self) -> bool {
            false
        }
        fn is_alt_screen_active(&self) -> bool {
            false
        }
        fn get_current_working_dir(&self, _policy: CachePolicy) -> Option<Url> {
            None
        }
    }

    #[test]
    fn tab_splitting() {
        let size = test_size();

        let tab = Tab::new(&size);
        tab.assign_pane(&FakePane::new(1, size));

        let panes = tab.iter_panes();
        assert_eq!(1, panes.len());
        assert_eq!(0, panes[0].index);
        assert_eq!(true, panes[0].is_active);
        assert_eq!(0, panes[0].left);
        assert_eq!(0, panes[0].top);
        assert_eq!(80, panes[0].width);
        assert_eq!(24, panes[0].height);

        assert!(tab
            .compute_split_size(
                1,
                SplitRequest {
                    direction: SplitDirection::Horizontal,
                    ..Default::default()
                }
            )
            .is_none());

        let horz_size = tab
            .compute_split_size(
                0,
                SplitRequest {
                    direction: SplitDirection::Horizontal,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(
            horz_size,
            SplitDirectionAndSize {
                direction: SplitDirection::Horizontal,
                second: TerminalSize {
                    rows: 24,
                    cols: 40,
                    pixel_width: 400,
                    pixel_height: 600,
                    dpi: 96,
                },
                first: TerminalSize {
                    rows: 24,
                    cols: 39,
                    pixel_width: 390,
                    pixel_height: 600,
                    dpi: 96,
                },
            }
        );

        let vert_size = tab
            .compute_split_size(
                0,
                SplitRequest {
                    direction: SplitDirection::Vertical,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(
            vert_size,
            SplitDirectionAndSize {
                direction: SplitDirection::Vertical,
                second: TerminalSize {
                    rows: 12,
                    cols: 80,
                    pixel_width: 800,
                    pixel_height: 300,
                    dpi: 96,
                },
                first: TerminalSize {
                    rows: 11,
                    cols: 80,
                    pixel_width: 800,
                    pixel_height: 275,
                    dpi: 96,
                }
            }
        );

        let new_index = tab
            .split_and_insert(
                0,
                SplitRequest {
                    direction: SplitDirection::Horizontal,
                    ..Default::default()
                },
                FakePane::new(2, horz_size.second),
            )
            .unwrap();
        assert_eq!(new_index, 1);

        let panes = tab.iter_panes();
        assert_eq!(2, panes.len());

        assert_eq!(0, panes[0].index);
        assert_eq!(false, panes[0].is_active);
        assert_eq!(0, panes[0].left);
        assert_eq!(0, panes[0].top);
        assert_eq!(39, panes[0].width);
        assert_eq!(24, panes[0].height);
        assert_eq!(390, panes[0].pixel_width);
        assert_eq!(600, panes[0].pixel_height);
        assert_eq!(1, panes[0].pane.pane_id());

        assert_eq!(1, panes[1].index);
        assert_eq!(true, panes[1].is_active);
        assert_eq!(40, panes[1].left);
        assert_eq!(0, panes[1].top);
        assert_eq!(40, panes[1].width);
        assert_eq!(24, panes[1].height);
        assert_eq!(400, panes[1].pixel_width);
        assert_eq!(600, panes[1].pixel_height);
        assert_eq!(2, panes[1].pane.pane_id());

        let vert_size = tab
            .compute_split_size(
                0,
                SplitRequest {
                    direction: SplitDirection::Vertical,
                    ..Default::default()
                },
            )
            .unwrap();
        let new_index = tab
            .split_and_insert(
                0,
                SplitRequest {
                    direction: SplitDirection::Vertical,
                    top_level: false,
                    target_is_second: true,
                    size: Default::default(),
                },
                FakePane::new(3, vert_size.second),
            )
            .unwrap();
        assert_eq!(new_index, 1);

        let panes = tab.iter_panes();
        assert_eq!(3, panes.len());

        assert_eq!(0, panes[0].index);
        assert_eq!(false, panes[0].is_active);
        assert_eq!(0, panes[0].left);
        assert_eq!(0, panes[0].top);
        assert_eq!(39, panes[0].width);
        assert_eq!(11, panes[0].height);
        assert_eq!(390, panes[0].pixel_width);
        assert_eq!(275, panes[0].pixel_height);
        assert_eq!(1, panes[0].pane.pane_id());

        assert_eq!(1, panes[1].index);
        assert_eq!(true, panes[1].is_active);
        assert_eq!(0, panes[1].left);
        assert_eq!(12, panes[1].top);
        assert_eq!(39, panes[1].width);
        assert_eq!(12, panes[1].height);
        assert_eq!(390, panes[1].pixel_width);
        assert_eq!(300, panes[1].pixel_height);
        assert_eq!(3, panes[1].pane.pane_id());

        assert_eq!(2, panes[2].index);
        assert_eq!(false, panes[2].is_active);
        assert_eq!(40, panes[2].left);
        assert_eq!(0, panes[2].top);
        assert_eq!(40, panes[2].width);
        assert_eq!(24, panes[2].height);
        assert_eq!(400, panes[2].pixel_width);
        assert_eq!(600, panes[2].pixel_height);
        assert_eq!(2, panes[2].pane.pane_id());

        tab.resize_split_by(1, 1);
        let panes = tab.iter_panes();
        assert_eq!(39, panes[0].width);
        assert_eq!(12, panes[0].height);
        assert_eq!(390, panes[0].pixel_width);
        assert_eq!(300, panes[0].pixel_height);

        assert_eq!(39, panes[1].width);
        assert_eq!(11, panes[1].height);
        assert_eq!(390, panes[1].pixel_width);
        assert_eq!(275, panes[1].pixel_height);

        assert_eq!(40, panes[2].width);
        assert_eq!(24, panes[2].height);
        assert_eq!(400, panes[2].pixel_width);
        assert_eq!(600, panes[2].pixel_height);
    }

    fn test_size() -> TerminalSize {
        TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 600,
            dpi: 96,
        }
    }

    struct MuxTestGuard;

    impl Drop for MuxTestGuard {
        fn drop(&mut self) {
            Mux::shutdown();
        }
    }

    fn install_mux() -> MuxTestGuard {
        Mux::set_mux(&Arc::new(Mux::new(None)));
        MuxTestGuard
    }

    #[test]
    fn pane_stack_add_switch_and_remove() {
        let _mux = install_mux();
        let size = test_size();
        let tab = Tab::new(&size);
        let pane_1 = FakePane::new(100, size);
        let pane_2 = FakePane::new(101, size);

        tab.assign_pane(&pane_1);
        assert_eq!(tab.add_pane_to_stack(100, Arc::clone(&pane_2)).unwrap(), 0);
        assert_eq!(tab.count_panes(), Some(2));
        assert_eq!(tab.pane_index_for_pane(100), Some(0));
        assert_eq!(tab.pane_index_for_pane(101), Some(0));

        let panes = tab.iter_panes();
        assert_eq!(panes.len(), 1);
        assert_eq!(panes[0].pane.pane_id(), 101);
        assert!(panes[0].is_active);

        let tabs = tab.pane_stack_tabs(100);
        assert_eq!(tabs.len(), 2);
        assert_eq!(tabs[0].pane_id, 100);
        assert!(!tabs[0].is_active);
        assert_eq!(tabs[1].pane_id, 101);
        assert!(tabs[1].is_active);

        tab.activate_pane_in_stack(100).unwrap();
        assert_eq!(tab.get_active_pane().unwrap().pane_id(), 100);

        let removed = tab.remove_pane(101).unwrap();
        assert_eq!(removed.pane_id(), 101);
        assert_eq!(tab.count_panes(), Some(1));
        assert_eq!(tab.get_active_pane().unwrap().pane_id(), 100);

        let pane_3 = FakePane::new(102, size);
        tab.add_pane_to_stack(100, Arc::clone(&pane_3)).unwrap();
        assert_eq!(tab.get_active_pane().unwrap().pane_id(), 102);

        let removed_active = tab.remove_pane(102).unwrap();
        assert_eq!(removed_active.pane_id(), 102);
        assert_eq!(tab.count_panes(), Some(1));
        assert_eq!(tab.get_active_pane().unwrap().pane_id(), 100);

        let pane_3 = FakePane::new(103, size);
        tab.add_pane_to_stack(100, Arc::clone(&pane_3)).unwrap();
        assert_eq!(tab.get_active_pane().unwrap().pane_id(), 103);

        tab.set_zoomed(true);
        assert_eq!(tab.get_zoomed_pane().unwrap().pane_id(), 103);
        assert!(tab
            .add_pane_to_stack(100, FakePane::new(104, size))
            .is_err());
        tab.activate_pane_in_stack(100).unwrap();
        assert_eq!(tab.get_active_pane().unwrap().pane_id(), 100);
        assert_eq!(tab.get_zoomed_pane().unwrap().pane_id(), 100);

        let panes = tab.iter_panes();
        assert_eq!(panes.len(), 1);
        assert_eq!(panes[0].pane.pane_id(), 100);
        assert!(panes[0].is_zoomed);
    }

    fn pane_entry(pane_id: PaneId, size: TerminalSize, is_active_pane: bool) -> PaneEntry {
        PaneEntry {
            window_id: 1,
            tab_id: 1,
            pane_id,
            title: format!("pane {pane_id}"),
            size,
            working_dir: None,
            is_active_pane,
            is_zoomed_pane: false,
            workspace: "default".to_string(),
            cursor_pos: StableCursorPosition::default(),
            physical_top: 0,
            top_row: 0,
            left_col: 0,
            tty_name: None,
        }
    }

    #[test]
    fn sync_with_pane_tree_preserves_stack_order_and_active_pane() {
        let size = test_size();
        let tab = Tab::new(&size);
        let root = PaneNode::Split {
            left: Box::new(PaneNode::Stack(PaneStackEntry {
                active: 1,
                panes: vec![pane_entry(200, size, false), pane_entry(201, size, true)],
                pane_stack_id: None,
            })),
            right: Box::new(PaneNode::Leaf(pane_entry(202, size, false))),
            node: SplitDirectionAndSize {
                direction: SplitDirection::Horizontal,
                first: size,
                second: size,
            },
        };

        tab.sync_with_pane_tree(size, root, |entry| FakePane::new(entry.pane_id, entry.size));

        let all_panes: Vec<_> = tab
            .iter_all_panes()
            .iter()
            .map(|pane| pane.pane_id())
            .collect();
        assert_eq!(all_panes, vec![200, 201, 202]);
        assert_eq!(tab.pane_index_for_pane(200), Some(0));
        assert_eq!(tab.pane_index_for_pane(201), Some(0));
        assert_eq!(tab.pane_index_for_pane(202), Some(1));

        let panes = tab.iter_panes();
        assert_eq!(panes.len(), 2);
        assert_eq!(panes[0].pane.pane_id(), 201);
        assert!(panes[0].is_active);
        assert_eq!(panes[1].pane.pane_id(), 202);
        assert!(!panes[1].is_active);

        let tabs = tab.pane_stack_tabs(200);
        assert_eq!(tabs.len(), 2);
        assert_eq!(tabs[0].pane_id, 200);
        assert!(!tabs[0].is_active);
        assert_eq!(tabs[1].pane_id, 201);
        assert!(tabs[1].is_active);
    }

    #[test]
    fn sync_with_pane_tree_does_not_overwrite_stack_active_tab() {
        let size = test_size();
        let tab = Tab::new(&size);
        let root = PaneNode::Stack(PaneStackEntry {
            active: 1,
            panes: vec![pane_entry(200, size, true), pane_entry(201, size, false)],
            pane_stack_id: None,
        });

        tab.sync_with_pane_tree(size, root, |entry| FakePane::new(entry.pane_id, entry.size));

        let panes = tab.iter_panes();
        assert_eq!(panes.len(), 1);
        assert_eq!(panes[0].pane.pane_id(), 201);
        assert!(panes[0].is_active);

        let tabs = tab.pane_stack_tabs(201);
        assert_eq!(tabs.len(), 2);
        assert_eq!(tabs[0].pane_id, 200);
        assert!(!tabs[0].is_active);
        assert_eq!(tabs[1].pane_id, 201);
        assert!(tabs[1].is_active);
    }

    #[test]
    fn sync_with_pane_tree_keeps_stack_id_stable_across_resyncs() {
        let size = test_size();
        let tab = Tab::new(&size);

        let make_root = || PaneNode::Stack(PaneStackEntry {
            active: 0,
            panes: vec![pane_entry(200, size, true), pane_entry(201, size, false)],
            pane_stack_id: Some(7),
        });

        tab.sync_with_pane_tree(size, make_root(), |entry| {
            FakePane::new(entry.pane_id, entry.size)
        });
        let first_id = tab.pane_stack_id(200).expect("stack id after first sync");
        assert_eq!(first_id, 7, "sync honors the id carried by the entry");

        // A resync with the same wire id must keep the same local id, so
        // GUI state keyed by pane_stack_id survives.
        tab.sync_with_pane_tree(size, make_root(), |entry| {
            FakePane::new(entry.pane_id, entry.size)
        });
        let second_id = tab.pane_stack_id(200).expect("stack id after resync");
        assert_eq!(first_id, second_id);

        // Entries without an id (older snapshots) still mint fresh ids.
        let unnamed = PaneNode::Stack(PaneStackEntry {
            active: 0,
            panes: vec![pane_entry(200, size, true), pane_entry(201, size, false)],
            pane_stack_id: None,
        });
        tab.sync_with_pane_tree(size, unnamed, |entry| {
            FakePane::new(entry.pane_id, entry.size)
        });
        assert!(tab.pane_stack_id(200).is_some());
    }

    fn is_send_and_sync<T: Send + Sync>() -> bool {
        true
    }

    #[test]
    fn tab_is_send_and_sync() {
        assert!(is_send_and_sync::<Tab>());
    }
}
