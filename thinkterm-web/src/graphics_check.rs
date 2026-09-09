//! Token-free WebGPU regression probe using the real line emitter/cache.
use crate::{emit, fallback::FallbackBudget, glyphs::GlyphCache, gpu::Gpu, ime};
use anyhow::{anyhow, ensure, Result};
use std::{rc::Rc, sync::Arc};
use termwiz::surface::{CursorVisibility, Line};
use thinkterm_font_web::{Face, FontSet};
use thinkterm_proto::StableCursorPosition;
use thinkterm_render::{pipeline::GpuTexture, quad::HeapQuadAllocator};
use wasm_bindgen::{prelude::*, JsCast};
use wezterm_term::{color::ColorPalette, CellAttributes};

#[wasm_bindgen]
pub async fn graphics_check(
    font_names: Vec<String>,
    fonts: Vec<js_sys::Uint8Array>,
    size_pt: f64,
) -> Result<String, JsValue> {
    run(font_names, fonts, size_pt)
        .await
        .map_err(|e| JsValue::from_str(&format!("{e:#}")))
}

async fn run(names: Vec<String>, fonts: Vec<js_sys::Uint8Array>, size: f64) -> Result<String> {
    let window = web_sys::window().ok_or_else(|| anyhow!("no window"))?;
    let document = window.document().ok_or_else(|| anyhow!("no document"))?;
    let canvas: web_sys::HtmlCanvasElement = document
        .get_element_by_id("term")
        .unwrap()
        .dyn_into()
        .unwrap();
    let field: web_sys::HtmlTextAreaElement = document
        .get_element_by_id("kbd")
        .unwrap()
        .dyn_into()
        .unwrap();
    let clock = window.performance().unwrap();
    let rect = canvas.get_bounding_client_rect();
    let dpr = window.device_pixel_ratio();
    let (w, h) = ((rect.width() * dpr) as u32, (rect.height() * dpr) as u32);
    canvas.set_width(w);
    canvas.set_height(h);
    let mut gpu = Gpu::new(canvas.clone(), w, h).await?;
    let faces = names
        .iter()
        .zip(fonts.iter())
        .map(|(n, f)| Face::new(n, f.to_vec(), 0))
        .collect::<Result<Vec<_>>>()?;
    let texture = Rc::new(GpuTexture::new(
        &gpu.device,
        Arc::clone(&gpu.queue),
        1024,
        1024,
    )?);
    let mut cache = GlyphCache::new(
        Rc::new(FontSet::new(faces)?),
        size,
        (96.0 * dpr) as u32,
        texture,
        crate::canvas::DEFAULT_FAMILIES.into(),
    )?;
    let (cw, ch) = (
        cache.metrics.cell_size.width as u32,
        cache.metrics.cell_size.height as u32,
    );
    ensure!(
        w >= 4 * cw && h >= 2 * ch,
        "the probe needs at least four columns and two rows"
    );
    let cols = (w / cw).min(40) as usize;
    let samples = [
        "█".repeat(cols),
        "█".repeat(cols),
        "▀".repeat(cols),
        "▄".repeat(cols),
        "▌▐".repeat(cols / 2),
        "░".repeat(cols),
        "▒".repeat(cols),
        "▓".repeat(cols),
        format!("┌{}┐", "─".repeat(cols - 2)),
        format!("│{}│", " ".repeat(cols - 2)),
        format!("└{}┘", "─".repeat(cols - 2)),
        "╔══╦══╗ ╭──╮ ┣━╋━┫".into(),
        "▁▂▃▄▅▆▇█ ▏▎▍▌▋▊▉ ▖▗▘▙▚▛▜▝▞▟".into(),
        "⠁⠃⠇⡇⣇⣧⣷⣿".into(),
    ];
    let palette = ColorPalette::default();
    let cursor = StableCursorPosition {
        visibility: CursorVisibility::Hidden,
        ..Default::default()
    };
    let mut quads = HeapQuadAllocator::default();
    let mut vertices = Vec::new();
    let emit = |cache: &mut GlyphCache,
                quads: &mut HeapQuadAllocator,
                texts: &[String],
                frozen|
     -> Result<()> {
        cache.begin_frame(frozen);
        quads.recycle();
        let mut budget = FallbackBudget::new(clock.now());
        for (i, text) in texts.iter().enumerate() {
            let line = Line::from_text(text, &CellAttributes::default(), 0, None);
            emit::emit_line(
                cache,
                quads,
                &mut budget,
                &emit::LineParams {
                    line: &line,
                    stable_row: i as isize,
                    top_pixel_y: (i as u32 * ch) as f32,
                    cursor: &cursor,
                    palette: &palette,
                    selection: 0..0,
                    focused: false,
                    reverse_video: false,
                    surface: (w as f32, h as f32),
                },
            )?;
        }
        ensure!(budget.deferred() == 0, "geometry spent Canvas budget");
        Ok(())
    };
    let t = clock.now();
    emit(&mut cache, &mut quads, &samples, false)?;
    let cold_ms = clock.now() - t;
    let usage = cache.atlas.usage();
    let t = clock.now();
    for _ in 0..20 {
        emit(&mut cache, &mut quads, &samples, false)?;
    }
    let warm_ms = (clock.now() - t) / 20.0;
    ensure!(
        cache.atlas.usage() == usage,
        "warm frames allocated more atlas space"
    );
    // An uncached shape declined while frozen must work once unfrozen.
    let missed = ["╬".to_string()];
    emit(&mut cache, &mut quads, &missed, true)?;
    ensure!(
        cache.declined() > 0 && cache.atlas.usage() == usage,
        "frozen miss allocated or was not declined"
    );
    emit(&mut cache, &mut quads, &missed, false)?;
    ensure!(
        cache.declined() == 0 && cache.atlas.usage().allocations > usage.allocations,
        "frozen miss stayed blank"
    );
    emit(&mut cache, &mut quads, &samples, false)?;
    quads.extract_vertices(&mut vertices);
    let reference = gpu
        .draw_and_read_pixel(&vertices, cache.texture(), cw / 2, ch / 2)
        .await?;
    ensure!(reference[0] > 32, "full block is blank");
    for (x, y) in [
        (0, 0),
        (cw - 1, ch / 2),
        (cw, ch / 2),
        (cw + 1, ch / 2),
        (cw / 2, ch - 1),
        (cw / 2, ch),
        (cw / 2, ch + 1),
        (cw - 1, ch - 1),
        (cw, ch),
    ] {
        let pixel = gpu
            .draw_and_read_pixel(&vertices, cache.texture(), x, y)
            .await?;
        ensure!(
            pixel == reference,
            "seam at ({x},{y}): {pixel:?}, expected {reference:?}"
        );
    }
    // Check the real DOM update path, including the no-op and preservation
    // of composition text. Native IME candidate UI still needs manual QA.
    let bounds = [rect.left(), rect.top(), rect.width(), rect.height()];
    let mut previous = None;
    field.set_value("pinyin");
    for point in [
        (0.0, 0.0),
        (cw as f64 * 3.0, ch as f64 * 4.0),
        (w as f64, h as f64),
    ] {
        let anchor = ime::anchor(bounds, (w, h), point, (cw as f64, ch as f64)).unwrap();
        ime::update_field(&field, &mut previous, Some(anchor))
            .map_err(|e| anyhow!("IME: {e:?}"))?;
        let actual = field.get_bounding_client_rect();
        ensure!(
            (actual.left() - anchor.left).abs() < 0.1 && (actual.top() - anchor.top).abs() < 0.1,
            "IME field differs from cursor anchor"
        );
        ensure!(
            !ime::update_field(&field, &mut previous, Some(anchor))
                .map_err(|e| anyhow!("IME: {e:?}"))?,
            "unchanged anchor writes DOM"
        );
        ensure!(
            field.value() == "pinyin",
            "position update changed composition text"
        );
    }
    field.set_value("");
    Ok(format!("{{\"ok\":true,\"seams\":0,\"warm_allocations\":0,\"frozen_recovered\":true,\"ime_anchor\":true,\"cell\":[{cw},{ch}],\"dpr\":{dpr},\"cold_emit_ms\":{cold_ms:.2},\"warm_emit_ms\":{warm_ms:.2}}}"))
}
