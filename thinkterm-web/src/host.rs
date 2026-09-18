//! The session layer's view of the App's platform: a clock, a spawner and
//! an event sink over a [`Platform`], and the link. One type for every
//! client; only `P` and `L` differ.

use crate::platform::{Link, Platform};
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use thinkterm_session::clock::{Clock, Timestamp};
use thinkterm_session::host::{
    DetachedFuture, HostConfig, HostPaneId, ImageDomainKey, SessionEvents, SessionHost, Spawner,
};

pub struct PlatformClock<P: Platform>(Rc<P>);

impl<P: Platform> Clock for PlatformClock<P> {
    fn now(&self) -> Timestamp {
        Timestamp::from_micros((self.0.monotonic_ms() * 1000.0) as u64)
    }

    fn wall_millis(&self) -> u64 {
        self.0.wall_ms() as u64
    }
}

pub struct PlatformSpawner<P: Platform>(Rc<P>);

impl<P: Platform> Spawner for PlatformSpawner<P> {
    fn spawn_detached(&self, fut: DetachedFuture) {
        self.0.spawn(fut);
    }
}

/// Which panes had output since the last frame, and a wake to ask for
/// one when the first arrives.
#[derive(Default)]
pub struct Events {
    dirty: RefCell<std::collections::HashSet<HostPaneId>>,
    wake: RefCell<Option<Rc<dyn Fn()>>>,
}

impl Events {
    pub fn set_wake(&self, wake: Rc<dyn Fn()>) {
        *self.wake.borrow_mut() = Some(wake);
    }

    pub fn take_dirty(&self) -> std::collections::HashSet<HostPaneId> {
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

pub struct AppHost<P: Platform, L: Link> {
    pub clock: PlatformClock<P>,
    pub spawner: PlatformSpawner<P>,
    pub events: Events,
    pub link: L,
    pub config: Config,
}

impl<P: Platform, L: Link> AppHost<P, L> {
    pub fn new(platform: Rc<P>, link: L) -> Self {
        Self {
            clock: PlatformClock(Rc::clone(&platform)),
            spawner: PlatformSpawner(platform),
            events: Events::default(),
            link,
            config: Config::default(),
        }
    }
}

impl<P: Platform, L: Link> SessionHost for AppHost<P, L> {
    type Clock = PlatformClock<P>;
    type Spawner = PlatformSpawner<P>;
    type Events = Events;
    type Link = L;
    type Config = Config;

    fn clock(&self) -> &PlatformClock<P> {
        &self.clock
    }
    fn spawner(&self) -> &PlatformSpawner<P> {
        &self.spawner
    }
    fn events(&self) -> &Events {
        &self.events
    }
    fn link(&self) -> &L {
        &self.link
    }
    fn config(&self) -> &Config {
        &self.config
    }
    fn image_domain(&self) -> ImageDomainKey {
        1
    }
}
