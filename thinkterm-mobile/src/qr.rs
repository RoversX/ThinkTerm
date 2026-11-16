//! A QR code as a picture, for the shell to show as it is: the tailnet's
//! login link, scanned by another device's camera to log this one in
//! there. Drawn here once for every phone, as the desktop draws its own
//! (wezterm-gui's web settings).

use crate::Picture;

/// The modules of white round a code that a reader needs to find it.
const QUIET_ZONE: usize = 4;

/// `text` as a QR code at most `size` pixels square: dark modules on
/// white, with the quiet zone round them, whatever the screen's
/// appearance, since a camera reads that best. Each module is a whole
/// number of pixels, so the picture can come out smaller than `size`.
/// None for text no code can hold, or a size too small for its modules.
#[uniffi::export]
pub fn qr_picture(text: String, size: u32) -> Option<Picture> {
    let code = qrcode::QrCode::new(text.as_bytes()).ok()?;
    let modules = code.width();
    let colors = code.to_colors();
    let span = modules + 2 * QUIET_ZONE;
    let scale = size as usize / span;
    if scale == 0 {
        return None;
    }
    let side = span * scale;
    // White to begin with; the dark modules painted over it.
    let mut rgba = vec![255u8; side * side * 4];
    for y in 0..modules {
        for x in 0..modules {
            if colors[y * modules + x] != qrcode::Color::Dark {
                continue;
            }
            let (left, top) = ((x + QUIET_ZONE) * scale, (y + QUIET_ZONE) * scale);
            for row in top..top + scale {
                let start = (row * side + left) * 4;
                for pixel in rgba[start..start + scale * 4].chunks_exact_mut(4) {
                    pixel.copy_from_slice(&[0, 0, 0, 255]);
                }
            }
        }
    }
    Some(Picture { width: side as u32, height: side as u32, rgba })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_link_is_drawn_in_whole_modules_with_its_quiet_zone() {
        let picture = qr_picture("https://login.tailscale.com/a/0123456789ab".into(), 300).unwrap();
        assert_eq!(picture.width, picture.height);
        assert!(picture.width <= 300);
        assert_eq!(picture.rgba.len(), (picture.width * picture.height * 4) as usize);
        // The quiet zone's corner is white; the finder pattern just inside
        // it is dark.
        let side = picture.width as usize;
        let span = qrcode::QrCode::new(b"https://login.tailscale.com/a/0123456789ab").unwrap().width() + 2 * QUIET_ZONE;
        let scale = side / span;
        assert_eq!(&picture.rgba[0..4], &[255, 255, 255, 255]);
        let inside = ((QUIET_ZONE * scale) * side + QUIET_ZONE * scale) * 4;
        assert_eq!(&picture.rgba[inside..inside + 4], &[0, 0, 0, 255]);
    }

    #[test]
    fn too_small_a_size_draws_nothing() {
        assert!(qr_picture("https://example.com".into(), 10).is_none());
    }
}
