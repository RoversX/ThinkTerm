//! The images this connection has already described to its client.
//!
//! Lines cross the wire without their pixels: a `SerializedImageCell` names
//! its image by hash, and the client fetches the bytes with `GetImageCell`
//! the first time it meets a hash. That request names the cell the image
//! was attached to, and the server used to answer by reading the cell
//! again -- which came back empty whenever the cell had moved on between
//! the push and the fetch: a screen switch, a scroll, a frame that
//! overwrote the picture. The client then painted the cell bare and, with
//! the line stamped clean, never asked again.
//!
//! So every image sent on a pane is remembered here for a while, keyed by
//! hash, and a fetch is answered from memory first. The cell is still
//! consulted for anything this has let go of.

use lru::LruCache;
use std::num::NonZeroUsize;
use std::sync::Arc;
use termwiz::image::ImageData;
use termwiz::surface::Line;
use wezterm_term::StableRowIndex;

/// Per pane, per connection. The byte cap matters more than the count: a
/// single picture can run to tens of megabytes, and this must stay well
/// inside the terminal's own image budget, since the entries here outlive
/// the terminal's copy once it lets an image go.
const MAX_IMAGES: usize = 32;
const MAX_BYTES: usize = 32 * 1024 * 1024;

pub(crate) struct SentImages {
    cache: LruCache<[u8; 32], Arc<ImageData>>,
    max_bytes: usize,
}

impl Default for SentImages {
    fn default() -> Self {
        Self::with_limits(MAX_IMAGES, MAX_BYTES)
    }
}

impl std::fmt::Debug for SentImages {
    fn fmt(&self, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        fmt.debug_struct("SentImages")
            .field("images", &self.cache.len())
            .finish()
    }
}

impl SentImages {
    fn with_limits(max_images: usize, max_bytes: usize) -> Self {
        Self {
            cache: LruCache::new(NonZeroUsize::new(max_images.max(1)).unwrap()),
            max_bytes,
        }
    }

    /// Note every image attached to `lines`, which are about to be sent.
    pub fn remember(&mut self, lines: &[(StableRowIndex, Line)]) {
        for (_, line) in lines {
            for cell in line.visible_cells() {
                let Some(images) = cell.attrs().images() else {
                    continue;
                };
                for image in images {
                    self.insert(Arc::clone(image.image_data()));
                }
            }
        }
    }

    /// Keep `image`; an image already here is merely made recent again.
    pub fn insert(&mut self, image: Arc<ImageData>) {
        let hash = image.hash();
        if self.cache.contains(&hash) {
            self.cache.promote(&hash);
            return;
        }
        self.cache.put(hash, image);
        // Sizes are read now rather than tracked, because an animation
        // grows in place after it was put here.
        while self.cache.len() > 1 && self.bytes() > self.max_bytes {
            self.cache.pop_lru();
        }
    }

    /// The image behind `hash`, if it was sent recently.
    pub fn get(&mut self, hash: &[u8; 32]) -> Option<Arc<ImageData>> {
        self.cache.get(hash).cloned()
    }

    fn bytes(&self) -> usize {
        self.cache.iter().map(|(_, image)| image.len()).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use termwiz::cell::CellAttributes;
    use termwiz::image::{ImageCell, TextureCoordinate};
    use termwiz::surface::SEQ_ZERO;

    fn image(bytes: &[u8]) -> Arc<ImageData> {
        Arc::new(ImageData::with_raw_data(bytes.to_vec()))
    }

    fn line_with(image: &Arc<ImageData>) -> (StableRowIndex, Line) {
        let mut line = Line::from_text("x", &CellAttributes::default(), SEQ_ZERO, None);
        line.cells_mut()[0]
            .attrs_mut()
            .attach_image(Box::new(ImageCell::new(
                TextureCoordinate::new_f32(0., 0.),
                TextureCoordinate::new_f32(1., 1.),
                Arc::clone(image),
            )));
        (0, line)
    }

    #[test]
    fn an_image_sent_with_a_line_is_answered_after_the_line_moved_on() {
        let image = image(b"picture");
        let mut sent = SentImages::default();
        sent.remember(&[line_with(&image)]);
        // The line is gone; only the hash remains.
        let answer = sent
            .get(&image.hash())
            .expect("the sent image is remembered");
        assert!(
            Arc::ptr_eq(&answer, &image),
            "the very object the terminal holds is handed back, so later \
             in-place frame updates are visible through it"
        );
    }

    #[test]
    fn a_hash_never_sent_is_not_answered() {
        let mut sent = SentImages::default();
        sent.remember(&[line_with(&image(b"one"))]);
        assert!(sent.get(&image(b"two").hash()).is_none());
    }

    #[test]
    fn the_oldest_image_goes_first_when_over_the_byte_budget() {
        let mut sent = SentImages::with_limits(10, 25);
        let first = image(&[1u8; 10]);
        let second = image(&[2u8; 10]);
        let third = image(&[3u8; 10]);
        sent.insert(Arc::clone(&first));
        sent.insert(Arc::clone(&second));
        sent.insert(Arc::clone(&third));
        assert!(sent.get(&first.hash()).is_none(), "the oldest is let go");
        assert!(sent.get(&second.hash()).is_some());
        assert!(sent.get(&third.hash()).is_some());
    }

    #[test]
    fn one_image_is_always_kept_even_when_it_alone_exceeds_the_budget() {
        let mut sent = SentImages::with_limits(10, 5);
        let big = image(&[9u8; 100]);
        sent.insert(Arc::clone(&big));
        assert!(sent.get(&big.hash()).is_some());
    }

    #[test]
    fn re_sending_an_image_makes_it_recent_again() {
        let mut sent = SentImages::with_limits(2, usize::MAX);
        let first = image(b"first");
        let second = image(b"second");
        sent.insert(Arc::clone(&first));
        sent.insert(Arc::clone(&second));
        sent.insert(Arc::clone(&first));
        sent.insert(image(b"third"));
        assert!(sent.get(&first.hash()).is_some(), "re-sent, so kept");
        assert!(sent.get(&second.hash()).is_none(), "the stale one went");
    }
}
