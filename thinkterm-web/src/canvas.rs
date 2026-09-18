//! The browser's side of glyph fallback: a 2D canvas as the painter and
//! `performance.now` as the clock. The procedure that uses them is in
//! `raster.rs`; the arithmetic in `fallback.rs`. This is what xterm.js's
//! WebGL renderer does too.

use crate::raster::{GlyphPlatform, Platform, ScratchGeometry, TextPainter};
use anyhow::{anyhow, Result};
use std::rc::Rc;
use wasm_bindgen::{JsCast, JsValue};

/// The fonts the canvas draws with, in CSS order.
///
/// The page gives the terminal no font stack of its own -- `#term` is a
/// bare canvas, and the two names in `ui/src/main.ts` identify the files we parse
/// ourselves, not CSS families -- so this is it, and `?glyphfont=`
/// replaces it.
///
/// This is also how the regional shape of a Han character is chosen. Not
/// `ctx.lang`: on this browser, switching it between Chinese and Japanese
/// moved not one pixel, while switching the face moved several hundred.
pub const DEFAULT_FAMILIES: &str =
    "ui-monospace, SFMono-Regular, Menlo, Consolas, \"DejaVu Sans Mono\", monospace";

pub struct WebPlatform;

impl GlyphPlatform for WebPlatform {
    fn now_ms(&self) -> f64 {
        crate::web_platform::WebPlatform::now_ms()
    }

    fn painter(&self, geometry: &ScratchGeometry, families: &str) -> Result<Box<dyn TextPainter>> {
        Ok(Box::new(CanvasPainter::new(geometry, families)?))
    }
}

pub fn platform() -> Platform {
    Rc::new(WebPlatform)
}

/// A canvas kept aside, sized to the scratch geometry, drawing with the
/// requested font stack.
pub struct CanvasPainter {
    ctx: web_sys::CanvasRenderingContext2d,
    geometry: ScratchGeometry,
}

impl CanvasPainter {
    pub fn new(geometry: &ScratchGeometry, families: &str) -> Result<Self> {
        let document = web_sys::window()
            .and_then(|w| w.document())
            .ok_or_else(|| anyhow!("no document"))?;
        let canvas: web_sys::HtmlCanvasElement = document
            .create_element("canvas")
            .map_err(|e| anyhow!("canvas: {e:?}"))?
            .dyn_into()
            .map_err(|_| anyhow!("created element is not a canvas"))?;
        canvas.set_width(geometry.width as u32);
        canvas.set_height(geometry.height as u32);

        let options = js_sys::Object::new();
        let set = |key: &str, value: bool| {
            js_sys::Reflect::set(&options, &JsValue::from_str(key), &JsValue::from_bool(value))
                .map_err(|e| anyhow!("context option {key}: {e:?}"))
        };
        set("alpha", true)?;
        set("willReadFrequently", true)?;
        let ctx: web_sys::CanvasRenderingContext2d = canvas
            .get_context_with_context_options("2d", &options)
            .map_err(|e| anyhow!("2d context: {e:?}"))?
            .ok_or_else(|| anyhow!("no 2d context"))?
            .dyn_into()
            .map_err(|_| anyhow!("2d context has the wrong type"))?;
        let size = format!("{}px", geometry.px);
        ctx.set_font(&format!("{size} {families}"));
        if !ctx.font().starts_with(&size) {
            log::warn!(
                "the browser would not take the font {families:?} (it kept {:?}); \
                 using the built-in stack",
                ctx.font()
            );
            ctx.set_font(&format!("{size} {DEFAULT_FAMILIES}"));
        }
        if !ctx.font().starts_with(&size) {
            log::warn!(
                "the canvas font is {:?} rather than {size}; fallback glyphs may be mis-sized",
                ctx.font()
            );
        }
        ctx.set_text_baseline("alphabetic");
        Ok(Self {
            ctx,
            geometry: *geometry,
        })
    }
}

impl TextPainter for CanvasPainter {
    fn paint(&self, text: &str, fill: [u8; 3]) -> Result<Vec<u8>> {
        let g = &self.geometry;
        self.ctx.clear_rect(0.0, 0.0, g.width as f64, g.height as f64);
        self.ctx
            .set_fill_style_str(&format!("#{:02x}{:02x}{:02x}", fill[0], fill[1], fill[2]));
        self.ctx
            .fill_text(text, g.pen_x, g.baseline)
            .map_err(|e| anyhow!("fillText: {e:?}"))?;
        Ok(self
            .ctx
            .get_image_data(0.0, 0.0, g.width as f64, g.height as f64)
            .map_err(|e| anyhow!("getImageData: {e:?}"))?
            .data()
            .0)
    }
}
