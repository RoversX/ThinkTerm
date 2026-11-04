use crate::clock::{Clock, SystemClock, Tick};
use crate::{Error, Id, Result, MAX_ROUTES};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

pub const ROUTE_BYTES: usize = 8 * 1024 * 1024;
pub const PROCESS_BYTES: usize = 32 * 1024 * 1024;
pub const ROUTE_REQUESTS: usize = 8;
pub const PROCESS_REQUESTS: usize = 32;

#[derive(Clone, Copy)]
struct Rate {
    credit: u64,
    last: Tick,
}

impl Rate {
    const UNIT: u64 = 1_000_000_000;

    fn new(now: Tick, burst: u64) -> Self {
        Self {
            credit: burst * Self::UNIT,
            last: now,
        }
    }

    fn replenished(self, now: Tick, per_second: u64, burst: u64) -> Result<Self> {
        let elapsed = now.elapsed_since(self.last)?.as_nanos();
        let added = elapsed.saturating_mul(u128::from(per_second));
        let credit = (u128::from(self.credit) + added).min(u128::from(burst * Self::UNIT));
        Ok(Self {
            credit: credit as u64,
            last: now,
        })
    }
}

struct Usage {
    bytes: usize,
    requests: usize,
    rate: Rate,
    output: Rate,
}

struct State {
    total: Usage,
    routes: HashMap<Id, Usage>,
}

struct Shared {
    clock: Arc<dyn Clock>,
    state: Mutex<State>,
}

#[derive(Clone)]
pub struct Budgets {
    shared: Arc<Shared>,
}

impl Budgets {
    pub fn process() -> Result<Self> {
        static BUDGETS: std::sync::OnceLock<Result<Budgets>> = std::sync::OnceLock::new();
        BUDGETS.get_or_init(Self::new).clone()
    }

    pub fn new() -> Result<Self> {
        Self::with_clock(SystemClock::shared())
    }

    pub(crate) fn with_clock(clock: Arc<dyn Clock>) -> Result<Self> {
        let now = clock.now()?;
        Ok(Self {
            shared: Arc::new(Shared {
                clock,
                state: Mutex::new(State {
                    total: Usage {
                        bytes: 0,
                        requests: 0,
                        rate: Rate::new(now, 80),
                        output: Rate::new(now, 8 * 1024 * 1024),
                    },
                    routes: HashMap::new(),
                }),
            }),
        })
    }

    pub fn open_route(&self, id: Id) -> Result<RouteBudget> {
        let mut state = self.shared.state.lock().map_err(|_| Error::Unavailable)?;
        if state.routes.len() >= MAX_ROUTES || state.routes.contains_key(&id) {
            return Err(Error::Busy);
        }
        let now = self.shared.clock.now()?;
        state.routes.insert(
            id,
            Usage {
                bytes: 0,
                requests: 0,
                rate: Rate::new(now, 20),
                output: Rate::new(now, 2 * 1024 * 1024),
            },
        );
        Ok(RouteBudget {
            inner: Arc::new(RouteInner {
                id,
                shared: self.shared.clone(),
            }),
        })
    }

    pub fn retained(&self) -> Result<(usize, usize)> {
        let state = self.shared.state.lock().map_err(|_| Error::Unavailable)?;
        Ok((state.total.requests, state.total.bytes))
    }
}

struct RouteInner {
    id: Id,
    shared: Arc<Shared>,
}

impl Drop for RouteInner {
    fn drop(&mut self) {
        if let Ok(mut state) = self.shared.state.lock() {
            if let Some(usage) = state.routes.remove(&self.id) {
                debug_assert_eq!((usage.requests, usage.bytes), (0, 0));
            }
        }
    }
}

#[derive(Clone)]
pub struct RouteBudget {
    inner: Arc<RouteInner>,
}

impl RouteBudget {
    pub fn charge_output(&self, bytes: usize) -> Result<()> {
        let mut state = self
            .inner
            .shared
            .state
            .lock()
            .map_err(|_| Error::Unavailable)?;
        let now = self.inner.shared.clock.now()?;
        let route = state.routes.get(&self.inner.id).ok_or(Error::Unavailable)?;
        let route_rate = route
            .output
            .replenished(now, 2 * 1024 * 1024, 2 * 1024 * 1024)?;
        let global_rate = state
            .total
            .output
            .replenished(now, 8 * 1024 * 1024, 8 * 1024 * 1024)?;
        let cost = u64::try_from(bytes)
            .ok()
            .and_then(|n| n.checked_mul(Rate::UNIT))
            .ok_or(Error::TooLarge)?;
        if cost > route_rate.credit || cost > global_rate.credit {
            return Err(Error::Busy);
        }
        state
            .routes
            .get_mut(&self.inner.id)
            .ok_or(Error::Unavailable)?
            .output = Rate {
            credit: route_rate.credit - cost,
            ..route_rate
        };
        state.total.output = Rate {
            credit: global_rate.credit - cost,
            ..global_rate
        };
        Ok(())
    }

    /// Acquired before a request allocates or queues its payload. Retain
    /// this reservation through encoding and the last pending send.
    pub fn acquire(&self, bytes: usize) -> Result<Reservation> {
        if bytes > ROUTE_BYTES {
            return Err(Error::TooLarge);
        }
        let mut state = self
            .inner
            .shared
            .state
            .lock()
            .map_err(|_| Error::Unavailable)?;
        let route = state.routes.get(&self.inner.id).ok_or(Error::Unavailable)?;
        if route.requests >= ROUTE_REQUESTS
            || state.total.requests >= PROCESS_REQUESTS
            || bytes > ROUTE_BYTES - route.bytes
            || bytes > PROCESS_BYTES - state.total.bytes
        {
            return Err(Error::Busy);
        }
        let now = self.inner.shared.clock.now()?;
        let global_rate = state.total.rate.replenished(now, 40, 80)?;
        let route_rate = route.rate.replenished(now, 10, 20)?;
        if global_rate.credit < Rate::UNIT || route_rate.credit < Rate::UNIT {
            return Err(Error::Busy);
        }
        let route = state
            .routes
            .get_mut(&self.inner.id)
            .ok_or(Error::Unavailable)?;
        route.rate = Rate {
            credit: route_rate.credit - Rate::UNIT,
            ..route_rate
        };
        route.bytes += bytes;
        route.requests += 1;
        state.total.rate = Rate {
            credit: global_rate.credit - Rate::UNIT,
            ..global_rate
        };
        state.total.bytes += bytes;
        state.total.requests += 1;
        Ok(Reservation {
            route: self.inner.clone(),
            bytes,
        })
    }
}

pub struct Reservation {
    route: Arc<RouteInner>,
    bytes: usize,
}

impl Reservation {
    /// Copies and encoded output need their own bytes, without releasing
    /// the original request's concurrency slot.
    pub fn grow(&mut self, additional: usize) -> Result<()> {
        let mut state = self
            .route
            .shared
            .state
            .lock()
            .map_err(|_| Error::Unavailable)?;
        let route = state.routes.get(&self.route.id).ok_or(Error::Unavailable)?;
        if additional > ROUTE_BYTES - route.bytes || additional > PROCESS_BYTES - state.total.bytes
        {
            return Err(Error::Busy);
        }
        state
            .routes
            .get_mut(&self.route.id)
            .ok_or(Error::Unavailable)?
            .bytes += additional;
        state.total.bytes += additional;
        self.bytes += additional;
        Ok(())
    }

    pub fn release_bytes(&mut self, bytes: usize) -> Result<()> {
        if bytes > self.bytes {
            return Err(Error::Protocol);
        }
        let mut state = self
            .route
            .shared
            .state
            .lock()
            .map_err(|_| Error::Unavailable)?;
        state
            .routes
            .get_mut(&self.route.id)
            .ok_or(Error::Unavailable)?
            .bytes -= bytes;
        state.total.bytes -= bytes;
        self.bytes -= bytes;
        Ok(())
    }

    pub fn bytes(&self) -> usize {
        self.bytes
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        if let Ok(mut state) = self.route.shared.state.lock() {
            if let Some(route) = state.routes.get_mut(&self.route.id) {
                route.bytes -= self.bytes;
                route.requests -= 1;
                state.total.bytes -= self.bytes;
                state.total.requests -= 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queued_output_keeps_its_request_and_byte_budget() {
        let budgets = Budgets::new().unwrap();
        let route = budgets.open_route(Id::from_bytes([1; 16])).unwrap();
        let mut held = route.acquire(1024).unwrap();
        held.grow(2048).unwrap();
        drop(route);
        assert_eq!(budgets.retained().unwrap(), (1, 3072));
        held.release_bytes(1024).unwrap();
        assert_eq!(budgets.retained().unwrap(), (1, 2048));
        drop(held);
        assert_eq!(budgets.retained().unwrap(), (0, 0));
        assert!(budgets.open_route(Id::from_bytes([1; 16])).is_ok());
    }

    #[test]
    fn route_and_process_limits_apply_before_allocation() {
        let budgets = Budgets::new().unwrap();
        let routes: Vec<_> = (0..5)
            .map(|id| budgets.open_route(Id::from_bytes([id; 16])).unwrap())
            .collect();
        let mut held = vec![];
        for route in &routes[..4] {
            for _ in 0..ROUTE_REQUESTS {
                held.push(route.acquire(1024 * 1024).unwrap());
            }
            assert!(matches!(route.acquire(1), Err(Error::Busy)));
        }
        assert_eq!(budgets.retained().unwrap(), (32, PROCESS_BYTES));
        assert!(matches!(routes[4].acquire(1), Err(Error::Busy)));
        assert!(matches!(held[0].grow(1), Err(Error::Busy)));
        drop(held);
        assert_eq!(budgets.retained().unwrap(), (0, 0));
    }

    #[test]
    fn short_requests_do_not_evade_rate_limits() {
        let budgets = Budgets::new().unwrap();
        let route = budgets.open_route(Id::from_bytes([1; 16])).unwrap();
        for _ in 0..20 {
            drop(route.acquire(0).unwrap());
        }
        assert!(matches!(route.acquire(0), Err(Error::Busy)));
    }
}
