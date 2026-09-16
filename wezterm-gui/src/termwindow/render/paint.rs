use crate::quad::{
    HeapQuadAllocator, HeapQuadMark, QuadClipRect, QuadTrait, TripleLayerQuadAllocator, Vertex,
};
use crate::termwindow::content_view::{ContentViewTypography, TerminalPreviewRequest};
use crate::termwindow::render::{LineToEleShapeCacheKey, RenderScreenLineParams};
use crate::termwindow::{PaintOutcome, RenderFrame, TermWindowNotif, UIItem, UIItemType};
use crate::ui::DrawContext;
use ::window::bitmaps::atlas::OutOfTextureSpace;
use ::window::color::LinearRgba;
use ::window::RectF;
use ::window::WindowOps;
use anyhow::Context;
use mux::renderable::{RenderableDimensions, StableCursorPosition};
use mux::tab::{SplitDirection, TabId};
use smol::Timer;
use std::collections::hash_map::DefaultHasher;
use std::collections::HashSet;
use std::hash::{Hash, Hasher};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};
use wezterm_font::ClearShapeCache;
use wezterm_term::color::ColorAttribute;
use wezterm_term::TerminalSize;
use window::Dimensions;

const TERMINAL_PREVIEW_EXTENT_BUCKET_DESIGN_PX: f32 = 8.0;
/// Coarse on purpose: every distinct bucket becomes a `FontConfiguration`
/// (a 4-8ms main-thread build plus its own glyph-atlas population and a new
/// shape-cache identity). At 128 buckets/unit a session of scrolling and
/// resizing built 96 of them in two minutes; at 16 the cell-size difference
/// between adjacent buckets is sub-pixel at thumbnail sizes.
const TERMINAL_PREVIEW_SCALE_BUCKETS_PER_UNIT: f64 = 16.0;
/// How far the preview may walk down from its estimated font scale looking for
/// one that fits. Each step costs a `FontConfiguration`, and the estimate is
/// close enough that one is usually all it takes.
const MAX_PREVIEW_SCALE_STEPS: usize = 12;
/// How far a thumbnail may be stretched on one axis to undo the proportions
/// lost when a cell is rounded to whole pixels. Enough to cover that rounding
/// at the sizes cards use; far short of reshaping a terminal that is honestly
/// a different shape.
const MAX_PREVIEW_ASPECT_TRIM: f32 = 1.15;
/// Ceiling on enlarging a thumbnail to fill its card. Only ever closes the gap
/// left by whole-pixel cells, which is under one cell's worth.
const MAX_PREVIEW_FILL: f32 = 1.5;

/// Height of the pane layer divider the tab bar draws below itself, in the same
/// device pixels the divider quad uses. Kept in step with `fancy_tab_bar`.
const TAB_BAR_SEAM_HEIGHT: f32 = 1.0;

/// How many cards may rebuild their recorded quads in one frame.
///
/// Measured with five cards open: replaying a card costs ~0.3ms and rebuilding
/// one costs ~10ms, so this is the difference between a frame that fits a
/// 120Hz deadline and one that misses two. Five at once -- which is what
/// reopening the overview used to do -- froze the window for 100-170ms.
///
/// One, because two already overruns. Cards over the budget replay their last
/// picture and ask for another frame, so no card goes blank and the queue
/// drains at one per frame.
const MAX_PREVIEW_QUAD_REBUILDS_PER_FRAME: usize = 1;

/// How long one frame may spend rebuilding a card before the rest of the work
/// is carried over to the next frame.
///
/// The one-card budget above bounds *how many* cards rebuild per frame, but
/// not how big that card is: a full-window card of braille TUI output measured
/// 29ms at p99 and 71ms at worst -- two to four missed frames for one card.
/// Slicing inside the card caps the per-frame cost; the partially built card
/// keeps showing its previous texture until the new picture is complete.
const PREVIEW_REBUILD_SLICE: Duration = Duration::from_millis(3);

/// How long a card's picture takes to fade in once its terminal first shows
/// visible content.
///
/// A card whose pane has nothing cached locally -- a remote pane, a
/// background tab -- opens as an empty frame and fills in whenever its fetch
/// completes, one card at a time, with no transition: a hard blank-to-full
/// pop that reads as flicker. The fade turns that pop into an appearance.
/// It only runs on the blank->content edge, so settled cards, refreshes of a
/// card that already has content, and atlas rebuilds never re-fade.
const PREVIEW_CONTENT_FADE: Duration = Duration::from_millis(120);

/// Whether a card has ever shown visible content, and when it first did.
/// Keyed per card alongside `preview_quad_cache`, and kept across overview
/// closes for the same reason that cache is: reopening should not replay
/// the fade.
pub(crate) enum PreviewContentFade {
    /// Still blank. `snapshot` is the address of the snapshot last scanned,
    /// so each snapshot is scanned for content at most once.
    Blank {
        snapshot: usize,
    },
    ContentSince(Instant),
}

fn snapshot_has_content(
    snapshot: &crate::termwindow::content_view::TerminalPreviewSnapshot,
) -> bool {
    snapshot
        .panes
        .iter()
        .any(|pane| pane.lines.iter().any(|line| !line.is_whitespace()))
}

/// Everything a card's recorded quads depend on.
///
/// Equal keys mean the recorded heap still draws the right picture and can be
/// replayed instead of rebuilt. Note what is *not* here: where the card is.
/// Position is recovered at replay time, so scrolling the overview costs a
/// remap rather than a rebuild.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PreviewQuadKey {
    /// Address of the snapshot these quads were built from.
    ///
    /// `resolve_snapshot` hands back the very same `Arc` while a terminal's
    /// content is unchanged and a freshly allocated one when it is not, so
    /// pointer equality answers "is this the same picture?" exactly, for free.
    /// [`CachedPreviewQuads`] keeps the `Arc` alive so a dead snapshot's
    /// address cannot be reissued to a different one.
    snapshot: usize,
    /// Bumped when a font finishes loading and every shape cache is thrown
    /// away. That makes the recorded quads *out of date* -- a character the
    /// shaper had no glyph for may now have one -- but not wrong: the atlas is
    /// untouched, so every texel they address still holds the glyph they were
    /// built with. It belongs on this side of the split for that reason.
    ///
    /// It matters that it is here and not in the geometry. A font fallback
    /// landed roughly once a second against busy terminals, and while this was
    /// geometry every one of them forced all five cards to rebuild inside the
    /// frame that noticed -- a ~100ms freeze, once a second, for a difference
    /// nobody could see. The atlas repack that *does* invalidate these quads
    /// clears the cache outright, in the same breath as the rest of the
    /// captures (`discard_content_view_captures_after_atlas_recreation`), so
    /// nothing here has to stand in for it.
    shape_generation: usize,
    geometry: PreviewGeometryKey,
}

/// Everything a card's quads depend on *except* what the terminal was showing.
///
/// The split is the difference between two kinds of staleness. Quads whose
/// geometry still matches but whose snapshot has moved on draw the right
/// pixels in the right places, just a refresh behind -- safe to put on screen
/// for a frame while the rebuild waits its turn. Quads whose geometry has moved
/// on are not stale, they are *wrong*: the quads carry positions relative to
/// the centre of the window and a font scale chosen from the card's size, so
/// showing them under a different window or a different card draws the picture
/// somewhere it does not belong. Replaying across that is what made the
/// overview flicker while opening.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PreviewGeometryKey {
    area_width: u32,
    area_height: u32,
    /// The *resolved* render scale (`preview_scale_estimate`), not the
    /// drag-time hold flag it replaced: the flag flipped at every gesture
    /// start and end, and each flip re-keyed all cards at once -- a full
    /// same-frame rebuild of the overview to draw the identical picture.
    scale_bits: u64,
    pixel_width: usize,
    pixel_height: usize,
    dpi: usize,
}

/// One card's thumbnail, kept between frames.
pub(crate) struct CachedPreviewQuads {
    key: PreviewQuadKey,
    /// Never read. Held so that the address recorded in `key.snapshot` belongs
    /// to a live allocation for as long as this entry does, which is what makes
    /// comparing addresses a sound test for "same snapshot".
    #[allow(dead_code)]
    snapshot: Arc<crate::termwindow::content_view::TerminalPreviewSnapshot>,
    /// The card rectangle these quads were laid out in.
    area: RectF,
    heap: HeapQuadAllocator,
    /// The card's picture as a texture (WebGpu only). Rendered from `heap`
    /// when content changes; composited as a single quad on every other
    /// frame, which is what makes an unchanged card nearly free.
    texture: Option<Rc<crate::termwindow::webgpu::CardRenderTexture>>,
}

/// What the card preview caches hold, for accounting.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct PreviewCacheFootprint {
    pub cards: usize,
    /// Resident quad bytes, the rebuild in flight included: it owns the
    /// heap it took from its cache entry, so the map alone under-reads
    /// exactly while one is in flight.
    pub quad_bytes: usize,
    /// GPU bytes of the card textures.
    pub texture_bytes: usize,
}

impl crate::TermWindow {
    pub(crate) fn preview_cache_footprint(&self) -> PreviewCacheFootprint {
        let cards = self.preview_quad_cache.borrow();
        let partial = self.preview_rebuild_partial.borrow();
        PreviewCacheFootprint {
            cards: cards.len(),
            quad_bytes: cards
                .values()
                .map(|entry| entry.heap.resident_bytes())
                .chain(partial.as_ref().map(|partial| partial.heap.resident_bytes()))
                .sum(),
            texture_bytes: cards
                .values()
                .filter_map(|entry| entry.texture.as_ref())
                .map(|texture| texture.width as usize * texture.height as usize * 4)
                .sum(),
        }
    }
}

/// A card rebuild in flight, sliced across frames.
///
/// A single slot rather than a map: the per-frame rebuild budget is one card,
/// so at most one card is ever mid-rebuild. A different card claiming the
/// budget simply drops the slot and the interrupted card starts over when its
/// turn comes back.
pub(crate) struct PreviewRebuildPartial {
    tab_id: TabId,
    key: PreviewQuadKey,
    /// Keeps `key.snapshot`'s address alive, same as the cache entry.
    #[allow(dead_code)]
    snapshot: Arc<crate::termwindow::content_view::TerminalPreviewSnapshot>,
    heap: HeapQuadAllocator,
    /// Resume point: the next pane and the next line within it.
    pane_idx: usize,
    line_idx: usize,
}

/// A card whose heap must be rendered into its texture this frame, queued by
/// the paint pass and encoded by `draw_webgpu_layers` before the main pass.
pub(crate) struct PendingCardRender {
    pub texture: Rc<crate::termwindow::webgpu::CardRenderTexture>,
    /// Where this card's flattened quads live inside the window's shared
    /// `card_frame_verts` buffer. A range instead of an owned Vec: extracting
    /// a large card is a multi-megabyte allocation, and doing that per rebuild
    /// (plus a per-frame combined copy in draw) was the biggest source of
    /// malloc large-block churn.
    pub first_vertex: usize,
    pub quad_count: usize,
    /// The rect (top-left window pixels) the verts were recorded against.
    pub area: RectF,
}

/// One textured quad standing in for a card's thousands of glyph quads,
/// drawn between the base fills and the glyph sub-layers of the main pass.
#[derive(Clone)]
pub(crate) struct CardComposite {
    pub texture: Rc<crate::termwindow::webgpu::CardRenderTexture>,
    /// Where the card is being drawn this frame (top-left window pixels).
    pub dest: RectF,
    /// Visible region; the quad is shrunk to this and its UVs follow.
    pub clip: RectF,
    pub opacity: f32,
    /// Which render layer to composite after: drawn between that layer's
    /// base-fill sub-buffer and its glyph sub-buffers, so card pictures sit
    /// above their card's background and below every label. Settled frames
    /// use layer 0; a closing ghost's fading pictures use the fade layer.
    pub zindex: i8,
}

/// Where a dedicated-texture image composite is drawn relative to the
/// terminal layer's sub-buffers, mirroring the sub-buffer the atlas path
/// would have used: under the glyphs for z<0 pictures, over them for z>=0.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ImageCompositeSlot {
    AfterFills,
    AfterGlyphs,
}

/// A run of per-cell quads that all sample one dedicated image texture,
/// drawn with a single bind + draw in the composite pass.
pub(crate) struct ImageComposite {
    pub texture: Rc<crate::termwindow::webgpu::ImageTexture>,
    pub slot: ImageCompositeSlot,
    /// Offset into `ImageCompositeBatch::verts`.
    pub first_vertex: usize,
    pub quad_count: usize,
}

/// The dedicated-texture image quads of one paint pass. Persistent and
/// cleared per pass like `card_frame_verts`, so a stream does not allocate
/// per frame; consecutive quads on the same texture and slot coalesce into
/// one composite.
#[derive(Default)]
pub(crate) struct ImageCompositeBatch {
    pub verts: Vec<Vertex>,
    pub groups: Vec<ImageComposite>,
}

impl ImageCompositeBatch {
    pub fn push_quad(
        &mut self,
        texture: &Rc<crate::termwindow::webgpu::ImageTexture>,
        slot: ImageCompositeSlot,
        quad: [Vertex; 4],
    ) {
        let first_vertex = self.verts.len();
        self.verts.extend_from_slice(&quad);
        match self.groups.last_mut() {
            Some(group)
                if group.slot == slot
                    && Rc::ptr_eq(&group.texture, texture)
                    && group.first_vertex + group.quad_count * 4 == first_vertex =>
            {
                group.quad_count += 1;
            }
            _ => self.groups.push(ImageComposite {
                texture: Rc::clone(texture),
                slot,
                first_vertex,
                quad_count: 1,
            }),
        }
    }

    pub fn clear(&mut self) {
        self.verts.clear();
        self.groups.clear();
    }

    pub fn max_quad_count(&self) -> usize {
        self.groups.iter().map(|g| g.quad_count).max().unwrap_or(0)
    }
}

fn preview_quad_key(
    preview: &TerminalPreviewRequest,
    dimensions: &Dimensions,
    shape_generation: usize,
    scale: f64,
) -> PreviewQuadKey {
    PreviewQuadKey {
        snapshot: Arc::as_ptr(&preview.snapshot) as usize,
        shape_generation,
        geometry: PreviewGeometryKey {
            // The font scale is chosen from the card's size, so a resized card
            // is a different picture even from the same snapshot. Position is
            // deliberately absent: a moved card is replayed, not rebuilt.
            area_width: preview.area.size.width.to_bits(),
            area_height: preview.area.size.height.to_bits(),
            scale_bits: scale.to_bits(),
            // Quad positions are relative to the centre of the window.
            pixel_width: dimensions.pixel_width,
            pixel_height: dimensions.pixel_height,
            dpi: dimensions.dpi,
        },
    }
}

fn quad_clip_rect(rect: RectF, dimensions: &Dimensions) -> QuadClipRect {
    QuadClipRect::from_top_left_pixels(
        rect.min_x(),
        rect.min_y(),
        rect.max_x(),
        rect.max_y(),
        dimensions,
    )
}

fn quantized_terminal_preview_extent(extent: f32, dpi: usize) -> f32 {
    let bucket = crate::ui::scale_ui_f32(TERMINAL_PREVIEW_EXTENT_BUCKET_DESIGN_PX, dpi).max(1.0);
    if extent <= bucket {
        extent.max(1.0)
    } else {
        (extent / bucket).floor() * bucket
    }
}

/// How much to stretch a laid-out grid, per axis, so it fills its card.
///
/// The font scale a thumbnail settles on lands on whole pixels -- a cell is
/// 6px or 7px and nothing between -- so the grid it builds routinely stops
/// short of the card, by up to a whole cell across the full width. Worse,
/// rounding does not preserve a cell's proportions: a real 19x41 cell becomes
/// 6x14, which is 8% narrow for its height, and a grid of narrow cells is the
/// wrong *shape* for its card however uniformly it is scaled. It fills the
/// height and leaves a bare strip down the side.
///
/// So: enlarge uniformly as far as both axes allow, then let the short axis
/// catch up by a bounded amount. That second step undoes the rounding rather
/// than inventing a distortion. The bound is what keeps it honest -- a card
/// can be showing a terminal from another window whose grid is a genuinely
/// different shape, and that difference is not ours to erase.
fn preview_fill_factors(
    grid_width: f32,
    grid_height: f32,
    area_width: f32,
    area_height: f32,
) -> (f32, f32) {
    if !(grid_width > 0.0 && grid_height > 0.0 && area_width > 0.0 && area_height > 0.0) {
        return (1.0, 1.0);
    }
    let want_x = area_width / grid_width;
    let want_y = area_height / grid_height;
    let smaller = want_x.min(want_y);
    let uniform = smaller.clamp(1.0, MAX_PREVIEW_FILL);
    // Measured against what both axes wanted, not against the capped uniform:
    // dividing by the cap makes the ratio enormous whenever the card is much
    // larger than the grid, and then both axes take the full trim -- turning a
    // bounded uniform fill into an unbounded one, for a grid that was already
    // the right shape.
    let trim = |want: f32| (want / smaller).clamp(1.0, MAX_PREVIEW_ASPECT_TRIM);
    (uniform * trim(want_x), uniform * trim(want_y))
}

fn minimum_terminal_preview_scale(font_size: f64, dpi: usize, global_scale: f64) -> f64 {
    let global_scale = global_scale.max(1.0 / TERMINAL_PREVIEW_SCALE_BUCKETS_PER_UNIT);
    if !font_size.is_finite() || font_size <= 0.0 || dpi == 0 {
        return (1.0 / TERMINAL_PREVIEW_SCALE_BUCKETS_PER_UNIT).min(global_scale);
    }
    (72.0 / (font_size * dpi as f64))
        .max(1.0 / TERMINAL_PREVIEW_SCALE_BUCKETS_PER_UNIT)
        .min(global_scale)
}

fn quantize_terminal_preview_scale_down(scale: f64, minimum: f64) -> f64 {
    if !scale.is_finite() {
        return minimum;
    }
    ((scale * TERMINAL_PREVIEW_SCALE_BUCKETS_PER_UNIT).floor()
        / TERMINAL_PREVIEW_SCALE_BUCKETS_PER_UNIT)
        .max(minimum)
}

/// Recover a pane's font-size ratio from the terminal geometry captured in
/// the snapshot. `Tab::get_size` is expressed using the root grid's cell
/// metrics, while each pane's pixel dimensions use that pane's own metrics.
/// Comparing their effective cell sizes preserves pane-local font scaling
/// without reaching back into another GUI window's mutable pane state.
fn terminal_preview_pane_scale_ratio(
    tab_size: TerminalSize,
    pane_dims: RenderableDimensions,
) -> f64 {
    fn ratio(
        pane_pixels: usize,
        pane_cells: usize,
        root_pixels: usize,
        root_cells: usize,
    ) -> Option<f64> {
        if pane_pixels == 0 || pane_cells == 0 || root_pixels == 0 || root_cells == 0 {
            return None;
        }
        let pane_cell = pane_pixels as f64 / pane_cells as f64;
        let root_cell = root_pixels as f64 / root_cells as f64;
        let ratio = pane_cell / root_cell;
        ratio.is_finite().then_some(ratio)
    }

    let width_ratio = ratio(
        pane_dims.pixel_width,
        pane_dims.cols,
        tab_size.pixel_width,
        tab_size.cols,
    );
    let height_ratio = ratio(
        pane_dims.pixel_height,
        pane_dims.viewport_rows,
        tab_size.pixel_height,
        tab_size.rows,
    );

    // Start from the larger axis. The raster-metric correction in the paint
    // path then scales down to the largest font that fits both axes, avoiding
    // a permanently under-filled pane due to integer font metrics.
    match (width_ratio, height_ratio) {
        (Some(width), Some(height)) => width.max(height),
        (Some(width), None) => width,
        (None, Some(height)) => height,
        (None, None) => 1.0,
    }
    .clamp(0.25, 4.0)
}

#[cfg(test)]
mod terminal_preview_tests {
    use super::{
        minimum_terminal_preview_scale, quantize_terminal_preview_scale_down,
        quantized_terminal_preview_extent, terminal_preview_pane_scale_ratio,
    };
    use mux::renderable::RenderableDimensions;
    use wezterm_term::TerminalSize;

    #[test]
    fn preview_extent_uses_four_logical_pixel_buckets() {
        let design_dpi = if cfg!(target_os = "macos") { 144 } else { 192 };
        let one_x_dpi = if cfg!(target_os = "macos") { 72 } else { 96 };
        assert_eq!(quantized_terminal_preview_extent(103.0, design_dpi), 96.0);
        assert_eq!(quantized_terminal_preview_extent(104.0, design_dpi), 104.0);
        assert_eq!(quantized_terminal_preview_extent(103.0, one_x_dpi), 100.0);
    }

    #[test]
    fn preview_scale_can_shrink_below_the_old_twelve_percent_floor() {
        let minimum = minimum_terminal_preview_scale(14.0, 144, 1.0);
        assert!(minimum < 0.12);
        // 0.08 rounds down to the next 1/16 bucket, still below the old floor.
        assert_eq!(quantize_terminal_preview_scale_down(0.08, minimum), 0.0625);
        assert_eq!(
            quantize_terminal_preview_scale_down(0.001, minimum),
            minimum
        );
    }

    #[test]
    fn preview_recovers_a_pane_local_font_scale_from_its_cell_geometry() {
        let tab_size = TerminalSize {
            rows: 40,
            cols: 100,
            pixel_width: 1_000,
            pixel_height: 800,
            dpi: 144,
        };
        let pane_dims = RenderableDimensions {
            cols: 40,
            viewport_rows: 12,
            pixel_width: 600,
            pixel_height: 360,
            ..RenderableDimensions::default()
        };

        assert_eq!(terminal_preview_pane_scale_ratio(tab_size, pane_dims), 1.5);
    }

    #[test]
    fn preview_uses_the_default_scale_when_cell_geometry_is_unavailable() {
        assert_eq!(
            terminal_preview_pane_scale_ratio(
                TerminalSize::default(),
                RenderableDimensions::default()
            ),
            1.0
        );
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AllowImage {
    Yes,
    Scale(usize),
    No,
}

impl AllowImage {
    /// The next coarser level after an atlas overflow, or None when there
    /// is nothing left to give up.
    fn coarser(self) -> Option<AllowImage> {
        match self {
            AllowImage::Yes => Some(AllowImage::Scale(2)),
            AllowImage::Scale(2) => Some(AllowImage::Scale(4)),
            AllowImage::Scale(4) => Some(AllowImage::Scale(8)),
            AllowImage::Scale(_) => Some(AllowImage::No),
            AllowImage::No => None,
        }
    }
}

/// What the overflow handler does about an atlas that could not fit the
/// frame. Pure so the freeze case below can be pinned by a test.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AtlasAction {
    /// Rebuild at this larger size.
    Grow(usize),
    /// Rebuild at the current size to evict sprites no frame needs any more.
    ClearInPlace,
    /// The frame wants more than the cap allows. Handled as an allocation
    /// failure: clear in place and downscale images. Never rounded down to
    /// the cap -- at the cap that rebuild would succeed, skip the downscale
    /// step, and overflow identically on the next pass, forever.
    CapExceeded,
}

/// `pass` is the ordinal of this atlas overflow within the frame (0 for
/// the first), not the paint loop's pass counter, which also advances on
/// quad-growth and shape-cache retries.
///
/// First overflow: grow while there is headroom under `grow_ceiling` rather
/// than clearing in place. Clearing answers "the atlas is full of glyphs we
/// no longer need"; it is the wrong answer to "the working set does not
/// fit", because the frame that overflowed fits once the atlas is empty, so
/// the growth branch is never reached and the next frame that wants the
/// same glyphs overflows again -- toggling the overview used to clear the
/// atlas every single time. At the ceiling, clearing is all that is left,
/// and it is also what reclaims the one-off glyphs a closed overview leaves
/// behind.
///
/// Later overflows: the working set did not fit even after a clear, so grow
/// to what the frame asked for -- up to `cap`, past which the request is a
/// failure (see [`AtlasAction::CapExceeded`]).
pub(crate) fn atlas_overflow_action(
    pass: usize,
    current: usize,
    wanted: usize,
    grow_ceiling: usize,
    cap: usize,
) -> AtlasAction {
    if pass == 0 {
        let grown = wanted.min(grow_ceiling);
        if grown > current {
            AtlasAction::Grow(grown)
        } else {
            AtlasAction::ClearInPlace
        }
    } else if wanted > cap {
        AtlasAction::CapExceeded
    } else if wanted > current {
        AtlasAction::Grow(wanted)
    } else {
        AtlasAction::ClearInPlace
    }
}

/// The image level a frame starts at, and the hold to carry forward: a
/// downscale forced by an earlier overflow holds for `hold_for`, after
/// which the next frame probes full size again.
pub(crate) fn sticky_allow_images(
    hold: Option<(AllowImage, Instant)>,
    now: Instant,
    hold_for: Duration,
) -> (AllowImage, Option<(AllowImage, Instant)>) {
    match hold {
        Some((level, since)) if now.saturating_duration_since(since) < hold_for => (level, hold),
        _ => (AllowImage::Yes, None),
    }
}

impl crate::TermWindow {
    fn paint_frontend_handoff_overlay(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
    ) -> anyhow::Result<bool> {
        let gate = self.frontend_terminal_gate();
        let Some((title, hint)) = gate.overlay_message() else {
            return Ok(false);
        };
        let now = Instant::now();
        let animation_ms = self.created.elapsed().as_millis() as u64;
        let eyes_closed = animation_ms % 5_000 >= 4_750;
        // Reuse the renderer's existing animation wakeup. Once the overlay is
        // gone no next frame is requested, so an ordinary terminal remains
        // fully draw-on-demand.
        self.update_next_frame_time(Some(now + Duration::from_millis(125)));
        let area = self.content_view_area();
        let palette = self.chrome();
        self.filled_rectangle(layers, 0, area, palette.window_bg)
            .context("frontend handoff opaque background")?;

        let font_size =
            crate::native_settings::home_font_size(&crate::native_settings::load_shared());
        let title_font = self.fonts.title_font_with_size(font_size + 2.0)?;
        let hint_font = self.fonts.title_font_with_size(font_size)?;
        let metrics = crate::utilsprites::RenderMetrics::with_font_metrics(&title_font.metrics());
        let gl_state = self.render_state.as_ref().unwrap();
        let ctx = DrawContext::new(gl_state, self.dimensions, &metrics);
        let title_width = ctx.measure_text_width(&title_font, &title);
        let hint_width = ctx.measure_text_width(&hint_font, &hint);
        let line_height = metrics.cell_size.height as f32;
        let eyes = if eyes_closed { "─  ─" } else { "•  •" };
        let eyes_width = ctx.measure_text_width(&title_font, eyes);
        let eyes_height = line_height;
        let show_eyes = area.size.width >= 160.0 && area.size.height >= 100.0;
        let text_height = line_height * 2.4;
        let group_height = if show_eyes {
            eyes_height + line_height * 1.1 + text_height
        } else {
            text_height
        };
        let x_title = area.origin.x + ((area.size.width - title_width).max(0.0) / 2.0);
        let x_hint = area.origin.x + ((area.size.width - hint_width).max(0.0) / 2.0);
        let group_y = area.origin.y + ((area.size.height - group_height).max(0.0) / 2.0);

        // One measured text run keeps the two eyes centered as a unit across
        // fonts and display scales, without introducing a mascot asset.
        if show_eyes {
            let x_eyes = area.origin.x + ((area.size.width - eyes_width).max(0.0) / 2.0);
            ctx.draw_text_on_layer(
                layers,
                2,
                &title_font,
                x_eyes,
                group_y,
                eyes,
                palette.text,
                area.size.width,
            )?;
        }

        let y_title = if show_eyes {
            group_y + eyes_height + line_height * 1.1
        } else {
            group_y
        };
        ctx.draw_text_on_layer(
            layers,
            2,
            &title_font,
            x_title,
            y_title,
            &title,
            palette.text,
            area.size.width,
        )?;
        ctx.draw_text_on_layer(
            layers,
            2,
            &hint_font,
            x_hint,
            y_title + line_height * 1.4,
            &hint,
            palette.secondary_text,
            area.size.width,
        )?;
        Ok(true)
    }

    /// A terminal another device holds is drawn as that device sees it; the
    /// badge in the corner says so, and that a click or a scroll takes it.
    /// Replaces the opaque card: the picture stays, only the input is gated.
    fn paint_frontend_takeover_badge(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
    ) -> anyhow::Result<()> {
        let gate = self.frontend_terminal_gate();
        if !gate.is_claimable() {
            return Ok(());
        }
        let Some((title, hint)) = gate.overlay_message() else {
            return Ok(());
        };
        let text = format!("{title} \u{2014} {hint}");
        let area = self.content_view_area();
        let palette = self.chrome();
        let font_size =
            crate::native_settings::home_font_size(&crate::native_settings::load_shared());
        let font = self.fonts.title_font_with_size(font_size)?;
        let metrics = crate::utilsprites::RenderMetrics::with_font_metrics(&font.metrics());
        let gl_state = self.render_state.as_ref().unwrap();
        let ctx = DrawContext::new(gl_state, self.dimensions, &metrics);
        let line_height = metrics.cell_size.height as f32;
        let pad_x = line_height * 0.6;
        let pad_y = line_height * 0.25;
        let max_text = (area.size.width - pad_x * 4.0).max(0.0);
        let text_width = ctx.measure_text_width(&font, &text).min(max_text);
        if text_width <= 0.0 || area.size.height < line_height * 3.0 {
            return Ok(());
        }
        let width = text_width + pad_x * 2.0;
        let height = line_height + pad_y * 2.0;
        let x = area.max_x() - width - pad_x;
        let y = area.max_y() - height - pad_y;
        self.filled_rectangle(
            layers,
            0,
            euclid::rect(x, y, width, height),
            palette.window_bg.mul_alpha(0.92),
        )
        .context("takeover badge background")?;
        ctx.draw_text_on_layer(
            layers,
            2,
            &font,
            x + pad_x,
            y + pad_y,
            &text,
            palette.text,
            text_width,
        )?;
        Ok(())
    }

    /// A follower keeps the owner's canonical PTY grid. If its window is
    /// larger, mark the renderer-only remainder with a faint cell grid rather
    /// than stretching or reflowing terminal data that belongs to the owner.
    fn paint_frontend_shared_unused_grid(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
    ) -> anyhow::Result<()> {
        let Some(state) = self.active_remote_frontend_viewport_state() else {
            return Ok(());
        };
        if self.owns_frontend_viewport() {
            return Ok(());
        }
        let area = self.content_view_area();
        let cell_w = self.render_metrics.cell_size.width.max(1) as f32;
        let cell_h = self.render_metrics.cell_size.height.max(1) as f32;
        let used_w = (state.canonical_size.cols as f32 * cell_w).min(area.size.width);
        let used_h = (state.canonical_size.rows as f32 * cell_h).min(area.size.height);
        if used_w >= area.size.width && used_h >= area.size.height {
            return Ok(());
        }
        let palette = self.chrome();
        let line = palette.muted_text.mul_alpha(0.10);
        let right_x = area.origin.x + used_w;
        let bottom_y = area.origin.y + used_h;

        let mut x = right_x;
        while x <= area.max_x() {
            self.filled_rectangle(
                layers,
                0,
                euclid::rect(x, area.origin.y, 1.0, area.size.height),
                line,
            )?;
            x += cell_w;
        }
        let mut y = area.origin.y;
        while y <= area.max_y() {
            if right_x < area.max_x() {
                self.filled_rectangle(
                    layers,
                    0,
                    euclid::rect(right_x, y, area.max_x() - right_x, 1.0),
                    line,
                )?;
            }
            y += cell_h;
        }
        y = bottom_y;
        while y <= area.max_y() {
            self.filled_rectangle(layers, 0, euclid::rect(area.origin.x, y, used_w, 1.0), line)?;
            y += cell_h;
        }
        x = area.origin.x;
        while x <= right_x {
            if bottom_y < area.max_y() {
                self.filled_rectangle(
                    layers,
                    0,
                    euclid::rect(x, bottom_y, 1.0, area.max_y() - bottom_y),
                    line,
                )?;
            }
            x += cell_w;
        }
        Ok(())
    }

    fn damp_scroll_value(current: f32, target: f32) -> (f32, bool) {
        let delta = target - current;
        if delta.abs() <= 0.75 {
            (target, false)
        } else {
            (current + delta * 0.38, true)
        }
    }

    fn advance_tab_scroll_animation(&mut self, now: Instant) {
        let mut animating = false;
        let (next_window_scroll, window_animating) =
            Self::damp_scroll_value(self.tab_bar_scroll_offset, self.tab_bar_scroll_target);
        if (next_window_scroll - self.tab_bar_scroll_offset).abs() > f32::EPSILON {
            self.tab_bar_scroll_offset = next_window_scroll;
            self.invalidate_fancy_tab_bar();
        }
        animating |= window_animating;

        for (pane_id, target) in self.pane_nav_tab_scroll_targets.clone() {
            let current = self
                .pane_nav_tab_scroll_offsets
                .get(&pane_id)
                .copied()
                .unwrap_or(0.0);
            let (next, pane_animating) = Self::damp_scroll_value(current, target);
            if (next - current).abs() > f32::EPSILON {
                self.pane_nav_tab_scroll_offsets.insert(pane_id, next);
            }
            animating |= pane_animating;
        }

        if animating {
            self.update_next_frame_time(Some(now + Duration::from_millis(16)));
        }
    }

    /// The render memory series for one paint: how many passes it took, and
    /// what every CPU-side cache weighed when it finished.
    ///
    /// Called on every exit from `paint_impl`'s pass loop, not just the
    /// successful one. A frame that gave up at the atlas retry cap is
    /// precisely the frame a memory investigation wants to see, and emitting
    /// this only where the frame completed left a hole in the series across
    /// the blow-up. `frame_complete` distinguishes a capped frame from a
    /// healthy one, so absent-from-the-log and gave-up stay separable.
    ///
    /// Safe to read the caches here: `paint_pass()` has returned by the time
    /// this runs on any path, so nothing holds a borrow across it.
    fn log_render_memory_counters(&self, passes: usize, frame_complete: bool) {
        if !crate::perf::enabled() {
            return;
        }
        // Recorded even for the ordinary single-pass frame, so the counter
        // can be read as a distribution rather than as "something went
        // wrong once".
        crate::perf::log_counter("paint_passes", passes);
        if !frame_complete {
            // The pass loop gave up (retry cap or error). Marked rather
            // than left absent so a post-processor can tell the two apart.
            crate::perf::log_counter("paint_capped", 1);
        }
        // Entry counts alone cannot say whether the render caches' byte
        // budgets are set sensibly: these caches hold entries of wildly
        // different sizes. The weights are already computed on every
        // insert, and without these counters the only way to read them is
        // the settings window, which a scripted measurement cannot open.
        crate::perf::log_counter("shape_cache_bytes", self.shape_cache.borrow().total_weight());
        crate::perf::log_counter(
            "line_shape_cache_bytes",
            self.line_to_ele_shape_cache.borrow().total_weight(),
        );
        crate::perf::log_counter(
            "line_quad_cache_bytes",
            self.line_quad_cache.borrow().total_weight(),
        );
        crate::perf::log_counter(
            "line_state_cache_bytes",
            self.line_state_cache.borrow().total_weight(),
        );
        // The UI shape caches, the glyph map and the atlas are what the four
        // counters above cannot see, and together they are the larger half
        // of the render memory: note alone is budgeted 32 MiB, file_preview
        // 16, and the glyph map has no bound at all. Every value here is a
        // field read or a len(), so unlike the GPU allocator report below
        // this needs no throttle.
        {
            use crate::shapecache::UiTextDomain;
            let ui = self.ui_shape_caches.borrow();
            for domain in [
                UiTextDomain::Chrome,
                UiTextDomain::Note,
                UiTextDomain::FilePreview,
            ] {
                // The same name the diagnostics panel publishes this under
                // (`publish_ui_shape_cache_diagnostics`), taken from the
                // same table: the two surfaces have separate enable flags,
                // so both are needed, but one value must not have two names.
                crate::perf::log_counter(
                    domain.gauge_names().bytes,
                    ui.domain(domain).total_weight(),
                );
            }
        }
        // Skipped rather than logged as 0 if a line is somehow still being
        // recorded: a 0 would read as "empty", which is the healthy value.
        if let Ok(scratch) = self.line_quad_scratch.try_borrow() {
            crate::perf::log_counter("line_quad_scratch_bytes", scratch.resident_bytes());
        }
        // The overview's thumbnails: one recorded heap per live card, plus
        // the one rebuild that may be sliced across frames. Unlike the line
        // cache these are stored with their doubling slack and are dropped
        // only when the atlas is repacked, so they are the largest CPU-side
        // quad population nothing else here can see. Capacity only, like
        // every other figure in this block.
        {
            let footprint = self.preview_cache_footprint();
            crate::perf::log_counter("preview_quad_cache_cards", footprint.cards);
            crate::perf::log_counter("preview_quad_cache_bytes", footprint.quad_bytes);
            crate::perf::log_counter("preview_texture_bytes", footprint.texture_bytes);
        }
        crate::perf::log_counter(
            "content_view_last_frame_bytes",
            self.content_view_last_frame
                .as_ref()
                .map_or(0, |heap| heap.resident_bytes()),
        );
        if let Some(render_state) = self.render_state.as_ref() {
            let glyphs = render_state.glyph_cache.borrow();
            crate::perf::log_counter("glyph_cache_entries", glyphs.glyph_entries());
            // Texel counts, not bytes: the atlas is side x side x 4 bytes,
            // and allocated_px is a high-water mark of packed area that only
            // Atlas::clear resets.
            crate::perf::log_counter("atlas_side_texels", glyphs.atlas.size());
            crate::perf::log_counter(
                "atlas_allocated_texels_high_water",
                glyphs.atlas.usage().allocated_px,
            );
        }
        // The GPU allocator is the other half of the picture and the cache
        // counters cannot see it. Throttled because generating the report
        // walks every live allocation.
        self.log_gpu_allocator_throttled();
    }

    pub(crate) fn paint_impl(&mut self, frame: &mut RenderFrame) -> anyhow::Result<PaintOutcome> {
        self.num_frames += 1;
        // If nothing on screen needs animating, then we can avoid
        // invalidating as frequently
        *self.has_animation.borrow_mut() = None;
        // Start at full-size images unless a recent overflow forced a
        // downscale; that level holds for ATLAS_SCALE_HOLD so the frames
        // after it do not each re-probe full size and overflow again.
        let had_hold = self.atlas_scale_hold.is_some();
        let (allow_images, hold) = sticky_allow_images(
            self.atlas_scale_hold,
            Instant::now(),
            crate::termwindow::ATLAS_SCALE_HOLD,
        );
        self.allow_images = allow_images;
        self.atlas_scale_hold = hold;
        if had_hold && hold.is_none() {
            // The hold lapsed: drop the downscaled sprites so this frame
            // really does re-probe full size. The frame cache is keyed by
            // hash alone and would otherwise keep serving the small copies.
            if let Some(render_state) = self.render_state.as_ref() {
                let evicted = render_state.evict_scaled_image_frames();
                if evicted > 0 {
                    log::trace!("atlas downscale hold lapsed; evicted {evicted} scaled frames");
                }
            }
        }

        let start = Instant::now();
        self.advance_tab_scroll_animation(start);

        {
            let diff = start.duration_since(self.last_fps_check_time);
            if diff > Duration::from_secs(1) {
                let seconds = diff.as_secs_f32();
                self.fps = self.num_frames as f32 / seconds;
                self.num_frames = 0;
                self.last_fps_check_time = start;
            }
        }

        // How many times one frame may be thrown away and started again because
        // a font finished loading underneath it.
        //
        // `ClearShapeCache` means "a font you asked for has only just arrived;
        // the shapes you cached are stale". The handler answers by clearing
        // every shape cache and repainting the whole window -- and each repaint
        // can pull in the next not-yet-loaded font and ask for the same thing
        // again. Opening the live overview introduces a font size per card at
        // once, so the frame that opens it was chaining eight and nine of these
        // rounds, re-shaping five thumbnails from scratch in every one: 680ms
        // of frozen window, measured.
        //
        // One retry is enough to present a correct frame in the common case.
        // Past that, stop chasing the fonts inside this frame and ask for
        // another one: the caches have been cleared either way, so the next
        // frame starts warm and the cost lands as one more frame rather than as
        // half a second of nothing.
        const MAX_SHAPE_CACHE_RETRIES: usize = 1;
        let mut shape_retries = 0usize;
        let mut atlas_retries = 0usize;
        // Cleared when the retry cap gives up on this frame: the quad buffers
        // then hold however much of the window the aborted pass got through,
        // and presenting that paints the terminal without its chrome for the
        // couple of frames until the requested repaint lands -- a visible
        // black flash on every font-size change. Skipping the present keeps
        // the previous complete frame on screen instead, which nobody can
        // see. Only WebGpu can skip: the glium frame was created by do_paint
        // and will be swapped regardless, so an unpainted frame there would
        // present undefined content, which is worse than a partial one.
        let mut present_frame = true;
        let mut frame_complete = false;
        // Held outside the loop so the memory counters can be emitted on
        // every way out of it, the retry cap and the error arms included.
        let mut passes_used = 0usize;

        'pass: for pass in 0.. {
            passes_used = pass + 1;
            self.frame_pane_output_generations.clear();
            self.track_pane_output_generations_for_frame = false;
            match self.paint_pass() {
                Ok(_) => match self.render_state.as_mut().unwrap().allocated_more_quads() {
                    Ok(allocated) => {
                        if !allocated {
                            frame_complete = true;
                            break 'pass;
                        }
                        // Each retry repaints the *whole* window, thumbnails
                        // included. Opening the overview was costing five of
                        // them inside one frame -- 580ms -- so which resource
                        // ran out, and how often, decides where the fix goes.
                        crate::perf::log_counter("paint_retry_quads", pass);
                        self.invalidate_fancy_tab_bar();
                        self.invalidate_modal();
                    }
                    Err(err) => {
                        log::error!("{:#}", err);
                        break 'pass;
                    }
                },
                Err(err) => {
                    if let Some(&OutOfTextureSpace {
                        size: Some(size),
                        current_size,
                    }) = err.root_cause().downcast_ref::<OutOfTextureSpace>()
                    {
                        crate::perf::log_counter(
                            "paint_retry_atlas",
                            format!("pass={pass} have={current_size} want={size}"),
                        );
                        atlas_retries += 1;
                        if atlas_retries > crate::termwindow::MAX_ATLAS_RETRIES {
                            // Keep the previous complete frame rather than
                            // present a half-built one (only WebGpu can skip
                            // the present; glium swaps regardless). Unlike
                            // the shape-retry cap below this does NOT
                            // re-invalidate: clearing shape caches changes
                            // the next frame, but a working set that will
                            // not fit at the cap even with images downscaled
                            // paints identically, and re-arming would turn
                            // every vsync into ten atlas rebuilds.
                            crate::perf::log_counter("paint_atlas_retry_capped", atlas_retries);
                            log::error!(
                                "texture atlas: {atlas_retries} rebuilds in one frame \
                                 (have={current_size} want={size}); keeping the previous frame"
                            );
                            self.note_atlas_overflow(pass, current_size, size, "gave-up");
                            present_frame = false;
                            break 'pass;
                        }
                        // The outer `pass` also counts quad-growth and
                        // shape-cache retries, so the first *atlas* overflow
                        // of a frame is not necessarily pass 0. The policy
                        // wants the atlas overflow ordinal.
                        let action = atlas_overflow_action(
                            atlas_retries - 1,
                            current_size,
                            size,
                            crate::termwindow::MAX_GROWN_ATLAS_SIZE,
                            crate::termwindow::MAX_ATLAS_SIZE,
                        );
                        let (result, action_name) = match action {
                            AtlasAction::Grow(grown) => {
                                // Growth up to the first-pass ceiling is
                                // routine (startup climbs 128 -> 512 this
                                // way); only growth past it is the event
                                // worth a default-level log line.
                                if grown > crate::termwindow::MAX_GROWN_ATLAS_SIZE {
                                    log::warn!(
                                        "texture atlas grow {current_size}->{grown} pass={pass} scene={}",
                                        self.atlas_scene()
                                    );
                                } else {
                                    log::trace!("grow texture atlas {current_size} -> {grown}");
                                }
                                (self.recreate_texture_atlas(Some(grown)), "grow")
                            }
                            AtlasAction::ClearInPlace => {
                                log::trace!("recreate_texture_atlas at {current_size}");
                                (self.recreate_texture_atlas(Some(current_size)), "clear")
                            }
                            AtlasAction::CapExceeded => {
                                log::warn!(
                                    "texture atlas wants {size} texels per side, past the {} cap \
                                     (have {current_size}); clearing and downscaling images, pass={pass} scene={}",
                                    crate::termwindow::MAX_ATLAS_SIZE,
                                    self.atlas_scene()
                                );
                                // Clear first: the frame cache would otherwise
                                // keep handing out the full-size sprites by
                                // hash and the downscaled retry would change
                                // nothing. Then take the failure path below.
                                let cleared = self.recreate_texture_atlas(Some(current_size));
                                (
                                    cleared.and_then(|()| {
                                        Err(anyhow::anyhow!(
                                            "texture atlas cap {} exceeded (wanted {size})",
                                            crate::termwindow::MAX_ATLAS_SIZE
                                        ))
                                    }),
                                    "cap",
                                )
                            }
                        };
                        self.note_atlas_overflow(pass, current_size, size, action_name);
                        self.invalidate_fancy_tab_bar();
                        self.invalidate_modal();
                        // Captured sidebars hold atlas UV coordinates, not
                        // pixels, so cached frames cannot survive repacking.
                        // Preserve only a fast committed flick that is still
                        // waiting for its first source capture; the retry can
                        // repaint that source and complete the pending switch.
                        self.recover_workspace_space_swipe_after_atlas_recreation();
                        self.discard_content_view_captures_after_atlas_recreation();

                        if let Err(err) = result {
                            if !matches!(action, AtlasAction::CapExceeded) {
                                // The rebuild failed at the GPU (limit or
                                // memory) and left the old atlas in place,
                                // full-size sprites included. Clear it so the
                                // downscaled retry does not just hit them by
                                // hash and overflow the same way.
                                if let Err(clear_err) =
                                    self.recreate_texture_atlas(Some(current_size))
                                {
                                    log::error!(
                                        "texture atlas clear after failed resize also failed: {clear_err:#}"
                                    );
                                }
                            }
                            let Some(coarser) = self.allow_images.coarser() else {
                                log::error!(
                                    "Failed to {} texture: {}",
                                    if pass == 0 { "clear" } else { "resize" },
                                    err
                                );
                                // Nothing left to give up. The atlas was just
                                // cleared (or the rebuild failed), so the
                                // quads built this pass point at a zeroed or
                                // stale texture: keep the previous frame.
                                present_frame = false;
                                break 'pass;
                            };
                            self.allow_images = coarser;
                            self.atlas_scale_hold = Some((coarser, Instant::now()));

                            log::info!(
                                "Not enough texture space ({:#}); \
                                     will retry render with {:?}",
                                err,
                                self.allow_images,
                            );
                        }
                    } else if err.root_cause().downcast_ref::<ClearShapeCache>().is_some() {
                        // The shaper asked for the frame to be redone because a
                        // font it needed had only just finished loading. Each
                        // one of these throws away every shape cache and starts
                        // the whole window again, thumbnails included.
                        crate::perf::log_counter("paint_retry_shape", pass);
                        self.invalidate_fancy_tab_bar();
                        self.invalidate_modal();
                        self.shape_generation += 1;
                        self.shape_cache.borrow_mut().clear();
                        self.ui_shape_caches.borrow_mut().clear_all();
                        self.publish_ui_shape_cache_diagnostics();
                        self.line_to_ele_shape_cache.borrow_mut().clear();
                        shape_retries += 1;
                        if shape_retries > MAX_SHAPE_CACHE_RETRIES {
                            crate::perf::log_counter("paint_shape_retry_capped", shape_retries);
                            if let Some(window) = self.window.as_ref() {
                                window.invalidate();
                            }
                            present_frame = false;
                            break 'pass;
                        }
                    } else {
                        log::error!("paint_pass failed: {:#}", err);
                        // The frame is half-built: quads reference state from
                        // the aborted pass, and submitting them is what used
                        // to trip wgpu validation (index buffer overrun) and
                        // abort the process. Skip presenting, exactly like
                        // the shape-retry cap above.
                        present_frame = false;
                        break 'pass;
                    }
                }
            }
        }
        // After the loop rather than inside its successful arm: every exit
        // above reaches here, so a frame that gave up still reports what the
        // caches weighed when it did.
        self.log_render_memory_counters(passes_used, frame_complete);
        log::debug!("paint_impl before call_draw elapsed={:?}", start.elapsed());

        // One paint, presented or not: retire dedicated textures nobody
        // drew. Ticking only after a successful draw left the cache's
        // budget switched off for exactly the frames that fail or are
        // skipped, while every pass kept uploading a fresh texture.
        if let Some(render_state) = self.render_state.as_ref() {
            render_state.dedicated_images.borrow_mut().end_frame();
        }
        let draw_result = if present_frame || !matches!(frame, RenderFrame::WebGpu) {
            self.call_draw(frame).map(|_| true)
        } else {
            Ok(false)
        };
        self.publish_ui_shape_cache_diagnostics_throttled();
        self.last_frame_duration = start.elapsed();
        log::debug!(
            "paint_impl elapsed={:?}, fps={}",
            self.last_frame_duration,
            self.fps
        );
        metrics::histogram!("gui.paint.impl").record(self.last_frame_duration);
        metrics::histogram!("gui.paint.impl.rate").record(1.);

        // If self.has_animation is some, then the last render detected
        // image attachments with multiple frames, so we also need to
        // invalidate the viewport when the next frame is due
        //
        // The focus gate exists so an unfocused terminal does not burn frames
        // animating gifs nobody is looking at. But a content-view transition
        // or an open overview owes frames regardless of focus: toggling the
        // overview can switch macOS Spaces, and during that switch the window
        // is briefly unfocused -- dropping the schedule right there strands
        // the animation, which then only advances when some terminal happens
        // to emit output. Measured: 150-210ms between transition frames, with
        // the main thread idle the whole time.
        // (A dwell or grace deadline registered while the window is
        // unfocused would otherwise be dropped, stranding a half-slid
        // panel until some terminal happens to emit output.) The set of
        // states lives in owes_frames_regardless_of_focus so the
        // unfocused repaint throttle exempts exactly the same ones.
        if self.focused.is_some() || self.owes_frames_regardless_of_focus() {
            if let Some(next_due) = *self.has_animation.borrow() {
                // "The next frame the display will give me" is the common
                // request: every frame of a scroll, of a card travelling, of a
                // transition, and of a preview that is owed a capture asks for
                // it.
                //
                // Answering it through the timer answers it the slowest way
                // available -- spawn a task, await a `Timer` that is already
                // due, notify the window across a channel, spend a turn of the
                // run loop applying that notification, invalidate, and only
                // then wait for the display refresh that was the entire point.
                // Those hops cost more than the frame does: measured with the
                // overview open, 14.7ms of painting delivered a frame every
                // 33.6ms, the difference spent idle in the run loop waiting for
                // the app's own message to come back.
                //
                // So ask the window directly and let the backend pace it; it
                // already throttles to min(max_fps, the display's rate).
                if next_due <= Instant::now() {
                    if let Some(window) = self.window.as_ref() {
                        window.invalidate();
                    }
                    return draw_result.map(|draw_submitted| PaintOutcome {
                        draw_submitted,
                        frame_complete,
                    });
                }
                let prior = self.scheduled_animation.borrow_mut().take();
                match prior {
                    // A timer for an earlier-or-equal deadline is already in
                    // flight -- but only trust it while the deadline is
                    // current. Display sleep can swallow the spawned task
                    // whole, and a swallowed timer whose deadline is trusted
                    // forever strands the animation. Put the deadline back so
                    // the next frame doesn't spawn a duplicate timer for it.
                    Some(prior)
                        if prior <= next_due
                            && Instant::now().saturating_duration_since(prior)
                                < Duration::from_millis(250) =>
                    {
                        self.scheduled_animation.borrow_mut().replace(prior);
                    }
                    _ => {
                        self.scheduled_animation.borrow_mut().replace(next_due);
                        let window = self.window.clone().take().unwrap();
                        promise::spawn::spawn(async move {
                            Timer::at(next_due).await;
                            let win = window.clone();
                            window.notify(TermWindowNotif::Apply(Box::new(move |tw| {
                                tw.scheduled_animation.borrow_mut().take();
                                win.invalidate();
                            })));
                        })
                        .detach();
                    }
                }
            }
        }
        draw_result.map(|draw_submitted| PaintOutcome {
            draw_submitted,
            frame_complete,
        })
    }

    /// Paint the active content view into the content area (right of the
    /// sidebar, below the tab bar).
    /// Advance a full-window view's arrival or departure.
    fn advance_content_view_fade(&mut self, now: Instant) {
        let Some(fade) = self.content_view_fade.as_mut() else {
            return;
        };
        // Once the travelling terminal has closed to within touching distance
        // of its card, hand the card back its own thumbnail and dissolve the
        // recording into it. Both pictures are then on screen at the same
        // rectangle, which is the only arrangement in which a dissolve reads as
        // one thing settling rather than as two things overlapping.
        let mut landed = false;
        if fade.landing.is_none() && fade.travel.target() >= 0.5 {
            if let Some(flight) = fade.flight.as_ref() {
                if crate::termwindow::content_view::flight_is_landing(
                    flight.source,
                    flight.destination,
                    fade.travel.value(now),
                ) {
                    fade.landing = Some(crate::ui::anim::Timeline::new(
                        now,
                        1.0,
                        0.0,
                        crate::termwindow::CONTENT_VIEW_LANDING_FADE,
                        crate::ui::anim::Easing::Smooth,
                    ));
                    landed = true;
                }
            }
        }
        let travelling = fade.travel.advance(now)
            | fade.chrome_travel.advance(now)
            | fade.landing.as_mut().is_some_and(|fade| fade.advance(now));
        if landed {
            // Painted after this runs, so the thumbnail appears underneath the
            // dissolve on this very frame rather than one frame late.
            if let Some(view) = self.active_content_view_mut() {
                view.set_terminal_in_flight(None);
            }
        }
        let Some(fade) = self.content_view_fade.as_mut() else {
            return;
        };
        if fade.opacity.advance(now) || travelling {
            // Unnamed interval, as with the Space swipe: the backend paces
            // repaints to this display's refresh rate.
            if let Some(window) = self.window.as_ref() {
                window.invalidate();
            }
        } else {
            self.content_view_fade = None;
            // The departing picture has finished leaving; nothing else refers
            // to it, and an arriving one is now simply the foreground.
            self.content_view_last_frame = None;
            self.content_view_last_composites.borrow_mut().clear();
            if let Some(view) = self.active_content_view_mut() {
                view.set_terminal_in_flight(None);
            }
            self.invalidate_window();
        }
    }

    fn content_view_fade_opacity(&self, now: Instant) -> Option<f32> {
        self.content_view_fade
            .as_ref()
            .map(|fade| fade.opacity.value(now))
    }

    fn window_rect(&self) -> RectF {
        euclid::rect(
            0.0,
            0.0,
            self.dimensions.pixel_width as f32,
            self.dimensions.pixel_height as f32,
        )
    }

    fn clip_of(&self, rect: RectF) -> QuadClipRect {
        QuadClipRect::from_top_left_pixels(
            rect.min_x(),
            rect.min_y(),
            rect.max_x(),
            rect.max_y(),
            &self.dimensions,
        )
    }

    /// Replay the recorded terminal at the size its travel has reached.
    ///
    /// It rides above the view: the terminal is shrinking *into* the card, so
    /// it has to be seen crossing the grid that is arriving underneath it. The
    /// last stretch is spent fading, because what it lands on is the card's
    /// own thumbnail of the same terminal drawn from the same snapshot -- near
    /// enough to blend into, not near enough to cut to.
    fn paint_content_view_flight(&self) -> anyhow::Result<()> {
        let now = Instant::now();
        let Some(fade) = self.content_view_fade.as_ref() else {
            return Ok(());
        };
        let Some(flight) = fade.flight.as_ref() else {
            return Ok(());
        };
        // The terminal grid is what travels, and the card's own thumbnail is
        // what it lands on, so both ends of the journey are the same picture.
        //
        // The source is the one recorded with the surface, not the terminal's
        // rectangle now: these quads hold the positions they were authored at.
        let source = flight.source;
        // The destination, on the other hand, is re-asked every frame. An
        // arriving overview keeps laying itself out while the terminal crosses
        // the window -- closing a card reflows the grid underneath it -- and a
        // rectangle sampled once meant landing on where the card used to be
        // and then jumping to where it is.
        let destination = flight
            .tab_id
            .and_then(|tab_id| {
                self.active_content_view()
                    .and_then(|view| view.terminal_landing_rect(tab_id))
            })
            .unwrap_or(flight.destination);
        let travel = fade.travel.value(now);
        let target = crate::termwindow::content_view::flight_rect_at(source, destination, travel);
        // Opaque for all of the journey but the landing.
        //
        // An earlier version faded over the last tenth of the *distance*, and
        // ease-out spends its time unevenly: that tenth is nearly half of the
        // duration, so the fade ran translucent for seven frames at a size
        // 3-18% off the card it was landing on. Worse, the card drew no
        // thumbnail while its terminal was in flight, so there was nothing on
        // the far side to dissolve into -- only the flat panel colour. Two
        // misaligned pictures with a panel showing between them is exactly
        // what "it looks like two layers" meant.
        //
        // Both of those are now addressed rather than avoided: the window is
        // bounded by size instead of by distance, and the card is handed its
        // thumbnail back as the window opens. See `flight_is_landing`.
        let opacity = fade.landing.as_ref().map_or(1.0, |fade| fade.value(now));

        let gl_state = self.render_state.as_ref().unwrap();
        let layer = gl_state
            .layer_for_zindex(crate::termwindow::CONTENT_VIEW_FLIGHT_ZINDEX)
            .context("content view flight layer")?;
        let mut layers = layer.quad_allocator();
        flight.surface.apply_to_scaled(
            &mut layers,
            self.clip_of(source),
            self.clip_of(target),
            self.clip_of(target),
            opacity,
        )
    }

    /// Record the window frame in three pieces, one per edge it can leave by.
    fn record_content_view_chrome(&mut self) -> anyhow::Result<()> {
        let mut chrome = crate::termwindow::content_view::ContentViewChrome::default();
        {
            let mut left = TripleLayerQuadAllocator::Heap(&mut chrome.left);
            self.paint_workspace_sidebar(&mut left)
                .context("record workspace sidebar")?;
        }
        {
            let mut right = TripleLayerQuadAllocator::Heap(&mut chrome.right);
            self.paint_right_sidebar(&mut right)
                .context("record right sidebar")?;
        }
        if self.show_tab_bar {
            let mut top = TripleLayerQuadAllocator::Heap(&mut chrome.top);
            self.paint_tab_bar(&mut top).context("record tab bar")?;
        }
        if let Some(fade) = self.content_view_fade.as_mut() {
            fade.chrome = Some(chrome);
        }
        Ok(())
    }

    /// Slide each piece of the frame off the edge it belongs to.
    ///
    /// Anchored motion rather than a fade: a panel that lives against the left
    /// edge reads as leaving when it goes left, and as merely disappearing
    /// when it dissolves in place.
    fn paint_content_view_chrome(&self) -> anyhow::Result<()> {
        let now = Instant::now();
        let Some(fade) = self.content_view_fade.as_ref() else {
            return Ok(());
        };
        let Some(chrome) = fade.chrome.as_ref() else {
            return Ok(());
        };
        let gone = fade.chrome_travel.value(now).clamp(0.0, 1.0);
        let window = self.window_rect();
        let terminal = self.terminal_content_rect();
        let left_width = terminal.min_x() - window.min_x();
        let right_width = window.max_x() - terminal.max_x();
        // The tab bar paints one row past its own band. The pane layer divider
        // in `fancy_tab_bar` sits at the seam -- `row_y + row_height`, which is
        // the terminal's first row, not the tab bar's last -- so sliding by the
        // terminal's top inset alone parks exactly that row against the top of
        // the window and leaves it there. Windowed, the rounded corner mask
        // hides most of it; fullscreen has no corners and it reads as a
        // hairline that never leaves.
        let top_height = terminal.min_y() - window.min_y() + TAB_BAR_SEAM_HEIGHT;

        let gl_state = self.render_state.as_ref().unwrap();
        let layer = gl_state
            .layer_for_zindex(crate::termwindow::CONTENT_VIEW_FLIGHT_ZINDEX)
            .context("content view chrome layer")?;
        let mut layers = layer.quad_allocator();
        let full = self.clip_of(window);
        for (surface, dx, dy) in [
            (&chrome.left, -left_width * gone, 0.0),
            (&chrome.right, right_width * gone, 0.0),
            (&chrome.top, 0.0, -top_height * gone),
        ] {
            let shifted = window.translate(euclid::vec2(dx, dy));
            surface.apply_to_scaled(&mut layers, full, self.clip_of(shifted), full, 1.0)?;
        }
        Ok(())
    }

    /// Work out where the terminal is heading and hand it the recorded frame.
    ///
    /// Called after the view has painted, because the destination comes from
    /// the view's layout and the layout is produced by painting. This lands on
    /// the transition's first frame, which the timelines have deliberately not
    /// started counting yet.
    fn resolve_content_view_flight(&mut self, surface: HeapQuadAllocator) {
        let tab_id = mux::Mux::get()
            .get_active_tab_for_window(self.mux_window_id)
            .map(|tab| tab.tab_id());
        // A closing transition recorded its destination before the view was
        // torn down; an opening one asks the view that has just laid itself
        // out.
        let recorded = self
            .content_view_fade
            .as_ref()
            .and_then(|fade| fade.pending_destination);
        let destination = recorded
            .or_else(|| {
                tab_id.and_then(|tab_id| {
                    self.active_content_view()
                        .and_then(|view| view.terminal_landing_rect(tab_id))
                })
            })
            .unwrap_or_else(|| self.window_rect());
        // The view must not draw its own copy of a terminal that is currently
        // crossing the window towards it.
        if destination != self.window_rect() {
            if let Some(view) = self.active_content_view_mut() {
                view.set_terminal_in_flight(tab_id);
            }
        }
        let source = self.terminal_content_rect();
        if let Some(fade) = self.content_view_fade.as_mut() {
            fade.flight = Some(crate::termwindow::content_view::ContentViewFlight {
                surface,
                source,
                destination,
                tab_id,
            });
        }
    }

    fn surface_clip(&self) -> QuadClipRect {
        QuadClipRect::from_top_left_pixels(
            0.0,
            0.0,
            self.dimensions.pixel_width as f32,
            self.dimensions.pixel_height as f32,
            &self.dimensions,
        )
    }

    /// Composite a recorded surface above everything the terminal drew.
    ///
    /// The three quad layers are a global z-order, not per-surface depth:
    /// every layer-0 quad in the window is drawn, then every layer-1 quad,
    /// then every layer-2 quad. Appending a second surface into the same
    /// layers therefore interleaves the two -- terminal text, which lives in
    /// layer 1, lands on top of a view's card backgrounds in layer 0. A
    /// separate z-index is a separate set of passes, so the whole surface
    /// arrives above the whole terminal.
    fn composite_above_terminal(
        &self,
        surface: &HeapQuadAllocator,
        opacity: f32,
    ) -> anyhow::Result<()> {
        let gl_state = self.render_state.as_ref().unwrap();
        let layer = gl_state
            .layer_for_zindex(crate::termwindow::CONTENT_VIEW_FADE_ZINDEX)
            .context("content view transition layer")?;
        let mut layers = layer.quad_allocator();
        surface.apply_to_clipped(&mut layers, self.surface_clip(), opacity)
    }

    /// Paint the foreground view, recording the frame so that closing it later
    /// has a picture to take away, and compositing it at the transition's
    /// opacity while one is running.
    fn paint_content_view_composited(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
    ) -> anyhow::Result<()> {
        if !self.content_view_is_full_window() {
            // Only full-window views transition, and only they are worth the
            // extra copy through a heap.
            return self.paint_content_view(layers);
        }

        // One surface, recorded in one pass: the view, its thumbnails and the
        // window chrome it owns. Fading them separately -- or holding some of
        // them back -- is what makes an arrival look like several things
        // happening near each other rather than one thing happening.
        let composites_before = self.card_composites.borrow().len();
        let mut heap = HeapQuadAllocator::default();
        {
            let mut recorded = TripleLayerQuadAllocator::Heap(&mut heap);
            self.paint_content_view(&mut recorded)?;
            let mut chrome_items = self
                .paint_full_window_chrome(&mut recorded)
                .context("paint full-window client chrome")?;
            self.ui_items.append(&mut chrome_items);
        }
        match self.content_view_fade_opacity(Instant::now()) {
            // Arriving: the terminal is underneath this frame, so the view has
            // to be lifted clear of it. Card textures are not in the heap;
            // lift their composites to the fade layer at the fade's opacity
            // so the pictures arrive as part of the view.
            Some(opacity) => {
                {
                    let mut composites = self.card_composites.borrow_mut();
                    for composite in composites[composites_before..].iter_mut() {
                        composite.zindex = crate::termwindow::CONTENT_VIEW_FADE_ZINDEX;
                        composite.opacity *= opacity;
                    }
                }
                self.composite_above_terminal(&heap, opacity)?
            }
            // Settled: nothing else is on screen to be ordered against.
            None => heap.apply_to_clipped(layers, self.surface_clip(), 1.0)?,
        }
        self.content_view_last_frame = Some(heap);
        // The heap holds no thumbnail quads on the texture path, so a closing
        // ghost needs this frame's composites to fade the pictures out.
        *self.content_view_last_composites.borrow_mut() =
            self.card_composites.borrow()[composites_before..].to_vec();
        Ok(())
    }

    /// Composite the recorded frame of a view that has already been closed.
    fn paint_departing_content_view(&mut self) -> anyhow::Result<()> {
        let opacity = self
            .content_view_fade_opacity(Instant::now())
            .unwrap_or(0.0);
        let Some(ghost) = self
            .content_view_fade
            .as_ref()
            .and_then(|fade| fade.ghost.as_ref())
        else {
            return Ok(());
        };
        let gl_state = self.render_state.as_ref().unwrap();
        let layer = gl_state
            .layer_for_zindex(crate::termwindow::CONTENT_VIEW_FADE_ZINDEX)
            .context("departing content view layer")?;
        let mut layers = layer.quad_allocator();
        ghost.apply_to_clipped(&mut layers, self.surface_clip(), opacity)?;
        // Card pictures live in textures, not in the ghost heap: fade them
        // with it. NOTE: these draw after the base fills of the main layer,
        // i.e. underneath the travelling ghost chrome above them.
        let mut composites = self.card_composites.borrow_mut();
        for saved in self.content_view_last_composites.borrow().iter() {
            composites.push(CardComposite {
                opacity,
                zindex: crate::termwindow::CONTENT_VIEW_FADE_ZINDEX,
                ..saved.clone()
            });
        }
        Ok(())
    }

    pub fn paint_content_view(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
    ) -> anyhow::Result<()> {
        let settings = crate::native_settings::load_shared();
        let font_weight = crate::native_settings::settings_font_weight(&settings);
        let active_content_view_idx = self.active_content_view_index();
        let typography = active_content_view_idx
            .map(|idx| self.content_views[idx].view.typography())
            .unwrap_or_default();
        let (ui_font, title_font, section_font) = match typography {
            ContentViewTypography::Default => {
                let font_size = crate::native_settings::home_font_size(&settings);
                (
                    self.fonts
                        .command_palette_font_with_size_and_weight(font_size, font_weight)?,
                    self.fonts
                        .title_font_with_size_and_weight(font_size + 10.0, font_weight.max(760))?,
                    self.fonts
                        .title_font_with_size_and_weight(font_size + 2.0, font_weight.max(700))?,
                )
            }
            ContentViewTypography::Overview => (
                self.fonts.command_palette_font_with_size_and_weight(
                    crate::native_settings::settings_font_size(&settings),
                    font_weight,
                )?,
                self.fonts
                    .title_font_with_size(crate::native_settings::sidebar_font_size())?,
                self.fonts
                    .title_font_with_size(crate::native_settings::pane_header_font_size())?,
            ),
        };
        let render_metrics =
            crate::utilsprites::RenderMetrics::with_font_metrics(&ui_font.metrics());
        let dimensions = self.dimensions;
        let palette = self.chrome();

        // Occupy the terminal content area between the workspace and right
        // sidebars, below the top tab bar and above a bottom tab bar.
        let area = self.content_view_area();
        let surface = euclid::rect(
            0.0,
            0.0,
            dimensions.pixel_width as f32,
            dimensions.pixel_height as f32,
        );
        // Cursor blink: only animate when the view wants it (focused input).
        let wants_blink = active_content_view_idx
            .map(|idx| self.content_views[idx].view.wants_cursor_blink())
            .unwrap_or(false);
        let blink_ms = (self.config.cursor_blink_rate as u64).max(100);
        let cursor_on = if wants_blink {
            let ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0);
            (ms / blink_ms as u128) % 2 == 0
        } else {
            true
        };
        if wants_blink {
            self.update_next_frame_time(Some(Instant::now() + Duration::from_millis(blink_ms)));
        }

        // Measured before the view is borrowed, and re-measured every frame:
        // the terminal area behind a full-window view keeps changing shape
        // while the view is up.
        let host_preview_aspect = {
            let content = self.terminal_content_rect();
            if content.size.width > 0.0 && content.size.height > 0.0 {
                content.size.width / content.size.height
            } else {
                0.0
            }
        };

        let defer_preview_captures = self
            .content_view_fade
            .as_ref()
            .is_some_and(|fade| fade.flight.is_none());
        let (next_frame, previews) = {
            let gl_state = self.render_state.as_ref().unwrap();
            let ctx = DrawContext::new(gl_state, dimensions, &render_metrics);
            if let Some(idx) = active_content_view_idx {
                let view = self.content_views[idx].view.as_mut();
                view.set_defer_preview_captures(defer_preview_captures);
                view.set_host_preview_aspect(host_preview_aspect);
                view.paint_surface_background(&ctx, layers, surface, palette)?;
                view.paint(
                    &ctx,
                    layers,
                    area,
                    palette,
                    &ui_font,
                    &title_font,
                    &section_font,
                    cursor_on,
                )?;
                (view.next_frame_time(), view.terminal_previews())
            } else {
                (None, Vec::new())
            }
        };
        self.paint_terminal_previews(layers, &previews)?;
        {
            let gl_state = self.render_state.as_ref().unwrap();
            let ctx = DrawContext::new(gl_state, dimensions, &render_metrics);
            if let Some(idx) = active_content_view_idx {
                self.content_views[idx].view.paint_after_terminal_previews(
                    &ctx,
                    layers,
                    area,
                    palette,
                    &ui_font,
                    &title_font,
                    &section_font,
                )?;
            }
        }
        self.update_next_frame_time(next_frame);
        Ok(())
    }

    /// Render live, read-only terminal thumbnails requested by a ContentView.
    /// This reuses the normal screen-line renderer at a smaller font scale;
    /// panes are never resized and no input or focus is sent to them.
    fn paint_terminal_previews(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        previews: &[TerminalPreviewRequest],
    ) -> anyhow::Result<()> {
        // Drop the cards that are no longer asked for -- but only while there
        // are cards to compare against. Closing the overview asks for none, and
        // pruning on that frame threw away every recorded thumbnail; reopening
        // then rebuilt all of them in the single frame that revealed them,
        // which is the 100-170ms freeze the gesture was landing in. Nothing
        // about a closed overview says the cards are gone, and a stale entry
        // costs one key comparison to reject.
        if !previews.is_empty() {
            self.preview_cache_last_wanted = Some(Instant::now());
            let live: HashSet<TabId> = previews.iter().map(|preview| preview.tab_id).collect();
            self.preview_quad_cache
                .borrow_mut()
                .retain(|tab_id, _| live.contains(tab_id));
            self.preview_content_fade
                .borrow_mut()
                .retain(|tab_id, _| live.contains(tab_id));
            // Same policy for a rebuild sliced across frames: its card is gone.
            let mut partial = self.preview_rebuild_partial.borrow_mut();
            if partial
                .as_ref()
                .is_some_and(|partial| !live.contains(&partial.tab_id))
            {
                *partial = None;
            }
        }
        let started = crate::perf::now();
        crate::perf::reset_accums();
        let mut rebuilt = 0usize;
        // How many cards may rebuild their quads in this frame.
        //
        // A rebuild is ~10ms and a replay is ~0.3ms, so a frame that rebuilds
        // two cards has already missed a 120Hz deadline and a frame that
        // rebuilds five has frozen the window for a fifth of a second. Cards
        // over the budget replay what they last showed and ask for another
        // frame, so the picture is at worst one frame out of date and the cost
        // is spread instead of spiked.
        let mut budget = MAX_PREVIEW_QUAD_REBUILDS_PER_FRAME;
        for preview in previews {
            if self.paint_terminal_preview(layers, preview, &mut budget)? {
                rebuilt += 1;
            }
        }
        // What a frame spent on thumbnails, and how much of that was a card
        // whose quads could not be replayed. Without the split, a slow frame
        // says nothing about whether the cache missed or whether the cost is
        // somewhere else entirely.
        crate::perf::log_duration("preview_paint", started);
        crate::perf::log_counter("preview_cards", previews.len());
        crate::perf::log_counter("preview_rebuilds", rebuilt);
        if rebuilt > 0 {
            crate::perf::log_accums("pv_");
        } else {
            crate::perf::reset_accums();
        }
        // A shaping cache sitting at its capacity is a cache that is evicting
        // entries it is about to be asked for again. Reading the occupancy is
        // the difference between knowing that and inferring it from timings.
        if crate::perf::enabled() {
            crate::perf::log_counter("shape_cache_len", self.shape_cache.borrow().len());
            crate::perf::log_counter(
                "line_shape_cache_len",
                self.line_to_ele_shape_cache.borrow().len(),
            );
            crate::perf::log_counter("line_quad_cache_len", self.line_quad_cache.borrow().len());
        }
        Ok(())
    }

    /// The opacity a card's picture should draw at this frame: 1.0 for a
    /// settled card, ramping 0->1 across [`PREVIEW_CONTENT_FADE`] from the
    /// moment its terminal first shows visible content. Requests further
    /// frames itself while a ramp is running.
    fn preview_content_alpha(&self, preview: &TerminalPreviewRequest) -> f32 {
        let now = Instant::now();
        let snapshot_ptr = Arc::as_ptr(&preview.snapshot) as usize;
        let mut fades = self.preview_content_fade.borrow_mut();
        let entry = fades.entry(preview.tab_id).or_insert_with(|| {
            if snapshot_has_content(&preview.snapshot) {
                PreviewContentFade::ContentSince(now)
            } else {
                PreviewContentFade::Blank {
                    snapshot: snapshot_ptr,
                }
            }
        });
        if let PreviewContentFade::Blank { snapshot } = entry {
            if *snapshot != snapshot_ptr {
                if snapshot_has_content(&preview.snapshot) {
                    *entry = PreviewContentFade::ContentSince(now);
                } else {
                    *snapshot = snapshot_ptr;
                }
            }
        }
        match entry {
            // A blank picture looks the same at any opacity; full keeps the
            // card's (invisible) quads out of the blending special cases.
            PreviewContentFade::Blank { .. } => 1.0,
            PreviewContentFade::ContentSince(since) => {
                let t =
                    now.duration_since(*since).as_secs_f32() / PREVIEW_CONTENT_FADE.as_secs_f32();
                if t >= 1.0 {
                    1.0
                } else {
                    self.update_next_frame_time(Some(Instant::now()));
                    // Ease out: fast early rise reveals the content sooner.
                    let t = t.max(0.0);
                    t * (2.0 - t)
                }
            }
        }
    }

    /// Draw one card's thumbnail, building its quads only when they cannot be
    /// replayed.
    ///
    /// A thumbnail is thousands of quads and rebuilding one means re-shaping
    /// every line of the terminal it depicts. Doing that per card per frame
    /// was the whole cost of the overview: measured over 4621 frames it put
    /// the median frame at 14.8ms against a 8.3ms budget, and the snapshots
    /// feeding it were mostly *unchanged* -- the work was being redone to
    /// arrive at the same picture. Keeping the quads is what turns a repaint
    /// into a copy.
    ///
    /// Returns whether the quads had to be rebuilt.
    fn paint_terminal_preview(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        preview: &TerminalPreviewRequest,
        budget: &mut usize,
    ) -> anyhow::Result<bool> {
        let key = preview_quad_key(
            preview,
            &self.dimensions,
            self.shape_generation,
            self.preview_scale_estimate(preview),
        );
        let clip = quad_clip_rect(preview.clip, &self.dimensions);
        let content_alpha = self.preview_content_alpha(preview);

        // A hit replays. A miss replays too -- but only when the miss is the
        // snapshot alone and the budget is spent, because then the recorded
        // quads are merely a refresh out of date, which at thumbnail size is
        // not a difference anyone can see. A geometry miss is never replayed:
        // those quads are wrong, not old.
        let mut deferred = false;
        let recorded_area = self
            .preview_quad_cache
            .borrow()
            .get(&preview.tab_id)
            .filter(|cached| {
                if cached.key == key {
                    return true;
                }
                if cached.key.geometry == key.geometry && *budget == 0 {
                    deferred = true;
                    return true;
                }
                false
            })
            .map(|cached| cached.area);
        if deferred {
            // Come straight back rather than naming a time: this card owes a
            // rebuild and should get it as soon as a frame has room.
            self.update_next_frame_time(Some(Instant::now()));
        }
        if let Some(recorded_area) = recorded_area {
            // Texture path: the card's picture already lives in its own
            // texture; a single textured quad replaces the whole replay.
            if self.card_texture_path_active()
                && self.composite_cached_card(preview, content_alpha)?
            {
                return Ok(false);
            }
            let cache = self.preview_quad_cache.borrow();
            let cached = cache
                .get(&preview.tab_id)
                .expect("entry was present a statement ago and nothing removes it");
            // Scrolling moves a card without changing what it shows. The quads
            // carry the position they were recorded at, so a card that has
            // moved is replayed through the same source->target mapping the
            // flight animation uses. The key carries the card's *size*, so a
            // matching key guarantees the two rects are the same shape and the
            // mapping degenerates to a translation -- none of the softening
            // that a genuinely rescaled replay costs. A card that has not moved
            // takes the plain path, which is bit-for-bit what this drew before
            // the cache existed.
            if recorded_area == preview.area {
                cached.heap.apply_to_clipped(layers, clip, content_alpha)?;
            } else {
                let source = quad_clip_rect(recorded_area, &self.dimensions);
                let target = quad_clip_rect(preview.area, &self.dimensions);
                cached
                    .heap
                    .apply_to_scaled(layers, source, target, clip, content_alpha)?;
            }
            return Ok(false);
        }

        *budget = budget.saturating_sub(1);

        // Continue the sliced rebuild when the slot holds this card and the
        // key still describes the same picture; anything else in the slot is
        // stale and a fresh build starts. The old cache entry stays in place
        // meanwhile -- it is what keeps the card showing its previous picture
        // until the new one is complete.
        let mut partial = match self.preview_rebuild_partial.borrow_mut().take() {
            Some(partial) if partial.tab_id == preview.tab_id && partial.key == key => partial,
            _ => {
                // When the old entry's texture can carry the display duty by
                // itself, take its quad storage: recycling keeps the rebuild
                // allocation-free instead of freeing thousands of quads and
                // asking for the same memory back a moment later.
                let mut heap = HeapQuadAllocator::default();
                if let Some(cached) = self
                    .preview_quad_cache
                    .borrow_mut()
                    .get_mut(&preview.tab_id)
                    .filter(|cached| cached.texture.is_some())
                {
                    heap = std::mem::take(&mut cached.heap);
                    heap.recycle();
                }
                PreviewRebuildPartial {
                    tab_id: preview.tab_id,
                    key,
                    snapshot: Arc::clone(&preview.snapshot),
                    heap,
                    pane_idx: 0,
                    line_idx: 0,
                }
            }
        };
        let finished = {
            let card_started = crate::perf::now();
            let deadline = Instant::now() + PREVIEW_REBUILD_SLICE;
            let mut heap_layers = TripleLayerQuadAllocator::Heap(&mut partial.heap);
            let finished = self.paint_terminal_preview_unclipped(
                &mut heap_layers,
                preview,
                &mut partial.pane_idx,
                &mut partial.line_idx,
                deadline,
            )?;
            crate::perf::log_duration("preview_rebuild_card", card_started);
            finished
        };
        if !finished {
            // Keep the card's previous picture on screen: its texture
            // stretches to the current rect even across a geometry change,
            // and a card that never had one simply stays background until
            // the first build lands.
            if !(self.card_texture_path_active()
                && self.composite_cached_card(preview, content_alpha)?)
            {
                let cache = self.preview_quad_cache.borrow();
                if let Some(cached) = cache.get(&preview.tab_id) {
                    if cached.area == preview.area {
                        cached.heap.apply_to_clipped(layers, clip, content_alpha)?;
                    } else {
                        let source = quad_clip_rect(cached.area, &self.dimensions);
                        let target = quad_clip_rect(preview.area, &self.dimensions);
                        cached
                            .heap
                            .apply_to_scaled(layers, source, target, clip, content_alpha)?;
                    }
                }
            }
            *self.preview_rebuild_partial.borrow_mut() = Some(partial);
            // The rest of this card's build should get the very next frame.
            self.update_next_frame_time(Some(Instant::now()));
            return Ok(true);
        }
        let PreviewRebuildPartial { heap, .. } = partial;
        let prior_texture = self
            .preview_quad_cache
            .borrow_mut()
            .remove(&preview.tab_id)
            .and_then(|cached| cached.texture);
        let mut texture = None;
        let result = if self.card_texture_path_active() {
            // Reuse the previous texture when the card size is unchanged --
            // content changes every refresh interval and reallocating a
            // texture for each one would churn.
            let wanted_w = (preview.area.size.width.ceil() as u32).max(1);
            let wanted_h = (preview.area.size.height.ceil() as u32).max(1);
            let tex = match prior_texture.filter(|t| t.width == wanted_w && t.height == wanted_h) {
                Some(prior) => Ok(prior),
                None => self.create_card_texture(preview.area),
            };
            match tex {
                Ok(tex) => {
                    let (first_vertex, quad_count) = {
                        let mut frame_verts = self.card_frame_verts.borrow_mut();
                        let base = frame_verts.len();
                        heap.extract_vertices(&mut frame_verts);
                        (base, (frame_verts.len() - base) / 4)
                    };
                    self.pending_card_renders
                        .borrow_mut()
                        .push(PendingCardRender {
                            texture: Rc::clone(&tex),
                            first_vertex,
                            quad_count,
                            area: preview.area,
                        });
                    self.card_composites.borrow_mut().push(CardComposite {
                        texture: Rc::clone(&tex),
                        dest: preview.area,
                        clip: preview.clip,
                        opacity: content_alpha,
                        zindex: 0,
                    });
                    texture = Some(tex);
                    Ok(true)
                }
                Err(err) => {
                    log::warn!("card texture unavailable, replaying quads: {err:#}");
                    heap.apply_to_clipped(layers, clip, content_alpha)
                        .map(|()| true)
                }
            }
        } else {
            heap.apply_to_clipped(layers, clip, content_alpha)
                .map(|()| true)
        };
        self.preview_quad_cache.borrow_mut().insert(
            preview.tab_id,
            CachedPreviewQuads {
                key,
                // Held so the address in `key` cannot be handed to a different
                // snapshot while this entry is alive.
                snapshot: Arc::clone(&preview.snapshot),
                area: preview.area,
                heap,
                texture,
            },
        );
        result
    }

    /// True while cards may draw as textures (WebGpu only). Transition
    /// frames also qualify: the card quads are not recorded into the fade
    /// heap; instead `paint_content_view_composited` lifts the composites to
    /// the fade layer and gives them the fade's opacity, so the pictures
    /// arrive and leave with the view while transition frames stay cheap.
    fn card_texture_path_active(&self) -> bool {
        self.webgpu.is_some()
    }

    fn create_card_texture(
        &self,
        area: RectF,
    ) -> anyhow::Result<Rc<crate::termwindow::webgpu::CardRenderTexture>> {
        let webgpu = self
            .webgpu
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("no webgpu state"))?;
        let width = (area.size.width.ceil() as u32).max(1);
        let height = (area.size.height.ceil() as u32).max(1);
        Ok(Rc::new(crate::termwindow::webgpu::CardRenderTexture::new(
            width, height, webgpu,
        )?))
    }

    /// Composite a cached card as one textured quad, first rendering the
    /// cached heap into a fresh texture if the entry does not have one yet
    /// (recorded during a transition, when the texture path is off). Returns
    /// false when there is nothing usable and the caller must replay quads.
    fn composite_cached_card(
        &self,
        preview: &TerminalPreviewRequest,
        opacity: f32,
    ) -> anyhow::Result<bool> {
        let (needs_texture, recorded_area) = {
            let cache = self.preview_quad_cache.borrow();
            let Some(cached) = cache.get(&preview.tab_id) else {
                return Ok(false);
            };
            (cached.texture.is_none(), cached.area)
        };
        if needs_texture {
            let texture = match self.create_card_texture(recorded_area) {
                Ok(texture) => texture,
                Err(err) => {
                    log::warn!("card texture unavailable, replaying quads: {err:#}");
                    return Ok(false);
                }
            };
            let (first_vertex, quad_count) = {
                let mut cache = self.preview_quad_cache.borrow_mut();
                let Some(cached) = cache.get_mut(&preview.tab_id) else {
                    return Ok(false);
                };
                let mut frame_verts = self.card_frame_verts.borrow_mut();
                let base = frame_verts.len();
                cached.heap.extract_vertices(&mut frame_verts);
                cached.texture = Some(Rc::clone(&texture));
                (base, (frame_verts.len() - base) / 4)
            };
            self.pending_card_renders
                .borrow_mut()
                .push(PendingCardRender {
                    texture,
                    first_vertex,
                    quad_count,
                    area: recorded_area,
                });
        }
        let Some(texture) = self
            .preview_quad_cache
            .borrow()
            .get(&preview.tab_id)
            .and_then(|cached| cached.texture.clone())
        else {
            return Ok(false);
        };
        self.card_composites.borrow_mut().push(CardComposite {
            texture,
            dest: preview.area,
            clip: preview.clip,
            opacity,
            zindex: 0,
        });
        Ok(true)
    }

    /// Build one card's thumbnail into `layers`, whole.
    ///
    /// Everything here is laid out against `preview.area` -- the card's full
    /// rectangle -- and nothing against `preview.clip`. That is what lets the
    /// result be kept: a heap built against the visible slice would hold only
    /// the rows that happened to be on screen when it was recorded, and the
    /// first scroll would reveal the gap. Cropping is the replaying caller's
    /// job, and it already does it.
    /// The scale a card's grid will be rendered at, before raster-metric
    /// correction: a pure function of the card's bucketed size, the
    /// terminal's shape and the drag-time hold. Cheap -- no font metrics --
    /// so the quad-cache key can carry the resolved value instead of the
    /// hold *flag*: a drag whose hold resolves to the scale already on
    /// screen then reads as the same geometry, where the flag flipping used
    /// to rebuild every card at gesture start and end.
    fn preview_scale_estimate(&self, preview: &TerminalPreviewRequest) -> f64 {
        let tab_size = preview.snapshot.tab_size;
        if tab_size.cols == 0 || tab_size.rows == 0 {
            return 1.0;
        }
        let bucketed_width =
            quantized_terminal_preview_extent(preview.area.size.width, self.dimensions.dpi);
        let bucketed_height =
            quantized_terminal_preview_extent(preview.area.size.height, self.dimensions.dpi);
        let width_ratio = bucketed_width
            / (tab_size.cols as f32 * self.render_metrics.cell_size.width.max(1) as f32);
        let height_ratio = bucketed_height
            / (tab_size.rows as f32 * self.render_metrics.cell_size.height.max(1) as f32);
        let global_scale = self.fonts.get_font_scale();
        let minimum_scale = minimum_terminal_preview_scale(
            self.config.font_size,
            self.dimensions.dpi,
            global_scale,
        );
        let maximum_scale = (global_scale * 0.84).max(minimum_scale);
        let desired_scale = (global_scale * f64::from(width_ratio.min(height_ratio)))
            .clamp(minimum_scale, maximum_scale);
        let mut quantized_scale =
            quantize_terminal_preview_scale_down(desired_scale, minimum_scale);
        // Mid-drag, keep the scale this grid was last drawn at. The search
        // in the render path would otherwise walk a new bucket every few
        // pixels of card width, and every bucket is a `FontConfiguration`.
        if preview.hold_scale {
            if let Some(held) = self
                .preview_scale_hold
                .borrow()
                .get(&(tab_size.cols, tab_size.rows))
            {
                quantized_scale = *held;
            }
        }
        quantized_scale
    }

    /// Returns whether the card is complete. `false` means the slice deadline
    /// arrived first: the resume indices point at the next line to render and
    /// the caller re-enters with the same heap on a later frame. Everything
    /// outside the line loop is either recomputed idempotently on re-entry
    /// (scale search, font lookups) or guarded by the resume indices (the
    /// background fills, which would otherwise be recorded twice).
    fn paint_terminal_preview_unclipped(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        preview: &TerminalPreviewRequest,
        resume_pane: &mut usize,
        resume_line: &mut usize,
        deadline: Instant,
    ) -> anyhow::Result<bool> {
        let snapshot = &preview.snapshot;
        let tab_size = snapshot.tab_size;
        if tab_size.cols == 0 || tab_size.rows == 0 {
            return Ok(true);
        }
        if snapshot.panes.is_empty() {
            return Ok(true);
        }

        // A thumbnail represents the whole terminal surface, not just the
        // shrunken PTY grid. Fill any aspect-ratio remainder with the active
        // terminal's own background so the card never looks letterboxed.
        let preview_background = snapshot
            .panes
            .iter()
            .find(|pane| pane.is_active)
            .or_else(|| snapshot.panes.first())
            .map(|pane| pane.palette.resolve_bg(ColorAttribute::Default).to_linear())
            .expect("checked that the tab has panes");
        if *resume_pane == 0 && *resume_line == 0 {
            self.filled_rectangle(layers, 0, preview.area, preview_background)?;
        }

        // Quantization bounds the number of cached FontConfigurations even
        // when cards continuously resize. Flooring guarantees the terminal
        // grid stays inside its preview rather than clipping the last column.
        let bucketed_width =
            quantized_terminal_preview_extent(preview.area.size.width, self.dimensions.dpi);
        let bucketed_height =
            quantized_terminal_preview_extent(preview.area.size.height, self.dimensions.dpi);
        let global_scale = self.fonts.get_font_scale();
        let minimum_scale = minimum_terminal_preview_scale(
            self.config.font_size,
            self.dimensions.dpi,
            global_scale,
        );
        // Shared with the quad-cache key: both must resolve the drag-time
        // hold the same way, or a gesture's start and end read as geometry
        // changes and rebuild every card in one frame.
        let mut quantized_scale = self.preview_scale_estimate(preview);
        let scale_key = (tab_size.cols, tab_size.rows);
        let (mut font_config, mut metrics) = self.pane_font_resources(quantized_scale)?;

        // Font raster metrics are integer pixels and therefore do not scale
        // perfectly linearly. Correct the analytical estimate once using the
        // actual metrics; if the one-pixel raster floor is still too large, the
        // hard quad clip below remains the final safety boundary.
        let rendered_width = tab_size.cols as f32 * metrics.cell_size.width.max(1) as f32;
        let rendered_height = tab_size.rows as f32 * metrics.cell_size.height.max(1) as f32;
        let correction = (bucketed_width / rendered_width)
            .min(bucketed_height / rendered_height)
            .min(1.0);
        if correction < 1.0 {
            let corrected_scale = quantize_terminal_preview_scale_down(
                quantized_scale * f64::from(correction) * 0.999,
                minimum_scale,
            );
            if corrected_scale < quantized_scale {
                quantized_scale = corrected_scale;
                (font_config, metrics) = self.pane_font_resources(quantized_scale)?;
            }
        }

        // Step down a bucket at a time until the grid fits.
        //
        // This used to answer any remaining overflow by dropping straight to
        // `minimum_scale`, which is a cliff rather than a correction: being one
        // pixel too tall after the analytical estimate is a rounding artefact
        // of integer raster metrics, and paying for it with the smallest font
        // the preview allows collapsed the whole thumbnail to 1x3px cells --
        // a thumb-sized smear of text in the corner of an otherwise empty card.
        // Reachable as soon as a card is tall enough relative to its terminal,
        // which is what opening a sidebar does.
        //
        // The step is bounded: each iteration builds a FontConfiguration, and
        // the estimate is close enough that this normally settles in one.
        for _ in 0..MAX_PREVIEW_SCALE_STEPS {
            if quantized_scale <= minimum_scale {
                break;
            }
            let too_wide =
                tab_size.cols as f32 * metrics.cell_size.width.max(1) as f32 > bucketed_width;
            let too_tall =
                tab_size.rows as f32 * metrics.cell_size.height.max(1) as f32 > bucketed_height;
            if !too_wide && !too_tall {
                break;
            }
            let next = quantize_terminal_preview_scale_down(
                quantized_scale - 1.0 / TERMINAL_PREVIEW_SCALE_BUCKETS_PER_UNIT,
                minimum_scale,
            );
            if next >= quantized_scale {
                break;
            }
            quantized_scale = next;
            (font_config, metrics) = self.pane_font_resources(quantized_scale)?;
        }
        // Whatever the search settled on is what a drag will hold to. Recorded
        // even mid-drag, because the stepping loop above may still have had to
        // come down to make the grid fit a card that has since shrunk.
        self.preview_scale_hold
            .borrow_mut()
            .insert(scale_key, quantized_scale);

        let cell_width = metrics.cell_size.width.max(1) as f32;
        let cell_height = metrics.cell_size.height.max(1) as f32;

        // Close the gap left by whole-pixel cells.
        //
        // A thumbnail cell is 6px or 7px and nothing in between, and the scale
        // search can only round down, so the grid routinely stops a whole cell
        // short across its full width -- 87 columns at 6px is 522px inside a
        // 610px card, a bare strip down the right-hand side. The per-pane
        // position transform below already maps rendered text into whatever
        // rect the layout asks for; it simply had nothing to do, because the
        // layout asked for exactly the size the text already was. Stretching
        // the layout is what gives it something to do. One factor for both
        // axes, so this enlarges the picture rather than distorting it.
        let (fill_x, fill_y) = preview_fill_factors(
            tab_size.cols as f32 * cell_width,
            tab_size.rows as f32 * cell_height,
            preview.area.size.width,
            preview.area.size.height,
        );
        let cell_width = cell_width * fill_x;
        let cell_height = cell_height * fill_y;
        // Terminal content begins at the same top-left origin as the real
        // terminal. Any remainder stays on the right/bottom and is visually
        // continuous with the background painted above.
        let origin_x = preview.area.origin.x;
        let origin_y = preview.area.origin.y;

        // One nav bar height for every pane in the tab, because that is how the
        // terminal draws it.
        let nav_bar_height = self.pane_nav_bar_height() as f32;

        let gl_state = self.render_state.as_ref().unwrap();
        let white_space = gl_state.util_sprites.white_space.texture_coords();
        let filled_box = gl_state.util_sprites.filled_box.texture_coords();
        let mut hidden_cursor = StableCursorPosition::default();
        hidden_cursor.y = isize::MIN;

        let mut rendered_this_slice = 0usize;
        for (pane_idx, pane) in snapshot.panes.iter().enumerate().skip(*resume_pane) {
            // Lines already recorded in an earlier slice of this same build.
            let start_line = if pane_idx == *resume_pane {
                *resume_line
            } else {
                0
            };
            let palette = &pane.palette;
            let pane_x = origin_x + pane.left as f32 * cell_width;
            let pane_y = origin_y + pane.top as f32 * cell_height;
            let pane_width = pane.width as f32 * cell_width;
            let pane_height = pane.height as f32 * cell_height;
            let pane_rect = euclid::rect(pane_x, pane_y, pane_width, pane_height);
            let Some(pane_bounds) = pane_rect.intersection(&preview.area) else {
                continue;
            };
            if start_line == 0 {
                self.filled_rectangle(
                    layers,
                    0,
                    pane_bounds,
                    palette.resolve_bg(ColorAttribute::Default).to_linear(),
                )?;
            }

            let source_dims = pane.dimensions;
            let rows = pane.rows;
            let cols = pane.cols;
            if rows == 0 || cols == 0 {
                continue;
            }

            // The real terminal reserves the top of a pane's box for its nav
            // bar and starts the grid below it: `terminal_size_for_positioned_pane`
            // subtracts that height before dividing into rows. `pane.height`
            // is the whole box, so laying the grid out against the box's top
            // edge both lifts the text by the height of a nav bar and stretches
            // it vertically, mapping the same rows onto a taller target. The
            // strip left behind is the picture the terminal shows once its
            // chrome is taken away, which is what a card is.
            // Take the nav bar's real height rather than inferring it from
            // what the grid left over. That leftover is the nav bar *plus* the
            // remainder of dividing the box by a cell, and the remainder
            // depends on each pane's own cell height -- so two panes sharing
            // one nav bar derived strips 22px apart and their text no longer
            // lined up across the split, which it does in the terminal.
            let reserved = pane
                .box_pixel_height
                .saturating_sub(source_dims.pixel_height);
            let nav_fraction = if reserved > 0 && pane.box_pixel_height > 0 {
                (nav_bar_height / pane.box_pixel_height as f32).clamp(0.0, 0.5)
            } else {
                0.0
            };
            let grid_top = pane_rect.min_y() + pane_height * nav_fraction;
            let grid_height = pane_height * (1.0 - nav_fraction);

            // Pane placement remains in the root grid so every split keeps
            // the same outer frame. Content inside that frame uses the pane's
            // own effective cell size, reconstructed from the immutable
            // snapshot. This is the preview equivalent of the normal pane
            // renderer's `pane_font_resources` path.
            let pane_scale_ratio = terminal_preview_pane_scale_ratio(tab_size, source_dims);
            let pane_scale = quantize_terminal_preview_scale_down(
                quantized_scale * pane_scale_ratio,
                minimum_scale,
            );
            let (pane_font_config, pane_metrics) =
                if pane_scale.to_bits() == quantized_scale.to_bits() {
                    (font_config.clone(), metrics)
                } else {
                    self.pane_font_resources(pane_scale)?
                };

            let pane_cell_width = pane_metrics.cell_size.width.max(1) as f32;
            let pane_cell_height = pane_metrics.cell_size.height.max(1) as f32;
            let rendered_width = cols as f32 * pane_cell_width;
            let rendered_height = rows as f32 * pane_cell_height;

            let mut render_dims = source_dims;
            render_dims.cols = cols;
            render_dims.viewport_rows = rows;
            render_dims.pixel_width = rendered_width.round() as usize;
            render_dims.pixel_height = rendered_height.round() as usize;
            let foreground = palette.foreground.to_linear();
            let default_bg = palette.background.to_linear();
            // LineToElementShape caches resolved colors as well as glyph
            // geometry. Include the pane palette so two terminals with the
            // same text/ANSI indexes cannot reuse each other's resolved color
            // values inside the overview.
            let mut palette_hasher = DefaultHasher::new();
            palette.colors.0.hash(&mut palette_hasher);
            palette.foreground.hash(&mut palette_hasher);
            palette.background.hash(&mut palette_hasher);
            palette.cursor_fg.hash(&mut palette_hasher);
            palette.cursor_bg.hash(&mut palette_hasher);
            palette.cursor_border.hash(&mut palette_hasher);
            palette.selection_fg.hash(&mut palette_hasher);
            palette.selection_bg.hash(&mut palette_hasher);
            let palette_identity = palette_hasher.finish();
            let font_identity =
                pane_scale.to_bits() ^ palette_identity.rotate_left(17) ^ 0x4c49_5645_5052_4556;

            // Map positions as the pane is authored. At thumbnail sizes a
            // cell can only jump from (for example) 3 px to 4 px, so no font
            // scale can fill both axes exactly. Applying this tiny correction
            // during allocation keeps exact pane geometry without a second
            // CPU walk over every htop glyph each frame.
            //
            // The source space starts at the origin rather than at the card's
            // corner, so everything drawn through this transform is expressed
            // relative to the pane itself. That is what makes the line quads
            // below cacheable: `LineQuadCacheKey` carries `top_pixel_y` and
            // `left_pixel_x`, and while those were the card's position on
            // screen, scrolling the overview minted a fresh key for every line
            // at every scroll offset -- thousands of entries that would never
            // be asked for again, evicting the ones that would. Measured with
            // four cards open, the cache sat pegged at its 4096 capacity and a
            // card that had to rebuild cost 7.9ms against 1.7ms for one that
            // did not.
            let source_rect = QuadClipRect::from_top_left_pixels(
                0.0,
                0.0,
                rendered_width,
                rendered_height,
                &self.dimensions,
            );
            let target_rect = QuadClipRect::from_top_left_pixels(
                pane_rect.min_x(),
                grid_top,
                pane_rect.max_x(),
                grid_top + grid_height,
                &self.dimensions,
            );
            // Bind the call before asserting on it. `debug_assert!` does not
            // evaluate its argument in release, and this workspace ships
            // release without debug assertions, so writing the call inside the
            // macro meant the transform was never applied in the build users
            // run -- panes were drawn at their authored size and whatever did
            // not fit was clipped away.
            let transformed = layers.set_heap_position_transform(Some((source_rect, target_rect)));
            debug_assert!(transformed);
            // In the same pane-relative space as `source_rect`.
            let source_visible_top =
                (pane_bounds.min_y() - grid_top) * rendered_height / grid_height.max(1.0);
            let source_visible_bottom =
                (pane_bounds.max_y() - grid_top) * rendered_height / grid_height.max(1.0);

            for (line_idx, line) in pane.lines.iter().take(rows).enumerate().skip(start_line) {
                let y = line_idx as f32 * pane_cell_height;
                if y + pane_cell_height <= source_visible_top || y >= source_visible_bottom {
                    continue;
                }
                // Out of time: park the resume point at this line. Requiring
                // one rendered line first guarantees forward progress even
                // when a single line overruns the whole slice.
                if rendered_this_slice > 0 && Instant::now() >= deadline {
                    *resume_pane = pane_idx;
                    *resume_line = line_idx;
                    let cleared = layers.set_heap_position_transform(None);
                    debug_assert!(cleared);
                    return Ok(false);
                }
                let hash_started = crate::perf::now();
                let shape_hash = self.shape_hash_for_line(line);
                crate::perf::accum("line_hash", hash_started);

                // Deliberately *not* going through `line_quad_cache` here, the
                // way the real pane renderer does.
                //
                // It was tried, on the theory that a card whose snapshot moved
                // should only pay for the rows that actually changed. Measured,
                // it made a card rebuild 4.5x more expensive: 2.3ms per card
                // without it, 9.96ms with. Two reasons, both structural.
                //
                // The cache cannot hold a thumbnail's working set. Its key
                // carries the row a line was drawn at, so a terminal that
                // scrolls mints a fresh entry for every row it shifts text
                // through -- five cards of scrolling output filled all 4096
                // slots and stayed there, evicting entries as fast as they were
                // put in. Growing it is not the answer either: an entry is the
                // line's quads, so the capacity that would hold the working set
                // is measured in hundreds of megabytes.
                //
                // And a miss is not free. It allocates a heap, renders into it,
                // copies every quad a second time into `layers`, then evicts
                // someone else's entry to store it. At the hit rate a full
                // cache gives, that is pure overhead on top of the work it was
                // supposed to avoid -- and the work it was supposed to avoid is
                // already cheap, because `shape_cache` and
                // `line_to_ele_shape_cache` catch the re-shaping. Those two sit
                // at a few hundred entries of their thousands while this one is
                // pegged, which is the whole argument in two numbers.
                //
                // What does pay is the layer above: `preview_quad_cache` keeps
                // the finished card, so an unchanged terminal costs a replay
                // (1.41ms for four cards) and never reaches this loop at all.
                // Whatever else changes here, keep this path a straight render.
                //
                // Whatever the terminal itself was waiting to redraw for, put
                // back afterwards: a thumbnail must not claim the window's
                // animation deadline, and must not silently inherit one.
                let next_due = self.has_animation.borrow_mut().take();
                let line_started = crate::perf::now();
                self.render_screen_line(
                    RenderScreenLineParams {
                        top_pixel_y: y,
                        left_pixel_x: 0.0,
                        pixel_width: rendered_width,
                        stable_line_idx: Some(pane.resolved_top + line_idx as isize),
                        line,
                        selection: 0..0,
                        cursor: &hidden_cursor,
                        palette,
                        dims: &render_dims,
                        config: &self.config,
                        pane: None,
                        white_space,
                        filled_box,
                        cursor_border_color: palette.cursor_border.to_linear(),
                        foreground,
                        is_active: true,
                        selection_fg: palette.selection_fg.to_linear(),
                        selection_bg: palette.selection_bg.to_linear(),
                        cursor_fg: palette.cursor_fg.to_linear(),
                        cursor_bg: palette.cursor_bg.to_linear(),
                        cursor_is_default_color: true,
                        window_is_transparent: false,
                        default_bg,
                        font: None,
                        style: None,
                        use_pixel_positioning: false,
                        render_metrics: pane_metrics,
                        font_config: Some(pane_font_config.clone()),
                        font_identity,
                        shape_key: Some(LineToEleShapeCacheKey {
                            shape_hash,
                            composing: None,
                            shape_generation: self.shape_generation,
                            font_identity,
                        }),
                        password_input: false,
                        allow_images: false,
                        simple_shaping: true,
                    },
                    layers,
                )?;
                crate::perf::accum("line_render", line_started);
                // Restore by assignment, not min-merge: the line render above
                // may have claimed a blink/expiry deadline of its own, and a
                // merge would let the thumbnail keep it.
                *self.has_animation.borrow_mut() = next_due;
                rendered_this_slice += 1;
            }

            // Draw a non-blinking cursor for the active split. The regular
            // renderer intentionally receives a hidden cursor above, so this
            // thumbnail cannot start the foreground cursor animation timer.
            if pane.is_active {
                let cursor = pane.cursor;
                let cursor_row = cursor.y.saturating_sub(pane.resolved_top);
                if cursor.visibility == termwiz::surface::CursorVisibility::Visible
                    && cursor_row >= 0
                    && (cursor_row as usize) < rows
                    && cursor.x < cols
                {
                    // Still inside the position transform, so pane-relative
                    // like everything else drawn through it.
                    let cursor_rect: ::window::RectF = euclid::rect(
                        cursor.x as f32 * pane_cell_width,
                        cursor_row as f32 * pane_cell_height,
                        pane_cell_width,
                        pane_cell_height,
                    );
                    let color = palette.cursor_border.to_linear().mul_alpha(0.72);
                    let stroke = 1.0_f32.min(cursor_rect.size.width / 2.0);
                    self.filled_rectangle(
                        layers,
                        2,
                        euclid::rect(
                            cursor_rect.origin.x,
                            cursor_rect.origin.y,
                            cursor_rect.size.width,
                            stroke,
                        ),
                        color,
                    )?;
                    self.filled_rectangle(
                        layers,
                        2,
                        euclid::rect(
                            cursor_rect.origin.x,
                            cursor_rect.max_y() - stroke,
                            cursor_rect.size.width,
                            stroke,
                        ),
                        color,
                    )?;
                }
            }
            let cleared = layers.set_heap_position_transform(None);
            debug_assert!(cleared);
        }

        // Preserve split topology in the thumbnail.  PositionedSplit is part
        // of the immutable snapshot, so this pass also performs no mux reads.
        let split_color = snapshot
            .panes
            .iter()
            .find(|pane| pane.is_active)
            .or_else(|| snapshot.panes.first())
            .map(|pane| pane.palette.split.to_linear())
            .unwrap_or(preview_background);
        let split_stroke = (metrics.underline_height as f32 * 0.7).max(1.0);
        for split in &snapshot.splits {
            let rect = if split.direction == SplitDirection::Horizontal {
                euclid::rect(
                    origin_x + (split.left as f32 + 0.5) * cell_width,
                    origin_y + (split.top as f32 - 0.5) * cell_height,
                    split_stroke,
                    (1.0 + split.size as f32) * cell_height,
                )
            } else {
                euclid::rect(
                    origin_x + (split.left as f32 - 0.5) * cell_width,
                    origin_y + (split.top as f32 + 0.5) * cell_height,
                    (1.0 + split.size as f32) * cell_width,
                    split_stroke,
                )
            };
            if let Some(visible) = rect.intersection(&preview.area) {
                self.filled_rectangle(layers, 2, visible, split_color)?;
            }
        }
        Ok(true)
    }

    pub fn paint_modal(&mut self) -> anyhow::Result<()> {
        if let Some(modal) = self.get_modal() {
            for computed in modal.computed_element(self)?.iter() {
                let mut ui_items = computed.ui_items();

                let gl_state = self.render_state.as_ref().unwrap();
                self.render_element(&computed, gl_state, None)?;

                self.ui_items.append(&mut ui_items);
            }
        }

        Ok(())
    }

    fn paint_bottom_quote(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
    ) -> anyhow::Result<()> {
        let settings = crate::native_settings::load_shared();
        if !settings.terminal.bottom_quote_enabled {
            return Ok(());
        }

        let interval_minutes = crate::native_settings::bottom_quote_interval_minutes(&settings);
        let Some(quote) = crate::bottom_quotes::selected_quote(
            settings.terminal.bottom_quote_mode,
            interval_minutes,
        ) else {
            return Ok(());
        };
        let quote = quote.display_text();
        if quote.is_empty() {
            return Ok(());
        }
        self.update_next_frame_time(Some(
            Instant::now() + crate::bottom_quotes::next_rotation_delay(interval_minutes),
        ));

        let (padding_left, padding_top) = self.padding_left_top();
        let border = self.get_os_border();
        let tab_bar_height = if self.show_tab_bar {
            self.tab_bar_pixel_height().unwrap_or(0.0)
        } else {
            0.0
        };
        let (top_tab_height, bottom_tab_height) = if self.config.tab_bar_at_bottom {
            (0.0, tab_bar_height)
        } else {
            (tab_bar_height, 0.0)
        };

        let grid_left = padding_left + border.left.get() as f32;
        let grid_top = border.top.get() as f32 + top_tab_height + padding_top;
        let grid_bottom = grid_top + self.terminal_size.pixel_height as f32;
        let content_bottom =
            self.dimensions.pixel_height as f32 - border.bottom.get() as f32 - bottom_tab_height;
        let gutter_height = (content_bottom - grid_bottom).floor();
        if gutter_height < 10.0 {
            return Ok(());
        }

        let quote_font_size = crate::native_settings::bottom_quote_font_size(&settings);
        let quote_font = self
            .fonts
            .command_palette_font_with_size_and_weight(quote_font_size, 500)?;
        let quote_metrics =
            crate::utilsprites::RenderMetrics::with_font_metrics(&quote_font.metrics());
        let quote_height = quote_metrics.cell_size.height as f32;

        let inset = 10.0;
        let max_width = (self.terminal_size.pixel_width as f32 - inset * 2.0).max(0.0);
        if max_width <= 0.0 {
            return Ok(());
        }
        let gl_state = self.render_state.as_ref().unwrap();
        let ctx = DrawContext::new(gl_state, self.dimensions, &quote_metrics);
        let display_text = ctx.text_with_ellipsis(&quote_font, &quote, max_width);
        if display_text.is_empty() {
            return Ok(());
        }
        let text_width = ctx
            .measure_text_width(&quote_font, &display_text)
            .min(max_width);
        let x = (grid_left + self.terminal_size.pixel_width as f32 - inset - text_width).max(0.0);
        let y = (grid_bottom + ((gutter_height - quote_height) / 2.0).max(0.0)).max(0.0);
        // Opaque, and deliberately so. These carried `.mul_alpha(0.46)` and
        // `.mul_alpha(0.38)`, but the glyph shader discarded a vertex alpha
        // until it was fixed to honour one, so the quote has always rendered
        // solid and was tuned by eye against that. Keeping the multiplier now
        // that it works would darken shipped output to settle an intent nobody
        // ever saw. The muting lives in the colour itself.
        let color = match crate::native_settings::effective_appearance() {
            window::Appearance::Light | window::Appearance::LightHighContrast => {
                LinearRgba::with_srgba(80, 80, 90, 255)
            }
            window::Appearance::Dark | window::Appearance::DarkHighContrast => {
                LinearRgba::with_srgba(210, 210, 220, 255)
            }
        };

        ctx.draw_text_on_layer(
            layers,
            2,
            &quote_font,
            x,
            y,
            &display_text,
            color,
            max_width,
        )
        .context("paint_bottom_quote")?;

        Ok(())
    }

    /// Floating label that follows the cursor while a Files-panel row is
    /// being dragged toward the terminal. Painted after everything else so
    /// it stays on top; deliberately registers no UIItem (hit-transparent).
    fn paint_file_drag_ghost(&mut self) -> anyhow::Result<()> {
        let Some(state) = self.right_sidebar_file_drag.as_ref() else {
            return Ok(());
        };
        if !state.active {
            return Ok(());
        }
        let label = state.payload.label();
        let anchor = state.current;
        self.paint_drag_ghost_pill(&label, anchor)
    }

    /// Translucent preview of where a dragged level-2 pane tab would land
    /// (full pane = move into its stack, half pane = split), plus the
    /// floating tab-title pill. Registers no UIItem (hit-transparent).
    /// Reorder drag of a left-sidebar Project/thread row: a 2px accent
    /// insert line at the gap releasing would drop into, plus the floating
    /// ghost pill with the row's title.
    fn paint_sidebar_row_drag_overlay(&mut self) -> anyhow::Result<()> {
        let Some(state) = self.sidebar_row_drag.as_ref() else {
            return Ok(());
        };
        if !state.active {
            return Ok(());
        }
        let label = state.title.clone();
        let anchor = state.current;
        let line_y = state.target.as_ref().map(|target| target.line_y);

        let sidebar_span = self
            .ui_items
            .iter()
            .find(|item| {
                item.item_type == crate::termwindow::UIItemType::WorkspaceSidebarBackground
            })
            .map(|bg| (bg.x as f32, bg.width as f32));

        if let (Some(line_y), Some((bg_x, bg_width))) = (line_y, sidebar_span) {
            // Same explicit accent blue as the pane drop preview: readable
            // in both appearances regardless of the palette's selected_bg.
            let accent = match crate::native_settings::effective_appearance() {
                window::Appearance::Light | window::Appearance::LightHighContrast => {
                    LinearRgba::with_srgba(0, 122, 255, 255)
                }
                window::Appearance::Dark | window::Appearance::DarkHighContrast => {
                    LinearRgba::with_srgba(10, 132, 255, 255)
                }
            };
            let inset = self.ui_px(crate::termwindow::ui::tokens::SIDEBAR_INSET) as f32;
            let thickness = self.ui_f32(2.0).max(2.0);
            let rect = euclid::rect(
                bg_x + inset,
                line_y as f32 - thickness / 2.0,
                (bg_width - inset * 2.0).max(0.0),
                thickness,
            );
            let gl_state = self.render_state.as_ref().unwrap();
            let layer = gl_state
                .layer_for_zindex(0)
                .context("sidebar drag overlay layer")?;
            let mut layers = layer.quad_allocator();
            self.filled_rectangle(&mut layers, 0, rect, accent)
                .context("sidebar drag insert line")?;
        }

        self.paint_drag_ghost_pill(&label, anchor)
    }

    fn paint_pane_tab_drag_overlay(&mut self) -> anyhow::Result<()> {
        let Some(state) = self.pane_tab_drag.as_ref() else {
            // The drag ended (dropped or cancelled): the layout underneath
            // has already changed, so a lingering highlight would point at
            // nothing. Drop the picture rather than fade it.
            self.pane_drop_preview = None;
            return Ok(());
        };
        if !state.active {
            self.pane_drop_preview = None;
            return Ok(());
        }
        let label = state.title.clone();
        let anchor = state.current;
        let target_rect = state.target.as_ref().map(|target| target.rect);

        // Ease the drawn highlight toward the hit-tested target. Driven here
        // and not from the mouse handler because paint_impl clears
        // has_animation on entry, so only a deadline registered during paint
        // survives to schedule the next frame.
        let now = Instant::now();
        let shown = match (self.pane_drop_preview.as_mut(), target_rect) {
            (None, None) => None,
            (None, Some(rect)) => {
                // First target of this drag: fade in where it is.
                let mut anim = crate::termwindow::PaneDropPreviewAnim {
                    rect,
                    alpha: 0.0,
                    last_tick: now,
                };
                let moving = anim.step(target_rect, now);
                self.pane_drop_preview = Some(anim);
                Some((anim.rect, anim.alpha, moving))
            }
            (Some(anim), _) => {
                let moving = anim.step(target_rect, now);
                let shown = (anim.rect, anim.alpha, moving);
                if target_rect.is_none() && !moving {
                    // Faded out completely; forget it so the next target
                    // fades in fresh instead of sliding from a stale spot.
                    self.pane_drop_preview = None;
                }
                Some(shown)
            }
        };

        if let Some((rect, alpha, moving)) = shown {
            if moving {
                self.update_next_frame_time(Some(now + Duration::from_millis(16)));
            }
            // Explicit accent blue: the palette's selected_bg is gray in
            // dark mode, but the drop preview should read as blue in both.
            let accent = match crate::native_settings::effective_appearance() {
                window::Appearance::Light | window::Appearance::LightHighContrast => {
                    LinearRgba::with_srgba(0, 122, 255, 255)
                }
                window::Appearance::Dark | window::Appearance::DarkHighContrast => {
                    LinearRgba::with_srgba(10, 132, 255, 255)
                }
            };
            let fill = accent.mul_alpha(0.28 * alpha);
            let border = accent.mul_alpha(0.8 * alpha);

            let gl_state = self.render_state.as_ref().unwrap();
            let layer = gl_state
                .layer_for_zindex(0)
                .context("pane drag overlay layer")?;
            let mut layers = layer.quad_allocator();

            // Keep the radius on the same integral grid the corner sprites
            // snap to (and clamped the same way), so the ring corners meet
            // the fill's corners exactly even on short strips.
            let radius = self
                .ui_f32(crate::termwindow::ui::tokens::PANE_DROP_PREVIEW_RADIUS)
                .min(rect.size.width / 2.0)
                .min(rect.size.height / 2.0)
                .floor()
                .max(1.0);
            self.fill_rounded_rectangle(&mut layers, 0, rect, fill, radius)
                .context("pane drag overlay fill")?;
            // The translucent fill can't occlude an underlying border rect,
            // so build the outline from edge strips plus quarter-ring
            // corner sprites of the same thickness (radius / 5).
            let b = (radius / 5.0).max(1.0);
            let (x, y) = (rect.origin.x, rect.origin.y);
            let (w, h) = (rect.size.width, rect.size.height);
            let span_w = (w - radius * 2.0).max(0.0);
            let span_h = (h - radius * 2.0).max(0.0);
            for edge in [
                euclid::rect(x + radius, y, span_w, b),
                euclid::rect(x + radius, y + h - b, span_w, b),
                euclid::rect(x, y + radius, b, span_h),
                euclid::rect(x + w - b, y + radius, b, span_h),
            ] {
                self.filled_rectangle(&mut layers, 0, edge, border)
                    .context("pane drag overlay border")?;
            }
            let corner_size = euclid::size2(radius, radius);
            for (cx, cy, poly) in [
                (x, y, super::corners::TOP_LEFT_ROUNDED_CORNER_RING),
                (
                    x + w - radius,
                    y,
                    super::corners::TOP_RIGHT_ROUNDED_CORNER_RING,
                ),
                (
                    x,
                    y + h - radius,
                    super::corners::BOTTOM_LEFT_ROUNDED_CORNER_RING,
                ),
                (
                    x + w - radius,
                    y + h - radius,
                    super::corners::BOTTOM_RIGHT_ROUNDED_CORNER_RING,
                ),
            ] {
                self.poly_quad(
                    &mut layers,
                    0,
                    euclid::point2(cx, cy),
                    poly,
                    0,
                    corner_size,
                    border,
                )
                .context("pane drag overlay corner")?
                .set_grayscale();
            }
        }

        self.paint_drag_ghost_pill(&label, anchor)
    }

    /// Floating label that follows the cursor during a drag. Painted after
    /// everything else so it stays on top; registers no UIItem.
    /// Name tag for an icon-only button the pointer has been resting on.
    ///
    /// Anchored beside the button rather than under the pointer: the pointer
    /// is already sitting on the icon, so a tag placed under it would cover
    /// the very thing it names.
    fn paint_hover_tooltip(&mut self) -> anyhow::Result<()> {
        let Some(hover) = self.hover_tooltip.clone() else {
            return Ok(());
        };
        let remaining = crate::termwindow::TOOLTIP_DELAY.saturating_sub(hover.since.elapsed());
        if !remaining.is_zero() {
            // Ask for the frame that will find the delay elapsed. This has to
            // happen here and not where the pointer arrived: paint_impl clears
            // has_animation on entry, so a deadline registered from the mouse
            // handler is wiped before anything can read it, and the tag would
            // then only appear when some unrelated event forced a repaint.
            self.update_next_frame_time(Some(Instant::now() + remaining));
            return Ok(());
        }
        // Re-check the dynamic conditions at paint time: a rename started
        // from a key assignment (no mouse event) or a row whose label no
        // longer overflows must suppress an already-armed tag.
        if !self.hover_tooltip_allowed(&hover.item.item_type) {
            return Ok(());
        }
        let Some(label) = crate::termwindow::tooltip_label_for(&hover.item.item_type) else {
            return Ok(());
        };

        let settings = crate::native_settings::load_shared();
        let font_size = crate::native_settings::home_font_size(&settings);
        let ui_font = self
            .fonts
            .title_font_with_size(font_size)
            .context("hover tooltip font")?;
        let metrics = crate::utilsprites::RenderMetrics::with_font_metrics(&ui_font.metrics());
        let line_height = metrics.cell_size.height as f32;

        // From the shared chrome palette, not a hand-picked grey: the tag sits
        // directly against the sidebar it overhangs, so anything lighter than
        // the sidebar's own controls reads as a foreign surface.
        let chrome = self.chrome();

        let gl_state = self.render_state.as_ref().unwrap();
        let layer = gl_state
            .layer_for_zindex(crate::termwindow::TOOLTIP_ZINDEX)
            .context("hover tooltip layer")?;
        let mut layers = layer.quad_allocator();

        let ctx = DrawContext::new(gl_state, self.dimensions, &metrics);
        let max_width = (self.dimensions.pixel_width as f32 * 0.4).max(80.0);
        let display_text = ctx.text_with_ellipsis(&ui_font, &label, max_width);
        let text_width = ctx
            .measure_text_width(&ui_font, &display_text)
            .min(max_width);

        let pad_x = self.ui_f32(8.0);
        let pad_y = self.ui_f32(4.0);
        let gap = self.ui_f32(8.0);
        let tip_w = text_width + pad_x * 2.0;
        let tip_h = line_height + pad_y * 2.0;

        let (bx, by, bw, bh) = (
            hover.item.x as f32,
            hover.item.y as f32,
            hover.item.width as f32,
            hover.item.height as f32,
        );
        let window_w = self.dimensions.pixel_width as f32;
        let window_h = self.dimensions.pixel_height as f32;

        // Above the button. Beside it would sit on top of the neighbouring
        // buttons in the same row — these are laid out horizontally, so the
        // only free direction is up. Flip below when there is no room above.
        // Full-width list rows left-align their tag; icon buttons centre it.
        let left_align = crate::termwindow::tooltip_left_aligns(&hover.item.item_type);
        let x = crate::termwindow::tooltip_anchor_x(bx, bw, tip_w, window_w, left_align);
        let y = if by - gap - tip_h >= 0.0 {
            by - gap - tip_h
        } else {
            (by + bh + gap).min((window_h - tip_h).max(0.0))
        };

        self.fill_rounded_rectangle_with_border(
            &mut layers,
            0,
            euclid::rect(x, y, tip_w, tip_h),
            chrome.control_bg,
            chrome.control_border,
            self.ui_f32(6.0),
            1.0,
        )
        .context("hover tooltip background")?;
        ctx.draw_text_on_layer(
            &mut layers,
            2,
            &ui_font,
            x + pad_x,
            y + pad_y,
            &display_text,
            chrome.text,
            max_width,
        )
        .context("hover tooltip label")?;

        Ok(())
    }

    fn paint_drag_ghost_pill(
        &mut self,
        label: &str,
        anchor: ::window::Point,
    ) -> anyhow::Result<()> {
        if label.is_empty() {
            return Ok(());
        }
        let settings = crate::native_settings::load_shared();
        let font_size = crate::native_settings::home_font_size(&settings);
        let ui_font = self
            .fonts
            .title_font_with_size(font_size)
            .context("drag ghost font")?;
        let metrics = crate::utilsprites::RenderMetrics::with_font_metrics(&ui_font.metrics());
        let line_height = metrics.cell_size.height as f32;

        let (bg, fg, pill_border) = match crate::native_settings::effective_appearance() {
            window::Appearance::Light | window::Appearance::LightHighContrast => (
                LinearRgba::with_srgba(245, 245, 248, 235),
                LinearRgba::with_srgba(40, 40, 48, 255),
                LinearRgba::with_srgba(60, 60, 67, 70),
            ),
            window::Appearance::Dark | window::Appearance::DarkHighContrast => (
                LinearRgba::with_srgba(58, 58, 66, 235),
                LinearRgba::with_srgba(235, 235, 240, 255),
                LinearRgba::with_srgba(255, 255, 255, 60),
            ),
        };

        let gl_state = self.render_state.as_ref().unwrap();
        let layer = gl_state.layer_for_zindex(0).context("drag ghost layer")?;
        let mut layers = layer.quad_allocator();

        let ctx = DrawContext::new(gl_state, self.dimensions, &metrics);
        let max_width = (self.dimensions.pixel_width as f32 * 0.4).max(80.0);
        let display_text = ctx.text_with_ellipsis(&ui_font, &label, max_width);
        let text_width = ctx
            .measure_text_width(&ui_font, &display_text)
            .min(max_width);

        let pad_x = 8.0;
        let pad_y = 4.0;
        let pill_w = text_width + pad_x * 2.0;
        let pill_h = line_height + pad_y * 2.0;
        let x = (anchor.x as f32 + 12.0)
            .min(self.dimensions.pixel_width as f32 - pill_w)
            .max(0.0);
        let y = (anchor.y as f32 + 12.0)
            .min(self.dimensions.pixel_height as f32 - pill_h)
            .max(0.0);

        self.fill_rounded_rectangle_with_border(
            &mut layers,
            0,
            euclid::rect(x, y, pill_w, pill_h),
            bg,
            pill_border,
            pill_h / 2.0,
            1.0,
        )
        .context("drag ghost background")?;
        ctx.draw_text_on_layer(
            &mut layers,
            2,
            &ui_font,
            x + pad_x,
            y + pad_y,
            &display_text,
            fg,
            max_width,
        )
        .context("drag ghost label")?;

        Ok(())
    }

    /// Step the hover-reveal machine from the frame loop, so a motionless
    /// pointer still reveals at dwell expiry and a departed one still
    /// retreats at grace expiry.
    fn advance_workspace_sidebar_hover(&mut self, now: Instant) {
        let input = self.workspace_sidebar_hover_input();
        match self.workspace_sidebar_hover.step(input, now) {
            crate::termwindow::sidebar_hover::HoverFrame::None => {}
            // Unnamed interval, as with the Space swipe below: the backend
            // paces repaints to min(max_fps, this display's rate).
            crate::termwindow::sidebar_hover::HoverFrame::Now => {
                if let Some(window) = self.window.as_ref() {
                    window.invalidate();
                }
            }
            // One wakeup at the deadline, as the sidebar scrollbar does.
            crate::termwindow::sidebar_hover::HoverFrame::At(due) => {
                self.update_next_frame_time(Some(due));
            }
        }
    }

    /// A soft shadow falling off the hover-revealed panel's right edge, so
    /// the overlay reads as a layer floating above the terminal rather than
    /// a slab butted against it. Only the hover overlay gets this: the
    /// docked sidebar sits beside the content, not on top of it.
    ///
    /// Painted into the same recording as the panel, so it travels with the
    /// reveal/retreat animation. A run of thin strips with quadratically
    /// decaying alpha stands in for a gradient.
    fn paint_workspace_sidebar_hover_shadow(
        &mut self,
        layers: &mut TripleLayerQuadAllocator,
    ) -> anyhow::Result<()> {
        let Some(rect) = self.workspace_sidebar_rect() else {
            return Ok(());
        };
        let edge = rect.x.saturating_add(rect.width) as f32;
        let top = rect.y as f32;
        let height = rect.height as f32;
        let spread = self.ui_f32(16.0);
        const STEPS: usize = 8;
        const BASE_ALPHA: f32 = 0.22;
        let step_width = spread / STEPS as f32;
        for i in 0..STEPS {
            let t = (i as f32 + 0.5) / STEPS as f32;
            let alpha = BASE_ALPHA * (1.0 - t) * (1.0 - t);
            self.filled_rectangle(
                layers,
                0,
                euclid::rect(edge + i as f32 * step_width, top, step_width, height),
                LinearRgba::with_components(0.0, 0.0, 0.0, alpha),
            )
            .context("sidebar hover shadow strip")?;
        }
        Ok(())
    }

    /// Record the hover-revealed sidebar off-screen, holding back its hit
    /// targets until after the tab bar has laid out its own.
    ///
    /// Returns the recording, how far the panel still has to travel
    /// (0 = arrived) and the items to register once it is safe to.
    fn record_workspace_sidebar_hover_overlay(
        &mut self,
        now: Instant,
    ) -> anyhow::Result<Option<(HeapQuadAllocator, f32, Vec<UIItem>)>> {
        if !self.workspace_sidebar_collapsed {
            return Ok(None);
        }
        let Some(progress) = self.workspace_sidebar_hover.progress(now) else {
            return Ok(None);
        };
        let ui_items_before = self.ui_items.len();
        let mut sidebar_frame = HeapQuadAllocator::default();
        let result = {
            let mut sidebar_layers = TripleLayerQuadAllocator::Heap(&mut sidebar_frame);
            self.paint_workspace_sidebar(&mut sidebar_layers)
                .and_then(|_| self.paint_workspace_sidebar_hover_shadow(&mut sidebar_layers))
        };
        // The marks describe `sidebar_frame`, which the swipe composite below
        // may still split; nobody downstream of this fn may.
        let live_list = self.workspace_sidebar_list_quads;
        self.workspace_sidebar_list_quads = None;
        result.context("record hover-revealed workspace sidebar")?;
        let mut items: Vec<UIItem> = self.ui_items.drain(ui_items_before..).collect();
        if !self.workspace_sidebar_hover.is_fully_presented(now) {
            // While the panel is moving, no target inside it is where the
            // user thinks it is. One inert item (its handler only sets the
            // arrow cursor) keeps the revealed strip from handing clicks to
            // the terminal underneath, and keeps the row-drag target lookup's
            // x-extent truthful.
            items.clear();
            if let Some(rect) = self.workspace_sidebar_rect() {
                let revealed = (rect.width as f32 * progress).round() as usize;
                if revealed > 0 {
                    items.push(UIItem {
                        x: rect.x,
                        y: 0,
                        width: revealed,
                        height: rect.y + rect.height,
                        item_type: UIItemType::WorkspaceSidebarBackground,
                    });
                }
            }
        }
        let quads = self
            .composite_space_swipe_into_hover_overlay(sidebar_frame, live_list, now)
            .context("composite space swipe into hover overlay")?;
        Ok(Some((quads, 1.0 - progress, items)))
    }

    /// While a Space swipe runs on the hover-revealed panel, its page slide
    /// has to happen inside the recording: the overlay replaces the live
    /// sidebar paint entirely, so this mirrors the docked compositing branch
    /// of `paint_pass`, writing into a heap instead of the GPU stream. When
    /// no swipe is in flight the recording passes through untouched.
    fn composite_space_swipe_into_hover_overlay(
        &mut self,
        sidebar_frame: HeapQuadAllocator,
        live_list: Option<(HeapQuadMark, HeapQuadMark)>,
        now: Instant,
    ) -> anyhow::Result<HeapQuadAllocator> {
        let render_space_push = self.workspace_space_swipe_push_active
            && self.workspace_space_swipe_source_frame.is_some();
        let render_space_track = !render_space_push
            && !self.workspace_space_swipe_push_active
            && self.workspace_space_swipe_target_frame.is_some();
        let capture_space_source =
            self.workspace_space_swipe_capture_source && !render_space_push && !render_space_track;

        if capture_space_source {
            // MayBegin captured the panel at rest: keep a copy as the
            // outgoing page and present the original untouched.
            self.workspace_space_swipe_source_frame = Some(crate::termwindow::CapturedSidebar {
                quads: sidebar_frame.clone(),
                list: live_list,
            });
            self.workspace_space_swipe_capture_source = false;
            #[cfg(target_os = "macos")]
            if self.workspace_space_swipe_pending_commit.is_some() {
                if let Some(window) = self.window.clone() {
                    window.notify(TermWindowNotif::Apply(Box::new(|term_window| {
                        term_window.complete_workspace_space_swipe_switch();
                    })));
                }
            }
            return Ok(sidebar_frame);
        }
        if !(render_space_push || render_space_track) {
            return Ok(sidebar_frame);
        }

        if self.workspace_space_swipe_needs_settle_start {
            let gesture_extent = self.workspace_sidebar_presented_width() as f32;
            let opening = if self.workspace_space_swipe_tracked {
                crate::termwindow::space_swipe::SettleOpening::WhereTheFingerLeftIt
            } else {
                crate::termwindow::space_swipe::SettleOpening::AtRest
            };
            self.workspace_sidebar_swipe.resolve_switch(
                true,
                Instant::now(),
                gesture_extent,
                opening,
            );
            self.workspace_space_swipe_needs_settle_start = false;
            if let Some(window) = self.window.as_ref() {
                window.invalidate();
            }
        }

        let mut composite = HeapQuadAllocator::default();
        let mut composited_tracking_frame = false;
        {
            let mut composite_layers = TripleLayerQuadAllocator::Heap(&mut composite);
            if let Some(rect) = self.workspace_sidebar_rect() {
                // Same rebase and page arithmetic as the docked branch; see
                // the comments there.
                let sidebar_clip = crate::quad::QuadClipRect::from_top_left_pixels(
                    rect.x as f32,
                    rect.y as f32,
                    rect.x.saturating_add(rect.width) as f32,
                    rect.y.saturating_add(rect.height) as f32,
                    &self.dimensions,
                );
                let page_right = (sidebar_clip.right() - 1.0).max(sidebar_clip.left());
                let page_width = page_right - sidebar_clip.left();
                let page_clip = sidebar_clip.with_horizontal(sidebar_clip.left(), page_right);
                let offsets = self
                    .workspace_space_swipe_push_offsets(now, page_width)
                    .filter(|_| page_width > 0.0);
                let offscreen = if render_space_push {
                    self.workspace_space_swipe_source_frame.as_ref()
                } else {
                    self.workspace_space_swipe_target_frame
                        .as_ref()
                        .map(|(_, captured)| captured)
                };
                let offscreen_span = offscreen.and_then(|captured| captured.list);

                match (offsets, live_list, offscreen_span) {
                    (Some((source_offset, target_offset)), Some(live), Some(other)) => {
                        let (live_offset, offscreen_offset) = if render_space_push {
                            (target_offset, source_offset)
                        } else {
                            (source_offset, target_offset)
                        };
                        sidebar_frame
                            .apply_before(&mut composite_layers, &live.0)
                            .context("hover swipe sidebar chrome above the list")?;
                        if let Some(captured) = offscreen {
                            captured
                                .quads
                                .apply_between(
                                    &mut composite_layers,
                                    &other.0,
                                    &other.1,
                                    offscreen_offset,
                                    page_clip,
                                )
                                .context("hover swipe offscreen sidebar page")?;
                        }
                        sidebar_frame
                            .apply_between(
                                &mut composite_layers,
                                &live.0,
                                &live.1,
                                live_offset,
                                page_clip,
                            )
                            .context("hover swipe live sidebar page")?;
                        sidebar_frame
                            .apply_after(&mut composite_layers, &live.1)
                            .context("hover swipe sidebar chrome below the list")?;
                        composited_tracking_frame = render_space_track;
                    }
                    _ => {
                        sidebar_frame
                            .apply_to(&mut composite_layers)
                            .context("hover swipe sidebar fallback")?;
                    }
                }
            } else {
                sidebar_frame
                    .apply_to(&mut composite_layers)
                    .context("hover swipe sidebar without viewport")?;
            }
        }
        self.workspace_space_swipe_tracked |= composited_tracking_frame;

        // The finger lifted on a committing gesture while the pages were
        // already tracking it: keep this paint as the outgoing page (see the
        // docked branch for the full story).
        if render_space_track && self.workspace_space_swipe_capture_source {
            self.workspace_space_swipe_source_frame = Some(crate::termwindow::CapturedSidebar {
                quads: sidebar_frame,
                list: live_list,
            });
            self.workspace_space_swipe_capture_source = false;
            #[cfg(target_os = "macos")]
            if self.workspace_space_swipe_pending_commit.is_some() {
                if let Some(window) = self.window.clone() {
                    window.notify(TermWindowNotif::Apply(Box::new(|term_window| {
                        term_window.complete_workspace_space_swipe_switch();
                    })));
                }
            }
        }
        Ok(composite)
    }

    /// Slide the recorded panel in from the left edge, above everything
    /// already drawn.
    ///
    /// Flattened into the last sub-layer rather than replayed layer for
    /// layer: see `HeapQuadAllocator::apply_to_single_layer`. Anything
    /// painted after this -- window borders, the modal, the context menu,
    /// the drag overlays -- still lands on top, which is why this needs no
    /// z-index of its own.
    fn paint_workspace_sidebar_hover_overlay(
        &self,
        overlay: &HeapQuadAllocator,
        hidden: f32,
    ) -> anyhow::Result<()> {
        let Some(rect) = self.workspace_sidebar_rect() else {
            return Ok(());
        };
        let gl_state = self.render_state.as_ref().unwrap();
        let layer = gl_state
            .layer_for_zindex(0)
            .context("hover sidebar layer")?;
        let mut layers = layer.quad_allocator();
        let offset_x = -(hidden * rect.width as f32);
        // Containment only: the panel travels left and the window crops the
        // overhang.
        let clip = crate::quad::QuadClipRect::from_top_left_pixels(
            0.0,
            0.0,
            self.dimensions.pixel_width as f32,
            self.dimensions.pixel_height as f32,
            &self.dimensions,
        );
        overlay.apply_to_single_layer(&mut layers, 2, offset_x, clip)
    }

    fn advance_workspace_space_swipe_push(&mut self, now: Instant) {
        if self.workspace_sidebar_swipe.advance(now) {
            // Ask for another frame without naming an interval. The native
            // backend already throttles repaints to
            // min(config.max_fps, this display's refresh rate), so letting it
            // set the pace gives 120Hz on a ProMotion panel, 60Hz on a
            // 60Hz one, and follows the panel when it varies -- whereas the
            // fixed 16ms this replaces pinned every machine to ~60fps. The
            // transition is driven by elapsed time, not by a frame count, so
            // a slower display simply draws fewer frames over the same 220ms.
            //
            // `invalidate` is part of `WindowOps`, so the Windows and Linux
            // gesture backends can reuse this path with their own pacing.
            if let Some(window) = self.window.as_ref() {
                window.invalidate();
            }
        }

        if !self.workspace_sidebar_swipe.is_active() {
            // The gesture is over, whether it committed, rebounded or never
            // locked an axis. Retire both captures: holding either one would
            // keep the compositor splitting a sidebar that is no longer
            // transitioning.
            self.workspace_space_swipe_push_active = false;
            self.workspace_space_swipe_source_frame = None;
            self.workspace_space_swipe_target_frame = None;
            self.workspace_space_swipe_direction = 0.0;
            self.workspace_space_swipe_tracked = false;
        }
    }

    /// Where the outgoing and incoming list pages sit this frame, as
    /// `(source, target)`. Live throughout the gesture, not just after the
    /// commit: the pages follow the finger, so an offset exists as soon as the
    /// axis locks horizontal.
    fn workspace_space_swipe_push_offsets(
        &self,
        now: Instant,
        page_width: f32,
    ) -> Option<(f32, f32)> {
        let gesture_extent = self.workspace_sidebar_presented_width() as f32;
        let visual = self.workspace_sidebar_swipe.visual(now, gesture_extent)?;
        // Before the commit the direction is whichever way the finger has
        // travelled; after it, the committed direction is authoritative,
        // because the settle animates the offset back through zero and its
        // sign would otherwise flip mid-transition.
        let direction = if self.workspace_space_swipe_push_active {
            self.workspace_space_swipe_direction
        } else {
            visual.offset.signum()
        };
        Some(super::super::space_swipe::sidebar_page_push_offsets(
            visual.offset,
            gesture_extent,
            page_width,
            direction,
        ))
    }

    /// Record the neighbouring Space's sidebar so it can slide in beside the
    /// live one while the finger is still down.
    ///
    /// Cheap to call every frame: it repaints only when the neighbour changes,
    /// which is once when the axis locks and once more if the drag reverses.
    fn capture_workspace_space_swipe_target(&mut self, now: Instant) -> anyhow::Result<()> {
        if self.workspace_space_swipe_push_active {
            // The switch already happened, so the live sidebar *is* the
            // destination and the outgoing one is held in the source frame.
            return Ok(());
        }
        let gesture_extent = self.workspace_sidebar_presented_width() as f32;
        let target = self
            .workspace_sidebar_swipe
            .visual(now, gesture_extent)
            .and_then(|visual| visual.target_space_id);
        let Some(target) = target else {
            // Either no gesture, or one rubber-banding against the end of the
            // list with no neighbour to show.
            self.workspace_space_swipe_target_frame = None;
            return Ok(());
        };
        if self
            .workspace_space_swipe_target_frame
            .as_ref()
            .is_some_and(|(captured, _)| *captured == target)
        {
            return Ok(());
        }

        let ui_items_len = self.ui_items.len();
        let mut quads = HeapQuadAllocator::default();
        self.workspace_sidebar_preview_space_id = Some(target.clone());
        // Draw the neighbour where adopting it would actually put it: at the
        // offset it was left scrolled to. Using the *outgoing* Space's offset
        // instead slides in a view that does not exist -- blank, when the
        // neighbour has fewer threads than the offset scrolls past -- and then
        // jumps once the switch applies the real one.
        //
        // Restoring afterwards is not tidiness. `paint_workspace_sidebar`
        // clamps this field against the painted Space's own scroll extent and
        // writes it back, so a shorter neighbour would drag the live sidebar
        // up under the finger the instant the axis locked.
        let preview_scroll_offset = self
            .workspace_sidebar_scroll_offsets
            .get(&target)
            .copied()
            .unwrap_or(0.0);
        let live_scroll_offset = std::mem::replace(
            &mut self.workspace_sidebar_scroll_offset,
            preview_scroll_offset,
        );
        let mut layers = TripleLayerQuadAllocator::Heap(&mut quads);
        let painted = self.paint_workspace_sidebar(&mut layers);
        drop(layers);
        self.workspace_sidebar_scroll_offset = live_scroll_offset;
        self.workspace_sidebar_preview_space_id = None;
        // This paint laid out hit targets for a Space the window has not
        // adopted, at positions the pointer will never see, and it ran before
        // the live paint that owns those slots. Drop them.
        self.ui_items.truncate(ui_items_len);
        painted.context("capture neighbouring Space sidebar")?;

        let list = self.workspace_sidebar_list_quads;
        self.workspace_space_swipe_target_frame =
            Some((target, crate::termwindow::CapturedSidebar { quads, list }));
        Ok(())
    }

    pub fn paint_pass(&mut self) -> anyhow::Result<()> {
        let frame_now = Instant::now();
        self.advance_workspace_space_swipe_push(frame_now);
        self.advance_workspace_sidebar_hover(frame_now);
        self.advance_content_view_fade(frame_now);
        // Card texture work is queued per pass; a retried pass re-queues it.
        self.pending_card_renders.borrow_mut().clear();
        self.card_composites.borrow_mut().clear();
        // Truncate, not drop: the buffer's capacity is the whole point.
        self.card_frame_verts.borrow_mut().clear();
        self.image_composites.borrow_mut().clear();
        {
            let gl_state = self.render_state.as_ref().unwrap();
            for layer in gl_state.layers.borrow().iter() {
                layer.clear_quad_allocation();
            }
        }

        // Clear out UI item positions; we'll rebuild these as we render
        self.ui_items.clear();

        // The right sidebar is part of the local window geometry. A deferred
        // content-view resize or an asynchronous remote resync can leave the
        // active mux tab at its old full width; heal that before any pane
        // positions, hit targets or quads are derived from it.
        self.reconcile_active_mux_tab_size_before_paint();
        self.sync_pane_font_sizes();
        let panes = self.get_panes_to_render();
        let focused = self.focused.is_some();
        let window_is_transparent =
            !self.window_background.is_empty() || self.config.window_background_opacity != 1.0;

        let start = Instant::now();
        let gl_state = self.render_state.as_ref().unwrap();
        let layer = gl_state
            .layer_for_zindex(0)
            .context("layer_for_zindex(0)")?;
        // The first frame of a transition records the terminal instead of
        // drawing it, and every frame after replays that recording at the size
        // its travel has reached. Redirecting the allocator here catches the
        // whole world -- panes, splits, sidebars, tab bar -- without each of
        // them having to know a transition is running.
        let recording_flight = self
            .content_view_fade
            .as_ref()
            .is_some_and(|fade| fade.flight.is_none());
        let mut flight_capture = HeapQuadAllocator::default();
        let mut layers = if recording_flight {
            TripleLayerQuadAllocator::Heap(&mut flight_capture)
        } else {
            layer.quad_allocator()
        };
        log::trace!("quad map elapsed {:?}", start.elapsed());
        metrics::histogram!("quad.map").record(start.elapsed());

        let mut paint_terminal_background = false;

        // Render the full window background
        match (self.window_background.is_empty(), self.allow_images) {
            (false, AllowImage::Yes | AllowImage::Scale(_)) => {
                let bg_color = self.palette().background.to_linear();

                let top = panes
                    .iter()
                    .find(|p| p.is_active)
                    .map(|p| match self.get_viewport(p.pane.pane_id()) {
                        Some(top) => top,
                        None => p.pane.get_dimensions().physical_top,
                    })
                    .unwrap_or(0);

                let loaded_any = self
                    .render_backgrounds(bg_color, top)
                    .context("render_backgrounds")?;

                if !loaded_any {
                    // Either there was a problem loading the background(s)
                    // or they haven't finished loading yet.
                    // Use the regular terminal background until that changes.
                    paint_terminal_background = true;
                }
            }
            _ if window_is_transparent => {
                // Avoid doubling up the background color: the panes
                // will render out through the padding so there
                // should be no gaps that need filling in
            }
            _ => {
                paint_terminal_background = true;
            }
        }

        if paint_terminal_background {
            // Regular window background color
            let background = if matches!(
                crate::native_settings::effective_appearance(),
                window::Appearance::Dark | window::Appearance::DarkHighContrast
            ) {
                self.chrome()
                    .sidebar_bg
                    .mul_alpha(self.config.window_background_opacity)
            } else if panes.len() == 1 {
                // If we're the only pane, use the pane's palette
                // to draw the padding background
                panes[0]
                    .pane
                    .palette()
                    .background
                    .to_linear()
                    .mul_alpha(self.config.window_background_opacity)
            } else {
                self.palette()
                    .background
                    .to_linear()
                    .mul_alpha(self.config.window_background_opacity)
            };

            self.filled_rectangle(
                &mut layers,
                0,
                euclid::rect(
                    0.,
                    0.,
                    self.dimensions.pixel_width as f32,
                    self.dimensions.pixel_height as f32,
                ),
                background,
            )
            .context("filled_rectangle for window background")?;
        }

        let border = self.get_os_border();
        let header_height = border.top.get() as f32;
        if header_height > 0.0 {
            let chrome = self.chrome();
            self.filled_rectangle(
                &mut layers,
                0,
                euclid::rect(0.0, 0.0, self.dimensions.pixel_width as f32, header_height),
                chrome.sidebar_bg,
            )
            .context("filled_rectangle for chrome header background")?;
        }

        // When a content view is the foreground it takes over the content area,
        // so skip painting the terminal panes / splits -- except while one is
        // arriving or leaving, when both have to be on screen at once for the
        // view to have anything to fade against.
        let content_view_active = self.content_view_foreground();
        // A transition puts both worlds on screen: the terminal is painted as
        // usual and the view is composited over it at a partial opacity. A
        // closing view is already gone by now, so its side of the transition
        // is a recorded frame rather than a live paint.
        let fading_content_view = self.content_view_fade.is_some();
        // While a transition runs the terminal is a recording: drawn once into
        // `flight_capture` on the opening frame, replayed thereafter.
        //
        // `content_view_active` is false for the whole of a *closing*
        // transition -- the view is removed from `content_views` before the
        // fade is started, so there is no longer an active one to find. Left to
        // `!content_view_active` alone this put the live terminal on screen at
        // full size from the transition's second frame, underneath the
        // recording that was still growing back out of the card. Two terminals,
        // two scales, and anything the terminal world draws once per frame --
        // the bottom quote most visibly, at 38% alpha over itself -- drawn
        // twice. It also made the return look like a dissolve rather than a
        // move, because the picture being travelled towards was already there.
        let paint_terminal_world =
            (!content_view_active && !fading_content_view) || recording_flight;

        if !content_view_active {
            self.advance_remote_open();
            // Poll and hydrate the post-resize screen before sampling the
            // gate. Explicit takeovers may show the grid during this wait;
            // connection recovery still uses the opaque surface.
            self.advance_frontend_geometry_confirmation();
        }

        let frontend_blocked = !content_view_active && self.frontend_surface_blocked();

        self.track_pane_output_generations_for_frame = !recording_flight
            && mux::Mux::get()
                .get_active_tab_for_window(self.mux_window_id)
                .is_some_and(|tab| self.can_track_presented_terminal_output(&tab));

        // Everything the terminal registers during a transition sits under a
        // view that is on its way in or out. Leaving those targets live would
        // let a click land on a pane the user is looking at through a
        // half-drawn overview.
        let ui_items_before_terminal = self.ui_items.len();

        if paint_terminal_world && !frontend_blocked {
            for pos in panes {
                if pos.is_active {
                    if focused {
                        pos.pane.advise_focus();
                        mux::Mux::get().record_focus_for_current_identity(pos.pane.pane_id());
                    }
                }
                self.paint_pane(&pos, &mut layers).context("paint_pane")?;
            }

            if let Some(pane) = self.get_active_pane_or_overlay() {
                let splits = self.get_splits();
                for split in &splits {
                    self.paint_split(&mut layers, split, &pane)
                        .context("paint_split")?;
                }
            }
            self.paint_frontend_shared_unused_grid(&mut layers)
                .context("paint shared unused grid")?;
            self.paint_frontend_takeover_badge(&mut layers)
                .context("paint takeover badge")?;
        }

        if paint_terminal_world && !frontend_blocked {
            self.paint_bottom_quote(&mut layers)
                .context("paint_bottom_quote")?;
        }

        if frontend_blocked {
            self.paint_frontend_handoff_overlay(&mut layers)
                .context("paint frontend handoff overlay")?;
        }

        if fading_content_view {
            self.ui_items.truncate(ui_items_before_terminal);
        }

        if content_view_active {
            self.paint_content_view_composited(&mut layers)
                .context("paint_content_view")?;
        } else if fading_content_view {
            self.paint_departing_content_view()
                .context("paint departing content view")?;
        }
        // Only the arriving view answers to the pointer while a transition
        // runs. The chrome painted below belongs to the terminal, which is on
        // screen but on its way behind something, and a click landing there
        // would go somewhere the user is no longer looking.
        let ui_items_after_view = self.ui_items.len();

        // A full-window ContentView owns all ThinkTerm chrome below the native
        // title bar. This is presentation-only: sidebar widths/collapse state
        // and terminal geometry stay unchanged behind the view.
        //
        // The window frame stays where it is while the terminal inside it
        // travels, so a transition keeps painting it. Only the terminal grid
        // flies, because the card it is flying into shows a terminal and
        // nothing else -- carrying the sidebar along made the picture that
        // landed and the picture already in the card visibly different things.
        if self.content_view_is_full_window() && !fading_content_view {
            // Recorded into the view's own surface, so the two arrive and
            // leave together.
            drop(layers);
        } else if fading_content_view {
            drop(layers);
            if recording_flight {
                self.record_content_view_chrome()
                    .context("record window frame")?;
            }
        } else {
            // Space switching is a left-sidebar interaction. Keep the terminal,
            // tab bar, right sidebar and window chrome on the live GPU path, then
            // isolate just the left sidebar while its middle list page transitions.
            drop(layers);

            self.capture_workspace_space_swipe_target(frame_now)?;

            // A hover reveal replaces the normal sidebar paint entirely: the
            // panel is recorded off-screen here and composited after the tab
            // bar, so both its pixels and its hit targets land above the
            // chrome that would otherwise cover its top edge.
            let hover_overlay = self.record_workspace_sidebar_hover_overlay(frame_now)?;

            let render_space_push = self.workspace_space_swipe_push_active
                && self.workspace_space_swipe_source_frame.is_some();
            // Before the commit the window still shows the Space being left, so
            // the live paint is the *source* page and the captured neighbour is
            // the target. Committing swaps those roles.
            let render_space_track = !render_space_push
                && !self.workspace_space_swipe_push_active
                && self.workspace_space_swipe_target_frame.is_some();
            // Only needed when a gesture committed before the pages ever tracked
            // it -- a flick fast enough to finish inside one frame. Otherwise the
            // tracking branch below hands over its own last paint.
            let capture_space_source = self.workspace_space_swipe_capture_source
                && !render_space_push
                && !render_space_track;
            let mut sidebar_frame = HeapQuadAllocator::default();
            let mut composited_tracking_frame = false;

            if hover_overlay.is_some() {
                // The overlay recording above already painted the sidebar;
                // painting it again here would put a second copy underneath
                // the terminal-glyph sub-layer.
            } else if render_space_push || render_space_track {
                let mut sidebar_layers = TripleLayerQuadAllocator::Heap(&mut sidebar_frame);
                self.paint_workspace_sidebar(&mut sidebar_layers)
                    .context("paint live workspace sidebar")?;
                drop(sidebar_layers);
                let live_list = self.workspace_sidebar_list_quads;

                if self.workspace_space_swipe_needs_settle_start {
                    let gesture_extent = self.workspace_sidebar_presented_width() as f32;
                    let opening = if self.workspace_space_swipe_tracked {
                        crate::termwindow::space_swipe::SettleOpening::WhereTheFingerLeftIt
                    } else {
                        crate::termwindow::space_swipe::SettleOpening::AtRest
                    };
                    self.workspace_sidebar_swipe.resolve_switch(
                        true,
                        Instant::now(),
                        gesture_extent,
                        opening,
                    );
                    self.workspace_space_swipe_needs_settle_start = false;
                    // The settle clock does not start until the *next* frame (see
                    // `Settle::started`), so this frame only has to make sure a
                    // next frame happens; `advance` paces everything after it.
                    if let Some(window) = self.window.as_ref() {
                        window.invalidate();
                    }
                }

                let mut gpu_layers = layer.quad_allocator();
                if let Some(rect) = self.workspace_sidebar_rect() {
                    // Quad positions are window-centre relative (see
                    // `filled_rectangle`) while the sidebar rect is in top-left
                    // pixels. Rebase, or every quad fails the bounds test and the
                    // sidebar renders empty for the whole transition.
                    //
                    // This is a containment bound, nothing more: it keeps a page
                    // that has slid partway out of the sidebar from spilling over
                    // the terminal. The pages may use the sidebar's full height --
                    // the masks painted after the list already hide whatever
                    // overshoots the viewport, and they do it without cutting a row
                    // in half the way a clip edge through the middle of the list
                    // would.
                    let sidebar_clip = crate::quad::QuadClipRect::from_top_left_pixels(
                        rect.x as f32,
                        rect.y as f32,
                        rect.x.saturating_add(rect.width) as f32,
                        rect.y.saturating_add(rect.height) as f32,
                        &self.dimensions,
                    );
                    // The one-pixel separator is sidebar chrome, not page content.
                    let page_right = (sidebar_clip.right() - 1.0).max(sidebar_clip.left());
                    let page_width = page_right - sidebar_clip.left();
                    let page_clip = sidebar_clip.with_horizontal(sidebar_clip.left(), page_right);
                    let offsets = self
                        .workspace_space_swipe_push_offsets(Instant::now(), page_width)
                        .filter(|_| page_width > 0.0);
                    // Which Space the live paint holds flips at the commit, so the
                    // offset that belongs to it flips with it. The captured
                    // neighbour always takes the other one.
                    let offscreen = if render_space_push {
                        self.workspace_space_swipe_source_frame.as_ref()
                    } else {
                        self.workspace_space_swipe_target_frame
                            .as_ref()
                            .map(|(_, captured)| captured)
                    };
                    let offscreen_span = offscreen.and_then(|captured| captured.list);

                    match (offsets, live_list, offscreen_span) {
                        (Some((source_offset, target_offset)), Some(live), Some(other)) => {
                            let (live_offset, offscreen_offset) = if render_space_push {
                                (target_offset, source_offset)
                            } else {
                                (source_offset, target_offset)
                            };
                            // Order matters within a layer: the chrome recorded
                            // after the list is what masks it, so it has to be
                            // replayed after the pages here too.
                            sidebar_frame
                                .apply_before(&mut gpu_layers, &live.0)
                                .context("space swipe sidebar chrome above the list")?;
                            if let Some(captured) = offscreen {
                                captured
                                    .quads
                                    .apply_between(
                                        &mut gpu_layers,
                                        &other.0,
                                        &other.1,
                                        offscreen_offset,
                                        page_clip,
                                    )
                                    .context("space swipe offscreen sidebar page")?;
                            }
                            sidebar_frame
                                .apply_between(
                                    &mut gpu_layers,
                                    &live.0,
                                    &live.1,
                                    live_offset,
                                    page_clip,
                                )
                                .context("space swipe live sidebar page")?;
                            sidebar_frame
                                .apply_after(&mut gpu_layers, &live.1)
                                .context("space swipe sidebar chrome below the list")?;
                            composited_tracking_frame = render_space_track;
                        }
                        _ => {
                            sidebar_frame
                                .apply_to(&mut gpu_layers)
                                .context("space swipe sidebar fallback")?;
                        }
                    }
                } else {
                    sidebar_frame
                        .apply_to(&mut gpu_layers)
                        .context("space swipe target sidebar without viewport")?;
                }
                drop(gpu_layers);
                // Unconditional: the pages either followed the finger this frame
                // or they did not, and that is true regardless of which branch
                // captured what. Gating this on the source capture meant the usual
                // path -- source captured back at `MayBegin`, long before the axis
                // locked -- never recorded a single tracking frame, so committing
                // opened `AtRest` and yanked the pages back to zero first.
                self.workspace_space_swipe_tracked |= composited_tracking_frame;

                // The finger lifted on a committing gesture while the pages were
                // already tracking it. This paint is the last frame of the Space
                // being left, so keep it as the outgoing page instead of spending
                // another frame re-rendering it -- that frame would have to show
                // the sidebar untransitioned, snapping the pages back to rest just
                // before the settle animates them forward again.
                if render_space_track && self.workspace_space_swipe_capture_source {
                    self.workspace_space_swipe_source_frame =
                        Some(crate::termwindow::CapturedSidebar {
                            quads: std::mem::take(&mut sidebar_frame),
                            list: live_list,
                        });
                    self.workspace_space_swipe_capture_source = false;
                    #[cfg(target_os = "macos")]
                    if self.workspace_space_swipe_pending_commit.is_some() {
                        if let Some(window) = self.window.clone() {
                            window.notify(crate::termwindow::TermWindowNotif::Apply(Box::new(
                                |term_window| {
                                    term_window.complete_workspace_space_swipe_switch();
                                },
                            )));
                        }
                    }
                }
            } else if capture_space_source {
                let mut sidebar_layers = layer.tee_quad_allocator(&mut sidebar_frame);
                self.paint_workspace_sidebar(&mut sidebar_layers)
                    .context("capture source workspace sidebar")?;
                drop(sidebar_layers);

                self.workspace_space_swipe_source_frame =
                    Some(crate::termwindow::CapturedSidebar {
                        quads: sidebar_frame,
                        list: self.workspace_sidebar_list_quads,
                    });
                self.workspace_space_swipe_capture_source = false;
                #[cfg(target_os = "macos")]
                if self.workspace_space_swipe_pending_commit.is_some() {
                    if let Some(window) = self.window.clone() {
                        window.notify(crate::termwindow::TermWindowNotif::Apply(Box::new(
                            |term_window| {
                                term_window.complete_workspace_space_swipe_switch();
                            },
                        )));
                    }
                }
            } else {
                let mut sidebar_layers = layer.quad_allocator();
                self.paint_workspace_sidebar(&mut sidebar_layers)
                    .context("paint_workspace_sidebar")?;
                drop(sidebar_layers);
            }

            let mut chrome_layers = layer.quad_allocator();
            self.paint_right_sidebar(&mut chrome_layers)
                .context("paint_right_sidebar")?;

            if self.show_tab_bar {
                self.paint_tab_bar(&mut chrome_layers)
                    .context("paint_tab_bar")?;
            }
            drop(chrome_layers);

            if let Some((overlay, hidden, items)) = hover_overlay {
                self.paint_workspace_sidebar_hover_overlay(&overlay, hidden)
                    .context("paint hover-revealed workspace sidebar")?;
                // After the tab bar, on purpose: collapsed, the tab strip
                // starts at x=0 and `resolve_ui_item` is last-pushed-wins,
                // so items registered before it would lose the top of the
                // panel to the tabs underneath.
                self.ui_items.extend(items);
            } else if self.workspace_sidebar_hover.is_arming() {
                // The dwell is counting down: show a thin strip at the edge
                // so a correctly-parked pointer looks different from a
                // wrongly-parked one. Sub-layer 2, after the tab bar, for the
                // same reason the overlay itself is.
                if let Some((zx, zy, _, zh)) = self.workspace_sidebar_hover_hot_zone() {
                    let hint_width =
                        self.ui_px(crate::termwindow::ui::tokens::SIDEBAR_HOVER_HINT_WIDTH) as f32;
                    let palette =
                        self.chrome();
                    let mut hint_layers = layer.quad_allocator();
                    self.filled_rectangle(
                        &mut hint_layers,
                        2,
                        euclid::rect(zx as f32, zy as f32, hint_width, zh as f32),
                        palette.sidebar_row_active_border.mul_alpha(0.7),
                    )
                    .context("paint sidebar hover arming hint")?;
                }
            }
        }

        if fading_content_view {
            self.ui_items.truncate(ui_items_after_view);
        }

        if recording_flight {
            // Every allocator borrowing the recording has been dropped, and
            // the view has laid itself out, so it can now say where this
            // terminal is going.
            self.resolve_content_view_flight(flight_capture);
        }
        if fading_content_view {
            self.paint_content_view_chrome()
                .context("paint content view chrome")?;
            self.paint_content_view_flight()
                .context("paint content view flight")?;
        }

        let mut layers = layer.quad_allocator();
        self.paint_window_borders(&mut layers)
            .context("paint_window_borders")?;
        drop(layers);

        self.paint_modal().context("paint_modal")?;
        self.paint_context_menu().context("paint_context_menu")?;
        self.paint_command_palette()
            .context("paint_command_palette")?;
        self.paint_pane_tab_drag_overlay()
            .context("paint_pane_tab_drag_overlay")?;
        self.paint_sidebar_row_drag_overlay()
            .context("paint_sidebar_row_drag_overlay")?;
        self.paint_file_drag_ghost()
            .context("paint_file_drag_ghost")?;
        // Last, so the tag sits above every chrome surface it might overhang.
        self.paint_hover_tooltip().context("paint_hover_tooltip")?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_preview(area: RectF) -> TerminalPreviewRequest {
        TerminalPreviewRequest {
            tab_id: 1,
            snapshot: Arc::new(crate::termwindow::content_view::TerminalPreviewSnapshot {
                tab_size: TerminalSize::default(),
                panes: vec![],
                splits: vec![],
            }),
            area,
            clip: area,
            hold_scale: false,
        }
    }

    fn test_dimensions() -> Dimensions {
        Dimensions {
            pixel_width: 1600,
            pixel_height: 1000,
            dpi: 144,
        }
    }

    /// The whole point of the cache: an unchanged snapshot in an unchanged card
    /// replays instead of being rebuilt.
    #[test]
    fn the_same_snapshot_in_the_same_card_keeps_its_quads() {
        let preview = test_preview(euclid::rect(10.0, 20.0, 400.0, 260.0));
        assert_eq!(
            preview_quad_key(&preview, &test_dimensions(), 3, 1.0),
            preview_quad_key(&preview, &test_dimensions(), 3, 1.0)
        );
    }

    /// A card that has only scrolled shows the same picture somewhere else, so
    /// it stays a cache hit and is replayed through a translation.
    #[test]
    fn a_card_that_only_moved_keeps_its_quads() {
        let mut moved = test_preview(euclid::rect(10.0, 20.0, 400.0, 260.0));
        let key = preview_quad_key(&moved, &test_dimensions(), 3, 1.0);
        moved.area = moved.area.translate(euclid::vec2(0.0, -140.0));
        assert_eq!(preview_quad_key(&moved, &test_dimensions(), 3, 1.0), key);
    }

    /// Resizing a card rebuckets the font scale, so the recorded quads are the
    /// wrong picture even though the terminal has not changed.
    #[test]
    fn a_resized_card_rebuilds() {
        let small = test_preview(euclid::rect(10.0, 20.0, 400.0, 260.0));
        let mut large = small.clone();
        large.area.size.width = 480.0;
        assert_ne!(
            preview_quad_key(&large, &test_dimensions(), 3, 1.0),
            preview_quad_key(&small, &test_dimensions(), 3, 1.0)
        );
    }

    /// A font finishing its load means the card should be re-shaped, so the
    /// key stops matching and the card rebuilds.
    #[test]
    fn a_newly_loaded_font_rebuilds() {
        let preview = test_preview(euclid::rect(10.0, 20.0, 400.0, 260.0));
        assert_ne!(
            preview_quad_key(&preview, &test_dimensions(), 4, 1.0),
            preview_quad_key(&preview, &test_dimensions(), 3, 1.0)
        );
    }

    /// The distinction the rebuild budget rests on: which misses may be shown
    /// one frame late, and which may not be shown at all.
    ///
    /// New content and a newly loaded font both leave quads that draw valid
    /// pixels in the right places -- only what they depict is behind. A card
    /// that has changed size or moved to another window does not: its quads
    /// carry positions relative to the window's centre and glyphs sized for the
    /// old card, so replaying them puts the picture in the wrong place. That is
    /// the flicker.
    #[test]
    fn only_a_stale_picture_may_be_shown_late() {
        let first = test_preview(euclid::rect(10.0, 20.0, 400.0, 260.0));
        let mut recaptured = first.clone();
        recaptured.snapshot = Arc::new((*first.snapshot).clone());

        let old = preview_quad_key(&first, &test_dimensions(), 3, 1.0);

        let new_content = preview_quad_key(&recaptured, &test_dimensions(), 3, 1.0);
        assert_ne!(new_content, old, "a recapture must eventually be rebuilt");
        assert_eq!(
            new_content.geometry, old.geometry,
            "but the recorded quads still draw the right pixels meanwhile"
        );

        let new_font = preview_quad_key(&first, &test_dimensions(), 4, 1.0);
        assert_ne!(new_font, old, "a loaded font must eventually be rebuilt");
        assert_eq!(
            new_font.geometry, old.geometry,
            "shaping does not move the atlas, so the quads stay showable"
        );

        let mut resized = first.clone();
        resized.area.size.width = 480.0;
        assert_ne!(
            preview_quad_key(&resized, &test_dimensions(), 3, 1.0).geometry,
            old.geometry,
            "a resized card must not be replayed, at any budget"
        );

        let mut wider = test_dimensions();
        wider.pixel_width += 200;
        assert_ne!(
            preview_quad_key(&first, &wider, 3, 1.0).geometry,
            old.geometry,
            "a resized window must not be replayed, at any budget"
        );
    }

    /// Quad positions are relative to the centre of the window, so every
    /// recorded position moves when the window does.
    #[test]
    fn a_resized_window_rebuilds() {
        let preview = test_preview(euclid::rect(10.0, 20.0, 400.0, 260.0));
        let mut wider = test_dimensions();
        wider.pixel_width += 200;
        assert_ne!(
            preview_quad_key(&preview, &wider, 3, 1.0),
            preview_quad_key(&preview, &test_dimensions(), 3, 1.0)
        );
    }

    /// Two cards holding equal-looking snapshots are still two pictures: the
    /// key is the snapshot's identity, not its value.
    #[test]
    fn a_fresh_capture_rebuilds_even_when_it_looks_the_same() {
        let first = test_preview(euclid::rect(10.0, 20.0, 400.0, 260.0));
        let mut recaptured = first.clone();
        recaptured.snapshot = Arc::new((*first.snapshot).clone());
        assert_ne!(
            preview_quad_key(&recaptured, &test_dimensions(), 3, 1.0),
            preview_quad_key(&first, &test_dimensions(), 3, 1.0)
        );
    }

    /// A card exactly the size of the grid it holds is left alone. This is the
    /// case the fill exists to *not* disturb.
    #[test]
    fn a_grid_that_already_fits_its_card_is_not_touched() {
        let (x, y) = preview_fill_factors(522.0, 504.0, 522.0, 504.0);
        assert_eq!((x, y), (1.0, 1.0));
    }

    /// Whole-pixel cells leave the grid short on both axes; closing that gap is
    /// a uniform enlargement, so the picture grows without changing shape.
    #[test]
    fn a_grid_short_on_both_axes_is_enlarged_without_reshaping() {
        let (x, y) = preview_fill_factors(500.0, 400.0, 550.0, 440.0);
        assert!((x - 1.1).abs() < 1e-4, "{x}");
        assert!((y - x).abs() < 1e-4, "axes diverged: {x} vs {y}");
    }

    /// The case that left a bare strip down the side of every card: rounding
    /// made the cells narrow, so the grid filled the height with room to spare
    /// across. The short axis is allowed to catch up.
    #[test]
    fn the_axis_left_short_by_cell_rounding_catches_up() {
        // Height binds at 1.0; width has 8% of slack, the amount a 19x41 cell
        // loses becoming 6x14.
        let (x, y) = preview_fill_factors(500.0, 400.0, 540.0, 400.0);
        assert!((y - 1.0).abs() < 1e-4, "bound axis moved: {y}");
        assert!((x - 1.08).abs() < 1e-4, "{x}");
    }

    /// A card can be showing a terminal from another window, genuinely a
    /// different shape. Filling the card must not turn it into a different
    /// terminal.
    #[test]
    fn a_terminal_of_a_different_shape_is_not_reshaped_to_fit() {
        // Twice as wide as the card wants: far past anything rounding explains.
        let (x, y) = preview_fill_factors(500.0, 400.0, 1000.0, 400.0);
        assert!((y - 1.0).abs() < 1e-4);
        assert!(
            (x - MAX_PREVIEW_ASPECT_TRIM).abs() < 1e-4,
            "stretched to {x}, past the bound"
        );
    }

    #[test]
    fn enlargement_has_a_ceiling() {
        let (x, y) = preview_fill_factors(100.0, 100.0, 10_000.0, 10_000.0);
        assert!((x - MAX_PREVIEW_FILL).abs() < 1e-4, "{x}");
        assert!((y - MAX_PREVIEW_FILL).abs() < 1e-4, "{y}");
    }

    #[test]
    fn a_degenerate_card_or_grid_asks_for_no_stretch() {
        assert_eq!(preview_fill_factors(0.0, 400.0, 500.0, 400.0), (1.0, 1.0));
        assert_eq!(preview_fill_factors(500.0, 400.0, 0.0, 400.0), (1.0, 1.0));
        assert_eq!(preview_fill_factors(500.0, 0.0, 500.0, 400.0), (1.0, 1.0));
    }

    /// Quantization only ever rounds down, so a grid laid out at the quantized
    /// scale cannot overflow the extent it was measured against.
    #[test]
    fn quantizing_a_scale_never_rounds_up() {
        let minimum = 1.0 / TERMINAL_PREVIEW_SCALE_BUCKETS_PER_UNIT;
        for raw in [0.9999_f64, 0.5, 0.33, 0.0417, 0.001] {
            let quantized = quantize_terminal_preview_scale_down(raw, minimum);
            assert!(quantized <= raw.max(minimum) + 1e-9, "{raw} -> {quantized}");
            assert!(quantized >= minimum);
        }
    }

    #[test]
    fn a_scale_that_is_not_a_number_falls_back_to_the_minimum() {
        let minimum = 0.25;
        assert_eq!(
            quantize_terminal_preview_scale_down(f64::NAN, minimum),
            minimum
        );
    }
}

#[cfg(test)]
mod atlas_policy_tests {
    use super::{atlas_overflow_action, sticky_allow_images, AllowImage, AtlasAction};
    use std::time::{Duration, Instant};

    const CEIL: usize = 2048;
    const CAP: usize = 8192;

    #[test]
    fn first_pass_grows_only_up_to_the_ceiling() {
        assert_eq!(
            atlas_overflow_action(0, 512, 1024, CEIL, CAP),
            AtlasAction::Grow(1024)
        );
        assert_eq!(
            atlas_overflow_action(0, 1024, 4096, CEIL, CAP),
            AtlasAction::Grow(CEIL)
        );
    }

    #[test]
    fn first_pass_at_or_past_the_ceiling_clears_in_place() {
        assert_eq!(
            atlas_overflow_action(0, CEIL, 4096, CEIL, CAP),
            AtlasAction::ClearInPlace
        );
        assert_eq!(
            atlas_overflow_action(0, CAP, 16384, CEIL, CAP),
            AtlasAction::ClearInPlace
        );
    }

    #[test]
    fn retry_passes_grow_to_the_request_within_the_cap() {
        assert_eq!(
            atlas_overflow_action(1, CEIL, 4096, CEIL, CAP),
            AtlasAction::Grow(4096)
        );
        assert_eq!(
            atlas_overflow_action(2, 4096, CAP, CEIL, CAP),
            AtlasAction::Grow(CAP)
        );
    }

    #[test]
    fn a_request_past_the_cap_is_a_failure_not_a_rounding() {
        // The freeze case: already at the cap, asked for more. Rounding to
        // the cap would rebuild at the same size, succeed, and overflow
        // identically on the next pass.
        assert_eq!(
            atlas_overflow_action(1, CAP, 16384, CEIL, CAP),
            AtlasAction::CapExceeded
        );
        assert_eq!(
            atlas_overflow_action(1, 4096, 16384, CEIL, CAP),
            AtlasAction::CapExceeded
        );
    }

    #[test]
    fn downscale_levels_end_at_no_images() {
        assert_eq!(AllowImage::Yes.coarser(), Some(AllowImage::Scale(2)));
        assert_eq!(AllowImage::Scale(2).coarser(), Some(AllowImage::Scale(4)));
        assert_eq!(AllowImage::Scale(4).coarser(), Some(AllowImage::Scale(8)));
        assert_eq!(AllowImage::Scale(8).coarser(), Some(AllowImage::No));
        assert_eq!(AllowImage::No.coarser(), None);
    }

    #[test]
    fn sticky_hold_carries_the_level_until_it_lapses() {
        let armed = Instant::now();
        let hold = Some((AllowImage::Scale(2), armed));
        let hold_for = Duration::from_secs(30);

        let (level, kept) = sticky_allow_images(hold, armed + Duration::from_secs(10), hold_for);
        assert_eq!(level, AllowImage::Scale(2));
        assert_eq!(kept, hold);

        let (level, kept) = sticky_allow_images(hold, armed + Duration::from_secs(31), hold_for);
        assert_eq!(level, AllowImage::Yes);
        assert_eq!(kept, None);

        let (level, kept) = sticky_allow_images(None, armed, hold_for);
        assert_eq!(level, AllowImage::Yes);
        assert_eq!(kept, None);
    }
}
