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
//!
//! The numbers are alphas as OpenGL mixes them, which mixes the sRGB values
//! themselves. WebGPU draws into an sRGB surface and mixes in linear light,
//! which takes a faint white over a dark capsule half as light again; there
//! each is turned into the alpha that comes out the same (`GlassMix`).

use crate::quad::{QuadBlend, QuadTrait, TripleLayerQuadAllocator, TripleLayerQuadAllocatorTrait};
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
    /// comes back to it by being more see-through, which keeps its tint. On
    /// a ground about as light as that glass already, it still comes out a
    /// little lighter than the ground (`LIGHT_GLASS_MIN_LIFT`), or keeps the
    /// fill it had where that is lighter still: made see-through down to the
    /// ceiling, it would vanish and leave the capsule only its outline.
    /// `linear` says the frame mixes in linear light (`GlassMix`), which the
    /// see-through-ness is worked out for.
    pub(crate) fn fill(self, fill: LinearRgba, ground: LinearRgba, linear: bool) -> LinearRgba {
        match self {
            Self::Dark => fill,
            Self::Light => {
                let (over, under) = (lightness(fill), lightness(ground));
                let most = LIGHT_GLASS_CEILING.max(under + LIGHT_GLASS_MIN_LIFT);
                // As the frame mixes them: the sRGB values, or the light.
                let (over, under, most) = if linear {
                    (decode(over), decode(under), decode(most))
                } else {
                    (over, under, most)
                };
                let seen = under + fill.3 * (over - under);
                if seen <= most || over <= under {
                    fill
                } else {
                    let alpha = (most - under) / (over - under);
                    LinearRgba(fill.0, fill.1, fill.2, alpha.clamp(0.0, fill.3))
                }
            }
        }
    }
}

/// How light a colour is, in sRGB, 0 to 1. Its own curve rather than
/// `LinearRgba::to_srgb`, which turns linear values below 0.04045 back with
/// the slope meant for sRGB ones and so takes a dark grey, a capsule's in the
/// dark, for one more than twice as light.
fn lightness(c: LinearRgba) -> f32 {
    0.2126 * encode(c.0) + 0.7152 * encode(c.1) + 0.0722 * encode(c.2)
}

/// An sRGB value as linear light.
fn decode(v: f32) -> f32 {
    if v <= 0.04045 {
        v / 12.92
    } else {
        ((v + 0.055) / 1.055).powf(2.4)
    }
}

/// Linear light as an sRGB value.
fn encode(v: f32) -> f32 {
    if v <= 0.0031308 {
        v * 12.92
    } else {
        1.055 * v.powf(1.0 / 2.4) - 0.055
    }
}

/// The alpha that puts `colour` over `under` -- sRGB values, 0 to 1 -- in a
/// frame that mixes in linear light, where mixing the sRGB values themselves
/// at `alpha`, as OpenGL does, puts it.
fn alpha_mixed_in_linear(alpha: f32, colour: f32, under: f32) -> f32 {
    let target = under + alpha * (colour - under);
    let (colour, under, target) = (decode(colour), decode(under), decode(target));
    if (colour - under).abs() < f32::EPSILON {
        return alpha;
    }
    ((target - under) / (colour - under)).clamp(0.0, 1.0)
}

/// How a glass edge's white and black mix with what is under them, so that
/// they come out as OpenGL draws them whichever way the frame mixes.
#[derive(Clone, Copy, Debug)]
struct GlassMix {
    /// The frame mixes in linear light: WebGPU's sRGB surface.
    linear: bool,
    /// How light the capsule shows, in sRGB: its fill over its ground.
    capsule: f32,
    /// How light the ground round it is, in sRGB.
    ground: f32,
}

impl GlassMix {
    /// For a capsule `fill`ed as drawn, on `ground`.
    fn new(linear: bool, fill: LinearRgba, ground: LinearRgba) -> Self {
        let (fill_lightness, ground_lightness) = (lightness(fill), lightness(ground));
        let capsule = if linear {
            let ground_light = decode(ground_lightness);
            encode(ground_light + fill.3 * (decode(fill_lightness) - ground_light))
        } else {
            ground_lightness + fill.3 * (fill_lightness - ground_lightness)
        };
        Self {
            linear,
            capsule,
            ground: ground_lightness,
        }
    }

    /// White at `alpha` over the capsule.
    fn light(&self, alpha: f32) -> LinearRgba {
        LinearRgba::with_components(1.0, 1.0, 1.0, self.alpha(alpha, 1.0, self.capsule))
    }

    /// Black at `alpha` over the capsule.
    fn shade(&self, alpha: f32) -> LinearRgba {
        LinearRgba::with_components(0.0, 0.0, 0.0, self.alpha(alpha, 0.0, self.capsule))
    }

    /// Black at `alpha` over the ground, outside the capsule.
    fn shade_outside(&self, alpha: f32) -> LinearRgba {
        LinearRgba::with_components(0.0, 0.0, 0.0, self.alpha(alpha, 0.0, self.ground))
    }

    fn alpha(&self, alpha: f32, colour: f32, under: f32) -> f32 {
        if self.linear {
            alpha_mixed_in_linear(alpha, colour, under)
        } else {
            alpha
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
/// How much lighter than its ground a glass capsule's fill comes out at
/// least in the light, in sRGB, where the ground is about as light as the
/// glass already: enough that the capsule still shows as one.
const LIGHT_GLASS_MIN_LIFT: f32 = 6.0 / 255.0;
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

/// Over a see-through window, how much more of what is behind the window
/// shows through a glass capsule in the dark than round it, as a share of
/// what shows round it. Read off a Mac's own Liquid Glass row in a
/// see-through sidebar: what is behind comes through it a fifth again as
/// strongly as through the sidebar.
const THROUGH_GLASS_CLEAR: f32 = 0.22;

/// The light a glass capsule's fill adds over a see-through window in the
/// dark, in sRGB. The Mac's row is 28 of 255 lighter than the sidebar round
/// it; clearing the glass takes it a little of the way, as what shows
/// through it is lighter than the sidebar's own dark.
const THROUGH_GLASS_LIFT: f32 = 26.0 / 255.0;

/// How much more of what is behind shows through a glass capsule's edge
/// than through its fill, `depth` points in, on top of
/// `THROUGH_GLASS_CLEAR`: on the Mac half again as much as round the row
/// on the very edge, still more than through the middle a point or two in.
fn glass_clear_through(depth: f32) -> f32 {
    glass_profile(
        &[
            (0.0, 0.33),
            (0.45, 0.3),
            (0.55, 0.2),
            (1.0, 0.12),
            (1.3, 0.06),
            (3.0, 0.05),
            (4.0, 0.0),
        ],
        depth,
    )
}

/// The light on a glass capsule's edge over a see-through window in the
/// dark, `depth` points in, in sRGB over its fill. On the Mac a line half a
/// point wide is 49 of 255 lighter than the middle, the next half point 28,
/// then a glow goes back down to the fill by six or seven points. Some of
/// that is what shows through the edge more than the middle
/// (`glass_clear_through`), lighter than the sidebar it replaces; this is
/// the rest.
fn glass_light_through(depth: f32) -> f32 {
    glass_profile(
        &[
            (0.0, 43.0),
            (0.4, 42.0),
            (0.6, 24.0),
            (0.9, 20.0),
            (1.1, 7.0),
            (1.5, 6.0),
            (2.0, 5.5),
            (3.0, 4.0),
            (4.0, 3.0),
            (5.0, 2.0),
            (6.0, 1.2),
            (7.0, 0.0),
        ],
        depth,
    ) / 255.0
}

/// How much of a see-through window's light glass capsule is a white veil
/// over what shows through: the Mac's row there is 3 or 4 of 255 lighter
/// than the sidebar round it, which is about 236, and what is behind shows
/// through it about 0.85 as strongly.
const THROUGH_LIGHT_GLASS_VEIL: f32 = 0.16;

/// The light added to a light glass capsule over a see-through window, in
/// sRGB, at its top and at its bottom, which hides nothing of what shows
/// through: the Mac's row is 2 or 3 of 255 lighter through its lower half
/// than its top, where what is behind shows hardly less.
const THROUGH_LIGHT_GLASS_LIFT_TOP: f32 = 0.0;
const THROUGH_LIGHT_GLASS_LIFT_BOTTOM: f32 = 5.0 / 255.0;

/// The light inside a light glass capsule's outline over a see-through
/// window, `depth` points in from the outline, as the alpha of white over
/// the capsule. On the Mac the line inside the outline is white for half a
/// point, nearly so for the next, then glows down to the veil by about five
/// points: less, and over less, than the iPhone's opaque row
/// (`glass_light_on_light`).
fn glass_light_through_light(depth: f32) -> f32 {
    glass_profile(
        &[
            (0.0, 1.0),
            (0.45, 1.0),
            (0.6, 0.9),
            (0.9, 0.8),
            (1.1, 0.45),
            (1.5, 0.36),
            (2.0, 0.3),
            (2.75, 0.22),
            (3.25, 0.14),
            (4.0, 0.11),
            (4.75, 0.05),
            (5.5, 0.0),
        ],
        depth,
    )
}

/// How dark a light glass capsule's shadow is along its bottom, `depth`
/// points out from its edge, as the alpha of black: on the Mac 9 of 255
/// under the row's ground of 234 just under it, gone by about seven points.
fn glass_shadow_below(depth: f32) -> f32 {
    glass_profile(
        &[
            (0.0, 0.04),
            (1.0, 0.035),
            (2.0, 0.028),
            (3.0, 0.018),
            (4.0, 0.012),
            (5.0, 0.01),
            (6.0, 0.005),
            (7.0, 0.0),
        ],
        depth,
    )
}

/// The same along the top, where the Mac's is a sixth as dark and gone by
/// about five points.
fn glass_shadow_above(depth: f32) -> f32 {
    glass_profile(
        &[
            (0.0, 0.008),
            (1.0, 0.005),
            (2.0, 0.0035),
            (3.0, 0.0025),
            (5.0, 0.0),
        ],
        depth,
    )
}

/// What a see-through window's frame holds under a glass capsule, and how
/// clearing and lighting it come out there (`TermWindow::paint_through_glass`).
///
/// The frame holds each colour times how solid it is, which the system then
/// lays over what is behind the window: so clearing some of it lets that
/// much more of what is behind show, and adding light to it lights that
/// without hiding any of it.
#[derive(Clone, Copy, Debug)]
struct ThroughGlass {
    /// The frame mixes in linear light: WebGPU's sRGB surface.
    linear: bool,
    /// How solid the ground under the capsule is.
    solid: f32,
    /// How light the ground's own colour is: in linear light if the frame
    /// mixes in it, sRGB if not.
    colour: f32,
}

impl ThroughGlass {
    fn new(linear: bool, ground: LinearRgba) -> Self {
        let colour = if linear {
            0.2126 * ground.0 + 0.7152 * ground.1 + 0.0722 * ground.2
        } else {
            lightness(ground)
        };
        Self {
            linear,
            solid: ground.3.clamp(0.0, 1.0),
            colour,
        }
    }

    /// How much of the ground to keep so that `more` again as much of what is
    /// behind the window shows through as round the capsule.
    fn kept(&self, more: f32) -> f32 {
        if self.solid <= f32::EPSILON {
            return 1.0;
        }
        ((1.0 - (1.0 + more) * (1.0 - self.solid)) / self.solid).clamp(0.0, 1.0)
    }

    /// What the frame holds where `kept` of the ground is left, in sRGB.
    fn held(&self, kept: f32) -> f32 {
        let held = self.colour * self.solid * kept;
        if self.linear {
            encode(held)
        } else {
            held
        }
    }

    /// A quad laid over where the frame holds `held` (sRGB) that comes out
    /// as the grey `level` (sRGB) laid over it at `alpha` does as the system
    /// mixes what is behind the window in: the sRGB values themselves. It is
    /// that solid, so it hides as much of what is behind. A frame mixing in
    /// linear light would make its colour come out lighter over a light
    /// ground, so there it is a darker grey: at most `level`, as mixing in
    /// linear light never comes out darker.
    fn over(&self, level: f32, alpha: f32, held: f32) -> LinearRgba {
        let alpha = alpha.clamp(0.0, 1.0);
        let grey = if self.linear && alpha > 0.0 {
            let seen = alpha * level + (1.0 - alpha) * held;
            ((decode(seen) - (1.0 - alpha) * decode(held)) / alpha).clamp(0.0, 1.0)
        } else {
            decode(level)
        };
        LinearRgba::with_components(grey, grey, grey, alpha)
    }

    /// Black laid over where the frame holds `held` (sRGB) that darkens it
    /// as black at `alpha` does when the sRGB values themselves are mixed. In
    /// a frame mixing in linear light that takes more black, which also
    /// hides a little more of what is behind the window than it would.
    fn shade(&self, alpha: f32, held: f32) -> LinearRgba {
        let alpha = alpha.clamp(0.0, 1.0);
        let alpha = if self.linear && held > 0.0 {
            (1.0 - decode((1.0 - alpha) * held) / decode(held)).clamp(0.0, 1.0)
        } else {
            alpha
        };
        LinearRgba::with_components(0.0, 0.0, 0.0, alpha)
    }

    /// The alpha of white that, added where the frame holds `held` (sRGB),
    /// makes it `lift` lighter in sRGB.
    fn light(&self, held: f32, lift: f32) -> f32 {
        if self.linear {
            (decode((held + lift).min(1.0)) - decode(held)).clamp(0.0, 1.0)
        } else {
            lift.clamp(0.0, 1.0)
        }
    }
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

    /// Whether this window's frame mixes colours in linear light: WebGPU's,
    /// when it draws into an sRGB surface, as it asks to. OpenGL's mixes the
    /// sRGB values themselves.
    pub(crate) fn glass_mixes_in_linear(&self) -> bool {
        self.webgpu
            .as_ref()
            .map_or(false, |webgpu| webgpu.config.borrow().format.is_srgb())
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
    /// round each end. `dark` leaves out the dark parts. `over_line` says a
    /// one-pixel line was drawn round the capsule under the glass -- a
    /// see-through window's frame, or high contrast's hairline -- which the
    /// glass then stays inside rather than painting over its top and bottom.
    /// `fill` is the capsule's fill as it was drawn, and `ground` what it
    /// sits on: what the edge is mixed with (`GlassMix`).
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
        over_line: bool,
        fill: LinearRgba,
        ground: LinearRgba,
    ) -> anyhow::Result<()> {
        let radius = snapped_rounded_corner_radius(rect, radius);
        // Pixels to a point: the chrome is authored in 2x backing pixels.
        let pixel = self.ui_f32(2.0);
        if !(pixel > 0.0) || radius < 1.0 {
            return Ok(());
        }
        let mix = GlassMix::new(self.glass_mixes_in_linear(), fill, ground);
        // Where the light starts, in pixels in from the edge: inside a line
        // drawn under the glass, which it would otherwise cover.
        let mut light_from = if over_line { 1.0_f32 } else { 0.0 };
        // What the lower half's lift covers: the same, so it leaves that
        // line's bottom as it is.
        let lifted = if over_line {
            euclid::rect(
                rect.min_x() + 1.0,
                rect.min_y() + 1.0,
                (rect.width() - 2.0).max(0.0),
                (rect.height() - 2.0).max(0.0),
            )
        } else {
            rect
        };
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
                        mix.shade_outside(alpha),
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
                        lifted,
                        (radius - light_from).max(0.0),
                        mix.light(0.0),
                        mix.light(LIGHT_GLASS_LIFT),
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
                            mix.shade(alpha),
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
                mix.light(alpha),
                GlassRing::Lit,
            )?;
            inset += 1.0;
        }
        Ok(())
    }

    /// Whether a glass capsule is drawn as glass that lets what is behind a
    /// see-through window through (`paint_through_glass`): over such a
    /// window, out of high contrast, which keeps its solid fill and frame.
    pub(crate) fn glass_lets_through(&self) -> bool {
        self.chrome_see_through() && !self.glass_high_contrast()
    }

    /// A glass capsule over a see-through window, as the Mac's Liquid Glass
    /// draws a selected row there: `rect` with corners of `radius`, on
    /// `ground` as that was painted, see-through-ness and all, with the glass
    /// `edge` chose for it.
    ///
    /// In the dark there is no fill laid over the ground. Some of the ground
    /// is cleared, so that what is behind the window shows through the
    /// capsule more than round it, and more again along its edge
    /// (`glass_clear_through`); then light is added, which lights what shows
    /// through without hiding it: over the whole capsule, and along the top
    /// and bottom edge in a line and a glow (`glass_light_through`). The
    /// edge's light and clearing fade round each end as the opaque glass's
    /// do, and outside each end there is the same dark line.
    ///
    /// In the light the capsule is a thin white veil, which still lets most
    /// of what is behind through (`THROUGH_LIGHT_GLASS_VEIL`), with light
    /// added that grows towards its bottom, the opaque glass's grey outline,
    /// a white line inside it and a short glow (`glass_light_through_light`),
    /// and a soft shadow round it, more below than above.
    pub(crate) fn paint_through_glass(
        &self,
        layers: &mut TripleLayerQuadAllocator,
        layer_num: usize,
        rect: RectF,
        radius: f32,
        edge: GlassEdge,
        ground: LinearRgba,
    ) -> anyhow::Result<()> {
        let radius = snapped_rounded_corner_radius(rect, radius);
        let pixel = self.ui_f32(2.0);
        if !(pixel > 0.0) || radius < 1.0 {
            return Ok(());
        }
        if edge == GlassEdge::Light {
            return self.paint_through_glass_light(layers, layer_num, rect, radius, pixel, ground);
        }
        let through = ThroughGlass::new(self.glass_mixes_in_linear(), ground);
        // A cleared quad's colour is not used. A renderer that cannot clear
        // lays it over instead, where the ground's own colour changes
        // nothing.
        let clearing = |alpha: f32| {
            LinearRgba::with_components(ground.0, ground.1, ground.2, alpha.clamp(0.0, 1.0))
        };
        let light = |alpha: f32| LinearRgba::with_components(1.0, 1.0, 1.0, alpha);
        // Each ring of the edge, in from it, with how much more shows
        // through there and how much lighter it is than the fill.
        let mut rings = vec![];
        let mut inset = 0.0_f32;
        while radius - inset >= 1.0 {
            let depth = inset / pixel;
            let next = depth + 1.0 / pixel;
            let clear = across_pixel(glass_clear_through, depth, next);
            let lift = across_pixel(glass_light_through, depth, next);
            if clear <= 0.0 && lift <= 0.0 {
                break;
            }
            rings.push((inset, clear, lift));
            inset += 1.0;
        }

        let kept = through.kept(THROUGH_GLASS_CLEAR);
        let result = (|| {
            layers.set_blend(layer_num, QuadBlend::Clear);
            self.fill_rounded_rectangle(layers, layer_num, rect, clearing(1.0 - kept), radius)?;
            for &(inset, clear, _) in &rings {
                let ring_kept = through.kept(THROUGH_GLASS_CLEAR + clear) / kept.max(f32::EPSILON);
                self.paint_glass_edge_ring(
                    layers,
                    layer_num,
                    rect,
                    radius,
                    inset,
                    clearing(1.0 - ring_kept.min(1.0)),
                    GlassRing::Lit,
                )?;
            }

            layers.set_blend(layer_num, QuadBlend::Add);
            let held = through.held(kept);
            self.fill_rounded_rectangle(
                layers,
                layer_num,
                rect,
                light(through.light(held, THROUGH_GLASS_LIFT)),
                radius,
            )?;
            for &(inset, clear, lift) in &rings {
                let held =
                    through.held(through.kept(THROUGH_GLASS_CLEAR + clear)) + THROUGH_GLASS_LIFT;
                self.paint_glass_edge_ring(
                    layers,
                    layer_num,
                    rect,
                    radius,
                    inset,
                    light(through.light(held, lift)),
                    GlassRing::Lit,
                )?;
            }
            anyhow::Ok(())
        })();
        // Whatever happened, what is painted after is laid over again.
        layers.set_blend(layer_num, QuadBlend::Over);
        result?;

        // The dark outside the ends, laid over what shows round the capsule:
        // mostly what is behind the window, which the system mixes as its
        // sRGB values, so the alphas go as they are.
        let mix = GlassMix::new(false, ground, ground);
        let mut outset = 1.0_f32;
        loop {
            let alpha = across_pixel(glass_shade_on_dark, (outset - 1.0) / pixel, outset / pixel);
            if alpha <= 0.0 {
                break;
            }
            self.paint_glass_edge_ring(
                layers,
                layer_num,
                rect,
                radius,
                -outset,
                mix.shade_outside(alpha),
                GlassRing::Ends,
            )?;
            outset += 1.0;
        }
        Ok(())
    }

    /// `paint_through_glass` in the light, with `radius` already snapped and
    /// `pixel` pixels to a point.
    fn paint_through_glass_light(
        &self,
        layers: &mut TripleLayerQuadAllocator,
        layer_num: usize,
        rect: RectF,
        radius: f32,
        pixel: f32,
        ground: LinearRgba,
    ) -> anyhow::Result<()> {
        // What the frame holds under the capsule as each coat goes on, in
        // sRGB, for laying the next one over it.
        let through = ThroughGlass::new(self.glass_mixes_in_linear(), ground);
        let held = through.held(1.0);
        let mut outset = 1.0_f32;
        loop {
            let (from, to) = ((outset - 1.0) / pixel, outset / pixel);
            let above = across_pixel(glass_shadow_above, from, to);
            let below = across_pixel(glass_shadow_below, from, to);
            if above <= 0.0 && below <= 0.0 {
                break;
            }
            self.paint_glass_shadow_ring(
                layers,
                layer_num,
                rect,
                radius,
                outset,
                through.shade(above, held),
                through.shade(below, held),
            )?;
            outset += 1.0;
        }

        self.fill_rounded_rectangle(
            layers,
            layer_num,
            rect,
            through.over(1.0, THROUGH_LIGHT_GLASS_VEIL, held),
            radius,
        )?;
        let held = THROUGH_LIGHT_GLASS_VEIL + (1.0 - THROUGH_LIGHT_GLASS_VEIL) * held;
        if let Some(gl_state) = self.render_state.as_ref() {
            let light =
                |lift: f32| LinearRgba::with_components(1.0, 1.0, 1.0, through.light(held, lift));
            layers.set_blend(layer_num, QuadBlend::Add);
            let result =
                crate::ui::draw::DrawContext::new(gl_state, self.dimensions, &self.render_metrics)
                    .draw_rounded_rect_vertical_gradient(
                        layers,
                        layer_num,
                        rect,
                        radius,
                        light(THROUGH_LIGHT_GLASS_LIFT_TOP),
                        light(THROUGH_LIGHT_GLASS_LIFT_BOTTOM),
                    );
            layers.set_blend(layer_num, QuadBlend::Over);
            result?;
        }
        let held = held + (THROUGH_LIGHT_GLASS_LIFT_TOP + THROUGH_LIGHT_GLASS_LIFT_BOTTOM) / 2.0;
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
                through.shade(alpha, held),
                ring,
            )?;
        }
        let mut inset = 1.0_f32;
        loop {
            let depth = (inset - 1.0) / pixel;
            let alpha = across_pixel(glass_light_through_light, depth, depth + 1.0 / pixel);
            if radius - inset < 1.0 || alpha <= 0.0 {
                break;
            }
            self.paint_glass_edge_ring(
                layers,
                layer_num,
                rect,
                radius,
                inset,
                through.over(1.0, alpha, held),
                GlassRing::Lit,
            )?;
            inset += 1.0;
        }
        Ok(())
    }

    /// One ring of a glass capsule's shadow, a pixel wide, `outset` pixels
    /// out from `rect`: `above` along its top, `below` along its bottom, and
    /// going from one to the other round each end.
    #[allow(clippy::too_many_arguments)]
    fn paint_glass_shadow_ring(
        &self,
        layers: &mut TripleLayerQuadAllocator,
        layer_num: usize,
        rect: RectF,
        radius: f32,
        outset: f32,
        above: LinearRgba,
        below: LinearRgba,
    ) -> anyhow::Result<()> {
        let r = radius + outset;
        let x = rect.min_x() - outset;
        let y = rect.min_y() - outset;
        let width = rect.width() + outset * 2.0;
        let height = rect.height() + outset * 2.0;
        let middle = LinearRgba::with_components(0.0, 0.0, 0.0, (above.3 + below.3) / 2.0);
        let straight = (width - r * 2.0).max(0.0);
        for (line_y, color) in [(y, above), (y + height - 1.0, below)] {
            self.filled_rectangle(
                layers,
                layer_num,
                euclid::rect(x + r, line_y, straight, 1.0),
                color,
            )?;
        }
        let upright = (height - r * 2.0).max(0.0);
        for line_x in [x, x + width - 1.0] {
            self.filled_rectangle(
                layers,
                layer_num,
                euclid::rect(line_x, y + r, 1.0, upright),
                middle,
            )?;
        }
        let size = euclid::size2(r, r);
        for (polys, corner_x, corner_y, top, bottom) in [
            (TOP_LEFT_ROUNDED_CORNER_OUTLINE, x, y, above, middle),
            (
                TOP_RIGHT_ROUNDED_CORNER_OUTLINE,
                x + width - r,
                y,
                above,
                middle,
            ),
            (
                BOTTOM_LEFT_ROUNDED_CORNER_OUTLINE,
                x,
                y + height - r,
                middle,
                below,
            ),
            (
                BOTTOM_RIGHT_ROUNDED_CORNER_OUTLINE,
                x + width - r,
                y + height - r,
                middle,
                below,
            ),
        ] {
            let mut quad = self.poly_quad(
                layers,
                layer_num,
                euclid::point2(corner_x, corner_y),
                polys,
                0,
                size,
                top,
            )?;
            quad.set_grayscale();
            quad.set_corner_gradient(top, top, bottom, bottom);
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

#[cfg(test)]
mod test {
    use super::*;

    /// What `fill` over `ground` comes out as, blended in sRGB as the
    /// chrome blends.
    fn seen(fill: LinearRgba, ground: LinearRgba) -> f32 {
        let (over, under) = (lightness(fill), lightness(ground));
        under + fill.3 * (over - under)
    }

    fn grey(level: u8) -> LinearRgba {
        LinearRgba::with_srgba(level, level, level, 255)
    }

    /// A see-through window's dark sidebar, as it is painted there.
    fn see_through_sidebar() -> LinearRgba {
        LinearRgba::with_srgba(18, 18, 20, 198)
    }

    #[test]
    fn through_glass_lets_as_much_more_through_as_it_is_asked() {
        for linear in [false, true] {
            let through = ThroughGlass::new(linear, see_through_sidebar());
            for more in [THROUGH_GLASS_CLEAR, THROUGH_GLASS_CLEAR + 0.33] {
                let shown = 1.0 - through.solid * through.kept(more);
                let round = 1.0 - through.solid;
                assert!(
                    (shown / round - (1.0 + more)).abs() < 1e-4,
                    "linear {}, more {}: {} against {}",
                    linear,
                    more,
                    shown,
                    round
                );
            }
        }
        // An opaque ground has nothing behind it to show, and a clear one
        // nothing to clear.
        assert_eq!(ThroughGlass::new(true, grey(18)).kept(0.5), 1.0);
        let clear = LinearRgba::with_srgba(18, 18, 20, 0);
        assert_eq!(ThroughGlass::new(true, clear).kept(0.5), 1.0);
    }

    #[test]
    fn through_glass_lays_white_and_black_over_as_the_system_mixes() {
        let sidebar = LinearRgba::with_srgba(227, 227, 230, 134);
        for linear in [false, true] {
            let through = ThroughGlass::new(linear, sidebar);
            let held = through.held(1.0);
            // What the frame holds after a quad is laid over, as it mixes.
            let after = |quad: LinearRgba| {
                if linear {
                    encode(quad.0 * quad.3 + (1.0 - quad.3) * decode(held))
                } else {
                    encode(quad.0) * quad.3 + (1.0 - quad.3) * held
                }
            };
            let veil = through.over(1.0, THROUGH_LIGHT_GLASS_VEIL, held);
            assert_eq!(veil.3, THROUGH_LIGHT_GLASS_VEIL, "as solid in either frame");
            let wanted = THROUGH_LIGHT_GLASS_VEIL + (1.0 - THROUGH_LIGHT_GLASS_VEIL) * held;
            assert!((after(veil) - wanted).abs() < 1e-4, "linear {}", linear);
            let line = through.shade(LIGHT_GLASS_OUTLINE, held);
            let wanted = (1.0 - LIGHT_GLASS_OUTLINE) * held;
            assert!((after(line) - wanted).abs() < 1e-4, "linear {}", linear);
        }
    }

    #[test]
    fn through_glass_light_lifts_the_frame_by_as_much_in_srgb() {
        for linear in [false, true] {
            let through = ThroughGlass::new(linear, see_through_sidebar());
            let held = through.held(through.kept(THROUGH_GLASS_CLEAR));
            let alpha = through.light(held, THROUGH_GLASS_LIFT);
            let after = if linear {
                encode(decode(held) + alpha)
            } else {
                held + alpha
            };
            assert!(
                (after - held - THROUGH_GLASS_LIFT).abs() < 1e-4,
                "linear {}: {} to {}",
                linear,
                held,
                after
            );
        }
    }

    /// The sidebar's selected row in the light: white, mostly opaque.
    fn row() -> LinearRgba {
        LinearRgba::with_components(1.0, 1.0, 1.0, 0.78)
    }

    #[test]
    fn on_the_usual_light_sidebar_the_fill_comes_out_as_light_as_the_iphones() {
        let ground = grey(226);
        let fill = GlassEdge::Light.fill(row(), ground, false);
        assert!(
            (seen(fill, ground) - LIGHT_GLASS_CEILING).abs() < 0.5 / 255.0,
            "{:?}",
            fill
        );
    }

    #[test]
    fn on_a_ground_lighter_than_the_glass_the_fill_does_not_vanish() {
        let ground = grey(250);
        let fill = GlassEdge::Light.fill(row(), ground, false);
        assert_eq!(fill, row());
        assert!(seen(fill, ground) > lightness(ground));
    }

    #[test]
    fn just_under_the_glass_the_fill_is_still_a_little_lighter_than_its_ground() {
        let ground = grey(240);
        let fill = GlassEdge::Light.fill(row(), ground, false);
        let lift = seen(fill, ground) - lightness(ground);
        assert!(
            (lift - LIGHT_GLASS_MIN_LIFT).abs() < 0.5 / 255.0,
            "{}",
            lift
        );
    }

    #[test]
    fn a_fill_darker_than_its_ground_and_the_dark_glass_keep_their_fill() {
        let tab = LinearRgba::with_srgba(238, 238, 238, 255);
        assert_eq!(GlassEdge::Light.fill(tab, grey(255), false), tab);
        assert_eq!(GlassEdge::Dark.fill(row(), grey(18), false), row());
    }

    /// What `colour` at `alpha` over `under` comes out as when mixed in
    /// linear light, as WebGPU mixes, back in sRGB.
    fn mixed_in_linear(alpha: f32, colour: f32, under: f32) -> f32 {
        encode(decode(under) + alpha * (decode(colour) - decode(under)))
    }

    #[test]
    fn in_linear_light_the_edge_comes_out_as_opengl_draws_it() {
        // A Mac's selected row in the dark, as measured under each renderer:
        // OpenGL 111 over a fill of 46, WebGPU 157 for the same alpha.
        let under = 46.0 / 255.0;
        assert!((mixed_in_linear(0.31, 1.0, under) * 255.0 - 157.0).abs() < 4.0);
        for alpha in [0.31, 0.21, 0.0525, 0.0375, 0.006] {
            for under in [18.0 / 255.0, 46.0 / 255.0, 0.5, 242.0 / 255.0] {
                for colour in [0.0, 1.0] {
                    let opengl = under + alpha * (colour - under);
                    let webgpu =
                        mixed_in_linear(alpha_mixed_in_linear(alpha, colour, under), colour, under);
                    assert!(
                        (webgpu - opengl).abs() < 0.5 / 255.0,
                        "{} of {} over {}: {} against {}",
                        alpha,
                        colour,
                        under,
                        webgpu,
                        opengl
                    );
                }
            }
        }
    }

    #[test]
    fn a_dark_capsule_is_as_dark_as_it_is_drawn() {
        // The sidebar's selected row in the dark: 54 of 255, nearly solid,
        // over a ground of 18.
        let fill = LinearRgba::with_srgba(54, 54, 57, 250);
        let mix = GlassMix::new(true, fill, grey(18));
        assert!((mix.capsule * 255.0 - 54.0).abs() < 1.0, "{:?}", mix);
        assert!((lightness(grey(18)) * 255.0 - 18.0).abs() < 0.5);
    }

    #[test]
    fn a_frame_mixing_srgb_values_keeps_the_alphas() {
        let mix = GlassMix::new(false, row(), grey(226));
        assert_eq!(mix.light(0.31).3, 0.31);
        assert_eq!(mix.shade(0.1).3, 0.1);
        assert_eq!(mix.shade_outside(0.8).3, 0.8);
    }

    #[test]
    fn in_linear_light_the_light_fill_still_comes_out_as_light_as_the_iphones() {
        let ground = grey(226);
        let fill = GlassEdge::Light.fill(row(), ground, true);
        let under = decode(lightness(ground));
        let seen = encode(under + fill.3 * (decode(lightness(fill)) - under));
        assert!(
            (seen - LIGHT_GLASS_CEILING).abs() < 0.5 / 255.0,
            "{:?}",
            fill
        );
    }
}
