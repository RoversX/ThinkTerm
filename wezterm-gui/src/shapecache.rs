use crate::customglyph::BlockKey;
use crate::glyphcache::CachedGlyph;
use config::TextStyle;
use std::rc::Rc;
use wezterm_font::shaper::GlyphInfo;
use wezterm_font::units::*;

#[derive(PartialEq, Eq, Hash, Clone, Debug)]
pub struct ShapeCacheKey {
    pub font_identity: u64,
    pub style: TextStyle,
    pub text: String,
}

#[derive(Debug, PartialEq)]
pub struct GlyphPosition {
    pub glyph_idx: u32,
    pub num_cells: u8,
    pub x_offset: PixelLength,
    pub bearing_x: f32,
    pub bitmap_pixel_width: u32,
}

#[derive(Debug)]
pub struct ShapedInfo {
    pub glyph: Rc<CachedGlyph>,
    pub pos: GlyphPosition,
    pub cluster: usize,
    pub block_key: Option<BlockKey>,
}

impl ShapedInfo {
    /// Process the results from the shaper, stitching together glyph
    /// and positioning information
    pub fn process(infos: &[GlyphInfo], glyphs: &[Rc<CachedGlyph>]) -> Vec<ShapedInfo> {
        let mut pos: Vec<ShapedInfo> = Vec::with_capacity(infos.len());

        for (info, glyph) in infos.iter().zip(glyphs.iter()) {
            pos.push(ShapedInfo {
                pos: GlyphPosition {
                    glyph_idx: info.glyph_pos,
                    bitmap_pixel_width: glyph
                        .texture
                        .as_ref()
                        .map_or(0, |t| t.coords.width() as u32),
                    num_cells: info.num_cells,
                    x_offset: info.x_offset,
                    bearing_x: glyph.bearing_x.get() as f32,
                },
                glyph: Rc::clone(glyph),
                cluster: info.cluster as usize,
                block_key: info.only_char.and_then(BlockKey::from_char),
            });
        }
        pos
    }
}

/// We'd like to avoid allocating when resolving from the cache
/// so this is the borrowed version of ShapeCacheKey.
/// It's a bit involved to make this work; more details can be
/// found in the excellent guide here:
/// <https://github.com/sunshowers/borrow-complex-key-example/blob/master/src/lib.rs>
#[derive(Copy, Debug, Clone, PartialEq, Eq, Hash)]
pub struct BorrowedShapeCacheKey<'a> {
    pub font_identity: u64,
    pub style: &'a TextStyle,
    pub text: &'a str,
}

impl<'a> BorrowedShapeCacheKey<'a> {
    pub fn to_owned(&self) -> ShapeCacheKey {
        ShapeCacheKey {
            font_identity: self.font_identity,
            style: self.style.clone(),
            text: self.text.to_owned(),
        }
    }
}

pub trait ShapeCacheKeyTrait: std::fmt::Debug {
    fn key<'k>(&'k self) -> BorrowedShapeCacheKey<'k>;
}

impl ShapeCacheKeyTrait for ShapeCacheKey {
    fn key<'k>(&'k self) -> BorrowedShapeCacheKey<'k> {
        BorrowedShapeCacheKey {
            font_identity: self.font_identity,
            style: &self.style,
            text: &self.text,
        }
    }
}

impl<'a> ShapeCacheKeyTrait for BorrowedShapeCacheKey<'a> {
    fn key<'k>(&'k self) -> BorrowedShapeCacheKey<'k> {
        *self
    }
}

impl<'a> std::borrow::Borrow<dyn ShapeCacheKeyTrait + 'a> for ShapeCacheKey {
    fn borrow(&self) -> &(dyn ShapeCacheKeyTrait + 'a) {
        self
    }
}

impl<'a> PartialEq for dyn ShapeCacheKeyTrait + 'a {
    fn eq(&self, other: &Self) -> bool {
        self.key().eq(&other.key())
    }
}

impl<'a> Eq for dyn ShapeCacheKeyTrait + 'a {}

impl<'a> std::hash::Hash for dyn ShapeCacheKeyTrait + 'a {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.key().hash(state)
    }
}

/// Which UI surface a piece of proportional text belongs to. Shaping is
/// routed through per-domain caches so a long Note cannot evict tab-bar or
/// sidebar entries (nor the terminal grid's cache), and so Note / File
/// Preview shaping memory can be released independently when hidden.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiTextDomain {
    Chrome,
    Note,
    FilePreview,
}

impl UiTextDomain {
    pub fn miss_stage_name(self) -> &'static str {
        match self {
            Self::Chrome => "chrome_shape_miss",
            Self::Note => "note_shape_miss",
            Self::FilePreview => "file_preview_shape_miss",
        }
    }

    pub fn gauge_names(self) -> &'static UiShapeGaugeNames {
        match self {
            Self::Chrome => &CHROME_GAUGE_NAMES,
            Self::Note => &NOTE_GAUGE_NAMES,
            Self::FilePreview => &FILE_PREVIEW_GAUGE_NAMES,
        }
    }
}

/// Static gauge names per domain (the diagnostics API requires
/// `&'static str`).
pub struct UiShapeGaugeNames {
    pub len: &'static str,
    pub bytes: &'static str,
    pub cap: &'static str,
    pub budget: &'static str,
    pub hits: &'static str,
    pub misses: &'static str,
    pub rejected_oversize: &'static str,
    pub evicted_entries: &'static str,
    pub evicted_bytes: &'static str,
}

static CHROME_GAUGE_NAMES: UiShapeGaugeNames = UiShapeGaugeNames {
    len: "chrome_cache_len",
    bytes: "chrome_cache_bytes",
    cap: "chrome_cache_cap",
    budget: "chrome_cache_budget",
    hits: "chrome_cache_hits",
    misses: "chrome_cache_misses",
    rejected_oversize: "chrome_cache_rejected_oversize",
    evicted_entries: "chrome_cache_evictions_count",
    evicted_bytes: "chrome_cache_evictions_bytes",
};

static NOTE_GAUGE_NAMES: UiShapeGaugeNames = UiShapeGaugeNames {
    len: "note_cache_len",
    bytes: "note_cache_bytes",
    cap: "note_cache_cap",
    budget: "note_cache_budget",
    hits: "note_cache_hits",
    misses: "note_cache_misses",
    rejected_oversize: "note_cache_rejected_oversize",
    evicted_entries: "note_cache_evictions_count",
    evicted_bytes: "note_cache_evictions_bytes",
};

static FILE_PREVIEW_GAUGE_NAMES: UiShapeGaugeNames = UiShapeGaugeNames {
    len: "file_preview_cache_len",
    bytes: "file_preview_cache_bytes",
    cap: "file_preview_cache_cap",
    budget: "file_preview_cache_budget",
    hits: "file_preview_cache_hits",
    misses: "file_preview_cache_misses",
    rejected_oversize: "file_preview_cache_rejected_oversize",
    evicted_entries: "file_preview_cache_evictions_count",
    evicted_bytes: "file_preview_cache_evictions_bytes",
};

/// Cumulative counters maintained locally (plain fields, no global locks on
/// the request path) and published in batch to the diagnostics panel.
#[derive(Debug, Default, Clone, Copy)]
pub struct UiShapeCacheStats {
    pub hits: u64,
    pub misses: u64,
    pub rejected_oversize: u64,
    pub evicted_entries: u64,
    pub evicted_bytes: u64,
}

pub type UiShapedValue = anyhow::Result<Rc<Vec<ShapedInfo>>>;

const SHAPE_ENTRY_FIXED_OVERHEAD: usize = 128;
const SHAPE_ERR_ENTRY_BYTES: usize = 256;

fn estimate_text_style_heap_bytes(style: &TextStyle) -> usize {
    style
        .font
        .capacity()
        .saturating_mul(std::mem::size_of::<config::FontAttributes>())
        .saturating_add(
            style
                .font
                .iter()
                .map(|attributes| attributes.family.capacity())
                .sum(),
        )
}

/// Estimated resident bytes for one cache entry. Glyph bitmaps live in the
/// shared glyph atlas behind `Rc<CachedGlyph>` and are deliberately not
/// counted here.
pub fn estimate_shaped_entry_bytes(key: &ShapeCacheKey, value: &UiShapedValue) -> usize {
    let key_bytes = std::mem::size_of::<ShapeCacheKey>()
        .saturating_add(key.text.capacity())
        .saturating_add(estimate_text_style_heap_bytes(&key.style));
    let value_bytes = match value {
        Ok(shaped) => std::mem::size_of::<Vec<ShapedInfo>>().saturating_add(
            shaped
                .capacity()
                .saturating_mul(std::mem::size_of::<ShapedInfo>()),
        ),
        Err(_) => SHAPE_ERR_ENTRY_BYTES,
    };
    SHAPE_ENTRY_FIXED_OVERHEAD
        .saturating_add(key_bytes)
        .saturating_add(value_bytes)
}

fn chrome_shape_cache_cap(config: &config::ConfigHandle) -> usize {
    config.shape_cache_size
}
fn note_shape_cache_cap(config: &config::ConfigHandle) -> usize {
    config.shape_cache_size.saturating_mul(4)
}
fn file_preview_shape_cache_cap(config: &config::ConfigHandle) -> usize {
    config.shape_cache_size.saturating_mul(2)
}
fn chrome_shape_cache_budget(_: &config::ConfigHandle) -> usize {
    4 * 1024 * 1024
}
fn note_shape_cache_budget(_: &config::ConfigHandle) -> usize {
    32 * 1024 * 1024
}
fn file_preview_shape_cache_budget(_: &config::ConfigHandle) -> usize {
    16 * 1024 * 1024
}

pub struct UiShapeDomainCache {
    cache: lfucache::LfuCache<ShapeCacheKey, UiShapedValue>,
    stats: UiShapeCacheStats,
    cap_func: fn(&config::ConfigHandle) -> usize,
    budget_func: fn(&config::ConfigHandle) -> usize,
    cap: usize,
    byte_budget: usize,
}

impl UiShapeDomainCache {
    fn new(domain: UiTextDomain, config: &config::ConfigHandle) -> Self {
        let (hit, miss, cap_func, budget_func): (
            &'static str,
            &'static str,
            fn(&config::ConfigHandle) -> usize,
            fn(&config::ConfigHandle) -> usize,
        ) = match domain {
            UiTextDomain::Chrome => (
                "chrome_shape_cache.hit.rate",
                "chrome_shape_cache.miss.rate",
                chrome_shape_cache_cap,
                chrome_shape_cache_budget,
            ),
            UiTextDomain::Note => (
                "note_shape_cache.hit.rate",
                "note_shape_cache.miss.rate",
                note_shape_cache_cap,
                note_shape_cache_budget,
            ),
            UiTextDomain::FilePreview => (
                "file_preview_shape_cache.hit.rate",
                "file_preview_shape_cache.miss.rate",
                file_preview_shape_cache_cap,
                file_preview_shape_cache_budget,
            ),
        };
        Self {
            cache: lfucache::LfuCache::new_weighted(hit, miss, cap_func, budget_func, config),
            stats: UiShapeCacheStats::default(),
            cap_func,
            budget_func,
            cap: cap_func(config),
            byte_budget: budget_func(config),
        }
    }

    pub fn get(&mut self, key: &dyn ShapeCacheKeyTrait) -> Option<&UiShapedValue> {
        let value = self.cache.get(key);
        if value.is_some() {
            self.stats.hits += 1;
        } else {
            self.stats.misses += 1;
        }
        value
    }

    pub fn put(&mut self, key: ShapeCacheKey, value: UiShapedValue) {
        let weight = estimate_shaped_entry_bytes(&key, &value);
        let outcome = self.cache.put_weighted(key, value, weight);
        if outcome.rejected_oversize {
            self.stats.rejected_oversize += 1;
        }
        self.stats.evicted_entries += outcome.evicted_entries as u64;
        self.stats.evicted_bytes += outcome.evicted_bytes as u64;
    }

    pub fn clear(&mut self) {
        self.cache.clear();
    }

    /// Evict only the entries whose text contains any of `chars`. Used when
    /// a font fallback resolve lands: entries without the newly resolved
    /// codepoints shaped correctly the first time and stay warm.
    pub fn evict_containing(&mut self, chars: &[char]) {
        self.cache
            .retain(|key, _| !key.text.chars().any(|c| chars.contains(&c)));
    }

    pub fn update_config(&mut self, config: &config::ConfigHandle) {
        self.cache.update_config(config);
        self.cap = (self.cap_func)(config);
        self.byte_budget = (self.budget_func)(config);
    }

    pub fn len(&self) -> usize {
        self.cache.len()
    }

    pub fn total_weight(&self) -> usize {
        self.cache.total_weight()
    }

    pub fn cap(&self) -> usize {
        self.cap
    }

    pub fn byte_budget(&self) -> usize {
        self.byte_budget
    }

    pub fn stats(&self) -> UiShapeCacheStats {
        self.stats
    }
}

/// The three per-domain proportional-text shaping caches; the terminal grid
/// keeps its own separate `shape_cache`.
pub struct UiShapeCaches {
    chrome: UiShapeDomainCache,
    note: UiShapeDomainCache,
    file_preview: UiShapeDomainCache,
}

impl UiShapeCaches {
    pub fn new(config: &config::ConfigHandle) -> Self {
        Self {
            chrome: UiShapeDomainCache::new(UiTextDomain::Chrome, config),
            note: UiShapeDomainCache::new(UiTextDomain::Note, config),
            file_preview: UiShapeDomainCache::new(UiTextDomain::FilePreview, config),
        }
    }

    pub fn domain_mut(&mut self, domain: UiTextDomain) -> &mut UiShapeDomainCache {
        match domain {
            UiTextDomain::Chrome => &mut self.chrome,
            UiTextDomain::Note => &mut self.note,
            UiTextDomain::FilePreview => &mut self.file_preview,
        }
    }

    pub fn domain(&self, domain: UiTextDomain) -> &UiShapeDomainCache {
        match domain {
            UiTextDomain::Chrome => &self.chrome,
            UiTextDomain::Note => &self.note,
            UiTextDomain::FilePreview => &self.file_preview,
        }
    }

    pub fn clear_all(&mut self) {
        self.chrome.clear();
        self.note.clear();
        self.file_preview.clear();
    }

    /// Selective cross-domain eviction for a completed font fallback
    /// resolve: see [`UiShapeDomainCache::evict_containing`].
    pub fn evict_containing(&mut self, chars: &[char]) {
        self.chrome.evict_containing(chars);
        self.note.evict_containing(chars);
        self.file_preview.evict_containing(chars);
    }

    /// Note idle release: only the Note domain is dropped.
    pub fn clear_note(&mut self) {
        self.note.clear();
    }

    /// File Preview idle release: only the preview domain is dropped.
    pub fn clear_file_preview(&mut self) {
        self.file_preview.clear();
    }

    pub fn update_config(&mut self, config: &config::ConfigHandle) {
        self.chrome.update_config(config);
        self.note.update_config(config);
        self.file_preview.update_config(config);
    }
}

#[cfg(test)]
mod ui_shape_cache_tests {
    use super::*;

    fn key(text: &str) -> ShapeCacheKey {
        ShapeCacheKey {
            font_identity: 1,
            style: TextStyle::default(),
            text: text.to_string(),
        }
    }

    fn caches() -> UiShapeCaches {
        UiShapeCaches::new(&config::configuration())
    }

    #[test]
    fn domains_are_isolated_and_release_independently() {
        let mut caches = caches();
        caches
            .domain_mut(UiTextDomain::Chrome)
            .put(key("chrome"), Ok(Rc::new(vec![])));
        caches
            .domain_mut(UiTextDomain::Note)
            .put(key("note"), Ok(Rc::new(vec![])));
        caches
            .domain_mut(UiTextDomain::FilePreview)
            .put(key("preview"), Ok(Rc::new(vec![])));

        caches.clear_note();
        assert_eq!(caches.domain(UiTextDomain::Note).len(), 0);
        assert_eq!(caches.domain(UiTextDomain::Note).total_weight(), 0);
        assert_eq!(caches.domain(UiTextDomain::Chrome).len(), 1);
        assert_eq!(caches.domain(UiTextDomain::FilePreview).len(), 1);

        caches.clear_file_preview();
        assert_eq!(caches.domain(UiTextDomain::FilePreview).len(), 0);
        assert_eq!(caches.domain(UiTextDomain::FilePreview).total_weight(), 0);
        assert_eq!(caches.domain(UiTextDomain::Chrome).len(), 1);

        caches.clear_all();
        assert_eq!(caches.domain(UiTextDomain::Chrome).len(), 0);
        assert_eq!(caches.domain(UiTextDomain::Chrome).total_weight(), 0);
    }

    #[test]
    fn oversize_entries_are_rejected_and_counted() {
        let mut caches = caches();
        let note = caches.domain_mut(UiTextDomain::Note);
        // Exceeds the 32 MiB note budget on its own.
        let huge = "x".repeat(40 * 1024 * 1024);
        note.put(key(&huge), Ok(Rc::new(vec![])));
        assert_eq!(note.len(), 0);
        assert_eq!(note.total_weight(), 0);
        assert_eq!(note.stats().rejected_oversize, 1);
    }

    #[test]
    fn hit_and_miss_counters_track_lookups() {
        let mut caches = caches();
        let chrome = caches.domain_mut(UiTextDomain::Chrome);
        let probe = key("label");
        assert!(chrome.get(&probe as &dyn ShapeCacheKeyTrait).is_none());
        chrome.put(key("label"), Ok(Rc::new(vec![])));
        assert!(chrome.get(&probe as &dyn ShapeCacheKeyTrait).is_some());
        let stats = chrome.stats();
        assert_eq!(stats.misses, 1);
        assert_eq!(stats.hits, 1);
    }
}

#[cfg(test)]
mod test {
    use crate::glyphcache::CachedGlyph;
    use crate::glyphcache::GlyphCache;
    use crate::shapecache::{GlyphPosition, ShapedInfo};
    use crate::utilsprites::RenderMetrics;
    use config::{FontAttributes, TextStyle};
    use std::rc::Rc;
    use termwiz::cell::CellAttributes;
    use termwiz::surface::{Line, SEQ_ZERO};
    use wezterm_bidi::Direction;
    use wezterm_font::shaper::{GlyphInfo, GlyphInfoParts, PresentationWidth};
    use wezterm_font::units::PixelLength;
    use wezterm_font::{FontConfiguration, LoadedFont};

    fn cluster_and_shape(
        render_metrics: &RenderMetrics,
        glyph_cache: &mut GlyphCache,
        style: &TextStyle,
        font: &Rc<LoadedFont>,
        text: &str,
    ) -> Vec<GlyphPosition> {
        let line = Line::from_text(text, &CellAttributes::default(), SEQ_ZERO, None);
        eprintln!("{:?}", line);
        let mut all_infos = vec![];
        let mut all_glyphs = vec![];

        for cluster in line.cluster(None) {
            let presentation_width = PresentationWidth::with_cluster(&cluster);
            let mut infos = font
                .shape(
                    &cluster.text,
                    |_: &[char]| {},
                    |_| {},
                    None,
                    Direction::LeftToRight,
                    None,
                    Some(&presentation_width),
                )
                .unwrap();
            let mut glyphs = infos
                .iter()
                .map(|info| {
                    let cell_idx = cluster.byte_to_cell_idx(info.cluster as usize);
                    let num_cells = cluster.byte_to_cell_width(info.cluster as usize);

                    let followed_by_space = match line.get_cell(cell_idx + 1) {
                        Some(cell) => cell.str() == " ",
                        None => false,
                    };

                    glyph_cache
                        .cached_glyph(
                            info,
                            &style,
                            followed_by_space,
                            font,
                            render_metrics,
                            num_cells,
                        )
                        .unwrap()
                })
                .collect::<Vec<_>>();

            all_infos.append(&mut infos);
            all_glyphs.append(&mut glyphs);
        }

        eprintln!("infos: {:#?}", all_infos);
        eprintln!("glyphs: {:#?}", all_glyphs);
        ShapedInfo::process(&all_infos, &all_glyphs)
            .into_iter()
            .map(|p| p.pos)
            .collect()
    }

    #[test]
    fn shaped_info_preserves_clusters() {
        let glyph = Rc::new(CachedGlyph {
            has_color: false,
            brightness_adjust: 1.0,
            x_offset: PixelLength::new(0.0),
            y_offset: PixelLength::new(0.0),
            x_advance: PixelLength::new(1.0),
            bearing_x: PixelLength::new(0.0),
            bearing_y: PixelLength::new(0.0),
            texture: None,
            scale: 1.0,
        });
        let infos = vec![
            GlyphInfo::new(
                "a",
                GlyphInfoParts {
                    only_char: Some('a'),
                    is_space: false,
                    num_cells: 1,
                    cluster: 0,
                    font_idx: 0,
                    glyph_pos: 11,
                    x_advance: PixelLength::new(1.0),
                    y_advance: PixelLength::new(0.0),
                    x_offset: PixelLength::new(0.0),
                    y_offset: PixelLength::new(0.0),
                },
            ),
            GlyphInfo::new(
                "你",
                GlyphInfoParts {
                    only_char: Some('你'),
                    is_space: false,
                    num_cells: 2,
                    cluster: 3,
                    font_idx: 0,
                    glyph_pos: 12,
                    x_advance: PixelLength::new(2.0),
                    y_advance: PixelLength::new(0.0),
                    x_offset: PixelLength::new(0.0),
                    y_offset: PixelLength::new(0.0),
                },
            ),
        ];
        let shaped = ShapedInfo::process(&infos, &[Rc::clone(&glyph), glyph]);

        assert_eq!(
            shaped.iter().map(|info| info.cluster).collect::<Vec<_>>(),
            vec![0, 3]
        );
    }

    #[test]
    fn ligatures_fira() {
        config::use_test_configuration();
        let _ = env_logger::Builder::new()
            .is_test(true)
            .filter_level(log::LevelFilter::Trace)
            .try_init();

        let config = config::configuration();

        let mut config: config::Config = (*config).clone();
        config.font = TextStyle {
            font: vec![FontAttributes::new("Fira Code")],
            foreground: None,
        };
        config.font_rules.clear();
        config.compute_extra_defaults(None);
        config::use_this_configuration(config.clone());

        let fonts = Rc::new(
            FontConfiguration::new(
                None,
                config.dpi.unwrap_or_else(|| ::window::default_dpi()) as usize,
            )
            .unwrap(),
        );
        let render_metrics = RenderMetrics::new(&fonts).unwrap();
        let mut glyph_cache = GlyphCache::new_in_memory(&fonts, 128).unwrap();

        let style = TextStyle::default();
        let font = fonts.resolve_font(&style).unwrap();

        k9::snapshot!(
            cluster_and_shape(&render_metrics, &mut glyph_cache, &style, &font, "a..."),
            "
[
    GlyphPosition {
        glyph_idx: 189,
        num_cells: 1,
        x_offset: 0.0,
        bearing_x: 1.0,
        bitmap_pixel_width: 8,
    },
    GlyphPosition {
        glyph_idx: 1742,
        num_cells: 1,
        x_offset: 0.0,
        bearing_x: 0.0,
        bitmap_pixel_width: 0,
    },
    GlyphPosition {
        glyph_idx: 1742,
        num_cells: 1,
        x_offset: 0.0,
        bearing_x: 0.0,
        bitmap_pixel_width: 0,
    },
    GlyphPosition {
        glyph_idx: 896,
        num_cells: 1,
        x_offset: 0.0,
        bearing_x: -15.0,
        bitmap_pixel_width: 20,
    },
]
"
        );
    }

    #[test]
    fn bench_shaping() {
        config::use_test_configuration();

        // let mut glyph_cache = GlyphCache::new_in_memory(&fonts, 128, &render_metrics).unwrap();
        // let render_metrics = RenderMetrics::new(&fonts).unwrap();

        benchmarking::warm_up();

        for &n in &[100, 1000, 10_000] {
            let bench_result = benchmarking::measure_function(move |measurer| {
                let text: String = (0..n).map(|_| ' ').collect();

                let fonts = Rc::new(
                    FontConfiguration::new(
                        None,
                        config::configuration()
                            .dpi
                            .unwrap_or_else(|| ::window::default_dpi())
                            as usize,
                    )
                    .unwrap(),
                );
                let style = TextStyle::default();
                let font = fonts.resolve_font(&style).unwrap();
                let line = Line::from_text(&text, &CellAttributes::default(), SEQ_ZERO, None);
                let cell_clusters = line.cluster(None);
                let cluster = &cell_clusters[0];
                let presentation_width = PresentationWidth::with_cluster(&cluster);

                measurer.measure(|| {
                    let _x = font
                        .shape(
                            &cluster.text,
                            |_: &[char]| {},
                            |_| {},
                            None,
                            Direction::LeftToRight,
                            None,
                            Some(&presentation_width),
                        )
                        .unwrap();
                    // println!("{:?}", &x[0..2]);
                });
            })
            .unwrap();
            println!("{}: {:?}", n, bench_result.elapsed());
        }
    }

    #[test]
    fn ligatures_jetbrains() {
        config::use_test_configuration();
        let _ = env_logger::Builder::new()
            .is_test(true)
            .filter_level(log::LevelFilter::Trace)
            .try_init();
        let config = config::configuration();

        let fonts = Rc::new(
            FontConfiguration::new(
                None,
                config.dpi.unwrap_or_else(|| ::window::default_dpi()) as usize,
            )
            .unwrap(),
        );
        let render_metrics = RenderMetrics::new(&fonts).unwrap();
        let mut glyph_cache = GlyphCache::new_in_memory(&fonts, 128).unwrap();

        let style = TextStyle::default();
        let font = fonts.resolve_font(&style).unwrap();

        k9::snapshot!(
            cluster_and_shape(&render_metrics, &mut glyph_cache, &style, &font, "ab"),
            "
[
    GlyphPosition {
        glyph_idx: 189,
        num_cells: 1,
        x_offset: 0.0,
        bearing_x: 1.0,
        bitmap_pixel_width: 8,
    },
    GlyphPosition {
        glyph_idx: 214,
        num_cells: 1,
        x_offset: 0.0,
        bearing_x: 1.0,
        bitmap_pixel_width: 8,
    },
]
"
        );

        k9::snapshot!(
            cluster_and_shape(&render_metrics, &mut glyph_cache, &style, &font, "a b"),
            "
[
    GlyphPosition {
        glyph_idx: 189,
        num_cells: 1,
        x_offset: 0.0,
        bearing_x: 1.0,
        bitmap_pixel_width: 8,
    },
    GlyphPosition {
        glyph_idx: 958,
        num_cells: 1,
        x_offset: 0.0,
        bearing_x: 0.0,
        bitmap_pixel_width: 0,
    },
    GlyphPosition {
        glyph_idx: 214,
        num_cells: 1,
        x_offset: 0.0,
        bearing_x: 1.0,
        bitmap_pixel_width: 8,
    },
]
"
        );

        k9::snapshot!(
            cluster_and_shape(&render_metrics, &mut glyph_cache, &style, &font, "a..."),
            "
[
    GlyphPosition {
        glyph_idx: 189,
        num_cells: 1,
        x_offset: 0.0,
        bearing_x: 1.0,
        bitmap_pixel_width: 8,
    },
    GlyphPosition {
        glyph_idx: 1742,
        num_cells: 1,
        x_offset: 0.0,
        bearing_x: 0.0,
        bitmap_pixel_width: 0,
    },
    GlyphPosition {
        glyph_idx: 1742,
        num_cells: 1,
        x_offset: 0.0,
        bearing_x: 0.0,
        bitmap_pixel_width: 0,
    },
    GlyphPosition {
        glyph_idx: 896,
        num_cells: 1,
        x_offset: 0.0,
        bearing_x: -15.0,
        bitmap_pixel_width: 20,
    },
]
"
        );

        k9::snapshot!(
            cluster_and_shape(&render_metrics, &mut glyph_cache, &style, &font, "e_or_"),
            "
[
    GlyphPosition {
        glyph_idx: 225,
        num_cells: 1,
        x_offset: 0.0,
        bearing_x: 1.0,
        bitmap_pixel_width: 8,
    },
    GlyphPosition {
        glyph_idx: 860,
        num_cells: 1,
        x_offset: 0.0,
        bearing_x: 0.0,
        bitmap_pixel_width: 9,
    },
    GlyphPosition {
        glyph_idx: 290,
        num_cells: 1,
        x_offset: 0.0,
        bearing_x: 1.0,
        bitmap_pixel_width: 8,
    },
    GlyphPosition {
        glyph_idx: 320,
        num_cells: 1,
        x_offset: 0.0,
        bearing_x: 1.0,
        bitmap_pixel_width: 8,
    },
    GlyphPosition {
        glyph_idx: 860,
        num_cells: 1,
        x_offset: 0.0,
        bearing_x: 0.0,
        bitmap_pixel_width: 9,
    },
]
"
        );

        k9::snapshot!(
            cluster_and_shape(&render_metrics, &mut glyph_cache, &style, &font, "a  b"),
            "
[
    GlyphPosition {
        glyph_idx: 189,
        num_cells: 1,
        x_offset: 0.0,
        bearing_x: 1.0,
        bitmap_pixel_width: 8,
    },
    GlyphPosition {
        glyph_idx: 958,
        num_cells: 1,
        x_offset: 0.0,
        bearing_x: 0.0,
        bitmap_pixel_width: 0,
    },
    GlyphPosition {
        glyph_idx: 958,
        num_cells: 1,
        x_offset: 0.0,
        bearing_x: 0.0,
        bitmap_pixel_width: 0,
    },
    GlyphPosition {
        glyph_idx: 214,
        num_cells: 1,
        x_offset: 0.0,
        bearing_x: 1.0,
        bitmap_pixel_width: 8,
    },
]
"
        );

        k9::snapshot!(
            cluster_and_shape(&render_metrics, &mut glyph_cache, &style, &font, "<-"),
            "
[
    GlyphPosition {
        glyph_idx: 1742,
        num_cells: 1,
        x_offset: 0.0,
        bearing_x: 0.0,
        bitmap_pixel_width: 0,
    },
    GlyphPosition {
        glyph_idx: 1588,
        num_cells: 1,
        x_offset: 0.0,
        bearing_x: -9.0,
        bitmap_pixel_width: 17,
    },
]
"
        );

        k9::snapshot!(
            cluster_and_shape(&render_metrics, &mut glyph_cache, &style, &font, "<>"),
            "
[
    GlyphPosition {
        glyph_idx: 1742,
        num_cells: 1,
        x_offset: 0.0,
        bearing_x: 0.0,
        bitmap_pixel_width: 0,
    },
    GlyphPosition {
        glyph_idx: 1613,
        num_cells: 1,
        x_offset: 0.0,
        bearing_x: -8.0,
        bitmap_pixel_width: 16,
    },
]
"
        );

        k9::snapshot!(
            cluster_and_shape(&render_metrics, &mut glyph_cache, &style, &font, "|=>"),
            "
[
    GlyphPosition {
        glyph_idx: 1742,
        num_cells: 1,
        x_offset: 0.0,
        bearing_x: 0.0,
        bitmap_pixel_width: 0,
    },
    GlyphPosition {
        glyph_idx: 1742,
        num_cells: 1,
        x_offset: 0.0,
        bearing_x: 0.0,
        bitmap_pixel_width: 0,
    },
    GlyphPosition {
        glyph_idx: 1562,
        num_cells: 1,
        x_offset: 0.0,
        bearing_x: -18.0,
        bitmap_pixel_width: 27,
    },
]
"
        );

        let block_bottom_one_eighth = "\u{2581}";
        k9::snapshot!(
            cluster_and_shape(
                &render_metrics,
                &mut glyph_cache,
                &style,
                &font,
                block_bottom_one_eighth
            ),
            "
[
    GlyphPosition {
        glyph_idx: 1178,
        num_cells: 1,
        x_offset: 0.0,
        bearing_x: 0.0,
        bitmap_pixel_width: 10,
    },
]
"
        );

        let powerline_extra_honeycomb = "\u{e0cc}";
        k9::snapshot!(
            cluster_and_shape(
                &render_metrics,
                &mut glyph_cache,
                &style,
                &font,
                powerline_extra_honeycomb,
            ),
            "
[
    GlyphPosition {
        glyph_idx: 58,
        num_cells: 1,
        x_offset: 0.0,
        bearing_x: -0.8333333,
        bitmap_pixel_width: 12,
    },
]
"
        );

        k9::snapshot!(
            cluster_and_shape(&render_metrics, &mut glyph_cache, &style, &font, "<!--"),
            "
[
    GlyphPosition {
        glyph_idx: 1742,
        num_cells: 1,
        x_offset: 0.0,
        bearing_x: 0.0,
        bitmap_pixel_width: 0,
    },
    GlyphPosition {
        glyph_idx: 1742,
        num_cells: 1,
        x_offset: 0.0,
        bearing_x: 0.0,
        bitmap_pixel_width: 0,
    },
    GlyphPosition {
        glyph_idx: 1742,
        num_cells: 1,
        x_offset: 0.0,
        bearing_x: 0.0,
        bitmap_pixel_width: 0,
    },
    GlyphPosition {
        glyph_idx: 1595,
        num_cells: 1,
        x_offset: 0.0,
        bearing_x: -28.0,
        bitmap_pixel_width: 37,
    },
]
"
        );

        let deaf_man_medium_light_skin_tone = "\u{1F9CF}\u{1F3FC}\u{200D}\u{2642}\u{FE0F}";
        println!(
            "deaf_man_medium_light_skin_tone: {}",
            deaf_man_medium_light_skin_tone
        );
        k9::snapshot!(
            cluster_and_shape(
                &render_metrics,
                &mut glyph_cache,
                &style,
                &font,
                deaf_man_medium_light_skin_tone
            ),
            "
[
    GlyphPosition {
        glyph_idx: 2712,
        num_cells: 2,
        x_offset: 0.0,
        bearing_x: 0.0,
        bitmap_pixel_width: 16,
    },
]
"
        );

        let england_flag = "\u{1F3F4}\u{E0067}\u{E0062}\u{E0065}\u{E006E}\u{E0067}\u{E007F}";
        println!("england_flag: {}", england_flag);
        k9::snapshot!(
            cluster_and_shape(
                &render_metrics,
                &mut glyph_cache,
                &style,
                &font,
                england_flag
            ),
            "
[
    GlyphPosition {
        glyph_idx: 3855,
        num_cells: 2,
        x_offset: 0.0,
        bearing_x: 0.0,
        bitmap_pixel_width: 20,
    },
]
"
        );
    }
}
