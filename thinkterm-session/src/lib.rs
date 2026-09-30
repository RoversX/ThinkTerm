//! The mux client session layer, shared by every ThinkTerm client.
//!
//! Everything here runs on `wasm32-unknown-unknown`: the host supplies time
//! (`Clock`), task spawning (`Spawner`), the wire (`PduLink`) and the event
//! sink (`SessionEvents`); nothing in this crate reaches a process, a file,
//! a socket, a thread or a system clock directly.

pub mod clock;
pub mod blobs;
pub mod byte_queue;
pub mod connection;
pub mod decide;
pub mod delta_queue;
pub mod host;
pub mod hydrate;
pub mod images;
pub mod input;
pub mod lines;
pub mod mouse;
pub mod pane;
pub mod pane_state;

/// What a pane's session is told once, at construction. Everything a
/// host may change at run time is asked for through `host::HostConfig`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionConfig {
    /// How many rows the line cache holds (at least 128 are kept).
    pub scrollback_lines: usize,
    /// Predict the echo of typed keys once the measured input round trip
    /// reaches this many milliseconds; `None` never predicts.
    pub local_echo_threshold_ms: Option<u64>,
    /// Paint a "since last response" overlay on a tardy pane.
    pub overlay_lag_indicator: bool,
}

/// A mutex that does not poison. The session holds its locks for a few
/// statements and never across an await; a panic inside one must not turn
/// every later lock into a panic too (which `std::sync::Mutex` would).
///
/// Some host callbacks run with a session lock held (a `pane_output` from
/// inside a delta), so a host that re-enters the session from one would
/// deadlock. Debug builds catch that loudly instead: the thread holding
/// the lock is remembered and a second `lock()` from it panics.
#[derive(Default)]
pub struct Lock<T> {
    inner: std::sync::Mutex<T>,
    #[cfg(debug_assertions)]
    holder: std::sync::Mutex<Option<std::thread::ThreadId>>,
}

/// The guard, releasing the re-entrancy record with the lock.
pub struct LockGuard<'a, T> {
    guard: Option<std::sync::MutexGuard<'a, T>>,
    #[cfg(debug_assertions)]
    holder: &'a std::sync::Mutex<Option<std::thread::ThreadId>>,
}

impl<T> Lock<T> {
    pub fn new(value: T) -> Self {
        Self {
            inner: std::sync::Mutex::new(value),
            #[cfg(debug_assertions)]
            holder: std::sync::Mutex::new(None),
        }
    }

    pub fn lock(&self) -> LockGuard<'_, T> {
        #[cfg(debug_assertions)]
        {
            let me = std::thread::current().id();
            let holder = self.holder.lock().unwrap_or_else(|p| p.into_inner());
            if *holder == Some(me) {
                panic!("the session lock was re-entered from a host callback");
            }
        }
        let guard = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        #[cfg(debug_assertions)]
        {
            *self.holder.lock().unwrap_or_else(|p| p.into_inner()) =
                Some(std::thread::current().id());
        }
        LockGuard {
            guard: Some(guard),
            #[cfg(debug_assertions)]
            holder: &self.holder,
        }
    }
}

impl<T> std::ops::Deref for LockGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        self.guard.as_ref().expect("guard is held until drop")
    }
}

impl<T> std::ops::DerefMut for LockGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        self.guard.as_mut().expect("guard is held until drop")
    }
}

impl<T> Drop for LockGuard<'_, T> {
    fn drop(&mut self) {
        #[cfg(debug_assertions)]
        {
            *self.holder.lock().unwrap_or_else(|p| p.into_inner()) = None;
        }
        self.guard.take();
    }
}

impl<T: std::fmt::Debug> std::fmt::Debug for Lock<T> {
    fn fmt(&self, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // try_lock, never lock: formatting a lock from under itself must
        // not hang.
        match self.inner.try_lock() {
            Ok(guard) => fmt.debug_tuple("Lock").field(&*guard).finish(),
            Err(_) => fmt.write_str("Lock(<held>)"),
        }
    }
}
