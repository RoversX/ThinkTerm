//! Shaping and rasterising with rustybuzz and swash. See Cargo.toml.
//!
//! Fallback is a fixed list: each grapheme cluster goes to the first face
//! that has every one of its characters, runs of the same face are shaped
//! together so ligatures survive, and a cluster no face knows is reported
//! through `no_glyphs` and drawn with the base face's `.notdef`. The
//! desktop resolves fallback through the OS instead; that difference is
//! accepted and documented. So is a smaller one: advances here are the
//! font's design units scaled linearly, where the desktop reads
//! FreeType-hinted advances, so a cell may differ by a fraction of a pixel.

use anyhow::{anyhow, Context, Result};
use finl_unicode::grapheme_clusters::Graphemes;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use swash::scale::{image::Content, Render, ScaleContext, Source, StrikeWith};
use swash::zeno::Format;
use swash::FontRef;
use termwiz::cell::{unicode_column_width, Presentation};
use thinkterm_font_core::units::PixelLength;
use thinkterm_font_core::{
    Direction, FontMetrics, FontRasterizer, FontShaper, GlyphInfo, GlyphInfoParts,
    PresentationWidth, RasterizedGlyph,
};

/// One font file the page shipped.
pub struct Face {
    name: String,
    data: Vec<u8>,
    index: u32,
    presentation: Presentation,
    scaler: RefCell<ScaleContext>,
    /// What the cmap said about a character, once asked.
    ///
    /// `swash()` rebuilds a `FontRef` and re-reads the table directory on
    /// every call, and `face_for` calls it for every grapheme against every
    /// face. A line of Chinese is a *single* cluster -- `CellCluster` only
    /// breaks on attribute changes and whitespace, and Chinese prose has no
    /// whitespace -- so its `ShapeKey` differs on every row and the shape
    /// cache misses on every frame of a scroll. This is what keeps that
    /// miss cheap.
    coverage: RefCell<HashMap<char, bool>>,
}

impl std::fmt::Debug for Face {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Face")
            .field("name", &self.name)
            .field("presentation", &self.presentation)
            .finish()
    }
}

/// Characters that join or qualify their neighbours rather than being drawn
/// themselves: what matters is whether the face can draw what they connect.
///
/// Format characters only. A combining mark like U+0301 is a mark, not a
/// joiner; treating it as one would send every accented letter to the
/// fallback even where the face draws it perfectly well.
fn is_joiner_or_selector(c: char) -> bool {
    matches!(c, '\u{200c}' | '\u{200d}')
        || ('\u{fe00}'..='\u{fe0f}').contains(&c)
        || ('\u{e0100}'..='\u{e01ef}').contains(&c)
}

impl Face {
    pub fn new(name: &str, data: Vec<u8>, index: u32) -> Result<Self> {
        let hb = rustybuzz::Face::from_slice(&data, index)
            .ok_or_else(|| anyhow!("{name}: not a font rustybuzz can read"))?;
        // A face with colour tables is an emoji face; the shaper prefers
        // it for text asked for in emoji presentation, and avoids it for
        // text presentation, the way the desktop classifies its fonts.
        let tables = hb.tables();
        let presentation = if tables.colr.is_some()
            || tables.cbdt.is_some()
            || tables.sbix.is_some()
            || tables.svg.is_some()
        {
            Presentation::Emoji
        } else {
            Presentation::Text
        };
        drop(hb);
        FontRef::from_index(&data, index as usize)
            .ok_or_else(|| anyhow!("{name}: not a font swash can read"))?;
        Ok(Self {
            name: name.to_string(),
            data,
            index,
            presentation,
            scaler: RefCell::new(ScaleContext::new()),
            coverage: RefCell::new(HashMap::new()),
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn presentation(&self) -> Presentation {
        self.presentation
    }

    fn hb(&self) -> rustybuzz::Face<'_> {
        rustybuzz::Face::from_slice(&self.data, self.index).expect("validated in new")
    }

    fn swash(&self) -> FontRef<'_> {
        FontRef::from_index(&self.data, self.index as usize).expect("validated in new")
    }

    pub fn glyph_index(&self, c: char) -> Option<u16> {
        match self.swash().charmap().map(c) {
            0 => None,
            id => Some(id),
        }
    }

    /// Whether this face has a glyph for `c`, remembering the answer.
    fn has(&self, c: char) -> bool {
        if let Some(known) = self.coverage.borrow().get(&c) {
            return *known;
        }
        let known = self.swash().charmap().map(c) != 0;
        let mut coverage = self.coverage.borrow_mut();
        // Nothing else bounds this: it is keyed by character, and a pane can
        // emit any of them. Flushed wholesale rather than by LRU -- the
        // entries cost one cmap lookup to rebuild.
        if coverage.len() > 65536 {
            coverage.clear();
        }
        coverage.insert(c, known);
        known
    }

    /// Whether every character that matters in `cluster` has a glyph here.
    ///
    /// Per character, so it cannot answer whether the face can *join* them:
    /// a ZWJ sequence whose every piece is in the cmap looks covered here
    /// and comes out as several separate glyphs. That is why the gap
    /// decision is finished after shaping, in `FontSet::judge`.
    fn covers(&self, cluster: &str) -> bool {
        let mut any = false;
        for c in cluster.chars() {
            if is_joiner_or_selector(c) {
                continue;
            }
            any = true;
            if !self.has(c) {
                return false;
            }
        }
        any
    }

    /// Points at `dpi` to pixels, the way the desktop sizes every face.
    fn pixels(size: f64, dpi: u32) -> f32 {
        (size * dpi as f64 / 72.0) as f32
    }
}

impl FontRasterizer for Face {
    fn rasterize_glyph(&self, glyph_pos: u32, size: f64, dpi: u32) -> Result<RasterizedGlyph> {
        let px = Self::pixels(size, dpi);
        let font = self.swash();
        let mut ctx = self.scaler.borrow_mut();
        let mut scaler = ctx.builder(font).size(px).hint(true).build();
        let image = Render::new(&[
            Source::ColorOutline(0),
            Source::ColorBitmap(StrikeWith::BestFit),
            Source::Outline,
        ])
        .format(Format::Alpha)
        .render(&mut scaler, glyph_pos as u16)
        .with_context(|| format!("{}: glyph {glyph_pos} did not render", self.name))?;

        let (width, height) = (image.placement.width as usize, image.placement.height as usize);
        let has_color = matches!(image.content, Content::Color);
        // The atlas wants premultiplied RGBA, like the desktop's FreeType path.
        let mut data = Vec::with_capacity(width * height * 4);
        match image.content {
            Content::Mask => {
                for a in &image.data {
                    data.extend_from_slice(&[*a, *a, *a, *a]);
                }
            }
            Content::Color => {
                for px in image.data.chunks(4) {
                    let a = px[3] as u32;
                    data.extend_from_slice(&[
                        (px[0] as u32 * a / 255) as u8,
                        (px[1] as u32 * a / 255) as u8,
                        (px[2] as u32 * a / 255) as u8,
                        px[3],
                    ]);
                }
            }
            Content::SubpixelMask => {
                for px in image.data.chunks(3) {
                    let a = ((px[0] as u32 + px[1] as u32 + px[2] as u32) / 3) as u8;
                    data.extend_from_slice(&[a, a, a, a]);
                }
            }
        }
        Ok(RasterizedGlyph {
            data,
            height,
            width,
            bearing_x: PixelLength::new(image.placement.left as f64),
            bearing_y: PixelLength::new(image.placement.top as f64),
            has_color,
            // swash always scales outlines and picks the best strike for
            // bitmaps, so the bitmap is at the requested size.
            is_scaled: true,
        })
    }
}

/// The fallback list, in order. Face 0 is the base font whose metrics
/// define the cell.
pub struct FontSet {
    faces: Vec<Face>,
    metrics: RefCell<HashMap<(usize, u64, u32), FontMetrics>>,
}

impl FontSet {
    pub fn new(faces: Vec<Face>) -> Result<Self> {
        if faces.is_empty() {
            anyhow::bail!("a font set needs at least one face");
        }
        Ok(Self {
            faces,
            metrics: RefCell::new(HashMap::new()),
        })
    }

    pub fn faces(&self) -> &[Face] {
        &self.faces
    }

    pub fn face(&self, idx: usize) -> Result<&Face> {
        self.faces
            .get(idx)
            .ok_or_else(|| anyhow!("no face with index {idx}"))
    }

    /// The face to shape `cluster` with: one matching the wanted
    /// presentation first, then any that covers it.
    fn face_for(&self, cluster: &str, presentation: Option<Presentation>) -> Option<usize> {
        if let Some(wanted) = presentation {
            if let Some(idx) = self
                .faces
                .iter()
                .position(|f| f.presentation == wanted && f.covers(cluster))
            {
                return Some(idx);
            }
        }
        self.faces.iter().position(|f| f.covers(cluster))
    }

    fn shape_run(
        &self,
        face_idx: usize,
        text: &str,
        run: Range<usize>,
        px: f32,
        direction: Direction,
        presentation_width: Option<&PresentationWidth>,
    ) -> Vec<GlyphInfo> {
        let face = &self.faces[face_idx];
        let hb = face.hb();
        let scale = px / hb.units_per_em() as f32;

        let mut buffer = rustybuzz::UnicodeBuffer::new();
        buffer.push_str(&text[run.clone()]);
        buffer.set_direction(match direction {
            Direction::LeftToRight => rustybuzz::Direction::LeftToRight,
            Direction::RightToLeft => rustybuzz::Direction::RightToLeft,
        });
        buffer.set_cluster_level(rustybuzz::BufferClusterLevel::MonotoneGraphemes);
        buffer.guess_segment_properties();
        let shaped = rustybuzz::shape(&hb, &[], buffer);
        let infos = shaped.glyph_infos();
        let positions = shaped.glyph_positions();

        // Cluster values are byte offsets into the run. Their sorted, unique
        // set gives each cluster's byte length; the cell width of that byte
        // range is what the terminal knows about the text, and the glyphs
        // that share a cluster split it in proportion to their advances,
        // exactly as the desktop's HarfBuzz shaper does.
        let mut starts: Vec<u32> = infos.iter().map(|i| i.cluster).collect();
        starts.sort_unstable();
        starts.dedup();
        // The shaper may split one grapheme's cell into two clusters; the
        // desktop's ClusterResolver folds those back by cell index so the
        // cell is not charged twice. Same here.
        if let Some(pw) = presentation_width {
            let mut folded: Vec<u32> = Vec::with_capacity(starts.len());
            for start in starts {
                let cell = pw.byte_to_cell_idx(run.start + start as usize);
                match folded.last() {
                    Some(prev) if pw.byte_to_cell_idx(run.start + *prev as usize) == cell => {}
                    _ => folded.push(start),
                }
            }
            starts = folded;
        }
        let run_len = run.len() as u32;
        // The kept start a glyph's cluster falls under.
        let owner = |cluster: u32| -> u32 {
            match starts.binary_search(&cluster) {
                Ok(pos) => starts[pos],
                Err(pos) => starts[pos.saturating_sub(1)],
            }
        };
        let byte_len = |start: u32| -> u32 {
            let pos = starts.binary_search(&start).expect("start came from starts");
            starts.get(pos + 1).copied().unwrap_or(run_len) - start
        };

        let mut out = Vec::with_capacity(infos.len());
        let mut i = 0;
        while i < infos.len() {
            let cluster = owner(infos[i].cluster);
            let mut j = i;
            while j < infos.len() && owner(infos[j].cluster) == cluster {
                j += 1;
            }
            let start_abs = run.start + cluster as usize;
            let len = byte_len(cluster) as usize;
            let substr = &text[start_abs..start_abs + len];
            let cell_width = presentation_width
                .map(|pw| pw.num_cells(start_abs..start_abs + len))
                .unwrap_or_else(|| unicode_column_width(substr, None) as u8);
            let total_width: f64 = positions[i..j].iter().map(|p| p.x_advance as f64).sum();
            let mut remaining = cell_width;
            for k in i..j {
                let (info, pos) = (&infos[k], &positions[k]);
                let weighted = if total_width == 0.0 {
                    1
                } else {
                    (cell_width as f64 * pos.x_advance as f64 / total_width).ceil() as u8
                };
                let weighted = weighted.min(remaining);
                remaining = remaining.saturating_sub(weighted);
                out.push(GlyphInfo::new(
                    substr,
                    GlyphInfoParts {
                        only_char: {
                            let mut chars = substr.chars();
                            match (chars.next(), chars.next()) {
                                (Some(c), None) => Some(c),
                                _ => None,
                            }
                        },
                        is_space: substr == " ",
                        num_cells: weighted,
                        cluster: start_abs as u32,
                        font_idx: face_idx,
                        glyph_pos: info.glyph_id,
                        x_advance: PixelLength::new((pos.x_advance as f32 * scale) as f64),
                        y_advance: PixelLength::new((pos.y_advance as f32 * scale) as f64),
                        x_offset: PixelLength::new((pos.x_offset as f32 * scale) as f64),
                        y_offset: PixelLength::new((pos.y_offset as f32 * scale) as f64),
                    },
                ));
            }
            i = j;
        }
        out
    }
}

/// Why a grapheme has to be drawn with the platform's own fonts.
///
/// The order is part of the design. `.notdef` outranks the rest because it
/// is the box the user is looking at; colour outranks joining because
/// `\u{1f468}\u{200d}\u{1f4bb}` asked for emoji presentation is a coverage
/// problem, not a joining one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GapReason {
    /// No bundled face has all of its characters.
    Uncovered,
    /// A face took it and the shaper still produced `.notdef`.
    Notdef,
    /// Emoji presentation was asked for and the face that answered has no
    /// colour tables, so all it can draw is a monochrome stand-in.
    NoColor,
    /// The face has the pieces but not the join: a ZWJ sequence came out
    /// as several drawn glyphs.
    Unjoined,
}

/// A grapheme the bundled faces cannot draw, handed to the caller whole.
///
/// The browser client draws these with the platform's own fonts and keeps
/// the result in its atlas. Everything the caller needs to do that is here,
/// and in particular the text is the **whole grapheme**: splitting it would
/// break combining marks, variation selectors and ZWJ sequences.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gap {
    /// The grapheme, exactly as it appeared.
    pub text: String,
    /// Where it sits in the text that was shaped, in bytes. The same basis
    /// as `GlyphInfo::cluster`.
    pub bytes: Range<usize>,
    /// How many terminal columns it occupies. Taken from the row's own
    /// width data, never re-derived: the fallback has to land on the grid
    /// the terminal already decided on. In particular it is *not* the sum
    /// of the stand-in glyphs' cells, which `shape_run` apportions by
    /// advance and which does not add up to this when the advances are 0.
    pub cells: u8,
    /// Which presentation was asked for, so the caller can tell a text
    /// glyph it may tint from an emoji that carries its own colour.
    pub presentation: Option<Presentation>,
    pub reason: GapReason,
}

/// What a shaped glyph is for.
///
/// This replaces "ask for the indices, then delete them": the set is
/// computed once, the shared `WebShaped` is never mutated, and "exactly
/// this group, no more and no fewer" becomes a property of the vector
/// rather than of every caller's loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GlyphRole {
    /// Nothing to do with a gap. Draw it.
    Own,
    /// Where a gap's fallback glyph goes, indexing `gaps`.
    GapLead(u16),
    /// Another of the same gap's stand-ins. Drop it once the fallback is
    /// drawn; keeping it draws the cell twice and advances the column
    /// twice.
    GapTail(u16),
}

/// `shape` plus what it could not do.
#[derive(Debug, Clone)]
pub struct WebShaped {
    pub infos: Vec<GlyphInfo>,
    /// Invariant: each of these has exactly one `GapLead` in `roles`.
    pub gaps: Vec<Gap>,
    /// Exactly `infos.len()` entries.
    pub roles: Vec<GlyphRole>,
}

/// The gap whose byte range holds `at`, if any.
///
/// Gaps come from one forward walk over the graphemes, so they are in
/// ascending order and do not overlap: the only candidate is the last one
/// that starts at or before `at`.
fn gap_at(gaps: &[Gap], at: usize) -> Option<usize> {
    let g = match gaps.binary_search_by(|gap| gap.bytes.start.cmp(&at)) {
        Ok(g) => g,
        Err(0) => return None,
        Err(pos) => pos - 1,
    };
    (at < gaps[g].bytes.end).then_some(g)
}

impl WebShaped {
    fn new(infos: Vec<GlyphInfo>, gaps: Vec<Gap>) -> Self {
        // A gap no glyph stands in for has no column to take over, so
        // drawing a fallback for it would add one. `shape_web` folds
        // graphemes onto shaped clusters before it gets here, so this is a
        // backstop rather than the mechanism it once was -- what remains is
        // a grapheme that produced no glyphs at all. Dropped here, so
        // `gaps` and `roles` cannot disagree about how many there are.
        let mut stood_in = vec![false; gaps.len()];
        for info in &infos {
            if let Some(g) = gap_at(&gaps, info.cluster as usize) {
                stood_in[g] = true;
            }
        }
        let gaps: Vec<Gap> = gaps
            .into_iter()
            .zip(&stood_in)
            .filter(|(_, kept)| **kept)
            .map(|(gap, _)| gap)
            .collect();

        debug_assert!(gaps.len() <= u16::MAX as usize, "more gaps than a role can index");
        let mut roles = vec![GlyphRole::Own; infos.len()];
        let mut has_lead = vec![false; gaps.len()];
        for (i, info) in infos.iter().enumerate() {
            if let Some(g) = gap_at(&gaps, info.cluster as usize) {
                roles[i] = if std::mem::replace(&mut has_lead[g], true) {
                    GlyphRole::GapTail(g as u16)
                } else {
                    GlyphRole::GapLead(g as u16)
                };
            }
        }
        Self { infos, gaps, roles }
    }

    /// The glyphs that stand in for a gap: every one whose cluster falls in
    /// the gap's byte range.
    ///
    /// These are **not** all `.notdef`. rustybuzz maps a ZWJ to the face's
    /// space glyph rather than deleting it, so a joined sequence puts a
    /// glyph with a real id in here, and a base the face does cover puts
    /// its own glyph in here too. Narrowing the removal to `glyph_pos == 0`
    /// would leave those to be drawn on top of the fallback.
    ///
    /// Production code reads `roles`, which says the same thing without
    /// scanning or allocating; this is the contract in readable form, and
    /// the tests hold the two to each other.
    pub fn glyphs_for(&self, gap: &Gap) -> Vec<usize> {
        self.infos
            .iter()
            .enumerate()
            .filter(|(_, info)| gap.bytes.contains(&(info.cluster as usize)))
            .map(|(i, _)| i)
            .collect()
    }
}

impl FontSet {
    /// Shape, and report the graphemes no bundled face can draw.
    ///
    /// The `FontShaper` trait reports missing characters as a flat
    /// `Vec<char>`, which the desktop uses to log them. That is not enough
    /// to draw a replacement: the grapheme's boundaries, its place in the
    /// text and its column count are all gone by then. This is the same
    /// walk, keeping them.
    ///
    /// `keeps_notdef` names the graphemes the caller draws itself and would
    /// rather have the box for than a substitute. The browser passes
    /// Braille that way -- it draws the dots -- and the filter is here
    /// rather than applied to the result because a screenful of them would
    /// otherwise allocate a `String` per cell. The desktop filters at the
    /// same point, before it goes looking for a fallback font.
    #[allow(clippy::too_many_arguments)]
    pub fn shape_web(
        &self,
        text: &str,
        size: f64,
        dpi: u32,
        presentation: Option<Presentation>,
        direction: Direction,
        range: Option<Range<usize>>,
        presentation_width: Option<&PresentationWidth>,
        keeps_notdef: &dyn Fn(&str) -> bool,
    ) -> Result<WebShaped> {
        let range = range.unwrap_or(0..text.len());
        let px = Face::pixels(size, dpi);
        let mut runs: Vec<(usize, Range<usize>)> = Vec::new();
        // Every grapheme and the face that took it. Which of them are gaps
        // cannot be decided yet: a face can take a cluster and still fail
        // to draw it.
        let mut candidates: Vec<(Range<usize>, Option<usize>)> = Vec::new();
        let mut byte = range.start;
        for grapheme in Graphemes::new(&text[range.clone()]) {
            let end = byte + grapheme.len();
            let taken = self.face_for(grapheme, presentation);
            candidates.push((byte..end, taken));
            // A grapheme no face covers is still shaped with the base face:
            // its `.notdef` holds the run's positions together, and `roles`
            // says which glyphs a fallback replaces.
            let face = taken.unwrap_or(0);
            match runs.last_mut() {
                Some((f, r)) if *f == face && r.end == byte => r.end = end,
                _ => runs.push((face, byte..end)),
            }
            byte = end;
        }
        let mut infos = Vec::new();
        for (face, run) in runs {
            infos.extend(self.shape_run(face, text, run, px, direction, presentation_width));
        }

        // rustybuzz decides cluster boundaries by its own rules, and they
        // are coarser than UAX#29 in several scripts: Burmese, Tai Tham and
        // Cham all put a base and its vowel signs in one cluster where the
        // walk above sees two or three graphemes.
        //
        // A grapheme inside a merged cluster has no glyph that starts at it.
        // Judged on its own it would be measured against its neighbour's
        // glyphs, and then dropped for having no stand-in -- taking its own
        // glyphs with it, as `GapTail`s of the gap before it. The character
        // would not fall back to a box; it would disappear. So fold them:
        // one shaped cluster is one unit, whole.
        let starts: HashSet<usize> = infos.iter().map(|g| g.cluster as usize).collect();
        let mut units: Vec<(Range<usize>, Option<usize>, bool)> =
            Vec::with_capacity(candidates.len());
        for (bytes, taken) in candidates {
            match units.last_mut() {
                Some((range, face, folded)) if !starts.contains(&bytes.start) => {
                    range.end = bytes.end;
                    *folded = true;
                    // One uncovered part makes the whole cluster the
                    // platform's problem: they share glyphs, so there is no
                    // drawing one without the other.
                    if taken.is_none() {
                        *face = None;
                    }
                }
                _ => units.push((bytes, taken, false)),
            }
        }
        // Every unit now begins at a cluster boundary and ends where the
        // next one begins, so the glyphs whose cluster falls in its range
        // are exactly its own -- which is what `judge` and `WebShaped::new`
        // both rely on.

        // Walk the glyphs in logical byte order alongside the units. A
        // missing glyph must not make every covered unit scan the entire
        // line. Sort references, leaving the drawing order in `infos`
        // intact: RTL runs can have descending cluster offsets, and a
        // change of face can start another run in either direction.
        let mut by_cluster: Vec<_> = infos.iter().collect();
        by_cluster.sort_unstable_by_key(|g| g.cluster);
        let mut remaining = by_cluster.as_slice();
        let mut gaps = Vec::new();
        for (bytes, taken, folded) in &units {
            let count = remaining
                .iter()
                .take_while(|g| (g.cluster as usize) < bytes.end)
                .count();
            let (mine, rest) = remaining.split_at(count);
            remaining = rest;
            let grapheme = &text[bytes.clone()];
            if keeps_notdef(grapheme) {
                continue;
            }
            let reason = match taken {
                // Nobody covered it, so it was shaped with the base face
                // and those glyphs are that face's `.notdef`. Not looking
                // at them is what keeps a screenful of CJK linear.
                None => Some(GapReason::Uncovered),
                Some(idx) => {
                    self.judge(*idx, grapheme, mine, presentation, *folded)
                }
            };
            let Some(reason) = reason else { continue };
            gaps.push(Gap {
                text: grapheme.to_string(),
                bytes: bytes.clone(),
                // The row's own width data. Never reconstructed from the
                // shaped glyphs: `shape_run` splits a cluster's columns
                // between them in proportion to their advances, and gives
                // each one column when the advances are all zero, so their
                // sum is not this number.
                cells: presentation_width
                    .map(|pw| pw.num_cells(bytes.clone()))
                    .unwrap_or_else(|| unicode_column_width(grapheme, None) as u8),
                presentation,
                reason,
            });
        }
        Ok(WebShaped::new(infos, gaps))
    }

    /// Why a grapheme a face *did* take still needs the platform's fonts,
    /// if it does.
    fn judge(
        &self,
        face_idx: usize,
        grapheme: &str,
        infos: &[&GlyphInfo],
        presentation: Option<Presentation>,
        folded: bool,
    ) -> Option<GapReason> {
        let joined = grapheme.contains('\u{200d}');
        // The slice contains every cluster in this unit and no neighbour's
        // glyphs, including when one Hangul grapheme spans several clusters.
        if infos.iter().any(|g| g.glyph_pos == 0) {
            return Some(GapReason::Notdef);
        }
        // The terminal asked for emoji and the face that answered has no
        // colour tables. Bundling no emoji face at all is the usual case,
        // so this is the rule that puts emoji on the screen.
        if presentation == Some(Presentation::Emoji)
            && self.faces[face_idx].presentation != Presentation::Emoji
        {
            return Some(GapReason::NoColor);
        }
        // A joined sequence the face has the pieces of but cannot join
        // comes out as more than one drawn glyph.
        //
        // Counted by advance rather than by "is not .notdef": rustybuzz
        // does not delete default ignorables, it maps them to the face's
        // space glyph and zeroes the advance, so a ZWJ is a glyph with a
        // real id sitting in this range. A variation selector never
        // reaches here -- what it asks is a question about presentation,
        // which the rule above already answered.
        // Not asked of a folded unit. That already spans more than one
        // grapheme, so several drawn glyphs is what it looks like when
        // everything is fine, and the count would condemn it on sight.
        if joined && !folded && infos.iter().filter(|g| g.x_advance.get() != 0.0).count() != 1 {
            return Some(GapReason::Unjoined);
        }
        None
    }
}

impl FontShaper for FontSet {
    #[allow(clippy::too_many_arguments)]
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
    ) -> Result<Vec<GlyphInfo>> {
        // Nothing is drawn by this caller, so no grapheme keeps its box.
        let shaped = self.shape_web(
            text,
            size,
            dpi,
            presentation,
            direction,
            range,
            presentation_width,
            &|_| false,
        )?;
        // The trait reports characters, not graphemes, and drops the ones
        // that only join others. It now also reports graphemes a face took
        // and could not draw, which it could not see before; the desktop
        // uses this only to log, and has its own shaper besides.
        no_glyphs.extend(
            shaped
                .gaps
                .iter()
                .flat_map(|gap| gap.text.chars())
                .filter(|c| !is_joiner_or_selector(*c)),
        );
        Ok(shaped.infos)
    }

    fn metrics(&self, size: f64, dpi: u32) -> Result<FontMetrics> {
        self.metrics_for_idx(0, size, dpi)
    }

    fn metrics_for_idx(&self, font_idx: usize, size: f64, dpi: u32) -> Result<FontMetrics> {
        let key = (font_idx, size.to_bits(), dpi);
        if let Some(m) = self.metrics.borrow().get(&key) {
            return Ok(*m);
        }
        let face = self.face(font_idx)?;
        let px = Face::pixels(size, dpi);
        let font = face.swash();
        let m = font.metrics(&[]).scale(px);
        let glyphs = font.glyph_metrics(&[]).scale(px);
        let charmap = font.charmap();

        // The desktop's FreeType path: the cell is as wide as the widest
        // ASCII glyph and as tall as the font's line spacing.
        let mut width = 0f32;
        for c in 32u8..128 {
            let id = charmap.map(c as char);
            if id != 0 {
                width = width.max(glyphs.advance_width(id));
            }
        }
        if width == 0.0 {
            for id in 1..8u16 {
                width = width.max(glyphs.advance_width(id));
            }
        }
        let height = m.ascent + m.descent + m.leading;
        if width == 0.0 {
            width = height;
        }
        let metrics = FontMetrics {
            cell_width: PixelLength::new(width as f64),
            cell_height: PixelLength::new(height as f64),
            descender: PixelLength::new(-(m.descent as f64)),
            underline_thickness: PixelLength::new(m.stroke_size.max(1.0) as f64),
            underline_position: PixelLength::new(m.underline_offset as f64),
            cap_height_ratio: (m.cap_height > 0.0).then(|| (m.cap_height / height) as f64),
            cap_height: (m.cap_height > 0.0).then(|| PixelLength::new(m.cap_height as f64)),
            is_scaled: true,
            presentation: face.presentation,
            force_y_adjust: PixelLength::new(0.0),
        };
        self.metrics.borrow_mut().insert(key, metrics);
        Ok(metrics)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jetbrains() -> FontSet {
        let data = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../assets/fonts/JetBrainsMono-Regular.ttf"
        ))
        .unwrap();
        FontSet::new(vec![Face::new("JetBrains Mono", data, 0).unwrap()]).unwrap()
    }

    fn shape(set: &FontSet, text: &str, presentation: Option<Presentation>) -> WebShaped {
        set.shape_web(
            text,
            12.0,
            96,
            presentation,
            Direction::LeftToRight,
            None,
            None,
            &|_| false,
        )
        .unwrap()
    }

    /// One glyph, placed by hand, for the tests that exercise `WebShaped`
    /// rather than the shaper.
    fn glyph_at(cluster: u32) -> GlyphInfo {
        GlyphInfo::new(
            "x",
            GlyphInfoParts {
                only_char: Some('x'),
                is_space: false,
                num_cells: 1,
                cluster,
                font_idx: 0,
                glyph_pos: 1,
                x_advance: PixelLength::new(1.0),
                y_advance: PixelLength::new(0.0),
                x_offset: PixelLength::new(0.0),
                y_offset: PixelLength::new(0.0),
            },
        )
    }

    fn gap_over(bytes: Range<usize>) -> Gap {
        Gap {
            text: "?".to_string(),
            bytes,
            cells: 1,
            presentation: None,
            reason: GapReason::Uncovered,
        }
    }

    /// The bundled faces cover no CJK, so these are the real thing rather
    /// than a contrived gap.
    #[test]
    fn a_grapheme_no_face_covers_comes_back_whole() {
        let set = jetbrains();
        let shaped = shape(&set, "a\u{4e2d}b", None);
        assert_eq!(shaped.gaps.len(), 1, "{:?}", shaped.gaps);
        let gap = &shaped.gaps[0];
        assert_eq!(gap.text, "\u{4e2d}");
        assert_eq!(gap.bytes, 1..4, "the byte range of the grapheme itself");
        assert_eq!(gap.cells, 2, "a wide character keeps its two columns");
        assert_eq!(gap.reason, GapReason::Uncovered);
    }

    #[test]
    fn a_zwj_sequence_is_one_gap_and_is_not_split() {
        let set = jetbrains();
        let family = "\u{1f468}\u{200d}\u{1f4bb}"; // man + ZWJ + laptop
        let shaped = shape(&set, family, None);
        assert_eq!(shaped.gaps.len(), 1, "the sequence must not be torn apart");
        assert_eq!(shaped.gaps[0].text, family);
        assert_eq!(shaped.gaps[0].bytes, 0..family.len());
    }

    #[test]
    fn a_combining_mark_stays_with_its_base() {
        let set = jetbrains();
        // A base no face covers plus a mark: still one grapheme, so the
        // fallback gets both or neither. The base has to be one the face
        // cannot draw -- with a covered base there is no gap at all, and
        // the assertion would run zero times.
        let text = "\u{4e2d}\u{0301}";
        assert_eq!(Graphemes::new(text).count(), 1, "premise: one grapheme");
        let shaped = shape(&set, text, None);
        assert_eq!(shaped.gaps.len(), 1, "{:?}", shaped.gaps);
        assert_eq!(shaped.gaps[0].text, text, "a mark was separated from its base");
    }

    #[test]
    fn the_glyphs_a_fallback_replaces_are_exactly_the_gap_s_own() {
        let set = jetbrains();
        let shaped = shape(&set, "ab\u{4e2d}de", None);
        let gap = &shaped.gaps[0];
        let idx = shaped.glyphs_for(gap);
        assert_eq!(idx.len(), 1, "one .notdef stands in for one gap: {idx:?}");
        assert_eq!(shaped.infos[idx[0]].glyph_pos, 0, "notdef from the base face");
        // And nothing belonging to the covered text is caught up in it.
        for (i, info) in shaped.infos.iter().enumerate() {
            if !idx.contains(&i) {
                assert!(!gap.bytes.contains(&(info.cluster as usize)));
            }
        }
    }

    #[test]
    fn a_face_with_the_pieces_but_not_the_join_is_still_a_gap() {
        let set = jetbrains();
        let face = set.face(0).unwrap();
        // The premise, asserted rather than assumed: the face has both
        // pieces and not the joiner, which is exactly the hole `covers`
        // cannot see.
        assert!(face.has('\u{26a0}') && face.has('\u{2194}'), "premise: pieces present");
        assert!(!face.has('\u{200d}'), "premise: no ZWJ glyph");
        let text = "\u{26a0}\u{200d}\u{2194}";
        assert_eq!(Graphemes::new(text).count(), 1, "premise: one grapheme");
        let shaped = shape(&set, text, None);
        assert_eq!(shaped.gaps.len(), 1, "{:?}", shaped.gaps);
        assert_eq!(shaped.gaps[0].reason, GapReason::Unjoined);
    }

    #[test]
    fn an_emoji_cluster_a_text_face_took_is_a_gap() {
        let set = jetbrains();
        assert!(set.face(0).unwrap().has('\u{26a1}'), "premise: the face has it");
        let asked = shape(&set, "\u{26a1}", Some(Presentation::Emoji));
        assert_eq!(asked.gaps.len(), 1, "{:?}", asked.gaps);
        assert_eq!(asked.gaps[0].reason, GapReason::NoColor);
        // The same character as text is drawn by the face, as it always was.
        assert!(shape(&set, "\u{26a1}", Some(Presentation::Text)).gaps.is_empty());
    }

    #[test]
    fn a_variation_selector_the_face_honours_is_not_a_gap() {
        // U+26A0 U+FE0E asks for the text presentation the face already
        // draws. Writing the joining rule as a bare glyph count would call
        // this a gap: rustybuzz keeps the selector as a zero-advance space
        // glyph, so the cluster has two glyphs and one of them is drawn.
        let set = jetbrains();
        let text = "\u{26a0}\u{fe0e}";
        assert_eq!(Graphemes::new(text).count(), 1, "premise: one grapheme");
        assert!(shape(&set, text, Some(Presentation::Text)).gaps.is_empty());
    }

    #[test]
    fn an_uncovered_grapheme_is_uncovered_whatever_else_is_wrong_with_it() {
        // Asked for as emoji, and no face has it at all. The reason has to
        // be the one that says "nobody could draw this", not the one about
        // colour: the caller keys its cache on the grapheme either way, but
        // the reason is what a person reads when this goes wrong.
        let set = jetbrains();
        let shaped = shape(&set, "\u{4e2d}", Some(Presentation::Emoji));
        assert_eq!(shaped.gaps.len(), 1);
        assert_eq!(shaped.gaps[0].reason, GapReason::Uncovered);
    }

    #[test]
    fn a_zero_width_non_joiner_does_not_send_a_covered_letter_to_the_canvas() {
        // U+200C is in no bundled face. Before it was treated as a joiner,
        // the whole grapheme was reported missing even though the face
        // draws the `a` perfectly well.
        let set = jetbrains();
        assert!(!set.face(0).unwrap().has('\u{200c}'), "premise: no ZWNJ glyph");
        assert!(shape(&set, "a\u{200c}", None).gaps.is_empty());
    }

    #[test]
    fn roles_and_glyphs_for_agree() {
        let set = jetbrains();
        let shaped = shape(&set, "a\u{4e2d}b\u{d55c}c", None);
        assert_eq!(shaped.roles.len(), shaped.infos.len());
        assert!(shaped.gaps.len() >= 2, "{:?}", shaped.gaps);
        for (g, gap) in shaped.gaps.iter().enumerate() {
            let by_role: Vec<usize> = shaped
                .roles
                .iter()
                .enumerate()
                .filter(|(_, role)| {
                    matches!(role, GlyphRole::GapLead(n) | GlyphRole::GapTail(n) if *n as usize == g)
                })
                .map(|(i, _)| i)
                .collect();
            assert_eq!(by_role, shaped.glyphs_for(gap), "gap {g}");
        }
    }

    #[test]
    fn every_gap_has_exactly_one_lead() {
        let set = jetbrains();
        let shaped = shape(&set, "\u{4e2d}\u{6587}\u{d55c}", None);
        assert!(!shaped.gaps.is_empty());
        for g in 0..shaped.gaps.len() {
            let leads = shaped
                .roles
                .iter()
                .filter(|role| matches!(role, GlyphRole::GapLead(n) if *n as usize == g))
                .count();
            assert_eq!(leads, 1, "gap {g} has {leads} leads");
        }
    }

    #[test]
    fn a_gap_with_no_stand_in_is_dropped() {
        // No glyph falls in the gap's byte range, so there is no column for
        // a fallback to take over: drawing one would add a column. The gap
        // has to be gone, not merely unreachable, or `gaps` and `roles`
        // disagree about how many fallbacks there are.
        let orphan = WebShaped::new(vec![], vec![gap_over(0..3)]);
        assert!(orphan.gaps.is_empty());
        let beside = WebShaped::new(vec![glyph_at(9)], vec![gap_over(0..3)]);
        assert!(beside.gaps.is_empty());
        assert_eq!(beside.roles, vec![GlyphRole::Own]);
        // And one that does have a stand-in survives, so the test above is
        // not passing for want of ever keeping anything.
        let kept = WebShaped::new(vec![glyph_at(1)], vec![gap_over(0..3)]);
        assert_eq!(kept.gaps.len(), 1);
        assert_eq!(kept.roles, vec![GlyphRole::GapLead(0)]);
    }

    #[test]
    fn nothing_a_face_cannot_draw_is_lost() {
        // rustybuzz clusters a Burmese base with its vowel signs; UAX#29
        // makes them separate graphemes. Before the two were folded onto
        // cluster boundaries, the vowel signs were dropped for having no
        // stand-in glyph and took their own glyphs with them -- they did not
        // fall back to a box, they vanished.
        let set = jetbrains();
        for text in [
            "\u{1005}\u{102c}\u{1038}",     // Burmese, one HB cluster
            "a\u{200d}\u{1f600}",           // letter, ZWJ, emoji
            "\u{4e2d}\u{6587}",             // two clusters of their own
            "\u{d55c}\u{ad6d}",             // Hangul: several clusters each
        ] {
            let shaped = shape(&set, text, None);
            let mut accounted = 0;
            for gap in &shaped.gaps {
                assert_eq!(gap.text, &text[gap.bytes.clone()], "{text:?}: {gap:?}");
                accounted += gap.bytes.len();
            }
            assert_eq!(
                accounted,
                text.len(),
                "{text:?}: {accounted} of {} bytes are in a gap; the rest would be drawn by \
                 nothing at all",
                text.len()
            );
        }
    }

    #[test]
    fn every_gap_owns_at_least_one_glyph_and_they_do_not_overlap() {
        let set = jetbrains();
        for text in ["a\u{4e2d}b\u{d55c}c", "\u{1005}\u{102c}\u{1038}", "e\u{301}\u{4e2d}\u{301}"] {
            let shaped = shape(&set, text, None);
            let mut end = 0;
            for gap in &shaped.gaps {
                assert!(gap.bytes.start >= end, "{text:?}: gaps overlap or run backwards");
                end = gap.bytes.end;
                assert!(!shaped.glyphs_for(gap).is_empty(), "{text:?}: {gap:?} has no stand-in");
            }
        }
    }

    #[test]
    fn a_letter_the_face_draws_is_not_condemned_by_its_neighbour() {
        // The `.notdef` belongs to the CJK character. Sweeping it into the
        // letter's own judgement sent a perfectly drawable `a` to the canvas.
        let set = jetbrains();
        let shaped = shape(&set, "a\u{4e2d}", None);
        assert_eq!(shaped.gaps.len(), 1, "{:?}", shaped.gaps);
        assert_eq!(shaped.gaps[0].text, "\u{4e2d}");
    }

    #[test]
    fn mixed_run_gap_judgments_preserve_direction_and_subrange() {
        let set = jetbrains();
        let joined = "\u{26a0}\u{200d}\u{2194}";
        let middle = format!("{}中{}{}", "x".repeat(512), joined, "y".repeat(512));
        // The missing characters outside the requested range must not be
        // included, and range-relative offsets must not pick up neighbours.
        let text = format!("韓{middle}文");
        let start = "韓".len();
        let end = start + middle.len();
        for direction in [Direction::LeftToRight, Direction::RightToLeft] {
            let shaped = set
                .shape_web(
                    &text, 12.0, 96, None, direction, Some(start..end), None, &|_| false,
                )
                .unwrap();
            assert_eq!(shaped.gaps.len(), 2, "{direction:?}: {:?}", shaped.gaps);
            assert_eq!(shaped.gaps[0].text, "中");
            assert_eq!(shaped.gaps[0].reason, GapReason::Uncovered);
            assert_eq!(shaped.gaps[0].bytes, start + 512..start + 515);
            assert_eq!(shaped.gaps[1].text, joined);
            assert_eq!(shaped.gaps[1].reason, GapReason::Unjoined);
            // Grouping for judgment must not reorder the actual glyphs.
            assert!(shaped.infos.windows(2).all(|pair| match direction {
                Direction::LeftToRight => pair[0].cluster <= pair[1].cluster,
                Direction::RightToLeft => pair[0].cluster >= pair[1].cluster,
            }));
        }
    }

    #[test]
    fn the_second_stand_in_of_a_gap_is_a_tail() {
        let both = WebShaped::new(vec![glyph_at(0), glyph_at(2)], vec![gap_over(0..3)]);
        assert_eq!(both.roles, vec![GlyphRole::GapLead(0), GlyphRole::GapTail(0)]);
    }

    #[test]
    fn the_trait_still_reports_the_characters_it_always_did() {
        let set = jetbrains();
        let mut missing = vec![];
        set.shape("a中\u{1f468}\u{200d}\u{1f4bb}", 12.0, 96, &mut missing, None, Direction::LeftToRight, None, None)
            .unwrap();
        // Joiners are still dropped, and the covered "a" is not reported.
        assert_eq!(missing, vec!['中', '\u{1f468}', '\u{1f4bb}'], "{missing:?}");
    }

    #[test]
    fn metrics_describe_a_monospace_cell() {
        let set = jetbrains();
        let m = set.metrics(12.0, 96).unwrap();
        let px = 16.0;
        assert!(m.cell_width.get() > px * 0.5 && m.cell_width.get() < px * 0.7, "{m:?}");
        assert!(m.cell_height.get() > px && m.cell_height.get() < px * 1.6, "{m:?}");
        assert!(m.descender.get() < 0.0, "{m:?}");
        assert!(m.underline_thickness.get() >= 1.0);
        assert_eq!(m.presentation, Presentation::Text);
    }

    #[test]
    fn plain_text_shapes_one_glyph_per_cell() {
        let set = jetbrains();
        let mut missing = vec![];
        let glyphs = set
            .shape("abc", 12.0, 96, &mut missing, None, Direction::LeftToRight, None, None)
            .unwrap();
        assert_eq!(glyphs.len(), 3);
        assert_eq!(glyphs.iter().map(|g| g.num_cells).collect::<Vec<_>>(), [1, 1, 1]);
        assert_eq!(glyphs.iter().map(|g| g.cluster).collect::<Vec<_>>(), [0, 1, 2]);
        assert!(glyphs.iter().all(|g| g.glyph_pos != 0 && g.font_idx == 0));
        assert!(missing.is_empty());
        let m = set.metrics(12.0, 96).unwrap();
        assert!((glyphs[0].x_advance.get() - m.cell_width.get()).abs() < 0.01);
    }

    #[test]
    fn a_ligature_keeps_the_cells_of_the_text_it_replaced() {
        // JetBrains Mono builds its ligatures from per-character pieces
        // (contextual alternates), so `==>` stays three glyphs in three
        // cells; what proves the ligature formed is that they are not the
        // glyphs `=` and `>` shape to on their own.
        let set = jetbrains();
        let mut missing = vec![];
        let alone = set
            .shape("=", 12.0, 96, &mut missing, None, Direction::LeftToRight, None, None)
            .unwrap()[0]
            .glyph_pos;
        let glyphs = set
            .shape("==>", 12.0, 96, &mut missing, None, Direction::LeftToRight, None, None)
            .unwrap();
        let cells: u32 = glyphs.iter().map(|g| g.num_cells as u32).sum();
        assert_eq!(cells, 3, "{glyphs:?}");
        assert_eq!(glyphs.len(), 3);
        assert_ne!(glyphs[0].glyph_pos, alone, "the ligature did not form: {glyphs:?}");
    }

    #[test]
    fn a_combining_mark_stays_in_its_cell() {
        let set = jetbrains();
        let mut missing = vec![];
        let glyphs = set
            .shape("e\u{301}x", 12.0, 96, &mut missing, None, Direction::LeftToRight, None, None)
            .unwrap();
        let cells: u32 = glyphs.iter().map(|g| g.num_cells as u32).sum();
        assert_eq!(cells, 2, "{glyphs:?}");
        assert!(missing.is_empty());
    }

    #[test]
    fn text_no_face_covers_is_reported_and_drawn_as_notdef() {
        let set = jetbrains();
        let mut missing = vec![];
        let glyphs = set
            .shape("a\u{4e2d}", 12.0, 96, &mut missing, None, Direction::LeftToRight, None, None)
            .unwrap();
        assert_eq!(missing, vec!['\u{4e2d}']);
        let cjk = glyphs.iter().find(|g| g.cluster == 1).expect("the CJK cluster");
        assert_eq!(cjk.glyph_pos, 0, "notdef from the base face");
        assert_eq!(cjk.num_cells, 2);
    }

    #[test]
    fn a_rasterized_glyph_has_a_bitmap_above_the_baseline() {
        let set = jetbrains();
        let face = set.face(0).unwrap();
        let id = face.glyph_index('A').unwrap();
        let glyph = face.rasterize_glyph(id as u32, 12.0, 96).unwrap();
        assert!(glyph.width > 4 && glyph.height > 8, "{}x{}", glyph.width, glyph.height);
        assert!(glyph.bearing_y.get() > 8.0);
        assert!(!glyph.has_color);
        assert_eq!(glyph.data.len(), glyph.width * glyph.height * 4);
    }
}
