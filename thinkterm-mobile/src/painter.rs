//! Glyph fallback on a phone: the shell paints text the bundled faces lack
//! (CJK, emoji) with the platform's own fonts -- CoreText on iOS -- and the
//! shared procedure in `thinkterm_web::raster` does the rest.

use anyhow::{ensure, Result};
use std::sync::Arc;
use thinkterm_web::raster::{GlyphPlatform, ScratchGeometry, TextPainter};

/// Implemented by the shell. Called on the core thread, synchronously,
/// while a frame is being built: keep it quick and never block on the
/// main thread.
#[uniffi::export(callback_interface)]
pub trait GlyphPainter: Send + Sync {
    /// Paint `text` at `px` pixels into a transparent `width` x `height`
    /// RGBA bitmap, pen at (`pen_x`, `baseline`), in the solid colour
    /// given, and return the bitmap's bytes (non-premultiplied, row-major,
    /// `width * height * 4`). An empty vector means the platform could not
    /// draw it.
    #[allow(clippy::too_many_arguments)]
    fn paint(
        &self,
        text: String,
        px: f64,
        width: u32,
        height: u32,
        pen_x: f64,
        baseline: f64,
        red: u8,
        green: u8,
        blue: u8,
    ) -> Vec<u8>;
}

pub struct GlyphSeams {
    painter: Arc<dyn GlyphPainter>,
}

impl GlyphSeams {
    pub fn new(painter: Box<dyn GlyphPainter>) -> Self {
        Self {
            painter: Arc::from(painter),
        }
    }
}

impl GlyphPlatform for GlyphSeams {
    /// The platform's clock: the frame budget's deadline is set on it.
    fn now_ms(&self) -> f64 {
        crate::platform::monotonic_ms()
    }

    fn painter(&self, geometry: &ScratchGeometry, _families: &str) -> Result<Box<dyn TextPainter>> {
        Ok(Box::new(ShellPainter {
            painter: Arc::clone(&self.painter),
            geometry: *geometry,
        }))
    }
}

struct ShellPainter {
    painter: Arc<dyn GlyphPainter>,
    geometry: ScratchGeometry,
}

impl TextPainter for ShellPainter {
    fn paint(&self, text: &str, fill: [u8; 3]) -> Result<Vec<u8>> {
        let g = &self.geometry;
        let bytes = self.painter.paint(
            text.to_string(),
            g.px,
            g.width as u32,
            g.height as u32,
            g.pen_x,
            g.baseline,
            fill[0],
            fill[1],
            fill[2],
        );
        ensure!(!bytes.is_empty(), "the shell could not paint {text:?}");
        Ok(bytes)
    }
}
