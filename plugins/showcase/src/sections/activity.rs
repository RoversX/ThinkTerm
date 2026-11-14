//! Activity: a year of made-up contributions as a heat map, a week to a
//! column and a day to a cell -- every cell a rectangle in one of four of
//! ThinkTerm's colours, and a hit. A day is picked with a click or the
//! arrow keys, and said in full below; under that, the year in numbers and
//! the days of the week as bars.

use crate::chrome;
use crate::ui::{self, Look, Rgn, GAP, PAD};
use crate::Section;
use thinkterm_plugin_sdk::panel::{Align, Frame, Hit, Rect, Scroll, Size, Text, Token};

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];
const DAYS: [&str; 7] = [
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
    "Sunday",
];
/// The grounds of a cell, from none to the most.
const LEVELS: [Token; 4] = [
    Token::BgHover,
    Token::PositiveBg,
    Token::PositiveBgStrong,
    Token::Positive,
];

#[derive(Debug, Default)]
pub struct State {
    /// The day picked, as days since 1970-01-01.
    pub picked: Option<i64>,
    /// The span the map shows, as the last frame drew it: its first day
    /// and today.
    span: Option<(i64, i64)>,
}

/// A click on `id`, past "act:".
pub fn click(state: &mut State, id: &str) {
    if let Some(day) = id.strip_prefix("day:").and_then(|n| n.parse().ok()) {
        state.picked = Some(day);
    }
}

/// Up and down are the day before and after, left and right the week.
pub fn key(state: &mut State, key: &str) {
    let Some((first, last)) = state.span else {
        return;
    };
    let by = match key {
        "ArrowUp" => -1,
        "ArrowDown" => 1,
        "ArrowLeft" => -7,
        "ArrowRight" => 7,
        _ => return,
    };
    let day = state.picked.map_or(last, |day| day + by);
    state.picked = Some(day.clamp(first, last));
}

/// How many contributions there were on `day`: more on weekdays, in waves
/// through the year, and none on some days.
pub fn count(day: i64) -> u32 {
    let r = ui::mix(day as u64 ^ 0xC0FFEE);
    let weekday = weekday(day);
    if r.is_multiple_of(7) || (weekday >= 5 && !r.is_multiple_of(3)) {
        return 0;
    }
    let season = ((day as f32 / 58.0).sin() * 0.5 + 0.5) * 9.0;
    ((r >> 8) % 7) as u32 + season as u32 + if weekday < 5 { 2 } else { 0 }
}

fn level(count: u32) -> usize {
    match count {
        0 => 0,
        1..=5 => 1,
        6..=10 => 2,
        _ => 3,
    }
}

/// Monday is 0: 1970-01-01 was a Thursday.
fn weekday(day: i64) -> usize {
    (day + 3).rem_euclid(7) as usize
}

/// The calendar date of a day counted from 1970-01-01 (Howard Hinnant's
/// `civil_from_days`).
fn civil(days: i64) -> (i64, usize, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let date = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    (
        yoe + era * 400 + i64::from(month <= 2),
        month as usize,
        date,
    )
}

fn today() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(20_400, |since| (since.as_secs() / 86_400) as i64)
}

pub fn draw(frame: &mut Frame, look: &Look, r: Rgn, state: &mut State) {
    draw_on(frame, look, r, state, today());
}

/// The map, as it is on `today`.
pub fn draw_on(frame: &mut Frame, look: &Look, r: Rgn, state: &mut State, today: i64) {
    let env = look.env;
    let items = &mut frame.items;
    let top = chrome::header(items, look, r, Section::Activity, 0.0);
    let w = r.w - PAD * 2.0;
    let mut inside = Vec::new();
    let x = PAD;
    let mut y = 0.0;

    // How big a cell is: as big as the year fits in, within reason; fewer
    // weeks where even the smallest do not fit.
    let label_w = 32.0;
    let gap = 3.0;
    let least = if look.touch { 14.0 } else { 10.0 };
    let cell = ((w - label_w) / 53.0 - gap).clamp(least, 18.0);
    let weeks = (((w - label_w + gap) / (cell + gap)).floor() as i64).clamp(4, 53);
    let first = today - weekday(today) as i64 - (weeks - 1) * 7;
    state.span = Some((first, today));
    let picked = state.picked.filter(|day| (first..=today).contains(day));

    // The months, over the column each starts in.
    let months_h = env.small.line + 4.0;
    let mut last_label = -10_i64;
    for week in 0..weeks {
        let monday = first + week * 7;
        let (_, month, date) = civil(monday);
        if date <= 7 && week - last_label >= 3 {
            let mx = x + label_w + week as f32 * (cell + gap);
            inside.push(
                Text::new(
                    mx,
                    y,
                    40.0_f32.min(x + w - mx),
                    env.small.line,
                    MONTHS[month - 1],
                )
                .size(Size::Small)
                .color(Token::TextMuted)
                .into(),
            );
            last_label = week;
        }
    }
    y += months_h;
    for (row, name) in ["Mon", "", "Wed", "", "Fri", "", ""].iter().enumerate() {
        if !name.is_empty() {
            let ry = y + row as f32 * (cell + gap) + (cell - env.small.line) / 2.0;
            inside.push(
                Text::new(x, ry, label_w - 6.0, env.small.line, *name)
                    .size(Size::Small)
                    .color(Token::TextMuted)
                    .into(),
            );
        }
    }
    let mut total = 0;
    let mut by_weekday = [0_u32; 7];
    let (mut streak, mut longest) = (0, 0);
    let mut busiest = (0, first);
    for week in 0..weeks {
        for row in 0..7 {
            let day = first + week * 7 + row;
            if day > today {
                continue;
            }
            let n = count(day);
            total += n;
            by_weekday[row as usize] += n;
            streak = if n > 0 { streak + 1 } else { 0 };
            longest = longest.max(streak);
            if n > busiest.0 {
                busiest = (n, day);
            }
            let cx = x + label_w + week as f32 * (cell + gap);
            let cy = y + row as f32 * (cell + gap);
            inside.push(
                Rect::new(cx, cy, cell, cell)
                    .fill(LEVELS[level(n)])
                    .radius(3.0)
                    .into(),
            );
            inside.push(
                Hit::new(format!("act:day:{day}"), cx, cy, cell, cell)
                    .hover(Token::BgSelected)
                    .radius(3.0)
                    .into(),
            );
            if picked == Some(day) {
                inside.push(
                    Rect::new(cx - 2.0, cy - 2.0, cell + 4.0, cell + 4.0)
                        .border(Token::Text)
                        .radius(4.0)
                        .into(),
                );
            }
        }
    }
    y += 7.0 * (cell + gap) + GAP;

    // The day picked, at the left, and what the colours say, at the right.
    let legend_w = LEVELS.len() as f32 * 14.0 + 80.0;
    let said = match picked {
        Some(day) => {
            let (year, month, date) = civil(day);
            let n = count(day);
            let plural = if n == 1 { "" } else { "s" };
            format!(
                "{}, {date} {} {year}: {n} contribution{plural}",
                DAYS[weekday(day)],
                MONTHS[month - 1]
            )
        }
        None => "Press a day, or use the arrow keys".to_string(),
    };
    let detail_w = if w - legend_w > 200.0 {
        w - legend_w - GAP
    } else {
        w
    };
    inside.push(
        Text::new(x, y, detail_w, env.body.line, said)
            .color(if picked.is_some() {
                Token::Text
            } else {
                Token::TextMuted
            })
            .into(),
    );
    let legend_y = if detail_w < w {
        y
    } else {
        y + env.body.line + 4.0
    };
    let lx = x + w - legend_w;
    inside.push(
        Text::new(lx, legend_y, 36.0, env.body.line, "Less")
            .size(Size::Small)
            .color(Token::TextFaint)
            .align(Align::Right)
            .into(),
    );
    for (n, ground) in LEVELS.iter().enumerate() {
        let sx = lx + 40.0 + n as f32 * 14.0;
        inside.push(
            Rect::new(sx, legend_y + (env.body.line - 10.0) / 2.0, 10.0, 10.0)
                .fill(*ground)
                .radius(2.0)
                .into(),
        );
    }
    inside.push(
        Text::new(
            lx + 40.0 + LEVELS.len() as f32 * 14.0 + 4.0,
            legend_y,
            36.0,
            env.body.line,
            "More",
        )
        .size(Size::Small)
        .color(Token::TextFaint)
        .into(),
    );
    y = legend_y + env.body.line + PAD;

    // The year in numbers.
    let (by, bm, bd) = civil(busiest.1);
    let stats = [
        (
            "Contributions",
            ui::thousands(u64::from(total)),
            format!("in {weeks} weeks"),
        ),
        (
            "Longest streak",
            format!("{longest} days"),
            "in a row with some".to_string(),
        ),
        (
            "Busiest day",
            format!("{}", busiest.0),
            format!("{bd} {} {by}", MONTHS[bm - 1]),
        ),
    ];
    let columns = if w >= 480.0 { 3 } else { 1 };
    let stat_w = (w - GAP * (columns - 1) as f32) / columns as f32;
    let stat_h = env.small.line * 2.0 + env.title.line + 20.0;
    for (n, (label, value, under)) in stats.into_iter().enumerate() {
        let (col, row) = (n % columns, n / columns);
        let sx = x + col as f32 * (stat_w + GAP);
        let sy = y + row as f32 * (stat_h + GAP);
        ui::card(&mut inside, Rgn::new(sx, sy, stat_w, stat_h));
        ui::caption(
            &mut inside,
            look,
            sx + 12.0,
            sy + 10.0,
            stat_w - 24.0,
            label,
        );
        inside.push(
            Text::new(
                sx + 12.0,
                sy + 10.0 + env.small.line,
                stat_w - 24.0,
                env.title.line,
                value,
            )
            .size(Size::Title)
            .bold()
            .into(),
        );
        ui::caption(
            &mut inside,
            look,
            sx + 12.0,
            sy + 10.0 + env.small.line + env.title.line,
            stat_w - 24.0,
            under,
        );
    }
    let rows = 3_usize.div_ceil(columns);
    y += rows as f32 * stat_h + (rows - 1) as f32 * GAP + PAD;

    // The days of the week, as bars.
    inside.push(
        Text::new(x, y, w, env.body.line, "By day of the week")
            .bold()
            .into(),
    );
    y += env.body.line + GAP;
    let most = by_weekday.iter().copied().max().unwrap_or(1).max(1) as f32;
    let name_w = 88.0;
    let bar_h = if look.touch { 22.0 } else { 16.0 };
    for (n, sum) in by_weekday.iter().enumerate() {
        inside.push(
            Text::new(x, y, name_w, bar_h, DAYS[n])
                .size(Size::Small)
                .color(Token::TextMuted)
                .into(),
        );
        let track = w - name_w - 56.0;
        let color = if n >= 5 {
            Token::TextFaint
        } else {
            Token::Positive
        };
        ui::progress(
            &mut inside,
            x + name_w,
            y + bar_h / 2.0 - 4.0,
            track,
            8.0,
            *sum as f32 / most,
            color,
        );
        inside.push(
            Text::new(x + w - 48.0, y, 48.0, bar_h, ui::thousands(u64::from(*sum)))
                .size(Size::Small)
                .align(Align::Right)
                .into(),
        );
        y += bar_h + 6.0;
    }
    y += PAD;
    let view_h = (r.bottom() - top).max(0.0);
    items.push(Scroll::new("activity", r.x, top, r.w, view_h, y, inside).into());
}
