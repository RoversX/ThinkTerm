//! A pane opened by a split slides in from the side it was opened on, and
//! a pane closed goes back out the same way.
//!
//! Only the picture moves. By the time the new pane is first painted the
//! split has already given every pane its final size, once, so the programs
//! in them see a single resize rather than one a frame. The new pane is
//! painted where it ends up, recorded, and replayed shifted out towards the
//! edge it comes from, cropped to its own place, the divider beside it going
//! along. A closed pane is out of the tab, and its room given to the panes
//! beside it, by the time it is found missing: it is painted from last
//! frame's hold on it, where it stood, over them on its way out.
//!
//! Room given or taken along a pane's left or top moves the pane, and its
//! text with it, rather than only changing its size. Drawn where they stand
//! by then, those panes would have their text gone from where it was at the
//! first frame, and found across the window at the last. They are drawn
//! from where they stood instead, and go along with the slide.
//!
//! Nothing says a pane came by a split, wherever the split was asked for
//! (a key, the menu, the CLI, another client). It is read off the layout:
//! one pane more than last frame, in the same tab, with nobody gone, next
//! to a pane that used to cover the ground it now stands on. A closed pane
//! is read off it backwards: one pane fewer, nobody new, and a pane that
//! stood beside it covering its ground.
//!
//! A pane zoomed to the whole tab grows out of its place and pushes the
//! others out past the tab's edges, each the way it lies from it; let back
//! down, it shrinks into its place and they come back in. Here too the
//! zoom has given the pane its size by then, once: its text goes along
//! with its top left corner, the buttons at the end of its nav bar with its
//! right edge. The others keep theirs, and are only moved. Zoomed, they are
//! out of the tab's frames, and are painted from last frame's on their way
//! out.
//!
//! A pane carried to another place in the tab -- dropped beside another,
//! into another's stack or out of its own, swapped with another -- goes
//! there from where it stood, over the others, and the panes that make way
//! for it or take its room go to theirs at the same time. Each is drawn as
//! a zoomed one is, laid out once where it goes. Room none of them covers
//! yet shows a darker ground, the panes crossing it like cards. One that
//! goes out of sight into a stack stays where it stood, under them, as
//! last painted; one that comes out of a stack is where it stands. The
//! move is read off the layout too: the same panes in the tab, one of them
//! in another stack than it was.

use ::window::RectF;
use mux::pane::{Pane, PaneId};
use mux::tab::{PaneStackId, PositionedPane, PositionedSplit, SplitDirection, TabId};
use std::sync::Arc;
use std::time::{Duration, Instant};

const DURATION: Duration = Duration::from_millis(180);
/// Going out is quicker than coming in: by then the panes left are the
/// ones being looked at.
const LEAVE_DURATION: Duration = Duration::from_millis(140);
/// A slide is shown in no fewer frames than this. Where a frame takes
/// longer than its share of `DURATION` -- a busy window, a build without
/// optimisation -- the slide takes longer rather than skip most of its way:
/// timed on the clock alone it got one frame in, at a third of the way,
/// and was then found at its end.
pub(crate) const MIN_FRAMES: u32 = 8;
/// The first frame is drawn as if one had already passed, so the pane is
/// on its way in that frame rather than still all outside its place.
const FIRST_FRAME: Duration = Duration::from_micros(16_667);
/// A slide painted this many times and still not at its end is let go of.
/// Frames that are painted but never shown -- a pass that fails every time
/// -- would otherwise have it ask for frames for ever.
pub(crate) const MOST_PAINTS: u32 = 8 * MIN_FRAMES;

/// The edge of its place a new pane comes in from, or a closed one goes
/// out by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Edge {
    Left,
    Right,
    Top,
    Bottom,
}

impl Edge {
    /// The shift that puts a pane `out` of the way out through this edge
    /// of a place `width` by `height` pixels.
    fn shift(self, out: f32, width: f32, height: f32) -> (f32, f32) {
        match self {
            Edge::Right => (out * width, 0.),
            Edge::Left => (-out * width, 0.),
            Edge::Bottom => (0., out * height),
            Edge::Top => (0., -out * height),
        }
    }

    /// Whether `split` runs along the side of `pane` across from this edge:
    /// the divider its split put in.
    fn leads(self, pane: CellRect, split: &PositionedSplit) -> bool {
        let rows = split.top < pane.bottom() && pane.top < split.top + split.size;
        let cols = split.left < pane.right() && pane.left < split.left + split.size;
        match (self, split.direction) {
            (Edge::Right, SplitDirection::Horizontal) => split.left + 1 == pane.left && rows,
            (Edge::Left, SplitDirection::Horizontal) => split.left == pane.right() && rows,
            (Edge::Bottom, SplitDirection::Vertical) => split.top + 1 == pane.top && cols,
            (Edge::Top, SplitDirection::Vertical) => split.top == pane.bottom() && cols,
            _ => false,
        }
    }

    /// That divider, for a pane at `pane` that was one side of a split: it
    /// runs the whole of that side.
    fn divider(self, pane: CellRect) -> PositionedSplit {
        let (direction, left, top, size) = match self {
            Edge::Right => (
                SplitDirection::Horizontal,
                pane.left.saturating_sub(1),
                pane.top,
                pane.height,
            ),
            Edge::Left => (SplitDirection::Horizontal, pane.right(), pane.top, pane.height),
            Edge::Bottom => (
                SplitDirection::Vertical,
                pane.left,
                pane.top.saturating_sub(1),
                pane.width,
            ),
            Edge::Top => (SplitDirection::Vertical, pane.left, pane.bottom(), pane.width),
        };
        PositionedSplit {
            // Only ever painted, never dragged: no node of the tab's tree.
            index: 0,
            direction,
            left,
            top,
            size,
        }
    }

    /// The edge across from this one.
    fn opposite(self) -> Self {
        match self {
            Edge::Left => Edge::Right,
            Edge::Right => Edge::Left,
            Edge::Top => Edge::Bottom,
            Edge::Bottom => Edge::Top,
        }
    }

    /// Where the side of `rect` across from this edge is.
    fn far_side(self, rect: RectF) -> f32 {
        match self {
            Edge::Left => rect.max_x(),
            Edge::Right => rect.min_x(),
            Edge::Top => rect.max_y(),
            Edge::Bottom => rect.min_y(),
        }
    }

    /// The cells a pane that stood at `was` and stands at `now` moved by,
    /// along this edge: towards it, into the room of a pane going out
    /// through it, or away from it, giving room to one coming in by it.
    /// Room given or taken along the right or the bottom only changes the
    /// size of the panes beside it.
    fn moved(self, was: CellRect, now: CellRect, towards: bool) -> Option<usize> {
        let (was, now) = match self {
            Edge::Left => (was.left, now.left),
            Edge::Top => (was.top, now.top),
            Edge::Right | Edge::Bottom => return None,
        };
        let by = if towards {
            was.checked_sub(now)
        } else {
            now.checked_sub(was)
        };
        by.filter(|by| *by > 0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CellRect {
    left: usize,
    top: usize,
    width: usize,
    height: usize,
}

impl CellRect {
    fn of(pos: &PositionedPane) -> Self {
        Self {
            left: pos.left,
            top: pos.top,
            width: pos.width,
            height: pos.height,
        }
    }

    /// The cells a divider runs along.
    fn of_split(split: &PositionedSplit) -> Self {
        let (width, height) = match split.direction {
            SplitDirection::Horizontal => (1, split.size),
            SplitDirection::Vertical => (split.size, 1),
        };
        Self {
            left: split.left,
            top: split.top,
            width,
            height,
        }
    }

    fn right(&self) -> usize {
        self.left + self.width
    }

    fn bottom(&self) -> usize {
        self.top + self.height
    }

    fn overlaps(&self, other: &Self) -> bool {
        self.left < other.right()
            && other.left < self.right()
            && self.top < other.bottom()
            && other.top < self.bottom()
    }

    fn contains(&self, other: &Self) -> bool {
        self.left <= other.left
            && other.right() <= self.right()
            && self.top <= other.top
            && other.bottom() <= self.bottom()
    }

    fn union(&self, other: &Self) -> Self {
        let left = self.left.min(other.left);
        let top = self.top.min(other.top);
        Self {
            left,
            top,
            width: self.right().max(other.right()) - left,
            height: self.bottom().max(other.bottom()) - top,
        }
    }

    fn rows_overlap(&self, other: &Self) -> bool {
        self.top < other.bottom() && other.top < self.bottom()
    }

    fn cols_overlap(&self, other: &Self) -> bool {
        self.left < other.right() && other.left < self.right()
    }


    /// `pos` standing here instead.
    fn at(&self, pos: &PositionedPane) -> PositionedPane {
        let scaled = |pixels: usize, cells: usize, to: usize| pixels * to / cells.max(1);
        PositionedPane {
            left: self.left,
            top: self.top,
            width: self.width,
            height: self.height,
            pixel_width: scaled(pos.pixel_width, pos.width, self.width),
            pixel_height: scaled(pos.pixel_height, pos.height, self.height),
            ..pos.clone()
        }
    }
}

/// The panes a tab showed in one frame, each with the stack it is the
/// shown one of.
#[derive(Debug, PartialEq, Eq)]
pub struct TabLayout {
    tab_id: TabId,
    zoomed: bool,
    panes: Vec<(PaneId, CellRect, PaneStackId)>,
    active: Option<PaneId>,
}

impl TabLayout {
    pub fn of(tab_id: TabId, panes: &[PositionedPane]) -> Self {
        Self {
            tab_id,
            zoomed: panes.iter().any(|pos| pos.is_zoomed),
            panes: panes
                .iter()
                .map(|pos| (pos.pane.pane_id(), CellRect::of(pos), pos.pane_stack_id))
                .collect(),
            active: panes
                .iter()
                .find(|pos| pos.is_active)
                .map(|pos| pos.pane.pane_id()),
        }
    }

    fn rect(&self, pane_id: PaneId) -> Option<CellRect> {
        self.panes
            .iter()
            .find(|(id, _, _)| *id == pane_id)
            .map(|(_, rect, _)| *rect)
    }

    /// Whether one of its panes is the shown one of `stack`.
    fn shows(&self, stack: PaneStackId) -> bool {
        self.panes.iter().any(|(_, _, shown)| *shown == stack)
    }

    fn stack_of(&self, pane_id: PaneId) -> Option<PaneStackId> {
        self.panes
            .iter()
            .find(|(id, _, _)| *id == pane_id)
            .map(|(_, _, stack)| *stack)
    }

    /// The panes `self` has elsewhere than `before` had them, by one carried
    /// to another place in the tab: each with where it stood, the one
    /// carried first; and those `before` showed that went out of sight into
    /// a stack still shown, by their place in `before`. A carry puts panes
    /// out of sight or in sight within their stacks, or changes how the
    /// panes lie from one another: one that was beside another is now over
    /// it. What else moves panes -- a divider dragged, the window resized --
    /// leaves them lying as they did; what adds or takes away a pane has one
    /// new to the tab or one gone from it. Stack ids alone do not tell: a
    /// client's resync gives a stack of one pane a new one every time.
    fn rearranged(&self, before: &Self) -> Option<Rearranged> {
        if self.tab_id != before.tab_id || self.zoomed || before.zoomed {
            return None;
        }
        // Each shown on one side is shown, or out of sight in a stack
        // shown, on the other.
        let new = self
            .panes
            .iter()
            .any(|(id, _, stack)| before.rect(*id).is_none() && !before.shows(*stack));
        let gone = before
            .panes
            .iter()
            .any(|(id, _, stack)| self.rect(*id).is_none() && !self.shows(*stack));
        if new || gone {
            return None;
        }
        let mut moved: Vec<(PaneId, CellRect)> = self
            .panes
            .iter()
            .filter_map(|(id, now, _)| {
                let was = before.rect(*id)?;
                (was != *now).then_some((*id, was))
            })
            .collect();
        let hidden: Vec<usize> = before
            .panes
            .iter()
            .enumerate()
            .filter(|(_, (id, _, _))| self.rect(*id).is_none())
            .map(|(index, _)| index)
            .collect();
        // A pane out of sight or in sight in a stack counts where the pane
        // that stack shows on the other side is on its way: one carried in
        // or out. Another tab of a strip expanded only shows in turn.
        let moves = |pane_id: PaneId| moved.iter().any(|(id, _)| *id == pane_id);
        let shown_in = |layout: &Self, stack: PaneStackId| {
            layout
                .panes
                .iter()
                .find(|(_, _, shown)| *shown == stack)
                .map(|(id, _, _)| *id)
        };
        let carried_off = hidden
            .iter()
            .any(|index| shown_in(self, before.panes[*index].2).is_some_and(moves));
        let carried_in = self.panes.iter().any(|(id, _, stack)| {
            before.rect(*id).is_none() && shown_in(before, *stack).is_some_and(moves)
        });
        if moved.is_empty() || !(carried_off || carried_in || self.relaid(before)) {
            return None;
        }
        // The ones carried first, in another stack than they were; of two
        // swapped, the one the move left active. A move asked for from
        // outside the window may leave its active pane as it was, and come
        // with every stack of one pane renamed: then the one that went
        // furthest.
        moved.sort_by_key(|(id, was)| {
            let now = self.rect(*id).unwrap_or(*was);
            let (dx, dy) = (
                (now.left * 2 + now.width) as isize - (was.left * 2 + was.width) as isize,
                (now.top * 2 + now.height) as isize - (was.top * 2 + was.height) as isize,
            );
            (
                before.stack_of(*id) == self.stack_of(*id),
                Some(*id) != self.active,
                std::cmp::Reverse(dx * dx + dy * dy),
            )
        });
        let carried = moved
            .iter()
            .map(|(id, _)| *id)
            .filter(|id| before.stack_of(*id) != self.stack_of(*id))
            .collect();
        Some(Rearranged {
            moved,
            hidden,
            carried,
        })
    }

    /// Whether two panes shown on both sides lie otherwise from one another
    /// in `self` than in `before`: side by side then, one over the other
    /// now, or the other way round. Two that keep one way they lie -- left
    /// of the other both times, say -- have not: a divider dragged between
    /// two others can put a corner of one past a corner of the other.
    fn relaid(&self, before: &Self) -> bool {
        let lies = |rect: CellRect, other: CellRect| {
            [
                rect.right() <= other.left,
                other.right() <= rect.left,
                rect.bottom() <= other.top,
                other.bottom() <= rect.top,
            ]
        };
        self.panes.iter().enumerate().any(|(index, (id, now, _))| {
            self.panes[index + 1..].iter().any(|(other_id, other_now, _)| {
                match (before.rect(*id), before.rect(*other_id)) {
                    (Some(was), Some(other_was)) => {
                        let (then, now) = (lies(was, other_was), lies(*now, *other_now));
                        !then.iter().zip(&now).any(|(then, now)| *then && *now)
                    }
                    _ => false,
                }
            })
        })
    }

    /// The pane `self` gained over `before` by a split, the edge it comes
    /// in from and a pane that made room for it. Anything else that adds a
    /// pane -- a tab switched to, a zoom let go of, an overlay closed -- also
    /// changes or removes one that was there, or leaves no neighbour that
    /// gave up the room. A pane dragged out of its stack leaves another of
    /// the stack shown where it was: that one was in the tab all along.
    fn opened_by_split(&self, before: &Self) -> Option<(PaneId, Edge, PaneId)> {
        if self.tab_id != before.tab_id
            || self.zoomed
            || before.zoomed
            || before.panes.is_empty()
            || self.panes.len() != before.panes.len() + 1
        {
            return None;
        }
        let mut new = None;
        for (id, rect, stack) in &self.panes {
            if before.rect(*id).is_none() {
                if new.is_some() {
                    return None;
                }
                new = Some((*id, *rect, *stack));
            }
        }
        let (new_id, new, stack) = new?;
        if before.shows(stack) {
            return None;
        }
        self.panes.iter().find_map(|(id, now, _)| {
            let was = before.rect(*id)?;
            // It stood where the new pane is now, and gave that up.
            if was == *now || !was.overlaps(&new) {
                return None;
            }
            let edge = if new.left == now.right() + 1 && new.rows_overlap(now) {
                Edge::Right
            } else if new.right() + 1 == now.left && new.rows_overlap(now) {
                Edge::Left
            } else if new.top == now.bottom() + 1 && new.cols_overlap(now) {
                Edge::Bottom
            } else if new.bottom() + 1 == now.top && new.cols_overlap(now) {
                Edge::Top
            } else {
                return None;
            };
            Some((new_id, edge, *id))
        })
    }

    /// The pane `before` had that `self` lost by its closing, as its place
    /// in `before`, and the edge it goes out by: away from the panes that
    /// took its room. A tab switched away from or a zoom changes the tab or
    /// zooms it; an overlay puts another pane where one was. A pane dropped
    /// into another's stack hides that one behind it, in a stack still shown.
    fn closed(&self, before: &Self) -> Option<(usize, Edge)> {
        if self.tab_id != before.tab_id
            || self.zoomed
            || before.zoomed
            || self.panes.is_empty()
            || self.panes.len() + 1 != before.panes.len()
        {
            return None;
        }
        let mut gone = None;
        for (index, (id, rect, stack)) in before.panes.iter().enumerate() {
            if self.rect(*id).is_none() {
                if gone.is_some() {
                    return None;
                }
                gone = Some((index, *rect, *stack));
            }
        }
        let (index, gone, stack) = gone?;
        if self.shows(stack) {
            return None;
        }
        self.panes.iter().find_map(|(id, now, _)| {
            let was = before.rect(*id)?;
            // It stood beside the closed pane, and has its ground now.
            if was == *now || !now.overlaps(&gone) {
                return None;
            }
            let edge = if gone.left == was.right() + 1 && gone.rows_overlap(&was) {
                Edge::Right
            } else if gone.right() + 1 == was.left && gone.rows_overlap(&was) {
                Edge::Left
            } else if gone.top == was.bottom() + 1 && gone.cols_overlap(&was) {
                Edge::Bottom
            } else if gone.bottom() + 1 == was.top && gone.cols_overlap(&was) {
                Edge::Top
            } else {
                return None;
            };
            Some((index, edge))
        })
    }

    /// The pane zoomed to the whole tab between `before` and `self`, or let
    /// back down among the others, and which of the two: `true` zoomed. A
    /// tab switched to or from zoomed changes the tab; a pane alone in its
    /// tab already had all of it.
    fn zoom(&self, before: &Self) -> Option<(PaneId, bool)> {
        if self.tab_id != before.tab_id || self.zoomed == before.zoomed {
            return None;
        }
        let (zoomed, all) = if self.zoomed {
            (self, before)
        } else {
            (before, self)
        };
        let &[(pane_id, whole, _)] = zoomed.panes.as_slice() else {
            return None;
        };
        let place = all.rect(pane_id)?;
        (all.panes.len() > 1 && place != whole).then_some((pane_id, self.zoomed))
    }

    /// The cells its panes cover between them: the whole tab.
    fn bounds(&self) -> Option<CellRect> {
        self.panes
            .iter()
            .map(|(_, rect, _)| *rect)
            .reduce(|all, rect| all.union(&rect))
    }

    /// The panes that moved over between `before` and `self` for a slide
    /// through `edge` (see `Edge::moved`). The mux keeps the size of the
    /// first of two panes and puts what changes on the second, so they move
    /// as one block, all by as much.
    fn moved(&self, before: &Self, edge: Edge, towards: bool) -> Option<Moved> {
        let mut moved: Option<Moved> = None;
        for (id, now, _) in &self.panes {
            let Some(was) = before.rect(*id) else {
                continue;
            };
            let Some(by) = edge.moved(was, *now, towards) else {
                continue;
            };
            moved = match moved {
                None => Some(Moved { block: *now, by }),
                Some(moved) if moved.by == by => Some(Moved {
                    block: moved.block.union(now),
                    by,
                }),
                // Not one block: drawn where they stand.
                Some(_) => return None,
            };
        }
        moved
    }
}

/// Panes moved over for a slide, as the block they make where they stand
/// now, and the cells they moved by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Moved {
    block: CellRect,
    by: usize,
}

/// See `TabLayout::rearranged`.
#[derive(Debug, PartialEq, Eq)]
struct Rearranged {
    moved: Vec<(PaneId, CellRect)>,
    hidden: Vec<usize>,
    /// Those of `moved` in another stack than they were.
    carried: Vec<PaneId>,
}

/// A slide under way.
#[derive(Clone, Copy, Debug)]
pub struct SplitSlide {
    pub pane_id: PaneId,
    pub edge: Edge,
    /// A pane that made room for it. Until the new pane gets there, the
    /// room still looks like this one's.
    pub from_pane: PaneId,
    /// Where it ends up.
    rect: CellRect,
    /// The panes that moved over to make its room.
    moved: Option<Moved>,
    started: Instant,
    /// Frames shown with it so far.
    shown: u32,
    /// Times painted with it so far, shown or not.
    paints: u32,
}

impl SplitSlide {
    /// How much of the way it still has to go, 1 at the start and 0 on
    /// arrival; `None` once it is there.
    fn remaining(&self, now: Instant) -> Option<f32> {
        progress(self.started, self.shown, DURATION, now).map(|t| 1.0 - ease_out(t))
    }

    /// Whether `pos` moved over to make the room, and goes along.
    pub fn moves(&self, pos: &PositionedPane) -> bool {
        self.moved
            .is_some_and(|moved| moved.block.contains(&CellRect::of(pos)))
    }
}

/// A closed pane on its way out.
#[derive(Clone, Debug)]
pub struct Leaving<P = PositionedPane> {
    /// The pane as last painted, to paint it again from.
    pub pane: P,
    pub edge: Edge,
    /// Where it stood.
    rect: CellRect,
    /// The panes that moved over into its room.
    moved: Option<Moved>,
    /// Its nav bar showed its tab. One an overlay stood in for shows none.
    showed_tab: bool,
    tab_id: TabId,
    started: Instant,
    /// Frames shown with it so far.
    shown: u32,
    /// Times painted with it so far, shown or not.
    paints: u32,
}

impl<P> Leaving<P> {
    /// How far out it is, 0 at the start and 1 when gone; `None` once it is.
    fn out(&self, now: Instant) -> Option<f32> {
        progress(self.started, self.shown, LEAVE_DURATION, now).map(ease_in)
    }

    /// The divider it goes with: the one between it and the panes that have
    /// its room, which no longer has a place in the tab.
    pub fn divider(&self) -> PositionedSplit {
        self.edge.divider(self.rect)
    }

    /// Whether it stood over or under another pane, where its nav bar had
    /// a button to collapse the two.
    pub fn stood_in_a_column(&self) -> bool {
        matches!(self.edge, Edge::Top | Edge::Bottom)
    }

    /// Whether its nav bar showed its tab.
    pub fn showed_tab(&self) -> bool {
        self.showed_tab
    }
}

impl Leaving {
    /// Whether `pos` moved over into its room, and goes along.
    pub fn moves(&self, pos: &PositionedPane) -> bool {
        self.moved
            .is_some_and(|moved| moved.block.contains(&CellRect::of(pos)))
    }
}

/// A pane zoomed to the whole tab, or let back down into its place, on its
/// way there.
#[derive(Clone, Debug)]
pub struct Zooming<P = PositionedPane> {
    pub pane_id: PaneId,
    /// Zoomed, rather than let back down.
    zoomed: bool,
    /// Its place among the others.
    place: CellRect,
    /// The whole tab.
    whole: CellRect,
    /// Zoomed, the others and the dividers between them are out of the
    /// tab's frames: as last painted, to paint them on their way out from.
    pub others: Vec<P>,
    pub dividers: Vec<PositionedSplit>,
    tab_id: TabId,
    started: Instant,
    /// Frames shown with it so far.
    shown: u32,
    /// Times painted with it so far, shown or not.
    paints: u32,
}

impl<P> Zooming<P> {
    /// How far from its place towards the whole tab it is drawn, from 0 to
    /// 1; `None` once it is where it goes.
    fn toward_whole(&self, now: Instant) -> Option<f32> {
        progress(self.started, self.shown, DURATION, now)
            .map(ease_out)
            .map(|t| if self.zoomed { t } else { 1.0 - t })
    }

    /// Have it set out from `toward_whole` of the way there a frame before
    /// `now`: from where one the other way, taken back on its way, had got
    /// to, rather than from its start.
    fn set_out_from(&mut self, toward_whole: f32, now: Instant) {
        // `toward_whole` played backwards.
        let left = if self.zoomed {
            1.0 - toward_whole
        } else {
            toward_whole
        };
        let t = (1.0 - left.clamp(0.0, 1.0).sqrt()).clamp(0.0, 1.0);
        self.started = now.checked_sub(DURATION.mul_f32(t)).unwrap_or(now);
        self.shown = (t * MIN_FRAMES as f32).ceil() as u32;
    }
}

impl Zooming {
    /// `pos`, the pane on its way, standing in its place among the others.
    pub fn at_place(&self, pos: &PositionedPane) -> PositionedPane {
        self.place.at(pos)
    }

    /// `pos` standing over the whole tab.
    pub fn at_whole(&self, pos: &PositionedPane) -> PositionedPane {
        self.whole.at(pos)
    }
}

/// Panes rearranged by one carried to another place in the tab, on their
/// way from where they stood.
#[derive(Clone, Debug)]
pub struct Rearranging<P = PositionedPane> {
    /// The panes on their way and where each stood, the one carried first.
    moved: Vec<(PaneId, CellRect)>,
    /// Those gone out of sight into a stack, as last painted, where they
    /// stood: under the others until they are covered.
    pub hidden: Vec<P>,
    /// Those of them whose nav bar showed no tab.
    bare: Vec<PaneId>,
    /// The panes on their way into another stack, each with the tabs its
    /// nav bar showed and where the first of them stood, if it was painted:
    /// it goes from those to its new stack's on the way.
    carried: Vec<(PaneId, Option<f32>, Vec<PaneId>)>,
    tab_id: TabId,
    started: Instant,
    /// Frames shown with it so far.
    shown: u32,
    /// Times painted with it so far, shown or not.
    paints: u32,
}

impl<P> Rearranging<P> {
    /// How far from where they stood to where they stand the panes are
    /// drawn, from 0 to 1; `None` once they are there.
    fn along(&self, now: Instant) -> Option<f32> {
        progress(self.started, self.shown, DURATION, now).map(ease_out)
    }

    /// The panes on their way, the one carried first.
    pub fn moving(&self) -> impl Iterator<Item = PaneId> + '_ {
        self.moved.iter().map(|(pane_id, _)| *pane_id)
    }

    /// Whether the nav bar of `pane_id`, out of sight now, showed its tab.
    pub fn showed_tab(&self, pane_id: PaneId) -> bool {
        !self.bare.contains(&pane_id)
    }

    /// The tabs the nav bar of `pane_id` showed, and where the first stood
    /// along it, if it is on its way into another stack.
    pub fn tabs_before(&self, pane_id: PaneId) -> Option<(Option<f32>, &[PaneId])> {
        self.carried
            .iter()
            .find(|(carried, _, _)| *carried == pane_id)
            .map(|(_, lead, tabs)| (*lead, tabs.as_slice()))
    }
}

impl Rearranging {
    /// Whether `pos` is on its way.
    pub fn moves(&self, pos: &PositionedPane) -> bool {
        self.moving().any(|pane_id| pane_id == pos.pane.pane_id())
    }

    /// `pos` standing where it stood, if it is on its way.
    pub fn was(&self, pos: &PositionedPane) -> Option<PositionedPane> {
        self.moved
            .iter()
            .find(|(pane_id, _)| *pane_id == pos.pane.pane_id())
            .map(|(_, was)| was.at(pos))
    }
}

/// How far through a slide lasting `duration` is, from 0 to 1, in the
/// frame after `shown` frames; `None` once it is over.
pub(crate) fn progress(
    started: Instant,
    shown: u32,
    duration: Duration,
    now: Instant,
) -> Option<f32> {
    let elapsed = now.saturating_duration_since(started) + FIRST_FRAME;
    let by_clock = elapsed.as_secs_f32() / duration.as_secs_f32();
    let by_frames = (shown + 1) as f32 / MIN_FRAMES as f32;
    let t = by_clock.min(by_frames);
    (t < 1.0).then_some(t)
}

/// Fast, then settling. Squared rather than cubed: cubed, the first two
/// frames covered two thirds of the way and the rest was barely seen to move.
fn ease_out(t: f32) -> f32 {
    1.0 - (1.0 - t.clamp(0.0, 1.0)).powi(2)
}

/// Slow, then quick: `ease_out` played backwards.
fn ease_in(t: f32) -> f32 {
    t.clamp(0.0, 1.0).powi(2)
}

/// `rect` cut to `area`; nothing, at its corner, where they do not meet.
fn within(rect: RectF, area: RectF) -> RectF {
    rect.intersection(&area)
        .unwrap_or_else(|| euclid::rect(rect.min_x(), rect.min_y(), 0., 0.))
}

/// The shift along one axis, at most the whole of `place` that way.
fn clamped(place: RectF, shift: (f32, f32)) -> (f32, f32) {
    (
        shift.0.clamp(-place.width(), place.width()),
        shift.1.clamp(-place.height(), place.height()),
    )
}

/// What of `place` a pane shifted by `shift` still covers.
fn cover(place: RectF, shift: (f32, f32)) -> RectF {
    let (dx, dy) = clamped(place, shift);
    euclid::rect(
        place.min_x() + dx.max(0.),
        place.min_y() + dy.max(0.),
        place.width() - dx.abs(),
        place.height() - dy.abs(),
    )
}

/// What of `place` a pane shifted by `shift` has yet to cover.
fn wake(place: RectF, shift: (f32, f32)) -> RectF {
    let (dx, dy) = clamped(place, shift);
    if dx > 0. {
        euclid::rect(place.min_x(), place.min_y(), dx, place.height())
    } else if dx < 0. {
        euclid::rect(place.max_x() + dx, place.min_y(), -dx, place.height())
    } else if dy > 0. {
        euclid::rect(place.min_x(), place.min_y(), place.width(), dy)
    } else if dy < 0. {
        euclid::rect(place.min_x(), place.max_y() + dy, place.width(), -dy)
    } else {
        euclid::rect(place.min_x(), place.min_y(), 0., 0.)
    }
}

/// What of `area` lies past `rect` from `edge`: the far side of a pane on
/// a slide through `edge`, where the panes it comes up against are.
fn beyond(edge: Edge, rect: RectF, area: RectF) -> RectF {
    let (mut left, mut top, mut right, mut bottom) =
        (area.min_x(), area.min_y(), area.max_x(), area.max_y());
    match edge {
        Edge::Left => left = left.max(rect.max_x()),
        Edge::Right => right = right.min(rect.min_x()),
        Edge::Top => top = top.max(rect.max_y()),
        Edge::Bottom => bottom = bottom.min(rect.min_y()),
    }
    euclid::rect(left, top, (right - left).max(0.), (bottom - top).max(0.))
}

/// `t` of the way from `from` to `to`.
fn lerp(from: f32, to: f32, t: f32) -> f32 {
    from + (to - from) * t
}

/// `t` of the way from `from` to `to`, each side.
fn between(from: RectF, to: RectF, t: f32) -> RectF {
    let left = lerp(from.min_x(), to.min_x(), t);
    let top = lerp(from.min_y(), to.min_y(), t);
    euclid::rect(
        left,
        top,
        lerp(from.max_x(), to.max_x(), t) - left,
        lerp(from.max_y(), to.max_y(), t) - top,
    )
}

fn translated(rect: RectF, (dx, dy): (f32, f32)) -> RectF {
    euclid::rect(rect.min_x() + dx, rect.min_y() + dy, rect.width(), rect.height())
}

/// The rows of `rect` from `top` to `bottom`.
fn rows(rect: RectF, top: f32, bottom: f32) -> RectF {
    let top = top.clamp(rect.min_y(), rect.max_y());
    let bottom = bottom.clamp(top, rect.max_y());
    euclid::rect(rect.min_x(), top, rect.width(), bottom - top)
}

/// The columns of `rect` from `left` to `right`.
fn cols(rect: RectF, left: f32, right: f32) -> RectF {
    let left = left.clamp(rect.min_x(), rect.max_x());
    let right = right.clamp(left, rect.max_x());
    euclid::rect(left, rect.min_y(), right - left, rect.height())
}

/// Less than this across is left out of what `without` leaves: what float
/// sums put between two rects that meet.
const SLIVER: f32 = 0.5;

/// What of `rects` lies outside `cut`, as rects that do not overlap.
fn without(rects: Vec<RectF>, cut: RectF) -> Vec<RectF> {
    let mut left = Vec::with_capacity(rects.len());
    for rect in rects {
        let Some(inside) = rect.intersection(&cut).filter(|inside| !inside.is_empty()) else {
            left.push(rect);
            continue;
        };
        // Above and below it all across, beside it the rows between.
        let beside = rows(rect, inside.min_y(), inside.max_y());
        left.extend(
            [
                rows(rect, rect.min_y(), inside.min_y()),
                rows(rect, inside.max_y(), rect.max_y()),
                cols(beside, rect.min_x(), inside.min_x()),
                cols(beside, inside.max_x(), rect.max_x()),
            ]
            .iter()
            .copied()
            .filter(|piece| piece.width() >= SLIVER && piece.height() >= SLIVER),
        );
    }
    left
}

/// Where a pane stands: its ground with its nav bar, and the top left
/// corner of its cells, which its text is laid out from.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Placed {
    pub rect: RectF,
    pub origin: (f32, f32),
}

/// The nav bar of a pane on its way: the band it runs along where the pane
/// stands, where along that the buttons at its end start, and how high the
/// band stands where the pane is drawn from and where it goes -- for a zoom,
/// its place and the whole tab. A terminal bar along the top lifts the bands
/// of the panes along the tab's top edge, so a band need not go along with
/// the pane's text.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Nav {
    pub band: RectF,
    pub buttons: f32,
    pub top_from: f32,
    pub top_to: f32,
}

/// A pane on its way in a rearrangement: where it stood and where it stands,
/// as drawn, and its nav bar.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MovingPane {
    pane_id: PaneId,
    from: Placed,
    to: Placed,
    nav: Option<Nav>,
}

impl MovingPane {
    /// `pos`, drawn `from` where it stood (see `Rearranging::was`) `to`
    /// where it stands.
    pub fn new(pos: &PositionedPane, from: Placed, to: Placed, nav: Option<Nav>) -> Self {
        Self {
            pane_id: pos.pane.pane_id(),
            from,
            to,
            nav,
        }
    }
}

/// The pieces a pane painted where it `stands` is drawn in at `now`: its
/// text and ground shifted by `shift`, its nav bar's band at the height
/// given with it, its tabs with its left edge and the buttons at the end
/// with its right. With them, what of `now` they leave bare, to be filled
/// like the pane's own room, and how far down the band is drawn there.
fn pieces(
    stands: RectF,
    now: RectF,
    shift: (f32, f32),
    nav: Option<(Nav, f32)>,
    area: RectF,
) -> (Vec<Drawn>, Vec<RectF>, f32) {
    // What of the pane as painted a piece drawn so shifted covers.
    let covered = |shift| translated(stands, shift);
    let mut pieces = Vec::with_capacity(3);
    let mut room = vec![now];
    let mut nav_shift = shift.1;
    match nav {
        Some((nav, top)) => {
            nav_shift = top - nav.band.min_y();
            let bar = rows(now, top, top + nav.band.height());
            let end_shift = now.max_x() - within(stands, area).max_x();
            let ends = cols(bar, nav.buttons + end_shift, bar.max_x());
            // Its tabs up to them, short of where they stand in it.
            let title = cols(bar, bar.min_x(), nav.buttons + shift.0.min(end_shift));
            // Its text under it, none of the bar with it.
            let text = rows(
                now,
                bar.max_y().max(nav.band.max_y() + shift.1),
                now.max_y(),
            );
            for (shift, clip) in [
                ((shift.0, nav_shift), title),
                ((end_shift, nav_shift), ends),
                (shift, text),
            ] {
                if let Some(drawn) = clip.intersection(&covered(shift)) {
                    room = without(room, drawn);
                }
                pieces.push(Drawn { shift, clip });
            }
        }
        None => {
            pieces.push(Drawn { shift, clip: now });
            room = without(room, covered(shift));
        }
    }
    pieces.retain(|drawn| !drawn.clip.is_empty());
    (pieces, room, nav_shift)
}

/// What of `rects` none of `over` covers.
fn cut(rects: Vec<RectF>, over: &[RectF]) -> Vec<RectF> {
    over.iter().fold(rects, |rects, over| without(rects, *over))
}

/// `pieces`, cut short of what is drawn over them.
fn under(pieces: Vec<Drawn>, over: &[RectF]) -> Vec<Drawn> {
    pieces
        .into_iter()
        .flat_map(|drawn| {
            cut(vec![drawn.clip], over)
                .into_iter()
                .map(move |clip| Drawn {
                    shift: drawn.shift,
                    clip,
                })
        })
        .collect()
}

/// One of the panes a zoom pushes out of its way: in cells, which tell the
/// side of the pane zoomed it lies on, and where it stands.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OtherPane {
    pane_id: PaneId,
    cells: CellRect,
    rect: RectF,
}

impl OtherPane {
    pub fn of(pos: &PositionedPane, rect: RectF) -> Self {
        Self {
            pane_id: pos.pane.pane_id(),
            cells: CellRect::of(pos),
            rect,
        }
    }
}

/// How far a zoom on its way pushes the panes beside the pane zoomed, by
/// the side of its place they lie past: as far as that side of it has gone.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Push {
    /// The pane's place, in cells and where it is drawn.
    place: CellRect,
    from: RectF,
    /// Where it is drawn now.
    now: RectF,
    /// The whole tab, that nothing is drawn outside of.
    bounds: RectF,
}

impl Push {
    /// Which sides of the pane's place `cells` lies past: left, right,
    /// above, below. Told by cells, as the ground of a pane above another
    /// can run on under the other's nav bar where the window is padded.
    fn sides(&self, cells: CellRect) -> (bool, bool, bool, bool) {
        let place = self.place;
        (
            cells.right() <= place.left,
            cells.left >= place.right(),
            cells.bottom() <= place.top,
            cells.top >= place.bottom(),
        )
    }

    /// The shift for something past these sides of the pane's place.
    fn shift(&self, (left, right, above, below): (bool, bool, bool, bool)) -> Option<(f32, f32)> {
        let dx = if left {
            self.now.min_x() - self.from.min_x()
        } else if right {
            self.now.max_x() - self.from.max_x()
        } else {
            0.
        };
        let dy = if above {
            self.now.min_y() - self.from.min_y()
        } else if below {
            self.now.max_y() - self.from.max_y()
        } else {
            0.
        };
        (left || right || above || below).then_some((dx, dy))
    }

    /// How a pane in `cells` is drawn: pushed, and cut to the side of the
    /// pane zoomed it lies on, clear of it.
    fn pane(&self, cells: CellRect) -> Option<Drawn> {
        let sides = self.sides(cells);
        let shift = self.shift(sides)?;
        let (left, right, above, below) = sides;
        let mut clip = self.bounds;
        if left {
            clip = cols(clip, clip.min_x(), self.now.min_x());
        } else if right {
            clip = cols(clip, self.now.max_x(), clip.max_x());
        }
        if above {
            clip = rows(clip, clip.min_y(), self.now.min_y());
        } else if below {
            clip = rows(clip, self.now.max_y(), clip.max_y());
        }
        Some(Drawn { shift, clip })
    }

    /// How a divider running along `rect` is drawn: pushed with the panes
    /// beside it. Its line runs along the edge of the pane past it, as it
    /// does standing still, over the pane zoomed where that is the one.
    fn divider(&self, rect: CellRect) -> Option<Drawn> {
        let shift = self.shift(self.sides(rect))?;
        Some(Drawn {
            shift,
            clip: self.bounds,
        })
    }
}

/// How a pane or a divider is drawn in a frame of a slide: shifted by
/// `shift` from where it stands, and cut to `clip`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Drawn {
    pub shift: (f32, f32),
    pub clip: RectF,
}

/// What one frame of a slide draws otherwise than where it stands. What
/// it moves or cuts stays clear of everything else, the sliding pane
/// included: the quad layers within a z-index are one order for the whole
/// window, so of two panes drawn over one another, the text of the one
/// beneath would show through the ground of the other.
#[derive(Debug, Default)]
pub struct SlideFrame {
    /// The pane coming in.
    arriving: Option<(PaneId, Drawn)>,
    /// The divider coming in with it, by the edge it comes in from and its
    /// place.
    leading: Option<(Edge, CellRect, Drawn)>,
    /// The closed pane on its way out, and the divider going with it.
    pub leaving: Option<(Drawn, Drawn)>,
    /// The panes moved over for the slide, and the dividers between them:
    /// one block, up against the far side of the pane on its way.
    block: Option<(CellRect, Drawn)>,
    /// Where a closed pane stood: the panes and dividers that reach into
    /// it are cut short of what of it it still covers.
    room: Option<(CellRect, Drawn)>,
    /// Room no pane covers in this frame, and the pane to fill it like.
    pub fills: Vec<(PaneId, RectF)>,
    /// How far down from where it stands the nav bar of the pane a fill is
    /// like is drawn, its band in the fill with it.
    pub fills_shift: f32,
    /// Room between panes pushed apart, and the pane whose ground fills it.
    pub grounds: Vec<(PaneId, RectF)>,
    /// A pane zoomed or let back down on its way, in the pieces that go
    /// their own ways.
    zooming: Option<(PaneId, Vec<Drawn>)>,
    /// The panes it pushes out of its way, and the dividers between them.
    pushed: Vec<(PaneId, Drawn)>,
    push: Option<Push>,
    /// Panes rearranged on their way, in their pieces, and those under them
    /// they cross, cut short of them.
    moving: Vec<(PaneId, Vec<Drawn>)>,
    /// Room of a pane on its way that its pieces leave bare, and how far
    /// down its nav bar is drawn there.
    pub rooms: Vec<(PaneId, RectF, f32)>,
    /// Room no pane covers yet, in the darker ground.
    pub backdrop: Vec<RectF>,
    /// The lines along the panes on their way, in the dividers' colour.
    pub edges: Vec<RectF>,
    /// Where the dividers are drawn while panes are rearranged: clear of
    /// the panes on their way, and of where each stood and goes.
    dividers: Option<Vec<RectF>>,
}

impl SlideFrame {
    /// `slide` coming into `place`, `remaining` of the way still to go;
    /// `moving` where each pane that moved over for it stands, `cell` the
    /// size of a cell. Nothing is drawn outside `area`.
    pub fn coming(
        slide: &SplitSlide,
        remaining: f32,
        place: RectF,
        moving: &[(PaneId, RectF)],
        cell: (f32, f32),
        area: RectF,
    ) -> Self {
        let place = within(place, area);
        let shift = slide.edge.shift(remaining, place.width(), place.height());
        let arriving = Drawn { shift, clip: place };
        let mut frame = Self {
            arriving: Some((slide.pane_id, arriving)),
            leading: Some((slide.edge, slide.rect, arriving)),
            // Its room is the pane's that made it until it gets there.
            fills: vec![(slide.from_pane, wake(place, shift))],
            ..Self::default()
        };
        if let Some(pushed) = slide.moved.filter(|_| !moving.is_empty()) {
            // Up against its far side, from where they stood. They move by
            // whole cells, the room by the half cells either side of it as
            // well: what of it is left between them is filled, and nothing
            // they cover, or a see-through ground would be laid on twice.
            let back = slide.edge.shift(
                remaining,
                pushed.by as f32 * cell.0,
                pushed.by as f32 * cell.1,
            );
            let room = moving
                .iter()
                .fold(place, |room, (_, rect)| room.union(&within(*rect, area)));
            frame.block = Some((
                pushed.block,
                Drawn {
                    shift: back,
                    clip: beyond(slide.edge, cover(place, shift), room),
                },
            ));
            frame.leading = Some((slide.edge, slide.rect, Drawn { shift, clip: room }));
            frame.fills = vec![(
                slide.from_pane,
                beyond(slide.edge.opposite(), wake(place, back), wake(place, shift)),
            )];
            // Behind them, room that is theirs and they are not in yet, along
            // the block's far side: one further in has another of them there.
            let far = slide.edge.far_side(room);
            frame.fills.extend(
                moving
                    .iter()
                    .map(|(pane_id, rect)| (*pane_id, within(*rect, area)))
                    .filter(|(_, rect)| (slide.edge.far_side(*rect) - far).abs() < 0.5)
                    .map(|(pane_id, rect)| (pane_id, wake(rect, back))),
            );
        }
        frame
    }

    /// `leaving` going out of `place`, `out` of the way gone; `moving` where
    /// each pane that moved over into its room stands, `cell` the size of a
    /// cell. Nothing is drawn outside `area`.
    pub fn going<P>(
        leaving: &Leaving<P>,
        out: f32,
        place: RectF,
        moving: &[(PaneId, RectF)],
        cell: (f32, f32),
        area: RectF,
    ) -> Self {
        let place = within(place, area);
        let shift = leaving.edge.shift(out, place.width(), place.height());
        let covered = cover(place, shift);
        let mut frame = Self {
            // Its divider runs along its side, just outside its place.
            leaving: Some((Drawn { shift, clip: place }, Drawn { shift, clip: area })),
            ..Self::default()
        };
        match leaving.moved.filter(|_| !moving.is_empty()) {
            // From where they stood, up against it.
            Some(pushed) => {
                let (dx, dy) = leaving.edge.shift(
                    1.0 - out,
                    pushed.by as f32 * cell.0,
                    pushed.by as f32 * cell.1,
                );
                let room = moving
                    .iter()
                    .fold(place, |room, (_, rect)| room.union(&within(*rect, area)));
                frame.block = Some((
                    pushed.block,
                    Drawn {
                        shift: (-dx, -dy),
                        clip: beyond(leaving.edge, covered, room),
                    },
                ));
            }
            // Where they stand, cut short of it.
            None => {
                frame.room = Some((
                    leaving.rect,
                    Drawn {
                        shift: (0., 0.),
                        clip: beyond(leaving.edge, covered, area),
                    },
                ));
            }
        }
        frame
    }

    /// The pane `zooming` drawn `toward_whole` of the way from its place
    /// towards the whole tab, standing at `place` or at `whole`, with its
    /// nav bar `nav`, and the `others` pushed out of its way. Nothing is
    /// drawn outside the whole tab, or outside `area`.
    pub fn zoom<P>(
        zooming: &Zooming<P>,
        toward_whole: f32,
        place: Placed,
        whole: Placed,
        nav: Option<Nav>,
        others: &[OtherPane],
        area: RectF,
    ) -> Self {
        let bounds = within(whole.rect, area);
        let from = within(place.rect, area);
        let now = between(from, bounds, toward_whole);
        let stands = if zooming.zoomed { whole } else { place };
        // Its text and its ground go with its top left corner, from where
        // the zoom has laid them out.
        let shift = (
            lerp(place.origin.0, whole.origin.0, toward_whole) - stands.origin.0,
            lerp(place.origin.1, whole.origin.1, toward_whole) - stands.origin.1,
        );
        // Its nav bar goes from where it stands in its place to where it
        // stands over the whole tab.
        let nav = nav.map(|nav| (nav, lerp(nav.top_from, nav.top_to, toward_whole)));
        let (pieces, room, fills_shift) = pieces(stands.rect, now, shift, nav, area);
        let push = Push {
            place: zooming.place,
            from,
            now,
            bounds,
        };
        let mut gaps = without(vec![bounds], now);
        let mut pushed = Vec::with_capacity(others.len());
        for other in others {
            let Some(drawn) = push.pane(other.cells) else {
                continue;
            };
            let image = translated(within(other.rect, area), drawn.shift);
            if let Some(image) = image.intersection(&drawn.clip) {
                gaps = without(gaps, image);
            }
            pushed.push((other.pane_id, drawn));
        }
        Self {
            fills: room.into_iter().map(|rect| (zooming.pane_id, rect)).collect(),
            fills_shift,
            grounds: gaps.into_iter().map(|rect| (zooming.pane_id, rect)).collect(),
            zooming: Some((zooming.pane_id, pieces)),
            pushed,
            push: Some(push),
            ..Self::default()
        }
    }

    /// The panes `moving` drawn `t` of the way from where they stood to
    /// where they stand, the first over the rest, each edged with a `line`
    /// wide as a divider; under them the `still` ones where they stand, and
    /// under those the `hidden` ones, out of sight in a stack now, where
    /// they stood. Nothing is drawn outside `area`.
    pub fn rearrange(
        moving: &[MovingPane],
        still: &[(PaneId, RectF)],
        hidden: &[(PaneId, RectF)],
        t: f32,
        line: f32,
        area: RectF,
    ) -> Self {
        let mut frame = Self::default();
        let mut ends = Vec::with_capacity(moving.len() * 2);
        // What the panes so far are drawn in: each after goes under them.
        let mut over = Vec::with_capacity(moving.len() + still.len() + hidden.len());
        let bounds = moving
            .iter()
            .flat_map(|pane| [pane.from.rect, pane.to.rect])
            .chain(still.iter().chain(hidden).map(|(_, rect)| *rect))
            .map(|rect| within(rect, area))
            .reduce(|bounds, rect| bounds.union(&rect));
        for pane in moving {
            let (from, to) = (within(pane.from.rect, area), within(pane.to.rect, area));
            let now = between(from, to, t);
            let shift = (
                lerp(pane.from.origin.0, pane.to.origin.0, t) - pane.to.origin.0,
                lerp(pane.from.origin.1, pane.to.origin.1, t) - pane.to.origin.1,
            );
            let nav = pane
                .nav
                .map(|nav| (nav, lerp(nav.top_from, nav.top_to, t)));
            let (pieces, room, nav_shift) = pieces(pane.to.rect, now, shift, nav, area);
            frame.moving.push((pane.pane_id, under(pieces, &over)));
            frame.rooms.extend(
                cut(room, &over)
                    .into_iter()
                    .map(|rect| (pane.pane_id, rect, nav_shift)),
            );
            // Edged where it meets the others, as dividers edge the panes
            // standing still: just right of and under where two meet, so a
            // line ends up where the divider takes over. Not along the
            // window, where none do.
            if let Some(bounds) = bounds {
                let (x, y, w, h) = (now.min_x(), now.min_y(), now.width(), now.height());
                let sides = [
                    (x - bounds.min_x(), euclid::rect(x, y, line, h)),
                    (bounds.max_x() - now.max_x(), euclid::rect(now.max_x(), y, line, h)),
                    (y - bounds.min_y(), euclid::rect(x, y, w, line)),
                    (bounds.max_y() - now.max_y(), euclid::rect(x, now.max_y(), w + line, line)),
                ];
                let edges = sides
                    .iter()
                    .filter(|(inside, _)| *inside >= SLIVER)
                    .filter_map(|(_, edge)| edge.intersection(&area))
                    .collect();
                frame.edges.extend(cut(edges, &over));
            }
            // A divider beside where it stood or where it goes has no place
            // until it gets there: its own edges run there meanwhile.
            ends.extend([from, to].iter().map(|rect| rect.inflate(line, line)));
            over.push(now);
        }
        frame.dividers = Some(cut(cut(vec![area], &over), &ends));
        for (pane_id, rect) in still {
            let rect = within(*rect, area);
            // One nothing crosses is drawn as it stands; one beside it only
            // meets it, give or take what float sums put between them.
            let crossed = over.iter().any(|over| {
                over.intersection(&rect).is_some_and(|crossed| {
                    crossed.width() >= SLIVER && crossed.height() >= SLIVER
                })
            });
            if crossed {
                let clips = cut(vec![rect], &over);
                frame.moving.push((
                    *pane_id,
                    clips
                        .into_iter()
                        .map(|clip| Drawn {
                            shift: (0., 0.),
                            clip,
                        })
                        .collect(),
                ));
            }
            over.push(rect);
        }
        for (pane_id, rect) in hidden {
            let rect = within(*rect, area);
            let clips = cut(vec![rect], &over);
            frame.moving.push((
                *pane_id,
                clips
                    .into_iter()
                    .map(|clip| Drawn {
                        shift: (0., 0.),
                        clip,
                    })
                    .collect(),
            ));
            over.push(rect);
        }
        frame.backdrop = bounds.map_or_else(Vec::new, |bounds| cut(vec![bounds], &over));
        frame
    }

    /// How the frame draws `pos`, where not as it stands.
    pub fn pane(&self, pos: &PositionedPane) -> Option<Drawn> {
        self.pane_at(pos.pane.pane_id(), CellRect::of(pos))
    }

    fn pane_at(&self, pane_id: PaneId, rect: CellRect) -> Option<Drawn> {
        match self.arriving {
            Some((arriving, drawn)) if arriving == pane_id => Some(drawn),
            _ => self
                .pushed
                .iter()
                .find(|(pushed, _)| *pushed == pane_id)
                .map(|(_, drawn)| *drawn)
                .or_else(|| self.moved_at(rect)),
        }
    }

    /// The pieces the frame draws `pos` in: the pane zoomed or let back
    /// down on its way, or one rearranged on its way or under those.
    pub fn pieces(&self, pos: &PositionedPane) -> Option<&[Drawn]> {
        self.pieces_of(pos.pane.pane_id())
    }

    fn pieces_of(&self, pane_id: PaneId) -> Option<&[Drawn]> {
        match &self.zooming {
            Some((zooming, pieces)) if *zooming == pane_id => Some(pieces),
            _ => self
                .moving
                .iter()
                .find(|(moving, _)| *moving == pane_id)
                .map(|(_, pieces)| pieces.as_slice()),
        }
    }

    /// Where the frame draws the dividers, the panes rearranged.
    pub fn divider_clips(&self) -> Option<&[RectF]> {
        self.dividers.as_deref()
    }

    /// How the frame draws `split`, where not as it stands.
    pub fn split(&self, split: &PositionedSplit) -> Option<Drawn> {
        match self.leading {
            Some((edge, pane, drawn)) if edge.leads(pane, split) => Some(drawn),
            _ => {
                let rect = CellRect::of_split(split);
                match self.push {
                    Some(push) => push.divider(rect),
                    None => self.moved_at(rect),
                }
            }
        }
    }

    fn moved_at(&self, rect: CellRect) -> Option<Drawn> {
        match (self.block, self.room) {
            (Some((block, drawn)), _) if block.contains(&rect) => Some(drawn),
            (_, Some((room, drawn))) if room.overlaps(&rect) => Some(drawn),
            _ => None,
        }
    }
}

/// Last frame's layout and panes, and the slide under way.
pub struct SplitSlides<P = PositionedPane> {
    last: Option<TabLayout>,
    /// The panes `last` was made of, in its order. A pane found closed is
    /// painted from here on its way out; held no longer than a frame.
    last_panes: Vec<P>,
    /// Swapped with `last_panes` each frame, so neither is allocated anew.
    spare_panes: Vec<P>,
    /// The dividers between them, held and swapped the same way: a zoom
    /// pushes them out along with the panes.
    last_splits: Vec<PositionedSplit>,
    spare_splits: Vec<PositionedSplit>,
    coming: Option<SplitSlide>,
    going: Option<Leaving<P>>,
    zooming: Option<Zooming<P>>,
    rearranging: Option<Rearranging<P>>,
    /// The panes whose nav bar the frame painted showed no tab, as one an
    /// overlay stands in for does: one of them that closes goes out so.
    bare_navs: Vec<PaneId>,
    /// The tabs each nav bar the frame painted showed, by where they lie in
    /// `nav_tabs`, and where along the bar its first tab stood, scrolled as
    /// it was: one carried into another stack goes on from them.
    navs: Vec<(PaneId, f32, std::ops::Range<usize>)>,
    nav_tabs: Vec<PaneId>,
}

impl<P> Default for SplitSlides<P> {
    fn default() -> Self {
        Self {
            last: None,
            last_panes: vec![],
            spare_panes: vec![],
            last_splits: vec![],
            spare_splits: vec![],
            coming: None,
            going: None,
            zooming: None,
            rearranging: None,
            bare_navs: vec![],
            navs: vec![],
            nav_tabs: vec![],
        }
    }
}

/// A frame's slides, each with how far out of its place it is drawn; a
/// zoom with how far from its place towards the whole tab, a rearrangement
/// how far from where the panes stood to where they stand.
pub struct Slides<P = PositionedPane> {
    pub coming: Option<(SplitSlide, f32)>,
    pub going: Option<(Leaving<P>, f32)>,
    pub zooming: Option<(Zooming<P>, f32)>,
    pub rearranging: Option<(Rearranging<P>, f32)>,
}

impl<P: Clone> SplitSlides<P> {
    /// Take this frame's layout, the panes it was made of, in its order,
    /// and the dividers between them; the slides to paint the frame with. A
    /// frame painted again -- the quads ran out, a font came in -- takes the
    /// same layout again, and gets the same slides.
    pub fn advance(
        &mut self,
        layout: Option<TabLayout>,
        panes: &[P],
        splits: &[PositionedSplit],
        enabled: bool,
        now: Instant,
    ) -> Slides<P> {
        let before = std::mem::replace(&mut self.last, layout);
        std::mem::swap(&mut self.last_panes, &mut self.spare_panes);
        std::mem::swap(&mut self.last_splits, &mut self.spare_splits);
        self.last_panes.clear();
        self.last_splits.clear();
        if self
            .last
            .as_ref()
            .is_some_and(|layout| layout.panes.len() == panes.len())
        {
            self.last_panes.extend_from_slice(panes);
        }
        self.last_splits.extend_from_slice(splits);
        if !enabled {
            self.coming = None;
            self.going = None;
            self.zooming = None;
            self.rearranging = None;
        } else if let (Some(before), Some(layout)) = (before.as_ref(), self.last.as_ref()) {
            let opened = layout.opened_by_split(before).and_then(|(pane_id, edge, from)| {
                Some((pane_id, edge, from, layout.rect(pane_id)?))
            });
            if let Some((pane_id, edge, from_pane, rect)) = opened {
                // One slide at a time: the panes a slide draws elsewhere are
                // laid out for it alone. One still under way is cut short.
                self.going = None;
                self.zooming = None;
                self.rearranging = None;
                self.coming = Some(SplitSlide {
                    pane_id,
                    edge,
                    from_pane,
                    rect,
                    moved: layout.moved(before, edge, false),
                    started: now,
                    shown: 0,
                    paints: 0,
                });
            }
            if let Some((index, edge)) = layout.closed(before) {
                let (pane_id, rect, _) = before.panes[index];
                // Closed before it was all the way in: it has no place of
                // its own to go out of.
                let arriving = self.coming.is_some_and(|slide| slide.pane_id == pane_id);
                if let Some(pane) = self.spare_panes.get(index).filter(|_| !arriving) {
                    self.coming = None;
                    self.zooming = None;
                    self.rearranging = None;
                    self.going = Some(Leaving {
                        pane: pane.clone(),
                        edge,
                        rect,
                        moved: layout.moved(before, edge, true),
                        showed_tab: !self.bare_navs.contains(&pane_id),
                        tab_id: before.tab_id,
                        started: now,
                        shown: 0,
                        paints: 0,
                    });
                }
            }
            let zoom = layout.zoom(before).and_then(|(pane_id, zoomed)| {
                let (now_at, then_at) = (layout.rect(pane_id)?, before.rect(pane_id)?);
                Some((pane_id, zoomed, now_at, then_at))
            });
            if let Some((pane_id, zoomed, now_at, then_at)) = zoom {
                let (place, whole, others, dividers) = if zoomed {
                    // Out of the tab's frames from this one on.
                    let others = before
                        .panes
                        .iter()
                        .zip(&self.spare_panes)
                        .filter(|((id, _, _), _)| *id != pane_id)
                        .map(|(_, pane)| pane.clone())
                        .collect();
                    (then_at, now_at, others, std::mem::take(&mut self.spare_splits))
                } else {
                    (now_at, then_at, vec![], vec![])
                };
                // Taken back on its way, it turns round where it has got to.
                let turning = self
                    .zooming
                    .as_ref()
                    .filter(|zooming| {
                        zooming.pane_id == pane_id
                            && zooming.tab_id == layout.tab_id
                            && zooming.zoomed != zoomed
                    })
                    .and_then(|zooming| zooming.toward_whole(now));
                let mut zooming = Zooming {
                    pane_id,
                    zoomed,
                    place,
                    whole,
                    others,
                    dividers,
                    tab_id: layout.tab_id,
                    started: now,
                    shown: 0,
                    paints: 0,
                };
                if let Some(toward_whole) = turning {
                    zooming.set_out_from(toward_whole, now);
                }
                self.coming = None;
                self.going = None;
                self.rearranging = None;
                self.zooming = Some(zooming);
            }
            if let Some(rearranged) = layout.rearranged(before) {
                let hidden = rearranged
                    .hidden
                    .iter()
                    .filter_map(|index| self.spare_panes.get(*index).cloned())
                    .collect();
                let bare = rearranged
                    .hidden
                    .iter()
                    .map(|index| before.panes[*index].0)
                    .filter(|pane_id| self.bare_navs.contains(pane_id))
                    .collect();
                let carried = rearranged
                    .carried
                    .iter()
                    .map(|pane_id| {
                        match self.navs.iter().find(|(nav, _, _)| nav == pane_id) {
                            Some((_, lead, tabs)) => {
                                (*pane_id, Some(*lead), self.nav_tabs[tabs.clone()].to_vec())
                            }
                            None => (*pane_id, None, vec![*pane_id]),
                        }
                    })
                    .collect();
                self.coming = None;
                self.going = None;
                self.zooming = None;
                self.rearranging = Some(Rearranging {
                    moved: rearranged.moved,
                    hidden,
                    bare,
                    carried,
                    tab_id: layout.tab_id,
                    started: now,
                    shown: 0,
                    paints: 0,
                });
            }
        }
        // Last frame's panes have been looked through: let go of them, a
        // closed one among them.
        self.spare_panes.clear();
        self.spare_splits.clear();
        self.bare_navs.clear();
        self.navs.clear();
        self.nav_tabs.clear();
        Slides {
            coming: self.advance_coming(now),
            going: self.advance_going(now),
            zooming: self.advance_zooming(now),
            rearranging: self.advance_rearranging(now),
        }
    }

    /// The frame being painted shows `pane_id`'s nav bar without a tab.
    pub fn bare_nav(&mut self, pane_id: PaneId) {
        self.bare_navs.push(pane_id);
    }

    /// The frame being painted shows `tabs` in `pane_id`'s nav bar, the
    /// first `lead` along it.
    pub fn nav(&mut self, pane_id: PaneId, lead: f32, tabs: impl IntoIterator<Item = PaneId>) {
        let start = self.nav_tabs.len();
        self.nav_tabs.extend(tabs);
        self.navs.push((pane_id, lead, start..self.nav_tabs.len()));
    }

    /// The frame painted with the slides was shown: they are a frame on.
    /// Counted here rather than in `advance`, which a frame painted again
    /// calls again.
    pub fn shown(&mut self) {
        if let Some(slide) = self.coming.as_mut() {
            slide.shown += 1;
        }
        if let Some(leaving) = self.going.as_mut() {
            leaving.shown += 1;
        }
        if let Some(zooming) = self.zooming.as_mut() {
            zooming.shown += 1;
        }
        if let Some(rearranging) = self.rearranging.as_mut() {
            rearranging.shown += 1;
        }
    }

    fn advance_coming(&mut self, now: Instant) -> Option<(SplitSlide, f32)> {
        let slide = self.coming.as_mut()?;
        slide.paints += 1;
        // Gone from the tab, or the tab gone from the window: nothing left
        // to slide.
        let rect = self
            .last
            .as_ref()
            .filter(|layout| !layout.zoomed && slide.paints <= MOST_PAINTS)
            .and_then(|layout| layout.rect(slide.pane_id));
        match rect.and_then(|rect| Some((rect, slide.remaining(now)?))) {
            Some((rect, remaining)) => {
                slide.rect = rect;
                Some((*slide, remaining))
            }
            None => {
                self.coming = None;
                None
            }
        }
    }

    fn advance_going(&mut self, now: Instant) -> Option<(Leaving<P>, f32)> {
        let leaving = self.going.as_mut()?;
        leaving.paints += 1;
        // Another tab, or a pane zoomed over its room: nothing to go out of.
        let on_screen = leaving.paints <= MOST_PAINTS
            && self
                .last
                .as_ref()
                .is_some_and(|layout| layout.tab_id == leaving.tab_id && !layout.zoomed);
        match leaving.out(now).filter(|_| on_screen) {
            Some(out) => Some((leaving.clone(), out)),
            None => {
                self.going = None;
                None
            }
        }
    }

    fn advance_zooming(&mut self, now: Instant) -> Option<(Zooming<P>, f32)> {
        let zooming = self.zooming.as_mut()?;
        zooming.paints += 1;
        // Another tab, the zoom taken back or the pane gone: nothing left
        // to grow or shrink.
        let layout = self.last.as_ref().filter(|layout| {
            layout.tab_id == zooming.tab_id
                && layout.zoomed == zooming.zoomed
                && zooming.paints <= MOST_PAINTS
        });
        let rect = layout.and_then(|layout| layout.rect(zooming.pane_id));
        match rect.and_then(|rect| Some((rect, zooming.toward_whole(now)?))) {
            Some((rect, toward_whole)) => {
                // Where it stands now, should the tab have changed size.
                if zooming.zoomed {
                    zooming.whole = rect;
                } else {
                    zooming.place = rect;
                    if let Some(whole) = layout.and_then(TabLayout::bounds) {
                        zooming.whole = whole;
                    }
                }
                Some((zooming.clone(), toward_whole))
            }
            None => {
                self.zooming = None;
                None
            }
        }
    }

    fn advance_rearranging(&mut self, now: Instant) -> Option<(Rearranging<P>, f32)> {
        let rearranging = self.rearranging.as_mut()?;
        rearranging.paints += 1;
        // Another tab, or a zoom: nothing left to go anywhere.
        let layout = self.last.as_ref().filter(|layout| {
            layout.tab_id == rearranging.tab_id
                && !layout.zoomed
                && rearranging.paints <= MOST_PAINTS
        });
        if let Some(layout) = layout {
            // One closed on its way has nowhere to go.
            rearranging
                .moved
                .retain(|(pane_id, _)| layout.rect(*pane_id).is_some());
        }
        let along = layout
            .filter(|_| !rearranging.moved.is_empty())
            .and_then(|_| rearranging.along(now));
        match along {
            Some(along) => Some((rearranging.clone(), along)),
            None => {
                self.rearranging = None;
                None
            }
        }
    }

    /// Let go of every pane held, and of the layout last painted. A window
    /// that paints no frames would keep them until it painted one, a closed
    /// pane among them alive, and then take whatever changed meanwhile for a
    /// split, a close or a zoom to slide.
    pub fn release(&mut self) {
        self.last = None;
        self.last_panes.clear();
        self.last_splits.clear();
        self.coming = None;
        self.going = None;
        self.zooming = None;
        self.rearranging = None;
        self.bare_navs.clear();
        self.navs.clear();
        self.nav_tabs.clear();
    }
}

/// What the nav bar of a closed pane on its way out showed, which the tab
/// and the mux it has left no longer say.
#[derive(Clone)]
pub struct ClosedNav {
    pub pane: Arc<dyn Pane>,
    /// It showed a tab of its own.
    pub tab: bool,
    /// It had the button that collapses it and the pane over or under it.
    pub collapsible: bool,
}

impl ClosedNav {
    /// The closed pane, if it is `pane_id`.
    pub fn pane(&self, pane_id: PaneId) -> Option<&Arc<dyn Pane>> {
        (self.pane.pane_id() == pane_id).then_some(&self.pane)
    }
}

/// The nav bar of a pane carried into another stack, `along` of the way
/// from the tabs it showed, `from`, to its new stack's. `from_lead` is how
/// far along its old bar the first of them stood, scrolled as that was;
/// `None` where that bar was not painted.
#[derive(Clone, Debug, PartialEq)]
pub struct NavMorph {
    pub pane_id: PaneId,
    pub from: Vec<PaneId>,
    pub from_lead: Option<f32>,
    pub along: f32,
}

/// One tab of a nav bar on its way: how far along the strip it is drawn,
/// how opaque, and whether it is one of the stack's, to be clicked.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TabPlace {
    pub pane_id: PaneId,
    pub x: f32,
    pub opacity: f32,
    pub kept: bool,
}

impl NavMorph {
    /// Where along the bar its tabs are drawn, `to` being the new stack's,
    /// the first of them `to_lead` along, and each step how far apart tabs
    /// stand among as many as either list has: one in both goes from its
    /// place among `from` to its place among `to`; one only in `to` comes
    /// up where it goes, and one only in `from` fades where it was. At the
    /// end they stand as the stack has them.
    pub fn places(
        &self,
        to: &[PaneId],
        from_step: f32,
        to_step: f32,
        to_lead: f32,
    ) -> Vec<TabPlace> {
        let from_lead = self.from_lead.unwrap_or(to_lead);
        let mut places: Vec<TabPlace> = to
            .iter()
            .enumerate()
            .map(|(index, pane_id)| {
                let x = to_lead + index as f32 * to_step;
                match self.from.iter().position(|from| from == pane_id) {
                    Some(was) => TabPlace {
                        pane_id: *pane_id,
                        x: lerp(from_lead + was as f32 * from_step, x, self.along),
                        opacity: 1.0,
                        kept: true,
                    },
                    None => TabPlace {
                        pane_id: *pane_id,
                        x,
                        opacity: self.along,
                        kept: true,
                    },
                }
            })
            .collect();
        places.extend(
            self.from
                .iter()
                .enumerate()
                .filter(|(_, pane_id)| !to.contains(pane_id))
                .map(|(was, pane_id)| TabPlace {
                    pane_id: *pane_id,
                    x: from_lead + was as f32 * from_step,
                    opacity: 1.0 - self.along,
                    kept: false,
                }),
        );
        places
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each pane alone in a stack of its own, numbered as it is.
    fn layout(tab_id: TabId, panes: &[(PaneId, [usize; 4])]) -> TabLayout {
        TabLayout {
            tab_id,
            zoomed: false,
            panes: panes
                .iter()
                .map(|(id, [left, top, width, height])| {
                    (
                        *id,
                        CellRect {
                            left: *left,
                            top: *top,
                            width: *width,
                            height: *height,
                        },
                        *id,
                    )
                })
                .collect(),
            active: None,
        }
    }

    /// `layout`, with `pane` shown in `stack` instead.
    fn shown_in(mut layout: TabLayout, pane: PaneId, stack: PaneStackId) -> TabLayout {
        for (id, _, shown) in &mut layout.panes {
            if *id == pane {
                *shown = stack;
            }
        }
        layout
    }

    fn cells([left, top, width, height]: [usize; 4]) -> CellRect {
        CellRect {
            left,
            top,
            width,
            height,
        }
    }

    fn new_and_edge((new, edge, _): (PaneId, Edge, PaneId)) -> (PaneId, Edge) {
        (new, edge)
    }

    #[test]
    fn a_split_slides_the_new_pane_in_from_the_side_it_opened_on() {
        let one = layout(1, &[(1, [0, 0, 81, 24])]);
        let right = layout(1, &[(1, [0, 0, 40, 24]), (2, [41, 0, 40, 24])]);
        assert_eq!(right.opened_by_split(&one).map(new_and_edge), Some((2, Edge::Right)));
        let left = layout(1, &[(2, [0, 0, 40, 24]), (1, [41, 0, 40, 24])]);
        assert_eq!(left.opened_by_split(&one).map(new_and_edge), Some((2, Edge::Left)));
        let below = layout(1, &[(1, [0, 0, 81, 12]), (2, [0, 13, 81, 11])]);
        assert_eq!(below.opened_by_split(&one).map(new_and_edge), Some((2, Edge::Bottom)));
        let above = layout(1, &[(2, [0, 0, 81, 11]), (1, [0, 12, 81, 12])]);
        assert_eq!(above.opened_by_split(&one).map(new_and_edge), Some((2, Edge::Top)));
    }

    #[test]
    fn a_split_across_the_whole_tab_slides_in_beside_every_pane_it_squeezed() {
        let two = layout(1, &[(1, [0, 0, 81, 12]), (2, [0, 13, 81, 11])]);
        let three = layout(
            1,
            &[(1, [0, 0, 40, 12]), (2, [0, 13, 40, 11]), (3, [41, 0, 40, 24])],
        );
        assert_eq!(
            three.opened_by_split(&two).map(new_and_edge),
            Some((3, Edge::Right))
        );
        // Squeezed on the side away from it, they stay where they were.
        assert_eq!(three.moved(&two, Edge::Right, false), None);
    }

    #[test]
    fn a_pane_that_did_not_come_by_a_split_does_not_slide() {
        let one = layout(1, &[(1, [0, 0, 81, 24])]);
        let two = layout(1, &[(1, [0, 0, 40, 24]), (2, [41, 0, 40, 24])]);
        // Another tab with two panes.
        assert_eq!(layout(2, &two.panes_spec()).opened_by_split(&one), None);
        // A zoom let go of.
        let zoomed = TabLayout {
            zoomed: true,
            ..layout(1, &[(1, [0, 0, 81, 24])])
        };
        assert_eq!(two.opened_by_split(&zoomed), None);
        // An overlay closed: the pane under it comes back in its place.
        let overlay = layout(1, &[(1, [0, 0, 40, 24]), (9, [41, 0, 40, 24])]);
        let back = layout(1, &[(1, [0, 0, 40, 24]), (2, [41, 0, 40, 24])]);
        assert_eq!(back.opened_by_split(&overlay), None);
        // A window's first frame.
        assert_eq!(
            two.opened_by_split(&TabLayout {
                tab_id: 1,
                zoomed: false,
                panes: vec![],
                active: None,
            }),
            None
        );
        // Its neighbour has not given up the room yet.
        let stale = layout(1, &[(1, [0, 0, 81, 24]), (2, [41, 0, 40, 24])]);
        assert_eq!(stale.opened_by_split(&one), None);
    }

    impl TabLayout {
        fn panes_spec(&self) -> Vec<(PaneId, [usize; 4])> {
            self.panes
                .iter()
                .map(|(id, r, _)| (*id, [r.left, r.top, r.width, r.height]))
                .collect()
        }
    }

    fn one() -> Option<TabLayout> {
        Some(layout(1, &[(1, [0, 0, 81, 24])]))
    }

    fn two() -> Option<TabLayout> {
        Some(layout(1, &[(1, [0, 0, 40, 24]), (2, [41, 0, 40, 24])]))
    }

    /// The remaining way of each frame shown with the slide, a frame every
    /// `frame` from the split on, each painted `passes` times.
    fn run(frame: Duration, passes: usize) -> Vec<f32> {
        let start = Instant::now();
        let mut slides = SplitSlides::<PaneId>::default();
        assert!(slides.advance(one(), &[1], &[], true, start).coming.is_none());
        let mut steps = vec![];
        let mut now = start;
        loop {
            let mut painted = None;
            for _ in 0..passes {
                painted = slides.advance(two(), &[1, 2], &[], true, now).coming;
            }
            let Some((slide, remaining)) = painted else {
                break;
            };
            slides.shown();
            assert_eq!((slide.pane_id, slide.edge, slide.from_pane), (2, Edge::Right, 1));
            steps.push(remaining);
            now += frame;
            assert!(steps.len() < 100);
        }
        // And it does not start again.
        assert!(slides.advance(two(), &[1, 2], &[], true, now + frame).coming.is_none());
        steps
    }

    /// How far out the closed pane is drawn in each frame shown from its
    /// closing on, a frame every `frame`, each painted `passes` times.
    fn run_close(frame: Duration, passes: usize) -> Vec<f32> {
        let start = Instant::now();
        let mut slides = SplitSlides::<PaneId>::default();
        assert!(slides.advance(two(), &[1, 2], &[], true, start).going.is_none());
        let mut steps = vec![];
        let mut now = start;
        loop {
            let mut painted = None;
            for _ in 0..passes {
                painted = slides.advance(one(), &[1], &[], true, now).going;
            }
            let Some((leaving, out)) = painted else {
                break;
            };
            slides.shown();
            assert_eq!((leaving.pane, leaving.edge), (2, Edge::Right));
            steps.push(out);
            now += frame;
            assert!(steps.len() < 100);
        }
        assert!(slides.advance(one(), &[1], &[], true, now + frame).going.is_none());
        steps
    }

    #[test]
    fn a_slide_starts_on_the_split_and_settles_into_place() {
        let steps = run(Duration::from_micros(8_333), 1);
        // On its way from the first frame, never back, and there within
        // its time.
        assert!(steps[0] < 1.0 && steps[0] > 0.6, "{:?}", steps);
        // No frame jumps much more of the way than the first.
        let first = 1.0 - steps[0];
        assert!(
            steps.windows(2).all(|w| w[0] - w[1] <= first * 1.05),
            "{:?}", steps
        );
        assert!(steps.windows(2).all(|w| w[1] < w[0]), "{:?}", steps);
        assert!(steps.len() as u32 >= MIN_FRAMES, "{:?}", steps);
        assert!(steps.len() <= 22, "{:?}", steps);
        assert_eq!(Edge::Right.shift(0.5, 300., 200.), (150., 0.));
    }

    #[test]
    fn slow_frames_slow_the_slide_rather_than_skip_it() {
        let steps = run(Duration::from_millis(60), 1);
        assert!(steps.len() as u32 >= MIN_FRAMES - 1, "{:?}", steps);
        assert!(steps.windows(2).all(|w| w[1] < w[0]), "{:?}", steps);
        let steps = run_close(Duration::from_millis(60), 1);
        assert!(steps.len() as u32 >= MIN_FRAMES - 1, "{:?}", steps);
        assert!(steps.windows(2).all(|w| w[1] > w[0]), "{:?}", steps);
    }

    #[test]
    fn a_frame_painted_again_is_not_a_frame_further_on() {
        // Slow frames, each painted twice: as many frames shown as painted
        // once, and from as near the start.
        let once = run(Duration::from_millis(60), 1);
        let twice = run(Duration::from_millis(60), 2);
        assert_eq!(twice.len(), once.len(), "{twice:?} {once:?}");
        assert!((twice[0] - once[0]).abs() < 0.01, "{:?} {:?}", twice, once);
        let once = run_close(Duration::from_millis(60), 1);
        let twice = run_close(Duration::from_millis(60), 2);
        assert_eq!(twice.len(), once.len(), "{twice:?} {once:?}");
        assert!(twice[0] < 0.05, "{:?}", twice);
    }

    #[test]
    fn with_the_setting_off_nothing_slides() {
        let start = Instant::now();
        let mut slides = SplitSlides::<PaneId>::default();
        assert!(slides.advance(one(), &[1], &[], false, start).coming.is_none());
        assert!(slides.advance(two(), &[1, 2], &[], false, start).coming.is_none());
        assert!(slides.advance(one(), &[1], &[], false, start).going.is_none());
        slides.advance(Some(three()), &[1, 2, 3], &[], false, start);
        assert!(slides.advance(Some(zoomed(3)), &[3], &[], false, start).zooming.is_none());
    }

    #[test]
    fn a_pane_closed_mid_slide_stops_it() {
        let start = Instant::now();
        let mut slides = SplitSlides::<PaneId>::default();
        slides.advance(one(), &[1], &[], true, start);
        assert!(slides.advance(two(), &[1, 2], &[], true, start).coming.is_some());
        let closed = slides.advance(one(), &[1], &[], true, start + Duration::from_millis(10));
        assert!(closed.coming.is_none());
        // Nor does it go out of a place it never got to.
        assert!(closed.going.is_none());
    }

    #[test]
    fn one_slide_at_a_time() {
        let start = Instant::now();
        // The right of two split down, then the left one closed: the column
        // takes its room.
        let three = layout(
            1,
            &[(1, [0, 0, 40, 24]), (2, [41, 0, 40, 12]), (3, [41, 13, 40, 11])],
        );
        let column = layout(1, &[(2, [0, 0, 81, 12]), (3, [0, 13, 81, 11])]);
        let split = layout(
            1,
            &[(2, [0, 0, 81, 12]), (3, [0, 13, 40, 11]), (4, [41, 13, 40, 11])],
        );
        let mut slides = SplitSlides::<PaneId>::default();
        slides.advance(two(), &[1, 2], &[], true, start);
        assert!(slides.advance(Some(three), &[1, 2, 3], &[], true, start).coming.is_some());
        // Another pane closed while one comes in: that one is in at once.
        let closing = slides.advance(Some(column), &[2, 3], &[], true, start);
        assert!(closing.coming.is_none());
        assert_eq!(closing.going.map(|(leaving, _)| leaving.pane), Some(1));
        // And a split while one goes out.
        let splitting = slides.advance(Some(split), &[2, 3, 4], &[], true, start);
        assert!(splitting.going.is_none());
        assert_eq!(splitting.coming.map(|(slide, _)| slide.pane_id), Some(4));
    }

    /// The closed pane and the edge it goes out by, between two layouts.
    fn closed(after: &TabLayout, before: &TabLayout) -> Option<(PaneId, Edge)> {
        after
            .closed(before)
            .map(|(index, edge)| (before.panes[index].0, edge))
    }

    #[test]
    fn a_closed_pane_goes_out_away_from_the_panes_that_take_its_room() {
        let one = layout(1, &[(1, [0, 0, 81, 24])]);
        let right = layout(1, &[(1, [0, 0, 40, 24]), (2, [41, 0, 40, 24])]);
        assert_eq!(closed(&one, &right), Some((2, Edge::Right)));
        let left = layout(1, &[(2, [0, 0, 40, 24]), (1, [41, 0, 40, 24])]);
        assert_eq!(closed(&one, &left), Some((2, Edge::Left)));
        let below = layout(1, &[(1, [0, 0, 81, 12]), (2, [0, 13, 81, 11])]);
        assert_eq!(closed(&one, &below), Some((2, Edge::Bottom)));
        let above = layout(1, &[(2, [0, 0, 81, 11]), (1, [0, 12, 81, 12])]);
        assert_eq!(closed(&one, &above), Some((2, Edge::Top)));
        // A column of two takes the room of the pane beside it.
        let three = layout(
            1,
            &[(1, [0, 0, 40, 12]), (2, [0, 13, 40, 11]), (3, [41, 0, 40, 24])],
        );
        let column = layout(1, &[(1, [0, 0, 81, 12]), (2, [0, 13, 81, 11])]);
        assert_eq!(closed(&column, &three), Some((3, Edge::Right)));
    }

    #[test]
    fn a_pane_that_was_not_closed_does_not_go_out() {
        let one = layout(1, &[(1, [0, 0, 81, 24])]);
        let two = layout(1, &[(1, [0, 0, 40, 24]), (2, [41, 0, 40, 24])]);
        // Another tab with one pane.
        assert_eq!(closed(&layout(2, &one.panes_spec()), &two), None);
        // A zoom.
        let zoomed = TabLayout {
            zoomed: true,
            ..layout(1, &[(1, [0, 0, 81, 24])])
        };
        assert_eq!(closed(&zoomed, &two), None);
        // An overlay opened over a pane.
        let overlay = layout(1, &[(1, [0, 0, 40, 24]), (9, [41, 0, 40, 24])]);
        assert_eq!(closed(&overlay, &two), None);
        // Its neighbour has not taken the room yet.
        let stale = layout(1, &[(1, [0, 0, 40, 24])]);
        assert_eq!(closed(&stale, &two), None);
        // Two gone at once.
        let three = layout(
            1,
            &[(1, [0, 0, 27, 24]), (2, [28, 0, 26, 24]), (3, [55, 0, 26, 24])],
        );
        assert_eq!(closed(&one, &three), None);
    }

    #[test]
    fn a_pane_dragged_into_or_out_of_a_stack_is_carried_neither_coming_nor_going() {
        let start = Instant::now();
        let two = layout(1, &[(1, [0, 0, 40, 24]), (2, [41, 0, 40, 24])]);
        // 1 dropped into 2's stack, shown there in front of 2.
        let joined = shown_in(layout(1, &[(1, [0, 0, 81, 24])]), 1, 2);
        assert_eq!(closed(&joined, &two), None);
        // 2 dropped into 1's.
        let joined_left = shown_in(layout(1, &[(2, [0, 0, 81, 24])]), 2, 1);
        assert_eq!(closed(&joined_left, &two), None);
        // 1 dragged back out to the right of its stack, leaving 2 shown.
        let out = shown_in(
            layout(1, &[(2, [0, 0, 40, 24]), (1, [41, 0, 40, 24])]),
            1,
            7,
        );
        assert_eq!(out.opened_by_split(&joined), None);

        let mut slides = SplitSlides::<PaneId>::default();
        slides.advance(Some(two), &[1, 2], &[], true, start);
        let dropped = slides.advance(Some(joined), &[1], &[], true, start);
        assert!(dropped.going.is_none() && dropped.coming.is_none());
        assert!(dropped.rearranging.is_some());
        let dragged = slides.advance(Some(out), &[2, 1], &[], true, start);
        assert!(dragged.going.is_none() && dragged.coming.is_none());
        assert!(dragged.rearranging.is_some());
    }

    #[test]
    fn a_closed_pane_goes_out_the_way_it_came_and_sooner() {
        let steps = run_close(Duration::from_micros(8_333), 1);
        // Where it stood on the first frame, on its way after, never back.
        assert!(steps[0] < 0.05, "{:?}", steps);
        assert!(steps.windows(2).all(|w| w[1] > w[0]), "{:?}", steps);
        assert!(steps.len() as u32 >= MIN_FRAMES, "{:?}", steps);
        let coming = run(Duration::from_micros(8_333), 1);
        assert!(steps.len() < coming.len(), "{:?} {:?}", steps, coming);
    }

    #[test]
    fn a_closed_pane_goes_with_the_divider_its_split_put_in() {
        let pane = CellRect {
            left: 41,
            top: 13,
            width: 40,
            height: 11,
        };
        for edge in [Edge::Left, Edge::Right, Edge::Top, Edge::Bottom] {
            assert!(edge.leads(pane, &edge.divider(pane)), "{:?}", edge);
        }
        assert_eq!(
            Edge::Right.divider(pane),
            PositionedSplit {
                index: 0,
                direction: SplitDirection::Horizontal,
                left: 40,
                top: 13,
                size: 11,
            }
        );
    }

    #[test]
    fn a_window_that_lets_go_has_no_closed_pane_to_paint() {
        let start = Instant::now();
        let mut slides = SplitSlides::<PaneId>::default();
        slides.advance(two(), &[1, 2], &[], true, start);
        slides.release();
        assert!(slides.advance(one(), &[1], &[], true, start).going.is_none());
        // Nor takes what was split meanwhile for a split to slide in.
        slides.release();
        assert!(slides.advance(two(), &[1, 2], &[], true, start).coming.is_none());
        // Nor a tab switched to mid-way out.
        let mut slides = SplitSlides::<PaneId>::default();
        slides.advance(two(), &[1, 2], &[], true, start);
        assert!(slides.advance(one(), &[1], &[], true, start).going.is_some());
        let other = Some(layout(2, &[(5, [0, 0, 81, 24])]));
        assert!(slides.advance(other, &[5], &[], true, start).going.is_none());
    }

    #[test]
    fn a_pane_whose_nav_bar_showed_no_tab_goes_out_without_one() {
        let start = Instant::now();
        let mut slides = SplitSlides::<PaneId>::default();
        slides.advance(two(), &[1, 2], &[], true, start);
        // An overlay stood in for it, asking whether to close it.
        slides.bare_nav(2);
        let closing = slides.advance(one(), &[1], &[], true, start);
        assert_eq!(closing.going.map(|(leaving, _)| leaving.showed_tab()), Some(false));
        let mut slides = SplitSlides::<PaneId>::default();
        slides.advance(two(), &[1, 2], &[], true, start);
        slides.bare_nav(1);
        let closing = slides.advance(one(), &[1], &[], true, start);
        assert_eq!(closing.going.map(|(leaving, _)| leaving.showed_tab()), Some(true));
    }

    #[test]
    fn a_slide_whose_frames_are_never_shown_still_ends() {
        let start = Instant::now();
        let mut slides = SplitSlides::<PaneId>::default();
        slides.advance(one(), &[1], &[], true, start);
        let mut now = start;
        let mut paints = 0;
        while slides.advance(two(), &[1, 2], &[], true, now).coming.is_some() {
            paints += 1;
            now += Duration::from_millis(16);
            assert!(paints <= MOST_PAINTS, "{}", paints);
        }
        assert_eq!(paints, MOST_PAINTS);
        // Only frames that are not shown run into that: a slow one is
        // still shown in whole.
        let steps = run(Duration::from_millis(1500), 1);
        assert!(steps.len() as u32 >= MIN_FRAMES - 1, "{:?}", steps);
    }

    #[test]
    fn panes_that_move_over_for_a_slide_are_one_block() {
        let one = layout(1, &[(1, [0, 0, 81, 24])]);
        // Room made on the left or the top moves the pane that made it.
        let left = layout(1, &[(2, [0, 0, 40, 24]), (1, [41, 0, 40, 24])]);
        assert_eq!(
            left.moved(&one, Edge::Left, false),
            Some(Moved {
                block: cells([41, 0, 40, 24]),
                by: 41
            })
        );
        let above = layout(1, &[(2, [0, 0, 81, 11]), (1, [0, 12, 81, 12])]);
        assert_eq!(
            above.moved(&one, Edge::Top, false),
            Some(Moved {
                block: cells([0, 12, 81, 12]),
                by: 12
            })
        );
        // On the right or the bottom it only changes its size.
        let right = layout(1, &[(1, [0, 0, 40, 24]), (2, [41, 0, 40, 24])]);
        assert_eq!(right.moved(&one, Edge::Right, false), None);
        // Room taken back moves the panes that take it, a column of them
        // together.
        assert_eq!(
            one.moved(&left, Edge::Left, true),
            Some(Moved {
                block: cells([0, 0, 81, 24]),
                by: 41
            })
        );
        let three = layout(
            1,
            &[(3, [0, 0, 40, 24]), (1, [41, 0, 40, 12]), (2, [41, 13, 40, 11])],
        );
        let column = layout(1, &[(1, [0, 0, 81, 12]), (2, [0, 13, 81, 11])]);
        assert_eq!(
            column.moved(&three, Edge::Left, true),
            Some(Moved {
                block: cells([0, 0, 81, 24]),
                by: 41
            })
        );
        // Panes that moved by different amounts are not one block.
        let apart = layout(1, &[(1, [0, 0, 30, 24]), (2, [31, 0, 50, 24])]);
        let wider = layout(1, &[(1, [10, 0, 30, 24]), (2, [45, 0, 36, 24])]);
        assert_eq!(wider.moved(&apart, Edge::Left, false), None);
    }

    fn rect(x: f32, y: f32, w: f32, h: f32) -> RectF {
        euclid::rect(x, y, w, h)
    }

    fn slide(edge: Edge, rect: CellRect, moved: Option<Moved>) -> SplitSlide {
        SplitSlide {
            pane_id: 2,
            edge,
            from_pane: 1,
            rect,
            moved,
            started: Instant::now(),
            shown: 0,
            paints: 0,
        }
    }

    fn leaving(edge: Edge, rect: CellRect, moved: Option<Moved>) -> Leaving<PaneId> {
        Leaving {
            pane: 2,
            edge,
            rect,
            moved,
            showed_tab: true,
            tab_id: 1,
            started: Instant::now(),
            shown: 0,
            paints: 0,
        }
    }

    const CELL: (f32, f32) = (10., 20.);

    /// The terminal's part of the window, a sidebar 100px wide on its left.
    fn area() -> RectF {
        rect(100., 0., 810., 500.)
    }

    #[test]
    fn a_pane_coming_in_is_drawn_inside_its_place_with_its_room_filled() {
        // Split to the right: the new pane's place is 400px wide.
        let frame = SlideFrame::coming(
            &slide(Edge::Right, cells([41, 0, 40, 24]), None),
            0.75,
            rect(505., 0., 405., 500.),
            &[],
            CELL,
            area(),
        );
        let drawn = frame.pane_at(2, cells([41, 0, 40, 24])).unwrap();
        assert_eq!(drawn.shift, (303.75, 0.));
        assert_eq!(drawn.clip, rect(505., 0., 405., 500.));
        // The pane that made the room keeps it looking its own, and stands
        // where it is.
        assert_eq!(frame.fills, vec![(1, rect(505., 0., 303.75, 500.))]);
        assert_eq!(frame.pane_at(1, cells([0, 0, 40, 24])), None);
        // Along the window's left edge, a place reaches under the sidebar,
        // which nothing is drawn over.
        let frame = SlideFrame::coming(
            &slide(Edge::Left, cells([0, 0, 40, 24]), None),
            0.5,
            rect(0., 0., 505., 500.),
            &[],
            CELL,
            area(),
        );
        let drawn = frame.pane_at(2, cells([0, 0, 40, 24])).unwrap();
        assert_eq!(drawn.clip, rect(100., 0., 405., 500.));
        assert_eq!(drawn.shift, (-202.5, 0.));
    }

    #[test]
    fn panes_moved_over_by_a_split_go_along_from_where_they_stood() {
        // Split to the left of a pane along the sidebar: it moves 41 cells
        // over, and is drawn back from there all the way at the start.
        let pushed = Moved {
            block: cells([41, 0, 40, 24]),
            by: 41,
        };
        let place = rect(0., 0., 505., 500.);
        let theirs = rect(505., 0., 405., 500.);
        let at = |remaining| {
            SlideFrame::coming(
                &slide(Edge::Left, cells([0, 0, 40, 24]), Some(pushed)),
                remaining,
                place,
                &[(1, theirs)],
                CELL,
                area(),
            )
        };
        let start = at(1.0).pane_at(1, cells([41, 0, 40, 24])).unwrap();
        assert_eq!(start.shift, (-410., 0.));
        let frame = at(0.5);
        let drawn = frame.pane_at(1, cells([41, 0, 40, 24])).unwrap();
        assert_eq!(drawn.shift, (-205., 0.));
        // Up against the new pane, which has 202.5px still to come in.
        assert_eq!(drawn.clip, rect(302.5, 0., 607.5, 500.));
        // Behind it, its room it is not in yet, and nothing it covers: a
        // see-through ground laid on twice comes out darker.
        assert!(frame.fills.contains(&(1, rect(705., 0., 205., 500.))));
        let covered = within(theirs.translate(euclid::vec2(drawn.shift.0, 0.)), drawn.clip);
        for (_, fill) in &frame.fills {
            assert!(fill.intersection(&covered).is_none(), "{:?} {:?}", fill, covered);
        }
        // The divider between them goes with both.
        let divider = Edge::Left.divider(cells([0, 0, 40, 24]));
        assert_eq!(frame.split(&divider).unwrap().shift, (-202.5, 0.));
        // Where whole cells come up short of the room, what is left between
        // them is filled like the pane it was.
        let short = SlideFrame::coming(
            &slide(
                Edge::Left,
                cells([0, 0, 40, 24]),
                Some(Moved {
                    block: cells([41, 0, 40, 24]),
                    by: 40,
                }),
            ),
            0.5,
            place,
            &[(1, theirs)],
            CELL,
            area(),
        );
        assert_eq!(short.fills[0], (1, rect(302.5, 0., 2.5, 500.)));
    }

    #[test]
    fn a_closed_pane_goes_out_inside_its_place_over_room_cut_short_of_it() {
        // The right of two goes: the left one stands in all the room,
        // cut where the closed pane still is.
        let frame = SlideFrame::going(
            &leaving(Edge::Right, cells([41, 0, 40, 24]), None),
            0.25,
            rect(505., 0., 405., 500.),
            &[],
            CELL,
            area(),
        );
        let (pane, divider) = frame.leaving.unwrap();
        assert_eq!(pane.shift, (101.25, 0.));
        assert_eq!(pane.clip, rect(505., 0., 405., 500.));
        assert_eq!(divider.shift, (101.25, 0.));
        let left = frame.pane_at(1, cells([0, 0, 81, 24])).unwrap();
        assert_eq!(left.shift, (0., 0.));
        assert_eq!(left.clip, rect(100., 0., 506.25, 500.));
        // A pane beside neither stands as it is.
        assert_eq!(frame.pane_at(3, cells([82, 0, 10, 24])), None);
        // Nor does it draw itself under the sidebar.
        let frame = SlideFrame::going(
            &leaving(Edge::Left, cells([0, 0, 40, 24]), None),
            0.0,
            rect(0., 0., 505., 500.),
            &[],
            CELL,
            area(),
        );
        assert_eq!(frame.leaving.unwrap().0.clip, rect(100., 0., 405., 500.));
    }

    #[test]
    fn panes_moved_over_into_a_closed_panes_room_come_from_where_they_stood() {
        // The left of two goes: the right one takes its room, 41 cells
        // over, drawn back there at first and up against it after.
        let pushed = Moved {
            block: cells([0, 0, 81, 24]),
            by: 41,
        };
        let at = |out| {
            SlideFrame::going(
                &leaving(Edge::Left, cells([0, 0, 40, 24]), Some(pushed)),
                out,
                rect(0., 0., 505., 500.),
                &[(1, rect(0., 0., 910., 500.))],
                CELL,
                area(),
            )
        };
        let start = at(0.0).pane_at(1, cells([0, 0, 81, 24])).unwrap();
        assert_eq!(start.shift, (410., 0.));
        let frame = at(0.5);
        let drawn = frame.pane_at(1, cells([0, 0, 81, 24])).unwrap();
        assert_eq!(drawn.shift, (205., 0.));
        assert_eq!(drawn.clip, rect(302.5, 0., 607.5, 500.));
        let end = at(1.0).pane_at(1, cells([0, 0, 81, 24])).unwrap();
        assert_eq!(end.shift, (0., 0.));
    }

    #[test]
    fn dividers_reaching_into_a_closed_panes_room_are_cut_short_of_it() {
        // A column of two takes the room of the pane on its right: the
        // divider between them runs on into it.
        let frame = SlideFrame::going(
            &leaving(Edge::Right, cells([41, 0, 40, 24]), None),
            0.5,
            rect(505., 0., 405., 500.),
            &[],
            CELL,
            area(),
        );
        let across = PositionedSplit {
            index: 1,
            direction: SplitDirection::Vertical,
            left: 0,
            top: 12,
            size: 81,
        };
        assert_eq!(frame.split(&across).unwrap().clip.max_x(), 505. + 202.5);
        // One beside it is not.
        let beside = PositionedSplit {
            index: 1,
            direction: SplitDirection::Horizontal,
            left: 81,
            top: 0,
            size: 24,
        };
        assert_eq!(frame.split(&beside), None);
    }

    /// A pane down the left of the tab, and two over and under each other
    /// on its right.
    fn three() -> TabLayout {
        layout(
            1,
            &[(1, [0, 0, 40, 24]), (2, [41, 0, 40, 11]), (3, [41, 12, 40, 12])],
        )
    }

    fn zoomed(pane_id: PaneId) -> TabLayout {
        TabLayout {
            zoomed: true,
            ..layout(1, &[(pane_id, [0, 0, 81, 24])])
        }
    }

    /// The dividers of `three`: down beside the left pane, and across
    /// between the two on its right.
    fn three_splits() -> [PositionedSplit; 2] {
        [
            PositionedSplit {
                index: 0,
                direction: SplitDirection::Horizontal,
                left: 40,
                top: 0,
                size: 24,
            },
            PositionedSplit {
                index: 1,
                direction: SplitDirection::Vertical,
                left: 41,
                top: 11,
                size: 40,
            },
        ]
    }

    fn zooming(zoomed: bool, pane_id: PaneId, place: [usize; 4]) -> Zooming<PaneId> {
        Zooming {
            pane_id,
            zoomed,
            place: cells(place),
            whole: cells([0, 0, 81, 24]),
            others: vec![],
            dividers: vec![],
            tab_id: 1,
            started: Instant::now(),
            shown: 0,
            paints: 0,
        }
    }

    /// Where the panes of `three` stand in `area`, by their ids.
    fn three_places() -> [(PaneId, RectF); 3] {
        [
            (1, rect(100., 0., 405., 500.)),
            (2, rect(505., 0., 405., 230.)),
            (3, rect(505., 230., 405., 270.)),
        ]
    }

    /// A pane standing at `rect`, its text starting `inset` in from its
    /// corner: further along the sidebar than half way across a divider.
    fn placed(rect: RectF, inset: f32) -> Placed {
        Placed {
            rect,
            origin: (rect.min_x() + inset, rect.min_y() + 10.),
        }
    }

    fn whole() -> Placed {
        placed(area(), 10.)
    }

    /// The others, standing where `three_places` has them.
    fn others_of(pane_id: PaneId) -> Vec<OtherPane> {
        let three = three();
        three_places()
            .iter()
            .filter(|(id, _)| *id != pane_id)
            .map(|(id, rect)| OtherPane {
                pane_id: *id,
                cells: three.rect(*id).unwrap(),
                rect: *rect,
            })
            .collect()
    }

    /// A nav bar standing along `band`, with no terminal bar to lift it:
    /// its top in either place is where the pane's text starts there.
    fn nav(band: RectF, buttons: f32, place: Placed, whole: Placed) -> Option<Nav> {
        Some(Nav {
            band,
            buttons,
            top_from: place.origin.1,
            top_to: whole.origin.1,
        })
    }

    fn area_of(rects: &[RectF]) -> f32 {
        rects.iter().map(|rect| rect.area()).sum()
    }

    /// Whether any two of `rects` overlap.
    fn any_overlap(rects: &[RectF]) -> bool {
        rects.iter().enumerate().any(|(i, a)| {
            rects[i + 1..]
                .iter()
                .any(|b| a.intersection(b).is_some_and(|both| both.area() > 0.01))
        })
    }

    /// What `frame` draws of the zooming pane, painted standing at `stood`,
    /// and fills like its own: over where it is now, each once.
    fn zooming_drawn(frame: &SlideFrame, pane_id: PaneId, stood: RectF) -> Vec<RectF> {
        let mut drawn: Vec<RectF> = frame
            .pieces_of(pane_id)
            .unwrap()
            .iter()
            .filter_map(|piece| piece.clip.intersection(&translated(stood, piece.shift)))
            .collect();
        drawn.extend(frame.fills.iter().map(|(_, rect)| *rect));
        drawn
    }

    #[test]
    fn a_zoom_is_read_off_the_layout_both_ways() {
        assert_eq!(zoomed(3).zoom(&three()), Some((3, true)));
        assert_eq!(three().zoom(&zoomed(3)), Some((3, false)));
        // Another tab, zoomed or not.
        let other = TabLayout {
            tab_id: 2,
            ..zoomed(3)
        };
        assert_eq!(other.zoom(&three()), None);
        // Zoomed both frames, or neither.
        assert_eq!(zoomed(3).zoom(&zoomed(3)), None);
        assert_eq!(three().zoom(&three()), None);
        // A pane alone in its tab already had all of it.
        let one = layout(1, &[(1, [0, 0, 81, 24])]);
        assert_eq!(zoomed(1).zoom(&one), None);
        // A window's first frame, and a pane zoomed that was not there.
        assert_eq!(zoomed(3).zoom(&layout(1, &[])), None);
        assert_eq!(zoomed(9).zoom(&three()), None);
        // Nor is it a split or a close, either way.
        assert_eq!(three().opened_by_split(&zoomed(3)), None);
        assert_eq!(zoomed(3).closed(&three()), None);
    }

    #[test]
    fn a_zoom_holds_the_others_and_their_dividers_on_their_way_out() {
        let start = Instant::now();
        let mut slides = SplitSlides::<PaneId>::default();
        let splits = three_splits();
        let frame = Duration::from_millis(16);
        assert!(slides
            .advance(Some(three()), &[1, 2, 3], &splits, true, start)
            .zooming
            .is_none());
        let mut now = start;
        let mut steps = vec![];
        while let Some((zooming, toward_whole)) = slides
            .advance(Some(zoomed(3)), &[3], &[], true, now)
            .zooming
        {
            slides.shown();
            assert_eq!(zooming.pane_id, 3);
            assert_eq!(zooming.others, vec![1, 2]);
            assert_eq!(zooming.dividers, splits.to_vec());
            steps.push(toward_whole);
            now += frame;
            assert!(steps.len() < 100);
        }
        // On its way from the first frame, and steadily on.
        assert!(steps.len() >= 8, "{:?}", steps);
        assert!(steps[0] > 0.0 && steps[0] < 0.3, "{:?}", steps);
        assert!(steps.windows(2).all(|two| two[0] < two[1]), "{:?}", steps);
        // Let back down: the others are the tab's again, and come back in.
        let mut steps = vec![];
        while let Some((zooming, toward_whole)) = slides
            .advance(Some(three()), &[1, 2, 3], &splits, true, now)
            .zooming
        {
            slides.shown();
            assert!(zooming.others.is_empty() && zooming.dividers.is_empty());
            steps.push(toward_whole);
            now += frame;
            assert!(steps.len() < 100);
        }
        assert!(steps.len() >= 8, "{:?}", steps);
        assert!(steps[0] < 1.0 && steps[0] > 0.7, "{:?}", steps);
        assert!(steps.windows(2).all(|two| two[0] > two[1]), "{:?}", steps);
        // One on the way is cut short by the next, the other way or a split.
        assert!(slides
            .advance(Some(zoomed(3)), &[3], &[], true, now)
            .zooming
            .is_some_and(|(zooming, _)| zooming.zoomed));
        let back = slides.advance(Some(three()), &[1, 2, 3], &splits, true, now);
        assert!(back.zooming.is_some_and(|(zooming, _)| !zooming.zoomed));
        let split = layout(
            1,
            &[
                (1, [0, 0, 40, 24]),
                (2, [41, 0, 40, 11]),
                (3, [41, 12, 20, 12]),
                (5, [62, 12, 19, 12]),
            ],
        );
        let split = slides.advance(Some(split), &[1, 2, 3, 5], &splits, true, now);
        assert!(split.coming.is_some() && split.zooming.is_none());
        // And a split by a zoom; zoomed again, the window stops painting.
        let zoom = slides.advance(Some(zoomed(3)), &[3], &[], true, now);
        assert!(zoom.coming.is_none() && zoom.zooming.is_some());
        slides.release();
        assert!(slides.zooming.is_none());
        assert!(slides.last_splits.is_empty() && slides.spare_splits.is_empty());
    }

    #[test]
    fn a_zoomed_pane_pushes_the_others_out_past_the_tabs_edges() {
        // The pane under the top right one zooms, half way there: it has
        // grown up and to the left, from its place's corner at 505, 230.
        let place = placed(rect(505., 230., 405., 270.), 5.);
        let zoom = |toward_whole| {
            SlideFrame::zoom(
                &zooming(true, 3, [41, 12, 40, 12]),
                toward_whole,
                place,
                whole(),
                nav(rect(100., 10., 810., 30.), 790., place, whole()),
                &others_of(3),
                area(),
            )
        };
        let frame = zoom(0.5);
        let now = rect(302.5, 115., 607.5, 385.);
        // Its tabs and its text with its top left corner, half way between
        // where they started in its place and where they end up; the
        // buttons at the end of its nav bar along its right edge, which has
        // not moved.
        let text = (200., 115.);
        assert_eq!(
            frame.pieces_of(3),
            Some(
                &[
                    Drawn {
                        shift: text,
                        clip: rect(302.5, 125., 487.5, 30.),
                    },
                    Drawn {
                        shift: (0., 115.),
                        clip: rect(790., 125., 120., 30.),
                    },
                    Drawn {
                        shift: text,
                        clip: rect(302.5, 155., 607.5, 345.),
                    },
                ][..]
            )
        );
        // Laid out for the whole tab it covers all it has grown into, but
        // for the strip above its nav bar, which is its ground.
        assert_eq!(frame.fills, vec![(3, rect(302.5, 115., 607.5, 10.))]);
        assert_eq!(frame.fills_shift, 115.);
        // The pane on the left goes out to the left, ahead of its left
        // edge; the one above goes out the top.
        assert_eq!(
            frame.pane_at(1, cells([0, 0, 40, 24])),
            Some(Drawn {
                shift: (-202.5, 0.),
                clip: rect(100., 0., 202.5, 500.),
            })
        );
        assert_eq!(
            frame.pane_at(2, cells([41, 0, 40, 11])),
            Some(Drawn {
                shift: (0., -115.),
                clip: rect(100., 0., 810., 115.),
            })
        );
        // Between the two of them, over its corner, room nobody covers.
        assert_eq!(frame.grounds, vec![(3, rect(302.5, 0., 202.5, 115.))]);
        assert_eq!(
            area_of(&[now, rect(100., 0., 202.5, 500.), rect(505., 0., 405., 115.)])
                + area_of(&[frame.grounds[0].1]),
            area().area()
        );
        // The dividers go with the panes beside them.
        let [down, across] = three_splits();
        assert_eq!(frame.split(&down).unwrap().shift, (-202.5, 0.));
        assert_eq!(frame.split(&across).unwrap().shift, (0., -115.));
        // All the way there, the others are all the way out.
        let there = zoom(1.0);
        for (pane_id, stood) in [(1, cells([0, 0, 40, 24])), (2, cells([41, 0, 40, 11]))] {
            assert!(there.pane_at(pane_id, stood).unwrap().clip.is_empty());
        }
        assert!(there.grounds.is_empty());
    }

    #[test]
    fn the_side_a_pane_lies_on_is_told_by_its_cells() {
        // Padded at the top by more than half a cell, the ground of the pane
        // above runs on under the nav bar of the one below.
        let place = placed(rect(505., 230., 405., 270.), 5.);
        let mut others = others_of(3);
        others[1].rect = rect(505., 0., 405., 240.);
        let frame = SlideFrame::zoom(
            &zooming(true, 3, [41, 12, 40, 12]),
            0.5,
            place,
            whole(),
            nav(rect(100., 10., 810., 30.), 790., place, whole()),
            &others,
            area(),
        );
        // It still goes out the top, clear of the pane zoomed.
        assert_eq!(
            frame.pane_at(2, cells([41, 0, 40, 11])),
            Some(Drawn {
                shift: (0., -115.),
                clip: rect(100., 0., 810., 115.),
            })
        );
    }

    #[test]
    fn a_nav_bar_a_terminal_bar_lifts_goes_its_own_way() {
        // A terminal bar 20px high along the top lifts the nav bars of the
        // panes along the tab's top edge: over the whole tab the pane's nav
        // bar stands 20px above where its text starts, in its place it
        // stands where its text does.
        let place = placed(rect(505., 230., 405., 270.), 5.);
        let whole = Placed {
            rect: area(),
            origin: (110., 30.),
        };
        let zoom = |toward_whole| {
            SlideFrame::zoom(
                &zooming(true, 3, [41, 12, 40, 12]),
                toward_whole,
                place,
                whole,
                Some(Nav {
                    band: rect(100., 10., 810., 30.),
                    buttons: 790.,
                    top_from: 240.,
                    top_to: 10.,
                }),
                &others_of(3),
                area(),
            )
        };
        // It sets out from where it stood, not the bar's height above.
        let start = zoom(0.0);
        assert_eq!(start.pieces_of(3).unwrap()[0].clip.min_y(), 240.);
        let frame = zoom(0.5);
        let pieces = frame.pieces_of(3).unwrap();
        // The nav bar half way between, the text below it on its own way,
        // with none of the nav bar it has under it as painted.
        assert_eq!(pieces[0].clip, rect(302.5, 125., 487.5, 30.));
        assert_eq!(pieces[0].shift, (200., 115.));
        assert_eq!(pieces[2].shift, (200., 105.));
        assert_eq!(pieces[2].clip.min_y(), 155.);
        assert_eq!(frame.fills_shift, 115.);
        let drawn = zooming_drawn(&frame, 3, area());
        assert!(!any_overlap(&drawn), "{:?}", drawn);
        assert!((area_of(&drawn) - rect(302.5, 115., 607.5, 385.).area()).abs() < 0.01);
    }

    #[test]
    fn the_buttons_at_the_end_of_a_zoomed_panes_nav_bar_keep_to_its_right_edge() {
        // The pane down the left zooms: it grows to the right, half way to
        // the tab's right edge at 910.
        let place = placed(rect(100., 0., 405., 500.), 10.);
        let frame = SlideFrame::zoom(
            &zooming(true, 1, [0, 0, 40, 24]),
            0.5,
            place,
            whole(),
            nav(rect(100., 10., 810., 30.), 790., place, whole()),
            &others_of(1),
            area(),
        );
        let pieces = frame.pieces_of(1).unwrap();
        // Its tab strip up to them, as laid out for the whole tab; they are
        // drawn back from its end by as much as it has still to grow.
        assert_eq!(pieces[0].clip, rect(100., 10., 487.5, 30.));
        assert_eq!(pieces[0].shift, (0., 0.));
        assert_eq!(pieces[1].clip, rect(587.5, 10., 120., 30.));
        assert_eq!(pieces[1].shift, (-202.5, 0.));
        assert_eq!(frame.fills, vec![(1, rect(100., 0., 607.5, 10.))]);
        // The two on its right go out to the right, together.
        for (pane_id, stood) in [(2, cells([41, 0, 40, 11])), (3, cells([41, 12, 40, 12]))] {
            let drawn = frame.pane_at(pane_id, stood).unwrap();
            assert_eq!(drawn.shift, (202.5, 0.));
            assert_eq!(drawn.clip, rect(707.5, 0., 202.5, 500.));
        }
        assert!(frame.grounds.is_empty());
        // Let back down, half way: laid out for its place now, the buttons
        // still keep to its right edge, and the bar between them and its
        // tabs is filled like its own.
        let frame = SlideFrame::zoom(
            &zooming(false, 1, [0, 0, 40, 24]),
            0.5,
            place,
            whole(),
            nav(rect(100., 10., 405., 30.), 385., place, whole()),
            &others_of(1),
            area(),
        );
        let pieces = frame.pieces_of(1).unwrap();
        assert_eq!(pieces[0].clip, rect(100., 10., 285., 30.));
        assert_eq!(pieces[1].clip, rect(587.5, 10., 120., 30.));
        assert_eq!(pieces[1].shift, (202.5, 0.));
        let fills: Vec<RectF> = frame.fills.iter().map(|(_, rect)| *rect).collect();
        assert!(fills.contains(&rect(385., 10., 202.5, 30.)), "{:?}", fills);
        assert_eq!(frame.fills_shift, 0.);
    }

    #[test]
    fn a_pane_let_back_down_covers_what_it_does_not_once() {
        // The pane under the top right one, half way back into its place:
        // laid out for that, it covers less than it has still to give up.
        let place = placed(rect(505., 230., 405., 270.), 5.);
        let frame = SlideFrame::zoom(
            &zooming(false, 3, [41, 12, 40, 12]),
            0.5,
            place,
            whole(),
            nav(rect(505., 240., 405., 30.), 790., place, whole()),
            &others_of(3),
            area(),
        );
        let now = rect(302.5, 115., 607.5, 385.);
        // Its nav bar's band is drawn as far up as its text.
        assert_eq!(frame.fills_shift, -115.);
        let pieces = frame.pieces_of(3).unwrap();
        assert_eq!(pieces[1].shift, (0., -115.));
        // Over it, what each piece draws of the pane and what is filled
        // like it are the whole of it, once.
        let drawn = zooming_drawn(&frame, 3, place.rect);
        assert!(!any_overlap(&drawn), "{:?}", drawn);
        assert!((area_of(&drawn) - now.area()).abs() < 0.01);
        // And around it, the others and the room between them.
        let mut around: Vec<RectF> = vec![now];
        for other in others_of(3) {
            let pushed = frame.pane_at(other.pane_id, other.cells).unwrap();
            around.extend(translated(other.rect, pushed.shift).intersection(&pushed.clip));
        }
        around.extend(frame.grounds.iter().map(|(_, rect)| *rect));
        assert!(!any_overlap(&around), "{:?}", around);
        assert!((area_of(&around) - area().area()).abs() < 0.01);
    }

    #[test]
    fn a_zoom_taken_back_on_its_way_turns_round_where_it_has_got_to() {
        let start = Instant::now();
        let frame = Duration::from_millis(16);
        let mut slides = SplitSlides::<PaneId>::default();
        slides.advance(Some(three()), &[1, 2, 3], &[], true, start);
        let mut now = start;
        let mut last = 0.0;
        for _ in 0..3 {
            last = slides
                .advance(Some(zoomed(3)), &[3], &[], true, now)
                .zooming
                .unwrap()
                .1;
            slides.shown();
            now += frame;
        }
        let (zooming, back) = slides
            .advance(Some(three()), &[1, 2, 3], &[], true, now)
            .zooming
            .unwrap();
        assert!(!zooming.zoomed);
        // About where it was, on its way back rather than starting over
        // from the whole tab.
        assert!(back < last && last - back < 0.05, "{} {}", last, back);
        slides.shown();
        now += frame;
        let next = slides
            .advance(Some(three()), &[1, 2, 3], &[], true, now)
            .zooming
            .unwrap()
            .1;
        assert!(next < back, "{} {}", back, next);
    }

    #[test]
    fn a_tab_that_grows_while_a_zoom_is_let_down_is_all_drawn() {
        let start = Instant::now();
        let mut slides = SplitSlides::<PaneId>::default();
        slides.advance(Some(zoomed(3)), &[3], &[], true, start);
        assert!(slides
            .advance(Some(three()), &[1, 2, 3], &[], true, start)
            .zooming
            .is_some());
        let wider = layout(
            1,
            &[(1, [0, 0, 45, 24]), (2, [46, 0, 45, 11]), (3, [46, 12, 45, 12])],
        );
        let (zooming, _) = slides
            .advance(Some(wider), &[1, 2, 3], &[], true, start)
            .zooming
            .unwrap();
        assert_eq!(zooming.whole, cells([0, 0, 91, 24]));
        assert_eq!(zooming.place, cells([46, 12, 45, 12]));
    }

    /// `layout`, with `pane` the active one.
    fn active(mut layout: TabLayout, pane: PaneId) -> TabLayout {
        layout.active = Some(pane);
        layout
    }

    #[test]
    fn a_pane_carried_elsewhere_is_read_off_the_layout_with_those_making_way() {
        let two = layout(1, &[(1, [0, 0, 40, 24]), (2, [41, 0, 40, 24])]);
        // 1 dropped under 2, in a stack of its own, 2 across the top.
        let under = active(
            shown_in(
                layout(1, &[(2, [0, 0, 81, 12]), (1, [0, 13, 81, 11])]),
                1,
                7,
            ),
            1,
        );
        let rearranged = under.rearranged(&two).unwrap();
        // The one carried first, then the one that made way.
        assert_eq!(
            rearranged.moved,
            vec![(1, cells([0, 0, 40, 24])), (2, cells([41, 0, 40, 24]))]
        );
        assert!(rearranged.hidden.is_empty());
        // So too where the move left the other one active.
        let from_outside = active(under, 2).rearranged(&two).unwrap();
        assert_eq!(from_outside.moved[0].0, 1);
        // Moved from outside the window, every stack of one pane named
        // afresh by the resync that brings it: the one gone furthest.
        let resynced = shown_in(
            shown_in(
                layout(1, &[(2, [0, 0, 81, 12]), (1, [0, 13, 81, 11])]),
                1,
                7,
            ),
            2,
            8,
        );
        assert_eq!(resynced.rearranged(&two).unwrap().moved[0].0, 1);
        // Into 2's stack: 2 out of sight under it, by its place before;
        // 1 the one carried into another stack.
        let joined = active(shown_in(layout(1, &[(1, [0, 0, 81, 24])]), 1, 2), 1);
        let rearranged = joined.rearranged(&two).unwrap();
        assert_eq!(rearranged.moved, vec![(1, cells([0, 0, 40, 24]))]);
        assert_eq!(rearranged.hidden, vec![1]);
        assert_eq!(rearranged.carried, vec![1]);
        // Back out to the right of it: 2 comes out where the stack stands,
        // and goes nowhere.
        let out = active(
            shown_in(
                layout(1, &[(2, [0, 0, 40, 24]), (1, [41, 0, 40, 24])]),
                1,
                7,
            ),
            1,
        );
        let rearranged = out.rearranged(&joined).unwrap();
        assert_eq!(rearranged.moved, vec![(1, cells([0, 0, 81, 24]))]);
        assert!(rearranged.hidden.is_empty());
        // Swapped, each into the other's stack: the one carried first,
        // second as the tab has them.
        let swapped = active(
            shown_in(
                shown_in(
                    layout(1, &[(2, [0, 0, 40, 24]), (1, [41, 0, 40, 24])]),
                    2,
                    1,
                ),
                1,
                2,
            ),
            1,
        );
        let rearranged = swapped.rearranged(&two).unwrap();
        assert_eq!(
            rearranged.moved,
            vec![(1, cells([0, 0, 40, 24])), (2, cells([41, 0, 40, 24]))]
        );
        assert_eq!(rearranged.carried, vec![1, 2]);
    }

    #[test]
    fn panes_resized_opened_closed_or_shown_in_turn_are_not_carried() {
        let two = layout(1, &[(1, [0, 0, 40, 24]), (2, [41, 0, 40, 24])]);
        // A divider dragged: each in the stack it was in.
        let dragged = layout(1, &[(1, [0, 0, 50, 24]), (2, [51, 0, 30, 24])]);
        assert_eq!(dragged.rearranged(&two), None);
        // A split, a pane closed.
        let split = layout(
            1,
            &[(1, [0, 0, 40, 24]), (2, [41, 0, 40, 12]), (3, [41, 13, 40, 11])],
        );
        assert_eq!(split.rearranged(&two), None);
        assert_eq!(layout(1, &[(1, [0, 0, 81, 24])]).rearranged(&two), None);
        // A zoom, another tab.
        assert_eq!(zoomed(1).rearranged(&two), None);
        let other = shown_in(
            layout(2, &[(2, [0, 0, 40, 24]), (1, [41, 0, 40, 24])]),
            1,
            2,
        );
        assert_eq!(other.rearranged(&two), None);
        // Another of a stack's panes shown in its place.
        let turn = shown_in(
            layout(1, &[(3, [0, 0, 40, 24]), (2, [41, 0, 40, 24])]),
            3,
            1,
        );
        assert_eq!(turn.rearranged(&two), None);
        // A client's resync names every stack of one pane afresh: with
        // nothing moved, and with a divider dragged at the same time.
        let afresh = |left: usize| {
            shown_in(
                shown_in(
                    layout(1, &[(1, [0, 0, left, 24]), (2, [left + 1, 0, 80 - left, 24])]),
                    1,
                    8,
                ),
                2,
                9,
            )
        };
        assert_eq!(afresh(40).rearranged(&two), None);
        assert_eq!(afresh(50).rearranged(&two), None);
    }

    #[test]
    fn a_carried_panes_tabs_go_from_those_it_showed_to_its_new_stacks() {
        let morph = |from: &[PaneId], along| NavMorph {
            pane_id: 1,
            from: from.to_vec(),
            from_lead: Some(10.),
            along,
        };
        let place = |pane_id, x, opacity, kept| TabPlace {
            pane_id,
            x,
            opacity,
            kept,
        };
        // Dropped into 2's stack: its own tab makes way for 2's, which
        // comes up where it goes; at the end they stand as the stack's.
        assert_eq!(
            morph(&[1], 0.).places(&[2, 1], 100., 100., 10.),
            vec![place(2, 10., 0., true), place(1, 10., 1., true)]
        );
        assert_eq!(
            morph(&[1], 0.5).places(&[2, 1], 100., 100., 10.),
            vec![place(2, 10., 0.5, true), place(1, 60., 1., true)]
        );
        assert_eq!(
            morph(&[1], 1.).places(&[2, 1], 100., 100., 10.),
            vec![place(2, 10., 1., true), place(1, 110., 1., true)]
        );
        // Dragged back out: 2's tab stays behind, fading where it was, and
        // takes no clicks.
        assert_eq!(
            morph(&[2, 1], 0.25).places(&[1], 80., 100., 10.),
            vec![place(1, 70., 1., true), place(2, 10., 0.75, false)]
        );
        // Its old bar scrolled, or along another edge of the window: it
        // goes on from where its tabs stood there, not from the new bar's.
        let scrolled = NavMorph {
            from_lead: Some(-200.),
            ..morph(&[2, 1], 0.)
        };
        assert_eq!(scrolled.places(&[1], 80., 100., 16.)[0].x, -120.);
    }

    #[test]
    fn a_carried_pane_goes_on_from_the_tabs_its_nav_bar_showed() {
        let start = Instant::now();
        let two = || layout(1, &[(1, [0, 0, 40, 24]), (2, [41, 0, 40, 24])]);
        let joined = || active(shown_in(layout(1, &[(1, [0, 0, 81, 24])]), 1, 2), 1);
        let mut slides = SplitSlides::<PaneId>::default();
        slides.advance(Some(two()), &[1, 2], &[], true, start);
        slides.nav(1, 16., [1]);
        slides.nav(2, 10., [2]);
        let (rearranging, _) = slides
            .advance(Some(joined()), &[1], &[], true, start)
            .rearranging
            .unwrap();
        assert_eq!(rearranging.tabs_before(1), Some((Some(16.), &[1][..])));
        // 2 is not carried: it goes out of sight where it stood.
        assert_eq!(rearranging.tabs_before(2), None);
        // A nav bar not painted last frame: the pane showed itself.
        let mut slides = SplitSlides::<PaneId>::default();
        slides.advance(Some(two()), &[1, 2], &[], true, start);
        let (rearranging, _) = slides
            .advance(Some(joined()), &[1], &[], true, start)
            .rearranging
            .unwrap();
        assert_eq!(rearranging.tabs_before(1), Some((None, &[1][..])));
    }

    #[test]
    fn panes_rearranged_go_from_where_they_stood_until_they_get_there() {
        let start = Instant::now();
        let frame = Duration::from_micros(16_667);
        let two = || layout(1, &[(1, [0, 0, 40, 24]), (2, [41, 0, 40, 24])]);
        let joined = || active(shown_in(layout(1, &[(1, [0, 0, 81, 24])]), 1, 2), 1);
        let mut slides = SplitSlides::<PaneId>::default();
        slides.advance(Some(two()), &[1, 2], &[], true, start);
        let (rearranging, first) = slides
            .advance(Some(joined()), &[1], &[], true, start)
            .rearranging
            .unwrap();
        assert_eq!(rearranging.moving().collect::<Vec<_>>(), vec![1]);
        // 2, out of sight in the stack now, as last painted.
        assert_eq!(rearranging.hidden, vec![2]);
        assert!(first > 0. && first < 0.5, "{}", first);
        let mut steps = vec![first];
        let mut now = start;
        loop {
            slides.shown();
            now += frame;
            match slides.advance(Some(joined()), &[1], &[], true, now).rearranging {
                Some((_, along)) => steps.push(along),
                None => break,
            }
        }
        assert!(steps.windows(2).all(|w| w[1] > w[0]), "{:?}", steps);
        assert!(steps.len() as u32 >= MIN_FRAMES - 1, "{:?}", steps);
        // With the setting off, they are where they go at once.
        let mut slides = SplitSlides::<PaneId>::default();
        slides.advance(Some(two()), &[1, 2], &[], false, start);
        assert!(slides
            .advance(Some(joined()), &[1], &[], false, start)
            .rearranging
            .is_none());
    }

    /// `pane_id` on its way from `from` to `to`, its text starting 10 in
    /// from its corner, without a nav bar.
    fn moving_pane(pane_id: PaneId, from: RectF, to: RectF) -> MovingPane {
        MovingPane {
            pane_id,
            from: placed(from, 10.),
            to: placed(to, 10.),
            nav: None,
        }
    }

    /// What `frame` draws of the panes, each painted standing where
    /// `stood` has it, and fills: each spot of the tab once.
    fn rearranged_drawn(frame: &SlideFrame, stood: &[(PaneId, RectF)]) -> Vec<RectF> {
        let mut drawn: Vec<RectF> = stood
            .iter()
            .flat_map(|(pane_id, stood)| {
                frame.pieces_of(*pane_id).unwrap_or(&[]).iter().filter_map(move |piece| {
                    piece
                        .clip
                        .intersection(&translated(*stood, piece.shift))
                })
            })
            .collect();
        drawn.extend(frame.rooms.iter().map(|(_, rect, _)| *rect));
        drawn.extend(frame.backdrop.iter().copied());
        drawn
    }

    #[test]
    fn a_carried_pane_goes_over_the_others_and_each_spot_is_drawn_once() {
        // 3, at the bottom right of `three`, dropped under 1: 1 gives it
        // its lower half, and 2 takes the right side all the way down.
        let [(_, left), (_, top_right), (_, bottom_right)] = three_places();
        let moving = [
            moving_pane(
                3,
                bottom_right,
                rect(100., 230., 405., 270.),
            ),
            moving_pane(
                1,
                left,
                rect(100., 0., 405., 230.),
            ),
            moving_pane(
                2,
                top_right,
                rect(505., 0., 405., 500.),
            ),
        ];
        let stood = [
            (3, rect(100., 230., 405., 270.)),
            (1, rect(100., 0., 405., 230.)),
            (2, rect(505., 0., 405., 500.)),
        ];
        for t in [0., 0.25, 0.5, 0.75, 0.99] {
            let frame = SlideFrame::rearrange(&moving, &[], &[], t, 1., area());
            let drawn = rearranged_drawn(&frame, &stood);
            assert!(!any_overlap(&drawn), "{} {:?}", t, drawn);
            assert!((area_of(&drawn) - area().area()).abs() < 1., "{} {:?}", t, drawn);
            // The one carried is all there, over the others.
            let now = between(bottom_right, rect(100., 230., 405., 270.), t);
            let carried = frame.pieces_of(3).unwrap();
            assert_eq!(area_of(&carried.iter().map(|d| d.clip).collect::<Vec<_>>()), now.area());
        }
        // Where it stood, painted where it goes.
        let start = SlideFrame::rearrange(&moving, &[], &[], 0., 1., area());
        assert_eq!(
            start.pieces_of(3).unwrap(),
            &[Drawn {
                shift: (405., 0.),
                clip: bottom_right,
            }]
        );
        assert!(start.backdrop.is_empty());
        // On its way, room none of them covers is the darker ground.
        let half = SlideFrame::rearrange(&moving, &[], &[], 0.5, 1., area());
        assert!(!half.backdrop.is_empty());
    }

    #[test]
    fn panes_under_a_carried_one_are_cut_short_of_it() {
        let [(_, left), (_, top_right), (_, bottom_right)] = three_places();
        // 1 and 3 swapped: 2 stays, and 3 crosses it on the way over.
        let moving = [
            moving_pane(3, bottom_right, left),
            moving_pane(1, left, bottom_right),
        ];
        let still = [(2, top_right)];
        let frame = SlideFrame::rearrange(&moving, &still, &[], 0.5, 1., area());
        let crossed: Vec<RectF> = frame.pieces_of(2).unwrap().iter().map(|d| d.clip).collect();
        let carried = between(bottom_right, left, 0.5);
        assert!(!any_overlap(&[crossed.as_slice(), &[carried]].concat()));
        let drawn = rearranged_drawn(&frame, &[(3, left), (1, bottom_right), (2, top_right)]);
        assert!(!any_overlap(&drawn), "{:?}", drawn);
        assert!((area_of(&drawn) - area().area()).abs() < 1., "{:?}", drawn);
        // One nobody crosses is drawn as it stands.
        let start = SlideFrame::rearrange(&moving, &still, &[], 0., 1., area());
        assert!(start.pieces_of(2).is_none());
        // Out of sight in a stack now, where it stood under them all.
        let joined = [moving_pane(1, left, area())];
        let hidden = [(2, rect(505., 0., 405., 500.))];
        let frame = SlideFrame::rearrange(&joined, &[], &hidden, 0.5, 1., area());
        assert_eq!(
            frame.pieces_of(2).unwrap(),
            &[Drawn {
                shift: (0., 0.),
                clip: rect(707.5, 0., 202.5, 500.),
            }]
        );
        assert!(frame.backdrop.is_empty());
    }

    #[test]
    fn panes_on_their_way_are_edged_where_they_meet_the_others() {
        let [(_, left), (_, top_right), (_, bottom_right)] = three_places();
        let under_left = rect(100., 230., 405., 270.);
        let (top_left, right) = (rect(100., 0., 405., 230.), rect(505., 0., 405., 500.));
        let moving = [
            moving_pane(3, bottom_right, under_left),
            moving_pane(1, left, top_left),
            moving_pane(2, top_right, right),
        ];
        let frame = SlideFrame::rearrange(&moving, &[], &[], 0.5, 1., area());
        let carried = between(bottom_right, under_left, 0.5);
        // The one carried is edged all round but along the window's
        // bottom, each line just right of or under where it meets another,
        // where a divider runs; the others' edges stop short of it.
        let own: Vec<&RectF> = frame
            .edges
            .iter()
            .filter(|edge| carried.inflate(1., 1.).contains_rect(edge))
            .collect();
        assert_eq!(
            own,
            vec![
                &rect(carried.min_x(), carried.min_y(), 1., carried.height()),
                &rect(carried.max_x(), carried.min_y(), 1., carried.height()),
                &rect(carried.min_x(), carried.min_y(), carried.width(), 1.),
            ]
        );
        let inside = carried.inflate(-1., -1.);
        assert!(frame.edges.iter().all(|edge| {
            own.contains(&edge)
                || edge
                    .intersection(&inside)
                    .map_or(true, |both| both.area() < 0.01)
        }));
        // None runs along the window's own edges.
        let area = area();
        assert!(frame.edges.iter().all(|edge| {
            let upright = edge.width() <= 1.;
            let flat = edge.height() <= 1.;
            let along_side = edge.min_x() <= area.min_x() || edge.max_x() >= area.max_x();
            let along_end = edge.min_y() <= area.min_y() || edge.max_y() >= area.max_y();
            !(upright && along_side) && !(flat && along_end)
        }));
    }

    #[test]
    fn dividers_beside_a_carried_pane_wait_for_it_the_rest_stay() {
        let [(_, left), (_, top_right), (_, bottom_right)] = three_places();
        let under_left = rect(100., 230., 405., 270.);
        let moving = [moving_pane(3, bottom_right, under_left)];
        let still = [(1, left), (2, top_right)];
        let frame = SlideFrame::rearrange(&moving, &still, &[], 0.5, 1., area());
        let clips = frame.divider_clips().unwrap();
        // Not where it is, stood or goes, a line's width round them.
        let carried = between(bottom_right, under_left, 0.5);
        for gone in [carried, bottom_right.inflate(1., 1.), under_left.inflate(1., 1.)] {
            assert!(!any_overlap(&[clips, &[gone]].concat()), "{:?}", clips);
        }
        // Between two panes standing still, above them all, it stays: down
        // between 1 and 2 where neither of the others comes.
        let between_still = rect(504., 0., 2., 200.);
        let kept: f32 = clips
            .iter()
            .filter_map(|clip| clip.intersection(&between_still))
            .map(|both| both.area())
            .sum();
        assert_eq!(kept, between_still.area());
        // Nothing rearranged, none of that.
        assert_eq!(SlideFrame::default().divider_clips(), None);
    }

    #[test]
    fn a_divider_dragged_in_a_grid_carries_nothing() {
        let grid = layout(
            1,
            &[
                (1, [0, 0, 40, 12]),
                (2, [0, 13, 40, 11]),
                (3, [41, 0, 40, 12]),
                (4, [41, 13, 40, 11]),
            ],
        );
        // The right column's divider dragged up past the left's: 1 now
        // ends under where 4 starts, yet still lies left of it.
        let dragged = layout(
            1,
            &[
                (1, [0, 0, 40, 12]),
                (2, [0, 13, 40, 11]),
                (3, [41, 0, 40, 9]),
                (4, [41, 10, 40, 14]),
            ],
        );
        assert_eq!(dragged.rearranged(&grid), None);
        // A stack collapsed to a strip shown in turn as it expands: its
        // neighbour gives way, one of its tabs is out of sight, another in.
        let column = layout(1, &[(1, [0, 0, 81, 22]), (2, [0, 23, 81, 1])]);
        let expanded = shown_in(
            layout(1, &[(1, [0, 0, 81, 12]), (9, [0, 13, 81, 11])]),
            9,
            2,
        );
        assert_eq!(expanded.rearranged(&column), None);
    }
}
