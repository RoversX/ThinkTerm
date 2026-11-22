//! A node in a tailnet, of Tailscale's own Rust implementation, and the
//! words for where it is: the one place ThinkTerm calls that library.
//!
//! Whoever holds a node starts it on a [`node::NodeConfig`] -- the node's
//! keys, the control server, an auth key or none -- and reads its
//! [`Status`] and the [`Peer`]s it sees; connections go through it
//! ([`node::Shared::tcp_connect`]). Where the keys are kept, who may ask
//! for connections and what restarts a node given up are the holder's:
//! `thinkterm-tailnet` on a desktop or a server, `thinkterm-mobile` on a
//! phone.

mod status;

#[cfg(all(
    feature = "node",
    any(
        target_os = "macos",
        target_os = "linux",
        target_os = "windows",
        target_os = "ios",
        target_os = "android"
    )
))]
pub mod node;

pub use status::{Family, Peer, Status, NOT_LET_IN};

use anyhow::{Context, Result};

/// Whether this build can hold a node: the library runs on macOS, Linux,
/// Windows, iOS and Android only.
pub const SUPPORTED: bool = cfg!(any(
    target_os = "macos",
    target_os = "linux",
    target_os = "windows",
    target_os = "ios",
    target_os = "android"
));

/// What a person types for a control server, as a URL: "https://" when no
/// scheme is given.
pub fn control_url(text: &str) -> Result<url::Url> {
    let text = text.trim();
    let with_scheme = if text.contains("://") {
        text.to_string()
    } else {
        format!("https://{text}")
    };
    with_scheme
        .parse()
        .with_context(|| format!("the control server address {text:?}"))
}
