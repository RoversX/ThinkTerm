//! Numbers from inside the server, on request.
//!
//! Set `THINKTERM_MUX_STATS` to a number of seconds and every histogram
//! and counter the server records is logged at that interval: push
//! latency, how often a push found its pane busy, parser throughput.
//! Without it no recorder is installed and the metrics macros cost
//! nothing.

use hdrhistogram::Histogram;
use metrics::{Counter, Gauge, Key, KeyName, Metadata, Recorder, SharedString, Unit};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Latencies in nanoseconds; sizes and rates as they come.
struct Hist {
    values: Mutex<Histogram<u64>>,
    scale: f64,
    /// For `.rate` keys: everything recorded since the last report.
    total: AtomicU64,
}

impl metrics::HistogramFn for Hist {
    fn record(&self, value: f64) {
        self.total.fetch_add(value as u64, Ordering::Relaxed);
        self.values
            .lock()
            .unwrap()
            .record((value * self.scale) as u64)
            .ok();
    }
}

struct Count(AtomicU64);

impl metrics::CounterFn for Count {
    fn increment(&self, value: u64) {
        self.0.fetch_add(value, Ordering::Relaxed);
    }

    fn absolute(&self, value: u64) {
        self.0.store(value, Ordering::Relaxed);
    }
}

#[derive(Default)]
struct Registry {
    histograms: HashMap<Key, Arc<Hist>>,
    counters: HashMap<Key, Arc<Count>>,
}

pub struct Stats {
    registry: Arc<Mutex<Registry>>,
}

pub fn init_from_env() -> anyhow::Result<()> {
    let Some(seconds) = std::env::var("THINKTERM_MUX_STATS")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|s| *s > 0)
    else {
        return Ok(());
    };
    let registry = Arc::new(Mutex::new(Registry::default()));
    let reporter = Arc::clone(&registry);
    std::thread::Builder::new()
        .name("mux-stats".into())
        .spawn(move || loop {
            std::thread::sleep(Duration::from_secs(seconds));
            report(&reporter, seconds);
        })?;
    metrics::set_global_recorder(Stats { registry })
        .map_err(|e| anyhow::anyhow!("installing the metrics recorder: {e}"))
}

fn report(registry: &Mutex<Registry>, interval: u64) {
    let registry = registry.lock().unwrap();
    let mut lines = vec![];
    for (key, hist) in &registry.histograms {
        let name = key.name();
        if name.ends_with(".rate") {
            let total = hist.total.swap(0, Ordering::Relaxed);
            lines.push(format!("{key}: {}/s", total / interval));
            continue;
        }
        let values = hist.values.lock().unwrap();
        if values.is_empty() {
            continue;
        }
        if name.ends_with(".size") {
            lines.push(format!(
                "{key}: n={} p50={} p95={} max={}",
                values.len(),
                values.value_at_percentile(50.),
                values.value_at_percentile(95.),
                values.max()
            ));
        } else {
            let at = |p| Duration::from_nanos(values.value_at_percentile(p));
            lines.push(format!(
                "{key}: n={} p50={:.2?} p95={:.2?} p99={:.2?} max={:.2?}",
                values.len(),
                at(50.),
                at(95.),
                at(99.),
                Duration::from_nanos(values.max())
            ));
        }
    }
    for (key, count) in &registry.counters {
        lines.push(format!("{key}: {}", count.0.load(Ordering::Relaxed)));
    }
    lines.sort();
    log::info!("mux stats\n{}", lines.join("\n"));
}

impl Recorder for Stats {
    fn describe_counter(&self, _key: KeyName, _unit: Option<Unit>, _description: SharedString) {}

    fn describe_gauge(&self, _key: KeyName, _unit: Option<Unit>, _description: SharedString) {}

    fn describe_histogram(&self, _key: KeyName, _unit: Option<Unit>, _description: SharedString) {}

    fn register_counter(&self, key: &Key, _metadata: &Metadata) -> Counter {
        let mut registry = self.registry.lock().unwrap();
        let count = registry
            .counters
            .entry(key.clone())
            .or_insert_with(|| Arc::new(Count(AtomicU64::new(0))));
        Counter::from_arc(Arc::clone(count))
    }

    fn register_gauge(&self, _key: &Key, _metadata: &Metadata) -> Gauge {
        Gauge::noop()
    }

    fn register_histogram(&self, key: &Key, _metadata: &Metadata) -> metrics::Histogram {
        let mut registry = self.registry.lock().unwrap();
        let name = key.name();
        let scale = if name.ends_with(".size") || name.ends_with(".rate") {
            1.0
        } else {
            1e9
        };
        let hist = registry.histograms.entry(key.clone()).or_insert_with(|| {
            Arc::new(Hist {
                values: Mutex::new(Histogram::new(2).expect("a fresh histogram")),
                scale,
                total: AtomicU64::new(0),
            })
        });
        metrics::Histogram::from_arc(Arc::clone(hist))
    }
}
