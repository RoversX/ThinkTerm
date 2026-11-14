//! Motion: a wave, an orbit, an equaliser and three loaders, drawn anew in
//! every frame. There is no animation in the client: while this plays, the
//! motion thread has the plugin draw so many frames a second, and each is
//! sent one at a time -- one drawn while the client has not taken in the
//! last replaces the one waiting. What it says it drew in the last second
//! is what the plugin managed; the client may have shown fewer.

use crate::chrome;
use crate::ui::{self, Kind, Look, Rgn, GAP, PAD};
use crate::Section;
use std::collections::VecDeque;
use std::f32::consts::TAU;
use std::time::{Duration, Instant};
use thinkterm_plugin_sdk::panel::{Align, Area, Item, Line, Rect, Size, Text, Token};

const SPEEDS: [(&str, f64); 3] = [
    ("\u{00bd}\u{00d7}", 0.5),
    ("1\u{00d7}", 1.0),
    ("2\u{00d7}", 2.0),
];
const RATES: [u32; 4] = [15, 30, 60, 120];
/// The most frame times kept, for counting those of the last second.
const KEPT: usize = 512;

#[derive(Debug)]
pub struct State {
    pub playing: bool,
    /// Which of `SPEEDS`, and of `RATES`.
    pub speed: usize,
    pub target: usize,
    /// How far the animation has got, in its own seconds: it stands still
    /// while paused, and runs at its speed while it plays.
    phase: f64,
    last: Option<Instant>,
    /// When the last frames were drawn, the newest last.
    drawn: VecDeque<Instant>,
    frames: u64,
}

impl Default for State {
    fn default() -> Self {
        Self {
            playing: true,
            speed: 1,
            target: 2,
            phase: 0.0,
            last: None,
            drawn: VecDeque::new(),
            frames: 0,
        }
    }
}

impl State {
    /// How many frames a second it asks to be drawn while it plays.
    pub fn rate(&self) -> u32 {
        RATES[self.target.min(RATES.len() - 1)]
    }

    /// How many frames were drawn in the second up to `now`.
    fn per_second(&mut self, now: Instant) -> usize {
        while self
            .drawn
            .front()
            .is_some_and(|at| now.duration_since(*at) > Duration::from_secs(1))
        {
            self.drawn.pop_front();
        }
        self.drawn.len()
    }
}

/// A click on `id`, past "mo:".
pub fn click(state: &mut State, id: &str) {
    if id == "play" {
        state.playing = !state.playing;
    } else if let Some(n) = id
        .strip_prefix("speed:")
        .and_then(|n| n.parse::<usize>().ok())
    {
        state.speed = n.min(SPEEDS.len() - 1);
    } else if let Some(n) = id
        .strip_prefix("rate:")
        .and_then(|n| n.parse::<usize>().ok())
    {
        state.target = n.min(RATES.len() - 1);
    }
}

pub fn draw(items: &mut Vec<Item>, look: &Look, r: Rgn, state: &mut State) {
    let env = look.env;
    let now = Instant::now();
    if state.playing {
        let since = state
            .last
            .map_or(Duration::ZERO, |last| now.duration_since(last));
        state.phase += since.min(Duration::from_millis(250)).as_secs_f64() * SPEEDS[state.speed].1;
        state.drawn.push_back(now);
        if state.drawn.len() > KEPT {
            state.drawn.pop_front();
        }
        state.frames += 1;
    }
    state.last = Some(now);
    let drawn = state.per_second(now);
    let t = state.phase as f32;

    let top = chrome::header(items, look, r, Section::Motion, 0.0);
    let x = r.x + PAD;
    let w = r.w - PAD * 2.0;
    let h = look.control();
    let mut y = top;

    // Play or pause, the speed, and how often to draw.
    let play_w = 104.0;
    play_button(items, look, x, y, play_w, state.playing);
    let narrow = w < 560.0;
    let seg_x = x + play_w + GAP;
    let speed_w = 132.0_f32.min(w - play_w - GAP);
    let speed_h = ui::segmented(
        items,
        look,
        "mo:speed",
        seg_x,
        y + (h - seg_h(look)) / 2.0,
        speed_w,
        &SPEEDS.map(|(said, _)| said),
        state.speed,
    );
    let rate_labels = RATES.map(|rate| rate.to_string());
    let rate_labels: Vec<&str> = rate_labels.iter().map(String::as_str).collect();
    let (rate_x, rate_y) = match narrow {
        true => (x, y + h + GAP),
        false => (seg_x + speed_w + GAP, y + (h - seg_h(look)) / 2.0),
    };
    let rate_w = 176.0_f32.min(w);
    ui::segmented(
        items,
        look,
        "mo:rate",
        rate_x,
        rate_y,
        rate_w,
        &rate_labels,
        state.target,
    );
    let caption_x = rate_x + rate_w + GAP;
    if r.right() - PAD - caption_x > 60.0 {
        let cy = rate_y + (speed_h - env.small.line * 2.0) / 2.0;
        ui::caption(
            items,
            look,
            caption_x,
            cy,
            r.right() - PAD - caption_x,
            "frames a second",
        );
        ui::caption(
            items,
            look,
            caption_x,
            cy + env.small.line,
            r.right() - PAD - caption_x,
            "asked of the plugin",
        );
    }
    y = match narrow {
        true => rate_y + seg_h(look) + GAP + 4.0,
        false => y + h + GAP + 4.0,
    };

    // What the plugin drew.
    let reached = match state.playing {
        true => format!("{drawn}"),
        false => "\u{2014}".to_string(),
    };
    let stats = [
        ("Drawn, last 1 s", reached),
        ("Asked for", format!("{} / s", state.rate())),
        ("Frames drawn", ui::thousands(state.frames)),
    ];
    let stat_h = env.small.line + env.title.line + 16.0;
    let stat_w = (w - GAP * 2.0) / 3.0;
    for (n, (label, value)) in stats.into_iter().enumerate() {
        let sx = x + n as f32 * (stat_w + GAP);
        ui::card(items, Rgn::new(sx, y, stat_w, stat_h));
        ui::caption(items, look, sx + 12.0, y + 8.0, stat_w - 24.0, label);
        let color = match n {
            0 if state.playing && drawn as f32 >= state.rate() as f32 * 0.9 => Token::Positive,
            0 if state.playing => Token::Warning,
            _ => Token::Text,
        };
        items.push(
            Text::new(
                sx + 12.0,
                y + 8.0 + env.small.line,
                stat_w - 24.0,
                env.title.line,
                value,
            )
            .size(Size::Title)
            .bold()
            .color(color)
            .into(),
        );
    }
    y += stat_h + GAP;

    // The stage: the wave across the top, the rest below it.
    let room = (r.bottom() - PAD - y).max(0.0);
    if room < 120.0 {
        return;
    }
    let wave_h = (room * if narrow { 0.32 } else { 0.42 }).clamp(80.0, 240.0);
    let wave = Rgn::new(x, y, w, wave_h);
    ui::card(items, wave);
    draw_wave(items, wave.inset(1.0), t);
    let below = Rgn::new(x, y + wave_h + GAP, w, room - wave_h - GAP);
    if below.h < 60.0 {
        return;
    }
    if narrow {
        let loaders_h = (env.small.line + 64.0).min(below.h * 0.4);
        let top_h = below.h - loaders_h - GAP;
        let half = (w - GAP) / 2.0;
        let orbit = Rgn::new(x, below.y, half, top_h);
        let bars = Rgn::new(x + half + GAP, below.y, half, top_h);
        let loaders = Rgn::new(x, below.y + top_h + GAP, w, loaders_h);
        for (card, part) in [(orbit, 0), (bars, 1), (loaders, 2)] {
            stage(items, look, card, part, t);
        }
    } else {
        let third = (w - GAP * 2.0) / 3.0;
        for part in 0..3 {
            let card = Rgn::new(x + part as f32 * (third + GAP), below.y, third, below.h);
            stage(items, look, card, part, t);
        }
    }
}

fn seg_h(look: &Look) -> f32 {
    if look.touch {
        40.0
    } else {
        look.env.body.line + 10.0
    }
}

/// Play or pause, its icon drawn: a triangle of two filled areas, or two
/// bars.
fn play_button(items: &mut Vec<Item>, look: &Look, x: f32, y: f32, w: f32, playing: bool) {
    let h = look.control();
    let label = if playing { "Pause" } else { "Play" };
    ui::button(items, look, "mo:play", x, y, w, "", Kind::Accent);
    let (ix, iy) = (x + 16.0, y + h / 2.0 - 6.0);
    if playing {
        items.push(
            Rect::new(ix, iy, 4.0, 12.0)
                .fill(Token::OnAccent)
                .radius(1.0)
                .into(),
        );
        items.push(
            Rect::new(ix + 7.0, iy, 4.0, 12.0)
                .fill(Token::OnAccent)
                .radius(1.0)
                .into(),
        );
    } else {
        let mid = iy + 6.0;
        items.push(Area::new(vec![ix, iy, ix + 11.0, mid], mid, Token::OnAccent).into());
        items.push(Area::new(vec![ix, iy + 12.0, ix + 11.0, mid], mid, Token::OnAccent).into());
    }
    items.push(
        Text::new(ix + 18.0, y, w - (ix + 18.0 - x) - 8.0, h, label)
            .bold()
            .color(Token::OnAccent)
            .into(),
    );
}

/// Two waves running across, the first with its fill fading under it.
fn draw_wave(items: &mut Vec<Item>, r: Rgn, t: f32) {
    let steps = ((r.w / 6.0) as usize).clamp(8, 400);
    let mid = r.y + r.h * 0.5;
    let amp = r.h * 0.32;
    let mut first = Vec::with_capacity(steps * 2 + 2);
    let mut second = Vec::with_capacity(steps * 2 + 2);
    for n in 0..=steps {
        let u = n as f32 / steps as f32;
        let px = r.x + u * r.w;
        let a = (u * TAU * 1.5 - t * 2.2).sin() * 0.62 + (u * TAU * 3.7 + t * 1.3).sin() * 0.24;
        let b = (u * TAU * 2.2 - t * 1.6 + 1.0).sin() * 0.5 + (u * TAU * 0.8 + t * 0.7).sin() * 0.3;
        // Quieter at the ends, so the waves start and end on the line.
        let ends = (u * std::f32::consts::PI).sin().max(0.0).powf(0.6);
        first.extend([px, mid - a * amp * ends]);
        second.extend([px, mid - b * amp * 0.8 * ends]);
    }
    items.push(
        Rect::new(r.x + 12.0, mid, r.w - 24.0, 1.0)
            .fill(Token::Border)
            .into(),
    );
    items.push(
        Area::new(first.clone(), r.bottom() - 8.0, Token::Accent)
            .fade()
            .into(),
    );
    items.push(Line::new(second, Token::Positive).width(1.5).into());
    items.push(Line::new(first, Token::Accent).width(2.5).into());
    // A scanner moving across, with a dot where it meets the first wave.
    let u = (t * 0.25).rem_euclid(1.0);
    let sx = r.x + u * r.w;
    items.push(
        Rect::new(sx, r.y + 8.0, 1.0, r.h - 16.0)
            .fill(Token::TextFaint)
            .into(),
    );
}

/// One card of the stage: the orbit (0), the equaliser (1), the loaders (2).
fn stage(items: &mut Vec<Item>, look: &Look, card: Rgn, part: usize, t: f32) {
    let env = look.env;
    ui::card(items, card);
    let title = ["Orbit", "Equaliser", "Loaders"][part];
    items.push(
        Text::new(
            card.x + 12.0,
            card.y + 8.0,
            card.w - 24.0,
            env.small.line,
            title,
        )
        .size(Size::Small)
        .bold()
        .color(Token::TextMuted)
        .into(),
    );
    let inner = Rgn::new(
        card.x + 12.0,
        card.y + 8.0 + env.small.line + 4.0,
        card.w - 24.0,
        card.h - env.small.line - 24.0,
    );
    if inner.w < 20.0 || inner.h < 20.0 {
        return;
    }
    match part {
        0 => orbit(items, inner, t),
        1 => bars(items, inner, t),
        _ => loaders(items, look, inner, t),
    }
}

/// Three planets round a sun, each with the arc it just went along.
fn orbit(items: &mut Vec<Item>, r: Rgn, t: f32) {
    let (cx, cy) = (r.x + r.w / 2.0, r.y + r.h / 2.0);
    let big = (r.w.min(r.h) / 2.0 - 8.0).max(8.0);
    ui::dot(items, cx, cy, (big * 0.16).clamp(4.0, 10.0), Token::Warning);
    for (n, (share, speed, color, size)) in [
        (0.42, 0.31, Token::Accent, 4.0_f32),
        (0.7, 0.19, Token::Positive, 5.0),
        (1.0, 0.11, Token::Negative, 6.0),
    ]
    .into_iter()
    .enumerate()
    {
        let radius = big * share;
        items.push(Line::new(ui::arc(cx, cy, radius, 0.0, 1.0, 64), Token::Border).into());
        let at = (t * speed + n as f32 * 0.33).rem_euclid(1.0);
        items.push(
            Line::new(
                ui::arc(cx, cy, radius, at - 0.12, 0.12, 16),
                Token::BgSelected,
            )
            .width(3.0)
            .into(),
        );
        items.push(
            Line::new(ui::arc(cx, cy, radius, at - 0.05, 0.05, 8), color)
                .width(2.0)
                .into(),
        );
        let a = at * TAU;
        ui::dot(
            items,
            cx + radius * a.sin(),
            cy - radius * a.cos(),
            size.min(big * 0.2),
            color,
        );
    }
}

/// Bars rising and falling, each to its own beat.
fn bars(items: &mut Vec<Item>, r: Rgn, t: f32) {
    let (bar, gap) = (6.0, 4.0);
    let count = (((r.w + gap) / (bar + gap)).floor() as usize).clamp(1, 64);
    let used = count as f32 * (bar + gap) - gap;
    let x0 = r.x + (r.w - used) / 2.0;
    let foot = r.bottom();
    items.push(Rect::new(r.x, foot, r.w, 1.0).fill(Token::Border).into());
    for n in 0..count {
        let i = n as f32;
        let level = 0.5 + 0.3 * (t * 3.1 + i * 0.55).sin() + 0.2 * (t * 1.7 + i * 1.3).sin();
        let level = level.clamp(0.06, 1.0);
        let h = (r.h - 4.0) * level;
        let color = if level > 0.85 {
            Token::Negative
        } else if level > 0.65 {
            Token::Warning
        } else {
            Token::Accent
        };
        let bx = x0 + i * (bar + gap);
        items.push(
            Rect::new(bx, foot - h, bar, h)
                .fill(color)
                .radius(2.0)
                .into(),
        );
    }
}

/// A spinner, three dots that bounce, a bar that runs and a ring that
/// fills: what waiting looks like.
fn loaders(items: &mut Vec<Item>, look: &Look, r: Rgn, t: f32) {
    let env = look.env;
    let wide = r.w >= 200.0 && r.h >= 120.0;
    let (cx, cy) = (r.x + 18.0, r.y + 18.0_f32.min(r.h / 2.0));
    let spin = 12.0_f32.min(r.h / 2.0 - 2.0).max(4.0);
    items.push(
        Line::new(ui::arc(cx, cy, spin, 0.0, 1.0, 36), Token::BgHover)
            .width(3.0)
            .into(),
    );
    let from = (t * 1.1).rem_euclid(1.0);
    let sweep = 0.18 + 0.12 * (t * 2.0).sin().abs();
    items.push(
        Line::new(ui::arc(cx, cy, spin, from, sweep, 14), Token::Accent)
            .width(3.0)
            .into(),
    );
    // The dots.
    let dx = cx + 34.0;
    for n in 0..3 {
        let bounce = ((t * 5.0 - n as f32 * 0.7).sin() * 0.5 + 0.5) * 6.0;
        let size = 3.5;
        let x = dx + n as f32 * 12.0;
        if x + size <= r.right() {
            ui::dot(items, x, cy + 3.0 - bounce, size, Token::TextMuted);
        }
    }
    // The ring that fills and empties.
    let ring_x = dx + 52.0;
    if ring_x + spin <= r.right() {
        let fill = (t * 0.4).rem_euclid(1.0);
        ui::ring(items, ring_x + spin, cy, spin, fill, Token::Positive);
    }
    // The bar, its run cut to its track.
    let by = if wide { r.y + 52.0 } else { r.bottom() - 6.0 };
    let track = r.w;
    items.push(
        Rect::new(r.x, by, track, 4.0)
            .fill(Token::BgHover)
            .radius(2.0)
            .into(),
    );
    let run = track * 0.3;
    let at = (t * 0.5).rem_euclid(1.0) * (track + run) - run;
    let (from, to) = (at.max(0.0), (at + run).min(track));
    if to - from > 1.0 {
        items.push(
            Rect::new(r.x + from, by, to - from, 4.0)
                .fill(Token::Accent)
                .radius(2.0)
                .into(),
        );
    }
    if wide && by + 16.0 + env.small.line * 2.0 <= r.bottom() {
        let said = "Each of these is drawn again in every frame: the plugin moves them, the client only shows them.";
        let lines = ((r.bottom() - by - 16.0) / env.small.line).floor() as usize;
        ui::paragraph(
            items,
            look,
            r.x,
            by + 16.0,
            r.w,
            said,
            Size::Small,
            Token::TextFaint,
            lines,
        );
    }
    let _ = Align::Left;
}
