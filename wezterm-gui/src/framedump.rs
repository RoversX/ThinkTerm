//! Dump rendered frames to PNG on request, so the picture the user sees can
//! be inspected without screen-recording permissions.
//!
//! Enable by launching with `THINKTERM_FRAME_DUMP=<dir>`. Nothing is captured
//! until a file named `trigger` appears in that directory:
//!   - an empty (or any) `trigger` file: the next frame is written and the
//!     trigger is consumed;
//!   - a `trigger` containing the word `all`: every frame is written, at most
//!     four per second, until the file is removed.
//!
//! Frames land as `<dir>/frame-<pid>-<seq>.png`. Capture happens on the paint
//! path with a blocking map, costing a few milliseconds -- this is a debugging
//! tool, not something to leave triggered while measuring.

use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

static DUMP_DIR: OnceLock<Option<PathBuf>> = OnceLock::new();

thread_local! {
    static LAST_DUMP: RefCell<Vec<(usize, Instant)>> = const { RefCell::new(Vec::new()) };
    static SEQ: RefCell<u64> = const { RefCell::new(0) };
}

const CONTINUOUS_MIN_INTERVAL: Duration = Duration::from_millis(250);

fn dump_dir() -> Option<&'static PathBuf> {
    DUMP_DIR
        .get_or_init(|| {
            let dir = std::env::var_os("THINKTERM_FRAME_DUMP").map(PathBuf::from);
            match &dir {
                Some(dir) => log::info!("framedump: enabled, dir={}", dir.display()),
                None => log::info!("framedump: THINKTERM_FRAME_DUMP not set"),
            }
            dir
        })
        .as_ref()
}

pub fn enabled() -> bool {
    dump_dir().is_some()
}

/// Whether this frame should be captured; consumes a one-shot trigger.
/// The continuous mode throttles per window, so a quiet window's frames are
/// not starved out by a busy one.
pub fn should_dump(window_label: usize) -> bool {
    let Some(dir) = dump_dir() else {
        return false;
    };
    let trigger = dir.join("trigger");
    let Ok(contents) = std::fs::read_to_string(&trigger) else {
        return false;
    };
    let contents = contents.trim();
    log::info!("framedump: trigger seen ({contents}) for win={window_label}");
    if contents == "burst" {
        // Every painted frame, no throttle: the only way to catch a
        // single-frame flicker. PNG encoding runs on the paint thread, so
        // this deliberately slows the app while active.
        true
    } else if contents == "all" {
        LAST_DUMP.with(|last| {
            let mut last = last.borrow_mut();
            let now = Instant::now();
            match last.iter_mut().find(|(label, _)| *label == window_label) {
                Some((_, prev)) if now.duration_since(*prev) < CONTINUOUS_MIN_INTERVAL => false,
                Some((_, prev)) => {
                    *prev = now;
                    true
                }
                None => {
                    last.push((window_label, now));
                    true
                }
            }
        })
    } else {
        let _ = std::fs::remove_file(&trigger);
        true
    }
}

/// Copy `texture` (the frame about to be presented) into a PNG on disk.
/// Blocks on the GPU for the copy; returns the path it wrote.
pub fn dump_texture(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    texture: &wgpu::Texture,
    window_label: usize,
) -> anyhow::Result<PathBuf> {
    let dir = dump_dir().ok_or_else(|| anyhow::anyhow!("frame dump not enabled"))?;
    std::fs::create_dir_all(dir)?;

    let width = texture.width();
    let height = texture.height();
    let format = texture.format();
    let bytes_per_pixel = 4u32;
    let unpadded = width * bytes_per_pixel;
    let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let padded = unpadded.div_ceil(align) * align;

    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("frame dump readback"),
        size: (padded * height) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });

    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("frame dump encoder"),
    });
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &buffer,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded),
                rows_per_image: Some(height),
            },
        },
        wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
    );
    queue.submit(std::iter::once(encoder.finish()));

    let slice = buffer.slice(..);
    let (tx, rx) = std::sync::mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |result| {
        let _ = tx.send(result);
    });
    device.poll(wgpu::PollType::Wait)?;
    rx.recv()??;

    let data = slice.get_mapped_range();
    let mut pixels = Vec::with_capacity((unpadded * height) as usize);
    for row in 0..height {
        let start = (row * padded) as usize;
        pixels.extend_from_slice(&data[start..start + unpadded as usize]);
    }
    drop(data);
    buffer.unmap();

    // The surface is BGRA on Metal; PNG wants RGBA.
    let is_bgra = matches!(
        format,
        wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Bgra8UnormSrgb
    );
    if is_bgra {
        for px in pixels.chunks_exact_mut(4) {
            px.swap(0, 2);
        }
    }
    // The compositor owns real transparency; an opaque PNG is easier to read.
    for px in pixels.chunks_exact_mut(4) {
        px[3] = 0xff;
    }

    let seq = SEQ.with(|seq| {
        let mut seq = seq.borrow_mut();
        *seq += 1;
        *seq
    });
    let path = dir.join(format!(
        "frame-{}-win{window_label}-{seq:04}.png",
        std::process::id()
    ));
    let img = image::RgbaImage::from_raw(width, height, pixels)
        .ok_or_else(|| anyhow::anyhow!("pixel buffer size mismatch"))?;
    img.save(&path)?;
    log::info!("framedump: wrote {}", path.display());
    Ok(path)
}
