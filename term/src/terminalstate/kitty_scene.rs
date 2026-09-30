//! Relative relationships and their current positions in the terminal model.

use super::*;
use crate::kitty_relative::{Anchor, PlacementGeometry, RelativePlacement, RelativeSnapshot};

impl TerminalState {
    fn kitty_root_exists(&self, key: PlacementKey) -> bool {
        self.kitty_img.placements.contains_key(&key)
            || (key.placement_id <= u64::from(u32::MAX)
                && self.kitty_img.virtual_placements.contains(key.image_id, key.placement_id as u32))
    }

    fn kitty_parent_key(&self, image_id: u32, placement_id: u32) -> Option<PlacementKey> {
        if placement_id != 0 {
            let key = PlacementKey { image_id, placement_id: u64::from(placement_id) };
            return (self.kitty_root_exists(key) || self.kitty_img.relatives.get(key).is_some()).then_some(key);
        }
        self.kitty_img.placements.keys().copied()
            .chain(self.kitty_img.relatives.iter().map(|p| p.key))
            .filter(|key| key.image_id == image_id)
            .chain(self.kitty_img.virtual_placements.first(image_id).map(|p| PlacementKey {
                image_id, placement_id: u64::from(p.placement_id),
            }))
            .min()
    }

    pub(super) fn kitty_place_relative(
        &mut self,
        key: PlacementKey,
        placement: &KittyImagePlacement,
        image_size: (u32, u32),
    ) -> anyhow::Result<()> {
        anyhow::ensure!(!placement.virtual_placement, "EINVAL: a virtual placement cannot have a parent");
        self.kitty_scene_refresh();
        let parent = self.kitty_parent_key(placement.parent_image_id.unwrap_or(0), placement.parent_placement_id.unwrap_or(0))
            .ok_or_else(|| anyhow::anyhow!("ENOPARENT: no matching parent placement"))?;
        let geometry = PlacementGeometry::from(placement);
        let cell = (self.pixel_width / self.screen().physical_cols, self.pixel_height / self.screen().physical_rows);
        anyhow::ensure!(geometry.layout(image_size, (cell.0 as u32, cell.1 as u32)).is_some(), "EINVAL: invalid placement geometry");
        let relative = RelativePlacement { key, parent, geometry,
            horizontal: placement.horizontal_offset.unwrap_or(0), vertical: placement.vertical_offset.unwrap_or(0) };
        let ordinary = &self.kitty_img.placements;
        let virtuals = &self.kitty_img.virtual_placements;
        self.kitty_img.relatives.insert(relative, |key| ordinary.contains_key(&key)
            || (key.placement_id <= u64::from(u32::MAX) && virtuals.contains(key.image_id, key.placement_id as u32)))
            .map_err(|err| anyhow::anyhow!("{}: invalid placement ancestry", err.code()))?;
        // A valid conversion keeps descendants referring to this identity.
        self.kitty_delete_selected_placements([key].into(), HashSet::new(), false);
        if let Some(id) = key.protocol_id() {
            self.kitty_img.virtual_placements.remove(key.image_id, Some(id));
        }
        self.kitty_img.relative_seqno = None;
        self.kitty_img.selection_revision += 1;
        Ok(())
    }

    pub(in crate::terminalstate) fn kitty_replace_placement(&mut self, image_id: u32, placement_id: u32) {
        let key = PlacementKey { image_id, placement_id: u64::from(placement_id) };
        self.kitty_delete_selected_placements([key].into(), HashSet::new(), false);
        if self.kitty_img.relatives.make_root(key) {
            self.kitty_img.selection_revision += 1;
        }
        self.kitty_img.relative_seqno = None;
    }

    fn kitty_relative_roots(&self) -> BTreeMap<PlacementKey, Anchor> {
        let wanted: BTreeSet<_> = self.kitty_img.relatives.iter().map(|p| p.parent)
            .filter(|key| self.kitty_img.relatives.get(*key).is_none()).collect();
        let virtuals: BTreeSet<_> = wanted.iter().copied().filter(|key| {
            key.placement_id <= u64::from(u32::MAX)
                && self.kitty_img.virtual_placements.contains(key.image_id, key.placement_id as u32)
        }).collect();
        let mut roots = BTreeMap::new();
        let mut first_source = BTreeMap::new();
        for alternate in [false, true] {
            let screen = self.screen_for_alt(alternate);
            screen.for_each_phys_line(|physical, line| {
                let row = screen.phys_to_stable_row_index(physical) as i64;
                if line.has_images() {
                    for cell in line.visible_cells() {
                        for image in cell.attrs().image_attachments() {
                            let Some(image_id) = image.image_id() else { continue };
                            let key = PlacementKey { image_id, placement_id: image.placement_tag() };
                            if !wanted.contains(&key) { continue; }
                            let uv = image.top_left();
                            let source = (uv.y, uv.x);
                            if first_source.get(&key).is_some_and(|old| *old <= source) { continue; }
                            let anchor = self.kitty_img.placements.get(&key).and_then(|p| p.origin)
                                .and_then(|origin| origin.anchor(cell.cell_index(), row, alternate, image))
                                .unwrap_or(Anchor { column: cell.cell_index() as i64, row, alt_screen: alternate });
                            roots.insert(key, anchor);
                            first_source.insert(key, source);
                        }
                    }
                }
                if !virtuals.is_empty() {
                    crate::kitty_placeholder::visit(line, |column, placeholder| {
                        let key = if placeholder.placement_id == 0 {
                            self.kitty_img.virtual_placements.first(placeholder.image_id)
                                .map(|p| PlacementKey { image_id: placeholder.image_id, placement_id: u64::from(p.placement_id) })
                        } else {
                            Some(PlacementKey { image_id: placeholder.image_id, placement_id: u64::from(placeholder.placement_id) })
                        };
                        let Some(key) = key.filter(|key| virtuals.contains(key)) else { return };
                        let anchor = Anchor { column: column as i64 - i64::from(placeholder.column),
                            row: row - i64::from(placeholder.row), alt_screen: alternate };
                        // The active screen's actual placeholder images supply
                        // a virtual root; the other screen retains its text.
                        if alternate != self.screen.is_alt_screen_active() { return; }
                        roots.entry(key).and_modify(|old| {
                            old.column = old.column.min(anchor.column);
                            old.row = old.row.min(anchor.row);
                        }).or_insert(anchor);
                    });
                }
            });
        }
        roots
    }

    pub(crate) fn kitty_scene_refresh(&mut self) {
        if self.kitty_img.relatives.is_empty() {
            if !self.kitty_img.relative_views.is_empty() {
                self.kitty_img.relative_views = BTreeMap::new();
                self.kitty_img.selection_revision += 1;
            }
            return;
        }
        if self.kitty_img.relative_seqno == Some(self.seqno) { return; }
        let roots = self.kitty_relative_roots();
        let missing: BTreeSet<_> = self.kitty_img.relatives.iter().map(|p| p.parent)
            .filter(|key| self.kitty_img.relatives.get(*key).is_none() && !roots.contains_key(key)
                && !(key.placement_id <= u64::from(u32::MAX)
                    && self.kitty_img.virtual_placements.contains(key.image_id, key.placement_id as u32))).collect();
        if !missing.is_empty() {
            self.kitty_img.placements.retain(|key, _| !missing.contains(key));
            self.kitty_remove_relative_dependents(missing.iter().copied());
        }
        let mut views: BTreeMap<u32, Vec<RelativeView>> = BTreeMap::new();
        let mut image = None;
        let mut image_size = (0, 0);
        for relative in self.kitty_img.relatives.iter() {
            if image != Some(relative.key.image_id) {
                image = Some(relative.key.image_id);
                image_size = self.kitty_img.id_to_data.get(&relative.key.image_id)
                    .and_then(|data| data.data().dimensions().ok()).unwrap_or((0, 0));
            }
            if let Some(anchor) = self.kitty_img.relatives.resolve(relative.key, |key| roots.get(&key).copied()) {
                let previous = self.kitty_img.relative_views.get(&relative.key.image_id).and_then(|views|
                    views.binary_search_by_key(&relative.key.placement_id, |view| view.placement_id).ok().map(|i| views[i]));
                let source_seqno = previous.filter(|view| view.anchor == anchor
                    && view.geometry == relative.geometry && view.image_size == image_size)
                    .map_or(self.seqno as u64, |view| view.source_seqno);
                views.entry(relative.key.image_id).or_default().push(RelativeView {
                    placement_id: relative.key.placement_id, source_seqno, anchor, geometry: relative.geometry, image_size,
                });
            }
        }
        if self.kitty_img.relative_views != views {
            self.kitty_img.relative_views = views;
            self.kitty_img.selection_revision += 1;
        }
        self.kitty_img.relative_seqno = Some(self.seqno);
    }

    pub(super) fn kitty_remove_relative_dependents(&mut self, roots: impl IntoIterator<Item = PlacementKey>) {
        let removed = self.kitty_img.relatives.remove(roots);
        if removed.is_empty() { return; }
        let mut release: HashSet<_> = removed.iter().map(|p| p.key.image_id).collect();
        release.retain(|id| !self.kitty_img.placements.keys().any(|p| p.image_id == *id)
            && !self.kitty_img.virtual_placements.contains_image(*id)
            && !self.kitty_img.relatives.iter().any(|p| p.key.image_id == *id));
        self.kitty_img.evict_unplaced(&release);
        self.kitty_img.relative_seqno = None;
        self.kitty_img.selection_revision += 1;
    }

    pub(super) fn snapshot_kitty_relatives(&self) -> Option<RelativeSnapshot> {
        let origins: Vec<_> = self.kitty_img.placements.iter().filter_map(|(key, p)| p.origin.map(|o| (*key, o))).collect();
        if origins.is_empty() && self.kitty_img.relatives.is_empty() { return None; }
        Some(RelativeSnapshot { origins, placements: self.kitty_img.relatives.iter().copied().collect() })
    }

    pub fn restore_kitty_relatives(&mut self, state: RelativeSnapshot) -> anyhow::Result<()> {
        anyhow::ensure!(state.origins.len() <= MAX_PLACEMENTS, "too many placement origins");
        let mut origins = BTreeMap::new();
        for (key, origin) in state.origins {
            anyhow::ensure!(self.kitty_img.id_to_data.contains_key(&key.image_id)
                && key.placement_id <= self.kitty_img.next_placement_id.max(u64::from(u32::MAX))
                && origin.valid() && origins.insert(key, origin).is_none(), "invalid or duplicate placement origin");
        }
        for p in &state.placements {
            anyhow::ensure!(self.kitty_img.id_to_data.contains_key(&p.key.image_id), "relative image is absent");
            anyhow::ensure!(!self.kitty_root_exists(p.key) && p.key.placement_id != 0
                && p.key.placement_id <= self.kitty_img.next_placement_id.max(u64::from(u32::MAX)),
                "invalid relative placement identity");
        }
        // An ordinary root can have been trimmed by the new history limit.
        // Validate its carried identity before pruning its whole subtree.
        let graph = RelativePlacements::restore(state.placements, |key| self.kitty_root_exists(key) || origins.contains_key(&key))
            .map_err(|err| anyhow::anyhow!("{}: invalid relative snapshot", err.code()))?;
        self.kitty_img.relatives = graph;
        let missing: Vec<_> = origins.keys().copied().filter(|key| !self.kitty_img.placements.contains_key(key)).collect();
        for (key, origin) in origins {
            if let Some(p) = self.kitty_img.placements.get_mut(&key) { p.origin = Some(origin); }
        }
        self.kitty_remove_relative_dependents(missing);
        self.kitty_img.relative_seqno = None;
        self.kitty_scene_refresh();
        Ok(())
    }
}
