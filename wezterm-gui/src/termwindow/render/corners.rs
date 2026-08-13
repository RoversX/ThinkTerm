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

pub const TOP_LEFT_ROUNDED_CORNER_MASK: &[Poly] = &[Poly {
    path: &[
        PolyCommand::MoveTo(BlockCoord::Zero, BlockCoord::Zero),
        PolyCommand::LineTo(BlockCoord::One, BlockCoord::Zero),
        PolyCommand::QuadTo {
            control: (BlockCoord::Zero, BlockCoord::Zero),
            to: (BlockCoord::Zero, BlockCoord::One),
        },
        PolyCommand::Close,
    ],
    intensity: BlockAlpha::Full,
    style: PolyStyle::Fill,
}];

pub const TOP_RIGHT_ROUNDED_CORNER_MASK: &[Poly] = &[Poly {
    path: &[
        PolyCommand::MoveTo(BlockCoord::Zero, BlockCoord::Zero),
        PolyCommand::LineTo(BlockCoord::One, BlockCoord::Zero),
        PolyCommand::LineTo(BlockCoord::One, BlockCoord::One),
        PolyCommand::QuadTo {
            control: (BlockCoord::One, BlockCoord::Zero),
            to: (BlockCoord::Zero, BlockCoord::Zero),
        },
        PolyCommand::Close,
    ],
    intensity: BlockAlpha::Full,
    style: PolyStyle::Fill,
}];

pub const BOTTOM_LEFT_ROUNDED_CORNER_MASK: &[Poly] = &[Poly {
    path: &[
        PolyCommand::MoveTo(BlockCoord::Zero, BlockCoord::Zero),
        PolyCommand::LineTo(BlockCoord::Zero, BlockCoord::One),
        PolyCommand::LineTo(BlockCoord::One, BlockCoord::One),
        PolyCommand::QuadTo {
            control: (BlockCoord::Zero, BlockCoord::One),
            to: (BlockCoord::Zero, BlockCoord::Zero),
        },
        PolyCommand::Close,
    ],
    intensity: BlockAlpha::Full,
    style: PolyStyle::Fill,
}];

pub const BOTTOM_RIGHT_ROUNDED_CORNER_MASK: &[Poly] = &[Poly {
    path: &[
        PolyCommand::MoveTo(BlockCoord::One, BlockCoord::Zero),
        PolyCommand::LineTo(BlockCoord::One, BlockCoord::One),
        PolyCommand::LineTo(BlockCoord::Zero, BlockCoord::One),
        PolyCommand::QuadTo {
            control: (BlockCoord::One, BlockCoord::One),
            to: (BlockCoord::One, BlockCoord::Zero),
        },
        PolyCommand::Close,
    ],
    intensity: BlockAlpha::Full,
    style: PolyStyle::Fill,
}];

pub const TOP_LEFT_ROUNDED_CORNER_OUTLINE: &[Poly] = &[Poly {
    path: &[
        PolyCommand::MoveTo(BlockCoord::One, BlockCoord::Zero),
        PolyCommand::QuadTo {
            control: (BlockCoord::Zero, BlockCoord::Zero),
            to: (BlockCoord::Zero, BlockCoord::One),
        },
    ],
    intensity: BlockAlpha::Full,
    style: PolyStyle::OutlineThin,
}];

pub const TOP_RIGHT_ROUNDED_CORNER_OUTLINE: &[Poly] = &[Poly {
    path: &[
        PolyCommand::MoveTo(BlockCoord::Zero, BlockCoord::Zero),
        PolyCommand::QuadTo {
            control: (BlockCoord::One, BlockCoord::Zero),
            to: (BlockCoord::One, BlockCoord::One),
        },
    ],
    intensity: BlockAlpha::Full,
    style: PolyStyle::OutlineThin,
}];

pub const BOTTOM_LEFT_ROUNDED_CORNER_OUTLINE: &[Poly] = &[Poly {
    path: &[
        PolyCommand::MoveTo(BlockCoord::Zero, BlockCoord::Zero),
        PolyCommand::QuadTo {
            control: (BlockCoord::Zero, BlockCoord::One),
            to: (BlockCoord::One, BlockCoord::One),
        },
    ],
    intensity: BlockAlpha::Full,
    style: PolyStyle::OutlineThin,
}];

pub const BOTTOM_RIGHT_ROUNDED_CORNER_OUTLINE: &[Poly] = &[Poly {
    path: &[
        PolyCommand::MoveTo(BlockCoord::One, BlockCoord::Zero),
        PolyCommand::QuadTo {
            control: (BlockCoord::One, BlockCoord::One),
            to: (BlockCoord::Zero, BlockCoord::One),
        },
    ],
    intensity: BlockAlpha::Full,
    style: PolyStyle::OutlineThin,
}];

// Quarter-annulus ring corners: the border stroke of a rounded corner,
// filled as outer arc + inner arc with a ring thickness of 1/5 of the
// corner cell. Together with edge strips of the same thickness they form
// a rounded-rectangle outline that works with translucent colors (which
// the occlusion-based fill_rounded_rectangle_with_border cannot do).

pub const TOP_LEFT_ROUNDED_CORNER_RING: &[Poly] = &[Poly {
    path: &[
        PolyCommand::MoveTo(BlockCoord::Zero, BlockCoord::One),
        PolyCommand::QuadTo {
            control: (BlockCoord::Zero, BlockCoord::Zero),
            to: (BlockCoord::One, BlockCoord::Zero),
        },
        PolyCommand::LineTo(BlockCoord::One, BlockCoord::Frac(1, 5)),
        PolyCommand::QuadTo {
            control: (BlockCoord::Frac(1, 5), BlockCoord::Frac(1, 5)),
            to: (BlockCoord::Frac(1, 5), BlockCoord::One),
        },
        PolyCommand::Close,
    ],
    intensity: BlockAlpha::Full,
    style: PolyStyle::Fill,
}];

pub const TOP_RIGHT_ROUNDED_CORNER_RING: &[Poly] = &[Poly {
    path: &[
        PolyCommand::MoveTo(BlockCoord::Zero, BlockCoord::Zero),
        PolyCommand::QuadTo {
            control: (BlockCoord::One, BlockCoord::Zero),
            to: (BlockCoord::One, BlockCoord::One),
        },
        PolyCommand::LineTo(BlockCoord::Frac(4, 5), BlockCoord::One),
        PolyCommand::QuadTo {
            control: (BlockCoord::Frac(4, 5), BlockCoord::Frac(1, 5)),
            to: (BlockCoord::Zero, BlockCoord::Frac(1, 5)),
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
            control: (BlockCoord::Zero, BlockCoord::One),
            to: (BlockCoord::One, BlockCoord::One),
        },
        PolyCommand::LineTo(BlockCoord::One, BlockCoord::Frac(4, 5)),
        PolyCommand::QuadTo {
            control: (BlockCoord::Frac(1, 5), BlockCoord::Frac(4, 5)),
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
            control: (BlockCoord::One, BlockCoord::One),
            to: (BlockCoord::Zero, BlockCoord::One),
        },
        PolyCommand::LineTo(BlockCoord::Zero, BlockCoord::Frac(4, 5)),
        PolyCommand::QuadTo {
            control: (BlockCoord::Frac(4, 5), BlockCoord::Frac(4, 5)),
            to: (BlockCoord::Frac(4, 5), BlockCoord::Zero),
        },
        PolyCommand::Close,
    ],
    intensity: BlockAlpha::Full,
    style: PolyStyle::Fill,
}];
