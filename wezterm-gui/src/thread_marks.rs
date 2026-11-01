//! What a sidebar thread row shows of its panes: the icons of the programs
//! they run, laid out as Settings says, and the mark at the row's end that
//! says what the thread is doing. Settings draws its choices with the same
//! painters, so what it offers is what the sidebar will show.
//!
//! The icons are the tab icons -- the same cards, picked for a pane the way
//! its tab picks one; only how several of them share a row is new here.

use crate::native_settings::NativeThreadIcons;
use crate::quad::{QuadTrait, TripleLayerQuadAllocator, TripleLayerQuadAllocatorTrait};
use crate::tab_icons::{self, Card, Catalog, GlyphKey, ResolvedIcon, RgbExt as _, TERMINAL_CARD};
use crate::termwindow::ui::tokens::TAB_ICON_GLYPH_RATIO;
use crate::ui::draw::{DrawContext, ShapedText};
use anyhow::Result;
use mux::Mux;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use wezterm_font::LoadedFont;
use window::bitmaps::TextureRect;
use window::color::LinearRgba;

/// The layer everything here is drawn on, in order, so a later icon covers
/// an earlier one.
const LAYER: usize = 2;

/// How long the icons worked out for every thread are trusted when no
/// program said it changed: panes open and close without saying so.
const ICONS_TTL: Duration = Duration::from_secs(1);

/// The marks step on this clock, a little slower than the spinner's 80ms:
/// a spinner turning at the same time costs some repaints of its own.
pub(crate) const MARK_STEP: Duration = Duration::from_millis(100);

/// Steps in one turn of a working mark.
const MARK_STEPS: u64 = 8;

static GENERATION: AtomicU64 = AtomicU64::new(0);

/// A pane started or stopped a program, or an agent came or went: the
/// icons worked out for the threads are stale.
pub(crate) fn programs_changed() {
    GENERATION.fetch_add(1, Ordering::Relaxed);
}

struct Cached {
    generation: u64,
    at: Instant,
    catalog: Rc<Catalog>,
    by_workspace: Rc<HashMap<String, Vec<ResolvedIcon>>>,
}

thread_local! {
    /// One entry, replaced whenever it is rebuilt.
    static CACHE: RefCell<Option<Cached>> = RefCell::new(None);
    /// The counts as shaped, cleared when full.
    static SHAPED: RefCell<ShapedText> = RefCell::new(ShapedText::default());
}

/// The icons of the programs each workspace's panes run, by workspace:
/// agents first, then other programs, the terminal last, and each card
/// once however many panes wear it.
pub(crate) fn icons_by_workspace() -> Rc<HashMap<String, Vec<ResolvedIcon>>> {
    let catalog = tab_icons::catalog();
    let generation = GENERATION.load(Ordering::Relaxed);
    CACHE.with(|slot| {
        let mut slot = slot.borrow_mut();
        if let Some(cached) = slot.as_ref() {
            if cached.generation == generation
                && Rc::ptr_eq(&cached.catalog, &catalog)
                && cached.at.elapsed() < ICONS_TTL
            {
                return Rc::clone(&cached.by_workspace);
            }
        }
        let by_workspace = Rc::new(build(&catalog));
        *slot = Some(Cached {
            generation,
            at: Instant::now(),
            catalog,
            by_workspace: Rc::clone(&by_workspace),
        });
        by_workspace
    })
}

fn rank(card: &Card) -> u8 {
    if !card.agents.is_empty() {
        0
    } else if card.id == TERMINAL_CARD {
        2
    } else {
        1
    }
}

fn build(catalog: &Catalog) -> HashMap<String, Vec<ResolvedIcon>> {
    let mut by_workspace: HashMap<String, Vec<(u8, String, ResolvedIcon)>> = HashMap::new();
    let mux = Mux::get();
    for window_id in mux.iter_windows() {
        let Some(window) = mux.get_window(window_id) else {
            continue;
        };
        let workspace = window.get_workspace().to_string();
        let tabs = window.iter().cloned().collect::<Vec<_>>();
        drop(window);
        let cards = by_workspace.entry(workspace).or_default();
        for tab in tabs {
            for pane in tab.iter_all_panes() {
                let agent = pane.agent_status().filter(|status| !status.ended);
                let card = catalog.card_for(
                    agent.as_ref().map(|status| status.agent_id.as_str()),
                    pane.foreground_program().as_ref(),
                );
                if !cards.iter().any(|(_, id, _)| *id == card.id) {
                    cards.push((rank(card), card.id.clone(), card.resolved()));
                }
            }
        }
    }
    by_workspace
        .into_iter()
        .map(|(workspace, mut cards)| {
            cards.sort_by_key(|(rank, _, _)| *rank);
            (
                workspace,
                cards.into_iter().map(|(_, _, icon)| icon).collect(),
            )
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Icons.

/// The width of the slot a layout keeps at the row's start for icons
/// `diameter` across; the trailing layout keeps none.
pub(crate) fn slot_width(layout: NativeThreadIcons, diameter: f32) -> f32 {
    match layout {
        NativeThreadIcons::Badge => diameter * 1.33,
        NativeThreadIcons::Coins => diameter * 1.35,
        NativeThreadIcons::Cluster => diameter * 1.2,
        NativeThreadIcons::Trailing => 0.0,
    }
}

/// How icons are drawn on a row: what sets one off from another, and what
/// a count is written in.
pub(crate) struct IconStyle<'a> {
    /// The row's colour, ringed round an icon where it covers another.
    pub surface: LinearRgba,
    pub dark: bool,
    pub count_font: &'a Rc<LoadedFont>,
    pub count_metrics: &'a crate::utilsprites::RenderMetrics,
    /// "+N" after trailing icons.
    pub count_color: LinearRgba,
}

/// Draw `icons` in the slot at `x` (see [`slot_width`]) centred on `cy`;
/// the trailing layout is [`draw_trailing`]. An empty list draws nothing.
pub(crate) fn draw_slot(
    ctx: &DrawContext,
    layers: &mut TripleLayerQuadAllocator,
    layout: NativeThreadIcons,
    icons: &[ResolvedIcon],
    x: f32,
    cy: f32,
    diameter: f32,
    style: &IconStyle,
) -> Result<()> {
    let Some(first) = icons.first() else {
        return Ok(());
    };
    let d = diameter;
    let gap = (d * 0.08).round().max(1.0);
    match layout {
        NativeThreadIcons::Badge | NativeThreadIcons::Trailing => {
            let y = cy - d / 2.0;
            draw_icon(ctx, layers, first, x, y, d, style, true, 0.0)?;
            // Out to the right rather than down, so it stays within the row.
            let small = (d * 0.55).round();
            let (bx, by) = (x + d * 0.78, y + d * 0.55);
            match icons.len() {
                1 => {}
                2 => draw_icon(ctx, layers, &icons[1], bx, by, small, style, true, gap)?,
                count => draw_count(ctx, layers, count - 1, bx, by, small, style, gap)?,
            }
        }
        NativeThreadIcons::Coins => {
            let y = cy - d / 2.0;
            let shown = icons.len().min(3);
            for (index, icon) in icons[..shown].iter().enumerate().rev() {
                let ring = if index + 1 < shown { gap } else { 0.0 };
                let offset = d * 0.175 * index as f32;
                draw_icon(ctx, layers, icon, x + offset, y, d, style, index == 0, ring)?;
            }
        }
        NativeThreadIcons::Cluster => {
            let side = d * 1.2;
            let top = cy - side / 2.0;
            match icons.len() {
                1 => draw_icon(ctx, layers, first, x + (side - d) / 2.0, cy - d / 2.0, d, style, true, 0.0)?,
                2 => {
                    let s = (d * 0.75).round();
                    draw_icon(ctx, layers, &icons[1], x + side - s, top + side - s, s, style, true, 0.0)?;
                    draw_icon(ctx, layers, first, x, top, s, style, true, gap)?;
                }
                count => {
                    let s = (d * 0.65).round();
                    if count == 3 {
                        draw_icon(ctx, layers, &icons[2], x, top + side - s, s, style, true, 0.0)?;
                    } else {
                        draw_count(ctx, layers, count - 2, x, top + side - s, s, style, 0.0)?;
                    }
                    draw_icon(ctx, layers, &icons[1], x + side - s, top + side - s, s, style, true, gap)?;
                    draw_icon(ctx, layers, first, x + (side - s) / 2.0, top, s, style, true, gap)?;
                }
            }
        }
    }
    Ok(())
}

/// Draw `icons` side by side ending at `right`, the first on top, at most
/// three and a "+N" for the rest. Returns where they start.
pub(crate) fn draw_trailing(
    ctx: &DrawContext,
    layers: &mut TripleLayerQuadAllocator,
    icons: &[ResolvedIcon],
    right: f32,
    cy: f32,
    diameter: f32,
    style: &IconStyle,
) -> Result<f32> {
    if icons.is_empty() {
        return Ok(right);
    }
    let d = diameter;
    let gap = (d * 0.08).round().max(1.0);
    let shown = icons.len().min(3);
    let step = d * 0.75;
    let more = (icons.len() > shown).then(|| format!("+{}", icons.len() - shown));
    let more_width = more
        .as_ref()
        .map(|text| count_text_width(ctx, text, style) + d * 0.15)
        .unwrap_or(0.0);
    let left = (right - more_width - d - step * (shown - 1) as f32).round();
    let y = cy - d / 2.0;
    for (index, icon) in icons[..shown].iter().enumerate().rev() {
        let ring = if index + 1 < shown { gap } else { 0.0 };
        draw_icon(ctx, layers, icon, left + step * index as f32, y, d, style, true, ring)?;
    }
    if let Some(text) = more {
        let x = left + d + step * (shown - 1) as f32 + d * 0.15;
        draw_count_text(ctx, layers, &text, x, cy, style.count_color, style)?;
    }
    Ok(left)
}

/// One icon: its plate, its glyph unless `glyph` is false, and a ring of
/// the row's colour `ring` wide round it when it covers another.
#[allow(clippy::too_many_arguments)]
fn draw_icon(
    ctx: &DrawContext,
    layers: &mut TripleLayerQuadAllocator,
    icon: &ResolvedIcon,
    x: f32,
    y: f32,
    d: f32,
    style: &IconStyle,
    glyph: bool,
    ring: f32,
) -> Result<()> {
    if ring > 0.0 {
        ctx.draw_disc(layers, LAYER, x + d / 2.0, y + d / 2.0, d + ring * 2.0, style.surface)?;
    }
    crate::ui::tile::draw_tile(
        ctx,
        layers,
        LAYER,
        x,
        y,
        d,
        d / 2.0,
        icon.circle.linear(),
        style.dark,
        &tab_icons::PLATE,
    )?;
    if glyph {
        let size = (d * TAB_ICON_GLYPH_RATIO).round();
        tab_icons::draw_glyph(
            ctx,
            layers,
            LAYER,
            &icon.glyph,
            x + (d - size) / 2.0,
            y + (d - size) / 2.0,
            size,
            icon.glyph_color.linear(),
        )?;
    }
    Ok(())
}

/// One icon on its own, `alpha` of the way in: the sidebar's jump pill
/// fades the icon of the thread it stands for in and out with itself.
#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_alone(
    ctx: &DrawContext,
    layers: &mut TripleLayerQuadAllocator,
    icon: &ResolvedIcon,
    x: f32,
    y: f32,
    d: f32,
    dark: bool,
    alpha: f32,
) -> Result<()> {
    crate::ui::tile::draw_tile(
        ctx,
        layers,
        LAYER,
        x,
        y,
        d,
        d / 2.0,
        icon.circle.linear().mul_alpha(alpha),
        dark,
        &tab_icons::PLATE,
    )?;
    let size = (d * TAB_ICON_GLYPH_RATIO).round();
    tab_icons::draw_glyph(
        ctx,
        layers,
        LAYER,
        &icon.glyph,
        x + (d - size) / 2.0,
        y + (d - size) / 2.0,
        size,
        icon.glyph_color.linear().mul_alpha(alpha),
    )
}

/// A neutral disc saying how many more programs there are.
#[allow(clippy::too_many_arguments)]
fn draw_count(
    ctx: &DrawContext,
    layers: &mut TripleLayerQuadAllocator,
    count: usize,
    x: f32,
    y: f32,
    d: f32,
    style: &IconStyle,
    ring: f32,
) -> Result<()> {
    let (cx, cy) = (x + d / 2.0, y + d / 2.0);
    if ring > 0.0 {
        ctx.draw_disc(layers, LAYER, cx, cy, d + ring * 2.0, style.surface)?;
    }
    let disc = if style.dark {
        LinearRgba::with_srgba(72, 72, 74, 255)
    } else {
        LinearRgba::with_srgba(142, 142, 147, 255)
    };
    ctx.draw_disc(layers, LAYER, cx, cy, d, disc)?;
    let text = count.min(99).to_string();
    let width = count_text_width(ctx, &text, style);
    draw_count_text(
        ctx,
        layers,
        &text,
        (cx - width / 2.0).round(),
        cy,
        LinearRgba::with_components(1.0, 1.0, 1.0, 1.0),
        style,
    )
}

/// How wide `text` is in the count font. Counts are shaped once and kept,
/// being drawn every frame.
fn count_text_width(ctx: &DrawContext, text: &str, style: &IconStyle) -> f32 {
    let ctx = ctx.with_metrics(style.count_metrics);
    SHAPED.with(|shaped| {
        ctx.measure_text_width_shaped(style.count_font, text, &mut shaped.borrow_mut())
    })
}

/// `text` in the count font from `x`, its figures centred on `cy`.
#[allow(clippy::too_many_arguments)]
fn draw_count_text(
    ctx: &DrawContext,
    layers: &mut TripleLayerQuadAllocator,
    text: &str,
    x: f32,
    cy: f32,
    color: LinearRgba,
    style: &IconStyle,
) -> Result<()> {
    let ctx = ctx.with_metrics(style.count_metrics);
    SHAPED.with(|shaped| {
        let mut shaped = shaped.borrow_mut();
        let width = ctx.measure_text_width_shaped(style.count_font, text, &mut shaped);
        ctx.draw_text_shaped_on_layer(
            layers,
            LAYER,
            style.count_font,
            x,
            text_top(style.count_metrics, cy),
            text,
            color,
            width + 1.0,
            &mut shaped,
        )
    })
}

/// Where text in `metrics` goes for its digits to sit centred on `cy`.
fn text_top(metrics: &crate::utilsprites::RenderMetrics, cy: f32) -> f32 {
    let baseline = metrics.cell_size.height as f32 + metrics.descender.get() as f32;
    // Figures stand about seven tenths of the ascent tall.
    (cy - baseline + baseline * 0.35).round()
}

// ---------------------------------------------------------------------------
// Marks.

/// The one set of colours a thread's or an agent's state is shown in,
/// wherever it is shown -- the sidebar, the thread search, the Agents
/// panel -- so a colour means the same thing everywhere. An error is the
/// chrome's own danger red.
pub(crate) const WORKING_COLOR: LinearRgba = LinearRgba::with_components(0.12, 0.48, 1.0, 1.0);
/// Waiting on the user: amber, a question rather than something wrong.
pub(crate) const WAITING_COLOR: LinearRgba = LinearRgba::with_components(0.86, 0.45, 0.12, 1.0);
/// A finished result nobody has looked at yet.
pub(crate) const FINISHED_COLOR: LinearRgba = LinearRgba::with_components(0.20, 0.78, 0.36, 1.0);

/// The thread the user is in, at rest or working. It never has an unseen
/// result, being looked at, so the green cannot be mistaken for one.
pub(crate) const CURRENT_COLOR: LinearRgba = FINISHED_COLOR;

/// Open and idle: the blue of working -- a working mark moves, an idle
/// one does not.
pub(crate) const OPEN_COLOR: LinearRgba = WORKING_COLOR;

/// Open with output nobody has seen: brighter than idle.
pub(crate) fn unread_color(text: LinearRgba) -> LinearRgba {
    text.mul_alpha(0.85)
}

/// Saved but not open.
pub(crate) fn closed_color(text: LinearRgba) -> LinearRgba {
    text.mul_alpha(0.16)
}

/// What a thread's mark says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mark {
    Working,
    Waiting,
    Failed,
    Finished,
    /// Anything else: active, unread, open, closed -- told apart by colour.
    Still,
}

/// Steps of [`MARK_STEP`] since the epoch, so every working mark in every
/// window steps together.
pub(crate) fn mark_step() -> u64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    (now.as_millis() / MARK_STEP.as_millis()) as u64
}

/// When the working marks next step.
pub(crate) fn next_mark_step() -> Instant {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let step = MARK_STEP.as_millis();
    let into = now.as_millis() % step;
    Instant::now() + Duration::from_millis((step - into) as u64)
}

/// Pixels lit by a still mark, row by row.
const PLUS: [usize; 5] = [1, 3, 4, 5, 7];
const CROSS: [usize; 5] = [0, 2, 4, 6, 8];
/// The ring a working mark runs round, clockwise from the top left.
const RING: [usize; 8] = [0, 1, 2, 5, 8, 7, 6, 3];
/// How lit a pixel of a working mark is, by steps since the run passed it.
const TRAIL: [f32; 4] = [1.0, 0.8, 0.6, 0.39];
const UNLIT: f32 = 0.18;

/// A three-by-three grid of pixels in the `size` square at `x`, `y`.
/// `step` is [`mark_step`] for a mark that moves, `None` for a still
/// picture of one.
#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_pixels(
    ctx: &DrawContext,
    layers: &mut TripleLayerQuadAllocator,
    mark: Mark,
    step: Option<u64>,
    x: f32,
    y: f32,
    size: f32,
    color: LinearRgba,
) -> Result<()> {
    // Three pixels and two gaps a little wider than the square, centred
    // on it; corners round enough to show at this size, as drawing snaps
    // a radius to whole pixels.
    let cell = (size * 0.34).round().max(2.0);
    let gap = (size * 0.1).round().max(1.0);
    let grid = cell * 3.0 + gap * 2.0;
    let left = (x + (size - grid) / 2.0).round();
    let top = (y + (size - grid) / 2.0).round();
    for index in 0..9 {
        let alpha = pixel_alpha(mark, step, index);
        let (column, row) = ((index % 3) as f32, (index / 3) as f32);
        ctx.draw_rounded_rect(
            layers,
            LAYER,
            left + column * (cell + gap),
            top + row * (cell + gap),
            cell,
            cell,
            color.mul_alpha(alpha),
            cell * 0.3,
        )?;
    }
    Ok(())
}

fn pixel_alpha(mark: Mark, step: Option<u64>, index: usize) -> f32 {
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

/// How high a dot of a working mark is lifted, by steps since the wave
/// reached it.
const LIFT: [f32; 3] = [0.5, 1.0, 0.5];

/// Three dots rising in turn while working, like someone typing; one dot
/// otherwise, in the `size` square at `x`, `y`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_dots(
    ctx: &DrawContext,
    layers: &mut TripleLayerQuadAllocator,
    mark: Mark,
    step: Option<u64>,
    x: f32,
    y: f32,
    size: f32,
    color: LinearRgba,
) -> Result<()> {
    let dot = (size * 0.24).round().max(2.0);
    let cy = y + size / 2.0;
    if mark != Mark::Working {
        return ctx.draw_disc(layers, LAYER, x + size / 2.0, cy, dot, color);
    }
    let gap = ((size - dot * 3.0) / 2.0).max(1.0);
    let left = x + (size - dot * 3.0 - gap * 2.0) / 2.0;
    for index in 0..3u64 {
        let lift = dot_lift(step, index);
        ctx.draw_disc(
            layers,
            LAYER,
            left + dot / 2.0 + (dot + gap) * index as f32,
            cy - lift * size * 0.15,
            dot,
            color.mul_alpha(0.4 + 0.6 * lift),
        )?;
    }
    Ok(())
}

fn dot_lift(step: Option<u64>, index: u64) -> f32 {
    // A still picture shows the wave at the middle dot.
    let step = step.unwrap_or(3);
    let since = (step + MARK_STEPS - (index * 2 + 1) % MARK_STEPS) % MARK_STEPS;
    LIFT.get(since as usize).copied().unwrap_or(0.0)
}

/// A heartbeat trace drawn left to right while working, a flat line
/// otherwise, in the `size` square at `x`, `y`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_pulse(
    ctx: &DrawContext,
    layers: &mut TripleLayerQuadAllocator,
    mark: Mark,
    step: Option<u64>,
    x: f32,
    y: f32,
    size: f32,
    color: LinearRgba,
) -> Result<()> {
    if mark != Mark::Working {
        let thickness = ctx.px(3.2).round().max(1.0);
        let width = (size * 0.85).round();
        return ctx.draw_rounded_rect(
            layers,
            LAYER,
            (x + (size - width) / 2.0).round(),
            (y + (size - thickness) / 2.0).round(),
            width,
            thickness,
            color,
            thickness / 2.0,
        );
    }
    // Drawn out from a third of the way, never from nothing: a trace that
    // has barely begun reads as no mark at all.
    let drawn = match step {
        Some(step) => (0.35 + 0.65 * (step % MARK_STEPS) as f32 / 4.0).min(1.0),
        None => 1.0,
    };
    let glyph = GlyphKey::Builtin(thinkterm_tab_icons::BuiltinGlyph::Activity);
    let sprite = ctx
        .render_state
        .glyph_cache
        .borrow_mut()
        .cached_tab_glyph(&glyph, size.round() as usize)?;
    let Some(sprite) = sprite else {
        return Ok(());
    };
    let coords = sprite.texture_coords();
    let coords = TextureRect::new(
        coords.origin,
        euclid::size2(coords.size.width * drawn, coords.size.height),
    );
    let left_offset = ctx.dimensions.pixel_width as f32 / 2.0;
    let top_offset = ctx.dimensions.pixel_height as f32 / 2.0;
    let mut quad = layers.allocate(LAYER)?;
    quad.set_position(
        x - left_offset,
        y - top_offset,
        x + size * drawn - left_offset,
        y + size - top_offset,
    );
    quad.set_texture(coords);
    quad.set_fg_color(color);
    quad.set_alt_color_and_mix_value(color, 0.0);
    quad.set_hsv(None);
    quad.set_has_color(false);
    quad.set_grayscale();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_working_mark_runs_round_the_edge_and_leaves_the_middle_half_lit() {
        assert_eq!(pixel_alpha(Mark::Working, Some(0), 0), 1.0);
        assert_eq!(pixel_alpha(Mark::Working, Some(1), 1), 1.0);
        assert_eq!(pixel_alpha(Mark::Working, Some(1), 0), 0.8);
        // Step 3 is at the top right's neighbour below it.
        assert_eq!(pixel_alpha(Mark::Working, Some(3), 5), 1.0);
        assert_eq!(pixel_alpha(Mark::Working, Some(3), 3), UNLIT);
        assert_eq!(pixel_alpha(Mark::Working, Some(7), 4), 0.5);
        assert_eq!(pixel_alpha(Mark::Working, Some(8), 0), 1.0);
    }

    #[test]
    fn working_dots_rise_one_after_another() {
        // Each dot peaks two steps after the one before it.
        assert_eq!(dot_lift(Some(2), 0), 1.0);
        assert_eq!(dot_lift(Some(4), 1), 1.0);
        assert_eq!(dot_lift(Some(6), 2), 1.0);
        assert_eq!(dot_lift(Some(2), 2), 0.0);
    }

    #[test]
    fn still_marks_light_a_plus_a_cross_or_every_pixel() {
        let lit = |mark| (0..9).filter(|&i| pixel_alpha(mark, None, i) == 1.0).collect::<Vec<_>>();
        assert_eq!(lit(Mark::Still), PLUS);
        assert_eq!(lit(Mark::Waiting), PLUS);
        assert_eq!(lit(Mark::Failed), CROSS);
        assert_eq!(lit(Mark::Finished), (0..9).collect::<Vec<_>>());
    }
}
