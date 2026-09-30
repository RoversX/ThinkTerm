//! Virtual placements describe placeholder grids without attaching desktop cells.

use std::collections::BTreeMap;

pub const MAX_VIRTUAL_PLACEMENTS: usize = 65536;

#[cfg_attr(feature = "use_serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VirtualPlacement {
    pub placement_id: u32,
    /// Zero uses the natural image size in that dimension.
    pub columns: u32,
    pub rows: u32,
}

/// Process-independent identity for the optional handoff companion. Pixel data
/// stays in the terminal snapshot's image table; it is not duplicated here.
#[cfg_attr(feature = "use_serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VirtualImage {
    pub image_id: u32,
    pub data_hash: [u8; 32],
    pub placements: Vec<VirtualPlacement>,
}

pub fn find(placements: &[VirtualPlacement], id: u32) -> Option<VirtualPlacement> {
    if id == 0 {
        placements.first().copied()
    } else {
        placements.binary_search_by_key(&id, |p| p.placement_id)
            .ok().map(|index| placements[index])
    }
}

pub fn valid(placements: &[VirtualPlacement]) -> bool {
    placements.len() <= MAX_VIRTUAL_PLACEMENTS
        && placements.windows(2).all(|pair| pair[0].placement_id < pair[1].placement_id)
}

#[derive(Debug, Default)]
pub(crate) struct VirtualPlacements {
    entries: BTreeMap<(u32, u32), VirtualPlacement>,
}

impl VirtualPlacements {
    pub fn contains(&self, image_id: u32, placement_id: u32) -> bool {
        self.entries.contains_key(&(image_id, placement_id))
    }

    pub fn first(&self, image_id: u32) -> Option<VirtualPlacement> {
        self.entries.range((image_id, 0)..=(image_id, u32::MAX)).next().map(|(_, p)| *p)
    }

    pub fn insert(&mut self, image_id: u32, placement: VirtualPlacement) -> bool {
        let key = (image_id, placement.placement_id);
        if self.entries.get(&key) == Some(&placement) {
            return false;
        }
        if self.entries.len() == MAX_VIRTUAL_PLACEMENTS && !self.entries.contains_key(&key) {
            log::warn!("Kitty virtual placement budget exhausted");
            return false;
        }
        self.entries.insert(key, placement);
        true
    }

    pub fn for_image(&self, image_id: u32) -> Vec<VirtualPlacement> {
        self.entries
            .range((image_id, 0)..=(image_id, u32::MAX))
            .map(|(_, placement)| *placement)
            .collect()
    }

    pub fn contains_image(&self, image_id: u32) -> bool {
        self.entries.range((image_id, 0)..=(image_id, u32::MAX)).next().is_some()
    }

    pub fn remove(&mut self, image_id: u32, placement_id: Option<u32>) -> bool {
        if let Some(id) = placement_id.filter(|id| *id != 0) {
            return self.entries.remove(&(image_id, id)).is_some();
        }
        let before = self.entries.len();
        // Remove only this image's range, without scanning unrelated images
        // or allocating a temporary list of all its placement keys.
        while let Some(key) = self.entries
            .range((image_id, 0)..=(image_id, u32::MAX))
            .next()
            .map(|(key, _)| *key)
        {
            self.entries.remove(&key);
        }
        before != self.entries.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn placement(id: u32) -> VirtualPlacement {
        VirtualPlacement { placement_id: id, columns: 10, rows: 5 }
    }

    #[test]
    fn replace_and_remove_leave_other_images_and_placements_intact() {
        let mut store = VirtualPlacements::default();
        assert!(store.insert(1, placement(2)));
        assert!(!store.insert(1, placement(2)));
        store.insert(1, placement(3));
        store.insert(2, placement(2));
        let resized = VirtualPlacement { columns: 20, ..placement(2) };
        assert!(store.insert(1, resized));
        assert_eq!(store.for_image(1), [resized, placement(3)]);
        assert!(store.remove(1, Some(2)));
        assert_eq!(store.for_image(1), [placement(3)]);
        assert!(store.remove(1, Some(0)));
        assert!(store.for_image(1).is_empty());
        assert_eq!(store.for_image(2), [placement(2)]);
        assert!(!store.remove(1, None));
        assert!(store.remove(2, None));
        assert!(store.entries.is_empty());
    }

    #[test]
    fn global_limit_allows_replacement_and_recovers_after_deletion() {
        let mut store = VirtualPlacements::default();
        for id in 0..MAX_VIRTUAL_PLACEMENTS as u32 {
            assert!(store.insert(id, placement(0)));
        }
        assert!(!store.insert(u32::MAX, placement(0)));
        assert!(store.insert(0, VirtualPlacement { rows: 7, ..placement(0) }));
        assert!(store.remove(0, None));
        assert!(store.insert(u32::MAX, placement(0)));
        assert_eq!(store.entries.len(), MAX_VIRTUAL_PLACEMENTS);
    }
}
