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
    placements: HashMap<(u32, Option<u32>), PlacementInfo>,
    used_memory: usize,
}

impl KittyImageState {
    fn remove_data_for_id(&mut self, image_id: u32) {
        if let Some(data) = self.id_to_data.remove(&image_id) {
            self.used_memory = self.used_memory.saturating_sub(data.len());
        }
    }

    fn record_id_to_data(&mut self, image_id: u32, data: Arc<ImageData>) {
        // Unconditionally, id 0 included. The insert below replaces whatever
        // was at this key either way, so skipping the bookkeeping for the
        // anonymous-transmission key does not keep that image alive — it only
        // loses track of its bytes. Left uncounted, `used_memory` climbs
        // forever, and once it passes the budget `prune_unreferenced` starts
        // evicting every unplaced image on every transfer, which breaks
        // transmit-now-place-later.
        self.remove_data_for_id(image_id);
        self.prune_unreferenced();
        self.used_memory += data.len();
        self.id_to_data.insert(image_id, data);
    }

    #[cfg(test)]
    pub(crate) fn used_memory(&self) -> usize {
        self.used_memory
    }

    pub(crate) fn prune_unreferenced(&mut self) {
        let budget = 320 * 1024 * 1024; // FIXME: make this configurable
        if self.used_memory > budget {
            let referenced: HashSet<u32> = self.placements.keys().map(|(k, _)| *k).collect();
            let target = self.used_memory - budget;
            let mut freed = 0;
            self.id_to_data.retain(|id, data| {
                if referenced.contains(id) || freed > target {
                    true
                } else {
                    freed += data.len();
                    false
                }
            });

            log::info!(
                "using {} RAM for images, pruned {}",
                self.used_memory,
                freed
            );
            self.used_memory = self.used_memory.saturating_sub(freed);
        }
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

impl TerminalState {
    #[cfg(test)]
    pub(crate) fn kitty_used_memory(&self) -> usize {
        self.kitty_img.used_memory()
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
        let placements: Vec<(StableRowIndex, usize)> = self
            .kitty_img
            .placements
            .iter()
            .filter(|((id, _), _)| *id == image_id)
            .map(|(_, info)| (info.first_row, info.rows))
            .collect();

        let seqno = self.seqno;
        let screen = self.screen_mut();
        for (first_row, rows) in placements {
            let range = screen.stable_range(&(first_row..first_row + rows as StableRowIndex));
            for idx in range {
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
        let screen = self.screen_mut();
        let range =
            screen.stable_range(&(info.first_row..info.first_row + info.rows as StableRowIndex));
        for idx in range {
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

        // Measured before and after, because frames are appended behind the
        // Mutex: the size reported to the memory budget when the image was
        // transmitted goes stale the moment an animation grows.
        let bytes_before = image.len();

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
                    if frames.len() + 1 > MAX_ANIM_FRAMES
                        || held.saturating_add(frame_bytes) > MAX_ANIM_BYTES
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
                            MAX_ANIM_BYTES,
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

        let bytes_after = image.len();
        self.kitty_img.used_memory = self
            .kitty_img
            .used_memory
            .saturating_add(bytes_after)
            .saturating_sub(bytes_before);

        self.kitty_touch_placements_for_image(image_id);

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

        let data = transmit
            .data
            .load_data()
            .context("data should have been materialized in coalesce_kitty_accumulation")?;

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
        self.kitty_img.record_id_to_data(image_id, img);

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
