//! Everything a terminal remembers, in a form that can leave the process.
//!
//! A mux server that is being replaced hands each pane to the new binary:
//! the pty goes as a file descriptor, and what the old process had parsed
//! out of it goes as one of these. The snapshot carries both screens with
//! their scrollback, the cursor, every mode an application can set, the
//! titles and variables it announced, and the pictures on screen -- each
//! picture once, however many cells and kitty ids share it.
//!
//! Not carried, on purpose: the configuration and the writer (the new
//! process has its own), the clipboard and notification handlers (the pane
//! reinstalls them), the decode cache, mouse buttons held at that instant,
//! a title still accumulating, the parser's position inside an escape
//! sequence (the sender drains the pty to a quiet point first), and a
//! kitty chunked transfer still in flight (flagged, so the sender can say
//! so).

use super::{
    CharSet, MouseEncoding, SavedCursor, TabStop, TerminalState, UnicodeVersionStackEntry,
};
use crate::color::ColorPalette;
use crate::screen::Screen;
use crate::terminalstate::image::{KittyPlacementSnapshot, PlacementInfo};
use crate::{CursorPosition, Progress, TerminalSize};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use termwiz::input::KeyboardEncoding;
use url::Url;
use wezterm_bidi::ParagraphDirectionHint;
use wezterm_cell::image::{ImageCell, ImageData, ImageDataType, TextureCoordinate};
use wezterm_cell::{CellAttributes, UnicodeVersion};
use wezterm_escape_parser::csi::KittyKeyboardFlags;
use wezterm_surface::{Line, SequenceNo};

/// Bumped whenever the shape of [`TerminalSnapshot`] changes; `restore`
/// refuses any other number rather than guess at the fields.
pub const SNAPSHOT_VERSION: u32 = 1;

/// What a terminal remembers. Build one with [`TerminalState::snapshot`]
/// and unpack it into a fresh terminal of the same size with
/// [`TerminalState::restore`].
#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub struct TerminalSnapshot {
    pub version: u32,
    pub size: TerminalSize,
    pub seqno: SequenceNo,
    pub screen: ScreenSnapshot,
    pub alt_screen: ScreenSnapshot,
    pub alt_screen_is_active: bool,
    /// Every picture a cell or a kitty id refers to, once per content hash,
    /// sorted by hash.
    pub images: Vec<SnapshotImage>,
    pub cursor: CursorSnapshot,
    pub modes: ModeSnapshot,
    pub identity: IdentitySnapshot,
    pub kitty: KittySnapshot,
}

/// One screen: the primary with its scrollback, or the alternate.
#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub struct ScreenSnapshot {
    /// Scrollback first, visible rows last, as the screen keeps them.
    /// Pictures are detached; `image_cells` says where they were.
    pub lines: Vec<Line>,
    pub image_cells: Vec<SnapshotImageCell>,
    pub stable_row_index_offset: usize,
    pub physical_rows: usize,
    pub physical_cols: usize,
    pub dpi: u32,
    pub keyboard_stack: Vec<SnapshotKeyboardEncoding>,
    pub saved_cursor: Option<SavedCursor>,
}

/// A picture's place in a cell, with the picture itself replaced by its
/// hash. Keeps the kitty image and placement ids, which the wire format's
/// image cell drops: a delete-by-id after the restore has to find them.
#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub struct SnapshotImageCell {
    /// Index into `ScreenSnapshot::lines`.
    pub line_idx: usize,
    pub cell_idx: usize,
    pub hash: [u8; 32],
    pub top_left: TextureCoordinate,
    pub bottom_right: TextureCoordinate,
    pub z_index: i32,
    pub padding_left: u16,
    pub padding_top: u16,
    pub padding_right: u16,
    pub padding_bottom: u16,
    pub image_id: Option<u32>,
    pub placement_id: Option<u32>,
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub struct SnapshotImage {
    pub hash: [u8; 32],
    pub data: ImageDataType,
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub struct CursorSnapshot {
    pub position: CursorPosition,
    pub pen: CellAttributes,
    pub wrap_next: bool,
    pub insert: bool,
    pub cursor_visible: bool,
}

/// Every mode an application can set, one field each, named as the
/// terminal names them so a missing one is easy to spot against
/// `TerminalState`.
#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub struct ModeSnapshot {
    pub dec_auto_wrap: bool,
    pub reverse_wraparound_mode: bool,
    pub reverse_video_mode: bool,
    pub dec_origin_mode: bool,
    pub top_and_bottom_margins: std::ops::Range<i64>,
    pub left_and_right_margins: std::ops::Range<usize>,
    pub left_and_right_margin_mode: bool,
    pub application_cursor_keys: bool,
    pub application_keypad: bool,
    pub modify_other_keys: Option<i64>,
    pub dec_ansi_mode: bool,
    pub sixel_display_mode: bool,
    pub sixel_scrolls_right: bool,
    pub use_private_color_registers_for_each_graphic: bool,
    pub bracketed_paste: bool,
    pub any_event_mouse: bool,
    pub button_event_mouse: bool,
    pub mouse_tracking: bool,
    pub focus_tracking: bool,
    pub mouse_encoding: MouseEncoding,
    pub keyboard_encoding: SnapshotKeyboardEncoding,
    pub g0_charset: CharSet,
    pub g1_charset: CharSet,
    pub shift_out: bool,
    pub newline_mode: bool,
    pub clear_semantic_attribute_on_newline: bool,
    pub tabs: TabStop,
    pub unicode_version: SnapshotUnicodeVersion,
    pub unicode_version_stack: Vec<SnapshotUnicodeVersionStackEntry>,
    pub bidi_enabled: Option<bool>,
    pub bidi_hint: Option<SnapshotBidiHint>,
    pub focused: bool,
}

/// What the applications announced about themselves.
#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub struct IdentitySnapshot {
    pub title: String,
    pub icon_title: Option<String>,
    /// OSC 7, as the URL text.
    pub current_dir: Option<String>,
    pub user_vars: BTreeMap<String, String>,
    pub progress: Progress,
    pub agent_osc_title: Option<String>,
    pub agent_osc_progress: Option<String>,
    pub palette: Option<ColorPalette>,
    /// The sixel / regis colour registers, as 8-bit RGB.
    pub color_map: BTreeMap<u16, (u8, u8, u8)>,
}

/// The kitty graphics bookkeeping: which ids name which pictures (by
/// hash, see `TerminalSnapshot::images`), their transmission order, and
/// where each placement sits.
#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub struct KittySnapshot {
    pub max_image_id: u32,
    pub number_to_id: BTreeMap<u32, u32>,
    pub id_to_hash: BTreeMap<u32, [u8; 32]>,
    pub id_seq: BTreeMap<u32, u64>,
    pub next_seq: u64,
    pub placements: BTreeMap<(u32, Option<u32>), PlacementInfo>,
    /// A chunked transfer had begun and not finished when the snapshot was
    /// taken. Its fragments are not carried: the picture is lost, and the
    /// program will see an error for the rest of it.
    pub transmission_in_progress: bool,
}

/// `termwiz::input::KeyboardEncoding` without serde; the kitty flags travel
/// as their bits.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotKeyboardEncoding {
    Xterm,
    CsiU,
    Win32,
    Kitty(u16),
}

impl From<KeyboardEncoding> for SnapshotKeyboardEncoding {
    fn from(encoding: KeyboardEncoding) -> Self {
        match encoding {
            KeyboardEncoding::Xterm => Self::Xterm,
            KeyboardEncoding::CsiU => Self::CsiU,
            KeyboardEncoding::Win32 => Self::Win32,
            KeyboardEncoding::Kitty(flags) => Self::Kitty(flags.bits()),
        }
    }
}

impl From<SnapshotKeyboardEncoding> for KeyboardEncoding {
    fn from(encoding: SnapshotKeyboardEncoding) -> Self {
        match encoding {
            SnapshotKeyboardEncoding::Xterm => Self::Xterm,
            SnapshotKeyboardEncoding::CsiU => Self::CsiU,
            SnapshotKeyboardEncoding::Win32 => Self::Win32,
            SnapshotKeyboardEncoding::Kitty(bits) => {
                Self::Kitty(KittyKeyboardFlags::from_bits_truncate(bits))
            }
        }
    }
}

/// `wezterm_cell::UnicodeVersion` without serde.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct SnapshotUnicodeVersion {
    pub version: u8,
    pub ambiguous_are_wide: bool,
    pub cell_widths: Option<BTreeMap<u32, u8>>,
}

impl From<&UnicodeVersion> for SnapshotUnicodeVersion {
    fn from(vers: &UnicodeVersion) -> Self {
        Self {
            version: vers.version,
            ambiguous_are_wide: vers.ambiguous_are_wide,
            cell_widths: vers
                .cell_widths
                .as_ref()
                .map(|widths| widths.iter().map(|(k, v)| (*k, *v)).collect()),
        }
    }
}

impl From<SnapshotUnicodeVersion> for UnicodeVersion {
    fn from(vers: SnapshotUnicodeVersion) -> Self {
        Self {
            version: vers.version,
            ambiguous_are_wide: vers.ambiguous_are_wide,
            cell_widths: vers
                .cell_widths
                .map(|widths| Arc::new(widths.into_iter().collect::<HashMap<u32, u8>>())),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct SnapshotUnicodeVersionStackEntry {
    pub vers: SnapshotUnicodeVersion,
    pub label: Option<String>,
}

/// `wezterm_bidi::ParagraphDirectionHint` without serde.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotBidiHint {
    LeftToRight,
    RightToLeft,
    AutoLeftToRight,
    AutoRightToLeft,
}

impl From<ParagraphDirectionHint> for SnapshotBidiHint {
    fn from(hint: ParagraphDirectionHint) -> Self {
        match hint {
            ParagraphDirectionHint::LeftToRight => Self::LeftToRight,
            ParagraphDirectionHint::RightToLeft => Self::RightToLeft,
            ParagraphDirectionHint::AutoLeftToRight => Self::AutoLeftToRight,
            ParagraphDirectionHint::AutoRightToLeft => Self::AutoRightToLeft,
        }
    }
}

impl From<SnapshotBidiHint> for ParagraphDirectionHint {
    fn from(hint: SnapshotBidiHint) -> Self {
        match hint {
            SnapshotBidiHint::LeftToRight => Self::LeftToRight,
            SnapshotBidiHint::RightToLeft => Self::RightToLeft,
            SnapshotBidiHint::AutoLeftToRight => Self::AutoLeftToRight,
            SnapshotBidiHint::AutoRightToLeft => Self::AutoRightToLeft,
        }
    }
}

/// Pictures met while walking cells and kitty ids, kept once per hash.
#[derive(Default)]
pub(crate) struct ImageTable {
    images: BTreeMap<[u8; 32], ImageDataType>,
}

impl ImageTable {
    pub(crate) fn remember(&mut self, data: &Arc<ImageData>) -> [u8; 32] {
        let hash = data.hash();
        self.images
            .entry(hash)
            .or_insert_with(|| data.data().clone());
        hash
    }

    fn into_snapshot(self) -> Vec<SnapshotImage> {
        self.images
            .into_iter()
            .map(|(hash, data)| SnapshotImage { hash, data })
            .collect()
    }
}

impl TerminalState {
    /// Everything this terminal remembers; see the module documentation
    /// for what is left behind.
    pub fn snapshot(&self) -> TerminalSnapshot {
        let mut images = ImageTable::default();
        let screen = snapshot_screen(&self.screen.screen, &mut images);
        let alt_screen = snapshot_screen(&self.screen.alt_screen, &mut images);
        let kitty = self.kitty_img.snapshot(&mut images);
        TerminalSnapshot {
            version: SNAPSHOT_VERSION,
            size: self.get_size(),
            seqno: self.seqno,
            screen,
            alt_screen,
            alt_screen_is_active: self.screen.alt_screen_is_active,
            images: images.into_snapshot(),
            cursor: CursorSnapshot {
                position: self.cursor,
                pen: self.pen.clone(),
                wrap_next: self.wrap_next,
                insert: self.insert,
                cursor_visible: self.cursor_visible,
            },
            modes: ModeSnapshot {
                dec_auto_wrap: self.dec_auto_wrap,
                reverse_wraparound_mode: self.reverse_wraparound_mode,
                reverse_video_mode: self.reverse_video_mode,
                dec_origin_mode: self.dec_origin_mode,
                top_and_bottom_margins: self.top_and_bottom_margins.clone(),
                left_and_right_margins: self.left_and_right_margins.clone(),
                left_and_right_margin_mode: self.left_and_right_margin_mode,
                application_cursor_keys: self.application_cursor_keys,
                application_keypad: self.application_keypad,
                modify_other_keys: self.modify_other_keys,
                dec_ansi_mode: self.dec_ansi_mode,
                sixel_display_mode: self.sixel_display_mode,
                sixel_scrolls_right: self.sixel_scrolls_right,
                use_private_color_registers_for_each_graphic: self
                    .use_private_color_registers_for_each_graphic,
                bracketed_paste: self.bracketed_paste,
                any_event_mouse: self.any_event_mouse,
                button_event_mouse: self.button_event_mouse,
                mouse_tracking: self.mouse_tracking,
                focus_tracking: self.focus_tracking,
                mouse_encoding: self.mouse_encoding,
                keyboard_encoding: self.keyboard_encoding.into(),
                g0_charset: self.g0_charset,
                g1_charset: self.g1_charset,
                shift_out: self.shift_out,
                newline_mode: self.newline_mode,
                clear_semantic_attribute_on_newline: self.clear_semantic_attribute_on_newline,
                tabs: self.tabs.clone(),
                unicode_version: (&self.unicode_version).into(),
                unicode_version_stack: self
                    .unicode_version_stack
                    .iter()
                    .map(|entry| SnapshotUnicodeVersionStackEntry {
                        vers: (&entry.vers).into(),
                        label: entry.label.clone(),
                    })
                    .collect(),
                bidi_enabled: self.bidi_enabled,
                bidi_hint: self.bidi_hint.map(Into::into),
                focused: self.focused,
            },
            identity: IdentitySnapshot {
                title: self.title.clone(),
                icon_title: self.icon_title.clone(),
                current_dir: self.current_dir.as_ref().map(|url| url.to_string()),
                user_vars: self
                    .user_vars
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect(),
                progress: self.progress.clone(),
                agent_osc_title: self.agent_osc_title.clone(),
                agent_osc_progress: self.agent_osc_progress.clone(),
                palette: self.palette.clone(),
                color_map: self
                    .color_map
                    .iter()
                    .map(|(idx, color)| (*idx, color.to_tuple_rgb8()))
                    .collect(),
            },
            kitty,
        }
    }

    /// Make this terminal what `snapshot` remembers. Call it on a terminal
    /// just made by `Terminal::new` with the snapshot's size, the new
    /// process's configuration and writer, and nothing fed to it yet.
    /// Scrollback beyond what this configuration allows is dropped from the
    /// oldest end, with the stable row offset advanced to match, so stable
    /// row indices keep their meaning.
    pub fn restore(&mut self, snapshot: TerminalSnapshot) -> anyhow::Result<()> {
        self.restore_with_kitty_placements(snapshot, None)
    }

    pub fn restore_with_kitty_placements(
        &mut self,
        snapshot: TerminalSnapshot,
        placements: Option<KittyPlacementSnapshot>,
    ) -> anyhow::Result<()> {
        if let Some(placements) = &placements {
            validate_kitty_placements(&snapshot, placements)?;
        }
        anyhow::ensure!(
            snapshot.version == SNAPSHOT_VERSION,
            "terminal snapshot version {} cannot be restored by a terminal that speaks version {}",
            snapshot.version,
            SNAPSHOT_VERSION
        );
        let size = self.get_size();
        anyhow::ensure!(
            size.rows == snapshot.size.rows && size.cols == snapshot.size.cols,
            "the snapshot is {}x{} but this terminal is {}x{}: make the terminal with the snapshot's size first",
            snapshot.size.cols,
            snapshot.size.rows,
            size.cols,
            size.rows
        );

        let images: HashMap<[u8; 32], Arc<ImageData>> = snapshot
            .images
            .into_iter()
            .map(|image| {
                (
                    image.hash,
                    Arc::new(ImageData::with_data_and_hash(image.data, image.hash)),
                )
            })
            .collect();

        restore_screen(&mut self.screen.screen, snapshot.screen, &images,
            placements.as_ref().map(|p| p.cell_tags[0].as_slice()))?;
        restore_screen(&mut self.screen.alt_screen, snapshot.alt_screen, &images,
            placements.as_ref().map(|p| p.cell_tags[1].as_slice()))?;
        self.screen.alt_screen_is_active = snapshot.alt_screen_is_active;

        self.pixel_width = snapshot.size.pixel_width;
        self.pixel_height = snapshot.size.pixel_height;
        self.dpi = snapshot.size.dpi;

        let cursor = snapshot.cursor;
        self.cursor = cursor.position;
        self.pen = cursor.pen;
        self.wrap_next = cursor.wrap_next;
        self.insert = cursor.insert;
        self.cursor_visible = cursor.cursor_visible;

        let modes = snapshot.modes;
        self.dec_auto_wrap = modes.dec_auto_wrap;
        self.reverse_wraparound_mode = modes.reverse_wraparound_mode;
        self.reverse_video_mode = modes.reverse_video_mode;
        self.dec_origin_mode = modes.dec_origin_mode;
        self.top_and_bottom_margins = modes.top_and_bottom_margins;
        self.left_and_right_margins = modes.left_and_right_margins;
        self.left_and_right_margin_mode = modes.left_and_right_margin_mode;
        self.application_cursor_keys = modes.application_cursor_keys;
        self.application_keypad = modes.application_keypad;
        self.modify_other_keys = modes.modify_other_keys;
        self.dec_ansi_mode = modes.dec_ansi_mode;
        self.sixel_display_mode = modes.sixel_display_mode;
        self.sixel_scrolls_right = modes.sixel_scrolls_right;
        self.use_private_color_registers_for_each_graphic =
            modes.use_private_color_registers_for_each_graphic;
        self.bracketed_paste = modes.bracketed_paste;
        self.any_event_mouse = modes.any_event_mouse;
        self.button_event_mouse = modes.button_event_mouse;
        self.mouse_tracking = modes.mouse_tracking;
        self.focus_tracking = modes.focus_tracking;
        self.mouse_encoding = modes.mouse_encoding;
        self.keyboard_encoding = modes.keyboard_encoding.into();
        self.g0_charset = modes.g0_charset;
        self.g1_charset = modes.g1_charset;
        self.shift_out = modes.shift_out;
        self.newline_mode = modes.newline_mode;
        self.clear_semantic_attribute_on_newline = modes.clear_semantic_attribute_on_newline;
        self.tabs = modes.tabs;
        self.unicode_version = modes.unicode_version.into();
        self.unicode_version_stack = modes
            .unicode_version_stack
            .into_iter()
            .map(|entry| UnicodeVersionStackEntry {
                vers: entry.vers.into(),
                label: entry.label,
            })
            .collect();
        self.bidi_enabled = modes.bidi_enabled;
        self.bidi_hint = modes.bidi_hint.map(Into::into);
        self.focused = modes.focused;

        let identity = snapshot.identity;
        self.title = identity.title;
        self.icon_title = identity.icon_title;
        self.current_dir = match identity.current_dir {
            Some(url) => Some(Url::parse(&url)?),
            None => None,
        };
        self.user_vars = identity.user_vars.into_iter()
            .map(|(name, mut value)| {
                crate::agent_contract::sanitize_agent_user_var(&name, &mut value);
                (name, value)
            })
            .collect();
        self.progress = identity.progress;
        self.agent_osc_title = identity.agent_osc_title;
        self.agent_osc_progress = identity.agent_osc_progress;
        self.palette = identity.palette;
        self.color_map = identity
            .color_map
            .into_iter()
            .map(|(idx, (r, g, b))| (idx, crate::color::RgbColor::new_8bpc(r, g, b)))
            .collect();

        self.kitty_img.restore(snapshot.kitty, &images)?;
        if let Some(placements) = placements {
            self.kitty_img.next_placement_id = placements.next_id;
            self.kitty_img.placements = placements.placements.into_iter().map(|(key, info)|
                (key, super::image::ImagePlacement { info, origin: None })).collect();
            self.kitty_prune_placements();
        }

        self.seqno = snapshot.seqno;
        self.lost_focus_seqno = snapshot.seqno;
        self.lost_focus_alerted_seqno = snapshot.seqno;
        self.current_mouse_buttons.clear();
        self.last_mouse_move = None;
        self.accumulating_title = None;
        Ok(())
    }
}

fn validate_kitty_placements(snapshot: &TerminalSnapshot, state: &KittyPlacementSnapshot) -> anyhow::Result<()> {
    use crate::kitty_relative::PlacementKey;
    anyhow::ensure!(state.placements.len() <= super::kitty::MAX_PLACEMENTS, "too many Kitty placements");
    anyhow::ensure!(state.next_id >= u64::from(u32::MAX), "invalid Kitty placement counter");
    let mut placements = BTreeMap::new();
    for &(key, info) in &state.placements {
        anyhow::ensure!(key.placement_id <= state.next_id && snapshot.kitty.id_to_hash.contains_key(&key.image_id),
            "invalid Kitty placement identity");
        anyhow::ensure!(placements.insert(key, info).is_none(), "duplicate Kitty placement identity");
    }
    for (index, screen) in [&snapshot.screen, &snapshot.alt_screen].iter().enumerate() {
        let tags = &state.cell_tags[index];
        anyhow::ensure!(tags.len() == screen.image_cells.len(), "Kitty placement cell count mismatch");
        for (&tag, cell) in tags.iter().zip(&screen.image_cells) {
            let Some(image_id) = cell.image_id else {
                anyhow::ensure!(tag == 0, "non-Kitty image has a placement identity");
                continue;
            };
            let key = PlacementKey { image_id, placement_id: tag };
            anyhow::ensure!(key.protocol_id() == cell.placement_id.filter(|id| *id != 0),
                "Kitty placement identity does not match its protocol id");
            let info = placements.get(&key).ok_or_else(|| anyhow::anyhow!("unknown Kitty placement identity"))?;
            anyhow::ensure!(info.alt_screen == (index == 1), "Kitty placement is on the wrong screen");
        }
    }
    Ok(())
}

/// Clones the screen's lines with their pictures detached into `images`.
fn snapshot_screen(screen: &Screen, images: &mut ImageTable) -> ScreenSnapshot {
    let mut lines = Vec::with_capacity(screen.lines().len());
    let mut image_cells = vec![];
    for (line_idx, line) in screen.lines().iter().enumerate() {
        let mut line = line.clone();
        // Only a line that carries a picture is walked mutably: that walk
        // coerces the line to per-cell storage, which for a scrollback line
        // undoes the compression it just got, and the snapshot should leave
        // the rest as it found them.
        let has_images = line.has_hyperlinks_or_images()
            && (0..line.len())
                .filter_map(|idx| line.get_cell(idx))
                .any(|cell| cell.attrs().images().is_some());
        if has_images {
            for (cell_idx, cell) in line.cells_mut_for_attr_changes_only().iter_mut().enumerate()
            {
                let Some(cell_images) = cell.attrs().images() else {
                    continue;
                };
                for image in cell_images {
                    let (padding_left, padding_top, padding_right, padding_bottom) =
                        image.padding();
                    image_cells.push(SnapshotImageCell {
                        line_idx,
                        cell_idx,
                        hash: images.remember(image.image_data()),
                        top_left: image.top_left(),
                        bottom_right: image.bottom_right(),
                        z_index: image.z_index(),
                        padding_left,
                        padding_top,
                        padding_right,
                        padding_bottom,
                        image_id: image.image_id(),
                        placement_id: image.placement_id(),
                    });
                }
                cell.attrs_mut().clear_images();
            }
        }
        lines.push(line);
    }
    ScreenSnapshot {
        lines,
        image_cells,
        stable_row_index_offset: screen.stable_row_index_offset(),
        physical_rows: screen.physical_rows,
        physical_cols: screen.physical_cols,
        dpi: screen.dpi,
        keyboard_stack: screen
            .keyboard_stack
            .iter()
            .map(|encoding| (*encoding).into())
            .collect(),
        saved_cursor: screen.saved_cursor.clone(),
    }
}

fn restore_screen(
    screen: &mut Screen,
    snapshot: ScreenSnapshot,
    images: &HashMap<[u8; 32], Arc<ImageData>>,
    tags: Option<&[u64]>,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        snapshot.physical_rows == screen.physical_rows
            && snapshot.physical_cols == screen.physical_cols,
        "the snapshot's screen is {}x{} but this one is {}x{}",
        snapshot.physical_cols,
        snapshot.physical_rows,
        screen.physical_cols,
        screen.physical_rows
    );
    anyhow::ensure!(
        snapshot.lines.len() >= snapshot.physical_rows,
        "the snapshot holds {} lines for a {}-row screen",
        snapshot.lines.len(),
        snapshot.physical_rows
    );

    let mut lines = snapshot.lines;
    for (index, image_cell) in snapshot.image_cells.into_iter().enumerate() {
        let data = images.get(&image_cell.hash).ok_or_else(|| {
            anyhow::anyhow!(
                "line {} cell {} refers to a picture the snapshot does not carry",
                image_cell.line_idx,
                image_cell.cell_idx
            )
        })?;
        let line = lines.get_mut(image_cell.line_idx).ok_or_else(|| {
            anyhow::anyhow!("a picture sits on line {}, past the end", image_cell.line_idx)
        })?;
        let cell = line
            .cells_mut_for_attr_changes_only()
            .get_mut(image_cell.cell_idx)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "a picture sits on line {} cell {}, past the end",
                    image_cell.line_idx,
                    image_cell.cell_idx
                )
            })?;
        cell.attrs_mut()
            .attach_image(Box::new(ImageCell::with_z_index(
                image_cell.top_left,
                image_cell.bottom_right,
                Arc::clone(data),
                image_cell.z_index,
                image_cell.padding_left,
                image_cell.padding_top,
                image_cell.padding_right,
                image_cell.padding_bottom,
                image_cell.image_id,
                image_cell.placement_id,
            ).with_placement_tag(tags.map_or(0, |tags| {
                // Named placements derive their identity from their external id.
                if tags[index] > u64::from(u32::MAX) { tags[index] } else { 0 }
            }))));
    }

    // The oldest scrollback goes first when this process allows less of
    // it; the stable offset advances by as much, so a stable row index
    // still names the same line.
    let mut offset = snapshot.stable_row_index_offset;
    let capacity = screen.line_capacity();
    let excess = lines.len().saturating_sub(capacity);
    if excess > 0 {
        lines.drain(..excess);
        offset += excess;
    }

    screen.replace_lines(lines.into(), offset);
    screen.dpi = snapshot.dpi;
    screen.keyboard_stack = snapshot
        .keyboard_stack
        .into_iter()
        .map(Into::into)
        .collect();
    screen.saved_cursor = snapshot.saved_cursor;
    Ok(())
}
