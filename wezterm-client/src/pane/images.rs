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
pub(crate) fn frame_count(data: &ImageDataType) -> u32 {
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
pub(crate) fn merge_into(
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
