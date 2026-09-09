//! The ThinkTerm browser client. See Cargo.toml for the charter.
//!
//! `braille`, `fallback`, `keymap`, `lease` and `viewport` are pure and
//! build everywhere; everything that touches the page is `wasm32` only.

pub mod agents;
pub mod braille;
pub mod chrome;
pub mod fallback;
pub mod ime;
pub mod keymap;
pub mod icons;
pub mod layout;
pub mod lease;
pub mod menu;
pub mod navbar;
pub mod palette;
pub mod settings;
pub mod sidebar;
pub mod tree;
pub mod viewport;
pub mod views;

#[cfg(target_arch = "wasm32")]
mod app;
#[cfg(target_arch = "wasm32")]
mod attach;
#[cfg(target_arch = "wasm32")]
mod bridge;
#[cfg(target_arch = "wasm32")]
mod canvas;
#[cfg(target_arch = "wasm32")]
mod emit;
#[cfg(target_arch = "wasm32")]
mod glyphs;
#[cfg(target_arch = "wasm32")]
mod gpu;
#[cfg(target_arch = "wasm32")]
mod graphics_check;
#[cfg(target_arch = "wasm32")]
mod host;
#[cfg(target_arch = "wasm32")]
mod input;
#[cfg(target_arch = "wasm32")]
mod link;
#[cfg(target_arch = "wasm32")]
mod page;

#[cfg(target_arch = "wasm32")]
pub use bridge::Client;
#[cfg(target_arch = "wasm32")]
pub use page::{color_check, start};
