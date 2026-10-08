#![cfg(target_os = "macos")]
#![allow(unexpected_cfgs)] // <https://github.com/SSheldon/rust-objc/issues/125>

use crate::locator::{FontDataSource, FontLocator, FontOrigin};
use crate::parser::ParsedFont;
use cocoa::base::id;
use config::{FontAttributes, FontStretch, FontStyle, FontWeight};
use core_foundation::array::CFArray;
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
    static ref SYSTEM_UI: Vec<ParsedFont> = build_system_ui_font_list();
}

/// The family name that stands for the font the system draws its own
/// interface with (San Francisco, on current releases).
pub const SYSTEM_UI_FONT_FAMILY: &str = ".AppleSystemUIFont";

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
        Ok(unsafe { CFArray::wrap_under_get_rule(array) })
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
            if attr.family == SYSTEM_UI_FONT_FAMILY {
                if let Some(parsed) = ParsedFont::best_match(attr, pixel_size, SYSTEM_UI.clone()) {
                    log::trace!("system ui font for {:?} is {:?}", attr, parsed);
                    fonts.push(parsed);
                    loaded.insert(attr.clone());
                    continue;
                }
            }
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
        for (cov_len, font) in &mut matches {
            if let Ok(cov) = font.coverage_intersection(&wanted) {
                *cov_len = cov.len();
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

        log::trace!("fallback candidates for {codepoints:?} is {matches:#?}");

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

/// The faces of the system interface font.
///
/// Core Text keeps that font out of its collections and matches nothing to
/// its family name, so asking for it the way any other family is asked for
/// finds nothing and the interface falls through to the next font in its
/// list. Asking for the UI font itself does find it.
///
/// It is a variable font whose named instances come at every weight and
/// width, and then again at several grades. One instance is kept for each
/// weight, width and style -- the first, which is the ungraded one -- and its
/// weight is rounded to the hundred the instance is named for, since the
/// font's own are a little off them (Medium is 510, Semibold 590) and
/// matching looks for a weight exactly before it looks nearby.
fn build_system_ui_font_list() -> Vec<ParsedFont> {
    let font = new_ui_font_for_language(kCTFontSystemFontType, 0.0, None);
    // How far each kept face's own weight is from the one it is filed under.
    let mut kept: Vec<(u16, ParsedFont)> = vec![];
    for parsed in handles_from_descriptor(&font.copy_descriptor()) {
        let exact = parsed.weight().to_opentype_weight();
        let rounded = ((exact + 50) / 100 * 100).clamp(100, 1000);
        let off_by = exact.abs_diff(rounded);
        let parsed = parsed.with_weight(FontWeight::from_opentype_weight(rounded));
        match kept.iter_mut().find(|(_, other)| {
            other.weight() == parsed.weight()
                && other.stretch() == parsed.stretch()
                && other.style() == parsed.style()
        }) {
            // Ultralight and Thin both round to 100: the nearer stays.
            Some(other) if off_by < other.0 => *other = (off_by, parsed),
            Some(_) => {}
            None => kept.push((off_by, parsed)),
        }
    }
    if kept.is_empty() {
        log::warn!("the system interface font has no faces that could be read");
    }
    kept.into_iter().map(|(_, parsed)| parsed).collect()
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

    let user_defaults: id = unsafe { msg_send![class!(NSUserDefaults), standardUserDefaults] };

    let apple_lang = "AppleLanguages"
        .parse::<CFString>()
        .map_err(|_| anyhow::anyhow!("failed to parse lang name en as CFString"))?;

    let langs: CFArray<CFString> =
        unsafe { msg_send![user_defaults, stringArrayForKey:apple_lang] };

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
    use super::*;

    /// Core Text matches nothing to the system interface font's family
    /// name, and the interface was drawn with the next font in its list.
    #[test]
    fn the_system_interface_font_is_found_at_the_weight_asked_for() {
        for weight in [FontWeight::REGULAR, FontWeight::MEDIUM, FontWeight::BOLD] {
            let attr = FontAttributes {
                family: SYSTEM_UI_FONT_FAMILY.to_string(),
                weight,
                ..Default::default()
            };
            let mut loaded = HashSet::new();
            let fonts = CoreTextFontLocator {}
                .load_fonts(&[attr.clone()], &mut loaded, 16)
                .unwrap();
            assert_eq!(fonts.len(), 1, "one face for {attr:?}: {fonts:?}");
            assert_eq!(fonts[0].weight(), weight);
            assert_eq!(fonts[0].stretch(), FontStretch::Normal);
            assert_eq!(fonts[0].style(), FontStyle::Normal);
            assert!(loaded.contains(&attr));
        }
    }

    #[test]
    fn the_families_on_offer_are_named_without_opening_them() {
        let families = CoreTextFontLocator {}.enumerate_family_names().unwrap();
        assert!(families.iter().any(|family| family == "Menlo"));
    }
}
