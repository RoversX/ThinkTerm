//! Keeping a remote pane's copy of an image in step with the original.
//!
//! The server shares one `Arc<ImageData>` between its terminal and every
//! cell that shows the picture, and an animation grows inside it: frames
//! are appended behind the mutex while the hash, which is the picture's
//! identity and the glyph cache's key, stays put. This side holds a copy
//! made from the wire, which nothing grows. So the copy is grown here: a
//! fetch brings the frames the copy lacks, and they are written into the
//! very Arc the cached lines and the glyph cache already point at. The
//! painter then sees the new frames the way it does for a local pane.

use std::sync::Arc;
use termwiz::image::{ImageData, ImageDataType};

/// The animation frames a copy holds: what to tell the server we have.
pub fn frame_count(data: &ImageDataType) -> u32 {
    match data {
        ImageDataType::AnimRgba8 { frames, .. } => frames.len() as u32,
        ImageDataType::Rgba8 { .. } => 1,
        _ => 0,
    }
}

/// Bring `held` up to date from `fetched`, in place. `frames_from` is 0
/// when `fetched` is the whole image, otherwise the index its frames start
/// at, in which case its durations and hashes cover every frame and the
/// hashes of the frames in front must match the ones held.
///
/// False means `fetched` could not be applied: the frames in front no
/// longer match, or the fetch would leave fewer frames than are held --
/// the painter indexes into the frame list by a cursor it keeps, and the
/// list must never shrink under it. `held` and `fetched` are untouched
/// then; the caller fetches the whole image or adopts `fetched` as is.
pub fn merge_into(
    held: &Arc<ImageData>,
    fetched: &Arc<ImageData>,
    frames_from: u32,
    generation: u64,
) -> bool {
    // Both locks, held first. `fetched` is private to this task, so the
    // order cannot cross anyone else's.
    let mut mine = held.data();
    let mut theirs = fetched.data();

    let merged = if frames_from == 0 {
        if frame_count(&theirs) < frame_count(&mine) {
            return false;
        }
        std::mem::replace(&mut *theirs, ImageDataType::EncodedFile(Vec::new()))
    } else {
        let from = frames_from as usize;
        let matches = match (&*theirs, &*mine) {
            (
                ImageDataType::AnimRgba8 {
                    frames: tail,
                    hashes,
                    ..
                },
                ImageDataType::AnimRgba8 {
                    hashes: held_hashes,
                    ..
                },
            ) => {
                hashes.len() == from + tail.len()
                    && held_hashes.len() == from
                    && held_hashes[..] == hashes[..from]
            }
            (
                ImageDataType::AnimRgba8 {
                    frames: tail,
                    hashes,
                    ..
                },
                ImageDataType::Rgba8 { hash, .. },
            ) => from == 1 && hashes.len() == 1 + tail.len() && hashes[0] == *hash,
            _ => false,
        };
        if !matches {
            return false;
        }
        // `theirs` is taken apart first; `mine` is emptied last and for
        // the shortest possible stretch, since every cached line and the
        // glyph cache point at it.
        let taken = std::mem::replace(&mut *theirs, ImageDataType::EncodedFile(Vec::new()));
        let ImageDataType::AnimRgba8 {
            width,
            height,
            durations,
            frames: tail,
            hashes,
        } = taken
        else {
            unreachable!("checked above");
        };
        let mut frames = match std::mem::replace(&mut *mine, ImageDataType::EncodedFile(Vec::new()))
        {
            ImageDataType::AnimRgba8 { frames, .. } => frames,
            ImageDataType::Rgba8 { data, .. } => vec![data],
            other => {
                *mine = other;
                return false;
            }
        };
        frames.extend(tail);
        ImageDataType::AnimRgba8 {
            width,
            height,
            durations,
            frames,
            hashes,
        }
    };

    *mine = merged;
    drop(theirs);
    drop(mine);
    held.set_generation(generation);
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn still(pixel: u8) -> Arc<ImageData> {
        Arc::new(ImageData::with_data(
            ImageDataType::new_single_frame_content_hashed(1, 1, vec![pixel; 4]),
        ))
    }

    fn frame_hash(pixel: u8) -> [u8; 32] {
        let image = still(pixel);
        let data = image.data();
        match &*data {
            ImageDataType::Rgba8 { hash, .. } => *hash,
            _ => unreachable!(),
        }
    }

    fn anim(pixels: &[u8]) -> ImageDataType {
        ImageDataType::AnimRgba8 {
            width: 1,
            height: 1,
            durations: vec![Duration::from_millis(40); pixels.len()],
            frames: pixels.iter().map(|p| vec![*p; 4]).collect(),
            hashes: pixels.iter().map(|p| frame_hash(*p)).collect(),
        }
    }

    /// A delta as the server builds it: frames from `from` on, every
    /// duration and hash.
    fn delta(pixels: &[u8], from: usize, hash: [u8; 32]) -> Arc<ImageData> {
        let ImageDataType::AnimRgba8 {
            width,
            height,
            durations,
            frames,
            hashes,
        } = anim(pixels)
        else {
            unreachable!()
        };
        Arc::new(ImageData::with_data_and_hash(
            ImageDataType::AnimRgba8 {
                width,
                height,
                durations,
                frames: frames[from..].to_vec(),
                hashes,
            },
            hash,
        ))
    }

    #[test]
    fn appended_frames_grow_the_held_copy_in_place() {
        let held = still(1);
        let hash = held.hash();
        assert!(merge_into(&held, &delta(&[1, 2, 3], 1, hash), 1, 7));
        assert_eq!(frame_count(&held.data()), 3);
        assert_eq!(held.generation(), 7);
        // Then two more frames on top of the three.
        assert!(merge_into(&held, &delta(&[1, 2, 3, 4, 5], 3, hash), 3, 9));
        assert_eq!(frame_count(&held.data()), 5);
        let data = held.data();
        let ImageDataType::AnimRgba8 { frames, hashes, .. } = &*data else {
            panic!("expected an animation, got {:?}", &*data);
        };
        assert_eq!(frames[4], vec![5; 4]);
        assert_eq!(hashes[4], frame_hash(5));
    }

    #[test]
    fn a_delta_whose_leading_frames_differ_is_refused() {
        // The frame in front was edited in place on the server: what we
        // hold is no longer the prefix, so appending would splice a stale
        // frame into a fresh animation.
        let held = still(1);
        let hash = held.hash();
        assert!(!merge_into(&held, &delta(&[9, 2], 1, hash), 1, 3));
        assert_eq!(frame_count(&held.data()), 1, "left alone");
        assert_eq!(held.generation(), 0, "left alone");
    }

    #[test]
    fn a_whole_image_replaces_the_copy_but_never_shrinks_it() {
        let held = still(1);
        let hash = held.hash();
        let whole = Arc::new(ImageData::with_data_and_hash(anim(&[1, 2]), hash));
        assert!(merge_into(&held, &whole, 0, 2));
        assert_eq!(frame_count(&held.data()), 2);

        let shorter = Arc::new(ImageData::with_data_and_hash(anim(&[1]), hash));
        assert!(!merge_into(&held, &shorter, 0, 3));
        assert_eq!(frame_count(&held.data()), 2, "a painter may be on frame 2");
    }
}

/// How much the images fetched from remote panes may hold: a count, and
/// above all bytes. A program streaming full-window pictures gives every
/// frame a new hash, and with the count alone the store held the last
/// hundred-odd frames of it -- half a gigabyte of pixels nothing would
/// show again. A picture larger than the whole budget is kept on its
/// own.
pub const MAX_IMAGES: usize = 128;
pub const MAX_IMAGE_BYTES: usize = 64 * 1024 * 1024;

/// The images fetched from remote panes, by connection and hash, with the
/// size each was last measured at.
pub struct ImageStore {
    images: lru::LruCache<(crate::host::ImageDomainKey, [u8; 32]), (Arc<ImageData>, usize)>,
    bytes: usize,
    max_bytes: usize,
}

impl Default for ImageStore {
    fn default() -> Self {
        Self::new(MAX_IMAGES, MAX_IMAGE_BYTES)
    }
}

impl ImageStore {
    pub fn new(max_images: usize, max_bytes: usize) -> Self {
        Self {
            images: lru::LruCache::new(std::num::NonZeroUsize::new(max_images.max(1)).unwrap()),
            bytes: 0,
            max_bytes,
        }
    }

    pub fn get(&mut self, key: &(crate::host::ImageDomainKey, [u8; 32])) -> Option<Arc<ImageData>> {
        self.images.get(key).map(|(data, _)| Arc::clone(data))
    }

    /// File `data` under `key`, measured now: the same Arc filed again
    /// after it grew is measured again. The least recently used go while
    /// the total is over budget, though never the one just filed.
    pub fn put(&mut self, key: (crate::host::ImageDomainKey, [u8; 32]), data: Arc<ImageData>) {
        if let Some((_, old)) = self.images.pop(&key) {
            self.bytes = self.bytes.saturating_sub(old);
        }
        let size = data.len();
        if let Some((_, (_, evicted))) = self.images.push(key, (data, size)) {
            self.bytes = self.bytes.saturating_sub(evicted);
        }
        self.bytes += size;
        while self.images.len() > 1 && self.bytes > self.max_bytes {
            if let Some((_, (_, size))) = self.images.pop_lru() {
                self.bytes = self.bytes.saturating_sub(size);
            }
        }
    }

    pub fn forget_domain(&mut self, domain: crate::host::ImageDomainKey) {
        let stale: Vec<(crate::host::ImageDomainKey, [u8; 32])> = self
            .images
            .iter()
            .filter(|((held, _), _)| *held == domain)
            .map(|(key, _)| *key)
            .collect();
        for key in stale {
            if let Some((_, size)) = self.images.pop(&key) {
                self.bytes = self.bytes.saturating_sub(size);
            }
        }
    }

    /// Images held and the bytes they were last measured at.
    pub fn footprint(&self) -> (usize, usize) {
        (self.images.len(), self.bytes)
    }
}

/// File `data` under its hash and return the Arc to attach to cells. When
/// a copy is already filed there, that copy is brought up to date from
/// `data` and is the one returned: the glyph cache is keyed by hash and
/// keeps the first Arc it met, so a second Arc under the same hash would
/// leave the painter on the old one, blind to every frame that grows in
/// the new. Two fetches can miss the same hash at once (a row fetch and a
/// push both naming it), and a fetch answered with the picture now in the
/// cell lands under a hash that may well be filed already.
pub fn file_image(
    store: &crate::Lock<ImageStore>,
    domain: crate::host::ImageDomainKey,
    data: Arc<ImageData>,
) -> Arc<ImageData> {
    let key = (domain, data.hash());
    let existing = store.lock().get(&key);
    let filed = match existing {
        Some(existing) if !Arc::ptr_eq(&existing, &data) => {
            if !merge_into(&existing, &data, 0, data.generation()) {
                // The copy holds more than the fetch brought, and is
                // therefore at least as current as it.
                existing.set_generation(existing.generation().max(data.generation()));
            }
            existing
        }
        _ => data,
    };
    // Filed again whichever way: a copy that just grew, in place or by
    // merge, is measured again here.
    store.lock().put(key, Arc::clone(&filed));
    filed
}

#[cfg(test)]
mod store_tests {
    use super::*;

    /// A stream of full-window frames must not pin its last hundred
    /// frames: the store is bounded in bytes, and the newest stays.
    #[test]
    fn the_image_store_lets_old_frames_go_when_over_its_byte_budget() {
        let mut store = ImageStore::new(128, 100);
        let frame = |n: u8| {
            Arc::new(ImageData::with_data(ImageDataType::AnimRgba8 {
                width: 1,
                height: 1,
                durations: vec![std::time::Duration::from_millis(40)],
                frames: vec![vec![n; 40]],
                hashes: vec![[n; 32]],
            }))
        };
        let frames: Vec<Arc<ImageData>> = (1..=5).map(frame).collect();
        for f in &frames {
            store.put((3, f.hash()), Arc::clone(f));
        }
        assert_eq!(store.footprint(), (2, 80), "two of forty fit in a hundred");
        assert!(
            store.get(&(3, frames[4].hash())).is_some(),
            "the newest stays"
        );
        assert!(
            store.get(&(3, frames[0].hash())).is_none(),
            "the oldest went"
        );

        let big = Arc::new(ImageData::with_raw_data(vec![7; 500]));
        store.put((3, big.hash()), Arc::clone(&big));
        assert_eq!(
            store.footprint(),
            (1, 500),
            "a picture over the budget is kept alone"
        );

        store.forget_domain(3);
        assert_eq!(store.footprint(), (0, 0));
    }

    /// The glyph cache keys on the hash and keeps the first Arc it met:
    /// a hash that is already filed keeps its Arc, brought up to date
    /// from what was fetched, whatever the fetch brought.
    #[test]
    fn a_hash_already_filed_keeps_its_arc_and_grows_it() {
        let hash = [77u8; 32];
        let anim = |pixels: &[u8]| ImageDataType::AnimRgba8 {
            width: 1,
            height: 1,
            durations: vec![std::time::Duration::from_millis(40); pixels.len()],
            frames: pixels.iter().map(|p| vec![*p; 4]).collect(),
            hashes: pixels.iter().map(|p| [*p; 32]).collect(),
        };
        let frames = |image: &ImageData| frame_count(&image.data());
        let store = crate::Lock::new(ImageStore::new(128, 1 << 20));
        let domain = 3;

        let first = Arc::new(ImageData::with_data_and_hash(anim(&[1, 2]), hash));
        let filed = file_image(&store, domain, Arc::clone(&first));
        assert!(
            Arc::ptr_eq(&filed, &first),
            "nothing filed yet: this Arc is the one"
        );

        let longer = Arc::new(ImageData::with_data_and_hash(anim(&[1, 2, 3]), hash));
        longer.set_generation(3);
        let filed = file_image(&store, domain, longer);
        assert!(Arc::ptr_eq(&filed, &first), "the Arc already filed is kept");
        assert_eq!(frames(&first), 3, "and holds what the fetch brought");
        assert_eq!(first.generation(), 3);

        let shorter = Arc::new(ImageData::with_data_and_hash(anim(&[1]), hash));
        shorter.set_generation(7);
        let filed = file_image(&store, domain, shorter);
        assert!(Arc::ptr_eq(&filed, &first));
        assert_eq!(frames(&first), 3, "a copy never shrinks under the painter");
        assert_eq!(
            first.generation(),
            7,
            "but counts as current, or every push would fetch it again"
        );
        assert_eq!(store.lock().footprint().0, 1);
    }
}
