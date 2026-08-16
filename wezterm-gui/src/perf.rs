use std::sync::OnceLock;
use std::time::Instant;

static PERF_ENABLED: OnceLock<bool> = OnceLock::new();

pub(crate) fn enabled() -> bool {
    *PERF_ENABLED.get_or_init(|| {
        std::env::var_os("THINKTERM_PERF")
            .map(|value| value != "0" && !value.is_empty())
            .unwrap_or(false)
    })
}

pub(crate) fn now() -> Option<Instant> {
    enabled().then(Instant::now)
}

pub(crate) fn log_duration(label: &str, started: Option<Instant>) {
    if let Some(started) = started {
        log::info!("thinkterm_perf {label}={:.2?}", started.elapsed());
    }
}

pub(crate) fn log_counter(label: &str, value: impl std::fmt::Display) {
    if enabled() {
        log::info!("thinkterm_perf {label}={value}");
    }
}

thread_local! {
    static ACCUM: std::cell::RefCell<Vec<(&'static str, std::time::Duration, u64)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Add `started.elapsed()` to a named accumulator. Costless when perf is off
/// (`now()` returned None). Accumulators live until `log_accums` drains them.
pub(crate) fn accum(label: &'static str, started: Option<Instant>) {
    let Some(started) = started else { return };
    let elapsed = started.elapsed();
    ACCUM.with(|accum| {
        let mut accum = accum.borrow_mut();
        if let Some(entry) = accum.iter_mut().find(|entry| entry.0 == label) {
            entry.1 += elapsed;
            entry.2 += 1;
        } else {
            accum.push((label, elapsed, 1));
        }
    });
}

/// Count an event without timing it.
pub(crate) fn accum_count(label: &'static str) {
    if !enabled() {
        return;
    }
    ACCUM.with(|accum| {
        let mut accum = accum.borrow_mut();
        if let Some(entry) = accum.iter_mut().find(|entry| entry.0 == label) {
            entry.2 += 1;
        } else {
            accum.push((label, std::time::Duration::ZERO, 1));
        }
    });
}

/// Drop whatever has accumulated so a section can measure only itself.
pub(crate) fn reset_accums() {
    ACCUM.with(|accum| accum.borrow_mut().clear());
}

/// Emit and clear every accumulator, prefixing each label.
pub(crate) fn log_accums(prefix: &str) {
    if !enabled() {
        return;
    }
    ACCUM.with(|accum| {
        for (label, total, count) in accum.borrow_mut().drain(..) {
            log::info!("thinkterm_perf {prefix}{label}={total:.2?} {prefix}{label}_n={count}");
        }
    });
}
