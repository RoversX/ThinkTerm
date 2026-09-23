use crate::line::CellRef;
use core::convert::TryInto;
use core::num::NonZeroU8;
use finl_unicode::grapheme_clusters::Graphemes;
use fixedbitset::FixedBitSet;
#[cfg(feature = "use_serde")]
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use wezterm_cell::{Cell, CellAttributes};

extern crate alloc;
use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

#[cfg_attr(feature = "use_serde", derive(Serialize, Deserialize))]
#[derive(Debug, Clone, PartialEq)]
struct Cluster {
    cell_width: u16,
    attrs: CellAttributes,
}

/// Stores line data as a contiguous string and a series of
/// clusters of attribute data describing attributed ranges
/// within the line
#[cfg_attr(feature = "use_serde", derive(Serialize))]
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ClusteredLine {
    pub text: String,
    #[cfg_attr(
        feature = "use_serde",
        serde(
            serialize_with = "serialize_bitset"
        )
    )]
    is_double_wide: Option<Box<FixedBitSet>>,
    clusters: Vec<Cluster>,
    /// Length, measured in cells
    len: u32,
    last_cell_width: Option<NonZeroU8>,
}

#[cfg(feature = "use_serde")]
impl<'de> Deserialize<'de> for ClusteredLine {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // Keep the existing field order and wire representation, but defer
        // sparse-bitset expansion until its indices can be checked against
        // the text. Each UTF-8 byte can contribute at most two cell columns.
        #[derive(Deserialize)]
        struct Wire {
            text: String,
            is_double_wide: Vec<usize>,
            clusters: Vec<Cluster>,
            len: u32,
            last_cell_width: Option<NonZeroU8>,
        }
        let wire = Wire::deserialize(deserializer)?;
        let is_double_wide = if let Some(&max_idx) = wire.is_double_wide.iter().max() {
            if max_idx >= wire.text.len().saturating_mul(2) {
                return Err(serde::de::Error::custom("wide-cell index exceeds line text"));
            }
            let mut bits = FixedBitSet::with_capacity(max_idx + 1);
            for idx in wire.is_double_wide {
                bits.set(idx, true);
            }
            Some(Box::new(bits))
        } else {
            None
        };
        Ok(Self {
            text: wire.text, is_double_wide, clusters: wire.clusters,
            len: wire.len, last_cell_width: wire.last_cell_width,
        })
    }
}

/// Serialize the bitset as a vector of the indices of just the 1 bits;
/// the thesis is that most of the cells on a given line are single width.
/// That may not be strictly true for users that heavily use asian scripts,
/// but we'll start with this and see if we need to improve it.
#[cfg(feature = "use_serde")]
fn serialize_bitset<S>(value: &Option<Box<FixedBitSet>>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    let mut wide_indices: Vec<usize> = vec![];
    if let Some(bits) = value {
        for idx in bits.ones() {
            wide_indices.push(idx);
        }
    }
    wide_indices.serialize(serializer)
}

impl ClusteredLine {
    /// Reuse only small buffers when a discarded screen row becomes blank.
    /// Drop all attributes and wide-cell metadata, including image/link refs.
    pub(crate) fn clear_for_scrolling(&mut self, max_text_capacity: usize) -> bool {
        if self.text.capacity() > max_text_capacity || self.clusters.capacity() > 4 {
            return false;
        }
        self.text.clear();
        self.clusters.clear();
        self.is_double_wide = None;
        self.len = 0;
        self.last_cell_width = None;
        true
    }

    pub fn new() -> Self {
        Self {
            text: String::with_capacity(80),
            is_double_wide: None,
            clusters: vec![],
            len: 0,
            last_cell_width: None,
        }
    }

    /// Whether any cluster's attributes satisfy `pred`. A cell cannot carry
    /// an attribute its cluster does not, so this answers per-cell questions
    /// without expanding the line.
    pub(crate) fn any_cluster_attrs(&self, pred: impl FnMut(&CellAttributes) -> bool) -> bool {
        let mut pred = pred;
        self.clusters.iter().any(|cluster| pred(&cluster.attrs))
    }

    pub fn to_cell_vec(&self) -> Vec<Cell> {
        // This vec becomes the line's retained storage, so doubling growth
        // would keep up to 60% slack alive in scrollback. len() is a width
        // sum, which zero-width cells can overshoot; the shrink is a no-op
        // in the exact case and trims the rare doubled buffer otherwise.
        // len() is a sum of widths that arrive off the wire; reserve for a
        // plausible line, and let a wider one grow as it is filled.
        let mut cells = Vec::with_capacity(self.len().min(4096));

        for c in self.iter() {
            cells.push(c.as_cell());
            for _ in 1..c.width() {
                cells.push(Cell::blank_with_attrs(c.attrs().clone()));
            }
        }

        cells.shrink_to_fit();
        cells
    }

    pub fn from_cell_vec<'a>(hint: usize, iter: impl Iterator<Item = CellRef<'a>>) -> Self {
        let mut last_cluster: Option<Cluster> = None;
        // Runs once for every line that scrolls off: size the text up front
        // and touch the bitset only if a wide cell actually appears, instead
        // of growing one realloc at a time and allocating a bitset that the
        // typical all-narrow line immediately throws away.
        let mut is_double_wide: Option<Box<FixedBitSet>> = None;
        let mut text = String::with_capacity(hint);
        let mut clusters = vec![];
        let mut len = 0;
        let mut last_cell_width = None;

        for cell in iter {
            len += cell.width();
            last_cell_width = NonZeroU8::new(1);

            if cell.width() > 1 {
                is_double_wide
                    .get_or_insert_with(|| Box::new(FixedBitSet::with_capacity(hint)))
                    .set(cell.cell_index(), true);
            }

            text.push_str(cell.str());

            last_cluster = match last_cluster.take() {
                None => Some(Cluster {
                    cell_width: cell.width() as u16,
                    attrs: cell.attrs().clone(),
                }),
                Some(cluster) if cluster.attrs != *cell.attrs() => {
                    clusters.push(cluster);
                    Some(Cluster {
                        cell_width: cell.width() as u16,
                        attrs: cell.attrs().clone(),
                    })
                }
                Some(mut cluster) => {
                    cluster.cell_width += cell.width() as u16;
                    Some(cluster)
                }
            };
        }

        if let Some(cluster) = last_cluster.take() {
            clusters.push(cluster);
        }

        // `hint` counts cells, not bytes: a few multi-byte graphemes push
        // past the reservation and amortized growth would retain ~2x hint
        // for the life of the scrollback line. No-op when the fit is exact.
        text.shrink_to_fit();

        Self {
            text,
            is_double_wide,
            clusters,
            len: len.try_into().unwrap(),
            last_cell_width,
        }
    }

    pub fn len(&self) -> usize {
        self.len as usize
    }

    fn is_double_wide(&self, cell_index: usize) -> bool {
        match &self.is_double_wide {
            Some(bitset) => bitset.contains(cell_index),
            None => false,
        }
    }

    pub fn iter(&self) -> ClusterLineCellIter<'_> {
        let mut clusters = self.clusters.iter();
        let cluster = clusters.next();
        ClusterLineCellIter {
            graphemes: Graphemes::new(&self.text),
            clusters,
            cluster,
            idx: 0,
            cluster_total: 0,
            line: self,
        }
    }

    /// Append printable ASCII that has already been validated by Line.
    pub fn append_ascii(&mut self, text: &str, attrs: &CellAttributes) {
        debug_assert!(text.bytes().all(|b| (b' '..=b'~').contains(&b)));
        if text.is_empty() {
            return;
        }
        let mut remaining = text.len();
        if let Some(cluster) = self.clusters.last_mut() {
            if cluster.attrs == *attrs {
                let count = remaining.min((u16::MAX - cluster.cell_width) as usize);
                cluster.cell_width += count as u16;
                remaining -= count;
            }
        }
        while remaining > 0 {
            let count = remaining.min(u16::MAX as usize);
            self.clusters.push(Cluster {
                attrs: attrs.clone(),
                cell_width: count as u16,
            });
            remaining -= count;
        }
        self.text.push_str(text);
        self.len += text.len() as u32;
        self.last_cell_width = NonZeroU8::new(1);
    }

    pub fn append_grapheme(&mut self, text: &str, cell_width: usize, attrs: CellAttributes) {
        let cell_width = cell_width as u16;
        let new_cluster = match self.clusters.last() {
            Some(cluster) => {
                if cluster.attrs != attrs {
                    true
                } else {
                    // If we overflow the max length of a run,
                    // then we need a new cluster
                    let (_, did_overflow) = cluster.cell_width.overflowing_add(cell_width);
                    did_overflow
                }
            }
            None => true,
        };
        let new_cell_index = self.len as usize;
        if new_cluster {
            self.clusters.push(Cluster { attrs, cell_width });
        } else if let Some(cluster) = self.clusters.last_mut() {
            cluster.cell_width += cell_width;
        }
        self.text.push_str(text);

        if cell_width > 1 {
            let bitset = match self.is_double_wide.take() {
                Some(mut bitset) => {
                    bitset.grow(new_cell_index + 1);
                    bitset.set(new_cell_index, true);
                    bitset
                }
                None => {
                    let mut bitset = FixedBitSet::with_capacity(new_cell_index + 1);
                    bitset.set(new_cell_index, true);
                    Box::new(bitset)
                }
            };
            self.is_double_wide.replace(bitset);
        }
        self.last_cell_width = NonZeroU8::new(cell_width as u8);
        self.len += cell_width as u32;
    }

    pub fn append(&mut self, cell: Cell) {
        let cell_width = cell.width() as u16;
        let new_cluster = match self.clusters.last() {
            Some(cluster) => {
                if cluster.attrs != *cell.attrs() {
                    true
                } else {
                    // If we overflow the max length of a run,
                    // then we need a new cluster
                    let (_, did_overflow) = cluster.cell_width.overflowing_add(cell_width);
                    did_overflow
                }
            }
            None => true,
        };
        let new_cell_index = self.len as usize;
        if new_cluster {
            self.clusters.push(Cluster {
                attrs: (*cell.attrs()).clone(),
                cell_width,
            });
        } else if let Some(cluster) = self.clusters.last_mut() {
            cluster.cell_width += cell_width;
        }
        self.text.push_str(cell.str());

        if cell_width > 1 {
            let bitset = match self.is_double_wide.take() {
                Some(mut bitset) => {
                    bitset.grow(new_cell_index + 1);
                    bitset.set(new_cell_index, true);
                    bitset
                }
                None => {
                    let mut bitset = FixedBitSet::with_capacity(new_cell_index + 1);
                    bitset.set(new_cell_index, true);
                    Box::new(bitset)
                }
            };
            self.is_double_wide.replace(bitset);
        }
        self.last_cell_width = NonZeroU8::new(cell_width as u8);
        self.len += cell_width as u32;
    }

    pub fn prune_trailing_blanks(&mut self) -> bool {
        let num_spaces = self.text.chars().rev().take_while(|&c| c == ' ').count();
        if num_spaces == 0 {
            return false;
        }

        let blank = CellAttributes::blank();
        let mut pruned = false;
        for _ in 0..num_spaces {
            let mut need_pop = false;
            if let Some(cluster) = self.clusters.last_mut() {
                if cluster.attrs != blank {
                    break;
                }
                cluster.cell_width -= 1;
                self.text.pop();
                self.len -= 1;
                self.last_cell_width.take();
                pruned = true;
                if cluster.cell_width == 0 {
                    need_pop = true;
                }
            }
            if need_pop {
                self.clusters.pop();
            }
        }

        pruned
    }

    fn compute_last_cell_width(&mut self) -> Option<NonZeroU8> {
        if self.last_cell_width.is_none() {
            if let Some(last_cell) = self.iter().last() {
                self.last_cell_width = NonZeroU8::new(last_cell.width() as u8);
            }
        }
        self.last_cell_width
    }

    pub fn set_last_cell_was_wrapped(&mut self, wrapped: bool) {
        if let Some(width) = self.compute_last_cell_width() {
            let width = width.get() as u16;
            if let Some(last_cluster) = self.clusters.last_mut() {
                let mut attrs = last_cluster.attrs.clone();
                attrs.set_wrapped(wrapped);

                if last_cluster.cell_width == width {
                    // Re-purpose final cluster
                    last_cluster.attrs = attrs;
                } else {
                    last_cluster.cell_width -= width;
                    self.clusters.push(Cluster {
                        cell_width: width,
                        attrs,
                    });
                }
            }
        }
    }
}

pub(crate) struct ClusterLineCellIter<'a> {
    graphemes: Graphemes<'a>,
    clusters: core::slice::Iter<'a, Cluster>,
    cluster: Option<&'a Cluster>,
    idx: usize,
    cluster_total: usize,
    line: &'a ClusteredLine,
}

impl<'a> Iterator for ClusterLineCellIter<'a> {
    type Item = CellRef<'a>;

    fn next(&mut self) -> Option<CellRef<'a>> {
        let text = self.graphemes.next()?;

        let cell_index = self.idx;
        let width = if self.line.is_double_wide(cell_index) {
            2
        } else {
            1
        };
        self.idx += width;
        self.cluster_total += width;
        let attrs = &self.cluster.as_ref()?.attrs;

        if self.cluster_total >= self.cluster.as_ref()?.cell_width as usize {
            self.cluster = self.clusters.next();
            self.cluster_total = 0;
        }

        Some(CellRef::ClusterRef {
            cell_index,
            width,
            text,
            attrs,
        })
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn scroll_reuse_keeps_small_buffers_but_rejects_large_ones() {
        let mut line = ClusteredLine::new();
        line.append_ascii("abc", &CellAttributes::default());
        let text = line.text.as_ptr();
        let clusters = line.clusters.as_ptr();
        assert!(line.clear_for_scrolling(160));
        assert_eq!(line, ClusteredLine::new());
        assert_eq!(line.text.as_ptr(), text);
        assert_eq!(line.clusters.as_ptr(), clusters);

        line.text.reserve(4096);
        assert!(!line.clear_for_scrolling(160));
        let mut many_styles = ClusteredLine::new();
        for n in 0..16 {
            let mut attrs = CellAttributes::default();
            attrs.set_foreground(wezterm_cell::color::ColorAttribute::PaletteIndex(n));
            many_styles.append_ascii("x", &attrs);
        }
        assert!(!many_styles.clear_for_scrolling(160));
    }

    #[test]
    #[cfg(target_pointer_width = "64")]
    fn memory_usage() {
        assert_eq!(core::mem::size_of::<ClusteredLine>(), 64);
        assert_eq!(core::mem::size_of::<String>(), 24);
        assert_eq!(core::mem::size_of::<Vec<Cluster>>(), 24);
        assert_eq!(core::mem::size_of::<Option<Box<FixedBitSet>>>(), 8);
        assert_eq!(core::mem::size_of::<Option<NonZeroU8>>(), 1);
    }
}
