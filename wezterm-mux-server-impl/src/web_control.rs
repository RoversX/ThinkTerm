//! The handle the session handler uses to start and stop the web listener.
//!
//! The listener lives in the `wezterm-mux-server` binary, and the binary
//! depends on this crate rather than the other way round, so it hands these
//! in at startup. A build that never installs them -- a test, or a client
//! linking this crate -- answers "no web listener here" rather than
//! pretending to have one.

use std::sync::OnceLock;

pub struct WebControl {
    pub start: fn(&config::WebServer) -> anyhow::Result<()>,
    /// Point the token store at its file and start the expiry sweep. Safe
    /// to call again; the first listener to run wins.
    pub configure_tokens: fn(&[config::WebServer]) -> anyhow::Result<()>,
    /// `false` when nothing was listening on that address.
    pub stop: fn(&str) -> bool,
    /// The bind addresses currently accepting.
    pub listening: fn() -> Vec<String>,
}

static CONTROL: OnceLock<WebControl> = OnceLock::new();

/// Installed once, by the server binary, before it answers any client.
pub fn install(control: WebControl) {
    if CONTROL.set(control).is_err() {
        log::warn!("the web listener controls were installed twice; keeping the first");
    }
}

pub fn get() -> Option<&'static WebControl> {
    CONTROL.get()
}

/// What is accepting right now. Empty when the controls are absent, which
/// is indistinguishable from "nothing is listening" and means the same to
/// every caller.
pub fn listening() -> Vec<String> {
    get().map(|c| (c.listening)()).unwrap_or_default()
}
