//! Where client connections run.
//!
//! The server's main thread is where the mux lives: panes are spawned,
//! split and killed there, and every request that changes the mux hops to
//! it. Until now each client connection ran there too -- decoding
//! requests, encoding replies and output pushes, answering Ping and the
//! version handshake -- so a main thread held up by one pane's work held
//! up every client's connection with it, and from outside the server
//! simply stopped answering.
//!
//! Connections now run here, on a few threads of their own. What has to
//! touch the mux still hops to the main thread the way it always did; what
//! does not -- the handshake, a Ping, the encoding of a reply -- is
//! answered regardless of what the main thread is doing. A stuck pane is
//! then a stuck pane, visible as such, rather than a server that has gone
//! away.

use futures::FutureExt;
use smol::Executor;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::sync::{Arc, LazyLock};

static EXECUTOR: LazyLock<Arc<Executor<'static>>> = LazyLock::new(|| {
    let executor = Arc::new(Executor::new());
    // Two at least: a reply being encoded for one client must not hold
    // the Ping of another. More than a few buys nothing; the mux work
    // itself is still serialised on the main thread.
    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(2)
        .clamp(2, 4);
    for n in 0..threads {
        let executor = Arc::clone(&executor);
        std::thread::Builder::new()
            .name(format!("mux-conn-{n}"))
            .spawn(move || smol::block_on(executor.run(std::future::pending::<()>())))
            .expect("spawning a connection thread");
    }
    executor
});

/// Run `future` -- one client connection, start to finish -- on the
/// connection threads.
pub fn spawn<F>(future: F)
where
    F: Future<Output = ()> + Send + 'static,
{
    // The executor catches a task's panic and hands it to whoever holds
    // the task; detached, that would be nobody, and the connection would
    // simply vanish. Say so, at least.
    EXECUTOR
        .spawn(async move {
            if let Err(payload) = AssertUnwindSafe(future).catch_unwind().await {
                let message = payload
                    .downcast_ref::<String>()
                    .map(String::as_str)
                    .or_else(|| payload.downcast_ref::<&str>().copied())
                    .unwrap_or("(no message)");
                log::error!("a client connection panicked and was dropped: {message}");
            }
        })
        .detach();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_connection_runs_off_the_calling_thread() {
        let (tx, rx) = std::sync::mpsc::channel();
        spawn(async move {
            tx.send(std::thread::current().id()).unwrap();
        });
        let ran_on = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("the future ran");
        assert_ne!(ran_on, std::thread::current().id());
    }
}
