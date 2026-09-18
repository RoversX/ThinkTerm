//! The session layer's view of the core thread: a monotonic clock, a
//! spawner onto the thread's own executor, the dirty-pane events that turn
//! into frame requests, and the configuration the browser client uses.

use crate::link::SshLink;
use futures::executor::LocalSpawner;
use futures::task::LocalSpawnExt;
use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;
use thinkterm_session::clock::{Clock, Timestamp};
use thinkterm_session::host::{
    DetachedFuture, HostConfig, HostPaneId, ImageDomainKey, SessionEvents, SessionHost, Spawner,
};

#[derive(Clone, Copy)]
pub struct MobileClock {
    start: Instant,
}

impl MobileClock {
    pub fn new() -> Self {
        Self {
            start: Instant::now(),
        }
    }

    pub fn now_ms(&self) -> f64 {
        self.start.elapsed().as_secs_f64() * 1000.0
    }
}

impl Clock for MobileClock {
    fn now(&self) -> Timestamp {
        Timestamp::from_micros(self.start.elapsed().as_micros() as u64)
    }

    fn wall_millis(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }
}

/// Futures the session detaches run on the core thread's `LocalPool`,
/// which the core loop drives after every event it handles.
pub struct Spawn(pub LocalSpawner);

impl Spawner for Spawn {
    fn spawn_detached(&self, fut: DetachedFuture) {
        if let Err(err) = self.0.spawn_local(fut) {
            log::error!("the core executor refused a task: {err}");
        }
    }
}

#[derive(Default)]
pub struct Events {
    dirty: RefCell<HashSet<HostPaneId>>,
    wake: RefCell<Option<Rc<dyn Fn()>>>,
}

impl Events {
    pub fn set_wake(&self, wake: Rc<dyn Fn()>) {
        *self.wake.borrow_mut() = Some(wake);
    }

    pub fn take_dirty(&self) -> HashSet<HostPaneId> {
        std::mem::take(&mut *self.dirty.borrow_mut())
    }

    fn mark(&self, pane: HostPaneId) {
        self.dirty.borrow_mut().insert(pane);
        if let Some(wake) = self.wake.borrow().clone() {
            wake();
        }
    }
}

impl SessionEvents for Events {
    fn pane_output(&self, pane: HostPaneId) {
        self.mark(pane);
    }
    fn alert(&self, _pane: HostPaneId, _alert: wezterm_term::Alert) {}
    fn agent_status_changed(&self, _pane: HostPaneId) {}
    fn pane_removed(&self, pane: HostPaneId) {
        self.mark(pane);
    }
    fn pane_focused(&self, _pane: HostPaneId) {}
    fn input_recorded(&self) {}
}

pub struct Config {
    rules: Arc<Vec<termwiz::hyperlink::Rule>>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            rules: Arc::new(Vec::new()),
        }
    }
}

impl HostConfig for Config {
    fn hyperlink_rules(&self) -> Arc<Vec<termwiz::hyperlink::Rule>> {
        Arc::clone(&self.rules)
    }
    fn fetch_rate_per_second(&self) -> u32 {
        10
    }
    fn scrollback_lookahead_screens(&self) -> usize {
        1
    }
    fn warm_scrollback(&self) -> bool {
        false
    }
}

pub struct MobileHost {
    pub clock: MobileClock,
    pub spawner: Spawn,
    pub events: Events,
    pub link: SshLink,
    pub config: Config,
}

impl SessionHost for MobileHost {
    type Clock = MobileClock;
    type Spawner = Spawn;
    type Events = Events;
    type Link = SshLink;
    type Config = Config;

    fn clock(&self) -> &MobileClock {
        &self.clock
    }
    fn spawner(&self) -> &Spawn {
        &self.spawner
    }
    fn events(&self) -> &Events {
        &self.events
    }
    fn link(&self) -> &SshLink {
        &self.link
    }
    fn config(&self) -> &Config {
        &self.config
    }
    fn image_domain(&self) -> ImageDomainKey {
        1
    }
}
