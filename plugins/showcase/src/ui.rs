//! The controls every section is built from. There are no widgets in a
//! frame: a button here is a rounded rectangle, a hit over it and its
//! label, and the same goes for a toggle, a checkbox or a chart. Each
//! function pushes its items and says how much room it took, on one
//! spacing grid: `PAD` at the edges, `GAP` between controls.

use thinkterm_plugin_sdk::panel::{
    feature, Align, Area, Color, Cursor, Env, Hit, Item, Line, Pointer, Rect, Size, Text, Token,
};

/// Room at a view's edges and inside a card.
pub const PAD: f32 = 16.0;
/// Between controls.
pub const GAP: f32 = 8.0;
/// A card's and a button's corners.
pub const RADIUS: f32 = 8.0;

/// A tint for a hit over the accent colour: the theme's own `bg-hover`
/// would show nothing there.
pub const ON_ACCENT_TINT: Color = Color::rgba(255, 255, 255, 36);

/// A box by its corner and size.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rgn {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rgn {
    pub fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self {
            x,
            y,
            w: w.max(0.0),
            h: h.max(0.0),
        }
    }

    pub fn right(&self) -> f32 {
        self.x + self.w
    }

    pub fn bottom(&self) -> f32 {
        self.y + self.h
    }

    pub fn inset(&self, by: f32) -> Self {
        Self::new(
            self.x + by,
            self.y + by,
            self.w - by * 2.0,
            self.h - by * 2.0,
        )
    }

    /// What is left below `y`.
    pub fn below(&self, y: f32) -> Self {
        Self::new(self.x, y, self.w, self.bottom() - y)
    }
}

/// How a view looks: its environment, and whether it is pressed with a
/// finger, which wants bigger targets and never hovers.
#[derive(Clone, Copy)]
pub struct Look<'a> {
    pub env: &'a Env,
    pub touch: bool,
}

impl<'a> Look<'a> {
    pub fn of(env: &'a Env) -> Self {
        Self {
            env,
            touch: env.pointer == Pointer::Touch,
        }
    }

    /// How tall a button or a field is.
    pub fn control(&self) -> f32 {
        if self.touch {
            44.0
        } else {
            (self.env.body.line + 14.0).round()
        }
    }

    /// How tall a row of a list is.
    pub fn row(&self) -> f32 {
        if self.touch {
            44.0
        } else {
            (self.env.body.line + 12.0).round()
        }
    }

    pub fn line(&self, size: Size) -> f32 {
        match size {
            Size::Small => self.env.small.line,
            Size::Body => self.env.body.line,
            Size::Title => self.env.title.line,
        }
    }

    pub fn wraps(&self) -> bool {
        self.env.has(feature::WRAP)
    }

    /// About how wide `text` is in the interface font at `size`: a little
    /// more than it is, so a box sized by it holds it. A character most
    /// scripts write in one column takes two in CJK.
    pub fn text_w(&self, text: &str, size: Size) -> f32 {
        let font = match size {
            Size::Small => self.env.small.size,
            Size::Body => self.env.body.size,
            Size::Title => self.env.title.size,
        };
        let columns: f32 = text.chars().map(|c| if wide(c) { 1.9 } else { 1.0 }).sum();
        (columns * font * 0.58).ceil()
    }

    /// How many lines `text` takes wrapped into `w` at `size`, about: a
    /// box this many lines tall holds it.
    pub fn lines_for(&self, text: &str, w: f32, size: Size) -> usize {
        if !self.wraps() {
            return 1;
        }
        let mut lines = 0;
        for paragraph in text.split('\n') {
            let width = self.text_w(paragraph, size);
            lines += ((width / w.max(1.0)) * 1.08).ceil().max(1.0) as usize;
        }
        lines.max(1)
    }
}

/// A character CJK writes in two columns.
fn wide(c: char) -> bool {
    matches!(c as u32, 0x1100..=0x115F | 0x2E80..=0xA4CF | 0xAC00..=0xD7A3 | 0xF900..=0xFAFF | 0xFE30..=0xFE4F | 0xFF00..=0xFF60 | 0xFFE0..=0xFFE6)
}

/// A small, muted line: what a control is called, a caption.
pub fn caption(
    items: &mut Vec<Item>,
    look: &Look,
    x: f32,
    y: f32,
    w: f32,
    said: impl Into<String>,
) {
    let line = look.env.small.line;
    items.push(
        Text::new(x, y, w.max(0.0), line, said)
            .size(Size::Small)
            .color(Token::TextMuted)
            .into(),
    );
}

/// Text wrapped into as many lines as it takes, at most `most`; one line
/// where the client does not wrap. Says how tall it came out.
#[allow(clippy::too_many_arguments)]
pub fn paragraph(
    items: &mut Vec<Item>,
    look: &Look,
    x: f32,
    y: f32,
    w: f32,
    said: &str,
    size: Size,
    color: Token,
    most: usize,
) -> f32 {
    let lines = look.lines_for(said, w, size).min(most.max(1));
    let h = lines as f32 * look.line(size);
    let mut item = Text::new(x, y, w.max(0.0), h, said).size(size).color(color);
    if look.wraps() {
        item = item.wrap();
    }
    items.push(item.into());
    h
}

/// A raised card with a border.
pub fn card(items: &mut Vec<Item>, r: Rgn) {
    items.push(
        Rect::new(r.x, r.y, r.w, r.h)
            .fill(Token::BgRaised)
            .border(Token::Border)
            .radius(RADIUS + 2.0)
            .into(),
    );
}

/// A card's heading, and the room under it.
pub fn card_title(items: &mut Vec<Item>, look: &Look, r: Rgn, title: &str) -> f32 {
    let h = look.env.body.line;
    items.push(Text::new(r.x, r.y, r.w, h, title).bold().into());
    h + GAP + 2.0
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Plain,
    Accent,
    Danger,
    Disabled,
}

/// How wide a button of `label` is.
pub fn button_w(look: &Look, label: &str) -> f32 {
    (look.text_w(label, Size::Body) + 28.0).max(if look.touch { 64.0 } else { 56.0 })
}

/// A button: its shape, a hit over it that tints it while hovered, and its
/// label on top. A disabled one takes no clicks, and keeps the arrow.
#[allow(clippy::too_many_arguments)]
pub fn button(
    items: &mut Vec<Item>,
    look: &Look,
    id: &str,
    x: f32,
    y: f32,
    w: f32,
    label: &str,
    kind: Kind,
) {
    let h = look.control();
    let shape = Rect::new(x, y, w, h).radius(RADIUS);
    let (shape, tint, color) = match kind {
        Kind::Plain => (
            shape.fill(Token::BgRaised).border(Token::Border),
            Some(Color::from(Token::BgHover)),
            Token::Text,
        ),
        Kind::Accent => (
            shape.fill(Token::Accent),
            Some(ON_ACCENT_TINT),
            Token::OnAccent,
        ),
        Kind::Danger => (
            shape.fill(Token::NegativeBg).border(Token::Negative),
            Some(Color::from(Token::NegativeBgStrong)),
            Token::Negative,
        ),
        Kind::Disabled => (shape.border(Token::Border), None, Token::TextFaint),
    };
    items.push(shape.into());
    match tint {
        Some(tint) => items.push(Hit::new(id, x, y, w, h).hover(tint).radius(RADIUS).into()),
        None => items.push(Hit::new(id, x, y, w, h).cursor(Cursor::Arrow).into()),
    }
    let mut label = Text::new(x, y, w, h, label)
        .color(color)
        .align(Align::Center);
    if kind == Kind::Accent {
        label = label.bold();
    }
    items.push(label.into());
}

/// A switch: a pill-shaped track and its knob, at the right when on.
pub fn toggle(
    items: &mut Vec<Item>,
    look: &Look,
    id: &str,
    x: f32,
    y: f32,
    on: bool,
) -> (f32, f32) {
    let (w, h) = if look.touch {
        (48.0, 28.0)
    } else {
        (36.0, 20.0)
    };
    let track = Rect::new(x, y, w, h).radius(h / 2.0);
    items.push(match on {
        true => track.fill(Token::Accent).into(),
        false => track.fill(Token::BgHover).border(Token::Border).into(),
    });
    let knob = h - 6.0;
    let knob_x = if on { x + w - knob - 3.0 } else { x + 3.0 };
    let knob = Rect::new(knob_x, y + 3.0, knob, knob).radius(knob / 2.0);
    items.push(match on {
        true => knob.fill(Token::OnAccent).into(),
        false => knob.fill(Token::TextMuted).into(),
    });
    items.push(
        Hit::new(id, x, y, w, h)
            .hover(Token::BgHover)
            .radius(h / 2.0)
            .into(),
    );
    (w, h)
}

/// A box ticked or not, with its label beside it: the whole row takes the
/// click.
#[allow(clippy::too_many_arguments)]
pub fn checkbox(
    items: &mut Vec<Item>,
    look: &Look,
    id: &str,
    x: f32,
    y: f32,
    w: f32,
    checked: bool,
    label: &str,
) -> f32 {
    let h = if look.touch {
        40.0
    } else {
        look.env.body.line + 8.0
    };
    items.push(
        Hit::new(id, x - 4.0, y, w + 4.0, h)
            .hover(Token::BgHover)
            .radius(6.0)
            .into(),
    );
    let side = 18.0;
    let (bx, by) = (x, y + (h - side) / 2.0);
    let shape = Rect::new(bx, by, side, side).radius(5.0);
    items.push(match checked {
        true => shape.fill(Token::Accent).into(),
        false => shape.border(Token::TextFaint).into(),
    });
    if checked {
        let tick = vec![bx + 4.5, by + 9.5, bx + 7.5, by + 12.5, bx + 13.5, by + 5.5];
        items.push(Line::new(tick, Token::OnAccent).width(2.0).into());
    }
    let tx = x + side + GAP;
    items.push(Text::new(tx, y, (w - side - GAP).max(0.0), h, label).into());
    h
}

/// Options side by side in one rounded box, the picked one raised.
#[allow(clippy::too_many_arguments)]
pub fn segmented(
    items: &mut Vec<Item>,
    look: &Look,
    prefix: &str,
    x: f32,
    y: f32,
    w: f32,
    options: &[&str],
    picked: usize,
) -> f32 {
    let h = if look.touch {
        40.0
    } else {
        look.env.body.line + 10.0
    };
    items.push(
        Rect::new(x, y, w, h)
            .fill(Token::BgHover)
            .radius(RADIUS)
            .into(),
    );
    let each = w / options.len().max(1) as f32;
    for (n, option) in options.iter().enumerate() {
        let sx = x + n as f32 * each;
        if n == picked {
            items.push(
                Rect::new(sx + 2.0, y + 2.0, each - 4.0, h - 4.0)
                    .fill(Token::Bg)
                    .border(Token::Border)
                    .radius(RADIUS - 2.0)
                    .into(),
            );
        } else {
            items.push(
                Hit::new(
                    format!("{prefix}:{n}"),
                    sx + 2.0,
                    y + 2.0,
                    each - 4.0,
                    h - 4.0,
                )
                .hover(Token::BgSelected)
                .radius(RADIUS - 2.0)
                .into(),
            );
        }
        let color = if n == picked {
            Token::Text
        } else {
            Token::TextMuted
        };
        let mut label = Text::new(sx, y, each, h, *option)
            .size(Size::Small)
            .color(color)
            .align(Align::Center);
        if n == picked {
            label = label.bold();
        }
        items.push(label.into());
    }
    h
}

/// A pill of text on a tinted ground; says how wide it came out.
#[allow(clippy::too_many_arguments)]
pub fn chip(
    items: &mut Vec<Item>,
    look: &Look,
    x: f32,
    y: f32,
    said: &str,
    ground: Token,
    color: Token,
    h: f32,
) -> f32 {
    let w = look.text_w(said, Size::Small) + 16.0;
    items.push(Rect::new(x, y, w, h).fill(ground).radius(h / 2.0).into());
    items.push(
        Text::new(x, y, w, h, said)
            .size(Size::Small)
            .color(color)
            .align(Align::Center)
            .into(),
    );
    w
}

/// A bar filled to `value`, from 0 to 1.
pub fn progress(items: &mut Vec<Item>, x: f32, y: f32, w: f32, h: f32, value: f32, color: Token) {
    items.push(
        Rect::new(x, y, w, h)
            .fill(Token::BgHover)
            .radius(h / 2.0)
            .into(),
    );
    let filled = (w * value.clamp(0.0, 1.0)).max(if value > 0.0 { h } else { 0.0 });
    if filled > 0.0 {
        items.push(
            Rect::new(x, y, filled, h)
                .fill(color)
                .radius(h / 2.0)
                .into(),
        );
    }
}

/// The points of an arc around (`cx`, `cy`), from `from` turning `turn`,
/// in turns (1.0 is all the way round), clockwise from the top.
pub fn arc(cx: f32, cy: f32, r: f32, from: f32, turn: f32, steps: usize) -> Vec<f32> {
    let steps = steps.max(2);
    (0..=steps)
        .flat_map(|n| {
            let a = (from + turn * n as f32 / steps as f32) * std::f32::consts::TAU;
            [cx + r * a.sin(), cy - r * a.cos()]
        })
        .collect()
}

/// A ring filled to `value`, from 0 to 1, from the top.
pub fn ring(items: &mut Vec<Item>, cx: f32, cy: f32, r: f32, value: f32, color: Token) {
    items.push(
        Line::new(arc(cx, cy, r, 0.0, 1.0, 72), Token::BgHover)
            .width(5.0)
            .into(),
    );
    let value = value.clamp(0.0, 1.0);
    if value > 0.0 {
        let steps = (72.0 * value).ceil() as usize;
        items.push(
            Line::new(arc(cx, cy, r, 0.0, value, steps), color)
                .width(5.0)
                .into(),
        );
    }
}

/// A round dot.
pub fn dot(items: &mut Vec<Item>, cx: f32, cy: f32, r: f32, color: impl Into<Color>) {
    items.push(
        Rect::new(cx - r, cy - r, r * 2.0, r * 2.0)
            .fill(color)
            .radius(r)
            .into(),
    );
}

/// Someone's initials on a round ground.
pub fn avatar(items: &mut Vec<Item>, x: f32, y: f32, side: f32, initials: &str, ground: Token) {
    items.push(
        Rect::new(x, y, side, side)
            .fill(ground)
            .radius(side / 2.0)
            .into(),
    );
    let color = if ground == Token::Accent {
        Token::OnAccent
    } else {
        Token::Text
    };
    items.push(
        Text::new(x, y, side, side, initials)
            .size(Size::Small)
            .bold()
            .color(color)
            .align(Align::Center)
            .into(),
    );
}

/// `values` as a line across the box, low at its foot, with the fill under
/// it fading out when `fill` says so.
#[allow(clippy::too_many_arguments)]
pub fn spark(
    items: &mut Vec<Item>,
    values: &[f32],
    r: Rgn,
    low: f32,
    high: f32,
    color: Token,
    fill: Option<Token>,
    width: f32,
) {
    if values.len() < 2 || r.w < 2.0 || r.h < 2.0 {
        return;
    }
    let span = (high - low).max(f32::EPSILON);
    let step = r.w / (values.len() - 1) as f32;
    let points: Vec<f32> = values
        .iter()
        .enumerate()
        .flat_map(|(n, value)| {
            let level = ((value - low) / span).clamp(0.0, 1.0);
            [r.x + n as f32 * step, r.bottom() - level * r.h]
        })
        .collect();
    if let Some(fill) = fill {
        items.push(Area::new(points.clone(), r.bottom(), fill).fade().into());
    }
    items.push(Line::new(points, color).width(width).into());
}

/// The lowest and highest of `values`, spread a little so a flat line
/// sits in the middle.
pub fn bounds(values: impl Iterator<Item = f32>) -> (f32, f32) {
    let (low, high) = values.fold((f32::MAX, f32::MIN), |(lo, hi), v| (lo.min(v), hi.max(v)));
    if low > high {
        return (0.0, 1.0);
    }
    let pad = ((high - low) * 0.12).max(high.abs() * 0.02).max(0.5);
    (low - pad, high + pad)
}

/// A thin rule across.
pub fn rule(items: &mut Vec<Item>, x: f32, y: f32, w: f32) {
    items.push(Rect::new(x, y, w, 1.0).fill(Token::Border).into());
}

/// A number with thousands set apart: 50,000.
pub fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (at, digit) in digits.chars().enumerate() {
        if at > 0 && (digits.len() - at).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

/// Text cut to `most` characters, with an ellipsis when it was cut.
pub fn cut(said: &str, most: usize) -> String {
    match said.char_indices().nth(most) {
        Some((at, _)) => format!("{}\u{2026}", &said[..at]),
        None => said.to_string(),
    }
}

/// Lays boxes of `w` out in rows across `room`, `GAP` apart: where the
/// next goes, starting a new row when it does not fit.
pub struct Flow {
    pub x0: f32,
    pub room: f32,
    pub x: f32,
    pub y: f32,
    pub row_h: f32,
}

impl Flow {
    pub fn new(x: f32, y: f32, room: f32) -> Self {
        Self {
            x0: x,
            room,
            x,
            y,
            row_h: 0.0,
        }
    }

    /// Where a box `w` wide and `h` tall goes.
    pub fn place(&mut self, w: f32, h: f32) -> (f32, f32) {
        if self.x > self.x0 && self.x + w > self.x0 + self.room {
            self.y += self.row_h + GAP;
            self.x = self.x0;
            self.row_h = 0.0;
        }
        let at = (self.x, self.y);
        self.x += w + GAP;
        self.row_h = self.row_h.max(h);
        at
    }

    /// Where the flow ends, below its last row.
    pub fn bottom(&self) -> f32 {
        self.y + self.row_h
    }
}

/// A tiny pseudo-random generator, so the sample data needs no crate:
/// SplitMix64, which is plenty for numbers to look at.
#[derive(Debug, Clone)]
pub struct Rng(pub u64);

impl Rng {
    pub fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        mix(self.0)
    }

    /// A number in [0, 1).
    pub fn unit(&mut self) -> f32 {
        (self.next() >> 40) as f32 / (1u64 << 24) as f32
    }
}

/// SplitMix64's mixing step: a different, well spread number for each.
pub fn mix(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}
