//! Mermaid diagrams in Markdown: a fenced block whose language is `mermaid`
//! is drawn as its diagram rather than as code. merman parses Mermaid and
//! lays it out in Rust, down to an SVG; resvg rasterizes that.
//!
//! Drawing runs off the UI thread and one diagram at a time, so a note full
//! of diagrams costs one diagram's working memory at once. A diagram is drawn
//! at the size it is shown, not larger, since its pixels end up in the shared
//! glyph atlas, which does not shrink. What is kept is a PNG, which the note's
//! image cache holds and counts like any other image; the SVG and its parsed
//! tree are dropped as soon as the pixels exist.
//!
//! A diagram is the note's text, so it is treated as untrusted: pictures it
//! names are never loaded, one too big to lay out quickly is not drawn, and a
//! panic inside merman makes a block that cannot be read rather than a lost
//! window.

use anyhow::{anyhow, Context, Result};
use merman::render::{
    HeadlessRenderer, HostThemeAppearance, HostThemeOutput, HostThemeProfile, HostThemeRoles,
};
use merman_render::text::{
    DeterministicTextMeasurer, TextMeasurer, TextMetrics, TextStyle, WrapMode,
};
use resvg::{tiny_skia, usvg};
use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::num::NonZeroUsize;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, TryLockError};

/// The longest side a diagram is rasterized with, in pixels, whatever room
/// it is shown in: half the glyph atlas's usual 2048, which text shares.
const MAX_RASTER_SIDE: f32 = 1024.0;
/// The most pixels one diagram is rasterized with (2.4 MB once decoded), so a
/// few on screen at once still leave the glyph atlas room for text.
const MAX_RASTER_PIXELS: f32 = 600_000.0;
/// The longest Mermaid block drawn; a longer one stays code. It is parsed on
/// the UI thread when a note is laid out, which at this length takes up to
/// about 8 ms.
const MAX_SOURCE_BYTES: usize = 16 * 1024;
/// The most links one diagram is drawn with. Laying a graph out costs more
/// the more its links cross: 150 among 50 boxes take about 0.4 s, 300 among
/// 100 over 5 s, and nothing can stop merman once it has started.
const MAX_LINKS: usize = 150;
/// How many label advances the fallback measurer remembers before it starts
/// over.
const MAX_REMEMBERED_ADVANCES: usize = 4096;
/// The largest font file the fallback measurer keeps in memory; a larger one
/// is read again for each character it has not measured yet.
const MAX_KEPT_FONT_BYTES: usize = 2 * 1024 * 1024;
/// The size Mermaid sets labels in, in points; a diagram is scaled from it to
/// the size of the text around it.
pub(crate) const LABEL_SIZE: f32 = 16.0;
/// How many verdicts are remembered.
const VERDICT_CAPACITY: usize = 256;

/// Mermaid's own font stack, whose metrics merman ships, spelled as the fonts
/// name themselves: faces are found by exact name.
const MERMAID_FONT_FAMILY: &str = "\"Trebuchet MS\", Verdana, Arial, sans-serif";
/// Where Trebuchet MS is missing (most Linux systems), the first of these
/// that is installed draws the labels, and is also what they are measured
/// with, so boxes fit what is drawn.
const FALLBACK_FAMILIES: &[&str] = &[
    "Liberation Sans",
    "Arimo",
    "DejaVu Sans",
    "Noto Sans",
    "Ubuntu",
    "Cantarell",
    "FreeSans",
];

pub(crate) fn is_mermaid(language: Option<&str>) -> bool {
    language.is_some_and(|language| language.eq_ignore_ascii_case("mermaid"))
}

pub(crate) fn content_hash(text: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

/// What is known of a Mermaid block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    Draws,
    /// merman cannot read it, or cannot draw what it read.
    Unreadable,
    /// Too long, or with too many links, to lay out quickly.
    TooLarge,
}

static VERDICTS: Mutex<Option<lru::LruCache<u64, Verdict>>> = Mutex::new(None);
/// Moves on each time a diagram that parsed turns out not to draw.
static VERDICT_EPOCH: AtomicU64 = AtomicU64::new(0);

fn remembered_verdict(key: u64) -> Option<Verdict> {
    VERDICTS
        .lock()
        .unwrap_or_else(|err| err.into_inner())
        .as_mut()
        .and_then(|verdicts| verdicts.get(&key).copied())
}

fn remember_verdict(key: u64, verdict: Verdict) {
    VERDICTS
        .lock()
        .unwrap_or_else(|err| err.into_inner())
        .get_or_insert_with(|| {
            lru::LruCache::new(NonZeroUsize::new(VERDICT_CAPACITY).expect("not zero"))
        })
        .put(key, verdict);
}

/// How many links a parsed diagram has: its longest list of edges, relations
/// or relationships, whichever the diagram keeps them in.
fn links(model: &serde_json::Value) -> usize {
    ["edges", "relations", "relationships"]
        .iter()
        .filter_map(|key| model.get(key)?.as_array().map(Vec::len))
        .max()
        .unwrap_or(0)
}

/// What merman makes of `source`, without laying it out.
fn judge(source: &str, engine: &merman::Engine) -> Verdict {
    if source.len() > MAX_SOURCE_BYTES {
        return Verdict::TooLarge;
    }
    guarded(source, || {
        match engine.parse_diagram_sync(source, merman::ParseOptions::default()) {
            Ok(Some(parsed)) if links(&parsed.model) > MAX_LINKS => Verdict::TooLarge,
            Ok(Some(_)) => Verdict::Draws,
            _ => Verdict::Unreadable,
        }
    })
    .unwrap_or(Verdict::Unreadable)
}

/// Whether `source` is a Mermaid diagram to draw. Asked each time a note's
/// layout is rebuilt, so verdicts are remembered by content; parsing alone is
/// cheap next to drawing.
pub(crate) fn draws(source: &str) -> bool {
    if source.len() > MAX_SOURCE_BYTES {
        return false;
    }
    let key = content_hash(source);
    let verdict = remembered_verdict(key).unwrap_or_else(|| {
        let verdict = judge(source, &merman::Engine::new());
        remember_verdict(key, verdict);
        verdict
    });
    verdict == Verdict::Draws
}

/// What stops `source` from drawing, if that is already known. Never parses,
/// so it costs nothing to ask on every edit.
pub(crate) fn known_problem(source: &str) -> Option<Verdict> {
    if source.len() > MAX_SOURCE_BYTES {
        return Some(Verdict::TooLarge);
    }
    remembered_verdict(content_hash(source)).filter(|verdict| *verdict != Verdict::Draws)
}

/// `f`, run on a diagram's `source`, or `None` if merman panicked. Some text
/// does make it panic (an ER diagram with CJK attribute types, in 0.7), and
/// the message would quote the note, so the log only learns that it happened.
fn guarded<R>(source: &str, f: impl FnOnce() -> R) -> Option<R> {
    let result = env_bootstrap::with_quiet_panics(|| std::panic::catch_unwind(AssertUnwindSafe(f)));
    if result.is_err() {
        log::warn!(
            "merman panicked on a Mermaid diagram ({} bytes)",
            source.len()
        );
    }
    result.ok()
}

/// Where the verdicts stand; a note's layout is rebuilt when this moves.
pub(crate) fn verdict_epoch() -> u64 {
    VERDICT_EPOCH.load(Ordering::Relaxed)
}

/// Let go of the font index, the one large thing drawing keeps between
/// diagrams, when a note's memory is released; the next diagram finds it
/// again. Verdicts stay: they are few and small, and one saying a diagram
/// does not draw is what keeps it shown as code in every window. Fonts being
/// found for a diagram right now are left to the next release, not waited on.
pub(crate) fn release() {
    match FONTS.try_lock() {
        Ok(mut fonts) => {
            fonts.take();
        }
        Err(TryLockError::Poisoned(fonts)) => {
            fonts.into_inner().take();
        }
        Err(TryLockError::WouldBlock) => {}
    }
}

/// The fonts diagrams are drawn with. Only the faces' names and locations are
/// indexed; font files are mapped, and read as glyphs are used.
struct Fonts {
    db: Arc<usvg::fontdb::Database>,
    /// The family labels are drawn in, as the installed fonts spell it.
    family: String,
    /// `None` when Mermaid's own font is installed and merman's metrics for it
    /// apply; otherwise the family drawn with and a measurer for it.
    fallback: Option<(String, Arc<FontMeasurer>)>,
}

static FONTS: Mutex<Option<Arc<Fonts>>> = Mutex::new(None);

/// The fonts, found on first use and kept until `release`.
fn fonts() -> Arc<Fonts> {
    let mut fonts = FONTS.lock().unwrap_or_else(|err| err.into_inner());
    Arc::clone(fonts.get_or_insert_with(|| Arc::new(Fonts::load())))
}

impl Fonts {
    fn load() -> Self {
        let mut db = usvg::fontdb::Database::new();
        db.load_system_fonts();
        // A family as the installed fonts spell it, since that is how usvg
        // looks faces up.
        let installed = |family: &str| {
            db.faces().find_map(|face| {
                face.families
                    .iter()
                    .find(|(name, _)| name.eq_ignore_ascii_case(family))
                    .map(|(name, _)| name.clone())
            })
        };
        let trebuchet = installed("Trebuchet MS");
        let family = match &trebuchet {
            Some(trebuchet) => trebuchet.clone(),
            None => FALLBACK_FAMILIES
                .iter()
                .find_map(|family| installed(family))
                .or_else(|| {
                    db.faces()
                        .find_map(|face| face.families.first().map(|(name, _)| name.clone()))
                })
                .unwrap_or_else(|| "sans-serif".to_string()),
        };
        // What the diagram's font stack ends in, too.
        db.set_sans_serif_family(family.clone());
        let db = Arc::new(db);
        let fallback = trebuchet.is_none().then(|| {
            let measurer = Arc::new(FontMeasurer::new(&db, &family));
            (family.clone(), measurer)
        });
        Fonts {
            db,
            family,
            fallback,
        }
    }
}

/// Which look a diagram is drawn in; part of what its cached image is keyed
/// by.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct DiagramStyle {
    pub dark: bool,
}

fn theme(style: DiagramStyle, font_family: &str) -> HostThemeProfile {
    // A clear canvas: the diagram is drawn on whatever the note shows (left
    // unset, Mermaid's default paints the root white).
    let canvas = Some("transparent".to_string());
    let text = if style.dark { "#e3e5e8" } else { "#1f2328" };
    let cluster = if style.dark { "#2f3238" } else { "#eef0f3" };
    let surface = if style.dark { "#2b2d31" } else { "#f3f4f6" };
    let roles = if style.dark {
        HostThemeRoles {
            canvas,
            surface: Some(surface.into()),
            surface_alt: Some("#33363d".into()),
            text: Some(text.into()),
            subtle_text: Some("#a6abb3".into()),
            border: Some("#5c616b".into()),
            line: Some("#9aa0a8".into()),
            edge_label_background: Some("#26282c".into()),
            cluster_background: Some(cluster.into()),
            cluster_border: Some("#565b64".into()),
            note_background: Some("#3a3325".into()),
            note_border: Some("#b8954f".into()),
            note_text: Some("#f0e4c8".into()),
            ..HostThemeRoles::default()
        }
    } else {
        HostThemeRoles {
            canvas,
            surface: Some(surface.into()),
            surface_alt: Some("#e8eaee".into()),
            text: Some(text.into()),
            subtle_text: Some("#57606a".into()),
            border: Some("#9ca3af".into()),
            line: Some("#4b5563".into()),
            edge_label_background: Some("#ffffff".into()),
            cluster_background: Some(cluster.into()),
            cluster_border: Some("#b6bcc6".into()),
            note_background: Some("#fff6d5".into()),
            note_border: Some("#d4b35a".into()),
            note_text: Some("#3b2f0b".into()),
            ..HostThemeRoles::default()
        }
    };
    // Series colours carry white labels in pies, timelines and kanban
    // columns, so both looks use ones dark enough for that; twelve, as many as
    // Mermaid has, so no column falls back to its grey.
    HostThemeProfile::builder()
        .appearance(if style.dark {
            HostThemeAppearance::Dark
        } else {
            HostThemeAppearance::Light
        })
        .font_family(font_family)
        .roles(roles)
        .series_palette([
            "#3b82f6", "#10b981", "#d97706", "#db2777", "#7c3aed", "#0891b2", "#4f46e5", "#059669",
            "#b45309", "#be185d", "#6d28d9", "#0e7490",
        ])
        // Titles and Venn labels read these, not the text role, and are
        // otherwise near-black even in the dark look.
        .theme_variable("titleColor", text)
        .theme_variable("vennTitleTextColor", text)
        .theme_variable("vennSetTextColor", text)
        // A state holding others is drawn as a group is, not in Mermaid's
        // pale default, and named in the text colour, not near-black.
        .theme_variable("compositeBackground", cluster)
        .theme_variable("altBackground", cluster)
        .theme_variable("compositeTitleBackground", surface)
        .theme_variable("stateLabelColor", text)
        .site_config("themeCSS", fixed_text_css(style, text))
        .site_config("c4", c4_colors())
        .output(HostThemeOutput::resvg_safe_editor())
        .build()
}

/// Rules for text merman colours in ways no theme variable reaches.
/// Event-modeling lane names are styled with `color`, which SVG text does not
/// use, so without a fill they are black. C4 boundary names and relation
/// labels are fixed at #444444, unreadable on the dark look's ground.
fn fixed_text_css(style: DiagramStyle, text: &str) -> String {
    let mut css = format!(".em-swimlane text{{fill:{text};}}");
    if style.dark {
        css.push_str(&format!("text[fill=\"#444444\"]{{fill:{text};}}"));
    }
    css
}

/// C4 shapes' colours, in both looks. merman always letters them in white,
/// so they keep dark fills: shades of blue, grey for what is external. Left
/// to the host roles they would take the light look's pale surface, and their
/// text would vanish.
fn c4_colors() -> serde_json::Value {
    let mut colors = serde_json::Map::new();
    for (kind, fill) in [
        ("person", "#0b3d91"),
        ("system", "#1d4ed8"),
        ("container", "#2563eb"),
        ("component", "#2f6fdb"),
    ] {
        for shape in ["", "_db", "_queue"] {
            if kind == "person" && !shape.is_empty() {
                continue;
            }
            for (prefix, fill) in [
                (format!("{kind}{shape}"), fill),
                (format!("external_{kind}{shape}"), "#5b6470"),
            ] {
                colors.insert(format!("{prefix}_bg_color"), fill.into());
                colors.insert(format!("{prefix}_border_color"), fill.into());
            }
        }
    }
    serde_json::Value::Object(colors)
}

/// The room a diagram is shown in, in pixels, and the pixels one of its
/// points takes, which sets how big its labels are.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct DiagramBox {
    pub width: f32,
    pub height: f32,
    pub scale: f32,
}

/// A drawn diagram, PNG-encoded, and its size in pixels. `natural` is its own
/// size in points, which gives its size in any other box without drawing it.
pub(crate) struct RenderedDiagram {
    pub png: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub natural: (f32, f32),
}

/// The scale a diagram of `natural` size is drawn at in `room`: the room's,
/// lowered until the diagram fits the room and the limits above. Never
/// raised: a small diagram stays its own size.
fn raster_scale(natural: (f32, f32), room: DiagramBox) -> f32 {
    let (width, height) = natural;
    let mut scale = room.scale.max(0.1);
    if width > 0.0 {
        scale = scale.min(room.width.clamp(1.0, MAX_RASTER_SIDE) / width);
    }
    if height > 0.0 {
        scale = scale.min(room.height.clamp(1.0, MAX_RASTER_SIDE) / height);
    }
    let area = width * height;
    if area > 0.0 {
        scale = scale.min((MAX_RASTER_PIXELS / area).sqrt());
    }
    scale
}

/// The pixels a diagram of `natural` size is drawn with in `room`.
pub(crate) fn raster_size(natural: (f32, f32), room: DiagramBox) -> (u32, u32) {
    let scale = raster_scale(natural, room);
    (
        (natural.0 * scale).round().max(1.0) as u32,
        (natural.1 * scale).round().max(1.0) as u32,
    )
}

/// Whether a diagram drawn `drawn` pixels wide is drawn again to be `wanted`
/// wide. Small differences are left to scaling, so dragging a pane edge
/// redraws it now and then rather than on every step.
pub(crate) fn needs_redraw(drawn: u32, wanted: u32) -> bool {
    let (drawn, wanted) = (drawn as f32, wanted as f32);
    (drawn - wanted).abs() > (wanted / 16.0).max(8.0)
}

/// `source` as SVG, themed and measured for the fonts it will be drawn with.
fn render_svg(source: &str, style: DiagramStyle, fonts: &Fonts) -> Result<String> {
    let family = fonts
        .fallback
        .as_ref()
        .map(|(family, _)| format!("\"{family}\", sans-serif"))
        .unwrap_or_else(|| MERMAID_FONT_FAMILY.to_string());
    let renderer = HeadlessRenderer::new()
        .with_host_theme(&theme(style, &family))
        .with_diagram_id("note-diagram");
    let renderer = match &fonts.fallback {
        Some((_, measurer)) => renderer.with_text_measurer(measurer.clone()),
        None => renderer.with_vendored_text_measurer(),
    };
    renderer
        .render_svg_sync(source)
        .map_err(|err| anyhow!("{err}"))?
        .ok_or_else(|| anyhow!("no Mermaid diagram found"))
}

/// Draw `source` to fit `room`. Blocking and CPU-bound: call it off the UI
/// thread. Diagrams are drawn one at a time. One that does not draw is
/// remembered so, and shows as code from then on.
pub(crate) fn render(
    source: &str,
    style: DiagramStyle,
    room: DiagramBox,
) -> Result<RenderedDiagram> {
    static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());
    let _turn = ONE_AT_A_TIME.lock().unwrap_or_else(|err| err.into_inner());

    // Judged again rather than trusted: the verdict it was asked for on may
    // have been forgotten since.
    let judged = judge(source, &merman::Engine::new());
    let drawn = match judged {
        Verdict::Draws => guarded(source, || draw(source, style, room))
            .unwrap_or_else(|| Err(anyhow!("drawing the diagram panicked"))),
        Verdict::TooLarge => Err(anyhow!("the diagram is too large to draw")),
        Verdict::Unreadable => Err(anyhow!("the diagram cannot be read")),
    };
    if drawn.is_err() {
        let verdict = match judged {
            Verdict::TooLarge => Verdict::TooLarge,
            _ => Verdict::Unreadable,
        };
        remember_verdict(content_hash(source), verdict);
        VERDICT_EPOCH.fetch_add(1, Ordering::Relaxed);
    }
    drawn
}

/// What usvg reads the diagram's SVG with: the system fonts, and no pictures
/// at all. usvg would otherwise load any path a diagram names, and a device
/// such as /dev/zero never ends.
fn svg_options(fontdb: Arc<usvg::fontdb::Database>, family: &str) -> usvg::Options<'static> {
    usvg::Options {
        fontdb,
        // For text that names no font (some diagrams' does not): the labels'
        // own family, not usvg's Times New Roman.
        font_family: family.to_string(),
        image_href_resolver: usvg::ImageHrefResolver {
            resolve_data: Box::new(|_, _, _| None),
            resolve_string: Box::new(|_, _| None),
        },
        ..usvg::Options::default()
    }
}

fn draw(source: &str, style: DiagramStyle, room: DiagramBox) -> Result<RenderedDiagram> {
    let fonts = fonts();
    let svg = render_svg(source, style, &fonts)?;
    let options = svg_options(Arc::clone(&fonts.db), &fonts.family);
    let tree = usvg::Tree::from_str(&svg, &options).context("read the diagram's SVG")?;
    drop(svg);
    let natural = (tree.size().width(), tree.size().height());
    let scale = raster_scale(natural, room);
    let (width, height) = raster_size(natural, room);
    let mut pixmap =
        tiny_skia::Pixmap::new(width, height).context("allocate the diagram's pixels")?;
    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    drop(tree);
    let png = pixmap.encode_png().context("encode the diagram")?;
    Ok(RenderedDiagram {
        png,
        width,
        height,
        natural,
    })
}

/// Measures labels with the font they are drawn in, where Mermaid's own font
/// (whose metrics merman ships) is missing. Advances come straight from the
/// font, remembered per character; a character the font lacks is drawn from a
/// fallback font, and is counted as a full em when it is a wide (CJK)
/// character, as most such fonts draw it, else as a typical Latin advance.
struct FontMeasurer {
    regular: Option<Face>,
    bold: Option<Face>,
    advances: Mutex<HashMap<(char, bool), f64>>,
}

/// One face of the measured family.
enum Face {
    /// Its font file, read once: small, and otherwise mapped and parsed again
    /// for every character not yet measured.
    Kept { data: Vec<u8>, index: u32 },
    /// Too large to keep; read from the index each time.
    Indexed {
        db: Arc<usvg::fontdb::Database>,
        id: usvg::fontdb::ID,
    },
}

impl Face {
    fn advance_em(&self, ch: char) -> Option<f64> {
        let advance = |data: &[u8], index: u32| {
            let font = ttf_parser::Face::parse(data, index).ok()?;
            let glyph = font.glyph_index(ch)?;
            let advance = font.glyph_hor_advance(glyph)?;
            Some(advance as f64 / font.units_per_em().max(1) as f64)
        };
        match self {
            Face::Kept { data, index } => advance(data, *index),
            Face::Indexed { db, id } => db.with_face_data(*id, advance).flatten(),
        }
    }
}

impl FontMeasurer {
    fn new(db: &Arc<usvg::fontdb::Database>, family: &str) -> Self {
        let face = |weight: usvg::fontdb::Weight| {
            let id = db.query(&usvg::fontdb::Query {
                families: &[usvg::fontdb::Family::Name(family)],
                weight,
                ..usvg::fontdb::Query::default()
            })?;
            let kept = db.with_face_data(id, |data, index| {
                (data.len() <= MAX_KEPT_FONT_BYTES).then(|| Face::Kept {
                    data: data.to_vec(),
                    index,
                })
            })?;
            Some(kept.unwrap_or_else(|| Face::Indexed {
                db: Arc::clone(db),
                id,
            }))
        };
        Self {
            regular: face(usvg::fontdb::Weight::NORMAL),
            bold: face(usvg::fontdb::Weight::BOLD),
            advances: Mutex::new(HashMap::new()),
        }
    }

    /// `ch`'s advance in ems.
    fn advance_em(&self, ch: char, bold: bool) -> f64 {
        if let Some(em) = self
            .advances
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .get(&(ch, bold))
        {
            return *em;
        }
        let face = if bold {
            self.bold.as_ref().or(self.regular.as_ref())
        } else {
            self.regular.as_ref()
        };
        let em = face
            .and_then(|face| face.advance_em(ch))
            .unwrap_or_else(|| {
                if unicode_width::UnicodeWidthChar::width(ch) == Some(2) {
                    1.0
                } else {
                    0.55
                }
            });
        let mut advances = self.advances.lock().unwrap_or_else(|err| err.into_inner());
        if advances.len() >= MAX_REMEMBERED_ADVANCES {
            advances.clear();
        }
        advances.insert((ch, bold), em);
        em
    }

    fn width(&self, text: &str, style: &TextStyle, bold: bool) -> f64 {
        text.chars()
            .map(|ch| self.advance_em(ch, bold))
            .sum::<f64>()
            * style.font_size
    }
}

fn is_bold(style: &TextStyle) -> bool {
    style.font_weight.as_deref().is_some_and(|weight| {
        weight.eq_ignore_ascii_case("bold")
            || weight.eq_ignore_ascii_case("bolder")
            || weight.parse::<u32>().is_ok_and(|weight| weight >= 600)
    })
}

impl TextMeasurer for FontMeasurer {
    fn measure(&self, text: &str, style: &TextStyle) -> TextMetrics {
        self.measure_wrapped(text, style, None, WrapMode::SvgLike)
    }

    fn measure_wrapped(
        &self,
        text: &str,
        style: &TextStyle,
        max_width: Option<f64>,
        wrap_mode: WrapMode,
    ) -> TextMetrics {
        let bold = is_bold(style);
        let space = self.width(" ", style, bold);
        let max_width = max_width.filter(|width| width.is_finite() && *width > 0.0);
        let mut widest = 0.0f64;
        let mut line_count = 0usize;
        // Lines as merman splits them: at `<br>` too, without trailing blanks.
        for line in &DeterministicTextMeasurer::normalized_text_lines(text) {
            let Some(max_width) = max_width else {
                widest = widest.max(self.width(line, style, bold));
                line_count += 1;
                continue;
            };
            // Greedy wrap at spaces, as SVG and HTML labels do; a word wider
            // than the line stands alone.
            let mut current = 0.0f64;
            let mut started = false;
            for word in line.split(' ').filter(|word| !word.is_empty()) {
                let word_width = self.width(word, style, bold);
                if started && current + space + word_width > max_width {
                    widest = widest.max(current);
                    line_count += 1;
                    current = word_width;
                } else {
                    current += if started {
                        space + word_width
                    } else {
                        word_width
                    };
                }
                started = true;
            }
            widest = widest.max(current);
            line_count += 1;
        }
        let line_height = match wrap_mode {
            WrapMode::HtmlLike => 1.5,
            _ => 1.1,
        };
        let line_count = line_count.max(1);
        TextMetrics {
            width: widest,
            height: line_count as f64 * style.font_size * line_height,
            line_count,
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn only_mermaid_blocks_are_diagrams() {
        assert!(is_mermaid(Some("mermaid")));
        assert!(is_mermaid(Some("Mermaid")));
        assert!(!is_mermaid(Some("rust")));
        assert!(!is_mermaid(None));
    }

    #[test]
    fn parse_verdicts_are_remembered() {
        assert!(draws("flowchart TD\n    A --> B"));
        assert!(!draws("flowchart TD\n    A --> \n    B -->> ((("));
        assert!(!draws("just some prose"));
        // Asked again, from the cache.
        assert!(draws("flowchart TD\n    A --> B"));
    }

    #[test]
    fn diagrams_are_drawn_at_the_size_they_are_shown() {
        let room = DiagramBox {
            width: 800.0,
            height: 1440.0,
            scale: 2.0,
        };
        // Small: its own size at the display's density, never enlarged.
        assert_eq!(raster_size((300.0, 200.0), room), (600, 400));
        // Wide: as wide as the room.
        assert_eq!(raster_size((1000.0, 200.0), room), (800, 160));
        // Tall: as tall as the room allows.
        assert_eq!(raster_size((200.0, 1000.0), room), (205, 1024));
        // However big the room, within the limits.
        let (width, height) = raster_size(
            (3000.0, 3000.0),
            DiagramBox {
                width: 9000.0,
                height: 9000.0,
                scale: 2.0,
            },
        );
        assert!(width as f32 <= MAX_RASTER_SIDE && height as f32 <= MAX_RASTER_SIDE);
        // Rounding each side may add a pixel's worth.
        assert!((width * height) as f32 <= MAX_RASTER_PIXELS * 1.01);
    }

    #[test]
    fn pictures_named_in_a_diagram_are_never_loaded() {
        let options = svg_options(Arc::new(usvg::fontdb::Database::new()), "sans-serif");
        let resolver = &options.image_href_resolver;
        assert!((resolver.resolve_string)("picture.png", &options).is_none());
        let png = Arc::new(b"\x89PNG\r\n\x1a\n".to_vec());
        assert!((resolver.resolve_data)("image/png", png, &options).is_none());
    }

    #[test]
    fn a_diagram_that_does_not_draw_is_remembered_as_unreadable() {
        // Parses, but relates to a requirement nobody defined.
        let source = "requirementDiagram\n    requirement one {\n    id: 1\n    text: t\n    risk: high\n    verifymethod: test\n    }\n    one - satisfies -> nowhere\n";
        assert!(draws(source));
        assert_eq!(known_problem(source), None);
        let epoch = verdict_epoch();
        let room = DiagramBox {
            width: 800.0,
            height: 800.0,
            scale: 1.0,
        };
        assert!(render(source, DiagramStyle { dark: false }, room).is_err());
        assert!(!draws(source));
        assert_eq!(known_problem(source), Some(Verdict::Unreadable));
        assert!(verdict_epoch() > epoch);
    }

    #[test]
    fn very_long_blocks_are_not_drawn() {
        let long = format!(
            "flowchart TD\n{}",
            "    A --> B\n".repeat(MAX_SOURCE_BYTES / 10)
        );
        assert!(long.len() > MAX_SOURCE_BYTES);
        assert!(!draws(&long));
        assert_eq!(known_problem(&long), Some(Verdict::TooLarge));
    }

    #[test]
    fn diagrams_with_too_many_links_are_not_drawn() {
        let mut dense = String::from("flowchart TD\n");
        for link in 0..=MAX_LINKS {
            dense.push_str(&format!(
                "    N{} --> N{}\n",
                link % 20,
                (link * 7 + 3) % 20
            ));
        }
        assert!(dense.len() <= MAX_SOURCE_BYTES);
        assert!(!draws(&dense));
        assert_eq!(known_problem(&dense), Some(Verdict::TooLarge));
        // The same number of boxes, with fewer links, draws.
        let sparse: String = dense
            .lines()
            .take(MAX_LINKS / 2)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(draws(&sparse));
    }

    #[test]
    fn a_panic_in_merman_is_a_block_that_cannot_be_read() {
        // merman 0.7 slices this ER diagram's CJK attribute type mid-character.
        let source = "erDiagram\n顧客 {\n  文字列 名前\n}";
        assert!(!draws(source));
        let room = DiagramBox {
            width: 800.0,
            height: 800.0,
            scale: 1.0,
        };
        assert!(render(source, DiagramStyle { dark: false }, room).is_err());
    }

    #[test]
    fn c4_shapes_and_titles_keep_readable_colours() {
        let fonts = fonts();
        let c4 = "C4Context\n    title Context\n    Person(user, \"User\")\n    System(app, \"App\")\n    Rel(user, app, \"Uses\")";
        let venn = "venn-beta\n    title Overlap\n    set Desirable\n    set Feasible\n    union Desirable,Feasible[\"Buildable\"]";
        for dark in [true, false] {
            let style = DiagramStyle { dark };
            // merman letters C4 shapes in white: their fills stay dark.
            let svg = render_svg(c4, style, &fonts).unwrap();
            assert!(
                svg.contains("#0b3d91") && svg.contains("#1d4ed8"),
                "dark={dark}"
            );
            // Titles take the look's text colour, not near-black.
            let text = if dark { "#e3e5e8" } else { "#1f2328" };
            let svg = render_svg(venn, style, &fonts).unwrap();
            let title = svg
                .split('<')
                .find(|element| element.starts_with("text class=\"venn-title\""))
                .expect("a title");
            assert!(title.contains(&format!("fill:{text}")), "dark={dark}");
        }
    }

    #[test]
    fn redraws_wait_for_a_real_change() {
        assert!(!needs_redraw(800, 800));
        assert!(!needs_redraw(800, 780));
        assert!(!needs_redraw(100, 94));
        assert!(needs_redraw(800, 600));
        assert!(needs_redraw(400, 800));
    }

    #[test]
    fn a_diagram_is_drawn_to_a_png() {
        let room = DiagramBox {
            width: 2000.0,
            height: 2000.0,
            scale: 2.0,
        };
        let drawn = render(
            "flowchart LR\n    A[打开终端] --> B{Ready?}\n    B -->|yes| C[Done]",
            DiagramStyle { dark: true },
            room,
        )
        .unwrap();
        assert!(drawn.width > 0 && drawn.height > 0);
        assert_eq!(
            (drawn.width, drawn.height),
            raster_size(drawn.natural, room)
        );
        assert!(drawn.png.starts_with(b"\x89PNG"));
        // Compressed, a diagram is far smaller than its pixels.
        assert!(drawn.png.len() < (drawn.width * drawn.height) as usize);
        assert!(render("not a diagram at all", DiagramStyle { dark: false }, room).is_err());
    }

    #[test]
    fn diagrams_are_drawn_on_a_clear_background() {
        let room = DiagramBox {
            width: 1000.0,
            height: 1000.0,
            scale: 1.0,
        };
        for dark in [true, false] {
            let drawn = render("flowchart LR\n    A --> B", DiagramStyle { dark }, room).unwrap();
            let image = image::load_from_memory(&drawn.png).unwrap().to_rgba8();
            // A corner is outside every node and edge: nothing is painted there.
            assert_eq!(image.get_pixel(0, 0)[3], 0, "dark={dark}");
            assert_eq!(
                image.get_pixel(image.width() - 1, image.height() - 1)[3],
                0,
                "dark={dark}"
            );
        }
    }

    #[test]
    fn measured_widths_follow_the_font() {
        let fonts = fonts();
        let family = fonts
            .db
            .faces()
            .find_map(|face| face.families.first().map(|(name, _)| name.clone()))
            .expect("a system font");
        let measurer = FontMeasurer::new(&fonts.db, &family);
        let style = TextStyle {
            font_size: 16.0,
            ..TextStyle::default()
        };
        let short = measurer.measure("Start", &style);
        let long = measurer.measure("Start the whole build again", &style);
        assert!(short.width > 0.0 && long.width > short.width);
        assert_eq!(short.line_count, 1);
        // Wrapping at a width narrower than the text makes more lines.
        let wrapped = measurer.measure_wrapped(
            "Start the whole build again",
            &style,
            Some(short.width * 2.0),
            WrapMode::SvgLike,
        );
        assert!(wrapped.line_count > 1);
        assert!(wrapped.width <= long.width);
        // Wide characters count a full em each where the font lacks them.
        assert!(measurer.advance_em('中', false) > 0.5);
        // `<br>` breaks a line as a newline does; a trailing newline does not.
        assert_eq!(measurer.measure("Start<br/>again", &style).line_count, 2);
        assert_eq!(measurer.measure("Start\n", &style).line_count, 1);
        // What is remembered stays bounded, however many characters pass.
        for code in 0x4e00..0x4e00 + MAX_REMEMBERED_ADVANCES as u32 + 16 {
            measurer.advance_em(char::from_u32(code).unwrap(), false);
        }
        assert!(measurer.advances.lock().unwrap().len() <= MAX_REMEMBERED_ADVANCES);
    }
}
