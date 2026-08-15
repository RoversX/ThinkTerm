use crate::quad::{HeapQuadAllocator, QuadClipRect, QuadTrait, TripleLayerQuadAllocator};
use crate::termwindow::content_view::{ContentViewTypography, TerminalPreviewRequest};
use crate::termwindow::render::{LineToEleShapeCacheKey, RenderScreenLineParams};
use crate::termwindow::{RenderFrame, TermWindowNotif};
use crate::ui::{DrawContext, UiPalette};
use ::window::bitmaps::atlas::OutOfTextureSpace;
use ::window::color::LinearRgba;
use ::window::RectF;
use ::window::WindowOps;
use anyhow::Context;
use mux::renderable::{RenderableDimensions, StableCursorPosition};
use mux::tab::SplitDirection;
use smol::Timer;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::time::{Duration, Instant};
use wezterm_font::ClearShapeCache;
use wezterm_term::color::ColorAttribute;
use wezterm_term::TerminalSize;

const TERMINAL_PREVIEW_EXTENT_BUCKET_DESIGN_PX: f32 = 8.0;
const TERMINAL_PREVIEW_SCALE_BUCKETS_PER_UNIT: f64 = 128.0;
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
        assert_eq!(
            quantize_terminal_preview_scale_down(0.08, minimum),
            0.078125
        );
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
        let palette = UiPalette::for_appearance(crate::native_settings::effective_appearance());
        self.filled_rectangle(layers, 0, area, palette.window_bg)
            .context("frontend handoff opaque background")?;

        let font_size = crate::native_settings::home_font_size(&crate::native_settings::load_shared());
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

    /// In A mode a follower keeps the canonical PTY grid. If its window is
    /// larger, mark the renderer-only remainder with a faint cell grid rather
    /// than stretching or reflowing terminal data that belongs to the owner.
    fn paint_frontend_shared_unused_grid(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
    ) -> anyhow::Result<()> {
        let Some(state) = self.active_remote_frontend_viewport_state() else {
            return Ok(());
        };
        if state.access.mode != codec::FrontendAccessMode::TmuxLatest
            || self.owns_frontend_viewport()
        {
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
        let palette = UiPalette::for_appearance(crate::native_settings::effective_appearance());
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

    pub fn paint_impl(&mut self, frame: &mut RenderFrame) {
        self.num_frames += 1;
        // If nothing on screen needs animating, then we can avoid
        // invalidating as frequently
        *self.has_animation.borrow_mut() = None;
        // Start with the assumption that we should allow images to render
        self.allow_images = AllowImage::Yes;

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

        'pass: for pass in 0.. {
            match self.paint_pass() {
                Ok(_) => match self.render_state.as_mut().unwrap().allocated_more_quads() {
                    Ok(allocated) => {
                        if !allocated {
                            break 'pass;
                        }
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
                        let result = if pass == 0 {
                            // Grow while there is headroom, rather than
                            // clearing in place.
                            //
                            // Clearing answers "the atlas is full of glyphs we
                            // no longer need". It is the wrong answer to "the
                            // working set does not fit": the frame that
                            // overflowed fits once the atlas is empty, so this
                            // never reached the growth branch below, and the
                            // next frame that wants the same glyphs overflows
                            // again. Toggling the overview cleared the atlas
                            // every single time, re-rasterising every glyph on
                            // screen -- and taking the recorded transition
                            // frames, whose texture coordinates the rebuild
                            // invalidates, with it.
                            let grown = size.min(crate::termwindow::MAX_GROWN_ATLAS_SIZE);
                            if grown > current_size {
                                log::trace!("grow texture atlas {current_size} -> {grown}");
                                self.recreate_texture_atlas(Some(grown))
                            } else {
                                // At the ceiling: clearing is all that is left,
                                // and it is also what reclaims the one-off
                                // glyphs a closed overview leaves behind.
                                log::trace!("recreate_texture_atlas at {current_size}");
                                self.recreate_texture_atlas(Some(current_size))
                            }
                        } else {
                            log::trace!("grow texture atlas to {}", size);
                            self.recreate_texture_atlas(Some(size))
                        };
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
                            self.allow_images = match self.allow_images {
                                AllowImage::Yes => AllowImage::Scale(2),
                                AllowImage::Scale(2) => AllowImage::Scale(4),
                                AllowImage::Scale(4) => AllowImage::Scale(8),
                                AllowImage::Scale(8) => AllowImage::No,
                                AllowImage::No | _ => {
                                    log::error!(
                                        "Failed to {} texture: {}",
                                        if pass == 0 { "clear" } else { "resize" },
                                        err
                                    );
                                    break 'pass;
                                }
                            };

                            log::info!(
                                "Not enough texture space ({:#}); \
                                     will retry render with {:?}",
                                err,
                                self.allow_images,
                            );
                        }
                    } else if err.root_cause().downcast_ref::<ClearShapeCache>().is_some() {
                        self.invalidate_fancy_tab_bar();
                        self.invalidate_modal();
                        self.shape_generation += 1;
                        self.shape_cache.borrow_mut().clear();
                        self.ui_shape_caches.borrow_mut().clear_all();
                        self.publish_ui_shape_cache_diagnostics();
                        self.line_to_ele_shape_cache.borrow_mut().clear();
                    } else {
                        log::error!("paint_pass failed: {:#}", err);
                        break 'pass;
                    }
                }
            }
        }
        log::debug!("paint_impl before call_draw elapsed={:?}", start.elapsed());

        self.call_draw(frame).ok();
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
        if self.focused.is_some() {
            if let Some(next_due) = *self.has_animation.borrow() {
                let prior = self.scheduled_animation.borrow_mut().take();
                match prior {
                    Some(prior) if prior <= next_due => {
                        // Already due before that time
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
            // to be lifted clear of it.
            Some(opacity) => self.composite_above_terminal(&heap, opacity)?,
            // Settled: nothing else is on screen to be ordered against.
            None => heap.apply_to_clipped(layers, self.surface_clip(), 1.0)?,
        }
        self.content_view_last_frame = Some(heap);
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
        ghost.apply_to_clipped(&mut layers, self.surface_clip(), opacity)
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
        let palette = UiPalette::for_appearance(crate::native_settings::effective_appearance());

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

        let (next_frame, previews) = {
            let gl_state = self.render_state.as_ref().unwrap();
            let ctx = DrawContext::new(gl_state, dimensions, &render_metrics);
            if let Some(idx) = active_content_view_idx {
                let view = self.content_views[idx].view.as_mut();
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
        // One allocator for every card, borrowed out of the window and put
        // back, so its pool of quad boxes outlives both the loop and the frame.
        // Each card's thumbnail is thousands of quads and there are as many
        // cards as fit the viewport; building and dropping that from scratch
        // per card per frame is millions of allocations a second, spent on
        // memory that was about to be asked for again.
        let mut heap = std::mem::take(&mut *self.preview_quad_heap.borrow_mut());
        let result = (|| {
            for preview in previews {
                self.paint_terminal_preview(layers, preview, &mut heap)?;
            }
            Ok(())
        })();
        heap.recycle();
        *self.preview_quad_heap.borrow_mut() = heap;
        result
    }

    fn paint_terminal_preview(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        preview: &TerminalPreviewRequest,
        heap: &mut HeapQuadAllocator,
    ) -> anyhow::Result<()> {
        heap.recycle();
        {
            let mut clipped_layers = TripleLayerQuadAllocator::Heap(heap);
            self.paint_terminal_preview_unclipped(&mut clipped_layers, preview)?;
        }
        let clip = QuadClipRect::from_top_left_pixels(
            preview.clip.min_x(),
            preview.clip.min_y(),
            preview.clip.max_x(),
            preview.clip.max_y(),
            &self.dimensions,
        );
        heap.apply_to_clipped(layers, clip, 1.0)
    }

    fn paint_terminal_preview_unclipped(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        preview: &TerminalPreviewRequest,
    ) -> anyhow::Result<()> {
        let snapshot = &preview.snapshot;
        let tab_size = snapshot.tab_size;
        if tab_size.cols == 0 || tab_size.rows == 0 {
            return Ok(());
        }
        if snapshot.panes.is_empty() {
            return Ok(());
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
        self.filled_rectangle(layers, 0, preview.clip, preview_background)?;

        // Quantization bounds the number of cached FontConfigurations even
        // when cards continuously resize. Flooring guarantees the terminal
        // grid stays inside its preview rather than clipping the last column.
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
        // below would otherwise walk a new bucket every few pixels of card
        // width, and every bucket is a `FontConfiguration` that builds a font,
        // rasterises glyphs and grows the atlas -- and is never evicted.
        let scale_key = (tab_size.cols, tab_size.rows);
        if preview.hold_scale {
            if let Some(held) = self.preview_scale_hold.borrow().get(&scale_key) {
                quantized_scale = *held;
            }
        }
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

        for pane in &snapshot.panes {
            let palette = &pane.palette;
            let pane_x = origin_x + pane.left as f32 * cell_width;
            let pane_y = origin_y + pane.top as f32 * cell_height;
            let pane_width = pane.width as f32 * cell_width;
            let pane_height = pane.height as f32 * cell_height;
            let pane_rect = euclid::rect(pane_x, pane_y, pane_width, pane_height);
            let Some(pane_clip) = pane_rect.intersection(&preview.clip) else {
                continue;
            };
            self.filled_rectangle(
                layers,
                0,
                pane_clip,
                palette.resolve_bg(ColorAttribute::Default).to_linear(),
            )?;

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
            let rendered_y = grid_top;
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
            let source_rect = QuadClipRect::from_top_left_pixels(
                pane_x,
                rendered_y,
                pane_x + rendered_width,
                rendered_y + rendered_height,
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
            let transformed =
                layers.set_heap_position_transform(Some((source_rect, target_rect)));
            debug_assert!(transformed);
            let source_visible_top = rendered_y
                + (pane_clip.min_y() - grid_top) * rendered_height / grid_height.max(1.0);
            let source_visible_bottom = rendered_y
                + (pane_clip.max_y() - grid_top) * rendered_height / grid_height.max(1.0);

            for (line_idx, line) in pane.lines.iter().take(rows).enumerate() {
                let y = rendered_y + line_idx as f32 * pane_cell_height;
                if y + pane_cell_height <= source_visible_top || y >= source_visible_bottom {
                    continue;
                }
                let shape_hash = self.shape_hash_for_line(line);
                self.render_screen_line(
                    RenderScreenLineParams {
                        top_pixel_y: y,
                        left_pixel_x: pane_x,
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
                    },
                    layers,
                )?;
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
                    let cursor_rect: ::window::RectF = euclid::rect(
                        pane_x + cursor.x as f32 * pane_cell_width,
                        rendered_y + cursor_row as f32 * pane_cell_height,
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
            if let Some(visible) = rect.intersection(&preview.clip) {
                self.filled_rectangle(layers, 2, visible, split_color)?;
            }
        }
        Ok(())
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
            return Ok(());
        };
        if !state.active {
            return Ok(());
        }
        let label = state.title.clone();
        let anchor = state.current;
        let target_rect = state.target.as_ref().map(|target| target.rect);

        if let Some(rect) = target_rect {
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
            let fill = accent.mul_alpha(0.28);
            let border = accent.mul_alpha(0.8);

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
        let gesture_extent = self.workspace_sidebar_width() as f32;
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
        let gesture_extent = self.workspace_sidebar_width() as f32;
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
        self.advance_content_view_fade(frame_now);
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
                UiPalette::for_appearance(crate::native_settings::effective_appearance())
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
            let chrome = UiPalette::for_appearance(crate::native_settings::effective_appearance());
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
        let paint_terminal_world = (!content_view_active && !fading_content_view) || recording_flight;

        if !content_view_active {
            // Takeover remains opaque while this actively polls and hydrates
            // the post-resize remote screen.  Once every pane is coherent the
            // matching epoch is cleared before `frontend_blocked` is sampled.
            self.advance_frontend_geometry_confirmation();
        }

        let frontend_blocked = !content_view_active && self.frontend_surface_blocked();

        // Everything the terminal registers during a transition sits under a
        // view that is on its way in or out. Leaving those targets live would
        // let a click land on a pane the user is looking at through a
        // half-drawn overview.
        let ui_items_before_terminal = self.ui_items.len();

        if paint_terminal_world && !frontend_blocked {
            for pos in panes {
                if pos.is_active {
                    self.update_text_cursor(&pos);
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

            if render_space_push || render_space_track {
                let mut sidebar_layers = TripleLayerQuadAllocator::Heap(&mut sidebar_frame);
                self.paint_workspace_sidebar(&mut sidebar_layers)
                    .context("paint live workspace sidebar")?;
                drop(sidebar_layers);
                let live_list = self.workspace_sidebar_list_quads;

                if self.workspace_space_swipe_needs_settle_start {
                    let gesture_extent = self.workspace_sidebar_width() as f32;
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
        self.paint_pane_tab_drag_overlay()
            .context("paint_pane_tab_drag_overlay")?;
        self.paint_sidebar_row_drag_overlay()
            .context("paint_sidebar_row_drag_overlay")?;
        self.paint_file_drag_ghost()
            .context("paint_file_drag_ghost")?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
