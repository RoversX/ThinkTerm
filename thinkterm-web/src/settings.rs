//! The page's own preferences, as the model holds them: what the page
//! stores per browser and hands back at boot. The model applies what
//! concerns it (language, font); the page draws the rest from here.

use serde::{Deserialize, Serialize};

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
        assert!(s.set("colour", "1").is_err());
        assert!(s.set("theme", "\"blue\"").is_err());
        let json = serde_json::to_value(&s).unwrap();
        assert_eq!(json["font"], serde_json::json!({"mode": "follow"}));
    }
}
