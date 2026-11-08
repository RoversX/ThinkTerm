pub mod agent_panel;
pub mod command_palette;
pub mod context_menu;
pub mod folder_problem;
pub mod icons;
pub mod platform_chrome;
pub mod plugin_panel;
pub mod right_sidebar;
pub mod recording_overlay;
pub mod sidebar;
pub mod library;
pub mod sidebar_jump;
pub mod status_icon;
pub mod tokens;

use crate::utilsprites::RenderMetrics;

pub use right_sidebar::{
    right_sidebar_file_preview_width, right_sidebar_note_pane_width_for_dpi,
    right_sidebar_width_for_metrics,
};
pub use sidebar::workspace_sidebar_width_for_metrics;

pub fn pane_nav_bar_height_for_metrics(render_metrics: RenderMetrics) -> usize {
    // Match the primary tab content height so both tab levels keep the same
    // vertical rhythm on every monitor DPI.
    let cell_height = render_metrics.cell_size.height.max(1) as usize;
    crate::termwindow::ui::tokens::tab_row_height_for_cell(cell_height)
}

impl crate::TermWindow {
    /// Record a tab's capsule at the tab's own origin -- left edge at x = 0 --
    /// so the caller can replay it clipped to the row.
    ///
    /// Recording it at full width is what keeps a cut tab looking like a cut
    /// tab. `snapped_rounded_corner_radius` clamps the radius to half the rect
    /// it is handed, so a capsule drawn pre-cut turned a 30px remnant of a
    /// 320px pill into its own little lozenge. At full width the radius is the
    /// one every other tab gets, and the clip supplies the hard edge.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn paint_tab_capsule(
        &self,
        layers: &mut crate::quad::TripleLayerQuadAllocator,
        layer_num: usize,
        y: usize,
        width: usize,
        height: usize,
        radius: f32,
        fill: ::window::color::LinearRgba,
        border: ::window::color::LinearRgba,
        active: bool,
    ) -> anyhow::Result<()> {
        let rect = euclid::rect(0.0, y as f32, width as f32, height as f32);
        // Over a see-through window a tab, as a sidebar row, lets some of
        // what is behind it through: no shadow under it to darken that, and
        // its edge a ring round the fill rather than a coat under it.
        if self.chrome_see_through() {
            use crate::termwindow::ui::tokens::{
                SEE_THROUGH_BORDER_BOOST, SEE_THROUGH_SURFACE_ALPHA,
            };
            let Some(gl_state) = self.render_state.as_ref() else {
                return Ok(());
            };
            let border = if active {
                border.mul_alpha(SEE_THROUGH_BORDER_BOOST.min(1.0 / border.3.max(f32::EPSILON)))
            } else {
                border
            };
            return crate::ui::draw::DrawContext::new(gl_state, self.dimensions, &self.render_metrics)
                .draw_rounded_frame(
                    layers,
                    layer_num,
                    rect.min_x(),
                    rect.min_y(),
                    rect.width(),
                    rect.height(),
                    fill.mul_alpha(SEE_THROUGH_SURFACE_ALPHA),
                    border,
                    radius,
                );
        }
        if active {
            self.paint_active_surface_shadow(layers, layer_num, rect, radius)?;
        }
        self.fill_rounded_rectangle_with_border(
            layers,
            layer_num,
            rect,
            fill,
            border,
            radius,
            crate::termwindow::ui::tokens::CAPSULE_BORDER_WIDTH,
        )
    }

    /// A short drop shadow under a selected surface — an active tab, a selected
    /// sidebar row — so it reads as sitting on its bar rather than merely being
    /// a different colour.
    ///
    /// Two tight strips, far shorter than `draw_elevated_surface`'s card
    /// shadow: the tab pill clears the pane nav bar's bottom edge by only ~4px,
    /// and anything softer would spill a dark line onto the terminal below.
    /// Offset downward, so the lift comes from the asymmetry rather than from
    /// depth the dark palette has no room for.
    ///
    /// Draw before the surface, on the surface's own layer. The halo is
    /// inflated past the surface, so a caller whose surface can be cut by a
    /// viewport has to record both into the same clipped buffer -- otherwise
    /// the halo escapes past the boundary the surface stops at.
    pub(crate) fn paint_active_surface_shadow(
        &self,
        layers: &mut crate::quad::TripleLayerQuadAllocator,
        layer_num: usize,
        rect: ::window::RectF,
        radius: f32,
    ) -> anyhow::Result<()> {
        // The bar underneath decides what a given alpha is worth. Black at 0.28
        // moves a dark bar (rgb 25) by 7 levels but a light one (rgb 238) by
        // 66 — nine times as far — so one ramp cannot serve both: tuned for
        // dark it bruises the light bar, tuned for light it vanishes on dark.
        let (outer, inner) = match crate::native_settings::effective_appearance() {
            ::window::Appearance::Light | ::window::Appearance::LightHighContrast => (0.04, 0.05),
            ::window::Appearance::Dark | ::window::Appearance::DarkHighContrast => (0.12, 0.18),
        };
        for (spread, offset_y, alpha) in [
            (self.ui_f32(2.0), self.ui_f32(1.0), outer),
            (self.ui_f32(1.0), self.ui_f32(1.0), inner),
        ] {
            self.fill_rounded_rectangle(
                layers,
                layer_num,
                euclid::rect(
                    rect.min_x() - spread,
                    rect.min_y() - spread + offset_y,
                    rect.width() + spread * 2.0,
                    rect.height() + spread * 2.0,
                ),
                ::window::color::LinearRgba::with_components(0.0, 0.0, 0.0, alpha),
                radius + spread,
            )?;
        }
        Ok(())
    }

    /// Left edge of a tab's close-button box, placed so the glyph inside it
    /// lands `TAB_CONTENT_INSET` from the tab's right edge — the same inset the
    /// leading icon keeps on the left. Derived rather than a fixed gap so the
    /// two stay symmetric whatever the icon and button sizes work out to.
    pub(crate) fn tab_close_button_x(
        &self,
        tab_right: usize,
        button_size: usize,
        icon_size: usize,
    ) -> usize {
        let glyph_pad = button_size.saturating_sub(icon_size) / 2;
        // Optical, not geometric. Lucide's `x` inks only the middle half of its
        // 24-unit box (6..18) where `square-terminal` inks three quarters
        // (3..21), so boxing the two at the same inset leaves the X looking
        // further from the tab's right edge than the icon looks from its left.
        // Nudge the button out by that difference — an eighth of the icon — and
        // the two glyphs' ink lands the same distance from their own edges.
        let optical = icon_size / 8;
        let inset = self
            .ui_px(crate::termwindow::ui::tokens::TAB_CONTENT_INSET)
            .saturating_sub(optical);
        tab_right.saturating_sub(inset + icon_size + glyph_pad)
    }

    pub(crate) fn pane_nav_bar_height(&self) -> usize {
        let metrics = self
            .fonts
            .title_font_with_size(crate::native_settings::pane_header_font_size(
                &crate::native_settings::load_shared(),
            ))
            .map(|font| RenderMetrics::with_font_metrics(&font.metrics()))
            .unwrap_or(self.render_metrics);
        pane_nav_bar_height_for_metrics(metrics)
    }
}

pub fn terminal_title_for_display(title: &str) -> &str {
    let title = title.trim();
    if title.is_empty() || is_default_shell_title(title) {
        "Terminal"
    } else {
        title
    }
}

/// Whether a process or title names a shell rather than something a shell is
/// running. A shell sitting at its prompt is the absence of activity, so
/// surfaces that report "what is this terminal doing" say nothing at all
/// rather than saying `fish` on every idle card.
pub(crate) fn is_default_shell_title(title: &str) -> bool {
    let title = title.rsplit(['/', '\\']).next().unwrap_or(title);
    let title = title.to_ascii_lowercase();
    // Windows reports the executable, extension and all, so an idle prompt
    // arrives here as `pwsh.exe` and matches nothing -- which reads to the
    // caller as "this terminal is running a program called pwsh.exe".
    let title = title.strip_suffix(".exe").unwrap_or(title.as_str());
    matches!(
        title,
        "zsh"
            | "bash"
            | "sh"
            | "dash"
            | "ksh"
            | "csh"
            | "tcsh"
            | "fish"
            | "nu"
            | "nushell"
            | "elvish"
            | "xonsh"
            | "pwsh"
            | "powershell"
            | "cmd"
    )
}

#[cfg(test)]
mod default_shell_title_tests {
    use super::is_default_shell_title;

    /// A shell sitting at its prompt is not "running" anything worth naming.
    #[test]
    fn a_bare_shell_is_not_a_running_program() {
        for shell in ["zsh", "bash", "fish", "nu", "pwsh", "cmd", "dash", "tcsh"] {
            assert!(is_default_shell_title(shell), "{shell}");
        }
    }

    /// Windows reports the executable, extension and all. Without stripping it
    /// an idle prompt was announcing itself as a program called `pwsh.exe`.
    #[test]
    fn windows_executables_are_recognised_with_their_extension() {
        for shell in ["pwsh.exe", "powershell.exe", "cmd.exe", "CMD.EXE"] {
            assert!(is_default_shell_title(shell), "{shell}");
        }
    }

    #[test]
    fn a_full_path_is_matched_on_its_last_component() {
        assert!(is_default_shell_title("/bin/zsh"));
        assert!(is_default_shell_title(
            r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe"
        ));
    }

    /// The point of all this: something the user actually started must survive.
    #[test]
    fn an_actual_program_is_still_named() {
        for program in ["vim", "htop", "node", "cargo", "claude", "zshrc", "bashful"] {
            assert!(!is_default_shell_title(program), "{program}");
        }
    }
}
