use crate::quad::TripleLayerQuadAllocator;
use crate::termwindow::render::RenderScreenLineParams;
use crate::termwindow::ui::tokens::WINDOW_TAB_TOP_SPACER;
use crate::ui::scale_ui_usize;
use crate::utilsprites::RenderMetrics;
use mux::renderable::RenderableDimensions;
use mux::tab::PositionedPane;
use window::color::LinearRgba;
use window::RectF;

fn fancy_tab_bar_pixel_height(cell_height: usize, dpi: usize) -> usize {
    crate::termwindow::ui::tokens::tab_row_height_for_cell(cell_height)
        + scale_ui_usize(WINDOW_TAB_TOP_SPACER, dpi)
}

#[cfg(test)]
mod tests {
    use super::fancy_tab_bar_pixel_height;

    #[test]
    fn fancy_height_scales_with_monitor_dpi() {
        let design_dpi = if cfg!(target_os = "macos") { 144 } else { 192 };
        let base = fancy_tab_bar_pixel_height(20, design_dpi);
        let doubled = fancy_tab_bar_pixel_height(40, design_dpi * 2);
        assert_eq!(doubled, base * 2);
    }

    #[test]
    fn fancy_height_keeps_compact_comfortable_proportion() {
        let dpi = if cfg!(target_os = "macos") { 144 } else { 192 };
        assert_eq!(fancy_tab_bar_pixel_height(20, dpi), 49);
        assert_eq!(
            fancy_tab_bar_pixel_height(30, dpi) - fancy_tab_bar_pixel_height(20, dpi),
            22
        );
    }
}

/// Height of the terminal bar while it shows: one terminal row.
pub(crate) fn terminal_bar_height(
    kind: Option<crate::tabbar::TabBarKind>,
    render_metrics: &RenderMetrics,
) -> usize {
    kind.map_or(0, |_| render_metrics.cell_size.height.max(0) as usize)
}

/// Where the terminal bar sits while it shows: the bar wezterm
/// configurations draw, which lives inside the terminal area.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct TerminalBarPlacement {
    pub at_bottom: bool,
    pub height: f32,
}

impl crate::TermWindow {
    /// ThinkTerm's window tab row. The wezterm tab bar options configure the
    /// terminal bar instead (see `tabbar::TabBarKind`).
    pub fn paint_tab_bar(&mut self, layers: &mut TripleLayerQuadAllocator) -> anyhow::Result<()> {
        let mut tab_bar_items = self.paint_fancy_tab_bar(layers)?;
        self.ui_items.append(&mut tab_bar_items);
        Ok(())
    }

    pub(crate) fn terminal_bar_placement(&self) -> Option<TerminalBarPlacement> {
        self.terminal_bar_kind?;
        Some(TerminalBarPlacement {
            at_bottom: self.config.tab_bar_at_bottom,
            height: terminal_bar_height(self.terminal_bar_kind, &self.render_metrics) as f32,
        })
    }

    /// Pixels the window tab row and the terminal bar take above and below
    /// the pane tree.
    pub(crate) fn pane_area_insets(&self) -> (f32, f32) {
        let window_tabs = self.tab_bar_pixel_height().unwrap_or(0.);
        match self.terminal_bar_placement() {
            Some(bar) if bar.at_bottom => (window_tabs, bar.height),
            Some(bar) => (window_tabs + bar.height, 0.),
            None => (window_tabs, 0.),
        }
    }

    /// Where the bottom-most panes' backgrounds end: the window's bottom
    /// edge, or the top of a terminal bar along it -- a bar that leaves its
    /// background to the window (bar.wezterm's is "transparent") must not
    /// show the panes through it.
    pub(crate) fn pane_area_bottom(&self) -> f32 {
        let window_bottom = self.dimensions.pixel_height as f32;
        match self.terminal_bar_placement() {
            Some(bar) if bar.at_bottom => {
                (window_bottom - self.get_os_border().bottom.get() as f32 - bar.height).max(0.)
            }
            _ => window_bottom,
        }
    }

    /// How far above its pane the nav bar of a pane on the terminal area's
    /// top edge is drawn. A terminal bar at the top belongs under the nav
    /// bars, beside the terminal: the pane tree makes room for it, and these
    /// nav bars move up into that room.
    pub(crate) fn pane_nav_lift(&self, pos: &PositionedPane) -> f32 {
        match self.terminal_bar_placement() {
            Some(bar) if !bar.at_bottom && pos.top == 0 => bar.height,
            _ => 0.,
        }
    }

    /// The terminal bar's rectangle: across the terminal area, under the top
    /// panes' nav bars or along its bottom edge.
    pub(crate) fn terminal_bar_rect(&self) -> Option<RectF> {
        let bar = self.terminal_bar_placement()?;
        let border = self.get_os_border();
        let left = self.tab_bar_left_edge() as f32;
        let width = (self.terminal_viewport_right() - left).max(1.);
        let y = if bar.at_bottom {
            self.pane_area_bottom()
        } else {
            let (top, _) = self.pane_area_insets();
            border.top.get() as f32 + top - bar.height + self.pane_nav_bar_height() as f32
        };
        Some(euclid::rect(left, y, width, bar.height))
    }

    pub fn paint_terminal_bar(
        &mut self,
        layers: &mut TripleLayerQuadAllocator,
    ) -> anyhow::Result<()> {
        let Some(rect) = self.terminal_bar_rect() else {
            return Ok(());
        };
        let palette = self.palette().clone();
        let tab_bar_height = rect.size.height;
        let tab_bar_y = rect.origin.y;
        let tab_bar_x = rect.origin.x as usize;
        let tab_bar_width = rect.size.width as usize;

        // Cut to the bar: a left status wider than it lays its items past
        // the right edge, over the right sidebar, whose clicks they would
        // take -- they are registered after it.
        let bar_right = tab_bar_x + tab_bar_width;
        let items = self.terminal_bar.compute_ui_items(
            tab_bar_y as usize,
            self.render_metrics.cell_size.height as usize,
            self.render_metrics.cell_size.width as usize,
            tab_bar_x,
        );
        self.ui_items
            .extend(items.into_iter().filter_map(|mut item| {
                item.width = item.width.min(bar_right.saturating_sub(item.x));
                (item.width > 0).then_some(item)
            }));

        let window_is_transparent =
            !self.window_background.is_empty() || self.config.window_background_opacity != 1.0;
        let terminal_bg = self.terminal_default_background();
        // Without colours of its own the bar is on the terminal's ground.
        let tab_bar_bg = match self.config.resolved_palette.tab_bar.as_ref() {
            Some(colors) => colors.background().to_linear(),
            None => terminal_bg,
        }
        .mul_alpha(self.config.window_background_opacity);
        let gl_state = self.render_state.as_ref().unwrap();
        let white_space = gl_state.util_sprites.white_space.texture_coords();
        let filled_box = gl_state.util_sprites.filled_box.texture_coords();
        // Layer 0, like the cell backgrounds drawn over it; painted after the
        // panes, so it still covers what it crosses of theirs.
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
        let default_bg = terminal_bg.mul_alpha(if window_is_transparent {
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
                line: self.terminal_bar.line(),
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
                allow_images: true,
                simple_shaping: false,
            },
            layers,
        )?;

        Ok(())
    }

    /// Height of the window tab row.
    pub fn tab_bar_pixel_height_impl(
        fontconfig: &wezterm_font::FontConfiguration,
    ) -> anyhow::Result<f32> {
        // The row uses its own UI font. Basing its height on the terminal
        // font made the chrome unexpectedly tall whenever the terminal font
        // was enlarged, even though the tab text stayed at its configured
        // size.
        let font = fontconfig.title_font_with_size(crate::native_settings::tab_font_size())?;
        let tab_metrics = RenderMetrics::with_font_metrics(&font.metrics());
        Ok(fancy_tab_bar_pixel_height(
            tab_metrics.cell_size.height.max(1) as usize,
            fontconfig.get_dpi(),
        ) as f32)
    }

    pub fn tab_bar_pixel_height(&self) -> anyhow::Result<f32> {
        Self::tab_bar_pixel_height_impl(&self.fonts)
    }
}
