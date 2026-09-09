//! A 2D canvas kept aside for drawing the glyphs the bundled faces cannot.
//!
//! The page ships two font files and does its own shaping, so it has no
//! system fallback chain: CJK, Hangul, emoji and a scattering of symbols
//! have no glyph at all. The browser, however, has every font the machine
//! has installed, and `fillText` will use them. So the missing graphemes
//! are drawn here, read back with `getImageData` and put into the same
//! atlas as everything else; WebGPU then draws them like any other glyph.
//! This is what xterm.js's WebGL renderer does.
//!
//! Only the DOM lives here. The arithmetic -- ink bounds, the colour
//! decision, the bearing -- is in `fallback.rs`, where it can be tested.

use crate::fallback::{self, Ink, Placement};
use anyhow::{anyhow, Result};
use thinkterm_render::bitmaps::Image;
use thinkterm_render::geom::Size;
use wasm_bindgen::{JsCast, JsValue};

/// The widest glyph we will draw. Wider than any terminal cluster we expect
/// and cheap to be generous about: the scratch canvas is a few hundred
/// kilobytes at the largest cell size.
pub const MAX_FALLBACK_CELLS: u8 = 4;

/// The fonts the canvas draws with, in CSS order.
///
/// The page gives the terminal no font stack of its own -- `#term` is a
/// bare canvas, and the two names in `app.js` identify the files we parse
/// ourselves, not CSS families -- so this is it, and `?glyphfont=`
/// replaces it.
///
/// This is also how the regional shape of a Han character is chosen. Not
/// `ctx.lang`: on this browser, switching it between Chinese and Japanese
/// moved not one pixel, while switching the face moved several hundred.
pub const DEFAULT_FAMILIES: &str =
    "ui-monospace, SFMono-Regular, Menlo, Consolas, \"DejaVu Sans Mono\", monospace";

/// Two fills that differ in **every** channel.
///
/// A glyph that takes our colour looks different under the two; one that
/// carries its own looks the same. Sharing a channel between them -- white
/// and red both have full red -- is what made the first measurements call
/// every Chinese character a colour glyph.
const FILL_A: &str = "#ff0000";
const FILL_B: &str = "#00ffff";

/// A glyph the browser drew for us.
pub struct Drawn {
    /// The ink, cropped, as straight (not premultiplied) RGBA.
    pub image: Image,
    pub placement: Placement,
    pub has_color: bool,
    /// The ink reached the edge of the scratch canvas, so it may have been
    /// cut off. Reported rather than corrected: growing the canvas per
    /// glyph would defeat the fixed geometry below.
    pub clipped: bool,
}

/// The off-screen canvas the fallback glyphs are drawn on.
pub struct Scratch {
    ctx: web_sys::CanvasRenderingContext2d,
    width: usize,
    height: usize,
    /// Where `fillText` is told to start, and the baseline it sits on --
    /// both inside the scratch canvas, not the terminal.
    pen_x: f64,
    baseline: f64,
    cell_width: f64,
}

impl Scratch {
    /// A canvas sized once, for a cell of this size.
    ///
    /// The size never changes afterwards. Setting either dimension of a
    /// canvas resets every attribute of its 2D context -- font, fill,
    /// baseline, the lot -- so a canvas that resized would have to reinstate
    /// all of them, and forgetting one would show up as glyphs drawn at the
    /// wrong size with nothing to say why.
    pub fn new(cell: Size, descender: f64, px: f64, families: &str) -> Result<Self> {
        let document = web_sys::window()
            .and_then(|w| w.document())
            .ok_or_else(|| anyhow!("no document"))?;
        let canvas: web_sys::HtmlCanvasElement = document
            .create_element("canvas")
            .map_err(|e| anyhow!("canvas: {e:?}"))?
            .dyn_into()
            .map_err(|_| anyhow!("created element is not a canvas"))?;
        // Never added to the document. `getContext`, `fillText` and
        // `getImageData` all work on a detached element, and the page has no
        // styling that would find a stray canvas anyway.
        let (width, height) = (
            (MAX_FALLBACK_CELLS as usize + 2) * cell.width.max(1) as usize,
            2 * cell.height.max(1) as usize,
        );
        canvas.set_width(width as u32);
        canvas.set_height(height as u32);

        let options = js_sys::Object::new();
        let set = |key: &str, value: bool| {
            js_sys::Reflect::set(&options, &JsValue::from_str(key), &JsValue::from_bool(value))
                .map_err(|e| anyhow!("context option {key}: {e:?}"))
        };
        set("alpha", true)?;
        // Every draw here is followed by a read. Without this the browser
        // keeps the canvas on the GPU and each `getImageData` stalls on a
        // readback: measured at 1053 us per glyph rather than 54.
        set("willReadFrequently", true)?;
        let ctx: web_sys::CanvasRenderingContext2d = canvas
            .get_context_with_context_options("2d", &options)
            .map_err(|e| anyhow!("2d context: {e:?}"))?
            .ok_or_else(|| anyhow!("no 2d context"))?
            .dyn_into()
            .map_err(|_| anyhow!("2d context has the wrong type"))?;
        // The canvas `font` setter silently ignores anything that is not
        // a valid CSS `font` shorthand and keeps what it had, which on a
        // fresh context is the spec default `10px sans-serif`. `families`
        // comes straight from `?glyphfont=`, so one stray comma -- or a
        // family whose name starts with a digit, which CSS will not take
        // unquoted -- would draw every glyph at 10px into a cell several
        // times that size, with nothing anywhere saying so. Read it back.
        //
        // Whole pixels, because the browser hands the string back in its own
        // serialised form: it turns `17.333333333333332px` into
        // `17.3333px`, and comparing those two is how the first version of
        // this check decided every font had been refused.
        let px = px.max(1.0).round();
        let size = format!("{px}px");
        ctx.set_font(&format!("{size} {families}"));
        if !ctx.font().starts_with(&size) {
            log::warn!(
                "the browser would not take the font {families:?} (it kept {:?}); \
                 using the built-in stack",
                ctx.font()
            );
            ctx.set_font(&format!("{size} {DEFAULT_FAMILIES}"));
        }
        // Reported, never fatal. Refusing here leaves every fallback
        // grapheme a box for the life of this cache -- far worse than glyphs
        // drawn at the wrong size, which is all this check was ever meant to
        // catch.
        if !ctx.font().starts_with(&size) {
            log::warn!(
                "the canvas font is {:?} rather than {size}; fallback glyphs may be mis-sized",
                ctx.font()
            );
        }
        ctx.set_text_baseline("alphabetic");

        // A cell of margin on the left and half a cell above, so a glyph
        // that overhangs its cell is measured rather than clipped.
        let pen_x = cell.width as f64;
        let row_top = cell.height as f64 / 2.0;
        // The same baseline the emitter uses: it puts a sprite's top at
        // `cell_height + descender - bearing_y`, and `cell_height +
        // descender` is the baseline. Kept in floating point, because the
        // emitter subtracts exactly this number.
        let baseline = row_top + cell.height as f64 + descender;
        Ok(Self {
            ctx,
            width,
            height,
            pen_x,
            baseline,
            cell_width: cell.width as f64,
        })
    }

    fn clear(&self) {
        self.ctx
            .clear_rect(0.0, 0.0, self.width as f64, self.height as f64);
    }

    fn draw(&self, text: &str, fill: &str) -> Result<()> {
        self.clear();
        self.ctx.set_fill_style_str(fill);
        self.ctx
            .fill_text(text, self.pen_x, self.baseline)
            .map_err(|e| anyhow!("fillText: {e:?}"))
    }

    fn read(&self, x: f64, y: f64, w: f64, h: f64) -> Result<Vec<u8>> {
        Ok(self
            .ctx
            .get_image_data(x, y, w, h)
            .map_err(|e| anyhow!("getImageData: {e:?}"))?
            .data()
            .0)
    }

    /// Draw one grapheme twice and read back what happened.
    ///
    /// The first draw says where the ink is; the second says whether the
    /// glyph took the colour we asked for or brought its own. The second is
    /// read back over the ink alone -- strictly less work, and it comes out
    /// packed the same way as the first crop, so the two compare directly.
    ///
    /// `Ok(None)` means the browser drew nothing, which is what a machine
    /// with no font for this grapheme does.
    fn render(&self, text: &str) -> Result<Option<(Ink, Vec<u8>, bool)>> {
        self.draw(text, FILL_A)?;
        let first = self.read(0.0, 0.0, self.width as f64, self.height as f64)?;
        let Some(ink) = fallback::ink_box(&first, self.width, self.height) else {
            return Ok(None);
        };
        let cropped = fallback::crop(&first, self.width, &ink);
        self.draw(text, FILL_B)?;
        let second = self.read(
            ink.left as f64,
            ink.top as f64,
            ink.width as f64,
            ink.height as f64,
        )?;
        let has_color = fallback::carries_own_colour(&cropped, &second);
        Ok(Some((ink, cropped, has_color)))
    }

    /// How wide the ink may be before it is scaled down, in the same terms
    /// `load_glyph` uses: the columns the row gave it, plus a quarter.
    ///
    /// The columns come from the row and never from the canvas.
    /// `measureText` would offer an advance, and an advance is not a column
    /// count -- Hangul measured 1.73 cells and the arrows 1.2 -- which is
    /// why its web-sys feature is deliberately not enabled.
    fn max_width(&self, cells: u8) -> f64 {
        self.cell_width * (cells.max(1) as f64 + 0.25)
    }

    /// The width limit a glyph is placed against. A colour glyph gets none:
    /// see `glyph`.
    fn limit(&self, cells: u8, has_color: bool) -> f64 {
        if has_color {
            f64::INFINITY
        } else {
            self.max_width(cells)
        }
    }

    /// One grapheme, ready for the atlas.
    pub fn glyph(&self, text: &str, cells: u8) -> Result<Option<Drawn>> {
        let Some((ink, cropped, has_color)) = self.render(text)? else {
            return Ok(None);
        };
        // A colour glyph is given no width limit, so it is never scaled.
        // `getImageData` returns straight RGBA, in which a fully transparent
        // pixel is (0,0,0,0), and the resampler mixes channels without
        // regard to alpha -- so every partly transparent pixel is dragged
        // towards black and a scaled emoji picks up a dark halo. Letting it
        // overflow its cells is what wide glyphs already do, and it costs
        // almost nothing here: the browser draws emoji at the cell size, so
        // their ink fits inside the quarter-cell of slack anyway. Ink that
        // overflows so far that it hits the edge of this canvas is refused
        // by the caller rather than placed from a truncated box.
        Ok(Some(Drawn {
            image: Image::from_raw(ink.width, ink.height, cropped),
            placement: fallback::place(
                &ink,
                self.pen_x,
                self.baseline,
                self.limit(cells, has_color),
            ),
            has_color,
            clipped: ink.clipped,
        }))
    }

    /// One grapheme, reported rather than drawn. Only the probe uses this.
    pub fn measure(&self, text: &str, cells: u8) -> Result<Option<(Ink, Placement, bool)>> {
        let Some((ink, _, has_color)) = self.render(text)? else {
            return Ok(None);
        };
        // The same limit `glyph` uses, or the probe would report a scale
        // the terminal never applies.
        let placement =
            fallback::place(&ink, self.pen_x, self.baseline, self.limit(cells, has_color));
        Ok(Some((ink, placement, has_color)))
    }

    /// What this canvas is doing, for the `?check=fallback` probe.
    pub fn geometry(&self) -> (usize, usize, f64, f64) {
        (self.width, self.height, self.pen_x, self.baseline)
    }
}
