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
    timeout_check(&crate::web_platform::WebPlatform::new(canvas.clone(), field.clone())).await?;
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
        crate::canvas::platform(),
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
                    origin: (0.0, 0.0),
                    clip: (w as f32, h as f32),
                    hsv: None,
                    draw_cursor: true,
                    cursor_shape: None,
                    cursor_hidden: false,
                    min_contrast: 0.0,
                    composing: None,
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
    image_check(&mut gpu, &mut cache, (w, h), (cw, ch)).await?;
    image_atlas_check(&mut gpu, (w, h), (cw, ch)).await?;
    Ok(format!("{{\"ok\":true,\"seams\":0,\"warm_allocations\":0,\"frozen_recovered\":true,\"ime_anchor\":true,\"image_pixels\":true,\"image_release\":true,\"timer_cancel\":true,\"cell\":[{cw},{ch}],\"dpr\":{dpr},\"cold_emit_ms\":{cold_ms:.2},\"warm_emit_ms\":{warm_ms:.2}}}"))
}

async fn image_atlas_check(gpu: &mut Gpu, surface: (u32, u32), cell: (u32, u32)) -> Result<()> {
    use crate::graphics::Graphics;
    use termwiz::image::{ImageCell, ImageData, ImageDataType, TextureCoordinate};
    use termwiz::cell::Cell;

    let color = |i: usize| [(i * 67) as u8, (i * 19 + 41) as u8, (i * 53 + 7) as u8, 255];
    let image = |i| Arc::new(ImageData::with_data(ImageDataType::new_single_frame(1, 1, color(i).to_vec())));
    let image_cell = |data: &Arc<ImageData>| {
        let mut attrs = CellAttributes::default();
        attrs.attach_image(Box::new(ImageCell::new(
            TextureCoordinate::new_f32(0.0, 0.0),
            TextureCoordinate::new_f32(1.0, 1.0),
            Arc::clone(data),
        )));
        Cell::new(' ', attrs)
    };
    let mut images: Vec<_> = (0..100).map(image).collect();
    let mut lines: Vec<_> = (0..10).map(|_| Line::with_width(10, 0)).collect();
    for (i, data) in images.iter().enumerate() {
        lines[i / 10].set_cell(i % 10, image_cell(data), 0);
    }
    let cursor = StableCursorPosition::default();
    let palette = ColorPalette::default();
    let (w, h) = (surface.0 as f32, surface.1 as f32);
    let (cw, ch) = (cell.0 as f32, cell.1 as f32);
    ensure!(w >= cw * 10.0 && h >= ch * 10.0, "image atlas probe needs a 10x10 grid");
    let render = |graphics: &mut Graphics, gpu: &mut Gpu, lines: &[Line]| -> Result<()> {
        graphics.begin_frame(0);
        for (row, line) in lines.iter().enumerate() {
            let params = emit::LineParams {
                line, stable_row: row as _, top_pixel_y: row as f32 * ch,
                cursor: &cursor, palette: &palette, selection: 0..0,
                focused: true, reverse_video: false, surface: (w, h),
                origin: (0.0, 0.0), clip: (w, h), hsv: None,
                draw_cursor: false, cursor_shape: None, cursor_hidden: true,
                min_contrast: 0.0,
                composing: None,
            };
            graphics.collect_line(gpu, 0, &params, (cw, ch), (1.0, 1.0), [0.0, 0.0, w, h])?;
        }
        graphics.finish_frame(gpu, (w, h));
        Ok(())
    };
    let mut graphics = Graphics::default();
    render(&mut graphics, gpu, &lines)?;
    let initial = graphics.stats();
    ensure!(initial.0 < 10 && initial.1 < 1024 * 1024 && initial.2 == 100, "small images were omitted or not packed: {initial:?}");
    render(&mut graphics, gpu, &lines)?;
    ensure!(graphics.stats() == initial, "unchanged atlas images were reallocated or uploaded");
    for i in (0..100).step_by(2) {
        lines[i / 10].set_cell(i % 10, Cell::new(' ', CellAttributes::default()), 0);
    }
    render(&mut graphics, gpu, &lines)?;
    for i in (0..100).step_by(2) {
        images[i] = image(i + 100);
        lines[i / 10].set_cell(i % 10, image_cell(&images[i]), 0);
    }
    render(&mut graphics, gpu, &lines)?;
    let reused = graphics.stats();
    ensure!((reused.0, reused.1, reused.2) == (initial.0, initial.1, initial.2 + 50), "freed atlas slots were not reused: {reused:?}");
    let mut batches = Vec::new();
    graphics.append_batches(2, &mut batches);
    // Sample cell edges, where a missing gutter would blend with neighbours.
    for i in 0..100 {
        let x = (i % 10) as u32 * cell.0 + if i % 2 == 0 { 0 } else { cell.0 - 1 };
        let y = (i / 10) as u32 * cell.1;
        let actual = gpu.draw_batches_and_read_pixel(&batches, x, y).await?;
        let expected = color(if i % 2 == 0 { i + 100 } else { i });
        ensure!(actual == expected, "atlas cell {i} sampled stale or adjacent pixels: {actual:?}, expected {expected:?}");
    }
    drop(batches);
    {
        *images[67].data() = ImageDataType::new_single_frame(1, 1, vec![255; 4]);
        images[67].bump_generation();
    }
    render(&mut graphics, gpu, &lines)?;
    ensure!(graphics.stats().2 == reused.2 + 1, "one image update re-uploaded neighbouring images");
    let mut batches = Vec::new();
    graphics.append_batches(2, &mut batches);
    for i in 66..=68 {
        let actual = gpu.draw_batches_and_read_pixel(&batches, (i % 10) as u32 * cell.0, (i / 10) as u32 * cell.1).await?;
        let expected = if i == 67 { [255; 4] } else { color(i + 100) };
        ensure!(actual == expected, "atlas mutation corrupted cell {i}: {actual:?}");
    }
    drop(batches);
    graphics.begin_frame(0);
    graphics.finish_frame(gpu, (w, h));
    ensure!(graphics.stats().0 == 0 && graphics.stats().1 == 0 && graphics.staging_bytes() == 0, "empty atlas kept GPU or staging storage");
    Ok(())
}

async fn timeout_check(platform: &crate::web_platform::WebPlatform) -> Result<()> {
    use crate::platform::{Platform, Timeout};
    use std::cell::{Cell, RefCell};

    let calls = Rc::new(Cell::new(0));
    for _ in 0..256 {
        let captured = Rc::new(());
        let weak = Rc::downgrade(&captured);
        let called = Rc::clone(&calls);
        let timer = platform.cancellable_timeout(0.0, Box::new(move || {
            called.set(called.get() + 1);
            drop(captured);
        }));
        drop(timer);
        ensure!(weak.upgrade().is_none(), "cancelled timer retained its callback");
    }

    // A fired callback may drop its own handle, as the App's wake does.
    let slot = Rc::new(RefCell::new(None::<Timeout>));
    let weak = Rc::downgrade(&slot);
    let (tx, rx) = futures::channel::oneshot::channel();
    *slot.borrow_mut() = Some(platform.cancellable_timeout(0.0, Box::new(move || {
        if let Some(slot) = weak.upgrade() {
            let timer = slot.borrow_mut().take();
            drop(timer);
        }
        let _ = tx.send(());
    })));
    rx.await?;
    ensure!(slot.borrow().is_none(), "fired timer kept its handle");
    ensure!(calls.get() == 0, "cancelled timer fired");
    Ok(())
}

async fn image_check(
    gpu: &mut Gpu,
    cache: &mut GlyphCache,
    surface: (u32, u32),
    cell: (u32, u32),
) -> Result<()> {
    use crate::graphics::Graphics;
    use termwiz::image::{ImageCell, ImageData, ImageDataType, TextureCoordinate};

    let pixel = [128, 64, 32, 255];
    let data = Arc::new(ImageData::with_data(ImageDataType::new_single_frame(
        2,
        2,
        pixel.repeat(4),
    )));
    let mut attrs = CellAttributes::default();
    attrs.attach_image(Box::new(ImageCell::new(
        TextureCoordinate::new_f32(0.0, 0.0),
        TextureCoordinate::new_f32(1.0, 1.0),
        Arc::clone(&data),
    )));
    let line = Line::from_text("  ", &attrs, 0, None);
    let cursor = StableCursorPosition::default();
    let palette = ColorPalette::default();
    let (w, h) = (surface.0 as f32, surface.1 as f32);
    let (cw, ch) = (cell.0 as f32, cell.1 as f32);
    let params = emit::LineParams {
        line: &line,
        stable_row: 0,
        top_pixel_y: 0.0,
        cursor: &cursor,
        palette: &palette,
        selection: 0..0,
        focused: true,
        reverse_video: false,
        surface: (w, h),
        origin: (0.0, -ch / 4.0),
        clip: (w, h),
        hsv: None,
        draw_cursor: false,
        cursor_shape: None,
        cursor_hidden: true,
        min_contrast: 0.0,
        composing: None,
    };
    let mut graphics = Graphics::default();
    for _ in 0..3 {
        graphics.begin_frame(0);
        graphics.collect_line(gpu, 0, &params, (cw, ch), (1.0, 1.0), [0.0, 0.0, cw * 1.5, h])?;
        graphics.finish_frame(gpu, (w, h));
        ensure!(
            graphics.stats() == (1, 256, 1),
            "unchanged image uploaded again"
        );
    }
    let mut batches = Vec::new();
    graphics.append_batches(2, &mut batches);
    ensure!(batches.len() == 1, "adjacent slices were not batched");
    for (x, y, expected) in [
        (cell.0 / 2, cell.1 / 4, pixel),
        (cell.0, cell.1 / 4, pixel),
        (cell.0 * 2, cell.1 / 4, [0, 0, 0, 255]),
        (cell.0 / 2, cell.1, [0, 0, 0, 255]),
    ] {
        let actual = gpu
            .draw_and_read_pixel(batches[0].0, batches[0].1, x, y)
            .await?;
        ensure!(
            actual == expected,
            "image at ({x},{y}): {actual:?}, expected {expected:?}"
        );
    }
    drop(batches);
    {
        let mut held = data.data();
        *held = ImageDataType::new_single_frame(2, 2, [0, 255, 0, 255].repeat(4));
        data.bump_generation();
    }
    graphics.begin_frame(0);
    graphics.collect_line(gpu, 0, &params, (cw, ch), (1.0, 1.0), [0.0, 0.0, w, h])?;
    graphics.finish_frame(gpu, (w, h));
    ensure!(
        graphics.stats() == (1, 256, 2),
        "image generation did not replace pixels once"
    );
    let mut batches = Vec::new();
    graphics.append_batches(2, &mut batches);
    let actual = gpu
        .draw_and_read_pixel(batches[0].0, batches[0].1, cell.0 / 2, cell.1 / 4)
        .await?;
    ensure!(
        actual == [0, 255, 0, 255],
        "old image pixels survived replacement"
    );
    drop(batches);
    graphics.begin_frame(0);
    graphics.finish_frame(gpu, (w, h));
    ensure!(
        graphics.stats() == (0, 0, 2),
        "invisible textures were retained"
    );
    gpu.draw_batches(&[], [0.0; 4], 0)?;

    // More textures than the old atlas-only cache could hold in a frame.
    let mut line = Line::with_width(12, 0);
    for x in 0..12 {
        let mut attrs = CellAttributes::default();
        attrs.attach_image(Box::new(ImageCell::new(
            TextureCoordinate::new_f32(0.0, 0.0),
            TextureCoordinate::new_f32(1.0, 1.0),
            Arc::new(ImageData::with_data(ImageDataType::new_single_frame(
                1,
                1,
                vec![x as u8 * 20, 128, 64, 255],
            ))),
        )));
        line.set_cell(x, termwiz::cell::Cell::new(' ', attrs), 0);
    }
    let params = emit::LineParams {
        line: &line,
        origin: (0.0, 0.0),
        ..params
    };
    graphics.begin_frame(0);
    graphics.collect_line(gpu, 0, &params, (cw, ch), (1.0, 1.0), [0.0, 0.0, w, h])?;
    graphics.finish_frame(gpu, (w, h));
    let mut batches = Vec::new();
    graphics.append_batches(2, &mut batches);
    ensure!(
        !batches.is_empty() && batches.len() < 12,
        "small images did not share texture batches"
    );
    for x in 0..12 {
        let pixel = gpu
            .draw_batches_and_read_pixel(&batches, cell.0 * x + cell.0 / 2, cell.1 / 2)
            .await?;
        ensure!(
            pixel == [x as u8 * 20, 128, 64, 255],
            "batch {x} sampled another texture: {pixel:?}"
        );
    }
    drop(batches);
    let red = Arc::new(ImageData::with_data(ImageDataType::new_single_frame(
        1,
        1,
        vec![255, 0, 0, 255],
    )));
    let mut palette = ColorPalette::default();
    palette.foreground = wezterm_color_types::SrgbaTuple(1.0, 1.0, 1.0, 1.0);
    for z in [i32::MIN, -1, 0] {
        let mut attrs = CellAttributes::default();
        attrs.set_background(
            wezterm_term::color::ColorAttribute::TrueColorWithDefaultFallback(
                wezterm_color_types::SrgbaTuple(0.0, 1.0, 0.0, 1.0),
            ),
        );
        attrs.attach_image(Box::new(ImageCell::with_z_index(
            TextureCoordinate::new_f32(0.0, 0.0),
            TextureCoordinate::new_f32(1.0, 1.0),
            Arc::clone(&red),
            z,
            0,
            0,
            0,
            0,
            Some(1),
            Some(1),
        )));
        let line = Line::from_text("█ ", &attrs, 0, None);
        let params = emit::LineParams {
            line: &line,
            palette: &palette,
            selection: 0..0,
            ..params
        };
        let mut quads = HeapQuadAllocator::default();
        let mut budget =
            FallbackBudget::new(web_sys::window().unwrap().performance().unwrap().now());
        emit::emit_line(cache, &mut quads, &mut budget, &params)?;
        let mut vertices = Vec::new();
        let layers = quads.extract_layer_vertices(&mut vertices);
        graphics.begin_frame(0);
        graphics.collect_line(gpu, 0, &params, (cw, ch), (1.0, 1.0), [0.0, 0.0, w, h])?;
        graphics.finish_frame(gpu, (w, h));
        let mut batches = Vec::new();
        for layer in 0..3 {
            graphics.append_batches(layer, &mut batches);
            batches.push((&vertices[layers[layer].clone()], cache.texture()));
        }
        let text = gpu
            .draw_batches_and_read_pixel(&batches, cell.0 / 2, cell.1 / 2)
            .await?;
        let background = gpu
            .draw_batches_and_read_pixel(&batches, cell.0 + cell.0 / 2, cell.1 / 2)
            .await?;
        ensure!(
            text == if z < 0 { [255; 4] } else { [255, 0, 0, 255] },
            "image z={z} covered the wrong text layer: {text:?}"
        );
        ensure!(
            background
                == if z == i32::MIN {
                    [0, 255, 0, 255]
                } else {
                    [255, 0, 0, 255]
                },
            "image z={z} covered the wrong background layer: {background:?}"
        );
    }
    for (alpha, expected) in [
        (0, [255u8, 0, 0, 255]),
        (128, [187, 188, 0, 255]),
        (255, [0, 255, 0, 255]),
    ] {
        let green = Arc::new(ImageData::with_data(ImageDataType::new_single_frame(
            1,
            1,
            vec![0, 255, 0, alpha],
        )));
        let mut attrs = CellAttributes::default();
        // Reverse insertion order: at equal z the higher image id must
        // remain on top, with its alpha blended over the lower image.
        for (id, image) in [(8, green), (7, Arc::clone(&red))] {
            attrs.attach_image(Box::new(ImageCell::with_z_index(
                TextureCoordinate::new_f32(0.0, 0.0),
                TextureCoordinate::new_f32(1.0, 1.0),
                image,
                0,
                0,
                0,
                0,
                0,
                Some(id),
                Some(1),
            )));
        }
        let line = Line::from_text(" ", &attrs, 0, None);
        let params = emit::LineParams {
            line: &line,
            selection: 0..0,
            ..params
        };
        graphics.begin_frame(0);
        graphics.collect_line(gpu, 0, &params, (cw, ch), (1.0, 1.0), [0.0, 0.0, w, h])?;
        graphics.finish_frame(gpu, (w, h));
        let mut batches = Vec::new();
        graphics.append_batches(2, &mut batches);
        let pixel = gpu
            .draw_batches_and_read_pixel(&batches, cell.0 / 2, cell.1 / 2)
            .await?;
        ensure!(
            pixel.iter().zip(expected).all(|(a, b)| a.abs_diff(b) <= 1),
            "image alpha={alpha} or equal-z ordering is wrong: {pixel:?}"
        );
    }
    // Three upload strips, with cells sampling both sides of each boundary.
    // The final strip is short, so an incorrect destination offset or length
    // cannot pass by repeatedly uploading the first strip.
    let colors = [[255, 0, 0, 255], [0, 255, 0, 255], [0, 0, 255, 255]];
    let tall = Arc::new(ImageData::with_data(ImageDataType::new_single_frame(
        1024,
        600,
        (0..600)
            .flat_map(|y| colors[y / 256].repeat(1024))
            .collect(),
    )));
    let rows = [0, 255, 256, 511, 512, 599];
    let mut line = Line::with_width(rows.len(), 0);
    for (col, row) in rows.iter().enumerate() {
        let mut attrs = CellAttributes::default();
        attrs.attach_image(Box::new(ImageCell::new(
            TextureCoordinate::new_f32(0.0, (*row as f32 + 0.5) / 600.0),
            TextureCoordinate::new_f32(1.0, (*row as f32 + 0.5) / 600.0),
            Arc::clone(&tall),
        )));
        line.set_cell(col, termwiz::cell::Cell::new(' ', attrs), 0);
    }
    let strip_params = emit::LineParams {
        line: &line,
        ..params
    };
    graphics.begin_frame(0);
    graphics.collect_line(gpu, 0, &strip_params, (cw, ch), (1.0, 1.0), [0.0, 0.0, w, h])?;
    graphics.finish_frame(gpu, (w, h));
    ensure!(
        graphics.staging_bytes() == 0,
        "contiguous uploads retained staging"
    );
    let mut batches = Vec::new();
    graphics.append_batches(2, &mut batches);
    for (col, row) in rows.iter().enumerate() {
        let pixel = gpu
            .draw_batches_and_read_pixel(&batches, col as u32 * cell.0 + cell.0 / 2, cell.1 / 2)
            .await?;
        ensure!(
            pixel
                .iter()
                .zip(colors[row / 256])
                .all(|(a, b)| a.abs_diff(b) <= 1),
            "upload strip row {row} has wrong pixels: {pixel:?}"
        );
    }
    drop(batches);
    let frames = vec![vec![255, 0, 0, 255], vec![0, 255, 0, 255]];
    let hashes = frames.iter().map(|frame| ImageDataType::content_key(frame)).collect();
    let animation = Arc::new(ImageData::with_data(ImageDataType::AnimRgba8 {
        width: 1, height: 1, frames, hashes,
        durations: vec![std::time::Duration::ZERO, std::time::Duration::from_millis(40)],
    }));
    let mut animated = Line::with_width(2, 0);
    for col in 0..2 {
        let mut attrs = CellAttributes::default();
        attrs.attach_image(Box::new(ImageCell::with_z_index(
            TextureCoordinate::new_f32(0.0, 0.0), TextureCoordinate::new_f32(1.0, 1.0),
            Arc::clone(&animation), 0, 0, 0, 0, 0, Some(col as u32 + 7), Some(1),
        )));
        animated.set_cell(col, termwiz::cell::Cell::new(' ', attrs), 0);
    }
    let animated_params = emit::LineParams { line: &animated, selection: 0..0, ..params };
    let mut selected_texture = None;
    for frame in [1, 0, 1] {
        graphics.set_selections(0, &[wezterm_term::KittyFrameSelection { relative_placements: Vec::new(), virtual_placements: Vec::new(),
            data_generation: 0,
            image_id: 7, data_hash: animation.hash(), animation: wezterm_term::kitty_animation::KittyAnimation { frame, ..wezterm_term::kitty_animation::KittyAnimation::new([0, 40], 0) },
        }]);
        graphics.begin_frame(0);
        graphics.collect_line(gpu, 0, &animated_params, (cw, ch), (1.0, 1.0), [0.0, 0.0, w, h])?;
        graphics.finish_frame(gpu, (w, h));
        let mut batches = Vec::new();
        graphics.append_batches(2, &mut batches);
        if let Some(id) = selected_texture {
            ensure!(batches[0].1.id() == id, "frame selection recreated the GPU texture");
        }
        selected_texture = Some(batches[0].1.id());
        let pixel = gpu.draw_batches_and_read_pixel(&batches, cell.0 / 2, cell.1 / 2).await?;
        ensure!(pixel == if frame == 0 { [255, 0, 0, 255] } else { [0, 255, 0, 255] }, "selected frame has wrong pixels: {pixel:?}");
        let sibling = gpu.draw_batches_and_read_pixel(&batches, cell.0 + cell.0 / 2, cell.1 / 2).await?;
        ensure!(sibling == [255, 0, 0, 255], "selection changed another image id: {sibling:?}");
    }
    graphics.begin_frame(0);
    graphics.collect_line(gpu, 1, &animated_params, (cw, ch), (1.0, 1.0), [0.0, 0.0, w, h])?;
    graphics.finish_frame(gpu, (w, h));
    let mut batches = Vec::new();
    graphics.append_batches(2, &mut batches);
    let pixel = gpu.draw_batches_and_read_pixel(&batches, cell.0 / 2, cell.1 / 2).await?;
    ensure!(pixel == [255, 0, 0, 255], "selection changed another pane: {pixel:?}");
    drop(batches);
    let mut timeline = wezterm_term::kitty_animation::KittyAnimation::new([40, 60], 100);
    timeline.mode = wezterm_term::kitty_animation::Playback::Running;
    timeline.max_loops = 2;
    graphics.set_selections(0, &[wezterm_term::KittyFrameSelection { relative_placements: Vec::new(), virtual_placements: Vec::new(),
        data_generation: 0,
        image_id: 7, data_hash: animation.hash(), animation: timeline,
    }]);
    let mut texture = None;
    for (now, color, deadline) in [
        (100, [255, 0, 0, 255], Some(140)),
        (140, [0, 255, 0, 255], Some(200)),
        (220, [255, 0, 0, 255], Some(240)),
        (10_000, [0, 255, 0, 255], None),
    ] {
        graphics.begin_frame(now);
        graphics.collect_line(gpu, 0, &animated_params, (cw, ch), (1.0, 1.0), [0.0, 0.0, w, h])?;
        graphics.finish_frame(gpu, (w, h));
        ensure!(graphics.next_animation_at() == deadline, "wrong animation deadline");
        let mut batches = Vec::new();
        graphics.append_batches(2, &mut batches);
        if let Some(id) = texture { ensure!(id == batches[0].1.id(), "playback recreated texture"); }
        texture = Some(batches[0].1.id());
        let pixel = gpu.draw_batches_and_read_pixel(&batches, cell.0 / 2, cell.1 / 2).await?;
        ensure!(pixel == color, "automatic frame has wrong pixels: {pixel:?}");
    }
    graphics.begin_frame(150);
    graphics.collect_line(gpu, 0, &animated_params, (cw, ch), (1.0, 1.0), [w, h, w + cw, h + ch])?;
    graphics.finish_frame(gpu, (w, h));
    ensure!(graphics.next_animation_at().is_none(), "offscreen image schedules playback");
    graphics.set_selections(0, &[]);
    let limit = gpu.max_texture_dimension();
    let side = limit - 2;
    let width = limit * 2 + 17;
    let height = 1024 * 1024 / (limit * 4) + 3;
    let row: Vec<u8> = (0..width)
        .flat_map(|x| {
            if x < side {
                [255, 0, 0, 255]
            } else {
                [0, 0, 255, 255]
            }
        })
        .collect();
    let wide = Arc::new(ImageData::with_data(ImageDataType::new_single_frame(
        width,
        height,
        row.repeat(height as usize),
    )));
    let reference = Arc::new(ImageData::with_data(ImageDataType::new_single_frame(
        4,
        2,
        [
            255, 0, 0, 255, 255, 0, 0, 255, 0, 0, 255, 255, 0, 0, 255, 255,
        ]
        .repeat(2),
    )));
    let mut line = Line::from_text("  ", &CellAttributes::default(), 0, None);
    for (col, data, left, right) in [
        (
            0,
            Arc::clone(&wide),
            (side - 2) as f32 / width as f32,
            (side + 2) as f32 / width as f32,
        ),
        (1, reference, 0.0, 1.0),
    ] {
        let mut attrs = CellAttributes::default();
        attrs.attach_image(Box::new(ImageCell::new(
            TextureCoordinate::new_f32(left, 0.0),
            TextureCoordinate::new_f32(right, 1.0),
            data,
        )));
        line.set_cell(col, termwiz::cell::Cell::new(' ', attrs), 0);
    }
    let params = emit::LineParams {
        line: &line,
        selection: 0..0,
        ..params
    };
    graphics.begin_frame(0);
    graphics.collect_line(gpu, 0, &params, (cw, ch), (1.0, 1.0), [0.0, 0.0, w, h])?;
    graphics.finish_frame(gpu, (w, h));
    ensure!(
        graphics.stats().0 == 3,
        "oversized image was not split into visible tiles"
    );
    let mut batches = Vec::new();
    graphics.append_batches(2, &mut batches);
    ensure!(
        graphics.staging_bytes() <= 1024 * 1024,
        "tile staging exceeded its byte bound"
    );
    for (x, y) in (0..cell.0)
        .map(|x| (x, cell.1 / 2))
        .chain((0..cell.1).map(|y| (cell.0 - 1, y)))
    {
        let tiled = gpu.draw_batches_and_read_pixel(&batches, x, y).await?;
        let reference = gpu
            .draw_batches_and_read_pixel(&batches, x + cell.0, y)
            .await?;
        ensure!(
            tiled.iter().zip(reference).all(|(a, b)| a.abs_diff(b) <= 2),
            "tile seam at pixel ({x},{y}): tiled={tiled:?}, reference={reference:?}"
        );
    }
    drop(batches);
    let uploaded = graphics.stats().2;
    graphics.begin_frame(0);
    graphics.collect_line(gpu, 0, &params, (cw, ch), (1.0, 1.0), [0.0, 0.0, w, h])?;
    graphics.finish_frame(gpu, (w, h));
    ensure!(
        graphics.stats().2 == uploaded,
        "unchanged image tiles uploaded again"
    );
    {
        let mut payload = wide.data();
        let ImageDataType::Rgba8 { data, hash, .. } = &mut *payload else {
            unreachable!()
        };
        for row in data.chunks_exact_mut(width as usize * 4) {
            for pixel in row[side as usize * 4..].chunks_exact_mut(4) {
                pixel.copy_from_slice(&[0, 255, 0, 255]);
            }
        }
        *hash = ImageDataType::content_key(data);
        wide.bump_generation();
    }
    graphics.begin_frame(0);
    graphics.collect_line(gpu, 0, &params, (cw, ch), (1.0, 1.0), [0.0, 0.0, w, h])?;
    graphics.finish_frame(gpu, (w, h));
    ensure!(
        graphics.stats().2 == uploaded + 2,
        "a tile generation update uploaded the wrong regions"
    );
    let mut batches = Vec::new();
    graphics.append_batches(2, &mut batches);
    let pixel = gpu
        .draw_batches_and_read_pixel(&batches, cell.0 - 1, cell.1 - 1)
        .await?;
    ensure!(
        pixel == [0, 255, 0, 255],
        "tile mutation left stale pixels: {pixel:?}"
    );
    drop(batches);
    graphics.begin_frame(0);
    graphics.finish_frame(gpu, (w, h));
    ensure!(
        graphics.stats().0 == 0 && graphics.stats().1 == 0 && graphics.staging_bytes() == 0,
        "hidden tiles were retained"
    );
    Ok(())
}
