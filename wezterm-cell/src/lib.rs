#![cfg_attr(not(feature = "std"), no_std)]
//! Model a cell in the terminal display
use crate::color::{ColorAttribute, PaletteIndex};
#[cfg(feature = "use_image")]
use crate::image::ImageCell;
use alloc::sync::Arc;
use core::hash::{Hash, Hasher};
use core::mem;
use finl_unicode::grapheme_clusters::Graphemes;
#[cfg(feature = "use_serde")]
use serde::{Deserialize, Deserializer, Serialize, Serializer};
pub use wezterm_char_props::emoji::Presentation;
use wezterm_char_props::emoji_variation::WCWIDTH_TABLE;
use wezterm_char_props::widechar_width::WcWidth;
use wezterm_dynamic::{FromDynamic, ToDynamic};
pub use wezterm_escape_parser::osc::Hyperlink;

extern crate alloc;
use crate::alloc::string::ToString;
use alloc::boxed::Box;
use alloc::vec::Vec;

pub mod color;
#[cfg(feature = "use_image")]
pub mod image;

#[cfg_attr(feature = "use_serde", derive(Serialize, Deserialize))]
#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash)]
enum SmallColor {
    Default,
    PaletteIndex(PaletteIndex),
}

impl Default for SmallColor {
    fn default() -> Self {
        Self::Default
    }
}

impl Into<ColorAttribute> for SmallColor {
    fn into(self) -> ColorAttribute {
        match self {
            Self::Default => ColorAttribute::Default,
            Self::PaletteIndex(idx) => ColorAttribute::PaletteIndex(idx),
        }
    }
}

/// Holds the attributes for a cell.
/// Most style attributes are stored internally as part of a bitfield
/// to reduce per-cell overhead.
/// The setter methods return a mutable self reference so that they can
/// be chained together.
#[cfg_attr(feature = "use_serde", derive(Serialize, Deserialize))]
#[derive(Eq, PartialEq)]
pub struct CellAttributes {
    attributes: u32,
    /// The foreground color
    foreground: SmallColor,
    /// The background color
    background: SmallColor,
    /// Relatively rarely used attributes spill over to a heap
    /// allocated struct in order to keep CellAttributes
    /// smaller in the common case. Cells copied from one another share
    /// it; a change applies to a copy unless this cell holds it alone.
    #[cfg_attr(feature = "use_serde", serde(with = "fat_wire"))]
    fat: Option<Arc<FatAttributes>>,
}

/// The wire form is that of the unshared `Option<Box<FatAttributes>>`
/// this replaced: an optional struct.
#[cfg(feature = "use_serde")]
mod fat_wire {
    use super::*;

    pub fn serialize<S: Serializer>(
        fat: &Option<Arc<FatAttributes>>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        fat.as_deref().serialize(serializer)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<Arc<FatAttributes>>, D::Error> {
        Ok(Option::<FatAttributes>::deserialize(deserializer)?.map(Arc::new))
    }
}

impl Clone for CellAttributes {
    #[inline]
    fn clone(&self) -> Self {
        Self {
            attributes: self.attributes,
            foreground: self.foreground,
            background: self.background,
            fat: self.fat.clone(),
        }
    }

    #[inline]
    fn clone_from(&mut self, source: &Self) {
        self.attributes = source.attributes;
        self.foreground = source.foreground;
        self.background = source.background;
        self.fat.clone_from(&source.fat);
    }
}

impl core::fmt::Debug for CellAttributes {
    fn fmt(&self, fmt: &mut core::fmt::Formatter<'_>) -> Result<(), core::fmt::Error> {
        fmt.debug_struct("CellAttributes")
            .field("attributes", &self.attributes)
            .field("intensity", &self.intensity())
            .field("underline", &self.underline())
            .field("blink", &self.blink())
            .field("italic", &self.italic())
            .field("reverse", &self.reverse())
            .field("strikethrough", &self.strikethrough())
            .field("invisible", &self.invisible())
            .field("wrapped", &self.wrapped())
            .field("overline", &self.overline())
            .field("semantic_type", &self.semantic_type())
            .field("foreground", &self.foreground)
            .field("background", &self.background)
            .field("fat", &self.fat)
            .finish()
    }
}

/// With the image list kept aside, these fit in the allocation the
/// unshared attributes had even with the reference counts added.
#[derive(Default, Clone, Eq, PartialEq)]
struct FatAttributes {
    /// The hyperlink content, if any
    hyperlink: Option<Arc<Hyperlink>>,
    /// The image data, if any: far fewer cells carry it than colors.
    /// None whenever empty, so that equality holds by value.
    #[cfg(feature = "use_image")]
    image: Option<Box<Vec<Box<ImageCell>>>>,
    /// The color of the underline.  If None, then
    /// the foreground color is to be used
    underline_color: ColorAttribute,
    foreground: ColorAttribute,
    background: ColorAttribute,
}

impl core::fmt::Debug for FatAttributes {
    fn fmt(&self, fmt: &mut core::fmt::Formatter<'_>) -> Result<(), core::fmt::Error> {
        let mut debug = fmt.debug_struct("FatAttributes");
        debug.field("hyperlink", &self.hyperlink());
        #[cfg(feature = "use_image")]
        debug.field("image", &self.images());
        debug
            .field("underline_color", &self.underline_color)
            .field("foreground", &self.foreground)
            .field("background", &self.background)
            .finish()
    }
}

/// The wire form of the extended attributes as they were before the image
/// list moved aside: the same fields, in the same order.
#[cfg(feature = "use_serde")]
#[derive(Serialize)]
#[serde(rename = "FatAttributes")]
struct FatAttributesWire<'a> {
    hyperlink: Option<&'a Arc<Hyperlink>>,
    #[cfg(feature = "use_image")]
    image: &'a [Box<ImageCell>],
    underline_color: &'a ColorAttribute,
    foreground: &'a ColorAttribute,
    background: &'a ColorAttribute,
}

#[cfg(feature = "use_serde")]
#[derive(Deserialize)]
#[serde(rename = "FatAttributes")]
struct FatAttributesOwnedWire {
    hyperlink: Option<Arc<Hyperlink>>,
    #[cfg(feature = "use_image")]
    image: Vec<Box<ImageCell>>,
    underline_color: ColorAttribute,
    foreground: ColorAttribute,
    background: ColorAttribute,
}

#[cfg(feature = "use_serde")]
impl Serialize for FatAttributes {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        FatAttributesWire {
            hyperlink: self.hyperlink(),
            #[cfg(feature = "use_image")]
            image: self.images(),
            underline_color: &self.underline_color,
            foreground: &self.foreground,
            background: &self.background,
        }
        .serialize(serializer)
    }
}

#[cfg(feature = "use_serde")]
impl<'de> Deserialize<'de> for FatAttributes {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let wire = FatAttributesOwnedWire::deserialize(deserializer)?;
        #[allow(unused_mut)]
        let mut fat = FatAttributes {
            hyperlink: wire.hyperlink,
            #[cfg(feature = "use_image")]
            image: None,
            underline_color: wire.underline_color,
            foreground: wire.foreground,
            background: wire.background,
        };
        #[cfg(feature = "use_image")]
        fat.update_images(|images| *images = wire.image);
        Ok(fat)
    }
}

impl FatAttributes {
    /// Whether every attribute has its default, so that none need be kept.
    fn is_empty(&self) -> bool {
        #[cfg(feature = "use_image")]
        {
            if self.image.is_some() {
                return false;
            }
        }
        self.hyperlink.is_none()
            && self.underline_color == ColorAttribute::Default
            && self.foreground == ColorAttribute::Default
            && self.background == ColorAttribute::Default
    }

    fn hyperlink(&self) -> Option<&Arc<Hyperlink>> {
        self.hyperlink.as_ref()
    }

    #[cfg(feature = "use_image")]
    fn images(&self) -> &[Box<ImageCell>] {
        self.image
            .as_deref()
            .map_or(&[], |images| images.as_slice())
    }

    /// Change the image list, keeping it None when empty.
    #[cfg(feature = "use_image")]
    fn update_images(&mut self, change: impl FnOnce(&mut Vec<Box<ImageCell>>)) {
        change(self.image.get_or_insert_with(Default::default));
        if self.image.as_ref().is_some_and(|images| images.is_empty()) {
            self.image = None;
        }
    }

    pub fn compute_shape_hash<H: Hasher>(&self, hasher: &mut H) {
        if let Some(link) = self.hyperlink() {
            link.compute_shape_hash(hasher);
        }
        #[cfg(feature = "use_image")]
        for cell in self.images() {
            cell.compute_shape_hash(hasher);
        }
        self.underline_color.hash(hasher);
        self.foreground.hash(hasher);
        self.background.hash(hasher);
    }
}

/// Define getter and setter for the attributes bitfield.
/// The first form is for a simple boolean value stored in
/// a single bit.  The $bitnum parameter specifies which bit.
/// The second form is for an integer value that occupies a range
/// of bits.  The $bitmask and $bitshift parameters define how
/// to transform from the stored bit value to the consumable
/// value.
macro_rules! bitfield {
    ($getter:ident, $setter:ident, $bitnum:expr) => {
        #[inline]
        pub fn $getter(&self) -> bool {
            (self.attributes & (1 << $bitnum)) == (1 << $bitnum)
        }

        #[inline]
        pub fn $setter(&mut self, value: bool) -> &mut Self {
            let attr_value = if value { 1 << $bitnum } else { 0 };
            self.attributes = (self.attributes & !(1 << $bitnum)) | attr_value;
            self
        }
    };

    ($getter:ident, $setter:ident, $bitmask:expr, $bitshift:expr) => {
        #[inline]
        pub fn $getter(&self) -> u32 {
            (self.attributes >> $bitshift) & $bitmask
        }

        #[inline]
        pub fn $setter(&mut self, value: u32) -> &mut Self {
            let clear = !($bitmask << $bitshift);
            let attr_value = (value & $bitmask) << $bitshift;
            self.attributes = (self.attributes & clear) | attr_value;
            self
        }
    };

    ($getter:ident, $setter:ident, $enum:ident, $bitmask:expr, $bitshift:expr) => {
        #[inline]
        pub fn $getter(&self) -> $enum {
            unsafe { mem::transmute(((self.attributes >> $bitshift) & $bitmask) as u8) }
        }

        #[inline]
        pub fn $setter(&mut self, value: $enum) -> &mut Self {
            let value = value as u32;
            let clear = !($bitmask << $bitshift);
            let attr_value = (value & $bitmask) << $bitshift;
            self.attributes = (self.attributes & clear) | attr_value;
            self
        }
    };
}

/// Describes the semantic "type" of the cell.
/// This categorizes cells into Output (from the actions the user is
/// taking; this is the default if left unspecified),
/// Input (that the user typed) and Prompt (effectively, "chrome" provided
/// by the shell or application that the user is interacting with.
#[cfg_attr(feature = "use_serde", derive(Serialize, Deserialize))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, FromDynamic, ToDynamic)]
#[repr(u8)]
pub enum SemanticType {
    Output = 0,
    Input = 1,
    Prompt = 2,
}

impl Default for SemanticType {
    fn default() -> Self {
        Self::Output
    }
}

pub use wezterm_escape_parser::csi::{Blink, Intensity, Underline, VerticalAlign};

impl Default for CellAttributes {
    fn default() -> Self {
        Self::blank()
    }
}

impl CellAttributes {
    bitfield!(intensity, set_intensity, Intensity, 0b11, 0);
    bitfield!(underline, set_underline, Underline, 0b111, 2);
    bitfield!(blink, set_blink, Blink, 0b11, 5);
    bitfield!(italic, set_italic, 7);
    bitfield!(reverse, set_reverse, 8);
    bitfield!(strikethrough, set_strikethrough, 9);
    bitfield!(invisible, set_invisible, 10);
    bitfield!(wrapped, set_wrapped, 11);
    bitfield!(overline, set_overline, 12);
    bitfield!(semantic_type, set_semantic_type, SemanticType, 0b11, 13);
    bitfield!(vertical_align, set_vertical_align, VerticalAlign, 0b11, 15);

    pub const fn blank() -> Self {
        Self {
            attributes: 0,
            foreground: SmallColor::Default,
            background: SmallColor::Default,
            fat: None,
        }
    }

    /// Returns true if the attribute bits in both objects are equal.
    /// This can be used to cheaply test whether the styles of the two
    /// cells are the same, and is used by some `Renderer` implementations.
    pub fn attribute_bits_equal(&self, other: &Self) -> bool {
        self.attributes == other.attributes
    }

    pub fn compute_shape_hash<H: Hasher>(&self, hasher: &mut H) {
        self.attributes.hash(hasher);
        self.foreground.hash(hasher);
        self.background.hash(hasher);
        if let Some(fat) = &self.fat {
            fat.compute_shape_hash(hasher);
        }
    }

    /// Set the foreground color for the cell to that specified
    pub fn set_foreground<C: Into<ColorAttribute>>(&mut self, foreground: C) -> &mut Self {
        let foreground: ColorAttribute = foreground.into();
        let (small, fat) = match foreground {
            ColorAttribute::Default => (SmallColor::Default, ColorAttribute::Default),
            ColorAttribute::PaletteIndex(idx) => {
                (SmallColor::PaletteIndex(idx), ColorAttribute::Default)
            }
            foreground => (SmallColor::Default, foreground),
        };
        self.foreground = small;
        if self.fat_foreground() != fat {
            self.update_fat(|attrs| attrs.foreground = fat);
        }
        self.deallocate_fat_attributes_if_none();
        self
    }

    fn fat_foreground(&self) -> ColorAttribute {
        self.fat
            .as_ref()
            .map_or(ColorAttribute::Default, |fat| fat.foreground)
    }

    pub fn foreground(&self) -> ColorAttribute {
        if let Some(fat) = self.fat.as_ref() {
            if fat.foreground != ColorAttribute::Default {
                return fat.foreground;
            }
        }
        self.foreground.into()
    }

    pub fn set_background<C: Into<ColorAttribute>>(&mut self, background: C) -> &mut Self {
        let background: ColorAttribute = background.into();
        let (small, fat) = match background {
            ColorAttribute::Default => (SmallColor::Default, ColorAttribute::Default),
            ColorAttribute::PaletteIndex(idx) => {
                (SmallColor::PaletteIndex(idx), ColorAttribute::Default)
            }
            background => (SmallColor::Default, background),
        };
        self.background = small;
        if self.fat_background() != fat {
            self.update_fat(|attrs| attrs.background = fat);
        }
        self.deallocate_fat_attributes_if_none();
        self
    }

    fn fat_background(&self) -> ColorAttribute {
        self.fat
            .as_ref()
            .map_or(ColorAttribute::Default, |fat| fat.background)
    }

    pub fn background(&self) -> ColorAttribute {
        if let Some(fat) = self.fat.as_ref() {
            if fat.background != ColorAttribute::Default {
                return fat.background;
            }
        }
        self.background.into()
    }

    /// Clear all attributes from a cell
    pub fn clear(&mut self) {
        *self = Self::blank();
    }

    /// Drop extended attributes that are all at their defaults, as every
    /// change does; only ones received that way can be kept otherwise.
    fn deallocate_fat_attributes_if_none(&mut self) {
        if self.fat.as_ref().is_some_and(|fat| fat.is_empty()) {
            self.fat = None;
        }
    }

    /// Apply `change` to the extended attributes: in place when this cell
    /// alone holds them, otherwise to a copy, as other cells may share
    /// them. None are kept once every one is back at its default.
    fn update_fat(&mut self, change: impl FnOnce(&mut FatAttributes)) {
        if let Some(fat) = self.fat.as_mut().and_then(Arc::get_mut) {
            change(fat);
            if fat.is_empty() {
                self.fat = None;
            }
            return;
        }
        let mut fat = self.fat.as_deref().cloned().unwrap_or_default();
        change(&mut fat);
        self.fat = if fat.is_empty() {
            None
        } else {
            Some(Arc::new(fat))
        };
    }

    pub fn set_hyperlink(&mut self, link: Option<Arc<Hyperlink>>) -> &mut Self {
        let unchanged = match (self.hyperlink(), &link) {
            (None, None) => true,
            (Some(current), Some(link)) => Arc::ptr_eq(current, link),
            _ => false,
        };
        if !unchanged {
            self.update_fat(|fat| fat.hyperlink = link);
        }
        self.deallocate_fat_attributes_if_none();
        self
    }
}

#[cfg(feature = "use_image")]
impl CellAttributes {
    /// Assign a single image to a cell.
    pub fn set_image(&mut self, image: Box<ImageCell>) -> &mut Self {
        self.update_fat(|fat| fat.update_images(|images| *images = vec![image]));
        self
    }

    /// Clear all images from a cell
    pub fn clear_images(&mut self) -> &mut Self {
        if self.has_images() {
            self.update_fat(|fat| fat.update_images(|images| images.clear()));
        }
        self.deallocate_fat_attributes_if_none();
        self
    }

    pub fn detach_image_with_placement(&mut self, image_id: u32, placement_id: Option<u32>) {
        let attached = self.fat.as_ref().is_some_and(|fat| {
            fat.images()
                .iter()
                .any(|im| im.matches_placement(image_id, placement_id))
        });
        if attached {
            self.update_fat(|fat| {
                fat.update_images(|images| {
                    images.retain(|im| !im.matches_placement(image_id, placement_id))
                })
            });
        }
        self.deallocate_fat_attributes_if_none();
    }

    /// Remove matching attachments while preserving shared rows and z order.
    pub fn detach_images(&mut self, matches: impl Fn(&ImageCell) -> bool) -> bool {
        if !self.image_attachments().any(&matches) {
            return false;
        }
        self.update_fat(|fat| {
            fat.update_images(|images| images.retain(|image| !matches(image)))
        });
        self.deallocate_fat_attributes_if_none();
        true
    }

    /// Rebind one image's attachments without changing their order or geometry.
    pub fn replace_image_data(
        &mut self,
        image_id: u32,
        old: &Arc<image::ImageData>,
        new: &Arc<image::ImageData>,
    ) -> bool {
        let matches = |image: &ImageCell| {
            image.image_id() == Some(image_id) && Arc::ptr_eq(image.image_data(), old)
        };
        if !self.image_attachments().any(matches) {
            return false;
        }
        self.update_fat(|fat| {
            fat.update_images(|images| {
                for image in images.iter_mut().filter(|image| matches(image)) {
                    image.set_image_data(Arc::clone(new));
                }
            });
        });
        true
    }

    /// Add an image attachement, preserving any existing attachments.
    /// The list of images is maintained in z-index order
    pub fn attach_image(&mut self, image: Box<ImageCell>) -> &mut Self {
        self.update_fat(|fat| {
            fat.update_images(|images| {
                let z_index = image.z_index();
                match images.binary_search_by(|probe| probe.z_index().cmp(&z_index)) {
                    Ok(idx) | Err(idx) => images.insert(idx, image),
                }
            })
        });
        self
    }
}

impl CellAttributes {
    pub fn set_underline_color<C: Into<ColorAttribute>>(
        &mut self,
        underline_color: C,
    ) -> &mut Self {
        let underline_color = underline_color.into();
        if self.underline_color() != underline_color {
            self.update_fat(|fat| fat.underline_color = underline_color);
        }
        self.deallocate_fat_attributes_if_none();
        self
    }

    /// Clone the attributes, but exclude fancy extras such
    /// as hyperlinks or future sprite things
    pub fn clone_sgr_only(&self) -> Self {
        let mut res = Self {
            attributes: self.attributes,
            foreground: self.foreground,
            background: self.background,
            fat: None,
        };
        // Keep the colors, including the underline color, and leave
        // hyperlinks and images behind. Without those the extended
        // attributes are exactly the colors, so share them.
        if let Some(fat) = self.fat.as_ref() {
            #[cfg(feature = "use_image")]
            let images = fat.image.is_some();
            #[cfg(not(feature = "use_image"))]
            let images = false;
            if fat.hyperlink.is_none() && !images {
                if !fat.is_empty() {
                    res.fat = Some(Arc::clone(fat));
                }
            } else {
                res.update_fat(|colors| {
                    colors.foreground = fat.foreground;
                    colors.background = fat.background;
                    colors.underline_color = fat.underline_color;
                });
            }
        }
        // Reset the semantic type; clone_sgr_only is used primarily
        // to create a "blank" cell when clearing and we want that to
        // be deterministically tagged as Output so that we have an
        // easier time in get_semantic_zones.
        res.set_semantic_type(SemanticType::default());

        // Turn off underline because it can have surprising results
        // if underline is on, then we get CRLF and then SGR reset:
        // If the CRLF causes a line to scroll, we'll call clone_sgr_only()
        // to get a blank cell for the new line and it would be filled
        // with underlines.
        // clone_sgr_only() is primarily for preserving the background
        // color when erasing rather than other attributes, so it should
        // be fine to clear out the actual underline attribute.
        // Let's extend this to other line attribute types as well.
        // <https://github.com/wezterm/wezterm/issues/2489>
        res.set_underline(Underline::None);
        res.set_overline(false);
        res.set_strikethrough(false);
        res
    }

    pub fn hyperlink(&self) -> Option<&Arc<Hyperlink>> {
        self.fat.as_ref().and_then(|fat| fat.hyperlink())
    }

    /// Returns the list of attached images in z-index order.
    /// Returns None if there are no attached images; will
    /// never return Some(vec![]).
    #[cfg(feature = "use_image")]
    pub fn images(&self) -> Option<Vec<ImageCell>> {
        let fat = self.fat.as_ref()?;
        if fat.images().is_empty() {
            return None;
        }
        Some(fat.images().iter().map(|im| im.as_ref().clone()).collect())
    }

    /// Test for image attachments without cloning the attachment list.
    /// Checked before every cell write in a repeated range.
    #[cfg(feature = "use_image")]
    #[inline]
    pub fn has_images(&self) -> bool {
        self.fat
            .as_ref()
            .is_some_and(|fat| !fat.images().is_empty())
    }

    /// Borrow attachments without allocating a list or cloning their pixels' Arcs.
    #[cfg(feature = "use_image")]
    pub fn image_attachments(&self) -> impl Iterator<Item = &ImageCell> {
        self.fat
            .iter()
            .flat_map(|fat| fat.images().iter().map(Box::as_ref))
    }

    pub fn underline_color(&self) -> ColorAttribute {
        self.fat
            .as_ref()
            .map(|fat| fat.underline_color)
            .unwrap_or(ColorAttribute::Default)
    }

    pub fn apply_change(&mut self, change: &AttributeChange) {
        use AttributeChange::*;
        match change {
            Intensity(value) => {
                self.set_intensity(*value);
            }
            Underline(value) => {
                self.set_underline(*value);
            }
            Italic(value) => {
                self.set_italic(*value);
            }
            Blink(value) => {
                self.set_blink(*value);
            }
            Reverse(value) => {
                self.set_reverse(*value);
            }
            StrikeThrough(value) => {
                self.set_strikethrough(*value);
            }
            Invisible(value) => {
                self.set_invisible(*value);
            }
            Foreground(value) => {
                self.set_foreground(*value);
            }
            Background(value) => {
                self.set_background(*value);
            }
            Hyperlink(value) => {
                self.set_hyperlink(value.clone());
            }
        }
    }
}

#[cfg(feature = "use_serde")]
fn deserialize_teenystring<'de, D>(deserializer: D) -> Result<TeenyString, D::Error>
where
    D: Deserializer<'de>,
{
    let text = String::deserialize(deserializer)?;
    Ok(TeenyString::from_str(&text, None, None))
}

#[cfg(feature = "use_serde")]
fn serialize_teenystring<S>(value: &TeenyString, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    // unsafety: this is safe because the Cell constructor guarantees
    // that the storage is valid utf8
    let s = unsafe { core::str::from_utf8_unchecked(value.as_bytes()) };
    s.serialize(serializer)
}

/// TeenyString encodes string storage in a single u64.
/// The scheme is simple but effective: strings that encode into a
/// byte slice that is 1 less byte than the machine word size can
/// be encoded directly into the usize bits stored in the struct.
/// A marker bit (LSB for big endian, MSB for little endian) is
/// set to indicate that the string is stored inline.
/// If the string is longer than this then a `Vec<u8>` is allocated
/// from the heap and the usize holds its raw pointer address.
///
/// When the string is inlined, the next-MSB is used to short-cut
/// calling grapheme_column_width; if it is set, then the TeenyString
/// has length 2, otherwise, it has length 1 (we don't allow zero-length
/// strings).
struct TeenyString(u64);
struct TeenyStringHeap {
    bytes: Vec<u8>,
    width: usize,
}

impl TeenyString {
    const fn marker_mask() -> u64 {
        if cfg!(target_endian = "little") {
            0x80000000_00000000
        } else {
            0x1
        }
    }

    const fn double_wide_mask() -> u64 {
        if cfg!(target_endian = "little") {
            0xc0000000_00000000
        } else {
            0x3
        }
    }

    const fn is_marker_bit_set(word: u64) -> bool {
        let mask = Self::marker_mask();
        word & mask == mask
    }

    const fn is_double_width(word: u64) -> bool {
        let mask = Self::double_wide_mask();
        word & mask == mask
    }

    const fn set_marker_bit(word: u64, width: usize) -> u64 {
        if width > 1 {
            word | Self::double_wide_mask()
        } else {
            word | Self::marker_mask()
        }
    }

    pub fn from_str(
        s: &str,
        width: Option<usize>,
        unicode_version: Option<&UnicodeVersion>,
    ) -> Self {
        // De-fang the input text such that it has no special meaning
        // to a terminal.  All control and movement characters are rewritten
        // as a space.
        let s = if s.is_empty() || s == "\r\n" {
            " "
        } else if s.len() == 1 {
            let b = s.as_bytes()[0];
            if b < 0x20 || b == 0x7f { " " } else { s }
        } else {
            s
        };

        let bytes = s.as_bytes();
        let len = bytes.len();
        let width = width.unwrap_or_else(|| grapheme_column_width(s, unicode_version));

        if len < core::mem::size_of::<u64>() && width < 3 {
            let mut word = 0u64;
            unsafe {
                core::ptr::copy_nonoverlapping(
                    bytes.as_ptr(),
                    &mut word as *mut u64 as *mut u8,
                    len,
                );
            }
            let word = Self::set_marker_bit(word as u64, width);
            Self(word)
        } else {
            let vec = Box::new(TeenyStringHeap {
                bytes: bytes.to_vec(),
                width,
            });
            let ptr = Box::into_raw(vec);
            Self(ptr as u64)
        }
    }

    pub const fn space() -> Self {
        Self(if cfg!(target_endian = "little") {
            0x80000000_00000020
        } else {
            0x20000000_00000001
        })
    }

    pub fn from_char(c: char) -> Self {
        let mut bytes = [0u8; 8];
        Self::from_str(c.encode_utf8(&mut bytes), None, None)
    }

    pub fn width(&self) -> usize {
        if Self::is_marker_bit_set(self.0) {
            if Self::is_double_width(self.0) { 2 } else { 1 }
        } else {
            let heap = self.0 as *const u64 as *const TeenyStringHeap;
            unsafe { (*heap).width }
        }
    }

    pub fn str(&self) -> &str {
        // unsafety: this is safe because the constructor guarantees
        // that the storage is valid utf8
        unsafe { core::str::from_utf8_unchecked(self.as_bytes()) }
    }

    pub fn as_bytes(&self) -> &[u8] {
        if Self::is_marker_bit_set(self.0) {
            let bytes = &self.0 as *const u64 as *const u8;
            let bytes =
                unsafe { core::slice::from_raw_parts(bytes, core::mem::size_of::<u64>() - 1) };
            let len = bytes
                .iter()
                .position(|&b| b == 0)
                .unwrap_or(core::mem::size_of::<u64>() - 1);

            &bytes[0..len]
        } else {
            let heap = self.0 as *const u64 as *const TeenyStringHeap;
            unsafe { (*heap).bytes.as_slice() }
        }
    }
}

// Every cell write drops one of these and copies another. Without LTO (the
// release profile that Linux and Windows ship) each was a call across crates
// per cell, a large share of filling or erasing a row; the inline-string
// checks are small enough to inline.
impl Drop for TeenyString {
    #[inline]
    fn drop(&mut self) {
        if !Self::is_marker_bit_set(self.0) {
            let vec = unsafe { Box::from_raw(self.0 as *mut usize as *mut TeenyStringHeap) };
            drop(vec);
        }
    }
}

impl core::clone::Clone for TeenyString {
    #[inline]
    fn clone(&self) -> Self {
        if Self::is_marker_bit_set(self.0) {
            Self(self.0)
        } else {
            Self::from_str(self.str(), None, None)
        }
    }

    /// An inline string is a plain word: overwriting one frees nothing and
    /// copying one allocates nothing.
    #[inline]
    fn clone_from(&mut self, source: &Self) {
        if Self::is_marker_bit_set(self.0) && Self::is_marker_bit_set(source.0) {
            self.0 = source.0;
        } else {
            *self = source.clone();
        }
    }
}

impl core::cmp::PartialEq for TeenyString {
    fn eq(&self, rhs: &Self) -> bool {
        self.as_bytes().eq(rhs.as_bytes())
    }
}
impl core::cmp::Eq for TeenyString {}

/// Models the contents of a cell on the terminal display
#[cfg_attr(feature = "use_serde", derive(Serialize, Deserialize))]
#[derive(Eq, PartialEq)]
pub struct Cell {
    #[cfg_attr(
        feature = "use_serde",
        serde(
            deserialize_with = "deserialize_teenystring",
            serialize_with = "serialize_teenystring"
        )
    )]
    text: TeenyString,
    attrs: CellAttributes,
}

impl Clone for Cell {
    #[inline]
    fn clone(&self) -> Self {
        Self {
            text: self.text.clone(),
            attrs: self.attrs.clone(),
        }
    }

    #[inline]
    fn clone_from(&mut self, source: &Self) {
        self.text.clone_from(&source.text);
        self.attrs.clone_from(&source.attrs);
    }
}

impl core::fmt::Debug for Cell {
    fn fmt(&self, fmt: &mut core::fmt::Formatter<'_>) -> Result<(), core::fmt::Error> {
        fmt.debug_struct("Cell")
            .field("text", &self.str())
            .field("width", &self.width())
            .field("attrs", &self.attrs)
            .finish()
    }
}

impl Default for Cell {
    fn default() -> Self {
        Self::blank()
    }
}

impl Cell {
    /// Create a new cell holding the specified character and with the
    /// specified cell attributes.
    /// All control and movement characters are rewritten as a space.
    pub fn new(text: char, attrs: CellAttributes) -> Self {
        let storage = TeenyString::from_char(text);
        Self {
            text: storage,
            attrs,
        }
    }

    pub const fn blank() -> Self {
        Self {
            text: TeenyString::space(),
            attrs: CellAttributes::blank(),
        }
    }

    pub const fn blank_with_attrs(attrs: CellAttributes) -> Self {
        Self {
            text: TeenyString::space(),
            attrs,
        }
    }

    /// Indicates whether this cell has text or emoji presentation.
    /// The width already reflects that choice; this information
    /// is also useful when selecting an appropriate font.
    pub fn presentation(&self) -> Presentation {
        match Presentation::for_grapheme(self.str()) {
            (_, Some(variation)) => variation,
            (presentation, None) => presentation,
        }
    }

    /// Create a new cell holding the specified grapheme.
    /// The grapheme is passed as a string slice and is intended to hold
    /// double-width characters, or combining unicode sequences, that need
    /// to be treated as a single logical "character" that can be cursored
    /// over.  This function technically allows for an arbitrary string to
    /// be passed but it should not be used to hold strings other than
    /// graphemes.
    pub fn new_grapheme(
        text: &str,
        attrs: CellAttributes,
        unicode_version: Option<&UnicodeVersion>,
    ) -> Self {
        let storage = TeenyString::from_str(text, None, unicode_version);

        Self {
            text: storage,
            attrs,
        }
    }

    pub fn new_grapheme_with_width(text: &str, width: usize, attrs: CellAttributes) -> Self {
        let storage = TeenyString::from_str(text, Some(width), None);
        Self {
            text: storage,
            attrs,
        }
    }

    /// Returns the textual content of the cell
    pub fn str(&self) -> &str {
        self.text.str()
    }

    /// Returns the number of cells visually occupied by this grapheme
    pub fn width(&self) -> usize {
        self.text.width()
    }

    /// Returns the attributes of the cell
    pub fn attrs(&self) -> &CellAttributes {
        &self.attrs
    }

    pub fn attrs_mut(&mut self) -> &mut CellAttributes {
        &mut self.attrs
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UnicodeVersion {
    pub version: u8,
    pub ambiguous_are_wide: bool,
    #[cfg(feature = "std")]
    pub cell_widths: Option<Arc<std::collections::HashMap<u32, u8>>>,
}

impl UnicodeVersion {
    pub const fn new(version: u8) -> Self {
        Self {
            version,
            ambiguous_are_wide: false,
            #[cfg(feature = "std")]
            cell_widths: None,
        }
    }

    #[inline]
    fn width(&self, c: WcWidth) -> usize {
        // Special case for symbol fonts that are naughtly and use
        // the unassigned range instead of the private use range.
        // <https://github.com/wezterm/wezterm/issues/1864>
        if c == WcWidth::Unassigned {
            1
        } else if c == WcWidth::Ambiguous && self.ambiguous_are_wide {
            2
        } else if self.version >= 9 {
            c.width_unicode_9_or_later() as usize
        } else {
            c.width_unicode_8_or_earlier() as usize
        }
    }

    #[inline]
    fn wcwidth(&self, c: char) -> usize {
        #[cfg(feature = "std")]
        if let Some(ref cell_widths) = self.cell_widths {
            if let Some(width) = cell_widths.get(&(c as u32)) {
                return (*width).into();
            }
        }
        self.width(WCWIDTH_TABLE.classify(c))
    }

    #[inline]
    pub fn idx(&self) -> usize {
        (if self.version > 9 { 2 } else { 0 }) | (if self.ambiguous_are_wide { 1 } else { 0 })
    }
}

pub const LATEST_UNICODE_VERSION: UnicodeVersion = UnicodeVersion {
    version: 14,
    ambiguous_are_wide: false,
    #[cfg(feature = "std")]
    cell_widths: None,
};

/// Returns true if the char `c` has the unicode White_Space property
pub fn is_white_space_char(c: char) -> bool {
    wezterm_char_props::white_space::WHITE_SPACE.contains_u32(c as u32)
}

/// Returns true if the grapheme string `g` consists entirely of characters
/// that have the unicode White_Space property.
pub fn is_white_space_grapheme(g: &str) -> bool {
    for c in g.chars() {
        if !is_white_space_char(c) {
            return false;
        }
    }
    true
}

/// Returns the number of cells visually occupied by a sequence
/// of graphemes.
/// Calls through to `grapheme_column_width` for each grapheme
/// and sums up the length.
pub fn unicode_column_width(s: &str, version: Option<&UnicodeVersion>) -> usize {
    Graphemes::new(s)
        .map(|g| grapheme_column_width(g, version))
        .sum()
}

/// Returns the number of cells visually occupied by a grapheme.
/// The input string must be a single grapheme.
///
/// There are some frustrating dragons in the realm of terminal cell widths:
///
/// a) wcwidth and wcswidth are widely used by applications and may be
///    several versions of unicode behind the current version
/// b) The width of characters has and will change in the future.
///    Unicode Version 8 -> 9 made some characters wider.
///    Unicode 14 defines Emoji variation selectors that change the
///    width depending on trailing context in the unicode sequence.
///
/// Differing opinions about the width leads to visual artifacts in
/// text and and line editors, especially with respect to cursor placement.
///
/// There aren't any really great solutions to this problem, as a given
/// terminal emulator may be fine locally but essentially breaks when
/// ssh'ing into a remote system with a divergent wcwidth implementation.
///
/// This means that a global understanding of the unicode version that
/// is in use isn't a good solution.
///
/// The approach that wezterm wants to take here is to define a
/// configuration value that sets the starting level of unicode conformance,
/// and to define an escape sequence that can push/pop a desired confirmance
/// level onto a stack maintained by the terminal emulator.
///
/// The terminal emulator can then pass the unicode version through to
/// the Cell that is used to hold a grapheme, and that per-Cell version
/// can then be used to calculate width.
pub fn grapheme_column_width(s: &str, version: Option<&UnicodeVersion>) -> usize {
    let version = version.as_deref().unwrap_or(&LATEST_UNICODE_VERSION);

    // Optimization: if there is a single byte we can directly cast
    // that byte as a char which will be in the range 0.255.
    // This takes ~1.5ns, and we can then look that up in the table
    // which is valid for chars in the range 0-0xffff.
    // That lookup also takes ~1.5ns, giving us a hot path latency
    // of ~3-4ns for a grapheme string that is comprised of a single
    // ASCII byte.
    //
    // Since we know this is a single ASCII char, we know that it
    // cannot be a sequence with a variation selector, so we don't
    // need to requested `Presentation` for it.
    if s.len() == 1 {
        return version.wcwidth(s.as_bytes()[0] as char);
    }

    // Decomposed Latin text (NFD, as in many macOS file names) is a
    // printable ASCII character followed by combining diacritical marks,
    // U+0300..=U+036F. Those marks have no width and select no emoji
    // presentation in any Unicode version, so the base decides: one cell.
    // Configured cell widths may say otherwise, so they take the general
    // path. `decomposed_latin_matches_the_general_path` checks this.
    #[cfg(feature = "std")]
    let custom_widths = version.cell_widths.is_some();
    #[cfg(not(feature = "std"))]
    let custom_widths = false;
    if !custom_widths && is_ascii_with_combining_diacritics(s.as_bytes()) {
        return 1;
    }

    general_column_width(s, version)
}

/// A printable ASCII character followed by at least one combining
/// diacritical mark, U+0300..=U+036F (UTF-8 `CC 80..=BF` or `CD 80..=AF`).
fn is_ascii_with_combining_diacritics(bytes: &[u8]) -> bool {
    let Some((base, marks)) = bytes.split_first() else {
        return false;
    };
    (b' '..=b'~').contains(base)
        && !marks.is_empty()
        && marks.len() % 2 == 0
        && marks
            .chunks_exact(2)
            .all(|mark| matches!(mark, [0xcc, 0x80..=0xbf] | [0xcd, 0x80..=0xaf]))
}

fn general_column_width(s: &str, version: &UnicodeVersion) -> usize {
    // Slow path: `s.chars()` will dominate and pull up the minimum
    // runtime to ~20ns
    if version.version >= 14 {
        // Lookup the grapheme to see if the presentation of
        // the grapheme forces the width. We can bypass
        // the WcWidth classification if that is true.
        match Presentation::for_grapheme(s) {
            (_, Some(Presentation::Emoji)) => return 2,
            (_, Some(Presentation::Text)) => return 1,
            (Presentation::Emoji, None) => return 2,
            (Presentation::Text, None) => {}
        }
    }

    // Otherwise, classify and sum up
    let mut width = 0;
    for c in s.chars() {
        width += version.wcwidth(c);
    }

    width.min(2)
}

/// Models a change in the attributes of a cell in a stream of changes.
/// Each variant specifies one of the possible attributes; the corresponding
/// value holds the new value to be used for that attribute.
#[cfg_attr(feature = "use_serde", derive(Serialize, Deserialize))]
#[derive(Debug, Clone, Eq, PartialEq, FromDynamic, ToDynamic)]
pub enum AttributeChange {
    Intensity(Intensity),
    Underline(Underline),
    Italic(bool),
    Blink(Blink),
    Reverse(bool),
    StrikeThrough(bool),
    Invisible(bool),
    Foreground(ColorAttribute),
    Background(ColorAttribute),
    Hyperlink(Option<Arc<Hyperlink>>),
}

#[cfg(test)]
mod test {
    use super::*;

    fn rgb_attrs(red: f32) -> CellAttributes {
        let mut attrs = CellAttributes::blank();
        attrs.set_foreground(ColorAttribute::TrueColorWithDefaultFallback(
            (red, 0.25, 0.5, 1.0).into(),
        ));
        attrs
    }

    fn shares(a: &CellAttributes, b: &CellAttributes) -> bool {
        match (&a.fat, &b.fat) {
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            _ => false,
        }
    }

    #[test]
    fn decomposed_latin_matches_the_general_path() {
        let mut versions = vec![];
        for version in [8, 9, 14] {
            for ambiguous_are_wide in [false, true] {
                let mut v = UnicodeVersion::new(version);
                v.ambiguous_are_wide = ambiguous_are_wide;
                versions.push(v);
            }
        }
        let check = |s: &str| {
            assert!(is_ascii_with_combining_diacritics(s.as_bytes()), "{s:?}");
            for version in &versions {
                assert_eq!(
                    grapheme_column_width(s, Some(version)),
                    general_column_width(s, version),
                    "{s:?} in Unicode {} (ambiguous wide: {})",
                    version.version,
                    version.ambiguous_are_wide
                );
            }
        };
        for base in b' '..=b'~' {
            for mark in '\u{300}'..='\u{36f}' {
                for count in 1..=3 {
                    let s: String = core::iter::once(base as char)
                        .chain(core::iter::repeat(mark).take(count))
                        .collect();
                    check(&s);
                }
            }
        }
        for base in ['a', ' '] {
            for first in '\u{300}'..='\u{36f}' {
                for second in '\u{300}'..='\u{36f}' {
                    check(&[base, first, second].iter().collect::<String>());
                }
            }
        }
        // Neighbouring text keeps the general path.
        for s in [
            "a",
            "ab",
            "\u{7f}\u{301}",
            "\u{1f}\u{301}",
            "a\u{2ff}",
            "a\u{36f}\u{370}",
            "a\u{301}\u{fe0f}",
            "\u{e9}\u{301}",
        ] {
            assert!(!is_ascii_with_combining_diacritics(s.as_bytes()), "{s:?}");
        }
        // Configured widths still apply to the marks.
        let mut custom = UnicodeVersion::new(14);
        custom.cell_widths = Some(Arc::new([(0x301, 1u8)].into_iter().collect()));
        assert_eq!(grapheme_column_width("a\u{301}", Some(&custom)), 2);
    }

    #[test]
    fn copies_share_extended_attributes_until_one_changes() {
        let link = Arc::new(Hyperlink::new("https://example.com"));
        let other = Arc::new(Hyperlink::new("https://example.org"));
        let mut source = rgb_attrs(0.7);
        source.set_hyperlink(Some(Arc::clone(&link)));
        source.set_underline_color(ColorAttribute::PaletteIndex(2));
        let mut dest = rgb_attrs(0.1);
        dest.clone_from(&source);
        assert_eq!(dest, source);
        assert!(shares(&dest, &source));
        assert!(shares(&source.clone(), &source));

        // Every change leaves the other holders as they were.
        let changes: [&dyn Fn(&mut CellAttributes); 9] = [
            &|a| {
                a.set_foreground(ColorAttribute::TrueColorWithDefaultFallback(
                    (0.9, 0.1, 0.1, 1.0).into(),
                ));
            },
            &|a| {
                a.set_foreground(ColorAttribute::PaletteIndex(5));
            },
            &|a| {
                a.set_foreground(ColorAttribute::Default);
            },
            &|a| {
                a.set_background(ColorAttribute::TrueColorWithDefaultFallback(
                    (0.2, 0.3, 0.4, 1.0).into(),
                ));
            },
            &|a| {
                a.set_underline_color(ColorAttribute::Default);
            },
            &|a| {
                a.set_hyperlink(None);
            },
            &|a| {
                a.set_hyperlink(Some(Arc::clone(&other)));
            },
            &|a| {
                a.set_intensity(Intensity::Bold);
            },
            &|a| {
                *a = a.clone_sgr_only();
            },
        ];
        for change in changes.iter() {
            let before = source.clone();
            let mut changed = source.clone();
            change(&mut changed);
            let mut expected = rgb_attrs(0.7);
            expected.set_hyperlink(Some(Arc::clone(&link)));
            expected.set_underline_color(ColorAttribute::PaletteIndex(2));
            assert_eq!(source, expected);
            assert_eq!(before, expected);
            // The same change on an unshared value gives the same result.
            let mut alone = expected.clone();
            alone.fat = alone.fat.as_deref().cloned().map(Arc::new);
            change(&mut alone);
            assert_eq!(alone, changed);
        }

        // Setting what is already there keeps sharing.
        let mut same = source.clone();
        same.set_foreground(source.foreground());
        same.set_hyperlink(Some(Arc::clone(&link)));
        same.set_underline_color(ColorAttribute::PaletteIndex(2));
        assert!(shares(&same, &source));

        // A sole holder changes in place.
        let mut alone = rgb_attrs(0.3);
        let at = Arc::as_ptr(alone.fat.as_ref().unwrap());
        alone.set_background(ColorAttribute::TrueColorWithDefaultFallback(
            (0.5, 0.5, 0.5, 1.0).into(),
        ));
        assert_eq!(at, Arc::as_ptr(alone.fat.as_ref().unwrap()));

        // Back to defaults, no extended attributes are kept.
        dest.set_hyperlink(None);
        dest.set_underline_color(ColorAttribute::Default);
        dest.set_foreground(ColorAttribute::Default);
        assert!(dest.fat.is_none());
        assert_eq!(source.hyperlink(), Some(&link));
        dest.clone_from(&CellAttributes::blank());
        assert!(dest.fat.is_none());
        drop((source, dest, same));
        assert_eq!(Arc::strong_count(&link), 1);
    }

    #[test]
    #[cfg(feature = "use_image")]
    fn shared_images_are_copied_on_change_and_released() {
        use crate::image::{ImageCell, ImageData, ImageDataType, TextureCoordinate};
        let data = Arc::new(ImageData::with_data(ImageDataType::new_single_frame(
            1, 1, vec![255; 4],
        )));
        let image = |id| {
            Box::new(ImageCell::with_z_index(
                TextureCoordinate::new_f32(0.0, 0.0),
                TextureCoordinate::new_f32(1.0, 1.0),
                Arc::clone(&data),
                0,
                0,
                0,
                0,
                0,
                Some(id),
                Some(7),
            ))
        };
        let mut source = rgb_attrs(0.7);
        source.attach_image(image(3));
        let copied = source.clone();
        let mut dest = rgb_attrs(0.1);
        dest.clone_from(&source);
        assert!(shares(&dest, &source) && shares(&copied, &source));
        assert_eq!(Arc::strong_count(&data), 2);

        // Changing one holder's images copies the list for it alone.
        dest.attach_image(image(4));
        assert_eq!(dest.images().unwrap().len(), 2);
        assert_eq!(source.images().unwrap().len(), 1);
        assert_eq!(Arc::strong_count(&data), 4);
        dest.detach_image_with_placement(4, Some(7));
        assert_eq!(dest, source);
        assert_eq!(Arc::strong_count(&data), 3);
        // Detaching nothing leaves the list shared.
        let mut unmatched = source.clone();
        unmatched.detach_image_with_placement(9, Some(7));
        assert!(shares(&unmatched, &source));
        drop(unmatched);

        let mut cleared = source.clone();
        cleared.clear_images();
        assert!(!cleared.has_images() && source.has_images());
        assert_eq!(cleared.foreground(), source.foreground());
        let mut only_image = CellAttributes::blank();
        only_image.set_image(image(5));
        let mut emptied = only_image.clone();
        emptied.clear_images();
        assert!(emptied.fat.is_none() && only_image.has_images());

        drop((source, copied, dest, cleared, only_image, emptied));
        assert_eq!(Arc::strong_count(&data), 1);
    }

    #[test]
    fn cell_clone_from_matches_replacement_across_text_and_style_changes() {
        let cells = [
            Cell::blank(),
            Cell::new('a', rgb_attrs(0.3)),
            Cell::new_grapheme("界", CellAttributes::blank(), None),
            Cell::new_grapheme("👩🏿‍🤝‍👩🏿", rgb_attrs(0.7), None),
        ];
        for initial in &cells {
            let mut dest = initial.clone();
            for source in &cells {
                dest.clone_from(source);
                assert_eq!(dest, source.clone());
                assert_eq!(dest.width(), source.clone().width());
            }
        }
    }

    #[test]
    fn teeny_string() {
        assert!(
            core::mem::size_of::<usize>() <= core::mem::size_of::<u64>(),
            "if a pointer doesn't fit in u64 then we need to change TeenyString"
        );

        let s = TeenyString::from_char('a');
        assert_eq!(s.as_bytes(), &[b'a']);

        let longer = TeenyString::from_str("hellothere", None, None);
        assert_eq!(longer.as_bytes(), b"hellothere");

        assert_eq!(
            TeenyString::from_char(' ').as_bytes(),
            TeenyString::space().as_bytes()
        );
    }

    #[test]
    #[cfg(target_pointer_width = "64")]
    fn memory_usage() {
        assert_eq!(core::mem::size_of::<crate::color::RgbColor>(), 4);
        assert_eq!(core::mem::size_of::<ColorAttribute>(), 20);
        assert_eq!(core::mem::size_of::<CellAttributes>(), 16);
        assert_eq!(core::mem::size_of::<Cell>(), 24);
        assert_eq!(core::mem::size_of::<Vec<u8>>(), 24);
        assert_eq!(core::mem::size_of::<char>(), 4);
        assert_eq!(core::mem::size_of::<TeenyString>(), 8);
    }

    #[test]
    fn nerf_special() {
        for c in " \n\r\t".chars() {
            let cell = Cell::new(c, CellAttributes::default());
            assert_eq!(cell.str(), " ");
        }

        for g in &["", " ", "\n", "\r", "\t", "\r\n"] {
            let cell = Cell::new_grapheme(g, CellAttributes::default(), None);
            assert_eq!(cell.str(), " ");
        }
    }

    #[test]
    fn test_width() {
        let foot = "\u{1f9b6}";
        eprintln!("foot chars");
        for c in foot.chars() {
            eprintln!("char: {:?}", c);
        }
        assert_eq!(unicode_column_width(foot, None), 2, "{} should be 2", foot);

        let women_holding_hands_dark_skin_tone_medium_light_skin_tone =
            "\u{1F469}\u{1F3FF}\u{200D}\u{1F91D}\u{200D}\u{1F469}\u{1F3FC}";

        // Ensure that we can hold this longer grapheme sequence in the cell
        // and correctly return its string contents!
        let cell = Cell::new_grapheme(
            women_holding_hands_dark_skin_tone_medium_light_skin_tone,
            CellAttributes::default(),
            None,
        );
        assert_eq!(
            cell.str(),
            women_holding_hands_dark_skin_tone_medium_light_skin_tone
        );
        assert_eq!(
            cell.width(),
            2,
            "width of {} should be 2",
            women_holding_hands_dark_skin_tone_medium_light_skin_tone
        );

        let deaf_man = "\u{1F9CF}\u{200D}\u{2642}\u{FE0F}";
        eprintln!("deaf_man chars");
        for c in deaf_man.chars() {
            eprintln!("char: {:?}", c);
        }
        assert_eq!(unicode_column_width(deaf_man, None), 2);

        let man_dancing = "\u{1F57A}";
        assert_eq!(
            unicode_column_width(man_dancing, Some(&UnicodeVersion::new(9))),
            2
        );
        assert_eq!(
            unicode_column_width(man_dancing, Some(&UnicodeVersion::new(8))),
            2
        );

        let raised_fist = "\u{270a}";
        assert_eq!(
            unicode_column_width(raised_fist, Some(&UnicodeVersion::new(9))),
            2
        );
        assert_eq!(
            unicode_column_width(raised_fist, Some(&UnicodeVersion::new(8))),
            1
        );

        // This is a codepoint in the private use area
        let font_awesome_star = "\u{f005}";
        eprintln!("font_awesome_star {}", font_awesome_star.escape_debug());
        assert_eq!(unicode_column_width(font_awesome_star, None), 1);

        let england_flag = "\u{1f3f4}\u{e0067}\u{e0062}\u{e0065}\u{e006e}\u{e0067}\u{e007f}";
        assert_eq!(unicode_column_width(england_flag, None), 2);
    }

    #[test]
    fn issue_1161() {
        let x_ideographic_space_x = "x\u{3000}x";
        assert_eq!(unicode_column_width(x_ideographic_space_x, None), 4);
        assert_eq!(
            Graphemes::new(x_ideographic_space_x).collect::<Vec<_>>(),
            vec!["x".to_string(), "\u{3000}".to_string(), "x".to_string()],
        );

        let c = Cell::new_grapheme("\u{3000}", CellAttributes::blank(), None);
        assert_eq!(c.width(), 2);
    }

    #[test]
    fn issue_997() {
        let victory_hand = "\u{270c}";
        let victory_hand_text_presentation = "\u{270c}\u{fe0e}";

        assert_eq!(
            unicode_column_width(victory_hand_text_presentation, None),
            1
        );
        assert_eq!(unicode_column_width(victory_hand, None), 1);

        assert_eq!(
            Graphemes::new(victory_hand_text_presentation).collect::<Vec<_>>(),
            vec![victory_hand_text_presentation.to_string()]
        );
        assert_eq!(
            Graphemes::new(victory_hand).collect::<Vec<_>>(),
            vec![victory_hand.to_string()]
        );

        let copyright_emoji_presentation = "\u{00A9}\u{FE0F}";
        assert_eq!(
            Graphemes::new(copyright_emoji_presentation).collect::<Vec<_>>(),
            vec![copyright_emoji_presentation.to_string()]
        );
        assert_eq!(unicode_column_width(copyright_emoji_presentation, None), 2);
        assert_eq!(
            unicode_column_width(copyright_emoji_presentation, Some(&UnicodeVersion::new(9))),
            1
        );

        let copyright_text_presentation = "\u{00A9}";
        assert_eq!(
            Graphemes::new(copyright_text_presentation).collect::<Vec<_>>(),
            vec![copyright_text_presentation.to_string()]
        );
        assert_eq!(unicode_column_width(copyright_text_presentation, None), 1);

        let raised_fist = "\u{270a}";
        // Not valid to have explicit Text presentation for raised fist
        let raised_fist_text = "\u{270a}\u{fe0e}";
        assert_eq!(
            Presentation::for_grapheme(raised_fist),
            (Presentation::Emoji, None)
        );
        assert_eq!(unicode_column_width(raised_fist, None), 2);
        assert_eq!(
            Presentation::for_grapheme(raised_fist_text),
            (Presentation::Emoji, None)
        );
        assert_eq!(unicode_column_width(raised_fist_text, None), 2);

        assert_eq!(
            Graphemes::new(raised_fist_text).collect::<Vec<_>>(),
            vec![raised_fist_text.to_string()]
        );
        assert_eq!(
            Graphemes::new(raised_fist).collect::<Vec<_>>(),
            vec![raised_fist.to_string()]
        );
    }

    #[test]
    fn issue_1573() {
        let sequence = "\u{1112}\u{1161}\u{11ab}";
        assert_eq!(unicode_column_width(sequence, None), 2);
        assert_eq!(grapheme_column_width(sequence, None), 2);

        let sequence2 = core::str::from_utf8(b"\xe1\x84\x92\xe1\x85\xa1\xe1\x86\xab").unwrap();
        assert_eq!(unicode_column_width(sequence2, None), 2);
        assert_eq!(grapheme_column_width(sequence2, None), 2);
    }

    // See <https://github.com/wezterm/wezterm/issues/6637>
    // We're not directly "fixing" that issue here in termwiz at this time
    // because it isn't clear that this cell module has enough context
    // to eg: decide that the width of U+2028 should be returned as 1.
    // That decision is made over in wezterm-term when processing
    // a sequence of graphemes. This test case is just making assertions
    // about the properties of a couple of problematic zero-width
    // characters.
    #[test]
    fn issue_6637() {
        // U+2028 is the unicode line separator. It is Non-printing White_Space.
        let sequence = "\u{2028}";
        // It has zero width
        assert_eq!(unicode_column_width(sequence, None), 0);
        assert_eq!(grapheme_column_width(sequence, None), 0);
        // it is white space
        assert!(is_white_space_grapheme(sequence));

        // Just a couple of sanity checks for the white space function
        assert!(is_white_space_char(' '));
        assert!(!is_white_space_char('x'));

        // U+2068 is a BIDI control character and is relevant here
        // due to <https://github.com/wezterm/wezterm/issues/1422>.
        // It is Non-Printing, non-White_Space
        assert!(!is_white_space_char('\u{2068}'));
    }
}
