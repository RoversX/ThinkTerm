//! The browser's glyph cache: the desktop's glyph key and `load_glyph`
//! arithmetic (wezterm-gui/src/glyphcache.rs), its cell metrics
//! (utilsprites.rs) and its underline and cursor sprites, over the shared
//! atlas and the pure-Rust font set. Moved, not re-derived, so the only
//! placement differences left are the shaper's (see thinkterm-font-web).

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::rc::Rc;
use termwiz::cell::Underline;
use termwiz::surface::CursorShape;
use thinkterm_font_core::units::PixelLength;
use thinkterm_font_core::{FontMetrics, FontRasterizer, FontShaper, GlyphInfo};
use thinkterm_font_web::FontSet;
use thinkterm_render::atlas::{Atlas, AtlasTag, OutOfTextureSpace, Sprite};
use thinkterm_render::bitmaps::{BitmapImage, Image, Texture2d};
use thinkterm_render::geom::{Point, Rect, Size};
use thinkterm_render::pipeline::GpuTexture;
use wezterm_color_types::SrgbaPixel;

/// utilsprites.rs `RenderMetrics`, the config-free constructor.
#[derive(Copy, Clone, Debug)]
pub struct RenderMetrics {
    pub descender: PixelLength,
    pub descender_row: isize,
    pub descender_plus_two: isize,
    pub underline_height: isize,
    pub strike_row: isize,
    pub cell_size: Size,
}

impl RenderMetrics {
    pub fn with_font_metrics(metrics: &FontMetrics) -> Self {
        let (cell_height, cell_width) = (
            metrics.cell_height.get().ceil() as usize,
            metrics.cell_width.get().ceil() as usize,
        );
        let underline_height = metrics.underline_thickness.get().round().max(1.) as isize;
        let descender_row =
            (cell_height as f64 + (metrics.descender - metrics.underline_position).get()) as isize;
        let descender_plus_two =
            (2 * underline_height + descender_row).min(cell_height as isize - underline_height);
        let strike_row = descender_row / 2;
        Self {
            descender: metrics.descender,
            descender_row,
            descender_plus_two,
            strike_row,
            cell_size: Size::new(cell_width as isize, cell_height as isize),
            underline_height,
        }
    }

    pub fn scale_cell_width(&self, scale: f64) -> Self {
        let mut scaled = *self;
        scaled.cell_size.width = (self.cell_size.width as f64 * scale) as isize;
        scaled
    }
}

/// glyphcache.rs `CachedGlyph`, fields and all: the emitter reads what it
/// needs today and the rest is the desktop's record.
#[derive(Debug)]
#[allow(dead_code)]
pub struct CachedGlyph {
    pub has_color: bool,
    pub brightness_adjust: f32,
    pub x_offset: PixelLength,
    pub y_offset: PixelLength,
    pub x_advance: PixelLength,
    pub bearing_x: PixelLength,
    pub bearing_y: PixelLength,
    pub texture: Option<Sprite>,
    pub scale: f64,
}

/// glyphcache.rs `GlyphKey` without the style: one style in the browser.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct GlyphKey {
    font_idx: usize,
    glyph_pos: u32,
    num_cells: u8,
    followed_by_space: bool,
    cell_width: u16,
    cell_height: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct LineKey {
    strike_through: bool,
    underline: Underline,
    overline: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ShapeKey {
    text: String,
    /// `Presentation` has no `Hash`; Text is 0, Emoji is 1.
    presentation: Option<u8>,
}

pub struct GlyphCache {
    fonts: Rc<FontSet>,
    pub size_pt: f64,
    pub dpi: u32,
    pub base_metrics: FontMetrics,
    pub metrics: RenderMetrics,
    texture: Rc<GpuTexture>,
    pub atlas: Atlas,
    glyphs: HashMap<GlyphKey, Rc<CachedGlyph>>,
    lines: HashMap<LineKey, Sprite>,
    cursors: HashMap<(Option<CursorShape>, u8), Sprite>,
    /// Braille is drawn rather than shaped, so it is keyed by the dot
    /// pattern: every one of the 256 shapes to the same .notdef, and
    /// `glyphs` would collide them onto a single entry.
    braille: HashMap<u8, Rc<CachedGlyph>>,
    shapes: HashMap<ShapeKey, Rc<Vec<GlyphInfo>>>,
    #[allow(dead_code)]
    pub white_space: Sprite,
    pub filled_box: Sprite,
}

impl GlyphCache {
    pub fn new(
        fonts: Rc<FontSet>,
        size_pt: f64,
        dpi: u32,
        texture: Rc<GpuTexture>,
    ) -> Result<Self> {
        let base_metrics = fonts.metrics(size_pt, dpi)?;
        let metrics = RenderMetrics::with_font_metrics(&base_metrics);
        let surface: Rc<dyn Texture2d> = texture.clone();
        let mut atlas = Atlas::new(&surface)?;
        // utilsprites.rs `UtilSprites`: a white cell and a blank cell.
        let mut buffer = Image::new(
            metrics.cell_size.width as usize,
            metrics.cell_size.height as usize,
        );
        let cell_rect = Rect::new(Point::new(0, 0), metrics.cell_size);
        buffer.clear_rect(cell_rect, SrgbaPixel::rgba(0xff, 0xff, 0xff, 0xff));
        let filled_box = atlas.allocate(&buffer)?;
        buffer.clear_rect(cell_rect, SrgbaPixel::rgba(0, 0, 0, 0));
        let white_space = atlas.allocate(&buffer)?;
        Ok(Self {
            fonts,
            size_pt,
            dpi,
            base_metrics,
            metrics,
            texture,
            atlas,
            glyphs: HashMap::new(),
            lines: HashMap::new(),
            cursors: HashMap::new(),
            braille: HashMap::new(),
            shapes: HashMap::new(),
            white_space,
            filled_box,
        })
    }

    pub fn texture(&self) -> &GpuTexture {
        &self.texture
    }

    /// Shape one cluster's text, memoised: the same text with the same
    /// attributes shapes the same every frame.
    pub fn shape(
        &mut self,
        text: &str,
        presentation: Option<termwiz::cell::Presentation>,
        presentation_width: Option<&thinkterm_font_core::PresentationWidth>,
    ) -> Result<Rc<Vec<GlyphInfo>>> {
        let key = ShapeKey {
            text: text.to_string(),
            presentation: presentation.map(|p| match p {
                termwiz::cell::Presentation::Text => 0,
                termwiz::cell::Presentation::Emoji => 1,
            }),
        };
        if let Some(shaped) = self.shapes.get(&key) {
            return Ok(Rc::clone(shaped));
        }
        let mut no_glyphs = Vec::new();
        let shaped = Rc::new(self.fonts.shape(
            text,
            self.size_pt,
            self.dpi,
            &mut no_glyphs,
            presentation,
            thinkterm_font_core::Direction::LeftToRight,
            None,
            presentation_width,
        )?);
        if !no_glyphs.is_empty() {
            log::debug!("no bundled face has {no_glyphs:?}");
        }
        if self.shapes.len() > 4096 {
            self.shapes.clear();
        }
        self.shapes.insert(key, Rc::clone(&shaped));
        Ok(shaped)
    }

    pub fn cached_glyph(
        &mut self,
        info: &GlyphInfo,
        followed_by_space: bool,
        num_cells: u8,
    ) -> Result<Rc<CachedGlyph>> {
        // Drawn, not looked up: no bundled face covers U+2800..=U+28FF, so
        // every Braille character would otherwise be a missing-glyph box --
        // and every graph btop or macmon draws is made of them.
        if let Some(dots) = info.only_char.and_then(crate::braille::dots) {
            return self.braille_glyph(dots);
        }
        let key = GlyphKey {
            font_idx: info.font_idx,
            glyph_pos: info.glyph_pos,
            num_cells,
            followed_by_space,
            cell_width: self.metrics.cell_size.width as u16,
            cell_height: self.metrics.cell_size.height as u16,
        };
        if let Some(entry) = self.glyphs.get(&key) {
            return Ok(Rc::clone(entry));
        }
        let glyph = match self.load_glyph(info, followed_by_space, num_cells) {
            Ok(g) => g,
            Err(err) => {
                if err.root_cause().downcast_ref::<OutOfTextureSpace>().is_some() {
                    return Err(err);
                }
                log::error!("load_glyph failed; using blank instead: {err:#} {info:?}");
                Rc::new(CachedGlyph {
                    brightness_adjust: 1.0,
                    has_color: false,
                    texture: None,
                    x_advance: PixelLength::new(0.0),
                    x_offset: PixelLength::new(0.0),
                    y_offset: PixelLength::new(0.0),
                    bearing_x: PixelLength::new(0.0),
                    bearing_y: PixelLength::new(0.0),
                    scale: 1.0,
                })
            }
        };
        self.glyphs.insert(key, Rc::clone(&glyph));
        Ok(glyph)
    }

    /// glyphcache.rs `load_glyph`, with the desktop's default of letting
    /// square glyphs overflow when a space follows.
    #[allow(clippy::float_cmp)]
    fn load_glyph(
        &mut self,
        info: &GlyphInfo,
        followed_by_space: bool,
        num_cells: u8,
    ) -> Result<Rc<CachedGlyph>> {
        let base_metrics = self.base_metrics;
        let face = self.fonts.face(info.font_idx)?;
        let glyph = face.rasterize_glyph(info.glyph_pos, self.size_pt, self.dpi)?;
        let idx_metrics = self.fonts.metrics_for_idx(info.font_idx, self.size_pt, self.dpi)?;
        let brightness_adjust = 1.0;

        let aspect = (idx_metrics.cell_width / idx_metrics.cell_height).get();
        let is_square_or_wide = aspect >= 0.7;
        let allow_width_overflow = is_square_or_wide && followed_by_space;
        let num_cells = num_cells.max(1) as f64;
        let max_pixel_width = base_metrics.cell_width.get() * (num_cells + 0.25);

        let scale;
        let mut metrics_only_scale = 1.0;
        if info.font_idx == 0 {
            scale = if allow_width_overflow || glyph.width as f64 <= max_pixel_width {
                1.0
            } else {
                1.0 / num_cells
            };
        } else if !glyph.is_scaled {
            let y_scale = base_metrics.cell_height.get() / idx_metrics.cell_height.get();
            let y_scaled_width = y_scale * glyph.width as f64;
            if allow_width_overflow || y_scaled_width <= max_pixel_width {
                scale = y_scale;
            } else {
                scale = max_pixel_width / glyph.width as f64;
            }
        } else {
            let f_width = glyph.width as f64;
            if allow_width_overflow || f_width <= max_pixel_width {
                scale = 1.0;
            } else {
                scale = max_pixel_width / f_width;
            }
            if !idx_metrics.is_scaled {
                metrics_only_scale =
                    base_metrics.cell_height.get() / idx_metrics.cell_height.get();
            }
        }

        let descender_adjust = if info.font_idx == 0 {
            PixelLength::new(0.0)
        } else {
            idx_metrics.force_y_adjust
        };

        let glyph = if glyph.width == 0 || glyph.height == 0 {
            CachedGlyph {
                brightness_adjust: 1.0,
                has_color: glyph.has_color,
                texture: None,
                x_offset: info.x_offset * scale,
                y_offset: info.y_offset * scale,
                x_advance: info.x_advance * scale,
                bearing_x: PixelLength::new(0.0),
                bearing_y: descender_adjust,
                scale,
            }
        } else {
            let raw_im = Image::with_rgba32(
                glyph.width,
                glyph.height,
                4 * glyph.width,
                &glyph.data,
            );
            let bearing_x = glyph.bearing_x * scale * metrics_only_scale;
            let bearing_y = descender_adjust + (glyph.bearing_y * scale);
            let x_offset = info.x_offset * scale * metrics_only_scale;
            let y_offset = info.y_offset * scale * metrics_only_scale;
            let x_advance = info.x_advance * scale * metrics_only_scale;
            let (scale, raw_im) = if scale != 1.0 {
                (1.0, raw_im.scale_by(scale))
            } else {
                (scale, raw_im)
            };
            let tex = self
                .atlas
                .allocate_tagged(&raw_im, None, None, AtlasTag::Glyph)?;
            CachedGlyph {
                brightness_adjust,
                has_color: glyph.has_color,
                texture: Some(tex),
                x_offset,
                y_offset,
                x_advance,
                bearing_x,
                bearing_y,
                scale,
            }
        };
        Ok(Rc::new(glyph))
    }

    /// glyphcache.rs `cached_line_sprite`: underline, strike and overline
    /// drawn into one cell-sized sprite.
    pub fn line_sprite(
        &mut self,
        strike_through: bool,
        underline: Underline,
        overline: bool,
    ) -> Result<Sprite> {
        let key = LineKey {
            strike_through,
            underline,
            overline,
        };
        if let Some(s) = self.lines.get(&key) {
            return Ok(s.clone());
        }
        let metrics = self.metrics;
        let mut buffer = Image::new(
            metrics.cell_size.width as usize,
            metrics.cell_size.height as usize,
        );
        let black = SrgbaPixel::rgba(0, 0, 0, 0);
        let white = SrgbaPixel::rgba(0xff, 0xff, 0xff, 0xff);
        let cell_rect = Rect::new(Point::new(0, 0), metrics.cell_size);
        let width = metrics.cell_size.width;

        let draw_rows = |buffer: &mut Image, first_row: isize| {
            for row in 0..metrics.underline_height {
                buffer.draw_line(
                    Point::new(0, first_row + row),
                    Point::new(width, first_row + row),
                    white,
                );
            }
        };
        let draw_pattern = |buffer: &mut Image, segment: usize| {
            for row in 0..metrics.underline_height {
                let y = (metrics.descender_row + row) as usize;
                if y >= metrics.cell_size.height as usize {
                    break;
                }
                let mut color = white;
                let mut count = segment;
                let range = buffer.horizontal_pixel_range_mut(0, width as usize, y);
                for c in range.iter_mut() {
                    *c = color.as_srgba32();
                    count -= 1;
                    if count == 0 {
                        color = if color == white { black } else { white };
                        count = segment;
                    }
                }
            }
        };

        buffer.clear_rect(cell_rect, black);
        if overline {
            draw_rows(&mut buffer, 0);
        }
        match underline {
            Underline::None => {}
            Underline::Single => draw_rows(&mut buffer, metrics.descender_row),
            Underline::Dotted => draw_pattern(&mut buffer, (width / 4).max(1) as usize),
            Underline::Dashed => draw_pattern(&mut buffer, (width / 3) as usize + 1),
            Underline::Double => {
                let first_line = metrics
                    .descender_row
                    .min(metrics.descender_plus_two - 2 * metrics.underline_height);
                draw_rows(&mut buffer, first_line);
                draw_rows(&mut buffer, metrics.descender_plus_two);
            }
            Underline::Curly => {
                let max_y = metrics.cell_size.height as usize - 1;
                let x_factor = (2. * std::f32::consts::PI) / width as f32;
                let wave_height = metrics.cell_size.height - metrics.descender_row;
                let half_height = (wave_height as f32 / 4.).max(1.);
                let y = (metrics.descender_row as usize).saturating_sub(half_height as usize);
                fn add(x: usize, y: usize, val: u8, max_y: usize, buffer: &mut Image) {
                    let y = y.min(max_y);
                    let pixel = buffer.pixel_mut(x, y);
                    let (current, _, _, _) = SrgbaPixel::with_srgba_u32(*pixel).as_rgba();
                    let value = current.saturating_add(val);
                    *pixel = SrgbaPixel::rgba(value, value, value, value).as_srgba32();
                }
                for x in 0..width as usize {
                    let vertical = -half_height * (x as f32 * x_factor).sin() + half_height;
                    let v1 = vertical.floor();
                    let v2 = vertical.ceil();
                    for row in 0..metrics.underline_height as usize {
                        let value = (255. * (vertical - v1).abs()) as u8;
                        add(x, row + y + v1 as usize, 255u8.saturating_sub(value), max_y, &mut buffer);
                        add(x, row + y + v2 as usize, value, max_y, &mut buffer);
                    }
                }
            }
        }
        if strike_through {
            draw_rows(&mut buffer, metrics.strike_row);
        }
        let sprite = self.atlas.allocate(&buffer)?;
        self.lines.insert(key, sprite.clone());
        Ok(sprite)
    }

    /// The cursor's sprite for a shape and a width in cells. Filled block,
    /// outlined block, bar and underline as flat pixels: the desktop draws
    /// these through its custom-glyph poly rasteriser, which is not shared
    /// yet.
    /// The glyph for a Braille dot pattern, drawn into the atlas once.
    ///
    /// The sprite covers the whole cell, which is not where a shaped glyph
    /// sits: `emit.rs` puts a glyph's top at `cell_height + descender -
    /// bearing_y`, so a bearing of zero would drop the pattern to the
    /// baseline and let it hang into the row below. This bearing is the one
    /// that puts the sprite's top on the cell's top.
    fn braille_glyph(&mut self, dots: u8) -> Result<Rc<CachedGlyph>> {
        if let Some(glyph) = self.braille.get(&dots) {
            return Ok(Rc::clone(glyph));
        }
        let cell = self.metrics.cell_size;
        let buffer = crate::braille::image(dots, cell);
        let sprite = self.atlas.allocate(&buffer).context("braille sprite")?;
        let glyph = Rc::new(CachedGlyph {
            brightness_adjust: 1.0,
            has_color: false,
            texture: Some(sprite),
            x_offset: PixelLength::new(0.0),
            y_offset: PixelLength::new(0.0),
            x_advance: PixelLength::new(cell.width as f64),
            bearing_x: PixelLength::new(0.0),
            bearing_y: PixelLength::new(cell.height as f64 + self.metrics.descender.get()),
            scale: 1.0,
        });
        self.braille.insert(dots, Rc::clone(&glyph));
        Ok(glyph)
    }

    pub fn cursor_sprite(&mut self, shape: Option<CursorShape>, width: u8) -> Result<Sprite> {
        if let Some(sprite) = self.cursors.get(&(shape, width)) {
            return Ok(sprite.clone());
        }
        let metrics = self.metrics.scale_cell_width(width.max(1) as f64);
        let (w, h) = (metrics.cell_size.width, metrics.cell_size.height);
        let thickness = metrics.underline_height.max(1);
        let mut buffer = Image::new(w as usize, h as usize);
        let black = SrgbaPixel::rgba(0, 0, 0, 0);
        let white = SrgbaPixel::rgba(0xff, 0xff, 0xff, 0xff);
        let cell_rect = Rect::new(Point::new(0, 0), metrics.cell_size);
        buffer.clear_rect(cell_rect, black);
        let fill = |buffer: &mut Image, x: isize, y: isize, rw: isize, rh: isize| {
            buffer.clear_rect(Rect::new(Point::new(x, y), Size::new(rw, rh)), white);
        };
        match shape {
            None => {}
            Some(CursorShape::Default) => buffer.clear_rect(cell_rect, white),
            Some(CursorShape::BlinkingBlock | CursorShape::SteadyBlock) => {
                fill(&mut buffer, 0, 0, w, thickness);
                fill(&mut buffer, 0, h - thickness, w, thickness);
                fill(&mut buffer, 0, 0, thickness, h);
                fill(&mut buffer, w - thickness, 0, thickness, h);
            }
            Some(CursorShape::BlinkingBar | CursorShape::SteadyBar) => {
                fill(&mut buffer, 0, 0, thickness, h);
            }
            Some(CursorShape::BlinkingUnderline | CursorShape::SteadyUnderline) => {
                fill(&mut buffer, 0, h - thickness, w, thickness);
            }
        }
        let sprite = self.atlas.allocate(&buffer).context("cursor sprite")?;
        self.cursors.insert((shape, width), sprite.clone());
        Ok(sprite)
    }
}
