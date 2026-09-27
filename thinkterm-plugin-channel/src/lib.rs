//! The plugin channel: how ThinkTerm's clients reach the plugin host.
//!
//! The mux runs terminals and nothing else. What a client shows beside them
//! -- the snippets today -- is kept by the plugin host, a process of its
//! own, started when something first asks for it and gone again once
//! nothing has been connected to it for a while. A slow or crashed plugin
//! is then a slow or crashed plugin, never a stalled terminal, and a
//! machine where nobody opens a plugin never runs one.
//!
//! A client on the host's machine talks to it over a unix socket. A
//! browser can only reach the mux, so the mux carries its frames there and
//! back without looking inside them.
//!
//! A frame is a little-endian `u32` length and that many bytes of JSON: a
//! [`wire::ToHost`] one way, a [`wire::FromHost`] the other. What a call's
//! body means is up to the plugin it names; the host's own API, the list of
//! plugins and their commands, is [`registry`].

#[cfg(feature = "native")]
pub mod client;
#[cfg(feature = "native")]
pub mod paths;
pub mod registry;
pub mod wire;
