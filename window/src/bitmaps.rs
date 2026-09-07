//! Bitmaps, textures and the atlas live in `thinkterm-render` now; this
//! module keeps the `window::bitmaps::*` paths the desktop uses.
pub use thinkterm_render::bitmaps::*;

pub mod atlas {
    pub use thinkterm_render::atlas::*;
}
