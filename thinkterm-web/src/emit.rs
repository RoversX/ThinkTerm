//! One row of cells to quads: the phase-one cut of the desktop's
//! `render_screen_line` (wezterm-gui/src/termwindow/render/screen_line.rs):
//! backgrounds, underlines, the selection, the cursor, then glyphs sliced
//! into strips wherever the cursor or the selection changes their colour.
//! No images, no bidi, no hyperlink hover, no blink.

use crate::fallback::FallbackBudget;
use crate::glyphs::{CachedGlyph, GlyphCache};
use anyhow::{Context, Result};
use std::ops::Range;
use std::rc::Rc;
use termwiz::cell::{Intensity, Underline};
use termwiz::surface::{CursorShape, CursorVisibility, Line};
use thinkterm_font_core::PresentationWidth;
use thinkterm_font_web::GlyphRole;
use thinkterm_proto::StableCursorPosition;
use thinkterm_render::quad::{HeapQuadAllocator, QuadTrait, TripleLayerQuadAllocatorTrait};
use wezterm_color_types::{HsbTransform, LinearRgba};
use wezterm_term::color::{ColorAttribute, ColorPalette};
use wezterm_term::{CellAttributes, StableRowIndex};

pub struct LineParams<'a> {
    pub line: &'a Line,
    pub stable_row: StableRowIndex,
    /// Top edge of the row in device pixels from the canvas top.
    pub top_pixel_y: f32,
    pub cursor: &'a StableCursorPosition,
    pub palette: &'a ColorPalette,
    /// Selected cells on this row.
    pub selection: Range<usize>,
    pub focused: bool,
    pub reverse_video: bool,
    /// The canvas, in device pixels: the projection's centre.
    pub surface: (f32, f32),
    /// Where the pane's content box starts on the canvas, in device
    /// pixels. `top_pixel_y` is measured from here.
    pub origin: (f32, f32),
    /// The content box's size in device pixels; nothing is drawn past its
    /// right edge, so a pane on the right of a split does not run into
    /// its neighbour.
    pub clip: (f32, f32),
    /// The desktop's inactive-pane dimming, on every quad of the row when
    /// this pane is not the focused one.
    pub hsv: Option<HsbTransform>,
    /// Only the focused pane shows its cursor.
    pub draw_cursor: bool,
}

/// A flat rectangle at `at` of `size`, both in device pixels from the
/// canvas's top-left: a pane's ground, a divider.
#[allow(clippy::too_many_arguments)]
pub fn fill_rect(
    cache: &GlyphCache,
    layers: &mut HeapQuadAllocator,
    layer: usize,
    surface: (f32, f32),
    at: (f32, f32),
    size: (f32, f32),
    color: LinearRgba,
    hsv: Option<HsbTransform>,
) -> Result<()> {
    let (x, y) = (-surface.0 / 2.0 + at.0, -surface.1 / 2.0 + at.1);
    let mut quad = layers.allocate(layer).context("allocate")?;
    quad.set_position(x, y, x + size.0, y + size.1);
    quad.set_texture(cache.filled_box.texture_coords());
    quad.set_is_background();
    quad.set_fg_color(color);
    quad.set_hsv(hsv);
    Ok(())
}

/// The desktop's default `bold_brightens_ansi_colors`: bold lifts the
/// first eight palette colours to their bright twins.
fn resolve_fg(attrs: &CellAttributes, fg: ColorAttribute, palette: &ColorPalette) -> LinearRgba {
    match fg {
        ColorAttribute::PaletteIndex(idx) if idx < 8 => {
            let idx = if attrs.intensity() == Intensity::Bold {
                idx + 8
            } else {
                idx
            };
            palette.resolve_fg(ColorAttribute::PaletteIndex(idx))
        }
        other => palette.resolve_fg(other),
    }
    .to_linear()
}

fn intersection(r1: &Range<f32>, r2: &Range<f32>) -> Range<f32> {
    let start = r1.start.max(r2.start);
    let end = r1.end.min(r2.end);
    if end > start {
        start..end
    } else {
        start..start
    }
}

/// `r` split into the parts left of, inside and right of `within`.
fn range3(r: &Range<f32>, within: &Range<f32>) -> (Range<f32>, Range<f32>, Range<f32>) {
    if r.is_empty() {
        return (r.clone(), r.clone(), r.clone());
    }
    let i = intersection(r, within);
    if i.is_empty() {
        return (r.clone(), i.clone(), i.clone());
    }
    let left = if i.start > r.start {
        r.start..i.start
    } else {
        r.start..r.start
    };
    let right = if i.end < r.end {
        i.end..r.end
    } else {
        r.end..r.end
    };
    (left, i, right)
}

fn is_block(shape: CursorShape) -> bool {
    matches!(
        shape,
        CursorShape::Default | CursorShape::BlinkingBlock | CursorShape::SteadyBlock
    )
}

pub fn emit_line(
    cache: &mut GlyphCache,
    layers: &mut HeapQuadAllocator,
    budget: &mut FallbackBudget,
    p: &LineParams<'_>,
) -> Result<()> {
    if p.line.is_double_height_bottom() {
        return Ok(());
    }
    let metrics = cache.metrics;
    let width_scale: f32 = if p.line.is_single_width() { 1.0 } else { 2.0 };
    let height_scale: f32 = if p.line.is_double_height_top() { 2.0 } else { 1.0 };
    let cell_width = metrics.cell_size.width as f32 * width_scale;
    let cell_height = metrics.cell_size.height as f32 * height_scale;
    let gl_x = -p.surface.0 / 2.0 + p.origin.0;
    let pos_y = -p.surface.1 / 2.0 + p.origin.1 + p.top_pixel_y;
    let pixel_width = p.clip.0;

    let cursor_on_row = p.stable_row == p.cursor.y;
    let cursor_cell = if cursor_on_row {
        p.line.get_cell(p.cursor.x)
    } else {
        None
    };
    let cursor_range = if cursor_on_row {
        p.cursor.x..p.cursor.x + cursor_cell.as_ref().map(|c| c.width()).unwrap_or(1)
    } else {
        0..0
    };
    let cursor_range_pixels =
        cursor_range.start as f32 * cell_width..cursor_range.end as f32 * cell_width;
    let selection_pixels = if p.selection.is_empty() {
        0.0..0.0
    } else {
        p.selection.start as f32 * cell_width..p.selection.end as f32 * cell_width
    };
    let cursor_visible =
        p.draw_cursor && cursor_on_row && p.cursor.visibility == CursorVisibility::Visible;
    let filled_cursor = cursor_visible && p.focused && is_block(p.cursor.shape);

    let selection_fg = p.palette.selection_fg.to_linear();
    let selection_bg = p.palette.selection_bg.to_linear();
    let cursor_fg = p.palette.cursor_fg.to_linear();
    let cursor_bg = p.palette.cursor_bg.to_linear();
    let cursor_border = p.palette.cursor_border.to_linear();
    let default_fg = p.palette.foreground.to_linear();
    let filled_box = cache.filled_box.texture_coords();

    let filled_rectangle =
        |layers: &mut HeapQuadAllocator, layer: usize, x: f32, w: f32, color: LinearRgba| -> Result<()> {
            let mut quad = layers.allocate(layer).context("allocate")?;
            quad.set_position(gl_x + x, pos_y, gl_x + x + w, pos_y + cell_height);
            quad.set_texture(filled_box);
            quad.set_is_background();
            quad.set_fg_color(color);
            quad.set_hsv(p.hsv);
            Ok(())
        };

    if p.reverse_video {
        filled_rectangle(layers, 0, 0.0, pixel_width, default_fg)?;
    }

    // Shape and resolve every cluster first: colours and glyphs.
    struct Shaped {
        first_cell: usize,
        width: usize,
        fg: LinearRgba,
        bg: LinearRgba,
        bg_is_default: bool,
        invisible: bool,
        underline: Option<(thinkterm_render::bitmaps::TextureRect, LinearRgba)>,
        glyphs: Vec<(u8, Rc<CachedGlyph>)>,
    }
    let clusters = p.line.cluster(None);
    let mut shaped: Vec<Shaped> = Vec::with_capacity(clusters.len());
    for cluster in &clusters {
        let attrs = &cluster.attrs;
        let bg_is_default = attrs.background() == ColorAttribute::Default;
        let bg = p.palette.resolve_bg(attrs.background()).to_linear();
        let fg = resolve_fg(attrs, attrs.foreground(), p.palette);
        let (fg, bg, bg_is_default) = if attrs.reverse() == !p.reverse_video {
            (bg, fg, false)
        } else {
            (fg, bg, bg_is_default)
        };
        let underline = if attrs.underline() != Underline::None
            || attrs.strikethrough()
            || attrs.overline()
        {
            let sprite = cache.line_sprite(attrs.strikethrough(), attrs.underline(), attrs.overline())?;
            let color = match attrs.underline_color() {
                ColorAttribute::Default => fg,
                other => p.palette.resolve_fg(other).to_linear(),
            };
            Some((sprite.texture_coords(), color))
        } else {
            None
        };
        let widths = PresentationWidth::with_cluster(cluster);
        let run = cache.shape(
            &cluster.text,
            Some(cluster.presentation),
            Some(&widths),
            cluster.width,
        )?;
        // Nothing off the right edge gets a fallback. The stand-in used to
        // be one shared `.notdef` and cost nothing; a fallback is a canvas
        // draw and an atlas slot each, out of a budget the whole frame
        // shares.
        //
        // Asked per gap, not per cluster. `CellCluster` breaks only on an
        // attribute change or whitespace, and Chinese prose has no
        // whitespace, so a 150-column row of Han is one cluster starting at
        // column 0: a test on where the cluster starts called all 150
        // visible in an 80-column viewport and paid for 70 draws the strip
        // loop below then threw away.
        let mut fallbacks: Vec<Option<Rc<CachedGlyph>>> = vec![None; run.gaps.len()];
        let mut columns: Vec<u8> = vec![0; run.gaps.len()];
        if !attrs.invisible() {
            for (g, gap) in run.gaps.iter().enumerate() {
                // Already absolute: `CellCluster::byte_to_cell_idx` adds
                // `first_cell_idx` itself (cellcluster.rs:34, :295). Adding
                // it again put every cluster but the first too far right,
                // so its gaps were taken for off-screen and left as boxes.
                let cell = widths.byte_to_cell_idx(gap.bytes.start);
                if (cell as f32) * cell_width >= pixel_width {
                    break;
                }
                // Counted from the row in hand, not read back from the
                // shape cache. `ShapeKey` holds the cluster's *total*
                // columns, so two rows that spend the same total
                // differently -- `[1,2,1]` and `[2,1,1]` for the same text,
                // which explicit cell widths allow -- share an entry, and
                // the second would inherit the first's per-gap count and
                // shift everything after it.
                columns[g] = widths.num_cells(gap.bytes.clone());
                fallbacks[g] = cache.fallback_glyph(gap, columns[g], budget)?;
            }
        }
        let mut glyphs = Vec::with_capacity(run.infos.len());
        for (idx, info) in run.infos.iter().enumerate() {
            // Read from the list as it was shaped: whether a space follows
            // decides if this glyph may overflow its cell, and that must not
            // change because the gap after it got replaced.
            let followed_by_space =
                run.infos.get(idx + 1).map(|n| n.is_space).unwrap_or(false);
            match run.roles[idx] {
                GlyphRole::GapLead(g) if fallbacks[g as usize].is_some() => {
                    // The whole grapheme, in the columns the row's own width
                    // data gave it. No `.max(1)`: a zero-column gap was
                    // refused before it got here, and inventing a column
                    // shifts the rest of the line with nothing to report it.
                    let glyph = Rc::clone(fallbacks[g as usize].as_ref().expect("just checked"));
                    glyphs.push((columns[g as usize], glyph));
                }
                // The gap's other stand-ins. Keeping one draws the cell
                // twice and advances the column twice.
                GlyphRole::GapTail(g) if fallbacks[g as usize].is_some() => {}
                _ => glyphs.push((
                    info.num_cells,
                    cache.cached_glyph(info, followed_by_space, info.num_cells)?,
                )),
            }
        }
        shaped.push(Shaped {
            first_cell: cluster.first_cell_idx,
            width: cluster.width,
            fg,
            bg,
            bg_is_default,
            invisible: attrs.invisible(),
            underline,
            glyphs,
        });
    }

    // Backgrounds and underlines.
    for item in &shaped {
        if !item.bg_is_default {
            let x = item.first_cell as f32 * cell_width;
            let w = item.width as f32 * cell_width;
            if x < pixel_width {
                filled_rectangle(layers, 0, x, w.min(pixel_width - x), item.bg)?;
            }
        }
        if let Some((tex, color)) = &item.underline {
            for i in 0..item.width {
                let x = gl_x + (item.first_cell + i) as f32 * cell_width;
                let mut quad = layers.allocate(0).context("allocate")?;
                quad.set_position(x, pos_y, x + cell_width, pos_y + cell_height);
                quad.set_hsv(p.hsv);
                quad.set_has_color(false);
                quad.set_texture(*tex);
                quad.set_fg_color(*color);
            }
        }
    }

    if !p.selection.is_empty() && selection_pixels.start < pixel_width {
        filled_rectangle(
            layers,
            0,
            selection_pixels.start,
            (selection_pixels.end - selection_pixels.start).min(pixel_width - selection_pixels.start),
            selection_bg,
        )?;
    }

    // The cursor: filled when this page has focus and the shape is a
    // block, an outline when it does not, bar and underline as themselves.
    if cursor_visible {
        let (shape, color) = if !p.focused {
            (CursorShape::SteadyBlock, cursor_border)
        } else if is_block(p.cursor.shape) {
            (CursorShape::Default, cursor_bg)
        } else {
            (p.cursor.shape, cursor_bg)
        };
        let layer = match shape {
            CursorShape::BlinkingBar | CursorShape::SteadyBar => 2,
            _ => 0,
        };
        let width_cells = (cursor_range.end - cursor_range.start).max(1) as u8;
        let sprite = cache.cursor_sprite(Some(shape), width_cells)?;
        let x = gl_x + cursor_range_pixels.start;
        let mut quad = layers.allocate(layer).context("allocate")?;
        quad.set_hsv(p.hsv);
        quad.set_has_color(false);
        quad.set_position(x, pos_y, x + width_cells as f32 * cell_width, pos_y + cell_height);
        quad.set_texture(sprite.texture_coords());
        quad.set_fg_color(color);
        quad.set_alt_color_and_mix_value(color, 0.0);
    }

    // Glyphs, in strips.
    let descender = metrics.descender.get() as f32;
    for item in &shaped {
        let mut cluster_x = item.first_cell as f32 * cell_width;
        for (num_cells, glyph) in &item.glyphs {
            if let Some(texture) = &glyph.texture {
                let top = cell_height
                    + (descender - (glyph.y_offset + glyph.bearing_y).get() as f32) * height_scale;
                let pos_x = cluster_x;
                if pos_x > pixel_width {
                    break;
                }
                let adjust = (glyph.x_offset + glyph.bearing_x).get() as f32;
                let texture_range = pos_x + adjust
                    ..pos_x + adjust + texture.coords.size.width as f32 * width_scale;
                let (left, mid, right) = range3(&texture_range, &cursor_range_pixels);
                let (la, lb, lc) = range3(&left, &selection_pixels);
                let (ra, rb, rc) = range3(&right, &selection_pixels);
                for range in [la, lb, lc, mid, ra, rb, rc] {
                    if range.is_empty() {
                        continue;
                    }
                    let is_cursor = cursor_range_pixels.contains(&range.start);
                    let selected = !is_cursor && selection_pixels.contains(&range.start);
                    let (glyph_color, bg_color) = if is_cursor && filled_cursor {
                        (cursor_fg.when_fully_transparent(item.fg), cursor_bg)
                    } else if selected {
                        (selection_fg.when_fully_transparent(item.fg), selection_bg)
                    } else {
                        (item.fg, item.bg)
                    };
                    // A colour glyph takes its colour from the texture, not
                    // from the foreground, so a foreground that happens to
                    // equal the background says nothing about whether it
                    // would be visible. `invisible` still hides everything.
                    if item.invisible || (!glyph.has_color && glyph_color == bg_color) {
                        continue;
                    }
                    let pixel_rect = euclid::rect(
                        texture.coords.origin.x + (range.start - (pos_x + adjust)) as isize,
                        texture.coords.origin.y,
                        ((range.end - range.start) / width_scale) as isize,
                        texture.coords.size.height,
                    );
                    let texture_rect = texture.texture.to_texture_coords(pixel_rect);
                    let mut quad = layers.allocate(1).context("allocate")?;
                    quad.set_position(
                        gl_x + range.start,
                        pos_y + top,
                        gl_x + range.end,
                        pos_y + top + texture.coords.size.height as f32 * height_scale,
                    );
                    quad.set_fg_color(glyph_color);
                    quad.set_alt_color_and_mix_value(glyph_color, 0.0);
                    quad.set_texture(texture_rect);
                    quad.set_hsv(p.hsv);
                    quad.set_has_color(glyph.has_color);
                }
            }
            cluster_x += *num_cells as f32 * cell_width;
        }
    }
    Ok(())
}
