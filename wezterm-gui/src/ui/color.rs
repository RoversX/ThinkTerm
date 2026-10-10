//! Colour arithmetic for the interface's own chrome.

use window::color::LinearRgba;

/// `from` moved `t` of the way towards `to`, **in sRGB space**, alpha
/// included.
///
/// The blend must happen in sRGB, not in the linear values these colours
/// are stored as. Lerping 10% of the way from near-black to near-white in
/// *linear* space lands around 34% in sRGB — so greys meant to sit just off the
/// page came out as mid-greys.
pub(crate) fn mix(from: LinearRgba, to: LinearRgba, t: f32) -> LinearRgba {
    let t = t.clamp(0.0, 1.0);
    let lerp = |a: f32, b: f32| srgb_decode(srgb_encode(a) + (srgb_encode(b) - srgb_encode(a)) * t);
    LinearRgba::with_components(
        lerp(from.0, to.0),
        lerp(from.1, to.1),
        lerp(from.2, to.2),
        from.3 + (to.3 - from.3) * t,
    )
}

// The standard sRGB transfer function, written out rather than reached for on
// `LinearRgba`/`SrgbaTuple`: those two use different curves (one exact, one a
// gamma-2.2 approximation) and so do not round-trip. These are exact inverses,
// which is what a blend needs.
pub(crate) fn srgb_encode(c: f32) -> f32 {
    if c <= 0.003_130_8 {
        c * 12.92
    } else {
        1.055 * c.powf(1.0 / 2.4) - 0.055
    }
}

pub(crate) fn srgb_decode(c: f32) -> f32 {
    if c <= 0.040_45 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mix_blends_perceptually_not_in_linear_space() {
        let black = LinearRgba::with_srgba(0, 0, 0, 255);
        let white = LinearRgba::with_srgba(255, 255, 255, 255);

        let tenth = srgb_encode(mix(black, white, 0.10).0);
        assert!(
            (tenth - 0.10).abs() < 0.02,
            "10% of the way to white should be ~10% in sRGB, got {}",
            tenth
        );
        // Halfway is mid-grey to the eye, not the much lighter linear midpoint.
        let half = srgb_encode(mix(black, white, 0.5).0);
        assert!((half - 0.5).abs() < 0.02, "midpoint drifted to {}", half);
    }

    #[test]
    fn mix_carries_alpha_along() {
        let clear = LinearRgba::with_components(1.0, 1.0, 1.0, 0.2);
        let solid = LinearRgba::with_components(1.0, 1.0, 1.0, 1.0);
        assert!((mix(clear, solid, 0.5).3 - 0.6).abs() < 1e-6);
        assert_eq!(mix(clear, clear, 0.7).3, 0.2);
    }
}
