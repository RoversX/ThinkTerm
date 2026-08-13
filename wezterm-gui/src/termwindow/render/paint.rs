use crate::quad::{HeapQuadAllocator, QuadTrait, TripleLayerQuadAllocator};
use crate::termwindow::content_view::{ContentViewTypography, TerminalPreviewRequest};
use crate::termwindow::render::{LineToEleShapeCacheKey, RenderScreenLineParams};
use crate::termwindow::{RenderFrame, TermWindowNotif};
use crate::ui::{DrawContext, UiPalette};
use ::window::bitmaps::atlas::OutOfTextureSpace;
use ::window::color::LinearRgba;
use ::window::WindowOps;
use anyhow::Context;
use mux::renderable::StableCursorPosition;
use mux::tab::SplitDirection;
use smol::Timer;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::time::{Duration, Instant};
use wezterm_font::ClearShapeCache;
use wezterm_term::color::ColorAttribute;

const TERMINAL_PREVIEW_EXTENT_BUCKET_DESIGN_PX: f32 = 8.0;
const TERMINAL_PREVIEW_SCALE_BUCKETS_PER_UNIT: f64 = 128.0;

fn quantized_terminal_preview_extent(extent: f32, dpi: usize) -> f32 {
    let bucket = crate::ui::scale_ui_f32(TERMINAL_PREVIEW_EXTENT_BUCKET_DESIGN_PX, dpi).max(1.0);
    if extent <= bucket {
        extent.max(1.0)
    } else {
        (extent / bucket).floor() * bucket
    }
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

#[cfg(test)]
mod terminal_preview_tests {
    use super::{
        minimum_terminal_preview_scale, quantize_terminal_preview_scale_down,
        quantized_terminal_preview_extent,
    };

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

        let font_size = crate::native_settings::home_font_size(&crate::native_settings::load());
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
                            // Let's try clearing out the atlas and trying again
                            // self.clear_texture_atlas()
                            log::trace!("recreate_texture_atlas");
                            self.recreate_texture_atlas(Some(current_size))
                        } else {
                            log::trace!("grow texture atlas to {}", size);
                            self.recreate_texture_atlas(Some(size))
                        };
                        self.invalidate_fancy_tab_bar();
                        self.invalidate_modal();

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
    pub fn paint_content_view(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
    ) -> anyhow::Result<()> {
        let settings = crate::native_settings::load();
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

        let (next_frame, previews) = {
            let gl_state = self.render_state.as_ref().unwrap();
            let ctx = DrawContext::new(gl_state, dimensions, &render_metrics);
            if let Some(idx) = active_content_view_idx {
                let view = self.content_views[idx].view.as_mut();
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
        for preview in previews {
            self.paint_terminal_preview(layers, preview)?;
        }
        Ok(())
    }

    fn paint_terminal_preview(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        preview: &TerminalPreviewRequest,
    ) -> anyhow::Result<()> {
        let mut heap = HeapQuadAllocator::default();
        {
            let mut clipped_layers = TripleLayerQuadAllocator::Heap(&mut heap);
            self.paint_terminal_preview_unclipped(&mut clipped_layers, preview)?;
        }
        heap.apply_to_clipped(
            layers,
            preview.clip,
            self.dimensions.pixel_width as f32,
            self.dimensions.pixel_height as f32,
        )
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

        let still_too_wide =
            tab_size.cols as f32 * metrics.cell_size.width.max(1) as f32 > bucketed_width;
        let still_too_tall =
            tab_size.rows as f32 * metrics.cell_size.height.max(1) as f32 > bucketed_height;
        if (still_too_wide || still_too_tall) && quantized_scale > minimum_scale {
            quantized_scale = minimum_scale;
            (font_config, metrics) = self.pane_font_resources(quantized_scale)?;
        }
        let cell_width = metrics.cell_size.width.max(1) as f32;
        let cell_height = metrics.cell_size.height.max(1) as f32;
        // Terminal content begins at the same top-left origin as the real
        // terminal. Any remainder stays on the right/bottom and is visually
        // continuous with the background painted above.
        let origin_x = preview.area.origin.x;
        let origin_y = preview.area.origin.y;

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
            if let Some(visible) = pane_rect.intersection(&preview.clip) {
                self.filled_rectangle(
                    layers,
                    0,
                    visible,
                    palette.resolve_bg(ColorAttribute::Default).to_linear(),
                )?;
            }

            let source_dims = pane.dimensions;
            let rows = pane.rows;
            let cols = pane.cols;
            if rows == 0 || cols == 0 {
                continue;
            }

            let mut render_dims = source_dims;
            render_dims.cols = cols;
            render_dims.viewport_rows = rows;
            render_dims.pixel_width = (cols as f32 * cell_width).round() as usize;
            render_dims.pixel_height = (rows as f32 * cell_height).round() as usize;
            let rendered_y = pane_y + pane.height.saturating_sub(rows) as f32 * cell_height;
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
            let font_identity = quantized_scale.to_bits()
                ^ palette_identity.rotate_left(17)
                ^ 0x4c49_5645_5052_4556;

            for (line_idx, line) in pane.lines.iter().enumerate() {
                let y = rendered_y + line_idx as f32 * cell_height;
                if y + cell_height <= preview.clip.min_y() || y >= preview.clip.max_y() {
                    continue;
                }
                let shape_hash = self.shape_hash_for_line(line);
                self.render_screen_line(
                    RenderScreenLineParams {
                        top_pixel_y: y,
                        left_pixel_x: pane_x,
                        pixel_width: cols as f32 * cell_width,
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
                        render_metrics: metrics,
                        font_config: Some(font_config.clone()),
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
                    let cursor_rect = euclid::rect(
                        pane_x + cursor.x as f32 * cell_width,
                        rendered_y + cursor_row as f32 * cell_height,
                        cell_width,
                        cell_height,
                    );
                    if let Some(cursor_rect) = cursor_rect.intersection(&preview.clip) {
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
            }
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
        let settings = crate::native_settings::load();
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
        let color = match crate::native_settings::effective_appearance() {
            window::Appearance::Light | window::Appearance::LightHighContrast => {
                LinearRgba::with_srgba(80, 80, 90, 255).mul_alpha(0.46)
            }
            window::Appearance::Dark | window::Appearance::DarkHighContrast => {
                LinearRgba::with_srgba(210, 210, 220, 255).mul_alpha(0.38)
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
        let settings = crate::native_settings::load();
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

    pub fn paint_pass(&mut self) -> anyhow::Result<()> {
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
        let mut layers = layer.quad_allocator();
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
        // so skip painting the terminal panes / splits.
        let content_view_active = self.content_view_foreground();

        if !content_view_active {
            // Takeover remains opaque while this actively polls and hydrates
            // the post-resize remote screen.  Once every pane is coherent the
            // matching epoch is cleared before `frontend_blocked` is sampled.
            self.advance_frontend_geometry_confirmation();
        }

        let frontend_blocked = !content_view_active && self.frontend_surface_blocked();

        if !content_view_active && !frontend_blocked {
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

        if !content_view_active && !frontend_blocked {
            self.paint_bottom_quote(&mut layers)
                .context("paint_bottom_quote")?;
        }

        if frontend_blocked {
            self.paint_frontend_handoff_overlay(&mut layers)
                .context("paint frontend handoff overlay")?;
        }

        if content_view_active {
            self.paint_content_view(&mut layers)
                .context("paint_content_view")?;
        }

        // A full-window ContentView owns all ThinkTerm chrome below the native
        // title bar. This is presentation-only: sidebar widths/collapse state
        // and terminal geometry stay unchanged behind the view.
        if self.content_view_is_full_window() {
            let mut chrome_items = self
                .paint_full_window_chrome(&mut layers)
                .context("paint full-window client chrome")?;
            self.ui_items.append(&mut chrome_items);
        } else {
            self.paint_workspace_sidebar(&mut layers)
                .context("paint_workspace_sidebar")?;
            self.paint_right_sidebar(&mut layers)
                .context("paint_right_sidebar")?;

            if self.show_tab_bar {
                self.paint_tab_bar(&mut layers).context("paint_tab_bar")?;
            }
        }

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
