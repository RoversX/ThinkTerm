use crate::native_settings::{NativeLanguagePreference, ThinkTermNativeSettings};
use fluent_bundle::concurrent::FluentBundle;
use fluent_bundle::{FluentArgs, FluentResource};
use fluent_langneg::negotiate_languages;
use fluent_langneg::NegotiationStrategy;
use parking_lot::Mutex;
use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::OnceLock;
use unic_langid::LanguageIdentifier;

pub(crate) const SYSTEM_PREFERENCE: &str = "system";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LanguageOption {
    /// Stable value persisted in native settings.
    pub preference: &'static str,
    /// Language name written in that language. System is localized separately.
    pub native_name: &'static str,
}

pub(crate) const LANGUAGE_OPTIONS: &[LanguageOption] = &[
    LanguageOption {
        preference: SYSTEM_PREFERENCE,
        native_name: "System",
    },
    LanguageOption {
        preference: "en-US",
        native_name: "English",
    },
    LanguageOption {
        preference: "zh-CN",
        native_name: "简体中文",
    },
    LanguageOption {
        preference: "ja-JP",
        native_name: "日本語",
    },
    LanguageOption {
        preference: "fr-FR",
        native_name: "Français",
    },
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Locale {
    code: &'static str,
    source: &'static str,
}

const EN_US: Locale = Locale {
    code: "en-US",
    source: include_str!("../i18n/en-US.ftl"),
};
const ZH_CN: Locale = Locale {
    code: "zh-CN",
    source: include_str!("../i18n/zh-CN.ftl"),
};
const JA_JP: Locale = Locale {
    code: "ja-JP",
    source: include_str!("../i18n/ja-JP.ftl"),
};
const FR_FR: Locale = Locale {
    code: "fr-FR",
    source: include_str!("../i18n/fr-FR.ftl"),
};

/// The one registry that drives negotiation, catalog construction and parity
/// tests. Adding a locale is intentionally just a resource, one entry here,
/// and one user-facing entry in `LANGUAGE_OPTIONS`.
const SUPPORTED_LOCALES: &[Locale] = &[EN_US, ZH_CN, JA_JP, FR_FR];

impl Locale {
    fn index(self) -> usize {
        SUPPORTED_LOCALES
            .iter()
            .position(|locale| locale.code == self.code)
            .expect("locale must be registered")
    }

    fn from_code(code: &str) -> Option<Self> {
        SUPPORTED_LOCALES
            .iter()
            .copied()
            .find(|locale| locale.code.eq_ignore_ascii_case(code))
    }
}

struct Catalog {
    bundle: FluentBundle<FluentResource>,
}

struct Catalogs {
    locales: Vec<Catalog>,
}

static CATALOGS: OnceLock<Catalogs> = OnceLock::new();
static ACTIVE_LOCALE: OnceLock<AtomicUsize> = OnceLock::new();
static REPORTED_ERRORS: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

fn build_catalog(locale: Locale) -> Catalog {
    let language_id: LanguageIdentifier = locale
        .code
        .parse()
        .unwrap_or_else(|err| panic!("invalid built-in locale {}: {err}", locale.code));
    let resource =
        FluentResource::try_new(locale.source.to_string()).unwrap_or_else(|(_, errors)| {
            panic!("invalid {} localization resource: {errors:?}", locale.code)
        });
    let mut bundle = FluentBundle::new_concurrent(vec![language_id]);
    bundle.set_use_isolating(false);
    bundle
        .add_resource(resource)
        .unwrap_or_else(|errors| panic!("invalid {} localization bundle: {errors:?}", locale.code));
    Catalog { bundle }
}

fn catalogs() -> &'static Catalogs {
    CATALOGS.get_or_init(|| Catalogs {
        locales: SUPPORTED_LOCALES
            .iter()
            .copied()
            .map(build_catalog)
            .collect(),
    })
}

fn report_once(message: String) {
    let errors = REPORTED_ERRORS.get_or_init(|| Mutex::new(HashSet::new()));
    if errors.lock().insert(message.clone()) {
        log::error!("{message}");
    }
}

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

fn negotiate(requested: impl IntoIterator<Item = String>) -> Locale {
    let requested: Vec<LanguageIdentifier> = requested
        .into_iter()
        .filter_map(|locale| locale.parse().ok())
        .collect();
    let available: Vec<LanguageIdentifier> = SUPPORTED_LOCALES
        .iter()
        .copied()
        .map(|locale| locale.code.parse().expect("built-in locale must parse"))
        .collect();
    let negotiated = negotiate_languages(
        &requested,
        &available,
        Some(&available[EN_US.index()]),
        NegotiationStrategy::Lookup,
    );
    negotiated
        .first()
        .and_then(|locale| Locale::from_code(&locale.to_string()))
        .unwrap_or(EN_US)
}

fn resolve_preference(preference: &str) -> Locale {
    if preference.eq_ignore_ascii_case(SYSTEM_PREFERENCE) {
        return negotiate(sys_locale::get_locales());
    }
    negotiate(std::iter::once(preference.to_string()))
}

fn initial_locale() -> Locale {
    let settings = crate::native_settings::load();
    resolve_preference(configured_preference(&settings))
}

fn active_locale() -> Locale {
    let index = ACTIVE_LOCALE
        .get_or_init(|| AtomicUsize::new(initial_locale().index()))
        .load(Ordering::Acquire);
    SUPPORTED_LOCALES.get(index).copied().unwrap_or(EN_US)
}

pub(crate) fn current_locale() -> &'static str {
    active_locale().code
}

pub(crate) fn activate_preference(preference: &str) -> &'static str {
    let locale = resolve_preference(preference);
    ACTIVE_LOCALE
        .get_or_init(|| AtomicUsize::new(locale.index()))
        .store(locale.index(), Ordering::Release);
    locale.code
}

pub(crate) fn activate_from_settings(settings: &ThinkTermNativeSettings) -> &'static str {
    activate_preference(configured_preference(settings))
}

fn format_for(locale: Locale, id: &'static str, args: Option<&FluentArgs<'_>>) -> Option<String> {
    let catalog = &catalogs().locales[locale.index()];
    let message = catalog.bundle.get_message(id)?;
    let pattern = message.value()?;
    let mut errors = vec![];
    let value = catalog.bundle.format_pattern(pattern, args, &mut errors);
    if !errors.is_empty() {
        report_once(format!(
            "unable to format localization key {id:?} for {}: {errors:?}",
            locale.code
        ));
        return None;
    }
    Some(value.into_owned())
}

fn format(id: &'static str, args: Option<&FluentArgs<'_>>) -> String {
    let locale = active_locale();
    if let Some(value) = format_for(locale, id, args) {
        return value;
    }
    if locale != EN_US {
        report_once(format!(
            "missing localization key {id:?} for {}; using en-US",
            locale.code
        ));
        if let Some(value) = format_for(EN_US, id, args) {
            return value;
        }
    }
    report_once(format!("missing en-US localization key {id:?}"));
    id.to_string()
}

pub(crate) fn tr(id: &'static str) -> String {
    format(id, None)
}

pub(crate) fn tr_args(id: &'static str, args: &FluentArgs<'_>) -> String {
    format(id, Some(args))
}

pub(crate) fn language_option_label(option: LanguageOption) -> String {
    if option.preference == SYSTEM_PREFERENCE {
        tr("language-system")
    } else {
        option.native_name.to_string()
    }
}

pub(crate) fn configured_language_label(settings: &ThinkTermNativeSettings) -> String {
    let preference = configured_preference(settings);
    if preference.eq_ignore_ascii_case(SYSTEM_PREFERENCE) {
        let mut args = FluentArgs::new();
        args.set("language", native_name_for_locale(current_locale()));
        tr_args("language-system-resolved", &args)
    } else {
        native_name_for_locale(resolve_preference(preference).code).to_string()
    }
}

fn native_name_for_locale(locale: &str) -> &'static str {
    LANGUAGE_OPTIONS
        .iter()
        .find(|option| option.preference.eq_ignore_ascii_case(locale))
        .map(|option| option.native_name)
        .unwrap_or("English")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn message_ids(source: &str) -> BTreeSet<String> {
        source
            .lines()
            .filter(|line| !line.starts_with(char::is_whitespace) && !line.starts_with('#'))
            .filter_map(|line| line.split_once('=').map(|(id, _)| id.trim().to_string()))
            .filter(|id| !id.is_empty())
            .collect()
    }

    #[test]
    fn every_shipped_language_has_the_complete_key_set() {
        let english = message_ids(EN_US.source);
        assert!(!english.is_empty());
        for locale in [ZH_CN, JA_JP, FR_FR] {
            assert_eq!(message_ids(locale.source), english, "{}", locale.code);
        }
    }

    #[test]
    fn locale_negotiation_uses_supported_language_families() {
        assert_eq!(negotiate(["zh-Hans-SG".to_string()]), ZH_CN);
        assert_eq!(negotiate(["ja".to_string()]), JA_JP);
        assert_eq!(negotiate(["fr-CA".to_string()]), FR_FR);
        assert_eq!(negotiate(["en-GB".to_string()]), EN_US);
        assert_eq!(negotiate(["de-DE".to_string()]), EN_US);
    }

    #[test]
    fn new_language_setting_overrides_the_legacy_onboarding_value() {
        let mut settings = ThinkTermNativeSettings::default();
        settings.onboarding.language = NativeLanguagePreference::Japanese;
        assert_eq!(configured_preference(&settings), "ja-JP");
        settings.localization.language = Some("fr-FR".to_string());
        assert_eq!(configured_preference(&settings), "fr-FR");
    }

    #[test]
    fn fluent_arguments_can_be_reordered_by_a_translation() {
        let mut args = FluentArgs::new();
        args.set("name", "demo");
        for locale in SUPPORTED_LOCALES.iter().copied() {
            assert!(format_for(locale, "menu-space-occupied", Some(&args))
                .expect("test key")
                .contains("demo"));
        }
    }

    /// The parity test only compares the locale files to each other, so a
    /// tr() call whose key exists in *no* file sails through and renders
    /// as the raw key at runtime. Sweep the source for string-literal
    /// lookups and pin every one to a real en-US entry.
    #[test]
    fn every_translated_string_literal_has_a_definition() {
        let defined: std::collections::HashSet<&str> = EN_US
            .source
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
