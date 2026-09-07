//! The cell metrics a renderer lays text out with.

use crate::units::PixelLength;
use termwiz::cell::Presentation;

/// Describes the key font metrics that we use in rendering
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct FontMetrics {
    /// Width of a character cell in pixels
    pub cell_width: PixelLength,
    /// Height of a character cell in pixels
    pub cell_height: PixelLength,
    /// Added to the bottom y coord to find the baseline.
    /// descender is typically negative.
    pub descender: PixelLength,

    /// Vertical size of underline/strikethrough in pixels
    pub underline_thickness: PixelLength,

    /// Position of underline relative to descender. Negative
    /// values are below the descender.
    pub underline_position: PixelLength,

    /// Fraction of the EM square occupied by the cap height
    pub cap_height_ratio: Option<f64>,
    pub cap_height: Option<PixelLength>,

    /// True if the font is scalable and this is a scaled metric.
    /// False if the font only has bitmap strikes and what we
    /// have here is a best approximation.
    pub is_scaled: bool,

    pub presentation: Presentation,

    /// When the user has configured a fallback-specific override,
    /// this field contains the difference in the descender heights
    /// between the scaled and unscaled versions of the descender.
    /// This represents a y-adjustment that should be applied to
    /// the glyph to make it appear to line up better.
    /// <https://github.com/wezterm/wezterm/issues/1803>
    pub force_y_adjust: PixelLength,
}
