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
    pub sidebar_row_hover_bg: LinearRgba,
    pub sidebar_row_active_bg: LinearRgba,
    pub sidebar_row_active_border: LinearRgba,
    pub selected_bg: LinearRgba,
    pub text: LinearRgba,
    pub secondary_text: LinearRgba,
    pub muted_text: LinearRgba,
    pub selected_text: LinearRgba,
    pub scrollbar_thumb: LinearRgba,
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
                sidebar_row_active_bg: rgba(255, 255, 255, 0.78),
                sidebar_row_active_border: rgba(60, 60, 67, 0.18),
                selected_bg: rgb(0, 122, 255),
                text: rgb(28, 28, 30),
                secondary_text: rgb(72, 72, 74),
                muted_text: rgb(142, 142, 147),
                selected_text: rgb(255, 255, 255),
                scrollbar_thumb: rgba(60, 60, 67, 0.32),
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
                sidebar_row_hover_bg: rgba(255, 255, 255, 0.075),
                sidebar_row_active_bg: rgba(48, 48, 50, 0.98),
                sidebar_row_active_border: rgba(118, 118, 128, 0.22),
                selected_bg: rgb(58, 58, 60),
                text: rgb(242, 242, 247),
                secondary_text: rgb(199, 199, 204),
                muted_text: rgb(142, 142, 147),
                selected_text: rgb(255, 255, 255),
                scrollbar_thumb: rgba(142, 142, 147, 0.42),
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
    pub row_radius: f32,
    pub icon_size: f32,
    pub resize_handle_width: f32,
    pub scrollbar_width: f32,
}

impl Default for UiTokens {
    fn default() -> Self {
        Self {
            sidebar_min_width: 340.0,
            sidebar_max_width: 580.0,
            sidebar_default_width: 380.0,
            sidebar_padding: 28.0,
            row_height: 48.0,
            row_gap: 8.0,
            control_height: 56.0,
            control_radius: 12.0,
            row_radius: 9.0,
            icon_size: 26.0,
            resize_handle_width: 24.0,
            scrollbar_width: 5.0,
        }
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
