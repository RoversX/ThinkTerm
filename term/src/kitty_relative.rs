//! Bounded parent relationships for Kitty relative placements.

use std::collections::{BTreeMap, BTreeSet};
use std::convert::TryFrom;
use wezterm_escape_parser::apc::KittyImagePlacement;
use wezterm_cell::image::{ImageCell, TextureCoordinate};

pub const MAX_RELATIVE_PLACEMENTS: usize = 65536;
pub const MAX_PARENT_DEPTH: usize = 64;

/// A stable placement identity. The wider placement key also permits distinct
/// anonymous placements; their wire p=0 must be resolved before entering here.
#[cfg_attr(feature = "use_serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PlacementKey {
    pub image_id: u32,
    pub placement_id: u64,
}

impl PlacementKey {
    pub fn protocol_id(self) -> Option<u32> {
        u32::try_from(self.placement_id).ok().filter(|id| *id != 0)
    }
}

/// Recover a placement's origin from any surviving cell after scrolling.
#[cfg_attr(feature = "use_serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellOrigin {
    pub source: TextureCoordinate,
    pub step: TextureCoordinate,
}

impl CellOrigin {
    pub fn anchor(&self, column: usize, row: i64, alternate: bool, cell: &ImageCell) -> Option<Anchor> {
        let source = cell.top_left();
        let x = self.step.x.into_inner();
        let y = self.step.y.into_inner();
        if !self.valid() { return None; }
        let dx = ((source.x.into_inner() - self.source.x.into_inner()) / x).round() as i64;
        let dy = ((source.y.into_inner() - self.source.y.into_inner()) / y).round() as i64;
        Some(Anchor { column: (column as i64).checked_sub(dx)?, row: row.checked_sub(dy)?, alt_screen: alternate })
    }

    pub fn valid(&self) -> bool {
        [self.source.x, self.source.y, self.step.x, self.step.y].iter().all(|v| v.into_inner().is_finite())
            && self.step.x.into_inner() > 0.0 && self.step.y.into_inner() > 0.0
    }
}

#[cfg_attr(feature = "use_serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RelativeView {
    pub placement_id: u64,
    /// The terminal update that established this position.
    pub source_seqno: u64,
    pub anchor: Anchor,
    pub geometry: PlacementGeometry,
    pub image_size: (u32, u32),
}

#[cfg_attr(feature = "use_serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelativeSnapshot {
    pub origins: Vec<(PlacementKey, CellOrigin)>,
    pub placements: Vec<RelativePlacement>,
}

#[cfg_attr(feature = "use_serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PlacementGeometry {
    pub x: u32,
    pub y: u32,
    /// Zero uses the remaining source extent or the natural cell extent.
    pub width: u32,
    pub height: u32,
    pub columns: u32,
    pub rows: u32,
    pub x_offset: u32,
    pub y_offset: u32,
    pub z_index: i32,
}

impl From<&KittyImagePlacement> for PlacementGeometry {
    fn from(placement: &KittyImagePlacement) -> Self {
        Self {
            x: placement.x.unwrap_or(0),
            y: placement.y.unwrap_or(0),
            width: placement.w.unwrap_or(0),
            height: placement.h.unwrap_or(0),
            columns: placement.columns.unwrap_or(0),
            rows: placement.rows.unwrap_or(0),
            x_offset: placement.x_offset.unwrap_or(0),
            y_offset: placement.y_offset.unwrap_or(0),
            z_index: placement.z_index.unwrap_or(0),
        }
    }
}

/// Source UVs and destination pixel edges relative to the anchor cell. Keep
/// double precision until clipping; large cell counts must not allocate cells.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PlacementLayout {
    pub rect: [f64; 4],
    pub uv: [f64; 4],
}

impl PlacementGeometry {
    pub fn layout(&self, image: (u32, u32), cell: (u32, u32)) -> Option<PlacementLayout> {
        if image.0 == 0 || image.1 == 0 || cell.0 == 0 || cell.1 == 0 {
            return None;
        }
        let remaining_width = image.0.saturating_sub(self.x);
        let remaining_height = image.1.saturating_sub(self.y);
        let width = if self.width == 0 {
            remaining_width
        } else {
            self.width.min(remaining_width)
        };
        let height = if self.height == 0 {
            remaining_height
        } else {
            self.height.min(remaining_height)
        };
        if width == 0 || height == 0 {
            return None;
        }
        let left = f64::from(self.x_offset.min(cell.0 - 1));
        let top = f64::from(self.y_offset.min(cell.1 - 1));
        let aspect = f64::from(width) / f64::from(height);
        let (right, bottom) = if self.rows > 0 {
            let bottom = f64::from(self.rows) * f64::from(cell.1);
            let right = if self.columns > 0 {
                f64::from(self.columns) * f64::from(cell.0)
            } else {
                left + (bottom - top) * aspect
            };
            (right, bottom)
        } else {
            let right = if self.columns > 0 {
                f64::from(self.columns) * f64::from(cell.0)
            } else {
                left + f64::from(width)
            };
            (right, top + (right - left) / aspect)
        };
        Some(PlacementLayout {
            rect: [left, top, right, bottom],
            uv: [
                f64::from(self.x) / f64::from(image.0),
                f64::from(self.y) / f64::from(image.1),
                (f64::from(self.x) + f64::from(width)) / f64::from(image.0),
                (f64::from(self.y) + f64::from(height)) / f64::from(image.1),
            ],
        })
    }
}

#[cfg_attr(feature = "use_serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RelativePlacement {
    pub key: PlacementKey,
    pub parent: PlacementKey,
    pub horizontal: i32,
    pub vertical: i32,
    pub geometry: PlacementGeometry,
}

#[cfg_attr(feature = "use_serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Anchor {
    pub column: i64,
    /// Stable row, kept at fixed width when transferred to a 32-bit browser.
    pub row: i64,
    pub alt_screen: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlacementError {
    MissingParent,
    Cycle,
    TooDeep,
    Full,
    Duplicate,
}

impl PlacementError {
    pub fn code(self) -> &'static str {
        match self {
            Self::MissingParent => "ENOPARENT",
            Self::Cycle => "ECYCLE",
            Self::TooDeep => "ETOODEEP",
            Self::Full => "ENOSPC",
            Self::Duplicate => "EINVAL",
        }
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct RelativePlacements {
    entries: BTreeMap<PlacementKey, RelativePlacement>,
    children: BTreeSet<(PlacementKey, PlacementKey)>,
}

impl RelativePlacements {
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn get(&self, key: PlacementKey) -> Option<&RelativePlacement> {
        self.entries.get(&key)
    }

    pub fn iter(&self) -> impl Iterator<Item = &RelativePlacement> {
        self.entries.values()
    }

    /// Turning a relative into a root preserves its descendants.
    pub fn make_root(&mut self, key: PlacementKey) -> bool {
        if let Some(old) = self.entries.remove(&key) {
            self.unlink(old.parent, key);
            true
        } else {
            false
        }
    }

    fn children_of(&self, key: PlacementKey) -> impl Iterator<Item = PlacementKey> + '_ {
        let first = PlacementKey {
            image_id: 0,
            placement_id: 0,
        };
        let last = PlacementKey {
            image_id: u32::MAX,
            placement_id: u64::MAX,
        };
        self.children
            .range((key, first)..=(key, last))
            .map(|(_, child)| *child)
    }

    /// Validate before changing either index. A replacement also checks its
    /// descendants: moving a short subtree deeper can exceed the depth limit.
    pub fn insert(
        &mut self,
        placement: RelativePlacement,
        parent_exists: impl Fn(PlacementKey) -> bool,
    ) -> Result<bool, PlacementError> {
        let old = self.entries.get(&placement.key).copied();
        if old.is_none() && self.entries.len() == MAX_RELATIVE_PLACEMENTS {
            return Err(PlacementError::Full);
        }
        let depth = self.parent_depth(placement.key, placement.parent, &parent_exists)?;
        if old.is_none_or(|old| old.parent != placement.parent) {
            let mut pending = vec![(placement.key, depth)];
            while let Some((key, depth)) = pending.pop() {
                for child in self.children_of(key) {
                    if depth == MAX_PARENT_DEPTH {
                        return Err(PlacementError::TooDeep);
                    }
                    pending.push((child, depth + 1));
                }
            }
        }
        if old == Some(placement) {
            return Ok(false);
        }
        if let Some(old) = old.filter(|old| old.parent != placement.parent) {
            self.unlink(old.parent, old.key);
        }
        self.children.insert((placement.parent, placement.key));
        self.entries.insert(placement.key, placement);
        Ok(true)
    }

    fn parent_depth(
        &self,
        key: PlacementKey,
        mut parent: PlacementKey,
        parent_exists: &impl Fn(PlacementKey) -> bool,
    ) -> Result<usize, PlacementError> {
        for depth in 1..=MAX_PARENT_DEPTH + 1 {
            if parent == key {
                return Err(PlacementError::Cycle);
            }
            if depth > MAX_PARENT_DEPTH {
                return Err(PlacementError::TooDeep);
            }
            match self.entries.get(&parent) {
                Some(placement) => parent = placement.parent,
                None if parent_exists(parent) => return Ok(depth),
                None => return Err(PlacementError::MissingParent),
            }
        }
        Err(PlacementError::TooDeep)
    }

    fn unlink(&mut self, parent: PlacementKey, key: PlacementKey) {
        self.children.remove(&(parent, key));
    }

    /// Roots may be ordinary or virtual placements outside this graph. Return
    /// removed relatives so the image store can release now-unreferenced data.
    pub fn remove(
        &mut self,
        roots: impl IntoIterator<Item = PlacementKey>,
    ) -> Vec<RelativePlacement> {
        let mut removed = Vec::new();
        let mut pending: Vec<_> = roots.into_iter().collect();
        while let Some(key) = pending.pop() {
            loop {
                let child = self.children_of(key).next();
                let Some(child) = child else { break };
                self.unlink(key, child);
                pending.push(child);
            }
            if let Some(placement) = self.entries.remove(&key) {
                self.unlink(placement.parent, key);
                removed.push(placement);
            }
        }
        removed
    }

    /// Resolve at the current terminal geometry, never at creation time. The
    /// root provider handles ordinary cells and Unicode placeholder anchors.
    pub fn resolve(
        &self,
        mut key: PlacementKey,
        root: impl Fn(PlacementKey) -> Option<Anchor>,
    ) -> Option<Anchor> {
        let (mut horizontal, mut vertical) = (0i64, 0i64);
        for _ in 0..=MAX_PARENT_DEPTH {
            match self.entries.get(&key) {
                Some(placement) => {
                    horizontal += i64::from(placement.horizontal);
                    vertical += i64::from(placement.vertical);
                    key = placement.parent;
                }
                None => {
                    let anchor = root(key)?;
                    return Some(Anchor {
                        column: anchor.column.checked_add(horizontal)?,
                        row: anchor.row.checked_add(vertical)?,
                        alt_screen: anchor.alt_screen,
                    });
                }
            }
        }
        None
    }

    pub fn restore(
        placements: Vec<RelativePlacement>,
        parent_exists: impl Fn(PlacementKey) -> bool,
    ) -> Result<Self, PlacementError> {
        if placements.len() > MAX_RELATIVE_PLACEMENTS {
            return Err(PlacementError::Full);
        }
        let mut graph = Self::default();
        for placement in placements {
            if graph.entries.insert(placement.key, placement).is_some() {
                return Err(PlacementError::Duplicate);
            }
            graph.children.insert((placement.parent, placement.key));
        }
        for placement in graph.entries.values() {
            graph.parent_depth(placement.key, placement.parent, &parent_exists)?;
        }
        Ok(graph)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(image_id: u32) -> PlacementKey {
        PlacementKey {
            image_id,
            placement_id: 1,
        }
    }

    fn placement(image_id: u32, parent: u32) -> RelativePlacement {
        RelativePlacement {
            key: key(image_id),
            parent: key(parent),
            horizontal: -2,
            vertical: 3,
            geometry: PlacementGeometry {
                columns: 4,
                rows: 2,
                ..Default::default()
            },
        }
    }

    #[test]
    fn geometry_preserves_aspect_only_when_one_or_both_cell_extents_are_omitted() {
        for (columns, rows, rect) in [
            (0, 0, [0.0, 0.0, 100.0, 50.0]),
            (4, 0, [0.0, 0.0, 40.0, 20.0]),
            (0, 4, [0.0, 0.0, 160.0, 80.0]),
            (4, 4, [0.0, 0.0, 40.0, 80.0]),
        ] {
            let layout = PlacementGeometry {
                columns,
                rows,
                ..Default::default()
            }
            .layout((100, 50), (10, 20))
            .unwrap();
            assert_eq!(
                layout,
                PlacementLayout {
                    rect,
                    uv: [0.0, 0.0, 1.0, 1.0]
                }
            );
        }
        for (columns, rows, rect) in [
            (4, 0, [3.0, 5.0, 40.0, 23.5]),
            (0, 4, [3.0, 5.0, 153.0, 80.0]),
            (4, 4, [3.0, 5.0, 40.0, 80.0]),
        ] {
            let layout = PlacementGeometry {
                columns,
                rows,
                x_offset: 3,
                y_offset: 5,
                ..Default::default()
            }
            .layout((100, 50), (10, 20))
            .unwrap();
            assert_eq!(layout.rect, rect);
        }
    }

    #[test]
    fn geometry_clips_the_source_and_handles_empty_and_extreme_inputs() {
        for (width, height) in [(0, 0), (30, 20)] {
            let layout = PlacementGeometry {
                x: 90,
                y: 40,
                width,
                height,
                ..Default::default()
            }
            .layout((100, 50), (10, 20))
            .unwrap();
            assert_eq!(
                layout,
                PlacementLayout {
                    rect: [0.0, 0.0, 10.0, 10.0],
                    uv: [0.9, 0.8, 1.0, 1.0]
                }
            );
        }
        assert!(PlacementGeometry {
            x: 100,
            ..Default::default()
        }
        .layout((100, 50), (10, 20))
        .is_none());
        for (image, cell) in [
            ((0, 1), (10, 20)),
            ((1, 0), (10, 20)),
            ((1, 1), (0, 20)),
            ((1, 1), (10, 0)),
        ] {
            assert!(PlacementGeometry::default().layout(image, cell).is_none());
        }
        let layout = PlacementGeometry {
            columns: u32::MAX,
            rows: u32::MAX,
            x_offset: u32::MAX,
            y_offset: u32::MAX,
            ..Default::default()
        }
        .layout((u32::MAX, u32::MAX), (10, 20))
        .unwrap();
        assert_eq!(&layout.rect[..2], &[9.0, 19.0]);
        assert!(layout.rect.iter().all(|value| value.is_finite()));
        assert_eq!(layout.rect[2], f64::from(u32::MAX) * 10.0);
    }

    #[test]
    fn relationships_follow_current_roots_and_preserve_signed_offsets() {
        let mut graph = RelativePlacements::default();
        graph
            .insert(placement(2, 1), |key| key.image_id == 1)
            .unwrap();
        graph
            .insert(placement(3, 2), |key| key.image_id == 1)
            .unwrap();
        for (row, column, alt_screen) in [(0, 0, false), (-100, 20, true)] {
            let root = |key: PlacementKey| {
                (key.image_id == 1).then_some(Anchor {
                    row,
                    column,
                    alt_screen,
                })
            };
            assert_eq!(
                graph.resolve(key(3), root),
                Some(Anchor {
                    column: column - 4,
                    row: row + 6,
                    alt_screen,
                })
            );
        }
        assert_eq!(graph.resolve(key(3), |_| None), None);
        assert_eq!(
            graph.resolve(key(3), |_| Some(Anchor {
                column: i64::MIN,
                row: 0,
                alt_screen: false
            })),
            None
        );
    }

    #[test]
    fn invalid_replacements_leave_the_graph_and_children_unchanged() {
        let mut graph = RelativePlacements::default();
        let root = |key: PlacementKey| key.image_id == 1;
        graph.insert(placement(2, 1), root).unwrap();
        graph.insert(placement(3, 2), root).unwrap();
        let snapshot: Vec<_> = graph.iter().copied().collect();
        for (replacement, error) in [
            (placement(2, 2), PlacementError::Cycle),
            (placement(2, 3), PlacementError::Cycle),
            (placement(2, 99), PlacementError::MissingParent),
        ] {
            assert_eq!(graph.insert(replacement, root), Err(error));
            assert_eq!(
                graph,
                RelativePlacements::restore(snapshot.clone(), root).unwrap()
            );
        }
        assert_eq!(graph.insert(placement(2, 1), root), Ok(false));
    }

    #[test]
    fn deleting_a_root_removes_only_its_descendants_and_releases_indexes() {
        let mut graph = RelativePlacements::default();
        for (child, parent) in [(2, 1), (3, 2), (4, 1), (6, 5)] {
            graph
                .insert(placement(child, parent), |key| {
                    matches!(key.image_id, 1 | 5)
                })
                .unwrap();
        }
        // Reparenting removes the old reverse edge.
        graph
            .insert(placement(4, 5), |key| matches!(key.image_id, 1 | 5))
            .unwrap();
        let removed: BTreeSet<_> = graph
            .remove([key(1), key(2), key(1)])
            .into_iter()
            .map(|p| p.key.image_id)
            .collect();
        assert_eq!(removed, [2, 3].into());
        assert_eq!(
            graph.iter().map(|p| p.key.image_id).collect::<Vec<_>>(),
            [4, 6]
        );
        graph.remove([key(5)]);
        assert_eq!(graph, RelativePlacements::default());
    }

    #[test]
    fn deep_chains_and_reparented_subtrees_share_the_same_depth_bound() {
        let mut graph = RelativePlacements::default();
        let root = |key: PlacementKey| key.image_id == 1;
        for child in 2..=MAX_PARENT_DEPTH as u32 + 1 {
            graph.insert(placement(child, child - 1), root).unwrap();
        }
        assert!(MAX_PARENT_DEPTH >= 8);
        let snapshot: Vec<_> = graph.iter().copied().collect();
        assert_eq!(
            graph.insert(placement(1, MAX_PARENT_DEPTH as u32 + 1), root),
            Err(PlacementError::Cycle)
        );
        assert_eq!(
            graph.insert(placement(100, MAX_PARENT_DEPTH as u32 + 1), root),
            Err(PlacementError::TooDeep)
        );
        // A node itself fits at depth64, but its child would not.
        graph.insert(placement(101, 1), root).unwrap();
        graph.insert(placement(102, 101), root).unwrap();
        assert_eq!(
            graph.insert(placement(101, MAX_PARENT_DEPTH as u32), root),
            Err(PlacementError::TooDeep)
        );
        assert_eq!(graph.get(key(101)), Some(&placement(101, 1)));
        let mut reversed = snapshot.clone();
        reversed.reverse();
        assert_eq!(
            RelativePlacements::restore(reversed, root)
                .unwrap()
                .iter()
                .copied()
                .collect::<Vec<_>>(),
            snapshot
        );
    }

    #[test]
    fn restore_rejects_duplicate_missing_and_cyclic_parents() {
        for placements in [
            vec![placement(2, 1), placement(2, 1)],
            vec![placement(2, 99)],
            vec![placement(2, 3), placement(3, 2)],
        ] {
            assert!(RelativePlacements::restore(placements, |key| key.image_id == 1).is_err());
        }
    }

    #[test]
    fn anonymous_keys_and_maximum_ids_keep_distinct_parent_edges() {
        let root = PlacementKey {
            image_id: u32::MAX,
            placement_id: u64::MAX,
        };
        let first = PlacementKey {
            image_id: 7,
            placement_id: u64::from(u32::MAX) + 1,
        };
        let second = PlacementKey {
            image_id: 7,
            placement_id: first.placement_id + 1,
        };
        let mut graph = RelativePlacements::default();
        let a = RelativePlacement {
            key: first,
            parent: root,
            ..placement(7, 1)
        };
        let b = RelativePlacement {
            key: second,
            parent: first,
            ..a
        };
        graph.insert(a, |key| key == root).unwrap();
        graph.insert(b, |key| key == root).unwrap();
        assert_eq!(graph.remove([first]), [a, b]);
        assert_eq!(graph, RelativePlacements::default());
    }

    #[test]
    fn indexed_mutations_match_an_unindexed_parent_walk() {
        let mut graph = RelativePlacements::default();
        let mut reference = BTreeMap::<PlacementKey, RelativePlacement>::new();
        let mut random = 17u32;
        for step in 0..1000 {
            random = random.wrapping_mul(1664525).wrapping_add(1013904223);
            let child = key(3 + random % 40);
            let parent = key((random >> 12) % 43);
            if step % 3 == 0 {
                let root = if step % 9 == 0 { key(1) } else { child };
                // Walk each candidate's ancestors; no reverse-child index.
                let doomed: Vec<_> = reference
                    .keys()
                    .copied()
                    .filter(|key| {
                        let mut current = *key;
                        loop {
                            if current == root {
                                return true;
                            }
                            match reference.get(&current) {
                                Some(placement) => current = placement.parent,
                                None => return false,
                            }
                        }
                    })
                    .collect();
                for key in doomed {
                    reference.remove(&key);
                }
                graph.remove([root]);
            } else {
                let candidate = RelativePlacement {
                    key: child,
                    parent,
                    ..placement(3, 1)
                };
                let mut current = parent;
                let mut visited = BTreeSet::from([child]);
                let error = loop {
                    if !visited.insert(current) {
                        break Some(PlacementError::Cycle);
                    }
                    match reference.get(&current) {
                        Some(placement) => current = placement.parent,
                        None if matches!(current.image_id, 1 | 2) => break None,
                        None => break Some(PlacementError::MissingParent),
                    }
                };
                let result = graph.insert(candidate, |key| matches!(key.image_id, 1 | 2));
                match error {
                    Some(error) => assert_eq!(result, Err(error)),
                    None => {
                        assert_eq!(result, Ok(reference.get(&child) != Some(&candidate)));
                        reference.insert(child, candidate);
                    }
                }
            }
            assert_eq!(graph.entries, reference);
            let expected_edges: BTreeSet<_> =
                reference.values().map(|p| (p.parent, p.key)).collect();
            assert_eq!(graph.children, expected_edges);
        }
    }

    #[test]
    fn placement_budget_allows_replacement_and_is_reusable_after_removal() {
        let mut graph = RelativePlacements::default();
        for id in 2..MAX_RELATIVE_PLACEMENTS as u32 + 2 {
            graph
                .insert(placement(id, 1), |key| key.image_id == 1)
                .unwrap();
        }
        assert_eq!(graph.len(), MAX_RELATIVE_PLACEMENTS);
        assert_eq!(
            graph.insert(placement(u32::MAX, 1), |_| true),
            Err(PlacementError::Full)
        );
        let mut changed = placement(2, 1);
        changed.horizontal = i32::MIN;
        graph.insert(changed, |_| true).unwrap();
        graph.remove([key(2)]);
        graph.insert(placement(u32::MAX, 1), |_| true).unwrap();
        graph.remove([key(1)]);
        assert!(graph.is_empty());
        assert!(graph.children.is_empty());
    }
}
