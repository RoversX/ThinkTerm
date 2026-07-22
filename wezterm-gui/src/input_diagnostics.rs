use std::collections::{HashMap, VecDeque};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

const RECENT_LIMIT: usize = 2048;

static STATE: OnceLock<Mutex<InputDiagnosticsState>> = OnceLock::new();

fn state() -> &'static Mutex<InputDiagnosticsState> {
    STATE.get_or_init(|| Mutex::new(InputDiagnosticsState::default()))
}

#[derive(Debug, Clone)]
pub(crate) struct InputDiagnosticsSnapshot {
    pub(crate) enabled: bool,
    pub(crate) started_ago: Option<Duration>,
    pub(crate) key_events: u64,
    pub(crate) key_down_events: u64,
    pub(crate) key_up_events: u64,
    pub(crate) modifier_events: u64,
    pub(crate) handled_events: u64,
    pub(crate) total_duration: Duration,
    pub(crate) max_duration: Duration,
    pub(crate) recent_p95: Duration,
    pub(crate) slowest_stage: Option<StageSnapshot>,
    pub(crate) stages: Vec<StageSnapshot>,
    /// Gauge values summed across every reporting source (window); each
    /// source publishes its own set so windows never clobber each other.
    pub(crate) gauges: Vec<(&'static str, u64)>,
}

#[derive(Debug, Clone)]
pub(crate) struct StageSnapshot {
    pub(crate) name: &'static str,
    pub(crate) count: u64,
    pub(crate) success: u64,
    pub(crate) total_duration: Duration,
    pub(crate) max_duration: Duration,
    pub(crate) recent_p95: Duration,
}

impl InputDiagnosticsSnapshot {
    pub(crate) fn average_duration(&self) -> Duration {
        avg_duration(self.total_duration, self.key_events)
    }

    pub(crate) fn summary_lines(&self) -> Vec<String> {
        let mut lines = vec![
            "ThinkTerm Input Diagnostics".to_string(),
            format!("status: {}", if self.enabled { "running" } else { "off" }),
        ];
        if let Some(started_ago) = self.started_ago {
            lines.push(format!("started: {} ago", format_duration(started_ago)));
        }
        lines.push(format!("key_events: {}", self.key_events));
        lines.push(format!("key_down_events: {}", self.key_down_events));
        lines.push(format!("key_up_events: {}", self.key_up_events));
        lines.push(format!("modifier_events: {}", self.modifier_events));
        lines.push(format!("handled_events: {}", self.handled_events));
        lines.push(format!(
            "key_event_avg: {}",
            format_duration(self.average_duration())
        ));
        lines.push(format!(
            "key_event_p95_recent: {}",
            format_duration(self.recent_p95)
        ));
        lines.push(format!(
            "key_event_max: {}",
            format_duration(self.max_duration)
        ));
        if let Some(stage) = &self.slowest_stage {
            lines.push(format!(
                "slowest_stage: {} max={} p95={} count={}",
                stage.name,
                format_duration(stage.max_duration),
                format_duration(stage.recent_p95),
                stage.count
            ));
        }
        for stage in &self.stages {
            lines.push(format!(
                "stage.{}: count={} success={} avg={} p95={} max={}",
                stage.name,
                stage.count,
                stage.success,
                format_duration(avg_duration(stage.total_duration, stage.count)),
                format_duration(stage.recent_p95),
                format_duration(stage.max_duration),
            ));
        }
        for (name, value) in &self.gauges {
            lines.push(format!("gauge.{name}: {value} (process total)"));
        }
        lines
    }
}

#[derive(Debug)]
pub(crate) struct KeyEventTrace {
    enabled: bool,
    started: Instant,
    key_down: bool,
    modifier: bool,
    handled: bool,
}

impl KeyEventTrace {
    pub(crate) fn begin(key_down: bool, modifier: bool) -> Self {
        Self {
            enabled: enabled(),
            started: Instant::now(),
            key_down,
            modifier,
            handled: false,
        }
    }

    pub(crate) fn handled(&mut self) {
        self.handled = true;
    }
}

impl Drop for KeyEventTrace {
    fn drop(&mut self) {
        if !self.enabled {
            return;
        }
        state().lock().record_key_event(
            self.started.elapsed(),
            self.key_down,
            self.modifier,
            self.handled,
        );
    }
}

#[derive(Debug)]
pub(crate) struct StageTimer {
    enabled: bool,
    name: &'static str,
    started: Instant,
}

impl StageTimer {
    pub(crate) fn begin(name: &'static str) -> Self {
        Self {
            enabled: enabled(),
            name,
            started: Instant::now(),
        }
    }

    pub(crate) fn finish(self, success: bool) {
        if self.enabled {
            state()
                .lock()
                .record_stage(self.name, self.started.elapsed(), success);
        }
    }
}

#[derive(Debug)]
struct InputDiagnosticsState {
    enabled: bool,
    started_at: Option<Instant>,
    key_events: u64,
    key_down_events: u64,
    key_up_events: u64,
    modifier_events: u64,
    handled_events: u64,
    total_duration: Duration,
    max_duration: Duration,
    recent: VecDeque<Duration>,
    stages: HashMap<&'static str, StageStats>,
    /// Per-source (per-window) gauge sets, keyed by a stable source id such
    /// as `space_owner_id`. Kept separate so multiple windows do not
    /// overwrite each other; snapshots aggregate across sources.
    gauges: HashMap<u64, HashMap<&'static str, u64>>,
}

impl Default for InputDiagnosticsState {
    fn default() -> Self {
        Self {
            enabled: false,
            started_at: None,
            key_events: 0,
            key_down_events: 0,
            key_up_events: 0,
            modifier_events: 0,
            handled_events: 0,
            total_duration: Duration::ZERO,
            max_duration: Duration::ZERO,
            recent: VecDeque::with_capacity(RECENT_LIMIT),
            stages: HashMap::new(),
            gauges: HashMap::new(),
        }
    }
}

impl InputDiagnosticsState {
    fn clear_stats(&mut self) {
        self.key_events = 0;
        self.key_down_events = 0;
        self.key_up_events = 0;
        self.modifier_events = 0;
        self.handled_events = 0;
        self.total_duration = Duration::ZERO;
        self.max_duration = Duration::ZERO;
        self.recent.clear();
        self.stages.clear();
        self.gauges.clear();
    }

    fn set_gauges_for_source(&mut self, source_id: u64, values: &[(&'static str, u64)]) {
        if !self.enabled {
            return;
        }
        let entry = self.gauges.entry(source_id).or_default();
        for (name, value) in values {
            entry.insert(name, *value);
        }
    }

    fn remove_gauges_for_source(&mut self, source_id: u64) {
        self.gauges.remove(&source_id);
    }

    fn aggregated_gauges(&self) -> Vec<(&'static str, u64)> {
        let mut totals: HashMap<&'static str, u64> = HashMap::new();
        for source in self.gauges.values() {
            for (name, value) in source {
                *totals.entry(name).or_default() += value;
            }
        }
        let mut totals: Vec<_> = totals.into_iter().collect();
        totals.sort_by_key(|(name, _)| *name);
        totals
    }

    fn set_enabled(&mut self, enabled: bool) {
        if self.enabled == enabled {
            return;
        }
        self.enabled = enabled;
        if enabled {
            self.started_at = Some(Instant::now());
            self.clear_stats();
        }
    }

    fn record_key_event(
        &mut self,
        duration: Duration,
        key_down: bool,
        modifier: bool,
        handled: bool,
    ) {
        if !self.enabled {
            return;
        }
        self.key_events += 1;
        if key_down {
            self.key_down_events += 1;
        } else {
            self.key_up_events += 1;
        }
        if modifier {
            self.modifier_events += 1;
        }
        if handled {
            self.handled_events += 1;
        }
        self.total_duration += duration;
        self.max_duration = self.max_duration.max(duration);
        push_recent(&mut self.recent, duration);
    }

    fn record_stage(&mut self, name: &'static str, duration: Duration, success: bool) {
        if !self.enabled {
            return;
        }
        self.stages
            .entry(name)
            .or_default()
            .record(duration, success);
    }

    fn snapshot(&self) -> InputDiagnosticsSnapshot {
        let mut stages = self
            .stages
            .iter()
            .map(|(name, stats)| stats.snapshot(name))
            .collect::<Vec<_>>();
        stages.sort_by(|a, b| b.max_duration.cmp(&a.max_duration));
        let slowest_stage = stages.first().cloned();
        InputDiagnosticsSnapshot {
            enabled: self.enabled,
            started_ago: self.started_at.map(|started| started.elapsed()),
            key_events: self.key_events,
            key_down_events: self.key_down_events,
            key_up_events: self.key_up_events,
            modifier_events: self.modifier_events,
            handled_events: self.handled_events,
            total_duration: self.total_duration,
            max_duration: self.max_duration,
            recent_p95: percentile_duration(&self.recent, 0.95),
            slowest_stage,
            stages,
            gauges: self.aggregated_gauges(),
        }
    }
}

#[derive(Debug, Default)]
struct StageStats {
    count: u64,
    success: u64,
    total_duration: Duration,
    max_duration: Duration,
    recent: VecDeque<Duration>,
}

impl StageStats {
    fn record(&mut self, duration: Duration, success: bool) {
        self.count += 1;
        if success {
            self.success += 1;
        }
        self.total_duration += duration;
        self.max_duration = self.max_duration.max(duration);
        push_recent(&mut self.recent, duration);
    }

    fn snapshot(&self, name: &'static str) -> StageSnapshot {
        StageSnapshot {
            name,
            count: self.count,
            success: self.success,
            total_duration: self.total_duration,
            max_duration: self.max_duration,
            recent_p95: percentile_duration(&self.recent, 0.95),
        }
    }
}

pub(crate) fn enabled() -> bool {
    state().lock().enabled
}

pub(crate) fn set_enabled(enabled: bool) {
    state().lock().set_enabled(enabled);
}

pub(crate) fn reset() {
    let mut state = state().lock();
    state.clear_stats();
    if state.enabled {
        state.started_at = Some(Instant::now());
    }
}

pub(crate) fn snapshot() -> InputDiagnosticsSnapshot {
    state().lock().snapshot()
}

/// Publish one window's gauge set in a single lock acquisition. Values are
/// stored per source so concurrent windows never overwrite each other;
/// snapshots aggregate across sources.
pub(crate) fn set_gauges_for_source(source_id: u64, values: &[(&'static str, u64)]) {
    state().lock().set_gauges_for_source(source_id, values);
}

/// Drop a closed window's gauges so they do not linger in process totals.
pub(crate) fn remove_gauges_for_source(source_id: u64) {
    state().lock().remove_gauges_for_source(source_id);
}

fn push_recent(recent: &mut VecDeque<Duration>, duration: Duration) {
    if recent.len() == RECENT_LIMIT {
        recent.pop_front();
    }
    recent.push_back(duration);
}

fn percentile_duration(values: &VecDeque<Duration>, percentile: f64) -> Duration {
    if values.is_empty() {
        return Duration::ZERO;
    }
    let mut values = values.iter().copied().collect::<Vec<_>>();
    values.sort_unstable();
    let idx = ((values.len() - 1) as f64 * percentile).round() as usize;
    values[idx.min(values.len() - 1)]
}

fn avg_duration(total: Duration, count: u64) -> Duration {
    if count == 0 {
        Duration::ZERO
    } else {
        Duration::from_secs_f64(total.as_secs_f64() / count as f64)
    }
}

pub(crate) fn format_duration(duration: Duration) -> String {
    let micros = duration.as_secs_f64() * 1_000_000.0;
    if micros >= 1000.0 {
        format!("{:.2} ms", micros / 1000.0)
    } else {
        format!("{micros:.0} us")
    }
}

#[cfg(test)]
mod gauge_tests {
    use super::*;

    fn enabled_state() -> InputDiagnosticsState {
        let mut state = InputDiagnosticsState::default();
        state.set_enabled(true);
        state
    }

    #[test]
    fn gauges_are_isolated_per_source_and_summed_in_snapshots() {
        let mut state = enabled_state();
        state.set_gauges_for_source(1, &[("note_cache_len", 10), ("note_cache_bytes", 100)]);
        state.set_gauges_for_source(2, &[("note_cache_len", 5)]);
        // A later publish from source 1 replaces only its own values.
        state.set_gauges_for_source(1, &[("note_cache_len", 7)]);

        let gauges = state.aggregated_gauges();
        assert!(gauges.contains(&("note_cache_len", 12)));
        assert!(gauges.contains(&("note_cache_bytes", 100)));
    }

    #[test]
    fn removed_source_disappears_from_totals() {
        let mut state = enabled_state();
        state.set_gauges_for_source(1, &[("note_cache_len", 10)]);
        state.set_gauges_for_source(2, &[("note_cache_len", 5)]);
        state.remove_gauges_for_source(1);
        assert_eq!(state.aggregated_gauges(), vec![("note_cache_len", 5)]);
        state.remove_gauges_for_source(2);
        assert!(state.aggregated_gauges().is_empty());
    }

    #[test]
    fn reset_clears_stages_and_gauges() {
        let mut state = enabled_state();
        state.record_stage("note_wrap_sync", Duration::from_millis(1), true);
        state.set_gauges_for_source(1, &[("note_cache_len", 10)]);
        state.clear_stats();
        assert!(state.stages.is_empty());
        assert!(state.aggregated_gauges().is_empty());
    }

    #[test]
    fn disabled_state_ignores_gauge_publishes() {
        let mut state = InputDiagnosticsState::default();
        state.set_gauges_for_source(1, &[("note_cache_len", 10)]);
        assert!(state.aggregated_gauges().is_empty());
    }
}
