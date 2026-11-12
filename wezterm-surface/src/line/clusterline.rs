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
#[cfg(any(feature = "use_serde", test))]
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
#[derive(Clone, PartialEq)]
pub(crate) struct ClusteredLine {
    /// Always valid UTF-8: only text goes in, and only ASCII over ASCII. It
    /// is held as bytes for that second part, which a program moving around
    /// the screen does for every character, and checked by `text()` when it
    /// is read, which is rarer and goes through the whole line anyway.
    #[cfg_attr(feature = "use_serde", serde(serialize_with = "serialize_text"))]
    pub text: Vec<u8>,
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

/// Whether `after`, written right behind `before`, would become part of its
/// last grapheme. A clustered line has nothing but its text to say where the
/// cells are, so two cells that read back as one grapheme are one cell: text
/// that comes apart like this has to be kept as cells.
#[inline]
pub(crate) fn runs_into(before: &[u8], after: &str) -> bool {
    match (before.last(), after.as_bytes().first()) {
        (Some(last), Some(first)) => {
            !(ends_simply(before, *last) && starts_simply(*first))
                && !plain_meets_plain(before, after)
                && chars_join(before, after)
        }
        _ => false,
    }
}

/// Whether a character with this first byte can only be ASCII, a Han
/// ideograph (U+4000..=U+9FFF) or a private use one (U+E000..=U+EFFF, which
/// is where icons in prompts are), which is what nearly all text comes down
/// to, and which join nothing on either side of them unless the other asks.
#[inline]
fn starts_simply(first: u8) -> bool {
    first.is_ascii() || matches!(first, 0xe4..=0xe9 | 0xee)
}

/// `starts_simply` of the character at the end of `before`.
#[inline]
fn ends_simply(before: &[u8], last: u8) -> bool {
    last.is_ascii() || matches!(*before, [.., 0xe4..=0xe9 | 0xee, 0x80..=0xbf, 0x80..=0xbf])
}

/// Characters that are the same to segmenting whichever version of Unicode
/// their widths come from.
#[cfg(feature = "std")]
fn plain_chars() -> Option<&'static wezterm_cell::PlainWidths> {
    wezterm_cell::UnicodeVersion::new(9).plain_widths()
}

/// Whether the characters that meet are ones that never join.
#[cfg(feature = "std")]
fn plain_meets_plain(before: &[u8], after: &str) -> bool {
    let earliest = before.len().saturating_sub(3);
    let start = (earliest..before.len())
        .rfind(|&index| before[index] & 0xc0 != 0x80)
        .unwrap_or(earliest);
    plain_chars().is_some_and(|plain| {
        plain.first(&before[start..]).is_some() && plain.first(after.as_bytes()).is_some()
    })
}

#[cfg(not(feature = "std"))]
fn plain_meets_plain(_before: &[u8], _after: &str) -> bool {
    false
}

#[inline(never)]
fn chars_join(before: &[u8], after: &str) -> bool {
    // What joins can depend on the characters before the last one, though
    // not on many of them.
    const CONTEXT: usize = 16;
    let Some(first) = after.chars().next() else {
        return false;
    };
    // Except that regional indicators pair off from the first of them.
    if ('\u{1f1e6}'..='\u{1f1ff}').contains(&first) {
        let (_, indicators) = before.as_rchunks::<4>();
        return indicators
            .iter()
            .rev()
            .take_while(|c| matches!(c, [0xf0, 0x9f, 0x87, 0xa6..=0xbf]))
            .count()
            % 2
            == 1;
    }
    let earliest = before.len().saturating_sub(CONTEXT);
    let start = (earliest..before.len())
        .find(|&index| before[index] & 0xc0 != 0x80)
        .unwrap_or(before.len());
    let context = &before[start..];
    let mut joined = [0; CONTEXT + 4];
    joined[..context.len()].copy_from_slice(context);
    let end = context.len() + first.encode_utf8(&mut joined[context.len()..]).len();
    core::str::from_utf8(&joined[..end]).is_ok_and(|text| {
        Graphemes::new(text)
            .last()
            .is_some_and(|g| g.len() > first.len_utf8())
    })
}

impl core::fmt::Debug for ClusteredLine {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ClusteredLine")
            .field("text", &self.text())
            .field("is_double_wide", &self.is_double_wide)
            .field("clusters", &self.clusters)
            .field("len", &self.len)
            .field("last_cell_width", &self.last_cell_width)
            .finish()
    }
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
            text: wire.text.into_bytes(), is_double_wide, clusters: wire.clusters,
            len: wire.len, last_cell_width: wire.last_cell_width,
        })
    }
}

/// As the string it is on the wire.
#[cfg(feature = "use_serde")]
fn serialize_text<S>(value: &[u8], serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    core::str::from_utf8(value)
        .map_err(serde::ser::Error::custom)?
        .serialize(serializer)
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
    /// How many clusters a line can have for one in the middle to be worth
    /// changing in place.
    const MAX_CLUSTERS: usize = 16;

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

    /// The text capacity a new row starts with.
    pub(crate) const NEW_TEXT_CAPACITY: usize = 80;

    pub fn new() -> Self {
        Self {
            text: Vec::with_capacity(Self::NEW_TEXT_CAPACITY),
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

        // Cells can only read back short of `len()` from a line that was not
        // written here; there is no use in believing all of what that claims.
        let len = self.len().min(cells.len() + 256);
        if cells.len() < len {
            cells.resize_with(len, Cell::blank);
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
        let mut text = Vec::with_capacity(hint);
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

            text.extend_from_slice(cell.str().as_bytes());

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

    /// The line's text. It can only be anything but valid from a line off
    /// the wire that lied about its cells, which is not worth a crash.
    pub(crate) fn text(&self) -> &str {
        core::str::from_utf8(&self.text).unwrap_or_default()
    }

    /// Whether `text` at the end of the line would run into what is there.
    #[inline]
    pub(crate) fn joins(&self, text: &str) -> bool {
        runs_into(&self.text, text)
    }

    /// Whether the line's graphemes are `cells`, as they have to be for each
    /// to be read back as the cell it was.
    pub(crate) fn reads_back_as<'a>(&self, mut cells: impl Iterator<Item = CellRef<'a>>) -> bool {
        self.text.is_ascii()
            || self.is_plain()
            || Graphemes::new(self.text())
                .all(|g| matches!(cells.next(), Some(cell) if cell.str() == g))
                && cells.next().is_none()
    }

    /// Whether the text is all characters that never join each other.
    #[cfg(feature = "std")]
    fn is_plain(&self) -> bool {
        let Some(plain) = plain_chars() else {
            return false;
        };
        let mut rest = &self.text[..];
        while let Some((len, _)) = plain.first(rest) {
            rest = &rest[len..];
        }
        rest.is_empty()
    }

    #[cfg(not(feature = "std"))]
    fn is_plain(&self) -> bool {
        false
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
            graphemes: Graphemes::new(self.text()),
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
        self.text.extend_from_slice(text.as_bytes());
        self.len += text.len() as u32;
        self.last_cell_width = NonZeroU8::new(1);
    }

    /// Whether each cell of the line is one byte of its text, which is what
    /// lets a cell be found, and replaced, without going through the rest.
    fn cells_are_bytes(&self) -> bool {
        self.is_double_wide.is_none() && self.text.len() == self.len as usize
    }

    /// Whether the line is nothing but default blanks.
    pub(crate) fn is_all_blank(&self) -> bool {
        self.cells_are_bytes()
            && self
                .clusters
                .iter()
                .all(|c| c.attrs == CellAttributes::blank())
            && self.text.iter().all(|&b| b == b' ')
    }

    /// Whether nothing but default blanks are left from cell `idx` on, which
    /// a run can take the place of without looking at the cells. Not for a
    /// line with a lot of clusters already: going through them all for each
    /// new one is what cells are for.
    pub(crate) fn is_blank_from(&self, idx: usize) -> bool {
        let Some(count) = (self.len as usize).checked_sub(idx) else {
            return false;
        };
        let Some(end) = self.text.len().checked_sub(count) else {
            return false;
        };
        let (words, rest) = self.text[end..].as_chunks::<8>();
        self.clusters.len() < Self::MAX_CLUSTERS
            && matches!(self.clusters.last(), Some(cluster)
                if cluster.cell_width as usize >= count && cluster.attrs == CellAttributes::blank())
            && words.iter().all(|word| *word == [b' '; 8])
            && rest.iter().all(|&b| b == b' ')
            && self.no_wide_cells_from(idx.saturating_sub(1))
            && !runs_into(&self.text[..end], " ")
    }

    /// Whether there are no wide cells from cell `idx` on. The bits stop at
    /// the last wide cell unless the line was built all at once.
    fn no_wide_cells_from(&self, idx: usize) -> bool {
        self.is_double_wide
            .as_ref()
            .is_none_or(|bits| (idx..bits.len()).all(|cell| !bits.contains(cell)))
    }

    /// The cell at `cell_index`, if `cells_are_bytes` says it can be told
    /// without going through the graphemes before it.
    pub(crate) fn get_byte_cell(&self, cell_index: usize) -> Option<CellRef<'_>> {
        if cell_index >= self.len as usize || !self.cells_are_bytes() {
            return None;
        }
        let mut end = 0;
        let cluster = self.clusters.iter().find(|cluster| {
            end += cluster.cell_width as usize;
            cell_index < end
        })?;
        Some(CellRef::ClusterRef {
            cell_index,
            text: core::str::from_utf8(self.text.get(cell_index..cell_index + 1)?).ok()?,
            width: 1,
            attrs: &cluster.attrs,
        })
    }

    /// Replace `range` of the cells with ASCII written by `write`, all with
    /// `attrs`, keeping the line compact. False, changing nothing, unless the
    /// range is on a line `cells_are_bytes` is true of. The clusters are the
    /// only part of it to go through, and a row that has many of them is
    /// going to be cheaper as cells.
    pub(crate) fn overwrite_ascii(
        &mut self,
        range: core::ops::Range<usize>,
        attrs: &CellAttributes,
        write: impl FnOnce(&mut [u8]),
    ) -> bool {
        if range.end > self.len as usize || range.is_empty() || !self.cells_are_bytes() {
            return false;
        }
        // A cell's image placements outlive what is written over it. Clusters
        // holding any are left for the cells to sort out.
        #[cfg(feature = "use_image")]
        if attrs.has_images() || self.clusters.iter().any(|c| c.attrs.has_images()) {
            return false;
        }
        // The clusters `range` touches, and how far into them it starts and
        // ends.
        let mut pos = 0;
        let mut first = None;
        let mut last = None;
        for (index, cluster) in self.clusters.iter().enumerate() {
            let end = pos + cluster.cell_width as usize;
            if first.is_none() && range.start < end {
                first = Some((index, range.start - pos));
            }
            if range.end <= end {
                last = Some((index, end - range.end));
                break;
            }
            pos = end;
        }
        let (Some((first, before)), Some((last, after))) = (first, last) else {
            return false;
        };
        let same = first == last && self.clusters[first].attrs == *attrs;
        if !same {
            if self.clusters.len() >= Self::MAX_CLUSTERS {
                return false;
            }
            let cells = range.len();
            if cells > u16::MAX as usize {
                return false;
            }
            let mut pieces: [Option<Cluster>; 3] = [None, None, None];
            if before > 0 {
                pieces[0] = Some(Cluster {
                    cell_width: before as u16,
                    attrs: self.clusters[first].attrs.clone(),
                });
            }
            pieces[1] = Some(Cluster {
                cell_width: cells as u16,
                attrs: attrs.clone(),
            });
            if after > 0 {
                pieces[2] = Some(Cluster {
                    cell_width: after as u16,
                    attrs: self.clusters[last].attrs.clone(),
                });
            }
            self.clusters
                .splice(first..=last, IntoIterator::into_iter(pieces).flatten());
        }
        write(&mut self.text[range.clone()]);
        debug_assert!(self.text[range].is_ascii());
        true
    }

    /// `append_ascii` of `count` copies of the printable ASCII character `c`.
    pub fn append_ascii_repeated(&mut self, c: char, count: usize, attrs: &CellAttributes) {
        assert!((' '..='~').contains(&c));
        let extend = matches!(self.clusters.last(), Some(cluster) if cluster.attrs == *attrs);
        let mut remaining = count;
        if extend {
            if let Some(cluster) = self.clusters.last_mut() {
                let more = remaining.min((u16::MAX - cluster.cell_width) as usize);
                cluster.cell_width += more as u16;
                remaining -= more;
            }
        }
        while remaining > 0 {
            let more = remaining.min(u16::MAX as usize);
            self.clusters.push(Cluster {
                attrs: attrs.clone(),
                cell_width: more as u16,
            });
            remaining -= more;
        }
        self.text.resize(self.text.len() + count, c as u8);
        self.len += count as u32;
        self.last_cell_width = NonZeroU8::new(1);
    }

    /// Whether `text` at cell `idx`, which is the end of the line or where
    /// only blanks are left of it, would run into what is before it.
    pub(crate) fn joins_at(&self, idx: usize, text: &str) -> bool {
        let end = self.text.len() - (self.len as usize - idx);
        runs_into(&self.text[..end], text)
    }

    /// Take away the blanks from cell `idx` on (see `is_blank_from`).
    pub(crate) fn truncate_blanks(&mut self, idx: usize) {
        let blanks = self.len as usize - idx;
        if blanks == 0 {
            return;
        }
        self.text.truncate(self.text.len() - blanks);
        self.len = idx as u32;
        self.last_cell_width = None;
        if let Some(cluster) = self.clusters.last_mut() {
            cluster.cell_width -= blanks as u16;
            if cluster.cell_width == 0 {
                self.clusters.pop();
            }
        }
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
        self.text.extend_from_slice(text.as_bytes());

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

    /// `append_grapheme` for each grapheme of a run in turn, all with
    /// `attrs`: `text` is their concatenation, `max_len` the byte length of
    /// the longest, `cells` the sum of their widths, `wide` the offsets from
    /// the current end of those more than one cell wide, and `last_width`
    /// the width of the final one. The text and the wide-cell set are left
    /// with the capacities appending one at a time would give them. Returns
    /// false, changing nothing, if the run would overflow a cluster's width,
    /// where appending one at a time would start another cluster, or if the
    /// text's capacity would depend on more than `max_len` and the total.
    pub fn append_run(
        &mut self,
        text: &str,
        max_len: usize,
        cells: usize,
        wide: &[u16],
        last_width: u8,
        attrs: &CellAttributes,
    ) -> bool {
        let extend = matches!(self.clusters.last(), Some(cluster) if cluster.attrs == *attrs);
        let prior = if extend {
            self.clusters
                .last()
                .map_or(0, |cluster| cluster.cell_width as usize)
        } else {
            0
        };
        if prior + cells > u16::MAX as usize {
            return false;
        }
        let Some(capacity) =
            grown_capacity(self.text.capacity(), self.text.len() + text.len(), max_len)
        else {
            return false;
        };
        match self.clusters.last_mut() {
            Some(cluster) if extend => cluster.cell_width = (prior + cells) as u16,
            _ => self.clusters.push(Cluster {
                attrs: attrs.clone(),
                cell_width: cells as u16,
            }),
        }
        self.text.reserve_exact(capacity - self.text.len());
        self.text.extend_from_slice(text.as_bytes());
        if let (Some(&first), Some(&last)) = (wide.first(), wide.last()) {
            let base = self.len as usize;
            let bitset = self.is_double_wide.get_or_insert_with(|| {
                Box::new(FixedBitSet::with_capacity(base + first as usize + 1))
            });
            // Grown wherever growing for each wide cell in turn would add a
            // block (32 bits, or a multiple), so it reallocates to the same
            // sizes.
            let mut blocks = bitset.len().div_ceil(32);
            for &offset in wide {
                let bits = base + offset as usize + 1;
                if bits.div_ceil(32) > blocks {
                    bitset.grow(bits);
                    blocks = bits.div_ceil(32);
                }
            }
            bitset.grow(base + last as usize + 1);
            for &offset in wide {
                bitset.set(base + offset as usize, true);
            }
        }
        self.last_cell_width = NonZeroU8::new(last_width);
        self.len += cells as u32;
        true
    }

    /// `append_run` for a run whose wide cells are given as a mask: bit n of
    /// `wide`, lowest first, is the cell n places from the current end, set
    /// where a grapheme two cells wide starts. A row of wide characters is
    /// marked a word at a time, and its set of them sized once.
    pub fn append_run_masked(
        &mut self,
        text: &str,
        max_len: usize,
        cells: usize,
        wide: &[u64],
        last_width: u8,
        attrs: &CellAttributes,
    ) -> bool {
        let extend = matches!(self.clusters.last(), Some(cluster) if cluster.attrs == *attrs);
        let prior = if extend {
            self.clusters
                .last()
                .map_or(0, |cluster| cluster.cell_width as usize)
        } else {
            0
        };
        if prior + cells > u16::MAX as usize {
            return false;
        }
        let Some(capacity) =
            grown_capacity(self.text.capacity(), self.text.len() + text.len(), max_len)
        else {
            return false;
        };
        match self.clusters.last_mut() {
            Some(cluster) if extend => cluster.cell_width = (prior + cells) as u16,
            _ => self.clusters.push(Cluster {
                attrs: attrs.clone(),
                cell_width: cells as u16,
            }),
        }
        self.text.reserve_exact(capacity - self.text.len());
        self.text.extend_from_slice(text.as_bytes());
        let last = wide
            .iter()
            .enumerate()
            .rev()
            .find(|(_, word)| **word != 0)
            .map(|(index, word)| index * 64 + 63 - word.leading_zeros() as usize);
        if let Some(last) = last {
            let base = self.len as usize;
            let bits = base + last + 1;
            // Sized once for the run: a row of wide characters was otherwise
            // a growth for every 32 cells of it.
            let bitset = self
                .is_double_wide
                .get_or_insert_with(|| Box::new(FixedBitSet::with_capacity(bits)));
            bitset.grow(bits);
            // The set keeps its bits 32 to a block; a word of the
            // mask lands on up to three of them.
            let blocks = bitset.as_mut_slice();
            for (index, &word) in wide.iter().enumerate() {
                if word == 0 {
                    continue;
                }
                let bit = base + index * 64;
                let spread = (word as u128) << (bit % 32);
                let parts = [spread as u32, (spread >> 32) as u32, (spread >> 64) as u32];
                for (block, part) in blocks[bit / 32..].iter_mut().zip(parts) {
                    *block |= part;
                }
            }
        }
        self.last_cell_width = NonZeroU8::new(last_width);
        self.len += cells as u32;
        true
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
        self.text.extend_from_slice(cell.str().as_bytes());

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
        let num_spaces = self.text.iter().rev().take_while(|&&b| b == b' ').count();
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

/// The capacity text of `capacity` ends with when graphemes of at most
/// `max_len` bytes are pushed one at a time until it holds `len` bytes, or
/// None where that would depend on each one's length. Text grows to twice
/// its capacity, or to what it needs if more, and to at least 8 bytes.
fn grown_capacity(capacity: usize, len: usize, max_len: usize) -> Option<usize> {
    if len <= capacity {
        return Some(capacity);
    }
    let mut capacity = match capacity {
        // The first push takes it to 8 bytes.
        0 if max_len <= 8 => 8,
        // No push then needs more than twice the capacity.
        4.. if max_len <= capacity => capacity,
        _ => return None,
    };
    while capacity < len {
        capacity *= 2;
    }
    Some(capacity)
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
