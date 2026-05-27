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
