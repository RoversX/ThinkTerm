use window::color::LinearRgba;

pub const SIDEBAR_WIDTH_CELLS: usize = 26;
pub const SIDEBAR_MIN_WIDTH: usize = 224;
pub const SIDEBAR_MAX_WIDTH: usize = 520;
pub const SIDEBAR_INSET: usize = 10;
pub const SIDEBAR_ICON_GAP: usize = 10;
pub const SIDEBAR_ROW_RADIUS: f32 = 6.0;
pub const SIDEBAR_RESIZE_HANDLE_WIDTH: usize = 8;

pub const PANE_NAV_INSET: usize = 6;
pub const PANE_NAV_ICON_GAP: usize = 8;
pub const PANE_NAV_BUTTON_GAP: usize = 4;
pub const PANE_NAV_EXTRA_HEIGHT: usize = 12;
pub const PANE_NAV_MIN_HEIGHT: usize = 42;
pub const PANE_NAV_MAX_HEIGHT: usize = 48;
pub const PANE_NAV_TAB_MIN_WIDTH: usize = 180;
pub const PANE_NAV_TAB_MAX_WIDTH: usize = 220;
pub const PANE_NAV_TAB_RADIUS: f32 = 5.0;

pub fn mix(a: LinearRgba, b: LinearRgba, amount: f32) -> LinearRgba {
    let amount = amount.clamp(0.0, 1.0);
    let inverse = 1.0 - amount;
    LinearRgba::with_components(
        a.0 * inverse + b.0 * amount,
        a.1 * inverse + b.1 * amount,
        a.2 * inverse + b.2 * amount,
        a.3 * inverse + b.3 * amount,
    )
}
