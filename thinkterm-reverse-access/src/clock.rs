use crate::{Error, Result};
use std::sync::Arc;
use std::time::Duration;

/// Time in the executor's clock domain. Values never travel on the wire.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Tick(Duration);

impl Tick {
    pub fn after(self, duration: Duration) -> Result<Self> {
        self.0.checked_add(duration).map(Self).ok_or(Error::Clock)
    }

    pub fn elapsed_since(self, earlier: Self) -> Result<Duration> {
        self.0.checked_sub(earlier.0).ok_or(Error::Clock)
    }

    #[cfg(test)]
    pub(crate) fn seconds(value: u64) -> Self {
        Self(Duration::from_secs(value))
    }
}

pub trait Clock: Send + Sync {
    fn now(&self) -> Result<Tick>;
}

pub struct SystemClock {
    #[cfg(windows)]
    started: std::time::Instant,
}

impl SystemClock {
    pub fn shared() -> Arc<dyn Clock> {
        Arc::new(Self {
            #[cfg(windows)]
            started: std::time::Instant::now(),
        })
    }
}

impl Clock for SystemClock {
    fn now(&self) -> Result<Tick> {
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        {
            use nix::time::{clock_gettime, ClockId};
            #[cfg(target_os = "macos")]
            let clock = ClockId::from_raw(nix::libc::CLOCK_MONOTONIC_RAW);
            #[cfg(target_os = "linux")]
            let clock = ClockId::CLOCK_BOOTTIME;
            let value = clock_gettime(clock).map_err(|_| Error::Clock)?;
            let seconds = u64::try_from(value.tv_sec()).map_err(|_| Error::Clock)?;
            let nanos = u32::try_from(value.tv_nsec()).map_err(|_| Error::Clock)?;
            if nanos >= 1_000_000_000 {
                return Err(Error::Clock);
            }
            Ok(Tick(Duration::new(seconds, nanos)))
        }
        #[cfg(windows)]
        {
            // Rust's Windows Instant uses QueryPerformanceCounter. Windows
            // includes sleep in QPC; this is not assumed for other targets.
            std::time::Instant::now()
                .checked_duration_since(self.started)
                .map(Tick)
                .ok_or(Error::Clock)
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
        Err(Error::Clock)
    }
}
