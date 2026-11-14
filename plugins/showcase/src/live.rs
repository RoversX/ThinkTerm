//! What moves by itself: made-up numbers about a few servers, which a thread
//! of the plugin's own steps once a second while a view shows them, and the
//! ticker that has the Motion section drawn again and again while it plays.
//! Both threads wait on a condition variable while nothing wants them, so a
//! plugin left idle costs nothing.

use crate::ui::Rng;
use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use thinkterm_plugin_sdk::Emitter;

/// How many seconds of each number are kept: the length of the charts.
pub const HISTORY: usize = 90;

/// One number over time, the newest last.
#[derive(Debug, Clone, Default)]
pub struct Series(pub VecDeque<f32>);

impl Series {
    fn push(&mut self, value: f32) {
        if self.0.len() == HISTORY {
            self.0.pop_front();
        }
        self.0.push_back(value);
    }

    pub fn last(&self) -> f32 {
        self.0.back().copied().unwrap_or_default()
    }

    /// The value `back` seconds ago, or the oldest kept.
    pub fn ago(&self, back: usize) -> f32 {
        let at = self.0.len().saturating_sub(back + 1);
        self.0.get(at).copied().unwrap_or_default()
    }

    /// The last `n` values.
    pub fn tail(&self, n: usize) -> Vec<f32> {
        let skip = self.0.len().saturating_sub(n);
        self.0.iter().skip(skip).copied().collect()
    }
}

#[derive(Debug, Clone)]
pub struct Host {
    pub name: &'static str,
    pub region: &'static str,
    pub cpu: f32,
    pub memory: f32,
}

/// The servers' numbers, made up and moving.
#[derive(Debug, Clone)]
pub struct Metrics {
    pub requests: Series,
    pub errors: Series,
    pub latency: Series,
    pub sessions: Series,
    pub hosts: Vec<Host>,
    /// How many times they have moved.
    pub tick: u64,
    rng: Rng,
}

impl Metrics {
    /// Numbers with a history already, so a chart is never empty.
    pub fn new(seed: u64) -> Self {
        let names = [
            ("server-a", "us-east"),
            ("server-b", "us-east"),
            ("server-c", "eu-west"),
            ("server-d", "eu-west"),
            ("server-e", "ap-south"),
            ("server-f", "ap-south"),
        ];
        let mut metrics = Self {
            requests: Series::default(),
            errors: Series::default(),
            latency: Series::default(),
            sessions: Series::default(),
            hosts: names
                .iter()
                .map(|(name, region)| Host {
                    name,
                    region,
                    cpu: 0.4,
                    memory: 0.5,
                })
                .collect(),
            tick: 0,
            rng: Rng(seed),
        };
        for _ in 0..HISTORY {
            metrics.step();
        }
        metrics.tick = 0;
        metrics
    }

    /// One second on: each number takes a step from where it was, pulled
    /// back toward where it usually is.
    pub fn step(&mut self) {
        self.tick += 1;
        let t = self.tick as f32;
        let wave = (t / 14.0).sin() * 0.5 + (t / 5.0).sin() * 0.2;
        let walk = |series: &Series, usual: f32, spread: f32, rng: &mut Rng| {
            let last = if series.0.is_empty() {
                usual
            } else {
                series.last()
            };
            let pulled = last + (usual - last) * 0.18;
            pulled + (rng.unit() - 0.5) * spread
        };
        let requests = walk(&self.requests, 1240.0 + wave * 260.0, 140.0, &mut self.rng).max(0.0);
        let burst = if self.rng.unit() > 0.94 { 18.0 } else { 0.0 };
        let errors = (walk(&self.errors, 6.0 + wave * 2.0, 3.0, &mut self.rng) + burst).max(0.0);
        let latency = walk(&self.latency, 84.0 - wave * 10.0, 9.0, &mut self.rng).max(5.0);
        let sessions = walk(&self.sessions, 412.0 + wave * 30.0, 12.0, &mut self.rng).max(0.0);
        self.requests.push(requests);
        self.errors.push(errors);
        self.latency.push(latency);
        self.sessions.push(sessions);
        for (n, host) in self.hosts.iter_mut().enumerate() {
            let usual = 0.3 + n as f32 * 0.09 + wave * 0.1;
            host.cpu = (host.cpu + (usual - host.cpu) * 0.3 + (self.rng.unit() - 0.5) * 0.12)
                .clamp(0.02, 0.99);
            host.memory = (host.memory
                + (0.45 + n as f32 * 0.06 - host.memory) * 0.1
                + (self.rng.unit() - 0.5) * 0.03)
                .clamp(0.05, 0.98);
        }
    }
}

/// The numbers, and how many views want them moving.
pub struct Live {
    pub metrics: Metrics,
    pub wanted: usize,
    /// The numbers stand still, whoever wants them: the user paused them.
    pub paused: bool,
}

pub type SharedLive = Arc<(Mutex<Live>, Condvar)>;

pub fn lock<T>(shared: &Mutex<T>) -> MutexGuard<'_, T> {
    shared
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Steps the numbers once a second while a view wants them, and has the
/// views drawn again; waits for nothing while none does.
pub fn tick_metrics(shared: SharedLive, emitter: Emitter) {
    let (live, wake) = &*shared;
    let mut live = lock(live);
    loop {
        while live.wanted == 0 || live.paused {
            live = wake
                .wait(live)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
        live.metrics.step();
        drop(live);
        emitter.redraw();
        live = lock(&shared.0);
        // A second, or less once nobody wants them any more.
        let (again, _) = wake
            .wait_timeout_while(live, Duration::from_secs(1), |live| {
                live.wanted > 0 && !live.paused
            })
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        live = again;
    }
}

/// How often the Motion section wants drawing: 0 while nothing plays.
pub struct Ticker {
    pub rate: u32,
}

pub type SharedTicker = Arc<(Mutex<Ticker>, Condvar)>;

/// Has the views drawn `rate` times a second while some view plays the
/// Motion section; waits for nothing while none does. A redraw asked for
/// while one is still to be done folds into it, so a plugin that cannot
/// draw that fast draws as fast as it can, and no faster.
pub fn tick_motion(shared: SharedTicker, emitter: Emitter) {
    let (ticker, wake) = &*shared;
    let mut next = Instant::now();
    loop {
        let rate = {
            let mut ticker = lock(ticker);
            while ticker.rate == 0 {
                ticker = wake
                    .wait(ticker)
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                next = Instant::now();
            }
            ticker.rate
        };
        emitter.redraw();
        let interval = Duration::from_secs_f64(1.0 / f64::from(rate.clamp(1, 240)));
        next += interval;
        let now = Instant::now();
        if next <= now {
            // Fell behind: start the beat again from here rather than
            // catching up with a burst.
            next = now + interval;
        }
        std::thread::sleep(next - now);
    }
}
