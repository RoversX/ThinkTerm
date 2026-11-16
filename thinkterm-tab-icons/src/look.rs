//! How a card and a thread's mark look, said once for every painter: how
//! a card's circle is lit, where the second icon of a row sits against
//! the first, which pixels of a mark are lit and in what colour.
//!
//! The desktop draws them as quads from these numbers. A client that has
//! no such painter -- the phone, a page -- is handed the finished picture
//! as an SVG (`cards_svg`, `mark_svg`) made from the same numbers, and
//! draws nothing of its own.

use crate::{Rgb, WireCard};
use std::fmt::Write as _;

/// How a card's circle is lit: the fill lightens toward white at the top
/// and darkens toward black at the bottom, and the rim -- one point on a
/// 25pt icon -- does the same more strongly, so the edge reads as a bevel
/// in the circle's own colour. The shadow is in its hue too. Every amount
/// is a blend in sRGB, as a designer's tool would make it.
pub struct Plate {
    pub fill_top_lighten: f32,
    pub fill_bottom_darken: f32,
    pub rim_top_lighten: f32,
    pub rim_bottom_darken: f32,
    /// The rim's width against the circle's diameter.
    pub rim_per_side: f32,
    /// The shadow's colour, as the circle's darkened this far (1 is black).
    pub shadow_darken: f32,
    /// Its peak opacity on dark chrome and on light, where the same shadow
    /// shows far more.
    pub shadow_alpha_dark: f32,
    pub shadow_alpha_light: f32,
    /// Its softness and how far it drops, in design pixels.
    pub shadow_sigma: f32,
    pub shadow_drop: f32,
}

pub const PLATE: Plate = Plate {
    fill_top_lighten: 0.18,
    fill_bottom_darken: 0.13,
    rim_top_lighten: 0.42,
    rim_bottom_darken: 0.25,
    rim_per_side: 1.0 / 25.0,
    shadow_darken: 0.55,
    shadow_alpha_dark: 0.5,
    shadow_alpha_light: 0.32,
    shadow_sigma: 3.5,
    shadow_drop: 2.5,
};

/// A glyph's side against its circle's diameter.
pub const GLYPH_RATIO: f32 = 0.56;

fn blend(color: Rgb, target: f32, amount: f32) -> Rgb {
    let toward = |channel: u8| {
        let channel = channel as f32;
        (channel + (target - channel) * amount).round().clamp(0.0, 255.0) as u8
    };
    Rgb(toward(color.0), toward(color.1), toward(color.2))
}

/// `color` taken `amount` of the way to white.
pub fn lighten(color: Rgb, amount: f32) -> Rgb {
    blend(color, 255.0, amount)
}

/// `color` taken `amount` of the way to black.
pub fn darken(color: Rgb, amount: f32) -> Rgb {
    blend(color, 0.0, amount)
}

/// Where a card stands among the cards of a thread's row: those that
/// dress agents first, then other programs', the terminal's last.
pub fn row_rank(dresses_agents: bool, terminal: bool) -> u8 {
    if dresses_agents {
        0
    } else if terminal {
        2
    } else {
        1
    }
}

/// Where the second of a row's icons sits against the first, `d` across:
/// out to the right rather than down, so it stays within the row.
pub struct Badge {
    /// Its diameter.
    pub small: f32,
    /// Its corner, from the first icon's.
    pub x: f32,
    pub y: f32,
    /// How wide a ring of the row's colour sets it off from the first.
    pub ring: f32,
}

pub fn badge(d: f32) -> Badge {
    Badge {
        small: (d * 0.55).round(),
        x: d * 0.78,
        y: d * 0.55,
        ring: ring(d),
    }
}

/// How wide a ring of the row's colour goes round an icon `d` across
/// where it covers another.
pub fn ring(d: f32) -> f32 {
    (d * 0.08).round().max(1.0)
}

/// How wide a first icon and its badge are together, against the first:
/// the room a row keeps for its icons however many there are, so every
/// row's name starts at the same place.
pub const BADGE_WIDTH: f32 = 1.33;

/// The disc saying how many more programs there are, on dark chrome and
/// on light. Its figures are white.
pub const COUNT_DISC_DARK: Rgb = Rgb(72, 72, 74);
pub const COUNT_DISC_LIGHT: Rgb = Rgb(142, 142, 147);

// ---------------------------------------------------------------------------
// Marks.

/// What a thread's mark says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mark {
    Working,
    Waiting,
    Failed,
    Finished,
    /// Anything else: active, unread, open, closed -- told apart by colour.
    Still,
}

/// The one set of colours a thread's or an agent's state is shown in,
/// wherever it is shown, so a colour means the same thing everywhere; in
/// linear light.
pub const WORKING: [f32; 3] = [0.12, 0.48, 1.0];
/// Waiting on the user: amber, a question rather than something wrong.
pub const WAITING: [f32; 3] = [0.86, 0.45, 0.12];
/// A finished result nobody has looked at yet.
pub const FINISHED: [f32; 3] = [0.20, 0.78, 0.36];
/// A program stopped on an error: the desktop chrome's danger red, which
/// the desktop paints a failure in itself (`status_color` gives none for
/// it there); in linear light, the dark ground's `DANGER_DARK`.
pub const DANGER: [f32; 3] = [1.0, 0.0595, 0.0423];
/// The thread the user is in, at rest or working. It never has an unseen
/// result, being looked at, so the green cannot be mistaken for one.
pub const CURRENT: [f32; 3] = FINISHED;
/// Open and idle: the blue of working -- a working mark moves, an idle
/// one does not.
pub const OPEN: [f32; 3] = WORKING;
/// Open with output nobody has seen: the row's text this present,
/// brighter than idle.
pub const UNREAD_ALPHA: f32 = 0.85;
/// Saved but not open: the row's text this present.
pub const CLOSED_ALPHA: f32 = 0.16;

/// What a mark is coloured with.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Tone {
    /// One of the status colours, in linear light.
    Color([f32; 3]),
    /// The row's own text, this present.
    Text(f32),
}

/// A thread at rest: the one the user is in, `current`, in green; then
/// one that is not open, one with output nobody saw, an open one.
pub fn still_tone(current: bool, open: bool, unread: bool) -> Tone {
    if current {
        Tone::Color(CURRENT)
    } else if !open {
        Tone::Text(CLOSED_ALPHA)
    } else if unread {
        Tone::Text(UNREAD_ALPHA)
    } else {
        Tone::Color(OPEN)
    }
}

/// A thread doing something; the one the user is in, `current`, works in
/// its own green. None for a mark at rest (`still_tone`) and for a
/// failure, which is the chrome's own danger red.
pub fn status_color(mark: Mark, current: bool) -> Option<[f32; 3]> {
    match mark {
        Mark::Working if current => Some(CURRENT),
        Mark::Working => Some(WORKING),
        Mark::Waiting => Some(WAITING),
        Mark::Finished => Some(FINISHED),
        Mark::Failed | Mark::Still => None,
    }
}

/// How far a status colour with no light twin of its own is taken toward
/// black on a light ground.
pub const LIGHT_GROUND_DARKEN: f32 = 0.28;

/// The status colours on a light ground. The dark ground's glow on white
/// washes out, and taken toward black it turns muddy: the light side has
/// colours of its own instead, as clear as the system's light blue, orange
/// and green.
pub const WORKING_LIGHT: Rgb = Rgb(0, 122, 255);
pub const WAITING_LIGHT: Rgb = Rgb(255, 149, 0);
pub const FINISHED_LIGHT: Rgb = Rgb(52, 199, 89);
/// The desktop chrome's danger red on each ground (`ui::tokens`).
pub const DANGER_DARK: Rgb = Rgb(255, 69, 58);
pub const DANGER_LIGHT: Rgb = Rgb(215, 38, 61);

/// A status colour as it is shown on a ground that is `dark` or light.
pub fn status_srgb(color: [f32; 3], dark: bool) -> Rgb {
    if color == DANGER {
        return if dark { DANGER_DARK } else { DANGER_LIGHT };
    }
    if dark {
        return srgb(color);
    }
    if color == WORKING {
        WORKING_LIGHT
    } else if color == WAITING {
        WAITING_LIGHT
    } else if color == FINISHED {
        FINISHED_LIGHT
    } else {
        darken(srgb(color), LIGHT_GROUND_DARKEN)
    }
}

/// A colour in linear light as the sRGB it is shown in.
pub fn srgb(linear: [f32; 3]) -> Rgb {
    let channel = |linear: f32| {
        let srgb = if linear <= 0.0031308 {
            linear * 12.92
        } else {
            linear.powf(1.0 / 2.4) * 1.055 - 0.055
        };
        (srgb * 255.0).round().clamp(0.0, 255.0) as u8
    };
    Rgb(channel(linear[0]), channel(linear[1]), channel(linear[2]))
}

/// How long a working mark holds each step, in milliseconds: a little
/// slower than a spinner's 80ms, a spinner turning at the same time
/// costing some repaints of its own. Every working mark steps on the
/// wall clock, so they all step together.
pub const MARK_STEP_MS: u64 = 100;
/// Steps in one turn of a working mark.
pub const MARK_STEPS: u64 = 8;

/// Pixels lit by a still mark, row by row.
pub const PLUS: [usize; 5] = [1, 3, 4, 5, 7];
pub const CROSS: [usize; 5] = [0, 2, 4, 6, 8];
/// The ring a working mark runs round, clockwise from the top left.
pub const RING: [usize; 8] = [0, 1, 2, 5, 8, 7, 6, 3];
/// How lit a pixel of a working mark is, by steps since the run passed it.
pub const TRAIL: [f32; 4] = [1.0, 0.8, 0.6, 0.39];
pub const UNLIT: f32 = 0.18;

/// How lit pixel `index` of a mark's three-by-three grid is. `step` is
/// the clock's for a mark that moves, `None` for a still picture of one.
pub fn pixel_alpha(mark: Mark, step: Option<u64>, index: usize) -> f32 {
    let lit = |pixels: &[usize]| if pixels.contains(&index) { 1.0 } else { UNLIT };
    match mark {
        Mark::Working => {
            let Some(place) = RING.iter().position(|&pixel| pixel == index) else {
                return 0.5;
            };
            // A still picture shows the run three pixels in.
            let step = step.unwrap_or(2);
            let since = (step + MARK_STEPS - place as u64 % MARK_STEPS) % MARK_STEPS;
            TRAIL.get(since as usize).copied().unwrap_or(UNLIT)
        }
        Mark::Failed => lit(&CROSS),
        Mark::Finished => 1.0,
        Mark::Waiting | Mark::Still => lit(&PLUS),
    }
}

/// A mark's grid in a `size` square: three pixels and two gaps a little
/// wider than the square, centred on it; corners round enough to show at
/// this size.
pub struct PixelGrid {
    pub cell: f32,
    pub gap: f32,
    pub radius: f32,
    /// The grid's corner, from the square's: negative, the grid being the
    /// wider. Not on whole pixels: a painter rounds where it draws.
    pub inset: f32,
}

pub fn pixel_grid(size: f32) -> PixelGrid {
    let cell = (size * 0.34).round().max(2.0);
    let gap = (size * 0.1).round().max(1.0);
    PixelGrid {
        cell,
        gap,
        radius: cell * 0.3,
        inset: (size - (cell * 3.0 + gap * 2.0)) / 2.0,
    }
}

/// A result waiting to be seen is a tick in every style, this much larger
/// than the marks: an outline does not read at their size.
pub const TICK_SCALE: f32 = 1.5;

fn tick_svg() -> &'static str {
    std::str::from_utf8(include_bytes!("../../third_party/lucide/icons/circle-check.svg")).unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Pictures.

/// An SVG as an `href`: every byte that could end an attribute or a URL
/// is escaped.
fn data_url(svg: &str) -> String {
    let mut url = String::with_capacity(svg.len() * 2 + 24);
    url.push_str("data:image/svg+xml,");
    for byte in svg.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            url.push(byte as char);
        } else {
            let _ = write!(url, "%{byte:02X}");
        }
    }
    url
}

/// `shape` drawn `size` square at `x`, `y`: in its own colours, or as a
/// shape filled with `color` -- what it was drawn in is dropped.
fn glyph_svg(out: &mut String, id: &str, shape: &str, own_colors: bool, x: f32, y: f32, size: f32, color: Rgb, alpha: f32) {
    if shape.is_empty() || size < 1.0 {
        return;
    }
    let image = format!(r#"<image x="{x}" y="{y}" width="{size}" height="{size}" href="{}"/>"#, data_url(shape));
    if own_colors {
        let _ = write!(out, r#"<g opacity="{alpha}">{image}</g>"#);
    } else {
        let _ = write!(
            out,
            r#"<mask id="{id}" mask-type="alpha" style="mask-type:alpha" maskUnits="userSpaceOnUse" x="{x}" y="{y}" width="{size}" height="{size}">{image}</mask><rect x="{x}" y="{y}" width="{size}" height="{size}" fill="{}" fill-opacity="{alpha}" mask="url(#{id})"/>"#,
            color.to_hex()
        );
    }
}

/// A row's icons as one picture, ready to show.
pub struct CardsPicture {
    pub svg: String,
    /// The picture's size, in pixels.
    pub width: f32,
    pub height: f32,
    /// How far in from the picture's edges the icons' own box starts: the
    /// room their shadows fall in.
    pub inset: f32,
    /// The box's width, the room a row keeps for its icons
    /// (`BADGE_WIDTH`); it is as high as the first icon.
    pub slot: f32,
    /// The disc standing for the programs past the first: the picture has
    /// the disc, and whoever shows it writes the number on it.
    pub count: Option<Count>,
}

pub struct Count {
    pub more: usize,
    /// The disc's corner in the picture, and its diameter.
    pub x: f32,
    pub y: f32,
    pub diameter: f32,
}

/// The icons of what a thread's panes run, the first `d` pixels across:
/// the first card, and at its corner the second, or a count of the rest
/// when there are more. `px` is how many pixels a design pixel is, `dark`
/// the chrome's appearance. With `glyphs` false the circles are bare, for
/// something else to be shown on them. None when there are no cards.
pub fn cards_svg(cards: &[&WireCard], d: f32, px: f32, dark: bool, glyphs: bool) -> Option<CardsPicture> {
    let first = cards.first()?;
    let d = d.round().max(1.0);
    let sigma = (PLATE.shadow_sigma * px).round();
    let drop = PLATE.shadow_drop * px;
    let inset = (sigma * 3.0 + drop).ceil();
    let slot = (d * BADGE_WIDTH).ceil();
    let (width, height) = (slot + inset * 2.0, d + inset * 2.0);
    let mut svg = format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" viewBox="0 0 {width} {height}"><defs><filter id="s" filterUnits="userSpaceOnUse" x="0" y="0" width="{width}" height="{height}"><feGaussianBlur stdDeviation="{sigma}"/></filter></defs>"#
    );
    let shadow_alpha = if dark { PLATE.shadow_alpha_dark } else { PLATE.shadow_alpha_light };
    // A circle `d` across at `x`, `y`: its shadow, its rim, and inside
    // the rim its fill.
    let plate = |svg: &mut String, id: &str, x: f32, y: f32, d: f32, circle: Rgb| {
        let (r, cx, cy) = (d / 2.0, x + d / 2.0, y + d / 2.0);
        let rim = (d * PLATE.rim_per_side).round().max(1.0);
        let _ = write!(
            svg,
            r#"<linearGradient id="{id}r" gradientUnits="userSpaceOnUse" x1="0" y1="{y}" x2="0" y2="{bottom}"><stop offset="0" stop-color="{}"/><stop offset="1" stop-color="{}"/></linearGradient><linearGradient id="{id}f" gradientUnits="userSpaceOnUse" x1="0" y1="{top}" x2="0" y2="{foot}"><stop offset="0" stop-color="{}"/><stop offset="1" stop-color="{}"/></linearGradient><circle cx="{cx}" cy="{shadow_cy}" r="{r}" fill="{}" fill-opacity="{shadow_alpha}" filter="url(#s)"/><circle cx="{cx}" cy="{cy}" r="{r}" fill="url(#{id}r)"/><circle cx="{cx}" cy="{cy}" r="{inner}" fill="url(#{id}f)"/>"#,
            lighten(circle, PLATE.rim_top_lighten).to_hex(),
            darken(circle, PLATE.rim_bottom_darken).to_hex(),
            lighten(circle, PLATE.fill_top_lighten).to_hex(),
            darken(circle, PLATE.fill_bottom_darken).to_hex(),
            darken(circle, PLATE.shadow_darken).to_hex(),
            bottom = y + d,
            top = y + rim,
            foot = y + d - rim,
            shadow_cy = cy + drop,
            inner = (r - rim).max(0.0),
        );
    };
    let card = |svg: &mut String, id: &str, x: f32, y: f32, d: f32, card: &WireCard| {
        let circle = Rgb::parse(&card.circle).unwrap_or(Rgb(0x8E, 0x8E, 0x93));
        plate(svg, id, x, y, d, circle);
        if glyphs {
            // A mark drawn bigger than the rest keeps their centre.
            let size = ((d * GLYPH_RATIO).round() * card.scale).round();
            let color = Rgb::parse(&card.glyph).unwrap_or(crate::WHITE);
            let at = |corner: f32| (corner + (d - size) / 2.0).round();
            glyph_svg(svg, &format!("{id}g"), &card.svg, card.full_color, at(x), at(y), size, color, 1.0);
        }
    };
    let (x, y) = (inset, inset);
    let mut count = None;
    if cards.len() == 1 {
        card(&mut svg, "a", x, y, d, first);
    } else {
        let badge = badge(d);
        let (bx, by) = (x + badge.x, y + badge.y);
        let (cx, cy) = (bx + badge.small / 2.0, by + badge.small / 2.0);
        // The ring that sets the badge off is the row showing through.
        let _ = write!(
            svg,
            r##"<mask id="c" maskUnits="userSpaceOnUse" x="0" y="0" width="{width}" height="{height}"><rect width="{width}" height="{height}" fill="#fff"/><circle cx="{cx}" cy="{cy}" r="{}" fill="#000"/></mask><g mask="url(#c)">"##,
            badge.small / 2.0 + badge.ring
        );
        card(&mut svg, "a", x, y, d, first);
        svg.push_str("</g>");
        if cards.len() == 2 {
            card(&mut svg, "b", bx, by, badge.small, cards[1]);
        } else {
            let disc = if dark { COUNT_DISC_DARK } else { COUNT_DISC_LIGHT };
            let _ = write!(svg, r#"<circle cx="{cx}" cy="{cy}" r="{}" fill="{}"/>"#, badge.small / 2.0, disc.to_hex());
            count = Some(Count { more: cards.len() - 1, x: bx, y: by, diameter: badge.small });
        }
    }
    svg.push_str("</svg>");
    Some(CardsPicture { svg, width, height, inset, slot, count })
}

/// How many pixels square the picture of a mark `size` across is: room
/// for the tick, so a row keeps its width whatever its thread is doing.
pub fn mark_side(size: f32) -> f32 {
    (size.round() * TICK_SCALE).round()
}

/// A thread's mark `size` pixels across as a picture `mark_side(size)`
/// square, the mark in its middle: the grid of pixels, or the tick of a
/// result waiting to be seen. `step` is the clock's for a mark that
/// moves; `color` is what `tone` came to, `alpha` of the way there.
pub fn mark_svg(mark: Mark, step: Option<u64>, size: f32, color: Rgb, alpha: f32) -> String {
    let size = size.round().max(1.0);
    let side = mark_side(size);
    let mut svg = format!(r#"<svg xmlns="http://www.w3.org/2000/svg" width="{side}" height="{side}" viewBox="0 0 {side} {side}">"#);
    if mark == Mark::Finished {
        glyph_svg(&mut svg, "t", tick_svg(), false, 0.0, 0.0, side, color, alpha);
    } else {
        let grid = pixel_grid(size);
        let corner = ((side - size) / 2.0 + grid.inset).round();
        for index in 0..9 {
            let (column, row) = ((index % 3) as f32, (index / 3) as f32);
            let _ = write!(
                svg,
                r#"<rect x="{}" y="{}" width="{cell}" height="{cell}" rx="{}" fill="{}" fill-opacity="{}"/>"#,
                corner + column * (grid.cell + grid.gap),
                corner + row * (grid.cell + grid.gap),
                grid.radius,
                color.to_hex(),
                alpha * pixel_alpha(mark, step, index),
                cell = grid.cell,
            );
        }
    }
    svg.push_str("</svg>");
    svg
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_working_mark_runs_round_the_edge_and_leaves_the_middle_half_lit() {
        assert_eq!(pixel_alpha(Mark::Working, Some(0), 0), 1.0);
        assert_eq!(pixel_alpha(Mark::Working, Some(1), 1), 1.0);
        assert_eq!(pixel_alpha(Mark::Working, Some(1), 0), 0.8);
        // Three steps in, the run is down the right side.
        assert_eq!(pixel_alpha(Mark::Working, Some(3), 5), 1.0);
        assert_eq!(pixel_alpha(Mark::Working, Some(3), 3), UNLIT);
        assert_eq!(pixel_alpha(Mark::Working, Some(7), 4), 0.5);
        assert_eq!(pixel_alpha(Mark::Working, Some(8), 0), 1.0);
    }

    #[test]
    fn still_marks_light_a_plus_a_cross_or_every_pixel() {
        let lit = |mark| (0..9).filter(|&i| pixel_alpha(mark, None, i) == 1.0).collect::<Vec<_>>();
        assert_eq!(lit(Mark::Still), PLUS);
        assert_eq!(lit(Mark::Waiting), PLUS);
        assert_eq!(lit(Mark::Failed), CROSS);
        assert_eq!(lit(Mark::Finished), (0..9).collect::<Vec<_>>());
    }

    #[test]
    fn the_status_colours_are_the_ones_on_show() {
        assert_eq!(srgb(WORKING).to_hex(), "#61B8FF");
        assert_eq!(srgb(WAITING).to_hex(), "#EFB361");
        assert_eq!(srgb(FINISHED).to_hex(), "#7CE5A2");
        // Deeper on a light ground, the same on a dark one.
        assert_eq!(status_srgb(FINISHED, true), srgb(FINISHED));
        assert_eq!(status_srgb(FINISHED, false).to_hex(), "#34C759");
        assert_eq!(status_srgb(WORKING, false).to_hex(), "#007AFF");
        assert_eq!(status_srgb(WAITING, false).to_hex(), "#FF9500");
        // A failure is the desktop chrome's own red on either ground, and
        // the linear value is that red's.
        assert_eq!(status_srgb(DANGER, true), DANGER_DARK);
        assert_eq!(status_srgb(DANGER, false), DANGER_LIGHT);
        assert_eq!(srgb(DANGER), DANGER_DARK);
        assert_eq!(still_tone(true, true, true), Tone::Color(CURRENT));
        assert_eq!(still_tone(false, false, true), Tone::Text(CLOSED_ALPHA));
        assert_eq!(still_tone(false, true, true), Tone::Text(UNREAD_ALPHA));
        assert_eq!(still_tone(false, true, false), Tone::Color(OPEN));
        assert_eq!(status_color(Mark::Working, true), Some(CURRENT));
        assert_eq!(status_color(Mark::Working, false), Some(WORKING));
        assert_eq!(status_color(Mark::Waiting, true), Some(WAITING));
        assert_eq!(status_color(Mark::Still, false), None);
    }

    #[test]
    fn a_row_leads_with_its_agents_and_ends_with_the_terminal() {
        assert!(row_rank(true, false) < row_rank(false, false));
        assert!(row_rank(false, false) < row_rank(false, true));
        // A terminal card someone gave to an agent is the agent's.
        assert_eq!(row_rank(true, true), row_rank(true, false));
    }

    #[test]
    fn a_plate_is_lit_in_srgb() {
        let grey = Rgb(0x80, 0x80, 0x80);
        assert_eq!(lighten(grey, 0.5), Rgb(0xC0, 0xC0, 0xC0));
        assert_eq!(darken(grey, 0.5), Rgb(0x40, 0x40, 0x40));
        assert_eq!(lighten(Rgb(0x37, 0x76, 0xAB), 0.0), Rgb(0x37, 0x76, 0xAB));
    }

    fn card(circle: &str) -> WireCard {
        WireCard {
            id: "x".into(),
            circle: circle.into(),
            glyph: "#FFFFFF".into(),
            svg: r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><path d="M2 2h20"/></svg>"#.into(),
            full_color: false,
            scale: 1.0,
            programs: Vec::new(),
            agents: Vec::new(),
        }
    }

    #[test]
    fn a_row_s_icons_are_one_picture() {
        assert!(cards_svg(&[], 40.0, 2.0, true, true).is_none());
        let (a, b) = (card("#112233"), card("#445566"));
        let one = cards_svg(&[&a], 40.0, 2.0, true, true).unwrap();
        // A shadow seven pixels soft that drops five: 26 pixels of room.
        assert_eq!((one.inset, one.slot, one.width, one.height), (26.0, 54.0, 106.0, 92.0));
        assert!(one.count.is_none() && !one.svg.contains(r#"id="c""#));
        // Nothing of the glyph's own text ends an attribute.
        assert!(one.svg.contains("data:image/svg+xml,%3Csvg") && !one.svg.contains("href=\"<"));
        let two = cards_svg(&[&a, &b], 40.0, 2.0, true, true).unwrap();
        assert_eq!(two.slot, 54.0);
        assert!(two.svg.contains(r#"id="c""#) && two.svg.contains(&lighten(Rgb(0x44, 0x55, 0x66), 0.18).to_hex()));
        let many = cards_svg(&[&a, &b, &a, &b], 40.0, 2.0, false, true).unwrap();
        let count = many.count.unwrap();
        assert_eq!((count.more, count.diameter), (3, 22.0));
        assert!(many.svg.contains(&COUNT_DISC_LIGHT.to_hex()));
        let bare = cards_svg(&[&a], 40.0, 2.0, true, false).unwrap();
        assert!(!bare.svg.contains("<image"));
    }

    #[test]
    fn a_mark_is_a_grid_or_a_tick_in_the_same_square() {
        let color = srgb(WORKING);
        let grid = mark_svg(Mark::Working, Some(0), 20.0, color, 1.0);
        assert_eq!(grid.matches("<rect").count(), 9);
        assert!(grid.contains(r#"width="30""#) && grid.contains(r#"fill-opacity="0.18""#));
        let tick = mark_svg(Mark::Finished, None, 20.0, srgb(FINISHED), 1.0);
        assert!(tick.contains(r#"width="30""#) && tick.contains("<image"));
    }
}
