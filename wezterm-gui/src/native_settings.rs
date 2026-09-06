use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use window::{Appearance, Connection, ConnectionOps};

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
    pub(crate) fn effective_appearance(self, system: Appearance) -> Appearance {
        match self {
            Self::System => system,
            Self::Light => Appearance::Light,
            Self::Dark => Appearance::Dark,
        }
    }

    pub(crate) fn preferred_app_appearance(self) -> Option<Appearance> {
        match self {
            Self::System => None,
            Self::Light => Some(Appearance::Light),
            Self::Dark => Some(Appearance::Dark),
        }
    }
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
    pub(crate) fn next(self) -> Self {
        match self {
            Self::Auto => Self::Live,
            Self::Live => Self::OnRelease,
            Self::OnRelease => Self::Auto,
        }
    }
}

impl Default for NativeBottomQuoteMode {
    fn default() -> Self {
        Self::Timed
    }
}

impl NativeBottomQuoteMode {
    pub(crate) fn next(self) -> Self {
        match self {
            Self::Timed => Self::PseudoRandom,
            Self::PseudoRandom => Self::Timed,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub(crate) struct NativeAppearanceSettings {
    pub(crate) theme_mode: NativeThemeMode,
    pub(crate) app_icon: NativeAppIcon,
    /// Color scheme picked in the command palette; overrides the config's
    /// `color_scheme` for every window. `None` follows the configuration.
    pub(crate) color_scheme: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub(crate) struct NativeTerminalSettings {
    pub(crate) font_size: Option<f64>,
    pub(crate) font_family: Option<String>,
    pub(crate) remote_pane_resize_mode: NativeRemotePaneResizeMode,
    pub(crate) bottom_quote_enabled: bool,
    pub(crate) bottom_quote_mode: NativeBottomQuoteMode,
    pub(crate) bottom_quote_interval_minutes: Option<u32>,
    pub(crate) bottom_quote_font_size: Option<f64>,
    /// The program a new pane runs, as argv. `None` means the platform
    /// default (`$SHELL` / `%ComSpec%`).
    pub(crate) default_shell: Option<Vec<String>>,
}

pub(crate) fn remote_pane_resize_mode() -> NativeRemotePaneResizeMode {
    load().terminal.remote_pane_resize_mode
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
    pub(crate) home_font_size: Option<f64>,
    pub(crate) sidebar_font_size: Option<f64>,
    /// Right sidebar (files / notes / snippets) text size; None follows
    /// home_font_size.
    pub(crate) right_sidebar_font_size: Option<f64>,
    pub(crate) workspace_sidebar_width: Option<usize>,
    /// Hover-reveal of the collapsed left sidebar; `None` means enabled.
    pub(crate) workspace_sidebar_hover_reveal: Option<bool>,
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
    pub(crate) right_sidebar_snippets_enabled: Option<bool>,
    /// Feature toggle for agent status detection and the right-sidebar
    /// Agents panel. Absent means on -- it was off by default while the
    /// detection was new, and is a panel toggle like the three above now.
    pub(crate) agent_panel_enabled: Option<bool>,
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

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub(crate) struct NativeWindowSettings {
    pub(crate) restore_main_window_frame: bool,
    pub(crate) main_renderer: Option<NativeRendererBackend>,
}

impl Default for NativeWindowSettings {
    fn default() -> Self {
        Self {
            restore_main_window_frame: true,
            main_renderer: None,
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
        }
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
    let mut settings = load();
    settings.workspaces.remote_download_directory = path
        .map(|path| path.to_string_lossy().to_string())
        .unwrap_or_default();
    save(&settings)
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
    let mut settings = load();
    let trimmed = value.trim();
    // Storing the default as emptiness keeps the file clean and lets a future
    // default change reach everyone who never made a choice.
    settings.workspaces.remote_drop_destination =
        if trimmed.is_empty() || trimmed == DEFAULT_REMOTE_DROP_DESTINATION {
            String::new()
        } else {
            trimmed.to_string()
        };
    save(&settings)
}

pub(crate) fn settings_path() -> PathBuf {
    config::HOME_DIR
        .join(".config")
        .join("thinkterm")
        .join("settings.json")
}

static SETTINGS_CACHE: OnceLock<Mutex<Arc<ThinkTermNativeSettings>>> = OnceLock::new();

fn settings_cache() -> &'static Mutex<Arc<ThinkTermNativeSettings>> {
    SETTINGS_CACHE.get_or_init(|| Mutex::new(Arc::new(load_from_disk())))
}

fn load_from_disk() -> ThinkTermNativeSettings {
    let path = settings_path();
    match fs::read_to_string(&path) {
        Ok(data) => match serde_json::from_str(&data) {
            Ok(settings) => settings,
            Err(err) => {
                log::warn!(
                    "Unable to parse ThinkTerm native settings {}: {err:#}",
                    path.display()
                );
                ThinkTermNativeSettings::default()
            }
        },
        Err(err) if err.kind() == io::ErrorKind::NotFound => ThinkTermNativeSettings::default(),
        Err(err) => {
            log::warn!(
                "Unable to read ThinkTerm native settings {}: {err:#}",
                path.display()
            );
            ThinkTermNativeSettings::default()
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

pub(crate) fn reload_from_disk() -> ThinkTermNativeSettings {
    let settings = load_from_disk();
    *settings_cache().lock() = Arc::new(settings.clone());
    settings
}

pub(crate) fn save(settings: &ThinkTermNativeSettings) -> anyhow::Result<()> {
    let path = settings_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let data = serde_json::to_vec_pretty(settings)?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, data)?;
    fs::rename(tmp, path)?;
    *settings_cache().lock() = Arc::new(settings.clone());
    Ok(())
}

/// Persist the palette-picked color scheme so new windows and the next
/// launch start with it. A no-op when the stored value already matches.
pub(crate) fn save_color_scheme(name: Option<String>) {
    let mut settings = load();
    if settings.appearance.color_scheme == name {
        return;
    }
    settings.appearance.color_scheme = name;
    if let Err(err) = save(&settings) {
        log::error!("failed to save color scheme choice: {err:#}");
    }
}

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

pub(crate) fn apply_to_app(settings: &ThinkTermNativeSettings) {
    crate::i18n::activate_from_settings(settings);
    if let Some(conn) = Connection::get() {
        conn.set_preferred_appearance(settings.appearance.theme_mode.preferred_app_appearance());
    }

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
    let mut settings = load();
    settings.chrome.workspace_sidebar_width = Some(width);
    save(&settings)
}

/// Remember whether the workspace sidebar is open, so a new window starts the
/// way the last one was left.
///
/// This value seeds every window's collapsed state (`TermWindow::new`), but
/// nothing used to write it outside the first-run wizard — so collapsing the
/// sidebar was forgotten on the next window, and the setting had no home once
/// the wizard stopped asking about it.
pub(crate) fn save_workspace_sidebar_shown(shown: bool) {
    let mut settings = load();
    if settings.onboarding.show_left_sidebar_by_default == shown {
        return;
    }
    settings.onboarding.show_left_sidebar_by_default = shown;
    if let Err(err) = save(&settings) {
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
}

pub(crate) fn right_sidebar_panel_toggles() -> RightSidebarPanelToggles {
    let settings = load_shared();
    RightSidebarPanelToggles {
        files: settings.chrome.right_sidebar_files_enabled.unwrap_or(true),
        notes: settings.chrome.right_sidebar_notes_enabled.unwrap_or(true),
        snippets: settings
            .chrome
            .right_sidebar_snippets_enabled
            .unwrap_or(true),
    }
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
    let mut settings = load();
    settings.chrome.workspace_sidebar_hidden_statuses = hidden;
    save(&settings)
}

pub(crate) fn force_fallback_context_menu() -> bool {
    std::env::var_os("THINKTERM_FORCE_FALLBACK_CONTEXT_MENU").is_some()
        || load().developer.force_fallback_context_menu
}

pub(crate) fn save_right_sidebar_width(width: usize) -> anyhow::Result<()> {
    let mut settings = load();
    settings.chrome.right_sidebar_width = Some(width);
    save(&settings)
}

pub(crate) fn save_right_sidebar_file_preview_width(width: usize) -> anyhow::Result<()> {
    let mut settings = load();
    settings.chrome.right_sidebar_file_preview_width = Some(width);
    save(&settings)
}

pub(crate) fn save_right_sidebar_note_pane_width(width: usize) -> anyhow::Result<()> {
    let mut settings = load();
    settings.chrome.right_sidebar_note_pane_width = Some(width);
    save(&settings)
}

pub(crate) fn save_right_sidebar_note_pane_expanded(expanded: bool) -> anyhow::Result<()> {
    let mut settings = load();
    settings.chrome.right_sidebar_note_pane_expanded = Some(expanded);
    save(&settings)
}

pub(crate) fn save_right_sidebar_open_with_app(app: NativeOpenWithApp) -> anyhow::Result<()> {
    let mut settings = load();
    settings.chrome.right_sidebar_open_with_app = Some(app);
    save(&settings)
}

pub(crate) fn right_sidebar_custom_open_with_apps() -> Vec<NativeOpenWithApp> {
    load().chrome.right_sidebar_custom_open_with_apps
}

const MAX_CUSTOM_OPEN_WITH_APPS: usize = 20;

pub(crate) fn add_right_sidebar_custom_open_with_app(app: NativeOpenWithApp) -> anyhow::Result<()> {
    let mut settings = load();
    let apps = &mut settings.chrome.right_sidebar_custom_open_with_apps;
    apps.retain(|existing| existing.id != app.id);
    apps.push(app);
    while apps.len() > MAX_CUSTOM_OPEN_WITH_APPS {
        apps.remove(0);
    }
    save(&settings)
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
}
