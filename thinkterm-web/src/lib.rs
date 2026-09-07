//! The ThinkTerm browser client. See Cargo.toml for the charter.
//!
//! `braille`, `keymap`, `lease` and `viewport` are pure and build
//! everywhere; everything that touches the page is `wasm32` only.

pub mod braille;
pub mod keymap;
pub mod lease;
pub mod viewport;

#[cfg(target_arch = "wasm32")]
mod app;
#[cfg(target_arch = "wasm32")]
mod attach;
#[cfg(target_arch = "wasm32")]
mod emit;
#[cfg(target_arch = "wasm32")]
mod glyphs;
#[cfg(target_arch = "wasm32")]
mod gpu;
#[cfg(target_arch = "wasm32")]
mod host;
#[cfg(target_arch = "wasm32")]
mod input;
#[cfg(target_arch = "wasm32")]
mod link;
#[cfg(target_arch = "wasm32")]
mod page;

#[cfg(target_arch = "wasm32")]
pub use page::{color_check, start};
