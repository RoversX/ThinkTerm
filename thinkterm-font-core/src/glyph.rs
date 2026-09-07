//! What a shaper hands back and what a rasterizer produces.

use crate::units::PixelLength;

/// A bitmap representation of a glyph.
/// The data is stored as pre-multiplied RGBA 32bpp.
#[derive(Debug)]
pub struct RasterizedGlyph {
    pub data: Vec<u8>,
    pub height: usize,
    pub width: usize,
    pub bearing_x: PixelLength,
    pub bearing_y: PixelLength,
    pub has_color: bool,
    /// if true, glyphcache shouldn't need to scale the
    /// glyph to match metrics
    pub is_scaled: bool,
}

/// Holds information about a shaped glyph
#[derive(Clone, Debug, PartialEq)]
pub struct GlyphInfo {
    /// We only retain text in debug mode for diagnostic purposes
    #[cfg(any(debug_assertions, feature = "glyph-text"))]
    pub text: String,
    /// If text is comprised of a single char, this is it
    pub only_char: Option<char>,
    pub is_space: bool,
    /// Number of cells occupied by this single glyph.
    /// This accounts for eg: the shaper combining adjacent graphemes
    /// into a single glyph, such as in `!=` and other ligatures.
    /// Without tracking this version of the width, we may not detect
    /// the combined case as the corresponding cluster index is simply
    /// omitted from the shaped result.
    /// <https://github.com/wezterm/wezterm/issues/1563>
    pub num_cells: u8,
    /// Offset within text
    pub cluster: u32,
    /// Which font alternative to use; index into Font.fonts
    pub font_idx: FallbackIdx,
    /// Which freetype glyph to load
    pub glyph_pos: u32,
    /// How far to advance the render cursor after drawing this glyph
    pub x_advance: PixelLength,
    /// How far to advance the render cursor after drawing this glyph
    pub y_advance: PixelLength,
    /// Destination render offset
    pub x_offset: PixelLength,
    /// Destination render offset
    pub y_offset: PixelLength,
}

/// Represents a numbered index in the fallback sequence for a `NamedFont`.
/// 0 is the first, best match.  If a glyph isn't present then we will
/// want to search for a fallback in later indices.
pub type FallbackIdx = usize;

/// Everything a shaper knows about one glyph, minus the text, which only
/// some builds keep. Passed to [`GlyphInfo::new`], so a shaper never has
/// to know whether the `text` field exists in this build.
#[derive(Clone, Debug)]
pub struct GlyphInfoParts {
    pub only_char: Option<char>,
    pub is_space: bool,
    pub num_cells: u8,
    pub cluster: u32,
    pub font_idx: FallbackIdx,
    pub glyph_pos: u32,
    pub x_advance: PixelLength,
    pub y_advance: PixelLength,
    pub x_offset: PixelLength,
    pub y_offset: PixelLength,
}

impl GlyphInfo {
    #[allow(unused_variables)]
    pub fn new(text: &str, parts: GlyphInfoParts) -> Self {
        Self {
            #[cfg(any(debug_assertions, feature = "glyph-text"))]
            text: text.to_string(),
            only_char: parts.only_char,
            is_space: parts.is_space,
            num_cells: parts.num_cells,
            cluster: parts.cluster,
            font_idx: parts.font_idx,
            glyph_pos: parts.glyph_pos,
            x_advance: parts.x_advance,
            y_advance: parts.y_advance,
            x_offset: parts.x_offset,
            y_offset: parts.y_offset,
        }
    }
}
