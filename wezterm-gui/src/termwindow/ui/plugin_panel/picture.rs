//! A plugin's view -- the panel on show, its extended view, a page -- drawn
//! alone into a picture, for `thinkterm plugin shot`: what its player has,
//! painted with the window's own fonts and colours, on the processor rather
//! than the GPU, into a PNG of the view's size in device pixels.
//!
//! Only what the plugin drew is painted, on the panel's ground, so nothing
//! else of the window can be in it: not what is drawn over the view -- a
//! menu, a tooltip -- nor the terminal beside it, nor what the user is
//! typing in a field and the plugin has not drawn there itself. Painted apart
//! from the frame, it is the same whichever front end the window has, and
//! whether or not the view is in sight. What only the GPU does -- see-through
//! grounds, the caret -- is left out.

use super::{color, token_color, PanelFonts, Shown};
use crate::termwindow::TermWindow;
use crate::ui::{SvgIcon, UiPalette};
use crate::utilsprites::RenderMetrics;
use anyhow::Context;
use std::rc::Rc;
use thinkterm_plugin_panel::player::{
    field_clear_at, field_icon, field_inside, field_radius, FIELD_ICON, FIELD_INSET,
};
use thinkterm_plugin_panel::{
    Align, Area, Bounds, Bytes, Draw, Drawn, Field, FieldKind, FieldPart, Font, Line, Picture,
    Rect, Text, Token,
};
use tiny_skia::{FillRule, Mask, Paint, PathBuilder, Pixmap, PixmapPaint, Stroke, Transform};
use wezterm_font::LoadedFont;
use window::color::LinearRgba;
use window::BitmapImage;

/// The largest picture drawn, across or down, in device pixels.
const LARGEST: u32 = 8192;
/// The largest PNG sent: as base64 in a message, it has to fit in a frame
/// to the host (`MAX_FRAME`, 16 MiB).
const LARGEST_PNG: usize = 11 * 1024 * 1024;

impl TermWindow {
    /// `shown` drawn alone into a picture, or why it could not be.
    pub(super) fn picture_of_view(&self, shown: &Shown) -> Picture {
        match self.draw_view_picture(shown) {
            Ok((png, width, height)) => Picture::Drawn {
                png: Bytes(png),
                width,
                height,
            },
            Err(err) => Picture::Failed {
                why: format!("{err:#}"),
            },
        }
    }

    fn draw_view_picture(&self, shown: &Shown) -> anyhow::Result<(Vec<u8>, u32, u32)> {
        anyhow::ensure!(shown.player.shown(), "it has drawn nothing yet");
        let env = shown.player.env();
        let scale = shown.scale;
        let width = (env.width * scale).round().max(1.0) as u32;
        let height = (env.height * scale).round().max(1.0) as u32;
        anyhow::ensure!(
            width <= LARGEST && height <= LARGEST,
            "it is too large to draw: {width} by {height} pixels"
        );
        let chrome = self.chrome();
        let fonts = self.plugin_panel_fonts()?;
        let mut canvas = Canvas {
            pixmap: Pixmap::new(width, height).context("making the picture")?,
            clip: None,
            scale,
        };
        // Opaque, whatever the panel's ground is: a picture has nothing
        // behind it to see through to.
        let ground = token_color(&chrome, Token::Bg);
        canvas.pixmap.fill(skia_color(ground));
        for draw in &shown.player.draw() {
            canvas.clip_to(draw.clip);
            self.picture_draw(&mut canvas, draw, &fonts, &chrome)?;
        }
        let png = canvas.pixmap.encode_png().context("writing the PNG")?;
        anyhow::ensure!(
            png.len() <= LARGEST_PNG,
            "the picture comes to {} MB, more than can be sent",
            png.len() / (1024 * 1024)
        );
        Ok((png, width, height))
    }

    fn picture_draw(
        &self,
        canvas: &mut Canvas,
        draw: &Draw<'_>,
        fonts: &PanelFonts,
        chrome: &UiPalette,
    ) -> anyhow::Result<()> {
        let scale = canvas.scale;
        let place = |x: f32, y: f32, w: f32, h: f32| Area4 {
            x: (x + draw.dx) * scale,
            y: (y + draw.dy) * scale,
            w: w.max(0.0) * scale,
            h: h.max(0.0) * scale,
        };
        let points = |points: &[f32]| -> Vec<(f32, f32)> {
            points
                .chunks_exact(2)
                .map(|pair| ((pair[0] + draw.dx) * scale, (pair[1] + draw.dy) * scale))
                .collect()
        };
        match draw.what {
            Drawn::Rect(Rect {
                x,
                y,
                w,
                h,
                fill,
                radius,
                border,
            }) => {
                let fill = fill.map(|fill| color(chrome, fill));
                let border = border.map(|border| color(chrome, border));
                canvas.rect(place(*x, *y, *w, *h), fill, border, radius * scale);
            }
            Drawn::Hover(hit) => {
                let tint = color(chrome, hit.hover.unwrap_or_default());
                let area = place(hit.x, hit.y, hit.w, hit.h);
                canvas.rect(area, Some(tint), None, hit.radius * scale);
            }
            Drawn::Text(text) => {
                let area = place(text.x, text.y, text.w, text.h);
                self.picture_text(canvas, text, area, fonts, color(chrome, text.color))?;
            }
            Drawn::Line(Line {
                points: through,
                width,
                color: line_color,
            }) => {
                let through = points(through);
                canvas.line(&through, width * scale, color(chrome, *line_color));
            }
            Drawn::Area(Area {
                points: through,
                base,
                color: fill,
                fade,
            }) => {
                let through = points(through);
                let base = (base + draw.dy) * scale;
                canvas.area(&through, base, color(chrome, *fill), *fade);
            }
            Drawn::Thumb(thumb) => {
                let area = Area4 {
                    x: thumb.left * scale,
                    y: thumb.top * scale,
                    w: thumb.width() * scale,
                    h: thumb.height() * scale,
                };
                let radius = area.w.min(area.h) / 2.0;
                canvas.rect(area, Some(chrome.scrollbar_thumb), None, radius);
            }
            Drawn::Field(field, FieldPart::Box { focused, hovered }) => {
                let area = place(field.x, field.y, field.w, field.h);
                let border = if focused {
                    chrome.accent
                } else if hovered {
                    chrome.separator
                } else {
                    chrome.control_border
                };
                let radius = field_radius(field) * scale;
                canvas.rect(area, Some(chrome.control_bg), Some(border), radius);
                if let Some(icon) = field_icon(field).and_then(SvgIcon::for_field) {
                    let y = field.y + (field.h - FIELD_ICON) / 2.0;
                    let at = place(field.x + FIELD_INSET, y, FIELD_ICON, FIELD_ICON);
                    canvas.icon(icon, at, chrome.secondary_text)?;
                }
                if let Some((x, y, size)) =
                    field_clear_at(field).filter(|_| !field.value.is_empty())
                {
                    let at = place(x, y, size, size);
                    canvas.icon(SvgIcon::X, at, chrome.secondary_text)?;
                }
            }
            Drawn::Field(field, FieldPart::Text) => {
                self.picture_field_text(canvas, field, draw, fonts, chrome)?;
            }
        }
        Ok(())
    }

    /// Text in its box, laid out as the panel lays it: one line centred in
    /// its height and cut to fit -- with an ellipsis in the interface font
    /// -- or, asked to wrap, as many lines as the box holds.
    fn picture_text(
        &self,
        canvas: &mut Canvas,
        text: &Text,
        area: Area4,
        fonts: &PanelFonts,
        ink: LinearRgba,
    ) -> anyhow::Result<()> {
        if text.text.is_empty() || area.w < 1.0 {
            return Ok(());
        }
        let (font, metrics) = fonts.of(text);
        let line_height = metrics.cell_size.height as f32;
        if text.wrap {
            let rect = euclid::rect(area.x, area.y, area.w, area.h);
            let lines = self.panel_text_lines(text, font, metrics, rect)?;
            for (row, shown) in lines.iter().enumerate() {
                let top = area.y + row as f32 * line_height;
                canvas.text_line(font, metrics, shown, text.align, area, top, ink)?;
            }
            return Ok(());
        }
        let top = area.y + (area.h - line_height) / 2.0;
        let shown = match text.font {
            Font::Ui => self.ellipsize_ui_text(font, &text.text, area.w as usize)?,
            Font::Mono => std::borrow::Cow::Borrowed(text.text.as_str()),
        };
        canvas.text_line(font, metrics, &shown, text.align, area, top, ink)
    }

    /// What the plugin put in a field -- or, empty, its placeholder, faint
    /// -- inside its box. What the user is typing there is theirs until the
    /// plugin draws it itself, and a secret field shows only dots.
    fn picture_field_text(
        &self,
        canvas: &mut Canvas,
        field: &Field,
        draw: &Draw<'_>,
        fonts: &PanelFonts,
        chrome: &UiPalette,
    ) -> anyhow::Result<()> {
        let scale = canvas.scale;
        let (font, metrics) = match field.font {
            Font::Ui => &fonts.body,
            Font::Mono => &fonts.mono,
        };
        let inside = field_inside(field);
        let area = Area4 {
            x: (inside.left + draw.dx) * scale,
            y: (inside.top + draw.dy) * scale,
            w: inside.width() * scale,
            h: inside.height() * scale,
        };
        let (said, ink) = if field.value.is_empty() {
            if field.placeholder.is_empty() {
                return Ok(());
            }
            (
                field.placeholder.clone(),
                chrome.secondary_text.mul_alpha(0.72),
            )
        } else if field.kind == FieldKind::Secret {
            ("\u{2022}".repeat(field.value.chars().count()), chrome.text)
        } else {
            (field.value.clone(), chrome.text)
        };
        let line_height = metrics.cell_size.height as f32;
        canvas.within(area, |canvas| {
            if field.kind == FieldKind::Lines {
                for (row, line) in said.split('\n').enumerate() {
                    let top = area.y + row as f32 * line_height;
                    if top >= area.y + area.h {
                        break;
                    }
                    canvas.text_line(font, metrics, line, Align::Left, area, top, ink)?;
                }
                return Ok(());
            }
            let top = area.y + (area.h - line_height) / 2.0;
            let line = said.lines().next().unwrap_or_default();
            canvas.text_line(font, metrics, line, Align::Left, area, top, ink)
        })
    }
}

/// A box in the picture's pixels.
#[derive(Debug, Clone, Copy)]
struct Area4 {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}

/// The picture being drawn, and the region what is drawn now is cut to.
struct Canvas {
    pixmap: Pixmap,
    /// The region, by its edges' bits, and the mask letting only it
    /// through -- none for one covering the whole picture. One at a time:
    /// what a view draws comes in runs of one region.
    clip: Option<([u32; 4], Option<Mask>)>,
    scale: f32,
}

impl Canvas {
    /// Cuts what is drawn next to `clip`, a region in the view's units.
    fn clip_to(&mut self, clip: Bounds) {
        let key = [
            clip.left.to_bits(),
            clip.top.to_bits(),
            clip.right.to_bits(),
            clip.bottom.to_bits(),
        ];
        if self.clip.as_ref().is_some_and(|(known, _)| *known == key) {
            return;
        }
        let scale = self.scale;
        let rect = (
            clip.left * scale,
            clip.top * scale,
            clip.right * scale,
            clip.bottom * scale,
        );
        let (width, height) = (self.pixmap.width() as f32, self.pixmap.height() as f32);
        let whole = rect.0 <= 0.0 && rect.1 <= 0.0 && rect.2 >= width && rect.3 >= height;
        let mask = if whole {
            None
        } else {
            self.mask_of(rect.0, rect.1, rect.2, rect.3, None)
        };
        self.clip = Some((key, mask));
    }

    /// A mask letting through the box from `left`, `top` to `right`,
    /// `bottom`, within `within` when given -- nothing, for a box with no
    /// room in the picture.
    fn mask_of(
        &self,
        left: f32,
        top: f32,
        right: f32,
        bottom: f32,
        within: Option<&Mask>,
    ) -> Option<Mask> {
        let (width, height) = (self.pixmap.width(), self.pixmap.height());
        let mut mask = Mask::new(width, height)?;
        let rect = tiny_skia::Rect::from_ltrb(
            left.max(0.0),
            top.max(0.0),
            right.min(width as f32),
            bottom.min(height as f32),
        );
        if let Some(rect) = rect {
            mask.fill_path(
                &PathBuilder::from_rect(rect),
                FillRule::Winding,
                false,
                Transform::identity(),
            );
        }
        if let Some(within) = within {
            // What both let through.
            for (kept, other) in mask.data_mut().iter_mut().zip(within.data()) {
                *kept = ((*kept as u16 * *other as u16) / 255) as u8;
            }
        }
        Some(mask)
    }

    /// Draws with what is drawn cut to `area` as well, for the while of
    /// `draw`.
    fn within<R>(&mut self, area: Area4, draw: impl FnOnce(&mut Canvas) -> R) -> R {
        let current = self.clip.take();
        let around = current.as_ref().and_then(|(_, mask)| mask.as_ref());
        let narrowed = self.mask_of(area.x, area.y, area.x + area.w, area.y + area.h, around);
        // A key no region has: the next region is cut to anew.
        self.clip = Some(([u32::MAX; 4], narrowed));
        let drawn = draw(self);
        self.clip = current;
        drawn
    }

    fn rect(
        &mut self,
        area: Area4,
        fill: Option<LinearRgba>,
        border: Option<LinearRgba>,
        radius: f32,
    ) {
        if area.w <= 0.0 || area.h <= 0.0 {
            return;
        }
        let mask = self.clip.as_ref().and_then(|(_, mask)| mask.as_ref());
        if let Some(fill) = fill {
            if let Some(path) = rounded(area, radius) {
                self.pixmap.fill_path(
                    &path,
                    &paint(fill),
                    FillRule::Winding,
                    Transform::identity(),
                    mask,
                );
            }
        }
        if let Some(border) = border {
            // A line one unit wide, inside the box: as the panel strokes it.
            let width = self.scale.round().max(1.0);
            let inset = width / 2.0;
            let inner = Area4 {
                x: area.x + inset,
                y: area.y + inset,
                w: (area.w - width).max(0.0),
                h: (area.h - width).max(0.0),
            };
            if let Some(path) = rounded(inner, (radius - inset).max(0.0)) {
                let stroke = Stroke {
                    width,
                    ..Stroke::default()
                };
                self.pixmap.stroke_path(
                    &path,
                    &paint(border),
                    &stroke,
                    Transform::identity(),
                    mask,
                );
            }
        }
    }

    fn line(&mut self, through: &[(f32, f32)], width: f32, ink: LinearRgba) {
        let mut path = PathBuilder::new();
        for (at, (x, y)) in through.iter().enumerate() {
            if at == 0 {
                path.move_to(*x, *y);
            } else {
                path.line_to(*x, *y);
            }
        }
        let Some(path) = path.finish() else {
            return;
        };
        let stroke = Stroke {
            width: width.max(0.5),
            line_cap: tiny_skia::LineCap::Round,
            line_join: tiny_skia::LineJoin::Round,
            ..Stroke::default()
        };
        let mask = self.clip.as_ref().and_then(|(_, mask)| mask.as_ref());
        self.pixmap
            .stroke_path(&path, &paint(ink), &stroke, Transform::identity(), mask);
    }

    /// The area between a line through `through` and `base` below it --
    /// fading out towards `base`, with `fade`.
    fn area(&mut self, through: &[(f32, f32)], base: f32, ink: LinearRgba, fade: bool) {
        let (Some(first), Some(last)) = (through.first(), through.last()) else {
            return;
        };
        let mut path = PathBuilder::new();
        path.move_to(first.0, base);
        for (x, y) in through {
            path.line_to(*x, *y);
        }
        path.line_to(last.0, base);
        path.close();
        let Some(path) = path.finish() else {
            return;
        };
        let mut brush = paint(ink);
        if fade {
            let top = through
                .iter()
                .map(|(_, y)| *y)
                .fold(f32::INFINITY, f32::min);
            let (r, g, b, a) = srgb8(ink);
            let stops = vec![
                tiny_skia::GradientStop::new(0.0, tiny_skia::Color::from_rgba8(r, g, b, a)),
                tiny_skia::GradientStop::new(1.0, tiny_skia::Color::from_rgba8(r, g, b, 0)),
            ];
            if let Some(shader) = tiny_skia::LinearGradient::new(
                tiny_skia::Point::from_xy(0.0, top),
                tiny_skia::Point::from_xy(0.0, base),
                stops,
                tiny_skia::SpreadMode::Pad,
                Transform::identity(),
            ) {
                brush.shader = shader;
            }
        }
        let mask = self.clip.as_ref().and_then(|(_, mask)| mask.as_ref());
        self.pixmap.fill_path(
            &path,
            &brush,
            FillRule::Winding,
            Transform::identity(),
            mask,
        );
    }

    /// One of ThinkTerm's icons, in `ink`, filling `at`.
    fn icon(&mut self, icon: SvgIcon, at: Area4, ink: LinearRgba) -> anyhow::Result<()> {
        let side = at.w.round().max(1.0) as usize;
        let image = icon.rasterize(side)?;
        let (width, height) = image.image_dimensions();
        let (r, g, b, a) = srgb8(ink);
        // White as drawn: its coverage is what is kept, in the ink.
        let mut tinted = Vec::with_capacity(width * height * 4);
        for pixel in image.pixel_data_slice().chunks_exact(4) {
            let cover = pixel[3] as u32 * a as u32 / 255;
            tinted.extend_from_slice(&[
                (r as u32 * cover / 255) as u8,
                (g as u32 * cover / 255) as u8,
                (b as u32 * cover / 255) as u8,
                cover as u8,
            ]);
        }
        self.blit(
            tinted,
            width,
            height,
            at.x.round() as i32,
            at.y.round() as i32,
            1.0,
        );
        Ok(())
    }

    /// One line of text, `shown`, its top at `top`, placed across `area`
    /// as `align` says and cut to it.
    #[allow(clippy::too_many_arguments)]
    fn text_line(
        &mut self,
        font: &Rc<LoadedFont>,
        metrics: &RenderMetrics,
        shown: &str,
        align: Align,
        area: Area4,
        top: f32,
        ink: LinearRgba,
    ) -> anyhow::Result<()> {
        if shown.is_empty() {
            return Ok(());
        }
        let glyphs = font.blocking_shape(
            shown,
            None,
            wezterm_bidi::Direction::LeftToRight,
            None,
            None,
        )?;
        let width: f32 = glyphs
            .iter()
            .map(|glyph| glyph.x_advance.get() as f32)
            .sum();
        let left = match align {
            Align::Left => area.x,
            Align::Center => area.x + (area.w - width) / 2.0,
            Align::Right => area.x + area.w - width,
        };
        let line_height = metrics.cell_size.height as f32;
        let baseline = line_height + metrics.descender.get() as f32;
        let right = area.x + area.w;
        let (r, g, b, a) = srgb8(ink);
        let mut pen = left;
        for glyph in &glyphs {
            let advance = glyph.x_advance.get() as f32;
            // Whole glyphs only, as the panel lays them.
            if pen + advance > right + 0.5 {
                break;
            }
            if glyph.is_space || pen + advance <= area.x {
                pen += advance;
                continue;
            }
            let raster = font.rasterize_glyph(glyph.glyph_pos, glyph.font_idx)?;
            if raster.width > 0 && raster.height > 0 {
                // A coloured one -- an emoji -- taller than the line is made
                // to fit it.
                let fit = if raster.has_color && raster.height as f32 > line_height {
                    line_height / raster.height as f32
                } else {
                    1.0
                };
                let x = pen + ((glyph.x_offset + raster.bearing_x).get() as f32) * fit;
                let y = top + baseline - ((glyph.y_offset + raster.bearing_y).get() as f32) * fit;
                let pixels = if raster.has_color {
                    raster.data
                } else {
                    // Its coverage, in the ink.
                    raster
                        .data
                        .chunks_exact(4)
                        .flat_map(|pixel| {
                            let cover = pixel[3] as u32 * a as u32 / 255;
                            [
                                (r as u32 * cover / 255) as u8,
                                (g as u32 * cover / 255) as u8,
                                (b as u32 * cover / 255) as u8,
                                cover as u8,
                            ]
                        })
                        .collect()
                };
                self.blit(
                    pixels,
                    raster.width,
                    raster.height,
                    x.round() as i32,
                    y.round() as i32,
                    fit,
                );
            }
            pen += advance;
        }
        Ok(())
    }

    /// Premultiplied RGBA `pixels`, `width` by `height`, laid with their top
    /// left corner at `x`, `y`, made `fit` times as large.
    fn blit(&mut self, pixels: Vec<u8>, width: usize, height: usize, x: i32, y: i32, fit: f32) {
        let Some(size) = tiny_skia::IntSize::from_wh(width as u32, height as u32) else {
            return;
        };
        let Some(image) = Pixmap::from_vec(pixels, size) else {
            return;
        };
        let transform = if fit == 1.0 {
            Transform::identity()
        } else {
            Transform::from_row(
                fit,
                0.0,
                0.0,
                fit,
                x as f32 * (1.0 - fit),
                y as f32 * (1.0 - fit),
            )
        };
        let paint = PixmapPaint {
            quality: if fit == 1.0 {
                tiny_skia::FilterQuality::Nearest
            } else {
                tiny_skia::FilterQuality::Bilinear
            },
            ..PixmapPaint::default()
        };
        let mask = self.clip.as_ref().and_then(|(_, mask)| mask.as_ref());
        self.pixmap
            .draw_pixmap(x, y, image.as_ref(), &paint, transform, mask);
    }
}

/// A rounded box's outline.
fn rounded(area: Area4, radius: f32) -> Option<tiny_skia::Path> {
    let radius = radius.max(0.0).min(area.w / 2.0).min(area.h / 2.0);
    if radius < 0.5 {
        let rect = tiny_skia::Rect::from_xywh(area.x, area.y, area.w, area.h)?;
        return Some(PathBuilder::from_rect(rect));
    }
    // A quarter circle as a cubic: its handles this much of the radius out.
    const K: f32 = 0.552_284_8;
    let (left, top) = (area.x, area.y);
    let (right, bottom) = (area.x + area.w, area.y + area.h);
    let k = radius * K;
    let mut path = PathBuilder::new();
    path.move_to(left + radius, top);
    path.line_to(right - radius, top);
    path.cubic_to(
        right - radius + k,
        top,
        right,
        top + radius - k,
        right,
        top + radius,
    );
    path.line_to(right, bottom - radius);
    path.cubic_to(
        right,
        bottom - radius + k,
        right - radius + k,
        bottom,
        right - radius,
        bottom,
    );
    path.line_to(left + radius, bottom);
    path.cubic_to(
        left + radius - k,
        bottom,
        left,
        bottom - radius + k,
        left,
        bottom - radius,
    );
    path.line_to(left, top + radius);
    path.cubic_to(
        left,
        top + radius - k,
        left + radius - k,
        top,
        left + radius,
        top,
    );
    path.close();
    path.finish()
}

fn paint(ink: LinearRgba) -> Paint<'static> {
    let mut paint = Paint {
        anti_alias: true,
        ..Paint::default()
    };
    paint.set_color(skia_color(ink));
    paint
}

fn skia_color(ink: LinearRgba) -> tiny_skia::Color {
    let (r, g, b, a) = srgb8(ink);
    tiny_skia::Color::from_rgba8(r, g, b, a)
}

/// `ink` as 8-bit sRGB, its alpha straight.
fn srgb8(ink: LinearRgba) -> (u8, u8, u8, u8) {
    let LinearRgba(r, g, b, a) = ink;
    let byte = |value: f32| (value.clamp(0.0, 1.0) * 255.0).round() as u8;
    (byte(encode(r)), byte(encode(g)), byte(encode(b)), byte(a))
}

/// A linear light value as sRGB encodes it.
fn encode(linear: f32) -> f32 {
    if linear <= 0.003_130_8 {
        linear * 12.92
    } else {
        1.055 * linear.powf(1.0 / 2.4) - 0.055
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_colour_is_written_as_srgb_and_back() {
        for byte in [0u8, 1, 10, 14, 30, 54, 128, 200, 255] {
            let ink = LinearRgba::with_srgba(byte, byte, byte, 255);
            let (r, g, b, a) = srgb8(ink);
            assert_eq!((r, g, b, a), (byte, byte, byte, 255), "{byte}");
        }
    }

    #[test]
    fn a_rounded_box_is_a_closed_outline_inside_its_area() {
        let path = rounded(
            Area4 {
                x: 2.0,
                y: 3.0,
                w: 40.0,
                h: 20.0,
            },
            50.0,
        )
        .unwrap();
        let bounds = path.bounds();
        assert!((bounds.left() - 2.0).abs() < 0.01 && (bounds.top() - 3.0).abs() < 0.01);
        assert!((bounds.right() - 42.0).abs() < 0.01 && (bounds.bottom() - 23.0).abs() < 0.01);
        assert!(rounded(
            Area4 {
                x: 0.0,
                y: 0.0,
                w: 10.0,
                h: 10.0
            },
            0.0
        )
        .is_some());
    }

    #[test]
    fn one_region_at_a_time_is_kept_and_a_box_within_it_is_both() {
        let mut canvas = Canvas {
            pixmap: Pixmap::new(100, 50).unwrap(),
            clip: None,
            scale: 2.0,
        };
        let whole = Bounds {
            left: 0.0,
            top: 0.0,
            right: 50.0,
            bottom: 25.0,
        };
        canvas.clip_to(whole);
        assert!(
            canvas.clip.as_ref().unwrap().1.is_none(),
            "the whole: no mask"
        );
        let half = Bounds {
            left: 0.0,
            top: 0.0,
            right: 25.0,
            bottom: 25.0,
        };
        canvas.clip_to(half);
        let mask = canvas.clip.as_ref().unwrap().1.as_ref().unwrap();
        // Through on its left half, not its right.
        assert_eq!(mask.data()[10], 255);
        assert_eq!(mask.data()[60], 0);
        let inside = canvas.within(
            Area4 {
                x: 0.0,
                y: 0.0,
                w: 20.0,
                h: 50.0,
            },
            |canvas| canvas.clip.as_ref().unwrap().1.as_ref().unwrap().data()[30],
        );
        assert_eq!(inside, 0, "past the box, though within the region");
        assert_eq!(
            canvas.clip.as_ref().unwrap().0,
            [
                0.0f32.to_bits(),
                0.0f32.to_bits(),
                25.0f32.to_bits(),
                25.0f32.to_bits()
            ],
            "the region again after it"
        );
    }
}
