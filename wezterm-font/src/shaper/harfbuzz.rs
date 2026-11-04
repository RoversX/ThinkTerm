use crate::parser::ParsedFont;
use crate::shaper::{FallbackIdx, FontMetrics, FontShaper, GlyphInfo, PresentationWidth, GlyphInfoParts};
use crate::units::*;
use crate::{ftwrap, hbwrap as harfbuzz};
use anyhow::{anyhow, Context};
use config::ConfigHandle;
use finl_unicode::grapheme_clusters::Graphemes;
use log::error;
use ordered_float::NotNan;
use std::cell::{Cell, RefCell, RefMut};
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::time::{Duration, Instant};
use termwiz::cell::{unicode_column_width, Presentation};
use wezterm_bidi::Direction;

// Changing these will switch to using harfbuzz's opentype functions.
// There's something awry with our integration in that mode: the advances
// aren't equivalent to its freetype functions and this manifests most
// prominently in proportional fonts as well as with fonts with bitmap
// strikes.
// Until we get to the bottom of this, these are compile-time rather
// than runtime configs.
const USE_OT_FUNCS: bool = false;
const USE_OT_FACE: bool = false;

#[derive(Clone, Debug)]
struct Info {
    cluster: usize,
    len: usize,
    codepoint: harfbuzz::hb_codepoint_t,
    x_advance: harfbuzz::hb_position_t,
    y_advance: harfbuzz::hb_position_t,
    x_offset: harfbuzz::hb_position_t,
    y_offset: harfbuzz::hb_position_t,
}

fn get_only_char(s: &str) -> Option<char> {
    let mut chars = s.chars();
    let first_char = chars.next()?;
    if chars.next().is_some() {
        None
    } else {
        Some(first_char)
    }
}

fn make_glyphinfo(text: &str, num_cells: u8, font_idx: usize, info: &Info) -> GlyphInfo {
    let is_space = text == " ";
    let only_char = get_only_char(text);
    GlyphInfo::new(
        text,
        GlyphInfoParts {
            only_char,
            is_space,
            num_cells,
            font_idx,
            glyph_pos: info.codepoint,
            cluster: info.cluster as u32,
            x_advance: PixelLength::new(f64::from(info.x_advance) / 64.0),
            y_advance: PixelLength::new(f64::from(info.y_advance) / 64.0),
            x_offset: PixelLength::new(f64::from(info.x_offset) / 64.0),
            y_offset: PixelLength::new(f64::from(info.y_offset) / 64.0),
        },
    )
}

struct FontPair {
    face: ftwrap::Face,
    font: RefCell<harfbuzz::Font>,
    shaped_any: bool,
    presentation: Presentation,
    features: Vec<harfbuzz::hb_feature_t>,
    last_size_and_dpi: RefCell<Option<(f64, u32)>>,
    /// When a pass over this font last left it with nothing shaped.
    last_fruitless: Cell<Instant>,
}

#[derive(Debug, Hash, PartialEq, Eq, Clone)]
struct MetricsKey {
    font_idx: usize,
    size: NotNan<f64>,
    dpi: u32,
}

/// How long a fallback that shapes nothing stays loaded after the last pass
/// that tried it. Loading one means opening the file, validating its tables
/// and measuring a glyph, so unloading after each pass was fine for a stray
/// character and ruinous for a screen of them: text a font covers all but a
/// little of goes through it cell by cell, and a frame of letters with
/// combining marks took 3960 loads.
const FRUITLESS_FALLBACK_LINGER: Duration = Duration::from_secs(10);

/// How many characters each font remembers lacking: more than the CJK text
/// of a page, and the set starts over once it is full.
const MISSING_CHARS_LIMIT: usize = 4096;

pub struct HarfbuzzShaper {
    handles: Vec<ParsedFont>,
    fonts: Vec<RefCell<Option<FontPair>>>,
    /// For each font, characters it was asked to shape and had no glyph for.
    /// A run made only of them goes straight on to the next font. Without
    /// this, every run of CJK text in a UI font loaded each fallback ahead
    /// of the CJK font again -- face, size, cell metrics -- since a fallback
    /// that shapes nothing does not stay loaded for long (see
    /// `unload_fruitless_fallbacks`).
    missing: Vec<RefCell<HashSet<char>>>,
    /// When `unload_fruitless_fallbacks` next looks.
    next_unload: Cell<Instant>,
    /// How many times each font has been loaded.
    #[cfg(test)]
    loads: Vec<std::cell::Cell<usize>>,
    /// The font and the range of each pass, in order.
    #[cfg(test)]
    passes: RefCell<Vec<(FallbackIdx, Range<usize>)>>,
    lib: ftwrap::Library,
    metrics: RefCell<HashMap<MetricsKey, FontMetrics>>,
    features: Vec<harfbuzz::hb_feature_t>,
    lang: harfbuzz::hb_language_t,
}

/// Make a string holding a set of unicode replacement
/// characters equal to the number of graphemes in the
/// original string.  That isn't perfect, but it should
/// be good enough to indicate that something isn't right.
fn make_question_string(s: &str) -> String {
    let len = Graphemes::new(s).count();
    let mut result = String::new();
    let c = if !is_question_string(s) {
        std::char::REPLACEMENT_CHARACTER
    } else {
        '?'
    };
    for _ in 0..len {
        result.push(c);
    }
    result
}

/// Put glyphs shaped from `question`, in place of `text` at `start` of its
/// line, where the graphemes they stand for are and as wide as those are:
/// the rest of the line is laid out from each glyph's cells.
fn place_question_glyphs(
    glyphs: &mut [GlyphInfo],
    question: &str,
    text: &str,
    start: usize,
    presentation_width: Option<&PresentationWidth>,
) {
    let mut next = start;
    let graphemes: Vec<(usize, u8)> = Graphemes::new(text)
        .map(|grapheme| {
            let this = next;
            next += grapheme.len();
            let num_cells = match presentation_width {
                Some(pw) => pw.num_cells(this..next),
                None => unicode_column_width(grapheme, None) as u8,
            };
            (this, num_cells)
        })
        .collect();
    let char_len = question.len() / graphemes.len().max(1);
    for glyph in glyphs {
        let (cluster, num_cells) = graphemes[glyph.cluster as usize / char_len];
        glyph.cluster = cluster as u32;
        glyph.num_cells = num_cells;
    }
}

fn is_question_string(s: &str) -> bool {
    for c in s.chars() {
        if c != std::char::REPLACEMENT_CHARACTER {
            return false;
        }
    }
    true
}

fn presentation_of(handle: &ParsedFont) -> Presentation {
    if handle.assume_emoji_presentation {
        Presentation::Emoji
    } else {
        Presentation::Text
    }
}

impl HarfbuzzShaper {
    pub fn new(config: &ConfigHandle, handles: &[ParsedFont]) -> anyhow::Result<Self> {
        let lib = ftwrap::Library::new()?;
        let handles = handles.to_vec();
        let mut fonts = vec![];
        let mut missing = vec![];
        #[cfg(test)]
        let mut loads = vec![];
        for _ in 0..handles.len() {
            fonts.push(RefCell::new(None));
            missing.push(RefCell::new(HashSet::new()));
            #[cfg(test)]
            loads.push(std::cell::Cell::new(0));
        }

        let lang = harfbuzz::language_from_string("en")?;

        let features: Vec<harfbuzz::hb_feature_t> = config
            .harfbuzz_features
            .iter()
            .filter_map(|s| harfbuzz::feature_from_string(s).ok())
            .collect();

        Ok(Self {
            fonts,
            handles,
            missing,
            next_unload: Cell::new(Instant::now() + FRUITLESS_FALLBACK_LINGER),
            #[cfg(test)]
            loads,
            #[cfg(test)]
            passes: RefCell::new(vec![]),
            lib,
            metrics: RefCell::new(HashMap::new()),
            features,
            lang,
        })
    }

    /// Unload the fallbacks that have shaped nothing lately. They are
    /// probably fonts from the system that are no use for what is on screen,
    /// and not worth keeping around.
    fn unload_fruitless_fallbacks(&self) {
        let now = Instant::now();
        if now < self.next_unload.get() {
            return;
        }
        self.next_unload.set(now + FRUITLESS_FALLBACK_LINGER);
        for (font_idx, opt_pair) in self.fonts.iter().enumerate() {
            let mut opt_pair = opt_pair.borrow_mut();
            let fruitless = opt_pair.as_ref().is_some_and(|pair| {
                !pair.shaped_any
                    && now.duration_since(pair.last_fruitless.get()) >= FRUITLESS_FALLBACK_LINGER
            });
            if fruitless {
                log::trace!(
                    "Shaper didn't resolve glyphs from {:?}, so unload it",
                    self.handles[font_idx]
                );
                opt_pair.take();
            }
        }
    }

    /// Whether `font_idx` has been seen to have no glyph for any character
    /// of `text`, each a grapheme of its own. Characters that combine can
    /// shape into a glyph none of them has alone -- conjoining Hangul jamo
    /// into a syllable -- so a run with any is tried as before.
    fn lacks_all(&self, font_idx: FallbackIdx, text: &str) -> bool {
        let Some(missing) = self.missing.get(font_idx) else {
            return false;
        };
        let missing = missing.borrow();
        !text.is_empty()
            && !missing.is_empty()
            && text.chars().all(|c| missing.contains(&c))
            && Graphemes::new(text).all(|grapheme| get_only_char(grapheme).is_some())
    }

    fn note_missing(&self, font_idx: FallbackIdx, c: char) {
        if let Some(missing) = self.missing.get(font_idx) {
            let mut missing = missing.borrow_mut();
            if missing.contains(&c) {
                return;
            }
            if missing.len() >= MISSING_CHARS_LIMIT {
                missing.clear();
            }
            missing.insert(c);
        }
    }

    fn load_fallback(
        &self,
        font_idx: FallbackIdx,
        dpi: u32,
    ) -> anyhow::Result<Option<RefMut<'_, FontPair>>> {
        if font_idx >= self.handles.len() {
            return Ok(None);
        }
        match self.fonts.get(font_idx) {
            None => Ok(None),
            Some(opt_pair) => {
                let mut opt_pair = opt_pair.borrow_mut();
                if opt_pair.is_none() {
                    let handle = &self.handles[font_idx];
                    log::trace!("shaper wants {} {:?}", font_idx, handle);
                    #[cfg(test)]
                    self.loads[font_idx].set(self.loads[font_idx].get() + 1);
                    let face = self.lib.face_from_locator(&handle.handle)?;

                    let font = if USE_OT_FACE {
                        harfbuzz::Font::from_locator(&handle.handle)?
                    } else {
                        let (load_flags, _) = ftwrap::compute_load_flags_from_config(
                            handle.freetype_load_flags,
                            handle.freetype_load_target,
                            handle.freetype_render_target,
                            Some(dpi),
                        );
                        let mut font = harfbuzz::Font::new(face.face);
                        font.set_load_flags(load_flags);
                        font
                    };

                    let features = match &handle.harfbuzz_features {
                        Some(features) => features
                            .iter()
                            .filter_map(|s| harfbuzz::feature_from_string(s).ok())
                            .collect(),
                        None => self.features.clone(),
                    };

                    *opt_pair = Some(FontPair {
                        face,
                        font: RefCell::new(font),
                        shaped_any: false,
                        presentation: presentation_of(handle),
                        features,
                        last_size_and_dpi: RefCell::new(None),
                        last_fruitless: Cell::new(Instant::now()),
                    });
                }

                Ok(Some(RefMut::map(opt_pair, |opt_pair| {
                    opt_pair.as_mut().unwrap()
                })))
            }
        }
    }

    fn do_shape(
        &self,
        mut font_idx: FallbackIdx,
        s: &str,
        font_size: f64,
        dpi: u32,
        no_glyphs: &mut Vec<char>,
        presentation: Option<Presentation>,
        direction: Direction,
        range: Range<usize>,
        presentation_width: Option<&PresentationWidth>,
    ) -> anyhow::Result<Vec<GlyphInfo>> {
        #[cfg(test)]
        self.passes.borrow_mut().push((font_idx, range.clone()));
        let mut buf = harfbuzz::Buffer::new()?;
        // We deliberately omit setting the script and leave it to harfbuzz
        // to infer from the buffer contents so that it can correctly
        // enable appropriate preprocessing for eg: Hangul.
        // <https://github.com/wezterm/wezterm/issues/1474> and
        // <https://github.com/wezterm/wezterm/issues/1573>
        // buf.set_script(harfbuzz::hb_script_t::HB_SCRIPT_LATIN);
        buf.set_direction(match direction {
            Direction::LeftToRight => harfbuzz::hb_direction_t::HB_DIRECTION_LTR,
            Direction::RightToLeft => harfbuzz::hb_direction_t::HB_DIRECTION_RTL,
        });
        buf.set_language(self.lang);

        buf.add_str(s, range.clone());
        buf.guess_segment_properties();
        buf.set_cluster_level(
            harfbuzz::hb_buffer_cluster_level_t::HB_BUFFER_CLUSTER_LEVEL_MONOTONE_GRAPHEMES,
        );

        let shaped_any;

        // We set this to true when we've run out of fallback fonts to try.
        // In that case, we accept shaper info with codepoint==0 and
        // will use the notdef glyph from the base font.
        let mut no_more_fallbacks = false;

        loop {
            // A font already seen without any of these characters would only
            // hand them all on to the next one.
            if !no_more_fallbacks && self.lacks_all(font_idx, &s[range.clone()]) {
                font_idx += 1;
                continue;
            }
            // Before loading it: a font only ever passed over would have
            // nothing to show for being loaded, and go again.
            if let (Some(p), Some(handle)) = (presentation, self.handles.get(font_idx)) {
                let font_presentation = presentation_of(handle);
                if font_presentation != p {
                    log::trace!(
                        "wanted presentation is {p:?} != font \
                             presentation {font_presentation:?} so skip \
                             font_idx={font_idx}"
                    );
                    font_idx += 1;
                    continue;
                }
            }
            match self.load_fallback(font_idx, dpi).context("load_fallback")? {
                Some(mut pair) => {
                    let point_size = font_size * self.handles[font_idx].scale.unwrap_or(1.);

                    // Tell harfbuzz to recompute important font metrics!
                    if *pair.last_size_and_dpi.borrow() != Some((point_size, dpi)) {
                        let selected_font_size = pair.face.set_font_size(point_size, dpi)?;

                        let pixel_size = if selected_font_size.is_scaled {
                            (point_size * dpi as f64 / 72.) as u32
                        } else {
                            selected_font_size.height as u32
                        };

                        let mut font = pair.font.borrow_mut();

                        if USE_OT_FACE {
                            font.set_ppem(pixel_size, pixel_size);
                            font.set_ptem(point_size as f32);
                            let scale = pixel_size as i32 * 64;
                            font.set_font_scale(scale, scale);
                        }

                        font.font_changed();

                        if USE_OT_FUNCS {
                            font.set_ot_funcs();
                        }
                        pair.last_size_and_dpi
                            .borrow_mut()
                            .replace((point_size, dpi));
                    }

                    let mut font = pair.font.borrow_mut();
                    shaped_any = pair.shaped_any;
                    font.shape(&mut buf, pair.features.as_slice());
                    log::trace!(
                        "shaped font_idx={} {:?} presentation={presentation:?} as: {}",
                        font_idx,
                        &s[range.start..range.end],
                        buf.serialize(Some(&*font))
                    );
                    break;
                }
                None => {
                    for c in s.chars() {
                        no_glyphs.push(c);
                    }

                    if presentation.is_some() {
                        log::debug!(
                            "Ran out of fallback options, retry shape with no presentation"
                        );
                        // Ran out of fallbacks and we have an explicit presentation.
                        // This is a little awkward; we want to record the missing
                        // glyphs so that we can resolve them async, but we also
                        // want to try the current set of fonts without forcing
                        // the presentation match as we might find the results
                        // that way.
                        // Let's restart the shape but pretend that no specific
                        // presentation was used.
                        // We'll probably match the emoji presentation for something,
                        // but might potentially discover the text presentation for
                        // that glyph in a fallback font and swap it out a little
                        // later after a flash of showing the emoji one.
                        return self.do_shape(
                            0,
                            s,
                            font_size,
                            dpi,
                            no_glyphs,
                            None,
                            direction,
                            range,
                            presentation_width,
                        );
                    }

                    // One more go around to pick up the base font and
                    // accept using the notdef glyph from that.
                    no_more_fallbacks = true;
                    font_idx = 0;
                    continue;
                }
            }
        }

        let hb_infos = buf.glyph_infos();
        let positions = buf.glyph_positions();

        let mut cluster = Vec::with_capacity(s.len());
        let mut info_clusters: Vec<Vec<Info>> = Vec::with_capacity(s.len());

        // At this point we have a list of glyphs from the shaper.
        // Each glyph will have `info.cluster` set to the byte index
        // into `s`.  Multiple byte positions can be coalesced into
        // the same `info.cluster` value, representing text that combines
        // into a ligature.
        // It is important for the terminal to understand this relationship
        // because the cell width of that range of text depends on the unicode
        // version at the time that the text was added to the terminal.
        // To calculate the width per glyph:
        // * Make a pass over the clusters to identify the `info.cluster` starting
        //   positions of all of the glyphs
        // * Sort by info.cluster
        // * Dedup
        // * We can now get the byte length of each cluster by looking at the difference
        //   between the `info.cluster` values.
        // * `presentation_width` can be used to resolve the cell width of those
        //   byte ranges.
        // * Then distribute the glyphs across that cell width when assigning them
        //   to a GlyphInfo.
        //
        // A further complication is that the shaper may cluster in surprising ways.
        // For example, the sequence "\u{28}\u{ff9f}" is a single grapheme with
        // width of 2 cells, but harfbuzz returns that as two different clusters.
        //
        // In situations where we know better (eg: when we have presentation_width)
        // we can fixup the cluster information to be based on the cell index for
        // the harfbuzz byte start.
        // <https://github.com/wezterm/wezterm/issues/2572>

        let mut cluster_resolver = ClusterResolver {
            presentation_width,
            ..Default::default()
        };

        cluster_resolver.build(hb_infos, s, &range);
        log::debug!("cluster_resolver: {cluster_resolver:#?}");

        let info_iter = hb_infos.iter().zip(positions.iter()).peekable();
        for (info, pos) in info_iter {
            let cluster_info = match cluster_resolver.get_mut(info.cluster as usize) {
                Some(i) => i,
                None => panic!(
                    "expected cluster info.cluster {} to be in cluster_resolver",
                    info.cluster
                ),
            };
            let len = cluster_info.byte_len;

            let mut info = Info {
                // Take care to use our adjusted cluster_info.start rather
                // than the info.cluster value, otherwise the combination
                // of info.cluster + info.len will be out of bounds later
                // in this code
                cluster: cluster_info.start,
                len,
                codepoint: info.codepoint,
                x_advance: pos.x_advance,
                y_advance: pos.y_advance,
                x_offset: pos.x_offset,
                y_offset: pos.y_offset,
            };
            log::debug!("hb info.cluster {} -> {info:?}", info.cluster);

            if info.codepoint == 0 && !no_more_fallbacks {
                cluster_info.incomplete = true;
                // A character that shaped on its own and got no glyph is
                // one this font does not have.
                let start = cluster_info.start;
                if let Some(c) = get_only_char(&s[start..start + cluster_info.byte_len]) {
                    self.note_missing(font_idx, c);
                }
            }

            if let Some(ref mut cluster) = info_clusters.last_mut() {
                // Don't fragment runs of unresolved codepoints; they could be a sequence
                // that shapes together in a fallback font.
                if info.codepoint == 0 && !no_more_fallbacks {
                    let prior = cluster.last_mut().unwrap();
                    // This logic essentially merges `info` into `prior` by
                    // extending the length of prior by `info`.
                    // We can only do that if they are contiguous.
                    // Take care, as the shaper may have re-ordered things!
                    if prior.codepoint == 0 || prior.cluster == info.cluster {
                        if prior.cluster + prior.len == info.cluster {
                            // Coalesce with prior
                            prior.len += info.len;
                            continue;
                        } else if info.cluster + info.len == prior.cluster {
                            // We actually precede prior; we must have been
                            // re-ordered by the shaper. Re-arrange and
                            // coalesce
                            std::mem::swap(&mut info, prior);
                            prior.len += info.len;
                            continue;
                        } else if info.cluster + info.len == prior.cluster + prior.len {
                            // Overlaps and coincide with the end of prior; this one folds away.
                            // This can happen with NFD rather than NFC text.
                            // <https://github.com/wezterm/wezterm/issues/2032>
                            continue;
                        }
                        // log::info!("prior={:#?}, info={:#?}", prior, info);
                    }
                }

                // It is important that this bit happens after we've had the
                // opportunity to coalesce runs of unresolved codepoints,
                // otherwise we can produce incorrect shaping
                // <https://github.com/wezterm/wezterm/issues/2482>
                if cluster.last().unwrap().cluster == info.cluster {
                    cluster.push(info);
                    continue;
                }
            }
            info_clusters.push(vec![info]);
        }
        //  log::error!("do_shape: font_idx={} {:?} {:#?}", font_idx, &s[range.clone()], info_clusters);
        log::debug!("font_idx={font_idx} info_clusters: {:#?}", info_clusters);

        let mut direct_clusters = 0;

        // One or more entries of a cluster didn't have a corresponding glyph,
        // so it needs a fallback. Neighbours that do go on together: the
        // coalescing above only joins what got no glyph at all, which leaves
        // letters with marks the font lacks one at a time, and a pass per
        // cell made a screen of them take 70 ms. Only those of one script,
        // though: a pass is shaped under the script guessed from its text,
        // and a cell put under another's does not come out as it should.
        let mut incomplete: Option<(Range<usize>, Option<harfbuzz::hb_script_t>)> = None;
        let shape_incomplete = |run: Range<usize>,
                                cluster: &mut Vec<GlyphInfo>,
                                no_glyphs: &mut Vec<char>|
         -> anyhow::Result<()> {
            let mut shape = match self.do_shape(
                font_idx + 1,
                s,
                font_size,
                dpi,
                no_glyphs,
                presentation,
                direction,
                // NOT! substr; this is a coalesced sequence of incomplete clusters!
                run.clone(),
                presentation_width,
            ) {
                Ok(shape) => shape,
                Err(e) => {
                    let substr = &s[run.clone()];
                    error!("{:?} for {:?}", e, substr);
                    let question = make_question_string(substr);
                    let mut shape = self.do_shape(
                        0,
                        &question,
                        font_size,
                        dpi,
                        no_glyphs,
                        presentation,
                        direction,
                        0..question.len(),
                        // That one describes the line, not this.
                        None,
                    )?;
                    place_question_glyphs(
                        &mut shape,
                        &question,
                        substr,
                        run.start,
                        presentation_width,
                    );
                    shape
                }
            };
            cluster.append(&mut shape);
            Ok(())
        };

        for infos in &info_clusters {
            let cluster_info = cluster_resolver
                .get(infos[0].cluster)
                .expect("assigned above");
            let substr = &s[cluster_info.start..cluster_info.start + cluster_info.byte_len];

            if cluster_info.incomplete {
                let first_info = &infos[0];
                let this = first_info.cluster..first_info.cluster + first_info.len;
                // Coalescing can have joined this cluster's first glyphs to
                // what came before and still left it glyphs of its own.
                if incomplete
                    .as_ref()
                    .is_some_and(|(run, _)| run.start <= this.start && this.end <= run.end)
                {
                    continue;
                }
                let script = harfbuzz::guess_script(&s[this.clone()]);
                match &mut incomplete {
                    Some((run, run_script)) if run.end == this.start && *run_script == script => {
                        run.end = this.end
                    }
                    run => {
                        if let Some((run, _)) = run.replace((this, script)) {
                            shape_incomplete(run, &mut cluster, no_glyphs)?;
                        }
                    }
                }
                continue;
            }
            if let Some((run, _)) = incomplete.take() {
                shape_incomplete(run, &mut cluster, no_glyphs)?;
            }

            let total_width: f64 = infos.iter().map(|info| info.x_advance as f64).sum();
            let mut remaining_cells = cluster_info.cell_width;

            for info in infos.iter() {
                // Proportional width based on relative pixel dimensions vs. other glyphs in
                // this same cluster
                // Note that weighted_cell_width can legitimately compute as zero here
                // for the case where a combining mark composes over another glyph
                // However, some symbol fonts have broken advance metrics and we don't
                // want those glyphs to end up with zero width, so if this run is zero
                // width then we round up to 1 cell.
                // <https://github.com/wezterm/wezterm/issues/1787>
                let weighted_cell_width = if total_width == 0. {
                    1
                } else {
                    (cluster_info.cell_width as f64 * info.x_advance as f64 / total_width).ceil()
                        as u8
                };
                let weighted_cell_width = weighted_cell_width.min(remaining_cells);
                remaining_cells = remaining_cells.saturating_sub(weighted_cell_width);

                let glyph = make_glyphinfo(substr, weighted_cell_width, font_idx, info);

                cluster.push(glyph);
                direct_clusters += 1;
            }
        }
        if let Some((run, _)) = incomplete.take() {
            shape_incomplete(run, &mut cluster, no_glyphs)?;
        }

        if !shaped_any {
            if let Some(opt_pair) = self.fonts.get(font_idx) {
                if let Some(pair) = &mut *opt_pair.borrow_mut() {
                    if direct_clusters == 0 {
                        // We've never shaped anything from this font, and we
                        // didn't shape anything just now: it goes if that
                        // stays so (see `unload_fruitless_fallbacks`).
                        pair.last_fruitless.set(Instant::now());
                    } else {
                        // We shaped something: mark this pair up so that it sticks around
                        pair.shaped_any = true;
                    }
                }
            }
        }

        Ok(cluster)
    }
}

impl FontShaper for HarfbuzzShaper {
    fn shape(
        &self,
        text: &str,
        size: f64,
        dpi: u32,
        no_glyphs: &mut Vec<char>,
        presentation: Option<Presentation>,
        direction: Direction,
        range: Option<Range<usize>>,
        presentation_width: Option<&PresentationWidth>,
    ) -> anyhow::Result<Vec<GlyphInfo>> {
        let range = range.unwrap_or_else(|| 0..text.len());
        self.unload_fruitless_fallbacks();

        log::trace!(
            "shape {range:?} `{}` with presentation={presentation:?}",
            text.escape_debug()
        );
        let start = std::time::Instant::now();
        let result = self.do_shape(
            0,
            text,
            size,
            dpi,
            no_glyphs,
            presentation,
            direction,
            range,
            presentation_width,
        );
        metrics::histogram!("shape.harfbuzz").record(start.elapsed());
        /*
        if let Ok(glyphs) = &result {
            for g in glyphs {
                if g.font_idx > 0 {
                    log::error!("{:#?}", result);
                    break;
                }
            }
        }
        */
        result
    }

    fn metrics_for_idx(&self, font_idx: usize, size: f64, dpi: u32) -> anyhow::Result<FontMetrics> {
        let mut pair = self
            .load_fallback(font_idx, dpi)?
            .ok_or_else(|| anyhow!("metrics_for_idx: there is no font with idx={font_idx}!?"))?;

        let key = MetricsKey {
            font_idx,
            size: NotNan::new(size).unwrap(),
            dpi,
        };
        if let Some(metrics) = self.metrics.borrow().get(&key) {
            return Ok(*metrics);
        }

        let scale = self.handles[font_idx].scale.unwrap_or(1.);

        let selected_size = pair.face.set_font_size(size * scale, dpi)?;
        let y_scale = unsafe { (*(*pair.face.face).size).metrics.y_scale.to_num::<f64>() };
        let mut metrics = FontMetrics {
            cell_height: PixelLength::new(selected_size.height),
            cell_width: PixelLength::new(selected_size.width),
            // Note: face.face.descender is useless, we have to go through
            // face.face.size.metrics to get to the real descender!
            descender: PixelLength::new(unsafe {
                (*(*pair.face.face).size).metrics.descender.f26d6().to_num()
            }),
            underline_thickness: PixelLength::new(
                unsafe { (*pair.face.face).underline_thickness as f64 } * y_scale / 64.,
            ),
            underline_position: PixelLength::new(
                unsafe { (*pair.face.face).underline_position as f64 } * y_scale / 64.,
            ),
            cap_height_ratio: selected_size.cap_height_to_height_ratio,
            cap_height: selected_size.cap_height.map(PixelLength::new),
            is_scaled: selected_size.is_scaled,
            presentation: pair.presentation,
            force_y_adjust: PixelLength::new(0.),
        };

        // When the user has overridden the scale, we need to stash a y-adjustment
        // so that the glyphs are better vertically aligned
        // <https://github.com/wezterm/wezterm/issues/1803>
        if scale != 1.0 && metrics.is_scaled {
            let diff = metrics.descender - (metrics.descender / scale);
            metrics.force_y_adjust = diff;
        }

        self.metrics.borrow_mut().insert(key, metrics);

        log::trace!(
            "metrics_for_idx={}, size={}, dpi={} -> {:?}",
            font_idx,
            size,
            dpi,
            metrics
        );

        Ok(metrics)
    }

    fn metrics(&self, size: f64, dpi: u32) -> anyhow::Result<FontMetrics> {
        // Returns the metrics for the selected font... but look out
        // for implausible sizes.
        // Ideally we wouldn't need this, but in the event that a user
        // has a wonky configuration we don't want to pick something
        // like a bitmap emoji font for the metrics or well end up
        // with crazy huge cells.
        // We do a sniff test based on the theoretical pixel height for
        // the supplied size+dpi.
        // If a given fallback slot deviates from the theoretical size
        // by too much we'll skip to the next slot.
        let theoretical_height = size * dpi as f64 / 72.0;
        let mut metrics_idx = 0;
        log::trace!(
            "compute metrics across these handles for size={}, dpi={},
             theoretical pixel height {}: {:?}",
            size,
            dpi,
            theoretical_height,
            self.handles
        );
        while let Ok(Some(mut pair)) = self.load_fallback(metrics_idx, dpi) {
            let selected_size = pair
                .face
                .set_font_size(size * self.handles[metrics_idx].scale.unwrap_or(1.), dpi)?;
            let diff = (theoretical_height - selected_size.height).abs();
            let factor = diff / theoretical_height;
            if factor < 2.0 {
                log::trace!(
                    "idx {} cell_height is {}, which is {} away from theoretical
                     height (factor {}). Seems good enough",
                    metrics_idx,
                    selected_size.height,
                    diff,
                    factor
                );
                break;
            }
            log::trace!(
                "skip idx {} because diff={} factor={} theoretical_height={} cell_height={}",
                metrics_idx,
                diff,
                factor,
                theoretical_height,
                selected_size.height
            );

            if metrics_idx + 1 >= self.handles.len() {
                log::warn!(
                    "metrics: I wanted to skip idx {} because diff={} factor={} \
                    theoretical_height={} cell_height={}, but there are no more \
                    fallback fonts. Metrics will likely be crazy.",
                    metrics_idx,
                    diff,
                    factor,
                    theoretical_height,
                    selected_size.height
                );
                break;
            }

            metrics_idx += 1;
        }

        self.metrics_for_idx(metrics_idx, size, dpi)
    }
}

#[derive(Debug)]
struct ClusterInfo {
    start: usize,
    cell_idx: Option<usize>,
    byte_len: usize,
    cell_width: u8,
    incomplete: bool,
}

#[derive(Default, Debug)]
struct ClusterResolver<'a> {
    /// In order of `cell_idx` and `start`: every line of text comes through
    /// here for each font, so these are looked up, not hashed.
    clusters: Vec<ClusterInfo>,
    presentation_width: Option<&'a PresentationWidth<'a>>,
}

impl<'a> ClusterResolver<'a> {
    pub fn build(&mut self, hb_infos: &[harfbuzz::hb_glyph_info_t], s: &str, range: &Range<usize>) {
        #[derive(PartialOrd, Ord, Eq, PartialEq, Copy, Clone)]
        struct Item {
            cell_idx: Option<usize>,
            start: usize,
        }

        // Collect the cell index, which is the "true cluster"
        // position from our perspective: the start of the grapheme
        let mut cluster_starts: Vec<Item> = hb_infos
            .iter()
            .map(|info| {
                let start = info.cluster as usize;
                Item {
                    cell_idx: self.presentation_width.map(|pw| pw.byte_to_cell_idx(start)),
                    start,
                }
            })
            .collect();
        cluster_starts.sort_unstable();
        cluster_starts.dedup();

        // If we have multiple entries with the same starting cell index,
        // remove the duplicates.  Don't do this for `None` as that will
        // falsely remove valid cluster locations in the case where
        // we have no presentation_width information.
        cluster_starts.dedup_by(|a, b| match (a.cell_idx, b.cell_idx) {
            (Some(a), Some(b)) => a == b,
            _ => false,
        });

        self.clusters.reserve(cluster_starts.len());
        let mut iter = cluster_starts.iter().peekable();
        while let Some(item) = iter.next().copied() {
            let start = item.start;
            let next_start = iter.peek().map(|&&s| s.start).unwrap_or(range.end);
            let byte_len = next_start - start;
            let cell_width = match self.presentation_width {
                Some(p) => p.num_cells(start..next_start),
                None => unicode_column_width(&s[start..next_start], None) as u8,
            };
            self.clusters.push(ClusterInfo {
                start,
                cell_idx: item.cell_idx,
                byte_len,
                cell_width,
                incomplete: false,
            });
        }
    }

    fn position(&self, start: usize) -> Option<usize> {
        match self.presentation_width {
            Some(pw) => {
                let cell_idx = Some(pw.byte_to_cell_idx(start));
                self.clusters
                    .binary_search_by_key(&cell_idx, |cluster| cluster.cell_idx)
                    .ok()
            }
            None => self
                .clusters
                .binary_search_by_key(&start, |cluster| cluster.start)
                .ok(),
        }
    }

    pub fn get_mut(&mut self, start: usize) -> Option<&mut ClusterInfo> {
        let position = self.position(start)?;
        self.clusters.get_mut(position)
    }

    pub fn get(&self, start: usize) -> Option<&ClusterInfo> {
        self.clusters.get(self.position(start)?)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::FontDatabase;
    use config::FontAttributes;
    use termwiz::cell::CellAttributes;
    use termwiz::surface::{Line, SEQ_ZERO};

    #[test]
    fn ligatures() {
        let _ = env_logger::Builder::new()
            .is_test(true)
            .filter_level(log::LevelFilter::Trace)
            .try_init();

        let db = FontDatabase::with_built_in().unwrap();
        let handle = db
            .resolve(
                &FontAttributes {
                    family: "JetBrains Mono".into(),
                    stretch: Default::default(),
                    weight: Default::default(),
                    is_fallback: false,
                    is_synthetic: false,
                    style: Default::default(),
                    freetype_load_flags: None,
                    freetype_load_target: None,
                    freetype_render_target: None,
                    harfbuzz_features: None,
                    scale: None,
                    assume_emoji_presentation: None,
                },
                14,
            )
            .unwrap()
            .clone();

        let config = config::configuration();

        let shaper = HarfbuzzShaper::new(&config, &[handle]).unwrap();
        {
            let mut no_glyphs = vec![];
            let info = shaper
                .shape(
                    "abc",
                    10.,
                    72,
                    &mut no_glyphs,
                    None,
                    Direction::LeftToRight,
                    None,
                    None,
                )
                .unwrap();
            assert!(no_glyphs.is_empty(), "{:?}", no_glyphs);
            k9::snapshot!(
                info,
                r#"
[
    GlyphInfo {
        text: "a",
        only_char: Some(
            'a',
        ),
        is_space: false,
        num_cells: 1,
        cluster: 0,
        font_idx: 0,
        glyph_pos: 189,
        x_advance: 6.0,
        y_advance: 0.0,
        x_offset: 0.0,
        y_offset: 0.0,
    },
    GlyphInfo {
        text: "b",
        only_char: Some(
            'b',
        ),
        is_space: false,
        num_cells: 1,
        cluster: 1,
        font_idx: 0,
        glyph_pos: 214,
        x_advance: 6.0,
        y_advance: 0.0,
        x_offset: 0.0,
        y_offset: 0.0,
    },
    GlyphInfo {
        text: "c",
        only_char: Some(
            'c',
        ),
        is_space: false,
        num_cells: 1,
        cluster: 2,
        font_idx: 0,
        glyph_pos: 215,
        x_advance: 6.0,
        y_advance: 0.0,
        x_offset: 0.0,
        y_offset: 0.0,
    },
]
"#
            );
        }
        {
            let mut no_glyphs = vec![];
            let info = shaper
                .shape(
                    "<",
                    10.,
                    72,
                    &mut no_glyphs,
                    None,
                    Direction::LeftToRight,
                    None,
                    None,
                )
                .unwrap();
            assert!(no_glyphs.is_empty(), "{:?}", no_glyphs);
            k9::snapshot!(
                info,
                r#"
[
    GlyphInfo {
        text: "<",
        only_char: Some(
            '<',
        ),
        is_space: false,
        num_cells: 1,
        cluster: 0,
        font_idx: 0,
        glyph_pos: 1052,
        x_advance: 6.0,
        y_advance: 0.0,
        x_offset: 0.0,
        y_offset: 0.0,
    },
]
"#
            );
        }
        {
            // This is a ligatured sequence, but you wouldn't know
            // from this info :-/
            let mut no_glyphs = vec![];
            let info = shaper
                .shape(
                    "<-",
                    10.,
                    72,
                    &mut no_glyphs,
                    None,
                    Direction::LeftToRight,
                    None,
                    None,
                )
                .unwrap();
            assert!(no_glyphs.is_empty(), "{:?}", no_glyphs);
            k9::snapshot!(
                info,
                r#"
[
    GlyphInfo {
        text: "<",
        only_char: Some(
            '<',
        ),
        is_space: false,
        num_cells: 1,
        cluster: 0,
        font_idx: 0,
        glyph_pos: 1742,
        x_advance: 6.0,
        y_advance: 0.0,
        x_offset: 0.0,
        y_offset: 0.0,
    },
    GlyphInfo {
        text: "-",
        only_char: Some(
            '-',
        ),
        is_space: false,
        num_cells: 1,
        cluster: 1,
        font_idx: 0,
        glyph_pos: 1588,
        x_advance: 6.0,
        y_advance: 0.0,
        x_offset: 0.0,
        y_offset: 0.0,
    },
]
"#
            );
        }
        {
            let mut no_glyphs = vec![];
            let info = shaper
                .shape(
                    "<--",
                    10.,
                    72,
                    &mut no_glyphs,
                    None,
                    Direction::LeftToRight,
                    None,
                    None,
                )
                .unwrap();
            assert!(no_glyphs.is_empty(), "{:?}", no_glyphs);
            k9::snapshot!(
                info,
                r#"
[
    GlyphInfo {
        text: "<",
        only_char: Some(
            '<',
        ),
        is_space: false,
        num_cells: 1,
        cluster: 0,
        font_idx: 0,
        glyph_pos: 1742,
        x_advance: 6.0,
        y_advance: 0.0,
        x_offset: 0.0,
        y_offset: 0.0,
    },
    GlyphInfo {
        text: "-",
        only_char: Some(
            '-',
        ),
        is_space: false,
        num_cells: 1,
        cluster: 1,
        font_idx: 0,
        glyph_pos: 1742,
        x_advance: 6.0,
        y_advance: 0.0,
        x_offset: 0.0,
        y_offset: 0.0,
    },
    GlyphInfo {
        text: "-",
        only_char: Some(
            '-',
        ),
        is_space: false,
        num_cells: 1,
        cluster: 2,
        font_idx: 0,
        glyph_pos: 1589,
        x_advance: 6.0,
        y_advance: 0.0,
        x_offset: 0.0,
        y_offset: 0.0,
    },
]
"#
            );
        }

        {
            let mut no_glyphs = vec![];
            let info = shaper
                .shape(
                    "x x",
                    10.,
                    72,
                    &mut no_glyphs,
                    None,
                    Direction::LeftToRight,
                    None,
                    None,
                )
                .unwrap();
            assert!(no_glyphs.is_empty(), "{:?}", no_glyphs);
            k9::snapshot!(
                info,
                r#"
[
    GlyphInfo {
        text: "x",
        only_char: Some(
            'x',
        ),
        is_space: false,
        num_cells: 1,
        cluster: 0,
        font_idx: 0,
        glyph_pos: 367,
        x_advance: 6.0,
        y_advance: 0.0,
        x_offset: 0.0,
        y_offset: 0.0,
    },
    GlyphInfo {
        text: " ",
        only_char: Some(
            ' ',
        ),
        is_space: true,
        num_cells: 1,
        cluster: 1,
        font_idx: 0,
        glyph_pos: 958,
        x_advance: 6.0,
        y_advance: 0.0,
        x_offset: 0.0,
        y_offset: 0.0,
    },
    GlyphInfo {
        text: "x",
        only_char: Some(
            'x',
        ),
        is_space: false,
        num_cells: 1,
        cluster: 2,
        font_idx: 0,
        glyph_pos: 367,
        x_advance: 6.0,
        y_advance: 0.0,
        x_offset: 0.0,
        y_offset: 0.0,
    },
]
"#
            );
        }

        {
            let mut no_glyphs = vec![];
            let info = shaper
                .shape(
                    "x\u{3000}x",
                    10.,
                    72,
                    &mut no_glyphs,
                    None,
                    Direction::LeftToRight,
                    None,
                    None,
                )
                .unwrap();
            assert!(no_glyphs.is_empty(), "{:?}", no_glyphs);
            k9::snapshot!(
                info,
                r#"
[
    GlyphInfo {
        text: "x",
        only_char: Some(
            'x',
        ),
        is_space: false,
        num_cells: 1,
        cluster: 0,
        font_idx: 0,
        glyph_pos: 367,
        x_advance: 6.0,
        y_advance: 0.0,
        x_offset: 0.0,
        y_offset: 0.0,
    },
    GlyphInfo {
        text: "\u{3000}",
        only_char: Some(
            '\u{3000}',
        ),
        is_space: false,
        num_cells: 2,
        cluster: 1,
        font_idx: 0,
        glyph_pos: 958,
        x_advance: 10.0,
        y_advance: 0.0,
        x_offset: 0.0,
        y_offset: 0.0,
    },
    GlyphInfo {
        text: "x",
        only_char: Some(
            'x',
        ),
        is_space: false,
        num_cells: 1,
        cluster: 4,
        font_idx: 0,
        glyph_pos: 367,
        x_advance: 6.0,
        y_advance: 0.0,
        x_offset: 0.0,
        y_offset: 0.0,
    },
]
"#
            );
        }
    }

    /// None of the first three has emoji; none of the four has CJK.
    fn emoji_last() -> Vec<ParsedFont> {
        let db = FontDatabase::with_built_in().unwrap();
        [
            "Roboto",
            "JetBrains Mono",
            "Symbols Nerd Font Mono",
            "Noto Color Emoji",
        ]
        .iter()
        .map(|family| db.resolve(&FontAttributes::new(family), 14).unwrap().clone())
        .collect()
    }

    fn shape_with(
        shaper: &HarfbuzzShaper,
        text: &str,
        presentation: Option<Presentation>,
        presentation_width: Option<&PresentationWidth>,
    ) -> (Vec<GlyphInfo>, Vec<char>) {
        let mut no_glyphs = vec![];
        let info = shaper
            .shape(
                text,
                10.,
                72,
                &mut no_glyphs,
                presentation,
                Direction::LeftToRight,
                None,
                presentation_width,
            )
            .unwrap();
        (info, no_glyphs)
    }

    /// Fallbacks seen without a run's characters are passed over the next
    /// time -- not even loaded -- and the run shapes exactly as it does when
    /// every font is tried.
    #[test]
    fn fallbacks_known_to_lack_a_run_are_passed_over() {
        let handles = emoji_last();
        let config = config::configuration();
        let texts = ["x", "😀", "a😀b", "😀😀", "\u{f120}", "中", "a中"];
        let shaper = HarfbuzzShaper::new(&config, &handles).unwrap();
        for text in texts {
            shape_with(&shaper, text, None, None);
        }
        for font_idx in 0..3 {
            assert!(shaper.lacks_all(font_idx, "😀😀"), "font {font_idx}");
            assert!(!shaper.lacks_all(font_idx, "a😀"), "font {font_idx}");
        }
        assert!(!shaper.lacks_all(3, "😀"));
        for font_idx in 0..4 {
            assert!(shaper.lacks_all(font_idx, "中"), "font {font_idx}");
        }

        // Tried, JetBrains Mono would be loaded for the emoji and unloaded
        // again, as it gives none of them a glyph.
        let loads = |shaper: &HarfbuzzShaper| {
            shaper
                .loads
                .iter()
                .map(std::cell::Cell::get)
                .collect::<Vec<_>>()
        };
        let before = loads(&shaper);
        shape_with(&shaper, "😀😀", None, None);
        shape_with(&shaper, "😀😀", None, None);
        assert_eq!(loads(&shaper), before);

        for text in texts {
            for presentation in [None, Some(Presentation::Text), Some(Presentation::Emoji)] {
                let fresh = HarfbuzzShaper::new(&config, &handles).unwrap();
                assert_eq!(
                    shape_with(&shaper, text, presentation, None),
                    shape_with(&fresh, text, presentation, None),
                    "{text} {presentation:?}"
                );
            }
        }

        // And cell by cell, the way the terminal shapes a line.
        let line = Line::from_text(
            "a😀b中\u{f120}x",
            &CellAttributes::default(),
            SEQ_ZERO,
            None,
        );
        for cluster in line.cluster(None) {
            let width = PresentationWidth::with_cluster(&cluster);
            let fresh = HarfbuzzShaper::new(&config, &handles).unwrap();
            assert_eq!(
                shape_with(&shaper, &cluster.text, Some(cluster.presentation), Some(&width)),
                shape_with(&fresh, &cluster.text, Some(cluster.presentation), Some(&width)),
                "{}",
                cluster.text
            );
        }
    }

    /// A font with the letter and not its marks gets every cell of a line
    /// wrong in turn. Sent on together they come out as they do one by one.
    #[test]
    fn neighbouring_incomplete_cells_shape_as_they_do_alone() {
        let handles = emoji_last();
        let config = config::configuration();
        let cells = [
            "a\u{300}\u{301}\u{302}",
            "a\u{34f}\u{35c}\u{36f}",
            "b",
            "a\u{30a}\u{324}\u{336}",
            "a\u{361}\u{362}\u{36e}",
            "😀",
            "a\u{34f}\u{301}",
        ];
        let shaper = HarfbuzzShaper::new(&config, &handles).unwrap();
        let together: Vec<_> = shape_with(&shaper, &cells.concat(), None, None)
            .0
            .iter()
            .map(|glyph| (glyph.font_idx, glyph.glyph_pos, glyph.num_cells))
            .collect();
        let alone: Vec<_> = cells
            .iter()
            .flat_map(|cell| {
                let shaper = HarfbuzzShaper::new(&config, &handles).unwrap();
                shape_with(&shaper, cell, None, None).0
            })
            .map(|glyph| (glyph.font_idx, glyph.glyph_pos, glyph.num_cells))
            .collect();
        assert_eq!(together, alone);
    }

    /// A run sent on to a fallback is shaped under one script, HarfBuzz's
    /// guess from its first letter, and a cell of another script does not
    /// shape right under that. So a run is made of cells that guess alike:
    /// each gets what it would have got alone.
    #[test]
    fn a_run_sent_to_a_fallback_keeps_to_one_script() {
        let config = config::configuration();
        let shaper = HarfbuzzShaper::new(&config, &emoji_last()).unwrap();
        let marks = "\u{34f}\u{35c}\u{36f}";
        let latin = format!("a{marks}");
        let cyrillic = format!("\u{434}{marks}");
        let digit = format!("1{marks}");
        let text = [&latin, &latin, &cyrillic, &cyrillic, &digit, &digit, &latin]
            .map(String::as_str)
            .concat();
        shape_with(&shaper, &text, None, None);
        let at = |cells: usize| latin.len() * cells + cells.saturating_sub(2).min(2);
        let runs: Vec<_> = shaper
            .passes
            .borrow()
            .iter()
            .filter(|(font_idx, _)| *font_idx == 1)
            .map(|(_, range)| range.clone())
            .collect();
        assert_eq!(
            runs,
            [at(0)..at(2), at(2)..at(4), at(4)..at(6), at(6)..at(7)],
            "{text:?}"
        );
    }

    /// A font with no glyph for a whole cell and only part of the next, which
    /// it gives an invisible glyph for what follows, joins the two into the
    /// first and then has the second on its own as well. Sent on, the run
    /// already holds that second cell, and it must not go a second time: a
    /// cell drawn twice pushes the rest of the line along.
    #[test]
    fn a_cell_a_run_already_holds_is_not_sent_again() {
        let db = FontDatabase::with_built_in().unwrap();
        let handles: Vec<_> = ["JetBrains Mono", "Noto Color Emoji", "Symbols Nerd Font Mono"]
            .iter()
            .map(|family| db.resolve(&FontAttributes::new(family), 14).unwrap().clone())
            .collect();
        let config = config::configuration();
        let text = "o\u{361}a\u{34f}\u{35c}\u{36f}";
        let line = Line::from_text(text, &CellAttributes::default(), SEQ_ZERO, None);
        let clusters = line.cluster(None);
        let width = PresentationWidth::with_cluster(&clusters[0]);
        let shaper = HarfbuzzShaper::new(&config, &handles).unwrap();
        let (glyphs, _) = shape_with(&shaper, &clusters[0].text, None, Some(&width));
        let placed: Vec<_> = glyphs
            .iter()
            .map(|glyph| (glyph.cluster, glyph.num_cells))
            .collect();
        let cells: usize = glyphs.iter().map(|glyph| glyph.num_cells as usize).sum();
        assert_eq!(cells, 2, "{placed:?}");
    }

    /// `guess_script` is only worth having if it says what HarfBuzz says.
    #[test]
    fn the_script_guessed_is_the_one_harfbuzz_guesses() {
        use crate::hbwrap::hb_script_t::*;
        for (text, script) in [
            ("a\u{301}", Some(HB_SCRIPT_LATIN)),
            ("1\u{301}a", Some(HB_SCRIPT_LATIN)),
            ("\u{434}a", Some(HB_SCRIPT_CYRILLIC)),
            ("\u{e01}\u{e48}", Some(HB_SCRIPT_THAI)),
            ("\u{915}\u{93f}", Some(HB_SCRIPT_DEVANAGARI)),
            ("1\u{301}", None),
            ("\u{301}", None),
            ("", None),
        ] {
            let mut buf = harfbuzz::Buffer::new().unwrap();
            buf.add_str(text, 0..text.len());
            buf.guess_segment_properties();
            assert_eq!(harfbuzz::guess_script(text), script, "{text:?}");
            assert_eq!(buf.script(), script, "{text:?}");
        }
    }

    /// Loading a font to find it is the wrong sort of font cost the load,
    /// and a fallback that way never shapes anything to be kept for.
    #[test]
    fn a_font_of_the_wrong_presentation_is_not_loaded() {
        let config = config::configuration();
        let shaper = HarfbuzzShaper::new(&config, &emoji_last()).unwrap();
        shape_with(&shaper, "\u{1f600}", Some(Presentation::Emoji), None);
        let loads: Vec<_> = shaper.loads.iter().map(std::cell::Cell::get).collect();
        assert_eq!(loads, [0, 0, 0, 1]);
    }

    /// A fallback that will not load leaves what it was for showing as
    /// replacement characters, in the place of what they replace.
    #[test]
    fn text_a_broken_fallback_was_for_stays_where_it_was() {
        let mut handles = emoji_last();
        handles[1].handle.source =
            crate::locator::FontDataSource::OnDisk("/no/such/font.ttf".into());
        let config = config::configuration();
        let shaper = HarfbuzzShaper::new(&config, &handles).unwrap();
        let text = "ab\u{1100}\u{1161}\u{1100}\u{1161}cd";
        let clusters: Vec<_> = shape_with(&shaper, text, None, None)
            .0
            .iter()
            .map(|glyph| glyph.cluster)
            .collect();
        assert_eq!(clusters, [0, 1, 2, 8, 14, 15], "{text:?}");
    }

    /// The window lays a line out by each glyph's cells, so one that takes up
    /// less than what it replaces drags the rest of the line with it.
    #[test]
    fn text_a_broken_fallback_was_for_keeps_its_width() {
        let mut handles = emoji_last();
        handles[1].handle.source =
            crate::locator::FontDataSource::OnDisk("/no/such/font.ttf".into());
        let config = config::configuration();
        let shaper = HarfbuzzShaper::new(&config, &handles).unwrap();
        let line = Line::from_text(
            "x\u{4e2d}\u{6587}y",
            &CellAttributes::default(),
            SEQ_ZERO,
            None,
        );
        let clusters = line.cluster(None);
        assert_eq!(clusters.len(), 1, "one run of text");
        let width = PresentationWidth::with_cluster(&clusters[0]);
        let placed: Vec<_> = shape_with(&shaper, &clusters[0].text, None, Some(&width))
            .0
            .iter()
            .map(|glyph| (glyph.cluster, glyph.num_cells))
            .collect();
        assert_eq!(placed, [(0, 1), (1, 2), (4, 2), (7, 1)]);

        let placed: Vec<_> = shape_with(&shaper, "\u{4e2d}y", None, None)
            .0
            .iter()
            .map(|glyph| (glyph.cluster, glyph.num_cells))
            .collect();
        assert_eq!(placed, [(0, 2), (3, 1)], "with nothing to say how wide");
    }

    /// Text that combines is tried in every font each time, so a fallback
    /// unloaded after each pass was loaded again for each. One that has shaped
    /// nothing goes only once it has been left alone for a while.
    #[test]
    fn a_fallback_that_shapes_nothing_goes_after_a_while() {
        let config = config::configuration();
        let shaper = HarfbuzzShaper::new(&config, &emoji_last()).unwrap();
        let loads = |shaper: &HarfbuzzShaper| {
            shaper
                .loads
                .iter()
                .map(std::cell::Cell::get)
                .collect::<Vec<_>>()
        };
        // Conjoining jamo, which none of these fonts has.
        let text = "\u{1100}\u{1161}";
        shape_with(&shaper, text, None, None);
        let first = loads(&shaper);
        assert!(first[1..].iter().all(|&loads| loads == 1), "{first:?}");
        shape_with(&shaper, text, None, None);
        assert_eq!(loads(&shaper), first, "still loaded");

        let long_ago = Instant::now() - FRUITLESS_FALLBACK_LINGER * 2;
        for opt_pair in &shaper.fonts {
            if let Some(pair) = &*opt_pair.borrow() {
                pair.last_fruitless.set(long_ago);
            }
        }
        shaper.next_unload.set(long_ago);
        // Only Roboto is left, having shaped this; the rest were never any use.
        shape_with(&shaper, "x", None, None);
        assert!(shaper.fonts[0].borrow().is_some(), "it shapes what it can");
        for font_idx in 1..4 {
            assert!(shaper.fonts[font_idx].borrow().is_none(), "font {font_idx}");
        }
    }

    /// Characters that combine shape together, so the run is tried even when
    /// the font lacks each alone; an empty run lacks nothing.
    #[test]
    fn only_runs_of_single_character_graphemes_are_passed_over() {
        let config = config::configuration();
        let shaper = HarfbuzzShaper::new(&config, &emoji_last()).unwrap();
        // Conjoining jamo: a font can have the syllable they make and
        // neither of them.
        shaper.note_missing(0, '\u{1100}');
        shaper.note_missing(0, '\u{1161}');
        assert!(shaper.lacks_all(0, "\u{1100}"));
        assert!(shaper.lacks_all(0, "\u{1161}\u{1100}"));
        assert!(!shaper.lacks_all(0, "\u{1100}\u{1161}"));
        assert!(!shaper.lacks_all(0, ""));
    }

    /// Past the limit the set starts over, but only to take in a character
    /// it does not hold yet.
    #[test]
    fn a_full_set_starts_over_only_for_a_new_character() {
        let config = config::configuration();
        let shaper = HarfbuzzShaper::new(&config, &emoji_last()).unwrap();
        let chars: Vec<char> = ('\u{4e00}'..).take(MISSING_CHARS_LIMIT + 1).collect();
        for &c in &chars[..MISSING_CHARS_LIMIT] {
            shaper.note_missing(0, c);
        }
        shaper.note_missing(0, chars[0]);
        assert_eq!(shaper.missing[0].borrow().len(), MISSING_CHARS_LIMIT);
        shaper.note_missing(0, chars[MISSING_CHARS_LIMIT]);
        assert_eq!(shaper.missing[0].borrow().len(), 1);
    }
}
