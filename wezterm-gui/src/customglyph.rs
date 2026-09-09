//! Desktop configuration and caching around the shared terminal graphics.
use crate::glyphcache::{GlyphCache, SizedBlockKey};
use crate::utilsprites::RenderMetrics;
use config::DimensionContext;
use termwiz::surface::CursorShape;
pub use thinkterm_render::customglyph::*;
use tiny_skia::BlendMode;
use window::bitmaps::atlas::Sprite;
use window::color::SrgbaPixel;
use window::{BitmapImage, Image, Point, Rect};

pub fn filter_out_synthetic(glyphs: &mut Vec<char>) {
    if config::configuration().custom_block_glyphs {
        glyphs.retain(|&c| BlockKey::from_char(c).is_none());
    }
}

fn block_metrics(metrics: &RenderMetrics) -> BlockMetrics {
    BlockMetrics {
        cell_size: metrics.cell_size,
        underline_height: metrics.underline_height,
    }
}

impl GlyphCache {
    pub fn cursor_sprite(
        &mut self,
        shape: Option<CursorShape>,
        metrics: &RenderMetrics,
        width: u8,
    ) -> anyhow::Result<Sprite> {
        if let Some(sprite) = self.cursor_glyphs.get(&(shape, width)) {
            return Ok(sprite.clone());
        }

        let mut metrics = metrics.scale_cell_width(width as f64);
        if let Some(d) = &self.fonts.config().cursor_thickness {
            metrics.underline_height = d.evaluate_as_pixels(DimensionContext {
                dpi: self.fonts.get_dpi() as f32,
                pixel_max: metrics.underline_height as f32,
                pixel_cell: metrics.cell_size.height as f32,
            }) as isize;
        }

        let mut buffer = Image::new(
            metrics.cell_size.width as usize,
            metrics.cell_size.height as usize,
        );
        let black = SrgbaPixel::rgba(0, 0, 0, 0);
        let cell_rect = Rect::new(Point::new(0, 0), metrics.cell_size);
        buffer.clear_rect(cell_rect, black);

        match shape {
            None => {}
            Some(CursorShape::Default) => {
                buffer.clear_rect(cell_rect, SrgbaPixel::rgba(0xff, 0xff, 0xff, 0xff));
            }
            Some(CursorShape::BlinkingBlock | CursorShape::SteadyBlock) => {
                draw_polys(
                    &block_metrics(&metrics),
                    &[Poly {
                        path: &[
                            PolyCommand::MoveTo(BlockCoord::Zero, BlockCoord::Zero),
                            PolyCommand::LineTo(BlockCoord::One, BlockCoord::Zero),
                            PolyCommand::LineTo(BlockCoord::One, BlockCoord::One),
                            PolyCommand::LineTo(BlockCoord::Zero, BlockCoord::One),
                            PolyCommand::LineTo(BlockCoord::Zero, BlockCoord::Zero),
                        ],
                        intensity: BlockAlpha::Full,
                        style: PolyStyle::OutlineHeavy,
                    }],
                    &mut buffer,
                    PolyAA::AntiAlias,
                    BlendMode::default(),
                );
            }
            Some(CursorShape::BlinkingBar | CursorShape::SteadyBar) => {
                draw_polys(
                    &block_metrics(&metrics),
                    &[Poly {
                        path: &[
                            PolyCommand::MoveTo(BlockCoord::Zero, BlockCoord::Zero),
                            PolyCommand::LineTo(BlockCoord::Zero, BlockCoord::One),
                        ],
                        intensity: BlockAlpha::Full,
                        style: PolyStyle::OutlineHeavy,
                    }],
                    &mut buffer,
                    PolyAA::AntiAlias,
                    BlendMode::default(),
                );
            }
            Some(CursorShape::BlinkingUnderline | CursorShape::SteadyUnderline) => {
                draw_polys(
                    &block_metrics(&metrics),
                    &[Poly {
                        path: &[
                            PolyCommand::MoveTo(BlockCoord::Zero, BlockCoord::One),
                            PolyCommand::LineTo(BlockCoord::One, BlockCoord::One),
                        ],
                        intensity: BlockAlpha::Full,
                        style: PolyStyle::OutlineHeavy,
                    }],
                    &mut buffer,
                    PolyAA::AntiAlias,
                    BlendMode::default(),
                );
            }
        }

        let sprite = self.atlas.allocate(&buffer)?;
        self.cursor_glyphs.insert((shape, width), sprite.clone());
        Ok(sprite)
    }

    pub fn block_sprite(
        &mut self,
        metrics: &RenderMetrics,
        key: SizedBlockKey,
    ) -> anyhow::Result<Sprite> {
        let aa = if config::configuration().anti_alias_custom_block_glyphs {
            PolyAA::AntiAlias
        } else {
            PolyAA::MoarPixels
        };
        let buffer = block_image(key.block, &block_metrics(metrics), aa);
        let sprite = self.atlas.allocate(&buffer)?;
        self.block_glyphs.insert(key, sprite.clone());
        Ok(sprite)
    }
}
