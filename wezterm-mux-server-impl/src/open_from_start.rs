//! `thinkterm start` handing the folder and program it names to the window
//! already running (`OpenFromStart`). Only a window's process can open
//! them, so it installs the handler for the server it runs for the command
//! line; a server with no window says it has none.

use std::sync::OnceLock;

type Handler = fn(codec::OpenFromStart) -> anyhow::Result<()>;

static HANDLER: OnceLock<Handler> = OnceLock::new();

/// Set by a window's process, once.
pub fn install(handler: Handler) {
    let _ = HANDLER.set(handler);
}

/// Hand `request` to the window, on the main thread.
pub(crate) fn open(request: codec::OpenFromStart) -> anyhow::Result<()> {
    match HANDLER.get() {
        Some(handler) => handler(request),
        None => anyhow::bail!("this server has no window to open it in"),
    }
}
