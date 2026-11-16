use crate::fcwrap;
use crate::locator::{FontDataHandle, FontDataSource, FontLocator, FontOrigin};
use crate::parser::ParsedFont;
use anyhow::Context;
use config::{FontAttributes, FontStyle, FontWeight};
use fcwrap::{CharSet, FontSet, Pattern as FontPattern, FC_CHARCELL, FC_DUAL, FC_MONO};
use std::collections::HashSet;
use std::convert::TryInto;

/// The language of the user's locale in the form fontconfig expects
/// (`zh-cn`, not `zh_CN`), so a fallback is picked for the script the user
/// actually reads rather than for whatever the cache happens to list first.
fn locale_lang() -> Option<String> {
    for key in ["LC_ALL", "LC_CTYPE", "LANG"] {
        let Ok(value) = std::env::var(key) else {
            continue;
        };
        let language = value.split('.').next().unwrap_or(&value).replace('_', "-");
        let language = language.to_lowercase();
        if language.is_empty() || language == "c" || language == "posix" {
            continue;
        }
        return Some(language);
    }
    None
}

/// Allow for monospace, dual and charcell spacing; these are
/// fixed width styles so are desirable for a terminal use case.
const SPACING: [i32; 3] = [FC_MONO, FC_DUAL, FC_CHARCELL];

/// A FontLocator implemented using the system font loading
/// functions provided by font-config
pub struct FontConfigFontLocator {}

impl FontLocator for FontConfigFontLocator {
    fn load_fonts(
        &self,
        fonts_selection: &[FontAttributes],
        loaded: &mut HashSet<FontAttributes>,
        pixel_size: u16,
    ) -> anyhow::Result<Vec<ParsedFont>> {
        let mut fonts = vec![];

        /// Returns a FontSet list filtered to only mono/dual spaced fonts
        fn monospaced(matches: FontSet) -> Vec<FontPattern> {
            matches
                .iter()
                .filter_map(|p| match p.get_integer("spacing") {
                    Ok(n) if n == FC_MONO || n == FC_DUAL || n == FC_CHARCELL => Some(p),
                    // (probably!) no spacing defined. Assume monospace.
                    Err(_) => Some(p),
                    _ => None,
                })
                .collect()
        }

        /// Search fontconfig using only the family name
        fn by_family(attr: &FontAttributes) -> anyhow::Result<Vec<FontPattern>> {
            let mut pattern = FontPattern::new()?;
            let start = std::time::Instant::now();
            pattern.family(&attr.family)?;
            let matches = monospaced(pattern.list()?);
            log::trace!(
                "listing {:?} by family took {:?} to compute and is {:?}",
                attr.family,
                start.elapsed(),
                matches
            );
            Ok(matches)
        }

        /// Search fontconfig using on the postscript name
        fn by_postscript(attr: &FontAttributes) -> anyhow::Result<Vec<FontPattern>> {
            let mut pattern = FontPattern::new()?;
            let start = std::time::Instant::now();
            pattern.add_string("postscriptname", &attr.family)?;
            let matches = monospaced(pattern.list()?);
            log::trace!(
                "listing {:?} by postscriptname took {:?} to compute and is {:?}",
                attr.family,
                start.elapsed(),
                matches
            );
            Ok(matches)
        }

        fn to_handle(
            pat: FontPattern,
            match_name: Option<String>,
        ) -> anyhow::Result<FontDataHandle> {
            let file = pat.get_file()?;
            let index = pat.get_integer("index")? as u32;
            let variation = index >> 16;
            let index = index & 0xffff;
            Ok(FontDataHandle {
                source: FontDataSource::OnDisk(file.into()),
                index,
                variation,
                origin: match_name
                    .map(FontOrigin::FontConfigMatch)
                    .unwrap_or(FontOrigin::FontConfig),
                coverage: pat.get_charset().ok().map(|c| c.to_range_set()),
            })
        }

        for attr in fonts_selection {
            let mut candidates = vec![];

            // Aggregate results of both family and postscript name lookups
            for resolver in &[by_family, by_postscript] {
                match resolver(attr) {
                    Ok(matches) => {
                        for pat in matches {
                            let handle = to_handle(pat, None)?;

                            // fontconfig will give us a boatload of random fallbacks.
                            // so we need to parse the returned font
                            // here to see if we got what we asked for.
                            if let Ok(parsed) = crate::parser::ParsedFont::from_locator(&handle) {
                                if parsed.matches_name(attr) {
                                    log::trace!("found font-config match for {:?}", parsed.names());
                                    candidates.push(parsed);
                                }
                            }
                        }
                    }
                    Err(err) => log::trace!("while searching for {:?}: {:#}", attr, err),
                }
            }

            // Aliases like 'monospace' that users might have customized
            // can only be resolved via get_best_match, do it here
            if candidates.is_empty() {
                let mut pattern = FontPattern::new()?;
                let start = std::time::Instant::now();
                pattern.family(&attr.family)?;
                pattern.add_integer("weight", to_fc_weight(attr.weight))?;
                pattern.add_integer(
                    "slant",
                    match attr.style {
                        FontStyle::Normal => fcwrap::FC_SLANT_ROMAN,
                        FontStyle::Italic => fcwrap::FC_SLANT_ITALIC,
                        FontStyle::Oblique => fcwrap::FC_SLANT_OBLIQUE,
                    },
                )?;
                pattern.config_substitute(fcwrap::MatchKind::Pattern)?;
                let best_match = pattern.get_best_match()?;
                log::trace!(
                    "matching by family '{}' took {:?} to compute and is {:?}",
                    attr.family,
                    start.elapsed(),
                    best_match
                );
                // For the fallback, be very careful, only select known monospace
                if let Ok(spacing) = best_match.get_integer("spacing") {
                    if spacing == FC_MONO || spacing == FC_DUAL {
                        let handle = to_handle(best_match, Some(attr.family.clone()))?;
                        if let Ok(parsed) = crate::parser::ParsedFont::from_locator(&handle) {
                            log::trace!(
                                "found font-config fallback match for {:?}",
                                parsed.names()
                            );
                            candidates.push(parsed);
                        }
                    }
                }
            }

            // Apply our CSS-style font matching criteria
            if let Some(parsed) = ParsedFont::best_match(attr, pixel_size, candidates) {
                log::trace!("selected best font-config match {:?}", parsed.names());
                fonts.push(parsed);
                loaded.insert(attr.clone());
            }
        }

        Ok(fonts)
    }

    fn locate_fallback_for_codepoints(
        &self,
        codepoints: &[char],
    ) -> anyhow::Result<Vec<ParsedFont>> {
        log::trace!("locate_fallback_for_codepoints: {:?}", codepoints);
        let mut fonts: Vec<ParsedFont> = vec![];

        // In <https://github.com/wezterm/wezterm/issues/4310> we discover
        // that a font-config query for a charset containing both
        // 3065 and 2686 fails because no fonts contain both codepoints,
        // but querying separately does find the separate fonts.
        // We therefore need to break up our query so that we resolve
        // each codepoint individually.
        // However, if we need to resolve a block of characters that
        // are found in the same font (eg: someone is printing an
        // entire unicode block) we don't want to issue N queries
        // that return the same font.
        //
        // So we check the fonts that have been resolved in earlier
        // iterations to see if any of those cover a given codepoint
        // and allow that to satisfy the query if they do.

        'next_codepoint: for &c in codepoints {
            if !fonts.is_empty() {
                let mut wanted_range = rangeset::RangeSet::new();
                wanted_range.add(c as u32);
                for f in &fonts {
                    match f.coverage_intersection(&wanted_range) {
                        Ok(r) if !r.is_empty() => {
                            // already found a font with this one!
                            continue 'next_codepoint;
                        }
                        _ => {}
                    }
                }
            }

            let mut charset = CharSet::new()?;
            charset.add(c)?;

            // Take the font fontconfig itself considers the best match for
            // this codepoint rather than the first font in its cache that has
            // it. `FcFontList` answers in cache order, and on Ubuntu that puts
            // the Arphic UKai/UMing faces -- a brush-written kai, not the sans
            // face the rest of the desktop draws -- ahead of Noto Sans CJK, so
            // every CJK cell in the terminal came out looking like a woodblock
            // print. `FcFontSort` on the same query, with the generic
            // monospace family and the user's language applied, puts the
            // system's own preference first: Noto Sans Mono CJK SC on zh_CN.
            let mut pattern = FontPattern::new()?;
            pattern.family("monospace")?;
            if let Some(lang) = locale_lang() {
                pattern.add_string("lang", &lang)?;
            }
            pattern.add_charset(&charset)?;
            pattern.add_integer("weight", 80)?;
            pattern.add_integer("slant", 0)?;
            pattern.config_substitute(fcwrap::MatchKind::Pattern)?;
            pattern.default_substitute();

            let push_font = |pat: &FontPattern, fonts: &mut Vec<ParsedFont>| -> anyhow::Result<bool> {
                let num = pat.charset_intersect_count(&charset)?;
                if num == 0 {
                    log::error!(
                        "Skipping bogus font-config result {:?} because it doesn't overlap",
                        pat
                    );
                    return Ok(false);
                }

                let Ok(file) = pat.get_file().context("pat.get_file") else {
                    return Ok(false);
                };
                log::trace!("{file:?} has {num} codepoints from {codepoints:?}");
                let handle = FontDataHandle {
                    source: FontDataSource::OnDisk(file.into()),
                    index: pat.get_integer("index")?.try_into()?,
                    variation: 0,
                    origin: FontOrigin::FontConfig,
                    coverage: pat.get_charset().ok().map(|c| c.to_range_set()),
                };
                if let Ok(parsed) = crate::parser::ParsedFont::from_locator(&handle) {
                    fonts.push(parsed);
                    return Ok(true);
                }
                Ok(false)
            };

            let mut pushed = false;
            for pat in pattern
                .sort(true)
                .context("pattern.sort with charset")?
                .iter()
            {
                if push_font(&pat, &mut fonts)? {
                    pushed = true;
                    break;
                }
            }

            if !pushed {
                // A fontconfig that trimmed everything (an empty or broken
                // cache) still has an unsorted answer; take that over none.
                let mut plain = FontPattern::new()?;
                plain.add_charset(&charset)?;
                for pat in plain
                    .list()
                    .context("pattern.list with no spacing constraint")?
                    .iter()
                {
                    if push_font(&pat, &mut fonts)? {
                        break;
                    }
                }
            }
        }

        Ok(fonts)
    }

    fn enumerate_all_fonts(&self) -> anyhow::Result<Vec<ParsedFont>> {
        let mut fonts = vec![];
        let pattern = FontPattern::new()?;
        let mut files = HashSet::new();
        for pat in pattern
            .list()
            .context("listing fonts from font-config")?
            .iter()
        {
            let file = pat.get_file().context("pat.get_file")?;
            if files.contains(&file) {
                continue;
            }
            files.insert(file.clone());

            let source = FontDataSource::OnDisk(file.into());
            if let Err(err) = crate::parser::parse_and_collect_font_info(
                &source,
                &mut fonts,
                FontOrigin::FontConfig,
            ) {
                log::warn!("While parsing: {:?}: {:#}", source, err);
            }
        }
        fonts.sort();

        Ok(fonts)
    }
}

fn to_fc_weight(w: FontWeight) -> std::os::raw::c_int {
    if w <= FontWeight::THIN {
        fcwrap::FC_WEIGHT_THIN
    } else if w <= FontWeight::EXTRALIGHT {
        fcwrap::FC_WEIGHT_EXTRALIGHT
    } else if w <= FontWeight::LIGHT {
        fcwrap::FC_WEIGHT_LIGHT
    } else if w <= FontWeight::BOOK {
        fcwrap::FC_WEIGHT_BOOK
    } else if w <= FontWeight::REGULAR {
        fcwrap::FC_WEIGHT_REGULAR
    } else if w <= FontWeight::MEDIUM {
        fcwrap::FC_WEIGHT_MEDIUM
    } else if w <= FontWeight::DEMIBOLD {
        fcwrap::FC_WEIGHT_DEMIBOLD
    } else if w <= FontWeight::BOLD {
        fcwrap::FC_WEIGHT_BOLD
    } else if w <= FontWeight::EXTRABOLD {
        fcwrap::FC_WEIGHT_EXTRABOLD
    } else if w <= FontWeight::BLACK {
        fcwrap::FC_WEIGHT_BLACK
    } else {
        fcwrap::FC_WEIGHT_EXTRABLACK
    }
}
