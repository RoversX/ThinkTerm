//! The two things a font backend has to be able to do.

use crate::glyph::{GlyphInfo, RasterizedGlyph};
use crate::metrics::FontMetrics;
use std::ops::Range;
use termwiz::cellcluster::CellCluster;
use wezterm_bidi::Direction;

/// Rasterizes the specified glyph index in the associated font
/// and returns the generated bitmap
pub trait FontRasterizer {
    fn rasterize_glyph(
        &self,
        glyph_pos: u32,
        size: f64,
        dpi: u32,
    ) -> anyhow::Result<RasterizedGlyph>;
}

#[derive(Debug)]
pub struct PresentationWidth<'a> {
    cluster: &'a CellCluster,
}

impl<'a> PresentationWidth<'a> {
    pub fn with_cluster(cluster: &'a CellCluster) -> Self {
        Self { cluster }
    }

    pub fn num_cells(&self, cluster_range: Range<usize>) -> u8 {
        let mut width = 0;
        let mut done_cells = vec![];

        for byte_idx in cluster_range {
            let cell_idx = self.cluster.byte_to_cell_idx(byte_idx);
            if done_cells.contains(&cell_idx) {
                continue;
            }
            done_cells.push(cell_idx);
            width += self.cluster.byte_to_cell_width(byte_idx);
        }
        width
    }

    pub fn byte_to_cell_idx(&self, start_byte: usize) -> usize {
        self.cluster.byte_to_cell_idx(start_byte)
    }
}

pub trait FontShaper {
    /// Shape text and return a vector of GlyphInfo
    fn shape(
        &self,
        text: &str,
        size: f64,
        dpi: u32,
        no_glyphs: &mut Vec<char>,
        presentation: Option<termwiz::cell::Presentation>,
        direction: Direction,
        range: Option<Range<usize>>,
        presentation_width: Option<&PresentationWidth>,
    ) -> anyhow::Result<Vec<GlyphInfo>>;

    /// Compute the font metrics for the preferred font
    /// at the specified size.
    fn metrics(&self, size: f64, dpi: u32) -> anyhow::Result<FontMetrics>;

    /// Compute the metrics for a given fallback font at the specified size
    fn metrics_for_idx(&self, font_idx: usize, size: f64, dpi: u32) -> anyhow::Result<FontMetrics>;
}
