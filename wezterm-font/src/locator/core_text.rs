#![cfg(target_os = "macos")]
#![allow(unexpected_cfgs)] // <https://github.com/SSheldon/rust-objc/issues/125>

use crate::locator::{FontDataSource, FontLocator, FontOrigin};
use crate::parser::ParsedFont;
use cocoa::base::id;
use config::{FontAttributes, FontStretch, FontStyle, FontWeight};
use core_foundation::array::{CFArray, CFArrayRef};
use core_foundation::base::{CFRange, TCFType};
use core_foundation::dictionary::CFDictionary;
use core_foundation::string::{CFString, CFStringRef};
use core_text::font::*;
use core_text::font_descriptor::*;
use objc::*;
use rangeset::RangeSet;
use std::cmp::Ordering;
use std::collections::HashSet;

lazy_static::lazy_static! {
    static ref FALLBACK: Vec<ParsedFont> = build_fallback_list();
    static ref SIMPLIFIED_CHINESE: Vec<ParsedFont> = simplified_chinese_fonts();
    static ref PINGFANG_MISSING: bool = pingfang_missing();
}

/// Whether this Mac has no PingFang to draw Chinese with while Chinese is
/// what it reads (`prefers_simplified_chinese`): Han characters then come
/// from Hiragino Sans GB, and the user may want to download PingFang.
/// Found out once.
pub fn simplified_chinese_font_missing() -> bool {
    *PINGFANG_MISSING
}

#[link(name = "CoreText", kind = "framework")]
extern "C" {
    fn CTFontCreateForString(
        currentFont: CTFontRef,
        string: CFStringRef,
        range: CFRange,
    ) -> CTFontRef;
}

/// A FontLocator implemented using the system font loading
/// functions provided by core text.
pub struct CoreTextFontLocator {}

fn descriptor_from_attr(attr: &FontAttributes) -> anyhow::Result<CFArray<CTFontDescriptor>> {
    let family_name = attr
        .family
        .parse::<CFString>()
        .map_err(|_| anyhow::anyhow!("failed to parse family name {} as CFString", attr.family))?;

    let family_attr: CFString = unsafe { TCFType::wrap_under_get_rule(kCTFontFamilyNameAttribute) };

    let attributes = CFDictionary::from_CFType_pairs(&[(family_attr, family_name.as_CFType())]);
    let desc = core_text::font_descriptor::new_from_attributes(&attributes);

    let array = unsafe {
        core_text::font_descriptor::CTFontDescriptorCreateMatchingFontDescriptors(
            desc.as_concrete_TypeRef(),
            std::ptr::null(),
        )
    };
    if array.is_null() {
        anyhow::bail!("no font matches {:?}", attr);
    } else {
        Ok(unsafe { CFArray::wrap_under_create_rule(array) })
    }
}

/// Given a descriptor, return a handle that can be used to open it.
/// The descriptor may not refer to an on-disk font and thus may
/// not have a path.
/// In addition, it may point to a ttc; so we'll need to reference
/// each contained font to figure out which one is the one that
/// the descriptor is referencing.
fn handles_from_descriptor(descriptor: &CTFontDescriptor) -> Vec<ParsedFont> {
    let mut result = vec![];
    if let Some(path) = descriptor.font_path() {
        let source = FontDataSource::OnDisk(path);
        let _ =
            crate::parser::parse_and_collect_font_info(&source, &mut result, FontOrigin::CoreText);
    }

    result
}

impl FontLocator for CoreTextFontLocator {
    fn load_fonts(
        &self,
        fonts_selection: &[FontAttributes],
        loaded: &mut HashSet<FontAttributes>,
        pixel_size: u16,
    ) -> anyhow::Result<Vec<ParsedFont>> {
        let mut fonts = vec![];

        for attr in fonts_selection {
            match descriptor_from_attr(attr) {
                Ok(descriptors) => {
                    let mut handles = vec![];
                    for descriptor in descriptors.iter() {
                        handles.append(&mut handles_from_descriptor(&descriptor));
                    }
                    log::trace!("core text matched {:?} to {:#?}", attr, handles);

                    // If we got a series of .ttc files, we may have a selection of
                    // different font families.  Let's make a first pass a limit
                    // ourselves to name matches
                    let name_matches: Vec<_> = handles
                        .iter()
                        .filter_map(|p| {
                            if p.matches_name(attr) {
                                Some(p.clone())
                            } else {
                                None
                            }
                        })
                        .collect();
                    if !name_matches.is_empty() {
                        handles = name_matches;
                    }

                    if let Some(parsed) = ParsedFont::best_match(attr, pixel_size, handles) {
                        log::trace!("best match from core text is {:?}", parsed);
                        fonts.push(parsed);
                        loaded.insert(attr.clone());
                    }
                }
                Err(err) => log::trace!("load_fonts: descriptor_from_attr: {:#}", err),
            }
        }

        Ok(fonts)
    }

    fn locate_fallback_for_codepoints(
        &self,
        codepoints: &[char],
    ) -> anyhow::Result<Vec<ParsedFont>> {
        let mut matches = vec![];

        let menlo =
            new_from_name("Menlo", 0.0).map_err(|_| anyhow::anyhow!("failed to get Menlo font"))?;

        for &c in codepoints {
            let mut wanted = RangeSet::new();
            wanted.add(c as u32);
            let text = CFString::new(&c.to_string());

            let font = unsafe {
                CTFontCreateForString(
                    menlo.as_concrete_TypeRef(),
                    text.as_concrete_TypeRef(),
                    CFRange::init(0, 1),
                )
            };

            if font.is_null() {
                continue;
            }

            let font = unsafe { CTFont::wrap_under_create_rule(font) };

            let candidates = handles_from_descriptor(&font.copy_descriptor());

            let mut matched_any = false;

            for font in candidates {
                if font.names().family == ".LastResort"
                    || font.names().postscript_name.as_deref() == Some("LastResort")
                {
                    // Always exclude a last resort font, as it has
                    // placeholder glyphs for everything
                    continue;
                }

                let is_normal = font.weight() == FontWeight::REGULAR
                    && font.stretch() == FontStretch::Normal
                    && font.style() == FontStyle::Normal;
                if !is_normal {
                    // Only use normal attributed text for fallbacks,
                    // otherwise we'll end up picking something with
                    // undefined and undesirable attributes
                    // <https://github.com/wezterm/wezterm/issues/4808>
                    continue;
                }

                if let Ok(cov) = font.coverage_intersection(&wanted) {
                    // Explicitly check coverage because the list may not
                    // actually match the text we asked about(!)
                    if !cov.is_empty() {
                        matches.push((cov.len(), font));
                        matched_any = true;
                    }
                }
            }

            if !matched_any {
                // Consult our global, more general list of fallbacks
                for font in FALLBACK.iter() {
                    if let Ok(cov) = font.coverage_intersection(&wanted) {
                        if !cov.is_empty() {
                            matches.push((cov.len(), font.clone()));
                        }
                    }
                }
            }
        }

        // Add the handles in order of descending coverage; the idea being
        // that if a font has a large coverage then it is probably a better
        // candidate and more likely to result in other glyphs matching
        // in future shaping calls.
        let mut wanted = RangeSet::new();
        for &c in codepoints {
            wanted.add(c as u32);
        }
        let mut uncovered = wanted.clone();
        for (cov_len, font) in &mut matches {
            if let Ok(cov) = font.coverage_intersection(&wanted) {
                *cov_len = cov.len();
                uncovered = uncovered.difference(&cov);
            }
        }

        matches.sort_by(|(a_len, a), (b_len, b)| {
            let primary = a_len.cmp(&b_len).reverse();
            if primary == Ordering::Equal {
                a.cmp(b)
            } else {
                primary
            }
        });
        matches.dedup();

        // Han characters: from Hiragino Sans GB where PingFang is missing
        // and Chinese is what this Mac reads -- all of them and first, with
        // the punctuation and kana around them, or a line would mix it with
        // the Japanese or Korean font the system offers for what those
        // share (a run of only punctuation loads that font for the Han
        // characters that follow). Otherwise only Han characters no font
        // above covers, and last, so that it changes nothing where PingFang
        // is installed and takes no character another font draws.
        let all_han = *PINGFANG_MISSING;
        let mut han = RangeSet::new();
        for &c in codepoints {
            let c = c as u32;
            let wanted = if all_han {
                is_han(c) || is_chinese_punctuation_or_kana(c)
            } else {
                is_han(c) && uncovered.contains(c)
            };
            if wanted {
                han.add(c);
            }
        }
        if !han.is_empty() {
            let found: Vec<(u32, ParsedFont)> = SIMPLIFIED_CHINESE
                .iter()
                .filter_map(|font| {
                    let cov = font.coverage_intersection(&han).ok()?;
                    (!cov.is_empty()).then(|| (cov.len(), font.clone()))
                })
                .collect();
            matches.retain(|(_, font)| !found.iter().any(|(_, han_font)| han_font == font));
            if all_han {
                matches.splice(0..0, found);
            } else {
                matches.extend(found);
            }
        }

        log::trace!(
            "found {} fallback candidates for {} codepoints",
            matches.len(),
            codepoints.len()
        );

        Ok(matches.into_iter().map(|(_len, handle)| handle).collect())
    }

    fn enumerate_family_names(&self) -> anyhow::Result<Vec<String>> {
        Ok(core_text::font_collection::get_family_names()
            .iter()
            .map(|name| name.to_string())
            .collect())
    }

    fn enumerate_all_fonts(&self) -> anyhow::Result<Vec<ParsedFont>> {
        let mut fonts = vec![];

        let collection = core_text::font_collection::create_for_all_families();
        if let Some(descriptors) = collection.get_descriptors() {
            for descriptor in descriptors.iter() {
                fonts.append(&mut handles_from_descriptor(&descriptor));
            }
        }

        fonts.sort();
        fonts.dedup();
        Ok(fonts)
    }
}

/// Simplified Chinese where the system's own choice cannot be had. PingFang,
/// which CoreText answers Chinese with, is a font asset macOS downloads on
/// demand; until it has, every font left in the cascade is Japanese or
/// Korean, whose Han characters lack the simplified-only forms, and those
/// drew as boxes. Hiragino Sans GB ships in /System/Library/Fonts and has
/// them all; its text face, W3, says it weighs 300, which is why it is not
/// in the regular-weight list `build_fallback_list` makes.
fn simplified_chinese_fonts() -> Vec<ParsedFont> {
    let attributes = FontAttributes::new_fallback("Hiragino Sans GB");
    let Ok(descriptors) = descriptor_from_attr(&attributes) else {
        return vec![];
    };
    let mut fonts: Vec<ParsedFont> = descriptors
        .iter()
        .flat_map(|descriptor| handles_from_descriptor(&descriptor))
        .filter(|font| font.names().full_name == "Hiragino Sans GB W3")
        .collect();
    fonts.sort();
    fonts.dedup();
    fonts
}

/// Whether `c` is a CJK unified or compatibility ideograph.
fn is_han(c: u32) -> bool {
    matches!(
        c,
        0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xF900..=0xFAFF | 0x20000..=0x323AF
    )
}

/// CJK symbols and punctuation, kana, and full-width forms.
fn is_chinese_punctuation_or_kana(c: u32) -> bool {
    matches!(c, 0x3000..=0x30FF | 0xFF00..=0xFFEF)
}

/// The languages this Mac's user reads, most preferred first; none when the
/// defaults name none.
fn preferred_languages() -> anyhow::Result<CFArray<CFString>> {
    let user_defaults: id = unsafe { msg_send![class!(NSUserDefaults), standardUserDefaults] };

    let apple_lang = "AppleLanguages"
        .parse::<CFString>()
        .map_err(|_| anyhow::anyhow!("failed to parse lang name en as CFString"))?;

    // The defaults keep the array: it is borrowed, not handed over, so it
    // is wrapped without taking a reference that dropping it would give
    // back -- that released what was not ours, once per caller.
    let langs: id = unsafe { msg_send![user_defaults, stringArrayForKey:apple_lang] };
    if langs.is_null() {
        return Ok(CFArray::from_CFTypes(&[]));
    }
    Ok(unsafe { CFArray::wrap_under_get_rule(langs as CFArrayRef) })
}

/// See `simplified_chinese_font_missing`. PingFang is a font asset macOS
/// downloads on demand: until it has, its family names no file.
fn pingfang_missing() -> bool {
    let languages: Vec<String> = preferred_languages()
        .map(|languages| languages.iter().map(|language| language.to_string()).collect())
        .unwrap_or_default();
    if !prefers_simplified_chinese(&languages) {
        return false;
    }
    let installed = descriptor_from_attr(&FontAttributes::new_fallback("PingFang SC"))
        .is_ok_and(|descriptors| {
            descriptors.iter().any(|descriptor| {
                handles_from_descriptor(&descriptor)
                    .iter()
                    .any(|font| font.names().family == "PingFang SC")
            })
        });
    !installed
}

/// Whether the first of `languages` (BCP 47 tags, as macOS lists them) that
/// is Chinese, Japanese or Korean is Simplified Chinese. Japanese and Korean
/// readers want the Han characters their own fonts draw, Traditional Chinese
/// readers theirs; someone who lists none of them is told nothing about a
/// Chinese font.
fn prefers_simplified_chinese(languages: &[String]) -> bool {
    for language in languages {
        let language = language.to_ascii_lowercase();
        let mut subtags = language.split(|c| c == '-' || c == '_');
        match subtags.next() {
            Some("zh") => {
                return !subtags.any(|subtag| matches!(subtag, "hant" | "tw" | "hk" | "mo"));
            }
            Some("ja" | "ko") => return false,
            _ => {}
        }
    }
    false
}

fn build_fallback_list() -> Vec<ParsedFont> {
    build_fallback_list_impl().unwrap_or_else(|err| {
        log::error!("Error getting system fallback fonts: {:#}", err);
        Vec::new()
    })
}

fn build_fallback_list_impl() -> anyhow::Result<Vec<ParsedFont>> {
    let menlo =
        new_from_name("Menlo", 0.0).map_err(|_| anyhow::anyhow!("failed to get Menlo font"))?;

    let langs = preferred_languages()?;
    let cascade = cascade_list_for_languages(&menlo, &langs);
    let mut fonts = vec![];
    // Explicitly include Menlo itself, as it appears to be the only
    // font on macOS that contains U+2718.
    // <https://github.com/wezterm/wezterm/issues/849>
    fonts.append(&mut handles_from_descriptor(&menlo.copy_descriptor()));
    for descriptor in &cascade {
        fonts.append(&mut handles_from_descriptor(&descriptor));
    }
    // Some of the fallback fonts are special fonts that don't exist on
    // disk, and that we can't open.
    // In particular, `.AppleSymbolsFB` is one such font.  Let's try
    // a nearby approximation.
    let symbols = FontAttributes {
        family: "Apple Symbols".to_string(),
        weight: FontWeight::REGULAR,
        stretch: FontStretch::Normal,
        style: FontStyle::Normal,
        is_fallback: true,
        is_synthetic: true,
        harfbuzz_features: None,
        freetype_load_target: None,
        freetype_render_target: None,
        freetype_load_flags: None,
        scale: None,
        assume_emoji_presentation: None,
    };
    if let Ok(descriptors) = descriptor_from_attr(&symbols) {
        for descriptor in descriptors.iter() {
            fonts.append(&mut handles_from_descriptor(&descriptor));
        }
    }

    // Constrain to default weight/stretch/style
    fonts.retain(|f| {
        f.weight() == FontWeight::REGULAR
            && f.stretch() == FontStretch::Normal
            && f.style() == FontStyle::Normal
    });

    let mut seen = HashSet::new();
    let fonts: Vec<ParsedFont> = fonts
        .into_iter()
        .filter_map(|f| {
            if seen.contains(&f.handle) {
                None
            } else {
                seen.insert(f.handle.clone());
                Some(f)
            }
        })
        .collect();

    // Pre-compute coverage
    let empty = RangeSet::new();
    for font in &fonts {
        if let Err(err) = font.coverage_intersection(&empty) {
            log::error!("Error computing coverage for {:?}: {:#}", font, err);
        }
    }

    Ok(fonts)
}

#[cfg(test)]
mod test {
    use super::prefers_simplified_chinese;

    fn prefers(languages: &[&str]) -> bool {
        let languages: Vec<String> = languages.iter().map(|l| l.to_string()).collect();
        prefers_simplified_chinese(&languages)
    }

    #[test]
    fn simplified_chinese_is_preferred_only_when_it_is_the_first_han_reader() {
        assert!(prefers(&["zh-Hans-CN", "en-US"]));
        assert!(prefers(&["en-US", "zh-Hans-CN", "ja-JP"]));
        assert!(!prefers(&["en-AU"]));
        assert!(!prefers(&[]));
        assert!(prefers(&["zh-CN"]));
        assert!(!prefers(&["ja-JP", "zh-Hans-CN"]));
        assert!(!prefers(&["en-US", "ko-KR", "zh-Hans"]));
        assert!(!prefers(&["zh-Hant-TW", "zh-Hans-CN"]));
        assert!(!prefers(&["zh-HK"]));
    }
}
