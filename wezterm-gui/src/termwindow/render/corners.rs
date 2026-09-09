use crate::customglyph::*;

pub const TOP_LEFT_ROUNDED_CORNER: &[Poly] = &[Poly {
    path: &[PolyCommand::Oval {
        center: (BlockCoord::One, BlockCoord::One),
        radiuses: (BlockCoord::One, BlockCoord::One),
    }],
    intensity: BlockAlpha::Full,
    style: PolyStyle::Fill,
}];

pub const BOTTOM_LEFT_ROUNDED_CORNER: &[Poly] = &[Poly {
    path: &[PolyCommand::Oval {
        center: (BlockCoord::One, BlockCoord::Zero),
        radiuses: (BlockCoord::One, BlockCoord::One),
    }],
    intensity: BlockAlpha::Full,
    style: PolyStyle::Fill,
}];

pub const TOP_RIGHT_ROUNDED_CORNER: &[Poly] = &[Poly {
    path: &[PolyCommand::Oval {
        center: (BlockCoord::Zero, BlockCoord::One),
        radiuses: (BlockCoord::One, BlockCoord::One),
    }],
    intensity: BlockAlpha::Full,
    style: PolyStyle::Fill,
}];

pub const BOTTOM_RIGHT_ROUNDED_CORNER: &[Poly] = &[Poly {
    path: &[PolyCommand::Oval {
        center: (BlockCoord::Zero, BlockCoord::Zero),
        radiuses: (BlockCoord::One, BlockCoord::One),
    }],
    intensity: BlockAlpha::Full,
    style: PolyStyle::Fill,
}];

// The complementary pieces outside a rounded corner.  They are used after a
// live terminal preview has been painted: covering those four small wedges is
// the GPU equivalent of a rounded clip, without introducing a stencil pass or
// re-rasterising the terminal into an intermediate texture.
//
// The arc is four 22.5-degree quadratic Beziers, not one 90-degree one: a
// single quad with its control point on the corner is a parabola that passes
// 0.354 radii from the corner where the `Oval` fill above passes 0.414, so
// the mask left a sliver of live terminal showing outside the chrome at every
// corner. Four quads stay within 0.02% of a radius of the true circle. `Oval`
// itself cannot be used here: there is no path command for "everything but",
// and the fill rule is non-zero winding.
//
// The fractions look odd on purpose. `BlockCoord::to_pixel` nudges any
// coordinate that lands on a whole pixel by half a pixel (a stroke hint), so
// a tidy 1/2 would kink the arc at every even radius. Every denominator here
// is a prime above 66, which no corner drawn today is a multiple of, and each
// fraction is within 0.001 of its irrational value (a twentieth of a pixel at
// the 64 px onboarding disc). The test below holds the mask and the fill to
// covering every pixel between them once.

pub const TOP_LEFT_ROUNDED_CORNER_MASK: &[Poly] = &[Poly {
    path: &[
        PolyCommand::MoveTo(BlockCoord::Zero, BlockCoord::Zero),
        PolyCommand::LineTo(BlockCoord::One, BlockCoord::Zero),
        PolyCommand::QuadTo {
            control: (BlockCoord::Frac(81, 101), BlockCoord::Zero),
            to: (BlockCoord::Frac(66, 107), BlockCoord::Frac(6, 79)),
        },
        PolyCommand::QuadTo {
            control: (BlockCoord::Frac(49, 113), BlockCoord::Frac(12, 79)),
            to: (BlockCoord::Frac(32, 109), BlockCoord::Frac(32, 109)),
        },
        PolyCommand::QuadTo {
            control: (BlockCoord::Frac(12, 79), BlockCoord::Frac(49, 113)),
            to: (BlockCoord::Frac(6, 79), BlockCoord::Frac(66, 107)),
        },
        PolyCommand::QuadTo {
            control: (BlockCoord::Zero, BlockCoord::Frac(81, 101)),
            to: (BlockCoord::Zero, BlockCoord::One),
        },
        PolyCommand::Close,
    ],
    intensity: BlockAlpha::Full,
    style: PolyStyle::Fill,
}];

pub const TOP_RIGHT_ROUNDED_CORNER_MASK: &[Poly] = &[Poly {
    path: &[
        PolyCommand::MoveTo(BlockCoord::One, BlockCoord::Zero),
        PolyCommand::LineTo(BlockCoord::Zero, BlockCoord::Zero),
        PolyCommand::QuadTo {
            control: (BlockCoord::Frac(20, 101), BlockCoord::Zero),
            to: (BlockCoord::Frac(41, 107), BlockCoord::Frac(6, 79)),
        },
        PolyCommand::QuadTo {
            control: (BlockCoord::Frac(64, 113), BlockCoord::Frac(12, 79)),
            to: (BlockCoord::Frac(77, 109), BlockCoord::Frac(32, 109)),
        },
        PolyCommand::QuadTo {
            control: (BlockCoord::Frac(67, 79), BlockCoord::Frac(49, 113)),
            to: (BlockCoord::Frac(73, 79), BlockCoord::Frac(66, 107)),
        },
        PolyCommand::QuadTo {
            control: (BlockCoord::One, BlockCoord::Frac(81, 101)),
            to: (BlockCoord::One, BlockCoord::One),
        },
        PolyCommand::Close,
    ],
    intensity: BlockAlpha::Full,
    style: PolyStyle::Fill,
}];

pub const BOTTOM_LEFT_ROUNDED_CORNER_MASK: &[Poly] = &[Poly {
    path: &[
        PolyCommand::MoveTo(BlockCoord::Zero, BlockCoord::One),
        PolyCommand::LineTo(BlockCoord::One, BlockCoord::One),
        PolyCommand::QuadTo {
            control: (BlockCoord::Frac(81, 101), BlockCoord::One),
            to: (BlockCoord::Frac(66, 107), BlockCoord::Frac(73, 79)),
        },
        PolyCommand::QuadTo {
            control: (BlockCoord::Frac(49, 113), BlockCoord::Frac(67, 79)),
            to: (BlockCoord::Frac(32, 109), BlockCoord::Frac(77, 109)),
        },
        PolyCommand::QuadTo {
            control: (BlockCoord::Frac(12, 79), BlockCoord::Frac(64, 113)),
            to: (BlockCoord::Frac(6, 79), BlockCoord::Frac(41, 107)),
        },
        PolyCommand::QuadTo {
            control: (BlockCoord::Zero, BlockCoord::Frac(20, 101)),
            to: (BlockCoord::Zero, BlockCoord::Zero),
        },
        PolyCommand::Close,
    ],
    intensity: BlockAlpha::Full,
    style: PolyStyle::Fill,
}];

pub const BOTTOM_RIGHT_ROUNDED_CORNER_MASK: &[Poly] = &[Poly {
    path: &[
        PolyCommand::MoveTo(BlockCoord::One, BlockCoord::One),
        PolyCommand::LineTo(BlockCoord::Zero, BlockCoord::One),
        PolyCommand::QuadTo {
            control: (BlockCoord::Frac(20, 101), BlockCoord::One),
            to: (BlockCoord::Frac(41, 107), BlockCoord::Frac(73, 79)),
        },
        PolyCommand::QuadTo {
            control: (BlockCoord::Frac(64, 113), BlockCoord::Frac(67, 79)),
            to: (BlockCoord::Frac(77, 109), BlockCoord::Frac(77, 109)),
        },
        PolyCommand::QuadTo {
            control: (BlockCoord::Frac(67, 79), BlockCoord::Frac(64, 113)),
            to: (BlockCoord::Frac(73, 79), BlockCoord::Frac(41, 107)),
        },
        PolyCommand::QuadTo {
            control: (BlockCoord::One, BlockCoord::Frac(20, 101)),
            to: (BlockCoord::One, BlockCoord::Zero),
        },
        PolyCommand::Close,
    ],
    intensity: BlockAlpha::Full,
    style: PolyStyle::Fill,
}];

// The stroke of a rounded corner. Deliberately the *same* `Oval` the filled
// corner above uses, only stroked instead of filled.
//
// These were a quadratic Bezier from one end of the arc to the other with
// the control point on the corner, which is a parabola, not a circle: it
// passes 0.354 radii from the corner where a circle passes 0.414, so the
// stroke bulged outward by about 6% of the radius along each diagonal and
// away from the fill it was supposed to trace. At the 8-12px radii of a
// settings card that is half a pixel and invisible; on the 64px disc of the
// onboarding mark it was a visible second ring floating outside the shape.
// Sharing one primitive is what makes them agree by construction rather
// than by two curves happening to be close.

pub const TOP_LEFT_ROUNDED_CORNER_OUTLINE: &[Poly] = &[Poly {
    path: &[PolyCommand::Oval {
        center: (BlockCoord::One, BlockCoord::One),
        radiuses: (BlockCoord::One, BlockCoord::One),
    }],
    intensity: BlockAlpha::Full,
    style: PolyStyle::OutlineThin,
}];

pub const TOP_RIGHT_ROUNDED_CORNER_OUTLINE: &[Poly] = &[Poly {
    path: &[PolyCommand::Oval {
        center: (BlockCoord::Zero, BlockCoord::One),
        radiuses: (BlockCoord::One, BlockCoord::One),
    }],
    intensity: BlockAlpha::Full,
    style: PolyStyle::OutlineThin,
}];

pub const BOTTOM_LEFT_ROUNDED_CORNER_OUTLINE: &[Poly] = &[Poly {
    path: &[PolyCommand::Oval {
        center: (BlockCoord::One, BlockCoord::Zero),
        radiuses: (BlockCoord::One, BlockCoord::One),
    }],
    intensity: BlockAlpha::Full,
    style: PolyStyle::OutlineThin,
}];

pub const BOTTOM_RIGHT_ROUNDED_CORNER_OUTLINE: &[Poly] = &[Poly {
    path: &[PolyCommand::Oval {
        center: (BlockCoord::Zero, BlockCoord::Zero),
        radiuses: (BlockCoord::One, BlockCoord::One),
    }],
    intensity: BlockAlpha::Full,
    style: PolyStyle::OutlineThin,
}];

// Quarter-annulus ring corners: the border stroke of a rounded corner,
// filled as outer arc + inner arc with a ring thickness of 1/5 of the
// corner cell. Together with edge strips of the same thickness they form
// a rounded-rectangle outline that works with translucent colors (which
// the occlusion-based fill_rounded_rectangle_with_border cannot do).
// Both arcs are the four-quad circle approximation the masks use, the inner
// one scaled to radius 4/5; the same prime-denominator rule applies.

pub const TOP_LEFT_ROUNDED_CORNER_RING: &[Poly] = &[Poly {
    path: &[
        PolyCommand::MoveTo(BlockCoord::Zero, BlockCoord::One),
        PolyCommand::QuadTo {
            control: (BlockCoord::Zero, BlockCoord::Frac(81, 101)),
            to: (BlockCoord::Frac(6, 79), BlockCoord::Frac(66, 107)),
        },
        PolyCommand::QuadTo {
            control: (BlockCoord::Frac(12, 79), BlockCoord::Frac(49, 113)),
            to: (BlockCoord::Frac(32, 109), BlockCoord::Frac(32, 109)),
        },
        PolyCommand::QuadTo {
            control: (BlockCoord::Frac(49, 113), BlockCoord::Frac(12, 79)),
            to: (BlockCoord::Frac(66, 107), BlockCoord::Frac(6, 79)),
        },
        PolyCommand::QuadTo {
            control: (BlockCoord::Frac(81, 101), BlockCoord::Zero),
            to: (BlockCoord::One, BlockCoord::Zero),
        },
        PolyCommand::LineTo(BlockCoord::One, BlockCoord::Frac(1, 5)),
        PolyCommand::QuadTo {
            control: (BlockCoord::Frac(95, 113), BlockCoord::Frac(1, 5)),
            to: (BlockCoord::Frac(70, 101), BlockCoord::Frac(19, 73)),
        },
        PolyCommand::QuadTo {
            control: (BlockCoord::Frac(53, 97), BlockCoord::Frac(35, 109)),
            to: (BlockCoord::Frac(36, 83), BlockCoord::Frac(36, 83)),
        },
        PolyCommand::QuadTo {
            control: (BlockCoord::Frac(35, 109), BlockCoord::Frac(53, 97)),
            to: (BlockCoord::Frac(19, 73), BlockCoord::Frac(70, 101)),
        },
        PolyCommand::QuadTo {
            control: (BlockCoord::Frac(1, 5), BlockCoord::Frac(95, 113)),
            to: (BlockCoord::Frac(1, 5), BlockCoord::One),
        },
        PolyCommand::Close,
    ],
    intensity: BlockAlpha::Full,
    style: PolyStyle::Fill,
}];

pub const TOP_RIGHT_ROUNDED_CORNER_RING: &[Poly] = &[Poly {
    path: &[
        PolyCommand::MoveTo(BlockCoord::One, BlockCoord::One),
        PolyCommand::QuadTo {
            control: (BlockCoord::One, BlockCoord::Frac(81, 101)),
            to: (BlockCoord::Frac(73, 79), BlockCoord::Frac(66, 107)),
        },
        PolyCommand::QuadTo {
            control: (BlockCoord::Frac(67, 79), BlockCoord::Frac(49, 113)),
            to: (BlockCoord::Frac(77, 109), BlockCoord::Frac(32, 109)),
        },
        PolyCommand::QuadTo {
            control: (BlockCoord::Frac(64, 113), BlockCoord::Frac(12, 79)),
            to: (BlockCoord::Frac(41, 107), BlockCoord::Frac(6, 79)),
        },
        PolyCommand::QuadTo {
            control: (BlockCoord::Frac(20, 101), BlockCoord::Zero),
            to: (BlockCoord::Zero, BlockCoord::Zero),
        },
        PolyCommand::LineTo(BlockCoord::Zero, BlockCoord::Frac(1, 5)),
        PolyCommand::QuadTo {
            control: (BlockCoord::Frac(18, 113), BlockCoord::Frac(1, 5)),
            to: (BlockCoord::Frac(31, 101), BlockCoord::Frac(19, 73)),
        },
        PolyCommand::QuadTo {
            control: (BlockCoord::Frac(44, 97), BlockCoord::Frac(35, 109)),
            to: (BlockCoord::Frac(47, 83), BlockCoord::Frac(36, 83)),
        },
        PolyCommand::QuadTo {
            control: (BlockCoord::Frac(74, 109), BlockCoord::Frac(53, 97)),
            to: (BlockCoord::Frac(54, 73), BlockCoord::Frac(70, 101)),
        },
        PolyCommand::QuadTo {
            control: (BlockCoord::Frac(4, 5), BlockCoord::Frac(95, 113)),
            to: (BlockCoord::Frac(4, 5), BlockCoord::One),
        },
        PolyCommand::Close,
    ],
    intensity: BlockAlpha::Full,
    style: PolyStyle::Fill,
}];

pub const BOTTOM_LEFT_ROUNDED_CORNER_RING: &[Poly] = &[Poly {
    path: &[
        PolyCommand::MoveTo(BlockCoord::Zero, BlockCoord::Zero),
        PolyCommand::QuadTo {
            control: (BlockCoord::Zero, BlockCoord::Frac(20, 101)),
            to: (BlockCoord::Frac(6, 79), BlockCoord::Frac(41, 107)),
        },
        PolyCommand::QuadTo {
            control: (BlockCoord::Frac(12, 79), BlockCoord::Frac(64, 113)),
            to: (BlockCoord::Frac(32, 109), BlockCoord::Frac(77, 109)),
        },
        PolyCommand::QuadTo {
            control: (BlockCoord::Frac(49, 113), BlockCoord::Frac(67, 79)),
            to: (BlockCoord::Frac(66, 107), BlockCoord::Frac(73, 79)),
        },
        PolyCommand::QuadTo {
            control: (BlockCoord::Frac(81, 101), BlockCoord::One),
            to: (BlockCoord::One, BlockCoord::One),
        },
        PolyCommand::LineTo(BlockCoord::One, BlockCoord::Frac(4, 5)),
        PolyCommand::QuadTo {
            control: (BlockCoord::Frac(95, 113), BlockCoord::Frac(4, 5)),
            to: (BlockCoord::Frac(70, 101), BlockCoord::Frac(54, 73)),
        },
        PolyCommand::QuadTo {
            control: (BlockCoord::Frac(53, 97), BlockCoord::Frac(74, 109)),
            to: (BlockCoord::Frac(36, 83), BlockCoord::Frac(47, 83)),
        },
        PolyCommand::QuadTo {
            control: (BlockCoord::Frac(35, 109), BlockCoord::Frac(44, 97)),
            to: (BlockCoord::Frac(19, 73), BlockCoord::Frac(31, 101)),
        },
        PolyCommand::QuadTo {
            control: (BlockCoord::Frac(1, 5), BlockCoord::Frac(18, 113)),
            to: (BlockCoord::Frac(1, 5), BlockCoord::Zero),
        },
        PolyCommand::Close,
    ],
    intensity: BlockAlpha::Full,
    style: PolyStyle::Fill,
}];

pub const BOTTOM_RIGHT_ROUNDED_CORNER_RING: &[Poly] = &[Poly {
    path: &[
        PolyCommand::MoveTo(BlockCoord::One, BlockCoord::Zero),
        PolyCommand::QuadTo {
            control: (BlockCoord::One, BlockCoord::Frac(20, 101)),
            to: (BlockCoord::Frac(73, 79), BlockCoord::Frac(41, 107)),
        },
        PolyCommand::QuadTo {
            control: (BlockCoord::Frac(67, 79), BlockCoord::Frac(64, 113)),
            to: (BlockCoord::Frac(77, 109), BlockCoord::Frac(77, 109)),
        },
        PolyCommand::QuadTo {
            control: (BlockCoord::Frac(64, 113), BlockCoord::Frac(67, 79)),
            to: (BlockCoord::Frac(41, 107), BlockCoord::Frac(73, 79)),
        },
        PolyCommand::QuadTo {
            control: (BlockCoord::Frac(20, 101), BlockCoord::One),
            to: (BlockCoord::Zero, BlockCoord::One),
        },
        PolyCommand::LineTo(BlockCoord::Zero, BlockCoord::Frac(4, 5)),
        PolyCommand::QuadTo {
            control: (BlockCoord::Frac(18, 113), BlockCoord::Frac(4, 5)),
            to: (BlockCoord::Frac(31, 101), BlockCoord::Frac(54, 73)),
        },
        PolyCommand::QuadTo {
            control: (BlockCoord::Frac(44, 97), BlockCoord::Frac(74, 109)),
            to: (BlockCoord::Frac(47, 83), BlockCoord::Frac(47, 83)),
        },
        PolyCommand::QuadTo {
            control: (BlockCoord::Frac(74, 109), BlockCoord::Frac(44, 97)),
            to: (BlockCoord::Frac(54, 73), BlockCoord::Frac(31, 101)),
        },
        PolyCommand::QuadTo {
            control: (BlockCoord::Frac(4, 5), BlockCoord::Frac(18, 113)),
            to: (BlockCoord::Frac(4, 5), BlockCoord::Zero),
        },
        PolyCommand::Close,
    ],
    intensity: BlockAlpha::Full,
    style: PolyStyle::Fill,
}];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::customglyph::{block_image, BlockKey, BlockMetrics, PolyAA};
    use ::window::bitmaps::BitmapImage;

    const SIDE: usize = 64;

    fn raster(polys: &'static [Poly]) -> Vec<u8> {
        let im = block_image(
            BlockKey::Poly(polys),
            &BlockMetrics {
                cell_size: euclid::size2(SIDE as isize, SIDE as isize),
                underline_height: 1,
            },
            PolyAA::AntiAlias,
        );
        im.pixel_data_slice()
            .chunks_exact(4)
            .map(|p| p[3])
            .collect()
    }

    /// The mask is everything the filled corner is not. A one-quad mask
    /// traced a parabola that left pixels 16..18 along the diagonal of a
    /// 64 px corner covered by neither, so live terminal showed through.
    /// The band allows for the 16-level anti-aliasing on the shared edge.
    #[test]
    fn a_mask_and_its_fill_cover_every_pixel_between_them_exactly_once() {
        for (name, fill, mask) in [
            ("top-left", TOP_LEFT_ROUNDED_CORNER, TOP_LEFT_ROUNDED_CORNER_MASK),
            ("top-right", TOP_RIGHT_ROUNDED_CORNER, TOP_RIGHT_ROUNDED_CORNER_MASK),
            ("bottom-left", BOTTOM_LEFT_ROUNDED_CORNER, BOTTOM_LEFT_ROUNDED_CORNER_MASK),
            ("bottom-right", BOTTOM_RIGHT_ROUNDED_CORNER, BOTTOM_RIGHT_ROUNDED_CORNER_MASK),
        ] {
            let (fill, mask) = (raster(fill), raster(mask));
            for (i, (f, m)) in fill.iter().zip(mask.iter()).enumerate() {
                let sum = *f as u32 + *m as u32;
                assert!(
                    (200..=310).contains(&sum),
                    "{name}: pixel ({}, {}) fill {f} + mask {m} = {sum}",
                    i % SIDE,
                    i / SIDE
                );
            }
        }
    }

    /// The ring is the band between radius 4/5 and 1 of the same circle the
    /// fill draws: solid in the band, empty inside and outside it.
    #[test]
    fn a_ring_is_the_outer_fifth_of_the_filled_corner() {
        let at = |px: &[u8], x: usize, y: usize| px[y * SIDE + x];
        let ring = raster(TOP_LEFT_ROUNDED_CORNER_RING);
        let fill = raster(TOP_LEFT_ROUNDED_CORNER);
        // Along the diagonal from the corner: outside the circle, in the
        // band, inside the band.
        assert_eq!(at(&ring, 12, 12), 0, "outside the circle");
        assert_eq!(at(&fill, 12, 12), 0);
        assert!(at(&ring, 23, 23) > 240, "in the band: {}", at(&ring, 23, 23));
        assert!(at(&fill, 23, 23) > 240);
        assert_eq!(at(&ring, 33, 33), 0, "inside the band");
        assert!(at(&fill, 33, 33) > 240);
        // The ring's outer edge is the fill's edge: wherever the fill has
        // started, so has the ring, along the whole arc.
        for y in 0..SIDE {
            for x in 0..SIDE {
                let (f, r) = (at(&fill, x, y), at(&ring, x, y));
                if f > 0 && r == 0 {
                    // Then this pixel must be well inside the band's inner
                    // edge, never on the outer one.
                    let (dx, dy) = (SIDE as f32 - (x as f32 + 0.5), SIDE as f32 - (y as f32 + 0.5));
                    let rho = (dx * dx + dy * dy).sqrt() / SIDE as f32;
                    assert!(rho < 0.82, "({x}, {y}) fill {f} ring {r} at rho {rho:.3}");
                }
            }
        }
    }
}
