use crate::quad::TripleLayerQuadAllocator;
use crate::termwindow::render::RenderScreenLineParams;
use crate::termwindow::theme_aligned_tab_bar_colors_from_palette;
use crate::termwindow::ui::tokens::WINDOW_TAB_TOP_SPACER;
use crate::ui::scale_ui_usize;
use crate::utilsprites::RenderMetrics;
use config::ConfigHandle;
use mux::renderable::RenderableDimensions;
use wezterm_term::color::ColorAttribute;
use window::color::LinearRgba;

fn fancy_tab_bar_pixel_height(cell_height: usize, dpi: usize) -> usize {
    let cell_height = cell_height.max(1);
    // Keep a comfortable capsule while trimming a small amount of the
    // vertical whitespace around the tab text.
    cell_height * 2 + scale_ui_usize(WINDOW_TAB_TOP_SPACER, dpi)
}

#[cfg(test)]
mod tests {
    use super::fancy_tab_bar_pixel_height;

    #[test]
    fn fancy_height_scales_with_monitor_dpi() {
        let design_dpi = if cfg!(target_os = "macos") { 144 } else { 96 };
        let base = fancy_tab_bar_pixel_height(20, design_dpi);
        let doubled = fancy_tab_bar_pixel_height(40, design_dpi * 2);
        assert_eq!(doubled, base * 2);
    }

    #[test]
    fn fancy_height_keeps_compact_comfortable_proportion() {
        let dpi = if cfg!(target_os = "macos") { 144 } else { 96 };
        assert_eq!(fancy_tab_bar_pixel_height(20, dpi), 44);
        assert_eq!(
            fancy_tab_bar_pixel_height(30, dpi) - fancy_tab_bar_pixel_height(20, dpi),
            20
        );
    }
}

impl crate::TermWindow {
    pub fn paint_tab_bar(&mut self, layers: &mut TripleLayerQuadAllocator) -> anyhow::Result<()> {
        if self.config.use_fancy_tab_bar {
            let mut tab_bar_items = self.paint_fancy_tab_bar(layers)?;
            self.ui_items.append(&mut tab_bar_items);
            return Ok(());
        }

        let border = self.get_os_border();

        let palette = self.palette().clone();
        let tab_bar_height = self.tab_bar_pixel_height()?;
        let tab_bar_y = if self.config.tab_bar_at_bottom {
            ((self.dimensions.pixel_height as f32) - (tab_bar_height + border.bottom.get() as f32))
                .max(0.)
        } else {
            border.top.get() as f32
        };
        let tab_bar_x = self.tab_bar_left_edge();
        let tab_bar_width = self.dimensions.pixel_width.saturating_sub(tab_bar_x).max(1);

        // Register the tab bar location
        self.ui_items.append(&mut self.tab_bar.compute_ui_items(
            tab_bar_y as usize,
            self.render_metrics.cell_size.height as usize,
            self.render_metrics.cell_size.width as usize,
            tab_bar_x,
        ));

        let window_is_transparent =
            !self.window_background.is_empty() || self.config.window_background_opacity != 1.0;
        let gl_state = self.render_state.as_ref().unwrap();
        let white_space = gl_state.util_sprites.white_space.texture_coords();
        let filled_box = gl_state.util_sprites.filled_box.texture_coords();
        let tab_bar_colors = self
            .config
            .resolved_palette
            .tab_bar
            .as_ref()
            .cloned()
            .unwrap_or_else(|| theme_aligned_tab_bar_colors_from_palette(&palette));
        let tab_bar_bg = tab_bar_colors
            .background()
            .to_linear()
            .mul_alpha(self.config.window_background_opacity);
        self.filled_rectangle(
            layers,
            0,
            euclid::rect(
                tab_bar_x as f32,
                tab_bar_y,
                tab_bar_width as f32,
                tab_bar_height,
            ),
            tab_bar_bg,
        )?;
        let default_bg = palette
            .resolve_bg(ColorAttribute::Default)
            .to_linear()
            .mul_alpha(if window_is_transparent {
                0.
            } else {
                self.config.text_background_opacity
            });

        self.render_screen_line(
            RenderScreenLineParams {
                top_pixel_y: tab_bar_y,
                left_pixel_x: tab_bar_x as f32,
                pixel_width: tab_bar_width as f32,
                stable_line_idx: None,
                line: self.tab_bar.line(),
                selection: 0..0,
                cursor: &Default::default(),
                palette: &palette,
                dims: &RenderableDimensions {
                    cols: (tab_bar_width / self.render_metrics.cell_size.width as usize).max(1),
                    physical_top: 0,
                    scrollback_rows: 0,
                    scrollback_top: 0,
                    viewport_rows: 1,
                    dpi: self.terminal_size.dpi,
                    pixel_height: self.render_metrics.cell_size.height as usize,
                    pixel_width: tab_bar_width,
                    reverse_video: false,
                },
                config: &self.config,
                cursor_border_color: LinearRgba::default(),
                foreground: palette.foreground.to_linear(),
                pane: None,
                is_active: true,
                selection_fg: LinearRgba::default(),
                selection_bg: LinearRgba::default(),
                cursor_fg: LinearRgba::default(),
                cursor_bg: LinearRgba::default(),
                cursor_is_default_color: true,
                white_space,
                filled_box,
                window_is_transparent,
                default_bg,
                style: None,
                font: None,
                use_pixel_positioning: self.config.experimental_pixel_positioning,
                render_metrics: self.render_metrics,
                font_config: None,
                font_identity: self.fonts.get_font_scale().to_bits(),
                shape_key: None,
                password_input: false,
            },
            layers,
        )?;

        Ok(())
    }

    pub fn tab_bar_pixel_height_impl(
        config: &ConfigHandle,
        fontconfig: &wezterm_font::FontConfiguration,
        render_metrics: &RenderMetrics,
    ) -> anyhow::Result<f32> {
        if config.use_fancy_tab_bar {
            // Fancy tabs use their own UI font. Basing their height on the
            // terminal font made the chrome unexpectedly tall whenever the
            // terminal font was enlarged, even though the tab text stayed at
            // its configured size.
            let font = fontconfig.title_font_with_size(crate::native_settings::tab_font_size())?;
            let tab_metrics = RenderMetrics::with_font_metrics(&font.metrics());
            Ok(fancy_tab_bar_pixel_height(
                tab_metrics.cell_size.height.max(1) as usize,
                fontconfig.get_dpi(),
            ) as f32)
        } else {
            Ok(render_metrics.cell_size.height as f32)
        }
    }

    pub fn tab_bar_pixel_height(&self) -> anyhow::Result<f32> {
        Self::tab_bar_pixel_height_impl(&self.config, &self.fonts, &self.render_metrics)
    }
}
