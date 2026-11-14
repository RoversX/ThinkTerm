//! Components: a gallery of controls, each made of the few items a frame
//! has -- buttons, switches, checkboxes, a segmented control, tabs, chips
//! and badges, a slider, a stepper, progress, cards, what hovering and
//! cursors do, corners and borders, every colour ThinkTerm names, and its
//! text. Cards of them sit in one column or two, and scroll.

use crate::chrome;
use crate::ui::{self, Flow, Kind, Look, Rgn, GAP, PAD, RADIUS};
use crate::Section;
use thinkterm_plugin_sdk::panel::{
    Align, Click, Color, Cursor, Hit, Item, Line, Rect, Scroll, Size, Text, Token,
};

const TABS: [&str; 3] = ["Overview", "Activity", "Settings"];
const TAB_TEXT: [&str; 3] = [
    "A tab strip is three hits and a line under the one picked. What shows below it is whatever the plugin draws for that tab.",
    "Activity would list what happened. Here it says that the plugin drew this text again when the tab changed, and nothing else moved.",
    "Settings would hold switches like the ones above: each a rounded rectangle, a knob and a hit, redrawn when pressed.",
];
const CHIPS: [&str; 6] = ["rust", "wasm", "ui", "perf", "docs", "infra"];
const PERIODS: [&str; 4] = ["Day", "Week", "Month", "Year"];
const SWITCHES: [&str; 3] = ["Sync over Wi-Fi only", "Notify when done", "Compact rows"];
const CHECKS: [&str; 3] = [
    "Show hidden files",
    "Follow symbolic links",
    "Ask before deleting",
];

#[derive(Debug, Clone)]
pub struct State {
    pub switches: [bool; 3],
    pub checks: [bool; 3],
    pub period: usize,
    pub tab: usize,
    pub chips: [bool; 6],
    /// The slider, from 0 to 1.
    pub level: f32,
    pub count: i32,
    /// The button pressed last, and how many presses there have been.
    pub pressed: Option<(&'static str, u32)>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            switches: [true, false, true],
            checks: [false, true, true],
            period: 1,
            tab: 0,
            chips: [true, false, true, false, false, false],
            level: 0.62,
            count: 3,
            pressed: None,
        }
    }
}

/// A click on `id`, past "comp:".
pub fn click(state: &mut State, id: &str, click: &Click) {
    let (what, n) = match id.split_once(':') {
        Some((what, n)) => (what, n.parse::<usize>().ok()),
        None => (id, None),
    };
    match (what, n) {
        ("switch", Some(n)) if n < 3 => state.switches[n] = !state.switches[n],
        ("check", Some(n)) if n < 3 => state.checks[n] = !state.checks[n],
        ("period", Some(n)) if n < PERIODS.len() => state.period = n,
        ("tab", Some(n)) if n < TABS.len() => state.tab = n,
        ("chip", Some(n)) if n < CHIPS.len() => state.chips[n] = !state.chips[n],
        // The click says where along the track it landed.
        ("slider", Some(width)) if width > 0 => {
            state.level = (click.x / width as f32).clamp(0.0, 1.0);
        }
        ("minus", _) => state.count = (state.count - 1).max(0),
        ("plus", _) => state.count = (state.count + 1).min(99),
        // The disabled one has a hit too, to keep the arrow over it: a
        // press there counts for nothing.
        ("button", Some(n)) => {
            if let Some(name) = ["Plain", "Accent", "Danger"].get(n).copied() {
                let presses = state.pressed.map_or(0, |(_, presses)| presses) + 1;
                state.pressed = Some((name, presses));
            }
        }
        _ => {}
    }
}

/// The gallery in `r`.
pub fn draw(items: &mut Vec<Item>, look: &Look, r: Rgn, state: &mut State) {
    let top = chrome::header(items, look, r, Section::Components, 0.0);
    let w = r.w - PAD * 2.0;
    let columns = if w >= 720.0 { 2 } else { 1 };
    let col_w = (w - GAP * 2.0 * (columns - 1) as f32) / columns as f32;
    // Each card goes in the column that is shortest so far.
    let mut heights = vec![0.0_f32; columns];
    let mut inside = Vec::new();
    type Part = fn(&mut Vec<Item>, &Look, Rgn, &State) -> f32;
    let parts: [(&str, Part); 10] = [
        ("Buttons", buttons),
        ("Switches and checkboxes", switches),
        ("Segmented control and tabs", choices),
        ("Slider, stepper and progress", levels),
        ("Chips and badges", chips),
        ("Cards", cards),
        ("Hover tints and cursors", hovers),
        ("Corners and borders", shapes),
        ("Colours", colours),
        ("Text", typography),
    ];
    for (title, part) in parts {
        let col = heights
            .iter()
            .enumerate()
            .min_by(|a, b| a.1.total_cmp(b.1))
            .map_or(0, |(n, _)| n);
        let x = PAD + col as f32 * (col_w + GAP * 2.0);
        let y = heights[col];
        // Drawn once to learn how tall it is, then its card goes under it.
        let mut body = Vec::new();
        let inner = Rgn::new(x + PAD, y + PAD, col_w - PAD * 2.0, 0.0);
        let head = ui::card_title(&mut body, look, inner, title);
        let tall = part(
            &mut body,
            look,
            Rgn::new(inner.x, inner.y + head, inner.w, 0.0),
            state,
        );
        let card_h = PAD + head + tall + PAD;
        ui::card(&mut inside, Rgn::new(x, y, col_w, card_h));
        inside.append(&mut body);
        heights[col] = y + card_h + GAP * 2.0;
    }
    let height = heights.iter().copied().fold(0.0, f32::max);
    let view_h = (r.bottom() - top).max(0.0);
    items.push(Scroll::new("gallery", r.x, top, r.w, view_h, height, inside).into());
}

fn buttons(items: &mut Vec<Item>, look: &Look, r: Rgn, state: &State) -> f32 {
    let mut flow = Flow::new(r.x, r.y, r.w);
    let h = look.control();
    for (n, (label, kind)) in [
        ("Plain", Kind::Plain),
        ("Accent", Kind::Accent),
        ("Danger", Kind::Danger),
        ("Disabled", Kind::Disabled),
    ]
    .into_iter()
    .enumerate()
    {
        let w = ui::button_w(look, label);
        let (x, y) = flow.place(w, h);
        ui::button(
            items,
            look,
            &format!("comp:button:{n}"),
            x,
            y,
            w,
            label,
            kind,
        );
    }
    let mut y = flow.bottom() + GAP;
    let said = match state.pressed {
        None => "Press one: the plugin hears the click and draws the count.".to_string(),
        Some((name, presses)) => {
            format!("{name} was pressed last \u{00b7} {presses} presses in all")
        }
    };
    ui::caption(items, look, r.x, y, r.w, said);
    y += look.env.small.line;
    y - r.y
}

fn switches(items: &mut Vec<Item>, look: &Look, r: Rgn, state: &State) -> f32 {
    let env = look.env;
    let mut y = r.y;
    for (n, label) in SWITCHES.iter().enumerate() {
        let (tw, th): (f32, f32) = if look.touch {
            (48.0, 28.0)
        } else {
            (36.0, 20.0)
        };
        let row = th.max(env.body.line) + GAP;
        items.push(Text::new(r.x, y, r.w - tw - GAP, row, *label).into());
        ui::toggle(
            items,
            look,
            &format!("comp:switch:{n}"),
            r.right() - tw,
            y + (row - th) / 2.0,
            state.switches[n],
        );
        y += row;
    }
    y += GAP;
    ui::rule(items, r.x, y - GAP / 2.0, r.w);
    for (n, label) in CHECKS.iter().enumerate() {
        y += ui::checkbox(
            items,
            look,
            &format!("comp:check:{n}"),
            r.x + 4.0,
            y,
            r.w - 4.0,
            state.checks[n],
            label,
        );
    }
    y - r.y
}

fn choices(items: &mut Vec<Item>, look: &Look, r: Rgn, state: &State) -> f32 {
    let env = look.env;
    let mut y = r.y;
    let seg_w = r.w.min(320.0);
    y += ui::segmented(
        items,
        look,
        "comp:period",
        r.x,
        y,
        seg_w,
        &PERIODS,
        state.period,
    ) + GAP;
    ui::caption(
        items,
        look,
        r.x,
        y,
        r.w,
        format!("Showing this {}", PERIODS[state.period].to_lowercase()),
    );
    y += env.small.line + PAD;
    // Tabs: a label each, a line under all of them, and a thicker one under
    // the one picked.
    let tab_h = if look.touch {
        44.0
    } else {
        env.body.line + 14.0
    };
    let mut x = r.x;
    for (n, tab) in TABS.iter().enumerate() {
        let w = look.text_w(tab, Size::Body) + 20.0;
        if x + w > r.right() {
            break;
        }
        let picked = n == state.tab;
        if !picked {
            items.push(
                Hit::new(format!("comp:tab:{n}"), x, y, w, tab_h)
                    .hover(Token::BgHover)
                    .radius(6.0)
                    .into(),
            );
        }
        let mut label = Text::new(x, y, w, tab_h, *tab).align(Align::Center);
        label = match picked {
            true => label.bold(),
            false => label.color(Token::TextMuted),
        };
        items.push(label.into());
        if picked {
            items.push(
                Rect::new(x + 6.0, y + tab_h - 2.0, w - 12.0, 2.0)
                    .fill(Token::Accent)
                    .radius(1.0)
                    .into(),
            );
        }
        x += w + 4.0;
    }
    y += tab_h;
    ui::rule(items, r.x, y, r.w);
    y += GAP + 4.0;
    y += ui::paragraph(
        items,
        look,
        r.x,
        y,
        r.w,
        TAB_TEXT[state.tab],
        Size::Body,
        Token::TextMuted,
        4,
    );
    y - r.y
}

fn levels(items: &mut Vec<Item>, look: &Look, r: Rgn, state: &State) -> f32 {
    let env = look.env;
    let mut y = r.y;
    // The slider: a track, the part up to its knob in the accent, and a hit
    // over all of it whose click says where it landed.
    let ring_room = 64.0;
    let track_w = (r.w - ring_room - GAP).max(40.0);
    let slider_h = if look.touch { 44.0 } else { 28.0 };
    ui::caption(
        items,
        look,
        r.x,
        y,
        track_w,
        format!("Volume \u{00b7} {:.0}%", state.level * 100.0),
    );
    y += env.small.line + 4.0;
    let ty = y + slider_h / 2.0 - 2.0;
    items.push(
        Rect::new(r.x, ty, track_w, 4.0)
            .fill(Token::BgHover)
            .radius(2.0)
            .into(),
    );
    items.push(
        Rect::new(r.x, ty, track_w * state.level, 4.0)
            .fill(Token::Accent)
            .radius(2.0)
            .into(),
    );
    let kx = r.x + track_w * state.level;
    ui::dot(items, kx, ty + 2.0, 9.0, Token::Border);
    ui::dot(items, kx, ty + 2.0, 8.0, Token::Bg);
    ui::dot(items, kx, ty + 2.0, 4.0, Token::Accent);
    items.push(
        Hit::new(
            format!("comp:slider:{}", track_w.round() as u32),
            r.x,
            y,
            track_w.round(),
            slider_h,
        )
        .into(),
    );
    // The same level as a ring beside it.
    let (cx, cy) = (r.right() - ring_room / 2.0, y + 6.0);
    ui::ring(items, cx, cy, 22.0, state.level, Token::Accent);
    items.push(
        Text::new(
            cx - 22.0,
            cy - env.small.line / 2.0,
            44.0,
            env.small.line,
            format!("{:.0}", state.level * 100.0),
        )
        .size(Size::Small)
        .bold()
        .align(Align::Center)
        .into(),
    );
    y += slider_h + PAD;

    // The stepper: minus, the count, plus.
    let side = look.control();
    ui::caption(items, look, r.x, y, r.w, "Replicas");
    y += env.small.line + 4.0;
    items.push(
        Rect::new(r.x, y, side * 3.0, side)
            .border(Token::Border)
            .radius(RADIUS)
            .into(),
    );
    let minus = if state.count > 0 {
        Token::BgHover
    } else {
        Token::Bg
    };
    items.push(
        Hit::new("comp:minus", r.x, y, side, side)
            .hover(minus)
            .radius(RADIUS)
            .into(),
    );
    items.push(
        Hit::new("comp:plus", r.x + side * 2.0, y, side, side)
            .hover(Token::BgHover)
            .radius(RADIUS)
            .into(),
    );
    let (mx, my) = (r.x + side / 2.0, y + side / 2.0);
    let faint = if state.count > 0 {
        Token::Text
    } else {
        Token::TextFaint
    };
    items.push(
        Line::new(vec![mx - 5.0, my, mx + 5.0, my], faint)
            .width(1.5)
            .into(),
    );
    let px = r.x + side * 2.5;
    items.push(
        Line::new(vec![px - 5.0, my, px + 5.0, my], Token::Text)
            .width(1.5)
            .into(),
    );
    items.push(
        Line::new(vec![px, my - 5.0, px, my + 5.0], Token::Text)
            .width(1.5)
            .into(),
    );
    items.push(
        Rect::new(r.x + side, y + 6.0, 1.0, side - 12.0)
            .fill(Token::Border)
            .into(),
    );
    items.push(
        Rect::new(r.x + side * 2.0, y + 6.0, 1.0, side - 12.0)
            .fill(Token::Border)
            .into(),
    );
    items.push(
        Text::new(r.x + side, y, side, side, state.count.to_string())
            .mono()
            .align(Align::Center)
            .into(),
    );
    let note_x = r.x + side * 3.0 + PAD;
    items.push(
        Text::new(
            note_x,
            y,
            r.right() - note_x,
            side,
            format!("{} running", state.count),
        )
        .color(Token::TextMuted)
        .into(),
    );
    y += side + PAD;

    // Progress, in each of the colours that say how it goes.
    for (label, value, color) in [
        ("Uploading", 0.72, Token::Accent),
        ("Checks passed", 1.0, Token::Positive),
        ("Disk", 0.88, Token::Warning),
        ("Quota", 0.97, Token::Negative),
    ] {
        let label_w = 104.0_f32.min(r.w * 0.4);
        items.push(
            Text::new(r.x, y, label_w, env.small.line, label)
                .size(Size::Small)
                .color(Token::TextMuted)
                .into(),
        );
        let bar_w = r.w - label_w - 48.0;
        ui::progress(
            items,
            r.x + label_w,
            y + env.small.line / 2.0 - 3.0,
            bar_w,
            6.0,
            value,
            color,
        );
        items.push(
            Text::new(
                r.right() - 40.0,
                y,
                40.0,
                env.small.line,
                format!("{:.0}%", value * 100.0),
            )
            .size(Size::Small)
            .align(Align::Right)
            .into(),
        );
        y += env.small.line + GAP;
    }
    y - GAP - r.y
}

fn chips(items: &mut Vec<Item>, look: &Look, r: Rgn, state: &State) -> f32 {
    let env = look.env;
    let h = if look.touch {
        36.0
    } else {
        env.small.line + 10.0
    };
    ui::caption(items, look, r.x, r.y, r.w, "Filters: press to pick");
    let mut flow = Flow::new(r.x, r.y + env.small.line + 6.0, r.w);
    for (n, label) in CHIPS.iter().enumerate() {
        let on = state.chips[n];
        let said = if on {
            format!("\u{2713} {label}")
        } else {
            label.to_string()
        };
        let w = look.text_w(&said, Size::Small) + 24.0;
        let (x, y) = flow.place(w, h);
        let shape = Rect::new(x, y, w, h).radius(h / 2.0);
        items.push(match on {
            true => shape.fill(Token::Accent).into(),
            false => shape.border(Token::Border).into(),
        });
        let tint = if on {
            ui::ON_ACCENT_TINT
        } else {
            Token::BgHover.into()
        };
        items.push(
            Hit::new(format!("comp:chip:{n}"), x, y, w, h)
                .hover(tint)
                .radius(h / 2.0)
                .into(),
        );
        let color = if on {
            Token::OnAccent
        } else {
            Token::TextMuted
        };
        items.push(
            Text::new(x, y, w, h, said)
                .size(Size::Small)
                .color(color)
                .align(Align::Center)
                .into(),
        );
    }
    let mut y = flow.bottom() + PAD;
    ui::caption(items, look, r.x, y, r.w, "Badges");
    y += env.small.line + 6.0;
    let bh = env.small.line + 4.0;
    let mut flow = Flow::new(r.x, y, r.w);
    for (said, ground, color) in [
        ("New", Token::Accent, Token::OnAccent),
        ("Stable", Token::PositiveBg, Token::Positive),
        ("Beta", Token::BgSelected, Token::Warning),
        ("Deprecated", Token::NegativeBg, Token::Negative),
        ("v0.4.9", Token::BgHover, Token::TextMuted),
    ] {
        let w = look.text_w(said, Size::Small) + 16.0;
        let (x, y) = flow.place(w, bh);
        ui::chip(items, look, x, y, said, ground, color, bh);
    }
    // A count on a bell, as a notification badge sits.
    let (x, y) = flow.place(34.0, bh.max(26.0));
    items.push(
        Rect::new(x, y + 4.0, 22.0, 22.0)
            .border(Token::Border)
            .radius(6.0)
            .into(),
    );
    items.push(
        Line::new(
            ui::arc(x + 11.0, y + 16.0, 5.0, -0.25, 0.5, 10),
            Token::TextMuted,
        )
        .width(1.5)
        .into(),
    );
    ui::dot(items, x + 22.0, y + 5.0, 7.0, Token::Negative);
    items.push(
        Text::new(x + 15.0, y - 2.0, 14.0, 14.0, "3")
            .size(Size::Small)
            .bold()
            .color(Token::OnAccent)
            .align(Align::Center)
            .into(),
    );
    flow.bottom() - r.y
}

fn cards(items: &mut Vec<Item>, look: &Look, r: Rgn, _state: &State) -> f32 {
    let env = look.env;
    let people = [
        (
            "AK",
            "Alex Kim",
            "Reviewed the pull request and left two notes on the parser.",
            Token::Accent,
        ),
        (
            "SR",
            "Sam Rivera",
            "Deployed build 4821 to server-c. All checks passed.",
            Token::PositiveBg,
        ),
        (
            "JT",
            "Jordan Tan",
            "Opened an issue: the upload stalls past 2 GiB.",
            Token::NegativeBg,
        ),
    ];
    let columns = if r.w >= 520.0 { 3 } else { 1 };
    let cw = (r.w - GAP * (columns - 1) as f32) / columns as f32;
    let text_lines = if columns == 1 { 2 } else { 4 };
    let ch = 12.0 + 28.0 + GAP + env.body.line * text_lines as f32 + GAP + env.small.line + 12.0;
    for (n, (initials, name, said, ground)) in people.into_iter().enumerate() {
        let (col, row) = (n % columns, n / columns);
        let x = r.x + col as f32 * (cw + GAP);
        let y = r.y + row as f32 * (ch + GAP);
        items.push(
            Rect::new(x, y, cw, ch)
                .fill(Token::Bg)
                .border(Token::Border)
                .radius(RADIUS)
                .into(),
        );
        items.push(
            Hit::new(format!("comp:card:{n}"), x, y, cw, ch)
                .hover(Token::BgHover)
                .radius(RADIUS)
                .into(),
        );
        ui::avatar(items, x + 12.0, y + 12.0, 28.0, initials, ground);
        items.push(
            Text::new(x + 48.0, y + 12.0, cw - 60.0, 28.0, name)
                .bold()
                .into(),
        );
        let ty = y + 12.0 + 28.0 + GAP;
        let mut body = Text::new(
            x + 12.0,
            ty,
            cw - 24.0,
            env.body.line * text_lines as f32,
            said,
        )
        .color(Token::TextMuted);
        if look.wraps() {
            body = body.wrap();
        }
        items.push(body.into());
        ui::caption(
            items,
            look,
            x + 12.0,
            y + ch - 12.0 - env.small.line,
            cw - 24.0,
            format!("{} min ago", 4 + n * 7),
        );
    }
    let rows = 3_usize.div_ceil(columns);
    rows as f32 * ch + (rows - 1) as f32 * GAP
}

fn hovers(items: &mut Vec<Item>, look: &Look, r: Rgn, _state: &State) -> f32 {
    let env = look.env;
    let tiles = [
        (
            "bg-hover",
            "Theme hover",
            Some(Color::from(Token::BgHover)),
            Cursor::Pointer,
        ),
        (
            "accent",
            "Accent tint",
            Some(Color::rgba(90, 140, 255, 60)),
            Cursor::Pointer,
        ),
        (
            "arrow",
            "Arrow cursor",
            Some(Color::from(Token::BgSelected)),
            Cursor::Arrow,
        ),
        ("text", "Text cursor", None, Cursor::Text),
    ];
    let columns = if r.w >= 400.0 { 4 } else { 2 };
    let tw = (r.w - GAP * (columns - 1) as f32) / columns as f32;
    let th = env.body.line + env.small.line + 28.0;
    for (n, (id, label, tint, cursor)) in tiles.into_iter().enumerate() {
        let (col, row) = (n % columns, n / columns);
        let x = r.x + col as f32 * (tw + GAP);
        let y = r.y + row as f32 * (th + GAP);
        items.push(
            Rect::new(x, y, tw, th)
                .border(Token::Border)
                .radius(RADIUS)
                .into(),
        );
        let mut hit = Hit::new(format!("comp:hover:{id}"), x, y, tw, th)
            .radius(RADIUS)
            .cursor(cursor);
        if let Some(tint) = tint {
            hit = hit.hover(tint);
        }
        items.push(hit.into());
        items.push(
            Text::new(x, y + 12.0, tw, env.body.line, label)
                .align(Align::Center)
                .into(),
        );
        let note = match cursor {
            Cursor::Pointer => "pointer",
            Cursor::Arrow => "arrow",
            Cursor::Text => "text, no tint",
        };
        items.push(
            Text::new(x, y + 12.0 + env.body.line, tw, env.small.line, note)
                .size(Size::Small)
                .color(Token::TextFaint)
                .align(Align::Center)
                .into(),
        );
    }
    let rows = 4_usize.div_ceil(columns);
    let mut y = r.y + rows as f32 * th + (rows - 1) as f32 * GAP + GAP;
    let said = if look.touch {
        "A finger never hovers: these tint only under a mouse."
    } else {
        "The player tints and changes the cursor itself, without asking the plugin."
    };
    y += ui::paragraph(
        items,
        look,
        r.x,
        y,
        r.w,
        said,
        Size::Small,
        Token::TextMuted,
        2,
    );
    y - r.y
}

fn shapes(items: &mut Vec<Item>, look: &Look, r: Rgn, _state: &State) -> f32 {
    let side = 48.0;
    let mut flow = Flow::new(r.x, r.y, r.w);
    for (label, radius, fill, border) in [
        ("0", 0.0, Some(Token::BgSelected), None),
        ("4", 4.0, Some(Token::BgSelected), None),
        ("8", 8.0, Some(Token::Accent), None),
        ("16", 16.0, None, Some(Token::Accent)),
        (
            "pill",
            side / 2.0,
            Some(Token::PositiveBg),
            Some(Token::Positive),
        ),
        ("ring", side / 2.0, None, Some(Token::Warning)),
    ] {
        let w = if label == "pill" { side * 1.6 } else { side };
        let (x, y) = flow.place(w, side + look.env.small.line + 4.0);
        let mut shape = Rect::new(x, y, w, side).radius(radius);
        if let Some(fill) = fill {
            shape = shape.fill(fill);
        }
        if let Some(border) = border {
            shape = shape.border(border);
        }
        items.push(shape.into());
        items.push(
            Text::new(x, y + side + 4.0, w, look.env.small.line, label)
                .size(Size::Small)
                .color(Token::TextFaint)
                .align(Align::Center)
                .into(),
        );
    }
    flow.bottom() - r.y
}

fn colours(items: &mut Vec<Item>, look: &Look, r: Rgn, _state: &State) -> f32 {
    let env = look.env;
    let each = 168.0;
    let columns = ((r.w + GAP) / (each + GAP)).floor().max(1.0) as usize;
    let cw = (r.w - GAP * (columns - 1) as f32) / columns as f32;
    let row = 28.0;
    for (n, token) in Token::ALL.into_iter().enumerate() {
        let (col, line) = (n % columns, n / columns);
        let x = r.x + col as f32 * (cw + GAP);
        let y = r.y + line as f32 * (row + 4.0);
        items.push(
            Rect::new(x, y + 2.0, 24.0, 24.0)
                .fill(token)
                .border(Token::Border)
                .radius(6.0)
                .into(),
        );
        items.push(
            Text::new(x + 32.0, y, cw - 32.0, row, token.name())
                .mono()
                .into(),
        );
    }
    let lines = Token::ALL.len().div_ceil(columns);
    let mut y = r.y + lines as f32 * (row + 4.0) + GAP;
    let said = "Named colours follow the theme: switch it and every swatch here changes, with nothing redrawn.";
    y += ui::paragraph(
        items,
        look,
        r.x,
        y,
        r.w,
        said,
        Size::Small,
        Token::TextMuted,
        3,
    );
    let _ = env;
    y - r.y
}

fn typography(items: &mut Vec<Item>, look: &Look, r: Rgn, _state: &State) -> f32 {
    let env = look.env;
    let mut y = r.y;
    for (said, size, bold, mono) in [
        ("Title, bold", Size::Title, true, false),
        ("Title", Size::Title, false, false),
        ("Body, bold", Size::Body, true, false),
        (
            "Body: the quick brown fox jumps over the lazy dog",
            Size::Body,
            false,
            false,
        ),
        ("Small: captions and labels", Size::Small, false, false),
        (
            "Mono: fn main() { println!(\"hi\"); }",
            Size::Body,
            false,
            true,
        ),
    ] {
        let line = if mono { env.mono.line } else { look.line(size) };
        let mut text = Text::new(r.x, y, r.w, line, said).size(size);
        if bold {
            text = text.bold();
        }
        if mono {
            text = text.mono();
        }
        items.push(text.into());
        y += line + 4.0;
    }
    y += GAP;
    // Alignment, in boxes that show their edges.
    let bw = (r.w - GAP * 2.0) / 3.0;
    let bh = env.body.line + 12.0;
    for (n, (said, align)) in [
        ("Left", Align::Left),
        ("Center", Align::Center),
        ("Right", Align::Right),
    ]
    .into_iter()
    .enumerate()
    {
        let x = r.x + n as f32 * (bw + GAP);
        items.push(
            Rect::new(x, y, bw, bh)
                .border(Token::Border)
                .radius(4.0)
                .into(),
        );
        items.push(
            Text::new(x + 8.0, y, bw - 16.0, bh, said)
                .align(align)
                .into(),
        );
    }
    y += bh + GAP;
    let long = "A line too long for its box is cut with an ellipsis in the interface font";
    items.push(
        Rect::new(r.x, y, r.w, bh)
            .border(Token::Border)
            .radius(4.0)
            .into(),
    );
    items.push(Text::new(r.x + 8.0, y, r.w - 16.0, bh, long).into());
    y += bh + GAP;
    let mono = "and at the box's edge in the monospaced one, whose columns the plugin lines up";
    items.push(
        Rect::new(r.x, y, r.w, bh)
            .border(Token::Border)
            .radius(4.0)
            .into(),
    );
    items.push(Text::new(r.x + 8.0, y, r.w - 16.0, bh, mono).mono().into());
    y += bh + PAD;
    let wrapped = "Wrapped text is broken into lines that fit its box, after a space or between the characters of a script written without spaces: 插件画的每一个字都是一个文本项，换行由客户端完成。 A box a whole number of lines tall holds that many.";
    let h = ui::paragraph(
        items,
        look,
        r.x,
        y,
        r.w,
        wrapped,
        Size::Body,
        Token::Text,
        8,
    );
    y += h;
    y - r.y
}
