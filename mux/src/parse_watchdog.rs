//! A watchdog for the per-pane output parse threads.
//!
//! A program that emits faster than the terminal model can process keeps its
//! pane's parse thread pegged for as long as the flood lasts. The 2026-08-21
//! incident ran a mux server core at 100% for nine hours without writing a
//! single log byte. Backpressure already bounds memory (the socketpair
//! between pty reader and parser fills and blocks the read); the missing
//! piece was a signal. This module measures how busy each parse thread is
//! and logs loudly when one saturates, with enough context (pane id,
//! throughput, duration) to identify the offender from the server log alone.

use crate::pane::PaneId;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Once, OnceLock};
use std::time::{Duration, Instant};

const SCAN_INTERVAL: Duration = Duration::from_secs(10);
/// Fraction of a scan interval the parse thread must have been busy for the
/// interval to count as saturated.
const SATURATION_BUSY_FRACTION: f64 = 0.90;
/// Consecutive saturated intervals before the first warning; one busy burst
/// (a huge paste, one large image) is normal and not worth a log line.
const SATURATED_INTERVALS_BEFORE_WARN: u32 = 2;
/// While saturation persists, repeat the warning this often.
const REWARN_EVERY: Duration = Duration::from_secs(60);

pub(crate) struct ParseHeartbeat {
    busy_nanos: AtomicU64,
    bytes: AtomicU64,
}

impl ParseHeartbeat {
    fn new() -> Self {
        Self {
            busy_nanos: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
        }
    }

    /// Called from the parse thread after each unit of work.
    pub fn add(&self, busy: Duration, bytes: usize) {
        self.busy_nanos
            .fetch_add(busy.as_nanos() as u64, Ordering::Relaxed);
        if bytes > 0 {
            self.bytes.fetch_add(bytes as u64, Ordering::Relaxed);
        }
    }
}

struct PaneWatch {
    heartbeat: Arc<ParseHeartbeat>,
    last_busy_nanos: u64,
    last_bytes: u64,
    saturated_intervals: u32,
    saturated_since: Option<Instant>,
    last_warned: Option<Instant>,
}

fn registry() -> &'static Mutex<HashMap<PaneId, PaneWatch>> {
    static REGISTRY: OnceLock<Mutex<HashMap<PaneId, PaneWatch>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Register a pane's parse thread; the returned heartbeat is fed by that
/// thread. The watchdog thread is started on first use.
pub(crate) fn register(pane_id: PaneId) -> Arc<ParseHeartbeat> {
    let heartbeat = Arc::new(ParseHeartbeat::new());
    registry().lock().unwrap().insert(
        pane_id,
        PaneWatch {
            heartbeat: Arc::clone(&heartbeat),
            last_busy_nanos: 0,
            last_bytes: 0,
            saturated_intervals: 0,
            saturated_since: None,
            last_warned: None,
        },
    );
    static WATCHDOG: Once = Once::new();
    WATCHDOG.call_once(|| {
        if let Err(err) = std::thread::Builder::new()
            .name("parse-watchdog".to_string())
            .spawn(watchdog_loop)
        {
            log::error!("failed to spawn parse-watchdog thread: {err:#}");
        }
    });
    heartbeat
}

pub(crate) fn unregister(pane_id: PaneId) {
    registry().lock().unwrap().remove(&pane_id);
}

fn watchdog_loop() {
    let mut last_scan = Instant::now();
    loop {
        std::thread::sleep(SCAN_INTERVAL);
        let now = Instant::now();
        let elapsed = now.duration_since(last_scan);
        last_scan = now;
        let interval_nanos = elapsed.as_nanos() as u64;
        if interval_nanos == 0 {
            continue;
        }
        let mut registry = registry().lock().unwrap();
        for (pane_id, watch) in registry.iter_mut() {
            let busy = watch.heartbeat.busy_nanos.load(Ordering::Relaxed);
            let bytes = watch.heartbeat.bytes.load(Ordering::Relaxed);
            let busy_delta = busy.saturating_sub(watch.last_busy_nanos);
            let byte_delta = bytes.saturating_sub(watch.last_bytes);
            watch.last_busy_nanos = busy;
            watch.last_bytes = bytes;

            let busy_fraction = busy_delta as f64 / interval_nanos as f64;
            if busy_fraction >= SATURATION_BUSY_FRACTION {
                if watch.saturated_intervals == 0 {
                    watch.saturated_since = Some(now - elapsed);
                }
                watch.saturated_intervals += 1;
                let warn_due = watch.saturated_intervals >= SATURATED_INTERVALS_BEFORE_WARN
                    && watch
                        .last_warned
                        .is_none_or(|warned| now.duration_since(warned) >= REWARN_EVERY);
                if warn_due {
                    let sustained = watch
                        .saturated_since
                        .map_or(Duration::ZERO, |since| now.duration_since(since));
                    log::warn!(
                        "pane {pane_id}: output parser saturated for {}s \
                         ({:.0}% busy, {:.1} MB/s); a program in this pane is \
                         emitting faster than the terminal can process it",
                        sustained.as_secs(),
                        busy_fraction * 100.,
                        byte_delta as f64 / elapsed.as_secs_f64() / 1e6,
                    );
                    watch.last_warned = Some(now);
                }
            } else if watch.saturated_intervals > 0 {
                if watch.last_warned.is_some() {
                    let sustained = watch
                        .saturated_since
                        .map_or(Duration::ZERO, |since| now.duration_since(since));
                    log::info!(
                        "pane {pane_id}: output parser recovered after {}s of saturation",
                        sustained.as_secs()
                    );
                }
                watch.saturated_intervals = 0;
                watch.saturated_since = None;
                watch.last_warned = None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heartbeat_accumulates() {
        let hb = ParseHeartbeat::new();
        hb.add(Duration::from_millis(5), 100);
        hb.add(Duration::from_millis(7), 0);
        assert_eq!(
            hb.busy_nanos.load(Ordering::Relaxed),
            Duration::from_millis(12).as_nanos() as u64
        );
        assert_eq!(hb.bytes.load(Ordering::Relaxed), 100);
    }

    #[test]
    fn register_unregister_round_trip() {
        // Use a pane id far outside anything a test mux would allocate.
        let pane_id: PaneId = usize::MAX - 7;
        let hb = register(pane_id);
        hb.add(Duration::from_millis(1), 1);
        assert!(registry().lock().unwrap().contains_key(&pane_id));
        unregister(pane_id);
        assert!(!registry().lock().unwrap().contains_key(&pane_id));
    }
}
