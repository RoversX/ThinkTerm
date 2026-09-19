//! The core thread as the App's [`Platform`]: a monotonic clock, timers
//! the core loop fires, tasks on the thread's own executor, the frame
//! request that reaches the shell, and the shell's services through
//! [`Notify`].

use crate::Notify;
use futures::executor::LocalSpawner;
use futures::task::LocalSpawnExt;
use std::cell::{Cell, RefCell};
use std::sync::Arc;
use std::time::{Duration, Instant};
use thinkterm_web::platform::{LocalFuture, Platform, Viewport};

struct Timer {
    due: Instant,
    cb: Box<dyn FnOnce()>,
}

struct Interval {
    next: Instant,
    every: Duration,
    cb: Box<dyn FnMut()>,
}

pub struct MobilePlatform {
    start: Instant,
    spawner: LocalSpawner,
    notify: Arc<dyn Notify>,
    timers: RefCell<Vec<Timer>>,
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
            start: Instant::now(),
            spawner,
            notify,
            timers: RefCell::new(Vec::new()),
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
        let timers = self.timers.borrow().iter().map(|t| t.due).min();
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
        let due: Vec<Timer> = {
            let mut timers = self.timers.borrow_mut();
            let (due, later): (Vec<_>, Vec<_>) = timers.drain(..).partition(|t| t.due <= now);
            *timers = later;
            due
        };
        for timer in due {
            (timer.cb)();
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
}

impl Platform for MobilePlatform {
    fn monotonic_ms(&self) -> f64 {
        self.start.elapsed().as_secs_f64() * 1000.0
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
        self.timers.borrow_mut().push(Timer {
            due: Instant::now() + Duration::from_secs_f64(delay_ms.max(0.0) / 1000.0),
            cb,
        });
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
