//! Limits shared by direct Web replies, proxy relays and snapshot mailboxes.

use crate::KittyFrameSelections;
use std::collections::HashSet;
use std::mem::size_of;
use wezterm_term::kitty_animation::KittyAnimation;
use wezterm_term::kitty_virtual::{valid, VirtualPlacement, MAX_VIRTUAL_PLACEMENTS};
use wezterm_term::KittyFrameSelection;
use wezterm_term::kitty_relative::RelativeView;

pub const MAX_METADATA_BYTES: usize = 8 * 1024 * 1024;

impl KittyFrameSelections {
    /// Include spare capacity: a small logical snapshot can retain a large
    /// allocation after coalescing. This is also the mailbox's byte charge.
    pub fn retained_bytes(&self) -> anyhow::Result<usize> {
        self.selections
            .capacity()
            .checked_mul(size_of::<KittyFrameSelection>())
            .and_then(|bytes| bytes.checked_add(size_of::<Self>()))
            .and_then(|bytes| {
                self.selections.iter().try_fold(bytes, |bytes, selection| {
                    bytes
                        .checked_add(
                            selection
                                .animation
                                .frame_ends
                                .capacity()
                                .checked_mul(size_of::<u64>())?,
                        )?
                        .checked_add(
                            selection
                                .virtual_placements
                                .capacity()
                                .checked_mul(size_of::<VirtualPlacement>())?,
                        )?
                        .checked_add(selection.relative_placements.capacity().checked_mul(size_of::<RelativeView>())?)
                })
            })
            .ok_or_else(|| anyhow::anyhow!("Kitty metadata size overflow"))
    }

    /// This snapshot as a receiver will take it. A terminal can hold more
    /// placements, or bytes of them, than `validate` accepts, and a rejected
    /// snapshot leaves every picture in its pane undrawn. Spare capacity goes
    /// first, then placeholder and relative placements, then animation
    /// timelines; what is left always validates.
    pub fn fitted(mut self) -> Self {
        self.selections.shrink_to_fit();
        for selection in &mut self.selections {
            selection.animation.frame_ends.shrink_to_fit();
            selection.virtual_placements.shrink_to_fit();
            selection.relative_placements.shrink_to_fit();
        }
        if self.validate().is_ok() {
            return self;
        }
        for selection in &mut self.selections {
            selection.virtual_placements = Vec::new();
            selection.relative_placements = Vec::new();
        }
        if self.validate().is_ok() {
            return self;
        }
        for selection in &mut self.selections {
            selection.animation = KittyAnimation::new([0], 0);
        }
        if self.validate().is_err() {
            self.selections = Vec::new();
        }
        self
    }

    /// Validate before changing epochs, clock mappings, image caches or grids.
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.retained_bytes()? <= MAX_METADATA_BYTES,
            "Kitty metadata exceeds byte budget"
        );
        let mut ids = HashSet::with_capacity(self.selections.len());
        let mut placements = 0usize;
        for selection in &self.selections {
            anyhow::ensure!(ids.insert(selection.image_id), "duplicate Kitty image id");
            anyhow::ensure!(selection.animation.valid(), "invalid Kitty animation");
            anyhow::ensure!(
                valid(&selection.virtual_placements),
                "invalid Kitty virtual placements"
            );
            placements = placements
                .checked_add(selection.virtual_placements.len())
                .and_then(|n| n.checked_add(selection.relative_placements.len()))
                .ok_or_else(|| anyhow::anyhow!("Kitty placement count overflow"))?;
            anyhow::ensure!(
                placements <= MAX_VIRTUAL_PLACEMENTS,
                "Kitty metadata exceeds placement budget"
            );
            anyhow::ensure!(selection.relative_placements.iter().all(|p| p.placement_id != 0 && p.image_size.0 > 0 && p.image_size.1 > 0)
                && selection.relative_placements.windows(2).all(|p| p[0].placement_id < p[1].placement_id),
                "invalid Kitty relative placements");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot() -> KittyFrameSelections {
        KittyFrameSelections {
            pane_id: 1,
            image_epoch: 0,
            now_ms: 1,
            revision: 1,
            selections: vec![KittyFrameSelection { relative_placements: Vec::new(),
                image_id: 1,
                data_hash: [1; 32],
                data_generation: 0,
                animation: KittyAnimation::new([20, 30], 1),
                virtual_placements: vec![VirtualPlacement {
                    placement_id: 1,
                    columns: 8,
                    rows: 4,
                }],
            }],
        }
    }

    #[test]
    fn fitting_keeps_what_a_receiver_accepts() {
        assert_eq!(snapshot().fitted(), snapshot());
        // Spare capacity alone past the budget is trimmed; nothing else goes.
        let mut state = snapshot();
        state.selections[0].animation.frame_ends.reserve(MAX_METADATA_BYTES / 8);
        assert!(state.validate().is_err());
        assert_eq!(state.fitted(), snapshot());
        // Too many placements: they go, the picture and its timeline stay.
        let mut state = snapshot();
        state.selections[0].virtual_placements = (1..=MAX_VIRTUAL_PLACEMENTS as u32 + 1)
            .map(|placement_id| VirtualPlacement { placement_id, columns: 1, rows: 1 })
            .collect();
        assert!(state.validate().is_err());
        let fitted = state.fitted();
        fitted.validate().unwrap();
        assert!(fitted.selections[0].virtual_placements.is_empty());
        assert_eq!(fitted.selections[0].animation, snapshot().selections[0].animation);
        // What no trimming repairs is sent as no pictures at all.
        let mut state = snapshot();
        state.selections.push(state.selections[0].clone());
        assert!(state.fitted().selections.is_empty());
    }

    #[test]
    fn valid_metadata_accepts_static_animation_and_virtual_grids() {
        let mut state = snapshot();
        state.validate().unwrap();
        state.selections[0].animation = KittyAnimation::new([0], 0);
        state.selections[0].virtual_placements[0].columns = 0;
        state.selections[0].virtual_placements[0].rows = u32::MAX;
        state.validate().unwrap();
        state.selections.clear();
        state.validate().unwrap();
    }

    #[test]
    fn rejects_ambiguous_ids_grids_and_invalid_timelines() {
        let mut state = snapshot();
        state.selections.push(state.selections[0].clone());
        assert!(state.validate().is_err());
        let mut state = snapshot();
        let placement = state.selections[0].virtual_placements[0];
        state.selections[0].virtual_placements.push(placement);
        assert!(state.validate().is_err());
        state.selections[0].virtual_placements[1].placement_id = 0;
        assert!(state.validate().is_err());
        let mut state = snapshot();
        state.selections[0].animation.frame = 2;
        assert!(state.validate().is_err());
    }

    #[test]
    fn byte_budget_counts_spare_capacity_in_all_vectors() {
        for vector in 0..4 {
            let mut state = snapshot();
            match vector {
                0 => {
                    state.selections = Vec::with_capacity(
                        MAX_METADATA_BYTES / size_of::<KittyFrameSelection>() + 1,
                    )
                }
                1 => {
                    state.selections[0].animation.frame_ends =
                        Vec::with_capacity(MAX_METADATA_BYTES / size_of::<u64>())
                }
                2 => {
                    state.selections[0].virtual_placements =
                        Vec::with_capacity(MAX_METADATA_BYTES / size_of::<VirtualPlacement>() + 1)
                }
                _ => {
                    state.selections[0].relative_placements =
                        Vec::with_capacity(MAX_METADATA_BYTES / size_of::<RelativeView>() + 1)
                }
            }
            assert!(state.retained_bytes().unwrap() > MAX_METADATA_BYTES);
            assert!(state.validate().is_err());
        }
    }

    #[test]
    fn relative_metadata_rejects_ambiguous_identity_empty_images_and_mixed_overflow() {
        use wezterm_term::kitty_relative::Anchor;
        let view = RelativeView { placement_id: 1, source_seqno: 10,
            anchor: Anchor { column: -3, row: -1, alt_screen: false },
            geometry: Default::default(), image_size: (20, 10) };
        let mut state = snapshot();
        state.selections[0].relative_placements.push(view);
        state.validate().unwrap();
        for bad in [RelativeView { placement_id: 0, ..view }, RelativeView { image_size: (0, 10), ..view }] {
            state.selections[0].relative_placements[0] = bad;
            assert!(state.validate().is_err());
        }
        state.selections[0].relative_placements = vec![view, view];
        assert!(state.validate().is_err());
        state.selections[0].relative_placements = vec![RelativeView { placement_id: 2, ..view }, view];
        assert!(state.validate().is_err());
        state.selections[0].relative_placements = vec![view];
        state.selections[0].virtual_placements = (0..MAX_VIRTUAL_PLACEMENTS as u32).map(|placement_id|
            VirtualPlacement { placement_id, columns: 1, rows: 1 }).collect();
        assert!(state.retained_bytes().unwrap() < MAX_METADATA_BYTES);
        assert!(state.validate().is_err());
        state.selections[0].virtual_placements.pop();
        state.validate().unwrap();
    }

    #[test]
    fn placement_budget_is_global_across_images() {
        let mut state = snapshot();
        let grids: Vec<_> = (0..MAX_VIRTUAL_PLACEMENTS as u32 / 2 + 1)
            .map(|placement_id| VirtualPlacement {
                placement_id,
                columns: 1,
                rows: 1,
            })
            .collect();
        state.selections[0].virtual_placements = grids;
        let mut other = state.selections[0].clone();
        other.image_id = 2;
        state.selections.push(other);
        assert!(state.retained_bytes().unwrap() < MAX_METADATA_BYTES);
        assert!(state.validate().is_err());
        state.selections[0].virtual_placements.pop();
        state.selections[1].virtual_placements.pop();
        state.validate().unwrap();
    }
}
