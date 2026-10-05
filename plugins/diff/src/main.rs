//! A ThinkTerm plugin with a panel in the right sidebar: the changes in the
//! git repository of the terminal beside it since its last commit, shown
//! the way GitHub Desktop shows them -- the files in the panel, and the
//! lines of the one picked in its extended view, the wide area beside the
//! sidebar; or below the files, where a client has no room for one. It
//! asks git again every two seconds while a panel is on show, and sends a
//! file's lines only as they are asked for, so a file of ten thousand
//! changed lines costs what shows. A terminal on another machine has git
//! asked there, through ThinkTerm's connection to it. Where a client has
//! fields, one above the files filters them by name; with the keyboard in
//! the panel, up and down pick the file before or after, and Return in
//! the filter the first it leaves.
//!
//! It draws by hand, with the SDK's items and one field.
//! docs/thinkterm/plugins.md says how to install it.

mod git;
mod text;
mod watch;

use git::{Change, FileDiff, Kind, Line, Note, Status};
use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use text::Words;
use thinkterm_plugin_sdk::panel::{
    feature, Align, Click, Env, FieldText, Frame, Hit, Input, Item, Jump, Key, List, Rect,
    RowsWanted, Size, Text, Token,
};
use thinkterm_plugin_sdk::{Cx, Plugin, View};
use unicode_width::UnicodeWidthChar;
use watch::{Dir, Place, Repo, Shared};

/// Room at the panel's sides.
const PAD: f32 = 12.0;
/// Room at the extended view's sides, which has more to spare.
const WIDE_PAD: f32 = 20.0;
/// A status badge's side.
const BADGE: f32 = 16.0;
/// The corners of a file's row where it is picked, or the pointer is over
/// it.
const ROW_RADIUS: f32 = 6.0;
/// Room kept at a list's right edge for its scrollbar.
const THUMB_ROOM: f32 = 8.0;
/// The field the files are filtered with.
const FILTER: &str = "filter";

struct DiffPanel {
    shared: Shared,
    /// What each panel's filter holds, by the panel's view.
    filters: HashMap<u64, FieldText>,
    /// How many times each panel's file was picked with the keys: its list
    /// keeps the last in view, once for each.
    reveals: HashMap<u64, u32>,
}

/// Where things go in a panel of one size, listing `files` rows: the
/// picked file's lines below them when it shows those itself (`inside`),
/// the files alone down to its foot when its extended view does. A client
/// with fields has the filter above the files.
struct Layout {
    width: f32,
    /// Where the filter goes, and how tall it is: none without fields.
    filter: Option<(f32, f32)>,
    header: f32,
    file_row: f32,
    files_height: f32,
    /// Room at the file rows' right: for the scrollbar when they scroll.
    files_room: f32,
    diff_header_top: f32,
    diff_header: f32,
    diff_top: f32,
}

impl Layout {
    fn of(env: &Env, files: usize, inside: bool) -> Self {
        let mut header = 10.0 + env.title.line + env.small.line + 10.0;
        let filter = env.has(feature::FIELDS).then(|| {
            let tall = env.body.line + 10.0;
            let top = header - 2.0;
            header += tall + 6.0;
            (top, tall)
        });
        let file_row = env.body.line + env.small.line + 12.0;
        let wanted = files as f32 * file_row;
        let room = (env.height - header).max(0.0);
        let files_height = if inside {
            wanted
                .min((env.height * 0.38).max(file_row * 3.0))
                .min(room)
        } else {
            room
        };
        let diff_header_top = header + files_height + 1.0;
        let diff_header = env.body.line + 16.0;
        let diff_top = diff_header_top + diff_header;
        Self {
            width: env.width,
            filter,
            header,
            file_row,
            files_height,
            files_room: if wanted > files_height {
                THUMB_ROOM
            } else {
                0.0
            },
            diff_header_top,
            diff_header,
            diff_top,
        }
    }
}

/// Where a diff's columns go: its two line numbers, the mark, the text --
/// spaced out more in the extended view (`wide`) than in the panel.
struct Gutter {
    advance: f32,
    row: f32,
    /// Room at the rows' sides.
    pad: f32,
    /// Where the first number's column starts.
    left: f32,
    gutter: f32,
    marker_x: f32,
    text_x: f32,
}

impl Gutter {
    fn of(env: &Env, diff: &FileDiff, wide: bool) -> Self {
        let advance = env.mono.advance.max(1.0);
        let digits = diff.biggest.to_string().len().max(3) as f32;
        let (pad, spare, lead) = if wide {
            (WIDE_PAD, 12.0, 4.0)
        } else {
            (PAD, 8.0, 2.0)
        };
        let gutter = digits * advance + spare;
        let left = pad / 2.0;
        let marker_x = left + gutter * 2.0;
        Self {
            advance,
            row: env.mono.line + lead,
            pad,
            left,
            gutter,
            marker_x,
            text_x: marker_x + advance * 2.0,
        }
    }

    /// How wide the rows are: as wide as the widest line, or the view.
    fn width(&self, diff: &FileDiff, view: f32) -> f32 {
        (self.text_x + diff.columns as f32 * self.advance + self.pad).max(view)
    }
}

impl Plugin for DiffPanel {
    fn draw(&mut self, view: &View, frame: &mut Frame) {
        if view.extended() {
            return self.draw_extended(view, frame);
        }
        let env = &view.env;
        let words = text::words(&env.locale);
        let dir = Dir::of(env);
        // A file's lines go in the extended view when there is one, or
        // room for one; below the files when there is not.
        let inside = view.extension.is_none() && !env.can_extend;
        watch::place(&self.shared, view.id, dir.clone(), inside);
        let Some(dir) = dir else {
            say(frame, env, PAD, PAD, words.no_terminal, None);
            return;
        };
        let state = watch::lock(&self.shared);
        match state.places.get(&dir) {
            None => say(frame, env, PAD, PAD, words.looking, None),
            Some(Place::NoGit) => say(frame, env, PAD, PAD, words.no_git, None),
            Some(Place::NotRepo) => {
                let dir = dir.to_string();
                say(frame, env, PAD, PAD, words.not_repo, Some(&dir));
            }
            Some(Place::Failed(why)) => say(frame, env, PAD, PAD, words.failed, Some(why)),
            Some(Place::Repo(repo)) => {
                frame.extend(state.picked(view.id, repo).is_some());
                // Up and down pick the file before or after, while the
                // panel has the keyboard.
                frame.keys(&["ArrowUp", "ArrowDown"]);
                let shown = state.shown(view.id, repo);
                let filter = self.filters.entry(view.id).or_default();
                let reveal = self.reveals.get(&view.id).copied();
                draw_repo(frame, env, words, &dir, repo, shown, inside, filter, reveal);
            }
        }
    }

    fn rows(&mut self, view: &View, wanted: &RowsWanted) -> Vec<Vec<Item>> {
        let env = &view.env;
        let words = text::words(&env.locale);
        let panel = view.panel();
        let state = watch::lock(&self.shared);
        let Some(want) = state.views.get(&panel) else {
            return Vec::new();
        };
        let Some(dir) = want.dir.clone() else {
            return Vec::new();
        };
        let Some(Place::Repo(repo)) = state.places.get(&dir) else {
            return Vec::new();
        };
        // What the panel marks and shows the lines of: in its extended
        // view, only what was picked.
        let shown = match view.extended() {
            true => state.picked(panel, repo),
            false => state.shown(panel, repo),
        };
        let (from, to) = (wanted.from as usize, wanted.to as usize);
        let filter = self.filters.get(&panel).map_or("", FieldText::text);
        match wanted.list.as_str() {
            "files" if !view.extended() && wanted.key == files_key(&dir, filter) => {
                let files = listed(repo, filter);
                let count = rows_of(repo, filter);
                let at = Layout::of(env, count, want.inside);
                (from..to.min(count))
                    .map(|row| match files.get(row) {
                        Some(file) => {
                            let marked = shown.is_some_and(|shown| shown.path == file.path);
                            file_row(env, &at, file, marked)
                        }
                        None => more_row(&at, words, repo.snapshot.more),
                    })
                    .collect()
            }
            "diff" => {
                let Some(file) = shown.filter(|file| wanted.key == diff_key(&dir, &file.path))
                else {
                    return Vec::new();
                };
                let Some(read) = repo.diffs.get(&file.path) else {
                    return Vec::new();
                };
                let diff = Arc::clone(&read.diff);
                let at = Gutter::of(env, &diff, view.extended());
                let lines = &diff.lines;
                lines[from.min(lines.len())..to.min(lines.len())]
                    .iter()
                    .map(|line| diff_row(env, &at, words, line))
                    .collect()
            }
            _ => Vec::new(),
        }
    }

    fn input(&mut self, view: &View, input: Input, _cx: &mut Cx) {
        let panel = view.panel();
        match input {
            Input::Click(Click { id, .. }) => {
                if let Some(path) = id.strip_prefix("file:") {
                    watch::pick(&self.shared, panel, Some(path.to_string()));
                }
            }
            // The extended view was closed: the file is put down, and the
            // panel stops asking for it.
            Input::Close => watch::pick(&self.shared, panel, None),
            Input::Text(typed) if typed.id == FILTER => {
                self.filters.entry(panel).or_default().heard(&typed);
            }
            // Return in the filter picks the first file it leaves.
            Input::Submit(typed) if typed.id == FILTER => {
                self.filters.entry(panel).or_default().heard(&typed);
                self.pick_listed(panel, |_, _| 0);
                *self.reveals.entry(panel).or_default() += 1;
            }
            Input::Key(Key { key, .. }) => {
                match key.as_str() {
                    "ArrowUp" => {
                        self.pick_listed(panel, |at, _| at.map_or(0, |at| at.saturating_sub(1)))
                    }
                    "ArrowDown" => self.pick_listed(panel, |at, count| {
                        at.map_or(0, |at| (at + 1).min(count - 1))
                    }),
                    _ => return,
                }
                *self.reveals.entry(panel).or_default() += 1;
            }
            _ => {}
        }
    }

    fn closed(&mut self, view: &View) {
        // The panel an extended view leaves keeps what it wants.
        if !view.extended() {
            watch::close(&self.shared, view.id);
            self.filters.remove(&view.id);
            self.reveals.remove(&view.id);
        }
    }
}

impl DiffPanel {
    /// Picks the file of panel `panel`'s list -- as its filter leaves it --
    /// that `next` says, from where the one it shows is in it, if it is, and
    /// how many there are.
    fn pick_listed(&self, panel: u64, next: impl FnOnce(Option<usize>, usize) -> usize) {
        let path = {
            let state = watch::lock(&self.shared);
            let Some(dir) = state.views.get(&panel).and_then(|want| want.dir.clone()) else {
                return;
            };
            let Some(Place::Repo(repo)) = state.places.get(&dir) else {
                return;
            };
            let filter = self.filters.get(&panel).map_or("", FieldText::text);
            let files = listed(repo, filter);
            if files.is_empty() {
                return;
            }
            let shown = state.shown(panel, repo);
            let at = shown.and_then(|shown| files.iter().position(|file| file.path == shown.path));
            files[next(at, files.len()).min(files.len() - 1)]
                .path
                .clone()
        };
        watch::pick(&self.shared, panel, Some(path));
    }

    /// The extended view: the lines of the file its panel picked.
    fn draw_extended(&self, view: &View, frame: &mut Frame) {
        let env = &view.env;
        let words = text::words(&env.locale);
        let panel = view.panel();
        let state = watch::lock(&self.shared);
        let Some(dir) = state.views.get(&panel).and_then(|want| want.dir.clone()) else {
            return;
        };
        let Some(Place::Repo(repo)) = state.places.get(&dir) else {
            return;
        };
        // Nothing picked: the panel no longer asks for it, and it goes.
        if let Some(file) = state.picked(panel, repo) {
            draw_file(frame, env, words, &dir, repo, file);
        }
    }
}

/// The key of the files list of the panels in `dir`, filtered by `filter`:
/// another filter, other rows.
fn files_key(dir: &Dir, filter: &str) -> String {
    format!("{}\n{filter}", dir.key())
}

/// The files of `repo` whose paths have `filter` in them, whatever their
/// case: all of them for none.
fn listed<'a>(repo: &'a Repo, filter: &str) -> Vec<&'a Change> {
    let filter = filter.trim().to_lowercase();
    repo.snapshot
        .files
        .iter()
        .filter(|file| filter.is_empty() || file.path.to_lowercase().contains(&filter))
        .collect()
}

/// The key of the list of `path`'s lines, in `dir`.
fn diff_key(dir: &Dir, path: &str) -> String {
    format!("{}\n{path}", dir.key())
}

/// Rows the files list has, as `filter` leaves it: one a file, and, with no
/// filter, one saying how many more there are when not all are listed.
fn rows_of(repo: &Repo, filter: &str) -> usize {
    let more = filter.trim().is_empty() && repo.snapshot.more > 0;
    listed(repo, filter).len() + usize::from(more)
}

/// The repository's name, branch and changes, the filter, and the files:
/// with the lines of the one `shown` below them, `inside` the panel.
#[allow(clippy::too_many_arguments)]
fn draw_repo(
    frame: &mut Frame,
    env: &Env,
    words: &Words,
    dir: &Dir,
    repo: &Repo,
    shown: Option<&Change>,
    inside: bool,
    filter: &FieldText,
    reveal: Option<u32>,
) {
    let snapshot = &repo.snapshot;
    let rows = rows_of(repo, filter.text());
    let at = Layout::of(env, rows, inside);
    let name = snapshot
        .root
        .rsplit('/')
        .find(|name| !name.is_empty())
        .unwrap_or(&snapshot.root)
        .to_string();
    let (added, removed) = snapshot
        .files
        .iter()
        .filter_map(|file| file.counts)
        .fold((0, 0), |(a, r), (added, removed)| (a + added, r + removed));
    let totals = counts_width(env, added, removed) + 8.0;
    let title = Text::new(
        PAD,
        10.0,
        at.width - PAD * 2.0 - totals,
        env.title.line,
        name,
    );
    frame.push(title.size(Size::Title).bold());
    counts(
        &mut frame.items,
        env,
        added,
        removed,
        at.width - PAD,
        10.0,
        env.title.line,
    );
    // A repository on another machine says which.
    let mut about: Vec<String> = dir.host.iter().map(|host| host.name.clone()).collect();
    about.push(snapshot.branch.clone());
    if snapshot.unborn {
        about.push(words.unborn.to_string());
    }
    about.push((words.files)(snapshot.files.len() + snapshot.more));
    let about = Text::new(
        PAD,
        10.0 + env.title.line,
        at.width - PAD * 2.0,
        env.small.line,
        about.join(" \u{00b7} "),
    );
    frame.push(about.size(Size::Small).color(Token::TextMuted));
    if snapshot.files.is_empty() {
        // Where the filter would be: there is nothing to filter.
        let top = at.filter.map_or(at.header, |(top, _)| top);
        say(frame, env, PAD, top, words.clean, None);
        return;
    }

    if let Some((top, tall)) = at.filter {
        let field = filter.field(FILTER, PAD, top, at.width - PAD * 2.0, tall);
        frame.push(field.placeholder(words.filter).icon("search").clear());
    }
    let listed = listed(repo, filter.text());
    if listed.is_empty() {
        say(frame, env, PAD, at.header, words.no_match, None);
    }
    let version = {
        let mut hasher = DefaultHasher::new();
        (repo.revision, shown.map(|file| &file.path), inside).hash(&mut hasher);
        hasher.finish() as u32
    };
    let mut files = List::new(
        "files",
        0.0,
        at.header,
        at.width,
        at.files_height,
        rows as u32,
        at.file_row,
        files_key(dir, filter.text()),
    );
    // The file picked with the keys, kept in view.
    let picked = shown.and_then(|file| listed.iter().position(|row| row.path == file.path));
    if let (Some(seq), Some(at)) = (reveal, picked) {
        let at = at as f32;
        files = files.top(Jump {
            to: at,
            seq,
            through: Some(at + 1.0),
        });
    }
    frame.push(files.version(version));
    if !inside {
        return;
    }
    frame.push(Rect::new(0.0, at.diff_header_top - 1.0, at.width, 1.0).fill(Token::Border));

    let Some(file) = shown else {
        return;
    };
    let top = at.diff_header_top;
    frame.push(Rect::new(0.0, top, at.width, at.diff_header).fill(Token::BgRaised));
    let room = file.counts.map_or(0.0, |(added, removed)| {
        counts_width(env, added, removed) + 8.0
    });
    let path = Text::new(
        PAD,
        top,
        at.width - PAD * 2.0 - room,
        at.diff_header,
        &file.path,
    );
    frame.push(path.bold());
    if let Some((added, removed)) = file.counts {
        counts(
            &mut frame.items,
            env,
            added,
            removed,
            at.width - PAD,
            top,
            at.diff_header,
        );
    }
    lines(frame, env, words, dir, repo, file, at.diff_top, false);
}

/// The extended view: the picked file's name, where it is and how much of
/// it changed, and its lines, with room to spare. The name starts after the
/// close button ThinkTerm draws at the top left, and lines up with it.
fn draw_file(frame: &mut Frame, env: &Env, words: &Words, dir: &Dir, repo: &Repo, file: &Change) {
    let width = env.width;
    let (folder, name) = split_path(&file.path);
    let below = match &file.from {
        Some(from) => format!("{from} \u{2192} {folder}"),
        None => folder.to_string(),
    };
    let (left, top) = match env.close {
        Some(close) => (
            close.x + close.size + 10.0,
            close.y + (close.size - env.title.line) / 2.0,
        ),
        None => (WIDE_PAD, 14.0),
    };
    let tall = env.title.line
        + if below.is_empty() {
            0.0
        } else {
            env.small.line
        };
    let clear = env.close.map_or(0.0, |close| close.y + close.size + 8.0);
    let head = (top + tall + 12.0).max(clear);
    let middle = top + env.title.line / 2.0;

    let (letter, color) = status_mark(file.status);
    let badge = BADGE + 2.0;
    let badge_y = middle - badge / 2.0;
    frame.push(
        Rect::new(left, badge_y, badge, badge)
            .border(color)
            .radius(4.0),
    );
    let mark = Text::new(left, badge_y, badge, badge, letter)
        .color(color)
        .size(Size::Small)
        .bold();
    frame.push(mark.align(Align::Center));

    let text_x = left + badge + 12.0;
    let mut end = width - WIDE_PAD;
    if let Some((added, removed)) = file.counts {
        counts(
            &mut frame.items,
            env,
            added,
            removed,
            end,
            top,
            env.title.line,
        );
        end -= counts_width(env, added, removed) + 16.0;
    }
    let title = Text::new(text_x, top, end - text_x, env.title.line, name);
    frame.push(title.size(Size::Title).bold());
    if !below.is_empty() {
        let below = Text::new(
            text_x,
            top + env.title.line,
            width - WIDE_PAD - text_x,
            env.small.line,
            below,
        );
        frame.push(below.size(Size::Small).color(Token::TextMuted));
    }
    frame.push(Rect::new(0.0, head - 1.0, width, 1.0).fill(Token::Border));
    lines(frame, env, words, dir, repo, file, head, true);
}

/// `file`'s lines, from `top` to the foot of the view: laid out `wide` in
/// the extended view.
#[allow(clippy::too_many_arguments)]
fn lines(
    frame: &mut Frame,
    env: &Env,
    words: &Words,
    dir: &Dir,
    repo: &Repo,
    file: &Change,
    top: f32,
    wide: bool,
) {
    let pad = if wide { WIDE_PAD } else { PAD };
    let Some(read) = repo.diffs.get(&file.path) else {
        say(frame, env, pad, top, words.reading, None);
        return;
    };
    let gutter = Gutter::of(env, &read.diff, wide);
    let list = List::new(
        "diff",
        0.0,
        top,
        env.width,
        (env.height - top).max(0.0),
        read.diff.lines.len() as u32,
        gutter.row,
        diff_key(dir, &file.path),
    );
    let width = gutter.width(&read.diff, env.width);
    frame.push(list.version(read.version).wide(width, gutter.text_x));
}

/// A path's folder, empty at the root, and its name.
fn split_path(path: &str) -> (&str, &str) {
    path.rsplit_once('/').unwrap_or(("", path))
}

/// A file's row: its status, name and directory, and how many lines it
/// gained and lost. The whole row answers a click; the tint under the
/// pointer and the mark of the one picked sit inside it, rounded alike.
fn file_row(env: &Env, at: &Layout, file: &Change, picked: bool) -> Vec<Item> {
    let row = at.file_row;
    let id = format!("file:{}", file.path);
    let (left, right) = (PAD / 2.0, at.width - PAD / 2.0 - at.files_room);
    let (inset, height) = (2.0, row - 4.0);
    let mut items: Vec<Item> = vec![Hit::new(&id, 0.0, 0.0, at.width, row).into()];
    let tint = Hit::new(&id, left, inset, right - left, height);
    items.push(tint.hover(Token::BgHover).radius(ROW_RADIUS).into());
    if picked {
        let pick = Rect::new(left, inset, right - left, height);
        items.push(pick.fill(Token::BgSelected).radius(ROW_RADIUS).into());
    }
    let (letter, color) = status_mark(file.status);
    let badge_y = (row - BADGE) / 2.0;
    let badge = Rect::new(PAD, badge_y, BADGE, BADGE)
        .border(color)
        .radius(4.0);
    items.push(badge.into());
    let mark = Text::new(PAD, badge_y, BADGE, BADGE, letter)
        .color(color)
        .size(Size::Small)
        .bold();
    items.push(mark.align(Align::Center).into());

    let (dir, name) = split_path(&file.path);
    let text_x = PAD + BADGE + 10.0;
    let below = match &file.from {
        Some(from) => format!("{from} \u{2192} {dir}"),
        None => dir.to_string(),
    };
    // A file at the root has no second line: its name sits in the middle.
    let top = if below.is_empty() {
        (row - env.body.line) / 2.0
    } else {
        (row - env.body.line - env.small.line) / 2.0
    };
    let counts_room = file.counts.map_or(0.0, |(added, removed)| {
        counts_width(env, added, removed) + 8.0
    });
    let end = right - PAD / 2.0;
    items.push(Text::new(text_x, top, end - text_x - counts_room, env.body.line, name).into());
    if !below.is_empty() {
        let below = Text::new(
            text_x,
            top + env.body.line,
            end - text_x,
            env.small.line,
            below,
        );
        items.push(below.size(Size::Small).color(Token::TextMuted).into());
    }
    if let Some((added, removed)) = file.counts {
        counts(&mut items, env, added, removed, end, top, env.body.line);
    }
    items
}

/// The row after the files listed, saying how many more changed.
fn more_row(at: &Layout, words: &Words, more: usize) -> Vec<Item> {
    let text = Text::new(
        PAD,
        0.0,
        at.width - PAD * 2.0,
        at.file_row,
        (words.more)(more),
    );
    vec![text.color(Token::TextMuted).into()]
}

/// A line of the diff: the hunk it starts, or a line with its numbers, its
/// mark and what changed in it. What lies left of the text stays put while
/// the lines scroll sideways.
fn diff_row(env: &Env, at: &Gutter, words: &Words, line: &Line) -> Vec<Item> {
    let width = env.width;
    let height = at.row;
    let mut row: Vec<Item> = Vec::new();
    let marks = match line.kind {
        Kind::Hunk => {
            row.push(
                Rect::new(0.0, 0.0, width, height)
                    .fill(Token::BgRaised)
                    .into(),
            );
            let text = Text::new(at.left, 0.0, width - at.pad, height, &line.text);
            row.push(text.mono().color(Token::TextMuted).into());
            return row;
        }
        Kind::Note(note) => {
            let (said, x) = match note {
                Note::NoNewline => (words.no_newline.to_string(), at.text_x),
                Note::Binary => (words.binary.to_string(), at.pad),
                Note::Mode => (words.mode.to_string(), at.pad),
                Note::Unchanged => (words.unchanged.to_string(), at.pad),
                Note::Cut => (words.cut.to_string(), at.pad),
                Note::Failed => (format!("{}{}", words.failed, line.text), at.pad),
            };
            let text = Text::new(x, 0.0, width - x - at.left, height, said);
            row.push(text.color(Token::TextFaint).size(Size::Small).into());
            return row;
        }
        Kind::Added => Some((
            Token::PositiveBg,
            Token::PositiveBgStrong,
            "+",
            Token::Positive,
        )),
        Kind::Removed => Some((
            Token::NegativeBg,
            Token::NegativeBgStrong,
            "-",
            Token::Negative,
        )),
        Kind::Context => None,
    };
    if let Some((ground, _, _, _)) = marks {
        row.push(Rect::new(0.0, 0.0, width, height).fill(ground).into());
    }
    for (number, x) in [(line.old, at.left), (line.new, at.left + at.gutter)] {
        if let Some(number) = number {
            let text = Text::new(x, 0.0, at.gutter - 8.0, height, number.to_string());
            row.push(
                text.mono()
                    .color(Token::TextFaint)
                    .align(Align::Right)
                    .into(),
            );
        }
    }
    let advance = at.advance;
    if let Some((_, strong, mark, color)) = marks {
        let mark = Text::new(at.marker_x, 0.0, advance * 2.0, height, mark);
        row.push(mark.mono().color(color).into());
        if let Some((start, end)) = line.changed {
            let left = at.text_x + columns_of(line.text.chars().take(start)) * advance;
            let right = at.text_x + columns_of(line.text.chars().take(end)) * advance;
            if right > left {
                let changed = Rect::new(left, 1.0, right - left, height - 2.0);
                row.push(changed.fill(strong).radius(2.0).into());
            }
        }
    }
    if !line.text.is_empty() {
        let wide = (git::columns(&line.text) + 1) as f32 * advance;
        let text = Text::new(at.text_x, 0.0, wide, height, &line.text);
        row.push(text.mono().into());
    }
    row
}

/// A message where the view's content goes, from `top`, `pad` in from its
/// sides: `said`, broken into lines to fit, and `detail` faint on one line
/// below.
fn say(frame: &mut Frame, env: &Env, pad: f32, top: f32, said: &str, detail: Option<&str>) {
    let room = (env.width - pad * 2.0).max(1.0);
    let mut y = top + 8.0;
    for line in wrap(said, room, env.body.size) {
        let text = Text::new(pad, y, room, env.body.line, line);
        frame.push(text.color(Token::TextMuted));
        y += env.body.line;
    }
    if let Some(detail) = detail {
        let text = Text::new(pad, y + 4.0, room, env.small.line, detail);
        frame.push(text.size(Size::Small).color(Token::TextFaint));
    }
}

/// `text` broken into lines no wider than `room`, as wide as a font of
/// `size` is thought to set them: at spaces, or between two characters a
/// language writes without them. The panel measures nothing for a plugin,
/// so a character is taken to be half its size wide, twice that when wide.
fn wrap(text: &str, room: f32, size: f32) -> Vec<String> {
    let width = |c: char| match c.width() {
        Some(2) => size,
        _ => size * 0.55,
    };
    let mut lines = Vec::new();
    let mut line = String::new();
    let mut used = 0.0;
    for piece in pieces(text) {
        let wide: f32 = piece.chars().map(width).sum();
        if used + wide > room && !line.trim().is_empty() {
            lines.push(line.trim_end().to_string());
            line = String::new();
            used = 0.0;
            if piece == " " {
                continue;
            }
        }
        line.push_str(piece);
        used += wide;
    }
    if !line.trim().is_empty() {
        lines.push(line.trim_end().to_string());
    }
    lines
}

/// `text` in the pieces a line may break between: words, the spaces
/// between them, and each wide character on its own.
fn pieces(text: &str) -> Vec<&str> {
    let mut pieces = Vec::new();
    let mut start = 0;
    for (at, c) in text.char_indices() {
        if c == ' ' || c.width() == Some(2) {
            if start < at {
                pieces.push(&text[start..at]);
            }
            pieces.push(&text[at..at + c.len_utf8()]);
            start = at + c.len_utf8();
        }
    }
    if start < text.len() {
        pieces.push(&text[start..]);
    }
    pieces
}

fn status_mark(status: Status) -> (&'static str, Token) {
    match status {
        Status::Added | Status::Untracked => ("A", Token::Positive),
        Status::Modified => ("M", Token::Warning),
        Status::Deleted => ("D", Token::Negative),
        Status::Renamed => ("R", Token::Accent),
        Status::Copied => ("C", Token::Accent),
        Status::Conflicted => ("!", Token::Negative),
    }
}

/// "+12 -3" -- either left out when it is naught -- ending at `right`: in
/// the monospaced font, whose widths are known, so the two can sit side by
/// side.
fn counts(
    items: &mut Vec<Item>,
    env: &Env,
    added: u32,
    removed: u32,
    right: f32,
    y: f32,
    height: f32,
) {
    let advance = env.mono.advance;
    let mut right = right;
    if removed > 0 || added == 0 {
        let minus = format!("-{removed}");
        let width = minus.len() as f32 * advance;
        let minus = Text::new(right - width, y, width + 1.0, height, minus);
        items.push(minus.mono().color(Token::Negative).into());
        right -= width + advance;
    }
    if added > 0 {
        let plus = format!("+{added}");
        let width = plus.len() as f32 * advance;
        let plus = Text::new(right - width, y, width + 1.0, height, plus);
        items.push(plus.mono().color(Token::Positive).into());
    }
}

fn counts_width(env: &Env, added: u32, removed: u32) -> f32 {
    let mut chars = 0;
    if removed > 0 || added == 0 {
        chars += format!("-{removed}").len();
    }
    if added > 0 {
        chars += format!("+{added}").len() + usize::from(chars > 0);
    }
    chars as f32 * env.mono.advance
}

/// How many columns `chars` take in the monospaced font.
fn columns_of(chars: impl Iterator<Item = char>) -> f32 {
    chars.map(|c| c.width().unwrap_or(0)).sum::<usize>() as f32
}

fn main() -> std::io::Result<()> {
    let shared = Shared::default();
    thinkterm_plugin_sdk::run_with(|emitter| {
        let fetching = Arc::clone(&shared);
        let started = std::thread::Builder::new()
            .name("git".into())
            .spawn(move || watch::fetch(fetching, emitter));
        if let Err(err) = started {
            eprintln!("diff: cannot start looking: {err}");
        }
        DiffPanel {
            shared,
            filters: HashMap::new(),
            reveals: HashMap::new(),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use git::Snapshot;
    use thinkterm_plugin_sdk::panel::{
        Button, CloseButton, Drawn, Mods, MonoMetrics, Player, TextMetrics,
    };
    use watch::{Read, Want};

    const DIR: &str = "/home/user/project";

    fn env() -> Env {
        let text = |size, line| TextMetrics { size, line };
        Env {
            width: 320.0,
            height: 800.0,
            scale: 2.0,
            dark: true,
            small: text(11.0, 15.0),
            body: text(13.0, 18.0),
            title: text(15.0, 20.0),
            mono: MonoMetrics {
                size: 12.0,
                line: 17.0,
                advance: 7.0,
            },
            locale: "en-US".into(),
            cwd: Some(DIR.into()),
            remote: None,
            can_extend: false,
            close: None,
            features: Vec::new(),
        }
    }

    fn change(path: &str, status: Status) -> Change {
        Change {
            path: path.into(),
            from: None,
            status,
            counts: Some((3, 1)),
        }
    }

    /// A panel over a repository with three changed files, the second of
    /// them ten thousand lines long, one of those far wider than a panel.
    fn panel() -> DiffPanel {
        let shared = Shared::default();
        let mut long = String::from("@@ -1,10000 +1,10000 @@\n");
        for n in 0..10_000 {
            let text = if n == 3 {
                "x".repeat(900)
            } else {
                format!("line {n}")
            };
            long.push_str(&format!(" {text}\n"));
        }
        let files = vec![
            change("README.md", Status::Modified),
            change("src/long.rs", Status::Modified),
            change("src/new.rs", Status::Untracked),
        ];
        let diffs = [(
            "src/long.rs".to_string(),
            Read::new(
                files[1].clone(),
                git::lines(long.as_bytes(), false, false),
                7,
            ),
        )];
        {
            let mut state = watch::lock(&shared);
            state.places.insert(
                Dir {
                    host: None,
                    path: DIR.into(),
                },
                Place::Repo(Repo {
                    snapshot: Snapshot {
                        root: DIR.into(),
                        branch: "main".into(),
                        unborn: false,
                        base: "HEAD".into(),
                        files,
                        more: 0,
                    },
                    revision: 1,
                    diffs: diffs.into_iter().collect(),
                }),
            );
            state.views.insert(1, Want::default());
        }
        DiffPanel {
            shared,
            filters: HashMap::new(),
            reveals: HashMap::new(),
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

    fn show(panel: &mut DiffPanel, view: &View, player: &mut Player) {
        let mut frame = Frame::default();
        panel.draw(view, &mut frame);
        player.frame(frame);
        for want in player.wanted() {
            let rows = panel.rows(view, &want);
            player.rows(thinkterm_plugin_sdk::panel::Rows {
                list: want.list.clone(),
                key: want.key.clone(),
                version: want.version,
                layout: want.layout,
                from: want.from,
                rows,
            });
        }
    }

    #[test]
    fn a_panel_with_no_room_beside_it_shows_the_picked_file_below_a_page_at_a_time() {
        let mut panel = panel();
        let view = View::new(1, env());
        let mut player = Player::new(env());
        show(&mut panel, &view, &mut player);
        let shown = texts(&player);
        assert!(shown.contains(&"project".to_string()), "{shown:?}");
        assert!(shown.contains(&"README.md".to_string()));
        assert!(shown.contains(&"main \u{00b7} 3 changed files".to_string()));
        assert!(
            shown.contains(&"Reading the changes…".to_string()),
            "README.md is not read yet"
        );
        assert!(!player.extend(), "nothing picked asks for nothing");

        // Pick the long file by clicking its row, near the right edge.
        let at = Layout::of(&env(), 3, true);
        let y = at.header + at.file_row * 1.5;
        let input = player
            .click(300.0, y, Button::Left, 1, Mods::default())
            .expect("the row answers across");
        panel.input(&view, input, &mut Cx::new());
        assert_eq!(
            watch::lock(&panel.shared).views[&1].picked.as_deref(),
            Some("src/long.rs")
        );
        show(&mut panel, &view, &mut player);
        let shown = texts(&player);
        assert!(shown.contains(&"line 0".to_string()), "{shown:?}");
        assert!(player.rows_kept() < 1_000, "a page at a time");
        assert!(
            player.extend(),
            "asked for, for a client that has room after all"
        );
    }

    #[test]
    fn the_filter_leaves_the_files_it_names_and_the_keys_pick_among_them() {
        let mut panel = panel();
        let with_fields = Env {
            features: vec![feature::FIELDS.into(), feature::KEYS.into()],
            ..env()
        };
        let view = View::new(1, with_fields.clone());
        let mut player = Player::new(with_fields);
        show(&mut panel, &view, &mut player);
        assert_eq!(player.fields().count(), 1, "the filter");
        let picked = |panel: &DiffPanel| watch::lock(&panel.shared).views[&1].picked.clone();
        let key = |name: &str| {
            Input::Key(Key {
                key: name.into(),
                mods: Mods::default(),
            })
        };
        // Down from the first shown below the files.
        panel.input(&view, key("ArrowDown"), &mut Cx::new());
        assert_eq!(picked(&panel).as_deref(), Some("src/long.rs"));
        panel.input(&view, key("ArrowDown"), &mut Cx::new());
        panel.input(&view, key("ArrowDown"), &mut Cx::new());
        assert_eq!(
            picked(&panel).as_deref(),
            Some("src/new.rs"),
            "no further than the last"
        );
        panel.input(&view, key("ArrowUp"), &mut Cx::new());
        assert_eq!(picked(&panel).as_deref(), Some("src/long.rs"));

        // The list follows the pick made with the keys.
        let mut frame = Frame::default();
        panel.draw(&view, &mut frame);
        let top = frame.items.iter().find_map(|item| match item {
            Item::List(list) if list.id == "files" => list.top,
            _ => None,
        });
        assert_eq!(
            top.and_then(|jump| jump.through),
            Some(2.0),
            "row 1, in view"
        );

        let typed = |text: &str| thinkterm_plugin_sdk::panel::Typed {
            id: FILTER.into(),
            text: text.into(),
            seq: 0,
        };
        panel.input(&view, Input::Text(typed("READ")), &mut Cx::new());
        show(&mut panel, &view, &mut player);
        let shown = texts(&player);
        assert!(shown.contains(&"README.md".to_string()), "{shown:?}");
        assert!(
            !shown.contains(&"new.rs".to_string()),
            "filtered out: {shown:?}"
        );
        panel.input(&view, Input::Submit(typed("READ")), &mut Cx::new());
        assert_eq!(
            picked(&panel).as_deref(),
            Some("README.md"),
            "the first it leaves"
        );
        // The client empties it with its button: a field that asks for one.
        assert!(player.fields().next().is_some_and(|field| field.clear));
        panel.input(&view, Input::Text(typed("zzz")), &mut Cx::new());
        show(&mut panel, &view, &mut player);
        let words = text::words("en-US");
        assert!(texts(&player).contains(&words.no_match.to_string()));
        panel.closed(&view);
        assert!(panel.filters.is_empty() && panel.reveals.is_empty());
    }

    #[test]
    fn a_repository_on_another_machine_says_which_and_is_its_own_place() {
        let mut panel = panel();
        let mut far = env();
        far.cwd = None;
        far.remote = Some(thinkterm_plugin_sdk::panel::Remote {
            host: "server-a".into(),
            machine: "m1".into(),
            cwd: DIR.into(),
        });
        let view = View::new(1, far.clone());
        let mut player = Player::new(far);
        show(&mut panel, &view, &mut player);
        let shown = texts(&player);
        assert_eq!(
            shown,
            ["Looking for changes…"],
            "the same path here is not the same place"
        );
        {
            let mut state = watch::lock(&panel.shared);
            let here = Dir {
                host: None,
                path: DIR.into(),
            };
            let Some(Place::Repo(repo)) = state.places.remove(&here) else {
                panic!("no repository here")
            };
            let there = Dir {
                host: Some(watch::Host {
                    name: "server-a".into(),
                    machine: "m1".into(),
                }),
                path: DIR.into(),
            };
            state.places.insert(there, Place::Repo(repo));
        }
        show(&mut panel, &view, &mut player);
        let shown = texts(&player);
        assert!(
            shown.contains(&"server-a \u{00b7} main \u{00b7} 3 changed files".to_string()),
            "{shown:?}"
        );
    }

    #[test]
    fn long_lines_scroll_sideways_under_still_numbers() {
        let mut panel = panel();
        let view = View::new(1, env());
        let mut player = Player::new(env());
        show(&mut panel, &view, &mut player);
        watch::pick(&panel.shared, 1, Some("src/long.rs".into()));
        show(&mut panel, &view, &mut player);
        let place = |player: &Player, text: &str| {
            player.draw().iter().find_map(|draw| match draw.what {
                Drawn::Text(t) if t.text == text => Some(t.x + draw.dx),
                _ => None,
            })
        };
        let at = Layout::of(&env(), 3, true);
        let long = "x".repeat(900);
        let (number, line) = (place(&player, "4").unwrap(), place(&player, &long).unwrap());
        assert!(
            player.wheel(100.0, at.diff_top + 20.0, 300.0, 0.0),
            "across"
        );
        assert_eq!(place(&player, "4"), Some(number), "the numbers stay");
        assert_eq!(place(&player, &long), Some(line - 300.0), "the lines move");
        assert_eq!(
            place(&player, "line 0"),
            None,
            "a short line went out of sight"
        );
    }

    #[test]
    fn the_hover_and_the_pick_are_one_rounded_shape() {
        let at = Layout::of(&env(), 3, true);
        let row = file_row(&env(), &at, &change("a", Status::Added), true);
        let tint = row.iter().find_map(|item| match item {
            Item::Hit(hit) if hit.hover.is_some() => Some(hit.clone()),
            _ => None,
        });
        let pick = row.iter().find_map(|item| match item {
            Item::Rect(rect) if rect.fill == Some(Token::BgSelected.into()) => Some(rect.clone()),
            _ => None,
        });
        let (tint, pick) = (tint.unwrap(), pick.unwrap());
        assert_eq!(
            (tint.x, tint.y, tint.w, tint.h, tint.radius),
            (pick.x, pick.y, pick.w, pick.h, pick.radius)
        );
        assert!(pick.x + pick.w <= at.width - PAD / 2.0);
    }

    #[test]
    fn a_panel_without_a_terminal_here_says_so() {
        let mut panel = panel();
        let mut env = env();
        env.cwd = None;
        env.locale = "zh-CN".into();
        let view = View::new(2, env.clone());
        let mut player = Player::new(env);
        show(&mut panel, &view, &mut player);
        let shown = texts(&player).join("");
        assert!(shown.contains("在本机的终端里打开一个目录"), "{shown}");
    }

    #[test]
    fn a_panel_with_room_beside_it_lists_the_files_alone_and_asks_once_one_is_picked() {
        let mut panel = panel();
        let env = Env {
            can_extend: true,
            ..env()
        };
        let view = View::new(1, env.clone());
        let mut player = Player::new(env.clone());
        show(&mut panel, &view, &mut player);
        let shown = texts(&player);
        assert!(shown.contains(&"README.md".to_string()), "{shown:?}");
        assert!(
            !shown.contains(&"Reading the changes…".to_string()),
            "no lines in the panel"
        );
        assert!(!player.extend(), "nothing picked asks for nothing");
        let at = Layout::of(&env, 3, false);
        assert_eq!(
            at.header + at.files_height,
            env.height,
            "the files go down to the foot"
        );

        let y = at.header + at.file_row * 1.5;
        let input = player
            .click(100.0, y, Button::Left, 1, Mods::default())
            .unwrap();
        panel.input(&view, input, &mut Cx::new());
        show(&mut panel, &view, &mut player);
        assert!(player.extend(), "a pick asks for the extended view");
        assert!(
            !texts(&player).contains(&"line 0".to_string()),
            "whose lines go there"
        );
    }

    #[test]
    fn the_extended_view_shows_the_picked_file_with_room_beside_thinkterms_close_button() {
        let mut panel = panel();
        let env = Env {
            can_extend: true,
            ..env()
        };
        let mut view = View::new(1, env.clone());
        let mut player = Player::new(env.clone());
        show(&mut panel, &view, &mut player);
        watch::pick(&panel.shared, 1, Some("src/long.rs".into()));
        // The client opens the extended view the panel asks for, its close
        // button at the top left.
        let close = CloseButton {
            x: 5.0,
            y: 2.0,
            size: 22.0,
        };
        let wide = Env {
            width: 640.0,
            can_extend: false,
            close: Some(close),
            ..env.clone()
        };
        let extended = View {
            extends: Some(1),
            ..View::new(2, wide.clone())
        };
        view.extension = Some(2);
        show(&mut panel, &view, &mut player);
        let mut beside = Player::new(wide);
        show(&mut panel, &extended, &mut beside);
        let shown = texts(&beside);
        for text in ["long.rs", "src", "+3", "-1", "line 0"] {
            assert!(shown.contains(&text.to_string()), "{text}: {shown:?}");
        }
        assert!(!texts(&player).contains(&"line 0".to_string()));
        let number = beside.draw().iter().find_map(|draw| match draw.what {
            Drawn::Text(text) if text.text == "1" => Some(text.x + draw.dx),
            _ => None,
        });
        assert_eq!(number, Some(WIDE_PAD / 2.0), "further in than the panel's");
        // The name is beside the button and lined up with it; nothing is
        // drawn over the button.
        let name = beside.draw().iter().find_map(|draw| match draw.what {
            Drawn::Text(text) if text.text == "long.rs" => Some(text.clone()),
            _ => None,
        });
        let name = name.unwrap();
        assert!(name.x > close.x + close.size);
        assert_eq!(name.y + name.h / 2.0, close.y + close.size / 2.0);
        let clear = |x: f32, y: f32| x >= close.x + close.size || y >= close.y + close.size;
        assert!(beside.draw().iter().all(|draw| match draw.what {
            Drawn::Rect(rect) => clear(rect.x + draw.dx, rect.y + draw.dy),
            Drawn::Text(text) => clear(text.x + draw.dx, text.y + draw.dy),
            _ => true,
        }));

        // Closed with it: the file is put down, and the panel stops asking.
        panel.input(&extended, Input::Close, &mut Cx::new());
        assert_eq!(watch::lock(&panel.shared).views[&1].picked, None);
        show(&mut panel, &view, &mut player);
        assert!(!player.extend(), "put down, it is let go");
        panel.closed(&extended);
        assert!(
            watch::lock(&panel.shared).views.contains_key(&1),
            "the panel keeps what it wants"
        );
    }

    #[test]
    fn a_message_breaks_into_lines_that_fit() {
        let lines = wrap("Open a folder in a terminal on this computer", 100.0, 10.0);
        assert!(lines.len() > 2, "{lines:?}");
        assert!(lines
            .iter()
            .all(|line| line.chars().count() as f32 * 5.5 <= 100.0 + 0.01));
        assert!(lines.iter().all(|line| !line.starts_with(' ')));
        let chinese = wrap("在本机的终端里打开一个目录", 50.0, 10.0);
        assert_eq!(chinese[0], "在本机的终");
    }
}
