//! The desktop's view of `thinkterm_i18n`: the catalogues and the engine
//! live there, shared with the browser page; what stays here is what
//! needs the desktop -- the settings file the preference is read from,
//! and the system's own language list.

use crate::native_settings::{NativeLanguagePreference, ThinkTermNativeSettings};
pub(crate) use thinkterm_i18n::{
    current_locale, language_option_label, tr, tr_args, LanguageOption, LANGUAGE_OPTIONS,
    SYSTEM_PREFERENCE,
};

fn legacy_preference(language: NativeLanguagePreference) -> &'static str {
    match language {
        NativeLanguagePreference::System => SYSTEM_PREFERENCE,
        NativeLanguagePreference::English => "en-US",
        NativeLanguagePreference::Chinese => "zh-CN",
        NativeLanguagePreference::Japanese => "ja-JP",
    }
}

pub(crate) fn configured_preference(settings: &ThinkTermNativeSettings) -> &str {
    settings
        .localization
        .language
        .as_deref()
        .unwrap_or_else(|| legacy_preference(settings.onboarding.language))
}

fn system_locales() -> Vec<String> {
    sys_locale::get_locales().collect()
}

fn initial_locale() -> &'static str {
    let settings = crate::native_settings::load();
    thinkterm_i18n::resolve_preference(configured_preference(&settings), &system_locales())
}

/// Called once, early: a lookup that comes before the settings are applied
/// still resolves the configured language rather than en-US.
pub(crate) fn install() {
    thinkterm_i18n::set_initial_resolver(initial_locale);
}

pub(crate) fn activate_preference(preference: &str) -> &'static str {
    thinkterm_i18n::activate_preference(preference, &system_locales())
}

pub(crate) fn activate_from_settings(settings: &ThinkTermNativeSettings) -> &'static str {
    activate_preference(configured_preference(settings))
}

pub(crate) fn configured_language_label(settings: &ThinkTermNativeSettings) -> String {
    let preference = configured_preference(settings);
    if preference.eq_ignore_ascii_case(SYSTEM_PREFERENCE) {
        let mut args = fluent_bundle::FluentArgs::new();
        args.set("language", thinkterm_i18n::native_name_for_locale(current_locale()));
        tr_args("language-system-resolved", &args)
    } else {
        let resolved = thinkterm_i18n::resolve_preference(preference, &system_locales());
        thinkterm_i18n::native_name_for_locale(resolved).to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_language_setting_overrides_the_legacy_onboarding_value() {
        let mut settings = ThinkTermNativeSettings::default();
        settings.onboarding.language = NativeLanguagePreference::Japanese;
        assert_eq!(configured_preference(&settings), "ja-JP");
        settings.localization.language = Some("fr-FR".to_string());
        assert_eq!(configured_preference(&settings), "fr-FR");
    }

    /// The parity test only compares the locale files to each other, so a
    /// tr() call whose key exists in *no* file sails through and renders
    /// as the raw key at runtime. Sweep the source for string-literal
    /// lookups and pin every one to a real en-US entry.
    #[test]
    fn every_translated_string_literal_has_a_definition() {
        let defined: std::collections::HashSet<&str> = thinkterm_i18n::en_us_source()
            .lines()
            .filter_map(|line| {
                let (key, rest) = line.split_once('=')?;
                let key = key.trim();
                (!key.is_empty()
                    && rest.starts_with(' ')
                    && key
                        .chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'))
                .then_some(key)
            })
            .collect();

        let mut missing = Vec::new();
        let mut stack = vec![std::path::PathBuf::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src"
        ))];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).expect("src readable") {
                let path = entry.expect("entry").path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                    continue;
                }
                let text = std::fs::read_to_string(&path).expect("source readable");
                for (idx, _) in text.match_indices("tr(") {
                    // Covers tr and the *_tr helpers: all funnel through a
                    // fluent key as the first argument. Anything else whose
                    // name merely ends in "tr" (push_str, …) is skipped by
                    // requiring the identifier to be tr or *_tr. The key
                    // may sit on the next line, so whitespace between the
                    // paren and the literal is skipped.
                    let ident_start = text[..idx]
                        .rfind(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                        .map(|p| p + 1)
                        .unwrap_or(0);
                    let ident = &text[ident_start..idx + 2];
                    if ident != "tr" && !ident.ends_with("_tr") {
                        continue;
                    }
                    let after_paren = &text[idx + 3..];
                    let literal = after_paren.trim_start();
                    let Some(literal) = literal.strip_prefix('"') else {
                        continue;
                    };
                    let key: String = literal.chars().take_while(|c| *c != '"').collect();
                    if key.is_empty()
                        || !key
                            .chars()
                            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
                    {
                        continue;
                    }
                    if !defined.contains(key.as_str()) {
                        missing.push(format!("{} (in {})", key, path.display()));
                    }
                }
            }
        }
        missing.sort();
        missing.dedup();
        assert!(
            missing.is_empty(),
            "tr() keys with no en-US definition:\n{}",
            missing.join("\n")
        );
    }
}
