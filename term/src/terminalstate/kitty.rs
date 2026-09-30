use crate::kitty_animation::{monotonic_ms, KittyAnimation, KittyPlaybackSnapshot};
use crate::kitty_relative::{PlacementKey, RelativePlacements, RelativeView};
#[path = "kitty_scene.rs"]
mod scene;
use crate::terminalstate::image::*;
use crate::terminalstate::{ImageAttachParams, PlacementInfo};
use crate::{StableRowIndex, TerminalState};
use ::image::{
    DynamicImage, GenericImage, GenericImageView, ImageBuffer, RgbImage, Rgba, RgbaImage,
};
use anyhow::Context;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
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
pub(crate) const MAX_PLACEMENTS: usize = 65536;

/// An animation grows one frame at a time with no natural end. Without a
/// ceiling, a client that never deletes frames can append until the process dies. Sized for real content: a terminal-sized
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
    /// Set when a transfer was abandoned after exceeding the limits or
    /// becoming idle. Discards its tail until `m=0` or a fresh keyed opening;
    /// without it the remaining fragments would be parsed without their header.
    accumulator_overflowed: bool,
    max_image_id: u32,
    /// Numbered ids increase with successful transmissions. Keep older ids
    /// until eviction so deleting the newest reveals the previous image.
    numbered_images: BTreeSet<(u32, u32)>,
    id_to_data: HashMap<u32, Arc<ImageData>>,
    animations: HashMap<u32, KittyAnimation>,
    virtual_placements: crate::kitty_virtual::VirtualPlacements,
    selection_revision: u64,
    /// Transmission order of each stored image, so eviction drops the
    /// oldest unplaced image first. A HashMap walk would evict at random,
    /// which for a frame stream means keeping stale frames over fresh ones.
    id_seq: HashMap<u32, u64>,
    next_seq: u64,
    pub(super) placements: BTreeMap<PlacementKey, ImagePlacement>,
    relatives: RelativePlacements,
    relative_views: BTreeMap<u32, Vec<RelativeView>>,
    relative_seqno: Option<wezterm_surface::SequenceNo>,
    pub(super) next_placement_id: u64,
    used_memory: usize,
    /// The idle sweep's memory of `next_seq` and of the transfer in
    /// progress, and how many sweeps in a row found both unchanged. Ticks,
    /// not a clock: this crate also builds for the browser, where there is
    /// no monotonic time to read.
    idle_seen_seq: u64,
    idle_seen_transfer: (usize, usize),
    idle_ticks: u32,
}

impl TerminalState {
    /// The picture stored under a kitty image id, for tests.
    #[cfg(test)]
    pub(crate) fn kitty_image_data_for_id(&self, image_id: u32) -> Option<Arc<ImageData>> {
        self.kitty_img.image_data_for_id(image_id)
    }
}

impl KittyImageState {
    fn id_for_number(&self, number: u32) -> Option<u32> {
        self.numbered_images.range((number, 0)..=(number, u32::MAX))
            .next_back().map(|(_, id)| *id)
    }

    /// The bookkeeping, with each picture replaced by its hash in `images`.
    #[cfg(feature = "use_serde")]
    pub(crate) fn snapshot(
        &self,
        images: &mut crate::terminalstate::snapshot::ImageTable,
    ) -> crate::terminalstate::snapshot::KittySnapshot {
        crate::terminalstate::snapshot::KittySnapshot {
            max_image_id: self.max_image_id,
            // The legacy snapshot carries only the newest id for each number.
            number_to_id: self.numbered_images.iter().copied().collect(),
            id_to_hash: self
                .id_to_data
                .iter()
                .map(|(id, data)| (*id, images.remember(data)))
                .collect(),
            id_seq: self.id_seq.iter().map(|(k, v)| (*k, *v)).collect(),
            next_seq: self.next_seq,
            placements: self.placements.iter().map(|(k, v)| ((k.image_id, k.protocol_id()), v.info)).collect(),
            transmission_in_progress: !self.accumulator.is_empty(),
        }
    }

    /// The bookkeeping from a snapshot, pictures looked up in `images` by
    /// hash. A transfer that was in flight is not resumed: the accumulator
    /// starts empty, and the program hears an error for its next fragment.
    #[cfg(feature = "use_serde")]
    pub(crate) fn restore(
        &mut self,
        snapshot: crate::terminalstate::snapshot::KittySnapshot,
        images: &std::collections::HashMap<[u8; 32], Arc<ImageData>>,
    ) -> anyhow::Result<()> {
        let mut id_to_data = HashMap::new();
        for (id, hash) in snapshot.id_to_hash {
            let data = images.get(&hash).ok_or_else(|| {
                anyhow::anyhow!("kitty image {id} refers to a picture the snapshot does not carry")
            })?;
            id_to_data.insert(id, Arc::clone(data));
        }
        self.accumulator.clear();
        self.accumulated_bytes = 0;
        self.accumulator_overflowed = false;
        if !self.animations.is_empty() {
            self.animations = HashMap::new();
        }
        self.selection_revision += 1;
        self.virtual_placements = Default::default();
        self.relatives = Default::default();
        self.relative_views = Default::default();
        self.relative_seqno = None;
        self.max_image_id = snapshot.max_image_id;
        self.numbered_images = snapshot.number_to_id.into_iter()
            .filter(|(_, id)| id_to_data.contains_key(id)).collect();
        self.id_to_data = id_to_data;
        self.id_seq = snapshot.id_seq.into_iter().collect();
        self.next_seq = snapshot.next_seq;
        self.placements = snapshot.placements.into_iter().map(|((image_id, placement_id), info)| {
            (PlacementKey { image_id, placement_id: u64::from(placement_id.unwrap_or(0)) }, ImagePlacement { info, origin: None })
        }).collect();
        self.next_placement_id = u64::from(u32::MAX);
        self.recompute_used_memory();
        Ok(())
    }

    /// The picture stored under a kitty image id, for tests that check a
    /// restore shares one `Arc` between the cells and this map.
    #[cfg(test)]
    pub(crate) fn image_data_for_id(&self, image_id: u32) -> Option<Arc<ImageData>> {
        self.id_to_data.get(&image_id).cloned()
    }

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
        if self.virtual_placements.remove(image_id, None) {
            self.selection_revision += 1;
        }
        if self.animations.remove(&image_id).is_some() {
            if self.animations.is_empty() {
                self.animations = HashMap::new();
            }
        }
        if self.id_to_data.remove(&image_id).is_some() {
            self.selection_revision += 1;
        }
        self.id_seq.remove(&image_id);
        self.recompute_used_memory();
    }

    /// Eviction, as opposed to replacement: an image number that named the
    /// evicted id must not keep resolving to it, or every later `a=p,I=n`
    /// fails on an id that no longer exists.
    fn evict(&mut self, image_id: u32) {
        self.remove_data_for_id(image_id);
        self.numbered_images.retain(|(_, id)| *id != image_id);
    }

    fn evict_unplaced(&mut self, ids: &HashSet<u32>) {
        if ids.is_empty() {
            return;
        }
        let before = self.id_to_data.len();
        self.id_to_data.retain(|id, _| !ids.contains(id));
        self.id_seq.retain(|id, _| !ids.contains(id));
        self.animations.retain(|id, _| !ids.contains(id));
        self.numbered_images.retain(|(_, id)| !ids.contains(id));
        if self.id_to_data.len() != before {
            self.selection_revision += 1;
            self.recompute_used_memory();
        }
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
        self.selection_revision += 1;
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

    /// Release fragments of an abandoned transfer after enough quiet sweeps.
    /// Completed images remain reusable until explicit deletion or byte-budget
    /// eviction, regardless of whether they currently have placements.
    /// New image data or another fragment resets the quiet count.
    pub(crate) fn idle_tick(&mut self, ticks_until_release: u32) -> usize {
        let transfer = (self.accumulator.len(), self.accumulated_bytes);
        if self.next_seq != self.idle_seen_seq || transfer != self.idle_seen_transfer {
            self.idle_seen_seq = self.next_seq;
            self.idle_seen_transfer = transfer;
            self.idle_ticks = 0;
            return 0;
        }
        self.idle_ticks = self.idle_ticks.saturating_add(1);
        if self.idle_ticks < ticks_until_release {
            return 0;
        }
        // A transfer that never finished (its producer was killed between
        // chunks) would otherwise hold its fragments until the pane closes.
        // Discard any late tail just as for an over-limit transfer. Preserve
        // an existing latch, but don't arm one when no transfer was pending.
        let released = self.accumulated_bytes;
        self.accumulator_overflowed |= !self.accumulator.is_empty();
        self.accumulator = Vec::new();
        self.accumulated_bytes = 0;
        self.idle_seen_transfer = (0, 0);
        released
    }

    /// Evict unplaced images, oldest use first, until the stored bytes fit
    /// `budget`. Placed images and the most recently used image are spared.
    pub(crate) fn prune_unreferenced(&mut self, budget: usize) {
        if self.used_memory <= budget {
            return;
        }
        let referenced: HashSet<u32> = self.placements.keys().map(|key| key.image_id)
            .chain(self.relatives.iter().map(|p| p.key.image_id)).collect();
        let newest = self.next_seq;
        let before = self.used_memory;
        let mut candidates: Vec<(u64, u32)> = self
            .id_to_data
            .keys()
            .filter(|id| !referenced.contains(id) && !self.virtual_placements.contains_image(**id)
                && self.id_seq.get(id).copied() != Some(newest))
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
    fn placement_limit_preserves_existing_cells_and_recovers_after_deletion() {
        use super::{ImagePlacement, PlacementInfo, PlacementKey, MAX_PLACEMENTS};
        use crate::{Terminal, TerminalConfiguration, TerminalSize};
        use wezterm_cell::image::{ImageCell, TextureCoordinate};
        #[derive(Debug)]
        struct Config;
        impl TerminalConfiguration for Config {
            fn scrollback_size(&self) -> usize { 0 }
            fn color_palette(&self) -> crate::color::ColorPalette { Default::default() }
            fn enable_kitty_graphics(&self) -> bool { true }
        }
        let mut terminal = Terminal::new(TerminalSize { rows: 2, cols: 2,
            pixel_width: 16, pixel_height: 32, dpi: 96 }, Arc::new(Config), "ThinkTerm", "test", Box::new(Vec::new()));
        let data = image(1);
        terminal.kitty_img.record_id_to_data(1, data.clone(), 1024);
        let mut cell = wezterm_cell::Cell::blank();
        for index in 0..MAX_PLACEMENTS {
            let tag = u64::from(u32::MAX) + 1 + index as u64;
            terminal.kitty_img.placements.insert(PlacementKey { image_id: 1, placement_id: tag },
                ImagePlacement { info: PlacementInfo { first_row: 0, rows: 1, cols: 1, alt_screen: false }, origin: None });
            cell.attrs_mut().attach_image(Box::new(
                ImageCell::with_z_index(TextureCoordinate::new_f32(0.0, 0.0), TextureCoordinate::new_f32(1.0, 1.0),
                    data.clone(), index as i32, 0, 0, 0, 0, Some(1), None).with_placement_tag(tag)));
            terminal.kitty_img.next_placement_id = tag;
        }
        terminal.screen_mut().set_cell(0, 0, &cell, 0);
        drop(cell);
        let counter = terminal.kitty_img.next_placement_id;
        terminal.advance_bytes("\x1b_Ga=p,i=1,C=1,q=2\x1b\\");
        assert_eq!(terminal.kitty_img.placements.len(), MAX_PLACEMENTS);
        assert_eq!(terminal.kitty_img.next_placement_id, counter);
        assert_eq!(terminal.screen_mut().line_mut(0).get_cell(0).unwrap().attrs().image_attachments().count(), MAX_PLACEMENTS);
        terminal.advance_bytes("\x1b_Ga=d,d=z,z=1,q=2\x1b\\");
        assert_eq!(terminal.kitty_img.placements.len(), MAX_PLACEMENTS - 1);
        terminal.advance_bytes("\x1b_Ga=p,i=1,C=1,q=2\x1b\\");
        assert_eq!(terminal.kitty_img.next_placement_id, counter + 1);
        assert_eq!(terminal.kitty_img.placements.len(), MAX_PLACEMENTS);
        terminal.advance_bytes("\x1b_Ga=d,d=I,i=1,q=2\x1b\\");
        assert_eq!(terminal.kitty_image_stats(), (0, 0, 0));
        assert!(!terminal.screen_mut().line_mut(0).has_images());
    }

    #[test]
    fn idle_ticks_preserve_completed_images_until_budget_pressure() {
        let mut state = KittyImageState::default();
        let one = image(16).len();
        for id in 1..=3 {
            state.record_id_to_data(id, image(16), 10 * one);
        }
        // The first tick only notices the transfers; the next ones count.
        assert_eq!(state.idle_tick(2), 0);
        assert_eq!(state.idle_tick(2), 0);
        // A transfer in between starts the count over.
        state.record_id_to_data(4, image(16), 10 * one);
        assert_eq!(state.idle_tick(2), 0);
        assert_eq!(state.idle_tick(2), 0);
        assert_eq!(state.id_to_data.len(), 4, "still held");
        for _ in 0..10 {
            assert_eq!(state.idle_tick(2), 0);
        }
        assert_eq!(state.id_to_data.len(), 4, "idle is not a deletion request");
        state.prune_unreferenced(one);
        assert_eq!(
            state.id_to_data.keys().copied().collect::<Vec<_>>(),
            vec![4],
            "the byte budget still evicts older images"
        );
        assert_eq!(state.used_memory, one);
        assert_eq!(state.idle_tick(2), 0, "nothing left to release");
    }

    #[test]
    fn idle_sweeps_preserve_an_existing_discard_latch() {
        let mut state = KittyImageState::default();
        for _ in 0..3 {
            assert_eq!(state.idle_tick(2), 0);
            assert!(!state.accumulator_overflowed);
        }
        // A rejected transfer has already released its fragments but must
        // keep discarding the tail, no matter how many idle sweeps follow.
        state.accumulator_overflowed = true;
        for _ in 0..3 {
            assert_eq!(state.idle_tick(2), 0);
            assert!(state.accumulator_overflowed);
        }
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
fn placement_error(err: &anyhow::Error) -> String {
    let message = err.to_string();
    if ["EINVAL:", "ENOENT:", "ENOPARENT:", "ECYCLE:", "ETOODEEP:", "ENOSPC:"].iter().any(|code| message.starts_with(code)) {
        message
    } else {
        format!("ERROR:{message}")
    }
}

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
    /// See `KittyImageState::idle_tick`; driven by the mux's periodic sweep.
    pub fn idle_image_tick(&mut self, ticks_until_release: u32) -> usize {
        self.kitty_img.idle_tick(ticks_until_release)
    }

    /// Stored kitty images, their placements, and the bytes they hold: for
    /// the sweep's log line, so a pane that keeps pictures says why.
    pub fn kitty_image_stats(&self) -> (usize, usize, usize) {
        (
            self.kitty_img.id_to_data.len(),
            self.kitty_img.placements.len(),
            self.kitty_img.used_memory,
        )
    }

    /// Only subscribers to frame control read this; desktop pixels and frame
    /// durations remain unchanged. Static images also publish pixel versions.
    pub fn kitty_frame_selections(&mut self, known: Option<u64>) -> Option<(u64, Vec<crate::KittyFrameSelection>)> {
        self.kitty_scene_refresh();
        if known == Some(self.kitty_img.selection_revision) {
            return None;
        }
        Some((self.kitty_img.selection_revision, self.kitty_img.id_to_data.iter().map(|(id, data)| {
            crate::KittyFrameSelection {
                image_id: *id, data_hash: data.hash(), data_generation: data.generation(),
                animation: self.kitty_img.animations.get(id).cloned().unwrap_or_else(|| KittyAnimation::new([0], 0)),
                virtual_placements: self.kitty_img.virtual_placements.for_image(*id),
                relative_placements: self.kitty_img.relative_views.get(id).cloned().unwrap_or_default(),
            }
        }).collect()))
    }

    /// The picture `image_id` holds now, whichever version a caller last saw.
    pub fn kitty_image(&self, image_id: u32) -> Option<Arc<ImageData>> {
        self.kitty_img.id_to_data.get(&image_id).cloned()
    }

    pub fn snapshot_kitty_playback(&self) -> KittyPlaybackSnapshot {
        KittyPlaybackSnapshot {
            captured_at_ms: monotonic_ms(),
            selections: self.kitty_img.animations.iter().filter_map(|(id, animation)| {
                self.kitty_img.id_to_data.get(id).map(|data| crate::kitty_animation::KittyPlaybackEntry {
                    image_id: *id, data_hash: data.hash(), animation: animation.clone(),
                })
            }).collect(),
        }
    }

    pub fn snapshot_kitty_graphics(&self) -> crate::KittyGraphicsSnapshot {
        crate::KittyGraphicsSnapshot {
            playback: self.snapshot_kitty_playback(),
            virtual_images: self.snapshot_kitty_virtual(),
            image_numbers: self.kitty_img.numbered_images.iter().copied().collect(),
            placements: self.snapshot_kitty_placements(),
            relatives: self.snapshot_kitty_relatives(),
        }
    }

    fn snapshot_kitty_placements(&self) -> Option<KittyPlacementSnapshot> {
        if self.kitty_img.next_placement_id <= u64::from(u32::MAX) {
            return None;
        }
        let mut live = HashSet::new();
        let cell_tags = [false, true].map(|alternate| {
            let mut tags = Vec::new();
            self.screen_for_alt(alternate).for_each_phys_line(|_, line| {
                if !line.has_images() { return; }
                for cell in (0..line.len()).filter_map(|idx| line.get_cell(idx)) {
                    for image in cell.attrs().image_attachments() {
                        let tag = image.placement_tag();
                        tags.push(tag);
                        if let Some(image_id) = image.image_id() {
                            live.insert(PlacementKey { image_id, placement_id: tag });
                        }
                    }
                }
            });
            tags
        });
        Some(KittyPlacementSnapshot {
            next_id: self.kitty_img.next_placement_id,
            placements: self.kitty_img.placements.iter()
                .filter(|(key, _)| live.contains(key)).map(|(key, placement)| (*key, placement.info)).collect(),
            cell_tags,
        })
    }

    pub fn restore_kitty_numbers(&mut self, numbers: Vec<(u32, u32)>) -> anyhow::Result<()> {
        anyhow::ensure!(numbers.len() <= self.kitty_img.id_to_data.len(), "too many numbered images");
        let mut ids = HashSet::with_capacity(numbers.len());
        let mut numbered_images = BTreeSet::new();
        for (number, id) in numbers {
            anyhow::ensure!(self.kitty_img.id_to_data.contains_key(&id), "number refers to an absent image");
            anyhow::ensure!(id != 0 && ids.insert(id), "invalid or duplicate numbered image id");
            let latest = self.kitty_img.id_for_number(number)
                .ok_or_else(|| anyhow::anyhow!("number missing from the terminal snapshot"))?;
            anyhow::ensure!(id <= latest, "number history is newer than the terminal snapshot");
            numbered_images.insert((number, id));
        }
        anyhow::ensure!(self.kitty_img.numbered_images.is_subset(&numbered_images), "incomplete number history");
        self.kitty_img.numbered_images = numbered_images;
        Ok(())
    }

    pub fn snapshot_kitty_virtual(&self) -> Vec<crate::kitty_virtual::VirtualImage> {
        let mut images: Vec<_> = self.kitty_img.id_to_data.iter().filter_map(|(id, data)| {
            let placements = self.kitty_img.virtual_placements.for_image(*id);
            (!placements.is_empty()).then(|| crate::kitty_virtual::VirtualImage {
                image_id: *id, data_hash: data.hash(), placements,
            })
        }).collect();
        images.sort_unstable_by_key(|image| image.image_id);
        images
    }

    pub fn restore_kitty_virtual(&mut self, images: Vec<crate::kitty_virtual::VirtualImage>) -> anyhow::Result<()> {
        use crate::kitty_virtual::{valid, VirtualPlacements, MAX_VIRTUAL_PLACEMENTS};
        anyhow::ensure!(images.len() <= MAX_VIRTUAL_PLACEMENTS, "too many virtual images");
        let count = images.iter().try_fold(0usize, |count, image| count.checked_add(image.placements.len()));
        anyhow::ensure!(count.is_some_and(|count| count <= MAX_VIRTUAL_PLACEMENTS), "too many virtual placements");
        let mut ids = HashSet::with_capacity(images.len());
        let mut placements = VirtualPlacements::default();
        for image in images {
            let data = self.kitty_img.id_to_data.get(&image.image_id)
                .ok_or_else(|| anyhow::anyhow!("virtual placement refers to an absent image"))?;
            anyhow::ensure!(data.hash() == image.data_hash, "virtual placement does not match its image");
            anyhow::ensure!(!image.placements.is_empty() && valid(&image.placements), "invalid virtual placements");
            anyhow::ensure!(ids.insert(image.image_id), "duplicate virtual image id");
            for placement in image.placements {
                placements.insert(image.image_id, placement);
            }
        }
        self.kitty_img.virtual_placements = placements;
        self.kitty_img.selection_revision += 1;
        Ok(())
    }

    pub fn restore_kitty_playback(&mut self, snapshot: KittyPlaybackSnapshot) -> anyhow::Result<()> {
        let now = monotonic_ms();
        let mut animations = HashMap::new();
        for selection in snapshot.selections {
            let data = self.kitty_img.id_to_data.get(&selection.image_id)
                .ok_or_else(|| anyhow::anyhow!("animation refers to an absent image"))?;
            let count = match &*data.data() {
                ImageDataType::AnimRgba8 { frames, .. } => frames.len(),
                _ => 1,
            };
            let mut animation = selection.animation;
            anyhow::ensure!(data.hash() == selection.data_hash && animation.valid()
                && animation.frame_ends.len() == count, "animation does not match its image");
            animation.rebase(snapshot.captured_at_ms, now);
            anyhow::ensure!(animations.insert(selection.image_id, animation).is_none(), "duplicate animation image id");
        }
        self.kitty_img.animations = animations;
        self.kitty_img.selection_revision += 1;
        Ok(())
    }

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
        self.kitty_scene_refresh();
        let image_id = match image_id {
            Some(id) => id,
            None => self.kitty_img.id_for_number(
                image_number.ok_or_else(|| anyhow::anyhow!("no image_id or image_number specified!"))?,
            ).ok_or_else(|| anyhow::anyhow!("no image for image_number {:?}", image_number))?,
        };

        log::trace!(
            "kitty_img_place image_id {:?} image_no {:?} placement {:?} verb {:?}",
            image_id,
            image_number,
            placement,
            verbosity
        );

        let placement_id = placement.placement_id.filter(|id| image_id != 0 && *id != 0);
        let img = Arc::clone(self.kitty_img.id_to_data.get(&image_id).ok_or_else(|| {
            anyhow::anyhow!("ENOENT: no matching image id {image_id}")
        })?);

        anyhow::ensure!(!placement.virtual_placement || placement.parent_image_id.unwrap_or(0) == 0,
            "EINVAL: a virtual placement cannot have a parent");
        if placement.parent_image_id.unwrap_or(0) != 0 {
            let tag = match placement_id {
                Some(id) => u64::from(id),
                None => self.kitty_img.next_placement_id.max(u64::from(u32::MAX)).checked_add(1)
                    .ok_or_else(|| anyhow::anyhow!("ENOSPC: placement ids exhausted"))?,
            };
            self.kitty_place_relative(PlacementKey { image_id, placement_id: tag }, &placement, img.data().dimensions()?)?;
            if placement_id.is_none() { self.kitty_img.next_placement_id = tag; }
            self.kitty_img.mark_newest(image_id);
            return Ok(());
        }
        if placement.virtual_placement {
            // Publish the grid to Web/mobile without changing the legacy
            // cursor, cells, response or image eviction order.
            if self.kitty_img.id_to_data.contains_key(&image_id)
                && self.kitty_img.virtual_placements.insert(image_id, crate::kitty_virtual::VirtualPlacement {
                    placement_id: placement_id.unwrap_or(0),
                    columns: placement.columns.unwrap_or(0),
                    rows: placement.rows.unwrap_or(0),
                })
            {
                if let Some(placement_id) = placement_id {
                    self.kitty_replace_placement(image_id, placement_id);
                }
                self.kitty_img.selection_revision += 1;
            }
            return Ok(());
        }
        let (image_width, image_height) = img.data().dimensions()?;
        let tag = match placement_id {
            Some(id) => u64::from(id),
            None => self.kitty_img.next_placement_id.max(u64::from(u32::MAX))
                .checked_add(1).ok_or_else(|| anyhow::anyhow!("ENOSPC: placement ids exhausted"))?,
        };
        let key = PlacementKey { image_id, placement_id: tag };
        if self.kitty_img.placements.len() >= MAX_PLACEMENTS && !self.kitty_img.placements.contains_key(&key) {
            self.kitty_prune_placements();
            anyhow::ensure!(self.kitty_img.placements.len() < MAX_PLACEMENTS, "ENOSPC: too many image placements");
        }
        // A placement spanning more cells than the grid has is clamped to
        // the grid: cells are assigned one column at a time below, and a
        // count near u32::MAX would allocate them until the process died,
        // holding the terminal lock the whole way.
        let grid_cols = self.screen().physical_cols.max(1);
        let grid_rows = self.screen().physical_rows.max(1);
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
            columns: placement.columns.map(|x| (x as usize).min(grid_cols)),
            rows: placement.rows.map(|x| (x as usize).min(grid_rows)),
            image_id: Some(image_id),
            placement_id,
            placement_tag: if placement_id.is_none() { tag } else { 0 },
            do_not_move_cursor: placement.do_not_move_cursor,
        })?;

        if let Some(placement_id) = placement_id {
            if self.kitty_img.virtual_placements.remove(image_id, Some(placement_id)) {
                self.kitty_img.selection_revision += 1;
            }
        }
        self.kitty_img.mark_newest(image_id);
        if placement_id.is_none() { self.kitty_img.next_placement_id = tag; }
        self.kitty_img.placements.insert(key, info);
        self.kitty_img.relative_seqno = None;
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
                let placement_id = placement.placement_id.filter(|p| *p != 0 && image_id != 0);
                match self.kitty_img_place(Some(image_id), image_number, placement, verbosity) {
                    Ok(()) => {
                        if image_id != 0 || image_number.is_some() {
                            self.kitty_send_placement_response(
                                verbosity,
                                true,
                                Some(image_id),
                                image_number,
                                placement_id,
                                "OK".to_string(),
                            );
                        }
                        Ok(())
                    }
                    Err(err) => {
                        if image_id != 0 || image_number.is_some() {
                            self.kitty_send_placement_response(
                                verbosity,
                                false,
                                Some(image_id),
                                image_number,
                                placement_id,
                                placement_error(&err),
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
                    | KittyImage::ControlAnimation { .. }
            )
        {
            self.kitty_interrupt_transfer();
        }

        let verbosity = img.verbosity();
        match img {
            // The legacy cell-backed renderer does not consume animation
            // controls. Keep its pixels, gaps and transfer state unchanged.
            KittyImage::ControlAnimation { control, .. } => {
                if let Some(id) = control.image_id.or_else(|| control.image_number.and_then(|no| self.kitty_img.id_for_number(no))) {
                    if let Some(data) = self.kitty_img.id_to_data.get(&id) {
                        let now = monotonic_ms();
                        if let Some(animation) = self.kitty_img.animations.get_mut(&id) {
                            if animation.control(&control, now) { self.kitty_img.selection_revision += 1; }
                        } else {
                            let mut animation = match &*data.data() {
                                ImageDataType::AnimRgba8 { durations, .. } => KittyAnimation::new(durations.iter().map(|gap| gap.as_millis().min(u32::MAX as u128) as u32), now),
                                _ => KittyAnimation::new([0], now),
                            };
                            if animation.control(&control, now) {
                                self.kitty_img.animations.insert(id, animation);
                                self.kitty_img.selection_revision += 1;
                            }
                        }
                    }
                }
            }
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
                let id = image_id.or_else(|| image_number.and_then(|n| self.kitty_img.id_for_number(n)));
                let placement_id = placement.placement_id.filter(|p| *p != 0 && id != Some(0));
                let result = if image_id.is_some() && image_number.is_some() {
                    Err(anyhow::anyhow!("EINVAL: i and I are mutually exclusive"))
                } else {
                    self.kitty_img_place(image_id, image_number, placement, verbosity)
                };
                self.kitty_send_placement_response(verbosity, result.is_ok(), id, image_number, placement_id,
                    result.as_ref().map(|_| "OK".to_owned()).unwrap_or_else(placement_error));
                result?;
            }
            KittyImage::Delete { what, .. } => self.kitty_delete(what),
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
            .filter(|(key, _)| key.image_id == image_id)
            .map(|(_, placement)| placement.info)
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

    fn kitty_image_for_mutation(image: &Arc<ImageData>) -> Arc<ImageData> {
        if ImageDataType::is_nonce_key(&image.hash()) {
            Arc::clone(image)
        } else {
            Arc::new(image.copy_for_mutation())
        }
    }

    /// Install a successful first edit. Later edits keep the private identity.
    fn kitty_commit_image_mutation(&mut self, image_id: u32, image: &Arc<ImageData>) -> bool {
        let old = &self.kitty_img.id_to_data[&image_id];
        if Arc::ptr_eq(old, image) {
            return false;
        }
        let old = Arc::clone(old);
        self.kitty_img.id_to_data.insert(image_id, Arc::clone(image));
        if !self.kitty_img.placements.keys().any(|key| key.image_id == image_id) {
            return true;
        }
        let seqno = self.seqno;
        // Reflow can move attachments outside their recorded placement rows.
        // Scan only on the transition from content-shared to private pixels.
        for alternate in [false, true] {
            self.screen.screen_for_alt_mut(alternate).for_each_phys_line_mut(|_, line| {
                if !line.has_images() {
                    return;
                }
                let references_image = line.visible_cells().any(|cell| {
                    cell.attrs().image_attachments().any(|attached| {
                        attached.image_id() == Some(image_id)
                            && Arc::ptr_eq(attached.image_data(), &old)
                    })
                });
                if references_image {
                    for cell in line.cells_mut_for_attr_changes_only() {
                        cell.attrs_mut().replace_image_data(image_id, &old, image);
                    }
                    line.update_last_change_seqno(seqno);
                }
            });
        }
        true
    }

    pub(super) fn kitty_prune_placements(&mut self) {
        let mut live = HashSet::new();
        for alternate in [false, true] {
            self.screen_for_alt(alternate).for_each_phys_line(|_, line| {
                if !line.has_images() { return; }
                for cell in line.visible_cells() {
                    for image in cell.attrs().image_attachments() {
                        if let Some(image_id) = image.image_id() {
                            live.insert(PlacementKey { image_id, placement_id: image.placement_tag() });
                        }
                    }
                }
            });
        }
        self.kitty_img.placements.retain(|key, _| live.contains(key));
    }

    pub(super) fn kitty_remove_placement(&mut self, image_id: u32, placement_id: Option<u32>) {
        let selected = self.kitty_img.placements.keys().copied()
            .chain(self.kitty_img.relatives.iter().map(|p| p.key))
            .chain(self.kitty_img.virtual_placements.for_image(image_id).iter().map(|p| PlacementKey {
                image_id, placement_id: u64::from(p.placement_id),
            })).filter(|key| {
            key.image_id == image_id && placement_id.is_none_or(|id| key.protocol_id() == Some(id))
        }).collect();
        self.kitty_delete_selected_placements(selected, HashSet::new(), true);
    }

    fn kitty_delete(&mut self, what: KittyImageDelete) {
        self.kitty_scene_refresh();
        use KittyImageDelete::*;
        let delete = match what {
            ByImageId { image_id, placement_id, delete } => {
                self.kitty_delete_image_ids(&[image_id], placement_id, delete);
                return;
            }
            ByImageNumber { image_number, placement_id, delete } => {
                if let Some(id) = self.kitty_img.id_for_number(image_number) {
                    self.kitty_delete_image_ids(&[id], placement_id, delete);
                }
                return;
            }
            ByImageIdRange { first, last, delete } => {
                let ids: Vec<_> = self.kitty_img.id_to_data.keys().copied()
                    .filter(|id| *id != 0 && first <= *id && *id <= last).collect();
                self.kitty_delete_image_ids(&ids, None, delete);
                return;
            }
            AnimationFrames { image_id, image_number, frame_number, delete } => {
                self.kitty_delete_frame(image_id, image_number, frame_number, delete);
                return;
            }
            All { delete } | AtCursorPosition { delete } | DeleteAt { delete, .. }
            | DeleteAtZ { delete, .. } | DeleteColumn { delete, .. }
            | DeleteRow { delete, .. } | DeleteZ { delete, .. } => delete,
        };
        let screen = self.screen();
        let top = screen.phys_row(0);
        let mut selected = HashSet::new();
        screen.for_each_phys_line(|physical, line| {
            if !line.has_images() {
                return;
            }
            let row = physical as isize - top as isize;
            for cell in line.visible_cells() {
                let column = cell.cell_index();
                let at = |x: u32, y: u32| {
                    x.checked_sub(1).is_some_and(|x| column == x as usize)
                        && y.checked_sub(1).is_some_and(|y| row == y as isize)
                };
                for image in cell.attrs().image_attachments() {
                    let Some(id) = image.image_id() else { continue };
                    let matches = match what {
                        All { .. } => row >= 0 && row < screen.physical_rows as isize,
                        AtCursorPosition { .. } => column == self.cursor.x && row == self.cursor.y as isize,
                        DeleteAt { x, y, .. } => at(x, y),
                        DeleteAtZ { x, y, z, .. } => at(x, y) && image.z_index() == z,
                        DeleteColumn { x, .. } => x.checked_sub(1).is_some_and(|x| column == x as usize),
                        DeleteRow { y, .. } => y.checked_sub(1).is_some_and(|y| row == y as isize),
                        DeleteZ { z, .. } => image.z_index() == z,
                        _ => false,
                    };
                    if matches {
                        selected.insert(PlacementKey { image_id: id, placement_id: image.placement_tag() });
                    }
                }
            }
        });
        let cell = ((self.pixel_width / screen.physical_cols) as u32, (self.pixel_height / screen.physical_rows) as u32);
        if cell.0 > 0 && cell.1 > 0 {
            for (&id, views) in &self.kitty_img.relative_views {
                let Some(data) = self.kitty_img.id_to_data.get(&id) else { continue };
                let Ok(size) = data.data().dimensions() else { continue };
                for view in views {
                    if view.anchor.alt_screen != self.screen.is_alt_screen_active() { continue; }
                    let Some(layout) = view.geometry.layout(size, cell) else { continue };
                    let x0 = view.anchor.column as f64 + layout.rect[0] / f64::from(cell.0);
                    let y0 = view.anchor.row as f64 - screen.visible_row_to_stable_row(0) as f64 + layout.rect[1] / f64::from(cell.1);
                    let x1 = view.anchor.column as f64 + layout.rect[2] / f64::from(cell.0);
                    let y1 = view.anchor.row as f64 - screen.visible_row_to_stable_row(0) as f64 + layout.rect[3] / f64::from(cell.1);
                    let column = |x: f64| x < x1 && x + 1.0 > x0;
                    let row = |y: f64| y < y1 && y + 1.0 > y0;
                    let matches = match what {
                        All { .. } => x1 > 0.0 && y1 > 0.0 && x0 < screen.physical_cols as f64 && y0 < screen.physical_rows as f64,
                        AtCursorPosition { .. } => column(self.cursor.x as f64) && row(self.cursor.y as f64),
                        DeleteAt { x, y, .. } => x > 0 && y > 0 && column(f64::from(x - 1)) && row(f64::from(y - 1)),
                        DeleteAtZ { x, y, z, .. } => x > 0 && y > 0 && column(f64::from(x - 1)) && row(f64::from(y - 1)) && view.geometry.z_index == z,
                        DeleteColumn { x, .. } => x > 0 && column(f64::from(x - 1)),
                        DeleteRow { y, .. } => y > 0 && row(f64::from(y - 1)),
                        DeleteZ { z, .. } => view.geometry.z_index == z,
                        _ => false,
                    };
                    if matches { selected.insert(PlacementKey { image_id: id, placement_id: view.placement_id }); }
                }
            }
        }
        let release = selected.iter().filter_map(|key| (delete || key.image_id == 0).then_some(key.image_id)).collect();
        self.kitty_delete_selected_placements(selected, release, true);
    }

    fn kitty_delete_image_ids(&mut self, ids: &[u32], placement: Option<u32>, delete: bool) {
        let ids: HashSet<_> = ids.iter().copied().filter(|id| *id != 0).collect();
        let placement = placement.filter(|id| *id != 0);
        let mut selected: HashSet<_> = self.kitty_img.placements.keys().copied()
            .chain(self.kitty_img.relatives.iter().map(|p| p.key))
            .filter(|key| ids.contains(&key.image_id) && placement.is_none_or(|id| key.protocol_id() == Some(id)))
            .collect();
        let mut release: HashSet<_> = if delete {
            selected.iter().map(|key| key.image_id).collect()
        } else {
            HashSet::new()
        };
        for id in ids {
            selected.extend(self.kitty_img.virtual_placements.for_image(id).into_iter()
                .filter(|p| placement.is_none_or(|wanted| p.placement_id == wanted))
                .map(|p| PlacementKey { image_id: id, placement_id: u64::from(p.placement_id) }));
            let removed = self.kitty_img.virtual_placements.remove(id, placement);
            if removed {
                self.kitty_img.selection_revision += 1;
            }
            if delete && (removed || placement.is_none()) {
                release.insert(id);
            }
        }
        self.kitty_delete_selected_placements(selected, release, true);
    }

    fn kitty_delete_selected_placements(
        &mut self,
        selected: HashSet<PlacementKey>,
        mut release: HashSet<u32>,
        cascade: bool,
    ) {
        if selected.is_empty() && release.is_empty() {
            return;
        }
        // Placement coordinates can be stale after reflow. Select by actual
        // cells, then detach every piece of each selected placement in one pass.
        let scan = !selected.is_empty()
            || self.kitty_img.placements.keys().any(|key| release.contains(&key.image_id));
        if scan {
            let seqno = self.seqno;
            let mut remaining = HashSet::new();
            let matches = |image: &wezterm_cell::image::ImageCell| {
                image.image_id().is_some_and(|id| selected.contains(&PlacementKey { image_id: id, placement_id: image.placement_tag() }))
            };
            for alternate in [false, true] {
                self.screen.screen_for_alt_mut(alternate).for_each_phys_line_mut(|_, line| {
                    if !line.has_images() {
                        return;
                    }
                    if line.visible_cells().any(|cell| cell.attrs().image_attachments().any(matches)) {
                        for cell in line.cells_mut_for_attr_changes_only() {
                            cell.attrs_mut().detach_images(matches);
                        }
                        line.update_last_change_seqno(seqno);
                    }
                    for cell in line.visible_cells() {
                        for image in cell.attrs().image_attachments() {
                            if let Some(id) = image.image_id() {
                                remaining.insert(PlacementKey { image_id: id, placement_id: image.placement_tag() });
                            }
                        }
                    }
                });
            }
            self.kitty_img.placements.retain(|key, _| remaining.contains(key));
        }
        if cascade { self.kitty_remove_relative_dependents(selected.iter().copied()); }
        self.kitty_img.relative_seqno = None;
        // Uppercase deletion releases data only after the last ordinary or
        // virtual placement disappears. Unplaced images use the ID path above.
        if release.is_empty() {
            return;
        }
        let retained: HashSet<_> = self.kitty_img.placements.keys().map(|key| key.image_id).collect();
        release.retain(|id| !retained.contains(id) && !self.kitty_img.virtual_placements.contains_image(*id)
            && !self.kitty_img.relatives.iter().any(|p| p.key.image_id == *id));
        self.kitty_img.evict_unplaced(&release);
    }

    fn kitty_delete_frame(
        &mut self,
        image_id: Option<u32>,
        image_number: Option<u32>,
        frame_number: Option<u32>,
        delete: bool,
    ) {
        let Some(id) = image_id.filter(|id| *id != 0).or_else(|| {
            image_number.and_then(|number| self.kitty_img.id_for_number(number))
        }) else { return };
        let Some(old) = self.kitty_img.id_to_data.get(&id).cloned() else { return };
        let data = old.data();
        let count = match &*data {
            ImageDataType::AnimRgba8 { frames, .. } => frames.len(),
            _ => 1,
        };
        if count <= 1 {
            drop(data);
            if delete {
                self.kitty_delete_image_ids(&[id], None, true);
            }
            return;
        }
        let index = (frame_number.unwrap_or(1).max(1) as usize).min(count) - 1;
        let ImageDataType::AnimRgba8 { width, height, frames, hashes, durations } = &*data else { return };
        let now = monotonic_ms();
        let mut animation = self.kitty_img.animations.get(&id).cloned().unwrap_or_else(|| {
            KittyAnimation::new(durations.iter().map(|gap| gap.as_millis().min(u32::MAX as u128) as u32), now)
        });
        if !animation.remove_frame(index, now) {
            return;
        }
        // Shrinking under an existing identity leaves remote frame cursors and
        // append-only image caches pointing at obsolete indices. Publish a new
        // immutable payload and rebind placements; copy only surviving frames.
        let payload = if count == 2 {
            let kept = 1 - index;
            ImageDataType::Rgba8 { width: *width, height: *height, data: frames[kept].clone(), hash: hashes[kept] }
        } else {
            ImageDataType::AnimRgba8 {
                width: *width, height: *height,
                frames: frames.iter().enumerate().filter(|(i, _)| *i != index).map(|(_, frame)| frame.clone()).collect(),
                hashes: hashes.iter().enumerate().filter(|(i, _)| *i != index).map(|(_, hash)| *hash).collect(),
                durations: durations.iter().enumerate().filter(|(i, _)| *i != index).map(|(_, gap)| *gap).collect(),
            }
        };
        // A payload keyed by a surviving frame's nonce can come back under
        // the animation's own identity, and a remote copy never takes the
        // shorter list under the same hash.
        let hash = Some(payload.compute_hash()).filter(|hash| *hash != old.hash());
        let image = Arc::new(ImageData::with_data_and_hash(payload, hash.unwrap_or_else(ImageDataType::nonce_key)));
        image.set_generation(old.generation() + 1);
        drop(data);
        self.kitty_commit_image_mutation(id, &image);
        self.kitty_img.animations.insert(id, animation);
        self.kitty_img.selection_revision += 1;
        self.kitty_img.recompute_used_memory();
        self.kitty_img.mark_newest(id);
        self.kitty_enforce_image_budget();
    }

    pub(crate) fn kitty_remove_all_placements(&mut self, delete: bool) {
        let selected = self.kitty_img.placements.keys().copied().collect();
        self.kitty_delete_selected_placements(selected, HashSet::new(), true);
        if delete {
            self.kitty_img.relatives = Default::default();
            self.kitty_img.relative_views = Default::default();
            self.kitty_img.relative_seqno = None;
            self.kitty_img.virtual_placements = Default::default();
            if !self.kitty_img.animations.is_empty() {
                self.kitty_img.animations = HashMap::new();
            }
            if !self.kitty_img.id_to_data.is_empty() {
                self.kitty_img.selection_revision += 1;
            }
            self.kitty_img.id_to_data.clear();
            self.kitty_img.id_seq.clear();
            self.kitty_img.used_memory = 0;
            self.kitty_img.numbered_images.clear();
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
        self.kitty_send_placement_response(verbosity, success, image_id, image_no, None, message);
    }

    fn kitty_send_placement_response(
        &mut self,
        verbosity: KittyImageVerbosity,
        success: bool,
        image_id: Option<u32>,
        image_no: Option<u32>,
        placement_id: Option<u32>,
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
        // The message can carry text the application chose (a shm name, a
        // file path in an error), and this reply is written to the pty as
        // if typed. A control byte in it would be a keystroke in the user's
        // shell, so only printable ASCII goes out, and not much of it.
        let message: String = message
            .chars()
            .filter(|c| (' '..='~').contains(c))
            .take(256)
            .collect();

        match (image_id, image_no) {
            (Some(id), Some(no)) => { write!(self.writer, "\x1b_GI={},i={}", no, id).ok(); }
            (Some(id), None) => { write!(self.writer, "\x1b_Gi={}", id).ok(); }
            (None, Some(no)) => { write!(self.writer, "\x1b_GI={}", no).ok(); }
            (None, None) => return,
        }
        if let Some(id) = placement_id { write!(self.writer, ",p={}", id).ok(); }
        write!(self.writer, ";{}\x1b\\", message).ok();
        self.writer.flush().ok();
    }

    fn kitty_frame_compose(
        &mut self,
        frame: KittyImageFrameCompose,
        verbosity: KittyImageVerbosity,
    ) -> anyhow::Result<()> {
        let image_id = match frame.image_number {
            Some(no) => match self.kitty_img.id_for_number(no) {
                Some(id) => id,
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

        let image = Self::kitty_image_for_mutation(
            self.kitty_img
                .id_to_data
                .get(&image_id)
                .ok_or_else(|| anyhow::anyhow!("invalid image id {}", image_id))?,
        );

        let mut img = image.data();
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

        // Under the data lock, so whoever reads the payload and the
        // generation together sees them agree.
        image.bump_generation();
        drop(img);
        if self.kitty_commit_image_mutation(image_id, &image) {
            self.kitty_img.recompute_used_memory();
            self.kitty_img.mark_newest(image_id);
            self.kitty_enforce_image_budget();
        }
        self.kitty_img.selection_revision += 1;
        self.kitty_touch_placements_for_image(image_id);

        Ok(())
    }

    fn kitty_frame_transmit(
        &mut self,
        mut transmit: KittyImageTransmit,
        frame: KittyImageFrame,
        verbosity: KittyImageVerbosity,
    ) -> anyhow::Result<()> {
        let image_number = transmit.image_number;
        if transmit.image_id.is_none() {
            if let Some(no) = image_number {
                let Some(id) = self.kitty_img.id_for_number(no) else {
                    self.kitty_send_response(verbosity, false, None, Some(no), "ENOENT".to_string());
                    anyhow::bail!("no such image_number {}", no);
                };
                transmit.image_number = None;
                transmit.image_id = Some(id);
            }
        }

        let (image_id, _, img) = self.kitty_img_transmit_inner(transmit, verbosity)?;

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
            Some(anim) => Self::kitty_image_for_mutation(anim),
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
        let count = match &*anim { ImageDataType::AnimRgba8 { frames, .. } => frames.len(), _ => 1 };
        let target = frame.frame_number.unwrap_or(count as u32 + 1) as usize;
        let explicit_gap = if frame.gapless { Some(0) } else { frame.duration_ms.filter(|gap| *gap > 0) };
        let new_timeline = if !self.kitty_img.animations.contains_key(&image_id)
            && (target == count + 1 || explicit_gap.is_some())
        {
            Some(match &*anim {
                ImageDataType::AnimRgba8 { durations, .. } => KittyAnimation::new(durations.iter().map(|gap| gap.as_millis().min(u32::MAX as u128) as u32), monotonic_ms()),
                _ => KittyAnimation::new([0], monotonic_ms()),
            })
        } else { None };
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

        image.bump_generation();
        drop(anim);
        self.kitty_commit_image_mutation(image_id, &image);
        if let Some(animation) = new_timeline {
            self.kitty_img.animations.insert(image_id, animation);
        }
        if let Some(animation) = self.kitty_img.animations.get_mut(&image_id) {
            let now = monotonic_ms();
            if target == count + 1 {
                animation.append(explicit_gap.unwrap_or(40), now);
            } else if let Some(gap) = explicit_gap {
                animation.set_gap(target - 1, gap, now);
            }
        }
        // Subscribers also track pixel generations: an edit with no
        // timing change still needs a new metadata snapshot.
        self.kitty_img.selection_revision += 1;

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
                // Publish this allocation only after decoding and storing the
                // image succeeds; failures must retain the previous mapping.
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
                (id, Some(no))
            }
        };

        let data = match transmit.data.load_data() {
            Ok(data) => data,
            Err(err) => {
                // A client that counts one answer per transmission (q=0, or
                // a stream pacing itself on ACKs) would otherwise wait on a
                // reply that never comes; kitty answers a failed read too.
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
        let img = self
            .raw_image_to_image_data(img)
            .context("storing image data")?;
        if image_id != 0 { self.kitty_delete_image_ids(&[image_id], None, false); }
        self.kitty_img
            .record_id_to_data(image_id, img, self.config.kitty_image_memory_budget());
        self.kitty_img.max_image_id = self.kitty_img.max_image_id.max(image_id);
        if let Some(number) = image_number {
            self.kitty_img.numbered_images.insert((number, image_id));
        }
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
