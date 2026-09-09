//! The browser's glyph cache: the desktop's glyph key and `load_glyph`
//! arithmetic (wezterm-gui/src/glyphcache.rs), its cell metrics
//! (utilsprites.rs) and its underline and cursor sprites, over the shared
//! atlas and the pure-Rust font set. Moved, not re-derived, so the only
//! placement differences left are the shaper's (see thinkterm-font-web).

use crate::fallback::FallbackBudget;
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::rc::Rc;
use termwiz::cell::Underline;
use termwiz::surface::CursorShape;
use thinkterm_font_core::units::PixelLength;
use thinkterm_font_core::{FontMetrics, FontRasterizer, FontShaper, GlyphInfo};
use thinkterm_font_web::{FontSet, Gap, WebShaped};
use thinkterm_render::atlas::{Atlas, AtlasTag, OutOfTextureSpace, Sprite};
use thinkterm_render::bitmaps::{BitmapImage, Image, Texture2d};
use thinkterm_render::customglyph::{block_image, BlockKey, BlockMetrics, PolyAA};
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

/// How many drawn-glyph verdicts to keep. Large enough for several screens
/// of distinct CJK, small enough that a stream of junk code points cannot
/// grow the heap without end.
const MAX_FALLBACK_ENTRIES: usize = 8192;

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
    /// The columns the row says this cluster occupies.
    ///
    /// The same text really is a different number of columns either side of
    /// an `ambiguous_are_wide` or `cell_widths` change, and a character can
    /// be both ambiguous-width and a gap -- U+2605 is in neither bundled
    /// face. This used to only get `num_cells` wrong; now `Gap.cells` comes
    /// out of this cache entry and decides how far a fallback glyph
    /// advances the column.
    cells: u16,
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
    blocks: HashMap<(BlockKey, u8), Rc<CachedGlyph>>,
    shapes: HashMap<ShapeKey, Rc<WebShaped>>,
    /// Glyphs the browser drew for us, by columns and then by grapheme.
    /// `None` remembers that it could not draw one either, so a machine
    /// without the font does not redraw the same nothing every frame.
    ///
    /// Not keyed on `GlyphKey`: every uncovered grapheme shapes to font 0's
    /// glyph 0, so CJK, Hangul, emoji and arrows would all collide onto one
    /// entry -- the Braille bug, at the scale of Unicode. Not on
    /// `only_char` either: that is `None` for every ZWJ, combining and
    /// variation-selector cluster.
    ///
    /// Cell size, dpi and the font chain are deliberately absent from the
    /// key: none of them can change without rebuilding this whole cache,
    /// and this table goes with it. So is presentation -- one font string,
    /// one drawing procedure, no difference in the pixels.
    fallback: HashMap<u8, HashMap<String, Option<Rc<CachedGlyph>>>>,
    /// Gaps whose drawing threw, with when to try again and how long the
    /// next wait is. Neither a fact about this machine (so not `fallback`)
    /// nor something to redo every frame: a canvas that keeps throwing
    /// would otherwise cost a draw and a warning per gap per frame.
    failed: HashMap<(u8, String), (f64, f64)>,
    scratch: Renderer,
    /// The CSS font stack the fallback draws with.
    pub families: Rc<str>,
    /// Handed back wherever a glyph cannot be made right now. Shared, so
    /// the frozen path allocates nothing at all.
    blank: Rc<CachedGlyph>,
    /// No sprite may be allocated this frame; see `reserve`.
    frozen: bool,
    declined: u32,
    pub white_space: Sprite,
    pub filled_box: Sprite,
}

/// The scratch canvas is made on first use: a session that never meets a
/// missing glyph never touches the DOM for one, and a browser that will not
/// give us a 2D context is asked exactly once.
enum Renderer {
    Untried,
    Ready(crate::canvas::Scratch),
    Unavailable,
}

/// A glyph with nothing to draw. The emitter skips a `texture: None` glyph
/// but still advances the column, so a cell left out this way keeps the
/// line aligned.
fn blank_glyph() -> CachedGlyph {
    CachedGlyph {
        brightness_adjust: 1.0,
        has_color: false,
        texture: None,
        x_offset: PixelLength::new(0.0),
        y_offset: PixelLength::new(0.0),
        x_advance: PixelLength::new(0.0),
        bearing_x: PixelLength::new(0.0),
        bearing_y: PixelLength::new(0.0),
        scale: 1.0,
    }
}

impl GlyphCache {
    pub fn new(
        fonts: Rc<FontSet>,
        size_pt: f64,
        dpi: u32,
        texture: Rc<GpuTexture>,
        families: Rc<str>,
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
            blocks: HashMap::new(),
            shapes: HashMap::new(),
            fallback: HashMap::new(),
            failed: HashMap::new(),
            scratch: Renderer::Untried,
            families,
            blank: Rc::new(blank_glyph()),
            frozen: false,
            declined: 0,
            white_space,
            filled_box,
        })
    }

    pub fn texture(&self) -> &GpuTexture {
        &self.texture
    }

    /// The atlas texture, so this cache can be rebuilt over the same one --
    /// which is how the atlas gets cleared.
    pub fn texture_rc(&self) -> Rc<GpuTexture> {
        Rc::clone(&self.texture)
    }

    /// Start a frame. While `frozen`, nothing new is allocated and nothing
    /// is remembered; see `reserve`.
    pub fn begin_frame(&mut self, frozen: bool) {
        self.frozen = frozen;
        self.declined = 0;
    }

    /// Sprites this frame asked for and did not get.
    pub fn declined(&self) -> u32 {
        self.declined
    }

    /// Sprite allocation, and the one place that gives up.
    ///
    /// Once the atlas has grown to the GPU's largest texture and still has
    /// no room, the frame must reach `gpu.draw` anyway, so allocation stops
    /// failing and starts declining. Every caller has to say what it draws
    /// instead, which is why this returns an `Option` rather than taking a
    /// flag: a new kind of sprite that forgot the frozen case would not
    /// compile.
    ///
    /// Nothing declined is written to any cache. There is no invalidation
    /// to get wrong -- the glyph is simply drawn once there is room.
    ///
    /// (`filled_box` and `white_space` are allocated in `new`, before there
    /// is a `self` to freeze, and are the two sprites this always has.)
    fn reserve(&mut self, im: &dyn BitmapImage, tag: AtlasTag) -> Result<Option<Sprite>> {
        if self.frozen {
            self.declined += 1;
            return Ok(None);
        }
        Ok(Some(self.atlas.allocate_tagged(im, None, None, tag)?))
    }

    /// Shape one cluster's text, memoised: the same text with the same
    /// attributes shapes the same every frame.
    ///
    /// The gaps come back cached with the glyphs, which is the whole point:
    /// held separately, the second frame would hit this cache and no longer
    /// know which graphemes need drawing.
    pub fn shape(
        &mut self,
        text: &str,
        presentation: Option<termwiz::cell::Presentation>,
        presentation_width: Option<&thinkterm_font_core::PresentationWidth>,
        cells: usize,
    ) -> Result<Rc<WebShaped>> {
        let key = ShapeKey {
            text: text.to_string(),
            presentation: presentation.map(|p| match p {
                termwiz::cell::Presentation::Text => 0,
                termwiz::cell::Presentation::Emoji => 1,
            }),
            cells: cells.min(u16::MAX as usize) as u16,
        };
        if let Some(shaped) = self.shapes.get(&key) {
            return Ok(Rc::clone(shaped));
        }
        let shaped = Rc::new(self.fonts.shape_web(
            text,
            self.size_pt,
            self.dpi,
            presentation,
            thinkterm_font_core::Direction::LeftToRight,
            None,
            presentation_width,
            &crate::fallback::keeps_notdef,
        )?);
        // Flushing this is safe and cheap: `shape_web` is pure, so the next
        // frame derives the same answer again. The drawn fallbacks live in
        // their own table and are deliberately *not* flushed with it --
        // sharing one would make an ordinary scroll redraw a screenful of
        // CJK, at the moment that costs the most.
        if self.shapes.len() > 4096 {
            self.shapes.clear();
        }
        self.shapes.insert(key, Rc::clone(&shaped));
        Ok(shaped)
    }

    /// The scratch canvas, made on first use.
    ///
    /// A session that never meets a missing glyph never creates a DOM
    /// element for one. A browser that will not give us a 2D context is
    /// asked once per cache -- this is a field, so an atlas growth or clear
    /// rebuilds it along with everything else, which is what keeps it in
    /// step with the cell size.
    fn scratch(&mut self) -> Option<&crate::canvas::Scratch> {
        if matches!(self.scratch, Renderer::Untried) {
            let px = self.size_pt * self.dpi as f64 / 72.0;
            self.scratch = match crate::canvas::Scratch::new(
                self.metrics.cell_size,
                self.metrics.descender.get(),
                px,
                &self.families,
            ) {
                Ok(scratch) => Renderer::Ready(scratch),
                Err(err) => {
                    log::warn!("no canvas for fallback glyphs: {err:#}");
                    Renderer::Unavailable
                }
            };
        }
        match &self.scratch {
            Renderer::Ready(scratch) => Some(scratch),
            _ => None,
        }
    }

    /// Keep what the canvas produced, including that it produced nothing.
    fn remember(
        &mut self,
        gap: &Gap,
        cells: u8,
        glyph: Option<Rc<CachedGlyph>>,
    ) -> Option<Rc<CachedGlyph>> {
        // Every other table here is bounded by the atlas: fill it and the
        // whole cache is rebuilt. The `None` entries are not -- they hold no
        // sprite, so they never push the atlas towards that -- and a pane
        // spewing unassigned or private-use code points would grow this
        // without limit. Flushed wholesale rather than by LRU, like
        // `shapes`: what is lost is redrawn, at worst once.
        if self.fallback.values().map(HashMap::len).sum::<usize>() > MAX_FALLBACK_ENTRIES {
            self.fallback.clear();
        }
        self.fallback
            .entry(cells)
            .or_default()
            .insert(gap.text.clone(), glyph.clone());
        glyph
    }

    /// The glyph for a gap, drawn with the platform's own fonts.
    ///
    /// `Ok(None)` means **leave the missing-glyph box** -- which is what
    /// this machine's native terminal would show anyway, and is never worse
    /// than what the page does today.
    ///
    /// The order of the first four steps is the design:
    ///
    /// 1. A gap with no columns is refused. The `.notdef` it would replace
    ///    advances no columns; a fallback advances one, and the rest of the
    ///    line shifts with nothing to report it.
    /// 2. The cache is consulted before the budget, so scrolling back over
    ///    CJK already drawn costs nothing at all.
    /// 3. **The freeze is checked before the budget and before the canvas.**
    ///    Leaving it to `reserve` would draw the glyph and throw it away,
    ///    and -- worse -- would spend budget, producing a `deferred` that
    ///    asks for another frame and undoes the atlas backoff.
    /// 4. Only then is a glyph drawn.
    /// `cells` is the column count taken from the row being drawn, not
    /// `gap.cells`. The shape cache is keyed on a cluster's *total* width,
    /// so two rows that spend the same total differently -- `[1,2,1]` and
    /// `[2,1,1]` for the same text, which explicit cell widths allow --
    /// share an entry, and the second would inherit the first's per-gap
    /// count and shift everything after it.
    pub fn fallback_glyph(
        &mut self,
        gap: &Gap,
        cells: u8,
        budget: &mut FallbackBudget,
    ) -> Result<Option<Rc<CachedGlyph>>> {
        if cells == 0 {
            return Ok(None);
        }
        // Wider than the scratch canvas was built for: its ink would be cut
        // off at the edge and the bearing measured from the truncated box,
        // so the box is the honest answer. Nothing in a terminal is this
        // wide -- the widest cluster the shaper produces is two columns --
        // but `cells` comes from the row, and the row is data.
        if cells > crate::canvas::MAX_FALLBACK_CELLS {
            return Ok(self.remember(gap, cells, None));
        }
        if let Some(known) = self
            .fallback
            .get(&cells)
            .and_then(|by_text| by_text.get(&gap.text))
        {
            return Ok(known.clone());
        }
        if self.frozen {
            self.declined += 1;
            return Ok(None);
        }
        let now = crate::app::monotonic_ms();
        if let Some((retry_at, _)) = self.failed.get(&(cells, gap.text.clone())) {
            if now < *retry_at {
                return Ok(None);
            }
        }
        // The canvas is checked before the budget. Spending budget on a
        // browser that will not give us a 2D context produced `deferred`,
        // `deferred` asked for another frame, and the next frame was
        // byte-identical: a repaint loop at the display's refresh rate,
        // drawing nothing, on a device already constrained enough that
        // `getContext` failed.
        //
        // The verdict is latched for the life of this cache, so a context
        // refused under transient pressure is retried only when the cache is
        // next rebuilt -- a font size or dpr change, or an atlas growth. A
        // page that never gets one shows boxes until it is reloaded.
        let drawn = match self.scratch() {
            None => return Ok(None),
            Some(scratch) => {
                if !budget.take(crate::app::monotonic_ms()) {
                    return Ok(None);
                }
                scratch.glyph(&gap.text, cells)
            }
        };
        let drawn = match drawn {
            Ok(Some(drawn)) => drawn,
            // The browser has no font for it either, or it threw. Either
            // way this machine will not start being able to draw it, and
            // retrying every frame would redraw the same nothing for ever.
            Ok(None) => return Ok(self.remember(gap, cells, None)),
            Err(err) => {
                // Not a fact about this machine the way `Ok(None)` is:
                // `getImageData` can fail for reasons that pass, and writing
                // "undrawable" here left the grapheme a box for the life of
                // the cache. Not forgotten either: retried every frame, a
                // canvas that keeps throwing costs a draw and a warning per
                // gap per frame. So: a box now, another try after a wait
                // that doubles up to the atlas's own cap.
                if self.failed.len() > MAX_FALLBACK_ENTRIES {
                    self.failed.clear();
                }
                let backoff = self
                    .failed
                    .get(&(cells, gap.text.clone()))
                    .map(|(_, backoff)| (backoff * 2.0).min(crate::fallback::MAX_RETRY_MS))
                    .unwrap_or(crate::fallback::MIN_RETRY_MS);
                self.failed
                    .insert((cells, gap.text.clone()), (now + backoff, backoff));
                log::warn!(
                    "the canvas could not draw {:?}, retrying in {backoff:.0} ms: {err:#}",
                    gap.text
                );
                return Ok(None);
            }
        };
        if drawn.clipped {
            // The ink ran into the edge of the scratch canvas, so its
            // bounding box -- and every bearing derived from it -- describes
            // whatever fitted rather than the glyph. Caching that would put a
            // permanently mis-placed, half-drawn glyph on screen; the box is
            // the honest answer. The canvas has a full cell of margin on each
            // side, so this means genuinely enormous ink.
            log::warn!(
                "the fallback glyph for {:?} did not fit the scratch canvas; keeping the box",
                gap.text
            );
            return Ok(self.remember(gap, cells, None));
        }
        // Scaled here rather than at draw time, the way `load_glyph` does
        // it: the bearings were already scaled with it.
        // A colour glyph never arrives scaled; see `Scratch::glyph`.
        let image = if drawn.placement.scale != 1.0 {
            drawn.image.scale_by(drawn.placement.scale)
        } else {
            drawn.image
        };
        // `.context`, never `format!`: the downcast in `cached_glyph` and in
        // `frame` walks the root cause, and hiding an `OutOfTextureSpace`
        // behind a string turns a recoverable full atlas into a terminal
        // that never draws again.
        let Some(texture) = self
            .reserve(&image, AtlasTag::Glyph)
            .context("canvas fallback sprite")?
        else {
            return Ok(None);
        };
        let glyph = Rc::new(CachedGlyph {
            brightness_adjust: 1.0,
            has_color: drawn.has_color,
            texture: Some(texture),
            x_offset: PixelLength::new(0.0),
            y_offset: PixelLength::new(0.0),
            // The columns the row gave it, not what the canvas measured.
            x_advance: PixelLength::new(
                cells as f64 * self.metrics.cell_size.width as f64,
            ),
            bearing_x: PixelLength::new(drawn.placement.bearing_x),
            bearing_y: PixelLength::new(drawn.placement.bearing_y),
            scale: 1.0,
        });
        Ok(self.remember(gap, cells, Some(glyph)))
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
        if let Some(block) = info.only_char.and_then(BlockKey::from_char) {
            return self.block_glyph(block, num_cells);
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
        // Checked before rasterising, not just before allocating: a frozen
        // frame that rendered the outline and then threw it away would burn
        // the work for nothing.
        if self.frozen {
            self.declined += 1;
            return Ok(Rc::clone(&self.blank));
        }
        let glyph = match self.load_glyph(info, followed_by_space, num_cells) {
            Ok(Some(glyph)) => glyph,
            // The atlas declined it. Hand back a blank without remembering:
            // storing it here would turn "no room just now" into a cell
            // that stays empty until the whole cache is rebuilt.
            Ok(None) => return Ok(Rc::clone(&self.blank)),
            Err(err) => {
                if err.root_cause().downcast_ref::<OutOfTextureSpace>().is_some() {
                    return Err(err);
                }
                // A glyph that genuinely will not rasterise, on the other
                // hand, is worth remembering: it will not start working.
                log::error!("load_glyph failed; using blank instead: {err:#} {info:?}");
                Rc::new(blank_glyph())
            }
        };
        self.glyphs.insert(key, Rc::clone(&glyph));
        Ok(glyph)
    }

    /// Cell geometry has no font bearing or baseline padding. Cache it by
    /// shape and column count; atlas/DPR rebuilds discard this table too.
    fn block_glyph(&mut self, block: BlockKey, cells: u8) -> Result<Rc<CachedGlyph>> {
        if cells == 0 {
            return Ok(Rc::clone(&self.blank));
        }
        if let Some(glyph) = self.blocks.get(&(block, cells)) {
            return Ok(Rc::clone(glyph));
        }
        if self.frozen {
            self.declined += 1;
            return Ok(Rc::clone(&self.blank));
        }
        let metrics = BlockMetrics {
            cell_size: Size::new(
                self.metrics.cell_size.width * cells as isize,
                self.metrics.cell_size.height,
            ),
            underline_height: self.metrics.underline_height,
        };
        let buffer = block_image(block, &metrics, PolyAA::AntiAlias);
        let Some(texture) = self.reserve(&buffer, AtlasTag::Glyph).context("block sprite")? else {
            return Ok(Rc::clone(&self.blank));
        };
        let glyph = Rc::new(CachedGlyph {
            texture: Some(texture),
            x_advance: PixelLength::new(metrics.cell_size.width as f64),
            // Cancels the emitter's baseline to anchor at the cell top.
            bearing_y: self.metrics.descender
                + PixelLength::new(metrics.cell_size.height as f64),
            ..blank_glyph()
        });
        self.blocks.insert((block, cells), Rc::clone(&glyph));
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
    ) -> Result<Option<Rc<CachedGlyph>>> {
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
            // `Image::scale_by` truncates without a floor, so a glyph one
            // pixel tall scales away to nothing; the same clamp
            // `fallback::place` applies to the canvas path.
            let scale = scale
                .max(1.0 / raw_im.image_dimensions().0.max(1) as f64)
                .max(1.0 / raw_im.image_dimensions().1.max(1) as f64)
                .min(1.0);
            let (scale, raw_im) = if scale != 1.0 {
                (1.0, raw_im.scale_by(scale))
            } else {
                (scale, raw_im)
            };
            let Some(tex) = self.reserve(&raw_im, AtlasTag::Glyph)? else {
                return Ok(None);
            };
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
        Ok(Some(Rc::new(glyph)))
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
        // No underline this frame rather than no frame at all. After a
        // clear these are gone along with everything else, so re-allocating
        // them is exactly the moment the atlas is most likely to refuse.
        let Some(sprite) = self.reserve(&buffer, AtlasTag::Other)? else {
            return Ok(self.white_space.clone());
        };
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
        // Its own allocation, so it needs its own frozen branch -- this is
        // the entry point an earlier design missed, because `cached_glyph`
        // returns here before it reaches any of its own error handling.
        let Some(sprite) = self
            .reserve(&buffer, AtlasTag::Glyph)
            .context("braille sprite")?
        else {
            return Ok(Rc::clone(&self.blank));
        };
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
        // Same again: a missing cursor for a frame or two is survivable in
        // an emergency; a black screen is not.
        let Some(sprite) = self
            .reserve(&buffer, AtlasTag::Other)
            .context("cursor sprite")?
        else {
            return Ok(self.white_space.clone());
        };
        self.cursors.insert((shape, width), sprite.clone());
        Ok(sprite)
    }
}
