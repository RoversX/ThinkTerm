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
use termwiz::image::{ImageData, ImageDataType};
use termwiz::surface::Line;
use wezterm_term::StableRowIndex;

/// Per pane, per connection. The byte cap matters more than the count: a
/// single picture can run to tens of megabytes, and this must stay well
/// inside the terminal's own image budget, since the entries here outlive
/// the terminal's copy once it lets an image go.
const MAX_IMAGES: usize = 32;
const MAX_BYTES: usize = 8 * 1024 * 1024;

pub(crate) struct SentImages {
    /// Each image with its size when it came in. An animation grows after
    /// that, so the total is an estimate; measuring every entry on every
    /// insert meant a lock per image per frame on the main thread.
    cache: LruCache<[u8; 32], (Arc<ImageData>, usize)>,
    bytes: usize,
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
            bytes: 0,
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
        let size = image.len();
        if let Some((_, evicted)) = self.cache.push(hash, (image, size)) {
            self.bytes = self.bytes.saturating_sub(evicted.1);
        }
        self.bytes += size;
        while self.cache.len() > 1 && self.bytes > self.max_bytes {
            if let Some((_, (_, size))) = self.cache.pop_lru() {
                self.bytes = self.bytes.saturating_sub(size);
            }
        }
    }

    /// The image behind `hash`, if it was sent recently.
    pub fn get(&mut self, hash: &[u8; 32]) -> Option<Arc<ImageData>> {
        self.cache.get(hash).map(|(image, _)| Arc::clone(image))
    }
}

/// What to send for `image` to a client that already holds `have_frames`
/// of it: the generation, the payload, and the index the payload's frames
/// start at (0 for the whole image).
///
/// An animation the client has most of is answered with the frames after
/// the ones it holds, plus the durations and hashes of every frame so it
/// can verify the ones in front are still the same. Anything else, or a
/// client holding nothing, gets the whole image.
pub(crate) fn reply_for(image: &Arc<ImageData>, have_frames: u32) -> (u64, Arc<ImageData>, u32) {
    // Payload and generation under the one guard: the terminal bumps the
    // generation while it still holds the data lock, so what is read here
    // is a matching pair.
    let data = image.data();
    let generation = image.generation();
    if have_frames > 0 {
        if let ImageDataType::AnimRgba8 {
            width,
            height,
            durations,
            frames,
            hashes,
        } = &*data
        {
            let have = (have_frames as usize).min(frames.len());
            // A client holding every frame (its generation was merely
            // behind) gets an empty tail: the hashes let it confirm its
            // frames are still the ones here, at no pixel cost.
            let tail = ImageDataType::AnimRgba8 {
                width: *width,
                height: *height,
                durations: durations.clone(),
                frames: frames[have..].to_vec(),
                hashes: hashes.clone(),
            };
            // Carries the original's hash although it is only part of
            // it: the client files it under that hash, and never
            // re-derives the hash from a delta.
            let delta = Arc::new(ImageData::with_data_and_hash(tail, image.hash()));
            return (generation, delta, have as u32);
        }
    }
    (generation, Arc::clone(image), 0)
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

    fn animation(pixels: &[u8]) -> Arc<ImageData> {
        Arc::new(ImageData::with_data(ImageDataType::AnimRgba8 {
            width: 1,
            height: 1,
            durations: vec![std::time::Duration::from_millis(40); pixels.len()],
            frames: pixels.iter().map(|p| vec![*p; 4]).collect(),
            hashes: pixels.iter().map(|p| [*p; 32]).collect(),
        }))
    }

    fn frames_of(image: &ImageData) -> Vec<Vec<u8>> {
        match &*image.data() {
            ImageDataType::AnimRgba8 { frames, .. } => frames.clone(),
            _ => vec![],
        }
    }

    #[test]
    fn a_client_holding_some_frames_gets_only_the_rest() {
        let image = animation(&[1, 2, 3, 4]);
        image.bump_generation();
        let (generation, payload, from) = reply_for(&image, 2);
        assert_eq!(generation, 1);
        assert_eq!(from, 2);
        assert_eq!(frames_of(&payload), vec![vec![3; 4], vec![4; 4]]);
        assert_eq!(payload.hash(), image.hash(), "filed under the same hash");
        let data = payload.data();
        let ImageDataType::AnimRgba8 {
            hashes, durations, ..
        } = &*data
        else {
            panic!("expected an animation, got {:?}", &*data);
        };
        assert_eq!(hashes.len(), 4, "every frame's hash, for the prefix check");
        assert_eq!(durations.len(), 4);
    }

    #[test]
    fn a_client_holding_nothing_gets_the_whole_image_and_one_holding_all_an_empty_tail() {
        let image = animation(&[1, 2]);
        let (_, payload, from) = reply_for(&image, 0);
        assert!(Arc::ptr_eq(&payload, &image));
        assert_eq!(from, 0);
        // Holding everything: an empty tail, so the client can confirm
        // what it has without a single pixel crossing.
        let (_, payload, from) = reply_for(&image, 2);
        assert!(frames_of(&payload).is_empty());
        assert_eq!(from, 2);
        let (_, payload, from) = reply_for(&image, 5);
        assert!(
            frames_of(&payload).is_empty(),
            "a count from the future is clamped"
        );
        assert_eq!(from, 2);
    }

    #[test]
    fn a_still_image_is_never_answered_with_a_delta() {
        let image = image(b"still");
        let (_, payload, from) = reply_for(&image, 1);
        assert!(Arc::ptr_eq(&payload, &image));
        assert_eq!(from, 0);
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
