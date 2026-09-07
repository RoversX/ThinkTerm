//! Geometry shared by every renderer. Pixel-space points and rects come
//! from `wezterm-input-types`; this module adds what the quads need on top.

pub use wezterm_input_types::{PixelUnit, Point, PointF};

pub type ULength = euclid::Length<usize, PixelUnit>;
pub type Rect = euclid::Rect<isize, PixelUnit>;
pub type RectF = euclid::Rect<f32, PixelUnit>;
pub type Size = euclid::Size2D<isize, PixelUnit>;
pub type SizeF = euclid::Size2D<f32, PixelUnit>;

/// The size of the surface being drawn, in physical pixels, and its DPI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Dimensions {
    pub pixel_width: usize,
    pub pixel_height: usize,
    pub dpi: usize,
}
