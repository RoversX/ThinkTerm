//! The core thread as the App's [`Platform`]: a monotonic clock, timers
//! the core loop fires, tasks on the thread's own executor, the frame
//! request that reaches the shell, and the shell's services through
//! [`Notify`].

use crate::Notify;
use futures::executor::LocalSpawner;
use futures::task::LocalSpawnExt;
use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};
use thinkterm_web::platform::{LocalFuture, Platform, Timeout, Viewport};

struct Timer {
    due: Instant,
    cb: Box<dyn FnOnce()>,
}

struct Interval {
    next: Instant,
    every: Duration,
    cb: Box<dyn FnMut()>,
}

/// Milliseconds since the process's first look at the clock. The App's
/// frame budget takes its deadline from the platform and checks it in
/// the glyph cache, so both must read this one clock.
pub fn monotonic_ms() -> f64 {
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_secs_f64() * 1000.0
}

pub struct MobilePlatform {
    spawner: LocalSpawner,
    notify: Arc<dyn Notify>,
    timers: Rc<RefCell<BTreeMap<u64, Timer>>>,
    next_timer: Cell<u64>,
    intervals: RefCell<Vec<Interval>>,
    frame: RefCell<Option<Box<dyn Fn()>>>,
    frame_requested: Cell<bool>,
    viewport: Cell<Viewport>,
    rng: Cell<u64>,
    /// The last size handed to `set_backing_size`, so a phone that resizes
    /// its surface itself can tell a real change from a repeat.
    backing: Cell<(u32, u32)>,
}

impl MobilePlatform {
    pub fn new(spawner: LocalSpawner, notify: Arc<dyn Notify>) -> Self {
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x9E37_79B9_7F4A_7C15)
            | 1;
        Self {
            spawner,
            notify,
            timers: Rc::new(RefCell::new(BTreeMap::new())),
            next_timer: Cell::new(0),
            intervals: RefCell::new(Vec::new()),
            frame: RefCell::new(None),
            frame_requested: Cell::new(false),
            viewport: Cell::new(Viewport {
                left: 0.0,
                top: 0.0,
                width: 1.0,
                height: 1.0,
                dpr: 1.0,
            }),
            rng: Cell::new(seed),
            backing: Cell::new((0, 0)),
        }
    }

    /// The surface's size in device pixels and its scale, from the shell.
    pub fn set_viewport(&self, width: u32, height: u32, scale: f64) {
        self.viewport.set(Viewport {
            left: 0.0,
            top: 0.0,
            width: width as f64 / scale,
            height: height as f64 / scale,
            dpr: scale,
        });
    }

    /// The soonest a timer is due, for the core loop's wait.
    pub fn next_deadline(&self) -> Option<Instant> {
        let timers = self.timers.borrow().values().map(|t| t.due).min();
        let intervals = self.intervals.borrow().iter().map(|i| i.next).min();
        match (timers, intervals) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    /// Run every timer that is due. Callbacks run outside the borrows, so
    /// they may set timers of their own.
    pub fn fire_due(&self) {
        let now = Instant::now();
        let due: Vec<u64> = self.timers.borrow().iter()
            .filter_map(|(id, timer)| (timer.due <= now).then_some(*id)).collect();
        for id in due {
            // A callback can cancel another wake that was already due.
            let timer = self.timers.borrow_mut().remove(&id);
            if let Some(timer) = timer {
                (timer.cb)();
            }
        }
        let mut ready: Vec<Interval> = {
            let mut intervals = self.intervals.borrow_mut();
            let (ready, later): (Vec<_>, Vec<_>) = intervals.drain(..).partition(|i| i.next <= now);
            *intervals = later;
            ready
        };
        for interval in ready.iter_mut() {
            (interval.cb)();
            interval.next = now + interval.every;
        }
        self.intervals.borrow_mut().extend(ready);
    }

    /// The shell's display callback: paint if a frame was asked for.
    pub fn run_frame(&self) -> bool {
        if !self.frame_requested.replace(false) {
            return false;
        }
        if let Some(frame) = self.frame.borrow().as_ref() {
            frame();
            true
        } else {
            false
        }
    }

    pub fn frame_pending(&self) -> bool {
        self.frame_requested.get()
    }

    /// Drop every timer and interval: they were an App's, and it is gone.
    pub fn clear_timers(&self) {
        let timers = std::mem::take(&mut *self.timers.borrow_mut());
        drop(timers);
        self.intervals.borrow_mut().clear();
    }
}

impl Platform for MobilePlatform {
    fn monotonic_ms(&self) -> f64 {
        monotonic_ms()
    }

    fn wall_ms(&self) -> f64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as f64)
            .unwrap_or(0.0)
    }

    fn random_u32(&self) -> u32 {
        // xorshift64*: plenty for ids, and no dependency.
        let mut x = self.rng.get();
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.rng.set(x);
        (x.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 32) as u32
    }

    fn units_per_inch(&self) -> f64 {
        72.0
    }

    fn spawn(&self, fut: LocalFuture<()>) {
        if let Err(err) = self.spawner.spawn_local(fut) {
            log::error!("the core executor refused a task: {err}");
        }
    }

    fn set_timeout(&self, delay_ms: f64, cb: Box<dyn FnOnce()>) {
        let id = self.next_timer.get();
        self.next_timer.set(id.checked_add(1).expect("timer id exhausted"));
        self.timers.borrow_mut().insert(id, Timer {
            due: Instant::now() + Duration::from_secs_f64(delay_ms.max(0.0).min(i32::MAX as f64) / 1000.0),
            cb,
        });
    }

    fn cancellable_timeout(&self, delay_ms: f64, cb: Box<dyn FnOnce()>) -> Timeout {
        let id = self.next_timer.get();
        self.set_timeout(delay_ms, cb);
        let timers = Rc::downgrade(&self.timers);
        Timeout::new(move || {
            if let Some(timers) = timers.upgrade() {
                let timer = timers.borrow_mut().remove(&id);
                drop(timer);
            }
        })
    }

    fn set_interval(&self, every_ms: f64, cb: Box<dyn FnMut()>) {
        let every = Duration::from_secs_f64(every_ms.max(1.0) / 1000.0);
        self.intervals.borrow_mut().push(Interval {
            next: Instant::now() + every,
            every,
            cb,
        });
    }

    fn set_frame_handler(&self, cb: Box<dyn Fn()>) {
        *self.frame.borrow_mut() = Some(cb);
    }

    fn request_frame(&self) {
        if self.frame_requested.replace(true) {
            return;
        }
        self.notify.on_frame_needed();
    }

    fn viewport(&self) -> Viewport {
        self.viewport.get()
    }

    fn set_backing_size(&self, width: u32, height: u32) -> bool {
        // The shell owns the surface's size; the App only learns it. A
        // change is still worth a prompt paint.
        let changed = self.backing.replace((width, height)) != (width, height);
        changed
    }

    fn is_mobile(&self) -> bool {
        true
    }

    fn bare_lone_pane(&self) -> bool {
        true
    }

    fn set_title(&self, title: &str) {
        self.notify.on_title(title.to_string());
    }

    fn clipboard_write(&self, text: &str) {
        self.notify.on_clipboard(text.to_string());
    }

    fn focus_input(&self) {
        self.notify.on_focus_input();
    }

    fn set_cursor(&self, _cursor: &str) {}

    fn bell(&self) {
        self.notify.on_bell();
    }

    fn set_ime_anchor(&self, anchor: thinkterm_web::ime::Anchor) {
        self.notify
            .on_ime_anchor(anchor.left, anchor.top, anchor.width, anchor.height);
    }

    fn publish(&self, key: &str, value: &str) {
        self.notify.on_published(key.to_string(), value.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct QuietNotify;
    impl Notify for QuietNotify {
        fn on_frame_needed(&self) {}
        fn on_status(&self, _: String) {}
        fn on_log(&self, _: String) {}
        fn on_title(&self, _: String) {}
        fn on_change(&self) {}
        fn on_clipboard(&self, _: String) {}
        fn on_focus_input(&self) {}
        fn on_ime_anchor(&self, _: f64, _: f64, _: f64, _: f64) {}
        fn on_host_key_required(&self, _: String, _: String) {}
        fn on_published(&self, _: String, _: String) {}
        fn on_bell(&self) {}
        fn on_preview(&self, _: u32, _: String) {}
    }

    fn platform() -> Rc<MobilePlatform> {
        let pool = futures::executor::LocalPool::new();
        Rc::new(MobilePlatform::new(pool.spawner(), Arc::new(QuietNotify)))
    }

    #[test]
    fn cancelled_wakes_release_captures_and_leave_no_deadline() {
        let platform = platform();
        for _ in 0..256 {
            let captured = Rc::new(());
            let weak = Rc::downgrade(&captured);
            let timer = platform.cancellable_timeout(60_000.0, Box::new(move || {
                drop(captured);
                panic!("cancelled wake fired");
            }));
            assert!(platform.next_deadline().is_some());
            drop(timer);
            assert!(weak.upgrade().is_none());
            assert!(platform.next_deadline().is_none());
        }
        platform.fire_due();
    }

    #[test]
    fn a_due_callback_can_cancel_another_due_wake() {
        let platform = platform();
        let slot = Rc::new(RefCell::new(None::<Timeout>));
        let cancel = Rc::clone(&slot);
        platform.set_timeout(0.0, Box::new(move || {
            let timer = cancel.borrow_mut().take();
            drop(timer);
        }));
        *slot.borrow_mut() = Some(platform.cancellable_timeout(0.0, Box::new(|| {
            panic!("cancelled wake already in the due list fired");
        })));
        platform.fire_due();
        assert!(platform.next_deadline().is_none());
    }

    #[test]
    fn callbacks_can_rearm_and_retirement_can_drop_nested_handles() {
        let platform = platform();
        let calls = Rc::new(Cell::new(0));
        let next = Rc::clone(&calls);
        let weak = Rc::downgrade(&platform);
        platform.set_timeout(0.0, Box::new(move || {
            weak.upgrade().unwrap().set_timeout(0.0, Box::new(move || {
                next.set(next.get() + 1);
            }));
        }));
        platform.fire_due();
        assert_eq!(calls.get(), 0, "new wakes wait until the next tick");
        platform.fire_due();
        assert_eq!(calls.get(), 1);
        let timer = platform.cancellable_timeout(60_000.0, Box::new(|| panic!("retired wake fired")));
        platform.set_timeout(60_000.0, Box::new(move || drop(timer)));
        platform.clear_timers();
        assert!(platform.next_deadline().is_none());
    }
}
