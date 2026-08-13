use anyhow::{Context, Result};
use ratatui::style::Color;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{OnceLock, RwLock};

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ThemeName {
    #[default]
    MonochromeDark,
    MonochromeLight,
    Terminal,
    Catppuccin,
    TokyoNight,
    Dracula,
    Nord,
    Gruvbox,
    SolarizedDark,
    SolarizedLight,
}

impl ThemeName {
    pub const ALL: [Self; 10] = [
        Self::MonochromeDark,
        Self::MonochromeLight,
        Self::Terminal,
        Self::Catppuccin,
        Self::TokyoNight,
        Self::Dracula,
        Self::Nord,
        Self::Gruvbox,
        Self::SolarizedDark,
        Self::SolarizedLight,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::MonochromeDark => "Monochrome Dark",
            Self::MonochromeLight => "Monochrome Light",
            Self::Terminal => "Terminal Default",
            Self::Catppuccin => "Catppuccin",
            Self::TokyoNight => "Tokyo Night",
            Self::Dracula => "Dracula",
            Self::Nord => "Nord",
            Self::Gruvbox => "Gruvbox",
            Self::SolarizedDark => "Solarized Dark",
            Self::SolarizedLight => "Solarized Light",
        }
    }

    pub fn next(self, delta: isize) -> Self {
        let current = Self::ALL
            .iter()
            .position(|theme| *theme == self)
            .unwrap_or(0);
        Self::ALL[(current as isize + delta).rem_euclid(Self::ALL.len() as isize) as usize]
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct TuiConfig {
    pub theme: ThemeName,
    pub sidebar_visible: bool,
    pub sidebar_width: u16,
    pub mouse: bool,
    pub copy_on_select: bool,
    pub scroll_lines: usize,
    /// Width at or below which the tree stops sharing the screen with the
    /// terminal and becomes a full-width overlay instead. Phones and tablets
    /// vary enormously in how many columns they report, so this is the one
    /// number worth turning up by hand.
    pub narrow_width: u16,
    /// Whether to size the buttons for a fingertip rather than a pointer.
    ///
    /// How big a target has to be is a property of what is doing the pointing,
    /// not of how wide the screen is: a phone held sideways reports plenty of
    /// columns and still has no mouse. Left unset this is inferred from the
    /// layout being narrow, which is right often enough to be a default and
    /// wrong exactly when a phone is roomy — hence the override.
    pub touch_targets: Option<bool>,
    /// Draw an interactive scrollbar beside each terminal pane.
    ///
    /// Off by default: the column it takes is one the server is not told about,
    /// because a tab reports a single grid and the split arithmetic belongs to
    /// the server. Until the two agree, the bar is worth having only where the
    /// last column can be spared.
    pub pane_scrollbars: bool,
    /// Draw a frame around each pane.
    ///
    /// On by default for split layouts. The pane nav remains a separate layer
    /// above the frame, so the terminal surface has one unambiguous, complete
    /// outline rather than borrowing disconnected pieces of split dividers.
    pub pane_borders: bool,
    /// Keep the command/status strip at the bottom of the screen.
    ///
    /// Off by default: transient messages and non-terminal mode hints are
    /// overlays, so showing or expiring one never changes the PTY grid.
    pub show_status_bar: bool,
    /// Give each pane a strip of its own carrying the panes stacked behind it
    /// and what can be done to it.
    ///
    /// On by default, and the one piece of chrome that earns its row even on a
    /// single unsplit pane: zoom, split and new-terminal live nowhere else a
    /// finger can reach them. Without it a phone can only get at them through
    /// `Ctrl-b`, which is exactly the keyboard a phone does not have.
    pub pane_nav_bar: bool,
}

impl Default for TuiConfig {
    fn default() -> Self {
        Self {
            theme: ThemeName::MonochromeDark,
            sidebar_visible: true,
            sidebar_width: 26,
            mouse: true,
            copy_on_select: true,
            scroll_lines: 3,
            narrow_width: 64,
            touch_targets: None,
            pane_scrollbars: false,
            pane_borders: true,
            show_status_bar: false,
            pane_nav_bar: true,
        }
    }
}

impl TuiConfig {
    pub fn normalize(&mut self) {
        self.sidebar_width = self.sidebar_width.clamp(18, 36);
        self.scroll_lines = self.scroll_lines.clamp(1, 20);
        // The floor keeps a tree that is narrower than its own indentation from
        // being reachable at all; the ceiling keeps a typo from turning a
        // desktop into a phone.
        self.narrow_width = self.narrow_width.clamp(20, 200);
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct TuiPersistentState {
    pub last_domain: Option<String>,
    pub selected_threads: BTreeMap<String, String>,
    pub selected_tabs: BTreeMap<String, usize>,
}

fn home_relative(env_name: &str, fallback: &[&str]) -> PathBuf {
    if let Some(path) = std::env::var_os(env_name) {
        return PathBuf::from(path);
    }
    let mut path = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    for component in fallback {
        path.push(component);
    }
    path
}

pub fn config_path(override_path: Option<PathBuf>) -> PathBuf {
    override_path
        .or_else(|| std::env::var_os("THINKTERM_TUI_CONFIG").map(PathBuf::from))
        .unwrap_or_else(|| {
            home_relative("XDG_CONFIG_HOME", &[".config"]).join("thinkterm/tui.toml")
        })
}

pub fn state_path() -> PathBuf {
    home_relative("XDG_STATE_HOME", &[".local", "state"]).join("thinkterm/tui.json")
}

pub fn load_config(path: &Path) -> Result<TuiConfig> {
    if !path.exists() {
        return Ok(TuiConfig::default());
    }
    let mut config: TuiConfig = toml::from_str(
        &std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?,
    )
    .with_context(|| format!("parse {}", path.display()))?;
    config.normalize();
    Ok(config)
}

/// Write only the settings that differ from the built-in defaults.
///
/// Writing the whole struct is what made changing a default pointless: any
/// reader who had ever touched a single setting had every *other* setting
/// written out at its value of that day, pinned forever. A file that records
/// only the deliberate choices lets a default stay a default.
pub fn save_config(path: &Path, config: &TuiConfig) -> Result<()> {
    write_atomic(path, changed_settings_toml(config)?.as_bytes())
}

fn changed_settings_toml(config: &TuiConfig) -> Result<String> {
    let toml::Value::Table(mut table) =
        toml::Value::try_from(config).context("serialize TUI settings")?
    else {
        anyhow::bail!("TUI settings did not serialize to a table");
    };
    let toml::Value::Table(defaults) =
        toml::Value::try_from(TuiConfig::default()).context("serialize default TUI settings")?
    else {
        anyhow::bail!("default TUI settings did not serialize to a table");
    };
    table.retain(|key, value| defaults.get(key) != Some(value));
    toml::to_string_pretty(&toml::Value::Table(table)).context("format TUI settings")
}

pub fn load_state(path: &Path) -> Result<TuiPersistentState> {
    if !path.exists() {
        return Ok(TuiPersistentState::default());
    }
    serde_json::from_slice(
        &std::fs::read(path).with_context(|| format!("read {}", path.display()))?,
    )
    .with_context(|| format!("parse {}", path.display()))
}

pub fn save_state(path: &Path, state: &TuiPersistentState) -> Result<()> {
    write_atomic(
        path,
        &serde_json::to_vec_pretty(state).context("serialize TUI state")?,
    )
}

fn write_atomic(path: &Path, contents: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .with_context(|| format!("{} has no parent", path.display()))?;
    std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    let temporary = path.with_extension("tmp");
    std::fs::write(&temporary, contents)
        .with_context(|| format!("write {}", temporary.display()))?;
    std::fs::rename(&temporary, path).with_context(|| format!("replace {}", path.display()))
}

#[derive(Clone, Copy, Debug)]
/// A theme paints its own `background` only when it cannot rely on the host's.
///
/// The dark themes leave it `Color::Reset` and share whatever background the
/// terminal already has, so the sidebar and the panes can never disagree about
/// what "the background" is — that disagreement is what made the chrome read as
/// a different shade of grey stuck beside the terminal. A terminal is dark
/// almost always, so this is almost always right.
///
/// The two light themes cannot make that bet: black text inherited onto a dark
/// terminal is black text nobody can read. They state a background and accept
/// looking like a panel, which is the lesser of the two.
///
/// Either way only `surface` lifts anything off it, and only for the one row
/// being worked on.
pub struct ThemePalette {
    pub reset_chrome: bool,
    pub foreground: Color,
    pub background: Color,
    /// The background this theme's colours were *chosen* against. Never
    /// painted — `background` is what gets painted — but it is what the
    /// readability of every other colour here is measured against, which is
    /// where the only reader of it lives.
    #[allow(dead_code)]
    pub designed_for: Color,
    pub surface: Color,
    pub dim: Color,
    pub border: Color,
    pub accent: Color,
    pub accent_foreground: Color,
    pub success: Color,
    pub warning: Color,
    pub danger: Color,
}

impl ThemePalette {
    fn for_name(name: ThemeName) -> Self {
        let rgb = Color::Rgb;
        match name {
            ThemeName::MonochromeDark => Self {
                reset_chrome: false,
                foreground: Color::White,
                background: Color::Reset,
                designed_for: Color::Rgb(0, 0, 0),
                surface: Color::DarkGray,
                dim: Color::DarkGray,
                border: Color::DarkGray,
                accent: Color::White,
                accent_foreground: Color::Black,
                success: Color::White,
                warning: Color::White,
                danger: Color::White,
            },
            ThemeName::Terminal => Self {
                reset_chrome: true,
                foreground: Color::Reset,
                background: Color::Reset,
                designed_for: Color::Rgb(0, 0, 0),
                surface: Color::Reset,
                dim: Color::DarkGray,
                border: Color::DarkGray,
                accent: Color::White,
                accent_foreground: Color::Black,
                success: Color::Green,
                warning: Color::Yellow,
                danger: Color::LightRed,
            },
            ThemeName::MonochromeLight => Self {
                reset_chrome: false,
                foreground: Color::Black,
                background: Color::White,
                designed_for: Color::Rgb(255, 255, 255),
                surface: Color::Gray,
                dim: Color::DarkGray,
                border: Color::DarkGray,
                accent: Color::Black,
                accent_foreground: Color::White,
                success: Color::Black,
                warning: Color::Black,
                danger: Color::Black,
            },
            ThemeName::Catppuccin => Self {
                reset_chrome: false,
                foreground: rgb(205, 214, 244),
                background: Color::Reset,
                designed_for: rgb(24, 24, 37),
                surface: rgb(49, 50, 68),
                dim: rgb(166, 173, 200),
                border: rgb(69, 71, 90),
                accent: rgb(137, 180, 250),
                accent_foreground: rgb(30, 30, 46),
                success: rgb(166, 227, 161),
                warning: rgb(249, 226, 175),
                danger: rgb(243, 139, 168),
            },
            ThemeName::TokyoNight => Self {
                reset_chrome: false,
                foreground: rgb(192, 202, 245),
                background: Color::Reset,
                designed_for: rgb(26, 27, 38),
                surface: rgb(36, 40, 59),
                dim: rgb(169, 177, 214),
                border: rgb(65, 72, 104),
                accent: rgb(122, 162, 247),
                accent_foreground: rgb(26, 27, 38),
                success: rgb(158, 206, 106),
                warning: rgb(224, 175, 104),
                danger: rgb(247, 118, 142),
            },
            ThemeName::Dracula => Self {
                reset_chrome: false,
                foreground: rgb(248, 248, 242),
                background: Color::Reset,
                designed_for: rgb(40, 42, 54),
                surface: rgb(68, 71, 90),
                dim: rgb(210, 210, 220),
                border: rgb(98, 114, 164),
                accent: rgb(189, 147, 249),
                accent_foreground: rgb(40, 42, 54),
                success: rgb(80, 250, 123),
                warning: rgb(241, 250, 140),
                danger: rgb(255, 85, 85),
            },
            ThemeName::Nord => Self {
                reset_chrome: false,
                foreground: rgb(236, 239, 244),
                background: Color::Reset,
                designed_for: rgb(46, 52, 64),
                surface: rgb(59, 66, 82),
                dim: rgb(216, 222, 233),
                border: rgb(76, 86, 106),
                accent: rgb(136, 192, 208),
                accent_foreground: rgb(46, 52, 64),
                success: rgb(163, 190, 140),
                warning: rgb(235, 203, 139),
                danger: rgb(191, 97, 106),
            },
            ThemeName::Gruvbox => Self {
                reset_chrome: false,
                foreground: rgb(235, 219, 178),
                background: Color::Reset,
                designed_for: rgb(40, 40, 40),
                surface: rgb(60, 56, 54),
                dim: rgb(213, 196, 161),
                border: rgb(80, 73, 69),
                accent: rgb(250, 189, 47),
                accent_foreground: rgb(40, 40, 40),
                success: rgb(184, 187, 38),
                warning: rgb(250, 189, 47),
                danger: rgb(251, 73, 52),
            },
            ThemeName::SolarizedDark => Self {
                reset_chrome: false,
                foreground: rgb(147, 161, 161),
                background: Color::Reset,
                designed_for: rgb(0, 43, 54),
                surface: rgb(7, 54, 66),
                dim: rgb(131, 148, 150),
                border: rgb(88, 110, 117),
                accent: rgb(38, 139, 210),
                accent_foreground: rgb(0, 0, 0),
                success: rgb(133, 153, 0),
                warning: rgb(181, 137, 0),
                danger: rgb(220, 50, 47),
            },
            ThemeName::SolarizedLight => Self {
                reset_chrome: false,
                foreground: rgb(88, 110, 117),
                background: rgb(253, 246, 227),
                designed_for: rgb(253, 246, 227),
                surface: rgb(238, 232, 213),
                dim: rgb(88, 110, 117),
                border: rgb(147, 161, 161),
                accent: rgb(38, 139, 210),
                accent_foreground: rgb(0, 0, 0),
                success: rgb(133, 153, 0),
                warning: rgb(181, 137, 0),
                danger: rgb(220, 50, 47),
            },
        }
    }
}

static ACTIVE_THEME: OnceLock<RwLock<ThemePalette>> = OnceLock::new();

pub fn set_active_theme(name: ThemeName) {
    let palette = ThemePalette::for_name(name);
    if let Some(active) = ACTIVE_THEME.get() {
        *active.write().unwrap() = palette;
    } else {
        let _ = ACTIVE_THEME.set(RwLock::new(palette));
    }
}

pub fn palette() -> ThemePalette {
    *ACTIVE_THEME
        .get_or_init(|| RwLock::new(ThemePalette::for_name(ThemeName::default())))
        .read()
        .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A default that a reader never chose must not be frozen into their file,
    /// or changing it later reaches nobody who has ever opened the settings.
    #[test]
    fn only_deliberate_choices_are_written_back() {
        assert_eq!(
            changed_settings_toml(&TuiConfig::default()).unwrap().trim(),
            "",
            "an untouched config writes nothing at all"
        );

        let mut config = TuiConfig::default();
        config.pane_borders = !TuiConfig::default().pane_borders;
        config.scroll_lines = 9;
        let written = changed_settings_toml(&config).unwrap();
        assert!(written.contains("pane_borders"));
        assert!(written.contains("scroll_lines = 9"));
        assert!(
            !written.contains("theme"),
            "a setting left alone stays absent: {written}"
        );

        // And what is written still round-trips.
        let reloaded: TuiConfig = toml::from_str(&written).unwrap();
        assert_eq!(reloaded.pane_borders, config.pane_borders);
        assert_eq!(reloaded.scroll_lines, 9);
        assert_eq!(reloaded.theme, TuiConfig::default().theme);
    }

    #[test]
    fn config_defaults_and_normalizes_bounds() {
        let mut config: TuiConfig = toml::from_str("sidebar_width = 2\nscroll_lines = 99").unwrap();
        config.normalize();
        assert_eq!(config.sidebar_width, 18);
        assert_eq!(config.scroll_lines, 20);
        assert_eq!(config.theme, ThemeName::MonochromeDark);
        assert!(config.pane_borders);
    }

    #[test]
    fn themes_cycle_without_falling_off_the_list() {
        assert_eq!(
            ThemeName::MonochromeDark.next(-1),
            ThemeName::SolarizedLight
        );
        assert_eq!(ThemeName::SolarizedLight.next(1), ThemeName::MonochromeDark);
    }

    #[test]
    fn monochrome_and_terminal_default_are_distinct_themes() {
        let monochrome = ThemePalette::for_name(ThemeName::MonochromeDark);
        let terminal = ThemePalette::for_name(ThemeName::Terminal);
        // Both paint on the terminal's own background; what separates them is
        // whether the theme states its foreground at all.
        assert_eq!(monochrome.background, Color::Reset);
        assert_eq!(terminal.background, Color::Reset);
        assert!(!monochrome.reset_chrome);
        assert_eq!(monochrome.foreground, Color::White);
        assert!(terminal.reset_chrome);
        assert_eq!(terminal.foreground, Color::Reset);
    }

    #[test]
    fn named_themes_keep_primary_muted_and_selected_text_readable() {
        fn rgb(color: Color) -> [u8; 3] {
            let Color::Rgb(red, green, blue) = color else {
                panic!("named theme color is not RGB: {color:?}")
            };
            [red, green, blue]
        }
        fn luminance(color: [u8; 3]) -> f64 {
            let channel = |value: u8| {
                let value = f64::from(value) / 255.0;
                if value <= 0.03928 {
                    value / 12.92
                } else {
                    ((value + 0.055) / 1.055).powf(2.4)
                }
            };
            0.2126 * channel(color[0]) + 0.7152 * channel(color[1]) + 0.0722 * channel(color[2])
        }
        fn contrast(left: Color, right: Color) -> f64 {
            let left = luminance(rgb(left));
            let right = luminance(rgb(right));
            (left.max(right) + 0.05) / (left.min(right) + 0.05)
        }

        for name in [
            ThemeName::Catppuccin,
            ThemeName::TokyoNight,
            ThemeName::Dracula,
            ThemeName::Nord,
            ThemeName::Gruvbox,
            ThemeName::SolarizedDark,
            ThemeName::SolarizedLight,
        ] {
            let palette = ThemePalette::for_name(name);
            assert!(
                contrast(palette.foreground, palette.designed_for) >= 4.5,
                "{} primary text lacks contrast",
                name.label()
            );
            assert!(
                contrast(palette.dim, palette.designed_for) >= 4.5,
                "{} muted text lacks contrast",
                name.label()
            );
            assert!(
                contrast(palette.accent_foreground, palette.accent) >= 4.5,
                "{} selected text lacks contrast",
                name.label()
            );
        }
    }
}
