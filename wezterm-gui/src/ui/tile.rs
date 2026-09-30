//! Lit tiles: a coloured shape with its fill lit from above, a rim that
//! bevels its edge and a soft shadow under it. One painter for every icon
//! that sits on one -- the circles heading pane tabs, the rounded squares
//! heading settings rows -- so they are lit alike; a circle is a tile whose
//! corner radius is half its side.
//!
//! Nothing here is a texture of its own: the fill and rim are gradient quads
//! over the atlas's shared corner sprites, and the shadow is the atlas's
//! blurred silhouette for the tile's radius, so a page of tiles costs a few
//! quads each and no memory per tile.

use crate::quad::TripleLayerQuadAllocator;
use crate::ui::draw::DrawContext;
use window::color::{LinearRgba, SrgbaTuple};

/// How a tile is lit. Every amount is how far the tile's colour is blended
/// toward white (`lighten`) or black (`darken`), in sRGB, as a designer's
/// tool would.
pub(crate) struct TileStyle {
    pub fill_top_lighten: f32,
    pub fill_bottom_darken: f32,
    pub rim_top_lighten: f32,
    pub rim_bottom_darken: f32,
    /// The rim's width against the tile's side.
    pub rim_per_side: f32,
    /// The shadow's colour, as the tile's darkened this far (1 is black).
    pub shadow_darken: f32,
    /// Its peak opacity on dark chrome and on light, where the same shadow
    /// shows far more.
    pub shadow_alpha_dark: f32,
    pub shadow_alpha_light: f32,
    /// Its softness and how far it drops, in design pixels.
    pub shadow_sigma: f32,
    pub shadow_drop: f32,
}

/// Linear light to sRGB, one channel. Not `LinearRgba::to_srgb`: that one
/// switches to the power curve at 0.04045, the cut-off on the sRGB side
/// rather than the linear one (0.0031308), so a channel whose sRGB byte is
/// below about 57 comes back far too light.
fn to_srgb(linear: f32) -> f32 {
    if linear <= 0.0031308 {
        linear * 12.92
    } else {
        linear.powf(1.0 / 2.4) * 1.055 - 0.055
    }
}

/// `color` taken `amount` of the way to `target`, blended in sRGB.
fn blend(color: LinearRgba, target: f32, amount: f32) -> LinearRgba {
    let toward = |linear: f32| {
        let channel = to_srgb(linear);
        channel + (target - channel) * amount
    };
    SrgbaTuple(toward(color.0), toward(color.1), toward(color.2), color.3).to_linear()
}

pub(crate) fn lighten(color: LinearRgba, amount: f32) -> LinearRgba {
    blend(color, 1.0, amount)
}

pub(crate) fn darken(color: LinearRgba, amount: f32) -> LinearRgba {
    blend(color, 0.0, amount)
}

/// A tile `side` pixels square at `(x, y)` with corners of `radius`,
/// coloured `color` and lit by `style`. `dark` is the chrome's appearance.
#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_tile(
    ctx: &DrawContext,
    layers: &mut TripleLayerQuadAllocator,
    layer: usize,
    x: f32,
    y: f32,
    side: f32,
    radius: f32,
    color: LinearRgba,
    dark: bool,
    style: &TileStyle,
) -> anyhow::Result<()> {
    let bounds = euclid::rect(x, y, side, side);
    let shadow_alpha = if dark {
        style.shadow_alpha_dark
    } else {
        style.shadow_alpha_light
    };
    ctx.draw_shadow(
        layers,
        layer,
        bounds,
        radius,
        ctx.px(style.shadow_sigma),
        ctx.px(style.shadow_drop),
        darken(color, style.shadow_darken).mul_alpha(shadow_alpha),
    )?;
    ctx.draw_rounded_rect_vertical_gradient(
        layers,
        layer,
        bounds,
        radius,
        lighten(color, style.rim_top_lighten),
        darken(color, style.rim_bottom_darken),
    )?;
    let rim = (side * style.rim_per_side).round().max(1.0);
    let inner = (side - rim * 2.0).max(0.0);
    ctx.draw_rounded_rect_vertical_gradient(
        layers,
        layer,
        euclid::rect(x + rim, y + rim, inner, inner),
        (radius - rim).max(0.0),
        lighten(color, style.fill_top_lighten),
        darken(color, style.fill_bottom_darken),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn srgb_bytes(color: LinearRgba) -> (u8, u8, u8) {
        let byte = |linear: f32| (to_srgb(linear) * 255.0).round() as u8;
        (byte(color.0), byte(color.1), byte(color.2))
    }

    #[test]
    fn blending_happens_in_srgb() {
        let grey = LinearRgba::with_srgba(0x80, 0x80, 0x80, 0xFF);
        // Halfway to white in sRGB is 0xC0, not what a linear blend gives.
        assert_eq!(srgb_bytes(lighten(grey, 0.5)), (0xC0, 0xC0, 0xC0));
        assert_eq!(srgb_bytes(darken(grey, 0.5)), (0x40, 0x40, 0x40));
        assert_eq!(srgb_bytes(lighten(grey, 0.0)), (0x80, 0x80, 0x80));
    }

    #[test]
    fn dark_channels_keep_their_value() {
        // 0x37 is in the range the shared conversion gets wrong: it came
        // back as 0x7E and turned a blue icon lavender.
        let blue = LinearRgba::with_srgba(0x37, 0x76, 0xAB, 0xFF);
        assert_eq!(srgb_bytes(lighten(blue, 0.0)), (0x37, 0x76, 0xAB));
        let dim = LinearRgba::with_srgba(0x31, 0x31, 0x31, 0xFF);
        assert_eq!(srgb_bytes(lighten(dim, 0.5)), (0x98, 0x98, 0x98));
        let dimmer = LinearRgba::with_srgba(0x32, 0x32, 0x32, 0xFF);
        assert_eq!(srgb_bytes(darken(dimmer, 0.5)), (0x19, 0x19, 0x19));
    }
}
