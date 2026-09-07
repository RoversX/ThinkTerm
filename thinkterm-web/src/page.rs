//! What the page calls: `start` brings a pane up on a canvas, and
//! `color_check` is the smoke test's numeric colour assertion.

use crate::app::{App, Setup};
use crate::attach::attach;
use crate::glyphs::GlyphCache;
use crate::gpu::Gpu;
use crate::host::{LocalSpawner, WebClock, WebConfig, WebEvents, WebHost};
use crate::link::WsLink;
use anyhow::{anyhow, Context, Result};
use std::rc::Rc;
use std::sync::atomic::AtomicUsize;
use std::sync::Arc;
use thinkterm_font_web::{Face, FontSet};
use thinkterm_render::bitmaps::BitmapImage;
use thinkterm_render::pipeline::GpuTexture;
use thinkterm_render::quad::HeapQuadAllocator;
use thinkterm_session::pane::PaneSession;
use thinkterm_session::{Lock, SessionConfig};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;

fn js_err(e: anyhow::Error) -> JsValue {
    JsValue::from_str(&format!("{e:#}"))
}

fn element<T: JsCast>(id: &str) -> Result<T> {
    web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.get_element_by_id(id))
        .ok_or_else(|| anyhow!("no element #{id}"))?
        .dyn_into::<T>()
        .map_err(|_| anyhow!("#{id} is not the expected element"))
}

struct ConsoleLog;
impl log::Log for ConsoleLog {
    fn enabled(&self, m: &log::Metadata) -> bool {
        m.level() <= log::Level::Info
    }
    fn log(&self, record: &log::Record) {
        if self.enabled(record.metadata()) {
            web_sys::console::log_1(&JsValue::from_str(&format!(
                "{} {}",
                record.level(),
                record.args()
            )));
        }
    }
    fn flush(&self) {}
}
static LOGGER: ConsoleLog = ConsoleLog;

#[wasm_bindgen(start)]
pub fn init() {
    console_error_panic_hook::set_once();
    let _ = log::set_logger(&LOGGER).map(|()| log::set_max_level(log::LevelFilter::Info));
}

/// Bring the server's active pane up on `canvas_id`. `fonts` are the face
/// files the page fetched, base face first.
#[wasm_bindgen]
pub async fn start(
    canvas_id: String,
    textarea_id: String,
    status_id: String,
    url: String,
    token: String,
    font_names: Vec<String>,
    fonts: Vec<js_sys::Uint8Array>,
    size_pt: f64,
) -> Result<(), JsValue> {
    run(canvas_id, textarea_id, status_id, url, token, font_names, fonts, size_pt)
        .await
        .map_err(js_err)
}

#[allow(clippy::too_many_arguments)]
async fn run(
    canvas_id: String,
    textarea_id: String,
    status_id: String,
    url: String,
    token: String,
    font_names: Vec<String>,
    fonts: Vec<js_sys::Uint8Array>,
    size_pt: f64,
) -> Result<()> {
    let canvas: web_sys::HtmlCanvasElement = element(&canvas_id)?;
    let textarea: web_sys::HtmlTextAreaElement = element(&textarea_id)?;
    let status: Option<web_sys::Element> = element(&status_id).ok();
    let set_status = |text: &str| {
        if let Some(s) = &status {
            s.set_text_content(Some(text));
        }
    };
    let window = web_sys::window().ok_or_else(|| anyhow!("no window"))?;
    if js_sys::Reflect::get(&window.navigator(), &JsValue::from_str("gpu"))
        .map(|v| v.is_undefined())
        .unwrap_or(true)
    {
        anyhow::bail!(
            "this browser exposes no WebGPU here. It needs a secure context (https, or \
             http://localhost) and a browser with WebGPU enabled"
        );
    }

    set_status("loading fonts…");
    let mut faces = Vec::new();
    for (name, data) in font_names.iter().zip(fonts.iter()) {
        faces.push(Face::new(name, data.to_vec(), 0).with_context(|| name.clone())?);
    }
    let fonts = Rc::new(FontSet::new(faces)?);
    let dpr = window.device_pixel_ratio();
    let dpi = (96.0 * dpr) as u32;

    set_status("connecting…");
    let link = WsLink::connect(&url, &token).await?;

    set_status("starting WebGPU…");
    let rect = canvas.get_bounding_client_rect();
    let dev_w = (rect.width() * dpr).floor().max(1.0) as u32;
    let dev_h = (rect.height() * dpr).floor().max(1.0) as u32;
    canvas.set_width(dev_w);
    canvas.set_height(dev_h);
    let gpu = Gpu::new(canvas.clone(), dev_w, dev_h).await?;
    let atlas_side = 1024u32.min(gpu.max_texture_dimension());
    let texture = Rc::new(GpuTexture::new(&gpu.device, Arc::clone(&gpu.queue), atlas_side, atlas_side)?);
    let glyphs = GlyphCache::new(Rc::clone(&fonts), size_pt, dpi, texture)?;
    let (cw, ch) = (
        glyphs.metrics.cell_size.width as u32,
        glyphs.metrics.cell_size.height as u32,
    );
    // A page too small for a cell (hidden, collapsed, mid-layout) reports
    // no viewport: it follows the pane's own size until it has a real one.
    let size = crate::app::grid_for(dev_w, dev_h, cw, ch).map(|(cols, rows)| {
        wezterm_term::TerminalSize {
            rows,
            cols,
            pixel_width: cols * cw as usize,
            pixel_height: rows * ch as usize,
            dpi,
        }
    });

    set_status("attaching…");
    let attached = attach(&link, size).await?;
    let (cols, rows) = match size {
        Some(size) => (size.cols, size.rows),
        None => (attached.dims.cols, attached.dims.viewport_rows),
    };
    log::info!(
        "attached to pane {} in tab {} on {} ({}x{})",
        attached.pane_id,
        attached.tab_id,
        attached.server_version,
        cols,
        rows
    );

    let host = Arc::new(WebHost {
        clock: WebClock::new(),
        spawner: LocalSpawner,
        events: WebEvents::default(),
        link: link.clone(),
        config: WebConfig::default(),
    });
    let session = PaneSession::new(
        Arc::clone(&host),
        Arc::new(Lock::new(thinkterm_session::images::ImageStore::default())),
        SessionConfig {
            scrollback_lines: 3500,
            local_echo_threshold_ms: Some(100),
            overlay_lag_indicator: false,
        },
        attached.pane_id,
        Arc::new(AtomicUsize::new(attached.tab_id)),
        0,
        attached.dims,
        &attached.title,
        attached.alt_screen,
    );

    let app = App::new(Setup {
        link: link.clone(),
        session,
        host: Arc::clone(&host),
        gpu,
        glyphs,
        fonts,
        canvas: canvas.clone(),
        textarea: textarea.clone(),
        status,
        pane_id: attached.pane_id,
        tab_id: attached.tab_id,
        dpr,
        cols,
        rows,
        title: attached.title.clone(),
    });
    host.events.set_wake(app.wake());
    {
        let app = Rc::clone(&app);
        link.set_push_handler(move |pdu| app.on_push(pdu));
    }
    {
        let app = Rc::clone(&app);
        link.set_close_handler(move |reason| app.on_close(reason));
    }
    crate::input::install(Rc::clone(&app), &canvas, &textarea);
    let _ = textarea.focus();
    app.resize();
    app.request_frame();
    // The push and close handlers and every DOM listener hold the app.
    Ok(())
}

/// Render a mid-grey quad and read the pixel back: linear 0.5 must come
/// out as sRGB 188 (±1), which proves the surface is being encoded.
#[wasm_bindgen]
pub async fn color_check(canvas_id: String) -> Result<String, JsValue> {
    async fn run(canvas_id: String) -> Result<String> {
        let canvas: web_sys::HtmlCanvasElement = element(&canvas_id)?;
        canvas.set_width(64);
        canvas.set_height(64);
        let mut gpu = Gpu::new(canvas, 64, 64).await?;
        let texture = GpuTexture::new(&gpu.device, Arc::clone(&gpu.queue), 64, 64)?;
        // A white filled box in the atlas, like UtilSprites::filled_box.
        let mut image = thinkterm_render::bitmaps::Image::new(8, 8);
        image.clear_rect(
            thinkterm_render::geom::Rect::new(
                thinkterm_render::geom::Point::new(0, 0),
                thinkterm_render::geom::Size::new(8, 8),
            ),
            wezterm_color_types::SrgbaPixel::rgba(0xff, 0xff, 0xff, 0xff),
        );
        let surface: Rc<dyn thinkterm_render::bitmaps::Texture2d> = Rc::new(texture);
        let mut atlas = thinkterm_render::atlas::Atlas::new(&surface)?;
        let sprite = atlas.allocate(&image)?;
        let mut quads = HeapQuadAllocator::default();
        {
            use thinkterm_render::quad::{QuadTrait, TripleLayerQuadAllocatorTrait};
            let mut quad = quads.allocate(0)?;
            quad.set_position(-32.0, -32.0, 32.0, 32.0);
            quad.set_texture(sprite.texture_coords());
            quad.set_is_background();
            quad.set_fg_color(wezterm_color_types::LinearRgba::with_components(0.5, 0.5, 0.5, 1.0));
            quad.set_hsv(None);
        }
        let mut vertices = Vec::new();
        quads.extract_vertices(&mut vertices);
        let gpu_texture = surface
            .downcast_ref::<GpuTexture>()
            .ok_or_else(|| anyhow!("atlas texture type"))?;
        let pixel = gpu.draw_and_read_pixel(&vertices, gpu_texture, 32, 32).await?;
        let expected = 188u8;
        let ok = pixel[..3].iter().all(|c| (*c as i32 - expected as i32).abs() <= 1);
        Ok(format!(
            "{{\"pixel\":[{},{},{},{}],\"expected\":{expected},\"ok\":{ok},\"adapter\":\"{}\",\"format\":\"{:?}\"}}",
            pixel[0], pixel[1], pixel[2], pixel[3], gpu.adapter_info.name, gpu.pipeline.target_format
        ))
    }
    run(canvas_id).await.map_err(js_err)
}
