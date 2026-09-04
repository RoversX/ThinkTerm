use crate::terminalstate::image::*;
use crate::terminalstate::{ImageAttachParams, PlacementInfo};
use crate::{StableRowIndex, TerminalState};
use ::image::{
    DynamicImage, GenericImage, GenericImageView, ImageBuffer, RgbImage, Rgba, RgbaImage,
};
use anyhow::Context;
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::sync::Arc;
use std::time::Duration;
use wezterm_cell::image::ImageDataType;
use wezterm_escape_parser::apc::{
    KittyFrameCompositionMode, KittyImage, KittyImageCompression, KittyImageData, KittyImageDelete,
    KittyImageFormat, KittyImageFrame, KittyImageFrameCompose, KittyImagePlacement,
    KittyImageTransmit, KittyImageVerbosity,
};
use wezterm_surface::change::ImageData;

/// A chunked transfer holds every fragment in memory until the final `m=0`
/// arrives. A producer that never sends it — a runaway program as easily as a
/// hostile one — grows the accumulator without bound. The byte limit is set
/// well above a legal single image (100MB decoded is roughly 133MB of base64)
/// so that honest transfers are unaffected.
pub(crate) const MAX_ACCUM_BYTES: usize = 256 * 1024 * 1024;
pub(crate) const MAX_ACCUM_CHUNKS: usize = 65536;

/// An animation grows one frame at a time with no natural end, and `d=f`
/// (drop the frames again) is not implemented, so without a ceiling a client
/// can append until the process dies. Sized for real content: a terminal-sized
/// 640x384 frame is under a megabyte, so this is a few hundred of them.
pub(crate) const MAX_ANIM_BYTES: usize = 256 * 1024 * 1024;

/// The byte cap cannot see the per-frame bookkeeping (frame Vec, duration,
/// hash), which for tiny frames dwarfs the pixels: 256MB worth of 2x2 frames
/// is sixteen million entries. Real animations run into the byte cap first;
/// this one bounds the degenerate shapes.
pub(crate) const MAX_ANIM_FRAMES: usize = 4096;

/// What the opening fragment of a chunked transfer asked for. The fragments
/// that follow carry payload only, so this is captured from the head and
/// decides which command the reassembled transfer becomes.
enum ChunkedTransfer {
    Transmit,
    Display(KittyImagePlacement),
    Frame(KittyImageFrame),
}

#[derive(Debug, Default)]
pub struct KittyImageState {
    accumulator: Vec<KittyImage>,
    accumulated_bytes: usize,
    /// Set when a transfer was abandoned for exceeding the limits above, and
    /// cleared by the `m=0` that ends it. Without it every subsequent chunk
    /// would log and reply again, turning one bad transfer into a flood.
    accumulator_overflowed: bool,
    max_image_id: u32,
    number_to_id: HashMap<u32, u32>,
    id_to_data: HashMap<u32, Arc<ImageData>>,
    /// Transmission order of each stored image, so eviction drops the
    /// oldest unplaced image first. A HashMap walk would evict at random,
    /// which for a frame stream means keeping stale frames over fresh ones.
    id_seq: HashMap<u32, u64>,
    next_seq: u64,
    placements: HashMap<(u32, Option<u32>), PlacementInfo>,
    used_memory: usize,
}

impl KittyImageState {
    /// Bytes actually held: small pictures with identical pixels share one
    /// `ImageData` through the content-hash cache, so summing per id would
    /// count a logo re-emitted under fresh ids once per prompt, and the
    /// sweep acting on that phantom total would detach real pictures.
    fn recompute_used_memory(&mut self) {
        let mut seen: HashSet<*const ImageData> = HashSet::new();
        self.used_memory = self
            .id_to_data
            .values()
            .filter(|data| seen.insert(Arc::as_ptr(data)))
            .map(|data| data.len())
            .sum();
    }

    fn remove_data_for_id(&mut self, image_id: u32) {
        self.id_to_data.remove(&image_id);
        self.id_seq.remove(&image_id);
        self.recompute_used_memory();
    }

    /// Eviction, as opposed to replacement: an image number that named the
    /// evicted id must not keep resolving to it, or every later `a=p,I=n`
    /// fails on an id that no longer exists.
    fn evict(&mut self, image_id: u32) {
        self.remove_data_for_id(image_id);
        self.number_to_id.retain(|_, id| *id != image_id);
    }

    /// The stored image that was transmitted earliest, skipping the most
    /// recent one (it is about to be placed).
    fn oldest_stored_image_except_newest(&self) -> Option<u32> {
        let newest = self.next_seq;
        self.id_to_data
            .keys()
            .filter_map(|id| {
                let seq = self.id_seq.get(id).copied().unwrap_or(0);
                (seq != newest).then_some((seq, *id))
            })
            .min()
            .map(|(_, id)| id)
    }

    /// An appended animation frame is the newest image data in the store,
    /// even though it arrived through `a=f` rather than a transmit. Without
    /// this the sweep would protect some later still picture and evict the
    /// animation that is being streamed into, mid-stream.
    fn mark_newest(&mut self, image_id: u32) {
        if self.id_seq.contains_key(&image_id) {
            self.next_seq += 1;
            self.id_seq.insert(image_id, self.next_seq);
        }
    }

    fn record_id_to_data(&mut self, image_id: u32, data: Arc<ImageData>, budget: usize) {
        // Unconditionally, id 0 included. The insert below replaces whatever
        // was at this key either way, so skipping the bookkeeping for the
        // anonymous-transmission key does not keep that image alive — it only
        // loses track of its bytes. Left uncounted, `used_memory` climbs
        // forever, and once it passes the budget `prune_unreferenced` starts
        // evicting every unplaced image on every transfer, which breaks
        // transmit-now-place-later.
        self.remove_data_for_id(image_id);
        self.id_to_data.insert(image_id, data);
        self.recompute_used_memory();
        self.next_seq += 1;
        self.id_seq.insert(image_id, self.next_seq);
        if self.next_seq % 100 == 0 {
            log::info!(
                "kitty store: images={} bytes={} placements={} (after {} transmits)",
                self.id_to_data.len(),
                self.used_memory,
                self.placements.len(),
                self.next_seq
            );
        }
        // After the insert, so the budget holds strictly; the image just
        // stored is the newest and prune never evicts that one, which keeps
        // transmit-now-place-later working.
        self.prune_unreferenced(budget);
    }

    #[cfg(test)]
    pub(crate) fn used_memory(&self) -> usize {
        self.used_memory
    }

    /// Evict unplaced images, oldest transmission first, until the stored
    /// bytes fit `budget`. Placed images and the most recently transmitted
    /// image are never touched.
    pub(crate) fn prune_unreferenced(&mut self, budget: usize) {
        if self.used_memory <= budget {
            return;
        }
        let referenced: HashSet<u32> = self.placements.keys().map(|(k, _)| *k).collect();
        let newest = self.next_seq;
        let before = self.used_memory;
        let mut candidates: Vec<(u64, u32)> = self
            .id_to_data
            .keys()
            .filter(|id| !referenced.contains(id) && self.id_seq.get(id).copied() != Some(newest))
            .map(|id| (self.id_seq.get(id).copied().unwrap_or(0), *id))
            .collect();
        candidates.sort_unstable();
        // Re-measured after every eviction rather than summed from lengths:
        // dropping one id of a shared picture frees nothing.
        for (_, id) in candidates {
            if self.used_memory <= budget {
                break;
            }
            self.evict(id);
        }
        // Debug, not info: a frame stream prunes on nearly every transfer.
        log::debug!(
            "using {} RAM for images, pruned {} (budget {})",
            self.used_memory,
            before.saturating_sub(self.used_memory),
            budget
        );
    }
}

#[cfg(test)]
mod prune_tests {
    use super::KittyImageState;
    use std::sync::Arc;
    use wezterm_cell::image::{ImageData, ImageDataType};

    fn image(side: u32) -> Arc<ImageData> {
        Arc::new(ImageData::with_data(ImageDataType::new_single_frame(
            side,
            side,
            vec![0u8; (side * side * 4) as usize],
        )))
    }

    #[test]
    fn the_oldest_unplaced_images_go_first_and_placed_ones_never() {
        let mut state = KittyImageState::default();
        let one = image(16).len();
        let budget = 3 * one;
        for id in 1..=5 {
            state.record_id_to_data(id, image(16), budget);
        }
        // Five transfers against a three-image budget hold the three newest.
        let mut kept: Vec<u32> = state.id_to_data.keys().copied().collect();
        kept.sort();
        assert_eq!(kept, vec![3, 4, 5]);
        assert_eq!(state.used_memory, budget);

        // Re-transmitting an id replaces it without double counting.
        state.record_id_to_data(5, image(16), budget);
        assert_eq!(state.used_memory, budget);
        assert_eq!(state.id_to_data.len(), 3);
    }
}

/// True for a fragment that can only be the continuation of a chunked
/// transfer: no action key (`a=` defaults to `t`), no identifying or format
/// keys, just `m=` and payload. `q=` is deliberately not consulted — the
/// protocol permits it on continuation fragments.
fn is_bare_continuation(img: &KittyImage) -> bool {
    match img {
        KittyImage::TransmitData { transmit, .. } => {
            transmit.format.is_none()
                && transmit.width.is_none()
                && transmit.height.is_none()
                && transmit.image_id.is_none()
                && transmit.image_number.is_none()
                && transmit.compression == KittyImageCompression::None
                && matches!(transmit.data, KittyImageData::Direct(_))
        }
        _ => false,
    }
}

/// The physical rows a placement still covers. Rows that have scrolled out
/// of the buffer are skipped rather than substituted: `Screen::stable_range`
/// answers an unknown range with the top or bottom of the buffer, which for
/// a placement means touching rows that never carried it.
fn placement_phys_rows(screen: &crate::Screen, info: &PlacementInfo) -> Vec<crate::PhysRowIndex> {
    (info.first_row..info.first_row + info.rows as StableRowIndex)
        .filter_map(|row| screen.stable_row_to_phys(row))
        .collect()
}

impl TerminalState {
    #[cfg(test)]
    pub(crate) fn kitty_used_memory(&self) -> usize {
        self.kitty_img.used_memory()
    }

    /// Kitty's own rule for a full image store: the oldest images go,
    /// placed ones included, placements and all. Without it an application
    /// that never deletes -- a frame stream placing every frame under a
    /// fresh id, so every frame stays attached to its cells under the next
    /// one -- grows the terminal without bound until it happens to clear
    /// the screen. Unplaced images go first; the image transmitted last is
    /// always kept.
    pub(crate) fn kitty_enforce_image_budget(&mut self) {
        let budget = self.config.kitty_image_memory_budget();
        self.kitty_img.prune_unreferenced(budget);
        while self.kitty_img.used_memory > budget {
            let Some(victim) = self.kitty_img.oldest_stored_image_except_newest() else {
                break;
            };
            log::debug!(
                "kitty store over budget ({} > {}): evicting placed image {victim}",
                self.kitty_img.used_memory,
                budget
            );
            self.kitty_remove_placement(victim, None);
            self.kitty_img.evict(victim);
        }
    }

    fn kitty_img_place(
        &mut self,
        image_id: Option<u32>,
        image_number: Option<u32>,
        placement: KittyImagePlacement,
        verbosity: KittyImageVerbosity,
    ) -> anyhow::Result<()> {
        let image_id = match image_id {
            Some(id) => id,
            None => *self
                .kitty_img
                .number_to_id
                .get(
                    &image_number
                        .ok_or_else(|| anyhow::anyhow!("no image_id or image_number specified!"))?,
                )
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "image_number has no matching image id {:?} in number_to_id",
                        image_number
                    )
                })?,
        };

        log::trace!(
            "kitty_img_place image_id {:?} image_no {:?} placement {:?} verb {:?}",
            image_id,
            image_number,
            placement,
            verbosity
        );

        // Replace any previous placement under this key before the virtual
        // check below: converting a real placement to a virtual one must
        // erase the pixels the real one painted, or they stay on screen with
        // nothing left that can remove them.
        if image_id != 0 {
            self.kitty_remove_placement(image_id, placement.placement_id);
        }

        if placement.virtual_placement {
            // A virtual placement is a promise that the image is ready; the
            // application decides where it goes by printing U+10EEEE
            // placeholder cells. Rendering those is not implemented, so draw
            // nothing rather than painting a real placement at the cursor —
            // that would put a stray image on screen, move the cursor, and
            // leave the placeholder cells rendering as tofu on top of it.
            log::trace!("ignoring virtual placement for image_id {}", image_id);
            return Ok(());
        }
        let img = Arc::clone(self.kitty_img.id_to_data.get(&image_id).ok_or_else(|| {
            anyhow::anyhow!(
                "no matching image id {} in id_to_data for image_number {:?}",
                image_id,
                image_number
            )
        })?);

        let (image_width, image_height) = img.data().dimensions()?;

        // Kitty evicts by last use, not by transmit order: a picture sent
        // once and re-placed on every redraw must outlive frames streamed
        // after it, or the client's next `a=p` fails on a missing id.
        self.kitty_img.mark_newest(image_id);
        let info = self.assign_image_to_cells(ImageAttachParams {
            image_width,
            image_height,
            source_width: placement.w,
            source_height: placement.h,
            source_origin_x: placement.x.unwrap_or(0),
            source_origin_y: placement.y.unwrap_or(0),
            cell_padding_left: placement.x_offset.unwrap_or(0) as u16,
            cell_padding_top: placement.y_offset.unwrap_or(0) as u16,
            data: img,
            style: ImageAttachStyle::Kitty,
            z_index: placement.z_index.unwrap_or(0),
            columns: placement.columns.map(|x| x as usize),
            rows: placement.rows.map(|x| x as usize),
            image_id: Some(image_id),
            placement_id: placement.placement_id,
            do_not_move_cursor: placement.do_not_move_cursor,
        })?;

        self.kitty_img
            .placements
            .insert((image_id, placement.placement_id), info);
        log::trace!(
            "record placement for {} (image_number {:?}) {:?}",
            image_id,
            image_number,
            placement.placement_id
        );

        Ok(())
    }

    /// Drops any parked fragments and gives their memory back. `Vec::clear`
    /// would keep the allocation, which is the thing being reclaimed here.
    pub(crate) fn kitty_reset_accumulator(&mut self) {
        self.kitty_img.accumulator = Vec::new();
        self.kitty_img.accumulated_bytes = 0;
        self.kitty_img.accumulator_overflowed = false;
    }

    /// Routes one fragment of a possibly-chunked transfer: park it until the
    /// closing `m=0` arrives, or run it now if this is that closing fragment.
    /// A transfer that outgrows the limits is abandoned rather than allowed to
    /// consume the terminal's memory, and the client is told once.
    fn kitty_accumulate_or_run(
        &mut self,
        img: KittyImage,
        more_data_follows: bool,
    ) -> anyhow::Result<()> {
        if self.kitty_img.accumulator_overflowed {
            // Nothing that arrives after an abandoned transfer can be decoded,
            // since the fragments we would have needed are already gone. But
            // only that transfer's own residue may be swallowed: a true
            // continuation fragment carries nothing but m= and payload, while
            // anything with an identifying key is the next program's opening
            // fragment — if the producer that overflowed was killed, the m=0
            // this latch waits for never comes at all.
            if is_bare_continuation(&img) {
                if !more_data_follows {
                    self.kitty_reset_accumulator();
                }
                return Ok(());
            }
            // A client that redundantly repeats its keys on continuation
            // fragments lands here and has its dead transfer's tail parsed as
            // a fresh one, which decodes to garbage and earns an error —
            // still better than eating a healthy image. A keyless standalone
            // transfer is misread the other way, but without s=/v= it could
            // never have decoded anyway.
            self.kitty_reset_accumulator();
        }

        let chunk_len = match &img {
            KittyImage::TransmitData { transmit, .. }
            | KittyImage::TransmitDataAndDisplay { transmit, .. }
            | KittyImage::TransmitFrame { transmit, .. } => transmit.data.in_memory_len(),
            _ => 0,
        };

        // Charge the fragment before taking it, and charge the closing one as
        // well. Letting `m=0` through unmeasured would let a transfer park the
        // whole budget and then hand over a second payload just as large, and
        // taking a fragment before checking would overshoot by however much
        // one APC can carry.
        if self.kitty_img.accumulated_bytes + chunk_len > MAX_ACCUM_BYTES
            || self.kitty_img.accumulator.len() + 1 > MAX_ACCUM_CHUNKS
        {
            self.kitty_reject_transfer(&img, more_data_follows);
            return Ok(());
        }

        if !more_data_follows {
            return self.kitty_img_inner(img);
        }

        self.kitty_img.accumulated_bytes += chunk_len;
        self.kitty_img.accumulator.push(img);

        Ok(())
    }

    /// Abandons the transfer in progress and tells the client once.
    ///
    /// Both the identifying keys and the `q=` verbosity ride on the opening
    /// fragment — continuation fragments carry only `m=`, which would read as
    /// the default `q=0` — so a transfer that asked for silence keeps it.
    fn kitty_reject_transfer(&mut self, current: &KittyImage, more_data_follows: bool) {
        log::warn!(
            "abandoning kitty image transfer: {} bytes across {} chunks reached \
             the {} byte / {} chunk limit",
            self.kitty_img.accumulated_bytes,
            self.kitty_img.accumulator.len(),
            MAX_ACCUM_BYTES,
            MAX_ACCUM_CHUNKS
        );

        let (verbosity, image_id, image_number) = {
            let opening = self.kitty_img.accumulator.first().unwrap_or(current);
            let (image_id, image_number) = match opening {
                KittyImage::TransmitData { transmit, .. }
                | KittyImage::TransmitDataAndDisplay { transmit, .. }
                | KittyImage::TransmitFrame { transmit, .. } => {
                    (transmit.image_id, transmit.image_number)
                }
                _ => (None, None),
            };
            (opening.verbosity(), image_id, image_number)
        };

        self.kitty_reset_accumulator();
        // Latch only while the transfer is still running. A rejected closing
        // fragment has already ended it, and latching here would swallow the
        // opening fragment of whatever comes next.
        self.kitty_img.accumulator_overflowed = more_data_follows;

        self.kitty_send_response(
            verbosity,
            false,
            image_id,
            image_number,
            "EFBIG:chunked transfer too large".to_string(),
        );
    }

    /// Abandons the transfer in progress because an unrelated command arrived
    /// in the middle of it, and tells its opener. Leaving the fragments
    /// parked instead means the next transmission is glued onto them: one
    /// image corrupted, the other never created.
    fn kitty_interrupt_transfer(&mut self) {
        let (verbosity, image_id, image_number) = {
            let opening = &self.kitty_img.accumulator[0];
            let (image_id, image_number) = match opening {
                KittyImage::TransmitData { transmit, .. }
                | KittyImage::TransmitDataAndDisplay { transmit, .. }
                | KittyImage::TransmitFrame { transmit, .. } => {
                    (transmit.image_id, transmit.image_number)
                }
                _ => (None, None),
            };
            (opening.verbosity(), image_id, image_number)
        };

        self.kitty_reset_accumulator();

        self.kitty_send_response(
            verbosity,
            false,
            image_id,
            image_number,
            "EINVAL:chunked transfer interrupted".to_string(),
        );
    }

    fn kitty_img_inner(&mut self, img: KittyImage) -> anyhow::Result<()> {
        match self
            .coalesce_kitty_accumulation(img)
            .context("coalesce_kitty_accumulation")?
        {
            KittyImage::TransmitData {
                transmit,
                verbosity,
            } => {
                self.kitty_img_transmit(transmit, verbosity)?;
                Ok(())
            }
            KittyImage::TransmitDataAndDisplay {
                transmit,
                placement,
                verbosity,
            } => {
                log::trace!("TransmitDataAndDisplay {:#?} {:#?}", transmit, placement);
                // Store quietly and answer only once the display half is
                // done: acknowledging the transmit alone claims success for
                // an image that may never appear.
                let (image_id, image_number) =
                    self.kitty_img_transmit_quietly(transmit, verbosity)?;
                match self.kitty_img_place(Some(image_id), image_number, placement, verbosity) {
                    Ok(()) => {
                        if image_id != 0 || image_number.is_some() {
                            self.kitty_send_response(
                                verbosity,
                                true,
                                Some(image_id),
                                image_number,
                                "OK".to_string(),
                            );
                        }
                        Ok(())
                    }
                    Err(err) => {
                        if image_id != 0 || image_number.is_some() {
                            self.kitty_send_response(
                                verbosity,
                                false,
                                Some(image_id),
                                image_number,
                                format!("ERROR:{:#}", err),
                            );
                        }
                        Err(err)
                    }
                }
            }
            KittyImage::TransmitFrame {
                transmit,
                frame,
                verbosity,
            } => {
                if let Err(err) = self.kitty_frame_transmit(transmit, frame, verbosity) {
                    log::error!("Error {:#} while handling KittyImage::TransmitFrame", err,);
                }
                Ok(())
            }
            other => anyhow::bail!("{:?} is not a transfer", other),
        }
    }

    pub(crate) fn kitty_img(&mut self, img: KittyImage) -> anyhow::Result<()> {
        log::trace!("{:?}", img);
        if !self.config.enable_kitty_graphics() {
            return Ok(());
        }

        // The latch never coexists with parked fragments: rejection resets
        // the accumulator before arming it.
        debug_assert!(
            !self.kitty_img.accumulator_overflowed || self.kitty_img.accumulator.is_empty()
        );

        // Any command other than a transmission ends the chunked transfer it
        // interrupts. The spec forbids interleaving, and the alternative is
        // worse than an error: the parked fragments would wait for the next
        // transmission and quietly absorb it. The overflow latch is left
        // alone — that transfer was already reported once, and its remaining
        // fragments may still be streaming in.
        if !self.kitty_img.accumulator.is_empty()
            && !matches!(
                img,
                KittyImage::TransmitData { .. }
                    | KittyImage::TransmitDataAndDisplay { .. }
                    | KittyImage::TransmitFrame { .. }
            )
        {
            self.kitty_interrupt_transfer();
        }

        let verbosity = img.verbosity();
        match img {
            // `verbosity` above is `img.verbosity()`, which now reports the
            // query's own `q=` rather than assuming q=0.
            KittyImage::Query { transmit, .. } => match transmit.data.load_data() {
                Ok(_) => {
                    self.kitty_send_response(
                        verbosity,
                        true,
                        transmit.image_id,
                        transmit.image_number,
                        "OK".to_string(),
                    );
                }
                Err(err) => {
                    self.kitty_send_response(
                        verbosity,
                        false,
                        transmit.image_id,
                        transmit.image_number,
                        format!("ERROR:{:#}", err),
                    );
                }
            },
            KittyImage::TransmitData {
                transmit,
                verbosity,
            } => {
                let more_data_follows = transmit.more_data_follows;
                let img = KittyImage::TransmitData {
                    transmit,
                    verbosity,
                };
                self.kitty_accumulate_or_run(img, more_data_follows)?;
            }
            KittyImage::TransmitDataAndDisplay {
                transmit,
                placement,
                verbosity,
            } => {
                let more_data_follows = transmit.more_data_follows;
                let img = KittyImage::TransmitDataAndDisplay {
                    transmit,
                    placement,
                    verbosity,
                };
                self.kitty_accumulate_or_run(img, more_data_follows)?;
            }
            KittyImage::Display {
                image_id,
                image_number,
                placement,
                verbosity,
            } => {
                self.kitty_img_place(image_id, image_number, placement, verbosity)?;
            }
            KittyImage::Delete {
                what:
                    KittyImageDelete::ByImageId {
                        image_id,
                        placement_id,
                        delete,
                    },
                verbosity,
            } => {
                log::trace!(
                    "remove a placement: image_id {} placement_id {:?} delete {} verb {:?}",
                    image_id,
                    placement_id,
                    delete,
                    verbosity
                );

                self.kitty_remove_placement(image_id, placement_id);

                if delete {
                    self.kitty_img.remove_data_for_id(image_id);
                }
            }
            KittyImage::Delete {
                what: KittyImageDelete::All { delete },
                verbosity: _,
            } => {
                self.kitty_remove_all_placements(delete);
            }
            KittyImage::Delete { what, verbosity } => {
                log::warn!("unhandled KittyImage::Delete {:?} {:?}", what, verbosity);
            }
            KittyImage::TransmitFrame {
                transmit,
                frame,
                verbosity,
            } => {
                let more_data_follows = transmit.more_data_follows;
                let img = KittyImage::TransmitFrame {
                    transmit,
                    frame,
                    verbosity,
                };
                self.kitty_accumulate_or_run(img, more_data_follows)?;
            }
            KittyImage::ComposeFrame { frame, verbosity } => {
                if let Err(err) = self.kitty_frame_compose(frame, verbosity) {
                    log::error!("Error {:#} while handling KittyImage::ComposeFrame", err);
                }
            }
        };

        Ok(())
    }

    /// Marks every line covered by a placement of `image_id` as changed.
    ///
    /// Editing an animation edits pixels behind a shared `Arc`: it touches no
    /// `Line` and bumps no sequence number, so the quad cache goes on serving
    /// the frame it first rendered and a mux server decides the line is clean
    /// and never resends it. The image identity is deliberately left alone —
    /// it doubles as the glyph cache's key, and changing it per frame would
    /// rebuild the decoded image on every paint and reset the animation clock.
    fn kitty_touch_placements_for_image(&mut self, image_id: u32) {
        let placements: Vec<PlacementInfo> = self
            .kitty_img
            .placements
            .iter()
            .filter(|((id, _), _)| *id == image_id)
            .map(|(_, info)| *info)
            .collect();

        let seqno = self.seqno;
        for info in placements {
            // The recorded screen, not the active one: frames keep arriving
            // for a picture on the primary screen while a full-screen app
            // has the alternate one up. Dirtying the active screen instead
            // left the real rows clean (a mux server never resent them) and
            // marked unrelated rows of the other screen changed.
            let screen = self.screen.screen_for_alt_mut(info.alt_screen);
            for idx in placement_phys_rows(screen, &info) {
                screen.line_mut(idx).update_last_change_seqno(seqno);
            }
        }
    }

    fn kitty_remove_placement_from_model(
        &mut self,
        image_id: u32,
        placement_id: Option<u32>,
        info: PlacementInfo,
    ) {
        let seqno = self.seqno;
        // The recorded screen, not the active one: the sweep can run while
        // the other screen is up (a stream in one pane, btop in this one),
        // and StableRowIndex only means anything on the screen it came from.
        let screen = self.screen.screen_for_alt_mut(info.alt_screen);
        for idx in placement_phys_rows(screen, &info) {
            let line = screen.line_mut(idx);
            for c in line.cells_mut() {
                c.attrs_mut()
                    .detach_image_with_placement(image_id, placement_id);
            }
            line.update_last_change_seqno(seqno);
        }
    }

    fn kitty_remove_placement(&mut self, image_id: u32, placement_id: Option<u32>) {
        if placement_id.is_some() {
            if let Some(info) = self.kitty_img.placements.remove(&(image_id, placement_id)) {
                log::trace!("removed placement {} {:?}", image_id, placement_id);
                self.kitty_remove_placement_from_model(image_id, placement_id, info);
            }
        } else {
            let mut to_clear = vec![];
            for (id, p) in self.kitty_img.placements.keys() {
                if *id == image_id {
                    to_clear.push(*p);
                }
            }
            for p in to_clear.into_iter() {
                if let Some(info) = self.kitty_img.placements.remove(&(image_id, p)) {
                    self.kitty_remove_placement_from_model(image_id, p, info);
                }
            }
        }

        log::trace!(
            "after remove: there are {} placements, {} images, {} memory",
            self.kitty_img.placements.len(),
            self.kitty_img.id_to_data.len(),
            self.kitty_img.used_memory,
        );
    }

    pub(crate) fn kitty_remove_all_placements(&mut self, delete: bool) {
        for ((image_id, p), info) in std::mem::take(&mut self.kitty_img.placements).into_iter() {
            self.kitty_remove_placement_from_model(image_id, p, info);
        }
        if delete {
            self.kitty_img.id_to_data.clear();
            self.kitty_img.id_seq.clear();
            self.kitty_img.used_memory = 0;
            self.kitty_img.number_to_id.clear();
        }
    }

    fn kitty_send_response(
        &mut self,
        verbosity: KittyImageVerbosity,
        success: bool,
        image_id: Option<u32>,
        image_no: Option<u32>,
        message: String,
    ) {
        match verbosity {
            KittyImageVerbosity::Verbose => {}
            KittyImageVerbosity::OnlyErrors => {
                if success {
                    return;
                }
            }
            KittyImageVerbosity::Quiet => {
                return;
            }
        }

        log::trace!("Query Response: {}", message);

        match (image_id, image_no) {
            (Some(id), Some(no)) => {
                write!(self.writer, "\x1b_GI={},i={};{}\x1b\\", no, id, message).ok();
            }
            (Some(id), None) => {
                write!(self.writer, "\x1b_Gi={};{}\x1b\\", id, message).ok();
            }
            (None, Some(no)) => {
                write!(self.writer, "\x1b_GI={};{}\x1b\\", no, message).ok();
            }
            (None, None) => {
                // The protocol says not to answer a request that identified no
                // image. There is also nothing well-formed to say: the reply
                // syntax is `<keys> ; <message>`, and with no keys this used to
                // emit `ESC _ G OK ESC \` — a reply that this crate's own
                // parser rejects, because every comma-separated token before
                // the `;` has to contain an `=`.
            }
        }
        self.writer.flush().ok();
    }

    fn kitty_frame_compose(
        &mut self,
        frame: KittyImageFrameCompose,
        verbosity: KittyImageVerbosity,
    ) -> anyhow::Result<()> {
        let image_id = match frame.image_number {
            Some(no) => match self.kitty_img.number_to_id.get(&no) {
                Some(id) => *id,
                None => {
                    self.kitty_send_response(
                        verbosity,
                        false,
                        frame.image_id,
                        frame.image_number,
                        "ENOENT".to_string(),
                    );
                    anyhow::bail!("no such image_number {}", no);
                }
            },
            None => frame.image_id.ok_or_else(|| {
                self.kitty_send_response(
                    verbosity,
                    false,
                    frame.image_id,
                    frame.image_number,
                    "ENOENT".to_string(),
                );
                anyhow::anyhow!("no image_id")
            })?,
        };

        let src_frame = frame.source_frame.ok_or_else(|| {
            self.kitty_send_response(
                verbosity,
                false,
                frame.image_id,
                frame.image_number,
                "ENOENT".to_string(),
            );
            anyhow::anyhow!("missing source frame")
        })? as usize;
        let target_frame = frame.target_frame.ok_or_else(|| {
            self.kitty_send_response(
                verbosity,
                false,
                frame.image_id,
                frame.image_number,
                "ENOENT".to_string(),
            );
            anyhow::anyhow!("missing target frame")
        })? as usize;

        let img = Arc::clone(
            self.kitty_img
                .id_to_data
                .get(&image_id)
                .ok_or_else(|| anyhow::anyhow!("invalid image id {}", image_id))?,
        );

        let mut img = img.data();
        match &mut *img {
            ImageDataType::EncodedLease(_) | ImageDataType::EncodedFile(_) => {
                anyhow::bail!("invalid image type")
            }
            ImageDataType::Rgba8 {
                width,
                height,
                data,
                hash,
            } => {
                anyhow::ensure!(
                    src_frame == target_frame && src_frame == 1,
                    "src_frame={} target_frame={} but there is only a single frame",
                    src_frame,
                    target_frame
                );

                let src = clip_view(
                    *width,
                    *height,
                    data.as_mut_slice(),
                    frame.src_x,
                    frame.src_y,
                    frame.w,
                    frame.h,
                )?;

                let mut dest: ImageBuffer<Rgba<u8>, &mut [u8]> =
                    ImageBuffer::from_raw(*width, *height, data.as_mut_slice())
                        .ok_or_else(|| anyhow::anyhow!("ill formed image"))?;

                blit(
                    &mut dest,
                    &src,
                    frame.x.unwrap_or(0),
                    frame.y.unwrap_or(0),
                    frame.composition_mode,
                )?;

                drop(dest);

                *hash = ImageDataType::content_key(data);
            }
            ImageDataType::AnimRgba8 {
                width,
                height,
                frames,
                hashes,
                ..
            } => {
                anyhow::ensure!(
                    src_frame > 0 && src_frame <= frames.len(),
                    "src_frame {} is out of range",
                    src_frame
                );
                anyhow::ensure!(
                    target_frame > 0 && target_frame <= frames.len(),
                    "target_frame {} is out of range",
                    target_frame
                );

                let src = clip_view(
                    *width,
                    *height,
                    frames[src_frame - 1].as_mut_slice(),
                    frame.src_x,
                    frame.src_y,
                    frame.w,
                    frame.h,
                )?;

                let mut dest: ImageBuffer<Rgba<u8>, &mut [u8]> =
                    ImageBuffer::from_raw(*width, *height, frames[target_frame - 1].as_mut_slice())
                        .ok_or_else(|| anyhow::anyhow!("ill formed image"))?;

                blit(
                    &mut dest,
                    &src,
                    frame.x.unwrap_or(0),
                    frame.y.unwrap_or(0),
                    frame.composition_mode,
                )?;

                drop(dest);
                hashes[target_frame - 1] = ImageDataType::content_key(&frames[target_frame - 1]);
            }
        }

        drop(img);
        self.kitty_touch_placements_for_image(image_id);

        Ok(())
    }

    fn kitty_frame_transmit(
        &mut self,
        mut transmit: KittyImageTransmit,
        frame: KittyImageFrame,
        verbosity: KittyImageVerbosity,
    ) -> anyhow::Result<()> {
        if let Some(no) = transmit.image_number.take() {
            match self.kitty_img.number_to_id.get(&no) {
                Some(id) => {
                    transmit.image_id.replace(*id);
                }
                None => {
                    transmit.image_number.replace(no);
                }
            }
        }

        let (image_id, image_number, img) = self.kitty_img_transmit_inner(transmit, verbosity)?;

        let img = match img.decode() {
            ImageDataType::Rgba8 {
                data,
                width,
                height,
                ..
            } => RgbaImage::from_vec(width, height, data)
                .ok_or_else(|| anyhow::anyhow!("data isn't rgba8"))?,
            wat => anyhow::bail!("data isn't rgba8 {:?}", wat),
        };

        let background_pixel = frame.background_pixel.unwrap_or(0);
        let background_pixel = Rgba([
            ((background_pixel >> 24) & 0xff) as u8,
            ((background_pixel >> 16) & 0xff) as u8,
            ((background_pixel >> 8) & 0xff) as u8,
            (background_pixel & 0xff) as u8,
        ]);

        let image = match self.kitty_img.id_to_data.get(&image_id) {
            Some(anim) => Arc::clone(anim),
            None => {
                self.kitty_send_response(
                    verbosity,
                    false,
                    Some(image_id),
                    image_number,
                    "ENOENT".to_string(),
                );
                anyhow::bail!(
                    "no matching image id {} in id_to_data for image_number {:?}",
                    image_id,
                    image_number
                )
            }
        };

        let mut anim = image.data();
        let x = frame.x.unwrap_or(0);
        let y = frame.y.unwrap_or(0);
        let frame_gap = Duration::from_millis(match frame.duration_ms {
            None | Some(0) => 40,
            Some(n) => n.into(),
        });

        match &mut *anim {
            ImageDataType::EncodedLease(_) | ImageDataType::EncodedFile(_) => {
                anyhow::bail!("Expected decoded image for image id {}", image_id)
            }
            ImageDataType::Rgba8 {
                data,
                width,
                height,
                hash,
            } => {
                let base_frame = match frame.base_frame {
                    Some(1) => Some(1),
                    None => None,
                    Some(n) => anyhow::bail!(
                        "attempted to copy frame {} but there is only a single frame",
                        n
                    ),
                };

                match frame.frame_number {
                    Some(1) => {
                        // Edit in place
                        let len = data.len();
                        let mut anim_img: ImageBuffer<Rgba<u8>, &mut [u8]> =
                            ImageBuffer::from_raw(*width, *height, data.as_mut_slice())
                                .ok_or_else(|| {
                                    anyhow::anyhow!(
                                        "ImageBuffer::from_raw failed for single \
                                         frame of {}x{} ({} bytes)",
                                        width,
                                        height,
                                        len
                                    )
                                })?;

                        blit(&mut anim_img, &img, x, y, frame.composition_mode)?;

                        drop(anim_img);
                        *hash = ImageDataType::content_key(data);
                    }
                    Some(2) | None => {
                        // Create a second frame. Deliberately not gated by
                        // the animation caps: this makes exactly two frames
                        // of a dimension-checked image, and all further
                        // growth goes through the append branch and its
                        // gates.

                        let mut new_frame = if base_frame.is_some() {
                            RgbaImage::from_vec(*width, *height, data.clone()).unwrap()
                        } else {
                            RgbaImage::from_pixel(*width, *height, background_pixel)
                        };

                        blit(&mut new_frame, &img, x, y, frame.composition_mode)?;

                        let new_frame_data = new_frame.into_vec();
                        let new_frame_hash = ImageDataType::content_key(&new_frame_data);

                        let frames = vec![std::mem::take(data), new_frame_data];
                        let durations = vec![Duration::from_millis(0), frame_gap];
                        let hashes = vec![*hash, new_frame_hash];

                        *anim = ImageDataType::AnimRgba8 {
                            width: *width,
                            height: *height,
                            frames,
                            durations,
                            hashes,
                        };
                    }
                    Some(n) => anyhow::bail!(
                        "attempted to edit frame {} but there is only a single frame",
                        n
                    ),
                }
            }
            ImageDataType::AnimRgba8 {
                width,
                height,
                frames,
                durations,
                hashes,
            } => {
                let frame_no = frame.frame_number.unwrap_or(frames.len() as u32 + 1);
                if frame_no == frames.len() as u32 + 1 {
                    // Append a new frame

                    // Every frame is exactly width*height*4 bytes, so the
                    // held total is a product, not a walk over the frame
                    // list on each append.
                    let frame_bytes = (*width as usize)
                        .saturating_mul(*height as usize)
                        .saturating_mul(4);
                    let held = frames.len().saturating_mul(frame_bytes);
                    // The animation caps bound one image; the pane budget
                    // bounds the store, and an appended frame grows the
                    // store without passing through the transmit path that
                    // enforces it. One image is never allowed to fill the
                    // whole budget on its own, and the sweep below the
                    // append reclaims older images for what does fit.
                    let budget = self.config.kitty_image_memory_budget();
                    if frames.len() + 1 > MAX_ANIM_FRAMES
                        || held.saturating_add(frame_bytes) > MAX_ANIM_BYTES.min(budget)
                    {
                        // Answered from here because the caller only logs
                        // errors; a refusal the client never hears leaves it
                        // streaming frames into nothing.
                        self.kitty_send_response(
                            verbosity,
                            false,
                            Some(image_id),
                            image_number,
                            "EFBIG:animation too large".to_string(),
                        );
                        anyhow::bail!(
                            "animation for image {} would reach {} bytes across {} \
                             frames, over the {} byte / {} frame limit",
                            image_id,
                            held.saturating_add(frame_bytes),
                            frames.len() + 1,
                            MAX_ANIM_BYTES.min(budget),
                            MAX_ANIM_FRAMES
                        );
                    }

                    let mut new_frame = match frame.base_frame {
                        None => RgbaImage::from_pixel(*width, *height, background_pixel),
                        Some(n) => {
                            let n = n as usize;
                            anyhow::ensure!(
                                n > 0 && n <= frames.len(),
                                "attempted to copy frame {} which is outside range 1-{}",
                                n,
                                frames.len()
                            );
                            RgbaImage::from_vec(*width, *height, frames[n - 1].clone()).unwrap()
                        }
                    };

                    blit(&mut new_frame, &img, x, y, frame.composition_mode)?;

                    let new_frame_data = new_frame.into_vec();
                    let new_frame_hash = ImageDataType::content_key(&new_frame_data);

                    frames.push(new_frame_data);
                    hashes.push(new_frame_hash);
                    durations.push(frame_gap);
                } else {
                    anyhow::ensure!(
                        frame_no > 0 && frame_no <= frames.len() as u32,
                        "attempted to edit frame {} which is outside range 1-{}",
                        frame_no,
                        frames.len()
                    );

                    let frame_no = frame_no as usize;

                    let len = frames[frame_no - 1].len();
                    let mut anim_img: ImageBuffer<Rgba<u8>, &mut [u8]> =
                        ImageBuffer::from_raw(*width, *height, frames[frame_no - 1].as_mut_slice())
                            .ok_or_else(|| {
                                anyhow::anyhow!(
                                    "ImageBuffer::from_raw failed for single \
                                         frame of {}x{} ({} bytes)",
                                    width,
                                    height,
                                    len
                                )
                            })?;

                    blit(&mut anim_img, &img, x, y, frame.composition_mode)?;

                    drop(anim_img);
                    hashes[frame_no - 1] = ImageDataType::content_key(&frames[frame_no - 1]);
                }
            }
        }

        drop(anim);

        // Frames are appended behind the Mutex, so the total measured when
        // the image was transmitted is stale the moment an animation grows.
        self.kitty_img.recompute_used_memory();

        self.kitty_touch_placements_for_image(image_id);
        // A grown animation is a bigger store: apply the same sweep a
        // transmit gets, so appends cannot carry the pane past its budget.
        self.kitty_img.mark_newest(image_id);
        self.kitty_enforce_image_budget();

        Ok(())
    }

    fn kitty_img_transmit_inner(
        &mut self,
        transmit: KittyImageTransmit,
        verbosity: KittyImageVerbosity,
    ) -> anyhow::Result<(u32, Option<u32>, ImageDataType)> {
        log::trace!("transmit {:?}", transmit);
        let (id, no) = match (transmit.image_id, transmit.image_number) {
            (Some(id), Some(no)) => {
                self.kitty_send_response(
                    verbosity,
                    false,
                    Some(id),
                    Some(no),
                    "EINVAL:i= and I= are mutually exclusive".to_string(),
                );
                anyhow::bail!("cannot use both i= and I= in the same request");
            }
            (None, None) => {
                // Assume image id 0
                (0, None)
            }
            (Some(id), None) => (id, None),
            (None, Some(no)) => {
                // Claim the id here rather than leaving it to the caller. Only
                // `kitty_img_transmit` used to advance the counter, so an
                // `a=f` naming an unknown image number handed out an id that
                // the next allocation would hand out again.
                //
                // Checked, because the counter follows the largest client
                // chosen i=: after i=4294967295 a plain `+ 1` panics the
                // parser thread in debug builds, and in release it wraps
                // onto 0 — the anonymous-transmission slot.
                let id = match self.kitty_img.max_image_id.checked_add(1) {
                    Some(id) => id,
                    None => {
                        self.kitty_send_response(
                            verbosity,
                            false,
                            None,
                            Some(no),
                            "ENOSPC:image id space exhausted".to_string(),
                        );
                        anyhow::bail!("image id space exhausted");
                    }
                };
                self.kitty_img.max_image_id = id;
                self.kitty_img.number_to_id.insert(no, id);
                (id, Some(no))
            }
        };

        let data = match transmit.data.load_data() {
            Ok(data) => data,
            Err(err) => {
                // A client that counts one answer per transmission (q=0, or
                // a stream pacing itself on ACKs) would otherwise wait on a
                // reply that never comes; kitty answers a failed read too.
                // The number allocated above must not outlive the image it
                // never got.
                if let Some(no) = no {
                    self.kitty_img.number_to_id.remove(&no);
                }
                self.kitty_send_response(verbosity, false, Some(id), no, format!("EBADF:{err}"));
                return Err(anyhow::Error::new(err)
                    .context("data should have been materialized in coalesce_kitty_accumulation"));
            }
        };

        let data = match transmit.compression {
            KittyImageCompression::None => data,
            KittyImageCompression::Deflate => {
                // The payload picks its own expansion ratio, and deflate
                // reaches roughly 1000:1, so a few megabytes of `o=z` data
                // inflates to gigabytes long before anything downstream gets
                // to reject it for being too large.
                miniz_oxide::inflate::decompress_to_vec_zlib_with_limit(
                    &data,
                    MAX_IMAGE_SIZE as usize,
                )
                // Display, not Debug: the error owns everything decompressed
                // before the limit tripped, and debug-formatting it would turn
                // 100MB of bytes into a several-hundred-megabyte string — the
                // very blowup the limit above exists to prevent.
                .map_err(|e| anyhow::anyhow!("decompressing data: {}", e))?
            }
        };

        let img = match transmit.format {
            None | Some(KittyImageFormat::Rgba) | Some(KittyImageFormat::Rgb) => {
                let (width, height) = match (transmit.width, transmit.height) {
                    (Some(w), Some(h)) => (w, h),
                    _ => {
                        anyhow::bail!("missing width/height info for kitty img");
                    }
                };

                check_image_dimensions(width, height)?;

                let data = match transmit.format {
                    Some(KittyImageFormat::Rgb) => {
                        let img = DynamicImage::ImageRgb8(
                            RgbImage::from_vec(width, height, data)
                                .ok_or_else(|| anyhow::anyhow!("failed to decode image"))?,
                        );
                        let img = img.into_rgba8();
                        img.into_vec()
                    }
                    _ => data,
                };

                anyhow::ensure!(
                    width * height * 4 == data.len() as u32,
                    "transmit data len is {} but it doesn't match width*height*4 {}x{}x4 = {}",
                    data.len(),
                    width,
                    height,
                    width * height * 4
                );

                // The buffer keeps whatever capacity reading it left behind,
                // and this image may sit in the store for a long time; the
                // memory accounting counts len(), so make the two agree.
                let mut data = data;
                data.shrink_to_fit();
                ImageDataType::new_single_frame(width, height, data)
            }
            Some(KittyImageFormat::Png) => {
                let info = dimensions(&data)?;
                check_image_dimensions(info.width, info.height)?;
                let decoded = image::load_from_memory(&data).context("decode png")?;
                let (width, height) = decoded.dimensions();
                let data = decoded.into_rgba8().into_vec();
                ImageDataType::new_single_frame(width, height, data)
            }
        };

        Ok((id, no, img))
    }

    /// Stores the transmitted image without answering. The caller decides
    /// when success can honestly be claimed — for transmit-and-display that
    /// is only after the display half worked.
    fn kitty_img_transmit_quietly(
        &mut self,
        transmit: KittyImageTransmit,
        verbosity: KittyImageVerbosity,
    ) -> anyhow::Result<(u32, Option<u32>)> {
        let (image_id, image_number, img) = self.kitty_img_transmit_inner(transmit, verbosity)?;
        self.kitty_img.max_image_id = self.kitty_img.max_image_id.max(image_id);

        let img = self
            .raw_image_to_image_data(img)
            .context("storing image data")?;
        self.kitty_img
            .record_id_to_data(image_id, img, self.config.kitty_image_memory_budget());
        self.kitty_enforce_image_budget();

        Ok((image_id, image_number))
    }

    fn kitty_img_transmit(
        &mut self,
        transmit: KittyImageTransmit,
        verbosity: KittyImageVerbosity,
    ) -> anyhow::Result<u32> {
        let (image_id, image_number) = self.kitty_img_transmit_quietly(transmit, verbosity)?;

        // Acknowledge whenever the request identified the image, by id or by
        // number. Answering only for `I=` left the far more common `i=` form
        // with no reply at all, success or failure.
        if image_id != 0 || image_number.is_some() {
            self.kitty_send_response(
                verbosity,
                true,
                Some(image_id),
                image_number,
                "OK".to_string(),
            );
        }

        Ok(image_id)
    }

    fn coalesce_kitty_accumulation(&mut self, img: KittyImage) -> anyhow::Result<KittyImage> {
        if self.kitty_img.accumulator.is_empty() {
            Ok(img)
        } else {
            let mut data = vec![];
            let mut trans;
            let kind;

            self.kitty_img.accumulator.push(img);

            let mut empty_data = KittyImageData::Direct(String::new());
            // The opening fragment decides what this transfer is and who it is
            // for. Continuation fragments carry only `m=` and payload, so
            // reading the command, the image id or `q=` off the closing
            // fragment would pick up defaults the client never sent.
            let opening = self.kitty_img.accumulator.remove(0);
            let verbosity = opening.verbosity();
            match opening {
                KittyImage::TransmitData { transmit, .. } => {
                    kind = ChunkedTransfer::Transmit;
                    trans = transmit;
                    std::mem::swap(&mut empty_data, &mut trans.data);
                }
                KittyImage::TransmitDataAndDisplay {
                    transmit,
                    placement,
                    ..
                } => {
                    kind = ChunkedTransfer::Display(placement);
                    trans = transmit;
                    std::mem::swap(&mut empty_data, &mut trans.data);
                }
                KittyImage::TransmitFrame {
                    transmit, frame, ..
                } => {
                    kind = ChunkedTransfer::Frame(frame);
                    trans = transmit;
                    std::mem::swap(&mut empty_data, &mut trans.data);
                }
                other => anyhow::bail!("{:?} carries no payload to accumulate", other),
            }
            data.push(empty_data);

            self.kitty_img.accumulated_bytes = 0;

            for item in self.kitty_img.accumulator.drain(..) {
                match item {
                    KittyImage::TransmitData { transmit, .. }
                    | KittyImage::TransmitDataAndDisplay { transmit, .. }
                    | KittyImage::TransmitFrame { transmit, .. } => {
                        data.push(transmit.data);
                    }
                    // A stream that switches to a payload-free command midway
                    // through a transfer reaches this, so refuse the transfer
                    // rather than panicking on it.
                    other => {
                        anyhow::bail!("{:?} carries no payload to accumulate", other)
                    }
                }
            }

            let mut b64_decoded = vec![];
            for mut data in data.into_iter() {
                match &mut data {
                    KittyImageData::DirectBin(b) => {
                        b64_decoded.append(b);
                    }
                    KittyImageData::Direct(b) => {
                        if !b.is_empty() {
                            b64_decoded.append(&mut data.load_data()?);
                        }
                    }
                    data => {
                        anyhow::bail!("expected data chunks to be Direct data, found {:#?}", data)
                    }
                }
            }

            trans.data = KittyImageData::DirectBin(b64_decoded);

            Ok(match kind {
                ChunkedTransfer::Transmit => KittyImage::TransmitData {
                    transmit: trans,
                    verbosity,
                },
                ChunkedTransfer::Display(placement) => KittyImage::TransmitDataAndDisplay {
                    transmit: trans,
                    placement,
                    verbosity,
                },
                ChunkedTransfer::Frame(frame) => KittyImage::TransmitFrame {
                    transmit: trans,
                    frame,
                    verbosity,
                },
            })
        }
    }
}

/// Make a copy of the source region.
/// Ideally we wouldn't need this, but Rust's mutability rules
/// make it very awkward to mutably reference a frame while
/// an immutable reference exists to a separate frame.
fn clip_view(
    width: u32,
    height: u32,
    data: &mut [u8],
    src_x: Option<u32>,
    src_y: Option<u32>,
    view_width: Option<u32>,
    view_height: Option<u32>,
) -> anyhow::Result<RgbaImage> {
    let src = ImageBuffer::from_raw(width, height, data)
        .ok_or_else(|| anyhow::anyhow!("ill formed image"))?;

    let src_x = src_x.unwrap_or(0);
    let src_y = src_y.unwrap_or(0);

    let view_width = view_width.unwrap_or(width);
    let view_height = view_height.unwrap_or(height);

    let (view_width, view_height) =
        image::imageops::overlay_bounds((width, height), (view_width, view_height), src_x, src_y);

    let view = src.view(src_x, src_y, view_width, view_height);

    let mut tmp = RgbaImage::new(view_width, view_height);
    tmp.copy_from(&*view, 0, 0).context("copy source image")?;
    Ok(tmp)
}

fn blit<D, S, P>(
    dest: &mut D,
    src: &S,
    x: u32,
    y: u32,
    mode: KittyFrameCompositionMode,
) -> anyhow::Result<()>
where
    D: GenericImage<Pixel = P>,
    S: GenericImageView<Pixel = P>,
{
    match mode {
        KittyFrameCompositionMode::Overwrite => {
            ::image::imageops::replace(dest, src, x.into(), y.into());
        }
        KittyFrameCompositionMode::AlphaBlending => {
            ::image::imageops::overlay(dest, src, x.into(), y.into());
        }
    }
    Ok(())
}
