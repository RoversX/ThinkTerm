use crate::resize_increment_calculator::ResizeIncrementCalculator;
use crate::termwindow::ui::pane_nav_bar_height_for_metrics;
use crate::ui::rescale_ui_usize;
use crate::utilsprites::RenderMetrics;
use ::window::{Dimensions, ResizeIncrement, Window, WindowOps, WindowState};
use config::{ConfigHandle, DimensionContext};
use mux::pane::PaneId;
use mux::tab::PositionedPane;
use mux::Mux;
use std::collections::HashMap;
use std::rc::Rc;
use wezterm_font::FontConfiguration;
use wezterm_term::TerminalSize;

#[derive(Debug, Clone, Copy)]
pub struct RowsAndCols {
    pub rows: usize,
    pub cols: usize,
}

#[derive(Debug)]
pub enum ScaleChange {
    Absolute(f64),
    Relative(f64),
}

impl super::TermWindow {
    fn normalized_font_scale_value(&self, font_scale: Option<f64>) -> Option<f64> {
        let global_scale = self.fonts.get_font_scale();
        font_scale
            .filter(|scale| scale.is_finite() && *scale > 0.0)
            .filter(|scale| scale.to_bits() != global_scale.to_bits())
    }

    fn persisted_font_scale_for_pane(&self, pane_id: PaneId) -> Option<f64> {
        let font_scale = self
            .pane_state
            .borrow()
            .get(&pane_id)
            .and_then(|state| state.font_scale);
        self.normalized_font_scale_value(font_scale)
    }

    pub(crate) fn snapshot_active_workspace_thread_layout(&self) {
        let mux = Mux::get();
        let Some(window) = mux.get_window(self.mux_window_id) else {
            return;
        };
        crate::workspace_threads::snapshot_active_space_thread_layout_with_font_scales(
            &self.active_space_id,
            window.get_workspace(),
            self.mux_window_id,
            |pane_id| self.persisted_font_scale_for_pane(pane_id),
        );
    }

    fn persist_workspace_pane_font_scales(&self) {
        let mux = Mux::get();
        let Some(window) = mux.get_window(self.mux_window_id) else {
            return;
        };
        crate::workspace_threads::snapshot_active_space_thread_layout_with_font_scales(
            &self.active_space_id,
            window.get_workspace(),
            self.mux_window_id,
            |pane_id| self.persisted_font_scale_for_pane(pane_id),
        );
    }

    fn apply_font_scales_to_mux_window_panes(&mut self, font_scales: HashMap<PaneId, Option<f64>>) {
        let mux = Mux::get();
        let Some(mux_window) = mux.get_window(self.mux_window_id) else {
            return;
        };

        let mut changed = false;
        for tab in mux_window.iter() {
            for pos in tab.iter_panes_ignoring_zoom() {
                let pane_id = pos.pane.pane_id();
                let font_scale = self.normalized_font_scale_value(
                    font_scales.get(&pane_id).copied().unwrap_or(None),
                );
                let mut state = self.pane_state(pos.pane.pane_id());
                if state.font_scale != font_scale {
                    state.font_scale = font_scale;
                    changed = true;
                }
            }
        }
        if !changed {
            // Called per TabAddedToWindow during resyncs; skip the cache
            // flush when nothing actually changed.
            return;
        }

        self.sync_pane_font_sizes();
        self.quad_generation += 1;
        self.shape_generation += 1;
        self.pane_font_cache.borrow_mut().clear();
        self.shape_cache.borrow_mut().clear();
        self.line_to_ele_shape_cache.borrow_mut().clear();
        self.invalidate_fancy_tab_bar();
        self.invalidate_modal();
        if let Some(window) = self.window.as_ref() {
            window.invalidate();
        }
    }

    pub(crate) fn apply_workspace_thread_font_scales(&mut self) {
        let mux = Mux::get();
        let Some(window) = mux.get_window(self.mux_window_id) else {
            return;
        };
        let Some(font_scales) = crate::workspace_threads::workspace_pane_font_scales(
            window.get_workspace(),
            self.mux_window_id,
        ) else {
            return;
        };
        self.apply_font_scales_to_mux_window_panes(font_scales);
    }

    pub(crate) fn apply_native_terminal_settings(&mut self) {
        let settings = crate::native_settings::load();
        let Some(font_size) = settings.terminal.font_size else {
            return;
        };
        if !font_size.is_finite() || font_size <= 0.0 || self.config.font_size <= 0.0 {
            return;
        }
        let font_scale = (font_size / self.config.font_size).clamp(0.25, 4.0);
        if let Some(window) = self.window.as_ref().cloned() {
            self.adjust_font_scale(font_scale, &window);
        }
    }

    pub(crate) fn resize_layout_for_dimensions(
        &self,
        dimensions: &Dimensions,
        scale_changed_cells: Option<RowsAndCols>,
    ) -> (TerminalSize, Dimensions, ResizeIncrementCalculator) {
        let config = &self.config;

        let tab_bar_height = if self.show_tab_bar {
            self.tab_bar_pixel_height().unwrap_or(0.)
        } else {
            0.
        };

        let border = self.get_os_border();

        if let Some(cell_dims) = scale_changed_cells {
            // Scaling preserves existing terminal dimensions, yielding a new
            // overall set of window dimensions
            let size = TerminalSize {
                rows: cell_dims.rows,
                cols: cell_dims.cols,
                pixel_height: cell_dims.rows * self.render_metrics.cell_size.height as usize,
                pixel_width: cell_dims.cols * self.render_metrics.cell_size.width as usize,
                dpi: dimensions.dpi as u32,
            };

            let rows = size.rows;
            let cols = size.cols;

            let h_context = DimensionContext {
                dpi: dimensions.dpi as f32,
                pixel_max: size.pixel_width as f32,
                pixel_cell: self.render_metrics.cell_size.width as f32,
            };
            let v_context = DimensionContext {
                dpi: dimensions.dpi as f32,
                pixel_max: size.pixel_height as f32,
                pixel_cell: self.render_metrics.cell_size.height as f32,
            };
            let padding_left = config.window_padding.left.evaluate_as_pixels(h_context) as usize
                + self.workspace_sidebar_width();
            let padding_top = config.window_padding.top.evaluate_as_pixels(v_context) as usize;
            let padding_bottom =
                config.window_padding.bottom.evaluate_as_pixels(v_context) as usize;
            let padding_right =
                effective_right_padding(&config, h_context) + self.right_sidebar_width();

            let pixel_height = (rows * self.render_metrics.cell_size.height as usize)
                + (padding_top + padding_bottom)
                + (border.top + border.bottom).get() as usize
                + tab_bar_height as usize;

            let pixel_width = (cols * self.render_metrics.cell_size.width as usize)
                + (padding_left + padding_right)
                + (border.left + border.right).get() as usize;

            let dims = Dimensions {
                pixel_width: pixel_width as usize,
                pixel_height: pixel_height as usize,
                dpi: dimensions.dpi,
            };

            let ri_calc = ResizeIncrementCalculator {
                x: self.render_metrics.cell_size.width as u16,
                y: self.render_metrics.cell_size.height as u16,
                padding_left: padding_left,
                padding_top: padding_top,
                padding_right: padding_right,
                padding_bottom: padding_bottom,
                border: border,
                tab_bar_height: tab_bar_height as usize,
            };

            (size, dims, ri_calc)
        } else {
            // Resize of the window dimensions may result in changed terminal dimensions

            let h_context = DimensionContext {
                dpi: dimensions.dpi as f32,
                pixel_max: self.terminal_size.pixel_width as f32,
                pixel_cell: self.render_metrics.cell_size.width as f32,
            };
            let v_context = DimensionContext {
                dpi: dimensions.dpi as f32,
                pixel_max: self.terminal_size.pixel_height as f32,
                pixel_cell: self.render_metrics.cell_size.height as f32,
            };
            let padding_left = config.window_padding.left.evaluate_as_pixels(h_context) as usize
                + self.workspace_sidebar_width();
            let padding_top = config.window_padding.top.evaluate_as_pixels(v_context) as usize;
            let padding_bottom =
                config.window_padding.bottom.evaluate_as_pixels(v_context) as usize;
            let padding_right =
                effective_right_padding(&config, h_context) + self.right_sidebar_width();

            let avail_width = dimensions.pixel_width.saturating_sub(
                (padding_left + padding_right) as usize
                    + (border.left + border.right).get() as usize,
            );
            let avail_height = dimensions
                .pixel_height
                .saturating_sub(
                    (padding_top + padding_bottom) as usize
                        + (border.top + border.bottom).get() as usize,
                )
                .saturating_sub(tab_bar_height as usize);

            let rows = avail_height / self.render_metrics.cell_size.height as usize;
            let cols = avail_width / self.render_metrics.cell_size.width as usize;

            let size = TerminalSize {
                rows,
                cols,
                // Take care to use the exact pixel dimensions of the cells, rather
                // than the available space, so that apps that are sensitive to
                // the pixels-per-cell have consistent values at a given font size.
                // https://github.com/wezterm/wezterm/issues/535
                pixel_height: rows * self.render_metrics.cell_size.height as usize,
                pixel_width: cols * self.render_metrics.cell_size.width as usize,
                dpi: dimensions.dpi as u32,
            };

            let ri_calc = ResizeIncrementCalculator {
                x: self.render_metrics.cell_size.width as u16,
                y: self.render_metrics.cell_size.height as u16,
                padding_left: padding_left,
                padding_top: padding_top,
                padding_right: padding_right,
                padding_bottom: padding_bottom,
                border: border,
                tab_bar_height: tab_bar_height as usize,
            };

            (size, *dimensions, ri_calc)
        }
    }

    pub(crate) fn terminal_size_for_positioned_pane(
        &self,
        pos: &PositionedPane,
        render_metrics: RenderMetrics,
    ) -> TerminalSize {
        let cell_width = render_metrics.cell_size.width.max(1) as usize;
        let cell_height = render_metrics.cell_size.height.max(1) as usize;
        let pane_nav_height = pane_nav_bar_height_for_metrics(render_metrics)
            .min(pos.pixel_height.saturating_sub(cell_height));
        let pixel_width = pos.pixel_width.max(cell_width);
        let pixel_height = pos
            .pixel_height
            .saturating_sub(pane_nav_height)
            .max(cell_height);
        let cols = (pixel_width / cell_width).max(1);
        let rows = (pixel_height / cell_height).max(1);

        TerminalSize {
            rows,
            cols,
            pixel_width: cols * cell_width,
            pixel_height: rows * cell_height,
            dpi: self.dimensions.dpi as u32,
        }
    }

    fn sync_positioned_pane_font_size(&self, pos: &PositionedPane) -> anyhow::Result<()> {
        let pane_id = pos.pane.pane_id();
        let font_scale = self.pane_font_scale(pane_id);
        let render_metrics = if font_scale.to_bits() == self.fonts.get_font_scale().to_bits() {
            self.render_metrics
        } else {
            self.pane_font_resources(font_scale)?.1
        };
        let target_size = self.terminal_size_for_positioned_pane(pos, render_metrics);
        let dims = pos.pane.get_dimensions();

        if dims.cols != target_size.cols
            || dims.viewport_rows != target_size.rows
            || dims.pixel_width != target_size.pixel_width
            || dims.pixel_height != target_size.pixel_height
            || dims.dpi != target_size.dpi
        {
            pos.pane.resize(target_size)?;
        }

        Ok(())
    }

    fn sync_active_mux_tab_pane_sizes(&self) {
        for pos in self.get_panes_to_render() {
            if let Err(err) = self.sync_positioned_pane_font_size(&pos) {
                log::error!(
                    "failed to sync font-scaled pane size for pane {}: {:#}",
                    pos.pane.pane_id(),
                    err
                );
            }
        }
    }

    pub fn sync_pane_font_sizes(&self) {
        if self.content_view_foreground() {
            return;
        }
        self.sync_active_mux_tab_pane_sizes();
    }

    /// Synchronize the newly adopted mux window even if a connection content
    /// view is still foreground. Remote mirror panes are intentionally skipped
    /// by Tab::resize, so attach/switch paths must explicitly size each pane.
    pub(crate) fn force_sync_active_mux_tab_pane_sizes(&self) {
        self.sync_active_mux_tab_pane_sizes();
    }

    pub(crate) fn resize_mux_tabs_to_current_terminal_size(&mut self) {
        let mux = Mux::get();
        if let Some(window) = mux.get_window(self.mux_window_id) {
            for tab in window.iter() {
                tab.resize(self.terminal_size);
            }
        }
        self.reapply_collapsed_panes_for_window();
        self.force_sync_active_mux_tab_pane_sizes();
    }

    pub fn resize(
        &mut self,
        dimensions: Dimensions,
        window_state: WindowState,
        window: &Window,
        live_resizing: bool,
    ) {
        log::trace!(
            "resize event, live={} current cells: {:?}, current dims: {:?}, new dims: {:?} window_state:{:?}",
            live_resizing,
            self.current_cell_dimensions(),
            self.dimensions,
            dimensions,
            window_state,
        );
        if dimensions.pixel_width == 0 || dimensions.pixel_height == 0 {
            // on windows, this can happen when minimizing the window.
            // NOP!
            log::trace!("new dimensions are zero: NOP!");
            return;
        }
        if self.dimensions == dimensions && self.window_state == window_state {
            // It didn't really change
            log::trace!("dimensions didn't change NOP!");
            return;
        }
        super::gpu_debug(format!(
            "main_window resize live={} {}x{} -> {}x{} dpi {} -> {} backend={}",
            live_resizing,
            self.dimensions.pixel_width,
            self.dimensions.pixel_height,
            dimensions.pixel_width,
            dimensions.pixel_height,
            self.dimensions.dpi,
            dimensions.dpi,
            if self.webgpu.is_some() {
                "WebGpu"
            } else if self.gl.is_some() {
                "OpenGL"
            } else {
                "none"
            }
        ));
        let last_state = self.window_state;
        self.window_state = window_state;
        self.quad_generation += 1;
        if last_state != self.window_state {
            self.load_os_parameters();
        }

        if let Some(webgpu) = self.webgpu.as_mut() {
            webgpu.resize(dimensions);
        }

        // For simple, user-interactive resizes where the dpi doesn't change,
        // skip our scaling recalculation
        if live_resizing && self.dimensions.dpi == dimensions.dpi {
            self.apply_dimensions(&dimensions, None, window);
        } else {
            self.scaling_changed(dimensions, self.fonts.get_font_scale(), window);
        }
        if let Some(modal) = self.get_modal() {
            modal.reconfigure(self);
        }
        self.emit_window_event("window-resized", None);
    }

    pub fn apply_pending_scale_changes(&mut self) {
        while self.resizes_pending == 0 {
            match self.pending_scale_changes.pop_front() {
                Some(ScaleChange::Relative(change)) => {
                    if let Some(window) = self.window.as_ref().map(|w| w.clone()) {
                        self.adjust_font_scale(self.fonts.get_font_scale() * change, &window);
                    }
                }
                Some(ScaleChange::Absolute(change)) => {
                    if let Some(window) = self.window.as_ref().map(|w| w.clone()) {
                        self.adjust_font_scale(change, &window);
                    }
                }
                None => break,
            }
        }
    }

    pub fn apply_scale_change(&mut self, dimensions: &Dimensions, font_scale: f64) {
        let config = &self.config;
        let font_size = config.font_size * font_scale;
        let theoretical_height = font_size * dimensions.dpi as f64 / 72.0;

        if theoretical_height < 2.0 {
            log::warn!(
                "refusing to go to an unreasonably small font scale {:?}
                       font_scale={} would yield font_height {}",
                dimensions,
                font_scale,
                theoretical_height
            );
            return;
        }

        let (prior_font, prior_dpi) = self.fonts.change_scaling(font_scale, dimensions.dpi);
        match RenderMetrics::new(&self.fonts) {
            Ok(metrics) => {
                self.render_metrics = metrics;
            }
            Err(err) => {
                log::error!(
                    "{:#} while attempting to scale font to {} with {:?}",
                    err,
                    font_scale,
                    dimensions
                );
                // Restore prior scaling factors
                self.fonts.change_scaling(prior_font, prior_dpi);
            }
        }

        if let Err(err) = self.recreate_texture_atlas(None) {
            log::error!("recreate_texture_atlas: {:#}", err);
        }
        self.invalidate_fancy_tab_bar();
        self.invalidate_modal();
    }

    pub fn apply_dimensions(
        &mut self,
        dimensions: &Dimensions,
        mut scale_changed_cells: Option<RowsAndCols>,
        window: &Window,
    ) {
        log::trace!(
            "apply_dimensions {:?} scale_changed_cells {:?}. window_state {:?}",
            dimensions,
            scale_changed_cells,
            self.window_state
        );
        let saved_dims = self.dimensions;
        self.dimensions = *dimensions;
        self.quad_generation += 1;

        if scale_changed_cells.is_some() && !self.window_state.can_resize() {
            log::warn!(
                "cannot resize window to match {:?} because window_state is {:?}",
                scale_changed_cells,
                self.window_state
            );
            scale_changed_cells.take();
        }

        // Technically speaking, we should compute the rows and cols
        // from the new dimensions and apply those to the tabs, and
        // then for the scaling changed case, try to re-apply the
        // original rows and cols, but if we do that we end up
        // double resizing the tabs, so we speculatively apply the
        // final size, which in that case should result in a NOP
        // change to the tab size.

        let (size, dims, ri_calc) =
            self.resize_layout_for_dimensions(dimensions, scale_changed_cells);

        log::trace!("apply_dimensions computed size {:?}, dims {:?}", size, dims);

        let terminal_size_changed = self.terminal_size != size;
        self.terminal_size = size;
        if terminal_size_changed {
            if self.content_view_foreground() {
                log::trace!("content view foreground; deferring mux tab resize");
            } else {
                self.resize_mux_tabs_to_current_terminal_size();
            }
        } else {
            log::trace!("terminal size unchanged; syncing active pane geometry");
            if !self.content_view_foreground() {
                self.sync_pane_font_sizes();
            }
        }

        self.resize_overlays();
        self.invalidate_fancy_tab_bar();
        self.update_title();

        window.set_resize_increments(if self.config.use_resize_increments {
            ri_calc.into()
        } else {
            ResizeIncrement::disabled()
        });

        // Queue up a speculative resize in order to preserve the number of rows+cols
        if let Some(cell_dims) = scale_changed_cells {
            // If we don't think the dimensions have changed, don't request
            // the window to change.  This seems to help on Wayland where
            // we won't know what size the compositor thinks we should have
            // when we're first opened, until after it sends us a configure event.
            // If we send this too early, it will trump that configure event
            // and we'll end up with weirdness where our window renders in the
            // middle of a larger region that the compositor thinks we live in.
            // Wayland is weird!
            if saved_dims != dims {
                log::trace!(
                    "scale changed so resize from {:?} to {:?} {:?} (event called with {:?})",
                    saved_dims,
                    dims,
                    cell_dims,
                    dimensions
                );
                // Stash this size pre-emptively. Without this, on Windows,
                // when the font scaling is changed we can end up not seeing
                // these dimensions and the scaling_changed logic ends up
                // comparing two dimensions that have the same DPI and recomputing
                // an adjusted terminal size.
                // eg: rather than a simple old-dpi -> new dpi transition, we'd
                // see old-dpi -> new dpi, call set_inner_size, then see a
                // new-dpi -> new-dpi adjustment with a slightly different
                // pixel geometry which is considered to be a user-driven resize.
                // Stashing the dimensions here avoids that misconception.
                self.dimensions = dims;
                self.set_inner_size(window, dims.pixel_width, dims.pixel_height);
            }
        }
    }

    pub fn current_cell_dimensions(&self) -> RowsAndCols {
        RowsAndCols {
            rows: self.terminal_size.rows as usize,
            cols: self.terminal_size.cols as usize,
        }
    }

    #[allow(clippy::float_cmp)]
    pub fn scaling_changed(&mut self, dimensions: Dimensions, font_scale: f64, window: &Window) {
        fn dpi_adjusted(n: usize, dpi: usize) -> f32 {
            n as f32 / dpi as f32
        }

        /// On Windows, scaling changes may adjust the pixel geometry by a few pixels,
        /// so this function checks if we're in a close-enough ballpark.
        fn close_enough(a: f32, b: f32) -> bool {
            let diff = (a - b).abs();
            diff < 10.
        }

        // Distinguish between eg: dpi being detected as double the initial dpi (where
        // the pixel dimensions don't change), and the dpi change being detected, but
        // where the window manager also decides to tile/resize the window.
        // In the latter case, we don't want to preserve the terminal rows/cols.
        let simple_dpi_change = dimensions.dpi != self.dimensions.dpi
            && ((close_enough(
                dpi_adjusted(dimensions.pixel_height, dimensions.dpi),
                dpi_adjusted(self.dimensions.pixel_height, self.dimensions.dpi),
            ) && close_enough(
                dpi_adjusted(dimensions.pixel_width, dimensions.dpi),
                dpi_adjusted(self.dimensions.pixel_width, self.dimensions.dpi),
            )) || (close_enough(
                dimensions.pixel_width as f32,
                self.dimensions.pixel_width as f32,
            ) && close_enough(
                dimensions.pixel_height as f32,
                self.dimensions.pixel_height as f32,
            )));

        if simple_dpi_change && cfg!(target_os = "macos") {
            // Spooky action at a distance: on macOS, NSWindow::isZoomed can falsely
            // return YES in situations such as the current screen changing.
            // That causes window_state to believe that we are MAXIMIZED.
            // We cannot easily detect that in the window layer, but at this
            // layer, if we realize that the dpi was the only thing that changed
            // then remove the MAXIMIZED state so that the can_resize check
            // in adjust_font_scale will not block us from adapting to the new
            // DPI. This is gross and it would be better handled at the macOS
            // layer.
            // <https://github.com/wezterm/wezterm/issues/3503>
            self.window_state -= WindowState::MAXIMIZED;
        }

        let dpi_changed = dimensions.dpi != self.dimensions.dpi;
        let font_scale_changed = font_scale != self.fonts.get_font_scale();
        let scale_changed = dpi_changed || font_scale_changed;

        log::trace!(
            "dpi_changed={}, font_scale_changed={} scale_changed={} simple_dpi_change={}",
            dpi_changed,
            font_scale_changed,
            scale_changed,
            simple_dpi_change
        );

        let cell_dims = self.current_cell_dimensions();

        if dpi_changed {
            let old_dpi = self.dimensions.dpi;
            let new_dpi = dimensions.dpi;
            self.workspace_sidebar_width =
                rescale_ui_usize(self.workspace_sidebar_width, old_dpi, new_dpi);
            self.right_sidebar_width = rescale_ui_usize(self.right_sidebar_width, old_dpi, new_dpi);
            self.right_sidebar_file_tree_width =
                rescale_ui_usize(self.right_sidebar_file_tree_width, old_dpi, new_dpi);
            self.right_sidebar_file_preview_width =
                rescale_ui_usize(self.right_sidebar_file_preview_width, old_dpi, new_dpi);
        }

        if scale_changed {
            self.apply_scale_change(&dimensions, font_scale);
        }

        let scale_changed_cells = if font_scale_changed || simple_dpi_change {
            Some(cell_dims)
        } else {
            None
        };

        log::trace!(
            "scaling_changed, follow with applying dimensions. scale_changed_cells={:?}",
            scale_changed_cells
        );
        self.apply_dimensions(&dimensions, scale_changed_cells, window);
    }

    /// Used for applying font size changes only; this takes into account
    /// the `adjust_window_size_when_changing_font_size` configuration and
    /// revises the scaling/resize change accordingly
    pub fn adjust_font_scale(&mut self, font_scale: f64, window: &Window) {
        let adjust_window_size_when_changing_font_size =
            match self.config.adjust_window_size_when_changing_font_size {
                Some(value) => value,
                None => {
                    let is_tiling = self
                        .config
                        .tiling_desktop_environments
                        .iter()
                        .any(|item| item.as_str() == self.connection_name.as_str());
                    !is_tiling
                }
            };

        if self.window_state.can_resize() && adjust_window_size_when_changing_font_size {
            self.scaling_changed(self.dimensions, font_scale, window);
        } else {
            let dimensions = self.dimensions;
            // Compute new font metrics
            self.apply_scale_change(&dimensions, font_scale);
            // Now revise the pty size to fit the window
            self.apply_dimensions(&dimensions, None, window);
        }
    }

    fn adjust_active_pane_font_scale(&mut self, font_scale: f64) {
        let pane_id = match self.get_active_pane_no_overlay() {
            Some(pane) => pane.pane_id(),
            None => {
                if let Some(window) = self.window.as_ref().map(|w| w.clone()) {
                    self.adjust_font_scale(font_scale, &window);
                }
                return;
            }
        };

        let font_size = self.config.font_size * font_scale;
        let theoretical_height = font_size * self.dimensions.dpi as f64 / 72.0;

        if theoretical_height < 2.0 {
            log::warn!(
                "refusing to go to an unreasonably small pane font scale {:?}
                       font_scale={} would yield font_height {}",
                self.dimensions,
                font_scale,
                theoretical_height
            );
            return;
        }

        let global_scale = self.fonts.get_font_scale();
        {
            let mut state = self.pane_state(pane_id);
            state.font_scale = if font_scale.to_bits() == global_scale.to_bits() {
                None
            } else {
                Some(font_scale)
            };
        }
        self.persist_workspace_pane_font_scales();

        self.sync_pane_font_sizes();
        self.quad_generation += 1;
        self.shape_generation += 1;
        self.shape_cache.borrow_mut().clear();
        self.line_to_ele_shape_cache.borrow_mut().clear();
        self.invalidate_fancy_tab_bar();
        self.invalidate_modal();

        if let Some(window) = self.window.as_ref() {
            window.invalidate();
        }
    }

    pub fn decrease_font_size(&mut self) {
        if let Some(pane) = self.get_active_pane_no_overlay() {
            let font_scale = self.pane_font_scale(pane.pane_id());
            self.adjust_active_pane_font_scale(font_scale * (1.0 / 1.1));
        } else {
            self.pending_scale_changes
                .push_back(ScaleChange::Relative(1.0 / 1.1));
            self.apply_pending_scale_changes();
        }
    }

    pub fn increase_font_size(&mut self) {
        if let Some(pane) = self.get_active_pane_no_overlay() {
            let font_scale = self.pane_font_scale(pane.pane_id());
            self.adjust_active_pane_font_scale(font_scale * 1.1);
        } else {
            self.pending_scale_changes
                .push_back(ScaleChange::Relative(1.1));
            self.apply_pending_scale_changes();
        }
    }

    pub fn reset_font_size(&mut self) {
        if self.get_active_pane_no_overlay().is_some() {
            self.adjust_active_pane_font_scale(self.fonts.get_font_scale());
        } else {
            self.pending_scale_changes
                .push_back(ScaleChange::Absolute(1.0));
            self.apply_pending_scale_changes();
        }
    }

    pub fn set_window_size(&mut self, size: TerminalSize, window: &Window) -> anyhow::Result<()> {
        let config = &self.config;
        let fontconfig = Rc::new(FontConfiguration::new(
            Some(config.clone()),
            self.dimensions.dpi,
        )?);
        let render_metrics = RenderMetrics::new(&fontconfig)?;

        let terminal_size = TerminalSize {
            rows: size.rows,
            cols: size.cols,
            pixel_width: (render_metrics.cell_size.width as usize * size.cols),
            pixel_height: (render_metrics.cell_size.height as usize * size.rows),
            dpi: size.dpi,
        };

        let show_tab_bar = config.enable_tab_bar && !config.hide_tab_bar_if_only_one_tab;
        let tab_bar_height = if show_tab_bar {
            self.tab_bar_pixel_height()? as usize
        } else {
            0
        };

        let h_context = DimensionContext {
            dpi: self.dimensions.dpi as f32,
            pixel_max: self.dimensions.pixel_width as f32,
            pixel_cell: render_metrics.cell_size.width as f32,
        };
        let v_context = DimensionContext {
            dpi: self.dimensions.dpi as f32,
            pixel_max: self.dimensions.pixel_height as f32,
            pixel_cell: render_metrics.cell_size.height as f32,
        };
        let padding_left = config.window_padding.left.evaluate_as_pixels(h_context) as usize;
        let padding_top = config.window_padding.top.evaluate_as_pixels(v_context) as usize;
        let padding_bottom = config.window_padding.bottom.evaluate_as_pixels(v_context) as usize;

        let dimensions = Dimensions {
            pixel_width: ((terminal_size.cols as usize * render_metrics.cell_size.width as usize)
                + padding_left
                + effective_right_padding(&config, h_context)),
            pixel_height: ((terminal_size.rows as usize * render_metrics.cell_size.height as usize)
                + padding_top
                + padding_bottom) as usize
                + tab_bar_height,
            dpi: self.dimensions.dpi,
        };

        self.apply_scale_change(&dimensions, 1.0);
        self.apply_dimensions(
            &dimensions,
            Some(RowsAndCols {
                rows: size.rows as usize,
                cols: size.cols as usize,
            }),
            window,
        );
        Ok(())
    }

    pub fn reset_font_and_window_size(&mut self, window: &Window) -> anyhow::Result<()> {
        let size = self.config.initial_size(
            self.dimensions.dpi as u32,
            Some(crate::cell_pixel_dims(
                &self.config,
                self.dimensions.dpi as f64,
            )?),
        );
        self.set_window_size(size, window)
    }

    pub fn effective_right_padding(&self, config: &ConfigHandle) -> usize {
        effective_right_padding(
            config,
            DimensionContext {
                pixel_cell: self.render_metrics.cell_size.width as f32,
                dpi: self.dimensions.dpi as f32,
                pixel_max: self.dimensions.pixel_width as f32,
            },
        )
    }
}

/// Computes the effective padding for the RHS.
/// This is needed because the default is 0, but if the user has
/// enabled the scroll bar then they will expect it to have a reasonable
/// size unless they've specified differently.
pub fn effective_right_padding(config: &ConfigHandle, context: DimensionContext) -> usize {
    if config.enable_scroll_bar && config.window_padding.right.is_zero() {
        context.pixel_cell as usize
    } else {
        config.window_padding.right.evaluate_as_pixels(context) as usize
    }
}
