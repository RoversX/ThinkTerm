use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::PathBuf;
use window::{Appearance, Connection, ConnectionOps};

pub(crate) const DEFAULT_SETTINGS_FONT_SIZE: f64 = 14.0;
pub(crate) const DEFAULT_SETTINGS_FONT_WEIGHT: u16 = 600;
pub(crate) const DEFAULT_SIDEBAR_FONT_SIZE: f64 = 15.0;
pub(crate) const DEFAULT_TAB_FONT_SIZE: f64 = 14.0;
pub(crate) const DEFAULT_PANE_HEADER_FONT_SIZE: f64 = 14.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum NativeThemeMode {
    System,
    Light,
    Dark,
}

impl Default for NativeThemeMode {
    fn default() -> Self {
        Self::System
    }
}

impl NativeThemeMode {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::System => "System",
            Self::Light => "Light",
            Self::Dark => "Dark",
        }
    }

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

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub(crate) struct NativeAppearanceSettings {
    pub(crate) theme_mode: NativeThemeMode,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub(crate) struct NativeTerminalSettings {
    pub(crate) font_size: Option<f64>,
    pub(crate) font_family: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub(crate) struct NativeChromeSettings {
    pub(crate) settings_font_size: Option<f64>,
    pub(crate) settings_font_weight: Option<u16>,
    pub(crate) sidebar_font_size: Option<f64>,
    pub(crate) tab_font_size: Option<f64>,
    pub(crate) pane_header_font_size: Option<f64>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub(crate) struct ThinkTermNativeSettings {
    pub(crate) version: u32,
    pub(crate) appearance: NativeAppearanceSettings,
    pub(crate) terminal: NativeTerminalSettings,
    pub(crate) chrome: NativeChromeSettings,
}

impl Default for ThinkTermNativeSettings {
    fn default() -> Self {
        Self {
            version: 1,
            appearance: NativeAppearanceSettings::default(),
            terminal: NativeTerminalSettings::default(),
            chrome: NativeChromeSettings::default(),
        }
    }
}

pub(crate) fn settings_path() -> PathBuf {
    config::HOME_DIR
        .join(".config")
        .join("thinkterm")
        .join("settings.json")
}

pub(crate) fn load() -> ThinkTermNativeSettings {
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

pub(crate) fn save(settings: &ThinkTermNativeSettings) -> anyhow::Result<()> {
    let path = settings_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let data = serde_json::to_vec_pretty(settings)?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, data)?;
    fs::rename(tmp, path)?;
    Ok(())
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
    if let Some(conn) = Connection::get() {
        conn.set_preferred_appearance(settings.appearance.theme_mode.preferred_app_appearance());
    }
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

pub(crate) fn sidebar_font_size() -> f64 {
    load()
        .chrome
        .sidebar_font_size
        .unwrap_or(DEFAULT_SIDEBAR_FONT_SIZE)
        .clamp(10.0, 28.0)
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
