//! Time as the session layer sees it: a monotonic `Timestamp` the host
//! supplies. The crate never asks the system for the time itself.
use std::ops::Add;
use std::time::Duration;

/// Microseconds since an origin the host chose; only differences matter.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Timestamp(u64);

impl Timestamp {
    pub const ZERO: Self = Self(0);

    pub const fn from_micros(micros: u64) -> Self {
        Self(micros)
    }

    pub const fn as_micros(self) -> u64 {
        self.0
    }

    /// Zero when `earlier` is not earlier, like `Instant`'s.
    pub fn saturating_duration_since(self, earlier: Self) -> Duration {
        Duration::from_micros(self.0.saturating_sub(earlier.0))
    }

    pub fn saturating_add(self, d: Duration) -> Self {
        Self(
            self.0
                .saturating_add(d.as_micros().min(u64::MAX as u128) as u64),
        )
    }
}

/// The host's clocks: a monotonic one for deadlines and a wall clock for
/// `InputSerial`, which the wire defines as millis since the unix epoch.
pub trait Clock {
    fn now(&self) -> Timestamp;
    fn wall_millis(&self) -> u64;
}

/// A token bucket: `rate` admits a second, at most `rate` at once, full
/// to begin with. Replaces the desktop's governor-backed limiter, whose
/// per-second quota behaves the same way for unit checks.
#[derive(Debug, Clone)]
pub struct RateLimiter {
    rate: u32,
    tokens: f64,
    last: Timestamp,
}

impl RateLimiter {
    pub fn new(rate: u32, now: Timestamp) -> Self {
        Self {
            rate: rate.max(1),
            tokens: rate.max(1) as f64,
            last: now,
        }
    }

    /// Admit `amount` now, or refuse. `rate` is the current setting: a
    /// changed value rebuilds the bucket full (a reload changed the
    /// limit), an unchanged one only refills what time has earned.
    pub fn admit(&mut self, rate: u32, amount: u32, now: Timestamp) -> bool {
        let rate = rate.max(1);
        if rate != self.rate {
            *self = Self::new(rate, now);
        }
        let elapsed = now.saturating_duration_since(self.last).as_secs_f64();
        self.tokens = (self.tokens + elapsed * self.rate as f64).min(self.rate as f64);
        self.last = now;
        if self.tokens >= amount as f64 {
            self.tokens -= amount as f64;
            true
        } else {
            false
        }
    }
}

impl Add<Duration> for Timestamp {
    type Output = Self;
    fn add(self, d: Duration) -> Self {
        self.saturating_add(d)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(ms: u64) -> Timestamp {
        Timestamp::from_micros(ms * 1000)
    }

    #[test]
    fn n_admits_pass_then_the_next_is_denied_at_the_same_instant() {
        let mut limiter = RateLimiter::new(3, at(0));
        assert!(limiter.admit(3, 1, at(0)));
        assert!(limiter.admit(3, 1, at(0)));
        assert!(limiter.admit(3, 1, at(0)), "the burst is the whole rate");
        assert!(!limiter.admit(3, 1, at(0)), "and then it is spent");
        assert!(
            limiter.admit(3, 1, at(334)),
            "a third of a second earns one back"
        );
    }

    #[test]
    fn a_changed_rate_rebuilds_the_bucket_full() {
        let mut limiter = RateLimiter::new(1, at(0));
        assert!(limiter.admit(1, 1, at(0)));
        assert!(!limiter.admit(1, 1, at(0)));
        assert!(
            limiter.admit(5, 1, at(0)),
            "the reload starts a full bucket"
        );
    }

    #[test]
    fn an_unchanged_rate_does_not_reset_the_bucket() {
        let mut limiter = RateLimiter::new(2, at(0));
        assert!(limiter.admit(2, 1, at(0)));
        assert!(limiter.admit(2, 1, at(0)));
        assert!(
            !limiter.admit(2, 1, at(0)),
            "still spent: no rebuild without a change"
        );
    }
}
