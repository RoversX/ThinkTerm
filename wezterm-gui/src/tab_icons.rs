//! The icon heading each pane tab: a coloured circle carrying a one-colour
//! glyph, chosen by what the pane runs -- as the desktop draws and edits it.
//!
//! The cards themselves, what the user's settings make of them and which
//! card a pane gets are `thinkterm_tab_icons`, shared with the browser
//! client so a page dresses a tab exactly as the desktop does. This module
//! keeps what is the desktop's own: the catalog the settings describe now,
//! rasterizing a glyph, importing an SVG, drawing the plate, and editing
//! the cards.
//!
//! A card's glyph is only ever a shape. Built-in marks and imported SVGs are
//! rasterized to their coverage and tinted with the card's glyph colour, so
//! every icon in the row belongs to the same family whatever it was drawn in.

use crate::native_settings::{self, NativeTabIconCard, NativeTabIconSettings};
use crate::quad::{QuadTrait, TripleLayerQuadAllocator, TripleLayerQuadAllocatorTrait};
use crate::ui::draw::DrawContext;
use crate::ui::tile::{draw_tile, TileStyle};
use anyhow::{anyhow, bail, Context, Result};
use mux::pane::Pane;
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
pub(crate) use thinkterm_tab_icons::{
    normalize_program_name, Card, Catalog, GlyphKey, ResolvedIcon, Rgb,
    CIRCLE_PRESETS, GLYPH_PRESETS, MAX_CUSTOM_CARDS, TERMINAL_CARD,
};
use thinkterm_tab_icons::{is_custom_id, is_svg_id, BUILTIN_CARDS, NEW_CARD_CIRCLES};
use window::color::LinearRgba;
use window::{BitmapImage, Image};

/// Largest SVG accepted for a card. Icons are a few kilobytes; anything
/// much bigger is a drawing, not an icon, and would be parsed on every
/// rasterization.
pub(crate) const MAX_SVG_BYTES: u64 = 256 * 1024;

/// A card's colour as this side draws it.
pub(crate) trait RgbExt {
    fn linear(self) -> LinearRgba;
}

impl RgbExt for Rgb {
    fn linear(self) -> LinearRgba {
        LinearRgba::with_srgba(self.0, self.1, self.2, 255)
    }
}

thread_local! {
    /// The catalog built from the settings last read, rebuilt when they are
    /// replaced (every save swaps the shared settings for a new `Arc`).
    static CATALOG: RefCell<Option<(Arc<native_settings::ThinkTermNativeSettings>, Rc<Catalog>)>> =
        RefCell::new(None);
}

/// The catalog as the settings currently describe it.
pub(crate) fn catalog() -> Rc<Catalog> {
    let settings = native_settings::load_shared();
    let (catalog, rebuilt) = CATALOG.with(|slot| {
        let mut slot = slot.borrow_mut();
        if let Some((built_from, catalog)) = slot.as_ref() {
            if Arc::ptr_eq(built_from, &settings) {
                return (Rc::clone(catalog), false);
            }
        }
        let catalog = Rc::new(Catalog::build(&settings.tab_icons, crate::i18n::tr));
        *slot = Some((settings, Rc::clone(&catalog)));
        (catalog, true)
    });
    if rebuilt {
        // The settings can change without `set_enabled` -- edited by hand,
        // reloaded from disk, restored from a backup -- and the observer's
        // switch has to follow the one the painters read.
        mux::foreground_program::refresh_enabled();
    }
    catalog
}

/// How to draw the icon of the tab showing `pane`, or `None` when the user
/// turned tab icons off and the tab keeps its plain terminal mark.
pub(crate) fn resolve(pane: &dyn Pane) -> Option<ResolvedIcon> {
    let catalog = catalog();
    catalog.enabled.then(|| {
        let agent = pane.agent_status().filter(|status| !status.ended);
        catalog
            .card_for(
                agent.as_ref().map(|status| status.agent_id.as_str()),
                pane.foreground_program().as_ref(),
            )
            .resolved()
    })
}

/// How to draw a window tab's icon: always the terminal card, as a window
/// tab holds panes that may each run something else. `None` when the user
/// turned tab icons off.
pub(crate) fn resolve_window_tab() -> Option<ResolvedIcon> {
    let catalog = catalog();
    catalog.enabled.then(|| catalog.terminal_card().resolved())
}


// ---------------------------------------------------------------------------
// Glyphs.

/// Where imported SVGs are kept, next to the settings that name them.
pub(crate) fn icon_dir() -> PathBuf {
    native_settings::settings_path().with_file_name("tab-icons")
}

fn svg_path(id: &str) -> PathBuf {
    icon_dir().join(format!("{id}.svg"))
}

/// The glyph as a white mask `size` pixels square, for the painter to tint.
pub(crate) fn rasterize_glyph(key: &GlyphKey, size: usize) -> Result<Image> {
    match key {
        GlyphKey::Builtin(glyph) => rasterize_mask(glyph.bytes(), size),
        GlyphKey::Svg(id) => {
            let path = svg_path(id);
            let bytes = read_bounded(&path)?;
            rasterize_mask(&bytes, size)
        }
    }
}

fn read_bounded(path: &Path) -> Result<Vec<u8>> {
    use std::io::Read;
    let file = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut bytes = Vec::new();
    file.take(MAX_SVG_BYTES + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("reading {}", path.display()))?;
    if bytes.len() as u64 > MAX_SVG_BYTES {
        bail!("{} is larger than {} bytes", path.display(), MAX_SVG_BYTES);
    }
    Ok(bytes)
}

/// Keep the shape, drop the colours: every pixel becomes white at its own
/// coverage (the atlas is premultiplied, so that is all four channels set to
/// alpha), and the painter tints it.
fn rasterize_mask(svg: &[u8], size: usize) -> Result<Image> {
    let size = size.max(1);
    let mut options = resvg::usvg::Options::default();
    // An icon is a shape: nothing it names outside itself is ever fetched.
    options.image_href_resolver.resolve_string = Box::new(|_, _| None);
    let tree = resvg::usvg::Tree::from_data(svg, &options).context("parsing SVG")?;
    let svg_size = tree.size();
    let scale = (size as f32 / svg_size.width()).min(size as f32 / svg_size.height());
    let translate_x = (size as f32 - svg_size.width() * scale) / 2.0;
    let translate_y = (size as f32 - svg_size.height() * scale) / 2.0;
    let transform = resvg::tiny_skia::Transform::from_translate(translate_x, translate_y)
        .pre_scale(scale, scale);
    let mut pixmap = resvg::tiny_skia::Pixmap::new(size as u32, size as u32)
        .ok_or_else(|| anyhow!("allocating a {size}px glyph"))?;
    resvg::render(&tree, transform, &mut pixmap.as_mut());
    let mut data = pixmap.take();
    for pixel in data.chunks_exact_mut(4) {
        let alpha = pixel[3];
        pixel[0] = alpha;
        pixel[1] = alpha;
        pixel[2] = alpha;
    }
    Ok(Image::from_raw(size, size, data))
}

fn covers_anything(mask: &Image) -> bool {
    mask.pixel_data_slice()
        .chunks_exact(4)
        .any(|pixel| pixel[3] != 0)
}

/// Why an SVG was not taken, in words for the user.
#[derive(Debug)]
pub(crate) enum ImportError {
    TooLarge,
    NotAnIcon,
    Io(anyhow::Error),
}

impl ImportError {
    pub(crate) fn message(&self) -> String {
        match self {
            Self::TooLarge => crate::i18n::tr("tab-icons-import-too-large"),
            Self::NotAnIcon => crate::i18n::tr("tab-icons-import-not-svg"),
            Self::Io(_) => crate::i18n::tr("tab-icons-import-failed"),
        }
    }
}

/// Take a copy of the SVG at `path` into the icon directory, named by its
/// hash so the same file imported twice is kept once. The copy is what the
/// card uses: moving or deleting the original changes nothing.
pub(crate) fn import_svg(path: &Path) -> std::result::Result<String, ImportError> {
    let len = std::fs::metadata(path)
        .map_err(|err| ImportError::Io(err.into()))?
        .len();
    if len > MAX_SVG_BYTES {
        return Err(ImportError::TooLarge);
    }
    // The size was checked above; what can still go wrong is reading.
    let bytes = read_bounded(path).map_err(ImportError::Io)?;
    // It has to draw something: parse it and look for a single covered
    // pixel, so a file that is not SVG, or an SVG of nothing, is refused
    // here rather than showing up as an empty circle.
    let mask = rasterize_mask(&bytes, 64).map_err(|_| ImportError::NotAnIcon)?;
    if !covers_anything(&mask) {
        return Err(ImportError::NotAnIcon);
    }
    use sha2::Digest;
    let id: String = sha2::Sha256::digest(&bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let dir = icon_dir();
    std::fs::create_dir_all(&dir).map_err(|err| ImportError::Io(err.into()))?;
    let target = svg_path(&id);
    if !target.exists() {
        let tmp = target.with_extension("svg.tmp");
        std::fs::write(&tmp, &bytes).map_err(|err| ImportError::Io(err.into()))?;
        std::fs::rename(&tmp, &target).map_err(|err| ImportError::Io(err.into()))?;
    }
    Ok(id)
}

/// How long an imported SVG may sit unreferenced before a cleanup takes it.
/// An import writes its file on a worker and names it in the settings only
/// when it comes back; a save in between must not mistake it for litter.
const UNREFERENCED_SVG_GRACE: std::time::Duration = std::time::Duration::from_secs(60);

/// Delete imported SVGs no card names any more. Called after every change
/// that can drop one, so the directory never outgrows the cards.
fn remove_unreferenced_svgs(settings: &NativeTabIconSettings) {
    let Ok(entries) = std::fs::read_dir(icon_dir()) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(id) = path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_suffix(".svg"))
        else {
            continue;
        };
        if !is_svg_id(id) {
            continue;
        }
        let referenced = settings
            .cards
            .iter()
            .any(|card| card.svg.as_deref() == Some(id));
        let fresh = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .is_none_or(|age| age < UNREFERENCED_SVG_GRACE);
        if !referenced && !fresh {
            if let Err(err) = std::fs::remove_file(&path) {
                log::warn!(
                    "unable to remove unused tab icon {}: {err:#}",
                    path.display()
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Drawing, shared by the pane tabs and the settings page so both show the
// same icon.

/// How a tab icon's circle is lit: the fill lightens toward white at the
/// top and darkens toward black at the bottom, and the rim -- one point on
/// a 25pt icon -- does the same more strongly, so the edge reads as a bevel
/// in the circle's own colour. The shadow is in its hue too.
const PLATE: TileStyle = TileStyle {
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

/// The circle a glyph sits on. `dark` is the chrome's appearance.
#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_plate(
    ctx: &DrawContext,
    layers: &mut TripleLayerQuadAllocator,
    layer: usize,
    x: f32,
    y: f32,
    diameter: f32,
    circle: Rgb,
    dark: bool,
) -> Result<()> {
    draw_tile(
        ctx,
        layers,
        layer,
        x,
        y,
        diameter,
        diameter / 2.0,
        circle.linear(),
        dark,
        &PLATE,
    )
}

/// A card's glyph, `size` pixels square: its white mask from the glyph
/// cache, tinted. An imported SVG deleted from under its card draws
/// nothing -- the circle alone still says which card it is -- rather than
/// failing the frame; a full atlas does fail it, so the painter grows the
/// atlas and paints again.
#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_glyph(
    ctx: &DrawContext,
    layers: &mut TripleLayerQuadAllocator,
    layer: usize,
    glyph: &GlyphKey,
    x: f32,
    y: f32,
    size: f32,
    color: LinearRgba,
) -> Result<()> {
    if size < 1.0 {
        return Ok(());
    }
    let sprite = ctx
        .render_state
        .glyph_cache
        .borrow_mut()
        .cached_tab_glyph(glyph, size.round() as usize)?;
    let Some(sprite) = sprite else {
        return Ok(());
    };
    let sprite = sprite.texture_coords();
    let left_offset = ctx.dimensions.pixel_width as f32 / 2.0;
    let top_offset = ctx.dimensions.pixel_height as f32 / 2.0;
    let mut quad = layers.allocate(layer)?;
    quad.set_position(
        x - left_offset,
        y - top_offset,
        x + size - left_offset,
        y + size - top_offset,
    );
    quad.set_texture(sprite);
    quad.set_fg_color(color);
    quad.set_alt_color_and_mix_value(color, 0.0);
    quad.set_hsv(None);
    quad.set_has_color(false);
    quad.set_grayscale();
    Ok(())
}

// ---------------------------------------------------------------------------
// Changes, as the settings page makes them. Each reads the settings, changes
// them, saves them and cleans up after itself; the painters pick the result
// up through `catalog`, which notices the settings were replaced.

fn edit(change: impl FnOnce(&mut NativeTabIconSettings)) -> Result<()> {
    let settings = native_settings::update(|settings| {
        change(&mut settings.tab_icons);
        // A built-in card whose every field went back to the default no
        // longer needs an entry, and the terminal card never has one that
        // counts.
        settings.tab_icons.cards.retain(|card| {
            is_custom_id(&card.id)
                || (card.id != TERMINAL_CARD
                    && *card
                        != NativeTabIconCard {
                            id: card.id.clone(),
                            ..Default::default()
                        })
        });
    })?;
    remove_unreferenced_svgs(&settings.tab_icons);
    Ok(())
}

/// Change card `id`'s entry. The card has to still be there: an import or
/// a file picker that comes back after its card was deleted must not bring
/// the card back empty, and the terminal card takes no changes at all.
fn edit_card(id: &str, change: impl FnOnce(&mut NativeTabIconCard)) -> Result<()> {
    let exists = if is_custom_id(id) {
        native_settings::load_shared()
            .tab_icons
            .cards
            .iter()
            .any(|card| card.id == id)
    } else {
        id != TERMINAL_CARD && BUILTIN_CARDS.iter().any(|card| card.id == id)
    };
    if !exists {
        bail!("no tab icon card {id}");
    }
    edit(|settings| change(entry(settings, id)))
}

fn entry<'a>(settings: &'a mut NativeTabIconSettings, id: &str) -> &'a mut NativeTabIconCard {
    let index = match settings.cards.iter().position(|card| card.id == id) {
        Some(index) => index,
        None => {
            settings.cards.push(NativeTabIconCard {
                id: id.to_string(),
                ..Default::default()
            });
            settings.cards.len() - 1
        }
    };
    &mut settings.cards[index]
}

pub(crate) fn set_enabled(enabled: bool) -> Result<()> {
    edit(|settings| settings.enabled = (!enabled).then_some(false))?;
    mux::foreground_program::refresh_enabled();
    Ok(())
}

/// Whether tab icons are on; the foreground program observer asks this.
pub(crate) fn enabled() -> bool {
    native_settings::load_shared()
        .tab_icons
        .enabled
        .unwrap_or(true)
}

/// Add a card of the user's own, wearing `svg` if one was just imported,
/// and return its id -- or `None` when they already have as many as they
/// may. The card and its SVG go in one save: a separate save in between
/// would find the SVG referenced by nothing and clean it away.
pub(crate) fn create_card(svg: Option<String>) -> Result<Option<String>> {
    let catalog = catalog();
    let custom = catalog.cards.iter().filter(|card| !card.builtin).count();
    if custom >= MAX_CUSTOM_CARDS {
        return Ok(None);
    }
    let next = catalog
        .cards
        .iter()
        .filter_map(|card| card.id.strip_prefix("custom-")?.parse::<u32>().ok())
        .max()
        .map_or(1, |n| n + 1);
    let id = format!("custom-{next}");
    let circle = NEW_CARD_CIRCLES[custom % NEW_CARD_CIRCLES.len()];
    edit(|settings| {
        let card = entry(settings, &id);
        card.circle = Some(circle.to_hex());
        card.glyph = Some(circle.legible_glyph().to_hex());
        card.svg = svg;
    })?;
    Ok(Some(id))
}

pub(crate) fn delete_card(id: &str) -> Result<()> {
    if !is_custom_id(id) {
        bail!("{id} is built in and cannot be deleted");
    }
    edit(|settings| settings.cards.retain(|card| card.id != id))
}

/// Put a built-in card back as it shipped.
pub(crate) fn reset_card(id: &str) -> Result<()> {
    edit(|settings| settings.cards.retain(|card| card.id != id))
}

pub(crate) fn rename_card(id: &str, name: &str) -> Result<()> {
    if !is_custom_id(id) {
        bail!("{id} is built in and keeps its name");
    }
    let name = name.trim().to_string();
    edit_card(id, |card| card.name = (!name.is_empty()).then_some(name))
}

pub(crate) fn set_card_svg(id: &str, svg: String) -> Result<()> {
    edit_card(id, |card| card.svg = Some(svg))
}

/// Give the card its built-in glyph back (a card of the user's own goes
/// back to the plain one it started with).
pub(crate) fn clear_card_svg(id: &str) -> Result<()> {
    edit_card(id, |card| card.svg = None)
}

pub(crate) fn set_card_circle(id: &str, color: Rgb) -> Result<()> {
    edit_card(id, |card| card.circle = Some(color.to_hex()))
}

pub(crate) fn set_card_glyph_color(id: &str, color: Rgb) -> Result<()> {
    edit_card(id, |card| card.glyph = Some(color.to_hex()))
}

/// Give `program` to card `id`. A name belongs to one card at a time, so it
/// is taken off whichever card had it; the returned name is that card's,
/// for the page to say where it came from. The terminal card is not
/// looked at: its shells are the fallback, fixed, and any other card wins
/// a name from it.
pub(crate) fn add_program(id: &str, program: &str) -> Result<Option<String>> {
    let Some(program) = normalize_program_name(program) else {
        bail!("{program:?} is not a program name");
    };
    let catalog = catalog();
    let previous = catalog
        .cards
        .iter()
        .find(|card| {
            card.id != id
                && card.id != TERMINAL_CARD
                && card
                    .programs
                    .iter()
                    .any(|name| normalize_program_name(name).as_deref() == Some(&program))
        })
        .map(|card| (card.id.clone(), card.name.clone(), card.programs.clone()));
    let mut programs = catalog
        .card(id)
        .map(|card| card.programs.clone())
        .ok_or_else(|| anyhow!("no tab icon card {id}"))?;
    if !programs.iter().any(|name| name == &program) {
        programs.push(program.clone());
    }
    edit(|settings| {
        if let Some((other, _, other_programs)) = &previous {
            entry(settings, other).programs = Some(
                other_programs
                    .iter()
                    .filter(|name| normalize_program_name(name).as_deref() != Some(&program))
                    .cloned()
                    .collect(),
            );
        }
        entry(settings, id).programs = Some(programs);
    })?;
    Ok(previous.map(|(_, name, _)| name))
}

pub(crate) fn remove_program(id: &str, program: &str) -> Result<()> {
    let catalog = catalog();
    let programs: Vec<String> = catalog
        .card(id)
        .map(|card| card.programs.clone())
        .ok_or_else(|| anyhow!("no tab icon card {id}"))?
        .into_iter()
        .filter(|name| name != program)
        .collect();
    edit(|settings| entry(settings, id).programs = Some(programs))
}


#[cfg(test)]
mod tests {
    use super::*;
    use thinkterm_tab_icons::BuiltinGlyph;

    #[test]
    fn every_built_in_glyph_draws_a_shape() {
        for card in BUILTIN_CARDS {
            let mask = rasterize_mask(card.glyph.bytes(), 32).unwrap();
            assert!(covers_anything(&mask), "{} draws nothing", card.id);
        }
        rasterize_mask(BuiltinGlyph::Package.bytes(), 32).unwrap();
    }

    #[test]
    fn a_mask_is_white_at_the_shapes_coverage() {
        let svg = br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 4 4"><rect width="2" height="4" fill="#ff0000"/></svg>"##;
        let mask = rasterize_mask(svg, 4).unwrap();
        let pixels: Vec<&[u8]> = mask.pixel_data_slice().chunks_exact(4).collect();
        for pixel in &pixels {
            assert_eq!(
                (pixel[0], pixel[1], pixel[2]),
                (pixel[3], pixel[3], pixel[3])
            );
        }
        assert!(pixels.iter().any(|pixel| pixel[3] == 0xFF));
        assert!(pixels.iter().any(|pixel| pixel[3] == 0));
    }
}
