use crate::customglyph::{BlockKey, Poly};
use crate::glyphcache::CachedGlyph;
use crate::quad::{QuadTrait, TripleLayerQuadAllocator, TripleLayerQuadAllocatorTrait};
use crate::renderstate::{RenderContext, RenderState};
use crate::termwindow::render::corners::{
    BOTTOM_LEFT_ROUNDED_CORNER, BOTTOM_RIGHT_ROUNDED_CORNER, TOP_LEFT_ROUNDED_CORNER,
    TOP_RIGHT_ROUNDED_CORNER,
};
use crate::termwindow::render::draw::draw_webgpu_layers;
use crate::termwindow::webgpu::WebGpuState;
use crate::ui::{
    rect, scale_ui_f32, scale_ui_usize, BrandIcon, ButtonSpec, ControlState, EditModifiers,
    InputCaret, InteractionState, ResizablePaneState, ScrollState, ScrollbarSpec, SettingsIcon,
    SvgIcon, TextInputSpec, TextInputState, UiContext, UiPalette, UiTokens, WidgetKind,
};
use crate::utilsprites::RenderMetrics;
use anyhow::{Context, Error};
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
use wezterm_font::{FontConfiguration, LoadedFont};
use window::bitmaps::atlas::OutOfTextureSpace;
use window::color::LinearRgba;
use window::{
    Appearance, Clipboard, Connection, ConnectionOps, Dimensions, FolderPickerOptions,
    IntegratedTitleButton, IntegratedTitleButtonStyle, KeyCode, KeyEvent, Modifiers, MouseButtons,
    MouseCursor, MouseEvent, MouseEventKind, MousePress, RequestedWindowGeometry, Window,
    WindowDecorations, WindowEvent, WindowOps, WindowState,
};

use crate::native_settings::{
    NativeAppIcon, NativeBottomQuoteMode, NativeRemotePaneResizeMode, NativeRendererBackend,
    NativeThemeMode, ThinkTermNativeSettings, DEFAULT_HOME_FONT_SIZE,
    DEFAULT_PANE_HEADER_FONT_SIZE, DEFAULT_SETTINGS_FONT_SIZE, DEFAULT_SIDEBAR_FONT_SIZE,
    DEFAULT_TAB_FONT_SIZE,
};
use fluent_bundle::FluentArgs;

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
const CONTROL_HEIGHT: f32 = 56.0;
const CONTROL_RADIUS: f32 = 14.0;
const NAV_ROW_RADIUS: f32 = 14.0;
const NAV_ROW_HEIGHT: f32 = 56.0;
const NAV_ROW_STEP: f32 = 68.0;
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
const CONTENT_TITLE_Y: f32 = 82.0;
const CONTENT_SECTION_Y: f32 = 168.0;
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

fn settings_window_for_instance(instance_id: u64) -> Option<Rc<RefCell<SettingsWindow>>> {
    SETTINGS_WINDOW.with(|slot| match &*slot.borrow() {
        SettingsWindowSlot::Open {
            instance_id: current_id,
            settings,
        } if *current_id == instance_id => Some(Rc::clone(settings)),
        SettingsWindowSlot::Closed | SettingsWindowSlot::Opening(_) => None,
        SettingsWindowSlot::Open { .. } => None,
    })
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SettingsSection {
    General,
    Appearance,
    Terminal,
    Workspaces,
    Agents,
    Archived,
    Keymap,
    Compatibility,
    Developer,
    UiKit,
    Memory,
    About,
}

const BASE_SECTIONS: &[SettingsSection] = &[
    SettingsSection::General,
    SettingsSection::Appearance,
    SettingsSection::Terminal,
    SettingsSection::Workspaces,
    SettingsSection::Agents,
    SettingsSection::Archived,
    SettingsSection::Keymap,
    SettingsSection::Compatibility,
    SettingsSection::Developer,
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

/// Debug affordance: `THINKTERM_SETTINGS_SECTION=<name>` selects that section
/// when the settings window opens (e.g. for unattended UI captures with
/// `THINKTERM_FRAME_DUMP`). Names match the enum variants, case-insensitive.
fn initial_section() -> SettingsSection {
    let Some(name) = std::env::var_os("THINKTERM_SETTINGS_SECTION") else {
        return SettingsSection::Appearance;
    };
    let name = name.to_string_lossy().to_ascii_lowercase();
    let section = match name.as_str() {
        "general" => SettingsSection::General,
        "appearance" => SettingsSection::Appearance,
        "terminal" => SettingsSection::Terminal,
        "workspaces" => SettingsSection::Workspaces,
        "agents" => SettingsSection::Agents,
        "archived" => SettingsSection::Archived,
        "keymap" => SettingsSection::Keymap,
        "compatibility" => SettingsSection::Compatibility,
        "developer" => SettingsSection::Developer,
        "about" => SettingsSection::About,
        _ => SettingsSection::Appearance,
    };
    if section == SettingsSection::Agents {
        crate::agent_status::refresh_path_probe();
    }
    section
}

impl SettingsSection {
    fn label(self) -> String {
        match self {
            Self::General => crate::i18n::tr("settings-section-general"),
            Self::Appearance => crate::i18n::tr("settings-section-appearance"),
            Self::Terminal => crate::i18n::tr("settings-section-terminal"),
            Self::Workspaces => crate::i18n::tr("settings-section-workspaces"),
            Self::Agents => crate::i18n::tr("settings-section-agents"),
            Self::Archived => crate::i18n::tr("settings-section-archived"),
            Self::Keymap => crate::i18n::tr("settings-section-keymap"),
            Self::Compatibility => crate::i18n::tr("settings-section-compatibility"),
            Self::Developer => crate::i18n::tr("settings-section-developer"),
            Self::UiKit => "UI Kit".to_string(),
            Self::Memory => "Memory".to_string(),
            Self::About => crate::i18n::tr("settings-section-about"),
        }
    }

    fn icon(self) -> SettingsIcon {
        match self {
            Self::General => SettingsIcon::General,
            Self::Appearance => SettingsIcon::Appearance,
            Self::Terminal => SettingsIcon::Terminal,
            Self::Workspaces => SettingsIcon::Workspaces,
            Self::Agents => SettingsIcon::Agents,
            Self::Archived => SettingsIcon::Archived,
            Self::Keymap => SettingsIcon::Keymap,
            Self::Compatibility => SettingsIcon::Sync,
            Self::Developer => SettingsIcon::Developer,
            Self::UiKit => SettingsIcon::UiKit,
            Self::Memory => SettingsIcon::Memory,
            Self::About => SettingsIcon::About,
        }
    }

    fn search_terms(self) -> &'static [&'static str] {
        match self {
            Self::General => &[
                "Language",
                "Config Source",
                "ThinkTerm Native Settings",
                "Theme Mode",
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
            Self::Appearance => &[
                "Theme Mode",
                "Effective Color Scheme",
                "Config Source",
                "App Icon",
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
            Self::Keymap => &["Keymap", "Keyboard", "Shortcut", "Command Palette"],
            Self::Compatibility => &[
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
            Self::About => &["About", "Version", "ThinkTerm"],
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

fn localized_theme_mode_label(mode: NativeThemeMode) -> String {
    crate::i18n::tr(match mode {
        NativeThemeMode::System => "settings-theme-system",
        NativeThemeMode::Light => "settings-theme-light",
        NativeThemeMode::Dark => "settings-theme-dark",
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

fn settings_tr(id: &'static str, values: &[(&'static str, String)]) -> String {
    let mut args = FluentArgs::new();
    for (name, value) in values {
        args.set(*name, value.clone());
    }
    crate::i18n::tr_args(id, &args)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SettingsAction {
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
    ToggleAgentPanel,
    ToggleAgentDetails(&'static str),
    /// Index into the archived rows cached at paint time.
    UnarchiveArchivedRow(usize),
    DeleteArchivedRow(usize),
    ToggleDeveloperMode,
    ToggleFallbackContextMenu,
    ShowOnboardingNow,
    ToggleMemoryMonitoring,
    RefreshMemorySnapshot,
    CopyMemorySnapshot,
    ToggleInputDiagnostics,
    ResetInputDiagnostics,
    CopyInputDiagnostics,
    ToggleThemeModeMenu,
    SetThemeMode(NativeThemeMode),
    ToggleLanguageMenu,
    SetLanguage(&'static str),
    ToggleAppIconMenu,
    SetAppIcon(NativeAppIcon),
    ToggleMainRendererMenu,
    SetMainRenderer(NativeRendererBackend),
    /// Swallows clicks that land in an open dropdown's padding or row
    /// gaps instead of letting them reach the controls underneath.
    DropdownMenuBackdrop,
    ToggleDefaultShellMenu,
    /// `None` is the platform default; `Some` indexes the shell catalog
    /// cached at paint time, exactly like the archived-row actions above.
    SetDefaultShell(Option<usize>),
    RestartApplication,
    QuitApplication,
    ToggleBottomQuote,
    CycleRemotePaneResizeMode,
    CycleBottomQuoteMode,
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
    FontFamilyInput,
    ClearSearch,
    SidebarResize,
    SidebarScrollArea,
    ContentScrollArea,
}

#[derive(Debug, Clone, Copy)]
enum SettingsDrag {
    SidebarResize { start_x: f32, start_width: f32 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SettingsDropdown {
    Language,
    ThemeMode,
    AppIcon,
    MainRenderer,
    DefaultShell,
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
            Self::InactivePaneHsb => format!(
                "h {:.2}, s {:.2}, b {:.2}",
                config.inactive_pane_hsb.hue,
                config.inactive_pane_hsb.saturation,
                config.inactive_pane_hsb.brightness
            ),
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
    vmmap_total_resident: Option<u64>,
    vmmap_graphics_resident: Option<u64>,
    vmmap_malloc_resident: Option<u64>,
    vmmap_text_resident: Option<u64>,
    vmmap_iosurface_resident: Option<u64>,
    vmmap_error: Option<String>,
    error: Option<String>,
}

impl MemorySnapshot {
    fn has_vmmap_breakdown(&self) -> bool {
        self.vmmap_total_resident.is_some()
            || self.vmmap_graphics_resident.is_some()
            || self.vmmap_malloc_resident.is_some()
            || self.vmmap_text_resident.is_some()
            || self.vmmap_iosurface_resident.is_some()
    }

    fn log_line(&self) -> String {
        format!(
            "pid={} rss={} physical_footprint={} peak_physical_footprint={} vmmap_total_resident={} graphics_resident={} malloc_resident={}{}{}",
            self.pid,
            self.resident_size
                .map(format_bytes)
                .unwrap_or_else(|| "unavailable".to_string()),
            self.physical_footprint
                .map(format_bytes)
                .unwrap_or_else(|| "unavailable".to_string()),
            self.peak_physical_footprint
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
        let mut lines = vec![
            "ThinkTerm Memory Snapshot".to_string(),
            format!("pid: {}", self.pid),
            format!(
                "rss: {}",
                self.resident_size
                    .map(format_bytes)
                    .unwrap_or_else(|| "unavailable".to_string())
            ),
            format!(
                "physical_footprint: {}",
                self.physical_footprint
                    .map(format_bytes)
                    .unwrap_or_else(|| "unavailable".to_string())
            ),
            format!(
                "peak_physical_footprint: {}",
                self.peak_physical_footprint
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

    fn localized_description(self) -> String {
        crate::i18n::tr(match self {
            Self::Settings => "settings-font-size-settings-description",
            Self::Home => "settings-font-size-home-description",
            Self::RightSidebar => "settings-font-size-right-sidebar-description",
            Self::Sidebar => "settings-font-size-workspace-sidebar-description",
            Self::TabBar => "settings-font-size-tab-bar-description",
            Self::PaneHeader => "settings-font-size-pane-header-description",
        })
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
    open_dropdown: Option<SettingsDropdown>,
    memory_monitoring: bool,
    memory_monitor_generation: u64,
    memory_snapshot: Option<MemorySnapshot>,
    main_window_resource_lines: Vec<String>,
    memory_snapshot_copied_until: Option<Instant>,
    /// The archived rows as last painted; row-action indices resolve here
    /// so a click acts on exactly what the user saw.
    archived_rows: Vec<crate::workspace_threads::ArchivedProjectRow>,
    /// The shells found on this machine. Refreshed when the Terminal
    /// section is entered, never while painting: discovery touches the
    /// filesystem, and the paint path must not.
    shell_catalog: Vec<crate::shell_catalog::DiscoveredShell>,
    /// Two-step delete: the project id whose Delete button was clicked
    /// once. A second click executes; any other action clears it.
    confirm_delete_archived: Option<String>,
    input_diagnostics_copied_until: Option<Instant>,
    sidebar_scrollbar_visible_until: Option<Instant>,
    content_scrollbar_visible_until: Option<Instant>,
}

impl SettingsUiState {
    fn new(dpi: usize) -> Self {
        let tokens = UiTokens::for_dpi(dpi);
        Self {
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
            open_dropdown: None,
            memory_monitoring: false,
            memory_monitor_generation: 0,
            memory_snapshot: None,
            main_window_resource_lines: Vec::new(),
            memory_snapshot_copied_until: None,
            archived_rows: Vec::new(),
            shell_catalog: Vec::new(),
            confirm_delete_archived: None,
            input_diagnostics_copied_until: None,
            sidebar_scrollbar_visible_until: None,
            content_scrollbar_visible_until: None,
        }
    }
}

fn format_bytes(bytes: u64) -> String {
    let mib = bytes as f64 / 1024.0 / 1024.0;
    if mib >= 1024.0 {
        format!("{:.2} GB", mib / 1024.0)
    } else {
        format!("{mib:.1} MB")
    }
}

fn capture_memory_snapshot(detailed: bool) -> MemorySnapshot {
    let pid = std::process::id();
    let mut snapshot = MemorySnapshot {
        captured_at: Instant::now(),
        pid,
        resident_size: None,
        physical_footprint: None,
        peak_physical_footprint: None,
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
            snapshot.physical_footprint = Some(info.physical_footprint);
            snapshot.peak_physical_footprint = Some(info.peak_physical_footprint);
        }
        Err(err) => {
            snapshot.error = Some(err);
        }
    }

    if detailed {
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
    resident_size: u64,
    physical_footprint: u64,
    peak_physical_footprint: u64,
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
        physical_footprint: info.ri_phys_footprint,
        peak_physical_footprint: info.ri_lifetime_max_phys_footprint,
    })
}

#[cfg(not(target_os = "macos"))]
fn capture_process_memory_info(_pid: u32) -> Result<ProcessMemoryInfo, String> {
    Err("memory diagnostics are only wired on macOS for now".to_string())
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
    control_bg: LinearRgba,
    control_hover_bg: LinearRgba,
    control_pressed_bg: LinearRgba,
    control_border: LinearRgba,
    card_bg: LinearRgba,
    title: LinearRgba,
    text: LinearRgba,
    secondary_text: LinearRgba,
    muted_text: LinearRgba,
    selected_text: LinearRgba,
    rule: LinearRgba,
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

struct SettingsWindow {
    instance_id: u64,
    cleaned_up: bool,
    window: Option<Window>,
    dimensions: Dimensions,
    window_state: WindowState,
    fonts: Rc<FontConfiguration>,
    ui_font: Rc<LoadedFont>,
    title_font: Rc<LoadedFont>,
    sidebar_title_font: Rc<LoadedFont>,
    metrics: RenderMetrics,
    render_state: Option<RenderState>,
    webgpu: Option<Rc<WebGpuState>>,
    appearance: Appearance,
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

        let settings = Rc::new(RefCell::new(Self {
            instance_id,
            cleaned_up: false,
            window: None,
            dimensions,
            window_state: WindowState::default(),
            fonts: Rc::clone(&fonts),
            ui_font,
            title_font,
            sidebar_title_font,
            metrics,
            render_state: None,
            webgpu: None,
            appearance,
            selected: initial_section(),
            agents_expanded: initial_expanded_agent(),
            native_settings,
            active_main_renderer,
            ui,
            ui_context: UiContext::default(),
            compatibility_import: CompatibilityImportState::default(),
            status: Self::initial_status(),
            shape_cache: RefCell::new(HashMap::new()),
            pending_glyph_error: RefCell::new(None),
        }));

        let event_settings = Rc::clone(&settings);
        let geometry = RequestedWindowGeometry {
            width: Dimension::Pixels(dimensions.pixel_width as f32),
            height: Dimension::Pixels(dimensions.pixel_height as f32),
            x: None,
            y: None,
            macos_frame_autosave_name: None,
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
        let webgpu = match WebGpuState::new(&window, dimensions, &config).await {
            Ok(webgpu) => Rc::new(webgpu),
            Err(err) => {
                window.close();
                return Err(err);
            }
        };
        // `dimensions` is still the pre-window guess: we had to pick a dpi
        // before there was a window to ask. A Resized event can already have
        // landed while the window and its gpu surface were being created, and
        // that handler is what rebuilt the ui tokens and re-rasterized the
        // fonts. Writing the guess back wholesale would strand the geometry on
        // the old dpi while the tokens and fonts sit on the new one, which
        // paints half-size controls around full-size text.
        let mut dimensions = dimensions;
        dimensions.dpi = settings.borrow().dimensions.dpi;
        webgpu.resize(dimensions);
        let dimensions = *webgpu.dimensions.borrow();
        {
            let mut settings = settings.borrow_mut();
            settings.dimensions = dimensions;
            if let Err(err) = settings.created(RenderContext::WebGpu(Rc::clone(&webgpu))) {
                window.close();
                return Err(err);
            }
            settings.webgpu.replace(webgpu);
            settings.window.replace(window.clone());
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

        window.show();
        window.invalidate();

        Ok(())
    }

    fn created(&mut self, context: RenderContext) -> anyhow::Result<()> {
        self.render_state
            .replace(RenderState::new(context, &self.fonts, &self.metrics, 256)?);
        Ok(())
    }

    fn cleanup(&mut self) {
        if self.cleaned_up {
            return;
        }
        self.cleaned_up = true;
        self.commit_focused_input();
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
                self.ui.drag = None;
                window.set_cursor(Some(MouseCursor::Arrow));
                window.invalidate();
                Ok(true)
            }
            WindowEvent::AppearanceChanged(appearance) => {
                self.appearance = appearance;
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
                        Some(WidgetKind::ScrollArea) | None => MouseCursor::Arrow,
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
                        | SettingsAction::RemoteDropDestinationInput,
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
                    Some(
                        SettingsAction::ToggleThemeModeMenu
                        | SettingsAction::SetThemeMode(_)
                        | SettingsAction::ToggleLanguageMenu
                        | SettingsAction::SetLanguage(_)
                        | SettingsAction::ToggleAppIconMenu
                        | SettingsAction::SetAppIcon(_)
                        | SettingsAction::ToggleMainRendererMenu
                        | SettingsAction::SetMainRenderer(_)
                        | SettingsAction::ToggleDefaultShellMenu
                        | SettingsAction::SetDefaultShell(_)
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
            if self.webgpu.is_some() {
                "WebGpu"
            } else {
                "none"
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
            return configured.to_string();
        }
        match crate::native_settings::effective_remote_download_directory() {
            Some(path) => settings_tr(
                "settings-system-default-path",
                &[("path", path.display().to_string())],
            ),
            None => crate::i18n::tr("settings-no-downloads-folder"),
        }
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
                self.native_settings = crate::native_settings::load();
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

    fn save_and_apply_bottom_quote_settings(&mut self, status: String) {
        match crate::native_settings::save(&self.native_settings) {
            Ok(()) => {
                if let Some(front_end) = crate::frontend::try_front_end() {
                    front_end.invalidate_all_windows();
                }
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

    fn effective_appearance(&self) -> Appearance {
        self.native_settings
            .appearance
            .theme_mode
            .effective_appearance(self.appearance)
    }

    fn palette(&self) -> SettingsPalette {
        let appearance = self.effective_appearance();
        let ui = UiPalette::for_appearance(appearance);
        match appearance {
            Appearance::Light | Appearance::LightHighContrast => SettingsPalette {
                window_bg: ui.window_bg,
                sidebar_bg: ui.workspace_sidebar_bg,
                separator: ui.separator,
                search_bg: ui.control_bg,
                search_border: ui.control_border,
                nav_hover_bg: ui.sidebar_row_hover_bg,
                nav_pressed_bg: ui.control_pressed_bg,
                nav_selected_bg: ui.sidebar_row_active_bg,
                control_bg: ui.control_bg,
                control_hover_bg: ui.control_hover_bg,
                control_pressed_bg: ui.control_pressed_bg,
                control_border: ui.control_border,
                card_bg: rgba(255, 255, 255, 0.72),
                title: ui.text,
                text: ui.text,
                secondary_text: ui.secondary_text,
                muted_text: ui.muted_text,
                selected_text: ui.text,
                rule: ui.separator,
            },
            Appearance::Dark | Appearance::DarkHighContrast => SettingsPalette {
                window_bg: ui.window_bg,
                sidebar_bg: ui.workspace_sidebar_bg,
                separator: ui.separator,
                search_bg: ui.control_bg,
                search_border: ui.control_border,
                nav_hover_bg: ui.sidebar_row_hover_bg,
                nav_pressed_bg: ui.control_pressed_bg,
                nav_selected_bg: ui.sidebar_row_active_bg,
                control_bg: ui.control_bg,
                control_hover_bg: ui.control_hover_bg,
                control_pressed_bg: ui.control_pressed_bg,
                control_border: ui.control_border,
                card_bg: rgba(30, 30, 32, 0.78),
                title: ui.text,
                text: ui.text,
                secondary_text: ui.secondary_text,
                muted_text: ui.muted_text,
                selected_text: ui.text,
                rule: ui.separator,
            },
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

    fn settings_content_extent(&self, bottom_y: f32) -> f32 {
        (bottom_y + self.ui_px(65.0)).max(self.content_bottom())
    }

    fn developer_mode_enabled(&self) -> bool {
        self.native_settings.developer.developer_mode
    }

    fn visible_sections(&self) -> Vec<SettingsSection> {
        let mut sections = Vec::with_capacity(BASE_SECTIONS.len() + DEVELOPER_SECTIONS.len());
        for section in BASE_SECTIONS {
            if *section == SettingsSection::About && self.developer_mode_enabled() {
                sections.extend_from_slice(DEVELOPER_SECTIONS);
            }
            sections.push(*section);
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
                self.selected = section;
                self.ui.content_scroll.reset();
                self.ui.open_dropdown = None;
                if section == SettingsSection::Agents {
                    // Probe PATH on entry so painting never touches the
                    // filesystem.
                    crate::agent_status::refresh_path_probe();
                }
                if section == SettingsSection::Terminal {
                    // Same rule: shell discovery stats candidate paths, so
                    // it happens on entry and the row paints from the cache.
                    self.refresh_shell_catalog();
                }
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
                    }
                    Err(err) => {
                        self.status = settings_tr(
                            "settings-status-import-error",
                            &[("error", format!("{err:#}"))],
                        );
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
                        self.native_settings = pending;
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
            SettingsAction::CycleRemotePaneResizeMode => {
                self.ui.open_dropdown = None;
                self.native_settings.terminal.remote_pane_resize_mode =
                    self.native_settings.terminal.remote_pane_resize_mode.next();
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
                                (
                                    "value",
                                    localized_remote_pane_resize_mode_label(
                                        self.native_settings.terminal.remote_pane_resize_mode,
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
            SettingsAction::CycleBottomQuoteMode => {
                self.ui.open_dropdown = None;
                self.native_settings.terminal.bottom_quote_mode =
                    self.native_settings.terminal.bottom_quote_mode.next();
                self.save_and_apply_bottom_quote_settings(settings_tr(
                    "settings-status-value-now",
                    &[
                        ("setting", crate::i18n::tr("settings-quote-rotation")),
                        (
                            "value",
                            localized_quote_mode_label(
                                self.native_settings.terminal.bottom_quote_mode,
                            ),
                        ),
                    ],
                ));
            }
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
            SettingsAction::ResetBottomQuotesJson => {
                self.ui.open_dropdown = None;
                match crate::bottom_quotes::reset_quotes_file() {
                    Ok(path) => {
                        if let Some(front_end) = crate::frontend::try_front_end() {
                            front_end.invalidate_all_windows();
                        }
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
            SettingsAction::ToggleAgentPanel => {
                self.ui.open_dropdown = None;
                self.native_settings.chrome.agent_panel_enabled =
                    !self.native_settings.chrome.agent_panel_enabled;
                match crate::native_settings::save(&self.native_settings) {
                    Ok(()) => {
                        // After the save so the detector's preference
                        // closure reads the new shared value; without this
                        // the gate waits for the next safety tick.
                        mux::agent_status::refresh_enabled();
                        self.status = if self.native_settings.chrome.agent_panel_enabled {
                            crate::i18n::tr("settings-agent-panel-enabled")
                        } else {
                            crate::i18n::tr("settings-agent-panel-disabled")
                        };
                        if let Some(front_end) = crate::frontend::try_front_end() {
                            front_end.invalidate_all_windows();
                        }
                    }
                    Err(err) => {
                        self.status = format!("Unable to save agent panel setting: {err:#}");
                    }
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
            SettingsAction::ToggleThemeModeMenu => {
                self.ui.open_dropdown =
                    if self.ui.open_dropdown == Some(SettingsDropdown::ThemeMode) {
                        None
                    } else {
                        Some(SettingsDropdown::ThemeMode)
                    };
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
                self.native_settings.appearance.theme_mode = mode;
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
                        let mut args = FluentArgs::new();
                        args.set("error", format!("{err:#}"));
                        self.status = crate::i18n::tr_args("settings-theme-save-error", &args);
                    }
                }
            }
            SettingsAction::ToggleAppIconMenu => {
                self.ui.open_dropdown = if self.ui.open_dropdown == Some(SettingsDropdown::AppIcon)
                {
                    None
                } else {
                    Some(SettingsDropdown::AppIcon)
                };
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
        if self.webgpu.is_none() || self.render_state.is_none() {
            return false;
        }

        let paint_start = crate::perf::now();
        let animating = self.advance_scroll_animations(Instant::now());
        match self.do_paint_webgpu() {
            Ok(ok) => {
                crate::perf::log_duration("settings_paint", paint_start);
                if animating {
                    window.invalidate();
                }
                ok
            }
            Err(err) => {
                crate::perf::log_duration("settings_paint_failed", paint_start);
                log::error!("settings window webgpu paint failed: {err:#}");
                false
            }
        }
    }

    fn advance_scroll_animations(&mut self, now: Instant) -> bool {
        self.ui.sidebar_scroll.advance_animation(now)
            | self.ui.content_scroll.advance_animation(now)
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

    fn do_paint_webgpu_impl(&mut self, webgpu: &WebGpuState) -> anyhow::Result<bool> {
        for _ in 0..3 {
            let paint_pass_start = crate::perf::now();
            match self.paint_pass() {
                Ok(()) => {
                    crate::perf::log_duration("settings_paint_pass", paint_pass_start);
                    match self.render_state.as_mut().unwrap().allocated_more_quads() {
                        Ok(true) => continue,
                        Ok(false) => break,
                        Err(err) => {
                            log::error!("settings window quad allocation failed: {err:#}");
                            break;
                        }
                    }
                }
                Err(err) => {
                    if let Some(&OutOfTextureSpace {
                        size: Some(size),
                        current_size,
                    }) = err.root_cause().downcast_ref::<OutOfTextureSpace>()
                    {
                        let size = size.max(current_size);
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
                        continue;
                    }
                    log::error!("settings window paint failed: {err:#}");
                    break;
                }
            }
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
        let ui_font = Rc::clone(&self.ui_font);
        let sidebar_title_font = Rc::clone(&self.sidebar_title_font);
        let nav_font = Rc::clone(&self.ui_font);
        let tokens = self.ui.tokens;
        let sidebar_width = self.ui.sidebar.width;
        let sidebar_icon_size = ((self.metrics.cell_size.height as f32 + self.ui_px(8.0))
            .clamp(self.ui_px(24.0), self.ui_px(34.0))
            .round()) as usize;

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
        let search_text = self.ui.search.text().to_string();
        self.paint_text_input(
            layers,
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
            search_rect.origin.y + (search_rect.size.height - sidebar_icon_size as f32) / 2.0,
            sidebar_icon_size as f32,
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
                self.draw_rounded_rect(
                    layers,
                    0,
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
        let list_top = self.ui_px(SIDEBAR_LIST_TOP);
        let list_bottom = (self.dimensions.pixel_height as f32 - self.ui_px(16.0)).max(list_top);
        let list_height = list_bottom - list_top;
        let content_extent = sections.len() as f32 * self.ui_px(NAV_ROW_STEP) + 8.0;
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
                &ui_font,
                tokens.sidebar_padding + 12.0,
                list_top + self.ui_px(18.0),
                &crate::i18n::tr("settings-search-no-results"),
                palette.muted_text,
                sidebar_width - tokens.sidebar_padding * 2.0 - 24.0,
            )?;
        }

        for section in sections {
            if y + self.ui_px(NAV_ROW_HEIGHT) < list_top || y > list_bottom {
                y += self.ui_px(NAV_ROW_STEP);
                continue;
            }
            let action = SettingsAction::Select(section);
            let selected = section == self.selected;
            let hovered = self.ui.interaction.hovered == Some(action);
            let pressed = self.ui.interaction.pressed == Some(action);
            let row_y = y - self.ui_px(6.0);
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
            if let Some(row_bg) = row_bg {
                self.draw_rounded_rect(
                    layers,
                    0,
                    row_x,
                    row_y,
                    row_width,
                    self.ui_px(NAV_ROW_HEIGHT),
                    row_bg,
                    self.ui_px(NAV_ROW_RADIUS),
                )?;
            }

            self.ui_context.push(
                rect(row_x, row_y, row_width, self.ui_px(NAV_ROW_HEIGHT)),
                WidgetKind::SidebarRow,
                action,
            );
            let text_color = if selected {
                palette.selected_text
            } else {
                palette.secondary_text
            };
            self.draw_svg_icon(
                layers,
                section.icon().svg(),
                row_x + self.ui_px(14.0),
                row_y + (self.ui_px(NAV_ROW_HEIGHT) - sidebar_icon_size as f32) / 2.0,
                sidebar_icon_size as f32,
                text_color,
            )?;
            let section_label = section.label();
            self.draw_text(
                layers,
                &nav_font,
                row_x + self.ui_px(56.0),
                self.control_text_y(row_y, self.ui_px(NAV_ROW_HEIGHT)),
                &section_label,
                text_color,
                row_width - self.ui_px(76.0),
            )?;
            y += self.ui_px(NAV_ROW_STEP);
        }

        self.paint_scrollbar(
            layers,
            list_area,
            self.ui.sidebar_scroll,
            self.sidebar_scrollbar_visible(),
        )?;

        Ok(())
    }

    fn paint_content(&mut self, layers: &mut TripleLayerQuadAllocator<'_>) -> anyhow::Result<()> {
        let palette = self.palette();
        let ui_font = Rc::clone(&self.ui_font);
        let title_font = Rc::clone(&self.title_font);
        let sidebar_width = self.ui.sidebar.width;
        let window_width = self.dimensions.pixel_width as f32;
        let content_gap = if window_width < 980.0 { 30.0 } else { 46.0 };
        let right_margin = if window_width < 980.0 { 34.0 } else { 50.0 };
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
        self.draw_text(
            layers,
            &title_font,
            x,
            self.ui_px(CONTENT_TITLE_Y) - scroll,
            &selected_label,
            palette.title,
            max_width,
        )?;

        match self.selected {
            SettingsSection::Appearance => self.paint_appearance(layers, x, max_width)?,
            SettingsSection::Compatibility => self.paint_compatibility(layers, x, max_width)?,
            SettingsSection::General => self.paint_general(layers, x, max_width)?,
            SettingsSection::Terminal => self.paint_terminal(layers, x, max_width)?,
            SettingsSection::Workspaces => self.paint_workspaces(layers, x, max_width)?,
            SettingsSection::Agents => self.paint_agents(layers, x, max_width)?,
            SettingsSection::Archived => self.paint_archived(layers, x, max_width)?,
            SettingsSection::Keymap => self.paint_placeholder(
                layers,
                &ui_font,
                x,
                &crate::i18n::tr("settings-keymap-description"),
                max_width,
            )?,
            SettingsSection::Developer => self.paint_developer(layers, x, max_width)?,
            SettingsSection::UiKit => self.paint_ui_kit(layers, x, max_width)?,
            SettingsSection::Memory => self.paint_memory_diagnostics(layers, x, max_width)?,
            SettingsSection::About => self.paint_placeholder(
                layers,
                &ui_font,
                x,
                &crate::i18n::tr("settings-about-description"),
                max_width,
            )?,
        }
        self.paint_scrollbar(
            layers,
            content_area,
            self.ui.content_scroll,
            self.content_scrollbar_visible(),
        )?;
        self.paint_open_dropdown_overlay(layers, x, max_width)?;

        Ok(())
    }

    fn paint_general(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        max_width: f32,
    ) -> anyhow::Result<()> {
        let palette = self.palette();
        let ui_font = Rc::clone(&self.ui_font);
        let scroll = self.ui.content_scroll.offset;
        let row_step = self.settings_row_step();
        let section_y = self.ui_px(CONTENT_SECTION_Y) - scroll;
        // Each settings card owns its row count because rows are painted manually.
        // General currently paints eight rows below; the count drives card height and scroll extent.
        let row_count = 8;
        let (card_y, first_row_y) = self.settings_card_geometry(section_y, row_count);
        let card_height = self.settings_card_height(row_count);
        self.ui.content_scroll.set_extents(
            self.content_viewport_extent(),
            self.settings_content_extent(card_y + scroll + card_height),
        );
        let source = Self::config_source_summary();
        let native_path = Self::native_settings_path();
        let native_status = if native_path.exists() {
            native_path.display().to_string()
        } else {
            let mut args = FluentArgs::new();
            args.set("path", native_path.display().to_string());
            crate::i18n::tr_args("settings-native-settings-default", &args)
        };
        let config_source_kind = if config::configuration_file().is_some() {
            crate::i18n::tr("common-file")
        } else {
            crate::i18n::tr("common-defaults")
        };

        self.draw_text(
            layers,
            &ui_font,
            x,
            section_y,
            &crate::i18n::tr("settings-general-heading"),
            palette.muted_text,
            max_width,
        )?;

        let card_x = x;
        let card_padding = 36.0;
        let row_x = card_x + card_padding;
        let row_width = max_width - card_padding * 2.0;
        self.paint_group_card(layers, card_x, card_y, max_width, card_height)?;
        self.paint_language_row(layers, row_x, first_row_y, row_width, false)?;
        self.paint_setting_row(
            layers,
            row_x,
            first_row_y + row_step,
            row_width,
            &crate::i18n::tr("settings-config-source"),
            &source,
            &config_source_kind,
            false,
        )?;
        self.paint_setting_row(
            layers,
            row_x,
            first_row_y + row_step * 2.0,
            row_width,
            &crate::i18n::tr("settings-native-settings"),
            &native_status,
            &format!("v{}", self.native_settings.version),
            true,
        )?;
        self.paint_theme_mode_row(
            layers,
            row_x,
            first_row_y + row_step * 3.0,
            row_width,
            &crate::i18n::tr("settings-native-settings-description"),
            true,
        )?;
        self.paint_main_renderer_row(layers, row_x, first_row_y + row_step * 4.0, row_width, true)?;
        self.paint_toggle_setting_row(
            layers,
            row_x,
            first_row_y + row_step * 5.0,
            row_width,
            &crate::i18n::tr("settings-restore-window-frame"),
            &crate::i18n::tr("settings-restore-window-frame-description"),
            self.native_settings.window.restore_main_window_frame,
            SettingsAction::ToggleMainWindowFrameRestore,
            true,
        )?;
        self.paint_restart_row(layers, row_x, first_row_y + row_step * 6.0, row_width, true)?;
        self.paint_action_setting_row(
            layers,
            row_x,
            first_row_y + row_step * 7.0,
            row_width,
            &crate::i18n::tr("settings-quit"),
            &crate::i18n::tr("settings-quit-description"),
            &crate::i18n::tr("settings-quit-app"),
            SettingsAction::QuitApplication,
            true,
        )?;
        Ok(())
    }

    fn paint_appearance(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        max_width: f32,
    ) -> anyhow::Result<()> {
        let palette = self.palette();
        let ui_font = Rc::clone(&self.ui_font);
        let config = configuration();
        let scroll = self.ui.content_scroll.offset;
        let row_step = self.settings_row_step();
        let section_y = self.ui_px(CONTENT_SECTION_Y) - scroll;
        let theme_row_count = 4;
        // The font-size steppers plus the trailing font-weight stepper.
        let typography_row_count = TYPOGRAPHY_FONT_AREAS.len() + 1;
        let app_icon_note_space = self.metrics.cell_size.height as f32 + self.ui_px(10.0);
        let (theme_card_y, mut y) = self.settings_card_geometry(section_y, theme_row_count);
        let theme_card_height = self.settings_card_height(theme_row_count) + app_icon_note_space;
        let typography_title_y =
            theme_card_y + theme_card_height + self.settings_section_card_gap();
        let typography_card_y = typography_title_y + self.settings_section_card_gap().min(54.0);
        let typography_first_row_y = typography_card_y + self.settings_card_top_padding();
        let typography_card_height = self.settings_card_height(typography_row_count);
        let config_source_kind = if config::configuration_file().is_some() {
            crate::i18n::tr("common-file")
        } else {
            crate::i18n::tr("common-defaults")
        };
        self.ui.content_scroll.set_extents(
            self.content_viewport_extent(),
            self.settings_content_extent(typography_card_y + scroll + typography_card_height),
        );

        self.draw_text(
            layers,
            &ui_font,
            x,
            section_y,
            &crate::i18n::tr("settings-appearance-theme-heading"),
            palette.muted_text,
            max_width,
        )?;

        let card_padding = 36.0;
        let row_x = x + card_padding;
        let row_width = max_width - card_padding * 2.0;
        self.paint_group_card(layers, x, theme_card_y, max_width, theme_card_height)?;
        self.paint_theme_mode_row(
            layers,
            row_x,
            y,
            row_width,
            &crate::i18n::tr("settings-theme-description"),
            false,
        )?;
        y += row_step;
        self.paint_app_icon_row(
            layers,
            row_x,
            y,
            row_width,
            &crate::i18n::tr("settings-app-icon-description"),
            &crate::i18n::tr("settings-app-icon-note"),
            true,
        )?;
        y += row_step + app_icon_note_space;
        self.paint_setting_row(
            layers,
            row_x,
            y,
            row_width,
            &crate::i18n::tr("settings-effective-color-scheme"),
            &crate::i18n::tr("settings-effective-color-scheme-description"),
            Self::effective_color_scheme_label(&config),
            true,
        )?;
        y += row_step;
        self.paint_setting_row(
            layers,
            row_x,
            y,
            row_width,
            &crate::i18n::tr("settings-config-source"),
            &Self::config_source_summary(),
            &config_source_kind,
            true,
        )?;

        self.draw_text(
            layers,
            &ui_font,
            x,
            typography_title_y,
            &crate::i18n::tr("settings-typography-heading"),
            palette.muted_text,
            max_width,
        )?;

        self.paint_group_card(
            layers,
            x,
            typography_card_y,
            max_width,
            typography_card_height,
        )?;
        y = typography_first_row_y;
        for area in TYPOGRAPHY_FONT_AREAS {
            let label = area.localized_label();
            let description = area.localized_description();
            self.paint_font_size_stepper_row(
                layers,
                row_x,
                y,
                row_width,
                &label,
                &description,
                self.current_chrome_font_size_value(area),
                None,
                SettingsAction::ResetChromeFontSize(area),
                SettingsAction::DecreaseChromeFontSize(area),
                SettingsAction::IncreaseChromeFontSize(area),
                area != ChromeFontArea::Settings,
            )?;
            y += row_step;
        }
        self.paint_font_size_stepper_row(
            layers,
            row_x,
            y,
            row_width,
            &crate::i18n::tr("settings-font-weight"),
            &crate::i18n::tr("settings-font-weight-description"),
            self.current_settings_font_weight_value(),
            None,
            SettingsAction::ResetSettingsFontWeight,
            SettingsAction::DecreaseSettingsFontWeight,
            SettingsAction::IncreaseSettingsFontWeight,
            true,
        )?;

        Ok(())
    }

    fn paint_archived(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        max_width: f32,
    ) -> anyhow::Result<()> {
        let palette = self.palette();
        let ui_font = Rc::clone(&self.ui_font);
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
            &ui_font,
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
                &ui_font,
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
                &ui_font,
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
        let ui_font = Rc::clone(&self.ui_font);
        let scroll = self.ui.content_scroll.offset;
        let section_y = self.ui_px(CONTENT_SECTION_Y) - scroll;
        // Idle timeout, download folder, its reset, and the drop destination.
        let row_count = 4;
        let (card_y, first_row_y) = self.settings_card_geometry(section_y, row_count);
        let card_height = self.settings_card_height(row_count);
        // Notification sounds are not a remote-files concern, so they get their
        // own card rather than being filed under that heading.
        let sounds_row_count = 1;
        let sounds_title_y = card_y + card_height + self.settings_section_card_gap();
        let sounds_card_y = sounds_title_y + self.settings_section_card_gap().min(54.0);
        let sounds_first_row_y = sounds_card_y + self.settings_card_top_padding();
        let sounds_card_height = self.settings_card_height(sounds_row_count);
        self.ui.content_scroll.set_extents(
            self.content_viewport_extent(),
            self.settings_content_extent(sounds_card_y + scroll + sounds_card_height),
        );

        self.draw_text(
            layers,
            &ui_font,
            x,
            section_y,
            &crate::i18n::tr("settings-workspaces-heading"),
            palette.muted_text,
            max_width,
        )?;
        let padding = 36.0;
        let row_x = x + padding;
        let row_width = max_width - padding * 2.0;
        self.paint_group_card(layers, x, card_y, max_width, card_height)?;
        let minutes = self.current_remote_sftp_idle_minutes();
        let mut minutes_args = FluentArgs::new();
        minutes_args.set("count", minutes);
        let value_label = crate::i18n::tr_args("common-minutes", &minutes_args);
        self.paint_font_size_stepper_row(
            layers,
            row_x,
            first_row_y,
            row_width,
            &crate::i18n::tr("settings-sftp-idle"),
            &crate::i18n::tr("settings-sftp-idle-description"),
            minutes as f64,
            Some(&value_label),
            SettingsAction::ResetRemoteSftpIdle,
            SettingsAction::DecreaseRemoteSftpIdle,
            SettingsAction::IncreaseRemoteSftpIdle,
            false,
        )?;

        let row_step = self.settings_row_step();
        let download_label = self.remote_download_directory_label();
        self.paint_action_setting_row(
            layers,
            row_x,
            first_row_y + row_step,
            row_width,
            &crate::i18n::tr("settings-download-folder"),
            &download_label,
            &crate::i18n::tr("common-choose"),
            SettingsAction::ChooseRemoteDownloadDirectory,
            true,
        )?;
        self.paint_action_setting_row(
            layers,
            row_x,
            first_row_y + row_step * 2.0,
            row_width,
            &crate::i18n::tr("settings-system-download-folder"),
            &crate::i18n::tr("settings-system-download-folder-description"),
            &crate::i18n::tr("common-reset"),
            SettingsAction::ResetRemoteDownloadDirectory,
            true,
        )?;
        let remote_drop_value = self.ui.remote_drop_input.text().to_string();
        self.paint_text_setting_row(
            layers,
            row_x,
            first_row_y + row_step * 3.0,
            row_width,
            &crate::i18n::tr("settings-remote-drop-destination"),
            &crate::i18n::tr("settings-remote-drop-destination-description"),
            &remote_drop_value,
            crate::native_settings::DEFAULT_REMOTE_DROP_DESTINATION,
            SettingsAction::RemoteDropDestinationInput,
            true,
        )?;

        self.draw_text(
            layers,
            &ui_font,
            x,
            sounds_title_y,
            &crate::i18n::tr("settings-notifications-heading"),
            palette.muted_text,
            max_width,
        )?;
        self.paint_group_card(layers, x, sounds_card_y, max_width, sounds_card_height)?;
        self.paint_toggle_setting_row(
            layers,
            row_x,
            sounds_first_row_y,
            row_width,
            &crate::i18n::tr("settings-notification-sounds"),
            &crate::i18n::tr("settings-notification-sounds-description"),
            self.native_settings.workspaces.notification_sounds_enabled,
            SettingsAction::ToggleNotificationSounds,
            true,
        )
    }

    fn paint_agents(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        max_width: f32,
    ) -> anyhow::Result<()> {
        let palette = self.palette();
        let ui_font = Rc::clone(&self.ui_font);
        let scroll = self.ui.content_scroll.offset;
        let section_y = self.ui_px(CONTENT_SECTION_Y) - scroll;

        // Card 1: the panel/detection feature toggle.
        let toggle_row_count = 1;
        let (card_y, first_row_y) = self.settings_card_geometry(section_y, toggle_row_count);
        let card_height = self.settings_card_height(toggle_row_count);
        // Card 2: one row per supported agent. Clicking a row expands an
        // inset card explaining how that agent is covered; the outer card
        // grows by the detail block's height.
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
        let integrations_title_y = card_y + card_height + self.settings_section_card_gap();
        let integrations_card_y = integrations_title_y + self.settings_section_card_gap().min(54.0);
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
            &ui_font,
            x,
            section_y,
            &crate::i18n::tr("settings-agents-heading"),
            palette.muted_text,
            max_width,
        )?;
        self.paint_group_card(layers, x, card_y, max_width, card_height)?;
        self.paint_toggle_setting_row(
            layers,
            row_x,
            first_row_y,
            row_width,
            &crate::i18n::tr("settings-agent-panel"),
            &crate::i18n::tr("settings-agent-panel-description"),
            self.native_settings.chrome.agent_panel_enabled,
            SettingsAction::ToggleAgentPanel,
            // Sole row in its card: a top rule would just underline the
            // group heading for no reason.
            false,
        )?;

        self.draw_text(
            layers,
            &ui_font,
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
                &ui_font,
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
                            &ui_font,
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

    fn paint_terminal(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        max_width: f32,
    ) -> anyhow::Result<()> {
        let palette = self.palette();
        let ui_font = Rc::clone(&self.ui_font);
        let config = configuration();
        let scroll = self.ui.content_scroll.offset;
        let row_step = self.settings_row_step();
        let section_y = self.ui_px(CONTENT_SECTION_Y) - scroll;
        // Each settings card owns its row count because rows are painted manually.
        // Terminal currently paints nine rows below; the count drives card height and scroll extent.
        // What must stay in sync with paint_open_dropdown_overlay is not this
        // number but each row's `row_step * N` multiplier, which that function
        // copies by hand to place an open menu.
        let row_count = 9;
        let (card_y, first_row_y) = self.settings_card_geometry(section_y, row_count);
        let card_height = self.settings_card_height(row_count);
        let button_y = card_y + card_height + self.settings_section_card_gap();
        let open_quotes_label = crate::i18n::tr("settings-open-quotes-json");
        let reset_quotes_label = crate::i18n::tr("settings-reset-quotes-json");
        let open_quotes_width = self.button_width_for_label(&open_quotes_label, 260.0);
        let reset_quotes_width = self.button_width_for_label(&reset_quotes_label, 260.0);
        let button_gap = 16.0;
        let reset_button_x = x + open_quotes_width + button_gap;
        let reset_button_y = if open_quotes_width + button_gap + reset_quotes_width <= max_width {
            button_y
        } else {
            button_y + self.ui_px(CONTROL_HEIGHT) + 14.0
        };
        self.ui.content_scroll.set_extents(
            self.content_viewport_extent(),
            self.settings_content_extent(reset_button_y + scroll + self.ui_px(CONTROL_HEIGHT)),
        );
        let mut font_size_args = FluentArgs::new();
        font_size_args.set("value", format!("{:.1}", config.font_size));
        let font_size = crate::i18n::tr_args("common-points", &font_size_args);
        let font_family = Self::effective_font_family(&config);
        let quote_font_size_label = self.bottom_quote_font_size_label();
        let quote_interval_label = self.bottom_quote_interval_label();

        self.draw_text(
            layers,
            &ui_font,
            x,
            section_y,
            &crate::i18n::tr("settings-terminal-heading"),
            palette.muted_text,
            max_width,
        )?;

        let card_padding = 36.0;
        let card_x = x;
        let row_x = card_x + card_padding;
        let row_width = max_width - card_padding * 2.0;
        self.paint_group_card(layers, card_x, card_y, max_width, card_height)?;
        self.paint_default_shell_row(layers, row_x, first_row_y, row_width, false)?;
        self.paint_setting_row(
            layers,
            row_x,
            first_row_y + row_step,
            row_width,
            &crate::i18n::tr("settings-terminal-font-size"),
            &crate::i18n::tr("settings-terminal-font-size-description"),
            &font_size,
            true,
        )?;
        self.paint_setting_row(
            layers,
            row_x,
            first_row_y + row_step * 2.0,
            row_width,
            &crate::i18n::tr("settings-terminal-font-family"),
            &crate::i18n::tr("settings-terminal-font-family-description"),
            &font_family,
            true,
        )?;
        self.paint_font_size_stepper_row(
            layers,
            row_x,
            first_row_y + row_step * 3.0,
            row_width,
            &crate::i18n::tr("settings-terminal-native-font-size"),
            &crate::i18n::tr("settings-terminal-native-font-size-description"),
            self.current_terminal_font_size_value(),
            None,
            SettingsAction::ResetFontSize,
            SettingsAction::DecreaseFontSize,
            SettingsAction::IncreaseFontSize,
            true,
        )?;
        self.paint_action_setting_row(
            layers,
            row_x,
            first_row_y + row_step * 4.0,
            row_width,
            &crate::i18n::tr("settings-remote-pane-resize-mode"),
            &crate::i18n::tr("settings-remote-pane-resize-mode-description"),
            &localized_remote_pane_resize_mode_label(
                self.native_settings.terminal.remote_pane_resize_mode,
            ),
            SettingsAction::CycleRemotePaneResizeMode,
            true,
        )?;
        self.paint_toggle_setting_row(
            layers,
            row_x,
            first_row_y + row_step * 5.0,
            row_width,
            &crate::i18n::tr("settings-bottom-quote"),
            &crate::i18n::tr("settings-bottom-quote-description"),
            self.native_settings.terminal.bottom_quote_enabled,
            SettingsAction::ToggleBottomQuote,
            true,
        )?;
        self.paint_font_size_stepper_row(
            layers,
            row_x,
            first_row_y + row_step * 6.0,
            row_width,
            &crate::i18n::tr("settings-quote-font-size"),
            &crate::i18n::tr("settings-quote-font-size-description"),
            self.current_bottom_quote_font_size(),
            Some(&quote_font_size_label),
            SettingsAction::ResetBottomQuoteFontSize,
            SettingsAction::DecreaseBottomQuoteFontSize,
            SettingsAction::IncreaseBottomQuoteFontSize,
            true,
        )?;
        self.paint_action_setting_row(
            layers,
            row_x,
            first_row_y + row_step * 7.0,
            row_width,
            &crate::i18n::tr("settings-quote-rotation"),
            &crate::i18n::tr("settings-quote-rotation-description"),
            &localized_quote_mode_label(self.native_settings.terminal.bottom_quote_mode),
            SettingsAction::CycleBottomQuoteMode,
            true,
        )?;
        self.paint_font_size_stepper_row(
            layers,
            row_x,
            first_row_y + row_step * 8.0,
            row_width,
            &crate::i18n::tr("settings-quote-interval"),
            &crate::i18n::tr("settings-quote-interval-description"),
            self.current_bottom_quote_interval_minutes() as f64,
            Some(&quote_interval_label),
            SettingsAction::ResetBottomQuoteInterval,
            SettingsAction::DecreaseBottomQuoteInterval,
            SettingsAction::IncreaseBottomQuoteInterval,
            true,
        )?;
        self.draw_button(
            layers,
            x,
            button_y,
            open_quotes_width,
            &open_quotes_label,
            SettingsAction::OpenBottomQuotesJson,
        )?;
        self.draw_button(
            layers,
            if reset_button_y == button_y {
                reset_button_x
            } else {
                x
            },
            reset_button_y,
            reset_quotes_width,
            &reset_quotes_label,
            SettingsAction::ResetBottomQuotesJson,
        )?;
        Ok(())
    }

    fn paint_developer(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        max_width: f32,
    ) -> anyhow::Result<()> {
        let palette = self.palette();
        let ui_font = Rc::clone(&self.ui_font);
        let scroll = self.ui.content_scroll.offset;
        let row_step = self.settings_row_step();
        let section_y = self.ui_px(CONTENT_SECTION_Y) - scroll;
        let (card_y, first_row_y) = self.settings_card_geometry(section_y, 4);
        let card_height = self.settings_card_height(4);
        let button_y = card_y + card_height + self.settings_section_card_gap();
        self.ui.content_scroll.set_extents(
            self.content_viewport_extent(),
            self.settings_content_extent(button_y + scroll + self.ui_px(CONTROL_HEIGHT)),
        );

        self.draw_text(
            layers,
            &ui_font,
            x,
            section_y,
            &crate::i18n::tr("settings-developer-description"),
            palette.secondary_text,
            max_width,
        )?;

        let card_padding = 36.0;
        let row_x = x + card_padding;
        let row_width = max_width - card_padding * 2.0;
        let developer_mode_value = if self.developer_mode_enabled() {
            crate::i18n::tr("common-on")
        } else {
            crate::i18n::tr("common-off")
        };
        let developer_tabs_value = if self.developer_mode_enabled() {
            crate::i18n::tr("settings-developer-tabs-value")
        } else {
            crate::i18n::tr("common-hidden")
        };
        self.paint_group_card(layers, x, card_y, max_width, card_height)?;
        self.paint_setting_row(
            layers,
            row_x,
            first_row_y,
            row_width,
            &crate::i18n::tr("settings-developer-mode"),
            &crate::i18n::tr("settings-developer-mode-description"),
            &developer_mode_value,
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
        let developer_label = if self.developer_mode_enabled() {
            crate::i18n::tr("settings-disable-developer")
        } else {
            crate::i18n::tr("settings-enable-developer")
        };
        let developer_width = self.button_width_for_label(&developer_label, 300.0);
        let show_onboarding_label = crate::i18n::tr("settings-show-onboarding");
        self.draw_button(
            layers,
            x,
            button_y,
            developer_width,
            &developer_label,
            SettingsAction::ToggleDeveloperMode,
        )?;
        self.draw_button(
            layers,
            x + developer_width + self.ui_px(14.0),
            button_y,
            self.button_width_for_label(&show_onboarding_label, 300.0),
            &show_onboarding_label,
            SettingsAction::ShowOnboardingNow,
        )?;

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
        let two_column = max_width >= 940.0;
        let content_extent = if two_column { 1020.0 } else { 1660.0 };
        self.ui
            .content_scroll
            .set_extents(self.content_bottom(), content_extent);
        let scroll = self.ui.content_scroll.offset;

        self.draw_text(
            layers,
            &ui_font,
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
        let scroll = self.ui.content_scroll.offset;
        let row_step = self.settings_row_step();
        let section_y = self.ui_px(CONTENT_SECTION_Y) - scroll;
        let row_count = 15;
        let (card_y, first_row_y) = self.settings_card_geometry(section_y, row_count);
        let card_height = self.settings_card_height(row_count);
        let button_y = card_y + card_height + self.settings_section_card_gap();

        self.draw_text(
            layers,
            &ui_font,
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
            .physical_footprint
            .map(format_bytes)
            .unwrap_or_else(|| "Unavailable".to_string());
        let rss = snapshot
            .resident_size
            .map(format_bytes)
            .unwrap_or_else(|| "Unavailable".to_string());
        let peak = snapshot
            .peak_physical_footprint
            .map(format_bytes)
            .unwrap_or_else(|| "Unavailable".to_string());
        let total_resident = snapshot
            .vmmap_total_resident
            .map(format_bytes)
            .unwrap_or_else(|| "Run Refresh Now".to_string());
        let graphics = snapshot
            .vmmap_graphics_resident
            .map(format_bytes)
            .unwrap_or_else(|| "Run Refresh Now".to_string());
        let iosurface = snapshot
            .vmmap_iosurface_resident
            .map(format_bytes)
            .unwrap_or_else(|| "Run Refresh Now".to_string());
        let malloc = snapshot
            .vmmap_malloc_resident
            .map(format_bytes)
            .unwrap_or_else(|| "Run Refresh Now".to_string());
        let text = snapshot
            .vmmap_text_resident
            .map(format_bytes)
            .unwrap_or_else(|| "Run Refresh Now".to_string());
        let breakdown_status = if snapshot.has_vmmap_breakdown() {
            "Captured"
        } else if snapshot.vmmap_error.is_some() {
            "Unavailable"
        } else {
            "Refresh for vmmap"
        };
        let monitor_state = if self.ui.memory_monitoring {
            "Running"
        } else {
            "Off"
        };
        let input = crate::input_diagnostics::snapshot();
        let input_state = if input.enabled { "Running" } else { "Off" };
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

        let card_padding = 36.0;
        let row_x = x + card_padding;
        let row_width = max_width - card_padding * 2.0;
        self.paint_group_card(layers, x, card_y, max_width, card_height)?;
        self.paint_setting_row(
            layers,
            row_x,
            first_row_y,
            row_width,
            "Manual Sampling",
            "Keeps refreshing this page until you turn it off.",
            monitor_state,
            false,
        )?;
        self.paint_setting_row(
            layers,
            row_x,
            first_row_y + row_step,
            row_width,
            "Physical Footprint",
            "Matches the macOS memory pressure number more closely than RSS.",
            &footprint,
            true,
        )?;
        self.paint_setting_row(
            layers,
            row_x,
            first_row_y + row_step * 2.0,
            row_width,
            "Resident Size",
            "Current resident process memory from proc_pid_rusage.",
            &rss,
            true,
        )?;
        self.paint_setting_row(
            layers,
            row_x,
            first_row_y + row_step * 3.0,
            row_width,
            "Peak Physical Footprint",
            "Highest physical footprint reported for this process lifetime.",
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
        self.paint_setting_row(
            layers,
            row_x,
            first_row_y + row_step * 11.0,
            row_width,
            "Input Diagnostics",
            "Manual tracing for long-running typing latency. No key text is stored.",
            input_state,
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
                &ui_font,
                row_x,
                first_row_y + row_step * 15.0,
                error,
                palette.muted_text,
                row_width,
            )?;
        }
        let primary_label = if self.ui.memory_monitoring {
            "Stop Memory Sampling"
        } else {
            "Start Memory Sampling"
        };
        self.draw_button(
            layers,
            x,
            button_y,
            self.button_width_for_label(primary_label, 300.0),
            primary_label,
            SettingsAction::ToggleMemoryMonitoring,
        )?;
        let refresh_x = x + self.button_width_for_label(primary_label, 300.0) + self.ui_px(16.0);
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
        let input_primary_label = if input.enabled {
            "Stop Input Trace"
        } else {
            "Start Input Trace"
        };
        self.draw_button(
            layers,
            x,
            input_button_y,
            self.button_width_for_label(input_primary_label, 250.0),
            input_primary_label,
            SettingsAction::ToggleInputDiagnostics,
        )?;
        let reset_input_x =
            x + self.button_width_for_label(input_primary_label, 250.0) + self.ui_px(16.0);
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
                &ui_font,
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
            &ui_font,
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
        let ui_font = Rc::clone(&self.ui_font);
        let row_width = width.min(self.ui_px(420.0));
        if selected {
            self.draw_rounded_rect(
                layers,
                0,
                x,
                y,
                row_width,
                self.ui_px(NAV_ROW_HEIGHT),
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
                self.ui_px(NAV_ROW_HEIGHT),
                palette.nav_hover_bg,
                self.ui_px(NAV_ROW_RADIUS),
            )?;
        }
        self.draw_text(
            layers,
            &ui_font,
            x + self.ui_px(16.0),
            self.control_text_y(y, self.ui_px(NAV_ROW_HEIGHT)),
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

    fn paint_compatibility(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        max_width: f32,
    ) -> anyhow::Result<()> {
        let palette = self.palette();
        let ui_font = Rc::clone(&self.ui_font);
        let scroll = self.ui.content_scroll.offset;
        let row_step = self.settings_row_step();
        let section_y = self.ui_px(CONTENT_SECTION_Y) - scroll;
        let field_count = self.compatibility_import.fields.len();
        let row_count = 3 + field_count;
        let (card_y, first_row_y) = self.settings_card_geometry(section_y, row_count);
        let card_height = self.settings_card_height(row_count);
        let buttons_y = card_y + card_height + self.settings_section_card_gap();
        self.ui.content_scroll.set_extents(
            self.content_viewport_extent(),
            self.settings_content_extent(
                buttons_y + scroll + self.ui_px(CONTROL_HEIGHT) * 2.0 + 14.0,
            ),
        );
        let thinkterm_path = Self::thinkterm_compatible_config_path();
        let thinkterm_source = if thinkterm_path.exists() {
            thinkterm_path.display().to_string()
        } else {
            let mut args = FluentArgs::new();
            args.set("path", thinkterm_path.display().to_string());
            crate::i18n::tr_args("settings-thinkterm-config-missing", &args)
        };
        let wezterm_source_path = self.selected_wezterm_source_path();
        let wezterm_source = wezterm_source_path
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| crate::i18n::tr("settings-wezterm-source-missing"));
        let thinkterm_status = if thinkterm_path.exists() {
            crate::i18n::tr("settings-import-config-ready")
        } else {
            crate::i18n::tr("settings-import-config-will-create")
        };
        let wezterm_status = if wezterm_source_path.is_some() {
            crate::i18n::tr("common-found")
        } else {
            crate::i18n::tr("common-missing")
        };
        let selected_count = self
            .compatibility_import
            .fields
            .iter()
            .filter(|field| field.lua_value.is_some() && self.import_field_selected(field.id))
            .count();
        let selection_status = if self.compatibility_import.error.is_some() {
            crate::i18n::tr("common-error")
        } else if field_count == 0 {
            crate::i18n::tr("settings-not-loaded")
        } else {
            let mut args = FluentArgs::new();
            args.set("selected", selected_count);
            args.set("total", field_count);
            crate::i18n::tr_args("settings-fields-selected", &args)
        };
        let selection_description = self
            .compatibility_import
            .error
            .clone()
            .unwrap_or_else(|| crate::i18n::tr("settings-import-selection-description"));

        self.draw_text(
            layers,
            &ui_font,
            x,
            section_y,
            &crate::i18n::tr("settings-compatibility-description"),
            palette.secondary_text,
            max_width,
        )?;
        let card_padding = 36.0;
        let row_x = x + card_padding;
        let row_width = max_width - card_padding * 2.0;
        self.paint_group_card(layers, x, card_y, max_width, card_height)?;
        self.paint_setting_row(
            layers,
            row_x,
            first_row_y,
            row_width,
            &crate::i18n::tr("settings-thinkterm-config"),
            &thinkterm_source,
            &thinkterm_status,
            false,
        )?;
        self.paint_setting_row(
            layers,
            row_x,
            first_row_y + row_step,
            row_width,
            &crate::i18n::tr("settings-wezterm-source"),
            &wezterm_source,
            &wezterm_status,
            true,
        )?;
        self.paint_setting_row(
            layers,
            row_x,
            first_row_y + row_step * 2.0,
            row_width,
            &crate::i18n::tr("settings-import-selection"),
            &selection_description,
            &selection_status,
            true,
        )?;

        let mut row_y = first_row_y + row_step * 3.0;
        if !self.compatibility_import.fields.is_empty() {
            let fields = self.compatibility_import.fields.clone();
            for field in fields {
                self.paint_import_field_row(layers, row_x, row_y, row_width, &field, true)?;
                row_y += row_step;
            }
        }
        let load_label = crate::i18n::tr("settings-load-wezterm-source");
        let select_all_label = crate::i18n::tr("common-select-all");
        let clear_label = crate::i18n::tr("common-clear");
        let import_label = crate::i18n::tr("settings-import-selected");
        let open_wezterm_label = crate::i18n::tr("settings-open-wezterm-source");
        let open_thinkterm_label = crate::i18n::tr("settings-open-thinkterm-config");
        self.draw_button(
            layers,
            x,
            buttons_y,
            self.button_width_for_label(&load_label, 290.0),
            &load_label,
            SettingsAction::LoadWezTermSource,
        )?;
        let second_x = x + self.button_width_for_label(&load_label, 290.0) + self.ui_px(16.0);
        self.draw_button(
            layers,
            second_x,
            buttons_y,
            self.button_width_for_label(&select_all_label, 180.0),
            &select_all_label,
            SettingsAction::SelectAllImportFields,
        )?;
        let third_x =
            second_x + self.button_width_for_label(&select_all_label, 180.0) + self.ui_px(16.0);
        self.draw_button(
            layers,
            third_x,
            buttons_y,
            self.button_width_for_label(&clear_label, 150.0),
            &clear_label,
            SettingsAction::ClearImportFields,
        )?;
        let fourth_x =
            third_x + self.button_width_for_label(&clear_label, 150.0) + self.ui_px(16.0);
        self.draw_button(
            layers,
            fourth_x,
            buttons_y,
            self.button_width_for_label(&import_label, 260.0),
            &import_label,
            SettingsAction::ImportSelectedFields,
        )?;
        let open_buttons_y = buttons_y + self.ui_px(CONTROL_HEIGHT) + 14.0;
        self.draw_button(
            layers,
            x,
            open_buttons_y,
            self.button_width_for_label(&open_wezterm_label, 300.0),
            &open_wezterm_label,
            SettingsAction::OpenWezTermConfigFile,
        )?;
        let fifth_x =
            x + self.button_width_for_label(&open_wezterm_label, 300.0) + self.ui_px(16.0);
        self.draw_button(
            layers,
            fifth_x,
            open_buttons_y,
            self.button_width_for_label(&open_thinkterm_label, 300.0),
            &open_thinkterm_label,
            SettingsAction::OpenThinkTermConfigFile,
        )?;

        Ok(())
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
    ) -> anyhow::Result<()> {
        let palette = self.palette();
        let ui_font = Rc::clone(&self.ui_font);
        if draw_top_rule {
            self.paint_separator(layers, x, y - self.ui_px(28.0), width)?;
        }
        let control_width = if width >= 680.0 {
            280.0_f32.min(width * 0.36)
        } else {
            220.0_f32.min(width * 0.44)
        };
        let control_x = x + width - control_width;
        let control_y = y + self.ui_px(4.0);
        let text_width = (control_x - x - 24.0).max(width * 0.45);
        self.draw_text(layers, &ui_font, x, y, label, palette.text, text_width)?;
        self.draw_text(
            layers,
            &ui_font,
            x,
            self.settings_row_description_y(y),
            description,
            palette.secondary_text,
            text_width,
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
        self.draw_text(
            layers,
            &ui_font,
            control_x + self.ui_px(14.0),
            self.control_text_y(control_y, self.ui_px(CONTROL_HEIGHT)),
            value,
            palette.text,
            control_width - self.ui_px(26.0),
        )?;
        Ok(())
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
    ) -> anyhow::Result<()> {
        let palette = self.palette();
        let ui_font = Rc::clone(&self.ui_font);
        if draw_top_rule {
            self.paint_separator(layers, x, y - self.ui_px(28.0), width)?;
        }
        let control_width = if width >= 680.0 {
            280.0_f32.min(width * 0.36)
        } else {
            220.0_f32.min(width * 0.44)
        };
        let control_x = x + width - control_width;
        let control_y = y + self.ui_px(4.0);
        let text_width = (control_x - x - 24.0).max(width * 0.45);
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
        let bg = if pressed {
            palette.control_pressed_bg
        } else if hovered {
            palette.control_hover_bg
        } else {
            palette.control_bg
        };
        let border = if hovered || pressed {
            palette.separator
        } else {
            palette.control_border
        };

        self.draw_text(layers, &ui_font, x, y, label, palette.text, text_width)?;
        self.draw_text(
            layers,
            &ui_font,
            x,
            self.settings_row_description_y(y),
            description,
            palette.secondary_text,
            text_width,
        )?;
        self.draw_rounded_frame(
            layers,
            0,
            control_x,
            control_y,
            control_width,
            self.ui_px(CONTROL_HEIGHT),
            bg,
            border,
            self.ui_px(CONTROL_RADIUS),
        )?;
        self.draw_text(
            layers,
            &ui_font,
            control_x + self.ui_px(14.0),
            self.control_text_y(control_y, self.ui_px(CONTROL_HEIGHT)),
            if enabled { "On" } else { "Off" },
            palette.text,
            control_width - self.ui_px(26.0),
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
    ) -> anyhow::Result<()> {
        let palette = self.palette();
        let ui_font = Rc::clone(&self.ui_font);
        if draw_top_rule {
            self.paint_separator(layers, x, y - self.ui_px(28.0), width)?;
        }
        let control_width = if width >= 680.0 {
            280.0_f32.min(width * 0.36)
        } else {
            220.0_f32.min(width * 0.44)
        };
        let control_x = x + width - control_width;
        let control_y = y + self.ui_px(4.0);
        let text_width = (control_x - x - 24.0).max(width * 0.45);
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
        let bg = if pressed {
            palette.control_pressed_bg
        } else if hovered {
            palette.control_hover_bg
        } else {
            palette.control_bg
        };
        let border = if hovered || pressed {
            palette.separator
        } else {
            palette.control_border
        };

        self.draw_text(layers, &ui_font, x, y, label, palette.text, text_width)?;
        self.draw_text(
            layers,
            &ui_font,
            x,
            self.settings_row_description_y(y),
            description,
            palette.secondary_text,
            text_width,
        )?;
        self.draw_rounded_frame(
            layers,
            0,
            control_x,
            control_y,
            control_width,
            self.ui_px(CONTROL_HEIGHT),
            bg,
            border,
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
        if selected || hovered || pressed {
            let bg = if pressed {
                palette.control_pressed_bg
            } else if selected {
                rgba(10, 132, 255, 0.14)
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

        let checkbox_size = 30.0;
        let checkbox_x = x;
        let checkbox_y = y + self.ui_px(6.0);
        let checkbox_fill = if selected {
            palette.nav_selected_bg
        } else if hovered {
            palette.control_hover_bg
        } else {
            palette.control_bg
        };
        let checkbox_border = if selected || hovered {
            palette.nav_selected_bg
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
            self.draw_rounded_rect(
                layers,
                1,
                checkbox_x + self.ui_px(8.0),
                checkbox_y + self.ui_px(8.0),
                checkbox_size - self.ui_px(16.0),
                checkbox_size - self.ui_px(16.0),
                palette.selected_text,
                self.ui_px(4.0),
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
        let title = format!("{} / {}", field.category, field.label);
        self.draw_text(
            layers,
            &ui_font,
            label_x,
            y,
            &title,
            if enabled {
                palette.text
            } else {
                palette.muted_text
            },
            text_width,
        )?;
        self.draw_text(
            layers,
            &ui_font,
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
            self.ui_px(28.0),
        )
    }

    fn paint_separator(
        &self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
    ) -> anyhow::Result<()> {
        let palette = self.palette();
        self.draw_rounded_rect(layers, 0, x, y, width, 2.0, palette.rule, 1.0)
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
    ) -> anyhow::Result<()> {
        let palette = self.palette();
        let ui_font = Rc::clone(&self.ui_font);
        if draw_top_rule {
            self.paint_separator(layers, x, y - self.ui_px(28.0), width)?;
        }

        let control_width = if width >= 680.0 {
            280.0_f32.min(width * 0.36)
        } else {
            220.0_f32.min(width * 0.44)
        };
        let control_x = x + width - control_width;
        let control_y = y + self.ui_px(4.0);
        let dynamic_icon_size =
            (self.metrics.cell_size.height as f32 + 4.0).clamp(self.ui_px(24.0), self.ui_px(34.0));
        let reset_size = self.ui_px(CONTROL_HEIGHT);
        let reset_gap = 14.0;
        let reset_x = (control_x - reset_gap - reset_size).max(x + width * 0.62);
        let text_width = (reset_x - x - 24.0).max(width * 0.40);

        self.draw_text(layers, &ui_font, x, y, label, palette.text, text_width)?;
        self.draw_text(
            layers,
            &ui_font,
            x,
            self.settings_row_description_y(y),
            description,
            palette.secondary_text,
            text_width,
        )?;

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

        let button_width = 62.0_f32.min(control_width * 0.28);
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

        let value_label = value_label.map(str::to_string).unwrap_or_else(|| {
            if value >= 100.0 && value.fract().abs() < f64::EPSILON {
                format!("{value:.0}")
            } else {
                format!("{value:.1}")
            }
        });
        let value_width = control_width - button_width * 2.0;
        let value_x = control_x + button_width;
        self.draw_text(
            layers,
            &ui_font,
            value_x + self.ui_px(8.0),
            self.control_text_y(control_y, self.ui_px(CONTROL_HEIGHT)),
            &value_label,
            palette.text,
            value_width - self.ui_px(16.0),
        )?;

        Ok(())
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
    ) -> anyhow::Result<()> {
        let palette = self.palette();
        let ui_font = Rc::clone(&self.ui_font);
        if draw_top_rule {
            self.paint_separator(layers, x, y - self.ui_px(28.0), width)?;
        }

        let control_width = if width >= 680.0 {
            280.0_f32.min(width * 0.36)
        } else {
            220.0_f32.min(width * 0.44)
        };
        let control_x = x + width - control_width;
        let control_y = y + self.ui_px(4.0);
        let text_width = (control_x - x - 24.0).max(width * 0.45);
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
        let control_rect = rect(
            control_x,
            control_y,
            control_width,
            self.ui_px(CONTROL_HEIGHT),
        );

        self.ui_context
            .push(control_rect, WidgetKind::TextInput, action);
        self.draw_text(layers, &ui_font, x, y, label, palette.text, text_width)?;
        self.draw_text(
            layers,
            &ui_font,
            x,
            self.settings_row_description_y(y),
            description,
            palette.secondary_text,
            text_width,
        )?;
        self.draw_rounded_frame(
            layers,
            0,
            control_x,
            control_y,
            control_width,
            self.ui_px(CONTROL_HEIGHT),
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

    fn paint_theme_mode_row(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
        description: &str,
        draw_top_rule: bool,
    ) -> anyhow::Result<()> {
        let palette = self.palette();
        let ui_font = Rc::clone(&self.ui_font);
        if draw_top_rule {
            self.paint_separator(layers, x, y - self.ui_px(28.0), width)?;
        }

        let (control_x, control_y, control_width) = self.theme_mode_control_geometry(x, y, width);
        let text_width = (control_x - x - 24.0).max(width * 0.45);
        let action = SettingsAction::ToggleThemeModeMenu;
        let control_rect = rect(
            control_x,
            control_y,
            control_width,
            self.ui_px(CONTROL_HEIGHT),
        );
        let open = self.ui.open_dropdown == Some(SettingsDropdown::ThemeMode);
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
            "Theme Mode",
            palette.text,
            text_width,
        )?;
        self.draw_text(
            layers,
            &ui_font,
            x,
            self.settings_row_description_y(y),
            description,
            palette.secondary_text,
            text_width,
        )?;
        self.draw_rounded_frame(
            layers,
            0,
            control_rect.origin.x,
            control_rect.origin.y,
            control_rect.size.width,
            control_rect.size.height,
            bg,
            border,
            self.ui_px(CONTROL_RADIUS),
        )?;
        self.draw_text(
            layers,
            &ui_font,
            control_x + self.ui_px(16.0),
            self.control_text_y(control_y, self.ui_px(CONTROL_HEIGHT)),
            &localized_theme_mode_label(self.native_settings.appearance.theme_mode),
            palette.text,
            control_width - self.ui_px(60.0),
        )?;
        self.draw_svg_icon(
            layers,
            SvgIcon::ChevronDown,
            control_x + control_width - self.ui_px(38.0),
            control_y + (self.ui_px(CONTROL_HEIGHT) - 22.0) / 2.0,
            self.ui_px(22.0),
            palette.secondary_text,
        )?;

        Ok(())
    }

    fn paint_language_row(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
        draw_top_rule: bool,
    ) -> anyhow::Result<()> {
        let palette = self.palette();
        let ui_font = Rc::clone(&self.ui_font);
        if draw_top_rule {
            self.paint_separator(layers, x, y - self.ui_px(28.0), width)?;
        }

        let (control_x, control_y, control_width) = self.dropdown_control_geometry(x, y, width);
        let text_width = (control_x - x - 24.0).max(width * 0.45);
        let action = SettingsAction::ToggleLanguageMenu;
        let control_rect = rect(
            control_x,
            control_y,
            control_width,
            self.ui_px(CONTROL_HEIGHT),
        );
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
        self.draw_text(
            layers,
            &ui_font,
            x,
            self.settings_row_description_y(y),
            &crate::i18n::tr("settings-language-description"),
            palette.secondary_text,
            text_width,
        )?;
        self.draw_rounded_frame(
            layers,
            0,
            control_rect.origin.x,
            control_rect.origin.y,
            control_rect.size.width,
            control_rect.size.height,
            bg,
            border,
            self.ui_px(CONTROL_RADIUS),
        )?;
        self.draw_text(
            layers,
            &ui_font,
            control_x + self.ui_px(16.0),
            self.control_text_y(control_y, self.ui_px(CONTROL_HEIGHT)),
            &crate::i18n::configured_language_label(&self.native_settings),
            palette.text,
            control_width - self.ui_px(60.0),
        )?;
        self.draw_svg_icon(
            layers,
            SvgIcon::ChevronDown,
            control_x + control_width - self.ui_px(38.0),
            control_y + (self.ui_px(CONTROL_HEIGHT) - 22.0) / 2.0,
            self.ui_px(22.0),
            palette.secondary_text,
        )?;
        Ok(())
    }

    fn paint_app_icon_row(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
        description: &str,
        note: &str,
        draw_top_rule: bool,
    ) -> anyhow::Result<()> {
        let palette = self.palette();
        let ui_font = Rc::clone(&self.ui_font);
        if draw_top_rule {
            self.paint_separator(layers, x, y - self.ui_px(28.0), width)?;
        }

        let (control_x, control_y, control_width) = self.dropdown_control_geometry(x, y, width);
        let text_width = (control_x - x - 24.0).max(width * 0.45);
        let action = SettingsAction::ToggleAppIconMenu;
        let control_rect = rect(
            control_x,
            control_y,
            control_width,
            self.ui_px(CONTROL_HEIGHT),
        );
        let open = self.ui.open_dropdown == Some(SettingsDropdown::AppIcon);
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
            &crate::i18n::tr("settings-app-icon"),
            palette.text,
            text_width,
        )?;
        self.draw_text(
            layers,
            &ui_font,
            x,
            self.settings_row_description_y(y),
            description,
            palette.secondary_text,
            text_width,
        )?;
        self.draw_text(
            layers,
            &ui_font,
            x,
            self.settings_row_description_y(y)
                + self.metrics.cell_size.height as f32
                + self.ui_px(4.0),
            note,
            palette.muted_text,
            text_width,
        )?;
        self.draw_rounded_frame(
            layers,
            0,
            control_rect.origin.x,
            control_rect.origin.y,
            control_rect.size.width,
            control_rect.size.height,
            bg,
            border,
            self.ui_px(CONTROL_RADIUS),
        )?;
        self.draw_text(
            layers,
            &ui_font,
            control_x + self.ui_px(16.0),
            self.control_text_y(control_y, self.ui_px(CONTROL_HEIGHT)),
            &localized_app_icon_label(self.native_settings.appearance.app_icon),
            palette.text,
            control_width - self.ui_px(60.0),
        )?;
        self.draw_svg_icon(
            layers,
            SvgIcon::ChevronDown,
            control_x + control_width - self.ui_px(38.0),
            control_y + (self.ui_px(CONTROL_HEIGHT) - 22.0) / 2.0,
            self.ui_px(22.0),
            palette.secondary_text,
        )?;

        Ok(())
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
    ) -> anyhow::Result<()> {
        let palette = self.palette();
        let ui_font = Rc::clone(&self.ui_font);
        if draw_top_rule {
            self.paint_separator(layers, x, y - self.ui_px(28.0), width)?;
        }

        let (control_x, control_y, control_width) = self.dropdown_control_geometry(x, y, width);
        let text_width = (control_x - x - 24.0).max(width * 0.45);
        let action = SettingsAction::ToggleDefaultShellMenu;
        let control_rect = rect(
            control_x,
            control_y,
            control_width,
            self.ui_px(CONTROL_HEIGHT),
        );
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
        self.draw_text(
            layers,
            &ui_font,
            x,
            self.settings_row_description_y(y),
            &description,
            palette.secondary_text,
            text_width,
        )?;
        self.draw_rounded_frame(
            layers,
            0,
            control_rect.origin.x,
            control_rect.origin.y,
            control_rect.size.width,
            control_rect.size.height,
            bg,
            border,
            self.ui_px(CONTROL_RADIUS),
        )?;
        let current = self.current_default_shell_label();
        self.draw_text(
            layers,
            &ui_font,
            control_x + self.ui_px(16.0),
            self.control_text_y(control_y, self.ui_px(CONTROL_HEIGHT)),
            &current,
            palette.text,
            control_width - self.ui_px(60.0),
        )?;
        self.draw_svg_icon(
            layers,
            SvgIcon::ChevronDown,
            control_x + control_width - self.ui_px(38.0),
            control_y + (self.ui_px(CONTROL_HEIGHT) - 22.0) / 2.0,
            self.ui_px(22.0),
            palette.secondary_text,
        )?;

        Ok(())
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
    ) -> anyhow::Result<()> {
        let palette = self.palette();
        let ui_font = Rc::clone(&self.ui_font);
        if draw_top_rule {
            self.paint_separator(layers, x, y - self.ui_px(28.0), width)?;
        }

        let (control_x, control_y, control_width) = self.dropdown_control_geometry(x, y, width);
        let text_width = (control_x - x - 24.0).max(width * 0.45);
        let action = SettingsAction::ToggleMainRendererMenu;
        let control_rect = rect(
            control_x,
            control_y,
            control_width,
            self.ui_px(CONTROL_HEIGHT),
        );
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
        self.draw_text(
            layers,
            &ui_font,
            x,
            self.settings_row_description_y(y),
            &crate::i18n::tr("settings-main-renderer-description"),
            palette.secondary_text,
            text_width,
        )?;
        self.draw_rounded_frame(
            layers,
            0,
            control_rect.origin.x,
            control_rect.origin.y,
            control_rect.size.width,
            control_rect.size.height,
            bg,
            border,
            self.ui_px(CONTROL_RADIUS),
        )?;
        self.draw_text(
            layers,
            &ui_font,
            control_x + self.ui_px(16.0),
            self.control_text_y(control_y, self.ui_px(CONTROL_HEIGHT)),
            self.current_main_renderer().label(),
            palette.text,
            control_width - self.ui_px(60.0),
        )?;
        self.draw_svg_icon(
            layers,
            SvgIcon::ChevronDown,
            control_x + control_width - self.ui_px(38.0),
            control_y + (self.ui_px(CONTROL_HEIGHT) - 22.0) / 2.0,
            self.ui_px(22.0),
            palette.secondary_text,
        )?;

        Ok(())
    }

    fn paint_restart_row(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
        draw_top_rule: bool,
    ) -> anyhow::Result<()> {
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
        self.draw_text(
            layers,
            &ui_font,
            x,
            self.settings_row_description_y(y),
            &crate::i18n::tr_args("settings-restart-status", &status_args),
            palette.secondary_text,
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

        Ok(())
    }

    fn theme_mode_control_geometry(&self, x: f32, y: f32, width: f32) -> (f32, f32, f32) {
        self.dropdown_control_geometry(x, y, width)
    }

    fn dropdown_control_geometry(&self, x: f32, y: f32, width: f32) -> (f32, f32, f32) {
        let control_width = if width >= 680.0 {
            280.0_f32.min(width * 0.36)
        } else {
            220.0_f32.min(width * 0.44)
        };
        (
            x + width - control_width,
            y + self.ui_px(4.0),
            control_width,
        )
    }

    fn paint_open_dropdown_overlay(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        max_width: f32,
    ) -> anyhow::Result<()> {
        let Some(dropdown) = self.ui.open_dropdown else {
            return Ok(());
        };

        let scroll = self.ui.content_scroll.offset;
        let card_padding = 36.0;
        let section_y = self.ui_px(CONTENT_SECTION_Y) - scroll;
        let row_count = match self.selected {
            SettingsSection::General => 8,
            SettingsSection::Appearance => 4,
            SettingsSection::Terminal => 9,
            _ => 4,
        };
        let (_, first_row_y) = self.settings_card_geometry(section_y, row_count);
        // NOTE: each multiplier below is the row's zero-based index inside
        // its section painter, copied by hand. Nothing checks the two
        // against each other, so moving a row means editing both places.
        let (row_x, row_y, row_width) = match self.selected {
            SettingsSection::Appearance => {
                let row_y = match dropdown {
                    SettingsDropdown::Language => return Ok(()),
                    SettingsDropdown::ThemeMode => first_row_y,
                    SettingsDropdown::AppIcon => first_row_y + self.settings_row_step(),
                    SettingsDropdown::MainRenderer | SettingsDropdown::DefaultShell => {
                        return Ok(())
                    }
                };
                (x + card_padding, row_y, max_width - card_padding * 2.0)
            }
            SettingsSection::General => {
                let row_y = match dropdown {
                    SettingsDropdown::Language => first_row_y,
                    SettingsDropdown::ThemeMode => first_row_y + self.settings_row_step() * 3.0,
                    SettingsDropdown::MainRenderer => first_row_y + self.settings_row_step() * 4.0,
                    SettingsDropdown::AppIcon | SettingsDropdown::DefaultShell => return Ok(()),
                };
                (x + card_padding, row_y, max_width - card_padding * 2.0)
            }
            SettingsSection::Terminal => {
                let row_y = match dropdown {
                    // First row of the section: the menu neither scrolls
                    // nor flips upward, so it needs the room below it.
                    SettingsDropdown::DefaultShell => first_row_y,
                    SettingsDropdown::Language
                    | SettingsDropdown::ThemeMode
                    | SettingsDropdown::AppIcon
                    | SettingsDropdown::MainRenderer => return Ok(()),
                };
                (x + card_padding, row_y, max_width - card_padding * 2.0)
            }
            _ => return Ok(()),
        };
        let (control_x, control_y, control_width) =
            self.dropdown_control_geometry(row_x, row_y, row_width);
        match dropdown {
            SettingsDropdown::Language => self.paint_language_menu(
                layers,
                control_x,
                control_y + self.ui_px(CONTROL_HEIGHT) + 8.0,
                control_width,
            ),
            SettingsDropdown::ThemeMode => self.paint_theme_mode_menu(
                layers,
                control_x,
                control_y + self.ui_px(CONTROL_HEIGHT) + 8.0,
                control_width,
            ),
            SettingsDropdown::AppIcon => self.paint_app_icon_menu(
                layers,
                control_x,
                control_y + self.ui_px(CONTROL_HEIGHT) + 8.0,
                control_width,
            ),
            SettingsDropdown::MainRenderer => self.paint_main_renderer_menu(
                layers,
                control_x,
                control_y + self.ui_px(CONTROL_HEIGHT) + 8.0,
                control_width,
            ),
            SettingsDropdown::DefaultShell => self.paint_default_shell_menu(
                layers,
                control_x,
                control_y + self.ui_px(CONTROL_HEIGHT) + 8.0,
                control_width,
            ),
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

    fn paint_theme_mode_menu(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
    ) -> anyhow::Result<()> {
        let options = [
            (
                localized_theme_mode_label(NativeThemeMode::System),
                SettingsAction::SetThemeMode(NativeThemeMode::System),
                self.native_settings.appearance.theme_mode == NativeThemeMode::System,
            ),
            (
                localized_theme_mode_label(NativeThemeMode::Light),
                SettingsAction::SetThemeMode(NativeThemeMode::Light),
                self.native_settings.appearance.theme_mode == NativeThemeMode::Light,
            ),
            (
                localized_theme_mode_label(NativeThemeMode::Dark),
                SettingsAction::SetThemeMode(NativeThemeMode::Dark),
                self.native_settings.appearance.theme_mode == NativeThemeMode::Dark,
            ),
        ];
        self.paint_dropdown_menu(layers, x, y, width, &options)
    }

    fn paint_app_icon_menu(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
    ) -> anyhow::Result<()> {
        let options = [
            (
                localized_app_icon_label(NativeAppIcon::Simple),
                SettingsAction::SetAppIcon(NativeAppIcon::Simple),
                self.native_settings.appearance.app_icon == NativeAppIcon::Simple,
            ),
            (
                localized_app_icon_label(NativeAppIcon::Classic),
                SettingsAction::SetAppIcon(NativeAppIcon::Classic),
                self.native_settings.appearance.app_icon == NativeAppIcon::Classic,
            ),
        ];
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
        let palette = self.palette();
        let ui_font = Rc::clone(&self.ui_font);
        let row_height = 46.0;
        let row_gap = 6.0;
        let menu_padding = 8.0;
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
            1,
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
                    1,
                    row_rect.origin.x,
                    row_rect.origin.y,
                    row_rect.size.width,
                    row_rect.size.height,
                    row_bg,
                    self.ui_px(9.0),
                )?;
            }
            self.draw_text(
                layers,
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

    fn paint_text_input(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        spec: TextInputSpec<'_, SettingsAction>,
    ) -> anyhow::Result<()> {
        let palette = self.palette();
        self.ui_context
            .push(spec.rect, WidgetKind::TextInput, spec.action);
        let border = if spec.focused {
            palette.nav_selected_bg
        } else if self.ui.interaction.hovered == Some(spec.action) {
            palette.separator
        } else {
            palette.search_border
        };
        self.draw_rounded_frame(
            layers,
            0,
            spec.rect.origin.x,
            spec.rect.origin.y,
            spec.rect.size.width,
            spec.rect.size.height,
            palette.search_bg,
            border,
            self.ui.tokens.control_radius,
        )?;
        if spec.focused {
            self.draw_rounded_rect(
                layers,
                0,
                spec.rect.origin.x,
                spec.rect.origin.y,
                spec.rect.size.width,
                2.0,
                palette.nav_selected_bg,
                1.0,
            )?;
        }

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
                    1,
                    text_left + start_x - self.ui_px(4.0),
                    spec.rect.origin.y + self.ui_px(6.0),
                    (end_x - start_x) + self.ui_px(8.0),
                    spec.rect.size.height - self.ui_px(12.0),
                    palette.nav_selected_bg.mul_alpha(0.56),
                    self.ui.tokens.control_radius - self.ui_px(4.0),
                )?;
            }
        }
        self.draw_text(
            layers,
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
                1,
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
        let ui_palette = UiPalette::for_appearance(self.effective_appearance());
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
        let width = width.max(self.button_width_for_label(label, 0.0));
        let button = ButtonSpec {
            label,
            action,
            rect: rect(x, y, width, self.ui_px(CONTROL_HEIGHT)),
            state: if self.ui.interaction.pressed == Some(action) {
                ControlState::Pressed
            } else if self.ui.interaction.hovered == Some(action) {
                ControlState::Hovered
            } else {
                ControlState::Normal
            },
            kind: WidgetKind::Button,
        };
        self.ui_context
            .push(button.rect, button.kind, button.action);

        let palette = self.palette();
        let ui_palette = UiPalette::for_appearance(self.effective_appearance());
        let (background, border) = button.state.colors(ui_palette);
        self.draw_rounded_frame(
            layers,
            0,
            x,
            y,
            width,
            self.ui_px(CONTROL_HEIGHT),
            background,
            border,
            self.ui_px(CONTROL_RADIUS),
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
            palette.text,
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
        self.draw_rounded_rect(layers, layer_num, x, y, width, height, border, radius)?;
        self.draw_rounded_rect(
            layers,
            layer_num,
            x + 1.0,
            y + 1.0,
            width - 2.0,
            height - 2.0,
            fill,
            (radius - 1.0).max(0.0),
        )?;
        Ok(())
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
        if width <= 0.0 || height <= 0.0 {
            return Ok(());
        }

        let radius = radius.min(width / 2.0).min(height / 2.0).round().max(0.0);
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
            if let Some(texture) = glyph.texture.as_ref() {
                let glyph_x = (pos_x + (glyph.x_offset + glyph.bearing_x).get() as f32).round();
                let glyph_y =
                    (y - (glyph.y_offset + glyph.bearing_y).get() as f32 + baseline).round();
                let width = texture.coords.size.width as f32 * glyph.scale as f32;
                let height = texture.coords.size.height as f32 * glyph.scale as f32;

                if glyph_x + width > right_edge {
                    break;
                }

                let mut quad = layers.allocate(1)?;
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
            pos_x += glyph.x_advance.get() as f32;
            if pos_x > right_edge {
                break;
            }
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

        format!("{}{}", text[..end].trim_end(), ellipsis)
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
            args.set("path", path.display().to_string());
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
            ImportFieldId::InactivePaneHsb => Some(config.inactive_pane_hsb.to_dynamic()),
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

    fn effective_color_scheme_label(config: &config::ConfigHandle) -> &str {
        if let Some(name) = config.color_scheme.as_deref() {
            name
        } else if config.colors.is_some() {
            "Custom inline colors"
        } else {
            "Default palette"
        }
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
        let args = std::env::args_os().skip(1).collect::<Vec<_>>();
        let mut command = Command::new(exe);
        command.args(args);
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
