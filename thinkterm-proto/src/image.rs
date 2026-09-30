//! Canonical image requests and the shared image payload shape.
use crate::PaneId;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use termwiz::image::{ImageData, ImageDataType};

/// Canonical Kitty pixels, independent of a proxy's native render cache.
#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub struct GetKittyImage {
    pub pane_id: PaneId,
    pub image_id: u32,
    pub data_hash: [u8; 32],
    pub have_frames: u32,
    pub image_epoch: Option<u64>,
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub struct GetImageCellResponse {
    pub pane_id: PaneId,
    pub data: Option<Arc<ImageData>>,
    /// The generation `data` was taken from.
    pub data_generation: u64,
    /// 0: `data` is the whole image. Otherwise `data` is an AnimRgba8
    /// holding only the frames from this index on, together with the
    /// durations and per-frame hashes of *every* frame, so the client can
    /// check the frames it holds are still the ones in front.
    pub frames_from: u32,
}

pub fn image_reply(image: &Arc<ImageData>, have_frames: u32) -> (u64, Arc<ImageData>, u32) {
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
