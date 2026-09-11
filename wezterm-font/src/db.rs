//! A font-database to keep track of fonts that we've located

use crate::locator::{FontDataSource, FontOrigin};
use crate::parser::{load_built_in_fonts, parse_and_collect_font_info, ParsedFont};
use anyhow::Context;
use config::{Config, FontAttributes};
use rangeset::RangeSet;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, OnceLock};

pub struct FontDatabase {
    by_full_name: HashMap<String, Vec<ParsedFont>>,
}

/// Process-wide built-in font database; see [`FontDatabase::shared_built_in`].
static BUILT_IN: OnceLock<Result<Arc<FontDatabase>, String>> = OnceLock::new();

impl FontDatabase {
    pub fn new() -> Self {
        Self {
            by_full_name: HashMap::new(),
        }
    }

    fn load_font_info(&mut self, font_info: Vec<ParsedFont>) {
        for parsed in font_info {
            if let Some(path) = parsed.handle.path_str() {
                self.by_full_name
                    .entry(path.to_string())
                    .or_insert_with(Vec::new)
                    .push(parsed.clone());
            }
            self.by_full_name
                .entry(parsed.names().full_name.clone())
                .or_insert_with(Vec::new)
                .push(parsed);
        }
    }

    /// Build up the database from the fonts found in the configured font dirs
    /// and from the built-in selection of fonts
    pub fn with_font_dirs(config: &Config) -> anyhow::Result<Self> {
        let mut font_info = vec![];
        for path in &config.font_dirs {
            for entry in walkdir::WalkDir::new(path).into_iter() {
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(_) => continue,
                };

                let source = FontDataSource::OnDisk(entry.path().to_path_buf());
                parse_and_collect_font_info(&source, &mut font_info, FontOrigin::FontDirs)
                    .map_err(|err| {
                        log::trace!("failed to read {:?}: {:#}", source, err);
                        err
                    })
                    .ok();
            }
        }

        let mut db = Self::new();
        db.load_font_info(font_info);
        Ok(db)
    }

    pub fn list_available(&self) -> Vec<ParsedFont> {
        let mut fonts = vec![];
        for parsed_list in self.by_full_name.values() {
            for parsed in parsed_list {
                fonts.push(parsed.clone());
            }
        }
        fonts
    }

    pub fn with_built_in() -> anyhow::Result<Self> {
        let mut font_info = vec![];
        load_built_in_fonts(&mut font_info)?;
        let mut db = Self::new();
        db.load_font_info(font_info);
        Ok(db)
    }

    /// The built-in database, built once per process.
    ///
    /// `with_built_in` takes no arguments and reads only `include_bytes!`
    /// data, so its result cannot vary by config, DPI or window -- yet every
    /// `FontConfiguration` used to build its own, paying ~30 FreeType face
    /// opens and name-table walks each time. That sits on an interactive path:
    /// the terminal-preview fit loop constructs up to `MAX_PREVIEW_SCALE_STEPS`
    /// configurations in a single layout pass, each a cache miss.
    ///
    /// Sharing also shares each `ParsedFont`'s lazily computed `coverage`
    /// (guarded by its own `Mutex`), which each instance would otherwise
    /// recompute for itself.
    ///
    /// Note the one behavioural change: a failure here is cached, so it is
    /// permanent for the process rather than retried per configuration. The
    /// inputs are compiled-in bytes, so a failure is deterministic and a retry
    /// could not have succeeded anyway.
    pub fn shared_built_in() -> anyhow::Result<Arc<Self>> {
        match BUILT_IN.get_or_init(|| {
            Self::with_built_in()
                .map(Arc::new)
                .map_err(|err| format!("{err:#}"))
        }) {
            Ok(db) => Ok(Arc::clone(db)),
            Err(err) => anyhow::bail!("{err}"),
        }
    }

    pub fn resolve_multiple(
        &self,
        fonts: &[FontAttributes],
        handles: &mut Vec<ParsedFont>,
        loaded: &mut HashSet<FontAttributes>,
        pixel_size: u16,
    ) {
        for attr in fonts {
            if let Some(handle) = self.resolve(attr, pixel_size) {
                handles.push(handle.clone().synthesize(attr));
                loaded.insert(attr.clone());
            }
        }
    }

    /// Equivalent to FontLocator::locate_fallback_for_codepoints
    pub fn locate_fallback_for_codepoints(
        &self,
        codepoints: &[char],
    ) -> anyhow::Result<Vec<ParsedFont>> {
        let mut wanted_range = RangeSet::new();
        for &c in codepoints {
            wanted_range.add(c as u32);
        }

        let mut matches = vec![];

        for parsed_list in self.by_full_name.values() {
            for parsed in parsed_list {
                if parsed.names().family == "Last Resort High-Efficiency" {
                    continue;
                }
                let covered = parsed
                    .coverage_intersection(&wanted_range)
                    .with_context(|| format!("coverage_interaction for {:?}", parsed))?;
                if !covered.is_empty() {
                    matches.push(parsed.clone());
                }
            }
        }

        Ok(matches)
    }

    pub fn candidates(&self, font_attr: &FontAttributes) -> Vec<&ParsedFont> {
        let mut fonts = vec![];
        for parsed_list in self.by_full_name.values() {
            for parsed in parsed_list {
                if parsed.matches_name(font_attr) {
                    fonts.push(parsed);
                }
            }
        }
        fonts
    }

    pub fn resolve(&self, font_attr: &FontAttributes, pixel_size: u16) -> Option<&ParsedFont> {
        let mut candidates = vec![];
        for parsed_list in self.by_full_name.values() {
            for parsed in parsed_list {
                if parsed.matches_name(font_attr) {
                    candidates.push(parsed);
                }
            }
        }

        if let Some(idx) = ParsedFont::best_matching_index(font_attr, &candidates, pixel_size) {
            return candidates.get(idx).copied();
        }

        None
    }
}
