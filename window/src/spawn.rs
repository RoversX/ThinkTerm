#[cfg(windows)]
use crate::os::windows::event::EventHandle;
#[cfg(target_os = "macos")]
use core_foundation::runloop::*;
use promise::spawn::{Runnable, SpawnFunc};
use std::collections::VecDeque;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
#[cfg(all(unix, not(target_os = "macos")))]
use {
    filedescriptor::{FileDescriptor, Pipe},
    std::os::unix::io::AsRawFd,
};

lazy_static::lazy_static! {
    pub(crate) static ref SPAWN_QUEUE: Arc<SpawnQueue> = Arc::new(SpawnQueue::new().expect("failed to create SpawnQueue"));
}

struct InstrumentedSpawnFunc {
    func: SpawnFunc,
    at: Instant,
}

pub(crate) struct SpawnQueue {
    spawned_funcs: Mutex<VecDeque<InstrumentedSpawnFunc>>,
    spawned_funcs_low_pri: Mutex<VecDeque<InstrumentedSpawnFunc>>,

    #[cfg(windows)]
    pub event_handle: EventHandle,

    #[cfg(all(unix, not(target_os = "macos")))]
    write: Mutex<FileDescriptor>,
    #[cfg(all(unix, not(target_os = "macos")))]
    read: Mutex<FileDescriptor>,

    #[cfg(target_os = "macos")]
    runloop_source: RunLoopSourceHandle,
}

/// A CFRunLoopSourceRef is just a CFTypeRef; signalling it from any thread is
/// documented as safe, and we never mutate through it after creation.
#[cfg(target_os = "macos")]
struct RunLoopSourceHandle(CFRunLoopSourceRef);
#[cfg(target_os = "macos")]
unsafe impl Send for RunLoopSourceHandle {}
#[cfg(target_os = "macos")]
unsafe impl Sync for RunLoopSourceHandle {}

fn schedule_with_pri(runnable: Runnable, high_pri: bool) {
    SPAWN_QUEUE.spawn_impl(
        Box::new(move || {
            runnable.run();
        }),
        high_pri,
    );
}

impl SpawnQueue {
    pub fn new() -> anyhow::Result<Self> {
        Self::new_impl()
    }

    pub fn register_promise_schedulers(&self) {
        promise::spawn::set_schedulers(
            Box::new(|runnable| {
                schedule_with_pri(runnable, true);
            }),
            Box::new(|runnable| {
                schedule_with_pri(runnable, false);
            }),
        );
    }

    pub fn run(&self) -> bool {
        self.run_impl()
    }

    // This needs to be a separate function from the loop in `run`
    // in order for the lock to be released before we call the
    // returned function
    fn pop_func(&self) -> Option<SpawnFunc> {
        if let Some(func) = self.spawned_funcs.lock().unwrap().pop_front() {
            metrics::histogram!("executor.spawn_delay").record(func.at.elapsed());
            Some(func.func)
        } else if let Some(func) = self.spawned_funcs_low_pri.lock().unwrap().pop_front() {
            metrics::histogram!("executor.spawn_delay.low_pri").record(func.at.elapsed());
            Some(func.func)
        } else {
            None
        }
    }

    fn queue_func(&self, f: SpawnFunc, high_pri: bool) {
        let f = InstrumentedSpawnFunc {
            func: f,
            at: Instant::now(),
        };
        if high_pri {
            self.spawned_funcs.lock().unwrap()
        } else {
            self.spawned_funcs_low_pri.lock().unwrap()
        }
        .push_back(f);
    }

    fn has_any_queued(&self) -> bool {
        !self.spawned_funcs.lock().unwrap().is_empty()
            || !self.spawned_funcs_low_pri.lock().unwrap().is_empty()
    }

    fn run_func(func: SpawnFunc) {
        if let Err(payload) = catch_unwind(AssertUnwindSafe(func)) {
            if let Some(message) = payload.downcast_ref::<&str>() {
                log::error!("spawn queue callback panicked: {message}");
            } else if let Some(message) = payload.downcast_ref::<String>() {
                log::error!("spawn queue callback panicked: {message}");
            } else {
                log::error!("spawn queue callback panicked");
            }
        }
    }
}

#[cfg(windows)]
impl SpawnQueue {
    fn new_impl() -> anyhow::Result<Self> {
        let spawned_funcs = Mutex::new(VecDeque::new());
        let spawned_funcs_low_pri = Mutex::new(VecDeque::new());
        let event_handle = EventHandle::new_manual_reset().expect("EventHandle creation failed");
        Ok(Self {
            spawned_funcs,
            spawned_funcs_low_pri,
            event_handle,
        })
    }

    fn spawn_impl(&self, f: SpawnFunc, high_pri: bool) {
        self.queue_func(f, high_pri);
        self.event_handle.set_event();
    }

    fn run_impl(&self) -> bool {
        self.event_handle.reset_event();
        while let Some(func) = self.pop_func() {
            Self::run_func(func);
        }
        self.has_any_queued()
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
impl SpawnQueue {
    fn new_impl() -> anyhow::Result<Self> {
        // On linux we have a slightly sloppy wakeup mechanism;
        // we have a non-blocking pipe that we can use to get
        // woken up after some number of enqueues.  We don't
        // guarantee a 1:1 enqueue to wakeup with this mechanism
        // but in practical terms it does guarantee a wakeup
        // if the main thread is asleep and we enqueue some
        // number of items.
        // We can't affort to use a blocking pipe for the wakeup
        // because the write needs to hold a mutex and that
        // can block reads as well as other writers.
        let mut pipe = Pipe::new()?;
        pipe.write.set_non_blocking(true)?;
        pipe.read.set_non_blocking(true)?;
        Ok(Self {
            spawned_funcs: Mutex::new(VecDeque::new()),
            spawned_funcs_low_pri: Mutex::new(VecDeque::new()),
            write: Mutex::new(pipe.write),
            read: Mutex::new(pipe.read),
        })
    }

    fn spawn_impl(&self, f: SpawnFunc, high_pri: bool) {
        use std::io::Write;

        self.queue_func(f, high_pri);
        while let Err(err) = self.write.lock().unwrap().write(b"x") {
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            log::warn!("Failed to signal spawn queue pipe: {:#}", err);
            break;
        }
    }

    fn run_impl(&self) -> bool {
        // On linux we only ever process one at at time, so that
        // we can return to the main loop and process messages
        // from the X server
        if let Some(func) = self.pop_func() {
            Self::run_func(func);
        }

        // try to drain the pipe.
        // We do this regardless of whether we popped an item
        // so that we avoid being in a perpetually signalled state.
        // It is ok if we completely drain the pipe because the
        // main loop uses the return value to set the sleep
        // interval and will unconditionally call us on each
        // iteration.
        let mut byte = [0u8; 64];
        use std::io::Read;
        self.read.lock().unwrap().read(&mut byte).ok();

        self.has_any_queued()
    }

    pub(crate) fn raw_fd(&self) -> std::os::unix::io::RawFd {
        self.read.lock().unwrap().as_raw_fd()
    }
}

#[cfg(target_os = "macos")]
impl SpawnQueue {
    fn new_impl() -> anyhow::Result<Self> {
        let spawned_funcs = Mutex::new(VecDeque::new());
        let spawned_funcs_low_pri = Mutex::new(VecDeque::new());

        // Safety-net drain on every runloop activity, in case the source
        // below is somehow not serviced.
        let observer = unsafe {
            CFRunLoopObserverCreate(
                std::ptr::null(),
                kCFRunLoopAllActivities,
                1,
                0,
                SpawnQueue::trigger,
                std::ptr::null_mut(),
            )
        };
        unsafe {
            CFRunLoopAddObserver(CFRunLoopGetMain(), observer, kCFRunLoopCommonModes);
        }

        // The primary wakeup is a version-0 runloop source: unlike a bare
        // CFRunLoopWakeUp (which can be swallowed if it lands while the loop
        // is between activities), a signalled source stays signalled until
        // the loop services it, so queued work can never be stranded until
        // the next user-input event.
        let mut context = CFRunLoopSourceContext {
            version: 0,
            info: std::ptr::null_mut(),
            retain: None,
            release: None,
            copyDescription: None,
            equal: None,
            hash: None,
            schedule: None,
            cancel: None,
            perform: SpawnQueue::perform,
        };
        let source = unsafe { CFRunLoopSourceCreate(std::ptr::null(), 0, &mut context) };
        unsafe {
            CFRunLoopAddSource(CFRunLoopGetMain(), source, kCFRunLoopCommonModes);
        }

        Ok(Self {
            spawned_funcs,
            spawned_funcs_low_pri,
            runloop_source: RunLoopSourceHandle(source),
        })
    }

    extern "C" fn trigger(
        _observer: *mut __CFRunLoopObserver,
        _: CFRunLoopActivity,
        _: *mut std::ffi::c_void,
    ) {
        if SPAWN_QUEUE.run() {
            SPAWN_QUEUE.signal_and_wake();
        }
    }

    extern "C" fn perform(_info: *const std::ffi::c_void) {
        if SPAWN_QUEUE.run() {
            SPAWN_QUEUE.signal_and_wake();
        }
    }

    fn signal_and_wake(&self) {
        unsafe {
            CFRunLoopSourceSignal(self.runloop_source.0);
            CFRunLoopWakeUp(CFRunLoopGetMain());
        }
    }

    fn spawn_impl(&self, f: SpawnFunc, high_pri: bool) {
        self.queue_func(f, high_pri);
        self.signal_and_wake();
    }

    fn run_impl(&self) -> bool {
        // Drain with a small time budget rather than one task per runloop
        // observer callback: under heavy load the one-at-a-time policy needs
        // a full wakeup round-trip per task, and if any wakeup is coalesced
        // the whole queue (paints, animations, output notifications) stalls
        // until the next user input event. Keep the budget small so native
        // event handling still interleaves.
        let deadline = Instant::now() + Duration::from_millis(2);
        while let Some(func) = self.pop_func() {
            Self::run_func(func);
            if Instant::now() >= deadline {
                break;
            }
        }
        self.has_any_queued()
    }
}
