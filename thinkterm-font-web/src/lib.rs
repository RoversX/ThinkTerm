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
use std::collections::HashMap;
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
}

impl std::fmt::Debug for Face {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Face")
            .field("name", &self.name)
            .field("presentation", &self.presentation)
            .finish()
    }
}

fn is_joiner_or_selector(c: char) -> bool {
    matches!(c, '\u{200d}' | '\u{fe0e}' | '\u{fe0f}') || ('\u{e0100}'..='\u{e01ef}').contains(&c)
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

    /// Whether every character that matters in `cluster` has a glyph here.
    fn covers(&self, cluster: &str) -> bool {
        let charmap = self.swash().charmap();
        let mut any = false;
        for c in cluster.chars() {
            if is_joiner_or_selector(c) {
                continue;
            }
            any = true;
            if charmap.map(c) == 0 {
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

impl FontShaper for FontSet {
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
        let range = range.unwrap_or(0..text.len());
        let px = Face::pixels(size, dpi);
        let mut runs: Vec<(usize, Range<usize>)> = Vec::new();
        let mut byte = range.start;
        for grapheme in Graphemes::new(&text[range.clone()]) {
            let end = byte + grapheme.len();
            let face = match self.face_for(grapheme, presentation) {
                Some(face) => face,
                None => {
                    no_glyphs.extend(grapheme.chars().filter(|c| !is_joiner_or_selector(*c)));
                    0
                }
            };
            match runs.last_mut() {
                Some((f, r)) if *f == face && r.end == byte => r.end = end,
                _ => runs.push((face, byte..end)),
            }
            byte = end;
        }
        let mut out = Vec::new();
        for (face, run) in runs {
            out.extend(self.shape_run(face, text, run, px, direction, presentation_width));
        }
        Ok(out)
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
