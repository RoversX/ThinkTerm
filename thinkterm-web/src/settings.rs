//! The page's own preferences, as the model holds them: what the page
//! stores per browser and hands back at boot. The model applies what
//! concerns it (language, font); the page draws the rest from here.

use serde::{Deserialize, Serialize};

/// The `terminal-scheme` value that means "whatever the server is set to".
pub const FOLLOW_DESKTOP: &str = "desktop";

/// The base palette every pane draws from: this browser's pick, else the
/// server's configured scheme, else the stock palette.
pub fn configured_palette(
    chosen: Option<&wezterm_term::color::ColorPalette>,
    server: Option<&wezterm_term::color::ColorPalette>,
) -> wezterm_term::color::ColorPalette {
    chosen.or(server).cloned().unwrap_or_default()
}

/// What a pane should draw now, given the base palette and the pane's own
/// application override; `None` when nothing moved.
pub fn recomputed_palette(
    current: &wezterm_term::color::ColorPalette,
    configured: &wezterm_term::color::ColorPalette,
    application: Option<&wezterm_term::color::ColorPalette>,
) -> Option<wezterm_term::color::ColorPalette> {
    let transition = thinkterm_session::decide::application_palette_transition(
        current,
        configured,
        application.is_some(),
        application.cloned(),
    );
    transition.palette_changed.then_some(transition.palette)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct WebSettings {
    /// `"system"` or a locale tag.
    pub language: String,
    pub theme: Theme,
    pub font: FontMode,
    pub hover_reveal: bool,
    pub agents_panel: bool,
    pub palette_hotkey: Hotkey,
    pub sidebar_width: f64,
    pub scroll_mode: ScrollMode,
    /// `"desktop"` follows the server's configured scheme; anything else
    /// names a scheme this browser picked.
    pub terminal_scheme: String,
}

/// A colour scheme as the page hands it over, straight out of
/// `schemes.json`: every colour a CSS hex string.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SchemeColors {
    pub foreground: String,
    pub background: String,
    pub cursor_bg: String,
    pub cursor_fg: String,
    pub cursor_border: String,
    pub selection_bg: String,
    pub selection_fg: String,
    pub ansi: [String; 8],
    pub brights: [String; 8],
}

impl SchemeColors {
    pub fn parse(json: &str) -> Result<Self, String> {
        serde_json::from_str(json).map_err(|e| e.to_string())
    }

    /// The scheme as a palette to draw with. Every other entry (the indexed
    /// colours, the split line) keeps the stock value.
    pub fn to_palette(&self) -> Result<wezterm_term::color::ColorPalette, String> {
        use std::str::FromStr;
        use wezterm_term::color::SrgbaTuple;
        fn colour(s: &str) -> Result<SrgbaTuple, String> {
            SrgbaTuple::from_str(s).map_err(|_| format!("not a colour: {s:?}"))
        }
        let mut p = wezterm_term::color::ColorPalette::default();
        p.foreground = colour(&self.foreground)?.into();
        p.background = colour(&self.background)?.into();
        p.cursor_bg = colour(&self.cursor_bg)?.into();
        p.cursor_fg = colour(&self.cursor_fg)?.into();
        p.cursor_border = colour(&self.cursor_border)?.into();
        p.selection_bg = colour(&self.selection_bg)?.into();
        p.selection_fg = colour(&self.selection_fg)?.into();
        for (idx, hex) in self.ansi.iter().enumerate() {
            p.colors.0[idx] = colour(hex)?.into();
        }
        for (idx, hex) in self.brights.iter().enumerate() {
            p.colors.0[idx + 8] = colour(hex)?.into();
        }
        Ok(p)
    }
}

impl Default for WebSettings {
    fn default() -> Self {
        Self {
            language: thinkterm_i18n::SYSTEM_PREFERENCE.to_string(),
            theme: Theme::Dark,
            font: FontMode::Follow,
            hover_reveal: true,
            agents_panel: false,
            palette_hotkey: Hotkey::CmdK,
            sidebar_width: 220.0,
            scroll_mode: ScrollMode::Smooth,
            terminal_scheme: FOLLOW_DESKTOP.to_string(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Theme {
    Dark,
    Light,
    System,
}

/// How the scrollback follows a finger, a trackpad or a wheel: by the
/// pixel, so the rows move with the hand, or a whole row at a time as a
/// wheel's notch always has.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ScrollMode {
    Stepped,
    Smooth,
}

impl ScrollMode {
    pub fn is_smooth(self) -> bool {
        matches!(self, Self::Smooth)
    }
}

/// The base font size: the desktop's cell, or a size of the page's own.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "kebab-case")]
pub enum FontMode {
    Follow,
    Pinned { pt: f64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Hotkey {
    CmdK,
    CmdShiftP,
    CtrlShiftP,
}

impl WebSettings {
    pub fn parse(json: &str) -> Result<Self, String> {
        serde_json::from_str(json).map_err(|e| e.to_string())
    }

    /// One field by its JSON name, from a JSON value.
    pub fn set(&mut self, key: &str, value: &str) -> Result<(), String> {
        let mut all = serde_json::to_value(&*self).map_err(|e| e.to_string())?;
        let value: serde_json::Value = serde_json::from_str(value).map_err(|e| e.to_string())?;
        let object = all.as_object_mut().ok_or("settings are an object")?;
        if !object.contains_key(key) {
            return Err(format!("no setting {key:?}"));
        }
        object.insert(key.to_string(), value);
        *self = serde_json::from_value(all).map_err(|e| e.to_string())?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tinted(v: f32) -> wezterm_term::color::ColorPalette {
        let mut p = wezterm_term::color::ColorPalette::default();
        p.background = (v, v, v, 1.0).into();
        p
    }

    #[test]
    fn the_browsers_pick_outranks_the_servers_scheme() {
        let chosen = tinted(0.25);
        let server = tinted(0.5);
        assert_eq!(configured_palette(Some(&chosen), Some(&server)), chosen);
        assert_eq!(configured_palette(None, Some(&server)), server);
        assert_eq!(
            configured_palette(None, None),
            wezterm_term::color::ColorPalette::default()
        );
    }

    #[test]
    fn an_application_override_outranks_the_base_and_survives_a_base_change() {
        let base = tinted(0.5);
        let app = tinted(0.75);
        let current = wezterm_term::color::ColorPalette::default();
        // No override: the base shows through, once.
        assert_eq!(recomputed_palette(&current, &base, None), Some(base.clone()));
        assert_eq!(recomputed_palette(&base, &base, None), None);
        // With one, the base is invisible: changing it moves nothing.
        assert_eq!(recomputed_palette(&base, &base, Some(&app)), Some(app.clone()));
        assert_eq!(recomputed_palette(&app, &tinted(0.1), Some(&app)), None);
        // Cleared, the (new) base comes back.
        let other = tinted(0.1);
        assert_eq!(recomputed_palette(&app, &other, None), Some(other));
    }

    #[test]
    fn scheme_colours_parse_into_a_palette() {
        let json = r##"{"foreground":"#f8f8f2","background":"#282a36",
            "cursor_bg":"#f8f8f2","cursor_fg":"#282a36","cursor_border":"#f8f8f2",
            "selection_bg":"#44475a","selection_fg":"#f8f8f2",
            "ansi":["#000000","#ff5555","#50fa7b","#f1fa8c","#bd93f9","#ff79c6","#8be9fd","#bfbfbf"],
            "brights":["#4d4d4d","#ff6e67","#5af78e","#f4f99d","#caa9fa","#ff92d0","#9aedfe","#e6e6e6"]}"##;
        let palette = SchemeColors::parse(json).unwrap().to_palette().unwrap();
        assert_eq!(palette.background.to_rgb_string(), "#282a36");
        assert_eq!(palette.colors.0[1].to_rgb_string(), "#ff5555");
        assert_eq!(palette.colors.0[15].to_rgb_string(), "#e6e6e6");
        let bad = SchemeColors::parse(&json.replace("#282a36", "not-a-colour")).unwrap();
        assert!(bad.to_palette().is_err());
    }

    #[test]
    fn missing_fields_take_defaults_and_keys_are_checked() {
        let s = WebSettings::parse(r#"{"theme":"light","font":{"mode":"pinned","pt":14}}"#).unwrap();
        assert_eq!(s.theme, Theme::Light);
        assert_eq!(s.font, FontMode::Pinned { pt: 14.0 });
        assert_eq!(s.language, "system");
        assert!(s.hover_reveal);
        let mut s = WebSettings::default();
        s.set("language", "\"de-DE\"").unwrap();
        s.set("palette-hotkey", "\"ctrl-shift-p\"").unwrap();
        assert_eq!((s.language.as_str(), s.palette_hotkey), ("de-DE", Hotkey::CtrlShiftP));
        s.set("terminal-scheme", "\"Dracula\"").unwrap();
        assert_eq!(s.terminal_scheme, "Dracula");
        assert_eq!(WebSettings::default().scroll_mode, ScrollMode::Smooth, "pixels by default");
        s.set("scroll-mode", "\"stepped\"").unwrap();
        assert_eq!(s.scroll_mode, ScrollMode::Stepped);
        assert!(!s.scroll_mode.is_smooth());
        assert_eq!(serde_json::to_value(&s).unwrap()["scroll-mode"], serde_json::json!("stepped"));
        s.set("scroll-mode", "\"smooth\"").unwrap();
        assert!(s.scroll_mode.is_smooth());
        assert!(s.set("scroll-mode", "\"glide\"").is_err());
        assert_eq!(s.scroll_mode, ScrollMode::Smooth, "a refused value changes nothing");
        assert!(s.set("colour", "1").is_err());
        assert!(s.set("theme", "\"blue\"").is_err());
        let json = serde_json::to_value(&s).unwrap();
        assert_eq!(json["font"], serde_json::json!({"mode": "follow"}));
    }
}
