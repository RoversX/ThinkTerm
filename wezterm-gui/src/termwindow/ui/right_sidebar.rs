use crate::markdown_editor::{
    build_spell_check_chunks_in_range, build_visual_document, fit_table_columns, load_remote_image,
    open_vault_document, resolve_local_image, save_document_revision, vault_file_paths,
    vault_markdown_paths, wrap_visual_document_by_width_cached, AutosaveWakeAction, BlockKind,
    EditorMode, InlineStyle, NoteCodeBlockLayout, NoteLineGeometry, NoteLineLayout, NoteRunLayout,
    NoteSpellingIssue, ProjectedCodeBlock, ProjectedObject, SaveState, SourceSelection,
    TableAlignment, VisualDocument, VisualLineKind,
};
use crate::quad::{
    QuadClipRect, QuadTrait, TripleLayerQuadAllocator, TripleLayerQuadAllocatorTrait,
};
use crate::termwindow::remote_files::{
    download_name_candidates, invalidate_remote_connection, invalidate_remote_connection_if_dead,
    local_path_is_occupied, remote_connection_key, remote_connection_manager,
    reserve_download_directory, reserve_download_path, RemoteAcquireError, RemoteFileBytes,
    RemoteFileEntry, RemoteFileKind, RemoteFileRow, RemoteFilesEffect, RemoteFilesEvent,
    RemoteFilesPhase, RemoteOperationOrigin, RemotePath, RemoteProjectListing, RemoteTransfer,
    RemoteTransferKind, RemoteTransferProgress, RemoteTransferSource, RemoteTransferStatus,
    TransferFailure, REMOTE_TRANSFER_CANCELED,
};
use crate::termwindow::remote_walk::{
    plan_remote_walk, RemoteWalkEntry, RemoteWalkMode, RemoteWalkPlan,
};
use crate::termwindow::transfer_walk::{
    apply_conflict_choice, destination_escapes_source, destination_stays_within, is_same_file,
    plan_transfer, ConflictChoice, DestinationRoot, OverwritePolicy, TransferEntryKind,
    TransferPlanError, TRANSFER_CONFIRM_THRESHOLD,
};
use crate::termwindow::ui::folder_problem::FolderProblem;
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
    NoteEditorCommand, PanelId, PendingLocalCopy, PendingRemoteConfirm, RightSidebarFileCharBag,
    RightSidebarFileDirCache, RightSidebarFileDirEntry, RightSidebarFileField,
    RightSidebarFileIndex, RightSidebarFileIndexEntry, RightSidebarFileIndexStatus,
    RightSidebarFilePreviewImage, RightSidebarFilePreviewLine, RightSidebarFilePreviewSelection,
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
use fluent_bundle::FluentArgs;
use mux::pane::{Pane, PaneId};
use mux::Mux;
use std::borrow::Cow;
use std::cmp::Ordering;
use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::fs::{self, OpenOptions};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};
use termwiz::image::{ImageData, ImageDataType};
use termwiz::input::{KeyCode as TermKeyCode, Modifiers as TermModifiers};
use thinkterm_syntax::{HighlightKind, HighlightResult, HighlightSpan, LanguageId};
use unicode_segmentation::UnicodeSegmentation;
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
/// The largest recording buffer `paint_right_sidebar_contents` keeps for the
/// next frame: a long note or preview at a small font.
const RIGHT_SIDEBAR_SCRATCH_KEEP_BYTES: usize = 8 << 20;
const RIGHT_SIDEBAR_CLOSE_BUTTON_SIZE: usize = 58;
const RIGHT_SIDEBAR_CLOSE_ICON_SIZE: usize = 27;
const RIGHT_SIDEBAR_CLOSE_BUTTON_X_ADJUST: usize = 8;
const RIGHT_SIDEBAR_CLOSE_BUTTON_Y_ADJUST: usize = 16;
const RIGHT_SIDEBAR_MODE_HEIGHT: usize = 72;
/// The most panels the selector names the active one of: past this its
/// label has no room, and every panel is its icon alone, the active one
/// told by its pill. The browser's selector does the same
/// (thinkterm-web/src/agents.rs).
const RIGHT_SIDEBAR_LABELED_MODES: usize = 5;
const RIGHT_SIDEBAR_EMPTY_HEIGHT: usize = 88;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SidebarMessageLayout {
    height: usize,
    visible_lines: usize,
}

fn sidebar_message_layout(
    available_height: usize,
    minimum_height: usize,
    line_height: usize,
    vertical_inset: usize,
    line_count: usize,
) -> SidebarMessageLayout {
    let desired = minimum_height.max(
        line_height
            .saturating_mul(line_count)
            .saturating_add(vertical_inset.saturating_mul(2)),
    );
    let height = desired.min(available_height);
    let visible_lines = if line_height == 0 {
        0
    } else {
        let fitted = height.saturating_sub(vertical_inset.saturating_mul(2)) / line_height;
        // Keep the first line whenever the box is a line tall at all: the
        // single-line callers ("No files", ...) predate the multi-line form
        // and must not squeeze down to an icon and an empty box.
        if fitted == 0 && height >= line_height {
            1
        } else {
            fitted
        }
    }
    .min(line_count);
    SidebarMessageLayout {
        height,
        visible_lines,
    }
}
/// Preview header geometry, in design pixels. These were the last raw literals
/// left in this file after the scaling sweep: the sweep went file by file and
/// this block reads like plain arithmetic, so it was missed. On a 0.5-scale
/// display the buttons ended up twice the size of everything around them.
const PREVIEW_HEADER_TOP_GAP: usize = 4;
const PREVIEW_HEADER_BUTTON: usize = 44;
const PREVIEW_HEADER_ACTION_GAP: usize = 6;
/// Icon + inner padding + gaps that the "Open With" label sits inside.
const PREVIEW_OPEN_LABEL_CHROME: usize = 12 + 10 + 36 + 6;
/// Room kept for the filename before the action buttons may grow.
const PREVIEW_HEADER_NAME_RESERVE: usize = 72;
const PREVIEW_OPEN_BUTTON_MIN: usize = 80;
/// Narrower than this and the "Open With" button drops its dropdown arrow.
const PREVIEW_OPEN_SPLIT_MIN_WIDTH: usize = 112;
const PREVIEW_OPEN_ARROW_WIDTH: usize = 36;
/// Empty-state geometry for the remote Files panel, in design pixels.
const REMOTE_EMPTY_ICON_SIZE: usize = 40;
const REMOTE_EMPTY_ICON_GAP: usize = 18;
const REMOTE_EMPTY_DETAIL_GAP: usize = 6;
const REMOTE_EMPTY_BUTTON_GAP: usize = 22;
const REMOTE_EMPTY_BUTTON_HEIGHT: usize = 44;
/// Toolbar control height.
const SNIPPET_TOOLBAR_HEIGHT: usize = 58;
const SNIPPET_SEARCH_HEIGHT: usize = 58;
const SNIPPET_CARD_HEIGHT: usize = 116;
const SNIPPET_EDITOR_HEADER_HEIGHT: usize = 108;
const SNIPPET_FIELD_HEIGHT: usize = 56;
const SNIPPET_BODY_FIELD_HEIGHT: usize = 190;
const SNIPPET_SAVE_BUTTON_HEIGHT: usize = 54;
const SNIPPET_ACTION_BUTTON_MIN_WIDTH: usize = 92;
const SNIPPET_ACTION_BUTTON_HEIGHT: usize = 46;
/// Gap between snippet cards. Shared with the Agents list so the two
/// panels space their rows identically rather than by coincidence.
pub(crate) const SNIPPET_ROW_GAP: usize = 16;
const SNIPPET_LIST_TOP_GAP: usize = 18;
const SNIPPET_LIST_BOTTOM_PADDING: usize = 40;
const RIGHT_SIDEBAR_SCROLLBAR_VISIBLE_MS: u64 = 900;
const SNIPPET_CARET_WIDTH: f32 = 3.0;
const NOTE_TOOLBAR_HEIGHT: usize = 54;
const NOTE_BODY_TOP_GAP: usize = 12;
const NOTE_BODY_PADDING: usize = 24;
/// The widest a rendered Markdown file preview's text runs.
const MARKDOWN_PREVIEW_READING_MAX_WIDTH: usize = 1500;
const NOTE_LINE_GAP: usize = 5;
const NOTE_CARET_WIDTH: f32 = 2.0;
const NOTE_TABLE_CELL_HORIZONTAL_PADDING: usize = 10;
const NOTE_TABLE_CELL_VERTICAL_PADDING: usize = 7;
const NOTE_TABLE_MIN_COLUMN_WIDTH: usize = 120;
const NOTE_TABLE_MAX_COLUMN_WIDTH: usize = 480;
const NOTE_TABLE_SCROLLBAR_HEIGHT: usize = 3;
const NOTE_CODE_HEADER_HEIGHT: usize = 38;
/// A code block's header names its language in small print, a caption to the
/// code rather than a heading over it: this much of the note's text size.
const NOTE_CODE_LABEL_SCALE: f64 = 0.8;
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
/// How far a Markdown image may be enlarged to fill the column.
const NOTE_IMAGE_MAX_UPSCALE: f32 = 3.0;
/// How long the width diagrams are shown at must hold still before they are
/// drawn again for it, so a drag redraws them once, at its end.
const NOTE_DIAGRAM_SETTLE: Duration = Duration::from_millis(300);
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
pub(crate) const FILE_SCROLL_FADE_HEIGHT: usize = 32;
const FILE_PREVIEW_HEADER_HEIGHT: usize = 64;
/// How many transfer rows the strip shows before it starts dropping the
/// oldest finished ones. The strip eats into the tree, so it stays small;
/// running transfers are never dropped, however many there are.
const REMOTE_TRANSFER_STRIP_MAX: usize = 3;

#[derive(Clone)]
pub(crate) struct TerminalPasteTarget {
    pane_id: PaneId,
    remote: Option<Result<RemoteTerminalPasteTarget, String>>,
}

#[derive(Clone)]
struct RemoteTerminalPasteTarget {
    space_id: String,
    target: workspace_threads::RemoteFilesTarget,
    source_key: String,
    connection_key: String,
    config: config::SshDomain,
    destination: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TerminalPasteSnapshotMismatch {
    Space,
    Project,
    Connection,
    Pane,
}

fn terminal_paste_snapshot_mismatch(
    captured_space: &str,
    current_space: &str,
    captured_target: &workspace_threads::RemoteFilesTarget,
    current_target: Option<&workspace_threads::RemoteFilesTarget>,
    captured_connection_key: &str,
    current_connection_key: Option<&str>,
    pane_matches_host: bool,
) -> Option<TerminalPasteSnapshotMismatch> {
    if captured_space != current_space {
        Some(TerminalPasteSnapshotMismatch::Space)
    } else if current_target != Some(captured_target) {
        Some(TerminalPasteSnapshotMismatch::Project)
    } else if current_connection_key != Some(captured_connection_key) {
        Some(TerminalPasteSnapshotMismatch::Connection)
    } else if !pane_matches_host {
        Some(TerminalPasteSnapshotMismatch::Pane)
    } else {
        None
    }
}

impl TerminalPasteTarget {
    pub(crate) fn pane_id(&self) -> PaneId {
        self.pane_id
    }
}

struct StagedPastedImage {
    directory: tempfile::TempDir,
    path: PathBuf,
    #[cfg(test)]
    worker_thread_id: std::thread::ThreadId,
}

impl StagedPastedImage {
    fn keep(self) -> PathBuf {
        let path = self.path;
        let _ = self.directory.keep();
        path
    }
}
/// Thickness of the progress track under a running transfer's label. Five
/// design pixels remains compact while being visible at a glance.
const REMOTE_TRANSFER_PROGRESS_HEIGHT: usize = 5;
static REMOTE_TRANSFER_ANIMATION_EPOCH: OnceLock<Instant> = OnceLock::new();

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RemoteLeaseFailureDisposition {
    FailedConnectionInstalled,
    ReplacementForSameTarget,
    ReplacementForDifferentTarget,
    NoLease,
}

fn remote_lease_failure_disposition(
    current: Option<(&str, u64)>,
    failed_key: &str,
    failed_id: u64,
) -> RemoteLeaseFailureDisposition {
    match current {
        Some((key, id)) if key == failed_key && id == failed_id => {
            RemoteLeaseFailureDisposition::FailedConnectionInstalled
        }
        Some((key, _)) if key == failed_key => {
            RemoteLeaseFailureDisposition::ReplacementForSameTarget
        }
        Some(_) => RemoteLeaseFailureDisposition::ReplacementForDifferentTarget,
        None => RemoteLeaseFailureDisposition::NoLease,
    }
}

const FILE_PREVIEW_MAX_BYTES: usize = 256 * 1024;
fn right_sidebar_arg(id: &'static str, name: &'static str, value: impl Into<String>) -> String {
    let mut args = FluentArgs::new();
    args.set(name, value.into());
    crate::i18n::tr_args(id, &args)
}
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
/// A plugin's extended view: 360 and 560 points at the least and to start
/// with, as in a browser. Twice the file preview's: it is there for what a
/// sidebar has no room for.
const PLUGIN_EXTENDED_MIN_WIDTH: usize = FILE_PREVIEW_PANE_MIN_WIDTH * 2;
const PLUGIN_EXTENDED_DEFAULT_WIDTH: usize = FILE_PREVIEW_PANE_DEFAULT_WIDTH * 2;
const FILE_PREVIEW_SLICE_CACHE_CAPACITY: usize = 256;
// Lines up to this many columns are shaped whole (once, cached) so horizontal
// scrolling is pure translation of the cached glyph run instead of re-shaping a
// new substring per step. Longer lines fall back to the per-window slice path to
// keep the one-time shaping cost bounded.
const FILE_PREVIEW_FULL_LINE_SHAPE_MAX_COLS: usize = 4096;
const FILE_PREVIEW_SCROLLBAR_THICKNESS: usize = 4;
const FILE_PREVIEW_SCROLLBAR_HIT_SLOP: usize = 6;
const FILE_TREE_ROW_LIMIT: usize = 2000;
const FILE_INDEX_ENTRY_LIMIT: usize = thinkterm_file_index::ENTRY_LIMIT;
// How long the file panel must stay closed/idle before its in-memory index and
// buffers are released. Reopening within this window keeps everything resident.
const FILE_INDEX_IDLE_RELEASE_SECS: u64 = 30;
const NOTE_IDLE_RELEASE_SECS: u64 = 30;
/// How long a snippet's Run or Paste waits for its text: a command that
/// arrives later than this is not the one the press was for.
const SNIPPET_SEND_WAIT: Duration = Duration::from_secs(3);
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

/// What a re-scan should actually do, now that browsing and search no longer
/// share one walk of the project.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RescanPlan {
    /// Re-read the directories the tree is currently showing.
    reread_loaded_dirs: bool,
    /// Rebuild the search index. Only ever a *refresh* of one that exists.
    refresh_search_index: bool,
}

/// Split out so the two rules that are easy to regress stay pinned by tests:
///
/// * the tree half never depends on the index state — a user who never searches
///   sits at "no index" forever, and gating on it would freeze their tree;
/// * the index half never *creates* an index — doing so would reinstate the
///   eager whole-project walk, on a 90-second timer no less.
fn rescan_plan(view_active: bool, index_ready: bool, index_refreshing: bool) -> RescanPlan {
    RescanPlan {
        reread_loaded_dirs: view_active,
        refresh_search_index: view_active && index_ready && !index_refreshing,
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
    max_upscale: f32,
) -> (f32, f32) {
    if image_width == 0 || image_height == 0 || available_width <= 0.0 || available_height <= 0.0 {
        return (0.0, 0.0);
    }

    // Markdown images should be useful at reading distance, including small
    // logos, so they may be enlarged (up to `max_upscale`) while both axes
    // stay bounded; giant/tall images remain inside the current viewport.
    let scale = (available_width / image_width as f32)
        .min(available_height / image_height as f32)
        .min(max_upscale);
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
            note_image_display_size(
                image.width,
                image.height,
                available_width,
                available_height,
                image.max_upscale,
            )
            .1
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
    /// What the block's header says: its language, and for a Mermaid block
    /// that cannot be drawn, that it cannot.
    label: String,
    code: Arc<ProjectedCodeBlock>,
    row_index: usize,
    row_count: usize,
    collapsed: bool,
    horizontal_offset: f32,
    block_height: f32,
    max_horizontal_scroll: f32,
}

/// Why the Notes panel cannot show its vault.
///
/// Classified on the worker thread that already touched the filesystem, for two
/// reasons: painting may never do IO, and by the time an error reaches the
/// window thread the distinction is gone -- a deleted folder and a macOS TCC
/// denial both arrive as an `anyhow` chain ending in a bare `os error`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NoteVaultProblem {
    /// The vault folder itself is gone: renamed, deleted, or on a volume that
    /// is not mounted.
    MissingRoot,
    /// The folder is there but the system refuses to open it. On macOS this is
    /// almost always TCC -- the vault sits under Desktop / Documents /
    /// Downloads and ThinkTerm was never granted access to it.
    UnreadableRoot,
    /// The folder lists fine; this one note would not load.
    Note,
    /// Any other IO failure.
    Other,
}

impl NoteVaultProblem {
    /// The folder-level view of this problem, for the cases the shared
    /// classifier owns. `Note` has none: the vault folder listed fine and it
    /// was one document that would not load.
    fn as_folder_problem(self) -> Option<FolderProblem> {
        match self {
            Self::MissingRoot => Some(FolderProblem::MissingRoot),
            Self::UnreadableRoot => Some(FolderProblem::UnreadableRoot),
            Self::Other => Some(FolderProblem::Other),
            Self::Note => None,
        }
    }

    pub(crate) fn icon(self) -> SvgIcon {
        match self.as_folder_problem() {
            Some(problem) => problem.icon(),
            // A note that will not open is a genuine fault, not a folder we
            // cannot get into.
            None => SvgIcon::CircleAlert,
        }
    }

    /// The headline: what went wrong, in the user's language, instead of the
    /// `anyhow` chain that used to be the whole panel.
    ///
    /// Not delegated to [`FolderProblem::title`]: this panel can say "Notes
    /// vault" where the shared wording can only say "folder". The permission
    /// case shares the one string that is already exactly right.
    pub(crate) fn title(self) -> String {
        crate::i18n::tr(match self {
            Self::MissingRoot => "right-notes-vault-missing",
            Self::UnreadableRoot => "folder-unreadable",
            Self::Note => "right-notes-note-unreadable",
            Self::Other => "right-notes-open-error",
        })
    }

    /// What to do about it; shared so every panel tells the same story.
    pub(crate) fn hint(self) -> Option<String> {
        self.as_folder_problem().and_then(FolderProblem::hint)
    }
}

/// A classified vault failure and the raw error behind it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NoteVaultFailure {
    pub(crate) problem: NoteVaultProblem,
    /// The full `anyhow` chain, shown verbatim and wrapped. It names the exact
    /// path and the exact refusal, which is precisely what the old single-line
    /// ellipsized message cut off.
    pub(crate) detail: String,
}

/// Map the outcome of listing the vault directory to a classification.
///
/// Pure, so the mapping is testable without a filesystem; the caller owns the
/// `read_dir` and therefore which thread it happens on.
fn problem_for_read_dir(kind: Option<std::io::ErrorKind>) -> NoteVaultProblem {
    match kind {
        // The directory listed, so the vault is reachable and it was the note
        // itself that failed.
        None => NoteVaultProblem::Note,
        Some(kind) => match FolderProblem::from_read_dir_kind(kind) {
            FolderProblem::MissingRoot => NoteVaultProblem::MissingRoot,
            FolderProblem::UnreadableRoot => NoteVaultProblem::UnreadableRoot,
            FolderProblem::Other => NoteVaultProblem::Other,
        },
    }
}

/// Collapse the two failure layers a classified worker produces. The outer one
/// is the worker thread failing to run at all, which says nothing about the
/// vault -- `spawn_into_new_thread` requires an `anyhow::Result`, so the real
/// classification has to travel inside its Ok payload.
fn flatten_vault_worker_result<T>(
    result: anyhow::Result<Result<T, NoteVaultFailure>>,
) -> Result<T, NoteVaultFailure> {
    match result {
        Ok(inner) => inner,
        Err(err) => Err(NoteVaultFailure {
            problem: NoteVaultProblem::Other,
            detail: format!("{err:#}"),
        }),
    }
}

/// Probe the vault root to classify a failure that just happened.
///
/// Touches the filesystem, so it belongs on the worker that failed -- never on
/// the paint path.
fn classify_vault_failure(vault_root: &Path, err: &anyhow::Error) -> NoteVaultFailure {
    NoteVaultFailure {
        problem: problem_for_read_dir(fs::read_dir(vault_root).err().map(|err| err.kind())),
        detail: format!("{err:#}"),
    }
}

/// Where the note painter draws. The Note is the right sidebar's editor; the
/// file preview renders a Markdown file read-only with the same painter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NoteSurface {
    Note,
    FilePreview,
}

/// One particular surface: which kind, and which instance of it. A file
/// preview is rebuilt for every file it shows, so background work records the
/// instance it started on and a result for an earlier one is dropped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct NoteSurfaceTicket {
    pub(crate) surface: NoteSurface,
    instance: u64,
}

static NEXT_MARKDOWN_PREVIEW_INSTANCE: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(1);

/// Everything the note painter draws a surface from. The Note's lives in the
/// `right_sidebar_note*` fields the painter and its helpers use; another
/// surface keeps its own here and is installed into those fields only for the
/// span of `TermWindow::with_note_surface`.
#[derive(Default)]
pub(crate) struct NoteSurfaceState {
    host: crate::markdown_editor::NoteHostState,
    table_horizontal_offsets: std::collections::BTreeMap<usize, f32>,
    table_layouts: Vec<RightSidebarNoteTableLayout>,
    code_highlight: NoteCodeHighlightState,
    paint_cache: NotePaintCache,
}

/// The file preview's rendered Markdown: which preview it renders and the
/// read-only surface it renders into. Dropped when the preview closes, the
/// file changes, the preview switches to source, or the panel's memory is
/// released.
pub(crate) struct MarkdownPreviewSurface {
    key: MarkdownPreviewKey,
    /// Unique per surface built; never 0, which is the Note's.
    instance: u64,
    state: NoteSurfaceState,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct MarkdownPreviewKey {
    /// The previewed file, local or remote.
    path: String,
    /// Bumped by every preview load, so a reload of the same path re-renders.
    generation: u64,
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

/// Idle pre-warmer for the Note shape cache. Painting shapes only the
/// visible/overscan lines, so a fast flick into never-seen content pays the
/// full shaping cost (~2ms per CJK line) mid-scroll. After a wrapped document
/// lands, this walks the whole document a few milliseconds per step and
/// shapes every run into the Note cache, after which any scroll position is
/// a cache hit.
pub(crate) struct NotePrewarmState {
    /// Weak: the warmer must never keep a replaced document's visual alive.
    visual: std::sync::Weak<VisualDocument>,
    fonts: NotePrewarmFonts,
    next_line: usize,
    shaped_runs: usize,
    scheduled: bool,
}

/// The exact fonts the Note painter would pick per run — shaping with
/// anything else would warm entries the painter never looks up. Mirrors the
/// per-run font match in the paint loop; keep the two in sync.
#[derive(Clone)]
pub(crate) struct NotePrewarmFonts {
    pub ui: (Rc<LoadedFont>, RenderMetrics),
    pub bold: (Rc<LoadedFont>, RenderMetrics),
    pub italic: (Rc<LoadedFont>, RenderMetrics),
    pub bold_italic: (Rc<LoadedFont>, RenderMetrics),
    pub h1: (Rc<LoadedFont>, RenderMetrics),
    pub h2: (Rc<LoadedFont>, RenderMetrics),
    pub h3: (Rc<LoadedFont>, RenderMetrics),
    pub h1_italic: (Rc<LoadedFont>, RenderMetrics),
    pub h2_italic: (Rc<LoadedFont>, RenderMetrics),
    pub h3_italic: (Rc<LoadedFont>, RenderMetrics),
    pub code: (Rc<LoadedFont>, RenderMetrics),
}

impl NotePrewarmFonts {
    fn for_run(&self, block: BlockKind, style: &InlineStyle) -> (&Rc<LoadedFont>, &RenderMetrics) {
        let pair = match block {
            BlockKind::Heading(1) if style.emphasis => &self.h1_italic,
            BlockKind::Heading(2) if style.emphasis => &self.h2_italic,
            BlockKind::Heading(_) if style.emphasis => &self.h3_italic,
            BlockKind::Heading(1) => &self.h1,
            BlockKind::Heading(2) => &self.h2,
            BlockKind::Heading(_) => &self.h3,
            BlockKind::CodeBlock => &self.code,
            _ if style.strong && style.emphasis => &self.bold_italic,
            _ if style.strong => &self.bold,
            _ if style.emphasis => &self.italic,
            _ => &self.ui,
        };
        (&pair.0, &pair.1)
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
    /// The preview text exactly as loaded, before display sanitization
    /// (tab expansion, control stripping). Copying must reproduce this —
    /// spaces where a Makefile had tabs is a different file.
    raw_text: Option<String>,
}

#[derive(Debug, Clone)]
struct RightSidebarFileRoot {
    project_name: String,
    path: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RightSidebarFileRowMetrics {
    pub(crate) row_height: usize,
    pub(crate) icon_size: usize,
    chevron_size: usize,
    indent_step: usize,
    pub(crate) icon_gap: usize,
}

impl RightSidebarMode {
    /// Selector order, and the order `fall_back_to_an_enabled_panel` walks --
    /// so the first entry is where a disabled panel falls back to.
    const ALL: [Self; 4] = [Self::Chat, Self::Tasks, Self::Snippets, Self::Agents];

    /// The panels the selector offers, in order. Reading the toggles here
    /// rather than hard-coding the list is what lets Settings hide a panel;
    /// the plugins' panels follow ThinkTerm's own, as the plugin host last
    /// listed them.
    pub(crate) fn enabled_panels() -> Vec<Self> {
        let toggles = crate::native_settings::right_sidebar_panel_toggles();
        let plugins = crate::native_settings::plugin_panels();
        Self::ALL
            .iter()
            .copied()
            .filter(|panel| panel.enabled_with(&toggles))
            .chain(
                plugins
                    .iter()
                    .filter_map(|panel| Some(Self::Plugin(PanelId::new(&panel.id)?))),
            )
            .collect()
    }

    /// Whether the sidebar offers anything at all. Hot: `right_sidebar_width`
    /// asks it, and that runs several times per frame and again on every
    /// pointer move over the tab bar -- so one settings read, no Vec, and it
    /// stops at the first panel that is on.
    pub(crate) fn any_panel_enabled() -> bool {
        let toggles = crate::native_settings::right_sidebar_panel_toggles();
        toggles.plugins || Self::ALL.iter().any(|panel| panel.enabled_with(&toggles))
    }

    pub(crate) fn panel_enabled(self) -> bool {
        self.enabled_with(&crate::native_settings::right_sidebar_panel_toggles())
    }

    fn enabled_with(self, toggles: &crate::native_settings::RightSidebarPanelToggles) -> bool {
        match self {
            Self::Chat => toggles.files,
            Self::Tasks => toggles.notes,
            Self::Snippets => toggles.snippets,
            // Not the panel toggle alone: this one also gates the detector,
            // and that master switch is a Lua config value rather than a
            // native setting, so this arm takes a second read. `ALL` puts it
            // last, where the short-circuit in `any_panel_enabled` reaches it
            // only when the other three are off.
            Self::Agents => crate::agent_status::enabled(),
            Self::Plugin(_) => toggles.plugins && self.plugin_panel().is_some(),
        }
    }

    /// The plugin panel this mode shows, as last heard.
    pub(crate) fn plugin_panel(self) -> Option<crate::native_settings::NativePluginPanel> {
        let Self::Plugin(id) = self else {
            return None;
        };
        crate::native_settings::plugin_panels()
            .into_iter()
            .find(|panel| panel.id == id.as_str())
    }

    fn icon(self) -> SvgIcon {
        match self {
            Self::Chat => SvgIcon::FolderTree,
            Self::Tasks => SvgIcon::NotebookTabs,
            Self::Snippets => SvgIcon::CodeXml,
            Self::Agents => SvgIcon::Bot,
            Self::Plugin(_) => self
                .plugin_panel()
                .map_or(SvgIcon::Puzzle, |panel| SvgIcon::for_panel(&panel.icon)),
        }
    }

    fn label(self) -> String {
        match self {
            Self::Chat => crate::i18n::tr("right-mode-files"),
            Self::Tasks => crate::i18n::tr("right-mode-notes"),
            Self::Snippets => crate::i18n::tr("right-mode-snippets"),
            Self::Agents => crate::i18n::tr("right-mode-agents"),
            Self::Plugin(_) => self
                .plugin_panel()
                .map(|panel| panel.name)
                .unwrap_or_default(),
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

/// How wide the user made plugin `plugin`'s extended view, in window
/// pixels at `dpi`; 0 when they never did.
pub fn right_sidebar_plugin_extended_width_for(plugin: &str, dpi: usize) -> usize {
    crate::native_settings::plugin_extended_width(plugin)
        .map_or(0, |width| scale_ui_usize(width, dpi))
}

fn file_preview_close_requires_reflow(
    preview_was_visible: bool,
    previous_width: usize,
    current_width: usize,
) -> bool {
    preview_was_visible || previous_width != current_width
}

impl crate::TermWindow {
    fn right_sidebar_file_preview_active(&self) -> bool {
        self.right_sidebar_presented()
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
        if !self.right_sidebar_presented()
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
        self.right_sidebar_presented()
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

    /// Whether the sidebar shows a plugin's panel that asks for its
    /// extended view. Only the panel of the plugin its mode names: one left
    /// open from the mode before, until the next paint lets it go, would
    /// keep its room for it, and the switch would not lay the terminal out
    /// again.
    fn right_sidebar_plugin_extended_active(&self) -> bool {
        let RightSidebarMode::Plugin(id) = self.right_sidebar_mode else {
            return false;
        };
        self.right_sidebar_presented()
            && self
                .right_sidebar_plugin
                .as_ref()
                .is_some_and(|panel| panel.plugin() == id.as_str() && panel.wants_extended())
    }

    /// The most a plugin's extended view can have beside the sidebar; none
    /// when that is less than it takes.
    fn right_sidebar_plugin_extended_room(&self) -> Option<usize> {
        let max = self
            .right_sidebar_pane_total_max_width()
            .saturating_sub(self.right_sidebar_tree_width());
        (max >= self.ui_px(PLUGIN_EXTENDED_MIN_WIDTH)).then_some(max)
    }

    /// Whether a plugin's panel would get its extended view were it to ask
    /// for one: there is room for it beside the sidebar.
    pub(crate) fn right_sidebar_plugin_can_extend(&self) -> bool {
        self.right_sidebar_plugin_extended_room().is_some()
    }

    fn right_sidebar_plugin_extended_width(&self) -> Option<usize> {
        if !self.right_sidebar_plugin_extended_active() {
            return None;
        }
        let max = self.right_sidebar_plugin_extended_room()?;
        let chosen = match self.right_sidebar_plugin.as_ref()?.extended_width {
            0 => self.ui_px(PLUGIN_EXTENDED_DEFAULT_WIDTH),
            width => width,
        };
        Some(chosen.clamp(self.ui_px(PLUGIN_EXTENDED_MIN_WIDTH), max))
    }

    /// Whether the sidebar has anything to show. Turning off every panel in
    /// Settings hides it outright rather than leaving an empty selector: the
    /// terminal reclaims the space through the same zero-width path collapse
    /// already uses, so nothing downstream needs its own special case.
    pub(crate) fn right_sidebar_has_panels(&self) -> bool {
        RightSidebarMode::any_panel_enabled()
    }

    /// The width the TERMINAL is laid out against. Zero whenever the sidebar
    /// is collapsed, hover reveal or not: a hover must never reflow a PTY.
    /// Drawing and hit-testing ask `right_sidebar_presented_width`.
    pub fn right_sidebar_width(&self) -> usize {
        if self.right_sidebar_collapsed {
            0
        } else {
            self.right_sidebar_docked_width()
        }
    }

    /// The width the panel has when it is on screen, docked or not.
    fn right_sidebar_docked_width(&self) -> usize {
        if !self.right_sidebar_has_panels() {
            return 0;
        }
        // The file preview pane, the Note pane and a plugin's extended view
        // are mutually exclusive (different sidebar modes); at most one is
        // non-zero.
        let pane_width = self
            .right_sidebar_file_preview_width()
            .or_else(|| self.right_sidebar_note_pane_width())
            .or_else(|| self.right_sidebar_plugin_extended_width())
            .unwrap_or(0);
        self.right_sidebar_tree_width()
            .saturating_add(pane_width)
            .min(if pane_width > 0 {
                self.right_sidebar_pane_total_max_width()
            } else {
                self.right_sidebar_available_width()
            })
    }

    /// The width the panel is DRAWN and HIT-TESTED at: the docked width while
    /// it is docked or hover-revealed, zero otherwise.
    pub(crate) fn right_sidebar_presented_width(&self) -> usize {
        if self.right_sidebar_presented() {
            self.right_sidebar_docked_width()
        } else {
            0
        }
    }

    /// Whether the panel is on screen at all, docked or by a hover reveal.
    /// What the panel shows follows this; the terminal's layout does not.
    pub(crate) fn right_sidebar_presented(&self) -> bool {
        !self.right_sidebar_collapsed || self.right_sidebar_hover.is_presented()
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
        // Collapsing must not instantly re-reveal under a pointer still on the
        // button, and docking makes a reveal moot.
        self.right_sidebar_hover.suppress_until_pointer_leaves();
        self.right_sidebar_hover_was_presented = false;
        self.right_sidebar_collapsed = !self.right_sidebar_collapsed;
        if self.right_sidebar_collapsed {
            self.right_sidebar_contents_scratch.clear();
            if self.right_sidebar_mode == RightSidebarMode::Tasks {
                self.clear_right_sidebar_text_focus();
            }
            self.close_plugin_panel();
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
        self.right_sidebar_hover.suppress_until_pointer_leaves();
        self.right_sidebar_hover_was_presented = false;
        self.right_sidebar_collapsed = false;
        self.kick_right_sidebar_file_rescan_cycle();
        self.request_right_sidebar_remote_files_connect(false);
        if self.right_sidebar_mode == RightSidebarMode::Tasks {
            self.right_sidebar_note_memory_release_token =
                self.right_sidebar_note_memory_release_token.wrapping_add(1);
        }
    }

    /// The right-edge strip that arms a hover reveal, as (x, y, w, h) in
    /// window pixels. Computed against the docked rect so it exists while the
    /// panel does not; like the left one it leaves out the tab bar row, where
    /// the pointer is on its way to the tabs or the sidebar button.
    pub(crate) fn right_sidebar_hover_hot_zone(&self) -> Option<(usize, usize, usize, usize)> {
        let rect = self.right_sidebar_rect_for_width(self.right_sidebar_docked_width())?;
        let top_tab_bar_height = self.tab_bar_pixel_height().unwrap_or(0.0).ceil() as usize;
        let width = self.ui_px(crate::termwindow::ui::tokens::SIDEBAR_HOVER_HOT_ZONE_WIDTH);
        let right_edge = rect.x.saturating_add(rect.width);
        crate::termwindow::sidebar_hover::hot_zone(
            right_edge.saturating_sub(width),
            rect.y,
            rect.height,
            width,
            rect.y.saturating_add(top_tab_bar_height),
        )
    }

    /// What the right sidebar's hover machine sees: the left one's rules,
    /// mirrored. The panel also holds on while one of its text fields has the
    /// keyboard -- the pointer drifting off must not take a note away mid-word.
    pub(crate) fn right_sidebar_hover_input(&self) -> crate::termwindow::sidebar_hover::HoverInput {
        use crate::termwindow::sidebar_hover::{HoverInput, PointerZone};
        // A menu, modal or rename opened from the revealed panel holds it
        // out; one open while it is away keeps it from revealing.
        let overlay_open = self.modal.borrow().is_some()
            || self.context_menu.is_some()
            || self.native_context_menu_open
            || self.inline_tab_rename.is_some();
        let eligible = self.right_sidebar_collapsed
            && self.right_sidebar_has_panels()
            && !self.content_view_foreground()
            && !self.content_view_transition_running()
            && !self.frontend_surface_blocked()
            && (!overlay_open || self.right_sidebar_hover.is_presented());
        let pointer = match &self.current_mouse_event {
            None => PointerZone::Away,
            Some(event) => match right_sidebar_hover_zone(
                event.coords.x,
                event.coords.y,
                self.right_sidebar_rect(),
                self.right_sidebar_hover_hot_zone(),
                self.ui_px(crate::termwindow::ui::tokens::SIDEBAR_HOVER_STICKY_ZONE_WIDTH),
                self.ui_items.iter().any(|item| {
                    matches!(item.item_type, UIItemType::RightSidebarToggle)
                        && item.hit_test(event.coords.x, event.coords.y)
                }),
            ) {
                // The rightmost pane's scrollbar runs down this edge too; a
                // pointer on its thumb is after the scrollbar, not the panel.
                // Only the thumb: the track spans the whole edge.
                PointerZone::HotZone | PointerZone::NearHotZone
                    if self.ui_items.iter().any(|item| {
                        matches!(item.item_type, UIItemType::ScrollThumb(_))
                            && item.hit_test(event.coords.x, event.coords.y)
                    }) =>
                {
                    PointerZone::Away
                }
                zone => zone,
            },
        };
        let pinned = !self.current_mouse_buttons.is_empty()
            || self.current_mouse_capture.is_some()
            || self.dragging.is_some()
            || self.sidebar_row_drag.is_some()
            || self.right_sidebar_file_drag.is_some()
            || self.pane_tab_drag.is_some()
            || overlay_open
            // Focus only holds a panel that is out: a field left focused when
            // the docked panel collapsed must not stop the next reveal.
            || (self.right_sidebar_hover.is_presented()
                && (self.right_sidebar_focused_input().is_some()
                    || self.right_sidebar_note.view.focused
                    || self.plugin_panel_has_keyboard()));
        HoverInput {
            eligible,
            pointer,
            pinned,
        }
    }

    /// Step the right sidebar's hover machine and keep what the panel shows in
    /// step with it being on screen.
    pub(crate) fn step_right_sidebar_hover(
        &mut self,
        now: Instant,
    ) -> crate::termwindow::sidebar_hover::HoverFrame {
        let input = self.right_sidebar_hover_input();
        let frame = self.right_sidebar_hover.step(input, now);
        self.sync_right_sidebar_hover_presence();
        frame
    }

    /// A hover reveal opens the panel's content the way docking does, without
    /// docking it; the panel leaving releases it the way collapsing does.
    fn sync_right_sidebar_hover_presence(&mut self) {
        let presented = self.right_sidebar_collapsed && self.right_sidebar_hover.is_presented();
        if presented == self.right_sidebar_hover_was_presented {
            return;
        }
        self.right_sidebar_hover_was_presented = presented;
        if presented {
            self.kick_right_sidebar_file_rescan_cycle();
            self.request_right_sidebar_remote_files_connect(false);
            if self.right_sidebar_mode == RightSidebarMode::Tasks {
                self.right_sidebar_note_memory_release_token =
                    self.right_sidebar_note_memory_release_token.wrapping_add(1);
            }
        } else {
            // A reveal comes and goes with the pointer, so the remote
            // connection and its preview wait for the idle release below
            // rather than going the moment the panel slides away.
            self.clear_right_sidebar_text_focus();
            self.schedule_right_sidebar_file_memory_release();
            self.schedule_right_sidebar_note_memory_release();
        }
    }

    /// The file panel (and its in-memory index) is only relevant while the right
    /// sidebar is open and in `Chat`/File mode.
    pub(crate) fn right_sidebar_file_view_active(&self) -> bool {
        self.right_sidebar_presented() && self.right_sidebar_mode == RightSidebarMode::Chat
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
        self.set_right_sidebar_file_preview_raw_text(None);

        self.right_sidebar_file_index = None;
        self.right_sidebar_file_index_status = RightSidebarFileIndexStatus::Empty;
        self.right_sidebar_remote_file_search.release();
        self.right_sidebar_file_browse_rows = Vec::new();
        self.right_sidebar_file_browse_cache_key = None;
        self.reset_right_sidebar_file_dir_cache();
        // Reopening must restore the view this teardown dropped; the index
        // status can no longer signal that, since it may never leave `Empty`.
        self.right_sidebar_file_view_needs_restore = true;
        // Remote trees survive a panel toggle so switching back is instant, but
        // a panel left hidden this long should give them back too. One that
        // went away with a hover reveal still holds its lease: hiding stashes
        // its tree first.
        self.release_right_sidebar_remote_files_if_hidden();
        self.right_sidebar_remote_files.release_cached_trees();
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
        self.clear_right_sidebar_note_failures();
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
        self.right_sidebar_note_image_epoch = self.right_sidebar_note_image_epoch.wrapping_add(1);
        self.right_sidebar_note_diagram_room = None;
        crate::markdown_editor::mermaid::release();
        self.right_sidebar_note_code_highlight = NoteCodeHighlightState::default();
        self.right_sidebar_note_paint_cache = NotePaintCache::default();
        self.ui_shape_caches.borrow_mut().clear_note();
        self.publish_ui_shape_cache_diagnostics();
    }

    fn set_right_sidebar_file_preview_raw_text(&mut self, text: Option<String>) {
        self.right_sidebar_file_preview_raw_text = text;
        self.right_sidebar_file_preview_text_generation = self
            .right_sidebar_file_preview_text_generation
            .wrapping_add(1);
        // Rendered Markdown is only ever of the text it was built from.
        self.right_sidebar_markdown_preview = None;
    }

    /// The previewed file when it is Markdown, as the key its rendering is
    /// kept under. A remote selection is the one shown when there is one.
    fn right_sidebar_previewed_markdown_path(&self) -> Option<String> {
        if let Some(path) = self.right_sidebar_remote_files.selected.as_ref() {
            return path
                .extension()
                .is_some_and(is_markdown_extension)
                .then(|| path.as_str().to_string());
        }
        let path = self.right_sidebar_file_selected.as_ref()?;
        path.extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(is_markdown_extension)
            .then(|| path.to_string_lossy().into_owned())
    }

    /// A Markdown file whose text the preview holds, so it can be rendered.
    fn right_sidebar_markdown_preview_available(&self) -> bool {
        self.right_sidebar_file_preview_message.is_none()
            && self.right_sidebar_file_preview_image.is_none()
            && self.right_sidebar_file_preview_raw_text.is_some()
            && self.right_sidebar_previewed_markdown_path().is_some()
    }

    /// Whether the file preview shows rendered Markdown right now.
    pub(crate) fn right_sidebar_markdown_preview_rendering(&self) -> bool {
        self.right_sidebar_file_preview_active()
            && self.right_sidebar_markdown_preview_available()
            && crate::native_settings::right_sidebar_markdown_preview_rendered()
    }

    pub(crate) fn toggle_right_sidebar_markdown_preview_rendered(&mut self) {
        let rendered = !crate::native_settings::right_sidebar_markdown_preview_rendered();
        if let Err(err) =
            crate::native_settings::save_right_sidebar_markdown_preview_rendered(rendered)
        {
            log::warn!("failed to save the Markdown preview mode: {err:#}");
        }
        if !rendered {
            // Source keeps nothing of the rendering.
            self.right_sidebar_markdown_preview = None;
        }
        // The choice is every window's: one showing a Markdown file switches
        // when it repaints.
        if let Some(front_end) = crate::frontend::try_front_end() {
            front_end.invalidate_all_windows();
        }
    }

    /// Build the rendered surface for the previewed text, unless the one
    /// there already renders it.
    fn ensure_right_sidebar_markdown_preview(&mut self) -> bool {
        let Some(path) = self.right_sidebar_previewed_markdown_path() else {
            return false;
        };
        if self.right_sidebar_file_preview_raw_text.is_none() {
            return false;
        }
        let key = MarkdownPreviewKey {
            path,
            generation: self.right_sidebar_file_preview_text_generation,
        };
        if self
            .right_sidebar_markdown_preview
            .as_ref()
            .is_some_and(|preview| preview.key == key)
        {
            return true;
        }
        // Copied only to build: this runs every frame.
        let Some(text) = self.right_sidebar_file_preview_raw_text.clone() else {
            return false;
        };
        let document = self.markdown_preview_document(&key.path, text);
        let mut state = NoteSurfaceState::default();
        state.host.bind_document(document);
        state.host.view.mode = EditorMode::ReadOnly;
        self.right_sidebar_markdown_preview = Some(Box::new(MarkdownPreviewSurface {
            key,
            instance: NEXT_MARKDOWN_PREVIEW_INSTANCE
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            state,
        }));
        true
    }

    /// The document a previewed Markdown file renders as: a session of its
    /// own, never registered with the Notebook's and never saved. A local
    /// file's images resolve beside it and cannot leave the project; a remote
    /// file has no local folder, so it shows none from relative paths.
    fn markdown_preview_document(
        &self,
        path: &str,
        text: String,
    ) -> crate::markdown_editor::VaultDocument {
        let remote_relative = self
            .right_sidebar_remote_files
            .selected
            .as_ref()
            .map(|selected| {
                self.right_sidebar_remote_files
                    .root
                    .as_ref()
                    .and_then(|root| remote_relative_path(root, selected))
                    .unwrap_or_else(|| selected.file_name().to_string())
            });
        markdown_preview_document(
            path,
            text,
            remote_relative,
            self.right_sidebar_file_index_root.as_deref(),
        )
    }

    /// Paint the previewed Markdown rendered, with the Note's painter on the
    /// preview's own read-only surface.
    fn paint_right_sidebar_markdown_preview(
        &mut self,
        layers: &mut TripleLayerQuadAllocator,
        ui_font: &Rc<LoadedFont>,
        ui_metrics: RenderMetrics,
        chrome: UiPalette,
        foreground: LinearRgba,
        muted_fg: LinearRgba,
        x: usize,
        top: usize,
        width: usize,
        bottom: usize,
    ) -> anyhow::Result<()> {
        let font_size = self.right_sidebar_file_preview_font_size();
        self.with_note_surface(NoteSurface::FilePreview, |term_window| {
            term_window.paint_note_editor_area(
                layers,
                ui_font,
                ui_metrics,
                chrome,
                foreground,
                muted_fg,
                x,
                top,
                width,
                bottom,
                font_size,
                false,
                SvgIcon::FolderTree,
            )
        })
        .unwrap_or(Ok(()))
    }

    /// The rendered/source switch beside a Markdown preview's close button.
    /// Returns the width it takes, gap included.
    fn paint_markdown_preview_toggle(
        &mut self,
        layers: &mut TripleLayerQuadAllocator,
        chrome: UiPalette,
        foreground: LinearRgba,
        muted_fg: LinearRgba,
        x: usize,
        y: usize,
        size: usize,
    ) -> anyhow::Result<usize> {
        if !self.right_sidebar_markdown_preview_available() {
            return Ok(0);
        }
        let icon = if crate::native_settings::right_sidebar_markdown_preview_rendered() {
            SvgIcon::CodeXml
        } else {
            SvgIcon::Eye
        };
        self.paint_files_preview_header_icon_button(
            layers,
            chrome,
            foreground,
            muted_fg,
            x,
            y,
            size,
            icon,
            UIItemType::RightSidebarFilePreviewMarkdownToggle,
        )?;
        Ok(size + self.ui_px(PREVIEW_HEADER_ACTION_GAP))
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

        // Read every restored folder in one batch. Left to the paint-time
        // "missing" pass this would discover one level per frame, so a deep
        // restored tree would visibly unfold instead of appearing at once.
        let restored: Vec<PathBuf> = self
            .right_sidebar_file_expanded
            .iter()
            .map(PathBuf::from)
            .filter(|dir| !self.right_sidebar_file_dir_cache.is_loaded(dir))
            .collect();
        self.spawn_right_sidebar_dir_reads(restored);

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
        // Held from the terminal, whatever the sidebar shows now.
        if self.plugin_panel_keys_held {
            return true;
        }
        match self.right_sidebar_mode {
            RightSidebarMode::Chat => self.right_sidebar_file_focus.is_some(),
            RightSidebarMode::Snippets => self.right_sidebar_snippet_focus.is_some(),
            RightSidebarMode::Tasks => self.right_sidebar_note.view.focused,
            // Given only by the user: a press in one of its fields, or
            // their key for it.
            RightSidebarMode::Plugin(_) => self.plugin_panel_has_keyboard(),
            RightSidebarMode::Agents => false,
        }
    }

    pub(crate) fn clear_right_sidebar_text_focus(&mut self) {
        self.release_plugin_panel_keyboard();
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

    /// Bring this window in line with a change to which panels exist.
    ///
    /// Repainting is not enough. Turning a panel off can take the sidebar's
    /// width to zero, and the terminal has to be laid out again for that or
    /// the panes stay sized around a sidebar that is no longer there. Leaving
    /// a panel also has to release what that panel was holding, the same way
    /// switching away from it by hand does -- otherwise the note focus and the
    /// file index survive a panel the user just turned off.
    pub(crate) fn right_sidebar_panels_changed(&mut self) {
        if !self.right_sidebar_mode.panel_enabled() {
            if self.right_sidebar_mode == RightSidebarMode::Tasks {
                self.clear_right_sidebar_text_focus();
                self.schedule_right_sidebar_note_memory_release();
            }
            self.fall_back_to_an_enabled_panel();
            if self.right_sidebar_file_view_active() {
                self.kick_right_sidebar_file_rescan_cycle();
                self.request_right_sidebar_remote_files_connect(false);
            } else {
                self.schedule_right_sidebar_file_memory_release();
                self.release_right_sidebar_remote_files_if_hidden();
            }
        }
        // Unconditional: the toggle that got us here has already been saved,
        // so the old width is gone and there is nothing left to compare
        // against. A settings change is rare enough to just re-lay-out.
        if let Some(window) = self.window.as_ref().cloned() {
            let dimensions = self.dimensions;
            self.apply_dimensions(&dimensions, None, &window);
            window.invalidate();
        }
    }

    /// Move off a panel that is no longer offered, onto the first one that
    /// is. All four can be off at once -- that is how you get rid of the
    /// right sidebar -- and then there is nothing to move to, so we stay put.
    pub(crate) fn fall_back_to_an_enabled_panel(&mut self) {
        // The panel with the keyboard went from under the user.
        if matches!(self.right_sidebar_mode, RightSidebarMode::Plugin(_))
            && self.plugin_panel_has_keyboard()
        {
            self.hold_plugin_panel_keys();
        }
        if let Some(first) = RightSidebarMode::enabled_panels().first().copied() {
            self.right_sidebar_mode = first;
        }
        // No panels at all: the sidebar is hidden, so the stale mode is
        // unreachable and harmless. It becomes correct again the moment a
        // panel is turned back on.
        // This frame's selector already painted for the old mode, so ask for
        // another or the body stays blank.
        if let Some(win) = self.window.as_ref() {
            win.invalidate();
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

    /// Opens the editor on a snippet once the plugin host has sent it.
    pub(crate) fn open_existing_snippet_editor(&mut self, id: &str) {
        let Some(window) = self.window.clone() else {
            return;
        };
        crate::snippets::open(id, window, |term_window, snippet| {
            if let Some(snippet) = snippet {
                term_window.show_snippet_editor(snippet);
            }
        });
    }

    fn show_snippet_editor(&mut self, snippet: crate::snippets::Snippet) {
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

    /// Hands what the editor holds to the plugin host, which decides what
    /// comes of it; the editor follows its answer.
    pub(crate) fn save_snippet_editor(&mut self) {
        let editing = self.right_sidebar_snippet_view.clone();
        let id = match &editing {
            RightSidebarSnippetView::List => return,
            RightSidebarSnippetView::EditNew => None,
            RightSidebarSnippetView::EditExisting(id) => Some(id.clone()),
        };
        // A second press waits for the first one's answer rather than
        // saving a new snippet twice.
        if self.right_sidebar_snippet_saving {
            return;
        }
        let Some(window) = self.window.clone() else {
            return;
        };
        self.right_sidebar_snippet_saving = true;
        let title = self.right_sidebar_snippet_title.text().to_string();
        let body = self.right_sidebar_snippet_body.text().to_string();
        crate::snippets::save(id, title, body, window, move |term_window, saved| {
            term_window.right_sidebar_snippet_saving = false;
            // The editor moved on meanwhile: the answer is not for it.
            if term_window.right_sidebar_snippet_view != editing {
                return;
            }
            match saved {
                Ok(crate::snippets::Saved::Saved { .. }) => term_window.close_snippet_editor(),
                Ok(crate::snippets::Saved::Empty) => {
                    term_window.right_sidebar_snippet_focus = Some(RightSidebarSnippetField::Body);
                }
                // Deleted meanwhile: what was typed is kept, as a new one.
                Ok(crate::snippets::Saved::Gone) => {
                    term_window.right_sidebar_snippet_view = RightSidebarSnippetView::EditNew;
                }
                // What was typed stays in the editor.
                Err(why) => log::error!("failed to save snippet: {why}"),
            }
        });
    }

    /// The rows the Snippets panel shows for this window's search, as the
    /// plugin host last sent them. A search it has not answered is asked
    /// about, one request at a time (thinkterm-snippets `view`).
    fn snippet_rows(&mut self) -> Vec<crate::snippets::Row> {
        let Some(window) = self.window.clone() else {
            return Vec::new();
        };
        crate::snippets::in_use(&window);
        let query = self.right_sidebar_snippet_search.text();
        let listing = &mut self.right_sidebar_snippet_listing;
        listing.set_query(&query);
        if let Some(query) = listing.next() {
            crate::snippets::list(query, window, |term_window, query, rows| {
                term_window.snippets_listed(query, rows);
            });
        }
        listing.rows().map(<[_]>::to_vec).unwrap_or_default()
    }

    fn snippets_listed(
        &mut self,
        query: String,
        rows: std::result::Result<Vec<crate::snippets::Row>, String>,
    ) {
        if let Err(why) = self.right_sidebar_snippet_listing.answered(query, rows) {
            log::warn!("snippets: {why}");
        }
    }

    /// The snippets changed, or the plugin host is back: this window's
    /// rows are asked for again.
    pub(crate) fn snippets_changed(&mut self) {
        self.right_sidebar_snippet_listing.changed();
    }

    /// The session with the plugin host was let go, and this window's rows
    /// with it.
    pub(crate) fn snippets_released(&mut self) {
        self.right_sidebar_snippet_listing.clear();
    }

    fn snippet_availability(&self) -> crate::snippets::Availability {
        if self.right_sidebar_snippet_listing.rows().is_some() {
            return crate::snippets::Availability::Ready;
        }
        match crate::snippets::trouble() {
            Some(why) => crate::snippets::Availability::Unavailable(why),
            None => crate::snippets::Availability::Loading,
        }
    }

    pub(crate) fn delete_snippet(&mut self, id: &str) {
        crate::snippets::delete(id);
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
        if !self.right_sidebar_presented()
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
        if !self.right_sidebar_presented()
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
                // Start reading now rather than waiting for the next paint to
                // notice the folder is missing; saves a frame on expand.
                self.spawn_right_sidebar_dir_reads(vec![path.clone()]);
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
        self.set_right_sidebar_file_preview_raw_text(None);
        self.right_sidebar_file_preview_max_columns = 0;
        self.clear_right_sidebar_file_preview_slice_cache();
        self.right_sidebar_file_preview_image = None;
        self.right_sidebar_file_preview_message = Some(crate::i18n::tr("right-loading-preview"));
        self.right_sidebar_file_preview_truncated = false;
        self.right_sidebar_file_preview_selection = None;
        self.right_sidebar_file_preview_scroll_offset = 0.0;
        self.right_sidebar_file_preview_horizontal_offset = 0;
        self.right_sidebar_file_view = RightSidebarFileView::Preview;
        self.prefetch_right_sidebar_file_open_with(&path);

        let Some(window) = self.window.as_ref().cloned() else {
            self.right_sidebar_file_preview_message =
                Some(crate::i18n::tr("right-window-unavailable"));
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
                message: Some(right_sidebar_arg(
                    "right-preview-error",
                    "error",
                    err.to_string(),
                )),
                truncated: false,
                raw_text: None,
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
            // `close_right_sidebar_file_preview` reflows for us.
            self.close_right_sidebar_file_preview();
        }
        self.force_right_sidebar_file_rescan();
    }

    /// Close the preview pane and give its width back to the terminal.
    ///
    /// The reflow lives here rather than at the call sites: closing narrows the
    /// sidebar, and without `apply_dimensions` the terminal keeps rendering at
    /// its old size so the reclaimed strip just sits empty. Of the nine callers
    /// only two remembered to do it, and the ones that forgot were exactly the
    /// remote paths — hence the pane's width surviving a workspace switch.
    pub(crate) fn close_right_sidebar_file_preview(&mut self) {
        // RemoteFilesState::TargetChanged clears its selection before the
        // ReleaseLease effect reaches this method. In that interval
        // right_sidebar_width() already reports the narrow tree-only width,
        // even though the terminal is still laid out for the Preview view.
        // Preserve the view marker so closing still forces a reflow.
        let preview_was_visible = !self.right_sidebar_collapsed
            && self.right_sidebar_mode == RightSidebarMode::Chat
            && self.right_sidebar_file_view == RightSidebarFileView::Preview;
        let previous_width = self.right_sidebar_width();
        self.close_right_sidebar_file_preview_without_reflow();
        if file_preview_close_requires_reflow(
            preview_was_visible,
            previous_width,
            self.right_sidebar_width(),
        ) {
            self.schedule_right_sidebar_reflow();
        }
    }

    fn close_right_sidebar_file_preview_without_reflow(&mut self) {
        self.right_sidebar_file_view = RightSidebarFileView::Tree;
        self.right_sidebar_file_selected = None;
        // The remote panel has its own selection; leaving it set keeps
        // `right_sidebar_file_preview_active` true and the pane wide.
        self.right_sidebar_remote_files.clear_selection();
        self.right_sidebar_file_preview_generation =
            self.right_sidebar_file_preview_generation.wrapping_add(1);
        self.right_sidebar_file_preview_highlight_cancel
            .store(1, AtomicOrdering::Relaxed);
        self.right_sidebar_file_preview_highlight_cancel = Arc::new(AtomicUsize::new(0));
        self.right_sidebar_file_preview_lines.clear();
        self.set_right_sidebar_file_preview_raw_text(None);
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
                let ticket = self.note_surface_ticket();
                promise::spawn::spawn(async move {
                    smol::Timer::after(Duration::from_millis(NOTE_CODE_HIGHLIGHT_DEBOUNCE_MS))
                        .await;
                    window.notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                        term_window.with_note_surface_ticket(ticket, move |term_window| {
                            term_window.start_note_code_highlight(
                                code.source.start,
                                key,
                                generation,
                                code,
                            );
                        });
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
        let ticket = self.note_surface_ticket();
        syntax_highlight_pool().spawn(move || {
            let stage = crate::input_diagnostics::StageTimer::begin("note_highlight");
            let result = note_code_highlight_pair(&code, Some(cancellation.as_ref()));
            stage.finish(result.is_some());
            window.notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                term_window.with_note_surface_ticket(ticket, move |term_window| {
                    term_window.apply_note_code_highlight_result(block_start, key, source, result);
                });
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
        let ticket = self.note_surface_ticket();
        let surface = ticket.surface;
        promise::spawn::spawn(async move {
            smol::Timer::after(Duration::from_millis(NOTE_CODE_HIGHLIGHT_REPAINT_MS)).await;
            window.notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                let visible = term_window
                    .with_note_surface_ticket(ticket, |term_window| {
                        term_window
                            .right_sidebar_note_code_highlight
                            .repaint_scheduled = false;
                        surface != NoteSurface::Note || term_window.right_sidebar_note_visible()
                    })
                    .unwrap_or(false);
                if visible {
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
        self.set_right_sidebar_file_preview_raw_text(result.raw_text);
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
        let items = self.right_sidebar_open_with_menu_items(&key, &path, false);
        self.show_term_context_menu(context, anchor, items);
    }

    /// Right-clicking a local file's preview: what the text offers first, as
    /// the remote preview's menu does, then the file, with every Open With
    /// app in a submenu.
    pub(crate) fn show_right_sidebar_file_preview_context_menu(
        &mut self,
        context: &dyn WindowOps,
        anchor: window::Point,
    ) {
        let Some(path) = self.right_sidebar_file_selected.clone() else {
            return;
        };
        let key = right_sidebar_open_with_cache_key(&path);
        self.start_right_sidebar_open_with_load_if_needed(&key, &path);
        let has_selection = self.right_sidebar_file_preview_selected_text().is_some();
        let has_text = self.right_sidebar_file_preview_image.is_none()
            && !self.right_sidebar_file_preview_lines.is_empty();
        let path_string = path.to_string_lossy().to_string();
        let open_with = self.right_sidebar_open_with_menu_items(&key, &path, true);

        self.begin_context_menu_application_actions();
        let copy_selection = self.context_menu_application_item_with_icon(
            crate::i18n::tr("menu-copy"),
            ContextMenuIcon::Copy,
            crate::termwindow::ContextMenuApplicationAction::CopyRemotePreviewSelection,
            has_selection,
        );
        let copy_all = self.context_menu_application_item_with_icon(
            crate::i18n::tr("right-copy-all"),
            ContextMenuIcon::Copy,
            crate::termwindow::ContextMenuApplicationAction::CopyRemotePreviewAll,
            has_text,
        );
        let items = vec![
            copy_selection,
            copy_all,
            ContextMenuItem::item_with_icon(
                crate::i18n::tr("right-copy-path"),
                ContextMenuIcon::Copy,
                KeyAssignment::CopyFilePathToClipboard(path_string.clone()),
            ),
            ContextMenuItem::Separator,
            ContextMenuItem::item_with_icon(
                super::context_menu::reveal_in_folder_label(),
                ContextMenuIcon::Folder,
                KeyAssignment::RevealFileInFolder(path_string),
            ),
            ContextMenuItem::submenu_with_icon(
                crate::i18n::tr("right-open-with-menu"),
                ContextMenuIcon::Application,
                open_with,
            ),
        ];
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
                crate::i18n::tr("right-open"),
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
            crate::i18n::tr("right-copy-path"),
            ContextMenuIcon::Copy,
            KeyAssignment::CopyFilePathToClipboard(path_string.clone()),
        ));
        items.push(ContextMenuItem::Separator);
        items.push(ContextMenuItem::item_with_icon(
            crate::i18n::tr("right-rename"),
            ContextMenuIcon::Edit,
            KeyAssignment::RenameSidebarFile(path_string.clone()),
        ));
        items.push(ContextMenuItem::item_with_icon(
            crate::i18n::tr("right-move-trash"),
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

    /// The Open With apps for `path`. The toolbar's list leaves out the app
    /// its button already opens with; a submenu (`in_submenu`) lists every app
    /// by its bare name, under a parent that says "Open With".
    fn right_sidebar_open_with_menu_items(
        &self,
        key: &str,
        path: &Path,
        in_submenu: bool,
    ) -> Vec<ContextMenuItem> {
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
                .filter(|candidate| {
                    in_submenu || current_id.as_deref() != Some(candidate.id.as_str())
                })
                .map(|candidate| {
                    ContextMenuItem::item_with_icon(
                        if in_submenu {
                            candidate.label.clone()
                        } else {
                            right_sidebar_arg("right-open-with", "app", candidate.label.clone())
                        },
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
            crate::i18n::tr("right-open-with-other"),
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

    /// Copy the whole preview, ignoring any selection. The toolbar button and
    /// the context menu's "Copy" fall back to this when nothing is selected;
    /// the menu also offers it outright, so it is separated out.
    pub(crate) fn copy_right_sidebar_file_preview_all_text(&mut self) {
        if self.right_sidebar_file_preview_lines.is_empty() {
            return;
        }
        // Whole-buffer copy must reproduce the file, not the display: the
        // lines have tabs expanded and controls stripped for rendering.
        if let Some(raw) = self.right_sidebar_file_preview_raw_text.clone() {
            self.copy_to_clipboard(ClipboardCopyDestination::Clipboard, raw);
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

    pub(crate) fn copy_right_sidebar_selected_file_preview_text(&mut self) {
        if let Some(text) = self.right_sidebar_file_preview_selected_text() {
            self.copy_to_clipboard(ClipboardCopyDestination::Clipboard, text);
            return;
        }
        // Nothing selected means "copy what I am looking at".
        self.copy_right_sidebar_file_preview_all_text();
    }

    pub(crate) fn right_sidebar_file_preview_selected_text(&self) -> Option<String> {
        if !self.right_sidebar_file_preview_active() {
            return None;
        }
        if self.right_sidebar_markdown_preview_rendering() {
            // The Markdown source of what is selected, as the Note copies.
            let host = &self.right_sidebar_markdown_preview.as_ref()?.state.host;
            let range = host.view.selection.range();
            if range.is_empty() {
                return None;
            }
            let session = host.session.as_ref()?;
            return session.lock().source().get(range).map(str::to_string);
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

    /// Point the Files panel at `root`, restoring that root's remembered view.
    ///
    /// Deliberately does *not* build a search index: the tree is served lazily
    /// per directory, and the whole-project walk only happens if the user
    /// actually searches. The old version keyed its "already handled" check off
    /// the index status, which no longer works — a user who never searches sits
    /// at `Empty` forever, and this runs on every paint.
    fn track_right_sidebar_file_root(&mut self, root: &RightSidebarFileRoot) {
        let same_root = self
            .right_sidebar_file_index_root
            .as_ref()
            .is_some_and(|path| path == &root.path)
            && self.right_sidebar_file_index_project_name == root.project_name;
        if same_root && !self.right_sidebar_file_view_needs_restore {
            return;
        }

        let new_key = (root.path.clone(), root.project_name.clone());
        self.right_sidebar_file_view_needs_restore = false;

        if !same_root {
            let previous_width = self.right_sidebar_width();
            // Remember the outgoing root's view, then restore the incoming one
            // (replaces the old blanket clear of expanded + preview).
            self.save_right_sidebar_file_view_state();
            self.close_right_sidebar_file_preview();
            self.right_sidebar_file_browse_rows.clear();
            self.right_sidebar_file_browse_cache_key = None;
            // Must precede the restore below: the reset bumps the generation, so
            // reads spawned by the restore are tagged for the incoming root and
            // any read still in flight for the outgoing one is discarded.
            self.reset_right_sidebar_file_dir_cache();
            // The previous root's search index describes a project we are no
            // longer showing; drop it rather than search the wrong tree.
            self.discard_right_sidebar_file_index();
            self.right_sidebar_file_index_root = Some(root.path.clone());
            self.right_sidebar_file_index_project_name = root.project_name.clone();
            self.restore_right_sidebar_file_view_state(&new_key);
            if self.right_sidebar_width() != previous_width {
                self.schedule_right_sidebar_reflow();
            }
        } else {
            // Same root, coming back from an idle release: restore the view the
            // release tore down (in particular, reload the preview).
            self.restore_right_sidebar_file_view_state(&new_key);
        }
    }

    /// Drop the search index and any search in flight. Used when the root
    /// changes; the browse tree is untouched and keeps rendering.
    fn discard_right_sidebar_file_index(&mut self) {
        if let Some(cancel) = self.right_sidebar_file_index_cancel.take() {
            cancel.store(true, AtomicOrdering::Relaxed);
        }
        self.right_sidebar_file_index_generation =
            self.right_sidebar_file_index_generation.wrapping_add(1);
        self.right_sidebar_file_index = None;
        self.right_sidebar_file_index_status = RightSidebarFileIndexStatus::Empty;
        self.right_sidebar_file_refreshing = false;
        self.clear_right_sidebar_file_search();
    }

    /// Build the whole-project search index unless it already exists or is on
    /// its way.
    ///
    /// Called when the filter box takes focus — the same moment VS Code primes
    /// its file-search cache — so the walk overlaps with the user typing their
    /// first query instead of delaying the panel opening. A session that never
    /// touches the filter box never walks the project at all.
    pub(crate) fn prime_right_sidebar_file_index(&mut self) {
        if matches!(self.active_remote_project_for_files(), Ok(Some(_))) {
            return;
        }
        if matches!(
            self.right_sidebar_file_index_status,
            RightSidebarFileIndexStatus::Indexing
                | RightSidebarFileIndexStatus::Ready
                // A failed build must not be retried from here: this runs once
                // per paint while a query is showing, so retrying would spawn a
                // walk every frame. The re-scan cycle clears `Failed` back to
                // `Empty`, which paces retries at the re-scan interval.
                | RightSidebarFileIndexStatus::Failed(_)
        ) {
            return;
        }
        let Ok(root) = self.active_local_project_for_files() else {
            return;
        };
        self.spawn_right_sidebar_file_index_build(root.path, root.project_name, false, false);
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
        self.reset_right_sidebar_file_dir_cache();
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
        self.track_right_sidebar_file_root(&root);
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
        let respect_gitignore = self.config.right_sidebar_search_respects_gitignore;
        promise::spawn::spawn(async move {
            let result = promise::spawn::spawn_into_new_thread(move || {
                Ok(if fresh {
                    build_fresh_shared_file_index(
                        &index_root_path,
                        &index_project_name,
                        &worker_cancel,
                        respect_gitignore,
                    )
                } else {
                    build_or_reuse_shared_file_index(
                        &index_root_path,
                        &index_project_name,
                        &worker_cancel,
                        respect_gitignore,
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

    /// Drop every lazily-read directory and invalidate loads still in flight.
    ///
    /// The generation bump is what makes in-flight reads safe to ignore: a read
    /// started for the previous root lands with a stale generation and is
    /// discarded instead of poisoning the new tree.
    fn reset_right_sidebar_file_dir_cache(&mut self) {
        self.right_sidebar_file_dir_cache.clear();
        self.right_sidebar_file_dir_loads_in_flight.clear();
        self.right_sidebar_file_dir_cache_generation =
            self.right_sidebar_file_dir_cache_generation.wrapping_add(1);
        self.right_sidebar_file_browse_cache_key = None;
    }

    /// Read `dirs` into the browse cache on a worker thread.
    ///
    /// The tree only ever needs the directories it is actually showing, so this
    /// replaces the old walk-the-whole-project index for browsing: opening the
    /// panel reads the root, and expanding a folder reads exactly that folder.
    /// Reads are off the UI thread so a stalled network mount cannot freeze the
    /// window, and the in-flight set keeps a folder that stays expanded across
    /// frames from being re-read on every paint.
    fn spawn_right_sidebar_dir_reads(&mut self, dirs: Vec<PathBuf>) {
        let dirs: Vec<PathBuf> = dirs
            .into_iter()
            .filter(|dir| {
                self.right_sidebar_file_dir_loads_in_flight
                    .insert(dir.clone())
            })
            .collect();
        if dirs.is_empty() {
            return;
        }
        let Some(window) = self.window.as_ref().cloned() else {
            for dir in &dirs {
                self.right_sidebar_file_dir_loads_in_flight.remove(dir);
            }
            return;
        };
        let generation = self.right_sidebar_file_dir_cache_generation;

        // Kept so `apply` can clear the in-flight marks even if the worker dies;
        // otherwise those directories would never be retried.
        let requested = dirs.clone();
        promise::spawn::spawn(async move {
            let loaded = promise::spawn::spawn_into_new_thread(move || {
                Ok(dirs
                    .into_iter()
                    .map(|dir| {
                        let children = read_right_sidebar_dir(&dir);
                        (dir, children)
                    })
                    .collect::<Vec<_>>())
            })
            .await
            .unwrap_or_else(|err| {
                log::warn!("Unable to read sidebar directories: {err:#}");
                Vec::new()
            });
            window.notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                term_window.apply_right_sidebar_dir_load_result(generation, requested, loaded);
            })));
        })
        .detach();
    }

    fn apply_right_sidebar_dir_load_result(
        &mut self,
        generation: u64,
        requested: Vec<PathBuf>,
        loaded: Vec<(
            PathBuf,
            Result<Vec<RightSidebarFileDirEntry>, FolderProblem>,
        )>,
    ) {
        if generation != self.right_sidebar_file_dir_cache_generation {
            // Root changed while we were reading; the in-flight set was already
            // cleared by the reset, so there is nothing to release here.
            return;
        }
        for dir in &requested {
            self.right_sidebar_file_dir_loads_in_flight.remove(dir);
        }
        // Only bump the generation for directories that actually changed: the
        // periodic re-scan re-reads everything on screen, and an unconditional
        // insert would rebuild rows and repaint every tick for nothing.
        let mut changed = false;
        for (dir, result) in loaded {
            if !dir_load_changes_cache(&self.right_sidebar_file_dir_cache, &dir, &result) {
                continue;
            }
            match result {
                Ok(children) => self.right_sidebar_file_dir_cache.insert(dir, children),
                Err(problem) => self
                    .right_sidebar_file_dir_cache
                    .insert_failure(dir, problem),
            }
            changed = true;
        }
        if changed {
            self.invalidate_window();
        }
    }

    /// Re-read what is on screen, and refresh the search index only if one
    /// already exists. Used by the periodic timer, window/panel focus, the
    /// manual Refresh button, and after we ourselves rename/delete/copy a file.
    ///
    /// Two rules matter here now that browsing and search no longer share a
    /// scan:
    ///
    /// * The tree re-read must not depend on the index status. The index is
    ///   built lazily on first search, so a user who never searches sits at
    ///   `Empty` forever — gating on `Ready` (as this used to) would mean their
    ///   tree never picked up a rename, a delete, or an external change again.
    /// * A re-scan may refresh an existing index but must never create one.
    ///   Building here would quietly restore the eager whole-project walk that
    ///   the lazy tree exists to avoid, and on the 90-second timer at that.
    pub(crate) fn force_right_sidebar_file_rescan(&mut self) {
        if matches!(self.active_remote_project_for_files(), Ok(Some(_))) {
            return;
        }
        if self
            .sync_right_sidebar_file_root_for_current_workspace()
            .is_err()
        {
            return;
        }

        let plan = rescan_plan(
            self.right_sidebar_file_view_active(),
            matches!(
                self.right_sidebar_file_index_status,
                RightSidebarFileIndexStatus::Ready
            ),
            self.right_sidebar_file_refreshing,
        );

        if plan.reread_loaded_dirs {
            // `spawn` skips directories with a read already in flight, so this
            // is safe to call as often as the timer fires.
            let loaded = self.right_sidebar_file_dir_cache.loaded_dirs();
            self.spawn_right_sidebar_dir_reads(loaded);

            // Clear a failed index so the next query can try again. Priming
            // deliberately refuses to retry a failure itself, so this is what
            // paces retries at the re-scan interval instead of per frame.
            if matches!(
                self.right_sidebar_file_index_status,
                RightSidebarFileIndexStatus::Failed(_)
            ) {
                self.right_sidebar_file_index_status = RightSidebarFileIndexStatus::Empty;
            }
        }

        if plan.refresh_search_index {
            if let Some((root, project)) = self.right_sidebar_file_view_state_key() {
                self.spawn_right_sidebar_file_index_build(root, project, true, true);
            }
        }
    }

    /// Re-scan after we ourselves changed the tree.
    ///
    /// [`Self::force_right_sidebar_file_rescan`] can still decline to refresh
    /// the search index (one is already in flight) — fine for a timer, wrong
    /// here: re-arm the periodic cycle so a refresh happens either way.
    pub(crate) fn force_right_sidebar_file_rescan_soon(&mut self) {
        self.force_right_sidebar_file_rescan();
        self.schedule_right_sidebar_file_rescan();
    }

    /// Re-read the visible tree and (re)start the 90s periodic re-scan cycle.
    /// Called on window focus and when entering the file view.
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
        // Unconditional: `force` decides for itself what needs refreshing, and
        // the tree half must run even when no search index has ever been built.
        self.force_right_sidebar_file_rescan();
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
            // No index yet — this is the choke point that covers every way a
            // query can appear (focus-primed, restored from saved view state,
            // typed while a previous build was discarded). Kick the build; the
            // panel shows "Indexing files…" and this runs again once it lands.
            self.prime_right_sidebar_file_index();
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

    /// Puts a snippet into the active pane: the plugin host says what to
    /// send, pasted or, with `run`, typed and run. Into the pane active at
    /// the press, never whichever is active when the answer comes, and not
    /// at all once that pane is gone or the answer comes too late to still
    /// be what was meant.
    pub(crate) fn paste_snippet_to_active_pane(&mut self, id: &str, run: bool) {
        let (Some(window), Some(pane)) = (self.window.clone(), self.get_active_pane_or_overlay())
        else {
            return;
        };
        let pressed = Instant::now();
        let id = id.to_string();
        crate::snippets::text(&id.clone(), run, window, move |_term_window, text| {
            if pane.is_dead() {
                return;
            }
            if pressed.elapsed() > SNIPPET_SEND_WAIT {
                log::warn!("not sending snippet {id}: its text came too late");
                return;
            }
            if run {
                if let Err(err) = pane.writer().write_all(text.as_bytes()) {
                    log::error!("failed to run snippet {id}: {err:#}");
                }
            } else if let Err(err) = pane.send_paste(&text) {
                log::error!("failed to paste snippet {id}: {err:#}");
            }
        });
    }

    pub(crate) fn copy_right_sidebar_focused_input(
        &mut self,
        destination: ClipboardCopyDestination,
    ) {
        if let RightSidebarMode::Plugin(_) = self.right_sidebar_mode {
            return self.plugin_panel_copy(destination);
        }
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
                title: crate::i18n::tr("right-create-vault"),
                prompt: crate::i18n::tr("right-create"),
                ..Default::default()
            }
        } else {
            FolderPickerOptions {
                title: crate::i18n::tr("right-choose-vault"),
                prompt: crate::i18n::tr("right-choose"),
                ..Default::default()
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
                    term_window.clear_right_sidebar_note_failures();
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
        self.clear_right_sidebar_note_failures();
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
        self.clear_right_sidebar_note_failures();
        self.request_right_sidebar_note_open(
            vault.root,
            relative_path.to_string(),
            false,
            project_id,
            false,
        );
    }

    /// The first component of a vault-relative note path this system cannot
    /// store as a file name, if any. The path arrives `/`-joined from
    /// [`workspace_threads::normalize_vault_markdown_path`], which has
    /// already refused anything that could leave the vault.
    fn unstorable_note_component(relative: &str) -> Option<&str> {
        let rules = crate::termwindow::remote_walk::DownloadNameRules::host();
        relative.split('/').find(|part| !rules.accepts(part))
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
        // A wiki link names the note it opens, so a name this system cannot
        // store is refused rather than rewritten the way a download is:
        // creating `Q3_ Planning.md` for `[[Q3: Planning]]` would leave the
        // link pointing at a note that does not exist. `normalize_vault_
        // markdown_path` above only guards traversal, and is shared with
        // every other note path, so the character rules belong here where
        // they can be asked of this host alone -- `:` and the rest are
        // ordinary characters in a note name on unix.
        if let Some(part) = Self::unstorable_note_component(&relative) {
            self.right_sidebar_note.load_error = Some(format!(
                "cannot create a note named {part:?}: this system does not allow that file name"
            ));
            return;
        }
        let Some(project_id) =
            workspace_threads::active_project_id_for_space(&self.active_space_id)
        else {
            return;
        };
        self.right_sidebar_note_vault_last_scan = None;
        self.clear_right_sidebar_note_failures();
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

    /// The wiki link under a point of the rendered Markdown preview: what it
    /// resolved to and every file it could mean.
    pub(crate) fn right_sidebar_markdown_preview_wiki_link_at(
        &mut self,
        x: f32,
        y: f32,
    ) -> Option<(Option<String>, Vec<String>)> {
        self.with_note_surface(NoteSurface::FilePreview, |term_window| {
            let host = &term_window.right_sidebar_note;
            let range = host.atomic_source_for_point(x, y)?;
            host.projection
                .objects
                .iter()
                .find_map(|object| match object {
                    crate::markdown_editor::ProjectedObject::WikiLink {
                        source,
                        resolved_path,
                        ambiguous_paths,
                        ..
                    } if *source == range => Some((resolved_path.clone(), ambiguous_paths.clone())),
                    _ => None,
                })
        })
        .flatten()
    }

    /// The file a preview's wiki link resolved to, from its project-relative
    /// path: under the local project root, or under the remote one for a
    /// remote file. Either way it cannot leave the root.
    fn right_sidebar_markdown_preview_link_action(
        &self,
        relative: &str,
    ) -> Option<crate::termwindow::ContextMenuApplicationAction> {
        if self.right_sidebar_remote_files.selected.is_some() {
            let root = self.right_sidebar_remote_files.root.as_ref()?;
            return remote_path_under(root, relative)
                .map(crate::termwindow::ContextMenuApplicationAction::OpenRemoteFilePreview);
        }
        let preview = self.right_sidebar_markdown_preview.as_ref()?;
        let raw_root = &preview.state.host.document.as_ref()?.vault_root;
        let root = raw_root.canonicalize().ok()?;
        let path = root.join(relative).canonicalize().ok()?;
        // Checked in canonical form, opened in the root's own spelling, so the
        // file keeps its place in the project (its tree row, its links).
        let inside = path.strip_prefix(&root).ok()?;
        Some(
            crate::termwindow::ContextMenuApplicationAction::OpenFilePreview(raw_root.join(inside)),
        )
    }

    /// Follow a rendered preview's wiki link: open the file it means in the
    /// preview, or offer the files it could mean. An unresolved link (or one
    /// in a remote file) does nothing -- a preview never creates notes.
    pub(crate) fn follow_right_sidebar_markdown_preview_wiki_link(
        &mut self,
        context: &dyn WindowOps,
        anchor: window::Point,
        resolved: Option<String>,
        ambiguous: Vec<String>,
    ) {
        if ambiguous.len() > 1 {
            self.begin_context_menu_application_actions();
            let items = ambiguous
                .into_iter()
                .filter_map(|relative| {
                    let action = self.right_sidebar_markdown_preview_link_action(&relative)?;
                    Some(self.context_menu_application_item_with_icon(
                        relative,
                        ContextMenuIcon::File,
                        action,
                        true,
                    ))
                })
                .collect();
            self.show_term_context_menu(context, anchor, items);
            return;
        }
        match resolved
            .as_deref()
            .and_then(|relative| self.right_sidebar_markdown_preview_link_action(relative))
        {
            Some(crate::termwindow::ContextMenuApplicationAction::OpenFilePreview(path)) => {
                self.open_right_sidebar_file_path(path);
            }
            Some(crate::termwindow::ContextMenuApplicationAction::OpenRemoteFilePreview(path)) => {
                self.open_right_sidebar_remote_file_preview(path);
            }
            _ => {}
        }
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
            crate::i18n::tr("right-source-mode"),
            ContextMenuIcon::Code,
            crate::termwindow::ContextMenuApplicationAction::Note(
                NoteEditorCommand::ToggleSourceMode,
            ),
            true,
        );
        source_item = source_item.checked(source_mode);
        let vault_tree_label = if self.right_sidebar_note_wide_layout {
            if self.right_sidebar_note_vault_tree_collapsed {
                crate::i18n::tr("right-show-vault-sidebar")
            } else {
                crate::i18n::tr("right-hide-vault-sidebar")
            }
        } else if self.right_sidebar_note_view == RightSidebarNoteView::Tree {
            crate::i18n::tr("right-hide-vault")
        } else {
            crate::i18n::tr("right-show-vault")
        };
        let items = vec![
            self.context_menu_application_item_with_icon(
                crate::i18n::tr("right-new-note"),
                ContextMenuIcon::Note,
                crate::termwindow::ContextMenuApplicationAction::Note(NoteEditorCommand::NewNote),
                true,
            ),
            self.context_menu_application_item_with_icon(
                crate::i18n::tr("right-save"),
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
                crate::i18n::tr("right-choose-another-vault"),
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
        if let RightSidebarMode::Plugin(_) = self.right_sidebar_mode {
            return self.plugin_panel_clear_selection();
        }
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
        if self.plugin_panel_keys_held {
            return self.plugin_panel_held_key(key, mods);
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
        if let RightSidebarMode::Plugin(_) = self.right_sidebar_mode {
            return self.plugin_panel_key(key, mods);
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
            RightSidebarMode::Tasks | RightSidebarMode::Agents | RightSidebarMode::Plugin(_) => {}
        }
    }

    pub(crate) fn push_right_sidebar_text(&mut self, text: &str) -> bool {
        if self.plugin_panel_keys_held {
            return true;
        }
        if let RightSidebarMode::Plugin(_) = self.right_sidebar_mode {
            return self.plugin_panel_text(text);
        }
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

    /// Run `f` with `surface`'s state in the `right_sidebar_note*` fields,
    /// then put the Note's back. The painter and the note helpers only ever
    /// read those fields, so this is how they draw or update a surface other
    /// than the Note without a line of them changing. Background work started
    /// inside records `note_surface_installed` and comes back through here,
    /// so a result can only land on the surface that asked for it; `None`
    /// when that surface is gone by then.
    pub(crate) fn with_note_surface<R>(
        &mut self,
        surface: NoteSurface,
        f: impl FnOnce(&mut Self) -> R,
    ) -> Option<R> {
        if surface == self.note_surface_installed {
            return Some(f(self));
        }
        // Only ever entered from the Note's state: a nested switch would put
        // one surface's state into another's slot when unwinding.
        if self.note_surface_installed != NoteSurface::Note {
            log::error!(
                "note surface {surface:?} requested inside {:?}",
                self.note_surface_installed
            );
            return None;
        }
        let mut preview = match surface {
            NoteSurface::Note => unreachable!("handled above"),
            NoteSurface::FilePreview => self.right_sidebar_markdown_preview.take()?,
        };
        self.swap_note_surface_state(&mut preview.state);
        self.note_surface_installed = surface;
        self.note_surface_instance = preview.instance;
        let result = f(self);
        self.note_surface_installed = NoteSurface::Note;
        self.note_surface_instance = 0;
        self.swap_note_surface_state(&mut preview.state);
        // `f` sees no preview while it runs, so it cannot have replaced it.
        self.right_sidebar_markdown_preview = Some(preview);
        Some(result)
    }

    /// The surface installed right now, instance included, for background
    /// work to come back to through `with_note_surface_ticket`.
    pub(crate) fn note_surface_ticket(&self) -> NoteSurfaceTicket {
        NoteSurfaceTicket {
            surface: self.note_surface_installed,
            instance: self.note_surface_instance,
        }
    }

    /// `with_note_surface` for the exact surface `ticket` names: `None` when
    /// that surface is gone, even if another of its kind has replaced it.
    pub(crate) fn with_note_surface_ticket<R>(
        &mut self,
        ticket: NoteSurfaceTicket,
        f: impl FnOnce(&mut Self) -> R,
    ) -> Option<R> {
        let current = match ticket.surface {
            NoteSurface::Note => 0,
            NoteSurface::FilePreview if self.note_surface_installed == NoteSurface::FilePreview => {
                self.note_surface_instance
            }
            NoteSurface::FilePreview => self.right_sidebar_markdown_preview.as_ref()?.instance,
        };
        if current != ticket.instance {
            return None;
        }
        self.with_note_surface(ticket.surface, f)
    }

    fn swap_note_surface_state(&mut self, state: &mut NoteSurfaceState) {
        std::mem::swap(&mut self.right_sidebar_note, &mut state.host);
        std::mem::swap(
            &mut self.right_sidebar_note_table_horizontal_offsets,
            &mut state.table_horizontal_offsets,
        );
        std::mem::swap(
            &mut self.right_sidebar_note_table_layouts,
            &mut state.table_layouts,
        );
        std::mem::swap(
            &mut self.right_sidebar_note_code_highlight,
            &mut state.code_highlight,
        );
        std::mem::swap(
            &mut self.right_sidebar_note_paint_cache,
            &mut state.paint_cache,
        );
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
        let ticket = self.note_surface_ticket();
        let surface = ticket.surface;
        // A preview's wiki links resolve among its project's files; nothing is
        // listed for a file without any.
        let link_files = match &document {
            Some((root, _)) if surface == NoteSurface::FilePreview && source.contains("[[") => {
                self.preview_link_files(root)
            }
            _ => None,
        };
        promise::spawn::spawn(async move {
            let result = promise::spawn::spawn_into_new_thread(move || {
                let stage = crate::input_diagnostics::StageTimer::begin("note_projection");
                let mut projection = crate::markdown_editor::MarkdownProjection::parse(&source);
                if let Some((vault_root, relative_path)) = document {
                    if surface == NoteSurface::Note {
                        projection.resolve_vault_links(&vault_root, &relative_path);
                    } else if projection.has_wiki_links() {
                        if let Some(files) = link_files {
                            resolve_preview_wiki_links(
                                &mut projection,
                                &vault_root,
                                &relative_path,
                                files,
                            );
                        }
                    }
                }
                let visual = build_visual_document(&source, &projection, mode, caret);
                stage.finish(true);
                Ok((projection, visual))
            })
            .await;
            window.notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                term_window.with_note_surface_ticket(ticket, move |term_window| {
                    match result {
                        Ok((projection, visual)) => {
                            let applied = term_window
                                .right_sidebar_note
                                .apply_background_parse(revision, mode, caret, projection, visual);
                            if applied {
                                term_window.schedule_right_sidebar_note_spellcheck(
                                    Duration::from_millis(50),
                                );
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
                });
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
        let ticket = self.note_surface_ticket();
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
                term_window.with_note_surface_ticket(ticket, move |term_window| match result {
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
                });
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

    /// Register (or refresh) the pre-warm target for the currently painted
    /// wrapped document and make sure a warmer step is pending. Called from
    /// the Note paint tail: the painter is the only place that knows both
    /// the live visual and the exact fonts it shapes with.
    fn ensure_right_sidebar_note_prewarm(
        &mut self,
        visual: &Arc<VisualDocument>,
        fonts: NotePrewarmFonts,
    ) {
        // Prewarming serves the Note's typing latency; a preview does not type.
        if self.note_surface_installed != NoteSurface::Note {
            return;
        }
        let same_target = self
            .right_sidebar_note_prewarm
            .as_ref()
            .is_some_and(|state| {
                state.visual.as_ptr() == Arc::as_ptr(visual)
                // A config/appearance change mints new LoadedFonts (new ids,
                // new cache keys); restart so the warm entries match paint.
                && Rc::ptr_eq(&state.fonts.ui.0, &fonts.ui.0)
            });
        if !same_target {
            self.right_sidebar_note_prewarm = Some(NotePrewarmState {
                visual: Arc::downgrade(visual),
                fonts,
                next_line: 0,
                shaped_runs: 0,
                scheduled: false,
            });
        }
        self.schedule_right_sidebar_note_prewarm_step();
    }

    fn schedule_right_sidebar_note_prewarm_step(&mut self) {
        let Some(state) = self.right_sidebar_note_prewarm.as_mut() else {
            return;
        };
        if state.scheduled {
            return;
        }
        let done = state
            .visual
            .upgrade()
            .map_or(true, |visual| state.next_line >= visual.lines.len());
        if done {
            return;
        }
        let Some(window) = self.window.as_ref().cloned() else {
            return;
        };
        state.scheduled = true;
        promise::spawn::spawn(async move {
            // Yield a frame's worth of time so the warmer interleaves with
            // interactive paints instead of competing for the same slice.
            smol::Timer::after(Duration::from_millis(8)).await;
            window.notify(TermWindowNotif::Apply(Box::new(|term_window| {
                term_window.drive_right_sidebar_note_prewarm();
            })));
        })
        .detach();
    }

    pub(crate) fn drive_right_sidebar_note_prewarm(&mut self) {
        let (visual, fonts, mut next_line, mut shaped_runs) = {
            let Some(state) = self.right_sidebar_note_prewarm.as_mut() else {
                return;
            };
            state.scheduled = false;
            let Some(visual) = state.visual.upgrade() else {
                self.right_sidebar_note_prewarm = None;
                return;
            };
            (
                visual,
                state.fonts.clone(),
                state.next_line,
                state.shaped_runs,
            )
        };
        if self.render_state.is_none() {
            // No GPU state to rasterize into yet; the next paint re-arms us.
            return;
        }
        // Warming past the LFU capacity would evict entries that are still
        // hot; stop at 3/4 so chrome strings sharing the domain keep room.
        let cap_budget = self
            .ui_shape_caches
            .borrow()
            .domain(crate::shapecache::UiTextDomain::Note)
            .cap()
            .saturating_mul(3)
            / 4;
        let previous_domain = self
            .ui_text_domain
            .replace(crate::shapecache::UiTextDomain::Note);
        let start = Instant::now();
        const STEP_BUDGET: Duration = Duration::from_millis(4);
        'warm: while next_line < visual.lines.len()
            && shaped_runs < cap_budget
            && start.elapsed() < STEP_BUDGET
        {
            let line = &visual.lines[next_line];
            for run in &line.runs {
                let (font, metrics) = fonts.for_run(line.block, &run.style);
                let (font, metrics) = (Rc::clone(font), *metrics);
                if self.cached_ui_shape(&font, &metrics, &run.text).is_err() {
                    // Shaper errors are cached; a ClearShapeCache unwind is
                    // not — either way this step should stop, the next one
                    // resumes from here.
                    break 'warm;
                }
                shaped_runs += 1;
            }
            next_line += 1;
        }
        self.ui_text_domain.set(previous_domain);
        if let Some(state) = self.right_sidebar_note_prewarm.as_mut() {
            state.next_line = next_line;
            state.shaped_runs = shaped_runs;
        }
        if next_line < visual.lines.len() && shaped_runs < cap_budget {
            self.schedule_right_sidebar_note_prewarm_step();
        }
    }

    fn schedule_right_sidebar_note_spellcheck(&mut self, delay: Duration) {
        // A read-only surface other than the Note is never spell-checked.
        if self.note_surface_installed != NoteSurface::Note {
            return;
        }
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
        // Only the Note takes text input.
        if self.note_surface_installed != NoteSurface::Note {
            return;
        }
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
        self.right_sidebar_presented() && self.right_sidebar_mode == RightSidebarMode::Tasks
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
            UIItemType::RightSidebarFileRow(path) => self.sidebar_file_rename_input(path),
            UIItemType::RightSidebarRemoteFileRow(path) => {
                self.sidebar_remote_file_rename_input(path)
            }
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

    pub(crate) fn right_sidebar_input_for_item_mut(
        &mut self,
        item_type: &UIItemType,
    ) -> Option<&mut TextInputState> {
        match item_type {
            UIItemType::RightSidebarFileFilter => Some(&mut self.right_sidebar_file_filter),
            UIItemType::RightSidebarSnippetSearch => Some(&mut self.right_sidebar_snippet_search),
            UIItemType::RightSidebarSnippetTitle => Some(&mut self.right_sidebar_snippet_title),
            UIItemType::RightSidebarFileRow(path) => {
                let rename = self.inline_tab_rename.as_mut()?;
                matches!(
                    &rename.target,
                    crate::termwindow::InlineTabRenameTarget::File(current) if current == path
                )
                .then_some(&mut rename.input)
            }
            UIItemType::RightSidebarRemoteFileRow(path) => {
                let rename = self.inline_tab_rename.as_mut()?;
                matches!(
                    &rename.target,
                    crate::termwindow::InlineTabRenameTarget::RemoteFile { path: current, .. }
                        if current == path
                )
                .then_some(&mut rename.input)
            }
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
            RightSidebarMode::Tasks | RightSidebarMode::Agents | RightSidebarMode::Plugin(_) => {
                None
            }
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
            RightSidebarMode::Tasks | RightSidebarMode::Agents | RightSidebarMode::Plugin(_) => {
                None
            }
        }
    }

    /// The panel as presented -- a hover reveal included. Terminal geometry
    /// wants `right_sidebar_width` instead.
    pub fn right_sidebar_rect(&self) -> Option<RightSidebarRect> {
        let mut rect = self.right_sidebar_rect_for_width(self.right_sidebar_presented_width())?;
        // Off macOS the panel has no toggle of its own and the tab bar keeps
        // the window buttons at its right end; a hover-revealed panel stays
        // below the tab bar so both remain reachable.
        if !cfg!(target_os = "macos") && self.right_sidebar_collapsed {
            let tab_bar = self.tab_bar_pixel_height().unwrap_or(0.0).ceil() as usize;
            let top = rect.y.saturating_add(tab_bar);
            rect.height = rect.height.saturating_sub(top - rect.y);
            rect.y = top;
            if rect.height == 0 {
                return None;
            }
        }
        Some(rect)
    }

    fn right_sidebar_rect_for_width(&self, width: usize) -> Option<RightSidebarRect> {
        let border = self.get_os_border();
        let width = width.min(
            self.dimensions
                .pixel_width
                .saturating_sub((border.left + border.right).get() as usize),
        );
        let y = border.top.get() as usize;
        let height = self
            .dimensions
            .pixel_height
            .saturating_sub(y + border.bottom.get() as usize);
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

    /// Where a plugin's extended view goes: the left of the sidebar, as the
    /// Note pane does.
    pub(crate) fn right_sidebar_plugin_extended_rect(&self) -> Option<RightSidebarRect> {
        let sidebar = self.right_sidebar_rect()?;
        let width = self.right_sidebar_plugin_extended_width()?;
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

    /// The line between a plugin's extended view and its panel dragged to
    /// `split_x`: the one grows as much as the other shrinks.
    pub(crate) fn set_right_sidebar_plugin_extended_split_x(&mut self, split_x: isize) -> bool {
        let Some(total_rect) = self.right_sidebar_rect() else {
            return false;
        };
        if self.right_sidebar_plugin_extended_rect().is_none() {
            return false;
        }

        let total_left = total_rect.x;
        let total_right = total_rect.x.saturating_add(total_rect.width);
        let min_extended = self.ui_px(PLUGIN_EXTENDED_MIN_WIDTH);
        let max_extended = total_rect
            .width
            .saturating_sub(self.ui_px(RIGHT_SIDEBAR_MIN_WIDTH));
        let min_panel = self.ui_px(RIGHT_SIDEBAR_MIN_WIDTH);
        let max_panel = self.right_sidebar_max_width().min(total_rect.width);
        let min_split = total_left
            .saturating_add(min_extended)
            .max(total_right.saturating_sub(max_panel));
        let max_split = total_right
            .saturating_sub(min_panel)
            .min(total_left.saturating_add(max_extended));
        if min_split > max_split {
            return false;
        }

        let split_x = split_x.clamp(min_split as isize, max_split as isize) as usize;
        let extended_width = split_x.saturating_sub(total_left);
        let panel_width = total_right.saturating_sub(split_x);
        let Some(panel) = self.right_sidebar_plugin.as_mut() else {
            return false;
        };
        let old_extended = std::mem::replace(&mut panel.extended_width, extended_width);
        let old_panel = self.right_sidebar_width;
        self.set_right_sidebar_width(panel_width);
        old_extended != extended_width || old_panel != self.right_sidebar_width
    }

    /// The sidebar's outer edge dragged to make it `width` wide while a
    /// plugin's extended view is on show: the view takes what the panel
    /// does not.
    pub(crate) fn set_right_sidebar_plugin_extended_total_width(&mut self, width: usize) -> bool {
        if self.right_sidebar_plugin_extended_rect().is_none() {
            return false;
        }
        let panel_width = self.right_sidebar_tree_width();
        let min = self.ui_px(PLUGIN_EXTENDED_MIN_WIDTH);
        let Some(max) = self.right_sidebar_plugin_extended_room() else {
            return false;
        };
        let Some(panel) = self.right_sidebar_plugin.as_mut() else {
            return false;
        };
        let extended_width = width.saturating_sub(panel_width).clamp(min, max);
        std::mem::replace(&mut panel.extended_width, extended_width) != extended_width
    }

    /// Keeps how wide the extended view of the plugin on show is, for the
    /// next time it is.
    pub fn persist_right_sidebar_plugin_extended_width(&self) {
        let Some(panel) = self.right_sidebar_plugin.as_ref() else {
            return;
        };
        let width = self
            .right_sidebar_plugin_extended_width()
            .unwrap_or(panel.extended_width)
            .max(self.ui_px(PLUGIN_EXTENDED_MIN_WIDTH));
        let width = unscale_ui_usize(width, self.dimensions.dpi);
        if let Err(err) = crate::native_settings::save_plugin_extended_width(panel.plugin(), width)
        {
            log::warn!("failed to save the width of a plugin's extended view: {err:#}");
        }
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
            max_line_columns.max(crate::i18n::tr("right-preview-truncated").chars().count())
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

    fn right_sidebar_top_bar_layout(&self, rect: RightSidebarRect) -> (usize, usize) {
        let y = rect.y + self.ui_px(SIDEBAR_INSET) * 2;
        // A non-macOS hover panel already sits below the window buttons and
        // has no toggle of its own, so it needs no additional button row.
        let height = if !cfg!(target_os = "macos") && self.right_sidebar_collapsed {
            0
        } else {
            self.ui_px(RIGHT_SIDEBAR_TOP_BAR_HEIGHT).min(
                rect.y
                    .saturating_add(rect.height)
                    .saturating_sub(y + self.ui_px(SIDEBAR_INSET)),
            )
        };
        (y, height)
    }

    fn right_sidebar_snippet_scroll_metrics(&self) -> Option<(usize, usize, usize)> {
        let Some(rect) = self.right_sidebar_rect() else {
            return None;
        };
        if !self.right_sidebar_presented()
            || self.right_sidebar_mode != RightSidebarMode::Snippets
            || self.right_sidebar_snippet_view != RightSidebarSnippetView::List
        {
            return None;
        }

        let (top_bar_y, top_bar_height) = self.right_sidebar_top_bar_layout(rect);
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

    pub(crate) fn right_sidebar_snippet_cursor_on(&self) -> bool {
        let blink_ms = (self.config.cursor_blink_rate as u64).max(100);
        self.update_next_frame_time(Some(Instant::now() + Duration::from_millis(blink_ms)));
        let ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        snippet_cursor_visible(ms, blink_ms)
    }

    fn filtered_snippet_count(&self) -> usize {
        self.right_sidebar_snippet_listing
            .rows()
            .map_or(0, <[_]>::len)
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
        let chrome = self.chrome();
        let foreground = chrome.text;
        let muted_fg = chrome.secondary_text;
        let sidebar_bg = self.chrome_surface(chrome.workspace_sidebar_bg);
        let see_through = self.chrome_see_through();
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
            // rect later in the frame. The band above it is the window
            // border's, which see-through must not get a second coat.
            let pane_top = if see_through { pane_rect.y } else { 0 };
            if pane_rect.y > 0 && !see_through {
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
                    pane_top as f32,
                    1.0,
                    pane_rect
                        .y
                        .saturating_add(pane_rect.height)
                        .saturating_sub(pane_top) as f32,
                ),
                chrome.separator,
            )
            .context("right sidebar note pane left separator")?;
        }

        // A plugin's extended view: its ground and edge here, what the
        // plugin draws in it with the panel's, below.
        if let Some(extended_rect) = self.right_sidebar_plugin_extended_rect() {
            // From the window's top, but for the border's band see-through.
            let ground_top = if see_through { extended_rect.y } else { 0 };
            self.filled_rectangle(
                layers,
                0,
                euclid::rect(
                    extended_rect.x as f32,
                    ground_top as f32,
                    extended_rect.width as f32,
                    extended_rect
                        .y
                        .saturating_add(extended_rect.height)
                        .saturating_sub(ground_top) as f32,
                ),
                sidebar_bg,
            )
            .context("right sidebar plugin extended view background")?;
            self.ui_items.push(UIItem {
                x: extended_rect.x,
                y: 0,
                width: extended_rect.width,
                height: extended_rect.y.saturating_add(extended_rect.height),
                item_type: UIItemType::RightSidebarBackground,
            });
            self.filled_rectangle(
                layers,
                1,
                euclid::rect(
                    extended_rect.x as f32,
                    ground_top as f32,
                    1.0,
                    extended_rect
                        .y
                        .saturating_add(extended_rect.height)
                        .saturating_sub(ground_top) as f32,
                ),
                chrome.separator,
            )
            .context("right sidebar plugin extended view left separator")?;
        }

        // The panel's ground runs from the window's top -- over the tab bar,
        // too, where a hover-revealed one off macOS keeps below it -- and its
        // edge with it; but for the border's band see-through.
        let ground_top = if see_through { rect.y } else { 0 };
        if rect.y > 0 && !see_through {
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
            euclid::rect(
                rect.x as f32,
                ground_top as f32,
                1.0,
                rect.y
                    .saturating_add(rect.height)
                    .saturating_sub(ground_top) as f32,
            ),
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
        if self.right_sidebar_plugin_extended_rect().is_some() {
            self.ui_items.push(UIItem {
                x: rect
                    .x
                    .saturating_sub(self.ui_px(SIDEBAR_RESIZE_HANDLE_WIDTH) / 2),
                y: rect.y,
                width: self.ui_px(SIDEBAR_RESIZE_HANDLE_WIDTH),
                height: rect.height,
                item_type: UIItemType::RightSidebarPluginExtendedResize,
            });
        }

        let content_x = rect.x + self.ui_px(SIDEBAR_INSET) * 2;
        let content_width = rect.width.saturating_sub(self.ui_px(SIDEBAR_INSET) * 4);
        let (top_bar_y, top_bar_height) = self.right_sidebar_top_bar_layout(rect);
        if cfg!(target_os = "macos") && top_bar_height > 0 {
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

        let modes = RightSidebarMode::enabled_panels();
        const MODE_LABEL_CLIP_SLOP: usize = 4;
        let mode_icon_size = (ui_cell_height + self.ui_px(12))
            .clamp(self.ui_px(24), self.ui_px(30))
            .min(mode_height.saturating_sub(self.ui_px(22)))
            .max(1);
        let labeled = modes.len() <= RIGHT_SIDEBAR_LABELED_MODES;
        let active_label_target_width = if labeled {
            let active_mode_label = self.right_sidebar_mode.label();
            self.sidebar_text_width(&ui_font, &active_mode_label)?
                .ceil() as usize
                + self.ui_px(MODE_LABEL_CLIP_SLOP)
        } else {
            0
        };
        let inactive_segment_min_width = (mode_icon_size + self.ui_px(SIDEBAR_INSET) * 4)
            .max(self.ui_px(70))
            .min((content_width / modes.len()).max(1));
        let inactive_segment_count = modes.len().saturating_sub(1);
        let inactive_segments_min_width =
            inactive_segment_min_width.saturating_mul(inactive_segment_count);
        let active_segment_width = if labeled {
            (mode_icon_size
                + self.ui_px(SIDEBAR_ICON_GAP)
                + active_label_target_width
                + self.ui_px(SIDEBAR_INSET) * 6)
                .min(
                    content_width
                        .saturating_sub(inactive_segments_min_width)
                        .max(1),
                )
                .max(1)
        } else {
            // A share like the others'.
            (content_width / modes.len()).max(1)
        };
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

            // The pill the active segment is filled with, and the hover:
            // the segment less an inset, or with icons alone, a circle
            // round the icon.
            let inner_inset = self.ui_px(5);
            let pill = if labeled {
                euclid::rect(
                    (segment_x + inner_inset) as f32,
                    (mode_y + inner_inset) as f32,
                    segment_width.saturating_sub(inner_inset * 2) as f32,
                    mode_height.saturating_sub(inner_inset * 2) as f32,
                )
            } else {
                let diameter = segment_width
                    .min(mode_height)
                    .saturating_sub(inner_inset * 2);
                euclid::rect(
                    (segment_x + segment_width.saturating_sub(diameter) / 2) as f32,
                    (mode_y + mode_height.saturating_sub(diameter) / 2) as f32,
                    diameter as f32,
                    diameter as f32,
                )
            };
            if active {
                self.fill_rounded_rectangle_with_border(
                    layers,
                    2,
                    pill,
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
                    pill,
                    chrome.sidebar_button_hover_bg,
                    WINDOW_TAB_ADD_BUTTON_RADIUS,
                )
                .context("right sidebar hovered mode")?;
            }

            let label_width = if active && labeled {
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
            let total_width = if label_width > 0 {
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
                let mode_label = mode.label();
                self.paint_sidebar_text(
                    layers,
                    &ui_font,
                    ui_metrics,
                    &mode_label,
                    icon_x + mode_icon_size + self.ui_px(SIDEBAR_ICON_GAP),
                    mode_y + (mode_height.saturating_sub(ui_cell_height)) / 2,
                    label_width,
                    foreground,
                )?;
            }
            segment_x = segment_right;
        }

        let content_top = mode_y + mode_height + self.ui_px(RIGHT_SIDEBAR_SECTION_GAP);
        // A panel turned off while it was open would paint a stranded mode:
        // the selector no longer offers it, so nothing could switch away.
        if !self.right_sidebar_mode.panel_enabled() {
            self.fall_back_to_an_enabled_panel();
            return Ok(());
        }
        // A plugin's panel on show only while its mode is: the plugin is
        // told it went, and what it held is let go.
        if !matches!(self.right_sidebar_mode, RightSidebarMode::Plugin(_)) {
            self.close_plugin_panel();
        }
        let panel_bottom = rect.y.saturating_add(rect.height);
        self.paint_right_sidebar_contents(layers, |this, layers| {
            this.paint_right_sidebar_panel(
                layers,
                &ui_font,
                ui_metrics,
                chrome,
                foreground,
                muted_fg,
                rect,
                content_x,
                content_top,
                content_width,
                panel_bottom,
                base_font_size,
                icon_size,
            )
        })
    }

    /// The open mode's panel, under the mode selector.
    #[allow(clippy::too_many_arguments)]
    fn paint_right_sidebar_panel(
        &mut self,
        layers: &mut TripleLayerQuadAllocator,
        ui_font: &Rc<LoadedFont>,
        ui_metrics: RenderMetrics,
        chrome: UiPalette,
        foreground: LinearRgba,
        muted_fg: LinearRgba,
        rect: RightSidebarRect,
        content_x: usize,
        content_top: usize,
        content_width: usize,
        panel_bottom: usize,
        base_font_size: f64,
        icon_size: usize,
    ) -> anyhow::Result<()> {
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
                    panel_bottom,
                    file_icon_size,
                )?;
                return Ok(());
            }
            RightSidebarMode::Snippets => {
                let stage = crate::input_diagnostics::StageTimer::begin("snippet_paint");
                let result = self.paint_snippets_sidebar(
                    layers,
                    ui_font,
                    ui_metrics,
                    chrome,
                    foreground,
                    muted_fg,
                    content_x,
                    content_top,
                    content_width,
                    panel_bottom,
                    icon_size,
                );
                stage.finish(result.is_ok());
                result?;
                return Ok(());
            }
            RightSidebarMode::Tasks => {
                let stage = crate::input_diagnostics::StageTimer::begin("note_paint");
                let result = self.paint_note_sidebar(
                    layers,
                    ui_font,
                    ui_metrics,
                    chrome,
                    foreground,
                    muted_fg,
                    content_x,
                    content_top,
                    content_width,
                    panel_bottom,
                    base_font_size,
                );
                stage.finish(result.is_ok());
                result?;
                return Ok(());
            }
            RightSidebarMode::Agents => {
                if !RightSidebarMode::Agents.panel_enabled() {
                    self.fall_back_to_an_enabled_panel();
                    return Ok(());
                }
                self.paint_agents_sidebar(
                    layers,
                    ui_font,
                    ui_metrics,
                    chrome,
                    foreground,
                    muted_fg,
                    content_x,
                    content_top,
                    content_width,
                    panel_bottom,
                )?;
                return Ok(());
            }
            RightSidebarMode::Plugin(_) => {
                let Some(panel) = self.right_sidebar_mode.plugin_panel() else {
                    self.fall_back_to_an_enabled_panel();
                    return Ok(());
                };
                let stage = crate::input_diagnostics::StageTimer::begin("plugin_panel_paint");
                let result = self.plugin_panel_paint().and_then(|paint| {
                    // The panel's own padding is the plugin's to draw.
                    self.paint_plugin_panel(
                        layers,
                        &panel.id,
                        &paint,
                        ui_font,
                        ui_metrics,
                        chrome,
                        rect.x + self.ui_px(SIDEBAR_INSET),
                        content_top,
                        rect.width.saturating_sub(self.ui_px(SIDEBAR_INSET) * 2),
                        panel_bottom,
                    )?;
                    // Its extended view from the top, as the file preview
                    // is, with the preview's close button where the preview
                    // has it: ThinkTerm's, over what the plugin draws.
                    let Some(extended_rect) = self.right_sidebar_plugin_extended_rect() else {
                        return Ok(());
                    };
                    let close_x = extended_rect.x + self.ui_px(SIDEBAR_INSET) * 2;
                    let close_y = extended_rect.y
                        + self.ui_px(SIDEBAR_INSET) * 2
                        + self.ui_px(PREVIEW_HEADER_TOP_GAP);
                    let close_size = self.ui_px(PREVIEW_HEADER_BUTTON).min(
                        extended_rect
                            .width
                            .saturating_sub(self.ui_px(SIDEBAR_INSET) * 4),
                    );
                    self.paint_plugin_panel_extended(
                        layers,
                        &paint,
                        ui_font,
                        ui_metrics,
                        chrome,
                        extended_rect.x + self.ui_px(SIDEBAR_INSET),
                        extended_rect.y + self.ui_px(SIDEBAR_INSET) * 2,
                        extended_rect
                            .width
                            .saturating_sub(self.ui_px(SIDEBAR_INSET) * 2),
                        extended_rect.y.saturating_add(extended_rect.height),
                        (close_x, close_y, close_size),
                    )?;
                    self.paint_files_preview_header_icon_button(
                        layers,
                        chrome,
                        foreground,
                        muted_fg,
                        close_x,
                        close_y,
                        close_size,
                        SvgIcon::X,
                        UIItemType::RightSidebarPluginExtendedClose,
                    )
                });
                stage.finish(result.is_ok());
                result?;
                return Ok(());
            }
        }
    }

    /// Re-attempt a vault that failed to open.
    ///
    /// Three pieces of state have to go or the retry is a silent no-op: the
    /// rescan throttle (`NOTE_VAULT_RESCAN_SECS` would otherwise swallow it),
    /// the cached open failure (replayed verbatim for the same key), and the
    /// classified failure itself. The button exists for the case where nothing
    /// about ThinkTerm changed and something outside it did -- a permission
    /// granted, a volume mounted -- so the fix must not be "restart the app".
    /// Clear both failure records together.
    ///
    /// They must never diverge: the keyed one gates whether an open is
    /// re-attempted at all, the classified one gates whether the problem page
    /// -- and therefore the way out -- is shown. Leaving either behind gives a
    /// panel that will not retry, or one with no escape hatch.
    fn clear_right_sidebar_note_failures(&mut self) {
        self.right_sidebar_note_open_failure = None;
        self.right_sidebar_note_vault_failure = None;
    }

    pub(crate) fn retry_right_sidebar_note_vault(&mut self) {
        self.right_sidebar_note_vault_last_scan = None;
        self.clear_right_sidebar_note_failures();
        self.right_sidebar_note.load_error = None;
        self.invalidate_window();
    }

    fn refresh_note_vault_index_if_needed(&mut self, root: &Path) {
        let root_changed = self
            .right_sidebar_note_vault_index_root
            .as_ref()
            .is_none_or(|current| current != root);
        if root_changed {
            self.right_sidebar_note_vault_failure = None;
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
                let outcome = (|| -> anyhow::Result<_> {
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
                })();
                // Classified here, on the thread that already has filesystem
                // access: paint may never do IO, and by the time this reaches
                // the window thread the reason is no longer recoverable.
                Ok(outcome.map_err(|err| classify_vault_failure(&worker_root, &err)))
            })
            .await;
            let result = flatten_vault_worker_result(result);
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
                        changed |= term_window
                            .right_sidebar_note_vault_failure
                            .take()
                            .is_some();
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
                    Err(failure) => {
                        changed = term_window.right_sidebar_note.load_error.as_deref()
                            != Some(failure.detail.as_str())
                            || term_window.right_sidebar_note_vault_failure.as_ref()
                                != Some(&failure);
                        term_window.right_sidebar_note.load_error = Some(failure.detail.clone());
                        term_window.right_sidebar_note_vault_failure = Some(failure);
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
            &crate::i18n::tr("right-vault"),
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
        let see_through = self.chrome_see_through();
        self.paint_right_sidebar_contents(layers, |this, layers| {
            let selected = this
                .right_sidebar_note
                .document
                .as_ref()
                .map(|document| document.relative_path.as_str());
            for index in range {
                let row = &rows[index];
                let y = tree_top as f32 + (index * row_metrics.row_height) as f32
                    - this.right_sidebar_note_tree_scroll_offset;
                // Opaque, a row scrolled part-way out stays whole at the top,
                // under the fade. See-through has no fade, so the row keeps its
                // place and what is above the list is cut away after the loop.
                let y = y
                    .floor()
                    .max(if see_through { 0.0 } else { tree_top as f32 })
                    as usize;
                let height = row_metrics.row_height.min(tree_bottom.saturating_sub(y));
                // What is hovered and clicked: the part inside the list.
                let hit_y = y.max(tree_top);
                let hit_height = (y + height).saturating_sub(hit_y);
                if hit_height == 0 {
                    continue;
                }
                let active = !row.is_dir && selected == Some(row.relative_path.as_str());
                let hovered =
                    this.is_pointer_over_ui_rect(content_x, hit_y, content_width, hit_height);
                if active || hovered {
                    this.fill_rounded_rectangle(
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
                        this.ui_f32(SIDEBAR_ROW_RADIUS),
                    )?;
                }
                this.ui_items.push(UIItem {
                    x: content_x,
                    y: hit_y,
                    width: content_width,
                    height: hit_height,
                    item_type: UIItemType::RightSidebarNoteTreeRow(row.relative_path.clone()),
                });
                let indent = row
                    .depth
                    .saturating_mul(row_metrics.indent_step)
                    .min(content_width.saturating_sub(24));
                let icon_x = content_x + this.ui_px(SIDEBAR_INSET) + indent;
                let icon_y = y + height.saturating_sub(row_metrics.icon_size) / 2;
                if row.is_dir {
                    this.paint_sidebar_icon(
                        layers,
                        if this
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
                        this.paint_sidebar_material_icon(
                            layers,
                            icon,
                            icon_x,
                            icon_y,
                            row_metrics.icon_size,
                        )?;
                    } else {
                        this.paint_sidebar_icon(
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
                this.paint_sidebar_text(
                    layers,
                    ui_font,
                    ui_metrics,
                    &row.name,
                    text_x,
                    y + height.saturating_sub(ui_metrics.cell_size.height as usize) / 2,
                    content_x
                        .saturating_add(content_width)
                        .saturating_sub(text_x + this.ui_px(SIDEBAR_INSET)),
                    if active { foreground } else { muted_fg },
                )?;
            }
            if see_through {
                this.paint_right_sidebar_file_mask(
                    layers,
                    chrome,
                    content_x,
                    content_top,
                    content_width,
                    tree_top.saturating_sub(content_top),
                )?;
            }
            Ok(())
        })?;

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
    /// Paint `text` as wrapped lines and report the height used.
    ///
    /// Unlike `paint_sidebar_text` this never ellipsizes. It is for text whose
    /// TAIL is the part that matters -- an error's reason, the end of a path --
    /// which is exactly what a one-line cut removes.
    #[allow(clippy::too_many_arguments)]
    fn paint_sidebar_wrapped_text(
        &mut self,
        layers: &mut TripleLayerQuadAllocator,
        ui_font: &Rc<LoadedFont>,
        ui_metrics: RenderMetrics,
        text: &str,
        x: usize,
        y: usize,
        width: usize,
        max_lines: usize,
        color: LinearRgba,
    ) -> anyhow::Result<usize> {
        if text.is_empty() || width == 0 || max_lines == 0 {
            return Ok(0);
        }
        let line_height = ui_metrics.cell_size.height as usize;
        // Measured against the shared shape cache before any painting starts,
        // so the immutable borrow is gone by the time the lines are drawn.
        let lines = wrap_snippet_text_for_width(text, max_lines, false, |segment| {
            self.sidebar_text_width(ui_font, segment)
                .unwrap_or(f32::MAX)
                / width.max(1) as f32
        });
        let mut line_y = y;
        for line in &lines {
            self.paint_sidebar_text(layers, ui_font, ui_metrics, line, x, line_y, width, color)?;
            line_y += line_height;
        }
        Ok(lines.len().saturating_mul(line_height))
    }

    /// The panel shown when a CONFIGURED vault will not open.
    ///
    /// Modelled on `paint_remote_files_empty_state`, but with the two things
    /// that one cannot do: a detail block that wraps instead of ellipsizing --
    /// the reason and the path are the whole point -- and more than one action.
    #[allow(clippy::too_many_arguments)]
    fn paint_note_vault_problem_state(
        &mut self,
        layers: &mut TripleLayerQuadAllocator,
        ui_font: &Rc<LoadedFont>,
        ui_metrics: RenderMetrics,
        chrome: UiPalette,
        foreground: LinearRgba,
        muted_fg: LinearRgba,
        failure: &NoteVaultFailure,
        vault_root: Option<&Path>,
        content_x: usize,
        content_top: usize,
        content_width: usize,
        content_bottom: usize,
    ) -> anyhow::Result<()> {
        let line_h = ui_metrics.cell_size.height as usize;
        let icon_size = self.ui_px(REMOTE_EMPTY_ICON_SIZE);
        let icon_gap = self.ui_px(REMOTE_EMPTY_ICON_GAP);
        let detail_gap = self.ui_px(REMOTE_EMPTY_DETAIL_GAP);
        let button_block_gap = self.ui_px(REMOTE_EMPTY_BUTTON_GAP);
        // Same metrics as the first-use panel's buttons, which this one stands
        // in for.
        let button_height = self.ui_px(52);
        let button_gap = self.ui_px(10);

        let title = failure.problem.title();
        let hint = failure.problem.hint();
        // Path first, then the refusal: the user has to recognise WHICH folder
        // before any of the three buttons mean anything.
        let mut detail = String::new();
        if let Some(root) = vault_root {
            detail.push_str(&root.display().to_string());
            detail.push('\n');
        }
        detail.push_str(&failure.detail);

        {
            let wrap = |text: &str, max_lines: usize| -> Vec<String> {
                wrap_snippet_text_for_width(text, max_lines, false, |segment| {
                    self.sidebar_text_width(ui_font, segment)
                        .unwrap_or(f32::MAX)
                        / content_width.max(1) as f32
                })
            };
            let title_lines = wrap(&title, 3);
            let hint_lines = hint
                .as_deref()
                .map(|hint| wrap(hint, 4))
                .unwrap_or_default();
            let detail_lines = wrap(&detail, 6);

            let block_h = icon_size
                + icon_gap
                + title_lines.len() * line_h
                + if hint_lines.is_empty() {
                    0
                } else {
                    detail_gap + hint_lines.len() * line_h
                }
                + detail_gap
                + detail_lines.len() * line_h
                + button_block_gap
                + button_height * 3
                + button_gap * 2;

            // Centre in the panel, but never above its top edge when short.
            let available = content_bottom.saturating_sub(content_top);
            let mut y = content_top + available.saturating_sub(block_h) / 2;

            let icon_x = content_x + content_width.saturating_sub(icon_size) / 2;
            self.paint_sidebar_icon(
                layers,
                failure.problem.icon(),
                icon_x,
                y,
                icon_size,
                chrome.secondary_text,
            )?;
            y += icon_size + icon_gap;

            for line in &title_lines {
                self.paint_remote_files_centered_text(
                    layers,
                    ui_font,
                    ui_metrics,
                    line,
                    content_x,
                    y,
                    content_width,
                    chrome.text,
                )?;
                y += line_h;
            }

            for lines in [&hint_lines, &detail_lines] {
                if lines.is_empty() {
                    continue;
                }
                y += detail_gap;
                for line in lines {
                    self.paint_remote_files_centered_text(
                        layers,
                        ui_font,
                        ui_metrics,
                        line,
                        content_x,
                        y,
                        content_width,
                        chrome.muted_text,
                    )?;
                    y += line_h;
                }
            }

            y += button_block_gap;
            // The escape hatches. These used to be gated on there being NO
            // vault at all, which hid "choose another folder" in the one state
            // where it is the only useful thing left to do.
            for (icon, label, item_type) in [
                (
                    SvgIcon::FolderOpen,
                    "right-choose-another-vault",
                    UIItemType::RightSidebarNoteChooseVault,
                ),
                (
                    SvgIcon::FolderPlus,
                    "right-create-new-vault",
                    UIItemType::RightSidebarNoteCreateVault,
                ),
                (
                    SvgIcon::RotateCcw,
                    "right-retry",
                    UIItemType::RightSidebarNoteRetry,
                ),
            ] {
                self.paint_snippet_button(
                    layers,
                    1,
                    ui_font,
                    ui_metrics,
                    chrome,
                    foreground,
                    muted_fg,
                    content_x,
                    y,
                    content_width,
                    button_height,
                    Some(icon),
                    &crate::i18n::tr(label),
                    item_type,
                    true,
                )?;
                y += button_height + button_gap;
            }
        }
        Ok(())
    }

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
        let vault_root = vault.map(|vault| vault.root);
        if let Some(root) = vault_root.as_ref() {
            self.refresh_note_vault_index_if_needed(root);
        }
        if !self.ensure_active_right_sidebar_note_document() {
            // A configured vault that will not open gets a real panel: what is
            // wrong, which folder, and the way out. It used to get one
            // ellipsized line of `anyhow` chain and no buttons at all.
            if let Some(failure) = self.right_sidebar_note_vault_failure.clone() {
                self.paint_note_vault_problem_state(
                    layers,
                    ui_font,
                    ui_metrics,
                    chrome,
                    foreground,
                    muted_fg,
                    &failure,
                    vault_root.as_deref(),
                    content_x,
                    content_top,
                    content_width,
                    content_bottom,
                )?;
                // The expanded pane is reserved even while the note is broken;
                // keep it legible and keep its collapse toggle reachable so it
                // is never blank and stuck.
                if let Some(pane_rect) = self.right_sidebar_note_pane_rect() {
                    let inset = self.ui_px(SIDEBAR_INSET);
                    let button_size = self.ui_px(NOTE_TOOLBAR_HEIGHT);
                    self.paint_sidebar_wrapped_text(
                        layers,
                        ui_font,
                        ui_metrics,
                        &failure.detail,
                        pane_rect.x + inset * 2,
                        pane_rect.y + inset * 2 + button_size + self.ui_px(NOTE_BODY_TOP_GAP),
                        pane_rect.width.saturating_sub(inset * 4),
                        6,
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
            let message = self
                .right_sidebar_note
                .load_error
                .clone()
                .unwrap_or_else(|| crate::i18n::tr("right-notes-open-error"));
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
                    1,
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
                    &crate::i18n::tr("right-choose-existing-vault"),
                    UIItemType::RightSidebarNoteChooseVault,
                    true,
                )?;
                self.paint_snippet_button(
                    layers,
                    1,
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
                    &crate::i18n::tr("right-create-new-vault"),
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

        // The Note has its toolbar; another surface brings its own header.
        let is_note = self.note_surface_installed == NoteSurface::Note;
        let toolbar_height = if is_note {
            self.ui_px(NOTE_TOOLBAR_HEIGHT)
        } else {
            0
        };
        let menu_size = self.ui_px(NOTE_TOOLBAR_HEIGHT);

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
            item_type: if is_note {
                UIItemType::RightSidebarNoteBody
            } else {
                UIItemType::RightSidebarFilePreviewMarkdownBody
            },
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
        let code_label_font = self
            .fonts
            .title_font_with_size(base_font_size * NOTE_CODE_LABEL_SCALE)
            .context("Note code label font")?;
        let code_label_metrics = RenderMetrics::with_font_metrics(&code_label_font.metrics());
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
        // A rendered file preview reads wider than a note: it is someone
        // else's document, often a README laid out for a browser.
        let reading_max_width = if self.note_surface_installed == NoteSurface::FilePreview {
            MARKDOWN_PREVIEW_READING_MAX_WIDTH
        } else {
            self.config.note_reading_max_width.max(320)
        };
        let reading_width = available_reading_width.min(self.ui_f32(reading_max_width as f32));
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
                &crate::i18n::tr("right-laying-out-note"),
                content_x + self.ui_px(NOTE_BODY_PADDING),
                body_y + self.ui_px(NOTE_BODY_PADDING),
                content_width.saturating_sub(self.ui_px(NOTE_BODY_PADDING) * 2),
                muted_fg,
            )?;
            self.right_sidebar_note.line_layouts.clear();
            self.right_sidebar_note.code_block_layouts.clear();
            self.right_sidebar_note.viewport_height = body_height as f32;
            self.right_sidebar_note.content_height = body_height as f32;
            if is_note {
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
        // Diagrams are drawn in the note's look.
        use_dark_syntax_theme.hash(&mut component_hasher);
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
            // Width measurement below covers EVERY code line and table cell,
            // visible or not. Real shaping here cost ~130ms per rebuild on a
            // long document (and the rebuild runs several times while the
            // provisional/parsed/wrapped visuals land), so use the same
            // approximate widths the wrap pass uses; they only feed column
            // sizing and horizontal-scroll bounds, where a few px of error
            // is invisible and the paint-side clamp absorbs it.
            let normal_approx_metrics = self.note_approximate_text_metrics(ui_font, &ui_metrics)?;
            let bold_approx_metrics =
                self.note_approximate_text_metrics(&bold_font, &bold_metrics)?;
            let code_approx_metrics =
                self.note_approximate_text_metrics(&code_font, &code_metrics)?;
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
                    let approx = if row_index == 0 {
                        bold_approx_metrics
                    } else {
                        normal_approx_metrics
                    };
                    for (column_index, cell) in row.iter().enumerate() {
                        let width = approximate_note_text_width(&cell.text, approx)
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
                    max_content_width = max_content_width
                        .max(approximate_note_text_width(line, code_approx_metrics));
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
                let language = code.language.as_deref().unwrap_or("code");
                // Only what is already known: while the block is edited its
                // text changes with every key, and parsing it here would too.
                let problem = crate::markdown_editor::mermaid::is_mermaid(Some(language))
                    .then(|| crate::markdown_editor::mermaid::known_problem(&code.text))
                    .flatten();
                let label = match problem {
                    Some(crate::markdown_editor::mermaid::Verdict::TooLarge) => format!(
                        "{language}  ⚠ {}",
                        crate::i18n::tr("right-notes-mermaid-too-large")
                    ),
                    Some(_) => format!(
                        "{language}  ⚠ {}",
                        crate::i18n::tr("right-notes-mermaid-unreadable")
                    ),
                    None => language.to_string(),
                };
                let code = Arc::new(code.clone());
                for (row_index, visual_index) in row_indices.into_iter().enumerate() {
                    code_rows.insert(
                        visual_index,
                        NoteCodeRowPaintLayout {
                            block_start: code.source.start,
                            label: label.clone(),
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
            let diagram_starts: HashSet<usize> = visual
                .lines
                .iter()
                .filter(|line| line.kind == VisualLineKind::Image)
                .map(|line| line.source.start)
                .collect();
            if let Some(document) = active_document.as_ref() {
                // Embeds resolve to canonical paths; the root may not be one
                // (a preview's project root is spelled as the user opened it).
                let canonical_root = if document.vault_root.as_os_str().is_empty() {
                    None
                } else {
                    document.vault_root.canonicalize().ok()
                };
                for object in &projected_objects {
                    let resolved = match object {
                        ProjectedObject::Image { source, target, .. } => {
                            // A previewed file is not the user's own note: it
                            // never makes the desktop fetch anything.
                            if self.config.note_remote_images_enabled
                                && self.note_surface_installed == NoteSurface::Note
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
                        // Only a block shown as its diagram: one being edited
                        // or shown as code has no picture to find.
                        ProjectedObject::CodeBlock(code)
                            if crate::markdown_editor::mermaid::is_mermaid(
                                code.language.as_deref(),
                            ) && diagram_starts.contains(&code.source.start) =>
                        {
                            let source: Arc<str> = Arc::from(code.text.as_str());
                            Some((
                                code.source.start,
                                RightSidebarNoteImageSource::Mermaid {
                                    hash: crate::markdown_editor::mermaid::content_hash(&source),
                                    source,
                                    style: crate::markdown_editor::mermaid::DiagramStyle {
                                        dark: use_dark_syntax_theme,
                                    },
                                },
                            ))
                        }
                        ProjectedObject::WikiLink {
                            source,
                            embed: true,
                            resolved_path: Some(target),
                            ..
                        } => {
                            // A remote preview has no local root: an empty one
                            // would resolve against the process's own folder.
                            canonical_root
                                .as_ref()
                                .and_then(|root| {
                                    root.join(target)
                                        .canonicalize()
                                        .ok()
                                        .filter(|candidate| candidate.starts_with(root))
                                })
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
        // Whole pixels, so a diagram drawn to fill it is shown unscaled; its
        // labels come out the size of the note's text.
        let diagram_room = crate::markdown_editor::mermaid::DiagramBox {
            width: wrap_width.floor(),
            height: image_max_height.floor(),
            scale: base_font_size as f32 * self.dimensions.dpi as f32
                / 72.0
                / crate::markdown_editor::mermaid::LABEL_SIZE,
        };
        let diagram_room_since = match self.right_sidebar_note_diagram_room {
            Some((room, since)) if room == diagram_room => since,
            _ => {
                let now = Instant::now();
                self.right_sidebar_note_diagram_room = Some((diagram_room, now));
                now
            }
        };
        let diagram_room_settled = diagram_room_since.elapsed() >= NOTE_DIAGRAM_SETTLE;
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
                        item_type: if is_note {
                            UIItemType::RightSidebarNoteCodeToggle(code_row.block_start)
                        } else {
                            UIItemType::RightSidebarFilePreviewMarkdownCodeToggle(
                                code_row.block_start,
                            )
                        },
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
                        if is_note {
                            UIItemType::RightSidebarNoteCodeToggle(code_row.block_start)
                        } else {
                            UIItemType::RightSidebarFilePreviewMarkdownCodeToggle(
                                code_row.block_start,
                            )
                        },
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
                        if is_note {
                            UIItemType::RightSidebarNoteCodeCopy(code_row.block_start)
                        } else {
                            UIItemType::RightSidebarFilePreviewMarkdownCodeCopy(
                                code_row.block_start,
                            )
                        },
                    )?;
                    let label_x = leading_x + control_size + self.ui_px(4);
                    self.paint_sidebar_text(
                        layers,
                        &code_label_font,
                        code_label_metrics,
                        &code_row.label,
                        label_x,
                        header_y
                            + header_height
                                .saturating_sub(code_label_metrics.cell_size.height as usize)
                                / 2,
                        copy_x.saturating_sub(label_x + self.ui_px(4)),
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
            if let Some(source) = note_image_source.clone() {
                // Load what is missing. A diagram drawn for another width is
                // drawn again once the width holds still, shown scaled meanwhile.
                let redraw = note_image.as_ref().is_some_and(|image| {
                    image.natural_size.is_some_and(|natural| {
                        crate::markdown_editor::mermaid::needs_redraw(
                            image.width,
                            crate::markdown_editor::mermaid::raster_size(natural, diagram_room).0,
                        )
                    })
                });
                if redraw && !diagram_room_settled {
                    // Paint again once it has held still, should nothing else.
                    self.update_next_frame_time(Some(diagram_room_since + NOTE_DIAGRAM_SETTLE));
                } else if note_image.is_none() || redraw {
                    self.schedule_right_sidebar_note_image(source, diagram_room);
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
        if is_note {
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
        }

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
        self.ensure_right_sidebar_note_prewarm(
            &visual,
            NotePrewarmFonts {
                ui: (Rc::clone(ui_font), ui_metrics),
                bold: (Rc::clone(&bold_font), bold_metrics),
                italic: (Rc::clone(&italic_font), italic_metrics),
                bold_italic: (Rc::clone(&bold_italic_font), bold_italic_metrics),
                h1: (Rc::clone(&h1_font), h1_metrics),
                h2: (Rc::clone(&h2_font), h2_metrics),
                h3: (Rc::clone(&h3_font), h3_metrics),
                h1_italic: (Rc::clone(&h1_italic_font), h1_italic_metrics),
                h2_italic: (Rc::clone(&h2_italic_font), h2_italic_metrics),
                h3_italic: (Rc::clone(&h3_italic_font), h3_italic_metrics),
                code: (Rc::clone(&code_font), code_metrics),
            },
        );
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
            self.clear_right_sidebar_note_failures();
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
        if let Some((failed_key, failure)) = self.right_sidebar_note_open_failure.as_ref() {
            if failed_key == &key {
                let failure = failure.clone();
                self.right_sidebar_note.load_error = Some(failure.detail.clone());
                self.right_sidebar_note_vault_failure = Some(failure);
                return false;
            }
        }
        if self.right_sidebar_note_opening.as_ref() == Some(&key) {
            self.right_sidebar_note.load_error = Some(crate::i18n::tr("right-opening-note"));
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
        self.clear_right_sidebar_note_failures();
        self.right_sidebar_note.load_error = Some(crate::i18n::tr("right-opening-note"));
        let Some(window) = self.window.as_ref().cloned() else {
            self.right_sidebar_note_opening = None;
            return false;
        };

        promise::spawn::spawn(async move {
            let worker_root = vault_root.clone();
            let worker_path = relative_path.clone();
            let result = promise::spawn::spawn_into_new_thread(move || {
                Ok(open_vault_document(&worker_root, &worker_path, create)
                    .map_err(|err| classify_vault_failure(&worker_root, &err)))
            })
            .await;
            let result = flatten_vault_worker_result(result);
            window.notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                if term_window.right_sidebar_note_open_generation != generation
                    || term_window.right_sidebar_note_opening.as_ref() != Some(&key)
                {
                    return;
                }
                term_window.right_sidebar_note_opening = None;
                match result {
                    Ok(document) => {
                        term_window.clear_right_sidebar_note_failures();
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
                    Err(failure) => {
                        term_window.right_sidebar_note.load_error = Some(failure.detail.clone());
                        term_window.right_sidebar_note_open_failure = Some((key, failure.clone()));
                        term_window.right_sidebar_note_vault_failure = Some(failure);
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
            self.clear_right_sidebar_note_failures();
            self.right_sidebar_note
                .clear_document(Some(crate::i18n::tr("right-notes-first-use")));
            return false;
        };
        let Some(project_id) =
            workspace_threads::active_project_id_for_space(&self.active_space_id)
        else {
            self.clear_right_sidebar_note_failures();
            self.right_sidebar_note
                .clear_document(Some(crate::i18n::tr("right-no-active-project")));
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
                    self.right_sidebar_note.load_error =
                        Some(crate::i18n::tr("right-indexing-vault"));
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

    /// Load `source`'s image, or draw it to fit `room` when it is a diagram;
    /// what arrives replaces what is cached. One load per source at a time.
    fn schedule_right_sidebar_note_image(
        &mut self,
        source: RightSidebarNoteImageSource,
        room: crate::markdown_editor::mermaid::DiagramBox,
    ) {
        // Diagrams are drawn one at a time anyway: starting just one keeps the
        // rest from each parking a thread until its turn.
        let is_diagram = |source: &RightSidebarNoteImageSource| {
            matches!(source, RightSidebarNoteImageSource::Mermaid { .. })
        };
        if is_diagram(&source)
            && self
                .right_sidebar_note_images_loading
                .iter()
                .any(is_diagram)
        {
            return;
        }
        if self
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
        let epoch = self.right_sidebar_note_image_epoch;
        promise::spawn::spawn(async move {
            let load_source = source.clone();
            let result = promise::spawn::spawn_into_new_thread(move || match load_source {
                RightSidebarNoteImageSource::Local(path) => load_file_preview_image(&path),
                RightSidebarNoteImageSource::Mermaid { source, style, .. } => {
                    let stage = crate::input_diagnostics::StageTimer::begin("note_diagram");
                    let drawn = crate::markdown_editor::mermaid::render(&source, style, room);
                    stage.finish(drawn.is_ok());
                    let drawn = drawn?;
                    // Kept compressed, and counted as such, as a file image is.
                    let encoded_bytes = drawn.png.len();
                    Ok(RightSidebarFilePreviewImage {
                        data: Arc::new(ImageData::with_data(ImageDataType::EncodedFile(drawn.png))),
                        width: drawn.width,
                        height: drawn.height,
                        encoded_bytes,
                        max_upscale: 1.0,
                        natural_size: Some(drawn.natural),
                    })
                }
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
                        max_upscale: NOTE_IMAGE_MAX_UPSCALE,
                        natural_size: None,
                    })
                }
            })
            .await;
            window.notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                // The cache was emptied meanwhile: nothing is waiting for this.
                if term_window.right_sidebar_note_image_epoch != epoch {
                    return;
                }
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
                        // Only failures recent enough to hold a retry back.
                        let now = Instant::now();
                        term_window
                            .right_sidebar_note_image_failures
                            .retain(|_, failed| {
                                now.duration_since(*failed) < Duration::from_secs(60)
                            });
                        term_window
                            .right_sidebar_note_image_failures
                            .insert(source.clone(), now);
                        match &source {
                            // A diagram's errors quote it, and it is the
                            // note's own text: say only that it failed.
                            RightSidebarNoteImageSource::Mermaid { source, .. } => {
                                log::warn!(
                                    "unable to draw a Mermaid diagram ({} bytes)",
                                    source.len()
                                )
                            }
                            _ => log::warn!("unable to render Note image {:?}: {err:#}", source),
                        }
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
            note_image_display_size(image.width, image.height, width, height, image.max_upscale);
        // On whole pixels, so a diagram drawn at the size it is shown maps
        // one texel to one pixel instead of blurring across two.
        let draw_x = (x + (width - draw_width) / 2.0).round();
        let draw_y = (y + (height - draw_height) / 2.0).round();
        let Some(gl_state) = self.render_state.as_ref() else {
            return Ok(());
        };
        // Same rule as terminal-cell images: once the atlas overflow chain
        // reached `No`, nothing uploads a picture this frame.
        if self.allow_images == crate::termwindow::render::paint::AllowImage::No {
            return Ok(());
        }
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
        let sidebar_bg = self.chrome_surface(chrome.workspace_sidebar_bg);
        // See-through, the band above is the window border's alone.
        if rect.y > 0 && !self.chrome_see_through() {
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
        self.paint_right_sidebar_contents(layers, |this, layers| {
            this.paint_files_preview(
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
        })
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
    /// Rebuild the visible rows from the lazily-read directory cache.
    ///
    /// Takes no index: the browse tree is independent of the search index, which
    /// may legitimately never be built. Rows are cached against the directory
    /// cache generation, so a landing read or an expand/collapse rebuilds them
    /// and nothing else does.
    fn refresh_right_sidebar_file_browse_rows(&mut self) {
        let Some((root, project_name)) = self.right_sidebar_file_view_state_key() else {
            self.right_sidebar_file_browse_rows.clear();
            self.right_sidebar_file_browse_cache_key = None;
            return;
        };
        let key = (
            self.right_sidebar_file_dir_cache.generation,
            self.right_sidebar_file_expanded_version,
        );
        if self.right_sidebar_file_browse_cache_key == Some(key) {
            return;
        }
        let (rows, missing) = right_sidebar_file_browse_rows_from_dir_cache(
            &self.right_sidebar_file_dir_cache,
            &root,
            &project_name,
            &self.right_sidebar_file_expanded,
        );
        self.right_sidebar_file_browse_rows = rows;
        self.right_sidebar_file_browse_cache_key = Some(key);
        if !missing.is_empty() {
            self.spawn_right_sidebar_dir_reads(missing);
        }
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
        // Rebuilt as the rows paint below: only rows recorded this frame may
        // carry the truncated-name hover tag.
        self.right_sidebar_truncated_file_rows.clear();
        self.right_sidebar_truncated_remote_file_rows.clear();
        if let Some(target) = self
            .active_remote_project_for_files()
            .map_err(anyhow::Error::msg)?
        {
            let changed = self.right_sidebar_remote_files.target.as_ref() != Some(&target);
            if changed {
                self.clear_right_sidebar_text_focus();
                self.right_sidebar_remote_file_tree_scroll_offset = 0.0;
                // A host whose setting says so is connected as though
                // Connect were pressed.
                if Self::remote_files_auto_connect(&target) {
                    crate::termwindow::remote_files::authorize_remote_source(
                        &crate::termwindow::remote_files::RemoteFilesState::source_key(
                            &target.source,
                        ),
                    );
                }
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
        // Anything the panel needs to say while it has no tree — such as a file
        // being dropped on it before it is connected — has to ride in as the
        // detail line, since only the tree view paints `error_message`.
        let notice = self.right_sidebar_remote_files.error_message.clone();
        let disconnected_detail = notice.as_deref().or(target_label.as_deref());
        let not_connected = crate::i18n::tr("right-not-connected");
        let connect = crate::i18n::tr("right-connect");
        let connecting = crate::i18n::tr("right-connecting");
        let connection_failed = crate::i18n::tr("right-connection-failed");
        let retry = crate::i18n::tr("right-retry");
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
                &not_connected,
                disconnected_detail,
                Some((&connect, true)),
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
                &connecting,
                target_label.as_deref(),
                Some((&connecting, false)),
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
                &connection_failed,
                Some(message.as_str()),
                Some((&retry, true)),
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
                UIItemType::RightSidebarRemoteFileConnect,
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
    /// A plugin's panel waiting to connect shows the same one (`item_type`).
    pub(crate) fn paint_remote_files_connect_button(
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
        item_type: UIItemType,
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
                item_type,
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
        // The same filter box as a local project; the project's name heads
        // the tree below it.
        let filter_width = content_width.saturating_sub(refresh_size + self.ui_px(SIDEBAR_INSET));
        let filter_label = crate::i18n::tr("right-filter-files");
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
            &filter_label,
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
        let query = self.right_sidebar_file_filter_for_tree().trim().to_string();
        self.update_remote_file_search(&query);
        let searching = !query.is_empty();
        let rows = if searching {
            Arc::clone(&self.right_sidebar_remote_file_search.rows)
        } else {
            Arc::new(self.right_sidebar_remote_files.rows())
        };
        if rows.is_empty() {
            let search = &self.right_sidebar_remote_file_search;
            let message = match (&search.status, search.index.is_some()) {
                _ if !searching => crate::i18n::tr("right-loading-remote-directory"),
                (RemoteFileSearchStatus::NeedsUpdate, _) => {
                    crate::i18n::tr("right-remote-search-needs-update")
                }
                (RemoteFileSearchStatus::Failed(message), false) => message.clone(),
                (_, false) => crate::i18n::tr("right-indexing-files"),
                // Still searching: say nothing rather than "no matches".
                (_, true) if search.search_cancel.is_some() => String::new(),
                (_, true) => crate::i18n::tr("right-no-matching-files"),
            };
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
                &message,
            );
        }

        let row_metrics = right_sidebar_file_row_metrics(ui_metrics);
        let truncated_height =
            if !searching && self.right_sidebar_remote_files.has_truncated_directory() {
                row_metrics.row_height
            } else {
                0
            };
        let strip_rows = self.transfer_strip_rows(
            content_bottom
                .saturating_sub(tree_top)
                .saturating_sub(self.ui_px(SIDEBAR_INSET).saturating_add(truncated_height)),
            row_metrics.row_height,
        );
        let footer_height =
            truncated_height.saturating_add(strip_rows.saturating_mul(row_metrics.row_height));
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
                &filter_label,
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
            // Full opacity exactly at the mask's bottom edge; see the
            // local tree above for why starting higher reads as a cut.
            let fade_top = tree_top;
            let fade_height = self
                .ui_px(FILE_SCROLL_FADE_HEIGHT)
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
        let mut footer_y = viewport_bottom;
        if truncated_height > 0 {
            self.paint_sidebar_text(
                layers,
                ui_font,
                ui_metrics,
                &crate::i18n::tr("right-more-entries"),
                content_x,
                footer_y,
                content_width,
                muted_fg,
            )?;
            footer_y += truncated_height;
        }
        self.paint_transfer_strip(
            layers,
            ui_font,
            ui_metrics,
            chrome,
            foreground,
            muted_fg,
            content_x,
            footer_y,
            content_width,
            strip_rows,
            row_metrics,
        )?;
        Ok(())
    }

    /// How many strip rows fit in `available` vertical pixels.
    ///
    /// Capped twice: by a fixed count, and by never taking more than half of
    /// what is left. Dropping thirty files at once must not collapse the tree
    /// to nothing or push rows up over the header.
    fn transfer_strip_rows(&self, available: usize, row_height: usize) -> usize {
        if self.right_sidebar_remote_transfers.is_empty() {
            return 0;
        }
        self.right_sidebar_remote_transfers
            .len()
            .min(REMOTE_TRANSFER_STRIP_MAX)
            .min((available / 2) / row_height.max(1))
    }

    /// Paint the transfer strip. Shared by both trees: a local copy started
    /// from the local panel has to report itself there, not only in the remote
    /// one where the rows happen to live.
    #[allow(clippy::too_many_arguments)]
    fn paint_transfer_strip(
        &mut self,
        layers: &mut TripleLayerQuadAllocator,
        ui_font: &Rc<LoadedFont>,
        ui_metrics: RenderMetrics,
        chrome: UiPalette,
        foreground: LinearRgba,
        muted_fg: LinearRgba,
        content_x: usize,
        mut y: usize,
        content_width: usize,
        strip_rows: usize,
        row_metrics: RightSidebarFileRowMetrics,
    ) -> anyhow::Result<()> {
        if strip_rows == 0 {
            return Ok(());
        }
        // When some are hidden, the last visible row reports how many rather
        // than letting them vanish silently.
        let hidden = self
            .right_sidebar_remote_transfers
            .len()
            .saturating_sub(strip_rows);
        let painted = if hidden > 0 {
            strip_rows.saturating_sub(1)
        } else {
            strip_rows
        };
        for index in 0..painted {
            self.paint_remote_transfer_row(
                layers,
                ui_font,
                ui_metrics,
                chrome,
                foreground,
                muted_fg,
                content_x,
                y,
                content_width,
                index,
                row_metrics,
            )?;
            y += row_metrics.row_height;
        }
        if hidden > 0 {
            let more = if hidden == 1 {
                crate::i18n::tr("right-more-transfer")
            } else {
                right_sidebar_arg("right-more-transfers", "count", hidden.to_string())
            };
            self.paint_sidebar_text(
                layers,
                ui_font,
                ui_metrics,
                &more,
                content_x + self.ui_px(SIDEBAR_INSET),
                y + row_metrics
                    .row_height
                    .saturating_sub(ui_metrics.cell_size.height as usize)
                    / 2,
                content_width.saturating_sub(self.ui_px(SIDEBAR_INSET)),
                muted_fg,
            )?;
        }
        Ok(())
    }

    /// One line in the transfer strip: an icon telling running from finished
    /// from failed, the file name, a short status, and — while it runs — a
    /// progress line beneath.
    #[allow(clippy::too_many_arguments)]
    fn paint_remote_transfer_row(
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
        index: usize,
        row_metrics: RightSidebarFileRowMetrics,
    ) -> anyhow::Result<()> {
        let Some(transfer) = self.right_sidebar_remote_transfers.get(index) else {
            return Ok(());
        };
        let id = transfer.id;
        let running = transfer.is_running();
        let (icon, tint) = match &transfer.status {
            RemoteTransferStatus::Running => (SvgIcon::LoaderCircle, foreground),
            RemoteTransferStatus::Done(_) => (SvgIcon::CircleCheck, chrome.selected_bg),
            // Muted rather than red, matching the panel's existing
            // "Connection failed" state.
            RemoteTransferStatus::Failed(_) | RemoteTransferStatus::FailedWithLeftover { .. } => {
                (SvgIcon::CircleAlert, muted_fg)
            }
        };
        let verb = transfer.kind.verb();
        let detail = match &transfer.status {
            RemoteTransferStatus::Running => match transfer.progress.items() {
                // A folder reads better as "12/300" than as a percentage.
                Some((done, total)) => format!("{verb} {done}/{total}"),
                None => match transfer.progress.fraction() {
                    Some(fraction) => format!("{verb} {}%", (fraction * 100.0).round() as u32),
                    None => verb.to_string(),
                },
            },
            RemoteTransferStatus::Done(detail) => detail.clone(),
            RemoteTransferStatus::Failed(message) => message.clone(),
            RemoteTransferStatus::FailedWithLeftover { message, leftover } => {
                format!("{message} — {leftover} was left behind")
            }
        };
        let label = format!("{} — {detail}", transfer.name);
        let fraction = transfer.progress.fraction();

        let hovered = self.is_pointer_over_ui_rect(x, y, width, row_metrics.row_height);
        if hovered {
            self.fill_rounded_rectangle(
                layers,
                1,
                euclid::rect(
                    x as f32,
                    y as f32,
                    width as f32,
                    row_metrics.row_height as f32,
                ),
                chrome.sidebar_button_hover_bg,
                self.ui_f32(SIDEBAR_ROW_RADIUS),
            )
            .context("remote transfer row hover")?;
        }
        self.ui_items.push(UIItem {
            x,
            y,
            width,
            height: row_metrics.row_height,
            item_type: UIItemType::RightSidebarRemoteTransfer(id),
        });

        let icon_x = x + self.ui_px(SIDEBAR_INSET);
        let icon_y = y + row_metrics.row_height.saturating_sub(row_metrics.icon_size) / 2;
        if running {
            self.paint_spinning_ui_icon(
                layers,
                1,
                icon,
                icon_x,
                icon_y,
                row_metrics.icon_size,
                tint,
            )?;
        } else {
            self.paint_ui_icon(layers, 1, icon, icon_x, icon_y, row_metrics.icon_size, tint)?;
        }

        let text_x = icon_x + row_metrics.icon_size + row_metrics.icon_gap;
        let text_width = width.saturating_sub(text_x.saturating_sub(x));
        let text_y = y + row_metrics
            .row_height
            .saturating_sub(ui_metrics.cell_size.height as usize)
            / 2;
        self.paint_sidebar_text(
            layers,
            ui_font,
            ui_metrics,
            &label,
            text_x,
            text_y,
            text_width,
            if running { foreground } else { muted_fg },
        )?;

        if running {
            let bar_height = self.ui_px(REMOTE_TRANSFER_PROGRESS_HEIGHT).max(1);
            let bar_y = y + row_metrics.row_height.saturating_sub(bar_height);
            let radius = (bar_height as f32) / 2.0;
            self.fill_rounded_rectangle(
                layers,
                1,
                euclid::rect(
                    text_x as f32,
                    bar_y as f32,
                    text_width as f32,
                    bar_height as f32,
                ),
                chrome.separator.mul_alpha(0.72),
                radius,
            )
            .context("remote transfer progress track")?;
            let (fill_x, filled) = match fraction {
                Some(fraction) => (
                    text_x as f32,
                    (text_width as f32 * fraction).max(radius * 2.0),
                ),
                None => {
                    // While a remote tree is still being enumerated there is
                    // no honest denominator. Show a moving segment instead of
                    // hiding the bar and making the operation look stuck.
                    let epoch = REMOTE_TRANSFER_ANIMATION_EPOCH.get_or_init(Instant::now);
                    let phase = epoch.elapsed().as_secs_f32() % 1.2 / 1.2;
                    let segment = (text_width as f32 * 0.3).max(radius * 2.0);
                    let raw_start = phase * (text_width as f32 + segment) - segment;
                    let start = raw_start.max(0.0);
                    let end = (raw_start + segment).min(text_width as f32);
                    (text_x as f32 + start, (end - start).max(0.0))
                }
            };
            if filled > 0.0 {
                self.fill_rounded_rectangle(
                    layers,
                    1,
                    euclid::rect(fill_x, bar_y as f32, filled, bar_height as f32),
                    chrome.selected_bg,
                    radius,
                )
                .context("remote transfer progress fill")?;
            }
            // A running transfer must keep repainting even when nothing else
            // changes, or the percentage freezes until the next event.
            self.update_next_frame_time(Some(Instant::now() + Duration::from_millis(100)));
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
        // A drag hovering here is about to drop into this directory; make that
        // unmistakable, and let it outrank the ordinary selection tint.
        let is_drop_target =
            self.right_sidebar_remote_drop_target.as_ref() == Some(&row.entry.path);
        if hovered || is_selected || is_drop_target {
            self.fill_rounded_rectangle(
                layers,
                1,
                euclid::rect(
                    x as f32,
                    visible_y as f32,
                    width as f32,
                    visible_height as f32,
                ),
                if is_drop_target {
                    chrome.selected_bg
                } else if is_selected {
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
        let text_width = x
            .saturating_add(width)
            .saturating_sub(text_x + self.ui_px(SIDEBAR_INSET));
        if let Some(input) = self
            .sidebar_remote_file_rename_input(&row.entry.path)
            .cloned()
        {
            self.paint_snippet_text_box(
                layers,
                1,
                ui_font,
                ui_metrics,
                chrome,
                muted_fg,
                text_x,
                y + self.ui_px(2),
                text_width,
                row_metrics.row_height.saturating_sub(self.ui_px(4)),
                None,
                "",
                &input,
                true,
                UIItemType::RightSidebarRemoteFileRow(row.entry.path.clone()),
                false,
            )
        } else {
            // Same rule as the local tree: only a name the ellipsis actually
            // cut earns the hover tag.
            if matches!(
                self.ellipsize_ui_text(ui_font, &row.entry.name, text_width)?,
                Cow::Owned(_)
            ) {
                self.right_sidebar_truncated_remote_file_rows
                    .insert(row.entry.path.clone());
            }
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
                text_width,
                if row.entry.is_directory() || is_selected {
                    foreground
                } else {
                    muted_fg
                },
            )
        }
    }

    pub(crate) fn active_remote_project_for_files(
        &self,
    ) -> Result<Option<workspace_threads::RemoteFilesTarget>, String> {
        let mux = Mux::get();
        // In a collection window the displayed workspace belongs to another
        // Space; Files must target that Space's machine, not the (local)
        // collection.
        let space_id = self.content_space_id();
        let active_workspace = self
            .current_mux_workspace()
            .unwrap_or_else(|| mux.active_workspace());
        let workspaces = mux.iter_workspaces();
        let view = workspace_threads::view_for_current_project(
            &space_id,
            &active_workspace,
            &workspaces,
            false,
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
        if let Some(target) = workspace_threads::remote_files_target(&space_id, &project.id) {
            return Ok(Some(target));
        }
        let Some(domain) = workspace_threads::client_domain_for_space(&space_id) else {
            return Err("Remote Files source is unavailable".to_string());
        };
        Ok(Some(workspace_threads::RemoteFilesTarget {
            project_id: project.id.clone(),
            project_name: project.name.clone(),
            source: workspace_threads::RemoteFilesSource::ClientDomain(domain),
            requested_root: "~".to_string(),
        }))
    }

    /// Seconds between keepalives on the Files connection. Unlike a terminal
    /// session this one carries no traffic while the panel sits idle, so
    /// without this the server (or a NAT) reaps it and the next browse fails.
    const REMOTE_FILES_KEEPALIVE_SECS: &'static str = "30";

    pub(crate) fn ssh_config_for_remote_files_target(
        target: &workspace_threads::RemoteFilesTarget,
    ) -> Result<config::SshDomain, String> {
        let mut domain = Self::ssh_config_for_remote_files_source(target)?;
        domain
            .ssh_option
            .entry("serveraliveinterval".to_string())
            .or_insert_with(|| Self::REMOTE_FILES_KEEPALIVE_SECS.to_string());
        Ok(domain)
    }

    /// Whether the Files panel connects by itself to the machine `target`
    /// names: a saved host -- reached directly, or through a mux domain it is
    /// connected under -- whose setting says to.
    fn remote_files_auto_connect(target: &workspace_threads::RemoteFilesTarget) -> bool {
        match &target.source {
            workspace_threads::RemoteFilesSource::SshHost(host_id) => {
                crate::ssh_hosts::host_spec(host_id).is_some_and(|spec| spec.files_auto_connect)
            }
            workspace_threads::RemoteFilesSource::ClientDomain(domain) => {
                crate::ssh_hosts::list_all_hosts().into_iter().any(|entry| {
                    entry.spec.files_auto_connect
                        && crate::ssh_hosts::domain_names_for_host(&entry.spec)
                            .iter()
                            .any(|name| name == domain)
                })
            }
        }
    }

    fn ssh_config_for_remote_files_source(
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
                                let connection_id = lease.connection_id();
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
                                            invalidate_remote_connection(
                                                &connection_key,
                                                connection_id,
                                            );
                                            Err(RemoteAcquireError::Failed(err))
                                        }
                                    },
                                    Err(err) => {
                                        drop(lease);
                                        invalidate_remote_connection(
                                            &connection_key,
                                            connection_id,
                                        );
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
                                    if term_window
                                        .current_remote_connection_key()
                                        .as_deref()
                                        != Some(lease.connection_key())
                                    {
                                        drop(lease);
                                        term_window
                                            .reconnect_remote_files_after_connection_change(
                                                generation,
                                                "Remote Files configuration changed while connecting"
                                                    .to_string(),
                                            );
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
                    if self.current_remote_connection_key().as_deref()
                        != Some(lease.connection_key())
                    {
                        self.reconnect_remote_files_after_connection_change(
                            generation,
                            "Remote Files configuration changed before loading the folder"
                                .to_string(),
                        );
                        continue;
                    }
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
                    let connection_id = lease.connection_id();
                    let Some(window) = self.window.as_ref().cloned() else {
                        continue;
                    };
                    promise::spawn::spawn(async move {
                        let result = backend.list_directory(path.clone(), limit).await;
                        drop(operation_lease);
                        let connection_died = result.as_ref().is_err_and(|message| {
                            invalidate_remote_connection_if_dead(
                                &connection_key,
                                connection_id,
                                message,
                            )
                        });
                        window.notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                            if term_window
                                .right_sidebar_remote_files
                                .current_source_key()
                                .as_deref()
                                != Some(source_key.as_str())
                                || term_window.right_sidebar_remote_files.generation != generation
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
                                    match term_window.remote_files_lease_failure_disposition(
                                        &connection_key,
                                        connection_id,
                                    ) {
                                        RemoteLeaseFailureDisposition::ReplacementForSameTarget => {
                                            term_window.apply_right_sidebar_remote_files_effects(
                                                vec![RemoteFilesEffect::ListDirectory {
                                                    generation,
                                                    source_key,
                                                    path,
                                                    limit,
                                                }],
                                            );
                                            term_window.invalidate_window();
                                            return;
                                        }
                                        RemoteLeaseFailureDisposition::ReplacementForDifferentTarget => {
                                            term_window
                                                .reconnect_remote_files_after_connection_change(
                                                    generation,
                                                    "Remote Files connection changed while loading the folder"
                                                        .to_string(),
                                                );
                                            term_window.invalidate_window();
                                            return;
                                        }
                                        RemoteLeaseFailureDisposition::FailedConnectionInstalled
                                        | RemoteLeaseFailureDisposition::NoLease => {}
                                    }
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
                            // A just-created folder waits for its listing to
                            // land before its inline rename can begin.
                            term_window.maybe_begin_pending_remote_rename();
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

    /// The right sidebar's background item spans the whole panel and is
    /// painted first, so anything inside the panel sits on top of it.
    fn pointer_is_over_right_sidebar(&self, x: isize, y: isize) -> bool {
        self.ui_items.iter().any(|item| {
            matches!(item.item_type, UIItemType::RightSidebarBackground) && item.hit_test(x, y)
        })
    }

    /// Which remote directory a pointer at `coords` is aiming at, if the
    /// remote Files panel is showing a connected tree there.
    pub(crate) fn remote_drop_target_at(&self, x: isize, y: isize) -> Option<RemotePath> {
        if !self.right_sidebar_file_view_active()
            || !matches!(
                self.right_sidebar_remote_files.phase,
                RemoteFilesPhase::Connected
            )
        {
            return None;
        }
        let root = self.right_sidebar_remote_files.root.as_ref()?;
        resolve_remote_drop_target(&self.ui_items, x, y, root, |path| {
            self.right_sidebar_remote_files.kind_for_path(path)
        })
    }

    pub(crate) fn update_right_sidebar_remote_drop_target(&mut self, coords: Option<Point>) {
        let target = coords.and_then(|coords| self.remote_drop_target_at(coords.x, coords.y));
        if self.right_sidebar_remote_drop_target != target {
            self.right_sidebar_remote_drop_target = target;
            self.invalidate_window();
        }
    }

    pub(crate) fn clear_right_sidebar_remote_drop_target(&mut self) {
        if self.right_sidebar_remote_drop_target.take().is_some() {
            self.invalidate_window();
        }
    }

    pub(crate) fn update_right_sidebar_local_drop_target(&mut self, coords: Option<Point>) {
        let target = coords.and_then(|coords| self.local_drop_target_at(coords.x, coords.y));
        if self.right_sidebar_local_drop_target != target {
            self.right_sidebar_local_drop_target = target;
            self.invalidate_window();
        }
    }

    pub(crate) fn clear_right_sidebar_local_drop_target(&mut self) {
        if self.right_sidebar_local_drop_target.take().is_some() {
            self.invalidate_window();
        }
    }

    /// Files dropped onto the TERMINAL of a remote session: upload them and
    /// paste the remote paths, where a local session would have pasted local
    /// ones. Returns false when the active Space is not remote, so the caller
    /// falls back to the plain local-path paste.
    pub(crate) fn upload_dropped_files_to_remote_terminal(
        &mut self,
        paths: &[PathBuf],
        coords: Option<Point>,
    ) -> bool {
        let Some(pane) = self.get_active_pane_or_overlay() else {
            return false;
        };
        let paste_target = self.capture_terminal_paste_target(&pane);
        self.upload_files_to_terminal_paste_target(paths, coords, paste_target)
    }

    /// Resolve everything an asynchronous typed paste is allowed to use while
    /// the initiating pane and Space are still authoritative. Later callbacks
    /// carry this snapshot instead of consulting whichever pane is active then.
    pub(crate) fn capture_terminal_paste_target(
        &self,
        pane: &Arc<dyn Pane>,
    ) -> TerminalPasteTarget {
        let pane_id = pane.pane_id();
        let Ok(Some(target)) = self.active_remote_project_for_files() else {
            return TerminalPasteTarget {
                pane_id,
                remote: None,
            };
        };
        // The paste target must live on the SAME host the upload goes to.
        // The Space being remote is not enough: the active pane can be a
        // local overlay (the debug pane), or in principle belong to another
        // domain entirely — pasting host A's path there would name nothing.
        // Anything that does not match falls back to the local-path paste.
        // (A mosh thread's pane is a local domain running mosh-client, so
        // mosh Spaces deliberately keep the old behavior for now.)
        if !pane_domain_matches_remote_source(pane.domain_id(), &target.source) {
            return TerminalPasteTarget {
                pane_id,
                remote: None,
            };
        }
        let source_key =
            crate::termwindow::remote_files::RemoteFilesState::source_key(&target.source);
        let config = match Self::ssh_config_for_remote_files_target(&target) {
            Ok(config) => config,
            Err(message) => {
                return TerminalPasteTarget {
                    pane_id,
                    remote: Some(Err(message)),
                };
            }
        };

        let setting = crate::native_settings::remote_drop_destination();
        // The cwd is only consulted when asked for, and only as a hint: a pane
        // that has not reported OSC 7 yet falls back to the default folder
        // rather than failing the drop.
        let cwd = (setting == crate::native_settings::REMOTE_DROP_DESTINATION_CWD)
            .then(|| {
                pane.get_current_working_dir(mux::pane::CachePolicy::AllowStale)
                    .and_then(|url| {
                        percent_encoding::percent_decode_str(url.path())
                            .decode_utf8()
                            .ok()
                            .map(|path| path.to_string())
                    })
            })
            .flatten();
        let destination = resolve_drop_destination(&setting, cwd.as_deref());
        let connection_key = remote_connection_key(&source_key, &config);
        TerminalPasteTarget {
            pane_id,
            remote: Some(Ok(RemoteTerminalPasteTarget {
                space_id: self.content_space_id(),
                target,
                source_key,
                connection_key,
                config,
                destination,
            })),
        }
    }

    fn validate_terminal_paste_target(
        &self,
        paste_target: &TerminalPasteTarget,
    ) -> Result<(), String> {
        let Some(remote) = paste_target.remote.as_ref() else {
            return Err("The paste did not originate in a remote terminal".to_string());
        };
        let remote = remote.as_ref().map_err(Clone::clone)?;
        let current_target = self
            .active_remote_project_for_files()
            .map_err(|message| format!("Paste canceled: {message}"))?;
        let current_config = Self::ssh_config_for_remote_files_target(&remote.target)
            .map_err(|message| format!("Paste canceled: {message}"))?;
        let current_key = remote_connection_key(&remote.source_key, &current_config);
        let pane = Mux::get()
            .get_pane(paste_target.pane_id)
            .ok_or_else(|| "Paste canceled because its pane was closed".to_string())?;
        let mismatch = terminal_paste_snapshot_mismatch(
            &remote.space_id,
            &self.content_space_id(),
            &remote.target,
            current_target.as_ref(),
            &remote.connection_key,
            Some(&current_key),
            pane_domain_matches_remote_source(pane.domain_id(), &remote.target.source),
        );
        if let Some(mismatch) = mismatch {
            return Err(match mismatch {
                TerminalPasteSnapshotMismatch::Space => {
                    "Paste canceled because the window switched Spaces".to_string()
                }
                TerminalPasteSnapshotMismatch::Project => {
                    "Paste canceled because the remote project changed".to_string()
                }
                TerminalPasteSnapshotMismatch::Connection => {
                    "Paste canceled because the remote connection changed".to_string()
                }
                TerminalPasteSnapshotMismatch::Pane => {
                    "Paste canceled because its pane no longer belongs to that host".to_string()
                }
            });
        }
        Ok(())
    }

    fn push_terminal_paste_failures(&mut self, paths: &[PathBuf], message: String) {
        for path in paths {
            self.push_remote_transfer_failure(
                RemoteTransferKind::Upload,
                display_name(path),
                message.clone(),
                None,
            );
        }
        self.invalidate_window();
    }

    pub(crate) fn upload_files_to_terminal_paste_target(
        &mut self,
        paths: &[PathBuf],
        coords: Option<Point>,
        paste_target: TerminalPasteTarget,
    ) -> bool {
        let Some(remote) = paste_target.remote.as_ref() else {
            return false;
        };
        let remote = match remote {
            Ok(remote) => remote.clone(),
            Err(message) => {
                self.push_terminal_paste_failures(paths, message.clone());
                return true;
            }
        };
        if let Err(message) = self.validate_terminal_paste_target(&paste_target) {
            self.push_terminal_paste_failures(paths, message);
            return true;
        }

        // Align the panel's state machine with the captured Space's target
        // only after revalidation. A delayed clipboard read must never point
        // the panel (and its connection lease) at a different host.
        let effects = self
            .right_sidebar_remote_files
            .transition(RemoteFilesEvent::TargetChanged(Some(remote.target.clone())));
        self.apply_right_sidebar_remote_files_effects(effects);

        let Some(window) = self.window.as_ref().cloned() else {
            return true;
        };
        // A drop is an explicit ask, so dialing is allowed — and a successful
        // dial authorizes the source exactly as the panel's Connect click
        // would (the session credentials are the same either way).
        let batch: Vec<PathBuf> = paths.to_vec();
        let anchor = coords.unwrap_or_else(|| Point::new(0, 0));
        let dispatch_target = paste_target.clone();
        promise::spawn::spawn(async move {
            let manager = remote_connection_manager();
            let prepared = match manager
                .acquire(remote.connection_key, remote.config, true)
                .await
            {
                Ok(lease) => {
                    let backend = lease.backend();
                    match backend.resolve_root(remote.destination).await {
                        // Merge-tolerant creation: the folder existing already
                        // is the normal case after the first drop.
                        Ok(directory) => match backend.create_directory(directory.clone()).await {
                            Ok(()) => Ok((lease, directory)),
                            Err(message) => Err(message),
                        },
                        Err(message) => Err(message),
                    }
                }
                Err(RemoteAcquireError::Failed(message)) => Err(message),
                Err(RemoteAcquireError::NotConnected) => {
                    Err("Remote Files connection is not available".to_string())
                }
            };
            window.notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                term_window.dispatch_terminal_drop(prepared, batch, dispatch_target, anchor);
            })));
        })
        .detach();
        true
    }

    /// An image pasted from the clipboard: encode to PNG, park it in the
    /// session's temp folder under a screenshot-style name, and hand it to
    /// the ordinary terminal-drop upload (which pastes the remote path).
    ///
    /// The temp file deliberately outlives the upload: the transfer row's
    /// Retry re-reads the local path, so deleting on completion would break
    /// it. The OS reaps the temp dir between sessions.
    pub(crate) fn upload_pasted_image_to_remote_terminal(
        &mut self,
        paste_target: TerminalPasteTarget,
        format: window::ClipboardImageFormat,
        bytes: Vec<u8>,
    ) {
        let Some(remote) = paste_target.remote.as_ref() else {
            return;
        };
        let name = pasted_image_file_name(chrono::Local::now());
        if let Err(message) = remote {
            self.push_remote_transfer_failure(
                RemoteTransferKind::Upload,
                name,
                message.clone(),
                None,
            );
            self.invalidate_window();
            return;
        }
        if let Err(message) = self.validate_terminal_paste_target(&paste_target) {
            self.push_remote_transfer_failure(RemoteTransferKind::Upload, name, message, None);
            self.invalidate_window();
            return;
        }
        let Some(window) = self.window.as_ref().cloned() else {
            return;
        };
        promise::spawn::spawn(async move {
            let worker_name = name.clone();
            let staged = spawn_pasted_image_staging(format, bytes, worker_name).await;
            window.notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                match staged {
                    Ok(staged) => {
                        // Revalidate after potentially expensive transcoding.
                        // Dropping `staged` on failure removes the private temp
                        // directory before any path can be retained for Retry.
                        if let Err(message) =
                            term_window.validate_terminal_paste_target(&paste_target)
                        {
                            term_window.push_remote_transfer_failure(
                                RemoteTransferKind::Upload,
                                name,
                                message,
                                None,
                            );
                            term_window.invalidate_window();
                            return;
                        }
                        let path = staged.keep();
                        term_window.upload_files_to_terminal_paste_target(
                            &[path],
                            None,
                            paste_target,
                        );
                    }
                    Err(err) => {
                        term_window.push_remote_transfer_failure(
                            RemoteTransferKind::Upload,
                            name,
                            format!("Could not stage the pasted image: {err}"),
                            None,
                        );
                        term_window.invalidate_window();
                    }
                }
            })));
        })
        .detach();
    }

    /// The prepared half of a terminal drop, back on the window thread: adopt
    /// the connection, then hand every dropped path to the ordinary upload
    /// machinery with the pane recorded as the paste target.
    fn dispatch_terminal_drop(
        &mut self,
        prepared: Result<
            (
                crate::termwindow::remote_files::RemoteConnectionLease,
                RemotePath,
            ),
            String,
        >,
        paths: Vec<PathBuf>,
        paste_target: TerminalPasteTarget,
        anchor: Point,
    ) {
        if let Err(message) = self.validate_terminal_paste_target(&paste_target) {
            self.push_terminal_paste_failures(&paths, message);
            return;
        }
        let remote = match paste_target.remote.as_ref() {
            Some(Ok(remote)) => remote.clone(),
            Some(Err(message)) => {
                self.push_terminal_paste_failures(&paths, message.clone());
                return;
            }
            None => return,
        };
        let pane_id = paste_target.pane_id;
        let (lease, directory) = match prepared {
            Ok(prepared) => prepared,
            Err(message) => {
                for path in &paths {
                    self.push_remote_transfer_failure(
                        RemoteTransferKind::Upload,
                        display_name(path),
                        message.clone(),
                        None,
                    );
                }
                self.invalidate_window();
                return;
            }
        };
        let origin = RemoteOperationOrigin::new(
            remote.source_key.clone(),
            lease.connection_key().to_string(),
        );
        if !self.remote_operation_origin_matches(&origin) {
            for path in &paths {
                self.push_remote_transfer_failure(
                    RemoteTransferKind::Upload,
                    display_name(path),
                    "The window moved to another host while connecting".to_string(),
                    None,
                );
            }
            self.invalidate_window();
            return;
        }
        crate::termwindow::remote_files::authorize_remote_source(&remote.source_key);
        // Lend the fresh connection to the panel's lease slot unless the slot
        // already holds this exact connection: `remote_transfer_handles` reads
        // that slot, so every upload below — and the Files panel itself — uses
        // whatever sits there. A slot left holding a session that died without
        // any failure callback noticing, or one dialled before this host's
        // settings changed, would otherwise defeat the reconnect we just did.
        let already_installed = self
            .right_sidebar_remote_files_lease
            .as_ref()
            .is_some_and(|current| current.is_same_connection(&lease));
        if !already_installed {
            self.right_sidebar_remote_files_lease = Some(lease);
        }
        for path in paths {
            if path.is_dir() {
                self.start_remote_folder_upload(path, directory.clone(), anchor, Some(pane_id));
                continue;
            }
            let Some(name) = path
                .file_name()
                .and_then(|name| name.to_str())
                .map(str::to_string)
            else {
                self.push_remote_transfer_failure(
                    RemoteTransferKind::Upload,
                    path.to_string_lossy().to_string(),
                    "That file name is not valid UTF-8".to_string(),
                    None,
                );
                continue;
            };
            self.spawn_terminal_drop_file_upload(
                path,
                name,
                directory.clone(),
                pane_id,
                origin.clone(),
            );
        }
        self.invalidate_window();
    }

    /// One dropped file: settle a free remote name off-thread, then run the
    /// ordinary single-file upload with the pane as paste target.
    fn spawn_terminal_drop_file_upload(
        &mut self,
        local: PathBuf,
        name: String,
        directory: RemotePath,
        pane_id: PaneId,
        origin: RemoteOperationOrigin,
    ) {
        let Some((backend, operation_lease, _connection_key, _connection_id)) =
            self.remote_transfer_handles(&origin)
        else {
            self.push_remote_transfer_failure(
                RemoteTransferKind::Upload,
                name,
                "Remote Files connection is no longer available".to_string(),
                None,
            );
            return;
        };
        let Some(window) = self.window.as_ref().cloned() else {
            return;
        };
        promise::spawn::spawn(async move {
            let chosen = pick_free_remote_name(&*backend, &directory, &name).await;
            drop(operation_lease);
            window.notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                // Reserving the name is another round trip, and the window can
                // be pointed at a different host while it runs:
                // `start_remote_upload` takes both its origin and its backend
                // from whatever is displayed NOW, so an unchecked hand-off
                // would write this host's absolute path onto the other one and
                // then paste it into a pane where it means something else.
                if !term_window.remote_operation_origin_matches(&origin) {
                    term_window.push_remote_transfer_failure(
                        RemoteTransferKind::Upload,
                        name,
                        "The window moved to another host before the upload started".to_string(),
                        None,
                    );
                    term_window.invalidate_window();
                    return;
                }
                match chosen {
                    Ok(remote) => {
                        term_window.start_remote_upload(local, remote, directory, Some(pane_id));
                    }
                    Err(message) => {
                        term_window.push_remote_transfer_failure(
                            RemoteTransferKind::Upload,
                            name,
                            message,
                            None,
                        );
                    }
                }
                term_window.invalidate_window();
            })));
        })
        .detach();
    }

    /// Put a landed file's remote path on the pane's command line.
    ///
    /// Always POSIX-quoted, regardless of `quote_dropped_files`: that setting
    /// is about the LOCAL platform's shell, while this path is by construction
    /// on a POSIX server. The distinction bites immediately — the default
    /// SpacesOnly mode leaves `(` bare, and our collision names (` (1)`) put
    /// parentheses in every re-dropped screenshot. Verified live: bash chokes
    /// on the unquoted form.
    pub(crate) fn paste_remote_path_to_pane(&mut self, pane_id: PaneId, remote: &RemotePath) {
        let Some(pane) = Mux::get().get_pane(pane_id) else {
            // The pane closed while the upload ran; the transfer row already
            // says where the file went.
            return;
        };
        let text = format!(
            "{} ",
            config::DroppedFileQuoting::Posix.escape(remote.as_str())
        );
        if let Err(err) = pane.send_paste(&text) {
            log::error!(
                "failed to paste the uploaded path {}: {err:#}",
                remote.as_str()
            );
        }
    }

    /// Take files dropped onto the remote tree and upload them. Returns false
    /// when the drop was not aimed at the panel, so the caller can fall back
    /// to its usual handling.
    pub(crate) fn upload_dropped_files_to_remote(
        &mut self,
        paths: &[PathBuf],
        coords: Option<Point>,
    ) -> bool {
        self.clear_right_sidebar_remote_drop_target();
        let Some(coords) = coords else {
            return false;
        };
        // Aimed at the panel but not usable: say so rather than silently
        // pasting the paths into the terminal behind it.
        if self.pointer_is_over_right_sidebar(coords.x, coords.y)
            && self.right_sidebar_file_view_active()
            && self.right_sidebar_remote_files.target.is_some()
            && !matches!(
                self.right_sidebar_remote_files.phase,
                RemoteFilesPhase::Connected
            )
        {
            self.right_sidebar_remote_files.error_message =
                Some("Connect before dropping files here".to_string());
            self.invalidate_window();
            return true;
        }
        let Some(directory) = self.remote_drop_target_at(coords.x, coords.y) else {
            return false;
        };

        for path in paths {
            if path.is_dir() {
                self.start_remote_folder_upload(path.clone(), directory.clone(), coords, None);
                continue;
            }
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                self.push_remote_transfer_failure(
                    RemoteTransferKind::Upload,
                    path.to_string_lossy().to_string(),
                    "That file name is not valid UTF-8".to_string(),
                    None,
                );
                continue;
            };
            // Build the destination from the *name* only: a local path could
            // carry separators (or a drive letter) that must never be spliced
            // into a remote path.
            let remote = match directory.join_name(name) {
                Ok(remote) => remote,
                Err(err) => {
                    self.push_remote_transfer_failure(
                        RemoteTransferKind::Upload,
                        name.to_string(),
                        err,
                        None,
                    );
                    continue;
                }
            };
            self.start_remote_upload(path.clone(), remote, directory.clone(), None);
        }
        self.invalidate_window();
        true
    }

    /// Walk a dropped folder and upload it as a single transfer.
    ///
    /// One row, not one per file: running rows are never evicted from the
    /// strip, so a thousand-file folder would otherwise bury the panel.
    fn start_remote_folder_upload(
        &mut self,
        source: PathBuf,
        directory: RemotePath,
        anchor: Point,
        paste_to: Option<PaneId>,
    ) {
        // The walk and its metadata calls must not run here: a large or
        // network-backed tree would freeze the window for as long as it takes,
        // before even a progress row appears.
        let Some(window) = self.window.as_ref().cloned() else {
            return;
        };
        // Remember which host the drop was aimed at. Walking is slow, and the
        // panel can be pointed somewhere else in the meantime — uploading to
        // whatever happens to be connected when the walk finishes would put
        // the files on the wrong machine, at a path that means something else
        // there.
        let aimed_at = self.current_remote_operation_origin();
        promise::spawn::spawn(async move {
            let planned = promise::spawn::spawn_into_new_thread(move || {
                Ok::<_, anyhow::Error>((source.clone(), plan_transfer(&source)))
            })
            .await;
            let Ok((source, plan)) = planned else {
                return;
            };
            window.notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                term_window
                    .apply_remote_folder_plan(source, directory, plan, aimed_at, anchor, paste_to);
            })));
        })
        .detach();
    }

    fn apply_remote_folder_plan(
        &mut self,
        source: PathBuf,
        directory: RemotePath,
        plan: Result<
            crate::termwindow::transfer_walk::TransferPlan,
            crate::termwindow::transfer_walk::TransferPlanError,
        >,
        aimed_at: Option<RemoteOperationOrigin>,
        anchor: Point,
        paste_to: Option<PaneId>,
    ) {
        let name = display_name(&source);
        let Some(origin) = aimed_at else {
            self.push_remote_transfer_failure(
                RemoteTransferKind::Upload,
                name,
                "The Remote Files connection is no longer available".to_string(),
                None,
            );
            return;
        };
        if !self.remote_operation_origin_matches(&origin) {
            // Refuse rather than retarget: the same remote path on a different
            // host is a different place entirely.
            self.push_remote_transfer_failure(
                RemoteTransferKind::Upload,
                name,
                "The panel moved to another host while this folder was being read".to_string(),
                None,
            );
            return;
        }
        let plan = match plan {
            Ok(plan) => plan,
            Err(err) => {
                self.push_remote_transfer_failure(
                    RemoteTransferKind::Upload,
                    name,
                    err.message(),
                    None,
                );
                return;
            }
        };
        if plan.entries.is_empty() {
            self.push_remote_transfer_failure(
                RemoteTransferKind::Upload,
                name,
                "Nothing to upload".to_string(),
                None,
            );
            return;
        }

        // Where the uploaded folder itself will live — the path a terminal
        // drop wants pasted once the tree has landed.
        let paste_target = paste_to.and_then(|pane_id| {
            directory
                .join_name(&name)
                .ok()
                .map(|remote| (pane_id, remote))
        });

        // The row exists from here whichever way the size question goes, so
        // the confirmation has something on screen to be about.
        let id = self.next_remote_transfer_id();
        let progress = RemoteTransferProgress::default();
        self.right_sidebar_remote_transfers.push(RemoteTransfer {
            id,
            kind: RemoteTransferKind::Upload,
            name,
            progress,
            status: RemoteTransferStatus::Running,
            source: None,
            origin: Some(origin.clone()),
        });
        self.trim_remote_transfers();

        // Every remote entry costs at least one round trip on a single
        // serialized session, so a big tree is slow in a way the user cannot
        // see coming. Ask, with the number on the table, instead of either
        // refusing outright or appearing to hang.
        if plan.entries.len() > TRANSFER_CONFIRM_THRESHOLD {
            self.set_pending_remote_confirm(
                PendingRemoteConfirm::FolderUpload {
                    transfer_id: id,
                    directory,
                    plan,
                    origin,
                    paste_target,
                },
                anchor,
            );
            self.invalidate_window();
            return;
        }
        self.start_remote_folder_upload_execution(id, directory, plan, paste_target);
        self.invalidate_window();
    }

    /// The planned (and, if it was big, confirmed) folder upload.
    fn start_remote_folder_upload_execution(
        &mut self,
        id: u64,
        directory: RemotePath,
        plan: crate::termwindow::transfer_walk::TransferPlan,
        paste_target: Option<(PaneId, RemotePath)>,
    ) {
        let Some(transfer) = self
            .right_sidebar_remote_transfers
            .iter()
            .find(|transfer| transfer.id == id)
        else {
            return;
        };
        let progress = transfer.progress.clone();
        let Some(origin) = transfer.origin.clone() else {
            self.finish_remote_transfer(
                id,
                RemoteTransferStatus::Failed(
                    "Remote transfer lost its source identity".to_string(),
                ),
            );
            return;
        };
        if !self.remote_operation_origin_matches(&origin) {
            self.finish_remote_transfer(
                id,
                RemoteTransferStatus::Failed(
                    "The panel moved to another host before the upload started".to_string(),
                ),
            );
            return;
        }
        if progress.is_canceled() {
            self.finish_remote_transfer(
                id,
                RemoteTransferStatus::Failed(REMOTE_TRANSFER_CANCELED.to_string()),
            );
            return;
        }
        let Some((backend, operation_lease, connection_key, connection_id)) =
            self.remote_transfer_handles(&origin)
        else {
            self.finish_remote_transfer(
                id,
                RemoteTransferStatus::Failed(
                    "Remote Files connection is no longer available".to_string(),
                ),
            );
            return;
        };
        let Some(window) = self.window.as_ref().cloned() else {
            return;
        };
        progress.set_item_total(plan.entries.len() as u64);

        // Symlinks and unreadable nodes were deliberately left out; saying so
        // is the difference between a truthful success and one that quietly
        // omitted data.
        let note = {
            let mut parts = Vec::new();
            if plan.skipped_symlinks > 0 {
                parts.push(format!("{} symlink(s) skipped", plan.skipped_symlinks));
            }
            if plan.unreadable > 0 {
                parts.push(format!("{} unreadable item(s)", plan.unreadable));
            }
            (!parts.is_empty()).then(|| parts.join(", "))
        };
        let entries = plan.entries;
        let refresh_dir = directory.clone();
        promise::spawn::spawn(async move {
            let result = upload_tree(&*backend, &directory, &entries, &progress).await;
            drop(operation_lease);
            let connection_died = result.as_ref().is_err_and(|failure| {
                invalidate_remote_connection_if_dead(
                    &connection_key,
                    connection_id,
                    &failure.message,
                )
            });
            window.notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                let done_detail = match &note {
                    Some(note) => format!("Uploaded ({note})"),
                    None => "Uploaded".to_string(),
                };
                let succeeded = result.is_ok();
                let status = transfer_status_from_result(&result, &done_detail);
                term_window.finish_remote_transfer(id, status);
                if succeeded {
                    if let Some((pane_id, folder_remote)) = paste_target {
                        term_window.paste_remote_path_to_pane(pane_id, &folder_remote);
                    }
                }
                // Re-list either way: a partial upload leaves real files the
                // tree would otherwise never show.
                if term_window.remote_operation_origin_matches(&origin) {
                    let effects = term_window
                        .right_sidebar_remote_files
                        .transition(RemoteFilesEvent::DirectoryInvalidated(refresh_dir));
                    term_window.apply_right_sidebar_remote_files_effects(effects);
                }
                if connection_died {
                    term_window
                        .release_remote_files_lease_if_connection(&connection_key, connection_id);
                }
                term_window.invalidate_window();
            })));
        })
        .detach();
    }

    fn next_remote_transfer_id(&mut self) -> u64 {
        self.right_sidebar_remote_transfer_next_id = self
            .right_sidebar_remote_transfer_next_id
            .wrapping_add(1)
            .max(1);
        self.right_sidebar_remote_transfer_next_id
    }

    /// A transfer that never got as far as starting. `source` is `Some` only
    /// when retrying could plausibly do better — a rejected folder or an
    /// unusable name has nothing to retry.
    fn push_remote_transfer_failure(
        &mut self,
        kind: RemoteTransferKind,
        name: String,
        message: String,
        source: Option<RemoteTransferSource>,
    ) {
        let id = self.next_remote_transfer_id();
        let origin = source
            .as_ref()
            .and_then(|_| self.current_remote_operation_origin());
        self.right_sidebar_remote_transfers.push(RemoteTransfer {
            id,
            kind,
            name,
            progress: RemoteTransferProgress::default(),
            status: RemoteTransferStatus::Failed(message),
            source,
            origin,
        });
        self.trim_remote_transfers();
    }

    /// Keep the strip from growing without bound: finished entries are the
    /// only ones ever dropped, and the oldest go first.
    fn trim_remote_transfers(&mut self) {
        while self.right_sidebar_remote_transfers.len() > REMOTE_TRANSFER_STRIP_MAX {
            let Some(index) = self
                .right_sidebar_remote_transfers
                .iter()
                .position(|transfer| !transfer.is_running())
            else {
                break;
            };
            self.right_sidebar_remote_transfers.remove(index);
        }
    }

    fn start_remote_upload(
        &mut self,
        local: PathBuf,
        remote: RemotePath,
        directory: RemotePath,
        paste_to: Option<PaneId>,
    ) {
        let name = remote.file_name().to_string();
        let Some(origin) = self.current_remote_operation_origin() else {
            self.push_remote_transfer_failure(
                RemoteTransferKind::Upload,
                name,
                "Remote Files source is no longer available".to_string(),
                None,
            );
            return;
        };
        let retry = RemoteTransferSource::Upload {
            local: local.clone(),
            remote: remote.clone(),
            directory: directory.clone(),
        };
        let Some((backend, operation_lease, connection_key, connection_id)) =
            self.remote_transfer_handles(&origin)
        else {
            // A missing lease is exactly the kind of failure retrying fixes,
            // once the panel has reconnected.
            self.push_remote_transfer_failure(
                RemoteTransferKind::Upload,
                name,
                "Remote Files connection is no longer available".to_string(),
                Some(retry),
            );
            return;
        };
        let Some(window) = self.window.as_ref().cloned() else {
            return;
        };

        let id = self.next_remote_transfer_id();
        let progress = RemoteTransferProgress::default();
        self.right_sidebar_remote_transfers.push(RemoteTransfer {
            id,
            kind: RemoteTransferKind::Upload,
            name,
            progress: progress.clone(),
            status: RemoteTransferStatus::Running,
            source: Some(retry),
            origin: Some(origin.clone()),
        });
        self.trim_remote_transfers();

        promise::spawn::spawn(async move {
            // A single dropped file refuses to clobber; there is no prompt on
            // this path, so silently replacing would be a choice the user
            // never made.
            let result = backend
                .upload_file(local, remote.clone(), progress, false)
                .await;
            drop(operation_lease);
            let connection_died = result.as_ref().is_err_and(|failure| {
                invalidate_remote_connection_if_dead(
                    &connection_key,
                    connection_id,
                    &failure.message,
                )
            });
            window.notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                let succeeded = result.is_ok();
                let status = transfer_status_from_result(&result, "Uploaded");
                term_window.finish_remote_transfer(id, status);
                if succeeded {
                    if let Some(pane_id) = paste_to {
                        // A drop on a remote terminal wants the file's REMOTE
                        // path on the command line, exactly where a local drop
                        // would have pasted the local one.
                        term_window.paste_remote_path_to_pane(pane_id, &remote);
                    }
                }
                // Re-list either way. On failure the directory may now hold a
                // partial file the tree would otherwise never show — and the
                // user would then hit "already exists" on retry with nothing
                // on screen to explain it.
                if term_window.remote_operation_origin_matches(&origin) {
                    let effects = term_window
                        .right_sidebar_remote_files
                        .transition(RemoteFilesEvent::DirectoryInvalidated(directory));
                    term_window.apply_right_sidebar_remote_files_effects(effects);
                }
                if !succeeded && connection_died {
                    term_window
                        .release_remote_files_lease_if_connection(&connection_key, connection_id);
                }
                term_window.invalidate_window();
            })));
        })
        .detach();
    }

    /// Right-clicking a remote row: the file-operations menu, shaped by what
    /// the row is. The root gets only what cannot orphan the whole panel —
    /// no Rename, no Delete.
    pub(crate) fn show_right_sidebar_remote_file_context_menu(
        &mut self,
        context: &dyn WindowOps,
        anchor: Point,
        path: RemotePath,
    ) {
        // A search result can sit in a folder the tree never loaded.
        let Some(kind) = self
            .right_sidebar_remote_files
            .kind_for_path(&path)
            .or_else(|| self.right_sidebar_remote_file_search.kind_for_path(&path))
        else {
            return;
        };
        let Some(origin) = self.current_remote_operation_origin() else {
            return;
        };
        let is_root = self.right_sidebar_remote_files.root.as_ref() == Some(&path);
        // Every operation needs a live lease; offering one without would only
        // produce a failure row.
        let enabled = self.right_sidebar_remote_files_lease.is_some();
        let path_string = path.as_str().to_string();

        // One pass: beginning the block clears the action table, so a second
        // call mid-assembly would turn the earlier items into dead entries.
        self.begin_context_menu_application_actions();
        let mut items = Vec::new();
        match kind {
            RemoteFileKind::File => {
                items.push(self.context_menu_application_item_with_icon(
                    crate::i18n::tr("right-download"),
                    // No dedicated download glyph in the shared icon set; Save
                    // is the closest fit and already maps on every platform.
                    ContextMenuIcon::Save,
                    crate::termwindow::ContextMenuApplicationAction::DownloadRemoteFile {
                        path: path.clone(),
                        origin: origin.clone(),
                    },
                    enabled,
                ));
            }
            RemoteFileKind::Directory => {
                items.push(self.context_menu_application_item_with_icon(
                    crate::i18n::tr("right-download"),
                    ContextMenuIcon::Save,
                    crate::termwindow::ContextMenuApplicationAction::DownloadRemoteFolder {
                        path: path.clone(),
                        anchor,
                        origin: origin.clone(),
                    },
                    enabled,
                ));
                items.push(self.context_menu_application_item_with_icon(
                    crate::i18n::tr("right-new-folder"),
                    ContextMenuIcon::FolderAdd,
                    crate::termwindow::ContextMenuApplicationAction::NewRemoteFolder {
                        parent: path.clone(),
                        origin: origin.clone(),
                    },
                    enabled,
                ));
            }
            // A link's target could be anywhere; downloading "it" would really
            // download something else. Rename and Delete below still apply.
            RemoteFileKind::Symlink | RemoteFileKind::Other => {}
        }
        items.push(ContextMenuItem::item_with_icon(
            crate::i18n::tr("right-copy-path"),
            ContextMenuIcon::Copy,
            KeyAssignment::CopyFilePathToClipboard(path_string),
        ));
        if !is_root {
            items.push(self.context_menu_application_item_with_icon(
                crate::i18n::tr("right-rename"),
                ContextMenuIcon::Edit,
                crate::termwindow::ContextMenuApplicationAction::RenameRemoteEntry {
                    path: path.clone(),
                    origin: origin.clone(),
                },
                enabled,
            ));
            items.push(ContextMenuItem::Separator);
            items.push(self.context_menu_application_item_with_icon(
                crate::i18n::tr("right-delete"),
                ContextMenuIcon::Delete,
                crate::termwindow::ContextMenuApplicationAction::DeleteRemoteEntry {
                    path,
                    anchor,
                    origin,
                },
                enabled,
            ));
        }
        self.show_term_context_menu(context, anchor, items);
    }

    /// Which local directory a pointer at `(x, y)` is aiming at.
    pub(crate) fn local_drop_target_at(&self, x: isize, y: isize) -> Option<PathBuf> {
        if !self.right_sidebar_file_view_active()
            || self.right_sidebar_remote_files.target.is_some()
        {
            return None;
        }
        let root = self.active_local_project_for_files().ok()?.path;
        // Look the row up in what was actually painted rather than asking the
        // filesystem: a syscall per drag event on the GUI thread is wasteful,
        // and the index's children map has no key for empty directories.
        let rows = if self.right_sidebar_file_search_rows.is_empty() {
            &self.right_sidebar_file_browse_rows
        } else {
            &self.right_sidebar_file_search_rows
        };
        resolve_local_drop_target(&self.ui_items, x, y, &root, |path| {
            if path == root {
                return Some(true);
            }
            rows.iter()
                .find(|row| row.path == path)
                .map(|row| row.is_dir)
        })
    }

    /// Copy files or folders dropped from the OS into the local Files panel.
    /// Returns false when the drop was not aimed here.
    pub(crate) fn copy_dropped_files_into_local_panel(
        &mut self,
        paths: &[PathBuf],
        coords: Option<Point>,
    ) -> bool {
        self.clear_right_sidebar_local_drop_target();
        let Some(coords) = coords else {
            return false;
        };
        let Some(directory) = self.local_drop_target_at(coords.x, coords.y) else {
            return false;
        };
        // A new drop supersedes any earlier one still waiting on an answer.
        // The fallback menu clears it on dismissal, but macOS hands the menu
        // to the system and reports only chosen actions, so a dismissed native
        // prompt has no other way to release its plan.
        self.cancel_pending_local_copy();
        self.local_copy_generation = self.local_copy_generation.wrapping_add(1);
        let generation = self.local_copy_generation;
        self.begin_local_copy(paths.to_vec(), directory, coords, generation);
        true
    }

    /// Plan the drop on a worker, then come back to ask about conflicts.
    ///
    /// The walk and its `metadata` calls must not run on the GUI thread: a
    /// large or network-backed tree would freeze the window for as long as it
    /// takes, before even the progress row appears.
    fn begin_local_copy(
        &mut self,
        sources: Vec<PathBuf>,
        directory: PathBuf,
        anchor: Point,
        generation: u64,
    ) {
        let Some(window) = self.window.as_ref().cloned() else {
            return;
        };
        let plan_dir = directory.clone();
        promise::spawn::spawn(async move {
            let preflight = promise::spawn::spawn_into_new_thread(move || {
                Ok::<_, anyhow::Error>(preflight_local_copy(&sources, &plan_dir))
            })
            .await;
            let Ok(preflight) = preflight else {
                return;
            };
            window.notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                term_window.apply_local_copy_preflight(preflight, directory, anchor, generation);
            })));
        })
        .detach();
    }

    fn apply_local_copy_preflight(
        &mut self,
        preflight: crate::termwindow::transfer_walk::TransferPreflight,
        directory: PathBuf,
        anchor: Point,
        generation: u64,
    ) {
        // A slower walk from an earlier drop must not replace a newer prompt.
        if generation != self.local_copy_generation {
            return;
        }
        for (name, reason) in &preflight.rejected {
            self.push_remote_transfer_failure(
                RemoteTransferKind::LocalCopy,
                name.clone(),
                reason.clone(),
                None,
            );
        }
        if preflight.plans.is_empty() {
            self.invalidate_window();
            return;
        }
        let note = preflight.omission_note();
        if preflight.conflicts.is_empty() {
            // Nothing was in the way when we looked, so nothing may be
            // replaced: anything there now arrived since, unauthorized.
            self.run_local_copy(preflight.plans, directory, OverwritePolicy::NoClobber, note);
        } else {
            // Show first, then record: opening a menu closes any previous
            // one, and that teardown is where an unanswered plan is dropped —
            // so storing the plan beforehand would have it clear itself.
            let count = preflight.conflicts.len();
            if let Some(window) = self.window.as_ref().cloned() {
                self.show_local_copy_conflict_menu(&window, anchor, count);
            }
            self.pending_local_copy_conflict_count = count;
            self.pending_local_copy = Some(PendingLocalCopy {
                plans: preflight.plans,
                directory,
                conflicts: preflight.conflicts,
                note,
            });
        }
        self.invalidate_window();
    }
    pub(crate) fn show_local_copy_conflict_menu(
        &mut self,
        context: &dyn WindowOps,
        anchor: Point,
        count: usize,
    ) {
        // The count goes into each label rather than a heading row: every
        // option then says exactly what it will do, and there is no inert row
        // for the user to try clicking.
        // One pass: beginning the block clears the action table, so minting an
        // item after a second call would leave the earlier ones dead.
        self.begin_context_menu_application_actions();
        let overwrite = self.context_menu_application_item_with_icon(
            right_sidebar_arg("right-replace-conflicts", "count", count.to_string()),
            ContextMenuIcon::Save,
            crate::termwindow::ContextMenuApplicationAction::ResolveLocalCopyConflict(
                ConflictChoice::Overwrite,
            ),
            true,
        );
        let skip = self.context_menu_application_item_with_icon(
            right_sidebar_arg("right-skip-conflicts", "count", count.to_string()),
            ContextMenuIcon::Check,
            crate::termwindow::ContextMenuApplicationAction::ResolveLocalCopyConflict(
                ConflictChoice::Skip,
            ),
            true,
        );
        let cancel = self.context_menu_application_item_with_icon(
            crate::i18n::tr("right-cancel-copy"),
            ContextMenuIcon::Close,
            crate::termwindow::ContextMenuApplicationAction::ResolveLocalCopyConflict(
                ConflictChoice::Cancel,
            ),
            true,
        );
        self.show_term_context_menu(context, anchor, vec![overwrite, skip, cancel]);
    }

    /// Resolve a queued copy once the user has answered the conflict prompt.
    pub(crate) fn resolve_pending_local_copy(&mut self, choice: ConflictChoice) {
        let Some(pending) = self.pending_local_copy.take() else {
            return;
        };
        let policy = match choice {
            ConflictChoice::Cancel => {
                self.invalidate_window();
                return;
            }
            ConflictChoice::Overwrite => OverwritePolicy::Replace,
            ConflictChoice::Skip => OverwritePolicy::SkipExisting,
        };
        let PendingLocalCopy {
            plans,
            directory,
            conflicts,
            note,
        } = pending;
        let plans = plans
            .into_iter()
            .map(|(source, plan)| (source, apply_conflict_choice(plan, &conflicts, choice)))
            .collect();
        self.run_local_copy(plans, directory, policy, note);
    }

    /// Drop a queued copy that was never answered.
    ///
    /// A context menu can be dismissed without choosing anything; leaving the
    /// plan behind means the next, unrelated drop would silently reopen the
    /// old prompt at the new location.
    pub(crate) fn cancel_pending_local_copy(&mut self) {
        if self.pending_local_copy.take().is_some() {
            self.invalidate_window();
        }
    }

    /// Run the whole drop in ONE worker.
    ///
    /// One thread for the batch, not one per dropped path: a multi-select of a
    /// few hundred files would otherwise spawn a few hundred threads, all
    /// fighting for the same disk.
    fn run_local_copy(
        &mut self,
        plans: Vec<(PathBuf, crate::termwindow::transfer_walk::TransferPlan)>,
        directory: PathBuf,
        policy: OverwritePolicy,
        note: Option<String>,
    ) {
        let Some(window) = self.window.as_ref().cloned() else {
            return;
        };
        let entries: Vec<_> = plans
            .into_iter()
            .flat_map(|(_, plan)| plan.entries.into_iter())
            .collect();
        if entries.is_empty() {
            return;
        }

        let id = self.next_remote_transfer_id();
        let progress = RemoteTransferProgress::default();
        progress.set_item_total(entries.len() as u64);
        self.right_sidebar_remote_transfers.push(RemoteTransfer {
            id,
            kind: RemoteTransferKind::LocalCopy,
            name: display_name(&directory),
            progress: progress.clone(),
            status: RemoteTransferStatus::Running,
            source: None,
            origin: None,
        });
        self.trim_remote_transfers();
        self.invalidate_window();

        promise::spawn::spawn(async move {
            let result = promise::spawn::spawn_into_new_thread(move || {
                // Pin the destination once for the whole batch. Re-opening it
                // per entry would leave the root's own ancestors free to be
                // swapped between operations.
                let root = match DestinationRoot::open(&directory) {
                    Ok(root) => root,
                    Err(err) => {
                        return Ok::<_, anyhow::Error>(Err(format!(
                            "Unable to use {}: {err}",
                            directory.display()
                        )))
                    }
                };
                Ok(copy_entries_blocking(&entries, &root, policy, &progress))
            })
            .await
            .unwrap_or_else(|err| Err(format!("Copy failed: {err}")));

            window.notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                let status = match result {
                    Ok(copied) => {
                        let mut detail = format!("Copied {copied} items");
                        if let Some(note) = note {
                            detail.push_str(&format!(" ({note})"));
                        }
                        RemoteTransferStatus::Done(detail)
                    }
                    Err(message) => RemoteTransferStatus::Failed(message),
                };
                term_window.finish_remote_transfer(id, status);
                term_window.force_right_sidebar_file_rescan_soon();
                term_window.invalidate_window();
            })));
        })
        .detach();
    }

    /// Right-clicking inside a *remote* file preview. The local preview offers
    /// "Open With", which is meaningless for a file that is not on this disk —
    /// so rather than the previous bail-out (right-click simply did nothing on
    /// a remote preview), give it a menu of things that do apply.
    pub(crate) fn show_right_sidebar_remote_file_preview_context_menu(
        &mut self,
        context: &dyn WindowOps,
        anchor: Point,
    ) {
        let Some(path) = self.right_sidebar_remote_files.selected.clone() else {
            return;
        };
        let Some(origin) = self.current_remote_operation_origin() else {
            return;
        };
        // Snapshot the gating up front: the item builders below take `&mut
        // self`, so reading these inline would fight the borrow checker.
        let has_selection = self.right_sidebar_file_preview_selected_text().is_some();
        let has_text = self.right_sidebar_file_preview_image.is_none()
            && !self.right_sidebar_file_preview_lines.is_empty();
        let can_download = self.right_sidebar_remote_files_lease.is_some();
        let path_string = path.as_str().to_string();

        // Every application action has to be minted in one pass: beginning the
        // block clears the whole table, so a second call mid-assembly would
        // turn the earlier items into dead entries.
        self.begin_context_menu_application_actions();
        let copy_selection = self.context_menu_application_item_with_icon(
            crate::i18n::tr("menu-copy"),
            ContextMenuIcon::Copy,
            crate::termwindow::ContextMenuApplicationAction::CopyRemotePreviewSelection,
            has_selection,
        );
        let copy_all = self.context_menu_application_item_with_icon(
            crate::i18n::tr("right-copy-all"),
            ContextMenuIcon::Copy,
            crate::termwindow::ContextMenuApplicationAction::CopyRemotePreviewAll,
            has_text,
        );
        let download = self.context_menu_application_item_with_icon(
            crate::i18n::tr("right-download"),
            ContextMenuIcon::Save,
            crate::termwindow::ContextMenuApplicationAction::DownloadRemoteFile { path, origin },
            can_download,
        );

        let items = vec![
            copy_selection,
            copy_all,
            ContextMenuItem::item_with_icon(
                "Copy Path",
                ContextMenuIcon::Copy,
                KeyAssignment::CopyFilePathToClipboard(path_string),
            ),
            ContextMenuItem::Separator,
            download,
        ];
        self.show_term_context_menu(context, anchor, items);
    }

    pub(crate) fn download_right_sidebar_remote_file(&mut self, remote: RemotePath) {
        let name = remote.file_name().to_string();
        let Some(origin) = self.current_remote_operation_origin() else {
            self.push_remote_transfer_failure(
                RemoteTransferKind::Download,
                name,
                "Remote Files source is no longer available".to_string(),
                None,
            );
            return;
        };
        let retry = RemoteTransferSource::Download {
            remote: remote.clone(),
        };
        let Some(directory) = crate::native_settings::effective_remote_download_directory() else {
            // No Downloads folder is an environment problem, not a transient
            // one; retrying would fail the same way.
            self.push_remote_transfer_failure(
                RemoteTransferKind::Download,
                name,
                "Unable to locate a Downloads folder".to_string(),
                None,
            );
            return;
        };
        if let Err(err) = fs::create_dir_all(&directory) {
            self.push_remote_transfer_failure(
                RemoteTransferKind::Download,
                name,
                format!("Unable to use {}: {err}", directory.display()),
                Some(retry),
            );
            return;
        }
        // Claim the destination and its staging file together, by creating the
        // staging file exclusively. Two downloads that would land on the same
        // name therefore take different ones, and an unrelated `X.part` that
        // happens to be sitting there is never truncated or deleted.
        let Some((local, partial)) =
            reserve_download_path(&directory, &name, local_path_is_occupied, |partial| {
                fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(partial)
                    .is_ok()
            })
        else {
            self.push_remote_transfer_failure(
                RemoteTransferKind::Download,
                name,
                format!("Unable to find a free name in {}", directory.display()),
                Some(retry),
            );
            return;
        };

        // From here on the staging file exists, so every early return has to
        // take it back down.
        let release_reservation = |partial: &Path| {
            if let Err(err) = fs::remove_file(partial) {
                log::warn!(
                    "remote files: unable to release the download reservation {}: {err:#}",
                    partial.display()
                );
            }
        };

        let Some((backend, operation_lease, connection_key, connection_id)) =
            self.remote_transfer_handles(&origin)
        else {
            release_reservation(&partial);
            self.push_remote_transfer_failure(
                RemoteTransferKind::Download,
                name,
                "Remote Files connection is no longer available".to_string(),
                Some(retry),
            );
            return;
        };
        let Some(window) = self.window.as_ref().cloned() else {
            release_reservation(&partial);
            return;
        };

        let id = self.next_remote_transfer_id();
        let progress = RemoteTransferProgress::default();
        self.right_sidebar_remote_transfers.push(RemoteTransfer {
            id,
            kind: RemoteTransferKind::Download,
            name,
            progress: progress.clone(),
            status: RemoteTransferStatus::Running,
            source: Some(retry),
            origin: Some(origin),
        });
        self.trim_remote_transfers();
        self.invalidate_window();

        promise::spawn::spawn(async move {
            let landed = local.clone();
            let result = backend.download_file(remote, local, progress).await;
            drop(operation_lease);
            let connection_died = result.as_ref().is_err_and(|failure| {
                invalidate_remote_connection_if_dead(
                    &connection_key,
                    connection_id,
                    &failure.message,
                )
            });
            window.notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                let landed_in = landed
                    .parent()
                    .map(|parent| parent.display().to_string())
                    .unwrap_or_else(|| landed.display().to_string());
                let status = transfer_status_from_result(&result, &format!("Saved to {landed_in}"));
                term_window.finish_remote_transfer(id, status);
                if connection_died {
                    term_window
                        .release_remote_files_lease_if_connection(&connection_key, connection_id);
                }
                term_window.invalidate_window();
            })));
        })
        .detach();
    }

    /// Right-clicked "Download" on a remote directory: walk it first, ask if
    /// it is big, then pull the whole tree into the Downloads folder.
    pub(crate) fn download_right_sidebar_remote_folder(
        &mut self,
        remote: RemotePath,
        anchor: Point,
    ) {
        let name = remote.file_name().to_string();
        let Some(origin) = self.current_remote_operation_origin() else {
            self.push_remote_transfer_failure(
                RemoteTransferKind::Download,
                name,
                "Remote Files source is no longer available".to_string(),
                None,
            );
            return;
        };
        let retry = RemoteTransferSource::DownloadFolder {
            remote: remote.clone(),
        };
        let Some((backend, operation_lease, connection_key, connection_id)) =
            self.remote_transfer_handles(&origin)
        else {
            self.push_remote_transfer_failure(
                RemoteTransferKind::Download,
                name,
                "Remote Files connection is no longer available".to_string(),
                Some(retry),
            );
            return;
        };
        let Some(window) = self.window.as_ref().cloned() else {
            return;
        };

        // The row exists from the walk onwards: listing a big tree takes real
        // time on a serialized channel, and a click on the row is the only
        // way to abort it.
        let id = self.next_remote_transfer_id();
        let progress = RemoteTransferProgress::default();
        self.right_sidebar_remote_transfers.push(RemoteTransfer {
            id,
            kind: RemoteTransferKind::Download,
            name,
            progress: progress.clone(),
            status: RemoteTransferStatus::Running,
            source: Some(retry),
            origin: Some(origin.clone()),
        });
        self.trim_remote_transfers();
        self.invalidate_window();

        // Same guard as folder uploads: the walk is slow, and the panel can
        // be pointed at another host meanwhile — a plan built against one
        // server must never execute against another.
        promise::spawn::spawn(async move {
            let plan =
                plan_remote_walk(&*backend, remote, RemoteWalkMode::Download, &progress).await;
            drop(operation_lease);
            let connection_died = plan.as_ref().is_err_and(|message| {
                invalidate_remote_connection_if_dead(&connection_key, connection_id, message)
            });
            window.notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                term_window.apply_remote_folder_download_plan(
                    id,
                    plan,
                    origin,
                    anchor,
                    connection_key,
                    connection_id,
                    connection_died,
                );
            })));
        })
        .detach();
    }

    fn apply_remote_folder_download_plan(
        &mut self,
        id: u64,
        plan: Result<RemoteWalkPlan, String>,
        origin: RemoteOperationOrigin,
        anchor: Point,
        connection_key: String,
        connection_id: u64,
        connection_died: bool,
    ) {
        if connection_died {
            self.release_remote_files_lease_if_connection(&connection_key, connection_id);
        }
        if !self.remote_operation_origin_matches(&origin) {
            self.finish_remote_transfer(
                id,
                RemoteTransferStatus::Failed(
                    "The panel moved to another host while this folder was being read".to_string(),
                ),
            );
            self.invalidate_window();
            return;
        }
        let plan = match plan {
            Ok(plan) => plan,
            Err(message) => {
                self.finish_remote_transfer(id, RemoteTransferStatus::Failed(message));
                self.invalidate_window();
                return;
            }
        };
        if plan.entries.len() > TRANSFER_CONFIRM_THRESHOLD {
            self.set_pending_remote_confirm(
                PendingRemoteConfirm::FolderDownload {
                    transfer_id: id,
                    plan,
                    origin,
                },
                anchor,
            );
        } else {
            self.start_remote_folder_download_execution(id, plan);
        }
        self.invalidate_window();
    }

    /// The plan is final and (if it was big) confirmed: reserve a fresh
    /// destination folder and stream the tree into it.
    fn start_remote_folder_download_execution(&mut self, id: u64, plan: RemoteWalkPlan) {
        let Some(transfer) = self
            .right_sidebar_remote_transfers
            .iter()
            .find(|transfer| transfer.id == id)
        else {
            return;
        };
        let progress = transfer.progress.clone();
        let folder_name = transfer.name.clone();
        let Some(origin) = transfer.origin.clone() else {
            self.finish_remote_transfer(
                id,
                RemoteTransferStatus::Failed(
                    "Remote transfer lost its source identity".to_string(),
                ),
            );
            return;
        };
        if !self.remote_operation_origin_matches(&origin) {
            self.finish_remote_transfer(
                id,
                RemoteTransferStatus::Failed(
                    "The panel moved to another host before the download started".to_string(),
                ),
            );
            return;
        }
        if progress.is_canceled() {
            self.finish_remote_transfer(
                id,
                RemoteTransferStatus::Failed(REMOTE_TRANSFER_CANCELED.to_string()),
            );
            return;
        }
        let Some(directory) = crate::native_settings::effective_remote_download_directory() else {
            self.finish_remote_transfer(
                id,
                RemoteTransferStatus::Failed("Unable to locate a Downloads folder".to_string()),
            );
            return;
        };
        let Some((backend, operation_lease, connection_key, connection_id)) =
            self.remote_transfer_handles(&origin)
        else {
            self.finish_remote_transfer(
                id,
                RemoteTransferStatus::Failed(
                    "Remote Files connection is no longer available".to_string(),
                ),
            );
            return;
        };
        let Some(window) = self.window.as_ref().cloned() else {
            return;
        };

        progress.set_item_total(plan.entries.len() as u64);
        let note = (plan.skipped > 0).then(|| format!("{} symlink(s) skipped", plan.skipped));
        promise::spawn::spawn(async move {
            let result = download_remote_tree(
                &*backend,
                &directory,
                &folder_name,
                &plan.entries,
                &progress,
            )
            .await;
            drop(operation_lease);
            let connection_died = result.as_ref().is_err_and(|failure| {
                invalidate_remote_connection_if_dead(
                    &connection_key,
                    connection_id,
                    &failure.message,
                )
            });
            window.notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                let status = match &result {
                    Ok(destination) => {
                        let mut detail = format!("Saved to {}", destination.display());
                        if let Some(note) = &note {
                            detail.push_str(&format!(" ({note})"));
                        }
                        RemoteTransferStatus::Done(detail)
                    }
                    Err(failure) => match &failure.leftover {
                        Some(leftover) => RemoteTransferStatus::FailedWithLeftover {
                            message: failure.message.clone(),
                            leftover: leftover.clone(),
                        },
                        None => RemoteTransferStatus::Failed(failure.message.clone()),
                    },
                };
                term_window.finish_remote_transfer(id, status);
                if connection_died {
                    term_window
                        .release_remote_files_lease_if_connection(&connection_key, connection_id);
                }
                term_window.invalidate_window();
            })));
        })
        .detach();
    }

    /// Put a confirmation menu on screen and remember what it is asking
    /// about. Show first, then record — opening a menu tears the previous one
    /// down, and that teardown is where an unanswered pending is cancelled,
    /// so recording first would have it cancel itself (the same ordering
    /// [`Self::apply_local_copy_preflight`] relies on).
    fn set_pending_remote_confirm(&mut self, pending: PendingRemoteConfirm, anchor: Point) {
        self.cancel_pending_remote_confirm();
        let Some(window) = self.window.as_ref().cloned() else {
            self.finish_pending_remote_confirm_row(&pending);
            return;
        };
        let (proceed_label, proceed_icon) = match &pending {
            PendingRemoteConfirm::FolderDownload { plan, .. } => (
                right_sidebar_arg(
                    "right-confirm-download",
                    "count",
                    plan.entries.len().to_string(),
                ),
                ContextMenuIcon::Save,
            ),
            PendingRemoteConfirm::FolderDelete { plan, .. } => (
                // +1: the folder itself goes too.
                right_sidebar_arg(
                    "right-confirm-delete-items",
                    "count",
                    (plan.entries.len() + 1).to_string(),
                ),
                ContextMenuIcon::Delete,
            ),
            PendingRemoteConfirm::FileDelete { remote, .. } => (
                right_sidebar_arg(
                    "right-confirm-delete-name",
                    "name",
                    remote.file_name().to_string(),
                ),
                ContextMenuIcon::Delete,
            ),
            PendingRemoteConfirm::FolderUpload { plan, .. } => (
                right_sidebar_arg(
                    "right-confirm-upload",
                    "count",
                    plan.entries.len().to_string(),
                ),
                ContextMenuIcon::Save,
            ),
        };
        self.begin_context_menu_application_actions();
        let proceed = self.context_menu_application_item_with_icon(
            proceed_label,
            proceed_icon,
            crate::termwindow::ContextMenuApplicationAction::ResolveRemoteConfirm(true),
            true,
        );
        let cancel = self.context_menu_application_item_with_icon(
            crate::i18n::tr("right-cancel"),
            ContextMenuIcon::Close,
            crate::termwindow::ContextMenuApplicationAction::ResolveRemoteConfirm(false),
            true,
        );
        self.show_term_context_menu(&window, anchor, vec![proceed, cancel]);
        self.pending_remote_confirm = Some(pending);
    }

    /// Drop a confirmation nobody answered — its menu was dismissed, or a
    /// newer confirmation is taking the slot.
    pub(crate) fn cancel_pending_remote_confirm(&mut self) {
        if let Some(pending) = self.pending_remote_confirm.take() {
            self.finish_pending_remote_confirm_row(&pending);
            self.invalidate_window();
        }
    }

    /// A pending confirmation that will never run still owns a Running
    /// transfer row; leave it saying "Canceled" rather than spinning forever.
    fn finish_pending_remote_confirm_row(&mut self, pending: &PendingRemoteConfirm) {
        let transfer_id = match pending {
            PendingRemoteConfirm::FolderDownload { transfer_id, .. }
            | PendingRemoteConfirm::FolderDelete { transfer_id, .. }
            | PendingRemoteConfirm::FolderUpload { transfer_id, .. } => Some(*transfer_id),
            PendingRemoteConfirm::FileDelete { .. } => None,
        };
        if let Some(id) = transfer_id {
            self.finish_remote_transfer(
                id,
                RemoteTransferStatus::Failed(REMOTE_TRANSFER_CANCELED.to_string()),
            );
        }
    }

    pub(crate) fn resolve_pending_remote_confirm(&mut self, proceed: bool) {
        let Some(pending) = self.pending_remote_confirm.take() else {
            return;
        };
        if !proceed {
            self.finish_pending_remote_confirm_row(&pending);
            self.invalidate_window();
            return;
        }
        match pending {
            PendingRemoteConfirm::FolderDownload {
                transfer_id,
                plan,
                origin,
            } => {
                if !self.remote_operation_origin_matches(&origin) {
                    self.finish_remote_transfer(
                        transfer_id,
                        RemoteTransferStatus::Failed(
                            "The panel moved to another host while waiting".to_string(),
                        ),
                    );
                } else {
                    self.start_remote_folder_download_execution(transfer_id, plan);
                }
            }
            PendingRemoteConfirm::FolderDelete {
                transfer_id,
                remote,
                plan,
                origin,
            } => {
                if !self.remote_operation_origin_matches(&origin) {
                    self.finish_remote_transfer(
                        transfer_id,
                        RemoteTransferStatus::Failed(
                            "The panel moved to another host while waiting".to_string(),
                        ),
                    );
                } else {
                    self.start_remote_folder_delete_execution(transfer_id, remote, plan);
                }
            }
            PendingRemoteConfirm::FileDelete { remote, origin } => {
                if self.remote_operation_origin_matches(&origin) {
                    self.execute_remote_file_delete(remote, origin);
                }
            }
            PendingRemoteConfirm::FolderUpload {
                transfer_id,
                directory,
                plan,
                origin,
                paste_target,
            } => {
                if !self.remote_operation_origin_matches(&origin) {
                    self.finish_remote_transfer(
                        transfer_id,
                        RemoteTransferStatus::Failed(
                            "The panel moved to another host while waiting".to_string(),
                        ),
                    );
                } else {
                    self.start_remote_folder_upload_execution(
                        transfer_id,
                        directory,
                        plan,
                        paste_target,
                    );
                }
            }
        }
        self.invalidate_window();
    }

    /// Right-clicked "Delete…" on a remote row. A directory is walked first so
    /// the confirmation can say how much it is really about to remove; a file
    /// (or link) goes straight to its confirmation. Everything here confirms —
    /// there is no trash on the far side to undo from.
    pub(crate) fn delete_right_sidebar_remote_entry(&mut self, path: RemotePath, anchor: Point) {
        match self.right_sidebar_remote_files.kind_for_path(&path) {
            Some(RemoteFileKind::Directory) => self.begin_remote_folder_delete(path, anchor),
            Some(_) => {
                if let Some(origin) = self.current_remote_operation_origin() {
                    self.set_pending_remote_confirm(
                        PendingRemoteConfirm::FileDelete {
                            remote: path,
                            origin,
                        },
                        anchor,
                    );
                }
            }
            None => {}
        }
    }

    /// The confirmed single-file delete. No transfer row: it is one round
    /// trip, and a failure reports through the panel's notice line.
    fn execute_remote_file_delete(&mut self, remote: RemotePath, origin: RemoteOperationOrigin) {
        if !self.remote_operation_origin_matches(&origin) {
            return;
        }
        let Some((backend, operation_lease, connection_key, connection_id)) =
            self.remote_transfer_handles(&origin)
        else {
            self.right_sidebar_remote_files.error_message =
                Some("Remote Files connection is no longer available".to_string());
            self.invalidate_window();
            return;
        };
        let Some(window) = self.window.as_ref().cloned() else {
            return;
        };
        promise::spawn::spawn(async move {
            let result = backend.remove_file(remote.clone()).await;
            drop(operation_lease);
            let connection_died = result.as_ref().is_err_and(|message| {
                invalidate_remote_connection_if_dead(&connection_key, connection_id, message)
            });
            window.notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                term_window.finish_remote_entry_removal(
                    remote,
                    result,
                    origin,
                    connection_key,
                    connection_id,
                    connection_died,
                );
            })));
        })
        .detach();
    }

    /// Shared tail of a delete or rename: the entry no longer exists under
    /// its old path, so forget its subtree, close a preview that was showing
    /// it, and re-list the directory it lived in.
    pub(crate) fn finish_remote_entry_removal(
        &mut self,
        remote: RemotePath,
        result: Result<(), String>,
        origin: RemoteOperationOrigin,
        connection_key: String,
        connection_id: u64,
        connection_died: bool,
    ) {
        if connection_died {
            self.release_remote_files_lease_if_connection(&connection_key, connection_id);
        }
        if !self.remote_operation_origin_matches(&origin) {
            // A different tree is on screen now; neither the error nor the
            // refresh belongs to it.
            return;
        }
        match result {
            Ok(()) => self.forget_remote_entry_and_refresh_parent(&remote),
            Err(message) => {
                self.right_sidebar_remote_files.error_message = Some(message);
            }
        }
        self.invalidate_window();
    }

    /// Drop `remote`'s cached subtree, close a preview living under it, and
    /// re-list its parent so the tree reflects what the server now holds.
    fn forget_remote_entry_and_refresh_parent(&mut self, remote: &RemotePath) {
        // Decided BEFORE the transition, which clears the selection.
        let preview_dies = self
            .right_sidebar_remote_files
            .selected
            .as_ref()
            .is_some_and(|selected| selected == remote || selected.is_descendant_of(remote));
        let effects = self
            .right_sidebar_remote_files
            .transition(RemoteFilesEvent::EntryForgotten(remote.clone()));
        self.apply_right_sidebar_remote_files_effects(effects);
        if preview_dies {
            self.close_right_sidebar_file_preview();
        }
        if let Some(parent) = remote.parent() {
            let effects = self
                .right_sidebar_remote_files
                .transition(RemoteFilesEvent::DirectoryInvalidated(parent));
            self.apply_right_sidebar_remote_files_effects(effects);
        }
    }

    /// Walk a directory that is about to be deleted. The row exists from the
    /// walk onwards so the listing phase is visible and abortable, exactly
    /// like a folder download's.
    fn begin_remote_folder_delete(&mut self, remote: RemotePath, anchor: Point) {
        let name = remote.file_name().to_string();
        let Some(origin) = self.current_remote_operation_origin() else {
            self.push_remote_transfer_failure(
                RemoteTransferKind::Delete,
                name,
                "Remote Files source is no longer available".to_string(),
                None,
            );
            return;
        };
        let Some((backend, operation_lease, connection_key, connection_id)) =
            self.remote_transfer_handles(&origin)
        else {
            self.push_remote_transfer_failure(
                RemoteTransferKind::Delete,
                name,
                "Remote Files connection is no longer available".to_string(),
                None,
            );
            return;
        };
        let Some(window) = self.window.as_ref().cloned() else {
            return;
        };
        let id = self.next_remote_transfer_id();
        let progress = RemoteTransferProgress::default();
        self.right_sidebar_remote_transfers.push(RemoteTransfer {
            id,
            kind: RemoteTransferKind::Delete,
            name,
            progress: progress.clone(),
            status: RemoteTransferStatus::Running,
            // Deliberately no retry: a delete that half-happened changed the
            // tree, and "run it again" deserves a fresh look, not a replay.
            source: None,
            origin: Some(origin.clone()),
        });
        self.trim_remote_transfers();
        self.invalidate_window();

        promise::spawn::spawn(async move {
            let plan =
                plan_remote_walk(&*backend, remote.clone(), RemoteWalkMode::Delete, &progress)
                    .await;
            drop(operation_lease);
            let connection_died = plan.as_ref().is_err_and(|message| {
                invalidate_remote_connection_if_dead(&connection_key, connection_id, message)
            });
            window.notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                term_window.apply_remote_folder_delete_plan(
                    id,
                    remote,
                    plan,
                    origin,
                    anchor,
                    connection_key,
                    connection_id,
                    connection_died,
                );
            })));
        })
        .detach();
    }

    fn apply_remote_folder_delete_plan(
        &mut self,
        id: u64,
        remote: RemotePath,
        plan: Result<RemoteWalkPlan, String>,
        origin: RemoteOperationOrigin,
        anchor: Point,
        connection_key: String,
        connection_id: u64,
        connection_died: bool,
    ) {
        if connection_died {
            self.release_remote_files_lease_if_connection(&connection_key, connection_id);
        }
        if !self.remote_operation_origin_matches(&origin) {
            self.finish_remote_transfer(
                id,
                RemoteTransferStatus::Failed(
                    "The panel moved to another host while this folder was being read".to_string(),
                ),
            );
            self.invalidate_window();
            return;
        }
        let plan = match plan {
            Ok(plan) => plan,
            Err(message) => {
                self.finish_remote_transfer(id, RemoteTransferStatus::Failed(message));
                self.invalidate_window();
                return;
            }
        };
        // A folder delete confirms at ANY size — unlike the transfer
        // threshold, this is about irreversibility, not time.
        self.set_pending_remote_confirm(
            PendingRemoteConfirm::FolderDelete {
                transfer_id: id,
                remote,
                plan,
                origin,
            },
            anchor,
        );
        self.invalidate_window();
    }

    /// The confirmed folder delete: children first, the folder itself last.
    fn start_remote_folder_delete_execution(
        &mut self,
        id: u64,
        remote: RemotePath,
        plan: RemoteWalkPlan,
    ) {
        let Some(transfer) = self
            .right_sidebar_remote_transfers
            .iter()
            .find(|transfer| transfer.id == id)
        else {
            return;
        };
        let progress = transfer.progress.clone();
        let Some(origin) = transfer.origin.clone() else {
            self.finish_remote_transfer(
                id,
                RemoteTransferStatus::Failed(
                    "Remote transfer lost its source identity".to_string(),
                ),
            );
            return;
        };
        if !self.remote_operation_origin_matches(&origin) {
            self.finish_remote_transfer(
                id,
                RemoteTransferStatus::Failed(
                    "The panel moved to another host before the delete started".to_string(),
                ),
            );
            return;
        }
        if progress.is_canceled() {
            self.finish_remote_transfer(
                id,
                RemoteTransferStatus::Failed(REMOTE_TRANSFER_CANCELED.to_string()),
            );
            return;
        }
        let Some((backend, operation_lease, connection_key, connection_id)) =
            self.remote_transfer_handles(&origin)
        else {
            self.finish_remote_transfer(
                id,
                RemoteTransferStatus::Failed(
                    "Remote Files connection is no longer available".to_string(),
                ),
            );
            return;
        };
        let Some(window) = self.window.as_ref().cloned() else {
            return;
        };
        // +1: the folder itself is the last deletion.
        progress.set_item_total(plan.entries.len() as u64 + 1);
        promise::spawn::spawn(async move {
            let result = delete_remote_tree(&*backend, &remote, &plan.entries, &progress).await;
            drop(operation_lease);
            let connection_died = result.as_ref().is_err_and(|message| {
                invalidate_remote_connection_if_dead(&connection_key, connection_id, message)
            });
            window.notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                let status = match &result {
                    Ok(removed) => RemoteTransferStatus::Done(format!("Deleted {removed} items")),
                    Err(message) => RemoteTransferStatus::Failed(message.clone()),
                };
                term_window.finish_remote_transfer(id, status);
                if connection_died {
                    term_window
                        .release_remote_files_lease_if_connection(&connection_key, connection_id);
                }
                // Success or not, the subtree changed underneath the cache:
                // forget it and re-list the parent. After a partial failure
                // the directory still exists — the fresh parent listing keeps
                // it, collapsed, and expanding shows what is left.
                if term_window.remote_operation_origin_matches(&origin) {
                    term_window.forget_remote_entry_and_refresh_parent(&remote);
                }
                term_window.invalidate_window();
            })));
        })
        .detach();
    }

    /// "New Folder" on a remote directory: mint `untitled folder` (or the
    /// first numbered variant that is free) and drop into renaming it once
    /// its row appears.
    pub(crate) fn create_right_sidebar_remote_folder(&mut self, parent: RemotePath) {
        let Some(origin) = self.current_remote_operation_origin() else {
            self.right_sidebar_remote_files.error_message =
                Some("Remote Files source is no longer available".to_string());
            self.invalidate_window();
            return;
        };
        let Some((backend, operation_lease, connection_key, connection_id)) =
            self.remote_transfer_handles(&origin)
        else {
            self.right_sidebar_remote_files.error_message =
                Some("Remote Files connection is no longer available".to_string());
            self.invalidate_window();
            return;
        };
        let Some(window) = self.window.as_ref().cloned() else {
            return;
        };
        promise::spawn::spawn(async move {
            let mut created: Result<RemotePath, String> =
                Err("Unable to find a free folder name".to_string());
            for attempt in 0..100u32 {
                let name = if attempt == 0 {
                    "untitled folder".to_string()
                } else {
                    format!("untitled folder {}", attempt + 1)
                };
                let path = match parent.join_name(&name) {
                    Ok(path) => path,
                    Err(err) => {
                        created = Err(err);
                        break;
                    }
                };
                match backend.create_directory_exclusive(path.clone()).await {
                    Ok(()) => {
                        created = Ok(path);
                        break;
                    }
                    // The exclusive-create contract spells an occupied name
                    // exactly this way; anything else is a real failure.
                    Err(message) if message.ends_with("already exists") => continue,
                    Err(message) => {
                        created = Err(message);
                        break;
                    }
                }
            }
            drop(operation_lease);
            let connection_died = created.as_ref().is_err_and(|message| {
                invalidate_remote_connection_if_dead(&connection_key, connection_id, message)
            });
            window.notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                term_window.finish_remote_folder_creation(
                    parent,
                    created,
                    origin,
                    connection_key,
                    connection_id,
                    connection_died,
                );
            })));
        })
        .detach();
    }

    fn finish_remote_folder_creation(
        &mut self,
        parent: RemotePath,
        result: Result<RemotePath, String>,
        origin: RemoteOperationOrigin,
        connection_key: String,
        connection_id: u64,
        connection_died: bool,
    ) {
        if connection_died {
            self.release_remote_files_lease_if_connection(&connection_key, connection_id);
        }
        if !self.remote_operation_origin_matches(&origin) {
            return;
        }
        match result {
            Ok(path) => {
                // Open the parent so the new row can appear, re-list it, and
                // remember to start renaming once the listing lands — the row
                // does not exist to edit until then.
                self.right_sidebar_remote_files
                    .expanded
                    .insert(parent.clone());
                self.pending_remote_rename = Some((path, origin));
                let effects = self
                    .right_sidebar_remote_files
                    .transition(RemoteFilesEvent::DirectoryInvalidated(parent));
                self.apply_right_sidebar_remote_files_effects(effects);
            }
            Err(message) => {
                self.right_sidebar_remote_files.error_message = Some(message);
            }
        }
        self.invalidate_window();
    }

    /// Fire the deferred rename of a just-created folder once (and only once)
    /// its row is really on screen. Runs after every remote listing lands.
    fn maybe_begin_pending_remote_rename(&mut self) {
        let Some((pending, origin)) = self.pending_remote_rename.clone() else {
            return;
        };
        if !self.remote_operation_origin_matches(&origin) {
            self.pending_remote_rename = None;
            return;
        }
        match self.right_sidebar_remote_files.kind_for_path(&pending) {
            Some(RemoteFileKind::Directory) => {
                self.pending_remote_rename = None;
                self.start_sidebar_remote_file_rename(pending);
            }
            Some(_) => {
                // Something else wears the name now; renaming it would edit
                // the wrong thing.
                self.pending_remote_rename = None;
            }
            None => {
                // Give up once the parent has a fresh listing that does not
                // hold the row; before that, some other directory just loaded.
                if pending
                    .parent()
                    .is_some_and(|parent| self.right_sidebar_remote_files.has_listing(&parent))
                {
                    self.pending_remote_rename = None;
                }
            }
        }
    }

    /// A click on a transfer row. Running: stop it. Finished with a recorded
    /// source: ask Retry-or-Dismiss rather than guessing which the click
    /// meant. Anything else: clear the notice away, as before.
    pub(crate) fn remote_transfer_row_clicked(&mut self, id: u64, anchor: Point) {
        let Some(transfer) = self
            .right_sidebar_remote_transfers
            .iter()
            .find(|transfer| transfer.id == id)
        else {
            return;
        };
        if transfer.is_running() {
            transfer.progress.request_cancel();
            self.invalidate_window();
            return;
        }
        if transfer.can_retry() {
            let Some(window) = self.window.as_ref().cloned() else {
                return;
            };
            self.begin_context_menu_application_actions();
            let retry = self.context_menu_application_item_with_icon(
                crate::i18n::tr("right-retry"),
                ContextMenuIcon::Refresh,
                crate::termwindow::ContextMenuApplicationAction::RetryRemoteTransfer { id, anchor },
                true,
            );
            let dismiss = self.context_menu_application_item_with_icon(
                crate::i18n::tr("right-dismiss"),
                ContextMenuIcon::Close,
                crate::termwindow::ContextMenuApplicationAction::DismissRemoteTransfer(id),
                true,
            );
            self.show_term_context_menu(&window, anchor, vec![retry, dismiss]);
            return;
        }
        self.remove_remote_transfer_row(id);
    }

    pub(crate) fn remove_remote_transfer_row(&mut self, id: u64) {
        if let Some(index) = self
            .right_sidebar_remote_transfers
            .iter()
            .position(|transfer| transfer.id == id)
        {
            self.right_sidebar_remote_transfers.remove(index);
            self.invalidate_window();
        }
    }

    /// Run a failed transfer again from its recorded source.
    pub(crate) fn retry_remote_transfer(&mut self, id: u64, anchor: Point) {
        let Some(index) = self
            .right_sidebar_remote_transfers
            .iter()
            .position(|transfer| transfer.id == id)
        else {
            return;
        };
        let Some(origin) = self.right_sidebar_remote_transfers[index].origin.clone() else {
            return;
        };
        if !self.remote_operation_origin_matches(&origin) {
            // Keep the row and its source intact so switching back makes the
            // same Retry action useful. Never replay an absolute path against
            // the host that merely happens to be visible now.
            self.right_sidebar_remote_transfers[index].status = RemoteTransferStatus::Failed(
                "Switch Files back to the original host to retry".to_string(),
            );
            self.invalidate_window();
            return;
        }
        let transfer = self.right_sidebar_remote_transfers.remove(index);
        let Some(source) = transfer.source else {
            return;
        };
        match source {
            RemoteTransferSource::Upload {
                local,
                remote,
                directory,
            } => self.start_remote_upload(local, remote, directory, None),
            RemoteTransferSource::Download { remote } => {
                self.download_right_sidebar_remote_file(remote)
            }
            RemoteTransferSource::DownloadFolder { remote } => {
                self.download_right_sidebar_remote_folder(remote, anchor)
            }
        }
        self.invalidate_window();
    }

    /// Everything a transfer worker needs from the panel's lease. Takes an
    /// operation lease so the pooled connection cannot expire mid-transfer.
    fn current_remote_connection_key(&self) -> Option<String> {
        let target = self.right_sidebar_remote_files.target.as_ref()?;
        let source_key =
            crate::termwindow::remote_files::RemoteFilesState::source_key(&target.source);
        let config = Self::ssh_config_for_remote_files_target(target).ok()?;
        Some(remote_connection_key(&source_key, &config))
    }

    pub(crate) fn current_remote_operation_origin(&self) -> Option<RemoteOperationOrigin> {
        let source_key = self.right_sidebar_remote_files.current_source_key()?;
        let connection_key = self.current_remote_connection_key()?;
        let lease = self.right_sidebar_remote_files_lease.as_ref()?;
        (lease.connection_key() == connection_key)
            .then(|| RemoteOperationOrigin::new(source_key, connection_key))
    }

    pub(crate) fn remote_operation_origin_matches(&self, origin: &RemoteOperationOrigin) -> bool {
        let current_connection_key = self.current_remote_connection_key();
        origin.matches(
            self.right_sidebar_remote_files
                .current_source_key()
                .as_deref(),
            current_connection_key.as_deref(),
        )
    }

    /// In addition to the Files panel still showing the same connection,
    /// require the terminal receiving a dragged remote path to belong to that
    /// host. A window may contain a local overlay or panes from another
    /// domain, where the same absolute path would name a different object.
    pub(crate) fn remote_operation_origin_matches_pane(
        &self,
        origin: &RemoteOperationOrigin,
        pane_id: PaneId,
    ) -> bool {
        if !self.remote_operation_origin_matches(origin) {
            return false;
        }
        let Some(target) = self.right_sidebar_remote_files.target.as_ref() else {
            return false;
        };
        let Some(pane) = Mux::get().get_pane(pane_id) else {
            return false;
        };
        pane_domain_matches_remote_source(pane.domain_id(), &target.source)
    }

    /// Drop the panel lease only if it is the exact connection that failed.
    /// A delayed callback from host A must not disconnect host B, nor a newer
    /// replacement connection for A.
    pub(crate) fn release_remote_files_lease_if_connection(
        &mut self,
        connection_key: &str,
        connection_id: u64,
    ) -> bool {
        let disposition = remote_lease_failure_disposition(
            self.right_sidebar_remote_files_lease
                .as_ref()
                .map(|lease| (lease.connection_key(), lease.connection_id())),
            connection_key,
            connection_id,
        );
        let matches = disposition == RemoteLeaseFailureDisposition::FailedConnectionInstalled;
        if matches {
            self.right_sidebar_remote_files_lease.take();
        }
        matches
    }

    fn remote_files_lease_failure_disposition(
        &mut self,
        connection_key: &str,
        connection_id: u64,
    ) -> RemoteLeaseFailureDisposition {
        let disposition = remote_lease_failure_disposition(
            self.right_sidebar_remote_files_lease
                .as_ref()
                .map(|lease| (lease.connection_key(), lease.connection_id())),
            connection_key,
            connection_id,
        );
        if disposition == RemoteLeaseFailureDisposition::FailedConnectionInstalled {
            self.right_sidebar_remote_files_lease.take();
        }
        disposition
    }

    fn reconnect_remote_files_after_connection_change(&mut self, generation: u64, message: String) {
        self.close_right_sidebar_file_preview();
        self.right_sidebar_remote_files
            .transition(RemoteFilesEvent::ConnectionFailed {
                generation,
                message,
            });
        let effects = self
            .right_sidebar_remote_files
            .transition(RemoteFilesEvent::ConnectRequested);
        self.apply_right_sidebar_remote_files_effects(effects);
    }

    pub(crate) fn remote_transfer_handles(
        &self,
        origin: &RemoteOperationOrigin,
    ) -> Option<(
        Arc<dyn crate::termwindow::remote_files::RemoteFileBackend>,
        crate::termwindow::remote_files::RemoteConnectionLease,
        String,
        u64,
    )> {
        if !self.remote_operation_origin_matches(origin) {
            return None;
        }
        let lease = self.right_sidebar_remote_files_lease.as_ref()?;
        if lease.connection_key() != origin.connection_key() {
            return None;
        }
        let operation_lease = lease.operation_lease()?;
        Some((
            lease.backend(),
            operation_lease,
            lease.connection_key().to_string(),
            lease.connection_id(),
        ))
    }

    fn finish_remote_transfer(&mut self, id: u64, status: RemoteTransferStatus) {
        if let Some(transfer) = self
            .right_sidebar_remote_transfers
            .iter_mut()
            .find(|transfer| transfer.id == id)
        {
            transfer.status = status;
        }
        self.trim_remote_transfers();
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
        self.right_sidebar_remote_file_search.release();
        let effects = self
            .right_sidebar_remote_files
            .transition(RemoteFilesEvent::Refresh);
        self.apply_right_sidebar_remote_files_effects(effects);
    }

    pub(crate) fn open_right_sidebar_remote_file(&mut self, path: RemotePath) {
        // A search result can sit in a folder the tree never loaded; its kind
        // comes from the listing then.
        let kind = self
            .right_sidebar_remote_files
            .kind_for_path(&path)
            .or_else(|| self.right_sidebar_remote_file_search.kind_for_path(&path));
        match kind {
            Some(RemoteFileKind::Directory) => {
                let effects = self
                    .right_sidebar_remote_files
                    .transition(RemoteFilesEvent::ToggleDirectory(path));
                self.apply_right_sidebar_remote_files_effects(effects);
            }
            Some(RemoteFileKind::File) => self.open_right_sidebar_remote_file_preview(path),
            Some(RemoteFileKind::Symlink) | Some(RemoteFileKind::Other) | None => {}
        }
    }

    /// Preview the remote file at `path`, known to be a file.
    pub(crate) fn open_right_sidebar_remote_file_preview(&mut self, path: RemotePath) {
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

    fn remote_file_search_key(&self) -> Option<RemoteFileSearchKey> {
        Some(RemoteFileSearchKey {
            source_key: self.right_sidebar_remote_files.current_source_key()?,
            root: self.right_sidebar_remote_files.root.clone()?,
            respect_gitignore: self.config.right_sidebar_search_respects_gitignore,
        })
    }

    /// Answer `query` for the remote project, listing the project on the
    /// remote host first when there is no index for it yet or it has gone
    /// stale. The listing is one short `thinkterm list-files` on the existing
    /// connection; typing only searches what it returned.
    fn update_remote_file_search(&mut self, query: &str) {
        if query.is_empty() {
            let search = &mut self.right_sidebar_remote_file_search;
            search.cancel_search();
            search.query.clear();
            search.rows = Arc::default();
            return;
        }
        let Some(key) = self.remote_file_search_key() else {
            return;
        };
        if self.right_sidebar_remote_file_search.key.as_ref() != Some(&key) {
            self.right_sidebar_remote_file_search.release();
            self.right_sidebar_remote_file_search.key = Some(key.clone());
        }
        let search = &self.right_sidebar_remote_file_search;
        let query_changed = search.query != query;
        let stale = search
            .built_at
            .is_some_and(|at| at.elapsed() >= Duration::from_secs(FILE_INDEX_RESCAN_SECS));
        let needs_listing = match &search.status {
            RemoteFileSearchStatus::Idle => true,
            RemoteFileSearchStatus::Indexing | RemoteFileSearchStatus::NeedsUpdate => false,
            RemoteFileSearchStatus::Ready => stale && query_changed,
            // Tried again when the query changes, not on every frame; a
            // timeout only by a refresh or reconnect.
            RemoteFileSearchStatus::Failed(message) => {
                query_changed
                    && message != crate::termwindow::remote_files::REMOTE_LISTING_TIMED_OUT
            }
        };
        if needs_listing {
            self.start_remote_file_search_listing(key.clone());
        }
        if query_changed {
            self.right_sidebar_remote_file_search.query = query.to_string();
            self.spawn_remote_file_search_rows(key.root);
            self.right_sidebar_remote_file_tree_scroll_offset = 0.0;
        }
    }

    /// Search the remote index for the current query off the UI thread, as
    /// the local search is; the rows answer an earlier query until it lands.
    /// A newer query cancels it.
    fn spawn_remote_file_search_rows(&mut self, root: RemotePath) {
        let search = &mut self.right_sidebar_remote_file_search;
        search.cancel_search();
        let Some(index) = search.index.clone() else {
            search.rows = Arc::default();
            return;
        };
        if search.query.is_empty() {
            search.rows = Arc::default();
            return;
        }
        let Some(window) = self.window.as_ref().cloned() else {
            return;
        };
        let cancel = Arc::new(AtomicBool::new(false));
        search.search_cancel = Some(Arc::clone(&cancel));
        let generation = search.generation;
        let query = search.query.clone();
        promise::spawn::spawn(async move {
            let (rows, query) = promise::spawn::spawn_into_new_thread(move || {
                let rows = remote_file_search_rows(&index, &root, &query, &cancel);
                Ok::<_, anyhow::Error>((
                    (!cancel.load(AtomicOrdering::Relaxed)).then_some(rows),
                    query,
                ))
            })
            .await?;
            let Some(rows) = rows else {
                return Ok(());
            };
            window.notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                let search = &mut term_window.right_sidebar_remote_file_search;
                if search.generation != generation || search.query != query {
                    return;
                }
                search.search_cancel = None;
                search.rows = Arc::new(rows);
                term_window.invalidate_window();
            })));
            Ok::<(), anyhow::Error>(())
        })
        .detach();
    }

    /// The remote project's files, for a remote preview's wiki links: the
    /// listing search keeps when there is one for this root; otherwise it is
    /// requested, and the preview resolves its links again when it lands.
    fn remote_file_paths_for_preview_links(&mut self) -> Option<Arc<Vec<String>>> {
        let key = self.remote_file_search_key()?;
        if self.right_sidebar_remote_file_search.key.as_ref() != Some(&key) {
            self.right_sidebar_remote_file_search.release();
            self.right_sidebar_remote_file_search.key = Some(key.clone());
        }
        if let Some(index) = self.right_sidebar_remote_file_search.index.as_ref() {
            return Some(index.file_paths());
        }
        if self.right_sidebar_remote_file_search.status == RemoteFileSearchStatus::Idle {
            self.start_remote_file_search_listing(key);
        }
        None
    }

    /// Where a preview rooted at `root` finds the files its wiki links may
    /// name: a remote project's listing; the local project's search index when
    /// there is one, else a walk under the search's rules; and for a file
    /// outside the project, the files beside it. `None` while a remote
    /// listing is still on its way.
    fn preview_link_files(&mut self, root: &Path) -> Option<PreviewLinkFiles> {
        if root.as_os_str().is_empty() {
            return self
                .remote_file_paths_for_preview_links()
                .map(PreviewLinkFiles::Listed);
        }
        if self.right_sidebar_file_index_root.as_deref() != Some(root) {
            return Some(PreviewLinkFiles::Folder);
        }
        Some(match self.right_sidebar_file_index.as_ref() {
            Some(index) => PreviewLinkFiles::Listed(index.file_paths()),
            None => PreviewLinkFiles::WalkProject {
                respect_gitignore: self.config.right_sidebar_search_respects_gitignore,
            },
        })
    }

    fn start_remote_file_search_listing(&mut self, key: RemoteFileSearchKey) {
        // Without a connection yet this stays Idle and is tried again once the
        // lease lands.
        let Some(lease) = self.right_sidebar_remote_files_lease.as_ref() else {
            return;
        };
        if self.current_remote_connection_key().as_deref() != Some(lease.connection_key()) {
            return;
        }
        let Some(operation_lease) = lease.operation_lease() else {
            return;
        };
        let backend = lease.backend();
        let connection_key = lease.connection_key().to_string();
        let connection_id = lease.connection_id();
        let Some(window) = self.window.as_ref().cloned() else {
            return;
        };
        let project_name = self
            .right_sidebar_remote_files
            .target
            .as_ref()
            .map(|target| target.project_name.clone())
            .unwrap_or_default();
        let search = &mut self.right_sidebar_remote_file_search;
        search.status = RemoteFileSearchStatus::Indexing;
        let generation = search.generation;
        promise::spawn::spawn(async move {
            let listed = backend
                .list_project_files(key.root.clone(), key.respect_gitignore)
                .await;
            drop(operation_lease);
            if let Err(message) = &listed {
                if crate::termwindow::remote_files::remote_listing_failure_is_transport(message) {
                    invalidate_remote_connection_if_dead(&connection_key, connection_id, message);
                }
            }
            let result = match listed {
                Ok(RemoteProjectListing::Listed(listing)) => {
                    let truncated = listing.truncated;
                    promise::spawn::spawn_into_new_thread(move || {
                        Ok(remote_file_index_from_listing(&project_name, &listing))
                    })
                    .await
                    .map(|index| Some((Arc::new(index), truncated)))
                    .map_err(|err| format!("Unable to index remote files: {err}"))
                }
                Ok(RemoteProjectListing::NeedsUpdate) => Ok(None),
                Err(message) => Err(message),
            };
            window.notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                term_window.apply_remote_file_search_listing(generation, key, result);
            })));
        })
        .detach();
    }

    fn apply_remote_file_search_listing(
        &mut self,
        generation: u64,
        key: RemoteFileSearchKey,
        result: Result<Option<(Arc<RightSidebarFileIndex>, bool)>, String>,
    ) {
        let mut listed = false;
        let search = &mut self.right_sidebar_remote_file_search;
        if search.generation != generation || search.key.as_ref() != Some(&key) {
            return;
        }
        match result {
            Ok(Some((index, truncated))) => {
                if truncated {
                    log::warn!(
                        "Remote file search stopped at a limit; some files will not be findable by name"
                    );
                }
                search.index = Some(index);
                search.status = RemoteFileSearchStatus::Ready;
                search.built_at = Some(Instant::now());
                listed = true;
            }
            Ok(None) => {
                search.index = None;
                search.status = RemoteFileSearchStatus::NeedsUpdate;
            }
            // An index from before stays searchable.
            Err(message) => search.status = RemoteFileSearchStatus::Failed(message),
        }
        self.spawn_remote_file_search_rows(key.root.clone());
        if listed && self.right_sidebar_remote_files.selected.is_some() {
            // A remote preview may be waiting on these files for its links.
            self.with_note_surface(NoteSurface::FilePreview, |term_window| {
                if term_window.right_sidebar_note.projection.has_wiki_links() {
                    term_window.right_sidebar_note.request_link_resolution();
                }
            });
        }
        if let Some(window) = self.window.as_ref() {
            window.invalidate();
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
        if self.current_remote_connection_key().as_deref() != Some(lease.connection_key()) {
            self.reconnect_remote_files_after_connection_change(
                generation,
                "Remote Files configuration changed before loading the preview".to_string(),
            );
            return;
        }
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
        let connection_id = lease.connection_id();
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
                invalidate_remote_connection_if_dead(&connection_key, connection_id, message)
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
                    raw_text: None,
                }),
                Err(err) => RightSidebarLoadedFilePreview {
                    lines: Vec::new(),
                    image: None,
                    message: Some(err),
                    truncated: false,
                    raw_text: None,
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
                    match term_window
                        .remote_files_lease_failure_disposition(&connection_key, connection_id)
                    {
                        RemoteLeaseFailureDisposition::ReplacementForSameTarget => {
                            term_window.spawn_right_sidebar_remote_file_preview(
                                generation, source_key, path,
                            );
                            term_window.invalidate_window();
                            return;
                        }
                        RemoteLeaseFailureDisposition::ReplacementForDifferentTarget => {
                            term_window.reconnect_remote_files_after_connection_change(
                                generation,
                                "Remote Files connection changed while loading the preview"
                                    .to_string(),
                            );
                            term_window.invalidate_window();
                            return;
                        }
                        RemoteLeaseFailureDisposition::FailedConnectionInstalled
                        | RemoteLeaseFailureDisposition::NoLease => {}
                    }
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
                term_window.set_right_sidebar_file_preview_raw_text(result.raw_text);
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
        let space_id = self.content_space_id();
        if workspace_threads::client_domain_for_space(&space_id).is_some() {
            return Err("Remote file browsing is not supported yet".to_string());
        }

        let mux = Mux::get();
        let active_workspace = self
            .current_mux_workspace()
            .unwrap_or_else(|| mux.active_workspace());
        let workspaces = mux.iter_workspaces();
        let view = workspace_threads::view_for_current_project(
            &space_id,
            &active_workspace,
            &workspaces,
            false,
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
        let filter_label = crate::i18n::tr("right-filter-files");

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
            &filter_label,
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
        // A root the system refused renders as the reason, not as an empty
        // project -- "no files" reads as "my work is gone". Same
        // classification as the content-area page so the two never disagree.
        // The Refresh button above is the retry.
        if let Some(problem) = self.right_sidebar_file_dir_cache.failure(&root.path) {
            // Wrap to the width the card actually paints into, or every
            // line gets ellipsized a second time at paint.
            let card_text_width = content_width.saturating_sub(
                self.ui_px(22) + self.ui_px(SIDEBAR_ICON_GAP) + self.ui_px(SIDEBAR_INSET) * 3,
            );
            let mut wrapped =
                self.wrap_sidebar_problem_lines(ui_font, &[problem.title()], card_text_width);
            let path_text = root.path.display().to_string();
            wrapped.extend(wrap_path_for_width(&path_text, 3, |segment| {
                self.sidebar_text_width(ui_font, segment)
                    .unwrap_or(f32::MAX)
                    / card_text_width.max(1) as f32
            }));
            if let Some(hint) = problem.hint() {
                wrapped.extend(self.wrap_sidebar_problem_lines(ui_font, &[hint], card_text_width));
            }
            let wrapped: Vec<&str> = wrapped.iter().map(String::as_str).collect();
            return self.paint_files_message_lines(
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
                problem.icon(),
                &wrapped,
            );
        }
        if self
            .right_sidebar_file_expanded
            .insert(path_key(&root.path))
        {
            self.right_sidebar_file_expanded_version =
                self.right_sidebar_file_expanded_version.wrapping_add(1);
        }

        let applied_filter = self.right_sidebar_file_filter_for_tree();
        let query = applied_filter.trim().to_string();
        self.start_right_sidebar_file_search_if_needed(&query);
        let row_count = if query.is_empty() {
            // Browsing is served entirely by the lazily-read directory cache and
            // deliberately does not consult the index status: the whole-project
            // index exists only for search and may never be built, so gating the
            // tree on it would leave the tree permanently blank.
            self.refresh_right_sidebar_file_browse_rows();
            self.right_sidebar_file_browse_rows.len()
        } else {
            // Search is the one consumer that needs the whole project walked.
            let index_ready = matches!(
                self.right_sidebar_file_index_status,
                RightSidebarFileIndexStatus::Ready
            ) && self.right_sidebar_file_index.is_some();
            if !index_ready {
                let message = match self.right_sidebar_file_index_status.clone() {
                    RightSidebarFileIndexStatus::Failed(message) => message,
                    _ => crate::i18n::tr("right-indexing-files"),
                };
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
                &crate::i18n::tr("right-searching-files"),
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
                &crate::i18n::tr("right-no-matching-files"),
            );
        }

        let row_metrics = right_sidebar_file_row_metrics(ui_metrics);
        // A copy started here must report itself here; the strip is shared
        // with the remote tree, so reserve its space the same way.
        // The transfer strip is the one thing that can sit under this list, and
        // it paints no background of its own, so rows must not run onto it.
        // With no strip there is nothing below at all: the panel ends at the
        // window edge, which cuts the overflow for free.
        let strip_rows = self.transfer_strip_rows(
            content_bottom
                .saturating_sub(tree_top)
                .saturating_sub(self.ui_px(SIDEBAR_INSET)),
            row_metrics.row_height,
        );
        let strip_top = content_bottom
            .saturating_sub(self.ui_px(SIDEBAR_INSET))
            .saturating_sub(strip_rows.saturating_mul(row_metrics.row_height));
        // The panel's bottom padding, which is also the mask that keeps rows
        // out of it. With a transfer strip below it has to swallow the row's
        // tallest element -- the inline rename box, `row_height - 4px` --
        // since anything it misses lands on the strip; without one the
        // window edge catches the rest, so plain padding is enough.
        let bottom_reserve = if strip_rows > 0 {
            (self.ui_px(SIDEBAR_INSET) * 2)
                .max(row_metrics.icon_size)
                .max(row_metrics.row_height.saturating_sub(self.ui_px(4)))
        } else {
            self.ui_px(SIDEBAR_INSET) * 2
        };
        let viewport_bottom = strip_top.saturating_sub(bottom_reserve);
        // How far a row may hang below the list: into the mask band while a
        // transfer strip is showing; otherwise as far as it likes.
        let row_overflow_bottom = if strip_rows > 0 {
            strip_top
        } else {
            usize::MAX
        };
        // Whether anything below the list needs protecting from overflow.
        // Decides both the bottom mask and how far row chrome may extend:
        // unprotected, rows run to the window edge and are cut there.
        let masked_below = strip_rows > 0;
        let row_clip_bottom = if masked_below {
            viewport_bottom
        } else {
            content_bottom
        };
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
                row_clip_bottom,
                content_top,
                row_overflow_bottom,
                row_metrics,
            )?;
        }

        if max_scroll > 0.0 && scroll_offset > 0.0 {
            // The fade must take over exactly where the opaque mask ends
            // (tree_top), at full opacity. Starting it higher makes it
            // arrive at the mask edge already part-faded, which shows as
            // an alpha step -- a hard cut -- instead of a gradient.
            let fade_top = tree_top;
            let fade_height = self
                .ui_px(FILE_SCROLL_FADE_HEIGHT)
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
                &filter_label,
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

        // The bottom mask exists to keep overflow off whatever sits under
        // the list -- the transfer strip or a bottom tab bar. With neither
        // there is only the window edge below, and rows should run to it
        // and be cut there: slicing glyphs short of the edge and leaving a
        // dead band under the cut reads as a rendering bug, not padding.
        if max_scroll > 0.0 && masked_below {
            self.paint_right_sidebar_file_mask(
                layers,
                chrome,
                content_x,
                viewport_bottom,
                content_width,
                bottom_reserve,
            )?;
        }

        self.paint_transfer_strip(
            layers,
            ui_font,
            ui_metrics,
            chrome,
            foreground,
            muted_fg,
            content_x,
            strip_top,
            content_width,
            strip_rows,
            row_metrics,
        )?;

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
        self.paint_files_message_lines(
            layers,
            ui_font,
            ui_metrics,
            chrome,
            muted_fg,
            x,
            y,
            width,
            content_bottom,
            icon_size,
            SvgIcon::CircleAlert,
            std::slice::from_ref(&message),
        )
    }

    /// Wrap each line to the panel width: the hint is a full sentence and
    /// the path can be long, and both would otherwise be ellipsized down to
    /// their least useful halves.
    fn wrap_sidebar_problem_lines(
        &mut self,
        ui_font: &Rc<LoadedFont>,
        lines: &[String],
        width: usize,
    ) -> Vec<String> {
        const MAX_LINES_PER_ENTRY: usize = 3;
        lines
            .iter()
            .flat_map(|line| {
                wrap_snippet_text_for_width(line, MAX_LINES_PER_ENTRY, false, |segment| {
                    self.sidebar_text_width(ui_font, segment)
                        .unwrap_or(f32::MAX)
                        / width.max(1) as f32
                })
            })
            .collect()
    }

    /// The same card, but able to hold a classified failure: its own icon, and
    /// as many lines as it takes to name the folder and what to do about it.
    /// One line renders identically to [`Self::paint_files_message`].
    #[allow(clippy::too_many_arguments)]
    fn paint_files_message_lines(
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
        icon: SvgIcon,
        lines: &[&str],
    ) -> anyhow::Result<()> {
        if lines.is_empty() {
            return Ok(());
        }
        let line_height = ui_metrics.cell_size.height as usize;
        let inset = self.ui_px(SIDEBAR_INSET);
        let layout = sidebar_message_layout(
            content_bottom.saturating_sub(y + inset),
            self.ui_px(RIGHT_SIDEBAR_EMPTY_HEIGHT),
            line_height,
            inset,
            lines.len(),
        );
        let height = layout.height;
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
        self.paint_sidebar_icon(layers, icon, icon_x, icon_y, empty_icon_size, muted_fg)?;
        let text_x = icon_x + empty_icon_size + self.ui_px(SIDEBAR_ICON_GAP) + 2;
        let text_width = width.saturating_sub(
            empty_icon_size + self.ui_px(SIDEBAR_ICON_GAP) + self.ui_px(SIDEBAR_INSET) * 3,
        );
        let mut visible_lines = lines
            .iter()
            .take(layout.visible_lines)
            .map(|line| Cow::Borrowed(*line))
            .collect::<Vec<_>>();
        if layout.visible_lines < lines.len() {
            if let Some(last) = visible_lines.last_mut() {
                *last = Cow::Owned(format!("{}…", last.trim_end_matches('…')));
            }
        }
        let mut text_y =
            y + (height.saturating_sub(line_height.saturating_mul(visible_lines.len()))) / 2;
        for line in &visible_lines {
            self.paint_sidebar_text(
                layers,
                ui_font,
                ui_metrics,
                line.as_ref(),
                text_x,
                text_y,
                text_width,
                muted_fg,
            )?;
            text_y += line_height;
        }
        Ok(())
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
        if width == 0 || height == 0 || self.chrome_see_through() {
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

    pub(crate) fn paint_right_sidebar_file_mask(
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
        if self.right_sidebar_recording_contents {
            if let TripleLayerQuadAllocator::Heap(contents) = layers {
                contents.occlude(&[QuadClipRect::from_top_left_pixels(
                    x as f32,
                    y as f32,
                    (x + width) as f32,
                    (y + height) as f32,
                    &self.dimensions,
                )]);
                return Ok(());
            }
        }

        self.filled_rectangle(
            layers,
            2,
            euclid::rect(x as f32, y as f32, width as f32, height as f32),
            self.chrome_surface(chrome.workspace_sidebar_bg),
        )
        .context("right sidebar file scroll mask")?;
        Ok(())
    }

    /// Paints `contents` -- what a panel holds, over the ground it has
    /// already painted -- into `layers`. See-through, they are recorded on
    /// their own first, so that the scroll masks painted among them cut what
    /// they cover out of what `contents` painted before them: painting the
    /// ground over it again, as an opaque window does, would show as a darker
    /// band. Nested, the inner block's masks cut only the inner block.
    pub(crate) fn paint_right_sidebar_contents(
        &mut self,
        layers: &mut TripleLayerQuadAllocator,
        contents: impl FnOnce(&mut Self, &mut TripleLayerQuadAllocator) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        if !self.chrome_see_through() {
            return contents(self, layers);
        }
        let mut recorded = self
            .right_sidebar_contents_scratch
            .pop()
            .unwrap_or_default();
        recorded.recycle();
        let outer = std::mem::replace(&mut self.right_sidebar_recording_contents, true);
        let result = contents(self, &mut TripleLayerQuadAllocator::Heap(&mut recorded));
        self.right_sidebar_recording_contents = outer;
        let applied = result.and_then(|()| recorded.apply_to(layers));
        // Kept for the next frame, unless a long note made it large.
        if recorded.resident_bytes() <= RIGHT_SIDEBAR_SCRATCH_KEEP_BYTES {
            self.right_sidebar_contents_scratch.push(recorded);
        }
        applied
    }

    pub(crate) fn paint_right_sidebar_file_top_fade(
        &mut self,
        layers: &mut TripleLayerQuadAllocator,
        chrome: UiPalette,
        x: usize,
        y: usize,
        width: usize,
        height: usize,
    ) -> anyhow::Result<()> {
        // See-through, the list is cut where the mask begins instead: a fade
        // of the ground over it would darken the ground.
        if width == 0 || height == 0 || self.chrome_see_through() {
            return Ok(());
        }

        for step in 0..height {
            let progress = step as f32 / height as f32;
            let alpha = 1.0 - progress * progress * (3.0 - 2.0 * progress);
            self.filled_rectangle(
                layers,
                2,
                euclid::rect(x as f32, (y + step) as f32, width as f32, 1.0),
                self.chrome_surface(chrome.workspace_sidebar_bg)
                    .mul_alpha(alpha),
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
        // Bounds of the two opaque scroll masks. Row contents are quads that
        // cannot be scissored, so they are painted while they straddle an edge
        // and the mask cuts them; these say how far that is allowed to go.
        mask_top: usize,
        panel_bottom: usize,
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
        // A directory the system refused still renders as a row -- hiding it
        // would repeat the "my work is gone" lie one level down -- but it must
        // not look like an ordinary empty folder, so it carries a warning
        // marker at the right edge.
        let dir_refused = row.is_dir
            && self
                .right_sidebar_file_dir_cache
                .failure(&row.path)
                .is_some();
        let is_selected = selected.is_some_and(|path| path == &row.path);
        // A drag hovering here is about to copy into this directory; say so
        // unmistakably, outranking the ordinary selection tint.
        let is_drop_target = self.right_sidebar_local_drop_target.as_ref() == Some(&row.path);
        if hovered || is_selected || is_drop_target {
            self.fill_rounded_rectangle(
                layers,
                1,
                euclid::rect(
                    x as f32,
                    visible_y as f32,
                    width as f32,
                    visible_height as f32,
                ),
                if is_drop_target {
                    chrome.selected_bg
                } else if is_selected {
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

        let visible = |elem_y: usize, elem_height: usize| {
            sidebar_row_element_visible(elem_y, elem_height, mask_top, clip_top, panel_bottom)
        };
        let row_icon_size = row_metrics.icon_size;
        let chevron_size = row_metrics.chevron_size;
        let indent = row
            .depth
            .saturating_mul(row_metrics.indent_step)
            .min(width.saturating_sub(24));
        let chevron_x = x + self.ui_px(SIDEBAR_INSET) + indent;
        let icon_y = y + (row_metrics.row_height.saturating_sub(row_icon_size)) / 2;
        let chevron_y = y + (row_metrics.row_height.saturating_sub(chevron_size)) / 2;
        if row.is_dir && visible(chevron_y, chevron_size) {
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
        if visible(icon_y, row_icon_size) {
            match file_icon_for_row(row) {
                RightSidebarFileIcon::Material(icon) => {
                    self.paint_sidebar_material_icon(
                        layers,
                        icon,
                        file_icon_x,
                        icon_y,
                        row_icon_size,
                    )?;
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
        }
        let text_x = file_icon_x + row_icon_size + row_metrics.icon_gap;
        let mut text_width = x
            .saturating_add(width)
            .saturating_sub(text_x + self.ui_px(SIDEBAR_INSET));
        if dir_refused {
            let marker_x = x
                .saturating_add(width)
                .saturating_sub(self.ui_px(SIDEBAR_INSET) + row_icon_size);
            if visible(icon_y, row_icon_size) {
                self.paint_sidebar_icon(
                    layers,
                    SvgIcon::CircleAlert,
                    marker_x,
                    icon_y,
                    row_icon_size,
                    muted_fg,
                )?;
            }
            text_width = text_width.saturating_sub(row_icon_size + row_metrics.icon_gap);
        }
        if let Some(input) = self.sidebar_file_rename_input(&row.path).cloned() {
            let box_y = y + self.ui_px(2);
            let box_height = row_metrics.row_height.saturating_sub(self.ui_px(4));
            if visible(box_y, box_height) {
                self.paint_snippet_text_box(
                    layers,
                    1,
                    ui_font,
                    ui_metrics,
                    chrome,
                    muted_fg,
                    text_x,
                    box_y,
                    text_width,
                    box_height,
                    None,
                    "",
                    &input,
                    true,
                    UIItemType::RightSidebarFileRow(row.path.clone()),
                    false,
                )?;
            }
            Ok(())
        } else {
            let cell_height = ui_metrics.cell_size.height as usize;
            let text_y = y + (row_metrics.row_height.saturating_sub(cell_height)) / 2;
            // The hover tag exists to recover what the ellipsis ate; a name
            // that fits carries no tag. The shared shape cache makes this
            // re-measure a lookup, not a second shaping pass.
            if matches!(
                self.ellipsize_ui_text(ui_font, &row.name, text_width)?,
                Cow::Owned(_)
            ) {
                self.right_sidebar_truncated_file_rows
                    .insert(row.path.clone());
            }
            if visible(text_y, cell_height) {
                self.paint_sidebar_text(
                    layers,
                    ui_font,
                    ui_metrics,
                    &row.name,
                    text_x,
                    text_y,
                    text_width,
                    if row.is_dir || is_selected {
                        foreground
                    } else {
                        muted_fg
                    },
                )?;
            }
            Ok(())
        }
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
        let header_top = content_top + self.ui_px(PREVIEW_HEADER_TOP_GAP);
        let button_size = self.ui_px(PREVIEW_HEADER_BUTTON).min(content_width);
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

        let action_gap = self.ui_px(PREVIEW_HEADER_ACTION_GAP);
        let toggle_width = self.paint_markdown_preview_toggle(
            layers,
            chrome,
            foreground,
            muted_fg,
            content_x + button_size + action_gap,
            header_top,
            button_size,
        )?;
        let available_after_back = content_width
            .saturating_sub(button_size + self.ui_px(SIDEBAR_INSET))
            .saturating_sub(toggle_width);
        // The label is always shown in full. Size the button to fit it, limited
        // only by the room left after the two icon buttons, the gaps and a small
        // reserved minimum for the filename — no fixed cap, so a wide pane is
        // actually used.
        let app_label = self.right_sidebar_current_open_with_app_label(path);
        let open_label = match &app_label {
            Some(app) => right_sidebar_arg("right-open-with", "app", app.clone()),
            None => crate::i18n::tr("right-open"),
        };
        let label_px = self
            .sidebar_text_width(ui_font, &open_label)
            .unwrap_or(0.0)
            .ceil() as usize;
        let desired_open_width = label_px + self.ui_px(PREVIEW_OPEN_LABEL_CHROME);
        let max_open_width = available_after_back.saturating_sub(
            (button_size * 2) + action_gap * 3 + self.ui_px(PREVIEW_HEADER_NAME_RESERVE),
        );
        let open_button_min = self.ui_px(PREVIEW_OPEN_BUTTON_MIN);
        let open_button_width = if max_open_width >= open_button_min {
            desired_open_width.min(max_open_width).max(open_button_min)
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

        let title_x = content_x + button_size + self.ui_px(SIDEBAR_INSET) + toggle_width;
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
        let header_top = content_top + self.ui_px(PREVIEW_HEADER_TOP_GAP);
        let button_size = self.ui_px(PREVIEW_HEADER_BUTTON).min(content_width);
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
        let toggle_width = self.paint_markdown_preview_toggle(
            layers,
            chrome,
            foreground,
            muted_fg,
            content_x + button_size + self.ui_px(PREVIEW_HEADER_ACTION_GAP),
            header_top,
            button_size,
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
        let title_x = content_x + button_size + self.ui_px(SIDEBAR_INSET) + toggle_width;
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
        // Only split off a dropdown arrow once the button is wide enough that
        // the label still fits beside it.
        let arrow_width = if width >= self.ui_px(PREVIEW_OPEN_SPLIT_MIN_WIDTH) {
            self.ui_px(PREVIEW_OPEN_ARROW_WIDTH).min(width / 3)
        } else {
            0
        };
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
    pub(crate) fn paint_files_preview_header_icon_button(
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
        content_bottom: usize,
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

        if self.right_sidebar_markdown_preview_rendering()
            && self.ensure_right_sidebar_markdown_preview()
        {
            self.paint_right_sidebar_markdown_preview(
                layers,
                ui_font,
                ui_metrics,
                chrome,
                foreground,
                muted_fg,
                metrics.x,
                metrics.y,
                metrics.width,
                content_bottom,
            )?;
        } else if let Some(message) = self.right_sidebar_file_preview_message.clone() {
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
                let truncated_label = crate::i18n::tr("right-preview-truncated");
                let line_top = body_y as f32 + (line_count * line_height) as f32 - scroll_offset;
                if line_top < body_bottom as f32 {
                    let visible_text =
                        preview_text_slice(&truncated_label, horizontal_offset, paint_columns);
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

        if self.allow_images == crate::termwindow::render::paint::AllowImage::No {
            return Ok(());
        }
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
    /// The snippet list's fixed header. Painted on layer 2 *after* the cards,
    /// so it lands on top of the mask that erases their scroll overflow.
    #[allow(clippy::too_many_arguments)]
    fn paint_snippets_toolbar(
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
    ) -> anyhow::Result<()> {
        let toolbar_y = content_top;
        let new_snippet_label = crate::i18n::tr("right-new-snippet");
        let new_button_icon_size = (ui_metrics.cell_size.height as usize + self.ui_px(2))
            .clamp(self.ui_px(18), self.ui_px(22));
        let new_button_label_width =
            self.sidebar_text_width(ui_font, &new_snippet_label)?.ceil() as usize;
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
            &new_snippet_label
        };
        self.paint_snippet_button(
            layers,
            2,
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
                2,
                ui_font,
                ui_metrics,
                chrome,
                muted_fg,
                search_x,
                search_y,
                search_width,
                self.ui_px(SNIPPET_SEARCH_HEIGHT),
                Some(SvgIcon::Search),
                &crate::i18n::tr("right-search"),
                &search_input,
                self.right_sidebar_snippet_focus == Some(RightSidebarSnippetField::Search),
                UIItemType::RightSidebarSnippetSearch,
                false,
            )?;
        }

        Ok(())
    }

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
        let list_top = content_top
            + self
                .ui_px(SNIPPET_TOOLBAR_HEIGHT)
                .max(self.ui_px(SNIPPET_SEARCH_HEIGHT))
            + self.ui_px(SNIPPET_LIST_TOP_GAP);
        let query = self.right_sidebar_snippet_search.text();
        let searching = !query.trim().is_empty();
        let snippets = self.snippet_rows();
        let row_height = self.ui_px(SNIPPET_CARD_HEIGHT) + self.ui_px(SNIPPET_ROW_GAP);
        // The panel's bottom padding, which doubles as the mask that keeps
        // cards out of it; whatever overshoots it is cut by the window edge.
        // Matches the panel's own left and right margin.
        let bottom_reserve = self.ui_px(SIDEBAR_INSET) * 2;
        let visible_height = content_bottom.saturating_sub(list_top + bottom_reserve);
        let total_height = self.right_sidebar_snippet_scroll_height(snippets.len(), visible_height);
        let max_scroll = total_height.saturating_sub(visible_height) as f32;
        self.right_sidebar_snippet_scroll_offset = self
            .right_sidebar_snippet_scroll_offset
            .clamp(0.0, max_scroll);
        let scroll_offset = self.right_sidebar_snippet_scroll_offset;

        if snippets.is_empty() {
            let availability = self.snippet_availability();
            let empty_height = self
                .ui_px(RIGHT_SIDEBAR_EMPTY_HEIGHT)
                .min(content_bottom.saturating_sub(list_top + self.ui_px(SIDEBAR_INSET)));
            // Still on their way from the plugin host, usually for a frame
            // or two: saying there are none would be wrong, so say nothing.
            if empty_height == 0 || availability == crate::snippets::Availability::Loading {
                // No room for the empty-state card, but the toolbar must
                // still paint: it holds the search box, and losing it here
                // would leave a non-matching filter impossible to clear.
                return self.paint_snippets_toolbar(
                    layers,
                    ui_font,
                    ui_metrics,
                    chrome,
                    foreground,
                    muted_fg,
                    content_x,
                    content_top,
                    content_width,
                );
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
            let empty_label = match availability {
                crate::snippets::Availability::Unavailable(_) => {
                    crate::i18n::tr("right-snippets-unavailable")
                }
                _ if !searching => crate::i18n::tr("right-no-snippets"),
                _ => crate::i18n::tr("right-no-matching-snippets"),
            };
            self.paint_sidebar_text(
                layers,
                ui_font,
                ui_metrics,
                &empty_label,
                empty_icon_x + empty_icon_size + self.ui_px(SIDEBAR_ICON_GAP) + 2,
                list_top + (empty_height.saturating_sub(ui_metrics.cell_size.height as usize)) / 2,
                content_width.saturating_sub(
                    empty_icon_size + self.ui_px(SIDEBAR_ICON_GAP) + self.ui_px(SIDEBAR_INSET) * 3,
                ),
                muted_fg,
            )?;
            return self.paint_snippets_toolbar(
                layers,
                ui_font,
                ui_metrics,
                chrome,
                foreground,
                muted_fg,
                content_x,
                content_top,
                content_width,
            );
        }

        let list_top_f = list_top as f32;
        let viewport_bottom = content_bottom.saturating_sub(bottom_reserve);
        let viewport_bottom_f = viewport_bottom as f32;
        // Downward overflow past the panel is cut by the window edge, so
        // cards run to it; their chrome must be allowed as far, or text
        // outruns its card.
        let overflow_bottom = usize::MAX;
        let card_clip_bottom = content_bottom;
        for (idx, snippet) in snippets.into_iter().enumerate() {
            let row_top = list_top_f + (idx * row_height) as f32 - scroll_offset;
            let row_bottom = row_top + self.ui_px(SNIPPET_CARD_HEIGHT) as f32;
            if row_bottom <= list_top_f {
                continue;
            }
            if row_top >= viewport_bottom_f {
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
                card_clip_bottom,
                content_top,
                overflow_bottom,
            )?;
        }

        // Erase the overflow the cards were allowed to paint above the list,
        // then put the toolbar back on top of that mask.
        if max_scroll > 0.0 && scroll_offset > 0.0 {
            self.paint_right_sidebar_file_mask(
                layers,
                chrome,
                content_x,
                content_top,
                content_width,
                list_top.saturating_sub(content_top),
            )?;
        }
        self.paint_snippets_toolbar(
            layers,
            ui_font,
            ui_metrics,
            chrome,
            foreground,
            muted_fg,
            content_x,
            content_top,
            content_width,
        )?;
        if max_scroll > 0.0 && scroll_offset > 0.0 {
            let fade_height = self
                .ui_px(FILE_SCROLL_FADE_HEIGHT)
                .min(viewport_bottom.saturating_sub(list_top));
            self.paint_right_sidebar_file_top_fade(
                layers,
                chrome,
                content_x,
                list_top,
                content_width,
                fade_height,
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
        let save_label = crate::i18n::tr("right-save");
        let save_label_width = self.sidebar_text_width(ui_font, &save_label)?.ceil() as usize;
        let save_width = (save_label_width + self.ui_px(SIDEBAR_INSET) * 6)
            .clamp(self.ui_px(110), self.ui_px(136))
            .min(content_width.saturating_sub(back_size + self.ui_px(SIDEBAR_INSET) * 2));
        let save_x = content_x + content_width.saturating_sub(save_width);
        let title_width = save_x.saturating_sub(title_x + self.ui_px(SIDEBAR_INSET) * 2);
        let title_label = match self.right_sidebar_snippet_view {
            RightSidebarSnippetView::EditNew => crate::i18n::tr("right-new-snippet"),
            RightSidebarSnippetView::EditExisting(_) => crate::i18n::tr("right-edit-snippet"),
            RightSidebarSnippetView::List => crate::i18n::tr("right-snippet"),
        };
        self.paint_sidebar_text(
            layers,
            ui_font,
            ui_metrics,
            &title_label,
            title_x,
            header_top,
            title_width,
            foreground,
        )?;
        self.paint_sidebar_text(
            layers,
            ui_font,
            ui_metrics,
            &crate::i18n::tr("right-personal-vault"),
            title_x,
            header_top + ui_metrics.cell_size.height as usize + 8,
            title_width,
            muted_fg,
        )?;

        self.paint_snippet_button(
            layers,
            1,
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
            &save_label,
            UIItemType::RightSidebarSnippetSave,
            true,
        )?;

        let field_label_height = ui_metrics.cell_size.height as usize;
        let title_label_y = header_y + self.ui_px(SNIPPET_EDITOR_HEADER_HEIGHT);
        self.paint_sidebar_text(
            layers,
            ui_font,
            ui_metrics,
            &crate::i18n::tr("right-action-description"),
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
            &crate::i18n::tr("right-action-description-placeholder"),
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
            &crate::i18n::tr("right-script-required"),
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
            &crate::i18n::tr("right-script-placeholder"),
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
        snippet: &crate::snippets::Row,
        clip_top: usize,
        clip_bottom: usize,
        // Bounds of the two opaque scroll masks. Text is a quad the sidebar
        // cannot scissor, so a line is painted while it straddles an edge and
        // the mask cuts it; these say how far that may go.
        mask_top: usize,
        panel_bottom: usize,
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

        let visible = |elem_y: usize, elem_height: usize| {
            sidebar_row_element_visible(elem_y, elem_height, mask_top, clip_top, panel_bottom)
        };
        let cell_height = ui_metrics.cell_size.height as usize;
        let card_pad = self.ui_px(SIDEBAR_INSET) * 2;
        let text_x = x + card_pad;
        let run_label = crate::i18n::tr("right-run");
        let paste_label = crate::i18n::tr("menu-paste");
        let run_button_width = (self.sidebar_text_width(ui_font, &run_label)?.ceil() as usize
            + self.ui_px(SIDEBAR_INSET) * 4)
            .max(self.ui_px(SNIPPET_ACTION_BUTTON_MIN_WIDTH));
        let paste_button_width = (self.sidebar_text_width(ui_font, &paste_label)?.ceil() as usize
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
        if visible(title_y, cell_height) {
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
        let preview_y = y + self.ui_px(SIDEBAR_INSET) * 2 + cell_height + 8;
        if visible(preview_y, cell_height) {
            self.paint_sidebar_text(
                layers,
                ui_font,
                ui_metrics,
                &snippet.preview,
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
                1,
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
                &run_label,
                UIItemType::RightSidebarSnippetRun(snippet.id.clone()),
                true,
            )?;
            self.paint_snippet_button(
                layers,
                1,
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
                &paste_label,
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
    pub(crate) fn paint_snippet_button(
        &mut self,
        layers: &mut TripleLayerQuadAllocator,
        // Background layer. Callers that re-draw the button above a scroll
        // mask pass 2; everything else passes 1.
        layer: usize,
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
            layer,
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
        // A label that fits is centred on its advance width but may draw into
        // the padding: a glyph's image can reach a pixel or two past its
        // advance, and the painter drops any glyph whose image crosses the
        // bound, so at exactly the advance width "Paste" read "Past".
        let label_room = if measured_text_width <= available_label_width {
            (x + width).saturating_sub(text_x)
        } else {
            text_width
        };
        self.paint_sidebar_text(
            layers,
            ui_font,
            ui_metrics,
            label,
            text_x,
            y + (height.saturating_sub(ui_metrics.cell_size.height as usize)) / 2,
            label_room,
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

/// Columns a tab advances to. Editor-style rather than the terminal's 8, since
/// this pane shows source files.
const PREVIEW_TAB_WIDTH: usize = 4;

/// Make raw file bytes safe to shape.
///
/// Nothing here draws a control character sensibly: no font has a glyph for
/// U+0009, so a tab-indented file renders a row of `.notdef` boxes (and spams
/// `No fonts contain glyphs for these codepoints: \u{9}` into the log). How
/// obvious that looks just depends on the font's `.notdef` — blank on some,
/// a hollow box on others — so it is not a per-platform or per-font problem.
///
/// Tabs expand to the next tab stop (not a fixed run of spaces) so indentation
/// lines up the way the file's author saw it; `\r` is dropped so CRLF files do
/// not end every line with a box; other controls are dropped outright. Returns
/// `Cow::Borrowed` when there is nothing to change, which is the common case.
fn sanitize_preview_text(text: &str) -> Cow<'_, str> {
    if !text
        .chars()
        .any(|ch| ch != '\n' && (ch == '\t' || ch.is_control()))
    {
        return Cow::Borrowed(text);
    }

    let mut out = String::with_capacity(text.len());
    let mut column = 0usize;
    for ch in text.chars() {
        match ch {
            '\n' => {
                out.push('\n');
                column = 0;
            }
            '\t' => {
                let advance = PREVIEW_TAB_WIDTH - (column % PREVIEW_TAB_WIDTH);
                for _ in 0..advance {
                    out.push(' ');
                }
                column += advance;
            }
            ch if ch.is_control() => {}
            ch => {
                out.push(ch);
                column += 1;
            }
        }
    }
    Cow::Owned(out)
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
    // Sanitize before highlighting so byte offsets in the highlight result line
    // up with what is actually drawn.
    let text = sanitize_preview_text(text);
    let text = text.as_ref();
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

pub(crate) fn right_sidebar_file_row_metrics(
    ui_metrics: RenderMetrics,
) -> RightSidebarFileRowMetrics {
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

/// Whether one element of a scrolling row -- an icon, a line of text -- may be
/// painted right now.
///
/// These are quads the sidebar cannot scissor, so a list hides their overflow
/// with opaque masks instead: one covering the strip above `list_top`, one
/// covering `panel_bottom` upwards. An element is therefore painted whenever
/// part of it is inside the list *and* whatever hangs outside lands in a mask.
/// That is what makes a list scroll steplessly: an element straddling an edge
/// is drawn and then cut, rather than withheld until it fits.
///
/// The two mask bands must each be at least as tall as the tallest element, or
/// the caller silently goes back to withholding elements near the edges.
pub(crate) fn sidebar_row_element_visible(
    elem_y: usize,
    elem_height: usize,
    mask_top: usize,
    list_top: usize,
    panel_bottom: usize,
) -> bool {
    let elem_bottom = elem_y.saturating_add(elem_height);
    elem_y >= mask_top && elem_bottom > list_top && elem_bottom <= panel_bottom
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

/// Turn a finished transfer into the status its row will show. Keeps the
/// "something was left behind" case from being flattened into a plain failure,
/// which is the difference between the user knowing to go delete a partial
/// file and finding it months later.
fn transfer_status_from_result(
    result: &Result<u64, crate::termwindow::remote_files::TransferFailure>,
    done_detail: &str,
) -> RemoteTransferStatus {
    match result {
        Ok(_) => RemoteTransferStatus::Done(done_detail.to_string()),
        Err(failure) => match &failure.leftover {
            Some(leftover) => RemoteTransferStatus::FailedWithLeftover {
                message: failure.message.clone(),
                leftover: leftover.clone(),
            },
            None => RemoteTransferStatus::Failed(failure.message.clone()),
        },
    }
}

/// Upload a planned tree, stopping at the first failure.
///
/// Entries arrive parents-first, so each directory exists before anything is
/// placed in it. Cancellation is checked per entry rather than only per 256KiB
/// chunk: a tree of small files never reaches the chunk check, so a cancel
/// would otherwise look like a hang.
async fn upload_tree(
    backend: &dyn crate::termwindow::remote_files::RemoteFileBackend,
    directory: &RemotePath,
    entries: &[crate::termwindow::transfer_walk::TransferEntry],
    progress: &RemoteTransferProgress,
) -> Result<u64, crate::termwindow::remote_files::TransferFailure> {
    use crate::termwindow::remote_files::TransferFailure;

    let mut uploaded = 0u64;
    for (index, entry) in entries.iter().enumerate() {
        if progress.is_canceled() {
            return Err(TransferFailure::new(format!(
                "{REMOTE_TRANSFER_CANCELED} — stopped after {index} of {}",
                entries.len()
            )));
        }
        // Rebuild the remote path one component at a time: a local relative
        // path may hold separators this platform accepts but SFTP must not see
        // spliced in, and `join_name` rejects `..` and empty components.
        let mut destination = directory.clone();
        for component in entry.relative.components() {
            let Some(name) = component.as_os_str().to_str() else {
                return Err(TransferFailure::new(format!(
                    "{} is not a valid name to send",
                    entry.relative.display()
                )));
            };
            destination = destination
                .join_name(name)
                .map_err(|err| TransferFailure::new(err))?;
        }

        match entry.kind {
            TransferEntryKind::Directory => {
                backend
                    .create_directory(destination)
                    .await
                    .map_err(|message| {
                        TransferFailure::new(format!(
                            "{message} — stopped after {index} of {}",
                            entries.len()
                        ))
                    })?;
            }
            TransferEntryKind::File => {
                // Overwriting is not offered yet for remote folders, so a name
                // already in use stops the transfer rather than replacing
                // something the user never agreed to lose.
                let written = backend
                    .upload_file(entry.source.clone(), destination, progress.clone(), false)
                    .await
                    .map_err(|mut failure| {
                        failure.message = format!(
                            "{} — stopped after {index} of {}",
                            failure.message,
                            entries.len()
                        );
                        failure
                    })?;
                uploaded = uploaded.saturating_add(written);
            }
        }
        progress.finish_item();
    }
    Ok(uploaded)
}

fn display_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| path.display().to_string())
}

/// Whether the pane at `domain_id` is a session on the same host that
/// `source` uploads to — a ClientDomain of the same name, or the SSH domain
/// registered for the same saved host.
fn pane_domain_matches_remote_source(
    domain_id: mux::domain::DomainId,
    source: &workspace_threads::RemoteFilesSource,
) -> bool {
    let Some(domain) = Mux::get().get_domain(domain_id) else {
        return false;
    };
    let name = domain.domain_name();
    match source {
        workspace_threads::RemoteFilesSource::ClientDomain(client) => name == client,
        workspace_threads::RemoteFilesSource::SshHost(host_id) => {
            crate::ssh_hosts::host_spec(host_id)
                .map(|spec| crate::ssh_hosts::ssh_domain_name(&spec) == name)
                .unwrap_or(false)
        }
    }
}

/// Where a terminal drop should upload, given the setting and (when the
/// setting asks for it) the pane's reported working directory. Pure, so the
/// fallback rules stay testable: `cwd` without a usable cwd must degrade to
/// the default folder, never fail the drop.
/// Screenshot-style name for an image pasted from the clipboard. The dots in
/// the timestamp are why the collision logic splits extensions on the LAST
/// dot; a pasted image re-pasted lands as `… (1).png` like any other file.
fn pasted_image_file_name(now: chrono::DateTime<chrono::Local>) -> String {
    format!("Pasted {}.png", now.format("%Y-%m-%d at %H.%M.%S"))
}

/// Bring a pasted clipboard image to PNG. PNG passes through untouched;
/// Windows hands us BMP and macOS can hand us TIFF, both of which decode
/// and re-encode here.
fn encode_pasted_image_png(
    format: window::ClipboardImageFormat,
    bytes: Vec<u8>,
) -> Result<Vec<u8>, String> {
    match format {
        window::ClipboardImageFormat::Png => Ok(bytes),
        window::ClipboardImageFormat::Bmp | window::ClipboardImageFormat::Tiff => {
            let decoded = image::load_from_memory(&bytes)
                .map_err(|err| format!("Could not decode the pasted image: {err}"))?;
            let mut png = std::io::Cursor::new(Vec::new());
            decoded
                .write_to(&mut png, image::ImageFormat::Png)
                .map_err(|err| format!("Could not convert the pasted image to PNG: {err}"))?;
            Ok(png.into_inner())
        }
    }
}

fn stage_pasted_image(
    format: window::ClipboardImageFormat,
    bytes: Vec<u8>,
    name: &str,
) -> Result<StagedPastedImage, String> {
    let png = encode_pasted_image_png(format, bytes)?;
    let directory = tempfile::Builder::new()
        .prefix("thinkterm-pasted-image-")
        .tempdir()
        .map_err(|err| format!("Could not create a private staging directory: {err}"))?;

    #[cfg(unix)]
    fs::set_permissions(
        directory.path(),
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .map_err(|err| format!("Could not protect the staging directory: {err}"))?;

    let path = directory.path().join(name);
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&path)
        .map_err(|err| format!("Could not create the private staged image: {err}"))?;
    file.write_all(&png)
        .map_err(|err| format!("Could not write the staged image: {err}"))?;
    #[cfg(unix)]
    fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o600))
        .map_err(|err| format!("Could not protect the staged image: {err}"))?;

    Ok(StagedPastedImage {
        directory,
        path,
        #[cfg(test)]
        worker_thread_id: std::thread::current().id(),
    })
}

fn spawn_pasted_image_staging(
    format: window::ClipboardImageFormat,
    bytes: Vec<u8>,
    name: String,
) -> promise::spawn::Task<anyhow::Result<StagedPastedImage>> {
    promise::spawn::spawn_into_new_thread(move || {
        stage_pasted_image(format, bytes, &name).map_err(anyhow::Error::msg)
    })
}

fn resolve_drop_destination(setting: &str, cwd: Option<&str>) -> String {
    if setting == crate::native_settings::REMOTE_DROP_DESTINATION_CWD {
        match cwd {
            Some(cwd) if !cwd.trim().is_empty() => cwd.to_string(),
            _ => crate::native_settings::DEFAULT_REMOTE_DROP_DESTINATION.to_string(),
        }
    } else {
        setting.to_string()
    }
}

/// First free name for an uploaded file, browser-style: the file's own name,
/// then ` (1)`, ` (2)`… before the extension. The probe is advisory — the
/// upload's exclusive create remains the authority — but it turns the common
/// re-drop of the same screenshot into a rename instead of a failure.
async fn pick_free_remote_name(
    backend: &dyn crate::termwindow::remote_files::RemoteFileBackend,
    directory: &RemotePath,
    file_name: &str,
) -> Result<RemotePath, String> {
    for candidate in download_name_candidates(file_name) {
        let remote = directory.join_name(&candidate)?;
        if !backend.exists(remote.clone()).await? {
            return Ok(remote);
        }
    }
    Err(format!(
        "Unable to find a free name for {file_name} in {}",
        directory.as_str()
    ))
}

/// Delete a planned remote tree — children first, the root folder last —
/// returning how many things were removed.
///
/// Stops at the first failure, keeping the count honest: what was already
/// deleted is gone, and the message says how far it got. There is no rollback
/// to offer; inventing one would mean re-creating files whose bytes no longer
/// exist anywhere.
async fn delete_remote_tree(
    backend: &dyn crate::termwindow::remote_files::RemoteFileBackend,
    root: &RemotePath,
    entries: &[RemoteWalkEntry],
    progress: &RemoteTransferProgress,
) -> Result<u64, String> {
    let total = entries.len() + 1;
    let mut removed = 0u64;
    // The plan is parents-first; deleting must be children-first, or every
    // rmdir would find its directory still occupied.
    for entry in entries.iter().rev() {
        if progress.is_canceled() {
            return Err(format!(
                "{REMOTE_TRANSFER_CANCELED} — stopped after {removed} of {total}"
            ));
        }
        let result = if entry.is_directory() {
            backend.remove_directory(entry.path.clone()).await
        } else {
            // Files, symlinks and specials all go through unlink.
            backend.remove_file(entry.path.clone()).await
        };
        if let Err(message) = result {
            return Err(format!("{message} — stopped after {removed} of {total}"));
        }
        removed += 1;
        progress.finish_item();
    }
    if progress.is_canceled() {
        return Err(format!(
            "{REMOTE_TRANSFER_CANCELED} — stopped after {removed} of {total}"
        ));
    }
    backend
        .remove_directory(root.clone())
        .await
        .map_err(|message| format!("{message} — stopped after {removed} of {total}"))?;
    progress.finish_item();
    Ok(removed + 1)
}

/// Download a planned remote tree into a freshly reserved folder under
/// `downloads_dir`, returning where it landed.
///
/// The reservation (`fs::create_dir`, exclusive) is what makes the rest
/// simple: everything under the new folder is new by construction, so files
/// are created exclusively through the pinned [`DestinationRoot`] handle with
/// no staging names — a failed file is deleted through that same handle, and
/// what already landed is kept and reported, matching the local copy's
/// stop-and-say-how-far semantics.
async fn download_remote_tree(
    backend: &dyn crate::termwindow::remote_files::RemoteFileBackend,
    downloads_dir: &Path,
    folder_name: &str,
    entries: &[RemoteWalkEntry],
    progress: &RemoteTransferProgress,
) -> Result<PathBuf, TransferFailure> {
    if let Err(err) = fs::create_dir_all(downloads_dir) {
        return Err(TransferFailure::new(format!(
            "Unable to use {}: {err}",
            downloads_dir.display()
        )));
    }
    let root = DestinationRoot::open(downloads_dir).map_err(|err| {
        TransferFailure::new(format!("Unable to use {}: {err}", downloads_dir.display()))
    })?;
    let Some(destination) = reserve_download_directory(downloads_dir, folder_name, |candidate| {
        candidate
            .file_name()
            .is_some_and(|name| root.create_dir_exclusive(Path::new(name)).is_ok())
    }) else {
        return Err(TransferFailure::new(format!(
            "Unable to find a free name in {}",
            downloads_dir.display()
        )));
    };
    // The reserved name is a single component by construction. Keeping the
    // Downloads handle (not reopening the new directory by path) pins every
    // subsequent operation beneath the same verified root.
    let top = PathBuf::from(
        destination
            .file_name()
            .expect("a reserved directory always has a name"),
    );

    let total = entries.len();
    for (index, entry) in entries.iter().enumerate() {
        if progress.is_canceled() {
            return Err(failed_folder_download(
                &root,
                &top,
                &destination,
                format!("{REMOTE_TRANSFER_CANCELED} — stopped after {index} of {total}"),
            ));
        }
        let mut relative = top.clone();
        for component in &entry.components {
            relative.push(component);
        }
        if entry.is_directory() {
            if let Err(err) = root.create_dir(&relative) {
                return Err(failed_folder_download(
                    &root,
                    &top,
                    &destination,
                    format!(
                        "Unable to create {}: {err} — stopped after {index} of {total}",
                        root.join(&relative).display()
                    ),
                ));
            }
        } else {
            // Only plain files reach here: the Download walk skips symlinks
            // and specials by construction.
            let sink = match root.create_file(&relative) {
                Ok(sink) => sink,
                Err(err) => {
                    return Err(failed_folder_download(
                        &root,
                        &top,
                        &destination,
                        format!(
                            "Unable to create {}: {err} — stopped after {index} of {total}",
                            root.join(&relative).display()
                        ),
                    ))
                }
            };
            if let Err(mut failure) = backend
                .download_into(entry.path.clone(), sink, progress.clone())
                .await
            {
                failure.message = format!("{} — stopped after {index} of {total}", failure.message);
                // A half-written file must not survive wearing a whole file's
                // name. Deleted through the handle — cleanup runs exactly when
                // something has gone wrong, the worst moment to trust a path.
                if let Err(err) = root.remove_file(&relative) {
                    log::warn!(
                        "remote files: unable to clean up the partial download {}: {err:#}",
                        root.join(&relative).display()
                    );
                }
                return Err(failed_folder_download(
                    &root,
                    &top,
                    &destination,
                    failure.message,
                ));
            }
        }
        progress.finish_item();
    }
    Ok(destination)
}

/// Turn a folder-download error into an honest cleanup result.
///
/// Removing the top directory succeeds only while it is still empty. If any
/// earlier item landed (or cleanup of the current partial failed), keep the
/// useful partial tree and tell the user exactly where it is.
fn failed_folder_download(
    root: &DestinationRoot,
    top: &Path,
    destination: &Path,
    message: String,
) -> TransferFailure {
    let _ = root.remove_dir(top);
    let mut failure = TransferFailure::new(message);
    if local_path_is_occupied(destination) {
        failure.leftover = Some(destination.display().to_string());
    }
    failure
}

/// Walk every dropped path and find what already exists, all off the GUI
/// thread. Rejections are collected rather than thrown, so one bad source does
/// not sink the rest of a multi-select.
fn preflight_local_copy(
    sources: &[PathBuf],
    directory: &Path,
) -> crate::termwindow::transfer_walk::TransferPreflight {
    use crate::termwindow::transfer_walk::TransferPreflight;

    let mut preflight = TransferPreflight::default();
    // Two sources in one drop can plan the same destination name; the second
    // would then silently land on the first.
    let mut claimed: HashSet<PathBuf> = HashSet::new();

    for source in sources {
        let name = display_name(source);
        if source.is_dir() && destination_escapes_source(source, directory) {
            preflight
                .rejected
                .push((name, TransferPlanError::DestinationInsideSource.message()));
            continue;
        }
        match plan_transfer(source) {
            Ok(plan) if plan.entries.is_empty() => {
                preflight
                    .rejected
                    .push((name, "Nothing to copy".to_string()));
            }
            Ok(plan) => {
                preflight.skipped_symlinks += plan.skipped_symlinks;
                preflight.unreadable += plan.unreadable;
                let mut collision = None;
                for entry in &plan.entries {
                    // Directories take part in the claim too: a `foo/` from one
                    // source and a `foo` file from another want the same name,
                    // and only one of them can have it.
                    if !claimed.insert(entry.relative.clone()) {
                        collision = Some(entry.relative.clone());
                        break;
                    }
                    if entry.kind != TransferEntryKind::File {
                        continue;
                    }
                    let destination = directory.join(&entry.relative);
                    // Dropping a file back into the folder it already lives in
                    // would have `fs::copy` truncate the shared inode and then
                    // report success on the resulting empty file.
                    if is_same_file(&entry.source, &destination) {
                        collision = Some(entry.relative.clone());
                        break;
                    }
                    if destination.exists() {
                        preflight.conflicts.insert(entry.relative.clone());
                    }
                }
                if let Some(relative) = collision {
                    preflight.rejected.push((
                        name,
                        format!(
                            "{} is already where it would be copied to",
                            relative.display()
                        ),
                    ));
                    continue;
                }
                preflight.plans.push((source.clone(), plan));
            }
            Err(err) => preflight.rejected.push((name, err.message())),
        }
    }
    preflight
}

/// Bytes moved per read/write while copying locally. Small enough that a
/// cancel is noticed promptly even inside one very large file.
const LOCAL_COPY_CHUNK: usize = 512 * 1024;

/// Copy a planned tree, stopping at the first failure.
///
/// Stopping rather than continuing is deliberate: what has already landed is
/// kept, and the user is told how far it got. Rolling back would mean deleting
/// files to recover from an error — more dangerous than the error.
fn copy_entries_blocking(
    entries: &[crate::termwindow::transfer_walk::TransferEntry],
    root: &DestinationRoot,
    policy: OverwritePolicy,
    progress: &RemoteTransferProgress,
) -> Result<usize, String> {
    let mut copied = 0usize;
    for entry in entries {
        if progress.is_canceled() {
            return Err(format!(
                "{} — stopped after {copied} of {}",
                REMOTE_TRANSFER_CANCELED,
                entries.len()
            ));
        }
        let destination = root.join(&entry.relative);
        // A cheap, clear rejection before any work starts. The guarantee comes
        // from the handle-based descent below, not from this.
        if !destination_stays_within(root.path(), &destination) {
            return Err(format!(
                "{} would be written outside the destination folder",
                entry.relative.display()
            ));
        }

        match entry.kind {
            TransferEntryKind::Directory => {
                // Descends through directory handles, so a symlink planted at
                // any level fails rather than redirecting the write.
                root.create_dir(&entry.relative)
                    .map_err(|err| format!("Unable to create {}: {err}", destination.display()))?;
            }
            TransferEntryKind::File => {
                // Last line of defence against a file copied over itself.
                // Covers plain paths, symlink aliases, and hard links — any of
                // which would have the write destroy the source.
                if is_same_file(&entry.source, &destination) {
                    return Err(format!(
                        "{} is already at the destination",
                        entry.source.display()
                    ));
                }
                // Under NoClobber a file that appeared after the preflight was
                // never authorized for replacement; under SkipExisting the
                // user said to leave it. Both come back as a skip, decided by
                // the filesystem rather than by a check that could be raced.
                match copy_file_chunked(&entry.source, root, &entry.relative, policy, progress) {
                    Ok(true) => copied += 1,
                    Ok(false) => {}
                    Err(err) => {
                        return Err(format!(
                            "{err} — stopped after {copied} of {}",
                            entries.len()
                        ))
                    }
                }
            }
        }
        progress.finish_item();
    }
    Ok(copied)
}

/// Copy one file, checking for a cancel between chunks.
///
/// `fs::copy` would be shorter but blocks until the whole file is done, so a
/// cancel during a multi-gigabyte file is never seen and the transfer goes on
/// to report success.
/// Returns whether the file was actually written; `false` means the
/// destination was already taken and the policy said to leave it.
///
/// `relative` names the file inside `root`; both are needed because the write
/// descends from `root` through directory handles rather than resolving the
/// joined path, which is what stops a symlink swapped in mid-copy from
/// redirecting it.
fn copy_file_chunked(
    source: &Path,
    root: &DestinationRoot,
    relative: &Path,
    policy: OverwritePolicy,
    progress: &RemoteTransferProgress,
) -> Result<bool, String> {
    let destination = root.join(relative);
    let mut reader = fs::File::open(source)
        .map_err(|err| format!("Unable to read {}: {err}", source.display()))?;

    match policy {
        // Nothing may be displaced, so let the *filesystem* enforce that: an
        // exclusive create fails atomically if anything is already there, and
        // cannot follow a symlink sitting at that name. A check-then-create
        // would leave a window for a file to appear and be truncated.
        OverwritePolicy::NoClobber | OverwritePolicy::SkipExisting => {
            let writer = match root.create_file(relative) {
                Ok(writer) => writer,
                // Refusing is the whole point here, and it is reported as a
                // skip rather than an error — but only for this one reason,
                // so a permission problem still surfaces.
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => return Ok(false),
                Err(err) => {
                    return Err(format!("Unable to create {}: {err}", destination.display()))
                }
            };
            stream_into(&mut reader, writer, source, &destination, progress)
                .inspect_err(|_| {
                    // Through the handle as well: cleanup runs exactly when
                    // something has gone wrong, which is the worst moment to
                    // start trusting a path again.
                    let _ = root.remove_file(relative);
                })
                .map(|()| true)
        }
        // The user asked to replace. Build the replacement beside the target
        // and swap it in at the end: the original survives a failure or a
        // cancel, and the rename replaces the directory entry rather than
        // following a symlink that happens to occupy the name.
        OverwritePolicy::Replace => {
            let (staging_relative, writer) = create_staging_file(root, relative)?;
            let staging = root.join(&staging_relative);
            let outcome =
                stream_into(&mut reader, writer, source, &staging, progress).and_then(|()| {
                    root.rename(&staging_relative, relative).map_err(|err| {
                        format!("Unable to replace {}: {err}", destination.display())
                    })
                });
            if outcome.is_err() {
                // Through the handle, like every other write: cleanup runs
                // exactly when something has gone wrong, which is the worst
                // moment to start trusting a path again.
                let _ = root.remove_file(&staging_relative);
            }
            outcome.map(|()| true)
        }
    }
}

/// Create a uniquely named sibling of the destination to assemble a
/// replacement in, returning its path relative to `root`. A sibling, not a
/// temp directory, so the finishing rename stays on one filesystem and is
/// therefore atomic.
fn create_staging_file(
    root: &DestinationRoot,
    relative: &Path,
) -> Result<(PathBuf, fs::File), String> {
    let parent = relative.parent().unwrap_or_else(|| Path::new(""));
    let name = relative
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .ok_or_else(|| format!("{} has no file name", relative.display()))?;
    for attempt in 0..1_000u32 {
        let candidate = parent.join(format!(".{name}.thinkterm-{attempt}"));
        match root.create_file(&candidate) {
            Ok(file) => return Ok((candidate, file)),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => {
                return Err(format!(
                    "Unable to write beside {}: {err}",
                    relative.display()
                ))
            }
        }
    }
    Err(format!(
        "Unable to find a free staging name beside {}",
        relative.display()
    ))
}

/// Stream `reader` into `writer`, checking for a cancel between chunks.
fn stream_into(
    reader: &mut fs::File,
    mut writer: fs::File,
    source: &Path,
    written_to: &Path,
    progress: &RemoteTransferProgress,
) -> Result<(), String> {
    use std::io::Write;

    let mut buffer = vec![0u8; LOCAL_COPY_CHUNK];
    loop {
        if progress.is_canceled() {
            return Err(REMOTE_TRANSFER_CANCELED.to_string());
        }
        let read = reader
            .read(&mut buffer)
            .map_err(|err| format!("Unable to read {}: {err}", source.display()))?;
        if read == 0 {
            break;
        }
        writer
            .write_all(&buffer[..read])
            .map_err(|err| format!("Unable to write {}: {err}", written_to.display()))?;
    }
    writer
        .flush()
        .map_err(|err| format!("Unable to finish {}: {err}", written_to.display()))
}

/// The local counterpart of [`resolve_remote_drop_target`]. Same rule, and
/// deliberately the same shape so the two cannot drift: a directory row takes
/// the drop itself, a file row hands it to the directory holding it, anywhere
/// else inside the panel takes the root.
fn resolve_local_drop_target(
    items: &[UIItem],
    x: isize,
    y: isize,
    root: &Path,
    is_dir: impl Fn(&Path) -> Option<bool>,
) -> Option<PathBuf> {
    let item = items.iter().rev().find(|item| item.hit_test(x, y))?;
    match &item.item_type {
        UIItemType::RightSidebarFileRow(path) => match is_dir(path) {
            Some(true) => Some(path.clone()),
            Some(false) => Some(path.parent().map(Path::to_path_buf).unwrap_or_else(|| {
                // A row with no parent should not exist, but falling back to
                // the root is better than dropping the file somewhere odd.
                root.to_path_buf()
            })),
            // A row painted from a listing that has since been dropped:
            // guessing could put the file somewhere unintended.
            None => None,
        },
        UIItemType::RightSidebarBackground
        | UIItemType::RightSidebarFileFilter
        | UIItemType::RightSidebarFileRefresh
        | UIItemType::RightSidebarRemoteTransfer(_) => Some(root.to_path_buf()),
        _ => None,
    }
}

/// Where a drop at `(x, y)` would land, given the UI items from the last
/// paint. A directory row takes the file itself; a file row takes the
/// directory holding it, which is what "drop it next to this" means; anywhere
/// else inside the panel takes the root.
///
/// Split out from the window so the rule can be tested against a handful of
/// rectangles instead of a rendered frame.
fn resolve_remote_drop_target(
    items: &[UIItem],
    x: isize,
    y: isize,
    root: &RemotePath,
    kind_for_path: impl Fn(&RemotePath) -> Option<RemoteFileKind>,
) -> Option<RemotePath> {
    // Last painted wins, matching how a click resolves.
    let item = items.iter().rev().find(|item| item.hit_test(x, y))?;
    match &item.item_type {
        UIItemType::RightSidebarRemoteFileRow(path) => match kind_for_path(path) {
            Some(RemoteFileKind::Directory) => Some(path.clone()),
            // A symlink could point anywhere; treat it as an ordinary entry
            // and aim at the directory it is listed in.
            Some(_) => Some(path.parent().unwrap_or_else(|| root.clone())),
            None => None,
        },
        // Empty space inside the panel, or its chrome: the root is the only
        // directory the whole panel unambiguously refers to. The transfer
        // strip counts too — it sits inside the panel, so a drop landing on it
        // must not fall through and paste into the terminal behind.
        UIItemType::RightSidebarBackground
        | UIItemType::RightSidebarRemoteFileRefresh
        | UIItemType::RightSidebarRemoteFileConnect
        | UIItemType::RightSidebarRemoteTransfer(_) => Some(root.clone()),
        _ => None,
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
                raw_text: None,
            },
            Err(err) => RightSidebarLoadedFilePreview {
                lines: Vec::new(),
                image: None,
                message: Some(format!("Unable to load image preview: {err}")),
                truncated: false,
                raw_text: None,
            },
        };
    }

    let (text, message, truncated) = load_file_preview(path);
    let lines = if message.is_none() {
        preview_lines_from_text_with_cancellation(path, &text, use_dark_syntax_theme, cancellation)
    } else {
        Vec::new()
    };
    let raw_text = message.is_none().then_some(text);
    RightSidebarLoadedFilePreview {
        lines,
        image: None,
        message,
        truncated,
        raw_text,
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
                raw_text: None,
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
                        max_upscale: NOTE_IMAGE_MAX_UPSCALE,
                        natural_size: None,
                    }),
                    message: None,
                    truncated: false,
                    raw_text: None,
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
                raw_text: None,
            },
            Err(err) => RightSidebarLoadedFilePreview {
                lines: Vec::new(),
                image: None,
                message: Some(format!("Unable to decode image dimensions: {err:#}")),
                truncated: false,
                raw_text: None,
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
    let raw_text = message.is_none().then_some(text);
    RightSidebarLoadedFilePreview {
        lines,
        image: None,
        message,
        truncated: remote.truncated,
        raw_text,
    }
}

fn load_file_preview_image(path: &Path) -> anyhow::Result<RightSidebarFilePreviewImage> {
    let bytes = crate::bounded_file::read_regular(path, FILE_PREVIEW_IMAGE_MAX_BYTES)?;
    let encoded_bytes = bytes.len();
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
        encoded_bytes,
        max_upscale: NOTE_IMAGE_MAX_UPSCALE,
        natural_size: None,
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

/// Key for the shared index registry.
///
/// The gitignore flag is part of the key: two windows configured differently
/// must not hand each other an index built under the other policy.
type SharedFileIndexKey = (PathBuf, String, bool);

/// Process-wide registry of weak references to file indexes. Lets multiple
/// windows on the same workspace share a single `Arc<RightSidebarFileIndex>`
/// instead of each scanning and holding its own copy. Only weak refs live here,
/// so an index is freed the moment the last window drops its strong ref (e.g.
/// via the idle-release path).
fn shared_file_index_registry(
) -> &'static Mutex<HashMap<SharedFileIndexKey, Weak<RightSidebarFileIndex>>> {
    static REGISTRY: OnceLock<Mutex<HashMap<SharedFileIndexKey, Weak<RightSidebarFileIndex>>>> =
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
    respect_gitignore: bool,
) -> Result<Arc<RightSidebarFileIndex>, String> {
    let key = (
        root.to_path_buf(),
        project_name.to_string(),
        respect_gitignore,
    );
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
        respect_gitignore,
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
    respect_gitignore: bool,
) -> Result<Arc<RightSidebarFileIndex>, String> {
    let index = Arc::new(build_right_sidebar_file_index_with_cancel(
        root,
        project_name,
        cancel,
        respect_gitignore,
    )?);
    if let Ok(mut registry) = shared_file_index_registry().lock() {
        registry.insert(
            (
                root.to_path_buf(),
                project_name.to_string(),
                respect_gitignore,
            ),
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
    respect_gitignore: bool,
) -> Result<RightSidebarFileIndex, String> {
    let cancel = AtomicBool::new(false);
    build_right_sidebar_file_index_with_cancel(root, project_name, &cancel, respect_gitignore)
}

/// Drive the lazy loader to a fixed point, the way successive paints would:
/// build rows, read whatever they reported missing, repeat. Returns the cache so
/// tests can assert on *which* directories were touched, not just the rows.
#[cfg(test)]
fn load_dir_cache_for_test(root: &Path, expanded: &HashSet<String>) -> RightSidebarFileDirCache {
    let mut cache = RightSidebarFileDirCache::default();
    loop {
        let (_, missing) =
            right_sidebar_file_browse_rows_from_dir_cache(&cache, root, "Project", expanded);
        if missing.is_empty() {
            break;
        }
        for dir in missing {
            match read_right_sidebar_dir(&dir) {
                Ok(children) => cache.insert(dir, children),
                Err(problem) => cache.insert_failure(dir, problem),
            }
        }
    }
    cache
}

fn build_right_sidebar_file_index_with_cancel(
    root: &Path,
    project_name: &str,
    cancel: &AtomicBool,
    respect_gitignore: bool,
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
        name_char_bag: RightSidebarFileCharBag::from_str(project_name),
        char_bag: RightSidebarFileCharBag::from_str(project_name),
    }];

    // The same rules a remote project is indexed with (`thinkterm list-files`).
    for entry in thinkterm_file_index::project_walker(root, respect_gitignore)
        .build()
        .skip(1)
    {
        if entries.len() >= FILE_INDEX_ENTRY_LIMIT {
            log::warn!(
                "File search index for {} hit the {} entry limit; \
                 some files will not be findable by name",
                root.display(),
                FILE_INDEX_ENTRY_LIMIT
            );
            break;
        }
        if cancel.load(AtomicOrdering::Relaxed) {
            return Err("File indexing canceled".to_string());
        }
        let Ok(entry) = entry else {
            continue;
        };
        let is_dir = entry.file_type().is_some_and(|kind| kind.is_dir());
        let path = entry.path().to_path_buf();
        let name = entry.file_name().to_string_lossy().to_string();
        let display_path = path
            .strip_prefix(root)
            .map(path_to_display_string)
            .unwrap_or_else(|_| name.clone());
        let name_char_bag = RightSidebarFileCharBag::from_str(&name);
        entries.push(RightSidebarFileIndexEntry {
            path,
            name,
            display_path: display_path.clone(),
            is_dir,
            name_char_bag,
            char_bag: RightSidebarFileCharBag::from_str(&display_path),
        });
    }

    if cancel.load(AtomicOrdering::Relaxed) {
        return Err("File indexing canceled".to_string());
    }

    Ok(RightSidebarFileIndex::new(entries))
}

/// Whether a directory that was just read differs from what the cache holds.
///
/// Pulled out of the apply loop so the one case that is easy to get wrong is
/// testable: a folder cached as *refused* is stored as empty, so a later read
/// that succeeds and finds it genuinely empty compares equal on children alone.
/// Without also comparing the failure, that success would not be written and
/// the error card would survive the very re-read that proved the problem gone
/// -- which is exactly the "I granted the permission and nothing happened"
/// path.
fn dir_load_changes_cache(
    cache: &RightSidebarFileDirCache,
    dir: &Path,
    result: &Result<Vec<RightSidebarFileDirEntry>, FolderProblem>,
) -> bool {
    match result {
        Ok(children) => {
            cache.children(dir) != Some(children.as_slice()) || cache.failure(dir).is_some()
        }
        // `is_loaded` covers the first failure for a directory the cache has
        // never held: the problem matches nothing, but there is still an empty
        // entry to write so the row builder stops re-queueing the read.
        Err(problem) => cache.failure(dir) != Some(*problem) || !cache.is_loaded(dir),
    }
}

/// Read one directory's children, applying the same skip list and ordering the
/// full-project index used, so a lazily-built tree renders identically to the
/// eagerly-walked one.
fn read_right_sidebar_dir(dir: &Path) -> Result<Vec<RightSidebarFileDirEntry>, FolderProblem> {
    let reader = match fs::read_dir(dir) {
        Ok(reader) => reader,
        Err(err) => {
            // Still cached (as a failure) rather than retried on every paint,
            // but no longer cached as "this folder is empty": a macOS TCC
            // denial and a genuinely empty project used to render identically,
            // which told the user their files were gone.
            return Err(FolderProblem::from_read_dir_kind(err.kind()));
        }
    };
    let mut children = Vec::new();
    for entry in reader.flatten() {
        // `file_type` here is `lstat`-like, matching the old walker's
        // `follow_links(false)`: a symlink to a directory stays a leaf.
        let is_dir = entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false);
        let name = entry.file_name().to_string_lossy().to_string();
        if is_dir && should_skip_file_index_dir(&name) {
            continue;
        }
        children.push(RightSidebarFileDirEntry {
            path: entry.path(),
            name,
            is_dir,
        });
    }
    children.sort_by(right_sidebar_dir_entry_cmp);
    Ok(children)
}

fn right_sidebar_dir_entry_cmp(
    a: &RightSidebarFileDirEntry,
    b: &RightSidebarFileDirEntry,
) -> Ordering {
    match (a.is_dir, b.is_dir) {
        (true, false) => Ordering::Less,
        (false, true) => Ordering::Greater,
        _ => naturalish_cmp(&a.name, &b.name),
    }
}

/// Build the visible rows from whatever directories have been read so far.
///
/// Returns the rows plus the directories that are expanded but not yet loaded,
/// which the caller turns into read requests. An unloaded folder simply renders
/// with no children for a frame; nothing blocks on disk.
fn right_sidebar_file_browse_rows_from_dir_cache(
    cache: &RightSidebarFileDirCache,
    root: &Path,
    project_name: &str,
    expanded: &HashSet<String>,
) -> (Vec<RightSidebarFileTreeRow>, Vec<PathBuf>) {
    let mut rows = vec![RightSidebarFileTreeRow {
        path: root.to_path_buf(),
        name: project_name.to_string(),
        depth: 0,
        is_dir: true,
        is_expanded: true,
    }];
    let mut missing = Vec::new();
    collect_dir_cache_rows(cache, root, 1, expanded, &mut rows, &mut missing);
    (rows, missing)
}

fn collect_dir_cache_rows(
    cache: &RightSidebarFileDirCache,
    dir: &Path,
    depth: usize,
    expanded: &HashSet<String>,
    rows: &mut Vec<RightSidebarFileTreeRow>,
    missing: &mut Vec<PathBuf>,
) {
    let Some(children) = cache.children(dir) else {
        missing.push(dir.to_path_buf());
        return;
    };
    for child in children {
        if rows.len() >= FILE_TREE_ROW_LIMIT {
            return;
        }
        let is_expanded = expanded.contains(&path_key(&child.path));
        rows.push(RightSidebarFileTreeRow {
            path: child.path.clone(),
            name: child.name.clone(),
            depth,
            is_dir: child.is_dir,
            is_expanded,
        });
        if child.is_dir && is_expanded {
            collect_dir_cache_rows(cache, &child.path, depth + 1, expanded, rows, missing);
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

/// What a remote project's search index is for: the connection, the root it
/// lists and the ignore rule it was listed under.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RemoteFileSearchKey {
    source_key: String,
    root: RemotePath,
    respect_gitignore: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) enum RemoteFileSearchStatus {
    #[default]
    Idle,
    Indexing,
    Ready,
    /// The remote host has no `thinkterm` that can list files.
    NeedsUpdate,
    Failed(String),
}

/// Search for a remote project: the listing `thinkterm list-files` sends back,
/// indexed and searched here exactly as a local project's index is. Held only
/// while it is used; released with the rest of the Files memory.
#[derive(Default)]
pub(crate) struct RemoteFileSearchState {
    key: Option<RemoteFileSearchKey>,
    status: RemoteFileSearchStatus,
    index: Option<Arc<RightSidebarFileIndex>>,
    built_at: Option<Instant>,
    /// Bumped on every reset, so a listing requested before it is dropped.
    generation: u64,
    /// The query `rows` answer, or will once the search running for it
    /// lands.
    query: String,
    /// Shared with each frame's painter rather than copied into it.
    rows: Arc<Vec<RemoteFileRow>>,
    /// Set to stop the search running off the UI thread; `Some` while one is.
    search_cancel: Option<Arc<AtomicBool>>,
}

impl RemoteFileSearchState {
    pub(crate) fn release(&mut self) {
        self.cancel_search();
        let generation = self.generation.wrapping_add(1);
        *self = Self {
            generation,
            ..Self::default()
        };
    }

    fn cancel_search(&mut self) {
        if let Some(cancel) = self.search_cancel.take() {
            cancel.store(true, AtomicOrdering::Relaxed);
        }
    }

    fn kind_for_path(&self, path: &RemotePath) -> Option<RemoteFileKind> {
        self.rows
            .iter()
            .find(|row| &row.entry.path == path)
            .map(|row| row.entry.kind)
    }
}

/// Index a remote listing the way a local project is indexed. Paths stay
/// relative to the root; entry 0 stands for the root itself, as search skips it.
fn remote_file_index_from_listing(
    project_name: &str,
    listing: &thinkterm_file_index::Listing,
) -> RightSidebarFileIndex {
    let mut entries = Vec::with_capacity(listing.entries.len() + 1);
    entries.push(RightSidebarFileIndexEntry {
        path: PathBuf::new(),
        name: project_name.to_string(),
        display_path: project_name.to_string(),
        is_dir: true,
        name_char_bag: RightSidebarFileCharBag::from_str(project_name),
        char_bag: RightSidebarFileCharBag::from_str(project_name),
    });
    for entry in &listing.entries {
        let name = entry
            .path
            .rsplit('/')
            .next()
            .unwrap_or(&entry.path)
            .to_string();
        entries.push(RightSidebarFileIndexEntry {
            path: PathBuf::from(&entry.path),
            name_char_bag: RightSidebarFileCharBag::from_str(&name),
            name,
            display_path: entry.path.clone(),
            is_dir: entry.is_dir,
            char_bag: RightSidebarFileCharBag::from_str(&entry.path),
        });
    }
    RightSidebarFileIndex::new(entries)
}

/// `relative` under `root`, one component at a time, so a listing cannot name
/// anything outside the root (`..`, `.` and empty components are refused).
fn remote_path_under(root: &RemotePath, relative: &str) -> Option<RemotePath> {
    let mut path = root.clone();
    for component in relative.split('/') {
        path = path.join_name(component).ok()?;
    }
    Some(path)
}

fn remote_file_search_rows(
    index: &RightSidebarFileIndex,
    root: &RemotePath,
    query: &str,
    cancel: &AtomicBool,
) -> Vec<RemoteFileRow> {
    search_right_sidebar_file_index(index, query, cancel)
        .into_iter()
        .filter_map(|row| {
            let path = remote_path_under(root, &row.name)?;
            Some(RemoteFileRow {
                entry: RemoteFileEntry {
                    path,
                    name: row.name,
                    kind: if row.is_dir {
                        RemoteFileKind::Directory
                    } else {
                        RemoteFileKind::File
                    },
                    size: None,
                },
                depth: 0,
                expanded: false,
            })
        })
        .collect()
}

/// Where the pointer is for the right sidebar's hover machine: over the panel
/// as presented, over the right-edge strip or the sidebar button that arm a
/// reveal, in the wider band just inside the strip that keeps a running dwell
/// alive, or away. The panel is checked first: once revealed, the strip is
/// inside it.
fn right_sidebar_hover_zone(
    x: isize,
    y: isize,
    panel: Option<RightSidebarRect>,
    hot_zone: Option<(usize, usize, usize, usize)>,
    sticky_width: usize,
    over_toggle: bool,
) -> crate::termwindow::sidebar_hover::PointerZone {
    use crate::termwindow::sidebar_hover::PointerZone;
    let in_panel = panel.is_some_and(|rect| {
        x >= rect.x as isize
            && x < rect.x.saturating_add(rect.width) as isize
            && y >= rect.y as isize
            && y < rect.y.saturating_add(rect.height) as isize
    });
    if in_panel {
        return PointerZone::Panel;
    }
    if over_toggle {
        return PointerZone::HotZone;
    }
    let Some((zx, zy, zw, zh)) = hot_zone else {
        return PointerZone::Away;
    };
    let right = zx.saturating_add(zw) as isize;
    if y < zy as isize || y >= zy.saturating_add(zh) as isize || x >= right {
        return PointerZone::Away;
    }
    if x >= zx as isize {
        PointerZone::HotZone
    } else if x >= right - sticky_width as isize {
        PointerZone::NearHotZone
    } else {
        PointerZone::Away
    }
}

/// See `TermWindow::markdown_preview_document`. `project_root` bounds where a
/// local file's images may come from; a file outside it is bounded by its own
/// folder.
fn markdown_preview_document(
    path: &str,
    text: String,
    remote_relative: Option<String>,
    project_root: Option<&Path>,
) -> crate::markdown_editor::VaultDocument {
    // A session stats its path; a remote path means nothing on this machine,
    // so a remote preview's session has none.
    let session_path = if remote_relative.is_some() {
        PathBuf::new()
    } else {
        PathBuf::from(path)
    };
    let session = Arc::new(parking_lot::Mutex::new(
        crate::markdown_editor::MarkdownDocumentSession::new(
            "file-preview".to_string(),
            session_path,
            text,
        ),
    ));
    if let Some(relative_path) = remote_relative {
        // No local root: nothing is read from this machine for it. The path
        // relative to the remote project lets its wiki links resolve among
        // the remote files.
        return crate::markdown_editor::VaultDocument {
            vault_root: PathBuf::new(),
            relative_path,
            document_path: PathBuf::new(),
            session,
        };
    }
    let document_path = PathBuf::from(path);
    let root = project_root
        .filter(|root| document_path.starts_with(root))
        .map(Path::to_path_buf)
        .or_else(|| document_path.parent().map(Path::to_path_buf))
        .unwrap_or_default();
    let relative_path = document_path
        .strip_prefix(&root)
        .map(path_to_display_string)
        .unwrap_or_default();
    crate::markdown_editor::VaultDocument {
        vault_root: root,
        relative_path,
        document_path,
        session,
    }
}

/// The files a preview's wiki links resolve among; see
/// `TermWindow::preview_link_files`.
pub(crate) enum PreviewLinkFiles {
    /// Relative to the root, `/`-separated.
    Listed(Arc<Vec<String>>),
    WalkProject {
        respect_gitignore: bool,
    },
    /// Only the files directly in the root folder.
    Folder,
}

/// A previewed file is not in a Notebook: its wiki links resolve among the
/// project's files, never by walking everything under the root. A remote file
/// has no local root, so nothing of this machine's is read for it.
fn resolve_preview_wiki_links(
    projection: &mut crate::markdown_editor::MarkdownProjection,
    root: &Path,
    relative_path: &str,
    files: PreviewLinkFiles,
) {
    let paths = match files {
        PreviewLinkFiles::Listed(paths) => {
            Arc::try_unwrap(paths).unwrap_or_else(|paths| paths.as_ref().clone())
        }
        PreviewLinkFiles::WalkProject { respect_gitignore } => {
            let Ok(listing) = thinkterm_file_index::list_project(
                root,
                respect_gitignore,
                thinkterm_file_index::ENTRY_LIMIT,
                Instant::now() + Duration::from_secs(5),
            ) else {
                return;
            };
            listing
                .entries
                .into_iter()
                .filter(|entry| !entry.is_dir)
                .map(|entry| entry.path)
                .collect()
        }
        PreviewLinkFiles::Folder => match fs::read_dir(root) {
            Ok(entries) => entries
                .flatten()
                .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
                .filter_map(|entry| entry.file_name().into_string().ok())
                .take(thinkterm_file_index::ENTRY_LIMIT)
                .collect(),
            Err(_) => return,
        },
    };
    if root.as_os_str().is_empty() {
        projection.resolve_links_among(None, paths, relative_path);
        return;
    }
    let Ok(root) = root.canonicalize() else {
        return;
    };
    projection.resolve_links_among(Some(&root), paths, relative_path);
}

/// `path` relative to `root` with `/` separators, when it lies under it.
fn remote_relative_path(root: &RemotePath, path: &RemotePath) -> Option<String> {
    let root = root.as_str().trim_end_matches('/');
    let rest = path.as_str().strip_prefix(root)?.strip_prefix('/')?;
    (!rest.is_empty()).then(|| rest.to_string())
}

fn is_markdown_extension(extension: &str) -> bool {
    extension.eq_ignore_ascii_case("md") || extension.eq_ignore_ascii_case("markdown")
}

fn should_skip_file_index_dir(name: &str) -> bool {
    thinkterm_file_index::should_skip_dir(name)
}

fn path_to_display_string(path: &Path) -> String {
    thinkterm_file_index::display_path(path)
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

/// Wrap a filesystem path to a width, breaking after separators.
///
/// The generic wrapper breaks wherever the width runs out, which cuts a
/// component mid-name ("/Desktop/or" / "der_system") -- legible as prose,
/// baffling as a path. Components are kept whole and lines end on their
/// separators; only a single component wider than the whole line falls back to
/// the character wrap. When the path still will not fit `max_lines`, the LAST
/// lines win with a leading ellipsis: the tail of a path is the half the user
/// recognises.
///
/// `measure` returns the fraction of the available width a segment occupies,
/// the same contract as [`wrap_snippet_text_for_width`].
pub(crate) fn wrap_path_for_width<F>(path: &str, max_lines: usize, mut measure: F) -> Vec<String>
where
    F: FnMut(&str) -> f32,
{
    let max_lines = max_lines.max(1);
    let mut segments: Vec<&str> = Vec::new();
    let mut start = 0;
    for (index, ch) in path.char_indices() {
        if ch == '/' || ch == '\\' {
            segments.push(&path[start..index + ch.len_utf8()]);
            start = index + ch.len_utf8();
        }
    }
    if start < path.len() {
        segments.push(&path[start..]);
    }

    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();
    for segment in segments {
        let candidate = format!("{current}{segment}");
        if !current.is_empty() && measure(&candidate) > 1.0 {
            lines.push(std::mem::take(&mut current));
            current = segment.to_string();
        } else {
            current = candidate;
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }

    // A single component wider than the line still has to break somewhere.
    let mut lines: Vec<String> = lines
        .into_iter()
        .flat_map(|line| {
            if measure(&line) > 1.0 {
                wrap_snippet_text_for_width(&line, max_lines, false, &mut measure)
            } else {
                vec![line]
            }
        })
        .collect();

    if lines.len() > max_lines {
        lines = lines.split_off(lines.len() - max_lines);
        if let Some(first) = lines.first_mut() {
            first.insert(0, '\u{2026}');
        }
    }
    lines
}

pub(crate) fn wrap_snippet_text_for_width<F>(
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
        copy_entries_blocking, copy_file_chunked, dir_load_changes_cache, download_name_candidates,
        encode_pasted_image_png, failed_folder_download, file_preview_close_requires_reflow,
        file_release_action, file_row_placement, full_line_colors_by_byte,
        image_pixels_within_preview_budget, load_dir_cache_for_test, load_file_preview,
        load_file_preview_image, naturalish_cmp, note_code_highlight_key,
        note_code_highlight_lines, note_code_row_height, note_image_display_size,
        note_open_pending_for_vault, note_release_action, open_with_candidate_allowed,
        pasted_image_file_name, path_key, pick_free_remote_name, preflight_local_copy,
        preview_line_count, preview_lines_from_text, preview_plain_lines_from_text,
        preview_text_range, preview_visible_colored, preview_visible_line_range,
        problem_for_read_dir, remote_lease_failure_disposition, rescan_plan,
        resolve_drop_destination, resolve_local_drop_target, resolve_remote_drop_target,
        right_sidebar_file_browse_rows_from_dir_cache, right_sidebar_file_row_metrics,
        right_sidebar_open_with_cache_key, sanitize_preview_text, scrollable_note_table_columns,
        search_right_sidebar_file_index, sidebar_message_layout, sidebar_row_element_visible,
        snippet_cursor_visible, sorted_open_with_candidates, spawn_pasted_image_staging,
        stage_pasted_image, terminal_paste_snapshot_mismatch, virtual_note_line_range,
        visible_code_block_rounded_edges, visible_file_row_range, wrap_snippet_text_for_width,
        FileReleaseAction, FileRowPlacement, NoteApproximateTextMetrics, NoteCodeHighlightEntry,
        NoteCodeHighlightState, NoteReleaseAction, NoteVaultProblem, RemoteLeaseFailureDisposition,
        TerminalPasteSnapshotMismatch, TerminalPasteTarget, FILE_PREVIEW_MAX_BYTES,
        LOCAL_COPY_CHUNK, NOTE_CODE_BLOCK_RADIUS, NOTE_CODE_HEADER_HEIGHT,
    };
    use super::{FolderProblem, RightSidebarFileDirCache, RightSidebarFileDirEntry};
    use crate::markdown_editor::{NoteLineGeometry, ProjectedCodeBlock};
    use crate::termwindow::remote_files::{
        RemoteFileBytes, RemoteFileKind, RemotePath, RemoteTransferProgress,
        REMOTE_TRANSFER_CANCELED,
    };
    use crate::termwindow::transfer_walk::{DestinationRoot, OverwritePolicy, TransferEntryKind};
    use crate::termwindow::{
        RightSidebarFilePreviewLine, RightSidebarFilePreviewSpan, UIItem, UIItemType,
    };
    use crate::utilsprites::RenderMetrics;
    use std::collections::HashSet;
    use std::fs;
    use std::io::Cursor;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
    use std::sync::Arc;
    use wezterm_font::units::PixelLength;
    use window::color::LinearRgba;
    use window::Size;

    /// Too short for a line plus its insets still shows the first line: the
    /// insets give way, not the message. Only a box shorter than one line of
    /// text has nothing it can honestly show.
    #[test]
    fn sidebar_message_layout_keeps_one_line_when_the_insets_do_not_fit() {
        let layout = sidebar_message_layout(31, 88, 20, 8, 3);
        assert_eq!(layout.height, 31);
        assert_eq!(layout.visible_lines, 1);

        let no_room = sidebar_message_layout(12, 88, 20, 8, 3);
        assert_eq!(no_room.visible_lines, 0);
    }

    #[test]
    fn sidebar_message_layout_keeps_the_first_line_when_tight() {
        let layout = sidebar_message_layout(36, 88, 20, 8, 3);
        assert_eq!(layout.height, 36);
        assert_eq!(layout.visible_lines, 1);
        assert!(8 + layout.visible_lines * 20 <= layout.height);
    }

    #[test]
    fn sidebar_message_layout_never_places_text_below_the_card() {
        let layout = sidebar_message_layout(77, 88, 20, 8, 6);
        assert_eq!(layout.height, 77);
        assert_eq!(layout.visible_lines, 3);
        let centered_top = (layout.height - layout.visible_lines * 20) / 2;
        assert!(centered_top + layout.visible_lines * 20 <= layout.height);
    }

    #[test]
    fn preview_close_reflows_after_remote_selection_was_cleared() {
        // TargetChanged clears the remote selection before ReleaseLease closes
        // the pane, so both width reads are already tree-only. The Preview view
        // marker must still force the terminal to reclaim the old pane width.
        assert!(file_preview_close_requires_reflow(true, 320, 320));
    }

    #[test]
    fn folder_download_failures_clean_empty_reservations_and_report_partial_trees() {
        let downloads = tempfile::tempdir().unwrap();
        let root = DestinationRoot::open(downloads.path()).unwrap();

        root.create_dir_exclusive(Path::new("empty")).unwrap();
        let empty = downloads.path().join("empty");
        let failure =
            failed_folder_download(&root, Path::new("empty"), &empty, "stopped".to_string());
        assert_eq!(failure.leftover, None);
        assert!(!empty.exists(), "an empty reservation should be removed");

        root.create_dir_exclusive(Path::new("partial")).unwrap();
        let partial = downloads.path().join("partial");
        fs::write(partial.join("landed.txt"), b"data").unwrap();
        let failure = failed_folder_download(
            &root,
            Path::new("partial"),
            &partial,
            "connection lost".to_string(),
        );
        assert_eq!(
            failure.leftover.as_deref(),
            Some(partial.display().to_string().as_str())
        );
        assert!(partial.join("landed.txt").exists());
    }

    #[test]
    fn preview_close_skips_a_tree_only_noop_but_tracks_width_changes() {
        assert!(!file_preview_close_requires_reflow(false, 320, 320));
        assert!(file_preview_close_requires_reflow(false, 640, 320));
    }

    fn drop_target_items() -> Vec<UIItem> {
        // Painted back to front, the way a frame builds up: the panel
        // background first, then the rows on top of it.
        vec![
            UIItem {
                x: 100,
                y: 0,
                width: 300,
                height: 500,
                item_type: UIItemType::RightSidebarBackground,
            },
            UIItem {
                x: 100,
                y: 0,
                width: 300,
                height: 20,
                item_type: UIItemType::RightSidebarRemoteFileRow(
                    RemotePath::from_server_absolute("/home/me/src").unwrap(),
                ),
            },
            UIItem {
                x: 100,
                y: 20,
                width: 300,
                height: 20,
                item_type: UIItemType::RightSidebarRemoteFileRow(
                    RemotePath::from_server_absolute("/home/me/src/main.rs").unwrap(),
                ),
            },
        ]
    }

    /// The `cwd` setting is a request, not a promise: a pane that has not
    /// reported its working directory yet must still land the drop somewhere
    /// stable rather than failing it.
    #[test]
    fn a_terminal_drop_destination_degrades_from_cwd_to_the_default() {
        use crate::native_settings::{
            DEFAULT_REMOTE_DROP_DESTINATION, REMOTE_DROP_DESTINATION_CWD,
        };
        assert_eq!(
            resolve_drop_destination(DEFAULT_REMOTE_DROP_DESTINATION, None),
            DEFAULT_REMOTE_DROP_DESTINATION
        );
        assert_eq!(
            resolve_drop_destination(REMOTE_DROP_DESTINATION_CWD, Some("/srv/app")),
            "/srv/app"
        );
        assert_eq!(
            resolve_drop_destination(REMOTE_DROP_DESTINATION_CWD, None),
            DEFAULT_REMOTE_DROP_DESTINATION
        );
        assert_eq!(
            resolve_drop_destination(REMOTE_DROP_DESTINATION_CWD, Some("  ")),
            DEFAULT_REMOTE_DROP_DESTINATION
        );
        // A custom path never consults the cwd, even when one is around.
        assert_eq!(
            resolve_drop_destination("~/inbox", Some("/srv/app")),
            "~/inbox"
        );
    }

    /// The timestamp uses dots, like macOS screenshots, so this name is also
    /// a regression canary for the last-dot extension split: a re-paste must
    /// collide into `… (1).png`, not `Pasted 2026-08-01 at 14 (1).30.00.png`.
    #[test]
    fn a_pasted_image_is_named_like_a_screenshot() {
        use chrono::TimeZone;
        let when = chrono::Local
            .with_ymd_and_hms(2026, 8, 1, 14, 30, 0)
            .unwrap();
        let name = pasted_image_file_name(when);
        assert_eq!(name, "Pasted 2026-08-01 at 14.30.00.png");
        assert_eq!(
            download_name_candidates(&name).nth(1).unwrap(),
            "Pasted 2026-08-01 at 14.30.00 (1).png"
        );
    }

    #[test]
    fn a_pasted_bmp_is_reencoded_as_png_and_png_passes_through() {
        // A real 1x1 BMP, produced by the same crate that decodes it.
        let mut bmp = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
            1,
            1,
            image::Rgba([255, 0, 0, 255]),
        ))
        .write_to(&mut bmp, image::ImageFormat::Bmp)
        .unwrap();
        let png = encode_pasted_image_png(window::ClipboardImageFormat::Bmp, bmp.into_inner())
            .expect("bmp converts");
        assert_eq!(&png[1..4], b"PNG", "the conversion output must be PNG");

        let passthrough =
            encode_pasted_image_png(window::ClipboardImageFormat::Png, png.clone()).unwrap();
        assert_eq!(passthrough, png, "png bytes must pass through untouched");

        assert!(
            encode_pasted_image_png(window::ClipboardImageFormat::Tiff, b"not an image".to_vec())
                .is_err(),
            "garbage must surface as an error, not a bogus upload"
        );
    }

    #[cfg(unix)]
    #[test]
    fn pasted_image_staging_is_private_and_cleans_up_until_retained() {
        use std::os::unix::fs::PermissionsExt;

        let staged = stage_pasted_image(
            window::ClipboardImageFormat::Png,
            b"\x89PNG\r\n\x1a\nprivate".to_vec(),
            "Pasted test.png",
        )
        .expect("stage image");
        let path = staged.path.clone();
        let directory = path.parent().unwrap().to_path_buf();
        assert_ne!(
            directory,
            std::env::temp_dir().join("thinkterm-pasted-images"),
            "the staging directory must be randomized per image"
        );
        assert_eq!(
            fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );

        drop(staged);
        assert!(!directory.exists(), "unaccepted staging must be cleaned up");
    }

    #[test]
    fn pasted_image_staging_runs_off_the_calling_thread() {
        let caller = std::thread::current().id();
        let mut bmp = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
            1,
            1,
            image::Rgba([0, 255, 0, 255]),
        ))
        .write_to(&mut bmp, image::ImageFormat::Bmp)
        .unwrap();
        let executor = promise::spawn::ScopedExecutor::new();
        let staged = smol::block_on(executor.run(async {
            spawn_pasted_image_staging(
                window::ClipboardImageFormat::Bmp,
                bmp.into_inner(),
                "Pasted worker.png".to_string(),
            )
            .await
        }))
        .expect("worker staging");
        assert_ne!(staged.worker_thread_id, caller);
        assert_eq!(&fs::read(&staged.path).unwrap()[1..4], b"PNG");
    }

    #[test]
    fn typed_paste_keeps_its_originating_pane_when_focus_changes() {
        let captured = TerminalPasteTarget {
            pane_id: 17,
            remote: None,
        };
        let active_after_clipboard_read = 29;

        assert_ne!(captured.pane_id(), active_after_clipboard_read);
        assert_eq!(captured.pane_id(), 17);
    }

    #[test]
    fn typed_paste_rejects_space_host_connection_and_pane_changes() {
        let host_a = crate::workspace_threads::RemoteFilesTarget {
            project_id: "project-a".to_string(),
            project_name: "A".to_string(),
            source: crate::workspace_threads::RemoteFilesSource::SshHost("host-a".to_string()),
            requested_root: "~".to_string(),
        };
        let host_b = crate::workspace_threads::RemoteFilesTarget {
            project_id: "project-b".to_string(),
            project_name: "B".to_string(),
            source: crate::workspace_threads::RemoteFilesSource::SshHost("host-b".to_string()),
            requested_root: "~".to_string(),
        };

        assert_eq!(
            terminal_paste_snapshot_mismatch(
                "space-a",
                "space-a",
                &host_a,
                Some(&host_a),
                "connection-a",
                Some("connection-a"),
                true,
            ),
            None
        );
        assert_eq!(
            terminal_paste_snapshot_mismatch(
                "space-a",
                "space-b",
                &host_a,
                Some(&host_a),
                "connection-a",
                Some("connection-a"),
                true,
            ),
            Some(TerminalPasteSnapshotMismatch::Space)
        );
        assert_eq!(
            terminal_paste_snapshot_mismatch(
                "space-a",
                "space-a",
                &host_a,
                Some(&host_b),
                "connection-a",
                Some("connection-a"),
                true,
            ),
            Some(TerminalPasteSnapshotMismatch::Project)
        );
        assert_eq!(
            terminal_paste_snapshot_mismatch(
                "space-a",
                "space-a",
                &host_a,
                Some(&host_a),
                "connection-a",
                Some("connection-b"),
                true,
            ),
            Some(TerminalPasteSnapshotMismatch::Connection)
        );
        assert_eq!(
            terminal_paste_snapshot_mismatch(
                "space-a",
                "space-a",
                &host_a,
                Some(&host_a),
                "connection-a",
                Some("connection-a"),
                false,
            ),
            Some(TerminalPasteSnapshotMismatch::Pane)
        );
    }

    /// Backend double whose `exists` answers from a fixed set of taken names.
    struct TakenNamesBackend {
        taken: std::collections::HashSet<String>,
    }

    impl crate::termwindow::remote_files::RemoteFileBackend for TakenNamesBackend {
        fn resolve_root(
            &self,
            _requested: String,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<RemotePath, String>> + Send + 'static>,
        > {
            Box::pin(async { RemotePath::from_server_absolute("/srv") })
        }

        fn list_directory(
            &self,
            _path: RemotePath,
            _limit: usize,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<
                            crate::termwindow::remote_files::RemoteDirectoryListing,
                            String,
                        >,
                    > + Send
                    + 'static,
            >,
        > {
            Box::pin(async {
                Ok(crate::termwindow::remote_files::RemoteDirectoryListing {
                    entries: Vec::new(),
                    truncated: false,
                })
            })
        }

        fn read_file(
            &self,
            _path: RemotePath,
            _limit: usize,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<RemoteFileBytes, String>> + Send + 'static>,
        > {
            Box::pin(async {
                Ok(RemoteFileBytes {
                    bytes: Vec::new(),
                    truncated: false,
                })
            })
        }

        fn exists(
            &self,
            remote: RemotePath,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<bool, String>> + Send + 'static>,
        > {
            let taken = self.taken.contains(remote.as_str());
            Box::pin(async move { Ok(taken) })
        }
    }

    /// Re-dropping the same screenshot must become ` (1)`, ` (2)` — the
    /// browser rule — with the extension kept whole.
    #[test]
    fn a_terminal_drop_picks_the_first_free_remote_name() {
        let directory = RemotePath::from_server_absolute("/home/x/ThinkTerm_Uploads").unwrap();
        let backend = TakenNamesBackend {
            taken: [
                "/home/x/ThinkTerm_Uploads/shot.png",
                "/home/x/ThinkTerm_Uploads/shot (1).png",
            ]
            .iter()
            .map(|name| (*name).to_string())
            .collect(),
        };
        let chosen =
            smol::block_on(pick_free_remote_name(&backend, &directory, "shot.png")).unwrap();
        assert_eq!(chosen.as_str(), "/home/x/ThinkTerm_Uploads/shot (2).png");

        let untouched =
            smol::block_on(pick_free_remote_name(&backend, &directory, "fresh.txt")).unwrap();
        assert_eq!(untouched.as_str(), "/home/x/ThinkTerm_Uploads/fresh.txt");
    }

    #[test]
    fn a_drop_lands_in_the_directory_it_is_aimed_at() {
        let root = RemotePath::from_server_absolute("/home/me").unwrap();
        let items = drop_target_items();
        let kind = |path: &RemotePath| match path.as_str() {
            "/home/me/src" => Some(RemoteFileKind::Directory),
            "/home/me/src/main.rs" => Some(RemoteFileKind::File),
            _ => None,
        };

        // A directory row takes the upload itself.
        assert_eq!(
            resolve_remote_drop_target(&items, 200, 10, &root, kind).as_ref(),
            Some(&RemotePath::from_server_absolute("/home/me/src").unwrap())
        );
        // A file row means "next to this", i.e. the directory holding it.
        assert_eq!(
            resolve_remote_drop_target(&items, 200, 30, &root, kind).as_ref(),
            Some(&RemotePath::from_server_absolute("/home/me/src").unwrap())
        );
        // Empty space inside the panel falls back to the root.
        assert_eq!(
            resolve_remote_drop_target(&items, 200, 400, &root, kind).as_ref(),
            Some(&root)
        );
        // Outside the panel entirely: not ours, so the terminal keeps its
        // long-standing paste behaviour.
        assert_eq!(
            resolve_remote_drop_target(&items, 50, 10, &root, kind),
            None
        );
    }

    fn file_entry(
        source: &Path,
        relative: &str,
    ) -> crate::termwindow::transfer_walk::TransferEntry {
        crate::termwindow::transfer_walk::TransferEntry {
            source: source.to_path_buf(),
            relative: PathBuf::from(relative),
            kind: TransferEntryKind::File,
            size: 0,
        }
    }

    /// The check-then-create shape this replaced could be raced: a file
    /// appearing between the two was truncated under an authorisation the user
    /// never gave. The exclusive create makes the filesystem decide.
    #[test]
    fn a_no_clobber_copy_leaves_an_existing_file_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("src.txt");
        let dest_dir = dir.path().join("dest");
        fs::create_dir_all(&dest_dir).unwrap();
        fs::write(&source, b"new contents").unwrap();
        fs::write(dest_dir.join("src.txt"), b"do not lose me").unwrap();

        let entries = vec![file_entry(&source, "src.txt")];
        let progress = RemoteTransferProgress::default();
        let copied = copy_entries_blocking(
            &entries,
            &DestinationRoot::open(&dest_dir).unwrap(),
            OverwritePolicy::NoClobber,
            &progress,
        )
        .expect("an occupied name is a skip, not a failure");

        assert_eq!(copied, 0, "nothing was copied");
        assert_eq!(
            fs::read(dest_dir.join("src.txt")).unwrap(),
            b"do not lose me"
        );
    }

    /// Replace assembles the new file beside the target and swaps it in, so a
    /// failure or a cancel leaves the original whole. Truncating in place
    /// destroyed it the moment the write began.
    #[test]
    fn a_canceled_replace_leaves_the_original_intact() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("src.txt");
        let dest_dir = dir.path().join("dest");
        fs::create_dir_all(&dest_dir).unwrap();
        fs::write(&source, vec![b'x'; LOCAL_COPY_CHUNK * 3]).unwrap();
        let destination = dest_dir.join("src.txt");
        fs::write(&destination, b"original").unwrap();

        let entries = vec![file_entry(&source, "src.txt")];
        let progress = RemoteTransferProgress::default();
        progress.request_cancel();
        let err = copy_entries_blocking(
            &entries,
            &DestinationRoot::open(&dest_dir).unwrap(),
            OverwritePolicy::Replace,
            &progress,
        )
        .expect_err("a canceled copy fails");
        assert!(err.contains(REMOTE_TRANSFER_CANCELED), "{}", err);

        assert_eq!(
            fs::read(&destination).unwrap(),
            b"original",
            "the file being replaced must survive a cancel"
        );
        // And no staging file is left lying about.
        let leftovers: Vec<_> = fs::read_dir(&dest_dir)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .filter(|name| name != "src.txt")
            .collect();
        assert!(leftovers.is_empty(), "left behind: {:?}", leftovers);
    }

    /// Goes through the real `copy_file_chunked` Replace path and forces its
    /// cleanup to run, then checks *where* the delete landed.
    ///
    /// The discriminator is the root symlink being repointed after the handle
    /// was taken: a path-based cleanup resolves the root's name again and so
    /// deletes from the new target, while a handle-based one stays where it
    /// started. The earlier tests covered `DestinationRoot::remove_file` on its
    /// own; this covers the branch actually calling it.
    #[cfg(unix)]
    #[test]
    fn a_failed_replace_cleans_up_through_the_handle() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        let decoy = dir.path().join("decoy");
        fs::create_dir_all(&real).unwrap();
        fs::create_dir_all(&decoy).unwrap();
        let link = dir.path().join("root");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let source = dir.path().join("src.txt");
        fs::write(&source, b"replacement").unwrap();
        fs::write(real.join("target.txt"), b"original").unwrap();

        // Handle taken while the name still points at `real`.
        let root = DestinationRoot::open(&link).unwrap();

        // The name now points somewhere else, and a file sits at exactly the
        // staging path a path-based cleanup would compute.
        fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(&decoy, &link).unwrap();
        let bystander = decoy.join(".target.txt.thinkterm-0");
        fs::write(&bystander, b"not yours to delete").unwrap();

        let progress = RemoteTransferProgress::default();
        progress.request_cancel();
        let err = copy_file_chunked(
            &source,
            &root,
            Path::new("target.txt"),
            OverwritePolicy::Replace,
            &progress,
        )
        .expect_err("a canceled replace fails");
        assert!(err.contains(REMOTE_TRANSFER_CANCELED), "{}", err);

        assert!(
            bystander.exists(),
            "cleanup must not follow the root's name to its new target"
        );
        assert_eq!(
            fs::read(real.join("target.txt")).unwrap(),
            b"original",
            "the file being replaced survives"
        );
        let leftovers: Vec<_> = fs::read_dir(&real)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .filter(|name| name != "target.txt")
            .collect();
        assert!(
            leftovers.is_empty(),
            "the staging file must be removed from where it was made: {:?}",
            leftovers
        );
    }

    #[test]
    fn a_replace_swaps_in_the_new_contents() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("src.txt");
        let dest_dir = dir.path().join("dest");
        fs::create_dir_all(&dest_dir).unwrap();
        fs::write(&source, b"new contents").unwrap();
        fs::write(dest_dir.join("src.txt"), b"old").unwrap();

        let entries = vec![file_entry(&source, "src.txt")];
        let progress = RemoteTransferProgress::default();
        let copied = copy_entries_blocking(
            &entries,
            &DestinationRoot::open(&dest_dir).unwrap(),
            OverwritePolicy::Replace,
            &progress,
        )
        .expect("replace");
        assert_eq!(copied, 1);
        assert_eq!(fs::read(dest_dir.join("src.txt")).unwrap(), b"new contents");
    }

    /// A symlink occupying the destination name must not be followed: writing
    /// through it puts the data wherever the link points, outside the folder
    /// the user chose.
    #[cfg(unix)]
    #[test]
    fn a_copy_does_not_write_through_a_symlinked_destination() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("src.txt");
        let dest_dir = dir.path().join("dest");
        let outside = dir.path().join("outside.txt");
        fs::create_dir_all(&dest_dir).unwrap();
        fs::write(&source, b"payload").unwrap();
        fs::write(&outside, b"untouched").unwrap();
        std::os::unix::fs::symlink(&outside, dest_dir.join("src.txt")).unwrap();

        let entries = vec![file_entry(&source, "src.txt")];
        let progress = RemoteTransferProgress::default();

        // No-clobber: the name is taken, so it is skipped.
        copy_entries_blocking(
            &entries,
            &DestinationRoot::open(&dest_dir).unwrap(),
            OverwritePolicy::NoClobber,
            &progress,
        )
        .expect("skip");
        assert_eq!(fs::read(&outside).unwrap(), b"untouched");

        // Replace: the link itself is replaced, not what it points at.
        let progress = RemoteTransferProgress::default();
        copy_entries_blocking(
            &entries,
            &DestinationRoot::open(&dest_dir).unwrap(),
            OverwritePolicy::Replace,
            &progress,
        )
        .expect("replace");
        assert_eq!(
            fs::read(&outside).unwrap(),
            b"untouched",
            "the file outside the destination must never be written"
        );
        assert_eq!(fs::read(dest_dir.join("src.txt")).unwrap(), b"payload");
    }

    /// Verified empirically: `fs::copy` with one path truncates the shared
    /// inode and reports success, so an 11-byte file silently becomes empty.
    #[cfg(unix)]
    #[test]
    fn a_copy_refuses_a_hard_linked_destination() {
        let dir = tempfile::tempdir().unwrap();
        let dest_dir = dir.path().join("dest");
        fs::create_dir_all(&dest_dir).unwrap();
        let source = dir.path().join("src.txt");
        fs::write(&source, b"important data").unwrap();
        fs::hard_link(&source, dest_dir.join("src.txt")).unwrap();

        let entries = vec![file_entry(&source, "src.txt")];
        let progress = RemoteTransferProgress::default();
        let err = copy_entries_blocking(
            &entries,
            &DestinationRoot::open(&dest_dir).unwrap(),
            OverwritePolicy::Replace,
            &progress,
        )
        .expect_err("copying a file over itself must be refused");
        assert!(err.contains("already at the destination"), "{}", err);
        assert_eq!(
            fs::read(&source).unwrap(),
            b"important data",
            "the source must not be destroyed"
        );
    }

    /// Two sources in one drop wanting the same destination name: whichever
    /// lands second would otherwise overwrite the first, or half-fail. It does
    /// not matter that one is a folder and the other a file — a name can only
    /// belong to one of them.
    #[test]
    fn two_sources_claiming_one_name_are_caught_before_anything_is_written() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("dest");
        fs::create_dir_all(&dest).unwrap();

        // A folder named `foo` and, from elsewhere, a file also named `foo`.
        let a = dir.path().join("a/foo");
        fs::create_dir_all(&a).unwrap();
        fs::write(a.join("inner.txt"), b"x").unwrap();
        let b = dir.path().join("b/foo");
        fs::create_dir_all(b.parent().unwrap()).unwrap();
        fs::write(&b, b"y").unwrap();

        let preflight = preflight_local_copy(&[a, b], &dest);
        assert_eq!(
            preflight.plans.len(),
            1,
            "only the first claim on the name survives"
        );
        assert_eq!(preflight.rejected.len(), 1);
        assert!(
            preflight.rejected[0]
                .1
                .contains("already where it would be copied to"),
            "{:?}",
            preflight.rejected[0]
        );
    }

    #[test]
    fn a_local_drop_lands_in_the_directory_it_is_aimed_at() {
        let root = Path::new("/home/me/proj");
        let items = vec![
            UIItem {
                x: 100,
                y: 0,
                width: 300,
                height: 500,
                item_type: UIItemType::RightSidebarBackground,
            },
            UIItem {
                x: 100,
                y: 0,
                width: 300,
                height: 20,
                item_type: UIItemType::RightSidebarFileRow(PathBuf::from("/home/me/proj/src")),
            },
            UIItem {
                x: 100,
                y: 20,
                width: 300,
                height: 20,
                item_type: UIItemType::RightSidebarFileRow(PathBuf::from(
                    "/home/me/proj/src/main.rs",
                )),
            },
        ];
        let is_dir = |path: &Path| match path.to_str() {
            Some("/home/me/proj/src") => Some(true),
            Some("/home/me/proj/src/main.rs") => Some(false),
            _ => None,
        };

        assert_eq!(
            resolve_local_drop_target(&items, 200, 10, root, is_dir),
            Some(PathBuf::from("/home/me/proj/src"))
        );
        // A file row means "put it beside this".
        assert_eq!(
            resolve_local_drop_target(&items, 200, 30, root, is_dir),
            Some(PathBuf::from("/home/me/proj/src"))
        );
        assert_eq!(
            resolve_local_drop_target(&items, 200, 400, root, is_dir),
            Some(root.to_path_buf())
        );
        // Outside the panel: the terminal keeps its paste behaviour.
        assert_eq!(
            resolve_local_drop_target(&items, 50, 10, root, is_dir),
            None
        );
        // A row the panel no longer knows the kind of resolves to nothing
        // rather than guessing a destination.
        assert_eq!(
            resolve_local_drop_target(&items, 200, 10, root, |_| None),
            None
        );
    }

    #[test]
    fn a_drop_onto_a_row_of_unknown_kind_is_refused() {
        // A row painted from a listing the state has since dropped: guessing
        // a destination here could put the file somewhere unintended.
        let root = RemotePath::from_server_absolute("/home/me").unwrap();
        let items = drop_target_items();
        assert_eq!(
            resolve_remote_drop_target(&items, 200, 10, &root, |_| None),
            None
        );
    }

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
        assert_eq!(
            note_image_display_size(0, 100, 600.0, 500.0, super::NOTE_IMAGE_MAX_UPSCALE),
            (0.0, 0.0)
        );
        assert_eq!(
            note_image_display_size(200, 100, 600.0, 500.0, super::NOTE_IMAGE_MAX_UPSCALE),
            (600.0, 300.0)
        );
        assert_eq!(
            note_image_display_size(1200, 2400, 600.0, 500.0, super::NOTE_IMAGE_MAX_UPSCALE),
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
    fn file_tree_sorts_dirs_first_and_natural() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("dir10")).unwrap();
        fs::create_dir(dir.path().join("dir2")).unwrap();
        fs::write(dir.path().join("file10.txt"), "").unwrap();
        fs::write(dir.path().join("file2.txt"), "").unwrap();

        let expanded = HashSet::new();
        let cache = load_dir_cache_for_test(dir.path(), &expanded);
        let (rows, _) =
            right_sidebar_file_browse_rows_from_dir_cache(&cache, dir.path(), "Project", &expanded);
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

    use super::{
        remote_file_index_from_listing, remote_file_search_rows, remote_path_under,
        right_sidebar_hover_zone, RemoteFileSearchState, RemoteFileSearchStatus,
    };

    #[test]
    fn markdown_previews_render_read_only_from_their_own_session() {
        use crate::markdown_editor::{EditorMode, NoteHostState, ProjectedObject};
        let base = tempfile::tempdir().unwrap();
        let project_root = base.path().join("project");
        let docs = project_root.join("docs");
        std::fs::create_dir_all(&docs).unwrap();
        std::fs::write(docs.join("shot.png"), b"not really a png").unwrap();
        let outside_root = base.path().join("other");
        std::fs::create_dir_all(&outside_root).unwrap();
        std::fs::write(outside_root.join("secret.png"), b"x").unwrap();
        let file = docs.join("README.md");
        let text = "# Title\n\nSome *text*.\n\n```rust\nfn main() {}\n```\n".to_string();

        let document = super::markdown_preview_document(
            file.to_str().unwrap(),
            text.clone(),
            None,
            Some(&project_root),
        );
        assert_eq!(document.vault_root, project_root);
        assert_eq!(document.relative_path, "docs/README.md");
        // Images resolve beside the file and cannot leave the project.
        assert!(crate::markdown_editor::resolve_local_image(
            &document.vault_root,
            &document.document_path,
            "shot.png"
        )
        .is_ok());
        // A real file one folder up from the project: reachable, but outside.
        assert!(docs.join("../../other/secret.png").exists());
        assert!(crate::markdown_editor::resolve_local_image(
            &document.vault_root,
            &document.document_path,
            "../../other/secret.png"
        )
        .is_err());

        let mut host = NoteHostState::default();
        host.bind_document(document);
        host.view.mode = EditorMode::ReadOnly;
        host.refresh_projection();
        assert!(host
            .projection
            .objects
            .iter()
            .any(|object| matches!(object, ProjectedObject::CodeBlock(_))));
        // Read-only: typing changes nothing.
        let session = host.session.clone().unwrap();
        let changed = session.lock().insert_text(&mut host.view, "x");
        assert!(!changed);
        assert_eq!(session.lock().source(), text);

        // Outside the project, the file's own folder bounds it.
        let loose = outside_root.join("notes.md");
        let document = super::markdown_preview_document(
            loose.to_str().unwrap(),
            String::new(),
            None,
            Some(&project_root),
        );
        assert_eq!(document.vault_root, outside_root);
        // A remote file has no local folder to take images from.
        let remote = super::markdown_preview_document(
            "/srv/app/docs/README.md",
            text,
            Some("docs/README.md".to_string()),
            None,
        );
        assert_eq!(remote.relative_path, "docs/README.md");
        assert!(remote.document_path.as_os_str().is_empty());
        assert!(crate::markdown_editor::resolve_local_image(
            &remote.vault_root,
            &remote.document_path,
            "shot.png"
        )
        .is_err());
        assert!(super::is_markdown_extension("MD"));
        assert!(super::is_markdown_extension("markdown"));
        assert!(!super::is_markdown_extension("mdx"));
    }

    #[test]
    fn preview_wiki_links_resolve_among_the_project_files() {
        use crate::markdown_editor::{MarkdownProjection, ProjectedObject};
        let project = tempfile::tempdir().unwrap();
        let root = project.path();
        std::fs::create_dir_all(root.join("docs")).unwrap();
        std::fs::create_dir_all(root.join("node_modules/pkg")).unwrap();
        std::fs::write(root.join("docs/guide.md"), "# Guide").unwrap();
        std::fs::write(root.join("node_modules/pkg/setup.md"), "").unwrap();
        let walk = || super::PreviewLinkFiles::WalkProject {
            respect_gitignore: true,
        };
        let resolved_in = |source: &str, root: &Path, current: &str, files| {
            let mut projection = MarkdownProjection::parse(source);
            super::resolve_preview_wiki_links(&mut projection, root, current, files);
            projection
                .objects
                .iter()
                .find_map(|object| match object {
                    ProjectedObject::WikiLink { resolved_path, .. } => Some(resolved_path.clone()),
                    _ => None,
                })
                .expect("a wiki link")
        };
        let resolved = |source: &str| resolved_in(source, root, "docs/README.md", walk());
        assert_eq!(
            resolved("See [[guide]]."),
            Some("docs/guide.md".to_string())
        );
        // Dependency trees are not part of the project for links either.
        assert_eq!(resolved("See [[setup]]."), None);
        // The search index's paths serve as well as a walk.
        let listed = super::PreviewLinkFiles::Listed(Arc::new(vec!["docs/guide.md".to_string()]));
        assert_eq!(
            resolved_in("See [[guide]].", root, "docs/README.md", listed),
            Some("docs/guide.md".to_string())
        );
        // A file outside the project sees only the files beside it.
        assert_eq!(
            resolved_in(
                "See [[guide]].",
                root,
                "README.md",
                super::PreviewLinkFiles::Folder
            ),
            None
        );
        std::fs::write(root.join("notes.md"), "").unwrap();
        assert_eq!(
            resolved_in(
                "See [[notes]].",
                root,
                "README.md",
                super::PreviewLinkFiles::Folder
            ),
            Some("notes.md".to_string())
        );
        // Without a local root (a remote file) nothing local is walked.
        let mut projection = MarkdownProjection::parse("See [[guide]].");
        super::resolve_preview_wiki_links(&mut projection, Path::new(""), "README.md", walk());
        assert!(projection.objects.iter().all(|object| !matches!(
            object,
            ProjectedObject::WikiLink {
                resolved_path: Some(_),
                ..
            }
        )));
    }

    #[test]
    fn remote_preview_links_resolve_among_listed_files_without_reading_any() {
        use crate::markdown_editor::{MarkdownProjection, ProjectedObject};
        let root = RemotePath::from_server_absolute("/srv/app").unwrap();
        let file = RemotePath::from_server_absolute("/srv/app/docs/README.md").unwrap();
        assert_eq!(
            super::remote_relative_path(&root, &file).as_deref(),
            Some("docs/README.md")
        );
        let slash = RemotePath::from_server_absolute("/").unwrap();
        assert_eq!(
            super::remote_relative_path(&slash, &file).as_deref(),
            Some("srv/app/docs/README.md")
        );
        let elsewhere = RemotePath::from_server_absolute("/srv/other/a.md").unwrap();
        assert_eq!(super::remote_relative_path(&root, &elsewhere), None);
        let sibling = RemotePath::from_server_absolute("/srv/application/a.md").unwrap();
        assert_eq!(super::remote_relative_path(&root, &sibling), None);

        let mut projection = MarkdownProjection::parse("See [[guide]].\n\n![[guide]]\n");
        projection.resolve_links_among(None, vec!["docs/guide.md".to_string()], "docs/README.md");
        let links: Vec<_> = projection
            .objects
            .iter()
            .filter_map(|object| match object {
                ProjectedObject::WikiLink {
                    resolved_path,
                    rendered_lines,
                    ..
                } => Some((resolved_path.clone(), rendered_lines.is_empty())),
                _ => None,
            })
            .collect();
        assert_eq!(links.len(), 2);
        // Resolved, and the embed renders nothing: no local root to read from.
        assert!(links
            .iter()
            .all(|(path, empty)| path.as_deref() == Some("docs/guide.md") && *empty));
    }

    #[test]
    fn right_sidebar_hover_zones_mirror_the_left_edge() {
        use crate::termwindow::sidebar_hover::PointerZone;
        // A 1000px-wide window: the strip is the last 12px below a 40px tab
        // bar, the sticky band the 28px ending at the window edge.
        let hot = crate::termwindow::sidebar_hover::hot_zone(988, 0, 800, 12, 40);
        assert_eq!(hot, Some((988, 40, 12, 760)));
        let zone = |x, y, panel, toggle| right_sidebar_hover_zone(x, y, panel, hot, 28, toggle);
        assert_eq!(zone(995, 400, None, false), PointerZone::HotZone);
        assert_eq!(zone(988, 400, None, false), PointerZone::HotZone);
        assert_eq!(zone(980, 400, None, false), PointerZone::NearHotZone);
        assert_eq!(zone(972, 400, None, false), PointerZone::NearHotZone);
        assert_eq!(zone(971, 400, None, false), PointerZone::Away);
        assert_eq!(zone(500, 400, None, false), PointerZone::Away);
        // Not in the tab bar row, where the pointer heads for the tabs...
        assert_eq!(zone(995, 20, None, false), PointerZone::Away);
        // ...unless it is on the sidebar button, which arms on its own.
        assert_eq!(zone(960, 20, None, true), PointerZone::HotZone);
        // Once out, the panel is the panel, strip included.
        let panel = Some(super::RightSidebarRect {
            x: 700,
            y: 0,
            width: 300,
            height: 800,
        });
        assert_eq!(zone(995, 400, panel, false), PointerZone::Panel);
        assert_eq!(zone(700, 400, panel, false), PointerZone::Panel);
        assert_eq!(zone(699, 400, panel, false), PointerZone::Away);
    }

    #[test]
    fn remote_search_finds_listed_files_under_the_root() {
        let listing = thinkterm_file_index::Listing {
            entries: vec![
                thinkterm_file_index::ListedEntry {
                    path: "src".to_string(),
                    is_dir: true,
                },
                thinkterm_file_index::ListedEntry {
                    path: "src/main.rs".to_string(),
                    is_dir: false,
                },
                thinkterm_file_index::ListedEntry {
                    path: "docs/main-notes.md".to_string(),
                    is_dir: false,
                },
            ],
            truncated: false,
        };
        let index = remote_file_index_from_listing("app", &listing);
        let root = RemotePath::from_server_absolute("/srv/app").unwrap();
        let rows = remote_file_search_rows(&index, &root, "main", &AtomicBool::new(false));
        let found: Vec<_> = rows
            .iter()
            .map(|row| (row.entry.path.as_str().to_string(), row.entry.kind))
            .collect();
        assert!(found.contains(&("/srv/app/src/main.rs".to_string(), RemoteFileKind::File)));
        assert!(found.contains(&(
            "/srv/app/docs/main-notes.md".to_string(),
            RemoteFileKind::File
        )));
        assert_eq!(found.len(), 2);
        let dirs = remote_file_search_rows(&index, &root, "src", &AtomicBool::new(false));
        assert_eq!(dirs[0].entry.kind, RemoteFileKind::Directory);
        // The same matching as a local project: a path query searches paths.
        assert_eq!(
            remote_file_search_rows(&index, &root, "src/ma", &AtomicBool::new(false)).len(),
            1
        );
        assert!(remote_file_search_rows(&index, &root, "", &AtomicBool::new(false)).is_empty());
    }

    #[test]
    fn remote_search_results_cannot_leave_the_root() {
        let root = RemotePath::from_server_absolute("/srv/app").unwrap();
        assert!(remote_path_under(&root, "../etc/passwd").is_none());
        assert!(remote_path_under(&root, "a/./b").is_none());
        assert!(remote_path_under(&root, "a//b").is_none());
        assert_eq!(
            remote_path_under(&root, "a/b").unwrap().as_str(),
            "/srv/app/a/b"
        );
    }

    #[test]
    fn remote_search_state_drops_stale_listings() {
        let mut state = RemoteFileSearchState::default();
        let before = state.generation;
        state.release();
        assert_ne!(state.generation, before);
        assert!(state.index.is_none());
        assert_eq!(state.status, RemoteFileSearchStatus::Idle);
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

        let index = build_right_sidebar_file_index(dir.path(), "Project", true).unwrap();
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

        let index = build_right_sidebar_file_index(dir.path(), "Project", true).unwrap();
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

        let index = build_right_sidebar_file_index(dir.path(), "Project", true).unwrap();
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

        let index = build_right_sidebar_file_index(dir.path(), "Project", true).unwrap();
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

        let index = build_right_sidebar_file_index(dir.path(), "Project", true).unwrap();
        let cancel = AtomicBool::new(false);
        let rows = search_right_sidebar_file_index(&index, "generated", &cancel);
        assert!(rows.is_empty());
    }

    #[test]
    fn file_tree_only_reads_expanded_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        fs::create_dir(&src).unwrap();
        fs::write(src.join("main.rs"), "fn main() {}\n").unwrap();

        let collapsed = HashSet::new();
        let cache = load_dir_cache_for_test(dir.path(), &collapsed);
        let (rows, _) = right_sidebar_file_browse_rows_from_dir_cache(
            &cache,
            dir.path(),
            "Project",
            &collapsed,
        );
        let names: Vec<_> = rows.iter().map(|row| row.name.as_str()).collect();
        assert_eq!(names, vec!["Project", "src"]);
        // The point of the lazy tree: a collapsed folder is never even read.
        assert!(!cache.is_loaded(&src));

        let mut expanded = HashSet::new();
        expanded.insert(path_key(&src));
        let cache = load_dir_cache_for_test(dir.path(), &expanded);
        let (rows, _) =
            right_sidebar_file_browse_rows_from_dir_cache(&cache, dir.path(), "Project", &expanded);
        let names: Vec<_> = rows.iter().map(|row| row.name.as_str()).collect();
        assert_eq!(names, vec!["Project", "src", "main.rs"]);
        assert!(cache.is_loaded(&src));
    }

    /// Regression test for the bug this lazy tree exists to make impossible: a
    /// project whose first directory dwarfs everything else used to swallow the
    /// whole entry budget of the up-front walk, leaving its siblings out of the
    /// tree entirely. Reading only what is expanded means the size of a
    /// collapsed subtree cannot influence its siblings at all.
    #[test]
    fn a_huge_collapsed_subtree_cannot_starve_its_siblings() {
        let dir = tempfile::tempdir().unwrap();
        let heavy = dir.path().join("aaa_research");
        fs::create_dir(&heavy).unwrap();
        for index in 0..200 {
            let nested = heavy.join(format!("repo{index}")).join("src");
            fs::create_dir_all(&nested).unwrap();
            fs::write(nested.join("lib.rs"), "").unwrap();
        }
        for sibling in ["crates", "docs", "scripts", "ui"] {
            fs::create_dir(dir.path().join(sibling)).unwrap();
        }
        fs::write(dir.path().join("Cargo.toml"), "").unwrap();

        let expanded = HashSet::new();
        let cache = load_dir_cache_for_test(dir.path(), &expanded);
        let (rows, _) =
            right_sidebar_file_browse_rows_from_dir_cache(&cache, dir.path(), "Project", &expanded);
        let names: Vec<_> = rows.iter().map(|row| row.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "Project",
                "aaa_research",
                "crates",
                "docs",
                "scripts",
                "ui",
                "Cargo.toml"
            ]
        );
        // One directory read in total — the heavy subtree is never descended.
        assert_eq!(cache.loaded_dirs().len(), 1);
    }

    #[test]
    fn search_index_respects_gitignore_only_when_configured() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(".gitignore"), "secret/\n").unwrap();
        let secret = dir.path().join("secret");
        fs::create_dir(&secret).unwrap();
        fs::write(secret.join("token.txt"), "").unwrap();
        fs::write(dir.path().join("visible.txt"), "").unwrap();

        // No `.git` here, so this also pins `require_git(false)`.
        let index = build_right_sidebar_file_index(dir.path(), "Project", true).unwrap();
        let cancel = AtomicBool::new(false);
        assert!(search_right_sidebar_file_index(&index, "token", &cancel).is_empty());
        assert!(!search_right_sidebar_file_index(&index, "visible", &cancel).is_empty());

        let index = build_right_sidebar_file_index(dir.path(), "Project", false).unwrap();
        assert!(!search_right_sidebar_file_index(&index, "token", &cancel).is_empty());
    }

    /// The tree half of a re-scan must not be gated on the search index. A user
    /// who never opens the filter box has no index and never will, and gating
    /// would mean their tree stopped picking up renames, deletes and external
    /// changes entirely.
    #[test]
    fn rescan_rereads_the_tree_even_with_no_search_index() {
        let plan = rescan_plan(true, false, false);
        assert!(plan.reread_loaded_dirs);
        assert!(!plan.refresh_search_index);
    }

    /// A re-scan may refresh an existing index but must never build one, or the
    /// eager whole-project walk comes back through the 90-second timer.
    #[test]
    fn rescan_never_creates_a_search_index() {
        for refreshing in [false, true] {
            assert!(!rescan_plan(true, false, refreshing).refresh_search_index);
        }
        assert!(rescan_plan(true, true, false).refresh_search_index);
        // ...and defers to a rebuild already in flight.
        assert!(!rescan_plan(true, true, true).refresh_search_index);
    }

    #[test]
    fn rescan_does_nothing_while_the_file_view_is_hidden() {
        let plan = rescan_plan(false, true, false);
        assert!(!plan.reread_loaded_dirs);
        assert!(!plan.refresh_search_index);
    }

    #[test]
    fn search_index_keeps_dotfiles_visible() {
        let dir = tempfile::tempdir().unwrap();
        let workflows = dir.path().join(".github").join("workflows");
        fs::create_dir_all(&workflows).unwrap();
        fs::write(workflows.join("ci.yml"), "").unwrap();

        let index = build_right_sidebar_file_index(dir.path(), "Project", true).unwrap();
        let cancel = AtomicBool::new(false);
        assert!(!search_right_sidebar_file_index(&index, "ci.yml", &cancel).is_empty());
    }

    #[test]
    fn file_index_search_respects_cancel() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("main.rs"), "fn main() {}\n").unwrap();
        let index = build_right_sidebar_file_index(dir.path(), "Project", true).unwrap();
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
    fn sidebar_row_elements_are_drawn_while_they_straddle_an_edge() {
        // mask strip 100..160, list 160..400, panel bottom 440 (a 40px bottom
        // mask). Elements are 30 tall.
        let vis = |y| sidebar_row_element_visible(y, 30, 100, 160, 440);
        assert!(vis(160), "fully inside");
        assert!(
            vis(140),
            "half out of the top -- drawn, then cut by the mask"
        );
        assert!(vis(131), "one pixel of it still below list_top");
        assert!(!vis(130), "entirely above the list: nothing to show");
        assert!(vis(399), "half out of the bottom -- drawn, then cut");
        assert!(
            vis(410),
            "wholly inside the bottom mask, still safe to draw"
        );
        assert!(!vis(411), "would cross the panel edge and escape the mask");
        assert!(!vis(99), "would escape above the top mask");
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
    fn tabs_expand_to_tab_stops_and_other_controls_are_dropped() {
        // No font has a glyph for U+0009, so an unexpanded tab renders as a
        // `.notdef` box and logs "No fonts contain glyphs for these
        // codepoints: \u{9}". Expanding to the next stop (not a fixed run of
        // spaces) is what keeps the file's indentation looking like the author
        // wrote it.
        assert_eq!(sanitize_preview_text("\tab"), "    ab");
        assert_eq!(sanitize_preview_text("a\tb"), "a   b");
        assert_eq!(sanitize_preview_text("abc\td"), "abc d");
        assert_eq!(sanitize_preview_text("abcd\te"), "abcd    e");
        // The column resets on every line.
        assert_eq!(sanitize_preview_text("ab\n\tc"), "ab\n    c");
        // CRLF files must not end every line with a box.
        assert_eq!(sanitize_preview_text("a\r\nb"), "a\nb");
        // Other controls are dropped outright; newlines always survive.
        assert_eq!(sanitize_preview_text("a\u{0b}b\n"), "ab\n");
    }

    #[test]
    fn text_without_controls_is_not_reallocated() {
        // The overwhelmingly common case must stay allocation-free.
        let plain = "fn main() {\n    println!(\"hi\");\n}\n";
        assert!(matches!(
            sanitize_preview_text(plain),
            std::borrow::Cow::Borrowed(_)
        ));
        assert!(matches!(
            sanitize_preview_text("has\ttab"),
            std::borrow::Cow::Owned(_)
        ));
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

    #[test]
    fn a_failed_lease_distinguishes_replacements_from_the_connection_that_failed() {
        assert_eq!(
            remote_lease_failure_disposition(Some(("host:key", 7)), "host:key", 7),
            RemoteLeaseFailureDisposition::FailedConnectionInstalled,
        );
        assert_eq!(
            remote_lease_failure_disposition(Some(("host:key", 8)), "host:key", 7),
            RemoteLeaseFailureDisposition::ReplacementForSameTarget,
        );
        assert_eq!(
            remote_lease_failure_disposition(Some(("host:new-key", 8)), "host:key", 7),
            RemoteLeaseFailureDisposition::ReplacementForDifferentTarget,
        );
        assert_eq!(
            remote_lease_failure_disposition(None, "host:key", 7),
            RemoteLeaseFailureDisposition::NoLease,
        );
    }

    /// The distinction the panel is built on. A deleted folder and a macOS TCC
    /// denial are indistinguishable in the error text, so it has to come from
    /// the `ErrorKind` -- and it has to be right, because each one sends the
    /// user somewhere different.
    #[test]
    fn a_vault_failure_is_classified_by_what_the_directory_said() {
        use std::io::ErrorKind;
        // The directory listed, so the vault is fine and the note is not.
        assert_eq!(problem_for_read_dir(None), NoteVaultProblem::Note);
        assert_eq!(
            problem_for_read_dir(Some(ErrorKind::NotFound)),
            NoteVaultProblem::MissingRoot
        );
        // EACCES and EPERM both land here; macOS TCC denials are the latter.
        assert_eq!(
            problem_for_read_dir(Some(ErrorKind::PermissionDenied)),
            NoteVaultProblem::UnreadableRoot
        );
        // Anything unrecognised must degrade to the generic page rather than
        // claim a cause it cannot support.
        assert_eq!(
            problem_for_read_dir(Some(ErrorKind::InvalidData)),
            NoteVaultProblem::Other
        );
    }

    /// Paths break after separators, keep components whole, and when they
    /// must be cut, keep the tail -- the half the user recognises.
    #[test]
    fn paths_wrap_at_separators_and_keep_their_tail() {
        // Measure = one character per 1/16th of the line: 16 chars fit.
        let measure = |segment: &str| segment.chars().count() as f32 / 16.0;

        let lines = super::wrap_path_for_width("/Users/someone/Desktop/order_system", 3, measure);
        assert_eq!(lines, vec!["/Users/someone/", "Desktop/", "order_system"]);
        for line in &lines {
            assert!(
                line.chars().count() <= 16,
                "line overflows its width: {line:?}"
            );
        }

        // A short path stays on one line.
        assert_eq!(
            super::wrap_path_for_width("/tmp/proj", 3, measure),
            vec!["/tmp/proj"]
        );

        // Too many components for the line budget: the tail survives, with a
        // leading ellipsis standing in for what was dropped.
        let deep =
            super::wrap_path_for_width("/one/two/three/four/five/six/seven/eight", 2, measure);
        assert_eq!(deep.len(), 2);
        assert!(deep[0].starts_with('\u{2026}'), "no ellipsis: {deep:?}");
        assert!(deep[1].ends_with("eight"), "tail lost: {deep:?}");

        // A single component wider than the whole line falls back to the
        // character wrap rather than overflowing.
        let long = super::wrap_path_for_width("/a/extraordinarily_long_component_name", 3, measure);
        assert!(long.len() > 1);
    }

    /// Granting the permission and re-reading has to clear the failure even
    /// when the folder turns out to be genuinely empty, because a refused
    /// folder is cached as empty too. Comparing children alone would call that
    /// re-read "no change" and leave the error card up forever -- the user
    /// grants access, nothing happens, and the app looks broken.
    #[test]
    fn a_successful_reread_clears_a_failure_even_when_the_folder_is_empty() {
        let dir = PathBuf::from("/project");
        let mut cache = RightSidebarFileDirCache::default();
        cache.insert_failure(dir.clone(), FolderProblem::UnreadableRoot);
        assert_eq!(cache.failure(&dir), Some(FolderProblem::UnreadableRoot));
        // Cached as empty so the row builder stops re-queueing the read.
        assert_eq!(cache.children(&dir), Some(&[][..]));

        let recovered: Result<Vec<RightSidebarFileDirEntry>, FolderProblem> = Ok(Vec::new());
        assert!(dir_load_changes_cache(&cache, &dir, &recovered));
        cache.insert(dir.clone(), Vec::new());
        assert_eq!(cache.failure(&dir), None);
    }

    /// The other half: an unchanged failure must not churn the cache, or the
    /// re-scan timer would bump the generation and repaint every tick for a
    /// folder whose state never moved.
    #[test]
    fn an_unchanged_dir_load_does_not_touch_the_cache() {
        let dir = PathBuf::from("/project");
        let mut cache = RightSidebarFileDirCache::default();

        // First failure for a directory the cache has never held still counts
        // as a change: there is an empty entry to write.
        let refused: Result<Vec<RightSidebarFileDirEntry>, FolderProblem> =
            Err(FolderProblem::UnreadableRoot);
        assert!(dir_load_changes_cache(&cache, &dir, &refused));
        cache.insert_failure(dir.clone(), FolderProblem::UnreadableRoot);
        assert!(!dir_load_changes_cache(&cache, &dir, &refused));

        // A different problem for the same directory is a change: the card has
        // to stop saying "permission denied" once the folder is deleted.
        let missing: Result<Vec<RightSidebarFileDirEntry>, FolderProblem> =
            Err(FolderProblem::MissingRoot);
        assert!(dir_load_changes_cache(&cache, &dir, &missing));

        // A successful read that matches what is cached, with no failure to
        // clear, is not a change.
        let entry = RightSidebarFileDirEntry {
            path: dir.join("a"),
            name: "a".to_string(),
            is_dir: false,
        };
        cache.insert(dir.clone(), vec![entry.clone()]);
        let same: Result<Vec<RightSidebarFileDirEntry>, FolderProblem> = Ok(vec![entry]);
        assert!(!dir_load_changes_cache(&cache, &dir, &same));
    }

    /// A refused subdirectory must not hide the rest of the tree: it is cached
    /// as empty so the folder still renders, with the failure recorded beside
    /// it for the row to mark.
    #[test]
    fn a_refused_dir_is_still_loaded_so_the_tree_keeps_rendering() {
        let dir = PathBuf::from("/project/secrets");
        let mut cache = RightSidebarFileDirCache::default();
        cache.insert_failure(dir.clone(), FolderProblem::UnreadableRoot);
        assert!(cache.is_loaded(&dir));
        assert!(cache.failure(&dir).is_some());

        cache.clear();
        assert!(!cache.is_loaded(&dir));
        assert_eq!(cache.failure(&dir), None);
    }

    /// The whole point of the panel is that the user can read it, so every
    /// variant must resolve to real text rather than leaking a Fluent key.
    #[test]
    fn every_vault_problem_has_a_translated_title() {
        for problem in [
            NoteVaultProblem::MissingRoot,
            NoteVaultProblem::UnreadableRoot,
            NoteVaultProblem::Note,
            NoteVaultProblem::Other,
        ] {
            let title = problem.title();
            assert!(!title.is_empty(), "empty title for {problem:?}");
            // A missing translation surfaces as the key itself. Both prefixes
            // are checked: the folder-level strings are shared with the Files
            // tree and carry `folder-` keys, so guarding only `right-` would
            // let an untranslated one through.
            assert!(
                !title.starts_with("right-") && !title.starts_with("folder-"),
                "untranslated title {title:?} for {problem:?}"
            );
            if let Some(hint) = problem.hint() {
                assert!(
                    !hint.is_empty() && !hint.starts_with("right-") && !hint.starts_with("folder-"),
                    "untranslated hint {hint:?} for {problem:?}"
                );
            }
        }
    }
}

#[cfg(test)]
mod wiki_link_name_tests {
    use crate::TermWindow;

    /// A wiki link names the note it opens, so a component this system
    /// cannot store is refused rather than rewritten the way a download is:
    /// creating the note under a near-miss of the name would leave the link
    /// pointing at nothing. Off Windows those names are ordinary and stay.
    #[test]
    fn a_note_name_this_system_cannot_store_is_refused() {
        assert_eq!(
            TermWindow::unstorable_note_component("Design/Overview.md"),
            None
        );
        assert_eq!(TermWindow::unstorable_note_component("Overview.md"), None);

        let refused = TermWindow::unstorable_note_component("Design/Q3: Planning.md");
        if cfg!(windows) {
            assert_eq!(refused, Some("Q3: Planning.md"));
        } else {
            assert_eq!(refused, None, "an ordinary note name here");
        }
    }
}
