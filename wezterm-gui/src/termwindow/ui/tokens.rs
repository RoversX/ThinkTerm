pub const SIDEBAR_WIDTH_CELLS: usize = 26;
pub const SIDEBAR_MIN_WIDTH: usize = 254;
pub const SIDEBAR_MAX_WIDTH: usize = 520;
pub const SIDEBAR_INSET: usize = 10;
pub const SIDEBAR_ICON_GAP: usize = 10;
pub const SIDEBAR_ROW_GAP: usize = 10;
pub const SIDEBAR_ROW_RADIUS: f32 = 10.0;
/// Corner of the highlight behind a selected or hovered sidebar row. Scaled as
/// a whole, unlike the `ui_f32(SIDEBAR_ROW_RADIUS) + 2.0` it replaces, where
/// the added 2 stayed in raw pixels and so grew relative to the design on any
/// display below the 2x grid the chrome is drawn for.
pub const SIDEBAR_ROW_HIGHLIGHT_RADIUS: f32 = 20.0;
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

/// A tab row's height, from its title font's line height.
///
/// Both tab levels go through this, so the window tab bar and the pane nav bar
/// keep one vertical rhythm at every font size and monitor DPI. The ratio, not
/// a pixel constant: the row exists to hold a line of text, so it has to track
/// that text rather than a fixed number.
pub fn tab_row_height_for_cell(cell_height: usize) -> usize {
    // 2.25x. At the 2x this replaces, the capsule inside came out only
    // `TAB_VERTICAL_PADDING` clear of the row on each side, which read tight
    // once the capsule went fully rounded.
    cell_height.max(1) * 9 / 4
}

pub const TAB_ROW_START_PADDING: usize = 16;
pub const WINDOW_TAB_TOP_SPACER: usize = 4;
pub const WINDOW_TAB_ACTION_RESERVED_WIDTH: usize = 60;
pub const WINDOW_TAB_FULLSCREEN_SIDEBAR_BUTTON_X: usize = 16;
pub const WINDOW_TAB_FULLSCREEN_SIDEBAR_BUTTON_Y_OFFSET: usize = 4;
pub const WINDOW_TAB_FULLSCREEN_SIDEBAR_BUTTON_SIZE: usize = 52;
pub const WINDOW_TAB_FULLSCREEN_SIDEBAR_ICON_SIZE: usize = TAB_ICON_SIZE;
pub const WINDOW_TAB_FULLSCREEN_NEW_SESSION_Y_OFFSET: usize = 6;
pub const WINDOW_TAB_FULLSCREEN_NEW_SESSION_EXTRA_HEIGHT: usize = 6;
pub const WINDOW_TAB_LEADING_ACTION_BUTTON_SIZE: usize = 34;
pub const WINDOW_TAB_LEADING_ACTION_GAP: usize = 8;
/// Matches `TAB_ICON_SIZE`: the leading action sits in the same row as the
/// tabs and the trailing actions, which both already draw at that size.
pub const WINDOW_TAB_LEADING_ACTION_ICON_SIZE: usize = TAB_ICON_SIZE;
pub const WINDOW_TAB_GAP: usize = 18;
/// How far a tab row dissolves into the bar at an edge the viewport cut.
/// Matches `ssh_hosts_view`'s list fades, which solve the same problem
/// vertically.
pub const TAB_ROW_FADE_WIDTH: usize = 32;
/// Pill: `snapped_rounded_corner_radius` clamps this to half the surface's
/// short side, so the tab keeps semicircular ends at every row height.
pub const WINDOW_TAB_RADIUS: f32 = 999.0;
/// A tab surface is chrome, so it sizes itself to the row it lives in rather
/// than to the terminal grid. A roomy row parks every tab at the target width;
/// crowding shrinks them all evenly down to the floor, and only past the floor
/// does the row scroll.
pub const TAB_TARGET_WIDTH: f32 = 320.0;
/// How far tabs give up width before the row scrolls instead.
///
/// Equal to the target, so they give up nothing: a tab is always
/// `TAB_TARGET_WIDTH` wide and the row starts scrolling the moment the tabs
/// stop fitting. Every value in between trades a scrollbar for a few narrower
/// tabs — 240 buys roughly two extra tabs before the row scrolls — but a tab
/// that cannot show its title has stopped doing its job, and scrolling to a
/// legible tab beats reading a row of stubs.
pub const TAB_MIN_WIDTH: f32 = TAB_TARGET_WIDTH;
pub const WINDOW_TAB_ADD_BUTTON_RADIUS: f32 = 999.0;
/// Inset from a tab pill's edge to its content — icon on the left, title's
/// right limit, and (via the close button's own box) the close glyph. Sized
/// against the pill's corner radius, which is half its height: content much
/// nearer than this reads as jammed into the rounded cap. It also lands the
/// leading icon and the trailing close glyph the same distance from their
/// respective ends.
pub const TAB_CONTENT_INSET: usize = 18;
/// Icon inside a tab pill, in the window tab row and the pane nav row alike.
/// Fixed chrome geometry, so the two rows cannot drift apart.
pub const TAB_ICON_SIZE: usize = 32;
/// Leaves a hover ring that clears the pill's top and bottom by the same
/// `TAB_VERTICAL_PADDING` the pill itself sits in.
pub const TAB_CLOSE_HOVER_INSET: usize = 8;
pub const TAB_CLOSE_HOVER_RADIUS: f32 = 999.0;
pub const TAB_VERTICAL_PADDING: usize = 8;

/// How far a pane tab's icon circle sits in from the pill's edges. The same
/// distance from the left as from the top and bottom is what keeps the
/// circle concentric with the pill's rounded end.
pub const TAB_ICON_CIRCLE_INSET: usize = 6;
/// A pane tab icon's glyph, as a share of its circle.
pub const TAB_ICON_GLYPH_RATIO: f32 = 0.56;

pub const PANE_NAV_INSET: usize = 10;
pub const PANE_NAV_ICON_GAP: usize = 8;
pub const PANE_NAV_BUTTON_GAP: usize = 4;
pub const PANE_NAV_TAB_GAP: usize = 18;
pub const PANE_NAV_TAB_RADIUS: f32 = 999.0;
/// Icon in the nav bar's trailing action buttons. Kept apart from
/// `TAB_ICON_SIZE`: these are buttons, not tabs, and want their own weight.
pub const PANE_NAV_ACTION_ICON_SIZE: usize = 26;
pub const PANE_NAV_ACTION_BUTTON_RADIUS: f32 = 999.0;
pub const PANE_DROP_PREVIEW_RADIUS: f32 = 10.0;

pub const CAPSULE_BORDER_WIDTH: f32 = 1.0;
pub const ICON_BUTTON_BORDER_WIDTH: f32 = 1.0;
