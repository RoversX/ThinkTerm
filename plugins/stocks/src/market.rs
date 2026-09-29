//! What the panels show, and the thread that fetches it: the watchlist's
//! quotes every [`QUOTES_EVERY`], the charts the panels have picked every
//! [`CHARTS_EVERY`], and each symbol's name once. Nothing is fetched while
//! no panel is on show.

use crate::yahoo::{self, Chart, Day, Failure, Range};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use thinkterm_plugin_sdk::Emitter;

const QUOTES_EVERY: Duration = Duration::from_secs(15);
const CHARTS_EVERY: Duration = Duration::from_secs(60);
/// How long to leave Yahoo alone once it says it was asked too often.
const LIMITED_FOR: Duration = Duration::from_secs(120);
/// What a watchlist starts with.
const START: &[&str] = &[
    "AAPL", "MSFT", "NVDA", "GOOGL", "AMZN", "META", "TSLA", "^GSPC", "^IXIC", "BTC-USD",
    "EURUSD=X", "GC=F",
];
/// The longest watchlist kept.
pub const WATCHLIST_LIMIT: usize = 50;

#[derive(Default)]
pub struct Market {
    pub symbols: Vec<String>,
    pub days: HashMap<String, Day>,
    pub names: HashMap<String, String>,
    /// Only the charts some panel has picked.
    pub charts: HashMap<(String, Range), Chart>,
    /// Why the last fetch failed, while it does.
    pub trouble: Option<String>,
    /// Panels on show, and the charts they picked.
    pub panels: usize,
    pub wanted: Vec<(String, Range)>,
    /// Where the watchlist is kept.
    file: Option<PathBuf>,
}

pub type Shared = Arc<(Mutex<Market>, Condvar)>;

pub fn lock(shared: &Shared) -> MutexGuard<'_, Market> {
    shared
        .0
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl Market {
    /// The watchlist kept in `file`, or the one a watchlist starts with.
    pub fn load(file: Option<PathBuf>) -> Self {
        let kept = file
            .as_ref()
            .and_then(|file| std::fs::read_to_string(file).ok());
        let symbols = match kept {
            Some(text) => text
                .lines()
                .map(str::trim)
                .filter(|symbol| !symbol.is_empty())
                .take(WATCHLIST_LIMIT)
                .map(str::to_string)
                .collect(),
            None => START.iter().map(|symbol| symbol.to_string()).collect(),
        };
        Self {
            symbols,
            file,
            ..Self::default()
        }
    }

    pub fn save(&self) -> std::io::Result<()> {
        match &self.file {
            Some(file) => std::fs::write(file, self.symbols.join("\n") + "\n"),
            None => Ok(()),
        }
    }

    /// Takes `symbol` off the watchlist, and lets go of what was fetched
    /// for it: where it was, if it was there.
    pub fn remove(&mut self, symbol: &str) -> Option<usize> {
        let at = self.symbols.iter().position(|kept| kept == symbol)?;
        self.symbols.remove(at);
        self.days.remove(symbol);
        self.names.remove(symbol);
        Some(at)
    }

    /// Whether `symbol` is on the watchlist: what comes for one that was
    /// taken off meanwhile is not kept.
    fn watched(&self, symbol: &str) -> bool {
        self.symbols.iter().any(|kept| kept == symbol)
    }

    /// The charts no panel wants any longer are let go.
    pub fn want(&mut self, panels: usize, wanted: Vec<(String, Range)>) {
        self.charts.retain(|picked, _| wanted.contains(picked));
        self.panels = panels;
        self.wanted = wanted;
    }

    /// The charts picked that are to be fetched at `now`: one not held --
    /// never fetched, or let go while no panel wanted it -- at once, one
    /// held once `due` says it was fetched [`CHARTS_EVERY`] ago.
    fn charts_due(
        &self,
        due: &HashMap<(String, Range), Instant>,
        now: Instant,
    ) -> Vec<(String, Range)> {
        self.wanted
            .iter()
            .filter(|picked| {
                !self.charts.contains_key(*picked) || due.get(*picked).is_none_or(|at| *at <= now)
            })
            .cloned()
            .collect()
    }
}

/// Fetches for as long as the plugin runs, telling the panels to draw
/// anew whenever something came.
pub fn fetch(shared: Shared, emitter: Emitter) {
    let mut quotes_due = Instant::now();
    let mut charts_due: HashMap<(String, Range), Instant> = HashMap::new();
    let mut quiet_until = Instant::now();
    loop {
        let (symbols, wanted, charts, unnamed) = {
            let mut market = lock(&shared);
            loop {
                let now = Instant::now();
                let chart_due = !market.charts_due(&charts_due, now).is_empty();
                let unnamed = market
                    .symbols
                    .iter()
                    .any(|symbol| !market.names.contains_key(symbol));
                let due = quotes_due <= now || chart_due || unnamed;
                if market.panels > 0 && due && quiet_until <= now {
                    break;
                }
                let wait = if market.panels == 0 {
                    QUOTES_EVERY
                } else {
                    let next = charts_due
                        .values()
                        .copied()
                        .chain([quotes_due, quiet_until])
                        .filter(|at| *at > now)
                        .min()
                        .unwrap_or(now + QUOTES_EVERY);
                    next - now
                };
                market = shared
                    .1
                    .wait_timeout(market, wait)
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .0;
            }
            let unnamed: Vec<String> = market
                .symbols
                .iter()
                .filter(|symbol| !market.names.contains_key(*symbol))
                .cloned()
                .collect();
            let charts = market.charts_due(&charts_due, Instant::now());
            (
                market.symbols.clone(),
                market.wanted.clone(),
                charts,
                unnamed,
            )
        };

        let now = Instant::now();
        let mut failure = None;
        if quotes_due <= now {
            quotes_due = now + QUOTES_EVERY;
            match yahoo::days(&symbols) {
                Ok(days) => {
                    let mut market = lock(&shared);
                    for day in days {
                        if market.watched(&day.symbol) {
                            market.days.insert(day.symbol.clone(), day);
                        }
                    }
                }
                Err(err) => failure = Some(err),
            }
        }
        for picked in &charts {
            if failure.is_some() {
                break;
            }
            charts_due.insert(picked.clone(), now + CHARTS_EVERY);
            match yahoo::chart(&picked.0, picked.1) {
                Ok(chart) => {
                    let mut market = lock(&shared);
                    if market.watched(&picked.0) {
                        market.names.insert(picked.0.clone(), chart.name.clone());
                    }
                    if market.wanted.contains(picked) {
                        market.charts.insert(picked.clone(), chart);
                    }
                }
                Err(err) => failure = Some(err),
            }
        }
        charts_due.retain(|picked, _| wanted.contains(picked));
        // A name comes with a chart: one symbol at a time, so a long
        // watchlist does not ask for all of them at once.
        if let (None, Some(symbol)) = (&failure, unnamed.first()) {
            let named = |name: String| {
                let mut market = lock(&shared);
                if market.watched(symbol) {
                    market.names.insert(symbol.clone(), name);
                }
            };
            match yahoo::chart(symbol, Range::Day) {
                Ok(chart) => named(chart.name),
                Err(Failure::Failed(why)) => {
                    // A symbol Yahoo does not know keeps its own name.
                    eprintln!("stocks: no name for {symbol}: {why}");
                    named(String::new());
                }
                Err(Failure::Limited) => failure = Some(Failure::Limited),
            }
        }
        {
            let mut market = lock(&shared);
            market.trouble = match failure {
                None => None,
                Some(Failure::Limited) => {
                    quiet_until = Instant::now() + LIMITED_FOR;
                    Some("Yahoo Finance asks to wait a while".into())
                }
                Some(Failure::Failed(why)) => {
                    // Tried again with the next quotes, not at once.
                    quiet_until = Instant::now() + QUOTES_EVERY;
                    eprintln!("stocks: {why}");
                    Some("Cannot reach Yahoo Finance".into())
                }
            };
        }
        emitter.redraw();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chart() -> Chart {
        Chart {
            name: "Apple Inc.".into(),
            currency: "USD".into(),
            zone: "EDT".into(),
            price: 1.0,
            previous: 1.0,
            offset: 0,
            time: None,
            times: Vec::new(),
            closes: Vec::new(),
        }
    }

    #[test]
    fn a_chart_let_go_is_fetched_again_as_soon_as_it_is_picked() {
        let picked = ("AAPL".to_string(), Range::Day);
        let now = Instant::now();
        let mut due = HashMap::new();
        let mut market = Market::default();
        market.want(1, vec![picked.clone()]);
        assert_eq!(market.charts_due(&due, now), std::slice::from_ref(&picked));
        // Fetched, it is not fetched again for a while.
        due.insert(picked.clone(), now + CHARTS_EVERY);
        market.charts.insert(picked.clone(), chart());
        assert!(market.charts_due(&due, now).is_empty());
        // The panel closes and opens again: what it showed was let go.
        market.want(0, Vec::new());
        market.want(1, vec![picked.clone()]);
        assert_eq!(market.charts_due(&due, now), [picked]);
    }
}
