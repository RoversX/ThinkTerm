use crate::customglyph::{BlockKey, Poly};
use crate::glyphcache::CachedGlyph;
use crate::quad::{
    HeapQuadAllocator, QuadTrait, TripleLayerQuadAllocator, TripleLayerQuadAllocatorTrait,
};
use crate::renderstate::{RenderContext, RenderState};
use crate::termwindow::render::corners::{
    BOTTOM_LEFT_ROUNDED_CORNER, BOTTOM_RIGHT_ROUNDED_CORNER, TOP_LEFT_ROUNDED_CORNER,
    TOP_RIGHT_ROUNDED_CORNER,
};
use crate::termwindow::render::draw::{draw_opengl_layers, draw_webgpu_layers};
use crate::termwindow::webgpu::WebGpuState;
use crate::ui::{
    home_relative, rect, scale_ui_f32, scale_ui_usize, BrandIcon, ButtonSpec, ButtonVariant,
    ControlState, EditModifiers, InputCaret, InteractionState, ResizablePaneState, ScrollState,
    ScrollbarSpec, SettingsIcon, SvgIcon, TextInputSpec, TextInputState, UiContext, UiPalette,
    UiTokens, WidgetKind,
};
use crate::utilsprites::RenderMetrics;
use anyhow::{Context, Error};
use mux::window::WindowId as MuxWindowId;
use config::{configuration, Dimension, GeometryOrigin};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use wezterm_bidi::Direction;
use wezterm_dynamic::{ToDynamic, Value};
use wezterm_font::{FontConfiguration, FontMetrics, LoadedFont};
use window::bitmaps::atlas::OutOfTextureSpace;
use window::color::LinearRgba;
use window::{
    Appearance, Clipboard, Connection, ConnectionOps, Dimensions, FilePickerOptions,
    FolderPickerOptions, IntegratedTitleButton, IntegratedTitleButtonStyle, KeyCode, KeyEvent,
    Modifiers, MouseButtons, MouseCursor, MouseEvent, MouseEventKind, MousePress,
    RequestedWindowGeometry, Window, WindowDecorations, WindowEvent, WindowOps, WindowState,
};

use crate::native_settings::{
    NativeAppIcon, NativeBottomQuoteMode, NativeRemotePaneResizeMode, NativeRendererBackend,
    NativeThemeMode, ThinkTermNativeSettings, DEFAULT_HOME_FONT_SIZE,
    DEFAULT_PANE_HEADER_FONT_SIZE, DEFAULT_SETTINGS_FONT_SIZE, DEFAULT_SIDEBAR_FONT_SIZE,
    DEFAULT_TAB_FONT_SIZE,
};
use fluent_bundle::FluentArgs;

#[cfg(unix)]
mod session_import;
mod import;
use import::{ImportButton, ImportSource, ImportStep};

// All chrome geometry below is authored in 2x macOS backing pixels.
// settings_ui_scale_for_dpi maps it onto other platforms by treating the
// design as a 192dpi surface, which halves everything at a 1x/96dpi
// display and keeps the proportions identical to macOS.
const DEFAULT_WIDTH: usize = 1840;
const DEFAULT_HEIGHT: usize = 1205;
// Font sizes are points, not scaled pixels: on macOS one point renders as
// one logical pixel, while at 96dpi it renders as 4/3 px, so non-mac
// sizes are 0.75x to come out at the same visual size.
const SIDEBAR_BRAND_FONT_SIZE: f64 = if cfg!(target_os = "macos") {
    17.0
} else {
    12.75
};
const SIDEBAR_BRAND_FONT_WEIGHT: u16 = 750;
// The copyright beside the About page's logo. A fixed size rather than the
// settings font size: it belongs with the logo, which does not follow it.
const LOGO_CAPTION_FONT_SIZE: f64 = if cfg!(target_os = "macos") {
    11.0
} else {
    8.25
};
const LOGO_CAPTION_FONT_WEIGHT: u16 = 500;
const CONTROL_HEIGHT: f32 = 56.0;
/// A dropdown menu's rows, the gap between them and the menu's own padding.
const DROPDOWN_ROW_HEIGHT: f32 = 46.0;
const DROPDOWN_ROW_GAP: f32 = 6.0;
const DROPDOWN_MENU_PADDING: f32 = 8.0;
/// How many rows the interface font menu shows of its long list, and how
/// wide it is where there is room.
const UI_FONT_MENU_ROWS: usize = 8;
const UI_FONT_MENU_WIDTH: f32 = 520.0;
const CONTROL_RADIUS: f32 = 14.0;
const HERO_PADDING: f32 = 36.0;
const HERO_MARK_SIZE: f32 = 108.0;
const SWITCH_WIDTH: f32 = 76.0;
const SWITCH_HEIGHT: f32 = 44.0;
const SWITCH_KNOB_INSET: f32 = 4.0;
// Matches termwindow::ui::tokens::SIDEBAR_ROW_HIGHLIGHT_RADIUS: the two
// sidebars draw the same shape.
const NAV_ROW_RADIUS: f32 = 20.0;
// Row geometry lifted from the main window's sidebar -- SESSION_ROW_MIN_HEIGHT,
// SIDEBAR_INSET and SIDEBAR_ROW_GAP in termwindow::ui. Both sidebars grow a row
// from their own font's cell height against a shared floor rather than pinning
// a constant, so a Settings row and a thread row are the same size even though
// the two use different font sizes.
const NAV_ROW_MIN_HEIGHT: f32 = 66.0;
const NAV_ROW_INSET: f32 = 10.0;
const NAV_ROW_GAP: f32 = 10.0;
const HEADER_HEIGHT: f32 = 132.0;
const SIDEBAR_TITLE_Y: f32 = 78.0;
const SIDEBAR_TITLE_Y_WITH_CUSTOM_CHROME: f32 = 34.0;
const SIDEBAR_BRAND_FONT_SIZE_WITH_CUSTOM_CHROME: f64 = if cfg!(target_os = "macos") {
    22.0
} else {
    16.5
};
const SIDEBAR_SEARCH_Y: f32 = 142.0;
const SIDEBAR_LIST_TOP: f32 = 222.0;
const SIDEBAR_LIST_FADE_HEIGHT: f32 = 24.0;
const CONTENT_TITLE_Y: f32 = 82.0;
const CONTENT_SECTION_Y: f32 = 168.0;
/// The tab icons page's margin to the window's edges. Its editor is the
/// page's side panel, so it runs nearer the right edge than other pages'
/// content (still clear of the scrollbar), and keeps the same distance
/// from the top and bottom while pinned.
const TAB_ICONS_EDGE_MARGIN: f32 = 24.0;
/// The tab icon editor's measures, in design pixels: its padding, the icon
/// heading it, the space above each group of rows, a row's padding, the
/// gap from a row's label to its value, between the items and the lines a
/// value wraps into, and the colour presets' size and their distance below
/// their row's field.
const TAB_ICON_EDITOR_PAD: f32 = 32.0;
const TAB_ICON_EDITOR_HEAD: f32 = 80.0;
const TAB_ICON_EDITOR_SECTION_GAP: f32 = 28.0;
const TAB_ICON_EDITOR_ROW_PAD_X: f32 = 24.0;
const TAB_ICON_EDITOR_ROW_PAD_Y: f32 = 20.0;
const TAB_ICON_EDITOR_LABEL_GAP: f32 = 24.0;
const TAB_ICON_EDITOR_ITEM_GAP: f32 = 12.0;
const TAB_ICON_EDITOR_LINE_GAP: f32 = 12.0;
const TAB_ICON_EDITOR_SWATCH: f32 = 36.0;
const TAB_ICON_EDITOR_SWATCH_GAP: f32 = 16.0;
/// The square heading a settings row, in design pixels (28pt), the gap to
/// its row's text, its corner radius and its glyph as shares of its side.
const ROW_TILE_SIDE: f32 = 56.0;
const ROW_TILE_GAP: f32 = 24.0;
const ROW_TILE_RADIUS: f32 = 0.26;
const ROW_TILE_GLYPH: f32 = 0.58;
/// The numbered discs tying the text-size rows to their picture: in a row,
/// centred where a row's square would be, and on the picture itself.
const ROW_BADGE_SIDE: f32 = 44.0;
const DIAGRAM_BADGE_SIDE: f32 = 38.0;
/// The ⓘ after a label whose explanation shows on hover, and its gap.
const HINT_ICON_SIDE: f32 = 20.0;
const HINT_ICON_GAP: f32 = 8.0;
/// How a row's square is lit: more gently than a tab icon, with a faint
/// light rim and a plain dark shadow, as System Settings draws its own.
const ROW_TILE: crate::ui::tile::TileStyle = crate::ui::tile::TileStyle {
    fill_top_lighten: 0.16,
    fill_bottom_darken: 0.10,
    rim_top_lighten: 0.30,
    rim_bottom_darken: 0.02,
    rim_per_side: 1.0 / 28.0,
    shadow_darken: 1.0,
    shadow_alpha_dark: 0.35,
    shadow_alpha_light: 0.18,
    shadow_sigma: 2.0,
    shadow_drop: 2.0,
};

/// The colours of the terminal a Settings picture shows.
#[derive(Debug, Clone, Copy)]
struct TerminalColors {
    background: LinearRgba,
    foreground: LinearRgba,
    cursor: LinearRgba,
    /// The sixteen ANSI colours.
    ansi: [LinearRgba; 16],
}

/// What `TerminalColors` are worked out from; any change works them out
/// again.
#[derive(Debug, Clone, PartialEq)]
struct TerminalColorsKey {
    mode: NativeThemeMode,
    generation: usize,
    picked: Option<String>,
    appearance: Appearance,
}

/// The palettes the theme thumbnails are drawn in, and the two terminal
/// colours the Follow terminal one writes its text in.
#[derive(Debug, Clone, Copy)]
struct ThemePreviews {
    light: crate::ui::UiPalette,
    dark: crate::ui::UiPalette,
    follow: crate::ui::UiPalette,
    follow_ink: [LinearRgba; 2],
}

/// What `ThemePreviews` are worked out from.
#[derive(Debug, Clone, PartialEq)]
struct ThemePreviewKey {
    generation: usize,
    picked: Option<String>,
    appearance: Appearance,
}

/// The quote the Terminal page previews: read from the quotes file, never
/// written, so it is found on entering the page and after a quote setting
/// changes rather than while painting.
fn quote_preview_text(settings: &ThinkTermNativeSettings) -> String {
    crate::bottom_quotes::preview_quote(
        settings.terminal.bottom_quote_mode,
        crate::native_settings::bottom_quote_interval_minutes(settings),
    )
    .map(|quote| quote.display_text())
    .unwrap_or_default()
}

/// Which of the Terminal page's previews a font is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PreviewFont {
    Terminal,
    Quote,
}

/// What a preview font was built for; any change builds it again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PreviewFontKey {
    which: PreviewFont,
    size: u64,
    generation: usize,
    dpi: usize,
}

/// The colours of the squares heading settings rows: Apple's system
/// colours, where the eye expects them. Yellow is taken darker so a white
/// glyph still reads on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TileColor {
    Blue,
    Indigo,
    Purple,
    Pink,
    Red,
    Orange,
    Yellow,
    Green,
    Teal,
    Gray,
    Slate,
}

impl TileColor {
    fn linear(self) -> LinearRgba {
        let (red, green, blue) = match self {
            Self::Blue => (0x0A, 0x84, 0xFF),
            Self::Indigo => (0x5E, 0x5C, 0xE6),
            Self::Purple => (0xBF, 0x5A, 0xF2),
            Self::Pink => (0xFF, 0x37, 0x5F),
            Self::Red => (0xFF, 0x45, 0x3A),
            Self::Orange => (0xFF, 0x9F, 0x0A),
            Self::Yellow => (0xE5, 0xB4, 0x00),
            Self::Green => (0x30, 0xD1, 0x58),
            Self::Teal => (0x30, 0xB0, 0xC7),
            Self::Gray => (0x63, 0x63, 0x66),
            Self::Slate => (0x5A, 0x64, 0x78),
        };
        LinearRgba::with_srgba(red, green, blue, 0xFF)
    }
}
const CONTENT_RULE_Y: f32 = 202.0;
const SETTINGS_WINDOW_CHROME_HEIGHT: f32 = 74.0;
const SETTINGS_WINDOW_CHROME_FADE_HEIGHT: usize = 18;
const SETTINGS_WINDOW_BUTTON_TOP_INSET: f32 = 18.0;
const SETTINGS_WINDOW_BUTTON_RIGHT_INSET: f32 = 22.0;
const SETTINGS_WINDOW_BUTTON_SIZE: f32 = 52.0;
const SETTINGS_WINDOW_BUTTON_GAP: f32 = 4.0;
const SETTINGS_WINDOW_BUTTON_ICON_SIZE: f32 = 26.0;

thread_local! {
    static SETTINGS_WINDOW: RefCell<SettingsWindowSlot> =
        RefCell::new(SettingsWindowSlot::Closed);
    static SETTINGS_WINDOW_NEXT_ID: Cell<u64> = const { Cell::new(1) };
    /// A page requested before the window exists -- the app menu's Check for
    /// Updates opens Settings straight onto Software Update. Consumed by the
    /// next window to open, and only ever set when one is about to be.
    static PENDING_SECTION: Cell<Option<SettingsSection>> = const { Cell::new(None) };
    /// The window Settings was opened from. The colour-scheme row hands the
    /// choosing back to that window's command palette, and a preview belongs
    /// in the terminal the user was looking at rather than in whichever
    /// window happens to sort first.
    static OPENED_FROM: Cell<Option<MuxWindowId>> = const { Cell::new(None) };
    static OPENED_FROM_SPACE: RefCell<Option<String>> = const { RefCell::new(None) };
}

enum SettingsWindowSlot {
    Closed,
    Opening(u64),
    Open {
        instance_id: u64,
        settings: Rc<RefCell<SettingsWindow>>,
    },
}

enum SettingsShowAction {
    Start(u64),
    Focus(Window),
    Ignore,
}

fn next_settings_window_id() -> u64 {
    SETTINGS_WINDOW_NEXT_ID.with(|next| {
        let id = next.get().max(1);
        next.set(id.wrapping_add(1).max(1));
        id
    })
}

/// Repaint the open settings window, if any. Worker-thread completion
/// callbacks (the agents PATH probe) need this because the settings
/// window is standalone — it is not in the frontend's known-windows
/// list, so `invalidate_all_windows` never reaches it. Main thread only.
pub(crate) fn invalidate_open_settings_window() {
    SETTINGS_WINDOW.with(|slot| {
        if let SettingsWindowSlot::Open { settings, .. } = &*slot.borrow() {
            if let Ok(settings) = settings.try_borrow() {
                if let Some(window) = settings.window.as_ref() {
                    window.invalidate();
                }
            }
        }
    });
}

/// Re-resolve the open settings window's chrome colours and repaint it.
///
/// Same reason as `invalidate_open_settings_window`: this window is standalone
/// and the broadcasts that reach the terminal windows do not reach it. It
/// needs this whenever the colours it derives from move -- the theme mode, or
/// the terminal colour scheme, which under "follow terminal colours" is what
/// its own surfaces are built from.
pub(crate) fn refresh_open_settings_window_chrome() {
    SETTINGS_WINDOW.with(|slot| {
        if let SettingsWindowSlot::Open { settings, .. } = &*slot.borrow() {
            // `try_borrow_mut` because this can be reached from inside the
            // settings window's own event handling, where it is already
            // borrowed; there the refresh that follows covers it anyway.
            if let Ok(mut settings) = settings.try_borrow_mut() {
                // The snapshot too, not just the colours: the scheme just
                // changed underneath this window, and a row that shows it --
                // or a save that writes the whole struct back -- would
                // otherwise be working from the value it was opened with.
                settings.set_native_settings(crate::native_settings::load());
                if let Some(window) = settings.window.as_ref() {
                    window.invalidate();
                }
            }
        }
    });
}

/// Bring the open settings window up to settings that changed outside it
/// (settings.json edited by hand): its copy, as
/// `refresh_open_settings_window_chrome` does, and what it holds beside
/// that copy -- the fields it filled when it opened, the fonts it paints
/// with and its title.
pub(crate) fn follow_open_settings_window(
    before: &ThinkTermNativeSettings,
    after: &ThinkTermNativeSettings,
) {
    SETTINGS_WINDOW.with(|slot| {
        if let SettingsWindowSlot::Open { settings, .. } = &*slot.borrow() {
            if let Ok(mut settings) = settings.try_borrow_mut() {
                settings.follow_settings_changed_elsewhere(before, after);
            }
        }
    });
}

thread_local! {
    /// The install started from Settings. Kept apart from any one Settings
    /// window: closing and reopening Settings must not forget that it runs,
    /// or a second press would start another installer over it.
    static UPDATE_INSTALL: RefCell<Option<UpdateInstall>> = const { RefCell::new(None) };
    /// A Check for Updates press, kept the same way for the same reason.
    static UPDATE_CHECK: RefCell<Option<UpdateCheck>> = const { RefCell::new(None) };
}

fn update_install() -> Option<UpdateInstall> {
    UPDATE_INSTALL.with(|install| install.borrow().clone())
}

fn set_update_install(install: Option<UpdateInstall>) {
    UPDATE_INSTALL.with(|slot| *slot.borrow_mut() = install);
}

fn update_check() -> Option<UpdateCheck> {
    UPDATE_CHECK.with(|check| check.borrow().clone())
}

fn set_update_check(check: Option<UpdateCheck>) {
    UPDATE_CHECK.with(|slot| *slot.borrow_mut() = check);
}

/// Whichever Settings window is open now, of any instance: the one an
/// install started from may have been closed and another opened since.
fn open_settings_window() -> Option<Rc<RefCell<SettingsWindow>>> {
    open_settings_window_with_id().map(|(_, settings)| settings)
}

fn open_settings_window_with_id() -> Option<(u64, Rc<RefCell<SettingsWindow>>)> {
    SETTINGS_WINDOW.with(|slot| match &*slot.borrow() {
        SettingsWindowSlot::Open {
            instance_id,
            settings,
        } => Some((*instance_id, Rc::clone(settings))),
        SettingsWindowSlot::Closed | SettingsWindowSlot::Opening(_) => None,
    })
}

/// Whether restarting ends terminals: a shell this process runs itself -- in
/// process, or over a direct SSH session -- goes with it, where one in the
/// session server or another mux server is kept for the next GUI.
fn restart_ends_terminals() -> bool {
    let Some(mux) = mux::Mux::try_get() else {
        return false;
    };
    mux.iter_panes().iter().any(|pane| {
        mux.get_domain(pane.domain_id()).is_some_and(|domain| {
            domain.downcast_ref::<mux::domain::LocalDomain>().is_some()
                || domain.downcast_ref::<mux::ssh::RemoteSshDomain>().is_some()
        })
    })
}

/// Repaint the open Settings window for a change to the install. With
/// `refresh`, first reread what the page knows about the installed copy,
/// which a finished install has just changed.
fn repaint_open_settings(refresh: bool) {
    let Some(settings) = open_settings_window() else {
        return;
    };
    let Ok(mut settings) = settings.try_borrow_mut() else {
        return;
    };
    if refresh {
        settings.ui.update_status = Some(crate::update::cached_update_status());
        settings.ui.update_method = Some(thinkterm_update::InstallMethod::detect());
    }
    if let Some(window) = settings.window.as_ref() {
        window.invalidate();
    }
}

fn settings_window_for_instance(instance_id: u64) -> Option<Rc<RefCell<SettingsWindow>>> {
    open_settings_window_with_id()
        .filter(|(current_id, _)| *current_id == instance_id)
        .map(|(_, settings)| settings)
}

fn settings_window_pixel_size(
    dpi: usize,
    active_screen_size: Option<(usize, usize)>,
) -> (usize, usize) {
    let mut width = scale_ui_usize(DEFAULT_WIDTH, dpi);
    let mut height = scale_ui_usize(DEFAULT_HEIGHT, dpi);
    if !cfg!(target_os = "macos") {
        if let Some((screen_width, screen_height)) = active_screen_size {
            width = width.min(screen_width.max(1).saturating_mul(9) / 10);
            height = height.min(screen_height.max(1).saturating_mul(9) / 10);
        }
    }
    (width, height)
}

/// "today" / "yesterday" / a local date, for the Archived list's second
/// line. Recency is what the user reasons about there, so the two recent
/// cases get words instead of a date they have to decode.
fn format_archived_when(archived_at: i64) -> String {
    let Some(when) = chrono::DateTime::from_timestamp(archived_at, 0) else {
        return String::new();
    };
    let when = when.with_timezone(&chrono::Local).date_naive();
    let today = chrono::Local::now().date_naive();
    match (today - when).num_days() {
        0 => crate::i18n::tr("settings-archived-when-today"),
        1 => crate::i18n::tr("settings-archived-when-yesterday"),
        _ => when.format("%Y-%m-%d").to_string(),
    }
}

/// What sits at the left of a hero card: the app's own mark on About, or a
/// tinted status glyph on Software Update.
enum HeroBadge {
    AppIcon,
}

/// An install run from the Update page, from the click to its outcome.
#[derive(Debug, Clone)]
enum UpdateInstall {
    /// `progress` is None until the installer first reports;
    /// `download_started` is when the first download report came in, which
    /// the time-left estimate counts from.
    Running {
        version: String,
        progress: Option<thinkterm_update::InstallProgress>,
        download_started: Option<Instant>,
    },
    Installed {
        version: String,
    },
    Failed {
        error: String,
    },
}

/// A Check for Updates press still asking GitHub, or how it went wrong.
#[derive(Debug, Clone)]
enum UpdateCheck {
    Running,
    Failed { error: String },
}

/// What the last check found, as the Update page's status card says it.
/// Read from the on-disk check cache, which the background checker and
/// Check for Updates both write; "up to date" is only ever claimed for a
/// build that actually carries a release tag.
#[derive(Debug, Clone)]
enum UpdateHero {
    /// No usable result on disk: either no check has run, or the cache is
    /// unreadable. Either way there is nothing to compare against.
    Unknown,
    UpToDate,
    Available {
        tag: String,
    },
    /// A commit-stamped build from a plain checkout. It has no ordering
    /// against any release tag, so releases simply do not apply to it.
    LocalBuild,
}

/// Secondary text sits a step below the label. Clamped at 350 so a user who
/// already runs a light UI weight does not end up with hairline text.
fn settings_body_font_weight(label_weight: u16) -> u16 {
    // Proportional, not a fixed subtraction. The label weight is a user
    // setting that ranges 300..800, and subtracting a constant collapses at
    // the light end: from a 400 label it lands on the floor and secondary
    // text ends up either identical to the label or Light-thin, neither of
    // which separates them.
    ((label_weight as f32 * 0.875) as u16).max(350)
}

/// Names the background check interval the way someone would say it out
/// loud, falling back to whole hours or minutes for a hand-edited value.
fn format_check_interval(seconds: u64) -> String {
    match seconds {
        0 => crate::i18n::tr("settings-update-frequency-off"),
        86_400 => crate::i18n::tr("settings-update-frequency-daily"),
        604_800 => crate::i18n::tr("settings-update-frequency-weekly"),
        s if s % 86_400 == 0 => settings_tr(
            "settings-update-frequency-days",
            &[("count", (s / 86_400).to_string())],
        ),
        s if s % 3_600 == 0 => settings_tr(
            "settings-update-frequency-hours",
            &[("count", (s / 3_600).to_string())],
        ),
        s => settings_tr(
            "settings-update-frequency-minutes",
            &[("count", (s.max(60) / 60).to_string())],
        ),
    }
}

/// "about 12 s left", from the average rate since the download started;
/// None until it has run for a second, when the rate means little.
fn download_time_left(started: Instant, done: u64, total: u64) -> Option<String> {
    let elapsed = started.elapsed().as_secs_f64();
    if elapsed < 1.0 || done == 0 || done >= total {
        return None;
    }
    let left = (total - done) as f64 / (done as f64 / elapsed);
    Some(if left < 60.0 {
        settings_tr(
            "settings-update-seconds-left",
            &[("count", (left.ceil() as u64).max(1).to_string())],
        )
    } else {
        settings_tr(
            "settings-update-minutes-left",
            &[("count", ((left / 60.0).ceil() as u64).to_string())],
        )
    })
}

/// "just now" / "N minutes ago" / "N hours ago" / a local date. The recent
/// cases get words because recency is what the user is checking for; older
/// than a day, the date is more useful than a growing hour count.
fn format_last_checked(when: SystemTime) -> String {
    let Ok(elapsed) = when.elapsed() else {
        // A clock that moved backwards leaves the stamp in the future.
        return crate::i18n::tr("settings-update-just-now");
    };
    let secs = elapsed.as_secs();
    if secs < 90 {
        return crate::i18n::tr("settings-update-just-now");
    }
    if secs < 3_600 {
        return settings_tr(
            "settings-update-minutes-ago",
            &[("count", (secs / 60).to_string())],
        );
    }
    if secs < 86_400 {
        return settings_tr(
            "settings-update-hours-ago",
            &[("count", (secs / 3_600).to_string())],
        );
    }
    let when: chrono::DateTime<chrono::Local> = when.into();
    when.format("%Y-%m-%d %H:%M").to_string()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SettingsSection {
    General,
    Appearance,
    TabIcons,
    Sidebar,
    Terminal,
    Workspaces,
    Agents,
    Web,
    Archived,
    Keymap,
    CommandPalette,
    Import,
    Developer,
    UiKit,
    Memory,
    Backup,
    Update,
    About,
}

const BASE_SECTIONS: &[SettingsSection] = &[
    SettingsSection::General,
    SettingsSection::Appearance,
    SettingsSection::TabIcons,
    SettingsSection::Sidebar,
    SettingsSection::Terminal,
    SettingsSection::Workspaces,
    SettingsSection::Agents,
    SettingsSection::Web,
    SettingsSection::Archived,
    SettingsSection::Keymap,
    SettingsSection::CommandPalette,
    SettingsSection::Import,
    SettingsSection::Developer,
    SettingsSection::Backup,
    SettingsSection::Update,
    SettingsSection::About,
];

const DEVELOPER_SECTIONS: &[SettingsSection] = &[SettingsSection::UiKit, SettingsSection::Memory];

/// The expandable per-agent explanation in the Integrations list.
fn agent_detail_lines(agent_id: &str, kind: crate::agent_status::IntegrationKind) -> Vec<String> {
    use crate::agent_status::IntegrationKind;
    match kind {
        IntegrationKind::ScreenRules => vec![
            crate::i18n::tr("settings-agent-details-screen-detect"),
            settings_tr(
                "settings-agent-details-screen-override",
                &[(
                    "path",
                    format!("~/.config/thinkterm/agent-detection/{agent_id}.toml"),
                )],
            ),
            crate::i18n::tr("settings-agent-details-states"),
        ],
        IntegrationKind::Native => vec![
            crate::i18n::tr("settings-agent-details-native"),
            crate::i18n::tr("settings-agent-details-native-doc"),
        ],
        IntegrationKind::Pending => vec![crate::i18n::tr("settings-agent-details-pending")],
    }
}

/// Debug affordance companion to `THINKTERM_SETTINGS_SECTION`: pre-expands
/// one Integrations row (e.g. for unattended UI captures).
fn initial_expanded_agent() -> Option<&'static str> {
    let name = std::env::var("THINKTERM_SETTINGS_EXPAND").ok()?;
    crate::agent_status::SUPPORTED_AGENTS
        .iter()
        .find(|(id, _, _)| *id == name)
        .map(|(id, _, _)| *id)
}

/// Debug affordance companion to `THINKTERM_SETTINGS_SECTION`: opens the
/// page scrolled down this many pixels, clamped to its end on the first
/// paint (e.g. for unattended captures of a page taller than the window).
fn initial_scroll() -> Option<f32> {
    std::env::var("THINKTERM_SETTINGS_SCROLL")
        .ok()?
        .parse::<f32>()
        .ok()
        .filter(|offset| offset.is_finite() && *offset > 0.0)
}

/// Debug affordance: `THINKTERM_SETTINGS_SECTION=<name>` selects that section
/// when the settings window opens (e.g. for unattended UI captures with
/// `THINKTERM_FRAME_DUMP`). Names match the enum variants, case-insensitive.
fn initial_section() -> SettingsSection {
    if let Some(section) = PENDING_SECTION.with(|pending| pending.take()) {
        return section;
    }
    let Some(name) = std::env::var_os("THINKTERM_SETTINGS_SECTION") else {
        return SettingsSection::Appearance;
    };
    let name = name.to_string_lossy().to_ascii_lowercase();
    let section = match name.as_str() {
        "general" => SettingsSection::General,
        "appearance" => SettingsSection::Appearance,
        "tabicons" | "tab-icons" | "tab_icons" => SettingsSection::TabIcons,
        "sidebar" => SettingsSection::Sidebar,
        "terminal" => SettingsSection::Terminal,
        "workspaces" => SettingsSection::Workspaces,
        "agents" => SettingsSection::Agents,
        "web" => SettingsSection::Web,
        // The plugins are on the Sidebar page, the only place they show.
        "plugins" => SettingsSection::Sidebar,
        "archived" => SettingsSection::Archived,
        "keymap" => SettingsSection::Keymap,
        "commandpalette" => SettingsSection::CommandPalette,
        "import" | "compatibility" => SettingsSection::Import,
        "developer" => SettingsSection::Developer,
        "uikit" => SettingsSection::UiKit,
        "memory" => SettingsSection::Memory,
        "backup" => SettingsSection::Backup,
        "update" => SettingsSection::Update,
        "about" => SettingsSection::About,
        #[cfg(unix)]
        id if thinkterm_import::source(id).is_ok() => SettingsSection::Import,
        _ => SettingsSection::Appearance,
    };
    if section == SettingsSection::Agents {
        crate::agent_status::refresh_path_probe();
    }
    if section == SettingsSection::Sidebar {
        crate::plugins::refresh();
    }
    section
}

impl SettingsSection {
    fn label(self) -> String {
        match self {
            Self::General => crate::i18n::tr("settings-section-general"),
            Self::Appearance => crate::i18n::tr("settings-section-appearance"),
            Self::TabIcons => crate::i18n::tr("settings-section-tab-icons"),
            Self::Sidebar => crate::i18n::tr("settings-section-sidebar"),
            Self::Terminal => crate::i18n::tr("settings-section-terminal"),
            Self::Workspaces => crate::i18n::tr("settings-section-workspaces"),
            Self::Agents => crate::i18n::tr("settings-section-agents"),
            Self::Web => crate::i18n::tr("settings-section-web"),
            Self::Archived => crate::i18n::tr("settings-section-archived"),
            Self::Keymap => crate::i18n::tr("settings-section-keymap"),
            Self::CommandPalette => crate::i18n::tr("settings-section-command-palette"),
            Self::Import => crate::i18n::tr("settings-section-import"),
            Self::Developer => crate::i18n::tr("settings-section-developer"),
            Self::UiKit => "UI Kit".to_string(),
            Self::Memory => "Memory".to_string(),
            Self::Backup => crate::i18n::tr("settings-section-backup"),
            Self::Update => crate::i18n::tr("settings-section-update"),
            Self::About => crate::i18n::tr("settings-section-about"),
        }
    }

    fn icon(self) -> SettingsIcon {
        match self {
            Self::General => SettingsIcon::General,
            Self::Appearance => SettingsIcon::Appearance,
            Self::TabIcons => SettingsIcon::TabIcons,
            Self::Sidebar => SettingsIcon::Sidebar,
            Self::Terminal => SettingsIcon::Terminal,
            Self::Workspaces => SettingsIcon::Workspaces,
            Self::Agents => SettingsIcon::Agents,
            Self::Web => SettingsIcon::Web,
            Self::Archived => SettingsIcon::Archived,
            Self::Keymap => SettingsIcon::Keymap,
            Self::CommandPalette => SettingsIcon::CommandPalette,
            Self::Import => SettingsIcon::Import,
            Self::Developer => SettingsIcon::Developer,
            Self::UiKit => SettingsIcon::UiKit,
            Self::Memory => SettingsIcon::Memory,
            Self::Backup => SettingsIcon::Backup,
            Self::Update => SettingsIcon::Update,
            Self::About => SettingsIcon::About,
        }
    }

    /// The colour of the section's square in the sidebar. Neighbours differ,
    /// so the list reads as a set of places rather than a column of grey.
    fn tile_color(self) -> TileColor {
        match self {
            Self::General => TileColor::Gray,
            Self::Appearance => TileColor::Purple,
            Self::TabIcons => TileColor::Pink,
            Self::Sidebar => TileColor::Blue,
            Self::Terminal => TileColor::Slate,
            Self::Workspaces => TileColor::Yellow,
            Self::Agents => TileColor::Orange,
            Self::Web => TileColor::Teal,
            Self::Archived => TileColor::Indigo,
            Self::Keymap => TileColor::Gray,
            Self::CommandPalette => TileColor::Blue,
            Self::Import => TileColor::Green,
            Self::Developer => TileColor::Gray,
            Self::UiKit => TileColor::Pink,
            Self::Memory => TileColor::Teal,
            Self::Backup => TileColor::Blue,
            Self::Update => TileColor::Green,
            Self::About => TileColor::Gray,
        }
    }

    fn search_terms(self) -> &'static [&'static str] {
        match self {
            Self::General => &[
                "Language",
                "Config Source",
                "ThinkTerm Native Settings",
                "Native Settings",
                "Restore Main Window Frame",
                "Main Window Renderer",
                "Renderer Backend",
                "OpenGL",
                "WebGpu",
                "Restart",
                "Quit",
                "Window Size",
                "Window Position",
                "Configuration",
            ],
            Self::Web => &[
                "Web",
                "Browser",
                "Browser Access",
                "Remote",
                "Link",
                "Token",
                "Share",
                "HTTP",
                "Port",
                "Listener",
            ],
            Self::TabIcons => &[
                "Tab Icons",
                "Tab Icon",
                "Icons",
                "Icon",
                "SVG",
                "Program",
                "Logo",
                "Agent",
            ],
            Self::Appearance => &[
                "Theme Mode",
                "Effective Color Scheme",
                "App Icon",
                "Window Opacity",
                "Transparency",
                "Blur",
                "Settings UI Font Size",
                "Workspace Sidebar Font Size",
                "Tab Bar Font Size",
                "Pane Header Font Size",
                "Settings UI Font Weight",
                "Typography",
                "Theme",
                "Font",
                "Weight",
            ],
            Self::Terminal => &[
                "Default Shell",
                "Shell",
                "zsh",
                "bash",
                "fish",
                "PowerShell",
                "pwsh",
                "cmd",
                "Font Size",
                "Font Family",
                "ThinkTerm Font Size",
                "Native Font Family",
                "Bottom Quote",
                "Quote Font Size",
                "Quote Rotation",
                "Quote Interval",
                "Open Quotes JSON",
                "Reset Quotes JSON",
                "Terminal",
            ],
            Self::Workspaces => &[
                "Workspace",
                "Sidebar",
                "Session",
                "Layout",
                "Remote Files",
                "SFTP",
                "Idle Timeout",
            ],
            Self::Agents => &[
                "Agent",
                "Agents",
                "Agent Panel",
                "Integration",
                "Integrations",
                "Claude",
                "Claude Code",
                "Codex",
                "Copilot",
                "Cursor",
                "Detection Rules",
            ],
            Self::Archived => &[
                "Archive",
                "Archived",
                "Unarchive",
                "Restore",
                "Hidden",
                "Archived Workspaces",
            ],
            Self::Keymap => &["Keymap", "Keyboard", "Shortcut"],
            Self::CommandPalette => &[
                "Command Palette",
                "Spotlight",
                "Hotkey",
                "Shortcut",
                "Search Commands",
                "Rows",
                "Font Size",
                "Theme Search",
            ],
            Self::Import => &[
                "Herdr",
                "Import Session",
                "ThinkTerm Config",
                "WezTerm Source",
                "Copy WezTerm Config",
                "Open ThinkTerm Config",
                "Full Config File",
                "Appearance",
                "Terminal",
                "Keymap",
                "WezTerm",
                "Import",
                "Sync",
            ],
            Self::Developer => &[
                "Developer Mode",
                "Diagnostics",
                "Debug Pages",
                "Memory Diagnostics",
                "Input Diagnostics",
                "UI Kit",
                "Context Menu",
                "Fallback Menu",
                "Right Click",
            ],
            Self::UiKit => &[
                "Search Field",
                "Sidebar Rows",
                "Buttons and Controls",
                "Setting Row",
                "Palette",
                "Layout",
                "Typography",
                "Component Preview",
                "Component Styles",
            ],
            Self::Memory => &[
                "Memory",
                "Diagnostics",
                "Manual Sampling",
                "Copy",
                "Refresh",
                "Physical Footprint",
                "RSS",
                "vmmap",
                "IOAccelerator",
                "IOSurface",
                "Graphics",
                "Input",
                "Latency",
                "Key Events",
                "P95",
            ],
            Self::Sidebar => &[
                "Sidebar",
                "Panels",
                "Files",
                "Notes",
                "Agents",
                "Agent Panel",
                "Right Sidebar",
                "Snippets",
                "Plugins",
                "Plugin",
                "Extensions",
                "Reload",
                "Installed",
            ],
            Self::Backup => &[
                "Backup",
                "Export",
                "Import",
                "Restore",
                "Spaces",
                "SSH Hosts",
                "Snippets",
            ],
            Self::Update => &[
                "Update",
                "Software Update",
                "Check for Updates",
                "Automatic Updates",
                "Release",
                "Upgrade",
                "Version",
            ],
            Self::About => &[
                "About",
                "Version",
                "Build",
                "ThinkTerm",
                "License",
                "Source Code",
                "Privacy",
                "Third-Party",
                "Diagnostics",
            ],
        }
    }

    fn matches_search(self, query: &str) -> bool {
        self.label().to_lowercase().contains(query)
            || self
                .search_terms()
                .iter()
                .any(|term| term.to_lowercase().contains(query))
            || (self == Self::General
                && crate::i18n::tr("settings-language")
                    .to_lowercase()
                    .contains(query))
            || (self == Self::Appearance
                && crate::i18n::tr("settings-window-opacity")
                    .to_lowercase()
                    .contains(query))
    }
}

#[cfg(test)]
mod settings_search_tests {
    use super::*;

    #[test]
    fn general_is_searchable_by_the_language_row_label() {
        assert!(SettingsSection::General.matches_search("language"));
        let localized_label = crate::i18n::tr("settings-language").to_lowercase();
        assert!(SettingsSection::General.matches_search(&localized_label));
    }
}

/// How often a drag of the window-opacity slider shows itself in the windows:
/// about once a frame.
const WINDOW_OPACITY_PREVIEW_INTERVAL: Duration = Duration::from_millis(16);

/// The opacity the slider's knob stands for at `x`, on a track from `left`
/// across `width`: the nearest step from the least to opaque.
fn window_opacity_at(x: f32, left: f32, width: f32) -> u8 {
    use crate::native_settings::{WINDOW_OPACITY_LEAST, WINDOW_OPACITY_STEP};
    let along = ((x - left) / width.max(1.0)).clamp(0.0, 1.0);
    let steps = f32::from((100 - WINDOW_OPACITY_LEAST) / WINDOW_OPACITY_STEP);
    WINDOW_OPACITY_LEAST + (along * steps).round() as u8 * WINDOW_OPACITY_STEP
}

#[cfg(test)]
mod window_opacity_slider_tests {
    use super::window_opacity_at;

    #[test]
    fn the_knob_lands_on_steps_from_the_least_to_opaque() {
        assert_eq!(window_opacity_at(100.0, 100.0, 140.0), 30);
        assert_eq!(window_opacity_at(240.0, 100.0, 140.0), 100);
        assert_eq!(window_opacity_at(170.0, 100.0, 140.0), 65);
        // Between steps it takes the nearer one; off the ends, the end.
        assert_eq!(window_opacity_at(104.0, 100.0, 140.0), 30);
        assert_eq!(window_opacity_at(106.0, 100.0, 140.0), 35);
        assert_eq!(window_opacity_at(-50.0, 100.0, 140.0), 30);
        assert_eq!(window_opacity_at(900.0, 100.0, 140.0), 100);
    }
}

/// How Settings names a window-opacity choice: the configuration's own, or a
/// percentage.
fn window_opacity_label(choice: Option<u8>) -> String {
    match choice {
        None => crate::i18n::tr("settings-window-opacity-default"),
        Some(percent) => format!("{percent}%"),
    }
}

/// What the expiry dropdown offers, shortest first.
///
/// `None` last, because it is the one with no bound and reading the list in
/// order should make that the deliberate end of a scale.
const WEB_LINK_TTL_CHOICES: &[Option<u64>] = &[
    Some(60 * 60),
    Some(8 * 60 * 60),
    Some(24 * 60 * 60),
    Some(7 * 24 * 60 * 60),
    None,
];

/// A lifetime as words: hours below a day, days above it, and "until
/// revoked" for no expiry at all.
fn web_link_ttl_label(ttl: Option<u64>) -> String {
    let Some(secs) = ttl else {
        return crate::i18n::tr("settings-web-ttl-never");
    };
    let mut args = FluentArgs::new();
    if secs < 24 * 60 * 60 {
        args.set("hours", (secs / 3600).max(1) as i64);
        crate::i18n::tr_args("settings-web-ttl-hours", &args)
    } else {
        args.set("days", (secs / (24 * 60 * 60)).max(1) as i64);
        crate::i18n::tr_args("settings-web-ttl-days", &args)
    }
}

/// When a browser link stops working, in the viewer's own time zone.
fn format_web_token_when(secs: u64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp(secs as i64, 0)
        .map(|when| {
            when.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_else(|| secs.to_string())
}

fn localized_theme_mode_label(mode: NativeThemeMode) -> String {
    crate::i18n::tr(match mode {
        NativeThemeMode::System => "settings-theme-system",
        NativeThemeMode::Light => "settings-theme-light",
        NativeThemeMode::Dark => "settings-theme-dark",
        NativeThemeMode::FollowTerminal => "settings-theme-follow-terminal",
    })
}

fn localized_app_icon_label(icon: NativeAppIcon) -> String {
    crate::i18n::tr(match icon {
        NativeAppIcon::Simple => "settings-app-icon-simple",
        NativeAppIcon::Classic => "settings-app-icon-classic",
    })
}

fn localized_quote_mode_label(mode: NativeBottomQuoteMode) -> String {
    crate::i18n::tr(match mode {
        NativeBottomQuoteMode::Timed => "settings-quote-mode-timed",
        NativeBottomQuoteMode::PseudoRandom => "settings-quote-mode-random",
    })
}

/// The shells to offer, probed fresh.
///
/// A stored choice that discovery no longer returns is appended so it
/// stays visible and re-selectable: without it the closed control shows a
/// path that appears in no menu row and nothing reads as selected. That
/// happens both when the shell is uninstalled and when it merely
/// re-resolves elsewhere (a Homebrew fish shadowing `/usr/bin/fish`).
fn shell_catalog_including(
    chosen: Option<&Vec<String>>,
) -> Vec<crate::shell_catalog::DiscoveredShell> {
    let mut catalog = crate::shell_catalog::discover();
    if let Some(argv) = chosen.filter(|argv| !argv.is_empty()) {
        if !catalog.iter().any(|shell| &shell.argv == argv) {
            catalog.push(crate::shell_catalog::DiscoveredShell {
                label: argv.join(" "),
                argv: argv.clone(),
            });
        }
    }
    catalog
}

fn localized_remote_pane_resize_mode_label(mode: NativeRemotePaneResizeMode) -> String {
    crate::i18n::tr(match mode {
        NativeRemotePaneResizeMode::Auto => "settings-remote-pane-resize-mode-auto",
        NativeRemotePaneResizeMode::Live => "settings-remote-pane-resize-mode-live",
        NativeRemotePaneResizeMode::OnRelease => "settings-remote-pane-resize-mode-on-release",
    })
}

fn localized_scroll_mode_label(mode: crate::native_settings::NativeScrollMode) -> String {
    use crate::native_settings::NativeScrollMode;
    crate::i18n::tr(match mode {
        NativeScrollMode::Stepped => "settings-scroll-mode-stepped",
        NativeScrollMode::Smooth => "settings-scroll-mode-smooth",
    })
}

fn localized_text_contrast_label(mode: crate::native_settings::NativeTextContrast) -> String {
    use crate::native_settings::NativeTextContrast;
    crate::i18n::tr(match mode {
        NativeTextContrast::Off => "settings-text-contrast-off",
        NativeTextContrast::Ratio3 => "settings-text-contrast-3",
        NativeTextContrast::Ratio45 => "settings-text-contrast-45",
        NativeTextContrast::Ratio7 => "settings-text-contrast-7",
    })
}

/// The groove and the selected segment of a segmented control, which have to
/// be told apart.
///
/// The dark palette does it by making the selection lighter than the groove.
/// The light one cannot: its `control_bg` is already white, and its selected
/// fill is white at 78% -- white on white, which is what the four choices in
/// the Terminal pane looked like. So on the light side the groove is the grey
/// and the selection is the white, which is also how the platform draws it.
fn segmented_track_and_fill(palette: &SettingsPalette) -> (LinearRgba, LinearRgba) {
    if palette.is_dark {
        (palette.control_bg, palette.nav_selected_bg)
    } else {
        (palette.track_off, palette.control_bg)
    }
}

fn settings_tr(id: &'static str, values: &[(&'static str, String)]) -> String {
    let mut args = FluentArgs::new();
    for (name, value) in values {
        args.set(*name, value.clone());
    }
    crate::i18n::tr_args(id, &args)
}

/// A plugin id as something a `Copy` action can carry, hashed as a token
/// id is: two plugins in one list do not collide.
fn plugin_key(id: &str) -> u64 {
    web_token_key(id)
}

/// Every window lays its right sidebar out again. Not invalidate_all_windows:
/// the sidebar's width can have gone to or from zero, and only a real
/// relayout resizes the panes around that.
fn right_sidebar_panels_changed() {
    if let Some(front_end) = crate::frontend::try_front_end() {
        for gui_window in front_end.gui_windows() {
            gui_window
                .window
                .notify(crate::termwindow::TermWindowNotif::Apply(Box::new(
                    |term_window| {
                        term_window.right_sidebar_panels_changed();
                    },
                )));
        }
    }
}

/// The name of how long a plugin runs unused, as an i18n key.
fn background_label(background: thinkterm_plugin_channel::registry::Background) -> &'static str {
    use thinkterm_plugin_channel::registry::Background;
    match background {
        Background::Always => "settings-plugins-background-always",
        Background::Briefly => "settings-plugins-background-briefly",
        Background::Never => "settings-plugins-background-never",
    }
}

/// What running so means, as an i18n key.
fn background_description(
    background: thinkterm_plugin_channel::registry::Background,
) -> &'static str {
    use thinkterm_plugin_channel::registry::Background;
    match background {
        Background::Always => "settings-plugins-background-always-description",
        Background::Briefly => "settings-plugins-background-briefly-description",
        Background::Never => "settings-plugins-background-never-description",
    }
}

/// The right-sidebar panel a built-in plugin provides, by its label.
fn builtin_panel_label(id: &str) -> Option<String> {
    (id == thinkterm_snippets::wire::PLUGIN).then(|| crate::i18n::tr("right-mode-snippets"))
}

/// The square a plugin's row carries: a built-in one takes its panel's, an
/// installed one a puzzle piece in a colour of its own, picked from its id
/// so it keeps it.
fn plugin_tile(plugin: &thinkterm_plugin_channel::registry::Info) -> (SvgIcon, TileColor) {
    if plugin.builtin && plugin.id == thinkterm_snippets::wire::PLUGIN {
        return (SvgIcon::Braces, TileColor::Purple);
    }
    const COLORS: [TileColor; 6] = [
        TileColor::Green,
        TileColor::Teal,
        TileColor::Blue,
        TileColor::Indigo,
        TileColor::Pink,
        TileColor::Orange,
    ];
    // FNV-1a over the id: a hash whose answer no toolchain changes, so a
    // plugin keeps its colour across updates.
    let hash = plugin
        .id
        .bytes()
        .fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
        });
    (
        SvgIcon::Puzzle,
        COLORS[(hash % COLORS.len() as u64) as usize],
    )
}

/// What a plugin's row says under its name: where it came from, what state
/// it is in when that is worth saying, and what it does. A built-in one
/// says which panel it provides.
fn plugin_row_description(plugin: &thinkterm_plugin_channel::registry::Info) -> String {
    use thinkterm_plugin_channel::registry::State;
    if let Some(panel) = plugin
        .builtin
        .then(|| builtin_panel_label(&plugin.id))
        .flatten()
    {
        return settings_tr("settings-plugins-builtin-panel", &[("panel", panel)]);
    }
    let origin = if plugin.builtin {
        crate::i18n::tr("settings-plugins-builtin")
    } else {
        plugin.version.clone()
    };
    let state = match &plugin.state {
        State::Off | State::Idle => None,
        State::New => Some(crate::i18n::tr("settings-plugins-new")),
        State::Starting => Some(crate::i18n::tr("settings-plugins-starting")),
        State::Running => Some(crate::i18n::tr("settings-plugins-running")),
        State::Crashed { reason } => Some(settings_tr(
            "settings-plugins-crashed",
            &[("reason", reason.clone())],
        )),
        State::Failed { reason } => Some(settings_tr(
            "settings-plugins-failed",
            &[("reason", reason.clone())],
        )),
        State::Invalid { reason } => Some(settings_tr(
            "settings-plugins-invalid",
            &[("reason", reason.clone())],
        )),
        State::Unsupported { .. } => Some(crate::i18n::tr("settings-plugins-unsupported")),
    };
    // A new one says where it is: that is what allowing it lets run.
    let place = plugin
        .target
        .as_deref()
        .or(plugin.dir.as_deref())
        .filter(|_| plugin.state == State::New)
        .map(|dir| crate::ui::home_relative(std::path::Path::new(dir)));
    vec![Some(origin), state, place, Some(plugin.description.clone())]
        .into_iter()
        .flatten()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" · ")
}

/// A token id as something a `Copy` action can carry. Ids are random, so a
/// 64-bit hash of one is as good as the id for telling rows apart.
fn web_token_key(id: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    id.hash(&mut hasher);
    hasher.finish()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SettingsAction {
    SelectImportSource(ImportSource),
    ImportContinue,
    ImportBack,
    ImportStartOver,
    #[cfg(unix)]
    SessionImportDetect,
    #[cfg(unix)]
    SessionImportPreview(u64),
    #[cfg(unix)]
    SessionImportHost(u64),
    #[cfg(unix)]
    SessionImportBack,
    #[cfg(unix)]
    SessionImportConfirm,
    #[cfg(unix)]
    SessionImportRecover,
    #[cfg(unix)]
    SessionImportOpen,
    WindowHide,
    WindowMaximize,
    WindowClose,
    Select(SettingsSection),
    OpenThinkTermConfigFile,
    OpenWezTermConfigFile,
    LoadWezTermSource,
    ImportSelectedFields,
    SelectAllImportFields,
    ClearImportFields,
    ToggleImportField(ImportFieldId),
    ToggleMainWindowFrameRestore,
    ToggleNotificationSounds,
    ToggleRemoteUpdateKeepsSessions,
    ToggleLocalSessionsViaMux,
    StopSessionServer,
    /// The ⓘ after a label; hovering it shows the text under this i18n key.
    Hint(&'static str),
    /// Turn one right-sidebar panel on or off.
    ToggleRightSidebarPanel(crate::termwindow::RightSidebarMode),
    /// Turn a plugin on or off. Carries `plugin_key(id)`, as RevokeWebToken
    /// carries its token's: the list can change between press and release.
    TogglePlugin(u64),
    /// Let a new plugin run, from where it is, keyed as TogglePlugin.
    AllowPlugin(u64),
    /// How long a plugin runs unused, keyed as TogglePlugin: the menu of the
    /// choices, and one chosen.
    TogglePluginBackgroundMenu(u64),
    SetPluginBackground(u64, thinkterm_plugin_channel::registry::Background),
    ReloadPlugins,
    OpenPluginsFolder,
    ToggleAgentDetails(&'static str),
    /// Index into the archived rows cached at paint time.
    UnarchiveArchivedRow(usize),
    DeleteArchivedRow(usize),
    ToggleWebServer,
    ToggleWebLinkTtlMenu,
    /// Seconds, or `None` for a link that lasts until it is revoked.
    SetWebLinkTtl(Option<u64>),
    /// Mint a link and put it straight on the clipboard.
    CopyWebLink,
    /// Listen on every address (a phone can reach it) or loopback only.
    ToggleWebReachable,
    /// Mint a link and show it as a QR code, or hide the one shown.
    ToggleWebQr,
    /// Index into the token rows cached at paint time.
    /// Carries `web_token_key(id)`, not the row's index: the press is
    /// remembered at mouse-down and compared by value with the action under
    /// the pointer at release, and the list can be refreshed in between.
    RevokeWebToken(u64),
    RevokeAllWebTokens,
    ToggleDeveloperMode,
    ToggleFallbackContextMenu,
    ShowOnboardingNow,
    ToggleMemoryMonitoring,
    RefreshMemorySnapshot,
    CopyMemorySnapshot,
    ToggleInputDiagnostics,
    ResetInputDiagnostics,
    CopyInputDiagnostics,
    CheckForUpdates,
    InstallUpdate,
    OpenLatestRelease,
    OpenReleasesIndex,
    OpenSourceRepository,
    OpenThirdPartyNotices,
    OpenPrivacyPolicy,
    OpenDataFolder,
    ExportBackup,
    ImportBackup,
    CopyVersionInfo,
    SetThemeMode(NativeThemeMode),
    ToggleLanguageMenu,
    SetLanguage(&'static str),
    SetAppIcon(NativeAppIcon),
    SetWindowOpacity(Option<u8>),
    WindowOpacitySlider,
    ToggleMainRendererMenu,
    SetMainRenderer(NativeRendererBackend),
    /// Swallows clicks that land in an open dropdown's padding or row
    /// gaps instead of letting them reach the controls underneath.
    DropdownMenuBackdrop,
    ToggleDefaultShellMenu,
    /// `None` is the platform default; `Some` indexes the shell catalog
    /// cached at paint time, exactly like the archived-row actions above.
    SetDefaultShell(Option<usize>),
    ToggleCommandPaletteHotkeyMenu,
    SetCommandPaletteHotkey(crate::native_settings::NativeCommandPaletteHotkey),
    DecreaseCommandPaletteRows,
    IncreaseCommandPaletteRows,
    ResetCommandPaletteRows,
    DecreaseCommandPaletteFontSize,
    IncreaseCommandPaletteFontSize,
    ResetCommandPaletteFontSize,
    ToggleCommandPaletteSearchPenetration,
    RestartApplication,
    QuitApplication,
    ToggleBottomQuote,
    SetRemotePaneResizeMode(NativeRemotePaneResizeMode),
    SetScrollMode(crate::native_settings::NativeScrollMode),
    /// Hand the choosing to the command palette's scheme list, which already
    /// has the search and the live preview a thousand entries need.
    OpenColorSchemePicker,
    SetTextContrast(crate::native_settings::NativeTextContrast),
    ToggleOverlayScrollbar,
    SetBottomQuoteMode(NativeBottomQuoteMode),
    DecreaseBottomQuoteFontSize,
    IncreaseBottomQuoteFontSize,
    ResetBottomQuoteFontSize,
    DecreaseBottomQuoteInterval,
    IncreaseBottomQuoteInterval,
    ResetBottomQuoteInterval,
    DecreaseRemoteSftpIdle,
    IncreaseRemoteSftpIdle,
    ResetRemoteSftpIdle,
    ChooseRemoteDownloadDirectory,
    RemoteDropDestinationInput,
    ResetRemoteDownloadDirectory,
    OpenBottomQuotesJson,
    ResetBottomQuotesJson,
    SearchInput,
    DecreaseFontSize,
    IncreaseFontSize,
    ResetFontSize,
    DecreaseChromeFontSize(ChromeFontArea),
    IncreaseChromeFontSize(ChromeFontArea),
    ResetChromeFontSize(ChromeFontArea),
    DecreaseSettingsFontWeight,
    IncreaseSettingsFontWeight,
    ResetSettingsFontWeight,
    ToggleUiFontMenu,
    /// `None` is the system's interface font; `Some` indexes the families
    /// listed when the menu opened.
    SetUiFont(Option<usize>),
    FontFamilyInput,
    ClearSearch,
    SidebarResize,
    SidebarScrollArea,
    ContentScrollArea,
    ToggleTabIcons,
    /// A card, by its place in `tab_icon_cards` as last painted.
    TabIconSelect(u16),
    TabIconNew,
    /// A program of the selected card, by its place in `tab_icon_programs`.
    TabIconRemoveProgram(u16),
    TabIconProgramInput,
    TabIconNameInput,
    TabIconCircleInput,
    TabIconGlyphColorInput,
    TabIconCirclePreset(u8),
    TabIconGlyphPreset(u8),
    TabIconChooseSvg,
    TabIconClearSvg,
    TabIconReset,
    TabIconDelete,
    /// The field that narrows the cards to those a query finds.
    TabIconSearchInput,
    ClearTabIconSearch,
}

/// Where an SVG dragged over the tab icons page would land.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TabIconDropTarget {
    /// A card, by its place in `tab_icon_cards` as last painted.
    Card(usize),
    /// The preview heading the editor: the card open there, which the
    /// search may be keeping out of the grid.
    Editor,
    /// The tile that makes a new card.
    NewCard,
}

/// Where the tab icon editor's parts go, worked out before it is drawn.
struct TabIconEditorLayout {
    height: f32,
    /// The widest row label, which sets where the rows' values start.
    label_width: f32,
    /// Each program chip: where it starts in the value column, its line,
    /// its width and the program.
    chips: Vec<(f32, usize, f32, String)>,
    /// The field that adds a program, under the chips: start, line, width.
    add_field: (f32, usize, f32),
    programs_row: f32,
    /// The Shape row's buttons: start, line, width, label and action.
    shape_buttons: Vec<(f32, usize, f32, String, SettingsAction)>,
    shape_row: f32,
    /// Each colour's row.
    color_row: f32,
}

#[derive(Debug, Clone, Copy)]
enum SettingsDrag {
    SidebarResize {
        start_x: f32,
        start_width: f32,
    },
    /// The window-opacity slider's knob: previewed while it moves, saved
    /// when it is let go.
    WindowOpacity,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SettingsDropdown {
    Language,
    MainRenderer,
    DefaultShell,
    UiFont,
    CommandPaletteHotkey,
    WebLinkTtl,
    /// How long an installed plugin runs unused, keyed as TogglePlugin: its
    /// row is where the list put it, so the menu opens under the pill last
    /// painted for it.
    PluginBackground(u64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ImportFieldId {
    ColorScheme,
    WindowBackgroundOpacity,
    MacosWindowBackgroundBlur,
    InactivePaneHsb,
    FontSize,
    Font,
    LineHeight,
    CellWidth,
    DefaultProg,
    DefaultCwd,
    FrontEnd,
    WindowDecorations,
    DisableDefaultKeyBindings,
    Keys,
    KeyTables,
}

impl ImportFieldId {
    fn all() -> &'static [Self] {
        &[
            Self::ColorScheme,
            Self::WindowBackgroundOpacity,
            Self::MacosWindowBackgroundBlur,
            Self::InactivePaneHsb,
            Self::FontSize,
            Self::Font,
            Self::LineHeight,
            Self::CellWidth,
            Self::DefaultProg,
            Self::DefaultCwd,
            Self::FrontEnd,
            Self::WindowDecorations,
            Self::DisableDefaultKeyBindings,
            Self::Keys,
            Self::KeyTables,
        ]
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::ColorScheme => "color_scheme",
            Self::WindowBackgroundOpacity => "window_background_opacity",
            Self::MacosWindowBackgroundBlur => "macos_window_background_blur",
            Self::InactivePaneHsb => "inactive_pane_hsb",
            Self::FontSize => "font_size",
            Self::Font => "font",
            Self::LineHeight => "line_height",
            Self::CellWidth => "cell_width",
            Self::DefaultProg => "default_prog",
            Self::DefaultCwd => "default_cwd",
            Self::FrontEnd => "front_end",
            Self::WindowDecorations => "window_decorations",
            Self::DisableDefaultKeyBindings => "disable_default_key_bindings",
            Self::Keys => "keys",
            Self::KeyTables => "key_tables",
        }
    }

    fn category(self) -> String {
        crate::i18n::tr(match self {
            Self::ColorScheme
            | Self::WindowBackgroundOpacity
            | Self::MacosWindowBackgroundBlur
            | Self::InactivePaneHsb => "settings-import-category-appearance",
            Self::FontSize
            | Self::Font
            | Self::LineHeight
            | Self::CellWidth
            | Self::DefaultProg
            | Self::DefaultCwd => "settings-import-category-terminal",
            Self::FrontEnd | Self::WindowDecorations => "settings-import-category-window",
            Self::DisableDefaultKeyBindings | Self::Keys | Self::KeyTables => {
                "settings-import-category-keymap"
            }
        })
    }

    fn label(self) -> String {
        crate::i18n::tr(match self {
            Self::ColorScheme => "settings-import-field-color-scheme",
            Self::WindowBackgroundOpacity => "settings-import-field-window-opacity",
            Self::MacosWindowBackgroundBlur => "settings-import-field-macos-blur",
            Self::InactivePaneHsb => "settings-import-field-inactive-pane",
            Self::FontSize => "settings-import-field-font-size",
            Self::Font => "settings-import-field-font",
            Self::LineHeight => "settings-import-field-line-height",
            Self::CellWidth => "settings-import-field-cell-width",
            Self::DefaultProg => "settings-import-field-default-program",
            Self::DefaultCwd => "settings-import-field-default-directory",
            Self::FrontEnd => "settings-import-field-renderer",
            Self::WindowDecorations => "settings-import-field-window-decorations",
            Self::DisableDefaultKeyBindings => "settings-import-field-disable-default-keys",
            Self::Keys => "settings-import-field-key-bindings",
            Self::KeyTables => "settings-import-field-key-tables",
        })
    }

    fn description(self, config: &config::Config) -> String {
        match self {
            Self::Keys | Self::KeyTables => {
                let count = if self == Self::Keys {
                    config.keys.len()
                } else {
                    config.key_tables.len()
                };
                settings_tr(
                    if self == Self::Keys {
                        "settings-import-description-key-bindings"
                    } else {
                        "settings-import-description-key-tables"
                    },
                    &[("count", count.to_string())],
                )
            }
            _ => crate::i18n::tr(match self {
                Self::ColorScheme => "settings-import-description-color-scheme",
                Self::WindowBackgroundOpacity => "settings-import-description-window-opacity",
                Self::MacosWindowBackgroundBlur => "settings-import-description-macos-blur",
                Self::InactivePaneHsb => "settings-import-description-inactive-pane",
                Self::FontSize => "settings-import-description-font-size",
                Self::Font => "settings-import-description-font",
                Self::LineHeight => "settings-import-description-line-height",
                Self::CellWidth => "settings-import-description-cell-width",
                Self::DefaultProg => "settings-import-description-default-program",
                Self::DefaultCwd => "settings-import-description-default-directory",
                Self::FrontEnd => "settings-import-description-renderer",
                Self::WindowDecorations => "settings-import-description-window-decorations",
                Self::DisableDefaultKeyBindings => {
                    "settings-import-description-disable-default-keys"
                }
                Self::Keys | Self::KeyTables => unreachable!(),
            }),
        }
    }

    fn preview(self, config: &config::Config, value: &Value) -> String {
        match self {
            Self::ColorScheme => config
                .color_scheme
                .clone()
                .unwrap_or_else(|| crate::i18n::tr("settings-import-preview-custom-colors")),
            Self::WindowBackgroundOpacity => format!("{:.2}", config.window_background_opacity),
            Self::MacosWindowBackgroundBlur => config.macos_window_background_blur.to_string(),
            Self::InactivePaneHsb => {
                // The list only carries this field when the config sets it
                // explicitly, so the background-derived default is never
                // what shows; build the palette only if it ever is.
                let hsb = config.inactive_pane_hsb.unwrap_or_else(|| {
                    let palette: wezterm_term::color::ColorPalette =
                        config.resolved_palette.clone().into();
                    config.inactive_pane_hsb_for_background(palette.background)
                });
                format!(
                    "h {:.2}, s {:.2}, b {:.2}",
                    hsb.hue, hsb.saturation, hsb.brightness
                )
            }
            Self::FontSize => format!("{:.1}", config.font_size),
            Self::Font => config
                .font
                .font
                .first()
                .map(|font| font.family.clone())
                .unwrap_or_else(|| crate::i18n::tr("settings-import-preview-font-table")),
            Self::LineHeight => format!("{:.2}", config.line_height),
            Self::CellWidth => format!("{:.2}", config.cell_width),
            Self::DefaultProg => config
                .default_prog
                .as_ref()
                .map(|prog| prog.join(" "))
                .unwrap_or_else(|| Self::value_preview(value)),
            Self::DefaultCwd => config
                .default_cwd
                .as_ref()
                .map(|cwd| cwd.display().to_string())
                .unwrap_or_else(|| Self::value_preview(value)),
            Self::FrontEnd => format!("{:?}", config.front_end),
            Self::WindowDecorations => {
                let value: String = (&config.window_decorations).into();
                value
            }
            Self::DisableDefaultKeyBindings => config.disable_default_key_bindings.to_string(),
            Self::Keys => format!("{} bindings", config.keys.len()),
            Self::KeyTables => format!("{} tables", config.key_tables.len()),
        }
    }

    fn value_preview(value: &Value) -> String {
        match value {
            Value::Null => "nil".to_string(),
            Value::Bool(value) => value.to_string(),
            Value::String(value) => value.clone(),
            Value::U64(value) => value.to_string(),
            Value::I64(value) => value.to_string(),
            Value::F64(value) => value.to_string(),
            Value::Array(value) => format!("{} items", value.len()),
            Value::Object(value) => format!("{} fields", value.len()),
        }
    }
}

#[derive(Debug, Clone)]
struct ImportableField {
    id: ImportFieldId,
    category: String,
    label: String,
    description: String,
    preview: String,
    lua_value: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct CompatibilityImportState {
    fields: Vec<ImportableField>,
    error: Option<String>,
}

#[derive(Debug, Clone)]
struct MemorySnapshot {
    captured_at: Instant,
    pid: u32,
    resident_size: Option<u64>,
    physical_footprint: Option<u64>,
    peak_physical_footprint: Option<u64>,
    /// Windows only: committed private bytes. Deliberately separate from
    /// `physical_footprint` -- see `ProcessMemoryInfo`.
    commit_size: Option<u64>,
    /// Windows only: peak working set.
    peak_resident_size: Option<u64>,
    vmmap_total_resident: Option<u64>,
    vmmap_graphics_resident: Option<u64>,
    vmmap_malloc_resident: Option<u64>,
    vmmap_text_resident: Option<u64>,
    vmmap_iosurface_resident: Option<u64>,
    vmmap_error: Option<String>,
    error: Option<String>,
}

impl MemorySnapshot {
    /// The headline resident number and what to call it. macOS and Windows
    /// both have one; only the name differs.
    fn resident_metric(&self) -> (&'static str, Option<u64>) {
        if cfg!(windows) {
            ("working_set", self.resident_size)
        } else {
            ("rss", self.resident_size)
        }
    }

    /// The second number, which is a genuinely *different measure* per
    /// platform rather than the same one renamed: macOS reports
    /// `phys_footprint` (resident-ish), Windows reports commit charge. Keeping
    /// one label for both is how commit gets mistaken for resident memory.
    fn secondary_metric(&self) -> (&'static str, Option<u64>) {
        if cfg!(windows) {
            ("commit_private_bytes", self.commit_size)
        } else {
            ("physical_footprint", self.physical_footprint)
        }
    }

    fn peak_metric(&self) -> (&'static str, Option<u64>) {
        if cfg!(windows) {
            ("peak_working_set", self.peak_resident_size)
        } else {
            ("peak_physical_footprint", self.peak_physical_footprint)
        }
    }

    fn has_vmmap_breakdown(&self) -> bool {
        self.vmmap_total_resident.is_some()
            || self.vmmap_graphics_resident.is_some()
            || self.vmmap_malloc_resident.is_some()
            || self.vmmap_text_resident.is_some()
            || self.vmmap_iosurface_resident.is_some()
    }

    fn log_line(&self) -> String {
        let (resident_label, resident) = self.resident_metric();
        let (secondary_label, secondary) = self.secondary_metric();
        let (peak_label, peak) = self.peak_metric();
        format!(
            "pid={} {resident_label}={} {secondary_label}={} {peak_label}={} vmmap_total_resident={} graphics_resident={} malloc_resident={}{}{}",
            self.pid,
            resident
                .map(format_bytes)
                .unwrap_or_else(|| "unavailable".to_string()),
            secondary
                .map(format_bytes)
                .unwrap_or_else(|| "unavailable".to_string()),
            peak
                .map(format_bytes)
                .unwrap_or_else(|| "unavailable".to_string()),
            self.vmmap_total_resident
                .map(format_bytes)
                .unwrap_or_else(|| "unavailable".to_string()),
            self.vmmap_graphics_resident
                .map(format_bytes)
                .unwrap_or_else(|| "unavailable".to_string()),
            self.vmmap_malloc_resident
                .map(format_bytes)
                .unwrap_or_else(|| "unavailable".to_string()),
            self.vmmap_error
                .as_ref()
                .map(|error| format!(" vmmap_error={error}"))
                .unwrap_or_default(),
            self.error
                .as_ref()
                .map(|error| format!(" error={error}"))
                .unwrap_or_default()
        )
    }

    fn summary_for_clipboard(&self) -> String {
        let (resident_label, resident) = self.resident_metric();
        let (secondary_label, secondary) = self.secondary_metric();
        let (peak_label, peak) = self.peak_metric();
        let mut lines = vec![
            "ThinkTerm Memory Snapshot".to_string(),
            format!("pid: {}", self.pid),
            format!(
                "{resident_label}: {}",
                resident
                    .map(format_bytes)
                    .unwrap_or_else(|| "unavailable".to_string())
            ),
            format!(
                "{secondary_label}: {}",
                secondary
                    .map(format_bytes)
                    .unwrap_or_else(|| "unavailable".to_string())
            ),
            format!(
                "{peak_label}: {}",
                peak
                    .map(format_bytes)
                    .unwrap_or_else(|| "unavailable".to_string())
            ),
            format!(
                "vmmap_total_resident: {}",
                self.vmmap_total_resident
                    .map(format_bytes)
                    .unwrap_or_else(|| "not captured".to_string())
            ),
            format!(
                "graphics_resident: {}",
                self.vmmap_graphics_resident
                    .map(format_bytes)
                    .unwrap_or_else(|| "not captured".to_string())
            ),
            format!(
                "iosurface_resident: {}",
                self.vmmap_iosurface_resident
                    .map(format_bytes)
                    .unwrap_or_else(|| "not captured".to_string())
            ),
            format!(
                "malloc_resident: {}",
                self.vmmap_malloc_resident
                    .map(format_bytes)
                    .unwrap_or_else(|| "not captured".to_string())
            ),
            format!(
                "text_segments_resident: {}",
                self.vmmap_text_resident
                    .map(format_bytes)
                    .unwrap_or_else(|| "not captured".to_string())
            ),
        ];
        if let Some(error) = &self.vmmap_error {
            lines.push(format!("vmmap_error: {error}"));
        }
        if let Some(error) = &self.error {
            lines.push(format!("error: {error}"));
        }
        lines.join("\n")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChromeFontArea {
    Settings,
    Home,
    RightSidebar,
    Sidebar,
    TabBar,
    PaneHeader,
}

/// Order of the font-size stepper rows in the Typography card. The card's
/// row count derives from this list (plus the trailing font-weight row),
/// so adding an area here automatically grows the card background.
const TYPOGRAPHY_FONT_AREAS: [ChromeFontArea; 6] = [
    ChromeFontArea::Settings,
    ChromeFontArea::Home,
    ChromeFontArea::RightSidebar,
    ChromeFontArea::Sidebar,
    ChromeFontArea::TabBar,
    ChromeFontArea::PaneHeader,
];

impl ChromeFontArea {
    fn localized_label(self) -> String {
        crate::i18n::tr(match self {
            Self::Settings => "settings-font-size-settings",
            Self::Home => "settings-font-size-home",
            Self::RightSidebar => "settings-font-size-right-sidebar",
            Self::Sidebar => "settings-font-size-workspace-sidebar",
            Self::TabBar => "settings-font-size-tab-bar",
            Self::PaneHeader => "settings-font-size-pane-header",
        })
    }

    fn description_key(self) -> &'static str {
        match self {
            Self::Settings => "settings-font-size-settings-description",
            Self::Home => "settings-font-size-home-description",
            Self::RightSidebar => "settings-font-size-right-sidebar-description",
            Self::Sidebar => "settings-font-size-workspace-sidebar-description",
            Self::TabBar => "settings-font-size-tab-bar-description",
            Self::PaneHeader => "settings-font-size-pane-header-description",
        }
    }

    fn default_size(self) -> f64 {
        match self {
            Self::Settings => DEFAULT_SETTINGS_FONT_SIZE,
            Self::Home => DEFAULT_HOME_FONT_SIZE,
            Self::RightSidebar => DEFAULT_HOME_FONT_SIZE,
            Self::Sidebar => DEFAULT_SIDEBAR_FONT_SIZE,
            Self::TabBar => DEFAULT_TAB_FONT_SIZE,
            Self::PaneHeader => DEFAULT_PANE_HEADER_FONT_SIZE,
        }
    }
}

#[derive(Debug, Clone)]
struct SettingsUiState {
    import_source: ImportSource,
    import_step: ImportStep,
    import_result: Option<String>,
    #[cfg(unix)]
    session_import: session_import::ImportUi,
    tokens: UiTokens,
    sidebar: ResizablePaneState,
    sidebar_scroll: ScrollState,
    content_scroll: ScrollState,
    search: TextInputState,
    font_size_input: TextInputState,
    font_family_input: TextInputState,
    font_family_input_dirty: bool,
    remote_drop_input: TextInputState,
    remote_drop_input_dirty: bool,
    interaction: InteractionState<SettingsAction>,
    drag: Option<SettingsDrag>,
    /// The window-opacity slider's track as last painted: its left edge and
    /// width, for turning the pointer's x into a value.
    window_opacity_track: Option<(f32, f32)>,
    /// When a drag of that slider last showed its value in the windows.
    window_opacity_previewed: Option<Instant>,
    open_dropdown: Option<SettingsDropdown>,
    /// The ⓘ under the pointer this frame: its i18n key and where the icon
    /// was painted, for the bubble the overlay pass draws.
    hint: Option<(&'static str, f32, f32, f32)>,
    memory_monitoring: bool,
    memory_monitor_generation: u64,
    memory_snapshot: Option<MemorySnapshot>,
    main_window_resource_lines: Vec<String>,
    memory_snapshot_copied_until: Option<Instant>,
    /// The archived rows as last painted; row-action indices resolve here
    /// so a click acts on exactly what the user saw.
    archived_rows: Vec<crate::workspace_threads::ArchivedProjectRow>,
    /// The browser links as last painted, for the same reason: a revoke
    /// must cut off the row that was under the pointer, not whichever row
    /// holds that index after the list refreshed.
    web_tokens: Vec<codec::WebTokenInfo>,
    /// The plugins' switches as last painted: each one's key, its id and
    /// the position it showed.
    plugin_switches: Vec<(u64, String, bool)>,
    /// The plugins whose background was offered as last painted: each
    /// one's key, its id and its manifest's choice.
    plugin_backgrounds: Vec<(
        u64,
        String,
        thinkterm_plugin_channel::registry::Background,
    )>,
    /// The pill of the plugin whose background menu is open, as last
    /// painted: the menu opens under it.
    plugin_background_pill: Option<(u64, window::RectF)>,
    /// The shells found on this machine. Refreshed when the Terminal
    /// section is entered, never while painting: discovery touches the
    /// filesystem, and the paint path must not.
    shell_catalog: Vec<crate::shell_catalog::DiscoveredShell>,
    /// Two-step delete: the project id whose Delete button was clicked
    /// once. A second click executes; any other action clears it.
    confirm_delete_archived: Option<String>,
    /// The stop-server row was pressed once; the next press stops it.
    confirm_stop_server: bool,
    input_diagnostics_copied_until: Option<Instant>,
    /// The Update page's view of the on-disk check cache. Read when the
    /// section is entered and when a check from the page finishes, never
    /// while painting: it stats and reads a file, and the paint path must not.
    update_status: Option<crate::update::CachedUpdateStatus>,
    /// Set briefly after Check Now so the button can report that it ran even
    /// when the cached answer is unchanged.
    update_checked_until: Option<Instant>,
    /// How this copy was installed, which decides whether the page can
    /// install an update itself or only say who can. Read with the status:
    /// it looks at the filesystem, so never on the paint path.
    update_method: Option<thinkterm_update::InstallMethod>,
    version_info_copied_until: Option<Instant>,
    sidebar_scrollbar_visible_until: Option<Instant>,
    content_scrollbar_visible_until: Option<Instant>,
    /// How many lines a row's description may take before it is cut short:
    /// the card being painted says (`paint_card_as`); rows placed at fixed
    /// steps outside one keep to one, or they would overlap.
    row_description_lines: usize,
    /// Where each dropdown's closed face was painted this frame, as the
    /// `(x, y, width)` its open menu hangs from. Recorded by the rows, so a
    /// menu follows its row wherever the page put it.
    dropdown_anchors: Vec<(SettingsDropdown, (f32, f32, f32))>,
    /// The families the interface font menu offers, listed when it first
    /// opens: empty until then.
    ui_font_families: Vec<String>,
    /// How far that menu is scrolled. It shows a few rows of a long list.
    ui_font_menu_scroll: ScrollState,
    /// Where that menu was painted this frame, for the wheel to find it.
    ui_font_menu_rect: Option<window::RectF>,
    /// The fonts the Terminal page previews in, at most one of each kind,
    /// and let go when another page is shown. `None` remembers a face that
    /// would not load, so it is not retried every frame.
    preview_fonts: Vec<(PreviewFontKey, Option<Rc<LoadedFont>>)>,
    /// Fonts were let go whose glyphs are still in the atlas: the next time
    /// it fills, clear it before growing it.
    stale_glyphs: bool,
    /// The terminal's colours per theme mode asked for, and the palettes
    /// the theme thumbnails are drawn in: working them out clones the
    /// settings and converts a whole scheme, so it happens when what they
    /// depend on changes rather than on every paint.
    terminal_colors: Vec<(TerminalColorsKey, TerminalColors)>,
    theme_previews: Option<(ThemePreviewKey, ThemePreviews)>,
    /// The quote the Terminal page previews (`quote_preview_text`).
    quote_preview: String,
    /// The quotes file's reset was pressed once; the next press replaces it.
    confirm_reset_quotes: bool,
    /// Tab icons: the card being edited, by id.
    tab_icon_selected: Option<String>,
    /// What the cards are narrowed to, by name or program.
    tab_icon_search: TextInputState,
    /// The cards as last painted, so a click acts on the card that was
    /// under the pointer.
    tab_icon_cards: Vec<String>,
    /// The selected card's programs as last painted, for the same reason.
    tab_icon_programs: Vec<String>,
    /// Where an SVG could be dropped, as last painted.
    tab_icon_drop_rects: Vec<(window::RectF, TabIconDropTarget)>,
    /// The drop target under an SVG being dragged, lit while it is.
    tab_icon_drop_target: Option<TabIconDropTarget>,
    tab_icon_program_input: TextInputState,
    tab_icon_name_input: TextInputState,
    tab_icon_name_input_dirty: bool,
    tab_icon_circle_input: TextInputState,
    tab_icon_circle_input_dirty: bool,
    tab_icon_glyph_input: TextInputState,
    tab_icon_glyph_input_dirty: bool,
    /// Two-step delete: the card whose Delete was clicked once.
    confirm_delete_tab_icon: Option<String>,
}

impl SettingsUiState {
    fn new(dpi: usize) -> Self {
        let tokens = UiTokens::for_dpi(dpi);
        Self {
            #[cfg(unix)]
            session_import: session_import::ImportUi::default(),
            import_source: ImportSource::initial(),
            import_step: {
                #[cfg(unix)]
                if session_import::remembered_source().is_some() {
                    ImportStep::Review
                } else {
                    ImportStep::Source
                }
                #[cfg(not(unix))]
                {
                    ImportStep::Source
                }
            },
            import_result: None,
            sidebar: ResizablePaneState::new(
                tokens.sidebar_default_width,
                tokens.sidebar_min_width,
                tokens.sidebar_max_width,
            ),
            tokens,
            sidebar_scroll: ScrollState::new(),
            content_scroll: ScrollState::new(),
            search: TextInputState::new(),
            font_size_input: TextInputState::new(),
            font_family_input: TextInputState::new(),
            font_family_input_dirty: false,
            remote_drop_input: TextInputState::new(),
            remote_drop_input_dirty: false,
            interaction: InteractionState::default(),
            drag: None,
            window_opacity_track: None,
            window_opacity_previewed: None,
            open_dropdown: None,
            hint: None,
            memory_monitoring: false,
            memory_monitor_generation: 0,
            memory_snapshot: None,
            main_window_resource_lines: Vec::new(),
            memory_snapshot_copied_until: None,
            archived_rows: Vec::new(),
            web_tokens: Vec::new(),
            plugin_switches: Vec::new(),
            plugin_backgrounds: Vec::new(),
            plugin_background_pill: None,
            shell_catalog: Vec::new(),
            confirm_delete_archived: None,
            confirm_stop_server: false,
            input_diagnostics_copied_until: None,
            update_status: None,
            update_checked_until: None,
            update_method: None,
            version_info_copied_until: None,
            sidebar_scrollbar_visible_until: None,
            content_scrollbar_visible_until: None,
            row_description_lines: 1,
            dropdown_anchors: Vec::new(),
            ui_font_families: Vec::new(),
            ui_font_menu_scroll: ScrollState::new(),
            ui_font_menu_rect: None,
            preview_fonts: Vec::new(),
            stale_glyphs: false,
            terminal_colors: Vec::new(),
            theme_previews: None,
            quote_preview: String::new(),
            confirm_reset_quotes: false,
            tab_icon_selected: None,
            tab_icon_search: TextInputState::new(),
            tab_icon_cards: Vec::new(),
            tab_icon_programs: Vec::new(),
            tab_icon_drop_rects: Vec::new(),
            tab_icon_drop_target: None,
            tab_icon_program_input: TextInputState::new(),
            tab_icon_name_input: TextInputState::new(),
            tab_icon_name_input_dirty: false,
            tab_icon_circle_input: TextInputState::new(),
            tab_icon_circle_input_dirty: false,
            tab_icon_glyph_input: TextInputState::new(),
            tab_icon_glyph_input_dirty: false,
            confirm_delete_tab_icon: None,
        }
    }
}

/// Where a panel `height` tall goes beside a scrolling column that ends at
/// `column_bottom`, when unpinned it would be at `natural_y` and the view
/// runs from `top` to `bottom`: with the page until it reaches `top`, held
/// there, and carried off with the column's end. A panel taller than the
/// view is held by its bottom edge at `bottom` instead.
fn pinned_beside_column(
    natural_y: f32,
    column_bottom: f32,
    height: f32,
    top: f32,
    bottom: f32,
) -> f32 {
    let pinned = if height <= bottom - top {
        top
    } else {
        bottom - height
    };
    natural_y
        .max(pinned)
        .min((column_bottom - height).max(natural_y))
}

/// Lay out items `widths` wide in lines no wider than `max`, `gap` apart,
/// each line pushed against the right edge. Returns where each item starts
/// and its line, and how many lines there are (at least one).
fn flow_right(widths: &[f32], max: f32, gap: f32) -> (Vec<(f32, usize)>, usize) {
    let mut places = Vec::with_capacity(widths.len());
    let mut line_widths = Vec::new();
    let (mut x, mut line) = (0.0f32, 0usize);
    for &width in widths {
        if x > 0.0 && x + width > max {
            line_widths.push(x - gap);
            x = 0.0;
            line += 1;
        }
        places.push((x, line));
        x += width + gap;
    }
    line_widths.push((x - gap).max(0.0));
    let places = places
        .into_iter()
        .map(|(start, line)| (start + (max - line_widths[line]).max(0.0), line))
        .collect();
    (places, line_widths.len())
}

/// How a card lays its rows out: the room above the first and below the
/// last, and how many lines a row's description may wrap to.
#[derive(Debug, Clone, Copy)]
struct CardLayout {
    top: f32,
    bottom: f32,
    description_lines: usize,
}

/// `color` made solid: the pictures of windows are opaque, so nothing under
/// them shows through and a ring drawn beneath shows only at the edge.
fn opaque(color: LinearRgba) -> LinearRgba {
    LinearRgba::with_components(color.0, color.1, color.2, 1.0)
}

/// Where the next row of a card goes, and where the last one ended. Rows
/// move the cursor by the standard step plus whatever their description
/// took beyond one line.
struct RowCursor {
    y: f32,
    bottom: f32,
    step: f32,
    visual: f32,
    rows: usize,
}

impl RowCursor {
    fn new(top: f32, window: &SettingsWindow) -> Self {
        Self {
            y: top,
            bottom: top,
            step: window.settings_row_step(),
            visual: window.settings_row_visual_height(),
            rows: 0,
        }
    }

    /// Whether the row about to be painted has a rule above it: all but
    /// the first.
    fn rule(&self) -> bool {
        self.rows > 0
    }

    /// The row just painted took `extra` beyond one line of description.
    fn add(&mut self, extra: f32) {
        self.bottom = self.y + self.visual + extra;
        self.y += self.step + extra;
        self.rows += 1;
    }
}

fn format_bytes(bytes: u64) -> String {
    thinkterm_update::format_size(bytes)
}

fn capture_memory_snapshot(detailed: bool) -> MemorySnapshot {
    let pid = std::process::id();
    let mut snapshot = MemorySnapshot {
        captured_at: Instant::now(),
        pid,
        resident_size: None,
        physical_footprint: None,
        peak_physical_footprint: None,
        commit_size: None,
        peak_resident_size: None,
        vmmap_total_resident: None,
        vmmap_graphics_resident: None,
        vmmap_malloc_resident: None,
        vmmap_text_resident: None,
        vmmap_iosurface_resident: None,
        vmmap_error: None,
        error: None,
    };

    match capture_process_memory_info(pid) {
        Ok(info) => {
            snapshot.resident_size = Some(info.resident_size);
            snapshot.physical_footprint = info.physical_footprint;
            snapshot.peak_physical_footprint = info.peak_physical_footprint;
            snapshot.commit_size = info.commit_size;
            snapshot.peak_resident_size = info.peak_resident_size;
        }
        Err(err) => {
            snapshot.error = Some(err);
        }
    }

    // vmmap is a macOS tool. Before the panel worked on Windows this branch was
    // unreachable in practice; now it would spawn a nonexistent /usr/bin/vmmap
    // on every Refresh and Copy, and write the resulting "path not found" into
    // the UI and the clipboard summary as though it were a diagnostic.
    if detailed && cfg!(target_os = "macos") {
        match capture_vmmap_breakdown(pid) {
            Ok(breakdown) => {
                snapshot.vmmap_total_resident = breakdown.total_resident;
                snapshot.vmmap_graphics_resident = breakdown.graphics_resident;
                snapshot.vmmap_malloc_resident = breakdown.malloc_resident;
                snapshot.vmmap_text_resident = breakdown.text_resident;
                snapshot.vmmap_iosurface_resident = breakdown.iosurface_resident;
            }
            Err(err) => {
                snapshot.vmmap_error = Some(err);
            }
        }
    }

    snapshot
}

#[derive(Debug, Clone, Default)]
struct VmmapBreakdown {
    total_resident: Option<u64>,
    graphics_resident: Option<u64>,
    malloc_resident: Option<u64>,
    text_resident: Option<u64>,
    iosurface_resident: Option<u64>,
}

fn capture_vmmap_breakdown(pid: u32) -> Result<VmmapBreakdown, String> {
    let output = Command::new("/usr/bin/vmmap")
        .arg("-summary")
        .arg(pid.to_string())
        .output()
        .map_err(|err| format!("failed to run vmmap: {err}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(stderr.trim().to_string());
    }

    parse_vmmap_summary(&String::from_utf8_lossy(&output.stdout))
}

fn parse_vmmap_summary(summary: &str) -> Result<VmmapBreakdown, String> {
    let mut result = VmmapBreakdown::default();
    let mut graphics_resident = 0;
    let mut has_graphics = false;
    let mut malloc_resident = 0;
    let mut has_malloc = false;
    let mut in_region_table = false;

    for line in summary.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("REGION TYPE") {
            in_region_table = true;
            continue;
        }
        if !in_region_table || trimmed.is_empty() || trimmed.starts_with("==========") {
            continue;
        }

        let Some((name, values)) = parse_vmmap_region_line(line) else {
            continue;
        };
        if values.len() < 2 {
            continue;
        }
        let resident = values[1];
        let name = name.trim();

        if name == "TOTAL" {
            result.total_resident = Some(resident);
            break;
        } else if name == "__TEXT" {
            result.text_resident = Some(resident);
        } else if name == "IOSurface" {
            result.iosurface_resident = Some(resident);
            graphics_resident += resident;
            has_graphics = true;
        } else if name == "IOAccelerator (graphics)" || name == "owned unmapped (graphics)" {
            graphics_resident += resident;
            has_graphics = true;
        } else if name.starts_with("MALLOC") {
            malloc_resident += resident;
            has_malloc = true;
        }
    }

    if has_graphics {
        result.graphics_resident = Some(graphics_resident);
    }
    if has_malloc {
        result.malloc_resident = Some(malloc_resident);
    }
    Ok(result)
}

fn parse_vmmap_region_line(line: &str) -> Option<(String, Vec<u64>)> {
    let tokens: Vec<&str> = line.split_whitespace().collect();
    let first_size = tokens
        .iter()
        .position(|token| parse_vmmap_size(token).is_some())?;
    if first_size == 0 {
        return None;
    }
    let name = tokens[..first_size].join(" ");
    let values = tokens[first_size..]
        .iter()
        .filter_map(|token| parse_vmmap_size(token))
        .collect::<Vec<_>>();
    Some((name, values))
}

fn parse_vmmap_size(token: &str) -> Option<u64> {
    let token = token.trim_end_matches(',');
    let (number, multiplier) = if let Some(number) = token.strip_suffix('K') {
        (number, 1024.0)
    } else if let Some(number) = token.strip_suffix('M') {
        (number, 1024.0 * 1024.0)
    } else if let Some(number) = token.strip_suffix('G') {
        (number, 1024.0 * 1024.0 * 1024.0)
    } else {
        return None;
    };
    number
        .parse::<f64>()
        .ok()
        .map(|value| (value * multiplier).round() as u64)
}

#[cfg(test)]
mod memory_parser_tests {
    use super::*;

    #[test]
    fn vmmap_summary_uses_region_type_table_total() {
        let summary = r#"
ReadOnly portion of Libraries: Total=579.0M resident=421.4M(73%) swapped_out_or_unallocated=157.6M(27%)
Writable regions: Total=81.0M written=19.4M(24%) resident=19.4M(24%) swapped_out=0K(0%) unallocated=61.6M(76%)

                                VIRTUAL RESIDENT    DIRTY  SWAPPED VOLATILE   NONVOL    EMPTY   REGION
REGION TYPE                        SIZE     SIZE     SIZE     SIZE     SIZE     SIZE     SIZE    COUNT (non-coalesced)
===========                     ======= ========    =====  ======= ========   ======    =====  =======
MALLOC metadata                    752K     192K     192K       0K       0K       0K       0K        4
IOSurface                         96.0M    82.6M      64K       0K       0K       0K       0K        2
IOAccelerator (graphics)          64.0M    36.2M      16K       0K       0K       0K       0K        1
__TEXT                           430.0M   421.4M       0K       0K       0K       0K       0K       46
===========                     ======= ========    =====  ======= ========   ======    =====  =======
TOTAL                            802.4M   563.7M    19.8M       0K       0K       0K       0K      261

MALLOC ZONE                         SIZE       SIZE       SIZE       SIZE      COUNT  ALLOCATED  FRAG SIZE  % FRAG   COUNT
===========                      =======  =========  =========  =========  =========  =========  =========  ======  ======
TOTAL                              94.4M      19.3M      19.3M         0K        184        11K       245K     96%       5
"#;

        let parsed = parse_vmmap_summary(summary).unwrap();
        assert_eq!(parsed.total_resident, parse_vmmap_size("563.7M"));
        assert_eq!(parsed.iosurface_resident, parse_vmmap_size("82.6M"));
        assert_eq!(
            parsed.graphics_resident,
            Some(parse_vmmap_size("82.6M").unwrap() + parse_vmmap_size("36.2M").unwrap())
        );
        assert_eq!(parsed.malloc_resident, Some(192 * 1024));
        assert_eq!(parsed.text_resident, parse_vmmap_size("421.4M"));
    }
}

#[cfg(test)]
mod flow_right_tests {
    use super::flow_right;

    #[test]
    fn items_wrap_and_each_line_sits_against_the_right_edge() {
        let (places, lines) = flow_right(&[100.0, 100.0, 100.0], 250.0, 10.0);
        assert_eq!(places, vec![(40.0, 0), (150.0, 0), (150.0, 1)]);
        assert_eq!(lines, 2);
    }

    #[test]
    fn an_item_wider_than_a_line_gets_a_line_of_its_own() {
        let (places, lines) = flow_right(&[300.0, 50.0], 250.0, 10.0);
        assert_eq!(places, vec![(0.0, 0), (200.0, 1)]);
        assert_eq!(lines, 2);
    }

    #[test]
    fn nothing_to_lay_out_is_still_one_line() {
        assert_eq!(flow_right(&[], 250.0, 10.0), (Vec::new(), 1));
    }
}

#[cfg(test)]
mod pinned_panel_tests {
    use super::pinned_beside_column;

    // A view from 50 to 1000, a 600-tall panel, a column ending at 3000
    // when the page is not scrolled.
    const TOP: f32 = 50.0;
    const BOTTOM: f32 = 1000.0;

    #[test]
    fn the_panel_scrolls_with_the_page_until_it_reaches_the_top() {
        assert_eq!(
            pinned_beside_column(400.0, 3000.0, 600.0, TOP, BOTTOM),
            400.0
        );
        assert_eq!(
            pinned_beside_column(-900.0, 1700.0, 600.0, TOP, BOTTOM),
            TOP
        );
    }

    #[test]
    fn the_column_ending_carries_the_panel_off() {
        // The column ends at 500: the panel's bottom goes with it.
        assert_eq!(
            pinned_beside_column(-2100.0, 500.0, 600.0, TOP, BOTTOM),
            -100.0
        );
    }

    #[test]
    fn a_column_shorter_than_the_panel_never_pins_it() {
        assert_eq!(
            pinned_beside_column(-300.0, 0.0, 600.0, TOP, BOTTOM),
            -300.0
        );
    }

    #[test]
    fn a_panel_taller_than_the_view_is_held_by_its_bottom() {
        // 1500 tall in a 950 view: held once its bottom reaches 1000.
        assert_eq!(
            pinned_beside_column(-200.0, 5000.0, 1500.0, TOP, BOTTOM),
            -200.0
        );
        assert_eq!(
            pinned_beside_column(-900.0, 5000.0, 1500.0, TOP, BOTTOM),
            -500.0
        );
    }
}

#[cfg(test)]
mod config_candidate_tests {
    use super::*;

    #[test]
    fn wezterm_import_candidates_do_not_search_thinkterm_config_dir() {
        let candidates = SettingsWindow::wezterm_config_candidates();
        let legacy_user_config_dir = std::env::var_os("XDG_CONFIG_HOME")
            .map(|dir| PathBuf::from(dir).join("wezterm"))
            .unwrap_or_else(|| config::HOME_DIR.join(".config").join("wezterm"));
        assert!(candidates.contains(&legacy_user_config_dir.join("wezterm.lua")));
        assert!(!candidates.contains(
            &config::HOME_DIR
                .join(".config")
                .join("thinkterm")
                .join("wezterm.lua")
        ));
    }

    #[test]
    fn thinkterm_import_entry_defaults_to_thinkterm_lua() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            SettingsWindow::preferred_thinkterm_config_entry(dir.path()),
            dir.path().join("thinkterm.lua")
        );
    }

    #[test]
    fn thinkterm_import_entry_prefers_existing_thinkterm_lua() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("thinkterm.lua"), "").unwrap();
        fs::write(dir.path().join("wezterm.lua"), "").unwrap();
        assert_eq!(
            SettingsWindow::preferred_thinkterm_config_entry(dir.path()),
            dir.path().join("thinkterm.lua")
        );
    }

    #[test]
    fn thinkterm_import_entry_preserves_legacy_wezterm_lua_without_native_file() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("wezterm.lua"), "").unwrap();
        assert_eq!(
            SettingsWindow::preferred_thinkterm_config_entry(dir.path()),
            dir.path().join("wezterm.lua")
        );
    }

    #[test]
    fn non_macos_settings_window_fits_the_active_screen() {
        if !cfg!(target_os = "macos") {
            // 2x-authored 1840x1205 halves to 920x603 on a 96dpi display
            assert_eq!(settings_window_pixel_size(96, None), (920, 603));
            assert_eq!(
                settings_window_pixel_size(96, Some((1920, 1080))),
                (920, 603)
            );
            assert_eq!(
                settings_window_pixel_size(96, Some((1366, 768))),
                (920, 603)
            );
            assert_eq!(settings_window_pixel_size(96, Some((900, 620))), (810, 558));
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct ProcessMemoryInfo {
    /// What the OS considers resident right now: `ri_resident_size` on macOS,
    /// `WorkingSetSize` on Windows.
    resident_size: u64,
    /// macOS `phys_footprint`. There is no Windows equivalent, so it stays
    /// `None` there -- commit charge is a different measure and lives in
    /// `commit_size` rather than being mislabelled as this one.
    physical_footprint: Option<u64>,
    peak_physical_footprint: Option<u64>,
    /// Windows `PrivateUsage`: private bytes committed, which is *not* a
    /// resident measure. `None` on macOS.
    commit_size: Option<u64>,
    /// Windows `PeakWorkingSetSize`. `None` on macOS, where the peak that is
    /// reported is the footprint one above.
    peak_resident_size: Option<u64>,
}

#[cfg(target_os = "macos")]
fn capture_process_memory_info(pid: u32) -> Result<ProcessMemoryInfo, String> {
    const RUSAGE_INFO_V4: libc::c_int = 4;

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct RUsageInfoV4 {
        ri_uuid: [u8; 16],
        ri_user_time: u64,
        ri_system_time: u64,
        ri_pkg_idle_wkups: u64,
        ri_interrupt_wkups: u64,
        ri_pageins: u64,
        ri_wired_size: u64,
        ri_resident_size: u64,
        ri_phys_footprint: u64,
        ri_proc_start_abstime: u64,
        ri_proc_exit_abstime: u64,
        ri_child_user_time: u64,
        ri_child_system_time: u64,
        ri_child_pkg_idle_wkups: u64,
        ri_child_interrupt_wkups: u64,
        ri_child_pageins: u64,
        ri_child_elapsed_abstime: u64,
        ri_diskio_bytesread: u64,
        ri_diskio_byteswritten: u64,
        ri_cpu_time_qos_default: u64,
        ri_cpu_time_qos_maintenance: u64,
        ri_cpu_time_qos_background: u64,
        ri_cpu_time_qos_utility: u64,
        ri_cpu_time_qos_legacy: u64,
        ri_cpu_time_qos_user_initiated: u64,
        ri_cpu_time_qos_user_interactive: u64,
        ri_billed_system_time: u64,
        ri_serviced_system_time: u64,
        ri_logical_writes: u64,
        ri_lifetime_max_phys_footprint: u64,
        ri_instructions: u64,
        ri_cycles: u64,
        ri_billed_energy: u64,
        ri_serviced_energy: u64,
    }

    unsafe extern "C" {
        fn proc_pid_rusage(
            pid: libc::c_int,
            flavor: libc::c_int,
            buffer: *mut libc::c_void,
        ) -> libc::c_int;
    }

    let mut info = std::mem::MaybeUninit::<RUsageInfoV4>::zeroed();
    let result = unsafe {
        proc_pid_rusage(
            pid as libc::c_int,
            RUSAGE_INFO_V4,
            info.as_mut_ptr().cast::<libc::c_void>(),
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }

    let info = unsafe { info.assume_init() };
    Ok(ProcessMemoryInfo {
        resident_size: info.ri_resident_size,
        physical_footprint: Some(info.ri_phys_footprint),
        peak_physical_footprint: Some(info.ri_lifetime_max_phys_footprint),
        commit_size: None,
        peak_resident_size: None,
    })
}

/// Windows has no `phys_footprint`, so this reports the two numbers Task
/// Manager actually shows and keeps them apart: the working set (its default
/// "Memory" column) and the commit charge (its opt-in "Commit size" column).
/// Conflating the two is not academic -- they differ by hundreds of megabytes
/// for this process, and reading the wrong one sends a memory investigation
/// after the wrong cause.
#[cfg(windows)]
fn capture_process_memory_info(pid: u32) -> Result<ProcessMemoryInfo, String> {
    use winapi::um::processthreadsapi::GetCurrentProcess;
    use winapi::um::psapi::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX};

    // This samples the calling process. Say so rather than silently returning
    // our own numbers for someone else's pid: reading another process (the mux
    // server, a pane's child) is a reasonable next step, and it would need
    // OpenProcess plus a handle to close.
    if pid != std::process::id() {
        return Err(format!(
            "Windows memory diagnostics can only sample this process (asked for pid {pid})"
        ));
    }

    let mut counters = std::mem::MaybeUninit::<PROCESS_MEMORY_COUNTERS_EX>::zeroed();
    let size = std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32;

    // GetProcessMemoryInfo takes a PROCESS_MEMORY_COUNTERS pointer; passing the
    // _EX layout plus its larger cb is how PrivateUsage is requested.
    let ok = unsafe {
        GetProcessMemoryInfo(
            GetCurrentProcess(),
            counters.as_mut_ptr() as *mut _,
            size,
        )
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }

    let counters = unsafe { counters.assume_init() };
    Ok(ProcessMemoryInfo {
        resident_size: counters.WorkingSetSize as u64,
        physical_footprint: None,
        peak_physical_footprint: None,
        commit_size: Some(counters.PrivateUsage as u64),
        peak_resident_size: Some(counters.PeakWorkingSetSize as u64),
    })
}

#[cfg(not(any(target_os = "macos", windows)))]
fn capture_process_memory_info(_pid: u32) -> Result<ProcessMemoryInfo, String> {
    Err("memory diagnostics are only wired on macOS and Windows for now".to_string())
}

struct StyleToken<'a> {
    name: &'a str,
    value: &'a str,
    swatch: Option<LinearRgba>,
}

#[derive(Clone, Copy)]
struct SettingsPalette {
    window_bg: LinearRgba,
    sidebar_bg: LinearRgba,
    separator: LinearRgba,
    search_bg: LinearRgba,
    search_border: LinearRgba,
    nav_hover_bg: LinearRgba,
    nav_pressed_bg: LinearRgba,
    nav_selected_bg: LinearRgba,
    nav_selected_border: LinearRgba,
    control_bg: LinearRgba,
    /// The groove a segmented control's segments sit in, and whether this
    /// palette is a dark one -- the two together decide which way a selected
    /// segment has to go to be seen. See `segmented_track_and_fill`.
    track_off: LinearRgba,
    is_dark: bool,
    control_hover_bg: LinearRgba,
    control_pressed_bg: LinearRgba,
    control_border: LinearRgba,
    card_bg: LinearRgba,
    on_accent: LinearRgba,
    title: LinearRgba,
    text: LinearRgba,
    secondary_text: LinearRgba,
    muted_text: LinearRgba,
    selected_text: LinearRgba,
    rule: LinearRgba,
}

/// Open Settings on the Software Update page. This is where the app menu's
/// Check for Updates lands: one page that already carries the status, the
/// automatic-check preference and the release links, rather than a second
/// dialog that can only say one of those things in isolation.
pub fn show_update_page() {
    let switched = SETTINGS_WINDOW.with(|slot| {
        let slot = slot.borrow();
        let SettingsWindowSlot::Open { settings, .. } = &*slot else {
            return false;
        };
        let mut settings = settings.borrow_mut();
        // An Open slot whose window is gone opens a fresh one below, which
        // reads the pending section instead.
        if settings.window.is_none() {
            return false;
        }
        settings.enter_section(SettingsSection::Update);
        if let Some(window) = settings.window.as_ref() {
            window.invalidate();
        }
        true
    });
    if !switched {
        PENDING_SECTION.with(|pending| pending.set(Some(SettingsSection::Update)));
    }
    show();
}

/// Open Settings, remembering which window asked. See `OPENED_FROM`.
pub fn show_from(mux_window_id: MuxWindowId, space_id: &str) {
    remember_origin(mux_window_id, space_id);
    show();
}

/// `show_update_page` asked from a terminal window. The Import page must
/// target that window's Space, not the one Settings was last opened from.
pub fn show_update_page_from(mux_window_id: MuxWindowId, space_id: &str) {
    remember_origin(mux_window_id, space_id);
    show_update_page();
}

fn remember_origin(mux_window_id: MuxWindowId, space_id: &str) {
    OPENED_FROM.with(|slot| slot.set(Some(mux_window_id)));
    let changed = OPENED_FROM_SPACE
        .with(|slot| slot.replace(Some(space_id.to_string())).as_deref() != Some(space_id));
    #[cfg(unix)]
    if changed {
        SETTINGS_WINDOW.with(|slot| {
            if let SettingsWindowSlot::Open { settings, .. } = &*slot.borrow() {
                let mut settings = settings.borrow_mut();
                // A preview is bound to its owner until that import finishes.
                if !settings.ui.session_import.busy() {
                    settings.ui.session_import = session_import::ImportUi::default();
                    settings.ui.import_step = ImportStep::Source;
                }
            }
        });
    }
    #[cfg(not(unix))]
    let _ = changed;
}

pub fn show() {
    let action = SETTINGS_WINDOW.with(|slot| {
        let mut slot = slot.borrow_mut();
        match std::mem::replace(&mut *slot, SettingsWindowSlot::Closed) {
            SettingsWindowSlot::Closed => {
                let instance_id = next_settings_window_id();
                *slot = SettingsWindowSlot::Opening(instance_id);
                SettingsShowAction::Start(instance_id)
            }
            SettingsWindowSlot::Opening(instance_id) => {
                *slot = SettingsWindowSlot::Opening(instance_id);
                SettingsShowAction::Ignore
            }
            SettingsWindowSlot::Open {
                instance_id,
                settings,
            } => {
                let window = settings.borrow().window.clone();
                *slot = SettingsWindowSlot::Open {
                    instance_id,
                    settings,
                };
                match window {
                    Some(window) => SettingsShowAction::Focus(window),
                    None => {
                        let instance_id = next_settings_window_id();
                        *slot = SettingsWindowSlot::Opening(instance_id);
                        SettingsShowAction::Start(instance_id)
                    }
                }
            }
        }
    });

    match action {
        SettingsShowAction::Focus(window) => {
            window.show();
            window.focus();
        }
        SettingsShowAction::Ignore => {}
        SettingsShowAction::Start(instance_id) => {
            promise::spawn::spawn(async move {
                if let Err(err) = SettingsWindow::open(instance_id).await {
                    SETTINGS_WINDOW.with(|slot| {
                        let mut slot = slot.borrow_mut();
                        if matches!(
                            *slot,
                            SettingsWindowSlot::Opening(opening_id)
                                if opening_id == instance_id
                        ) {
                            *slot = SettingsWindowSlot::Closed;
                        }
                    });
                    log::error!("failed to open settings window: {err:#}");
                    wezterm_toast_notification::persistent_toast_notification(
                        &crate::i18n::tr("settings-window-title"),
                        &settings_tr(
                            "settings-window-open-error",
                            &[("error", format!("{err:#}"))],
                        ),
                    );
                }
            })
            .detach();
        }
    }
}

/// One string, shaped and glyph-resolved once for one font.
///
/// `width` is the same sum the painter walks, so a measurement and the paint
/// that follows it can never disagree about how wide a label is.
struct ShapedText {
    glyphs: Vec<Rc<CachedGlyph>>,
    width: f32,
}

/// Identifies a shaped run. The font id covers family, size, weight and DPI,
/// so a settings change that rebuilds the fonts cannot be served stale entries
/// even before the cache is cleared.
#[derive(PartialEq, Eq, Hash)]
struct ShapedTextKey {
    font_id: usize,
    text: String,
}

/// Distinct strings held before the cache is dropped and rebuilt.
///
/// The window's own labels are a fixed set in the low hundreds; the headroom
/// above that is for text the user types into a field, which produces a new
/// string per keystroke and is the only unbounded source here.
const SHAPED_TEXT_CACHE_LIMIT: usize = 2048;
/// Wrapped descriptions kept at once; a page has a few dozen.
const WRAPPED_TEXT_CACHE_LIMIT: usize = 256;

struct SettingsWindow {
    instance_id: u64,
    cleaned_up: bool,
    window: Option<Window>,
    dimensions: Dimensions,
    window_state: WindowState,
    fonts: Rc<FontConfiguration>,
    ui_font: Rc<LoadedFont>,
    /// The same family two weights lighter, for every secondary line. One
    /// weight for labels and explanations made the two read as equally
    /// important; macOS separates them by weight, not only by color.
    body_font: Rc<LoadedFont>,
    import_body_font: Rc<LoadedFont>,
    import_title_font: Rc<LoadedFont>,
    title_font: Rc<LoadedFont>,
    sidebar_title_font: Rc<LoadedFont>,
    logo_caption_font: Rc<LoadedFont>,
    metrics: RenderMetrics,
    render_state: Option<RenderState>,
    webgpu: Option<Rc<WebGpuState>>,
    /// The last paint ran out of passes while the frame was still outgrowing
    /// its atlas or quad buffers, and kept the previous frame instead: paint
    /// again straight away rather than wait for the next event.
    repaint_after_growth: bool,
    /// Last system appearance seen. Not used for painting -- see
    /// `effective_appearance` -- only to notice that a repaint is due.
    appearance: Appearance,
    /// The chrome's colours, resolved when the appearance they come from
    /// moves rather than on each of the fifty-six `palette()` calls a paint
    /// makes. Two things move it: the system appearance (the event below) and
    /// this window's own `theme_mode`, which a dropdown edits directly so that
    /// a pick previews before it is saved. `refresh_chrome` is called from
    /// both.
    chrome_palette: UiPalette,
    selected: SettingsSection,
    agents_expanded: Option<&'static str>,
    native_settings: ThinkTermNativeSettings,
    active_main_renderer: NativeRendererBackend,
    ui: SettingsUiState,
    ui_context: UiContext<SettingsAction>,
    compatibility_import: CompatibilityImportState,
    status: String,
    /// Shaped runs for this window's proportional text.
    ///
    /// This window paints entirely from immediate-mode code: every label is
    /// measured and then drawn from scratch on each frame, and measuring an
    /// over-long one used to shape it once more per step of a binary search.
    /// Shaping is by far the most expensive part of that and the one part that
    /// does not change between frames.
    ///
    /// Latin text hid the cost. Text the UI font does not cover does not:
    /// `blocking_shape` hands those characters to the fallback resolver and
    /// blocks the UI thread until it answers, so scrolling in Chinese or
    /// Japanese dropped frames. Blocking is also why the result is safe to
    /// keep — it returns only once the fallback is in place, so a cached run
    /// is final rather than a first guess to be revised.
    shape_cache: RefCell<HashMap<ShapedTextKey, Rc<ShapedText>>>,
    /// Descriptions as `capped_lines` wrapped them, by font, width, line
    /// limit and a hash of the text, with the text kept to check against.
    wrapped_text:
        RefCell<HashMap<(wezterm_font::LoadedFontId, u32, usize, u64), (String, Vec<String>)>>,
    /// Glyph allocation can discover that the shared texture atlas is full
    /// while a helper is only measuring text and cannot return an error. Keep
    /// the first failure here so the enclosing paint pass can grow the atlas
    /// and retry the whole frame instead of caching a run with missing glyphs.
    pending_glyph_error: RefCell<Option<Error>>,
}

impl SettingsWindow {
    fn ui_px(&self, value: f32) -> f32 {
        scale_ui_f32(value, self.dimensions.dpi)
    }

    /// Design-pixel -> physical-pixel ratio for this window.
    fn ui_scale(&self) -> f32 {
        crate::ui::ui_scale_for_dpi(self.dimensions.dpi)
    }

    fn ui_usize(&self, value: usize) -> usize {
        scale_ui_usize(value, self.dimensions.dpi)
    }

    async fn open(instance_id: u64) -> anyhow::Result<()> {
        let config = configuration();
        let dpi = window::default_dpi() as usize;
        let fonts = Rc::new(FontConfiguration::new(Some(config.clone()), dpi)?);
        let native_settings = Self::load_native_settings();
        let active_main_renderer =
            crate::native_settings::main_window_renderer(&native_settings, config.front_end);
        let settings_font_size = crate::native_settings::settings_font_size(&native_settings);
        let settings_font_weight = crate::native_settings::settings_font_weight(&native_settings);
        let title_font = fonts
            .title_font_with_size_and_weight(settings_font_size + 4.0, settings_font_weight)?;
        let sidebar_title_font = fonts.title_font_with_size_and_weight(
            Self::sidebar_brand_font_size_for_config(&config),
            SIDEBAR_BRAND_FONT_WEIGHT,
        )?;
        let ui_font = fonts
            .command_palette_font_with_size_and_weight(settings_font_size, settings_font_weight)?;
        let body_font = fonts.command_palette_font_with_size_and_weight(
            settings_font_size,
            settings_body_font_weight(settings_font_weight),
        )?;
        let import_body_font = fonts.command_palette_font_with_size_and_weight(
            (settings_font_size - 1.0).max(10.0),
            (settings_font_weight as f32 * 0.67).round().max(350.0) as u16,
        )?;
        let import_title_font = fonts.title_font_with_size_and_weight(
            settings_font_size + 10.0,
            settings_font_weight,
        )?;
        let logo_caption_font = fonts.command_palette_font_with_size_and_weight(
            LOGO_CAPTION_FONT_SIZE,
            LOGO_CAPTION_FONT_WEIGHT,
        )?;
        let metrics = RenderMetrics::with_font_metrics(&ui_font.metrics());
        let appearance = Connection::get()
            .map(|conn| conn.get_appearance())
            .unwrap_or(Appearance::Dark);
        let active_screen_size = if cfg!(target_os = "macos") {
            None
        } else {
            Connection::get()
                .and_then(|conn| conn.screens().ok())
                .map(|screens| {
                    (
                        screens.active.rect.size.width.max(1) as usize,
                        screens.active.rect.size.height.max(1) as usize,
                    )
                })
        };
        let (pixel_width, pixel_height) = settings_window_pixel_size(dpi, active_screen_size);
        let dimensions = Dimensions {
            pixel_width,
            pixel_height,
            dpi,
        };

        let mut ui = SettingsUiState::new(dpi);
        if let Some(offset) = initial_scroll() {
            ui.content_scroll.offset = offset;
            ui.content_scroll.target_offset = offset;
        }
        // Must go through `set_text_end`: assigning `.text` leaves the caret at
        // 0, so typing into a prefilled field would insert at the front and
        // Backspace would do nothing.
        ui.font_size_input.set_text_end(
            native_settings
                .terminal
                .font_size
                .map(|value| format!("{value:.1}"))
                .unwrap_or_else(|| format!("{:.1}", config.font_size)),
        );
        ui.font_family_input.set_text_end(
            native_settings
                .terminal
                .font_family
                .clone()
                .unwrap_or_else(|| Self::effective_font_family(&config)),
        );
        ui.remote_drop_input
            .set_text_end(crate::native_settings::remote_drop_destination());
        // Discovered once here as well as on entering the Terminal section:
        // the window can open straight onto Terminal, and the search box can
        // jump to it without passing through the section action. A handful of
        // path probes, and never from the paint path.
        ui.shell_catalog = shell_catalog_including(native_settings.terminal.default_shell.as_ref());
        ui.quote_preview = quote_preview_text(&native_settings);
        // Same reasoning for the update cache: the window can open straight
        // onto Update or About, and both read it while painting.
        ui.update_status = Some(crate::update::cached_update_status());
        ui.update_method = Some(thinkterm_update::InstallMethod::detect());

        let settings = Rc::new(RefCell::new(Self {
            instance_id,
            cleaned_up: false,
            window: None,
            dimensions,
            window_state: WindowState::default(),
            fonts: Rc::clone(&fonts),
            ui_font,
            body_font,
            import_body_font,
            import_title_font,
            title_font,
            sidebar_title_font,
            logo_caption_font,
            metrics,
            render_state: None,
            webgpu: None,
            repaint_after_growth: false,
            appearance,
            chrome_palette: crate::native_settings::chrome_palette(
                native_settings.appearance.theme_mode,
                native_settings
                    .appearance
                    .theme_mode
                    .effective_appearance(appearance),
                &config,
                None,
            ),
            selected: initial_section(),
            agents_expanded: initial_expanded_agent(),
            native_settings,
            active_main_renderer,
            ui,
            ui_context: UiContext::default(),
            compatibility_import: CompatibilityImportState::default(),
            status: Self::initial_status(),
            shape_cache: RefCell::new(HashMap::new()),
            wrapped_text: RefCell::new(HashMap::new()),
            pending_glyph_error: RefCell::new(None),
        }));

        let event_settings = Rc::clone(&settings);
        let geometry = RequestedWindowGeometry {
            width: Dimension::Pixels(dimensions.pixel_width as f32),
            height: Dimension::Pixels(dimensions.pixel_height as f32),
            x: None,
            y: None,
            macos_frame_autosave_name: None,
            // The Settings window is not the main window: it neither restores
            // a remembered frame nor records one.
            windows_frame_rect: None,
            origin: GeometryOrigin::default(),
        };

        let title = crate::i18n::tr("settings-window-title");
        let window = Window::new_window(
            "thinkterm-settings",
            &title,
            geometry,
            Some(&config),
            Rc::clone(&fonts),
            move |event, window| {
                if let Err(err) = event_settings.borrow_mut().dispatch(event, window) {
                    log::error!("settings window event failed: {err:#}");
                }
            },
        )
        .await?;

        window.set_title(&title);
        let render_state = crate::renderer_choice::bring_up(
            active_main_renderer,
            config.front_end != config::FrontEndSelection::Software,
            async {
                let webgpu = Rc::new(WebGpuState::new(&window, dimensions, &config).await?);
                let settings = settings.borrow();
                // Window creation can already have delivered a resize or DPI
                // change. Keep the latest dimensions and font metrics.
                webgpu.resize(settings.dimensions);
                RenderState::new(
                    RenderContext::WebGpu(webgpu),
                    &settings.fonts,
                    &settings.metrics,
                    256,
                )
            },
            async {
                let gl = window.enable_opengl().await?;
                let settings = settings.borrow();
                RenderState::new(
                    RenderContext::Glium(gl),
                    &settings.fonts,
                    &settings.metrics,
                    256,
                )
            },
        )
        .await;
        let render_state = match render_state {
            Ok(render_state) => render_state,
            Err(err) => {
                window.close();
                return Err(err);
            }
        };
        {
            let mut settings = settings.borrow_mut();
            if let RenderContext::WebGpu(webgpu) = &render_state.context {
                settings.dimensions = *webgpu.dimensions.borrow();
                settings.webgpu = Some(Rc::clone(webgpu));
            }
            settings.render_state = Some(render_state);
            settings.window = Some(window.clone());
        }

        let installed = SETTINGS_WINDOW.with(|slot| {
            let mut slot = slot.borrow_mut();
            if matches!(
                *slot,
                SettingsWindowSlot::Opening(opening_id) if opening_id == instance_id
            ) {
                *slot = SettingsWindowSlot::Open {
                    instance_id,
                    settings,
                };
                true
            } else {
                false
            }
        });
        if !installed {
            window.close();
            return Ok(());
        }

        #[cfg(debug_assertions)]
        if std::env::var("THINKTERM_SETTINGS_EXPAND")
            .is_ok_and(|value| value == "import-preview")
        {
            if let Some(settings) = settings_window_for_instance(instance_id) {
                let mut settings = settings.borrow_mut();
                if settings.selected == SettingsSection::Import {
                    settings.perform_import_navigation(SettingsAction::ImportContinue, &window);
                }
            }
        }

        window.show();
        window.invalidate();

        Ok(())
    }

    fn cleanup(&mut self) {
        if self.cleaned_up {
            return;
        }
        self.cleaned_up = true;
        self.commit_focused_input();
        // Closed mid-drag: what the windows are previewing is what is kept.
        if matches!(self.ui.drag, Some(SettingsDrag::WindowOpacity)) {
            self.ui.drag = None;
            self.commit_window_opacity();
        }
        self.ui.memory_monitoring = false;
        self.ui.memory_monitor_generation = self.ui.memory_monitor_generation.wrapping_add(1);
        self.render_state.take();
        self.webgpu.take();
        self.window.take();

        let instance_id = self.instance_id;
        SETTINGS_WINDOW.with(|slot| {
            let mut slot = slot.borrow_mut();
            let current = std::mem::replace(&mut *slot, SettingsWindowSlot::Closed);
            match current {
                SettingsWindowSlot::Open {
                    instance_id: current_id,
                    ..
                } if current_id == instance_id => {}
                other => *slot = other,
            }
        });
    }

    fn dispatch(&mut self, event: WindowEvent, window: &Window) -> anyhow::Result<bool> {
        match event {
            WindowEvent::CloseRequested => {
                self.cleanup();
                window.close();
                Ok(true)
            }
            WindowEvent::Destroyed => {
                self.cleanup();
                Ok(true)
            }
            WindowEvent::Resized {
                dimensions,
                window_state,
                ..
            } => {
                let dpi_changed = self.dimensions.dpi != dimensions.dpi;
                self.dimensions = dimensions;
                self.window_state = window_state;
                if dpi_changed {
                    let old_default_width = self.ui.tokens.sidebar_default_width.max(1.0);
                    let tokens = UiTokens::for_dpi(dimensions.dpi);
                    let width_ratio = tokens.sidebar_default_width / old_default_width;
                    self.ui.sidebar = ResizablePaneState::new(
                        self.ui.sidebar.width * width_ratio,
                        tokens.sidebar_min_width,
                        tokens.sidebar_max_width,
                    );
                    self.ui.tokens = tokens;
                    self.fonts
                        .change_scaling(self.fonts.get_font_scale(), dimensions.dpi);
                    self.reload_settings_fonts()?;
                    if let Some(render_state) = self.render_state.as_mut() {
                        render_state.recreate_texture_atlas(&self.fonts, &self.metrics, None)?;
                    }
                    self.invalidate_shaped_text();
                }
                if let Some(webgpu) = self.webgpu.as_ref() {
                    webgpu.resize(dimensions);
                }
                self.clamp_sidebar_to_window();
                window.invalidate();
                Ok(true)
            }
            WindowEvent::NeedRepaint => Ok(self.do_paint(window)),
            WindowEvent::MouseEvent(event) => {
                self.mouse_event(event, window);
                Ok(true)
            }
            WindowEvent::KeyEvent(event) => {
                if self.key_event(event, window) {
                    window.invalidate();
                }
                Ok(true)
            }
            WindowEvent::MouseLeave => {
                self.ui.interaction.hovered = None;
                self.ui.interaction.pressed = None;
                if matches!(self.ui.drag, Some(SettingsDrag::WindowOpacity)) {
                    self.commit_window_opacity();
                }
                self.ui.drag = None;
                window.set_cursor(Some(MouseCursor::Arrow));
                window.invalidate();
                Ok(true)
            }
            WindowEvent::AppearanceChanged(appearance) => {
                self.appearance = appearance;
                self.refresh_chrome();
                window.invalidate();
                Ok(true)
            }
            WindowEvent::DraggedFile { coords, .. } => {
                let target =
                    coords.and_then(|at| self.tab_icon_drop_target_at(at.x as f32, at.y as f32));
                if self.ui.tab_icon_drop_target != target {
                    self.ui.tab_icon_drop_target = target;
                    window.invalidate();
                }
                Ok(true)
            }
            WindowEvent::DragLeave => {
                if self.ui.tab_icon_drop_target.take().is_some() {
                    window.invalidate();
                }
                Ok(true)
            }
            WindowEvent::DroppedFile { paths, coords } => {
                let target =
                    coords.and_then(|at| self.tab_icon_drop_target_at(at.x as f32, at.y as f32));
                self.ui.tab_icon_drop_target = None;
                if let (Some(target), Some(path)) = (target, paths.first()) {
                    let card = match target {
                        TabIconDropTarget::Card(index) => {
                            self.ui.tab_icon_cards.get(index).cloned()
                        }
                        TabIconDropTarget::Editor => self.ui.tab_icon_selected.clone(),
                        TabIconDropTarget::NewCard => None,
                    };
                    if card.is_some() || target == TabIconDropTarget::NewCard {
                        self.import_tab_icon_svg(card, path.clone());
                    }
                }
                window.invalidate();
                Ok(true)
            }
            _ => Ok(true),
        }
    }

    fn mouse_event(&mut self, event: MouseEvent, window: &Window) {
        let x = event.coords.x as f32;
        let y = event.coords.y as f32;
        let hit = self.action_at(x, y);
        let action = hit.map(|target| target.action);

        match event.kind {
            MouseEventKind::Move => {
                if action.is_none() && self.settings_window_chrome_drag_hit(x, y) {
                    window.set_window_drag_position(event.screen_coords);
                } else if let Some(target) =
                    hit.filter(|target| target.action == SettingsAction::WindowMaximize)
                {
                    let bounds: window::ScreenRect = euclid::rect(
                        target.rect.origin.x as isize
                            - (event.coords.x as isize - event.screen_coords.x),
                        target.rect.origin.y as isize
                            - (event.coords.y as isize - event.screen_coords.y),
                        target.rect.size.width as isize,
                        target.rect.size.height as isize,
                    );
                    window.set_maximize_button_position(bounds);
                }

                if let Some(SettingsDrag::SidebarResize {
                    start_x,
                    start_width,
                }) = self.ui.drag
                {
                    self.ui.sidebar.set_width(start_width + x - start_x);
                    window.invalidate();
                    return;
                }
                if let Some(SettingsDrag::WindowOpacity) = self.ui.drag {
                    self.drag_window_opacity(x);
                    window.invalidate();
                    return;
                }

                if self.ui.interaction.hovered != action {
                    self.ui.interaction.hovered = action;
                    window.set_cursor(Some(match hit.map(|target| target.kind) {
                        Some(WidgetKind::ResizeHandle) => MouseCursor::SizeLeftRight,
                        Some(WidgetKind::TextInput) => MouseCursor::Text,
                        Some(
                            WidgetKind::Button
                            | WidgetKind::SidebarRow
                            | WidgetKind::PreviewControl,
                        ) => MouseCursor::Hand,
                        Some(WidgetKind::ScrollArea | WidgetKind::Hint) | None => {
                            MouseCursor::Arrow
                        }
                    }));
                    window.invalidate();
                }
            }
            MouseEventKind::Press(MousePress::Left) => {
                self.ui.interaction.hovered = action;
                self.ui.interaction.pressed = action;
                match action {
                    Some(
                        SettingsAction::SearchInput
                        | SettingsAction::FontFamilyInput
                        | SettingsAction::RemoteDropDestinationInput
                        | SettingsAction::TabIconProgramInput
                        | SettingsAction::TabIconNameInput
                        | SettingsAction::TabIconCircleInput
                        | SettingsAction::TabIconGlyphColorInput
                        | SettingsAction::TabIconSearchInput,
                    ) => {
                        self.set_focused_input(action);
                        self.ui.open_dropdown = None;
                    }
                    Some(SettingsAction::SidebarResize) => {
                        self.set_focused_input(None);
                        self.ui.drag = Some(SettingsDrag::SidebarResize {
                            start_x: x,
                            start_width: self.ui.sidebar.width,
                        });
                        self.ui.open_dropdown = None;
                    }
                    Some(SettingsAction::WindowOpacitySlider) => {
                        self.set_focused_input(None);
                        self.ui.open_dropdown = None;
                        self.ui.drag = Some(SettingsDrag::WindowOpacity);
                        self.drag_window_opacity(x);
                    }
                    Some(
                        SettingsAction::SetThemeMode(_)
                        | SettingsAction::ToggleLanguageMenu
                        | SettingsAction::SetLanguage(_)
                        | SettingsAction::SetAppIcon(_)
                        | SettingsAction::ToggleMainRendererMenu
                        | SettingsAction::SetMainRenderer(_)
                        | SettingsAction::ToggleDefaultShellMenu
                        | SettingsAction::SetDefaultShell(_)
                        | SettingsAction::ToggleUiFontMenu
                        | SettingsAction::SetUiFont(_)
                        | SettingsAction::ToggleCommandPaletteHotkeyMenu
                        | SettingsAction::SetCommandPaletteHotkey(_)
                        | SettingsAction::ToggleWebLinkTtlMenu
                        | SettingsAction::SetWebLinkTtl(_)
                        | SettingsAction::TogglePluginBackgroundMenu(_)
                        | SettingsAction::SetPluginBackground(..)
                        | SettingsAction::DropdownMenuBackdrop,
                    ) => {
                        self.set_focused_input(None);
                    }
                    Some(_) => {
                        self.set_focused_input(None);
                        self.ui.open_dropdown = None;
                    }
                    None => {
                        self.set_focused_input(None);
                        self.ui.open_dropdown = None;
                        if self.settings_window_chrome_drag_hit(x, y) {
                            window.set_window_drag_position(event.screen_coords);
                            window.request_drag_move();
                        }
                    }
                }
                window.invalidate();
            }
            MouseEventKind::Release(MousePress::Left) => {
                let pressed = self.ui.interaction.pressed.take();
                if matches!(self.ui.drag, Some(SettingsDrag::WindowOpacity)) {
                    self.commit_window_opacity();
                }
                self.ui.drag = None;
                self.ui.interaction.hovered = action;
                if pressed.is_some() && pressed == action {
                    self.perform_action(action.unwrap(), window);
                }
                window.invalidate();
            }
            MouseEventKind::VertWheel(_) | MouseEventKind::HorzWheel(_)
                if matches!(event.kind, MouseEventKind::VertWheel(_))
                    || event.precise_scroll_delta.is_some() =>
            {
                if self.scroll_event(&event, window) {
                    window.invalidate();
                }
            }
            _ if event.mouse_buttons == MouseButtons::NONE => {
                if self.ui.interaction.pressed.take().is_some() {
                    window.invalidate();
                }
            }
            _ => {}
        }
    }

    fn action_at(&self, x: f32, y: f32) -> Option<crate::ui::HitTarget<SettingsAction>> {
        self.ui_context.hit_test(x, y)
    }

    fn scroll_event(&mut self, event: &MouseEvent, window: &Window) -> bool {
        let sidebar_width = self.ui.sidebar.width;
        let sidebar_area = rect(
            0.0,
            self.ui_px(HEADER_HEIGHT),
            sidebar_width,
            self.content_bottom(),
        );
        let content_top = self.content_scroll_area_top();
        let content_area = rect(
            sidebar_width + 1.0,
            content_top,
            self.dimensions.pixel_width as f32 - sidebar_width - 1.0,
            (self.content_bottom() - content_top).max(0.0),
        );
        let ui_scale = self.ui_scale();
        // The open font menu takes the wheel over it, and keeps it: the
        // page underneath must not move while its list is at an end.
        if let Some(menu) = self
            .ui
            .ui_font_menu_rect
            .filter(|_| self.ui.open_dropdown == Some(SettingsDropdown::UiFont))
        {
            if crate::ui::contains(menu, event.coords.x as f32, event.coords.y as f32) {
                return crate::ui::apply_wheel_to_area(
                    event,
                    menu,
                    &mut self.ui.ui_font_menu_scroll,
                    ui_scale,
                );
            }
        }
        if crate::ui::apply_wheel_to_area(
            event,
            sidebar_area,
            &mut self.ui.sidebar_scroll,
            ui_scale,
        ) {
            self.show_sidebar_scrollbar(window);
            return true;
        }
        if crate::ui::apply_wheel_to_area(
            event,
            content_area,
            &mut self.ui.content_scroll,
            ui_scale,
        ) {
            self.show_content_scrollbar(window);
            return true;
        }
        false
    }

    fn show_sidebar_scrollbar(&mut self, window: &Window) {
        self.ui.sidebar_scrollbar_visible_until =
            Some(Instant::now() + std::time::Duration::from_millis(900));
        Self::schedule_scrollbar_hide(window);
    }

    fn show_content_scrollbar(&mut self, window: &Window) {
        self.ui.content_scrollbar_visible_until =
            Some(Instant::now() + std::time::Duration::from_millis(900));
        Self::schedule_scrollbar_hide(window);
    }

    fn schedule_scrollbar_hide(window: &Window) {
        let window = window.clone();
        promise::spawn::spawn_into_main_thread(async move {
            smol::Timer::after(std::time::Duration::from_millis(930)).await;
            window.invalidate();
        })
        .detach();
    }

    fn schedule_memory_monitor_tick(&self, window: &Window, generation: u64) {
        let window = window.clone();
        let instance_id = self.instance_id;
        promise::spawn::spawn_into_main_thread(async move {
            smol::Timer::after(Duration::from_millis(1500)).await;
            if let Some(settings) = settings_window_for_instance(instance_id) {
                let mut settings = settings.borrow_mut();
                if !settings.ui.memory_monitoring
                    || settings.ui.memory_monitor_generation != generation
                {
                    return;
                }
                let snapshot = capture_memory_snapshot(false);
                log::info!("settings memory diagnostics: {}", snapshot.log_line());
                settings.ui.memory_snapshot = Some(snapshot);
                window.invalidate();
                settings.schedule_memory_monitor_tick(&window, generation);
            }
        })
        .detach();
    }

    fn settings_resource_lines(&self) -> Vec<String> {
        let mut lines = vec![format!(
            "Settings window: backend={} size={}x{} dpi={}",
            match self.render_state.as_ref().map(|state| &state.context) {
                Some(RenderContext::WebGpu(_)) => "WebGpu",
                Some(RenderContext::Glium(_)) => "OpenGL",
                None => "none",
            },
            self.dimensions.pixel_width,
            self.dimensions.pixel_height,
            self.dimensions.dpi
        )];

        if let Some(render_state) = self.render_state.as_ref() {
            let stats = render_state.stats();
            lines.push(format!(
                "Settings window: render_backend={} atlas={} glyphs={} svg_icons={} rotated_icons={} images={} frames={} blocks={} colors={} cursor_glyphs={}",
                stats.backend,
                stats.atlas_size,
                stats.glyphs,
                stats.svg_icons,
                stats.rotated_svg_icons,
                stats.decoded_images,
                stats.image_frames,
                stats.block_glyphs,
                stats.color_sprites,
                stats.cursor_glyphs,
            ));
            lines.push(format!(
                "Settings window: layers={} vertex_buffers={} quad_capacity={} line_glyphs={}",
                stats.layers, stats.vertex_buffers, stats.layer_quads, stats.line_glyphs
            ));
        } else {
            lines.push("Settings window: render_state=none".to_string());
        }

        lines
    }

    fn memory_resource_lines(&self) -> Vec<String> {
        let mut lines = self.settings_resource_lines();
        if self.ui.main_window_resource_lines.is_empty() {
            lines.push("Main windows: not captured yet; use Refresh Now".to_string());
        } else {
            lines.extend(self.ui.main_window_resource_lines.clone());
        }
        lines
    }

    fn request_main_window_resource_stats(&mut self) {
        let Some(front_end) = crate::frontend::try_front_end() else {
            self.ui.main_window_resource_lines =
                vec!["Main windows: frontend unavailable".to_string()];
            return;
        };
        let windows = front_end.gui_windows();
        if windows.is_empty() {
            self.ui.main_window_resource_lines = vec!["Main windows: none".to_string()];
            return;
        }

        self.ui.main_window_resource_lines =
            vec![format!("Main windows: {} pending", windows.len())];
        let instance_id = self.instance_id;
        for (idx, gui_window) in windows.into_iter().enumerate() {
            let label = format!("Main window {}", idx + 1);
            gui_window
                .window
                .notify(crate::termwindow::TermWindowNotif::Apply(Box::new(
                    move |term_window| {
                        let lines = term_window.memory_resource_lines(&label);
                        if let Some(settings) = settings_window_for_instance(instance_id) {
                            let mut settings = settings.borrow_mut();
                            settings
                                .ui
                                .main_window_resource_lines
                                .retain(|line| !line.contains(" pending"));
                            settings.ui.main_window_resource_lines.extend(lines);
                            if let Some(window) = settings.window.as_ref() {
                                window.invalidate();
                            }
                        }
                    },
                )));
        }
    }

    fn schedule_copied_state_clear(&self, window: &Window) {
        let window = window.clone();
        let instance_id = self.instance_id;
        promise::spawn::spawn_into_main_thread(async move {
            smol::Timer::after(Duration::from_millis(1450)).await;
            if let Some(settings) = settings_window_for_instance(instance_id) {
                let mut settings = settings.borrow_mut();
                if settings
                    .ui
                    .memory_snapshot_copied_until
                    .is_some_and(|until| Instant::now() >= until)
                {
                    settings.ui.memory_snapshot_copied_until = None;
                    window.invalidate();
                }
                if settings
                    .ui
                    .update_checked_until
                    .is_some_and(|until| Instant::now() >= until)
                {
                    settings.ui.update_checked_until = None;
                    window.invalidate();
                }
                if crate::web_settings::state().copied {
                    crate::web_settings::clear_copied();
                    window.invalidate();
                }
                if settings
                    .ui
                    .version_info_copied_until
                    .is_some_and(|until| Instant::now() >= until)
                {
                    settings.ui.version_info_copied_until = None;
                    window.invalidate();
                }
                if settings
                    .ui
                    .input_diagnostics_copied_until
                    .is_some_and(|until| Instant::now() >= until)
                {
                    settings.ui.input_diagnostics_copied_until = None;
                    window.invalidate();
                }
            }
        })
        .detach();
    }

    fn key_event(&mut self, event: KeyEvent, window: &Window) -> bool {
        if !event.key_is_down {
            return false;
        }

        let Some(focused) = self.ui.interaction.focused else {
            return false;
        };

        if let Some(handled) = self.handle_focused_input_key(focused, &event, window) {
            return handled;
        }

        if event
            .modifiers
            .intersects(Modifiers::SUPER | Modifiers::CTRL | Modifiers::ALT)
        {
            return false;
        }

        match event.key {
            KeyCode::Char('\u{8}') | KeyCode::Char('\u{7f}') => {
                self.backspace_focused_input(focused)
            }
            KeyCode::Char('\r') if focused == SettingsAction::TabIconProgramInput => {
                self.commit_tab_icon_input(focused);
                true
            }
            KeyCode::Char('\u{1b}') | KeyCode::Char('\r') => {
                self.set_focused_input(None);
                true
            }
            KeyCode::Char(ch) => {
                if !ch.is_control() {
                    self.push_focused_input(focused, &ch.to_string())
                } else {
                    false
                }
            }
            KeyCode::Composed(text) => self.push_focused_input(focused, &text),
            _ => false,
        }
    }

    /// Copies the selection when there is one, otherwise the whole field —
    /// matching `SshHostsView::copy_text`.
    fn copy_focused_input(&mut self, focused: SettingsAction, window: &Window) -> bool {
        let Some(input) = self.input_state_for(focused) else {
            return false;
        };
        let text = input
            .caret_selected_text()
            .unwrap_or_else(|| input.text().to_string());
        if text.is_empty() {
            return false;
        }
        window.set_clipboard(Clipboard::Clipboard, text);
        true
    }

    /// Cut only ever acts on a real selection, like every native text field.
    /// Routed through `edit_focused_input` so each field's side effects (search
    /// re-filter, font-family dirty flag) still run.
    fn cut_focused_input(&mut self, focused: SettingsAction, window: &Window) -> bool {
        let mut taken = None;
        self.edit_focused_input(focused, |input| taken = input.caret_take_selected_text());
        let Some(text) = taken.filter(|text| !text.is_empty()) else {
            return false;
        };
        window.set_clipboard(Clipboard::Clipboard, text);
        true
    }

    fn paste_focused_input_from_clipboard(
        &mut self,
        focused: SettingsAction,
        window: &Window,
    ) -> bool {
        let future = window.get_clipboard(Clipboard::Clipboard);
        let window = window.clone();
        let instance_id = self.instance_id;
        promise::spawn::spawn(async move {
            if let Ok(text) = future.await {
                promise::spawn::spawn_into_main_thread(async move {
                    if let Some(settings) = settings_window_for_instance(instance_id) {
                        let mut settings = settings.borrow_mut();
                        if settings.ui.interaction.focused == Some(focused)
                            && settings.push_focused_input(focused, &text)
                        {
                            window.invalidate();
                        }
                    }
                })
                .detach();
            }
        })
        .detach();
        true
    }

    /// Width of `text[..char_idx]` using the same measurement the renderer
    /// uses, so a caret never drifts from the glyphs it sits between.
    fn text_width_to_char(&self, font: &Rc<LoadedFont>, text: &str, char_idx: usize) -> f32 {
        let byte_idx = text
            .char_indices()
            .nth(char_idx)
            .map(|(idx, _)| idx)
            .unwrap_or(text.len());
        self.measure_text_width(font, &text[..byte_idx])
    }

    fn input_state_for(&self, action: SettingsAction) -> Option<&TextInputState> {
        match action {
            SettingsAction::SearchInput => Some(&self.ui.search),
            SettingsAction::FontFamilyInput => Some(&self.ui.font_family_input),
            SettingsAction::RemoteDropDestinationInput => Some(&self.ui.remote_drop_input),
            SettingsAction::TabIconProgramInput => Some(&self.ui.tab_icon_program_input),
            SettingsAction::TabIconNameInput => Some(&self.ui.tab_icon_name_input),
            SettingsAction::TabIconCircleInput => Some(&self.ui.tab_icon_circle_input),
            SettingsAction::TabIconGlyphColorInput => Some(&self.ui.tab_icon_glyph_input),
            SettingsAction::TabIconSearchInput => Some(&self.ui.tab_icon_search),
            _ => None,
        }
    }

    /// Caret + selection to draw for `action`, or `None` when it is not focused.
    fn caret_for_input(&self, action: SettingsAction) -> Option<InputCaret> {
        if self.ui.interaction.focused != Some(action) {
            return None;
        }
        let input = self.input_state_for(action)?;
        Some(InputCaret {
            cursor: input.cursor,
            selection: input.caret_selection_range(),
        })
    }

    fn backspace_focused_input(&mut self, focused: SettingsAction) -> bool {
        self.edit_focused_input(focused, |input| input.caret_backspace())
    }

    /// Run `f` against the focused field's caret model, then apply that
    /// field's side effects (search re-filter / font-family dirty flag).
    /// Every editing key routes through here so the two inputs can never
    /// drift apart again.
    fn edit_focused_input(
        &mut self,
        focused: SettingsAction,
        f: impl FnOnce(&mut TextInputState),
    ) -> bool {
        match focused {
            SettingsAction::SearchInput => {
                f(&mut self.ui.search);
                self.ui.sidebar_scroll.reset();
                self.sync_selected_section_with_search();
                true
            }
            SettingsAction::FontFamilyInput => {
                f(&mut self.ui.font_family_input);
                self.ui.font_family_input_dirty = true;
                true
            }
            SettingsAction::RemoteDropDestinationInput => {
                f(&mut self.ui.remote_drop_input);
                self.ui.remote_drop_input_dirty = true;
                true
            }
            SettingsAction::TabIconProgramInput => {
                f(&mut self.ui.tab_icon_program_input);
                true
            }
            SettingsAction::TabIconNameInput => {
                f(&mut self.ui.tab_icon_name_input);
                self.ui.tab_icon_name_input_dirty = true;
                true
            }
            SettingsAction::TabIconCircleInput => {
                f(&mut self.ui.tab_icon_circle_input);
                self.ui.tab_icon_circle_input_dirty = true;
                true
            }
            SettingsAction::TabIconGlyphColorInput => {
                f(&mut self.ui.tab_icon_glyph_input);
                self.ui.tab_icon_glyph_input_dirty = true;
                true
            }
            SettingsAction::TabIconSearchInput => {
                f(&mut self.ui.tab_icon_search);
                true
            }
            _ => false,
        }
    }

    /// Editing keys for the focused input. `None` means "not an editing key",
    /// so the caller can fall through to its own handling.
    fn handle_focused_input_key(
        &mut self,
        focused: SettingsAction,
        event: &KeyEvent,
        window: &Window,
    ) -> Option<bool> {
        let edit = EditModifiers::from(event.modifiers);
        let shift = edit.shift;
        let macos = cfg!(target_os = "macos");

        if edit.command {
            match event.key {
                KeyCode::Char('a') | KeyCode::Char('A') => {
                    return Some(
                        self.edit_focused_input(focused, |input| input.caret_select_all()),
                    );
                }
                KeyCode::Char('c') | KeyCode::Char('C') => {
                    return Some(self.copy_focused_input(focused, window));
                }
                KeyCode::Char('x') | KeyCode::Char('X') => {
                    return Some(self.cut_focused_input(focused, window));
                }
                KeyCode::Char('v') | KeyCode::Char('V') => {
                    return Some(self.paste_focused_input_from_clipboard(focused, window));
                }
                KeyCode::LeftArrow if macos => {
                    return Some(
                        self.edit_focused_input(focused, |input| input.caret_move_home(shift)),
                    );
                }
                KeyCode::RightArrow if macos => {
                    return Some(
                        self.edit_focused_input(focused, |input| input.caret_move_end(shift)),
                    );
                }
                _ => {}
            }
        }

        if edit.word {
            match event.key {
                KeyCode::LeftArrow => {
                    return Some(
                        self.edit_focused_input(focused, |input| input.caret_word_left(shift)),
                    );
                }
                KeyCode::RightArrow => {
                    return Some(
                        self.edit_focused_input(focused, |input| input.caret_word_right(shift)),
                    );
                }
                _ => {}
            }
        }

        if !edit.plain() {
            return None;
        }

        match event.key {
            KeyCode::LeftArrow => {
                Some(self.edit_focused_input(focused, |input| input.caret_move_left(shift)))
            }
            KeyCode::RightArrow => {
                Some(self.edit_focused_input(focused, |input| input.caret_move_right(shift)))
            }
            KeyCode::Home => {
                Some(self.edit_focused_input(focused, |input| input.caret_move_home(shift)))
            }
            KeyCode::End => {
                Some(self.edit_focused_input(focused, |input| input.caret_move_end(shift)))
            }
            _ => None,
        }
    }

    fn push_focused_input(&mut self, focused: SettingsAction, text: &str) -> bool {
        let text = text.to_string();
        self.edit_focused_input(focused, move |input| input.caret_insert(&text, false))
    }

    fn set_focused_input(&mut self, focused: Option<SettingsAction>) {
        if self.ui.interaction.focused != focused {
            self.commit_focused_input();
        }
        self.ui.interaction.focused = focused;
    }

    fn commit_focused_input(&mut self) {
        if self.ui.interaction.focused == Some(SettingsAction::FontFamilyInput)
            && self.commit_native_terminal_inputs_from_ui()
        {
            self.save_and_apply_native_terminal_settings();
        }
        if self.ui.interaction.focused == Some(SettingsAction::RemoteDropDestinationInput) {
            self.commit_remote_drop_destination_from_ui();
        }
        if let Some(focused) = self.ui.interaction.focused {
            self.commit_tab_icon_input(focused);
        }
    }

    fn commit_remote_drop_destination_from_ui(&mut self) {
        if !self.ui.remote_drop_input_dirty {
            return;
        }
        self.ui.remote_drop_input_dirty = false;
        let value = self.ui.remote_drop_input.text().to_string();
        if let Err(err) = crate::native_settings::set_remote_drop_destination(&value) {
            log::error!("failed to save the remote drop destination: {err:#}");
        }
        // Re-read so the field shows what will actually happen — an emptied
        // field snaps back to the default it now means.
        self.ui
            .remote_drop_input
            .set_text_end(crate::native_settings::remote_drop_destination());
    }

    fn commit_native_terminal_inputs_from_ui(&mut self) -> bool {
        if !self.ui.font_family_input_dirty {
            return false;
        }
        self.ui.font_family_input_dirty = false;

        let family = self.ui.font_family_input.text().trim();
        let next = if family.is_empty() {
            None
        } else {
            Some(family.to_string())
        };
        if self.native_settings.terminal.font_family == next {
            return false;
        }
        self.native_settings.terminal.font_family = next;
        true
    }

    fn sync_selected_section_with_search(&mut self) {
        let sections = self.filtered_sections();
        if !sections.is_empty() && !sections.contains(&self.selected) {
            self.selected = sections[0];
            if self.selected == SettingsSection::Terminal {
                // Reached without a Select action, so the catalog would
                // otherwise still be whatever the window opened with.
                self.refresh_shell_catalog();
            }
            self.ui.content_scroll.reset();
        }
    }

    fn current_terminal_font_size_value(&self) -> f64 {
        self.ui
            .font_size_input
            .text()
            .trim()
            .parse::<f64>()
            .ok()
            .or(self.native_settings.terminal.font_size)
            .unwrap_or_else(|| configuration().font_size)
    }

    fn step_terminal_font_size(&mut self, delta: f64) {
        let value = (self.current_terminal_font_size_value() + delta).clamp(8.0, 48.0);
        self.ui.font_size_input.set_text_end(format!("{value:.1}"));
        self.native_settings.terminal.font_size = Some(value);
        self.save_and_apply_native_terminal_settings();
    }

    fn reset_terminal_font_size(&mut self) {
        let config = configuration();
        self.ui
            .font_size_input
            .set_text_end(format!("{:.1}", config.font_size));
        self.native_settings.terminal.font_size = None;
        self.save_and_apply_native_terminal_settings();
    }

    fn current_bottom_quote_interval_minutes(&self) -> u32 {
        crate::native_settings::bottom_quote_interval_minutes(&self.native_settings)
    }

    fn bottom_quote_interval_label(&self) -> String {
        Self::format_bottom_quote_interval(self.current_bottom_quote_interval_minutes())
    }

    fn step_bottom_quote_interval(&mut self, delta: i32) {
        let current = self.current_bottom_quote_interval_minutes() as i32;
        let value = (current + delta).clamp(1, 24 * 60) as u32;
        self.native_settings.terminal.bottom_quote_interval_minutes = Some(value);
        self.save_and_apply_bottom_quote_settings(settings_tr(
            "settings-status-value-now",
            &[
                ("setting", crate::i18n::tr("settings-quote-interval")),
                ("value", Self::format_bottom_quote_interval(value)),
            ],
        ));
    }

    fn reset_bottom_quote_interval(&mut self) {
        self.native_settings.terminal.bottom_quote_interval_minutes = None;
        self.save_and_apply_bottom_quote_settings(settings_tr(
            "settings-status-value-reset",
            &[
                ("setting", crate::i18n::tr("settings-quote-interval")),
                (
                    "value",
                    Self::format_bottom_quote_interval(
                        crate::native_settings::DEFAULT_BOTTOM_QUOTE_INTERVAL_MINUTES,
                    ),
                ),
            ],
        ));
    }

    fn current_remote_sftp_idle_minutes(&self) -> u32 {
        self.native_settings
            .workspaces
            .remote_sftp_idle_minutes
            .clamp(1, 120)
    }

    fn step_remote_sftp_idle(&mut self, delta: i32) {
        let value = (self.current_remote_sftp_idle_minutes() as i32 + delta).clamp(1, 120) as u32;
        self.native_settings.workspaces.remote_sftp_idle_minutes = value;
        self.save_remote_sftp_idle(value);
    }

    fn reset_remote_sftp_idle(&mut self) {
        let value = crate::native_settings::DEFAULT_REMOTE_SFTP_IDLE_MINUTES;
        self.native_settings.workspaces.remote_sftp_idle_minutes = value;
        self.save_remote_sftp_idle(value);
    }

    fn save_remote_sftp_idle(&mut self, value: u32) {
        match crate::native_settings::save(&self.native_settings) {
            Ok(()) => {
                crate::termwindow::remote_files::update_remote_connection_idle_timeout(value);
                self.status =
                    settings_tr("settings-status-sftp-idle", &[("count", value.to_string())]);
            }
            Err(err) => {
                self.status = settings_tr(
                    "settings-status-sftp-error",
                    &[("error", format!("{err:#}"))],
                );
            }
        }
    }

    /// What the download row shows: the chosen folder, or where the system
    /// would put it, so the row never reads as "unset" when it is in fact
    /// working.
    fn remote_download_directory_label(&self) -> String {
        let configured = self
            .native_settings
            .workspaces
            .remote_download_directory
            .trim();
        if !configured.is_empty() {
            return home_relative(Path::new(configured));
        }
        match crate::native_settings::effective_remote_download_directory() {
            Some(path) => settings_tr(
                "settings-system-default-path",
                &[("path", home_relative(&path))],
            ),
            None => crate::i18n::tr("settings-no-downloads-folder"),
        }
    }

    /// Pick where to export a backup to, or which backup to import.
    fn choose_backup_folder(&mut self, import: bool) {
        self.ui.open_dropdown = None;
        let Some(window) = self.window.clone() else {
            return;
        };
        let instance_id = self.instance_id;
        let notify = window.clone();
        let title = if import {
            "settings-backup-import-picker-title"
        } else {
            "settings-backup-export-picker-title"
        };
        window.pick_folder_async_with_options(
            FolderPickerOptions {
                title: crate::i18n::tr(title),
                prompt: crate::i18n::tr("common-choose"),
                ..Default::default()
            },
            Box::new(move |path| {
                let Some(path) = path else {
                    return;
                };
                promise::spawn::spawn_into_main_thread(async move {
                    if let Some(settings) = settings_window_for_instance(instance_id) {
                        settings.borrow_mut().finish_backup(import, path);
                        notify.invalidate();
                    }
                })
                .detach();
            }),
        );
    }

    /// Export to, or stage an import from, the folder picked. The copies can
    /// be slow on a network or synced folder, so they run off the UI thread
    /// and report back through the status line.
    fn finish_backup(&mut self, import: bool, path: PathBuf) {
        self.status = crate::i18n::tr(if import {
            "settings-status-backup-checking"
        } else {
            "settings-status-backup-exporting"
        });
        let instance_id = self.instance_id;
        let window = self.window.clone();
        promise::spawn::spawn(async move {
            let done = promise::spawn::spawn_into_new_thread(move || {
                Ok::<_, anyhow::Error>(if import {
                    crate::state_backup::stage_import(&path).map(|()| None)
                } else {
                    crate::state_backup::export(&path).map(Some)
                })
            })
            .await
            .and_then(|done| done);
            let status = match (import, done) {
                (true, Ok(_)) => crate::i18n::tr("settings-status-backup-import-staged"),
                (true, Err(err)) => settings_tr(
                    "settings-status-backup-import-error",
                    &[("error", format!("{err:#}"))],
                ),
                (false, Ok(folder)) => settings_tr(
                    "settings-status-backup-exported",
                    &[(
                        "path",
                        folder
                            .map(|folder| folder.display().to_string())
                            .unwrap_or_default(),
                    )],
                ),
                (false, Err(err)) => settings_tr(
                    "settings-status-backup-export-error",
                    &[("error", format!("{err:#}"))],
                ),
            };
            if let Some(settings) = settings_window_for_instance(instance_id) {
                settings.borrow_mut().status = status;
            }
            if let Some(window) = window {
                window.invalidate();
            }
        })
        .detach();
    }

    fn choose_remote_download_directory(&mut self) {
        self.ui.open_dropdown = None;
        let Some(window) = self.window.clone() else {
            return;
        };
        let instance_id = self.instance_id;
        let notify = window.clone();
        window.pick_folder_async_with_options(
            FolderPickerOptions {
                title: crate::i18n::tr("settings-download-picker-title"),
                prompt: crate::i18n::tr("common-choose"),
                ..Default::default()
            },
            Box::new(move |path| {
                // Cancelling the picker must leave the setting alone, so only
                // a real selection reaches the store.
                let Some(path) = path else {
                    return;
                };
                promise::spawn::spawn_into_main_thread(async move {
                    if let Some(settings) = settings_window_for_instance(instance_id) {
                        settings
                            .borrow_mut()
                            .set_remote_download_directory(Some(path));
                        notify.invalidate();
                    }
                })
                .detach();
            }),
        );
    }

    fn set_remote_download_directory(&mut self, path: Option<PathBuf>) {
        self.ui.open_dropdown = None;
        match crate::native_settings::set_remote_download_directory(path) {
            Ok(()) => {
                self.set_native_settings(crate::native_settings::load());
                self.status = settings_tr(
                    "settings-status-download-path",
                    &[("path", self.remote_download_directory_label())],
                );
            }
            Err(err) => {
                self.status = settings_tr(
                    "settings-status-download-error",
                    &[("error", format!("{err:#}"))],
                );
            }
        }
    }

    fn format_bottom_quote_interval(minutes: u32) -> String {
        if minutes < 60 {
            format!("{minutes} min")
        } else if minutes % 60 == 0 {
            let hours = minutes / 60;
            if hours == 1 {
                "1 h".to_string()
            } else {
                format!("{hours} h")
            }
        } else {
            format!("{} h {} min", minutes / 60, minutes % 60)
        }
    }

    fn current_bottom_quote_font_size(&self) -> f64 {
        crate::native_settings::bottom_quote_font_size(&self.native_settings)
    }

    fn bottom_quote_font_size_label(&self) -> String {
        Self::format_bottom_quote_font_size(self.current_bottom_quote_font_size())
    }

    fn step_bottom_quote_font_size(&mut self, delta: f64) {
        let value = (self.current_bottom_quote_font_size() + delta).clamp(6.0, 20.0);
        self.native_settings.terminal.bottom_quote_font_size = Some(value);
        self.save_and_apply_bottom_quote_settings(settings_tr(
            "settings-status-value-now",
            &[
                ("setting", crate::i18n::tr("settings-quote-font-size")),
                ("value", Self::format_bottom_quote_font_size(value)),
            ],
        ));
    }

    fn reset_bottom_quote_font_size(&mut self) {
        self.native_settings.terminal.bottom_quote_font_size = None;
        self.save_and_apply_bottom_quote_settings(settings_tr(
            "settings-status-value-reset",
            &[
                ("setting", crate::i18n::tr("settings-quote-font-size")),
                (
                    "value",
                    Self::format_bottom_quote_font_size(
                        crate::native_settings::DEFAULT_BOTTOM_QUOTE_FONT_SIZE,
                    ),
                ),
            ],
        ));
    }

    fn format_bottom_quote_font_size(size: f64) -> String {
        if (size.round() - size).abs() < f64::EPSILON {
            format!("{size:.0} pt")
        } else {
            format!("{size:.1} pt")
        }
    }

    fn current_chrome_font_size_value(&self, area: ChromeFontArea) -> f64 {
        let value = match area {
            ChromeFontArea::Settings => self.native_settings.chrome.settings_font_size,
            ChromeFontArea::Home => self.native_settings.chrome.home_font_size,
            ChromeFontArea::RightSidebar => self.native_settings.chrome.right_sidebar_font_size,
            ChromeFontArea::Sidebar => self.native_settings.chrome.sidebar_font_size,
            ChromeFontArea::TabBar => self.native_settings.chrome.tab_font_size,
            ChromeFontArea::PaneHeader => self.native_settings.chrome.pane_header_font_size,
        };
        value.unwrap_or_else(|| match area {
            // An unset Right Sidebar size follows the resolved Home size;
            // show and step from what is actually rendered, not the
            // platform default.
            ChromeFontArea::RightSidebar => {
                crate::native_settings::home_font_size(&self.native_settings)
            }
            _ => area.default_size(),
        })
    }

    fn set_chrome_font_size_value(&mut self, area: ChromeFontArea, value: Option<f64>) {
        match area {
            ChromeFontArea::Settings => self.native_settings.chrome.settings_font_size = value,
            ChromeFontArea::Home => self.native_settings.chrome.home_font_size = value,
            ChromeFontArea::RightSidebar => {
                self.native_settings.chrome.right_sidebar_font_size = value
            }
            ChromeFontArea::Sidebar => self.native_settings.chrome.sidebar_font_size = value,
            ChromeFontArea::TabBar => self.native_settings.chrome.tab_font_size = value,
            ChromeFontArea::PaneHeader => self.native_settings.chrome.pane_header_font_size = value,
        }
    }

    fn step_chrome_font_size(&mut self, area: ChromeFontArea, delta: f64) {
        let value = (self.current_chrome_font_size_value(area) + delta).clamp(10.0, 28.0);
        self.set_chrome_font_size_value(area, Some(value));
        self.save_native_chrome_settings(area);
    }

    fn reset_chrome_font_size(&mut self, area: ChromeFontArea) {
        self.set_chrome_font_size_value(area, None);
        self.save_native_chrome_settings(area);
    }

    fn current_settings_font_weight_value(&self) -> f64 {
        crate::native_settings::settings_font_weight(&self.native_settings) as f64
    }

    fn step_settings_font_weight(&mut self, delta: i16) {
        let current = crate::native_settings::settings_font_weight(&self.native_settings) as i16;
        let value = (current + delta).clamp(300, 800) as u16;
        self.native_settings.chrome.settings_font_weight = Some(value);
        self.save_native_chrome_settings(ChromeFontArea::Settings);
    }

    fn reset_settings_font_weight(&mut self) {
        self.native_settings.chrome.settings_font_weight = None;
        self.save_native_chrome_settings(ChromeFontArea::Settings);
    }

    /// The family the interface is drawn with, if one was chosen.
    fn chosen_ui_font_family(&self) -> Option<&str> {
        self.native_settings
            .chrome
            .ui_font_family
            .as_deref()
            .map(str::trim)
            .filter(|family| !family.is_empty())
    }

    /// What the closed interface font dropdown shows.
    fn current_ui_font_label(&self) -> String {
        self.chosen_ui_font_family()
            .map(str::to_string)
            .unwrap_or_else(|| crate::i18n::tr("settings-ui-font-system"))
    }

    /// The rows the interface font menu holds -- the system's font, then
    /// every family -- how many of them it shows at once, and the distance
    /// from one row to the next.
    fn ui_font_menu_rows(&self) -> (usize, usize, f32) {
        let total = self.ui.ui_font_families.len() + 1;
        (
            total,
            total.min(UI_FONT_MENU_ROWS),
            self.ui_px(DROPDOWN_ROW_HEIGHT + DROPDOWN_ROW_GAP),
        )
    }

    /// Open the interface font menu on the family in use.
    fn open_ui_font_menu(&mut self) {
        if self.ui.ui_font_families.is_empty() {
            self.ui.ui_font_families = self.fonts.list_font_families();
        }
        // A family typed into settings.json, or uninstalled since it was
        // chosen, is still the one in use and has to be there to be seen.
        if let Some(chosen) = self.chosen_ui_font_family().map(str::to_string) {
            if !self.ui.ui_font_families.contains(&chosen) {
                self.ui.ui_font_families.insert(0, chosen);
            }
        }
        let selected = self
            .chosen_ui_font_family()
            .and_then(|chosen| {
                self.ui
                    .ui_font_families
                    .iter()
                    .position(|family| family == chosen)
            })
            .map_or(0, |index| index + 1);
        let (total, visible, step) = self.ui_font_menu_rows();
        let scroll = &mut self.ui.ui_font_menu_scroll;
        scroll.reset();
        scroll.set_extents(visible as f32 * step, total as f32 * step);
        // A few rows down from the top, so what is around it shows too.
        scroll.scroll_by(selected.saturating_sub(visible / 2) as f32 * step);
        self.ui.open_dropdown = Some(SettingsDropdown::UiFont);
    }

    /// Save `family` as the interface's font and draw every window with it.
    /// `None` is the system's interface font.
    fn set_ui_font_family(&mut self, family: Option<String>) {
        let setting = crate::i18n::tr("settings-ui-font");
        let label = family
            .clone()
            .unwrap_or_else(|| crate::i18n::tr("settings-ui-font-system"));
        // From a fresh read and adopted once it lands, as the default shell
        // is: the main window saves state of its own to the same file.
        let mut pending = crate::native_settings::load();
        pending.chrome.ui_font_family = family;
        match crate::native_settings::save(&pending) {
            Ok(()) => {
                crate::native_settings::apply_ui_font_to_app(&pending);
                self.set_native_settings(pending);
                self.status = match self.reload_settings_fonts() {
                    Ok(()) => settings_tr(
                        "settings-status-value-now",
                        &[("setting", setting), ("value", label)],
                    ),
                    Err(err) => settings_tr(
                        "settings-status-save-error",
                        &[("setting", setting), ("error", format!("{err:#}"))],
                    ),
                };
            }
            Err(err) => {
                self.status = settings_tr(
                    "settings-status-save-error",
                    &[("setting", setting), ("error", format!("{err:#}"))],
                );
            }
        }
    }

    fn current_main_renderer(&self) -> NativeRendererBackend {
        crate::native_settings::main_window_renderer(
            &self.native_settings,
            configuration().front_end,
        )
    }

    fn main_renderer_restart_required(&self) -> bool {
        self.current_main_renderer() != self.active_main_renderer
    }

    fn save_native_chrome_settings(&mut self, area: ChromeFontArea) {
        match crate::native_settings::save(&self.native_settings) {
            Ok(()) => {
                if area == ChromeFontArea::Settings {
                    if let Err(err) = self.reload_settings_fonts() {
                        self.status = settings_tr(
                            "settings-status-save-error",
                            &[
                                ("setting", area.localized_label()),
                                ("error", format!("{err:#}")),
                            ],
                        );
                        return;
                    }
                }
                if let Some(front_end) = crate::frontend::try_front_end() {
                    front_end.invalidate_all_windows();
                }
                self.status = settings_tr(
                    "settings-status-saved",
                    &[("setting", area.localized_label())],
                );
            }
            Err(err) => {
                self.status = settings_tr(
                    "settings-status-save-error",
                    &[
                        ("setting", area.localized_label()),
                        ("error", format!("{err:#}")),
                    ],
                );
            }
        }
    }

    fn reload_settings_fonts(&mut self) -> anyhow::Result<()> {
        let settings_font_size = crate::native_settings::settings_font_size(&self.native_settings);
        let settings_font_weight =
            crate::native_settings::settings_font_weight(&self.native_settings);
        self.ui_font = self
            .fonts
            .command_palette_font_with_size_and_weight(settings_font_size, settings_font_weight)?;
        self.title_font = self
            .fonts
            .title_font_with_size_and_weight(settings_font_size + 4.0, settings_font_weight)?;
        self.sidebar_title_font = self.fonts.title_font_with_size_and_weight(
            Self::sidebar_brand_font_size_for_config(&configuration()),
            SIDEBAR_BRAND_FONT_WEIGHT,
        )?;
        self.body_font = self.fonts.command_palette_font_with_size_and_weight(
            settings_font_size,
            settings_body_font_weight(settings_font_weight),
        )?;
        self.import_body_font = self.fonts.command_palette_font_with_size_and_weight(
            (settings_font_size - 1.0).max(10.0),
            (settings_font_weight as f32 * 0.67).round().max(350.0) as u16,
        )?;
        self.import_title_font = self.fonts.title_font_with_size_and_weight(
            settings_font_size + 10.0,
            settings_font_weight,
        )?;
        self.logo_caption_font = self.fonts.command_palette_font_with_size_and_weight(
            LOGO_CAPTION_FONT_SIZE,
            LOGO_CAPTION_FONT_WEIGHT,
        )?;
        self.metrics = RenderMetrics::with_font_metrics(&self.ui_font.metrics());
        self.invalidate_shaped_text();
        Ok(())
    }

    fn save_and_apply_native_terminal_settings(&mut self) {
        match crate::native_settings::save(&self.native_settings) {
            Ok(()) => {
                self.apply_terminal_font_size_to_open_windows();
                self.status = crate::i18n::tr("settings-status-terminal-saved");
            }
            Err(err) => {
                self.status = settings_tr(
                    "settings-status-terminal-error",
                    &[("error", format!("{err:#}"))],
                );
            }
        }
    }

    /// Move the window-opacity knob to `x` and show the result in every
    /// window, without saving it yet.
    fn drag_window_opacity(&mut self, x: f32) {
        let Some((left, width)) = self.ui.window_opacity_track else {
            return;
        };
        let percent = window_opacity_at(x, left, width);
        if self.native_settings.appearance.window_opacity == Some(percent) {
            return;
        }
        self.native_settings.appearance.window_opacity = Some(percent);
        // Each preview repaints every window, so a fast drag shows some of
        // the steps; letting go shows the last.
        let due = self
            .ui
            .window_opacity_previewed
            .is_none_or(|at| at.elapsed() >= WINDOW_OPACITY_PREVIEW_INTERVAL);
        if due {
            self.ui.window_opacity_previewed = Some(Instant::now());
            crate::native_settings::preview_window_opacity(Some(percent));
        }
    }

    fn commit_window_opacity(&mut self) {
        self.ui.window_opacity_previewed = None;
        let opacity = self.native_settings.appearance.window_opacity;
        // Let go where it started: nothing to save, only a preview to end.
        if crate::native_settings::load().appearance.window_opacity == opacity {
            crate::native_settings::preview_window_opacity(None);
            return;
        }
        self.apply_window_opacity(opacity);
    }

    fn apply_window_opacity(&mut self, opacity: Option<u8>) {
        self.ui.open_dropdown = None;
        // Merged into the freshest settings, as `apply_text_contrast` does:
        // this window's copy can be older than what other windows saved.
        let mut merged = crate::native_settings::load();
        merged.appearance.window_opacity = opacity;
        self.set_native_settings(merged);
        let setting = crate::i18n::tr("settings-window-opacity");
        match crate::native_settings::save(&self.native_settings) {
            Ok(()) => {
                crate::native_settings::apply_window_opacity_to_app(&self.native_settings);
                self.status = settings_tr(
                    "settings-status-value-now",
                    &[
                        ("setting", setting),
                        ("value", window_opacity_label(opacity)),
                    ],
                );
            }
            Err(err) => {
                // Not kept: the slider and the windows go back to the
                // opacity that is.
                self.set_native_settings(crate::native_settings::load());
                crate::native_settings::preview_window_opacity(None);
                self.status = settings_tr(
                    "settings-status-save-error",
                    &[("setting", setting), ("error", format!("{err:#}"))],
                );
            }
        }
    }

    fn apply_scroll_mode(&mut self, mode: crate::native_settings::NativeScrollMode) {
        self.ui.open_dropdown = None;
        self.native_settings.terminal.scroll_mode = mode;
        match crate::native_settings::save(&self.native_settings) {
            Ok(()) => {
                self.status = settings_tr(
                    "settings-status-value-now",
                    &[
                        ("setting", crate::i18n::tr("settings-scroll-mode")),
                        ("value", localized_scroll_mode_label(mode)),
                    ],
                );
            }
            Err(err) => {
                self.status = settings_tr(
                    "settings-status-terminal-error",
                    &[("error", format!("{err:#}"))],
                );
            }
        }
    }

    fn apply_text_contrast(&mut self, mode: crate::native_settings::NativeTextContrast) {
        self.ui.open_dropdown = None;
        // Merge into the freshest settings rather than writing this window's
        // whole clone: the colour-scheme row hands the choosing to the command
        // palette, which writes `appearance.color_scheme` while Settings is
        // still open. A full write from a snapshot taken before that would put
        // the old scheme back, and the revert would only show up in the next
        // window or the next launch. Same reasoning as
        // `save_command_palette_settings`.
        let mut merged = crate::native_settings::load();
        merged.terminal.text_contrast = mode;
        self.set_native_settings(merged);
        match crate::native_settings::save(&self.native_settings) {
            Ok(()) => {
                // Every open window resolves the floor for itself and retires
                // the quads that were built under the old one; a plain repaint
                // would redraw them in the colours they were already baked in.
                if let Some(front_end) = crate::frontend::try_front_end() {
                    for gui_window in front_end.gui_windows() {
                        gui_window.window.notify(
                            crate::termwindow::TermWindowNotif::Apply(Box::new(|term_window| {
                                term_window.refresh_text_min_contrast();
                            })),
                        );
                    }
                }
                self.status = settings_tr(
                    "settings-status-value-now",
                    &[
                        ("setting", crate::i18n::tr("settings-text-contrast")),
                        ("value", localized_text_contrast_label(mode)),
                    ],
                );
            }
            Err(err) => {
                self.status = settings_tr(
                    "settings-status-terminal-error",
                    &[("error", format!("{err:#}"))],
                );
            }
        }
    }

    fn apply_remote_pane_resize_mode(&mut self, mode: NativeRemotePaneResizeMode) {
        self.ui.open_dropdown = None;
        self.native_settings.terminal.remote_pane_resize_mode = mode;
        match crate::native_settings::save(&self.native_settings) {
            Ok(()) => {
                if let Some(front_end) = crate::frontend::try_front_end() {
                    front_end.invalidate_all_windows();
                }
                self.status = settings_tr(
                    "settings-status-value-now",
                    &[
                        (
                            "setting",
                            crate::i18n::tr("settings-remote-pane-resize-mode"),
                        ),
                        ("value", localized_remote_pane_resize_mode_label(mode)),
                    ],
                );
            }
            Err(err) => {
                self.status = settings_tr(
                    "settings-status-terminal-error",
                    &[("error", format!("{err:#}"))],
                );
            }
        }
    }

    fn apply_bottom_quote_mode(&mut self, mode: NativeBottomQuoteMode) {
        self.ui.open_dropdown = None;
        self.native_settings.terminal.bottom_quote_mode = mode;
        self.save_and_apply_bottom_quote_settings(settings_tr(
            "settings-status-value-now",
            &[
                ("setting", crate::i18n::tr("settings-quote-rotation")),
                ("value", localized_quote_mode_label(mode)),
            ],
        ));
    }

    fn save_and_apply_bottom_quote_settings(&mut self, status: String) {
        match crate::native_settings::save(&self.native_settings) {
            Ok(()) => {
                if let Some(front_end) = crate::frontend::try_front_end() {
                    front_end.invalidate_all_windows();
                }
                self.ui.quote_preview = quote_preview_text(&self.native_settings);
                self.status = status;
            }
            Err(err) => {
                self.status = settings_tr(
                    "settings-status-quote-error",
                    &[("error", format!("{err:#}"))],
                );
            }
        }
    }

    fn apply_terminal_font_size_to_open_windows(&self) {
        let font_size = self
            .native_settings
            .terminal
            .font_size
            .unwrap_or_else(|| configuration().font_size);
        let Some(front_end) = crate::frontend::try_front_end() else {
            return;
        };
        for gui_window in front_end.gui_windows() {
            gui_window
                .window
                .notify(crate::termwindow::TermWindowNotif::Apply(Box::new(
                    move |term_window| {
                        if !font_size.is_finite()
                            || font_size <= 0.0
                            || term_window.config.font_size <= 0.0
                        {
                            return;
                        }
                        let font_scale =
                            (font_size / term_window.config.font_size).clamp(0.25, 4.0);
                        if let Some(window) = term_window.window.as_ref().cloned() {
                            term_window.adjust_font_scale(font_scale, &window);
                        }
                    },
                )));
        }
    }

    /// The theme this window paints in.
    ///
    /// The system half is read live, which is what the main window's chrome
    /// does (`native_settings::effective_appearance`). It used to come from a
    /// snapshot taken when this window was built and refreshed only by an
    /// `AppearanceChanged` event -- so any moment the snapshot was wrong (the
    /// window built before AppKit settled on the system appearance, or an
    /// event that never arrived) left Settings painting a different theme
    /// from the terminal behind it. The theme *mode* still comes from this
    /// window's own copy so an edit previews before it is saved.
    fn effective_appearance(&self) -> Appearance {
        self.native_settings
            .appearance
            .theme_mode
            .effective_appearance(crate::native_settings::system_appearance())
    }

    /// Re-resolve the cached chrome colours. Call after anything that moves
    /// `effective_appearance`: the system appearance, or this window's own
    /// `theme_mode`.
    /// The window to hand the colour-scheme palette to: the one Settings was
    /// opened from if it is still there, otherwise any terminal window, so
    /// the row still works when that window has since been closed.
    fn color_scheme_picker_target(&self) -> Option<crate::scripting::guiwin::GuiWin> {
        let front_end = crate::frontend::try_front_end()?;
        let windows = front_end.gui_windows();
        let wanted = OPENED_FROM.with(|slot| slot.get());
        windows
            .iter()
            .find(|gui_window| Some(gui_window.mux_window_id) == wanted)
            .cloned()
            .or_else(|| windows.into_iter().next())
    }

    fn refresh_chrome(&mut self) {
        // The standalone window has no per-window overrides of its own, so it
        // reads the configuration file's `ui_colors` rather than a terminal
        // window's, which may carry a scheme picked only for that window.
        self.chrome_palette = crate::native_settings::chrome_palette(
            self.native_settings.appearance.theme_mode,
            self.effective_appearance(),
            &config::configuration(),
            None,
        );
    }

    /// Replace this window's copy of the settings, keeping the cached chrome
    /// in step. Assign through here rather than to the field directly: the
    /// merge-then-write helpers reload the shared settings, so a theme another
    /// window picked arrives this way and the cache would otherwise be stale
    /// until the next appearance event.
    fn set_native_settings(&mut self, settings: ThinkTermNativeSettings) {
        self.native_settings = settings;
        self.refresh_chrome();
    }

    /// See `follow_open_settings_window`. A field the user is typing into
    /// is left alone.
    fn follow_settings_changed_elsewhere(
        &mut self,
        before: &ThinkTermNativeSettings,
        after: &ThinkTermNativeSettings,
    ) {
        self.set_native_settings(crate::native_settings::load());
        let terminal = (&before.terminal, &after.terminal);
        if terminal.0.font_size != terminal.1.font_size {
            // The size steps from this field, so a stale one would put the
            // old size back on the next click.
            let size = terminal.1.font_size.unwrap_or_else(|| configuration().font_size);
            self.ui.font_size_input.set_text_end(format!("{size:.1}"));
        }
        if terminal.0.font_family != terminal.1.font_family && !self.ui.font_family_input_dirty {
            self.ui.font_family_input.set_text_end(
                terminal
                    .1
                    .font_family
                    .clone()
                    .unwrap_or_else(|| Self::effective_font_family(&configuration())),
            );
        }
        if before.workspaces.remote_drop_destination != after.workspaces.remote_drop_destination
            && !self.ui.remote_drop_input_dirty
        {
            self.ui
                .remote_drop_input
                .set_text_end(crate::native_settings::remote_drop_destination());
        }
        if terminal.0.default_shell != terminal.1.default_shell {
            self.refresh_shell_catalog();
        }
        if terminal.0.bottom_quote_enabled != terminal.1.bottom_quote_enabled
            || terminal.0.bottom_quote_mode != terminal.1.bottom_quote_mode
            || terminal.0.bottom_quote_interval_minutes != terminal.1.bottom_quote_interval_minutes
            || terminal.0.bottom_quote_font_size != terminal.1.bottom_quote_font_size
        {
            self.ui.quote_preview = quote_preview_text(&self.native_settings);
        }
        if before.tab_icons != after.tab_icons {
            self.sync_tab_icon_inputs();
        }
        if before.chrome.settings_font_size != after.chrome.settings_font_size
            || before.chrome.settings_font_weight != after.chrome.settings_font_weight
            || before.chrome.ui_font_family != after.chrome.ui_font_family
        {
            if let Err(err) = self.reload_settings_fonts() {
                log::warn!("settings window fonts after an outside change: {err:#}");
            }
        }
        if before.localization.language != after.localization.language
            || before.onboarding.language != after.onboarding.language
        {
            // As choosing a language here does; `apply_to_app` has switched
            // it already.
            self.ui.sidebar_scroll.reset();
            self.sync_selected_section_with_search();
            if let Some(window) = self.window.as_ref() {
                window.set_title(&crate::i18n::tr("settings-window-title"));
            }
        }
        if let Some(window) = self.window.as_ref() {
            window.invalidate();
        }
    }

    /// Every field reads a `UiPalette` token, so Settings tracks the main
    /// window by construction. It stopped doing that once: two arms that
    /// differed only in a hand-written `card_bg`, which was the token's value
    /// copied out by hand and would have gone stale the first time the token
    /// moved.
    fn palette(&self) -> SettingsPalette {
        let ui = self.chrome_palette;
        SettingsPalette {
            window_bg: ui.window_bg,
            sidebar_bg: ui.workspace_sidebar_bg,
            separator: ui.separator,
            search_bg: ui.control_bg,
            search_border: ui.control_border,
            nav_hover_bg: ui.sidebar_row_hover_bg,
            nav_pressed_bg: ui.sidebar_row_pressed_bg,
            nav_selected_bg: ui.sidebar_row_active_bg,
            track_off: ui.track_off,
            is_dark: ui.is_dark(),
            nav_selected_border: ui.sidebar_row_active_border,
            control_bg: ui.control_bg,
            control_hover_bg: ui.control_hover_bg,
            control_pressed_bg: ui.control_pressed_bg,
            control_border: ui.control_border,
            card_bg: ui.card_bg,
            on_accent: ui.on_accent,
            title: ui.text,
            text: ui.text,
            secondary_text: ui.secondary_text,
            muted_text: ui.muted_text,
            selected_text: ui.text,
            rule: ui.separator,
        }
    }

    fn settings_row_step(&self) -> f32 {
        let cell_height = self.metrics.cell_size.height as f32;
        (cell_height * 2.15 + self.ui_px(42.0))
            .max(self.ui_px(116.0))
            .ceil()
    }

    fn settings_card_top_padding(&self) -> f32 {
        let cell_height = self.metrics.cell_size.height as f32;
        (cell_height * 0.62)
            .clamp(self.ui_px(22.0), self.ui_px(30.0))
            .ceil()
    }

    fn settings_card_bottom_padding(&self) -> f32 {
        let cell_height = self.metrics.cell_size.height as f32;
        (cell_height * 0.72)
            .clamp(self.ui_px(26.0), self.ui_px(36.0))
            .ceil()
    }

    fn settings_row_visual_height(&self) -> f32 {
        let cell_height = self.metrics.cell_size.height as f32;
        (cell_height + self.ui_px(46.0))
            .max(self.ui_px(CONTROL_HEIGHT) + self.ui_px(10.0))
            .ceil()
    }

    fn settings_row_description_y(&self, y: f32) -> f32 {
        let cell_height = self.metrics.cell_size.height as f32;
        y + (cell_height + self.ui_px(8.0)).max(self.ui_px(34.0))
    }

    fn settings_section_card_gap(&self) -> f32 {
        let cell_height = self.metrics.cell_size.height as f32;
        (cell_height * 1.45).clamp(52.0, 72.0).ceil()
    }

    fn settings_card_height(&self, row_count: usize) -> f32 {
        if row_count == 0 {
            return 0.0;
        }
        self.settings_card_top_padding()
            + self.settings_row_step() * row_count.saturating_sub(1) as f32
            + self.settings_row_visual_height()
            + self.settings_card_bottom_padding()
    }

    fn settings_card_geometry(&self, section_y: f32, _row_count: usize) -> (f32, f32) {
        let card_y = section_y + self.settings_section_card_gap();
        let first_row_y = card_y + self.settings_card_top_padding();
        (card_y, first_row_y)
    }

    fn sidebar_icon_size(&self) -> f32 {
        (self.metrics.cell_size.height as f32 + self.ui_px(8.0))
            .clamp(self.ui_px(24.0), self.ui_px(34.0))
            .round()
    }

    fn nav_row_height(&self) -> f32 {
        ((self.metrics.cell_size.height as f32).max(self.sidebar_icon_size())
            + self.ui_px(NAV_ROW_INSET))
        .max(self.ui_px(NAV_ROW_MIN_HEIGHT))
    }

    fn sidebar_list_top(&self) -> f32 {
        self.ui_px(SIDEBAR_LIST_TOP)
    }

    fn nav_row_step(&self) -> f32 {
        self.nav_row_height() + self.ui_px(NAV_ROW_GAP)
    }

    /// Width of the control column shared by every settings row kind, so a
    /// value pill, a toggle and a link button all line up on the same right
    /// edge no matter which rows a card mixes.
    fn settings_control_width(&self, width: f32) -> f32 {
        // Design pixels, like every other layout constant here: the column
        // was 280 physical pixels regardless of DPI, which made it twice as
        // wide as designed on a 96-dpi screen.
        if width >= self.ui_px(680.0) {
            self.ui_px(280.0).min(width * 0.36)
        } else {
            self.ui_px(220.0).min(width * 0.44)
        }
    }

    fn settings_content_extent(&self, bottom_y: f32) -> f32 {
        (bottom_y + self.ui_px(65.0)).max(self.content_bottom())
    }

    fn developer_mode_enabled(&self) -> bool {
        self.native_settings.developer.developer_mode
    }

    /// Everything a section needs done when it becomes the visible page.
    /// Shared by the sidebar click and by an external request to open a
    /// particular page, so a page reached from the app menu arrives in the
    /// same state as one clicked into.
    /// Where the listener binds: every address when the page is to be
    /// reachable from other devices, else the configured or default one.
    fn web_bind_address(&self) -> String {
        crate::web_settings::bind_address(
            crate::web_settings::state().status.as_ref(),
            self.native_settings.web.reachable,
        )
    }

    fn enter_section(&mut self, section: SettingsSection) {
        self.selected = section;
        self.ui.content_scroll.reset();
        self.ui.open_dropdown = None;
        // A code on screen carries a live token; it does not outlast the
        // section it was asked for in.
        crate::web_settings::hide_qr();
        if section == SettingsSection::Agents {
            // Probe PATH on entry so painting never touches the filesystem.
            crate::agent_status::refresh_path_probe();
        }
        if section == SettingsSection::Terminal {
            // Same rule: shell discovery stats candidate paths, and finding
            // the quote reads a file, so both happen on entry and the rows
            // paint from what they found.
            self.refresh_shell_catalog();
            self.ui.quote_preview = quote_preview_text(&self.native_settings);
        }
        if section == SettingsSection::Sidebar {
            // Plugins installed or removed since are found on entry.
            crate::plugins::refresh();
        }
        if matches!(section, SettingsSection::Update | SettingsSection::About) {
            // Re-read the cache the background checker writes, so re-entering
            // the page picks up a check that ran while the window sat on
            // another section.
            self.ui.update_status = Some(crate::update::cached_update_status());
            self.ui.update_method = Some(thinkterm_update::InstallMethod::detect());
            // A failed check from an earlier visit says nothing about the
            // cache just read, which a later check may have refreshed.
            if matches!(update_check(), Some(UpdateCheck::Failed { .. })) {
                set_update_check(None);
            }
        }
    }

    fn visible_sections(&self) -> Vec<SettingsSection> {
        let mut sections = Vec::with_capacity(BASE_SECTIONS.len() + DEVELOPER_SECTIONS.len());
        for section in BASE_SECTIONS {
            sections.push(*section);
            // The developer pages belong to Developer, not to whatever
            // happens to follow it in BASE_SECTIONS; anchoring on Developer
            // keeps them adjacent to it as sections are added below.
            if *section == SettingsSection::Developer && self.developer_mode_enabled() {
                sections.extend_from_slice(DEVELOPER_SECTIONS);
            }
        }
        sections
    }

    fn section_is_visible(&self, section: SettingsSection) -> bool {
        self.visible_sections().contains(&section)
    }

    fn clamp_sidebar_to_window(&mut self) {
        let dynamic_max = (self.dimensions.pixel_width as f32 * 0.38)
            .max(self.ui.sidebar.min_width)
            .min(self.ui.sidebar.max_width);
        if self.ui.sidebar.width > dynamic_max {
            self.ui.sidebar.width = dynamic_max;
        }
    }

    fn perform_action(&mut self, action: SettingsAction, window: &Window) {
        match action {
            SettingsAction::SelectImportSource(source) => {
                if self.import_busy() { return; }
                #[cfg(unix)]
                if self.ui.import_source != source {
                    self.ui.session_import.abandon();
                    self.ui.session_import = session_import::ImportUi::default();
                }
                self.ui.import_source = source;
                self.ui.content_scroll.reset();
                self.ui.open_dropdown = None;
            }
            SettingsAction::ImportContinue
            | SettingsAction::ImportBack
            | SettingsAction::ImportStartOver => self.perform_import_navigation(action, window),
            #[cfg(unix)]
            SettingsAction::SessionImportDetect
            | SettingsAction::SessionImportPreview(_)
            | SettingsAction::SessionImportHost(_)
            | SettingsAction::SessionImportBack
            | SettingsAction::SessionImportRecover
            | SettingsAction::SessionImportConfirm
            | SettingsAction::SessionImportOpen => self.perform_session_import_action(action),
            SettingsAction::WindowHide => {
                self.ui.open_dropdown = None;
                window.hide();
            }
            SettingsAction::WindowMaximize => {
                self.ui.open_dropdown = None;
                if self
                    .window_state
                    .intersects(WindowState::MAXIMIZED | WindowState::FULL_SCREEN)
                {
                    window.restore();
                } else {
                    window.maximize();
                }
            }
            SettingsAction::WindowClose => {
                // Run cleanup before closing: on X11 the window is removed
                // from the event map inside close(), so the Destroyed event
                // never reaches us and the singleton slot would stay Open,
                // pointing at a dead window.
                self.cleanup();
                window.close();
            }
            SettingsAction::Select(section) => {
                self.commit_focused_input();
                self.enter_section(section);
            }
            SettingsAction::OpenThinkTermConfigFile => {
                self.ui.open_dropdown = None;
                let path = Self::thinkterm_compatible_config_path();
                if path.exists() {
                    self.status = settings_tr(
                        "settings-status-opening-thinkterm-config",
                        &[("path", path.display().to_string())],
                    );
                    Self::open_path(path);
                } else {
                    self.status = settings_tr(
                        "settings-status-no-thinkterm-config",
                        &[("path", path.display().to_string())],
                    );
                }
            }
            SettingsAction::OpenWezTermConfigFile => {
                self.ui.open_dropdown = None;
                if let Some(path) = self.selected_wezterm_source_path() {
                    self.status = settings_tr(
                        "settings-status-opening-wezterm-config",
                        &[("path", path.display().to_string())],
                    );
                    Self::open_path(path);
                } else {
                    self.status = crate::i18n::tr("settings-status-no-wezterm-config");
                }
            }
            SettingsAction::LoadWezTermSource => {
                self.ui.open_dropdown = None;
                self.compatibility_import = CompatibilityImportState::default();
                match self.load_compatibility_source() {
                    Ok(()) => window.invalidate(),
                    Err(err) => {
                        self.compatibility_import.error = Some(err.to_string());
                        self.status = settings_tr(
                            "settings-status-load-wezterm-error",
                            &[("error", format!("{err:#}"))],
                        );
                    }
                }
            }
            SettingsAction::ImportSelectedFields => {
                self.ui.open_dropdown = None;
                match self.import_selected_compatibility_fields() {
                    Ok((count, entry_created)) => {
                        self.status = settings_tr(
                            if entry_created {
                                "settings-status-import-complete-created"
                            } else {
                                "settings-status-import-complete"
                            },
                            &[("count", count.to_string())],
                        );
                        self.ui.import_result = Some(self.status.clone());
                        self.ui.import_step = ImportStep::Result;
                        self.ui.content_scroll.reset();
                    }
                    Err(err) => {
                        self.status = settings_tr(
                            "settings-status-import-error",
                            &[("error", format!("{err:#}"))],
                        );
                        self.compatibility_import.error = Some(self.status.clone());
                    }
                }
            }
            SettingsAction::SelectAllImportFields => {
                self.ui.open_dropdown = None;
                self.select_all_import_fields();
                window.invalidate();
            }
            SettingsAction::ClearImportFields => {
                self.ui.open_dropdown = None;
                self.clear_import_fields();
                window.invalidate();
            }
            SettingsAction::ToggleImportField(field_id) => {
                self.ui.open_dropdown = None;
                self.toggle_import_field(field_id);
            }
            SettingsAction::ToggleMainWindowFrameRestore => {
                self.ui.open_dropdown = None;
                self.native_settings.window.restore_main_window_frame =
                    !self.native_settings.window.restore_main_window_frame;
                match crate::native_settings::save(&self.native_settings) {
                    Ok(()) => {
                        self.status = if self.native_settings.window.restore_main_window_frame {
                            crate::i18n::tr("settings-status-window-restore-on")
                        } else {
                            crate::i18n::tr("settings-status-window-restore-off")
                        };
                    }
                    Err(err) => {
                        self.status = settings_tr(
                            "settings-status-window-restore-error",
                            &[("error", format!("{err:#}"))],
                        );
                    }
                }
            }
            SettingsAction::ToggleNotificationSounds => {
                self.ui.open_dropdown = None;
                self.native_settings.workspaces.notification_sounds_enabled =
                    !self.native_settings.workspaces.notification_sounds_enabled;
                match crate::native_settings::save(&self.native_settings) {
                    Ok(()) => {
                        self.status = if self.native_settings.workspaces.notification_sounds_enabled
                        {
                            crate::i18n::tr("settings-status-notification-sounds-on")
                        } else {
                            crate::i18n::tr("settings-status-notification-sounds-off")
                        };
                    }
                    Err(err) => {
                        self.status = settings_tr(
                            "settings-status-notification-sounds-error",
                            &[("error", format!("{err:#}"))],
                        );
                    }
                }
            }
            SettingsAction::Hint(_) => {}
            SettingsAction::ToggleRemoteUpdateKeepsSessions => {
                self.ui.open_dropdown = None;
                let enabled = !self.native_settings.workspaces.remote_update_keeps_sessions;
                self.native_settings.workspaces.remote_update_keeps_sessions = enabled;
                wezterm_client::remote_update::set_keep_sessions_on_update(enabled);
                match crate::native_settings::save(&self.native_settings) {
                    Ok(()) => {
                        self.status = crate::i18n::tr(if enabled {
                            "settings-status-remote-update-keep-sessions-on"
                        } else {
                            "settings-status-remote-update-keep-sessions-off"
                        });
                    }
                    Err(err) => {
                        self.status = settings_tr(
                            "settings-status-remote-update-keep-sessions-error",
                            &[("error", format!("{err:#}"))],
                        );
                    }
                }
            }
            SettingsAction::ToggleLocalSessionsViaMux => {
                self.ui.open_dropdown = None;
                let enabled = !self.native_settings.workspaces.local_sessions_via_mux;
                self.native_settings.workspaces.local_sessions_via_mux = enabled;
                match crate::native_settings::save(&self.native_settings) {
                    Ok(()) => {
                        self.status = crate::i18n::tr(if enabled {
                            "settings-status-local-sessions-via-mux-on"
                        } else {
                            "settings-status-local-sessions-via-mux-off"
                        });
                    }
                    Err(err) => {
                        self.status = settings_tr(
                            "settings-status-local-sessions-via-mux-error",
                            &[("error", format!("{err:#}"))],
                        );
                    }
                }
            }
            SettingsAction::ToggleMainRendererMenu => {
                self.ui.open_dropdown =
                    if self.ui.open_dropdown == Some(SettingsDropdown::MainRenderer) {
                        None
                    } else {
                        Some(SettingsDropdown::MainRenderer)
                    };
            }
            SettingsAction::SetMainRenderer(renderer) => {
                self.native_settings.window.main_renderer = Some(renderer);
                self.ui.open_dropdown = None;
                match crate::native_settings::save(&self.native_settings) {
                    Ok(()) => {
                        self.status = settings_tr(
                            "settings-status-renderer",
                            &[("renderer", renderer.label().to_string())],
                        );
                    }
                    Err(err) => {
                        self.status = settings_tr(
                            "settings-status-renderer-error",
                            &[("error", format!("{err:#}"))],
                        );
                    }
                }
            }
            SettingsAction::DropdownMenuBackdrop => {
                // Deliberately inert: the click was inside the open menu
                // but not on an option, so it selects nothing and the menu
                // stays open.
            }
            SettingsAction::ToggleDefaultShellMenu => {
                self.ui.open_dropdown =
                    if self.ui.open_dropdown == Some(SettingsDropdown::DefaultShell) {
                        None
                    } else {
                        Some(SettingsDropdown::DefaultShell)
                    };
            }
            SettingsAction::SetDefaultShell(index) => {
                self.ui.open_dropdown = None;
                // `get` rather than indexing: the catalog is a paint-time
                // snapshot, so a stale index must be a no-op, not a panic.
                let chosen = match index {
                    Some(index) => match self.ui.shell_catalog.get(index) {
                        Some(shell) => Some(shell.clone()),
                        None => return,
                    },
                    None => None,
                };
                let label = chosen
                    .as_ref()
                    .map(|shell| shell.label.clone())
                    .unwrap_or_else(|| self.no_override_label());
                // Written from a copy and only adopted once it lands: a
                // failed save would otherwise leave the dropdown showing a
                // shell that no pane will ever run, and the next unrelated
                // successful save would quietly commit it.
                // Modified from a fresh read rather than from the copy
                // this window opened with: the main window saves sidebar
                // widths and the open-with list independently, and writing
                // a stale whole-tree snapshot would silently revert them.
                let mut pending = crate::native_settings::load();
                pending.terminal.default_shell = chosen.map(|shell| shell.argv);
                // Nothing to repaint or reload: `save` refreshes the shared
                // settings handle, and the spawn path reads it per pane, so
                // the next terminal opened uses the new shell.
                match crate::native_settings::save(&pending) {
                    Ok(()) => {
                        self.set_native_settings(pending);
                        self.status = settings_tr(
                            "settings-status-value-now",
                            &[
                                ("setting", crate::i18n::tr("settings-default-shell")),
                                ("value", label),
                            ],
                        );
                    }
                    Err(err) => {
                        self.status = settings_tr(
                            "settings-status-terminal-error",
                            &[("error", format!("{err:#}"))],
                        );
                    }
                }
            }
            SettingsAction::RestartApplication => {
                self.ui.open_dropdown = None;
                // Not while an install from the Update page runs: quitting
                // would cut the installer off half way through its work.
                if matches!(update_install(), Some(UpdateInstall::Running { .. })) {
                    return;
                }
                match Self::restart_application() {
                    Ok(()) => {
                        self.status = crate::i18n::tr("settings-status-restarting");
                    }
                    Err(err) => {
                        self.status = settings_tr(
                            "settings-status-restart-error",
                            &[("error", format!("{err:#}"))],
                        );
                    }
                }
            }
            SettingsAction::QuitApplication => {
                self.ui.open_dropdown = None;
                self.status = crate::i18n::tr("settings-status-quitting");
                if let Some(conn) = Connection::get() {
                    conn.terminate_message_loop();
                }
            }
            SettingsAction::StopSessionServer => {
                self.ui.open_dropdown = None;
                if !self.ui.confirm_stop_server {
                    self.ui.confirm_stop_server = true;
                    self.status = crate::i18n::tr("settings-status-stop-session-server-confirm");
                } else {
                    self.ui.confirm_stop_server = false;
                    self.status = match crate::local_sessions::stop_server_now(&config::configuration()) {
                        Ok(mux::session_server::StopOutcome::Stopped { .. }) => {
                            crate::i18n::tr("settings-status-stop-session-server-done")
                        }
                        Ok(mux::session_server::StopOutcome::NotRunning) => {
                            crate::i18n::tr("settings-status-stop-session-server-none")
                        }
                        Ok(mux::session_server::StopOutcome::Lingering { .. }) => {
                            crate::i18n::tr("settings-status-stop-session-server-lingering")
                        }
                        Err(err) => settings_tr(
                            "settings-status-stop-session-server-error",
                            &[("error", format!("{err:#}"))],
                        ),
                    };
                }
            }
            SettingsAction::ToggleBottomQuote => {
                self.ui.open_dropdown = None;
                self.native_settings.terminal.bottom_quote_enabled =
                    !self.native_settings.terminal.bottom_quote_enabled;
                let status = if self.native_settings.terminal.bottom_quote_enabled {
                    crate::i18n::tr("settings-status-quote-enabled")
                } else {
                    crate::i18n::tr("settings-status-quote-disabled")
                };
                self.save_and_apply_bottom_quote_settings(status);
            }
            SettingsAction::ToggleOverlayScrollbar => {
                self.ui.open_dropdown = None;
                self.native_settings.terminal.overlay_scrollbar =
                    !self.native_settings.terminal.overlay_scrollbar;
                match crate::native_settings::save(&self.native_settings) {
                    Ok(()) => {
                        if let Some(front_end) = crate::frontend::try_front_end() {
                            front_end.invalidate_all_windows();
                        }
                        self.status = settings_tr(
                            "settings-status-value-now",
                            &[
                                ("setting", crate::i18n::tr("settings-overlay-scrollbar")),
                                (
                                    "value",
                                    crate::i18n::tr(
                                        if self.native_settings.terminal.overlay_scrollbar {
                                            "common-on"
                                        } else {
                                            "common-off"
                                        },
                                    ),
                                ),
                            ],
                        );
                    }
                    Err(err) => {
                        self.status = settings_tr(
                            "settings-status-terminal-error",
                            &[("error", format!("{err:#}"))],
                        );
                    }
                }
            }
            SettingsAction::SetScrollMode(mode) => self.apply_scroll_mode(mode),
            SettingsAction::SetWindowOpacity(opacity) => self.apply_window_opacity(opacity),
            // Handled as a drag, on press and release.
            SettingsAction::WindowOpacitySlider => {}
            SettingsAction::SetTextContrast(mode) => self.apply_text_contrast(mode),
            SettingsAction::SetRemotePaneResizeMode(mode) => {
                self.apply_remote_pane_resize_mode(mode)
            }
            SettingsAction::SetBottomQuoteMode(mode) => self.apply_bottom_quote_mode(mode),
            SettingsAction::DecreaseBottomQuoteFontSize => self.step_bottom_quote_font_size(-1.0),
            SettingsAction::IncreaseBottomQuoteFontSize => self.step_bottom_quote_font_size(1.0),
            SettingsAction::ResetBottomQuoteFontSize => self.reset_bottom_quote_font_size(),
            SettingsAction::DecreaseBottomQuoteInterval => self.step_bottom_quote_interval(-5),
            SettingsAction::IncreaseBottomQuoteInterval => self.step_bottom_quote_interval(5),
            SettingsAction::ResetBottomQuoteInterval => self.reset_bottom_quote_interval(),
            SettingsAction::DecreaseRemoteSftpIdle => self.step_remote_sftp_idle(-5),
            SettingsAction::IncreaseRemoteSftpIdle => self.step_remote_sftp_idle(5),
            SettingsAction::ResetRemoteSftpIdle => self.reset_remote_sftp_idle(),
            SettingsAction::ChooseRemoteDownloadDirectory => {
                self.choose_remote_download_directory()
            }
            SettingsAction::ResetRemoteDownloadDirectory => {
                self.set_remote_download_directory(None)
            }
            SettingsAction::OpenBottomQuotesJson => {
                self.ui.open_dropdown = None;
                match crate::bottom_quotes::ensure_quotes_file() {
                    Ok(path) => {
                        self.status = settings_tr(
                            "settings-status-opening-quotes",
                            &[("path", path.display().to_string())],
                        );
                        Self::open_path(path);
                    }
                    Err(err) => {
                        self.status = settings_tr(
                            "settings-status-open-quotes-error",
                            &[("error", format!("{err:#}"))],
                        );
                    }
                }
            }
            SettingsAction::ResetBottomQuotesJson if !self.ui.confirm_reset_quotes => {
                // It replaces a file the user may have written, so the first
                // press only arms it.
                self.ui.open_dropdown = None;
                self.ui.confirm_reset_quotes = true;
                self.status = crate::i18n::tr("settings-status-reset-quotes-confirm");
            }
            SettingsAction::ResetBottomQuotesJson => {
                self.ui.open_dropdown = None;
                self.ui.confirm_reset_quotes = false;
                match crate::bottom_quotes::reset_quotes_file() {
                    Ok(path) => {
                        if let Some(front_end) = crate::frontend::try_front_end() {
                            front_end.invalidate_all_windows();
                        }
                        self.ui.quote_preview = quote_preview_text(&self.native_settings);
                        self.status = settings_tr(
                            "settings-status-reset-quotes",
                            &[("path", path.display().to_string())],
                        );
                    }
                    Err(err) => {
                        self.status = settings_tr(
                            "settings-status-reset-quotes-error",
                            &[("error", format!("{err:#}"))],
                        );
                    }
                }
            }
            SettingsAction::ToggleWebServer => {
                self.ui.open_dropdown = None;
                let state = crate::web_settings::state();
                let on = state
                    .status
                    .as_ref()
                    .is_some_and(|status| !status.listening.is_empty());
                crate::web_settings::set_enabled(window.clone(), !on, Some(self.web_bind_address()));
            }
            SettingsAction::ToggleWebLinkTtlMenu => {
                self.ui.open_dropdown = if self.ui.open_dropdown == Some(SettingsDropdown::WebLinkTtl)
                {
                    None
                } else {
                    Some(SettingsDropdown::WebLinkTtl)
                };
            }
            SettingsAction::SetWebLinkTtl(ttl) => {
                self.native_settings.web.link_ttl_secs = ttl;
                self.ui.open_dropdown = None;
                self.save_web_settings();
            }
            SettingsAction::CopyWebLink => {
                self.ui.open_dropdown = None;
                crate::web_settings::mint(window.clone(), self.native_settings.web.link_ttl_secs);
                self.schedule_copied_state_clear(window);
            }
            SettingsAction::ToggleWebReachable => {
                self.ui.open_dropdown = None;
                self.native_settings.web.reachable = !self.native_settings.web.reachable;
                self.save_web_settings();
                let state = crate::web_settings::state();
                let on = state.status.as_ref().is_some_and(|status| !status.listening.is_empty());
                if on {
                    crate::web_settings::restart(window.clone(), self.web_bind_address());
                }
            }
            SettingsAction::ToggleWebQr => {
                self.ui.open_dropdown = None;
                if crate::web_settings::state().qr.is_some() {
                    crate::web_settings::hide_qr();
                } else {
                    crate::web_settings::show_qr(window.clone(), self.native_settings.web.link_ttl_secs);
                }
            }
            SettingsAction::RevokeWebToken(key) => {
                self.ui.open_dropdown = None;
                if let Some(token) = self
                    .ui
                    .web_tokens
                    .iter()
                    .find(|token| web_token_key(&token.id) == key)
                {
                    crate::web_settings::revoke(window.clone(), Some(token.id.clone()));
                }
            }
            SettingsAction::RevokeAllWebTokens => {
                self.ui.open_dropdown = None;
                crate::web_settings::revoke(window.clone(), None);
            }
            SettingsAction::ToggleDeveloperMode => {
                self.ui.open_dropdown = None;
                self.native_settings.developer.developer_mode =
                    !self.native_settings.developer.developer_mode;
                if !self.developer_mode_enabled() && !self.section_is_visible(self.selected) {
                    self.selected = SettingsSection::Developer;
                    self.ui.content_scroll.reset();
                }
                match crate::native_settings::save(&self.native_settings) {
                    Ok(()) => {
                        self.status = if self.developer_mode_enabled() {
                            "Developer mode enabled. Extra diagnostics tabs are now visible."
                                .to_string()
                        } else {
                            "Developer mode disabled. Diagnostics tabs are hidden.".to_string()
                        };
                    }
                    Err(err) => {
                        self.status = format!("Unable to save developer mode: {err:#}");
                    }
                }
            }
            SettingsAction::ToggleFallbackContextMenu => {
                self.ui.open_dropdown = None;
                self.native_settings.developer.force_fallback_context_menu =
                    !self.native_settings.developer.force_fallback_context_menu;
                match crate::native_settings::save(&self.native_settings) {
                    Ok(()) => {
                        self.status = if self.native_settings.developer.force_fallback_context_menu
                        {
                            "macOS fallback context menu enabled for future right-click menus."
                                .to_string()
                        } else if std::env::var_os("THINKTERM_FORCE_FALLBACK_CONTEXT_MENU")
                            .is_some()
                        {
                            "Fallback context menu setting disabled, but the environment variable still forces fallback."
                                .to_string()
                        } else {
                            "macOS native context menu restored for future right-click menus."
                                .to_string()
                        };
                    }
                    Err(err) => {
                        self.status = format!("Unable to save context menu setting: {err:#}");
                    }
                }
            }
            SettingsAction::UnarchiveArchivedRow(index) => {
                self.ui.open_dropdown = None;
                self.ui.confirm_delete_archived = None;
                if let Some(row) = self.ui.archived_rows.get(index).cloned() {
                    if crate::workspace_threads::unarchive_project(&row.id) {
                        let mut args = FluentArgs::new();
                        args.set("name", row.name.clone());
                        self.status = crate::i18n::tr_args("settings-archived-unarchived", &args);
                        if let Some(front_end) = crate::frontend::try_front_end() {
                            front_end.invalidate_all_windows();
                        }
                    } else {
                        // The only refusal for a row that still exists is a
                        // detached remote domain.
                        self.status = crate::i18n::tr("settings-archived-remote-offline");
                    }
                }
                window.invalidate();
            }
            SettingsAction::DeleteArchivedRow(index) => {
                self.ui.open_dropdown = None;
                if let Some(row) = self.ui.archived_rows.get(index).cloned() {
                    if self.ui.confirm_delete_archived.as_deref() == Some(&row.id) {
                        self.ui.confirm_delete_archived = None;
                        if crate::workspace_threads::remove_project(&row.id).is_some() {
                            let mut args = FluentArgs::new();
                            args.set("name", row.name.clone());
                            self.status = crate::i18n::tr_args("settings-archived-deleted", &args);
                            if let Some(front_end) = crate::frontend::try_front_end() {
                                front_end.invalidate_all_windows();
                            }
                        } else {
                            self.status = crate::i18n::tr("settings-archived-remote-offline");
                        }
                    } else {
                        // First click arms; the second one, on the relabeled
                        // button, deletes for good.
                        self.ui.confirm_delete_archived = Some(row.id.clone());
                    }
                }
                window.invalidate();
            }
            SettingsAction::ToggleRightSidebarPanel(
                crate::termwindow::RightSidebarMode::Snippets,
            ) => {
                // Snippets is a built-in plugin, and its panel's switch is
                // the plugin's, which every client follows. What was chosen
                // is kept here too, so this desktop's panel follows at once.
                self.ui.open_dropdown = None;
                let enabled = !self
                    .right_sidebar_panel_enabled(crate::termwindow::RightSidebarMode::Snippets);
                let chrome = &mut self.native_settings.chrome;
                chrome.right_sidebar_snippets_enabled = None;
                chrome.snippets_plugin_enabled = Some(enabled);
                match crate::native_settings::save(&self.native_settings) {
                    Ok(()) => right_sidebar_panels_changed(),
                    Err(err) => {
                        self.status = format!("Unable to save sidebar panel setting: {err:#}");
                    }
                }
                crate::plugins::set_enabled(thinkterm_snippets::wire::PLUGIN, enabled);
            }
            SettingsAction::TogglePlugin(key) => {
                self.ui.open_dropdown = None;
                let switch = self
                    .ui
                    .plugin_switches
                    .iter()
                    .find(|(shown, _, _)| *shown == key)
                    .cloned();
                if let Some((_, id, enabled)) = switch {
                    crate::plugins::set_enabled(&id, !enabled);
                }
                window.invalidate();
            }
            SettingsAction::AllowPlugin(key) => {
                self.ui.open_dropdown = None;
                let new = self
                    .ui
                    .plugin_switches
                    .iter()
                    .find(|(shown, _, _)| *shown == key)
                    .cloned();
                if let Some((_, id, _)) = new {
                    crate::plugins::set_enabled(&id, true);
                }
                window.invalidate();
            }
            SettingsAction::TogglePluginBackgroundMenu(key) => {
                let menu = SettingsDropdown::PluginBackground(key);
                self.ui.open_dropdown = if self.ui.open_dropdown == Some(menu) {
                    None
                } else {
                    Some(menu)
                };
                window.invalidate();
            }
            SettingsAction::SetPluginBackground(key, background) => {
                self.ui.open_dropdown = None;
                let offered = self
                    .ui
                    .plugin_backgrounds
                    .iter()
                    .find(|(shown, _, _)| *shown == key)
                    .cloned();
                if let Some((_, id, default)) = offered {
                    crate::plugins::set_background(&id, background, default);
                }
                window.invalidate();
            }
            SettingsAction::ReloadPlugins => {
                self.ui.open_dropdown = None;
                crate::plugins::reload();
            }
            SettingsAction::OpenPluginsFolder => {
                self.ui.open_dropdown = None;
                let dir = thinkterm_plugin_channel::paths::plugins_dir();
                // Made so there is something to open, and to drop a plugin
                // into.
                if let Err(err) = std::fs::create_dir_all(&dir) {
                    self.status = format!("Unable to create {}: {err:#}", dir.display());
                } else {
                    Self::open_path(dir);
                }
            }
            SettingsAction::ToggleRightSidebarPanel(panel) => {
                self.ui.open_dropdown = None;
                let slot = self.right_sidebar_panel_slot(panel);
                *slot = Some(!slot.unwrap_or(true));
                match crate::native_settings::save(&self.native_settings) {
                    Ok(()) => {
                        // After the save so the agent detector's preference
                        // closure reads the new shared value; without this the
                        // gate waits for the next safety tick.
                        mux::agent_status::refresh_enabled();
                        right_sidebar_panels_changed();
                    }
                    Err(err) => {
                        self.status = format!("Unable to save sidebar panel setting: {err:#}");
                    }
                }
            }
            SettingsAction::ToggleTabIcons => {
                let enabled = !crate::tab_icons::catalog().enabled;
                let result = crate::tab_icons::set_enabled(enabled);
                self.after_tab_icon_change(result);
            }
            SettingsAction::TabIconSelect(index) => {
                if let Some(id) = self.ui.tab_icon_cards.get(index as usize).cloned() {
                    self.select_tab_icon(id);
                }
            }
            SettingsAction::TabIconNew => match crate::tab_icons::create_card(None) {
                Ok(Some(id)) => {
                    // A query would keep the new card out of the grid.
                    self.ui.tab_icon_search.clear();
                    self.select_tab_icon(id);
                    self.after_tab_icon_change(Ok(()));
                    self.set_focused_input(Some(SettingsAction::TabIconNameInput));
                }
                Ok(None) => {
                    self.status = settings_tr(
                        "settings-status-tab-icons-too-many",
                        &[("count", crate::tab_icons::MAX_CUSTOM_CARDS.to_string())],
                    );
                }
                Err(err) => self.after_tab_icon_change(Err(err)),
            },
            SettingsAction::TabIconRemoveProgram(index) => {
                let program = self.ui.tab_icon_programs.get(index as usize).cloned();
                if let (Some(id), Some(program)) = (self.selected_tab_icon(), program) {
                    let result = crate::tab_icons::remove_program(&id, &program);
                    self.after_tab_icon_change(result);
                }
            }
            SettingsAction::TabIconProgramInput
            | SettingsAction::TabIconNameInput
            | SettingsAction::TabIconCircleInput
            | SettingsAction::TabIconGlyphColorInput
            | SettingsAction::TabIconSearchInput => {
                // Focused on press; there is nothing more to do on release.
            }
            SettingsAction::ClearTabIconSearch => {
                self.ui.tab_icon_search.clear();
                self.set_focused_input(Some(SettingsAction::TabIconSearchInput));
            }
            SettingsAction::TabIconCirclePreset(index) => {
                let preset = crate::tab_icons::CIRCLE_PRESETS
                    .get(index as usize)
                    .copied();
                if let (Some(id), Some(color)) = (self.selected_tab_icon(), preset) {
                    let result = crate::tab_icons::set_card_circle(&id, color);
                    self.after_tab_icon_change(result);
                }
            }
            SettingsAction::TabIconGlyphPreset(index) => {
                let preset = crate::tab_icons::GLYPH_PRESETS.get(index as usize).copied();
                if let (Some(id), Some(color)) = (self.selected_tab_icon(), preset) {
                    let result = crate::tab_icons::set_card_glyph_color(&id, color);
                    self.after_tab_icon_change(result);
                }
            }
            SettingsAction::TabIconChooseSvg => self.choose_tab_icon_svg(),
            SettingsAction::TabIconClearSvg => {
                if let Some(id) = self.selected_tab_icon() {
                    let result = crate::tab_icons::clear_card_svg(&id);
                    self.after_tab_icon_change(result);
                }
            }
            SettingsAction::TabIconReset => {
                if let Some(id) = self.selected_tab_icon() {
                    let result = crate::tab_icons::reset_card(&id);
                    self.after_tab_icon_change(result);
                }
            }
            SettingsAction::TabIconDelete => {
                let Some(id) = self.selected_tab_icon() else {
                    return;
                };
                if self.ui.confirm_delete_tab_icon.as_deref() == Some(id.as_str()) {
                    let result = crate::tab_icons::delete_card(&id);
                    self.ui.tab_icon_selected = None;
                    self.ui.confirm_delete_tab_icon = None;
                    self.after_tab_icon_change(result);
                } else {
                    self.ui.confirm_delete_tab_icon = Some(id);
                }
            }
            SettingsAction::ToggleAgentDetails(agent_id) => {
                self.ui.open_dropdown = None;
                self.agents_expanded = if self.agents_expanded == Some(agent_id) {
                    None
                } else {
                    Some(agent_id)
                };
            }
            SettingsAction::ShowOnboardingNow => {
                self.ui.open_dropdown = None;
                let Some(front_end) = crate::frontend::try_front_end() else {
                    self.status = "No main ThinkTerm window is available.".to_string();
                    return;
                };
                let windows = front_end.gui_windows();
                if windows.is_empty() {
                    self.status = "No main ThinkTerm window is open.".to_string();
                } else {
                    let count = windows.len();
                    for gui_window in windows {
                        gui_window
                            .window
                            .notify(crate::termwindow::TermWindowNotif::Apply(Box::new(
                                |term_window| {
                                    term_window.show_onboarding();
                                },
                            )));
                    }
                    self.status = format!("Onboarding opened in {count} main window(s).");
                }
            }
            SettingsAction::ToggleMemoryMonitoring => {
                self.ui.open_dropdown = None;
                self.ui.memory_monitoring = !self.ui.memory_monitoring;
                self.ui.memory_monitor_generation =
                    self.ui.memory_monitor_generation.wrapping_add(1);
                if self.ui.memory_monitoring {
                    let snapshot = capture_memory_snapshot(false);
                    log::info!("settings memory diagnostics: {}", snapshot.log_line());
                    self.ui.memory_snapshot = Some(snapshot);
                    self.request_main_window_resource_stats();
                    self.status = "Memory diagnostics are running manually.".to_string();
                    self.schedule_memory_monitor_tick(window, self.ui.memory_monitor_generation);
                } else {
                    self.status = "Memory diagnostics stopped.".to_string();
                }
            }
            SettingsAction::RefreshMemorySnapshot => {
                self.ui.open_dropdown = None;
                let snapshot = capture_memory_snapshot(true);
                log::info!("settings memory diagnostics: {}", snapshot.log_line());
                self.ui.memory_snapshot = Some(snapshot);
                self.request_main_window_resource_stats();
                self.status = "Memory snapshot refreshed.".to_string();
            }
            SettingsAction::CopyMemorySnapshot => {
                self.ui.open_dropdown = None;
                if self
                    .ui
                    .memory_snapshot
                    .as_ref()
                    .is_none_or(|snapshot| !snapshot.has_vmmap_breakdown())
                {
                    self.ui.memory_snapshot = Some(capture_memory_snapshot(true));
                }
                if let Some(snapshot) = &self.ui.memory_snapshot {
                    let mut summary = snapshot.summary_for_clipboard();
                    summary.push_str("\n\nThinkTerm Resource Stats\n");
                    summary.push_str(&self.memory_resource_lines().join("\n"));
                    summary.push_str("\n\n");
                    summary.push_str(
                        &crate::input_diagnostics::snapshot()
                            .summary_lines()
                            .join("\n"),
                    );
                    window.set_clipboard(Clipboard::Clipboard, summary);
                    self.request_main_window_resource_stats();
                    self.ui.memory_snapshot_copied_until =
                        Some(Instant::now() + Duration::from_millis(1400));
                    self.status = "Memory snapshot copied.".to_string();
                    self.schedule_copied_state_clear(window);
                }
            }
            SettingsAction::CheckForUpdates => {
                self.ui.open_dropdown = None;
                if matches!(update_check(), Some(UpdateCheck::Running)) {
                    return;
                }
                set_update_check(Some(UpdateCheck::Running));
                // A new check is the next attempt after a failed install.
                if matches!(update_install(), Some(UpdateInstall::Failed { .. })) {
                    set_update_install(None);
                }
                window.invalidate();

                // A live query to GitHub: off the UI thread, with the answer
                // posted back to whichever Settings window is open by then.
                let spawned = std::thread::Builder::new()
                    .name("thinkterm-update-check".into())
                    .spawn(move || {
                        let result = crate::update::check_now();
                        promise::spawn::spawn_into_main_thread(async move {
                            let checked = result.is_ok();
                            set_update_check(result.err().map(|err| UpdateCheck::Failed {
                                error: format!("{err:#}"),
                            }));
                            repaint_open_settings(true);
                            // Let the button say it ran, even when the answer
                            // is the one already showing.
                            if let Some(settings) = open_settings_window().filter(|_| checked) {
                                if let Ok(mut settings) = settings.try_borrow_mut() {
                                    settings.ui.update_checked_until =
                                        Some(Instant::now() + Duration::from_millis(1400));
                                    if let Some(window) = settings.window.clone() {
                                        settings.schedule_copied_state_clear(&window);
                                    }
                                }
                            }
                        })
                        .detach();
                    });
                if let Err(err) = spawned {
                    set_update_check(Some(UpdateCheck::Failed {
                        error: err.to_string(),
                    }));
                }
            }
            SettingsAction::InstallUpdate => {
                self.ui.open_dropdown = None;
                if matches!(update_install(), Some(UpdateInstall::Running { .. })) {
                    return;
                }
                let Some(release) = self
                    .ui
                    .update_status
                    .as_ref()
                    .and_then(|status| status.latest.clone())
                else {
                    return;
                };
                let method = self
                    .ui
                    .update_method
                    .clone()
                    .unwrap_or_else(thinkterm_update::InstallMethod::detect);
                let version = release.tag_name.trim_start_matches('v').to_string();
                set_update_install(Some(UpdateInstall::Running {
                    version: version.clone(),
                    progress: None,
                    download_started: None,
                }));
                if matches!(update_check(), Some(UpdateCheck::Failed { .. })) {
                    set_update_check(None);
                }
                window.invalidate();

                // The installer downloads and replaces files: off the UI
                // thread, with its progress and outcome posted back to
                // whichever Settings window is open by then.
                let spawned = std::thread::Builder::new()
                    .name("thinkterm-update-install".into())
                    .spawn(move || {
                        // A cache written by a build that did not keep
                        // GitHub's checksums lacks them: ask for the release
                        // again then, and settle for the cache if that fails.
                        let release = if release.assets.iter().any(|a| a.sha256().is_none()) {
                            thinkterm_update::get_release_by_tag(&release.tag_name)
                                .unwrap_or(release)
                        } else {
                            release
                        };
                        let mut progress = |progress| {
                            promise::spawn::spawn_into_main_thread(async move {
                                // Never over the outcome, should a report
                                // land after it.
                                let reported = UPDATE_INSTALL.with(|install| {
                                    let mut install = install.borrow_mut();
                                    let Some(UpdateInstall::Running {
                                        progress: current,
                                        download_started,
                                        ..
                                    }) = &mut *install
                                    else {
                                        return false;
                                    };
                                    use thinkterm_update::InstallProgress;
                                    if let InstallProgress::Downloading { done, .. } = progress {
                                        // A retry, or a redirect's body thrown
                                        // away, starts the count over, and the
                                        // rate is timed from there.
                                        let restarted = matches!(
                                            current,
                                            Some(InstallProgress::Downloading { done: before, .. })
                                                if done < *before
                                        );
                                        if download_started.is_none() || restarted {
                                            *download_started = Some(Instant::now());
                                        }
                                    }
                                    *current = Some(progress);
                                    true
                                });
                                if reported {
                                    repaint_open_settings(false);
                                }
                            })
                            .detach();
                        };
                        let result = if cfg!(windows) {
                            thinkterm_update::run_windows_installer(&release, &mut progress)
                        } else {
                            match method.manifest_for_install() {
                                Some(manifest) => thinkterm_update::run_local_installer_captured(
                                    &manifest,
                                    &release,
                                    &mut progress,
                                )
                                .map(|_| ()),
                                None => Err(anyhow::anyhow!("{}", method.how_to_update())),
                            }
                        };
                        let outcome = match result {
                            Ok(()) => UpdateInstall::Installed { version },
                            Err(err) => UpdateInstall::Failed {
                                error: format!("{err:#}"),
                            },
                        };
                        promise::spawn::spawn_into_main_thread(async move {
                            set_update_install(Some(outcome));
                            repaint_open_settings(true);
                        })
                        .detach();
                    });
                if let Err(err) = spawned {
                    set_update_install(Some(UpdateInstall::Failed {
                        error: err.to_string(),
                    }));
                }
            }
            SettingsAction::OpenLatestRelease => {
                self.ui.open_dropdown = None;
                let url = self
                    .ui
                    .update_status
                    .as_ref()
                    .and_then(|status| status.latest.as_ref())
                    .map(|latest| crate::update::release_tag_url(&latest.tag_name))
                    .unwrap_or_else(crate::update::releases_url);
                wezterm_open_url::open_url(&url);
            }
            SettingsAction::OpenReleasesIndex => {
                self.ui.open_dropdown = None;
                wezterm_open_url::open_url(&crate::update::releases_url());
            }
            SettingsAction::OpenSourceRepository => {
                self.ui.open_dropdown = None;
                wezterm_open_url::open_url(crate::update::REPO_URL);
            }
            SettingsAction::OpenThirdPartyNotices => {
                self.ui.open_dropdown = None;
                wezterm_open_url::open_url(&format!(
                    "{}/blob/main/NOTICE",
                    crate::update::REPO_URL
                ));
            }
            SettingsAction::OpenPrivacyPolicy => {
                self.ui.open_dropdown = None;
                wezterm_open_url::open_url(&format!(
                    "{}/blob/main/PRIVACY.md",
                    crate::update::REPO_URL
                ));
            }
            SettingsAction::OpenDataFolder => {
                self.ui.open_dropdown = None;
                Self::open_path(config::DATA_DIR.clone());
            }
            SettingsAction::ExportBackup => self.choose_backup_folder(false),
            SettingsAction::ImportBackup => self.choose_backup_folder(true),
            SettingsAction::CopyVersionInfo => {
                self.ui.open_dropdown = None;
                window.set_clipboard(Clipboard::Clipboard, self.version_info_for_clipboard());
                self.ui.version_info_copied_until =
                    Some(Instant::now() + Duration::from_millis(1400));
                self.schedule_copied_state_clear(window);
            }
            SettingsAction::ToggleInputDiagnostics => {
                self.ui.open_dropdown = None;
                let enabled = !crate::input_diagnostics::enabled();
                crate::input_diagnostics::set_enabled(enabled);
                self.status = if enabled {
                    "Input diagnostics started. Leave this running while you reproduce typing lag."
                        .to_string()
                } else {
                    "Input diagnostics stopped.".to_string()
                };
            }
            SettingsAction::ResetInputDiagnostics => {
                self.ui.open_dropdown = None;
                crate::input_diagnostics::reset();
                self.status = "Input diagnostics reset.".to_string();
            }
            SettingsAction::CopyInputDiagnostics => {
                self.ui.open_dropdown = None;
                window.set_clipboard(
                    Clipboard::Clipboard,
                    crate::input_diagnostics::snapshot()
                        .summary_lines()
                        .join("\n"),
                );
                self.ui.input_diagnostics_copied_until =
                    Some(Instant::now() + Duration::from_millis(1400));
                self.status = "Input diagnostics copied.".to_string();
                self.schedule_copied_state_clear(window);
            }
            SettingsAction::OpenColorSchemePicker => {
                self.ui.open_dropdown = None;
                self.set_focused_input(None);
                match self.color_scheme_picker_target() {
                    Some(gui_window) => {
                        // Bring it forward first: the palette it is about to
                        // open is a preview, and a preview behind the settings
                        // window previews nothing.
                        gui_window.window.focus();
                        gui_window.window.notify(
                            crate::termwindow::TermWindowNotif::Apply(Box::new(|term_window| {
                                term_window.open_color_scheme_picker();
                            })),
                        );
                    }
                    None => {
                        self.status = crate::i18n::tr("settings-color-scheme-needs-a-window");
                    }
                }
            }
            SettingsAction::ToggleCommandPaletteHotkeyMenu => {
                self.ui.open_dropdown =
                    if self.ui.open_dropdown == Some(SettingsDropdown::CommandPaletteHotkey) {
                        None
                    } else {
                        Some(SettingsDropdown::CommandPaletteHotkey)
                    };
            }
            SettingsAction::SetCommandPaletteHotkey(hotkey) => {
                self.native_settings.command_palette.hotkey = hotkey;
                self.ui.open_dropdown = None;
                self.save_command_palette_settings();
            }
            SettingsAction::DecreaseCommandPaletteRows => self.step_command_palette_rows(-1),
            SettingsAction::IncreaseCommandPaletteRows => self.step_command_palette_rows(1),
            SettingsAction::ResetCommandPaletteRows => {
                self.native_settings.command_palette.rows = 0;
                self.save_command_palette_settings();
            }
            SettingsAction::DecreaseCommandPaletteFontSize => {
                self.step_command_palette_font_size(-1.0)
            }
            SettingsAction::IncreaseCommandPaletteFontSize => {
                self.step_command_palette_font_size(1.0)
            }
            SettingsAction::ResetCommandPaletteFontSize => {
                self.native_settings.command_palette.font_size = None;
                self.save_command_palette_settings();
            }
            SettingsAction::ToggleCommandPaletteSearchPenetration => {
                self.ui.open_dropdown = None;
                self.native_settings.command_palette.search_penetrates_groups =
                    !self.native_settings.command_palette.search_penetrates_groups;
                self.save_command_palette_settings();
            }
            SettingsAction::ToggleLanguageMenu => {
                self.ui.open_dropdown = if self.ui.open_dropdown == Some(SettingsDropdown::Language)
                {
                    None
                } else {
                    Some(SettingsDropdown::Language)
                };
            }
            SettingsAction::SetLanguage(preference) => {
                self.ui.open_dropdown = None;
                let previous_language = self.native_settings.localization.language.clone();
                self.native_settings.localization.language = Some(preference.to_string());
                match crate::native_settings::save(&self.native_settings) {
                    Ok(()) => {
                        let locale = crate::i18n::activate_preference(preference);
                        self.status.clear();
                        self.ui.sidebar_scroll.reset();
                        self.sync_selected_section_with_search();
                        window.set_title(&crate::i18n::tr("settings-window-title"));
                        if let Some(front_end) = crate::frontend::try_front_end() {
                            for gui_window in front_end.gui_windows() {
                                gui_window.window.notify(
                                    crate::termwindow::TermWindowNotif::Apply(Box::new(
                                        |term_window| {
                                            term_window.dismiss_fallback_context_menu();
                                        },
                                    )),
                                );
                            }
                            front_end.invalidate_all_windows();
                        }
                        let mut args = FluentArgs::new();
                        args.set("language", locale);
                        self.status = crate::i18n::tr_args("settings-language-changed", &args);
                        window.invalidate();
                    }
                    Err(err) => {
                        self.native_settings.localization.language = previous_language;
                        let mut args = FluentArgs::new();
                        args.set("error", format!("{err:#}"));
                        self.status = crate::i18n::tr_args("settings-language-save-error", &args);
                    }
                }
            }
            SettingsAction::SetThemeMode(mode) => {
                let previous_mode = self.native_settings.appearance.theme_mode;
                self.native_settings.appearance.theme_mode = mode;
                // The pick previews from this window's own copy, so the cached
                // chrome has to follow it here rather than waiting for an
                // appearance event that only a system theme change sends.
                self.refresh_chrome();
                self.ui.open_dropdown = None;
                match crate::native_settings::save(&self.native_settings) {
                    Ok(()) => {
                        crate::native_settings::apply_to_app(&self.native_settings);
                        if let Some(front_end) = crate::frontend::try_front_end() {
                            front_end.invalidate_all_windows();
                        }
                        let mut args = FluentArgs::new();
                        args.set("mode", localized_theme_mode_label(mode));
                        self.status = crate::i18n::tr_args("settings-theme-changed", &args);
                    }
                    Err(err) => {
                        // Put it back: nothing else in the process took the
                        // new mode, so leaving it here would have this window
                        // previewing a theme that was never saved.
                        self.native_settings.appearance.theme_mode = previous_mode;
                        self.refresh_chrome();
                        let mut args = FluentArgs::new();
                        args.set("error", format!("{err:#}"));
                        self.status = crate::i18n::tr_args("settings-theme-save-error", &args);
                    }
                }
            }
            SettingsAction::SetAppIcon(icon) => {
                self.native_settings.appearance.app_icon = icon;
                self.ui.open_dropdown = None;
                match crate::native_settings::save(&self.native_settings) {
                    Ok(()) => {
                        crate::native_settings::apply_to_app(&self.native_settings);
                        self.status = settings_tr(
                            "settings-status-app-icon",
                            &[("icon", localized_app_icon_label(icon))],
                        );
                    }
                    Err(err) => {
                        self.status = settings_tr(
                            "settings-status-app-icon-error",
                            &[("error", format!("{err:#}"))],
                        );
                    }
                }
            }
            SettingsAction::SearchInput => {
                self.set_focused_input(Some(SettingsAction::SearchInput));
            }
            SettingsAction::DecreaseFontSize => self.step_terminal_font_size(-1.0),
            SettingsAction::IncreaseFontSize => self.step_terminal_font_size(1.0),
            SettingsAction::ResetFontSize => self.reset_terminal_font_size(),
            SettingsAction::DecreaseChromeFontSize(area) => self.step_chrome_font_size(area, -1.0),
            SettingsAction::IncreaseChromeFontSize(area) => self.step_chrome_font_size(area, 1.0),
            SettingsAction::ResetChromeFontSize(area) => self.reset_chrome_font_size(area),
            SettingsAction::DecreaseSettingsFontWeight => self.step_settings_font_weight(-100),
            SettingsAction::IncreaseSettingsFontWeight => self.step_settings_font_weight(100),
            SettingsAction::ResetSettingsFontWeight => self.reset_settings_font_weight(),
            SettingsAction::ToggleUiFontMenu => {
                if self.ui.open_dropdown == Some(SettingsDropdown::UiFont) {
                    self.ui.open_dropdown = None;
                } else {
                    self.open_ui_font_menu();
                }
            }
            SettingsAction::SetUiFont(index) => {
                self.ui.open_dropdown = None;
                // `get` rather than indexing, as for the shell catalog: a
                // stale index must be a no-op, not a panic.
                let family = match index {
                    Some(index) => match self.ui.ui_font_families.get(index) {
                        Some(family) => Some(family.clone()),
                        None => return,
                    },
                    None => None,
                };
                self.set_ui_font_family(family);
            }
            SettingsAction::FontFamilyInput => {
                self.set_focused_input(Some(SettingsAction::FontFamilyInput));
            }
            SettingsAction::RemoteDropDestinationInput => {
                self.set_focused_input(Some(SettingsAction::RemoteDropDestinationInput));
            }
            SettingsAction::ClearSearch => {
                self.ui.search.clear();
                self.ui.sidebar_scroll.reset();
                self.sync_selected_section_with_search();
                self.set_focused_input(Some(SettingsAction::SearchInput));
            }
            SettingsAction::SidebarResize
            | SettingsAction::SidebarScrollArea
            | SettingsAction::ContentScrollArea => {}
        }
    }

    fn do_paint(&mut self, window: &Window) -> bool {
        if self.render_state.is_none() {
            return false;
        }

        let paint_start = crate::perf::now();
        let animating = self.advance_scroll_animations(Instant::now());
        let result = if self.webgpu.is_some() {
            self.do_paint_webgpu()
        } else {
            self.do_paint_opengl(window)
        };
        match result {
            Ok(ok) => {
                crate::perf::log_duration("settings_paint", paint_start);
                if animating || std::mem::take(&mut self.repaint_after_growth) {
                    window.invalidate();
                }
                ok
            }
            Err(err) => {
                crate::perf::log_duration("settings_paint_failed", paint_start);
                log::error!("settings window paint failed: {err:#}");
                false
            }
        }
    }

    fn advance_scroll_animations(&mut self, now: Instant) -> bool {
        self.ui.sidebar_scroll.advance_animation(now)
            | self.ui.content_scroll.advance_animation(now)
            | self.ui.ui_font_menu_scroll.advance_animation(now)
    }

    fn do_paint_webgpu(&mut self) -> anyhow::Result<bool> {
        let webgpu = Rc::clone(
            self.webgpu
                .as_ref()
                .context("settings webgpu state not initialized")?,
        );
        match self.do_paint_webgpu_impl(&webgpu) {
            Ok(ok) => Ok(ok),
            Err(err) => {
                match err.downcast_ref::<wgpu::SurfaceError>() {
                    Some(wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated) => {
                        webgpu.resize(self.dimensions);
                        return self.do_paint_webgpu_impl(&webgpu);
                    }
                    _ => {}
                }
                Err(err)
            }
        }
    }

    fn prepare_paint(&mut self) -> anyhow::Result<bool> {
        let mut growing = false;
        for _ in 0..3 {
            growing = false;
            let paint_pass_start = crate::perf::now();
            match self.paint_pass() {
                Ok(()) => {
                    crate::perf::log_duration("settings_paint_pass", paint_pass_start);
                    match self.render_state.as_mut().unwrap().allocated_more_quads() {
                        Ok(true) => {
                            growing = true;
                            continue;
                        }
                        Ok(false) => break,
                        Err(err) => {
                            log::error!("settings window quad allocation failed: {err:#}");
                            // The layers hold a pass that ran out of quads:
                            // keep the previous frame rather than draw it.
                            return Ok(false);
                        }
                    }
                }
                Err(err) => {
                    if let Some(&OutOfTextureSpace {
                        size: Some(size),
                        current_size,
                    }) = err.root_cause().downcast_ref::<OutOfTextureSpace>()
                    {
                        // Glyphs of fonts since let go (the Terminal
                        // page's previews) still fill it: clear it at its
                        // size first, and grow only if the frame still does
                        // not fit.
                        let size = if std::mem::take(&mut self.ui.stale_glyphs) {
                            current_size
                        } else {
                            size.max(current_size)
                        };
                        if size > crate::termwindow::MAX_ATLAS_SIZE {
                            // The settings window draws no images, so a
                            // glyph working set past the cap is not
                            // something a downscale could fix. The failed
                            // pass already cleared the layers, so drawing
                            // now would present a half-built frame: return
                            // without drawing and the previous frame stays.
                            log::error!(
                                "settings window atlas would need {size} texels per side, \
                                 past the {} cap; keeping the previous frame",
                                crate::termwindow::MAX_ATLAS_SIZE
                            );
                            // Clear in place before giving up: a full atlas
                            // of stale glyphs asks for double its size, and
                            // without a clear every later paint would hit
                            // this same branch and the window would freeze.
                            if let Err(err) =
                                self.render_state.as_mut().unwrap().recreate_texture_atlas(
                                    &self.fonts,
                                    &self.metrics,
                                    Some(current_size),
                                )
                            {
                                log::error!("settings window atlas clear failed: {err:#}");
                            }
                            self.invalidate_shaped_text();
                            return Ok(false);
                        }
                        crate::perf::log_counter("settings_atlas_reallocate", size);
                        let recreated = self.render_state.as_mut().unwrap().recreate_texture_atlas(
                            &self.fonts,
                            &self.metrics,
                            Some(size),
                        );
                        // Every cached glyph now points into the old atlas,
                        // whether or not the new one was allocated.
                        self.invalidate_shaped_text();
                        if let Err(err) = recreated {
                            log::error!("settings window texture atlas resize failed: {err:#}");
                            break;
                        }
                        growing = true;
                        continue;
                    }
                    log::error!("settings window paint failed: {err:#}");
                    break;
                }
            }
        }

        if growing {
            // Every pass found the frame bigger than the room made for it --
            // a page of icons grows the atlas twice and the quad buffers once
            // on its first paint -- so the layers hold part of a frame. Keep
            // the previous one on screen and come straight back.
            self.repaint_after_growth = true;
            return Ok(false);
        }
        Ok(true)
    }

    fn do_paint_opengl(&mut self, window: &Window) -> anyhow::Result<bool> {
        if self.dimensions.pixel_width == 0 || self.dimensions.pixel_height == 0 {
            return Ok(false);
        }
        if !self.prepare_paint()? {
            return Ok(false);
        }
        let render_state = self
            .render_state
            .as_ref()
            .context("settings render state missing")?;
        let RenderContext::Glium(gl) = &render_state.context else {
            anyhow::bail!("settings OpenGL context missing");
        };
        anyhow::ensure!(!gl.is_context_lost(), "settings OpenGL context was lost");
        let config = configuration();
        let corner_radius = crate::termwindow::render::draw::effective_window_corner_radius(
            config.window_decorations,
            self.window_state,
            self.dimensions.dpi,
        );
        let clear_color = if corner_radius > 0.0 {
            LinearRgba::with_components(0.0, 0.0, 0.0, 0.0)
        } else {
            self.palette().window_bg
        };
        let mut frame = window::glium::Frame::new(
            Rc::clone(gl),
            (
                self.dimensions.pixel_width as u32,
                self.dimensions.pixel_height as u32,
            ),
        );
        let no_blink = std::array::from_fn(|_| crate::colorease::ColorEaseUniform {
            in_function: [1.0; 4],
            out_function: [0.0; 4],
            in_duration_ms: 1,
            out_duration_ms: 1,
        });
        let draw_result = draw_opengl_layers(
            &mut frame,
            render_state,
            self.dimensions,
            [1.0, 1.0, 1.0],
            0,
            clear_color,
            corner_radius,
            crate::termwindow::render::draw::effective_window_border(
                config.window_decorations,
                self.window_state,
                self.dimensions.dpi,
                self.effective_appearance(),
            ),
            matches!(
                config
                    .freetype_render_target
                    .unwrap_or(config.freetype_load_target),
                config::FreeTypeLoadTarget::HorizontalLcd | config::FreeTypeLoadTarget::VerticalLcd
            ),
            &no_blink,
        );
        // glium panics when an unfinished Frame is dropped, even on draw failure.
        let present_result = window
            .finish_frame(frame)
            .context("present settings OpenGL frame");
        draw_result?;
        present_result?;
        Ok(true)
    }

    fn do_paint_webgpu_impl(&mut self, webgpu: &WebGpuState) -> anyhow::Result<bool> {
        if !self.prepare_paint()? {
            return Ok(false);
        }

        let corner_radius = crate::termwindow::render::draw::effective_window_corner_radius(
            configuration().window_decorations,
            self.window_state,
            self.dimensions.dpi,
        );
        let window_border = crate::termwindow::render::draw::effective_window_border(
            configuration().window_decorations,
            self.window_state,
            self.dimensions.dpi,
            self.effective_appearance(),
        );
        let clear_color = if corner_radius > 0.0 {
            wgpu::Color::TRANSPARENT
        } else {
            wgpu_color(self.palette().window_bg)
        };
        let render_state = self
            .render_state
            .as_ref()
            .context("settings render state not initialized")?;
        // The settings window draws no terminal pictures.
        let no_images = crate::termwindow::render::paint::ImageCompositeBatch::default();
        draw_webgpu_layers(
            webgpu,
            render_state,
            self.dimensions,
            [1.0, 1.0, 1.0],
            0,
            clear_color,
            corner_radius,
            window_border,
            usize::MAX, // settings window; no mux window behind it
            crate::termwindow::render::draw::CardDrawData {
                pending: Vec::new(),
                composites: Vec::new(),
            },
            &no_images,
            &mut None,
            &mut Vec::new(),
        )?;
        Ok(true)
    }

    fn paint_pass(&mut self) -> anyhow::Result<()> {
        self.pending_glyph_error.borrow_mut().take();
        if let Some(render_state) = self.render_state.as_ref() {
            for layer in render_state.layers.borrow().iter() {
                layer.clear_quad_allocation();
            }
        }

        self.ui_context.clear();
        let layer = self
            .render_state
            .as_ref()
            .context("settings render state not initialized")?
            .layer_for_zindex(0)?;
        let mut layers = layer.quad_allocator();

        self.paint_background(&mut layers)?;
        self.paint_sidebar(&mut layers)?;
        self.paint_content(&mut layers)?;
        self.paint_content_chrome_mask(&mut layers)?;
        self.paint_window_chrome(&mut layers)?;

        if let Some(err) = self.pending_glyph_error.borrow_mut().take() {
            return Err(err);
        }

        Ok(())
    }

    fn settings_window_shows_window_buttons(&self) -> bool {
        if cfg!(target_os = "macos") {
            return false;
        }

        let config = configuration();
        crate::termwindow::ui::platform_chrome::uses_integrated_window_buttons(
            config.window_decorations,
            self.window_state,
        ) && config.integrated_title_button_style != IntegratedTitleButtonStyle::MacOsNative
            && !config.integrated_title_buttons.is_empty()
    }

    fn settings_window_shows_window_buttons_for_config(config: &config::ConfigHandle) -> bool {
        config
            .window_decorations
            .contains(WindowDecorations::INTEGRATED_BUTTONS)
            && config.integrated_title_button_style != IntegratedTitleButtonStyle::MacOsNative
            && !config.integrated_title_buttons.is_empty()
    }

    fn sidebar_brand_font_size_for_config(config: &config::ConfigHandle) -> f64 {
        if !cfg!(target_os = "macos")
            && Self::settings_window_shows_window_buttons_for_config(config)
        {
            SIDEBAR_BRAND_FONT_SIZE_WITH_CUSTOM_CHROME
        } else {
            SIDEBAR_BRAND_FONT_SIZE
        }
    }

    fn settings_window_chrome_drag_hit(&self, x: f32, y: f32) -> bool {
        self.settings_window_shows_window_buttons()
            && x >= 0.0
            && x <= self.dimensions.pixel_width as f32
            && (0.0..=self.ui_px(SETTINGS_WINDOW_CHROME_HEIGHT)).contains(&y)
    }

    fn paint_window_chrome(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
    ) -> anyhow::Result<()> {
        if !self.settings_window_shows_window_buttons() {
            return Ok(());
        }

        let config = configuration();
        let mut right =
            self.dimensions.pixel_width as f32 - self.ui_px(SETTINGS_WINDOW_BUTTON_RIGHT_INSET);
        let y = self.ui_px(SETTINGS_WINDOW_BUTTON_TOP_INSET);
        for button in config.integrated_title_buttons.iter().rev() {
            right -= self.ui_px(SETTINGS_WINDOW_BUTTON_SIZE);
            self.paint_window_chrome_button(layers, *button, right, y)?;
            right -= self.ui_px(SETTINGS_WINDOW_BUTTON_GAP);
        }

        Ok(())
    }

    fn paint_window_chrome_button(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        button: IntegratedTitleButton,
        x: f32,
        y: f32,
    ) -> anyhow::Result<()> {
        let action = match button {
            IntegratedTitleButton::Hide => SettingsAction::WindowHide,
            IntegratedTitleButton::Maximize => SettingsAction::WindowMaximize,
            IntegratedTitleButton::Close => SettingsAction::WindowClose,
        };
        let palette = self.palette();
        let button_rect = rect(
            x,
            y,
            self.ui_px(SETTINGS_WINDOW_BUTTON_SIZE),
            self.ui_px(SETTINGS_WINDOW_BUTTON_SIZE),
        );
        self.ui_context
            .push(button_rect, WidgetKind::Button, action);

        let hovered = self.ui.interaction.hovered == Some(action);
        let pressed = self.ui.interaction.pressed == Some(action);
        let close_button = button == IntegratedTitleButton::Close;
        let press_inset = if pressed { 1.0 } else { 0.0 };
        let visual_size = self.ui_px(SETTINGS_WINDOW_BUTTON_SIZE) - press_inset * 2.0;

        if hovered {
            let fill = if close_button {
                if pressed {
                    LinearRgba::with_srgba(232, 17, 35, 209)
                } else {
                    LinearRgba::with_srgba(232, 17, 35, 255)
                }
            } else if pressed {
                palette.control_pressed_bg
            } else {
                palette.control_hover_bg
            };
            let border = if close_button {
                LinearRgba::TRANSPARENT
            } else {
                palette.text.mul_alpha(if pressed { 0.52 } else { 0.38 })
            };
            self.draw_rounded_frame(
                layers,
                1,
                x + press_inset,
                y + press_inset,
                visual_size,
                visual_size,
                fill,
                border,
                999.0,
            )?;
        }

        let maximized = self
            .window_state
            .intersects(WindowState::MAXIMIZED | WindowState::FULL_SCREEN);
        let icon = match button {
            IntegratedTitleButton::Hide => SvgIcon::Minus,
            IntegratedTitleButton::Maximize if maximized => SvgIcon::Copy,
            IntegratedTitleButton::Maximize => SvgIcon::Square,
            IntegratedTitleButton::Close => SvgIcon::X,
        };
        let icon_size = if pressed {
            self.ui_px(SETTINGS_WINDOW_BUTTON_ICON_SIZE) - 1.0
        } else {
            self.ui_px(SETTINGS_WINDOW_BUTTON_ICON_SIZE)
        };
        let icon_color = if close_button && hovered {
            LinearRgba(1.0, 1.0, 1.0, 1.0)
        } else if hovered {
            palette.text
        } else {
            palette.muted_text
        };
        self.draw_svg_icon(
            layers,
            icon,
            x + press_inset + (visual_size - icon_size) / 2.0,
            y + press_inset + (visual_size - icon_size) / 2.0,
            icon_size,
            icon_color,
        )
    }

    fn paint_content_chrome_mask(
        &self,
        layers: &mut TripleLayerQuadAllocator<'_>,
    ) -> anyhow::Result<()> {
        if !self.settings_window_shows_window_buttons() {
            return Ok(());
        }

        let palette = self.palette();
        let x = self.ui.sidebar.width + 1.0;
        let width = (self.dimensions.pixel_width as f32 - x).max(0.0);
        self.draw_rect(
            layers,
            2,
            x,
            0.0,
            width,
            self.ui_px(SETTINGS_WINDOW_CHROME_HEIGHT),
            palette.window_bg,
        )?;

        let fade_height = self.ui_usize(SETTINGS_WINDOW_CHROME_FADE_HEIGHT).min(
            (self.content_bottom() - self.ui_px(SETTINGS_WINDOW_CHROME_HEIGHT)).max(0.0) as usize,
        );
        if self.ui.content_scroll.offset > 0.0 && fade_height > 0 {
            for step in 0..fade_height {
                let progress = (step + 1) as f32 / fade_height as f32;
                let alpha = 1.0 - progress * progress * (3.0 - 2.0 * progress);
                self.draw_rect(
                    layers,
                    2,
                    x,
                    self.ui_px(SETTINGS_WINDOW_CHROME_HEIGHT) + step as f32,
                    width,
                    1.0,
                    palette.window_bg.mul_alpha(alpha),
                )?;
            }
        }

        Ok(())
    }

    fn paint_background(&self, layers: &mut TripleLayerQuadAllocator<'_>) -> anyhow::Result<()> {
        let palette = self.palette();
        let width = self.dimensions.pixel_width as f32;
        let height = self.dimensions.pixel_height as f32;
        let sidebar_width = self.ui.sidebar.width;
        self.draw_rect(layers, 0, 0.0, 0.0, width, height, palette.window_bg)?;
        self.draw_rect(
            layers,
            0,
            0.0,
            0.0,
            sidebar_width,
            height,
            palette.sidebar_bg,
        )?;
        self.draw_rect(
            layers,
            0,
            sidebar_width,
            0.0,
            1.0,
            height,
            palette.separator,
        )?;
        Ok(())
    }

    fn paint_sidebar(&mut self, layers: &mut TripleLayerQuadAllocator<'_>) -> anyhow::Result<()> {
        let palette = self.palette();
        let body_font = Rc::clone(&self.body_font);
        let sidebar_title_font = Rc::clone(&self.sidebar_title_font);
        let nav_font = Rc::clone(&self.ui_font);
        let tokens = self.ui.tokens;
        let sidebar_width = self.ui.sidebar.width;
        let sidebar_icon_size = self.sidebar_icon_size();

        self.draw_text(
            layers,
            &sidebar_title_font,
            tokens.sidebar_padding + 6.0,
            self.sidebar_title_y(),
            "ThinkTerm",
            palette.title,
            sidebar_width - tokens.sidebar_padding * 2.0,
        )?;

        let search_rect = rect(
            tokens.sidebar_padding,
            self.ui_px(SIDEBAR_SEARCH_Y),
            sidebar_width - tokens.sidebar_padding * 2.0,
            tokens.control_height,
        );
        // Painted below, after the nav list and on the top layer: the mask
        // that hides the list's overhang reaches up into this field, so the
        // field has to go down after it and above it.
        let handle_rect = rect(
            sidebar_width - tokens.resize_handle_width / 2.0,
            0.0,
            tokens.resize_handle_width,
            self.dimensions.pixel_height as f32,
        );
        self.ui_context.push(
            handle_rect,
            WidgetKind::ResizeHandle,
            SettingsAction::SidebarResize,
        );
        let handle_color = if self.ui.interaction.hovered == Some(SettingsAction::SidebarResize)
            || matches!(self.ui.drag, Some(SettingsDrag::SidebarResize { .. }))
        {
            palette.nav_selected_bg
        } else {
            palette.separator
        };
        self.draw_rect(
            layers,
            0,
            sidebar_width - 1.0,
            0.0,
            2.0,
            self.dimensions.pixel_height as f32,
            handle_color,
        )?;

        let sections = self.filtered_sections();
        // A square a little larger than the old glyph, as System Settings
        // sizes its own, never crowding the row.
        let tile_side = (sidebar_icon_size + self.ui_px(12.0))
            .min(self.nav_row_height() - self.ui_px(14.0))
            .round();
        let list_top = self.sidebar_list_top();
        let list_bottom = (self.dimensions.pixel_height as f32 - self.ui_px(16.0)).max(list_top);
        let list_height = list_bottom - list_top;
        let content_extent = sections.len() as f32 * self.nav_row_step() + 8.0;
        self.ui
            .sidebar_scroll
            .set_extents(list_height, content_extent.max(list_height));
        let list_area = rect(0.0, list_top, sidebar_width, list_height);
        self.ui_context.push(
            list_area,
            WidgetKind::ScrollArea,
            SettingsAction::SidebarScrollArea,
        );

        let mut y = list_top - self.ui.sidebar_scroll.offset;
        if sections.is_empty() {
            self.draw_text(
                layers,
                &body_font,
                tokens.sidebar_padding + 12.0,
                list_top + self.ui_px(18.0),
                &crate::i18n::tr("settings-search-no-results"),
                palette.muted_text,
                sidebar_width - tokens.sidebar_padding * 2.0 - 24.0,
            )?;
        }

        for section in sections {
            if y + self.nav_row_height() < list_top || y > list_bottom {
                y += self.nav_row_step();
                continue;
            }
            let action = SettingsAction::Select(section);
            let selected = section == self.selected;
            let hovered = self.ui.interaction.hovered == Some(action);
            let pressed = self.ui.interaction.pressed == Some(action);
            let row_y = y;
            let row_x = tokens.sidebar_padding;
            let row_width = sidebar_width - tokens.sidebar_padding * 2.0;
            let row_bg = if selected {
                Some(palette.nav_selected_bg)
            } else if pressed {
                Some(palette.nav_pressed_bg)
            } else if hovered {
                Some(palette.nav_hover_bg)
            } else {
                None
            };
            let row_height = self.nav_row_height();
            let row_radius = self.ui_px(NAV_ROW_RADIUS);
            if selected {
                // Shadow, fill, then hairline border -- the same three passes
                // the main window's sidebar gives its active row.
                self.paint_active_row_shadow(
                    layers, row_x, row_y, row_width, row_height, row_radius,
                )?;
                self.draw_rounded_frame(
                    layers,
                    0,
                    row_x,
                    row_y,
                    row_width,
                    row_height,
                    palette.nav_selected_bg,
                    palette.nav_selected_border,
                    row_radius,
                )?;
            } else if let Some(row_bg) = row_bg {
                self.draw_rounded_rect(
                    layers, 0, row_x, row_y, row_width, row_height, row_bg, row_radius,
                )?;
            }

            // Clamped to the list, matching the main window's sidebar: the
            // part of a half-scrolled row that sits above the list is hidden,
            // and a hidden row must not be clickable.
            let hit_top = row_y.max(list_top);
            let hit_bottom = (row_y + self.nav_row_height()).min(list_bottom);
            if hit_bottom > hit_top {
                self.ui_context.push(
                    rect(row_x, hit_top, row_width, hit_bottom - hit_top),
                    WidgetKind::SidebarRow,
                    action,
                );
            }
            let text_color = if selected {
                palette.selected_text
            } else {
                palette.secondary_text
            };
            self.paint_tile(
                layers,
                row_x + self.ui_px(12.0),
                row_y + ((row_height - tile_side) / 2.0).round(),
                tile_side,
                section.icon().svg(),
                section.tile_color(),
            )?;
            let text_x = row_x + self.ui_px(12.0) + tile_side + self.ui_px(16.0);
            let section_label = section.label();
            self.draw_text(
                layers,
                &nav_font,
                text_x,
                self.control_text_y(row_y, self.nav_row_height()),
                &section_label,
                text_color,
                row_x + row_width - text_x - self.ui_px(20.0),
            )?;
            y += self.nav_row_step();
        }

        self.paint_sidebar_top_mask(layers, list_area)?;
        let search_text = self.ui.search.text().to_string();
        self.paint_text_input(
            layers,
            2,
            TextInputSpec {
                placeholder: &crate::i18n::tr("settings-search-placeholder"),
                text: &search_text,
                rect: search_rect,
                focused: self.ui.interaction.focused == Some(SettingsAction::SearchInput),
                selected_all: self.ui.search.selected_all,
                action: SettingsAction::SearchInput,
            },
        )?;
        self.draw_svg_icon(
            layers,
            SettingsIcon::Search.svg(),
            search_rect.origin.x + self.ui_px(15.0),
            search_rect.origin.y + (search_rect.size.height - sidebar_icon_size) / 2.0,
            sidebar_icon_size,
            palette.muted_text,
        )?;
        if !self.ui.search.is_empty() {
            let clear_size = self.ui_px(34.0);
            let clear_icon_size = self.ui_px(24.0);
            let clear_rect = rect(
                search_rect.origin.x + search_rect.size.width - clear_size - self.ui_px(10.0),
                search_rect.origin.y + (search_rect.size.height - clear_size) / 2.0,
                clear_size,
                clear_size,
            );
            self.ui_context
                .push(clear_rect, WidgetKind::Button, SettingsAction::ClearSearch);
            if self.ui.interaction.hovered == Some(SettingsAction::ClearSearch)
                || self.ui.interaction.pressed == Some(SettingsAction::ClearSearch)
            {
                // Layer 2, with the field: layers composite bottom-up
                // regardless of call order, so a highlight on layer 0 sits
                // under the field's own fill and is never seen.
                self.draw_rounded_rect(
                    layers,
                    2,
                    clear_rect.origin.x,
                    clear_rect.origin.y,
                    clear_rect.size.width,
                    clear_rect.size.height,
                    palette.control_hover_bg,
                    self.ui_px(14.0),
                )?;
            }
            self.draw_svg_icon(
                layers,
                SettingsIcon::Clear.svg(),
                clear_rect.origin.x + (clear_rect.size.width - clear_icon_size) / 2.0,
                clear_rect.origin.y + (clear_rect.size.height - clear_icon_size) / 2.0,
                clear_icon_size,
                palette.secondary_text,
            )?;
        }

        self.paint_scrollbar(
            layers,
            list_area,
            self.ui.sidebar_scroll,
            self.sidebar_scrollbar_visible(),
        )?;

        Ok(())
    }

    /// Hide what the nav list hangs above its own area.
    ///
    /// Rows are culled against the list but not clipped to it, so a row
    /// scrolled halfway out still paints its full height -- upwards, into the
    /// search field. The opaque band swallows that overhang and the ramp under
    /// it dissolves the row, the way `ssh_hosts_view::paint_list_fades` does
    /// for its list. Only the top: at the bottom the row runs off the window,
    /// which is what a list is supposed to look like.
    fn paint_sidebar_top_mask(
        &self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        list_area: window::RectF,
    ) -> anyhow::Result<()> {
        if list_area.size.width <= 0.0 || list_area.size.height <= 0.0 {
            return Ok(());
        }
        let palette = self.palette();
        let scroll = self.ui.sidebar_scroll;

        // Opaque, a full row tall, ending at the list. A row is culled only
        // once it is entirely above the list, so it hangs up to one row height
        // over this band -- which means the band reaches into the search field
        // and the field is repainted over it on a higher layer. Sizing the
        // band to the tallest element is the rule from
        // `right_sidebar::sidebar_row_element_visible`; a shorter band silently
        // goes back to withholding rows near the edge instead of sliding them.
        let gap_top = list_area.origin.y - self.nav_row_height();
        let gap_height = list_area.origin.y - gap_top;
        if gap_height > 0.0 {
            self.draw_rect(
                layers,
                2,
                list_area.origin.x,
                gap_top,
                list_area.size.width,
                gap_height,
                palette.sidebar_bg,
            )?;
        }

        let fade = self
            .ui_px(SIDEBAR_LIST_FADE_HEIGHT)
            .min(list_area.size.height)
            .ceil() as usize;
        if fade == 0 {
            return Ok(());
        }
        if scroll.offset > 0.5 {
            for step in 0..fade {
                let progress = step as f32 / fade as f32;
                let alpha = 1.0 - progress * progress * (3.0 - 2.0 * progress);
                self.draw_rect(
                    layers,
                    2,
                    list_area.origin.x,
                    list_area.origin.y + step as f32,
                    list_area.size.width,
                    1.0,
                    palette.sidebar_bg.mul_alpha(alpha),
                )?;
            }
        }
        Ok(())
    }

    fn paint_content(&mut self, layers: &mut TripleLayerQuadAllocator<'_>) -> anyhow::Result<()> {
        self.ui.dropdown_anchors.clear();
        // One line outside a card; a card says how many inside it.
        self.ui.row_description_lines = 1;
        if self.selected != SettingsSection::Terminal {
            if !self.ui.preview_fonts.is_empty() {
                self.ui.preview_fonts.clear();
                // Their glyphs stay in the atlas, which cannot free them
                // one by one.
                self.ui.stale_glyphs = true;
            }
            self.ui.confirm_reset_quotes = false;
        }
        let palette = self.palette();
        let ui_font = Rc::clone(&self.ui_font);
        let title_font = Rc::clone(&self.title_font);
        let sidebar_width = self.ui.sidebar.width;
        let window_width = self.dimensions.pixel_width as f32;
        let narrow = window_width < self.ui_px(980.0);
        let content_gap = self.ui_px(if narrow { 30.0 } else { 46.0 });
        let right_margin = self.ui_px(if narrow { 34.0 } else { 50.0 });
        let x = sidebar_width + content_gap;
        let max_width = (window_width - x - right_margin).max(self.ui_px(280.0));
        let content_top = self.content_scroll_area_top();
        let content_area = rect(
            sidebar_width + 1.0,
            content_top,
            self.dimensions.pixel_width as f32 - sidebar_width - 1.0,
            (self.content_bottom() - content_top).max(0.0),
        );
        self.ui_context.push(
            content_area,
            WidgetKind::ScrollArea,
            SettingsAction::ContentScrollArea,
        );

        let scroll = self.ui.content_scroll.offset;
        let selected_label = self.selected.label();
        if self.selected != SettingsSection::Import {
            self.draw_text(
                layers,
                &title_font,
                x,
                self.ui_px(CONTENT_TITLE_Y) - scroll,
                &selected_label,
                palette.title,
                max_width,
            )?;
        }

        match self.selected {
            SettingsSection::Appearance => self.paint_appearance(layers, x, max_width)?,
            SettingsSection::TabIcons => self.paint_tab_icons(layers, x, max_width)?,
            SettingsSection::Import => self.paint_import(layers, x, max_width)?,
            SettingsSection::General => self.paint_general(layers, x, max_width)?,
            SettingsSection::Terminal => self.paint_terminal(layers, x, max_width)?,
            SettingsSection::Workspaces => self.paint_workspaces(layers, x, max_width)?,
            SettingsSection::Agents => self.paint_agents(layers, x, max_width)?,
            SettingsSection::Web => self.paint_web(layers, x, max_width)?,
            SettingsSection::Archived => self.paint_archived(layers, x, max_width)?,
            SettingsSection::Keymap => self.paint_placeholder(
                layers,
                &ui_font,
                x,
                &crate::i18n::tr("settings-keymap-description"),
                max_width,
            )?,
            SettingsSection::CommandPalette => {
                self.paint_command_palette_section(layers, x, max_width)?
            }
            SettingsSection::Developer => self.paint_developer(layers, x, max_width)?,
            SettingsSection::UiKit => self.paint_ui_kit(layers, x, max_width)?,
            SettingsSection::Memory => self.paint_memory_diagnostics(layers, x, max_width)?,
            SettingsSection::Sidebar => self.paint_sidebar_settings(layers, x, max_width)?,
            SettingsSection::Backup => self.paint_backup(layers, x, max_width)?,
            SettingsSection::Update => self.paint_update(layers, x, max_width)?,
            SettingsSection::About => self.paint_about(layers, x, max_width)?,
        }
        self.paint_scrollbar(
            layers,
            content_area,
            self.ui.content_scroll,
            self.content_scrollbar_visible(),
        )?;
        self.paint_open_dropdown_overlay(layers)?;
        self.paint_hint_overlay(layers, x, max_width)?;

        Ok(())
    }

    /// The bubble for the ⓘ under the pointer, painted after everything
    /// else so it sits on top; nothing when no ⓘ is hovered.
    fn paint_hint_overlay(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        content_x: f32,
        max_width: f32,
    ) -> anyhow::Result<()> {
        let Some((key, icon_x, icon_y, icon_size)) = self.ui.hint.take() else {
            return Ok(());
        };
        let palette = self.palette();
        let body_font = Rc::clone(&self.body_font);
        let text = crate::i18n::tr(key);
        let pad = self.ui_px(12.0);
        let bubble_width = self.ui_px(380.0).min(max_width);
        let lines = self.wrap_settings_text(&body_font, &text, bubble_width - pad * 2.0);
        let line_step = self.metrics.cell_size.height as f32 + self.ui_px(4.0);
        let height = pad * 2.0 + line_step * lines.len().max(1) as f32;
        // Left-aligned with the icon and above it; below when the top of
        // the content is too close.
        let bubble_x = (icon_x - pad)
            .min(content_x + max_width - bubble_width)
            .max(content_x);
        let gap = self.ui_px(8.0);
        let bubble_y = if icon_y - gap - height >= self.ui_px(8.0) {
            icon_y - gap - height
        } else {
            icon_y + icon_size + gap
        };
        self.draw_rounded_frame(
            layers,
            2,
            bubble_x,
            bubble_y,
            bubble_width,
            height,
            palette.control_bg,
            palette.control_border,
            self.ui_px(10.0),
        )?;
        for (index, line) in lines.iter().enumerate() {
            self.draw_text_on_layer(
                layers,
                2,
                &body_font,
                bubble_x + pad,
                bubble_y + pad + line_step * index as f32,
                line,
                palette.text,
                bubble_width - pad * 2.0,
            )?;
        }
        Ok(())
    }

    /// Paint one card of rows. `rows` paints them into a buffer, from the
    /// card's first row down, and returns where they end; the card is sized
    /// to that and drawn under them. Returns the card's bottom. Rows can
    /// then take the height their descriptions need instead of a fixed step.
    fn paint_card(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        card_y: f32,
        width: f32,
        rows: impl FnOnce(&mut Self, &mut TripleLayerQuadAllocator<'_>, f32) -> anyhow::Result<f32>,
    ) -> anyhow::Result<f32> {
        let layout = self.card_layout();
        self.paint_card_as(layers, x, card_y, width, layout, rows)
    }

    /// `paint_card` laid out as `layout` says: the rows start `layout.top`
    /// into the card, the card ends `layout.bottom` below them, and their
    /// descriptions wrap to `layout.description_lines` -- a limit that is
    /// the card's, since only a card sized to its rows makes room for it.
    fn paint_card_as(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        card_y: f32,
        width: f32,
        layout: CardLayout,
        rows: impl FnOnce(&mut Self, &mut TripleLayerQuadAllocator<'_>, f32) -> anyhow::Result<f32>,
    ) -> anyhow::Result<f32> {
        let mut buffer = HeapQuadAllocator::default();
        let outside =
            std::mem::replace(&mut self.ui.row_description_lines, layout.description_lines);
        let rows_bottom = {
            let mut buffered = TripleLayerQuadAllocator::Heap(&mut buffer);
            rows(self, &mut buffered, card_y + layout.top)
        };
        self.ui.row_description_lines = outside;
        let height = rows_bottom? + layout.bottom - card_y;
        self.paint_group_card(layers, x, card_y, width, height)?;
        buffer.apply_to(layers)?;
        Ok(card_y + height)
    }

    /// How most cards lay out: rows of a label and a line or two of
    /// explanation, with room above and below sized for them.
    fn card_layout(&self) -> CardLayout {
        CardLayout {
            top: self.settings_card_top_padding(),
            bottom: self.settings_card_bottom_padding(),
            description_lines: 2,
        }
    }

    /// A window drawn small: rounded, filled with `ground`, with a sidebar
    /// `sidebar` wide down its left in `sidebar_ground`. Only the window's
    /// own corners are round. `window` is x, y, width, height and radius.
    fn paint_window_shape(
        &self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        window: (f32, f32, f32, f32, f32),
        sidebar: f32,
        ground: LinearRgba,
        sidebar_ground: LinearRgba,
    ) -> anyhow::Result<()> {
        let (x, y, width, height, radius) = window;
        self.draw_rounded_rect(layers, 0, x, y, width, height, ground, radius)?;
        self.draw_rounded_rect(
            layers,
            0,
            x,
            y,
            sidebar + radius,
            height,
            sidebar_ground,
            radius,
        )?;
        // The sidebar's inner corners, covered back up.
        self.draw_rect(layers, 0, x + sidebar, y, radius, height, ground)
    }

    /// A heading over the next card, `gap` below whatever came before;
    /// returns where that card starts.
    fn paint_card_heading(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        above: f32,
        width: f32,
        heading: &str,
    ) -> anyhow::Result<f32> {
        let palette = self.palette();
        let body_font = Rc::clone(&self.body_font);
        let heading_y = above + self.settings_section_card_gap();
        self.draw_text(
            layers,
            &body_font,
            x,
            heading_y,
            heading,
            palette.muted_text,
            width,
        )?;
        Ok(heading_y + self.settings_section_card_gap().min(54.0))
    }

    /// The square heading a one-line row whose label sits in the middle of
    /// a band `band` tall (the rows that keep their explanation behind an
    /// ⓘ). Returns where the row's text starts and how wide it may be.
    fn paint_band_tile(
        &self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        band: f32,
        width: f32,
        icon: SvgIcon,
        color: TileColor,
    ) -> anyhow::Result<(f32, f32)> {
        let side = self.ui_px(ROW_TILE_SIDE);
        self.paint_tile(layers, x, y + (band - side) / 2.0, side, icon, color)?;
        let indent = side + self.ui_px(ROW_TILE_GAP);
        Ok((x + indent, (width - indent).max(0.0)))
    }

    fn paint_general(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        max_width: f32,
    ) -> anyhow::Result<()> {
        let scroll = self.ui.content_scroll.offset;
        let section_y = self.ui_px(CONTENT_SECTION_Y) - scroll;
        let card_padding = self.ui_px(36.0);
        let row_x = x + card_padding;
        let row_width = max_width - card_padding * 2.0;
        let (card_y, _) = self.settings_card_geometry(section_y, 0);

        let bottom = self.paint_card(layers, x, card_y, max_width, |this, layers, top| {
            let mut rows = RowCursor::new(top, this);
            let (tx, tw) = this.paint_row_tile(
                layers,
                row_x,
                rows.y,
                row_width,
                SvgIcon::Globe,
                TileColor::Blue,
            )?;
            rows.add(this.paint_language_row(layers, tx, rows.y, tw, rows.rule())?);
            let (tx, tw) = this.paint_row_tile(
                layers,
                row_x,
                rows.y,
                row_width,
                SvgIcon::Cpu,
                TileColor::Teal,
            )?;
            rows.add(this.paint_main_renderer_row(layers, tx, rows.y, tw, rows.rule())?);
            let (tx, tw) = this.paint_row_tile(
                layers,
                row_x,
                rows.y,
                row_width,
                SvgIcon::AppWindow,
                TileColor::Orange,
            )?;
            rows.add(this.paint_toggle_setting_row(
                layers,
                tx,
                rows.y,
                tw,
                &crate::i18n::tr("settings-restore-window-frame"),
                &crate::i18n::tr("settings-restore-window-frame-description"),
                this.native_settings.window.restore_main_window_frame,
                SettingsAction::ToggleMainWindowFrameRestore,
                rows.rule(),
            )?);
            Ok(rows.bottom)
        })?;

        // The files the settings come from.
        let source = Self::config_source_summary();
        let config_source_kind = if config::configuration_file().is_some() {
            crate::i18n::tr("common-file")
        } else {
            crate::i18n::tr("common-defaults")
        };
        let native_path = Self::native_settings_path();
        let native_status = if native_path.exists() {
            home_relative(&native_path)
        } else {
            let mut args = FluentArgs::new();
            args.set("path", home_relative(&native_path));
            crate::i18n::tr_args("settings-native-settings-default", &args)
        };
        let native_version = format!("v{}", self.native_settings.version);
        let card_y = self.paint_card_heading(
            layers,
            x,
            bottom,
            max_width,
            &crate::i18n::tr("settings-general-heading"),
        )?;
        let mut bottom = self.paint_card(layers, x, card_y, max_width, |this, layers, top| {
            let mut rows = RowCursor::new(top, this);
            let (tx, tw) = this.paint_row_tile(
                layers,
                row_x,
                rows.y,
                row_width,
                SvgIcon::FileCode,
                TileColor::Gray,
            )?;
            rows.add(this.paint_setting_row(
                layers,
                tx,
                rows.y,
                tw,
                &crate::i18n::tr("settings-config-source"),
                &source,
                &config_source_kind,
                rows.rule(),
            )?);
            let (tx, tw) = this.paint_row_tile(
                layers,
                row_x,
                rows.y,
                row_width,
                SvgIcon::FileCog,
                TileColor::Gray,
            )?;
            rows.add(this.paint_setting_row(
                layers,
                tx,
                rows.y,
                tw,
                &crate::i18n::tr("settings-native-settings"),
                &native_status,
                &native_version,
                rows.rule(),
            )?);
            Ok(rows.bottom)
        })?;

        // Where the platform can keep local sessions in a server.
        if crate::local_sessions::supported() {
            let server_running = crate::local_sessions::server_running(&config::configuration());
            let card_y = self.paint_card_heading(
                layers,
                x,
                bottom,
                max_width,
                &crate::i18n::tr("settings-general-session-heading"),
            )?;
            // The bands carry their own air, so the card adds little.
            let layout = CardLayout {
                top: self.ui_px(12.0),
                bottom: self.ui_px(12.0),
                ..self.card_layout()
            };
            bottom =
                self.paint_card_as(layers, x, card_y, max_width, layout, |this, layers, top| {
                    let band = this.compact_row_step();
                    let y = top;
                    let (tx, tw) = this.paint_band_tile(
                        layers,
                        row_x,
                        y,
                        band,
                        row_width,
                        SvgIcon::Server,
                        TileColor::Slate,
                    )?;
                    this.paint_toggle_setting_row_with_hint(
                        layers,
                        tx,
                        y,
                        tw,
                        band,
                        &crate::i18n::tr("settings-local-sessions-via-mux"),
                        // Windows has no in-place handoff: sessions survive
                        // quitting and crashing there, not updating, and the
                        // text says so.
                        if cfg!(windows) {
                            "settings-local-sessions-via-mux-description-windows"
                        } else {
                            "settings-local-sessions-via-mux-description"
                        },
                        this.native_settings.workspaces.local_sessions_via_mux,
                        SettingsAction::ToggleLocalSessionsViaMux,
                    )?;
                    let mut end = y + band;
                    if server_running {
                        let button = crate::i18n::tr(if this.ui.confirm_stop_server {
                            "settings-stop-session-server-confirm"
                        } else {
                            "settings-stop-session-server-now"
                        });
                        let (tx, tw) = this.paint_band_tile(
                            layers,
                            row_x,
                            end,
                            band,
                            row_width,
                            SvgIcon::CircleStop,
                            TileColor::Slate,
                        )?;
                        this.paint_button_setting_row_with_hint(
                            layers,
                            tx,
                            end,
                            tw,
                            band,
                            &crate::i18n::tr("settings-stop-session-server"),
                            "settings-stop-session-server-description",
                            &button,
                            SettingsAction::StopSessionServer,
                            true,
                        )?;
                        end += band;
                    }
                    Ok(end)
                })?;
        }

        let card_y = self.paint_card_heading(
            layers,
            x,
            bottom,
            max_width,
            &crate::i18n::tr("settings-general-app-heading"),
        )?;
        let bottom = self.paint_card(layers, x, card_y, max_width, |this, layers, top| {
            let mut rows = RowCursor::new(top, this);
            let (tx, tw) = this.paint_row_tile(
                layers,
                row_x,
                rows.y,
                row_width,
                SvgIcon::RotateCw,
                TileColor::Green,
            )?;
            rows.add(this.paint_restart_row(layers, tx, rows.y, tw, rows.rule())?);
            let (tx, tw) = this.paint_row_tile(
                layers,
                row_x,
                rows.y,
                row_width,
                SvgIcon::Power,
                TileColor::Red,
            )?;
            rows.add(this.paint_action_setting_row(
                layers,
                tx,
                rows.y,
                tw,
                &crate::i18n::tr("settings-quit"),
                &crate::i18n::tr("settings-quit-description"),
                &crate::i18n::tr("settings-quit-app"),
                SettingsAction::QuitApplication,
                rows.rule(),
            )?);
            Ok(rows.bottom)
        })?;

        self.ui.content_scroll.set_extents(
            self.content_viewport_extent(),
            self.settings_content_extent(bottom + scroll),
        );
        Ok(())
    }

    fn paint_appearance(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        max_width: f32,
    ) -> anyhow::Result<()> {
        let palette = self.palette();
        let body_font = Rc::clone(&self.body_font);
        let scroll = self.ui.content_scroll.offset;
        let section_y = self.ui_px(CONTENT_SECTION_Y) - scroll;
        let card_padding = self.ui_px(36.0);
        let row_x = x + card_padding;
        let row_width = max_width - card_padding * 2.0;

        self.draw_text(
            layers,
            &body_font,
            x,
            section_y,
            &crate::i18n::tr("settings-appearance-theme-heading"),
            palette.muted_text,
            max_width,
        )?;
        let (card_y, _) = self.settings_card_geometry(section_y, 0);
        let bottom = self.paint_card(layers, x, card_y, max_width, |this, layers, top| {
            let mut rows = RowCursor::new(top, this);
            let end = this.paint_theme_choices(layers, row_x, rows.y, row_width)?;
            rows.add(end - (rows.y + rows.visual));
            let (tx, tw) = this.paint_row_tile(
                layers,
                row_x,
                rows.y,
                row_width,
                SvgIcon::Palette,
                TileColor::Purple,
            )?;
            rows.add(this.paint_color_scheme_row(layers, tx, rows.y, tw, rows.rule())?);
            let (tx, tw) = this.paint_row_tile(
                layers,
                row_x,
                rows.y,
                row_width,
                SvgIcon::AppWindow,
                TileColor::Pink,
            )?;
            rows.add(this.paint_app_icon_choice_row(layers, tx, rows.y, tw, rows.rule())?);
            Ok(rows.bottom)
        })?;

        let card_y = self.paint_card_heading(
            layers,
            x,
            bottom,
            max_width,
            &crate::i18n::tr("settings-typography-heading"),
        )?;
        // Its rows are bands, which carry their own air below.
        let layout = CardLayout {
            bottom: self.ui_px(12.0),
            ..self.card_layout()
        };
        let bottom =
            self.paint_card_as(layers, x, card_y, max_width, layout, |this, layers, top| {
                this.paint_typography_rows(layers, row_x, top, row_width)
            })?;

        let card_y = self.paint_card_heading(
            layers,
            x,
            bottom,
            max_width,
            &crate::i18n::tr("settings-appearance-window-heading"),
        )?;
        let bottom = self.paint_card(layers, x, card_y, max_width, |this, layers, top| {
            let mut rows = RowCursor::new(top, this);
            let (tx, tw) = this.paint_row_tile(
                layers,
                row_x,
                rows.y,
                row_width,
                SvgIcon::Contrast,
                TileColor::Blue,
            )?;
            rows.add(this.paint_window_opacity_row(layers, tx, rows.y, tw, rows.rule())?);
            Ok(rows.bottom)
        })?;

        self.ui.content_scroll.set_extents(
            self.content_viewport_extent(),
            self.settings_content_extent(bottom + scroll),
        );
        Ok(())
    }

    /// The terminal's colours as they are under theme `mode` -- the one in
    /// force, or one a thumbnail shows -- kept until what they depend on
    /// changes.
    fn terminal_colors(&mut self, mode: NativeThemeMode) -> TerminalColors {
        let key = TerminalColorsKey {
            mode,
            generation: configuration().generation(),
            picked: self.native_settings.appearance.color_scheme.clone(),
            appearance: self.effective_appearance(),
        };
        if let Some((_, colors)) = self
            .ui
            .terminal_colors
            .iter()
            .find(|(noted, _)| *noted == key)
        {
            return *colors;
        }
        let mut settings = self.native_settings.clone();
        settings.appearance.theme_mode = mode;
        let palette = crate::native_settings::terminal_palette(&settings, &configuration());
        let mut ansi = [LinearRgba::TRANSPARENT; 16];
        for (index, color) in ansi.iter_mut().enumerate() {
            *color = palette.colors.0[index].to_linear();
        }
        let colors = TerminalColors {
            background: palette.background.to_linear(),
            foreground: palette.foreground.to_linear(),
            cursor: palette.cursor_bg.to_linear(),
            ansi,
        };
        // One per mode: the one in force and the one Follow terminal shows.
        self.ui
            .terminal_colors
            .retain(|(noted, _)| noted.mode != mode);
        self.ui.terminal_colors.push((key, colors));
        colors
    }

    /// The palettes the theme thumbnails are drawn in. Follow terminal's is
    /// the scheme that mode would resolve to, not the one in force now: in
    /// Light that is the light scheme, which Follow terminal would drop.
    fn theme_previews(&mut self) -> ThemePreviews {
        let key = ThemePreviewKey {
            generation: configuration().generation(),
            picked: self.native_settings.appearance.color_scheme.clone(),
            appearance: self.effective_appearance(),
        };
        if let Some((noted, previews)) = &self.ui.theme_previews {
            if *noted == key {
                return *previews;
            }
        }
        let config = configuration();
        let terminal = self.terminal_colors(NativeThemeMode::FollowTerminal);
        let previews = ThemePreviews {
            light: crate::native_settings::chrome_palette(
                NativeThemeMode::Light,
                Appearance::Light,
                &config,
                None,
            ),
            dark: crate::native_settings::chrome_palette(
                NativeThemeMode::Dark,
                Appearance::Dark,
                &config,
                None,
            ),
            follow: crate::native_settings::chrome_palette(
                NativeThemeMode::FollowTerminal,
                crate::ui::UiPalette::appearance_of(terminal.background),
                &config,
                Some(terminal.background),
            ),
            // The mode that takes the terminal's colours says so with two
            // of them in place of its text.
            follow_ink: [terminal.ansi[2], terminal.ansi[4]],
        };
        self.ui.theme_previews = Some((key, previews));
        previews
    }

    /// The theme modes as pictures of a window in each, side by side: what a
    /// mode looks like says more than its name. Each is drawn from the
    /// colours that mode really resolves to. Returns where the block ends.
    fn paint_theme_choices(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
    ) -> anyhow::Result<f32> {
        let palette = self.palette();
        let body_font = Rc::clone(&self.body_font);
        let count = NativeThemeMode::ALL.len() as f32;
        let gap = self.ui_px(28.0);
        let thumb_width = ((width - gap * (count - 1.0)) / count)
            .min(self.ui_px(220.0))
            .floor();
        if thumb_width <= 0.0 {
            return Ok(y);
        }
        let thumb_height = (thumb_width * 0.62).round();
        let radius = self.ui_px(14.0).min(thumb_height / 4.0).round();
        let ring = self.ui_px(3.0).max(2.0).round();
        let label_y = y + thumb_height + self.ui_px(16.0);
        let label_room = thumb_width + gap * 0.75;
        let line_step = self.description_line_step();
        let cell_height = self.metrics.cell_size.height as f32;
        let chosen = self.native_settings.appearance.theme_mode;
        let accent = self.chrome_palette.accent;

        let ThemePreviews {
            light,
            dark,
            follow,
            follow_ink,
        } = self.theme_previews();

        let mut end = y + thumb_height;
        for (index, mode) in NativeThemeMode::ALL.iter().copied().enumerate() {
            let thumb_x = x + (thumb_width + gap) * index as f32;
            let action = SettingsAction::SetThemeMode(mode);
            let selected = chosen == mode;
            let hovered = self.ui.interaction.hovered == Some(action)
                || self.ui.interaction.pressed == Some(action);
            if selected || hovered {
                self.draw_rounded_rect(
                    layers,
                    0,
                    thumb_x - ring,
                    y - ring,
                    thumb_width + ring * 2.0,
                    thumb_height + ring * 2.0,
                    if selected {
                        accent
                    } else {
                        accent.mul_alpha(0.4)
                    },
                    radius + ring,
                )?;
            }
            let window = (thumb_x, y, thumb_width, thumb_height, radius);
            match mode {
                // Half of each, split down the middle.
                NativeThemeMode::System => {
                    let middle = (thumb_x + thumb_width / 2.0).round();
                    for (colors, left, right) in [
                        (&light, thumb_x - 1.0, middle),
                        (&dark, middle, thumb_x + thumb_width + 1.0),
                    ] {
                        let mut half = HeapQuadAllocator::default();
                        self.paint_mini_window(
                            &mut TripleLayerQuadAllocator::Heap(&mut half),
                            window,
                            colors,
                            None,
                        )?;
                        let clip = crate::quad::QuadClipRect::from_top_left_pixels(
                            left,
                            y - 1.0,
                            right,
                            y + thumb_height + 1.0,
                            &self.dimensions,
                        );
                        half.apply_to_clipped(layers, clip, 1.0)?;
                    }
                }
                NativeThemeMode::Light => self.paint_mini_window(layers, window, &light, None)?,
                NativeThemeMode::Dark => self.paint_mini_window(layers, window, &dark, None)?,
                NativeThemeMode::FollowTerminal => {
                    self.paint_mini_window(layers, window, &follow, Some(follow_ink))?
                }
            }

            let label = localized_theme_mode_label(mode);
            let lines = self.capped_lines(&body_font, &label, label_room, 2);
            let color = if selected {
                palette.text
            } else {
                palette.secondary_text
            };
            for (line_index, line) in lines.iter().enumerate() {
                let line_width = self.measure_text_width(&body_font, line).min(label_room);
                self.draw_text(
                    layers,
                    &body_font,
                    thumb_x + (thumb_width - line_width) / 2.0,
                    label_y + line_step * line_index as f32,
                    line,
                    color,
                    label_room,
                )?;
            }
            let label_bottom =
                label_y + line_step * lines.len().saturating_sub(1) as f32 + cell_height;
            end = end.max(label_bottom);
            self.ui_context.push(
                rect(
                    thumb_x - ring,
                    y - ring,
                    thumb_width + ring * 2.0,
                    label_bottom - y + ring,
                ),
                WidgetKind::Button,
                action,
            );
        }
        Ok(end + self.ui_px(10.0))
    }

    /// A window drawn small in `colors`: a sidebar with its current row, and
    /// a card holding a switch and two lines of text. `ink` recolours the
    /// text. Everything is opaque, so a ring drawn under it shows only
    /// around the edge. `window` is x, y, width, height and corner radius.
    fn paint_mini_window(
        &self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        window: (f32, f32, f32, f32, f32),
        colors: &crate::ui::UiPalette,
        ink: Option<[LinearRgba; 2]>,
    ) -> anyhow::Result<()> {
        let (x, y, width, height, radius) = window;
        let sidebar = (width * 0.3).round();
        let pad = (width * 0.05).round();
        let bar = (height * 0.055).max(self.ui_px(5.0)).round();
        self.paint_window_shape(
            layers,
            window,
            sidebar,
            opaque(colors.window_bg),
            opaque(colors.workspace_sidebar_bg),
        )?;

        let row_height = (bar * 3.4).round();
        let rows_y = y + (height * 0.2).round();
        self.draw_rounded_rect(
            layers,
            0,
            x + pad * 0.5,
            rows_y,
            sidebar - pad,
            row_height,
            opaque(colors.sidebar_row_active_bg),
            (row_height * 0.3).round(),
        )?;
        for (index, length) in [0.62, 0.48, 0.55].iter().copied().enumerate() {
            self.draw_rounded_rect(
                layers,
                0,
                x + pad * 1.2,
                rows_y + (row_height - bar) / 2.0 + row_height * 1.05 * index as f32,
                (sidebar - pad * 2.4) * length,
                bar,
                colors.muted_text.mul_alpha(0.7),
                bar / 2.0,
            )?;
        }

        let card_x = x + sidebar + pad;
        let card_width = width - sidebar - pad * 2.0;
        let card_y = rows_y;
        let card_height = (height * 0.6).round();
        self.draw_rounded_rect(
            layers,
            0,
            card_x,
            card_y,
            card_width,
            card_height,
            opaque(colors.card_bg),
            self.ui_px(8.0).round(),
        )?;
        let [first_ink, second_ink] = ink.unwrap_or([
            colors.text.mul_alpha(0.55),
            colors.muted_text.mul_alpha(0.6),
        ]);
        let first_y = card_y + (card_height * 0.3).round();
        self.draw_rounded_rect(
            layers,
            0,
            card_x + pad,
            first_y,
            card_width * 0.42,
            bar,
            first_ink,
            bar / 2.0,
        )?;
        let switch_height = (bar * 2.6).round();
        let switch_width = (switch_height * 1.7).round();
        let switch_x = card_x + card_width - pad - switch_width;
        let switch_y = (first_y + bar / 2.0 - switch_height / 2.0).round();
        self.draw_rounded_rect(
            layers,
            0,
            switch_x,
            switch_y,
            switch_width,
            switch_height,
            opaque(colors.accent),
            switch_height / 2.0,
        )?;
        let inset = (switch_height * 0.14).max(1.0).round();
        let knob = switch_height - inset * 2.0;
        self.draw_rounded_rect(
            layers,
            0,
            switch_x + switch_width - inset - knob,
            switch_y + inset,
            knob,
            knob,
            colors.on_accent,
            knob / 2.0,
        )?;
        self.draw_rounded_rect(
            layers,
            0,
            card_x + pad,
            card_y + (card_height * 0.64).round(),
            card_width * 0.64,
            bar,
            second_ink,
            bar / 2.0,
        )?;

        self.draw_rounded_frame(
            layers,
            0,
            x,
            y,
            width,
            height,
            LinearRgba::TRANSPARENT,
            colors.control_border,
            radius,
        )
    }

    /// The terminal's colour scheme shown as what it paints -- its ground,
    /// its text and eight of its colours -- beside its name. The whole face
    /// is one button into the command palette's scheme list.
    fn paint_color_scheme_row(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
        draw_top_rule: bool,
    ) -> anyhow::Result<f32> {
        let palette = self.palette();
        let ui_font = Rc::clone(&self.ui_font);
        if draw_top_rule {
            self.paint_separator(layers, x, y - self.ui_px(28.0), width)?;
        }
        let name = self.effective_color_scheme_label(&configuration());
        let terminal = self.terminal_colors(self.native_settings.appearance.theme_mode);

        let control_height = self.ui_px(CONTROL_HEIGHT);
        let control_y = y + self.ui_px(4.0);
        let dot = self.ui_px(14.0).round();
        let dot_gap = self.ui_px(6.0).round();
        let inner_pad = self.ui_px(12.0);
        let sample = "Aa";
        let sample_width = self.measure_text_width(&ui_font, sample);
        // Red to cyan, white, and bright black: black itself vanishes into
        // most grounds.
        let slots = [1, 2, 3, 4, 5, 6, 7, 8];
        let swatch_width = inner_pad * 2.0
            + dot * slots.len() as f32
            + dot_gap * (slots.len() - 1) as f32
            + self.ui_px(12.0)
            + sample_width;
        let lead = self.ui_px(8.0);
        let name_gap = self.ui_px(16.0);
        let chevron = self.ui_px(20.0);
        let chevron_gap = self.ui_px(10.0);
        let trail = self.ui_px(14.0);
        let fixed = lead + swatch_width + name_gap + chevron_gap + chevron + trail;
        let name_room = (width * 0.62 - fixed).max(self.ui_px(60.0));
        let name_width = self.measure_text_width(&ui_font, &name).min(name_room);
        let control_width = fixed + name_width;
        let control_x = x + width - control_width;

        let action = SettingsAction::OpenColorSchemePicker;
        self.ui_context.push(
            rect(control_x, control_y, control_width, control_height),
            WidgetKind::Button,
            action,
        );
        let fill = if self.ui.interaction.pressed == Some(action) {
            palette.control_pressed_bg
        } else if self.ui.interaction.hovered == Some(action) {
            palette.control_hover_bg
        } else {
            palette.control_bg
        };
        self.draw_rounded_frame(
            layers,
            0,
            control_x,
            control_y,
            control_width,
            control_height,
            fill,
            palette.control_border,
            self.ui_px(CONTROL_RADIUS),
        )?;

        let swatch_x = control_x + lead;
        let swatch_y = control_y + self.ui_px(8.0);
        let swatch_height = control_height - self.ui_px(16.0);
        let ground = terminal.background;
        self.draw_rounded_frame(
            layers,
            0,
            swatch_x,
            swatch_y,
            swatch_width,
            swatch_height,
            opaque(ground),
            palette.control_border,
            self.ui_px(CONTROL_RADIUS - 5.0),
        )?;
        let dot_y = (swatch_y + (swatch_height - dot) / 2.0).round();
        for (index, slot) in slots.iter().copied().enumerate() {
            self.draw_rounded_rect(
                layers,
                0,
                swatch_x + inner_pad + (dot + dot_gap) * index as f32,
                dot_y,
                dot,
                dot,
                terminal.ansi[slot],
                dot / 2.0,
            )?;
        }
        self.draw_text(
            layers,
            &ui_font,
            swatch_x + swatch_width - inner_pad - sample_width,
            self.control_text_y(swatch_y, swatch_height),
            sample,
            terminal.foreground,
            sample_width + 1.0,
        )?;

        let name_x = swatch_x + swatch_width + name_gap;
        self.draw_text(
            layers,
            &ui_font,
            name_x,
            self.control_text_y(control_y, control_height),
            &name,
            palette.text,
            name_width + 1.0,
        )?;
        self.draw_svg_icon(
            layers,
            SvgIcon::ChevronRight,
            name_x + name_width + chevron_gap,
            control_y + (control_height - chevron) / 2.0,
            chevron,
            palette.muted_text,
        )?;

        let text_width = (control_x - x - self.ui_px(24.0)).max(0.0);
        self.draw_text(
            layers,
            &ui_font,
            x,
            y,
            &crate::i18n::tr("settings-effective-color-scheme"),
            palette.text,
            text_width,
        )?;
        self.draw_row_description(
            layers,
            x,
            y,
            &crate::i18n::tr("settings-effective-color-scheme-description"),
            text_width,
        )
    }

    /// The app icons to switch between, shown as themselves.
    fn paint_app_icon_choice_row(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
        draw_top_rule: bool,
    ) -> anyhow::Result<f32> {
        let palette = self.palette();
        let ui_font = Rc::clone(&self.ui_font);
        let body_font = Rc::clone(&self.body_font);
        if draw_top_rule {
            self.paint_separator(layers, x, y - self.ui_px(28.0), width)?;
        }
        let side = self.ui_px(88.0).round();
        let gap = self.ui_px(20.0);
        // The artwork keeps the macOS icon grid's margin: 13 of every 128
        // pixels on each side are clear, and the ring hugs what is drawn.
        let inset = (side * 13.0 / 128.0).round();
        let face = side - inset * 2.0;
        let ring = self.ui_px(3.0).max(2.0).round();
        let cell_height = self.metrics.cell_size.height as f32;
        let chosen = self.native_settings.appearance.app_icon;
        let accent = self.chrome_palette.accent;
        let count = NativeAppIcon::ALL.len() as f32;
        let block_x = x + width - side * count - gap * (count - 1.0);
        // The drawn face's top lines up with the label's.
        let icon_y = y - inset;
        let label_y = icon_y + side - inset + self.ui_px(10.0);
        let label_room = side + gap * 0.8;

        for (index, icon) in NativeAppIcon::ALL.iter().copied().enumerate() {
            let icon_x = block_x + (side + gap) * index as f32;
            let action = SettingsAction::SetAppIcon(icon);
            let selected = chosen == icon;
            let hovered = self.ui.interaction.hovered == Some(action)
                || self.ui.interaction.pressed == Some(action);
            if selected || hovered {
                self.draw_rounded_rect(
                    layers,
                    0,
                    icon_x + inset - ring,
                    icon_y + inset - ring,
                    face + ring * 2.0,
                    face + ring * 2.0,
                    if selected {
                        accent
                    } else {
                        accent.mul_alpha(0.4)
                    },
                    (face * 0.2237).round() + ring,
                )?;
            }
            self.draw_app_icon_choice(layers, icon, icon_x, icon_y, side)?;
            let label = localized_app_icon_label(icon);
            let label_width = self.measure_text_width(&body_font, &label).min(label_room);
            self.draw_text(
                layers,
                &body_font,
                icon_x + (side - label_width) / 2.0,
                label_y,
                &label,
                if selected {
                    palette.text
                } else {
                    palette.secondary_text
                },
                label_room,
            )?;
            let top = icon_y + inset - ring;
            self.ui_context.push(
                rect(icon_x, top, side, label_y + cell_height - top),
                WidgetKind::Button,
                action,
            );
        }

        let text_width = (block_x - x - self.ui_px(24.0)).max(0.0);
        self.draw_text(
            layers,
            &ui_font,
            x,
            y,
            &crate::i18n::tr("settings-app-icon"),
            palette.text,
            text_width,
        )?;
        let extra = self.draw_row_description(
            layers,
            x,
            y,
            &crate::i18n::tr("settings-app-icon-description"),
            text_width,
        )?;
        let note_y = self.settings_row_description_y(y) + self.description_line_step() + extra;
        let last_note_y = note_y
            + self.draw_row_note(
                layers,
                x,
                note_y,
                &crate::i18n::tr("settings-app-icon-note"),
                text_width,
            )?;
        let bottom = (last_note_y + cell_height).max(label_y + cell_height) + self.ui_px(6.0);
        Ok((bottom - (y + self.settings_row_visual_height())).max(0.0))
    }

    /// Where each text size applies, as a picture with the parts numbered,
    /// then a row per size carrying the same number, then the weight.
    /// Returns where the last row ends.
    fn paint_typography_rows(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        top: f32,
        width: f32,
    ) -> anyhow::Result<f32> {
        let diagram_bottom = self.paint_typography_diagram(layers, x, top, width)?;
        let band = self.compact_row_step();
        let slot = self.ui_px(ROW_TILE_SIDE);
        let indent = slot + self.ui_px(ROW_TILE_GAP);
        let badge = self.ui_px(ROW_BADGE_SIDE).round();
        let mut band_y = diagram_bottom + self.ui_px(24.0);
        for (index, area) in TYPOGRAPHY_FONT_AREAS.iter().copied().enumerate() {
            // The first rule closes off the picture, across the card.
            let rule_x = if index == 0 { x } else { x + indent };
            self.paint_separator(layers, rule_x, band_y, x + width - rule_x)?;
            self.paint_number_badge(
                layers,
                x + ((slot - badge) / 2.0).round(),
                band_y + ((band - badge) / 2.0).round(),
                badge,
                index + 1,
            )?;
            self.paint_compact_stepper_row(
                layers,
                x + indent,
                band_y,
                width - indent,
                band,
                &area.localized_label(),
                area.description_key(),
                self.current_chrome_font_size_value(area),
                SettingsAction::ResetChromeFontSize(area),
                SettingsAction::DecreaseChromeFontSize(area),
                SettingsAction::IncreaseChromeFontSize(area),
            )?;
            band_y += band;
        }
        self.paint_separator(layers, x + indent, band_y, width - indent)?;
        let (tx, tw) = self.paint_band_tile(
            layers,
            x,
            band_y,
            band,
            width,
            SvgIcon::Bold,
            TileColor::Indigo,
        )?;
        self.paint_compact_stepper_row(
            layers,
            tx,
            band_y,
            tw,
            band,
            &crate::i18n::tr("settings-font-weight"),
            "settings-font-weight-description",
            self.current_settings_font_weight_value(),
            SettingsAction::ResetSettingsFontWeight,
            SettingsAction::DecreaseSettingsFontWeight,
            SettingsAction::IncreaseSettingsFontWeight,
        )?;
        band_y += band;
        self.paint_separator(layers, x + indent, band_y, width - indent)?;
        let (tx, tw) = self.paint_band_tile(
            layers,
            x,
            band_y,
            band,
            width,
            SvgIcon::Type,
            TileColor::Indigo,
        )?;
        self.paint_ui_font_row(layers, tx, band_y, tw, band)?;
        band_y += band;
        Ok(band_y)
    }

    /// The family the interface is drawn with, as a dropdown in a band of
    /// the typography card.
    fn paint_ui_font_row(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
        band: f32,
    ) -> anyhow::Result<()> {
        let palette = self.palette();
        let control_height = self.ui_px(CONTROL_HEIGHT);
        let control_y = y + ((band - control_height) / 2.0).max(0.0);
        let control_width = self.settings_control_width(width);
        let control_x = x + width - control_width;
        self.note_dropdown_anchor(
            SettingsDropdown::UiFont,
            (control_x, control_y, control_width),
        );
        let action = SettingsAction::ToggleUiFontMenu;
        // A family's name can be longer than the column is wide.
        let dropdown_label = self.text_with_ellipsis(
            &Rc::clone(&self.ui_font),
            &self.current_ui_font_label(),
            control_width - self.ui_px(68.0),
        );
        let control_rect =
            self.dropdown_pill_rect(control_x, control_y, control_width, &dropdown_label);
        let open = self.ui.open_dropdown == Some(SettingsDropdown::UiFont);
        let hovered = self.ui.interaction.hovered == Some(action);
        let pressed = self.ui.interaction.pressed == Some(action);
        let bg = if pressed || hovered {
            palette.control_hover_bg
        } else {
            palette.control_bg
        };
        let border = if open {
            palette.nav_selected_bg
        } else if hovered || pressed {
            palette.separator
        } else {
            palette.control_border
        };
        self.ui_context
            .push(control_rect, WidgetKind::Button, action);
        let label_y = self.control_text_y(control_y, control_height);
        let text_width = (control_rect.origin.x
            - x
            - self.ui_px(24.0)
            - self.ui_px(HINT_ICON_SIDE + HINT_ICON_GAP))
        .max(0.0);
        self.paint_hinted_label(
            layers,
            x,
            label_y,
            text_width,
            &crate::i18n::tr("settings-ui-font"),
            "settings-ui-font-description",
        )?;
        self.paint_dropdown_pill(layers, control_rect, &dropdown_label, bg, border)
    }

    /// The main window and the Settings window drawn small, each part a
    /// text size applies to numbered like its row. The part whose row is
    /// under the pointer lights up. Returns where the picture ends.
    fn paint_typography_diagram(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
    ) -> anyhow::Result<f32> {
        let colors = self.chrome_palette;
        let ground = opaque(colors.window_bg);
        let sidebar_ground = opaque(colors.workspace_sidebar_bg);
        let rule = colors.control_border;
        let line = self.ui_px(2.0).max(1.0).round();
        let height = self.ui_px(260.0).round();
        let radius = self.ui_px(16.0).round();
        let gap = self.ui_px(24.0);
        let badge = self.ui_px(DIAGRAM_BADGE_SIDE).round();
        let settings_width = self.ui_px(210.0).min(width * 0.25).round();
        let main_width = (width - settings_width - gap).round();
        let sidebar_width = (main_width * 0.2).round();
        let right_width = (main_width * 0.2).round();
        let middle_x = x + sidebar_width;
        let middle_width = main_width - sidebar_width - right_width;
        let right_x = x + main_width - right_width;
        let tabs_height = self.ui_px(54.0).round();
        let header_height = self.ui_px(50.0).round();
        let header_y = y + tabs_height;
        let content_y = header_y + header_height;
        let settings_x = x + main_width + gap;
        let lit = self.hovered_font_area();
        let centre = |left: f32, top: f32, w: f32, h: f32| {
            (
                (left + (w - badge) / 2.0).round(),
                (top + (h - badge) / 2.0).round(),
            )
        };

        // The main window: workspace sidebar, tabs over a pane's header
        // and content, right sidebar.
        let window = (x, y, main_width, height, radius);
        self.paint_window_shape(layers, window, sidebar_width, ground, sidebar_ground)?;
        let parts = [
            (ChromeFontArea::Sidebar, (x, y, sidebar_width, height)),
            (
                ChromeFontArea::TabBar,
                (middle_x, y, middle_width, tabs_height),
            ),
            (
                ChromeFontArea::PaneHeader,
                (middle_x, header_y, middle_width, header_height),
            ),
            (
                ChromeFontArea::Home,
                (middle_x, content_y, middle_width, y + height - content_y),
            ),
            (
                ChromeFontArea::RightSidebar,
                (right_x, y, right_width, height),
            ),
        ];
        for (area, part) in parts {
            if lit == Some(area) {
                self.paint_lit_part(layers, window, part)?;
            }
        }
        self.draw_rect(layers, 0, middle_x, y, line, height, rule)?;
        self.draw_rect(layers, 0, right_x, y, line, height, rule)?;
        self.draw_rect(layers, 0, middle_x, header_y, middle_width, line, rule)?;
        self.draw_rect(layers, 0, middle_x, content_y, middle_width, line, rule)?;
        self.draw_rounded_frame(
            layers,
            0,
            x,
            y,
            main_width,
            height,
            LinearRgba::TRANSPARENT,
            rule,
            radius,
        )?;

        // Two tabs after the tab bar's number.
        let pad = self.ui_px(14.0);
        let tab_width = self.ui_px(64.0).min(middle_width * 0.25).round();
        let tab_height = self.ui_px(22.0).round();
        for index in 0..2 {
            self.draw_rounded_rect(
                layers,
                0,
                middle_x + pad * 2.0 + badge + (tab_width + pad * 0.6) * index as f32,
                y + ((tabs_height - tab_height) / 2.0).round(),
                tab_width,
                tab_height,
                opaque(colors.control_bg),
                tab_height / 2.0,
            )?;
        }

        // The Settings window beside it, its own sidebar down the left.
        let settings_window = (settings_x, y, settings_width, height, radius);
        let settings_nav = (settings_width * 0.3).round();
        self.paint_window_shape(
            layers,
            settings_window,
            settings_nav,
            ground,
            sidebar_ground,
        )?;
        if lit == Some(ChromeFontArea::Settings) {
            self.paint_lit_part(
                layers,
                settings_window,
                (settings_x, y, settings_width, height),
            )?;
        }
        self.draw_rect(layers, 0, settings_x + settings_nav, y, line, height, rule)?;
        self.draw_rounded_frame(
            layers,
            0,
            settings_x,
            y,
            settings_width,
            height,
            LinearRgba::TRANSPARENT,
            rule,
            radius,
        )?;

        let badges = [
            (
                ChromeFontArea::Settings,
                centre(
                    settings_x + settings_nav,
                    y,
                    settings_width - settings_nav,
                    height,
                ),
            ),
            (
                ChromeFontArea::Home,
                centre(middle_x, content_y, middle_width, y + height - content_y),
            ),
            (
                ChromeFontArea::RightSidebar,
                centre(right_x, y, right_width, height),
            ),
            (ChromeFontArea::Sidebar, centre(x, y, sidebar_width, height)),
            (
                ChromeFontArea::TabBar,
                (
                    (middle_x + pad).round(),
                    (y + (tabs_height - badge) / 2.0).round(),
                ),
            ),
            (
                ChromeFontArea::PaneHeader,
                centre(middle_x, header_y, middle_width, header_height),
            ),
        ];
        for (area, (badge_x, badge_y)) in badges {
            let number = TYPOGRAPHY_FONT_AREAS
                .iter()
                .position(|listed| *listed == area)
                .map_or(0, |index| index + 1);
            self.paint_number_badge(layers, badge_x, badge_y, badge, number)?;
        }
        Ok(y + height)
    }

    /// Tint one part of a window drawn by `paint_typography_diagram`: the
    /// window's rounded shape, cut down to the part, so a part on the
    /// window's edge keeps its corner.
    fn paint_lit_part(
        &self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        window: (f32, f32, f32, f32, f32),
        part: (f32, f32, f32, f32),
    ) -> anyhow::Result<()> {
        let (x, y, width, height, radius) = window;
        let (left, top, part_width, part_height) = part;
        let mut tint = HeapQuadAllocator::default();
        self.draw_rounded_rect(
            &mut TripleLayerQuadAllocator::Heap(&mut tint),
            0,
            x,
            y,
            width,
            height,
            self.chrome_palette.accent.mul_alpha(0.22),
            radius,
        )?;
        let clip = crate::quad::QuadClipRect::from_top_left_pixels(
            left,
            top,
            left + part_width,
            top + part_height,
            &self.dimensions,
        );
        tint.apply_to_clipped(layers, clip, 1.0)
    }

    /// The text size whose row the pointer is on, if any.
    fn hovered_font_area(&self) -> Option<ChromeFontArea> {
        let action = self
            .ui
            .interaction
            .pressed
            .or(self.ui.interaction.hovered)?;
        match action {
            SettingsAction::ResetChromeFontSize(area)
            | SettingsAction::DecreaseChromeFontSize(area)
            | SettingsAction::IncreaseChromeFontSize(area) => Some(area),
            SettingsAction::Hint(key) => TYPOGRAPHY_FONT_AREAS
                .iter()
                .copied()
                .find(|area| area.description_key() == key),
            _ => None,
        }
    }

    /// A numbered disc in the accent colour, tying a row to the part of
    /// the picture it sizes.
    fn paint_number_badge(
        &self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        side: f32,
        number: usize,
    ) -> anyhow::Result<()> {
        let palette = self.palette();
        let ui_font = Rc::clone(&self.ui_font);
        self.draw_rounded_rect(
            layers,
            0,
            x,
            y,
            side,
            side,
            self.chrome_palette.accent,
            side / 2.0,
        )?;
        let text = number.to_string();
        let text_width = self.measure_text_width(&ui_font, &text);
        self.draw_text(
            layers,
            &ui_font,
            (x + (side - text_width) / 2.0).round(),
            self.control_text_y(y, side),
            &text,
            palette.on_accent,
            side,
        )
    }

    /// A one-line stepper row in a band `band` tall: the label with its
    /// explanation behind an ⓘ, then reset and − value + on the right.
    fn paint_compact_stepper_row(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
        band: f32,
        label: &str,
        hint_key: &'static str,
        value: f64,
        reset_action: SettingsAction,
        decrease_action: SettingsAction,
        increase_action: SettingsAction,
    ) -> anyhow::Result<()> {
        let control_height = self.ui_px(CONTROL_HEIGHT);
        let control_y = y + ((band - control_height) / 2.0).max(0.0);
        let reset_x = self.paint_font_size_stepper(
            layers,
            x,
            control_y,
            width,
            value,
            None,
            reset_action,
            decrease_action,
            increase_action,
        )?;
        let label_y = self.control_text_y(control_y, control_height);
        let text_width =
            (reset_x - x - self.ui_px(24.0) - self.ui_px(HINT_ICON_SIDE + HINT_ICON_GAP)).max(0.0);
        self.paint_hinted_label(layers, x, label_y, text_width, label, hint_key)
    }

    /// Browser access: whether the mux server is accepting browser clients,
    /// and the links that let one in.
    ///
    /// Two cards, because they answer different questions: the first is a
    /// switch and its state, the second is a list of credentials that
    /// outlive any one session. Putting a revoke button in the same card as
    /// the on/off switch read as though it were part of turning it off.
    ///
    /// Everything here is the *server's* state, not this window's, so it is
    /// read over the mux client and cached in `web_settings`; painting only
    /// ever reads that cache.
    fn paint_web(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        max_width: f32,
    ) -> anyhow::Result<()> {
        let palette = self.palette();
        let body_font = Rc::clone(&self.body_font);
        let scroll = self.ui.content_scroll.offset;
        let section_y = self.ui_px(CONTENT_SECTION_Y) - scroll;

        let state = crate::web_settings::state();
        // Asked here rather than in `enter_section`, which has no window to
        // wake when the answer lands. One request: `refresh` refuses to
        // start a second while one is out.
        if let Some(window) = self.window.clone() {
            // The first read, and then every couple of seconds for as long
            // as this section is the one on screen: the live-connection
            // count is the server's news, not this window's, so nothing
            // else would ever bring it up to date.
            if !state.loaded || crate::web_settings::poll_is_due() {
                crate::web_settings::refresh(window.clone());
            }
            crate::web_settings::arm_poll(window);
        }
        let listening = state
            .status
            .as_ref()
            .is_some_and(|status| !status.listening.is_empty());
        let address = state
            .status
            .as_ref()
            .and_then(|status| status.listening.first().cloned());

        // Snapshotted for the row actions, exactly as the archived rows are.
        self.ui.web_tokens = state.tokens.clone();

        let card_padding = self.ui_px(36.0);
        let row_x = x + card_padding;
        let row_width = max_width - card_padding * 2.0;

        self.draw_text(
            layers,
            &body_font,
            x,
            section_y,
            &crate::i18n::tr("settings-web-heading"),
            palette.muted_text,
            max_width,
        )?;

        // Every address the listener answers on, as the server lists them
        // (the ones another device can use first).
        let urls = state
            .status
            .as_ref()
            .map(|status| {
                status
                    .urls
                    .iter()
                    .map(|u| u.trim_end_matches('/').to_string())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        // The error takes the description slot rather than a line of its
        // own: it is always about the thing the toggle just tried to do,
        // and a row that appears and disappears moves everything below it.
        // Everything in this section is the server's. Where this platform
        // runs no local session host, that server is whichever domain is
        // attached -- so the text names it rather than saying "this
        // computer", which is what the switch would otherwise be read as.
        // Whole sentences per case rather than a prefix glued on:
        // translations are not built by concatenation.
        let description = match (&state.error, &state.elsewhere, &address) {
            (Some(error), _, _) => error.clone(),
            (None, elsewhere, address) => {
                let mut args = FluentArgs::new();
                if let Some(host) = elsewhere {
                    args.set("host", host.clone());
                }
                match (elsewhere, address) {
                    (Some(_), Some(address)) => {
                        args.set(
                            "address",
                            if urls.is_empty() {
                                address.clone()
                            } else {
                                urls.join("  ·  ")
                            },
                        );
                        crate::i18n::tr_args("settings-web-elsewhere-on-at", &args)
                    }
                    (Some(_), None) => crate::i18n::tr_args("settings-web-elsewhere-off", &args),
                    (None, Some(address)) => {
                        args.set(
                            "address",
                            if urls.is_empty() {
                                address.clone()
                            } else {
                                urls.join("  ·  ")
                            },
                        );
                        crate::i18n::tr_args("settings-web-on-at", &args)
                    }
                    (None, None) => crate::i18n::tr("settings-web-enable-description"),
                }
            }
        };
        let shown_address = urls.first().cloned().or(address);
        let copy_label = crate::i18n::tr(if state.copied {
            "settings-web-copied"
        } else {
            "settings-web-copy"
        });

        let (card_y, _) = self.settings_card_geometry(section_y, 0);
        // What "reachable from other devices" warns about is in the third
        // line of its explanation.
        let layout = CardLayout {
            description_lines: 3,
            ..self.card_layout()
        };
        let bottom =
            self.paint_card_as(layers, x, card_y, max_width, layout, |this, layers, top| {
                let diagram_bottom = this.paint_web_diagram(
                    layers,
                    row_x,
                    top,
                    row_width,
                    listening,
                    state.elsewhere.as_deref(),
                    shown_address.as_deref(),
                )?;
                let rule_y = diagram_bottom + this.ui_px(24.0);
                this.paint_separator(layers, row_x, rule_y, row_width)?;
                let mut rows = RowCursor::new(rule_y + this.ui_px(28.0), this);

                let (tx, tw) = this.paint_row_tile(
                    layers,
                    row_x,
                    rows.y,
                    row_width,
                    SvgIcon::Globe,
                    TileColor::Blue,
                )?;
                rows.add(this.paint_toggle_setting_row(
                    layers,
                    tx,
                    rows.y,
                    tw,
                    &crate::i18n::tr("settings-web-enable"),
                    &description,
                    listening,
                    SettingsAction::ToggleWebServer,
                    rows.rule(),
                )?);
                let (tx, tw) = this.paint_row_tile(
                    layers,
                    row_x,
                    rows.y,
                    row_width,
                    SvgIcon::Wifi,
                    TileColor::Teal,
                )?;
                rows.add(this.paint_toggle_setting_row(
                    layers,
                    tx,
                    rows.y,
                    tw,
                    &crate::i18n::tr("settings-web-reachable"),
                    &crate::i18n::tr("settings-web-reachable-description"),
                    this.native_settings.web.reachable,
                    SettingsAction::ToggleWebReachable,
                    rows.rule(),
                )?);
                let (tx, tw) = this.paint_row_tile(
                    layers,
                    row_x,
                    rows.y,
                    row_width,
                    SvgIcon::Timer,
                    TileColor::Orange,
                )?;
                rows.add(this.paint_web_link_ttl_row(layers, tx, rows.y, tw, rows.rule())?);
                let (tx, tw) = this.paint_row_tile(
                    layers,
                    row_x,
                    rows.y,
                    row_width,
                    SvgIcon::Link2,
                    TileColor::Indigo,
                )?;
                rows.add(this.paint_action_setting_row(
                    layers,
                    tx,
                    rows.y,
                    tw,
                    &crate::i18n::tr("settings-web-link"),
                    &crate::i18n::tr("settings-web-link-description"),
                    &copy_label,
                    SettingsAction::CopyWebLink,
                    rows.rule(),
                )?);
                let (tx, tw) = this.paint_row_tile(
                    layers,
                    row_x,
                    rows.y,
                    row_width,
                    SvgIcon::QrCode,
                    TileColor::Gray,
                )?;
                rows.add(this.paint_web_qr_row(layers, tx, rows.y, tw, &state, rows.rule())?);
                Ok(rows.bottom)
            })?;

        let card_y = self.paint_card_heading(
            layers,
            x,
            bottom,
            max_width,
            &crate::i18n::tr("settings-web-links-heading"),
        )?;
        let bottom = self.paint_card(layers, x, card_y, max_width, |this, layers, top| {
            if state.tokens.is_empty() {
                return this.paint_web_links_empty(layers, row_x, top, row_width);
            }
            let mut rows = RowCursor::new(top, this);
            for token in state.tokens.iter() {
                let (tx, tw) = this.paint_row_tile(
                    layers,
                    row_x,
                    rows.y,
                    row_width,
                    SvgIcon::Link2,
                    TileColor::Blue,
                )?;
                rows.add(this.paint_web_token_row(layers, tx, rows.y, tw, token, rows.rule())?);
            }
            let (tx, tw) = this.paint_row_tile(
                layers,
                row_x,
                rows.y,
                row_width,
                SvgIcon::Unlink2,
                TileColor::Red,
            )?;
            rows.add(this.paint_action_setting_row(
                layers,
                tx,
                rows.y,
                tw,
                &crate::i18n::tr("settings-web-revoke-all-label"),
                &crate::i18n::tr("settings-web-revoke-all-description"),
                &crate::i18n::tr("settings-web-revoke-all"),
                SettingsAction::RevokeAllWebTokens,
                rows.rule(),
            )?);
            Ok(rows.bottom)
        })?;

        self.ui.content_scroll.set_extents(
            self.content_viewport_extent(),
            self.settings_content_extent(bottom + scroll),
        );
        Ok(())
    }

    /// Who can reach these terminals right now, drawn: this computer (or
    /// the server the section is about) on the left, a browser on the
    /// right, and between them a line that is solid while the server
    /// listens and broken while it does not. The right side says whether
    /// only this computer's browser can come in or other devices too.
    /// Returns where the picture ends.
    fn paint_web_diagram(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
        listening: bool,
        elsewhere: Option<&str>,
        address: Option<&str>,
    ) -> anyhow::Result<f32> {
        let palette = self.palette();
        let body_font = Rc::clone(&self.body_font);
        let accent = self.chrome_palette.accent;
        let cell_height = self.metrics.cell_size.height as f32;
        let tile = self.ui_px(96.0).round();
        let label_gap = self.ui_px(14.0);
        let reachable = self.native_settings.web.reachable;

        let left_centre = x + width * 0.25;
        let right_centre = x + width * 0.75;
        let tile_y = y + self.ui_px(6.0);
        let (host_icon, host_label) = match elsewhere {
            Some(host) => (SvgIcon::Server, host.to_string()),
            None => (
                SvgIcon::Laptop,
                crate::i18n::tr("settings-web-diagram-this-computer"),
            ),
        };
        let (guest_icon, guest_label) = if reachable {
            (
                SvgIcon::Smartphone,
                crate::i18n::tr("settings-web-diagram-any-device"),
            )
        } else {
            (
                SvgIcon::Globe,
                crate::i18n::tr("settings-web-diagram-this-browser"),
            )
        };
        let label_room = (width * 0.5 - self.ui_px(24.0)).max(0.0);
        for (centre, icon, color, label) in [
            (left_centre, host_icon, TileColor::Slate, host_label),
            (
                right_centre,
                guest_icon,
                if listening {
                    TileColor::Blue
                } else {
                    TileColor::Gray
                },
                guest_label,
            ),
        ] {
            self.paint_tile(
                layers,
                (centre - tile / 2.0).round(),
                tile_y,
                tile,
                icon,
                color,
            )?;
            let shown = self.text_with_ellipsis(&body_font, &label, label_room);
            let label_width = self.measure_text_width(&body_font, &shown);
            self.draw_text(
                layers,
                &body_font,
                (centre - label_width / 2.0).round(),
                tile_y + tile + label_gap,
                &shown,
                palette.secondary_text,
                label_room,
            )?;
        }

        // The link between them, and what it is.
        let gap = self.ui_px(28.0);
        let line_left = left_centre + tile / 2.0 + gap;
        let line_right = right_centre - tile / 2.0 - gap;
        let line_y = (tile_y + tile / 2.0).round();
        let thickness = self.ui_px(4.0).max(2.0).round();
        let status_y = line_y + self.ui_px(18.0);
        let status_room = (line_right - line_left).max(0.0);
        if line_right > line_left {
            let chevron = self.ui_px(28.0);
            if listening {
                self.draw_rounded_rect(
                    layers,
                    0,
                    line_left,
                    line_y - thickness / 2.0,
                    line_right - line_left - chevron * 0.4,
                    thickness,
                    accent,
                    thickness / 2.0,
                )?;
                self.draw_svg_icon(
                    layers,
                    SvgIcon::ChevronRight,
                    line_right - chevron * 0.75,
                    line_y - chevron / 2.0,
                    chevron,
                    accent,
                )?;
            } else {
                // Dashes either side of a cross in the middle.
                let cross = self.ui_px(26.0);
                let middle = (line_left + line_right) / 2.0;
                let dash = self.ui_px(14.0);
                let space = self.ui_px(10.0);
                let color = palette.muted_text.mul_alpha(0.7);
                for (from, to) in [(line_left, middle - cross), (middle + cross, line_right)] {
                    let mut at = from;
                    while at < to {
                        let length = dash.min(to - at);
                        self.draw_rounded_rect(
                            layers,
                            0,
                            at,
                            line_y - thickness / 2.0,
                            length,
                            thickness,
                            color,
                            thickness / 2.0,
                        )?;
                        at += dash + space;
                    }
                }
                self.draw_svg_icon(
                    layers,
                    SvgIcon::X,
                    middle - cross / 2.0,
                    line_y - cross / 2.0,
                    cross,
                    palette.muted_text,
                )?;
            }
            let status = match (listening, address) {
                (true, Some(address)) => address.to_string(),
                (true, None) => String::new(),
                (false, _) => crate::i18n::tr("settings-web-diagram-off"),
            };
            if !status.is_empty() {
                let shown = self.text_with_ellipsis(&body_font, &status, status_room);
                let status_width = self.measure_text_width(&body_font, &shown);
                self.draw_text(
                    layers,
                    &body_font,
                    ((line_left + line_right - status_width) / 2.0).round(),
                    status_y,
                    &shown,
                    if listening {
                        palette.text
                    } else {
                        palette.muted_text
                    },
                    status_room,
                )?;
            }
        }
        Ok((tile_y + tile + label_gap + cell_height).max(status_y + cell_height))
    }

    /// The row that shows the link as a code for a phone, with the code
    /// under it while shown, and the certificate fingerprints to compare
    /// under that.
    fn paint_web_qr_row(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
        state: &crate::web_settings::WebState,
        draw_top_rule: bool,
    ) -> anyhow::Result<f32> {
        let palette = self.palette();
        let body_font = Rc::clone(&self.body_font);
        let label = crate::i18n::tr(if state.qr.is_some() {
            "settings-web-qr-hide"
        } else {
            "settings-web-qr-show"
        });
        let extra = self.paint_action_setting_row(
            layers,
            x,
            y,
            width,
            &crate::i18n::tr("settings-web-qr"),
            &crate::i18n::tr("settings-web-qr-description"),
            &label,
            SettingsAction::ToggleWebQr,
            draw_top_rule,
        )?;
        let cell_height = self.metrics.cell_size.height as f32;
        let mut bottom = self.settings_row_description_y(y) + extra + cell_height;

        if let Some(rows) = &state.qr {
            // White behind, dark modules on it, a quiet zone of four modules:
            // a scanner wants the contrast, whatever the theme.
            let scale = self.ui_px(4.0);
            let side = rows.len() as f32 * scale;
            let quiet = scale * 4.0;
            let top = bottom + self.ui_px(24.0) + quiet;
            let left = x + quiet;
            let white = LinearRgba::with_components(1.0, 1.0, 1.0, 1.0);
            let black = LinearRgba::with_components(0.0, 0.0, 0.0, 1.0);
            self.draw_rect(
                layers,
                0,
                left - quiet,
                top - quiet,
                side + quiet * 2.0,
                side + quiet * 2.0,
                white,
            )?;
            for (row_index, row) in rows.iter().enumerate() {
                let mut run: Option<usize> = None;
                for column in 0..=row.len() {
                    let dark = column < row.len() && row[column];
                    match (dark, run) {
                        (true, None) => run = Some(column),
                        (false, Some(start)) => {
                            self.draw_rect(
                                layers,
                                0,
                                left + start as f32 * scale,
                                top + row_index as f32 * scale,
                                (column - start) as f32 * scale,
                                scale,
                                black,
                            )?;
                            run = None;
                        }
                        _ => {}
                    }
                }
            }
            bottom = top + side + quiet;
        }

        let certificates = state.displayed_certificates();
        if !certificates.is_empty() {
            let mut lines = vec![crate::i18n::tr("settings-web-verify-certificate")];
            for certificate in certificates {
                lines.push(certificate.urls.join(" · "));
                lines.push("SHA-256:".to_string());
                let groups: Vec<_> = certificate.sha256.split(':').collect();
                for chunk in groups.chunks(8) {
                    lines.push(chunk.join(":"));
                }
            }
            let top = bottom + self.ui_px(20.0);
            bottom = top
                + self.draw_text_lines(layers, &body_font, x, top, &lines, palette.text, width)?
                + cell_height;
        }
        Ok((bottom + self.ui_px(6.0) - (y + self.settings_row_visual_height())).max(extra))
    }

    /// One link handed out: what it is, when it expires and how many are
    /// using it, and a button to revoke it.
    fn paint_web_token_row(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
        token: &codec::WebTokenInfo,
        draw_top_rule: bool,
    ) -> anyhow::Result<f32> {
        // What this link is, in order of how much it actually tells you:
        // the name a person gave it, then what the browser that used it
        // said it was, then the honest admission that nobody has used it
        // yet. The label used to be stamped "Settings" for every link
        // minted here, so the list was a column of identical rows -- a
        // name that names nothing is worse than no name.
        let headline = token
            .label
            .clone()
            .or_else(|| token.last_device.clone())
            .unwrap_or_else(|| crate::i18n::tr("settings-web-token-unused"));
        let mut args = FluentArgs::new();
        args.set(
            "expires",
            match token.expires_at {
                Some(at) => format_web_token_when(at),
                None => crate::i18n::tr("settings-web-token-never"),
            },
        );
        args.set("connections", token.live_connections as i64);
        self.paint_action_setting_row(
            layers,
            x,
            y,
            width,
            &headline,
            &crate::i18n::tr_args("settings-web-token-meta", &args),
            &crate::i18n::tr("settings-web-revoke"),
            SettingsAction::RevokeWebToken(web_token_key(&token.id)),
            draw_top_rule,
        )
    }

    /// The links card with nothing in it: the mark and the sentence,
    /// centred. Returns where it ends.
    fn paint_web_links_empty(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
    ) -> anyhow::Result<f32> {
        let palette = self.palette();
        let body_font = Rc::clone(&self.body_font);
        let mark = self.ui_px(44.0).round();
        self.draw_svg_icon(
            layers,
            SvgIcon::Link2,
            (x + (width - mark) / 2.0).round(),
            y,
            mark,
            palette.muted_text,
        )?;
        let text = crate::i18n::tr("settings-web-tokens-empty");
        let text_width = self.measure_text_width(&body_font, &text).min(width);
        let text_y = y + mark + self.ui_px(14.0);
        self.draw_text(
            layers,
            &body_font,
            (x + (width - text_width) / 2.0).round(),
            text_y,
            &text,
            palette.muted_text,
            width,
        )?;
        Ok(text_y + self.metrics.cell_size.height as f32)
    }

    fn paint_archived(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        max_width: f32,
    ) -> anyhow::Result<()> {
        let palette = self.palette();
        let ui_font = Rc::clone(&self.ui_font);
        let body_font = Rc::clone(&self.body_font);
        let scroll = self.ui.content_scroll.offset;
        let section_y = self.ui_px(CONTENT_SECTION_Y) - scroll;

        // Snapshot the rows once per paint; the row-action indices the
        // buttons register resolve against exactly this list.
        let rows = crate::workspace_threads::archived_projects_overview();
        if self
            .ui
            .confirm_delete_archived
            .as_ref()
            .is_some_and(|id| !rows.iter().any(|row| &row.id == id))
        {
            self.ui.confirm_delete_archived = None;
        }
        self.ui.archived_rows = rows.clone();

        self.draw_text(
            layers,
            &body_font,
            x,
            section_y,
            &crate::i18n::tr("settings-archived-heading"),
            palette.muted_text,
            max_width,
        )?;

        let row_count = rows.len().max(1);
        let (card_y, first_row_y) = self.settings_card_geometry(section_y, row_count);
        let card_height = self.settings_card_height(row_count);
        self.ui.content_scroll.set_extents(
            self.content_viewport_extent(),
            self.settings_content_extent(card_y + scroll + card_height),
        );
        self.paint_group_card(layers, x, card_y, max_width, card_height)?;

        let padding = 36.0;
        let row_x = x + padding;
        let row_width = max_width - padding * 2.0;
        let row_step = self.settings_row_step();

        if rows.is_empty() {
            let tile = self.settings_row_description_y(first_row_y) - first_row_y
                + self.metrics.cell_size.height as f32;
            let icon_gap = self.ui_px(12.0);
            self.draw_rounded_frame(
                layers,
                0,
                row_x,
                first_row_y,
                tile,
                tile,
                palette.control_bg,
                palette.rule,
                self.ui_px(8.0),
            )?;
            let mark = tile * 0.62;
            self.draw_svg_icon(
                layers,
                SvgIcon::Archive,
                row_x + (tile - mark) / 2.0,
                first_row_y + (tile - mark) / 2.0,
                mark,
                palette.muted_text,
            )?;
            self.draw_text(
                layers,
                &body_font,
                row_x + tile + icon_gap,
                first_row_y,
                &crate::i18n::tr("settings-archived-empty"),
                palette.muted_text,
                row_width - tile - icon_gap,
            )?;
            return Ok(());
        }

        let delete_label = crate::i18n::tr("settings-archived-delete");
        let confirm_label = crate::i18n::tr("settings-archived-delete-confirm");
        let unarchive_label = crate::i18n::tr("settings-archived-unarchive");
        // Wider than the usual control gap: these two do opposite things,
        // and the destructive one must not read as adjacent-by-accident.
        let button_gap = self.ui_px(18.0);
        let unarchive_width = self.button_width_for_label(&unarchive_label, 0.0);

        for (index, row) in rows.iter().enumerate() {
            let y = first_row_y + row_step * index as f32;
            // Same two-line shape as every other settings row: name on top,
            // muted provenance underneath, controls right-aligned.
            if index > 0 {
                self.paint_separator(layers, row_x, y - self.ui_px(28.0), row_width)?;
            }
            // Each button is only as wide as the words it is showing --
            // reserving room for the confirm label would leave "Delete"
            // rattling around in an oversized frame. The group is anchored
            // by its right edge, so arming grows the button leftward and
            // that movement is itself the state feedback.
            let delete_is_armed = self.ui.confirm_delete_archived.as_deref() == Some(&row.id);
            let delete_label = if delete_is_armed {
                &confirm_label
            } else {
                &delete_label
            };
            let delete_width = self.button_width_for_label(delete_label, 0.0);
            let controls_x = row_x + row_width - (unarchive_width + button_gap + delete_width);
            // Same leading-mark treatment as the Integrations rows: a
            // rounded tile spanning the two-line block with the glyph
            // centered at 62%. A bare icon sized to one text line reads as
            // a speck beside a row this tall.
            let tile =
                self.settings_row_description_y(y) - y + self.metrics.cell_size.height as f32;
            let icon_gap = self.ui_px(12.0);
            self.draw_rounded_frame(
                layers,
                0,
                row_x,
                y,
                tile,
                tile,
                palette.control_bg,
                palette.rule,
                self.ui_px(8.0),
            )?;
            let mark = tile * 0.62;
            self.draw_svg_icon(
                layers,
                if row.is_remote {
                    SvgIcon::Server
                } else {
                    SvgIcon::Archive
                },
                row_x + (tile - mark) / 2.0,
                y + (tile - mark) / 2.0,
                mark,
                palette.text,
            )?;
            let text_x = row_x + tile + icon_gap;
            let text_width = (controls_x - text_x - self.ui_px(24.0)).max(row_width * 0.3);

            self.draw_text(
                layers,
                &ui_font,
                text_x,
                y,
                &row.name,
                palette.text,
                text_width,
            )?;

            let mut meta_args = FluentArgs::new();
            meta_args.set("space", row.space_name.clone());
            meta_args.set("threads", row.thread_count as i64);
            meta_args.set("when", format_archived_when(row.archived_at));
            self.draw_text(
                layers,
                &body_font,
                text_x,
                self.settings_row_description_y(y),
                &crate::i18n::tr_args("settings-archived-meta", &meta_args),
                palette.secondary_text,
                text_width,
            )?;

            // Centered on the two-line block, not top-aligned to the first
            // line: the tile defines the row's visual height here, and a
            // top-biased control reads as misaligned against it.
            let control_y = y + (tile - self.ui_px(CONTROL_HEIGHT)) / 2.0;
            self.draw_button(
                layers,
                controls_x,
                control_y,
                unarchive_width,
                &unarchive_label,
                SettingsAction::UnarchiveArchivedRow(index),
            )?;
            self.draw_button(
                layers,
                controls_x + unarchive_width + button_gap,
                control_y,
                delete_width,
                delete_label,
                SettingsAction::DeleteArchivedRow(index),
            )?;
        }
        Ok(())
    }

    fn paint_workspaces(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        max_width: f32,
    ) -> anyhow::Result<()> {
        let palette = self.palette();
        let body_font = Rc::clone(&self.body_font);
        let scroll = self.ui.content_scroll.offset;
        let section_y = self.ui_px(CONTENT_SECTION_Y) - scroll;
        let card_padding = self.ui_px(36.0);
        let row_x = x + card_padding;
        let row_width = max_width - card_padding * 2.0;

        self.draw_text(
            layers,
            &body_font,
            x,
            section_y,
            &crate::i18n::tr("settings-workspaces-heading"),
            palette.muted_text,
            max_width,
        )?;
        let minutes = self.current_remote_sftp_idle_minutes();
        let mut minutes_args = FluentArgs::new();
        minutes_args.set("count", minutes);
        let minutes_label = crate::i18n::tr_args("common-minutes", &minutes_args);
        let download_folder = self.remote_download_directory_label();
        let remote_drop_value = self.ui.remote_drop_input.text().to_string();
        let (card_y, _) = self.settings_card_geometry(section_y, 0);
        let bottom = self.paint_card(layers, x, card_y, max_width, |this, layers, top| {
            let mut rows = RowCursor::new(top, this);
            let (tx, tw) = this.paint_row_tile(
                layers,
                row_x,
                rows.y,
                row_width,
                SvgIcon::Timer,
                TileColor::Teal,
            )?;
            rows.add(this.paint_font_size_stepper_row(
                layers,
                tx,
                rows.y,
                tw,
                &crate::i18n::tr("settings-sftp-idle"),
                &crate::i18n::tr("settings-sftp-idle-description"),
                minutes as f64,
                Some(&minutes_label),
                SettingsAction::ResetRemoteSftpIdle,
                SettingsAction::DecreaseRemoteSftpIdle,
                SettingsAction::IncreaseRemoteSftpIdle,
                rows.rule(),
            )?);
            let (tx, tw) = this.paint_row_tile(
                layers,
                row_x,
                rows.y,
                row_width,
                SvgIcon::Download,
                TileColor::Blue,
            )?;
            rows.add(this.paint_download_folder_row(
                layers,
                tx,
                rows.y,
                tw,
                &download_folder,
                rows.rule(),
            )?);
            let (tx, tw) = this.paint_row_tile(
                layers,
                row_x,
                rows.y,
                row_width,
                SvgIcon::Upload,
                TileColor::Indigo,
            )?;
            rows.add(this.paint_text_setting_row(
                layers,
                tx,
                rows.y,
                tw,
                &crate::i18n::tr("settings-remote-drop-destination"),
                &crate::i18n::tr("settings-remote-drop-destination-description"),
                &remote_drop_value,
                crate::native_settings::DEFAULT_REMOTE_DROP_DESTINATION,
                SettingsAction::RemoteDropDestinationInput,
                rows.rule(),
            )?);
            Ok(rows.bottom)
        })?;

        // Notification sounds are not a remote-files concern, so they get
        // their own card rather than being filed under that heading.
        let card_y = self.paint_card_heading(
            layers,
            x,
            bottom,
            max_width,
            &crate::i18n::tr("settings-notifications-heading"),
        )?;
        let bottom = self.paint_card(layers, x, card_y, max_width, |this, layers, top| {
            let mut rows = RowCursor::new(top, this);
            let (tx, tw) = this.paint_row_tile(
                layers,
                row_x,
                rows.y,
                row_width,
                SvgIcon::Bell,
                TileColor::Red,
            )?;
            rows.add(this.paint_toggle_setting_row(
                layers,
                tx,
                rows.y,
                tw,
                &crate::i18n::tr("settings-notification-sounds"),
                &crate::i18n::tr("settings-notification-sounds-description"),
                this.native_settings.workspaces.notification_sounds_enabled,
                SettingsAction::ToggleNotificationSounds,
                rows.rule(),
            )?);
            Ok(rows.bottom)
        })?;

        self.ui.content_scroll.set_extents(
            self.content_viewport_extent(),
            self.settings_content_extent(bottom + scroll),
        );
        Ok(())
    }

    /// Where files downloaded from a remote session land, with a button to
    /// pick another folder and, once one was picked, one to go back to the
    /// system's.
    fn paint_download_folder_row(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
        folder: &str,
        draw_top_rule: bool,
    ) -> anyhow::Result<f32> {
        let palette = self.palette();
        let ui_font = Rc::clone(&self.ui_font);
        if draw_top_rule {
            self.paint_separator(layers, x, y - self.ui_px(28.0), width)?;
        }
        let control_y = y + self.ui_px(4.0);
        let mut left = x + width;
        let chosen = !self
            .native_settings
            .workspaces
            .remote_download_directory
            .trim()
            .is_empty();
        if chosen {
            let reset = crate::i18n::tr("common-reset");
            let reset_width = self.button_width_for_label(&reset, 110.0);
            left -= reset_width;
            self.draw_button(
                layers,
                left,
                control_y,
                reset_width,
                &reset,
                SettingsAction::ResetRemoteDownloadDirectory,
            )?;
            left -= self.ui_px(12.0);
        }
        let choose = crate::i18n::tr("common-choose");
        let choose_width = self.button_width_for_label(&choose, 110.0);
        left -= choose_width;
        self.draw_button(
            layers,
            left,
            control_y,
            choose_width,
            &choose,
            SettingsAction::ChooseRemoteDownloadDirectory,
        )?;
        let text_width = (left - x - self.ui_px(24.0)).max(0.0);
        self.draw_text(
            layers,
            &ui_font,
            x,
            y,
            &crate::i18n::tr("settings-download-folder"),
            palette.text,
            text_width,
        )?;
        self.draw_row_description(layers, x, y, folder, text_width)
    }

    fn paint_command_palette_section(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        max_width: f32,
    ) -> anyhow::Result<()> {
        let palette = self.palette();
        let body_font = Rc::clone(&self.body_font);
        let scroll = self.ui.content_scroll.offset;
        let section_y = self.ui_px(CONTENT_SECTION_Y) - scroll;
        // Hotkey, rows, font size, group search.
        let row_count = 4;
        let (card_y, first_row_y) = self.settings_card_geometry(section_y, row_count);
        let card_height = self.settings_card_height(row_count);
        self.ui.content_scroll.set_extents(
            self.content_viewport_extent(),
            self.settings_content_extent(card_y + scroll + card_height),
        );

        self.draw_text(
            layers,
            &body_font,
            x,
            section_y,
            &crate::i18n::tr("settings-command-palette-heading"),
            palette.muted_text,
            max_width,
        )?;
        let padding = 36.0;
        let row_x = x + padding;
        let row_width = max_width - padding * 2.0;
        self.paint_group_card(layers, x, card_y, max_width, card_height)?;
        let row_step = self.settings_row_step();

        self.paint_command_palette_hotkey_row(layers, row_x, first_row_y, row_width, false)?;

        let rows = self.native_settings.command_palette.rows;
        let rows_label = if rows == 0 {
            crate::i18n::tr("settings-command-palette-rows-auto")
        } else {
            rows.to_string()
        };
        self.paint_font_size_stepper_row(
            layers,
            row_x,
            first_row_y + row_step,
            row_width,
            &crate::i18n::tr("settings-command-palette-rows"),
            &crate::i18n::tr("settings-command-palette-rows-description"),
            rows as f64,
            Some(&rows_label),
            SettingsAction::ResetCommandPaletteRows,
            SettingsAction::DecreaseCommandPaletteRows,
            SettingsAction::IncreaseCommandPaletteRows,
            true,
        )?;

        let font_size = self.current_command_palette_font_size();
        let mut size_args = FluentArgs::new();
        size_args.set("value", format!("{font_size:.1}"));
        let size_label = crate::i18n::tr_args("common-points", &size_args);
        self.paint_font_size_stepper_row(
            layers,
            row_x,
            first_row_y + row_step * 2.0,
            row_width,
            &crate::i18n::tr("settings-command-palette-font-size"),
            &crate::i18n::tr("settings-command-palette-font-size-description"),
            font_size,
            Some(&size_label),
            SettingsAction::ResetCommandPaletteFontSize,
            SettingsAction::DecreaseCommandPaletteFontSize,
            SettingsAction::IncreaseCommandPaletteFontSize,
            true,
        )?;

        self.paint_toggle_setting_row(
            layers,
            row_x,
            first_row_y + row_step * 3.0,
            row_width,
            &crate::i18n::tr("settings-command-palette-penetration"),
            &crate::i18n::tr("settings-command-palette-penetration-description"),
            self.native_settings.command_palette.search_penetrates_groups,
            SettingsAction::ToggleCommandPaletteSearchPenetration,
            true,
        )?;
        Ok(())
    }

    fn current_command_palette_font_size(&self) -> f64 {
        self.native_settings
            .command_palette
            .font_size
            .unwrap_or_else(|| configuration().command_palette_font_size)
    }

    fn step_command_palette_rows(&mut self, delta: i32) {
        let rows = (self.native_settings.command_palette.rows as i32 + delta).clamp(0, 30) as u32;
        self.native_settings.command_palette.rows = rows;
        self.save_command_palette_settings();
    }

    fn step_command_palette_font_size(&mut self, delta: f64) {
        let value = (self.current_command_palette_font_size() + delta).clamp(8.0, 32.0);
        self.native_settings.command_palette.font_size = Some(value);
        self.save_command_palette_settings();
    }

    /// The same merge-then-write dance as the palette's: another window
    /// may have written a different group while Settings was open.
    fn save_web_settings(&mut self) {
        let mut merged = crate::native_settings::load();
        merged.web = self.native_settings.web.clone();
        self.set_native_settings(merged);
        if let Err(err) = crate::native_settings::save(&self.native_settings) {
            log::error!("failed to save the browser link expiry: {err:#}");
        }
    }

    fn save_command_palette_settings(&mut self) {
        // Merge into the freshest cached settings instead of writing this
        // window's whole clone: the palette writes appearance.color_scheme
        // through its own path while Settings is open, and a stale full
        // write here would silently revert that choice.
        let mut merged = crate::native_settings::load();
        merged.command_palette = self.native_settings.command_palette.clone();
        self.set_native_settings(merged);
        match crate::native_settings::save(&self.native_settings) {
            Ok(()) => {
                // The palette re-reads its settings on every paint; a repaint
                // is all the open windows need.
                if let Some(front_end) = crate::frontend::try_front_end() {
                    front_end.invalidate_all_windows();
                }
                self.status = crate::i18n::tr("settings-status-command-palette-saved");
            }
            Err(err) => {
                self.status = settings_tr(
                    "settings-status-command-palette-error",
                    &[("error", format!("{err:#}"))],
                );
            }
        }
    }

    fn paint_agents(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        max_width: f32,
    ) -> anyhow::Result<()> {
        let palette = self.palette();
        let ui_font = Rc::clone(&self.ui_font);
        let body_font = Rc::clone(&self.body_font);
        let scroll = self.ui.content_scroll.offset;
        let section_y = self.ui_px(CONTENT_SECTION_Y) - scroll;

        // One row per supported agent. Clicking a row expands an inset card
        // explaining how that agent is covered; the outer card grows by the
        // detail block's height. Whether the panel is shown at all is a
        // sidebar question and lives in Settings > Sidebar.
        let integrations_row_count = crate::agent_status::SUPPORTED_AGENTS.len();
        let padding = 36.0;
        let row_x = x + padding;
        let row_width = max_width - padding * 2.0;
        let cell_height = self.metrics.cell_size.height as f32;
        let row_step = self.settings_row_step();
        let detail_line_step = (cell_height + self.ui_px(8.0)).max(self.ui_px(24.0));
        let detail_text_inset = self.ui_px(16.0);
        // Wrapped detail lines for the expanded row, computed up front so
        // the card height is known before anything paints.
        let expanded_detail: Option<Vec<String>> = self.agents_expanded.and_then(|expanded| {
            crate::agent_status::SUPPORTED_AGENTS
                .iter()
                .find(|(id, _, _)| *id == expanded)
                .map(|(id, _, kind)| {
                    agent_detail_lines(id, *kind)
                        .iter()
                        .flat_map(|text| {
                            self.wrap_settings_text(
                                &ui_font,
                                text,
                                row_width - detail_text_inset * 2.0,
                            )
                        })
                        .collect()
                })
        });
        // Where the inset card sits relative to its row's top, and how much
        // taller the row becomes.
        let desc_offset = (cell_height + self.ui_px(8.0)).max(self.ui_px(34.0));
        let detail_block_rel_y = desc_offset + cell_height + self.ui_px(12.0);
        let detail_block_height = expanded_detail
            .as_ref()
            .map(|lines| lines.len() as f32 * detail_line_step + self.ui_px(22.0))
            .unwrap_or(0.0);
        let expanded_extra = expanded_detail
            .as_ref()
            .map(|_| {
                (detail_block_rel_y + detail_block_height + self.ui_px(18.0) - row_step).max(0.0)
            })
            .unwrap_or(0.0);
        let integrations_title_y = section_y;
        let integrations_card_y = integrations_title_y + self.settings_section_card_gap();
        let integrations_first_row_y = integrations_card_y + self.settings_card_top_padding();
        let mut integrations_card_height =
            self.settings_card_height(integrations_row_count) + expanded_extra;
        // The card budgets less than a full row_step for its final row, so
        // an expansion of the *last* row would overhang the card's bottom
        // border; grow the card to contain the block plus breathing room.
        let last_agent_expanded = self.agents_expanded.is_some()
            && self.agents_expanded
                == crate::agent_status::SUPPORTED_AGENTS
                    .last()
                    .map(|(id, _, _)| *id);
        if last_agent_expanded && expanded_detail.is_some() {
            let block_bottom = self.settings_card_top_padding()
                + integrations_row_count.saturating_sub(1) as f32 * row_step
                + detail_block_rel_y
                + detail_block_height;
            integrations_card_height =
                integrations_card_height.max(block_bottom + self.ui_px(18.0));
        }
        self.ui.content_scroll.set_extents(
            self.content_viewport_extent(),
            self.settings_content_extent(integrations_card_y + scroll + integrations_card_height),
        );

        self.draw_text(
            layers,
            &body_font,
            x,
            integrations_title_y,
            &crate::i18n::tr("settings-agents-integrations-heading"),
            palette.muted_text,
            max_width,
        )?;
        self.paint_group_card(
            layers,
            x,
            integrations_card_y,
            max_width,
            integrations_card_height,
        )?;

        // One row per supported agent; every row expands on click with an
        // explanation of how that agent is covered. Nothing here is
        // installable: detection is screen-rule driven for everyone.
        use crate::agent_status::IntegrationKind;
        let mut row_y = integrations_first_row_y;
        let mut draw_top_rule = false;
        let mut prev_expanded = false;
        for (agent_id, _names, kind) in crate::agent_status::SUPPORTED_AGENTS {
            let expanded = self.agents_expanded == Some(*agent_id);
            let arrow = if expanded { "\u{25be}" } else { "\u{25b8}" };
            let label = format!("{arrow}  {}", crate::agent_status::display_name(agent_id));
            let on_path = crate::agent_status::agent_on_path(agent_id);
            let description = match kind {
                IntegrationKind::Native => crate::i18n::tr("settings-integration-native"),
                IntegrationKind::Pending => crate::i18n::tr("settings-integration-pending"),
                IntegrationKind::ScreenRules if on_path => {
                    crate::i18n::tr("settings-integration-screen-active")
                }
                IntegrationKind::ScreenRules => {
                    crate::i18n::tr("settings-integration-screen-missing")
                }
            };
            // No separator right after an expanded row: `row_y` includes
            // the detail block, so the rule would cut across its card; the
            // frame itself already separates.
            if draw_top_rule && !prev_expanded {
                self.paint_separator(layers, row_x, row_y - self.ui_px(28.0), row_width)?;
            }
            // The brand mark leads the row; agents without a logo (and the
            // expand arrow) still line up because the text column starts
            // past a fixed icon slot either way. The mark spans the
            // two-line row block, centered over both lines — sized to one
            // text line it read as a speck beside these tall rows.
            // The mark sits in a rounded tile spanning the two-line row
            // block, macOS-settings style. The tile is what makes the set
            // read as one family: full-bleed marks get breathing room
            // inside it, and white glyphs sit on a defined surface
            // instead of floating on the page background. Sizes derive
            // from the text metrics so every display scale agrees.
            let tile = self.settings_row_description_y(row_y) - row_y
                + self.metrics.cell_size.height as f32;
            let brand_gap = self.ui_px(12.0);
            let text_x = row_x + tile + brand_gap;
            let text_width = row_width - (tile + brand_gap);
            self.draw_rounded_frame(
                layers,
                0,
                row_x,
                row_y,
                tile,
                tile,
                palette.control_bg,
                palette.rule,
                self.ui_px(8.0),
            )?;
            let mark = tile * 0.62;
            let mark_x = row_x + (tile - mark) / 2.0;
            let mark_y = row_y + (tile - mark) / 2.0;
            // effective_appearance, not the raw OS appearance: the theme
            // override decides which Kimi mark is legible here.
            match crate::agent_status::brand_icon(agent_id, self.effective_appearance()) {
                Some(crate::agent_status::AgentIcon::Color(brand)) => {
                    self.draw_brand_icon(layers, brand, mark_x, mark_y, mark)?
                }
                Some(crate::agent_status::AgentIcon::Mono(icon)) => {
                    self.draw_svg_icon(layers, icon, mark_x, mark_y, mark, palette.text)?
                }
                None => {
                    self.draw_svg_icon(layers, SvgIcon::Bot, mark_x, mark_y, mark, palette.text)?
                }
            }
            self.draw_text(
                layers,
                &ui_font,
                text_x,
                row_y,
                &label,
                palette.text,
                text_width,
            )?;
            self.draw_text(
                layers,
                &body_font,
                text_x,
                self.settings_row_description_y(row_y),
                &description,
                palette.secondary_text,
                text_width,
            )?;
            self.ui_context.push(
                rect(row_x, row_y - self.ui_px(6.0), row_width, self.ui_px(56.0)),
                crate::ui::WidgetKind::Button,
                SettingsAction::ToggleAgentDetails(agent_id),
            );
            let row_top = row_y;
            row_y += row_step;
            if expanded {
                if let Some(lines) = &expanded_detail {
                    let block_y = row_top + detail_block_rel_y;
                    self.draw_rounded_frame(
                        layers,
                        0,
                        row_x,
                        block_y,
                        row_width,
                        detail_block_height,
                        palette.control_bg,
                        palette.rule,
                        self.ui_px(10.0),
                    )?;
                    let mut detail_y = block_y + self.ui_px(12.0);
                    for line in lines {
                        self.draw_text(
                            layers,
                            &body_font,
                            row_x + detail_text_inset,
                            detail_y,
                            line,
                            palette.secondary_text,
                            row_width - detail_text_inset * 2.0,
                        )?;
                        detail_y += detail_line_step;
                    }
                }
                row_y += expanded_extra;
            }
            draw_top_rule = true;
            prev_expanded = expanded;
        }
        Ok(())
    }

    /// Tab icons: the switch, a card for every look a pane tab can take but
    /// the fixed terminal one, and the editor for the card picked. Cards sit
    /// beside the editor where the page is wide enough, above it where it is
    /// not; beside them, the editor stays in view while they scroll.
    fn paint_tab_icons(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        max_width: f32,
    ) -> anyhow::Result<()> {
        let max_width =
            (self.dimensions.pixel_width as f32 - self.ui_px(TAB_ICONS_EDGE_MARGIN) - x)
                .max(max_width);
        let palette = self.palette();
        let body_font = Rc::clone(&self.body_font);
        let scroll = self.ui.content_scroll.offset;
        let section_y = self.ui_px(CONTENT_SECTION_Y) - scroll;
        let line_height = self.metrics.cell_size.height as f32;
        let catalog = crate::tab_icons::catalog();
        let editable = |card: &&crate::tab_icons::Card| card.id != crate::tab_icons::TERMINAL_CARD;
        if self
            .ui
            .tab_icon_selected
            .as_deref()
            .and_then(|id| catalog.card(id))
            .filter(editable)
            .is_none()
        {
            if let Some(first) = catalog.cards.iter().find(editable) {
                self.select_tab_icon(first.id.clone());
            }
        }

        let (switch_card_y, switch_row_y) = self.settings_card_geometry(section_y, 1);
        let switch_card_height = self.settings_card_height(1);
        let card_padding = self.ui_px(36.0);
        self.paint_group_card(layers, x, switch_card_y, max_width, switch_card_height)?;
        self.paint_toggle_setting_row(
            layers,
            x + card_padding,
            switch_row_y,
            max_width - card_padding * 2.0,
            &crate::i18n::tr("settings-tab-icons-enabled"),
            &crate::i18n::tr("settings-tab-icons-enabled-description"),
            catalog.enabled,
            SettingsAction::ToggleTabIcons,
            false,
        )?;

        let heading_y = switch_card_y + switch_card_height + self.settings_section_card_gap();
        self.draw_text(
            layers,
            &body_font,
            x,
            heading_y,
            &crate::i18n::tr("settings-tab-icons-cards-heading"),
            palette.muted_text,
            max_width,
        )?;
        let note_y = heading_y + line_height + self.ui_px(8.0);
        self.draw_text(
            layers,
            &body_font,
            x,
            note_y,
            &crate::i18n::tr("settings-tab-icons-cards-note"),
            palette.secondary_text,
            max_width,
        )?;
        let area_y = note_y + line_height + self.ui_px(28.0);

        let gap = self.ui_px(18.0);
        let editor_width = self.ui_px(640.0).min(max_width);
        let min_tile_width = self.ui_px(168.0);
        let side_by_side = max_width >= editor_width + gap + (min_tile_width + gap) * 3.0;
        let grid_width = if side_by_side {
            max_width - editor_width - gap
        } else {
            max_width
        };

        // The search heads the cards' column; the cards are what it finds.
        let search_height = self.ui.tokens.control_height;
        self.paint_tab_icon_search(layers, rect(x, area_y, grid_width, search_height))?;
        let grid_y = area_y + search_height + gap;
        let query = self.ui.tab_icon_search.text().trim().to_lowercase();
        let shown: Vec<&crate::tab_icons::Card> = catalog
            .cards
            .iter()
            .filter(editable)
            .filter(|card| card.matches(&query))
            .collect();
        // Nothing found: a line saying so, then the new-card tile as ever.
        let empty_note = if shown.is_empty() {
            self.draw_text(
                layers,
                &body_font,
                x,
                grid_y,
                &crate::i18n::tr("settings-tab-icons-no-matches"),
                palette.secondary_text,
                grid_width,
            )?;
            line_height + gap
        } else {
            0.0
        };

        let columns = (((grid_width + gap) / (min_tile_width + gap)).floor() as usize).max(1);
        let tile_width = (grid_width - gap * (columns - 1) as f32) / columns as f32;
        let tile_height = self.ui_px(196.0);
        let tile_count = shown.len() + 1;
        let rows = tile_count.div_ceil(columns);
        let tiles_y = grid_y + empty_note;
        let grid_bottom = tiles_y + rows as f32 * tile_height + rows.saturating_sub(1) as f32 * gap;

        self.ui.tab_icon_cards = shown.iter().map(|card| card.id.clone()).collect();
        self.ui.tab_icon_drop_rects.clear();
        for index in 0..tile_count {
            let tile = rect(
                x + (index % columns) as f32 * (tile_width + gap),
                tiles_y + (index / columns) as f32 * (tile_height + gap),
                tile_width,
                tile_height,
            );
            match shown.get(index) {
                Some(card) => self.paint_tab_icon_tile(layers, tile, index, card)?,
                None => self.paint_tab_icon_new_tile(layers, tile)?,
            }
        }

        let selected = self
            .ui
            .tab_icon_selected
            .as_deref()
            .and_then(|id| catalog.card(id))
            .cloned();
        let mut bottom = grid_bottom;
        if let Some(card) = selected {
            let layout = self.tab_icon_editor_layout(&card, editor_width);
            let (editor_x, natural_y) = if side_by_side {
                (x + grid_width + gap, area_y)
            } else {
                (x, grid_bottom + gap)
            };
            let editor_y = if side_by_side {
                self.pinned_tab_icon_editor_y(natural_y, grid_bottom, layout.height)
            } else {
                natural_y
            };
            self.paint_tab_icon_editor(layers, editor_x, editor_y, editor_width, &card, &layout)?;
            // The page ends where the editor would, left unpinned.
            bottom = bottom.max(natural_y + layout.height);
        }
        self.ui.content_scroll.set_extents(
            self.content_viewport_extent(),
            self.settings_content_extent(bottom + scroll),
        );
        Ok(())
    }

    /// Where the editor beside the cards goes: down the page with them until
    /// it reaches the top of the view, pinned there while they scroll on, and
    /// carried off again where they end. One taller than the view pins its
    /// bottom edge instead, so all of it can still be scrolled to.
    fn pinned_tab_icon_editor_y(&self, natural_y: f32, cards_bottom: f32, height: f32) -> f32 {
        pinned_beside_column(
            natural_y,
            cards_bottom,
            height,
            self.content_scroll_area_top() + self.ui_px(TAB_ICONS_EDGE_MARGIN),
            self.content_bottom() - self.ui_px(TAB_ICONS_EDGE_MARGIN),
        )
    }

    /// The field heading the cards, shaped like the sidebar's search: a
    /// pill with a magnifier, and a clear button once there is a query.
    fn paint_tab_icon_search(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        field: window::RectF,
    ) -> anyhow::Result<()> {
        let palette = self.palette();
        let query = self.ui.tab_icon_search.text().to_string();
        self.paint_text_input(
            layers,
            0,
            TextInputSpec {
                placeholder: &crate::i18n::tr("settings-tab-icons-search-placeholder"),
                text: &query,
                rect: field,
                focused: self.ui.interaction.focused == Some(SettingsAction::TabIconSearchInput),
                selected_all: self.ui.tab_icon_search.selected_all,
                action: SettingsAction::TabIconSearchInput,
            },
        )?;
        let icon_size = self.sidebar_icon_size();
        self.draw_svg_icon(
            layers,
            SettingsIcon::Search.svg(),
            field.origin.x + self.ui_px(15.0),
            field.origin.y + (field.size.height - icon_size) / 2.0,
            icon_size,
            palette.muted_text,
        )?;
        if query.is_empty() {
            return Ok(());
        }
        let clear_size = self.ui_px(34.0);
        let clear_icon_size = self.ui_px(24.0);
        let clear_rect = rect(
            field.origin.x + field.size.width - clear_size - self.ui_px(10.0),
            field.origin.y + (field.size.height - clear_size) / 2.0,
            clear_size,
            clear_size,
        );
        self.ui_context.push(
            clear_rect,
            WidgetKind::Button,
            SettingsAction::ClearTabIconSearch,
        );
        if self.ui.interaction.hovered == Some(SettingsAction::ClearTabIconSearch)
            || self.ui.interaction.pressed == Some(SettingsAction::ClearTabIconSearch)
        {
            // Above the field's own fill, which is on layer 0.
            self.draw_rounded_rect(
                layers,
                1,
                clear_rect.origin.x,
                clear_rect.origin.y,
                clear_rect.size.width,
                clear_rect.size.height,
                palette.control_hover_bg,
                self.ui_px(14.0),
            )?;
        }
        self.draw_svg_icon(
            layers,
            SettingsIcon::Clear.svg(),
            clear_rect.origin.x + (clear_rect.size.width - clear_icon_size) / 2.0,
            clear_rect.origin.y + (clear_rect.size.height - clear_icon_size) / 2.0,
            clear_icon_size,
            palette.secondary_text,
        )?;
        Ok(())
    }

    fn paint_tab_icon_tile(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        tile: window::RectF,
        index: usize,
        card: &crate::tab_icons::Card,
    ) -> anyhow::Result<()> {
        let palette = self.palette();
        let ui_font = Rc::clone(&self.ui_font);
        let action = SettingsAction::TabIconSelect(index.min(u16::MAX as usize) as u16);
        let selected = self.ui.tab_icon_selected.as_deref() == Some(card.id.as_str());
        let target = TabIconDropTarget::Card(index);
        self.paint_tab_icon_tile_frame(layers, tile, action, selected, target)?;
        self.ui.tab_icon_drop_rects.push((tile, target));

        let diameter = self.ui_px(88.0);
        let icon_y = tile.origin.y + self.ui_px(26.0);
        self.draw_tab_icon(
            layers,
            card.circle,
            &card.glyph,
            card.glyph_color.linear(),
            tile.origin.x + (tile.size.width - diameter) / 2.0,
            icon_y,
            diameter,
        )?;
        let name_width = (tile.size.width - self.ui_px(24.0)).max(0.0);
        let name = self.text_with_ellipsis(&ui_font, &card.name, name_width);
        let name_x = tile.origin.x
            + ((tile.size.width - self.measure_text_width(&ui_font, &name)) / 2.0).max(0.0);
        self.draw_text(
            layers,
            &ui_font,
            name_x,
            icon_y + diameter + self.ui_px(20.0),
            &name,
            palette.text,
            name_width,
        )?;
        Ok(())
    }

    fn paint_tab_icon_new_tile(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        tile: window::RectF,
    ) -> anyhow::Result<()> {
        let palette = self.palette();
        let ui_font = Rc::clone(&self.ui_font);
        let action = SettingsAction::TabIconNew;
        let target = TabIconDropTarget::NewCard;
        self.paint_tab_icon_tile_frame(layers, tile, action, false, target)?;
        self.ui.tab_icon_drop_rects.push((tile, target));

        let icon = self.ui_px(40.0);
        let icon_y = tile.origin.y + self.ui_px(50.0);
        self.draw_svg_icon(
            layers,
            SvgIcon::Plus,
            tile.origin.x + (tile.size.width - icon) / 2.0,
            icon_y,
            icon,
            palette.secondary_text,
        )?;
        let label = crate::i18n::tr("settings-tab-icons-new");
        let label_width = (tile.size.width - self.ui_px(24.0)).max(0.0);
        let label_x = tile.origin.x
            + ((tile.size.width - self.measure_text_width(&ui_font, &label)) / 2.0).max(0.0);
        self.draw_text(
            layers,
            &ui_font,
            label_x,
            icon_y + icon + self.ui_px(44.0),
            &label,
            palette.secondary_text,
            label_width,
        )?;
        Ok(())
    }

    /// A tile's surface: lifted while hovered or selected, ringed while an
    /// SVG is dragged over it.
    fn paint_tab_icon_tile_frame(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        tile: window::RectF,
        action: SettingsAction,
        selected: bool,
        target: TabIconDropTarget,
    ) -> anyhow::Result<()> {
        let palette = self.palette();
        let hovered = self.ui.interaction.hovered == Some(action);
        let pressed = self.ui.interaction.pressed == Some(action);
        let dropping = self.ui.tab_icon_drop_target == Some(target);
        let fill = if pressed {
            palette.control_pressed_bg
        } else if selected || hovered || dropping {
            palette.control_hover_bg
        } else {
            palette.card_bg
        };
        let border = if dropping {
            palette.text
        } else if selected {
            palette.muted_text
        } else {
            palette.separator
        };
        let radius = self.ui_px(28.0);
        self.draw_rounded_frame(
            layers,
            0,
            tile.origin.x,
            tile.origin.y,
            tile.size.width,
            tile.size.height,
            fill,
            border,
            radius,
        )?;
        if selected || dropping {
            // A second hairline inside the first: one pixel of border does
            // not tell the selected tile from its neighbours at a glance.
            let inset = self.ui_px(1.0).max(1.0);
            self.draw_rounded_frame(
                layers,
                0,
                tile.origin.x + inset,
                tile.origin.y + inset,
                tile.size.width - inset * 2.0,
                tile.size.height - inset * 2.0,
                fill,
                border,
                radius - inset,
            )?;
        }
        self.ui_context.push(tile, WidgetKind::Button, action);
        Ok(())
    }

    /// Where the editor's parts go for `card` at `width`, worked out before
    /// anything is drawn: its card needs the height first, and so does the
    /// place a pinned editor is kept at.
    fn tab_icon_editor_layout(
        &self,
        card: &crate::tab_icons::Card,
        width: f32,
    ) -> TabIconEditorLayout {
        let ui_font = Rc::clone(&self.ui_font);
        let body_font = Rc::clone(&self.body_font);
        let inner = (width - self.ui_px(TAB_ICON_EDITOR_PAD) * 2.0).max(0.0);
        let label_width = [
            crate::i18n::tr("settings-tab-icons-programs"),
            crate::i18n::tr("settings-tab-icons-glyph"),
        ]
        .iter()
        .map(|label| self.measure_text_width(&body_font, label))
        .fold(0.0, f32::max);
        let value_width = (inner
            - self.ui_px(TAB_ICON_EDITOR_ROW_PAD_X) * 2.0
            - label_width
            - self.ui_px(TAB_ICON_EDITOR_LABEL_GAP))
        .max(0.0);
        let gap = self.ui_px(TAB_ICON_EDITOR_ITEM_GAP);

        // The Programs row: a chip per program, wrapping right-aligned, and
        // under them the field that adds one, across the whole column. The
        // Shape row: its buttons, wrapping the same way.
        let chip_close = self.ui_px(40.0);
        let program_widths: Vec<f32> = card
            .programs
            .iter()
            .map(|program| {
                (self.measure_text_width(&ui_font, program) + self.ui_px(28.0) + chip_close)
                    .min(value_width)
            })
            .collect();
        let (program_places, chip_lines) = flow_right(&program_widths, value_width, gap);
        let add_line = if card.programs.is_empty() {
            0
        } else {
            chip_lines
        };
        let program_lines = add_line + 1;

        let mut shape_buttons = vec![(
            crate::i18n::tr("settings-tab-icons-choose-svg"),
            SettingsAction::TabIconChooseSvg,
        )];
        if matches!(card.glyph, crate::tab_icons::GlyphKey::Svg(_)) {
            shape_buttons.push((
                crate::i18n::tr("settings-tab-icons-clear-svg"),
                SettingsAction::TabIconClearSvg,
            ));
        }
        let shape_widths: Vec<f32> = shape_buttons
            .iter()
            .map(|(label, _)| self.button_width_for_label(label, 0.0).min(value_width))
            .collect();
        let (shape_places, shape_lines) = flow_right(&shape_widths, value_width, gap);

        let lines = |count: usize| {
            self.ui_px(CONTROL_HEIGHT) * count as f32
                + self.ui_px(TAB_ICON_EDITOR_LINE_GAP) * count.saturating_sub(1) as f32
        };
        let row_pad_y = self.ui_px(TAB_ICON_EDITOR_ROW_PAD_Y);
        let programs_row = row_pad_y * 2.0 + lines(program_lines);
        let shape_row = row_pad_y * 2.0 + lines(shape_lines);
        let color_row = row_pad_y * 2.0
            + self.ui_px(CONTROL_HEIGHT)
            + self.ui_px(TAB_ICON_EDITOR_SWATCH_GAP)
            + self.ui_px(TAB_ICON_EDITOR_SWATCH);
        let hairline = self.ui_px(1.0).max(1.0);
        let height = self.ui_px(TAB_ICON_EDITOR_PAD) * 2.0
            + self.ui_px(TAB_ICON_EDITOR_HEAD)
            + self.ui_px(TAB_ICON_EDITOR_SECTION_GAP) * 2.0
            + programs_row
            + hairline
            + shape_row
            + color_row * 2.0
            + hairline;

        let chips = card
            .programs
            .iter()
            .zip(program_places.iter().zip(program_widths.iter()))
            .map(|(program, (&(chip_x, line), &chip_width))| {
                (chip_x, line, chip_width, program.clone())
            })
            .collect();
        let add_field = (0.0, add_line, value_width);
        let shape_buttons = shape_buttons
            .into_iter()
            .zip(shape_places.into_iter().zip(shape_widths))
            .map(|((label, action), ((button_x, line), button_width))| {
                (button_x, line, button_width, label, action)
            })
            .collect();
        TabIconEditorLayout {
            height,
            label_width,
            chips,
            add_field,
            programs_row,
            shape_buttons,
            shape_row,
            color_row,
        }
    }

    /// The selected card's editor, laid out by `tab_icon_editor_layout`: a
    /// header with the icon, its name and what may be undone, then the
    /// settings in two groups of rows -- which programs and which shape,
    /// then the two colours.
    fn paint_tab_icon_editor(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
        card: &crate::tab_icons::Card,
        layout: &TabIconEditorLayout,
    ) -> anyhow::Result<()> {
        let palette = self.palette();
        let ui_font = Rc::clone(&self.ui_font);
        let body_font = Rc::clone(&self.body_font);
        let line_height = self.metrics.cell_size.height as f32;
        let pad = self.ui_px(TAB_ICON_EDITOR_PAD);
        let inner_x = x + pad;
        let inner_width = (width - pad * 2.0).max(0.0);
        let control_height = self.ui_px(CONTROL_HEIGHT);
        let row_pad_x = self.ui_px(TAB_ICON_EDITOR_ROW_PAD_X);
        let row_pad_y = self.ui_px(TAB_ICON_EDITOR_ROW_PAD_Y);
        let line_gap = self.ui_px(TAB_ICON_EDITOR_LINE_GAP);
        let hairline = self.ui_px(1.0).max(1.0);

        // An SVG dropped anywhere on the editor goes to the card it shows.
        let bounds = rect(x, y, width, layout.height);
        self.ui
            .tab_icon_drop_rects
            .push((bounds, TabIconDropTarget::Editor));
        let dropping = self.ui.tab_icon_drop_target == Some(TabIconDropTarget::Editor);
        self.draw_rounded_frame(
            layers,
            0,
            x,
            y,
            width,
            layout.height,
            palette.card_bg,
            if dropping {
                palette.text
            } else {
                palette.separator
            },
            self.ui_px(34.0),
        )?;

        // Header: the icon, the name, and Reset or Delete when there is one.
        let head = self.ui_px(TAB_ICON_EDITOR_HEAD);
        let head_y = y + pad;
        self.draw_tab_icon(
            layers,
            card.circle,
            &card.glyph,
            card.glyph_color.linear(),
            inner_x,
            head_y,
            head,
        )?;
        let text_x = inner_x + head + self.ui_px(24.0);
        let mut text_right = inner_x + inner_width;
        let header_button = if !card.builtin {
            if self.ui.confirm_delete_tab_icon.as_deref() == Some(card.id.as_str()) {
                Some((
                    crate::i18n::tr("settings-tab-icons-delete-confirm"),
                    SettingsAction::TabIconDelete,
                ))
            } else {
                Some((
                    crate::i18n::tr("settings-tab-icons-delete"),
                    SettingsAction::TabIconDelete,
                ))
            }
        } else if card.changed {
            Some((
                crate::i18n::tr("settings-tab-icons-reset"),
                SettingsAction::TabIconReset,
            ))
        } else {
            None
        };
        if let Some((label, action)) = header_button {
            let button_width = self
                .button_width_for_label(&label, 0.0)
                .min((text_right - text_x).max(0.0));
            text_right -= button_width;
            self.draw_button(
                layers,
                text_right,
                head_y + (head - control_height) / 2.0,
                button_width,
                &label,
                action,
            )?;
            text_right -= self.ui_px(16.0);
        }
        let text_width = (text_right - text_x).max(0.0);
        if card.builtin {
            let kind = crate::i18n::tr(if card.changed {
                "settings-tab-icons-kind-changed"
            } else {
                "settings-tab-icons-kind-builtin"
            });
            let lines_y = head_y + (head - line_height * 2.0 - self.ui_px(4.0)) / 2.0;
            self.draw_text(
                layers,
                &ui_font,
                text_x,
                lines_y,
                &card.name,
                palette.text,
                text_width,
            )?;
            self.draw_text(
                layers,
                &body_font,
                text_x,
                lines_y + line_height + self.ui_px(4.0),
                &kind,
                palette.muted_text,
                text_width,
            )?;
        } else {
            let value = self.ui.tab_icon_name_input.text().to_string();
            self.paint_value_input_box(
                layers,
                rect(
                    text_x,
                    head_y + (head - control_height) / 2.0,
                    text_width,
                    control_height,
                ),
                SettingsAction::TabIconNameInput,
                &value,
                &crate::i18n::tr("settings-tab-icons-name-placeholder"),
            )?;
        }

        // The first group: Programs, then Shape.
        let group_fill = self.chrome_palette.group_bg;
        let group_radius = self.ui_px(24.0);
        let group_y = head_y + head + self.ui_px(TAB_ICON_EDITOR_SECTION_GAP);
        let group_height = layout.programs_row + hairline + layout.shape_row;
        self.draw_rounded_rect(
            layers,
            0,
            inner_x,
            group_y,
            inner_width,
            group_height,
            group_fill,
            group_radius,
        )?;
        let label_x = inner_x + row_pad_x;
        let value_x = label_x + layout.label_width + self.ui_px(TAB_ICON_EDITOR_LABEL_GAP);
        let line_top =
            |row_y: f32, line: usize| row_y + row_pad_y + line as f32 * (control_height + line_gap);

        let row_y = group_y;
        self.draw_text(
            layers,
            &body_font,
            label_x,
            self.control_text_y(line_top(row_y, 0), control_height),
            &crate::i18n::tr("settings-tab-icons-programs"),
            palette.text,
            layout.label_width,
        )?;
        self.ui.tab_icon_programs = card.programs.clone();
        let chip_height = self.ui_px(48.0);
        let chip_close = self.ui_px(40.0);
        for (index, (chip_x, line, chip_width, program)) in layout.chips.iter().enumerate() {
            let chip_left = value_x + chip_x;
            let chip_top = line_top(row_y, *line) + (control_height - chip_height) / 2.0;
            self.draw_rounded_rect(
                layers,
                0,
                chip_left,
                chip_top,
                *chip_width,
                chip_height,
                palette.control_bg,
                chip_height / 2.0,
            )?;
            let text_width = (chip_width - self.ui_px(14.0) - chip_close).max(0.0);
            let text = self.text_with_ellipsis(&ui_font, program, text_width);
            self.draw_text(
                layers,
                &ui_font,
                chip_left + self.ui_px(14.0),
                self.control_text_y(chip_top, chip_height),
                &text,
                palette.text,
                text_width,
            )?;
            let action = SettingsAction::TabIconRemoveProgram(index.min(u16::MAX as usize) as u16);
            let close = rect(
                chip_left + chip_width - chip_close,
                chip_top,
                chip_close,
                chip_height,
            );
            self.ui_context.push(close, WidgetKind::Button, action);
            let hovered = self.ui.interaction.hovered == Some(action);
            if hovered {
                let ring = chip_height - self.ui_px(16.0);
                self.draw_rounded_rect(
                    layers,
                    1,
                    close.origin.x + (chip_close - ring) / 2.0 - self.ui_px(4.0),
                    chip_top + (chip_height - ring) / 2.0,
                    ring,
                    ring,
                    palette.control_hover_bg,
                    ring / 2.0,
                )?;
            }
            let glyph = self.ui_px(20.0);
            self.draw_svg_icon(
                layers,
                SvgIcon::X,
                close.origin.x + (chip_close - glyph) / 2.0 - self.ui_px(4.0),
                chip_top + (chip_height - glyph) / 2.0,
                glyph,
                if hovered {
                    palette.text
                } else {
                    palette.secondary_text
                },
            )?;
        }
        let (field_x, field_line, field_width) = layout.add_field;
        let program_value = self.ui.tab_icon_program_input.text().to_string();
        self.paint_value_input_box(
            layers,
            rect(
                value_x + field_x,
                line_top(row_y, field_line),
                field_width,
                control_height,
            ),
            SettingsAction::TabIconProgramInput,
            &program_value,
            &crate::i18n::tr("settings-tab-icons-program-placeholder"),
        )?;

        let rule_y = row_y + layout.programs_row;
        self.draw_rect(
            layers,
            0,
            label_x,
            rule_y,
            inner_x + inner_width - label_x,
            hairline,
            palette.separator,
        )?;
        let row_y = rule_y + hairline;
        self.draw_text(
            layers,
            &body_font,
            label_x,
            self.control_text_y(line_top(row_y, 0), control_height),
            &crate::i18n::tr("settings-tab-icons-glyph"),
            palette.text,
            layout.label_width,
        )?;
        for (button_x, line, button_width, label, action) in &layout.shape_buttons {
            self.draw_button(
                layers,
                value_x + button_x,
                line_top(row_y, *line),
                *button_width,
                label,
                *action,
            )?;
        }

        // The second group: the two colours.
        let group_y = group_y + group_height + self.ui_px(TAB_ICON_EDITOR_SECTION_GAP);
        self.draw_rounded_rect(
            layers,
            0,
            inner_x,
            group_y,
            inner_width,
            layout.color_row * 2.0 + hairline,
            group_fill,
            group_radius,
        )?;
        let circle_value = self.ui.tab_icon_circle_input.text().to_string();
        self.paint_tab_icon_color_row(
            layers,
            inner_x,
            group_y,
            inner_width,
            &crate::i18n::tr("settings-tab-icons-circle-color"),
            card.circle,
            &circle_value,
            SettingsAction::TabIconCircleInput,
            crate::tab_icons::CIRCLE_PRESETS,
            SettingsAction::TabIconCirclePreset,
        )?;
        let rule_y = group_y + layout.color_row;
        self.draw_rect(
            layers,
            0,
            label_x,
            rule_y,
            inner_x + inner_width - label_x,
            hairline,
            palette.separator,
        )?;
        let glyph_value = self.ui.tab_icon_glyph_input.text().to_string();
        self.paint_tab_icon_color_row(
            layers,
            inner_x,
            rule_y + hairline,
            inner_width,
            &crate::i18n::tr("settings-tab-icons-glyph-color"),
            card.glyph_color,
            &glyph_value,
            SettingsAction::TabIconGlyphColorInput,
            crate::tab_icons::GLYPH_PRESETS,
            SettingsAction::TabIconGlyphPreset,
        )?;
        Ok(())
    }

    /// One colour's row: its label with the `#RRGGBB` field and a sample of
    /// the colour in use across from it, and the presets to pick from under
    /// them.
    #[allow(clippy::too_many_arguments)]
    fn paint_tab_icon_color_row(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
        label: &str,
        current: crate::tab_icons::Rgb,
        value: &str,
        input: SettingsAction,
        presets: &[crate::tab_icons::Rgb],
        preset_action: fn(u8) -> SettingsAction,
    ) -> anyhow::Result<()> {
        let palette = self.palette();
        let body_font = Rc::clone(&self.body_font);
        let control_height = self.ui_px(CONTROL_HEIGHT);
        let row_pad_x = self.ui_px(TAB_ICON_EDITOR_ROW_PAD_X);
        let top = y + self.ui_px(TAB_ICON_EDITOR_ROW_PAD_Y);
        let left = x + row_pad_x;
        let right = x + width - row_pad_x;

        let field_width = self.ui_px(200.0).min((right - left) / 2.0);
        let field_x = right - field_width;
        self.draw_text(
            layers,
            &body_font,
            left,
            self.control_text_y(top, control_height),
            label,
            palette.text,
            (field_x - left).max(0.0),
        )?;
        self.paint_value_input_box(
            layers,
            rect(field_x, top, field_width, control_height),
            input,
            value,
            "#RRGGBB",
        )?;
        let sample = control_height - self.ui_px(28.0);
        self.draw_tab_icon_swatch(
            layers,
            field_x - self.ui_px(12.0) - sample,
            top + (control_height - sample) / 2.0,
            sample,
            current.linear(),
            false,
        )?;

        let swatch = self.ui_px(TAB_ICON_EDITOR_SWATCH);
        let swatch_gap = self.ui_px(TAB_ICON_EDITOR_ITEM_GAP);
        let presets_y = top + control_height + self.ui_px(TAB_ICON_EDITOR_SWATCH_GAP);
        for (index, preset) in presets.iter().enumerate() {
            let preset_x = left + index as f32 * (swatch + swatch_gap);
            if preset_x + swatch > right {
                break;
            }
            let action = preset_action(index.min(u8::MAX as usize) as u8);
            let hovered = self.ui.interaction.hovered == Some(action);
            self.draw_tab_icon_swatch(
                layers,
                preset_x,
                presets_y,
                swatch,
                preset.linear(),
                *preset == current || hovered,
            )?;
            self.ui_context.push(
                rect(preset_x, presets_y, swatch, swatch),
                WidgetKind::Button,
                action,
            );
        }
        Ok(())
    }

    /// A round colour sample with a hairline, so white and near-background
    /// colours keep an edge; `marked` rings it as the one in use.
    fn draw_tab_icon_swatch(
        &self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        size: f32,
        color: LinearRgba,
        marked: bool,
    ) -> anyhow::Result<()> {
        let palette = self.palette();
        let edge = self.ui_px(2.0).max(1.0);
        if marked {
            let ring = self.ui_px(4.0);
            self.draw_rounded_rect(
                layers,
                0,
                x - ring,
                y - ring,
                size + ring * 2.0,
                size + ring * 2.0,
                palette.text.mul_alpha(0.55),
                size / 2.0 + ring,
            )?;
            self.draw_rounded_rect(
                layers,
                0,
                x - ring + edge,
                y - ring + edge,
                size + (ring - edge) * 2.0,
                size + (ring - edge) * 2.0,
                palette.card_bg,
                size / 2.0 + ring - edge,
            )?;
        }
        self.draw_rounded_rect(
            layers,
            1,
            x,
            y,
            size,
            size,
            palette.text.mul_alpha(0.18),
            size / 2.0,
        )?;
        self.draw_rounded_rect(
            layers,
            1,
            x + edge,
            y + edge,
            size - edge * 2.0,
            size - edge * 2.0,
            color,
            size / 2.0 - edge,
        )
    }

    /// A tab icon as the pane tabs draw it: the lit circle and its glyph.
    #[allow(clippy::too_many_arguments)]
    fn draw_tab_icon(
        &self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        circle: crate::tab_icons::Rgb,
        glyph: &crate::tab_icons::GlyphKey,
        glyph_color: LinearRgba,
        x: f32,
        y: f32,
        diameter: f32,
    ) -> anyhow::Result<()> {
        let ctx = crate::ui::draw::DrawContext::new(
            self.render_state.as_ref().unwrap(),
            self.dimensions,
            &self.metrics,
        );
        crate::tab_icons::draw_plate(
            &ctx,
            layers,
            1,
            x,
            y,
            diameter,
            circle,
            self.chrome_palette.is_dark(),
        )?;
        let size = (diameter * crate::termwindow::ui::tokens::TAB_ICON_GLYPH_RATIO).round();
        crate::tab_icons::draw_glyph(
            &ctx,
            layers,
            2,
            glyph,
            x + (diameter - size) / 2.0,
            y + (diameter - size) / 2.0,
            size,
            glyph_color,
        )
    }

    /// Make `id` the card being edited, and show its values in the fields.
    fn select_tab_icon(&mut self, id: String) {
        if self.ui.tab_icon_selected.as_deref() != Some(id.as_str()) {
            self.ui.tab_icon_program_input.clear();
        }
        self.ui.tab_icon_selected = Some(id);
        self.ui.confirm_delete_tab_icon = None;
        self.sync_tab_icon_inputs();
    }

    /// The fields show the selected card as saved. A field being typed into
    /// is left alone: the user is not done with it.
    fn sync_tab_icon_inputs(&mut self) {
        let catalog = crate::tab_icons::catalog();
        let Some(card) = self
            .ui
            .tab_icon_selected
            .as_deref()
            .and_then(|id| catalog.card(id))
        else {
            return;
        };
        let focused = self.ui.interaction.focused;
        if focused != Some(SettingsAction::TabIconNameInput) {
            // The name as stored: an unnamed card shows the placeholder, not
            // the stand-in name the catalog gives it, which typing would
            // otherwise extend.
            let name = self
                .native_settings
                .tab_icons
                .cards
                .iter()
                .find(|stored| stored.id == card.id)
                .and_then(|stored| stored.name.clone())
                .unwrap_or_default();
            self.ui.tab_icon_name_input.set_text_end(name);
            self.ui.tab_icon_name_input_dirty = false;
        }
        if focused != Some(SettingsAction::TabIconCircleInput) {
            self.ui
                .tab_icon_circle_input
                .set_text_end(card.circle.to_hex());
            self.ui.tab_icon_circle_input_dirty = false;
        }
        if focused != Some(SettingsAction::TabIconGlyphColorInput) {
            self.ui
                .tab_icon_glyph_input
                .set_text_end(card.glyph_color.to_hex());
            self.ui.tab_icon_glyph_input_dirty = false;
        }
    }

    /// After any change to the cards: take the saved settings back (this
    /// window keeps a copy it writes whole, which must not overwrite the
    /// change later), repaint every terminal window, and say how it went.
    fn after_tab_icon_change(&mut self, result: anyhow::Result<()>) {
        match result {
            Ok(()) => {
                self.set_native_settings(crate::native_settings::load());
                if let Some(front_end) = crate::frontend::try_front_end() {
                    front_end.invalidate_all_windows();
                }
                self.sync_tab_icon_inputs();
                self.status = crate::i18n::tr("settings-status-tab-icons-saved");
            }
            Err(err) => {
                log::error!("failed to save tab icons: {err:#}");
                self.status = settings_tr(
                    "settings-status-tab-icons-error",
                    &[("error", format!("{err:#}"))],
                );
            }
        }
    }

    fn selected_tab_icon(&self) -> Option<String> {
        self.ui.tab_icon_selected.clone()
    }

    /// Commit a tab icon field on its way out of focus, or on Return in the
    /// program field (which stays focused for the next name).
    fn commit_tab_icon_input(&mut self, focused: SettingsAction) {
        let Some(id) = self.selected_tab_icon() else {
            return;
        };
        match focused {
            SettingsAction::TabIconProgramInput => {
                let program = self.ui.tab_icon_program_input.text().trim().to_string();
                if program.is_empty() {
                    return;
                }
                if crate::tab_icons::normalize_program_name(&program).is_none() {
                    self.status = crate::i18n::tr("settings-status-tab-icons-bad-program");
                    return;
                }
                match crate::tab_icons::add_program(&id, &program) {
                    Ok(moved_from) => {
                        self.ui.tab_icon_program_input.clear();
                        self.after_tab_icon_change(Ok(()));
                        if let Some(from) = moved_from {
                            self.status = settings_tr(
                                "settings-status-tab-icons-program-moved",
                                &[("program", program.to_lowercase()), ("from", from)],
                            );
                        }
                    }
                    Err(err) => self.after_tab_icon_change(Err(err)),
                }
            }
            SettingsAction::TabIconNameInput if self.ui.tab_icon_name_input_dirty => {
                self.ui.tab_icon_name_input_dirty = false;
                let name = self.ui.tab_icon_name_input.text().to_string();
                let result = crate::tab_icons::rename_card(&id, &name);
                self.after_tab_icon_change(result);
            }
            SettingsAction::TabIconCircleInput | SettingsAction::TabIconGlyphColorInput => {
                let (input, dirty) = if focused == SettingsAction::TabIconCircleInput {
                    (
                        self.ui.tab_icon_circle_input.text().to_string(),
                        std::mem::take(&mut self.ui.tab_icon_circle_input_dirty),
                    )
                } else {
                    (
                        self.ui.tab_icon_glyph_input.text().to_string(),
                        std::mem::take(&mut self.ui.tab_icon_glyph_input_dirty),
                    )
                };
                if !dirty {
                    return;
                }
                // `#` is optional to type: `3776ab` means what it says.
                let spelled = if input.trim().starts_with('#') {
                    input.trim().to_string()
                } else {
                    format!("#{}", input.trim())
                };
                match crate::tab_icons::Rgb::parse(&spelled) {
                    Some(color) => {
                        let result = if focused == SettingsAction::TabIconCircleInput {
                            crate::tab_icons::set_card_circle(&id, color)
                        } else {
                            crate::tab_icons::set_card_glyph_color(&id, color)
                        };
                        // The field is still focused here; let it show the
                        // colour as saved once focus moves on.
                        self.after_tab_icon_change(result);
                    }
                    None => {
                        self.status = crate::i18n::tr("settings-status-tab-icons-bad-color");
                        self.sync_tab_icon_inputs();
                    }
                }
            }
            _ => {}
        }
    }

    /// The drop target under a point, on the tab icons page only.
    fn tab_icon_drop_target_at(&self, x: f32, y: f32) -> Option<TabIconDropTarget> {
        // Cards scrolled up under the window's chrome are out of view.
        if self.selected != SettingsSection::TabIcons || y < self.content_scroll_area_top() {
            return None;
        }
        self.ui
            .tab_icon_drop_rects
            .iter()
            .find(|(area, _)| area.contains(euclid::point2(x, y)))
            .map(|(_, target)| *target)
    }

    /// Take an SVG onto card `card`, or onto a new one for `None`. The copy
    /// is made off the UI thread (the file may sit on a slow or synced
    /// disk) and applied when it comes back.
    fn import_tab_icon_svg(&mut self, card: Option<String>, path: PathBuf) {
        let instance_id = self.instance_id;
        let window = self.window.clone();
        promise::spawn::spawn(async move {
            let imported = promise::spawn::spawn_into_new_thread(move || {
                Ok::<_, anyhow::Error>(crate::tab_icons::import_svg(&path))
            })
            .await;
            promise::spawn::spawn_into_main_thread(async move {
                let Some(settings) = settings_window_for_instance(instance_id) else {
                    return;
                };
                let mut settings = settings.borrow_mut();
                match imported {
                    Ok(Ok(svg)) => settings.apply_imported_tab_icon_svg(card, svg),
                    Ok(Err(err)) => {
                        if let crate::tab_icons::ImportError::Io(io) = &err {
                            log::warn!("tab icon import failed: {io:#}");
                        }
                        settings.status = err.message();
                    }
                    Err(err) => {
                        log::warn!("tab icon import failed: {err:#}");
                        settings.status = crate::i18n::tr("tab-icons-import-failed");
                    }
                }
                if let Some(window) = window {
                    window.invalidate();
                }
            })
            .detach();
        })
        .detach();
    }

    fn apply_imported_tab_icon_svg(&mut self, card: Option<String>, svg: String) {
        // The selection is about to move to the card this lands on. A field
        // being typed into belongs to the card it was opened for, so commit
        // it there first.
        if card.as_deref() != self.ui.tab_icon_selected.as_deref() {
            self.set_focused_input(None);
        }
        let (id, result) = match card {
            Some(id) => {
                let result = crate::tab_icons::set_card_svg(&id, svg);
                (id, result)
            }
            None => match crate::tab_icons::create_card(Some(svg)) {
                Ok(Some(id)) => {
                    self.ui.tab_icon_search.clear();
                    (id, Ok(()))
                }
                Ok(None) => {
                    self.status = settings_tr(
                        "settings-status-tab-icons-too-many",
                        &[("count", crate::tab_icons::MAX_CUSTOM_CARDS.to_string())],
                    );
                    return;
                }
                Err(err) => return self.after_tab_icon_change(Err(err)),
            },
        };
        self.select_tab_icon(id);
        self.after_tab_icon_change(result);
    }

    fn choose_tab_icon_svg(&mut self) {
        let Some(id) = self.selected_tab_icon() else {
            return;
        };
        let Some(window) = self.window.clone() else {
            return;
        };
        let instance_id = self.instance_id;
        window.pick_file_async_with_options(
            FilePickerOptions {
                title: crate::i18n::tr("settings-tab-icons-svg-picker-title"),
                prompt: crate::i18n::tr("common-choose"),
                extension: "svg".to_string(),
                kind: "SVG".to_string(),
                directory: None,
            },
            Box::new(move |path| {
                let Some(path) = path else {
                    return;
                };
                promise::spawn::spawn_into_main_thread(async move {
                    if let Some(settings) = settings_window_for_instance(instance_id) {
                        settings.borrow_mut().import_tab_icon_svg(Some(id), path);
                    }
                })
                .detach();
            }),
        );
    }

    fn paint_terminal(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        max_width: f32,
    ) -> anyhow::Result<()> {
        let palette = self.palette();
        let body_font = Rc::clone(&self.body_font);
        let config = configuration();
        let scroll = self.ui.content_scroll.offset;
        let section_y = self.ui_px(CONTENT_SECTION_Y) - scroll;
        let card_padding = self.ui_px(36.0);
        let row_x = x + card_padding;
        let row_width = max_width - card_padding * 2.0;

        self.draw_text(
            layers,
            &body_font,
            x,
            section_y,
            &crate::i18n::tr("settings-terminal-heading"),
            palette.muted_text,
            max_width,
        )?;
        let (card_y, _) = self.settings_card_geometry(section_y, 0);
        let bottom = self.paint_card(layers, x, card_y, max_width, |this, layers, top| {
            let mut rows = RowCursor::new(top, this);
            let (tx, tw) = this.paint_row_tile(
                layers,
                row_x,
                rows.y,
                row_width,
                SvgIcon::SquareTerminal,
                TileColor::Gray,
            )?;
            rows.add(this.paint_default_shell_row(layers, tx, rows.y, tw, rows.rule())?);
            Ok(rows.bottom)
        })?;

        // The face: what it looks like, then what it is and how big.
        let mut lua_size = FluentArgs::new();
        lua_size.set("value", {
            let mut args = FluentArgs::new();
            args.set("value", format!("{:.1}", config.font_size));
            crate::i18n::tr_args("common-points", &args)
        });
        let size_description =
            crate::i18n::tr_args("settings-terminal-font-size-from-lua", &lua_size);
        let font_family = Self::effective_font_family(&config);
        let card_y = self.paint_card_heading(
            layers,
            x,
            bottom,
            max_width,
            &crate::i18n::tr("settings-terminal-font-heading"),
        )?;
        let bottom = self.paint_card(layers, x, card_y, max_width, |this, layers, top| {
            let preview_bottom = this.paint_terminal_font_preview(layers, row_x, top, row_width)?;
            let rule_y = preview_bottom + this.ui_px(24.0);
            this.paint_separator(layers, row_x, rule_y, row_width)?;
            let mut rows = RowCursor::new(rule_y + this.ui_px(28.0), this);
            let (tx, tw) = this.paint_row_tile(
                layers,
                row_x,
                rows.y,
                row_width,
                SvgIcon::Type,
                TileColor::Indigo,
            )?;
            rows.add(this.paint_setting_row(
                layers,
                tx,
                rows.y,
                tw,
                &crate::i18n::tr("settings-terminal-font-family"),
                &crate::i18n::tr("settings-terminal-font-family-description"),
                &font_family,
                rows.rule(),
            )?);
            let (tx, tw) = this.paint_row_tile(
                layers,
                row_x,
                rows.y,
                row_width,
                SvgIcon::ALargeSmall,
                TileColor::Indigo,
            )?;
            rows.add(this.paint_font_size_stepper_row(
                layers,
                tx,
                rows.y,
                tw,
                &crate::i18n::tr("settings-terminal-font-size"),
                &size_description,
                this.current_terminal_font_size_value(),
                None,
                SettingsAction::ResetFontSize,
                SettingsAction::DecreaseFontSize,
                SettingsAction::IncreaseFontSize,
                rows.rule(),
            )?);
            Ok(rows.bottom)
        })?;

        let card_y = self.paint_card_heading(
            layers,
            x,
            bottom,
            max_width,
            &crate::i18n::tr("settings-terminal-display-heading"),
        )?;
        let bottom = self.paint_card(layers, x, card_y, max_width, |this, layers, top| {
            use crate::native_settings::{NativeScrollMode, NativeTextContrast};
            let mut rows = RowCursor::new(top, this);

            let options: Vec<(String, SettingsAction)> = NativeScrollMode::ALL
                .iter()
                .map(|mode| {
                    (
                        localized_scroll_mode_label(*mode),
                        SettingsAction::SetScrollMode(*mode),
                    )
                })
                .collect();
            let selected = NativeScrollMode::ALL
                .iter()
                .position(|mode| *mode == this.native_settings.terminal.scroll_mode)
                .unwrap_or(0);
            let (tx, tw) = this.paint_row_tile(
                layers,
                row_x,
                rows.y,
                row_width,
                SvgIcon::Mouse,
                TileColor::Teal,
            )?;
            rows.add(this.paint_segmented_setting_row(
                layers,
                tx,
                rows.y,
                tw,
                &crate::i18n::tr("settings-scroll-mode"),
                &crate::i18n::tr("settings-scroll-mode-description"),
                &options,
                selected,
                rows.rule(),
            )?);

            let (tx, tw) = this.paint_row_tile(
                layers,
                row_x,
                rows.y,
                row_width,
                SvgIcon::MoveVertical,
                TileColor::Teal,
            )?;
            rows.add(this.paint_toggle_setting_row(
                layers,
                tx,
                rows.y,
                tw,
                &crate::i18n::tr("settings-overlay-scrollbar"),
                &crate::i18n::tr("settings-overlay-scrollbar-description"),
                this.native_settings.terminal.overlay_scrollbar,
                SettingsAction::ToggleOverlayScrollbar,
                rows.rule(),
            )?);

            let options: Vec<(String, SettingsAction)> = NativeTextContrast::ALL
                .iter()
                .map(|mode| {
                    (
                        localized_text_contrast_label(*mode),
                        SettingsAction::SetTextContrast(*mode),
                    )
                })
                .collect();
            let selected = NativeTextContrast::ALL
                .iter()
                .position(|mode| *mode == this.native_settings.terminal.text_contrast)
                .unwrap_or(0);
            let (tx, tw) = this.paint_row_tile(
                layers,
                row_x,
                rows.y,
                row_width,
                SvgIcon::Contrast,
                TileColor::Yellow,
            )?;
            rows.add(this.paint_segmented_setting_row(
                layers,
                tx,
                rows.y,
                tw,
                &crate::i18n::tr("settings-text-contrast"),
                &crate::i18n::tr("settings-text-contrast-description"),
                &options,
                selected,
                rows.rule(),
            )?);

            let options: Vec<(String, SettingsAction)> = NativeRemotePaneResizeMode::ALL
                .iter()
                .map(|mode| {
                    (
                        localized_remote_pane_resize_mode_label(*mode),
                        SettingsAction::SetRemotePaneResizeMode(*mode),
                    )
                })
                .collect();
            let selected = NativeRemotePaneResizeMode::ALL
                .iter()
                .position(|mode| *mode == this.native_settings.terminal.remote_pane_resize_mode)
                .unwrap_or(0);
            let (tx, tw) = this.paint_row_tile(
                layers,
                row_x,
                rows.y,
                row_width,
                SvgIcon::Columns2,
                TileColor::Blue,
            )?;
            rows.add(this.paint_segmented_setting_row(
                layers,
                tx,
                rows.y,
                tw,
                &crate::i18n::tr("settings-remote-pane-resize-mode"),
                &crate::i18n::tr("settings-remote-pane-resize-mode-description"),
                &options,
                selected,
                rows.rule(),
            )?);
            Ok(rows.bottom)
        })?;

        let quote_font_size_label = self.bottom_quote_font_size_label();
        let quote_interval_label = self.bottom_quote_interval_label();
        let card_y = self.paint_card_heading(
            layers,
            x,
            bottom,
            max_width,
            &crate::i18n::tr("settings-terminal-quote-heading"),
        )?;
        let bottom = self.paint_card(layers, x, card_y, max_width, |this, layers, top| {
            let preview_bottom = this.paint_quote_preview(layers, row_x, top, row_width)?;
            let rule_y = preview_bottom + this.ui_px(24.0);
            this.paint_separator(layers, row_x, rule_y, row_width)?;
            let mut rows = RowCursor::new(rule_y + this.ui_px(28.0), this);

            let (tx, tw) = this.paint_row_tile(
                layers,
                row_x,
                rows.y,
                row_width,
                SvgIcon::MessageSquareQuote,
                TileColor::Orange,
            )?;
            rows.add(this.paint_toggle_setting_row(
                layers,
                tx,
                rows.y,
                tw,
                &crate::i18n::tr("settings-bottom-quote"),
                &crate::i18n::tr("settings-bottom-quote-description"),
                this.native_settings.terminal.bottom_quote_enabled,
                SettingsAction::ToggleBottomQuote,
                rows.rule(),
            )?);

            let (tx, tw) = this.paint_row_tile(
                layers,
                row_x,
                rows.y,
                row_width,
                SvgIcon::ALargeSmall,
                TileColor::Orange,
            )?;
            rows.add(this.paint_font_size_stepper_row(
                layers,
                tx,
                rows.y,
                tw,
                &crate::i18n::tr("settings-quote-font-size"),
                &crate::i18n::tr("settings-quote-font-size-description"),
                this.current_bottom_quote_font_size(),
                Some(&quote_font_size_label),
                SettingsAction::ResetBottomQuoteFontSize,
                SettingsAction::DecreaseBottomQuoteFontSize,
                SettingsAction::IncreaseBottomQuoteFontSize,
                rows.rule(),
            )?);

            let options: Vec<(String, SettingsAction)> = NativeBottomQuoteMode::ALL
                .iter()
                .map(|mode| {
                    (
                        localized_quote_mode_label(*mode),
                        SettingsAction::SetBottomQuoteMode(*mode),
                    )
                })
                .collect();
            let selected = NativeBottomQuoteMode::ALL
                .iter()
                .position(|mode| *mode == this.native_settings.terminal.bottom_quote_mode)
                .unwrap_or(0);
            let (tx, tw) = this.paint_row_tile(
                layers,
                row_x,
                rows.y,
                row_width,
                SvgIcon::Shuffle,
                TileColor::Orange,
            )?;
            rows.add(this.paint_segmented_setting_row(
                layers,
                tx,
                rows.y,
                tw,
                &crate::i18n::tr("settings-quote-rotation"),
                &crate::i18n::tr("settings-quote-rotation-description"),
                &options,
                selected,
                rows.rule(),
            )?);

            let (tx, tw) = this.paint_row_tile(
                layers,
                row_x,
                rows.y,
                row_width,
                SvgIcon::Timer,
                TileColor::Orange,
            )?;
            rows.add(this.paint_font_size_stepper_row(
                layers,
                tx,
                rows.y,
                tw,
                &crate::i18n::tr("settings-quote-interval"),
                &crate::i18n::tr("settings-quote-interval-description"),
                this.current_bottom_quote_interval_minutes() as f64,
                Some(&quote_interval_label),
                SettingsAction::ResetBottomQuoteInterval,
                SettingsAction::DecreaseBottomQuoteInterval,
                SettingsAction::IncreaseBottomQuoteInterval,
                rows.rule(),
            )?);

            let (tx, tw) = this.paint_row_tile(
                layers,
                row_x,
                rows.y,
                row_width,
                SvgIcon::FileCode,
                TileColor::Gray,
            )?;
            rows.add(this.paint_quotes_file_row(layers, tx, rows.y, tw, rows.rule())?);
            Ok(rows.bottom)
        })?;

        self.ui.content_scroll.set_extents(
            self.content_viewport_extent(),
            self.settings_content_extent(bottom + scroll),
        );
        Ok(())
    }

    /// A few lines of a terminal in the terminal's own face, size and
    /// colours, so a change to any of them shows here first. Returns where
    /// the preview ends.
    fn paint_terminal_font_preview(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
    ) -> anyhow::Result<f32> {
        let palette = self.palette();
        let size = self.current_terminal_font_size_value();
        let font = self.preview_font(PreviewFont::Terminal, size);
        let terminal = self.terminal_colors(self.native_settings.appearance.theme_mode);
        let metrics = RenderMetrics::with_font_metrics(&font.metrics());
        let line_height = metrics.cell_size.height as f32;
        let pad = self.ui_px(22.0);
        let height = (pad * 2.0 + line_height * 3.0).round();
        let ground = terminal.background;
        self.draw_rounded_frame(
            layers,
            0,
            x,
            y,
            width,
            height,
            opaque(ground),
            palette.control_border,
            self.ui_px(16.0),
        )?;

        let ink = terminal.foreground;
        let colour = |index: usize| terminal.ansi[index];
        let lines: [&[(&str, LinearRgba)]; 3] = [
            &[
                ("~/project", colour(4)),
                (" $ ", colour(2)),
                ("git status", ink),
            ],
            &[("On branch ", ink), ("main", colour(2))],
            &[("0O 1lI {} [] => != -> ~~ // ::", ink.mul_alpha(0.6))],
        ];
        let text_x = x + pad;
        let room = width - pad * 2.0;
        for (index, segments) in lines.iter().enumerate() {
            let line_y = self.text_top_for(&metrics, y + pad + line_height * index as f32);
            let mut segment_x = text_x;
            for (text, color) in segments.iter() {
                let left = text_x + room - segment_x;
                if left <= 0.0 {
                    break;
                }
                self.draw_text(layers, &font, segment_x, line_y, text, *color, left)?;
                segment_x += self.measure_text_width(&font, text);
            }
            // The cursor, after the command.
            if index == 0 && segment_x + metrics.cell_size.width as f32 <= text_x + room {
                self.draw_rect(
                    layers,
                    0,
                    (segment_x + metrics.cell_size.width as f32 * 0.5).round(),
                    (y + pad + line_height * index as f32).round(),
                    metrics.cell_size.width as f32,
                    line_height,
                    terminal.cursor,
                )?;
            }
        }
        Ok(y + height)
    }

    /// The bottom edge of a terminal with the quote as it will appear there:
    /// the one the terminal would have shown on entering the page, at the
    /// chosen size, in the colour the terminal paints it. Faded while the
    /// quote is off.
    fn paint_quote_preview(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
    ) -> anyhow::Result<f32> {
        let palette = self.palette();
        let font = self.preview_font(PreviewFont::Quote, self.current_bottom_quote_font_size());
        let metrics = RenderMetrics::with_font_metrics(&font.metrics());
        let text_height = metrics.cell_size.height as f32;
        let height = (text_height + self.ui_px(44.0))
            .max(self.ui_px(80.0))
            .round();
        let ground = self
            .terminal_colors(self.native_settings.appearance.theme_mode)
            .background;
        self.draw_rounded_frame(
            layers,
            0,
            x,
            y,
            width,
            height,
            opaque(ground),
            palette.control_border,
            self.ui_px(16.0),
        )?;

        // Read on entering the page and when a quote setting changes, not
        // here: finding it stats and reads the quotes file.
        let quote = self.ui.quote_preview.clone();
        // As `paint_bottom_quote` colours it.
        let color = match crate::native_settings::effective_appearance() {
            Appearance::Light | Appearance::LightHighContrast => {
                LinearRgba::with_srgba(80, 80, 90, 255)
            }
            Appearance::Dark | Appearance::DarkHighContrast => {
                LinearRgba::with_srgba(210, 210, 220, 255)
            }
        };
        let color = if self.native_settings.terminal.bottom_quote_enabled {
            color
        } else {
            color.mul_alpha(0.35)
        };
        let inset = self.ui_px(22.0);
        let room = (width - inset * 2.0).max(0.0);
        let shown = self.text_with_ellipsis(&font, &quote, room);
        let text_width = self.measure_text_width(&font, &shown).min(room);
        self.draw_text(
            layers,
            &font,
            x + width - inset - text_width,
            self.text_top_for(&metrics, y + (height - text_height) / 2.0),
            &shown,
            color,
            room,
        )?;
        Ok(y + height)
    }

    /// Where the quotes are read from, with a button to open the file and
    /// one to put the shipped list back.
    fn paint_quotes_file_row(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
        draw_top_rule: bool,
    ) -> anyhow::Result<f32> {
        let palette = self.palette();
        let ui_font = Rc::clone(&self.ui_font);
        if draw_top_rule {
            self.paint_separator(layers, x, y - self.ui_px(28.0), width)?;
        }
        let open = crate::i18n::tr("settings-quotes-file-open");
        let reset = crate::i18n::tr(if self.ui.confirm_reset_quotes {
            "settings-reset-quotes-json-confirm"
        } else {
            "settings-reset-quotes-json"
        });
        let open_width = self.button_width_for_label(&open, 110.0);
        let reset_width = self.button_width_for_label(&reset, 110.0);
        let reset_x = x + width - reset_width;
        let open_x = reset_x - self.ui_px(12.0) - open_width;
        let control_y = y + self.ui_px(4.0);
        self.draw_button(
            layers,
            open_x,
            control_y,
            open_width,
            &open,
            SettingsAction::OpenBottomQuotesJson,
        )?;
        self.draw_button(
            layers,
            reset_x,
            control_y,
            reset_width,
            &reset,
            SettingsAction::ResetBottomQuotesJson,
        )?;
        let text_width = (open_x - x - self.ui_px(24.0)).max(0.0);
        self.draw_text(
            layers,
            &ui_font,
            x,
            y,
            &crate::i18n::tr("settings-quotes-file"),
            palette.text,
            text_width,
        )?;
        self.draw_row_description(
            layers,
            x,
            y,
            &home_relative(&crate::bottom_quotes::quotes_path()),
            text_width,
        )
    }

    /// The `y` to hand `draw_text` so a line of a font with `metrics`, whose
    /// line box starts at `line_top`, sits on that font's baseline:
    /// `draw_text` places glyphs by this window's own font.
    fn text_top_for(&self, metrics: &RenderMetrics, line_top: f32) -> f32 {
        let own = self.metrics.cell_size.height as f32 + self.metrics.descender.get() as f32;
        let theirs = metrics.cell_size.height as f32 + metrics.descender.get() as f32;
        line_top + theirs - own
    }

    /// A font only the Terminal page shows, built the first time it is
    /// asked for at this size and kept until the page is left
    /// (`paint_content` lets them go). Built from the configuration as it
    /// is now: this window's font configuration is as old as the window.
    /// Falls back to the body font, once logged, when the face cannot be
    /// loaded.
    fn preview_font(&mut self, which: PreviewFont, size: f64) -> Rc<LoadedFont> {
        let key = PreviewFontKey {
            which,
            size: size.to_bits(),
            generation: configuration().generation(),
            dpi: self.fonts.get_dpi(),
        };
        if let Some((_, font)) = self
            .ui
            .preview_fonts
            .iter()
            .find(|(noted, _)| *noted == key)
        {
            return font.clone().unwrap_or_else(|| Rc::clone(&self.body_font));
        }
        let before = self.ui.preview_fonts.len();
        self.ui
            .preview_fonts
            .retain(|(noted, _)| noted.which != which);
        if self.ui.preview_fonts.len() != before {
            // The one it replaces leaves its glyphs in the atlas.
            self.ui.stale_glyphs = true;
        }
        let config = configuration();
        let built = match which {
            PreviewFont::Terminal => self.fonts.terminal_font_uncached(&config, size),
            PreviewFont::Quote => self.fonts.command_palette_font_uncached(&config, size, 500),
        };
        let font = match built {
            Ok(font) => Some(font),
            Err(err) => {
                log::warn!("settings: loading the {which:?} preview font: {err:#}");
                None
            }
        };
        self.ui.preview_fonts.push((key, font.clone()));
        font.unwrap_or_else(|| Rc::clone(&self.body_font))
    }

    fn paint_developer(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        max_width: f32,
    ) -> anyhow::Result<()> {
        let palette = self.palette();
        let body_font = Rc::clone(&self.body_font);
        let scroll = self.ui.content_scroll.offset;
        let row_step = self.settings_row_step();
        let section_y = self.ui_px(CONTENT_SECTION_Y) - scroll;
        self.draw_text(
            layers,
            &body_font,
            x,
            section_y,
            &crate::i18n::tr("settings-developer-description"),
            palette.secondary_text,
            max_width,
        )?;

        let (card_y, first_row_y) = self.settings_card_geometry(section_y, 4);
        let card_height = self.settings_card_height(4);
        let button_y = card_y + card_height + self.settings_section_card_gap();

        let card_padding = self.ui_px(36.0);
        let row_x = x + card_padding;
        let row_width = max_width - card_padding * 2.0;
        let developer_tabs_value = if self.developer_mode_enabled() {
            crate::i18n::tr("settings-developer-tabs-value")
        } else {
            crate::i18n::tr("common-hidden")
        };
        self.paint_group_card(layers, x, card_y, max_width, card_height)?;
        self.paint_toggle_setting_row(
            layers,
            row_x,
            first_row_y,
            row_width,
            &crate::i18n::tr("settings-developer-mode"),
            &crate::i18n::tr("settings-developer-mode-description"),
            self.developer_mode_enabled(),
            SettingsAction::ToggleDeveloperMode,
            false,
        )?;
        self.paint_toggle_setting_row(
            layers,
            row_x,
            first_row_y + row_step,
            row_width,
            &crate::i18n::tr("settings-fallback-menu"),
            &crate::i18n::tr("settings-fallback-menu-description"),
            self.native_settings.developer.force_fallback_context_menu,
            SettingsAction::ToggleFallbackContextMenu,
            true,
        )?;
        self.paint_setting_row(
            layers,
            row_x,
            first_row_y + row_step * 2.0,
            row_width,
            &crate::i18n::tr("settings-developer-tabs"),
            &crate::i18n::tr("settings-developer-tabs-description"),
            &developer_tabs_value,
            false,
        )?;
        self.paint_setting_row(
            layers,
            row_x,
            first_row_y + row_step * 3.0,
            row_width,
            &crate::i18n::tr("settings-onboarding-launcher"),
            &crate::i18n::tr("settings-onboarding-launcher-description"),
            &crate::i18n::tr("settings-onboarding-launcher-value"),
            true,
        )?;
        let show_onboarding_label = crate::i18n::tr("settings-show-onboarding");
        self.draw_button(
            layers,
            x,
            button_y,
            self.button_width_for_label(&show_onboarding_label, 300.0),
            &show_onboarding_label,
            SettingsAction::ShowOnboardingNow,
        )?;

        self.ui.content_scroll.set_extents(
            self.content_viewport_extent(),
            self.settings_content_extent(button_y + scroll + self.ui_px(CONTROL_HEIGHT)),
        );
        if self.ui.content_scroll.offset != scroll {
            if let Some(window) = &self.window {
                window.invalidate();
            }
        }

        Ok(())
    }

    fn paint_ui_kit(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        max_width: f32,
    ) -> anyhow::Result<()> {
        let palette = self.palette();
        let ui_font = Rc::clone(&self.ui_font);
        let body_font = Rc::clone(&self.body_font);
        let two_column = max_width >= 940.0;
        let content_extent = if two_column { 1020.0 } else { 1660.0 };
        self.ui
            .content_scroll
            .set_extents(self.content_bottom(), content_extent);
        let scroll = self.ui.content_scroll.offset;

        self.draw_text(
            layers,
            &body_font,
            x,
            self.ui_px(CONTENT_SECTION_Y) - scroll,
            "Live settings UI components. The left side is rendered with the same primitives as the real window.",
            palette.secondary_text,
            max_width,
        )?;
        self.draw_rect(
            layers,
            0,
            x,
            self.ui_px(CONTENT_RULE_Y) - scroll,
            max_width,
            1.0,
            palette.rule,
        )?;

        let preview_width = if two_column {
            (max_width * 0.52).min(self.ui_px(620.0))
        } else {
            max_width
        };
        let notes_x = if two_column {
            x + preview_width + 42.0
        } else {
            x
        };
        let notes_width = if two_column {
            (max_width - preview_width - self.ui_px(42.0)).max(self.ui_px(280.0))
        } else {
            max_width
        };
        let notes_top = if two_column { 244.0 } else { 900.0 };

        self.draw_text(
            layers,
            &ui_font,
            x,
            244.0 - scroll,
            "Component Preview",
            palette.title,
            preview_width,
        )?;
        self.draw_rect(
            layers,
            0,
            x,
            274.0 - scroll,
            preview_width,
            1.0,
            palette.rule,
        )?;
        self.paint_preview_search(layers, x, 298.0 - scroll, preview_width)?;

        self.draw_text(
            layers,
            &ui_font,
            x,
            382.0 - scroll,
            "Sidebar Rows",
            palette.title,
            preview_width,
        )?;
        self.paint_preview_sidebar_row(layers, x, 416.0 - scroll, preview_width, "General", false)?;
        self.paint_preview_sidebar_row(
            layers,
            x,
            470.0 - scroll,
            preview_width,
            "Appearance",
            true,
        )?;

        self.draw_text(
            layers,
            &ui_font,
            x,
            540.0 - scroll,
            "Buttons and Controls",
            palette.title,
            preview_width,
        )?;
        self.paint_preview_button(layers, x, 578.0 - scroll, 250.0, "Normal Button", false)?;
        self.paint_preview_button(
            layers,
            x + self.ui_px(270.0),
            578.0 - scroll,
            self.ui_px(220.0),
            "Accent Button",
            true,
        )?;
        self.paint_preview_control(layers, x, 648.0 - scroll, 280.0, "System")?;

        self.draw_text(
            layers,
            &ui_font,
            x,
            730.0 - scroll,
            "Setting Row",
            palette.title,
            preview_width,
        )?;
        self.paint_setting_row(
            layers,
            x,
            800.0 - scroll,
            preview_width,
            &crate::i18n::tr("settings-theme-mode"),
            "Follow macOS appearance.",
            "System",
            false,
        )?;

        let component_tokens = [
            StyleToken {
                name: "search.field",
                value: "h56 bg+focus border",
                swatch: Some(palette.search_bg),
            },
            StyleToken {
                name: "nav.active.row",
                value: "h48 system selection",
                swatch: Some(palette.nav_selected_bg),
            },
            StyleToken {
                name: "button.normal",
                value: "h56 rounded hover",
                swatch: Some(palette.control_bg),
            },
            StyleToken {
                name: "button.accent",
                value: "h56 active bg",
                swatch: Some(palette.control_pressed_bg),
            },
            StyleToken {
                name: "control.select",
                value: "h56 rounded select",
                swatch: Some(palette.control_bg),
            },
            StyleToken {
                name: "setting.row",
                value: "rule + label/value",
                swatch: Some(palette.rule),
            },
        ];

        let layout_tokens = [
            StyleToken {
                name: "window",
                value: "1360 x 860",
                swatch: None,
            },
            StyleToken {
                name: "sidebar",
                value: "340 px",
                swatch: None,
            },
            StyleToken {
                name: "content",
                value: "responsive gap / 132 header",
                swatch: None,
            },
            StyleToken {
                name: "rows",
                value: "nav48/56 setting132",
                swatch: None,
            },
        ];

        let cell_size = format!(
            "{} x {} px",
            self.metrics.cell_size.width, self.metrics.cell_size.height
        );
        let dpi = format!("{} dpi", self.dimensions.dpi);
        let type_tokens = [
            StyleToken {
                name: "ui.font",
                value: "macOS title font",
                swatch: None,
            },
            StyleToken {
                name: "title.font",
                value: "window title font",
                swatch: None,
            },
            StyleToken {
                name: "cell size",
                value: &cell_size,
                swatch: None,
            },
            StyleToken {
                name: "window dpi",
                value: &dpi,
                swatch: None,
            },
        ];

        self.paint_style_group(
            layers,
            notes_x,
            notes_top - scroll,
            notes_width,
            "Component Styles",
            &component_tokens,
        )?;
        self.paint_style_group(
            layers,
            notes_x,
            notes_top + self.ui_px(330.0) - scroll,
            notes_width,
            "Typography",
            &type_tokens,
        )?;
        self.paint_style_group(
            layers,
            notes_x,
            notes_top + self.ui_px(590.0) - scroll,
            notes_width,
            "Layout",
            &layout_tokens,
        )?;

        Ok(())
    }

    fn paint_memory_diagnostics(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        max_width: f32,
    ) -> anyhow::Result<()> {
        let palette = self.palette();
        let ui_font = Rc::clone(&self.ui_font);
        let body_font = Rc::clone(&self.body_font);
        let scroll = self.ui.content_scroll.offset;
        let row_step = self.settings_row_step();
        let section_y = self.ui_px(CONTENT_SECTION_Y) - scroll;
        let row_count = 15;
        let (card_y, first_row_y) = self.settings_card_geometry(section_y, row_count);
        let card_height = self.settings_card_height(row_count);
        let button_y = card_y + card_height + self.settings_section_card_gap();

        self.draw_text(
            layers,
            &body_font,
            x,
            section_y,
            "Memory and input diagnostics are manual. Developer mode only reveals this page; sampling starts when you enable it here.",
            palette.secondary_text,
            max_width,
        )?;

        let snapshot = self
            .ui
            .memory_snapshot
            .clone()
            .unwrap_or_else(|| capture_memory_snapshot(false));
        if self.ui.memory_snapshot.is_none() {
            self.ui.memory_snapshot = Some(snapshot.clone());
        }
        let age_label = format!("{:.1}s ago", snapshot.captured_at.elapsed().as_secs_f32());
        let footprint = snapshot
            .secondary_metric()
            .1
            .map(format_bytes)
            .unwrap_or_else(|| "Unavailable".to_string());
        let rss = snapshot
            .resident_metric()
            .1
            .map(format_bytes)
            .unwrap_or_else(|| "Unavailable".to_string());
        let peak = snapshot
            .peak_metric()
            .1
            .map(format_bytes)
            .unwrap_or_else(|| "Unavailable".to_string());
        // The two platforms report different measures here, not the same one
        // under different names, so the row titles have to differ too.
        let (footprint_title, footprint_help, rss_title, rss_help, peak_title, peak_help) =
            if cfg!(windows) {
                (
                    "Commit (Private Bytes)",
                    "Committed private bytes. This is NOT resident memory -- it is Task Manager's \"Commit size\" column.",
                    "Working Set",
                    "Resident process memory: Task Manager's default \"Memory\" column.",
                    "Peak Working Set",
                    "Highest working set reached by this process.",
                )
            } else {
                (
                    "Physical Footprint",
                    "Matches the macOS memory pressure number more closely than RSS.",
                    "Resident Size",
                    "Current resident process memory from proc_pid_rusage.",
                    "Peak Physical Footprint",
                    "Highest physical footprint reported for this process lifetime.",
                )
            };
        // vmmap is macOS-only, so on every other platform these rows can never
        // fill in. Telling the user to press Refresh would be an instruction
        // that does nothing, forever.
        let no_breakdown = if cfg!(target_os = "macos") {
            "Run Refresh Now"
        } else {
            "Unavailable on this platform"
        };
        let total_resident = snapshot
            .vmmap_total_resident
            .map(format_bytes)
            .unwrap_or_else(|| no_breakdown.to_string());
        let graphics = snapshot
            .vmmap_graphics_resident
            .map(format_bytes)
            .unwrap_or_else(|| no_breakdown.to_string());
        let iosurface = snapshot
            .vmmap_iosurface_resident
            .map(format_bytes)
            .unwrap_or_else(|| no_breakdown.to_string());
        let malloc = snapshot
            .vmmap_malloc_resident
            .map(format_bytes)
            .unwrap_or_else(|| no_breakdown.to_string());
        let text = snapshot
            .vmmap_text_resident
            .map(format_bytes)
            .unwrap_or_else(|| no_breakdown.to_string());
        let breakdown_status = if snapshot.has_vmmap_breakdown() {
            "Captured"
        } else if snapshot.vmmap_error.is_some() {
            "Unavailable"
        } else if cfg!(target_os = "macos") {
            "Refresh for vmmap"
        } else {
            "macOS only"
        };
        let input = crate::input_diagnostics::snapshot();
        let input_events = format!("{} events", input.key_events);
        let input_p95 = crate::input_diagnostics::format_duration(input.recent_p95);
        let input_avg = crate::input_diagnostics::format_duration(input.average_duration());
        let input_slowest = input
            .slowest_stage
            .as_ref()
            .map(|stage| {
                format!(
                    "{} {}",
                    stage.name,
                    crate::input_diagnostics::format_duration(stage.max_duration)
                )
            })
            .unwrap_or_else(|| "No stage samples".to_string());
        let resource_lines = self.memory_resource_lines();
        let input_button_y = button_y + self.ui_px(CONTROL_HEIGHT) + 16.0;
        let resource_card_y =
            input_button_y + self.ui_px(CONTROL_HEIGHT) + self.settings_section_card_gap();
        let resource_line_height = 30.0;
        let resource_card_height = 88.0 + resource_line_height * resource_lines.len() as f32;
        self.ui.content_scroll.set_extents(
            self.content_viewport_extent(),
            self.settings_content_extent(resource_card_y + scroll + resource_card_height),
        );

        let card_padding = self.ui_px(36.0);
        let row_x = x + card_padding;
        let row_width = max_width - card_padding * 2.0;
        self.paint_group_card(layers, x, card_y, max_width, card_height)?;
        self.paint_toggle_setting_row(
            layers,
            row_x,
            first_row_y,
            row_width,
            "Manual Sampling",
            "Keeps refreshing this page until you turn it off.",
            self.ui.memory_monitoring,
            SettingsAction::ToggleMemoryMonitoring,
            false,
        )?;
        self.paint_setting_row(
            layers,
            row_x,
            first_row_y + row_step,
            row_width,
            footprint_title,
            footprint_help,
            &footprint,
            true,
        )?;
        self.paint_setting_row(
            layers,
            row_x,
            first_row_y + row_step * 2.0,
            row_width,
            rss_title,
            rss_help,
            &rss,
            true,
        )?;
        self.paint_setting_row(
            layers,
            row_x,
            first_row_y + row_step * 3.0,
            row_width,
            peak_title,
            peak_help,
            &peak,
            true,
        )?;
        self.paint_setting_row(
            layers,
            row_x,
            first_row_y + row_step * 4.0,
            row_width,
            "Last Snapshot",
            "Manual refresh and copy use this latest captured value.",
            &age_label,
            true,
        )?;
        self.paint_setting_row(
            layers,
            row_x,
            first_row_y + row_step * 5.0,
            row_width,
            "vmmap Breakdown",
            "Refresh Now captures the slower macOS breakdown; sampling keeps this lightweight.",
            breakdown_status,
            true,
        )?;
        self.paint_setting_row(
            layers,
            row_x,
            first_row_y + row_step * 6.0,
            row_width,
            "Activity Monitor Resident",
            "vmmap TOTAL resident; this is the scary-looking number Activity Monitor can resemble.",
            &total_resident,
            true,
        )?;
        self.paint_setting_row(
            layers,
            row_x,
            first_row_y + row_step * 7.0,
            row_width,
            "Graphics Surfaces",
            "IOSurface + IOAccelerator graphics + owned unmapped graphics.",
            &graphics,
            true,
        )?;
        self.paint_setting_row(
            layers,
            row_x,
            first_row_y + row_step * 8.0,
            row_width,
            "IOSurface",
            "macOS window backing surfaces and swapchain-style drawable storage.",
            &iosurface,
            true,
        )?;
        self.paint_setting_row(
            layers,
            row_x,
            first_row_y + row_step * 9.0,
            row_width,
            "Allocator Heap",
            "MALLOC resident from vmmap; Rust allocations mostly land here.",
            &malloc,
            true,
        )?;
        self.paint_setting_row(
            layers,
            row_x,
            first_row_y + row_step * 10.0,
            row_width,
            "Text Segments",
            "__TEXT resident pages from the app, dependencies, and system libraries.",
            &text,
            true,
        )?;
        self.paint_toggle_setting_row(
            layers,
            row_x,
            first_row_y + row_step * 11.0,
            row_width,
            "Input Diagnostics",
            "Manual tracing for long-running typing latency. No key text is stored.",
            input.enabled,
            SettingsAction::ToggleInputDiagnostics,
            true,
        )?;
        self.paint_setting_row(
            layers,
            row_x,
            first_row_y + row_step * 12.0,
            row_width,
            "Key Event Samples",
            "Counts events seen since diagnostics started or reset.",
            &input_events,
            true,
        )?;
        self.paint_setting_row(
            layers,
            row_x,
            first_row_y + row_step * 13.0,
            row_width,
            "Input Event Avg / P95",
            "Wall time spent in the key event path.",
            &format!("{input_avg} / {input_p95}"),
            true,
        )?;
        self.paint_setting_row(
            layers,
            row_x,
            first_row_y + row_step * 14.0,
            row_width,
            "Slowest Stage",
            "The slowest recorded sub-stage so far.",
            &input_slowest,
            true,
        )?;
        if let Some(error) = &snapshot.vmmap_error {
            self.draw_text(
                layers,
                &body_font,
                row_x,
                first_row_y + row_step * 15.0,
                error,
                palette.muted_text,
                row_width,
            )?;
        }
        let refresh_x = x;
        self.draw_button(
            layers,
            refresh_x,
            button_y,
            self.button_width_for_label("Refresh Now", 210.0),
            "Refresh Now",
            SettingsAction::RefreshMemorySnapshot,
        )?;
        let copied = self
            .ui
            .memory_snapshot_copied_until
            .is_some_and(|until| Instant::now() < until);
        let copy_label = if copied { "Copied" } else { "Copy" };
        let copy_x =
            refresh_x + self.button_width_for_label("Refresh Now", 210.0) + self.ui_px(16.0);
        self.draw_button(
            layers,
            copy_x,
            button_y,
            self.button_width_for_label(copy_label, 150.0),
            copy_label,
            SettingsAction::CopyMemorySnapshot,
        )?;
        let reset_input_x = x;
        self.draw_button(
            layers,
            reset_input_x,
            input_button_y,
            self.button_width_for_label("Reset Input", 190.0),
            "Reset Input",
            SettingsAction::ResetInputDiagnostics,
        )?;
        let input_copied = self
            .ui
            .input_diagnostics_copied_until
            .is_some_and(|until| Instant::now() < until);
        let input_copy_label = if input_copied { "Copied" } else { "Copy Input" };
        let copy_input_x =
            reset_input_x + self.button_width_for_label("Reset Input", 190.0) + self.ui_px(16.0);
        self.draw_button(
            layers,
            copy_input_x,
            input_button_y,
            self.button_width_for_label(input_copy_label, 180.0),
            input_copy_label,
            SettingsAction::CopyInputDiagnostics,
        )?;

        self.paint_group_card(layers, x, resource_card_y, max_width, resource_card_height)?;
        self.draw_text(
            layers,
            &ui_font,
            row_x,
            resource_card_y + self.ui_px(28.0),
            "Renderer Resources",
            palette.title,
            row_width,
        )?;
        for (idx, line) in resource_lines.iter().enumerate() {
            self.draw_text(
                layers,
                &body_font,
                row_x,
                resource_card_y + self.ui_px(66.0) + resource_line_height * idx as f32,
                line,
                palette.secondary_text,
                row_width,
            )?;
        }

        Ok(())
    }

    fn paint_preview_search(
        &self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
    ) -> anyhow::Result<()> {
        let palette = self.palette();
        let ui_font = Rc::clone(&self.ui_font);
        let body_font = Rc::clone(&self.body_font);
        self.draw_text(layers, &ui_font, x, y, "Search Field", palette.title, width)?;
        self.draw_rounded_frame(
            layers,
            0,
            x,
            y + self.ui_px(28.0),
            width.min(self.ui_px(420.0)),
            self.ui_px(CONTROL_HEIGHT),
            palette.search_bg,
            palette.search_border,
            self.ui_px(CONTROL_RADIUS),
        )?;
        self.draw_text(
            layers,
            &body_font,
            x + self.ui_px(18.0),
            self.control_text_y(y + 28.0, self.ui_px(CONTROL_HEIGHT)),
            "Search settings...",
            palette.muted_text,
            width.min(self.ui_px(420.0)) - self.ui_px(36.0),
        )?;
        Ok(())
    }

    fn paint_preview_sidebar_row(
        &self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
        label: &str,
        selected: bool,
    ) -> anyhow::Result<()> {
        let palette = self.palette();
        let body_font = Rc::clone(&self.body_font);
        let row_width = width.min(self.ui_px(420.0));
        if selected {
            self.draw_rounded_rect(
                layers,
                0,
                x,
                y,
                row_width,
                self.nav_row_height(),
                palette.nav_selected_bg,
                self.ui_px(NAV_ROW_RADIUS),
            )?;
        } else {
            self.draw_rounded_rect(
                layers,
                0,
                x,
                y,
                row_width,
                self.nav_row_height(),
                palette.nav_hover_bg,
                self.ui_px(NAV_ROW_RADIUS),
            )?;
        }
        self.draw_text(
            layers,
            &body_font,
            x + self.ui_px(16.0),
            self.control_text_y(y, self.nav_row_height()),
            label,
            if selected {
                palette.selected_text
            } else {
                palette.secondary_text
            },
            row_width - 32.0,
        )?;
        Ok(())
    }

    fn paint_preview_button(
        &self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
        label: &str,
        accent: bool,
    ) -> anyhow::Result<()> {
        let palette = self.palette();
        let width = width.max(self.button_width_for_label(label, 0.0));
        let bg = if accent {
            palette.nav_selected_bg
        } else {
            palette.control_bg
        };
        self.draw_rounded_frame(
            layers,
            0,
            x,
            y,
            width,
            self.ui_px(CONTROL_HEIGHT),
            bg,
            palette.control_border,
            self.ui_px(CONTROL_RADIUS),
        )?;
        self.draw_text(
            layers,
            &Rc::clone(&self.ui_font),
            x + self.ui_px(18.0),
            self.control_text_y(y, self.ui_px(CONTROL_HEIGHT)),
            label,
            if accent {
                palette.selected_text
            } else {
                palette.text
            },
            width - self.ui_px(36.0),
        )?;
        Ok(())
    }

    fn paint_preview_control(
        &self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
        value: &str,
    ) -> anyhow::Result<()> {
        let palette = self.palette();
        self.draw_rounded_frame(
            layers,
            0,
            x,
            y,
            width,
            self.ui_px(CONTROL_HEIGHT),
            palette.control_bg,
            palette.control_border,
            self.ui_px(CONTROL_RADIUS),
        )?;
        self.draw_text(
            layers,
            &Rc::clone(&self.ui_font),
            x + self.ui_px(14.0),
            self.control_text_y(y, self.ui_px(CONTROL_HEIGHT)),
            value,
            palette.text,
            width - 42.0,
        )?;
        self.draw_text(
            layers,
            &Rc::clone(&self.ui_font),
            x + width - self.ui_px(28.0),
            self.control_text_y(y, self.ui_px(CONTROL_HEIGHT)),
            "v",
            palette.muted_text,
            self.ui_px(14.0),
        )?;
        Ok(())
    }

    fn paint_style_group(
        &self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
        title: &str,
        tokens: &[StyleToken<'_>],
    ) -> anyhow::Result<()> {
        let palette = self.palette();
        self.draw_text(
            layers,
            &Rc::clone(&self.ui_font),
            x,
            y,
            title,
            palette.title,
            width,
        )?;
        self.paint_separator(layers, x, y + self.ui_px(28.0), width)?;

        let mut row_y = y + self.ui_px(58.0);
        for token in tokens {
            self.paint_style_token(
                layers,
                x,
                row_y,
                width,
                token.name,
                token.value,
                token.swatch,
            )?;
            row_y += 40.0;
        }

        Ok(())
    }

    fn paint_style_token(
        &self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
        name: &str,
        value: &str,
        swatch: Option<LinearRgba>,
    ) -> anyhow::Result<()> {
        let palette = self.palette();
        let name_x = if let Some(color) = swatch {
            self.draw_rect(layers, 0, x, y + 3.0, 14.0, 14.0, color)?;
            self.draw_rect(layers, 0, x, y + 3.0, 14.0, 1.0, rgba(255, 255, 255, 0.18))?;
            x + 22.0
        } else {
            x
        };
        let value_x = x + width * 0.48;
        self.draw_text(
            layers,
            &Rc::clone(&self.ui_font),
            name_x,
            y,
            name,
            palette.secondary_text,
            (value_x - name_x - 12.0).max(80.0),
        )?;
        self.draw_text(
            layers,
            &Rc::clone(&self.ui_font),
            value_x,
            y,
            value,
            palette.text,
            (x + width - value_x).max(80.0),
        )?;
        Ok(())
    }

    fn paint_backup(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        max_width: f32,
    ) -> anyhow::Result<()> {
        let palette = self.palette();
        let body_font = Rc::clone(&self.body_font);
        let scroll = self.ui.content_scroll.offset;
        let row_step = self.settings_row_step();
        let section_y = self.ui_px(CONTENT_SECTION_Y) - scroll;
        let staged = crate::state_backup::import_staged();
        let failed = crate::state_backup::last_import_error();
        let row_count = 2 + usize::from(staged) + usize::from(failed.is_some());
        let (card_y, first_row_y) = self.settings_card_geometry(section_y, row_count);
        let card_height = self.settings_card_height(row_count);
        self.ui.content_scroll.set_extents(
            self.content_viewport_extent(),
            self.settings_content_extent(card_y + scroll + card_height),
        );
        self.draw_text(
            layers,
            &body_font,
            x,
            section_y,
            &crate::i18n::tr("settings-backup-description"),
            palette.secondary_text,
            max_width,
        )?;
        let card_padding = self.ui_px(36.0);
        let row_x = x + card_padding;
        let row_width = max_width - card_padding * 2.0;
        self.paint_group_card(layers, x, card_y, max_width, card_height)?;
        self.paint_action_setting_row(
            layers,
            row_x,
            first_row_y,
            row_width,
            &crate::i18n::tr("settings-backup-export"),
            &crate::i18n::tr("settings-backup-export-description"),
            &crate::i18n::tr("settings-backup-export-button"),
            SettingsAction::ExportBackup,
            false,
        )?;
        self.paint_action_setting_row(
            layers,
            row_x,
            first_row_y + row_step,
            row_width,
            &crate::i18n::tr("settings-backup-import"),
            &crate::i18n::tr("settings-backup-import-description"),
            &crate::i18n::tr("settings-backup-import-button"),
            SettingsAction::ImportBackup,
            true,
        )?;
        let mut next_row_y = first_row_y + row_step * 2.0;
        if staged {
            self.paint_action_setting_row(
                layers,
                row_x,
                next_row_y,
                row_width,
                &crate::i18n::tr("settings-backup-import-ready"),
                &crate::i18n::tr("settings-backup-import-ready-description"),
                &crate::i18n::tr("settings-backup-quit-button"),
                SettingsAction::QuitApplication,
                true,
            )?;
            next_row_y += row_step;
        }
        if let Some(error) = failed {
            self.paint_setting_row(
                layers,
                row_x,
                next_row_y,
                row_width,
                &crate::i18n::tr("settings-backup-import-failed"),
                &error,
                "",
                true,
            )?;
        }
        Ok(())
    }

    /// Row geometry for the two app-level pages. Unlike `settings_row_step`
    /// these rows carry no explanation line, so they are a single line tall
    /// -- the shape macOS System Settings uses for a list of plain facts.
    fn compact_row_step(&self) -> f32 {
        let cell_height = self.metrics.cell_size.height as f32;
        (cell_height + self.ui_px(48.0))
            .max(self.ui_px(84.0))
            .ceil()
    }

    fn compact_card_height(&self, rows: usize) -> f32 {
        if rows == 0 {
            return 0.0;
        }
        self.compact_row_step() * rows as f32 + self.ui_px(24.0)
    }

    /// `label ............ value`, both on one line, the value flush right in
    /// the secondary weight. `y` is the top of the row band, not the text.
    fn paint_compact_row(
        &self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
        label: &str,
        value: &str,
        draw_top_rule: bool,
    ) -> anyhow::Result<()> {
        let palette = self.palette();
        let ui_font = Rc::clone(&self.ui_font);
        let body_font = Rc::clone(&self.body_font);
        let step = self.compact_row_step();
        if draw_top_rule {
            self.paint_separator(layers, x, y, width)?;
        }
        let text_y = self.control_text_y(y, step);

        // The value takes what it needs from the right; the label gets the
        // rest, so a long value shortens the label rather than overlapping.
        let value_width = self
            .measure_text_width(&body_font, value)
            .min(width * 0.62)
            .ceil();
        self.draw_text(
            layers,
            &ui_font,
            x,
            text_y,
            label,
            palette.text,
            (width - value_width - self.ui_px(20.0)).max(0.0),
        )?;
        self.draw_text(
            layers,
            &body_font,
            x + width - value_width,
            text_y,
            value,
            palette.secondary_text,
            value_width,
        )
    }

    /// The application icon at `size`, from the packaged PNG rather than the
    /// SVG set: it is the app's real mark, and About is where that matters.
    fn draw_app_icon(
        &self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        size: f32,
    ) -> anyhow::Result<()> {
        if size <= 0.0 {
            return Ok(());
        }
        let render_state = self.render_state.as_ref().unwrap();
        let sprite = render_state
            .glyph_cache
            .borrow_mut()
            .cached_app_icon(size.round() as usize)?
            .texture_coords();
        self.draw_picture(layers, sprite, x, y, size)
    }

    /// One of the app icons Appearance offers, from its small copy.
    fn draw_app_icon_choice(
        &self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        icon: NativeAppIcon,
        x: f32,
        y: f32,
        size: f32,
    ) -> anyhow::Result<()> {
        if size <= 0.0 {
            return Ok(());
        }
        let render_state = self.render_state.as_ref().unwrap();
        let sprite = render_state
            .glyph_cache
            .borrow_mut()
            .cached_app_icon_choice(icon, size.round() as usize)?
            .texture_coords();
        self.draw_picture(layers, sprite, x, y, size)
    }

    /// A full-colour sprite `size` square at `x`, `y`, as it is: not
    /// tinted like the SVG icons.
    fn draw_picture(
        &self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        sprite: ::window::bitmaps::TextureRect,
        x: f32,
        y: f32,
        size: f32,
    ) -> anyhow::Result<()> {
        let mut quad = layers.allocate(2)?;
        let left_offset = self.dimensions.pixel_width as f32 / 2.0;
        let top_offset = self.dimensions.pixel_height as f32 / 2.0;
        quad.set_position(
            x - left_offset,
            y - top_offset,
            x + size - left_offset,
            y + size - top_offset,
        );
        quad.set_texture(sprite);
        let white = LinearRgba::with_components(1.0, 1.0, 1.0, 1.0);
        quad.set_fg_color(white);
        quad.set_alt_color_and_mix_value(white, 0.0);
        quad.set_hsv(None);
        quad.set_has_color(true);
        Ok(())
    }

    /// The CloseX logo, `height` tall with its left edge at `x`, tinted like
    /// an SVG icon so it reads black on light and white on dark. Returns the
    /// width it took.
    fn draw_closex_logo(
        &self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        height: f32,
        color: LinearRgba,
    ) -> anyhow::Result<f32> {
        if height <= 0.0 {
            return Ok(0.0);
        }
        let render_state = self.render_state.as_ref().unwrap();
        let sprite = render_state
            .glyph_cache
            .borrow_mut()
            .cached_closex_logo(height.round() as usize)?;
        let width = sprite.coords.size.width as f32;
        let height = sprite.coords.size.height as f32;
        let x = x.round();
        let mut quad = layers.allocate(2)?;
        let left_offset = self.dimensions.pixel_width as f32 / 2.0;
        let top_offset = self.dimensions.pixel_height as f32 / 2.0;
        quad.set_position(
            x - left_offset,
            y - top_offset,
            x + width - left_offset,
            y + height - top_offset,
        );
        quad.set_texture(sprite.texture_coords());
        quad.set_fg_color(color);
        quad.set_alt_color_and_mix_value(color, 0.0);
        quad.set_hsv(None);
        quad.set_has_color(false);
        quad.set_grayscale();
        Ok(width)
    }

    /// A wrapping row of pill buttons, the way LaunchNext and macOS put a
    /// page's outbound links along the bottom instead of one per row.
    /// Returns the height it consumed.
    fn paint_button_row(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        max_width: f32,
        buttons: &[(String, SettingsAction)],
    ) -> anyhow::Result<f32> {
        let gap = self.ui_px(14.0);
        let height = self.ui_px(CONTROL_HEIGHT);
        let mut cursor_x = x;
        let mut cursor_y = y;
        for (label, action) in buttons {
            let width = self.button_width_for_label(label, 0.0);
            if cursor_x > x && cursor_x + width > x + max_width {
                cursor_x = x;
                cursor_y += height + gap;
            }
            self.draw_button(layers, cursor_x, cursor_y, width, label, *action)?;
            cursor_x += width + gap;
        }
        Ok(cursor_y + height - y)
    }

    /// What the last check found, from the cache every check writes. An
    /// install or a check in flight is said over it; see `paint_update`.
    fn update_hero(&self) -> UpdateHero {
        let Some(status) = self.ui.update_status.as_ref() else {
            return UpdateHero::Unknown;
        };
        if !status.running_a_release_build() {
            return UpdateHero::LocalBuild;
        }
        match (&status.latest, status.update_available) {
            (Some(latest), true) => UpdateHero::Available {
                tag: latest.tag_name.clone(),
            },
            (Some(_), false) => UpdateHero::UpToDate,
            (None, _) => UpdateHero::Unknown,
        }
    }

    /// One localized sentence on how this copy gets updated, from the
    /// detected install method.
    fn update_method_note(&self) -> String {
        use thinkterm_update::InstallMethod;
        match self.ui.update_method.as_ref() {
            Some(InstallMethod::Script(m)) => settings_tr(
                "settings-update-method-script",
                &[("variant", m.variant.clone())],
            ),
            Some(InstallMethod::MacAppBundle(app)) => settings_tr(
                "settings-update-method-macapp",
                &[("path", app.display().to_string())],
            ),
            Some(InstallMethod::AppImage) => crate::i18n::tr("settings-update-method-appimage"),
            Some(InstallMethod::Homebrew) => crate::i18n::tr("settings-update-method-homebrew"),
            Some(InstallMethod::Nix) => crate::i18n::tr("settings-update-method-nix"),
            Some(InstallMethod::SystemPackage) => {
                crate::i18n::tr("settings-update-method-system")
            }
            Some(InstallMethod::WindowsInstaller) => {
                crate::i18n::tr("settings-update-method-windows")
            }
            Some(InstallMethod::SourceBuild) => crate::i18n::tr("settings-update-method-source"),
            Some(InstallMethod::Unknown) | None => {
                crate::i18n::tr("settings-update-method-unknown")
            }
        }
    }

    fn running_version_label(&self) -> String {
        self.ui
            .update_status
            .as_ref()
            .map(|status| status.current_version.clone())
            .unwrap_or_else(|| config::wezterm_version().to_string())
    }

    /// The hero every app-level page opens with: a badge, a headline, a
    /// second line, and one button on the right. Returns the card's height.
    fn paint_hero(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        max_width: f32,
        badge: HeroBadge,
        headline: &str,
        subline: &str,
        progress: Option<f32>,
        button: Option<(String, SettingsAction)>,
    ) -> anyhow::Result<f32> {
        let palette = self.palette();
        let body_font = Rc::clone(&self.body_font);
        let title_font = Rc::clone(&self.title_font);
        let cell = self.metrics.cell_size.height as f32;
        let padding = self.ui_px(HERO_PADDING);
        let mark = self.ui_px(HERO_MARK_SIZE);
        let height = (cell * 2.0 + self.ui_px(74.0)).max(mark + padding * 2.0);

        self.paint_group_card(layers, x, y, max_width, height)?;

        let mut text_right = x + max_width - padding;
        if let Some((label, action)) = button {
            let width = self.button_width_for_label(&label, self.ui_px(230.0));
            let button_x = x + max_width - padding - width;
            self.draw_button(
                layers,
                button_x,
                y + (height - self.ui_px(CONTROL_HEIGHT)) / 2.0,
                width,
                &label,
                action,
            )?;
            text_right = button_x - self.ui_px(24.0);
        }

        let mark_x = x + padding;
        let mark_y = y + (height - mark) / 2.0;
        match badge {
            HeroBadge::AppIcon => self.draw_app_icon(layers, mark_x, mark_y, mark)?,
        }

        let text_x = mark_x + mark + self.ui_px(24.0);
        let text_width = (text_right - text_x).max(self.ui_px(120.0));
        let line_gap = self.ui_px(12.0);
        // A progress bar goes between the two lines, with air either side;
        // the card keeps its height.
        let bar_height = self.ui_px(6.0);
        let bar_gap = self.ui_px(14.0);
        let block = match progress {
            Some(_) => cell * 2.0 + bar_gap * 2.0 + bar_height,
            None => cell * 2.0 + line_gap,
        };
        let text_y = y + (height - block) / 2.0;
        self.draw_text(
            layers,
            &title_font,
            text_x,
            text_y,
            headline,
            palette.title,
            text_width,
        )?;
        let subline_y = match progress {
            Some(fraction) => {
                let bar_y = text_y + cell + bar_gap;
                let radius = bar_height / 2.0;
                self.draw_rounded_rect(
                    layers,
                    0,
                    text_x,
                    bar_y,
                    text_width,
                    bar_height,
                    palette.track_off,
                    radius,
                )?;
                if fraction > 0.0 {
                    self.draw_rounded_rect(
                        layers,
                        0,
                        text_x,
                        bar_y,
                        (text_width * fraction.min(1.0)).max(bar_height),
                        bar_height,
                        self.chrome_palette.accent,
                        radius,
                    )?;
                }
                bar_y + bar_height + bar_gap
            }
            None => text_y + cell + line_gap,
        };
        self.draw_text(
            layers,
            &body_font,
            text_x,
            subline_y,
            subline,
            palette.secondary_text,
            text_width,
        )?;
        Ok(height)
    }

    /// The four right-sidebar panels, in the order their selector shows them.
    const RIGHT_SIDEBAR_PANELS: [crate::termwindow::RightSidebarMode; 4] = [
        crate::termwindow::RightSidebarMode::Chat,
        crate::termwindow::RightSidebarMode::Tasks,
        crate::termwindow::RightSidebarMode::Snippets,
        crate::termwindow::RightSidebarMode::Agents,
    ];

    /// The stored flag for a panel. `None` means the user never chose, which
    /// every panel reads as on.
    fn right_sidebar_panel_slot(
        &mut self,
        panel: crate::termwindow::RightSidebarMode,
    ) -> &mut Option<bool> {
        use crate::termwindow::RightSidebarMode as Panel;
        let chrome = &mut self.native_settings.chrome;
        match panel {
            Panel::Chat => &mut chrome.right_sidebar_files_enabled,
            Panel::Tasks => &mut chrome.right_sidebar_notes_enabled,
            Panel::Snippets => &mut chrome.right_sidebar_snippets_enabled,
            Panel::Agents => &mut chrome.agent_panel_enabled,
            Panel::Plugin(_) => unreachable!("a plugin's panel is switched with its plugin"),
        }
    }

    /// Whether the switch shows as on. Deliberately the same predicate the
    /// sidebar itself uses rather than the stored flag: Agents is also gated
    /// by the Lua `agent_status_detection` option, and a switch reading on
    /// while the panel can never appear is how someone turns off the other
    /// three believing one is left.
    fn right_sidebar_panel_enabled(&self, panel: crate::termwindow::RightSidebarMode) -> bool {
        panel.panel_enabled()
    }

    fn right_sidebar_panel_label(panel: crate::termwindow::RightSidebarMode) -> String {
        use crate::termwindow::RightSidebarMode as Panel;
        crate::i18n::tr(match panel {
            Panel::Chat => "right-mode-files",
            Panel::Tasks => "right-mode-notes",
            Panel::Snippets => "right-mode-snippets",
            Panel::Agents => "right-mode-agents",
            Panel::Plugin(_) => unreachable!("a plugin's panel is listed with its plugin"),
        })
    }

    fn right_sidebar_panel_description(panel: crate::termwindow::RightSidebarMode) -> String {
        use crate::termwindow::RightSidebarMode as Panel;
        crate::i18n::tr(match panel {
            Panel::Chat => "settings-sidebar-files-description",
            Panel::Tasks => "settings-sidebar-notes-description",
            Panel::Snippets => "settings-sidebar-snippets-description",
            Panel::Agents => "settings-agent-panel-description",
            Panel::Plugin(_) => unreachable!("a plugin's panel is listed with its plugin"),
        })
    }

    fn paint_sidebar_settings(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        max_width: f32,
    ) -> anyhow::Result<()> {
        let palette = self.palette();
        let body_font = Rc::clone(&self.body_font);
        let scroll = self.ui.content_scroll.offset;
        let section_y = self.ui_px(CONTENT_SECTION_Y) - scroll;

        self.draw_text(
            layers,
            &body_font,
            x,
            section_y,
            &crate::i18n::tr("settings-sidebar-description"),
            palette.secondary_text,
            max_width,
        )?;
        let (grid_y, _) = self.settings_card_geometry(section_y, 0);
        let grid_bottom = self.paint_panel_cards(layers, x, grid_y, max_width)?;
        // The plugins below the panels: whatever a plugin shows, it shows in
        // the sidebar.
        let plugins_title_y = grid_bottom + self.settings_section_card_gap();
        self.paint_plugins(layers, x, max_width, plugins_title_y)
    }

    /// The right sidebar's panels as cards, two across where there is room:
    /// each with its square, its switch, its name and what it is for. A
    /// panel that is off greys its square. Returns where the cards end.
    fn paint_panel_cards(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
    ) -> anyhow::Result<f32> {
        let palette = self.palette();
        let ui_font = Rc::clone(&self.ui_font);
        let body_font = Rc::clone(&self.body_font);
        let panels = Self::RIGHT_SIDEBAR_PANELS;
        let columns = if width < self.ui_px(900.0) { 1 } else { 2 };
        let gap = self.ui_px(24.0);
        let card_width = ((width - gap * (columns - 1) as f32) / columns as f32).floor();
        let pad = self.ui_px(30.0);
        let tile = self.ui_px(64.0).round();
        let cell_height = self.metrics.cell_size.height as f32;
        let line_step = self.description_line_step();
        let text_width = (card_width - pad * 2.0).max(0.0);
        let switch_width = self.ui_px(SWITCH_WIDTH);
        let switch_height = self.ui_px(SWITCH_HEIGHT);
        let title_gap = self.ui_px(20.0);
        let description_gap = self.ui_px(8.0);
        // Every card as tall as the tallest, so the grid stays a grid.
        let descriptions: Vec<Vec<String>> = panels
            .iter()
            .map(|panel| {
                self.capped_lines(
                    &body_font,
                    &Self::right_sidebar_panel_description(*panel),
                    text_width,
                    2,
                )
            })
            .collect();
        let most_lines = descriptions.iter().map(Vec::len).max().unwrap_or(1).max(1);
        let card_height = (pad * 2.0
            + tile
            + title_gap
            + cell_height
            + description_gap
            + line_step * (most_lines - 1) as f32
            + cell_height)
            .round();

        for (index, panel) in panels.iter().copied().enumerate() {
            let card_x = x + (card_width + gap) * (index % columns) as f32;
            let card_y = y + (card_height + gap) * (index / columns) as f32;
            let enabled = self.right_sidebar_panel_enabled(panel);
            self.draw_rounded_frame(
                layers,
                0,
                card_x,
                card_y,
                card_width,
                card_height,
                palette.card_bg,
                palette.separator,
                self.ui_px(28.0),
            )?;
            let (icon, color) = Self::right_sidebar_panel_tile(panel);
            self.paint_tile(
                layers,
                card_x + pad,
                card_y + pad,
                tile,
                icon,
                if enabled { color } else { TileColor::Gray },
            )?;

            let action = SettingsAction::ToggleRightSidebarPanel(panel);
            let switch_x = card_x + card_width - pad - switch_width;
            let switch_y = card_y + pad + ((tile - switch_height) / 2.0).round();
            let reach = self.ui_px(10.0);
            self.ui_context.push(
                rect(
                    switch_x - reach,
                    switch_y - reach,
                    switch_width + reach * 2.0,
                    switch_height + reach * 2.0,
                ),
                WidgetKind::Button,
                action,
            );
            let hovered = self.ui.interaction.hovered == Some(action);
            let pressed = self.ui.interaction.pressed == Some(action);
            self.paint_switch(layers, switch_x, switch_y, enabled, hovered, pressed)?;

            let title_y = card_y + pad + tile + title_gap;
            self.draw_text(
                layers,
                &ui_font,
                card_x + pad,
                title_y,
                &Self::right_sidebar_panel_label(panel),
                if enabled {
                    palette.text
                } else {
                    palette.secondary_text
                },
                text_width,
            )?;
            for (line_index, line) in descriptions[index].iter().enumerate() {
                self.draw_text(
                    layers,
                    &body_font,
                    card_x + pad,
                    title_y + cell_height + description_gap + line_step * line_index as f32,
                    line,
                    if enabled {
                        palette.secondary_text
                    } else {
                        palette.muted_text
                    },
                    text_width,
                )?;
            }
        }
        let rows = (panels.len() + columns - 1) / columns;
        Ok(y + (card_height + gap) * rows as f32 - gap)
    }

    /// The square a panel's card carries.
    fn right_sidebar_panel_tile(
        panel: crate::termwindow::RightSidebarMode,
    ) -> (SvgIcon, TileColor) {
        use crate::termwindow::RightSidebarMode as Panel;
        match panel {
            Panel::Chat => (SvgIcon::FolderOpen, TileColor::Blue),
            Panel::Tasks => (SvgIcon::NotebookTabs, TileColor::Yellow),
            Panel::Snippets => (SvgIcon::Braces, TileColor::Purple),
            Panel::Agents => (SvgIcon::Bot, TileColor::Orange),
            Panel::Plugin(_) => (SvgIcon::Puzzle, TileColor::Green),
        }
    }

    /// The plugins, on the Sidebar page from `title_y`: where they are
    /// installed, reloading them, and each as the plugin host last listed
    /// it. An installed one has its switch here, and, while on, how long it
    /// runs unused beside it; a built-in one says which panel it provides,
    /// whose switch is above.
    fn paint_plugins(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        max_width: f32,
        title_y: f32,
    ) -> anyhow::Result<()> {
        use thinkterm_plugin_channel::registry::State;
        let palette = self.palette();
        let body_font = Rc::clone(&self.body_font);
        let scroll = self.ui.content_scroll.offset;
        let card_padding = self.ui_px(36.0);
        let row_x = x + card_padding;
        let row_width = max_width - card_padding * 2.0;

        // In the primary colour when it is news: a change refused, or the
        // host gone, which makes the list below only what was last heard.
        let (description, description_color) =
            match (crate::plugins::refused(), crate::plugins::trouble()) {
                (Some(why), _) => (
                    settings_tr("settings-plugins-refused", &[("reason", why)]),
                    palette.text,
                ),
                (None, Some(why)) => (
                    settings_tr("settings-plugins-unavailable", &[("reason", why)]),
                    palette.text,
                ),
                (None, None) => (
                    crate::i18n::tr("settings-plugins-description"),
                    palette.secondary_text,
                ),
            };
        self.draw_text(
            layers,
            &body_font,
            x,
            title_y,
            &description,
            description_color,
            max_width,
        )?;

        let card_y = title_y + self.settings_section_card_gap().min(54.0);
        let bottom = self.paint_card(layers, x, card_y, max_width, |this, layers, top| {
            let mut rows = RowCursor::new(top, this);
            let (tx, tw) = this.paint_row_tile(
                layers,
                row_x,
                rows.y,
                row_width,
                SvgIcon::Folder,
                TileColor::Gray,
            )?;
            rows.add(this.paint_action_setting_row(
                layers,
                tx,
                rows.y,
                tw,
                &crate::i18n::tr("settings-plugins-folder"),
                &home_relative(&thinkterm_plugin_channel::paths::plugins_dir()),
                &crate::i18n::tr("settings-plugins-open-folder"),
                SettingsAction::OpenPluginsFolder,
                rows.rule(),
            )?);
            let (tx, tw) = this.paint_row_tile(
                layers,
                row_x,
                rows.y,
                row_width,
                SvgIcon::RotateCw,
                TileColor::Green,
            )?;
            rows.add(this.paint_action_setting_row(
                layers,
                tx,
                rows.y,
                tw,
                &crate::i18n::tr("settings-plugins-reload"),
                &crate::i18n::tr("settings-plugins-reload-description"),
                &crate::i18n::tr("settings-plugins-reload-button"),
                SettingsAction::ReloadPlugins,
                rows.rule(),
            )?);
            Ok(rows.bottom)
        })?;

        self.ui.plugin_switches.clear();
        self.ui.plugin_backgrounds.clear();
        self.ui.plugin_background_pill = None;
        let plugins = crate::plugins::plugins().filter(|plugins| !plugins.is_empty());
        let list_y = bottom + self.settings_section_card_gap();
        let bottom = self.paint_card(layers, x, list_y, max_width, |this, layers, top| {
            let Some(plugins) = plugins else {
                let label = match crate::plugins::trouble() {
                    Some(why) => settings_tr("settings-plugins-unavailable", &[("reason", why)]),
                    None => crate::i18n::tr("settings-plugins-loading"),
                };
                this.draw_text(
                    layers,
                    &body_font,
                    row_x,
                    top,
                    &label,
                    palette.secondary_text,
                    row_width,
                )?;
                return Ok(top + this.metrics.cell_size.height as f32);
            };
            let mut rows = RowCursor::new(top, this);
            for plugin in plugins.iter() {
                let description = plugin_row_description(plugin);
                let (icon, color) = plugin_tile(plugin);
                let (tx, tw) =
                    this.paint_row_tile(layers, row_x, rows.y, row_width, icon, color)?;
                if plugin.builtin {
                    // Its switch is its panel's, among the panels above.
                    rows.add(this.paint_label_row(
                        layers,
                        tx,
                        rows.y,
                        tw,
                        &plugin.name,
                        &description,
                        rows.rule(),
                    )?);
                    continue;
                }
                if matches!(
                    plugin.state,
                    State::Invalid { .. } | State::Unsupported { .. }
                ) {
                    // Nothing a switch could do for it.
                    rows.add(this.paint_setting_row(
                        layers,
                        tx,
                        rows.y,
                        tw,
                        &plugin.name,
                        &description,
                        &crate::i18n::tr("settings-plugins-unusable"),
                        rows.rule(),
                    )?);
                    continue;
                }
                let key = plugin_key(&plugin.id);
                if plugin.state == State::New && crate::plugins::switching(&plugin.id).is_none() {
                    // Never let run: a button, not a switch, so that it
                    // takes a press made for it.
                    this.ui
                        .plugin_switches
                        .push((key, plugin.id.clone(), false));
                    rows.add(this.paint_action_setting_row(
                        layers,
                        tx,
                        rows.y,
                        tw,
                        &plugin.name,
                        &description,
                        &crate::i18n::tr("settings-plugins-allow"),
                        SettingsAction::AllowPlugin(key),
                        rows.rule(),
                    )?);
                    continue;
                }
                let enabled = crate::plugins::switching(&plugin.id).unwrap_or(plugin.enabled);
                this.ui
                    .plugin_switches
                    .push((key, plugin.id.clone(), enabled));
                rows.add(this.paint_plugin_row(
                    layers,
                    tx,
                    rows.y,
                    tw,
                    key,
                    plugin,
                    &description,
                    enabled,
                    rows.rule(),
                )?);
            }
            Ok(rows.bottom)
        })?;

        self.ui.content_scroll.set_extents(
            self.content_viewport_extent(),
            self.settings_content_extent(bottom + scroll),
        );
        Ok(())
    }

    /// An installed plugin's row: its name and what it is on the left; its
    /// switch on the right and, while it is on, how long it runs unused in
    /// a pill beside the switch, whose menu offers the three choices.
    #[allow(clippy::too_many_arguments)]
    fn paint_plugin_row(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
        key: u64,
        plugin: &thinkterm_plugin_channel::registry::Info,
        description: &str,
        enabled: bool,
        draw_top_rule: bool,
    ) -> anyhow::Result<f32> {
        let palette = self.palette();
        let ui_font = Rc::clone(&self.ui_font);
        if draw_top_rule {
            self.paint_separator(layers, x, y - self.ui_px(28.0), width)?;
        }
        let control_y = y + self.ui_px(4.0);
        let control_height = self.ui_px(CONTROL_HEIGHT);
        let switch_width = self.ui_px(SWITCH_WIDTH);
        let switch_x = x + width - switch_width;
        let toggle = SettingsAction::TogglePlugin(key);
        let reach = self.ui_px(8.0);
        self.ui_context.push(
            rect(
                switch_x - reach,
                control_y,
                switch_width + reach,
                control_height,
            ),
            WidgetKind::Button,
            toggle,
        );
        let mut text_right = switch_x - self.ui_px(24.0);

        if enabled {
            self.ui
                .plugin_backgrounds
                .push((key, plugin.id.clone(), plugin.background_default));
            let chosen = crate::plugins::choosing(&plugin.id).unwrap_or(plugin.background);
            let label = crate::i18n::tr(background_label(chosen));
            let pill_width = (self.measure_text_width(&ui_font, &label) + self.ui_px(68.0))
                .max(self.ui_px(120.0));
            let pill = rect(
                switch_x - self.ui_px(16.0) - pill_width,
                control_y,
                pill_width,
                control_height,
            );
            let menu = SettingsAction::TogglePluginBackgroundMenu(key);
            self.ui_context.push(pill, WidgetKind::Button, menu);
            let open = self.ui.open_dropdown == Some(SettingsDropdown::PluginBackground(key));
            let hovered = self.ui.interaction.hovered == Some(menu);
            let pressed = self.ui.interaction.pressed == Some(menu);
            let bg = if pressed || hovered {
                palette.control_hover_bg
            } else {
                palette.control_bg
            };
            let border = if open {
                palette.nav_selected_bg
            } else if hovered || pressed {
                palette.separator
            } else {
                palette.control_border
            };
            self.paint_dropdown_pill(layers, pill, &label, bg, border)?;
            if open {
                self.ui.plugin_background_pill = Some((key, pill));
            }
            text_right = pill.origin.x - self.ui_px(24.0);
        }

        let text_width = (text_right - x).max(width * 0.35);
        self.draw_text(layers, &ui_font, x, y, &plugin.name, palette.text, text_width)?;
        let extra = self.draw_row_description(layers, x, y, description, text_width)?;
        let hovered = self.ui.interaction.hovered == Some(toggle);
        let pressed = self.ui.interaction.pressed == Some(toggle);
        self.paint_switch(
            layers,
            switch_x,
            control_y + (control_height - self.ui_px(SWITCH_HEIGHT)) / 2.0,
            enabled,
            hovered,
            pressed,
        )?;
        Ok(extra)
    }

    /// The menu of how long plugin `key` runs unused, under its `pill`: each
    /// choice by its name and what it means, the manifest's said to be so.
    fn paint_plugin_background_menu(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        key: u64,
        pill: window::RectF,
    ) -> anyhow::Result<()> {
        use thinkterm_plugin_channel::registry::Background;
        let Some((_, id, default)) = self
            .ui
            .plugin_backgrounds
            .iter()
            .find(|(shown, _, _)| *shown == key)
            .cloned()
        else {
            return Ok(());
        };
        let chosen = crate::plugins::plugins()
            .and_then(|plugins| plugins.into_iter().find(|plugin| plugin.id == id))
            .map_or(default, |plugin| plugin.background);
        let chosen = crate::plugins::choosing(&id).unwrap_or(chosen);
        let options: Vec<(String, String, SettingsAction, bool)> =
            [Background::Always, Background::Briefly, Background::Never]
                .iter()
                .copied()
                .map(|background| {
                    let mut meaning = crate::i18n::tr(background_description(background));
                    if background == default {
                        meaning = format!(
                            "{meaning} · {}",
                            crate::i18n::tr("settings-plugins-background-default")
                        );
                    }
                    (
                        crate::i18n::tr(background_label(background)),
                        meaning,
                        SettingsAction::SetPluginBackground(key, background),
                        background == chosen,
                    )
                })
                .collect();
        self.paint_described_dropdown_menu(layers, pill, &options)
    }

    /// [`paint_dropdown_menu`], with a line under each choice saying what it
    /// means: under `pill`, as wide as its longest line and flush with the
    /// pill's right edge -- over it, where there is no room below. Painted on
    /// the top layer, so the icons of the rows it covers do not show through.
    fn paint_described_dropdown_menu(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        pill: window::RectF,
        options: &[(String, String, SettingsAction, bool)],
    ) -> anyhow::Result<()> {
        const LAYER: usize = 2;
        let palette = self.palette();
        let ui_font = Rc::clone(&self.ui_font);
        let body_font = Rc::clone(&self.body_font);
        let line = self.metrics.cell_size.height as f32;
        let row_height = line * 2.0 + self.ui_px(20.0);
        let row_gap = self.ui_px(6.0);
        let menu_padding = self.ui_px(8.0);
        let menu_height = menu_padding * 2.0
            + row_height * options.len() as f32
            + row_gap * options.len().saturating_sub(1) as f32;
        let widest = options
            .iter()
            .map(|(label, meaning, _, _)| {
                self.measure_text_width(&ui_font, label)
                    .max(self.measure_text_width(&body_font, meaning))
            })
            .fold(0.0, f32::max);
        let right = pill.origin.x + pill.size.width;
        let width = (widest + self.ui_px(48.0))
            .max(pill.size.width)
            .min(right - self.ui_px(8.0));
        let x = right - width;
        let below = pill.origin.y + pill.size.height + self.ui_px(8.0);
        let y = if below + menu_height <= self.content_bottom() - self.ui_px(8.0) {
            below
        } else {
            (pill.origin.y - self.ui_px(8.0) - menu_height).max(self.content_scroll_area_top())
        };
        let menu_bg = match self.effective_appearance() {
            Appearance::Light | Appearance::LightHighContrast => rgba(248, 248, 250, 1.0),
            Appearance::Dark | Appearance::DarkHighContrast => rgba(34, 34, 36, 1.0),
        };
        // As in `paint_dropdown_menu`: the whole menu is claimed, so a click
        // between its rows does not reach what it is painted over.
        self.ui_context.push(
            rect(x, y, width, menu_height),
            WidgetKind::Button,
            SettingsAction::DropdownMenuBackdrop,
        );
        self.draw_rounded_frame(
            layers,
            LAYER,
            x,
            y,
            width,
            menu_height,
            menu_bg,
            palette.control_border,
            self.ui_px(CONTROL_RADIUS),
        )?;
        let mut row_y = y + menu_padding;
        for (label, meaning, action, selected) in options {
            let (action, selected) = (*action, *selected);
            let row_rect = rect(
                x + self.ui_px(8.0),
                row_y,
                width - self.ui_px(16.0),
                row_height,
            );
            self.ui_context.push(row_rect, WidgetKind::Button, action);
            let hovered = self.ui.interaction.hovered == Some(action);
            let pressed = self.ui.interaction.pressed == Some(action);
            let row_bg = if selected {
                Some(palette.nav_selected_bg)
            } else if pressed {
                Some(palette.control_pressed_bg)
            } else if hovered {
                Some(palette.control_hover_bg)
            } else {
                None
            };
            if let Some(row_bg) = row_bg {
                self.draw_rounded_rect(
                    layers,
                    LAYER,
                    row_rect.origin.x,
                    row_rect.origin.y,
                    row_rect.size.width,
                    row_rect.size.height,
                    row_bg,
                    self.ui_px(9.0),
                )?;
            }
            let text_x = row_rect.origin.x + self.ui_px(14.0);
            let text_width = row_rect.size.width - self.ui_px(28.0);
            let top = row_rect.origin.y + self.ui_px(10.0);
            let (label_color, meaning_color) = if selected {
                (palette.selected_text, palette.selected_text)
            } else {
                (palette.text, palette.secondary_text)
            };
            self.draw_text_on_layer(
                layers,
                LAYER,
                &ui_font,
                text_x,
                top,
                label,
                label_color,
                text_width,
            )?;
            self.draw_text_on_layer(
                layers,
                LAYER,
                &body_font,
                text_x,
                top + line,
                meaning,
                meaning_color,
                text_width,
            )?;
            row_y += row_height + row_gap;
        }
        Ok(())
    }

    fn paint_update(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        max_width: f32,
    ) -> anyhow::Result<()> {
        let palette = self.palette();
        let body_font = Rc::clone(&self.body_font);
        let scroll = self.ui.content_scroll.offset;
        let card_padding = self.ui_px(HERO_PADDING);
        let row_x = x + card_padding;
        let row_width = max_width - card_padding * 2.0;
        let gap = self.settings_section_card_gap();

        let hero = self.update_hero();
        let version = self.running_version_label();
        // What the last check found; an install or a check in flight
        // outranks it below.
        let (headline, subline) = match &hero {
            UpdateHero::UpToDate => (
                crate::i18n::tr("settings-update-current"),
                settings_tr("settings-update-current-detail", &[("version", version)]),
            ),
            UpdateHero::Available { tag } => (
                crate::i18n::tr("settings-update-available"),
                settings_tr(
                    "settings-update-available-detail",
                    &[("latest", tag.clone()), ("current", version)],
                ),
            ),
            UpdateHero::LocalBuild => (
                crate::i18n::tr("settings-update-local-build"),
                settings_tr(
                    "settings-update-local-build-detail",
                    &[("version", version)],
                ),
            ),
            UpdateHero::Unknown => (
                crate::i18n::tr("settings-update-unknown"),
                settings_tr("settings-update-unknown-detail", &[("version", version)]),
            ),
        };

        let checked_recently = self
            .ui
            .update_checked_until
            .is_some_and(|until| Instant::now() < until);
        let check = update_check();
        let check_label = if matches!(check, Some(UpdateCheck::Running)) {
            crate::i18n::tr("settings-update-checking")
        } else if checked_recently {
            crate::i18n::tr("settings-update-checked")
        } else {
            crate::i18n::tr("settings-update-check-now")
        };

        // The one button on the hero: install when there is something to
        // install and this copy is ours to replace, restart once it has been
        // replaced, check otherwise. While an install runs there is nothing
        // to click, and the Windows installer restarts ThinkTerm itself.
        let self_updatable = self
            .ui
            .update_method
            .as_ref()
            .is_some_and(|method| method.self_updatable() || cfg!(windows));
        let install = update_install();
        let hero_button = match (&hero, &install) {
            (_, Some(UpdateInstall::Running { .. })) => None,
            // Restart only where it ends no terminal: shells this process
            // runs itself would go with it, so there the person quits and
            // reopens when ready. The Windows installer restarts by itself.
            (_, Some(UpdateInstall::Installed { .. }))
                if !cfg!(windows) && !restart_ends_terminals() =>
            {
                Some((
                    crate::i18n::tr("settings-restart"),
                    SettingsAction::RestartApplication,
                ))
            }
            (_, Some(UpdateInstall::Installed { .. })) => None,
            (UpdateHero::Available { .. }, _) if self_updatable => Some((
                crate::i18n::tr("settings-update-install"),
                SettingsAction::InstallUpdate,
            )),
            _ => Some((check_label, SettingsAction::CheckForUpdates)),
        };

        // The card says what is happening now: an install or a check in
        // flight, or how one went, before what the last check found.
        use thinkterm_update::InstallProgress;
        let (headline, subline, progress) =
            match (&install, &check) {
                (
                    Some(UpdateInstall::Running {
                        version,
                        progress: Some(InstallProgress::Downloading { done, total }),
                        download_started,
                    }),
                    _,
                ) => {
                    let fraction = if *total > 0 {
                        (*done as f32 / *total as f32).clamp(0.0, 1.0)
                    } else {
                        0.0
                    };
                    let mut detail = settings_tr(
                        "settings-update-download-progress",
                        &[
                            ("done", format_bytes(*done)),
                            ("total", format_bytes(*total)),
                            ("percent", ((fraction * 100.0).floor() as u32).to_string()),
                        ],
                    );
                    if let Some(left) = download_started
                        .and_then(|started| download_time_left(started, *done, *total))
                    {
                        detail = format!("{detail} · {left}");
                    }
                    (
                        settings_tr("settings-update-downloading", &[("version", version.clone())]),
                        detail,
                        Some(fraction),
                    )
                }
                // Looking the release up, fetching install.sh: nothing is
                // being installed yet.
                (
                    Some(UpdateInstall::Running {
                        version,
                        progress: None,
                        ..
                    }),
                    _,
                ) => (
                    settings_tr("settings-update-preparing", &[("version", version.clone())]),
                    crate::i18n::tr("settings-update-installing"),
                    None,
                ),
                (Some(UpdateInstall::Running { version, .. }), _) => (
                    settings_tr(
                        "settings-update-installing-title",
                        &[("version", version.clone())],
                    ),
                    crate::i18n::tr("settings-update-installing"),
                    None,
                ),
                (Some(UpdateInstall::Installed { version }), _) if cfg!(windows) => (
                    crate::i18n::tr("settings-update-installer-started"),
                    settings_tr(
                        "settings-update-installer-started-detail",
                        &[("version", version.clone())],
                    ),
                    None,
                ),
                (Some(UpdateInstall::Installed { version }), _) => (
                    settings_tr("settings-update-installed", &[("version", version.clone())]),
                    crate::i18n::tr("settings-update-installed-detail"),
                    None,
                ),
                (Some(UpdateInstall::Failed { .. }), _) => (
                    crate::i18n::tr("settings-update-install-failed-title"),
                    crate::i18n::tr("settings-update-failed-detail"),
                    None,
                ),
                (None, Some(UpdateCheck::Running)) => (
                    crate::i18n::tr("settings-update-checking-title"),
                    subline,
                    None,
                ),
                (None, Some(UpdateCheck::Failed { .. })) => (
                    crate::i18n::tr("settings-update-check-failed"),
                    crate::i18n::tr("settings-update-failed-detail"),
                    None,
                ),
                (None, None) => (headline, subline, None),
            };

        let hero_y = self.ui_px(CONTENT_SECTION_Y) - scroll;
        let hero_height = self.paint_hero(
            layers,
            x,
            hero_y,
            max_width,
            HeroBadge::AppIcon,
            &headline,
            &subline,
            progress,
            hero_button,
        )?;

        // Three plain facts, one line each: the labels say what they are, and
        // an explanation under every one of them was noise.
        let config = configuration();
        let rows = [
            (
                SvgIcon::RefreshCw,
                TileColor::Green,
                crate::i18n::tr("settings-update-automatic"),
                if config.check_for_updates {
                    crate::i18n::tr("common-on")
                } else {
                    crate::i18n::tr("common-off")
                },
            ),
            (
                SvgIcon::Calendar,
                TileColor::Orange,
                crate::i18n::tr("settings-update-frequency"),
                format_check_interval(config.check_for_updates_interval_seconds),
            ),
            (
                SvgIcon::Timer,
                TileColor::Blue,
                crate::i18n::tr("settings-update-last-checked"),
                self.ui
                    .update_status
                    .as_ref()
                    .and_then(|status| status.last_checked)
                    .map(format_last_checked)
                    .unwrap_or_else(|| crate::i18n::tr("settings-update-never")),
            ),
        ];
        let card_y = hero_y + hero_height + gap;
        let card_height = self.compact_card_height(rows.len());
        let row_step = self.compact_row_step();
        let first_row_y = card_y + self.ui_px(12.0);
        self.paint_group_card(layers, x, card_y, max_width, card_height)?;
        for (index, (icon, color, label, value)) in rows.iter().enumerate() {
            let row_y = first_row_y + row_step * index as f32;
            let (tx, tw) =
                self.paint_band_tile(layers, row_x, row_y, row_step, row_width, *icon, *color)?;
            self.paint_compact_row(layers, tx, row_y, tw, label, value, index > 0)?;
        }

        // One choice: what updating a remote server does to its sessions.
        let toggle_card_y = card_y + card_height + gap;
        let toggle_card_height = self.compact_card_height(1);
        self.paint_group_card(layers, x, toggle_card_y, max_width, toggle_card_height)?;
        let toggle_row_y = toggle_card_y + self.ui_px(12.0);
        let (tx, tw) = self.paint_band_tile(
            layers,
            row_x,
            toggle_row_y,
            row_step,
            row_width,
            SvgIcon::Server,
            TileColor::Slate,
        )?;
        self.paint_toggle_setting_row_with_hint(
            layers,
            tx,
            toggle_row_y,
            tw,
            self.compact_row_step(),
            &crate::i18n::tr("settings-remote-update-keep-sessions"),
            "settings-remote-update-keep-sessions-description",
            self.native_settings.workspaces.remote_update_keeps_sessions,
            SettingsAction::ToggleRemoteUpdateKeepsSessions,
        )?;

        // Under the facts: what went wrong, if something did -- the card
        // above only has room to say that it did -- then what an install
        // would do here, or why this page cannot do one.
        let note_y = toggle_card_y + toggle_card_height + gap;
        let line_step = self.metrics.cell_size.height as f32 + self.ui_px(6.0);
        let mut notes: Vec<(String, LinearRgba)> = Vec::new();
        let failure = match (&install, &check) {
            (Some(UpdateInstall::Failed { error }), _) => Some(error),
            (None, Some(UpdateCheck::Failed { error })) => Some(error),
            _ => None,
        };
        if let Some(error) = failure {
            notes.push((error.clone(), self.chrome_palette.danger));
        }
        notes.push((
            self.update_method_note(),
            palette.secondary_text,
        ));
        if self_updatable && !matches!(install, Some(UpdateInstall::Installed { .. })) {
            notes.push((
                crate::i18n::tr("settings-update-install-note"),
                palette.secondary_text,
            ));
        }
        let mut lines_drawn = 0usize;
        for (text, color) in &notes {
            let wrapped = self.wrap_settings_text(&body_font, text, max_width);
            for line in wrapped {
                self.draw_text(
                    layers,
                    &body_font,
                    x,
                    note_y + line_step * lines_drawn as f32,
                    &line,
                    *color,
                    max_width,
                )?;
                lines_drawn += 1;
            }
        }
        let note_y = note_y + line_step * lines_drawn.saturating_sub(1) as f32;

        let (link_label, link_action) = match &hero {
            UpdateHero::Available { .. } => (
                crate::i18n::tr("settings-update-whats-new"),
                SettingsAction::OpenLatestRelease,
            ),
            _ => (
                crate::i18n::tr("settings-update-all-releases"),
                SettingsAction::OpenReleasesIndex,
            ),
        };
        let button_y = note_y + self.metrics.cell_size.height as f32 + self.ui_px(24.0);
        let button_height =
            self.paint_button_row(layers, x, button_y, max_width, &[(link_label, link_action)])?;

        self.ui.content_scroll.set_extents(
            self.content_viewport_extent(),
            self.settings_content_extent(button_y + scroll + button_height),
        );
        Ok(())
    }

    fn paint_about(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        max_width: f32,
    ) -> anyhow::Result<()> {
        let scroll = self.ui.content_scroll.offset;
        let card_padding = self.ui_px(HERO_PADDING);
        let row_x = x + card_padding;
        let row_width = max_width - card_padding * 2.0;
        let gap = self.settings_section_card_gap();
        let version = self.running_version_label();

        let copied = self
            .ui
            .version_info_copied_until
            .is_some_and(|until| Instant::now() < until);
        let copy_label = if copied {
            crate::i18n::tr("settings-about-copied")
        } else {
            crate::i18n::tr("settings-about-copy")
        };

        let hero_y = self.ui_px(CONTENT_SECTION_Y) - scroll;
        let hero_height = self.paint_hero(
            layers,
            x,
            hero_y,
            max_width,
            HeroBadge::AppIcon,
            "ThinkTerm",
            &settings_tr(
                "settings-about-version-line",
                &[("version", version.clone())],
            ),
            None,
            Some((copy_label, SettingsAction::CopyVersionInfo)),
        )?;

        // Build only earns a line when it says something Version does not: a
        // release binary stamps both from the same tag.
        let build = config::wezterm_version();
        let mut rows = vec![(
            SvgIcon::Package,
            TileColor::Blue,
            crate::i18n::tr("settings-about-version"),
            version.clone(),
        )];
        if build != version {
            rows.push((
                SvgIcon::GitBranch,
                TileColor::Indigo,
                crate::i18n::tr("settings-about-build"),
                build.to_string(),
            ));
        }
        rows.push((
            SvgIcon::Cpu,
            TileColor::Teal,
            crate::i18n::tr("settings-about-platform"),
            config::wezterm_target_triple().to_string(),
        ));
        rows.push((
            SvgIcon::Scale,
            TileColor::Orange,
            crate::i18n::tr("settings-about-license"),
            // Must match what the packages declare -- ci/deploy.sh and
            // ci/make-winget-pr.sh both say GPL-3.0-only. "or-later" is a
            // different promise (recipients may use a future GPL), not a
            // wording variant, so the two must not drift.
            "GPL-3.0-only".to_string(),
        ));

        let card_y = hero_y + hero_height + gap;
        let card_height = self.compact_card_height(rows.len());
        let row_step = self.compact_row_step();
        let first_row_y = card_y + self.ui_px(12.0);
        self.paint_group_card(layers, x, card_y, max_width, card_height)?;
        for (index, (icon, color, label, value)) in rows.iter().enumerate() {
            let row_y = first_row_y + row_step * index as f32;
            let (tx, tw) =
                self.paint_band_tile(layers, row_x, row_y, row_step, row_width, *icon, *color)?;
            self.paint_compact_row(layers, tx, row_y, tw, label, value, index > 0)?;
        }

        // Everything outbound sits in one wrapping row along the bottom
        // rather than as a button per row, which is five cards' worth of
        // chrome for five links.
        let buttons = [
            (
                crate::i18n::tr("settings-about-source"),
                SettingsAction::OpenSourceRepository,
            ),
            (
                crate::i18n::tr("settings-about-release-notes"),
                SettingsAction::OpenReleasesIndex,
            ),
            (
                crate::i18n::tr("settings-about-config-file"),
                SettingsAction::OpenThinkTermConfigFile,
            ),
            (
                crate::i18n::tr("settings-about-data-folder"),
                SettingsAction::OpenDataFolder,
            ),
            (
                crate::i18n::tr("settings-about-notices"),
                SettingsAction::OpenThirdPartyNotices,
            ),
            (
                crate::i18n::tr("settings-about-privacy"),
                SettingsAction::OpenPrivacyPolicy,
            ),
        ];
        let button_y = card_y + card_height + gap;
        let button_height = self.paint_button_row(layers, x, button_y, max_width, &buttons)?;

        // Who makes it, held to the foot of the window. Its margin is tighter
        // than the page's usual one, so the extent uses the same margin: with
        // the usual one a pinned logo would leave a few pixels to scroll.
        let logo_height = self.ui_px(64.0);
        let logo_margin = self.ui_px(44.0);
        let logo_top = (button_y + scroll + button_height + self.ui_px(48.0))
            .max(self.content_bottom() - logo_margin - logo_height)
            .round();
        let logo_width = self.draw_closex_logo(
            layers,
            x,
            logo_top - scroll,
            logo_height,
            self.palette().text,
        )?;
        // The copyright just after it, small and sitting on the wordmark's
        // baseline. `draw_text` puts a baseline this far below the y it is
        // given, whatever the font.
        let caption_font = Rc::clone(&self.logo_caption_font);
        let caption_x = x + logo_width + self.ui_px(12.0);
        let baseline =
            logo_top - scroll + logo_height * crate::termwindow::ui::icons::CLOSEX_LOGO_BASELINE;
        let baseline_offset =
            self.metrics.cell_size.height as f32 + self.metrics.descender.get() as f32;
        self.draw_text(
            layers,
            &caption_font,
            caption_x,
            (baseline - baseline_offset).round(),
            "© 2026",
            self.palette().muted_text,
            (x + max_width - caption_x).max(0.0),
        )?;

        self.ui.content_scroll.set_extents(
            self.content_viewport_extent(),
            (logo_top + logo_height + logo_margin).max(self.content_bottom()),
        );
        Ok(())
    }

    /// The block the About page's Copy button puts on the clipboard: what a
    /// bug report needs on its first line so nobody has to ask.
    fn version_info_for_clipboard(&self) -> String {
        let mut lines = vec![
            format!("ThinkTerm {}", self.running_version_label()),
            format!("Build: {}", config::wezterm_version()),
            format!("Target: {}", config::wezterm_target_triple()),
            format!("OS: {}", std::env::consts::OS),
            format!(
                "Config: {}",
                Self::thinkterm_compatible_config_path().display()
            ),
            format!("Data: {}", config::DATA_DIR.display()),
        ];
        if let Some(status) = self.ui.update_status.as_ref() {
            if let Some(latest) = status.latest.as_ref() {
                lines.push(format!("Latest release seen: {}", latest.tag_name));
            }
        }
        lines.join("\n")
    }

    fn paint_placeholder(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        font: &Rc<LoadedFont>,
        x: f32,
        body: &str,
        max_width: f32,
    ) -> anyhow::Result<()> {
        let palette = self.palette();
        self.ui
            .content_scroll
            .set_extents(self.content_bottom(), self.ui_px(240.0));
        let scroll = self.ui.content_scroll.offset;
        self.draw_text(
            layers,
            font,
            x,
            self.ui_px(CONTENT_SECTION_Y) - scroll,
            body,
            palette.secondary_text,
            max_width,
        )?;
        self.draw_rect(
            layers,
            0,
            x,
            self.ui_px(CONTENT_RULE_Y) - scroll,
            max_width,
            1.0,
            palette.rule,
        )?;
        Ok(())
    }

    /// A row that only tells: its label and its description, no control.
    /// A row's description under its label, in as many lines as the page
    /// allows (`row_description_lines`), the last ellipsised if the text
    /// runs on. Returns the height taken beyond the one line every row has
    /// room for, which the page moves the next row down by.
    fn draw_row_description(
        &self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        label_y: f32,
        description: &str,
        width: f32,
    ) -> anyhow::Result<f32> {
        let palette = self.palette();
        let body_font = Rc::clone(&self.body_font);
        let lines = self.description_lines(&body_font, description, width);
        self.draw_text_lines(
            layers,
            &body_font,
            x,
            self.settings_row_description_y(label_y),
            &lines,
            palette.secondary_text,
            width,
        )
    }

    /// A row's note under its description, wrapped as the description is:
    /// one line in English is often two in French or German.
    fn draw_row_note(
        &self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        note: &str,
        width: f32,
    ) -> anyhow::Result<f32> {
        let palette = self.palette();
        let body_font = Rc::clone(&self.body_font);
        let lines = self.description_lines(&body_font, note, width);
        self.draw_text_lines(layers, &body_font, x, y, &lines, palette.muted_text, width)
    }

    /// `lines` one under another from `y`, a description line apart. Returns
    /// how far below `y` the last of them starts.
    #[allow(clippy::too_many_arguments)]
    fn draw_text_lines(
        &self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        font: &Rc<LoadedFont>,
        x: f32,
        y: f32,
        lines: &[String],
        color: LinearRgba,
        width: f32,
    ) -> anyhow::Result<f32> {
        let step = self.description_line_step();
        for (index, line) in lines.iter().enumerate() {
            self.draw_text(layers, font, x, y + step * index as f32, line, color, width)?;
        }
        Ok(step * lines.len().saturating_sub(1) as f32)
    }

    fn description_line_step(&self) -> f32 {
        self.metrics.cell_size.height as f32 + self.ui_px(4.0)
    }

    /// `description` broken into at most `row_description_lines` lines of
    /// `width`.
    fn description_lines(
        &self,
        font: &Rc<LoadedFont>,
        description: &str,
        width: f32,
    ) -> Vec<String> {
        self.capped_lines(font, description, width, self.ui.row_description_lines)
    }

    /// `text` broken into at most `most` lines of `width`, the last cut
    /// short with an ellipsis. One line is left whole: `draw_text`
    /// ellipsises it.
    fn capped_lines(
        &self,
        font: &Rc<LoadedFont>,
        text: &str,
        width: f32,
        most: usize,
    ) -> Vec<String> {
        let most = most.max(1);
        if most == 1 || text.is_empty() {
            return vec![text.to_string()];
        }
        // Wrapping measures every prefix of the text; a frame re-wraps
        // every row, so the answer is kept. The key hashes the text, and
        // the entry keeps it to compare, so a lookup allocates nothing.
        let key = (font.id(), width.to_bits(), most, {
            use std::hash::{Hash, Hasher};
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            text.hash(&mut hasher);
            hasher.finish()
        });
        if let Some((wrapped, lines)) = self.wrapped_text.borrow().get(&key) {
            if wrapped == text {
                return lines.clone();
            }
        }
        let mut lines = self.wrap_settings_text(font, text, width);
        if lines.len() > most {
            // Words were split at spaces, which the join puts back; text
            // without spaces (CJK) was split between characters.
            let joiner = if text.contains(char::is_whitespace) {
                " "
            } else {
                ""
            };
            let rest = lines.split_off(most - 1).join(joiner);
            lines.push(self.text_with_ellipsis(font, &rest, width));
        }
        let mut cache = self.wrapped_text.borrow_mut();
        // As the shape cache does: no recency to evict by, and the page
        // on screen fills it again at once.
        if cache.len() >= WRAPPED_TEXT_CACHE_LIMIT {
            cache.clear();
        }
        cache.insert(key, (text.to_string(), lines.clone()));
        lines
    }

    /// The coloured square heading a row, centred on its label and the
    /// first line of its description. Returns where the row's text starts
    /// and how wide it may be, for the row painter to take over from.
    fn paint_row_tile(
        &self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
        icon: SvgIcon,
        color: TileColor,
    ) -> anyhow::Result<(f32, f32)> {
        let side = self.ui_px(ROW_TILE_SIDE);
        let block_bottom =
            self.settings_row_description_y(y) + self.metrics.cell_size.height as f32;
        self.paint_tile(
            layers,
            x,
            (y + block_bottom - side) / 2.0,
            side,
            icon,
            color,
        )?;
        let indent = side + self.ui_px(ROW_TILE_GAP);
        Ok((x + indent, (width - indent).max(0.0)))
    }

    /// A coloured square `side` pixels across with `icon` on it in white,
    /// lit like the tab icons (`ui::tile`).
    fn paint_tile(
        &self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        side: f32,
        icon: SvgIcon,
        color: TileColor,
    ) -> anyhow::Result<()> {
        let ctx = crate::ui::draw::DrawContext::new(
            self.render_state.as_ref().unwrap(),
            self.dimensions,
            &self.metrics,
        );
        crate::ui::tile::draw_tile(
            &ctx,
            layers,
            1,
            x,
            y,
            side,
            (side * ROW_TILE_RADIUS).round(),
            color.linear(),
            self.chrome_palette.is_dark(),
            &ROW_TILE,
        )?;
        let glyph = (side * ROW_TILE_GLYPH).round();
        self.draw_svg_icon(
            layers,
            icon,
            x + (side - glyph) / 2.0,
            y + (side - glyph) / 2.0,
            glyph,
            LinearRgba::with_srgba(0xFF, 0xFF, 0xFF, 0xFF),
        )
    }

    /// Remember where a dropdown's closed face was painted this frame, as
    /// the `(x, y, width)` its open menu hangs from.
    fn note_dropdown_anchor(&mut self, dropdown: SettingsDropdown, geometry: (f32, f32, f32)) {
        self.ui
            .dropdown_anchors
            .retain(|(noted, _)| *noted != dropdown);
        self.ui.dropdown_anchors.push((dropdown, geometry));
    }

    fn paint_label_row(
        &self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
        label: &str,
        description: &str,
        draw_top_rule: bool,
    ) -> anyhow::Result<f32> {
        let palette = self.palette();
        let ui_font = Rc::clone(&self.ui_font);
        if draw_top_rule {
            self.paint_separator(layers, x, y - self.ui_px(28.0), width)?;
        }
        self.draw_text(layers, &ui_font, x, y, label, palette.text, width)?;
        self.draw_row_description(layers, x, y, description, width)
    }

    fn paint_setting_row(
        &self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
        label: &str,
        description: &str,
        value: &str,
        draw_top_rule: bool,
    ) -> anyhow::Result<f32> {
        let palette = self.palette();
        let ui_font = Rc::clone(&self.ui_font);
        if draw_top_rule {
            self.paint_separator(layers, x, y - self.ui_px(28.0), width)?;
        }
        let control_width = self.settings_control_width(width);
        let control_x = x + width - control_width;
        let control_y = y + self.ui_px(4.0);
        let text_width = (control_x - x - self.ui_px(24.0)).max(width * 0.45);
        self.draw_text(layers, &ui_font, x, y, label, palette.text, text_width)?;
        let extra = self.draw_row_description(layers, x, y, description, text_width)?;
        self.draw_rounded_frame(
            layers,
            0,
            control_x,
            control_y,
            control_width,
            self.ui_px(CONTROL_HEIGHT),
            palette.control_bg,
            palette.control_border,
            self.ui_px(CONTROL_RADIUS),
        )?;
        self.draw_text(
            layers,
            &ui_font,
            control_x + self.ui_px(14.0),
            self.control_text_y(control_y, self.ui_px(CONTROL_HEIGHT)),
            value,
            palette.text,
            control_width - self.ui_px(26.0),
        )?;
        Ok(extra)
    }

    /// An iOS-style switch: a pill track with a circular knob. Replaces the
    /// On/Off word pill, which read as a value to be inspected rather than a
    /// control to be flipped.
    fn paint_switch(
        &self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        enabled: bool,
        hovered: bool,
        pressed: bool,
    ) -> anyhow::Result<()> {
        let palette = self.palette();
        let width = self.ui_px(SWITCH_WIDTH);
        let height = self.ui_px(SWITCH_HEIGHT);
        let inset = self.ui_px(SWITCH_KNOB_INSET);
        let knob = height - inset * 2.0;

        // Shared with the in-window switch, which this cannot call: that one
        // draws through a DrawContext and this window has its own primitives.
        let track = crate::ui::widgets::toggle_track_color(
            self.chrome_palette,
            enabled,
            hovered,
            pressed,
        );
        // Passing the fill as the border makes draw_rounded_frame skip the
        // ring: a stroke the same colour only hardens the pill's edge.
        let border = track;
        self.draw_rounded_frame(layers, 0, x, y, width, height, track, border, height / 2.0)?;

        let knob_x = if enabled {
            x + width - inset - knob
        } else {
            x + inset
        };
        self.draw_rounded_rect(
            layers,
            0,
            knob_x,
            y + inset,
            knob,
            knob,
            palette.on_accent,
            knob / 2.0,
        )
    }

    fn paint_toggle_setting_row(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
        label: &str,
        description: &str,
        enabled: bool,
        action: SettingsAction,
        draw_top_rule: bool,
    ) -> anyhow::Result<f32> {
        let palette = self.palette();
        let ui_font = Rc::clone(&self.ui_font);
        if draw_top_rule {
            self.paint_separator(layers, x, y - self.ui_px(28.0), width)?;
        }
        let control_width = self.settings_control_width(width);
        let control_x = x + width - control_width;
        let control_y = y + self.ui_px(4.0);
        let text_width = (control_x - x - self.ui_px(24.0)).max(width * 0.45);
        let control_rect = rect(
            control_x,
            control_y,
            control_width,
            self.ui_px(CONTROL_HEIGHT),
        );
        self.ui_context
            .push(control_rect, WidgetKind::Button, action);

        let hovered = self.ui.interaction.hovered == Some(action);
        let pressed = self.ui.interaction.pressed == Some(action);
        self.draw_text(layers, &ui_font, x, y, label, palette.text, text_width)?;
        let extra = self.draw_row_description(layers, x, y, description, text_width)?;
        // The switch sits at the right edge of the control column; the
        // whole column stays the hit target so the row is still easy to hit.
        self.paint_switch(
            layers,
            control_x + control_width - self.ui_px(SWITCH_WIDTH),
            control_y + (self.ui_px(CONTROL_HEIGHT) - self.ui_px(SWITCH_HEIGHT)) / 2.0,
            enabled,
            hovered,
            pressed,
        )?;
        Ok(extra)
    }

    /// A single-line band like `paint_toggle_setting_row_with_hint`, with a
    /// button on the right instead of a switch: the label and its hint
    /// icon on the left, the button centred in the band.
    fn paint_button_setting_row_with_hint(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
        band_height: f32,
        label: &str,
        hint_key: &'static str,
        button: &str,
        action: SettingsAction,
        draw_top_rule: bool,
    ) -> anyhow::Result<()> {
        if draw_top_rule {
            self.paint_separator(layers, x, y, width)?;
        }
        let control_height = self.ui_px(CONTROL_HEIGHT);
        let control_y = y + ((band_height - control_height) / 2.0).max(0.0);
        let button_width = self.button_width_for_label(button, 120.0);
        let button_x = x + width - button_width;
        let cell_height = self.metrics.cell_size.height as f32;
        let icon_size = self.ui_px(HINT_ICON_SIDE);
        let icon_gap = self.ui_px(HINT_ICON_GAP);
        let text_width = (button_x - x - icon_size - icon_gap - 24.0).max(width * 0.45);

        let label_y = control_y + (control_height - cell_height) / 2.0;
        self.paint_hinted_label(layers, x, label_y, text_width, label, hint_key)?;

        self.draw_button(layers, button_x, control_y, button_width, button, action)
    }

    /// A row's label with an ⓘ after it; hovering the icon shows
    /// `hint_key`'s text in a bubble (`paint_hint_overlay`). The label gets
    /// `text_width` and the icon follows whatever of it the label used.
    fn paint_hinted_label(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        label_y: f32,
        text_width: f32,
        label: &str,
        hint_key: &'static str,
    ) -> anyhow::Result<()> {
        let palette = self.palette();
        let ui_font = Rc::clone(&self.ui_font);
        let cell_height = self.metrics.cell_size.height as f32;
        let icon_size = self.ui_px(HINT_ICON_SIDE);
        let icon_gap = self.ui_px(HINT_ICON_GAP);
        self.draw_text(layers, &ui_font, x, label_y, label, palette.text, text_width)?;
        let shown = self.text_with_ellipsis(&ui_font, label, text_width);
        let label_width = self.measure_text_width(&ui_font, &shown);

        let icon_x = x + label_width + icon_gap;
        let icon_y = label_y + (cell_height - icon_size) / 2.0;
        let hint = SettingsAction::Hint(hint_key);
        let reach = self.ui_px(6.0);
        self.ui_context.push(
            rect(
                icon_x - reach,
                icon_y - reach,
                icon_size + reach * 2.0,
                icon_size + reach * 2.0,
            ),
            WidgetKind::Hint,
            hint,
        );
        let icon_hovered = self.ui.interaction.hovered == Some(hint);
        self.draw_svg_icon(
            layers,
            SvgIcon::Info,
            icon_x,
            icon_y,
            icon_size,
            if icon_hovered {
                palette.text
            } else {
                palette.muted_text
            },
        )?;
        if icon_hovered {
            self.ui.hint = Some((hint_key, icon_x, icon_y, icon_size));
        }
        Ok(())
    }

    /// A toggle row whose explanation lives behind an ⓘ after the label:
    /// hovering the icon shows it in a bubble (`paint_hint_overlay`), so a
    /// long one is neither cut short nor given a line of its own.
    fn paint_toggle_setting_row_with_hint(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
        band_height: f32,
        label: &str,
        hint_key: &'static str,
        enabled: bool,
        action: SettingsAction,
    ) -> anyhow::Result<()> {
        let control_width = self.settings_control_width(width);
        let control_x = x + width - control_width;
        let control_height = self.ui_px(CONTROL_HEIGHT);
        // `y` is the top of a single-line band; the switch and the label
        // sit in its middle.
        let control_y = y + ((band_height - control_height) / 2.0).max(0.0);
        let cell_height = self.metrics.cell_size.height as f32;
        let icon_size = self.ui_px(HINT_ICON_SIDE);
        let icon_gap = self.ui_px(HINT_ICON_GAP);
        let text_width = (control_x - x - icon_size - icon_gap - 24.0).max(width * 0.45);
        let control_rect = rect(control_x, control_y, control_width, control_height);
        self.ui_context
            .push(control_rect, WidgetKind::Button, action);
        let hovered = self.ui.interaction.hovered == Some(action);
        let pressed = self.ui.interaction.pressed == Some(action);

        // One line, centred on the switch.
        let label_y = control_y + (control_height - cell_height) / 2.0;
        self.paint_hinted_label(layers, x, label_y, text_width, label, hint_key)?;

        self.paint_switch(
            layers,
            control_x + control_width - self.ui_px(SWITCH_WIDTH),
            control_y + (control_height - self.ui_px(SWITCH_HEIGHT)) / 2.0,
            enabled,
            hovered,
            pressed,
        )?;
        Ok(())
    }

    fn paint_action_setting_row(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
        label: &str,
        description: &str,
        value: &str,
        action: SettingsAction,
        draw_top_rule: bool,
    ) -> anyhow::Result<f32> {
        let palette = self.palette();
        let ui_font = Rc::clone(&self.ui_font);
        if draw_top_rule {
            self.paint_separator(layers, x, y - self.ui_px(28.0), width)?;
        }
        // A button sized to its label and pushed to the column's right edge,
        // like System Settings. The column is the target, but a value can be
        // longer than it offers -- a colour scheme name runs to forty
        // characters -- so let the button borrow from the label side before
        // the label is ellipsised, the same trade `paint_segmented_setting_row`
        // makes.
        let column_width = self.settings_control_width(width);
        let control_width = self
            .button_width_for_label(value, 0.0)
            .min(column_width.max(width * 0.45));
        let control_x = x + width - control_width;
        let control_y = y + self.ui_px(4.0);
        let text_width = (control_x - x - self.ui_px(24.0)).max(width * 0.45);

        self.draw_text(layers, &ui_font, x, y, label, palette.text, text_width)?;
        let extra = self.draw_row_description(layers, x, y, description, text_width)?;
        self.draw_button(layers, control_x, control_y, control_width, value, action)?;
        Ok(extra)
    }

    /// A row whose control is a segmented control: one track holding every
    /// choice, the current one filled. Each segment is its own hit target
    /// carrying a *set* action, so a click lands on the value it shows
    /// rather than cycling to whatever comes next.
    fn paint_segmented_setting_row(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
        label: &str,
        description: &str,
        options: &[(String, SettingsAction)],
        selected: usize,
        draw_top_rule: bool,
    ) -> anyhow::Result<f32> {
        let palette = self.palette();
        let ui_font = Rc::clone(&self.ui_font);
        if draw_top_rule {
            self.paint_separator(layers, x, y - self.ui_px(28.0), width)?;
        }
        let (track_bg, segment_fill) = segmented_track_and_fill(&palette);
        let column_width = self.settings_control_width(width);
        let control_y = y + self.ui_px(4.0);
        let control_height = self.ui_px(CONTROL_HEIGHT);
        let inset = self.ui_px(3.0);

        // Natural width: every segment fits its label; if that overflows the
        // column, shrink the segments evenly and ellipsise their labels.
        let natural: Vec<f32> = options
            .iter()
            .map(|(text, _)| {
                (self.measure_text_width(&ui_font, text) + self.ui_px(28.0)).max(self.ui_px(72.0))
            })
            .collect();
        let natural_total: f32 = natural.iter().sum::<f32>() + inset * 2.0;
        // The column is the target width, but three labels can need more
        // than it offers (fonts scale with the point size, the column with
        // the UI scale, and the two diverge at 96 dpi). Let the control
        // borrow from the label side before ellipsising its options.
        let max_width = column_width.max(width * 0.45);
        let control_width = natural_total.min(max_width);
        let shrink = if natural_total > inset * 2.0 {
            (control_width - inset * 2.0) / (natural_total - inset * 2.0)
        } else {
            1.0
        };
        let control_x = x + width - control_width;
        let text_width = (control_x - x - self.ui_px(24.0)).max(width * 0.45);

        self.draw_text(layers, &ui_font, x, y, label, palette.text, text_width)?;
        let extra = self.draw_row_description(layers, x, y, description, text_width)?;
        self.draw_rounded_frame(
            layers,
            0,
            control_x,
            control_y,
            control_width,
            control_height,
            track_bg,
            palette.control_border,
            self.ui_px(CONTROL_RADIUS),
        )?;

        let mut segment_x = control_x + inset;
        let segment_y = control_y + inset;
        let segment_height = control_height - inset * 2.0;
        let segment_radius = (self.ui_px(CONTROL_RADIUS) - inset).max(self.ui_px(4.0));
        for (index, ((text, action), natural_width)) in
            options.iter().zip(natural.iter()).enumerate()
        {
            let segment_width = natural_width * shrink;
            let segment_rect = rect(segment_x, segment_y, segment_width, segment_height);
            self.ui_context
                .push(segment_rect, WidgetKind::Button, *action);
            let is_selected = index == selected;
            let hovered = self.ui.interaction.hovered == Some(*action);
            let pressed = self.ui.interaction.pressed == Some(*action);
            let fill = if is_selected {
                Some(segment_fill)
            } else if pressed {
                Some(palette.control_pressed_bg)
            } else if hovered {
                Some(palette.control_hover_bg)
            } else {
                None
            };
            if let Some(fill) = fill {
                self.draw_rounded_rect(
                    layers,
                    0,
                    segment_x,
                    segment_y,
                    segment_width,
                    segment_height,
                    fill,
                    segment_radius,
                )?;
            }
            let available = (segment_width - self.ui_px(12.0)).max(0.0);
            let shown = self.text_with_ellipsis(&ui_font, text, available);
            let shown_width = self.measure_text_width(&ui_font, &shown);
            let text_x = segment_x + ((segment_width - shown_width) / 2.0).max(self.ui_px(6.0));
            self.draw_text(
                layers,
                &ui_font,
                text_x,
                self.control_text_y(segment_y, segment_height),
                &shown,
                if is_selected || hovered || pressed {
                    palette.text
                } else {
                    palette.secondary_text
                },
                available,
            )?;
            segment_x += segment_width;
        }
        Ok(extra)
    }

    /// The window's opacity: a slider from `WINDOW_OPACITY_LEAST` to opaque
    /// and its value -- "Default" while the configuration decides, the knob
    /// then showing what that decided -- with a note on where it works.
    fn paint_window_opacity_row(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
        draw_top_rule: bool,
    ) -> anyhow::Result<f32> {
        use crate::native_settings::WINDOW_OPACITY_LEAST;
        let palette = self.palette();
        let accent = self.chrome_palette.accent;
        let ui_font = Rc::clone(&self.ui_font);
        let body_font = Rc::clone(&self.body_font);
        if draw_top_rule {
            self.paint_separator(layers, x, y - self.ui_px(28.0), width)?;
        }
        let supported = crate::native_settings::window_opacity_supported();
        let chosen = self
            .native_settings
            .appearance
            .window_opacity
            .filter(|_| supported)
            .map(|percent| percent.clamp(WINDOW_OPACITY_LEAST, 100));
        let percent = chosen
            .unwrap_or_else(|| {
                (configuration().window_background_opacity.clamp(0.0, 1.0) * 100.0).round() as u8
            })
            .clamp(WINDOW_OPACITY_LEAST, 100);

        let column_width = self.settings_control_width(width);
        let control_x = x + width - column_width;
        let text_width = (control_x - x - self.ui_px(24.0)).max(width * 0.45);
        let title = crate::i18n::tr("settings-window-opacity");
        self.draw_text(layers, &ui_font, x, y, &title, palette.text, text_width)?;
        let title_width = self.measure_text_width(&ui_font, &title).min(text_width);
        self.paint_beta_badge(
            layers,
            x + title_width + self.ui_px(14.0),
            y,
            x + text_width,
        )?;
        let description_extra = self.draw_row_description(
            layers,
            x,
            y,
            &crate::i18n::tr("settings-window-opacity-description"),
            text_width,
        )?;

        // A groove, the part of it up to the knob, the knob, and the value.
        let control_y = y + self.ui_px(4.0);
        let control_height = self.ui_px(CONTROL_HEIGHT);
        let center_y = control_y + control_height / 2.0;
        let knob = self.ui_px(22.0);
        let groove = self.ui_px(4.0);
        // The value beside the track is as wide as the widest it can show --
        // a fixed width cut "Default" short in English, and more in French --
        // with a gap from the knob, but never takes the track below its
        // least, where the knob at opaque would run into it; then it is cut.
        let value_gap = self.ui_px(12.0);
        let track_least = self.ui_px(48.0);
        let widest_value = [window_opacity_label(None), window_opacity_label(Some(100))]
            .iter()
            .map(|label| self.measure_text_width(&ui_font, label))
            .fold(self.ui_px(60.0), f32::max);
        let value_width =
            (widest_value + value_gap).min((column_width - knob - track_least).max(0.0));
        let value_room = (value_width - value_gap).max(0.0);
        let track_left = control_x + knob / 2.0;
        // Never past the column either, however narrow it gets.
        let track_width = (column_width - value_width - knob)
            .max(track_least.min((column_width - knob).max(0.0)));
        let along =
            f32::from(percent - WINDOW_OPACITY_LEAST) / f32::from(100 - WINDOW_OPACITY_LEAST);
        let knob_x = track_left + track_width * along;
        let set = chosen.is_some();
        self.draw_rounded_rect(
            layers,
            0,
            track_left,
            center_y - groove / 2.0,
            track_width,
            groove,
            palette.track_off,
            groove / 2.0,
        )?;
        self.draw_rounded_rect(
            layers,
            0,
            track_left,
            center_y - groove / 2.0,
            knob_x - track_left,
            groove,
            if set { accent } else { palette.muted_text },
            groove / 2.0,
        )?;
        let knob_fill = if supported {
            LinearRgba::with_components(1.0, 1.0, 1.0, 1.0)
        } else {
            palette.control_hover_bg
        };
        self.draw_rounded_frame(
            layers,
            0,
            knob_x - knob / 2.0,
            center_y - knob / 2.0,
            knob,
            knob,
            knob_fill,
            palette.control_border,
            knob / 2.0,
        )?;
        // Cut first and then measured, so a value that had to be cut still
        // sits flush right with the others.
        let value = self.text_with_ellipsis(&ui_font, &window_opacity_label(chosen), value_room);
        let value_text_width = self.measure_text_width(&ui_font, &value);
        self.draw_text(
            layers,
            &ui_font,
            x + width - value_text_width,
            self.control_text_y(control_y, control_height),
            &value,
            if set {
                palette.text
            } else {
                palette.secondary_text
            },
            value_room,
        )?;
        if supported {
            self.ui.window_opacity_track = Some((track_left, track_width));
            self.ui_context.push(
                rect(control_x, control_y, track_width + knob, control_height),
                WidgetKind::Button,
                SettingsAction::WindowOpacitySlider,
            );
        } else {
            self.ui.window_opacity_track = None;
        }
        let step = self.description_line_step();
        let mut control_bottom = control_y + control_height;
        // Back to the configuration's, once Settings has one of its own.
        if set {
            let reset = crate::i18n::tr("settings-window-opacity-reset");
            let reset_width = self.measure_text_width(&body_font, &reset);
            let reset_x = x + width - reset_width;
            let reset_y = control_bottom + self.ui_px(6.0);
            let action = SettingsAction::SetWindowOpacity(None);
            let hovered = self.ui.interaction.hovered == Some(action);
            self.draw_text(
                layers,
                &body_font,
                reset_x,
                reset_y,
                &reset,
                if hovered { palette.text } else { accent },
                reset_width,
            )?;
            self.ui_context.push(
                rect(reset_x, reset_y, reset_width, step),
                WidgetKind::Button,
                action,
            );
            control_bottom = reset_y + step;
        }

        // Where the slider is not offered, and where it is not yet reliable,
        // said plainly, as the app icon's row says where it works.
        let note_y = self.settings_row_description_y(y) + step + description_extra;
        let last_note_y = note_y
            + self.draw_row_note(
                layers,
                x,
                note_y,
                &crate::i18n::tr("settings-window-opacity-platforms"),
                text_width,
            )?;
        let cell_height = self.metrics.cell_size.height as f32;
        let bottom = (last_note_y + cell_height).max(control_bottom) + self.ui_px(6.0);
        Ok((bottom - (y + self.settings_row_visual_height())).max(0.0))
    }

    /// A small "Beta" capsule starting at `x` after a title drawn at
    /// `title_y`, centred on the title's capitals. Left out when it would
    /// run past `right_edge`.
    fn beta_badge_width(&self) -> f32 {
        self.badge_width(&crate::i18n::tr("settings-badge-beta"))
    }

    fn paint_beta_badge(
        &self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        title_y: f32,
        right_edge: f32,
    ) -> anyhow::Result<()> {
        let label = crate::i18n::tr("settings-badge-beta");
        let accent = self.chrome_palette.accent;
        self.paint_badge(layers, &label, accent, x, title_y, right_edge)
    }

    fn badge_width(&self, label: &str) -> f32 {
        (self.measure_text_width(&self.logo_caption_font, label) + self.ui_px(12.0) * 2.0).round()
    }

    /// The capsule `paint_beta_badge` draws, with any label and color.
    fn paint_badge(
        &self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        label: &str,
        accent: LinearRgba,
        x: f32,
        title_y: f32,
        right_edge: f32,
    ) -> anyhow::Result<()> {
        let font = Rc::clone(&self.logo_caption_font);
        let pad = self.ui_px(12.0);
        let width = self.badge_width(label);
        if x + width > right_edge {
            return Ok(());
        }
        let cap_height = |metrics: FontMetrics| {
            metrics
                .cap_height
                .map(|cap| cap.get() as f32)
                .unwrap_or(metrics.cell_height.get() as f32 * 0.58)
        };
        // `draw_text` puts the baseline this far below the y it is given,
        // whatever the font.
        let baseline_offset =
            self.metrics.cell_size.height as f32 + self.metrics.descender.get() as f32;
        let center_y = title_y + baseline_offset - cap_height(self.ui_font.metrics()) / 2.0;
        let label_cap = cap_height(font.metrics());
        let height = (label_cap + self.ui_px(16.0)).round();
        self.draw_rounded_rect(
            layers,
            0,
            x,
            (center_y - height / 2.0).round(),
            width,
            height,
            accent.mul_alpha(0.16),
            height / 2.0,
        )?;
        self.draw_text(
            layers,
            &font,
            x + pad,
            (center_y + label_cap / 2.0 - baseline_offset).round(),
            label,
            accent,
            width - pad,
        )
    }

    /// The closed face of a dropdown: a pill wide enough for its current
    /// value plus the chevron, capped at the column and flush with the
    /// column's right edge (where the open menu also anchors).
    fn dropdown_pill_rect(
        &self,
        control_x: f32,
        control_y: f32,
        column_width: f32,
        label: &str,
    ) -> window::RectF {
        // The 60 is what `paint_dropdown_pill` takes back out of the width for
        // the inset and the chevron, so a label sized to exactly `text + 60`
        // is handed a budget of exactly its own width -- which lands on the
        // truncation boundary and loses its last character. "Dark" drew as
        // "Dar". The slack is for that boundary, not for looks.
        let natural = self.measure_text_width(&self.ui_font, label) + self.ui_px(68.0);
        let pill_width = natural.max(self.ui_px(120.0)).min(column_width);
        rect(
            control_x + column_width - pill_width,
            control_y,
            pill_width,
            self.ui_px(CONTROL_HEIGHT),
        )
    }

    fn paint_dropdown_pill(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        pill: window::RectF,
        label: &str,
        bg: LinearRgba,
        border: LinearRgba,
    ) -> anyhow::Result<()> {
        let palette = self.palette();
        let ui_font = Rc::clone(&self.ui_font);
        self.draw_rounded_frame(
            layers,
            0,
            pill.origin.x,
            pill.origin.y,
            pill.size.width,
            pill.size.height,
            bg,
            border,
            self.ui_px(CONTROL_RADIUS),
        )?;
        self.draw_text(
            layers,
            &ui_font,
            pill.origin.x + self.ui_px(16.0),
            self.control_text_y(pill.origin.y, pill.size.height),
            label,
            palette.text,
            pill.size.width - self.ui_px(60.0),
        )?;
        self.draw_svg_icon(
            layers,
            SvgIcon::ChevronDown,
            pill.origin.x + pill.size.width - self.ui_px(38.0),
            pill.origin.y + (pill.size.height - self.ui_px(22.0)) / 2.0,
            self.ui_px(22.0),
            palette.secondary_text,
        )?;
        Ok(())
    }

    fn paint_import_field_row(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
        field: &ImportableField,
        draw_top_rule: bool,
    ) -> anyhow::Result<()> {
        let palette = self.palette();
        let ui_font = Rc::clone(&self.ui_font);
        let body_font = Rc::clone(&self.import_body_font);
        let action = SettingsAction::ToggleImportField(field.id);
        let enabled = field.lua_value.is_some();
        let selected = enabled && self.import_field_selected(field.id);
        let hovered = enabled && self.ui.interaction.hovered == Some(action);
        let pressed = enabled && self.ui.interaction.pressed == Some(action);

        if draw_top_rule {
            self.paint_separator(layers, x, y - self.ui_px(28.0), width)?;
        }

        let row_rect = rect(
            x - self.ui_px(14.0),
            y - self.ui_px(18.0),
            width + self.ui_px(28.0),
            self.settings_row_visual_height() + self.ui_px(18.0),
        );
        if enabled {
            self.ui_context.push(row_rect, WidgetKind::Button, action);
        }
        if hovered || pressed {
            let bg = if pressed {
                palette.control_pressed_bg
            } else {
                palette.control_hover_bg
            };
            self.draw_rounded_rect(
                layers,
                0,
                row_rect.origin.x,
                row_rect.origin.y,
                row_rect.size.width,
                row_rect.size.height,
                bg,
                self.ui_px(16.0),
            )?;
        }

        let checkbox_size = self.ui_px(28.0);
        let checkbox_x = x;
        let checkbox_y = y + self.ui_px(6.0);
        let checkbox_fill = if selected {
            self.chrome_palette.accent
        } else if hovered {
            palette.control_hover_bg
        } else {
            palette.control_bg
        };
        let checkbox_border = if selected || hovered {
            self.chrome_palette.accent
        } else {
            palette.control_border
        };
        self.draw_rounded_frame(
            layers,
            0,
            checkbox_x,
            checkbox_y,
            checkbox_size,
            checkbox_size,
            checkbox_fill,
            checkbox_border,
            self.ui_px(8.0),
        )?;
        if selected {
            self.draw_svg_icon(
                layers,
                SvgIcon::Check,
                checkbox_x + self.ui_px(5.0),
                checkbox_y + self.ui_px(5.0),
                checkbox_size - self.ui_px(10.0),
                palette.on_accent,
            )?;
        }

        let label_x = x + checkbox_size + self.ui_px(18.0);
        let value_width = if width >= 760.0 {
            280.0_f32.min(width * 0.30)
        } else {
            210.0_f32.min(width * 0.34)
        };
        let value_x = x + width - value_width;
        let text_width = (value_x - label_x - self.ui_px(28.0)).max(width * 0.42);
        let title = &field.label;
        self.draw_text(
            layers,
            &ui_font,
            label_x,
            y,
            title,
            if enabled {
                palette.text
            } else {
                palette.muted_text
            },
            text_width,
        )?;
        self.draw_text(
            layers,
            &body_font,
            label_x,
            self.settings_row_description_y(y),
            &field.description,
            palette.secondary_text,
            text_width,
        )?;

        let preview_y = y + self.ui_px(4.0);
        self.draw_rounded_frame(
            layers,
            0,
            value_x,
            preview_y,
            value_width,
            self.ui_px(CONTROL_HEIGHT),
            palette.control_bg,
            palette.control_border,
            self.ui_px(CONTROL_RADIUS),
        )?;
        let preview =
            self.text_with_ellipsis(&ui_font, &field.preview, value_width - self.ui_px(28.0));
        self.draw_text(
            layers,
            &ui_font,
            value_x + self.ui_px(14.0),
            self.control_text_y(preview_y, self.ui_px(CONTROL_HEIGHT)),
            &preview,
            palette.text,
            value_width - self.ui_px(28.0),
        )?;
        Ok(())
    }

    fn paint_group_card(
        &self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
        height: f32,
    ) -> anyhow::Result<()> {
        let palette = self.palette();
        self.draw_rounded_frame(
            layers,
            0,
            x,
            y,
            width,
            height,
            palette.card_bg,
            palette.separator,
            self.ui_px(34.0),
        )
    }

    /// The soft drop shadow the main window puts under a selected sidebar
    /// row. Same two-pass ramp and the same alphas, so the two sidebars read
    /// as one surface treatment -- see
    /// `termwindow::ui::paint_active_surface_shadow` for why light and dark
    /// need different numbers rather than one shared ramp.
    fn paint_active_row_shadow(
        &self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
        height: f32,
        radius: f32,
    ) -> anyhow::Result<()> {
        let (outer, inner) = match self.effective_appearance() {
            Appearance::Light | Appearance::LightHighContrast => (0.04, 0.05),
            Appearance::Dark | Appearance::DarkHighContrast => (0.12, 0.18),
        };
        for (spread, offset_y, alpha) in [
            (self.ui_px(2.0), self.ui_px(1.0), outer),
            (self.ui_px(1.0), self.ui_px(1.0), inner),
        ] {
            self.draw_rounded_rect(
                layers,
                0,
                x - spread,
                y - spread + offset_y,
                width + spread * 2.0,
                height + spread * 2.0,
                LinearRgba::with_components(0.0, 0.0, 0.0, alpha),
                radius + spread,
            )?;
        }
        Ok(())
    }

    fn paint_separator(
        &self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
    ) -> anyhow::Result<()> {
        let palette = self.palette();
        let height = self.ui_px(2.0).max(1.0);
        self.draw_rounded_rect(layers, 0, x, y, width, height, palette.rule, height / 2.0)
    }

    fn paint_font_size_stepper_row(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
        label: &str,
        description: &str,
        value: f64,
        value_label: Option<&str>,
        reset_action: SettingsAction,
        decrease_action: SettingsAction,
        increase_action: SettingsAction,
        draw_top_rule: bool,
    ) -> anyhow::Result<f32> {
        let palette = self.palette();
        let ui_font = Rc::clone(&self.ui_font);
        if draw_top_rule {
            self.paint_separator(layers, x, y - self.ui_px(28.0), width)?;
        }
        let reset_x = self.paint_font_size_stepper(
            layers,
            x,
            y + self.ui_px(4.0),
            width,
            value,
            value_label,
            reset_action,
            decrease_action,
            increase_action,
        )?;
        let text_width = (reset_x - x - self.ui_px(24.0)).max(width * 0.40);
        self.draw_text(layers, &ui_font, x, y, label, palette.text, text_width)?;
        self.draw_row_description(layers, x, y, description, text_width)
    }

    /// A size's reset button and its − value + stepper, at the right of
    /// `width` from `x` with their tops at `control_y`. Returns where the
    /// reset button starts, for the row's text to stop short of.
    fn paint_font_size_stepper(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        control_y: f32,
        width: f32,
        value: f64,
        value_label: Option<&str>,
        reset_action: SettingsAction,
        decrease_action: SettingsAction,
        increase_action: SettingsAction,
    ) -> anyhow::Result<f32> {
        let palette = self.palette();
        let ui_font = Rc::clone(&self.ui_font);
        let value_label = value_label.map(str::to_string).unwrap_or_else(|| {
            if value >= 100.0 && value.fract().abs() < f64::EPSILON {
                format!("{value:.0}")
            } else {
                format!("{value:.1}")
            }
        });
        // Wide enough for the value between the buttons: "15 min" does not
        // fit the width a bare size does.
        let wanted =
            self.measure_text_width(&ui_font, &value_label) + self.ui_px(62.0 * 2.0 + 28.0);
        let control_width = self
            .ui_px(200.0)
            .max(wanted)
            .min(self.settings_control_width(width));
        let control_x = x + width - control_width;
        let dynamic_icon_size =
            (self.metrics.cell_size.height as f32 + 4.0).clamp(self.ui_px(24.0), self.ui_px(34.0));
        let reset_size = self.ui_px(CONTROL_HEIGHT);
        let reset_gap = self.ui_px(14.0);
        let reset_x = (control_x - reset_gap - reset_size).max(x + width * 0.62);

        self.paint_icon_button(
            layers,
            reset_x,
            control_y,
            reset_size,
            SvgIcon::RotateCcw,
            reset_action,
        )?;

        self.draw_rounded_frame(
            layers,
            0,
            control_x,
            control_y,
            control_width,
            self.ui_px(CONTROL_HEIGHT),
            palette.control_bg,
            palette.control_border,
            self.ui_px(CONTROL_RADIUS),
        )?;

        let button_width = self.ui_px(62.0).min(control_width * 0.28);
        let minus_rect = rect(
            control_x,
            control_y,
            button_width,
            self.ui_px(CONTROL_HEIGHT),
        );
        let plus_rect = rect(
            control_x + control_width - button_width,
            control_y,
            button_width,
            self.ui_px(CONTROL_HEIGHT),
        );
        self.ui_context
            .push(minus_rect, WidgetKind::Button, decrease_action);
        self.ui_context
            .push(plus_rect, WidgetKind::Button, increase_action);

        for (button_rect, action, icon) in [
            (minus_rect, decrease_action, SvgIcon::Minus),
            (plus_rect, increase_action, SvgIcon::Plus),
        ] {
            if self.ui.interaction.hovered == Some(action)
                || self.ui.interaction.pressed == Some(action)
            {
                self.draw_rounded_rect(
                    layers,
                    1,
                    button_rect.origin.x + self.ui_px(4.0),
                    button_rect.origin.y + self.ui_px(4.0),
                    button_rect.size.width - self.ui_px(8.0),
                    button_rect.size.height - self.ui_px(8.0),
                    palette.control_hover_bg,
                    self.ui_px(CONTROL_RADIUS - 4.0),
                )?;
            }
            self.draw_svg_icon(
                layers,
                icon,
                button_rect.origin.x + (button_rect.size.width - dynamic_icon_size) / 2.0,
                button_rect.origin.y + (button_rect.size.height - dynamic_icon_size) / 2.0,
                dynamic_icon_size,
                palette.secondary_text,
            )?;
        }

        let value_width = control_width - button_width * 2.0;
        let value_x = control_x + button_width;
        let value_room = (value_width - self.ui_px(16.0)).max(0.0);
        let shown = self.text_with_ellipsis(&ui_font, &value_label, value_room);
        let shown_width = self.measure_text_width(&ui_font, &shown).min(value_room);
        self.draw_text(
            layers,
            &ui_font,
            (value_x + (value_width - shown_width) / 2.0).round(),
            self.control_text_y(control_y, self.ui_px(CONTROL_HEIGHT)),
            &shown,
            palette.text,
            value_room,
        )?;

        Ok(reset_x)
    }

    fn paint_text_setting_row(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
        label: &str,
        description: &str,
        value: &str,
        placeholder: &str,
        action: SettingsAction,
        draw_top_rule: bool,
    ) -> anyhow::Result<f32> {
        let palette = self.palette();
        let ui_font = Rc::clone(&self.ui_font);
        if draw_top_rule {
            self.paint_separator(layers, x, y - self.ui_px(28.0), width)?;
        }

        // Wider than the other controls: what goes in it is a path, and a
        // path cut short in its own field hides the part that differs.
        let control_width = (width * 0.42).max(self.settings_control_width(width));
        let control_x = x + width - control_width;
        let control_y = y + self.ui_px(4.0);
        let text_width = (control_x - x - self.ui_px(24.0)).max(width * 0.45);
        let control_rect = rect(
            control_x,
            control_y,
            control_width,
            self.ui_px(CONTROL_HEIGHT),
        );

        self.draw_text(layers, &ui_font, x, y, label, palette.text, text_width)?;
        let extra = self.draw_row_description(layers, x, y, description, text_width)?;
        self.paint_value_input_box(layers, control_rect, action, value, placeholder)?;
        Ok(extra)
    }

    /// A one-line value field: frame, text or placeholder, selection and
    /// caret. The setting rows that carry one draw it through here, and so
    /// do pages that place their fields themselves.
    fn paint_value_input_box(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        control_rect: window::RectF,
        action: SettingsAction,
        value: &str,
        placeholder: &str,
    ) -> anyhow::Result<()> {
        let palette = self.palette();
        let ui_font = Rc::clone(&self.ui_font);
        let control_x = control_rect.origin.x;
        let control_y = control_rect.origin.y;
        let control_width = control_rect.size.width;
        let focused = self.ui.interaction.focused == Some(action);
        let hovered = self.ui.interaction.hovered == Some(action);
        let pressed = self.ui.interaction.pressed == Some(action);
        let bg = if pressed {
            palette.control_pressed_bg
        } else if focused || hovered {
            palette.control_hover_bg
        } else {
            palette.control_bg
        };
        let border = if focused {
            palette.nav_selected_bg
        } else if hovered || pressed {
            palette.separator
        } else {
            palette.control_border
        };
        self.ui_context
            .push(control_rect, WidgetKind::TextInput, action);
        self.draw_rounded_frame(
            layers,
            0,
            control_x,
            control_y,
            control_width,
            control_rect.size.height,
            bg,
            border,
            self.ui_px(CONTROL_RADIUS),
        )?;

        let display = if value.trim().is_empty() {
            placeholder
        } else {
            value
        };
        let text_color = if value.trim().is_empty() {
            palette.muted_text
        } else {
            palette.text
        };
        let text_left = control_x + self.ui_px(16.0);
        let text_area = (control_width - self.ui_px(32.0)).max(0.0);
        let selection = self
            .caret_for_input(action)
            .and_then(|caret| caret.selection)
            .filter(|(start, end)| start != end)
            .or_else(|| {
                let selected_all = self
                    .input_state_for(action)
                    .is_some_and(|input| input.selected_all);
                (selected_all && !value.is_empty()).then(|| (0, value.chars().count()))
            });
        if focused && !value.is_empty() {
            if let Some((start, end)) = selection {
                let start_x = self
                    .text_width_to_char(&ui_font, value, start)
                    .min(text_area);
                let end_x = self.text_width_to_char(&ui_font, value, end).min(text_area);
                self.draw_rounded_rect(
                    layers,
                    1,
                    text_left + start_x - self.ui_px(4.0),
                    control_y + self.ui_px(6.0),
                    (end_x - start_x) + self.ui_px(8.0),
                    self.ui_px(CONTROL_HEIGHT - 12.0),
                    palette.nav_selected_bg.mul_alpha(0.56),
                    self.ui_px(CONTROL_RADIUS - 4.0),
                )?;
            }
        }
        self.draw_text(
            layers,
            &ui_font,
            text_left,
            self.control_text_y(control_y, self.ui_px(CONTROL_HEIGHT)),
            display,
            text_color,
            text_area,
        )?;
        if focused && selection.is_none() {
            let caret_text = if value.trim().is_empty() { "" } else { value };
            let caret_dx = match self.caret_for_input(action) {
                Some(caret) => self.text_width_to_char(&ui_font, caret_text, caret.cursor),
                None => self.measure_text_width(&ui_font, caret_text),
            };
            let caret_width = self.ui_px(3.0).max(1.0);
            self.draw_rect(
                layers,
                1,
                text_left + caret_dx.min(text_area) - caret_width / 3.0,
                control_y + self.ui_px(8.0),
                caret_width,
                self.ui_px(CONTROL_HEIGHT - 16.0),
                palette.nav_selected_bg,
            )?;
        }
        Ok(())
    }

    /// How long the next link lasts. A dropdown rather than a fixed value
    /// because both ends are legitimate: an afternoon on a borrowed phone,
    /// and a link in a password manager for a machine reached every day.
    fn paint_web_link_ttl_row(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
        draw_top_rule: bool,
    ) -> anyhow::Result<f32> {
        let palette = self.palette();
        let ui_font = Rc::clone(&self.ui_font);
        if draw_top_rule {
            self.paint_separator(layers, x, y - self.ui_px(28.0), width)?;
        }

        let (control_x, control_y, control_width) = self.dropdown_control_geometry(x, y, width);
        self.note_dropdown_anchor(
            SettingsDropdown::WebLinkTtl,
            (control_x, control_y, control_width),
        );
        let text_width = (control_x - x - self.ui_px(24.0)).max(width * 0.45);
        let action = SettingsAction::ToggleWebLinkTtlMenu;
        let dropdown_label = (&web_link_ttl_label(self.native_settings.web.link_ttl_secs)).to_string();
        let control_rect =
            self.dropdown_pill_rect(control_x, control_y, control_width, &dropdown_label);
        let open = self.ui.open_dropdown == Some(SettingsDropdown::WebLinkTtl);
        let hovered = self.ui.interaction.hovered == Some(action);
        let pressed = self.ui.interaction.pressed == Some(action);
        let bg = if pressed || hovered {
            palette.control_hover_bg
        } else {
            palette.control_bg
        };
        let border = if open {
            palette.nav_selected_bg
        } else if hovered || pressed {
            palette.separator
        } else {
            palette.control_border
        };

        self.ui_context
            .push(control_rect, WidgetKind::Button, action);
        self.draw_text(
            layers,
            &ui_font,
            x,
            y,
            &crate::i18n::tr("settings-web-ttl"),
            palette.text,
            text_width,
        )?;
        let extra = self.draw_row_description(
            layers,
            x,
            y,
            &crate::i18n::tr("settings-web-ttl-description"),
            text_width,
        )?;
        self.paint_dropdown_pill(layers, control_rect, &dropdown_label, bg, border)?;
        Ok(extra)
    }

    fn paint_web_link_ttl_menu(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
    ) -> anyhow::Result<()> {
        let current = self.native_settings.web.link_ttl_secs;
        let options: Vec<(String, SettingsAction, bool)> = WEB_LINK_TTL_CHOICES
            .iter()
            .copied()
            .map(|ttl| {
                (
                    web_link_ttl_label(ttl),
                    SettingsAction::SetWebLinkTtl(ttl),
                    current == ttl,
                )
            })
            .collect();
        self.paint_dropdown_menu(layers, x, y, width, &options)
    }

    fn paint_command_palette_hotkey_row(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
        draw_top_rule: bool,
    ) -> anyhow::Result<()> {
        let palette = self.palette();
        let ui_font = Rc::clone(&self.ui_font);
        let body_font = Rc::clone(&self.body_font);
        if draw_top_rule {
            self.paint_separator(layers, x, y - self.ui_px(28.0), width)?;
        }

        let (control_x, control_y, control_width) = self.dropdown_control_geometry(x, y, width);
        self.note_dropdown_anchor(
            SettingsDropdown::CommandPaletteHotkey,
            (control_x, control_y, control_width),
        );
        let text_width = (control_x - x - self.ui_px(24.0)).max(width * 0.45);
        let action = SettingsAction::ToggleCommandPaletteHotkeyMenu;
        let dropdown_label = (self.native_settings.command_palette.hotkey.label()).to_string();
        let control_rect =
            self.dropdown_pill_rect(control_x, control_y, control_width, &dropdown_label);
        let open = self.ui.open_dropdown == Some(SettingsDropdown::CommandPaletteHotkey);
        let hovered = self.ui.interaction.hovered == Some(action);
        let pressed = self.ui.interaction.pressed == Some(action);
        let bg = if pressed || hovered {
            palette.control_hover_bg
        } else {
            palette.control_bg
        };
        let border = if open {
            palette.nav_selected_bg
        } else if hovered || pressed {
            palette.separator
        } else {
            palette.control_border
        };

        self.ui_context
            .push(control_rect, WidgetKind::Button, action);
        self.draw_text(
            layers,
            &ui_font,
            x,
            y,
            &crate::i18n::tr("settings-command-palette-hotkey"),
            palette.text,
            text_width,
        )?;
        self.draw_text(
            layers,
            &body_font,
            x,
            self.settings_row_description_y(y),
            &crate::i18n::tr("settings-command-palette-hotkey-description"),
            palette.secondary_text,
            text_width,
        )?;
        self.paint_dropdown_pill(layers, control_rect, &dropdown_label, bg, border)?;

        Ok(())
    }

    fn paint_command_palette_hotkey_menu(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
    ) -> anyhow::Result<()> {
        use crate::native_settings::NativeCommandPaletteHotkey as Hotkey;
        let current = self.native_settings.command_palette.hotkey;
        let options: Vec<(String, SettingsAction, bool)> = [
            Hotkey::CmdShiftP,
            Hotkey::CmdP,
            Hotkey::CmdK,
            Hotkey::CtrlShiftP,
        ]
        .iter()
        .copied()
        .map(|hotkey| {
            (
                hotkey.label().to_string(),
                SettingsAction::SetCommandPaletteHotkey(hotkey),
                current == hotkey,
            )
        })
        .collect();
        self.paint_dropdown_menu(layers, x, y, width, &options)
    }

    fn paint_language_row(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
        draw_top_rule: bool,
    ) -> anyhow::Result<f32> {
        let palette = self.palette();
        let ui_font = Rc::clone(&self.ui_font);
        if draw_top_rule {
            self.paint_separator(layers, x, y - self.ui_px(28.0), width)?;
        }

        let (control_x, control_y, control_width) = self.dropdown_control_geometry(x, y, width);
        self.note_dropdown_anchor(
            SettingsDropdown::Language,
            (control_x, control_y, control_width),
        );
        let text_width = (control_x - x - self.ui_px(24.0)).max(width * 0.45);
        let action = SettingsAction::ToggleLanguageMenu;
        let dropdown_label = (&crate::i18n::configured_language_label(&self.native_settings)).to_string();
        let control_rect =
            self.dropdown_pill_rect(control_x, control_y, control_width, &dropdown_label);
        let open = self.ui.open_dropdown == Some(SettingsDropdown::Language);
        let hovered = self.ui.interaction.hovered == Some(action);
        let pressed = self.ui.interaction.pressed == Some(action);
        let bg = if pressed || hovered {
            palette.control_hover_bg
        } else {
            palette.control_bg
        };
        let border = if open {
            palette.nav_selected_bg
        } else if hovered || pressed {
            palette.separator
        } else {
            palette.control_border
        };

        self.ui_context
            .push(control_rect, WidgetKind::Button, action);
        self.draw_text(
            layers,
            &ui_font,
            x,
            y,
            &crate::i18n::tr("settings-language"),
            palette.text,
            text_width,
        )?;
        let extra = self.draw_row_description(
            layers,
            x,
            y,
            &crate::i18n::tr("settings-language-description"),
            text_width,
        )?;
        self.paint_dropdown_pill(layers, control_rect, &dropdown_label, bg, border)?;
        Ok(extra)
    }

    fn refresh_shell_catalog(&mut self) {
        self.ui.shell_catalog =
            shell_catalog_including(self.native_settings.terminal.default_shell.as_ref());
    }

    /// What the "no choice" option means right now.
    ///
    /// Clearing the choice does not force the platform login shell: it
    /// removes the override, and `LocalDomain::build_command` then falls
    /// back to the Lua `default_prog` when the config sets one. Labelling
    /// that "System default" would be a plain lie about what the pane
    /// will run.
    fn no_override_label(&self) -> String {
        if config::configuration().default_prog.is_some() {
            crate::i18n::tr("settings-default-shell-follow-lua")
        } else {
            crate::i18n::tr("settings-default-shell-system")
        }
    }

    /// What the closed dropdown shows. A shell that has since been
    /// uninstalled still names itself rather than silently reading as the
    /// system default, so the user can see why their panes changed.
    fn current_default_shell_label(&self) -> String {
        let Some(argv) = self.native_settings.terminal.default_shell.as_ref() else {
            return self.no_override_label();
        };
        let Some(program) = argv.first() else {
            return self.no_override_label();
        };
        self.ui
            .shell_catalog
            .iter()
            .find(|shell| &shell.argv == argv)
            .map(|shell| shell.label.clone())
            .unwrap_or_else(|| program.clone())
    }

    fn paint_default_shell_row(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
        draw_top_rule: bool,
    ) -> anyhow::Result<f32> {
        let palette = self.palette();
        let ui_font = Rc::clone(&self.ui_font);
        if draw_top_rule {
            self.paint_separator(layers, x, y - self.ui_px(28.0), width)?;
        }

        let (control_x, control_y, control_width) = self.dropdown_control_geometry(x, y, width);
        self.note_dropdown_anchor(
            SettingsDropdown::DefaultShell,
            (control_x, control_y, control_width),
        );
        let text_width = (control_x - x - self.ui_px(24.0)).max(width * 0.45);
        let action = SettingsAction::ToggleDefaultShellMenu;
        let dropdown_label = (&self.current_default_shell_label()).to_string();
        let control_rect =
            self.dropdown_pill_rect(control_x, control_y, control_width, &dropdown_label);
        let open = self.ui.open_dropdown == Some(SettingsDropdown::DefaultShell);
        let hovered = self.ui.interaction.hovered == Some(action);
        let pressed = self.ui.interaction.pressed == Some(action);
        let bg = if pressed || hovered {
            palette.control_hover_bg
        } else {
            palette.control_bg
        };
        let border = if open {
            palette.nav_selected_bg
        } else if hovered || pressed {
            palette.separator
        } else {
            palette.control_border
        };

        // A Lua `default_prog` still exists in many configs; say plainly
        // that this row now decides, instead of leaving the user to wonder
        // why their config line stopped mattering.
        let description = if config::configuration().default_prog.is_some() {
            crate::i18n::tr("settings-default-shell-overrides-lua")
        } else {
            crate::i18n::tr("settings-default-shell-description")
        };

        self.ui_context
            .push(control_rect, WidgetKind::Button, action);
        self.draw_text(
            layers,
            &ui_font,
            x,
            y,
            &crate::i18n::tr("settings-default-shell"),
            palette.text,
            text_width,
        )?;
        let extra = self.draw_row_description(layers, x, y, &description, text_width)?;
        self.paint_dropdown_pill(layers, control_rect, &dropdown_label, bg, border)?;

        Ok(extra)
    }

    fn paint_default_shell_menu(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
    ) -> anyhow::Result<()> {
        let chosen = self.native_settings.terminal.default_shell.clone();
        let mut options = vec![(
            self.no_override_label(),
            SettingsAction::SetDefaultShell(None),
            chosen.is_none(),
        )];
        for (index, shell) in self.ui.shell_catalog.iter().enumerate() {
            options.push((
                shell.label.clone(),
                SettingsAction::SetDefaultShell(Some(index)),
                chosen.as_ref() == Some(&shell.argv),
            ));
        }
        self.paint_dropdown_menu(layers, x, y, width, &options)
    }

    fn paint_main_renderer_row(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
        draw_top_rule: bool,
    ) -> anyhow::Result<f32> {
        let palette = self.palette();
        let ui_font = Rc::clone(&self.ui_font);
        if draw_top_rule {
            self.paint_separator(layers, x, y - self.ui_px(28.0), width)?;
        }

        let (control_x, control_y, control_width) = self.dropdown_control_geometry(x, y, width);
        self.note_dropdown_anchor(
            SettingsDropdown::MainRenderer,
            (control_x, control_y, control_width),
        );
        let text_width = (control_x - x - self.ui_px(24.0)).max(width * 0.45);
        let action = SettingsAction::ToggleMainRendererMenu;
        let dropdown_label = (self.current_main_renderer().label()).to_string();
        let control_rect =
            self.dropdown_pill_rect(control_x, control_y, control_width, &dropdown_label);
        let open = self.ui.open_dropdown == Some(SettingsDropdown::MainRenderer);
        let hovered = self.ui.interaction.hovered == Some(action);
        let pressed = self.ui.interaction.pressed == Some(action);
        let bg = if pressed || hovered {
            palette.control_hover_bg
        } else {
            palette.control_bg
        };
        let border = if open {
            palette.nav_selected_bg
        } else if hovered || pressed {
            palette.separator
        } else {
            palette.control_border
        };

        self.ui_context
            .push(control_rect, WidgetKind::Button, action);
        self.draw_text(
            layers,
            &ui_font,
            x,
            y,
            &crate::i18n::tr("settings-main-renderer"),
            palette.text,
            text_width,
        )?;
        let extra = self.draw_row_description(
            layers,
            x,
            y,
            &crate::i18n::tr("settings-main-renderer-description"),
            text_width,
        )?;
        self.paint_dropdown_pill(layers, control_rect, &dropdown_label, bg, border)?;

        Ok(extra)
    }

    fn paint_restart_row(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
        draw_top_rule: bool,
    ) -> anyhow::Result<f32> {
        let palette = self.palette();
        let ui_font = Rc::clone(&self.ui_font);
        if draw_top_rule {
            self.paint_separator(layers, x, y - self.ui_px(28.0), width)?;
        }

        let button_label = crate::i18n::tr("settings-restart-app");
        let button_width = self
            .button_width_for_label(&button_label, 0.0)
            .max(self.ui_px(220.0));
        let button_x = x + width - button_width;
        let text_width = (button_x - x - 24.0).max(width * 0.45);
        let value = if self.main_renderer_restart_required() {
            crate::i18n::tr("common-required")
        } else {
            crate::i18n::tr("common-not-required")
        };
        self.draw_text(
            layers,
            &ui_font,
            x,
            y,
            &crate::i18n::tr("settings-restart"),
            palette.text,
            text_width,
        )?;
        let mut status_args = FluentArgs::new();
        status_args.set("status", value);
        let extra = self.draw_row_description(
            layers,
            x,
            y,
            &crate::i18n::tr_args("settings-restart-status", &status_args),
            text_width,
        )?;
        self.draw_button(
            layers,
            button_x,
            y + self.ui_px(4.0),
            button_width,
            &button_label,
            SettingsAction::RestartApplication,
        )?;

        Ok(extra)
    }

    fn dropdown_control_geometry(&self, x: f32, y: f32, width: f32) -> (f32, f32, f32) {
        let control_width = self.settings_control_width(width);
        (
            x + width - control_width,
            y + self.ui_px(4.0),
            control_width,
        )
    }

    fn paint_open_dropdown_overlay(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
    ) -> anyhow::Result<()> {
        self.ui.ui_font_menu_rect = None;
        let Some(dropdown) = self.ui.open_dropdown else {
            return Ok(());
        };
        // Under its pill, wherever the plugin list put it.
        if let SettingsDropdown::PluginBackground(key) = dropdown {
            return match self.ui.plugin_background_pill {
                Some((shown, pill)) if shown == key => {
                    self.paint_plugin_background_menu(layers, key, pill)
                }
                _ => Ok(()),
            };
        }

        // Under its row's control, where the row said it painted it this
        // frame. A row that was not painted -- its page is not up -- has no
        // menu to show.
        let Some((control_x, control_y, control_width)) = self
            .ui
            .dropdown_anchors
            .iter()
            .find(|(noted, _)| *noted == dropdown)
            .map(|(_, geometry)| *geometry)
        else {
            return Ok(());
        };
        match dropdown {
            SettingsDropdown::Language => self.paint_language_menu(
                layers,
                control_x,
                control_y + self.ui_px(CONTROL_HEIGHT) + self.ui_px(8.0),
                control_width,
            ),
            SettingsDropdown::MainRenderer => self.paint_main_renderer_menu(
                layers,
                control_x,
                control_y + self.ui_px(CONTROL_HEIGHT) + self.ui_px(8.0),
                control_width,
            ),
            SettingsDropdown::DefaultShell => self.paint_default_shell_menu(
                layers,
                control_x,
                control_y + self.ui_px(CONTROL_HEIGHT) + self.ui_px(8.0),
                control_width,
            ),
            SettingsDropdown::UiFont => {
                self.paint_ui_font_menu(layers, control_x, control_y, control_width)
            }
            SettingsDropdown::CommandPaletteHotkey => self.paint_command_palette_hotkey_menu(
                layers,
                control_x,
                control_y + self.ui_px(CONTROL_HEIGHT) + self.ui_px(8.0),
                control_width,
            ),
            SettingsDropdown::WebLinkTtl => self.paint_web_link_ttl_menu(
                layers,
                control_x,
                control_y + self.ui_px(CONTROL_HEIGHT) + self.ui_px(8.0),
                control_width,
            ),
            // Painted under its pill, above.
            SettingsDropdown::PluginBackground(_) => Ok(()),
        }
    }

    fn paint_language_menu(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
    ) -> anyhow::Result<()> {
        let configured = crate::i18n::configured_preference(&self.native_settings);
        let options: Vec<(String, SettingsAction, bool)> = crate::i18n::LANGUAGE_OPTIONS
            .iter()
            .map(|option| {
                (
                    crate::i18n::language_option_label(*option),
                    SettingsAction::SetLanguage(option.preference),
                    configured.eq_ignore_ascii_case(option.preference),
                )
            })
            .collect();
        self.paint_dropdown_menu(layers, x, y, width, &options)
    }

    fn paint_main_renderer_menu(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
    ) -> anyhow::Result<()> {
        let current = self.current_main_renderer();
        let options = [
            (
                NativeRendererBackend::OpenGL.label().to_string(),
                SettingsAction::SetMainRenderer(NativeRendererBackend::OpenGL),
                current == NativeRendererBackend::OpenGL,
            ),
            (
                NativeRendererBackend::WebGpu.label().to_string(),
                SettingsAction::SetMainRenderer(NativeRendererBackend::WebGpu),
                current == NativeRendererBackend::WebGpu,
            ),
        ];
        self.paint_dropdown_menu(layers, x, y, width, &options)
    }

    fn paint_dropdown_menu(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
        options: &[(String, SettingsAction, bool)],
    ) -> anyhow::Result<()> {
        self.paint_dropdown_menu_on_layer(layers, 1, x, y, width, options)
    }

    /// `paint_dropdown_menu` on layer `layer_num`, all of it. A menu that
    /// may open over a row's icons has to be on the top layer, where they
    /// are: on the one below, they show through it.
    fn paint_dropdown_menu_on_layer(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        layer_num: usize,
        x: f32,
        y: f32,
        width: f32,
        options: &[(String, SettingsAction, bool)],
    ) -> anyhow::Result<()> {
        let palette = self.palette();
        let ui_font = Rc::clone(&self.ui_font);
        let row_height = self.ui_px(DROPDOWN_ROW_HEIGHT);
        let row_gap = self.ui_px(DROPDOWN_ROW_GAP);
        let menu_padding = self.ui_px(DROPDOWN_MENU_PADDING);
        let menu_height = menu_padding * 2.0
            + row_height * options.len() as f32
            + row_gap * options.len().saturating_sub(1) as f32;
        let menu_bg = match self.effective_appearance() {
            Appearance::Light | Appearance::LightHighContrast => rgba(248, 248, 250, 1.0),
            Appearance::Dark | Appearance::DarkHighContrast => rgba(34, 34, 36, 1.0),
        };

        // Claim the whole menu area before the rows do. Hit testing takes
        // the last matching rect, so the rows still win where they cover;
        // this only catches the padding and the gaps between them, which
        // would otherwise pass the click through to whatever control the
        // open menu is painted over.
        self.ui_context.push(
            rect(x, y, width, menu_height),
            WidgetKind::Button,
            SettingsAction::DropdownMenuBackdrop,
        );

        self.draw_rounded_frame(
            layers,
            layer_num,
            x,
            y,
            width,
            menu_height,
            menu_bg,
            palette.control_border,
            self.ui_px(CONTROL_RADIUS),
        )?;

        let mut row_y = y + menu_padding;
        for (label, action, selected) in options {
            let action = *action;
            let selected = *selected;
            let row_rect = rect(
                x + self.ui_px(8.0),
                row_y,
                width - self.ui_px(16.0),
                row_height,
            );
            self.ui_context.push(row_rect, WidgetKind::Button, action);
            let hovered = self.ui.interaction.hovered == Some(action);
            let pressed = self.ui.interaction.pressed == Some(action);
            let row_bg = if selected {
                Some(palette.nav_selected_bg)
            } else if pressed {
                Some(palette.control_pressed_bg)
            } else if hovered {
                Some(palette.control_hover_bg)
            } else {
                None
            };
            if let Some(row_bg) = row_bg {
                self.draw_rounded_rect(
                    layers,
                    layer_num,
                    row_rect.origin.x,
                    row_rect.origin.y,
                    row_rect.size.width,
                    row_rect.size.height,
                    row_bg,
                    self.ui_px(9.0),
                )?;
            }
            self.draw_text_on_layer(
                layers,
                layer_num,
                &ui_font,
                row_rect.origin.x + self.ui_px(14.0),
                self.control_text_y(row_rect.origin.y, row_height),
                label,
                if selected {
                    palette.selected_text
                } else {
                    palette.text
                },
                row_rect.size.width - self.ui_px(28.0),
            )?;
            row_y += row_height + row_gap;
        }

        Ok(())
    }

    /// The interface font menu: the system's font and then every family,
    /// which is far more rows than a menu can show. It shows
    /// `UI_FONT_MENU_ROWS` of them and the wheel moves it a whole row at a
    /// time, so no row is ever cut by the menu's edge. It hangs under its
    /// control at `control_y`, or over it where the window ends too soon,
    /// and is wider than the control where the page has the room: a
    /// family's name is longer than the other menus' options are.
    fn paint_ui_font_menu(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        control_x: f32,
        control_y: f32,
        control_width: f32,
    ) -> anyhow::Result<()> {
        let palette = self.palette();
        let ui_font = Rc::clone(&self.ui_font);
        let right = control_x + control_width;
        let page_left = self.ui.sidebar.width + self.ui_px(24.0);
        let width = self
            .ui_px(UI_FONT_MENU_WIDTH)
            .min(right - page_left)
            .max(control_width);
        let x = right - width;
        let (total, visible, step) = self.ui_font_menu_rows();
        let menu_padding = self.ui_px(DROPDOWN_MENU_PADDING);
        let menu_height = menu_padding * 2.0 + step * visible as f32 - self.ui_px(DROPDOWN_ROW_GAP);
        let gap = self.ui_px(8.0);
        let below = control_y + self.ui_px(CONTROL_HEIGHT) + gap;
        let above = control_y - gap - menu_height;
        let y = if below + menu_height + gap > self.content_bottom()
            && above >= self.content_scroll_area_top()
        {
            above
        } else {
            below
        };

        self.ui
            .ui_font_menu_scroll
            .set_extents(visible as f32 * step, total as f32 * step);
        let first =
            ((self.ui.ui_font_menu_scroll.offset / step).round() as usize).min(total - visible);
        let chosen = self.chosen_ui_font_family().map(str::to_string);
        let row_width = width - self.ui_px(16.0) - self.ui_px(28.0);
        let options: Vec<(String, SettingsAction, bool)> = (first..first + visible)
            .map(|row| match row.checked_sub(1) {
                None => (
                    crate::i18n::tr("settings-ui-font-system"),
                    SettingsAction::SetUiFont(None),
                    chosen.is_none(),
                ),
                Some(index) => {
                    let family = &self.ui.ui_font_families[index];
                    (
                        self.text_with_ellipsis(&ui_font, family, row_width),
                        SettingsAction::SetUiFont(Some(index)),
                        chosen.as_deref() == Some(family.as_str()),
                    )
                }
            })
            .collect();
        self.ui.ui_font_menu_rect = Some(rect(x, y, width, menu_height));
        // Over the rows above it, where it opens upwards, and their icons.
        self.paint_dropdown_menu_on_layer(layers, 2, x, y, width, &options)?;

        // How far through the list this is, down the menu's right edge.
        let track = menu_height - menu_padding * 2.0;
        if let Some((thumb_y, thumb_height)) =
            self.ui
                .ui_font_menu_scroll
                .thumb_with_min(y + menu_padding, track, self.ui_px(24.0))
        {
            let thumb_width = self.ui_px(4.0);
            self.draw_rounded_rect(
                layers,
                2,
                x + width - thumb_width - self.ui_px(3.0),
                thumb_y,
                thumb_width,
                thumb_height,
                palette.secondary_text.mul_alpha(0.45),
                thumb_width / 2.0,
            )?;
        }
        Ok(())
    }

    fn content_bottom(&self) -> f32 {
        self.dimensions.pixel_height as f32
    }

    fn content_scroll_area_top(&self) -> f32 {
        if self.settings_window_shows_window_buttons() {
            self.ui_px(SETTINGS_WINDOW_CHROME_HEIGHT)
        } else {
            0.0
        }
    }

    fn content_viewport_extent(&self) -> f32 {
        (self.content_bottom() - self.content_scroll_area_top()).max(0.0)
    }

    fn sidebar_title_y(&self) -> f32 {
        if self.settings_window_shows_window_buttons() {
            self.ui_px(SIDEBAR_TITLE_Y_WITH_CUSTOM_CHROME)
        } else {
            self.ui_px(SIDEBAR_TITLE_Y)
        }
    }

    fn sidebar_scrollbar_visible(&self) -> bool {
        self.ui
            .sidebar_scrollbar_visible_until
            .is_some_and(|until| until > Instant::now())
            || matches!(self.ui.drag, Some(SettingsDrag::SidebarResize { .. }))
    }

    fn content_scrollbar_visible(&self) -> bool {
        self.ui
            .content_scrollbar_visible_until
            .is_some_and(|until| until > Instant::now())
    }

    fn filtered_sections(&self) -> Vec<SettingsSection> {
        let query = self.ui.search.text().trim().to_lowercase();
        let sections = self.visible_sections();
        if query.is_empty() {
            return sections;
        }
        sections
            .into_iter()
            .filter(|section| section.matches_search(&query))
            .collect()
    }

    /// `layer_num` is the layer the whole field paints on -- frame, selection,
    /// text and caret together. The sidebar's search field passes the top
    /// layer so it sits above the mask that hides the nav list's overhang;
    /// layers composite in order, so painting after the mask is not enough.
    fn paint_text_input(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        layer_num: usize,
        spec: TextInputSpec<'_, SettingsAction>,
    ) -> anyhow::Result<()> {
        let palette = self.palette();
        self.ui_context
            .push(spec.rect, WidgetKind::TextInput, spec.action);
        // A neutral bright ring. nav_selected_bg used to sit here, but it
        // composites darker than the unfocused border, so focus read as no
        // change at all -- it was the 2px bar above the field, since removed,
        // that was actually doing the work.
        let border = if spec.focused {
            palette.muted_text
        } else if self.ui.interaction.hovered == Some(spec.action) {
            palette.separator
        } else {
            palette.search_border
        };
        // The search fields are pills; the value inputs keep the control
        // radius, so the one you type a query into is shaped unlike the ones
        // you type a setting into.
        let radius = if matches!(
            spec.action,
            SettingsAction::SearchInput | SettingsAction::TabIconSearchInput
        ) {
            spec.rect.size.height / 2.0
        } else {
            self.ui.tokens.control_radius
        };
        self.draw_rounded_frame(
            layers,
            layer_num,
            spec.rect.origin.x,
            spec.rect.origin.y,
            spec.rect.size.width,
            spec.rect.size.height,
            palette.search_bg,
            border,
            radius,
        )?;
        let text = if spec.text.is_empty() && !spec.focused {
            spec.placeholder
        } else {
            spec.text
        };
        let color = if spec.text.is_empty() && !spec.focused {
            palette.muted_text
        } else {
            palette.text
        };
        let font = Rc::clone(&self.ui_font);
        let text_left = spec.rect.origin.x + self.ui_px(58.0);
        let text_area = (spec.rect.size.width - self.ui_px(116.0)).max(0.0);
        let selection = self
            .caret_for_input(spec.action)
            .and_then(|caret| caret.selection)
            .filter(|(start, end)| start != end)
            .or_else(|| {
                (spec.selected_all && !spec.text.is_empty()).then(|| (0, spec.text.chars().count()))
            });
        if spec.focused && !spec.text.is_empty() {
            if let Some((start, end)) = selection {
                let start_x = self
                    .text_width_to_char(&font, spec.text, start)
                    .min(text_area);
                let end_x = self
                    .text_width_to_char(&font, spec.text, end)
                    .min(text_area);
                self.draw_rounded_rect(
                    layers,
                    layer_num,
                    text_left + start_x - self.ui_px(4.0),
                    spec.rect.origin.y + self.ui_px(6.0),
                    (end_x - start_x) + self.ui_px(8.0),
                    spec.rect.size.height - self.ui_px(12.0),
                    palette.nav_selected_bg.mul_alpha(0.56),
                    self.ui.tokens.control_radius - self.ui_px(4.0),
                )?;
            }
        }
        self.draw_text_on_layer(
            layers,
            layer_num,
            &font,
            text_left,
            self.control_text_y(spec.rect.origin.y, spec.rect.size.height),
            text,
            color,
            text_area,
        )?;
        if spec.focused && selection.is_none() {
            let caret_dx = match self.caret_for_input(spec.action) {
                Some(caret) => self.text_width_to_char(&font, spec.text, caret.cursor),
                None => self.measure_text_width(&font, spec.text),
            };
            let caret_width = self.ui_px(3.0).max(1.0);
            self.draw_rect(
                layers,
                layer_num,
                text_left + caret_dx.min(text_area) - caret_width / 3.0,
                spec.rect.origin.y + self.ui_px(8.0),
                caret_width,
                spec.rect.size.height - self.ui_px(16.0),
                palette.nav_selected_bg,
            )?;
        }
        Ok(())
    }

    fn paint_scrollbar(
        &self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        area: window::RectF,
        scroll: ScrollState,
        visible: bool,
    ) -> anyhow::Result<()> {
        if !visible {
            return Ok(());
        }
        let ui_palette = self.chrome_palette;
        let spec = ScrollbarSpec::from_area(area, self.ui.tokens);
        if let Some((thumb_y, thumb_h)) = scroll.thumb(spec.y, spec.height) {
            self.draw_rounded_rect(
                layers,
                0,
                spec.x,
                thumb_y,
                spec.width,
                thumb_h,
                ui_palette.scrollbar_thumb,
                spec.width / 2.0,
            )?;
        }
        Ok(())
    }

    fn draw_button(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
        label: &str,
        action: SettingsAction,
    ) -> anyhow::Result<()> {
        self.draw_button_enabled(layers, x, y, width, label, action, true)
    }

    /// `draw_button`, or with `enabled` false the same button greyed out and
    /// taking no clicks: for an action with nothing to act on, which would
    /// otherwise answer only with an error.
    #[allow(clippy::too_many_arguments)]
    fn draw_button_enabled(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
        label: &str,
        action: SettingsAction,
        enabled: bool,
    ) -> anyhow::Result<()> {
        // The width given is the width drawn. This used to be
        // `width.max(button_width_for_label(..))` -- a button whose label did
        // not fit grew to the right and out of whatever was holding it, which
        // is exactly what a caller passing a *capped* width is trying to
        // prevent. The label is ellipsised to fit below instead. Every other
        // caller already passes a width sized to its own label, so nothing
        // else moves.
        let button = ButtonSpec {
            label,
            action,
            rect: rect(x, y, width, self.ui_px(CONTROL_HEIGHT)),
            state: if !enabled {
                ControlState::Disabled
            } else if self.ui.interaction.pressed == Some(action) {
                ControlState::Pressed
            } else if self.ui.interaction.hovered == Some(action) {
                ControlState::Hovered
            } else {
                ControlState::Normal
            },
            kind: WidgetKind::Button,
            // This window paints its own frame below and only reads the
            // state's colours, so the variant is inert here.
            variant: ButtonVariant::Secondary,
        };
        if enabled {
            self.ui_context
                .push(button.rect, button.kind, button.action);
        }

        let palette = self.palette();
        let ui_palette = self.chrome_palette;
        let (background, border) = button.state.colors(ui_palette);
        // Fully rounded rather than CONTROL_RADIUS: a pill reads as a
        // button, which is what distinguishes it from the value pills and
        // text fields that share this height and use the smaller radius.
        self.draw_rounded_frame(
            layers,
            0,
            x,
            y,
            width,
            self.ui_px(CONTROL_HEIGHT),
            background,
            border,
            self.ui_px(CONTROL_HEIGHT) / 2.0,
        )?;
        // Center the label in the frame. A fixed left inset left every
        // button's text biased toward the left edge -- button_width_for_label
        // adds 44px of padding, so a 14px inset left 30px on the right --
        // which is most visible on short labels in a wide frame.
        let font = Rc::clone(&self.ui_font);
        let label_width = self.measure_text_width(&font, button.label);
        let text_x = x + ((width - label_width) / 2.0).max(self.ui_px(12.0));
        self.draw_text(
            layers,
            &font,
            text_x,
            self.control_text_y(y, self.ui_px(CONTROL_HEIGHT)),
            button.label,
            if enabled {
                palette.text
            } else {
                palette.muted_text
            },
            (x + width - self.ui_px(12.0) - text_x).max(0.0),
        )?;
        Ok(())
    }

    fn paint_icon_button(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        size: f32,
        icon: SvgIcon,
        action: SettingsAction,
    ) -> anyhow::Result<()> {
        let palette = self.palette();
        let rect = rect(x, y, size, size);
        self.ui_context.push(rect, WidgetKind::Button, action);
        let hovered = self.ui.interaction.hovered == Some(action);
        let pressed = self.ui.interaction.pressed == Some(action);
        let bg = if pressed {
            palette.control_pressed_bg
        } else if hovered {
            palette.control_hover_bg
        } else {
            LinearRgba::TRANSPARENT
        };
        if bg.3 > 0.0 {
            self.draw_rounded_rect(layers, 0, x, y, size, size, bg, self.ui_px(CONTROL_RADIUS))?;
        }
        let icon_size =
            (self.metrics.cell_size.height as f32 + 4.0).clamp(self.ui_px(24.0), self.ui_px(34.0));
        self.draw_svg_icon(
            layers,
            icon,
            x + (size - icon_size) / 2.0,
            y + (size - icon_size) / 2.0,
            icon_size,
            if hovered || pressed {
                palette.text
            } else {
                palette.muted_text
            },
        )
    }

    fn button_width_for_label(&self, label: &str, min_width: f32) -> f32 {
        // min_width is in design pixels, like the layout constants
        (self.measure_text_width(&Rc::clone(&self.ui_font), label) + self.ui_px(44.0))
            .max(self.ui_px(min_width))
    }

    fn control_text_y(&self, y: f32, height: f32) -> f32 {
        let cell_height = self.metrics.cell_size.height as f32;
        y + ((height - cell_height) / 2.0).max(0.0)
    }

    fn draw_rounded_frame(
        &self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        layer_num: usize,
        x: f32,
        y: f32,
        width: f32,
        height: f32,
        fill: LinearRgba,
        border: LinearRgba,
        radius: f32,
    ) -> anyhow::Result<()> {
        crate::ui::draw::draw_rounded_frame(
            self, layers, layer_num, x, y, width, height, fill, border, radius,
        )
    }

    fn draw_rounded_rect(
        &self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        layer_num: usize,
        x: f32,
        y: f32,
        width: f32,
        height: f32,
        color: LinearRgba,
        radius: f32,
    ) -> anyhow::Result<()> {
        let Some(rect) = crate::ui::draw::pixel_snap_rounded_rect(x, y, width, height, radius)
        else {
            return Ok(());
        };
        let crate::ui::draw::PixelSnappedRoundedRect {
            x,
            y,
            width,
            height,
            radius,
        } = rect;

        if radius <= 0.0 {
            return self.draw_rect(layers, layer_num, x, y, width, height, color);
        }

        let corner_size = euclid::size2(radius, radius);
        self.draw_corner(
            layers,
            layer_num,
            x,
            y,
            TOP_LEFT_ROUNDED_CORNER,
            corner_size,
            color,
        )?;
        self.draw_corner(
            layers,
            layer_num,
            x + width - radius,
            y,
            TOP_RIGHT_ROUNDED_CORNER,
            corner_size,
            color,
        )?;
        self.draw_corner(
            layers,
            layer_num,
            x,
            y + height - radius,
            BOTTOM_LEFT_ROUNDED_CORNER,
            corner_size,
            color,
        )?;
        self.draw_corner(
            layers,
            layer_num,
            x + width - radius,
            y + height - radius,
            BOTTOM_RIGHT_ROUNDED_CORNER,
            corner_size,
            color,
        )?;

        self.draw_rect(
            layers,
            layer_num,
            x + radius,
            y,
            width - radius * 2.0,
            height,
            color,
        )?;
        self.draw_rect(
            layers,
            layer_num,
            x,
            y + radius,
            radius,
            height - radius * 2.0,
            color,
        )?;
        self.draw_rect(
            layers,
            layer_num,
            x + width - radius,
            y + radius,
            radius,
            height - radius * 2.0,
            color,
        )?;

        Ok(())
    }

    fn draw_corner(
        &self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        layer_num: usize,
        x: f32,
        y: f32,
        polys: &'static [Poly],
        size: euclid::Size2D<f32, window::PixelUnit>,
        color: LinearRgba,
    ) -> anyhow::Result<()> {
        let render_state = self.render_state.as_ref().unwrap();
        let sprite = render_state
            .glyph_cache
            .borrow_mut()
            .cached_block(
                BlockKey::PolyWithCustomMetrics {
                    polys,
                    underline_height: self.metrics.underline_height,
                    cell_size: euclid::size2(size.width as isize, size.height as isize),
                },
                &self.metrics,
            )?
            .texture_coords();

        let mut quad = layers.allocate(layer_num)?;
        let left_offset = self.dimensions.pixel_width as f32 / 2.0;
        let top_offset = self.dimensions.pixel_height as f32 / 2.0;
        quad.set_position(
            x - left_offset,
            y - top_offset,
            x + size.width - left_offset,
            y + size.height - top_offset,
        );
        quad.set_texture(sprite);
        quad.set_fg_color(color);
        quad.set_alt_color_and_mix_value(color, 0.0);
        quad.set_hsv(None);
        quad.set_has_color(false);
        quad.set_grayscale();
        Ok(())
    }

    fn draw_rect(
        &self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        layer_num: usize,
        x: f32,
        y: f32,
        width: f32,
        height: f32,
        color: LinearRgba,
    ) -> anyhow::Result<()> {
        if width <= 0.0 || height <= 0.0 {
            return Ok(());
        }

        let render_state = self.render_state.as_ref().unwrap();
        let mut quad = layers.allocate(layer_num)?;
        let left_offset = self.dimensions.pixel_width as f32 / 2.0;
        let top_offset = self.dimensions.pixel_height as f32 / 2.0;
        quad.set_position(
            x - left_offset,
            y - top_offset,
            x + width - left_offset,
            y + height - top_offset,
        );
        quad.set_texture(render_state.util_sprites.filled_box.texture_coords());
        quad.set_is_background();
        quad.set_fg_color(color);
        quad.set_hsv(None);
        Ok(())
    }

    fn draw_svg_icon(
        &self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        icon: SvgIcon,
        x: f32,
        y: f32,
        size: f32,
        color: LinearRgba,
    ) -> anyhow::Result<()> {
        if size <= 0.0 {
            return Ok(());
        }

        let render_state = self.render_state.as_ref().unwrap();
        let sprite = render_state
            .glyph_cache
            .borrow_mut()
            .cached_svg_icon(icon, size.round() as usize)?
            .texture_coords();
        let mut quad = layers.allocate(2)?;
        let left_offset = self.dimensions.pixel_width as f32 / 2.0;
        let top_offset = self.dimensions.pixel_height as f32 / 2.0;
        quad.set_position(
            x - left_offset,
            y - top_offset,
            x + size - left_offset,
            y + size - top_offset,
        );
        quad.set_texture(sprite);
        quad.set_fg_color(color);
        quad.set_alt_color_and_mix_value(color, 0.0);
        quad.set_hsv(None);
        quad.set_has_color(false);
        quad.set_grayscale();
        Ok(())
    }

    /// A brand mark in its own colors. Same geometry as
    /// [`Self::draw_svg_icon`], but the sprite is sampled in full color
    /// instead of being treated as an alpha mask to tint.
    fn draw_brand_icon(
        &self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        icon: BrandIcon,
        x: f32,
        y: f32,
        size: f32,
    ) -> anyhow::Result<()> {
        if size <= 0.0 {
            return Ok(());
        }

        let render_state = self.render_state.as_ref().unwrap();
        let sprite = render_state
            .glyph_cache
            .borrow_mut()
            .cached_brand_icon(icon, size.round() as usize)?
            .texture_coords();
        let mut quad = layers.allocate(2)?;
        let left_offset = self.dimensions.pixel_width as f32 / 2.0;
        let top_offset = self.dimensions.pixel_height as f32 / 2.0;
        quad.set_position(
            x - left_offset,
            y - top_offset,
            x + size - left_offset,
            y + size - top_offset,
        );
        quad.set_texture(sprite);
        let white = LinearRgba::with_components(1.0, 1.0, 1.0, 1.0);
        quad.set_fg_color(white);
        quad.set_alt_color_and_mix_value(white, 0.0);
        quad.set_hsv(None);
        quad.set_has_color(true);
        Ok(())
    }

    fn draw_text(
        &self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        font: &Rc<LoadedFont>,
        x: f32,
        y: f32,
        text: &str,
        color: LinearRgba,
        max_width: f32,
    ) -> anyhow::Result<()> {
        self.draw_text_on_layer(layers, 1, font, x, y, text, color, max_width)
    }

    /// Text on a caller-chosen layer. Everything uses the glyph layer except
    /// the sidebar's search field, which has to paint above the mask that
    /// hides the nav list's overhang -- and layers composite in order, so
    /// "after" is not enough there.
    #[allow(clippy::too_many_arguments)]
    fn draw_text_on_layer(
        &self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        layer_num: usize,
        font: &Rc<LoadedFont>,
        x: f32,
        y: f32,
        text: &str,
        color: LinearRgba,
        max_width: f32,
    ) -> anyhow::Result<()> {
        if text.is_empty() || max_width <= 0.0 {
            return Ok(());
        }

        let display_text = self.text_with_ellipsis(font, text, max_width);
        if display_text.is_empty() {
            return Ok(());
        }

        let Some(shaped) = self.shaped_text(font, &display_text) else {
            return Ok(());
        };
        let mut pos_x = x;
        let baseline = self.metrics.cell_size.height as f32 + self.metrics.descender.get() as f32;
        let left_offset = self.dimensions.pixel_width as f32 / 2.0;
        let top_offset = self.dimensions.pixel_height as f32 / 2.0;
        let right_edge = x + max_width;

        for glyph in &shaped.glyphs {
            let advance = glyph.x_advance.get() as f32;
            if !crate::ui::draw::glyph_fits(pos_x, advance, right_edge) {
                break;
            }
            if let Some(texture) = glyph.texture.as_ref() {
                let glyph_x = (pos_x + (glyph.x_offset + glyph.bearing_x).get() as f32).round();
                let glyph_y =
                    (y - (glyph.y_offset + glyph.bearing_y).get() as f32 + baseline).round();
                let width = texture.coords.size.width as f32 * glyph.scale as f32;
                let height = texture.coords.size.height as f32 * glyph.scale as f32;

                let mut quad = layers.allocate(layer_num)?;
                quad.set_position(
                    glyph_x - left_offset,
                    glyph_y - top_offset,
                    glyph_x + width - left_offset,
                    glyph_y + height - top_offset,
                );
                quad.set_texture(texture.texture_coords());
                quad.set_has_color(glyph.has_color);
                quad.set_fg_color(color);
                quad.set_hsv(None);
            }
            pos_x += advance;
        }

        Ok(())
    }

    /// Shape and glyph-resolve `text`, reusing the previous frame's work.
    fn shaped_text(&self, font: &Rc<LoadedFont>, text: &str) -> Option<Rc<ShapedText>> {
        if text.is_empty() {
            return None;
        }
        let key = ShapedTextKey {
            font_id: font.id(),
            text: text.to_string(),
        };
        if let Some(shaped) = self.shape_cache.borrow().get(&key) {
            return Some(Rc::clone(shaped));
        }

        let infos = font
            .blocking_shape(text, None, Direction::LeftToRight, None, None)
            .ok()?;
        let render_state = self.render_state.as_ref()?;
        let mut glyph_cache = render_state.glyph_cache.borrow_mut();
        let style = font.style();
        let glyphs = infos
            .into_iter()
            .map(|info| glyph_cache.cached_glyph(&info, style, false, font, &self.metrics, 1))
            .collect::<anyhow::Result<Vec<Rc<CachedGlyph>>>>();
        drop(glyph_cache);
        let glyphs = match glyphs {
            Ok(glyphs) => glyphs,
            Err(err) => {
                let mut pending = self.pending_glyph_error.borrow_mut();
                if pending.is_none() {
                    *pending = Some(err);
                }
                return None;
            }
        };

        let shaped = Rc::new(ShapedText {
            width: glyphs
                .iter()
                .map(|glyph| glyph.x_advance.get() as f32)
                .sum(),
            glyphs,
        });

        let mut cache = self.shape_cache.borrow_mut();
        // Dropping the whole map rather than one entry: there is no recency
        // order to evict by, and the labels it is full of are about to be
        // re-shaped on the very next frame anyway.
        if cache.len() >= SHAPED_TEXT_CACHE_LIMIT {
            cache.clear();
        }
        cache.insert(key, Rc::clone(&shaped));
        Some(shaped)
    }

    /// Drop shaped runs that a new font, DPI or glyph atlas has invalidated.
    ///
    /// The atlas cases matter as much as the font ones: a cached run holds
    /// glyphs by texture coordinate, and those describe the atlas that was
    /// current when they were rasterized.
    fn invalidate_shaped_text(&self) {
        self.shape_cache.borrow_mut().clear();
        self.wrapped_text.borrow_mut().clear();
    }

    /// Greedy word wrap against the shaped width; falls back to char-level
    /// breaking for unspaced (CJK) text or overlong tokens.
    fn wrap_settings_text(&self, font: &Rc<LoadedFont>, text: &str, max_width: f32) -> Vec<String> {
        let mut lines = Vec::new();
        let mut current = String::new();
        let push_wrapped_word = |word: &str, current: &mut String, lines: &mut Vec<String>| {
            let mut piece = String::new();
            for ch in word.chars() {
                let mut candidate = piece.clone();
                candidate.push(ch);
                if !piece.is_empty() && self.measure_text_width(font, &candidate) > max_width {
                    lines.push(std::mem::take(&mut piece));
                    piece.push(ch);
                } else {
                    piece = candidate;
                }
            }
            *current = piece;
        };
        for word in text.split_whitespace() {
            let candidate = if current.is_empty() {
                word.to_string()
            } else {
                format!("{current} {word}")
            };
            if self.measure_text_width(font, &candidate) <= max_width {
                current = candidate;
            } else if current.is_empty() {
                push_wrapped_word(word, &mut current, &mut lines);
            } else {
                lines.push(std::mem::take(&mut current));
                if self.measure_text_width(font, word) <= max_width {
                    current = word.to_string();
                } else {
                    push_wrapped_word(word, &mut current, &mut lines);
                }
            }
        }
        if !current.is_empty() {
            lines.push(current);
        }
        lines
    }

    fn measure_text_width(&self, font: &Rc<LoadedFont>, text: &str) -> f32 {
        self.shaped_text(font, text)
            .map(|shaped| shaped.width)
            .unwrap_or(0.0)
    }

    fn text_with_ellipsis(&self, font: &Rc<LoadedFont>, text: &str, max_width: f32) -> String {
        if max_width <= 0.0 || text.is_empty() {
            return String::new();
        }

        if self.measure_text_width(font, text) <= max_width {
            return text.to_string();
        }

        let ellipsis = "...";
        let ellipsis_width = self.measure_text_width(font, ellipsis);
        if ellipsis_width > max_width {
            return String::new();
        }

        // Walk the run already shaped above rather than binary-searching by
        // re-measuring prefixes. The old search shaped log2(len) extra strings
        // per label per frame and left every one of those prefixes behind,
        // which would now be junk filling the cache as well as time spent.
        let Some(shaped) = self.shaped_text(font, text) else {
            return String::new();
        };
        let budget = max_width - ellipsis_width;
        let mut width = 0.0;
        let mut chars = text.char_indices();
        let mut end = 0;
        // One glyph per character is the common case and the assumption the
        // painter already makes; zip stops at whichever runs out first, so a
        // string that shapes to fewer glyphs truncates early rather than
        // indexing past the end.
        for (glyph, (idx, _)) in shaped.glyphs.iter().zip(&mut chars) {
            let advance = glyph.x_advance.get() as f32;
            if width + advance > budget {
                break;
            }
            width += advance;
            end = idx + text[idx..].chars().next().map_or(0, char::len_utf8);
        }

        // Checked whole, as it is drawn -- a string `draw_text` shapes anyway,
        // so the check leaves nothing extra in the cache unless it steps back.
        crate::ui::draw::fit_ellipsized(text, end, max_width, |candidate| {
            self.measure_text_width(font, candidate)
        })
    }

    fn native_settings_path() -> PathBuf {
        crate::native_settings::settings_path()
    }

    fn load_native_settings() -> ThinkTermNativeSettings {
        crate::native_settings::reload_from_disk()
    }

    fn config_source_summary() -> String {
        if let Some(path) = config::configuration_file() {
            let mut args = FluentArgs::new();
            args.set("path", home_relative(&path));
            crate::i18n::tr_args("settings-config-source-file", &args)
        } else if config::is_config_overridden() {
            crate::i18n::tr("settings-config-source-overridden")
        } else {
            crate::i18n::tr("settings-config-source-default")
        }
    }

    fn thinkterm_compatible_config_path() -> PathBuf {
        Self::preferred_thinkterm_config_entry(&config::HOME_DIR.join(".config").join("thinkterm"))
    }

    fn preferred_thinkterm_config_entry(config_dir: &Path) -> PathBuf {
        let thinkterm = config_dir.join("thinkterm.lua");
        let legacy = config_dir.join("wezterm.lua");
        if thinkterm.exists() || !legacy.exists() {
            thinkterm
        } else {
            legacy
        }
    }

    fn wezterm_config_candidates() -> Vec<PathBuf> {
        let mut paths = vec![config::HOME_DIR.join(".wezterm.lua")];

        let legacy_user_config_dir = std::env::var_os("XDG_CONFIG_HOME")
            .map(|dir| PathBuf::from(dir).join("wezterm"))
            .unwrap_or_else(|| config::HOME_DIR.join(".config").join("wezterm"));
        paths.push(legacy_user_config_dir.join("wezterm.lua"));

        #[cfg(unix)]
        if let Some(dirs) = std::env::var_os("XDG_CONFIG_DIRS") {
            for base in std::env::split_paths(&dirs) {
                paths.push(base.join("wezterm").join("wezterm.lua"));
            }
        }
        paths
    }

    fn first_wezterm_config_path() -> Option<PathBuf> {
        Self::wezterm_config_candidates()
            .into_iter()
            .find(|path| path.exists())
    }

    fn thinkterm_imported_config_path() -> PathBuf {
        config::HOME_DIR
            .join(".config")
            .join("thinkterm")
            .join("imported_from_wezterm.lua")
    }

    fn selected_wezterm_source_path(&self) -> Option<PathBuf> {
        self.native_settings
            .compatibility
            .source_path
            .as_ref()
            .filter(|path| path.exists())
            .cloned()
            .or_else(Self::first_wezterm_config_path)
    }

    fn load_compatibility_source(&mut self) -> anyhow::Result<()> {
        let source = self
            .selected_wezterm_source_path()
            .ok_or_else(|| anyhow::anyhow!("no existing WezTerm config was found"))?;
        let loaded = config::load_config_file_for_import(&source)
            .with_context(|| format!("load {}", source.display()))?;
        let fields = Self::build_importable_fields(&loaded.config, &loaded.raw_keys);
        if self
            .native_settings
            .compatibility
            .selected_fields
            .is_empty()
        {
            self.native_settings.compatibility.selected_fields = fields
                .iter()
                .filter(|field| field.lua_value.is_some())
                .map(|field| field.id.as_str().to_string())
                .collect();
        }
        self.native_settings.compatibility.source_path = Some(loaded.file_name.clone());
        let _ = crate::native_settings::save(&self.native_settings);
        let field_count = fields.len();
        let warning_count = loaded.warnings.len();
        self.compatibility_import = CompatibilityImportState {
            fields,
            error: None,
        };
        self.status = settings_tr(
            if warning_count == 0 {
                "settings-status-wezterm-config-loaded"
            } else {
                "settings-status-wezterm-config-loaded-warnings"
            },
            &[
                ("path", loaded.file_name.display().to_string()),
                ("count", field_count.to_string()),
                ("warnings", warning_count.to_string()),
            ],
        );
        Ok(())
    }

    fn build_importable_fields(
        config: &config::Config,
        raw_keys: &std::collections::BTreeSet<String>,
    ) -> Vec<ImportableField> {
        ImportFieldId::all()
            .iter()
            .filter_map(|field_id| {
                if !raw_keys.contains(field_id.as_str()) {
                    return None;
                }
                let value = Self::import_field_value(config, *field_id)?;
                let lua_value = Self::lua_literal(&value);
                Some(ImportableField {
                    id: *field_id,
                    category: field_id.category(),
                    label: field_id.label(),
                    description: field_id.description(config),
                    preview: field_id.preview(config, &value),
                    lua_value,
                })
            })
            .collect()
    }

    fn import_field_value(config: &config::Config, field_id: ImportFieldId) -> Option<Value> {
        match field_id {
            ImportFieldId::ColorScheme => Some(config.color_scheme.to_dynamic()),
            ImportFieldId::WindowBackgroundOpacity => {
                Some(config.window_background_opacity.to_dynamic())
            }
            ImportFieldId::MacosWindowBackgroundBlur => {
                Some(config.macos_window_background_blur.to_dynamic())
            }
            ImportFieldId::InactivePaneHsb => config.inactive_pane_hsb.map(|hsb| hsb.to_dynamic()),
            ImportFieldId::FontSize => Some(config.font_size.to_dynamic()),
            ImportFieldId::Font => Some(config.font.to_dynamic()),
            ImportFieldId::LineHeight => Some(config.line_height.to_dynamic()),
            ImportFieldId::CellWidth => Some(config.cell_width.to_dynamic()),
            ImportFieldId::DefaultProg => Some(config.default_prog.to_dynamic()),
            ImportFieldId::DefaultCwd => Some(config.default_cwd.to_dynamic()),
            ImportFieldId::FrontEnd => Some(config.front_end.to_dynamic()),
            ImportFieldId::WindowDecorations => Some(config.window_decorations.to_dynamic()),
            ImportFieldId::DisableDefaultKeyBindings => {
                Some(config.disable_default_key_bindings.to_dynamic())
            }
            ImportFieldId::Keys => Some(config.keys.to_dynamic()),
            ImportFieldId::KeyTables => Some(config.key_tables.to_dynamic()),
        }
    }

    fn import_field_selected(&self, field_id: ImportFieldId) -> bool {
        self.native_settings
            .compatibility
            .selected_fields
            .iter()
            .any(|field| field == field_id.as_str())
    }

    fn toggle_import_field(&mut self, field_id: ImportFieldId) {
        let key = field_id.as_str();
        if let Some(pos) = self
            .native_settings
            .compatibility
            .selected_fields
            .iter()
            .position(|field| field == key)
        {
            self.native_settings
                .compatibility
                .selected_fields
                .remove(pos);
            self.status = settings_tr(
                "settings-status-import-field-disabled",
                &[("field", field_id.label())],
            );
        } else {
            self.native_settings
                .compatibility
                .selected_fields
                .push(key.to_string());
            self.status = settings_tr(
                "settings-status-import-field-enabled",
                &[("field", field_id.label())],
            );
        }

        if let Err(err) = crate::native_settings::save(&self.native_settings) {
            self.status = settings_tr(
                "settings-status-import-selection-error",
                &[("error", format!("{err:#}"))],
            );
        }
    }

    fn select_all_import_fields(&mut self) {
        let selected = self
            .compatibility_import
            .fields
            .iter()
            .filter(|field| field.lua_value.is_some())
            .map(|field| field.id.as_str().to_string())
            .collect::<Vec<_>>();
        let count = selected.len();
        self.native_settings.compatibility.selected_fields = selected;
        match crate::native_settings::save(&self.native_settings) {
            Ok(()) => {
                self.status = settings_tr(
                    "settings-status-import-all-selected",
                    &[("count", count.to_string())],
                );
            }
            Err(err) => {
                self.status = settings_tr(
                    "settings-status-import-selection-error",
                    &[("error", format!("{err:#}"))],
                );
            }
        }
    }

    fn clear_import_fields(&mut self) {
        self.native_settings.compatibility.selected_fields.clear();
        match crate::native_settings::save(&self.native_settings) {
            Ok(()) => {
                self.status = crate::i18n::tr("settings-status-import-all-cleared");
            }
            Err(err) => {
                self.status = settings_tr(
                    "settings-status-import-selection-error",
                    &[("error", format!("{err:#}"))],
                );
            }
        }
    }

    fn import_selected_compatibility_fields(&mut self) -> anyhow::Result<(usize, bool)> {
        if self.compatibility_import.fields.is_empty() {
            self.load_compatibility_source()?;
        }

        let selected = self
            .compatibility_import
            .fields
            .iter()
            .filter(|field| self.import_field_selected(field.id))
            .filter_map(|field| field.lua_value.as_ref().map(|value| (field, value)))
            .collect::<Vec<_>>();

        if selected.is_empty() {
            anyhow::bail!("no importable fields are selected");
        }

        let target = Self::thinkterm_imported_config_path();
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
        }

        let mut body = String::new();
        body.push_str("-- Generated by ThinkTerm Settings. Do not edit by hand.\n");
        body.push_str("-- Re-run Compatibility import to refresh these values.\n\n");
        body.push_str("return {\n");
        for (field, value) in &selected {
            body.push_str("  ");
            body.push_str(field.id.as_str());
            body.push_str(" = ");
            body.push_str(value);
            body.push_str(",\n");
        }
        body.push_str("}\n");

        fs::write(&target, body).with_context(|| format!("write {}", target.display()))?;
        let entry_created = Self::ensure_thinkterm_config_entry()?;
        self.native_settings.compatibility.last_imported_at =
            Some(Self::current_unix_timestamp_string());
        crate::native_settings::save(&self.native_settings)
            .context("save ThinkTerm compatibility settings")?;
        Ok((selected.len(), entry_created))
    }

    fn ensure_thinkterm_config_entry() -> anyhow::Result<bool> {
        let entry = Self::thinkterm_compatible_config_path();
        if entry.exists() {
            return Ok(false);
        }
        if let Some(parent) = entry.parent() {
            fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
        }

        let body = r#"local wezterm = require "wezterm"
local config = wezterm.config_builder and wezterm.config_builder() or {}

local imported = require "imported_from_wezterm"
for key, value in pairs(imported) do
  config[key] = value
end

return config
"#;
        fs::write(&entry, body).with_context(|| format!("write {}", entry.display()))?;
        Ok(true)
    }

    fn current_unix_timestamp_string() -> String {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_secs().to_string())
            .unwrap_or_else(|_| "0".to_string())
    }

    fn lua_literal(value: &Value) -> Option<String> {
        Some(match value {
            Value::Null => return None,
            Value::Bool(value) => value.to_string(),
            Value::String(value) => Self::lua_string(value),
            Value::U64(value) => value.to_string(),
            Value::I64(value) => value.to_string(),
            Value::F64(value) => value.to_string(),
            Value::Array(array) => {
                let mut parts = Vec::with_capacity(array.len());
                for value in array.iter() {
                    parts.push(Self::lua_literal(value)?);
                }
                format!("{{ {} }}", parts.join(", "))
            }
            Value::Object(object) => {
                let mut parts = Vec::with_capacity(object.len());
                for (key, value) in object.iter() {
                    let key = match key {
                        Value::String(key)
                            if key.chars().all(|c| c == '_' || c.is_ascii_alphanumeric())
                                && key
                                    .chars()
                                    .next()
                                    .map(|c| c == '_' || c.is_ascii_alphabetic())
                                    .unwrap_or(false) =>
                        {
                            key.to_string()
                        }
                        _ => format!("[{}]", Self::lua_literal(key)?),
                    };
                    parts.push(format!("{key} = {}", Self::lua_literal(value)?));
                }
                format!("{{ {} }}", parts.join(", "))
            }
        })
    }

    fn lua_string(value: &str) -> String {
        let mut out = String::with_capacity(value.len() + 2);
        out.push('"');
        for ch in value.chars() {
            match ch {
                '\\' => out.push_str("\\\\"),
                '"' => out.push_str("\\\""),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                _ => out.push(ch),
            }
        }
        out.push('"');
        out
    }

    /// What the terminal is actually painted in.
    ///
    /// The picked scheme first: a scheme chosen here or in the command palette
    /// lives in the native settings and reaches a window as that window's own
    /// override, so it is never in the configuration handle. Reading only the
    /// handle showed the file's scheme while the terminal in front of the user
    /// showed the one they picked.
    fn effective_color_scheme_label(&self, config: &config::ConfigHandle) -> String {
        if let Some(name) = self.native_settings.appearance.color_scheme.as_deref() {
            return name.to_string();
        }
        if let Some(name) = config.color_scheme.as_deref() {
            return name.to_string();
        }
        if config.colors.is_some() {
            return crate::i18n::tr("settings-color-scheme-inline");
        }
        crate::i18n::tr("settings-color-scheme-default")
    }

    fn effective_font_family(config: &config::ConfigHandle) -> String {
        config
            .font
            .font
            .first()
            .map(|font| font.family.clone())
            .unwrap_or_else(|| "Default font".to_string())
    }

    fn initial_status() -> String {
        Self::config_source_summary()
    }

    fn open_path(path: PathBuf) {
        match url::Url::from_file_path(&path) {
            Ok(url) => wezterm_open_url::open_url(url.as_str()),
            Err(_) => log::error!("Unable to convert {} into a file URL", path.display()),
        }
    }

    fn restart_application() -> anyhow::Result<()> {
        let exe = std::env::current_exe().context("resolve current executable")?;
        // Once an update has replaced the binary, Linux names the running
        // one "<path> (deleted)"; the new one is at the path itself.
        #[cfg(target_os = "linux")]
        let exe = match exe.to_str().and_then(|path| path.strip_suffix(" (deleted)")) {
            Some(path) => PathBuf::from(path),
            None => exe,
        };
        let args = std::env::args_os().skip(1).collect::<Vec<_>>();
        let mut command = Command::new(exe);
        command.args(args);
        // The replacement starts while this process is still on its way
        // out; told so, it publishes itself instead of handing this one the
        // window to open, which would lose both.
        command.env(crate::RESTARTED_ENV, "1");
        if let Ok(cwd) = std::env::current_dir() {
            command.current_dir(cwd);
        }
        command.spawn().context("spawn replacement ThinkTerm")?;
        if let Some(conn) = Connection::get() {
            conn.terminate_message_loop();
        }
        Ok(())
    }
}

fn rgba(r: u8, g: u8, b: u8, a: f32) -> LinearRgba {
    let mut color = LinearRgba::with_srgba(r, g, b, 255);
    color.3 = a;
    color
}

fn wgpu_color(color: LinearRgba) -> wgpu::Color {
    wgpu::Color {
        r: color.0 as f64,
        g: color.1 as f64,
        b: color.2 as f64,
        a: color.3 as f64,
    }
}

impl crate::ui::draw::RoundedFramePainter for SettingsWindow {
    fn frame_rounded_rect(
        &self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        layer_num: usize,
        x: f32,
        y: f32,
        width: f32,
        height: f32,
        color: LinearRgba,
        radius: f32,
    ) -> anyhow::Result<()> {
        self.draw_rounded_rect(layers, layer_num, x, y, width, height, color, radius)
    }

    fn frame_rect(
        &self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        layer_num: usize,
        x: f32,
        y: f32,
        width: f32,
        height: f32,
        color: LinearRgba,
    ) -> anyhow::Result<()> {
        self.draw_rect(layers, layer_num, x, y, width, height, color)
    }

    fn frame_corner(
        &self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        layer_num: usize,
        x: f32,
        y: f32,
        polys: &'static [Poly],
        size: euclid::Size2D<f32, window::PixelUnit>,
        color: LinearRgba,
    ) -> anyhow::Result<()> {
        self.draw_corner(layers, layer_num, x, y, polys, size, color)
    }
}
