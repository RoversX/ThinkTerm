use crate::db::FontDatabase;
use crate::locator::{new_locator, FontLocator};
use crate::parser::ParsedFont;
use crate::rasterizer::{new_rasterizer, FontRasterizer};
use crate::shaper::{new_shaper, FontShaper, PresentationWidth};
use anyhow::{Context, Error};
use config::{
    configuration, BoldBrightening, ConfigHandle, DisplayPixelGeometry, FontAttributes,
    FontRasterizerSelection, FontStretch, FontStyle, FontWeight, TextStyle,
};
use rangeset::RangeSet;
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::rc::{Rc, Weak};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use termwiz::cell::Presentation;
use thiserror::Error;
use wezterm_bidi::Direction;
use wezterm_term::{CellAttributes, Intensity};
use wezterm_toast_notification::ToastNotification;

mod hbwrap;

pub mod db;
pub mod ftwrap;
pub mod locator;
pub mod parser;
pub mod rasterizer;
pub mod shaper;
pub mod units;

#[cfg(all(unix, not(target_os = "macos")))]
pub mod fcwrap;

pub use crate::rasterizer::RasterizedGlyph;
pub use crate::shaper::{FallbackIdx, FontMetrics, GlyphInfo};

#[derive(Debug, Error)]
#[error("Font fallback recalculated")]
pub struct ClearShapeCache {}

static FONT_ID: ::std::sync::atomic::AtomicUsize = ::std::sync::atomic::AtomicUsize::new(0);
pub type LoadedFontId = usize;
/// The family the interface is drawn with instead of the system's, and how
/// many times that has changed: every `FontConfiguration` holds interface
/// fonts it built for the family of the time, and the count is how each
/// finds out that they are stale.
static UI_FONT_FAMILY: Mutex<Option<String>> = Mutex::new(None);
static UI_FONT_GENERATION: ::std::sync::atomic::AtomicUsize =
    ::std::sync::atomic::AtomicUsize::new(0);

/// Draw the interface -- window titles, tabs, sidebars, the command palette
/// -- with `family`, in every window. `None` goes back to the system's
/// interface font. The terminal's own font is not this one's business.
/// True when that changed the family.
pub fn set_ui_font_family(family: Option<String>) -> bool {
    let family = family
        .map(|family| family.trim().to_string())
        .filter(|family| !family.is_empty());
    let mut current = UI_FONT_FAMILY.lock().unwrap();
    if *current == family {
        return false;
    }
    *current = family;
    UI_FONT_GENERATION.fetch_add(1, ::std::sync::atomic::Ordering::Relaxed);
    true
}

/// What `set_ui_font_family` was last given.
pub fn ui_font_family() -> Option<String> {
    UI_FONT_FAMILY.lock().unwrap().clone()
}

pub fn alloc_font_id() -> LoadedFontId {
    FONT_ID.fetch_add(1, ::std::sync::atomic::Ordering::Relaxed)
}

lazy_static::lazy_static! {
    static ref LAST_WARNING: Mutex<Option<(Instant, usize)>> = Mutex::new(None);
    /// Fallback resolution results, process-wide, keyed by the sorted missing
    /// codepoints within one config generation. Resolution depends on neither
    /// font size nor scale, but every LoadedFont (one per preview scale bucket
    /// in the GUI) used to re-run the async resolve for the same codepoints
    /// and answer its arrival by throwing away every shape cache. A memo hit
    /// is a synchronous insert with nothing to clear. An empty entry records
    /// "nothing provides these", which also stops unresolvable glyphs from
    /// re-scheduling the resolver once per font forever.
    static ref FALLBACK_RESOLVE_MEMO: Mutex<FallbackResolveMemo> =
        Mutex::new(FallbackResolveMemo::default());
}

/// Entries can weigh tens of KB each for CJK fallbacks, so the memo holds
/// only the current config generation and a bounded number of entries;
/// exceeding either simply forgets and re-resolves.
const FALLBACK_RESOLVE_MEMO_CAP: usize = 512;

#[derive(Default)]
struct FallbackResolveMemo {
    generation: usize,
    entries: HashMap<Vec<char>, Vec<ParsedFont>>,
}

impl FallbackResolveMemo {
    fn get(&self, generation: usize, chars: &[char]) -> Option<Vec<ParsedFont>> {
        if self.generation != generation {
            return None;
        }
        self.entries.get(chars).cloned()
    }

    fn insert(&mut self, generation: usize, chars: Vec<char>, handles: Vec<ParsedFont>) {
        // Async resolves can complete out of order across a config reload;
        // a straggler from an older generation must not clear the newer
        // entries or roll the generation back.
        if generation < self.generation {
            return;
        }
        if generation > self.generation {
            self.entries.clear();
            self.generation = generation;
        } else if self.entries.len() >= FALLBACK_RESOLVE_MEMO_CAP
            && !self.entries.contains_key(&chars)
        {
            self.entries.clear();
        }
        self.entries.insert(chars, handles);
    }
}

pub struct LoadedFont {
    rasterizers: RefCell<HashMap<FallbackIdx, Box<dyn FontRasterizer>>>,
    handles: RefCell<Vec<ParsedFont>>,
    shaper: RefCell<Box<dyn FontShaper>>,
    metrics: FontMetrics,
    pixel_geometry: DisplayPixelGeometry,
    font_size: f64,
    dpi: u32,
    font_config: Weak<FontConfigInner>,
    pending_fallback: Arc<Mutex<Vec<ParsedFont>>>,
    text_style: TextStyle,
    id: LoadedFontId,
    /// Glyphs for which no font was found and for which we should
    /// stop searching
    tried_glyphs: RefCell<HashSet<char>>,
}

impl std::fmt::Debug for LoadedFont {
    fn fmt(&self, fmt: &mut std::fmt::Formatter) -> std::fmt::Result {
        fmt.debug_struct("LoadedFont")
            .field("handles", &self.handles)
            .field("metrics", &self.metrics)
            .field("font_size", &self.font_size)
            .field("dpi", &self.dpi)
            .field("pending_fallback", &self.pending_fallback)
            .field("text_style", &self.text_style)
            .finish()
    }
}

impl LoadedFont {
    pub fn metrics(&self) -> FontMetrics {
        self.metrics
    }

    pub fn style(&self) -> &TextStyle {
        &self.text_style
    }

    pub fn id(&self) -> LoadedFontId {
        self.id
    }

    fn insert_fallback_handles(&self, extra_handles: Vec<ParsedFont>) -> anyhow::Result<bool> {
        let mut loaded = false;
        {
            let mut handles = self.handles.borrow_mut();
            for h in extra_handles {
                if !handles.iter().any(|existing| *existing == h) {
                    handles.push(h);
                    loaded = true;
                }
            }
            if loaded {
                log::trace!("revised fallback: {:#?}", handles);
            }
        }
        if loaded {
            if let Some(font_config) = self.font_config.upgrade() {
                *self.shaper.borrow_mut() =
                    new_shaper(&*font_config.config.borrow(), &self.handles.borrow())?;
            }
        }
        Ok(loaded)
    }

    pub fn blocking_shape(
        &self,
        text: &str,
        presentation: Option<Presentation>,
        direction: Direction,
        range: Option<Range<usize>>,
        presentation_width: Option<&PresentationWidth>,
    ) -> anyhow::Result<Vec<GlyphInfo>> {
        loop {
            let (tx, rx) = channel();

            let (async_resolve, res) = match self.shape_impl(
                text,
                move |_: &[char]| {
                    let _ = tx.send(());
                },
                |_| {},
                presentation,
                direction,
                range.clone(),
                presentation_width,
            ) {
                Ok(tuple) => tuple,
                Err(err) if err.downcast_ref::<ClearShapeCache>().is_some() => {
                    continue;
                }
                Err(err) => return Err(err),
            };

            if !async_resolve {
                return Ok(res);
            }
            if rx.recv().is_err() {
                return Ok(res);
            }
        }
    }

    pub fn shape<F: FnOnce(&[char]) + Send + 'static, FS: FnMut(&mut Vec<char>)>(
        &self,
        text: &str,
        completion: F,
        filter_out_synthetic: FS,
        presentation: Option<Presentation>,
        direction: Direction,
        range: Option<Range<usize>>,
        presentation_width: Option<&PresentationWidth>,
    ) -> anyhow::Result<Vec<GlyphInfo>> {
        let (_async_resolve, res) = self.shape_impl(
            text,
            completion,
            filter_out_synthetic,
            presentation,
            direction,
            range,
            presentation_width,
        )?;
        Ok(res)
    }

    fn shape_impl<F: FnOnce(&[char]) + Send + 'static, FS: FnMut(&mut Vec<char>)>(
        &self,
        text: &str,
        completion: F,
        mut filter_out_synthetic: FS,
        presentation: Option<Presentation>,
        direction: Direction,
        range: Option<Range<usize>>,
        presentation_width: Option<&PresentationWidth>,
    ) -> anyhow::Result<(bool, Vec<GlyphInfo>)> {
        let mut completion = Some(completion);
        let mut consulted_memo = false;
        loop {
            {
                let mut pending = self.pending_fallback.lock().unwrap();
                if !pending.is_empty() {
                    match self.insert_fallback_handles(pending.split_off(0)) {
                        Ok(true) => return Err(ClearShapeCache {})?,
                        Ok(false) => {}
                        Err(err) => {
                            log::error!("Error adding fallback: {:#}", err);
                        }
                    }
                }
            }

            let mut no_glyphs = vec![];
            let result = self.shaper.borrow().shape(
                text,
                self.font_size,
                self.dpi,
                &mut no_glyphs,
                presentation,
                direction,
                range.clone(),
                presentation_width,
            );

            no_glyphs.retain(|&c| c != '\u{FE0F}' && c != '\u{FE0E}');
            filter_out_synthetic(&mut no_glyphs);
            no_glyphs
                .retain(|c| !self.tried_glyphs.borrow().contains(c));
            no_glyphs.sort();
            no_glyphs.dedup();

            // A previously resolved answer for exactly these codepoints can be
            // installed synchronously and the shape simply redone: nothing
            // upstream has cached this attempt, so there is nothing to clear
            // -- where the async path answers with ClearShapeCache and throws
            // away every shape cache in the window.
            if !no_glyphs.is_empty() && !consulted_memo {
                if let Some(font_config) = self.font_config.upgrade() {
                    let generation = font_config.config.borrow().generation();
                    let memo = FALLBACK_RESOLVE_MEMO
                        .lock()
                        .unwrap()
                        .get(generation, &no_glyphs);
                    if let Some(handles) = memo {
                        consulted_memo = true;
                        let inserted = if handles.is_empty() {
                            false
                        } else {
                            self.insert_fallback_handles(handles).unwrap_or_else(|err| {
                                log::error!("Error adding memoized fallback: {:#}", err);
                                false
                            })
                        };
                        if inserted {
                            continue;
                        }
                        // The memo says nothing (new) provides these: record
                        // them as tried and settle for placeholder glyphs
                        // without waking the resolver.
                        let mut tried_glyphs = self.tried_glyphs.borrow_mut();
                        for c in &no_glyphs {
                            tried_glyphs.insert(*c);
                        }
                        return result.map(|r| (false, r));
                    }
                }
            }

            let mut async_resolve = false;

            if !no_glyphs.is_empty() {
                {
                    let mut tried_glyphs = self.tried_glyphs.borrow_mut();
                    for c in &no_glyphs {
                        tried_glyphs.insert(*c);
                    }
                }
                if let Some(font_config) = self.font_config.upgrade() {
                    font_config.schedule_fallback_resolve(
                        no_glyphs,
                        &self.pending_fallback,
                        completion.take().expect("fallback scheduled at most once"),
                    );
                    async_resolve = true;
                }
            }

            return result.map(|r| (async_resolve, r));
        }
    }

    pub fn metrics_for_idx(&self, font_idx: usize) -> anyhow::Result<FontMetrics> {
        self.shaper
            .borrow()
            .metrics_for_idx(font_idx, self.font_size, self.dpi)
    }

    pub fn brightness_adjust(&self, font_idx: usize) -> f32 {
        let synthesize_dim = self
            .handles
            .borrow()
            .get(font_idx)
            .map(|p| p.synthesize_dim)
            .unwrap_or(false);
        if synthesize_dim {
            0.5
        } else {
            1.0
        }
    }

    pub fn rasterize_glyph(
        &self,
        glyph_pos: u32,
        fallback: FallbackIdx,
    ) -> anyhow::Result<RasterizedGlyph> {
        let mut rasterizers = self.rasterizers.borrow_mut();
        if let Some(raster) = rasterizers.get(&fallback) {
            raster.rasterize_glyph(glyph_pos, self.font_size, self.dpi)
        } else {
            let raster_selection = self
                .font_config
                .upgrade()
                .map_or(FontRasterizerSelection::default(), |c| {
                    c.config.borrow().font_rasterizer
                });
            let raster = new_rasterizer(
                raster_selection,
                &(self.handles.borrow())[fallback],
                self.pixel_geometry,
            )?;
            let result = raster.rasterize_glyph(glyph_pos, self.font_size, self.dpi);
            rasterizers.insert(fallback, raster);
            result
        }
    }

    pub fn clone_handles(&self) -> Vec<ParsedFont> {
        self.handles.borrow().clone()
    }
}

struct FallbackResolveInfo {
    no_glyphs: Vec<char>,
    pending: Arc<Mutex<Vec<ParsedFont>>>,
    /// Called with the codepoints this resolve was about, so the consumer
    /// can evict only the shape-cache entries containing them instead of
    /// clearing every cache in the window.
    completion: Box<dyn FnOnce(&[char]) + Send>,
    font_dirs: Arc<FontDatabase>,
    built_in: Arc<FontDatabase>,
    locator: Arc<dyn FontLocator + Send + Sync>,
    config: ConfigHandle,
}

impl FallbackResolveInfo {
    fn process(self) {
        let fallback_str = self.no_glyphs.iter().collect::<String>();
        // The codepoints arrive sorted and deduped from shape_impl; keep a
        // copy as the memo key before `wanted` consumes the list below.
        let memo_glyphs = self.no_glyphs.clone();
        let mut extra_handles = vec![];

        log::trace!(
            "Looking for {} in fallback fonts",
            fallback_str.escape_unicode()
        );

        match self.locator.locate_fallback_for_codepoints(&self.no_glyphs) {
            Ok(ref mut handles) => extra_handles.append(handles),
            Err(err) => log::error!(
                "Error: {:#} while resolving fallback for {} from font-locator",
                err,
                fallback_str.escape_unicode()
            ),
        }

        if self.config.search_font_dirs_for_fallback {
            match self
                .font_dirs
                .locate_fallback_for_codepoints(&self.no_glyphs)
            {
                Ok(ref mut handles) => extra_handles.append(handles),
                Err(err) => log::error!(
                    "Error: {:#} while resolving fallback for {} from font_dirs",
                    err,
                    fallback_str.escape_unicode()
                ),
            }
        }

        match self
            .built_in
            .locate_fallback_for_codepoints(&self.no_glyphs)
        {
            Ok(ref mut handles) => extra_handles.append(handles),
            Err(err) => log::error!(
                "Error: {:#} while resolving fallback for {} for built-in fonts",
                err,
                fallback_str.escape_unicode()
            ),
        }

        let mut wanted = RangeSet::new();
        for c in self.no_glyphs {
            wanted.add(c as u32);
        }
        log::trace!(
            "Fallback fonts that match {} before sorting are: {:#?}",
            fallback_str.escape_unicode(),
            extra_handles
        );

        if wanted.len() > 1 && self.config.sort_fallback_fonts_by_coverage {
            // Sort by ascending coverage
            extra_handles.sort_by_cached_key(|p| {
                p.coverage_intersection(&wanted)
                    .map(|r| r.len())
                    .unwrap_or(0)
            });
            // Re-arrange to descending coverage
            extra_handles.reverse();
            log::trace!(
                "Fallback fonts that match {} after sorting are: {:#?}",
                fallback_str.escape_unicode(),
                extra_handles
            );
        }

        // iteratively reduce to just the fonts that we need
        extra_handles.retain(|p| match p.coverage_intersection(&wanted) {
            Ok(cov) if cov.is_empty() => false,
            Ok(cov) => {
                // Remove the matches from the set, so that we avoid
                // picking up multiple fonts for the same glyphs
                wanted = wanted.difference(&cov);
                true
            }
            Err(_) => false,
        });

        // Remember the outcome -- including an empty one -- so the next
        // LoadedFont missing these same codepoints resolves synchronously
        // instead of repeating this search and clearing every shape cache.
        FALLBACK_RESOLVE_MEMO.lock().unwrap().insert(
            self.config.generation(),
            memo_glyphs.clone(),
            extra_handles.clone(),
        );

        if !extra_handles.is_empty() {
            let mut pending = self.pending.lock().unwrap();
            pending.append(&mut extra_handles);
            (self.completion)(&memo_glyphs);
        }

        if !wanted.is_empty() {
            // There were some glyphs we couldn't resolve!
            let fallback_str = wanted
                .iter_values()
                .map(|c| std::char::from_u32(c).unwrap_or(' '))
                .collect::<String>();

            let current_gen = self.config.generation();
            let show_warning = self.config.warn_about_missing_glyphs
                && LAST_WARNING
                    .lock()
                    .unwrap()
                    .map(|(instant, generation)| {
                        generation != current_gen
                            || instant.elapsed() > Duration::from_secs(60 * 60)
                    })
                    .unwrap_or(true);

            if show_warning {
                LAST_WARNING
                    .lock()
                    .unwrap()
                    .replace((Instant::now(), self.config.generation()));
                let url = "https://wezterm.org/config/fonts.html";
                log::warn!(
                    "No fonts contain glyphs for these codepoints: {}.\n\
                     Placeholder glyphs are being displayed instead.\n\
                     You may wish to install additional fonts, or adjust your\n\
                     configuration so that it can find them.\n\
                     {} has more information about configuring fonts.\n\
                     Set warn_about_missing_glyphs=false to suppress this message.",
                    fallback_str.escape_unicode(),
                    url,
                );

                ToastNotification {
                    title: "Font problem".to_string(),
                    message: format!(
                        "No fonts contain glyphs for these codepoints: {}.\n\
                            Placeholder glyphs are being displayed instead.\n\
                            You may wish to install additional fonts, or adjust\n\
                            your configuration so that it can find them.\n\
                            Set warn_about_missing_glyphs=false to suppress this\n\
                            message.",
                        fallback_str.escape_unicode()
                    ),
                    url: Some(url.to_string()),
                    timeout: Some(Duration::from_secs(15)),
                }
                .show();
            } else {
                log::debug!(
                    "No fonts contain glyphs for these codepoints: {}",
                    fallback_str.escape_unicode()
                );
            }
        }
    }
}

#[derive(Clone, Copy, Hash, PartialEq, Eq)]
enum Entity {
    Title,
    CommandPalette,
    CharSelect,
    PaneSelect,
}

struct FontConfigInner {
    fonts: RefCell<HashMap<TextStyle, Rc<LoadedFont>>>,
    metrics: RefCell<Option<FontMetrics>>,
    dpi: RefCell<usize>,
    font_scale: RefCell<f64>,
    config: RefCell<ConfigHandle>,
    locator: Arc<dyn FontLocator + Send + Sync>,
    font_dirs: RefCell<Arc<FontDatabase>>,
    built_in: RefCell<Arc<FontDatabase>>,
    title_font: RefCell<Option<Rc<LoadedFont>>>,
    pane_select_font: RefCell<Option<Rc<LoadedFont>>>,
    char_select_font: RefCell<Option<Rc<LoadedFont>>>,
    command_palette_font: RefCell<Option<Rc<LoadedFont>>>,
    entity_font_overrides: RefCell<HashMap<(Entity, u64, u16, bool), Rc<LoadedFont>>>,
    /// `UI_FONT_GENERATION` as it stood when the interface fonts above were
    /// built.
    ui_font_generation: Cell<usize>,
    fallback_channel: RefCell<Option<Sender<FallbackResolveInfo>>>,
}

/// Matches and loads fonts for a given input style
pub struct FontConfiguration {
    inner: Rc<FontConfigInner>,
}

impl FontConfigInner {
    /// Create a new empty configuration
    pub fn new(config: Option<ConfigHandle>, dpi: usize) -> anyhow::Result<Self> {
        let config = config.unwrap_or_else(configuration);
        let locator = new_locator(config.font_locator);
        Ok(Self {
            fonts: RefCell::new(HashMap::new()),
            locator,
            metrics: RefCell::new(None),
            title_font: RefCell::new(None),
            pane_select_font: RefCell::new(None),
            char_select_font: RefCell::new(None),
            command_palette_font: RefCell::new(None),
            entity_font_overrides: RefCell::new(HashMap::new()),
            ui_font_generation: Cell::new(
                UI_FONT_GENERATION.load(::std::sync::atomic::Ordering::Relaxed),
            ),
            font_scale: RefCell::new(1.0),
            dpi: RefCell::new(dpi),
            config: RefCell::new(config.clone()),
            font_dirs: RefCell::new(Arc::new(FontDatabase::with_font_dirs(&config)?)),
            built_in: RefCell::new(FontDatabase::shared_built_in()?),
            fallback_channel: RefCell::new(None),
        })
    }

    fn config_changed(&self, config: &ConfigHandle) -> anyhow::Result<()> {
        let mut fonts = self.fonts.borrow_mut();
        *self.config.borrow_mut() = config.clone();
        // Config was reloaded, invalidate our caches
        fonts.clear();
        self.title_font.borrow_mut().take();
        self.pane_select_font.borrow_mut().take();
        self.char_select_font.borrow_mut().take();
        self.command_palette_font.borrow_mut().take();
        self.entity_font_overrides.borrow_mut().clear();
        self.metrics.borrow_mut().take();
        *self.font_dirs.borrow_mut() = Arc::new(FontDatabase::with_font_dirs(config)?);
        Ok(())
    }

    fn schedule_fallback_resolve<F: FnOnce(&[char]) + Send + 'static>(
        &self,
        no_glyphs: Vec<char>,
        pending: &Arc<Mutex<Vec<ParsedFont>>>,
        completion: F,
    ) {
        if no_glyphs.is_empty() {
            return;
        }

        let info = FallbackResolveInfo {
            completion: Box::new(completion),
            no_glyphs,
            pending: Arc::clone(pending),
            font_dirs: Arc::clone(&*self.font_dirs.borrow()),
            built_in: Arc::clone(&*self.built_in.borrow()),
            locator: Arc::clone(&self.locator),
            config: self.config.borrow().clone(),
        };

        let mut fallback = self.fallback_channel.borrow_mut();

        if fallback.is_none() {
            let (tx, rx) = channel::<FallbackResolveInfo>();

            std::thread::spawn(move || {
                for info in rx {
                    info.process();
                }
            });

            fallback.replace(tx);
        }

        if let Err(err) = fallback.as_mut().expect("channel to exist").send(info) {
            log::error!("Failed to schedule font fallback resolve: {:#}", err);
        }
    }

    fn compute_title_font(&self, config: &ConfigHandle, make_bold: bool) -> (TextStyle, f64) {
        fn weighted(family: &str, weight: FontWeight) -> FontAttributes {
            FontAttributes {
                family: family.to_string(),
                weight,
                ..Default::default()
            }
        }

        let mut fonts = vec![];

        // Their own choice of family goes first, and the system's stays
        // behind it for whatever theirs has no glyph for -- or for all of
        // it, should theirs have been uninstalled since.
        if let Some(family) = ui_font_family() {
            fonts.push(weighted(
                &family,
                if make_bold {
                    FontWeight::MEDIUM
                } else {
                    FontWeight::REGULAR
                },
            ));
        }
        let chosen = !fonts.is_empty();

        let mut system = if cfg!(target_os = "macos") {
            weighted(
                ".AppleSystemUIFont",
                if make_bold {
                    FontWeight::MEDIUM
                } else {
                    FontWeight::REGULAR
                },
            )
        } else if make_bold {
            weighted("Roboto", FontWeight::BOLD)
        } else {
            FontAttributes::new("Roboto")
        };
        system.is_fallback = chosen;
        fonts.push(system);

        if cfg!(target_os = "macos") {
            let mut fallback = weighted(
                "Helvetica Neue",
                if make_bold {
                    FontWeight::MEDIUM
                } else {
                    FontWeight::REGULAR
                },
            );
            fallback.is_fallback = true;
            fonts.push(fallback);

            let mut fallback = weighted(".Lucida Grande UI", FontWeight::MEDIUM);
            fallback.is_fallback = true;
            fonts.push(fallback);
        }

        // Fallback to their main font selection, so that we can pick up
        // any fallback fonts they might have configured in the main
        // config and so that they don't have to replicate that list for
        // the title font.
        for font in &config.font.font {
            let mut font = font.clone();
            font.is_fallback = true;
            fonts.push(font);
        }

        let font_size = if cfg!(windows) { 11. } else { 14. };

        (
            TextStyle {
                foreground: None,
                font: fonts,
            },
            font_size,
        )
    }

    fn make_entity_font_impl(
        &self,
        myself: &Rc<Self>,
        entity: Entity,
        override_font_size: Option<f64>,
        override_weight: Option<config::FontWeight>,
        override_italic: bool,
    ) -> anyhow::Result<Rc<LoadedFont>> {
        let config = self.config.borrow();
        self.make_entity_font_from(
            myself,
            &config,
            entity,
            override_font_size,
            override_weight,
            override_italic,
        )
    }

    /// `make_entity_font_impl` from `config` rather than the configuration
    /// this was last handed.
    fn make_entity_font_from(
        &self,
        myself: &Rc<Self>,
        config: &ConfigHandle,
        entity: Entity,
        override_font_size: Option<f64>,
        override_weight: Option<config::FontWeight>,
        override_italic: bool,
    ) -> anyhow::Result<Rc<LoadedFont>> {
        let make_bold = entity != Entity::CommandPalette;
        let (sys_font, sys_size) = self.compute_title_font(&config, make_bold);

        let (configured_font_size, text_style) = match entity {
            Entity::Title => (config.window_frame.font_size.unwrap_or(sys_size), None),
            Entity::CommandPalette => (
                config.command_palette_font_size,
                config.command_palette_font.as_ref(),
            ),
            Entity::CharSelect => (
                config.char_select_font_size,
                config.char_select_font.as_ref(),
            ),
            Entity::PaneSelect => (
                config.pane_select_font_size,
                config.pane_select_font.as_ref(),
            ),
        };
        let font_size = override_font_size.unwrap_or(configured_font_size);

        // A family chosen in Settings is the newer word on the interface's
        // font, and outranks what the configuration file asks for.
        let text_style = if ui_font_family().is_some()
            && matches!(entity, Entity::Title | Entity::CommandPalette)
        {
            &sys_font
        } else {
            text_style.unwrap_or(config.window_frame.font.as_ref().unwrap_or(&sys_font))
        };
        let override_style;
        let text_style = if override_weight.is_some() || override_italic {
            override_style = {
                let mut style = text_style.clone();
                for attr in &mut style.font {
                    if let Some(weight) = override_weight {
                        attr.weight = weight;
                    }
                    if override_italic {
                        attr.style = config::FontStyle::Italic;
                    }
                }
                style
            };
            &override_style
        } else {
            text_style
        };

        self.build_font(myself, &config, text_style, font_size)
    }

    /// `text_style` at `font_size` points, as a new font owned by the
    /// caller; nothing here keeps it.
    fn build_font(
        &self,
        myself: &Rc<Self>,
        config: &ConfigHandle,
        text_style: &TextStyle,
        font_size: f64,
    ) -> anyhow::Result<Rc<LoadedFont>> {
        let dpi = *self.dpi.borrow() as u32;
        let pixel_size = (font_size * dpi as f64 / 72.0) as u16;

        let attributes = text_style.font_with_fallback();
        let (handles, _loaded) = self.resolve_font_helper_impl(&attributes, pixel_size)?;

        let shaper = new_shaper(config, &handles)?;

        let metrics = shaper.metrics(font_size, dpi).with_context(|| {
            format!(
                "obtaining metrics for font_size={} @ dpi {}",
                font_size, dpi
            )
        })?;

        let loaded = Rc::new(LoadedFont {
            rasterizers: RefCell::new(HashMap::new()),
            handles: RefCell::new(handles),
            shaper: RefCell::new(shaper),
            metrics,
            font_size,
            dpi,
            font_config: Rc::downgrade(myself),
            pending_fallback: Arc::new(Mutex::new(vec![])),
            text_style: text_style.clone(),
            id: alloc_font_id(),
            tried_glyphs: RefCell::new(HashSet::new()),
            pixel_geometry: config.display_pixel_geometry,
        });

        Ok(loaded)
    }

    /// Drop the interface fonts built for a family that has since been
    /// changed with `set_ui_font_family`.
    fn forget_stale_ui_fonts(&self) {
        let generation = UI_FONT_GENERATION.load(::std::sync::atomic::Ordering::Relaxed);
        if self.ui_font_generation.replace(generation) != generation {
            self.title_font.borrow_mut().take();
            self.pane_select_font.borrow_mut().take();
            self.char_select_font.borrow_mut().take();
            self.command_palette_font.borrow_mut().take();
            self.entity_font_overrides.borrow_mut().clear();
        }
    }

    fn title_font(&self, myself: &Rc<Self>) -> anyhow::Result<Rc<LoadedFont>> {
        self.forget_stale_ui_fonts();
        let mut title_font = self.title_font.borrow_mut();

        if let Some(entry) = title_font.as_ref() {
            return Ok(Rc::clone(entry));
        }

        let loaded = self.make_entity_font_impl(myself, Entity::Title, None, None, false)?;

        title_font.replace(Rc::clone(&loaded));

        Ok(loaded)
    }

    fn command_palette_font(&self, myself: &Rc<Self>) -> anyhow::Result<Rc<LoadedFont>> {
        self.forget_stale_ui_fonts();
        let mut command_palette_font = self.command_palette_font.borrow_mut();

        if let Some(entry) = command_palette_font.as_ref() {
            return Ok(Rc::clone(entry));
        }

        let loaded =
            self.make_entity_font_impl(myself, Entity::CommandPalette, None, None, false)?;

        command_palette_font.replace(Rc::clone(&loaded));

        Ok(loaded)
    }

    fn char_select_font(&self, myself: &Rc<Self>) -> anyhow::Result<Rc<LoadedFont>> {
        self.forget_stale_ui_fonts();
        let mut char_select_font = self.char_select_font.borrow_mut();

        if let Some(entry) = char_select_font.as_ref() {
            return Ok(Rc::clone(entry));
        }

        let loaded = self.make_entity_font_impl(myself, Entity::CharSelect, None, None, false)?;

        char_select_font.replace(Rc::clone(&loaded));

        Ok(loaded)
    }

    fn pane_select_font(&self, myself: &Rc<Self>) -> anyhow::Result<Rc<LoadedFont>> {
        self.forget_stale_ui_fonts();
        let mut pane_select_font = self.pane_select_font.borrow_mut();

        if let Some(entry) = pane_select_font.as_ref() {
            return Ok(Rc::clone(entry));
        }

        let loaded = self.make_entity_font_impl(myself, Entity::PaneSelect, None, None, false)?;

        pane_select_font.replace(Rc::clone(&loaded));

        Ok(loaded)
    }

    fn entity_font_with_size(
        &self,
        myself: &Rc<Self>,
        entity: Entity,
        font_size: f64,
        font_weight: Option<u16>,
        italic: bool,
    ) -> anyhow::Result<Rc<LoadedFont>> {
        self.forget_stale_ui_fonts();
        let font_size = font_size.clamp(6.0, 72.0);
        let font_weight = font_weight.unwrap_or(0);
        let key = (entity, font_size.to_bits(), font_weight, italic);
        if let Some(entry) = self.entity_font_overrides.borrow().get(&key) {
            return Ok(Rc::clone(entry));
        }
        let loaded = self.make_entity_font_impl(
            myself,
            entity,
            Some(font_size),
            (font_weight > 0).then(|| config::FontWeight::from_opentype_weight(font_weight)),
            italic,
        )?;
        self.entity_font_overrides
            .borrow_mut()
            .insert(key, Rc::clone(&loaded));
        Ok(loaded)
    }

    fn resolve_font_helper_impl(
        &self,
        attributes: &[FontAttributes],
        pixel_size: u16,
    ) -> anyhow::Result<(Vec<ParsedFont>, HashSet<FontAttributes>)> {
        let preferred_attributes = attributes
            .iter()
            .filter(|a| !a.is_fallback)
            .cloned()
            .collect::<Vec<_>>();
        let fallback_attributes = attributes
            .iter()
            .filter(|a| a.is_fallback)
            .cloned()
            .collect::<Vec<_>>();
        let mut loaded = HashSet::new();
        let mut handles = vec![];

        for &attrs in &[&preferred_attributes, &fallback_attributes] {
            let mut candidates = vec![];

            let font_dirs = self.font_dirs.borrow();
            for attr in attrs {
                candidates.append(&mut font_dirs.candidates(attr));
            }

            let mut loaded_ignored = HashSet::new();
            let located = self
                .locator
                .load_fonts(attrs, &mut loaded_ignored, pixel_size)?;
            for font in &located {
                candidates.push(font);
            }

            let built_in = self.built_in.borrow();
            for attr in attrs {
                candidates.append(&mut built_in.candidates(attr));
            }

            let mut is_fallback = false;

            for attr in attrs {
                if attr.is_fallback {
                    is_fallback = true;
                }

                if loaded.contains(attr) {
                    continue;
                }
                let named_candidates: Vec<&ParsedFont> = candidates
                    .iter()
                    .filter_map(|&p| if p.matches_name(attr) { Some(p) } else { None })
                    .collect();
                if let Some(idx) =
                    ParsedFont::best_matching_index(attr, &named_candidates, pixel_size)
                {
                    named_candidates.get(idx).map(|&p| {
                        loaded.insert(attr.clone());
                        handles.push(p.clone().synthesize(attr))
                    });
                }
            }

            if !is_fallback && loaded.is_empty() {
                // We didn't explicitly match any names.
                // When using fontconfig, the system may have expanded a family name
                // like "monospace" into the real font, in which case we wouldn't have
                // found a match in the `named_candidates` vec above, because of the
                // name mismatch.
                // So what we do now is make a second pass over all the located candidates,
                // ignoring their names, and just match based on font attributes.
                let located_candidates: Vec<_> = located.iter().collect();
                for attr in attrs {
                    if let Some(idx) =
                        ParsedFont::best_matching_index(attr, &located_candidates, pixel_size)
                    {
                        located_candidates.get(idx).map(|&p| {
                            loaded.insert(attr.clone());
                            handles.push(p.clone().synthesize(attr))
                        });
                    }
                }
            }
        }

        Ok((handles, loaded))
    }

    fn resolve_font_helper(
        &self,
        style: &TextStyle,
        config: &ConfigHandle,
        pixel_size: u16,
    ) -> anyhow::Result<(Box<dyn FontShaper>, Vec<ParsedFont>)> {
        let attributes = style.font_with_fallback();

        let (handles, loaded) = self.resolve_font_helper_impl(&attributes, pixel_size)?;

        for attr in &attributes {
            if !attr.is_synthetic && !attr.is_fallback && !loaded.contains(attr) {
                let styled_extra = if attr.weight != FontWeight::default()
                    || attr.style != FontStyle::default()
                    || attr.stretch != FontStretch::default()
                {
                    ". An alternative variant of the font was requested; \
                    TrueType and OpenType fonts don't have an automatic way to \
                    produce these font variants, so a separate font file containing \
                    the bold or italic variant must be installed"
                } else {
                    ""
                };

                let is_primary = config.font.font.iter().any(|a| a == attr);
                let derived_from_primary = config.font.font.iter().any(|a| a.family == attr.family);

                let explanation = if is_primary {
                    // This is the primary font selection
                    format!(
                        "Unable to load a font specified by your font={} configuration",
                        attr
                    )
                } else if derived_from_primary {
                    // it came from font_rules and may have been derived from
                    // their primary font (we can't know for sure)
                    format!(
                        "Unable to load a font matching one of your font_rules: {}. \
                        Note that wezterm will synthesize font_rules to select bold \
                        and italic fonts based on your primary font configuration",
                        attr
                    )
                } else {
                    format!(
                        "Unable to load a font matching one of your font_rules: {}",
                        attr
                    )
                };

                config::show_error(&format!(
                    "{}. Fallback(s) are being used instead, and the terminal \
                    may not render as intended{}. See \
                    https://wezterm.org/config/fonts.html for more information",
                    explanation, styled_extra
                ));
            }
        }

        Ok((new_shaper(&*config, &handles)?, handles))
    }

    /// Given a text style, load (with caching) the font that best
    /// matches according to the fontconfig pattern.
    fn resolve_font(&self, myself: &Rc<Self>, style: &TextStyle) -> anyhow::Result<Rc<LoadedFont>> {
        let config = self.config.borrow();
        let is_default = *style == config.font;
        let def_font = if !is_default && config.use_cap_height_to_scale_fallback_fonts {
            Some(self.default_font(myself)?)
        } else {
            None
        };

        let mut fonts = self.fonts.borrow_mut();

        if let Some(entry) = fonts.get(style) {
            return Ok(Rc::clone(entry));
        }

        let mut font_size = config.font_size * *self.font_scale.borrow();
        let dpi = *self.dpi.borrow() as u32;
        let pixel_size = (font_size * dpi as f64 / 72.0) as u16;

        let (mut shaper, mut handles) = self.resolve_font_helper(style, &config, pixel_size)?;

        let mut metrics = shaper.metrics(font_size, dpi).with_context(|| {
            format!(
                "obtaining metrics for font_size={} @ dpi {}",
                font_size, dpi
            )
        })?;

        if let Some(def_font) = def_font {
            let def_metrics = def_font.metrics();
            match (def_metrics.cap_height, metrics.cap_height) {
                (Some(d), Some(m)) => {
                    // Scale by the ratio of the pixel heights of the default
                    // and this font; this causes the `I` glyphs to appear to
                    // have the same height.
                    let scale = d.get() / m.get();
                    if scale != 1.0 {
                        let scaled_pixel_size = (pixel_size as f64 * scale) as u16;
                        let scaled_font_size = font_size * scale;
                        log::trace!(
                            "using cap height adjusted: pixel_size {} -> {}, font_size {} -> {}, {:?}",
                            pixel_size,
                            scaled_pixel_size,
                            font_size,
                            scaled_font_size,
                            metrics,
                        );
                        let (alt_shaper, alt_handles) =
                            self.resolve_font_helper(style, &config, scaled_pixel_size)?;
                        shaper = alt_shaper;
                        handles = alt_handles;

                        metrics = shaper.metrics(scaled_font_size, dpi).with_context(|| {
                            format!(
                                "obtaining cap-height adjusted metrics for font_size={} @ dpi {}",
                                scaled_font_size, dpi
                            )
                        })?;

                        font_size = scaled_font_size;
                    }
                }
                _ => {}
            }
        }

        let loaded = Rc::new(LoadedFont {
            rasterizers: RefCell::new(HashMap::new()),
            handles: RefCell::new(handles),
            shaper: RefCell::new(shaper),
            metrics,
            font_size,
            dpi,
            font_config: Rc::downgrade(myself),
            pending_fallback: Arc::new(Mutex::new(vec![])),
            text_style: style.clone(),
            id: alloc_font_id(),
            tried_glyphs: RefCell::new(HashSet::new()),
            pixel_geometry: config.display_pixel_geometry,
        });

        fonts.insert(style.clone(), Rc::clone(&loaded));

        Ok(loaded)
    }

    pub fn change_scaling(&self, font_scale: f64, dpi: usize) -> (f64, usize) {
        let prior_font = *self.font_scale.borrow();
        let prior_dpi = *self.dpi.borrow();
        let dpi_changed = prior_dpi != dpi;

        *self.dpi.borrow_mut() = dpi;
        *self.font_scale.borrow_mut() = font_scale;
        self.fonts.borrow_mut().clear();
        self.metrics.borrow_mut().take();

        if dpi_changed {
            self.title_font.borrow_mut().take();
            self.pane_select_font.borrow_mut().take();
            self.char_select_font.borrow_mut().take();
            self.command_palette_font.borrow_mut().take();
            // Sized entity fonts are rasterized for the DPI that was active
            // when they were cached. Reusing them after moving a window
            // between Retina and non-Retina displays leaves the custom UI
            // text at the old backing scale while its controls use the new
            // scale.
            self.entity_font_overrides.borrow_mut().clear();
        }

        (prior_font, prior_dpi)
    }

    /// Returns the baseline font specified in the configuration
    pub fn default_font(&self, myself: &Rc<Self>) -> anyhow::Result<Rc<LoadedFont>> {
        self.resolve_font(myself, &self.config.borrow().font)
    }

    pub fn get_font_scale(&self) -> f64 {
        *self.font_scale.borrow()
    }

    pub fn get_dpi(&self) -> usize {
        *self.dpi.borrow()
    }

    pub fn default_font_metrics(&self, myself: &Rc<Self>) -> Result<FontMetrics, Error> {
        {
            let metrics = self.metrics.borrow();
            if let Some(metrics) = metrics.as_ref() {
                return Ok(*metrics);
            }
        }

        let font = self.default_font(myself)?;
        let metrics = font.metrics();

        *self.metrics.borrow_mut() = Some(metrics);

        Ok(metrics)
    }

    /// Apply the defined font_rules from the user configuration to
    /// produce the text style that best matches the supplied input
    /// cell attributes.
    pub fn match_style<'a>(
        &self,
        config: &'a ConfigHandle,
        attrs: &CellAttributes,
    ) -> &'a TextStyle {
        // a little macro to avoid boilerplate for matching the rules.
        // If the rule doesn't specify a value for an attribute then
        // it will implicitly match.  If it specifies an attribute
        // then it has to have the same value as that in the input attrs.
        macro_rules! attr_match {
            ($ident:ident, $rule:expr) => {
                if let Some($ident) = $rule.$ident {
                    if $ident != attrs.$ident() {
                        // Does not match
                        continue;
                    }
                }
                // matches so far...
            };
        }

        let would_bright = match attrs.foreground() {
            wezterm_term::color::ColorAttribute::PaletteIndex(idx) if idx < 8 => {
                attrs.intensity() == Intensity::Bold
            }
            _ => false,
        };

        for rule in &config.font_rules {
            if let Some(intensity) = rule.intensity {
                let effective_intensity = match config.bold_brightens_ansi_colors {
                    BoldBrightening::BrightOnly if would_bright => Intensity::Normal,
                    BoldBrightening::No
                    | BoldBrightening::BrightAndBold
                    | BoldBrightening::BrightOnly => attrs.intensity(),
                };
                if intensity != effective_intensity {
                    // Rule does not match
                    continue;
                }
                // matches so far
            }
            attr_match!(underline, &rule);
            attr_match!(italic, &rule);
            attr_match!(blink, &rule);
            attr_match!(reverse, &rule);
            attr_match!(strikethrough, &rule);
            attr_match!(invisible, &rule);

            // If we get here, then none of the rules didn't match,
            // so we therefore assume that it did match overall.
            return &rule.font;
        }
        &config.font
    }
}

impl FontConfiguration {
    /// Create a new empty configuration
    pub fn new(config: Option<ConfigHandle>, dpi: usize) -> anyhow::Result<Self> {
        let inner = Rc::new(FontConfigInner::new(config, dpi)?);
        Ok(Self { inner })
    }

    pub fn config_changed(&self, config: &ConfigHandle) -> anyhow::Result<()> {
        self.inner.config_changed(config)
    }

    pub fn config(&self) -> ConfigHandle {
        self.inner.config.borrow().clone()
    }

    pub fn title_font(&self) -> anyhow::Result<Rc<LoadedFont>> {
        self.inner.title_font(&self.inner)
    }

    pub fn title_font_with_size(&self, font_size: f64) -> anyhow::Result<Rc<LoadedFont>> {
        self.inner
            .entity_font_with_size(&self.inner, Entity::Title, font_size, None, false)
    }

    pub fn title_font_with_size_and_weight(
        &self,
        font_size: f64,
        font_weight: u16,
    ) -> anyhow::Result<Rc<LoadedFont>> {
        self.inner.entity_font_with_size(
            &self.inner,
            Entity::Title,
            font_size,
            Some(font_weight),
            false,
        )
    }

    pub fn title_font_with_size_weight_and_italic(
        &self,
        font_size: f64,
        font_weight: u16,
    ) -> anyhow::Result<Rc<LoadedFont>> {
        self.inner.entity_font_with_size(
            &self.inner,
            Entity::Title,
            font_size,
            Some(font_weight),
            true,
        )
    }

    pub fn command_palette_font(&self) -> anyhow::Result<Rc<LoadedFont>> {
        self.inner.command_palette_font(&self.inner)
    }

    pub fn command_palette_font_with_size(&self, font_size: f64) -> anyhow::Result<Rc<LoadedFont>> {
        self.inner.entity_font_with_size(
            &self.inner,
            Entity::CommandPalette,
            font_size,
            None,
            false,
        )
    }

    pub fn command_palette_font_with_size_and_weight(
        &self,
        font_size: f64,
        font_weight: u16,
    ) -> anyhow::Result<Rc<LoadedFont>> {
        self.inner.entity_font_with_size(
            &self.inner,
            Entity::CommandPalette,
            font_size,
            Some(font_weight),
            false,
        )
    }

    pub fn pane_select_font(&self) -> anyhow::Result<Rc<LoadedFont>> {
        self.inner.pane_select_font(&self.inner)
    }

    /// The terminal's font at `font_size` points as `config` describes it,
    /// built for the caller and not cached here: a preview decides how long
    /// it lives. `config` rather than the one this holds, which is as old
    /// as the window that made it.
    pub fn terminal_font_uncached(
        &self,
        config: &ConfigHandle,
        font_size: f64,
    ) -> anyhow::Result<Rc<LoadedFont>> {
        self.inner.build_font(
            &self.inner,
            config,
            &config.font,
            font_size.clamp(6.0, 72.0),
        )
    }

    /// `command_palette_font_with_size_and_weight` as `config` describes it,
    /// built for the caller and not cached here, like
    /// `terminal_font_uncached`.
    pub fn command_palette_font_uncached(
        &self,
        config: &ConfigHandle,
        font_size: f64,
        font_weight: u16,
    ) -> anyhow::Result<Rc<LoadedFont>> {
        self.inner.make_entity_font_from(
            &self.inner,
            config,
            Entity::CommandPalette,
            Some(font_size.clamp(6.0, 72.0)),
            (font_weight > 0).then(|| config::FontWeight::from_opentype_weight(font_weight)),
            false,
        )
    }

    pub fn char_select_font(&self) -> anyhow::Result<Rc<LoadedFont>> {
        self.inner.char_select_font(&self.inner)
    }

    /// Given a text style, load (with caching) the font that best
    /// matches according to the fontconfig pattern.
    pub fn resolve_font(&self, style: &TextStyle) -> anyhow::Result<Rc<LoadedFont>> {
        self.inner.resolve_font(&self.inner, style)
    }

    pub fn change_scaling(&self, font_scale: f64, dpi: usize) -> (f64, usize) {
        self.inner.change_scaling(font_scale, dpi)
    }

    /// Returns the baseline font specified in the configuration
    pub fn default_font(&self) -> anyhow::Result<Rc<LoadedFont>> {
        self.inner.default_font(&self.inner)
    }

    pub fn get_font_scale(&self) -> f64 {
        self.inner.get_font_scale()
    }

    pub fn get_dpi(&self) -> usize {
        self.inner.get_dpi()
    }

    pub fn default_font_metrics(&self) -> Result<FontMetrics, Error> {
        self.inner.default_font_metrics(&self.inner)
    }

    pub fn list_fonts_in_font_dirs(&self) -> Vec<ParsedFont> {
        let mut font_dirs = self.inner.font_dirs.borrow().list_available();
        let mut built_in = self.inner.built_in.borrow().list_available();

        font_dirs.append(&mut built_in);
        font_dirs.sort();
        font_dirs
    }

    pub fn list_system_fonts(&self) -> anyhow::Result<Vec<ParsedFont>> {
        self.inner.locator.enumerate_all_fonts()
    }

    /// Every family there is to choose from -- the system's, those in the
    /// configured font directories and the built-in ones -- by name, in
    /// alphabetical order. Families the system keeps for itself, whose names
    /// start with a dot, are left out.
    pub fn list_font_families(&self) -> Vec<String> {
        let mut families = self
            .inner
            .locator
            .enumerate_family_names()
            .unwrap_or_else(|err| {
                log::warn!("listing the system's font families: {err:#}");
                vec![]
            });
        families.extend(
            self.list_fonts_in_font_dirs()
                .into_iter()
                .map(|font| font.names().family.clone()),
        );
        families.retain(|family| !family.is_empty() && !family.starts_with('.'));
        families.sort_by_cached_key(|family| (family.to_lowercase(), family.clone()));
        families.dedup();
        families
    }

    /// Apply the defined font_rules from the user configuration to
    /// produce the text style that best matches the supplied input
    /// cell attributes.
    pub fn match_style<'a>(
        &self,
        config: &'a ConfigHandle,
        attrs: &CellAttributes,
    ) -> &'a TextStyle {
        self.inner.match_style(config, attrs)
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    /// The interface's family is the whole process's, and a test that
    /// changes it would drop the fonts another is holding to compare.
    static UI_FONT_FAMILY_IN_USE: Mutex<()> = Mutex::new(());

    #[test]
    fn dpi_change_invalidates_sized_entity_font_cache() -> anyhow::Result<()> {
        let _family = UI_FONT_FAMILY_IN_USE.lock().unwrap();
        let fonts = FontConfiguration::new(Some(ConfigHandle::default_config()), 144)?;

        let retina = fonts.title_font_with_size(15.0)?;
        assert_eq!(retina.dpi, 144);
        let retina_id = retina.id();
        assert!(Rc::ptr_eq(&retina, &fonts.title_font_with_size(15.0)?));

        fonts.change_scaling(1.0, 72);
        let standard = fonts.title_font_with_size(15.0)?;
        assert_eq!(standard.dpi, 72);
        assert_ne!(standard.id(), retina_id);
        assert!(Rc::ptr_eq(&standard, &fonts.title_font_with_size(15.0)?));

        fonts.change_scaling(1.0, 144);
        let retina_again = fonts.title_font_with_size(15.0)?;
        assert_eq!(retina_again.dpi, 144);
        assert_ne!(retina_again.id(), standard.id());

        Ok(())
    }

    #[test]
    fn the_interface_is_drawn_with_the_family_chosen_for_it() -> anyhow::Result<()> {
        fn family(font: anyhow::Result<Rc<LoadedFont>>) -> anyhow::Result<String> {
            Ok(font?.clone_handles()[0].names().family.clone())
        }

        let _family = UI_FONT_FAMILY_IN_USE.lock().unwrap();
        let fonts = FontConfiguration::new(Some(ConfigHandle::default_config()), 144)?;

        // Not Helvetica Neue, which was next in the list while the system's
        // own font could not be found.
        let system = family(fonts.title_font_with_size(15.0))?;
        assert_ne!(system, "Helvetica Neue");
        assert_eq!(family(fonts.command_palette_font_with_size(15.0))?, system);

        // A window that already holds fonts takes the change up as well.
        assert!(set_ui_font_family(Some(" Menlo ".to_string())));
        assert!(!set_ui_font_family(Some("Menlo".to_string())));
        let chosen = fonts.title_font_with_size(15.0)?;
        assert_eq!(family(Ok(Rc::clone(&chosen)))?, "Menlo");
        assert_eq!(family(fonts.title_font())?, "Menlo");
        assert_eq!(family(fonts.command_palette_font_with_size(15.0))?, "Menlo");
        assert!(Rc::ptr_eq(&chosen, &fonts.title_font_with_size(15.0)?));

        // An empty name is no choice.
        assert!(set_ui_font_family(Some(String::new())));
        assert_eq!(ui_font_family(), None);
        assert_eq!(family(fonts.title_font_with_size(15.0))?, system);

        Ok(())
    }
}

#[cfg(test)]
mod fallback_memo_test {
    use super::*;

    #[test]
    fn memo_is_scoped_to_one_generation_and_bounded() {
        let mut memo = FallbackResolveMemo::default();
        memo.insert(1, vec!['a'], vec![]);
        assert!(memo.get(1, &['a']).is_some());
        assert!(memo.get(2, &['a']).is_none(), "other generations must miss");

        memo.insert(2, vec!['b'], vec![]);
        assert!(
            memo.get(1, &['a']).is_none(),
            "a new generation drops the old one's entries"
        );
        assert!(memo.get(2, &['b']).is_some());

        memo.insert(1, vec!['c'], vec![]);
        assert!(
            memo.get(2, &['b']).is_some(),
            "a straggler from an older generation must not clear newer entries"
        );
        assert!(memo.get(1, &['c']).is_none());
        assert!(memo.get(2, &['c']).is_none());

        for n in 0..FALLBACK_RESOLVE_MEMO_CAP as u32 {
            memo.insert(2, vec![char::from_u32(0x4e00 + n).unwrap()], vec![]);
        }
        assert!(memo.entries.len() <= FALLBACK_RESOLVE_MEMO_CAP);
        memo.insert(2, vec!['z'], vec![]);
        assert!(memo.get(2, &['z']).is_some());
        assert!(memo.entries.len() <= FALLBACK_RESOLVE_MEMO_CAP);
    }
}
