use window::color::LinearRgba;
use window::Appearance;

#[derive(Debug, Clone, Copy)]
pub(crate) struct UiPalette {
    pub window_bg: LinearRgba,
    pub sidebar_bg: LinearRgba,
    pub workspace_sidebar_bg: LinearRgba,
    pub header_bg: LinearRgba,
    pub separator: LinearRgba,
    pub control_bg: LinearRgba,
    pub control_hover_bg: LinearRgba,
    pub control_pressed_bg: LinearRgba,
    pub control_border: LinearRgba,
    pub sidebar_button_bg: LinearRgba,
    pub sidebar_button_hover_bg: LinearRgba,
    /// The sidebar row ramp. These three are ordered on purpose --
    /// hover < pressed < active -- because a selected row is a state and the
    /// other two are momentary feedback; feedback that outweighs the state
    /// makes the row under the pointer look more current than the one that is.
    /// Write them as concrete colors, never as a white wash: a wash composites
    /// in linear space, so 7.5% white over the dark bar landed at rgb(80) and
    /// silently jumped the whole ramp.
    pub sidebar_row_hover_bg: LinearRgba,
    pub sidebar_row_pressed_bg: LinearRgba,
    pub sidebar_row_active_bg: LinearRgba,
    pub sidebar_row_active_border: LinearRgba,
    pub selected_bg: LinearRgba,
    /// The one saturated color in the chrome. Everything else is a neutral,
    /// so this is what a selected row, an active switch or a primary button
    /// uses to say "this one". Kept identical in shape across platforms --
    /// the whole UI is drawn from these tokens, so one edit moves every OS.
    pub accent: LinearRgba,
    pub accent_hover: LinearRgba,
    /// Text and glyphs sitting on top of `accent`.
    pub on_accent: LinearRgba,
    /// Irreversible actions. Used as a label/border tint rather than a fill,
    /// so a destructive button still reads as a button and not as an alert.
    pub danger: LinearRgba,
    /// The off half of a switch track. Distinct from `control_border`, which
    /// is a hairline colour and disappears when used as a filled track.
    pub track_off: LinearRgba,
    /// Fill for a grouped card floating on `window_bg`. Translucent on
    /// purpose: it picks up whatever the page paints behind it.
    pub card_bg: LinearRgba,
    pub text: LinearRgba,
    pub secondary_text: LinearRgba,
    pub muted_text: LinearRgba,
    pub selected_text: LinearRgba,
    pub scrollbar_thumb: LinearRgba,
    pub spelling_error: LinearRgba,
}

impl UiPalette {
    pub(crate) fn for_appearance(appearance: Appearance) -> Self {
        match appearance {
            Appearance::Light | Appearance::LightHighContrast => Self {
                window_bg: rgb(238, 238, 242),
                sidebar_bg: rgb(238, 238, 242),
                workspace_sidebar_bg: rgb(226, 226, 232),
                header_bg: rgb(238, 238, 242),
                separator: rgba(60, 60, 67, 0.18),
                control_bg: rgba(255, 255, 255, 0.94),
                control_hover_bg: rgba(247, 247, 250, 0.98),
                control_pressed_bg: rgba(232, 242, 255, 0.98),
                control_border: rgba(60, 60, 67, 0.20),
                sidebar_button_bg: rgba(255, 255, 255, 0.72),
                sidebar_button_hover_bg: rgba(255, 255, 255, 0.94),
                sidebar_row_hover_bg: rgba(60, 60, 67, 0.08),
                sidebar_row_pressed_bg: rgba(60, 60, 67, 0.16),
                sidebar_row_active_bg: rgba(255, 255, 255, 0.78),
                sidebar_row_active_border: rgba(60, 60, 67, 0.18),
                selected_bg: rgb(0, 122, 255),
                accent: rgb(0, 122, 255),
                accent_hover: rgb(0, 106, 224),
                on_accent: rgb(255, 255, 255),
                danger: rgb(215, 38, 61),
                track_off: rgba(220, 220, 226, 1.0),
                card_bg: rgba(255, 255, 255, 0.72),
                text: rgb(28, 28, 30),
                secondary_text: rgb(72, 72, 74),
                muted_text: rgb(142, 142, 147),
                selected_text: rgb(255, 255, 255),
                scrollbar_thumb: rgba(60, 60, 67, 0.32),
                spelling_error: rgb(215, 38, 61),
            },
            Appearance::Dark | Appearance::DarkHighContrast => Self {
                window_bg: rgb(25, 25, 26),
                sidebar_bg: rgb(25, 25, 26),
                workspace_sidebar_bg: rgb(18, 18, 20),
                header_bg: rgb(25, 25, 26),
                separator: rgba(84, 84, 88, 0.22),
                control_bg: rgba(45, 45, 47, 0.98),
                control_hover_bg: rgba(55, 55, 57, 0.98),
                control_pressed_bg: rgba(66, 66, 69, 0.98),
                control_border: rgba(118, 118, 128, 0.28),
                sidebar_button_bg: rgba(36, 36, 38, 0.94),
                sidebar_button_hover_bg: rgba(48, 48, 50, 0.98),
                sidebar_row_hover_bg: rgba(42, 42, 44, 0.98),
                sidebar_row_pressed_bg: rgba(52, 52, 55, 0.98),
                sidebar_row_active_bg: rgba(62, 62, 65, 0.98),
                sidebar_row_active_border: rgba(118, 118, 128, 0.22),
                selected_bg: rgb(58, 58, 60),
                accent: rgb(10, 132, 255),
                accent_hover: rgb(50, 152, 255),
                on_accent: rgb(255, 255, 255),
                danger: rgb(255, 69, 58),
                track_off: rgba(78, 78, 82, 1.0),
                card_bg: rgba(30, 30, 32, 0.78),
                text: rgb(242, 242, 247),
                secondary_text: rgb(199, 199, 204),
                muted_text: rgb(142, 142, 147),
                selected_text: rgb(255, 255, 255),
                scrollbar_thumb: rgba(142, 142, 147, 0.42),
                spelling_error: rgb(255, 69, 58),
            },
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct UiTokens {
    pub sidebar_min_width: f32,
    pub sidebar_max_width: f32,
    pub sidebar_default_width: f32,
    pub sidebar_padding: f32,
    pub row_height: f32,
    pub row_gap: f32,
    pub control_height: f32,
    pub control_radius: f32,
    /// Grouped-card corner radius.
    pub card_radius: f32,
    pub row_radius: f32,
    pub icon_size: f32,
    pub resize_handle_width: f32,
    pub scrollbar_width: f32,
    /// Gap between the scrollbar and the right edge of its area.
    pub scrollbar_inset: f32,
    /// Gap above and below the scrollbar track inside its area.
    pub scrollbar_margin_y: f32,
    /// Shortest the scrollbar thumb is allowed to get.
    pub scrollbar_min_thumb: f32,
}

impl Default for UiTokens {
    fn default() -> Self {
        Self {
            sidebar_min_width: 340.0,
            sidebar_max_width: 580.0,
            sidebar_default_width: 440.0,
            sidebar_padding: 28.0,
            row_height: 48.0,
            row_gap: 8.0,
            control_height: 56.0,
            control_radius: 12.0,
            card_radius: 36.0,
            row_radius: 9.0,
            icon_size: 26.0,
            resize_handle_width: 24.0,
            scrollbar_width: 5.0,
            scrollbar_inset: 4.0,
            scrollbar_margin_y: 8.0,
            scrollbar_min_thumb: 32.0,
        }
    }
}

impl UiTokens {
    /// Scale ThinkTerm's custom chrome from its original Retina pixel grid to
    /// the current window's backing scale. Text already follows the window
    /// DPI; applying the same ratio to controls keeps their size in points
    /// stable when a macOS window moves between Retina and non-Retina screens.
    pub(crate) fn for_dpi(dpi: usize) -> Self {
        let scale = ui_scale_for_dpi(dpi);
        let base = Self::default();
        Self {
            sidebar_min_width: base.sidebar_min_width * scale,
            sidebar_max_width: base.sidebar_max_width * scale,
            sidebar_default_width: base.sidebar_default_width * scale,
            sidebar_padding: base.sidebar_padding * scale,
            row_height: base.row_height * scale,
            row_gap: base.row_gap * scale,
            control_height: base.control_height * scale,
            control_radius: base.control_radius * scale,
            card_radius: base.card_radius * scale,
            row_radius: base.row_radius * scale,
            icon_size: base.icon_size * scale,
            resize_handle_width: base.resize_handle_width * scale,
            scrollbar_width: base.scrollbar_width * scale,
            scrollbar_inset: base.scrollbar_inset * scale,
            scrollbar_margin_y: base.scrollbar_margin_y * scale,
            scrollbar_min_thumb: base.scrollbar_min_thumb * scale,
        }
    }
}

/// ThinkTerm's custom chrome is authored in 2x macOS backing pixels on every
/// platform. Convert those values to the current monitor's backing-pixel
/// grid: on macOS a 2x surface reports dpi 144, so that is the design dpi;
/// elsewhere windows report logical dpi, so the design maps to 192 — a
/// 96dpi/100% display renders at 0.5, Windows 150% (144dpi) at 0.75 and
/// 200% (192dpi) at 1.0, all matching the macOS logical proportions.
pub(crate) fn ui_scale_for_dpi(dpi: usize) -> f32 {
    let design_dpi = if cfg!(target_os = "macos") {
        144.0
    } else {
        192.0
    };
    (dpi.max(1) as f32 / design_dpi).clamp(0.25, 4.0)
}

pub(crate) fn scale_ui_usize(value: usize, dpi: usize) -> usize {
    if value == 0 {
        0
    } else {
        ((value as f32 * ui_scale_for_dpi(dpi)).round() as usize).max(1)
    }
}

pub(crate) fn scale_ui_f32(value: f32, dpi: usize) -> f32 {
    value * ui_scale_for_dpi(dpi)
}

pub(crate) fn unscale_ui_usize(value: usize, dpi: usize) -> usize {
    if value == 0 {
        0
    } else {
        ((value as f32 / ui_scale_for_dpi(dpi)).round() as usize).max(1)
    }
}

pub(crate) fn rescale_ui_usize(value: usize, old_dpi: usize, new_dpi: usize) -> usize {
    if value == 0 {
        0
    } else {
        ((value as f32 * ui_scale_for_dpi(new_dpi) / ui_scale_for_dpi(old_dpi)).round() as usize)
            .max(1)
    }
}

fn rgb(red: u8, green: u8, blue: u8) -> LinearRgba {
    rgba(red, green, blue, 1.0)
}

fn rgba(red: u8, green: u8, blue: u8, alpha: f32) -> LinearRgba {
    let mut color = LinearRgba::with_srgba(red, green, blue, 255);
    color.3 = alpha;
    color
}

#[cfg(test)]
mod tests {
    use super::*;

    fn design_dpi() -> usize {
        if cfg!(target_os = "macos") {
            144
        } else {
            192
        }
    }

    #[test]
    fn ui_pixels_follow_monitor_dpi() {
        let design_dpi = design_dpi();
        assert_eq!(ui_scale_for_dpi(design_dpi), 1.0);
        assert_eq!(scale_ui_usize(40, design_dpi / 2), 20);
        assert_eq!(scale_ui_usize(40, design_dpi * 2), 80);
    }

    #[test]
    fn ui_widths_round_trip_through_design_pixels() {
        let design_dpi = design_dpi();
        let monitor_dpi = design_dpi / 2;
        let scaled = scale_ui_usize(380, monitor_dpi);
        assert_eq!(unscale_ui_usize(scaled, monitor_dpi), 380);
        assert_eq!(rescale_ui_usize(80, design_dpi, monitor_dpi), 40);
    }
}
