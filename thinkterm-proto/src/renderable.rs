#[cfg(feature = "lua")]
use luahelper::impl_lua_conversion_dynamic;
use serde::{Deserialize, Serialize};
use wezterm_dynamic::{FromDynamic, ToDynamic};
use wezterm_term::StableRowIndex;

/// Describes the location of the cursor
#[derive(
    Debug, Default, Copy, Clone, Hash, Eq, PartialEq, Deserialize, Serialize, FromDynamic, ToDynamic,
)]
pub struct StableCursorPosition {
    pub x: usize,
    pub y: StableRowIndex,
    pub shape: termwiz::surface::CursorShape,
    pub visibility: termwiz::surface::CursorVisibility,
}
// The lua impls must live beside the type (orphan rule), but they must not
// reach the wasm build, whose whole point is having no Lua underneath.
#[cfg(feature = "lua")]
impl_lua_conversion_dynamic!(StableCursorPosition);

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize, FromDynamic, ToDynamic,
)]
pub struct RenderableDimensions {
    /// The viewport width
    pub cols: usize,
    /// How many rows fit in the viewport
    pub viewport_rows: usize,
    /// The total number of lines in the scrollback, including the viewport
    pub scrollback_rows: usize,

    /// The top of the physical, non-scrollback, screen expressed
    /// as a stable index.  It is envisioned that this will be used
    /// to compute row/cols for mouse events and to produce a range
    /// for the `get_lines` call when the scroll position is at the
    /// bottom of the screen.
    pub physical_top: StableRowIndex,
    /// The top of the scrollback (the earliest row we remember)
    /// expressed as a stable index.
    pub scrollback_top: StableRowIndex,
    pub dpi: u32,
    pub pixel_width: usize,
    pub pixel_height: usize,
    /// True if the lines should be rendered reversed
    pub reverse_video: bool,
}
#[cfg(feature = "lua")]
impl_lua_conversion_dynamic!(RenderableDimensions);
