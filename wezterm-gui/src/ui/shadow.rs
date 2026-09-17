//! Soft drop shadows for rounded surfaces.
//!
//! The quad renderer has no blur, so a shadow used to be two slightly larger
//! rounded rectangles stacked under the surface: two hard-edged steps that
//! read as a halo. A real shadow is a Gaussian-blurred silhouette, and that
//! is a picture -- so it is rasterized once per (corner radius, blur) pair
//! into the glyph atlas, like the rounded corners are, and drawn as nine
//! slices: four corners, four stretched edges and a stretched middle. The
//! sprite is the shadow of the smallest surface with those corners; because
//! the blur is uniform, every larger surface's shadow is that picture with
//! its edges pulled apart.

use ::window::bitmaps::{BitmapImage, Image};

/// Width of the stretchable band between the corner slices, in sprite
/// pixels. Two, so that a sample taken from its middle never reaches a
/// corner texel however the edges are stretched.
const MIDDLE: usize = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct ShadowKey {
    /// Corner radius of the surface, in pixels.
    pub radius: u16,
    /// Standard deviation of the blur, in pixels.
    pub sigma: u16,
}

/// How the sprite for a key is laid out. Everything is in sprite pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ShadowLayout {
    /// How far the shadow reaches beyond the surface's edge.
    pub extent: usize,
    /// Side of a corner slice: the reach plus the corner radius.
    pub corner: usize,
    /// Side of the (square) sprite.
    pub side: usize,
}

impl ShadowKey {
    pub(crate) fn layout(self) -> ShadowLayout {
        let extent = (self.sigma as usize) * 3;
        let radius = self.radius as usize;
        let corner = extent + radius;
        // The surface in the sprite must be wide enough that opposite edges
        // do not blur into each other: at least the blur's full reach either
        // way, or the corner slices would carry the far edge's falloff.
        let inner = (2 * radius).max(2 * extent) + MIDDLE;
        ShadowLayout {
            extent,
            corner,
            side: inner + 2 * extent,
        }
    }
}

/// The blurred silhouette of the smallest rounded rectangle with the key's
/// corners, in the alpha channel; the colour channels are left black, as
/// the grayscale quad path only reads alpha.
pub(crate) fn rasterize_shadow(key: ShadowKey) -> Image {
    let layout = key.layout();
    let side = layout.side;
    let extent = layout.extent as f32;
    let radius = key.radius as f32;
    let inner = (side - 2 * layout.extent) as f32;

    // Coverage of the rounded rectangle at each pixel centre, anti-aliased
    // by the signed distance to its edge.
    let half = inner / 2.0;
    let centre = extent + half;
    let mut coverage = vec![0f32; side * side];
    for y in 0..side {
        for x in 0..side {
            let px = (x as f32 + 0.5 - centre).abs() - (half - radius);
            let py = (y as f32 + 0.5 - centre).abs() - (half - radius);
            let outside = (px.max(0.0).powi(2) + py.max(0.0).powi(2)).sqrt();
            let inside = px.max(py).min(0.0);
            let distance = outside + inside - radius;
            coverage[y * side + x] = (0.5 - distance).clamp(0.0, 1.0);
        }
    }

    // Separable Gaussian blur, kernel out to three sigma either side.
    let sigma = key.sigma.max(1) as f32;
    let reach = layout.extent;
    let kernel: Vec<f32> = (0..=2 * reach)
        .map(|i| {
            let d = i as f32 - reach as f32;
            (-(d * d) / (2.0 * sigma * sigma)).exp()
        })
        .collect();
    let norm: f32 = kernel.iter().sum();
    let blur_axis = |src: &[f32], horizontal: bool| -> Vec<f32> {
        let mut out = vec![0f32; side * side];
        for y in 0..side {
            for x in 0..side {
                let mut acc = 0.0;
                for (i, weight) in kernel.iter().enumerate() {
                    let offset = i as isize - reach as isize;
                    let (sx, sy) = if horizontal {
                        (x as isize + offset, y as isize)
                    } else {
                        (x as isize, y as isize + offset)
                    };
                    if sx >= 0 && sy >= 0 && (sx as usize) < side && (sy as usize) < side {
                        acc += weight * src[sy as usize * side + sx as usize];
                    }
                }
                out[y * side + x] = acc / norm;
            }
        }
        out
    };
    let blurred = if key.sigma == 0 {
        coverage
    } else {
        blur_axis(&blur_axis(&coverage, true), false)
    };

    let mut image = Image::new(side, side);
    let pixels = image.pixel_data_slice_mut();
    for (i, alpha) in blurred.iter().enumerate() {
        pixels[i * 4 + 3] = (alpha.clamp(0.0, 1.0) * 255.0).round() as u8;
    }
    image
}

/// One slice of a nine-slice shadow: where it lands on screen, and which
/// part of the sprite it shows, both as (min, max) along each axis. Sprite
/// coordinates are in sprite pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct ShadowSlice {
    pub screen_x: (f32, f32),
    pub screen_y: (f32, f32),
    pub sprite_x: (f32, f32),
    pub sprite_y: (f32, f32),
}

/// The nine slices that draw the shadow of a `width` x `height` surface
/// whose top-left corner sits at `(x, y)`, with the shadow's own corner
/// radius already baked into `layout`. Corners keep their size; edges and
/// middle stretch. A surface narrower or shorter than two corners gets its
/// corner slices shrunk to fit, which is the best a fixed sprite can do.
pub(crate) fn shadow_slices(
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    layout: ShadowLayout,
) -> Vec<ShadowSlice> {
    let extent = layout.extent as f32;
    let corner = layout.corner as f32;
    let side = layout.side as f32;
    let middle_start = corner;
    let middle_end = side - corner;

    // Screen breakpoints along one axis for a run of `length` pixels.
    let axis = |start: f32, length: f32| -> [f32; 4] {
        let outer = length + extent * 2.0;
        let corner = corner.min(outer / 2.0);
        [
            start - extent,
            start - extent + corner,
            start + length + extent - corner,
            start + length + extent,
        ]
    };
    let xs = axis(x, width);
    let ys = axis(y, height);
    let sprite = [0.0, middle_start, middle_end, side];

    let mut slices = Vec::with_capacity(9);
    for row in 0..3 {
        for col in 0..3 {
            let screen_x = (xs[col], xs[col + 1]);
            let screen_y = (ys[row], ys[row + 1]);
            if screen_x.1 <= screen_x.0 || screen_y.1 <= screen_y.0 {
                continue;
            }
            slices.push(ShadowSlice {
                screen_x,
                screen_y,
                sprite_x: (sprite[col], sprite[col + 1]),
                sprite_y: (sprite[row], sprite[row + 1]),
            });
        }
    }
    slices
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alpha_at(image: &Image, x: usize, y: usize) -> u8 {
        let (width, _) = image.image_dimensions();
        image.pixel_data_slice()[(y * width + x) * 4 + 3]
    }

    #[test]
    fn the_shadow_is_solid_under_the_surface_and_gone_at_its_reach() {
        let key = ShadowKey {
            radius: 8,
            sigma: 4,
        };
        let layout = key.layout();
        let image = rasterize_shadow(key);
        let centre = layout.side / 2;
        assert!(alpha_at(&image, centre, centre) >= 250);
        assert_eq!(alpha_at(&image, 0, 0), 0);
        assert_eq!(alpha_at(&image, centre, 0), 0);
        // Symmetric in both axes.
        assert_eq!(
            alpha_at(&image, layout.extent, centre),
            alpha_at(&image, layout.side - 1 - layout.extent, centre)
        );
        assert_eq!(
            alpha_at(&image, centre, layout.extent),
            alpha_at(&image, layout.extent, centre)
        );
        // Soft: the edge of the surface is partway through the falloff.
        let edge = alpha_at(&image, layout.extent, centre);
        assert!(edge > 60 && edge < 200, "edge alpha {edge}");
    }

    #[test]
    fn nine_slices_tile_the_shadow_without_gaps_or_overlap() {
        let layout = ShadowKey {
            radius: 8,
            sigma: 4,
        }
        .layout();
        let slices = shadow_slices(100.0, 200.0, 300.0, 120.0, layout);
        assert_eq!(slices.len(), 9);
        let extent = layout.extent as f32;
        let left = slices.iter().map(|s| s.screen_x.0).fold(f32::MAX, f32::min);
        let right = slices.iter().map(|s| s.screen_x.1).fold(f32::MIN, f32::max);
        let top = slices.iter().map(|s| s.screen_y.0).fold(f32::MAX, f32::min);
        let bottom = slices.iter().map(|s| s.screen_y.1).fold(f32::MIN, f32::max);
        assert_eq!((left, right), (100.0 - extent, 400.0 + extent));
        assert_eq!((top, bottom), (200.0 - extent, 320.0 + extent));
        let area: f32 = slices
            .iter()
            .map(|s| (s.screen_x.1 - s.screen_x.0) * (s.screen_y.1 - s.screen_y.0))
            .sum();
        assert_eq!(area, (right - left) * (bottom - top));
        // The corners are never stretched.
        assert_eq!(slices[0].screen_x.1 - slices[0].screen_x.0, layout.corner as f32);
        assert_eq!(slices[0].sprite_x, (0.0, layout.corner as f32));
    }

    #[test]
    fn a_surface_smaller_than_two_corners_shrinks_its_corner_slices() {
        let layout = ShadowKey {
            radius: 20,
            sigma: 2,
        }
        .layout();
        let slices = shadow_slices(0.0, 0.0, 10.0, 10.0, layout);
        // Six slices: the middle band has no width to draw.
        assert!(slices.iter().all(|s| s.screen_x.1 > s.screen_x.0));
        assert!(slices.len() < 9);
    }
}
