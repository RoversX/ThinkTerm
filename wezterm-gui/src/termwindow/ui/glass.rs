//! Glass for the chrome's selected capsules -- the sidebar row on show, the
//! active tab in either tab row -- after Liquid Glass as the iPhone app
//! draws the same rows.
//!
//! The edge is what makes it glass (`TermWindow::paint_glass_edge`), so the
//! capsule loses the drop shadow and the even hairline it had: on a dark
//! ground a light along the top and bottom that fades out round the ends and
//! a dark line outside them; on a light ground a grey outline, darkest at the
//! ends, with white inside it along the top and bottom. The numbers are read
//! off screenshots of the iPhone's rows. High contrast keeps the shadow and
//! the hairline under the glass (`TermWindow::glass_high_contrast`).

use crate::quad::{QuadTrait, TripleLayerQuadAllocator};
use crate::termwindow::render::corners::{
    BOTTOM_LEFT_ROUNDED_CORNER_OUTLINE, BOTTOM_RIGHT_ROUNDED_CORNER_OUTLINE,
    TOP_LEFT_ROUNDED_CORNER_OUTLINE, TOP_RIGHT_ROUNDED_CORNER_OUTLINE,
};
use crate::termwindow::ui::sidebar::snapped_rounded_corner_radius;
use window::color::LinearRgba;
use window::{Appearance, RectF};

/// How a glass capsule's edge is drawn: on a dark ground or a light one.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum GlassEdge {
    Dark,
    Light,
}

impl GlassEdge {
    /// The glass for a capsule on `ground`: the side of what it sits on,
    /// not the appearance's. An interface following the terminal takes the
    /// scheme's side, and a colour the user named for a bar can put it on
    /// either.
    pub(crate) fn over(ground: LinearRgba) -> Self {
        match crate::ui::UiPalette::appearance_of(ground) {
            Appearance::Light | Appearance::LightHighContrast => Self::Light,
            Appearance::Dark | Appearance::DarkHighContrast => Self::Dark,
        }
    }

    /// The capsule's fill under this glass, from the one it would have had
    /// on `ground`. In the light it is no lighter than the iPhone's glass
    /// (`LIGHT_GLASS_CEILING`): the white along its edge needs room above it
    /// to show, and a capsule filled whiter than that -- the sidebar's row --
    /// comes back to it by being more see-through, which keeps its tint.
    pub(crate) fn fill(self, fill: LinearRgba, ground: LinearRgba) -> LinearRgba {
        match self {
            Self::Dark => fill,
            Self::Light => {
                let lightness = |c: LinearRgba| {
                    let c = c.to_srgb();
                    0.2126 * c.0 + 0.7152 * c.1 + 0.0722 * c.2
                };
                let (over, under) = (lightness(fill), lightness(ground));
                // Blended as the chrome blends, in sRGB.
                let seen = under + fill.3 * (over - under);
                if seen <= LIGHT_GLASS_CEILING || over <= under {
                    fill
                } else {
                    let alpha = (LIGHT_GLASS_CEILING - under) / (over - under);
                    LinearRgba(fill.0, fill.1, fill.2, alpha.clamp(0.0, fill.3))
                }
            }
        }
    }
}

/// How a ring of a glass edge runs round the pill.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum GlassRing {
    /// Along the top and bottom, fading out round each end.
    Lit,
    /// Round the ends only, strongest at the very side.
    Ends,
    /// All the way round, evenly.
    Even,
}

/// The lightest a glass capsule's fill comes out in the light, in sRGB: the
/// iPhone's, 242 of 255 through the middle of its selected row.
const LIGHT_GLASS_CEILING: f32 = 242.0 / 255.0;
/// The grey outline of a glass pill in the light, as the alpha of black: the
/// iPhone's is 218 of 255 along the top, over its fill of 242.
const LIGHT_GLASS_OUTLINE: f32 = 0.1;
/// How much lighter the lower half of a glass capsule is in the light, as
/// the alpha of white at its bottom edge, none at its top: the iPhone's row
/// is 246 through its lower half against 242 through the middle.
const LIGHT_GLASS_LIFT: f32 = 0.4;
/// What the outline darkens by at the very ends, on top of that: the
/// iPhone's goes to about 160 there.
const LIGHT_GLASS_OUTLINE_ENDS: f32 = 0.27;

/// A profile across a glass edge: straight lines between `knots` of (points
/// from the edge, alpha), nothing past the last.
fn glass_profile(knots: &[(f32, f32)], depth: f32) -> f32 {
    knots
        .windows(2)
        .find(|pair| depth < pair[1].0)
        .map_or(0.0, |pair| {
            let ((from, a), (to, b)) = (pair[0], pair[1]);
            let t = ((depth - from) / (to - from)).clamp(0.0, 1.0);
            a + (b - a) * t
        })
}

/// The light on a glass pill's edge in the dark, `depth` points in, as the
/// alpha of white over the pill: the line on the edge, then a glow that is
/// gone a few points in.
///
/// Read off the iPhone's selected row in the dark, which sits on the same
/// ground as the sidebar (18 of 255): its fill is about 49, the line on its
/// edge 116 and two thirds of a point wide, and the glow 59 a point in, 55
/// two points in and back to the fill by four or five.
fn glass_light_on_dark(depth: f32) -> f32 {
    glass_profile(
        &[
            (0.0, 0.31),
            (0.6, 0.31),
            (1.0, 0.06),
            (2.0, 0.03),
            (3.0, 0.018),
            (4.5, 0.0),
        ],
        depth,
    )
}

/// How dark the line outside a glass pill's ends is in the dark, `depth`
/// points out from its edge, as the alpha of black over the ground: about
/// half a point of near black on the iPhone (2 to 8 of 255 on a ground of
/// 18), softening over the next half point.
fn glass_shade_on_dark(depth: f32) -> f32 {
    glass_profile(
        &[(0.0, 0.8), (0.33, 0.8), (0.67, 0.45), (1.0, 0.1), (1.33, 0.0)],
        depth,
    )
}

/// The light inside a glass pill's outline in the light, `depth` points in
/// from the outline, as the alpha of white over the pill.
///
/// Read off the iPhone's selected row in the light: inside its outline the
/// pill is white (255) for over a point, then glows down to its fill, 242,
/// by about seven points -- 250 two points in, 246 four points in. Taken as
/// a share of the way from the fill to white, which `GlassEdge::fill` keeps
/// as the iPhone's.
fn glass_light_on_light(depth: f32) -> f32 {
    glass_profile(
        &[
            (0.0, 0.95),
            (1.3, 0.97),
            (1.6, 0.8),
            (2.3, 0.6),
            (3.2, 0.48),
            (4.0, 0.33),
            (5.2, 0.19),
            (6.5, 0.08),
            (7.2, 0.0),
        ],
        depth,
    )
}

/// A glass edge's `profile` averaged over a ring of pixels from `from` to
/// `to` points from the edge: a pixel wider than the line takes in some of
/// what is beside it too, so the line keeps the weight it has on the iPhone
/// at any scale.
fn across_pixel(profile: fn(f32) -> f32, from: f32, to: f32) -> f32 {
    const SAMPLES: usize = 8;
    let step = (to - from) / SAMPLES as f32;
    (0..SAMPLES)
        .map(|i| profile(from + step * (i as f32 + 0.5)))
        .sum::<f32>()
        / SAMPLES as f32
}

impl crate::TermWindow {
    /// Whether the chrome is in high contrast, which keeps a glass capsule's
    /// shadow, hairline and full fill under its edge.
    pub(crate) fn glass_high_contrast(&self) -> bool {
        matches!(
            self.chrome_appearance,
            Appearance::DarkHighContrast | Appearance::LightHighContrast
        )
    }

    /// A glass capsule's edge, as Liquid Glass on the iPhone has it, drawn
    /// over its fill: `rect` with corners of `radius`, on the ground `edge`
    /// was chosen for.
    ///
    /// In the dark: light on the very edge along the top and bottom, fading
    /// out round each end, a fainter glow just inside it, and a dark line
    /// outside each end where the light has gone. In the light: a grey
    /// outline all round, darkest at the ends, and inside it white along the
    /// top and bottom, glowing a few points into the fill and fading out
    /// round each end. `dark` leaves out the dark parts.
    ///
    /// Round an end the light dims with how far the edge has turned from
    /// level. Across a corner that is how far in from the pill's side the
    /// edge is, so each corner's ring takes a gradient across it: clear at
    /// the side, full where it meets the straight edge, and the dark the
    /// other way round. The iPhone's own rows, measured, fade round their
    /// ends the same way.
    ///
    /// Drawn as rings one pixel apart, each as strong as the light or the
    /// dark is that far from the edge (`glass_light_on_dark` and the rest),
    /// so the edge keeps its size in points. The outline in the light is the
    /// one outermost pixel at any scale, to stay a crisp hairline.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn paint_glass_edge(
        &self,
        layers: &mut TripleLayerQuadAllocator,
        layer_num: usize,
        rect: RectF,
        radius: f32,
        edge: GlassEdge,
        dark: bool,
    ) -> anyhow::Result<()> {
        let radius = snapped_rounded_corner_radius(rect, radius);
        // Pixels to a point: the chrome is authored in 2x backing pixels.
        let pixel = self.ui_f32(2.0);
        if !(pixel > 0.0) || radius < 1.0 {
            return Ok(());
        }
        let white = |alpha: f32| LinearRgba::with_components(1.0, 1.0, 1.0, alpha);
        let black = |alpha: f32| LinearRgba::with_components(0.0, 0.0, 0.0, alpha);
        // Where the light starts, in pixels in from the edge.
        let mut light_from = 0.0_f32;
        let light: fn(f32) -> f32 = match edge {
            GlassEdge::Dark => {
                let mut outset = 1.0_f32;
                while dark {
                    let alpha =
                        across_pixel(glass_shade_on_dark, (outset - 1.0) / pixel, outset / pixel);
                    if alpha <= 0.0 {
                        break;
                    }
                    self.paint_glass_edge_ring(
                        layers,
                        layer_num,
                        rect,
                        radius,
                        -outset,
                        black(alpha),
                        GlassRing::Ends,
                    )?;
                    outset += 1.0;
                }
                glass_light_on_dark
            }
            GlassEdge::Light => {
                // The lower half a little lighter, as light gathers there.
                if let Some(gl_state) = self.render_state.as_ref() {
                    crate::ui::draw::DrawContext::new(
                        gl_state,
                        self.dimensions,
                        &self.render_metrics,
                    )
                    .draw_rounded_rect_vertical_gradient(
                        layers,
                        layer_num,
                        rect,
                        radius,
                        white(0.0),
                        white(LIGHT_GLASS_LIFT),
                    )?;
                }
                if dark {
                    for (alpha, ring) in [
                        (LIGHT_GLASS_OUTLINE, GlassRing::Even),
                        (LIGHT_GLASS_OUTLINE_ENDS, GlassRing::Ends),
                    ] {
                        self.paint_glass_edge_ring(
                            layers,
                            layer_num,
                            rect,
                            radius,
                            0.0,
                            black(alpha),
                            ring,
                        )?;
                    }
                    light_from = 1.0;
                }
                glass_light_on_light
            }
        };
        let mut inset = light_from;
        loop {
            let depth = (inset - light_from) / pixel;
            let alpha = across_pixel(light, depth, depth + 1.0 / pixel);
            if radius - inset < 1.0 || alpha <= 0.0 {
                break;
            }
            self.paint_glass_edge_ring(
                layers,
                layer_num,
                rect,
                radius,
                inset,
                white(alpha),
                GlassRing::Lit,
            )?;
            inset += 1.0;
        }
        Ok(())
    }

    /// One ring of a glass edge, a pixel wide, `inset` pixels in from the
    /// edge of `rect` (out from it when negative), in `color`, running round
    /// the pill as `ring` says.
    #[allow(clippy::too_many_arguments)]
    fn paint_glass_edge_ring(
        &self,
        layers: &mut TripleLayerQuadAllocator,
        layer_num: usize,
        rect: RectF,
        radius: f32,
        inset: f32,
        color: LinearRgba,
        ring: GlassRing,
    ) -> anyhow::Result<()> {
        let r = radius - inset;
        let x = rect.min_x() + inset;
        let y = rect.min_y() + inset;
        let width = rect.width() - inset * 2.0;
        let height = rect.height() - inset * 2.0;
        let full = color;
        let clear = color.mul_alpha(0.0);
        if ring != GlassRing::Ends {
            let straight = (width - r * 2.0).max(0.0);
            for line_y in [y, y + height - 1.0] {
                self.filled_rectangle(
                    layers,
                    layer_num,
                    euclid::rect(x + r, line_y, straight, 1.0),
                    full,
                )?;
            }
        }
        if ring != GlassRing::Lit {
            // The sides between the corners, when the pill is taller than
            // its two corners.
            let upright = (height - r * 2.0).max(0.0);
            for line_x in [x, x + width - 1.0] {
                self.filled_rectangle(
                    layers,
                    layer_num,
                    euclid::rect(line_x, y + r, 1.0, upright),
                    full,
                )?;
            }
        }
        // The colour at the pill's side and where the straight edge begins;
        // the vertex colours carry it across each corner.
        let (side, inner) = match ring {
            GlassRing::Lit => (clear, full),
            GlassRing::Ends => (full, clear),
            GlassRing::Even => (full, full),
        };
        let size = euclid::size2(r, r);
        for (polys, corner_x, corner_y, left) in [
            (TOP_LEFT_ROUNDED_CORNER_OUTLINE, x, y, true),
            (TOP_RIGHT_ROUNDED_CORNER_OUTLINE, x + width - r, y, false),
            (BOTTOM_LEFT_ROUNDED_CORNER_OUTLINE, x, y + height - r, true),
            (
                BOTTOM_RIGHT_ROUNDED_CORNER_OUTLINE,
                x + width - r,
                y + height - r,
                false,
            ),
        ] {
            let mut quad = self.poly_quad(
                layers,
                layer_num,
                euclid::point2(corner_x, corner_y),
                polys,
                0,
                size,
                full,
            )?;
            quad.set_grayscale();
            if left {
                quad.set_corner_gradient(side, inner, side, inner);
            } else {
                quad.set_corner_gradient(inner, side, inner, side);
            }
        }
        Ok(())
    }
}
