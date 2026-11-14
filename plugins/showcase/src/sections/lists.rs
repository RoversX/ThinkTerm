//! Lists: three tabs. Events is a list of fifty thousand log lines, each
//! made up from its number when the player asks for its row, filtered by a
//! field above it and picked with a click or the keys, with what is picked
//! beside it. Wide table is a list of hosts whose rows are wider than the
//! page: they scroll sideways, but for the host's name at their left. Notes
//! is one long scroll area, with buttons that jump within it.

use crate::chrome;
use crate::ui::{self, Look, Rgn, GAP, PAD, RADIUS};
use crate::Section;
use thinkterm_plugin_sdk::panel::{
    Align, FieldText, Frame, Hit, Item, Jump, List, Rect, RowsWanted, Scroll, Size, Text, Token,
    Typed,
};

/// How many events there are.
pub const EVENTS: u32 = 50_000;
/// How many hosts the wide table has.
pub const HOSTS: u32 = 2_000;
const FILTER: &str = "list:filter";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Tab {
    #[default]
    Events,
    Table,
    Notes,
}

impl Tab {
    pub const ALL: [Self; 3] = [Self::Events, Self::Table, Self::Notes];

    pub fn name(self) -> &'static str {
        match self {
            Self::Events => "events",
            Self::Table => "table",
            Self::Notes => "notes",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|tab| tab.name() == name)
    }
}

#[derive(Debug, Default)]
pub struct State {
    pub tab: Tab,
    pub filter: FieldText,
    /// The events the filter leaves, in order, by number: `None` for all
    /// of them. Made again when the filter changes, and let go with the
    /// view; at most `EVENTS` of them.
    matches: Option<Vec<u32>>,
    /// The event picked, by number.
    pub picked: Option<u32>,
    /// How many times the keys moved the pick: the list keeps the last in
    /// view, once for each.
    reveal: u32,
    /// Counts the picks: the rows on show are drawn again for each.
    version: u32,
    /// The notes' jump: which heading, once for each seq.
    jump: Option<(usize, u32)>,
    /// How wide the events' rows are drawn, as the last frame laid them out.
    events_w: f32,
}

impl State {
    /// How many events the filter leaves.
    fn count(&self) -> u32 {
        self.matches.as_ref().map_or(EVENTS, |m| m.len() as u32)
    }

    /// The event shown at `row`.
    fn event_at(&self, row: u32) -> Option<u32> {
        match &self.matches {
            None => (row < EVENTS).then_some(row),
            Some(matches) => matches.get(row as usize).copied(),
        }
    }

    /// Where event `event` is shown, if the filter leaves it.
    fn row_of(&self, event: u32) -> Option<u32> {
        match &self.matches {
            None => (event < EVENTS).then_some(event),
            Some(matches) => matches.binary_search(&event).ok().map(|at| at as u32),
        }
    }

    fn key(&self) -> String {
        format!("events\n{}", self.filter.text().trim())
    }

    fn refilter(&mut self) {
        let wanted = self.filter.text().trim().to_lowercase();
        self.matches = (!wanted.is_empty()).then(|| {
            (0..EVENTS)
                .filter(|n| event(*n).haystack().contains(&wanted))
                .collect()
        });
    }
}

/// What is typed in the filter; Return (`submit`) picks the first event
/// it leaves.
pub fn typed(state: &mut State, typed: &Typed, submit: bool) {
    if typed.id != FILTER {
        return;
    }
    let before = state.filter.text().to_string();
    state.filter.heard(typed);
    if state.filter.text() != before {
        state.refilter();
    }
    if submit {
        if let Some(first) = state.event_at(0) {
            state.picked = Some(first);
            state.reveal += 1;
            state.version += 1;
        }
    }
}

/// A click on `id`, past "list:".
pub fn click(state: &mut State, id: &str) {
    if let Some(event) = id.strip_prefix("ev:").and_then(|n| n.parse().ok()) {
        state.picked = Some(event);
        state.version += 1;
    } else if let Some(tab) = id
        .strip_prefix("tab:")
        .and_then(|n| n.parse::<usize>().ok())
    {
        state.tab = Tab::ALL.get(tab).copied().unwrap_or_default();
    } else if let Some(heading) = id.strip_prefix("jump:").and_then(|n| n.parse().ok()) {
        let seq = state.jump.map_or(0, |(_, seq)| seq) + 1;
        state.jump = Some((heading, seq));
    }
}

/// A key the page takes, on the events: up and down pick the one before
/// or after, a page of them at a time, or the first or the last.
pub fn key(state: &mut State, key: &str) {
    if state.tab != Tab::Events {
        return;
    }
    let count = state.count();
    if count == 0 {
        return;
    }
    let at = state.picked.and_then(|picked| state.row_of(picked));
    let last = count - 1;
    let row = match (key, at) {
        ("ArrowDown", None) | ("Home", _) => 0,
        ("ArrowUp", None) | ("End", _) => last,
        ("ArrowDown", Some(at)) => (at + 1).min(last),
        ("ArrowUp", Some(at)) => at.saturating_sub(1),
        ("PageDown", at) => at.map_or(0, |at| (at + 12).min(last)),
        ("PageUp", at) => at.map_or(0, |at| at.saturating_sub(12)),
        _ => return,
    };
    state.picked = state.event_at(row);
    state.reveal += 1;
    state.version += 1;
}

pub fn draw(frame: &mut Frame, look: &Look, r: Rgn, state: &mut State) {
    let items = &mut frame.items;
    let top = chrome::header(items, look, r, Section::Lists, 0.0);
    let w = r.w - PAD * 2.0;
    let labels = [
        format!("Events \u{00b7} {}k", EVENTS / 1000),
        "Wide table".to_string(),
        "Scroll area".to_string(),
    ];
    let labels: Vec<&str> = labels.iter().map(String::as_str).collect();
    let picked = Tab::ALL
        .iter()
        .position(|tab| *tab == state.tab)
        .unwrap_or(0);
    let seg_w = w.min(420.0);
    let seg_h = ui::segmented(
        items,
        look,
        "list:tab",
        r.x + PAD,
        top,
        seg_w,
        &labels,
        picked,
    );
    let body = r.below(top + seg_h + GAP + 4.0);
    match state.tab {
        Tab::Events => events(frame, look, body, state),
        Tab::Table => table(frame, look, body),
        Tab::Notes => notes(frame, look, body, state),
    }
}

/// The filter, the events and, beside or below them, the one picked.
fn events(frame: &mut Frame, look: &Look, r: Rgn, state: &mut State) {
    let env = look.env;
    let items = &mut frame.items;
    let x = r.x + PAD;
    let w = r.w - PAD * 2.0;
    let h = look.control();
    let mut y = r.y;
    let count = state.count();
    let said = match &state.matches {
        None => format!("{} events", ui::thousands(u64::from(EVENTS))),
        Some(_) => format!(
            "{} of {}",
            ui::thousands(u64::from(count)),
            ui::thousands(u64::from(EVENTS))
        ),
    };
    let count_w = if w >= 480.0 {
        look.text_w(&said, Size::Small) + GAP
    } else {
        0.0
    };
    if env.has(thinkterm_plugin_sdk::panel::feature::FIELDS) {
        let field = state.filter.field(FILTER, x, y, w - count_w, h);
        items.push(
            field
                .placeholder("Filter: try error, server-c or billing")
                .icon("search")
                .max(64)
                .clear()
                .into(),
        );
    }
    if count_w > 0.0 {
        items.push(
            Text::new(x + w - count_w, y, count_w, h, said)
                .size(Size::Small)
                .color(Token::TextMuted)
                .align(Align::Right)
                .into(),
        );
    }
    y += h + GAP;

    let side = w >= 720.0;
    let detail_w = if side { 280.0 } else { 0.0 };
    let detail_h = if side {
        0.0
    } else {
        env.body.line + env.small.line * 2.0 + 28.0
    };
    let show_detail_below = !side && r.bottom() - y > detail_h + look.row() * 4.0;
    let list_w = w - if side { detail_w + PAD } else { 0.0 };
    let list_h = (r.bottom()
        - y
        - PAD
        - if show_detail_below {
            detail_h + GAP
        } else {
            0.0
        })
    .max(0.0);
    state.events_w = list_w;
    items.push(
        Rect::new(x, y, list_w, list_h)
            .border(Token::Border)
            .radius(RADIUS)
            .into(),
    );
    if count == 0 {
        items.push(
            Text::new(x, y, list_w, look.row() * 2.0, "Nothing matches the filter")
                .color(Token::TextMuted)
                .align(Align::Center)
                .into(),
        );
    }
    let mut list = List::new(
        "events",
        x + 1.0,
        y + 1.0,
        list_w - 2.0,
        (list_h - 2.0).max(0.0),
        count,
        look.row(),
        state.key(),
    )
    .version(state.version);
    // The event picked with the keys, kept in view.
    if let Some(row) = state.picked.and_then(|picked| state.row_of(picked)) {
        if state.reveal > 0 {
            let row = row as f32;
            list = list.top(Jump {
                to: row,
                seq: state.reveal,
                through: Some(row + 1.0),
            });
        }
    }
    items.push(list.into());

    let picked = state.picked.map(event);
    if side {
        let d = Rgn::new(x + list_w + PAD, y, detail_w, list_h);
        detail(items, look, d, picked.as_ref(), state);
    } else if show_detail_below {
        let d = Rgn::new(x, y + list_h + GAP, w, detail_h);
        ui::card(items, d);
        let inner = d.inset(12.0);
        match &picked {
            None => ui::caption(
                items,
                look,
                inner.x,
                inner.y,
                inner.w,
                "Pick an event: click one, or use \u{2191} \u{2193}",
            ),
            Some(event) => {
                let (ground, color) = event.level.tones();
                let bw = ui::chip(
                    items,
                    look,
                    inner.x,
                    inner.y,
                    event.level.name(),
                    ground,
                    color,
                    env.small.line + 2.0,
                );
                ui::caption(
                    items,
                    look,
                    inner.x + bw + GAP,
                    inner.y,
                    inner.w - bw - GAP,
                    format!(
                        "{} \u{00b7} {} \u{00b7} {}",
                        event.time, event.host, event.service
                    ),
                );
                let ty = inner.y + env.small.line + 6.0;
                items.push(
                    Text::new(inner.x, ty, inner.w, env.body.line, event.message.clone()).into(),
                );
                let row = state.row_of(event.index).map_or(String::new(), |row| {
                    format!(
                        "Row {} of {}",
                        ui::thousands(u64::from(row) + 1),
                        ui::thousands(u64::from(state.count()))
                    )
                });
                ui::caption(items, look, inner.x, ty + env.body.line, inner.w, row);
            }
        }
    }
}

/// The event picked, in a card of its own beside the list.
fn detail(items: &mut Vec<Item>, look: &Look, d: Rgn, picked: Option<&Event>, state: &State) {
    let env = look.env;
    ui::card(items, d);
    let inner = d.inset(PAD);
    let Some(event) = picked else {
        let said = "Pick an event: click one, or give the page the keyboard and use \u{2191} \u{2193}, Page Up, Page Down, Home and End.";
        ui::paragraph(
            items,
            look,
            inner.x,
            inner.y,
            inner.w,
            said,
            Size::Body,
            Token::TextMuted,
            6,
        );
        return;
    };
    let mut y = inner.y;
    let (ground, color) = event.level.tones();
    ui::chip(
        items,
        look,
        inner.x,
        y,
        event.level.name(),
        ground,
        color,
        env.small.line + 4.0,
    );
    y += env.small.line + 4.0 + GAP;
    y += ui::paragraph(
        items,
        look,
        inner.x,
        y,
        inner.w,
        &event.message,
        Size::Body,
        Token::Text,
        4,
    ) + GAP;
    for (label, value) in [
        ("Time", event.time.clone()),
        ("Host", event.host.to_string()),
        ("Service", event.service.to_string()),
        (
            "Trace",
            format!("{:016x}", ui::mix(u64::from(event.index) ^ 0x5eed)),
        ),
        ("Number", ui::thousands(u64::from(event.index) + 1)),
    ] {
        if y + env.small.line + env.mono.line > inner.bottom() {
            break;
        }
        ui::caption(items, look, inner.x, y, inner.w, label);
        y += env.small.line;
        items.push(
            Text::new(inner.x, y, inner.w, env.mono.line, value)
                .mono()
                .into(),
        );
        y += env.mono.line + 6.0;
    }
    if let Some(row) = state.row_of(event.index) {
        let said = format!(
            "Row {} of {}",
            ui::thousands(u64::from(row) + 1),
            ui::thousands(u64::from(state.count()))
        );
        let fy = inner.bottom() - env.small.line;
        if fy > y {
            ui::caption(items, look, inner.x, fy, inner.w, said);
        }
    }
}

/// Rows of a list of this view's, as the player asks for them.
pub fn rows(look: &Look, state: &mut State, wanted: &RowsWanted) -> Vec<Vec<Item>> {
    let (from, to) = (wanted.from, wanted.to);
    match wanted.list.as_str() {
        "events" if wanted.key == state.key() => (from..to.min(state.count()))
            .filter_map(|row| state.event_at(row))
            .map(|n| {
                event_row(
                    look,
                    &event(n),
                    state.picked == Some(n),
                    state.events_w - 2.0,
                )
            })
            .collect(),
        "table" => (from..to.min(HOSTS + 1))
            .map(|row| table_row(look, row))
            .collect(),
        _ => Vec::new(),
    }
}

/// One event: when, how bad, where, and what it says.
fn event_row(look: &Look, event: &Event, picked: bool, w: f32) -> Vec<Item> {
    let env = look.env;
    let row = look.row();
    let id = format!("list:ev:{}", event.index);
    let mut items: Vec<Item> = vec![Hit::new(&id, 0.0, 0.0, w, row).into()];
    items.push(
        Hit::new(&id, 4.0, 2.0, w - 8.0, row - 4.0)
            .hover(Token::BgHover)
            .radius(6.0)
            .into(),
    );
    if picked {
        items.push(
            Rect::new(4.0, 2.0, w - 8.0, row - 4.0)
                .fill(Token::BgSelected)
                .radius(6.0)
                .into(),
        );
    }
    let mut x = 12.0;
    if w >= 560.0 {
        let tw = env.mono.advance * 12.0 + GAP;
        items.push(
            Text::new(x, 0.0, tw, row, event.time.clone())
                .mono()
                .color(Token::TextFaint)
                .into(),
        );
        x += tw + GAP;
    }
    let (ground, color) = event.level.tones();
    let badge_h = env.small.line + 2.0;
    let bw = 50.0;
    items.push(
        Rect::new(x, (row - badge_h) / 2.0, bw, badge_h)
            .fill(ground)
            .radius(4.0)
            .into(),
    );
    items.push(
        Text::new(x, (row - badge_h) / 2.0, bw, badge_h, event.level.name())
            .size(Size::Small)
            .bold()
            .color(color)
            .align(Align::Center)
            .into(),
    );
    x += bw + GAP;
    let hw = env.mono.advance * 9.0;
    items.push(
        Text::new(x, 0.0, hw, row, event.host)
            .mono()
            .color(Token::TextMuted)
            .into(),
    );
    x += hw + GAP;
    if w >= 640.0 {
        let sw = 64.0;
        items.push(
            Text::new(x, 0.0, sw, row, event.service)
                .size(Size::Small)
                .color(Token::TextMuted)
                .into(),
        );
        x += sw + GAP;
    }
    items.push(Text::new(x, 0.0, (w - x - 12.0).max(0.0), row, event.message.clone()).into());
    items
}

/// The table's columns: what each says, how wide it is, and whether its
/// numbers sit at its right.
const COLUMNS: [(&str, f32, bool); 13] = [
    ("Host", 156.0, false),
    ("Region", 104.0, false),
    ("Status", 104.0, false),
    ("CPU", 72.0, true),
    ("Memory", 88.0, true),
    ("Disk", 72.0, true),
    ("Net in", 112.0, true),
    ("Net out", 112.0, true),
    ("Uptime", 96.0, true),
    ("Kernel", 136.0, false),
    ("Version", 88.0, false),
    ("Owner", 112.0, false),
    ("Last deploy", 136.0, false),
];

fn table_width() -> f32 {
    COLUMNS.iter().map(|(_, w, _)| w).sum::<f32>() + 24.0
}

/// The wide table, and a line on what it shows.
fn table(frame: &mut Frame, look: &Look, r: Rgn) {
    let env = look.env;
    let items = &mut frame.items;
    let x = r.x + PAD;
    let w = r.w - PAD * 2.0;
    let said = format!(
        "{} hosts, {} columns. The rows are wider than the page: scroll sideways with a trackpad or Shift and the wheel; the host column stays.",
        ui::thousands(u64::from(HOSTS)),
        COLUMNS.len()
    );
    let mut y = r.y;
    y += ui::paragraph(
        items,
        look,
        x,
        y,
        w,
        &said,
        Size::Small,
        Token::TextMuted,
        2,
    ) + GAP;
    let h = (r.bottom() - y - PAD).max(0.0);
    items.push(
        Rect::new(x, y, w, h)
            .border(Token::Border)
            .radius(RADIUS)
            .into(),
    );
    let list = List::new(
        "table",
        x + 1.0,
        y + 1.0,
        w - 2.0,
        (h - 2.0).max(0.0),
        HOSTS + 1,
        look.row(),
        "table",
    )
    .wide(table_width(), COLUMNS[0].1);
    items.push(list.into());
    let _ = env;
}

/// A row of the table: its headings at the top, a host below.
fn table_row(look: &Look, row: u32) -> Vec<Item> {
    let env = look.env;
    let h = look.row();
    let width = table_width();
    let mut items: Vec<Item> = Vec::new();
    if row == 0 {
        items.push(Rect::new(0.0, 0.0, width, h).fill(Token::BgRaised).into());
        items.push(
            Rect::new(0.0, h - 1.0, width, 1.0)
                .fill(Token::Border)
                .into(),
        );
        let mut x = 0.0;
        for (said, w, right) in COLUMNS {
            let align = if right { Align::Right } else { Align::Left };
            items.push(
                Text::new(x + 12.0, 0.0, w - 20.0, h, said)
                    .size(Size::Small)
                    .bold()
                    .color(Token::TextMuted)
                    .align(align)
                    .into(),
            );
            x += w;
        }
        return items;
    }
    let host = row - 1;
    let r = |salt: u64| ui::mix(u64::from(host) * 31 + salt);
    if host % 2 == 1 {
        items.push(Rect::new(0.0, 0.0, width, h).fill(Token::BgRaised).into());
    }
    let status = match r(1) % 20 {
        0 => ("Down", Token::Negative),
        1 | 2 => ("Degraded", Token::Warning),
        _ => ("Healthy", Token::Positive),
    };
    let regions = [
        "us-east", "us-west", "eu-west", "eu-north", "ap-south", "ap-east",
    ];
    let owners = ["platform", "storage", "search", "billing", "edge", "data"];
    let cpu = (r(2) % 100) as f32;
    let cells: [String; 13] = [
        format!("srv-{:04}", host + 1),
        regions[(r(3) % 6) as usize].to_string(),
        status.0.to_string(),
        format!("{cpu:.0}%"),
        format!("{:.1} GiB", (r(4) % 640) as f32 / 10.0),
        format!("{}%", r(5) % 100),
        format!("{:.1} MB/s", (r(6) % 9000) as f32 / 10.0),
        format!("{:.1} MB/s", (r(7) % 4000) as f32 / 10.0),
        format!("{}d {}h", r(8) % 400, r(9) % 24),
        format!("6.{}.{}-generic", r(10) % 12, r(11) % 40),
        format!("0.{}.{}", 3 + r(12) % 3, r(13) % 10),
        owners[(r(14) % 6) as usize].to_string(),
        format!(
            "2026-{:02}-{:02} {:02}:{:02}",
            1 + r(15) % 11,
            1 + r(16) % 28,
            r(17) % 24,
            r(18) % 60
        ),
    ];
    let mut x = 0.0;
    for (n, ((_, w, right), said)) in COLUMNS.iter().zip(cells).enumerate() {
        let align = if *right { Align::Right } else { Align::Left };
        let mut cell = Text::new(x + 12.0, 0.0, w - 20.0, h, said).align(align);
        match n {
            0 => {
                ui::dot(&mut items, x + 10.0, h / 2.0, 3.5, status.1);
                cell = Text::new(x + 20.0, 0.0, w - 28.0, h, cell.text).mono();
            }
            2 => cell = cell.color(status.1),
            3 if cpu > 85.0 => cell = cell.mono().color(Token::Negative),
            3..=8 => cell = cell.mono(),
            _ => cell = cell.color(Token::TextMuted),
        }
        items.push(cell.into());
        x += w;
    }
    // The fixed column's right edge, a line the rest slides under.
    items.push(
        Rect::new(COLUMNS[0].1 - 1.0, 0.0, 1.0, h)
            .fill(Token::Border)
            .into(),
    );
    let _ = env;
    items
}

const NOTES: [(&str, [&str; 3], &str); 4] = [
    (
        "Drawing",
        [
            "A frame is a flat list of items, painted in order, in the view's own units. There are rectangles, text, lines, filled areas, hits, scroll areas, lists and fields; nothing else.",
            "Colours are ThinkTerm's own, by name, so a page follows the theme it is shown in without being told of it. A fixed colour can be written in hex where a plugin needs one.",
            "Text is one line, cut to its box, unless it asks to wrap: then the client breaks it into lines in the font it shows it in, and the plugin never measures text.",
        ],
        "frame.push(Rect::new(x, y, w, h).fill(Token::BgRaised).radius(8.0));",
    ),
    (
        "Scrolling",
        [
            "This whole tab is one scroll area: its items are in its own units, as tall as they come, and the player scrolls it without asking the plugin.",
            "The buttons above jump within it. A jump is sent with a seq, and taken once for each new one, so a frame drawn again does not pull the area back.",
            "A list is the other way to scroll: its rows are asked for as they come near the view, so ten thousand of them cost the few dozen that show.",
        ],
        "Scroll::new(\"notes\", x, y, w, h, height, items).top(Jump { to, seq, through: None })",
    ),
    (
        "Typing",
        [
            "A field is the one item the client draws itself: the box, the text, the caret and what is selected, in its own font, with input methods and all.",
            "The plugin hears what the field holds as it changes, and again when Return is pressed. What it puts in the field is taken once for each new seq.",
            "A secret field is sent only when it is submitted, and nothing can be copied out of it.",
        ],
        "FieldText::field(\"title\", x, y, w, h).placeholder(\"Summary\").max(80).clear()",
    ),
    (
        "Pages",
        [
            "A page is a view in a pane's place: a tab of its own, one of the tabs over a pane, or a split beside a terminal. ThinkTerm keeps it as it keeps a terminal.",
            "What the page is opened with is the plugin's to choose, and to change: this one keeps the section and the tab it is on, and comes back on them after a restart.",
            "The page knows the terminal it was opened from, and can read and type in it with the command line any program has.",
        ],
        "frame.change_page(PageChange { seq, title, icon, args: Some(json!({\"section\": \"lists\"})) })",
    ),
];

/// The notes: one long scroll area, and buttons that jump to its headings.
fn notes(frame: &mut Frame, look: &Look, r: Rgn, state: &mut State) {
    let env = look.env;
    let items = &mut frame.items;
    let x = r.x + PAD;
    let w = r.w - PAD * 2.0;
    // The jumps.
    let mut flow = ui::Flow::new(x, r.y, w);
    let h = look.control();
    for (n, (heading, ..)) in NOTES.iter().enumerate() {
        let bw = ui::button_w(look, heading);
        let (bx, by) = flow.place(bw, h);
        ui::button(
            items,
            look,
            &format!("list:jump:{n}"),
            bx,
            by,
            bw,
            heading,
            ui::Kind::Plain,
        );
    }
    let top = flow.bottom() + GAP;
    // The document, laid out once to know where each heading is.
    let text_w = (w - PAD * 2.0).min(680.0);
    let mut inside = Vec::new();
    let mut anchors = Vec::new();
    let mut y = PAD;
    for (n, (heading, paragraphs, code)) in NOTES.iter().enumerate() {
        anchors.push((y - GAP).max(0.0));
        let tx = PAD;
        inside.push(
            Text::new(
                tx,
                y,
                text_w,
                env.title.line,
                format!("{}. {heading}", n + 1),
            )
            .size(Size::Title)
            .bold()
            .into(),
        );
        y += env.title.line + GAP;
        for said in paragraphs {
            y += ui::paragraph(
                &mut inside,
                look,
                tx,
                y,
                text_w,
                said,
                Size::Body,
                Token::Text,
                12,
            ) + GAP
                + 4.0;
        }
        let code_h = env.mono.line + 16.0;
        inside.push(
            Rect::new(tx, y, text_w, code_h)
                .fill(Token::BgRaised)
                .border(Token::Border)
                .radius(6.0)
                .into(),
        );
        inside.push(
            Text::new(tx + 12.0, y, text_w - 24.0, code_h, *code)
                .mono()
                .color(Token::TextMuted)
                .into(),
        );
        y += code_h + PAD * 2.0;
    }
    inside.push(
        Text::new(
            PAD,
            y,
            text_w,
            env.small.line,
            "The end. The jumps above go back up.",
        )
        .size(Size::Small)
        .color(Token::TextFaint)
        .into(),
    );
    y += env.small.line + PAD;
    let view_h = (r.bottom() - top - PAD).max(0.0);
    items.push(
        Rect::new(x, top, w, view_h)
            .border(Token::Border)
            .radius(RADIUS)
            .into(),
    );
    let mut scroll = Scroll::new(
        "notes",
        x + 1.0,
        top + 1.0,
        w - 2.0,
        (view_h - 2.0).max(0.0),
        y,
        inside,
    );
    if let Some((heading, seq)) = state.jump {
        let to = anchors.get(heading).copied().unwrap_or(0.0);
        scroll = scroll.top(Jump {
            to,
            seq,
            through: None,
        });
    }
    items.push(scroll.into());
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Info,
    Warn,
    Error,
}

impl Level {
    fn name(self) -> &'static str {
        match self {
            Self::Info => "INFO",
            Self::Warn => "WARN",
            Self::Error => "ERROR",
        }
    }

    fn tones(self) -> (Token, Token) {
        match self {
            Self::Info => (Token::BgHover, Token::TextMuted),
            Self::Warn => (Token::BgSelected, Token::Warning),
            Self::Error => (Token::NegativeBg, Token::Negative),
        }
    }
}

/// One made-up log line.
#[derive(Debug, Clone)]
pub struct Event {
    pub index: u32,
    pub time: String,
    pub level: Level,
    pub host: &'static str,
    pub service: &'static str,
    pub message: String,
}

impl Event {
    /// What the filter looks in.
    fn haystack(&self) -> String {
        format!(
            "{} {} {} {}",
            self.level.name(),
            self.host,
            self.service,
            self.message
        )
        .to_lowercase()
    }
}

/// Event number `n`, the same each time it is asked for.
pub fn event(n: u32) -> Event {
    let r = ui::mix(u64::from(n) ^ 0xA5A5_5A5A);
    let level = match r % 100 {
        0..=3 => Level::Error,
        4..=15 => Level::Warn,
        _ => Level::Info,
    };
    let hosts = [
        "server-a", "server-b", "server-c", "server-d", "server-e", "server-f", "server-g",
        "server-h",
    ];
    let services = ["api", "auth", "billing", "search", "worker", "gateway"];
    let host = hosts[(r >> 8) as usize % hosts.len()];
    let service = services[(r >> 12) as usize % services.len()];
    let a = (r >> 20) % 900 + 3;
    let b = (r >> 32) % 9000 + 100;
    let message = match level {
        Level::Info => match (r >> 40) % 6 {
            0 => format!("GET /v1/items/{b} 200 in {a} ms"),
            1 => format!("POST /v1/orders 201 in {a} ms"),
            2 => format!("cache hit for user:{b}"),
            3 => format!("job {b} finished in {} s", a / 10),
            4 => format!("{a} connections open"),
            _ => format!("synced {b} rows from replica"),
        },
        Level::Warn => match (r >> 40) % 4 {
            0 => format!("slow query took {a} ms on orders_by_day"),
            1 => format!("retrying upstream, attempt {} of 3", a % 3 + 1),
            2 => format!("queue depth {b} above the soft limit"),
            _ => format!("certificate expires in {} days", a % 30 + 1),
        },
        Level::Error => match (r >> 40) % 4 {
            0 => "connection reset by peer".to_string(),
            1 => format!("POST /v1/upload 500 after {a} ms"),
            2 => format!("worker {b} exited with status 137"),
            _ => format!("disk full on /var/lib/data ({}% used)", 95 + a % 5),
        },
    };
    let ms = u64::from(n) * 173;
    let time = format!(
        "{:02}:{:02}:{:02}.{:03}",
        (9 + ms / 3_600_000) % 24,
        ms / 60_000 % 60,
        ms / 1000 % 60,
        ms % 1000
    );
    Event {
        index: n,
        time,
        level,
        host,
        service,
        message,
    }
}
