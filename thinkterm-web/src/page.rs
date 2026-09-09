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
use std::sync::Arc;
use thinkterm_font_core::FontShaper;
use thinkterm_font_web::{Face, FontSet};
use thinkterm_render::bitmaps::BitmapImage;
use thinkterm_render::pipeline::GpuTexture;
use thinkterm_render::quad::HeapQuadAllocator;
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
    // `glyph_font` is the CSS font stack the glyph fallback draws with,
    // from `?glyphfont=`; empty means the built-in list.
    glyph_font: String,
    // `?font=` was given: keep that size rather than the desktop's cell.
    font_pinned: bool,
) -> Result<(), JsValue> {
    run(
        canvas_id, textarea_id, status_id, url, token, font_names, fonts, size_pt, glyph_font,
        font_pinned,
    )
    .await
    .map_err(js_err)
}

/// The stack to hand the fallback canvas.
fn families(requested: &str) -> Rc<str> {
    match requested.trim() {
        "" => crate::canvas::DEFAULT_FAMILIES.into(),
        given => given.into(),
    }
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
    glyph_font: String,
    font_pinned: bool,
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
    // From here on a failure must hand the socket back: without this the
    // server keeps a registered client and a TCP session for a page that
    // gave up, and the reader keeps answering its pings.
    let outcome = start_attached(
        &link, &canvas, &textarea, status.clone(), &url, &token, fonts, size_pt, &glyph_font, dpr,
        dpi, font_pinned,
    )
    .await;
    if outcome.is_err() {
        link.shutdown();
    }
    outcome
}

async fn start_attached(
    link: &WsLink,
    canvas: &web_sys::HtmlCanvasElement,
    textarea: &web_sys::HtmlTextAreaElement,
    status: Option<web_sys::Element>,
    url: &str,
    token: &str,
    fonts: Rc<FontSet>,
    size_pt: f64,
    glyph_font: &str,
    dpr: f64,
    dpi: u32,
    font_pinned: bool,
) -> Result<()> {
    let set_status = |text: &str| {
        if let Some(s) = &status {
            s.set_text_content(Some(text));
        }
    };
    set_status("starting WebGPU…");
    let rect = canvas.get_bounding_client_rect();
    let dev_w = (rect.width() * dpr).floor().max(1.0) as u32;
    let dev_h = (rect.height() * dpr).floor().max(1.0) as u32;
    canvas.set_width(dev_w);
    canvas.set_height(dev_h);
    let gpu = Gpu::new(canvas.clone(), dev_w, dev_h).await?;
    let atlas_side = 1024u32.min(gpu.max_texture_dimension());
    let texture = Rc::new(GpuTexture::new(&gpu.device, Arc::clone(&gpu.queue), atlas_side, atlas_side)?);
    let glyphs = GlyphCache::new(Rc::clone(&fonts), size_pt, dpi, texture, families(glyph_font))?;
    let (cw, ch) = (
        glyphs.metrics.cell_size.width as u32,
        glyphs.metrics.cell_size.height as u32,
    );
    // A page too small for a cell (hidden, collapsed, mid-layout) reports
    // no viewport: it follows the pane's own size until it has a real one.
    // Inside the desktop's window padding: a cell left and right, half
    // a cell top and bottom.
    let size = crate::app::grid_for(dev_w.saturating_sub(2 * cw), dev_h.saturating_sub(ch), cw, ch).map(|(cols, rows)| {
        wezterm_term::TerminalSize {
            rows,
            cols,
            pixel_width: cols * cw as usize,
            pixel_height: rows * ch as usize,
            dpi,
        }
    });

    set_status("attaching…");
    // Rows the bar above each pane takes, for the first report.
    let nav_rows = crate::navbar::nav_rows(crate::navbar::nav_css(ch as f64 / dpr, None) * dpr, ch as f64);
    let attached = attach(&link, size, nav_rows).await?;
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
    let images = Arc::new(thinkterm_session::Lock::new(
        thinkterm_session::images::ImageStore::default(),
    ));
    let remote_tab_id = Arc::new(std::sync::atomic::AtomicUsize::new(attached.tab_id));
    let session = crate::app::build_session(
        &host,
        &images,
        &remote_tab_id,
        attached.pane_id,
        attached.dims,
        &attached.title,
        attached.alt_screen,
    );

    let app = App::new(Setup {
        link: link.clone(),
        host: Arc::clone(&host),
        images,
        remote_tab_id,
        pane: crate::app::PaneCell::new(session, &attached.title),
        gpu,
        glyphs,
        fonts,
        canvas: canvas.clone(),
        textarea: textarea.clone(),
        status,
        url: url.to_string(),
        token: token.to_string(),
        pane_id: attached.pane_id,
        tab_id: attached.tab_id,
        window_id: attached.window_id,
        workspace: attached.workspace.clone(),
        dpr,
        cols,
        rows,
        strip: crate::chrome::TabStrip::mount("tabs"),
        navs: crate::navbar::NavBars::mount("panes"),
        side: crate::sidebar::Sidebar::mount("side"),
        font_pinned,
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
    crate::input::install(Rc::clone(&app), canvas, textarea);
    // The strip: one listener on its root, so rebuilding its contents
    // costs nothing to keep wired.
    for root in [app.strip_element(), app.navs_element()].into_iter().flatten() {
        let app = Rc::clone(&app);
        crate::input::listen::<web_sys::MouseEvent>(&root, "click", move |ev| {
            if let Some(click) = crate::chrome::TabStrip::click_target(&ev) {
                ev.prevent_default();
                app.on_chrome_click(click);
            }
        });
    }
    if let (Some(root), Some(window)) = (app.side_element(), web_sys::window()) {
        let app2 = Rc::clone(&app);
        crate::input::listen::<web_sys::MouseEvent>(&root, "click", move |ev| {
            if let Some(click) = crate::sidebar::Sidebar::click_target(&ev) {
                ev.prevent_default();
                app2.on_side_click(click);
            }
        });
        {
            let app = Rc::clone(&app);
            crate::input::listen::<web_sys::KeyboardEvent>(&root, "keydown", move |ev| {
                let Some(input) = ev.target().and_then(|t| t.dyn_into::<web_sys::HtmlInputElement>().ok()) else {
                    return;
                };
                let key = ev.key();
                if key == "Enter" || key == "Escape" {
                    ev.prevent_default();
                    app.on_side_key(&key, input.value());
                }
                ev.stop_propagation();
            });
        }
        // The resize handle: drag the panel's edge, within the desktop's bounds.
        let dragging = Rc::new(std::cell::Cell::new(false));
        {
            let dragging = Rc::clone(&dragging);
            crate::input::listen::<web_sys::PointerEvent>(&root, "pointerdown", move |ev| {
                let target: Option<web_sys::Element> = ev.target().and_then(|t| t.dyn_into().ok());
                if target.is_some_and(|t| t.get_attribute("class").as_deref() == Some("handle")) {
                    dragging.set(true);
                    ev.prevent_default();
                }
            });
        }
        {
            let dragging = Rc::clone(&dragging);
            let app = Rc::clone(&app);
            crate::input::listen::<web_sys::PointerEvent>(&window, "pointermove", move |ev| {
                if dragging.get() {
                    app.set_sidebar_width(ev.client_x() as f64);
                }
            });
        }
        crate::input::listen::<web_sys::PointerEvent>(&window, "pointerup", move |_| dragging.set(false));
        if let Some(px) = crate::sidebar::stored_width() {
            app.set_sidebar_width(px);
        }
    }
    app.fetch_tree();
    // The boot messages were shown in the status element directly; from
    // here it is the page's passing remark, shown only when there is one.
    app.hide_status();
    app.refresh_layout();
    app.poll_layout(5_000);
    let _ = textarea.focus();
    app.match_desktop_cell();
    app.resize();
    app.request_frame();
    // A moment after the first frame, in its own task: the fallback
    // canvas's first use is a one-off stall, and this is when nobody is
    // waiting on it. Not at 0 ms, which could land before the first paint
    // and the status line, and did show up in the attach time.
    {
        let app = Rc::clone(&app);
        let warm = Closure::once_into_js(move || app.warm_glyph_canvas());
        if let Some(window) = web_sys::window() {
            let _ = window.set_timeout_with_callback_and_timeout_and_arguments_0(
                warm.as_ref().unchecked_ref(),
                250,
            );
        }
    }
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

/// Every kind of glyph the fallback has to get right, and whether it should
/// come back carrying its own colour.
///
/// `\u{2713}` and `\u{2714}` sit together on purpose: JetBrains Mono has the
/// first and not the second, so they come from different fonts while looking
/// almost the same. That is the mismatch most likely to be reported.
const SAMPLES: &[(&str, &str, u8, bool)] = &[
    ("latin", "A", 1, false),
    ("braille", "\u{2801}", 1, false),
    ("han", "\u{4e2d}", 2, false),
    ("han-punct", "\u{3002}", 2, false),
    ("han-bracket", "\u{300c}", 2, false),
    ("kana", "\u{3042}", 2, false),
    ("han-shared-1", "\u{76f4}", 2, false),
    ("han-shared-2", "\u{9aa8}", 2, false),
    ("hangul", "\u{d55c}", 2, false),
    ("combining", "e\u{301}", 1, false),
    // rustybuzz clusters this Burmese syllable as one where UAX#29 sees
    // three graphemes -- the case that used to make the vowel signs vanish.
    ("burmese", "\u{1005}\u{102c}\u{1038}", 1, false),
    ("arabic", "\u{628}", 1, false),
    ("hebrew", "\u{5d0}", 1, false),
    ("thai", "\u{e01}", 1, false),
    ("devanagari", "\u{915}", 1, false),
    ("check-present", "\u{2713}", 1, false),
    ("check-missing", "\u{2714}", 1, false),
    ("cross", "\u{2718}", 1, false),
    ("star", "\u{2605}", 1, false),
    ("info", "\u{2139}", 1, false),
    ("arrow", "\u{27a4}", 1, false),
    ("return", "\u{21b5}", 1, false),
    ("circle", "\u{25d0}", 1, false),
    ("gear-text", "\u{2699}\u{fe0e}", 1, false),
    ("gear-emoji", "\u{2699}\u{fe0f}", 2, true),
    ("emoji", "\u{1f600}", 2, true),
    ("emoji-zwj", "\u{1f468}\u{200d}\u{1f4bb}", 2, true),
    ("emoji-flag", "\u{1f3f3}\u{fe0f}\u{200d}\u{1f308}", 2, true),
    ("emoji-heart", "\u{2764}\u{fe0f}", 2, true),
];

/// Draw the sample matrix on a scratch canvas and report what happened, as
/// JSON, with the assertions already evaluated.
///
/// This needs no server, no token, no pane and **no WebGPU**, which makes it
/// the only automatic check that can run on Safari, on Firefox, and on a
/// machine with hardly any fonts installed. It reports the geometry it used
/// as well as the results: an early round of measurements was done at a cell
/// size the product never uses, and nothing in the output said so.
#[wasm_bindgen]
pub fn fallback_check(
    font_names: Vec<String>,
    fonts: Vec<js_sys::Uint8Array>,
    size_pt: f64,
    glyph_font: String,
) -> Result<String, JsValue> {
    fn run(
        font_names: Vec<String>,
        fonts: Vec<js_sys::Uint8Array>,
        size_pt: f64,
        glyph_font: String,
    ) -> Result<String> {
        let mut faces = Vec::new();
        for (name, data) in font_names.iter().zip(fonts.iter()) {
            faces.push(Face::new(name, data.to_vec(), 0).with_context(|| name.clone())?);
        }
        let set = FontSet::new(faces)?;
        let dpr = web_sys::window().map(|w| w.device_pixel_ratio()).unwrap_or(1.0);
        let dpi = (96.0 * dpr) as u32;
        let metrics = crate::glyphs::RenderMetrics::with_font_metrics(&set.metrics(size_pt, dpi)?);
        let stack = families(&glyph_font);
        let px = size_pt * dpi as f64 / 72.0;
        let scratch =
            crate::canvas::Scratch::new(metrics.cell_size, metrics.descender.get(), px, &stack)?;
        let (canvas_w, canvas_h, pen_x, baseline) = scratch.geometry();

        let mut rows = Vec::new();
        let (mut drawn, mut wrong_colour, mut clipped, mut blank) = (0, 0, 0, 0);
        for (name, text, cells, expect_color) in SAMPLES {
            let points: Vec<String> =
                text.chars().map(|c| format!("\"U+{:04X}\"", c as u32)).collect();
            // Whether the terminal would actually use the canvas here --
            // asked of the real thing, not of half of it. Production routes
            // a grapheme to the canvas only when the bundled faces cannot
            // draw it AND `keeps_notdef` lets it through; consulting only
            // the second half marked `A` and the check mark U+2713 as
            // routed, so on a machine whose default monospace lacks U+2713
            // the probe failed over a character JetBrains Mono draws and the
            // canvas is never asked for. It also meant the U+2713/U+2714
            // pair -- there precisely because they look alike and come from
            // different fonts -- was measured from the canvas on both sides
            // and could never show the mismatch it exists to show.
            let routed = !set
                .shape_web(
                    text,
                    size_pt,
                    dpi,
                    None,
                    thinkterm_font_core::Direction::LeftToRight,
                    None,
                    None,
                    &crate::fallback::keeps_notdef,
                )?
                .gaps
                .is_empty();
            let row = match scratch.measure(text, *cells)? {
                None => {
                    if routed {
                        blank += 1;
                    }
                    format!(
                        "{{\"name\":\"{name}\",\"cp\":[{}],\"routed\":{routed},\
                         \"ink\":null,\"ok\":{}}}",
                        points.join(","),
                        !routed
                    )
                }
                Some((ink, placement, has_color)) => {
                    drawn += 1;
                    let mut ok = true;
                    if routed {
                        if has_color != *expect_color {
                            wrong_colour += 1;
                            ok = false;
                        }
                        if ink.clipped {
                            clipped += 1;
                            ok = false;
                        }
                    }
                    format!(
                        "{{\"name\":\"{name}\",\"cp\":[{}],\"routed\":{routed},\
                         \"ink\":{{\"left\":{},\"top\":{},\"width\":{},\"height\":{}}},\
                         \"clipped\":{},\"has_color\":{},\"expect_color\":{},\
                         \"bearing_x\":{:.2},\"bearing_y\":{:.2},\"scale\":{:.3},\"ok\":{ok}}}",
                        points.join(","),
                        ink.left,
                        ink.top,
                        ink.width,
                        ink.height,
                        ink.clipped,
                        has_color,
                        expect_color,
                        placement.bearing_x,
                        placement.bearing_y,
                        placement.scale,
                    )
                }
            };
            rows.push(row);
        }
        // The counts are the assertions. The first round of measurements had
        // a silent bug in exactly this place -- both branches produced
        // plausible numbers, and only printing "how many were judged
        // coloured" exposed it -- so they are printed whether or not anyone
        // is looking at them.
        let ok = blank == 0 && wrong_colour == 0 && clipped == 0;
        Ok(format!(
            "{{\"ok\":{ok},\"drawn\":{drawn},\"blank\":{blank},\"wrong_colour\":{wrong_colour},\
             \"clipped\":{clipped},\"samples\":{},\
             \"cell\":[{},{}],\"descender\":{:.2},\"px\":{:.2},\"dpr\":{dpr},\
             \"canvas\":[{canvas_w},{canvas_h}],\"pen_x\":{pen_x:.2},\"baseline\":{baseline:.2},\
             \"families\":\"{}\",\"results\":[{}]}}",
            SAMPLES.len(),
            metrics.cell_size.width,
            metrics.cell_size.height,
            metrics.descender.get(),
            px,
            // The stack comes from the query string, so it is data: a
            // stray quote or backslash would produce JSON the harness
            // cannot parse, and the harness is the point of this.
            stack.replace(['"', '\\'], "'"),
            rows.join(",")
        ))
    }
    run(font_names, fonts, size_pt, glyph_font).map_err(js_err)
}
