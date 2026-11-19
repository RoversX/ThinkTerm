use crate::fcwrap;
use crate::locator::{FontDataHandle, FontDataSource, FontLocator, FontOrigin, FontPurpose};
use crate::parser::ParsedFont;
use anyhow::Context;
use config::{FontAttributes, FontStyle, FontWeight};
use fcwrap::{CharSet, FontSet, Pattern as FontPattern, FC_CHARCELL, FC_DUAL, FC_MONO};
use std::collections::HashSet;
use std::convert::TryInto;

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
        self.load_fonts_for(FontPurpose::Terminal, fonts_selection, loaded, pixel_size)
    }

    fn load_fonts_for(
        &self,
        purpose: FontPurpose,
        fonts_selection: &[FontAttributes],
        loaded: &mut HashSet<FontAttributes>,
        pixel_size: u16,
    ) -> anyhow::Result<Vec<ParsedFont>> {
        let mut fonts = vec![];

        fn matching_spacing(matches: FontSet, purpose: FontPurpose) -> Vec<FontPattern> {
            if purpose == FontPurpose::Interface {
                return matches.iter().collect();
            }
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
        fn by_family(
            attr: &FontAttributes,
            purpose: FontPurpose,
        ) -> anyhow::Result<Vec<FontPattern>> {
            let mut pattern = FontPattern::new()?;
            let start = std::time::Instant::now();
            pattern.family(&attr.family)?;
            let matches = matching_spacing(pattern.list()?, purpose);
            log::trace!(
                "listing {:?} by family took {:?} to compute and is {:?}",
                attr.family,
                start.elapsed(),
                matches
            );
            Ok(matches)
        }

        /// Search fontconfig using on the postscript name
        fn by_postscript(
            attr: &FontAttributes,
            purpose: FontPurpose,
        ) -> anyhow::Result<Vec<FontPattern>> {
            let mut pattern = FontPattern::new()?;
            let start = std::time::Instant::now();
            pattern.add_string("postscriptname", &attr.family)?;
            let matches = matching_spacing(pattern.list()?, purpose);
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
                match resolver(attr, purpose) {
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
                if purpose == FontPurpose::Interface
                    || matches!(best_match.get_integer("spacing"), Ok(FC_MONO | FC_DUAL))
                {
                    let handle = to_handle(best_match, Some(attr.family.clone()))?;
                    if let Ok(parsed) = crate::parser::ParsedFont::from_locator(&handle) {
                        log::trace!("found font-config fallback match for {:?}", parsed.names());
                        candidates.push(parsed);
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
        log::trace!("locating fallback fonts for {} codepoints", codepoints.len());
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

            let mut pushed_this_pass = 0;

            let mut charset = CharSet::new()?;
            charset.add(c)?;

            // Make two passes to locate a fallback: first try to find any
            // strictly monospace version, then, if we didn't find any matches,
            // look for a version with any spacing.
            for only_monospace in [true, false] {
                let mut pattern = FontPattern::new()?;
                pattern.add_charset(&charset)?;
                pattern.add_integer("weight", 80)?;
                pattern.add_integer("slant", 0)?;

                let mut lists = vec![pattern
                    .list()
                    .context("pattern.list with no spacing constraint")?];

                if only_monospace {
                    for &spacing in &SPACING {
                        pattern.delete_property("spacing")?;
                        pattern.add_integer("spacing", spacing)?;
                        lists.push(
                            pattern.list().with_context(|| {
                                format!("pattern.list with spacing={}", spacing)
                            })?,
                        );
                    }
                }

                for list in lists {
                    for pat in list.iter() {
                        let num = pat.charset_intersect_count(&charset)?;
                        if num == 0 {
                            log::error!(
                                "Skipping bogus font-config result {:?} because it doesn't overlap",
                                pat
                            );
                            continue;
                        }

                        if let Ok(file) = pat.get_file().context("pat.get_file") {
                            log::trace!("{file:?} covers {num} requested codepoints");
                            let handle = FontDataHandle {
                                source: FontDataSource::OnDisk(file.into()),
                                index: pat.get_integer("index")?.try_into()?,
                                variation: 0,
                                origin: FontOrigin::FontConfig,
                                coverage: pat.get_charset().ok().map(|c| c.to_range_set()),
                            };
                            if let Ok(parsed) = crate::parser::ParsedFont::from_locator(&handle) {
                                fonts.push(parsed);
                                pushed_this_pass += 1;
                            }
                        }
                    }
                }

                if pushed_this_pass == 0 {
                    // If we get here on the first iteration, then we didn't
                    // find a monospace version of fonts with those codepoints,
                    // let's continue and try any matching font
                    continue;
                }

                break;
            }
        }

        Ok(fonts)
    }

    fn locate_fallback_with_config(
        &self,
        codepoints: &[char],
        config: &config::ConfigHandle,
    ) -> anyhow::Result<Vec<ParsedFont>> {
        if config.fontconfig_prefer_system_fallback {
            self.locate_system_fallback_for_codepoints(codepoints)
        } else {
            self.locate_fallback_for_codepoints(codepoints)
        }
    }

    fn enumerate_family_names(&self) -> anyhow::Result<Vec<String>> {
        let pattern = FontPattern::new()?;
        Ok(pattern
            .list()?
            .iter()
            .filter_map(|font| font.get_string("family").ok())
            .collect())
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

impl FontConfigFontLocator {
    fn locate_system_fallback_for_codepoints(
        &self,
        codepoints: &[char],
    ) -> anyhow::Result<Vec<ParsedFont>> {
        log::trace!("locating system fallback fonts for {} codepoints", codepoints.len());
        let mut fonts: Vec<ParsedFont> = vec![];

        // Fontconfig rules can depend on the requested character. Ask for
        // its winner even when an earlier character's font also covers it.
        for &c in codepoints {
            let mut charset = CharSet::new()?;
            charset.add(c)?;

            // Respect fontconfig's family and language preferences rather
            // than taking the first font in cache order that covers the glyph.
            let mut pattern = FontPattern::new()?;
            pattern.family("monospace")?;
            pattern.add_charset(&charset)?;
            pattern.add_integer("weight", 80)?;
            pattern.add_integer("slant", 0)?;
            pattern.config_substitute(fcwrap::MatchKind::Pattern)?;
            // Fontconfig supplies the default language from FC_LANG or the locale.
            pattern.default_substitute();

            let mut select = |pat: &FontPattern| -> anyhow::Result<bool> {
                if pat.charset_intersect_count(&charset)? == 0 {
                    return Ok(false);
                }
                let Ok(file) = pat.get_file() else {
                    return Ok(false);
                };
                let source = FontDataSource::OnDisk(file.into());
                let index = pat.get_integer("index")?.try_into()?;
                if let Some(font) = fonts
                    .iter_mut()
                    .find(|f| f.handle.source == source && f.handle.index == index)
                {
                    font.system_fallback_for
                        .get_or_insert_with(Default::default)
                        .add(c as u32);
                    return Ok(true);
                }
                let handle = FontDataHandle {
                    source,
                    index,
                    variation: 0,
                    origin: FontOrigin::FontConfig,
                    coverage: pat.get_charset().ok().map(|set| set.to_range_set()),
                };
                let Ok(mut font) = ParsedFont::from_locator(&handle) else {
                    return Ok(false);
                };
                font.system_fallback_for
                    .get_or_insert_with(Default::default)
                    .add(c as u32);
                fonts.push(font);
                Ok(true)
            };

            let mut pushed = false;
            for pat in pattern
                .sort(true)
                .context("pattern.sort with charset")?
                .iter()
            {
                if select(&pat)? {
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
                    if select(&pat)? {
                        break;
                    }
                }
            }
        }

        Ok(fonts)
    }
}
