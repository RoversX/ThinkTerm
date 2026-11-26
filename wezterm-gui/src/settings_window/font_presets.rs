//! Recommended fallback fonts for Chinese, Japanese and Korean: sets of the
//! families each system ships with (on Linux, the ones distributions
//! package), only those installed here.
//!
//! The four languages share most Han characters but write many of them
//! differently, and a character goes to the first font in the list that
//! has it. So a set puts one language's font first, which then draws the
//! shared characters in that language's forms, and the others' after it
//! for the characters it lacks: kana, Hangul, forms only one language has.

use super::font_picker::FontTarget;
use crate::native_settings::MAX_FONT_FALLBACKS;

/// A language a set gives the shared Han characters the forms of.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum CjkLanguage {
    SimplifiedChinese,
    TraditionalChinese,
    Japanese,
    Korean,
}

/// The order the sets come in when the system names none of the
/// languages, and the order the other languages follow a set's own in.
const DEFAULT_ORDER: [CjkLanguage; 4] = [
    CjkLanguage::SimplifiedChinese,
    CjkLanguage::Japanese,
    CjkLanguage::TraditionalChinese,
    CjkLanguage::Korean,
];

impl CjkLanguage {
    /// The key of the set's name in the catalogues.
    pub(super) fn label_key(self) -> &'static str {
        match self {
            Self::SimplifiedChinese => "settings-font-preset-zh-hans",
            Self::TraditionalChinese => "settings-font-preset-zh-hant",
            Self::Japanese => "settings-font-preset-ja",
            Self::Korean => "settings-font-preset-ko",
        }
    }

    /// The language of a locale such as "zh-Hant-TW", "zh_TW.UTF-8",
    /// "yue-Hant-HK" or "ja-JP", when it is one of these.
    fn of_locale(locale: &str) -> Option<Self> {
        let locale = locale.to_ascii_lowercase();
        let mut parts = locale.split(['-', '_', '.', '@']);
        let language = parts.next()?;
        let rest: Vec<&str> = parts.collect();
        // Cantonese, under its own code or as zh-yue, is written in
        // traditional characters unless it says otherwise.
        let cantonese = language == "yue" || rest.contains(&"yue");
        match language {
            "zh" | "cmn" | "yue" => {
                // A script named wins over a region: zh-Hans-HK is simplified.
                if rest.contains(&"hans") {
                    Some(Self::SimplifiedChinese)
                } else if cantonese
                    || rest
                        .iter()
                        .any(|part| matches!(*part, "hant" | "tw" | "hk" | "mo"))
                {
                    Some(Self::TraditionalChinese)
                } else {
                    Some(Self::SimplifiedChinese)
                }
            }
            "ja" => Some(Self::Japanese),
            "ko" => Some(Self::Korean),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum System {
    Mac,
    Windows,
    Unix,
}

const SYSTEM: System = if cfg!(target_os = "macos") {
    System::Mac
} else if cfg!(windows) {
    System::Windows
} else {
    System::Unix
};

/// The families that give `language` its forms on `system`, the better
/// ones first: a set takes the first one installed.
fn candidates(
    system: System,
    target: FontTarget,
    language: CjkLanguage,
) -> &'static [&'static str] {
    use CjkLanguage::*;
    use FontTarget::*;
    match (system, language) {
        // PingFang is a download macOS makes on demand; Hiragino Sans GB
        // and CNS ship with it. Neither system has fixed-width ones, so
        // the terminal takes the same as the interface.
        (System::Mac, SimplifiedChinese) => &["PingFang SC", "Hiragino Sans GB"],
        (System::Mac, TraditionalChinese) => &["PingFang TC", "Hiragino Sans CNS"],
        (System::Mac, Japanese) => &["Hiragino Sans", "Hiragino Kaku Gothic ProN"],
        (System::Mac, Korean) => &["Apple SD Gothic Neo"],
        // The UI faces are the ones Windows draws its own interface with,
        // with tighter lines; the terminal takes the text faces.
        (System::Windows, SimplifiedChinese) => match target {
            Interface => &["Microsoft YaHei UI", "Microsoft YaHei"],
            Terminal => &["Microsoft YaHei"],
        },
        (System::Windows, TraditionalChinese) => match target {
            Interface => &["Microsoft JhengHei UI", "Microsoft JhengHei"],
            Terminal => &["Microsoft JhengHei"],
        },
        // Yu Gothic's strokes are thin on most screens; BIZ UDGothic, in
        // Windows since 10 version 1809, is fixed-width and holds up small.
        (System::Windows, Japanese) => match target {
            Interface => &["Yu Gothic UI", "Meiryo UI"],
            Terminal => &["BIZ UDGothic", "Meiryo"],
        },
        (System::Windows, Korean) => &["Malgun Gothic"],
        // Noto Sans CJK is the one most distributions package; its Mono
        // faces are the terminal's. Source Han Sans is the same design
        // under Adobe's names, by language and by region.
        (System::Unix, SimplifiedChinese) => match target {
            Interface => &[
                "Noto Sans CJK SC",
                "Source Han Sans SC",
                "Source Han Sans CN",
                "WenQuanYi Micro Hei",
                "WenQuanYi Zen Hei",
            ],
            Terminal => &[
                "Noto Sans Mono CJK SC",
                "Noto Sans CJK SC",
                "Source Han Sans SC",
                "Source Han Sans CN",
                "WenQuanYi Micro Hei Mono",
                "WenQuanYi Zen Hei Mono",
            ],
        },
        (System::Unix, TraditionalChinese) => match target {
            Interface => &[
                "Noto Sans CJK TC",
                "Source Han Sans TC",
                "Source Han Sans TW",
            ],
            Terminal => &[
                "Noto Sans Mono CJK TC",
                "Noto Sans CJK TC",
                "Source Han Sans TC",
                "Source Han Sans TW",
            ],
        },
        (System::Unix, Japanese) => match target {
            Interface => &[
                "Noto Sans CJK JP",
                "Source Han Sans JP",
                "Source Han Sans",
                "IPAPGothic",
            ],
            Terminal => &[
                "Noto Sans Mono CJK JP",
                "Noto Sans CJK JP",
                "Source Han Sans JP",
                "Source Han Sans",
                "IPAGothic",
            ],
        },
        (System::Unix, Korean) => match target {
            Interface => &[
                "Noto Sans CJK KR",
                "Source Han Sans KR",
                "Source Han Sans K",
                "NanumGothic",
            ],
            Terminal => &[
                "Noto Sans Mono CJK KR",
                "Noto Sans CJK KR",
                "Source Han Sans KR",
                "Source Han Sans K",
                "D2Coding",
                "NanumGothicCoding",
            ],
        },
    }
}

/// One recommended set: the fallback fonts it puts in a list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct FontPreset {
    pub language: CjkLanguage,
    /// Installed families, spelled as the catalogue has them, `language`'s
    /// first.
    pub families: Vec<String>,
}

/// Whether `family` is one a set may put in a list here, for either font:
/// applying a set takes it out of the list, as the set says which of them
/// come first.
fn recommended_on(system: System, family: &str) -> bool {
    DEFAULT_ORDER.iter().any(|&language| {
        [FontTarget::Interface, FontTarget::Terminal]
            .iter()
            .any(|&target| {
                candidates(system, target, language)
                    .iter()
                    .any(|candidate| candidate.eq_ignore_ascii_case(family.trim()))
            })
    })
}

/// The fallbacks a list has once the set `set` is applied to `fallbacks`:
/// the set's families first, then the list's other fonts as they were,
/// symbols, emoji or any other language's, without the ones a set takes.
/// Those other fonts are the list's owner's and all stay: where they leave
/// no room for the whole set, it gives up its last families, and where
/// they leave none at all, the list is as it was.
pub(super) fn with_set(set: &[String], fallbacks: &[String]) -> Vec<String> {
    with_set_on(SYSTEM, set, fallbacks, MAX_FONT_FALLBACKS)
}

fn with_set_on(system: System, set: &[String], fallbacks: &[String], most: usize) -> Vec<String> {
    let in_set = |family: &String| set.iter().any(|ours| ours.eq_ignore_ascii_case(family));
    let kept: Vec<String> = fallbacks
        .iter()
        .filter(|family| !in_set(family) && !recommended_on(system, family))
        .cloned()
        .collect();
    let room = most.saturating_sub(kept.len());
    if room == 0 {
        return fallbacks.to_vec();
    }
    set.iter().take(room).cloned().chain(kept).collect()
}

/// The sets for `target` of the families in `installed` (the catalogue),
/// the language of the first of `locales` that is one of them first. A
/// language with none of its fonts installed has no set.
pub(super) fn font_presets<S: AsRef<str>>(
    target: FontTarget,
    installed: &[String],
    locales: &[S],
) -> Vec<FontPreset> {
    presets_for(SYSTEM, target, installed, locales)
}

fn presets_for<S: AsRef<str>>(
    system: System,
    target: FontTarget,
    installed: &[String],
    locales: &[S],
) -> Vec<FontPreset> {
    // The loader takes a family by its exact name, so the catalogue's
    // spelling is the one to keep.
    let font_of = |language: CjkLanguage| {
        candidates(system, target, language)
            .iter()
            .find_map(|wanted| {
                installed
                    .iter()
                    .find(|name| name.eq_ignore_ascii_case(wanted))
            })
            .cloned()
    };
    let fonts: Vec<(CjkLanguage, Option<String>)> = DEFAULT_ORDER
        .iter()
        .map(|&language| (language, font_of(language)))
        .collect();
    let first = locales
        .iter()
        .find_map(|locale| CjkLanguage::of_locale(locale.as_ref()));
    let order = first.into_iter().chain(
        DEFAULT_ORDER
            .iter()
            .copied()
            .filter(|&language| Some(language) != first),
    );
    order
        .filter_map(|language| {
            let font = |of: CjkLanguage| {
                fonts
                    .iter()
                    .find(|(listed, _)| *listed == of)
                    .and_then(|(_, font)| font.clone())
            };
            let mut families = vec![font(language)?];
            for (other, _) in fonts.iter().filter(|(other, _)| *other != language) {
                if let Some(family) = font(*other) {
                    if !families
                        .iter()
                        .any(|listed| listed.eq_ignore_ascii_case(&family))
                    {
                        families.push(family);
                    }
                }
            }
            Some(FontPreset { language, families })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use CjkLanguage::*;

    fn installed(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| name.to_string()).collect()
    }

    fn languages(presets: &[FontPreset]) -> Vec<CjkLanguage> {
        presets.iter().map(|preset| preset.language).collect()
    }

    #[test]
    fn locales_name_their_language() {
        for (locale, language) in [
            ("zh-CN", Some(SimplifiedChinese)),
            ("zh_CN.UTF-8", Some(SimplifiedChinese)),
            ("zh-Hans-HK", Some(SimplifiedChinese)),
            ("zh", Some(SimplifiedChinese)),
            ("zh-TW", Some(TraditionalChinese)),
            ("zh_HK", Some(TraditionalChinese)),
            ("zh-Hant", Some(TraditionalChinese)),
            ("ja-JP", Some(Japanese)),
            ("ko_KR.UTF-8", Some(Korean)),
            ("yue-Hant-HK", Some(TraditionalChinese)),
            ("yue", Some(TraditionalChinese)),
            ("yue-Hans", Some(SimplifiedChinese)),
            ("zh-yue", Some(TraditionalChinese)),
            ("cmn-Hans-CN", Some(SimplifiedChinese)),
            ("en-US", None),
            ("zu", None),
        ] {
            assert_eq!(CjkLanguage::of_locale(locale), language, "{locale}");
        }
    }

    #[test]
    fn the_first_language_the_system_names_comes_first() {
        let fonts = installed(&[
            "Noto Sans CJK JP",
            "Noto Sans CJK KR",
            "Noto Sans CJK SC",
            "Noto Sans CJK TC",
        ]);
        let none: [&str; 0] = [];
        let order = |locales: &[&str]| {
            languages(&presets_for(
                System::Unix,
                FontTarget::Interface,
                &fonts,
                locales,
            ))
        };
        assert_eq!(
            order(&none),
            [SimplifiedChinese, Japanese, TraditionalChinese, Korean]
        );
        assert_eq!(
            order(&["en-US", "ja-JP", "zh-CN"]),
            [Japanese, SimplifiedChinese, TraditionalChinese, Korean]
        );
        assert_eq!(
            order(&["zh-TW"]),
            [TraditionalChinese, SimplifiedChinese, Japanese, Korean]
        );
        assert_eq!(
            order(&["ko-KR"]),
            [Korean, SimplifiedChinese, Japanese, TraditionalChinese]
        );
    }

    #[test]
    fn a_set_puts_its_language_first_and_the_others_after() {
        let fonts = installed(&["Noto Sans CJK JP", "Noto Sans CJK SC", "Noto Sans CJK KR"]);
        let presets = presets_for(System::Unix, FontTarget::Interface, &fonts, &["ja-JP"]);
        assert_eq!(
            presets[0],
            FontPreset {
                language: Japanese,
                families: installed(&["Noto Sans CJK JP", "Noto Sans CJK SC", "Noto Sans CJK KR"]),
            }
        );
        assert_eq!(
            presets[1].families,
            installed(&["Noto Sans CJK SC", "Noto Sans CJK JP", "Noto Sans CJK KR"])
        );
        // No Traditional Chinese font installed: no set for it.
        assert_eq!(languages(&presets), [Japanese, SimplifiedChinese, Korean]);
    }

    #[test]
    fn the_terminal_takes_fixed_width_faces_where_there_are_some() {
        let fonts = installed(&["Noto Sans CJK SC", "Noto Sans Mono CJK SC"]);
        let none: [&str; 0] = [];
        let terminal = presets_for(System::Unix, FontTarget::Terminal, &fonts, &none);
        let interface = presets_for(System::Unix, FontTarget::Interface, &fonts, &none);
        assert_eq!(terminal[0].families, ["Noto Sans Mono CJK SC"]);
        assert_eq!(interface[0].families, ["Noto Sans CJK SC"]);
    }

    #[test]
    fn a_set_takes_the_first_candidate_installed_in_the_catalogues_spelling() {
        // PingFang not downloaded yet: the font that ships stands in.
        let fonts = installed(&["Hiragino Sans GB", "hiragino sans", "Apple SD Gothic Neo"]);
        let presets = presets_for(System::Mac, FontTarget::Terminal, &fonts, &["zh-Hans-CN"]);
        assert_eq!(
            presets[0].families,
            ["Hiragino Sans GB", "hiragino sans", "Apple SD Gothic Neo"]
        );
        let fonts = installed(&["PingFang SC", "Hiragino Sans GB"]);
        let presets = presets_for(System::Mac, FontTarget::Terminal, &fonts, &["zh-CN"]);
        assert_eq!(presets[0].families, ["PingFang SC"]);
    }

    #[test]
    fn windows_sets_take_the_interface_and_text_faces_apart() {
        let fonts = installed(&[
            "BIZ UDGothic",
            "Malgun Gothic",
            "Meiryo",
            "Microsoft YaHei",
            "Microsoft YaHei UI",
            "Yu Gothic UI",
        ]);
        let interface = presets_for(System::Windows, FontTarget::Interface, &fonts, &["ja-JP"]);
        let terminal = presets_for(System::Windows, FontTarget::Terminal, &fonts, &["ja-JP"]);
        assert_eq!(
            interface[0].families,
            ["Yu Gothic UI", "Microsoft YaHei UI", "Malgun Gothic"]
        );
        assert_eq!(
            terminal[0].families,
            ["BIZ UDGothic", "Microsoft YaHei", "Malgun Gothic"]
        );
    }

    #[test]
    fn a_set_goes_first_and_keeps_the_lists_other_fonts() {
        let set = installed(&["Noto Sans Mono CJK JP", "Noto Sans Mono CJK SC"]);
        let fallbacks = installed(&[
            "Noto Sans CJK SC",
            "Symbols Nerd Font Mono",
            "noto sans mono cjk jp",
            "LXGW WenKai Mono",
            "Noto Color Emoji",
        ]);
        assert_eq!(
            with_set_on(System::Unix, &set, &fallbacks, 8),
            [
                "Noto Sans Mono CJK JP",
                "Noto Sans Mono CJK SC",
                "Symbols Nerd Font Mono",
                "LXGW WenKai Mono",
                "Noto Color Emoji",
            ],
            "another set's font goes, and so does the set's own in another case"
        );
        // What a set may take differs by system.
        assert_eq!(
            with_set_on(System::Mac, &installed(&["PingFang SC"]), &fallbacks, 8),
            [
                "PingFang SC",
                "Noto Sans CJK SC",
                "Symbols Nerd Font Mono",
                "noto sans mono cjk jp",
                "LXGW WenKai Mono",
                "Noto Color Emoji",
            ]
        );
    }

    #[test]
    fn a_full_list_keeps_its_own_fonts_and_trims_the_set() {
        let set = installed(&["JP", "SC", "TC", "KR"]);
        let own = installed(&["A", "B", "C", "D", "E", "Symbols", "Emoji"]);
        // One place left: the set's first family only.
        assert_eq!(
            with_set_on(System::Unix, &set, &own, 8),
            installed(&["JP", "A", "B", "C", "D", "E", "Symbols", "Emoji"])
        );
        // A font a set takes frees its place for the set.
        let mut with_cjk = own.clone();
        with_cjk.insert(0, "Noto Sans CJK SC".to_string());
        assert_eq!(
            with_set_on(System::Unix, &set, &with_cjk, 8),
            installed(&["JP", "A", "B", "C", "D", "E", "Symbols", "Emoji"])
        );
        // No place at all: the list stays as it was.
        let mut full = own.clone();
        full.push("F".to_string());
        assert_eq!(with_set_on(System::Unix, &set, &full, 8), full);
    }

    #[test]
    fn nothing_installed_makes_no_sets() {
        let none: [&str; 0] = [];
        assert!(presets_for(
            System::Unix,
            FontTarget::Terminal,
            &installed(&["DejaVu Sans"]),
            &none
        )
        .is_empty());
    }
}
