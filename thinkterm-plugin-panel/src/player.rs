//! Showing a panel: the frame its plugin sent last, the rows of its lists
//! near the view, how far its scroll areas are scrolled, and what the
//! pointer is over. Nothing here waits for the plugin. A paint draws what
//! has arrived; a scroll moves at once and asks for the rows it will need
//! next; a hover tints at once.
//!
//! A client paints what [`Player::draw`] gives it with its own renderer,
//! and hands the pointer and the wheel in. What is to go to the plugin
//! comes back from [`Player::click`] and [`Player::wanted`].

use crate::scene::{
    Align, Area, Bounds, Color, Cursor, Font, Frame, Hit, Item, Line, List, Rect, Scroll, Size,
    Text,
};
use crate::wire::{Button, Click, Env, Input, Mods, Rows, RowsWanted};
use serde::Serialize;
use std::collections::HashMap;

/// Items a frame may hold, its scroll areas' included, and a page of rows;
/// the rest are dropped. Far more than a panel shows.
pub const ITEM_LIMIT: usize = 20_000;
/// Rows a list keeps. The ones furthest from the view go first.
pub const ROW_LIMIT: usize = 2_048;
/// The longest text drawn, in characters: the part a panel can show, and
/// no burden on the shaper.
pub const TEXT_LIMIT: usize = 2_000;
/// The most numbers a line or an area is drawn through.
pub const POINT_LIMIT: usize = 8_192;
/// Pages of rows one list may be waiting for at once.
const ASKING_LIMIT: usize = 3;
/// Rows are asked for a screenful at a time, and never fewer than this.
const PAGE_MIN: u32 = 32;
/// A scrollbar's thumb: how wide, how far in from the edge, how short at
/// the shortest.
const THUMB_WIDTH: f32 = 4.0;
const THUMB_GAP: f32 = 2.0;
const THUMB_MIN: f32 = 16.0;

/// One thing to paint, in order: what, moved by `dx`, `dy` from its own
/// units to the panel's, and cut to `clip`.
#[derive(Debug, Clone, Copy)]
pub struct Draw<'a> {
    pub what: Drawn<'a>,
    pub dx: f32,
    pub dy: f32,
    pub clip: Bounds,
}

#[derive(Debug, Clone, Copy)]
pub enum Drawn<'a> {
    Rect(&'a Rect),
    Text(&'a Text),
    Line(&'a Line),
    Area(&'a Area),
    /// A hit the pointer is over, to be filled with its `hover` where it is
    /// in the order: under what the plugin drew after it.
    Hover(&'a Hit),
    /// The thumb of a scroll area or a list taller than it shows, in the
    /// panel's units already.
    Thumb(Bounds),
}

/// What [`Player::draw`] gives, as plain data in the panel's units: for a
/// client that paints in another language -- the page's script, a phone's
/// own toolkit -- and so cannot hold the player's items.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Painting {
    /// The regions the ops are cut to, as left, top, right, bottom; each op
    /// names its own by index.
    pub clips: Vec<[f32; 4]>,
    pub ops: Vec<Op>,
    /// Over what the pointer is, the cursor to show; none over the rest.
    pub cursor: Option<Cursor>,
}

/// One thing to paint, in the panel's units, moved into place already.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Op {
    Rect {
        clip: usize,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        #[serde(skip_serializing_if = "Option::is_none")]
        fill: Option<Color>,
        #[serde(skip_serializing_if = "Option::is_none")]
        border: Option<Color>,
        radius: f32,
    },
    Text {
        clip: usize,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        text: String,
        color: Color,
        font: Font,
        size: Size,
        bold: bool,
        align: Align,
    },
    /// Through `points`, x, y and so on.
    Line {
        clip: usize,
        points: Vec<f32>,
        width: f32,
        color: Color,
    },
    Area {
        clip: usize,
        points: Vec<f32>,
        base: f32,
        color: Color,
        fade: bool,
    },
    /// A scrollbar's thumb, in the client's own colour for one.
    Thumb {
        clip: usize,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
    },
}

/// Where a hit is: which item, and inside what.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Spot {
    /// The frame's item at this index.
    Top(usize),
    /// Item `.1` of the scroll area at index `.0`.
    Scrolled(usize, usize),
    /// Item `.2` of row `.1` of the list at index `.0`.
    Row(usize, u32, usize),
}

struct ScrollState {
    offset: f32,
    /// The last jump taken.
    seq: Option<u32>,
}

struct ListState {
    key: String,
    version: u32,
    count: u32,
    offset: f32,
    /// How far its rows are scrolled sideways.
    offset_x: f32,
    seq: Option<u32>,
    rows: HashMap<u32, Kept>,
    /// Pages asked for and not had: where each starts and ends.
    asking: Vec<(u32, u32)>,
}

/// A row kept, and what it was drawn for: a list's version and the
/// panel's layout. One drawn for others stays on show until it is drawn
/// anew.
struct Kept {
    version: u32,
    layout: u32,
    items: Vec<Item>,
}

pub struct Player {
    env: Env,
    frame: Frame,
    shown: bool,
    /// Counts changes to what [`draw`](Self::draw) gives.
    revision: u64,
    /// Counts what makes the rows drawn before out of date: the panel's
    /// size and fonts changing, which rows are drawn for, and the plugin
    /// starting again, which may draw them otherwise under the same key
    /// and version. A row drawn for another is asked for again.
    layout: u32,
    lists: HashMap<String, ListState>,
    scrolls: HashMap<String, ScrollState>,
    pointer: Option<(f32, f32)>,
    hover: Option<Spot>,
    wanted: Vec<RowsWanted>,
}

impl Player {
    /// A panel of `env`'s size, blank until its plugin sends a frame.
    pub fn new(env: Env) -> Self {
        Self {
            env,
            frame: Frame::default(),
            shown: false,
            revision: 0,
            layout: 0,
            lists: HashMap::new(),
            scrolls: HashMap::new(),
            pointer: None,
            hover: None,
            wanted: Vec::new(),
        }
    }

    pub fn env(&self) -> &Env {
        &self.env
    }

    /// Whether the plugin has drawn the panel yet.
    pub fn shown(&self) -> bool {
        self.shown
    }

    /// Whether the panel's last frame asks for its extended view.
    pub fn extend(&self) -> bool {
        self.frame.extend
    }

    /// Changes whenever what [`draw`](Self::draw) gives may have: a client
    /// that keeps what it painted can paint it again while this stays.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// The panel's size, fonts or theme changed. True when they did, and
    /// the plugin is to be told -- before it is asked for what
    /// [`wanted`](Self::wanted) gives from now on, which is drawn for them.
    pub fn set_env(&mut self, env: Env) -> bool {
        if env == self.env {
            return false;
        }
        self.env = env;
        // Rows are drawn for a size: the ones on show are asked for again,
        // and what was asked for the old size is not taken when it comes,
        // nor asked for if it is not yet.
        self.layout = self.layout.wrapping_add(1);
        for state in self.lists.values_mut() {
            state.asking.clear();
        }
        self.wanted.clear();
        self.settle();
        true
    }

    /// The plugin drew the panel anew.
    pub fn frame(&mut self, mut frame: Frame) {
        let mut budget = ITEM_LIMIT;
        keep(&mut frame.items, &mut budget, false);
        let mut lists = HashMap::new();
        let mut scrolls = HashMap::new();
        for item in &frame.items {
            match item {
                Item::List(list) => {
                    let mut state = self.lists.remove(&list.id).unwrap_or_else(|| ListState {
                        key: list.key.clone(),
                        version: list.version,
                        count: 0,
                        offset: 0.0,
                        offset_x: 0.0,
                        seq: None,
                        rows: HashMap::new(),
                        asking: Vec::new(),
                    });
                    state.follow(list);
                    lists.insert(list.id.clone(), state);
                }
                Item::Scroll(scroll) => {
                    let mut state = self.scrolls.remove(&scroll.id).unwrap_or(ScrollState {
                        offset: 0.0,
                        seq: None,
                    });
                    if let Some(jump) = scroll.top.filter(|jump| state.seq != Some(jump.seq)) {
                        state.offset = jump.to;
                        state.seq = Some(jump.seq);
                    }
                    scrolls.insert(scroll.id.clone(), state);
                }
                _ => {}
            }
        }
        self.lists = lists;
        self.scrolls = scrolls;
        self.frame = frame;
        self.shown = true;
        self.settle();
    }

    /// Rows the plugin sent for a list: taken when they answer a page it
    /// waits for, drawn for the list and the panel as they are now. Rows
    /// it was asked for and left out are kept empty, so they are not asked
    /// for again and again.
    pub fn rows(&mut self, rows: Rows) {
        let layout = self.layout;
        let Some(state) = self.lists.get_mut(&rows.list) else {
            return;
        };
        if state.key != rows.key || state.version != rows.version || rows.layout != layout {
            return;
        }
        // Only an answer to what is asked for is taken: one for a page let
        // go of since is not the list's any more.
        let Some(at) = state.asking.iter().position(|(from, _)| *from == rows.from) else {
            return;
        };
        let (from, to) = state.asking.swap_remove(at);
        let to = to.min(state.count);
        let version = state.version;
        let mut budget = ITEM_LIMIT;
        let mut next = from;
        for mut items in rows.rows {
            if next >= to {
                break;
            }
            keep(&mut items, &mut budget, true);
            let kept = Kept {
                version,
                layout,
                items,
            };
            state.rows.insert(next, kept);
            next += 1;
        }
        for missing in next..to {
            let empty = Kept {
                version,
                layout,
                items: Vec::new(),
            };
            state.rows.insert(missing, empty);
        }
        self.settle();
    }

    /// What is to be asked of the plugin: the rows its lists need next.
    pub fn wanted(&mut self) -> Vec<RowsWanted> {
        std::mem::take(&mut self.wanted)
    }

    /// The plugin's program was started again: what the last one was asked
    /// will not be answered, and is asked again, and so are the rows on
    /// show, which stay until the new ones come.
    pub fn restarted(&mut self) {
        self.layout = self.layout.wrapping_add(1);
        for state in self.lists.values_mut() {
            state.asking.clear();
        }
        self.wanted.clear();
        self.settle();
    }

    /// The pointer moved to `x`, `y`. True when what it is over changed,
    /// and the panel is to be painted again.
    pub fn pointer_moved(&mut self, x: f32, y: f32) -> bool {
        self.pointer = Some((x, y));
        self.rehover()
    }

    pub fn pointer_left(&mut self) -> bool {
        self.pointer = None;
        self.rehover()
    }

    /// The cursor over what the pointer is on; `None` where nothing
    /// answers a click.
    pub fn cursor(&self) -> Option<Cursor> {
        let (hit, ..) = self.hit(self.hover?)?;
        Some(hit.cursor)
    }

    /// The wheel turned `dx` units across and `dy` down at `x`, `y`: the
    /// region there scrolls -- down for a positive `dy`, right for a
    /// positive `dx` -- whichever way the turn mostly went, so a stroke
    /// down a trackpad does not drift sideways. True when it moved.
    pub fn wheel(&mut self, x: f32, y: f32, dx: f32, dy: f32) -> bool {
        if !dx.is_finite() || !dy.is_finite() {
            return false;
        }
        let (dx, dy) = if dx.abs() > dy.abs() {
            (dx, 0.0)
        } else {
            (0.0, dy)
        };
        let mut moved = false;
        for item in self.frame.items.iter().rev() {
            let region = item.bounds();
            if !region.contains(x, y) {
                continue;
            }
            match item {
                Item::List(list) => {
                    if let Some(state) = self.lists.get_mut(&list.id) {
                        let before = (state.offset, state.offset_x);
                        state.offset = (before.0 + dy).clamp(0.0, list_room(list, state.count));
                        state.offset_x = (before.1 + dx).clamp(0.0, list_room_x(list));
                        moved = (state.offset, state.offset_x) != before;
                    }
                    break;
                }
                Item::Scroll(scroll) => {
                    if let Some(state) = self.scrolls.get_mut(&scroll.id) {
                        let before = state.offset;
                        state.offset = (before + dy).clamp(0.0, scroll_room(scroll));
                        moved = state.offset != before;
                    }
                    break;
                }
                _ => {}
            }
        }
        if moved {
            self.settle();
        }
        moved
    }

    /// A press at `x`, `y`: the plugin is to hear of it when it landed on a
    /// hit.
    pub fn click(
        &mut self,
        x: f32,
        y: f32,
        button: Button,
        count: u32,
        mods: Mods,
    ) -> Option<Input> {
        self.pointer = Some((x, y));
        self.rehover();
        let (hit, dx, dy) = self.hit(self.spot_at(x, y)?)?;
        Some(Input::Click(Click {
            id: hit.id.clone(),
            x: x - (hit.x + dx),
            y: y - (hit.y + dy),
            button,
            count,
            mods,
        }))
    }

    /// What to paint, in order, leaving out what does not show.
    pub fn draw(&self) -> Vec<Draw<'_>> {
        let panel = self.panel();
        let mut out = Vec::new();
        for (index, item) in self.frame.items.iter().enumerate() {
            match item {
                Item::Scroll(scroll) => {
                    let Some(state) = self.scrolls.get(&scroll.id) else {
                        continue;
                    };
                    let clip = item.bounds().intersect(&panel);
                    if clip.is_empty() {
                        continue;
                    }
                    let (dx, dy) = (scroll.x, scroll.y - state.offset);
                    for (inner, child) in scroll.items.iter().enumerate() {
                        self.push(&mut out, child, Spot::Scrolled(index, inner), dx, dy, clip);
                    }
                    let room = scroll_room(scroll);
                    if let Some(thumb) = thumb(item.bounds(), scroll.height, state.offset, room) {
                        push_thumb(&mut out, thumb, panel);
                    }
                }
                Item::List(list) => {
                    let Some(state) = self.lists.get(&list.id) else {
                        continue;
                    };
                    let clip = item.bounds().intersect(&panel);
                    if clip.is_empty() {
                        continue;
                    }
                    let (first, last) = state.visible(list);
                    for row in first..last {
                        let Some(Kept { items, .. }) = state.rows.get(&row) else {
                            continue;
                        };
                        let dy = list.y + row as f32 * list.row - state.offset;
                        for (inner, child) in items.iter().enumerate() {
                            let (dx, cut) = across(list, state.offset_x, child, clip);
                            let spot = Spot::Row(index, row, inner);
                            self.push(&mut out, child, spot, dx, dy, cut);
                        }
                    }
                    let height = state.count as f32 * list.row;
                    let room = list_room(list, state.count);
                    if let Some(thumb) = thumb(item.bounds(), height, state.offset, room) {
                        push_thumb(&mut out, thumb, panel);
                    }
                    if let Some(thumb) = thumb_x(list, state.offset_x) {
                        push_thumb(&mut out, thumb, panel);
                    }
                }
                _ => self.push(&mut out, item, Spot::Top(index), 0.0, 0.0, panel),
            }
        }
        out
    }

    /// [`draw`](Self::draw), as plain data.
    pub fn painting(&self) -> Painting {
        let mut clips: Vec<[f32; 4]> = Vec::new();
        let mut ops = Vec::new();
        for draw in self.draw() {
            let clip = [
                draw.clip.left,
                draw.clip.top,
                draw.clip.right,
                draw.clip.bottom,
            ];
            if clips.last() != Some(&clip) {
                clips.push(clip);
            }
            let clip = clips.len() - 1;
            let (dx, dy) = (draw.dx, draw.dy);
            let moved = |points: &[f32]| -> Vec<f32> {
                points
                    .iter()
                    .enumerate()
                    .map(|(n, value)| value + if n % 2 == 0 { dx } else { dy })
                    .collect()
            };
            ops.push(match draw.what {
                Drawn::Rect(rect) => Op::Rect {
                    clip,
                    x: rect.x + dx,
                    y: rect.y + dy,
                    w: rect.w,
                    h: rect.h,
                    fill: rect.fill,
                    border: rect.border,
                    radius: rect.radius,
                },
                Drawn::Hover(hit) => Op::Rect {
                    clip,
                    x: hit.x + dx,
                    y: hit.y + dy,
                    w: hit.w,
                    h: hit.h,
                    fill: hit.hover,
                    border: None,
                    radius: hit.radius,
                },
                Drawn::Text(text) => Op::Text {
                    clip,
                    x: text.x + dx,
                    y: text.y + dy,
                    w: text.w,
                    h: text.h,
                    text: text.text.clone(),
                    color: text.color,
                    font: text.font,
                    size: text.size,
                    bold: text.bold,
                    align: text.align,
                },
                Drawn::Line(line) => Op::Line {
                    clip,
                    points: moved(&line.points),
                    width: line.width,
                    color: line.color,
                },
                Drawn::Area(area) => Op::Area {
                    clip,
                    points: moved(&area.points),
                    base: area.base + dy,
                    color: area.color,
                    fade: area.fade,
                },
                Drawn::Thumb(thumb) => Op::Thumb {
                    clip,
                    x: thumb.left,
                    y: thumb.top,
                    w: thumb.width(),
                    h: thumb.height(),
                },
            });
        }
        Painting {
            clips,
            ops,
            cursor: self.cursor(),
        }
    }

    /// Rows a list keeps now, for looking at what the player holds.
    pub fn rows_kept(&self) -> usize {
        self.lists.values().map(|state| state.rows.len()).sum()
    }

    fn push<'a>(
        &self,
        out: &mut Vec<Draw<'a>>,
        item: &'a Item,
        spot: Spot,
        dx: f32,
        dy: f32,
        clip: Bounds,
    ) {
        if !item.bounds().offset(dx, dy).touches(&clip) {
            return;
        }
        let what = match item {
            Item::Rect(rect) => Drawn::Rect(rect),
            Item::Text(text) => Drawn::Text(text),
            Item::Line(line) => Drawn::Line(line),
            Item::Area(area) => Drawn::Area(area),
            Item::Hit(hit) if hit.hover.is_some() && self.hover == Some(spot) => Drawn::Hover(hit),
            Item::Hit(_) | Item::Scroll(_) | Item::List(_) => return,
        };
        out.push(Draw { what, dx, dy, clip });
    }

    fn panel(&self) -> Bounds {
        Bounds::new(0.0, 0.0, self.env.width, self.env.height)
    }

    /// Brings the regions in line with the frame and the size: scrolled no
    /// further than they reach, the rows near each list's view asked for
    /// and the rest let go, and the hover found again.
    fn settle(&mut self) {
        self.revision += 1;
        for item in &self.frame.items {
            match item {
                Item::List(list) => {
                    if let Some(state) = self.lists.get_mut(&list.id) {
                        state.offset = state.offset.clamp(0.0, list_room(list, state.count));
                        state.offset_x = state.offset_x.clamp(0.0, list_room_x(list));
                        state.serve(list, self.layout, &mut self.wanted);
                    }
                }
                Item::Scroll(scroll) => {
                    if let Some(state) = self.scrolls.get_mut(&scroll.id) {
                        state.offset = state.offset.clamp(0.0, scroll_room(scroll));
                    }
                }
                _ => {}
            }
        }
        self.rehover();
    }

    fn rehover(&mut self) -> bool {
        let hover = self.pointer.and_then(|(x, y)| self.spot_at(x, y));
        let changed = hover != self.hover;
        if changed {
            self.revision += 1;
        }
        self.hover = hover;
        changed
    }

    /// The topmost hit at `x`, `y`.
    fn spot_at(&self, x: f32, y: f32) -> Option<Spot> {
        if !self.panel().contains(x, y) {
            return None;
        }
        for (index, item) in self.frame.items.iter().enumerate().rev() {
            match item {
                Item::Hit(_) if item.bounds().contains(x, y) => return Some(Spot::Top(index)),
                Item::Scroll(scroll) if item.bounds().contains(x, y) => {
                    let Some(state) = self.scrolls.get(&scroll.id) else {
                        continue;
                    };
                    let (dx, dy) = (scroll.x, scroll.y - state.offset);
                    let found = scroll.items.iter().enumerate().rev().find(|(_, child)| {
                        matches!(child, Item::Hit(_))
                            && child.bounds().offset(dx, dy).contains(x, y)
                    });
                    if let Some((inner, _)) = found {
                        return Some(Spot::Scrolled(index, inner));
                    }
                }
                Item::List(list) if item.bounds().contains(x, y) => {
                    let Some(state) = self.lists.get(&list.id) else {
                        continue;
                    };
                    let row = ((y - list.y + state.offset) / list.row).floor();
                    if !(row >= 0.0 && row < state.count as f32) {
                        continue;
                    }
                    let row = row as u32;
                    let Some(Kept { items, .. }) = state.rows.get(&row) else {
                        continue;
                    };
                    let dy = list.y + row as f32 * list.row - state.offset;
                    let found = items.iter().enumerate().rev().find(|(_, child)| {
                        let (dx, cut) = across(list, state.offset_x, child, item.bounds());
                        matches!(child, Item::Hit(_))
                            && cut.contains(x, y)
                            && child.bounds().offset(dx, dy).contains(x, y)
                    });
                    if let Some((inner, _)) = found {
                        return Some(Spot::Row(index, row, inner));
                    }
                }
                _ => {}
            }
        }
        None
    }

    /// The hit at `spot`, and what moves it into the panel's units.
    fn hit(&self, spot: Spot) -> Option<(&Hit, f32, f32)> {
        let (item, dx, dy) = match spot {
            Spot::Top(index) => (self.frame.items.get(index)?, 0.0, 0.0),
            Spot::Scrolled(index, inner) => {
                let Item::Scroll(scroll) = self.frame.items.get(index)? else {
                    return None;
                };
                let state = self.scrolls.get(&scroll.id)?;
                (scroll.items.get(inner)?, scroll.x, scroll.y - state.offset)
            }
            Spot::Row(index, row, inner) => {
                let Item::List(list) = self.frame.items.get(index)? else {
                    return None;
                };
                let state = self.lists.get(&list.id)?;
                let dy = list.y + row as f32 * list.row - state.offset;
                let child = state.rows.get(&row)?.items.get(inner)?;
                let (dx, _) = across(list, state.offset_x, child, Bounds::EMPTY);
                (child, dx, dy)
            }
        };
        match item {
            Item::Hit(hit) => Some((hit, dx, dy)),
            _ => None,
        }
    }
}

impl ListState {
    /// Takes in the list as the new frame draws it.
    fn follow(&mut self, list: &List) {
        if self.key != list.key {
            self.key = list.key.clone();
            self.rows.clear();
            self.asking.clear();
            self.offset = 0.0;
            self.offset_x = 0.0;
            self.seq = None;
        } else if self.version != list.version {
            // What is on show stays until it is drawn anew; what was asked
            // for the old version comes back as that, and is dropped.
            self.asking.clear();
        }
        self.version = list.version;
        if list.count < self.count {
            let count = list.count;
            self.rows.retain(|row, _| *row < count);
            self.asking.retain(|(from, _)| *from < count);
        }
        self.count = list.count;
        if let Some(jump) = list.top.filter(|jump| self.seq != Some(jump.seq)) {
            self.offset = jump.to * list.row;
            self.seq = Some(jump.seq);
        }
    }

    /// The rows that show, as `first..last`.
    fn visible(&self, list: &List) -> (u32, u32) {
        if self.count == 0 {
            return (0, 0);
        }
        let first = ((self.offset / list.row).floor().max(0.0) as u32).min(self.count - 1);
        let last = ((self.offset + list.h) / list.row).ceil().max(0.0) as u32;
        (first, last.clamp(first + 1, self.count))
    }

    /// Asks for the pages within a screen of the view that are neither had
    /// nor asked for -- those showing first, then the ones below, then the
    /// ones above -- and lets go of the rows more than a page beyond them,
    /// and of asking for them: one not answered by then is asked for again
    /// if it comes back. What is asked for is always within what is kept:
    /// a page let go as soon as it came would be asked for again, and
    /// again. Counted in u64, so a list of u32::MAX rows adds up.
    fn serve(&mut self, list: &List, layout: u32, wanted: &mut Vec<RowsWanted>) {
        if self.count == 0 {
            self.rows.clear();
            self.asking.clear();
            return;
        }
        let count = u64::from(self.count);
        let screen = u64::from(screen_rows(list));
        let page = screen.max(u64::from(PAGE_MIN));
        let (first, last) = self.visible(list);
        let (first, last) = (u64::from(first), u64::from(last));
        let low = first.saturating_sub(screen) / page;
        let high = ((last + screen).min(count) - 1) / page;
        let (shown_low, shown_high) = (first / page, (last - 1) / page);
        let kept = low.saturating_sub(1) * page..(high + 2) * page;
        self.rows.retain(|row, _| kept.contains(&u64::from(*row)));
        self.asking
            .retain(|(from, to)| u64::from(*from) < kept.end && u64::from(*to) > kept.start);
        if self.rows.len() > ROW_LIMIT {
            let middle = first + (last - first) / 2;
            let mut rows: Vec<u32> = self.rows.keys().copied().collect();
            rows.sort_by_key(|row| u64::from(*row).abs_diff(middle));
            for row in rows.into_iter().skip(ROW_LIMIT) {
                self.rows.remove(&row);
            }
        }
        let order = (shown_low..=shown_high)
            .chain(shown_high + 1..=high)
            .chain((low..shown_low).rev());
        for number in order {
            if self.asking.len() >= ASKING_LIMIT || self.rows.len() >= ROW_LIMIT {
                break;
            }
            // Below `count`, which is a u32.
            let from = (number * page) as u32;
            let to = (u64::from(from) + page).min(count) as u32;
            let asked = self
                .asking
                .iter()
                .any(|(start, end)| *start < to && *end > from);
            let had = (from..to).all(|row| {
                self.rows
                    .get(&row)
                    .is_some_and(|kept| kept.version == self.version && kept.layout == layout)
            });
            if asked || had {
                continue;
            }
            self.asking.push((from, to));
            wanted.push(RowsWanted {
                list: list.id.clone(),
                key: self.key.clone(),
                version: self.version,
                layout,
                from,
                to,
            });
        }
    }
}

/// Rows in a screenful of `list`: at least one, and never so many that a
/// few pages of them do not fit in [`ROW_LIMIT`].
fn screen_rows(list: &List) -> u32 {
    ((list.h / list.row).ceil() as u32).clamp(1, ROW_LIMIT as u32 / 8)
}

/// How far a list scrolls.
fn list_room(list: &List, count: u32) -> f32 {
    (count as f32 * list.row - list.h).max(0.0)
}

/// How far a list's rows scroll sideways.
fn list_room_x(list: &List) -> f32 {
    (list.width - list.w).max(0.0)
}

/// Where item `child` of a row of `list` goes across, and what it is cut
/// to: one starting past the list's fixed part moves with the rows' scroll
/// sideways, and is cut where that part ends; the rest stay.
fn across(list: &List, offset_x: f32, child: &Item, clip: Bounds) -> (f32, Bounds) {
    if list_room_x(list) <= 0.0 || child.bounds().left < list.fixed {
        return (list.x, clip);
    }
    let left = clip.left.max(list.x + list.fixed);
    (list.x - offset_x, Bounds { left, ..clip })
}

fn scroll_room(scroll: &Scroll) -> f32 {
    (scroll.height - scroll.h).max(0.0)
}

/// Where the thumb of a region `height` tall, scrolled `offset` of its
/// `room`, goes: along the right edge of `region`, as long as the share of
/// it that shows. None when all of it shows.
fn thumb(region: Bounds, height: f32, offset: f32, room: f32) -> Option<Bounds> {
    let view = region.height();
    if room < 0.5 || height <= 0.0 || view <= 0.0 {
        return None;
    }
    let length = (view * view / height).clamp(THUMB_MIN.min(view), view);
    let top = region.top + (offset / room) * (view - length);
    let left = region.right - THUMB_WIDTH - THUMB_GAP;
    Some(Bounds::new(left, top, THUMB_WIDTH, length))
}

/// Where the thumb of a list's scroll sideways goes: along its bottom
/// edge, under the part that scrolls and clear of the other thumb, as long
/// as the share of that part that shows. None when all of it shows.
fn thumb_x(list: &List, offset_x: f32) -> Option<Bounds> {
    let room = list_room_x(list);
    let left = list.x + list.fixed.clamp(0.0, list.w);
    let right = list.x + list.w - THUMB_WIDTH - THUMB_GAP * 2.0;
    let view = right - left;
    if room < 0.5 || view <= 0.0 {
        return None;
    }
    let shown = (list.w - list.fixed).max(1.0);
    let length = (view * shown / (shown + room)).clamp(THUMB_MIN.min(view), view);
    let x = left + (offset_x / room) * (view - length);
    let top = list.y + list.h - THUMB_WIDTH - THUMB_GAP;
    Some(Bounds::new(x, top, length, THUMB_WIDTH))
}

fn push_thumb(out: &mut Vec<Draw<'_>>, thumb: Bounds, panel: Bounds) {
    out.push(Draw {
        what: Drawn::Thumb(thumb),
        dx: 0.0,
        dy: 0.0,
        clip: panel,
    });
}

/// Keeps what can be drawn of `items`, and no more than `budget` of them
/// between these and any already kept: regions do not go in regions, and
/// an item whose numbers are not all numbers is left out. Text and lines
/// are cut to their limits.
fn keep(items: &mut Vec<Item>, budget: &mut usize, nested: bool) {
    items.retain_mut(|item| {
        if *budget == 0 || !drawable(item) {
            return false;
        }
        match item {
            Item::Scroll(_) | Item::List(_) if nested => return false,
            Item::Scroll(scroll) => {
                *budget -= 1;
                keep(&mut scroll.items, budget, true);
                return true;
            }
            Item::Text(text) => {
                if let Some((cut, _)) = text.text.char_indices().nth(TEXT_LIMIT) {
                    text.text.truncate(cut);
                }
            }
            Item::Line(Line { points, .. }) | Item::Area(Area { points, .. }) => {
                points.truncate(POINT_LIMIT);
                if points.len() % 2 == 1 {
                    points.pop();
                }
            }
            _ => {}
        }
        *budget -= 1;
        true
    });
}

fn drawable(item: &Item) -> bool {
    let finite = |values: &[f32]| values.iter().all(|value| value.is_finite());
    match item {
        Item::Rect(rect) => finite(&[rect.x, rect.y, rect.w, rect.h, rect.radius]),
        Item::Text(text) => finite(&[text.x, text.y, text.w, text.h]),
        Item::Line(line) => finite(&line.points) && line.width.is_finite() && line.width > 0.0,
        Item::Area(area) => finite(&area.points) && area.base.is_finite(),
        Item::Hit(hit) => finite(&[hit.x, hit.y, hit.w, hit.h, hit.radius]),
        Item::Scroll(scroll) => {
            finite(&[scroll.x, scroll.y, scroll.w, scroll.h, scroll.height])
                && scroll.top.is_none_or(|jump| jump.to.is_finite())
        }
        Item::List(list) => {
            finite(&[list.x, list.y, list.w, list.h, list.width, list.fixed])
                && list.row.is_finite()
                && list.row >= 1.0
                && list.top.is_none_or(|jump| jump.to.is_finite())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scene::{Color, Jump, Token};
    use crate::wire::{MonoMetrics, TextMetrics};

    fn env(width: f32, height: f32) -> Env {
        let text = TextMetrics {
            size: 13.0,
            line: 18.0,
        };
        Env {
            width,
            height,
            scale: 2.0,
            dark: true,
            small: text,
            body: text,
            title: text,
            mono: MonoMetrics {
                size: 12.0,
                line: 18.0,
                advance: 7.0,
            },
            locale: String::new(),
            cwd: None,
            remote: None,
            can_extend: false,
            close: None,
        }
    }

    fn hit(id: &str, x: f32, y: f32, w: f32, h: f32) -> Item {
        Item::Hit(Hit {
            id: id.into(),
            x,
            y,
            w,
            h,
            hover: Some(Token::BgHover.into()),
            radius: 0.0,
            cursor: Cursor::Pointer,
        })
    }

    fn list(count: u32, key: &str, top: Option<Jump>) -> Item {
        Item::List(List {
            id: "l".into(),
            x: 10.0,
            y: 100.0,
            w: 200.0,
            h: 200.0,
            count,
            row: 20.0,
            key: key.into(),
            version: 0,
            width: 0.0,
            fixed: 0.0,
            top,
        })
    }

    /// Row `index` of a list: a hit across it, and its label.
    fn row(index: u32) -> Vec<Item> {
        vec![
            hit(&format!("row:{index}"), 0.0, 0.0, 200.0, 20.0),
            Item::Text(Text {
                x: 4.0,
                y: 0.0,
                w: 190.0,
                h: 20.0,
                text: format!("row {index}"),
                color: Color::default(),
                font: Default::default(),
                size: Default::default(),
                bold: false,
                align: Default::default(),
            }),
        ]
    }

    /// Answers what the player wants with rows made by `row`.
    fn answer(player: &mut Player) -> Vec<RowsWanted> {
        let wanted = player.wanted();
        answer_these(player, &wanted);
        wanted
    }

    fn answer_these(player: &mut Player, wanted: &[RowsWanted]) {
        for want in wanted {
            player.rows(Rows {
                list: want.list.clone(),
                key: want.key.clone(),
                version: want.version,
                layout: want.layout,
                from: want.from,
                rows: (want.from..want.to).map(row).collect(),
            });
        }
    }

    fn texts(player: &Player) -> Vec<String> {
        player
            .draw()
            .iter()
            .filter_map(|draw| match draw.what {
                Drawn::Text(text) => Some(text.text.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_list_asks_for_the_rows_it_shows_and_the_next_screen() {
        let mut player = Player::new(env(300.0, 400.0));
        player.frame(Frame {
            items: vec![list(10_000, "a", None)],
            ..Frame::default()
        });
        let wanted = player.wanted();
        // Ten rows show; a page is PAGE_MIN rows, which covers the next
        // screen too, and there is nothing above.
        assert_eq!(wanted.len(), 1, "{wanted:?}");
        assert_eq!((wanted[0].from, wanted[0].to), (0, 32));
        assert!(player.wanted().is_empty(), "asked once");
        player.rows(Rows {
            list: "l".into(),
            key: "a".into(),
            version: 0,
            layout: 0,
            from: 0,
            rows: (0..32).map(row).collect(),
        });
        let shown = texts(&player);
        assert_eq!(shown.first().map(String::as_str), Some("row 0"));
        assert_eq!(shown.len(), 10, "only the rows that show are drawn");
        assert!(player.wanted().is_empty(), "the next screen is had");
    }

    #[test]
    fn scrolling_moves_at_once_asks_ahead_and_lets_far_rows_go() {
        let mut player = Player::new(env(300.0, 400.0));
        player.frame(Frame {
            items: vec![list(10_000, "a", None)],
            ..Frame::default()
        });
        answer(&mut player);
        assert!(player.wheel(50.0, 150.0, 0.0, 20.0 * 100.0));
        let shown = texts(&player);
        assert!(
            shown.is_empty(),
            "rows 100 and on are not here yet: {shown:?}"
        );
        let wanted = answer(&mut player);
        assert!(
            wanted.iter().any(|want| want.from <= 100 && want.to > 100),
            "{wanted:?}"
        );
        assert_eq!(texts(&player).first().map(String::as_str), Some("row 100"));
        assert!(
            player.rows_kept() < 100,
            "the first rows are let go: {}",
            player.rows_kept()
        );

        let mut turns = 0;
        while player.wheel(50.0, 150.0, 0.0, 20.0 * 17.0) {
            answer(&mut player);
            turns += 1;
            assert!(turns < 1_000);
            // What is kept stays near the view: a page or so to each side.
            assert!(player.rows_kept() <= 4 * 32, "{}", player.rows_kept());
        }
        answer(&mut player);
        assert!(player.wanted().is_empty(), "nothing asked twice");
        assert_eq!(texts(&player).last().map(String::as_str), Some("row 9999"));
    }

    #[test]
    fn a_new_key_starts_over_and_a_jump_is_taken_once() {
        let mut player = Player::new(env(300.0, 400.0));
        player.frame(Frame {
            items: vec![list(1_000, "a", None)],
            ..Frame::default()
        });
        answer(&mut player);
        player.wheel(50.0, 150.0, 0.0, 400.0);
        answer(&mut player);
        assert_eq!(texts(&player)[0], "row 20");
        // The same list drawn again keeps its place and its rows.
        player.frame(Frame {
            items: vec![list(1_000, "a", None)],
            ..Frame::default()
        });
        assert_eq!(texts(&player)[0], "row 20");
        assert!(player.wanted().is_empty());
        // Other rows start at the top, with none of the old ones.
        player.frame(Frame {
            items: vec![list(1_000, "b", None)],
            ..Frame::default()
        });
        assert!(texts(&player).is_empty());
        let wanted = player.wanted();
        assert!(wanted.iter().all(|want| want.key == "b") && wanted[0].from == 0);
        // Rows for the old key are not taken.
        player.rows(Rows {
            list: "l".into(),
            key: "a".into(),
            version: 0,
            layout: 0,
            from: 0,
            rows: (0..32).map(row).collect(),
        });
        assert!(texts(&player).is_empty());

        let jump = Some(Jump { to: 500.0, seq: 1 });
        player.frame(Frame {
            items: vec![list(1_000, "b", jump)],
            ..Frame::default()
        });
        answer(&mut player);
        assert_eq!(texts(&player)[0], "row 500");
        player.wheel(50.0, 150.0, 0.0, 200.0);
        player.frame(Frame {
            items: vec![list(1_000, "b", jump)],
            ..Frame::default()
        });
        answer(&mut player);
        assert_eq!(
            texts(&player)[0],
            "row 510",
            "the same jump is not taken twice"
        );
    }

    #[test]
    fn a_new_version_redraws_the_rows_in_place() {
        let mut player = Player::new(env(300.0, 400.0));
        let versioned = |version| {
            let Item::List(list) = list(1_000, "a", None) else {
                unreachable!()
            };
            Item::List(list.version(version))
        };
        player.frame(Frame {
            items: vec![versioned(0)],
            ..Frame::default()
        });
        answer(&mut player);
        player.wheel(50.0, 150.0, 0.0, 400.0);
        answer(&mut player);
        assert_eq!(texts(&player)[0], "row 20");

        player.frame(Frame {
            items: vec![versioned(1)],
            ..Frame::default()
        });
        assert_eq!(
            texts(&player)[0],
            "row 20",
            "the old rows stay on show, in place"
        );
        let wanted = player.wanted();
        assert!(
            !wanted.is_empty() && wanted.iter().all(|want| want.version == 1),
            "{wanted:?}"
        );
        // An answer to the old version is dropped; the new one is taken.
        player.rows(Rows {
            list: "l".into(),
            key: "a".into(),
            version: 0,
            layout: 0,
            from: wanted[0].from,
            rows: vec![Vec::new(); 64],
        });
        assert_eq!(texts(&player)[0], "row 20");
        player.rows(Rows {
            list: "l".into(),
            key: "a".into(),
            version: 1,
            layout: 0,
            from: wanted[0].from,
            rows: vec![Vec::new(); 64],
        });
        assert!(texts(&player).is_empty(), "drawn anew: empty rows");
    }

    #[test]
    fn a_new_size_asks_again_and_what_was_drawn_for_the_old_is_not_taken() {
        let mut player = Player::new(env(300.0, 400.0));
        player.frame(Frame {
            items: vec![list(1_000, "a", None)],
            ..Frame::default()
        });
        let old = player.wanted();
        assert!(player.set_env(env(360.0, 400.0)));
        assert!(texts(&player).is_empty(), "nothing was had yet");
        let new = player.wanted();
        assert!(new.iter().all(|want| want.layout == 1), "{new:?}");
        // The answer to the first ask comes: drawn for the old size, it is
        // not taken; the one asked for the new size is.
        for want in &old {
            player.rows(Rows {
                list: want.list.clone(),
                key: want.key.clone(),
                version: want.version,
                layout: want.layout,
                from: want.from,
                rows: (want.from..want.to).map(row).collect(),
            });
        }
        assert!(
            texts(&player).is_empty(),
            "the old size's rows are not taken"
        );
        answer_these(&mut player, &new);
        assert_eq!(texts(&player)[0], "row 0");

        // Rows on show stay while the new size's come.
        assert!(player.set_env(env(300.0, 400.0)));
        assert_eq!(texts(&player)[0], "row 0");
        assert!(!player.wanted().is_empty());
    }

    #[test]
    fn rows_are_asked_for_again_from_a_plugin_started_again() {
        let mut player = Player::new(env(300.0, 400.0));
        let frame = || Frame {
            items: vec![list(1_000, "a", None)],
            ..Frame::default()
        };
        player.frame(frame());
        answer(&mut player);
        assert_eq!(texts(&player)[0], "row 0");
        // Reloaded, the new run draws the same key and version, and may
        // have other rows under them.
        player.restarted();
        player.frame(frame());
        let wanted = player.wanted();
        assert!(!wanted.is_empty(), "the rows on show are asked for again");
        assert_eq!(texts(&player)[0], "row 0", "and stay until the new come");
        for want in &wanted {
            player.rows(Rows {
                list: want.list.clone(),
                key: want.key.clone(),
                version: want.version,
                layout: want.layout,
                from: want.from,
                rows: (want.from..want.to)
                    .map(|n| vec![Text::new(0.0, 0.0, 100.0, 20.0, format!("new {n}")).into()])
                    .collect(),
            });
        }
        assert_eq!(texts(&player)[0], "new 0");
        assert!(player.wanted().is_empty(), "asked once");
    }

    /// A list 200 wide with rows 600 wide, the first 40 of them fixed: a
    /// number in the fixed part, the line and a hit on it past it.
    fn wide_list(player: &mut Player) {
        let Item::List(list) = list(100, "a", None) else {
            unreachable!()
        };
        player.frame(Frame {
            items: vec![Item::List(list.wide(600.0, 40.0))],
            ..Frame::default()
        });
        for want in player.wanted() {
            player.rows(Rows {
                list: want.list.clone(),
                key: want.key.clone(),
                version: want.version,
                layout: want.layout,
                from: want.from,
                rows: (want.from..want.to)
                    .map(|n| {
                        vec![
                            Text::new(0.0, 0.0, 36.0, 20.0, n.to_string()).into(),
                            hit(&format!("line:{n}"), 40.0, 0.0, 560.0, 20.0),
                            Text::new(40.0, 0.0, 560.0, 20.0, format!("line {n}")).into(),
                        ]
                    })
                    .collect(),
            });
        }
    }

    #[test]
    fn a_wide_list_scrolls_sideways_and_its_fixed_part_stays() {
        let mut player = Player::new(env(300.0, 400.0));
        wide_list(&mut player);
        let place = |player: &Player, text: &str| {
            player
                .draw()
                .iter()
                .find_map(|draw| match draw.what {
                    Drawn::Text(t) if t.text == text => Some((t.x + draw.dx, draw.clip.left)),
                    _ => None,
                })
                .unwrap()
        };
        assert_eq!(place(&player, "0"), (10.0, 10.0));
        assert_eq!(place(&player, "line 0"), (50.0, 50.0));
        // A stroke mostly down scrolls down only.
        assert!(player.wheel(100.0, 150.0, 5.0, 40.0));
        assert_eq!(place(&player, "line 2"), (50.0, 50.0));
        // Across: the lines move, cut where the numbers end; they stay.
        assert!(player.wheel(100.0, 150.0, 120.0, 0.0));
        assert_eq!(place(&player, "2"), (10.0, 10.0));
        assert_eq!(place(&player, "line 2"), (-70.0, 50.0));
        // No further than the rows reach.
        assert!(player.wheel(100.0, 150.0, 5_000.0, 0.0));
        assert_eq!(place(&player, "line 2").0, 50.0 - 400.0);
        assert!(!player.wheel(100.0, 150.0, 10.0, 0.0), "at the end");
        let thumbs: Vec<Bounds> = player
            .draw()
            .iter()
            .filter_map(|draw| match draw.what {
                Drawn::Thumb(bounds) => Some(bounds),
                _ => None,
            })
            .collect();
        let across = thumbs
            .iter()
            .find(|thumb| thumb.width() > thumb.height())
            .expect("a thumb across");
        assert_eq!(across.bottom, 300.0 - THUMB_GAP);
        assert!((across.right - (210.0 - THUMB_WIDTH - THUMB_GAP * 2.0)).abs() < 0.01);

        // A click lands on the line where it shows now, not where it was.
        let clicked = player.click(60.0, 110.0, Button::Left, 1, Mods::default());
        let Some(Input::Click(click)) = clicked else {
            panic!("{clicked:?}")
        };
        assert_eq!(click.id, "line:2");
        assert_eq!(click.x, 60.0 - (40.0 + 10.0 - 400.0));
        // Over the numbers, the line under them does not answer.
        assert_eq!(
            player.click(20.0, 110.0, Button::Left, 1, Mods::default()),
            None
        );

        // Other rows start at the left again.
        let Item::List(other) = list(100, "b", None) else {
            unreachable!()
        };
        player.frame(Frame {
            items: vec![Item::List(other.wide(600.0, 40.0))],
            ..Frame::default()
        });
        assert!(player.wheel(100.0, 150.0, 1.0, 0.0), "back from the start");
    }

    #[test]
    fn a_hover_tint_has_the_corners_of_its_hit() {
        let mut player = Player::new(env(300.0, 400.0));
        let Item::Hit(round) = hit("a", 10.0, 10.0, 100.0, 30.0) else {
            unreachable!()
        };
        player.frame(Frame {
            items: vec![Item::Hit(round.radius(6.0))],
            ..Frame::default()
        });
        player.pointer_moved(20.0, 20.0);
        let painting = player.painting();
        assert!(
            painting
                .ops
                .iter()
                .any(|op| matches!(op, Op::Rect { radius, .. } if *radius == 6.0)),
            "{painting:?}"
        );
    }

    #[test]
    fn a_page_never_answered_is_asked_for_again_when_it_comes_back() {
        let mut player = Player::new(env(300.0, 400.0));
        player.frame(Frame {
            items: vec![list(10_000, "a", None)],
            ..Frame::default()
        });
        let first = player.wanted();
        assert_eq!(first[0].from, 0);
        // The answer never comes; the list is scrolled far away and back.
        player.wheel(50.0, 150.0, 0.0, 20.0 * 5_000.0);
        answer(&mut player);
        player.wheel(50.0, 150.0, 0.0, -20.0 * 5_000.0);
        let again = player.wanted();
        assert!(again.iter().any(|want| want.from == 0), "{again:?}");
    }

    #[test]
    fn a_list_of_every_row_a_u32_counts_adds_up() {
        let mut player = Player::new(env(300.0, 400.0));
        player.frame(Frame {
            items: vec![list(u32::MAX, "a", None)],
            ..Frame::default()
        });
        answer(&mut player);
        assert!(player.wheel(50.0, 150.0, 0.0, f32::MAX));
        let wanted = answer(&mut player);
        assert!(wanted.iter().all(|want| want.from < want.to), "{wanted:?}");
        assert!(wanted.iter().any(|want| want.to == u32::MAX), "{wanted:?}");
        assert!(player.wanted().is_empty(), "asked once");
    }

    #[test]
    fn rows_left_out_are_not_asked_for_again() {
        let mut player = Player::new(env(300.0, 400.0));
        player.frame(Frame {
            items: vec![list(50, "a", None)],
            ..Frame::default()
        });
        let wanted = player.wanted();
        player.rows(Rows {
            list: "l".into(),
            key: "a".into(),
            version: 0,
            layout: 0,
            from: wanted[0].from,
            rows: (0..5).map(row).collect(),
        });
        assert!(
            player.wanted().iter().all(|want| want.from >= 32),
            "no loop"
        );
    }

    #[test]
    fn hover_tints_at_once_and_a_click_names_the_hit() {
        let mut player = Player::new(env(300.0, 400.0));
        player.frame(Frame {
            items: vec![hit("top", 0.0, 0.0, 300.0, 40.0), list(100, "a", None)],
            ..Frame::default()
        });
        answer(&mut player);
        let before = player.revision();
        assert!(player.pointer_moved(20.0, 10.0));
        assert!(player.revision() > before, "a new hover is a new picture");
        let before = player.revision();
        assert!(!player.pointer_moved(25.0, 12.0), "the same hit");
        assert_eq!(player.revision(), before, "the same picture");
        assert_eq!(player.cursor(), Some(Cursor::Pointer));
        let hovered: Vec<_> = player
            .draw()
            .into_iter()
            .filter(|draw| matches!(draw.what, Drawn::Hover(_)))
            .collect();
        assert_eq!(hovered.len(), 1);

        // Row 2 of the list sits at 140..160 before any scroll.
        let input = player.click(30.0, 145.0, Button::Left, 1, Mods::default());
        let Some(Input::Click(click)) = input else {
            panic!("{input:?}")
        };
        assert_eq!(click.id, "row:2");
        assert_eq!((click.x, click.y), (20.0, 5.0), "within the hit");
        player.wheel(30.0, 145.0, 0.0, 20.0);
        let Some(Input::Click(click)) = player.click(30.0, 145.0, Button::Left, 1, Mods::default())
        else {
            panic!()
        };
        assert_eq!(click.id, "row:3", "the rows moved under the pointer");
        assert!(player
            .click(299.0, 399.0, Button::Left, 1, Mods::default())
            .is_none());
        assert_eq!(player.cursor(), None, "over nothing that answers");
        player.pointer_moved(20.0, 10.0);
        assert!(player.pointer_left());
        assert_eq!(player.cursor(), None);
    }

    #[test]
    fn a_scroll_area_scrolls_is_cut_to_itself_and_shows_its_thumb() {
        let mut player = Player::new(env(300.0, 400.0));
        let scroll = |top| Scroll {
            id: "s".into(),
            x: 0.0,
            y: 50.0,
            w: 300.0,
            h: 100.0,
            height: 400.0,
            items: vec![hit("inner", 0.0, 120.0, 300.0, 20.0)],
            top,
        };
        player.frame(Frame {
            items: vec![Item::Scroll(scroll(None))],
            ..Frame::default()
        });
        assert!(
            player
                .click(10.0, 180.0, Button::Left, 1, Mods::default())
                .is_none(),
            "below"
        );
        assert!(player.wheel(10.0, 60.0, 0.0, 100.0));
        let Some(Input::Click(click)) = player.click(10.0, 75.0, Button::Left, 1, Mods::default())
        else {
            panic!()
        };
        assert_eq!((click.id.as_str(), click.y), ("inner", 5.0));
        let draws = player.draw();
        let thumb = draws
            .iter()
            .find_map(|draw| match draw.what {
                Drawn::Thumb(thumb) => Some(thumb),
                _ => None,
            })
            .expect("a thumb");
        assert_eq!(thumb.height(), 25.0, "a quarter of it shows");
        assert!(draws
            .iter()
            .filter(|draw| !matches!(draw.what, Drawn::Thumb(_)))
            .all(|draw| draw.clip == Bounds::new(0.0, 50.0, 300.0, 100.0)));
        player.frame(Frame {
            items: vec![Item::Scroll(scroll(Some(Jump { to: 0.0, seq: 7 })))],
            ..Frame::default()
        });
        assert!(
            player
                .click(10.0, 75.0, Button::Left, 1, Mods::default())
                .is_none(),
            "jumped back"
        );
    }

    #[test]
    fn a_painting_is_the_draw_as_plain_data_moved_into_place() {
        let mut player = Player::new(env(300.0, 400.0));
        player.frame(Frame {
            items: vec![hit("top", 0.0, 0.0, 300.0, 40.0), list(100, "a", None)],
            ..Frame::default()
        });
        answer(&mut player);
        player.wheel(50.0, 150.0, 0.0, 5.0);
        player.pointer_moved(20.0, 10.0);
        let painting = player.painting();
        assert_eq!(painting.cursor, Some(Cursor::Pointer));
        assert_eq!(painting.clips[0], [0.0, 0.0, 300.0, 400.0]);
        assert!(
            matches!(
                painting.ops[0],
                Op::Rect {
                    clip: 0,
                    w: 300.0,
                    ..
                }
            ),
            "the hover"
        );
        let first_row = painting
            .ops
            .iter()
            .find_map(|op| match op {
                Op::Text { text, y, clip, .. } => Some((text.clone(), *y, *clip)),
                _ => None,
            })
            .unwrap();
        assert_eq!(
            first_row,
            ("row 0".to_string(), 95.0, 1),
            "the list's own region, scrolled"
        );
        let json = serde_json::to_value(&painting.ops[0]).unwrap();
        assert_eq!(json["op"], "rect");
        assert_eq!(json["fill"], "bg-hover");
    }

    #[test]
    fn what_cannot_be_drawn_is_left_out() {
        let mut player = Player::new(env(300.0, 400.0));
        let mut items = vec![hit("nan", f32::NAN, 0.0, 1.0, 1.0)];
        items.push(Item::Scroll(Scroll {
            id: "s".into(),
            x: 0.0,
            y: 0.0,
            w: 10.0,
            h: 10.0,
            height: 10.0,
            items: vec![list(3, "a", None), hit("kept", 0.0, 0.0, 1.0, 1.0)],
            top: None,
        }));
        items.extend((0..ITEM_LIMIT + 10).map(|n| hit(&n.to_string(), 0.0, 0.0, 1.0, 1.0)));
        items.push(Item::Text(Text {
            x: 0.0,
            y: 0.0,
            w: 1.0,
            h: 1.0,
            text: "x".repeat(TEXT_LIMIT * 2),
            color: Color::default(),
            font: Default::default(),
            size: Default::default(),
            bold: false,
            align: Default::default(),
        }));
        player.frame(Frame {
            items,
            ..Frame::default()
        });
        let Item::Scroll(scroll) = &player.frame.items[0] else {
            panic!("the NaN hit went")
        };
        assert_eq!(scroll.items.len(), 1, "no list in a scroll area");
        assert_eq!(
            player.frame.items.len(),
            ITEM_LIMIT - 1,
            "the scroll area's item counts"
        );
        assert!(!player
            .frame
            .items
            .iter()
            .any(|item| matches!(item, Item::Text(_))));

        let long = Item::Text(Text {
            x: 0.0,
            y: 0.0,
            w: 1.0,
            h: 1.0,
            text: "\u{4e2d}".repeat(TEXT_LIMIT + 1),
            color: Color::default(),
            font: Default::default(),
            size: Default::default(),
            bold: false,
            align: Default::default(),
        });
        player.frame(Frame {
            items: vec![long],
            ..Frame::default()
        });
        let Item::Text(text) = &player.frame.items[0] else {
            panic!()
        };
        assert_eq!(text.text.chars().count(), TEXT_LIMIT);
    }

    #[test]
    fn a_page_that_comes_is_kept_while_it_is_near() {
        // A screen of one row: pages are PAGE_MIN rows, far more than the
        // screen, and each one asked for must stay once it has come.
        let mut player = Player::new(env(300.0, 400.0));
        let mut short = list(1_000, "a", None);
        if let Item::List(list) = &mut short {
            list.h = 20.0;
        }
        player.frame(Frame {
            items: vec![short],
            ..Frame::default()
        });
        for step in 0..200 {
            let wanted = answer(&mut player);
            assert!(wanted.len() <= 3, "{wanted:?}");
            player.wheel(50.0, 105.0, 0.0, if step % 7 == 6 { -60.0 } else { 30.0 });
        }
        answer(&mut player);
        assert!(player.wanted().is_empty(), "every page asked for was kept");
    }

    #[test]
    fn each_frame_says_whether_the_extended_view_is_wanted() {
        let mut player = Player::new(env(300.0, 400.0));
        assert!(!player.extend(), "nothing drawn asks nothing");
        player.frame(Frame {
            items: Vec::new(),
            extend: true,
        });
        assert!(player.extend());
        player.frame(Frame::default());
        assert!(!player.extend(), "a frame that does not ask lets it go");
    }
}
