//! What the session takes from the page: a clock, a spawner, an event
//! sink and its settings.

use crate::link::WsLink;
use std::cell::RefCell;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;
use thinkterm_session::clock::{Clock, Timestamp};
use thinkterm_session::host::{
    DetachedFuture, HostConfig, HostPaneId, ImageDomainKey, SessionEvents, SessionHost, Spawner,
};

pub type LocalFuture<T> = Pin<Box<dyn Future<Output = T> + 'static>>;

pub struct WebClock {
    performance: web_sys::Performance,
}

impl WebClock {
    pub fn new() -> Self {
        let performance = web_sys::window()
            .and_then(|w| w.performance())
            .expect("performance.now is a web standard");
        Self { performance }
    }
}

impl Clock for WebClock {
    fn now(&self) -> Timestamp {
        Timestamp::from_micros((self.performance.now() * 1000.0) as u64)
    }
    fn wall_millis(&self) -> u64 {
        js_sys::Date::now() as u64
    }
}

pub struct LocalSpawner;

impl Spawner for LocalSpawner {
    fn spawn_detached(&self, fut: DetachedFuture) {
        wasm_bindgen_futures::spawn_local(fut);
    }
}

/// The session's notifications, folded into "the page needs a paint" plus
/// the few facts the page shows.
#[derive(Default)]
pub struct WebEvents {
    /// The panes with news since the last frame, by the id the page
    /// keys them on (the server's pane id).
    dirty: RefCell<std::collections::HashSet<HostPaneId>>,
    wake: RefCell<Option<Rc<dyn Fn()>>>,
}

impl WebEvents {
    /// `wake` runs on every event that changes what the page should show;
    /// it schedules a frame.
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

impl SessionEvents for WebEvents {
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

pub struct WebConfig {
    rules: Arc<Vec<termwiz::hyperlink::Rule>>,
}

impl Default for WebConfig {
    fn default() -> Self {
        Self {
            rules: Arc::new(Vec::new()),
        }
    }
}

impl HostConfig for WebConfig {
    fn hyperlink_rules(&self) -> Arc<Vec<termwiz::hyperlink::Rule>> {
        Arc::clone(&self.rules)
    }
    fn fetch_rate_per_second(&self) -> u32 {
        // The desktop's default mux_output_parser... no: its default
        // ratelimit for line fetches is 10 per second per pane.
        10
    }
}

pub struct WebHost {
    pub clock: WebClock,
    pub spawner: LocalSpawner,
    pub events: WebEvents,
    pub link: WsLink,
    pub config: WebConfig,
}

impl SessionHost for WebHost {
    type Clock = WebClock;
    type Spawner = LocalSpawner;
    type Events = WebEvents;
    type Link = WsLink;
    type Config = WebConfig;

    fn clock(&self) -> &WebClock {
        &self.clock
    }
    fn spawner(&self) -> &LocalSpawner {
        &self.spawner
    }
    fn events(&self) -> &WebEvents {
        &self.events
    }
    fn link(&self) -> &WsLink {
        &self.link
    }
    fn config(&self) -> &WebConfig {
        &self.config
    }
    fn image_domain(&self) -> ImageDomainKey {
        1
    }
}
