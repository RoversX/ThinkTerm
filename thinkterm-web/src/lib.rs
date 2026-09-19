//! The ThinkTerm browser client. See Cargo.toml for the charter.
//!
//! `braille`, `fallback`, `keymap`, `lease`, `raster` and `viewport` are pure
//! and build everywhere, and so do `emit` and `glyphs` -- the drawing of a
//! line and the glyph atlas -- which the mobile client shares; everything
//! that touches the page is `wasm32` only.

pub mod agents;
pub mod app;
pub mod attach;
pub mod braille;
pub mod chrome;
pub mod commands;
pub mod emit;
pub mod fallback;
pub mod glyphs;
pub mod gpu;
pub mod host;
pub mod ime;
pub mod keymap;
pub mod icons;
pub mod layout;
pub mod lease;
pub mod menu;
pub mod navbar;
pub mod palette;
pub mod platform;
pub mod raster;
pub mod settings;
pub mod sidebar;
pub mod tree;
pub mod viewport;
pub mod views;

#[cfg(target_arch = "wasm32")]
mod bridge;
#[cfg(target_arch = "wasm32")]
mod canvas;
#[cfg(target_arch = "wasm32")]
mod graphics_check;
#[cfg(target_arch = "wasm32")]
mod input;
#[cfg(target_arch = "wasm32")]
mod link;
#[cfg(target_arch = "wasm32")]
mod page;
#[cfg(target_arch = "wasm32")]
mod web_platform;

#[cfg(target_arch = "wasm32")]
pub use bridge::Client;
#[cfg(target_arch = "wasm32")]
pub use page::{color_check, start};
