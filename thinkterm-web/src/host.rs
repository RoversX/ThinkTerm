//! The session layer's view of the App's platform: a clock, a spawner and
//! an event sink over a [`Platform`], and the link. One type for every
//! client; only `P` and `L` differ.

use crate::platform::{Link, Platform};
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use thinkterm_session::clock::{Clock, Timestamp};
use thinkterm_session::host::{
    DetachedFuture, HostConfig, HostPaneId, ImageDomainKey, SessionEvents, SessionHost, Spawner,
};

pub struct PlatformClock<P: Platform>(Rc<P>);

impl<P: Platform> Clock for PlatformClock<P> {
    fn now(&self) -> Timestamp {
        Timestamp::from_micros((self.0.monotonic_ms() * 1000.0) as u64)
    }

    fn wall_millis(&self) -> u64 {
        self.0.wall_ms() as u64
    }
}

pub struct PlatformSpawner<P: Platform>(Rc<P>);

impl<P: Platform> Spawner for PlatformSpawner<P> {
    fn spawn_detached(&self, fut: DetachedFuture) {
        self.0.spawn(fut);
    }
}

/// Which panes had output since the last frame, and a wake to ask for
/// one when the first arrives.
#[derive(Default)]
pub struct Events {
    dirty: RefCell<std::collections::HashSet<HostPaneId>>,
    wake: RefCell<Option<Rc<dyn Fn()>>>,
    /// A pane rang its bell; the platform decides what that sounds like.
    bell: RefCell<Option<Rc<dyn Fn(HostPaneId)>>>,
}

impl Events {
    pub fn set_wake(&self, wake: Rc<dyn Fn()>) {
        *self.wake.borrow_mut() = Some(wake);
    }

    pub fn set_bell(&self, bell: Rc<dyn Fn(HostPaneId)>) {
        *self.bell.borrow_mut() = Some(bell);
    }

    pub fn take_dirty(&self) -> std::collections::HashSet<HostPaneId> {
        std::mem::take(&mut *self.dirty.borrow_mut())
    }

    fn mark(&self, pane: HostPaneId) {
        self.dirty.borrow_mut().insert(pane);
        if let Some(wake) = self.wake.borrow().clone() {
            wake();
        }
    }
}

impl SessionEvents for Events {
    fn pane_output(&self, pane: HostPaneId) {
        self.mark(pane);
    }
    fn alert(&self, pane: HostPaneId, alert: wezterm_term::Alert) {
        if matches!(alert, wezterm_term::Alert::Bell) {
            if let Some(bell) = self.bell.borrow().clone() {
                bell(pane);
            }
        }
    }
    fn agent_status_changed(&self, _pane: HostPaneId) {}
    // The page draws no tab icons yet.
    fn foreground_program_changed(&self, _pane: HostPaneId) {}
    fn pane_removed(&self, pane: HostPaneId) {
        self.mark(pane);
    }
    fn pane_focused(&self, _pane: HostPaneId) {}
    fn input_recorded(&self) {}
}

pub struct Config {
    rules: Arc<Vec<termwiz::hyperlink::Rule>>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            rules: Arc::new(Vec::new()),
        }
    }
}

impl HostConfig for Config {
    fn hyperlink_rules(&self) -> Arc<Vec<termwiz::hyperlink::Rule>> {
        Arc::clone(&self.rules)
    }
    fn fetch_rate_per_second(&self) -> u32 {
        10
    }
    fn scrollback_lookahead_screens(&self) -> usize {
        1
    }
    fn warm_scrollback(&self) -> bool {
        false
    }
}

/// Distinct images a pane's rows are remembered to show. Past this, any
/// image change refetches the rows, as before the rows were tracked.
const MAX_SHOWN_IMAGES: usize = 4096;

#[derive(Default)]
struct PaneImages {
    epoch: u64,
    versions: std::collections::HashMap<(u32, [u8; 32]), u64>,
    virtual_images: std::collections::HashMap<u32, ([u8; 32], Vec<wezterm_term::kitty_virtual::VirtualPlacement>)>,
    relatives: std::collections::HashMap<u32, ([u8; 32], u64, Vec<wezterm_term::kitty_relative::RelativeView>)>,
    /// Images that rows hydrated since the last refetch show, attached or
    /// as Unicode placeholders. A change to any other image, such as an
    /// unplaced transmission, leaves the cached rows and their fetches alone.
    shown: std::collections::HashSet<u32>,
    shown_overflow: bool,
}

fn show_image(shown: &mut std::collections::HashSet<u32>, overflow: &mut bool, image_id: u32) {
    if shown.len() < MAX_SHOWN_IMAGES || shown.contains(&image_id) {
        shown.insert(image_id);
    } else {
        *overflow = true;
    }
}

#[derive(Default)]
pub(crate) struct ImageVersions {
    domain: usize,
    panes: std::collections::HashMap<HostPaneId, PaneImages>,
}

impl ImageVersions {
    /// Returns whether the image epoch was reset, and whether cached rows
    /// show an image whose pixels or virtual placements changed, appeared or
    /// went away, so that the pane's rows have to be fetched again.
    pub fn observe(&mut self, pane: HostPaneId, epoch: u64, selections: &[wezterm_term::KittyFrameSelection]) -> (bool, bool) {
        // A pane met for the first time (a new split, a parked tab brought
        // back) resets nothing shared: only its own rows are fetched again,
        // as those hydrated before this snapshot could not resolve against
        // it. A changed epoch means the server's images were replaced.
        let first = !self.panes.contains_key(&pane);
        let reset = self.panes.get(&pane).is_some_and(|held| held.epoch != epoch);
        // Images whose pixels or virtual placements differ, and images that
        // appeared or went away.
        let mut changed = Vec::new();
        match self.panes.get(&pane) {
            None => changed.extend(selections.iter().map(|s| s.image_id)),
            Some(held) => {
                let current: std::collections::HashSet<_> = selections.iter().map(|s| (s.image_id, s.data_hash)).collect();
                changed.extend(selections.iter().filter(|s| {
                    held.versions.get(&(s.image_id, s.data_hash)) != Some(&s.data_generation)
                        || held.virtual_images.get(&s.image_id).map(|(_, p)| p.as_slice()).unwrap_or(&[])
                            != s.virtual_placements.as_slice()
                }).map(|s| s.image_id));
                changed.extend(held.versions.keys().filter(|key| !current.contains(key)).map(|(id, _)| *id));
            }
        }
        let relative_changed = self.panes.get(&pane).is_none_or(|held| {
            held.relatives.len() != selections.iter().filter(|s| !s.relative_placements.is_empty()).count()
                || selections.iter().filter(|s| !s.relative_placements.is_empty()).any(|s| {
                    held.relatives.get(&s.image_id).is_none_or(|(hash, generation, views)|
                        *hash != s.data_hash || *generation != s.data_generation || *views != s.relative_placements)
                })
        });
        if !first && !reset && changed.is_empty() && !relative_changed { return (false, false); }
        if reset { self.domain += 1; }
        let held = self.panes.entry(pane).or_default();
        held.epoch = epoch;
        held.relatives = selections.iter().filter(|s| !s.relative_placements.is_empty())
            .map(|s| (s.image_id, (s.data_hash, s.data_generation, s.relative_placements.clone()))).collect();
        if !reset && changed.is_empty() { return (false, false); }
        let rows_changed = !reset && (first || held.shown_overflow || changed.iter().any(|id| held.shown.contains(id)));
        if reset || rows_changed {
            // Every cached row is fetched and hydrated again, which records
            // the images it shows anew.
            held.shown.clear();
            held.shown_overflow = false;
        }
        held.versions.clear();
        held.versions.extend(selections.iter().map(|s| ((s.image_id, s.data_hash), s.data_generation)));
        held.virtual_images = selections.iter()
            .filter(|s| !s.virtual_placements.is_empty() && wezterm_term::kitty_virtual::valid(&s.virtual_placements))
            .map(|s| (s.image_id, (s.data_hash, s.virtual_placements.clone())))
            .collect();
        (reset, rows_changed)
    }

    pub fn relative_images(&self, pane: HostPaneId) -> impl Iterator<Item = (u32, [u8; 32], u64, &[wezterm_term::kitty_relative::RelativeView])> {
        self.panes.get(&pane).into_iter().flat_map(|held| held.relatives.iter())
            .map(|(id, (hash, generation, views))| (*id, *hash, *generation, views.as_slice()))
    }

    fn resolve_cells(&mut self, pane: HostPaneId, lines: &mut [(wezterm_term::StableRowIndex, wezterm_term::Line)], images: &mut Vec<codec::SerializedImageCell>) {
        let Some(held) = self.panes.get_mut(&pane) else { return };
        let PaneImages { versions, virtual_images, shown, shown_overflow, .. } = held;
        for image in images.iter() {
            if let Some(image_id) = image.image_id {
                show_image(shown, shown_overflow, image_id);
            }
        }
        for (row, line) in lines {
            let mut has_placeholder = false;
            // Decode the immutable row first; changing only placeholder attrs
            // below keeps the original text available to selection and copy.
            crate::placeholders::visit(line, |column, placeholder| {
                has_placeholder = true;
                // Also before its virtual placement is known: its arrival is
                // what these rows wait for.
                show_image(shown, shown_overflow, placeholder.image_id);
                let Some((hash, placements)) = virtual_images.get(&placeholder.image_id) else { return };
                let Some(placement) = wezterm_term::kitty_virtual::find(placements, placeholder.placement_id) else { return };
                use termwiz::image::TextureCoordinate;
                images.push(codec::SerializedImageCell {
                    line_idx: *row, cell_idx: column,
                    top_left: TextureCoordinate::new_f32(0.0, 0.0),
                    bottom_right: TextureCoordinate::new_f32(1.0, 1.0),
                    data_hash: *hash,
                    data_generation: versions.get(&(placeholder.image_id, *hash)).copied().unwrap_or(0),
                    z_index: -1,
                    padding_left: 0, padding_top: 0, padding_right: 0, padding_bottom: 0,
                    image_id: Some(placeholder.image_id), placement_id: Some(placement.placement_id),
                });
            });
            if has_placeholder {
                for cell in line.cells_mut_for_attr_changes_only() {
                    if cell.str().starts_with(crate::placeholders::PLACEHOLDER) {
                        cell.attrs_mut().set_invisible(true);
                    }
                }
            }
        }
    }

    pub fn forget(&mut self, pane: HostPaneId) { self.panes.remove(&pane); }

    pub fn reset(&mut self) {
        self.domain += 1;
        self.panes.clear();
    }

    fn generation(&self, pane: HostPaneId, image: &codec::SerializedImageCell) -> u64 {
        match (image.image_id, self.panes.get(&pane)) {
            (Some(id), Some(held)) => held.versions.get(&(id, image.data_hash)).copied().unwrap_or(0),
            _ => image.data_generation,
        }
    }

    fn request(&self, request: codec::GetImageCell, image_id: Option<u32>) -> codec::Pdu {
        match (image_id, self.panes.get(&request.pane_id)) {
            // Every transmission without an id is image 0 on the server, and
            // only the newest is kept there; an older one is found by its cell.
            (Some(image_id), Some(held)) if image_id != 0 => codec::Pdu::GetKittyImage(codec::GetKittyImage {
                pane_id: request.pane_id, image_id, data_hash: request.data_hash,
                have_frames: request.have_frames, image_epoch: Some(held.epoch),
            }),
            _ => codec::Pdu::GetImageCell(request),
        }
    }
}

pub struct AppHost<P: Platform, L: Link> {
    pub clock: PlatformClock<P>,
    pub spawner: PlatformSpawner<P>,
    pub events: Events,
    pub link: L,
    pub config: Config,
    pub(crate) image_versions: RefCell<ImageVersions>,
}

impl<P: Platform, L: Link> AppHost<P, L> {
    pub fn new(platform: Rc<P>, link: L) -> Self {
        // The page and the mobile app both come through here; either needs a
        // store before a leased picture can arrive.
        thinkterm_session::blobs::ensure_blob_storage();
        Self {
            clock: PlatformClock(Rc::clone(&platform)),
            spawner: PlatformSpawner(platform),
            events: Events::default(),
            link,
            config: Config::default(),
            image_versions: RefCell::new(ImageVersions::default()),
        }
    }
}

impl<P: Platform, L: Link> SessionHost for AppHost<P, L> {
    type Clock = PlatformClock<P>;
    type Spawner = PlatformSpawner<P>;
    type Events = Events;
    type Link = L;
    type Config = Config;

    fn clock(&self) -> &PlatformClock<P> {
        &self.clock
    }
    fn spawner(&self) -> &PlatformSpawner<P> {
        &self.spawner
    }
    fn events(&self) -> &Events {
        &self.events
    }
    fn link(&self) -> &L {
        &self.link
    }
    fn config(&self) -> &Config {
        &self.config
    }
    fn image_generation(&self, pane: HostPaneId, image: &codec::SerializedImageCell) -> u64 {
        self.image_versions.borrow().generation(pane, image)
    }
    fn image_request(&self, request: codec::GetImageCell, image_id: Option<u32>) -> codec::Pdu {
        self.image_versions.borrow().request(request, image_id)
    }
    fn resolve_image_cells(&self, pane: HostPaneId, lines: &mut [(wezterm_term::StableRowIndex, wezterm_term::Line)], images: &mut Vec<codec::SerializedImageCell>) {
        self.image_versions.borrow_mut().resolve_cells(pane, lines, images);
    }
    fn image_domain(&self) -> ImageDomainKey {
        self.image_versions.borrow().domain
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codec::{GetImageCell, Pdu, SerializedImageCell};
    use termwiz::image::TextureCoordinate;
    use wezterm_term::kitty_animation::{KittyAnimation, Playback};

    #[test]
    fn relative_motion_updates_the_scene_without_invalidating_terminal_rows() {
        use wezterm_term::kitty_relative::{Anchor, RelativeView};
        let mut versions = ImageVersions::default();
        let mut selection = wezterm_term::KittyFrameSelection {
            image_id: 4, data_hash: [7; 32], data_generation: 3,
            animation: KittyAnimation::new([0], 0), virtual_placements: vec![],
            relative_placements: vec![RelativeView { placement_id: 2, source_seqno: 5,
                anchor: Anchor { column: 1, row: 3, alt_screen: false }, geometry: Default::default(), image_size: (20, 10) }],
        };
        let domain = versions.domain;
        assert_eq!(versions.observe(1, 5, &[selection.clone()]), (false, true));
        assert_eq!(versions.domain, domain);
        selection.relative_placements[0].anchor.row = 9;
        selection.relative_placements[0].source_seqno = 6;
        assert_eq!(versions.observe(1, 5, &[selection.clone()]), (false, false));
        assert_eq!(versions.relative_images(1).next().unwrap().3, selection.relative_placements);
        assert_eq!(versions.domain, domain);
        selection.relative_placements.clear();
        assert_eq!(versions.observe(1, 5, &[selection.clone()]), (false, false));
        assert_eq!(versions.relative_images(1).count(), 0);
        assert_eq!(versions.observe(1, 6, &[selection]), (true, false));
        assert!(versions.domain > domain);
        versions.forget(1);
        assert!(versions.panes.is_empty());
    }

    #[test]
    fn placeholder_cells_resolve_ids_and_keep_text_without_changing_plain_rows() {
        use wezterm_term::{Cell, CellAttributes, Line};
        use wezterm_term::color::ColorAttribute;
        use wezterm_term::kitty_virtual::VirtualPlacement;
        let mut versions = ImageVersions::default();
        let mut selection = wezterm_term::KittyFrameSelection { relative_placements: Vec::new(),
            image_id: 4, data_hash: [7; 32], data_generation: 3,
            animation: KittyAnimation::new([0], 0),
            virtual_placements: vec![VirtualPlacement { placement_id: 2, columns: 3, rows: 2 }],
        };
        versions.observe(1, 0, &[selection.clone()]);
        let mut attrs = CellAttributes::default();
        attrs.set_foreground(ColorAttribute::PaletteIndex(4));
        attrs.set_underline_color(ColorAttribute::PaletteIndex(2));
        let mut line = Line::new(0);
        line.set_cell(0, Cell::new(crate::placeholders::PLACEHOLDER, attrs.clone()), 0);
        line.set_cell(1, Cell::new(crate::placeholders::PLACEHOLDER, attrs), 0);
        line.set_cell(2, Cell::new('x', CellAttributes::default()), 0);
        let text = line.as_str().to_string();
        let mut lines = vec![(5, line)];
        let mut images = vec![];
        versions.resolve_cells(1, &mut lines, &mut images);
        assert_eq!(images.len(), 2);
        assert_eq!((images[1].line_idx, images[1].cell_idx, images[1].image_id, images[1].placement_id), (5, 1, Some(4), Some(2)));
        assert_eq!((images[0].data_hash, images[0].data_generation, images[0].z_index), ([7; 32], 3, -1));
        assert_eq!(lines[0].1.as_str(), text);
        assert!(lines[0].1.get_cell(0).unwrap().attrs().invisible());
        assert!(!lines[0].1.get_cell(2).unwrap().attrs().invisible());
        selection.virtual_placements[0].columns = 9;
        assert_eq!(versions.observe(1, 0, &[selection.clone()]), (false, true));
        // Until the rows are hydrated again, nothing records them as showing
        // the image; they are already on their way.
        selection.virtual_placements[0].columns = 8;
        assert_eq!(versions.observe(1, 0, &[selection.clone()]), (false, false));
        images.clear();
        versions.resolve_cells(1, &mut lines, &mut images);
        selection.virtual_placements.clear();
        assert_eq!(versions.observe(1, 0, &[selection]), (false, true));
        images.clear();
        versions.resolve_cells(1, &mut lines, &mut images);
        assert!(images.is_empty());
    }

    #[test]
    fn images_no_row_shows_leave_the_rows_alone() {
        use wezterm_term::{Cell, CellAttributes, Line};
        use wezterm_term::color::ColorAttribute;
        use wezterm_term::kitty_virtual::VirtualPlacement;
        let selection = |image_id| wezterm_term::KittyFrameSelection { relative_placements: Vec::new(),
            virtual_placements: Vec::new(), image_id, data_hash: [image_id as u8; 32], data_generation: 1,
            animation: KittyAnimation::new([0], 0),
        };
        let mut versions = ImageVersions::default();
        assert_eq!(versions.observe(1, 5, &[]), (false, false));
        // An unplaced transmission and its deletion, as a benchmark loops.
        assert_eq!(versions.observe(1, 5, &[selection(9)]), (false, false));
        assert_eq!(versions.observe(1, 5, &[]), (false, false));

        // A row attaches image 4: its deletion concerns the rows.
        let mut attached = image();
        attached.data_hash = [4; 32];
        versions.resolve_cells(1, &mut [], &mut vec![attached]);
        assert_eq!(versions.observe(1, 5, &[selection(4)]), (false, true));
        versions.resolve_cells(1, &mut [], &mut vec![image()]);
        assert_eq!(versions.observe(1, 5, &[]), (false, true));

        // A placeholder row waits for its virtual placement to arrive.
        let mut attrs = CellAttributes::default();
        attrs.set_foreground(ColorAttribute::PaletteIndex(6));
        let mut line = Line::new(0);
        line.set_cell(0, Cell::new(crate::placeholders::PLACEHOLDER, attrs), 0);
        versions.resolve_cells(1, &mut [(3, line)], &mut vec![]);
        let mut placed = selection(6);
        placed.virtual_placements = vec![VirtualPlacement { placement_id: 1, columns: 1, rows: 1 }];
        assert_eq!(versions.observe(1, 5, &[placed.clone()]), (false, true));
        assert_eq!(versions.observe(1, 5, &[placed.clone(), selection(9)]), (false, false));

        // Past the bound every change refetches, as before rows were tracked.
        let many: Vec<_> = (0..=MAX_SHOWN_IMAGES as u32).map(|id| SerializedImageCell { image_id: Some(1000 + id), ..image() }).collect();
        versions.resolve_cells(1, &mut [], &mut many.clone());
        assert_eq!(versions.observe(1, 5, &[placed]), (false, true));
    }

    fn image() -> SerializedImageCell {
        SerializedImageCell {
            line_idx: 2, cell_idx: 3, top_left: TextureCoordinate::new_f32(0.0, 0.0),
            bottom_right: TextureCoordinate::new_f32(1.0, 1.0), data_hash: [7; 32], data_generation: 1000,
            z_index: 0, padding_left: 0, padding_top: 0, padding_right: 0, padding_bottom: 0,
            image_id: Some(4), placement_id: Some(1),
        }
    }

    #[test]
    fn canonical_versions_override_stale_proxy_tokens_but_playback_does_not_invalidate_pixels() {
        let mut versions = ImageVersions::default();
        let mut selection = wezterm_term::KittyFrameSelection { relative_placements: Vec::new(), virtual_placements: Vec::new(),
            image_id: 4, data_hash: [7; 32], data_generation: 3,
            animation: KittyAnimation::new([100, 100], 0),
        };
        assert_eq!(versions.generation(1, &image()), 1000);
        let domain = versions.domain;
        assert_eq!(versions.observe(1, 5, &[selection.clone()]), (false, true));
        assert_eq!(versions.domain, domain);
        assert_eq!(versions.generation(1, &image()), 3);
        versions.resolve_cells(1, &mut [], &mut vec![image()]);
        selection.animation.mode = Playback::Running;
        selection.animation.frame = 1;
        assert_eq!(versions.observe(1, 5, &[selection.clone()]), (false, false));
        assert_eq!(versions.domain, domain);
        selection.data_generation = 4;
        assert_eq!(versions.observe(1, 5, &[selection.clone()]), (false, true));
        selection.data_generation = 0;
        assert_eq!(versions.observe(1, 6, &[selection]), (true, false));
        assert!(versions.domain > domain);
        assert_eq!(versions.generation(1, &image()), 0);
    }

    #[test]
    fn only_negotiated_kitty_images_use_the_canonical_request_and_reset_expires_it() {
        let mut versions = ImageVersions::default();
        let request = GetImageCell { pane_id: 1, line_idx: 2, cell_idx: 3, data_hash: [7; 32], data_generation: 1000, have_frames: 2 };
        assert!(matches!(versions.request(request.clone(), Some(4)), Pdu::GetImageCell(_)));
        versions.observe(1, 5, &[]);
        match versions.request(request.clone(), Some(4)) {
            Pdu::GetKittyImage(image) => {
                assert_eq!(image.image_epoch, Some(5));
                assert_eq!(image.have_frames, 2);
                assert_eq!(image.image_id, 4);
            }
            _ => panic!("expected canonical image request"),
        }
        assert!(matches!(versions.request(request.clone(), None), Pdu::GetImageCell(_)));
        let before = versions.domain;
        versions.reset();
        assert!(versions.domain > before);
        assert!(versions.panes.is_empty());
        assert!(matches!(versions.request(request, Some(4)), Pdu::GetImageCell(_)));
    }

    #[test]
    fn anonymous_images_are_fetched_by_their_cell() {
        let mut versions = ImageVersions::default();
        versions.observe(1, 5, &[]);
        let request = GetImageCell { pane_id: 1, line_idx: 2, cell_idx: 3, data_hash: [7; 32], data_generation: 0, have_frames: 0 };
        assert!(matches!(versions.request(request.clone(), Some(0)), Pdu::GetImageCell(_)));
        assert!(matches!(versions.request(request, Some(1)), Pdu::GetKittyImage(_)));
    }
}
