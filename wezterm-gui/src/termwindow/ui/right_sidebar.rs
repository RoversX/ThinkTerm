use crate::markdown_editor::{
    build_spell_check_chunks_in_range, build_visual_document, fit_table_columns, load_remote_image,
    open_vault_document, resolve_local_image, save_document_revision, vault_file_paths,
    vault_markdown_paths, wrap_visual_document_by_width_cached, AutosaveWakeAction, BlockKind,
    EditorMode, NoteCodeBlockLayout, NoteLineGeometry, NoteLineLayout, NoteRunLayout,
    NoteSpellingIssue, ProjectedCodeBlock, ProjectedObject, SaveState, SourceSelection,
    TableAlignment, VisualLineKind,
};
use crate::quad::{QuadTrait, TripleLayerQuadAllocator, TripleLayerQuadAllocatorTrait};
use crate::termwindow::remote_files::{
    invalidate_remote_connection, invalidate_remote_connection_if_dead, remote_connection_key,
    remote_connection_manager, RemoteAcquireError, RemoteFileBytes, RemoteFileKind, RemoteFileRow,
    RemoteFilesEffect, RemoteFilesEvent, RemoteFilesPhase, RemotePath,
};
use crate::termwindow::ui::icons::{
    material_file_icon_for_name, material_folder_icon_for_name, MaterialIcon, SvgIcon,
};
use crate::termwindow::ui::platform_chrome::uses_integrated_window_buttons;
use crate::termwindow::ui::tokens::{
    CAPSULE_BORDER_WIDTH, ICON_BUTTON_BORDER_WIDTH, SIDEBAR_ICON_GAP, SIDEBAR_INSET,
    SIDEBAR_RESIZE_HANDLE_WIDTH, SIDEBAR_ROW_RADIUS, WINDOW_TAB_ADD_BUTTON_RADIUS,
    WINDOW_TAB_LEADING_ACTION_BUTTON_SIZE, WINDOW_TAB_LEADING_ACTION_GAP,
};
use crate::termwindow::{
    NoteEditorCommand, RightSidebarFileCharBag, RightSidebarFileField, RightSidebarFileIndex,
    RightSidebarFileIndexEntry, RightSidebarFileIndexStatus, RightSidebarFilePreviewImage,
    RightSidebarFilePreviewLine, RightSidebarFilePreviewSelection,
    RightSidebarFilePreviewSelectionPoint, RightSidebarFilePreviewSliceCacheKey,
    RightSidebarFilePreviewSliceCacheValue, RightSidebarFilePreviewSpan, RightSidebarFileTreeRow,
    RightSidebarFileView, RightSidebarFileViewState, RightSidebarInputLayout, RightSidebarMode,
    RightSidebarNoteImageSource, RightSidebarNoteTableLayout, RightSidebarNoteView,
    RightSidebarOpenWithCacheEntry, RightSidebarSnippetField, RightSidebarSnippetView,
    TermWindowNotif, UIItem, UIItemType, UiShapeCacheLookup,
};
use crate::ui::{scale_ui_usize, unscale_ui_usize, EditModifiers, TextInputState, UiPalette};
use crate::utilsprites::RenderMetrics;
use crate::workspace_threads;
use anyhow::Context;
use config::keyassignment::{ClipboardCopyDestination, ClipboardPasteSource, KeyAssignment};
use mux::Mux;
use std::borrow::Cow;
use std::cmp::Ordering;
use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};
use termwiz::image::{ImageData, ImageDataType};
use termwiz::input::{KeyCode as TermKeyCode, Modifiers as TermModifiers};
use thinkterm_syntax::{HighlightKind, HighlightResult, HighlightSpan, LanguageId};
use unicode_segmentation::UnicodeSegmentation;
use walkdir::{DirEntry as WalkDirEntry, WalkDir};
use wezterm_font::LoadedFont;
use window::color::LinearRgba;
use window::{
    Clipboard, ContextMenuIcon, ContextMenuItem, DeadKeyStatus, FolderPickerOptions,
    IntegratedTitleButtonStyle, Point, Rect, WindowOps,
};

const RIGHT_SIDEBAR_SECTION_GAP: usize = 12;
const RIGHT_SIDEBAR_WIDTH_CELLS: usize = 40;
const RIGHT_SIDEBAR_MIN_WIDTH: usize = 340;
const RIGHT_SIDEBAR_MAX_WIDTH: usize = 900;
const RIGHT_SIDEBAR_TOP_BAR_HEIGHT: usize = 82;
const RIGHT_SIDEBAR_CLOSE_BUTTON_SIZE: usize = 58;
const RIGHT_SIDEBAR_CLOSE_ICON_SIZE: usize = 27;
const RIGHT_SIDEBAR_CLOSE_BUTTON_X_ADJUST: usize = 8;
const RIGHT_SIDEBAR_CLOSE_BUTTON_Y_ADJUST: usize = 16;
const RIGHT_SIDEBAR_MODE_HEIGHT: usize = 72;
const RIGHT_SIDEBAR_EMPTY_HEIGHT: usize = 88;
/// Empty-state geometry for the remote Files panel, in design pixels.
const REMOTE_EMPTY_ICON_SIZE: usize = 40;
const REMOTE_EMPTY_ICON_GAP: usize = 18;
const REMOTE_EMPTY_DETAIL_GAP: usize = 6;
const REMOTE_EMPTY_BUTTON_GAP: usize = 22;
const REMOTE_EMPTY_BUTTON_HEIGHT: usize = 44;
const SNIPPET_TOOLBAR_HEIGHT: usize = 58;
const SNIPPET_SEARCH_HEIGHT: usize = 58;
const SNIPPET_CARD_HEIGHT: usize = 116;
const SNIPPET_EDITOR_HEADER_HEIGHT: usize = 108;
const SNIPPET_FIELD_HEIGHT: usize = 56;
const SNIPPET_BODY_FIELD_HEIGHT: usize = 190;
const SNIPPET_SAVE_BUTTON_HEIGHT: usize = 54;
const SNIPPET_ACTION_BUTTON_MIN_WIDTH: usize = 92;
const SNIPPET_ACTION_BUTTON_HEIGHT: usize = 46;
const SNIPPET_ROW_GAP: usize = 16;
const SNIPPET_LIST_TOP_GAP: usize = 18;
const SNIPPET_LIST_BOTTOM_PADDING: usize = 40;
const RIGHT_SIDEBAR_SCROLLBAR_VISIBLE_MS: u64 = 900;
const SNIPPET_CARET_WIDTH: f32 = 3.0;
const NOTE_TOOLBAR_HEIGHT: usize = 54;
const NOTE_BODY_TOP_GAP: usize = 12;
const NOTE_BODY_PADDING: usize = 24;
const NOTE_LINE_GAP: usize = 5;
const NOTE_CARET_WIDTH: f32 = 2.0;
const NOTE_TABLE_CELL_HORIZONTAL_PADDING: usize = 10;
const NOTE_TABLE_CELL_VERTICAL_PADDING: usize = 7;
const NOTE_TABLE_MIN_COLUMN_WIDTH: usize = 120;
const NOTE_TABLE_MAX_COLUMN_WIDTH: usize = 480;
const NOTE_TABLE_SCROLLBAR_HEIGHT: usize = 3;
const NOTE_CODE_HEADER_HEIGHT: usize = 38;
const NOTE_CODE_HORIZONTAL_PADDING: usize = 12;
const NOTE_CODE_VERTICAL_PADDING: usize = 9;
const NOTE_CODE_CONTROL_SIZE: usize = 28;
const NOTE_CODE_BLOCK_RADIUS: f32 = SIDEBAR_ROW_RADIUS;
const NOTE_CODE_HIGHLIGHT_CACHE_CAPACITY: usize = 64;
const NOTE_CODE_HIGHLIGHT_LAST_BLOCK_CAPACITY: usize = 64;
const NOTE_CODE_HIGHLIGHT_DEBOUNCE_MS: u64 = 120;
const NOTE_CODE_HIGHLIGHT_REPAINT_MS: u64 = 8;
const NOTE_AUTOSAVE_MS: u64 = 500;
const NOTE_SPELLCHECK_DEBOUNCE_MS: u64 = 300;
const NOTE_IMAGE_CACHE_CAPACITY: usize = 32;
const NOTE_IMAGE_CACHE_MAX_ENCODED_BYTES: usize = 64 * 1024 * 1024;
const NOTE_VAULT_SPLIT_MIN_WIDTH: usize = 620;
const NOTE_VAULT_TREE_WIDTH: usize = 230;
const NOTE_VAULT_RESCAN_SECS: u64 = 10;
// Points render 4/3 larger at 96dpi than on macOS, so the floor follows
// the same 0.75x rule as the default font sizes; otherwise it pins the
// Files panel above any reasonable Home Font Size setting on Linux.
const FILE_FONT_MIN_SIZE: f64 = if cfg!(target_os = "macos") {
    14.0
} else {
    10.5
};
const FILE_FILTER_HEIGHT: usize = 66;
const FILE_TREE_TOP_GAP: usize = 14;
const FILE_SCROLL_FADE_HEIGHT: usize = 32;
const FILE_PREVIEW_HEADER_HEIGHT: usize = 64;
const FILE_PREVIEW_MAX_BYTES: usize = 256 * 1024;
const FILE_PREVIEW_TRUNCATED_LABEL: &str = "Preview truncated to 256 KiB";
const FILE_PREVIEW_IMAGE_MAX_BYTES: usize = 16 * 1024 * 1024;
// The file-size cap above bounds the *encoded* bytes, but a small encoded image
// can decode to an enormous RGBA bitmap (`width × height × 4`, ×frames for
// animations) — a decompression bomb that has spiked RAM to >1 GiB. The preview
// pane is only a few hundred px wide, so cap the decode at ~16 MP (≈64 MiB
// RGBA), which still covers 4K/5K screenshots and typical photos.
const FILE_PREVIEW_IMAGE_MAX_PIXELS: u64 = 16_000_000;
const FILE_PREVIEW_PANE_MIN_WIDTH: usize = 360;
const FILE_PREVIEW_PANE_DEFAULT_WIDTH: usize = 560;
const NOTE_PANE_MIN_WIDTH: usize = FILE_PREVIEW_PANE_MIN_WIDTH;
const NOTE_PANE_DEFAULT_WIDTH: usize = FILE_PREVIEW_PANE_DEFAULT_WIDTH;
const FILE_PREVIEW_SLICE_CACHE_CAPACITY: usize = 256;
// Lines up to this many columns are shaped whole (once, cached) so horizontal
// scrolling is pure translation of the cached glyph run instead of re-shaping a
// new substring per step. Longer lines fall back to the per-window slice path to
// keep the one-time shaping cost bounded.
const FILE_PREVIEW_FULL_LINE_SHAPE_MAX_COLS: usize = 4096;
const FILE_PREVIEW_SCROLLBAR_THICKNESS: usize = 4;
const FILE_PREVIEW_SCROLLBAR_HIT_SLOP: usize = 6;
const FILE_TREE_ROW_LIMIT: usize = 2000;
const FILE_INDEX_ENTRY_LIMIT: usize = 100_000;
// How long the file panel must stay closed/idle before its in-memory index and
// buffers are released. Reopening within this window keeps everything resident.
const FILE_INDEX_IDLE_RELEASE_SECS: u64 = 30;
const NOTE_IDLE_RELEASE_SECS: u64 = 30;
// How often the Files panel re-scans the tree while it's visible + focused.
const FILE_INDEX_RESCAN_SECS: u64 = 90;
// Max number of (root, project) view-state snapshots kept in memory.
const FILE_VIEW_STATE_CACHE_CAP: usize = 32;
const FILE_FILTER_DEBOUNCE_MS: u64 = 350;

fn note_boundary_x(boundaries: &[(f32, usize)], byte: usize) -> f32 {
    boundaries
        .iter()
        .filter(|(_, source)| *source <= byte)
        .next_back()
        .or_else(|| boundaries.first())
        .map(|(x, _)| *x)
        .unwrap_or(0.0)
}

fn note_code_row_height(
    row_index: usize,
    row_count: usize,
    collapsed: bool,
    line_height: f32,
    header_height: f32,
    vertical_padding: f32,
) -> f32 {
    if collapsed {
        return if row_index == 0 { header_height } else { 0.0 };
    }
    line_height
        + if row_index == 0 {
            header_height + vertical_padding
        } else {
            0.0
        }
        + if row_index + 1 == row_count {
            vertical_padding
        } else {
            0.0
        }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct NoteVaultTreeRow {
    relative_path: String,
    name: String,
    depth: usize,
    is_dir: bool,
}

fn note_vault_tree_rows(
    markdown_paths: &[String],
    expanded: &HashSet<String>,
) -> Vec<NoteVaultTreeRow> {
    let mut directories = BTreeSet::new();
    for path in markdown_paths {
        let mut current = PathBuf::new();
        let mut components = Path::new(path).components().peekable();
        while let Some(component) = components.next() {
            if components.peek().is_none() {
                break;
            }
            current.push(component.as_os_str());
            directories.insert(current.to_string_lossy().replace('\\', "/"));
        }
    }

    let mut candidates = directories
        .iter()
        .map(|path| (path.clone(), true))
        .chain(markdown_paths.iter().cloned().map(|path| (path, false)))
        .collect::<Vec<_>>();
    candidates.sort_by(|(left, left_dir), (right, right_dir)| {
        let left_key = left.to_ascii_lowercase();
        let right_key = right.to_ascii_lowercase();
        left_key
            .cmp(&right_key)
            .then_with(|| right_dir.cmp(left_dir))
    });

    candidates
        .into_iter()
        .filter_map(|(relative_path, is_dir)| {
            let path = Path::new(&relative_path);
            let parent_visible = path
                .ancestors()
                .skip(1)
                .filter(|parent| !parent.as_os_str().is_empty())
                .all(|parent| expanded.contains(&parent.to_string_lossy().replace('\\', "/")));
            if !parent_visible {
                return None;
            }
            Some(NoteVaultTreeRow {
                name: path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or(&relative_path)
                    .to_string(),
                depth: path.components().count().saturating_sub(1),
                relative_path,
                is_dir,
            })
        })
        .collect()
}

fn visible_code_block_rounded_edges(
    block_top: f32,
    block_bottom: f32,
    viewport_top: f32,
    viewport_bottom: f32,
) -> (bool, bool) {
    (block_top >= viewport_top, block_bottom <= viewport_bottom)
}

/// Idle-release decisions extracted as pure functions so the token/visibility
/// logic is unit-testable without a TermWindow or real timers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NoteReleaseAction {
    Skip,
    SaveAndReschedule,
    Release,
}

fn note_release_action(token_matches: bool, visible: bool, dirty: bool) -> NoteReleaseAction {
    if !token_matches || visible {
        NoteReleaseAction::Skip
    } else if dirty {
        NoteReleaseAction::SaveAndReschedule
    } else {
        NoteReleaseAction::Release
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileReleaseAction {
    Skip,
    RescheduleWhileIndexing,
    Release,
}

fn file_release_action(token_matches: bool, visible: bool, indexing: bool) -> FileReleaseAction {
    if !token_matches || visible {
        FileReleaseAction::Skip
    } else if indexing {
        FileReleaseAction::RescheduleWhileIndexing
    } else {
        FileReleaseAction::Release
    }
}

/// Hash an already-ordered sequence with the same shape as `Vec::hash`
/// (length prefix + elements) so adjacent collections cannot alias.
fn hash_ordered_iter<H: std::hash::Hasher, T: Hash>(
    hasher: &mut H,
    iter: impl ExactSizeIterator<Item = T>,
) {
    iter.len().hash(hasher);
    for item in iter {
        item.hash(hasher);
    }
}

fn virtual_note_line_range(
    geometry: &[NoteLineGeometry],
    scroll: f32,
    viewport_height: f32,
) -> std::ops::Range<usize> {
    if geometry.is_empty() || viewport_height <= 0.0 {
        return 0..0;
    }
    // Enough off-screen rows for drag-selection and caret hit-testing near the
    // edges without paying for multiple invisible viewports per frame.
    let overscan = viewport_height * 0.75;
    let paint_top = (scroll - overscan).max(0.0);
    let paint_bottom = scroll + viewport_height + overscan;
    let start = geometry.partition_point(|line| line.top + line.height + line.gap < paint_top);
    let end = geometry
        .partition_point(|line| line.top <= paint_bottom)
        .min(geometry.len());
    start..end
}

#[derive(Debug, Clone, Copy)]
struct NoteApproximateTextMetrics {
    latin: f32,
    space: f32,
    wide: f32,
}

fn approximate_note_text_width(text: &str, metrics: NoteApproximateTextMetrics) -> f32 {
    text.graphemes(true)
        .map(|grapheme| {
            if grapheme.chars().all(char::is_whitespace) {
                metrics.space
            } else if grapheme.is_ascii() {
                metrics.latin
            } else if unicode_width::UnicodeWidthStr::width(grapheme) >= 2 {
                metrics.wide
            } else {
                metrics.latin
            }
        })
        .sum()
}

#[derive(Debug, Clone)]
struct NoteTableRowPaintLayout {
    table_start: usize,
    column_widths: Rc<Vec<f32>>,
    alignments: Rc<Vec<TableAlignment>>,
    row_index: usize,
    row_count: usize,
    total_width: f32,
    horizontal_offset: f32,
    max_horizontal_scroll: f32,
}

fn scrollable_note_table_columns(
    desired: &[f32],
    available_width: f32,
    minimum_width: f32,
    maximum_width: f32,
) -> Vec<f32> {
    if desired.is_empty() {
        return Vec::new();
    }
    let available_width = available_width.max(1.0);
    let minimum_width = minimum_width.max(1.0);
    let maximum_width = maximum_width.max(minimum_width);
    let widths = desired
        .iter()
        .map(|width| width.clamp(minimum_width, maximum_width))
        .collect::<Vec<_>>();
    let total = widths.iter().sum::<f32>();
    if total < available_width {
        fit_table_columns(&widths, available_width, minimum_width)
    } else {
        widths
    }
}

fn note_image_display_size(
    image_width: u32,
    image_height: u32,
    available_width: f32,
    available_height: f32,
) -> (f32, f32) {
    if image_width == 0 || image_height == 0 || available_width <= 0.0 || available_height <= 0.0 {
        return (0.0, 0.0);
    }

    // Markdown images should be useful at reading distance, including small
    // logos and diagrams.  Allow moderate upscaling while bounding both axes;
    // giant/tall images remain contained inside the current viewport.
    const MAX_UPSCALE: f32 = 3.0;
    let scale = (available_width / image_width as f32)
        .min(available_height / image_height as f32)
        .min(MAX_UPSCALE);
    (
        (image_width as f32 * scale).max(1.0),
        (image_height as f32 * scale).max(1.0),
    )
}

fn note_image_row_height(
    image: Option<&RightSidebarFilePreviewImage>,
    text_height: f32,
    available_width: f32,
    available_height: f32,
) -> f32 {
    image
        .map(|image| {
            note_image_display_size(image.width, image.height, available_width, available_height).1
        })
        .unwrap_or(text_height)
        .max(text_height)
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct NoteTextureClip {
    left: f32,
    top: f32,
    right: f32,
    bottom: f32,
    texture_left: f32,
    texture_top: f32,
    texture_right: f32,
    texture_bottom: f32,
}

#[allow(clippy::too_many_arguments)]
fn clip_note_texture(
    left: f32,
    top: f32,
    right: f32,
    bottom: f32,
    texture_left: f32,
    texture_top: f32,
    texture_right: f32,
    texture_bottom: f32,
    clip_left: f32,
    clip_top: f32,
    clip_right: f32,
    clip_bottom: f32,
) -> Option<NoteTextureClip> {
    let width = right - left;
    let height = bottom - top;
    if width <= 0.0
        || height <= 0.0
        || texture_right <= texture_left
        || texture_bottom <= texture_top
        || clip_right <= clip_left
        || clip_bottom <= clip_top
    {
        return None;
    }
    let visible_left = left.max(clip_left);
    let visible_top = top.max(clip_top);
    let visible_right = right.min(clip_right);
    let visible_bottom = bottom.min(clip_bottom);
    if visible_right <= visible_left || visible_bottom <= visible_top {
        return None;
    }
    let texture_width = texture_right - texture_left;
    let texture_height = texture_bottom - texture_top;
    Some(NoteTextureClip {
        left: visible_left,
        top: visible_top,
        right: visible_right,
        bottom: visible_bottom,
        texture_left: texture_left + texture_width * ((visible_left - left) / width),
        texture_top: texture_top + texture_height * ((visible_top - top) / height),
        texture_right: texture_left + texture_width * ((visible_right - left) / width),
        texture_bottom: texture_top + texture_height * ((visible_bottom - top) / height),
    })
}

fn note_open_pending_for_vault(opening: Option<&(PathBuf, String)>, vault_root: &Path) -> bool {
    opening.is_some_and(|(root, _)| root == vault_root)
}

#[derive(Debug, Clone)]
struct NoteCodeRowPaintLayout {
    block_start: usize,
    language: String,
    code: Arc<ProjectedCodeBlock>,
    row_index: usize,
    row_count: usize,
    collapsed: bool,
    horizontal_offset: f32,
    block_height: f32,
    max_horizontal_scroll: f32,
}

/// Immutable component geometry for one visual/layout revision. Vertical
/// scrolling only clones these Arcs; it must not clone the full Markdown
/// projection or remeasure every table/code cell on every frame.
#[derive(Default)]
pub(crate) struct NotePaintCache {
    key: Option<u64>,
    table_rows: Arc<HashMap<usize, NoteTableRowPaintLayout>>,
    code_rows: Arc<HashMap<usize, NoteCodeRowPaintLayout>>,
    image_sources: Arc<HashMap<usize, RightSidebarNoteImageSource>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct NoteCodeHighlightKey {
    language: LanguageId,
    content_hash: u64,
    content_len: usize,
}

#[derive(Debug)]
struct NoteCodeHighlightEntry {
    key: NoteCodeHighlightKey,
    source: Arc<str>,
    light: Arc<Vec<Vec<LinearRgba>>>,
    dark: Arc<Vec<Vec<LinearRgba>>>,
}

impl NoteCodeHighlightEntry {
    fn colors(&self, use_dark_theme: bool) -> Arc<Vec<Vec<LinearRgba>>> {
        if use_dark_theme {
            Arc::clone(&self.dark)
        } else {
            Arc::clone(&self.light)
        }
    }
}

#[derive(Default)]
pub(crate) struct NoteCodeHighlightState {
    cache: HashMap<NoteCodeHighlightKey, Arc<NoteCodeHighlightEntry>>,
    cache_order: VecDeque<NoteCodeHighlightKey>,
    last_by_block: HashMap<usize, Arc<NoteCodeHighlightEntry>>,
    last_block_order: VecDeque<usize>,
    scheduled: HashMap<usize, (NoteCodeHighlightKey, u64)>,
    in_flight: HashMap<usize, (NoteCodeHighlightKey, Arc<AtomicUsize>)>,
    next_generation: u64,
    repaint_scheduled: bool,
}

impl NoteCodeHighlightState {
    fn cached(
        &self,
        key: NoteCodeHighlightKey,
        source: &str,
    ) -> Option<Arc<NoteCodeHighlightEntry>> {
        self.cache
            .get(&key)
            .filter(|entry| entry.source.as_ref() == source)
            .cloned()
    }

    fn insert_cache(&mut self, entry: Arc<NoteCodeHighlightEntry>) {
        let key = entry.key;
        if !self.cache.contains_key(&key) {
            self.cache_order.push_back(key);
        }
        self.cache.insert(key, entry);
        while self.cache_order.len() > NOTE_CODE_HIGHLIGHT_CACHE_CAPACITY {
            if let Some(expired) = self.cache_order.pop_front() {
                self.cache.remove(&expired);
            }
        }
    }

    fn remember_block(&mut self, block_start: usize, entry: Arc<NoteCodeHighlightEntry>) {
        self.last_block_order.retain(|start| *start != block_start);
        self.last_block_order.push_back(block_start);
        self.last_by_block.insert(block_start, entry);
        while self.last_block_order.len() > NOTE_CODE_HIGHLIGHT_LAST_BLOCK_CAPACITY {
            if let Some(expired) = self.last_block_order.pop_front() {
                self.last_by_block.remove(&expired);
            }
        }
    }

    fn previous_for_block(
        &self,
        block_start: usize,
        language: LanguageId,
    ) -> Option<Arc<NoteCodeHighlightEntry>> {
        self.last_by_block
            .get(&block_start)
            .filter(|entry| entry.key.language == language)
            .cloned()
    }

    fn schedule_block(&mut self, block_start: usize, key: NoteCodeHighlightKey) -> Option<u64> {
        if self
            .scheduled
            .get(&block_start)
            .is_some_and(|(scheduled, _)| *scheduled == key)
            || self
                .in_flight
                .get(&block_start)
                .is_some_and(|(pending, _)| *pending == key)
        {
            return None;
        }
        self.scheduled.remove(&block_start);
        if let Some((_, cancellation)) = self.in_flight.remove(&block_start) {
            cancellation.store(1, AtomicOrdering::Relaxed);
        }
        self.next_generation = self.next_generation.wrapping_add(1).max(1);
        let generation = self.next_generation;
        self.scheduled.insert(block_start, (key, generation));
        Some(generation)
    }

    fn cancel_block_work(&mut self, block_start: usize) {
        self.scheduled.remove(&block_start);
        if let Some((_, cancellation)) = self.in_flight.remove(&block_start) {
            cancellation.store(1, AtomicOrdering::Relaxed);
        }
    }

    fn retain_blocks(&mut self, current: &HashSet<usize>) {
        let removed = self
            .scheduled
            .keys()
            .chain(self.in_flight.keys())
            .chain(self.last_by_block.keys())
            .copied()
            .filter(|start| !current.contains(start))
            .collect::<HashSet<_>>();
        for block_start in removed {
            self.cancel_block_work(block_start);
            self.last_by_block.remove(&block_start);
        }
        self.last_block_order
            .retain(|start| current.contains(start));
    }
}

fn note_code_highlight_key(code: &ProjectedCodeBlock) -> Option<NoteCodeHighlightKey> {
    if code.text.len() > thinkterm_syntax::DEFAULT_HIGHLIGHT_BYTE_LIMIT {
        return None;
    }
    let language = code
        .language
        .as_deref()
        .and_then(thinkterm_syntax::detect_fence)?;
    let mut hasher = DefaultHasher::new();
    code.text.hash(&mut hasher);
    Some(NoteCodeHighlightKey {
        language,
        content_hash: hasher.finish(),
        content_len: code.text.len(),
    })
}

#[derive(Debug, Clone, Copy)]
pub struct RightSidebarRect {
    pub x: usize,
    pub y: usize,
    pub width: usize,
    pub height: usize,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct RightSidebarSnippetScrollGeometry {
    pub track_x: usize,
    pub track_y: usize,
    pub track_width: usize,
    pub track_height: usize,
    pub thumb_y: f32,
    pub thumb_height: f32,
    pub max_scroll: f32,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct RightSidebarFilePreviewScrollGeometry {
    pub track_x: usize,
    pub track_y: usize,
    pub track_width: usize,
    pub track_height: usize,
    pub thumb_y: f32,
    pub thumb_height: f32,
    pub max_scroll: f32,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct RightSidebarFilePreviewHorizontalScrollGeometry {
    pub track_x: usize,
    pub track_y: usize,
    pub track_width: usize,
    pub track_height: usize,
    pub thumb_x: f32,
    pub thumb_width: f32,
    pub max_scroll: usize,
}

struct FilePreviewPaintProfile {
    enabled: bool,
    start: Option<Instant>,
    line_count: usize,
    visible_lines: usize,
    plain_lines: usize,
    highlighted_lines: usize,
    slice_requests: usize,
    shape_requests: usize,
    shape_cache_hits: usize,
    shape_cache_misses: usize,
}

impl FilePreviewPaintProfile {
    fn new() -> Self {
        static ENABLED: OnceLock<bool> = OnceLock::new();
        let enabled =
            *ENABLED.get_or_init(|| std::env::var_os("THINKTERM_PROFILE_FILE_PREVIEW").is_some());
        Self {
            enabled,
            start: enabled.then(Instant::now),
            line_count: 0,
            visible_lines: 0,
            plain_lines: 0,
            highlighted_lines: 0,
            slice_requests: 0,
            shape_requests: 0,
            shape_cache_hits: 0,
            shape_cache_misses: 0,
        }
    }

    fn record_shape_lookup(&mut self, lookup: UiShapeCacheLookup) {
        if !self.enabled {
            return;
        }
        self.shape_requests += 1;
        match lookup {
            UiShapeCacheLookup::Hit => self.shape_cache_hits += 1,
            UiShapeCacheLookup::Miss => self.shape_cache_misses += 1,
            UiShapeCacheLookup::Skipped => {}
        }
    }

    fn finish(&self, scroll_offset: f32, horizontal_offset: usize) {
        if !self.enabled {
            return;
        }
        let Some(start) = self.start else {
            return;
        };
        let elapsed = start.elapsed();
        if elapsed < Duration::from_millis(8) {
            return;
        }
        log::info!(
            "file preview paint: {:?}, lines={}, visible={}, plain={}, highlighted={}, slices={}, shape_requests={}, shape_hits={}, shape_misses={}, scroll={:.1}, hscroll={}",
            elapsed,
            self.line_count,
            self.visible_lines,
            self.plain_lines,
            self.highlighted_lines,
            self.slice_requests,
            self.shape_requests,
            self.shape_cache_hits,
            self.shape_cache_misses,
            scroll_offset,
            horizontal_offset
        );
    }
}

/// Per-paint profiler for the Note markdown body, mirroring
/// `FilePreviewPaintProfile`. Enable with `THINKTERM_PROFILE_NOTE=1`; frames
/// slower than 8ms log a breakdown so jank can be attributed precisely.
struct NotePaintProfile {
    enabled: bool,
    start: Option<Instant>,
    wrap_source: &'static str,
    total_lines: usize,
    painted_lines: usize,
    visible_lines: usize,
    painted_runs: usize,
    shape_requests: usize,
    shape_cache_hits: usize,
    shape_cache_misses: usize,
}

impl NotePaintProfile {
    fn new() -> Self {
        static ENABLED: OnceLock<bool> = OnceLock::new();
        let enabled = *ENABLED.get_or_init(|| std::env::var_os("THINKTERM_PROFILE_NOTE").is_some());
        Self {
            enabled,
            start: enabled.then(Instant::now),
            wrap_source: "",
            total_lines: 0,
            painted_lines: 0,
            visible_lines: 0,
            painted_runs: 0,
            shape_requests: 0,
            shape_cache_hits: 0,
            shape_cache_misses: 0,
        }
    }

    /// Whole-frame shape accounting from the Note domain cache's cumulative
    /// counters: covers wrap measurement, approximate sampling, tables, code
    /// layout, preedit and toolbar — not just the visible run loop.
    fn capture_note_cache_delta(
        &mut self,
        before: crate::shapecache::UiShapeCacheStats,
        after: crate::shapecache::UiShapeCacheStats,
    ) {
        if !self.enabled {
            return;
        }
        self.shape_cache_hits = after.hits.saturating_sub(before.hits) as usize;
        self.shape_cache_misses = after.misses.saturating_sub(before.misses) as usize;
        self.shape_requests = self.shape_cache_hits + self.shape_cache_misses;
    }

    fn finish(&self, scroll_offset: f32) {
        if !self.enabled {
            return;
        }
        let Some(start) = self.start else {
            return;
        };
        let elapsed = start.elapsed();
        if elapsed < Duration::from_millis(8) {
            return;
        }
        log::info!(
            "note paint: {:?}, wrap={}, lines={}, painted={}, visible={}, runs={}, shape_requests={}, shape_hits={}, shape_misses={}, scroll={:.1}",
            elapsed,
            self.wrap_source,
            self.total_lines,
            self.painted_lines,
            self.visible_lines,
            self.painted_runs,
            self.shape_requests,
            self.shape_cache_hits,
            self.shape_cache_misses,
            scroll_offset
        );
    }
}

#[derive(Debug, Clone, Copy)]
struct RightSidebarFilePreviewBodyMetrics {
    x: usize,
    y: usize,
    width: usize,
    bottom: usize,
    line_height: usize,
    visible_height: usize,
    total_height: usize,
}

struct RightSidebarLoadedFilePreview {
    lines: Vec<RightSidebarFilePreviewLine>,
    image: Option<RightSidebarFilePreviewImage>,
    message: Option<String>,
    truncated: bool,
}

#[derive(Debug, Clone)]
struct RightSidebarFileRoot {
    project_name: String,
    path: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RightSidebarFileRowMetrics {
    row_height: usize,
    icon_size: usize,
    chevron_size: usize,
    indent_step: usize,
    icon_gap: usize,
}

impl RightSidebarMode {
    fn icon(self) -> SvgIcon {
        match self {
            Self::Chat => SvgIcon::FolderTree,
            Self::Tasks => SvgIcon::NotebookTabs,
            Self::Snippets => SvgIcon::CodeXml,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Chat => "File",
            Self::Tasks => "Note",
            Self::Snippets => "Snippets",
        }
    }
}

pub fn right_sidebar_width_for_metrics(render_metrics: &RenderMetrics, dpi: usize) -> usize {
    let min_width = scale_ui_usize(RIGHT_SIDEBAR_MIN_WIDTH, dpi);
    let max_width = scale_ui_usize(RIGHT_SIDEBAR_MAX_WIDTH, dpi);
    let default_width =
        (render_metrics.cell_size.width as usize * RIGHT_SIDEBAR_WIDTH_CELLS).max(min_width);
    crate::native_settings::right_sidebar_width()
        .map(|width| scale_ui_usize(width, dpi))
        .unwrap_or(default_width)
        .clamp(min_width, max_width)
}

pub fn right_sidebar_file_preview_width(dpi: usize) -> usize {
    let min_width = scale_ui_usize(FILE_PREVIEW_PANE_MIN_WIDTH, dpi);
    crate::native_settings::right_sidebar_file_preview_width()
        .map(|width| scale_ui_usize(width, dpi))
        .unwrap_or_else(|| scale_ui_usize(FILE_PREVIEW_PANE_DEFAULT_WIDTH, dpi))
        .max(min_width)
}

pub fn right_sidebar_note_pane_width_for_dpi(dpi: usize) -> usize {
    let min_width = scale_ui_usize(NOTE_PANE_MIN_WIDTH, dpi);
    crate::native_settings::right_sidebar_note_pane_width()
        .map(|width| scale_ui_usize(width, dpi))
        .unwrap_or_else(|| scale_ui_usize(NOTE_PANE_DEFAULT_WIDTH, dpi))
        .max(min_width)
}

impl crate::TermWindow {
    fn right_sidebar_file_preview_active(&self) -> bool {
        !self.right_sidebar_collapsed
            && self.right_sidebar_mode == RightSidebarMode::Chat
            && self.right_sidebar_file_view == RightSidebarFileView::Preview
            && (self.right_sidebar_file_selected.is_some()
                || self.right_sidebar_remote_files.selected.is_some())
    }

    fn right_sidebar_tree_width(&self) -> usize {
        let width = if self.right_sidebar_file_preview_active() {
            self.right_sidebar_file_tree_width
        } else {
            self.right_sidebar_width
        };
        width.clamp(
            self.ui_px(RIGHT_SIDEBAR_MIN_WIDTH),
            self.right_sidebar_max_width(),
        )
    }

    fn right_sidebar_available_width(&self) -> usize {
        let border = self.get_os_border();
        self.dimensions
            .pixel_width
            .saturating_sub((border.left + border.right).get() as usize)
    }

    /// Maximum sidebar-column + expanded-pane total width. Shared by the file
    /// preview pane and the Note pane; both keep a terminal reserve.
    fn right_sidebar_pane_total_max_width(&self) -> usize {
        let available_width = self.right_sidebar_available_width();
        let content_width = available_width.saturating_sub(self.workspace_sidebar_width());
        let min_preview_total =
            self.ui_px(RIGHT_SIDEBAR_MIN_WIDTH) + self.ui_px(FILE_PREVIEW_PANE_MIN_WIDTH);
        let terminal_reserve = content_width / 5;
        content_width
            .saturating_sub(terminal_reserve)
            .max(min_preview_total.min(content_width))
            .min(available_width)
    }

    fn right_sidebar_file_preview_width(&self) -> Option<usize> {
        if self.right_sidebar_collapsed
            || self.right_sidebar_mode != RightSidebarMode::Chat
            || self.right_sidebar_file_view != RightSidebarFileView::Preview
            || (self.right_sidebar_file_selected.is_none()
                && self.right_sidebar_remote_files.selected.is_none())
        {
            return None;
        }

        let max_preview_width = self
            .right_sidebar_pane_total_max_width()
            .saturating_sub(self.right_sidebar_tree_width());
        if max_preview_width < self.ui_px(FILE_PREVIEW_PANE_MIN_WIDTH) {
            return None;
        }

        let configured_width = if self.right_sidebar_file_preview_width == 0 {
            self.ui_px(FILE_PREVIEW_PANE_DEFAULT_WIDTH)
        } else {
            self.right_sidebar_file_preview_width
        };
        let width =
            configured_width.clamp(self.ui_px(FILE_PREVIEW_PANE_MIN_WIDTH), max_preview_width);
        Some(width)
    }

    pub(crate) fn right_sidebar_note_pane_active(&self) -> bool {
        !self.right_sidebar_collapsed
            && self.right_sidebar_mode == RightSidebarMode::Tasks
            && self.right_sidebar_note_pane_expanded
            // Without a vault there is no editor to expand; the sidebar shows
            // the choose/create-vault UI inline instead of a blank pane. This
            // is a cached flag: the predicate sits inside right_sidebar_width
            // and must not lock the workspace store.
            && self.active_space_has_note_vault
    }

    /// Re-derive the cached vault flag from the store. Call after anything
    /// that can change the active Space or its vault binding.
    pub(crate) fn refresh_active_space_note_vault_flag(&mut self) {
        self.active_space_has_note_vault =
            workspace_threads::space_note_vault(&self.active_space_id).is_some();
    }

    pub(crate) fn right_sidebar_note_pane_width(&self) -> Option<usize> {
        if !self.right_sidebar_note_pane_active() {
            return None;
        }
        let max_pane_width = self
            .right_sidebar_pane_total_max_width()
            .saturating_sub(self.right_sidebar_tree_width());
        if max_pane_width < self.ui_px(NOTE_PANE_MIN_WIDTH) {
            return None;
        }
        let configured_width = if self.right_sidebar_note_pane_width == 0 {
            self.ui_px(NOTE_PANE_DEFAULT_WIDTH)
        } else {
            self.right_sidebar_note_pane_width
        };
        Some(configured_width.clamp(self.ui_px(NOTE_PANE_MIN_WIDTH), max_pane_width))
    }

    pub fn right_sidebar_width(&self) -> usize {
        if self.right_sidebar_collapsed {
            0
        } else {
            // The file preview pane and the Note pane are mutually exclusive
            // (different sidebar modes); at most one is non-zero.
            let pane_width = self
                .right_sidebar_file_preview_width()
                .or_else(|| self.right_sidebar_note_pane_width())
                .unwrap_or(0);
            self.right_sidebar_tree_width()
                .saturating_add(pane_width)
                .min(if pane_width > 0 {
                    self.right_sidebar_pane_total_max_width()
                } else {
                    self.right_sidebar_available_width()
                })
        }
    }

    pub fn right_sidebar_max_width(&self) -> usize {
        let available_width = self.right_sidebar_available_width();
        let proportional_max = (available_width * 2 / 3).max(self.ui_px(RIGHT_SIDEBAR_MIN_WIDTH));
        self.ui_px(RIGHT_SIDEBAR_MAX_WIDTH).min(proportional_max)
    }

    fn right_sidebar_window_button_reserved_width(&self) -> usize {
        if cfg!(target_os = "macos")
            || !uses_integrated_window_buttons(self.config.window_decorations, self.window_state)
            || self.config.integrated_title_button_style == IntegratedTitleButtonStyle::MacOsNative
        {
            return 0;
        }

        self.config.integrated_title_buttons.len()
            * (self.ui_px(WINDOW_TAB_LEADING_ACTION_BUTTON_SIZE)
                + self.ui_px(WINDOW_TAB_LEADING_ACTION_GAP) / 2)
            + self.ui_px(WINDOW_TAB_LEADING_ACTION_GAP)
    }

    pub fn set_right_sidebar_width(&mut self, width: usize) {
        self.right_sidebar_width = width.clamp(
            self.ui_px(RIGHT_SIDEBAR_MIN_WIDTH),
            self.right_sidebar_max_width(),
        );
    }

    fn set_right_sidebar_file_tree_width(&mut self, width: usize) -> bool {
        let old_width = self.right_sidebar_file_tree_width;
        self.right_sidebar_file_tree_width = width.clamp(
            self.ui_px(RIGHT_SIDEBAR_MIN_WIDTH),
            self.right_sidebar_max_width(),
        );
        old_width != self.right_sidebar_file_tree_width
    }

    pub(crate) fn set_right_sidebar_file_preview_total_width(&mut self, width: usize) -> bool {
        if !self.right_sidebar_file_preview_active() {
            return false;
        }

        let tree_width = self.right_sidebar_tree_width();
        let preview_width = width.saturating_sub(tree_width);
        self.set_right_sidebar_file_preview_width_for_drag(preview_width)
    }

    fn set_right_sidebar_file_preview_width_for_drag(&mut self, width: usize) -> bool {
        if !self.right_sidebar_file_preview_active() {
            return false;
        }

        let max_preview_width = self
            .right_sidebar_pane_total_max_width()
            .saturating_sub(self.right_sidebar_tree_width());
        if max_preview_width < self.ui_px(FILE_PREVIEW_PANE_MIN_WIDTH) {
            return false;
        }

        let old_width = self.right_sidebar_file_preview_width;
        self.right_sidebar_file_preview_width =
            width.clamp(self.ui_px(FILE_PREVIEW_PANE_MIN_WIDTH), max_preview_width);
        old_width != self.right_sidebar_file_preview_width
    }

    pub(crate) fn set_right_sidebar_note_pane_total_width(&mut self, width: usize) -> bool {
        if !self.right_sidebar_note_pane_active() {
            return false;
        }

        let tree_width = self.right_sidebar_tree_width();
        let pane_width = width.saturating_sub(tree_width);
        let max_pane_width = self
            .right_sidebar_pane_total_max_width()
            .saturating_sub(tree_width);
        if max_pane_width < self.ui_px(NOTE_PANE_MIN_WIDTH) {
            return false;
        }

        let old_width = self.right_sidebar_note_pane_width;
        self.right_sidebar_note_pane_width =
            pane_width.clamp(self.ui_px(NOTE_PANE_MIN_WIDTH), max_pane_width);
        old_width != self.right_sidebar_note_pane_width
    }

    pub fn persist_right_sidebar_width(&self) {
        let width = self.right_sidebar_width.clamp(
            self.ui_px(RIGHT_SIDEBAR_MIN_WIDTH),
            self.ui_px(RIGHT_SIDEBAR_MAX_WIDTH),
        );
        let width = unscale_ui_usize(width, self.dimensions.dpi);
        if let Err(err) = crate::native_settings::save_right_sidebar_width(width) {
            log::warn!("failed to save right sidebar width: {err:#}");
        }
    }

    pub fn persist_right_sidebar_file_preview_width(&self) {
        let width = self
            .right_sidebar_file_preview_width()
            .unwrap_or(self.right_sidebar_file_preview_width)
            .max(self.ui_px(FILE_PREVIEW_PANE_MIN_WIDTH));
        let width = unscale_ui_usize(width, self.dimensions.dpi);
        if let Err(err) = crate::native_settings::save_right_sidebar_file_preview_width(width) {
            log::warn!("failed to save right sidebar file preview width: {err:#}");
        }
    }

    pub fn toggle_right_sidebar(&mut self) {
        self.right_sidebar_collapsed = !self.right_sidebar_collapsed;
        if self.right_sidebar_collapsed {
            if self.right_sidebar_mode == RightSidebarMode::Tasks {
                self.clear_right_sidebar_text_focus();
            }
            self.schedule_right_sidebar_file_memory_release();
            self.release_right_sidebar_remote_files_if_hidden();
            self.schedule_right_sidebar_note_memory_release();
        } else {
            self.kick_right_sidebar_file_rescan_cycle();
            self.request_right_sidebar_remote_files_connect(false);
            if self.right_sidebar_mode == RightSidebarMode::Tasks {
                self.right_sidebar_note_memory_release_token =
                    self.right_sidebar_note_memory_release_token.wrapping_add(1);
            }
        }
    }

    pub fn expand_right_sidebar(&mut self) {
        self.right_sidebar_collapsed = false;
        self.kick_right_sidebar_file_rescan_cycle();
        self.request_right_sidebar_remote_files_connect(false);
        if self.right_sidebar_mode == RightSidebarMode::Tasks {
            self.right_sidebar_note_memory_release_token =
                self.right_sidebar_note_memory_release_token.wrapping_add(1);
        }
    }

    /// The file panel (and its in-memory index) is only relevant while the right
    /// sidebar is open and in `Chat`/File mode.
    pub(crate) fn right_sidebar_file_view_active(&self) -> bool {
        !self.right_sidebar_collapsed && self.right_sidebar_mode == RightSidebarMode::Chat
    }

    /// Schedule a delayed check that frees the file index + buffers if the panel
    /// stays closed/idle. Bumping the token makes any earlier pending check a
    /// no-op, so reopening or re-toggling within the window keeps memory warm.
    pub(crate) fn schedule_right_sidebar_file_memory_release(&mut self) {
        let Some(window) = self.window.as_ref().cloned() else {
            return;
        };
        self.right_sidebar_file_memory_release_token =
            self.right_sidebar_file_memory_release_token.wrapping_add(1);
        let token = self.right_sidebar_file_memory_release_token;
        let target = Instant::now() + Duration::from_secs(FILE_INDEX_IDLE_RELEASE_SECS);
        promise::spawn::spawn(async move {
            smol::Timer::at(target).await;
            window.notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                term_window.release_right_sidebar_file_memory_if_idle(token);
            })));
        })
        .detach();
    }

    /// Free the in-memory file index, browse/search rows and preview buffers once
    /// the panel has been idle long enough. No-op if it was reopened/re-toggled
    /// since scheduling, or if an index build is still in flight. The root and
    /// expanded-folder set are kept so reopening rebuilds the same view.
    fn release_right_sidebar_file_memory_if_idle(&mut self, token: u64) {
        match file_release_action(
            token == self.right_sidebar_file_memory_release_token,
            self.right_sidebar_file_view_active(),
            matches!(
                self.right_sidebar_file_index_status,
                RightSidebarFileIndexStatus::Indexing
            ),
        ) {
            FileReleaseAction::Skip => return,
            FileReleaseAction::RescheduleWhileIndexing => {
                // The panel is hidden but an index build is still running; a
                // bare return here used to leak the whole File state forever
                // (no timer remained). Re-arm the release so it lands once
                // indexing settles.
                self.schedule_right_sidebar_file_memory_release();
                return;
            }
            FileReleaseAction::Release => {}
        }

        // Remember the view (paths/scroll/filter, a few KB) BEFORE the teardown
        // wipes selected/preview, so reopening this root restores where we were.
        self.save_right_sidebar_file_view_state();

        if let Some(cancel) = self.right_sidebar_file_index_cancel.take() {
            cancel.store(true, AtomicOrdering::Relaxed);
        }
        self.clear_right_sidebar_file_search();
        self.close_right_sidebar_file_preview();
        // `close_right_sidebar_file_preview` only `.clear()`s the preview lines,
        // which keeps the (potentially large) capacity; drop it outright.
        self.right_sidebar_file_preview_lines = Vec::new();

        self.right_sidebar_file_index = None;
        self.right_sidebar_file_index_status = RightSidebarFileIndexStatus::Empty;
        self.right_sidebar_file_browse_rows = Vec::new();
        self.right_sidebar_file_browse_cache_key = None;
        self.ui_shape_caches.borrow_mut().clear_file_preview();
        self.publish_ui_shape_cache_diagnostics();
        self.invalidate_window();
    }

    /// Notes are entirely lazy at runtime: once the panel stays hidden, drop
    /// source/projection/wrap/highlight/image/index state.  The persisted active
    /// path is enough to reopen it; no polling or worker remains owned by the
    /// feature while it is unused.
    pub(crate) fn schedule_right_sidebar_note_memory_release(&mut self) {
        let Some(window) = self.window.as_ref().cloned() else {
            return;
        };
        self.right_sidebar_note_memory_release_token =
            self.right_sidebar_note_memory_release_token.wrapping_add(1);
        let token = self.right_sidebar_note_memory_release_token;
        let target = Instant::now() + Duration::from_secs(NOTE_IDLE_RELEASE_SECS);
        promise::spawn::spawn(async move {
            smol::Timer::at(target).await;
            window.notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                term_window.release_right_sidebar_note_memory_if_idle(token);
            })));
        })
        .detach();
    }

    fn release_right_sidebar_note_memory_if_idle(&mut self, token: u64) {
        match note_release_action(
            token == self.right_sidebar_note_memory_release_token,
            self.right_sidebar_note_visible(),
            self.right_sidebar_note
                .session
                .as_ref()
                .is_some_and(|session| session.lock().is_dirty()),
        ) {
            NoteReleaseAction::Skip => return,
            NoteReleaseAction::SaveAndReschedule => {
                self.save_right_sidebar_note_now();
                self.schedule_right_sidebar_note_memory_release();
                return;
            }
            NoteReleaseAction::Release => {}
        }

        self.right_sidebar_note_open_generation =
            self.right_sidebar_note_open_generation.wrapping_add(1);
        self.right_sidebar_note_vault_index_generation = self
            .right_sidebar_note_vault_index_generation
            .wrapping_add(1);
        self.right_sidebar_note = crate::markdown_editor::NoteHostState::default();
        self.right_sidebar_note_opening = None;
        self.right_sidebar_note_open_failure = None;
        self.right_sidebar_note_vault_index_root = None;
        self.right_sidebar_note_vault_paths = Arc::new(Vec::new());
        self.right_sidebar_note_vault_indexing = false;
        self.right_sidebar_note_vault_last_scan = None;
        self.right_sidebar_note_tree_expanded.clear();
        self.right_sidebar_note_table_horizontal_offsets.clear();
        self.right_sidebar_note_table_layouts.clear();
        self.right_sidebar_note_images.clear();
        self.right_sidebar_note_images_loading.clear();
        self.right_sidebar_note_image_order.clear();
        self.right_sidebar_note_image_failures.clear();
        self.right_sidebar_note_code_highlight = NoteCodeHighlightState::default();
        self.right_sidebar_note_paint_cache = NotePaintCache::default();
        self.ui_shape_caches.borrow_mut().clear_note();
        self.publish_ui_shape_cache_diagnostics();
    }

    fn right_sidebar_file_view_state_key(&self) -> Option<(PathBuf, String)> {
        self.right_sidebar_file_index_root
            .clone()
            .map(|root| (root, self.right_sidebar_file_index_project_name.clone()))
    }

    /// Snapshot the active (root, project)'s Files view (paths/scroll/effective
    /// filter) so it survives idle-release / workspace switch / re-scan. KB-scale,
    /// LRU-bounded; never holds the index.
    fn save_right_sidebar_file_view_state(&mut self) {
        let Some(key) = self.right_sidebar_file_view_state_key() else {
            return;
        };
        let state = RightSidebarFileViewState {
            view: self.right_sidebar_file_view.clone(),
            selected: self.right_sidebar_file_selected.clone(),
            expanded: self.right_sidebar_file_expanded.clone(),
            tree_scroll: self.right_sidebar_file_tree_scroll_offset,
            preview_scroll: self.right_sidebar_file_preview_scroll_offset,
            preview_horizontal: self.right_sidebar_file_preview_horizontal_offset,
            filter: self.right_sidebar_file_applied_filter.clone(),
        };
        if !self
            .right_sidebar_file_view_state_by_root
            .contains_key(&key)
        {
            self.right_sidebar_file_view_state_order
                .push_back(key.clone());
        }
        self.right_sidebar_file_view_state_by_root
            .insert(key, state);
        while self.right_sidebar_file_view_state_order.len() > FILE_VIEW_STATE_CACHE_CAP {
            if let Some(old) = self.right_sidebar_file_view_state_order.pop_front() {
                self.right_sidebar_file_view_state_by_root.remove(&old);
            }
        }
    }

    /// Restore the remembered view for `key`, or apply defaults (tree view, only
    /// the project root expanded, no filter). Re-loads the preview asynchronously
    /// when one was open (the lines were dropped on release), preserving scroll.
    fn restore_right_sidebar_file_view_state(&mut self, key: &(PathBuf, String)) {
        self.right_sidebar_file_expanded_version =
            self.right_sidebar_file_expanded_version.wrapping_add(1);

        let Some(state) = self.right_sidebar_file_view_state_by_root.get(key).cloned() else {
            self.right_sidebar_file_view = RightSidebarFileView::Tree;
            self.right_sidebar_file_selected = None;
            self.right_sidebar_file_expanded.clear();
            self.right_sidebar_file_expanded.insert(path_key(&key.0));
            self.right_sidebar_file_tree_scroll_offset = 0.0;
            self.right_sidebar_file_filter.set_text_end(String::new());
            self.right_sidebar_file_applied_filter.clear();
            self.right_sidebar_file_filter_debounce_until = None;
            return;
        };

        self.right_sidebar_file_expanded = state.expanded;
        self.right_sidebar_file_tree_scroll_offset = state.tree_scroll;
        // Filter: set all three coupled fields so the first frame is consistent.
        self.right_sidebar_file_filter
            .set_text_end(state.filter.clone());
        self.right_sidebar_file_applied_filter = state.filter;
        self.right_sidebar_file_filter_debounce_until = None;

        match (state.view, state.selected) {
            (RightSidebarFileView::Preview, Some(path)) if path.is_file() => {
                self.open_right_sidebar_file_path_inner(
                    path,
                    Some((state.preview_scroll, state.preview_horizontal)),
                );
            }
            _ => {
                self.right_sidebar_file_view = RightSidebarFileView::Tree;
                self.right_sidebar_file_selected = None;
            }
        }
    }

    pub(crate) fn right_sidebar_toggle_icon(&self) -> SvgIcon {
        if self.right_sidebar_collapsed {
            SvgIcon::PanelRightOpen
        } else {
            SvgIcon::PanelRightClose
        }
    }

    pub(crate) fn right_sidebar_has_text_focus(&self) -> bool {
        match self.right_sidebar_mode {
            RightSidebarMode::Chat => self.right_sidebar_file_focus.is_some(),
            RightSidebarMode::Snippets => self.right_sidebar_snippet_focus.is_some(),
            RightSidebarMode::Tasks => self.right_sidebar_note.view.focused,
        }
    }

    pub(crate) fn clear_right_sidebar_text_focus(&mut self) {
        let note_was_focused = self.right_sidebar_note.view.focused;
        self.right_sidebar_snippet_focus = None;
        self.right_sidebar_file_focus = None;
        self.right_sidebar_note.view.focused = false;
        if note_was_focused {
            self.right_sidebar_note.native_text_input_snapshot_key = None;
            if let Some(window) = self.window.as_ref() {
                window.set_native_text_input_snapshot(None);
            }
            self.right_sidebar_note.freeze_live_source();
            self.save_right_sidebar_note_now();
        }
    }

    pub(crate) fn open_new_snippet_editor(&mut self) {
        if self.right_sidebar_mode == RightSidebarMode::Tasks {
            self.clear_right_sidebar_text_focus();
            self.schedule_right_sidebar_note_memory_release();
        }
        self.right_sidebar_mode = RightSidebarMode::Snippets;
        self.right_sidebar_snippet_view = RightSidebarSnippetView::EditNew;
        self.right_sidebar_snippet_scroll_offset = 0.0;
        self.right_sidebar_snippet_title.clear();
        self.right_sidebar_snippet_body.clear();
        self.right_sidebar_snippet_focus = Some(RightSidebarSnippetField::Title);
    }

    pub(crate) fn open_existing_snippet_editor(&mut self, id: &str) {
        let Some(snippet) = crate::snippets::get_snippet(id) else {
            return;
        };
        if self.right_sidebar_mode == RightSidebarMode::Tasks {
            self.clear_right_sidebar_text_focus();
            self.schedule_right_sidebar_note_memory_release();
        }
        self.right_sidebar_mode = RightSidebarMode::Snippets;
        self.right_sidebar_snippet_view = RightSidebarSnippetView::EditExisting(snippet.id);
        self.right_sidebar_snippet_scroll_offset = 0.0;
        self.right_sidebar_snippet_title.set_text_end(snippet.title);
        self.right_sidebar_snippet_body.set_text_end(snippet.body);
        self.right_sidebar_snippet_focus = Some(RightSidebarSnippetField::Title);
    }

    pub(crate) fn close_snippet_editor(&mut self) {
        self.right_sidebar_snippet_view = RightSidebarSnippetView::List;
        self.right_sidebar_snippet_focus = None;
        self.right_sidebar_snippet_scroll_offset = 0.0;
        self.right_sidebar_snippet_title.clear();
        self.right_sidebar_snippet_body.clear();
    }

    pub(crate) fn save_snippet_editor(&mut self) {
        let body = self.right_sidebar_snippet_body.text().trim().to_string();
        if body.is_empty() {
            self.right_sidebar_snippet_focus = Some(RightSidebarSnippetField::Body);
            return;
        }
        let title = self.right_sidebar_snippet_title.text().trim().to_string();
        let result = match self.right_sidebar_snippet_view.clone() {
            RightSidebarSnippetView::List => return,
            RightSidebarSnippetView::EditNew => {
                crate::snippets::create_snippet(title, body).map(Some)
            }
            RightSidebarSnippetView::EditExisting(id) => {
                crate::snippets::update_snippet(&id, title, body)
            }
        };
        match result {
            Ok(Some(_)) => self.close_snippet_editor(),
            Ok(None) => {
                self.right_sidebar_snippet_view = RightSidebarSnippetView::EditNew;
            }
            Err(err) => log::error!("failed to save snippet: {err:#}"),
        }
    }

    pub(crate) fn delete_snippet(&mut self, id: &str) {
        if let Err(err) = crate::snippets::delete_snippet(id) {
            log::error!("failed to delete snippet {id}: {err:#}");
        }
        if matches!(
            self.right_sidebar_snippet_view,
            RightSidebarSnippetView::EditExisting(ref editing_id) if editing_id == id
        ) {
            self.close_snippet_editor();
        }
        self.right_sidebar_snippet_scroll_offset = self
            .right_sidebar_snippet_scroll_offset
            .min(self.right_sidebar_snippet_scroll_max());
    }

    pub(crate) fn scroll_right_sidebar_snippets(&mut self, amount: i16) -> bool {
        if self.right_sidebar_collapsed
            || self.right_sidebar_mode != RightSidebarMode::Snippets
            || self.right_sidebar_snippet_view != RightSidebarSnippetView::List
            || amount == 0
        {
            return false;
        }

        let old = self.right_sidebar_snippet_scroll_offset;
        let max = self.right_sidebar_snippet_scroll_max();
        let steps = amount.unsigned_abs().max(1) as f32;
        let delta = (steps * self.ui_f32(6.0)).min(self.ui_f32(42.0));
        if amount < 0 {
            self.right_sidebar_snippet_scroll_offset =
                (self.right_sidebar_snippet_scroll_offset + delta).clamp(0.0, max);
        } else {
            self.right_sidebar_snippet_scroll_offset =
                (self.right_sidebar_snippet_scroll_offset - delta).clamp(0.0, max);
        }
        self.show_right_sidebar_snippet_scrollbar();
        (old - self.right_sidebar_snippet_scroll_offset).abs() > f32::EPSILON
    }

    pub(crate) fn scroll_right_sidebar_files_by(&mut self, delta: f32) -> bool {
        let offset = if self.right_sidebar_remote_files.target.is_some() {
            &mut self.right_sidebar_remote_file_tree_scroll_offset
        } else {
            &mut self.right_sidebar_file_tree_scroll_offset
        };
        let old = *offset;
        *offset = (*offset + delta).max(0.0);
        (old - *offset).abs() > f32::EPSILON
    }

    pub(crate) fn scroll_right_sidebar_file_preview_by(&mut self, delta: f32) -> bool {
        let old = self.right_sidebar_file_preview_scroll_offset;
        let max = self.right_sidebar_file_preview_scroll_max();
        self.right_sidebar_file_preview_scroll_offset =
            (self.right_sidebar_file_preview_scroll_offset + delta).clamp(0.0, max);
        (old - self.right_sidebar_file_preview_scroll_offset).abs() > f32::EPSILON
    }

    pub(crate) fn scroll_right_sidebar_file_preview_horizontal(&mut self, amount: i16) -> bool {
        if self.right_sidebar_collapsed
            || self.right_sidebar_mode != RightSidebarMode::Chat
            || self.right_sidebar_file_view != RightSidebarFileView::Preview
            || (self.right_sidebar_file_selected.is_none()
                && self.right_sidebar_remote_files.selected.is_none())
            || amount == 0
        {
            return false;
        }

        let old = self.right_sidebar_file_preview_horizontal_offset;
        let max = self.right_sidebar_file_preview_horizontal_scroll_max();
        let steps = amount.unsigned_abs().max(1) as usize;
        // The horizontal offset is measured in character columns (the cell
        // width already tracks DPI), so the step stays unscaled.
        let delta = steps.saturating_mul(4).min(32);
        if amount < 0 {
            self.right_sidebar_file_preview_horizontal_offset = self
                .right_sidebar_file_preview_horizontal_offset
                .saturating_add(delta)
                .min(max);
        } else {
            self.right_sidebar_file_preview_horizontal_offset = self
                .right_sidebar_file_preview_horizontal_offset
                .saturating_sub(delta);
        }
        old != self.right_sidebar_file_preview_horizontal_offset
    }

    pub(crate) fn open_right_sidebar_file_path(&mut self, path: PathBuf) {
        self.open_right_sidebar_file_path_inner(path, None);
    }

    /// `restore_scroll` re-applies a remembered preview scroll once the async
    /// load completes (consumed in `apply_right_sidebar_file_preview_result`);
    /// `None` (a normal click) resets to the top.
    fn open_right_sidebar_file_path_inner(
        &mut self,
        path: PathBuf,
        restore_scroll: Option<(f32, usize)>,
    ) {
        self.right_sidebar_file_preview_restore_scroll = restore_scroll;
        self.right_sidebar_file_focus = None;
        if path.is_dir() {
            let key = path_key(&path);
            if self.right_sidebar_file_expanded.contains(&key) {
                self.right_sidebar_file_expanded.remove(&key);
            } else {
                self.right_sidebar_file_expanded.insert(key);
            }
            self.right_sidebar_file_expanded_version =
                self.right_sidebar_file_expanded_version.wrapping_add(1);
            return;
        }

        if !self.right_sidebar_file_preview_active() {
            let max_tree_for_preview = self
                .right_sidebar_pane_total_max_width()
                .saturating_sub(self.ui_px(FILE_PREVIEW_PANE_MIN_WIDTH))
                .max(self.ui_px(RIGHT_SIDEBAR_MIN_WIDTH));
            self.right_sidebar_file_tree_width = self
                .right_sidebar_width
                .clamp(self.ui_px(RIGHT_SIDEBAR_MIN_WIDTH), max_tree_for_preview);
        }
        self.right_sidebar_file_selected = Some(path.clone());
        self.right_sidebar_file_preview_generation =
            self.right_sidebar_file_preview_generation.wrapping_add(1);
        let generation = self.right_sidebar_file_preview_generation;
        self.right_sidebar_file_preview_highlight_cancel
            .store(1, AtomicOrdering::Relaxed);
        self.right_sidebar_file_preview_highlight_cancel = Arc::new(AtomicUsize::new(0));
        let highlight_cancel = Arc::clone(&self.right_sidebar_file_preview_highlight_cancel);
        self.right_sidebar_file_preview_lines.clear();
        self.right_sidebar_file_preview_max_columns = 0;
        self.clear_right_sidebar_file_preview_slice_cache();
        self.right_sidebar_file_preview_image = None;
        self.right_sidebar_file_preview_message = Some("Loading file preview...".to_string());
        self.right_sidebar_file_preview_truncated = false;
        self.right_sidebar_file_preview_selection = None;
        self.right_sidebar_file_preview_scroll_offset = 0.0;
        self.right_sidebar_file_preview_horizontal_offset = 0;
        self.right_sidebar_file_view = RightSidebarFileView::Preview;
        self.prefetch_right_sidebar_file_open_with(&path);

        let Some(window) = self.window.as_ref().cloned() else {
            self.right_sidebar_file_preview_message = Some("Window is unavailable".to_string());
            return;
        };
        let use_dark_syntax_theme = matches!(
            crate::native_settings::effective_appearance(),
            window::Appearance::Dark | window::Appearance::DarkHighContrast
        );
        let load_path = path.clone();
        promise::spawn::spawn(async move {
            let result = promise::spawn::spawn_into_new_thread(move || {
                Ok(load_right_sidebar_file_preview_with_cancellation(
                    &load_path,
                    use_dark_syntax_theme,
                    Some(highlight_cancel.as_ref()),
                ))
            })
            .await
            .unwrap_or_else(|err| RightSidebarLoadedFilePreview {
                lines: Vec::new(),
                image: None,
                message: Some(format!("Unable to load file preview: {err}")),
                truncated: false,
            });
            window.notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                term_window.apply_right_sidebar_file_preview_result(generation, path, result);
            })));
        })
        .detach();
    }

    pub(crate) fn trash_sidebar_file(&mut self, path: &Path) {
        if let Err(err) = trash::delete(path) {
            log::error!("failed to move {} to trash: {err:#}", path.display());
            return;
        }
        if self.right_sidebar_file_selected.as_deref() == Some(path) {
            // Closing the preview narrows the sidebar, so the terminal must
            // reflow or it keeps rendering under the old, wider sidebar.
            let previous_width = self.right_sidebar_width();
            self.close_right_sidebar_file_preview();
            if let Some(window) = self.window.as_ref().cloned() {
                if self.right_sidebar_width() != previous_width {
                    let dimensions = self.dimensions;
                    self.apply_dimensions(&dimensions, None, &window);
                }
                window.invalidate();
            }
        }
        self.force_right_sidebar_file_rescan();
    }

    pub(crate) fn close_right_sidebar_file_preview(&mut self) {
        self.right_sidebar_file_view = RightSidebarFileView::Tree;
        self.right_sidebar_file_selected = None;
        self.right_sidebar_file_preview_generation =
            self.right_sidebar_file_preview_generation.wrapping_add(1);
        self.right_sidebar_file_preview_highlight_cancel
            .store(1, AtomicOrdering::Relaxed);
        self.right_sidebar_file_preview_highlight_cancel = Arc::new(AtomicUsize::new(0));
        self.right_sidebar_file_preview_lines.clear();
        self.right_sidebar_file_preview_max_columns = 0;
        self.clear_right_sidebar_file_preview_slice_cache();
        self.right_sidebar_file_preview_image = None;
        self.right_sidebar_file_preview_message = None;
        self.right_sidebar_file_preview_truncated = false;
        self.right_sidebar_file_preview_selection = None;
        self.right_sidebar_file_preview_scroll_offset = 0.0;
        self.right_sidebar_file_preview_horizontal_offset = 0;
    }

    fn clear_right_sidebar_file_preview_slice_cache(&self) {
        self.right_sidebar_file_preview_slice_cache
            .borrow_mut()
            .clear();
        self.right_sidebar_file_preview_slice_cache_order
            .borrow_mut()
            .clear();
        self.right_sidebar_file_preview_line_color_cache
            .borrow_mut()
            .clear();
        self.right_sidebar_file_preview_line_color_cache_order
            .borrow_mut()
            .clear();
    }

    fn cached_note_code_highlights(
        &mut self,
        code: &ProjectedCodeBlock,
        use_dark_theme: bool,
    ) -> Arc<Vec<Vec<LinearRgba>>> {
        let Some(key) = note_code_highlight_key(code) else {
            return Arc::new(vec![vec![]; note_code_line_count(&code.text)]);
        };
        if let Some(entry) = self
            .right_sidebar_note_code_highlight
            .cached(key, &code.text)
        {
            self.right_sidebar_note_code_highlight
                .cancel_block_work(code.source.start);
            self.right_sidebar_note_code_highlight
                .remember_block(code.source.start, Arc::clone(&entry));
            return entry.colors(use_dark_theme);
        }

        let previous = self
            .right_sidebar_note_code_highlight
            .previous_for_block(code.source.start, key.language);
        if let Some(generation) = self
            .right_sidebar_note_code_highlight
            .schedule_block(code.source.start, key)
        {
            if let Some(window) = self.window.as_ref().cloned() {
                let code = code.clone();
                promise::spawn::spawn(async move {
                    smol::Timer::after(Duration::from_millis(NOTE_CODE_HIGHLIGHT_DEBOUNCE_MS))
                        .await;
                    window.notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                        term_window.start_note_code_highlight(
                            code.source.start,
                            key,
                            generation,
                            code,
                        );
                    })));
                })
                .detach();
            } else {
                self.right_sidebar_note_code_highlight
                    .scheduled
                    .remove(&code.source.start);
            }
        }

        previous
            .map(|entry| entry.colors(use_dark_theme))
            .unwrap_or_else(|| Arc::new(vec![vec![]; note_code_line_count(&code.text)]))
    }

    fn current_note_code_matches(
        &self,
        block_start: usize,
        key: NoteCodeHighlightKey,
        source: &str,
    ) -> bool {
        self.right_sidebar_note
            .projection
            .objects
            .iter()
            .find_map(|object| match object {
                ProjectedObject::CodeBlock(code) if code.source.start == block_start => Some(code),
                _ => None,
            })
            .is_some_and(|code| {
                note_code_highlight_key(code) == Some(key) && code.text.as_str() == source
            })
    }

    fn start_note_code_highlight(
        &mut self,
        block_start: usize,
        key: NoteCodeHighlightKey,
        generation: u64,
        code: ProjectedCodeBlock,
    ) {
        if self
            .right_sidebar_note_code_highlight
            .scheduled
            .get(&block_start)
            .copied()
            != Some((key, generation))
        {
            return;
        }
        self.right_sidebar_note_code_highlight
            .scheduled
            .remove(&block_start);
        if !self.current_note_code_matches(block_start, key, &code.text) {
            return;
        }
        if let Some(entry) = self
            .right_sidebar_note_code_highlight
            .cached(key, &code.text)
        {
            self.right_sidebar_note_code_highlight
                .remember_block(block_start, entry);
            self.schedule_note_code_highlight_repaint();
            return;
        }
        let Some(window) = self.window.as_ref().cloned() else {
            return;
        };
        let cancellation = Arc::new(AtomicUsize::new(0));
        self.right_sidebar_note_code_highlight
            .in_flight
            .insert(block_start, (key, Arc::clone(&cancellation)));
        let source: Arc<str> = Arc::from(code.text.as_str());
        syntax_highlight_pool().spawn(move || {
            let stage = crate::input_diagnostics::StageTimer::begin("note_highlight");
            let result = note_code_highlight_pair(&code, Some(cancellation.as_ref()));
            stage.finish(result.is_some());
            window.notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                term_window.apply_note_code_highlight_result(block_start, key, source, result);
            })));
        });
    }

    fn apply_note_code_highlight_result(
        &mut self,
        block_start: usize,
        key: NoteCodeHighlightKey,
        source: Arc<str>,
        result: Option<(Vec<Vec<LinearRgba>>, Vec<Vec<LinearRgba>>)>,
    ) {
        if self
            .right_sidebar_note_code_highlight
            .in_flight
            .get(&block_start)
            .is_some_and(|(pending, _)| *pending == key)
        {
            self.right_sidebar_note_code_highlight
                .in_flight
                .remove(&block_start);
        }
        let Some((light, dark)) = result else {
            return;
        };
        let current_matches = self.current_note_code_matches(block_start, key, &source);
        let entry = Arc::new(NoteCodeHighlightEntry {
            key,
            source,
            light: Arc::new(light),
            dark: Arc::new(dark),
        });
        self.right_sidebar_note_code_highlight
            .insert_cache(Arc::clone(&entry));
        if current_matches {
            self.right_sidebar_note_code_highlight
                .remember_block(block_start, entry);
            self.schedule_note_code_highlight_repaint();
        }
    }

    fn schedule_note_code_highlight_repaint(&mut self) {
        if self.right_sidebar_note_code_highlight.repaint_scheduled {
            return;
        }
        let Some(window) = self.window.as_ref().cloned() else {
            return;
        };
        self.right_sidebar_note_code_highlight.repaint_scheduled = true;
        promise::spawn::spawn(async move {
            smol::Timer::after(Duration::from_millis(NOTE_CODE_HIGHLIGHT_REPAINT_MS)).await;
            window.notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                term_window
                    .right_sidebar_note_code_highlight
                    .repaint_scheduled = false;
                if term_window.right_sidebar_note_visible() {
                    term_window.invalidate_window();
                }
            })));
        })
        .detach();
    }

    /// Per-byte colours for a whole preview line, keyed by (generation, line
    /// index) so horizontal scrolling reuses the same colour list. Indexed by
    /// byte offset into `line.plain` so a glyph's `cluster` maps straight to its
    /// colour without rebuilding a byte→char table every frame.
    fn cached_full_line_colors(
        &self,
        line_index: usize,
        line: &RightSidebarFilePreviewLine,
    ) -> Rc<Vec<LinearRgba>> {
        let key = (self.right_sidebar_file_preview_generation, line_index);
        if let Some(value) = self
            .right_sidebar_file_preview_line_color_cache
            .borrow()
            .get(&key)
            .cloned()
        {
            return value;
        }
        let colors = Rc::new(full_line_colors_by_byte(line));
        {
            let mut cache = self
                .right_sidebar_file_preview_line_color_cache
                .borrow_mut();
            let mut order = self
                .right_sidebar_file_preview_line_color_cache_order
                .borrow_mut();
            if !cache.contains_key(&key) {
                order.push_back(key);
            }
            cache.insert(key, Rc::clone(&colors));
            while order.len() > FILE_PREVIEW_SLICE_CACHE_CAPACITY {
                if let Some(old_key) = order.pop_front() {
                    cache.remove(&old_key);
                }
            }
        }
        colors
    }

    fn cached_right_sidebar_file_preview_slice<F>(
        &self,
        key: RightSidebarFilePreviewSliceCacheKey,
        build: F,
    ) -> RightSidebarFilePreviewSliceCacheValue
    where
        F: FnOnce() -> RightSidebarFilePreviewSliceCacheValue,
    {
        if let Some(value) = self
            .right_sidebar_file_preview_slice_cache
            .borrow()
            .get(&key)
            .cloned()
        {
            return value;
        }

        let value = build();
        {
            let mut cache = self.right_sidebar_file_preview_slice_cache.borrow_mut();
            let mut order = self
                .right_sidebar_file_preview_slice_cache_order
                .borrow_mut();
            if !cache.contains_key(&key) {
                order.push_back(key.clone());
            }
            cache.insert(key, value.clone());
            while order.len() > FILE_PREVIEW_SLICE_CACHE_CAPACITY {
                if let Some(old_key) = order.pop_front() {
                    cache.remove(&old_key);
                }
            }
        }
        value
    }

    fn apply_right_sidebar_file_preview_result(
        &mut self,
        generation: u64,
        path: PathBuf,
        result: RightSidebarLoadedFilePreview,
    ) {
        if generation != self.right_sidebar_file_preview_generation
            || self.right_sidebar_file_selected.as_ref() != Some(&path)
        {
            return;
        }

        self.right_sidebar_file_preview_lines = result.lines;
        self.right_sidebar_file_preview_max_columns = self
            .right_sidebar_file_preview_lines
            .iter()
            .map(|line| line.char_count)
            .max()
            .unwrap_or(0);
        self.clear_right_sidebar_file_preview_slice_cache();
        self.right_sidebar_file_preview_image = result.image;
        self.right_sidebar_file_preview_message = result.message;
        self.right_sidebar_file_preview_truncated = result.truncated;
        self.right_sidebar_file_preview_selection = None;
        // When restoring a remembered preview, re-apply the saved scroll once the
        // lines arrive (clamped on paint); otherwise reset to the top.
        let (scroll, horizontal) = self
            .right_sidebar_file_preview_restore_scroll
            .take()
            .unwrap_or((0.0, 0));
        self.right_sidebar_file_preview_scroll_offset = scroll;
        self.right_sidebar_file_preview_horizontal_offset = horizontal;
        self.invalidate_window();
    }

    fn prefetch_right_sidebar_file_open_with(&mut self, path: &Path) {
        let key = right_sidebar_open_with_cache_key(path);
        self.start_right_sidebar_open_with_load_if_needed(&key, path);
    }

    pub(crate) fn open_right_sidebar_selected_file_with_current_app(&self) {
        let Some(path) = self.right_sidebar_file_selected.as_ref() else {
            return;
        };

        let key = right_sidebar_open_with_cache_key(path);
        if let Some(app) = self.current_right_sidebar_open_with_app(&key) {
            wezterm_open_url::open_path_with_candidate(path, &app.id);
        } else {
            wezterm_open_url::open_url(&path.to_string_lossy());
        }
    }

    pub(crate) fn show_right_sidebar_file_open_with_menu(
        &mut self,
        context: &dyn WindowOps,
        anchor: window::Point,
    ) {
        let Some(path) = self.right_sidebar_file_selected.clone() else {
            return;
        };
        let key = right_sidebar_open_with_cache_key(&path);
        self.start_right_sidebar_open_with_load_if_needed(&key, &path);
        let items = self.right_sidebar_open_with_menu_items(&key, &path);
        self.show_term_context_menu(context, anchor, items);
    }

    pub(crate) fn show_right_sidebar_file_context_menu(
        &mut self,
        context: &dyn WindowOps,
        anchor: window::Point,
        path: PathBuf,
    ) {
        // Deliberately not touching right_sidebar_file_selected: that field
        // tracks the open preview, and every action here carries its own path.
        let items = self.right_sidebar_file_context_menu_items(&path);
        self.show_term_context_menu(context, anchor, items);
    }

    /// File-operations menu for right-clicking a Files row. Open With lives
    /// in the preview toolbar only, so this menu stays about the file itself.
    fn right_sidebar_file_context_menu_items(&self, path: &Path) -> Vec<ContextMenuItem> {
        let path_string = path.to_string_lossy().to_string();
        let mut items = Vec::new();
        if !path.is_dir() {
            items.push(ContextMenuItem::item_with_icon(
                "Open",
                ContextMenuIcon::ExternalLink,
                KeyAssignment::OpenFileWithSystemDefault(path_string.clone()),
            ));
        }
        items.push(ContextMenuItem::item_with_icon(
            super::context_menu::reveal_in_folder_label(),
            ContextMenuIcon::Folder,
            KeyAssignment::RevealFileInFolder(path_string.clone()),
        ));
        items.push(ContextMenuItem::item_with_icon(
            "Copy Path",
            ContextMenuIcon::Copy,
            KeyAssignment::CopyFilePathToClipboard(path_string.clone()),
        ));
        items.push(ContextMenuItem::Separator);
        items.push(ContextMenuItem::item_with_icon(
            "Rename...",
            ContextMenuIcon::Edit,
            KeyAssignment::RenameSidebarFile(path_string.clone()),
        ));
        items.push(ContextMenuItem::item_with_icon(
            "Move to Trash",
            ContextMenuIcon::Delete,
            KeyAssignment::TrashSidebarFile(path_string),
        ));
        items
    }

    fn start_right_sidebar_open_with_load_if_needed(&mut self, key: &str, path: &Path) {
        if self
            .right_sidebar_open_with_cache
            .get(key)
            .is_some_and(|entry| {
                matches!(
                    entry,
                    RightSidebarOpenWithCacheEntry::Loading(_)
                        | RightSidebarOpenWithCacheEntry::Ready(_)
                )
            })
        {
            return;
        }

        let Some(window) = self.window.as_ref().cloned() else {
            self.right_sidebar_open_with_cache
                .insert(key.to_string(), RightSidebarOpenWithCacheEntry::Failed);
            return;
        };

        self.right_sidebar_open_with_generation =
            self.right_sidebar_open_with_generation.wrapping_add(1);
        let generation = self.right_sidebar_open_with_generation;
        let key = key.to_string();
        let path = path.to_path_buf();
        self.right_sidebar_open_with_cache.insert(
            key.clone(),
            RightSidebarOpenWithCacheEntry::Loading(generation),
        );

        promise::spawn::spawn(async move {
            let load_path = path.clone();
            let candidates = promise::spawn::spawn_into_new_thread(move || {
                Ok(wezterm_open_url::open_with_candidates(&load_path))
            })
            .await;
            window.notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                term_window
                    .apply_right_sidebar_open_with_candidates(generation, key, path, candidates);
            })));
        })
        .detach();
    }

    fn apply_right_sidebar_open_with_candidates(
        &mut self,
        generation: u64,
        key: String,
        _path: PathBuf,
        candidates: anyhow::Result<Vec<wezterm_open_url::OpenWithCandidate>>,
    ) {
        if !matches!(
            self.right_sidebar_open_with_cache.get(&key),
            Some(RightSidebarOpenWithCacheEntry::Loading(loading_generation))
                if *loading_generation == generation
        ) {
            return;
        }

        let entry = match candidates {
            Ok(candidates) => RightSidebarOpenWithCacheEntry::Ready(candidates),
            Err(_) => RightSidebarOpenWithCacheEntry::Failed,
        };
        self.right_sidebar_open_with_cache
            .insert(key.clone(), entry);
        self.invalidate_window();
    }

    fn right_sidebar_open_with_menu_items(&self, key: &str, path: &Path) -> Vec<ContextMenuItem> {
        let path_string = path.to_string_lossy().to_string();
        let current_id = self
            .current_right_sidebar_open_with_app(key)
            .map(|app| app.id);
        // Even with no platform candidates (Loading/Failed) the menu still
        // offers the user's custom apps and the "Other…" picker.
        let mut candidates = match self.right_sidebar_open_with_cache.get(key) {
            Some(RightSidebarOpenWithCacheEntry::Ready(candidates)) => candidates.clone(),
            _ => Vec::new(),
        };

        let custom_apps = crate::native_settings::right_sidebar_custom_open_with_apps();
        let custom_ids: std::collections::HashSet<String> =
            custom_apps.iter().map(|app| app.id.clone()).collect();
        for app in custom_apps {
            if candidates.iter().any(|candidate| candidate.id == app.id) {
                // Platform entry wins so is_default stays accurate
                continue;
            }
            // Drop custom entries whose app was uninstalled; `desktop:` ids
            // (Linux) aren't paths, so they skip the existence check.
            if !app.id.starts_with("desktop:") && !Path::new(&app.id).exists() {
                continue;
            }
            candidates.push(wezterm_open_url::OpenWithCandidate {
                id: app.id,
                label: app.label,
                icon_path: None,
                is_default: false,
            });
        }

        let saved_id = self
            .right_sidebar_open_with_app
            .as_ref()
            .map(|app| app.id.clone());
        candidates.retain(|candidate| {
            open_with_candidate_allowed(candidate, &custom_ids, saved_id.as_deref())
        });

        let mut items: Vec<ContextMenuItem> =
            sorted_open_with_candidates(candidates, current_id.as_deref())
                .into_iter()
                .filter(|candidate| current_id.as_deref() != Some(candidate.id.as_str()))
                .map(|candidate| {
                    ContextMenuItem::item_with_icon(
                        format!("Open With {}", candidate.label),
                        ContextMenuIcon::Application,
                        KeyAssignment::OpenFileWith {
                            path: path_string.clone(),
                            app: candidate.id,
                            label: candidate.label,
                        },
                    )
                })
                .collect();

        if !items.is_empty() {
            items.push(ContextMenuItem::Separator);
        }
        items.push(ContextMenuItem::item_with_icon(
            "Open With Other…",
            ContextMenuIcon::Application,
            KeyAssignment::PickOpenFileWithApp(path_string),
        ));
        items
    }

    /// Completion of the "Open With Other…" app picker: persist the picked
    /// app as a custom entry + current preference, then open the file.
    pub(crate) fn finish_pick_open_with_app(&mut self, file_path: &str, app_path: &Path) {
        let (id, label) = wezterm_open_url::app_candidate_for_picked_path(app_path);
        if id.is_empty() {
            return;
        }
        let app = crate::native_settings::NativeOpenWithApp {
            id: id.clone(),
            label,
        };
        if let Err(err) =
            crate::native_settings::add_right_sidebar_custom_open_with_app(app.clone())
        {
            log::error!("failed to save custom Open With app: {err:#}");
        }
        self.right_sidebar_open_with_app = Some(app.clone());
        if let Err(err) = crate::native_settings::save_right_sidebar_open_with_app(app) {
            log::error!("failed to save Open With app selection: {err:#}");
        }
        wezterm_open_url::open_path_with_candidate(Path::new(file_path), &id);
        self.invalidate_window();
    }

    fn current_right_sidebar_open_with_app(
        &self,
        key: &str,
    ) -> Option<crate::native_settings::NativeOpenWithApp> {
        if let Some(app) = self.right_sidebar_open_with_app.clone() {
            return Some(app);
        }

        let Some(RightSidebarOpenWithCacheEntry::Ready(candidates)) =
            self.right_sidebar_open_with_cache.get(key)
        else {
            return None;
        };

        if let Some(candidate) = candidates.iter().find(|candidate| candidate.is_default) {
            return Some(crate::native_settings::NativeOpenWithApp {
                id: candidate.id.clone(),
                label: candidate.label.clone(),
            });
        }

        sorted_open_with_candidates(candidates.clone(), None)
            .into_iter()
            .next()
            .map(|candidate| crate::native_settings::NativeOpenWithApp {
                id: candidate.id,
                label: candidate.label,
            })
    }

    fn right_sidebar_current_open_with_app_label(&self, path: &Path) -> Option<String> {
        let key = right_sidebar_open_with_cache_key(path);
        self.current_right_sidebar_open_with_app(&key)
            .map(|app| app.label)
    }

    pub(crate) fn reveal_right_sidebar_selected_file(&self) {
        if let Some(path) = self.right_sidebar_file_selected.as_ref() {
            wezterm_open_url::reveal_path(path);
        }
    }

    pub(crate) fn copy_right_sidebar_selected_file_preview_text(&mut self) {
        if let Some(text) = self.right_sidebar_file_preview_selected_text() {
            self.copy_to_clipboard(ClipboardCopyDestination::Clipboard, text);
            return;
        }

        if self.right_sidebar_file_preview_lines.is_empty() {
            return;
        }

        let mut text = String::new();
        for (idx, line) in self.right_sidebar_file_preview_lines.iter().enumerate() {
            if idx > 0 {
                text.push('\n');
            }
            text.push_str(&line.plain);
        }
        self.copy_to_clipboard(ClipboardCopyDestination::Clipboard, text);
    }

    pub(crate) fn right_sidebar_file_preview_selected_text(&self) -> Option<String> {
        if !self.right_sidebar_file_preview_active() {
            return None;
        }
        let (start, end) = self.right_sidebar_file_preview_selection_range()?;
        let mut text = String::new();
        for line_idx in start.line..=end.line {
            let Some(line) = self.right_sidebar_file_preview_lines.get(line_idx) else {
                break;
            };
            if line_idx > start.line {
                text.push('\n');
            }
            let line_start = if line_idx == start.line {
                start.column
            } else {
                0
            };
            let line_end = if line_idx == end.line {
                end.column
            } else {
                line.char_count
            };
            if line_end > line_start {
                text.push_str(&preview_text_range(&line.plain, line_start, line_end));
            }
        }
        if text.is_empty() {
            None
        } else {
            Some(text)
        }
    }

    fn right_sidebar_file_preview_selection_range(
        &self,
    ) -> Option<(
        RightSidebarFilePreviewSelectionPoint,
        RightSidebarFilePreviewSelectionPoint,
    )> {
        let selection = self.right_sidebar_file_preview_selection?;
        if selection.anchor == selection.focus {
            return None;
        }
        if selection.anchor <= selection.focus {
            Some((selection.anchor, selection.focus))
        } else {
            Some((selection.focus, selection.anchor))
        }
    }

    fn right_sidebar_file_preview_text_point_for_coords(
        &self,
        x: isize,
        y: isize,
    ) -> Option<RightSidebarFilePreviewSelectionPoint> {
        if self.right_sidebar_file_preview_image.is_some()
            || self.right_sidebar_file_preview_message.is_some()
        {
            return None;
        }
        let preview_metrics = self.right_sidebar_file_preview_render_metrics();
        let metrics = self.right_sidebar_file_preview_body_metrics(preview_metrics)?;
        let visible_height =
            self.right_sidebar_file_preview_effective_visible_height(metrics, preview_metrics);
        if visible_height == 0 {
            return None;
        }

        let line_height = metrics.line_height.max(1);
        let scroll_offset = self.right_sidebar_file_preview_scroll_offset.clamp(
            0.0,
            self.right_sidebar_file_preview_scroll_max_with_metrics(preview_metrics),
        );
        let relative_y = (y as f32 - metrics.y as f32 + scroll_offset).max(0.0);
        let line_count = preview_line_count(&self.right_sidebar_file_preview_lines).max(1);
        let line = (relative_y / line_height as f32).floor() as usize;
        let line = line.min(line_count.saturating_sub(1));
        let line_len = self
            .right_sidebar_file_preview_lines
            .get(line)
            .map(|line| line.char_count)
            .unwrap_or(0);

        let (_, _, text_x, text_width) =
            self.right_sidebar_file_preview_text_layout(metrics, preview_metrics);
        let cell_width = preview_metrics.cell_size.width.max(1) as f32;
        let relative_x = (x as f32 - text_x as f32).max(0.0);
        let visible_column = (relative_x / cell_width).round().max(0.0) as usize;
        let column = self
            .right_sidebar_file_preview_horizontal_offset
            .saturating_add(visible_column)
            .min(line_len);

        if x >= text_x.saturating_add(text_width) as isize {
            return Some(RightSidebarFilePreviewSelectionPoint {
                line,
                column: line_len,
            });
        }
        Some(RightSidebarFilePreviewSelectionPoint { line, column })
    }

    pub(crate) fn begin_right_sidebar_file_preview_selection(
        &mut self,
        x: isize,
        y: isize,
    ) -> bool {
        let Some(point) = self.right_sidebar_file_preview_text_point_for_coords(x, y) else {
            self.right_sidebar_file_preview_selection = None;
            return false;
        };
        self.clear_right_sidebar_text_focus();
        self.right_sidebar_file_preview_selection = Some(RightSidebarFilePreviewSelection {
            anchor: point,
            focus: point,
        });
        true
    }

    pub(crate) fn update_right_sidebar_file_preview_selection(
        &mut self,
        x: isize,
        y: isize,
    ) -> bool {
        let Some(point) = self.right_sidebar_file_preview_text_point_for_coords(x, y) else {
            return false;
        };
        let Some(selection) = self.right_sidebar_file_preview_selection.as_mut() else {
            return false;
        };
        if selection.focus == point {
            return false;
        }
        selection.focus = point;
        true
    }

    fn mark_right_sidebar_file_filter_changed(&mut self) {
        self.right_sidebar_file_tree_scroll_offset = 0.0;
        if self.right_sidebar_file_filter.text() == self.right_sidebar_file_applied_filter {
            self.right_sidebar_file_filter_debounce_until = None;
        } else {
            self.right_sidebar_file_filter_debounce_until =
                Some(Instant::now() + Duration::from_millis(FILE_FILTER_DEBOUNCE_MS));
        }
    }

    fn right_sidebar_file_filter_for_tree(&mut self) -> String {
        let current = self.right_sidebar_file_filter.text().to_string();
        if current == self.right_sidebar_file_applied_filter {
            self.right_sidebar_file_filter_debounce_until = None;
            return self.right_sidebar_file_applied_filter.clone();
        }

        if let Some(until) = self.right_sidebar_file_filter_debounce_until {
            let now = Instant::now();
            if now < until {
                self.update_next_frame_time(Some(until));
                return self.right_sidebar_file_applied_filter.clone();
            }
        }

        self.right_sidebar_file_applied_filter = current;
        self.right_sidebar_file_filter_debounce_until = None;
        self.right_sidebar_file_tree_scroll_offset = 0.0;
        self.right_sidebar_file_applied_filter.clone()
    }

    fn clear_right_sidebar_file_search(&mut self) {
        if let Some(cancel) = self.right_sidebar_file_search_cancel.take() {
            cancel.store(true, AtomicOrdering::Relaxed);
        }
        self.right_sidebar_file_search_query.clear();
        self.right_sidebar_file_search_rows.clear();
        self.right_sidebar_file_searching = false;
    }

    fn schedule_right_sidebar_reflow(&self) {
        let Some(window) = self.window.as_ref().cloned() else {
            return;
        };

        window.notify(TermWindowNotif::Apply(Box::new(|term_window| {
            if let Some(window) = term_window.window.as_ref().cloned() {
                let dimensions = term_window.dimensions;
                term_window.apply_dimensions(&dimensions, None, &window);
            }
            term_window.invalidate_window();
        })));
    }

    fn start_right_sidebar_file_index_if_needed(&mut self, root: &RightSidebarFileRoot) {
        let same_root = self
            .right_sidebar_file_index_root
            .as_ref()
            .is_some_and(|path| path == &root.path)
            && self.right_sidebar_file_index_project_name == root.project_name;
        if same_root
            && matches!(
                self.right_sidebar_file_index_status,
                RightSidebarFileIndexStatus::Indexing | RightSidebarFileIndexStatus::Ready
            )
        {
            return;
        }

        let new_key = (root.path.clone(), root.project_name.clone());

        if !same_root {
            let previous_width = self.right_sidebar_width();
            // Remember the outgoing root's view, then restore the incoming one
            // (replaces the old blanket clear of expanded + preview).
            self.save_right_sidebar_file_view_state();
            self.close_right_sidebar_file_preview();
            self.right_sidebar_file_browse_rows.clear();
            self.right_sidebar_file_browse_cache_key = None;
            self.right_sidebar_file_index_root = Some(root.path.clone());
            self.right_sidebar_file_index_project_name = root.project_name.clone();
            self.restore_right_sidebar_file_view_state(&new_key);
            if self.right_sidebar_width() != previous_width {
                self.schedule_right_sidebar_reflow();
            }
        } else {
            // Same root, status Empty/Failed (e.g. after idle-release): restore
            // the view the release tore down before rebuilding.
            self.restore_right_sidebar_file_view_state(&new_key);
        }

        self.spawn_right_sidebar_file_index_build(
            root.path.clone(),
            root.project_name.clone(),
            false,
            false,
        );
    }

    fn clear_right_sidebar_file_root_for_unavailable_project(&mut self) {
        if self.right_sidebar_file_index_root.is_some() {
            self.save_right_sidebar_file_view_state();
        }
        let previous_width = self.right_sidebar_width();
        if let Some(cancel) = self.right_sidebar_file_index_cancel.take() {
            cancel.store(true, AtomicOrdering::Relaxed);
        }
        if let Some(cancel) = self.right_sidebar_file_search_cancel.take() {
            cancel.store(true, AtomicOrdering::Relaxed);
        }
        self.close_right_sidebar_file_preview();
        self.clear_right_sidebar_file_search();
        self.right_sidebar_file_index_root = None;
        self.right_sidebar_file_index_project_name.clear();
        self.right_sidebar_file_index_status = RightSidebarFileIndexStatus::Empty;
        self.right_sidebar_file_index = None;
        self.right_sidebar_file_browse_rows.clear();
        self.right_sidebar_file_browse_cache_key = None;
        self.right_sidebar_file_refreshing = false;
        if self.right_sidebar_width() != previous_width {
            self.schedule_right_sidebar_reflow();
        }
    }

    fn sync_right_sidebar_file_root_for_current_workspace(
        &mut self,
    ) -> Result<RightSidebarFileRoot, String> {
        let root = match self.active_local_project_for_files() {
            Ok(root) => root,
            Err(err) => {
                self.clear_right_sidebar_file_root_for_unavailable_project();
                return Err(err);
            }
        };
        self.start_right_sidebar_file_index_if_needed(&root);
        Ok(root)
    }

    /// Build (or refresh) the file index on a background thread. `fresh` bypasses
    /// the shared-registry reuse (forces a real disk scan); `keep_showing` leaves
    /// the current tree + status on screen and only swaps the new `Arc` in on
    /// apply (no "Indexing files…" flicker during a refresh).
    fn spawn_right_sidebar_file_index_build(
        &mut self,
        root_path: PathBuf,
        project_name: String,
        fresh: bool,
        keep_showing: bool,
    ) {
        let Some(window) = self.window.as_ref().cloned() else {
            self.right_sidebar_file_index_status =
                RightSidebarFileIndexStatus::Failed("Window is unavailable".to_string());
            return;
        };

        if let Some(cancel) = self.right_sidebar_file_index_cancel.take() {
            cancel.store(true, AtomicOrdering::Relaxed);
        }
        let index_cancel = Arc::new(AtomicBool::new(false));
        self.right_sidebar_file_index_cancel = Some(index_cancel.clone());
        self.right_sidebar_file_index_generation =
            self.right_sidebar_file_index_generation.wrapping_add(1);
        let generation = self.right_sidebar_file_index_generation;

        self.right_sidebar_file_index_root = Some(root_path.clone());
        self.right_sidebar_file_index_project_name = project_name.clone();

        if keep_showing {
            // Refresh: keep the current tree + Ready visible; `apply` swaps the
            // new index in when it arrives. View state is left untouched.
            self.right_sidebar_file_refreshing = true;
        } else {
            self.right_sidebar_file_refreshing = false;
            self.right_sidebar_file_index_status = RightSidebarFileIndexStatus::Indexing;
            self.right_sidebar_file_index = None;
            if let Some(cancel) = self.right_sidebar_file_search_cancel.take() {
                cancel.store(true, AtomicOrdering::Relaxed);
            }
            self.right_sidebar_file_search_generation =
                self.right_sidebar_file_search_generation.wrapping_add(1);
            self.right_sidebar_file_search_query.clear();
            self.right_sidebar_file_search_rows.clear();
            self.right_sidebar_file_searching = false;
            // tree scroll is owned by restore_right_sidebar_file_view_state.
        }

        let index_root_path = root_path.clone();
        let index_project_name = project_name.clone();
        let worker_cancel = index_cancel.clone();
        promise::spawn::spawn(async move {
            let result = promise::spawn::spawn_into_new_thread(move || {
                Ok(if fresh {
                    build_fresh_shared_file_index(
                        &index_root_path,
                        &index_project_name,
                        &worker_cancel,
                    )
                } else {
                    build_or_reuse_shared_file_index(
                        &index_root_path,
                        &index_project_name,
                        &worker_cancel,
                    )
                })
            })
            .await
            .unwrap_or_else(|err| Err(format!("Unable to index files: {err}")));
            window.notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                term_window.apply_right_sidebar_file_index_result(
                    generation,
                    root_path,
                    project_name,
                    result,
                );
            })));
        })
        .detach();
    }

    /// Force a real (registry-bypassing) re-scan of the current root while
    /// keeping the tree + view state on screen. Used by the periodic timer,
    /// window/panel focus, and the manual Refresh button.
    pub(crate) fn force_right_sidebar_file_rescan(&mut self) {
        if matches!(self.active_remote_project_for_files(), Ok(Some(_))) {
            return;
        }
        if !self.right_sidebar_file_view_active() || self.right_sidebar_file_refreshing {
            return;
        }
        if self
            .sync_right_sidebar_file_root_for_current_workspace()
            .is_err()
        {
            return;
        }
        if !matches!(
            self.right_sidebar_file_index_status,
            RightSidebarFileIndexStatus::Ready
        ) {
            return;
        }
        let Some((root, project)) = self.right_sidebar_file_view_state_key() else {
            return;
        };
        self.spawn_right_sidebar_file_index_build(root, project, true, true);
    }

    /// Refresh now (if a tree is already loaded) and (re)start the 90s periodic
    /// re-scan cycle. Called on window focus and when entering the file view; a
    /// fresh open builds via the normal index path, so we only force when Ready.
    pub(crate) fn kick_right_sidebar_file_rescan_cycle(&mut self) {
        if matches!(self.active_remote_project_for_files(), Ok(Some(_))) {
            return;
        }
        if !(self.right_sidebar_file_view_active() && self.focused.is_some()) {
            return;
        }
        if self
            .sync_right_sidebar_file_root_for_current_workspace()
            .is_err()
        {
            return;
        }
        if matches!(
            self.right_sidebar_file_index_status,
            RightSidebarFileIndexStatus::Ready
        ) {
            self.force_right_sidebar_file_rescan();
        }
        self.schedule_right_sidebar_file_rescan();
    }

    /// Schedule the next periodic re-scan tick (token-guarded so close / blur /
    /// root change makes a pending tick a no-op and the cycle stops).
    pub(crate) fn schedule_right_sidebar_file_rescan(&mut self) {
        if matches!(self.active_remote_project_for_files(), Ok(Some(_))) {
            return;
        }
        let Some(window) = self.window.as_ref().cloned() else {
            return;
        };
        self.right_sidebar_file_rescan_token = self.right_sidebar_file_rescan_token.wrapping_add(1);
        let token = self.right_sidebar_file_rescan_token;
        let target = Instant::now() + Duration::from_secs(FILE_INDEX_RESCAN_SECS);
        promise::spawn::spawn(async move {
            smol::Timer::at(target).await;
            window.notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                term_window.run_right_sidebar_file_periodic_rescan(token);
            })));
        })
        .detach();
    }

    fn run_right_sidebar_file_periodic_rescan(&mut self, token: u64) {
        // Superseded, panel hidden, or window blurred → let the cycle lapse.
        if token != self.right_sidebar_file_rescan_token
            || !self.right_sidebar_file_view_active()
            || self.focused.is_none()
        {
            return;
        }
        if matches!(self.active_remote_project_for_files(), Ok(Some(_))) {
            return;
        }
        self.force_right_sidebar_file_rescan();
        self.schedule_right_sidebar_file_rescan();
    }

    fn apply_right_sidebar_file_index_result(
        &mut self,
        generation: u64,
        root_path: PathBuf,
        project_name: String,
        result: Result<Arc<RightSidebarFileIndex>, String>,
    ) {
        if generation != self.right_sidebar_file_index_generation
            || self.right_sidebar_file_index_root.as_ref() != Some(&root_path)
            || self.right_sidebar_file_index_project_name != project_name
        {
            return;
        }
        self.right_sidebar_file_index_cancel = None;
        let was_refreshing = self.right_sidebar_file_refreshing;
        self.right_sidebar_file_refreshing = false;

        match result {
            Ok(index) => {
                self.right_sidebar_file_index_status = RightSidebarFileIndexStatus::Ready;
                self.right_sidebar_file_index = Some(index);
                if was_refreshing {
                    self.right_sidebar_file_browse_rows.clear();
                    self.right_sidebar_file_browse_cache_key = None;
                    if !self.right_sidebar_file_applied_filter.trim().is_empty() {
                        self.clear_right_sidebar_file_search();
                    }
                }
            }
            Err(err) => {
                self.right_sidebar_file_index_status = RightSidebarFileIndexStatus::Failed(err);
                self.right_sidebar_file_index = None;
            }
        }
        self.invalidate_window();
    }

    fn start_right_sidebar_file_search_if_needed(&mut self, query: &str) {
        let query = query.trim().to_string();
        if query.is_empty() {
            if !self.right_sidebar_file_search_query.is_empty()
                || !self.right_sidebar_file_search_rows.is_empty()
                || self.right_sidebar_file_searching
            {
                self.clear_right_sidebar_file_search();
            }
            return;
        }

        if self.right_sidebar_file_search_query == query {
            return;
        }

        let Some(index) = self.right_sidebar_file_index.clone() else {
            return;
        };
        let Some(window) = self.window.as_ref().cloned() else {
            self.right_sidebar_file_searching = false;
            return;
        };

        if let Some(cancel) = self.right_sidebar_file_search_cancel.take() {
            cancel.store(true, AtomicOrdering::Relaxed);
        }
        let cancel = Arc::new(AtomicBool::new(false));
        self.right_sidebar_file_search_cancel = Some(cancel.clone());
        self.right_sidebar_file_search_generation =
            self.right_sidebar_file_search_generation.wrapping_add(1);
        let generation = self.right_sidebar_file_search_generation;

        self.right_sidebar_file_search_query = query.clone();
        self.right_sidebar_file_search_rows.clear();
        self.right_sidebar_file_searching = true;
        self.right_sidebar_file_tree_scroll_offset = 0.0;

        let worker_index = index.clone();
        let worker_query = query.clone();
        let worker_cancel = cancel.clone();
        let completion_cancel = cancel.clone();
        let notify_query = query.clone();
        promise::spawn::spawn(async move {
            let rows = promise::spawn::spawn_into_new_thread(move || {
                Ok(search_right_sidebar_file_index(
                    &worker_index,
                    &worker_query,
                    &worker_cancel,
                ))
            })
            .await
            .unwrap_or_else(|err| {
                log::warn!("Unable to search files: {err:#}");
                Vec::new()
            });
            if !completion_cancel.load(AtomicOrdering::Relaxed) {
                window.notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                    term_window.apply_right_sidebar_file_search_result(
                        generation,
                        notify_query,
                        rows,
                    );
                })));
            }
        })
        .detach();
    }

    fn apply_right_sidebar_file_search_result(
        &mut self,
        generation: u64,
        query: String,
        rows: Vec<RightSidebarFileTreeRow>,
    ) {
        if generation != self.right_sidebar_file_search_generation
            || self.right_sidebar_file_search_query != query
        {
            return;
        }

        self.right_sidebar_file_search_rows = rows;
        self.right_sidebar_file_searching = false;
        self.right_sidebar_file_search_cancel = None;
        self.invalidate_window();
    }

    pub(crate) fn paste_snippet_to_active_pane(&mut self, id: &str, run: bool) {
        let Some(snippet) = crate::snippets::get_snippet(id) else {
            return;
        };
        let Some(pane) = self.get_active_pane_or_overlay() else {
            return;
        };
        if run {
            let Some(buffer) = snippet_run_buffer(&snippet.body) else {
                return;
            };
            if let Err(err) = pane.writer().write_all(&buffer) {
                log::error!("failed to run snippet {id}: {err:#}");
            }
        } else if let Err(err) = pane.send_paste(&snippet.body) {
            log::error!("failed to paste snippet {id}: {err:#}");
        }
    }

    pub(crate) fn copy_right_sidebar_focused_input(&self, destination: ClipboardCopyDestination) {
        if self.right_sidebar_mode == RightSidebarMode::Tasks {
            let Some(session) = self.right_sidebar_note.session.as_ref() else {
                return;
            };
            let session = session.lock();
            if let Some(text) = session.selected_text(&self.right_sidebar_note.view) {
                if !text.is_empty() {
                    self.copy_to_clipboard(destination, text.to_string());
                }
            }
            return;
        }
        let Some(input) = self.right_sidebar_focused_input() else {
            return;
        };
        let text = input
            .caret_selected_text()
            .unwrap_or_else(|| input.text().to_string());
        if !text.is_empty() {
            self.copy_to_clipboard(destination, text);
        }
    }

    pub(crate) fn perform_right_sidebar_note_command(&mut self, command: NoteEditorCommand) {
        match command {
            NoteEditorCommand::Undo => {
                if let Some(session) = self.right_sidebar_note.session.clone() {
                    if session.lock().undo(&mut self.right_sidebar_note.view) {
                        self.note_did_edit();
                    }
                }
            }
            NoteEditorCommand::Redo => {
                if let Some(session) = self.right_sidebar_note.session.clone() {
                    if session.lock().redo(&mut self.right_sidebar_note.view) {
                        self.note_did_edit();
                    }
                }
            }
            NoteEditorCommand::Cut => self.cut_right_sidebar_focused_input(),
            NoteEditorCommand::Copy => {
                self.copy_right_sidebar_focused_input(ClipboardCopyDestination::Clipboard)
            }
            NoteEditorCommand::Paste => {
                self.paste_into_right_sidebar_from_clipboard(ClipboardPasteSource::Clipboard)
            }
            NoteEditorCommand::Delete => {
                if let Some(session) = self.right_sidebar_note.session.clone() {
                    if session
                        .lock()
                        .delete_selection(&mut self.right_sidebar_note.view)
                    {
                        self.note_did_edit();
                    }
                }
            }
            NoteEditorCommand::SelectAll => {
                if let Some(session) = self.right_sidebar_note.session.clone() {
                    session.lock().select_all(&mut self.right_sidebar_note.view);
                    self.right_sidebar_note.refresh_projection();
                }
            }
            NoteEditorCommand::ReplaceSpelling {
                revision,
                range,
                replacement,
            } => {
                if let Some(session) = self.right_sidebar_note.session.clone() {
                    if session.lock().replace_range_at_revision(
                        &mut self.right_sidebar_note.view,
                        revision,
                        range,
                        &replacement,
                    ) {
                        self.note_did_edit();
                    }
                }
            }
            NoteEditorCommand::IgnoreSpelling { word } => {
                self.ignore_right_sidebar_note_spelling(&word);
            }
            NoteEditorCommand::LearnSpelling { word } => {
                self.learn_right_sidebar_note_spelling(&word);
            }
            NoteEditorCommand::LookUp { text, anchor } => {
                if let Some(window) = self.window.as_ref() {
                    window.show_text_definition(&text, anchor);
                }
            }
            NoteEditorCommand::ToggleSourceMode => self.toggle_right_sidebar_note_mode(),
            NoteEditorCommand::Save => self.save_right_sidebar_note_now(),
            NoteEditorCommand::ChooseVault { managed } => {
                self.choose_right_sidebar_note_vault(managed)
            }
            NoteEditorCommand::NewNote => self.create_right_sidebar_note(),
            NoteEditorCommand::OpenNote { relative_path } => {
                self.activate_right_sidebar_note_tree_path(&relative_path)
            }
            NoteEditorCommand::RevealVault => {
                if let Some(vault) = workspace_threads::space_note_vault(&self.active_space_id) {
                    wezterm_open_url::reveal_path(&vault.root);
                }
            }
            NoteEditorCommand::ToggleVaultTree => self.toggle_right_sidebar_note_vault_tree(),
        }
    }

    pub(crate) fn toggle_right_sidebar_note_vault_tree(&mut self) {
        if self.right_sidebar_note_wide_layout {
            self.right_sidebar_note_vault_tree_collapsed =
                !self.right_sidebar_note_vault_tree_collapsed;
        } else {
            self.right_sidebar_note_view = match self.right_sidebar_note_view {
                RightSidebarNoteView::Tree => RightSidebarNoteView::Editor,
                RightSidebarNoteView::Editor => RightSidebarNoteView::Tree,
            };
        }
        self.right_sidebar_note_tree_scroll_offset = 0.0;
    }

    fn choose_right_sidebar_note_vault(&mut self, managed: bool) {
        let Some(window) = self.window.clone() else {
            return;
        };
        let space_id = self.active_space_id.clone();
        let notify_window = window.clone();
        let options = if managed {
            FolderPickerOptions {
                title: "Create Vault".to_string(),
                prompt: "Create".to_string(),
            }
        } else {
            FolderPickerOptions {
                title: "Choose Vault".to_string(),
                prompt: "Choose".to_string(),
            }
        };
        window.pick_folder_async_with_options(
            options,
            Box::new(move |path| {
                let Some(path) = path else {
                    return;
                };
                let result = workspace_threads::set_space_note_vault(&space_id, path, managed);
                notify_window.notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                    let previous_sidebar_width = term_window.right_sidebar_width();
                    term_window.refresh_active_space_note_vault_flag();
                    // Binding a vault can activate an expanded Note pane in a
                    // Space that previously had none.
                    term_window.reflow_right_sidebar_if_width_changed(previous_sidebar_width);
                    term_window.right_sidebar_note_open_generation = term_window
                        .right_sidebar_note_open_generation
                        .wrapping_add(1);
                    term_window.right_sidebar_note_opening = None;
                    term_window.right_sidebar_note_open_failure = None;
                    match result {
                        Ok(_) => term_window.right_sidebar_note.clear_document(None),
                        Err(err) => term_window
                            .right_sidebar_note
                            .clear_document(Some(format!("{err:#}"))),
                    }
                    term_window.invalidate_window();
                })));
            }),
        );
    }

    fn create_right_sidebar_note(&mut self) {
        let Some(vault) = workspace_threads::space_note_vault(&self.active_space_id) else {
            return;
        };
        let Some(project_id) =
            workspace_threads::active_project_id_for_space(&self.active_space_id)
        else {
            return;
        };
        let existing = vault_markdown_paths(&vault.root).unwrap_or_default();
        let existing = existing
            .into_iter()
            .map(|path| path.to_ascii_lowercase())
            .collect::<HashSet<_>>();
        let mut sequence = 1usize;
        let relative_path = loop {
            let candidate = if sequence == 1 {
                "Untitled.md".to_string()
            } else {
                format!("Untitled {sequence}.md")
            };
            if !existing.contains(&candidate.to_ascii_lowercase()) {
                break candidate;
            }
            sequence += 1;
        };
        self.right_sidebar_note_open_failure = None;
        self.request_right_sidebar_note_open(vault.root, relative_path, true, project_id, true);
    }

    pub(crate) fn activate_right_sidebar_note_tree_path(&mut self, relative_path: &str) {
        let Some(vault) = workspace_threads::space_note_vault(&self.active_space_id) else {
            return;
        };
        let candidate = vault.root.join(relative_path);
        if candidate.is_dir() {
            if !self.right_sidebar_note_tree_expanded.remove(relative_path) {
                self.right_sidebar_note_tree_expanded
                    .insert(relative_path.to_string());
            }
            return;
        }
        if candidate
            .extension()
            .and_then(|extension| extension.to_str())
            .is_none_or(|extension| !extension.eq_ignore_ascii_case("md"))
        {
            let safe_candidate = candidate.canonicalize().ok().filter(|candidate| {
                vault
                    .root
                    .canonicalize()
                    .is_ok_and(|root| candidate.starts_with(root))
            });
            let Some(candidate) = safe_candidate else {
                self.right_sidebar_note.load_error =
                    Some(format!("Vault file is unavailable: {relative_path}"));
                return;
            };
            match url::Url::from_file_path(&candidate) {
                Ok(url) => wezterm_open_url::open_url(url.as_str()),
                Err(()) => {
                    self.right_sidebar_note.load_error =
                        Some(format!("Unable to open {}", candidate.display()));
                }
            }
            return;
        }
        let Some(project_id) =
            workspace_threads::active_project_id_for_space(&self.active_space_id)
        else {
            return;
        };
        self.right_sidebar_note_open_failure = None;
        self.request_right_sidebar_note_open(
            vault.root,
            relative_path.to_string(),
            false,
            project_id,
            false,
        );
    }

    pub(crate) fn activate_or_create_right_sidebar_wiki_link(
        &mut self,
        target: &str,
        resolved_path: Option<&str>,
    ) {
        if let Some(path) = resolved_path {
            self.activate_right_sidebar_note_tree_path(path);
            return;
        }
        let Some(document) = self.right_sidebar_note.document.clone() else {
            return;
        };
        let target = target.split(['#', '^']).next().unwrap_or(target).trim();
        if target.is_empty() {
            return;
        }
        let mut relative = Path::new(&document.relative_path)
            .parent()
            .unwrap_or_else(|| Path::new(""))
            .join(target);
        if relative.extension().is_none() {
            relative.set_extension("md");
        }
        let relative = relative.to_string_lossy().replace('\\', "/");
        let relative = match workspace_threads::normalize_vault_markdown_path(&relative) {
            Ok(relative) => relative,
            Err(err) => {
                self.right_sidebar_note.load_error = Some(format!("{err:#}"));
                return;
            }
        };
        let Some(project_id) =
            workspace_threads::active_project_id_for_space(&self.active_space_id)
        else {
            return;
        };
        self.right_sidebar_note_vault_last_scan = None;
        self.right_sidebar_note_open_failure = None;
        self.request_right_sidebar_note_open(
            document.vault_root,
            relative,
            true,
            project_id,
            false,
        );
    }

    pub(crate) fn show_right_sidebar_note_wiki_link_choices(
        &mut self,
        context: &dyn WindowOps,
        anchor: window::Point,
        candidates: Vec<String>,
    ) {
        self.begin_context_menu_application_actions();
        let items = candidates
            .into_iter()
            .map(|relative_path| {
                self.context_menu_application_item_with_icon(
                    relative_path.clone(),
                    ContextMenuIcon::File,
                    crate::termwindow::ContextMenuApplicationAction::Note(
                        NoteEditorCommand::OpenNote { relative_path },
                    ),
                    true,
                )
            })
            .collect();
        self.show_term_context_menu(context, anchor, items);
    }

    pub(crate) fn show_right_sidebar_note_menu(
        &mut self,
        context: &dyn WindowOps,
        anchor: window::Point,
    ) {
        self.begin_context_menu_application_actions();
        let source_mode = self.right_sidebar_note.view.mode == EditorMode::Source;
        let save_enabled = !matches!(
            self.right_sidebar_note.display_save_state(),
            SaveState::Saved | SaveState::Saving(_)
        );
        let mut source_item = self.context_menu_application_item_with_icon(
            "Source Mode",
            ContextMenuIcon::Code,
            crate::termwindow::ContextMenuApplicationAction::Note(
                NoteEditorCommand::ToggleSourceMode,
            ),
            true,
        );
        source_item = source_item.checked(source_mode);
        let vault_tree_label = if self.right_sidebar_note_wide_layout {
            if self.right_sidebar_note_vault_tree_collapsed {
                "Show Vault Sidebar"
            } else {
                "Hide Vault Sidebar"
            }
        } else if self.right_sidebar_note_view == RightSidebarNoteView::Tree {
            "Hide Vault"
        } else {
            "Show Vault"
        };
        let items = vec![
            self.context_menu_application_item_with_icon(
                "New Note",
                ContextMenuIcon::Note,
                crate::termwindow::ContextMenuApplicationAction::Note(NoteEditorCommand::NewNote),
                true,
            ),
            self.context_menu_application_item_with_icon(
                "Save",
                ContextMenuIcon::Save,
                crate::termwindow::ContextMenuApplicationAction::Note(NoteEditorCommand::Save),
                save_enabled,
            ),
            source_item,
            self.context_menu_application_item_with_icon(
                vault_tree_label,
                ContextMenuIcon::Sidebar,
                crate::termwindow::ContextMenuApplicationAction::Note(
                    NoteEditorCommand::ToggleVaultTree,
                ),
                true,
            ),
            ContextMenuItem::Separator,
            self.context_menu_application_item_with_icon(
                "Choose Another Vault…",
                ContextMenuIcon::Vault,
                crate::termwindow::ContextMenuApplicationAction::Note(
                    NoteEditorCommand::ChooseVault { managed: false },
                ),
                true,
            ),
            self.context_menu_application_item_with_icon(
                super::context_menu::reveal_in_folder_label(),
                ContextMenuIcon::Folder,
                crate::termwindow::ContextMenuApplicationAction::Note(
                    NoteEditorCommand::RevealVault,
                ),
                workspace_threads::space_note_vault(&self.active_space_id).is_some(),
            ),
        ];
        self.show_term_context_menu(context, anchor, items);
    }

    fn ignore_right_sidebar_note_spelling(&mut self, word: &str) {
        let Some(session) = self.right_sidebar_note.session.as_ref() else {
            return;
        };
        let document_id = session.lock().document_id.clone();
        if let Some(window) = self.window.as_ref() {
            window.ignore_spelling_word(&document_id, word);
        }
        self.right_sidebar_note.spelling_issues = Arc::new(
            self.right_sidebar_note
                .spelling_issues
                .iter()
                .filter(|issue| !issue.word.eq_ignore_ascii_case(word))
                .cloned()
                .collect(),
        );
        self.right_sidebar_note.spelling_revision = None;
        self.right_sidebar_note.spelling_context = None;
        self.schedule_right_sidebar_note_spellcheck(Duration::from_millis(50));
        self.invalidate_window();
    }

    fn learn_right_sidebar_note_spelling(&mut self, word: &str) {
        if let Some(window) = self.window.as_ref() {
            window.learn_spelling_word(word);
        }
        self.right_sidebar_note.spelling_issues = Arc::new(
            self.right_sidebar_note
                .spelling_issues
                .iter()
                .filter(|issue| !issue.word.eq_ignore_ascii_case(word))
                .cloned()
                .collect(),
        );
        self.right_sidebar_note.spelling_revision = None;
        self.right_sidebar_note.spelling_context = None;
        self.schedule_right_sidebar_note_spellcheck(Duration::from_millis(50));
        self.invalidate_window();
    }

    pub(crate) fn cut_right_sidebar_focused_input(&mut self) {
        if self.right_sidebar_mode == RightSidebarMode::Tasks {
            let Some(session) = self.right_sidebar_note.session.clone() else {
                return;
            };
            let selected = session
                .lock()
                .selected_text(&self.right_sidebar_note.view)
                .map(str::to_string);
            if let Some(text) = selected {
                if session.lock().backspace(&mut self.right_sidebar_note.view) {
                    self.copy_to_clipboard(ClipboardCopyDestination::Clipboard, text);
                    self.note_did_edit();
                }
            }
            return;
        }
        let Some(input) = self.right_sidebar_focused_input_mut() else {
            return;
        };
        let text = if let Some(text) = input.caret_take_selected_text() {
            text
        } else {
            let text = input.text().to_string();
            input.clear();
            text
        };
        if !text.is_empty() {
            self.copy_to_clipboard(ClipboardCopyDestination::Clipboard, text);
        }
        self.after_right_sidebar_text_edit();
    }

    pub(crate) fn paste_into_right_sidebar_from_clipboard(&mut self, source: ClipboardPasteSource) {
        let Some(window) = self.window.as_ref().cloned() else {
            return;
        };
        let clipboard = match source {
            ClipboardPasteSource::Clipboard => Clipboard::Clipboard,
            ClipboardPasteSource::PrimarySelection => Clipboard::PrimarySelection,
        };
        let future = window.get_clipboard(clipboard);
        promise::spawn::spawn(async move {
            if let Ok(text) = future.await {
                window.notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                    if term_window.push_right_sidebar_text(&text) {
                        term_window.invalidate_window();
                    }
                })));
            }
        })
        .detach();
    }

    pub(crate) fn clear_right_sidebar_focused_input_selection(&mut self) {
        if self.right_sidebar_mode == RightSidebarMode::Tasks {
            let focus = self.right_sidebar_note.view.selection.focus;
            self.right_sidebar_note.view.selection = SourceSelection {
                anchor: focus,
                focus,
            };
            self.right_sidebar_note.refresh_projection();
            return;
        }
        if let Some(input) = self.right_sidebar_focused_input_mut() {
            input.clear_selection();
        }
    }

    pub(crate) fn handle_right_sidebar_key(
        &mut self,
        key: TermKeyCode,
        mods: TermModifiers,
    ) -> bool {
        if !self.right_sidebar_has_text_focus() {
            return false;
        }

        // Resolved once, platform-normalized: ⌘ on macOS, Ctrl on
        // Windows/Linux. Testing raw modifiers here is what left these inputs
        // macOS-only while the Note editor next door worked everywhere.
        let edit = EditModifiers::from(mods);
        let shift = edit.shift;
        let multiline = self.right_sidebar_focused_is_multiline();

        if self.right_sidebar_mode == RightSidebarMode::Tasks {
            return self.handle_right_sidebar_note_key(key, mods);
        }

        // Clipboard and select-all. Deliberately falls through rather than
        // returning on an unhandled key: off macOS the same Ctrl chord is also
        // the word modifier, which is handled just below.
        if edit.command {
            match key {
                TermKeyCode::Char('a') | TermKeyCode::Char('A') => {
                    if let Some(input) = self.right_sidebar_focused_input_mut() {
                        input.caret_select_all();
                    }
                    return true;
                }
                TermKeyCode::Char('c') | TermKeyCode::Char('C') => {
                    self.copy_right_sidebar_focused_input(ClipboardCopyDestination::Clipboard);
                    return true;
                }
                TermKeyCode::Char('x') | TermKeyCode::Char('X') => {
                    self.cut_right_sidebar_focused_input();
                    return true;
                }
                TermKeyCode::Char('v') | TermKeyCode::Char('V') => {
                    self.paste_into_right_sidebar_from_clipboard(ClipboardPasteSource::Clipboard);
                    return true;
                }
                // ⌘←/→/⌫ are line-start/end/delete-to-start on macOS only;
                // elsewhere Home/End cover it and Ctrl means word-wise.
                TermKeyCode::LeftArrow if !multiline && cfg!(target_os = "macos") => {
                    if let Some(input) = self.right_sidebar_focused_input_mut() {
                        input.caret_move_home(shift);
                    }
                    return true;
                }
                TermKeyCode::RightArrow if !multiline && cfg!(target_os = "macos") => {
                    if let Some(input) = self.right_sidebar_focused_input_mut() {
                        input.caret_move_end(shift);
                    }
                    return true;
                }
                TermKeyCode::Backspace if !multiline && cfg!(target_os = "macos") => {
                    if let Some(input) = self.right_sidebar_focused_input_mut() {
                        input.caret_delete_to_start();
                    }
                    self.after_right_sidebar_text_edit();
                    return true;
                }
                _ => {}
            }
        }

        // Word navigation / deletion (⌥ on macOS, Ctrl elsewhere).
        if edit.word {
            match key {
                TermKeyCode::LeftArrow if !multiline => {
                    if let Some(input) = self.right_sidebar_focused_input_mut() {
                        input.caret_word_left(shift);
                    }
                    return true;
                }
                TermKeyCode::RightArrow if !multiline => {
                    if let Some(input) = self.right_sidebar_focused_input_mut() {
                        input.caret_word_right(shift);
                    }
                    return true;
                }
                TermKeyCode::Backspace if !multiline => {
                    if let Some(input) = self.right_sidebar_focused_input_mut() {
                        input.caret_delete_word_back();
                    }
                    self.after_right_sidebar_text_edit();
                    return true;
                }
                _ => {}
            }
        }

        // Any chord we did not claim belongs to the app, not to this input.
        if !edit.plain() {
            return false;
        }

        // Plain caret navigation, shared by every single-line input.
        match key {
            TermKeyCode::LeftArrow if !multiline => {
                if let Some(input) = self.right_sidebar_focused_input_mut() {
                    input.caret_move_left(shift);
                }
                return true;
            }
            TermKeyCode::RightArrow if !multiline => {
                if let Some(input) = self.right_sidebar_focused_input_mut() {
                    input.caret_move_right(shift);
                }
                return true;
            }
            TermKeyCode::Home if !multiline => {
                if let Some(input) = self.right_sidebar_focused_input_mut() {
                    input.caret_move_home(shift);
                }
                return true;
            }
            TermKeyCode::End if !multiline => {
                if let Some(input) = self.right_sidebar_focused_input_mut() {
                    input.caret_move_end(shift);
                }
                return true;
            }
            TermKeyCode::LeftArrow
            | TermKeyCode::RightArrow
            | TermKeyCode::Home
            | TermKeyCode::End => {
                // Multiline body: swallow so the arrow does not leak to the pane.
                return true;
            }
            TermKeyCode::Delete if !multiline => {
                if let Some(input) = self.right_sidebar_focused_input_mut() {
                    input.caret_delete_forward();
                }
                self.after_right_sidebar_text_edit();
                return true;
            }
            _ => {}
        }

        if self.right_sidebar_mode == RightSidebarMode::Chat {
            return match key {
                TermKeyCode::Escape => {
                    self.right_sidebar_file_focus = None;
                    true
                }
                TermKeyCode::Enter | TermKeyCode::Tab => true,
                TermKeyCode::Backspace => {
                    if let Some(input) = self.right_sidebar_focused_input_mut() {
                        input.caret_backspace();
                    }
                    self.after_right_sidebar_text_edit();
                    true
                }
                TermKeyCode::Char(ch) => {
                    if !ch.is_control() {
                        self.push_right_sidebar_text(&ch.to_string());
                        return true;
                    }
                    false
                }
                _ => false,
            };
        }

        match key {
            TermKeyCode::Escape => {
                self.clear_right_sidebar_text_focus();
                true
            }
            TermKeyCode::Tab => {
                self.step_right_sidebar_snippet_field(if shift { -1 } else { 1 });
                true
            }
            TermKeyCode::Enter => {
                match self.right_sidebar_snippet_focus {
                    Some(RightSidebarSnippetField::Title) => {
                        self.right_sidebar_snippet_focus = Some(RightSidebarSnippetField::Body);
                    }
                    Some(RightSidebarSnippetField::Body) => {
                        self.push_right_sidebar_text("\n");
                    }
                    Some(RightSidebarSnippetField::Search) | None => {}
                }
                true
            }
            TermKeyCode::Backspace => {
                if let Some(input) = self.right_sidebar_focused_input_mut() {
                    input.caret_backspace();
                }
                self.after_right_sidebar_text_edit();
                true
            }
            TermKeyCode::Char(ch) => {
                if !ch.is_control() {
                    self.push_right_sidebar_text(&ch.to_string());
                    return true;
                }
                false
            }
            _ => false,
        }
    }

    fn right_sidebar_focused_is_multiline(&self) -> bool {
        self.right_sidebar_mode == RightSidebarMode::Snippets
            && self.right_sidebar_snippet_focus == Some(RightSidebarSnippetField::Body)
    }

    /// Side effects that must run after the focused input's text changes:
    /// re-filter the file tree, or reset the snippet list scroll.
    fn after_right_sidebar_text_edit(&mut self) {
        match self.right_sidebar_mode {
            RightSidebarMode::Chat => {
                if self.right_sidebar_file_focus == Some(RightSidebarFileField::Filter) {
                    self.mark_right_sidebar_file_filter_changed();
                }
            }
            RightSidebarMode::Snippets => {
                if self.right_sidebar_snippet_focus == Some(RightSidebarSnippetField::Search) {
                    self.right_sidebar_snippet_scroll_offset = 0.0;
                }
            }
            RightSidebarMode::Tasks => {}
        }
    }

    pub(crate) fn push_right_sidebar_text(&mut self, text: &str) -> bool {
        if self.right_sidebar_mode == RightSidebarMode::Tasks {
            self.right_sidebar_note.begin_live_editing();
            let Some(session) = self.right_sidebar_note.session.clone() else {
                return false;
            };
            let changed = session
                .lock()
                .insert_text(&mut self.right_sidebar_note.view, text);
            if changed {
                self.note_did_edit();
            }
            return changed;
        }
        let multiline = self.right_sidebar_focused_is_multiline();
        let Some(input) = self.right_sidebar_focused_input_mut() else {
            return false;
        };
        input.caret_insert(text, multiline);
        self.after_right_sidebar_text_edit();
        true
    }

    fn handle_right_sidebar_note_key(&mut self, key: TermKeyCode, mods: TermModifiers) -> bool {
        // begin_live_editing() is deliberately deferred until a key is known
        // to be handled: an unrelated key must not flip the display source to
        // Live and invalidate the projection caches.
        // This handler is where the correct cross-platform rule was first
        // written; it now lives in `EditModifiers` so every text surface shares
        // it. `command` stays slightly looser than the shared flag here because
        // the editor also accepts ⌘ chords that arrive alongside other
        // modifiers.
        let edit = EditModifiers::from(mods);
        let shift = edit.shift;
        let super_ = mods.contains(TermModifiers::SUPER);
        let ctrl = mods.contains(TermModifiers::CTRL);
        let alt = mods.contains(TermModifiers::ALT);
        let command = super_ || (ctrl && !cfg!(target_os = "macos"));
        let word_modifier = edit.word;

        if word_modifier
            && matches!(
                key,
                TermKeyCode::LeftArrow | TermKeyCode::RightArrow | TermKeyCode::Backspace
            )
        {
            let Some(session) = self.right_sidebar_note.session.clone() else {
                return false;
            };
            self.right_sidebar_note.begin_live_editing();
            match key {
                TermKeyCode::LeftArrow => {
                    session
                        .lock()
                        .move_word_left(&mut self.right_sidebar_note.view, shift);
                    self.right_sidebar_note.reveal_caret = true;
                    self.right_sidebar_note.refresh_projection();
                    return true;
                }
                TermKeyCode::RightArrow => {
                    session
                        .lock()
                        .move_word_right(&mut self.right_sidebar_note.view, shift);
                    self.right_sidebar_note.reveal_caret = true;
                    self.right_sidebar_note.refresh_projection();
                    return true;
                }
                TermKeyCode::Backspace => {
                    if session
                        .lock()
                        .delete_word_back(&mut self.right_sidebar_note.view)
                    {
                        self.note_did_edit();
                    }
                    return true;
                }
                _ => {}
            }
        }

        if command && !alt {
            let command_handled = match key {
                TermKeyCode::LeftArrow | TermKeyCode::RightArrow => super_,
                TermKeyCode::Char(
                    'a' | 'A' | 'c' | 'C' | 'x' | 'X' | 'v' | 'V' | 'z' | 'Z' | 's' | 'S' | 'e'
                    | 'E' | 'b' | 'B' | 'i' | 'I',
                ) => true,
                _ => false,
            };
            if command_handled {
                self.right_sidebar_note.begin_live_editing();
            }
            match key {
                TermKeyCode::LeftArrow if super_ => {
                    if let Some(session) = self.right_sidebar_note.session.clone() {
                        session
                            .lock()
                            .move_line_start(&mut self.right_sidebar_note.view, shift);
                        self.right_sidebar_note.reveal_caret = true;
                        self.right_sidebar_note.refresh_projection();
                    }
                    return true;
                }
                TermKeyCode::RightArrow if super_ => {
                    if let Some(session) = self.right_sidebar_note.session.clone() {
                        session
                            .lock()
                            .move_line_end(&mut self.right_sidebar_note.view, shift);
                        self.right_sidebar_note.reveal_caret = true;
                        self.right_sidebar_note.refresh_projection();
                    }
                    return true;
                }
                TermKeyCode::Char('a') | TermKeyCode::Char('A') => {
                    if let Some(session) = self.right_sidebar_note.session.clone() {
                        session.lock().select_all(&mut self.right_sidebar_note.view);
                        self.right_sidebar_note.refresh_projection();
                    }
                    return true;
                }
                TermKeyCode::Char('c') | TermKeyCode::Char('C') => {
                    self.copy_right_sidebar_focused_input(ClipboardCopyDestination::Clipboard);
                    return true;
                }
                TermKeyCode::Char('x') | TermKeyCode::Char('X') => {
                    self.cut_right_sidebar_focused_input();
                    return true;
                }
                TermKeyCode::Char('v') | TermKeyCode::Char('V') => {
                    self.paste_into_right_sidebar_from_clipboard(ClipboardPasteSource::Clipboard);
                    return true;
                }
                TermKeyCode::Char('z') | TermKeyCode::Char('Z') => {
                    let Some(session) = self.right_sidebar_note.session.clone() else {
                        return true;
                    };
                    let changed = if shift {
                        session.lock().redo(&mut self.right_sidebar_note.view)
                    } else {
                        session.lock().undo(&mut self.right_sidebar_note.view)
                    };
                    if changed {
                        self.note_did_edit();
                    }
                    return true;
                }
                TermKeyCode::Char('s') | TermKeyCode::Char('S') => {
                    self.save_right_sidebar_note_now();
                    return true;
                }
                TermKeyCode::Char('e') | TermKeyCode::Char('E') => {
                    self.toggle_right_sidebar_note_mode();
                    return true;
                }
                TermKeyCode::Char('b') | TermKeyCode::Char('B') => {
                    if let Some(session) = self.right_sidebar_note.session.clone() {
                        if session.lock().surround_selection(
                            &mut self.right_sidebar_note.view,
                            "**",
                            "**",
                        ) {
                            self.note_did_edit();
                        }
                    }
                    return true;
                }
                TermKeyCode::Char('i') | TermKeyCode::Char('I') => {
                    if let Some(session) = self.right_sidebar_note.session.clone() {
                        if session.lock().surround_selection(
                            &mut self.right_sidebar_note.view,
                            "*",
                            "*",
                        ) {
                            self.note_did_edit();
                        }
                    }
                    return true;
                }
                _ => {}
            }
        }

        if command || ctrl || alt {
            return false;
        }

        let will_handle = matches!(
            key,
            TermKeyCode::Escape
                | TermKeyCode::LeftArrow
                | TermKeyCode::RightArrow
                | TermKeyCode::UpArrow
                | TermKeyCode::DownArrow
                | TermKeyCode::Home
                | TermKeyCode::End
                | TermKeyCode::Backspace
                | TermKeyCode::Delete
                | TermKeyCode::Enter
                | TermKeyCode::Tab
        ) || matches!(key, TermKeyCode::Char(ch) if !ch.is_control());
        if !will_handle {
            return false;
        }
        let Some(session) = self.right_sidebar_note.session.clone() else {
            return false;
        };
        self.right_sidebar_note.begin_live_editing();
        let mut changed = false;
        let handled = match key {
            TermKeyCode::Escape => {
                self.right_sidebar_note.view.focused = false;
                true
            }
            TermKeyCode::LeftArrow => {
                session
                    .lock()
                    .move_left(&mut self.right_sidebar_note.view, shift);
                true
            }
            TermKeyCode::RightArrow => {
                session
                    .lock()
                    .move_right(&mut self.right_sidebar_note.view, shift);
                true
            }
            TermKeyCode::UpArrow => {
                if let Some((position, preferred)) =
                    self.right_sidebar_note.visual_vertical_target(-1)
                {
                    if shift {
                        self.right_sidebar_note.view.selection.focus = position;
                    } else {
                        self.right_sidebar_note.view.selection =
                            SourceSelection::caret(position.byte);
                    }
                    self.right_sidebar_note.view.preferred_column = Some(preferred);
                } else {
                    session
                        .lock()
                        .move_vertical(&mut self.right_sidebar_note.view, -1, shift);
                }
                true
            }
            TermKeyCode::DownArrow => {
                if let Some((position, preferred)) =
                    self.right_sidebar_note.visual_vertical_target(1)
                {
                    if shift {
                        self.right_sidebar_note.view.selection.focus = position;
                    } else {
                        self.right_sidebar_note.view.selection =
                            SourceSelection::caret(position.byte);
                    }
                    self.right_sidebar_note.view.preferred_column = Some(preferred);
                } else {
                    session
                        .lock()
                        .move_vertical(&mut self.right_sidebar_note.view, 1, shift);
                }
                true
            }
            TermKeyCode::Home => {
                session
                    .lock()
                    .move_line_start(&mut self.right_sidebar_note.view, shift);
                true
            }
            TermKeyCode::End => {
                session
                    .lock()
                    .move_line_end(&mut self.right_sidebar_note.view, shift);
                true
            }
            TermKeyCode::Backspace => {
                changed = session.lock().backspace(&mut self.right_sidebar_note.view);
                true
            }
            TermKeyCode::Delete => {
                changed = session
                    .lock()
                    .delete_forward(&mut self.right_sidebar_note.view);
                true
            }
            TermKeyCode::Enter => {
                changed = session
                    .lock()
                    .insert_newline(&mut self.right_sidebar_note.view);
                true
            }
            TermKeyCode::Tab => {
                if let Some(target) =
                    self.right_sidebar_note
                        .table_cell_target(if shift { -1 } else { 1 })
                {
                    session
                        .lock()
                        .set_caret(&mut self.right_sidebar_note.view, target, false);
                    self.right_sidebar_note.reveal_caret = true;
                } else if session
                    .lock()
                    .tab_indents_lines(&self.right_sidebar_note.view)
                {
                    changed = if shift {
                        session
                            .lock()
                            .outdent_lines(&mut self.right_sidebar_note.view)
                    } else {
                        session
                            .lock()
                            .indent_lines(&mut self.right_sidebar_note.view)
                    };
                } else if !shift {
                    changed = session
                        .lock()
                        .insert_text(&mut self.right_sidebar_note.view, "    ");
                }
                true
            }
            TermKeyCode::Char(ch) if !ch.is_control() => {
                changed = session
                    .lock()
                    .insert_text(&mut self.right_sidebar_note.view, &ch.to_string());
                true
            }
            _ => false,
        };
        if changed {
            self.note_did_edit();
        } else if handled {
            self.right_sidebar_note.reveal_caret = true;
            self.right_sidebar_note.refresh_projection();
        }
        handled
    }

    pub(crate) fn toggle_right_sidebar_note_mode(&mut self) {
        self.right_sidebar_note.view.mode = match self.right_sidebar_note.view.mode {
            EditorMode::LivePreview => EditorMode::Source,
            EditorMode::Source | EditorMode::ReadOnly => EditorMode::LivePreview,
        };
        self.schedule_right_sidebar_note_parse();
        self.right_sidebar_note.refresh_projection();
        self.schedule_right_sidebar_note_spellcheck(Duration::from_millis(50));
        self.invalidate_window();
    }

    pub(crate) fn save_right_sidebar_note_now(&mut self) {
        self.right_sidebar_note.cancel_autosave_deadline();
        let Some(session) = self.right_sidebar_note.session.clone() else {
            return;
        };
        if !session.lock().is_dirty() {
            return;
        }
        let Some(window) = self.window.as_ref().cloned() else {
            return;
        };
        let origin_mux_window_id = self.mux_window_id;
        let saved_session = Arc::clone(&session);
        promise::spawn::spawn(async move {
            let result = promise::spawn::spawn_into_new_thread(move || {
                let stage = crate::input_diagnostics::StageTimer::begin("note_save");
                let result = save_document_revision(&session);
                stage.finish(result.is_ok());
                result
            })
            .await;
            window.notify(TermWindowNotif::Apply(Box::new(
                move |term_window| match result {
                    Ok(outcome) => {
                        term_window.publish_right_sidebar_note_revision(
                            origin_mux_window_id,
                            saved_session,
                            outcome.saved_revision,
                        );
                    }
                    Err(err) => {
                        log::error!("failed to save Note: {err:#}");
                        if term_window.right_sidebar_note_visible() {
                            term_window.invalidate_window();
                        }
                    }
                },
            )));
        })
        .detach();
    }

    pub(crate) fn flush_right_sidebar_note_blocking(&mut self) {
        self.right_sidebar_note.cancel_autosave_deadline();
        let Some(session) = self.right_sidebar_note.session.clone() else {
            return;
        };
        if session.lock().is_dirty() {
            if let Err(err) = save_document_revision(&session) {
                log::error!("failed to flush Note before window lifecycle change: {err:#}");
            }
        }
    }

    pub(crate) fn note_did_edit(&mut self) {
        self.right_sidebar_note.refresh_projection();
        self.schedule_right_sidebar_note_parse();
        self.right_sidebar_note.reveal_caret = true;
        self.schedule_right_sidebar_note_spellcheck(Duration::from_millis(
            NOTE_SPELLCHECK_DEBOUNCE_MS,
        ));
        if self
            .right_sidebar_note
            .note_edited(Instant::now(), Duration::from_millis(NOTE_AUTOSAVE_MS))
        {
            if let Some(window) = self.window.as_ref().cloned() {
                Self::schedule_right_sidebar_note_autosave_wakeup(
                    window,
                    Duration::from_millis(NOTE_AUTOSAVE_MS),
                );
            }
        }
    }

    fn schedule_right_sidebar_note_parse(&mut self) {
        let Some((revision, source, mode, caret, document)) =
            self.right_sidebar_note.background_parse_request()
        else {
            return;
        };
        let Some(window) = self.window.as_ref().cloned() else {
            self.right_sidebar_note.parse_in_flight_revision = None;
            return;
        };
        promise::spawn::spawn(async move {
            let result = promise::spawn::spawn_into_new_thread(move || {
                let stage = crate::input_diagnostics::StageTimer::begin("note_projection");
                let mut projection = crate::markdown_editor::MarkdownProjection::parse(&source);
                if let Some((vault_root, relative_path)) = document {
                    projection.resolve_vault_links(&vault_root, &relative_path);
                }
                let visual = build_visual_document(&source, &projection, mode, caret);
                stage.finish(true);
                Ok((projection, visual))
            })
            .await;
            window.notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                match result {
                    Ok((projection, visual)) => {
                        let applied = term_window
                            .right_sidebar_note
                            .apply_background_parse(revision, mode, caret, projection, visual);
                        if applied {
                            term_window
                                .schedule_right_sidebar_note_spellcheck(Duration::from_millis(50));
                            term_window.invalidate_window();
                        }
                    }
                    Err(err) => {
                        term_window.right_sidebar_note.parse_in_flight_revision = None;
                        log::error!("failed to parse Note in background: {err:#}");
                    }
                }
                if term_window.right_sidebar_note.background_parse_pending() {
                    term_window.schedule_right_sidebar_note_parse();
                }
            })));
        })
        .detach();
    }

    fn schedule_right_sidebar_note_wrap(
        &mut self,
        wrap_key: usize,
        wrap_width: f32,
        normal_metrics: NoteApproximateTextMetrics,
        h1_metrics: NoteApproximateTextMetrics,
        h2_metrics: NoteApproximateTextMetrics,
        h3_metrics: NoteApproximateTextMetrics,
        code_metrics: NoteApproximateTextMetrics,
    ) {
        let Some((key, visual, mut wrap_cache)) =
            self.right_sidebar_note.background_wrap_request(wrap_key)
        else {
            return;
        };
        let Some(window) = self.window.as_ref().cloned() else {
            self.right_sidebar_note.background_wrap_in_flight_key = None;
            self.right_sidebar_note.wrap_cache = wrap_cache;
            return;
        };
        promise::spawn::spawn(async move {
            let result = promise::spawn::spawn_into_new_thread(move || {
                // Worker-side wrap: slow is acceptable here. Decisions about
                // real incremental wrapping must look at note_wrap_sync only.
                let stage = crate::input_diagnostics::StageTimer::begin("note_wrap_background");
                let wrapped = wrap_visual_document_by_width_cached(
                    &visual,
                    wrap_width,
                    wrap_key,
                    &mut wrap_cache,
                    |block, _, text| {
                        let metrics = match block {
                            BlockKind::Heading(1) => h1_metrics,
                            BlockKind::Heading(2) => h2_metrics,
                            BlockKind::Heading(_) => h3_metrics,
                            BlockKind::CodeBlock => code_metrics,
                            _ => normal_metrics,
                        };
                        Ok::<f32, anyhow::Error>(approximate_note_text_width(text, metrics))
                    },
                )?;
                stage.finish(true);
                Ok((wrapped, wrap_cache))
            })
            .await;
            window.notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                match result {
                    Ok((wrapped, wrap_cache)) => {
                        term_window
                            .right_sidebar_note
                            .apply_background_wrap(key, wrapped, wrap_cache);
                    }
                    Err(err) => {
                        if term_window.right_sidebar_note.background_wrap_in_flight_key == Some(key)
                        {
                            term_window.right_sidebar_note.background_wrap_in_flight_key = None;
                        }
                        log::error!("failed to wrap Note in background: {err:#}");
                    }
                }
                term_window.invalidate_window();
            })));
        })
        .detach();
    }

    fn note_approximate_text_metrics(
        &self,
        font: &Rc<LoadedFont>,
        render_metrics: &RenderMetrics,
    ) -> anyhow::Result<NoteApproximateTextMetrics> {
        // Background wrapping cannot move LoadedFont to its worker thread. Shape
        // a tiny representative sample on the UI thread and pass only these
        // Copy metrics across. Using the terminal cell width for every grapheme
        // made proportional Latin text almost twice as wide as what we paint,
        // causing long notes to wrap much earlier than short notes.
        const LATIN_SAMPLE: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
        const WIDE_SAMPLE: &str = "系统";

        let latin = self.cached_ui_text_advance(font, render_metrics, LATIN_SAMPLE)?
            / LATIN_SAMPLE.chars().count() as f32;
        let space = self.cached_ui_text_advance(font, render_metrics, " ")?;
        let wide = self.cached_ui_text_advance(font, render_metrics, WIDE_SAMPLE)?
            / WIDE_SAMPLE.chars().count() as f32;

        Ok(NoteApproximateTextMetrics {
            latin: latin.max(1.0),
            space: space.max(1.0),
            wide: wide.max(1.0),
        })
    }

    fn schedule_right_sidebar_note_spellcheck(&mut self, delay: Duration) {
        let Some(window) = self.window.as_ref().cloned() else {
            return;
        };
        if !self.config.note_spellcheck_enabled
            || !window.text_check_capabilities().spelling
            || self.right_sidebar_note.view.mode == EditorMode::ReadOnly
        {
            return;
        }
        let Some(session) = self.right_sidebar_note.session.as_ref() else {
            return;
        };
        let revision = session.lock().revision();
        let focus = self.right_sidebar_note.view.selection.focus.byte;
        let current_context_is_valid = self.right_sidebar_note.spelling_revision == Some(revision)
            && self
                .right_sidebar_note
                .spelling_context
                .as_ref()
                .is_some_and(|context| context.start <= focus && focus <= context.end);
        if current_context_is_valid
            || self.right_sidebar_note.spellcheck_scheduled_revision == Some(revision)
            || self.right_sidebar_note.spellcheck_in_flight_revision == Some(revision)
        {
            return;
        }
        self.right_sidebar_note.spellcheck_scheduled_revision = Some(revision);
        promise::spawn::spawn(async move {
            smol::Timer::after(delay).await;
            window.notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                term_window.run_right_sidebar_note_spellcheck(revision);
            })));
        })
        .detach();
    }

    fn run_right_sidebar_note_spellcheck(&mut self, revision: u64) {
        if self.right_sidebar_note.spellcheck_scheduled_revision != Some(revision) {
            return;
        }
        self.right_sidebar_note.spellcheck_scheduled_revision = None;
        let Some(session) = self.right_sidebar_note.session.clone() else {
            return;
        };
        let (document_id, chunks, check_range) = {
            let session = session.lock();
            if session.revision() != revision {
                self.schedule_right_sidebar_note_spellcheck(Duration::from_millis(
                    NOTE_SPELLCHECK_DEBOUNCE_MS,
                ));
                return;
            }
            let source = session.source();
            let mut check_range = if source.len() > 64 * 1024 {
                self.right_sidebar_note
                    .projection
                    .active_block_range(self.right_sidebar_note.view.selection.focus.byte)
                    .unwrap_or_else(|| {
                        session.source_line_selection_range(
                            self.right_sidebar_note.view.selection.focus.byte,
                        )
                    })
            } else {
                0..source.len()
            };
            const MAX_SPELLCHECK_CONTEXT_BYTES: usize = 64 * 1024;
            if check_range.end.saturating_sub(check_range.start) > MAX_SPELLCHECK_CONTEXT_BYTES {
                let focus = self
                    .right_sidebar_note
                    .view
                    .selection
                    .focus
                    .byte
                    .min(source.len());
                let mut start = focus.saturating_sub(MAX_SPELLCHECK_CONTEXT_BYTES / 2);
                while start < focus && !source.is_char_boundary(start) {
                    start += 1;
                }
                let mut end = (start + MAX_SPELLCHECK_CONTEXT_BYTES).min(source.len());
                while end > start && !source.is_char_boundary(end) {
                    end -= 1;
                }
                check_range = start..end;
            }
            (
                session.document_id.clone(),
                build_spell_check_chunks_in_range(
                    source,
                    &self.right_sidebar_note.projection,
                    check_range.clone(),
                ),
                check_range,
            )
        };
        let Some(window) = self.window.as_ref().cloned() else {
            return;
        };
        self.right_sidebar_note.spellcheck_in_flight_revision = Some(revision);
        promise::spawn::spawn(async move {
            let mut issues = Vec::new();
            for (index, chunk) in chunks.into_iter().enumerate() {
                let request_id = revision.wrapping_mul(1_000_003).wrapping_add(index as u64);
                let response = window
                    .request_text_check(window::TextCheckRequest {
                        document_id: document_id.clone(),
                        request_id,
                        text: chunk.text.clone(),
                    })
                    .await;
                let Ok(response) = response else {
                    continue;
                };
                if response.request_id != request_id {
                    continue;
                }
                for issue in response.issues {
                    let Some(source_range) = chunk.source_range_for_issue(issue.range.clone())
                    else {
                        continue;
                    };
                    let Some(word) = chunk.text.get(issue.range).map(str::to_string) else {
                        continue;
                    };
                    issues.push(NoteSpellingIssue {
                        source: source_range,
                        word,
                        suggestions: issue.suggestions,
                    });
                }
            }
            window.notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                term_window.apply_right_sidebar_note_spellcheck(revision, check_range, issues);
            })));
        })
        .detach();
    }

    fn apply_right_sidebar_note_spellcheck(
        &mut self,
        revision: u64,
        check_range: std::ops::Range<usize>,
        mut issues: Vec<NoteSpellingIssue>,
    ) {
        if self.right_sidebar_note.spellcheck_in_flight_revision == Some(revision) {
            self.right_sidebar_note.spellcheck_in_flight_revision = None;
        }
        let Some(session) = self.right_sidebar_note.session.as_ref() else {
            return;
        };
        if session.lock().revision() != revision {
            self.schedule_right_sidebar_note_spellcheck(Duration::from_millis(
                NOTE_SPELLCHECK_DEBOUNCE_MS,
            ));
            return;
        }
        issues.sort_by_key(|issue| (issue.source.start, issue.source.end));
        issues.dedup_by(|a, b| a.source == b.source);
        self.right_sidebar_note.spelling_issues = Arc::new(issues);
        self.right_sidebar_note.spelling_revision = Some(revision);
        self.right_sidebar_note.spelling_context = Some(check_range.clone());
        let focus = self.right_sidebar_note.view.selection.focus.byte;
        if focus < check_range.start || focus > check_range.end {
            self.schedule_right_sidebar_note_spellcheck(Duration::from_millis(50));
        }
        self.invalidate_window();
    }

    fn sync_right_sidebar_note_native_text_input_snapshot(&mut self, wrap_key: usize) {
        let active = self.right_sidebar_note.view.focused
            && self.right_sidebar_note.view.mode != EditorMode::ReadOnly;
        let Some(window) = self.window.as_ref().cloned() else {
            return;
        };
        if !active {
            if self
                .right_sidebar_note
                .native_text_input_snapshot_key
                .take()
                .is_some()
            {
                window.set_native_text_input_snapshot(None);
            }
            return;
        }
        let Some(session) = self.right_sidebar_note.session.clone() else {
            return;
        };
        let selection = self.right_sidebar_note.view.selection.range();
        let revision = session.lock().revision();
        let key = (revision, selection.start, selection.end, wrap_key);
        let scroll_bits = self.right_sidebar_note.view.scroll_offset.to_bits();
        if self.right_sidebar_note.native_text_input_snapshot_key == Some(key) {
            if self.right_sidebar_note.native_text_input_snapshot_scroll == Some(scroll_bits) {
                return;
            }
            // Only the scroll moved: the text/selection are still correct and
            // just the hit rects are stale. Rebuilding costs a large context
            // copy plus per-glyph rects, so skip it while the user is still
            // scrolling; the caret-blink repaint performs the settle refresh
            // shortly after the scroll stops.
            const SNAPSHOT_SCROLL_SETTLE: Duration = Duration::from_millis(150);
            if self
                .right_sidebar_note
                .last_scroll_change
                .is_some_and(|changed| changed.elapsed() < SNAPSHOT_SCROLL_SETTLE)
            {
                // Guarantee a repaint shortly after scrolling settles so the
                // refresh below runs even if nothing else invalidates.
                self.update_next_frame_time(Some(Instant::now() + SNAPSHOT_SCROLL_SETTLE));
                return;
            }
        }

        const MAX_NATIVE_CONTEXT_BYTES: usize = 64 * 1024;
        let focus = self.right_sidebar_note.view.selection.focus.byte;
        let mut context = self
            .right_sidebar_note
            .projection
            .active_block_range(focus)
            .unwrap_or_else(|| session.lock().source_line_selection_range(focus));
        let (context_text, native_selection) = {
            let session = session.lock();
            let source = session.source();
            let focus = focus.min(source.len());
            context.start = context.start.min(selection.start).min(source.len());
            context.end = context.end.max(selection.end).min(source.len());
            let native_selection =
                if context.end.saturating_sub(context.start) > MAX_NATIVE_CONTEXT_BYTES {
                    let half = MAX_NATIVE_CONTEXT_BYTES / 2;
                    let mut start = focus.saturating_sub(half);
                    let mut end = start
                        .saturating_add(MAX_NATIVE_CONTEXT_BYTES)
                        .min(source.len());
                    start = end.saturating_sub(MAX_NATIVE_CONTEXT_BYTES);
                    while start > 0 && !source.is_char_boundary(start) {
                        start -= 1;
                    }
                    while end > start && !source.is_char_boundary(end) {
                        end -= 1;
                    }
                    context = start..end;
                    focus..focus
                } else {
                    selection.clone()
                };
            while context.start > 0 && !source.is_char_boundary(context.start) {
                context.start -= 1;
            }
            while context.end > context.start && !source.is_char_boundary(context.end) {
                context.end -= 1;
            }
            (source[context.clone()].to_string(), native_selection)
        };

        let mut hits = Vec::new();
        for line in &self.right_sidebar_note.line_layouts {
            for run in &line.runs {
                for (x, byte) in &run.boundaries {
                    if *byte < context.start || *byte > context.end {
                        continue;
                    }
                    hits.push(window::NativeTextHit {
                        rect: Rect::new(
                            Point::new(*x as isize, line.y as isize),
                            window::Size::new(1, line.height.max(1.0) as isize),
                        ),
                        byte: byte.saturating_sub(context.start),
                    });
                }
            }
        }
        self.right_sidebar_note.native_text_input_token = self
            .right_sidebar_note
            .native_text_input_token
            .wrapping_add(1)
            .max(1);
        let relative_selection = native_selection.start.saturating_sub(context.start)
            ..native_selection.end.saturating_sub(context.start);
        window.set_native_text_input_snapshot(Some(window::NativeTextInputSnapshot {
            token: self.right_sidebar_note.native_text_input_token,
            revision,
            source_base: context.start,
            text: context_text,
            selection: relative_selection,
            hits,
        }));
        self.right_sidebar_note.native_text_input_snapshot_key = Some(key);
        self.right_sidebar_note.native_text_input_snapshot_scroll = Some(scroll_bits);
    }

    fn schedule_right_sidebar_note_autosave_wakeup(window: window::Window, delay: Duration) {
        promise::spawn::spawn(async move {
            smol::Timer::after(delay).await;
            let notify_window = window.clone();
            window.notify(TermWindowNotif::Apply(Box::new(
                move |term_window| match term_window
                    .right_sidebar_note
                    .autosave_woke(Instant::now())
                {
                    AutosaveWakeAction::Idle => {}
                    AutosaveWakeAction::SaveNow => term_window.save_right_sidebar_note_now(),
                    AutosaveWakeAction::Reschedule(remaining) => {
                        Self::schedule_right_sidebar_note_autosave_wakeup(notify_window, remaining);
                    }
                },
            )));
        })
        .detach();
    }

    fn right_sidebar_note_visible(&self) -> bool {
        !self.right_sidebar_collapsed && self.right_sidebar_mode == RightSidebarMode::Tasks
    }

    fn apply_right_sidebar_note_published_revision(
        &mut self,
        session: &Arc<parking_lot::Mutex<crate::markdown_editor::MarkdownDocumentSession>>,
        saved_revision: u64,
    ) {
        if self
            .right_sidebar_note
            .session
            .as_ref()
            .is_none_or(|current| !Arc::ptr_eq(current, session))
        {
            return;
        }
        self.right_sidebar_note
            .apply_published_snapshot(saved_revision);
        // Live hosts remain Live, but still repaint the Saving/Saved badge.
        if self.right_sidebar_note_visible() {
            self.invalidate_window();
        }
    }

    fn publish_right_sidebar_note_revision(
        &mut self,
        origin_mux_window_id: mux::window::WindowId,
        session: Arc<parking_lot::Mutex<crate::markdown_editor::MarkdownDocumentSession>>,
        saved_revision: u64,
    ) {
        self.apply_right_sidebar_note_published_revision(&session, saved_revision);
        let Some(front_end) = crate::frontend::try_front_end() else {
            return;
        };
        for gui_window in front_end.gui_windows() {
            if gui_window.mux_window_id == origin_mux_window_id {
                continue;
            }
            let session = Arc::clone(&session);
            gui_window
                .window
                .notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                    term_window
                        .apply_right_sidebar_note_published_revision(&session, saved_revision);
                })));
        }
    }

    /// Map a sidebar text-input `UIItemType` to its `TextInputState`.
    fn right_sidebar_input_for_item(&self, item_type: &UIItemType) -> Option<&TextInputState> {
        match item_type {
            UIItemType::RightSidebarFileFilter => Some(&self.right_sidebar_file_filter),
            UIItemType::RightSidebarSnippetSearch => Some(&self.right_sidebar_snippet_search),
            UIItemType::RightSidebarSnippetTitle => Some(&self.right_sidebar_snippet_title),
            _ => None,
        }
    }

    /// Hit-test an x coordinate against a single-line input painted this frame,
    /// returning the closest caret char index.
    pub(crate) fn right_sidebar_input_char_index_for_x(
        &self,
        item_type: &UIItemType,
        x: isize,
    ) -> Option<usize> {
        let layout = self
            .right_sidebar_input_layouts
            .iter()
            .find(|layout| &layout.item_type == item_type)?;
        let input = self.right_sidebar_input_for_item(item_type)?;
        let font = layout.font.clone();
        let chars: Vec<char> = input.text().chars().collect();
        let first = layout.first_char.min(chars.len());
        let relative = (x as f32 - layout.text_x).clamp(0.0, layout.text_width.max(0.0));
        let mut best_idx = first;
        let mut best_dist = f32::MAX;
        for idx in first..=chars.len() {
            let prefix: String = chars[first..idx].iter().collect();
            let width = self.sidebar_text_width(&font, &prefix).unwrap_or(0.0);
            let dist = (width - relative).abs();
            if dist < best_dist {
                best_dist = dist;
                best_idx = idx;
            }
            if width > relative {
                break;
            }
        }
        Some(best_idx)
    }

    fn right_sidebar_input_for_item_mut(
        &mut self,
        item_type: &UIItemType,
    ) -> Option<&mut TextInputState> {
        match item_type {
            UIItemType::RightSidebarFileFilter => Some(&mut self.right_sidebar_file_filter),
            UIItemType::RightSidebarSnippetSearch => Some(&mut self.right_sidebar_snippet_search),
            UIItemType::RightSidebarSnippetTitle => Some(&mut self.right_sidebar_snippet_title),
            _ => None,
        }
    }

    /// Move (or, with `extend`, stretch the selection to) the caret of a
    /// single-line sidebar input to the character nearest the given x.
    pub(crate) fn position_right_sidebar_input_caret(
        &mut self,
        item_type: &UIItemType,
        x: isize,
        extend: bool,
    ) {
        let Some(idx) = self.right_sidebar_input_char_index_for_x(item_type, x) else {
            return;
        };
        if let Some(input) = self.right_sidebar_input_for_item_mut(item_type) {
            input.caret_set(idx, extend);
        }
    }

    fn step_right_sidebar_snippet_field(&mut self, delta: isize) {
        let list_fields = [RightSidebarSnippetField::Search];
        let edit_fields = [
            RightSidebarSnippetField::Title,
            RightSidebarSnippetField::Body,
        ];
        let fields: &[RightSidebarSnippetField] = match self.right_sidebar_snippet_view {
            RightSidebarSnippetView::List => &list_fields,
            RightSidebarSnippetView::EditNew | RightSidebarSnippetView::EditExisting(_) => {
                &edit_fields
            }
        };
        let current = self.right_sidebar_snippet_focus.unwrap_or(fields[0]);
        let mut index = fields
            .iter()
            .position(|field| *field == current)
            .unwrap_or(0) as isize;
        index = (index + delta).rem_euclid(fields.len() as isize);
        self.right_sidebar_snippet_focus = Some(fields[index as usize]);
    }

    fn right_sidebar_focused_input(&self) -> Option<&TextInputState> {
        match self.right_sidebar_mode {
            RightSidebarMode::Chat => match self.right_sidebar_file_focus {
                Some(RightSidebarFileField::Filter) => Some(&self.right_sidebar_file_filter),
                None => None,
            },
            RightSidebarMode::Snippets => match self.right_sidebar_snippet_focus {
                Some(RightSidebarSnippetField::Search) => Some(&self.right_sidebar_snippet_search),
                Some(RightSidebarSnippetField::Title) => Some(&self.right_sidebar_snippet_title),
                Some(RightSidebarSnippetField::Body) => Some(&self.right_sidebar_snippet_body),
                None => None,
            },
            RightSidebarMode::Tasks => None,
        }
    }

    fn right_sidebar_focused_input_mut(&mut self) -> Option<&mut TextInputState> {
        match self.right_sidebar_mode {
            RightSidebarMode::Chat => match self.right_sidebar_file_focus {
                Some(RightSidebarFileField::Filter) => Some(&mut self.right_sidebar_file_filter),
                None => None,
            },
            RightSidebarMode::Snippets => match self.right_sidebar_snippet_focus {
                Some(RightSidebarSnippetField::Search) => {
                    Some(&mut self.right_sidebar_snippet_search)
                }
                Some(RightSidebarSnippetField::Title) => {
                    Some(&mut self.right_sidebar_snippet_title)
                }
                Some(RightSidebarSnippetField::Body) => Some(&mut self.right_sidebar_snippet_body),
                None => None,
            },
            RightSidebarMode::Tasks => None,
        }
    }

    pub fn right_sidebar_rect(&self) -> Option<RightSidebarRect> {
        let border = self.get_os_border();
        let bottom_tab_bar_height = if self.config.tab_bar_at_bottom && self.show_tab_bar {
            self.tab_bar_pixel_height().unwrap_or(0.0).ceil() as usize
        } else {
            0
        };
        let width = self.right_sidebar_width().min(
            self.dimensions
                .pixel_width
                .saturating_sub((border.left + border.right).get() as usize),
        );
        let y = border.top.get() as usize;
        let height = self
            .dimensions
            .pixel_height
            .saturating_sub(y + border.bottom.get() as usize + bottom_tab_bar_height);
        if width == 0 || height == 0 {
            return None;
        }

        let right_edge = self
            .dimensions
            .pixel_width
            .saturating_sub(border.right.get() as usize);
        Some(RightSidebarRect {
            x: right_edge.saturating_sub(width),
            y,
            width,
            height,
        })
    }

    pub(crate) fn right_sidebar_file_preview_rect(&self) -> Option<RightSidebarRect> {
        let sidebar = self.right_sidebar_rect()?;
        let width = self.right_sidebar_file_preview_width()?;
        if sidebar.width <= width {
            return None;
        }
        Some(RightSidebarRect {
            x: sidebar.x,
            y: sidebar.y,
            width,
            height: sidebar.height,
        })
    }

    fn right_sidebar_tree_rect(&self, total_rect: RightSidebarRect) -> RightSidebarRect {
        let tree_width = self.right_sidebar_tree_width().min(total_rect.width);
        RightSidebarRect {
            x: total_rect
                .x
                .saturating_add(total_rect.width.saturating_sub(tree_width)),
            y: total_rect.y,
            width: tree_width,
            height: total_rect.height,
        }
    }

    pub(crate) fn set_right_sidebar_file_preview_split_x(&mut self, split_x: isize) -> bool {
        let Some(total_rect) = self.right_sidebar_rect() else {
            return false;
        };
        if self.right_sidebar_file_preview_rect().is_none() {
            return false;
        }

        let total_left = total_rect.x;
        let total_right = total_rect.x.saturating_add(total_rect.width);
        let min_preview = self.ui_px(FILE_PREVIEW_PANE_MIN_WIDTH);
        let max_preview = total_rect
            .width
            .saturating_sub(self.ui_px(RIGHT_SIDEBAR_MIN_WIDTH));
        let min_tree = self.ui_px(RIGHT_SIDEBAR_MIN_WIDTH);
        let max_tree = self.right_sidebar_max_width().min(total_rect.width);
        let min_split = total_left
            .saturating_add(min_preview)
            .max(total_right.saturating_sub(max_tree));
        let max_split = total_right
            .saturating_sub(min_tree)
            .min(total_left.saturating_add(max_preview));
        if min_split > max_split {
            return false;
        }

        let split_x = split_x.clamp(min_split as isize, max_split as isize) as usize;
        let preview_width = split_x.saturating_sub(total_left);
        let tree_width = total_right.saturating_sub(split_x);
        let old_preview = self.right_sidebar_file_preview_width;
        self.right_sidebar_file_preview_width = preview_width;
        let tree_changed = self.set_right_sidebar_file_tree_width(tree_width);
        old_preview != self.right_sidebar_file_preview_width || tree_changed
    }

    pub(crate) fn right_sidebar_note_pane_rect(&self) -> Option<RightSidebarRect> {
        let sidebar = self.right_sidebar_rect()?;
        let width = self.right_sidebar_note_pane_width()?;
        if sidebar.width <= width {
            return None;
        }
        Some(RightSidebarRect {
            x: sidebar.x,
            y: sidebar.y,
            width,
            height: sidebar.height,
        })
    }

    pub(crate) fn set_right_sidebar_note_pane_split_x(&mut self, split_x: isize) -> bool {
        let Some(total_rect) = self.right_sidebar_rect() else {
            return false;
        };
        if self.right_sidebar_note_pane_rect().is_none() {
            return false;
        }

        let total_left = total_rect.x;
        let total_right = total_rect.x.saturating_add(total_rect.width);
        let min_pane = self.ui_px(NOTE_PANE_MIN_WIDTH);
        let max_pane = total_rect
            .width
            .saturating_sub(self.ui_px(RIGHT_SIDEBAR_MIN_WIDTH));
        let min_tree = self.ui_px(RIGHT_SIDEBAR_MIN_WIDTH);
        let max_tree = self.right_sidebar_max_width().min(total_rect.width);
        let min_split = total_left
            .saturating_add(min_pane)
            .max(total_right.saturating_sub(max_tree));
        let max_split = total_right
            .saturating_sub(min_tree)
            .min(total_left.saturating_add(max_pane));
        if min_split > max_split {
            return false;
        }

        let split_x = split_x.clamp(min_split as isize, max_split as isize) as usize;
        let pane_width = split_x.saturating_sub(total_left);
        let tree_width = total_right.saturating_sub(split_x);
        let old_pane = self.right_sidebar_note_pane_width;
        self.right_sidebar_note_pane_width = pane_width;
        let old_tree = self.right_sidebar_width;
        self.set_right_sidebar_width(tree_width);
        old_pane != self.right_sidebar_note_pane_width || old_tree != self.right_sidebar_width
    }

    pub fn persist_right_sidebar_note_pane_width(&self) {
        let width = self
            .right_sidebar_note_pane_width()
            .unwrap_or(self.right_sidebar_note_pane_width)
            .max(self.ui_px(NOTE_PANE_MIN_WIDTH));
        let width = unscale_ui_usize(width, self.dimensions.dpi);
        if let Err(err) = crate::native_settings::save_right_sidebar_note_pane_width(width) {
            log::warn!("failed to save right sidebar Note pane width: {err:#}");
        }
    }

    /// Reflow the terminal when a state change (space switch, vault binding,
    /// pane activation) altered the computed sidebar width; a bare invalidate
    /// leaves the terminal sized for the old width and the difference shows
    /// as dead space.
    pub(crate) fn reflow_right_sidebar_if_width_changed(&mut self, previous_width: usize) {
        if self.right_sidebar_width() == previous_width {
            return;
        }
        if let Some(window) = self.window.as_ref().cloned() {
            let dimensions = self.dimensions;
            self.apply_dimensions(&dimensions, None, &window);
            window.invalidate();
        }
    }

    /// Settle a Space-switch reflow that was deferred until the destination
    /// mux window was adopted.
    pub(crate) fn consume_pending_sidebar_reflow(&mut self) {
        if let Some(previous_width) = self.pending_sidebar_reflow_width.take() {
            self.reflow_right_sidebar_if_width_changed(previous_width);
        }
    }

    pub(crate) fn toggle_right_sidebar_note_pane(&mut self) {
        self.right_sidebar_note_pane_expanded = !self.right_sidebar_note_pane_expanded;
        if let Err(err) = crate::native_settings::save_right_sidebar_note_pane_expanded(
            self.right_sidebar_note_pane_expanded,
        ) {
            log::warn!("failed to save right sidebar Note pane mode: {err:#}");
        }
        // The sidebar total width changed; reflow the terminal like the file
        // preview open/close paths do.
        if let Some(window) = self.window.as_ref().cloned() {
            let dimensions = self.dimensions;
            self.apply_dimensions(&dimensions, None, &window);
            window.invalidate();
        }
    }

    fn right_sidebar_file_preview_font_size(&self) -> f64 {
        let settings = crate::native_settings::load();
        let base_font_size = crate::native_settings::right_sidebar_font_size(&settings);
        (base_font_size + 2.0).max(FILE_FONT_MIN_SIZE)
    }

    fn right_sidebar_file_preview_render_metrics(&self) -> RenderMetrics {
        self.fonts
            .title_font_with_size(self.right_sidebar_file_preview_font_size())
            .map(|font| RenderMetrics::with_font_metrics(&font.metrics()))
            .unwrap_or(self.render_metrics)
    }

    fn right_sidebar_file_preview_body_metrics(
        &self,
        preview_metrics: RenderMetrics,
    ) -> Option<RightSidebarFilePreviewBodyMetrics> {
        if self.right_sidebar_file_view != RightSidebarFileView::Preview
            || (self.right_sidebar_file_selected.is_none()
                && self.right_sidebar_remote_files.selected.is_none())
        {
            return None;
        }

        let rect = self.right_sidebar_file_preview_rect()?;
        let content_x = rect.x + self.ui_px(SIDEBAR_INSET) * 2;
        let content_width = rect.width.saturating_sub(self.ui_px(SIDEBAR_INSET) * 4);
        let content_top = rect.y + self.ui_px(SIDEBAR_INSET) * 2;
        let content_bottom = rect.y.saturating_add(rect.height);
        let y = content_top + self.ui_px(FILE_PREVIEW_HEADER_HEIGHT);
        let bottom = content_bottom.saturating_sub(self.ui_px(SIDEBAR_INSET));
        let visible_height = bottom.saturating_sub(y);
        if visible_height == 0 || content_width == 0 {
            return None;
        }

        let line_height = preview_metrics.cell_size.height as usize + self.ui_px(4);
        let total_height = if self.right_sidebar_file_preview_image.is_some() {
            visible_height
        } else {
            let line_count = preview_line_count(&self.right_sidebar_file_preview_lines);
            line_count.saturating_mul(line_height)
                + usize::from(self.right_sidebar_file_preview_truncated).saturating_mul(line_height)
        };

        Some(RightSidebarFilePreviewBodyMetrics {
            x: content_x,
            y,
            width: content_width,
            bottom,
            line_height,
            visible_height,
            total_height,
        })
    }

    fn right_sidebar_file_preview_vertical_scrollbar_active(
        &self,
        metrics: RightSidebarFilePreviewBodyMetrics,
    ) -> bool {
        metrics.total_height > metrics.visible_height
    }

    fn right_sidebar_file_preview_text_layout(
        &self,
        metrics: RightSidebarFilePreviewBodyMetrics,
        preview_metrics: RenderMetrics,
    ) -> (usize, usize, usize, usize) {
        let scrollbar_reserve =
            if self.right_sidebar_file_preview_vertical_scrollbar_active(metrics) {
                self.ui_px(SIDEBAR_INSET) + 4
            } else {
                0
            };
        let body_width = metrics.width.saturating_sub(scrollbar_reserve);
        let line_count = preview_line_count(&self.right_sidebar_file_preview_lines);
        let number_digits = decimal_digit_count(line_count);
        let number_width = file_preview_line_number_width(
            number_digits,
            preview_metrics.cell_size.width.max(1) as usize,
            body_width,
            self.ui_px(SIDEBAR_ICON_GAP),
        );
        let text_x = metrics
            .x
            .saturating_add(number_width)
            .saturating_add(self.ui_px(SIDEBAR_ICON_GAP));
        let text_width = metrics.x.saturating_add(body_width).saturating_sub(text_x);
        (body_width, number_width, text_x, text_width)
    }

    fn right_sidebar_file_preview_text_width(
        &self,
        preview_metrics: RenderMetrics,
    ) -> Option<usize> {
        let metrics = self.right_sidebar_file_preview_body_metrics(preview_metrics)?;
        let (_, _, _, text_width) =
            self.right_sidebar_file_preview_text_layout(metrics, preview_metrics);
        Some(text_width)
    }

    fn right_sidebar_file_preview_visible_columns(&self, preview_metrics: RenderMetrics) -> usize {
        let Some(text_width) = self.right_sidebar_file_preview_text_width(preview_metrics) else {
            return 0;
        };
        let cell_width = preview_metrics.cell_size.width.max(1) as usize;
        estimated_file_preview_visible_columns(text_width, cell_width)
    }

    fn right_sidebar_file_preview_max_line_columns(&self) -> usize {
        // `right_sidebar_file_preview_max_columns` is the cached max over all
        // preview lines, recomputed only when the lines change (see
        // `apply_right_sidebar_file_preview_result`). This getter is called
        // several times per frame, so it must stay O(1).
        let max_line_columns = self.right_sidebar_file_preview_max_columns;
        if self.right_sidebar_file_preview_truncated {
            max_line_columns.max(FILE_PREVIEW_TRUNCATED_LABEL.chars().count())
        } else {
            max_line_columns
        }
    }

    pub(crate) fn right_sidebar_file_preview_horizontal_scroll_max(&self) -> usize {
        let preview_metrics = self.right_sidebar_file_preview_render_metrics();
        self.right_sidebar_file_preview_horizontal_scroll_max_with_metrics(preview_metrics)
    }

    fn right_sidebar_file_preview_horizontal_scroll_max_with_metrics(
        &self,
        preview_metrics: RenderMetrics,
    ) -> usize {
        self.right_sidebar_file_preview_max_line_columns()
            .saturating_sub(self.right_sidebar_file_preview_visible_columns(preview_metrics))
    }

    fn right_sidebar_file_preview_horizontal_scroll_active(
        &self,
        preview_metrics: RenderMetrics,
    ) -> bool {
        self.right_sidebar_file_preview_horizontal_scroll_max_with_metrics(preview_metrics) > 0
    }

    fn right_sidebar_file_preview_effective_visible_height(
        &self,
        metrics: RightSidebarFilePreviewBodyMetrics,
        preview_metrics: RenderMetrics,
    ) -> usize {
        let horizontal_reserve =
            if self.right_sidebar_file_preview_horizontal_scroll_active(preview_metrics) {
                self.ui_px(SIDEBAR_INSET) + self.ui_px(FILE_PREVIEW_SCROLLBAR_THICKNESS)
            } else {
                0
            };
        metrics.visible_height.saturating_sub(horizontal_reserve)
    }

    pub(crate) fn right_sidebar_file_preview_scroll_max(&self) -> f32 {
        let preview_metrics = self.right_sidebar_file_preview_render_metrics();
        self.right_sidebar_file_preview_scroll_max_with_metrics(preview_metrics)
    }

    fn right_sidebar_file_preview_scroll_max_with_metrics(
        &self,
        preview_metrics: RenderMetrics,
    ) -> f32 {
        let Some(metrics) = self.right_sidebar_file_preview_body_metrics(preview_metrics) else {
            return 0.0;
        };
        metrics.total_height.saturating_sub(
            self.right_sidebar_file_preview_effective_visible_height(metrics, preview_metrics),
        ) as f32
    }

    pub(crate) fn right_sidebar_file_preview_scroll_geometry(
        &self,
    ) -> Option<RightSidebarFilePreviewScrollGeometry> {
        let preview_metrics = self.right_sidebar_file_preview_render_metrics();
        let metrics = self.right_sidebar_file_preview_body_metrics(preview_metrics)?;
        let visible_height =
            self.right_sidebar_file_preview_effective_visible_height(metrics, preview_metrics);
        let max_scroll = metrics.total_height.saturating_sub(visible_height) as f32;
        if max_scroll <= 0.0 || metrics.total_height == 0 {
            return None;
        }

        let track_width = self.ui_px(FILE_PREVIEW_SCROLLBAR_THICKNESS);
        let track_height = visible_height.max(1);
        let thumb_height = ((visible_height as f32 / metrics.total_height as f32)
            * track_height as f32)
            .clamp(self.ui_f32(28.0), track_height as f32);
        let travel = (track_height as f32 - thumb_height).max(1.0);
        let scroll_offset = self
            .right_sidebar_file_preview_scroll_offset
            .clamp(0.0, max_scroll);
        let thumb_y = metrics.y as f32 + (scroll_offset / max_scroll) * travel;
        let track_x = metrics
            .x
            .saturating_add(metrics.width)
            .saturating_sub(track_width);

        Some(RightSidebarFilePreviewScrollGeometry {
            track_x,
            track_y: metrics.y,
            track_width,
            track_height,
            thumb_y,
            thumb_height,
            max_scroll,
        })
    }

    pub(crate) fn right_sidebar_file_preview_horizontal_scroll_geometry(
        &self,
    ) -> Option<RightSidebarFilePreviewHorizontalScrollGeometry> {
        let preview_metrics = self.right_sidebar_file_preview_render_metrics();
        let metrics = self.right_sidebar_file_preview_body_metrics(preview_metrics)?;
        let max_scroll =
            self.right_sidebar_file_preview_horizontal_scroll_max_with_metrics(preview_metrics);
        if max_scroll == 0 {
            return None;
        }

        let (_, _, text_x, text_width) =
            self.right_sidebar_file_preview_text_layout(metrics, preview_metrics);
        if text_width == 0 {
            return None;
        }

        let max_columns = self.right_sidebar_file_preview_max_line_columns().max(1);
        let visible_columns = self
            .right_sidebar_file_preview_visible_columns(preview_metrics)
            .max(1);
        let track_width = text_width.max(1);
        let track_height = self.ui_px(FILE_PREVIEW_SCROLLBAR_THICKNESS);
        let thumb_width = ((visible_columns as f32 / max_columns as f32) * track_width as f32)
            .clamp(self.ui_f32(28.0), track_width as f32);
        let travel = (track_width as f32 - thumb_width).max(1.0);
        let scroll_offset = self
            .right_sidebar_file_preview_horizontal_offset
            .min(max_scroll);
        let thumb_x = text_x as f32 + (scroll_offset as f32 / max_scroll as f32) * travel;
        let track_y = metrics.bottom.saturating_sub(track_height);

        Some(RightSidebarFilePreviewHorizontalScrollGeometry {
            track_x: text_x,
            track_y,
            track_width,
            track_height,
            thumb_x,
            thumb_width,
            max_scroll,
        })
    }

    fn right_sidebar_snippet_scroll_height(
        &self,
        snippet_count: usize,
        viewport_height: usize,
    ) -> usize {
        let row_height = self.ui_px(SNIPPET_CARD_HEIGHT) + self.ui_px(SNIPPET_ROW_GAP);
        let height = snippet_count
            .saturating_mul(row_height)
            .saturating_sub(self.ui_px(SNIPPET_ROW_GAP));
        if height > viewport_height {
            height.saturating_add(self.ui_px(SNIPPET_LIST_BOTTOM_PADDING))
        } else {
            height
        }
    }

    fn right_sidebar_snippet_scroll_metrics(&self) -> Option<(usize, usize, usize)> {
        let Some(rect) = self.right_sidebar_rect() else {
            return None;
        };
        if self.right_sidebar_collapsed
            || self.right_sidebar_mode != RightSidebarMode::Snippets
            || self.right_sidebar_snippet_view != RightSidebarSnippetView::List
        {
            return None;
        }

        let top_bar_y = rect.y + self.ui_px(SIDEBAR_INSET) * 2;
        let top_bar_height = self.ui_px(RIGHT_SIDEBAR_TOP_BAR_HEIGHT).min(
            rect.y
                .saturating_add(rect.height)
                .saturating_sub(top_bar_y + self.ui_px(SIDEBAR_INSET)),
        );
        let content_top = top_bar_y
            + top_bar_height
            + self.ui_px(RIGHT_SIDEBAR_MODE_HEIGHT)
            + self.ui_px(RIGHT_SIDEBAR_SECTION_GAP);
        let list_top = content_top
            + self
                .ui_px(SNIPPET_TOOLBAR_HEIGHT)
                .max(self.ui_px(SNIPPET_SEARCH_HEIGHT))
            + self.ui_px(SNIPPET_LIST_TOP_GAP);
        let content_bottom = rect.y.saturating_add(rect.height);
        let visible_height = content_bottom.saturating_sub(list_top + self.ui_px(SIDEBAR_INSET));
        if visible_height == 0 {
            return None;
        }
        let snippet_count = self.filtered_snippet_count();
        let total_height = self.right_sidebar_snippet_scroll_height(snippet_count, visible_height);
        Some((list_top, visible_height, total_height))
    }

    pub(crate) fn right_sidebar_snippet_scroll_max(&self) -> f32 {
        let Some((_, visible_height, total_height)) = self.right_sidebar_snippet_scroll_metrics()
        else {
            return 0.0;
        };
        total_height.saturating_sub(visible_height) as f32
    }

    pub(crate) fn right_sidebar_snippet_scroll_geometry(
        &self,
    ) -> Option<RightSidebarSnippetScrollGeometry> {
        let rect = self.right_sidebar_rect()?;
        let (list_top, visible_height, total_height) =
            self.right_sidebar_snippet_scroll_metrics()?;
        let max_scroll = total_height.saturating_sub(visible_height) as f32;
        if max_scroll <= 0.0 || total_height == 0 {
            return None;
        }

        let track_width = 4usize;
        let track_height = visible_height.max(1);
        let thumb_height = ((visible_height as f32 / total_height as f32) * track_height as f32)
            .clamp(self.ui_f32(28.0), track_height as f32);
        let travel = (track_height as f32 - thumb_height).max(1.0);
        let scroll_offset = self
            .right_sidebar_snippet_scroll_offset
            .clamp(0.0, max_scroll);
        let thumb_y = list_top as f32 + (scroll_offset / max_scroll) * travel;
        let track_x = rect
            .x
            .saturating_add(rect.width)
            .saturating_sub(self.ui_px(SIDEBAR_INSET) / 2 + track_width);

        Some(RightSidebarSnippetScrollGeometry {
            track_x,
            track_y: list_top,
            track_width,
            track_height,
            thumb_y,
            thumb_height,
            max_scroll,
        })
    }

    pub(crate) fn show_right_sidebar_snippet_scrollbar(&mut self) {
        self.right_sidebar_snippet_scrollbar_visible_until =
            Some(Instant::now() + Duration::from_millis(RIGHT_SIDEBAR_SCROLLBAR_VISIBLE_MS));
    }

    fn right_sidebar_snippet_scrollbar_visible(&self) -> bool {
        let actively_dragging = self.dragging.as_ref().is_some_and(|(item, _)| {
            matches!(
                item.item_type,
                UIItemType::RightSidebarSnippetScrollTrack
                    | UIItemType::RightSidebarSnippetScrollThumb
            )
        });
        actively_dragging
            || self
                .right_sidebar_snippet_scrollbar_visible_until
                .is_some_and(|until| until > Instant::now())
    }

    fn right_sidebar_snippet_cursor_on(&self) -> bool {
        let blink_ms = (self.config.cursor_blink_rate as u64).max(100);
        self.update_next_frame_time(Some(Instant::now() + Duration::from_millis(blink_ms)));
        let ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        snippet_cursor_visible(ms, blink_ms)
    }

    fn filtered_snippet_count(&self) -> usize {
        let needle = self
            .right_sidebar_snippet_search
            .text()
            .trim()
            .to_ascii_lowercase();
        crate::snippets::list_snippets()
            .into_iter()
            .filter(|snippet| {
                needle.is_empty()
                    || snippet.title.to_ascii_lowercase().contains(&needle)
                    || snippet.body.to_ascii_lowercase().contains(&needle)
            })
            .count()
    }

    pub fn paint_right_sidebar(
        &mut self,
        layers: &mut TripleLayerQuadAllocator,
    ) -> anyhow::Result<()> {
        // Recorded fresh each frame; consumed by mouse hit-testing.
        self.right_sidebar_input_layouts.clear();
        let total_rect = match self.right_sidebar_rect() {
            Some(rect) => rect,
            None => return Ok(()),
        };
        let rect = self.right_sidebar_tree_rect(total_rect);
        let chrome = UiPalette::for_appearance(crate::native_settings::effective_appearance());
        let foreground = chrome.text;
        let muted_fg = chrome.secondary_text;
        let sidebar_bg = chrome.workspace_sidebar_bg;
        let settings = crate::native_settings::load();
        let base_font_size = crate::native_settings::right_sidebar_font_size(&settings);
        let ui_font = self
            .fonts
            .title_font_with_size(base_font_size)
            .context("right sidebar ui font")?;
        let ui_metrics = RenderMetrics::with_font_metrics(&ui_font.metrics());
        let ui_cell_height = ui_metrics.cell_size.height as usize;
        let icon_size = (ui_cell_height + self.ui_px(6)).clamp(self.ui_px(20), self.ui_px(24));

        if self.right_sidebar_mode == RightSidebarMode::Chat {
            if !matches!(self.active_remote_project_for_files(), Ok(Some(_))) {
                let _ = self.sync_right_sidebar_file_root_for_current_workspace();
            }
        }

        if let Some(preview_rect) = self.right_sidebar_file_preview_rect() {
            let file_font_size = self.right_sidebar_file_preview_font_size();
            let file_font = self
                .fonts
                .title_font_with_size(file_font_size)
                .context("right sidebar file preview font")?;
            let file_metrics = RenderMetrics::with_font_metrics(&file_font.metrics());
            self.paint_right_sidebar_file_preview_pane(
                layers,
                &file_font,
                file_metrics,
                chrome,
                foreground,
                muted_fg,
                preview_rect,
            )?;
        }

        if let Some(pane_rect) = self.right_sidebar_note_pane_rect() {
            // Chrome only; paint_note_sidebar paints the editor into this
            // rect later in the frame.
            if pane_rect.y > 0 {
                self.filled_rectangle(
                    layers,
                    0,
                    euclid::rect(
                        pane_rect.x as f32,
                        0.0,
                        pane_rect.width as f32,
                        pane_rect.y as f32,
                    ),
                    sidebar_bg,
                )
                .context("right sidebar note pane top background")?;
            }
            self.filled_rectangle(
                layers,
                0,
                euclid::rect(
                    pane_rect.x as f32,
                    pane_rect.y as f32,
                    pane_rect.width as f32,
                    pane_rect.height as f32,
                ),
                sidebar_bg,
            )
            .context("right sidebar note pane background")?;
            self.ui_items.push(UIItem {
                x: pane_rect.x,
                y: 0,
                width: pane_rect.width,
                height: pane_rect.y.saturating_add(pane_rect.height),
                item_type: UIItemType::RightSidebarBackground,
            });
            self.filled_rectangle(
                layers,
                1,
                euclid::rect(
                    pane_rect.x as f32,
                    pane_rect.y as f32,
                    1.0,
                    pane_rect.height as f32,
                ),
                chrome.separator,
            )
            .context("right sidebar note pane left separator")?;
        }

        if rect.y > 0 {
            self.filled_rectangle(
                layers,
                0,
                euclid::rect(rect.x as f32, 0.0, rect.width as f32, rect.y as f32),
                sidebar_bg,
            )
            .context("right sidebar top background")?;
        }
        self.filled_rectangle(
            layers,
            0,
            euclid::rect(
                rect.x as f32,
                rect.y as f32,
                rect.width as f32,
                rect.height as f32,
            ),
            sidebar_bg,
        )
        .context("right sidebar background")?;
        self.ui_items.push(UIItem {
            x: rect.x,
            y: 0,
            width: rect.width,
            height: rect.y.saturating_add(rect.height),
            item_type: UIItemType::RightSidebarBackground,
        });
        self.filled_rectangle(
            layers,
            1,
            euclid::rect(rect.x as f32, rect.y as f32, 1.0, rect.height as f32),
            chrome.separator,
        )
        .context("right sidebar separator")?;
        self.ui_items.push(UIItem {
            x: total_rect
                .x
                .saturating_sub(self.ui_px(SIDEBAR_RESIZE_HANDLE_WIDTH) / 2),
            y: total_rect.y,
            width: self.ui_px(SIDEBAR_RESIZE_HANDLE_WIDTH),
            height: total_rect.height,
            item_type: UIItemType::RightSidebarResize,
        });
        if self.right_sidebar_file_preview_rect().is_some() {
            self.ui_items.push(UIItem {
                x: rect
                    .x
                    .saturating_sub(self.ui_px(SIDEBAR_RESIZE_HANDLE_WIDTH) / 2),
                y: rect.y,
                width: self.ui_px(SIDEBAR_RESIZE_HANDLE_WIDTH),
                height: rect.height,
                item_type: UIItemType::RightSidebarFilePreviewResize,
            });
        }
        if self.right_sidebar_note_pane_rect().is_some() {
            self.ui_items.push(UIItem {
                x: rect
                    .x
                    .saturating_sub(self.ui_px(SIDEBAR_RESIZE_HANDLE_WIDTH) / 2),
                y: rect.y,
                width: self.ui_px(SIDEBAR_RESIZE_HANDLE_WIDTH),
                height: rect.height,
                item_type: UIItemType::RightSidebarNotePaneResize,
            });
        }

        let content_x = rect.x + self.ui_px(SIDEBAR_INSET) * 2;
        let content_width = rect.width.saturating_sub(self.ui_px(SIDEBAR_INSET) * 4);
        let top_bar_y = rect.y + self.ui_px(SIDEBAR_INSET) * 2;
        let top_bar_height = self.ui_px(RIGHT_SIDEBAR_TOP_BAR_HEIGHT).min(
            rect.y
                .saturating_add(rect.height)
                .saturating_sub(top_bar_y + self.ui_px(SIDEBAR_INSET)),
        );
        if top_bar_height == 0 {
            return Ok(());
        }

        if cfg!(target_os = "macos") {
            let close_button_size = self
                .ui_px(RIGHT_SIDEBAR_CLOSE_BUTTON_SIZE)
                .min(top_bar_height)
                .min(content_width)
                .max(1);
            let window_button_reserve = self.right_sidebar_window_button_reserved_width();
            let close_button_right_limit = rect
                .x
                .saturating_add(rect.width)
                .saturating_sub(self.ui_px(SIDEBAR_INSET))
                .saturating_sub(close_button_size)
                .saturating_sub(window_button_reserve);
            let close_button_x = content_x
                .saturating_add(content_width)
                .saturating_sub(close_button_size)
                .saturating_add(self.ui_px(RIGHT_SIDEBAR_CLOSE_BUTTON_X_ADJUST))
                .min(close_button_right_limit);
            let close_button_y = (top_bar_y
                + (top_bar_height.saturating_sub(close_button_size)) / 2)
                .saturating_sub(self.ui_px(RIGHT_SIDEBAR_CLOSE_BUTTON_Y_ADJUST));
            let close_hovered = self.is_pointer_over_ui_rect(
                close_button_x,
                close_button_y,
                close_button_size,
                close_button_size,
            );
            let close_pressed = close_hovered
                && self.is_pointer_pressing_ui_rect(
                    close_button_x,
                    close_button_y,
                    close_button_size,
                    close_button_size,
                );
            let close_press_inset = if close_pressed { 1 } else { 0 };
            let close_visual_size = close_button_size.saturating_sub(close_press_inset * 2);
            if close_hovered || close_pressed {
                let close_fill = if close_pressed {
                    chrome.control_pressed_bg
                } else {
                    chrome.control_hover_bg
                };
                self.fill_rounded_rectangle_with_border(
                    layers,
                    2,
                    euclid::rect(
                        (close_button_x + close_press_inset) as f32,
                        (close_button_y + close_press_inset) as f32,
                        close_visual_size as f32,
                        close_visual_size as f32,
                    ),
                    close_fill,
                    foreground.mul_alpha(0.58),
                    WINDOW_TAB_ADD_BUTTON_RADIUS,
                    ICON_BUTTON_BORDER_WIDTH,
                )
                .context("right sidebar close button")?;
            }
            self.ui_items.push(UIItem {
                x: close_button_x,
                y: close_button_y,
                width: close_button_size,
                height: close_button_size,
                item_type: UIItemType::RightSidebarToggle,
            });
            let close_icon_size = self
                .ui_px(RIGHT_SIDEBAR_CLOSE_ICON_SIZE)
                .min(close_visual_size.saturating_sub(self.ui_px(4)))
                .max(1);
            self.paint_sidebar_icon(
                layers,
                self.right_sidebar_toggle_icon(),
                close_button_x
                    + close_press_inset
                    + (close_visual_size.saturating_sub(close_icon_size)) / 2,
                close_button_y
                    + close_press_inset
                    + (close_visual_size.saturating_sub(close_icon_size)) / 2,
                close_icon_size,
                if close_hovered { foreground } else { muted_fg },
            )?;
        }

        let mode_y = top_bar_y + top_bar_height + self.ui_px(RIGHT_SIDEBAR_SECTION_GAP);
        let mode_height = self.ui_px(RIGHT_SIDEBAR_MODE_HEIGHT).min(
            rect.y
                .saturating_add(rect.height)
                .saturating_sub(mode_y + self.ui_px(SIDEBAR_INSET)),
        );
        if mode_height == 0 {
            return Ok(());
        }
        self.fill_rounded_rectangle_with_border(
            layers,
            1,
            euclid::rect(
                content_x as f32,
                mode_y as f32,
                content_width as f32,
                mode_height as f32,
            ),
            chrome.sidebar_button_bg,
            chrome.control_border,
            WINDOW_TAB_ADD_BUTTON_RADIUS,
            CAPSULE_BORDER_WIDTH,
        )
        .context("right sidebar mode selector")?;

        let modes = [
            RightSidebarMode::Chat,
            RightSidebarMode::Tasks,
            RightSidebarMode::Snippets,
        ];
        const MODE_LABEL_CLIP_SLOP: usize = 4;
        let mode_icon_size = (ui_cell_height + self.ui_px(12))
            .clamp(self.ui_px(24), self.ui_px(30))
            .min(mode_height.saturating_sub(self.ui_px(22)))
            .max(1);
        let active_label_target_width = self
            .sidebar_text_width(&ui_font, self.right_sidebar_mode.label())?
            .ceil() as usize
            + self.ui_px(MODE_LABEL_CLIP_SLOP);
        let inactive_segment_min_width = (mode_icon_size + self.ui_px(SIDEBAR_INSET) * 4)
            .max(self.ui_px(70))
            .min((content_width / modes.len()).max(1));
        let inactive_segment_count = modes.len().saturating_sub(1);
        let inactive_segments_min_width =
            inactive_segment_min_width.saturating_mul(inactive_segment_count);
        let active_segment_width = (mode_icon_size
            + self.ui_px(SIDEBAR_ICON_GAP)
            + active_label_target_width
            + self.ui_px(SIDEBAR_INSET) * 6)
            .min(
                content_width
                    .saturating_sub(inactive_segments_min_width)
                    .max(1),
            )
            .max(1);
        let inactive_segments_width = content_width.saturating_sub(active_segment_width);
        let inactive_segment_width = if inactive_segment_count > 0 {
            inactive_segments_width / inactive_segment_count
        } else {
            0
        };
        let mut inactive_segment_remainder = if inactive_segment_count > 0 {
            inactive_segments_width % inactive_segment_count
        } else {
            0
        };
        let mut segment_x = content_x;
        for mode in modes.iter() {
            let mode = *mode;
            let active = mode == self.right_sidebar_mode;
            let segment_width = if active {
                active_segment_width
            } else {
                let extra = usize::from(inactive_segment_remainder > 0);
                inactive_segment_remainder = inactive_segment_remainder.saturating_sub(extra);
                inactive_segment_width.saturating_add(extra)
            };
            let remaining_width = content_x
                .saturating_add(content_width)
                .saturating_sub(segment_x);
            let segment_width = segment_width.min(remaining_width);
            let segment_right = segment_x.saturating_add(segment_width);
            if segment_width == 0 {
                continue;
            }

            let hovered =
                self.is_pointer_over_ui_rect(segment_x, mode_y, segment_width, mode_height);
            self.ui_items.push(UIItem {
                x: segment_x,
                y: mode_y,
                width: segment_width,
                height: mode_height,
                item_type: UIItemType::RightSidebarMode(mode),
            });

            if active {
                let inner_inset = self.ui_px(5);
                self.fill_rounded_rectangle_with_border(
                    layers,
                    2,
                    euclid::rect(
                        (segment_x + inner_inset) as f32,
                        (mode_y + inner_inset) as f32,
                        segment_width.saturating_sub(inner_inset * 2) as f32,
                        mode_height.saturating_sub(inner_inset * 2) as f32,
                    ),
                    chrome.control_bg,
                    chrome.control_border,
                    WINDOW_TAB_ADD_BUTTON_RADIUS,
                    CAPSULE_BORDER_WIDTH,
                )
                .context("right sidebar active mode")?;
            } else if hovered {
                self.fill_rounded_rectangle(
                    layers,
                    2,
                    euclid::rect(
                        (segment_x + self.ui_px(5)) as f32,
                        (mode_y + self.ui_px(5)) as f32,
                        segment_width.saturating_sub(self.ui_px(10)) as f32,
                        mode_height.saturating_sub(self.ui_px(10)) as f32,
                    ),
                    chrome.sidebar_button_hover_bg,
                    WINDOW_TAB_ADD_BUTTON_RADIUS,
                )
                .context("right sidebar hovered mode")?;
            }

            let label_width = if active {
                segment_width
                    .saturating_sub(
                        mode_icon_size
                            + self.ui_px(SIDEBAR_ICON_GAP)
                            + self.ui_px(SIDEBAR_INSET) * 2,
                    )
                    .min(active_label_target_width)
            } else {
                0
            };
            let total_width = if active {
                mode_icon_size + self.ui_px(SIDEBAR_ICON_GAP) + label_width
            } else {
                mode_icon_size
            };
            let icon_x = segment_x + (segment_width.saturating_sub(total_width) / 2);
            let icon_y =
                (mode_y + (mode_height.saturating_sub(mode_icon_size)) / 2).saturating_sub(1);
            self.paint_sidebar_icon(
                layers,
                mode.icon(),
                icon_x,
                icon_y,
                mode_icon_size,
                if active || hovered {
                    foreground
                } else {
                    muted_fg
                },
            )?;
            if active && label_width > 0 {
                self.paint_sidebar_text(
                    layers,
                    &ui_font,
                    ui_metrics,
                    mode.label(),
                    icon_x + mode_icon_size + self.ui_px(SIDEBAR_ICON_GAP),
                    mode_y + (mode_height.saturating_sub(ui_cell_height)) / 2,
                    label_width,
                    foreground,
                )?;
            }
            segment_x = segment_right;
        }

        let content_top = mode_y + mode_height + self.ui_px(RIGHT_SIDEBAR_SECTION_GAP);
        match self.right_sidebar_mode {
            RightSidebarMode::Chat => {
                let file_font_size = self.right_sidebar_file_preview_font_size();
                let file_font = self
                    .fonts
                    .title_font_with_size(file_font_size)
                    .context("right sidebar file font")?;
                let file_metrics = RenderMetrics::with_font_metrics(&file_font.metrics());
                let file_icon_size = (file_metrics.cell_size.height as usize + self.ui_px(6))
                    .clamp(self.ui_px(22), self.ui_px(28));
                self.paint_files_sidebar(
                    layers,
                    &file_font,
                    file_metrics,
                    chrome,
                    foreground,
                    muted_fg,
                    content_x,
                    content_top,
                    content_width,
                    rect.y.saturating_add(rect.height),
                    file_icon_size,
                )?;
                return Ok(());
            }
            RightSidebarMode::Snippets => {
                self.paint_snippets_sidebar(
                    layers,
                    &ui_font,
                    ui_metrics,
                    chrome,
                    foreground,
                    muted_fg,
                    content_x,
                    content_top,
                    content_width,
                    rect.y.saturating_add(rect.height),
                    icon_size,
                )?;
                return Ok(());
            }
            RightSidebarMode::Tasks => {
                let stage = crate::input_diagnostics::StageTimer::begin("note_paint");
                let result = self.paint_note_sidebar(
                    layers,
                    &ui_font,
                    ui_metrics,
                    chrome,
                    foreground,
                    muted_fg,
                    content_x,
                    content_top,
                    content_width,
                    rect.y.saturating_add(rect.height),
                    base_font_size,
                );
                stage.finish(result.is_ok());
                result?;
                return Ok(());
            }
        }
    }

    fn refresh_note_vault_index_if_needed(&mut self, root: &Path) {
        let root_changed = self
            .right_sidebar_note_vault_index_root
            .as_ref()
            .is_none_or(|current| current != root);
        if root_changed {
            self.right_sidebar_note_vault_index_generation = self
                .right_sidebar_note_vault_index_generation
                .wrapping_add(1);
            self.right_sidebar_note_vault_index_root = Some(root.to_path_buf());
            self.right_sidebar_note_vault_paths = Arc::new(Vec::new());
            self.right_sidebar_note_tree_expanded.clear();
            self.right_sidebar_note_vault_last_scan = None;
            self.right_sidebar_note_vault_indexing = false;
            self.right_sidebar_note_tree_scroll_offset = 0.0;
        }
        if self.right_sidebar_note_vault_indexing
            || (!root_changed
                && self.right_sidebar_note_vault_last_scan.is_some_and(|last| {
                    last.elapsed() < Duration::from_secs(NOTE_VAULT_RESCAN_SECS)
                }))
        {
            return;
        }

        self.right_sidebar_note_vault_index_generation = self
            .right_sidebar_note_vault_index_generation
            .wrapping_add(1);
        let generation = self.right_sidebar_note_vault_index_generation;
        self.right_sidebar_note_vault_indexing = true;
        self.right_sidebar_note_vault_last_scan = Some(Instant::now());
        let root = root.to_path_buf();
        let worker_root = root.clone();
        let active_document = self
            .right_sidebar_note
            .document
            .as_ref()
            .filter(|document| document.vault_root == root)
            .map(|document| {
                (
                    document.document_path.clone(),
                    document.session.lock().disk_stamp(),
                )
            });
        let Some(window) = self.window.clone() else {
            return;
        };
        promise::spawn::spawn(async move {
            let result = promise::spawn::spawn_into_new_thread(move || {
                let paths = vault_file_paths(&worker_root)?;
                let active_snapshot = active_document
                    .map(|(path, known_stamp)| -> anyhow::Result<_> {
                        let metadata = fs::metadata(&path)
                            .with_context(|| format!("stat {}", path.display()))?;
                        let modified = metadata
                            .modified()
                            .with_context(|| format!("read mtime {}", path.display()))?;
                        let len = metadata.len();
                        if known_stamp == Some((modified, len)) {
                            return Ok(None);
                        }
                        let source = fs::read_to_string(&path)
                            .with_context(|| format!("read {}", path.display()))?;
                        Ok(Some((modified, len, source)))
                    })
                    .transpose()?
                    .flatten();
                Ok::<_, anyhow::Error>((paths, active_snapshot))
            })
            .await;
            window.notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                if generation != term_window.right_sidebar_note_vault_index_generation
                    || term_window.right_sidebar_note_vault_index_root.as_ref() != Some(&root)
                {
                    return;
                }
                term_window.right_sidebar_note_vault_indexing = false;
                let mut changed = term_window.right_sidebar_note.document.is_none();
                match result {
                    Ok((paths, active_snapshot)) => {
                        if term_window.right_sidebar_note_vault_paths.as_ref() != &paths {
                            for path in &paths {
                                let mut parent = Path::new(path).parent();
                                while let Some(directory) = parent {
                                    if directory.as_os_str().is_empty() {
                                        break;
                                    }
                                    term_window
                                        .right_sidebar_note_tree_expanded
                                        .insert(directory.to_string_lossy().replace('\\', "/"));
                                    parent = directory.parent();
                                }
                            }
                            term_window.right_sidebar_note_vault_paths = Arc::new(paths);
                            changed = true;
                        }
                        if let Some((modified, len, source)) = active_snapshot {
                            changed |= term_window
                                .right_sidebar_note
                                .apply_external_snapshot(modified, len, source);
                        }
                    }
                    Err(err) => {
                        let message = format!("{err:#}");
                        changed = term_window.right_sidebar_note.load_error.as_deref()
                            != Some(message.as_str());
                        term_window.right_sidebar_note.load_error = Some(message);
                    }
                }
                if changed {
                    term_window.invalidate_window();
                }
            })));
        })
        .detach();
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_note_vault_tree(
        &mut self,
        layers: &mut TripleLayerQuadAllocator,
        ui_font: &Rc<LoadedFont>,
        ui_metrics: RenderMetrics,
        chrome: UiPalette,
        foreground: LinearRgba,
        muted_fg: LinearRgba,
        content_x: usize,
        content_top: usize,
        content_width: usize,
        content_bottom: usize,
        narrow: bool,
        show_header_button: bool,
    ) -> anyhow::Result<()> {
        let header_height = self.ui_px(NOTE_TOOLBAR_HEIGHT);
        let button_size = header_height;
        let mut title_x = content_x + self.ui_px(SIDEBAR_INSET);
        if narrow {
            self.paint_snippet_icon_button(
                layers,
                chrome,
                foreground,
                muted_fg,
                content_x,
                content_top,
                button_size,
                SvgIcon::ArrowLeft,
                UIItemType::RightSidebarNoteTreeBack,
            )?;
            title_x = content_x + button_size + self.ui_px(6);
        } else if show_header_button {
            self.paint_snippet_icon_button(
                layers,
                chrome,
                foreground,
                muted_fg,
                content_x + content_width.saturating_sub(button_size),
                content_top,
                button_size,
                SvgIcon::PanelLeftClose,
                UIItemType::RightSidebarNoteTreeToggle,
            )?;
        }
        self.paint_sidebar_text(
            layers,
            ui_font,
            ui_metrics,
            "Vault",
            title_x,
            content_top + header_height.saturating_sub(ui_metrics.cell_size.height as usize) / 2,
            content_x.saturating_add(content_width).saturating_sub(
                title_x + self.ui_px(SIDEBAR_INSET) + if narrow { 0 } else { button_size },
            ),
            foreground,
        )?;

        let rows = note_vault_tree_rows(
            &self.right_sidebar_note_vault_paths,
            &self.right_sidebar_note_tree_expanded,
        );
        let tree_top = content_top + header_height + self.ui_px(4);
        let tree_bottom = content_bottom.saturating_sub(self.ui_px(SIDEBAR_INSET));
        let visible_height = tree_bottom.saturating_sub(tree_top);
        let row_metrics = right_sidebar_file_row_metrics(ui_metrics);
        let max_scroll = rows
            .len()
            .saturating_mul(row_metrics.row_height)
            .saturating_sub(visible_height) as f32;
        self.right_sidebar_note_tree_scroll_offset = self
            .right_sidebar_note_tree_scroll_offset
            .clamp(0.0, max_scroll);
        let range = visible_file_row_range(
            rows.len(),
            self.right_sidebar_note_tree_scroll_offset,
            visible_height,
            row_metrics.row_height,
        );
        let selected = self
            .right_sidebar_note
            .document
            .as_ref()
            .map(|document| document.relative_path.as_str());
        for index in range {
            let row = &rows[index];
            let y = tree_top as f32 + (index * row_metrics.row_height) as f32
                - self.right_sidebar_note_tree_scroll_offset;
            let y = y.floor().max(tree_top as f32) as usize;
            let height = row_metrics.row_height.min(tree_bottom.saturating_sub(y));
            if height == 0 {
                continue;
            }
            let active = !row.is_dir && selected == Some(row.relative_path.as_str());
            let hovered = self.is_pointer_over_ui_rect(content_x, y, content_width, height);
            if active || hovered {
                self.fill_rounded_rectangle(
                    layers,
                    1,
                    euclid::rect(
                        content_x as f32,
                        y as f32,
                        content_width as f32,
                        height as f32,
                    ),
                    if active {
                        chrome.selected_bg.mul_alpha(0.46)
                    } else {
                        chrome.sidebar_button_hover_bg
                    },
                    self.ui_f32(SIDEBAR_ROW_RADIUS),
                )?;
            }
            self.ui_items.push(UIItem {
                x: content_x,
                y,
                width: content_width,
                height,
                item_type: UIItemType::RightSidebarNoteTreeRow(row.relative_path.clone()),
            });
            let indent = row
                .depth
                .saturating_mul(row_metrics.indent_step)
                .min(content_width.saturating_sub(24));
            let icon_x = content_x + self.ui_px(SIDEBAR_INSET) + indent;
            let icon_y = y + height.saturating_sub(row_metrics.icon_size) / 2;
            if row.is_dir {
                self.paint_sidebar_icon(
                    layers,
                    if self
                        .right_sidebar_note_tree_expanded
                        .contains(&row.relative_path)
                    {
                        SvgIcon::FolderOpen
                    } else {
                        SvgIcon::Folder
                    },
                    icon_x,
                    icon_y,
                    row_metrics.icon_size,
                    muted_fg,
                )?;
            } else {
                if let Some(icon) = material_file_icon_for_name(&row.name) {
                    self.paint_sidebar_material_icon(
                        layers,
                        icon,
                        icon_x,
                        icon_y,
                        row_metrics.icon_size,
                    )?;
                } else {
                    self.paint_sidebar_icon(
                        layers,
                        SvgIcon::FileText,
                        icon_x,
                        icon_y,
                        row_metrics.icon_size,
                        if active { foreground } else { muted_fg },
                    )?;
                }
            }
            let text_x = icon_x + row_metrics.icon_size + row_metrics.icon_gap;
            self.paint_sidebar_text(
                layers,
                ui_font,
                ui_metrics,
                &row.name,
                text_x,
                y + height.saturating_sub(ui_metrics.cell_size.height as usize) / 2,
                content_x
                    .saturating_add(content_width)
                    .saturating_sub(text_x + self.ui_px(SIDEBAR_INSET)),
                if active { foreground } else { muted_fg },
            )?;
        }

        if self.right_sidebar_note_tree_scroll_offset > 0.0 {
            self.paint_right_sidebar_file_top_fade(
                layers,
                chrome,
                content_x,
                tree_top,
                content_width,
                self.ui_px(FILE_SCROLL_FADE_HEIGHT),
            )?;
        }
        if !narrow {
            self.filled_rectangle(
                layers,
                1,
                euclid::rect(
                    content_x.saturating_add(content_width) as f32,
                    content_top as f32,
                    1.0,
                    content_bottom.saturating_sub(content_top) as f32,
                ),
                chrome.separator,
            )?;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_note_sidebar(
        &mut self,
        layers: &mut TripleLayerQuadAllocator,
        ui_font: &Rc<LoadedFont>,
        ui_metrics: RenderMetrics,
        chrome: UiPalette,
        foreground: LinearRgba,
        muted_fg: LinearRgba,
        content_x: usize,
        content_top: usize,
        content_width: usize,
        content_bottom: usize,
        base_font_size: f64,
    ) -> anyhow::Result<()> {
        self.right_sidebar_note_table_layouts.clear();
        let vault = workspace_threads::space_note_vault(&self.active_space_id);
        self.active_space_has_note_vault = vault.is_some();
        if let Some(vault) = vault {
            self.refresh_note_vault_index_if_needed(&vault.root);
        }
        if !self.ensure_active_right_sidebar_note_document() {
            let message = self
                .right_sidebar_note
                .load_error
                .clone()
                .unwrap_or_else(|| "Unable to open Notes".to_string());
            self.paint_sidebar_text(
                layers,
                ui_font,
                ui_metrics,
                &message,
                content_x,
                content_top,
                content_width,
                muted_fg,
            )?;
            if workspace_threads::space_note_vault(&self.active_space_id).is_none() {
                let button_height = self.ui_px(52);
                let gap = self.ui_px(10);
                let first_y = content_top + ui_metrics.cell_size.height as usize + self.ui_px(20);
                self.paint_snippet_button(
                    layers,
                    ui_font,
                    ui_metrics,
                    chrome,
                    foreground,
                    muted_fg,
                    content_x,
                    first_y,
                    content_width,
                    button_height,
                    Some(SvgIcon::FolderOpen),
                    "Choose Existing Vault…",
                    UIItemType::RightSidebarNoteChooseVault,
                    true,
                )?;
                self.paint_snippet_button(
                    layers,
                    ui_font,
                    ui_metrics,
                    chrome,
                    foreground,
                    muted_fg,
                    content_x,
                    first_y + button_height + gap,
                    content_width,
                    button_height,
                    Some(SvgIcon::FolderPlus),
                    "Create New Vault…",
                    UIItemType::RightSidebarNoteCreateVault,
                    true,
                )?;
            }
            // The expanded pane may be reserved while the note is still
            // loading (or failed); keep the collapse toggle reachable so the
            // pane is never blank and stuck.
            if let Some(pane_rect) = self.right_sidebar_note_pane_rect() {
                let inset = self.ui_px(SIDEBAR_INSET);
                let button_size = self.ui_px(NOTE_TOOLBAR_HEIGHT);
                self.paint_sidebar_text(
                    layers,
                    ui_font,
                    ui_metrics,
                    &message,
                    pane_rect.x + inset * 2,
                    pane_rect.y + inset * 2 + button_size + self.ui_px(NOTE_BODY_TOP_GAP),
                    pane_rect.width.saturating_sub(inset * 4),
                    muted_fg,
                )?;
                self.paint_snippet_icon_button(
                    layers,
                    chrome,
                    foreground,
                    muted_fg,
                    pane_rect
                        .x
                        .saturating_add(pane_rect.width)
                        .saturating_sub(inset * 2 + button_size),
                    pane_rect.y + inset * 2,
                    button_size,
                    SvgIcon::Shrink,
                    UIItemType::RightSidebarNotePaneToggle,
                )?;
            }
            return Ok(());
        }
        if let Some(pane_rect) = self.right_sidebar_note_pane_rect() {
            // Pane mode: the sidebar column holds the vault tree and the
            // editor paints into the expanded pane on its left.
            self.right_sidebar_note_wide_layout = true;
            self.paint_note_vault_tree(
                layers,
                ui_font,
                ui_metrics,
                chrome,
                foreground,
                muted_fg,
                content_x,
                content_top,
                content_width,
                content_bottom,
                false,
                false,
            )?;
            let inset = self.ui_px(SIDEBAR_INSET);
            return self.paint_note_editor_area(
                layers,
                ui_font,
                ui_metrics,
                chrome,
                foreground,
                muted_fg,
                pane_rect.x + inset * 2,
                pane_rect.y + inset * 2,
                pane_rect.width.saturating_sub(inset * 4),
                pane_rect.y.saturating_add(pane_rect.height),
                base_font_size,
                false,
                SvgIcon::FolderTree,
            );
        }
        let wide_layout = content_width >= self.ui_px(NOTE_VAULT_SPLIT_MIN_WIDTH);
        self.right_sidebar_note_wide_layout = wide_layout;
        let split = wide_layout && !self.right_sidebar_note_vault_tree_collapsed;
        if !wide_layout && self.right_sidebar_note_view == RightSidebarNoteView::Tree {
            self.paint_note_vault_tree(
                layers,
                ui_font,
                ui_metrics,
                chrome,
                foreground,
                muted_fg,
                content_x,
                content_top,
                content_width,
                content_bottom,
                true,
                true,
            )?;
            return Ok(());
        }
        let (content_x, content_width) = if split {
            let tree_width = self
                .ui_px(NOTE_VAULT_TREE_WIDTH)
                .min(content_width.saturating_sub(self.ui_px(300)));
            self.paint_note_vault_tree(
                layers,
                ui_font,
                ui_metrics,
                chrome,
                foreground,
                muted_fg,
                content_x,
                content_top,
                tree_width,
                content_bottom,
                false,
                true,
            )?;
            (
                content_x + tree_width + self.ui_px(SIDEBAR_INSET),
                content_width.saturating_sub(tree_width + self.ui_px(SIDEBAR_INSET)),
            )
        } else {
            (content_x, content_width)
        };
        let show_tree_button = !split;
        let tree_button_icon = if wide_layout {
            SvgIcon::PanelLeftOpen
        } else {
            SvgIcon::FolderTree
        };
        self.paint_note_editor_area(
            layers,
            ui_font,
            ui_metrics,
            chrome,
            foreground,
            muted_fg,
            content_x,
            content_top,
            content_width,
            content_bottom,
            base_font_size,
            show_tree_button,
            tree_button_icon,
        )
    }

    /// Paint the Note toolbar and editor body into an arbitrary content rect —
    /// either inline in the sidebar column or in the expanded Note pane.
    /// Routes all shaping inside (wrap measurement, prose, preedit, toolbar)
    /// through the Note domain cache.
    #[allow(clippy::too_many_arguments)]
    fn paint_note_editor_area(
        &mut self,
        layers: &mut TripleLayerQuadAllocator,
        ui_font: &Rc<LoadedFont>,
        ui_metrics: RenderMetrics,
        chrome: UiPalette,
        foreground: LinearRgba,
        muted_fg: LinearRgba,
        content_x: usize,
        content_top: usize,
        content_width: usize,
        content_bottom: usize,
        base_font_size: f64,
        show_tree_button: bool,
        tree_button_icon: SvgIcon,
    ) -> anyhow::Result<()> {
        let previous_domain = self
            .ui_text_domain
            .replace(crate::shapecache::UiTextDomain::Note);
        let result = self.paint_note_editor_area_impl(
            layers,
            ui_font,
            ui_metrics,
            chrome,
            foreground,
            muted_fg,
            content_x,
            content_top,
            content_width,
            content_bottom,
            base_font_size,
            show_tree_button,
            tree_button_icon,
        );
        self.ui_text_domain.set(previous_domain);
        result
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_note_editor_area_impl(
        &mut self,
        layers: &mut TripleLayerQuadAllocator,
        ui_font: &Rc<LoadedFont>,
        ui_metrics: RenderMetrics,
        chrome: UiPalette,
        foreground: LinearRgba,
        muted_fg: LinearRgba,
        content_x: usize,
        content_top: usize,
        content_width: usize,
        content_bottom: usize,
        base_font_size: f64,
        show_tree_button: bool,
        tree_button_icon: SvgIcon,
    ) -> anyhow::Result<()> {
        // Starts before refresh_projection so parse/projection/visual-build
        // costs (typing and open latency) are attributed to the frame, not
        // just the layout/paint tail.
        let mut note_profile = NotePaintProfile::new();
        let note_stats_before = note_profile.enabled.then(|| {
            self.ui_shape_caches
                .borrow()
                .domain(crate::shapecache::UiTextDomain::Note)
                .stats()
        });
        self.right_sidebar_note.refresh_projection();
        self.schedule_right_sidebar_note_parse();
        self.schedule_right_sidebar_note_spellcheck(Duration::from_millis(50));

        let toolbar_height = self.ui_px(NOTE_TOOLBAR_HEIGHT);
        let menu_size = toolbar_height;

        let body_y = content_top + toolbar_height + self.ui_px(NOTE_BODY_TOP_GAP);
        let body_bottom = content_bottom.saturating_sub(self.ui_px(SIDEBAR_INSET));
        let body_height = body_bottom.saturating_sub(body_y);
        if body_height == 0 || content_width == 0 {
            return Ok(());
        }
        let note_layout_stage = crate::input_diagnostics::StageTimer::begin("note_layout");
        self.ui_items.push(UIItem {
            x: content_x,
            y: body_y,
            width: content_width,
            height: body_height,
            item_type: UIItemType::RightSidebarNoteBody,
        });

        let settings = crate::native_settings::load();
        let normal_weight = crate::native_settings::settings_font_weight(&settings);
        let bold_font = self
            .fonts
            .title_font_with_size_and_weight(base_font_size, normal_weight.max(700))?;
        let italic_font = self
            .fonts
            .title_font_with_size_weight_and_italic(base_font_size, normal_weight)?;
        let bold_italic_font = self
            .fonts
            .title_font_with_size_weight_and_italic(base_font_size, normal_weight.max(700))?;
        let h1_font = self
            .fonts
            .title_font_with_size_and_weight(base_font_size + 7.0, normal_weight.max(760))?;
        let h2_font = self
            .fonts
            .title_font_with_size_and_weight(base_font_size + 4.0, normal_weight.max(730))?;
        let h3_font = self
            .fonts
            .title_font_with_size_and_weight(base_font_size + 2.0, normal_weight.max(700))?;
        let h1_italic_font = self
            .fonts
            .title_font_with_size_weight_and_italic(base_font_size + 7.0, normal_weight.max(760))?;
        let h2_italic_font = self
            .fonts
            .title_font_with_size_weight_and_italic(base_font_size + 4.0, normal_weight.max(730))?;
        let h3_italic_font = self
            .fonts
            .title_font_with_size_weight_and_italic(base_font_size + 2.0, normal_weight.max(700))?;
        let code_font = self
            .fonts
            .resolve_font(&self.config.font)
            .context("Note code font")?;
        let h1_metrics = RenderMetrics::with_font_metrics(&h1_font.metrics());
        let h2_metrics = RenderMetrics::with_font_metrics(&h2_font.metrics());
        let h3_metrics = RenderMetrics::with_font_metrics(&h3_font.metrics());
        let bold_metrics = RenderMetrics::with_font_metrics(&bold_font.metrics());
        let italic_metrics = RenderMetrics::with_font_metrics(&italic_font.metrics());
        let bold_italic_metrics = RenderMetrics::with_font_metrics(&bold_italic_font.metrics());
        let h1_italic_metrics = RenderMetrics::with_font_metrics(&h1_italic_font.metrics());
        let h2_italic_metrics = RenderMetrics::with_font_metrics(&h2_italic_font.metrics());
        let h3_italic_metrics = RenderMetrics::with_font_metrics(&h3_italic_font.metrics());
        let code_metrics = RenderMetrics::with_font_metrics(&code_font.metrics());
        let selection = self.right_sidebar_note.view.selection;
        let focused = self.right_sidebar_note.view.focused;
        let spelling_issues = (self.right_sidebar_note.spelling_revision
            == self.right_sidebar_note.projection_revision
            && self.right_sidebar_note.view.mode != EditorMode::ReadOnly)
            .then(|| Arc::clone(&self.right_sidebar_note.spelling_issues));
        let active_spelling_word = if focused && selection.is_caret() {
            self.right_sidebar_note
                .session
                .as_ref()
                .map(|session| session.lock().word_range_at(selection.focus.byte))
        } else {
            None
        };
        self.right_sidebar_note.view.preedit = if focused {
            match &self.dead_key_status {
                DeadKeyStatus::Composing(text) => Some(text.clone()),
                DeadKeyStatus::None => None,
            }
        } else {
            None
        };
        let preedit = self.right_sidebar_note.view.preedit.clone();
        let padding = self.ui_px(NOTE_BODY_PADDING) as f32;
        let available_reading_width = (content_width as f32 - padding * 2.0).max(1.0);
        let reading_width = available_reading_width
            .min(self.ui_f32(self.config.note_reading_max_width.max(320) as f32));
        let text_left = content_x as f32 + (content_width as f32 - reading_width) / 2.0;
        let clip_left = text_left;
        let clip_right = text_left + reading_width;
        let wrap_width = (clip_right - clip_left).max(1.0);
        let mut wrap_hasher = DefaultHasher::new();
        (wrap_width.floor() as usize).hash(&mut wrap_hasher);
        self.dimensions.dpi.hash(&mut wrap_hasher);
        // LoadedFont ids are allocation identities. Appearance changes rebuild
        // those objects even when typography is identical, which used to
        // throw away the whole Note wrap cache on every light/dark switch.
        // Hash only inputs that can actually change line breaks.
        base_font_size.to_bits().hash(&mut wrap_hasher);
        normal_weight.hash(&mut wrap_hasher);
        self.config.font.hash(&mut wrap_hasher);
        for metrics in [
            ui_metrics,
            bold_metrics,
            italic_metrics,
            bold_italic_metrics,
            h1_metrics,
            h2_metrics,
            h3_metrics,
            h1_italic_metrics,
            h2_italic_metrics,
            h3_italic_metrics,
            code_metrics,
        ] {
            metrics.cell_size.width.hash(&mut wrap_hasher);
            metrics.cell_size.height.hash(&mut wrap_hasher);
        }
        let wrap_key = wrap_hasher.finish() as usize;
        let visual = if let Some(visual) = self.right_sidebar_note.cached_wrapped_visual(wrap_key) {
            note_profile.wrap_source = "cached";
            visual
        } else if self.right_sidebar_note.should_background_wrap(wrap_key) {
            note_profile.wrap_source = "background-provisional";
            let normal_approx_metrics = self.note_approximate_text_metrics(ui_font, &ui_metrics)?;
            let h1_approx_metrics = self.note_approximate_text_metrics(&h1_font, &h1_metrics)?;
            let h2_approx_metrics = self.note_approximate_text_metrics(&h2_font, &h2_metrics)?;
            let h3_approx_metrics = self.note_approximate_text_metrics(&h3_font, &h3_metrics)?;
            let code_approx_metrics =
                self.note_approximate_text_metrics(&code_font, &code_metrics)?;
            let provisional = self.right_sidebar_note.provisional_wrapped_visual();
            if provisional.lines.is_empty() && !self.right_sidebar_note.visual.lines.is_empty() {
                // Freshly opened note with no wrapped content yet. The
                // background worker would run exactly this approximate wrap
                // (same algorithm, same inputs), so do it synchronously ONCE
                // and install it as the real wrapped result — content shows
                // on the first frame (like File Preview) and no duplicate
                // worker round-trip re-lays-out identical content. Only
                // documents ≤ the 64 KiB parse threshold reach here with a
                // full visual; larger ones carry the tiny progressive-preview
                // seed, and their full wrap goes to the worker below on a
                // later frame.
                let wrap_stage = crate::input_diagnostics::StageTimer::begin("note_wrap_sync");
                let mut wrap_cache = std::mem::take(&mut self.right_sidebar_note.wrap_cache);
                let wrapped = wrap_visual_document_by_width_cached(
                    &Arc::clone(&self.right_sidebar_note.visual),
                    wrap_width,
                    wrap_key,
                    &mut wrap_cache,
                    |block, _, text| {
                        let metrics = match block {
                            BlockKind::Heading(1) => h1_approx_metrics,
                            BlockKind::Heading(2) => h2_approx_metrics,
                            BlockKind::Heading(_) => h3_approx_metrics,
                            BlockKind::CodeBlock => code_approx_metrics,
                            _ => normal_approx_metrics,
                        };
                        Ok::<f32, anyhow::Error>(approximate_note_text_width(text, metrics))
                    },
                );
                self.right_sidebar_note.wrap_cache = wrap_cache;
                match wrapped {
                    Ok(wrapped) => {
                        wrap_stage.finish(true);
                        note_profile.wrap_source = "approx-sync";
                        self.right_sidebar_note
                            .cache_wrapped_visual(wrap_key, wrapped)
                    }
                    Err(_) => {
                        wrap_stage.finish(false);
                        provisional
                    }
                }
            } else {
                self.schedule_right_sidebar_note_wrap(
                    wrap_key,
                    wrap_width,
                    normal_approx_metrics,
                    h1_approx_metrics,
                    h2_approx_metrics,
                    h3_approx_metrics,
                    code_approx_metrics,
                );
                provisional
            }
        } else {
            note_profile.wrap_source = "sync";
            // UI-thread wrap: the number that decides whether real
            // incremental wrapping is ever needed.
            let wrap_stage = crate::input_diagnostics::StageTimer::begin("note_wrap_sync");
            let mut wrap_cache = std::mem::take(&mut self.right_sidebar_note.wrap_cache);
            let wrapped = wrap_visual_document_by_width_cached(
                &self.right_sidebar_note.visual,
                wrap_width,
                wrap_key,
                &mut wrap_cache,
                |block, style, text| {
                    let (font, metrics) = match block {
                        BlockKind::Heading(1) if style.emphasis => {
                            (&h1_italic_font, h1_italic_metrics)
                        }
                        BlockKind::Heading(2) if style.emphasis => {
                            (&h2_italic_font, h2_italic_metrics)
                        }
                        BlockKind::Heading(_) if style.emphasis => {
                            (&h3_italic_font, h3_italic_metrics)
                        }
                        BlockKind::Heading(1) => (&h1_font, h1_metrics),
                        BlockKind::Heading(2) => (&h2_font, h2_metrics),
                        BlockKind::Heading(_) => (&h3_font, h3_metrics),
                        BlockKind::CodeBlock => (&code_font, code_metrics),
                        _ if style.strong && style.emphasis => {
                            (&bold_italic_font, bold_italic_metrics)
                        }
                        _ if style.strong => (&bold_font, bold_metrics),
                        _ if style.emphasis => (&italic_font, italic_metrics),
                        _ => (ui_font, ui_metrics),
                    };
                    self.cached_ui_text_advance(font, &metrics, text)
                },
            );
            self.right_sidebar_note.wrap_cache = wrap_cache;
            let wrapped = wrapped?;
            wrap_stage.finish(true);
            self.right_sidebar_note
                .cache_wrapped_visual(wrap_key, wrapped)
        };
        if visual.lines.is_empty() && self.right_sidebar_note.prefers_background_wrap() {
            self.paint_sidebar_text(
                layers,
                ui_font,
                ui_metrics,
                "Laying out note…",
                content_x + self.ui_px(NOTE_BODY_PADDING),
                body_y + self.ui_px(NOTE_BODY_PADDING),
                content_width.saturating_sub(self.ui_px(NOTE_BODY_PADDING) * 2),
                muted_fg,
            )?;
            self.right_sidebar_note.line_layouts.clear();
            self.right_sidebar_note.code_block_layouts.clear();
            self.right_sidebar_note.viewport_height = body_height as f32;
            self.right_sidebar_note.content_height = body_height as f32;
            self.paint_note_editor_toolbar(
                layers,
                chrome,
                foreground,
                muted_fg,
                content_x,
                content_top,
                content_width,
                menu_size,
                show_tree_button,
                tree_button_icon,
            )?;
            note_layout_stage.finish(true);
            if let Some(before) = note_stats_before {
                let after = self
                    .ui_shape_caches
                    .borrow()
                    .domain(crate::shapecache::UiTextDomain::Note)
                    .stats();
                note_profile.capture_note_cache_delta(before, after);
            }
            note_profile.finish(self.right_sidebar_note.view.scroll_offset);
            return Ok(());
        }
        let table_horizontal_padding = self.ui_f32(NOTE_TABLE_CELL_HORIZONTAL_PADDING as f32);
        let table_vertical_padding = self.ui_f32(NOTE_TABLE_CELL_VERTICAL_PADDING as f32);
        let table_row_height = ui_metrics.cell_size.height as f32 + table_vertical_padding * 2.0;
        let use_dark_syntax_theme = matches!(
            crate::native_settings::effective_appearance(),
            window::Appearance::Dark | window::Appearance::DarkHighContrast
        );
        let code_horizontal_padding = self.ui_f32(NOTE_CODE_HORIZONTAL_PADDING as f32);
        let code_vertical_padding = self.ui_f32(NOTE_CODE_VERTICAL_PADDING as f32);
        let code_header_height = self.ui_f32(NOTE_CODE_HEADER_HEIGHT as f32);
        let code_line_height = code_metrics.cell_size.height as f32;
        let code_inner_width = (wrap_width - code_horizontal_padding * 2.0).max(1.0);
        let active_document = self.right_sidebar_note.document.clone();
        let mut component_hasher = DefaultHasher::new();
        wrap_key.hash(&mut component_hasher);
        (Arc::as_ptr(&visual) as usize).hash(&mut component_hasher);
        self.right_sidebar_note
            .projection_revision
            .hash(&mut component_hasher);
        self.config
            .note_remote_images_enabled
            .hash(&mut component_hasher);
        if let Some(document) = active_document.as_ref() {
            document.vault_root.hash(&mut component_hasher);
            document.relative_path.hash(&mut component_hasher);
        }
        // These are ordered maps/sets so each frame can hash them in place
        // without sorting a temporary copy.
        hash_ordered_iter(
            &mut component_hasher,
            self.right_sidebar_note.collapsed_code_blocks.iter(),
        );
        hash_ordered_iter(
            &mut component_hasher,
            self.right_sidebar_note
                .code_horizontal_offsets
                .iter()
                .map(|(source, offset)| (*source, offset.to_bits())),
        );
        hash_ordered_iter(
            &mut component_hasher,
            self.right_sidebar_note_table_horizontal_offsets
                .iter()
                .map(|(source, offset)| (*source, offset.to_bits())),
        );
        let component_key = component_hasher.finish();

        if self.right_sidebar_note_paint_cache.key != Some(component_key) {
            // Component geometry only needs heavyweight objects. Avoid a deep
            // clone of every inline emphasis/link/tag in a multi-megabyte
            // projection when the first full layout is published.
            let projected_objects = self
                .right_sidebar_note
                .projection
                .objects
                .iter()
                .filter(|object| {
                    matches!(
                        object,
                        ProjectedObject::Table(_)
                            | ProjectedObject::CodeBlock(_)
                            | ProjectedObject::Image { .. }
                            | ProjectedObject::WikiLink { embed: true, .. }
                    )
                })
                .cloned()
                .collect::<Vec<_>>();
            let current_code_starts = projected_objects
                .iter()
                .filter_map(|object| match object {
                    ProjectedObject::CodeBlock(code) => Some(code.source.start),
                    _ => None,
                })
                .collect::<HashSet<_>>();
            self.right_sidebar_note_code_highlight
                .retain_blocks(&current_code_starts);

            let mut table_rows = HashMap::new();
            let mut current_table_starts = HashSet::new();
            for object in &projected_objects {
                let ProjectedObject::Table(table) = object else {
                    continue;
                };
                let start = visual
                    .lines
                    .partition_point(|line| line.source.end < table.source.start);
                let end = visual
                    .lines
                    .partition_point(|line| line.source.start <= table.source.end);
                let row_indices = (start..end)
                    .filter(|index| {
                        let line = &visual.lines[*index];
                        line.block == BlockKind::Table
                            && line.runs.iter().any(|run| {
                                table.source.start <= run.source.start
                                    && run.source.end <= table.source.end
                            })
                    })
                    .collect::<Vec<_>>();
                let column_count = table.rows.iter().map(Vec::len).max().unwrap_or(0);
                if row_indices.is_empty() || column_count == 0 {
                    continue;
                }
                let mut desired_widths = vec![0.0f32; column_count];
                for (row_index, row) in table.rows.iter().enumerate() {
                    let (font, metrics) = if row_index == 0 {
                        (&bold_font, bold_metrics)
                    } else {
                        (ui_font, ui_metrics)
                    };
                    for (column_index, cell) in row.iter().enumerate() {
                        let width = self.cached_ui_text_advance(font, &metrics, &cell.text)?
                            + table_horizontal_padding * 2.0;
                        desired_widths[column_index] = desired_widths[column_index].max(width);
                    }
                }
                let column_widths = Rc::new(scrollable_note_table_columns(
                    &desired_widths,
                    wrap_width,
                    self.ui_f32(NOTE_TABLE_MIN_COLUMN_WIDTH as f32),
                    self.ui_f32(NOTE_TABLE_MAX_COLUMN_WIDTH as f32),
                ));
                let total_width = column_widths.iter().sum::<f32>();
                let max_horizontal_scroll = (total_width - wrap_width).max(0.0);
                current_table_starts.insert(table.source.start);
                let horizontal_offset = self
                    .right_sidebar_note_table_horizontal_offsets
                    .entry(table.source.start)
                    .or_default();
                *horizontal_offset = horizontal_offset.clamp(0.0, max_horizontal_scroll);
                let horizontal_offset = *horizontal_offset;
                let mut alignments = table.alignments.clone();
                alignments.resize(column_count, TableAlignment::None);
                let alignments = Rc::new(alignments);
                let row_count = row_indices.len();
                for (row_index, visual_index) in row_indices.into_iter().enumerate() {
                    table_rows.insert(
                        visual_index,
                        NoteTableRowPaintLayout {
                            table_start: table.source.start,
                            column_widths: Rc::clone(&column_widths),
                            alignments: Rc::clone(&alignments),
                            row_index,
                            row_count,
                            total_width,
                            horizontal_offset,
                            max_horizontal_scroll,
                        },
                    );
                }
            }
            self.right_sidebar_note_table_horizontal_offsets
                .retain(|source_start, _| current_table_starts.contains(source_start));

            let mut code_rows = HashMap::new();
            for object in &projected_objects {
                let ProjectedObject::CodeBlock(code) = object else {
                    continue;
                };
                let start = visual
                    .lines
                    .partition_point(|line| line.source.end < code.content.start);
                let end = visual
                    .lines
                    .partition_point(|line| line.source.start <= code.content.end);
                let row_indices = (start..end)
                    .filter(|index| {
                        let line = &visual.lines[*index];
                        line.block == BlockKind::CodeBlock
                            && code.content.start <= line.source.start
                            && line.source.end <= code.content.end
                    })
                    .collect::<Vec<_>>();
                if row_indices.is_empty() {
                    continue;
                }
                let collapsed = self
                    .right_sidebar_note
                    .collapsed_code_blocks
                    .contains(&code.source.start);
                let mut max_content_width = 0.0f32;
                for raw_line in code.text.split_inclusive('\n') {
                    let line = raw_line.strip_suffix('\n').unwrap_or(raw_line);
                    max_content_width = max_content_width.max(self.cached_ui_text_advance(
                        &code_font,
                        &code_metrics,
                        line,
                    )?);
                }
                let max_horizontal_scroll = (max_content_width - code_inner_width).max(0.0);
                let horizontal_offset = self
                    .right_sidebar_note
                    .code_horizontal_offsets
                    .entry(code.source.start)
                    .or_default();
                *horizontal_offset = horizontal_offset.clamp(0.0, max_horizontal_scroll);
                let horizontal_offset = *horizontal_offset;
                let row_count = row_indices.len();
                let block_height = code_header_height
                    + if collapsed {
                        0.0
                    } else {
                        code_vertical_padding * 2.0 + code_line_height * row_count as f32
                    };
                let language = code
                    .language
                    .as_deref()
                    .unwrap_or("code")
                    .to_ascii_uppercase();
                let code = Arc::new(code.clone());
                for (row_index, visual_index) in row_indices.into_iter().enumerate() {
                    code_rows.insert(
                        visual_index,
                        NoteCodeRowPaintLayout {
                            block_start: code.source.start,
                            language: language.clone(),
                            code: Arc::clone(&code),
                            row_index,
                            row_count,
                            collapsed,
                            horizontal_offset,
                            block_height,
                            max_horizontal_scroll,
                        },
                    );
                }
            }

            let mut image_sources = HashMap::new();
            if let Some(document) = active_document.as_ref() {
                for object in &projected_objects {
                    let resolved = match object {
                        ProjectedObject::Image { source, target, .. } => {
                            if self.config.note_remote_images_enabled
                                && (target.starts_with("https://") || target.starts_with("http://"))
                            {
                                Some((
                                    source.start,
                                    RightSidebarNoteImageSource::Remote(target.clone()),
                                ))
                            } else {
                                resolve_local_image(
                                    &document.vault_root,
                                    &document.document_path,
                                    target,
                                )
                                .ok()
                                .map(|path| {
                                    (source.start, RightSidebarNoteImageSource::Local(path))
                                })
                            }
                        }
                        ProjectedObject::WikiLink {
                            source,
                            embed: true,
                            resolved_path: Some(target),
                            ..
                        } => {
                            let candidate = document.vault_root.join(target).canonicalize().ok();
                            candidate
                                .filter(|candidate| candidate.starts_with(&document.vault_root))
                                .map(|path| {
                                    (source.start, RightSidebarNoteImageSource::Local(path))
                                })
                        }
                        _ => None,
                    };
                    if let Some((source_start, source)) = resolved {
                        image_sources.insert(source_start, source);
                    }
                }
            }
            self.right_sidebar_note_paint_cache = NotePaintCache {
                key: Some(component_key),
                table_rows: Arc::new(table_rows),
                code_rows: Arc::new(code_rows),
                image_sources: Arc::new(image_sources),
            };
        }
        let table_rows = Arc::clone(&self.right_sidebar_note_paint_cache.table_rows);
        let code_rows = Arc::clone(&self.right_sidebar_note_paint_cache.code_rows);
        let note_image_paths = Arc::clone(&self.right_sidebar_note_paint_cache.image_sources);
        let viewport_top = body_y as f32;
        let viewport_bottom = body_bottom as f32;
        let image_max_height = (body_height as f32 * 0.72)
            .max(ui_metrics.cell_size.height as f32)
            .min(self.ui_f32(720.0));
        let line_gap = self.ui_px(NOTE_LINE_GAP) as f32;
        let mut geometry_hasher = DefaultHasher::new();
        wrap_key.hash(&mut geometry_hasher);
        self.right_sidebar_note
            .visual_key
            .hash(&mut geometry_hasher);
        table_row_height.to_bits().hash(&mut geometry_hasher);
        code_line_height.to_bits().hash(&mut geometry_hasher);
        code_header_height.to_bits().hash(&mut geometry_hasher);
        code_vertical_padding.to_bits().hash(&mut geometry_hasher);
        image_max_height.to_bits().hash(&mut geometry_hasher);
        let mut loaded_image_metrics = note_image_paths
            .iter()
            .filter_map(|(source_start, path)| {
                self.right_sidebar_note_images
                    .get(path)
                    .map(|image| (*source_start, image.width, image.height))
            })
            .collect::<Vec<_>>();
        loaded_image_metrics.sort_unstable();
        loaded_image_metrics.hash(&mut geometry_hasher);
        line_gap.to_bits().hash(&mut geometry_hasher);
        hash_ordered_iter(
            &mut geometry_hasher,
            self.right_sidebar_note.collapsed_code_blocks.iter(),
        );
        let geometry_key = geometry_hasher.finish();
        if self.right_sidebar_note.line_geometry_key != Some(geometry_key)
            || self.right_sidebar_note.line_geometry.len() != visual.lines.len()
        {
            let mut top = padding;
            // Reuse the previous geometry allocation when this host holds the
            // only reference (paint-local clones are dropped each frame).
            let mut geometry =
                Arc::try_unwrap(std::mem::take(&mut self.right_sidebar_note.line_geometry))
                    .unwrap_or_default();
            geometry.clear();
            geometry.reserve(visual.lines.len());
            for (line_index, line) in visual.lines.iter().enumerate() {
                let metrics = match line.block {
                    BlockKind::Heading(1) => h1_metrics,
                    BlockKind::Heading(2) => h2_metrics,
                    BlockKind::Heading(_) => h3_metrics,
                    BlockKind::CodeBlock => code_metrics,
                    _ => ui_metrics,
                };
                let mut height = metrics.cell_size.height as f32;
                if line.kind == VisualLineKind::Image {
                    let image = note_image_paths
                        .get(&line.source.start)
                        .and_then(|path| self.right_sidebar_note_images.get(path));
                    height = note_image_row_height(image, height, wrap_width, image_max_height);
                }
                if matches!(
                    line.block,
                    BlockKind::Properties | BlockKind::Callout | BlockKind::Embed
                ) {
                    height += self.ui_f32(8.0);
                }
                if table_rows.contains_key(&line_index) {
                    height = table_row_height;
                }
                if let Some(code_row) = code_rows.get(&line_index) {
                    height = note_code_row_height(
                        code_row.row_index,
                        code_row.row_count,
                        code_row.collapsed,
                        code_line_height,
                        code_header_height,
                        code_vertical_padding,
                    );
                }
                let gap = if let Some(code_row) = code_rows.get(&line_index) {
                    if code_row.row_index + 1 < code_row.row_count {
                        0.0
                    } else {
                        line_gap
                    }
                } else {
                    let grouped_card_continues =
                        visual
                            .lines
                            .get(line_index + 1)
                            .is_some_and(|next| match line.block {
                                BlockKind::Properties | BlockKind::Callout => {
                                    next.block == line.block
                                }
                                BlockKind::Embed => {
                                    next.block == line.block && next.source == line.source
                                }
                                _ => false,
                            });
                    if grouped_card_continues {
                        0.0
                    } else {
                        table_rows
                            .get(&line_index)
                            .filter(|row| row.row_index + 1 < row.row_count)
                            .map(|_| 0.0)
                            .unwrap_or(line_gap)
                    }
                };
                geometry.push(NoteLineGeometry { top, height, gap });
                top += height + gap;
            }
            self.right_sidebar_note.line_geometry_key = Some(geometry_key);
            self.right_sidebar_note.line_geometry = Arc::new(geometry);
        }
        let line_geometry = Arc::clone(&self.right_sidebar_note.line_geometry);
        let measured_content_height = line_geometry
            .last()
            .map(|line| line.top + line.height + line.gap + padding)
            .unwrap_or(padding * 2.0);
        let max_scroll = (measured_content_height - body_height as f32).max(0.0);
        let mut scroll = self
            .right_sidebar_note
            .view
            .scroll_offset
            .clamp(0.0, max_scroll);
        if focused && self.right_sidebar_note.reveal_caret {
            let caret_byte = selection.focus.byte;
            let candidate_start = visual
                .lines
                .partition_point(|line| line.source.end < caret_byte);
            let candidate_end = visual
                .lines
                .partition_point(|line| line.source.start <= caret_byte);
            let caret_row = (candidate_start..candidate_end)
                .find(|index| {
                    let line = &visual.lines[*index];
                    line.source.start <= caret_byte && caret_byte <= line.source.end
                })
                .and_then(|index| line_geometry.get(index));
            if let Some(caret_row) = caret_row {
                let caret_top = caret_row.top;
                let caret_height = caret_row.height;
                let safe_top = scroll + padding;
                let safe_bottom = scroll + body_height as f32 - padding;
                if caret_top < safe_top {
                    scroll = (caret_top - padding).max(0.0);
                } else if caret_top + caret_height > safe_bottom {
                    scroll =
                        (caret_top + caret_height + padding - body_height as f32).min(max_scroll);
                }
            }
            self.right_sidebar_note.reveal_caret = false;
        }
        self.right_sidebar_note.view.scroll_offset = scroll;
        // Reuse last frame's allocations; scroll repaints refill these every
        // frame and the capacities are stable.
        let mut layouts = std::mem::take(&mut self.right_sidebar_note.line_layouts);
        layouts.clear();
        let mut code_block_layouts =
            std::mem::take(&mut self.right_sidebar_note.code_block_layouts);
        code_block_layouts.clear();
        let mut table_layouts = std::mem::take(&mut self.right_sidebar_note_table_layouts);
        table_layouts.clear();
        let mut caret_rect: Option<(f32, f32, f32)> = None;
        let selected = selection.range();

        note_profile.total_lines = visual.lines.len();
        let paint_range = virtual_note_line_range(&line_geometry, scroll, body_height as f32);
        let paint_start = paint_range.start;
        for line_index in paint_range {
            note_profile.painted_lines += 1;
            let line = &visual.lines[line_index];
            let geometry = line_geometry[line_index];
            let table_row = table_rows.get(&line_index);
            let code_row = code_rows.get(&line_index);
            let (line_font, line_metrics) = match line.block {
                BlockKind::Heading(1) => (&h1_font, h1_metrics),
                BlockKind::Heading(2) => (&h2_font, h2_metrics),
                BlockKind::Heading(_) => (&h3_font, h3_metrics),
                BlockKind::CodeBlock => (&code_font, code_metrics),
                _ => (ui_font, ui_metrics),
            };
            let line_height = geometry.height;
            let line_y = body_y as f32 + geometry.top - scroll;
            let line_bottom = line_y + line_height;
            let visible = line_bottom >= viewport_top && line_y <= viewport_bottom;
            let code_text_y = code_row.map(|row| {
                if row.row_index == 0 {
                    line_y + code_header_height + code_vertical_padding
                } else {
                    line_y
                }
            });
            let card_horizontal_padding = self.ui_f32(10.0);
            let card_vertical_padding = self.ui_f32(4.0);
            let continuation_indent = line.continuation_indent();
            let mut x = if let Some(table_row) = table_row {
                text_left - table_row.horizontal_offset
            } else if let Some(code_row) = code_row {
                text_left + code_horizontal_padding - code_row.horizontal_offset
            } else if matches!(
                line.block,
                BlockKind::Properties | BlockKind::Callout | BlockKind::Embed
            ) {
                text_left + card_horizontal_padding + continuation_indent
            } else {
                text_left + continuation_indent
            };
            let mut run_layouts = Vec::new();

            let starts_painted_code_block = code_row.is_some_and(|row| {
                row.row_index == 0
                    || line_index == paint_start
                    || code_rows
                        .get(&line_index.saturating_sub(1))
                        .is_none_or(|previous| previous.block_start != row.block_start)
            });
            if let Some(code_row) = code_row.filter(|_| starts_painted_code_block) {
                let block_top = line_y
                    - if code_row.row_index == 0 {
                        0.0
                    } else {
                        code_header_height
                            + code_vertical_padding
                            + code_line_height * code_row.row_index as f32
                    };
                let block_bottom = block_top + code_row.block_height;
                let draw_top = block_top.max(viewport_top);
                let draw_bottom = block_bottom.min(viewport_bottom);
                if draw_bottom > draw_top {
                    let block_rect =
                        euclid::rect(text_left, draw_top, wrap_width, draw_bottom - draw_top);
                    let (round_top, round_bottom) = visible_code_block_rounded_edges(
                        block_top,
                        block_bottom,
                        viewport_top,
                        viewport_bottom,
                    );
                    self.fill_vertically_rounded_rectangle(
                        layers,
                        1,
                        block_rect,
                        chrome.sidebar_button_bg,
                        self.ui_f32(NOTE_CODE_BLOCK_RADIUS),
                        round_top,
                        round_bottom,
                    )?;
                }
                let content_y = block_top + code_header_height;
                let content_height = (code_row.block_height - code_header_height).max(0.0);
                code_block_layouts.push(NoteCodeBlockLayout {
                    source_start: code_row.block_start,
                    x: text_left,
                    y: content_y,
                    width: wrap_width,
                    height: content_height,
                    max_horizontal_scroll: code_row.max_horizontal_scroll,
                });
                if block_top >= viewport_top
                    && block_top + code_header_height <= viewport_bottom
                    && code_header_height >= 1.0
                {
                    let header_y = block_top.max(0.0) as usize;
                    let header_height = code_header_height as usize;
                    self.ui_items.push(UIItem {
                        x: text_left as usize,
                        y: header_y,
                        width: wrap_width as usize,
                        height: header_height,
                        item_type: UIItemType::RightSidebarNoteCodeToggle(code_row.block_start),
                    });
                    let control_size = self.ui_px(NOTE_CODE_CONTROL_SIZE);
                    let control_y = header_y + header_height.saturating_sub(control_size) / 2;
                    let leading_x = text_left as usize + self.ui_px(4);
                    self.paint_snippet_icon_button(
                        layers,
                        chrome,
                        foreground,
                        muted_fg,
                        leading_x,
                        control_y,
                        control_size,
                        if code_row.collapsed {
                            SvgIcon::ChevronRight
                        } else {
                            SvgIcon::ChevronDown
                        },
                        UIItemType::RightSidebarNoteCodeToggle(code_row.block_start),
                    )?;
                    let copy_x = clip_right.max(text_left) as usize - control_size - self.ui_px(4);
                    self.paint_snippet_icon_button(
                        layers,
                        chrome,
                        foreground,
                        muted_fg,
                        copy_x,
                        control_y,
                        control_size,
                        SvgIcon::Copy,
                        UIItemType::RightSidebarNoteCodeCopy(code_row.block_start),
                    )?;
                    let language_x = leading_x + control_size + self.ui_px(4);
                    self.paint_sidebar_text(
                        layers,
                        ui_font,
                        ui_metrics,
                        &code_row.language,
                        language_x,
                        header_y
                            + header_height.saturating_sub(ui_metrics.cell_size.height as usize)
                                / 2,
                        copy_x.saturating_sub(language_x + self.ui_px(4)),
                        muted_fg,
                    )?;
                    self.filled_rectangle(
                        layers,
                        1,
                        euclid::rect(
                            text_left,
                            block_top + code_header_height - 1.0,
                            wrap_width,
                            1.0,
                        ),
                        chrome.separator.mul_alpha(0.72),
                    )?;
                }
            }

            if line_height <= 0.0 {
                continue;
            }

            let note_image_source = if visible && line.kind == VisualLineKind::Image {
                note_image_paths.get(&line.source.start).cloned()
            } else {
                None
            };
            let note_image = note_image_source
                .as_ref()
                .and_then(|source| self.right_sidebar_note_images.get(source))
                .cloned();
            if note_image.is_none() {
                if let Some(source) = note_image_source.clone() {
                    self.schedule_right_sidebar_note_image(source);
                }
            }

            if visible {
                if matches!(
                    line.block,
                    BlockKind::Properties | BlockKind::Callout | BlockKind::Embed
                ) {
                    let belongs_to_same_card = |other_block: BlockKind, same_source: bool| {
                        other_block == line.block
                            && (matches!(line.block, BlockKind::Properties | BlockKind::Callout)
                                || same_source)
                    };
                    let same_as_previous = line_index > 0
                        && belongs_to_same_card(
                            visual.lines[line_index - 1].block,
                            visual.lines[line_index - 1].source == line.source,
                        );
                    let same_as_next = visual.lines.get(line_index + 1).is_some_and(|next| {
                        belongs_to_same_card(next.block, next.source == line.source)
                    });
                    let background = match line.block {
                        BlockKind::Properties => chrome.control_bg.mul_alpha(0.72),
                        BlockKind::Callout => chrome.selected_bg.mul_alpha(0.20),
                        BlockKind::Embed => chrome.sidebar_button_bg,
                        _ => unreachable!(),
                    };
                    self.fill_vertically_rounded_rectangle(
                        layers,
                        1,
                        euclid::rect(text_left, line_y, wrap_width, line_height),
                        background,
                        self.ui_f32(SIDEBAR_ROW_RADIUS),
                        !same_as_previous,
                        !same_as_next,
                    )?;
                    if matches!(line.block, BlockKind::Callout | BlockKind::Embed) {
                        self.filled_rectangle(
                            layers,
                            1,
                            euclid::rect(text_left, line_y, self.ui_f32(3.0).max(1.0), line_height),
                            chrome.secondary_text,
                        )?;
                    }
                }
                if let Some(table_row) = table_row {
                    table_layouts.push(RightSidebarNoteTableLayout {
                        source_start: table_row.table_start,
                        x: text_left,
                        y: line_y.max(viewport_top),
                        width: wrap_width,
                        height: line_bottom.min(viewport_bottom) - line_y.max(viewport_top),
                        max_horizontal_scroll: table_row.max_horizontal_scroll,
                    });
                    if table_row.row_index == 0 {
                        self.filled_rectangle(
                            layers,
                            1,
                            euclid::rect(text_left, line_y, wrap_width, line_height),
                            chrome.sidebar_row_active_bg,
                        )?;
                    }
                    self.filled_rectangle(
                        layers,
                        1,
                        euclid::rect(text_left, line_y, wrap_width, 1.0),
                        chrome.separator,
                    )?;
                    if table_row.row_index + 1 == table_row.row_count {
                        self.filled_rectangle(
                            layers,
                            1,
                            euclid::rect(text_left, line_y + line_height - 1.0, wrap_width, 1.0),
                            chrome.separator,
                        )?;
                    }
                    let mut boundary_x = text_left - table_row.horizontal_offset;
                    if boundary_x >= clip_left && boundary_x <= clip_right {
                        self.filled_rectangle(
                            layers,
                            1,
                            euclid::rect(boundary_x, line_y, 1.0, line_height),
                            chrome.separator,
                        )?;
                    }
                    for column_width in table_row.column_widths.iter() {
                        boundary_x += *column_width;
                        if boundary_x >= clip_left && boundary_x <= clip_right {
                            self.filled_rectangle(
                                layers,
                                1,
                                euclid::rect(boundary_x - 1.0, line_y, 1.0, line_height),
                                chrome.separator,
                            )?;
                        }
                    }
                    if table_row.row_index + 1 == table_row.row_count
                        && table_row.max_horizontal_scroll > 0.0
                    {
                        let track_height = self.ui_f32(NOTE_TABLE_SCROLLBAR_HEIGHT as f32);
                        let thumb_width = (wrap_width * wrap_width / table_row.total_width)
                            .max(self.ui_f32(24.0))
                            .min(wrap_width);
                        let thumb_x = text_left
                            + (wrap_width - thumb_width)
                                * (table_row.horizontal_offset
                                    / table_row.max_horizontal_scroll.max(1.0));
                        self.filled_rectangle(
                            layers,
                            2,
                            euclid::rect(
                                text_left,
                                line_y + line_height - track_height,
                                wrap_width,
                                track_height,
                            ),
                            chrome.separator.mul_alpha(0.46),
                        )?;
                        self.filled_rectangle(
                            layers,
                            2,
                            euclid::rect(
                                thumb_x,
                                line_y + line_height - track_height,
                                thumb_width,
                                track_height,
                            ),
                            chrome.scrollbar_thumb,
                        )?;
                    }
                }
                match line.kind {
                    VisualLineKind::Rule => {
                        self.filled_rectangle(
                            layers,
                            1,
                            euclid::rect(
                                text_left,
                                line_y + line_height / 2.0,
                                (clip_right - text_left).max(0.0),
                                1.0,
                            ),
                            chrome.separator,
                        )?;
                    }
                    VisualLineKind::TableHeader => {}
                    VisualLineKind::Image => {
                        if let Some(image) = note_image.as_ref() {
                            self.paint_right_sidebar_note_image(
                                layers,
                                image,
                                text_left,
                                line_y,
                                (clip_right - text_left).max(0.0),
                                line_height,
                                viewport_top,
                                viewport_bottom,
                            )?;
                        }
                    }
                    _ => {}
                }
                if line.block == BlockKind::Quote {
                    self.filled_rectangle(
                        layers,
                        1,
                        euclid::rect(
                            text_left - self.ui_f32(8.0),
                            line_y,
                            self.ui_f32(3.0),
                            line_height,
                        ),
                        muted_fg,
                    )?;
                }
            }

            if !visible {
                continue;
            }
            note_profile.visible_lines += 1;

            for (run_index, run) in line.runs.iter().enumerate() {
                let (font, metrics) = match line.block {
                    BlockKind::Heading(1) if run.style.emphasis => {
                        (&h1_italic_font, h1_italic_metrics)
                    }
                    BlockKind::Heading(2) if run.style.emphasis => {
                        (&h2_italic_font, h2_italic_metrics)
                    }
                    BlockKind::Heading(_) if run.style.emphasis => {
                        (&h3_italic_font, h3_italic_metrics)
                    }
                    BlockKind::Heading(_) => (line_font, line_metrics),
                    _ if run.style.strong && run.style.emphasis => {
                        (&bold_italic_font, bold_italic_metrics)
                    }
                    _ if run.style.strong => (&bold_font, bold_metrics),
                    _ if run.style.emphasis => (&italic_font, italic_metrics),
                    _ => (line_font, line_metrics),
                };
                let (shaped, _) = self.cached_ui_shape(font, &metrics, &run.text)?;
                note_profile.painted_runs += 1;
                let advance = shaped
                    .iter()
                    .map(|info| info.glyph.x_advance.get() as f32)
                    .sum::<f32>();
                let (run_x, run_y, run_clip_left, run_clip_right, hit_x, hit_width) =
                    if let Some(table_row) = table_row {
                        let cell_width = table_row
                            .column_widths
                            .get(run_index)
                            .copied()
                            .unwrap_or(0.0);
                        let cell_left = x;
                        let cell_right = cell_left + cell_width;
                        let cell_clip_left = (cell_left + table_horizontal_padding)
                            .max(clip_left)
                            .min(clip_right);
                        let cell_clip_right = (cell_right - table_horizontal_padding)
                            .max(cell_clip_left)
                            .min(clip_right);
                        let available = (cell_clip_right - cell_clip_left).max(0.0);
                        let alignment = table_row
                            .alignments
                            .get(run_index)
                            .copied()
                            .unwrap_or(TableAlignment::None);
                        let aligned_x = match alignment {
                            TableAlignment::Center if advance < available => {
                                cell_clip_left + (available - advance) / 2.0
                            }
                            TableAlignment::Right if advance < available => {
                                cell_clip_right - advance
                            }
                            TableAlignment::None
                            | TableAlignment::Left
                            | TableAlignment::Center
                            | TableAlignment::Right => cell_clip_left,
                        };
                        (
                            aligned_x,
                            line_y + table_vertical_padding,
                            cell_clip_left,
                            cell_clip_right,
                            cell_left,
                            cell_width,
                        )
                    } else if code_row.is_some() {
                        let content_left = text_left + code_horizontal_padding;
                        let content_right =
                            (clip_right - code_horizontal_padding).max(content_left);
                        (
                            x,
                            code_text_y.unwrap_or(line_y),
                            content_left,
                            content_right,
                            content_left,
                            (content_right - content_left).max(0.0),
                        )
                    } else if matches!(
                        line.block,
                        BlockKind::Properties | BlockKind::Callout | BlockKind::Embed
                    ) {
                        (
                            x,
                            line_y + card_vertical_padding,
                            clip_left + card_horizontal_padding,
                            clip_right - card_horizontal_padding,
                            x,
                            advance,
                        )
                    } else {
                        (x, line_y, clip_left, clip_right, x, advance)
                    };
                let run_height = metrics.cell_size.height as f32;
                let mut boundaries = Vec::with_capacity(shaped.len() + 2);
                if run.atomic {
                    boundaries.push((run_x, run.source.start));
                } else {
                    let mut boundary_advance = 0.0f32;
                    for info in shaped.iter() {
                        boundaries.push((
                            run_x + boundary_advance,
                            (run.source.start + info.cluster).min(run.source.end),
                        ));
                        boundary_advance += info.glyph.x_advance.get() as f32;
                    }
                }
                boundaries.push((run_x + advance, run.source.end));

                if selected.start < selected.end {
                    let start = selected.start.max(run.source.start);
                    let end = selected.end.min(run.source.end);
                    if start < end
                        || (run.atomic
                            && selected.start <= run.source.start
                            && selected.end >= run.source.end)
                    {
                        let left = note_boundary_x(&boundaries, start).max(run_clip_left);
                        let right = note_boundary_x(&boundaries, end).min(run_clip_right);
                        if right > left {
                            self.filled_rectangle(
                                layers,
                                1,
                                euclid::rect(left, run_y, right - left, run_height),
                                chrome.selected_bg.mul_alpha(0.72),
                            )?;
                        }
                    }
                }
                if run.style.code && line.block != BlockKind::CodeBlock && advance > 0.0 {
                    self.filled_rectangle(
                        layers,
                        1,
                        euclid::rect(
                            run_x,
                            run_y,
                            advance.min((run_clip_right - run_x).max(0.0)),
                            run_height,
                        ),
                        chrome.sidebar_button_bg,
                    )?;
                }
                if !(line.kind == VisualLineKind::Image && note_image.is_some()) {
                    let color = if run.style.link {
                        chrome.secondary_text
                    } else {
                        foreground
                    };
                    let code_colors = code_row.map(|row| {
                        (
                            row.row_index,
                            self.cached_note_code_highlights(&row.code, use_dark_syntax_theme),
                        )
                    });
                    self.paint_cached_ui_shape_pixel_clipped(
                        layers,
                        &metrics,
                        &shaped,
                        run_x,
                        run_y,
                        run_clip_left,
                        run_clip_right,
                        |info| {
                            code_colors
                                .as_ref()
                                .and_then(|(row_index, lines)| lines.get(*row_index))
                                .and_then(|colors| colors.get(info.cluster))
                                .copied()
                                .unwrap_or(color)
                        },
                    )?;
                    if run.style.strikethrough && advance > 0.0 {
                        self.filled_rectangle(
                            layers,
                            2,
                            euclid::rect(
                                run_x.max(run_clip_left),
                                run_y + run_height * 0.52,
                                advance.min((run_clip_right - run_x).max(0.0)),
                                1.0,
                            ),
                            color,
                        )?;
                    }
                    if !run.atomic {
                        if let Some(issues) = spelling_issues.as_ref() {
                            for issue in issues.iter().filter(|issue| {
                                issue.source.start < run.source.end
                                    && run.source.start < issue.source.end
                                    && !active_spelling_word.as_ref().is_some_and(|active| {
                                        active.start < issue.source.end
                                            && issue.source.start < active.end
                                    })
                            }) {
                                let source_start = issue.source.start.max(run.source.start);
                                let source_end = issue.source.end.min(run.source.end);
                                let underline_start =
                                    note_boundary_x(&boundaries, source_start).max(run_clip_left);
                                let underline_end =
                                    note_boundary_x(&boundaries, source_end).min(run_clip_right);
                                let segment_width = self.ui_f32(1.5).max(1.0);
                                let segment_gap = self.ui_f32(1.0).max(1.0);
                                let mut underline_x = underline_start;
                                let mut raised = false;
                                while underline_x < underline_end {
                                    let width =
                                        segment_width.min((underline_end - underline_x).max(0.0));
                                    if width <= 0.0 {
                                        break;
                                    }
                                    self.filled_rectangle(
                                        layers,
                                        2,
                                        euclid::rect(
                                            underline_x,
                                            run_y + run_height
                                                - self.ui_f32(if raised { 1.0 } else { 2.0 }),
                                            width,
                                            self.ui_f32(1.0).max(1.0),
                                        ),
                                        chrome.spelling_error,
                                    )?;
                                    raised = !raised;
                                    underline_x += segment_width + segment_gap;
                                }
                            }
                        }
                    }
                }
                if focused
                    && selection.is_caret()
                    && selection.focus.byte >= run.source.start
                    && selection.focus.byte <= run.source.end
                {
                    caret_rect = Some((
                        note_boundary_x(&boundaries, selection.focus.byte),
                        run_y,
                        run_height,
                    ));
                }
                run_layouts.push(NoteRunLayout {
                    source: run.source.clone(),
                    x: run_x,
                    width: advance,
                    hit_x,
                    hit_width,
                    boundaries,
                    atomic: run.atomic,
                });
                x = if let Some(table_row) = table_row {
                    x + table_row
                        .column_widths
                        .get(run_index)
                        .copied()
                        .unwrap_or(0.0)
                } else {
                    run_x + advance
                };
                if table_row.is_none() && x >= clip_right {
                    break;
                }
            }
            if focused
                && selection.is_caret()
                && line.runs.is_empty()
                && line.source.start == selection.focus.byte
            {
                caret_rect = Some((
                    if code_row.is_some() {
                        text_left + code_horizontal_padding
                    } else {
                        text_left + continuation_indent
                    },
                    code_text_y.unwrap_or(line_y),
                    if code_row.is_some() {
                        code_line_height
                    } else {
                        line_height
                    },
                ));
            } else if focused
                && selection.is_caret()
                && selection.focus.byte == line.source.end
                && caret_rect.is_none()
            {
                caret_rect = Some((x, line_y, line_height));
            }
            layouts.push(NoteLineLayout {
                source: line.source.clone(),
                y: code_text_y.unwrap_or(line_y),
                height: if code_row.is_some() {
                    code_line_height
                } else {
                    line_height
                },
                runs: run_layouts,
            });
        }

        self.right_sidebar_note.line_layouts = layouts;
        self.right_sidebar_note.code_block_layouts = code_block_layouts;
        self.right_sidebar_note_table_layouts = table_layouts;
        self.right_sidebar_note.viewport_height = body_height as f32;
        self.right_sidebar_note.content_height = measured_content_height;
        self.right_sidebar_note.clamp_scroll();
        self.sync_right_sidebar_note_native_text_input_snapshot(wrap_key);

        if let Some((caret_x, caret_y, caret_height)) = caret_rect {
            if caret_y + caret_height >= viewport_top && caret_y <= viewport_bottom {
                if self.right_sidebar_snippet_cursor_on() {
                    self.filled_rectangle(
                        layers,
                        2,
                        euclid::rect(
                            caret_x.clamp(clip_left, clip_right),
                            caret_y,
                            self.ui_f32(NOTE_CARET_WIDTH).max(1.0),
                            caret_height,
                        ),
                        foreground,
                    )?;
                }
                if let Some(composing) = preedit.as_ref() {
                    self.paint_ui_title_text_cached(
                        layers,
                        ui_font,
                        &ui_metrics,
                        composing,
                        caret_x.max(clip_left) as usize,
                        caret_y.max(viewport_top) as usize,
                        (clip_right - caret_x).max(0.0) as usize,
                        foreground,
                    )?;
                    let preedit_width = self
                        .cached_ui_text_advance(ui_font, &ui_metrics, composing)?
                        .min((clip_right - caret_x).max(0.0));
                    if preedit_width > 0.0 {
                        self.filled_rectangle(
                            layers,
                            2,
                            euclid::rect(
                                caret_x.max(clip_left),
                                caret_y + caret_height - 1.0,
                                preedit_width,
                                1.0,
                            ),
                            foreground,
                        )?;
                    }
                }
                if let Some(window) = self.window.as_ref() {
                    window.set_text_cursor_position(Rect::new(
                        Point::new(caret_x as isize, caret_y as isize),
                        ui_metrics.cell_size,
                    ));
                }
            }
        }

        self.paint_right_sidebar_file_mask(
            layers,
            chrome,
            content_x,
            content_top,
            content_width,
            body_y.saturating_sub(content_top),
        )?;
        self.paint_right_sidebar_file_mask(
            layers,
            chrome,
            content_x,
            body_bottom,
            content_width,
            content_bottom.saturating_sub(body_bottom),
        )?;
        if self.right_sidebar_note.view.scroll_offset > 0.0 {
            self.paint_right_sidebar_note_top_fade(
                layers,
                chrome,
                content_x,
                body_y,
                content_width,
                self.ui_px(FILE_SCROLL_FADE_HEIGHT).min(body_height),
            )?;
        }
        self.paint_note_editor_toolbar(
            layers,
            chrome,
            foreground,
            muted_fg,
            content_x,
            content_top,
            content_width,
            menu_size,
            show_tree_button,
            tree_button_icon,
        )?;

        if self.right_sidebar_note.content_height > self.right_sidebar_note.viewport_height {
            let track_height = body_height as f32;
            let thumb_height = (track_height * self.right_sidebar_note.viewport_height
                / self.right_sidebar_note.content_height)
                .max(self.ui_f32(24.0));
            let max_scroll = (self.right_sidebar_note.content_height
                - self.right_sidebar_note.viewport_height)
                .max(1.0);
            let thumb_y = body_y as f32
                + (track_height - thumb_height)
                    * (self.right_sidebar_note.view.scroll_offset / max_scroll);
            self.filled_rectangle(
                layers,
                2,
                euclid::rect(
                    (content_x + content_width).saturating_sub(self.ui_px(4)) as f32,
                    thumb_y,
                    self.ui_f32(3.0),
                    thumb_height,
                ),
                chrome.scrollbar_thumb,
            )?;
        }
        note_layout_stage.finish(true);
        if let Some(before) = note_stats_before {
            let after = self
                .ui_shape_caches
                .borrow()
                .domain(crate::shapecache::UiTextDomain::Note)
                .stats();
            note_profile.capture_note_cache_delta(before, after);
        }
        note_profile.finish(self.right_sidebar_note.view.scroll_offset);
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_note_editor_toolbar(
        &mut self,
        layers: &mut TripleLayerQuadAllocator,
        chrome: UiPalette,
        foreground: LinearRgba,
        muted_fg: LinearRgba,
        content_x: usize,
        content_top: usize,
        content_width: usize,
        menu_size: usize,
        show_tree_button: bool,
        tree_button_icon: SvgIcon,
    ) -> anyhow::Result<()> {
        if show_tree_button {
            if self.is_pointer_over_ui_rect(content_x, content_top, menu_size, menu_size) {
                self.fill_rounded_rectangle(
                    layers,
                    2,
                    euclid::rect(
                        content_x as f32,
                        content_top as f32,
                        menu_size as f32,
                        menu_size as f32,
                    ),
                    chrome.control_hover_bg,
                    WINDOW_TAB_ADD_BUTTON_RADIUS,
                )?;
            }
            self.paint_snippet_icon_button(
                layers,
                chrome,
                foreground,
                muted_fg,
                content_x,
                content_top,
                menu_size,
                tree_button_icon,
                UIItemType::RightSidebarNoteTreeToggle,
            )?;
        }
        let menu_x = content_x + content_width.saturating_sub(menu_size);
        if self.is_pointer_over_ui_rect(menu_x, content_top, menu_size, menu_size) {
            self.fill_rounded_rectangle(
                layers,
                2,
                euclid::rect(
                    menu_x as f32,
                    content_top as f32,
                    menu_size as f32,
                    menu_size as f32,
                ),
                chrome.control_hover_bg,
                WINDOW_TAB_ADD_BUTTON_RADIUS,
            )?;
        }
        self.paint_snippet_icon_button(
            layers,
            chrome,
            foreground,
            muted_fg,
            menu_x,
            content_top,
            menu_size,
            SvgIcon::Ellipsis,
            UIItemType::RightSidebarNoteMenu,
        )?;

        // Inline <-> expanded-pane switch, immediately left of the menu.
        let pane_toggle_x = menu_x.saturating_sub(menu_size + self.ui_px(2));
        if pane_toggle_x > content_x {
            if self.is_pointer_over_ui_rect(pane_toggle_x, content_top, menu_size, menu_size) {
                self.fill_rounded_rectangle(
                    layers,
                    2,
                    euclid::rect(
                        pane_toggle_x as f32,
                        content_top as f32,
                        menu_size as f32,
                        menu_size as f32,
                    ),
                    chrome.control_hover_bg,
                    WINDOW_TAB_ADD_BUTTON_RADIUS,
                )?;
            }
            self.paint_snippet_icon_button(
                layers,
                chrome,
                foreground,
                muted_fg,
                pane_toggle_x,
                content_top,
                menu_size,
                if self.right_sidebar_note_pane_expanded {
                    SvgIcon::Shrink
                } else {
                    SvgIcon::Expand
                },
                UIItemType::RightSidebarNotePaneToggle,
            )?;
        }
        Ok(())
    }

    fn request_right_sidebar_note_open(
        &mut self,
        vault_root: PathBuf,
        relative_path: String,
        create: bool,
        project_id: String,
        focus: bool,
    ) -> bool {
        let key = (vault_root.clone(), relative_path.clone());
        if self
            .right_sidebar_note
            .document
            .as_ref()
            .is_some_and(|document| {
                document.vault_root == vault_root && document.relative_path == relative_path
            })
        {
            self.right_sidebar_note_opening = None;
            self.right_sidebar_note_open_failure = None;
            self.right_sidebar_note.load_error = None;
            if focus {
                self.right_sidebar_note.view.focused = true;
            }
            if let Err(err) =
                workspace_threads::set_project_active_note_path(&project_id, Some(&relative_path))
            {
                log::warn!("failed to persist active Note path: {err:#}");
            }
            return true;
        }
        if let Some((failed_key, message)) = self.right_sidebar_note_open_failure.as_ref() {
            if failed_key == &key {
                self.right_sidebar_note.load_error = Some(message.clone());
                return false;
            }
        }
        if self.right_sidebar_note_opening.as_ref() == Some(&key) {
            self.right_sidebar_note.load_error = Some("Opening note…".to_string());
            return false;
        }

        // Keep switching responsive.  The immutable save snapshot and the new
        // document read can proceed independently on workers; save completion
        // is session-gated so it cannot publish into the newly bound note.
        if self.right_sidebar_note.document.is_some() {
            self.right_sidebar_note.freeze_live_source();
            self.save_right_sidebar_note_now();
        }
        self.right_sidebar_note_open_generation =
            self.right_sidebar_note_open_generation.wrapping_add(1);
        let generation = self.right_sidebar_note_open_generation;
        self.right_sidebar_note_opening = Some(key.clone());
        self.right_sidebar_note_open_failure = None;
        self.right_sidebar_note.load_error = Some("Opening note…".to_string());
        let Some(window) = self.window.as_ref().cloned() else {
            self.right_sidebar_note_opening = None;
            return false;
        };

        promise::spawn::spawn(async move {
            let worker_root = vault_root.clone();
            let worker_path = relative_path.clone();
            let result = promise::spawn::spawn_into_new_thread(move || {
                open_vault_document(&worker_root, &worker_path, create)
            })
            .await;
            window.notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                if term_window.right_sidebar_note_open_generation != generation
                    || term_window.right_sidebar_note_opening.as_ref() != Some(&key)
                {
                    return;
                }
                term_window.right_sidebar_note_opening = None;
                match result {
                    Ok(document) => {
                        term_window.right_sidebar_note_open_failure = None;
                        term_window.right_sidebar_note.load_error = None;
                        term_window.right_sidebar_note.bind_document(document);
                        term_window.right_sidebar_note.view.focused = focus;
                        term_window.right_sidebar_note_view = RightSidebarNoteView::Editor;
                        term_window
                            .right_sidebar_note_table_horizontal_offsets
                            .clear();
                        term_window.right_sidebar_note_table_layouts.clear();
                        if let Err(err) = workspace_threads::set_project_active_note_path(
                            &project_id,
                            Some(&relative_path),
                        ) {
                            log::warn!("failed to persist active Note path: {err:#}");
                        }
                        if create {
                            term_window.right_sidebar_note_vault_last_scan = None;
                        }
                    }
                    Err(err) => {
                        let message = format!("{err:#}");
                        term_window.right_sidebar_note.load_error = Some(message.clone());
                        term_window.right_sidebar_note_open_failure = Some((key, message));
                    }
                }
                term_window.invalidate_window();
            })));
        })
        .detach();
        false
    }

    fn ensure_active_right_sidebar_note_document(&mut self) -> bool {
        let Some(vault) = workspace_threads::space_note_vault(&self.active_space_id) else {
            self.right_sidebar_note.clear_document(Some(
                "Choose an existing Vault or create a new one to start Notes.".to_string(),
            ));
            return false;
        };
        let Some(project_id) =
            workspace_threads::active_project_id_for_space(&self.active_space_id)
        else {
            self.right_sidebar_note
                .clear_document(Some("No active Project in this Space.".to_string()));
            return false;
        };

        // The persisted active path is updated only after the worker has
        // successfully opened the clicked file. Until then it still names the
        // old document. Re-applying it from paint would cancel the user's new
        // request before its completion callback can bind the document.
        if note_open_pending_for_vault(self.right_sidebar_note_opening.as_ref(), &vault.root) {
            return self.right_sidebar_note.document.is_some();
        }

        let relative_path = match workspace_threads::project_active_note_path(&project_id) {
            Some(path) => path,
            None => {
                if self.right_sidebar_note_vault_index_root.as_ref() != Some(&vault.root)
                    || self.right_sidebar_note_vault_indexing
                {
                    self.right_sidebar_note.load_error = Some("Indexing Vault…".to_string());
                    return false;
                }
                self.right_sidebar_note_vault_paths
                    .iter()
                    .find(|path| {
                        Path::new(path)
                            .extension()
                            .and_then(|extension| extension.to_str())
                            .is_some_and(|extension| extension.eq_ignore_ascii_case("md"))
                    })
                    .cloned()
                    .unwrap_or_else(|| "Untitled.md".to_string())
            }
        };
        let create = !vault.root.join(&relative_path).exists();
        self.request_right_sidebar_note_open(vault.root, relative_path, create, project_id, false)
    }

    fn schedule_right_sidebar_note_image(&mut self, source: RightSidebarNoteImageSource) {
        if self.right_sidebar_note_images.contains_key(&source)
            || self
                .right_sidebar_note_image_failures
                .get(&source)
                .is_some_and(|failed| failed.elapsed() < Duration::from_secs(60))
            || !self
                .right_sidebar_note_images_loading
                .insert(source.clone())
        {
            return;
        }
        let Some(window) = self.window.as_ref().cloned() else {
            self.right_sidebar_note_images_loading.remove(&source);
            return;
        };
        promise::spawn::spawn(async move {
            let load_source = source.clone();
            let result = promise::spawn::spawn_into_new_thread(move || match load_source {
                RightSidebarNoteImageSource::Local(path) => load_file_preview_image(&path),
                RightSidebarNoteImageSource::Remote(url) => {
                    let image = load_remote_image(&url)?;
                    let encoded_bytes = image.bytes.len();
                    let width = image.width;
                    let height = image.height;
                    Ok(RightSidebarFilePreviewImage {
                        data: Arc::new(ImageData::with_data(image.into_image_data())),
                        width,
                        height,
                        encoded_bytes,
                    })
                }
            })
            .await;
            window.notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                term_window
                    .right_sidebar_note_images_loading
                    .remove(&source);
                match result {
                    Ok(image) => {
                        term_window
                            .right_sidebar_note_image_failures
                            .remove(&source);
                        term_window
                            .right_sidebar_note_image_order
                            .retain(|cached| cached != &source);
                        term_window
                            .right_sidebar_note_image_order
                            .push_back(source.clone());
                        term_window
                            .right_sidebar_note_images
                            .insert(source.clone(), image);
                        while term_window.right_sidebar_note_image_order.len()
                            > NOTE_IMAGE_CACHE_CAPACITY
                            || term_window
                                .right_sidebar_note_images
                                .values()
                                .map(|image| image.encoded_bytes)
                                .sum::<usize>()
                                > NOTE_IMAGE_CACHE_MAX_ENCODED_BYTES
                        {
                            if let Some(expired) =
                                term_window.right_sidebar_note_image_order.pop_front()
                            {
                                term_window.right_sidebar_note_images.remove(&expired);
                            }
                        }
                    }
                    Err(err) => {
                        term_window
                            .right_sidebar_note_image_failures
                            .insert(source.clone(), Instant::now());
                        log::warn!("unable to render Note image {:?}: {err:#}", source);
                    }
                }
                term_window.invalidate_window();
            })));
        })
        .detach();
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_right_sidebar_note_image(
        &mut self,
        layers: &mut TripleLayerQuadAllocator,
        image: &RightSidebarFilePreviewImage,
        x: f32,
        y: f32,
        width: f32,
        height: f32,
        clip_top: f32,
        clip_bottom: f32,
    ) -> anyhow::Result<()> {
        if image.width == 0 || image.height == 0 || width <= 0.0 || height <= 0.0 {
            return Ok(());
        }
        let (draw_width, draw_height) =
            note_image_display_size(image.width, image.height, width, height);
        let draw_x = x + (width - draw_width) / 2.0;
        let draw_y = y + (height - draw_height) / 2.0;
        let Some(gl_state) = self.render_state.as_ref() else {
            return Ok(());
        };
        let (sprite, next_due, _load_state) = gl_state
            .glyph_cache
            .borrow_mut()
            .cached_image(&image.data, None, self.allow_images)
            .context("right sidebar Note image")?;
        self.update_next_frame_time(next_due);
        let texture = sprite.texture_coords();
        let Some(clip) = clip_note_texture(
            draw_x,
            draw_y,
            draw_x + draw_width,
            draw_y + draw_height,
            texture.min_x(),
            texture.min_y(),
            texture.max_x(),
            texture.max_y(),
            x,
            clip_top,
            x + width,
            clip_bottom,
        ) else {
            return Ok(());
        };
        let left_offset = self.dimensions.pixel_width as f32 / 2.0;
        let top_offset = self.dimensions.pixel_height as f32 / 2.0;
        let mut quad = layers.allocate(2)?;
        quad.set_position(
            clip.left - left_offset,
            clip.top - top_offset,
            clip.right - left_offset,
            clip.bottom - top_offset,
        );
        quad.set_texture_discrete(
            clip.texture_left,
            clip.texture_right,
            clip.texture_top,
            clip.texture_bottom,
        );
        quad.set_hsv(None);
        quad.set_has_color(true);
        quad.set_fg_color(LinearRgba::with_components(1.0, 1.0, 1.0, 1.0));
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_right_sidebar_file_preview_pane(
        &mut self,
        layers: &mut TripleLayerQuadAllocator,
        ui_font: &Rc<LoadedFont>,
        ui_metrics: RenderMetrics,
        chrome: UiPalette,
        foreground: LinearRgba,
        muted_fg: LinearRgba,
        rect: RightSidebarRect,
    ) -> anyhow::Result<()> {
        // All preview shaping (visible slices, horizontal-scroll full lines,
        // syntax-coloured lines) goes through the File Preview domain cache.
        let previous_domain = self
            .ui_text_domain
            .replace(crate::shapecache::UiTextDomain::FilePreview);
        let result = self.paint_right_sidebar_file_preview_pane_impl(
            layers, ui_font, ui_metrics, chrome, foreground, muted_fg, rect,
        );
        self.ui_text_domain.set(previous_domain);
        result
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_right_sidebar_file_preview_pane_impl(
        &mut self,
        layers: &mut TripleLayerQuadAllocator,
        ui_font: &Rc<LoadedFont>,
        ui_metrics: RenderMetrics,
        chrome: UiPalette,
        foreground: LinearRgba,
        muted_fg: LinearRgba,
        rect: RightSidebarRect,
    ) -> anyhow::Result<()> {
        let sidebar_bg = chrome.workspace_sidebar_bg;
        if rect.y > 0 {
            self.filled_rectangle(
                layers,
                0,
                euclid::rect(rect.x as f32, 0.0, rect.width as f32, rect.y as f32),
                sidebar_bg,
            )
            .context("right sidebar file preview top background")?;
        }
        self.filled_rectangle(
            layers,
            0,
            euclid::rect(
                rect.x as f32,
                rect.y as f32,
                rect.width as f32,
                rect.height as f32,
            ),
            sidebar_bg,
        )
        .context("right sidebar file preview background")?;
        self.ui_items.push(UIItem {
            x: rect.x,
            y: 0,
            width: rect.width,
            height: rect.y.saturating_add(rect.height),
            item_type: UIItemType::RightSidebarBackground,
        });

        self.filled_rectangle(
            layers,
            1,
            euclid::rect(rect.x as f32, rect.y as f32, 1.0, rect.height as f32),
            chrome.separator,
        )
        .context("right sidebar file preview left separator")?;
        self.filled_rectangle(
            layers,
            1,
            euclid::rect(
                rect.x.saturating_add(rect.width).saturating_sub(1) as f32,
                rect.y as f32,
                1.0,
                rect.height as f32,
            ),
            chrome.separator,
        )
        .context("right sidebar file preview right separator")?;

        let content_x = rect.x + self.ui_px(SIDEBAR_INSET) * 2;
        let content_width = rect.width.saturating_sub(self.ui_px(SIDEBAR_INSET) * 4);
        let content_top = rect.y + self.ui_px(SIDEBAR_INSET) * 2;
        let content_bottom = rect.y.saturating_add(rect.height);
        self.paint_files_preview(
            layers,
            ui_font,
            ui_metrics,
            chrome,
            foreground,
            muted_fg,
            content_x,
            content_top,
            content_width,
            content_bottom,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_snippets_sidebar(
        &mut self,
        layers: &mut TripleLayerQuadAllocator,
        ui_font: &Rc<LoadedFont>,
        ui_metrics: RenderMetrics,
        chrome: UiPalette,
        foreground: LinearRgba,
        muted_fg: LinearRgba,
        content_x: usize,
        content_top: usize,
        content_width: usize,
        content_bottom: usize,
        icon_size: usize,
    ) -> anyhow::Result<()> {
        match self.right_sidebar_snippet_view.clone() {
            RightSidebarSnippetView::List => self.paint_snippets_list(
                layers,
                ui_font,
                ui_metrics,
                chrome,
                foreground,
                muted_fg,
                content_x,
                content_top,
                content_width,
                content_bottom,
                icon_size,
            ),
            RightSidebarSnippetView::EditNew | RightSidebarSnippetView::EditExisting(_) => self
                .paint_snippet_editor(
                    layers,
                    ui_font,
                    ui_metrics,
                    chrome,
                    foreground,
                    muted_fg,
                    content_x,
                    content_top,
                    content_width,
                    content_bottom,
                    icon_size,
                ),
        }
    }

    #[allow(clippy::too_many_arguments)]
    /// Rebuild the flattened browse-tree rows only when the index or the set of
    /// expanded folders has changed; otherwise reuse the cached rows. This runs
    /// on every paint, so it avoids re-cloning up to `FILE_TREE_ROW_LIMIT` rows
    /// (each holding a `PathBuf` + `String`) on frames where nothing changed.
    fn refresh_right_sidebar_file_browse_rows(&mut self, index: &RightSidebarFileIndex) {
        let key = (
            self.right_sidebar_file_index_generation,
            self.right_sidebar_file_expanded_version,
        );
        if self.right_sidebar_file_browse_cache_key == Some(key) {
            return;
        }
        self.right_sidebar_file_browse_rows =
            right_sidebar_file_browse_rows_from_index(index, &self.right_sidebar_file_expanded);
        self.right_sidebar_file_browse_cache_key = Some(key);
    }

    fn paint_files_sidebar(
        &mut self,
        layers: &mut TripleLayerQuadAllocator,
        ui_font: &Rc<LoadedFont>,
        ui_metrics: RenderMetrics,
        chrome: UiPalette,
        foreground: LinearRgba,
        muted_fg: LinearRgba,
        content_x: usize,
        content_top: usize,
        content_width: usize,
        content_bottom: usize,
        icon_size: usize,
    ) -> anyhow::Result<()> {
        if let Some(target) = self
            .active_remote_project_for_files()
            .map_err(anyhow::Error::msg)?
        {
            let changed = self.right_sidebar_remote_files.target.as_ref() != Some(&target);
            if changed {
                self.clear_right_sidebar_text_focus();
                self.right_sidebar_remote_file_tree_scroll_offset = 0.0;
            }
            let effects = self
                .right_sidebar_remote_files
                .transition(RemoteFilesEvent::TargetChanged(Some(target)));
            self.apply_right_sidebar_remote_files_effects(effects);
            if changed && self.right_sidebar_remote_files.can_resume_current() {
                let effects = self
                    .right_sidebar_remote_files
                    .transition(RemoteFilesEvent::ResumeRequested);
                self.apply_right_sidebar_remote_files_effects(effects);
            }
            return self.paint_remote_files_sidebar(
                layers,
                ui_font,
                ui_metrics,
                chrome,
                foreground,
                muted_fg,
                content_x,
                content_top,
                content_width,
                content_bottom,
            );
        }

        if self.right_sidebar_remote_files.target.is_some() {
            let effects = self
                .right_sidebar_remote_files
                .transition(RemoteFilesEvent::TargetChanged(None));
            self.apply_right_sidebar_remote_files_effects(effects);
        }
        self.paint_files_tree(
            layers,
            ui_font,
            ui_metrics,
            chrome,
            foreground,
            muted_fg,
            content_x,
            content_top,
            content_width,
            content_bottom,
            icon_size,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_remote_files_sidebar(
        &mut self,
        layers: &mut TripleLayerQuadAllocator,
        ui_font: &Rc<LoadedFont>,
        ui_metrics: RenderMetrics,
        chrome: UiPalette,
        foreground: LinearRgba,
        muted_fg: LinearRgba,
        content_x: usize,
        content_top: usize,
        content_width: usize,
        content_bottom: usize,
    ) -> anyhow::Result<()> {
        let phase = self.right_sidebar_remote_files.phase.clone();
        // Which host the panel is talking about; shown under the title so the
        // user knows what they are about to connect to.
        let target_label = self
            .right_sidebar_remote_files
            .target
            .as_ref()
            .map(|target| target.project_name.clone());
        match phase {
            RemoteFilesPhase::Disconnected => self.paint_remote_files_empty_state(
                layers,
                ui_font,
                ui_metrics,
                chrome,
                content_x,
                content_top,
                content_width,
                content_bottom,
                // A neutral state, not a fault: CircleAlert here reads as
                // "something broke" the first time the panel is opened.
                SvgIcon::Server,
                false,
                "Not connected",
                target_label.as_deref(),
                Some(("Connect", true)),
            ),
            RemoteFilesPhase::Connecting => self.paint_remote_files_empty_state(
                layers,
                ui_font,
                ui_metrics,
                chrome,
                content_x,
                content_top,
                content_width,
                content_bottom,
                SvgIcon::LoaderCircle,
                // Spinning also drives the repaint schedule, so the panel keeps
                // animating instead of freezing for the length of the connect.
                true,
                "Connecting…",
                target_label.as_deref(),
                Some(("Connecting…", false)),
            ),
            RemoteFilesPhase::Failed(message) => self.paint_remote_files_empty_state(
                layers,
                ui_font,
                ui_metrics,
                chrome,
                content_x,
                content_top,
                content_width,
                content_bottom,
                SvgIcon::CircleAlert,
                false,
                "Connection failed",
                Some(message.as_str()),
                Some(("Retry", true)),
            ),
            RemoteFilesPhase::Connected => self.paint_remote_files_tree(
                layers,
                ui_font,
                ui_metrics,
                chrome,
                foreground,
                muted_fg,
                content_x,
                content_top,
                content_width,
                content_bottom,
            ),
        }
    }

    /// Centred icon / title / detail / action block for the remote Files panel
    /// when there is no tree to show.
    ///
    /// Deliberately not `paint_files_message`: that draws a bordered card meant
    /// for transient status ("Indexing files…") pinned to the top of the panel.
    /// Stacking it above a same-coloured button produced two identical-looking
    /// boxes where only the lower one was clickable, and its icon slot squeezed
    /// the text until it ellipsized mid-sentence.
    #[allow(clippy::too_many_arguments)]
    fn paint_remote_files_empty_state(
        &mut self,
        layers: &mut TripleLayerQuadAllocator,
        ui_font: &Rc<LoadedFont>,
        ui_metrics: RenderMetrics,
        chrome: UiPalette,
        content_x: usize,
        content_top: usize,
        content_width: usize,
        content_bottom: usize,
        icon: SvgIcon,
        spinning: bool,
        title: &str,
        detail: Option<&str>,
        action: Option<(&str, bool)>,
    ) -> anyhow::Result<()> {
        let line_h = ui_metrics.cell_size.height as usize;
        let icon_size = self.ui_px(REMOTE_EMPTY_ICON_SIZE);
        let mut block_h = icon_size + self.ui_px(REMOTE_EMPTY_ICON_GAP) + line_h;
        if detail.is_some() {
            block_h += self.ui_px(REMOTE_EMPTY_DETAIL_GAP) + line_h;
        }
        if action.is_some() {
            block_h += self.ui_px(REMOTE_EMPTY_BUTTON_GAP) + self.ui_px(REMOTE_EMPTY_BUTTON_HEIGHT);
        }

        // Centre in the panel, but never above the top edge when it is short.
        let available = content_bottom.saturating_sub(content_top);
        let mut y = content_top + available.saturating_sub(block_h) / 2;

        let icon_x = content_x + content_width.saturating_sub(icon_size) / 2;
        if spinning {
            self.paint_spinning_ui_icon(
                layers,
                1,
                icon,
                icon_x,
                y,
                icon_size,
                chrome.secondary_text,
            )?;
        } else {
            self.paint_sidebar_icon(layers, icon, icon_x, y, icon_size, chrome.secondary_text)?;
        }
        y += icon_size + self.ui_px(REMOTE_EMPTY_ICON_GAP);

        self.paint_remote_files_centered_text(
            layers,
            ui_font,
            ui_metrics,
            title,
            content_x,
            y,
            content_width,
            chrome.text,
        )?;
        y += line_h;

        if let Some(detail) = detail {
            y += self.ui_px(REMOTE_EMPTY_DETAIL_GAP);
            self.paint_remote_files_centered_text(
                layers,
                ui_font,
                ui_metrics,
                detail,
                content_x,
                y,
                content_width,
                chrome.muted_text,
            )?;
            y += line_h;
        }

        if let Some((label, enabled)) = action {
            y += self.ui_px(REMOTE_EMPTY_BUTTON_GAP);
            self.paint_remote_files_connect_button(
                layers,
                ui_font,
                ui_metrics,
                chrome,
                content_x,
                y,
                content_width,
                label,
                enabled,
            )?;
        }
        Ok(())
    }

    /// Horizontally centre a single line, falling back to the full width (and
    /// therefore ellipsizing) only when the text cannot fit.
    #[allow(clippy::too_many_arguments)]
    fn paint_remote_files_centered_text(
        &mut self,
        layers: &mut TripleLayerQuadAllocator,
        ui_font: &Rc<LoadedFont>,
        ui_metrics: RenderMetrics,
        text: &str,
        content_x: usize,
        y: usize,
        content_width: usize,
        color: LinearRgba,
    ) -> anyhow::Result<()> {
        let text_w = self
            .sidebar_text_width(ui_font, text)
            .unwrap_or(content_width as f32)
            .ceil() as usize;
        let width = text_w.min(content_width);
        let x = content_x + content_width.saturating_sub(width) / 2;
        self.paint_sidebar_text(layers, ui_font, ui_metrics, text, x, y, width, color)
    }

    #[allow(clippy::too_many_arguments)]
    /// The single call to action in an otherwise empty panel, so it is styled
    /// as a primary button (accent fill) rather than reusing the neutral
    /// `sidebar_button_bg` that the surrounding message cards use — otherwise
    /// the only clickable thing on screen looks exactly like the text above it.
    fn paint_remote_files_connect_button(
        &mut self,
        layers: &mut TripleLayerQuadAllocator,
        ui_font: &Rc<LoadedFont>,
        ui_metrics: RenderMetrics,
        chrome: UiPalette,
        x: usize,
        y: usize,
        width: usize,
        label: &str,
        enabled: bool,
    ) -> anyhow::Result<()> {
        let height = self.ui_px(REMOTE_EMPTY_BUTTON_HEIGHT);
        let hovered = enabled && self.is_pointer_over_ui_rect(x, y, width, height);
        let (fill, text_color) = if !enabled {
            (chrome.sidebar_button_bg, chrome.muted_text)
        } else if hovered {
            (chrome.selected_bg.mul_alpha(0.85), chrome.selected_text)
        } else {
            (chrome.selected_bg, chrome.selected_text)
        };
        self.fill_rounded_rectangle_with_border(
            layers,
            1,
            euclid::rect(x as f32, y as f32, width as f32, height as f32),
            fill,
            if enabled { fill } else { chrome.control_border },
            self.ui_f32(SIDEBAR_ROW_RADIUS) + 4.0,
            CAPSULE_BORDER_WIDTH,
        )
        .context("remote Files connect button")?;
        if enabled {
            self.ui_items.push(UIItem {
                x,
                y,
                width,
                height,
                item_type: UIItemType::RightSidebarRemoteFileConnect,
            });
        }
        let label_width = self
            .sidebar_text_width(ui_font, label)
            .unwrap_or(width as f32)
            .ceil() as usize;
        let text_x = x + width.saturating_sub(label_width) / 2;
        self.paint_sidebar_text(
            layers,
            ui_font,
            ui_metrics,
            label,
            text_x,
            y + height.saturating_sub(ui_metrics.cell_size.height as usize) / 2,
            width.saturating_sub(text_x.saturating_sub(x)),
            text_color,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_remote_files_tree(
        &mut self,
        layers: &mut TripleLayerQuadAllocator,
        ui_font: &Rc<LoadedFont>,
        ui_metrics: RenderMetrics,
        chrome: UiPalette,
        foreground: LinearRgba,
        muted_fg: LinearRgba,
        content_x: usize,
        content_top: usize,
        content_width: usize,
        content_bottom: usize,
    ) -> anyhow::Result<()> {
        let refresh_size = (ui_metrics.cell_size.height as usize + self.ui_px(12))
            .clamp(self.ui_px(28), self.ui_px(38));
        let refresh_x = content_x + content_width.saturating_sub(refresh_size);
        let refresh_y =
            content_top + self.ui_px(FILE_FILTER_HEIGHT).saturating_sub(refresh_size) / 2;
        let label_width = content_width.saturating_sub(refresh_size + self.ui_px(SIDEBAR_INSET));
        let label = self
            .right_sidebar_remote_files
            .target
            .as_ref()
            .map(|target| target.project_name.as_str())
            .unwrap_or("Remote Files")
            .to_string();
        self.paint_sidebar_text(
            layers,
            ui_font,
            ui_metrics,
            &label,
            content_x,
            content_top
                + self
                    .ui_px(FILE_FILTER_HEIGHT)
                    .saturating_sub(ui_metrics.cell_size.height as usize)
                    / 2,
            label_width,
            foreground,
        )?;
        self.paint_files_preview_header_icon_button(
            layers,
            chrome,
            foreground,
            muted_fg,
            refresh_x,
            refresh_y,
            refresh_size,
            SvgIcon::RotateCcw,
            UIItemType::RightSidebarRemoteFileRefresh,
        )?;

        let mut tree_top =
            content_top + self.ui_px(FILE_FILTER_HEIGHT) + self.ui_px(FILE_TREE_TOP_GAP);
        let error_y = tree_top;
        let error_message = self.right_sidebar_remote_files.error_message.clone();
        if let Some(message) = error_message.as_deref() {
            self.paint_sidebar_text(
                layers,
                ui_font,
                ui_metrics,
                message,
                content_x,
                tree_top,
                content_width,
                muted_fg,
            )?;
            tree_top += ui_metrics.cell_size.height as usize + self.ui_px(8);
        }
        let rows = self.right_sidebar_remote_files.rows();
        if rows.is_empty() {
            return self.paint_files_message(
                layers,
                ui_font,
                ui_metrics,
                chrome,
                muted_fg,
                content_x,
                tree_top,
                content_width,
                content_bottom,
                self.ui_px(22),
                "Loading remote directory...",
            );
        }

        let row_metrics = right_sidebar_file_row_metrics(ui_metrics);
        let footer_height = if self.right_sidebar_remote_files.has_truncated_directory() {
            row_metrics.row_height
        } else {
            0
        };
        let viewport_bottom = content_bottom
            .saturating_sub(self.ui_px(SIDEBAR_INSET))
            .saturating_sub(footer_height);
        let visible_height = viewport_bottom.saturating_sub(tree_top);
        let total_height = rows.len().saturating_mul(row_metrics.row_height);
        let max_scroll = total_height.saturating_sub(visible_height) as f32;
        self.right_sidebar_remote_file_tree_scroll_offset = self
            .right_sidebar_remote_file_tree_scroll_offset
            .clamp(0.0, max_scroll);
        let scroll = self.right_sidebar_remote_file_tree_scroll_offset;
        let visible =
            visible_file_row_range(rows.len(), scroll, visible_height, row_metrics.row_height);
        let selected = self.right_sidebar_remote_files.selected.clone();
        for (offset, row) in rows.get(visible.clone()).unwrap_or(&[]).iter().enumerate() {
            let index = visible.start + offset;
            let row_top = match file_row_placement(
                index,
                row_metrics.row_height,
                tree_top as f32,
                viewport_bottom as f32,
                scroll,
            ) {
                FileRowPlacement::Above => continue,
                FileRowPlacement::Below => break,
                FileRowPlacement::Visible(row_top) => row_top,
            };
            self.paint_remote_file_tree_row(
                layers,
                ui_font,
                ui_metrics,
                chrome,
                foreground,
                muted_fg,
                content_x,
                row_top.floor().max(0.0) as usize,
                content_width,
                row,
                selected.as_ref(),
                tree_top,
                viewport_bottom,
                row_metrics,
            )?;
        }
        if max_scroll > 0.0 && scroll > 0.0 {
            // Rows retain their true origin so a partly scrolled first row has
            // the correct visible height and hit target. Mask the portion above
            // the viewport, then restore the fixed header content on top, just
            // like the local file tree does below.
            self.paint_right_sidebar_file_mask(
                layers,
                chrome,
                content_x,
                content_top,
                content_width,
                tree_top.saturating_sub(content_top),
            )?;
            self.paint_sidebar_text(
                layers,
                ui_font,
                ui_metrics,
                &label,
                content_x,
                content_top
                    + self
                        .ui_px(FILE_FILTER_HEIGHT)
                        .saturating_sub(ui_metrics.cell_size.height as usize)
                        / 2,
                label_width,
                foreground,
            )?;
            self.paint_files_preview_header_icon_button(
                layers,
                chrome,
                foreground,
                muted_fg,
                refresh_x,
                refresh_y,
                refresh_size,
                SvgIcon::RotateCcw,
                UIItemType::RightSidebarRemoteFileRefresh,
            )?;
            if let Some(message) = error_message.as_deref() {
                self.paint_sidebar_text(
                    layers,
                    ui_font,
                    ui_metrics,
                    message,
                    content_x,
                    error_y,
                    content_width,
                    muted_fg,
                )?;
            }
            let fade_top = tree_top.saturating_sub(self.ui_px(FILE_TREE_TOP_GAP));
            let fade_height = self
                .ui_px(FILE_TREE_TOP_GAP)
                .saturating_add(self.ui_px(FILE_SCROLL_FADE_HEIGHT))
                .min(viewport_bottom.saturating_sub(fade_top));
            self.paint_right_sidebar_file_top_fade(
                layers,
                chrome,
                content_x,
                fade_top,
                content_width,
                fade_height,
            )?;
        }
        if footer_height > 0 {
            self.paint_sidebar_text(
                layers,
                ui_font,
                ui_metrics,
                "More entries are not shown",
                content_x,
                viewport_bottom,
                content_width,
                muted_fg,
            )?;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_remote_file_tree_row(
        &mut self,
        layers: &mut TripleLayerQuadAllocator,
        ui_font: &Rc<LoadedFont>,
        ui_metrics: RenderMetrics,
        chrome: UiPalette,
        foreground: LinearRgba,
        muted_fg: LinearRgba,
        x: usize,
        y: usize,
        width: usize,
        row: &RemoteFileRow,
        selected: Option<&RemotePath>,
        clip_top: usize,
        clip_bottom: usize,
        row_metrics: RightSidebarFileRowMetrics,
    ) -> anyhow::Result<()> {
        let row_bottom = y.saturating_add(row_metrics.row_height);
        let visible_y = y.max(clip_top);
        let visible_bottom = row_bottom.min(clip_bottom);
        let visible_height = visible_bottom.saturating_sub(visible_y);
        if visible_height == 0 {
            return Ok(());
        }
        let hovered = self.is_pointer_over_ui_rect(x, visible_y, width, visible_height);
        let is_selected = selected == Some(&row.entry.path);
        if hovered || is_selected {
            self.fill_rounded_rectangle(
                layers,
                1,
                euclid::rect(
                    x as f32,
                    visible_y as f32,
                    width as f32,
                    visible_height as f32,
                ),
                if is_selected {
                    chrome.selected_bg.mul_alpha(0.46)
                } else {
                    chrome.sidebar_button_hover_bg
                },
                self.ui_f32(SIDEBAR_ROW_RADIUS),
            )
            .context("remote Files row hover")?;
        }
        self.ui_items.push(UIItem {
            x,
            y: visible_y,
            width,
            height: visible_height,
            item_type: UIItemType::RightSidebarRemoteFileRow(row.entry.path.clone()),
        });

        let indent = row
            .depth
            .saturating_mul(row_metrics.indent_step)
            .min(width.saturating_sub(24));
        let chevron_x = x + self.ui_px(SIDEBAR_INSET) + indent;
        let chevron_y = y + row_metrics
            .row_height
            .saturating_sub(row_metrics.chevron_size)
            / 2;
        if row.entry.is_directory() {
            self.paint_sidebar_icon(
                layers,
                if row.expanded {
                    SvgIcon::ChevronDown
                } else {
                    SvgIcon::ChevronRight
                },
                chevron_x,
                chevron_y,
                row_metrics.chevron_size,
                muted_fg,
            )?;
        }
        let icon_x = chevron_x + row_metrics.chevron_size + row_metrics.icon_gap;
        let icon_y = y + row_metrics.row_height.saturating_sub(row_metrics.icon_size) / 2;
        let icon = remote_file_icon(row);
        match icon {
            RightSidebarFileIcon::Material(icon) => {
                self.paint_sidebar_material_icon(
                    layers,
                    icon,
                    icon_x,
                    icon_y,
                    row_metrics.icon_size,
                )?;
            }
            RightSidebarFileIcon::Svg(icon) => {
                self.paint_sidebar_icon(
                    layers,
                    icon,
                    icon_x,
                    icon_y,
                    row_metrics.icon_size,
                    if row.entry.is_directory() {
                        muted_fg
                    } else {
                        foreground
                    },
                )?;
            }
        }
        let text_x = icon_x + row_metrics.icon_size + row_metrics.icon_gap;
        self.paint_sidebar_text(
            layers,
            ui_font,
            ui_metrics,
            &row.entry.name,
            text_x,
            y + row_metrics
                .row_height
                .saturating_sub(ui_metrics.cell_size.height as usize)
                / 2,
            x.saturating_add(width)
                .saturating_sub(text_x + self.ui_px(SIDEBAR_INSET)),
            if row.entry.is_directory() || is_selected {
                foreground
            } else {
                muted_fg
            },
        )
    }

    fn active_remote_project_for_files(
        &self,
    ) -> Result<Option<workspace_threads::RemoteFilesTarget>, String> {
        let mux = Mux::get();
        let active_workspace = self
            .current_mux_workspace()
            .unwrap_or_else(|| mux.active_workspace());
        let workspaces = mux.iter_workspaces();
        let view = workspace_threads::view_for_current_project(
            &self.active_space_id,
            &active_workspace,
            &workspaces,
        );
        let project = view
            .projects
            .iter()
            .find(|project| project.is_active)
            .or_else(|| view.projects.first())
            .ok_or_else(|| "No active project".to_string())?;
        if !project.is_remote {
            return Ok(None);
        }
        if let Some(target) =
            workspace_threads::remote_files_target(&self.active_space_id, &project.id)
        {
            return Ok(Some(target));
        }
        let Some(domain) = workspace_threads::client_domain_for_space(&self.active_space_id) else {
            return Err("Remote Files source is unavailable".to_string());
        };
        Ok(Some(workspace_threads::RemoteFilesTarget {
            project_id: project.id.clone(),
            project_name: project.name.clone(),
            source: workspace_threads::RemoteFilesSource::ClientDomain(domain),
            requested_root: "~".to_string(),
        }))
    }

    fn ssh_config_for_remote_files_target(
        target: &workspace_threads::RemoteFilesTarget,
    ) -> Result<config::SshDomain, String> {
        match &target.source {
            workspace_threads::RemoteFilesSource::SshHost(host_id) => {
                let spec = crate::ssh_hosts::host_spec(host_id)
                    .ok_or_else(|| format!("SSH host {host_id} is unavailable"))?;
                let mut domain = crate::ssh_hosts::build_ssh_domain(&spec);
                domain.stored_password = spec
                    .password
                    .as_deref()
                    .map(crate::secret::reveal)
                    .filter(|password| !password.is_empty());
                Ok(domain)
            }
            workspace_threads::RemoteFilesSource::ClientDomain(domain_name) => {
                let domain = Mux::get()
                    .get_domain_by_name(domain_name)
                    .ok_or_else(|| format!("Mux domain {domain_name} is unavailable"))?;
                let client = domain
                    .downcast_ref::<wezterm_client::domain::ClientDomain>()
                    .ok_or_else(|| {
                        "Remote Files supports only SSH-backed mux domains".to_string()
                    })?;
                client.ssh_domain_config().ok_or_else(|| {
                    "Remote Files does not support Unix or TLS mux domains".to_string()
                })
            }
        }
    }

    fn apply_right_sidebar_remote_files_effects(&mut self, effects: Vec<RemoteFilesEffect>) {
        for effect in effects {
            match effect {
                RemoteFilesEffect::ReleaseLease => {
                    self.right_sidebar_remote_files_lease.take();
                    self.close_right_sidebar_file_preview();
                }
                RemoteFilesEffect::Connect {
                    generation,
                    target,
                    allow_connect,
                } => {
                    let config = match Self::ssh_config_for_remote_files_target(&target) {
                        Ok(config) => config,
                        Err(message) => {
                            self.right_sidebar_remote_files.transition(
                                RemoteFilesEvent::ConnectionFailed {
                                    generation,
                                    message,
                                },
                            );
                            continue;
                        }
                    };
                    let Some(window) = self.window.as_ref().cloned() else {
                        self.right_sidebar_remote_files.transition(
                            RemoteFilesEvent::ConnectionFailed {
                                generation,
                                message: "Window is unavailable".to_string(),
                            },
                        );
                        continue;
                    };
                    let source_key = crate::termwindow::remote_files::RemoteFilesState::source_key(
                        &target.source,
                    );
                    let connection_key = remote_connection_key(&source_key, &config);
                    let requested_root = target.requested_root.clone();
                    promise::spawn::spawn(async move {
                        let manager = remote_connection_manager();
                        let result = match manager
                            .acquire(connection_key.clone(), config, allow_connect)
                            .await
                        {
                            Ok(lease) => {
                                let backend = lease.backend();
                                match backend.resolve_root(requested_root).await {
                                    Ok(root) => match backend
                                        .list_directory(
                                            root.clone(),
                                            crate::termwindow::remote_files::REMOTE_FILE_TREE_ROW_LIMIT,
                                        )
                                        .await
                                    {
                                        Ok(listing) => Ok((lease, root, listing)),
                                        // Bringing the panel up failed on an
                                        // established session, so the session
                                        // is not usable no matter why. Drop it
                                        // rather than letting Retry replay the
                                        // same failure against the same cached
                                        // connection forever.
                                        Err(err) => {
                                            drop(lease);
                                            invalidate_remote_connection(&connection_key);
                                            Err(RemoteAcquireError::Failed(err))
                                        }
                                    },
                                    Err(err) => {
                                        drop(lease);
                                        invalidate_remote_connection(&connection_key);
                                        Err(RemoteAcquireError::Failed(err))
                                    }
                                }
                            }
                            Err(err) => Err(err),
                        };
                        window.notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                            match result {
                                Ok((lease, root, listing)) => {
                                    let current = term_window
                                        .right_sidebar_remote_files
                                        .current_source_key();
                                    if current.as_deref() != Some(source_key.as_str())
                                        || term_window.right_sidebar_remote_files.generation
                                            != generation
                                    {
                                        drop(lease);
                                        return;
                                    }
                                    term_window.right_sidebar_remote_files_lease = Some(lease);
                                    let effects = term_window.right_sidebar_remote_files.transition(
                                        RemoteFilesEvent::Connected {
                                            generation,
                                            root,
                                            listing,
                                        },
                                    );
                                    term_window
                                        .apply_right_sidebar_remote_files_effects(effects);
                                }
                                Err(RemoteAcquireError::NotConnected) => {
                                    term_window.right_sidebar_remote_files.transition(
                                        RemoteFilesEvent::ResumeUnavailable { generation },
                                    );
                                }
                                Err(RemoteAcquireError::Failed(message)) => {
                                    term_window.right_sidebar_remote_files.transition(
                                        RemoteFilesEvent::ConnectionFailed {
                                            generation,
                                            message,
                                        },
                                    );
                                }
                            }
                            term_window.invalidate_window();
                        })));
                    })
                    .detach();
                }
                RemoteFilesEffect::ListDirectory {
                    generation,
                    source_key,
                    path,
                    limit,
                } => {
                    let Some(lease) = self.right_sidebar_remote_files_lease.as_ref() else {
                        self.right_sidebar_remote_files_lease.take();
                        self.close_right_sidebar_file_preview();
                        self.right_sidebar_remote_files.transition(
                            RemoteFilesEvent::ConnectionFailed {
                                generation,
                                message: "Remote Files connection is no longer available"
                                    .to_string(),
                            },
                        );
                        continue;
                    };
                    let Some(operation_lease) = lease.operation_lease() else {
                        self.right_sidebar_remote_files_lease.take();
                        self.close_right_sidebar_file_preview();
                        self.right_sidebar_remote_files.transition(
                            RemoteFilesEvent::ConnectionFailed {
                                generation,
                                message: "Remote Files connection is no longer available"
                                    .to_string(),
                            },
                        );
                        continue;
                    };
                    let backend = lease.backend();
                    let connection_key = lease.connection_key().to_string();
                    let Some(window) = self.window.as_ref().cloned() else {
                        continue;
                    };
                    promise::spawn::spawn(async move {
                        let result = backend.list_directory(path.clone(), limit).await;
                        drop(operation_lease);
                        let connection_died = result.as_ref().is_err_and(|message| {
                            invalidate_remote_connection_if_dead(&connection_key, message)
                        });
                        window.notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                            if term_window
                                .right_sidebar_remote_files
                                .current_source_key()
                                .as_deref()
                                != Some(source_key.as_str())
                            {
                                return;
                            }
                            let event = match result {
                                Ok(listing) => RemoteFilesEvent::DirectoryLoaded {
                                    generation,
                                    path,
                                    listing,
                                },
                                Err(message) if connection_died => {
                                    term_window.right_sidebar_remote_files_lease.take();
                                    term_window.close_right_sidebar_file_preview();
                                    RemoteFilesEvent::ConnectionFailed {
                                        generation,
                                        message,
                                    }
                                }
                                Err(message) => RemoteFilesEvent::DirectoryFailed {
                                    generation,
                                    path,
                                    message,
                                },
                            };
                            let effects = term_window.right_sidebar_remote_files.transition(event);
                            term_window.apply_right_sidebar_remote_files_effects(effects);
                            term_window.invalidate_window();
                        })));
                    })
                    .detach();
                }
                RemoteFilesEffect::LoadPreview {
                    generation,
                    source_key,
                    path,
                } => {
                    self.spawn_right_sidebar_remote_file_preview(generation, source_key, path);
                }
            }
        }
    }

    pub(crate) fn request_right_sidebar_remote_files_connect(&mut self, explicit: bool) {
        let event = if explicit {
            RemoteFilesEvent::ConnectRequested
        } else {
            RemoteFilesEvent::ResumeRequested
        };
        let effects = self.right_sidebar_remote_files.transition(event);
        self.apply_right_sidebar_remote_files_effects(effects);
    }

    pub(crate) fn refresh_right_sidebar_remote_files(&mut self) {
        self.close_right_sidebar_file_preview();
        let effects = self
            .right_sidebar_remote_files
            .transition(RemoteFilesEvent::Refresh);
        self.apply_right_sidebar_remote_files_effects(effects);
    }

    pub(crate) fn open_right_sidebar_remote_file(&mut self, path: RemotePath) {
        match self.right_sidebar_remote_files.kind_for_path(&path) {
            Some(RemoteFileKind::Directory) => {
                let effects = self
                    .right_sidebar_remote_files
                    .transition(RemoteFilesEvent::ToggleDirectory(path));
                self.apply_right_sidebar_remote_files_effects(effects);
            }
            Some(RemoteFileKind::File) => {
                if !self.right_sidebar_file_preview_active() {
                    let max_tree_for_preview = self
                        .right_sidebar_pane_total_max_width()
                        .saturating_sub(self.ui_px(FILE_PREVIEW_PANE_MIN_WIDTH))
                        .max(self.ui_px(RIGHT_SIDEBAR_MIN_WIDTH));
                    self.right_sidebar_file_tree_width = self
                        .right_sidebar_width
                        .clamp(self.ui_px(RIGHT_SIDEBAR_MIN_WIDTH), max_tree_for_preview);
                }
                self.close_right_sidebar_file_preview();
                self.right_sidebar_file_view = RightSidebarFileView::Preview;
                self.right_sidebar_file_preview_message =
                    Some("Loading remote file preview...".to_string());
                let effects = self
                    .right_sidebar_remote_files
                    .transition(RemoteFilesEvent::SelectFile(path));
                self.apply_right_sidebar_remote_files_effects(effects);
            }
            Some(RemoteFileKind::Symlink) | Some(RemoteFileKind::Other) | None => {}
        }
    }

    pub(crate) fn close_right_sidebar_remote_file_preview(&mut self) {
        self.close_right_sidebar_file_preview();
        self.right_sidebar_remote_files
            .transition(RemoteFilesEvent::ClosePreview);
    }

    pub(crate) fn release_right_sidebar_remote_files_if_hidden(&mut self) {
        if self.right_sidebar_file_view_active() {
            return;
        }
        let effects = self
            .right_sidebar_remote_files
            .transition(RemoteFilesEvent::PanelHidden);
        self.apply_right_sidebar_remote_files_effects(effects);
    }

    fn spawn_right_sidebar_remote_file_preview(
        &mut self,
        generation: u64,
        source_key: String,
        path: RemotePath,
    ) {
        let Some(lease) = self.right_sidebar_remote_files_lease.as_ref() else {
            self.right_sidebar_remote_files_lease.take();
            self.close_right_sidebar_file_preview();
            self.right_sidebar_remote_files
                .transition(RemoteFilesEvent::ConnectionFailed {
                    generation,
                    message: "Remote Files connection is no longer available".to_string(),
                });
            return;
        };
        let Some(operation_lease) = lease.operation_lease() else {
            self.right_sidebar_remote_files_lease.take();
            self.close_right_sidebar_file_preview();
            self.right_sidebar_remote_files
                .transition(RemoteFilesEvent::ConnectionFailed {
                    generation,
                    message: "Remote Files connection is no longer available".to_string(),
                });
            return;
        };
        let backend = lease.backend();
        let connection_key = lease.connection_key().to_string();
        let Some(window) = self.window.as_ref().cloned() else {
            return;
        };
        let is_image = is_preview_image_extension(path.extension());
        let limit = if is_image {
            FILE_PREVIEW_IMAGE_MAX_BYTES
        } else {
            FILE_PREVIEW_MAX_BYTES
        };
        let use_dark_syntax_theme = matches!(
            crate::native_settings::effective_appearance(),
            window::Appearance::Dark | window::Appearance::DarkHighContrast
        );
        promise::spawn::spawn(async move {
            let bytes = backend.read_file(path.clone(), limit).await;
            drop(operation_lease);
            let connection_died = bytes.as_ref().is_err_and(|message| {
                invalidate_remote_connection_if_dead(&connection_key, message)
            });
            let preview_path = path.clone();
            let result = match bytes {
                Ok(bytes) => promise::spawn::spawn_into_new_thread(move || {
                    Ok(remote_preview_from_bytes(
                        &preview_path,
                        bytes,
                        use_dark_syntax_theme,
                    ))
                })
                .await
                .unwrap_or_else(|err| RightSidebarLoadedFilePreview {
                    lines: Vec::new(),
                    image: None,
                    message: Some(format!("Unable to prepare remote preview: {err}")),
                    truncated: false,
                }),
                Err(err) => RightSidebarLoadedFilePreview {
                    lines: Vec::new(),
                    image: None,
                    message: Some(err),
                    truncated: false,
                },
            };
            window.notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                if term_window
                    .right_sidebar_remote_files
                    .current_source_key()
                    .as_deref()
                    != Some(source_key.as_str())
                    || term_window.right_sidebar_remote_files.generation != generation
                    || term_window.right_sidebar_remote_files.selected.as_ref() != Some(&path)
                {
                    return;
                }
                let error = result.message.clone();
                if connection_died {
                    term_window.right_sidebar_remote_files_lease.take();
                    term_window.close_right_sidebar_file_preview();
                    term_window.right_sidebar_remote_files.transition(
                        RemoteFilesEvent::ConnectionFailed {
                            generation,
                            message: error
                                .clone()
                                .unwrap_or_else(|| "Remote Files connection was lost".to_string()),
                        },
                    );
                    term_window.invalidate_window();
                    return;
                }
                term_window.right_sidebar_file_preview_lines = result.lines;
                term_window.right_sidebar_file_preview_max_columns = term_window
                    .right_sidebar_file_preview_lines
                    .iter()
                    .map(|line| line.char_count)
                    .max()
                    .unwrap_or(0);
                term_window.right_sidebar_file_preview_image = result.image;
                term_window.right_sidebar_file_preview_message = result.message;
                term_window.right_sidebar_file_preview_truncated = result.truncated;
                term_window.clear_right_sidebar_file_preview_slice_cache();
                term_window.right_sidebar_remote_files.transition(
                    RemoteFilesEvent::PreviewFinished {
                        generation,
                        path,
                        error,
                    },
                );
                term_window.invalidate_window();
            })));
        })
        .detach();
    }

    fn active_local_project_for_files(&self) -> Result<RightSidebarFileRoot, String> {
        if workspace_threads::client_domain_for_space(&self.active_space_id).is_some() {
            return Err("Remote file browsing is not supported yet".to_string());
        }

        let mux = Mux::get();
        let active_workspace = self
            .current_mux_workspace()
            .unwrap_or_else(|| mux.active_workspace());
        let workspaces = mux.iter_workspaces();
        let view = workspace_threads::view_for_current_project(
            &self.active_space_id,
            &active_workspace,
            &workspaces,
        );
        let project = view
            .projects
            .iter()
            .find(|project| project.is_active)
            .or_else(|| view.projects.first())
            .ok_or_else(|| "No active project".to_string())?;

        if project.is_remote {
            return Err("Remote file browsing is not supported yet".to_string());
        }

        let path = workspace_threads::project_reveal_path(&project.id)
            .ok_or_else(|| "Project folder is unavailable".to_string())?;
        Ok(RightSidebarFileRoot {
            project_name: project.name.clone(),
            path,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_files_tree(
        &mut self,
        layers: &mut TripleLayerQuadAllocator,
        ui_font: &Rc<LoadedFont>,
        ui_metrics: RenderMetrics,
        chrome: UiPalette,
        foreground: LinearRgba,
        muted_fg: LinearRgba,
        content_x: usize,
        content_top: usize,
        content_width: usize,
        content_bottom: usize,
        icon_size: usize,
    ) -> anyhow::Result<()> {
        // Reserve room on the right for an icon-only Refresh button so it never
        // eats into the filename/search width.
        let refresh_size = (ui_metrics.cell_size.height as usize + self.ui_px(12))
            .clamp(self.ui_px(28), self.ui_px(38));
        let refresh_gap = self.ui_px(SIDEBAR_INSET);
        let filter_width = content_width.saturating_sub(refresh_size + refresh_gap);
        let refresh_x = content_x + content_width - refresh_size;
        let refresh_y =
            content_top + self.ui_px(FILE_FILTER_HEIGHT).saturating_sub(refresh_size) / 2;

        let filter_input = self.right_sidebar_file_filter.clone();
        self.paint_snippet_text_box(
            layers,
            1,
            ui_font,
            ui_metrics,
            chrome,
            muted_fg,
            content_x,
            content_top,
            filter_width,
            self.ui_px(FILE_FILTER_HEIGHT),
            Some(SvgIcon::Search),
            "Filter files",
            &filter_input,
            self.right_sidebar_file_focus == Some(RightSidebarFileField::Filter),
            UIItemType::RightSidebarFileFilter,
            false,
        )?;
        self.paint_files_preview_header_icon_button(
            layers,
            chrome,
            foreground,
            muted_fg,
            refresh_x,
            refresh_y,
            refresh_size,
            SvgIcon::RotateCcw,
            UIItemType::RightSidebarFileRefresh,
        )?;

        let tree_top = content_top + self.ui_px(FILE_FILTER_HEIGHT) + self.ui_px(FILE_TREE_TOP_GAP);
        let root = match self.sync_right_sidebar_file_root_for_current_workspace() {
            Ok(root) => root,
            Err(message) => {
                return self.paint_files_message(
                    layers,
                    ui_font,
                    ui_metrics,
                    chrome,
                    muted_fg,
                    content_x,
                    tree_top,
                    content_width,
                    content_bottom,
                    icon_size,
                    &message,
                );
            }
        };
        if self
            .right_sidebar_file_expanded
            .insert(path_key(&root.path))
        {
            self.right_sidebar_file_expanded_version =
                self.right_sidebar_file_expanded_version.wrapping_add(1);
        }

        let applied_filter = self.right_sidebar_file_filter_for_tree();
        let index = match self.right_sidebar_file_index_status.clone() {
            RightSidebarFileIndexStatus::Ready => match self.right_sidebar_file_index.clone() {
                Some(index) => index,
                None => {
                    return self.paint_files_message(
                        layers,
                        ui_font,
                        ui_metrics,
                        chrome,
                        muted_fg,
                        content_x,
                        tree_top,
                        content_width,
                        content_bottom,
                        icon_size,
                        "Indexing files...",
                    );
                }
            },
            RightSidebarFileIndexStatus::Failed(message) => {
                return self.paint_files_message(
                    layers,
                    ui_font,
                    ui_metrics,
                    chrome,
                    muted_fg,
                    content_x,
                    tree_top,
                    content_width,
                    content_bottom,
                    icon_size,
                    &message,
                );
            }
            RightSidebarFileIndexStatus::Empty | RightSidebarFileIndexStatus::Indexing => {
                return self.paint_files_message(
                    layers,
                    ui_font,
                    ui_metrics,
                    chrome,
                    muted_fg,
                    content_x,
                    tree_top,
                    content_width,
                    content_bottom,
                    icon_size,
                    "Indexing files...",
                );
            }
        };

        let query = applied_filter.trim().to_string();
        self.start_right_sidebar_file_search_if_needed(&query);
        let row_count = if query.is_empty() {
            self.refresh_right_sidebar_file_browse_rows(&index);
            self.right_sidebar_file_browse_rows.len()
        } else {
            self.right_sidebar_file_search_rows.len()
        };

        if row_count == 0 && self.right_sidebar_file_searching {
            return self.paint_files_message(
                layers,
                ui_font,
                ui_metrics,
                chrome,
                muted_fg,
                content_x,
                tree_top,
                content_width,
                content_bottom,
                icon_size,
                "Searching files...",
            );
        }

        if row_count == 0 && !query.is_empty() {
            return self.paint_files_message(
                layers,
                ui_font,
                ui_metrics,
                chrome,
                muted_fg,
                content_x,
                tree_top,
                content_width,
                content_bottom,
                icon_size,
                "No matching files",
            );
        }

        let row_metrics = right_sidebar_file_row_metrics(ui_metrics);
        let viewport_bottom = content_bottom.saturating_sub(self.ui_px(SIDEBAR_INSET));
        let visible_height = viewport_bottom.saturating_sub(tree_top);
        let total_height = row_count.saturating_mul(row_metrics.row_height);
        let max_scroll = total_height.saturating_sub(visible_height) as f32;
        self.right_sidebar_file_tree_scroll_offset = self
            .right_sidebar_file_tree_scroll_offset
            .clamp(0.0, max_scroll);
        let scroll_offset = self.right_sidebar_file_tree_scroll_offset;

        let tree_top_f = tree_top as f32;
        let viewport_bottom_f = viewport_bottom as f32;
        let selected = self.right_sidebar_file_selected.clone();
        let visible_rows = visible_file_row_range(
            row_count,
            scroll_offset,
            visible_height,
            row_metrics.row_height,
        );
        let rows = if query.is_empty() {
            self.right_sidebar_file_browse_rows
                .get(visible_rows.clone())
                .unwrap_or(&[])
                .to_vec()
        } else {
            self.right_sidebar_file_search_rows
                .get(visible_rows.clone())
                .unwrap_or(&[])
                .to_vec()
        };
        for (offset, row) in rows.iter().enumerate() {
            let idx = visible_rows.start + offset;
            let row_top = match file_row_placement(
                idx,
                row_metrics.row_height,
                tree_top_f,
                viewport_bottom_f,
                scroll_offset,
            ) {
                FileRowPlacement::Above => continue,
                FileRowPlacement::Below => break,
                FileRowPlacement::Visible(row_top) => row_top,
            };
            self.paint_file_tree_row(
                layers,
                ui_font,
                ui_metrics,
                chrome,
                foreground,
                muted_fg,
                content_x,
                row_top.floor().max(0.0) as usize,
                content_width,
                &row,
                selected.as_ref(),
                tree_top,
                viewport_bottom,
                row_metrics,
            )?;
        }

        if max_scroll > 0.0 && scroll_offset > 0.0 {
            let fade_top = content_top + self.ui_px(FILE_FILTER_HEIGHT);
            let fade_height = self
                .ui_px(FILE_TREE_TOP_GAP)
                .saturating_add(self.ui_px(FILE_SCROLL_FADE_HEIGHT))
                .min(viewport_bottom.saturating_sub(fade_top));
            self.paint_right_sidebar_file_mask(
                layers,
                chrome,
                content_x,
                content_top,
                content_width,
                tree_top.saturating_sub(content_top),
            )?;
            self.paint_snippet_text_box(
                layers,
                2,
                ui_font,
                ui_metrics,
                chrome,
                muted_fg,
                content_x,
                content_top,
                filter_width,
                self.ui_px(FILE_FILTER_HEIGHT),
                Some(SvgIcon::Search),
                "Filter files",
                &filter_input,
                self.right_sidebar_file_focus == Some(RightSidebarFileField::Filter),
                UIItemType::RightSidebarFileFilter,
                false,
            )?;
            self.paint_files_preview_header_icon_button(
                layers,
                chrome,
                foreground,
                muted_fg,
                refresh_x,
                refresh_y,
                refresh_size,
                SvgIcon::RotateCcw,
                UIItemType::RightSidebarFileRefresh,
            )?;
            self.paint_right_sidebar_file_top_fade(
                layers,
                chrome,
                content_x,
                fade_top,
                content_width,
                fade_height,
            )?;
        }

        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_files_message(
        &mut self,
        layers: &mut TripleLayerQuadAllocator,
        ui_font: &Rc<LoadedFont>,
        ui_metrics: RenderMetrics,
        chrome: UiPalette,
        muted_fg: LinearRgba,
        x: usize,
        y: usize,
        width: usize,
        content_bottom: usize,
        icon_size: usize,
        message: &str,
    ) -> anyhow::Result<()> {
        let height = self
            .ui_px(RIGHT_SIDEBAR_EMPTY_HEIGHT)
            .min(content_bottom.saturating_sub(y + self.ui_px(SIDEBAR_INSET)));
        if height == 0 {
            return Ok(());
        }
        self.fill_rounded_rectangle_with_border(
            layers,
            1,
            euclid::rect(x as f32, y as f32, width as f32, height as f32),
            chrome.sidebar_button_bg,
            chrome.control_border,
            self.ui_f32(SIDEBAR_ROW_RADIUS) + 6.0,
            CAPSULE_BORDER_WIDTH,
        )
        .context("right sidebar files message")?;
        let empty_icon_size = icon_size
            .min(self.ui_px(22))
            .min(height.saturating_sub(20))
            .max(1);
        let icon_x = x + self.ui_px(SIDEBAR_INSET) + 2;
        let icon_y = y + (height.saturating_sub(empty_icon_size)) / 2;
        self.paint_sidebar_icon(
            layers,
            SvgIcon::CircleAlert,
            icon_x,
            icon_y,
            empty_icon_size,
            muted_fg,
        )?;
        self.paint_sidebar_text(
            layers,
            ui_font,
            ui_metrics,
            message,
            icon_x + empty_icon_size + self.ui_px(SIDEBAR_ICON_GAP) + 2,
            y + (height.saturating_sub(ui_metrics.cell_size.height as usize)) / 2,
            width.saturating_sub(
                empty_icon_size + self.ui_px(SIDEBAR_ICON_GAP) + self.ui_px(SIDEBAR_INSET) * 3,
            ),
            muted_fg,
        )
    }

    fn paint_right_sidebar_note_top_fade(
        &mut self,
        layers: &mut TripleLayerQuadAllocator,
        chrome: UiPalette,
        x: usize,
        y: usize,
        width: usize,
        height: usize,
    ) -> anyhow::Result<()> {
        if width == 0 || height == 0 {
            return Ok(());
        }
        let opaque_height = self.ui_px(12).min(height);
        self.paint_right_sidebar_file_mask(layers, chrome, x, y, width, opaque_height)?;
        self.paint_right_sidebar_file_top_fade(
            layers,
            chrome,
            x,
            y + opaque_height,
            width,
            height.saturating_sub(opaque_height),
        )
    }

    fn paint_right_sidebar_file_mask(
        &mut self,
        layers: &mut TripleLayerQuadAllocator,
        chrome: UiPalette,
        x: usize,
        y: usize,
        width: usize,
        height: usize,
    ) -> anyhow::Result<()> {
        if width == 0 || height == 0 {
            return Ok(());
        }

        self.filled_rectangle(
            layers,
            2,
            euclid::rect(x as f32, y as f32, width as f32, height as f32),
            chrome.workspace_sidebar_bg,
        )
        .context("right sidebar file scroll mask")?;
        Ok(())
    }

    fn paint_right_sidebar_file_top_fade(
        &mut self,
        layers: &mut TripleLayerQuadAllocator,
        chrome: UiPalette,
        x: usize,
        y: usize,
        width: usize,
        height: usize,
    ) -> anyhow::Result<()> {
        if width == 0 || height == 0 {
            return Ok(());
        }

        for step in 0..height {
            let progress = step as f32 / height as f32;
            let alpha = 1.0 - progress * progress * (3.0 - 2.0 * progress);
            self.filled_rectangle(
                layers,
                2,
                euclid::rect(x as f32, (y + step) as f32, width as f32, 1.0),
                chrome.workspace_sidebar_bg.mul_alpha(alpha),
            )
            .context("right sidebar file top fade")?;
        }

        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_file_tree_row(
        &mut self,
        layers: &mut TripleLayerQuadAllocator,
        ui_font: &Rc<LoadedFont>,
        ui_metrics: RenderMetrics,
        chrome: UiPalette,
        foreground: LinearRgba,
        muted_fg: LinearRgba,
        x: usize,
        y: usize,
        width: usize,
        row: &RightSidebarFileTreeRow,
        selected: Option<&PathBuf>,
        clip_top: usize,
        clip_bottom: usize,
        row_metrics: RightSidebarFileRowMetrics,
    ) -> anyhow::Result<()> {
        let row_bottom = y.saturating_add(row_metrics.row_height);
        let visible_y = y.max(clip_top);
        let visible_bottom = row_bottom.min(clip_bottom);
        let visible_height = visible_bottom.saturating_sub(visible_y);
        if visible_height == 0 {
            return Ok(());
        }
        let hovered = self.is_pointer_over_ui_rect(x, visible_y, width, visible_height);
        let is_selected = selected.is_some_and(|path| path == &row.path);
        if hovered || is_selected {
            self.fill_rounded_rectangle(
                layers,
                1,
                euclid::rect(
                    x as f32,
                    visible_y as f32,
                    width as f32,
                    visible_height as f32,
                ),
                if is_selected {
                    chrome.selected_bg.mul_alpha(0.46)
                } else {
                    chrome.sidebar_button_hover_bg
                },
                self.ui_f32(SIDEBAR_ROW_RADIUS),
            )
            .context("right sidebar file row hover")?;
        }
        self.ui_items.push(UIItem {
            x,
            y: visible_y,
            width,
            height: visible_height,
            item_type: UIItemType::RightSidebarFileRow(row.path.clone()),
        });

        let row_icon_size = row_metrics.icon_size;
        let chevron_size = row_metrics.chevron_size;
        let indent = row
            .depth
            .saturating_mul(row_metrics.indent_step)
            .min(width.saturating_sub(24));
        let chevron_x = x + self.ui_px(SIDEBAR_INSET) + indent;
        let icon_y = y + (row_metrics.row_height.saturating_sub(row_icon_size)) / 2;
        let chevron_y = y + (row_metrics.row_height.saturating_sub(chevron_size)) / 2;
        if row.is_dir {
            self.paint_sidebar_icon(
                layers,
                if row.is_expanded {
                    SvgIcon::ChevronDown
                } else {
                    SvgIcon::ChevronRight
                },
                chevron_x,
                chevron_y,
                chevron_size,
                muted_fg,
            )?;
        }

        let file_icon_x = chevron_x + chevron_size + row_metrics.icon_gap;
        match file_icon_for_row(row) {
            RightSidebarFileIcon::Material(icon) => {
                self.paint_sidebar_material_icon(layers, icon, file_icon_x, icon_y, row_icon_size)?;
            }
            RightSidebarFileIcon::Svg(icon) => {
                self.paint_sidebar_icon(
                    layers,
                    icon,
                    file_icon_x,
                    icon_y,
                    row_icon_size,
                    if row.is_dir { muted_fg } else { foreground },
                )?;
            }
        }
        let text_x = file_icon_x + row_icon_size + row_metrics.icon_gap;
        let row_title = self.sidebar_file_row_title(&row.path, &row.name);
        self.paint_sidebar_text(
            layers,
            ui_font,
            ui_metrics,
            &row_title,
            text_x,
            y + (row_metrics
                .row_height
                .saturating_sub(ui_metrics.cell_size.height as usize))
                / 2,
            x.saturating_add(width)
                .saturating_sub(text_x + self.ui_px(SIDEBAR_INSET)),
            if row.is_dir || is_selected {
                foreground
            } else {
                muted_fg
            },
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_files_preview_header(
        &mut self,
        layers: &mut TripleLayerQuadAllocator,
        ui_font: &Rc<LoadedFont>,
        ui_metrics: RenderMetrics,
        chrome: UiPalette,
        foreground: LinearRgba,
        muted_fg: LinearRgba,
        content_x: usize,
        content_top: usize,
        content_width: usize,
        path: &Path,
    ) -> anyhow::Result<()> {
        let header_top = content_top + 4;
        let button_size = 44.min(content_width);
        self.paint_files_preview_header_icon_button(
            layers,
            chrome,
            foreground,
            muted_fg,
            content_x,
            header_top,
            button_size,
            SvgIcon::X,
            UIItemType::RightSidebarFileBack,
        )?;

        let action_gap = 6;
        let available_after_back =
            content_width.saturating_sub(button_size + self.ui_px(SIDEBAR_INSET));
        // The label is always shown in full. Size the button to fit it, limited
        // only by the room left after the two icon buttons, the gaps and a small
        // reserved minimum for the filename — no fixed cap, so a wide pane is
        // actually used.
        let app_label = self.right_sidebar_current_open_with_app_label(path);
        let open_label = match &app_label {
            Some(app) => format!("Open With {app}"),
            None => "Open".to_string(),
        };
        let label_px = self
            .sidebar_text_width(ui_font, &open_label)
            .unwrap_or(0.0)
            .ceil() as usize;
        let desired_open_width = label_px + 12 + 10 + 36 + 6;
        let max_open_width =
            available_after_back.saturating_sub((button_size * 2) + action_gap * 3 + 72);
        let open_button_width = if max_open_width >= 80 {
            desired_open_width.min(max_open_width).max(self.ui_px(80))
        } else {
            button_size
        };
        let action_width = open_button_width
            .saturating_add(button_size * 2)
            .saturating_add(action_gap * 2);
        let actions_x = content_x
            .saturating_add(content_width)
            .saturating_sub(action_width);
        let mut action_x = actions_x;
        self.paint_files_preview_header_open_with_button(
            layers,
            ui_font,
            ui_metrics,
            chrome,
            foreground,
            muted_fg,
            action_x,
            header_top,
            open_button_width,
            button_size,
            &open_label,
        )?;
        action_x += open_button_width + action_gap;
        for (icon, item_type) in [
            (SvgIcon::FolderOpen, UIItemType::RightSidebarFileReveal),
            (SvgIcon::Copy, UIItemType::RightSidebarFileCopyText),
        ] {
            self.paint_files_preview_header_icon_button(
                layers,
                chrome,
                foreground,
                muted_fg,
                action_x,
                header_top,
                button_size,
                icon,
                item_type,
            )?;
            action_x += button_size + action_gap;
        }

        let title_x = content_x + button_size + self.ui_px(SIDEBAR_INSET);
        let title_right = actions_x.saturating_sub(self.ui_px(SIDEBAR_INSET));
        let title_width = title_right.saturating_sub(title_x);
        let title = file_name_for_path(path);
        self.paint_sidebar_text(
            layers,
            ui_font,
            ui_metrics,
            &title,
            title_x,
            header_top + (button_size.saturating_sub(ui_metrics.cell_size.height as usize)) / 2,
            title_width,
            foreground,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_remote_files_preview_header(
        &mut self,
        layers: &mut TripleLayerQuadAllocator,
        ui_font: &Rc<LoadedFont>,
        ui_metrics: RenderMetrics,
        chrome: UiPalette,
        foreground: LinearRgba,
        muted_fg: LinearRgba,
        content_x: usize,
        content_top: usize,
        content_width: usize,
        path: &RemotePath,
    ) -> anyhow::Result<()> {
        let header_top = content_top + 4;
        let button_size = 44.min(content_width);
        self.paint_files_preview_header_icon_button(
            layers,
            chrome,
            foreground,
            muted_fg,
            content_x,
            header_top,
            button_size,
            SvgIcon::X,
            UIItemType::RightSidebarRemoteFileBack,
        )?;
        let can_copy = self.right_sidebar_file_preview_image.is_none()
            && !self.right_sidebar_file_preview_lines.is_empty();
        let actions_x = content_x + content_width.saturating_sub(button_size);
        if can_copy {
            self.paint_files_preview_header_icon_button(
                layers,
                chrome,
                foreground,
                muted_fg,
                actions_x,
                header_top,
                button_size,
                SvgIcon::Copy,
                UIItemType::RightSidebarRemoteFileCopyText,
            )?;
        }
        let title_x = content_x + button_size + self.ui_px(SIDEBAR_INSET);
        let title_right = if can_copy {
            actions_x.saturating_sub(self.ui_px(SIDEBAR_INSET))
        } else {
            content_x + content_width
        };
        self.paint_sidebar_text(
            layers,
            ui_font,
            ui_metrics,
            path.file_name(),
            title_x,
            header_top + button_size.saturating_sub(ui_metrics.cell_size.height as usize) / 2,
            title_right.saturating_sub(title_x),
            foreground,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_current_files_preview_header(
        &mut self,
        layers: &mut TripleLayerQuadAllocator,
        ui_font: &Rc<LoadedFont>,
        ui_metrics: RenderMetrics,
        chrome: UiPalette,
        foreground: LinearRgba,
        muted_fg: LinearRgba,
        content_x: usize,
        content_top: usize,
        content_width: usize,
        local_path: Option<&Path>,
        remote_path: Option<&RemotePath>,
    ) -> anyhow::Result<()> {
        if let Some(path) = remote_path {
            self.paint_remote_files_preview_header(
                layers,
                ui_font,
                ui_metrics,
                chrome,
                foreground,
                muted_fg,
                content_x,
                content_top,
                content_width,
                path,
            )
        } else if let Some(path) = local_path {
            self.paint_files_preview_header(
                layers,
                ui_font,
                ui_metrics,
                chrome,
                foreground,
                muted_fg,
                content_x,
                content_top,
                content_width,
                path,
            )
        } else {
            Ok(())
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_files_preview_header_open_with_button(
        &mut self,
        layers: &mut TripleLayerQuadAllocator,
        ui_font: &Rc<LoadedFont>,
        ui_metrics: RenderMetrics,
        chrome: UiPalette,
        foreground: LinearRgba,
        muted_fg: LinearRgba,
        x: usize,
        y: usize,
        width: usize,
        height: usize,
        label: &str,
    ) -> anyhow::Result<()> {
        let arrow_width = if width >= 112 { 36.min(width / 3) } else { 0 };
        let main_width = width.saturating_sub(arrow_width);
        let main_hover = self.is_pointer_over_ui_rect(x, y, main_width, height);
        let menu_hover =
            arrow_width > 0 && self.is_pointer_over_ui_rect(x + main_width, y, arrow_width, height);
        if main_hover || menu_hover {
            self.fill_rounded_rectangle(
                layers,
                2,
                euclid::rect(x as f32, y as f32, width as f32, height as f32),
                chrome.control_hover_bg,
                WINDOW_TAB_ADD_BUTTON_RADIUS,
            )
            .context("right sidebar file preview open with hover")?;
        }

        self.ui_items.push(UIItem {
            x,
            y,
            width: main_width.max(1),
            height,
            item_type: UIItemType::RightSidebarFileOpen,
        });
        if arrow_width > 0 {
            self.ui_items.push(UIItem {
                x: x + main_width,
                y,
                width: arrow_width,
                height,
                item_type: UIItemType::RightSidebarFileOpenMenu,
            });
        }

        if main_width >= self.ui_px(28) {
            let text_x = x + self.ui_px(12);
            let text_right = x + main_width.saturating_sub(self.ui_px(10));
            let text_width = text_right.saturating_sub(text_x);
            self.paint_sidebar_text(
                layers,
                ui_font,
                ui_metrics,
                label,
                text_x,
                y + (height.saturating_sub(ui_metrics.cell_size.height as usize)) / 2,
                text_width,
                if main_hover { foreground } else { muted_fg },
            )?;
        }

        if arrow_width > 0 {
            let chevron_size = (height * 40 / 100).clamp(self.ui_px(14), self.ui_px(18));
            self.paint_sidebar_icon(
                layers,
                SvgIcon::ChevronDown,
                x + main_width + (arrow_width.saturating_sub(chevron_size)) / 2,
                y + (height.saturating_sub(chevron_size)) / 2,
                chevron_size,
                if menu_hover { foreground } else { muted_fg },
            )?;
        }

        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_files_preview_header_icon_button(
        &mut self,
        layers: &mut TripleLayerQuadAllocator,
        chrome: UiPalette,
        foreground: LinearRgba,
        muted_fg: LinearRgba,
        x: usize,
        y: usize,
        size: usize,
        icon: SvgIcon,
        item_type: UIItemType,
    ) -> anyhow::Result<()> {
        let hovered = self.is_pointer_over_ui_rect(x, y, size, size);
        if hovered {
            self.fill_rounded_rectangle(
                layers,
                2,
                euclid::rect(x as f32, y as f32, size as f32, size as f32),
                chrome.control_hover_bg,
                WINDOW_TAB_ADD_BUTTON_RADIUS,
            )
            .context("right sidebar file preview header button hover")?;
        }
        self.ui_items.push(UIItem {
            x,
            y,
            width: size,
            height: size,
            item_type,
        });
        let icon_size = (size * 58 / 100).max(self.ui_px(16));
        self.paint_sidebar_icon(
            layers,
            icon,
            x + (size.saturating_sub(icon_size)) / 2,
            y + (size.saturating_sub(icon_size)) / 2,
            icon_size,
            if hovered { foreground } else { muted_fg },
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_files_preview(
        &mut self,
        layers: &mut TripleLayerQuadAllocator,
        ui_font: &Rc<LoadedFont>,
        ui_metrics: RenderMetrics,
        chrome: UiPalette,
        foreground: LinearRgba,
        muted_fg: LinearRgba,
        content_x: usize,
        content_top: usize,
        content_width: usize,
        _content_bottom: usize,
    ) -> anyhow::Result<()> {
        let local_path = self.right_sidebar_file_selected.clone();
        let remote_path = self.right_sidebar_remote_files.selected.clone();
        if local_path.is_none() && remote_path.is_none() {
            return Ok(());
        }
        let mut profile = FilePreviewPaintProfile::new();
        let mut profile_scroll_offset = 0.0;
        let mut profile_horizontal_offset = 0usize;

        let Some(metrics) = self.right_sidebar_file_preview_body_metrics(ui_metrics) else {
            self.paint_current_files_preview_header(
                layers,
                ui_font,
                ui_metrics,
                chrome,
                foreground,
                muted_fg,
                content_x,
                content_top,
                content_width,
                local_path.as_deref(),
                remote_path.as_ref(),
            )?;
            return Ok(());
        };

        let body_x = metrics.x;
        let body_y = metrics.y;
        let body_bottom = metrics.y.saturating_add(
            self.right_sidebar_file_preview_effective_visible_height(metrics, ui_metrics),
        );
        let (body_width, number_width, text_x, text_width) =
            self.right_sidebar_file_preview_text_layout(metrics, ui_metrics);
        let mut show_top_fade = false;
        let mut show_scrollbars = false;

        if let Some(message) = self.right_sidebar_file_preview_message.clone() {
            self.paint_sidebar_text(
                layers, ui_font, ui_metrics, &message, body_x, body_y, body_width, muted_fg,
            )?;
        } else if let Some(image) = self.right_sidebar_file_preview_image.clone() {
            self.paint_right_sidebar_file_preview_image(layers, metrics, &image)?;
        } else {
            let line_height = metrics.line_height;
            let visible_height =
                self.right_sidebar_file_preview_effective_visible_height(metrics, ui_metrics);
            let line_count = preview_line_count(&self.right_sidebar_file_preview_lines);
            profile.line_count = line_count;
            let max_scroll = metrics.total_height.saturating_sub(visible_height) as f32;
            self.right_sidebar_file_preview_scroll_offset = self
                .right_sidebar_file_preview_scroll_offset
                .clamp(0.0, max_scroll);
            let scroll_offset = self.right_sidebar_file_preview_scroll_offset;
            profile_scroll_offset = scroll_offset;
            show_top_fade = max_scroll > 0.0 && scroll_offset > 0.0;
            show_scrollbars = true;

            let cell_width = ui_metrics.cell_size.width.max(1) as usize;
            let visible_columns =
                estimated_file_preview_visible_columns(text_width, cell_width).max(1);
            self.right_sidebar_file_preview_horizontal_offset =
                self.right_sidebar_file_preview_horizontal_offset.min(
                    self.right_sidebar_file_preview_horizontal_scroll_max_with_metrics(ui_metrics),
                );
            let horizontal_offset = self.right_sidebar_file_preview_horizontal_offset;
            profile_horizontal_offset = horizontal_offset;
            // Render only the visible horizontal window, never the whole line
            // (lines can be tens of thousands of columns wide). The preview font
            // is proportional, so `visible_columns` (text_width / cell_width)
            // under-counts how many glyphs actually fit; over-slice generously
            // and let the per-glyph pixel clip in the painter stop at the edge.
            let paint_columns = visible_columns.saturating_mul(3).saturating_add(8);
            self.ui_items.push(UIItem {
                x: body_x,
                y: body_y,
                width: body_width,
                height: visible_height,
                item_type: UIItemType::RightSidebarFilePreviewText,
            });
            let first_visible_line = (scroll_offset / line_height as f32).floor().max(0.0) as usize;
            let visible_line_count = visible_height / line_height + 3;
            let visible_range = preview_visible_line_range(
                self.right_sidebar_file_preview_lines.len(),
                first_visible_line,
                visible_line_count,
            );
            if self.right_sidebar_file_preview_lines.is_empty() && first_visible_line == 0 {
                let empty_line = preview_line_from_plain("");
                self.paint_right_sidebar_file_preview_text_line(
                    layers,
                    ui_font,
                    ui_metrics,
                    chrome,
                    foreground,
                    muted_fg,
                    0,
                    &empty_line,
                    body_x,
                    body_y,
                    body_bottom,
                    number_width,
                    text_x,
                    text_width,
                    line_height,
                    scroll_offset,
                    horizontal_offset,
                    visible_columns,
                    paint_columns,
                    cell_width,
                    Some(&mut profile),
                )?;
            } else {
                for idx in visible_range {
                    let Some(line) = self.right_sidebar_file_preview_lines.get(idx) else {
                        continue;
                    };
                    let should_break = self.paint_right_sidebar_file_preview_text_line(
                        layers,
                        ui_font,
                        ui_metrics,
                        chrome,
                        foreground,
                        muted_fg,
                        idx,
                        line,
                        body_x,
                        body_y,
                        body_bottom,
                        number_width,
                        text_x,
                        text_width,
                        line_height,
                        scroll_offset,
                        horizontal_offset,
                        visible_columns,
                        paint_columns,
                        cell_width,
                        Some(&mut profile),
                    )?;
                    if should_break {
                        break;
                    }
                }
            }

            if self.right_sidebar_file_preview_truncated {
                let line_top = body_y as f32 + (line_count * line_height) as f32 - scroll_offset;
                if line_top < body_bottom as f32 {
                    let visible_text = preview_text_slice(
                        FILE_PREVIEW_TRUNCATED_LABEL,
                        horizontal_offset,
                        paint_columns,
                    );
                    self.paint_sidebar_text(
                        layers,
                        ui_font,
                        ui_metrics,
                        &visible_text,
                        text_x,
                        line_top.floor().max(0.0) as usize,
                        text_width,
                        muted_fg,
                    )?;
                }
            }
        }

        self.paint_right_sidebar_file_mask(
            layers,
            chrome,
            content_x,
            content_top,
            content_width,
            metrics.y.saturating_sub(content_top),
        )?;
        if show_top_fade {
            self.paint_right_sidebar_file_top_fade(
                layers,
                chrome,
                metrics.x,
                metrics.y,
                metrics.width,
                self.ui_px(FILE_SCROLL_FADE_HEIGHT)
                    .min(body_bottom.saturating_sub(metrics.y)),
            )?;
        }
        self.paint_current_files_preview_header(
            layers,
            ui_font,
            ui_metrics,
            chrome,
            foreground,
            muted_fg,
            content_x,
            content_top,
            content_width,
            local_path.as_deref(),
            remote_path.as_ref(),
        )?;
        if show_scrollbars {
            self.paint_right_sidebar_file_preview_scrollbar(layers, chrome)?;
            self.paint_right_sidebar_file_preview_horizontal_scrollbar(layers, chrome)?;
        }
        profile.finish(profile_scroll_offset, profile_horizontal_offset);
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_right_sidebar_file_preview_text_line(
        &self,
        layers: &mut TripleLayerQuadAllocator,
        ui_font: &Rc<LoadedFont>,
        ui_metrics: RenderMetrics,
        chrome: UiPalette,
        foreground: LinearRgba,
        muted_fg: LinearRgba,
        idx: usize,
        line: &RightSidebarFilePreviewLine,
        body_x: usize,
        body_y: usize,
        body_bottom: usize,
        number_width: usize,
        text_x: usize,
        text_width: usize,
        line_height: usize,
        scroll_offset: f32,
        horizontal_offset: usize,
        visible_columns: usize,
        paint_columns: usize,
        cell_width: usize,
        mut profile: Option<&mut FilePreviewPaintProfile>,
    ) -> anyhow::Result<bool> {
        let line_top = body_y as f32 + (idx * line_height) as f32 - scroll_offset;
        let line_bottom = line_top + line_height as f32;
        if line_bottom <= body_y as f32 {
            return Ok(false);
        }
        if line_top >= body_bottom as f32 {
            return Ok(true);
        }
        if let Some(profile) = profile.as_deref_mut().filter(|profile| profile.enabled) {
            profile.visible_lines += 1;
            profile.slice_requests += 1;
            if line.spans.is_empty() {
                profile.plain_lines += 1;
            } else {
                profile.highlighted_lines += 1;
            }
        }

        let line_y = line_top.floor().max(0.0) as usize;
        self.paint_right_sidebar_file_preview_selection_for_line(
            layers,
            chrome,
            line,
            idx,
            horizontal_offset,
            visible_columns,
            text_x,
            line_y,
            line_height,
            cell_width,
        )?;
        let line_number_lookup = self.paint_ui_title_text_cached(
            layers,
            ui_font,
            &ui_metrics,
            &(idx + 1).to_string(),
            body_x,
            line_y,
            number_width,
            muted_fg.mul_alpha(0.72),
        )?;
        if let Some(profile) = profile.as_deref_mut() {
            profile.record_shape_lookup(line_number_lookup);
        }

        let text_lookup = if line.char_count <= FILE_PREVIEW_FULL_LINE_SHAPE_MAX_COLS {
            // Fast path: shape the whole line once (cached) and translate/clip it
            // for the current horizontal offset, so panning never re-shapes.
            self.paint_full_line_preview_text(
                layers,
                ui_font,
                ui_metrics,
                idx,
                line,
                horizontal_offset,
                text_x,
                line_y,
                text_width,
                foreground,
            )?
        } else if line.spans.is_empty() {
            // Fallback for pathological ultra-long lines: render only the visible
            // window (horizontal scroll re-shapes, but such lines are rare). Clip
            // at the edge (no "..."); the horizontal scrollbar shows there's more.
            let visible_text =
                self.cached_plain_preview_slice(idx, line, horizontal_offset, paint_columns);
            self.paint_ui_title_text_cached(
                layers,
                ui_font,
                &ui_metrics,
                &visible_text,
                text_x,
                line_y,
                text_width,
                foreground,
            )?
        } else {
            self.paint_highlighted_preview_line(
                layers,
                ui_font,
                ui_metrics,
                idx,
                line,
                horizontal_offset,
                paint_columns,
                text_x,
                line_y,
                text_width,
                foreground,
            )?
        };
        if let Some(profile) = profile.as_deref_mut() {
            profile.record_shape_lookup(text_lookup);
        }

        Ok(false)
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_right_sidebar_file_preview_selection_for_line(
        &self,
        layers: &mut TripleLayerQuadAllocator,
        chrome: UiPalette,
        line: &RightSidebarFilePreviewLine,
        line_idx: usize,
        horizontal_offset: usize,
        visible_columns: usize,
        text_x: usize,
        line_y: usize,
        line_height: usize,
        cell_width: usize,
    ) -> anyhow::Result<()> {
        let Some((start, end)) = self.right_sidebar_file_preview_selection_range() else {
            return Ok(());
        };
        if line_idx < start.line || line_idx > end.line || visible_columns == 0 {
            return Ok(());
        }

        let line_len = line.char_count;
        let selection_start = if line_idx == start.line {
            start.column
        } else {
            0
        };
        let selection_end = if line_idx == end.line {
            end.column
        } else {
            line_len
        };
        if selection_end <= selection_start {
            return Ok(());
        }

        let visible_start = horizontal_offset;
        let visible_end = horizontal_offset.saturating_add(visible_columns);
        let paint_start = selection_start.max(visible_start);
        let paint_end = selection_end.min(visible_end);
        if paint_end <= paint_start {
            return Ok(());
        }

        let x = text_x.saturating_add(paint_start.saturating_sub(horizontal_offset) * cell_width);
        let width = paint_end
            .saturating_sub(paint_start)
            .saturating_mul(cell_width);
        if width == 0 {
            return Ok(());
        }

        self.filled_rectangle(
            layers,
            1,
            euclid::rect(x as f32, line_y as f32, width as f32, line_height as f32),
            chrome.selected_bg.mul_alpha(0.48),
        )
        .context("right sidebar file preview selection")?;
        Ok(())
    }

    fn paint_right_sidebar_file_preview_image(
        &mut self,
        layers: &mut TripleLayerQuadAllocator,
        metrics: RightSidebarFilePreviewBodyMetrics,
        image: &RightSidebarFilePreviewImage,
    ) -> anyhow::Result<()> {
        if image.width == 0
            || image.height == 0
            || metrics.width == 0
            || metrics.visible_height == 0
        {
            return Ok(());
        }

        let max_width = metrics.width as f32;
        let max_height = metrics.visible_height as f32;
        let scale = (max_width / image.width as f32).min(max_height / image.height as f32);
        if !scale.is_finite() || scale <= 0.0 {
            return Ok(());
        }

        let draw_width = (image.width as f32 * scale).max(1.0).min(max_width);
        let draw_height = (image.height as f32 * scale).max(1.0).min(max_height);
        let draw_x = metrics.x as f32 + (max_width - draw_width) / 2.0;
        let draw_y = metrics.y as f32 + (max_height - draw_height) / 2.0;

        let gl_state = self.render_state.as_ref().unwrap();
        let (sprite, next_due, _load_state) = gl_state
            .glyph_cache
            .borrow_mut()
            .cached_image(&image.data, None, self.allow_images)
            .context("right sidebar file preview image")?;
        self.update_next_frame_time(next_due);

        let left_offset = self.dimensions.pixel_width as f32 / 2.0;
        let top_offset = self.dimensions.pixel_height as f32 / 2.0;
        let mut quad = layers.allocate(2)?;
        quad.set_position(
            draw_x - left_offset,
            draw_y - top_offset,
            draw_x + draw_width - left_offset,
            draw_y + draw_height - top_offset,
        );
        quad.set_texture(sprite.texture_coords());
        quad.set_hsv(None);
        quad.set_has_color(true);
        quad.set_fg_color(LinearRgba::with_components(1.0, 1.0, 1.0, 1.0));

        Ok(())
    }

    fn paint_right_sidebar_file_preview_scrollbar(
        &mut self,
        layers: &mut TripleLayerQuadAllocator,
        chrome: UiPalette,
    ) -> anyhow::Result<()> {
        let Some(scroll) = self.right_sidebar_file_preview_scroll_geometry() else {
            return Ok(());
        };

        let track_radius = scroll.track_width as f32 / 2.0;
        self.fill_rounded_rectangle(
            layers,
            2,
            euclid::rect(
                scroll.track_x as f32,
                scroll.track_y as f32,
                scroll.track_width as f32,
                scroll.track_height as f32,
            ),
            chrome.separator,
            track_radius,
        )
        .context("right sidebar file preview scroll track")?;

        self.fill_rounded_rectangle(
            layers,
            2,
            euclid::rect(
                scroll.track_x as f32,
                scroll.thumb_y,
                scroll.track_width as f32,
                scroll.thumb_height,
            ),
            chrome.scrollbar_thumb,
            track_radius,
        )
        .context("right sidebar file preview scroll thumb")?;

        let hit_slop = self.ui_px(FILE_PREVIEW_SCROLLBAR_HIT_SLOP);
        self.ui_items.push(UIItem {
            x: scroll.track_x.saturating_sub(hit_slop),
            y: scroll.track_y,
            width: scroll.track_width + hit_slop * 2,
            height: scroll.track_height,
            item_type: UIItemType::RightSidebarFilePreviewScrollTrack,
        });
        self.ui_items.push(UIItem {
            x: scroll.track_x.saturating_sub(hit_slop),
            y: scroll.thumb_y.round().max(0.0) as usize,
            width: scroll.track_width + hit_slop * 2,
            height: scroll.thumb_height.round().max(1.0) as usize,
            item_type: UIItemType::RightSidebarFilePreviewScrollThumb,
        });

        Ok(())
    }

    fn paint_right_sidebar_file_preview_horizontal_scrollbar(
        &mut self,
        layers: &mut TripleLayerQuadAllocator,
        chrome: UiPalette,
    ) -> anyhow::Result<()> {
        let Some(scroll) = self.right_sidebar_file_preview_horizontal_scroll_geometry() else {
            return Ok(());
        };

        let track_radius = scroll.track_height as f32 / 2.0;
        self.fill_rounded_rectangle(
            layers,
            2,
            euclid::rect(
                scroll.track_x as f32,
                scroll.track_y as f32,
                scroll.track_width as f32,
                scroll.track_height as f32,
            ),
            chrome.separator,
            track_radius,
        )
        .context("right sidebar file preview horizontal scroll track")?;

        self.fill_rounded_rectangle(
            layers,
            2,
            euclid::rect(
                scroll.thumb_x,
                scroll.track_y as f32,
                scroll.thumb_width,
                scroll.track_height as f32,
            ),
            chrome.scrollbar_thumb,
            track_radius,
        )
        .context("right sidebar file preview horizontal scroll thumb")?;

        self.ui_items.push(UIItem {
            x: scroll.track_x,
            y: scroll
                .track_y
                .saturating_sub(self.ui_px(FILE_PREVIEW_SCROLLBAR_HIT_SLOP)),
            width: scroll.track_width,
            height: scroll.track_height + self.ui_px(FILE_PREVIEW_SCROLLBAR_HIT_SLOP) * 2,
            item_type: UIItemType::RightSidebarFilePreviewHorizontalScrollTrack,
        });
        self.ui_items.push(UIItem {
            x: scroll.thumb_x.round().max(0.0) as usize,
            y: scroll
                .track_y
                .saturating_sub(self.ui_px(FILE_PREVIEW_SCROLLBAR_HIT_SLOP)),
            width: scroll.thumb_width.round().max(1.0) as usize,
            height: scroll.track_height + self.ui_px(FILE_PREVIEW_SCROLLBAR_HIT_SLOP) * 2,
            item_type: UIItemType::RightSidebarFilePreviewHorizontalScrollThumb,
        });

        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_highlighted_preview_line(
        &self,
        layers: &mut TripleLayerQuadAllocator,
        ui_font: &Rc<LoadedFont>,
        ui_metrics: RenderMetrics,
        line_index: usize,
        line: &RightSidebarFilePreviewLine,
        horizontal_offset: usize,
        visible_columns: usize,
        x: usize,
        y: usize,
        width: usize,
        fallback: LinearRgba,
    ) -> anyhow::Result<UiShapeCacheLookup> {
        if width == 0 || visible_columns == 0 {
            return Ok(UiShapeCacheLookup::Skipped);
        }

        // Build the visible window's text + a per-character colour list from the
        // spans, then shape it in a single call. Shaping once per line (instead
        // of once per coloured span) is what keeps scrolling smooth.
        let visible =
            self.cached_colored_preview_slice(line_index, line, horizontal_offset, visible_columns);
        if visible.text.is_empty() {
            return Ok(UiShapeCacheLookup::Skipped);
        }
        self.paint_ui_colored_text_cached(
            layers,
            ui_font,
            &ui_metrics,
            &visible.text,
            &visible.colors,
            fallback,
            x,
            y,
            width,
        )
    }

    /// Horizontal-scroll fast path: shape the whole line once (shared shape
    /// cache, keyed on the full line text so it is stable across every
    /// horizontal offset), then translate the cached glyph run left by the pixel
    /// width of the scrolled-past columns and clip it to the text column. Panning
    /// only recomputes the translation; it never re-shapes or re-slices.
    #[allow(clippy::too_many_arguments)]
    fn paint_full_line_preview_text(
        &self,
        layers: &mut TripleLayerQuadAllocator,
        ui_font: &Rc<LoadedFont>,
        ui_metrics: RenderMetrics,
        line_index: usize,
        line: &RightSidebarFilePreviewLine,
        horizontal_offset: usize,
        text_x: usize,
        line_y: usize,
        text_width: usize,
        foreground: LinearRgba,
    ) -> anyhow::Result<UiShapeCacheLookup> {
        if line.plain.is_empty() || text_width == 0 {
            return Ok(UiShapeCacheLookup::Skipped);
        }
        let (shaped, lookup) = self.cached_ui_shape(ui_font, &ui_metrics, &line.plain)?;
        if shaped.is_empty() {
            return Ok(lookup);
        }

        // Pixel offset of the first visible column within the cached run. The
        // glyph at `horizontal_offset` lands exactly on `text_x`, so columns to
        // its left are fully clipped (no partial glyph bleeds into the gutter).
        let start_byte = line
            .plain
            .char_indices()
            .nth(horizontal_offset)
            .map(|(byte, _)| byte)
            .unwrap_or(line.plain.len());
        let mut start_px = 0.0f32;
        for info in shaped.iter() {
            if info.cluster >= start_byte {
                break;
            }
            start_px += info.glyph.x_advance.get() as f32;
        }

        let start_x = text_x as f32 - start_px;
        let clip_left = text_x as f32;
        let clip_right = (text_x + text_width) as f32;
        let y = line_y as f32;

        if line.spans.is_empty() {
            self.paint_cached_ui_shape_pixel_clipped(
                layers,
                &ui_metrics,
                &shaped,
                start_x,
                y,
                clip_left,
                clip_right,
                |_| foreground,
            )?;
        } else {
            let colors = self.cached_full_line_colors(line_index, line);
            self.paint_cached_ui_shape_pixel_clipped(
                layers,
                &ui_metrics,
                &shaped,
                start_x,
                y,
                clip_left,
                clip_right,
                |info| colors.get(info.cluster).copied().unwrap_or(foreground),
            )?;
        }

        Ok(lookup)
    }

    fn cached_plain_preview_slice(
        &self,
        line_index: usize,
        line: &RightSidebarFilePreviewLine,
        horizontal_offset: usize,
        paint_columns: usize,
    ) -> String {
        let key = RightSidebarFilePreviewSliceCacheKey {
            generation: self.right_sidebar_file_preview_generation,
            line_index,
            horizontal_offset,
            paint_columns,
            highlighted: false,
        };
        self.cached_right_sidebar_file_preview_slice(key, || {
            RightSidebarFilePreviewSliceCacheValue {
                text: preview_text_slice(&line.plain, horizontal_offset, paint_columns)
                    .into_owned(),
                colors: Vec::new(),
            }
        })
        .text
    }

    fn cached_colored_preview_slice(
        &self,
        line_index: usize,
        line: &RightSidebarFilePreviewLine,
        horizontal_offset: usize,
        paint_columns: usize,
    ) -> RightSidebarFilePreviewSliceCacheValue {
        let key = RightSidebarFilePreviewSliceCacheKey {
            generation: self.right_sidebar_file_preview_generation,
            line_index,
            horizontal_offset,
            paint_columns,
            highlighted: true,
        };
        self.cached_right_sidebar_file_preview_slice(key, || {
            let (text, colors) = preview_visible_colored(line, horizontal_offset, paint_columns);
            RightSidebarFilePreviewSliceCacheValue { text, colors }
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_snippets_list(
        &mut self,
        layers: &mut TripleLayerQuadAllocator,
        ui_font: &Rc<LoadedFont>,
        ui_metrics: RenderMetrics,
        chrome: UiPalette,
        foreground: LinearRgba,
        muted_fg: LinearRgba,
        content_x: usize,
        content_top: usize,
        content_width: usize,
        content_bottom: usize,
        icon_size: usize,
    ) -> anyhow::Result<()> {
        let toolbar_y = content_top;
        let new_button_icon_size = (ui_metrics.cell_size.height as usize + self.ui_px(2))
            .clamp(self.ui_px(18), self.ui_px(22));
        let new_button_label_width =
            self.sidebar_text_width(ui_font, "New Snippet")?.ceil() as usize;
        let toolbar_gap = self.ui_px(SNIPPET_ROW_GAP);
        let search_is_active = self.right_sidebar_snippet_focus
            == Some(RightSidebarSnippetField::Search)
            || !self.right_sidebar_snippet_search.text().is_empty();
        let full_new_button_width = new_button_icon_size
            + self.ui_px(SIDEBAR_ICON_GAP)
            + new_button_label_width
            + self.ui_px(SIDEBAR_INSET) * 4;
        let min_search_width = 160.min(content_width);
        let collapse_new_button = search_is_active
            || content_width < full_new_button_width + toolbar_gap + min_search_width;
        let new_button_width = if collapse_new_button {
            self.ui_px(SNIPPET_TOOLBAR_HEIGHT).min(content_width)
        } else {
            full_new_button_width.min(content_width)
        };
        let new_button_label = if collapse_new_button {
            ""
        } else {
            "New Snippet"
        };
        self.paint_snippet_button(
            layers,
            ui_font,
            ui_metrics,
            chrome,
            foreground,
            muted_fg,
            content_x,
            toolbar_y,
            new_button_width,
            self.ui_px(SNIPPET_TOOLBAR_HEIGHT),
            Some(SvgIcon::CodeXml),
            new_button_label,
            UIItemType::RightSidebarSnippetNew,
            true,
        )?;

        let search_x = content_x + new_button_width + toolbar_gap;
        let search_y = toolbar_y;
        let search_width = content_x
            .saturating_add(content_width)
            .saturating_sub(search_x);
        if search_width > 28 {
            let search_input = self.right_sidebar_snippet_search.clone();
            self.paint_snippet_text_box(
                layers,
                1,
                ui_font,
                ui_metrics,
                chrome,
                muted_fg,
                search_x,
                search_y,
                search_width,
                self.ui_px(SNIPPET_SEARCH_HEIGHT),
                Some(SvgIcon::Search),
                "Search",
                &search_input,
                self.right_sidebar_snippet_focus == Some(RightSidebarSnippetField::Search),
                UIItemType::RightSidebarSnippetSearch,
                false,
            )?;
        }

        let list_top = toolbar_y
            + self
                .ui_px(SNIPPET_TOOLBAR_HEIGHT)
                .max(self.ui_px(SNIPPET_SEARCH_HEIGHT))
            + self.ui_px(SNIPPET_LIST_TOP_GAP);
        let needle = self
            .right_sidebar_snippet_search
            .text()
            .trim()
            .to_ascii_lowercase();
        let snippets: Vec<_> = crate::snippets::list_snippets()
            .into_iter()
            .filter(|snippet| {
                needle.is_empty()
                    || snippet.title.to_ascii_lowercase().contains(&needle)
                    || snippet.body.to_ascii_lowercase().contains(&needle)
            })
            .collect();
        let row_height = self.ui_px(SNIPPET_CARD_HEIGHT) + self.ui_px(SNIPPET_ROW_GAP);
        let visible_height = content_bottom.saturating_sub(list_top + self.ui_px(SIDEBAR_INSET));
        let total_height = self.right_sidebar_snippet_scroll_height(snippets.len(), visible_height);
        let max_scroll = total_height.saturating_sub(visible_height) as f32;
        self.right_sidebar_snippet_scroll_offset = self
            .right_sidebar_snippet_scroll_offset
            .clamp(0.0, max_scroll);
        let scroll_offset = self.right_sidebar_snippet_scroll_offset;

        if snippets.is_empty() {
            let empty_height = self
                .ui_px(RIGHT_SIDEBAR_EMPTY_HEIGHT)
                .min(content_bottom.saturating_sub(list_top + self.ui_px(SIDEBAR_INSET)));
            if empty_height == 0 {
                return Ok(());
            }
            self.fill_rounded_rectangle_with_border(
                layers,
                1,
                euclid::rect(
                    content_x as f32,
                    list_top as f32,
                    content_width as f32,
                    empty_height as f32,
                ),
                chrome.sidebar_button_bg,
                chrome.control_border,
                self.ui_f32(SIDEBAR_ROW_RADIUS) + 6.0,
                CAPSULE_BORDER_WIDTH,
            )
            .context("right sidebar snippets empty state")?;
            let empty_icon_size = icon_size
                .min(self.ui_px(22))
                .min(empty_height.saturating_sub(20))
                .max(1);
            let empty_icon_x = content_x + self.ui_px(SIDEBAR_INSET) + 2;
            let empty_icon_y = list_top + (empty_height.saturating_sub(empty_icon_size)) / 2;
            self.paint_sidebar_icon(
                layers,
                SvgIcon::CodeXml,
                empty_icon_x,
                empty_icon_y,
                empty_icon_size,
                muted_fg,
            )?;
            self.paint_sidebar_text(
                layers,
                ui_font,
                ui_metrics,
                if needle.is_empty() {
                    "No snippets"
                } else {
                    "No matching snippets"
                },
                empty_icon_x + empty_icon_size + self.ui_px(SIDEBAR_ICON_GAP) + 2,
                list_top + (empty_height.saturating_sub(ui_metrics.cell_size.height as usize)) / 2,
                content_width.saturating_sub(
                    empty_icon_size + self.ui_px(SIDEBAR_ICON_GAP) + self.ui_px(SIDEBAR_INSET) * 3,
                ),
                muted_fg,
            )?;
            return Ok(());
        }

        let list_top_f = list_top as f32;
        let content_bottom = content_bottom.saturating_sub(self.ui_px(SIDEBAR_INSET));
        let content_bottom_f = content_bottom as f32;
        for (idx, snippet) in snippets.into_iter().enumerate() {
            let row_top = list_top_f + (idx * row_height) as f32 - scroll_offset;
            let row_bottom = row_top + self.ui_px(SNIPPET_CARD_HEIGHT) as f32;
            if row_bottom <= list_top_f {
                continue;
            }
            if row_top >= content_bottom_f {
                break;
            }
            let y = row_top.floor().max(0.0) as usize;
            self.paint_snippet_card(
                layers,
                ui_font,
                ui_metrics,
                chrome,
                foreground,
                muted_fg,
                content_x,
                y,
                content_width,
                &snippet,
                list_top,
                content_bottom,
            )?;
        }
        self.paint_right_sidebar_snippet_scrollbar(layers, chrome)?;
        Ok(())
    }

    fn paint_right_sidebar_snippet_scrollbar(
        &mut self,
        layers: &mut TripleLayerQuadAllocator,
        chrome: UiPalette,
    ) -> anyhow::Result<()> {
        if self.right_sidebar_snippet_scrollbar_visible() {
            self.update_next_frame_time(self.right_sidebar_snippet_scrollbar_visible_until);
        }

        if !self.right_sidebar_snippet_scrollbar_visible() {
            return Ok(());
        }

        let Some(scroll) = self.right_sidebar_snippet_scroll_geometry() else {
            return Ok(());
        };

        let track_radius = scroll.track_width as f32 / 2.0;
        self.fill_rounded_rectangle(
            layers,
            2,
            euclid::rect(
                scroll.track_x as f32,
                scroll.track_y as f32,
                scroll.track_width as f32,
                scroll.track_height as f32,
            ),
            chrome.separator,
            track_radius,
        )
        .context("right sidebar snippet scroll track")?;

        self.fill_rounded_rectangle(
            layers,
            2,
            euclid::rect(
                scroll.track_x as f32,
                scroll.thumb_y,
                scroll.track_width as f32,
                scroll.thumb_height,
            ),
            chrome.scrollbar_thumb,
            track_radius,
        )
        .context("right sidebar snippet scroll thumb")?;

        let hit_slop = 6usize;
        self.ui_items.push(UIItem {
            x: scroll.track_x.saturating_sub(hit_slop),
            y: scroll.track_y,
            width: scroll.track_width + hit_slop * 2,
            height: scroll.track_height,
            item_type: UIItemType::RightSidebarSnippetScrollTrack,
        });
        self.ui_items.push(UIItem {
            x: scroll.track_x.saturating_sub(hit_slop),
            y: scroll.thumb_y.round().max(0.0) as usize,
            width: scroll.track_width + hit_slop * 2,
            height: scroll.thumb_height.round().max(1.0) as usize,
            item_type: UIItemType::RightSidebarSnippetScrollThumb,
        });

        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_snippet_editor(
        &mut self,
        layers: &mut TripleLayerQuadAllocator,
        ui_font: &Rc<LoadedFont>,
        ui_metrics: RenderMetrics,
        chrome: UiPalette,
        foreground: LinearRgba,
        muted_fg: LinearRgba,
        content_x: usize,
        content_top: usize,
        content_width: usize,
        content_bottom: usize,
        icon_size: usize,
    ) -> anyhow::Result<()> {
        let header_y = content_top;
        let back_size = self.ui_px(44);
        let header_top = header_y + self.ui_px(8);
        self.paint_snippet_icon_button(
            layers,
            chrome,
            foreground,
            muted_fg,
            content_x,
            header_top,
            back_size,
            SvgIcon::ArrowLeft,
            UIItemType::RightSidebarSnippetBack,
        )?;

        let title_x = content_x + back_size + self.ui_px(SIDEBAR_INSET);
        let save_label_width = self.sidebar_text_width(ui_font, "Save")?.ceil() as usize;
        let save_width = (save_label_width + self.ui_px(SIDEBAR_INSET) * 6)
            .clamp(self.ui_px(110), self.ui_px(136))
            .min(content_width.saturating_sub(back_size + self.ui_px(SIDEBAR_INSET) * 2));
        let save_x = content_x + content_width.saturating_sub(save_width);
        let title_width = save_x.saturating_sub(title_x + self.ui_px(SIDEBAR_INSET) * 2);
        let title_label = match self.right_sidebar_snippet_view {
            RightSidebarSnippetView::EditNew => "New Snippet",
            RightSidebarSnippetView::EditExisting(_) => "Edit Snippet",
            RightSidebarSnippetView::List => "Snippet",
        };
        self.paint_sidebar_text(
            layers,
            ui_font,
            ui_metrics,
            title_label,
            title_x,
            header_top,
            title_width,
            foreground,
        )?;
        self.paint_sidebar_text(
            layers,
            ui_font,
            ui_metrics,
            "Personal vault",
            title_x,
            header_top + ui_metrics.cell_size.height as usize + 8,
            title_width,
            muted_fg,
        )?;

        self.paint_snippet_button(
            layers,
            ui_font,
            ui_metrics,
            chrome,
            foreground,
            muted_fg,
            save_x,
            header_top,
            save_width,
            self.ui_px(SNIPPET_SAVE_BUTTON_HEIGHT)
                .min(self.ui_px(SNIPPET_EDITOR_HEADER_HEIGHT) - 12),
            None,
            "Save",
            UIItemType::RightSidebarSnippetSave,
            true,
        )?;

        let field_label_height = ui_metrics.cell_size.height as usize;
        let title_label_y = header_y + self.ui_px(SNIPPET_EDITOR_HEADER_HEIGHT);
        self.paint_sidebar_text(
            layers,
            ui_font,
            ui_metrics,
            "Action description",
            content_x,
            title_label_y,
            content_width,
            muted_fg,
        )?;
        let title_y = title_label_y + field_label_height + self.ui_px(8);
        let title_input = self.right_sidebar_snippet_title.clone();
        self.paint_snippet_text_box(
            layers,
            1,
            ui_font,
            ui_metrics,
            chrome,
            muted_fg,
            content_x,
            title_y,
            content_width,
            self.ui_px(SNIPPET_FIELD_HEIGHT),
            None,
            "Describe this action",
            &title_input,
            self.right_sidebar_snippet_focus == Some(RightSidebarSnippetField::Title),
            UIItemType::RightSidebarSnippetTitle,
            false,
        )?;

        let body_label_y =
            title_y + self.ui_px(SNIPPET_FIELD_HEIGHT) + self.ui_px(RIGHT_SIDEBAR_SECTION_GAP) + 4;
        self.paint_sidebar_text(
            layers,
            ui_font,
            ui_metrics,
            "Script *",
            content_x,
            body_label_y,
            content_width,
            muted_fg,
        )?;
        let body_y = body_label_y + field_label_height + self.ui_px(8);
        let body_height = self
            .ui_px(SNIPPET_BODY_FIELD_HEIGHT)
            .min(content_bottom.saturating_sub(body_y));
        let body_input = self.right_sidebar_snippet_body.clone();
        self.paint_snippet_text_box(
            layers,
            1,
            ui_font,
            ui_metrics,
            chrome,
            muted_fg,
            content_x,
            body_y,
            content_width,
            body_height,
            None,
            "Type command or script",
            &body_input,
            self.right_sidebar_snippet_focus == Some(RightSidebarSnippetField::Body),
            UIItemType::RightSidebarSnippetBody,
            true,
        )?;

        // Keep icon_size meaningful in this signature for future toolbar actions.
        let _ = icon_size;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_snippet_card(
        &mut self,
        layers: &mut TripleLayerQuadAllocator,
        ui_font: &Rc<LoadedFont>,
        ui_metrics: RenderMetrics,
        chrome: UiPalette,
        foreground: LinearRgba,
        muted_fg: LinearRgba,
        x: usize,
        y: usize,
        width: usize,
        snippet: &crate::snippets::SnippetRecord,
        clip_top: usize,
        clip_bottom: usize,
    ) -> anyhow::Result<()> {
        let card_bottom = y.saturating_add(self.ui_px(SNIPPET_CARD_HEIGHT));
        let visible_y = y.max(clip_top);
        let visible_bottom = card_bottom.min(clip_bottom);
        let visible_height = visible_bottom.saturating_sub(visible_y);
        if visible_height == 0 {
            return Ok(());
        }
        let hovered = self.is_pointer_over_ui_rect(x, visible_y, width, visible_height);
        let fill = if hovered {
            chrome.sidebar_button_hover_bg
        } else {
            chrome.sidebar_button_bg
        };
        self.fill_rounded_rectangle_with_border(
            layers,
            1,
            euclid::rect(
                x as f32,
                visible_y as f32,
                width as f32,
                visible_height as f32,
            ),
            fill,
            if hovered {
                chrome.control_border
            } else {
                chrome.control_border.mul_alpha(0.72)
            },
            self.ui_f32(SIDEBAR_ROW_RADIUS) + 10.0,
            CAPSULE_BORDER_WIDTH,
        )
        .context("right sidebar snippet card")?;
        self.ui_items.push(UIItem {
            x,
            y: visible_y,
            width,
            height: visible_height,
            item_type: UIItemType::RightSidebarSnippetEdit(snippet.id.clone()),
        });

        let card_pad = self.ui_px(SIDEBAR_INSET) * 2;
        let text_x = x + card_pad;
        let run_button_width = (self.sidebar_text_width(ui_font, "Run")?.ceil() as usize
            + self.ui_px(SIDEBAR_INSET) * 4)
            .max(self.ui_px(SNIPPET_ACTION_BUTTON_MIN_WIDTH));
        let paste_button_width = (self.sidebar_text_width(ui_font, "Paste")?.ceil() as usize
            + self.ui_px(SIDEBAR_INSET) * 4)
            .max(self.ui_px(SNIPPET_ACTION_BUTTON_MIN_WIDTH));
        let delete_button_size = self.ui_px(SNIPPET_ACTION_BUTTON_HEIGHT);
        let action_area_width = if hovered {
            run_button_width
                + paste_button_width
                + delete_button_size
                + self.ui_px(SIDEBAR_INSET) * 2
        } else {
            0
        };
        let title_y = y + self.ui_px(SIDEBAR_INSET) * 2;
        if title_y >= clip_top && title_y < clip_bottom {
            self.paint_sidebar_text(
                layers,
                ui_font,
                ui_metrics,
                &snippet.title,
                text_x,
                title_y,
                width.saturating_sub(card_pad * 2 + action_area_width),
                foreground,
            )?;
        }
        let preview = snippet_preview(&snippet.body);
        let preview_y =
            y + self.ui_px(SIDEBAR_INSET) * 2 + ui_metrics.cell_size.height as usize + 8;
        if preview_y >= clip_top && preview_y < clip_bottom {
            self.paint_sidebar_text(
                layers,
                ui_font,
                ui_metrics,
                &preview,
                text_x,
                preview_y,
                width.saturating_sub(card_pad * 2),
                muted_fg,
            )?;
        }

        let fully_visible = y >= clip_top && card_bottom <= clip_bottom;
        if hovered && fully_visible {
            let delete_x = x + width.saturating_sub(card_pad + delete_button_size);
            let paste_x = delete_x.saturating_sub(self.ui_px(SIDEBAR_INSET) + paste_button_width);
            let run_x = paste_x.saturating_sub(self.ui_px(SIDEBAR_INSET) + run_button_width);
            let action_y = y + self.ui_px(SIDEBAR_INSET) * 2 - 4;
            self.paint_snippet_button(
                layers,
                ui_font,
                ui_metrics,
                chrome,
                foreground,
                muted_fg,
                run_x,
                action_y,
                run_button_width,
                self.ui_px(SNIPPET_ACTION_BUTTON_HEIGHT),
                None,
                "Run",
                UIItemType::RightSidebarSnippetRun(snippet.id.clone()),
                true,
            )?;
            self.paint_snippet_button(
                layers,
                ui_font,
                ui_metrics,
                chrome,
                foreground,
                muted_fg,
                paste_x,
                action_y,
                paste_button_width,
                self.ui_px(SNIPPET_ACTION_BUTTON_HEIGHT),
                None,
                "Paste",
                UIItemType::RightSidebarSnippetPaste(snippet.id.clone()),
                true,
            )?;
            self.paint_snippet_icon_button(
                layers,
                chrome,
                foreground,
                muted_fg,
                delete_x,
                action_y,
                delete_button_size,
                SvgIcon::Trash2,
                UIItemType::RightSidebarSnippetDelete(snippet.id.clone()),
            )?;
        }

        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_snippet_text_box(
        &mut self,
        layers: &mut TripleLayerQuadAllocator,
        layer_num: usize,
        ui_font: &Rc<LoadedFont>,
        ui_metrics: RenderMetrics,
        chrome: UiPalette,
        muted_fg: LinearRgba,
        x: usize,
        y: usize,
        width: usize,
        height: usize,
        icon: Option<SvgIcon>,
        placeholder: &str,
        input: &TextInputState,
        focused: bool,
        item_type: UIItemType,
        multiline: bool,
    ) -> anyhow::Result<()> {
        if width == 0 || height == 0 {
            return Ok(());
        }
        let hovered = self.is_pointer_over_ui_rect(x, y, width, height);
        let is_search_field = matches!(
            &item_type,
            UIItemType::RightSidebarSnippetSearch | UIItemType::RightSidebarFileFilter
        );
        self.fill_rounded_rectangle_with_border(
            layers,
            layer_num,
            euclid::rect(x as f32, y as f32, width as f32, height as f32),
            chrome.control_bg,
            if focused {
                chrome.selected_bg
            } else if hovered {
                chrome.separator
            } else {
                chrome.control_border
            },
            if is_search_field {
                WINDOW_TAB_ADD_BUTTON_RADIUS
            } else {
                self.ui_f32(SIDEBAR_ROW_RADIUS) + 4.0
            },
            CAPSULE_BORDER_WIDTH,
        )
        .context("right sidebar snippet text box")?;
        self.ui_items.push(UIItem {
            x,
            y,
            width,
            height,
            item_type: item_type.clone(),
        });

        let text_pad = self.ui_px(SIDEBAR_INSET) + 2;
        let mut text_x = x + text_pad;
        if let Some(icon) = icon {
            let icon_size = (ui_metrics.cell_size.height as usize + self.ui_px(2))
                .clamp(self.ui_px(18), self.ui_px(22));
            self.paint_sidebar_icon(
                layers,
                icon,
                text_x,
                y + (height.saturating_sub(icon_size)) / 2,
                icon_size,
                muted_fg,
            )?;
            text_x += icon_size + self.ui_px(SIDEBAR_ICON_GAP);
        }

        let text_color = if input.text().is_empty() && !focused {
            muted_fg.mul_alpha(0.72)
        } else {
            chrome.text
        };
        let text = if input.text().is_empty() && !focused {
            placeholder
        } else {
            input.text()
        };
        if multiline {
            let line_height = ui_metrics.cell_size.height as usize + 4;
            let max_lines =
                height.saturating_sub(self.ui_px(SIDEBAR_INSET) * 2).max(1) / line_height.max(1);
            let mut line_y = y + self.ui_px(SIDEBAR_INSET) + 2;
            let text_width = width.saturating_sub((text_x - x) + text_pad);
            let visible_lines = wrap_snippet_text_for_width(text, max_lines.max(1), focused, |s| {
                self.sidebar_text_width(ui_font, s).unwrap_or(f32::MAX) / text_width.max(1) as f32
            });
            let mut last_line = visible_lines.last().map(String::as_str).unwrap_or("");
            let mut last_line_y = line_y;
            for line in &visible_lines {
                last_line = line;
                last_line_y = line_y;
                self.paint_ui_title_text(
                    layers,
                    ui_font,
                    &ui_metrics,
                    line,
                    text_x,
                    line_y,
                    text_width,
                    text_color,
                )?;
                line_y += line_height;
            }
            if focused && self.right_sidebar_snippet_cursor_on() {
                let caret_x = text_x
                    + (self.sidebar_text_width(ui_font, last_line)?.ceil() as usize)
                        .min(width.saturating_sub((text_x - x) + text_pad));
                self.filled_rectangle(
                    layers,
                    2,
                    euclid::rect(
                        caret_x as f32,
                        last_line_y as f32,
                        self.ui_f32(SNIPPET_CARET_WIDTH),
                        (ui_metrics.cell_size.height as f32).max(1.0),
                    ),
                    chrome.text,
                )
                .context("right sidebar snippet body caret")?;
            }
        } else {
            let text_area_width = width.saturating_sub((text_x - x) + text_pad);
            let baseline_y = y + (height.saturating_sub(ui_metrics.cell_size.height as usize)) / 2;

            if input.text().is_empty() && !focused {
                self.paint_sidebar_text(
                    layers,
                    ui_font,
                    ui_metrics,
                    placeholder,
                    text_x,
                    baseline_y,
                    text_area_width,
                    text_color,
                )?;
            } else {
                let chars: Vec<char> = input.text().chars().collect();
                let cursor = input.cursor.min(chars.len());
                let avail = text_area_width as f32;
                let caret_margin = self.ui_f32(SNIPPET_CARET_WIDTH) + 2.0;

                // Horizontal scroll: push the first visible char forward until
                // the caret is back inside the field.
                let mut first = 0usize;
                if focused {
                    while first < cursor {
                        let prefix: String = chars[first..cursor].iter().collect();
                        let prefix_w = self.sidebar_text_width(ui_font, &prefix)?;
                        if prefix_w <= (avail - caret_margin).max(0.0) {
                            break;
                        }
                        first += 1;
                    }
                }

                // Selection highlight, behind the glyphs.
                if let Some((sel_start, sel_end)) = input.caret_selection_range() {
                    let vis_start = sel_start.max(first).min(chars.len());
                    let vis_end = sel_end.max(first).min(chars.len());
                    if vis_end > vis_start {
                        let start_prefix: String = chars[first..vis_start].iter().collect();
                        let end_prefix: String = chars[first..vis_end].iter().collect();
                        let start_w = self.sidebar_text_width(ui_font, &start_prefix)?;
                        let end_w = self.sidebar_text_width(ui_font, &end_prefix)?.min(avail);
                        let sel_w = (end_w - start_w).max(0.0);
                        if sel_w > 0.0 {
                            self.filled_rectangle(
                                layers,
                                2,
                                euclid::rect(
                                    text_x as f32 + start_w,
                                    baseline_y as f32 - 2.0,
                                    sel_w,
                                    ui_metrics.cell_size.height as f32 + self.ui_f32(4.0),
                                ),
                                chrome.selected_bg.mul_alpha(0.55),
                            )
                            .context("right sidebar input selection")?;
                        }
                    }
                }

                // Visible text, clipped to the field (no ellipsis).
                let visible: String = chars[first..].iter().collect();
                self.paint_ui_title_text(
                    layers,
                    ui_font,
                    &ui_metrics,
                    &visible,
                    text_x,
                    baseline_y,
                    text_area_width,
                    text_color,
                )?;

                // Caret (hidden while a selection is active).
                if focused
                    && input.caret_selection_range().is_none()
                    && self.right_sidebar_snippet_cursor_on()
                {
                    let prefix: String = chars[first..cursor].iter().collect();
                    let caret_offset = self.sidebar_text_width(ui_font, &prefix)?.min(avail);
                    self.filled_rectangle(
                        layers,
                        2,
                        euclid::rect(
                            text_x as f32 + caret_offset,
                            baseline_y as f32,
                            self.ui_f32(SNIPPET_CARET_WIDTH),
                            (ui_metrics.cell_size.height as f32).max(1.0),
                        ),
                        chrome.text,
                    )
                    .context("right sidebar input caret")?;
                }

                self.right_sidebar_input_layouts
                    .push(RightSidebarInputLayout {
                        item_type: item_type.clone(),
                        text_x: text_x as f32,
                        text_width: text_area_width as f32,
                        first_char: first,
                        font: ui_font.clone(),
                    });
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_snippet_button(
        &mut self,
        layers: &mut TripleLayerQuadAllocator,
        ui_font: &Rc<LoadedFont>,
        ui_metrics: RenderMetrics,
        chrome: UiPalette,
        foreground: LinearRgba,
        muted_fg: LinearRgba,
        x: usize,
        y: usize,
        width: usize,
        height: usize,
        icon: Option<SvgIcon>,
        label: &str,
        item_type: UIItemType,
        enabled: bool,
    ) -> anyhow::Result<()> {
        if width == 0 || height == 0 {
            return Ok(());
        }
        let hovered = enabled && self.is_pointer_over_ui_rect(x, y, width, height);
        let pressed = hovered && self.is_pointer_pressing_ui_rect(x, y, width, height);
        let fill = if pressed {
            chrome.control_pressed_bg
        } else if hovered {
            chrome.control_hover_bg
        } else {
            chrome.sidebar_button_bg
        };
        self.fill_rounded_rectangle_with_border(
            layers,
            1,
            euclid::rect(x as f32, y as f32, width as f32, height as f32),
            fill,
            if hovered {
                chrome.control_border
            } else {
                chrome.control_border.mul_alpha(0.74)
            },
            WINDOW_TAB_ADD_BUTTON_RADIUS,
            CAPSULE_BORDER_WIDTH,
        )
        .context("right sidebar snippet button")?;
        if enabled {
            self.ui_items.push(UIItem {
                x,
                y,
                width,
                height,
                item_type,
            });
        }

        let icon_size = icon
            .map(|_| {
                (ui_metrics.cell_size.height as usize + self.ui_px(2))
                    .clamp(self.ui_px(18), self.ui_px(22))
            })
            .unwrap_or(0);
        let measured_text_width = self.sidebar_text_width(ui_font, label)?.ceil() as usize;
        let icon_label_gap = if icon.is_some() && measured_text_width > 0 {
            self.ui_px(SIDEBAR_ICON_GAP)
        } else {
            0
        };
        let intrinsic_width = icon_size + icon_label_gap + measured_text_width;
        let horizontal_pad =
            (self.ui_px(SIDEBAR_INSET) * 2).min(width.saturating_sub(intrinsic_width) / 2);
        let available_label_width =
            width.saturating_sub(horizontal_pad * 2 + icon_size + icon_label_gap);
        let text_width = measured_text_width.min(available_label_width);
        let total_width = icon_size + icon_label_gap + text_width;
        let start_x = x + width.saturating_sub(total_width) / 2;
        let color = if enabled { foreground } else { muted_fg };
        let mut text_x = start_x;
        if let Some(icon) = icon {
            self.paint_sidebar_icon(
                layers,
                icon,
                text_x,
                y + (height.saturating_sub(icon_size)) / 2,
                icon_size,
                color,
            )?;
            text_x += icon_size + icon_label_gap;
        }
        self.paint_sidebar_text(
            layers,
            ui_font,
            ui_metrics,
            label,
            text_x,
            y + (height.saturating_sub(ui_metrics.cell_size.height as usize)) / 2,
            text_width,
            color,
        )
    }

    fn paint_snippet_icon_button(
        &mut self,
        layers: &mut TripleLayerQuadAllocator,
        chrome: UiPalette,
        foreground: LinearRgba,
        muted_fg: LinearRgba,
        x: usize,
        y: usize,
        size: usize,
        icon: SvgIcon,
        item_type: UIItemType,
    ) -> anyhow::Result<()> {
        let hovered = self.is_pointer_over_ui_rect(x, y, size, size);
        if hovered {
            self.fill_rounded_rectangle(
                layers,
                1,
                euclid::rect(x as f32, y as f32, size as f32, size as f32),
                chrome.control_hover_bg,
                WINDOW_TAB_ADD_BUTTON_RADIUS,
            )
            .context("right sidebar snippet icon button hover")?;
        }
        self.ui_items.push(UIItem {
            x,
            y,
            width: size,
            height: size,
            item_type,
        });
        let icon_size = (size * 58 / 100).max(self.ui_px(16));
        self.paint_sidebar_icon(
            layers,
            icon,
            x + (size.saturating_sub(icon_size)) / 2,
            y + (size.saturating_sub(icon_size)) / 2,
            icon_size,
            if hovered { foreground } else { muted_fg },
        )
    }
}

fn path_key(path: &Path) -> String {
    path.to_string_lossy().to_string()
}

fn file_name_for_path(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| path.to_string_lossy().to_string())
}

#[cfg(test)]
fn preview_lines_from_text(
    path: &Path,
    text: &str,
    use_dark_theme: bool,
) -> Vec<RightSidebarFilePreviewLine> {
    preview_lines_from_text_with_cancellation(path, text, use_dark_theme, None)
}

fn preview_lines_from_text_with_cancellation(
    path: &Path,
    text: &str,
    use_dark_theme: bool,
    cancellation: Option<&std::sync::atomic::AtomicUsize>,
) -> Vec<RightSidebarFilePreviewLine> {
    if cancellation
        .map(|token| token.load(AtomicOrdering::Relaxed) != 0)
        .unwrap_or(false)
    {
        return Vec::new();
    }
    let mut highlight_end = text
        .len()
        .min(thinkterm_syntax::DEFAULT_HIGHLIGHT_BYTE_LIMIT);
    while !text.is_char_boundary(highlight_end) {
        highlight_end = highlight_end.saturating_sub(1);
    }
    let highlight_source = &text[..highlight_end];
    let highlighted = match syntax_highlight_pool()
        .install(|| thinkterm_syntax::highlight_path(path, highlight_source, cancellation))
    {
        Ok(Some(result)) => result,
        Ok(None) => {
            if cancellation
                .map(|token| token.load(AtomicOrdering::Relaxed) != 0)
                .unwrap_or(false)
            {
                return Vec::new();
            }
            return preview_plain_lines_from_text(text);
        }
        Err(err) => {
            let was_cancelled = cancellation
                .map(|token| token.load(AtomicOrdering::Relaxed) != 0)
                .unwrap_or(false);
            if !was_cancelled {
                log::warn!("failed to highlight file preview: {err:#}");
            } else {
                return Vec::new();
            }
            return preview_plain_lines_from_text(text);
        }
    };

    highlighted_preview_lines(text, &highlighted.spans, use_dark_theme)
}

fn syntax_highlight_pool() -> &'static rayon::ThreadPool {
    static POOL: OnceLock<rayon::ThreadPool> = OnceLock::new();
    POOL.get_or_init(|| {
        rayon::ThreadPoolBuilder::new()
            .num_threads(2)
            .thread_name(|index| format!("thinkterm-syntax-{index}"))
            .build()
            .expect("build ThinkTerm syntax highlighting pool")
    })
}

fn note_code_line_count(text: &str) -> usize {
    if text.is_empty() {
        1
    } else {
        text.split_inclusive('\n').count()
    }
}

fn note_code_syntax_result(
    code: &ProjectedCodeBlock,
    cancellation: Option<&AtomicUsize>,
) -> Option<HighlightResult> {
    let Some(language) = code.language.as_deref() else {
        return None;
    };
    let Some(language) = thinkterm_syntax::detect_fence(language) else {
        return None;
    };
    if code.text.len() > thinkterm_syntax::DEFAULT_HIGHLIGHT_BYTE_LIMIT {
        return None;
    }
    match thinkterm_syntax::highlight(language, &code.text, cancellation) {
        Ok(result) => Some(result),
        Err(err) => {
            let was_cancelled = cancellation
                .map(|token| token.load(AtomicOrdering::Relaxed) != 0)
                .unwrap_or(false);
            if !was_cancelled {
                log::warn!("failed to highlight Note code block: {err:#}");
            }
            None
        }
    }
}

#[cfg(test)]
fn note_code_highlight_lines(
    code: &ProjectedCodeBlock,
    use_dark_theme: bool,
) -> Vec<Vec<LinearRgba>> {
    let Some(syntax_result) = note_code_syntax_result(code, None) else {
        return vec![vec![]; note_code_line_count(&code.text)];
    };
    note_code_colors(code, &syntax_result, use_dark_theme)
}

fn note_code_highlight_pair(
    code: &ProjectedCodeBlock,
    cancellation: Option<&AtomicUsize>,
) -> Option<(Vec<Vec<LinearRgba>>, Vec<Vec<LinearRgba>>)> {
    let syntax_result = note_code_syntax_result(code, cancellation)?;
    Some((
        note_code_colors(code, &syntax_result, false),
        note_code_colors(code, &syntax_result, true),
    ))
}

fn note_code_colors(
    code: &ProjectedCodeBlock,
    syntax_result: &HighlightResult,
    use_dark_theme: bool,
) -> Vec<Vec<LinearRgba>> {
    let raw_lines = if code.text.is_empty() {
        vec![""]
    } else {
        code.text.split_inclusive('\n').collect::<Vec<_>>()
    };

    let default_color = syntax_default_color(use_dark_theme);
    let mut lines = Vec::with_capacity(raw_lines.len());
    let mut source_offset = 0usize;
    let mut span_index = 0usize;
    for raw_line in raw_lines {
        let line = raw_line.strip_suffix('\n').unwrap_or(raw_line);
        let line_end = source_offset.saturating_add(line.len());
        let mut colors = vec![default_color; line.len()];
        while span_index < syntax_result.spans.len()
            && syntax_result.spans[span_index].range.end <= source_offset
        {
            span_index += 1;
        }
        let mut index = span_index;
        while let Some(span) = syntax_result.spans.get(index) {
            if span.range.start >= line_end {
                break;
            }
            let start = span
                .range
                .start
                .max(source_offset)
                .saturating_sub(source_offset);
            let end = span.range.end.min(line_end).saturating_sub(source_offset);
            if start < end && end <= colors.len() {
                colors[start..end].fill(syntax_color(span.kind, use_dark_theme));
            }
            index += 1;
        }
        lines.push(colors);
        source_offset = source_offset.saturating_add(raw_line.len());
    }
    lines
}

fn preview_plain_lines_from_text(text: &str) -> Vec<RightSidebarFilePreviewLine> {
    if text.is_empty() {
        vec![preview_line_from_plain("")]
    } else {
        text.lines().map(preview_line_from_plain).collect()
    }
}

fn preview_line_from_plain(line: &str) -> RightSidebarFilePreviewLine {
    RightSidebarFilePreviewLine {
        plain: line.to_string(),
        char_count: line.chars().count(),
        spans: Vec::new(),
    }
}

fn highlighted_preview_lines(
    text: &str,
    highlights: &[HighlightSpan],
    use_dark_theme: bool,
) -> Vec<RightSidebarFilePreviewLine> {
    if text.is_empty() {
        return vec![preview_line_from_plain("")];
    }

    let default_color = syntax_default_color(use_dark_theme);
    let mut lines = Vec::new();
    let mut source_offset = 0usize;
    let mut span_index = 0usize;
    for raw_line in text.split_inclusive('\n') {
        let without_newline = raw_line.strip_suffix('\n').unwrap_or(raw_line);
        let line = without_newline
            .strip_suffix('\r')
            .unwrap_or(without_newline);
        let line_end = source_offset.saturating_add(line.len());
        while span_index < highlights.len() && highlights[span_index].range.end <= source_offset {
            span_index += 1;
        }

        let mut cursor = source_offset;
        let mut spans = Vec::new();
        let mut index = span_index;
        while let Some(span) = highlights.get(index) {
            if span.range.start >= line_end {
                break;
            }
            let start = span
                .range
                .start
                .max(cursor)
                .max(source_offset)
                .min(line_end);
            let end = span.range.end.min(line_end);
            if cursor < start {
                push_preview_span(&mut spans, &text[cursor..start], default_color);
            }
            if start < end {
                push_preview_span(
                    &mut spans,
                    &text[start..end],
                    syntax_color(span.kind, use_dark_theme),
                );
                cursor = end;
            }
            index += 1;
        }
        if cursor < line_end {
            push_preview_span(&mut spans, &text[cursor..line_end], default_color);
        }

        lines.push(RightSidebarFilePreviewLine {
            plain: line.to_string(),
            char_count: line.chars().count(),
            spans,
        });
        source_offset = source_offset.saturating_add(raw_line.len());
    }
    lines
}

fn push_preview_span(spans: &mut Vec<RightSidebarFilePreviewSpan>, text: &str, color: LinearRgba) {
    if text.is_empty() {
        return;
    }
    let char_count = text.chars().count();
    if let Some(previous) = spans.last_mut() {
        if previous.color == color {
            previous.text.push_str(text);
            previous.char_count = previous.char_count.saturating_add(char_count);
            return;
        }
    }
    spans.push(RightSidebarFilePreviewSpan {
        text: text.to_string(),
        char_count,
        color,
    });
}

fn syntax_default_color(use_dark_theme: bool) -> LinearRgba {
    if use_dark_theme {
        LinearRgba::with_srgba(0xab, 0xb2, 0xbf, 0xff)
    } else {
        LinearRgba::with_srgba(0x38, 0x3a, 0x42, 0xff)
    }
}

fn syntax_color(kind: HighlightKind, use_dark_theme: bool) -> LinearRgba {
    let (red, green, blue) = if use_dark_theme {
        match kind {
            HighlightKind::Comment => (0x7f, 0x84, 0x8e),
            HighlightKind::Keyword | HighlightKind::Label => (0xc6, 0x78, 0xdd),
            HighlightKind::String => (0x98, 0xc3, 0x79),
            HighlightKind::Number | HighlightKind::Constant => (0xd1, 0x9a, 0x66),
            HighlightKind::Type | HighlightKind::Module => (0xe5, 0xc0, 0x7b),
            HighlightKind::Function | HighlightKind::Constructor => (0x61, 0xaf, 0xef),
            HighlightKind::Property | HighlightKind::Attribute | HighlightKind::Tag => {
                (0xe0, 0x6c, 0x75)
            }
            HighlightKind::Variable
            | HighlightKind::Embedded
            | HighlightKind::Operator
            | HighlightKind::Punctuation => (0xab, 0xb2, 0xbf),
        }
    } else {
        match kind {
            HighlightKind::Comment => (0x6a, 0x73, 0x7d),
            HighlightKind::Keyword | HighlightKind::Label => (0xa6, 0x26, 0xa4),
            HighlightKind::String => (0x50, 0xa1, 0x4f),
            HighlightKind::Number | HighlightKind::Constant => (0x98, 0x68, 0x01),
            HighlightKind::Type | HighlightKind::Module => (0xc1, 0x84, 0x01),
            HighlightKind::Function | HighlightKind::Constructor => (0x40, 0x78, 0xf2),
            HighlightKind::Property | HighlightKind::Attribute | HighlightKind::Tag => {
                (0xe4, 0x56, 0x49)
            }
            HighlightKind::Variable
            | HighlightKind::Embedded
            | HighlightKind::Operator
            | HighlightKind::Punctuation => (0x38, 0x3a, 0x42),
        }
    };
    LinearRgba::with_srgba(red, green, blue, 0xff)
}

fn preview_line_count(lines: &[RightSidebarFilePreviewLine]) -> usize {
    if lines.is_empty() {
        1
    } else {
        lines.len()
    }
}

fn decimal_digit_count(mut value: usize) -> usize {
    let mut digits = 1;
    while value >= 10 {
        value /= 10;
        digits += 1;
    }
    digits
}

fn file_preview_line_number_width(
    number_digits: usize,
    cell_width: usize,
    body_width: usize,
    icon_gap: usize,
) -> usize {
    number_digits
        .saturating_mul(cell_width.max(1))
        .saturating_add(icon_gap * 2)
        .max(36)
        .min(body_width / 2)
}

fn estimated_file_preview_visible_columns(text_width: usize, cell_width: usize) -> usize {
    text_width / cell_width.max(1)
}

/// Per-byte colours for a whole highlighted line: `colors[b]` is the colour of
/// the character that byte `b` of `line.plain` belongs to. Spans concatenate to
/// `line.plain`, so this aligns with a glyph's `cluster` (a byte offset) for the
/// full-line horizontal-scroll fast path. Plain lines have no spans and use a
/// uniform foreground instead.
fn full_line_colors_by_byte(line: &RightSidebarFilePreviewLine) -> Vec<LinearRgba> {
    let mut colors = Vec::with_capacity(line.plain.len());
    for span in &line.spans {
        for ch in span.text.chars() {
            for _ in 0..ch.len_utf8() {
                colors.push(span.color);
            }
        }
    }
    colors
}

/// Collect the visible horizontal window of a highlighted line into a single
/// string plus a parallel per-character colour list, so the whole window can be
/// shaped in one call. `start_column`/`max_columns` are in characters.
fn preview_visible_colored(
    line: &RightSidebarFilePreviewLine,
    start_column: usize,
    max_columns: usize,
) -> (String, Vec<LinearRgba>) {
    let mut text = String::new();
    let mut colors = Vec::new();
    let mut skip = start_column;
    let mut remaining = max_columns;
    for span in &line.spans {
        if remaining == 0 {
            break;
        }
        let span_columns = span.char_count;
        if skip >= span_columns {
            skip -= span_columns;
            continue;
        }
        for ch in span.text.chars().skip(skip) {
            if remaining == 0 {
                break;
            }
            text.push(ch);
            colors.push(span.color);
            remaining -= 1;
        }
        skip = 0;
    }
    (text, colors)
}

fn preview_text_slice(text: &str, start_column: usize, max_columns: usize) -> Cow<'_, str> {
    if text.is_empty() || max_columns == 0 {
        return Cow::Borrowed("");
    }

    let start_byte = text
        .char_indices()
        .nth(start_column)
        .map(|(idx, _)| idx)
        .unwrap_or(text.len());
    if start_byte >= text.len() {
        return Cow::Borrowed("");
    }

    let end_byte = text[start_byte..]
        .char_indices()
        .nth(max_columns)
        .map(|(idx, _)| start_byte + idx)
        .unwrap_or(text.len());
    if start_byte == 0 && end_byte == text.len() {
        Cow::Borrowed(text)
    } else {
        Cow::Owned(text[start_byte..end_byte].to_string())
    }
}

fn preview_text_range(text: &str, start_column: usize, end_column: usize) -> Cow<'_, str> {
    if end_column <= start_column {
        Cow::Borrowed("")
    } else {
        preview_text_slice(text, start_column, end_column - start_column)
    }
}

fn preview_visible_line_range(
    line_count: usize,
    first_line: usize,
    max_lines: usize,
) -> std::ops::Range<usize> {
    if line_count == 0 || max_lines == 0 {
        return 0..0;
    }
    let start = first_line.min(line_count);
    let end = start.saturating_add(max_lines).min(line_count);
    start..end
}

fn right_sidebar_file_row_metrics(ui_metrics: RenderMetrics) -> RightSidebarFileRowMetrics {
    // Font metrics already follow the window DPI. Derive the row chrome from
    // those metrics so the file tree keeps the same logical size across
    // displays instead of being pinned by physical-pixel clamps.
    let cell_height = (ui_metrics.cell_size.height as usize).max(1);
    let row_height = (cell_height * 17 / 10).max(cell_height);
    let icon_size = (cell_height * 13 / 10)
        .min(row_height.saturating_sub(1))
        .max(1);
    let chevron_size = (icon_size * 72 / 100).max(1);
    let indent_step = (icon_size * 58 / 100).max(1);
    let icon_gap = (icon_size / 3).max(1);
    RightSidebarFileRowMetrics {
        row_height,
        icon_size,
        chevron_size,
        indent_step,
        icon_gap,
    }
}

fn right_sidebar_open_with_cache_key(path: &Path) -> String {
    path.extension()
        .map(|extension| format!("ext:{}", extension.to_string_lossy().to_lowercase()))
        .unwrap_or_else(|| format!("path:{}", path.to_string_lossy()))
}

const OPEN_WITH_DEV_TOOL_NEEDLES: &[&str] = &[
    "zed",
    "visual studio code",
    "vscode",
    "code",
    "cursor",
    "xcode",
    "android studio",
    "sublime",
    "webstorm",
    "intellij",
    "pycharm",
    "goland",
    "rustrover",
    "vim",
    "neovim",
    "emacs",
];

const OPEN_WITH_PRODUCTIVITY_NEEDLES: &[&str] = &[
    "excel",
    "numbers",
    "pages",
    "keynote",
    "word",
    "powerpoint",
    "preview",
    "textedit",
    "typora",
    "obsidian",
    "libreoffice",
    "onlyoffice",
    "okular",
    "evince",
];

/// Match allowlist needles against the app NAME only: matching the id/path
/// lets junk through (e.g. Instruments.app matches "xcode" merely because it
/// lives inside Xcode.app).
fn open_with_candidate_text(candidate: &wezterm_open_url::OpenWithCandidate) -> String {
    candidate.label.to_lowercase()
}

/// Candidates living in hidden directories (~/.cache tool runtimes and the
/// like) or nested inside another bundle (Xcode's Instruments, calibre's
/// viewer) are implementation details, not apps the user chose to install.
fn open_with_candidate_in_junk_location(candidate: &wezterm_open_url::OpenWithCandidate) -> bool {
    let id = &candidate.id;
    if !id.starts_with('/') {
        // Not an absolute path (e.g. Linux `desktop:` ids): no location info
        return false;
    }
    id.contains(".app/") || id.split('/').any(|component| component.starts_with('.'))
}

/// The Open With menu is curated: the system default, well-known editors /
/// office apps, the saved preference and the user's own additions. Every
/// other registered handler is noise and stays hidden.
fn open_with_candidate_allowed(
    candidate: &wezterm_open_url::OpenWithCandidate,
    custom_ids: &std::collections::HashSet<String>,
    saved_id: Option<&str>,
) -> bool {
    // Explicit user choices bypass the location heuristics
    if custom_ids.contains(&candidate.id) || saved_id == Some(candidate.id.as_str()) {
        return true;
    }
    if open_with_candidate_in_junk_location(candidate) {
        return false;
    }
    if candidate.is_default {
        return true;
    }
    let text = open_with_candidate_text(candidate);
    OPEN_WITH_DEV_TOOL_NEEDLES
        .iter()
        .chain(OPEN_WITH_PRODUCTIVITY_NEEDLES.iter())
        .any(|needle| text.contains(needle))
}

fn sorted_open_with_candidates(
    mut candidates: Vec<wezterm_open_url::OpenWithCandidate>,
    preferred_candidate_id: Option<&str>,
) -> Vec<wezterm_open_url::OpenWithCandidate> {
    candidates.sort_by(|a, b| {
        let a_preferred = preferred_candidate_id == Some(a.id.as_str());
        let b_preferred = preferred_candidate_id == Some(b.id.as_str());
        b_preferred
            .cmp(&a_preferred)
            .then_with(|| open_with_candidate_rank(a).cmp(&open_with_candidate_rank(b)))
            .then_with(|| a.id.cmp(&b.id))
    });
    candidates.truncate(10);
    candidates
}

fn open_with_candidate_rank(candidate: &wezterm_open_url::OpenWithCandidate) -> (u8, String) {
    let text = open_with_candidate_text(candidate);
    let rank = if OPEN_WITH_DEV_TOOL_NEEDLES
        .iter()
        .any(|needle| text.contains(needle))
    {
        0
    } else if candidate.is_default {
        1
    } else if OPEN_WITH_PRODUCTIVITY_NEEDLES
        .iter()
        .any(|needle| text.contains(needle))
    {
        2
    } else {
        3
    };
    (rank, candidate.label.to_lowercase())
}

/// Where a tree row lands once the viewport is scrolled, or that it is outside
/// the viewport entirely.
#[derive(Debug, Clone, Copy, PartialEq)]
enum FileRowPlacement {
    /// Scrolled off the top; skip it and keep going.
    Above,
    /// Paint at this y. May sit above `tree_top` when the row is only partly
    /// scrolled out — the row painters clip against `tree_top`/`viewport_bottom`
    /// themselves, and they need the *true* origin to do it. Clamping the origin
    /// instead keeps a full row height below `tree_top`, which overlaps the next
    /// row by the scroll remainder and drags an oversized hit rect along.
    Visible(f32),
    /// Past the bottom; every later row is too, so the caller can stop.
    Below,
}

fn file_row_placement(
    index: usize,
    row_height: usize,
    tree_top: f32,
    viewport_bottom: f32,
    scroll: f32,
) -> FileRowPlacement {
    let row_top = tree_top + (index * row_height) as f32 - scroll;
    if row_top + row_height as f32 <= tree_top {
        FileRowPlacement::Above
    } else if row_top >= viewport_bottom {
        FileRowPlacement::Below
    } else {
        FileRowPlacement::Visible(row_top)
    }
}

fn visible_file_row_range(
    row_count: usize,
    scroll_offset: f32,
    visible_height: usize,
    row_height: usize,
) -> std::ops::Range<usize> {
    if row_count == 0 || visible_height == 0 || row_height == 0 {
        return 0..0;
    }

    let row_height = row_height as f32;
    let start = (scroll_offset / row_height).floor().max(0.0) as usize;
    let end = ((scroll_offset + visible_height as f32) / row_height)
        .ceil()
        .max(0.0) as usize
        + 1;
    start.min(row_count)..end.min(row_count)
}

fn load_right_sidebar_file_preview_with_cancellation(
    path: &Path,
    use_dark_syntax_theme: bool,
    cancellation: Option<&AtomicUsize>,
) -> RightSidebarLoadedFilePreview {
    if is_preview_image_path(path) {
        return match load_file_preview_image(path) {
            Ok(image) => RightSidebarLoadedFilePreview {
                lines: Vec::new(),
                image: Some(image),
                message: None,
                truncated: false,
            },
            Err(err) => RightSidebarLoadedFilePreview {
                lines: Vec::new(),
                image: None,
                message: Some(format!("Unable to load image preview: {err}")),
                truncated: false,
            },
        };
    }

    let (text, message, truncated) = load_file_preview(path);
    let lines = if message.is_none() {
        preview_lines_from_text_with_cancellation(path, &text, use_dark_syntax_theme, cancellation)
    } else {
        Vec::new()
    };
    RightSidebarLoadedFilePreview {
        lines,
        image: None,
        message,
        truncated,
    }
}

fn is_preview_image_path(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|ext| ext.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "ico" | "tif" | "tiff")
    )
}

fn is_preview_image_extension(extension: Option<&str>) -> bool {
    matches!(
        extension.map(str::to_ascii_lowercase).as_deref(),
        Some("png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "ico" | "tif" | "tiff")
    )
}

fn remote_preview_from_bytes(
    path: &RemotePath,
    mut remote: RemoteFileBytes,
    use_dark_syntax_theme: bool,
) -> RightSidebarLoadedFilePreview {
    if is_preview_image_extension(path.extension()) {
        if remote.truncated {
            return RightSidebarLoadedFilePreview {
                lines: Vec::new(),
                image: None,
                message: Some(format!(
                    "image is larger than {} MiB",
                    FILE_PREVIEW_IMAGE_MAX_BYTES / 1024 / 1024
                )),
                truncated: true,
            };
        }
        let encoded_bytes = remote.bytes.len();
        let image_data = ImageDataType::EncodedFile(remote.bytes);
        return match image_data.dimensions() {
            Ok((width, height)) if image_pixels_within_preview_budget(width, height) => {
                RightSidebarLoadedFilePreview {
                    lines: Vec::new(),
                    image: Some(RightSidebarFilePreviewImage {
                        data: Arc::new(ImageData::with_data(image_data)),
                        width,
                        height,
                        encoded_bytes,
                    }),
                    message: None,
                    truncated: false,
                }
            }
            Ok((width, height)) => RightSidebarLoadedFilePreview {
                lines: Vec::new(),
                image: None,
                message: Some(format!(
                    "image is too large to preview ({width}×{height}, over {} megapixels)",
                    FILE_PREVIEW_IMAGE_MAX_PIXELS / 1_000_000
                )),
                truncated: false,
            },
            Err(err) => RightSidebarLoadedFilePreview {
                lines: Vec::new(),
                image: None,
                message: Some(format!("Unable to decode image dimensions: {err:#}")),
                truncated: false,
            },
        };
    }

    if remote.truncated {
        truncate_preview_bytes_to_utf8_boundary(&mut remote.bytes);
    }
    let (text, message) = if remote.bytes.contains(&0) {
        (
            String::new(),
            Some("Preview unavailable for binary file".to_string()),
        )
    } else {
        match String::from_utf8(remote.bytes) {
            Ok(text) if text.is_empty() => (String::new(), Some("Empty file".to_string())),
            Ok(text) => (text, None),
            Err(_) => (
                String::new(),
                Some("Preview unavailable for non-UTF-8 text".to_string()),
            ),
        }
    };
    let lines = if message.is_none() {
        // The highlighter only uses the basename/extension; never pass the
        // remote path through local filesystem operations.
        preview_lines_from_text_with_cancellation(
            Path::new(path.file_name()),
            &text,
            use_dark_syntax_theme,
            None,
        )
    } else {
        Vec::new()
    };
    RightSidebarLoadedFilePreview {
        lines,
        image: None,
        message,
        truncated: remote.truncated,
    }
}

fn load_file_preview_image(path: &Path) -> anyhow::Result<RightSidebarFilePreviewImage> {
    let metadata = fs::metadata(path).with_context(|| format!("stat {}", path.display()))?;
    if metadata.len() > FILE_PREVIEW_IMAGE_MAX_BYTES as u64 {
        anyhow::bail!(
            "image is larger than {} MiB",
            FILE_PREVIEW_IMAGE_MAX_BYTES / 1024 / 1024
        );
    }

    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let image_data = ImageDataType::EncodedFile(bytes);
    // `dimensions()` only reads the header, so this is cheap and lets us reject
    // decompression bombs *before* `cached_image` decodes the full RGBA bitmap.
    let (width, height) = image_data.dimensions().context("decode image dimensions")?;
    if !image_pixels_within_preview_budget(width, height) {
        anyhow::bail!(
            "image is too large to preview ({width}×{height}, over {} megapixels)",
            FILE_PREVIEW_IMAGE_MAX_PIXELS / 1_000_000
        );
    }
    Ok(RightSidebarFilePreviewImage {
        data: Arc::new(ImageData::with_data(image_data)),
        width,
        height,
        encoded_bytes: metadata.len() as usize,
    })
}

/// Whether an image of `width × height` is small enough to decode for preview
/// without risking a huge RGBA allocation. Uses `u64` so the product cannot
/// overflow for pathological dimensions.
fn image_pixels_within_preview_budget(width: u32, height: u32) -> bool {
    (width as u64).saturating_mul(height as u64) <= FILE_PREVIEW_IMAGE_MAX_PIXELS
}

fn load_file_preview(path: &Path) -> (String, Option<String>, bool) {
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(err) => {
            return (
                String::new(),
                Some(format!("Unable to open file: {err}")),
                false,
            );
        }
    };
    let mut bytes = Vec::with_capacity(FILE_PREVIEW_MAX_BYTES + 1);
    let mut limited = file.take((FILE_PREVIEW_MAX_BYTES + 1) as u64);
    if let Err(err) = limited.read_to_end(&mut bytes) {
        return (
            String::new(),
            Some(format!("Unable to read file: {err}")),
            false,
        );
    }

    let truncated = bytes.len() > FILE_PREVIEW_MAX_BYTES;
    if truncated {
        bytes.truncate(FILE_PREVIEW_MAX_BYTES);
        truncate_preview_bytes_to_utf8_boundary(&mut bytes);
    }
    if bytes.contains(&0) {
        return (
            String::new(),
            Some("Preview unavailable for binary file".to_string()),
            truncated,
        );
    }
    match String::from_utf8(bytes) {
        Ok(text) if text.is_empty() => (String::new(), Some("Empty file".to_string()), false),
        Ok(text) => (text, None, truncated),
        Err(_) => (
            String::new(),
            Some("Preview unavailable for non-UTF-8 text".to_string()),
            truncated,
        ),
    }
}

fn truncate_preview_bytes_to_utf8_boundary(bytes: &mut Vec<u8>) {
    if let Err(err) = std::str::from_utf8(bytes) {
        if err.error_len().is_none() {
            bytes.truncate(err.valid_up_to());
        }
    }
}

impl RightSidebarFileCharBag {
    fn from_str(value: &str) -> Self {
        let mut bits = 0u128;
        for ch in value.chars().flat_map(char::to_lowercase) {
            let bit = if ch.is_ascii_alphanumeric() {
                Some((ch as u8).wrapping_sub(b'0') as u32)
            } else {
                match ch {
                    '/' | '\\' => Some(75),
                    '.' => Some(76),
                    '-' => Some(77),
                    '_' => Some(78),
                    _ => None,
                }
            };
            if let Some(bit) = bit.filter(|bit| *bit < 128) {
                bits |= 1u128 << bit;
            }
        }
        Self(bits)
    }

    fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
}

/// Process-wide registry of weak references to file indexes, keyed by
/// (root, project). Lets multiple windows on the same workspace share a single
/// `Arc<RightSidebarFileIndex>` instead of each scanning and holding its own
/// copy. Only weak refs live here, so an index is freed the moment the last
/// window drops its strong ref (e.g. via the idle-release path).
fn shared_file_index_registry(
) -> &'static Mutex<HashMap<(PathBuf, String), Weak<RightSidebarFileIndex>>> {
    static REGISTRY: OnceLock<Mutex<HashMap<(PathBuf, String), Weak<RightSidebarFileIndex>>>> =
        OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Reuse a live shared index for `(root, project)` if one exists, otherwise scan
/// and register it. The scan runs without holding the registry lock, so two
/// windows racing on the same root may each scan once (harmless: both produce
/// equivalent indexes and the last registration wins). A cancelled scan returns
/// `Err` and is never registered.
fn build_or_reuse_shared_file_index(
    root: &Path,
    project_name: &str,
    cancel: &AtomicBool,
) -> Result<Arc<RightSidebarFileIndex>, String> {
    let key = (root.to_path_buf(), project_name.to_string());
    if let Some(existing) = shared_file_index_registry()
        .lock()
        .ok()
        .and_then(|registry| registry.get(&key).and_then(Weak::upgrade))
    {
        return Ok(existing);
    }

    let index = Arc::new(build_right_sidebar_file_index_with_cancel(
        root,
        project_name,
        cancel,
    )?);

    if let Ok(mut registry) = shared_file_index_registry().lock() {
        registry.insert(key, Arc::downgrade(&index));
        registry.retain(|_, weak| weak.strong_count() > 0);
    }
    Ok(index)
}

/// Always scan disk (never returns a cached `Arc`) and publish the fresh index to
/// the shared registry. Used by the manual/periodic/focus **refresh** so it can't
/// "succeed" by handing back a stale index another window still holds.
fn build_fresh_shared_file_index(
    root: &Path,
    project_name: &str,
    cancel: &AtomicBool,
) -> Result<Arc<RightSidebarFileIndex>, String> {
    let index = Arc::new(build_right_sidebar_file_index_with_cancel(
        root,
        project_name,
        cancel,
    )?);
    if let Ok(mut registry) = shared_file_index_registry().lock() {
        registry.insert(
            (root.to_path_buf(), project_name.to_string()),
            Arc::downgrade(&index),
        );
        registry.retain(|_, weak| weak.strong_count() > 0);
    }
    Ok(index)
}

#[cfg(test)]
fn build_right_sidebar_file_index(
    root: &Path,
    project_name: &str,
) -> Result<RightSidebarFileIndex, String> {
    let cancel = AtomicBool::new(false);
    build_right_sidebar_file_index_with_cancel(root, project_name, &cancel)
}

fn build_right_sidebar_file_index_with_cancel(
    root: &Path,
    project_name: &str,
    cancel: &AtomicBool,
) -> Result<RightSidebarFileIndex, String> {
    if !root.is_dir() {
        return Err("Project folder is unavailable".to_string());
    }

    let root_path = root.to_path_buf();
    let mut entries = vec![RightSidebarFileIndexEntry {
        path: root_path.clone(),
        name: project_name.to_string(),
        display_path: project_name.to_string(),
        is_dir: true,
        depth: 0,
        name_char_bag: RightSidebarFileCharBag::from_str(project_name),
        char_bag: RightSidebarFileCharBag::from_str(project_name),
    }];
    let mut children_by_parent: HashMap<PathBuf, Vec<usize>> = HashMap::new();

    let walker = WalkDir::new(root)
        .follow_links(false)
        .min_depth(1)
        .into_iter()
        .filter_entry(should_index_file_entry);
    for entry in walker {
        if entries.len() >= FILE_INDEX_ENTRY_LIMIT {
            break;
        }
        if cancel.load(AtomicOrdering::Relaxed) {
            return Err("File indexing canceled".to_string());
        }
        let Ok(entry) = entry else {
            continue;
        };
        let is_dir = entry.file_type().is_dir();
        let path = entry.path().to_path_buf();
        let parent = path.parent().unwrap_or(root).to_path_buf();
        let name = entry.file_name().to_string_lossy().to_string();
        let display_path = path
            .strip_prefix(root)
            .map(path_to_display_string)
            .unwrap_or_else(|_| name.clone());
        let name_char_bag = RightSidebarFileCharBag::from_str(&name);
        let index = entries.len();
        entries.push(RightSidebarFileIndexEntry {
            path: path.clone(),
            name,
            display_path: display_path.clone(),
            is_dir,
            depth: entry.depth(),
            name_char_bag,
            char_bag: RightSidebarFileCharBag::from_str(&display_path),
        });
        children_by_parent.entry(parent).or_default().push(index);
    }

    if cancel.load(AtomicOrdering::Relaxed) {
        return Err("File indexing canceled".to_string());
    }
    for children in children_by_parent.values_mut() {
        children.sort_by(|left, right| file_index_entry_cmp(&entries[*left], &entries[*right]));
    }

    Ok(RightSidebarFileIndex {
        entries,
        children_by_parent,
    })
}

fn right_sidebar_file_browse_rows_from_index(
    index: &RightSidebarFileIndex,
    expanded: &HashSet<String>,
) -> Vec<RightSidebarFileTreeRow> {
    let mut rows = Vec::new();
    collect_file_index_rows(index, 0, expanded, &mut rows);
    rows
}

fn collect_file_index_rows(
    index: &RightSidebarFileIndex,
    entry_index: usize,
    expanded: &HashSet<String>,
    rows: &mut Vec<RightSidebarFileTreeRow>,
) {
    if rows.len() >= FILE_TREE_ROW_LIMIT {
        return;
    }
    let Some(entry) = index.entries.get(entry_index) else {
        return;
    };
    let is_expanded = entry.depth == 0 || expanded.contains(&path_key(&entry.path));
    rows.push(RightSidebarFileTreeRow {
        path: entry.path.clone(),
        name: entry.name.clone(),
        depth: entry.depth,
        is_dir: entry.is_dir,
        is_expanded,
    });

    if !entry.is_dir || !is_expanded {
        return;
    }
    if let Some(children) = index.children_by_parent.get(&entry.path) {
        for child in children {
            collect_file_index_rows(index, *child, expanded, rows);
            if rows.len() >= FILE_TREE_ROW_LIMIT {
                break;
            }
        }
    }
}

fn search_right_sidebar_file_index(
    index: &RightSidebarFileIndex,
    query: &str,
    cancel: &AtomicBool,
) -> Vec<RightSidebarFileTreeRow> {
    let query = query.trim();
    if query.is_empty() {
        return vec![];
    }

    let query_bag = RightSidebarFileCharBag::from_str(query);
    let lower_query = query.to_ascii_lowercase();
    let search_path = query.contains('/') || query.contains('\\');
    let mut matches = vec![];
    for (index_position, entry) in index.entries.iter().enumerate().skip(1) {
        if cancel.load(AtomicOrdering::Relaxed) {
            return vec![];
        }
        let haystack = if search_path {
            entry.display_path.as_str()
        } else {
            entry.name.as_str()
        };
        let haystack_bag = if search_path {
            entry.char_bag
        } else {
            entry.name_char_bag
        };
        if !haystack_bag.contains(query_bag) {
            continue;
        }
        let haystack_lower = haystack.to_ascii_lowercase();
        let Some(match_offset) = haystack_lower.find(&lower_query) else {
            continue;
        };
        matches.push((match_offset, haystack.len(), index_position));
    }

    matches.sort_by(
        |(left_offset, left_len, left_index), (right_offset, right_len, right_index)| {
            left_offset
                .cmp(right_offset)
                .then_with(|| left_len.cmp(right_len))
                .then_with(|| {
                    file_index_entry_cmp(&index.entries[*left_index], &index.entries[*right_index])
                })
                .then_with(|| left_index.cmp(right_index))
        },
    );

    matches
        .into_iter()
        .take(FILE_TREE_ROW_LIMIT)
        .filter_map(|(_, _, entry_index)| index.entries.get(entry_index))
        .map(|entry| RightSidebarFileTreeRow {
            path: entry.path.clone(),
            name: entry.display_path.clone(),
            depth: 0,
            is_dir: entry.is_dir,
            is_expanded: false,
        })
        .collect()
}

fn should_index_file_entry(entry: &WalkDirEntry) -> bool {
    if entry.depth() == 0 || !entry.file_type().is_dir() {
        return true;
    }
    let name = entry.file_name().to_string_lossy();
    !should_skip_file_index_dir(&name)
}

fn should_skip_file_index_dir(name: &str) -> bool {
    matches!(
        name,
        ".git"
            | ".hg"
            | ".svn"
            | "target"
            | "node_modules"
            | ".next"
            | ".nuxt"
            | ".turbo"
            | ".cache"
            | "dist"
            | "build"
            | "coverage"
            | "vendor"
            | ".venv"
            | "venv"
            | "__pycache__"
    )
}

fn path_to_display_string(path: &Path) -> String {
    let mut display = String::new();
    for component in path.components() {
        if !display.is_empty() {
            display.push('/');
        }
        display.push_str(&component.as_os_str().to_string_lossy());
    }
    display
}

fn file_index_entry_cmp(
    a: &RightSidebarFileIndexEntry,
    b: &RightSidebarFileIndexEntry,
) -> Ordering {
    match (a.is_dir, b.is_dir) {
        (true, false) => Ordering::Less,
        (false, true) => Ordering::Greater,
        _ => naturalish_cmp(&a.name, &b.name),
    }
}

fn naturalish_cmp(a: &str, b: &str) -> Ordering {
    let mut ai = 0;
    let mut bi = 0;
    while ai < a.len() && bi < b.len() {
        let a_ch = a[ai..].chars().next().unwrap();
        let b_ch = b[bi..].chars().next().unwrap();
        if a_ch.is_ascii_digit() && b_ch.is_ascii_digit() {
            let a_start = ai;
            let b_start = bi;
            while ai < a.len() && a.as_bytes()[ai].is_ascii_digit() {
                ai += 1;
            }
            while bi < b.len() && b.as_bytes()[bi].is_ascii_digit() {
                bi += 1;
            }
            let a_digits = &a[a_start..ai];
            let b_digits = &b[b_start..bi];
            let a_trimmed = a_digits.trim_start_matches('0');
            let b_trimmed = b_digits.trim_start_matches('0');
            let cmp = a_trimmed
                .len()
                .cmp(&b_trimmed.len())
                .then_with(|| a_trimmed.cmp(b_trimmed))
                .then_with(|| a_digits.len().cmp(&b_digits.len()));
            if cmp != Ordering::Equal {
                return cmp;
            }
            continue;
        }

        ai += a_ch.len_utf8();
        bi += b_ch.len_utf8();
        let cmp = a_ch
            .to_ascii_lowercase()
            .cmp(&b_ch.to_ascii_lowercase())
            .then_with(|| a_ch.cmp(&b_ch));
        if cmp != Ordering::Equal {
            return cmp;
        }
    }
    a.len().cmp(&b.len())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RightSidebarFileIcon {
    Material(MaterialIcon),
    Svg(SvgIcon),
}

fn file_icon_for_row(row: &RightSidebarFileTreeRow) -> RightSidebarFileIcon {
    if row.is_dir {
        let is_root = row.depth == 0;
        if let Some(icon) = material_folder_icon_for_name(&row.name, row.is_expanded, is_root) {
            return RightSidebarFileIcon::Material(icon);
        }

        RightSidebarFileIcon::Svg(if row.is_expanded {
            SvgIcon::FolderOpen
        } else {
            SvgIcon::Folder
        })
    } else {
        let name = row
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(&row.name);
        if let Some(icon) = material_file_icon_for_name(name) {
            return RightSidebarFileIcon::Material(icon);
        }

        RightSidebarFileIcon::Svg(
            match row
                .path
                .extension()
                .and_then(|ext| ext.to_str())
                .map(str::to_ascii_lowercase)
                .as_deref()
            {
                Some(
                    "rs" | "toml" | "lua" | "js" | "jsx" | "ts" | "tsx" | "json" | "css" | "html"
                    | "sh" | "py" | "rb" | "go" | "swift" | "kt" | "java" | "c" | "cc" | "cpp"
                    | "h" | "hpp" | "m" | "mm",
                ) => SvgIcon::FileCode,
                Some("md" | "txt" | "log" | "yaml" | "yml" | "xml") => SvgIcon::FileText,
                _ => SvgIcon::File,
            },
        )
    }
}

fn remote_file_icon(row: &RemoteFileRow) -> RightSidebarFileIcon {
    if row.entry.is_directory() {
        if let Some(icon) =
            material_folder_icon_for_name(&row.entry.name, row.expanded, row.depth == 0)
        {
            return RightSidebarFileIcon::Material(icon);
        }
        return RightSidebarFileIcon::Svg(if row.expanded {
            SvgIcon::FolderOpen
        } else {
            SvgIcon::Folder
        });
    }
    if let Some(icon) = material_file_icon_for_name(&row.entry.name) {
        return RightSidebarFileIcon::Material(icon);
    }
    let icon = match row
        .entry
        .path
        .extension()
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some(
            "rs" | "toml" | "lua" | "js" | "jsx" | "ts" | "tsx" | "json" | "css" | "html" | "sh"
            | "py" | "rb" | "go" | "swift" | "kt" | "java" | "c" | "cc" | "cpp" | "h" | "hpp" | "m"
            | "mm",
        ) => SvgIcon::FileCode,
        Some("md" | "txt" | "log" | "yaml" | "yml" | "xml") => SvgIcon::FileText,
        _ => SvgIcon::File,
    };
    RightSidebarFileIcon::Svg(icon)
}

fn snippet_preview(body: &str) -> String {
    body.lines()
        .find_map(|line| {
            let trimmed = line.trim();
            (!trimmed.is_empty()).then_some(trimmed)
        })
        .unwrap_or("")
        .chars()
        .take(96)
        .collect()
}

fn snippet_run_buffer(body: &str) -> Option<Vec<u8>> {
    let normalized = body.replace("\r\n", "\n").replace('\r', "\n");
    let lines: Vec<_> = normalized.split('\n').collect();
    let start = lines.iter().position(|line| !line.trim().is_empty())?;
    let end = lines.iter().rposition(|line| !line.trim().is_empty())?;
    let trimmed = lines[start..=end].join("\n");

    let mut buffer = String::with_capacity(trimmed.len() + 1);
    buffer.push_str(&trimmed);
    buffer.push('\n');
    Some(buffer.replace('\n', "\r").into_bytes())
}

fn wrap_snippet_text_for_width<F>(
    text: &str,
    max_lines: usize,
    focused: bool,
    mut measure: F,
) -> Vec<String>
where
    F: FnMut(&str) -> f32,
{
    let max_lines = max_lines.max(1);
    let mut lines = if focused {
        let tail = bounded_snippet_tail(text, max_lines);
        let budget = max_lines.saturating_mul(16).max(max_lines);
        let mut lines = wrap_snippet_text_from_head(&tail, budget, &mut measure);
        if lines.len() > max_lines {
            lines.split_off(lines.len() - max_lines)
        } else {
            lines
        }
    } else {
        wrap_snippet_text_from_head(text, max_lines, &mut measure)
    };

    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

fn wrap_snippet_text_from_head<F>(text: &str, max_lines: usize, measure: &mut F) -> Vec<String>
where
    F: FnMut(&str) -> f32,
{
    let max_lines = max_lines.max(1);
    let mut lines = vec![];
    for hard_line in text.split('\n') {
        wrap_snippet_hard_line(hard_line, measure, &mut lines, max_lines);
        if lines.len() >= max_lines {
            break;
        }
    }
    lines
}

fn wrap_snippet_hard_line<F>(
    line: &str,
    measure: &mut F,
    wrapped: &mut Vec<String>,
    max_lines: usize,
) where
    F: FnMut(&str) -> f32,
{
    if wrapped.len() >= max_lines {
        return;
    }
    if line.is_empty() || measure(line) <= 1.0 {
        wrapped.push(line.to_string());
        return;
    }

    let mut current = String::new();
    for token in whitespace_tokens(line) {
        let token_is_whitespace = token.chars().all(char::is_whitespace);
        if current.is_empty() && token_is_whitespace {
            current.push_str(token);
            continue;
        }

        let candidate = format!("{current}{token}");
        if measure(&candidate) <= 1.0 {
            current = candidate;
            continue;
        }

        if !current.trim().is_empty() {
            wrapped.push(current.trim_end().to_string());
            if wrapped.len() >= max_lines {
                return;
            }
        }
        current.clear();

        let token = if token_is_whitespace {
            ""
        } else {
            token.trim_start()
        };
        if token.is_empty() {
            continue;
        }
        if measure(token) <= 1.0 {
            current.push_str(token);
        } else {
            current = wrap_long_snippet_token(token, measure, wrapped, max_lines);
            if wrapped.len() >= max_lines {
                return;
            }
        }
    }

    if !current.trim().is_empty() && wrapped.len() < max_lines {
        wrapped.push(current.trim_end().to_string());
    }
}

fn bounded_snippet_tail(text: &str, max_lines: usize) -> String {
    const TAIL_CHARS_PER_VISIBLE_LINE: usize = 256;

    let hard_line_limit = max_lines.max(1);
    let char_limit = hard_line_limit.saturating_mul(TAIL_CHARS_PER_VISIBLE_LINE);
    let mut lines = text
        .rsplit('\n')
        .take(hard_line_limit)
        .map(|line| tail_chars(line, char_limit))
        .collect::<Vec<_>>();
    lines.reverse();
    lines.join("\n")
}

fn tail_chars(text: &str, max_chars: usize) -> &str {
    if max_chars == 0 {
        return "";
    }

    let mut seen = 0;
    for (idx, _) in text.char_indices().rev() {
        seen += 1;
        if seen == max_chars {
            return &text[idx..];
        }
    }
    text
}

fn whitespace_tokens(text: &str) -> Vec<&str> {
    let mut tokens = vec![];
    let mut start = 0;
    let mut current_is_whitespace = None;
    for (idx, ch) in text.char_indices() {
        let is_whitespace = ch.is_whitespace();
        match current_is_whitespace {
            Some(kind) if kind != is_whitespace => {
                tokens.push(&text[start..idx]);
                start = idx;
                current_is_whitespace = Some(is_whitespace);
            }
            None => current_is_whitespace = Some(is_whitespace),
            _ => {}
        }
    }
    if start < text.len() {
        tokens.push(&text[start..]);
    }
    tokens
}

fn wrap_long_snippet_token<F>(
    token: &str,
    measure: &mut F,
    wrapped: &mut Vec<String>,
    max_lines: usize,
) -> String
where
    F: FnMut(&str) -> f32,
{
    let mut current = String::new();
    for ch in token.chars() {
        if wrapped.len() >= max_lines {
            return current;
        }
        let mut candidate = current.clone();
        candidate.push(ch);
        if !current.is_empty() && measure(&candidate) > 1.0 {
            wrapped.push(std::mem::take(&mut current));
            if wrapped.len() >= max_lines {
                return String::new();
            }
            current.push(ch);
        } else {
            current = candidate;
        }
    }
    current
}

fn snippet_cursor_visible(now_ms: u128, blink_ms: u64) -> bool {
    let blink_ms = u128::from(blink_ms.max(1));
    (now_ms / blink_ms) % 2 == 0
}

#[cfg(test)]
mod tests {
    use super::{
        approximate_note_text_width, build_right_sidebar_file_index, clip_note_texture,
        file_release_action, file_row_placement, full_line_colors_by_byte,
        image_pixels_within_preview_budget, load_file_preview, load_file_preview_image,
        naturalish_cmp, note_code_highlight_key, note_code_highlight_lines, note_code_row_height,
        note_image_display_size, note_open_pending_for_vault, note_release_action,
        open_with_candidate_allowed, path_key, preview_line_count, preview_lines_from_text,
        preview_plain_lines_from_text, preview_text_range, preview_visible_colored,
        preview_visible_line_range, right_sidebar_file_browse_rows_from_index,
        right_sidebar_file_row_metrics, right_sidebar_open_with_cache_key,
        scrollable_note_table_columns, search_right_sidebar_file_index, snippet_cursor_visible,
        snippet_run_buffer, sorted_open_with_candidates, virtual_note_line_range,
        visible_code_block_rounded_edges, visible_file_row_range, wrap_snippet_text_for_width,
        FileReleaseAction, FileRowPlacement, NoteApproximateTextMetrics, NoteCodeHighlightEntry,
        NoteCodeHighlightState, NoteReleaseAction, FILE_PREVIEW_MAX_BYTES, NOTE_CODE_BLOCK_RADIUS,
        NOTE_CODE_HEADER_HEIGHT,
    };
    use crate::markdown_editor::{NoteLineGeometry, ProjectedCodeBlock};
    use crate::termwindow::{RightSidebarFilePreviewLine, RightSidebarFilePreviewSpan};
    use crate::utilsprites::RenderMetrics;
    use std::collections::HashSet;
    use std::fs;
    use std::io::Cursor;
    use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
    use std::sync::Arc;
    use wezterm_font::units::PixelLength;
    use window::color::LinearRgba;
    use window::Size;

    fn test_render_metrics(cell_height: isize, cell_width: isize) -> RenderMetrics {
        RenderMetrics {
            descender: PixelLength::new(0.0),
            descender_row: 0,
            descender_plus_two: 0,
            underline_height: 1,
            strike_row: 0,
            cell_size: Size::new(cell_width, cell_height),
        }
    }

    #[test]
    fn idle_release_decisions_respect_token_visibility_and_dirty_state() {
        // Stale token: another open re-armed the timer; never release.
        assert_eq!(
            note_release_action(false, false, false),
            NoteReleaseAction::Skip
        );
        assert_eq!(
            file_release_action(false, false, false),
            FileReleaseAction::Skip
        );
        // Visible feature: never release, even with a matching token.
        assert_eq!(
            note_release_action(true, true, false),
            NoteReleaseAction::Skip
        );
        assert_eq!(
            file_release_action(true, true, true),
            FileReleaseAction::Skip
        );
        // Hidden + idle: release.
        assert_eq!(
            note_release_action(true, false, false),
            NoteReleaseAction::Release
        );
        assert_eq!(
            file_release_action(true, false, false),
            FileReleaseAction::Release
        );
        // Dirty Note saves first and re-arms instead of dropping edits.
        assert_eq!(
            note_release_action(true, false, true),
            NoteReleaseAction::SaveAndReschedule
        );
        // Hidden while still indexing: re-arm so the release eventually
        // happens instead of leaking forever.
        assert_eq!(
            file_release_action(true, false, true),
            FileReleaseAction::RescheduleWhileIndexing
        );
    }

    #[test]
    fn note_virtualization_keeps_modest_overscan_around_the_viewport() {
        let geometry = (0..1_000)
            .map(|index| NoteLineGeometry {
                top: index as f32 * 10.0,
                height: 10.0,
                gap: 0.0,
            })
            .collect::<Vec<_>>();
        // Viewport covers rows 500..510; overscan must extend past both edges
        // without painting multiple invisible viewports.
        let range = virtual_note_line_range(&geometry, 5_000.0, 100.0);
        assert!(range.start <= 495);
        assert!(range.end >= 515);
        assert!(range.len() < 30, "virtual range was {} rows", range.len());
    }

    #[test]
    fn note_images_scale_responsively_without_escaping_the_viewport() {
        assert_eq!(note_image_display_size(0, 100, 600.0, 500.0), (0.0, 0.0));
        assert_eq!(
            note_image_display_size(200, 100, 600.0, 500.0),
            (600.0, 300.0)
        );
        assert_eq!(
            note_image_display_size(1200, 2400, 600.0, 500.0),
            (250.0, 500.0)
        );
    }

    #[test]
    fn note_images_clip_position_and_texture_at_the_body_edges() {
        let clip = clip_note_texture(
            0.0, 0.0, 100.0, 100.0, 0.0, 0.0, 1.0, 1.0, 10.0, 25.0, 90.0, 75.0,
        )
        .unwrap();
        assert_eq!(clip.left, 10.0);
        assert_eq!(clip.top, 25.0);
        assert_eq!(clip.right, 90.0);
        assert_eq!(clip.bottom, 75.0);
        assert_eq!(clip.texture_left, 0.1);
        assert_eq!(clip.texture_top, 0.25);
        assert_eq!(clip.texture_right, 0.9);
        assert_eq!(clip.texture_bottom, 0.75);
    }

    #[test]
    fn background_note_width_does_not_double_count_cjk_or_emoji() {
        let metrics = NoteApproximateTextMetrics {
            latin: 5.0,
            space: 3.0,
            wide: 10.0,
        };
        assert_eq!(approximate_note_text_width("abc", metrics), 15.0);
        assert_eq!(approximate_note_text_width("a b", metrics), 13.0);
        assert_eq!(approximate_note_text_width("系统", metrics), 20.0);
        assert_eq!(approximate_note_text_width("👨‍👩‍👧‍👦", metrics), 10.0);

        let mixed = "alpha：示例 beta";
        let all_full_width = mixed.chars().count() as f32 * metrics.wide;
        assert!(approximate_note_text_width(mixed, metrics) < all_full_width * 0.7);
    }

    #[test]
    fn pending_note_open_wins_over_the_old_persisted_active_path() {
        let root = std::path::PathBuf::from("/vault");
        let opening = (root.clone(), "Second.md".to_string());
        assert!(note_open_pending_for_vault(Some(&opening), &root));
        assert!(!note_open_pending_for_vault(
            Some(&opening),
            std::path::Path::new("/another-vault")
        ));
    }

    #[test]
    fn wide_note_tables_keep_natural_width_for_horizontal_scrolling() {
        let widths = scrollable_note_table_columns(&[260.0, 240.0, 220.0], 400.0, 120.0, 480.0);
        assert_eq!(widths, vec![260.0, 240.0, 220.0]);
        assert!(widths.iter().sum::<f32>() > 400.0);

        let narrow = scrollable_note_table_columns(&[20.0, 40.0], 400.0, 120.0, 480.0);
        assert_eq!(narrow.iter().sum::<f32>(), 400.0);
        assert!(narrow.iter().all(|width| *width >= 120.0));
    }

    #[test]
    fn note_code_highlighting_resolves_aliases_and_utf8_byte_colors() {
        assert!(thinkterm_syntax::detect_fence("python").is_some());
        assert!(thinkterm_syntax::detect_fence("rust").is_some());
        assert!(thinkterm_syntax::detect_fence("definitely-not-a-language").is_none());

        let text = "def 你好(name):\n    return name\n";
        let code = ProjectedCodeBlock {
            source: 0..text.len() + 12,
            content: 6..6 + text.len(),
            text: text.to_string(),
            language: Some("python".to_string()),
        };
        let colors = note_code_highlight_lines(&code, true);
        assert_eq!(colors.len(), 2);
        assert_eq!(colors[0].len(), "def 你好(name):".len());
        assert_eq!(colors[1].len(), "    return name".len());
        let first = colors.iter().flatten().next().copied().unwrap();
        assert!(
            colors.iter().flatten().any(|color| *color != first),
            "Python should produce multiple token colors"
        );
    }

    #[test]
    fn note_code_highlight_cache_is_content_based_and_keeps_previous_colors() {
        let code = |start: usize, text: &str, language: &str| ProjectedCodeBlock {
            source: start..start + text.len() + 12,
            content: start + 6..start + 6 + text.len(),
            text: text.to_string(),
            language: Some(language.to_string()),
        };
        let python = code(0, "print(1)\n", "python");
        let shifted_alias = code(200, "print(1)\n", "py");
        let changed = code(0, "print(2)\n", "python");
        let key = note_code_highlight_key(&python).unwrap();
        assert_eq!(key, note_code_highlight_key(&shifted_alias).unwrap());
        let changed_key = note_code_highlight_key(&changed).unwrap();
        assert_ne!(key, changed_key);

        let empty = Arc::new(vec![vec![]]);
        let entry = Arc::new(NoteCodeHighlightEntry {
            key,
            source: Arc::from(python.text.as_str()),
            light: Arc::clone(&empty),
            dark: Arc::clone(&empty),
        });
        let mut state = NoteCodeHighlightState::default();
        state.insert_cache(Arc::clone(&entry));
        state.remember_block(python.source.start, Arc::clone(&entry));
        assert!(state.cached(key, &python.text).is_some());
        assert!(state.cached(key, "hash collision guard").is_none());
        assert!(state
            .previous_for_block(python.source.start, changed_key.language)
            .is_some());

        let generation = state
            .schedule_block(python.source.start, changed_key)
            .unwrap();
        assert!(generation > 0);
        assert!(state
            .schedule_block(python.source.start, changed_key)
            .is_none());
    }

    #[test]
    fn note_code_row_heights_form_one_continuous_block() {
        assert_eq!(note_code_row_height(0, 3, false, 20.0, 38.0, 9.0), 67.0);
        assert_eq!(note_code_row_height(1, 3, false, 20.0, 38.0, 9.0), 20.0);
        assert_eq!(note_code_row_height(2, 3, false, 20.0, 38.0, 9.0), 29.0);
        assert_eq!(note_code_row_height(0, 3, true, 20.0, 38.0, 9.0), 38.0);
        assert_eq!(note_code_row_height(1, 3, true, 20.0, 38.0, 9.0), 0.0);
    }

    #[test]
    fn clipped_code_blocks_only_round_real_visible_edges() {
        assert_eq!(
            visible_code_block_rounded_edges(20.0, 80.0, 0.0, 100.0),
            (true, true)
        );
        assert_eq!(
            visible_code_block_rounded_edges(-20.0, 80.0, 0.0, 100.0),
            (false, true)
        );
        assert_eq!(
            visible_code_block_rounded_edges(20.0, 120.0, 0.0, 100.0),
            (true, false)
        );
        assert_eq!(
            visible_code_block_rounded_edges(-20.0, 120.0, 0.0, 100.0),
            (false, false)
        );
    }

    #[test]
    fn note_code_block_uses_panel_radius_not_capsule_radius() {
        assert!(NOTE_CODE_BLOCK_RADIUS < NOTE_CODE_HEADER_HEIGHT as f32 / 2.0);
    }

    #[test]
    fn file_index_sorts_dirs_first_and_natural() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("dir10")).unwrap();
        fs::create_dir(dir.path().join("dir2")).unwrap();
        fs::write(dir.path().join("file10.txt"), "").unwrap();
        fs::write(dir.path().join("file2.txt"), "").unwrap();

        let index = build_right_sidebar_file_index(dir.path(), "Project").unwrap();
        let rows = right_sidebar_file_browse_rows_from_index(&index, &HashSet::new());
        let names: Vec<_> = rows.into_iter().map(|row| row.name).collect();
        assert_eq!(
            names,
            vec!["Project", "dir2", "dir10", "file2.txt", "file10.txt"]
        );
    }

    #[test]
    fn naturalish_cmp_sorts_digit_runs_by_value() {
        assert_eq!(naturalish_cmp("tab2", "tab10"), std::cmp::Ordering::Less);
        assert_eq!(naturalish_cmp("tab10", "tab2"), std::cmp::Ordering::Greater);
        assert_eq!(naturalish_cmp("tab01", "tab1"), std::cmp::Ordering::Greater);
    }

    #[test]
    fn file_index_search_matches_paths() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        let docs = dir.path().join("docs");
        fs::create_dir(&src).unwrap();
        fs::create_dir(&docs).unwrap();
        fs::write(src.join("main.rs"), "fn main() {}\n").unwrap();
        fs::write(docs.join("readme.md"), "# docs\n").unwrap();

        let index = build_right_sidebar_file_index(dir.path(), "Project").unwrap();
        let cancel = AtomicBool::new(false);
        let rows = search_right_sidebar_file_index(&index, "main", &cancel);
        let names: Vec<_> = rows.into_iter().map(|row| row.name).collect();
        assert_eq!(names, vec!["src/main.rs"]);
    }

    #[test]
    fn file_index_search_does_not_join_directory_and_extension_for_basename_queries() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        let elio = dir.path().join("research").join("elio-main").join("src");
        fs::create_dir(&src).unwrap();
        fs::create_dir_all(&elio).unwrap();
        fs::write(src.join("main.rs"), "fn main() {}\n").unwrap();
        fs::write(elio.join("cli.rs"), "").unwrap();
        fs::write(elio.join("lib.rs"), "").unwrap();
        fs::write(elio.parent().unwrap().join("build.rs"), "").unwrap();

        let index = build_right_sidebar_file_index(dir.path(), "Project").unwrap();
        let cancel = AtomicBool::new(false);
        let rows = search_right_sidebar_file_index(&index, "main.rs", &cancel);
        let names: Vec<_> = rows.into_iter().map(|row| row.name).collect();

        assert_eq!(names, vec!["src/main.rs"]);
    }

    #[test]
    fn file_index_search_is_not_fuzzy() {
        let dir = tempfile::tempdir().unwrap();
        let icons = dir.path().join("third_party").join("lucide").join("icons");
        let docs = dir.path().join("docs");
        let src = dir.path().join("src");
        fs::create_dir_all(&icons).unwrap();
        fs::create_dir_all(&docs).unwrap();
        fs::create_dir_all(&src).unwrap();
        fs::write(icons.join("mail-minus.js"), "").unwrap();
        fs::write(icons.join("map-pin.js"), "").unwrap();
        fs::write(docs.join("mermaid-init.js"), "").unwrap();
        fs::write(src.join("main.js"), "").unwrap();

        let index = build_right_sidebar_file_index(dir.path(), "Project").unwrap();
        let cancel = AtomicBool::new(false);
        let rows = search_right_sidebar_file_index(&index, "main.js", &cancel);
        let names: Vec<_> = rows.into_iter().map(|row| row.name).collect();

        assert_eq!(names, vec!["src/main.js"]);
    }

    #[test]
    fn file_index_search_uses_relative_path_when_query_has_separator() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        fs::create_dir(&src).unwrap();
        fs::write(src.join("main.rs"), "fn main() {}\n").unwrap();

        let index = build_right_sidebar_file_index(dir.path(), "Project").unwrap();
        let cancel = AtomicBool::new(false);
        let rows = search_right_sidebar_file_index(&index, "src/main", &cancel);
        let names: Vec<_> = rows.into_iter().map(|row| row.name).collect();

        assert_eq!(names, vec!["src/main.rs"]);
    }

    #[test]
    fn file_index_skips_generated_heavy_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("generated-artifact.rs"), "fn generated() {}\n").unwrap();

        let index = build_right_sidebar_file_index(dir.path(), "Project").unwrap();
        let cancel = AtomicBool::new(false);
        let rows = search_right_sidebar_file_index(&index, "generated", &cancel);
        assert!(rows.is_empty());
    }

    #[test]
    fn file_index_browse_only_descends_expanded_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        fs::create_dir(&src).unwrap();
        fs::write(src.join("main.rs"), "fn main() {}\n").unwrap();

        let index = build_right_sidebar_file_index(dir.path(), "Project").unwrap();
        let rows = right_sidebar_file_browse_rows_from_index(&index, &HashSet::new());
        let names: Vec<_> = rows.iter().map(|row| row.name.as_str()).collect();
        assert_eq!(names, vec!["Project", "src"]);

        let mut expanded = HashSet::new();
        expanded.insert(path_key(&src));
        let rows = right_sidebar_file_browse_rows_from_index(&index, &expanded);
        let names: Vec<_> = rows.iter().map(|row| row.name.as_str()).collect();
        assert_eq!(names, vec!["Project", "src", "main.rs"]);
    }

    #[test]
    fn file_index_search_respects_cancel() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("main.rs"), "fn main() {}\n").unwrap();
        let index = build_right_sidebar_file_index(dir.path(), "Project").unwrap();
        let cancel = AtomicBool::new(true);

        let rows = search_right_sidebar_file_index(&index, "main", &cancel);

        assert!(rows.is_empty());
        assert!(cancel.load(AtomicOrdering::Relaxed));
    }

    #[test]
    fn load_file_preview_rejects_binary_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("blob.bin");
        fs::write(&path, [0, 1, 2, 3]).unwrap();

        let (text, message, truncated) = load_file_preview(&path);
        assert!(text.is_empty());
        assert_eq!(
            message.as_deref(),
            Some("Preview unavailable for binary file")
        );
        assert!(!truncated);
    }

    #[test]
    fn load_file_preview_truncates_large_text() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("large.txt");
        fs::write(&path, vec![b'a'; FILE_PREVIEW_MAX_BYTES + 1]).unwrap();

        let (text, message, truncated) = load_file_preview(&path);
        assert!(message.is_none());
        assert!(truncated);
        assert_eq!(text.len(), FILE_PREVIEW_MAX_BYTES);
    }

    #[test]
    fn load_file_preview_truncates_large_utf8_text_at_character_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("large-utf8.txt");
        let prefix = "a".repeat(FILE_PREVIEW_MAX_BYTES - 1);
        fs::write(&path, format!("{prefix}你好")).unwrap();

        let (text, message, truncated) = load_file_preview(&path);

        assert!(message.is_none());
        assert!(truncated);
        assert_eq!(text, prefix);
    }

    #[test]
    fn load_file_preview_image_reads_dimensions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("image.png");
        let image = image::RgbaImage::from_pixel(2, 1, image::Rgba([255, 0, 0, 255]));
        let mut bytes = Cursor::new(Vec::new());
        image.write_to(&mut bytes, image::ImageFormat::Png).unwrap();
        fs::write(&path, bytes.into_inner()).unwrap();

        let preview = load_file_preview_image(&path).unwrap();

        assert_eq!((preview.width, preview.height), (2, 1));
    }

    #[test]
    fn preview_visible_line_range_only_returns_requested_window() {
        let cached_lines = preview_plain_lines_from_text("one\ntwo\nthree\nfour");
        let range = preview_visible_line_range(cached_lines.len(), 1, 2);

        assert_eq!(preview_line_count(&cached_lines), 4);
        assert_eq!(range, 1..3);
        assert_eq!(cached_lines[range.start].plain, "two");
        assert_eq!(cached_lines[range.end - 1].plain, "three");

        let empty_lines = preview_plain_lines_from_text("");
        assert_eq!(preview_line_count(&empty_lines), 1);
        assert_eq!(preview_visible_line_range(empty_lines.len(), 0, 2), 0..1);
        assert_eq!(empty_lines[0].plain, "");
        assert_eq!(preview_visible_line_range(0, 0, 2), 0..0);
    }

    #[test]
    fn partly_scrolled_row_keeps_its_true_origin() {
        // Scroll by a third of a row: the first row is still partly visible and
        // must report an origin *above* tree_top so the painter can clip it.
        // Clamping it to tree_top (the bug this pins) would draw a full-height
        // row starting at tree_top, overlapping row 1 by the remainder and
        // handing it an oversized hit rect.
        let row_height = 30usize;
        let tree_top = 100.0;
        let viewport_bottom = 400.0;
        let scroll = 10.0;

        assert_eq!(
            file_row_placement(0, row_height, tree_top, viewport_bottom, scroll),
            FileRowPlacement::Visible(90.0)
        );
        // Rows never overlap: each sits exactly one row height below the last.
        assert_eq!(
            file_row_placement(1, row_height, tree_top, viewport_bottom, scroll),
            FileRowPlacement::Visible(120.0)
        );
    }

    #[test]
    fn rows_outside_the_viewport_are_classified() {
        let row_height = 30usize;
        let tree_top = 100.0;
        let viewport_bottom = 400.0;

        // Scrolled a full row: row 0 is exactly flush with the top edge and
        // contributes nothing.
        assert_eq!(
            file_row_placement(0, row_height, tree_top, viewport_bottom, 30.0),
            FileRowPlacement::Above
        );
        // One pixel less and it is still (barely) on screen.
        assert_eq!(
            file_row_placement(0, row_height, tree_top, viewport_bottom, 29.0),
            FileRowPlacement::Visible(71.0)
        );
        // A row starting exactly at the bottom edge is out, and so is anything
        // after it — the caller stops there.
        assert_eq!(
            file_row_placement(10, row_height, tree_top, viewport_bottom, 0.0),
            FileRowPlacement::Below
        );
        assert_eq!(
            file_row_placement(9, row_height, tree_top, viewport_bottom, 0.0),
            FileRowPlacement::Visible(370.0)
        );
    }

    #[test]
    fn unscrolled_first_row_starts_at_the_tree_top() {
        assert_eq!(
            file_row_placement(0, 30, 100.0, 400.0, 0.0),
            FileRowPlacement::Visible(100.0)
        );
    }

    #[test]
    fn visible_file_row_range_only_returns_rows_near_viewport() {
        let row_height = 44;
        assert_eq!(visible_file_row_range(0, 0.0, 400, row_height), 0..0);
        assert_eq!(
            visible_file_row_range(100, 0.0, row_height * 3, row_height),
            0..4
        );
        assert_eq!(
            visible_file_row_range(100, (row_height * 50) as f32, row_height * 3, row_height),
            50..54
        );
        assert_eq!(
            visible_file_row_range(10, (row_height * 9) as f32, row_height * 4, row_height),
            9..10
        );
        assert_eq!(visible_file_row_range(10, 0.0, 400, 0), 0..0);
    }

    #[test]
    fn file_row_metrics_scale_with_font_height() {
        let small = right_sidebar_file_row_metrics(test_render_metrics(18, 9));
        let normal = right_sidebar_file_row_metrics(test_render_metrics(26, 13));
        let large = right_sidebar_file_row_metrics(test_render_metrics(48, 24));

        assert_eq!(small.row_height, 30);
        assert_eq!(small.icon_size, 23);
        assert!(normal.row_height > small.row_height);
        assert!(normal.icon_size > small.icon_size);
        assert!(normal.indent_step >= small.indent_step);
        assert!(normal.icon_gap >= small.icon_gap);
        assert_eq!(large.row_height, 81);
        assert_eq!(large.icon_size, 62);
        assert!(large.row_height > normal.row_height);
        assert!(large.icon_size > normal.icon_size);
        assert!(large.chevron_size > normal.chevron_size);
    }

    #[test]
    fn preview_image_pixel_budget_rejects_decompression_bombs() {
        // Typical sizes are allowed.
        assert!(image_pixels_within_preview_budget(1920, 1080)); // 2 MP
        assert!(image_pixels_within_preview_budget(4096, 2160)); // ~8.8 MP (4K)
        assert!(image_pixels_within_preview_budget(4000, 4000)); // 16 MP (== budget)
                                                                 // Bombs are rejected, and the u64 product cannot overflow.
        assert!(!image_pixels_within_preview_budget(8000, 8000)); // 64 MP
        assert!(!image_pixels_within_preview_budget(100_000, 100_000));
        assert!(!image_pixels_within_preview_budget(u32::MAX, u32::MAX));
    }

    #[test]
    fn open_with_cache_key_uses_extension_when_available() {
        assert_eq!(
            right_sidebar_open_with_cache_key(std::path::Path::new("/tmp/App.RS")),
            "ext:rs"
        );
        assert_eq!(
            right_sidebar_open_with_cache_key(std::path::Path::new("/tmp/Makefile")),
            "path:/tmp/Makefile"
        );
    }

    #[test]
    fn open_with_candidates_sort_preferred_first_then_developer_tools() {
        let candidates = vec![
            wezterm_open_url::OpenWithCandidate {
                id: "zed".to_string(),
                label: "Zed".to_string(),
                icon_path: None,
                is_default: false,
            },
            wezterm_open_url::OpenWithCandidate {
                id: "code".to_string(),
                label: "VS Code".to_string(),
                icon_path: None,
                is_default: false,
            },
            wezterm_open_url::OpenWithCandidate {
                id: "/Applications/TextEdit.app".to_string(),
                label: "TextEdit".to_string(),
                icon_path: None,
                is_default: true,
            },
        ];

        let labels = sorted_open_with_candidates(candidates, Some("zed"))
            .into_iter()
            .map(|candidate| candidate.label)
            .collect::<Vec<_>>();

        assert_eq!(labels, vec!["Zed", "VS Code", "TextEdit"]);
    }

    #[test]
    fn open_with_filter_hides_junk_candidates() {
        // Real-world LaunchServices output observed for a .md file
        fn cand(id: &str, label: &str, is_default: bool) -> wezterm_open_url::OpenWithCandidate {
            wezterm_open_url::OpenWithCandidate {
                id: id.to_string(),
                label: label.to_string(),
                icon_path: None,
                is_default,
            }
        }
        let candidates = vec![
            cand("/Applications/Typora.app", "Typora", true),
            cand("/Applications/MinerU.app", "MinerU", false),
            cand("/Applications/Xcode.app", "Xcode", false),
            cand("/Applications/calibre.app", "calibre", false),
            cand("/Applications/Cursor.app", "Cursor", false),
            cand("/Applications/Zed.app", "Zed", false),
            cand(
                "/Applications/Visual Studio Code.app",
                "Visual Studio Code",
                false,
            ),
            cand(
                "/Applications/calibre.app/Contents/ebook-viewer.app",
                "ebook-viewer",
                false,
            ),
            cand("/System/Applications/TextEdit.app", "TextEdit", false),
            cand("/Applications/Microsoft Word.app", "Microsoft Word", false),
            cand(
                "/Users/u/.cache/codex-runtimes/native/libreoffice/LibreOfficeDev.app",
                "LibreOfficeDev",
                false,
            ),
            cand(
                "/Applications/Xcode.app/Contents/Applications/Instruments.app",
                "Instruments",
                false,
            ),
            cand("/Applications/Google Chrome.app", "Google Chrome", false),
            cand("/System/Applications/Notes.app", "Notes", false),
            cand("/Applications/010 Editor.app", "010 Editor", false),
        ];

        let custom_ids = HashSet::new();
        let kept: Vec<&str> = candidates
            .iter()
            .filter(|candidate| open_with_candidate_allowed(candidate, &custom_ids, None))
            .map(|candidate| candidate.label.as_str())
            .collect();

        assert_eq!(
            kept,
            vec![
                "Typora",
                "Xcode",
                "Cursor",
                "Zed",
                "Visual Studio Code",
                "TextEdit",
                "Microsoft Word",
            ]
        );
    }

    #[test]
    fn preview_lines_from_text_highlights_known_file_types() {
        let path = std::path::Path::new("main.rs");
        let lines = preview_lines_from_text(path, "fn main() {}", true);

        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].plain, "fn main() {}");
        assert!(!lines[0].spans.is_empty());
    }

    #[test]
    fn preview_lines_from_text_highlights_python_files() {
        let path = std::path::Path::new("script.py");
        let lines = preview_lines_from_text(path, "def main():\n    return 1", true);

        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].plain, "def main():");
        assert!(!lines[0].spans.is_empty());
        let colors: HashSet<_> = lines[0]
            .spans
            .iter()
            .map(|span| {
                let color = span.color.to_srgb().to_tuple_rgba();
                (
                    (color.0 * 255.0).round() as u8,
                    (color.1 * 255.0).round() as u8,
                    (color.2 * 255.0).round() as u8,
                    (color.3 * 255.0).round() as u8,
                )
            })
            .collect();
        assert!(
            colors.len() > 1,
            "python preview should render visibly different token colors"
        );
    }

    #[test]
    fn preview_lines_store_character_counts() {
        let plain = preview_plain_lines_from_text("ab你好cd");
        assert_eq!(plain[0].char_count, 6);

        let highlighted =
            preview_lines_from_text(std::path::Path::new("script.py"), "def 你好():", true);
        let line = &highlighted[0];
        assert_eq!(line.char_count, line.plain.chars().count());
        assert_eq!(
            line.spans.iter().map(|span| span.char_count).sum::<usize>(),
            line.char_count
        );
    }

    #[test]
    fn preview_visible_colored_slices_by_character_columns() {
        let green = LinearRgba::with_components(0.0, 1.0, 0.0, 1.0);
        let blue = LinearRgba::with_components(0.0, 0.0, 1.0, 1.0);
        let line = RightSidebarFilePreviewLine {
            plain: "ab你好cd".to_string(),
            char_count: 6,
            spans: vec![
                RightSidebarFilePreviewSpan {
                    text: "ab".to_string(),
                    char_count: 2,
                    color: green,
                },
                RightSidebarFilePreviewSpan {
                    text: "你好cd".to_string(),
                    char_count: 4,
                    color: blue,
                },
            ],
        };

        let (text, colors) = preview_visible_colored(&line, 2, 2);
        assert_eq!(text, "你好");
        assert_eq!(colors, vec![blue, blue]);
    }

    #[test]
    fn full_line_colors_map_each_byte_to_its_span_color() {
        let green = LinearRgba::with_components(0.0, 1.0, 0.0, 1.0);
        let blue = LinearRgba::with_components(0.0, 0.0, 1.0, 1.0);
        // "ab你好cd": a=0 b=1 你=2..5 好=5..8 c=8 d=9 -> 10 bytes; 你/好 are 3
        // bytes each. The full-line h-scroll painter indexes this by a glyph's
        // `cluster` (a byte offset), so each byte must carry its span's colour.
        let line = RightSidebarFilePreviewLine {
            plain: "ab你好cd".to_string(),
            char_count: 6,
            spans: vec![
                RightSidebarFilePreviewSpan {
                    text: "ab你".to_string(),
                    char_count: 3,
                    color: green,
                },
                RightSidebarFilePreviewSpan {
                    text: "好cd".to_string(),
                    char_count: 3,
                    color: blue,
                },
            ],
        };

        let colors = full_line_colors_by_byte(&line);
        assert_eq!(colors.len(), line.plain.len());
        // bytes 0..5 (a, b, 你) -> green; 5..10 (好, c, d) -> blue.
        for b in 0..5 {
            assert_eq!(colors[b], green, "byte {b} should be green");
        }
        for b in 5..10 {
            assert_eq!(colors[b], blue, "byte {b} should be blue");
        }
        // A multi-byte glyph's cluster maps to the right colour: 你 at byte 2,
        // 好 at byte 5.
        assert_eq!(colors[2], green);
        assert_eq!(colors[5], blue);
    }

    #[test]
    fn preview_keeps_long_lines_intact() {
        // A line far longer than the highlight cap must be preserved verbatim
        // (no 640-char truncation, no "...") on both the plain and highlighted
        // paths.
        let line = "a".repeat(16_384);

        let plain = preview_plain_lines_from_text(&line);
        assert_eq!(plain.len(), 1);
        assert_eq!(plain[0].plain.chars().count(), line.chars().count());
        assert!(!plain[0].plain.contains("..."));

        let highlighted = preview_lines_from_text(std::path::Path::new("min.css"), &line, true);
        assert_eq!(highlighted.len(), 1);
        assert_eq!(highlighted[0].plain.chars().count(), line.chars().count());
        assert!(!highlighted[0].plain.contains("..."));
    }

    #[test]
    fn preview_highlight_limit_never_splits_utf8_or_truncates_text() {
        let mut text = "a".repeat(thinkterm_syntax::DEFAULT_HIGHLIGHT_BYTE_LIMIT - 1);
        text.push('你');
        text.push_str("tail");

        let lines = preview_lines_from_text(std::path::Path::new("large.py"), &text, true);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].plain, text);
        assert_eq!(lines[0].char_count, text.chars().count());
    }

    #[test]
    fn preview_text_range_slices_by_character_columns() {
        assert_eq!(preview_text_range("ab你好cd", 2, 4), "你好");
        assert_eq!(preview_text_range("ab日本語cd", 2, 5), "日本語");
        assert_eq!(preview_text_range("ab한글cd", 2, 4), "한글");
        assert_eq!(preview_text_range("abسلامcd", 2, 6), "سلام");
        assert_eq!(preview_text_range("abприветcd", 2, 8), "привет");
        assert_eq!(preview_text_range("ab你好cd", 4, 2), "");
        assert_eq!(preview_text_range("ab你好cd", 4, 99), "cd");
    }

    #[test]
    fn snippet_run_buffer_appends_single_enter() {
        assert_eq!(
            snippet_run_buffer("sudo apt update").unwrap(),
            b"sudo apt update\r"
        );
    }

    #[test]
    fn snippet_run_buffer_trims_outer_blank_lines() {
        assert_eq!(snippet_run_buffer("\n\ncmd\r\n").unwrap(), b"cmd\r");
    }

    #[test]
    fn snippet_run_buffer_preserves_internal_script_lines() {
        assert_eq!(snippet_run_buffer("one\ntwo").unwrap(), b"one\rtwo\r");
    }

    #[test]
    fn snippet_run_buffer_ignores_blank_body() {
        assert!(snippet_run_buffer("\n \r\n\t").is_none());
    }

    #[test]
    fn wrap_snippet_text_wraps_at_word_boundaries() {
        let lines = wrap_snippet_text_for_width("sudo apt update", 8, false, |s| {
            s.chars().count() as f32 / 8.0
        });
        assert_eq!(lines, vec!["sudo apt", "update"]);
    }

    #[test]
    fn wrap_snippet_text_falls_back_to_char_boundaries_for_long_tokens() {
        let lines =
            wrap_snippet_text_for_width("abcdef", 8, false, |s| s.chars().count() as f32 / 3.0);
        assert_eq!(lines, vec!["abc", "def"]);
    }

    #[test]
    fn wrap_snippet_text_preserves_explicit_newlines() {
        let lines = wrap_snippet_text_for_width("one\ntwo", 8, false, |_| 0.5);
        assert_eq!(lines, vec!["one", "two"]);
    }

    #[test]
    fn wrap_snippet_text_shows_tail_when_focused() {
        let lines = wrap_snippet_text_for_width("one\ntwo\nthree", 2, true, |_| 0.5);
        assert_eq!(lines, vec!["two", "three"]);
    }

    #[test]
    fn wrap_snippet_text_shows_head_when_unfocused() {
        let lines = wrap_snippet_text_for_width("one\ntwo\nthree", 2, false, |_| 0.5);
        assert_eq!(lines, vec!["one", "two"]);
    }

    #[test]
    fn wrap_snippet_text_stops_after_visible_unfocused_lines() {
        let text = format!("{}\nshould-not-be-measured", "abcdefghij ".repeat(1000));
        let mut calls = 0;
        let lines = wrap_snippet_text_for_width(&text, 2, false, |s| {
            calls += 1;
            s.chars().count() as f32 / 5.0
        });
        assert_eq!(lines.len(), 2);
        assert!(calls < 100, "measure called {} times", calls);
    }

    #[test]
    fn wrap_snippet_text_focus_uses_bounded_tail_lines() {
        let text = (0..1000)
            .map(|idx| format!("line{idx}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut calls = 0;
        let lines = wrap_snippet_text_for_width(&text, 2, true, |_| {
            calls += 1;
            0.5
        });
        assert_eq!(lines, vec!["line998", "line999"]);
        assert!(calls < 20, "measure called {} times", calls);
    }

    #[test]
    fn snippet_cursor_visible_alternates_by_blink_period() {
        assert!(snippet_cursor_visible(0, 500));
        assert!(snippet_cursor_visible(499, 500));
        assert!(!snippet_cursor_visible(500, 500));
        assert!(!snippet_cursor_visible(999, 500));
        assert!(snippet_cursor_visible(1000, 500));
    }
}
