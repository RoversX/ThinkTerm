//! Dashboard: four tiles with a number, how it moved and a sparkline, a
//! chart of two series with its scale, and a table of hosts with bars --
//! all of it moving once a second, as the metrics thread steps the numbers.
//! It scrolls when the page is shorter than it.

use crate::chrome;
use crate::live::{lock, Live, Metrics, SharedLive, HISTORY};
use crate::ui::{self, Look, Rgn, GAP, PAD};
use crate::Section;
use thinkterm_plugin_sdk::panel::{Align, Hit, Item, Line, Rect, Scroll, Size, Text, Token};

/// The chart's ranges, in seconds.
const RANGES: [(&str, usize); 3] = [("30 s", 30), ("1 min", 60), ("90 s", HISTORY)];

#[derive(Debug, Clone)]
pub struct State {
    /// Which of `RANGES` the chart shows.
    pub range: usize,
}

impl Default for State {
    fn default() -> Self {
        Self { range: 2 }
    }
}

/// A click on `id`, past "dash:"; true when every view is to show it.
pub fn click(state: &mut State, id: &str, live: &SharedLive) -> bool {
    if let Some(range) = id
        .strip_prefix("range:")
        .and_then(|n| n.parse::<usize>().ok())
    {
        state.range = range.min(RANGES.len() - 1);
        return false;
    }
    if id == "pause" {
        let (live, wake) = &**live;
        let mut live = lock(live);
        live.paused = !live.paused;
        wake.notify_all();
        return true;
    }
    false
}

/// How `now` moved from `before`: the ground and colour of its pill, and
/// what it says. `lower_is_better` turns the colours round, for latency
/// and errors.
pub fn change(now: f32, before: f32, lower_is_better: bool) -> (Token, Token, String) {
    let moved = if before.abs() < f32::EPSILON {
        0.0
    } else {
        (now - before) / before * 100.0
    };
    let good = (moved >= 0.0) != lower_is_better;
    let arrow = if moved >= 0.0 { "\u{2191}" } else { "\u{2193}" };
    let said = format!("{arrow} {:.1}%", moved.abs());
    match good {
        true => (Token::PositiveBg, Token::Positive, said),
        false => (Token::NegativeBg, Token::Negative, said),
    }
}

/// The whole dashboard in `r`; `heading` for the section's heading, which
/// the panel's extended view goes without.
pub fn draw(
    items: &mut Vec<Item>,
    look: &Look,
    r: Rgn,
    state: &mut State,
    live: &Live,
    heading: bool,
) {
    let env = look.env;
    let metrics = &live.metrics;
    // The heading stays put above what scrolls, with whether it is live.
    let said = live_said(live);
    let pill_w = look.text_w(&said, Size::Small) + 44.0;
    let top = if heading {
        chrome::header(items, look, r, Section::Dashboard, pill_w + GAP)
    } else {
        r.y + PAD
    };
    let pill_h = look.control();
    let pill_y = if heading { r.y + PAD } else { top };
    let pill_x = r.right() - PAD - pill_w;
    live_button(items, pill_x, pill_y, pill_w, pill_h, live);
    let top = if heading { top } else { top + pill_h + GAP };

    let w = r.w - PAD * 2.0;
    let mut inside = Vec::new();
    let mut y = 0.0;
    y += tiles(&mut inside, look, PAD, y, w, metrics) + PAD;
    y += chart(&mut inside, look, PAD, y, w, state, metrics) + PAD;
    y += hosts(&mut inside, look, PAD, y, w, metrics) + PAD;
    let view_h = (r.bottom() - top).max(0.0);
    items.push(Scroll::new("dash", r.x, top, r.w, view_h, y, inside).into());
    let _ = env;
}

fn live_said(live: &Live) -> String {
    match live.paused {
        false => format!("Live \u{00b7} {}", live.metrics.tick),
        true => "Paused".to_string(),
    }
}

/// Whether the numbers move, and a press that stops or starts them.
fn live_button(items: &mut Vec<Item>, x: f32, y: f32, w: f32, h: f32, live: &Live) {
    let said = live_said(live);
    let (ground, color) = match live.paused {
        false => (Token::PositiveBg, Token::Positive),
        true => (Token::BgHover, Token::TextMuted),
    };
    items.push(Rect::new(x, y, w, h).fill(ground).radius(h / 2.0).into());
    items.push(
        Hit::new("dash:pause", x, y, w, h)
            .hover(Token::BgHover)
            .radius(h / 2.0)
            .into(),
    );
    ui::dot(items, x + 16.0, y + h / 2.0, 4.0, color);
    items.push(
        Text::new(x + 28.0, y, w - 36.0, h, said)
            .size(Size::Small)
            .bold()
            .color(color)
            .into(),
    );
}

/// The four tiles, as many to a row as fit. Says how tall they came out.
fn tiles(items: &mut Vec<Item>, look: &Look, x: f32, y: f32, w: f32, m: &Metrics) -> f32 {
    let env = look.env;
    let columns = if w >= 640.0 {
        4
    } else if w >= 300.0 {
        2
    } else {
        1
    };
    let tile_w = (w - GAP * (columns - 1) as f32) / columns as f32;
    let tile_h = 12.0 + env.small.line + env.title.line + 6.0 + 32.0 + 12.0;
    let tiles = [
        (
            "Requests / s",
            m.requests.clone(),
            ui::thousands(m.requests.last() as u64),
            false,
            Token::Accent,
        ),
        (
            "p95 latency",
            m.latency.clone(),
            format!("{:.0} ms", m.latency.last()),
            true,
            Token::Warning,
        ),
        (
            "Errors / s",
            m.errors.clone(),
            format!("{:.1}", m.errors.last()),
            true,
            Token::Negative,
        ),
        (
            "Sessions",
            m.sessions.clone(),
            ui::thousands(m.sessions.last() as u64),
            false,
            Token::Positive,
        ),
    ];
    for (n, (label, series, value, lower_is_better, color)) in tiles.into_iter().enumerate() {
        let (col, row) = (n % columns, n / columns);
        let tx = x + col as f32 * (tile_w + GAP);
        let ty = y + row as f32 * (tile_h + GAP);
        ui::card(items, Rgn::new(tx, ty, tile_w, tile_h));
        let inner = Rgn::new(tx + 12.0, ty + 12.0, tile_w - 24.0, tile_h - 24.0);
        ui::caption(items, look, inner.x, inner.y, inner.w, label);
        let vy = inner.y + env.small.line;
        items.push(
            Text::new(inner.x, vy, inner.w * 0.6, env.title.line, value)
                .size(Size::Title)
                .bold()
                .into(),
        );
        let (ground, tone, said) = change(series.last(), series.ago(10), lower_is_better);
        let pill_h = env.small.line + 2.0;
        let pill_w = look.text_w(&said, Size::Small) + 16.0;
        if pill_w < inner.w * 0.45 {
            let py = vy + (env.title.line - pill_h) / 2.0;
            ui::chip(
                items,
                look,
                inner.right() - pill_w,
                py,
                &said,
                ground,
                tone,
                pill_h,
            );
        }
        let values = series.tail(30);
        let (low, high) = ui::bounds(values.iter().copied());
        let spark = Rgn::new(inner.x, inner.bottom() - 32.0, inner.w, 32.0);
        ui::spark(items, &values, spark, low, high, color, Some(color), 1.5);
    }
    let rows = 4_usize.div_ceil(columns);
    rows as f32 * tile_h + (rows - 1) as f32 * GAP
}

/// Requests and errors over the range picked, each on its own scale, with
/// the requests' scale at the right and the seconds under it.
#[allow(clippy::too_many_arguments)]
fn chart(
    items: &mut Vec<Item>,
    look: &Look,
    x: f32,
    y: f32,
    w: f32,
    state: &State,
    m: &Metrics,
) -> f32 {
    let env = look.env;
    let narrow = w < 520.0;
    let h = if narrow { 220.0 } else { 260.0 };
    ui::card(items, Rgn::new(x, y, w, h));
    let inner = Rgn::new(x + PAD, y + 12.0, w - PAD * 2.0, h - 24.0);
    let title_w = if narrow { inner.w } else { inner.w * 0.4 };
    items.push(
        Text::new(inner.x, inner.y, title_w, env.body.line, "Throughput")
            .bold()
            .into(),
    );
    // The ranges, at the heading's right; under it where it is narrow.
    let labels: Vec<&str> = RANGES.iter().map(|(label, _)| *label).collect();
    let seg_w = 168.0_f32.min(inner.w);
    let (seg_x, seg_y) = match narrow {
        true => (inner.x, inner.y + env.body.line + GAP),
        false => (inner.right() - seg_w, inner.y - 2.0),
    };
    let seg_h = ui::segmented(
        items,
        look,
        "dash:range",
        seg_x,
        seg_y,
        seg_w,
        &labels,
        state.range,
    );
    let mut top = inner.y + env.body.line + GAP;
    if narrow {
        top = seg_y + seg_h + GAP;
    }
    // The legend.
    let mut lx = inner.x;
    for (said, color) in [("Requests", Token::Accent), ("Errors", Token::Negative)] {
        ui::dot(items, lx + 4.0, top + env.small.line / 2.0, 4.0, color);
        let lw = look.text_w(said, Size::Small) + 8.0;
        items.push(
            Text::new(lx + 12.0, top, lw, env.small.line, said)
                .size(Size::Small)
                .color(Token::TextMuted)
                .into(),
        );
        lx += 12.0 + lw + GAP * 2.0;
    }
    top += env.small.line + GAP;

    let scale_w = 44.0;
    let plot = Rgn::new(
        inner.x,
        top + 4.0,
        inner.w - scale_w - GAP,
        inner.bottom() - top - env.small.line - 10.0,
    );
    if plot.h < 30.0 || plot.w < 60.0 {
        return h;
    }
    let seconds = RANGES[state.range.min(RANGES.len() - 1)].1;
    let requests = m.requests.tail(seconds);
    let errors = m.errors.tail(seconds);
    let (low, high) = ui::bounds(requests.iter().copied());
    // Four levels across, faint, each with its number at the right.
    for level in 0..4 {
        let ly = plot.y + plot.h * level as f32 / 3.0;
        items.push(
            Rect::new(plot.x, ly, plot.w, 1.0)
                .fill(Token::Border)
                .into(),
        );
        let value = high - (high - low) * level as f32 / 3.0;
        items.push(
            Text::new(
                plot.right() + GAP,
                ly - env.small.line / 2.0,
                scale_w,
                env.small.line,
                ui::thousands(value as u64),
            )
            .size(Size::Small)
            .color(Token::TextFaint)
            .into(),
        );
    }
    let (_, error_high) = ui::bounds(errors.iter().copied());
    ui::spark(
        items,
        &errors,
        plot,
        0.0,
        error_high * 3.0,
        Token::Negative,
        Some(Token::NegativeBg),
        1.5,
    );
    ui::spark(
        items,
        &requests,
        plot,
        low,
        high,
        Token::Accent,
        Some(Token::Accent),
        2.0,
    );
    // Where the newest is, marked.
    if let Some(last) = requests.last() {
        let level = ((last - low) / (high - low).max(f32::EPSILON)).clamp(0.0, 1.0);
        let (lx, ly) = (plot.right(), plot.bottom() - level * plot.h);
        items.push(Line::new(vec![lx, plot.y, lx, plot.bottom()], Token::TextFaint).into());
        ui::dot(items, lx, ly, 5.0, Token::Bg);
        ui::dot(items, lx, ly, 3.5, Token::Accent);
    }
    let under = plot.bottom() + 6.0;
    let marks = if narrow { 3 } else { 4 };
    for n in 0..marks {
        let ago = seconds - seconds * n / (marks - 1);
        let mx = plot.x + plot.w * n as f32 / (marks - 1) as f32;
        let said = if ago == 0 {
            "now".to_string()
        } else {
            format!("\u{2212}{ago} s")
        };
        let (bx, align) = match n {
            0 => (mx, Align::Left),
            n if n == marks - 1 => (mx - 60.0, Align::Right),
            _ => (mx - 30.0, Align::Center),
        };
        items.push(
            Text::new(bx, under, 60.0, env.small.line, said)
                .size(Size::Small)
                .color(Token::TextFaint)
                .align(align)
                .into(),
        );
    }
    h
}

/// The hosts: whether each is well, its name and region, and how busy its
/// processors and memory are, as bars.
fn hosts(items: &mut Vec<Item>, look: &Look, x: f32, y: f32, w: f32, m: &Metrics) -> f32 {
    let env = look.env;
    let row = look.row();
    let head = env.body.line + GAP * 2.0;
    let h = 12.0 + head + row * m.hosts.len() as f32 + 8.0;
    ui::card(items, Rgn::new(x, y, w, h));
    let inner = Rgn::new(x + PAD, y + 12.0, w - PAD * 2.0, h - 20.0);
    items.push(
        Text::new(inner.x, inner.y, inner.w * 0.5, env.body.line, "Hosts")
            .bold()
            .into(),
    );
    let wide = inner.w >= 520.0;
    let name_w = if wide { inner.w * 0.22 } else { inner.w * 0.38 };
    let region_w = if wide { inner.w * 0.16 } else { 0.0 };
    let bars = inner.w - name_w - region_w;
    let bar_w = if wide { bars / 2.0 } else { bars };
    let headings: Vec<(&str, f32)> = match wide {
        true => vec![
            ("Host", 0.0),
            ("Region", name_w),
            ("CPU", name_w + region_w),
            ("Memory", name_w + region_w + bar_w),
        ],
        false => vec![("Host", 0.0), ("CPU", name_w)],
    };
    let hy = inner.y + env.body.line + 4.0;
    for (said, at) in headings {
        ui::caption(items, look, inner.x + at, hy, 80.0, said);
    }
    let mut ry = inner.y + head;
    for host in &m.hosts {
        ui::rule(items, inner.x, ry, inner.w);
        let well = match host.cpu {
            cpu if cpu > 0.85 => Token::Negative,
            cpu if cpu > 0.7 => Token::Warning,
            _ => Token::Positive,
        };
        ui::dot(items, inner.x + 4.0, ry + row / 2.0, 4.0, well);
        items.push(
            Text::new(inner.x + 16.0, ry, name_w - 20.0, row, host.name)
                .mono()
                .into(),
        );
        if wide {
            items.push(
                Text::new(inner.x + name_w, ry, region_w - 8.0, row, host.region)
                    .color(Token::TextMuted)
                    .into(),
            );
        }
        let meters: &[(f32, f32)] = match wide {
            true => &[
                (host.cpu, name_w + region_w),
                (host.memory, name_w + region_w + bar_w),
            ],
            false => &[(host.cpu, name_w)],
        };
        for (value, at) in meters {
            let mx = inner.x + at;
            let label_w = 40.0;
            let track = (bar_w - label_w - GAP * 2.0).max(10.0);
            let color = if *value > 0.85 {
                Token::Negative
            } else if *value > 0.7 {
                Token::Warning
            } else {
                Token::Accent
            };
            ui::progress(items, mx, ry + row / 2.0 - 3.0, track, 6.0, *value, color);
            items.push(
                Text::new(
                    mx + track + GAP,
                    ry,
                    label_w,
                    row,
                    format!("{:.0}%", value * 100.0),
                )
                .mono()
                .color(Token::TextMuted)
                .align(Align::Right)
                .into(),
            );
        }
        ry += row;
    }
    h
}
