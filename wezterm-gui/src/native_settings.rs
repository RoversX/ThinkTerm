use config::ConfigHandle;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use window::{Appearance, Connection, ConnectionOps, WindowOps};

// One point is one logical pixel on macOS but 4/3 px at 96dpi, so the
// non-mac size is 0.75x for the same visual size (14px UI text).
pub(crate) const DEFAULT_SETTINGS_FONT_SIZE: f64 = if cfg!(target_os = "macos") {
    14.0
} else {
    10.5
};
pub(crate) const DEFAULT_SETTINGS_FONT_WEIGHT: u16 =
    if cfg!(target_os = "macos") { 600 } else { 500 };
pub(crate) const DEFAULT_HOME_FONT_SIZE: f64 = if cfg!(target_os = "macos") {
    15.0
} else {
    11.25
};
pub(crate) const DEFAULT_SIDEBAR_FONT_SIZE: f64 = if cfg!(target_os = "macos") {
    15.0
} else {
    11.25
};
pub(crate) const DEFAULT_TAB_FONT_SIZE: f64 = if cfg!(target_os = "macos") {
    14.0
} else {
    10.5
};
pub(crate) const DEFAULT_PANE_HEADER_FONT_SIZE: f64 = if cfg!(target_os = "macos") {
    14.0
} else {
    10.5
};
pub(crate) const DEFAULT_BOTTOM_QUOTE_INTERVAL_MINUTES: u32 = 60;
pub(crate) const DEFAULT_BOTTOM_QUOTE_FONT_SIZE: f64 =
    if cfg!(target_os = "macos") { 10.0 } else { 7.5 };
pub(crate) const ONBOARDING_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum NativeThemeMode {
    System,
    Light,
    Dark,
    /// Take the interface's colours from the terminal's own colour scheme.
    /// Light or dark is then the scheme's business, not a separate choice.
    #[serde(rename = "follow_terminal")]
    FollowTerminal,
}

impl Default for NativeThemeMode {
    fn default() -> Self {
        // Dark rather than Follow System: a terminal spends its life next to
        // other terminals, and following a light desktop theme is the one
        // default nobody keeps. Existing installs are unaffected — their
        // settings.json already records an explicit theme_mode.
        Self::Dark
    }
}

impl NativeThemeMode {
    /// Light or dark, for everything that needs one of the two.
    ///
    /// `FollowTerminal` answers `system` here, and under that mode `system`
    /// is not the desktop: [`apply_preferred_appearance`] pushes the scheme's
    /// own side onto the connection, so `system_appearance` -- and therefore
    /// this -- reports the side the scheme is on. That is the point. Reading
    /// the scheme a second time here would be the same answer arrived at
    /// twice, and would be wrong in the window between a scheme changing and
    /// that push landing. The interface's own colours go through
    /// [`chrome_palette`], which reads the scheme directly.
    pub(crate) fn effective_appearance(self, system: Appearance) -> Appearance {
        match self {
            Self::System | Self::FollowTerminal => system,
            Self::Light => Appearance::Light,
            Self::Dark => Appearance::Dark,
        }
    }

    pub(crate) fn preferred_app_appearance(self) -> Option<Appearance> {
        match self {
            Self::System | Self::FollowTerminal => None,
            Self::Light => Some(Appearance::Light),
            Self::Dark => Some(Appearance::Dark),
        }
    }

    /// Every mode, in the order the settings window offers them.
    pub(crate) const ALL: [Self; 4] = [
        Self::System,
        Self::Light,
        Self::Dark,
        Self::FollowTerminal,
    ];
}

/// Read a theme mode, falling back to the default rather than failing.
///
/// A value this build does not know -- a newer one wrote it -- would
/// otherwise fail the whole file, and `load_from_disk` answers a parse failure
/// with `Default::default()`. The next save then writes those defaults back
/// over the user's language, fonts and everything else. One unreadable field
/// must not cost the file.
fn theme_mode_or_default<'de, D>(deserializer: D) -> Result<NativeThemeMode, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = String::deserialize(deserializer)?;
    Ok(match raw.as_str() {
        "system" => NativeThemeMode::System,
        "light" => NativeThemeMode::Light,
        "dark" => NativeThemeMode::Dark,
        "follow_terminal" => NativeThemeMode::FollowTerminal,
        other => {
            log::warn!("unknown theme_mode {other:?} in the settings; using the default");
            NativeThemeMode::default()
        }
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum NativeAppIcon {
    Simple,
    #[serde(alias = "default")]
    Classic,
}

impl Default for NativeAppIcon {
    fn default() -> Self {
        Self::Simple
    }
}

impl NativeAppIcon {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Simple => "Simple",
            Self::Classic => "Classic",
        }
    }

    fn file_name(self) -> &'static str {
        match self {
            Self::Simple => "ThinkTerm_simple.icns",
            Self::Classic => "ThinkTerm.icns",
        }
    }

    pub(crate) const ALL: [Self; 2] = [Self::Simple, Self::Classic];

    /// A 128px copy of the icon, for Settings to show as a choice: the
    /// full-size art is megabytes of PNG to decode for a 44pt picture.
    pub(crate) fn thumbnail(self) -> &'static [u8] {
        match self {
            Self::Simple => include_bytes!("../../assets/icon/ThinkTerm_simple_128.png"),
            Self::Classic => include_bytes!("../../assets/icon/ThinkTerm_128.png"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum NativeRendererBackend {
    OpenGL,
    WebGpu,
}

impl NativeRendererBackend {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::OpenGL => "OpenGL",
            Self::WebGpu => "WebGpu",
        }
    }

    pub(crate) fn from_front_end(front_end: config::FrontEndSelection) -> Self {
        match front_end {
            config::FrontEndSelection::WebGpu => Self::WebGpu,
            config::FrontEndSelection::OpenGL | config::FrontEndSelection::Software => Self::OpenGL,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum NativeLanguagePreference {
    System,
    English,
    Chinese,
    Japanese,
}

impl Default for NativeLanguagePreference {
    fn default() -> Self {
        Self::System
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum NativeBottomQuoteMode {
    Timed,
    PseudoRandom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum NativeRemotePaneResizeMode {
    Auto,
    Live,
    OnRelease,
}

impl Default for NativeRemotePaneResizeMode {
    fn default() -> Self {
        Self::Auto
    }
}

impl NativeRemotePaneResizeMode {
    /// Every mode, in the order the settings window offers them.
    pub(crate) const ALL: [Self; 3] = [Self::Auto, Self::Live, Self::OnRelease];
}

/// How the terminal scrolls its scrollback under a wheel, trackpad or
/// finger: a whole row at a time, or by the exact distance travelled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum NativeScrollMode {
    Stepped,
    Smooth,
}

impl Default for NativeScrollMode {
    fn default() -> Self {
        Self::Smooth
    }
}

impl NativeScrollMode {
    /// Every mode, in the order the settings window offers them.
    pub(crate) const ALL: [Self; 2] = [Self::Smooth, Self::Stepped];
}

/// The floor the terminal holds text to against whatever it is drawn on.
///
/// A program that paints its own background is self-consistent and is never
/// touched by this: the check is per cell, against that cell's real
/// background. What it catches is the half-and-half case -- an application
/// that leaves the background to the terminal but picks its foregrounds for
/// a dark one. btop on a light colour scheme is the example: it hands the
/// background back and then writes in near-white.
///
/// The ratios are WCAG 2.0: 3:1 is the floor for large text, 4.5:1 for body
/// text (AA), 7:1 is AAA.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum NativeTextContrast {
    /// Leave every colour as the application asked for it -- and leave the
    /// Lua `text_min_contrast_ratio` in charge, the way `font_size: None`
    /// leaves the configured font size in charge.
    Off,
    Ratio3,
    Ratio45,
    Ratio7,
}

impl Default for NativeTextContrast {
    fn default() -> Self {
        // Raising contrast also flattens what an application dimmed on
        // purpose -- disabled entries, the unfilled half of a meter, comment
        // colouring. That is a trade the user has to choose, not inherit.
        Self::Off
    }
}

impl NativeTextContrast {
    /// Every choice, in the order the settings window offers them.
    pub(crate) const ALL: [Self; 4] = [Self::Off, Self::Ratio3, Self::Ratio45, Self::Ratio7];

    pub(crate) fn ratio(self) -> Option<f32> {
        match self {
            Self::Off => None,
            Self::Ratio3 => Some(3.0),
            Self::Ratio45 => Some(4.5),
            Self::Ratio7 => Some(7.0),
        }
    }
}

/// A value this build does not know about must not cost the user the rest of
/// their settings, so an unreadable choice reads as the default rather than
/// failing the whole file. Same reasoning as `theme_mode_or_default`.
fn text_contrast_or_default<'de, D>(deserializer: D) -> Result<NativeTextContrast, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = String::deserialize(deserializer)?;
    Ok(match raw.as_str() {
        "off" => NativeTextContrast::Off,
        "ratio3" => NativeTextContrast::Ratio3,
        "ratio45" => NativeTextContrast::Ratio45,
        "ratio7" => NativeTextContrast::Ratio7,
        other => {
            log::warn!("unknown text_contrast {other:?} in the settings; using the default");
            NativeTextContrast::default()
        }
    })
}

impl Default for NativeBottomQuoteMode {
    fn default() -> Self {
        Self::Timed
    }
}

impl NativeBottomQuoteMode {
    /// Every mode, in the order the settings window offers them.
    pub(crate) const ALL: [Self; 2] = [Self::Timed, Self::PseudoRandom];
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub(crate) struct NativeAppearanceSettings {
    #[serde(deserialize_with = "theme_mode_or_default")]
    pub(crate) theme_mode: NativeThemeMode,
    pub(crate) app_icon: NativeAppIcon,
    /// Color scheme picked in the command palette; overrides the config's
    /// `color_scheme` for every window. `None` follows the configuration.
    pub(crate) color_scheme: Option<String>,
    /// The window's opacity in percent, over the configuration's
    /// `window_background_opacity`. `None` follows the configuration.
    #[serde(deserialize_with = "window_opacity_or_default")]
    pub(crate) window_opacity: Option<u8>,
}

/// The window opacity, whatever shape a hand edit gave it. One field that
/// fails to parse loses the whole file to the defaults, and the next save
/// writes them over everything, so: a fraction such as 0.8 is read as 80, a
/// percentage out of range is brought into it, and anything else is no
/// setting at all.
fn window_opacity_or_default<'de, D>(deserializer: D) -> Result<Option<u8>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = serde_json::Value::deserialize(deserializer)?;
    Ok(match raw.as_f64() {
        Some(number) if number.is_finite() => {
            let percent = if number > 0.0 && number <= 1.0 {
                number * 100.0
            } else {
                number
            };
            Some(
                percent
                    .round()
                    .clamp(f64::from(WINDOW_OPACITY_LEAST), 100.0) as u8,
            )
        }
        _ => {
            if !raw.is_null() {
                log::warn!(
                    "unknown window_opacity {raw} in the settings; following the configuration"
                );
            }
            None
        }
    })
}

/// The least window opacity Settings offers, in percent, and the step of its
/// slider. Below the least the terminal's text is hard to read.
pub(crate) const WINDOW_OPACITY_LEAST: u8 = 30;
pub(crate) const WINDOW_OPACITY_STEP: u8 = 5;

/// Whether Settings can make the window see-through here, with what is
/// behind it blurred. Linux has no blur most desktops offer, so the slider
/// is not offered there; the configuration still is.
pub(crate) fn window_opacity_supported() -> bool {
    cfg!(any(target_os = "macos", windows))
}

/// The blur behind a window Settings made see-through, unless the
/// configuration names one: see-through alone shows the desktop sharp behind
/// the text, and what the setting is for is the frosted kind.
const WINDOW_BLUR_RADIUS: i64 = 20;

/// The configuration keys the window-opacity setting overrides, and with
/// what. `None` removes a key, so that the configuration decides again.
pub(crate) fn window_opacity_overrides(
    settings: &ThinkTermNativeSettings,
) -> [(&'static str, Option<wezterm_dynamic::Value>); 2] {
    use wezterm_dynamic::ToDynamic;
    let changes = window_opacity_changes(settings.appearance.window_opacity);
    let [opacity, blur] = WINDOW_OPACITY_KEYS;
    [
        (opacity, changes.opacity.map(|opacity| opacity.to_dynamic())),
        (blur, changes.blur.map(|blur| blur.to_dynamic())),
    ]
}

/// The configuration keys Settings' window opacity overrides in a window.
pub(crate) const WINDOW_OPACITY_KEYS: [&str; 2] =
    ["window_background_opacity", "macos_window_background_blur"];

/// What a window opacity of `percent` changes in a window's configuration:
/// the opacity, and while it is see-through on macOS the blur the
/// configuration file does not ask for itself. `None` changes nothing.
///
/// Windows is given no backdrop: with this window frame any
/// `win32_system_backdrop` turns a see-through window opaque (see
/// wezterm#6265), so that stays the configuration's to choose.
struct WindowOpacityChanges {
    opacity: Option<f64>,
    blur: Option<i64>,
}

fn window_opacity_changes(percent: Option<u8>) -> WindowOpacityChanges {
    let opacity = percent
        .filter(|_| window_opacity_supported())
        .map(|percent| f64::from(percent.clamp(WINDOW_OPACITY_LEAST, 100)) / 100.0);
    let see_through = opacity.is_some_and(|opacity| opacity < 1.0);
    let config = config::configuration();
    WindowOpacityChanges {
        opacity,
        blur: (see_through
            && cfg!(target_os = "macos")
            && config.macos_window_background_blur == 0)
            .then_some(WINDOW_BLUR_RADIUS),
    }
}

/// What `window_opacity_overrides` for a window opacity of `percent` leaves
/// in a window's configuration -- its opacity and blur -- without the reload
/// applying them takes. What the slider previews while dragged.
pub(crate) fn window_opacity_preview(percent: u8) -> (f32, i64) {
    let changes = window_opacity_changes(Some(percent));
    let file = config::configuration();
    (
        changes
            .opacity
            .map_or(file.window_background_opacity, |opacity| opacity as f32),
        changes.blur.unwrap_or(file.macos_window_background_blur),
    )
}

/// Every configuration key Settings overrides in a window -- the colour
/// scheme, the window's opacity and what goes with it -- and with what.
/// `None` removes a key. New windows seed their overrides from this and open
/// ones are brought to it, so both always agree.
pub(crate) fn settings_config_overrides(
    settings: &ThinkTermNativeSettings,
    config: &ConfigHandle,
) -> Vec<(&'static str, Option<wezterm_dynamic::Value>)> {
    use wezterm_dynamic::ToDynamic;
    let scheme = effective_color_scheme(settings, config);
    let mut overrides = vec![("color_scheme", scheme.map(|scheme| scheme.to_dynamic()))];
    overrides.extend(window_opacity_overrides(settings));
    overrides
}

/// Bring every window to the window opacity `settings` asks for: all a
/// change of it needs of `apply_to_app`. A window still showing a preview
/// goes back to what it loaded, since applying reloads only a window whose
/// overrides changed.
pub(crate) fn apply_window_opacity_to_app(settings: &ThinkTermNativeSettings) {
    let overrides = window_opacity_overrides(settings).to_vec();
    if let Some(front_end) = crate::frontend::try_front_end() {
        for gui_window in front_end.gui_windows() {
            let overrides = overrides.clone();
            gui_window
                .window
                .notify(crate::termwindow::TermWindowNotif::Apply(Box::new(
                    move |term_window| {
                        term_window.apply_settings_overrides(&overrides);
                        term_window.preview_window_opacity(None);
                    },
                )));
        }
    }
}

/// Show a window opacity of `percent` in every window, without saving it:
/// the Settings slider calls this while it is dragged. `None` ends the
/// preview, each window going back to its configuration as loaded.
pub(crate) fn preview_window_opacity(percent: Option<u8>) {
    if let Some(front_end) = crate::frontend::try_front_end() {
        for gui_window in front_end.gui_windows() {
            gui_window
                .window
                .notify(crate::termwindow::TermWindowNotif::Apply(Box::new(
                    move |term_window| {
                        term_window.preview_window_opacity(percent);
                    },
                )));
        }
    }
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub(crate) struct NativeTerminalSettings {
    /// The thin auto-hiding scrollbar drawn over each pane's right edge.
    /// Off means no indicator at all; the Lua `enable_scroll_bar` gutter is
    /// a separate, older thing and wins when it is on.
    #[serde(default = "default_true")]
    pub(crate) overlay_scrollbar: bool,
    pub(crate) font_size: Option<f64>,
    pub(crate) font_family: Option<String>,
    pub(crate) remote_pane_resize_mode: NativeRemotePaneResizeMode,
    pub(crate) scroll_mode: NativeScrollMode,
    /// See [`NativeTextContrast`]. `Off` defers to the Lua
    /// `text_min_contrast_ratio`, which is itself off unless set.
    #[serde(deserialize_with = "text_contrast_or_default")]
    pub(crate) text_contrast: NativeTextContrast,
    pub(crate) bottom_quote_enabled: bool,
    pub(crate) bottom_quote_mode: NativeBottomQuoteMode,
    pub(crate) bottom_quote_interval_minutes: Option<u32>,
    pub(crate) bottom_quote_font_size: Option<f64>,
    /// The program a new pane runs, as argv. `None` means the platform
    /// default (`$SHELL` / `%ComSpec%`).
    pub(crate) default_shell: Option<Vec<String>>,
}

impl Default for NativeTerminalSettings {
    fn default() -> Self {
        Self {
            overlay_scrollbar: true,
            font_size: None,
            font_family: None,
            remote_pane_resize_mode: NativeRemotePaneResizeMode::default(),
            scroll_mode: NativeScrollMode::default(),
            text_contrast: NativeTextContrast::default(),
            bottom_quote_enabled: false,
            bottom_quote_mode: NativeBottomQuoteMode::default(),
            bottom_quote_interval_minutes: None,
            bottom_quote_font_size: None,
            default_shell: None,
        }
    }
}

pub(crate) fn remote_pane_resize_mode() -> NativeRemotePaneResizeMode {
    load().terminal.remote_pane_resize_mode
}

/// Read on every wheel event, so through the shared handle rather than
/// `load`, which deep-clones the whole settings tree.
pub(crate) fn scroll_mode() -> NativeScrollMode {
    load_shared().terminal.scroll_mode
}

/// The contrast floor in force, or `None` to leave colours alone. The
/// settings window wins when it names a ratio; `Off` hands the question back
/// to the configuration file.
///
/// Resolved once per window rather than per cell -- see
/// `TermWindow::text_min_contrast`.
pub(crate) fn text_min_contrast_ratio(config: &ConfigHandle) -> Option<f32> {
    load_shared()
        .terminal
        .text_contrast
        .ratio()
        .or(config.text_min_contrast_ratio)
}

/// Read once per painted pane, through the shared handle.
pub(crate) fn overlay_scrollbar() -> bool {
    load_shared().terminal.overlay_scrollbar
}

/// The shell the user picked, if any. Read on every local spawn, so it
/// goes through the cached handle rather than `load`, which deep-clones
/// the whole settings tree.
///
/// A choice that no longer exists is dropped rather than passed on, so an
/// uninstalled shell degrades to the platform default instead of leaving
/// the user unable to open a terminal at all.
pub(crate) fn default_shell() -> Option<Vec<String>> {
    let argv = load_shared()
        .terminal
        .default_shell
        .as_ref()
        .filter(|argv| !argv.is_empty())
        .cloned()?;
    let Some(resolved) = crate::shell_catalog::resolve_chosen_argv_now(&argv) else {
        // Warned about once per distinct choice: this is consulted once per
        // pane, so restoring a large layout would otherwise repeat the same
        // line dozens of times.
        static WARNED_FOR: OnceLock<Mutex<Option<String>>> = OnceLock::new();
        let warned = WARNED_FOR.get_or_init(|| Mutex::new(None));
        let mut warned = warned.lock();
        if warned.as_deref() != Some(argv[0].as_str()) {
            *warned = Some(argv[0].clone());
            log::warn!(
                "the chosen default shell {:?} is not an executable file; \
                 falling back to the system default",
                argv[0]
            );
        }
        return None;
    };
    Some(resolved)
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub(crate) struct NativeChromeSettings {
    pub(crate) settings_font_size: Option<f64>,
    pub(crate) settings_font_weight: Option<u16>,
    /// The family the interface -- tabs, sidebars, Settings, the command
    /// palette -- is drawn with. `None` is the system's interface font. The
    /// terminal's font is `terminal.font_family`, not this.
    pub(crate) ui_font_family: Option<String>,
    pub(crate) home_font_size: Option<f64>,
    pub(crate) sidebar_font_size: Option<f64>,
    /// Right sidebar (files / notes / snippets) text size; None follows
    /// home_font_size.
    pub(crate) right_sidebar_font_size: Option<f64>,
    pub(crate) workspace_sidebar_width: Option<usize>,
    /// Hover-reveal of the collapsed left sidebar; `None` means enabled.
    pub(crate) workspace_sidebar_hover_reveal: Option<bool>,
    /// Whether the file preview shows a Markdown file rendered (the default)
    /// or as source; the last choice is kept.
    pub(crate) right_sidebar_markdown_preview_rendered: Option<bool>,
    pub(crate) right_sidebar_width: Option<usize>,
    pub(crate) right_sidebar_file_preview_width: Option<usize>,
    pub(crate) right_sidebar_note_pane_width: Option<usize>,
    pub(crate) right_sidebar_note_pane_expanded: Option<bool>,
    pub(crate) right_sidebar_open_with_app: Option<NativeOpenWithApp>,
    pub(crate) right_sidebar_custom_open_with_apps: Vec<NativeOpenWithApp>,
    pub(crate) tab_font_size: Option<f64>,
    pub(crate) pane_header_font_size: Option<f64>,
    /// Workspace-thread statuses hidden by the sidebar view-options filter,
    /// as stable keys ("idle", "running", "needs-attention", "finished").
    pub(crate) workspace_sidebar_hidden_statuses: Vec<String>,
    /// Which panels the right sidebar offers. Absent means on. All four may
    /// be off at once -- that is how you get rid of the right sidebar -- and
    /// the sidebar plus its tab-bar toggle then stop being drawn. Settings is
    /// a separate window, so turning one back on is still reachable.
    pub(crate) right_sidebar_files_enabled: Option<bool>,
    pub(crate) right_sidebar_notes_enabled: Option<bool>,
    /// The Snippets panel's own switch from before Snippets was a plugin.
    /// Only ever read to carry an "off" over to the plugin's switch, once
    /// (`plugins::follow_snippets_switch`), and cleared then.
    pub(crate) right_sidebar_snippets_enabled: Option<bool>,
    /// The Snippets plugin's switch as the plugin host last said it, so the
    /// panel is offered or not from the start; the host's switch, which the
    /// panel's in Settings › Sidebar & Plugins flips, is the one that
    /// counts.
    pub(crate) snippets_plugin_enabled: Option<bool>,
    /// Feature toggle for agent status detection and the right-sidebar
    /// Agents panel. Absent means on -- it was off by default while the
    /// detection was new, and is a panel toggle like the three above now.
    pub(crate) agent_panel_enabled: Option<bool>,
    /// The panels plugins add to the right sidebar, as the plugin host last
    /// listed them (`plugins::follow_panels`): offered from the start,
    /// before the host is asked.
    pub(crate) plugin_panels: Vec<NativePluginPanel>,
    /// How wide the user made each plugin's extended view, by plugin, in
    /// unscaled pixels. A plugin's goes when the host lists it no more.
    pub(crate) plugin_extended_widths: BTreeMap<String, usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub(crate) struct NativePluginPanel {
    pub(crate) id: String,
    pub(crate) name: String,
    /// A Lucide icon's name.
    pub(crate) icon: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub(crate) struct NativeOpenWithApp {
    pub(crate) id: String,
    pub(crate) label: String,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub(crate) struct NativeDeveloperSettings {
    pub(crate) developer_mode: bool,
    pub(crate) force_fallback_context_menu: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub(crate) struct NativeOnboardingSettings {
    pub(crate) seen_version: u32,
    pub(crate) language: NativeLanguagePreference,
    pub(crate) show_left_sidebar_by_default: bool,
}

impl Default for NativeOnboardingSettings {
    fn default() -> Self {
        Self {
            seen_version: 0,
            language: NativeLanguagePreference::System,
            show_left_sidebar_by_default: true,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub(crate) struct NativeCompatibilitySettings {
    pub(crate) source_path: Option<PathBuf>,
    pub(crate) selected_fields: Vec<String>,
    pub(crate) last_imported_at: Option<String>,
}

/// A rectangle in physical screen pixels, as the platform reports it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub(crate) struct NativeScreenRect {
    pub(crate) x: i32,
    pub(crate) y: i32,
    pub(crate) width: i32,
    pub(crate) height: i32,
}

/// Where the main window was the last time it was moved, resized or closed,
/// so the next launch can reopen it there.
///
/// The rect is the window's **outer frame** in physical screen pixels, in its
/// normal (unmaximized) state -- a maximized window remembers the rect it
/// would return to, next to `maximized: true`, so both the state and the size
/// behind it survive.
///
/// `work_area` is the work area of the display the window was on when this was
/// written. It is not what decides whether the placement is still usable --
/// the displays attached *now* decide that, see
/// `main_window_placement::placement_is_usable` -- but it records which desktop
/// layout the rect was measured against, which makes an unchanged layout cheap
/// to recognise and a rejected placement possible to explain.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub(crate) struct NativeMainWindowPlacement {
    pub(crate) x: i32,
    pub(crate) y: i32,
    pub(crate) width: i32,
    pub(crate) height: i32,
    pub(crate) maximized: bool,
    pub(crate) work_area: NativeScreenRect,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub(crate) struct NativeWindowSettings {
    pub(crate) restore_main_window_frame: bool,
    pub(crate) main_renderer: Option<NativeRendererBackend>,
    /// Written by the GUI rather than by the Settings UI; `None` until the
    /// main window has been placed somewhere worth remembering. Only consulted
    /// when `restore_main_window_frame` is on.
    pub(crate) main_window_placement: Option<NativeMainWindowPlacement>,
}

impl Default for NativeWindowSettings {
    fn default() -> Self {
        Self {
            restore_main_window_frame: true,
            main_renderer: None,
            main_window_placement: None,
        }
    }
}

pub(crate) const DEFAULT_REMOTE_SFTP_IDLE_MINUTES: u32 = 15;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub(crate) struct NativeWorkspaceSettings {
    pub(crate) remote_sftp_idle_minutes: u32,
    /// Where downloaded remote files land. Empty means "wherever this system
    /// puts downloads", which is what most people want and what keeps the
    /// setting meaningful after moving between machines.
    pub(crate) remote_download_directory: String,
    /// Where files dropped onto a REMOTE terminal are uploaded, as a remote
    /// path. Empty means [`DEFAULT_REMOTE_DROP_DESTINATION`]; the literal
    /// `cwd` means the shell's current directory at drop time.
    pub(crate) remote_drop_destination: String,
    /// Play a short sound when a thread you are not watching finishes, or when
    /// one starts waiting on you.
    pub(crate) notification_sounds_enabled: bool,
    /// Updating a remote mux server hands its sessions to the new version
    /// instead of stopping it; off, the update asks whether to stop it.
    pub(crate) remote_update_keeps_sessions: bool,
    /// Local terminals run in a background mux server (the default unix
    /// domain) instead of inside the GUI process, so they survive the GUI
    /// quitting, crashing or updating. Read once at launch.
    pub(crate) local_sessions_via_mux: bool,
}

impl Default for NativeWorkspaceSettings {
    fn default() -> Self {
        Self {
            remote_sftp_idle_minutes: DEFAULT_REMOTE_SFTP_IDLE_MINUTES,
            remote_download_directory: String::new(),
            remote_drop_destination: String::new(),
            notification_sounds_enabled: true,
            remote_update_keeps_sessions: true,
            // Preserve the behavior of older settings files without this field.
            // Fresh installs enable it in ThinkTermNativeSettings::for_new_install.
            local_sessions_via_mux: false,
        }
    }
}

/// Application-wide UI localization. `None` identifies a pre-localization
/// settings file so the effective preference can be migrated from the legacy
/// Onboarding-only value without mutating Onboarding itself.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub(crate) struct NativeLocalizationSettings {
    pub(crate) language: Option<String>,
}

/// Chord that toggles the command palette, picked in Settings. The default
/// bindings (⌘⇧P / ⌃⇧P) come from the keymap and always work; a non-default
/// choice here is intercepted ahead of the keymap, so it also wins over
/// whatever the chord normally does (e.g. ⌘K's clear-scrollback).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum NativeCommandPaletteHotkey {
    #[default]
    CmdShiftP,
    CmdP,
    CmdK,
    CtrlShiftP,
}

impl NativeCommandPaletteHotkey {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::CmdShiftP => "⌘ ⇧ P",
            Self::CmdP => "⌘ P",
            Self::CmdK => "⌘ K",
            Self::CtrlShiftP => "⌃ ⇧ P",
        }
    }
}

/// How long a link minted from the Web settings section lasts.
///
/// `None` is "until it is revoked". That is a real choice -- a link kept in
/// a password manager for a machine you reach every day should not expire
/// under you -- so it is offered rather than assumed away. The default is
/// still bounded, because the common case is handing a link to a phone for
/// an afternoon and never thinking about it again.
pub(crate) const DEFAULT_WEB_LINK_TTL_SECS: u64 = 8 * 60 * 60;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub(crate) struct NativeWebSettings {
    /// Seconds, or `None` for "until revoked".
    pub(crate) link_ttl_secs: Option<u64>,
    /// Listen on every address (with the server's own certificate) rather
    /// than loopback only, so a phone can reach it.
    pub(crate) reachable: bool,
}

impl Default for NativeWebSettings {
    fn default() -> Self {
        Self {
            reachable: false,
            link_ttl_secs: Some(DEFAULT_WEB_LINK_TTL_SECS),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub(crate) struct NativeCommandPaletteSettings {
    pub(crate) hotkey: NativeCommandPaletteHotkey,
    /// Visible list rows; 0 = automatic (fit the window, honouring the
    /// config's command_palette_rows).
    pub(crate) rows: u32,
    /// Overrides the config's command_palette_font_size when set.
    pub(crate) font_size: Option<f64>,
    /// Whether a top-level search may surface a few entries from inside
    /// groups (theme names, workspaces) without drilling in.
    pub(crate) search_penetrates_groups: bool,
}

impl Default for NativeCommandPaletteSettings {
    fn default() -> Self {
        Self {
            hotkey: NativeCommandPaletteHotkey::default(),
            rows: 0,
            font_size: None,
            search_penetrates_groups: true,
        }
    }
}

/// How pane tabs are dressed: see `crate::tab_icons`.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(default)]
pub(crate) struct NativeTabIconSettings {
    /// Absent means on.
    pub(crate) enabled: Option<bool>,
    /// Built-in cards the user changed, and the user's own cards, in the
    /// order they are shown.
    pub(crate) cards: Vec<NativeTabIconCard>,
}

/// One card's settings. On a built-in card every field left `None` keeps
/// the built-in value, so a future default still reaches whoever never
/// touched it.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(default)]
pub(crate) struct NativeTabIconCard {
    /// A built-in card's id (`python`), or `custom-<n>` for the user's own.
    pub(crate) id: String,
    /// The user's own cards only; a built-in keeps its own name.
    pub(crate) name: Option<String>,
    /// Replaces the card's program list.
    pub(crate) programs: Option<Vec<String>>,
    /// An imported SVG, by the hex SHA-256 of its bytes, replacing the glyph.
    pub(crate) svg: Option<String>,
    /// `#RRGGBB`.
    pub(crate) circle: Option<String>,
    /// `#RRGGBB`.
    pub(crate) glyph: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub(crate) struct ThinkTermNativeSettings {
    pub(crate) version: u32,
    pub(crate) appearance: NativeAppearanceSettings,
    pub(crate) terminal: NativeTerminalSettings,
    pub(crate) chrome: NativeChromeSettings,
    pub(crate) developer: NativeDeveloperSettings,
    pub(crate) onboarding: NativeOnboardingSettings,
    pub(crate) localization: NativeLocalizationSettings,
    pub(crate) compatibility: NativeCompatibilitySettings,
    pub(crate) window: NativeWindowSettings,
    pub(crate) workspaces: NativeWorkspaceSettings,
    pub(crate) command_palette: NativeCommandPaletteSettings,
    pub(crate) web: NativeWebSettings,
    pub(crate) tab_icons: NativeTabIconSettings,
}

impl Default for ThinkTermNativeSettings {
    fn default() -> Self {
        Self {
            version: 1,
            appearance: NativeAppearanceSettings::default(),
            terminal: NativeTerminalSettings::default(),
            chrome: NativeChromeSettings::default(),
            developer: NativeDeveloperSettings::default(),
            onboarding: NativeOnboardingSettings::default(),
            localization: NativeLocalizationSettings::default(),
            compatibility: NativeCompatibilitySettings::default(),
            window: NativeWindowSettings::default(),
            workspaces: NativeWorkspaceSettings::default(),
            command_palette: NativeCommandPaletteSettings::default(),
            web: NativeWebSettings::default(),
            tab_icons: NativeTabIconSettings::default(),
        }
    }
}

impl ThinkTermNativeSettings {
    fn for_new_install() -> Self {
        let mut settings = Self::default();
        settings.workspaces.local_sessions_via_mux = true;
        settings
    }
}

pub(crate) fn remote_sftp_idle_minutes() -> u32 {
    load().workspaces.remote_sftp_idle_minutes.clamp(1, 120)
}

/// Where remote downloads should land, or `None` to use the system's own
/// Downloads folder.
///
/// A configured path that no longer exists returns `None` rather than an
/// error: falling back to the system folder gets the file saved, whereas
/// failing the download over a stale setting does not.
pub(crate) fn remote_download_directory() -> Option<PathBuf> {
    let configured = load().workspaces.remote_download_directory;
    let trimmed = configured.trim();
    if trimmed.is_empty() {
        return None;
    }
    let path = PathBuf::from(trimmed);
    path.is_dir().then_some(path)
}

/// The folder downloads will actually use, for display and for saving into.
pub(crate) fn effective_remote_download_directory() -> Option<PathBuf> {
    remote_download_directory().or_else(|| {
        dirs_next::download_dir()
            .or_else(|| dirs_next::home_dir().map(|home| home.join("Downloads")))
    })
}

pub(crate) fn set_remote_download_directory(path: Option<PathBuf>) -> anyhow::Result<()> {
    update(|settings| {
        settings.workspaces.remote_download_directory = path
            .map(|path| path.to_string_lossy().to_string())
            .unwrap_or_default();
    })
    .map(drop)
}

/// Default landing folder for files dropped onto a remote terminal. Visible
/// on purpose: an upload the user cannot `ls` into might as well not exist,
/// and the branded name says where it came from.
pub(crate) const DEFAULT_REMOTE_DROP_DESTINATION: &str = "~/ThinkTerm_Uploads";

/// The literal setting value that means "the shell's current directory".
pub(crate) const REMOTE_DROP_DESTINATION_CWD: &str = "cwd";

/// The configured remote-drop destination, never empty. Not validated here:
/// only the remote side can judge a remote path, and its refusal surfaces on
/// the transfer row where the drop's outcome already lives.
pub(crate) fn remote_drop_destination() -> String {
    let configured = load().workspaces.remote_drop_destination;
    let trimmed = configured.trim();
    if trimmed.is_empty() {
        DEFAULT_REMOTE_DROP_DESTINATION.to_string()
    } else {
        trimmed.to_string()
    }
}

pub(crate) fn set_remote_drop_destination(value: &str) -> anyhow::Result<()> {
    let trimmed = value.trim();
    // Storing the default as emptiness keeps the file clean and lets a future
    // default change reach everyone who never made a choice.
    let destination = if trimmed.is_empty() || trimmed == DEFAULT_REMOTE_DROP_DESTINATION {
        String::new()
    } else {
        trimmed.to_string()
    };
    update(|settings| settings.workspaces.remote_drop_destination = destination).map(drop)
}

pub(crate) fn settings_path() -> PathBuf {
    config::native_settings_path()
}

/// The settings in use, and the ledger that keeps them in step with the
/// file. Made together, from one read of the file.
struct SettingsStore {
    settings: Mutex<Arc<ThinkTermNativeSettings>>,
    ledger: Mutex<WriteLedger>,
}

static SETTINGS_STORE: OnceLock<SettingsStore> = OnceLock::new();

fn settings_store() -> &'static SettingsStore {
    SETTINGS_STORE.get_or_init(|| {
        let (settings, known) = load_from_disk();
        SettingsStore {
            settings: Mutex::new(Arc::new(settings)),
            ledger: Mutex::new(WriteLedger {
                known,
                reload_owed: false,
            }),
        }
    })
}

fn settings_cache() -> &'static Mutex<Arc<ThinkTermNativeSettings>> {
    &settings_store().settings
}

/// The settings on disk, and the fingerprint of the file they were read
/// from.
fn load_from_disk() -> (ThinkTermNativeSettings, Option<u64>) {
    let path = settings_path();
    match fs::read(&path) {
        Ok(data) => {
            let settings = match serde_json::from_slice(&data) {
                Ok(settings) => settings,
                Err(err) => {
                    log::warn!(
                        "Unable to parse ThinkTerm native settings {}: {err:#}",
                        path.display()
                    );
                    ThinkTermNativeSettings::default()
                }
            };
            (settings, Some(fingerprint(&data)))
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            (ThinkTermNativeSettings::for_new_install(), None)
        }
        Err(err) => {
            log::warn!(
                "Unable to read ThinkTerm native settings {}: {err:#}",
                path.display()
            );
            (ThinkTermNativeSettings::default(), None)
        }
    }
}

pub(crate) fn load() -> ThinkTermNativeSettings {
    (**settings_cache().lock()).clone()
}

/// A shared handle to the settings, for callers that only read them.
///
/// [`load`] hands out a private copy, which is what a caller wanting to edit
/// and `save` needs. The paint path wants nothing of the sort and was taking
/// several copies a frame -- a lock and a deep clone each time, of a value
/// nobody was going to touch.
pub(crate) fn load_shared() -> Arc<ThinkTermNativeSettings> {
    Arc::clone(&settings_cache().lock())
}

pub(crate) fn should_show_onboarding(settings: &ThinkTermNativeSettings) -> bool {
    settings.onboarding.seen_version < ONBOARDING_VERSION
}

/// The settings in use, after taking in settings.json if it was changed
/// outside ThinkTerm. For the Settings window as it opens.
pub(crate) fn reload_from_disk() -> ThinkTermNativeSettings {
    take_in_disk_changes(&mut write_ledger().lock(), &settings_path());
    load()
}

/// Save a whole copy of the settings, such as the one the Settings window
/// keeps. The config watcher sees the file change and reloads the
/// configuration, as it always has for a setting. One setting is better
/// changed with `update`, which cannot write an older copy over an edit
/// made outside ThinkTerm.
pub(crate) fn save(settings: &ThinkTermNativeSettings) -> anyhow::Result<()> {
    let mut ledger = write_ledger().lock();
    if write_locked(&mut ledger, settings.clone())?.is_some() {
        ledger.reload_owed = true;
    }
    Ok(())
}

/// Change a setting and save it. The change is made to the latest settings:
/// an edit made outside ThinkTerm that the config watcher has not got to
/// yet is taken in first, not written over. Returns the settings saved.
pub(crate) fn update(
    edit: impl FnOnce(&mut ThinkTermNativeSettings),
) -> anyhow::Result<Arc<ThinkTermNativeSettings>> {
    update_with(edit, true)
}

/// Change and save state the app keeps for itself rather than a setting:
/// where a sidebar edge was dropped, whether a panel is open, where the
/// window is, what the plugin host reported. Made to the latest settings,
/// as `update` is.
/// The window that changed it has applied it already and nothing a
/// configuration reload does reads it, so the config watcher does not reload
/// for it. A reload drops every loaded font, and CJK text drew as
/// missing-glyph boxes until the fallback fonts were found again; it also
/// closed the command palette and reset key tables. That was every sidebar
/// toggle.
pub(crate) fn update_ui_state(edit: impl FnOnce(&mut ThinkTermNativeSettings)) -> anyhow::Result<()> {
    update_with(edit, false).map(drop)
}

fn update_with(
    edit: impl FnOnce(&mut ThinkTermNativeSettings),
    setting: bool,
) -> anyhow::Result<Arc<ThinkTermNativeSettings>> {
    let mut ledger = write_ledger().lock();
    take_in_disk_changes(&mut ledger, &settings_path());
    let mut settings = load();
    edit(&mut settings);
    match write_locked(&mut ledger, settings)? {
        Some(saved) => {
            ledger.reload_owed |= setting;
            Ok(saved)
        }
        None => Ok(load_shared()),
    }
}

/// Write `settings` and make them the settings in use, with the ledger held
/// so the watcher never compares the file with a write it has no record of.
/// `None` when the file holds them already and nothing was written.
fn write_locked(
    ledger: &mut WriteLedger,
    settings: ThinkTermNativeSettings,
) -> anyhow::Result<Option<Arc<ThinkTermNativeSettings>>> {
    let path = settings_path();
    let data = serde_json::to_vec_pretty(&settings)?;
    let written = Some(fingerprint(&data));
    if written == ledger.known {
        return Ok(None);
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, &data)?;
    fs::rename(tmp, path)?;
    ledger.known = written;
    let settings = Arc::new(settings);
    *settings_cache().lock() = Arc::clone(&settings);
    Ok(Some(settings))
}

/// Where the settings in use stand against settings.json, so a change to
/// the file by this process can be told from anybody else's. Every read and
/// write of the file holds it.
struct WriteLedger {
    /// The fingerprint of the file as this process last wrote or read it.
    known: Option<u64>,
    /// A setting was saved, or taken in from the file, since the watcher
    /// last looked, and is owed the configuration reload a setting has
    /// always had.
    reload_owed: bool,
}

fn write_ledger() -> &'static Mutex<WriteLedger> {
    &settings_store().ledger
}

fn fingerprint(data: &[u8]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    data.hash(&mut hasher);
    hasher.finish()
}

/// The config watcher's reload filter (`config::set_reload_filter`).
///
/// A change to anything but settings.json reloads, as before. A write of
/// settings.json by this process reloads only if it saved a setting rather
/// than state of its own. A write by anybody else (an editor, another
/// process) is taken in first: without that the settings in use stayed the
/// old ones until the Settings window was opened, and the next save here
/// wrote them back over the edit.
pub(crate) fn watched_change_needs_reload(paths: &[PathBuf]) -> bool {
    let settings = settings_path();
    let ours = paths
        .iter()
        .filter(|path| config::is_native_settings_file(path, &settings))
        .count();
    let mut ledger = write_ledger().lock();
    let (reload, external) = watched_change(ours > 0, ours < paths.len(), &settings, &mut ledger);
    if let Some(data) = external {
        take_in_external_settings(&data);
    }
    reload
}

/// Whether a watched change calls for a reload: `settings` was among the
/// changed files or not, and other files were or not. Also the new contents
/// of `settings` when somebody other than this process wrote it.
fn watched_change(
    settings_changed: bool,
    others_changed: bool,
    settings: &Path,
    ledger: &mut WriteLedger,
) -> (bool, Option<Vec<u8>>) {
    if !settings_changed {
        return (others_changed, None);
    }
    let external = external_change(ledger, settings);
    (
        others_changed || std::mem::take(&mut ledger.reload_owed),
        external,
    )
}

/// The contents of `settings` if somebody other than this process changed
/// it since this process last wrote or read it. The change becomes known,
/// and is owed the configuration reload a setting has; a file that is gone
/// has nothing to take in but is owed the reload too.
fn external_change(ledger: &mut WriteLedger, settings: &Path) -> Option<Vec<u8>> {
    let data = fs::read(settings).ok();
    let known = data.as_deref().map(fingerprint);
    if known == ledger.known {
        return None;
    }
    ledger.known = known;
    ledger.reload_owed = true;
    data
}

fn take_in_disk_changes(ledger: &mut WriteLedger, settings: &Path) {
    if let Some(data) = external_change(ledger, settings) {
        take_in_external_settings(&data);
    }
}

/// Settings written by somebody else become the ones in use, and the app is
/// brought in line with them on the GUI thread. A file that does not parse
/// leaves the settings in use alone. Called with the ledger held.
fn take_in_external_settings(data: &[u8]) {
    let after: ThinkTermNativeSettings = match serde_json::from_slice(data) {
        Ok(settings) => settings,
        Err(err) => {
            log::warn!(
                "{} changed but cannot be read; keeping the settings in use: {err:#}",
                settings_path().display()
            );
            return;
        }
    };
    let before = std::mem::replace(&mut *settings_cache().lock(), Arc::new(after));
    log::info!("{} changed outside ThinkTerm; applying it", settings_path().display());
    promise::spawn::spawn_into_main_thread(async move {
        // Against the settings in use by the time this runs: a save made in
        // between was applied by whoever made it, and is what stays.
        apply_external_change(&before, &load_shared());
    })
    .detach();
}

/// Bring the running app in line with settings that changed on disk, the
/// way the Settings window does for each change it makes; the configuration
/// reload a setting is owed follows, as it does for a save there. Most
/// settings are read where they are used and only need a repaint.
/// `local_sessions_via_mux` and `main_renderer` take effect at the next
/// start, as they do from the Settings window. Window state (sidebar widths
/// and the like) is picked up by the next window, as it is when another
/// window changes it.
fn apply_external_change(before: &ThinkTermNativeSettings, after: &ThinkTermNativeSettings) {
    let language = before.localization.language != after.localization.language
        || before.onboarding.language != after.onboarding.language;
    if language
        || before.appearance.theme_mode != after.appearance.theme_mode
        || before.appearance.app_icon != after.appearance.app_icon
        || before.appearance.color_scheme != after.appearance.color_scheme
        || before.appearance.window_opacity != after.appearance.window_opacity
        || before.chrome.ui_font_family != after.chrome.ui_font_family
    {
        // Language, theme, colour scheme, opacity, chrome, the interface's
        // font and app icon.
        apply_to_app(after);
    }
    crate::settings_window::follow_open_settings_window(before, after);

    let font_size = (before.terminal.font_size != after.terminal.font_size)
        .then_some(after.terminal.font_size);
    let contrast = before.terminal.text_contrast != after.terminal.text_contrast;
    let before_chrome = &before.chrome;
    let after_chrome = &after.chrome;
    let panels = before_chrome.right_sidebar_files_enabled != after_chrome.right_sidebar_files_enabled
        || before_chrome.right_sidebar_notes_enabled != after_chrome.right_sidebar_notes_enabled
        || before_chrome.right_sidebar_snippets_enabled
            != after_chrome.right_sidebar_snippets_enabled
        || before_chrome.snippets_plugin_enabled != after_chrome.snippets_plugin_enabled
        || before_chrome.agent_panel_enabled != after_chrome.agent_panel_enabled
        || before_chrome.plugin_panels != after_chrome.plugin_panels;
    if before_chrome.agent_panel_enabled != after_chrome.agent_panel_enabled {
        mux::agent_status::refresh_enabled();
    }
    if let Some(front_end) = crate::frontend::try_front_end() {
        for gui_window in front_end.gui_windows() {
            gui_window
                .window
                .notify(crate::termwindow::TermWindowNotif::Apply(Box::new(
                    move |term_window| {
                        if let Some(font_size) = font_size {
                            // As the Settings window applies it; no size is
                            // the configured one.
                            let base = term_window.config.font_size;
                            let font_size = font_size.unwrap_or(base);
                            if font_size.is_finite() && font_size > 0.0 && base > 0.0 {
                                let font_scale = (font_size / base).clamp(0.25, 4.0);
                                if let Some(window) = term_window.window.as_ref().cloned() {
                                    term_window.adjust_font_scale(font_scale, &window);
                                }
                            }
                        }
                        if contrast {
                            term_window.refresh_text_min_contrast();
                        }
                        if panels {
                            term_window.right_sidebar_panels_changed();
                        }
                        if language {
                            term_window.dismiss_fallback_context_menu();
                        }
                    },
                )));
        }
        front_end.invalidate_all_windows();
    }

    if before.web.reachable != after.web.reachable {
        crate::web_settings::follow_reachable();
    }
    if before_chrome.snippets_plugin_enabled != after_chrome.snippets_plugin_enabled {
        if let Some(enabled) = after_chrome.snippets_plugin_enabled {
            crate::plugins::set_enabled(thinkterm_snippets::wire::PLUGIN, enabled);
        }
    }
    if before.workspaces.remote_sftp_idle_minutes != after.workspaces.remote_sftp_idle_minutes {
        crate::termwindow::remote_files::update_remote_connection_idle_timeout(
            after.workspaces.remote_sftp_idle_minutes,
        );
    }
    if before.workspaces.remote_update_keeps_sessions
        != after.workspaces.remote_update_keeps_sessions
    {
        wezterm_client::remote_update::set_keep_sessions_on_update(
            after.workspaces.remote_update_keeps_sessions,
        );
    }
}

/// Where the main window should reopen, or `None` if nothing has been
/// remembered yet or the user turned the setting off.
pub(crate) fn main_window_placement() -> Option<NativeMainWindowPlacement> {
    let settings = load_shared();
    if !settings.window.restore_main_window_frame {
        return None;
    }
    settings.window.main_window_placement
}

/// Whether moving or resizing the main window should still be recorded.
/// Read per move/resize edge rather than cached, so turning the setting off
/// in Settings takes effect on the next drag rather than the next launch.
pub(crate) fn restore_main_window_frame_enabled() -> bool {
    load_shared().window.restore_main_window_frame
}

/// Persist where the main window is. A no-op when nothing changed, which is
/// the common case: the debounce upstream of this already collapses a drag
/// into one call, and a drag that ends where it started should not rewrite
/// the file.
pub(crate) fn save_main_window_placement(placement: NativeMainWindowPlacement) {
    if load_shared().window.main_window_placement == Some(placement) {
        return;
    }
    if let Err(err) =
        update_ui_state(|settings| settings.window.main_window_placement = Some(placement))
    {
        log::warn!("failed to save main window placement: {err:#}");
    }
}

/// Persist the palette-picked color scheme so new windows and the next
/// launch start with it. A no-op when the stored value already matches.
pub(crate) fn save_color_scheme(name: Option<String>) {
    if load_shared().appearance.color_scheme == name {
        return;
    }
    if let Err(err) = update(|settings| settings.appearance.color_scheme = name) {
        log::error!("failed to save color scheme choice: {err:#}");
    }
}

/// The appearance the windowing connection reports.
///
/// Despite the name this is **not** necessarily what the operating system is
/// set to: once a theme has been picked here, `get_appearance()` reports that
/// instead, on every platform. Nothing is lost by it -- `effective_appearance`
/// below ignores this argument for Light and Dark, and under System there is
/// no override to report -- but do not reach for this expecting to learn what
/// the OS itself says.
pub(crate) fn system_appearance() -> Appearance {
    Connection::get()
        .map(|conn| conn.get_appearance())
        .unwrap_or(Appearance::Dark)
}

pub(crate) fn effective_appearance() -> Appearance {
    let settings = load();
    settings
        .appearance
        .theme_mode
        .effective_appearance(system_appearance())
}

/// The interface's colours for `appearance`, with the user's `ui_colors`
/// overrides applied on top.
///
/// The overrides live here rather than in `ui::tokens` so that module stays
/// free of the configuration crate, and they are applied last so a colour the
/// user named wins over everything derived.
/// `mode` is handed in rather than read back out of the saved settings: the
/// settings window previews a theme mode from its own copy before saving it,
/// and reading the saved one there would derive the preview from the mode
/// being left rather than the one being chosen.
pub(crate) fn chrome_palette(
    mode: NativeThemeMode,
    appearance: Appearance,
    config: &config::ConfigHandle,
    preview_ground: Option<window::color::LinearRgba>,
) -> crate::ui::UiPalette {
    let mut palette = match mode {
        NativeThemeMode::FollowTerminal => {
            // `preview_ground` is the scheme the command palette is showing
            // but the user has not chosen. Without it the interface would keep
            // the configured scheme's colours while the terminal in front of
            // it changed -- a preview that does not preview the thing this
            // mode exists for.
            let ground = preview_ground.unwrap_or_else(|| terminal_ground(config));
            crate::ui::UiPalette::for_scheme(ground)
        }
        _ => crate::ui::UiPalette::for_appearance(appearance),
    };
    // The strip the tabs sit on is the terminal's header, not another piece of
    // sidebar: a seam where one ends and the other begins is the thing people
    // notice first. It takes the ground the terminal is actually painted with,
    // which in a dark interface is already the chrome's own colour (see
    // `dark_chrome_background` in `render/pane.rs`) and otherwise is the
    // scheme's background.
    palette.header_bg = match appearance {
        Appearance::Dark | Appearance::DarkHighContrast => palette.sidebar_bg,
        Appearance::Light | Appearance::LightHighContrast => {
            let ground = preview_ground.unwrap_or_else(|| terminal_ground(config));
            // Only when the scheme is on the same side as the interface. A
            // light interface with an explicitly chosen dark scheme would
            // otherwise get a dark strip carrying the light palette's dark
            // text -- Nord's background against it is about 1.4:1. A seam is
            // unavoidable when the two sides disagree; an unreadable tab bar
            // is not.
            if crate::ui::UiPalette::appearance_of(ground) == Appearance::Light {
                ground
            } else {
                palette.sidebar_bg
            }
        }
    };
    // Applied last so a colour the user named wins over everything derived,
    // `header_bg` included.
    apply_ui_colors(&mut palette, config.ui_colors.as_ref());
    palette
}

/// The colour scheme in force: the one the user picked, or -- when they have
/// picked none -- the one that goes with the side the interface is on.
///
/// The two are one decision to the person making it. Choosing a light
/// interface and being handed a terminal whose text was chosen for a black
/// background is not a combination anyone asks for; it is what you get when
/// the two settings are free to disagree and neither has a default that knows
/// about the other.
///
/// An explicit choice always wins -- from the palette, from the settings
/// window, or from `color_scheme` in the configuration file. Only the default
/// has a side. `FollowTerminal` is left out on purpose: there the scheme is
/// what decides the interface's side, so taking the default from that side
/// would be circular.
pub(crate) fn effective_color_scheme(
    settings: &ThinkTermNativeSettings,
    config: &ConfigHandle,
) -> Option<String> {
    resolve_color_scheme(
        settings.appearance.color_scheme.as_deref(),
        settings.appearance.theme_mode,
        settings
            .appearance
            .theme_mode
            .effective_appearance(system_appearance()),
        config.color_scheme.as_deref(),
        config.colors.is_some(),
    )
}

/// The decision itself, off the settings and the configuration so its rules
/// are testable without either.
fn resolve_color_scheme(
    picked: Option<&str>,
    mode: NativeThemeMode,
    appearance: Appearance,
    config_scheme: Option<&str>,
    config_has_inline_colors: bool,
) -> Option<String> {
    if let Some(picked) = picked {
        return Some(picked.to_string());
    }
    if mode == NativeThemeMode::FollowTerminal {
        return None;
    }
    // On macOS `color_scheme` is filled in with the dark default when the file
    // names none, so "is it set" cannot tell a choice from that fill.
    let chosen_in_the_file = match config_scheme {
        Some(name) => name != config::MACOS_DEFAULT_COLOR_SCHEME,
        None => config_has_inline_colors,
    };
    if chosen_in_the_file {
        return None;
    }
    match appearance {
        Appearance::Light | Appearance::LightHighContrast => {
            Some(config::MACOS_LIGHT_COLOR_SCHEME.to_string())
        }
        // The configuration's own default is already the dark one; leaving it
        // alone keeps the file in charge of everything it can be.
        Appearance::Dark | Appearance::DarkHighContrast => None,
    }
}

/// The terminal's own background: what the interface is being asked to sit
/// next to. Falls back to the appearance's ground when the scheme names no
/// background, which is what `resolved_palette` leaves for a configuration
/// that sets neither `colors` nor `color_scheme`.
/// A scheme's background as the terminal will actually paint it.
///
/// `colors` in the configuration overlays a scheme -- the precedence
/// `resolved_palette` is built with -- so reading the scheme raw gets the
/// wrong answer for anyone who sets both. A dark scheme with an inline white
/// background was classified as dark, which under "follow terminal colours"
/// put a dark interface behind black terminal text.
pub(crate) fn scheme_background(
    config: &config::ConfigHandle,
    scheme: &config::Palette,
) -> Option<window::color::LinearRgba> {
    config
        .colors
        .as_ref()
        .and_then(|colors| colors.background)
        .or(scheme.background)
        .map(|color| color.to_linear())
}

/// The colours the terminal is painted in, resolved the way
/// `terminal_ground` resolves its background: a scheme picked in the command
/// palette counts, and `colors` in the configuration overlays whichever
/// scheme is in force.
pub(crate) fn terminal_palette(
    settings: &ThinkTermNativeSettings,
    config: &config::ConfigHandle,
) -> wezterm_term::color::ColorPalette {
    if let Some(name) = effective_color_scheme(settings, config) {
        let picked = config
            .color_schemes
            .get(&name)
            .or_else(|| config::COLOR_SCHEMES.get(&name));
        if let Some(scheme) = picked {
            return match &config.colors {
                Some(colors) => scheme.overlay_with(colors).into(),
                None => scheme.clone().into(),
            };
        }
    }
    config.resolved_palette.clone().into()
}

fn terminal_ground(config: &config::ConfigHandle) -> window::color::LinearRgba {
    // A scheme picked in the command palette is kept here, not in the
    // configuration: it reaches a terminal window through that window's own
    // overrides. The settings window has no overrides, so reading only
    // `config` would derive its colours from the scheme in the file while the
    // terminal in front of it showed the one that was picked.
    if let Some(name) = effective_color_scheme(&load(), config) {
        let picked = config
            .color_schemes
            .get(&name)
            .or_else(|| config::COLOR_SCHEMES.get(&name));
        if let Some(background) = picked.and_then(|palette| scheme_background(config, palette)) {
            return background;
        }
    }
    match config.resolved_palette.background {
        Some(background) => background.to_linear(),
        None => crate::ui::UiPalette::for_appearance(system_appearance()).window_bg,
    }
}

/// Paint the user's chosen colours over `palette`, leaving every slot they did
/// not name alone.
pub(crate) fn apply_ui_colors(
    palette: &mut crate::ui::UiPalette,
    overrides: Option<&config::UiColors>,
) {
    let Some(colors) = overrides else {
        return;
    };
    macro_rules! apply_color {
        ($name:ident) => {
            if let Some(color) = colors.$name {
                palette.$name = color.to_linear();
            }
        };
    }
    apply_color!(window_bg);
    apply_color!(sidebar_bg);
    apply_color!(workspace_sidebar_bg);
    apply_color!(header_bg);
    apply_color!(separator);
    apply_color!(control_bg);
    apply_color!(control_hover_bg);
    apply_color!(control_pressed_bg);
    apply_color!(control_border);
    apply_color!(sidebar_button_bg);
    apply_color!(sidebar_button_hover_bg);
    apply_color!(sidebar_row_hover_bg);
    apply_color!(sidebar_row_pressed_bg);
    apply_color!(sidebar_row_active_bg);
    apply_color!(sidebar_row_active_border);
    apply_color!(selected_bg);
    apply_color!(accent);
    apply_color!(accent_hover);
    apply_color!(on_accent);
    apply_color!(danger);
    apply_color!(track_off);
    apply_color!(card_bg);
    apply_color!(group_bg);
    apply_color!(text);
    apply_color!(secondary_text);
    apply_color!(muted_text);
    apply_color!(selected_text);
    apply_color!(scrollbar_thumb);
    apply_color!(spelling_error);
}

/// Tell the platform which appearance to present its own chrome in.
///
/// Under "follow terminal colours" that is the side the *scheme* is on, not
/// the desktop's. The native chrome takes its tint from this: the title bar,
/// and on macOS the sidebar button, which is an `NSButton` drawing a template
/// SF Symbol. Left following the system it renders a white symbol onto the
/// white interface a light scheme produces, and the button disappears.
///
/// Called when the theme mode changes and again when the scheme does, since a
/// scheme change can move which side the interface is on.
pub(crate) fn apply_preferred_appearance(mode: NativeThemeMode) {
    let Some(conn) = Connection::get() else {
        return;
    };
    let preferred = match mode {
        NativeThemeMode::FollowTerminal => Some(crate::ui::UiPalette::appearance_of(
            terminal_ground(&config::configuration()),
        )),
        mode => mode.preferred_app_appearance(),
    };
    conn.set_preferred_appearance(preferred);
}

/// Draw the interface of every window with the family `settings` names:
/// all a change of it needs of `apply_to_app`. The fonts are looked up again
/// as each window next paints, so nothing happens when the family is the one
/// already in use.
pub(crate) fn apply_ui_font_to_app(settings: &ThinkTermNativeSettings) {
    if !wezterm_font::set_ui_font_family(settings.chrome.ui_font_family.clone()) {
        return;
    }
    if let Some(front_end) = crate::frontend::try_front_end() {
        for gui_window in front_end.gui_windows() {
            gui_window
                .window
                .notify(crate::termwindow::TermWindowNotif::Apply(Box::new(
                    |term_window| {
                        // Laid out with the old family's fonts, and kept.
                        term_window.invalidate_fancy_tab_bar();
                        term_window.refresh_chrome();
                    },
                )));
        }
    }
}

pub(crate) fn apply_to_app(settings: &ThinkTermNativeSettings) {
    crate::i18n::activate_from_settings(settings);
    apply_preferred_appearance(settings.appearance.theme_mode);
    // Tell the windows directly rather than waiting for the appearance event
    // the line above will eventually produce. That event is the normal route
    // and it also reloads the configuration, but macOS drops it when the
    // window is already dispatching something else (see
    // `view_did_change_effective_appearance`), and a window that missed it
    // would go on painting the old chrome. Refreshing twice costs a struct
    // copy; refreshing never is a visibly stale sidebar.
    // Moving between Light and Dark moves the *default* colour scheme with it
    // (see `effective_color_scheme`), so the terminal has to be re-pointed at
    // it, not just repainted. Applying a scheme the window already has is a
    // no-op, so the common case -- a mode change that does not move the
    // default -- costs nothing.
    // All at once, so a window reloads its configuration once for them.
    let overrides = settings_config_overrides(settings, &config::configuration());
    // Before the windows repaint below, so that they do it with the family.
    apply_ui_font_to_app(settings);
    if let Some(front_end) = crate::frontend::try_front_end() {
        for gui_window in front_end.gui_windows() {
            let overrides = overrides.clone();
            gui_window
                .window
                .notify(crate::termwindow::TermWindowNotif::Apply(Box::new(
                    move |term_window| {
                        term_window.apply_settings_overrides(&overrides);
                        term_window.refresh_chrome();
                    },
                )));
        }
    }
    // The settings window is standalone and in no broadcast list of its own.
    crate::settings_window::refresh_open_settings_window_chrome();

    #[cfg(target_os = "macos")]
    if let Some(path) = app_icon_path(settings.appearance.app_icon) {
        if let Err(err) = window::set_application_icon_from_file(&path) {
            log::warn!(
                "Unable to apply ThinkTerm app icon {} from {}: {err:#}",
                settings.appearance.app_icon.label(),
                path.display()
            );
        }
    }
}

#[cfg(target_os = "macos")]
pub(crate) fn app_icon_path(icon: NativeAppIcon) -> Option<PathBuf> {
    let file_name = icon.file_name();
    let mut candidates = Vec::new();

    if let Ok(exe) = std::env::current_exe() {
        if let Some(exe_dir) = exe.parent() {
            candidates.push(exe_dir.join(file_name));
            if let Some(contents_dir) = exe_dir.parent() {
                candidates.push(contents_dir.join("Resources").join(file_name));
            }
        }
    }

    if let Some(repo_dir) = PathBuf::from(env!("CARGO_MANIFEST_DIR")).parent() {
        candidates.push(repo_dir.join("assets").join("icon").join(file_name));
        candidates.push(
            repo_dir
                .join("assets")
                .join("macos")
                .join("ThinkTerm.app")
                .join("Contents")
                .join("Resources")
                .join(file_name),
        );
    }

    candidates.into_iter().find(|path| path.exists())
}

pub(crate) fn settings_font_size(settings: &ThinkTermNativeSettings) -> f64 {
    settings
        .chrome
        .settings_font_size
        .unwrap_or(DEFAULT_SETTINGS_FONT_SIZE)
        .clamp(10.0, 28.0)
}

pub(crate) fn settings_font_weight(settings: &ThinkTermNativeSettings) -> u16 {
    settings
        .chrome
        .settings_font_weight
        .unwrap_or(DEFAULT_SETTINGS_FONT_WEIGHT)
        .clamp(300, 800)
}

pub(crate) fn home_font_size(settings: &ThinkTermNativeSettings) -> f64 {
    settings
        .chrome
        .home_font_size
        .unwrap_or(DEFAULT_HOME_FONT_SIZE)
        .clamp(10.0, 28.0)
}

pub(crate) fn sidebar_font_size() -> f64 {
    load()
        .chrome
        .sidebar_font_size
        .unwrap_or(DEFAULT_SIDEBAR_FONT_SIZE)
        .clamp(10.0, 28.0)
}

/// Right sidebar (files / notes / snippets) text size; follows the
/// resolved Home Font Size until explicitly set.
pub(crate) fn right_sidebar_font_size(settings: &ThinkTermNativeSettings) -> f64 {
    settings
        .chrome
        .right_sidebar_font_size
        .unwrap_or_else(|| home_font_size(settings))
        .clamp(10.0, 28.0)
}

pub(crate) fn workspace_sidebar_width() -> Option<usize> {
    load().chrome.workspace_sidebar_width
}

pub(crate) fn save_workspace_sidebar_width(width: usize) -> anyhow::Result<()> {
    update_ui_state(|settings| settings.chrome.workspace_sidebar_width = Some(width))
}

/// Remember whether the workspace sidebar is open, so a new window starts the
/// way the last one was left.
///
/// This value seeds every window's collapsed state (`TermWindow::new`), but
/// nothing used to write it outside the first-run wizard — so collapsing the
/// sidebar was forgotten on the next window, and the setting had no home once
/// the wizard stopped asking about it.
pub(crate) fn save_workspace_sidebar_shown(shown: bool) {
    if load_shared().onboarding.show_left_sidebar_by_default == shown {
        return;
    }
    if let Err(err) =
        update_ui_state(|settings| settings.onboarding.show_left_sidebar_by_default = shown)
    {
        log::warn!("failed to save workspace sidebar visibility: {err:#}");
    }
}

/// Hover-reveal of the collapsed left sidebar. On unless explicitly turned
/// off, so `None` reads as enabled. `load_shared` rather than `load`: this is
/// asked once per mouse event and once per frame, and `load` deep-clones.
pub(crate) fn workspace_sidebar_hover_reveal_enabled() -> bool {
    load_shared()
        .chrome
        .workspace_sidebar_hover_reveal
        .unwrap_or(true)
}

/// Agent status detection + Agents panel feature toggle. Default on.
/// `load_shared`: asked once per work-status scan, and by the Agents arm of
/// `RightSidebarMode::panel_enabled`.
pub(crate) fn agent_panel_enabled() -> bool {
    load_shared().chrome.agent_panel_enabled.unwrap_or(true)
}

/// The three plain right-sidebar panel toggles, read under one lock. Absent
/// means on. Callers ask which panels are offered several times per frame --
/// `right_sidebar_width` alone is asked from 34 places -- and one
/// `load_shared` per panel adds up. Agents is not here: it needs the Lua
/// detector switch on top of its own toggle, so it goes through
/// `agent_status::enabled`.
pub(crate) struct RightSidebarPanelToggles {
    pub(crate) files: bool,
    pub(crate) notes: bool,
    pub(crate) snippets: bool,
    /// Whether any plugin adds a panel.
    pub(crate) plugins: bool,
}

pub(crate) fn right_sidebar_panel_toggles() -> RightSidebarPanelToggles {
    let settings = load_shared();
    RightSidebarPanelToggles {
        files: settings.chrome.right_sidebar_files_enabled.unwrap_or(true),
        notes: settings.chrome.right_sidebar_notes_enabled.unwrap_or(true),
        // An "off" from before Snippets was a plugin stands until the
        // plugin's switch has taken it over.
        snippets: settings.chrome.right_sidebar_snippets_enabled != Some(false)
            && settings.chrome.snippets_plugin_enabled.unwrap_or(true),
        plugins: !settings.chrome.plugin_panels.is_empty(),
    }
}

/// The panels plugins add to the right sidebar, as last heard.
pub(crate) fn plugin_panels() -> Vec<NativePluginPanel> {
    load_shared().chrome.plugin_panels.clone()
}

pub(crate) fn right_sidebar_width() -> Option<usize> {
    load().chrome.right_sidebar_width
}

pub(crate) fn right_sidebar_file_preview_width() -> Option<usize> {
    load().chrome.right_sidebar_file_preview_width
}

pub(crate) fn right_sidebar_note_pane_width() -> Option<usize> {
    load().chrome.right_sidebar_note_pane_width
}

pub(crate) fn right_sidebar_note_pane_expanded() -> bool {
    load()
        .chrome
        .right_sidebar_note_pane_expanded
        .unwrap_or(false)
}

pub(crate) fn right_sidebar_open_with_app() -> Option<NativeOpenWithApp> {
    load().chrome.right_sidebar_open_with_app
}

pub(crate) fn workspace_sidebar_hidden_statuses() -> Vec<String> {
    load().chrome.workspace_sidebar_hidden_statuses
}

pub(crate) fn save_workspace_sidebar_hidden_statuses(hidden: Vec<String>) -> anyhow::Result<()> {
    update_ui_state(|settings| settings.chrome.workspace_sidebar_hidden_statuses = hidden)
}

pub(crate) fn force_fallback_context_menu() -> bool {
    std::env::var_os("THINKTERM_FORCE_FALLBACK_CONTEXT_MENU").is_some()
        || load().developer.force_fallback_context_menu
}

pub(crate) fn save_right_sidebar_width(width: usize) -> anyhow::Result<()> {
    update_ui_state(|settings| settings.chrome.right_sidebar_width = Some(width))
}

pub(crate) fn save_right_sidebar_file_preview_width(width: usize) -> anyhow::Result<()> {
    update_ui_state(|settings| settings.chrome.right_sidebar_file_preview_width = Some(width))
}

pub(crate) fn right_sidebar_markdown_preview_rendered() -> bool {
    load_shared()
        .chrome
        .right_sidebar_markdown_preview_rendered
        .unwrap_or(true)
}

pub(crate) fn save_right_sidebar_markdown_preview_rendered(rendered: bool) -> anyhow::Result<()> {
    update_ui_state(|settings| {
        settings.chrome.right_sidebar_markdown_preview_rendered = Some(rendered)
    })
}

/// How wide the user made plugin `plugin`'s extended view.
pub(crate) fn plugin_extended_width(plugin: &str) -> Option<usize> {
    load_shared()
        .chrome
        .plugin_extended_widths
        .get(plugin)
        .copied()
}

pub(crate) fn save_plugin_extended_width(plugin: &str, width: usize) -> anyhow::Result<()> {
    update_ui_state(|settings| {
        settings
            .chrome
            .plugin_extended_widths
            .insert(plugin.to_string(), width);
    })
}

pub(crate) fn save_right_sidebar_note_pane_width(width: usize) -> anyhow::Result<()> {
    update_ui_state(|settings| settings.chrome.right_sidebar_note_pane_width = Some(width))
}

pub(crate) fn save_right_sidebar_note_pane_expanded(expanded: bool) -> anyhow::Result<()> {
    update_ui_state(|settings| settings.chrome.right_sidebar_note_pane_expanded = Some(expanded))
}

pub(crate) fn save_right_sidebar_open_with_app(app: NativeOpenWithApp) -> anyhow::Result<()> {
    update_ui_state(|settings| settings.chrome.right_sidebar_open_with_app = Some(app))
}

pub(crate) fn right_sidebar_custom_open_with_apps() -> Vec<NativeOpenWithApp> {
    load().chrome.right_sidebar_custom_open_with_apps
}

const MAX_CUSTOM_OPEN_WITH_APPS: usize = 20;

pub(crate) fn add_right_sidebar_custom_open_with_app(app: NativeOpenWithApp) -> anyhow::Result<()> {
    update_ui_state(|settings| {
        let apps = &mut settings.chrome.right_sidebar_custom_open_with_apps;
        apps.retain(|existing| existing.id != app.id);
        apps.push(app);
        while apps.len() > MAX_CUSTOM_OPEN_WITH_APPS {
            apps.remove(0);
        }
    })
}

pub(crate) fn mark_onboarding_seen(settings: &mut ThinkTermNativeSettings) {
    settings.onboarding.seen_version = ONBOARDING_VERSION;
}

pub(crate) fn tab_font_size() -> f64 {
    load()
        .chrome
        .tab_font_size
        .unwrap_or(DEFAULT_TAB_FONT_SIZE)
        .clamp(10.0, 28.0)
}

pub(crate) fn pane_header_font_size() -> f64 {
    load()
        .chrome
        .pane_header_font_size
        .unwrap_or(DEFAULT_PANE_HEADER_FONT_SIZE)
        .clamp(10.0, 28.0)
}

pub(crate) fn notification_sounds_enabled() -> bool {
    load().workspaces.notification_sounds_enabled
}

pub(crate) fn remote_update_keeps_sessions() -> bool {
    load().workspaces.remote_update_keeps_sessions
}

pub(crate) fn local_sessions_via_mux() -> bool {
    load().workspaces.local_sessions_via_mux
}

pub(crate) fn bottom_quote_interval_minutes(settings: &ThinkTermNativeSettings) -> u32 {
    settings
        .terminal
        .bottom_quote_interval_minutes
        .unwrap_or(DEFAULT_BOTTOM_QUOTE_INTERVAL_MINUTES)
        .clamp(1, 24 * 60)
}

pub(crate) fn bottom_quote_font_size(settings: &ThinkTermNativeSettings) -> f64 {
    settings
        .terminal
        .bottom_quote_font_size
        .unwrap_or(DEFAULT_BOTTOM_QUOTE_FONT_SIZE)
        .clamp(6.0, 20.0)
}

pub(crate) fn main_window_renderer(
    settings: &ThinkTermNativeSettings,
    config_front_end: config::FrontEndSelection,
) -> NativeRendererBackend {
    settings
        .window
        .main_renderer
        .unwrap_or_else(|| NativeRendererBackend::from_front_end(config_front_end))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hand_edited_window_opacity_never_costs_the_file() {
        for (written, read) in [
            ("80", Some(80)),
            ("0.8", Some(80)),
            ("150", Some(100)),
            ("5", Some(WINDOW_OPACITY_LEAST)),
            ("\"half\"", None),
            ("null", None),
        ] {
            let json =
                format!(r#"{{"appearance":{{"theme_mode":"dark","window_opacity":{written}}}}}"#);
            let settings: ThinkTermNativeSettings = serde_json::from_str(&json).unwrap();
            assert_eq!(settings.appearance.window_opacity, read, "{written}");
            // The rest of the file still counts.
            assert_eq!(
                settings.appearance.theme_mode,
                NativeThemeMode::Dark,
                "{written}"
            );
        }
    }

    #[test]
    fn window_opacity_overrides_the_opacity_and_asks_for_a_blur() {
        use wezterm_dynamic::ToDynamic;
        let mut settings = ThinkTermNativeSettings::default();
        let [opacity, blur] = window_opacity_overrides(&settings);
        assert_eq!(opacity, ("window_background_opacity", None));
        assert_eq!(blur, ("macos_window_background_blur", None));
        settings.appearance.window_opacity = Some(80);
        let [opacity, blur] = window_opacity_overrides(&settings);
        if window_opacity_supported() {
            assert_eq!(opacity.1, Some(0.8f64.to_dynamic()));
        } else {
            assert_eq!(opacity.1, None);
        }
        assert_eq!(blur.1.is_some(), cfg!(target_os = "macos"));
        // Below the least is the least; opaque needs no blur.
        settings.appearance.window_opacity = Some(5);
        let [opacity, ..] = window_opacity_overrides(&settings);
        if window_opacity_supported() {
            assert_eq!(opacity.1, Some(0.3f64.to_dynamic()));
        }
        settings.appearance.window_opacity = Some(100);
        let [_, blur] = window_opacity_overrides(&settings);
        assert_eq!(blur.1, None);
        // The Windows backdrop is never Settings' to set.
        assert!(!WINDOW_OPACITY_KEYS.contains(&"win32_system_backdrop"));
    }

    use config::UiColors;
    use std::convert::TryFrom;
    use window::Appearance;

    fn color(hex: &str) -> config::RgbaColor {
        config::RgbaColor::try_from(hex.to_string()).unwrap()
    }

    /// A file written before this setting existed has to keep working, and
    /// keep meaning "leave the application's colours alone".
    #[test]
    fn a_settings_file_without_a_contrast_choice_reads_as_off() {
        let json = r#"{
            "version": 1,
            "terminal": { "scroll_mode": "smooth" }
        }"#;
        let settings: ThinkTermNativeSettings =
            serde_json::from_str(json).expect("the file has to survive");
        assert_eq!(settings.terminal.text_contrast, NativeTextContrast::Off);
        assert_eq!(settings.terminal.text_contrast.ratio(), None);
        assert!(settings.terminal.scroll_mode == NativeScrollMode::Smooth);
    }

    #[test]
    fn a_contrast_choice_round_trips_through_the_file() {
        for (written, expected, ratio) in [
            ("off", NativeTextContrast::Off, None),
            ("ratio3", NativeTextContrast::Ratio3, Some(3.0)),
            ("ratio45", NativeTextContrast::Ratio45, Some(4.5)),
            ("ratio7", NativeTextContrast::Ratio7, Some(7.0)),
        ] {
            let json = format!(r#"{{"terminal": {{"text_contrast": "{written}"}}}}"#);
            let settings: ThinkTermNativeSettings =
                serde_json::from_str(&json).expect("the file has to survive");
            assert_eq!(settings.terminal.text_contrast, expected, "{written}");
            assert_eq!(settings.terminal.text_contrast.ratio(), ratio, "{written}");

            let out = serde_json::to_string(&settings).expect("serialises");
            assert!(
                out.contains(&format!("\"text_contrast\":\"{written}\"")),
                "{written} has to survive a save: {out}"
            );
        }
    }

    /// Same reasoning as the theme mode below: a ratio a newer build offers
    /// must not cost this one the rest of the file.
    #[test]
    fn an_unknown_contrast_choice_falls_back_without_failing_the_file() {
        let json = r#"{
            "version": 1,
            "terminal": { "text_contrast": "ratio21", "scroll_mode": "stepped" },
            "localization": { "language": "fr-FR" }
        }"#;
        let settings: ThinkTermNativeSettings =
            serde_json::from_str(json).expect("the file has to survive");
        assert_eq!(settings.terminal.text_contrast, NativeTextContrast::Off);
        assert_eq!(settings.terminal.scroll_mode, NativeScrollMode::Stepped);
        assert_eq!(settings.localization.language.as_deref(), Some("fr-FR"));
    }

    /// A settings file written by a newer build names a theme this one has
    /// never heard of. Failing the parse would be answered by
    /// `load_from_disk` with `Default::default()`, and the next save would
    /// write those defaults back over the language, the fonts and everything
    /// else. One unreadable field must not cost the file.
    #[test]
    fn an_unknown_theme_mode_does_not_take_the_rest_of_the_settings_with_it() {
        let json = r#"{
            "version": 1,
            "appearance": { "theme_mode": "some_future_theme" },
            "localization": { "language": "ja-JP" },
            "onboarding": { "seen_version": 7 }
        }"#;
        let settings: ThinkTermNativeSettings =
            serde_json::from_str(json).expect("the file has to survive");
        assert_eq!(
            settings.appearance.theme_mode,
            NativeThemeMode::default(),
            "the unreadable field falls back"
        );
        assert_eq!(
            settings.localization.language.as_deref(),
            Some("ja-JP"),
            "everything else is kept"
        );
        assert_eq!(settings.onboarding.seen_version, 7);
    }

    /// The four this build does know still round-trip.
    #[test]
    fn every_theme_mode_round_trips_through_the_settings_file() {
        for mode in NativeThemeMode::ALL {
            let mut settings = ThinkTermNativeSettings::default();
            settings.appearance.theme_mode = mode;
            let json = serde_json::to_string(&settings).unwrap();
            let back: ThinkTermNativeSettings = serde_json::from_str(&json).unwrap();
            assert_eq!(back.appearance.theme_mode, mode, "{mode:?}");
        }
    }

    /// A file that names two colours has to change two colours. The interface
    /// has twenty-eight slots and nobody is going to write them all out.
    #[test]
    fn ui_colors_change_only_the_slots_they_name() {
        let untouched = crate::ui::UiPalette::for_appearance(Appearance::Dark);
        let mut palette = untouched;

        let colors = UiColors {
            sidebar_bg: Some(color("#282828")),
            accent: Some(color("#d79921")),
            ..UiColors::default()
        };
        apply_ui_colors(&mut palette, Some(&colors));

        assert_eq!(palette.sidebar_bg, color("#282828").to_linear());
        assert_eq!(palette.accent, color("#d79921").to_linear());
        assert_eq!(palette.window_bg, untouched.window_bg);
        assert_eq!(palette.text, untouched.text);
        assert_eq!(palette.card_bg, untouched.card_bg);
    }

    fn resolved(
        picked: Option<&str>,
        mode: NativeThemeMode,
        appearance: Appearance,
        config_scheme: Option<&str>,
    ) -> Option<String> {
        resolve_color_scheme(picked, mode, appearance, config_scheme, false)
    }

    /// The interface's side and the terminal's colours are one decision.
    /// Nothing picked plus a light interface has to mean a light terminal, or
    /// the text is chosen for a background it is not on.
    #[test]
    fn a_light_interface_with_nothing_picked_gets_the_light_scheme() {
        assert_eq!(
            resolved(
                None,
                NativeThemeMode::Light,
                Appearance::Light,
                Some(config::MACOS_DEFAULT_COLOR_SCHEME)
            ),
            Some(config::MACOS_LIGHT_COLOR_SCHEME.to_string())
        );
    }

    /// Dark hands the question back to the configuration, whose own default is
    /// already the dark one. Nothing to override.
    #[test]
    fn a_dark_interface_leaves_the_configuration_in_charge() {
        assert_eq!(
            resolved(
                None,
                NativeThemeMode::Dark,
                Appearance::Dark,
                Some(config::MACOS_DEFAULT_COLOR_SCHEME)
            ),
            None
        );
    }

    #[test]
    fn a_picked_scheme_beats_the_side_the_interface_is_on() {
        assert_eq!(
            resolved(
                Some("Gruvbox Dark (Gogh)"),
                NativeThemeMode::Light,
                Appearance::Light,
                None
            ),
            Some("Gruvbox Dark (Gogh)".to_string())
        );
    }

    /// A scheme written in the configuration file is a choice too, and the
    /// macOS fill-in is not -- by the time this runs the two look the same, so
    /// the fill-in has to be recognised by name.
    #[test]
    fn a_scheme_named_in_the_configuration_is_left_alone() {
        assert_eq!(
            resolved(
                None,
                NativeThemeMode::Light,
                Appearance::Light,
                Some("Nord (base16)")
            ),
            None
        );
        assert_eq!(
            resolve_color_scheme(None, NativeThemeMode::Light, Appearance::Light, None, true),
            None,
            "inline `colors` is a choice as much as a named scheme"
        );
    }

    /// Under "follow terminal colours" the scheme is what decides the
    /// interface's side, so taking the default from that side is circular.
    #[test]
    fn follow_terminal_does_not_pick_a_scheme_for_itself() {
        assert_eq!(
            resolved(
                None,
                NativeThemeMode::FollowTerminal,
                Appearance::Light,
                Some(config::MACOS_DEFAULT_COLOR_SCHEME)
            ),
            None
        );
    }

    /// The light half has to be a scheme that resolves, or the override names
    /// nothing and the terminal silently keeps the dark one.
    #[test]
    fn the_light_half_is_a_scheme_we_ship_and_is_light() {
        let palette = config::COLOR_SCHEMES
            .get(config::MACOS_LIGHT_COLOR_SCHEME)
            .expect("the light default has to be a scheme we ship");
        let ground = palette
            .background
            .expect("it needs a background")
            .to_linear();
        assert_eq!(
            crate::ui::UiPalette::appearance_of(ground),
            Appearance::Light,
            "the light half has to be on the light side"
        );
    }

    /// The slot list is written out by hand in `UiColors`, again in
    /// `apply_ui_colors` and again in the documentation, and nothing makes
    /// the three agree. This catches the half that matters: a field the user
    /// can name in their configuration that the code then never reads, which
    /// would be a silently ignored setting rather than an error.
    #[test]
    fn every_slot_a_user_can_name_is_one_the_code_applies() {
        use wezterm_dynamic::{FromDynamic, ToDynamic, Value};

        for name in UiColors::possible_field_names() {
            let mut object = wezterm_dynamic::Object::default();
            object.insert(name.to_dynamic(), "#ff00ff".to_dynamic());
            let colors = UiColors::from_dynamic(&Value::Object(object), Default::default())
                .unwrap_or_else(|err| panic!("{} should deserialise: {:#}", name, err));

            let untouched = crate::ui::UiPalette::for_appearance(Appearance::Dark);
            let mut palette = untouched;
            apply_ui_colors(&mut palette, Some(&colors));
            assert_ne!(
                palette, untouched,
                "ui_colors.{} is a name the user can set and the code never reads",
                name
            );
        }
    }

    /// No `ui_colors` at all is the common case and must cost nothing.
    #[test]
    fn no_ui_colors_leaves_the_palette_alone() {
        let untouched = crate::ui::UiPalette::for_appearance(Appearance::Light);
        let mut palette = untouched;
        apply_ui_colors(&mut palette, None);
        assert_eq!(palette.window_bg, untouched.window_bg);
        assert_eq!(palette.accent, untouched.accent);
    }

    /// An empty table is not the same as no table, and must behave the same.
    #[test]
    fn an_empty_ui_colors_table_leaves_the_palette_alone() {
        let untouched = crate::ui::UiPalette::for_appearance(Appearance::Dark);
        let mut palette = untouched;
        apply_ui_colors(&mut palette, Some(&UiColors::default()));
        assert_eq!(palette.window_bg, untouched.window_bg);
        assert_eq!(palette.muted_text, untouched.muted_text);
    }

    #[test]
    fn onboarding_is_required_before_current_version() {
        let mut settings = ThinkTermNativeSettings::default();
        settings.onboarding.seen_version = 0;

        assert!(should_show_onboarding(&settings));
    }

    #[test]
    fn onboarding_is_not_required_after_current_version_is_seen() {
        let mut settings = ThinkTermNativeSettings::default();
        settings.onboarding.seen_version = ONBOARDING_VERSION;

        assert!(!should_show_onboarding(&settings));
    }

    #[test]
    fn mark_onboarding_seen_stores_current_version() {
        let mut settings = ThinkTermNativeSettings::default();

        mark_onboarding_seen(&mut settings);

        assert_eq!(settings.onboarding.seen_version, ONBOARDING_VERSION);
    }

    #[test]
    fn local_mux_is_enabled_for_new_installs_and_survives_saving() {
        let settings = ThinkTermNativeSettings::for_new_install();
        assert!(settings.workspaces.local_sessions_via_mux);

        let encoded = serde_json::to_string(&settings).unwrap();
        let decoded: ThinkTermNativeSettings = serde_json::from_str(&encoded).unwrap();
        assert!(decoded.workspaces.local_sessions_via_mux);
    }

    #[test]
    fn local_mux_preserves_existing_settings() {
        for (json, expected) in [
            (r#"{"version":1}"#, false),
            (r#"{"workspaces":{}}"#, false),
            (r#"{"workspaces":{"local_sessions_via_mux":false}}"#, false),
            (r#"{"workspaces":{"local_sessions_via_mux":true}}"#, true),
        ] {
            let settings: ThinkTermNativeSettings = serde_json::from_str(json).unwrap();
            assert_eq!(settings.workspaces.local_sessions_via_mux, expected, "{json}");
        }
    }

    #[test]
    fn an_unset_download_directory_defers_to_the_system() {
        let settings: ThinkTermNativeSettings = serde_json::from_str(r#"{"version":1}"#).unwrap();
        assert_eq!(settings.workspaces.remote_download_directory, "");
    }

    /// A folder that has since been deleted or unmounted must not fail every
    /// download; falling back to the system folder still saves the file.
    #[test]
    fn a_missing_download_directory_is_treated_as_unset() {
        let dir = tempfile::tempdir().unwrap();
        let gone = dir.path().join("no-such-folder");
        assert!(!gone.is_dir());
        assert_eq!(gone.is_dir().then_some(gone.clone()), None);

        // An existing folder is used as given.
        let present = dir.path().to_path_buf();
        assert_eq!(
            present.is_dir().then_some(present.clone()),
            Some(present.clone())
        );
    }

    #[test]
    fn older_settings_default_remote_sftp_idle_timeout() {
        let settings: ThinkTermNativeSettings = serde_json::from_str(r#"{"version":1}"#).unwrap();
        assert_eq!(
            settings.workspaces.remote_sftp_idle_minutes,
            DEFAULT_REMOTE_SFTP_IDLE_MINUTES
        );
    }

    #[test]
    fn older_settings_default_remote_pane_resize_to_auto() {
        let settings: ThinkTermNativeSettings = serde_json::from_str(r#"{"version":1}"#).unwrap();
        assert_eq!(
            settings.terminal.remote_pane_resize_mode,
            NativeRemotePaneResizeMode::Auto
        );
    }

    #[test]
    fn remote_pane_resize_modes_use_stable_snake_case_values() {
        for (mode, encoded) in [
            (NativeRemotePaneResizeMode::Auto, "auto"),
            (NativeRemotePaneResizeMode::Live, "live"),
            (NativeRemotePaneResizeMode::OnRelease, "on_release"),
        ] {
            let mut settings = ThinkTermNativeSettings::default();
            settings.terminal.remote_pane_resize_mode = mode;
            let value = serde_json::to_value(&settings).unwrap();
            assert_eq!(
                value["terminal"]["remote_pane_resize_mode"],
                serde_json::Value::String(encoded.to_string())
            );
            let decoded: ThinkTermNativeSettings = serde_json::from_value(value).unwrap();
            assert_eq!(decoded.terminal.remote_pane_resize_mode, mode);
        }
    }

    #[test]
    fn older_settings_leave_application_language_unset_for_legacy_migration() {
        let settings: ThinkTermNativeSettings = serde_json::from_str(r#"{"version":1}"#).unwrap();
        assert_eq!(settings.localization.language, None);
    }

    #[test]
    fn application_language_round_trips_independently_of_onboarding() {
        let mut settings = ThinkTermNativeSettings::default();
        settings.onboarding.language = NativeLanguagePreference::Japanese;
        settings.localization.language = Some("fr-FR".to_string());

        let encoded = serde_json::to_string(&settings).unwrap();
        let decoded: ThinkTermNativeSettings = serde_json::from_str(&encoded).unwrap();

        assert_eq!(decoded.localization.language.as_deref(), Some("fr-FR"));
        assert_eq!(
            decoded.onboarding.language,
            NativeLanguagePreference::Japanese
        );
    }

    fn ledger(known: &[u8], reload_owed: bool) -> WriteLedger {
        WriteLedger {
            known: Some(fingerprint(known)),
            reload_owed,
        }
    }

    /// A sidebar toggle rewrites settings.json: the watcher must not reload
    /// for that, must reload once for a setting, and must hand over an edit
    /// made by anybody else.
    #[test]
    fn the_watcher_reloads_for_settings_and_edits_but_not_for_window_state() {
        let dir = tempfile::tempdir().unwrap();
        let settings = dir.path().join("settings.json");
        fs::write(&settings, b"ours").unwrap();

        let mut state = ledger(b"ours", false);
        assert_eq!(
            watched_change(true, false, &settings, &mut state),
            (false, None),
            "window state written here"
        );

        let mut state = ledger(b"ours", true);
        assert_eq!(
            watched_change(true, false, &settings, &mut state),
            (true, None),
            "a setting saved here"
        );
        assert_eq!(
            watched_change(true, false, &settings, &mut state),
            (false, None),
            "reloaded for once"
        );
        assert_eq!(
            watched_change(true, true, &settings, &mut state),
            (true, None),
            "the config file changing alongside"
        );
        assert_eq!(watched_change(false, true, &settings, &mut state), (true, None));

        fs::write(&settings, b"theirs").unwrap();
        assert_eq!(
            watched_change(true, false, &settings, &mut state),
            (true, Some(b"theirs".to_vec())),
            "an edit made elsewhere"
        );
        assert_eq!(
            watched_change(true, false, &settings, &mut state),
            (false, None),
            "and known once taken in"
        );

        fs::remove_file(&settings).unwrap();
        assert_eq!(
            watched_change(true, false, &settings, &mut state),
            (true, None),
            "a file that is gone reloads and leaves the settings in use"
        );
    }

    /// A setting saved here and window state saved right after it land in
    /// one watcher batch; the setting's reload must not be lost.
    #[test]
    fn a_setting_and_window_state_in_one_batch_still_reload() {
        let dir = tempfile::tempdir().unwrap();
        let settings = dir.path().join("settings.json");
        fs::write(&settings, b"window state on top of a setting").unwrap();
        let mut state = ledger(b"window state on top of a setting", true);
        assert_eq!(watched_change(true, false, &settings, &mut state), (true, None));
    }

    /// An edit made elsewhere can be taken in before the watcher gets to it,
    /// by a save here or by the Settings window opening. It is taken in
    /// once, and the reload it is owed still comes when the watcher looks.
    #[test]
    fn an_edit_taken_in_before_the_watcher_looks_still_reloads() {
        let dir = tempfile::tempdir().unwrap();
        let settings = dir.path().join("settings.json");
        fs::write(&settings, b"theirs").unwrap();
        let mut state = ledger(b"ours", false);
        assert_eq!(external_change(&mut state, &settings), Some(b"theirs".to_vec()));
        assert_eq!(external_change(&mut state, &settings), None, "taken in once");
        assert_eq!(watched_change(true, false, &settings, &mut state), (true, None));
    }
}
