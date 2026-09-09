//! ThinkTerm's interface strings. The catalogues are Fluent files, one per
//! locale, compiled into the binary; a process has one active locale, set
//! from a preference (`"system"` or a tag) plus the platform's own list of
//! languages. Lookups fall back to en-US and report a missing key once.

use fluent_bundle::concurrent::FluentBundle;
use fluent_bundle::FluentResource;
pub use fluent_bundle::{FluentArgs, FluentValue};
use fluent_langneg::negotiate_languages;
use fluent_langneg::NegotiationStrategy;
use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};
use unic_langid::LanguageIdentifier;

/// The preference that means "whatever the platform prefers".
pub const SYSTEM_PREFERENCE: &str = "system";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LanguageOption {
    /// Stable value persisted in a client's settings.
    pub preference: &'static str,
    /// Language name written in that language. System is localized separately.
    pub native_name: &'static str,
}

pub const LANGUAGE_OPTIONS: &[LanguageOption] = &[
    LanguageOption { preference: SYSTEM_PREFERENCE, native_name: "System" },
    LanguageOption { preference: "en-US", native_name: "English" },
    LanguageOption { preference: "zh-CN", native_name: "简体中文" },
    LanguageOption { preference: "ja-JP", native_name: "日本語" },
    LanguageOption { preference: "fr-FR", native_name: "Français" },
    LanguageOption { preference: "de-DE", native_name: "Deutsch" },
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Locale {
    code: &'static str,
    source: &'static str,
}

const EN_US: Locale = Locale { code: "en-US", source: include_str!("../i18n/en-US.ftl") };
const ZH_CN: Locale = Locale { code: "zh-CN", source: include_str!("../i18n/zh-CN.ftl") };
const JA_JP: Locale = Locale { code: "ja-JP", source: include_str!("../i18n/ja-JP.ftl") };
const FR_FR: Locale = Locale { code: "fr-FR", source: include_str!("../i18n/fr-FR.ftl") };
const DE_DE: Locale = Locale { code: "de-DE", source: include_str!("../i18n/de-DE.ftl") };

/// The one registry that drives negotiation, catalogue construction and
/// the parity test. Adding a locale is a resource, one entry here, and one
/// user-facing entry in `LANGUAGE_OPTIONS`.
const SUPPORTED_LOCALES: &[Locale] = &[EN_US, ZH_CN, JA_JP, FR_FR, DE_DE];

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

/// The codes of the shipped locales, en-US first.
pub fn locale_codes() -> impl Iterator<Item = &'static str> {
    SUPPORTED_LOCALES.iter().map(|locale| locale.code)
}

/// The en-US catalogue's text, for tools that sweep a source tree for keys.
pub fn en_us_source() -> &'static str {
    EN_US.source
}

struct Catalog {
    bundle: FluentBundle<FluentResource>,
}

struct Catalogs {
    locales: Vec<Catalog>,
}

static CATALOGS: OnceLock<Catalogs> = OnceLock::new();
static ACTIVE_LOCALE: OnceLock<AtomicUsize> = OnceLock::new();
static INITIAL: OnceLock<fn() -> &'static str> = OnceLock::new();
static REPORTED_ERRORS: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

fn build_catalog(locale: Locale) -> Catalog {
    let language_id: LanguageIdentifier = locale
        .code
        .parse()
        .unwrap_or_else(|err| panic!("invalid built-in locale {}: {err}", locale.code));
    let resource = FluentResource::try_new(locale.source.to_string())
        .unwrap_or_else(|(_, errors)| panic!("invalid {} localization resource: {errors:?}", locale.code));
    let mut bundle = FluentBundle::new_concurrent(vec![language_id]);
    bundle.set_use_isolating(false);
    bundle
        .add_resource(resource)
        .unwrap_or_else(|errors| panic!("invalid {} localization bundle: {errors:?}", locale.code));
    Catalog { bundle }
}

fn catalogs() -> &'static Catalogs {
    CATALOGS.get_or_init(|| Catalogs {
        locales: SUPPORTED_LOCALES.iter().copied().map(build_catalog).collect(),
    })
}

fn report_once(message: String) {
    let errors = REPORTED_ERRORS.get_or_init(|| Mutex::new(HashSet::new()));
    let mut errors = errors.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if errors.insert(message.clone()) {
        log::error!("{message}");
    }
}

fn negotiate_locale(requested: &[String]) -> Locale {
    let requested: Vec<LanguageIdentifier> = requested.iter().filter_map(|locale| locale.parse().ok()).collect();
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

/// The shipped locale nearest the requested ones (`"zh-Hans-SG"` → zh-CN,
/// `"en-GB"` → en-US), en-US when none is close.
pub fn negotiate(requested: &[String]) -> &'static str {
    negotiate_locale(requested).code
}

fn resolve(preference: &str, system: &[String]) -> Locale {
    if preference.eq_ignore_ascii_case(SYSTEM_PREFERENCE) {
        return negotiate_locale(system);
    }
    negotiate_locale(&[preference.to_string()])
}

/// The locale a preference would pick, given the platform's languages.
pub fn resolve_preference(preference: &str, system: &[String]) -> &'static str {
    resolve(preference, system).code
}

/// What the locale is before anyone activates one: a client registers
/// this at startup (the desktop reads its settings file) so a lookup that
/// comes first still gets the right language. Without it, en-US.
pub fn set_initial_resolver(resolver: fn() -> &'static str) {
    let _ = INITIAL.set(resolver);
}

fn active_locale() -> Locale {
    let index = ACTIVE_LOCALE
        .get_or_init(|| {
            let code = INITIAL.get().map(|resolve| resolve()).unwrap_or(EN_US.code);
            AtomicUsize::new(Locale::from_code(code).unwrap_or(EN_US).index())
        })
        .load(Ordering::Acquire);
    SUPPORTED_LOCALES.get(index).copied().unwrap_or(EN_US)
}

pub fn current_locale() -> &'static str {
    active_locale().code
}

/// Make a preference the process's locale; returns the locale it resolved to.
pub fn activate_preference(preference: &str, system: &[String]) -> &'static str {
    let locale = resolve(preference, system);
    ACTIVE_LOCALE
        .get_or_init(|| AtomicUsize::new(locale.index()))
        .store(locale.index(), Ordering::Release);
    locale.code
}

fn format_for(locale: Locale, id: &str, args: Option<&FluentArgs<'_>>) -> Option<String> {
    let catalog = &catalogs().locales[locale.index()];
    let message = catalog.bundle.get_message(id)?;
    let pattern = message.value()?;
    let mut errors = vec![];
    let value = catalog.bundle.format_pattern(pattern, args, &mut errors);
    if !errors.is_empty() {
        report_once(format!("unable to format localization key {id:?} for {}: {errors:?}", locale.code));
        return None;
    }
    Some(value.into_owned())
}

fn format(id: &str, args: Option<&FluentArgs<'_>>) -> String {
    let locale = active_locale();
    if let Some(value) = format_for(locale, id, args) {
        return value;
    }
    if locale != EN_US {
        report_once(format!("missing localization key {id:?} for {}; using en-US", locale.code));
        if let Some(value) = format_for(EN_US, id, args) {
            return value;
        }
    }
    report_once(format!("missing en-US localization key {id:?}"));
    id.to_string()
}

pub fn tr(id: &str) -> String {
    format(id, None)
}

pub fn tr_args(id: &str, args: &FluentArgs<'_>) -> String {
    format(id, Some(args))
}

/// Whether the en-US catalogue defines `id` (with a value).
pub fn has_key(id: &str) -> bool {
    catalogs().locales[EN_US.index()]
        .bundle
        .get_message(id)
        .is_some_and(|message| message.value().is_some())
}

pub fn language_option_label(option: LanguageOption) -> String {
    if option.preference == SYSTEM_PREFERENCE {
        tr("language-system")
    } else {
        option.native_name.to_string()
    }
}

pub fn native_name_for_locale(locale: &str) -> &'static str {
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
        for locale in SUPPORTED_LOCALES.iter().copied().filter(|l| *l != EN_US) {
            let ids = message_ids(locale.source);
            let missing: Vec<_> = english.difference(&ids).collect();
            let extra: Vec<_> = ids.difference(&english).collect();
            assert!(
                missing.is_empty() && extra.is_empty(),
                "{}: missing {missing:?}, extra {extra:?}",
                locale.code
            );
        }
    }

    #[test]
    fn locale_negotiation_uses_supported_language_families() {
        let one = |s: &str| vec![s.to_string()];
        assert_eq!(negotiate(&one("zh-Hans-SG")), "zh-CN");
        assert_eq!(negotiate(&one("ja")), "ja-JP");
        assert_eq!(negotiate(&one("fr-CA")), "fr-FR");
        assert_eq!(negotiate(&one("en-GB")), "en-US");
        assert_eq!(negotiate(&one("de-AT")), "de-DE");
        assert_eq!(negotiate(&one("xx")), "en-US");
        assert_eq!(resolve_preference("system", &one("fr")), "fr-FR");
        assert_eq!(resolve_preference("ja-JP", &one("fr")), "ja-JP");
    }

    #[test]
    fn every_language_option_is_a_shipped_locale() {
        for option in LANGUAGE_OPTIONS.iter().filter(|o| o.preference != SYSTEM_PREFERENCE) {
            assert!(Locale::from_code(option.preference).is_some(), "{}", option.preference);
        }
    }

    #[test]
    fn fluent_arguments_can_be_reordered_by_a_translation() {
        let mut args = FluentArgs::new();
        args.set("name", "demo");
        for locale in SUPPORTED_LOCALES.iter().copied() {
            assert!(format_for(locale, "menu-space-occupied", Some(&args)).expect("test key").contains("demo"));
        }
    }
}
