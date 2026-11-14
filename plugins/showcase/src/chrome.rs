//! What frames every section: a rail of sections down the page's left on a
//! wide page, a bar with the section's name and arrows on a narrow one,
//! the section's own heading -- and the panel in the sidebar, and its
//! extended view.

use crate::live::Live;
use crate::sections::{board, dashboard};
use crate::ui::{self, Kind, Look, Rgn, GAP, PAD, RADIUS};
use crate::{Section, ViewState};
use thinkterm_plugin_sdk::panel::{Align, Hit, Item, Line, Rect, Size, Text, Token};

/// How wide the rail of sections is.
pub const RAIL_W: f32 = 212.0;

/// Whether the page has room for the rail beside a section.
pub fn wide(look: &Look) -> bool {
    look.env.width >= 760.0
}

/// The page's frame around its section; says where the section goes.
pub fn draw(items: &mut Vec<Item>, look: &Look, state: &ViewState) -> Rgn {
    let env = look.env;
    if wide(look) {
        rail(items, look, state.section);
        return Rgn::new(RAIL_W + 1.0, 0.0, env.width - RAIL_W - 1.0, env.height);
    }
    let bar = bar(items, look, state.section, state.menu);
    Rgn::new(0.0, bar + 1.0, env.width, env.height - bar - 1.0)
}

/// The rail: the tour's name, its sections, and a word on what draws them.
fn rail(items: &mut Vec<Item>, look: &Look, picked: Section) {
    let env = look.env;
    items.push(
        Rect::new(0.0, 0.0, RAIL_W, env.height)
            .fill(Token::BgRaised)
            .into(),
    );
    items.push(
        Rect::new(RAIL_W, 0.0, 1.0, env.height)
            .fill(Token::Border)
            .into(),
    );
    // The tour's mark: four squares, one of them lit.
    let (mx, my) = (PAD, PAD + 2.0);
    for (n, color) in [
        Token::Accent,
        Token::Positive,
        Token::Warning,
        Token::TextFaint,
    ]
    .into_iter()
    .enumerate()
    {
        let (col, row) = ((n % 2) as f32, (n / 2) as f32);
        items.push(
            Rect::new(mx + col * 9.0, my + row * 9.0, 7.0, 7.0)
                .fill(color)
                .radius(2.0)
                .into(),
        );
    }
    let tx = PAD + 28.0;
    let tw = RAIL_W - tx - PAD;
    items.push(
        Text::new(tx, PAD - 2.0, tw, env.title.line, "Showcase")
            .size(Size::Title)
            .bold()
            .into(),
    );
    ui::caption(
        items,
        look,
        tx,
        PAD - 2.0 + env.title.line,
        tw,
        "A plugin page tour",
    );
    let row = if look.touch {
        44.0
    } else {
        (env.body.line + 16.0).round()
    };
    let mut y = PAD + env.title.line + env.small.line + PAD;
    for section in Section::ALL {
        if y + row > env.height - PAD * 3.0 {
            break;
        }
        nav_row(
            items,
            look,
            section,
            section == picked,
            Rgn::new(GAP, y, RAIL_W - GAP * 2.0, row),
        );
        y += row + 2.0;
    }
    let foot = env.height - PAD - env.small.line * 2.0;
    if foot > y + GAP {
        let said = "Every pixel here is an item the plugin sent.";
        let foot_w = RAIL_W - PAD * 2.0;
        let h = env.small.line * 2.0;
        let mut text = Text::new(PAD, foot, foot_w, h, said)
            .size(Size::Small)
            .color(Token::TextFaint);
        if look.wraps() {
            text = text.wrap();
        }
        items.push(text.into());
    }
}

/// A section in the rail: its glyph and name, lit when picked.
fn nav_row(items: &mut Vec<Item>, look: &Look, section: Section, picked: bool, r: Rgn) {
    let id = format!("nav:{}", section.name());
    if picked {
        items.push(
            Rect::new(r.x, r.y, r.w, r.h)
                .fill(Token::BgSelected)
                .radius(RADIUS)
                .into(),
        );
        items.push(
            Rect::new(r.x, r.y + r.h / 2.0 - 8.0, 3.0, 16.0)
                .fill(Token::Accent)
                .radius(1.5)
                .into(),
        );
    } else {
        items.push(
            Hit::new(&id, r.x, r.y, r.w, r.h)
                .hover(Token::BgHover)
                .radius(RADIUS)
                .into(),
        );
    }
    let color = if picked {
        Token::Accent
    } else {
        Token::TextMuted
    };
    glyph(items, section, r.x + 12.0, r.y + (r.h - 16.0) / 2.0, color);
    let mut label = Text::new(r.x + 40.0, r.y, r.w - 48.0, r.h, section.title());
    if picked {
        label = label.bold();
    } else {
        label = label.color(Token::TextMuted);
    }
    items.push(label.into());
    let _ = look;
}

/// The narrow page's bar: the section before, the section's name, which
/// lists them all, and the section after. Says how tall it is.
fn bar(items: &mut Vec<Item>, look: &Look, section: Section, menu: bool) -> f32 {
    let env = look.env;
    let side = look.control();
    let h = side + GAP * 2.0;
    items.push(
        Rect::new(0.0, 0.0, env.width, h)
            .fill(Token::BgRaised)
            .into(),
    );
    items.push(Rect::new(0.0, h, env.width, 1.0).fill(Token::Border).into());
    let y = GAP;
    let arrow = |items: &mut Vec<Item>, id: &str, x: f32, points: Vec<f32>| {
        items.push(
            Hit::new(id, x, y, side, side)
                .hover(Token::BgHover)
                .radius(RADIUS)
                .into(),
        );
        items.push(Line::new(points, Token::TextMuted).width(2.0).into());
    };
    let (cx, cy) = (GAP + side / 2.0, y + side / 2.0);
    arrow(
        items,
        "prev",
        GAP,
        vec![cx + 3.0, cy - 6.0, cx - 3.0, cy, cx + 3.0, cy + 6.0],
    );
    let right = env.width - GAP - side;
    let cx = right + side / 2.0;
    arrow(
        items,
        "next",
        right,
        vec![cx - 3.0, cy - 6.0, cx + 3.0, cy, cx - 3.0, cy + 6.0],
    );
    // The middle: the section, and a chevron saying it opens a list.
    let mx = GAP * 2.0 + side;
    let mw = (right - GAP - mx).max(0.0);
    items.push(
        Hit::new("menu", mx, y, mw, side)
            .hover(Token::BgHover)
            .radius(RADIUS)
            .into(),
    );
    let label_w = look.text_w(section.title(), Size::Body) + 8.0;
    let all = 16.0 + GAP + label_w + 14.0;
    let start = mx + ((mw - all) / 2.0).max(0.0);
    glyph(items, section, start, cy - 8.0, Token::Accent);
    let lx = start + 16.0 + GAP;
    items.push(
        Text::new(lx, y, label_w.min(mx + mw - lx), side, section.title())
            .bold()
            .into(),
    );
    let vx = (lx + label_w + 2.0).min(mx + mw - 12.0);
    let chevron = match menu {
        false => vec![vx, cy - 2.0, vx + 4.0, cy + 2.0, vx + 8.0, cy - 2.0],
        true => vec![vx, cy + 2.0, vx + 4.0, cy - 2.0, vx + 8.0, cy + 2.0],
    };
    items.push(Line::new(chevron, Token::TextMuted).width(1.5).into());
    h
}

/// On a narrow page, the sections as a list in place of the one shown.
pub fn menu(items: &mut Vec<Item>, look: &Look, r: Rgn, picked: Section) {
    let env = look.env;
    let row = (env.body.line + env.small.line + 18.0).max(if look.touch { 56.0 } else { 0.0 });
    let mut y = r.y + GAP;
    for section in Section::ALL {
        if y + row > r.bottom() {
            break;
        }
        let x = GAP;
        let w = r.w - GAP * 2.0;
        let id = format!("nav:{}", section.name());
        if section == picked {
            items.push(
                Rect::new(x, y, w, row)
                    .fill(Token::BgSelected)
                    .radius(RADIUS)
                    .into(),
            );
        }
        items.push(
            Hit::new(&id, x, y, w, row)
                .hover(Token::BgHover)
                .radius(RADIUS)
                .into(),
        );
        let color = if section == picked {
            Token::Accent
        } else {
            Token::TextMuted
        };
        glyph(items, section, x + 12.0, y + (row - 16.0) / 2.0, color);
        let tx = x + 40.0;
        let tw = w - 48.0;
        let top = y + (row - env.body.line - env.small.line) / 2.0;
        items.push(
            Text::new(tx, top, tw, env.body.line, section.title())
                .bold()
                .into(),
        );
        ui::caption(items, look, tx, top + env.body.line, tw, section.blurb());
        y += row + 2.0;
    }
}

/// A section's heading: its name on a wide page, where the bar does not say
/// it already, and what it shows. `room` is kept free at the heading's
/// right for the section's own. Says where the section's body starts.
pub fn header(items: &mut Vec<Item>, look: &Look, r: Rgn, section: Section, room: f32) -> f32 {
    let env = look.env;
    let mut y = r.y + PAD;
    let w = (r.w - PAD * 2.0 - room).max(0.0);
    if wide(look) {
        items.push(
            Text::new(r.x + PAD, y, w, env.title.line, section.title())
                .size(Size::Title)
                .bold()
                .into(),
        );
        y += env.title.line + 2.0;
    }
    let most = if wide(look) { 1 } else { 2 };
    y += ui::paragraph(
        items,
        look,
        r.x + PAD,
        y,
        w,
        section.blurb(),
        Size::Small,
        Token::TextMuted,
        most,
    );
    y + PAD
}

/// A section's glyph, 16 units square, drawn with lines and rectangles.
pub fn glyph(items: &mut Vec<Item>, section: Section, x: f32, y: f32, color: Token) {
    let line = |items: &mut Vec<Item>, points: Vec<f32>| {
        let points = points
            .chunks(2)
            .flat_map(|p| [x + p[0], y + p[1]])
            .collect();
        items.push(Line::new(points, color).width(1.5).into());
    };
    let rect = |items: &mut Vec<Item>, rx: f32, ry: f32, w: f32, h: f32, filled: bool| {
        let shape = Rect::new(x + rx, y + ry, w, h).radius(2.0);
        items.push(match filled {
            true => shape.fill(color).into(),
            false => shape.border(color).into(),
        });
    };
    match section {
        Section::Dashboard => {
            items.push(
                Line::new(ui::arc(x + 8.0, y + 9.0, 6.5, -0.375, 0.75, 18), color)
                    .width(1.5)
                    .into(),
            );
            line(items, vec![8.0, 9.0, 11.5, 5.5]);
        }
        Section::Components => {
            rect(items, 1.0, 1.0, 6.0, 6.0, true);
            rect(items, 9.0, 1.0, 6.0, 6.0, false);
            rect(items, 1.0, 9.0, 6.0, 6.0, false);
            rect(items, 9.0, 9.0, 6.0, 6.0, false);
        }
        Section::Lists => {
            for row in 0..3 {
                let ry = 3.0 + row as f32 * 5.0;
                rect(items, 1.0, ry - 1.0, 2.0, 2.0, true);
                line(items, vec![6.0, ry, 15.0, ry]);
            }
        }
        Section::Forms => {
            rect(items, 1.0, 3.0, 14.0, 10.0, false);
            line(items, vec![4.0, 8.0, 9.0, 8.0]);
            line(items, vec![11.0, 5.5, 11.0, 10.5]);
        }
        Section::Motion => {
            let wave: Vec<f32> = (0..=12)
                .flat_map(|n| {
                    let px = 1.0 + n as f32 * (14.0 / 12.0);
                    [
                        px,
                        8.0 - (n as f32 / 12.0 * std::f32::consts::TAU).sin() * 5.0,
                    ]
                })
                .collect();
            line(items, wave);
        }
        Section::Board => {
            rect(items, 1.0, 1.0, 4.0, 14.0, false);
            rect(items, 6.0, 1.0, 4.0, 9.0, true);
            rect(items, 11.0, 1.0, 4.0, 11.0, false);
        }
        Section::Activity => {
            for n in 0..9 {
                let (col, row) = ((n % 3) as f32, (n / 3) as f32);
                rect(
                    items,
                    1.0 + col * 5.0,
                    1.0 + row * 5.0,
                    4.0,
                    4.0,
                    n % 4 == 0,
                );
            }
        }
        Section::Page => {
            rect(items, 1.0, 2.0, 14.0, 12.0, false);
            line(items, vec![1.5, 5.5, 14.5, 5.5]);
            line(items, vec![4.0, 9.0, 6.0, 10.5, 4.0, 12.0]);
        }
    }
}

/// The panel in the sidebar: a live number, a button that opens the tour,
/// and one for each of its sections.
pub fn panel(
    items: &mut Vec<Item>,
    look: &Look,
    state: &ViewState,
    live: &Live,
    board: &board::Board,
) {
    let env = look.env;
    let x = PAD;
    let w = (env.width - PAD * 2.0).max(0.0);
    let mut y = PAD - 4.0;
    items.push(
        Text::new(x, y, w, env.title.line, "Showcase")
            .size(Size::Title)
            .bold()
            .into(),
    );
    y += env.title.line;
    ui::caption(items, look, x, y, w, "What a plugin page can draw");
    y += env.small.line + GAP + 4.0;

    // The live number, as the dashboard has it.
    let metrics = &live.metrics;
    let card_h = env.small.line + env.title.line + 44.0 + GAP;
    ui::card(items, Rgn::new(x, y, w, card_h));
    let inner = Rgn::new(x + 12.0, y + 10.0, w - 24.0, card_h - 20.0);
    ui::caption(
        items,
        look,
        inner.x,
        inner.y,
        inner.w,
        "Requests per second \u{00b7} server-a",
    );
    let now = metrics.requests.last();
    let before = metrics.requests.ago(10);
    items.push(
        Text::new(
            inner.x,
            inner.y + env.small.line,
            inner.w * 0.6,
            env.title.line,
            ui::thousands(now as u64),
        )
        .size(Size::Title)
        .bold()
        .into(),
    );
    let (ground, color, said) = dashboard::change(now, before, false);
    let pill_h = env.small.line + 2.0;
    let pill_w = look.text_w(&said, Size::Small) + 16.0;
    let pill_y = inner.y + env.small.line + (env.title.line - pill_h) / 2.0;
    ui::chip(
        items,
        look,
        inner.right() - pill_w,
        pill_y,
        &said,
        ground,
        color,
        pill_h,
    );
    let values = metrics.requests.tail(40);
    let (low, high) = ui::bounds(values.iter().copied());
    let spark = Rgn::new(inner.x, inner.bottom() - 36.0, inner.w, 36.0);
    ui::spark(
        items,
        &values,
        spark,
        low,
        high,
        Token::Accent,
        Some(Token::Accent),
        1.5,
    );
    y += card_h + GAP + 4.0;

    let bw = w;
    ui::button(
        items,
        look,
        "open:",
        x,
        y,
        bw,
        "Open the tour",
        Kind::Accent,
    );
    y += look.control() + GAP;
    if env.can_extend {
        let label = if state.extend {
            "Hide the chart beside"
        } else {
            "Show the chart beside"
        };
        ui::button(items, look, "extend", x, y, bw, label, Kind::Plain);
        y += look.control() + GAP;
    }
    y += GAP;
    ui::caption(items, look, x, y, w, "Open the page on");
    y += env.small.line + 4.0;
    let row = if look.touch {
        44.0
    } else {
        (env.body.line + 12.0).round()
    };
    for section in Section::ALL {
        if y + row > env.height - GAP {
            break;
        }
        let id = format!("open:{}", section.name());
        items.push(
            Hit::new(&id, x - 6.0, y, w + 12.0, row)
                .hover(Token::BgHover)
                .radius(6.0)
                .into(),
        );
        glyph(items, section, x, y + (row - 16.0) / 2.0, Token::TextMuted);
        items.push(Text::new(x + 26.0, y, w - 60.0, row, section.title()).into());
        let note = match section {
            Section::Board => format!("{}", board.cards.len()),
            Section::Lists => "50k".to_string(),
            _ => String::new(),
        };
        if !note.is_empty() {
            items.push(
                Text::new(x + w - 60.0, y, 44.0, row, note)
                    .size(Size::Small)
                    .color(Token::TextFaint)
                    .align(Align::Right)
                    .into(),
            );
        }
        let (cx, cy) = (x + w - 6.0, y + row / 2.0);
        items.push(
            Line::new(
                vec![cx - 3.0, cy - 4.0, cx + 1.0, cy, cx - 3.0, cy + 4.0],
                Token::TextFaint,
            )
            .width(1.5)
            .into(),
        );
        y += row;
    }
}

/// The panel's extended view: the dashboard, with room left for the close
/// button ThinkTerm draws at its top left.
pub fn extended(items: &mut Vec<Item>, look: &Look, state: &mut ViewState, live: &Live) {
    let env = look.env;
    let top = env.close.map_or(0.0, |close| close.y + close.size);
    let r = Rgn::new(0.0, top, env.width, env.height - top);
    dashboard::draw(items, look, r, &mut state.dashboard, live, false);
}
