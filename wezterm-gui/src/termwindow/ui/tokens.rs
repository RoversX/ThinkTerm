use window::color::LinearRgba;

pub const SIDEBAR_WIDTH_CELLS: usize = 26;
pub const SIDEBAR_MIN_WIDTH: usize = 224;
pub const SIDEBAR_MAX_WIDTH: usize = 520;
pub const SIDEBAR_INSET: usize = 10;
pub const SIDEBAR_ICON_GAP: usize = 10;
pub const SIDEBAR_ROW_GAP: usize = 10;
pub const SIDEBAR_ROW_RADIUS: f32 = 10.0;
pub const SIDEBAR_RESIZE_HANDLE_WIDTH: usize = 8;

pub const TAB_ROW_START_PADDING: usize = 16;
pub const WINDOW_TAB_ACTION_RESERVED_WIDTH: usize = 48;
pub const WINDOW_TAB_GAP: usize = 18;
pub const WINDOW_TAB_RADIUS: f32 = 12.0;
pub const TAB_CLOSE_HOVER_INSET: usize = 5;
pub const TAB_CLOSE_HOVER_RADIUS: f32 = 999.0;
pub const TAB_CLOSE_RIGHT_GAP: usize = 6;
pub const TAB_VERTICAL_PADDING: usize = 8;

pub const PANE_NAV_INSET: usize = 10;
pub const PANE_NAV_ICON_GAP: usize = 8;
pub const PANE_NAV_BUTTON_GAP: usize = 4;
pub const PANE_NAV_TAB_GAP: usize = 18;
pub const PANE_NAV_EXTRA_HEIGHT: usize = 32;
pub const PANE_NAV_MIN_HEIGHT: usize = 62;
pub const PANE_NAV_MAX_HEIGHT: usize = 72;
pub const PANE_NAV_TAB_RADIUS: f32 = 12.0;

pub const ACTIVE_CAPSULE_CHANNEL: f32 = 8.0 / 255.0;
pub const CAPSULE_BORDER_WIDTH: f32 = 1.0;

pub fn active_capsule_bg(alpha: f32) -> LinearRgba {
    LinearRgba::with_components(
        ACTIVE_CAPSULE_CHANNEL,
        ACTIVE_CAPSULE_CHANNEL,
        ACTIVE_CAPSULE_CHANNEL,
        alpha.clamp(0.0, 1.0),
    )
}

pub fn capsule_border(foreground: LinearRgba, active: bool) -> LinearRgba {
    foreground.mul_alpha(if active { 0.24 } else { 0.10 })
}
