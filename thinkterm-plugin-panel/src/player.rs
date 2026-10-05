//! Showing a panel: the frame its plugin sent last, the rows of its lists
//! near the view, how far its scroll areas are scrolled, and what the
//! pointer is over. Nothing here waits for the plugin. A paint draws what
//! has arrived; a scroll moves at once and asks for the rows it will need
//! next; a hover tints at once.
//!
//! A client paints what [`Player::draw`] gives it with its own renderer,
//! and hands the pointer and the wheel in. What is to go to the plugin
//! comes back from [`Player::click`] and [`Player::wanted`], and from
//! [`Player::told`] for the keyboard.
//!
//! The keyboard is the terminal's until the user gives it to a panel: by
//! pressing in one of its fields, or with a key of their own that asks for
//! the panel ([`Player::focus_panel`]). A press anywhere else in the panel
//! leaves it where it is. While the panel has it, the keys the panel takes
//! go to its plugin and the rest go nowhere -- never to the terminal --
//! but for the app's own, with Ctrl, Alt or Command. Escape gives it back;
//! so does the panel going, and the client says so ([`Player::blur`]) when
//! the user puts it elsewhere. The plugin can move it between its own
//! fields, and give it back, but never take it.

use crate::scene::{
    Align, Area, Bounds, Color, Cursor, Field, FieldKind, Font, Frame, Hit, Item, Line, List, Rect,
    Scroll, Size, Text,
};
use crate::wire::{Button, Click, Env, Focus, Input, Key, Mods, Rows, RowsWanted, Typed};
use serde::Serialize;
use std::collections::{HashMap, HashSet};

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
/// The most characters a field holds.
pub const FIELD_LIMIT: usize = 16_384;
/// How far a field's text is kept from its box's sides, and -- in one of
/// lines -- from its top and bottom.
pub const FIELD_INSET: f32 = 10.0;
pub const FIELD_INSET_LINES: f32 = 5.0;
/// How wide a field's icon is, and the room between it and the text.
pub const FIELD_ICON: f32 = 14.0;
pub const FIELD_ICON_GAP: f32 = 6.0;
/// A field of lines' corners; one on one line is rounded at its ends.
pub const FIELD_RADIUS: f32 = 6.0;
/// The most keys a frame says its panel takes, and the longest name.
const KEY_LIMIT: usize = 64;
const KEY_NAME_LIMIT: usize = 32;
/// The most pieces of a field's box a painting says are covered: past
/// them, the client's own field takes the rest.
const COVERED_LIMIT: usize = 32;
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
    /// A field, which the client draws itself, in two parts.
    Field(&'a Field, FieldPart),
}

/// What of a field a draw is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldPart {
    /// Its box, with its icon: outlined in the accent while it has the
    /// keyboard, and as the sidebar's search is while the pointer is over
    /// it.
    Box { focused: bool, hovered: bool },
    /// What it holds -- its placeholder while that is empty -- the caret
    /// and what is selected, cut to inside the box ([`FIELD_INSET`]).
    Text,
}

/// Where the keyboard is, as far as one view knows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Keyboard {
    /// Not with this view: with the terminal, or another part of ThinkTerm.
    Away,
    /// With this view, in none of its fields: the keys it takes go to it.
    Panel,
    /// In its field with this id.
    Field(String),
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
    /// The panel has the keyboard, in none of its fields.
    pub focused: bool,
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
    /// A field's box, for the client to put a field of its own over:
    /// `text` is what it holds as the player has it, to be taken whenever
    /// `revision` moves -- the plugin put text there -- and `limit` the
    /// most characters it takes. `inside` is where its text goes, as x, y,
    /// w, h -- after `icon`, if it shows one. `covered` is where what is
    /// drawn after it and answers a press -- a hit, a scroll area, a list --
    /// lies over it, as x, y, w, h: the client's own field leaves those to
    /// the painting, which shows there and is pressed there.
    Field {
        clip: usize,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        id: String,
        kind: FieldKind,
        placeholder: String,
        font: Font,
        limit: usize,
        focused: bool,
        text: String,
        revision: u32,
        inside: [f32; 4],
        #[serde(skip_serializing_if = "Option::is_none")]
        icon: Option<FieldIcon>,
        /// Where its button that empties it goes, which the client shows
        /// while it holds something.
        #[serde(skip_serializing_if = "Option::is_none")]
        clear: Option<FieldIcon>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        covered: Vec<[f32; 4]>,
    },
}

/// The icon a field shows, by name, and its square: `size` across, from
/// `x`, `y`, in the panel's units.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FieldIcon {
    pub name: String,
    pub x: f32,
    pub y: f32,
    pub size: f32,
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

/// What a field holds, as the client edits it, and what it was given.
struct FieldState {
    /// The `seq` the plugin's text was last taken for; none once the
    /// plugin started again, whose next frame's text is taken whatever it
    /// says.
    seq: Option<u32>,
    text: String,
    /// Changes when the text was put there rather than typed: the plugin's
    /// taken, or what was typed cut to fit. A client that edits a copy of
    /// its own takes it again then.
    revision: u32,
    /// The last `focus` seen.
    focus: Option<u32>,
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
    /// The fields of the frame, by id.
    fields: HashMap<String, FieldState>,
    /// Counts what a field's revision is set to, so that one gone and come
    /// back under its id is never taken for what it was.
    field_revisions: u32,
    keyboard: Keyboard,
    /// The last `release` the frames said.
    release: Option<u32>,
    /// The plugin's program has not drawn since it started: the `release`
    /// of its first frame asks nothing.
    fresh: bool,
    /// The keyboard was given before the panel was drawn: the first field
    /// of its first frame gets it.
    first_field: bool,
    /// What the player decided to tell the plugin, in order.
    told: Vec<Input>,
    pointer: Option<(f32, f32)>,
    hover: Option<Spot>,
    /// The field the pointer is over, where nothing over it answers a press.
    hover_field: Option<String>,
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
            fields: HashMap::new(),
            field_revisions: 0,
            keyboard: Keyboard::Away,
            release: None,
            fresh: true,
            first_field: false,
            told: Vec::new(),
            pointer: None,
            hover: None,
            hover_field: None,
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
        let mut ids = HashSet::new();
        frame.items.retain(|item| match item {
            Item::Field(field) => ids.insert(field.id.clone()),
            _ => true,
        });
        frame
            .keys
            .retain(|key| !key.is_empty() && key.chars().count() <= KEY_NAME_LIMIT);
        frame.keys.truncate(KEY_LIMIT);
        let mut fields = HashMap::new();
        for item in &frame.items {
            let Item::Field(field) = item else {
                continue;
            };
            let state = match self.fields.remove(&field.id) {
                Some(mut state) => {
                    if state.seq != Some(field.seq) {
                        state.seq = Some(field.seq);
                        state.text = fit_field(field, &field.value);
                        self.field_revisions += 1;
                        state.revision = self.field_revisions;
                    } else {
                        // It may hold less than it did: on one line now, or
                        // fewer characters.
                        let fitted = fit_field(field, &state.text);
                        if fitted != state.text {
                            state.text = fitted;
                            self.field_revisions += 1;
                            state.revision = self.field_revisions;
                        }
                    }
                    state
                }
                None => {
                    self.field_revisions += 1;
                    FieldState {
                        seq: Some(field.seq),
                        text: fit_field(field, &field.value),
                        revision: self.field_revisions,
                        focus: None,
                    }
                }
            };
            fields.insert(field.id.clone(), state);
        }
        self.fields = fields;
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
                        state.offset = match jump.through {
                            Some(through) => reveal(state.offset, jump.to, through, scroll.h),
                            None => jump.to,
                        };
                        state.seq = Some(jump.seq);
                    }
                    scrolls.insert(scroll.id.clone(), state);
                }
                _ => {}
            }
        }
        self.lists = lists;
        self.scrolls = scrolls;
        let first = std::mem::take(&mut self.fresh);
        self.frame = frame;
        self.shown = true;
        self.follow_keyboard(first);
        self.settle();
    }

    /// Moves the keyboard as the new frame says: out of a field that went,
    /// back to the terminal for a new `release`, and to a field for a new
    /// `focus` -- or to the first field, given to the panel before it was
    /// drawn -- but only while the panel has it. The `release` of the
    /// program's first frame is only taken note of: one at the opening, or
    /// after a restart, asks nothing.
    fn follow_keyboard(&mut self, first: bool) {
        // Asked of a panel that had the keyboard as the frame came: the
        // field it was in may be the one the frame put another in place of.
        let had = self.keyboard != Keyboard::Away;
        let gone = matches!(&self.keyboard, Keyboard::Field(id) if !self.fields.contains_key(id));
        let mut released = false;
        if let Some(seq) = self.frame.release {
            if self.release != Some(seq) {
                self.release = Some(seq);
                released = !first;
            }
        }
        // The last one asked for wins.
        let mut asked = None;
        for item in &self.frame.items {
            let Item::Field(field) = item else {
                continue;
            };
            let Some(seq) = field.focus else {
                continue;
            };
            if let Some(state) = self.fields.get_mut(&field.id) {
                if state.focus != Some(seq) {
                    state.focus = Some(seq);
                    asked = Some(field.id.clone());
                }
            }
        }
        let first_field = std::mem::take(&mut self.first_field);
        if !had {
            return;
        }
        match asked {
            _ if released => self.set_keyboard(Keyboard::Away),
            Some(id) => self.set_keyboard(Keyboard::Field(id)),
            None if gone => self.set_keyboard(Keyboard::Away),
            None if first_field && self.keyboard == Keyboard::Panel => {
                if let Some(id) = self.first_field_id() {
                    self.set_keyboard(Keyboard::Field(id));
                }
            }
            None => {}
        }
    }

    /// Puts the keyboard at `keyboard`, telling the plugin where it went.
    fn set_keyboard(&mut self, keyboard: Keyboard) {
        if self.keyboard == keyboard {
            return;
        }
        self.told.push(match &keyboard {
            Keyboard::Away => Input::Blur,
            Keyboard::Panel => Input::Focus(Focus { id: None }),
            Keyboard::Field(id) => Input::Focus(Focus {
                id: Some(id.clone()),
            }),
        });
        self.keyboard = keyboard;
        self.revision += 1;
    }

    fn first_field_id(&self) -> Option<String> {
        self.fields().next().map(|field| field.id.clone())
    }

    /// Where the keyboard is, as this view knows it.
    pub fn keyboard(&self) -> &Keyboard {
        &self.keyboard
    }

    pub fn has_keyboard(&self) -> bool {
        self.keyboard != Keyboard::Away
    }

    /// The field the keyboard is in, if it is in one.
    pub fn focused_field(&self) -> Option<&str> {
        match &self.keyboard {
            Keyboard::Field(id) => Some(id),
            Keyboard::Away | Keyboard::Panel => None,
        }
    }

    /// The user asked for the panel -- with a key of their own: it has the
    /// keyboard, in its first field if it has one. One not drawn yet puts
    /// it there once its first frame comes, and swallows the keys pressed
    /// meanwhile.
    pub fn focus_panel(&mut self) {
        match self.first_field_id() {
            Some(id) => self.set_keyboard(Keyboard::Field(id)),
            None => {
                self.first_field = !self.shown;
                self.set_keyboard(Keyboard::Panel);
            }
        }
    }

    /// The user pressed in field `id`: it has the keyboard. False when the
    /// frame has no such field.
    pub fn focus_field(&mut self, id: &str) -> bool {
        if !self.fields.contains_key(id) {
            return false;
        }
        self.set_keyboard(Keyboard::Field(id.to_string()));
        true
    }

    /// The keyboard went elsewhere: the user put it there, or the panel is
    /// going.
    pub fn blur(&mut self) {
        self.set_keyboard(Keyboard::Away);
    }

    /// A key pressed while the panel has the keyboard, which the field with
    /// it -- if one has it -- did not use, named as a browser names it:
    /// Escape gives the keyboard back, Tab -- Shift-Tab back -- moves it to
    /// the next field, and a key the frame says the panel takes is sent.
    /// True when the panel took it. False for any other -- one with Ctrl,
    /// Alt or Command, one the panel does not take, any while the keyboard
    /// is elsewhere -- which the client lets the app have, and never the
    /// terminal.
    pub fn key(&mut self, key: &str, mods: Mods) -> bool {
        if self.keyboard == Keyboard::Away || mods.ctrl || mods.alt || mods.cmd {
            return false;
        }
        match key {
            "Escape" => self.set_keyboard(Keyboard::Away),
            "Tab" => self.step_field(mods.shift),
            _ if self.frame.keys.iter().any(|taken| taken == key) => {
                let mods = Mods {
                    shift: mods.shift,
                    ..Mods::default()
                };
                self.told.push(Input::Key(Key {
                    key: key.to_string(),
                    mods,
                }));
            }
            _ => return false,
        }
        true
    }

    /// Moves the keyboard to the next field in the frame's order, or the
    /// one before, going round; from none, to the first or the last.
    fn step_field(&mut self, back: bool) {
        let ids: Vec<&str> = self.fields().map(|field| field.id.as_str()).collect();
        if ids.is_empty() {
            return;
        }
        let count = ids.len();
        let at = self
            .focused_field()
            .and_then(|focused| ids.iter().position(|id| *id == focused));
        let next = match (at, back) {
            (Some(at), false) => (at + 1) % count,
            (Some(at), true) => (at + count - 1) % count,
            (None, false) => 0,
            (None, true) => count - 1,
        };
        let id = ids[next].to_string();
        self.set_keyboard(Keyboard::Field(id));
    }

    /// The fields of the last frame, in its order.
    pub fn fields(&self) -> impl Iterator<Item = &Field> {
        self.frame.items.iter().filter_map(|item| match item {
            Item::Field(field) => Some(field),
            _ => None,
        })
    }

    /// The field `id` of the last frame.
    pub fn field(&self, id: &str) -> Option<&Field> {
        self.fields().find(|field| field.id == id)
    }

    /// The field at `x`, `y`, unless what else answers a press -- a hit, a
    /// scroll area, a list -- is drawn over it there: a button laid on a
    /// field's end is the button's.
    pub fn field_at(&self, x: f32, y: f32) -> Option<&Field> {
        if !self.panel().contains(x, y) {
            return None;
        }
        let top = self.frame.items.iter().rev().find(|item| {
            matches!(
                item,
                Item::Field(_) | Item::Hit(_) | Item::Scroll(_) | Item::List(_)
            ) && item.bounds().contains(x, y)
        });
        match top {
            Some(Item::Field(field)) => Some(field),
            _ => None,
        }
    }

    /// What field `id` holds, as the player has it, and its revision: a
    /// client that edits a copy of its own takes the text again whenever
    /// that moves.
    pub fn field_text(&self, id: &str) -> Option<(&str, u32)> {
        let state = self.fields.get(id)?;
        Some((&state.text, state.revision))
    }

    /// What the field with the keyboard, `id`, holds now the user edited
    /// it: kept to what it takes -- the text taken again, cut to fit, when
    /// that is less -- and sent to the plugin, unless the field is secret.
    /// An edit of any other is not the user's, and is let be.
    pub fn edit_field(&mut self, id: &str, text: &str) {
        if self.focused_field() != Some(id) {
            return;
        }
        let Some(field) = self.field(id) else {
            return;
        };
        let secret = field.kind == FieldKind::Secret;
        let fitted = fit_field(field, text);
        let drawn = field.seq;
        let Some(state) = self.fields.get_mut(id) else {
            return;
        };
        if fitted != text {
            // Cut to fit: the client takes it again, even when that is
            // what the field held already.
            self.field_revisions += 1;
            state.revision = self.field_revisions;
            self.revision += 1;
        }
        if fitted == state.text {
            return;
        }
        state.text = fitted;
        self.revision += 1;
        if !secret {
            self.told.push(Input::Text(Typed {
                id: id.to_string(),
                text: state.text.clone(),
                seq: state.seq.unwrap_or(drawn),
            }));
        }
    }

    /// The user submitted the field with the keyboard, `id`: what it holds
    /// goes to the plugin.
    pub fn submit(&mut self, id: &str) {
        if self.focused_field() != Some(id) {
            return;
        }
        let drawn = self.field(id).map_or(0, |field| field.seq);
        if let Some(state) = self.fields.get(id) {
            self.told.push(Input::Submit(Typed {
                id: id.to_string(),
                text: state.text.clone(),
                seq: state.seq.unwrap_or(drawn),
            }));
        }
    }

    /// What the player decided to tell the plugin -- where the keyboard
    /// went, what a field holds, a key -- in order: taken after every
    /// change a client makes, and after every frame.
    pub fn told(&mut self) -> Vec<Input> {
        std::mem::take(&mut self.told)
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
    /// show, which stay until the new ones come. The keyboard goes back to
    /// the terminal; the new program never had it, and is not told. Its
    /// fields take what its first frame puts in them -- it never heard what
    /// was typed for the last one -- and its `focus` and `release` count
    /// afresh.
    pub fn restarted(&mut self) {
        self.layout = self.layout.wrapping_add(1);
        for state in self.lists.values_mut() {
            state.asking.clear();
        }
        for state in self.fields.values_mut() {
            state.seq = None;
            state.focus = None;
        }
        self.release = None;
        self.fresh = true;
        self.wanted.clear();
        self.keyboard = Keyboard::Away;
        self.first_field = false;
        self.told.clear();
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

    /// The cursor over what the pointer is on -- the I-beam over a field;
    /// `None` where nothing answers a click.
    pub fn cursor(&self) -> Option<Cursor> {
        if let Some((x, y)) = self.pointer {
            if self.field_at(x, y).is_some() {
                return Some(Cursor::Text);
            }
        }
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
    /// hit. One on a field is the field's, not a click
    /// ([`focus_field`](Self::focus_field)).
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
        if self.field_at(x, y).is_some() {
            return None;
        }
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
                Item::Field(field) => {
                    if !item.bounds().touches(&panel) {
                        continue;
                    }
                    let focused = self.focused_field() == Some(field.id.as_str());
                    let hovered = self.hover_field.as_deref() == Some(field.id.as_str());
                    out.push(Draw {
                        what: Drawn::Field(field, FieldPart::Box { focused, hovered }),
                        dx: 0.0,
                        dy: 0.0,
                        clip: panel,
                    });
                    let inside = field_inside(field).intersect(&panel);
                    if !inside.is_empty() {
                        out.push(Draw {
                            what: Drawn::Field(field, FieldPart::Text),
                            dx: 0.0,
                            dy: 0.0,
                            clip: inside,
                        });
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
            // A client that puts a field of its own over the box draws
            // what it holds itself.
            if let Drawn::Field(_, FieldPart::Text) = draw.what {
                continue;
            }
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
                Drawn::Field(field, part) => {
                    let (text, revision) = self.field_text(&field.id).unwrap_or_default();
                    Op::Field {
                        clip,
                        x: field.x + dx,
                        y: field.y + dy,
                        w: field.w,
                        h: field.h,
                        id: field.id.clone(),
                        kind: field.kind,
                        placeholder: field.placeholder.clone(),
                        font: field.font,
                        limit: field_limit(field),
                        focused: matches!(part, FieldPart::Box { focused: true, .. }),
                        text: text.to_string(),
                        revision,
                        inside: {
                            let inside = field_inside(field);
                            [
                                inside.left + dx,
                                inside.top + dy,
                                inside.width(),
                                inside.height(),
                            ]
                        },
                        icon: field_icon(field).map(|name| FieldIcon {
                            name: name.to_string(),
                            x: field.x + FIELD_INSET + dx,
                            y: field.y + (field.h - FIELD_ICON) / 2.0 + dy,
                            size: FIELD_ICON,
                        }),
                        clear: field_clear_at(field).map(|(x, y, size)| FieldIcon {
                            name: "x".to_string(),
                            x: x + dx,
                            y: y + dy,
                            size,
                        }),
                        covered: self.covering(field),
                    }
                }
            });
        }
        Painting {
            clips,
            ops,
            cursor: self.cursor(),
            focused: self.keyboard == Keyboard::Panel,
        }
    }

    /// Where what is drawn after field `field` and answers a press lies
    /// over it, in the panel -- there a press is not the field's
    /// ([`field_at`](Self::field_at)) -- in pieces that do not overlap.
    fn covering(&self, field: &Field) -> Vec<[f32; 4]> {
        let items = &self.frame.items;
        let Some(at) = items
            .iter()
            .position(|item| matches!(item, Item::Field(own) if own.id == field.id))
        else {
            return Vec::new();
        };
        let own = items[at].bounds().intersect(&self.panel());
        let mut covered: Vec<Bounds> = Vec::new();
        let over = items[at + 1..]
            .iter()
            .filter(|item| matches!(item, Item::Hit(_) | Item::Scroll(_) | Item::List(_)));
        for item in over {
            let mut pieces = vec![item.bounds().intersect(&own)];
            for done in &covered {
                pieces = pieces
                    .into_iter()
                    .flat_map(|piece| without(piece, done))
                    .collect();
            }
            covered.extend(pieces.into_iter().filter(|piece| !piece.is_empty()));
            if covered.len() >= COVERED_LIMIT {
                covered.truncate(COVERED_LIMIT);
                break;
            }
        }
        covered
            .into_iter()
            .map(|piece| [piece.left, piece.top, piece.width(), piece.height()])
            .collect()
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
            // A field is drawn where it is in the frame, by `draw`.
            Item::Hit(_) | Item::Scroll(_) | Item::List(_) | Item::Field(_) => return,
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
        let hover_field = self
            .pointer
            .and_then(|(x, y)| self.field_at(x, y))
            .map(|field| field.id.clone());
        let changed = hover != self.hover || hover_field != self.hover_field;
        if changed {
            self.revision += 1;
        }
        self.hover = hover;
        self.hover_field = hover_field;
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
            self.offset = match jump.through {
                Some(through) => {
                    reveal(self.offset, jump.to * list.row, through * list.row, list.h)
                }
                None => jump.to * list.row,
            };
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
            Item::Scroll(_) | Item::List(_) | Item::Field(_) if nested => return false,
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
            Item::Field(field) => {
                if let Some((cut, _)) = field.value.char_indices().nth(FIELD_LIMIT) {
                    field.value.truncate(cut);
                }
                if let Some((cut, _)) = field.placeholder.char_indices().nth(TEXT_LIMIT) {
                    field.placeholder.truncate(cut);
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
        Item::Field(field) => finite(&[field.x, field.y, field.w, field.h]),
    }
}

/// What of `piece` lies outside `cut`: up to four pieces, none of them
/// overlapping.
fn without(piece: Bounds, cut: &Bounds) -> Vec<Bounds> {
    let inside = piece.intersect(cut);
    if inside.is_empty() {
        return vec![piece];
    }
    let band = |top: f32, bottom: f32, left: f32, right: f32| Bounds {
        left,
        top,
        right,
        bottom,
    };
    [
        band(piece.top, inside.top, piece.left, piece.right),
        band(inside.bottom, piece.bottom, piece.left, piece.right),
        band(inside.top, inside.bottom, piece.left, inside.left),
        band(inside.top, inside.bottom, inside.right, piece.right),
    ]
    .into_iter()
    .filter(|part| !part.is_empty())
    .collect()
}

/// The most characters `field` holds.
pub fn field_limit(field: &Field) -> usize {
    match field.max {
        0 => FIELD_LIMIT,
        max => (max as usize).min(FIELD_LIMIT),
    }
}

/// What `field` holds of `text`: no more characters than it takes, and no
/// control characters but -- in one of lines -- line ends and tabs.
fn fit_field(field: &Field, text: &str) -> String {
    let lines = field.kind == FieldKind::Lines;
    text.chars()
        .filter(|ch| !ch.is_control() || (lines && (*ch == '\n' || *ch == '\t')))
        .take(field_limit(field))
        .collect()
}

/// Where in `field`'s box its text goes: after its icon, if it shows one.
pub fn field_inside(field: &Field) -> Bounds {
    let down = match field.kind {
        FieldKind::Lines => FIELD_INSET_LINES,
        FieldKind::Line | FieldKind::Secret => 0.0,
    };
    let lead = match field_icon(field) {
        Some(_) => FIELD_ICON + FIELD_ICON_GAP,
        None => 0.0,
    };
    let trail = match field_clear(field) {
        true => FIELD_ICON + FIELD_ICON_GAP,
        false => 0.0,
    };
    Bounds::new(
        field.x + FIELD_INSET + lead,
        field.y + down,
        field.w - FIELD_INSET * 2.0 - lead - trail,
        field.h - down * 2.0,
    )
}

/// Whether `field` has a button at its end that empties it: one on one
/// line that asks for it.
pub fn field_clear(field: &Field) -> bool {
    field.clear && field.kind != FieldKind::Lines
}

/// Where `field`'s button that empties it is, and its icon's: `size`
/// across from `x`, `y`, in its units.
pub fn field_clear_at(field: &Field) -> Option<(f32, f32, f32)> {
    field_clear(field).then(|| {
        (
            field.x + field.w - FIELD_INSET - FIELD_ICON,
            field.y + (field.h - FIELD_ICON) / 2.0,
            FIELD_ICON,
        )
    })
}

/// The offset of a region `room` tall that shows `from` to `to`, moved
/// from `offset` as little as it takes.
fn reveal(offset: f32, from: f32, to: f32, room: f32) -> f32 {
    if from < offset {
        from
    } else if to > offset + room {
        // Its start, at least, when it is taller than the room.
        (to - room).min(from)
    } else {
        offset
    }
}

/// The icon `field` shows: the one it names, if every client has it, on
/// one line.
pub fn field_icon(field: &Field) -> Option<&str> {
    let icon = field.icon.as_deref()?;
    (field.kind != FieldKind::Lines && crate::FIELD_ICONS.contains(&icon)).then_some(icon)
}

/// How round `field`'s corners are: wholly at the ends of one on one line,
/// as the sidebar's search is.
pub fn field_radius(field: &Field) -> f32 {
    match field.kind {
        FieldKind::Lines => FIELD_RADIUS.min(field.h / 2.0),
        FieldKind::Line | FieldKind::Secret => field.h / 2.0,
    }
    .max(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scene::{Color, Jump, Token};
    use crate::wire::{MonoMetrics, TextMetrics};
    use serde_json::json;

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
            features: Vec::new(),
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

        let jump = Some(Jump {
            to: 500.0,
            seq: 1,
            through: None,
        });
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
            items: vec![Item::Scroll(scroll(Some(Jump {
                to: 0.0,
                seq: 7,
                through: None,
            })))],
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
            ..Frame::default()
        });
        assert!(player.extend());
        player.frame(Frame::default());
        assert!(!player.extend(), "a frame that does not ask lets it go");
    }

    fn fields(fields: Vec<Field>) -> Frame {
        Frame {
            items: fields.into_iter().map(Item::Field).collect(),
            ..Frame::default()
        }
    }

    fn field(id: &str, y: f32) -> Field {
        Field::new(id, 10.0, y, 200.0, 26.0)
    }

    fn focus(id: &str) -> Input {
        Input::Focus(Focus {
            id: Some(id.into()),
        })
    }

    #[test]
    fn only_the_user_gives_a_panel_the_keyboard_and_a_press_elsewhere_keeps_it_where_it_is() {
        let mut player = Player::new(env(300.0, 400.0));
        let mut frame = fields(vec![field("add", 10.0).focus(1)]);
        frame.items.push(hit("row", 0.0, 100.0, 300.0, 20.0));
        player.frame(frame);
        assert_eq!(player.keyboard(), &Keyboard::Away, "asked for, not given");
        assert!(player.told().is_empty());
        assert!(!player.key("ArrowDown", Mods::default()), "the terminal's");

        // A press on a hit is a click, and does not move the keyboard.
        assert!(player
            .click(5.0, 105.0, Button::Left, 1, Mods::default())
            .is_some());
        assert_eq!(player.keyboard(), &Keyboard::Away);
        // One on the field is not a click: it is the field's.
        assert_eq!(
            player.field_at(50.0, 20.0).map(|f| f.id.as_str()),
            Some("add")
        );
        assert_eq!(
            player.click(50.0, 20.0, Button::Left, 1, Mods::default()),
            None
        );
        assert!(player.focus_field("add"));
        assert_eq!(player.told(), [focus("add")]);
        player.click(5.0, 105.0, Button::Left, 1, Mods::default());
        assert_eq!(player.focused_field(), Some("add"), "still typing there");
        assert!(!player.focus_field("gone"));

        // With the keyboard in the panel, its plugin moves it between its
        // own fields, once for each new seq.
        let two = || fields(vec![field("add", 10.0).focus(1), field("b", 50.0).focus(1)]);
        player.frame(two());
        assert_eq!(player.told(), [focus("b")]);
        player.frame(two());
        assert!(player.told().is_empty(), "the same seq is not taken again");
        player.blur();
        assert_eq!(player.told(), [Input::Blur]);
        player.frame(fields(vec![field("add", 10.0).focus(2), field("b", 50.0)]));
        assert_eq!(
            player.keyboard(),
            &Keyboard::Away,
            "never from the terminal"
        );
        assert!(player.told().is_empty());
    }

    #[test]
    fn what_is_typed_is_sent_kept_to_what_the_field_takes_and_a_new_seq_replaces_it() {
        let mut player = Player::new(env(300.0, 400.0));
        let frame = |seq: u32, value: &str| {
            fields(vec![
                field("add", 10.0).max(3).value(value, seq),
                field("pass", 50.0).secret(),
                field("note", 90.0).lines(),
            ])
        };
        player.frame(frame(0, "x"));
        let (text, first) = player.field_text("add").unwrap();
        assert_eq!(text, "x");
        player.edit_field("add", "nope");
        assert_eq!(player.field_text("add").unwrap().0, "x", "not the user's");

        player.focus_field("add");
        player.told();
        player.edit_field("add", "TSMC");
        let (text, cut) = player.field_text("add").unwrap();
        assert_eq!(text, "TSM");
        assert_ne!(cut, first, "cut to fit: to be taken back");
        assert_eq!(
            player.told(),
            [Input::Text(Typed {
                id: "add".into(),
                text: "TSM".into(),
                seq: 0,
            })]
        );
        // Cut to what it holds already: nothing to tell, but the client
        // takes it back all the same.
        let shown = player.revision();
        player.edit_field("add", "TSMD");
        assert!(player.told().is_empty());
        assert_ne!(player.revision(), shown);
        let cut = player.field_text("add").unwrap().1;
        player.frame(frame(0, ""));
        assert_eq!(
            player.field_text("add").unwrap(),
            ("TSM", cut),
            "typed, kept"
        );
        player.frame(frame(1, ""));
        let (text, taken) = player.field_text("add").unwrap();
        assert_eq!(text, "", "a new seq puts the plugin's there");
        assert_ne!(taken, cut);
        player.edit_field("add", "a\nb");
        assert_eq!(player.field_text("add").unwrap().0, "ab", "one line");
        player.submit("add");
        assert_eq!(
            player.told().last(),
            Some(&Input::Submit(Typed {
                id: "add".into(),
                text: "ab".into(),
                seq: 1,
            })),
            "typed over the text of seq 1"
        );

        player.focus_field("pass");
        player.told();
        player.edit_field("pass", "hunter2");
        assert!(
            player.told().is_empty(),
            "a secret goes only when submitted"
        );
        player.submit("pass");
        assert_eq!(
            player.told(),
            [Input::Submit(Typed {
                id: "pass".into(),
                text: "hunter2".into(),
                seq: 0,
            })]
        );
        player.focus_field("note");
        player.edit_field("note", "a\n\tb\r");
        assert_eq!(player.field_text("note").unwrap().0, "a\n\tb");
        player.submit("add");
        assert!(
            !player
                .told()
                .iter()
                .any(|input| matches!(input, Input::Submit(_))),
            "only the field with the keyboard submits"
        );
    }

    #[test]
    fn keys_go_to_the_panel_only_as_it_takes_them() {
        let mut player = Player::new(env(300.0, 400.0));
        let mut frame = fields(vec![field("a", 10.0), field("b", 50.0)]);
        frame.keys(&["ArrowDown", "j"]);
        player.frame(frame);
        let plain = Mods::default();
        assert!(!player.key("ArrowDown", plain));
        player.focus_panel();
        assert_eq!(player.told(), [focus("a")], "in its first field");
        assert!(player.key("ArrowDown", plain));
        assert_eq!(
            player.told(),
            [Input::Key(Key {
                key: "ArrowDown".into(),
                mods: plain
            })]
        );
        assert!(
            !player.key("x", plain),
            "not the panel's: the client lets the app have it"
        );
        assert!(player.told().is_empty());
        let ctrl = Mods {
            ctrl: true,
            ..Mods::default()
        };
        assert!(!player.key("j", ctrl), "a chord is the app's");
        let shift = Mods {
            shift: true,
            ..Mods::default()
        };
        player.key("Tab", plain);
        assert_eq!(player.focused_field(), Some("b"));
        player.key("Tab", plain);
        assert_eq!(player.focused_field(), Some("a"), "round again");
        player.key("Tab", shift);
        assert_eq!(player.focused_field(), Some("b"));
        player.told();
        assert!(player.key("Escape", plain));
        assert_eq!(player.told(), [Input::Blur]);
        assert_eq!(player.keyboard(), &Keyboard::Away);
    }

    #[test]
    fn a_field_that_goes_takes_the_keyboard_and_a_release_gives_it_back() {
        let mut player = Player::new(env(300.0, 400.0));
        player.frame(fields(vec![field("a", 10.0)]));
        player.focus_field("a");
        player.told();
        player.frame(Frame::default());
        assert_eq!(player.told(), [Input::Blur]);

        player.focus_panel();
        assert_eq!(player.keyboard(), &Keyboard::Panel, "nothing to type in");
        let mut released = Frame::default();
        released.release(1);
        player.frame(released.clone());
        assert_eq!(player.keyboard(), &Keyboard::Away);
        player.focus_panel();
        player.frame(released);
        assert_eq!(player.keyboard(), &Keyboard::Panel, "once for each seq");

        // Asked for before the panel is drawn: its first field gets it,
        // and the release its first frame says asks nothing.
        let mut fresh = Player::new(env(300.0, 400.0));
        fresh.focus_panel();
        assert!(fresh.has_keyboard());
        let mut first = fields(vec![field("q", 10.0)]);
        first.release(0);
        fresh.frame(first);
        assert_eq!(fresh.focused_field(), Some("q"));
        assert_eq!(fresh.told(), [Input::Focus(Focus::default()), focus("q")]);

        // Started again, the plugin never had it.
        fresh.restarted();
        assert_eq!(fresh.keyboard(), &Keyboard::Away);
        assert!(fresh.told().is_empty());
    }

    #[test]
    fn a_field_put_in_place_of_the_one_typed_in_takes_the_keyboard() {
        let mut player = Player::new(env(300.0, 400.0));
        player.frame(fields(vec![field("add", 10.0)]));
        player.focus_field("add");
        player.told();
        player.frame(fields(vec![field("edit", 10.0).focus(1)]));
        assert_eq!(player.focused_field(), Some("edit"));
        assert_eq!(player.told(), [focus("edit")], "no blur between");

        // Gone with none in its place, or let go of in the same frame.
        player.frame(fields(vec![field("other", 10.0)]));
        assert_eq!(player.keyboard(), &Keyboard::Away);
        player.focus_field("other");
        let mut both = fields(vec![field("next", 10.0).focus(2)]);
        both.release(1);
        player.frame(both);
        assert_eq!(player.keyboard(), &Keyboard::Away, "the release wins");
    }

    #[test]
    fn a_jump_through_a_row_scrolls_only_as_far_as_it_takes_to_show_it() {
        let mut player = Player::new(env(300.0, 400.0));
        let to_row = |row: f32, seq: u32| {
            Some(Jump {
                to: row,
                seq,
                through: Some(row + 1.0),
            })
        };
        let mut show = |top: Option<Jump>| {
            player.frame(Frame {
                items: vec![list(1_000, "a", top)],
                ..Frame::default()
            });
            answer(&mut player);
            texts(&player)[0].clone()
        };
        assert_eq!(show(to_row(15.0, 1)), "row 6", "down, to show it last");
        assert_eq!(show(to_row(3.0, 2)), "row 3", "up, to show it first");
        assert_eq!(show(to_row(5.0, 3)), "row 3", "in view already");
    }

    #[test]
    fn a_clear_button_takes_room_at_the_end_of_a_field_on_one_line() {
        let plain = Field::new("q", 10.0, 0.0, 200.0, 30.0);
        let clear = plain.clone().clear();
        assert_eq!(
            field_inside(&clear).width(),
            field_inside(&plain).width() - FIELD_ICON - FIELD_ICON_GAP
        );
        assert_eq!(
            field_clear_at(&clear),
            Some((
                10.0 + 200.0 - FIELD_INSET - FIELD_ICON,
                (30.0 - FIELD_ICON) / 2.0,
                FIELD_ICON
            ))
        );
        let lines = Field::new("n", 10.0, 0.0, 200.0, 80.0).lines().clear();
        assert!(!field_clear(&lines));
        assert_eq!(field_clear_at(&lines), None);
    }

    #[test]
    fn a_program_started_again_counts_its_focus_and_release_afresh() {
        let mut player = Player::new(env(300.0, 400.0));
        let plain = fields(vec![field("a", 10.0), field("b", 50.0)]);
        let mut to_b = plain.clone();
        if let Item::Field(b) = &mut to_b.items[1] {
            b.focus = Some(1);
        }
        let mut released = plain.clone();
        released.release(1);
        player.frame(plain.clone());
        player.focus_field("a");
        player.frame(to_b.clone());
        assert_eq!(player.focused_field(), Some("b"));
        player.frame(released.clone());
        assert_eq!(player.keyboard(), &Keyboard::Away);

        player.restarted();
        player.frame(plain.clone());
        player.focus_field("a");
        player.frame(to_b);
        assert_eq!(player.focused_field(), Some("b"), "its first ask");
        player.frame(released.clone());
        assert_eq!(player.keyboard(), &Keyboard::Away, "its first release");

        // Pressed in before it drew: the release it opens with asks nothing.
        player.restarted();
        player.focus_field("a");
        player.frame(released);
        assert_eq!(player.focused_field(), Some("a"));
    }

    #[test]
    fn a_hit_over_a_field_answers_and_what_was_typed_goes_with_the_program_that_heard_it() {
        let mut player = Player::new(env(300.0, 400.0));
        let mut frame = fields(vec![field("filter", 10.0)]);
        // A clear button over the field's right end.
        frame.items.push(hit("clear", 180.0, 10.0, 30.0, 26.0));
        player.frame(frame.clone());
        assert_eq!(
            player.field_at(50.0, 20.0).map(|f| f.id.as_str()),
            Some("filter")
        );
        assert!(
            player.field_at(190.0, 20.0).is_none(),
            "the button is over it"
        );
        // A client's own field over the painting leaves it to the button.
        let covered = |player: &Player| {
            player.painting().ops.into_iter().find_map(|op| match op {
                Op::Field { covered, .. } => Some(covered),
                _ => None,
            })
        };
        assert_eq!(covered(&player), Some(vec![[180.0, 10.0, 30.0, 26.0]]));
        // Two over the same end: in pieces that do not overlap.
        let mut twice = frame.clone();
        twice.items.push(hit("again", 160.0, 10.0, 30.0, 26.0));
        player.frame(twice);
        assert_eq!(
            covered(&player),
            Some(vec![[180.0, 10.0, 30.0, 26.0], [160.0, 10.0, 20.0, 26.0]])
        );
        let pressed = player.click(190.0, 20.0, Button::Left, 1, Mods::default());
        assert!(matches!(pressed, Some(Input::Click(Click { ref id, .. })) if id == "clear"));

        player.focus_field("filter");
        player.edit_field("filter", "src");
        // The program started again draws the field as it knows it: its text
        // is taken, though its seq is the one taken before.
        player.restarted();
        assert_eq!(
            player.field_text("filter").unwrap().0,
            "src",
            "until it draws"
        );
        player.frame(frame);
        assert_eq!(player.field_text("filter").unwrap().0, "");
    }

    #[test]
    fn a_field_is_drawn_in_its_place_and_painted_for_a_client_of_its_own() {
        let mut player = Player::new(env(300.0, 400.0));
        let mut frame = fields(vec![
            field("a", 10.0).value("hi", 0).placeholder("Symbol"),
            field("a", 50.0),
        ]);
        frame.items.push(Item::Scroll(Scroll {
            id: "s".into(),
            x: 0.0,
            y: 100.0,
            w: 300.0,
            h: 100.0,
            height: 300.0,
            items: vec![field("inner", 0.0).into()],
            top: None,
        }));
        player.frame(frame);
        assert_eq!(
            player.fields().count(),
            1,
            "the first of an id, and none scrolled"
        );
        player.focus_field("a");
        let draws = player.draw();
        let parts: Vec<(FieldPart, Bounds)> = draws
            .iter()
            .filter_map(|draw| match draw.what {
                Drawn::Field(_, part) => Some((part, draw.clip)),
                _ => None,
            })
            .collect();
        assert_eq!(parts.len(), 2);
        assert_eq!(
            parts[0].0,
            FieldPart::Box {
                focused: true,
                hovered: false
            }
        );
        assert_eq!(
            parts[1],
            (FieldPart::Text, Bounds::new(20.0, 10.0, 180.0, 26.0))
        );
        assert!(player.pointer_moved(20.0, 20.0), "over the field now");
        assert_eq!(player.cursor(), Some(Cursor::Text));
        let hovered = player.draw().iter().any(|draw| {
            matches!(
                draw.what,
                Drawn::Field(_, FieldPart::Box { hovered: true, .. })
            )
        });
        assert!(hovered);

        let painting = player.painting();
        let fields: Vec<&Op> = painting
            .ops
            .iter()
            .filter(|op| matches!(op, Op::Field { .. }))
            .collect();
        assert_eq!(
            fields.len(),
            1,
            "the box, which the client puts its own over"
        );
        let Op::Field {
            text,
            focused,
            limit,
            placeholder,
            ..
        } = fields[0]
        else {
            unreachable!()
        };
        assert_eq!(
            (text.as_str(), *focused, *limit, placeholder.as_str()),
            ("hi", true, FIELD_LIMIT, "Symbol")
        );
        let json = serde_json::to_value(fields[0]).unwrap();
        assert_eq!(
            (&json["op"], &json["kind"]),
            (&json!("field"), &json!("line"))
        );
        assert_eq!(json["inside"], json!([20.0, 10.0, 180.0, 26.0]));
        assert!(json.get("icon").is_none());
    }

    #[test]
    fn a_field_on_one_line_is_round_at_its_ends_with_room_for_an_icon_every_client_has() {
        let search = Field::new("q", 10.0, 0.0, 200.0, 30.0).icon("search");
        assert_eq!(field_icon(&search), Some("search"));
        let inside = field_inside(&search);
        let lead = FIELD_INSET + FIELD_ICON + FIELD_ICON_GAP;
        assert_eq!(
            (inside.left, inside.width()),
            (10.0 + lead, 200.0 - lead - FIELD_INSET)
        );
        assert_eq!(field_radius(&search), 15.0);

        // One no client has, or on a field of lines: none, and no room.
        let unknown = Field::new("q", 10.0, 0.0, 200.0, 30.0).icon("rocket");
        assert_eq!(field_icon(&unknown), None);
        assert_eq!(field_inside(&unknown).left, 10.0 + FIELD_INSET);
        let lines = Field::new("note", 10.0, 0.0, 200.0, 80.0)
            .lines()
            .icon("pencil");
        assert_eq!(field_icon(&lines), None);
        assert_eq!(field_radius(&lines), FIELD_RADIUS);

        let mut player = Player::new(env(300.0, 400.0));
        player.frame(fields(vec![field("a", 10.0).icon("plus")]));
        let icon = player.painting().ops.into_iter().find_map(|op| match op {
            Op::Field { icon, .. } => icon,
            _ => None,
        });
        let at = (10.0 + FIELD_INSET, 10.0 + (26.0 - FIELD_ICON) / 2.0);
        assert_eq!(
            icon.map(|icon| (icon.name, icon.x, icon.y, icon.size)),
            Some(("plus".to_string(), at.0, at.1, FIELD_ICON))
        );
    }
}
