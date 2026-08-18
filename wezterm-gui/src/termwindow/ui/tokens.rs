pub const SIDEBAR_WIDTH_CELLS: usize = 26;
pub const SIDEBAR_MIN_WIDTH: usize = 254;
pub const SIDEBAR_MAX_WIDTH: usize = 520;
pub const SIDEBAR_INSET: usize = 10;
pub const SIDEBAR_ICON_GAP: usize = 10;
pub const SIDEBAR_ROW_GAP: usize = 10;
pub const SIDEBAR_ROW_RADIUS: f32 = 10.0;
pub const SIDEBAR_RESIZE_HANDLE_WIDTH: usize = 8;
/// Left-edge strip that arms a hover reveal of the collapsed sidebar.
///
/// Scaled through `ui_px`, not a raw pixel count: cell width scales with DPI
/// too, so scaling keeps the fraction of terminal column 0 the strip covers
/// (~a cell) the same on every display instead of shrinking to nothing
/// at 1x or swallowing several columns at 2x.
pub const SIDEBAR_HOVER_HOT_ZONE_WIDTH: usize = 12;
/// Once armed, the pointer only cancels the dwell by leaving THIS wider
/// strip. Entry stays precise; holding does not have to be. Without the
/// hysteresis a one-pixel wobble restarts the count and the reveal feels
/// like it needs surgical stillness.
pub const SIDEBAR_HOVER_STICKY_ZONE_WIDTH: usize = 28;
/// The visible arming hint: a thin strip at the very edge that appears while
/// the dwell counts down, so a correctly-parked pointer looks different from
/// a wrongly-parked one.
pub const SIDEBAR_HOVER_HINT_WIDTH: usize = 4;
/// How long the pointer must sit in the strip. Long enough that crossing the
/// edge on the way to a window control never triggers it.
pub const SIDEBAR_HOVER_DWELL_MS: u64 = 180;
/// How long a departed pointer has to come back before the panel leaves.
/// Generous on purpose: it is the only defence against a native macOS menu,
/// which sets no flag while open (see `ui/context_menu.rs`).
pub const SIDEBAR_HOVER_GRACE_MS: u64 = 400;
pub const MACOS_TITLEBAR_CONTENT_TOP_INSET: usize = 52;
pub const MACOS_TRAFFIC_LIGHT_CLEARANCE_WIDTH: usize = 164;
pub const MACOS_WINDOW_TAB_RESERVED_ACTION_SLOTS: usize = 1;

pub const TAB_ROW_START_PADDING: usize = 16;
pub const WINDOW_TAB_TOP_SPACER: usize = 4;
pub const WINDOW_TAB_ACTION_RESERVED_WIDTH: usize = 60;
pub const WINDOW_TAB_FULLSCREEN_SIDEBAR_BUTTON_X: usize = 16;
pub const WINDOW_TAB_FULLSCREEN_SIDEBAR_BUTTON_Y_OFFSET: usize = 4;
pub const WINDOW_TAB_FULLSCREEN_SIDEBAR_BUTTON_SIZE: usize = 52;
pub const WINDOW_TAB_FULLSCREEN_SIDEBAR_ICON_SIZE: usize = 46;
pub const WINDOW_TAB_FULLSCREEN_NEW_SESSION_Y_OFFSET: usize = 6;
pub const WINDOW_TAB_FULLSCREEN_NEW_SESSION_EXTRA_HEIGHT: usize = 6;
pub const WINDOW_TAB_LEADING_ACTION_BUTTON_SIZE: usize = 34;
pub const WINDOW_TAB_LEADING_ACTION_GAP: usize = 8;
pub const WINDOW_TAB_LEADING_ACTION_ICON_SIZE: usize = 26;
pub const WINDOW_TAB_GAP: usize = 18;
pub const WINDOW_TAB_RADIUS: f32 = 12.0;
pub const WINDOW_TAB_ADD_BUTTON_RADIUS: f32 = 999.0;
pub const TAB_CLOSE_HOVER_INSET: usize = 5;
pub const TAB_CLOSE_HOVER_RADIUS: f32 = 999.0;
pub const TAB_CLOSE_RIGHT_GAP: usize = 6;
pub const TAB_VERTICAL_PADDING: usize = 8;

pub const PANE_NAV_INSET: usize = 10;
pub const PANE_NAV_ICON_GAP: usize = 8;
pub const PANE_NAV_BUTTON_GAP: usize = 4;
pub const PANE_NAV_TAB_GAP: usize = 18;
pub const PANE_NAV_TAB_TOP_OFFSET: usize = 4;
pub const PANE_NAV_TAB_RADIUS: f32 = 12.0;
pub const PANE_NAV_ACTION_BUTTON_RADIUS: f32 = 999.0;
pub const PANE_DROP_PREVIEW_RADIUS: f32 = 10.0;

pub const CAPSULE_BORDER_WIDTH: f32 = 1.0;
pub const ICON_BUTTON_BORDER_WIDTH: f32 = 1.0;
