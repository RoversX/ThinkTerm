//! An example ThinkTerm plugin with a panel in the right sidebar: a
//! watchlist of quotes from Yahoo Finance, each with its day so far, and a
//! chart of the one picked over the range picked. The quotes are fetched on
//! a thread of the plugin's own while a panel is on show (`market`), and
//! the panel is drawn anew as they come.
//!
//! The watchlist is kept in the plugin's data directory, one symbol a
//! line, and changed with calls:
//! `thinkterm plugin call stocks '{"op":"add","symbol":"TSM"}'`, and
//! `"remove"` the same way. docs/thinkterm/plugins.md says how to install it.

mod market;
mod yahoo;

use anyhow::{bail, Context as _};
use market::{lock, Market, Shared, WATCHLIST_LIMIT};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex};
use thinkterm_plugin_sdk::panel::{
    Align, Area, Click, Env, Frame, Hit, Input, Item, Line, Rect, Scroll, Size, Text, Token,
};
use thinkterm_plugin_sdk::{Cx, Plugin, View};
use yahoo::{Chart, Day, Range};

/// Room at the panel's sides.
const PAD: f32 = 12.0;
/// Where the chart's prices go, to its right.
const SCALE_WIDTH: f32 = 56.0;

/// What one panel on show has picked.
#[derive(Clone, Copy)]
struct Pick {
    symbol: usize,
    range: Range,
}

impl Default for Pick {
    fn default() -> Self {
        Self {
            symbol: 0,
            range: Range::Day,
        }
    }
}

struct Stocks {
    market: Shared,
    picks: HashMap<u64, Pick>,
}

impl Stocks {
    /// Tells the fetcher which charts the panels on show have picked.
    fn want(&self) {
        let mut market = lock(&self.market);
        let wanted = self
            .picks
            .values()
            .filter_map(|pick| Some((market.symbols.get(pick.symbol)?.clone(), pick.range)))
            .collect();
        market.want(self.picks.len(), wanted);
        self.market.1.notify_all();
    }

    fn change_watchlist(&mut self, body: &Value, add: bool) -> anyhow::Result<Value> {
        let symbol = body["symbol"]
            .as_str()
            .map(|symbol| symbol.trim().to_ascii_uppercase())
            .filter(|symbol| !symbol.is_empty() && symbol.len() <= 24)
            .context("a \"symbol\" is needed, such as \"TSM\"")?;
        let removed = {
            let mut market = lock(&self.market);
            let known = market.symbols.iter().position(|kept| *kept == symbol);
            let removed = match (add, known) {
                (true, Some(_)) | (false, None) => return Ok(json!(market.symbols)),
                (true, None) if market.symbols.len() >= WATCHLIST_LIMIT => {
                    bail!("the watchlist holds at most {WATCHLIST_LIMIT} symbols")
                }
                (true, None) => {
                    market.symbols.push(symbol);
                    None
                }
                (false, Some(_)) => market.remove(&symbol),
            };
            market.save().context("saving the watchlist")?;
            removed
        };
        // Each panel keeps the symbol it picked; one whose symbol went has
        // the one after it, or the last.
        let count = lock(&self.market).symbols.len();
        for pick in self.picks.values_mut() {
            if removed.is_some_and(|at| pick.symbol > at) {
                pick.symbol -= 1;
            }
            pick.symbol = pick.symbol.min(count.saturating_sub(1));
        }
        self.want();
        Ok(json!(lock(&self.market).symbols))
    }
}

impl Plugin for Stocks {
    fn call(&mut self, body: Value, cx: &mut Cx) -> anyhow::Result<Value> {
        let answer = match body["op"].as_str() {
            Some("list") => return Ok(json!(lock(&self.market).symbols)),
            Some("add") => self.change_watchlist(&body, true)?,
            Some("remove") => self.change_watchlist(&body, false)?,
            _ => bail!("unknown op; this plugin knows \"list\", \"add\" and \"remove\""),
        };
        cx.redraw();
        Ok(answer)
    }

    fn draw(&mut self, view: &View, frame: &mut Frame) {
        if let std::collections::hash_map::Entry::Vacant(new) = self.picks.entry(view.id) {
            new.insert(Pick::default());
            self.want();
        }
        let pick = self.picks[&view.id];
        let market = lock(&self.market);
        draw(&market, pick, &view.env, frame);
    }

    fn input(&mut self, view: &View, input: Input, _cx: &mut Cx) {
        let Input::Click(Click { id, .. }) = input else {
            return;
        };
        let pick = self.picks.entry(view.id).or_default();
        if let Some(symbol) = id.strip_prefix("quote:").and_then(|at| at.parse().ok()) {
            pick.symbol = symbol;
        } else if let Some(range) = id
            .strip_prefix("range:")
            .and_then(|label| Range::ALL.into_iter().find(|range| range.label() == label))
        {
            pick.range = range;
        }
        self.want();
    }

    fn closed(&mut self, view: &View) {
        self.picks.remove(&view.id);
        self.want();
    }
}

/// The whole panel: the watchlist above, the picked symbol's chart below.
fn draw(market: &Market, pick: Pick, env: &Env, frame: &mut Frame) {
    let width = env.width;
    let header = env.title.line + 20.0;
    frame.push(
        Text::new(PAD, 10.0, width * 0.5, env.title.line, "Watchlist")
            .size(Size::Title)
            .bold(),
    );
    let (status, color) = match &market.trouble {
        Some(trouble) => (trouble.as_str(), Token::Negative),
        None if market.days.is_empty() => ("Loading\u{2026}", Token::TextMuted),
        None => ("Yahoo Finance", Token::TextFaint),
    };
    let status = Text::new(width * 0.4, 10.0, width * 0.6 - PAD, env.title.line, status);
    frame.push(status.size(Size::Small).color(color).align(Align::Right));

    let row = env.body.line + env.small.line + 14.0;
    let count = market.symbols.len();
    let list_height = (count as f32 * row)
        .min(env.height * 0.5)
        .max(row * 2.0)
        .min((env.height - header).max(0.0));
    let mut rows = Vec::new();
    for (index, symbol) in market.symbols.iter().enumerate() {
        quote_row(
            &mut rows,
            market,
            env,
            symbol,
            index,
            index == pick.symbol,
            row,
        );
    }
    frame.push(Scroll::new(
        "quotes",
        0.0,
        header,
        width,
        list_height,
        count as f32 * row,
        rows,
    ));
    let top = header + list_height;
    frame.push(Rect::new(0.0, top, width, 1.0).fill(Token::Border));

    let Some(symbol) = market.symbols.get(pick.symbol) else {
        return;
    };
    detail(frame, market, env, symbol, pick.range, top + 1.0);
}

/// One symbol in the watchlist: its name, its day as a line, its price and
/// how far that moved.
fn quote_row(
    items: &mut Vec<Item>,
    market: &Market,
    env: &Env,
    symbol: &str,
    index: usize,
    picked: bool,
    row: f32,
) {
    let width = env.width;
    let y = index as f32 * row;
    items.push(
        Hit::new(format!("quote:{index}"), 0.0, y, width, row)
            .hover(Token::BgHover)
            .into(),
    );
    if picked {
        let pick = Rect::new(PAD / 2.0, y + 2.0, width - PAD, row - 4.0);
        items.push(pick.fill(Token::BgSelected).radius(6.0).into());
    }
    let top = y + (row - env.body.line - env.small.line) / 2.0;
    let left = width * 0.36;
    items.push(
        Text::new(PAD, top, left - PAD, env.body.line, symbol)
            .bold()
            .into(),
    );
    let name = market
        .names
        .get(symbol)
        .map(String::as_str)
        .unwrap_or_default();
    let name = Text::new(PAD, top + env.body.line, left - PAD, env.small.line, name);
    items.push(name.size(Size::Small).color(Token::TextMuted).into());

    let Some(day) = market.days.get(symbol) else {
        return;
    };
    let rising = day.price >= day.previous;
    let spark_left = left + 6.0;
    let spark_right = width * 0.62;
    if spark_right > spark_left + 8.0 {
        let (top, bottom) = (y + 10.0, y + row - 10.0);
        spark(items, day, spark_left, spark_right, top, bottom, rising);
    }
    let price = Text::new(
        spark_right,
        top,
        width - PAD - spark_right,
        env.body.line,
        price(day.price),
    );
    items.push(price.align(Align::Right).into());
    let change = percent(day.price, day.previous);
    let pill_width = change.chars().count() as f32 * env.mono.advance + 10.0;
    let pill_x = width - PAD - pill_width;
    let pill_y = top + env.body.line + 1.0;
    let pill_height = env.small.line;
    let (ground, color) = if rising {
        (Token::PositiveBg, Token::Positive)
    } else {
        (Token::NegativeBg, Token::Negative)
    };
    items.push(
        Rect::new(pill_x, pill_y, pill_width, pill_height)
            .fill(ground)
            .radius(4.0)
            .into(),
    );
    let change = Text::new(pill_x, pill_y, pill_width, pill_height, change);
    items.push(change.mono().color(color).align(Align::Center).into());
}

/// The day's prices as a line from `left` to `right`, with where the day
/// before closed as a faint level.
fn spark(
    items: &mut Vec<Item>,
    day: &Day,
    left: f32,
    right: f32,
    top: f32,
    bottom: f32,
    rising: bool,
) {
    let prices = &day.closes;
    if prices.len() < 2 {
        return;
    }
    let (low, high) = prices
        .iter()
        .chain([&day.previous])
        .fold((f64::MAX, f64::MIN), |(low, high), price| {
            (low.min(*price), high.max(*price))
        });
    let span = (high - low).max(f64::EPSILON);
    let height = bottom - top;
    let at = |price: f64| top + ((high - price) / span) as f32 * height;
    let step = (right - left) / (prices.len() - 1) as f32;
    let points = prices
        .iter()
        .enumerate()
        .flat_map(|(n, price)| [left + n as f32 * step, at(*price)])
        .collect();
    let level = at(day.previous);
    items.push(Line::new(vec![left, level, right, level], Token::TextFaint).into());
    let color = if rising {
        Token::Positive
    } else {
        Token::Negative
    };
    items.push(Line::new(points, color).width(1.5).into());
}

/// The picked symbol: its name, price and move, the ranges, and its chart.
fn detail(frame: &mut Frame, market: &Market, env: &Env, symbol: &str, range: Range, top: f32) {
    let width = env.width;
    let chart = market.charts.get(&(symbol.to_string(), range));
    let day = market.days.get(symbol);
    let mut y = top + 12.0;
    frame.push(
        Text::new(PAD, y, width * 0.5, env.title.line, symbol)
            .size(Size::Title)
            .bold(),
    );
    let name = market
        .names
        .get(symbol)
        .map(String::as_str)
        .unwrap_or_default();
    let name = Text::new(width * 0.35, y, width * 0.65 - PAD, env.title.line, name);
    frame.push(
        name.size(Size::Small)
            .color(Token::TextMuted)
            .align(Align::Right),
    );
    y += env.title.line + 4.0;

    let latest = day.map(|day| (day.price, day.previous));
    let latest = latest.or_else(|| chart.map(|chart| (chart.price, chart.previous)));
    if let Some((now, before)) = latest {
        frame.push(Text::new(PAD, y, width * 0.5, env.title.line, price(now)).size(Size::Title));
        let moved = format!("{} ({})", signed(now - before, now), percent(now, before));
        let color = if now >= before {
            Token::Positive
        } else {
            Token::Negative
        };
        let moved = Text::new(width * 0.4, y, width * 0.6 - PAD, env.title.line, moved);
        frame.push(moved.color(color).align(Align::Right));
    }
    y += env.title.line + 10.0;

    // The ranges, as a row of pills.
    let gap = 6.0;
    let count = Range::ALL.len() as f32;
    let pill = ((width - PAD * 2.0 - gap * (count - 1.0)) / count).max(1.0);
    let pill_height = env.body.line + 8.0;
    for (n, choice) in Range::ALL.into_iter().enumerate() {
        let x = PAD + n as f32 * (pill + gap);
        frame.push(
            Hit::new(format!("range:{}", choice.label()), x, y, pill, pill_height)
                .hover(Token::BgHover),
        );
        let shape = Rect::new(x, y, pill, pill_height).radius(6.0);
        let (shape, color) = if choice == range {
            (shape.fill(Token::BgSelected), Token::Text)
        } else {
            (shape.border(Token::Border), Token::TextMuted)
        };
        frame.push(shape);
        let label = Text::new(x, y, pill, pill_height, choice.label()).size(Size::Small);
        frame.push(label.color(color).align(Align::Center).bold());
    }
    y += pill_height + 14.0;

    let bottom = env.height - env.small.line - 14.0;
    let (left, right) = (PAD, width - PAD - SCALE_WIDTH);
    if bottom - y < 24.0 || right - left < 24.0 {
        return;
    }
    match chart {
        Some(chart) if chart.closes.len() >= 2 => {
            draw_chart(frame, env, chart, range, left, right, y, bottom)
        }
        _ => {
            let waiting = Text::new(left, y, right - left, bottom - y, "Loading\u{2026}");
            frame.push(waiting.color(Token::TextMuted).align(Align::Center));
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_chart(
    frame: &mut Frame,
    env: &Env,
    chart: &Chart,
    range: Range,
    left: f32,
    right: f32,
    top: f32,
    bottom: f32,
) {
    let prices = &chart.closes;
    let day = range == Range::Day;
    let (mut low, mut high) = prices
        .iter()
        .fold((f64::MAX, f64::MIN), |(low, high), price| {
            (low.min(*price), high.max(*price))
        });
    if day {
        low = low.min(chart.previous);
        high = high.max(chart.previous);
    }
    let span = (high - low).max(f64::EPSILON);
    let height = bottom - top;
    let at = |price: f64| top + ((high - price) / span) as f32 * height;
    // By step rather than by time: nights and weekends take no room.
    let step = (right - left) / (prices.len() - 1) as f32;
    let points: Vec<f32> = prices
        .iter()
        .enumerate()
        .flat_map(|(n, price)| [left + n as f32 * step, at(*price)])
        .collect();
    let rising = prices.last() >= Some(if day { &chart.previous } else { &prices[0] });
    let (ground, color) = if rising {
        (Token::PositiveBg, Token::Positive)
    } else {
        (Token::NegativeBg, Token::Negative)
    };
    frame.push(Rect::new(left, top, right - left, 1.0).fill(Token::Border));
    frame.push(Rect::new(left, bottom, right - left, 1.0).fill(Token::Border));
    if day {
        let level = at(chart.previous);
        frame.push(Line::new(vec![left, level, right, level], Token::TextFaint));
    }
    frame.push(Area::new(points.clone(), bottom, ground).fade());
    frame.push(Line::new(points, color).width(1.5));

    let scale_x = right + 6.0;
    let label = |text: String, y: f32| {
        Text::new(scale_x, y, SCALE_WIDTH - 6.0, env.small.line, text)
            .size(Size::Small)
            .color(Token::TextMuted)
    };
    frame.push(label(price(high), top - env.small.line / 2.0));
    frame.push(label(price(low), bottom - env.small.line / 2.0));

    let (first, last) = (chart.times.first(), chart.times.last());
    if let (Some(first), Some(last)) = (first, last) {
        let when = |time: i64| clock_or_date(time + chart.offset, day);
        let under = bottom + 6.0;
        let half = (right - left) / 2.0;
        let start = Text::new(left, under, half, env.small.line, when(*first));
        frame.push(start.size(Size::Small).color(Token::TextMuted));
        let mut end = when(*last);
        if day && !chart.zone.is_empty() {
            end = format!("{end} {}", chart.zone);
        }
        let end = Text::new(left + half, under, half, env.small.line, end);
        frame.push(
            end.size(Size::Small)
                .color(Token::TextMuted)
                .align(Align::Right),
        );
    }
}

/// A price with the places a market quotes it to: four below ten, which
/// is where currencies are.
fn price(value: f64) -> String {
    format!("{value:.*}", places(value))
}

fn places(value: f64) -> usize {
    if value.abs() >= 10.0 {
        2
    } else {
        4
    }
}

/// A move of a price of `of`, with its sign and that price's places.
fn signed(value: f64, of: f64) -> String {
    let sign = if value < 0.0 { "-" } else { "+" };
    format!("{sign}{:.*}", places(of), value.abs())
}

fn percent(now: f64, before: f64) -> String {
    if before == 0.0 {
        return "--".into();
    }
    let change = (now - before) / before * 100.0;
    format!(
        "{}{:.2}%",
        if change < 0.0 { "-" } else { "+" },
        change.abs()
    )
}

/// "15:45" for a time in a day's chart, "Sep 28" or "Sep 2026" otherwise:
/// `time` is seconds since the epoch, moved to the exchange's clock.
fn clock_or_date(time: i64, day: bool) -> String {
    let seconds = time.rem_euclid(86_400);
    if day {
        return format!("{:02}:{:02}", seconds / 3_600, seconds % 3_600 / 60);
    }
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let (year, month, date) = civil(time.div_euclid(86_400));
    format!("{} {date} {year}", MONTHS[(month - 1) as usize])
}

/// The calendar date of a day counted from 1970-01-01 (Howard Hinnant's
/// algorithm).
fn civil(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let date = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, date)
}

fn main() -> std::io::Result<()> {
    let file = thinkterm_plugin_sdk::data_dir().map(|dir| dir.join("watchlist.txt"));
    let market: Shared = Arc::new((Mutex::new(Market::load(file)), Condvar::new()));
    thinkterm_plugin_sdk::run_with(|emitter| {
        let fetching = Arc::clone(&market);
        let started = std::thread::Builder::new()
            .name("fetch".into())
            .spawn(move || market::fetch(fetching, emitter));
        if let Err(err) = started {
            eprintln!("stocks: cannot start fetching: {err}");
        }
        Stocks {
            market,
            picks: HashMap::new(),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use thinkterm_plugin_sdk::panel::{MonoMetrics, Player, TextMetrics};

    fn env() -> Env {
        let text = |size, line| TextMetrics { size, line };
        Env {
            width: 320.0,
            height: 800.0,
            scale: 2.0,
            dark: false,
            small: text(11.0, 15.0),
            body: text(13.0, 18.0),
            title: text(15.0, 20.0),
            mono: MonoMetrics {
                size: 12.0,
                line: 17.0,
                advance: 7.0,
            },
            locale: String::new(),
            cwd: None,
            remote: None,
            can_extend: false,
            close: None,
        }
    }

    #[test]
    fn numbers_and_dates_read_as_a_market_writes_them() {
        assert_eq!(price(341.071), "341.07");
        assert_eq!(price(1.08765), "1.0877");
        assert_eq!(percent(105.0, 100.0), "+5.00%");
        assert_eq!(percent(95.0, 100.0), "-5.00%");
        assert_eq!(signed(-1.5, 341.0), "-1.50");
        assert_eq!(signed(0.0012, 1.08), "+0.0012");
        assert_eq!(civil(0), (1970, 1, 1));
        assert_eq!(civil(20_724), (2026, 9, 28));
        assert_eq!(clock_or_date(15 * 3_600 + 45 * 60, true), "15:45");
        assert_eq!(clock_or_date(20_724 * 86_400, false), "Sep 28 2026");
    }

    #[test]
    fn a_click_picks_a_symbol_and_a_range() {
        let mut market = Market::load(None);
        market.days.insert(
            "AAPL".into(),
            Day {
                symbol: "AAPL".into(),
                price: 341.0,
                previous: 335.0,
                closes: vec![335.0, 338.0, 341.0],
            },
        );
        let shared: Shared = Arc::new((Mutex::new(market), Condvar::new()));
        let mut stocks = Stocks {
            market: shared,
            picks: HashMap::new(),
        };
        let view = View::new(3, env());
        let mut frame = Frame::default();
        stocks.draw(&view, &mut frame);
        assert_eq!(
            lock(&stocks.market).wanted,
            [("AAPL".to_string(), Range::Day)]
        );

        let mut player = Player::new(env());
        player.frame(frame);
        // The second row of the watchlist.
        let row = env().body.line + env().small.line + 14.0;
        let y = env().title.line + 20.0 + row * 1.5;
        let input = player
            .click(40.0, y, Default::default(), 1, Default::default())
            .unwrap();
        stocks.input(&view, input, &mut Cx::new());
        assert_eq!(lock(&stocks.market).wanted[0].0, "MSFT");

        let mut frame = Frame::default();
        stocks.draw(&view, &mut frame);
        let range = frame
            .items
            .iter()
            .find_map(|item| match item {
                Item::Hit(hit) if hit.id == "range:1Y" => Some(hit.clone()),
                _ => None,
            })
            .unwrap();
        player.frame(frame);
        let input = player
            .click(
                range.x + 2.0,
                range.y + 2.0,
                Default::default(),
                1,
                Default::default(),
            )
            .unwrap();
        stocks.input(&view, input, &mut Cx::new());
        assert_eq!(
            lock(&stocks.market).wanted,
            [("MSFT".to_string(), Range::Year)]
        );
        stocks.closed(&view);
        assert!(lock(&stocks.market).wanted.is_empty());
        assert_eq!(lock(&stocks.market).panels, 0);
    }

    #[test]
    fn a_symbol_taken_off_leaves_the_others_picked_and_nothing_of_itself() {
        let mut market = Market::load(None);
        market.symbols = vec!["A".into(), "B".into(), "C".into()];
        market.names.insert("A".into(), "Alpha".into());
        market.days.insert(
            "A".into(),
            Day {
                symbol: "A".into(),
                price: 1.0,
                previous: 1.0,
                closes: vec![1.0],
            },
        );
        let shared: Shared = Arc::new((Mutex::new(market), Condvar::new()));
        let mut stocks = Stocks {
            market: shared,
            picks: HashMap::new(),
        };
        // Two panels: one on B, one on C.
        for (view, symbol) in [(1, 1), (2, 2)] {
            stocks.picks.insert(
                view,
                Pick {
                    symbol,
                    ..Pick::default()
                },
            );
        }
        stocks
            .change_watchlist(&json!({"symbol": "A"}), false)
            .unwrap();
        let symbols = lock(&stocks.market).symbols.clone();
        let picked = |view: u64| symbols[stocks.picks[&view].symbol].clone();
        assert_eq!((picked(1), picked(2)), ("B".to_string(), "C".to_string()));
        let market = lock(&stocks.market);
        assert!(!market.days.contains_key("A") && !market.names.contains_key("A"));
        drop(market);

        // The one picked going, the panel has the one after it, or the last.
        stocks
            .change_watchlist(&json!({"symbol": "C"}), false)
            .unwrap();
        assert_eq!(stocks.picks[&2].symbol, 0);
        assert_eq!(lock(&stocks.market).symbols, ["B"]);
    }
}
