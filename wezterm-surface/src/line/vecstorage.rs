use crate::line::cellref::CellRef;
use alloc::sync::Arc;
#[cfg(feature = "use_serde")]
use serde::{Deserialize, Serialize};
use unicode_segmentation::UnicodeSegmentation;
use wezterm_cell::{Cell, CellAttributes};

extern crate alloc;
use alloc::vec::Vec;

#[cfg_attr(feature = "use_serde", derive(Serialize, Deserialize))]
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct VecStorage {
    cells: Vec<Cell>,
}

impl VecStorage {
    pub(crate) fn new(cells: Vec<Cell>) -> Self {
        Self { cells }
    }

    // Keep the clearing and allocation loops out of the line editing paths.
    #[inline(never)]
    pub(crate) fn resize_and_clear(&mut self, width: usize, attrs: CellAttributes) {
        // Discard removed cells before clearing, rather than allocating new
        // attributes for cells that will immediately be dropped.
        self.cells.truncate(width);
        let is_rgb = |color| {
            !matches!(
                color,
                wezterm_cell::color::ColorAttribute::Default
                    | wezterm_cell::color::ColorAttribute::PaletteIndex(_)
            )
        };
        if is_rgb(attrs.background()) || is_rgb(attrs.foreground()) {
            let blank = Cell::blank_with_attrs(attrs.clone());
            for cell in &mut self.cells {
                cell.clone_from(&blank);
            }
        } else {
            // Replacing lightweight attributes is faster than clone_from;
            // reserve allocation reuse for heap-backed RGB colors.
            for cell in &mut self.cells {
                *cell = Cell::blank_with_attrs(attrs.clone());
            }
        }
        self.cells
            .resize_with(width, || Cell::blank_with_attrs(attrs.clone()));
        self.cells.shrink_to_fit();
    }

    #[cfg_attr(not(feature = "use_image"), allow(unused_mut, unused_variables))]
    pub(crate) fn set_cell(&mut self, idx: usize, mut cell: Cell, clear_image_placement: bool) {
        #[cfg(feature = "use_image")]
        if !clear_image_placement {
            if let Some(images) = self.cells[idx].attrs().images() {
                for image in images {
                    if image.has_placement_id() {
                        cell.attrs_mut().attach_image(Box::new(image));
                    }
                }
            }
        }
        self.cells[idx] = cell;
    }

    /// Repeated writes can reuse a cell's extended-attribute allocation. Keep
    /// the existing merge path whenever the destination has image attachments.
    pub(crate) fn set_cell_from(&mut self, idx: usize, cell: &Cell) {
        #[cfg(feature = "use_image")]
        if self.cells[idx].attrs().has_images() {
            self.set_cell(idx, cell.clone(), false);
            return;
        }
        self.cells[idx].clone_from(cell);
    }

    pub(crate) fn scan_and_create_hyperlinks(
        &mut self,
        line: &str,
        matches: Vec<crate::hyperlink::RuleMatch>,
    ) -> bool {
        // The capture range is measured in bytes but we need to translate
        // that to the index of the column.  This is complicated a bit further
        // because double wide sequences have a blank column cell after them
        // in the cells array, but the string we match against excludes that
        // string.
        let mut cell_idx = 0;
        let mut has_implicit_hyperlinks = false;
        for (byte_idx, _grapheme) in line.grapheme_indices(true) {
            let cell = &mut self.cells[cell_idx];
            let mut matched = false;
            for m in &matches {
                if m.range.contains(&byte_idx) {
                    let attrs = cell.attrs_mut();
                    // Don't replace existing links
                    if attrs.hyperlink().is_none() {
                        attrs.set_hyperlink(Some(Arc::clone(&m.link)));
                        matched = true;
                    }
                }
            }
            cell_idx += cell.width();
            if matched {
                has_implicit_hyperlinks = true;
            }
        }

        has_implicit_hyperlinks
    }
}

impl core::ops::Deref for VecStorage {
    type Target = Vec<Cell>;

    fn deref(&self) -> &Vec<Cell> {
        &self.cells
    }
}

impl core::ops::DerefMut for VecStorage {
    fn deref_mut(&mut self) -> &mut Vec<Cell> {
        &mut self.cells
    }
}

/// Iterates over a slice of Cell, yielding only visible cells
pub(crate) struct VecStorageIter<'a> {
    pub cells: core::slice::Iter<'a, Cell>,
    pub idx: usize,
    pub skip_width: usize,
}

impl<'a> Iterator for VecStorageIter<'a> {
    type Item = CellRef<'a>;

    fn next(&mut self) -> Option<CellRef<'a>> {
        while self.skip_width > 0 {
            self.skip_width -= 1;
            let _ = self.cells.next()?;
            self.idx += 1;
        }
        let cell = self.cells.next()?;
        let cell_index = self.idx;
        self.idx += 1;
        self.skip_width = cell.width().saturating_sub(1);
        Some(CellRef::CellRef { cell_index, cell })
    }
}
