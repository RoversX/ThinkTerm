use crate::quad::{QuadTrait, TripleLayerQuadAllocator, TripleLayerQuadAllocatorTrait};
use crate::termwindow::ui::icons::{
    material_file_icon_for_name, material_folder_icon_for_name, MaterialIcon, SvgIcon,
};
use crate::termwindow::ui::tokens::{
    CAPSULE_BORDER_WIDTH, ICON_BUTTON_BORDER_WIDTH, SIDEBAR_ICON_GAP, SIDEBAR_INSET,
    SIDEBAR_RESIZE_HANDLE_WIDTH, SIDEBAR_ROW_RADIUS, WINDOW_TAB_ADD_BUTTON_RADIUS,
    WINDOW_TAB_LEADING_ACTION_BUTTON_SIZE, WINDOW_TAB_LEADING_ACTION_GAP,
};
use crate::termwindow::{
    RightSidebarFileCharBag, RightSidebarFileField, RightSidebarFileIndex,
    RightSidebarFileIndexEntry, RightSidebarFileIndexStatus, RightSidebarFilePreviewImage,
    RightSidebarFilePreviewLine, RightSidebarFilePreviewSelection,
    RightSidebarFilePreviewSelectionPoint, RightSidebarFilePreviewSliceCacheKey,
    RightSidebarFilePreviewSliceCacheValue, RightSidebarFilePreviewSpan, RightSidebarFileTreeRow,
    RightSidebarFileView, RightSidebarFileViewState, RightSidebarInputLayout, RightSidebarMode,
    RightSidebarOpenWithCacheEntry, RightSidebarSnippetField, RightSidebarSnippetView,
    TermWindowNotif, UIItem, UIItemType, UiShapeCacheLookup,
};
use crate::ui::TextInputState;
use crate::ui::UiPalette;
use crate::utilsprites::RenderMetrics;
use crate::workspace_threads;
use anyhow::Context;
use config::keyassignment::{ClipboardCopyDestination, ClipboardPasteSource, KeyAssignment};
use mux::Mux;
use std::borrow::Cow;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};
use syntect::easy::HighlightLines;
use syntect::highlighting::{Color as SyntectColor, Style as SyntectStyle, Theme, ThemeSet};
use syntect::parsing::SyntaxSet;
use termwiz::image::{ImageData, ImageDataType};
use termwiz::input::{KeyCode as TermKeyCode, Modifiers as TermModifiers};
use walkdir::{DirEntry as WalkDirEntry, WalkDir};
use wezterm_font::LoadedFont;
use window::color::LinearRgba;
use window::{
    Clipboard, ContextMenuItem, IntegratedTitleButtonStyle, WindowDecorations, WindowOps,
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
const FILE_FONT_MIN_SIZE: f64 = 14.0;
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
// Per-line we keep the *full* content (bounded only by FILE_PREVIEW_MAX_BYTES
// for the whole file) so minified CSS/JS, lockfiles and JSON aren't truncated.
// We only bound the *syntax-highlighting* work per line: characters past this
// point are rendered in the default colour instead of being dropped. This keeps
// syntect cost bounded on pathological single-line files without losing data.
const FILE_PREVIEW_HIGHLIGHT_CHAR_LIMIT: usize = 8192;
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
// How often the Files panel re-scans the tree while it's visible + focused.
const FILE_INDEX_RESCAN_SECS: u64 = 90;
// Max number of (root, project) view-state snapshots kept in memory.
const FILE_VIEW_STATE_CACHE_CAP: usize = 32;
const FILE_FILTER_DEBOUNCE_MS: u64 = 350;

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

    fn empty_label(self) -> &'static str {
        match self {
            Self::Chat => "Coming soon",
            Self::Tasks => "Coming soon",
            Self::Snippets => "No snippets",
        }
    }
}

pub fn right_sidebar_width_for_metrics(render_metrics: &RenderMetrics) -> usize {
    let default_width = (render_metrics.cell_size.width as usize * RIGHT_SIDEBAR_WIDTH_CELLS)
        .max(RIGHT_SIDEBAR_MIN_WIDTH);
    crate::native_settings::right_sidebar_width()
        .unwrap_or(default_width)
        .clamp(RIGHT_SIDEBAR_MIN_WIDTH, RIGHT_SIDEBAR_MAX_WIDTH)
}

pub fn right_sidebar_file_preview_width() -> usize {
    crate::native_settings::right_sidebar_file_preview_width()
        .unwrap_or(FILE_PREVIEW_PANE_DEFAULT_WIDTH)
        .max(FILE_PREVIEW_PANE_MIN_WIDTH)
}

impl crate::TermWindow {
    fn right_sidebar_file_preview_active(&self) -> bool {
        !self.right_sidebar_collapsed
            && self.right_sidebar_mode == RightSidebarMode::Chat
            && self.right_sidebar_file_view == RightSidebarFileView::Preview
            && self.right_sidebar_file_selected.is_some()
    }

    fn right_sidebar_tree_width(&self) -> usize {
        let width = if self.right_sidebar_file_preview_active() {
            self.right_sidebar_file_tree_width
        } else {
            self.right_sidebar_width
        };
        width.clamp(RIGHT_SIDEBAR_MIN_WIDTH, self.right_sidebar_max_width())
    }

    fn right_sidebar_available_width(&self) -> usize {
        let border = self.get_os_border();
        self.dimensions
            .pixel_width
            .saturating_sub((border.left + border.right).get() as usize)
    }

    fn right_sidebar_file_preview_total_max_width(&self) -> usize {
        let available_width = self.right_sidebar_available_width();
        let content_width = available_width.saturating_sub(self.workspace_sidebar_width());
        let min_preview_total = RIGHT_SIDEBAR_MIN_WIDTH + FILE_PREVIEW_PANE_MIN_WIDTH;
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
            || self.right_sidebar_file_selected.is_none()
        {
            return None;
        }

        let max_preview_width = self
            .right_sidebar_file_preview_total_max_width()
            .saturating_sub(self.right_sidebar_tree_width());
        if max_preview_width < FILE_PREVIEW_PANE_MIN_WIDTH {
            return None;
        }

        let configured_width = if self.right_sidebar_file_preview_width == 0 {
            FILE_PREVIEW_PANE_DEFAULT_WIDTH
        } else {
            self.right_sidebar_file_preview_width
        };
        let width = configured_width.clamp(FILE_PREVIEW_PANE_MIN_WIDTH, max_preview_width);
        Some(width)
    }

    pub fn right_sidebar_width(&self) -> usize {
        if self.right_sidebar_collapsed {
            0
        } else {
            self.right_sidebar_tree_width()
                .saturating_add(self.right_sidebar_file_preview_width().unwrap_or(0))
                .min(if self.right_sidebar_file_preview_active() {
                    self.right_sidebar_file_preview_total_max_width()
                } else {
                    self.right_sidebar_available_width()
                })
        }
    }

    pub fn right_sidebar_max_width(&self) -> usize {
        let available_width = self.right_sidebar_available_width();
        let proportional_max = (available_width * 2 / 3).max(RIGHT_SIDEBAR_MIN_WIDTH);
        RIGHT_SIDEBAR_MAX_WIDTH.min(proportional_max)
    }

    fn right_sidebar_window_button_reserved_width(&self) -> usize {
        if cfg!(target_os = "macos")
            || !self
                .config
                .window_decorations
                .contains(WindowDecorations::INTEGRATED_BUTTONS)
            || self.config.integrated_title_button_style == IntegratedTitleButtonStyle::MacOsNative
        {
            return 0;
        }

        self.config.integrated_title_buttons.len()
            * (WINDOW_TAB_LEADING_ACTION_BUTTON_SIZE + WINDOW_TAB_LEADING_ACTION_GAP / 2)
            + WINDOW_TAB_LEADING_ACTION_GAP
    }

    pub fn set_right_sidebar_width(&mut self, width: usize) {
        self.right_sidebar_width =
            width.clamp(RIGHT_SIDEBAR_MIN_WIDTH, self.right_sidebar_max_width());
    }

    fn set_right_sidebar_file_tree_width(&mut self, width: usize) -> bool {
        let old_width = self.right_sidebar_file_tree_width;
        self.right_sidebar_file_tree_width =
            width.clamp(RIGHT_SIDEBAR_MIN_WIDTH, self.right_sidebar_max_width());
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
            .right_sidebar_file_preview_total_max_width()
            .saturating_sub(self.right_sidebar_tree_width());
        if max_preview_width < FILE_PREVIEW_PANE_MIN_WIDTH {
            return false;
        }

        let old_width = self.right_sidebar_file_preview_width;
        self.right_sidebar_file_preview_width =
            width.clamp(FILE_PREVIEW_PANE_MIN_WIDTH, max_preview_width);
        old_width != self.right_sidebar_file_preview_width
    }

    pub fn persist_right_sidebar_width(&self) {
        let width = self
            .right_sidebar_width
            .clamp(RIGHT_SIDEBAR_MIN_WIDTH, RIGHT_SIDEBAR_MAX_WIDTH);
        if let Err(err) = crate::native_settings::save_right_sidebar_width(width) {
            log::warn!("failed to save right sidebar width: {err:#}");
        }
    }

    pub fn persist_right_sidebar_file_preview_width(&self) {
        let width = self
            .right_sidebar_file_preview_width()
            .unwrap_or(self.right_sidebar_file_preview_width)
            .max(FILE_PREVIEW_PANE_MIN_WIDTH);
        if let Err(err) = crate::native_settings::save_right_sidebar_file_preview_width(width) {
            log::warn!("failed to save right sidebar file preview width: {err:#}");
        }
    }

    pub fn toggle_right_sidebar(&mut self) {
        self.right_sidebar_collapsed = !self.right_sidebar_collapsed;
        if self.right_sidebar_collapsed {
            self.schedule_right_sidebar_file_memory_release();
        } else {
            self.kick_right_sidebar_file_rescan_cycle();
        }
    }

    pub fn expand_right_sidebar(&mut self) {
        self.right_sidebar_collapsed = false;
        self.kick_right_sidebar_file_rescan_cycle();
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
        if token != self.right_sidebar_file_memory_release_token
            || self.right_sidebar_file_view_active()
            || matches!(
                self.right_sidebar_file_index_status,
                RightSidebarFileIndexStatus::Indexing
            )
        {
            return;
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
        self.invalidate_window();
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
        if !self.right_sidebar_file_view_state_by_root.contains_key(&key) {
            self.right_sidebar_file_view_state_order.push_back(key.clone());
        }
        self.right_sidebar_file_view_state_by_root.insert(key, state);
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
        self.right_sidebar_file_filter.set_text_end(state.filter.clone());
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
            RightSidebarMode::Tasks => false,
        }
    }

    pub(crate) fn clear_right_sidebar_text_focus(&mut self) {
        self.right_sidebar_snippet_focus = None;
        self.right_sidebar_file_focus = None;
    }

    pub(crate) fn open_new_snippet_editor(&mut self) {
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
        let body = self.right_sidebar_snippet_body.text.trim().to_string();
        if body.is_empty() {
            self.right_sidebar_snippet_focus = Some(RightSidebarSnippetField::Body);
            return;
        }
        let title = self.right_sidebar_snippet_title.text.trim().to_string();
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
        let delta = (steps * 6.0).min(42.0);
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

    pub(crate) fn scroll_right_sidebar_files(&mut self, amount: i16) -> bool {
        if self.right_sidebar_collapsed
            || self.right_sidebar_mode != RightSidebarMode::Chat
            || amount == 0
        {
            return false;
        }

        let old = self.right_sidebar_file_tree_scroll_offset;
        let steps = amount.unsigned_abs().max(1) as f32;
        let delta = (steps * 14.0).min(98.0);
        if amount < 0 {
            self.right_sidebar_file_tree_scroll_offset += delta;
        } else {
            self.right_sidebar_file_tree_scroll_offset =
                (self.right_sidebar_file_tree_scroll_offset - delta).max(0.0);
        }
        (old - self.right_sidebar_file_tree_scroll_offset).abs() > f32::EPSILON
    }

    pub(crate) fn scroll_right_sidebar_file_preview(&mut self, amount: i16) -> bool {
        if self.right_sidebar_collapsed
            || self.right_sidebar_mode != RightSidebarMode::Chat
            || self.right_sidebar_file_view != RightSidebarFileView::Preview
            || self.right_sidebar_file_selected.is_none()
            || amount == 0
        {
            return false;
        }

        let old = self.right_sidebar_file_preview_scroll_offset;
        let max = self.right_sidebar_file_preview_scroll_max();
        let steps = amount.unsigned_abs().max(1) as f32;
        let delta = (steps * 14.0).min(98.0);
        if amount < 0 {
            self.right_sidebar_file_preview_scroll_offset =
                (self.right_sidebar_file_preview_scroll_offset + delta).clamp(0.0, max);
        } else {
            self.right_sidebar_file_preview_scroll_offset =
                (self.right_sidebar_file_preview_scroll_offset - delta).clamp(0.0, max);
        }
        (old - self.right_sidebar_file_preview_scroll_offset).abs() > f32::EPSILON
    }

    pub(crate) fn scroll_right_sidebar_file_preview_horizontal(&mut self, amount: i16) -> bool {
        if self.right_sidebar_collapsed
            || self.right_sidebar_mode != RightSidebarMode::Chat
            || self.right_sidebar_file_view != RightSidebarFileView::Preview
            || self.right_sidebar_file_selected.is_none()
            || amount == 0
        {
            return false;
        }

        let old = self.right_sidebar_file_preview_horizontal_offset;
        let max = self.right_sidebar_file_preview_horizontal_scroll_max();
        let steps = amount.unsigned_abs().max(1) as usize;
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
                .right_sidebar_file_preview_total_max_width()
                .saturating_sub(FILE_PREVIEW_PANE_MIN_WIDTH)
                .max(RIGHT_SIDEBAR_MIN_WIDTH);
            self.right_sidebar_file_tree_width = self
                .right_sidebar_width
                .clamp(RIGHT_SIDEBAR_MIN_WIDTH, max_tree_for_preview);
        }
        self.right_sidebar_file_selected = Some(path.clone());
        self.right_sidebar_file_preview_generation =
            self.right_sidebar_file_preview_generation.wrapping_add(1);
        let generation = self.right_sidebar_file_preview_generation;
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
                Ok(load_right_sidebar_file_preview(
                    &load_path,
                    use_dark_syntax_theme,
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
            let mut cache = self.right_sidebar_file_preview_line_color_cache.borrow_mut();
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
                "arrow.up.forward.app",
                KeyAssignment::OpenFileWithSystemDefault(path_string.clone()),
            ));
        }
        items.push(ContextMenuItem::item_with_icon(
            "Reveal in Folder",
            "folder",
            KeyAssignment::RevealFileInFolder(path_string.clone()),
        ));
        items.push(ContextMenuItem::item_with_icon(
            "Copy Path",
            "doc.on.doc",
            KeyAssignment::CopyFilePathToClipboard(path_string.clone()),
        ));
        items.push(ContextMenuItem::Separator);
        items.push(ContextMenuItem::item_with_icon(
            "Rename...",
            "pencil",
            KeyAssignment::RenameSidebarFile(path_string.clone()),
        ));
        items.push(ContextMenuItem::item_with_icon(
            "Move to Trash",
            "trash",
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
        candidates
            .retain(|candidate| open_with_candidate_allowed(candidate, &custom_ids, saved_id.as_deref()));

        let mut items: Vec<ContextMenuItem> =
            sorted_open_with_candidates(candidates, current_id.as_deref())
                .into_iter()
                .filter(|candidate| current_id.as_deref() != Some(candidate.id.as_str()))
                .map(|candidate| {
                    ContextMenuItem::item_with_icon(
                        format!("Open With {}", candidate.label),
                        "app",
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
            "app",
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
        if self.right_sidebar_file_filter.text == self.right_sidebar_file_applied_filter {
            self.right_sidebar_file_filter_debounce_until = None;
        } else {
            self.right_sidebar_file_filter_debounce_until =
                Some(Instant::now() + Duration::from_millis(FILE_FILTER_DEBOUNCE_MS));
        }
    }

    fn right_sidebar_file_filter_for_tree(&mut self) -> String {
        let current = self.right_sidebar_file_filter.text.clone();
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
        let Some(window) = self.window.as_ref().cloned() else {
            return;
        };
        self.right_sidebar_file_rescan_token =
            self.right_sidebar_file_rescan_token.wrapping_add(1);
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
        let Some(input) = self.right_sidebar_focused_input() else {
            return;
        };
        let text = input
            .caret_selected_text()
            .unwrap_or_else(|| input.text.clone());
        if !text.is_empty() {
            self.copy_to_clipboard(destination, text);
        }
    }

    pub(crate) fn cut_right_sidebar_focused_input(&mut self) {
        let Some(input) = self.right_sidebar_focused_input_mut() else {
            return;
        };
        let text = if let Some(text) = input.caret_take_selected_text() {
            text
        } else {
            let text = input.text.clone();
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

        let shift = mods.contains(TermModifiers::SHIFT);
        let super_ = mods.contains(TermModifiers::SUPER);
        let alt = mods.contains(TermModifiers::ALT);
        let ctrl = mods.contains(TermModifiers::CTRL);
        let multiline = self.right_sidebar_focused_is_multiline();

        // Cmd shortcuts (macOS): clipboard, select-all, jump to line start/end.
        if super_ && !alt && !ctrl {
            return match key {
                TermKeyCode::Char('a') | TermKeyCode::Char('A') => {
                    if let Some(input) = self.right_sidebar_focused_input_mut() {
                        input.caret_select_all();
                    }
                    true
                }
                TermKeyCode::Char('c') | TermKeyCode::Char('C') => {
                    self.copy_right_sidebar_focused_input(ClipboardCopyDestination::Clipboard);
                    true
                }
                TermKeyCode::Char('x') | TermKeyCode::Char('X') => {
                    self.cut_right_sidebar_focused_input();
                    true
                }
                TermKeyCode::Char('v') | TermKeyCode::Char('V') => {
                    self.paste_into_right_sidebar_from_clipboard(ClipboardPasteSource::Clipboard);
                    true
                }
                TermKeyCode::LeftArrow if !multiline => {
                    if let Some(input) = self.right_sidebar_focused_input_mut() {
                        input.caret_move_home(shift);
                    }
                    true
                }
                TermKeyCode::RightArrow if !multiline => {
                    if let Some(input) = self.right_sidebar_focused_input_mut() {
                        input.caret_move_end(shift);
                    }
                    true
                }
                TermKeyCode::Backspace if !multiline => {
                    if let Some(input) = self.right_sidebar_focused_input_mut() {
                        input.caret_delete_to_start();
                    }
                    self.after_right_sidebar_text_edit();
                    true
                }
                _ => false,
            };
        }

        // Option/Alt shortcuts (macOS): word navigation / deletion.
        if alt && !super_ && !ctrl {
            return match key {
                TermKeyCode::LeftArrow if !multiline => {
                    if let Some(input) = self.right_sidebar_focused_input_mut() {
                        input.caret_word_left(shift);
                    }
                    true
                }
                TermKeyCode::RightArrow if !multiline => {
                    if let Some(input) = self.right_sidebar_focused_input_mut() {
                        input.caret_word_right(shift);
                    }
                    true
                }
                TermKeyCode::Backspace if !multiline => {
                    if let Some(input) = self.right_sidebar_focused_input_mut() {
                        input.caret_delete_word_back();
                    }
                    self.after_right_sidebar_text_edit();
                    true
                }
                _ => false,
            };
        }

        if ctrl {
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
        let multiline = self.right_sidebar_focused_is_multiline();
        let Some(input) = self.right_sidebar_focused_input_mut() else {
            return false;
        };
        input.caret_insert(text, multiline);
        self.after_right_sidebar_text_edit();
        true
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
        let chars: Vec<char> = input.text.chars().collect();
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
        let min_preview = FILE_PREVIEW_PANE_MIN_WIDTH;
        let max_preview = total_rect.width.saturating_sub(RIGHT_SIDEBAR_MIN_WIDTH);
        let min_tree = RIGHT_SIDEBAR_MIN_WIDTH;
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

    fn right_sidebar_file_preview_font_size(&self) -> f64 {
        let settings = crate::native_settings::load();
        let base_font_size = crate::native_settings::home_font_size(&settings);
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
            || self.right_sidebar_file_selected.is_none()
        {
            return None;
        }

        let rect = self.right_sidebar_file_preview_rect()?;
        let content_x = rect.x + SIDEBAR_INSET * 2;
        let content_width = rect.width.saturating_sub(SIDEBAR_INSET * 4);
        let content_top = rect.y + SIDEBAR_INSET * 2;
        let content_bottom = rect.y.saturating_add(rect.height);
        let y = content_top + FILE_PREVIEW_HEADER_HEIGHT;
        let bottom = content_bottom.saturating_sub(SIDEBAR_INSET);
        let visible_height = bottom.saturating_sub(y);
        if visible_height == 0 || content_width == 0 {
            return None;
        }

        let line_height = preview_metrics.cell_size.height as usize + 4;
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
                SIDEBAR_INSET + 4
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
        );
        let text_x = metrics
            .x
            .saturating_add(number_width)
            .saturating_add(SIDEBAR_ICON_GAP);
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
                SIDEBAR_INSET + FILE_PREVIEW_SCROLLBAR_THICKNESS
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

        let track_width = FILE_PREVIEW_SCROLLBAR_THICKNESS;
        let track_height = visible_height.max(1);
        let thumb_height = ((visible_height as f32 / metrics.total_height as f32)
            * track_height as f32)
            .clamp(28.0, track_height as f32);
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
        let track_height = FILE_PREVIEW_SCROLLBAR_THICKNESS;
        let thumb_width = ((visible_columns as f32 / max_columns as f32) * track_width as f32)
            .clamp(28.0, track_width as f32);
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

    fn right_sidebar_snippet_scroll_height(snippet_count: usize, viewport_height: usize) -> usize {
        let row_height = SNIPPET_CARD_HEIGHT + SNIPPET_ROW_GAP;
        let height = snippet_count
            .saturating_mul(row_height)
            .saturating_sub(SNIPPET_ROW_GAP);
        if height > viewport_height {
            height.saturating_add(SNIPPET_LIST_BOTTOM_PADDING)
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

        let top_bar_y = rect.y + SIDEBAR_INSET * 2;
        let top_bar_height = RIGHT_SIDEBAR_TOP_BAR_HEIGHT.min(
            rect.y
                .saturating_add(rect.height)
                .saturating_sub(top_bar_y + SIDEBAR_INSET),
        );
        let content_top =
            top_bar_y + top_bar_height + RIGHT_SIDEBAR_MODE_HEIGHT + RIGHT_SIDEBAR_SECTION_GAP;
        let list_top =
            content_top + SNIPPET_TOOLBAR_HEIGHT.max(SNIPPET_SEARCH_HEIGHT) + SNIPPET_LIST_TOP_GAP;
        let content_bottom = rect.y.saturating_add(rect.height);
        let visible_height = content_bottom.saturating_sub(list_top + SIDEBAR_INSET);
        if visible_height == 0 {
            return None;
        }
        let snippet_count = self.filtered_snippet_count();
        let total_height = Self::right_sidebar_snippet_scroll_height(snippet_count, visible_height);
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
            .clamp(28.0, track_height as f32);
        let travel = (track_height as f32 - thumb_height).max(1.0);
        let scroll_offset = self
            .right_sidebar_snippet_scroll_offset
            .clamp(0.0, max_scroll);
        let thumb_y = list_top as f32 + (scroll_offset / max_scroll) * travel;
        let track_x = rect
            .x
            .saturating_add(rect.width)
            .saturating_sub(SIDEBAR_INSET / 2 + track_width);

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
            .text
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
        let base_font_size = crate::native_settings::home_font_size(&settings);
        let ui_font = self
            .fonts
            .title_font_with_size(base_font_size)
            .context("right sidebar ui font")?;
        let ui_metrics = RenderMetrics::with_font_metrics(&ui_font.metrics());
        let ui_cell_height = ui_metrics.cell_size.height as usize;
        let icon_size = (ui_cell_height + 6).clamp(20, 24);

        if self.right_sidebar_mode == RightSidebarMode::Chat {
            let _ = self.sync_right_sidebar_file_root_for_current_workspace();
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
            x: total_rect.x.saturating_sub(SIDEBAR_RESIZE_HANDLE_WIDTH / 2),
            y: total_rect.y,
            width: SIDEBAR_RESIZE_HANDLE_WIDTH,
            height: total_rect.height,
            item_type: UIItemType::RightSidebarResize,
        });
        if self.right_sidebar_file_preview_rect().is_some() {
            self.ui_items.push(UIItem {
                x: rect.x.saturating_sub(SIDEBAR_RESIZE_HANDLE_WIDTH / 2),
                y: rect.y,
                width: SIDEBAR_RESIZE_HANDLE_WIDTH,
                height: rect.height,
                item_type: UIItemType::RightSidebarFilePreviewResize,
            });
        }

        let content_x = rect.x + SIDEBAR_INSET * 2;
        let content_width = rect.width.saturating_sub(SIDEBAR_INSET * 4);
        let top_bar_y = rect.y + SIDEBAR_INSET * 2;
        let top_bar_height = RIGHT_SIDEBAR_TOP_BAR_HEIGHT.min(
            rect.y
                .saturating_add(rect.height)
                .saturating_sub(top_bar_y + SIDEBAR_INSET),
        );
        if top_bar_height == 0 {
            return Ok(());
        }

        if cfg!(target_os = "macos") {
            let close_button_size = RIGHT_SIDEBAR_CLOSE_BUTTON_SIZE
                .min(top_bar_height)
                .min(content_width)
                .max(1);
            let window_button_reserve = self.right_sidebar_window_button_reserved_width();
            let close_button_right_limit = rect
                .x
                .saturating_add(rect.width)
                .saturating_sub(SIDEBAR_INSET)
                .saturating_sub(close_button_size)
                .saturating_sub(window_button_reserve);
            let close_button_x = content_x
                .saturating_add(content_width)
                .saturating_sub(close_button_size)
                .saturating_add(RIGHT_SIDEBAR_CLOSE_BUTTON_X_ADJUST)
                .min(close_button_right_limit);
            let close_button_y = (top_bar_y
                + (top_bar_height.saturating_sub(close_button_size)) / 2)
                .saturating_sub(RIGHT_SIDEBAR_CLOSE_BUTTON_Y_ADJUST);
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
            let close_icon_size = RIGHT_SIDEBAR_CLOSE_ICON_SIZE
                .min(close_visual_size.saturating_sub(4))
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

        let mode_y = top_bar_y + top_bar_height + RIGHT_SIDEBAR_SECTION_GAP;
        let mode_height = RIGHT_SIDEBAR_MODE_HEIGHT.min(
            rect.y
                .saturating_add(rect.height)
                .saturating_sub(mode_y + SIDEBAR_INSET),
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
        let mode_icon_size = (ui_cell_height + 12)
            .clamp(24, 30)
            .min(mode_height.saturating_sub(22))
            .max(1);
        let active_label_target_width = self
            .sidebar_text_width(&ui_font, self.right_sidebar_mode.label())?
            .ceil() as usize
            + MODE_LABEL_CLIP_SLOP;
        let inactive_segment_min_width = (mode_icon_size + SIDEBAR_INSET * 4)
            .max(70)
            .min((content_width / modes.len()).max(1));
        let inactive_segment_count = modes.len().saturating_sub(1);
        let inactive_segments_min_width =
            inactive_segment_min_width.saturating_mul(inactive_segment_count);
        let active_segment_width =
            (mode_icon_size + SIDEBAR_ICON_GAP + active_label_target_width + SIDEBAR_INSET * 6)
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
                let inner_inset = 5;
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
                        (segment_x + 5) as f32,
                        (mode_y + 5) as f32,
                        segment_width.saturating_sub(10) as f32,
                        mode_height.saturating_sub(10) as f32,
                    ),
                    chrome.sidebar_button_hover_bg,
                    WINDOW_TAB_ADD_BUTTON_RADIUS,
                )
                .context("right sidebar hovered mode")?;
            }

            let label_width = if active {
                segment_width
                    .saturating_sub(mode_icon_size + SIDEBAR_ICON_GAP + SIDEBAR_INSET * 2)
                    .min(active_label_target_width)
            } else {
                0
            };
            let total_width = if active {
                mode_icon_size + SIDEBAR_ICON_GAP + label_width
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
                    icon_x + mode_icon_size + SIDEBAR_ICON_GAP,
                    mode_y + (mode_height.saturating_sub(ui_cell_height)) / 2,
                    label_width,
                    foreground,
                )?;
            }
            segment_x = segment_right;
        }

        let content_top = mode_y + mode_height + RIGHT_SIDEBAR_SECTION_GAP;
        match self.right_sidebar_mode {
            RightSidebarMode::Chat => {
                let file_font_size = self.right_sidebar_file_preview_font_size();
                let file_font = self
                    .fonts
                    .title_font_with_size(file_font_size)
                    .context("right sidebar file font")?;
                let file_metrics = RenderMetrics::with_font_metrics(&file_font.metrics());
                let file_icon_size = (file_metrics.cell_size.height as usize + 6).clamp(22, 28);
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
            RightSidebarMode::Tasks => {}
        }

        let empty_top = content_top;
        let empty_height = RIGHT_SIDEBAR_EMPTY_HEIGHT.min(
            rect.y
                .saturating_add(rect.height)
                .saturating_sub(empty_top + SIDEBAR_INSET),
        );
        if empty_height == 0 {
            return Ok(());
        }
        self.fill_rounded_rectangle_with_border(
            layers,
            1,
            euclid::rect(
                content_x as f32,
                empty_top as f32,
                content_width as f32,
                empty_height as f32,
            ),
            chrome.sidebar_button_bg,
            chrome.control_border,
            SIDEBAR_ROW_RADIUS + 6.0,
            CAPSULE_BORDER_WIDTH,
        )
        .context("right sidebar empty state")?;
        let empty_icon_size = icon_size
            .min(22)
            .min(empty_height.saturating_sub(20))
            .max(1);
        let empty_icon_x = content_x + SIDEBAR_INSET + 2;
        let empty_icon_y = empty_top + (empty_height.saturating_sub(empty_icon_size)) / 2;
        self.paint_sidebar_icon(
            layers,
            self.right_sidebar_mode.icon(),
            empty_icon_x,
            empty_icon_y,
            empty_icon_size,
            muted_fg,
        )?;
        self.paint_sidebar_text(
            layers,
            &ui_font,
            ui_metrics,
            self.right_sidebar_mode.empty_label(),
            empty_icon_x + empty_icon_size + SIDEBAR_ICON_GAP + 2,
            empty_top + (empty_height.saturating_sub(ui_cell_height)) / 2,
            content_width.saturating_sub(empty_icon_size + SIDEBAR_ICON_GAP + SIDEBAR_INSET * 3),
            muted_fg,
        )?;

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

        let content_x = rect.x + SIDEBAR_INSET * 2;
        let content_width = rect.width.saturating_sub(SIDEBAR_INSET * 4);
        let content_top = rect.y + SIDEBAR_INSET * 2;
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

    fn active_local_project_for_files(&self) -> Result<RightSidebarFileRoot, String> {
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
        let refresh_size = (ui_metrics.cell_size.height as usize + 12).clamp(28, 38);
        let refresh_gap = SIDEBAR_INSET;
        let filter_width = content_width.saturating_sub(refresh_size + refresh_gap);
        let refresh_x = content_x + content_width - refresh_size;
        let refresh_y = content_top + FILE_FILTER_HEIGHT.saturating_sub(refresh_size) / 2;

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
            FILE_FILTER_HEIGHT,
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

        let tree_top = content_top + FILE_FILTER_HEIGHT + FILE_TREE_TOP_GAP;
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
        let viewport_bottom = content_bottom.saturating_sub(SIDEBAR_INSET);
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
            let row_top = tree_top_f + (idx * row_metrics.row_height) as f32 - scroll_offset;
            let row_bottom = row_top + row_metrics.row_height as f32;
            if row_bottom <= tree_top_f {
                continue;
            }
            if row_top >= viewport_bottom_f {
                break;
            }
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
            let fade_top = content_top + FILE_FILTER_HEIGHT;
            let fade_height = FILE_TREE_TOP_GAP
                .saturating_add(FILE_SCROLL_FADE_HEIGHT)
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
                content_width,
                FILE_FILTER_HEIGHT,
                Some(SvgIcon::Search),
                "Filter files",
                &filter_input,
                self.right_sidebar_file_focus == Some(RightSidebarFileField::Filter),
                UIItemType::RightSidebarFileFilter,
                false,
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
        let height =
            RIGHT_SIDEBAR_EMPTY_HEIGHT.min(content_bottom.saturating_sub(y + SIDEBAR_INSET));
        if height == 0 {
            return Ok(());
        }
        self.fill_rounded_rectangle_with_border(
            layers,
            1,
            euclid::rect(x as f32, y as f32, width as f32, height as f32),
            chrome.sidebar_button_bg,
            chrome.control_border,
            SIDEBAR_ROW_RADIUS + 6.0,
            CAPSULE_BORDER_WIDTH,
        )
        .context("right sidebar files message")?;
        let empty_icon_size = icon_size.min(22).min(height.saturating_sub(20)).max(1);
        let icon_x = x + SIDEBAR_INSET + 2;
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
            icon_x + empty_icon_size + SIDEBAR_ICON_GAP + 2,
            y + (height.saturating_sub(ui_metrics.cell_size.height as usize)) / 2,
            width.saturating_sub(empty_icon_size + SIDEBAR_ICON_GAP + SIDEBAR_INSET * 3),
            muted_fg,
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
                SIDEBAR_ROW_RADIUS,
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
        let chevron_x = x + SIDEBAR_INSET + indent;
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
                .saturating_sub(text_x + SIDEBAR_INSET),
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
        let available_after_back = content_width.saturating_sub(button_size + SIDEBAR_INSET);
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
            desired_open_width.min(max_open_width).max(80)
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

        let title_x = content_x + button_size + SIDEBAR_INSET;
        let title_right = actions_x.saturating_sub(SIDEBAR_INSET);
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

        if main_width >= 28 {
            let text_x = x + 12;
            let text_right = x + main_width.saturating_sub(10);
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
            let chevron_size = (height * 40 / 100).clamp(14, 18);
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
        let icon_size = (size * 58 / 100).max(16);
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
        let Some(path) = self.right_sidebar_file_selected.clone() else {
            return Ok(());
        };
        let mut profile = FilePreviewPaintProfile::new();
        let mut profile_scroll_offset = 0.0;
        let mut profile_horizontal_offset = 0usize;

        let Some(metrics) = self.right_sidebar_file_preview_body_metrics(ui_metrics) else {
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
                &path,
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
                FILE_SCROLL_FADE_HEIGHT.min(body_bottom.saturating_sub(metrics.y)),
            )?;
        }
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
            &path,
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

        let hit_slop = FILE_PREVIEW_SCROLLBAR_HIT_SLOP;
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
                .saturating_sub(FILE_PREVIEW_SCROLLBAR_HIT_SLOP),
            width: scroll.track_width,
            height: scroll.track_height + FILE_PREVIEW_SCROLLBAR_HIT_SLOP * 2,
            item_type: UIItemType::RightSidebarFilePreviewHorizontalScrollTrack,
        });
        self.ui_items.push(UIItem {
            x: scroll.thumb_x.round().max(0.0) as usize,
            y: scroll
                .track_y
                .saturating_sub(FILE_PREVIEW_SCROLLBAR_HIT_SLOP),
            width: scroll.thumb_width.round().max(1.0) as usize,
            height: scroll.track_height + FILE_PREVIEW_SCROLLBAR_HIT_SLOP * 2,
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
            self.paint_cached_ui_shape_clipped(
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
            self.paint_cached_ui_shape_clipped(
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
        let new_button_icon_size = (ui_metrics.cell_size.height as usize + 2).clamp(18, 22);
        let new_button_label_width =
            self.sidebar_text_width(ui_font, "New Snippet")?.ceil() as usize;
        let toolbar_gap = SNIPPET_ROW_GAP;
        let search_is_active = self.right_sidebar_snippet_focus
            == Some(RightSidebarSnippetField::Search)
            || !self.right_sidebar_snippet_search.text.is_empty();
        let full_new_button_width =
            new_button_icon_size + SIDEBAR_ICON_GAP + new_button_label_width + SIDEBAR_INSET * 4;
        let min_search_width = 160.min(content_width);
        let collapse_new_button = search_is_active
            || content_width < full_new_button_width + toolbar_gap + min_search_width;
        let new_button_width = if collapse_new_button {
            SNIPPET_TOOLBAR_HEIGHT.min(content_width)
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
            SNIPPET_TOOLBAR_HEIGHT,
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
                SNIPPET_SEARCH_HEIGHT,
                Some(SvgIcon::Search),
                "Search",
                &search_input,
                self.right_sidebar_snippet_focus == Some(RightSidebarSnippetField::Search),
                UIItemType::RightSidebarSnippetSearch,
                false,
            )?;
        }

        let list_top =
            toolbar_y + SNIPPET_TOOLBAR_HEIGHT.max(SNIPPET_SEARCH_HEIGHT) + SNIPPET_LIST_TOP_GAP;
        let needle = self
            .right_sidebar_snippet_search
            .text
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
        let row_height = SNIPPET_CARD_HEIGHT + SNIPPET_ROW_GAP;
        let visible_height = content_bottom.saturating_sub(list_top + SIDEBAR_INSET);
        let total_height =
            Self::right_sidebar_snippet_scroll_height(snippets.len(), visible_height);
        let max_scroll = total_height.saturating_sub(visible_height) as f32;
        self.right_sidebar_snippet_scroll_offset = self
            .right_sidebar_snippet_scroll_offset
            .clamp(0.0, max_scroll);
        let scroll_offset = self.right_sidebar_snippet_scroll_offset;

        if snippets.is_empty() {
            let empty_height = RIGHT_SIDEBAR_EMPTY_HEIGHT
                .min(content_bottom.saturating_sub(list_top + SIDEBAR_INSET));
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
                SIDEBAR_ROW_RADIUS + 6.0,
                CAPSULE_BORDER_WIDTH,
            )
            .context("right sidebar snippets empty state")?;
            let empty_icon_size = icon_size
                .min(22)
                .min(empty_height.saturating_sub(20))
                .max(1);
            let empty_icon_x = content_x + SIDEBAR_INSET + 2;
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
                empty_icon_x + empty_icon_size + SIDEBAR_ICON_GAP + 2,
                list_top + (empty_height.saturating_sub(ui_metrics.cell_size.height as usize)) / 2,
                content_width
                    .saturating_sub(empty_icon_size + SIDEBAR_ICON_GAP + SIDEBAR_INSET * 3),
                muted_fg,
            )?;
            return Ok(());
        }

        let list_top_f = list_top as f32;
        let content_bottom = content_bottom.saturating_sub(SIDEBAR_INSET);
        let content_bottom_f = content_bottom as f32;
        for (idx, snippet) in snippets.into_iter().enumerate() {
            let row_top = list_top_f + (idx * row_height) as f32 - scroll_offset;
            let row_bottom = row_top + SNIPPET_CARD_HEIGHT as f32;
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
        let back_size = 44;
        let header_top = header_y + 8;
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

        let title_x = content_x + back_size + SIDEBAR_INSET;
        let save_label_width = self.sidebar_text_width(ui_font, "Save")?.ceil() as usize;
        let save_width = (save_label_width + SIDEBAR_INSET * 6)
            .clamp(110, 136)
            .min(content_width.saturating_sub(back_size + SIDEBAR_INSET * 2));
        let save_x = content_x + content_width.saturating_sub(save_width);
        let title_width = save_x.saturating_sub(title_x + SIDEBAR_INSET * 2);
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
            SNIPPET_SAVE_BUTTON_HEIGHT.min(SNIPPET_EDITOR_HEADER_HEIGHT - 12),
            None,
            "Save",
            UIItemType::RightSidebarSnippetSave,
            true,
        )?;

        let field_label_height = ui_metrics.cell_size.height as usize;
        let title_label_y = header_y + SNIPPET_EDITOR_HEADER_HEIGHT;
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
        let title_y = title_label_y + field_label_height + 8;
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
            SNIPPET_FIELD_HEIGHT,
            None,
            "Describe this action",
            &title_input,
            self.right_sidebar_snippet_focus == Some(RightSidebarSnippetField::Title),
            UIItemType::RightSidebarSnippetTitle,
            false,
        )?;

        let body_label_y = title_y + SNIPPET_FIELD_HEIGHT + RIGHT_SIDEBAR_SECTION_GAP + 4;
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
        let body_y = body_label_y + field_label_height + 8;
        let body_height = SNIPPET_BODY_FIELD_HEIGHT.min(content_bottom.saturating_sub(body_y));
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
        let card_bottom = y.saturating_add(SNIPPET_CARD_HEIGHT);
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
            SIDEBAR_ROW_RADIUS + 10.0,
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

        let card_pad = SIDEBAR_INSET * 2;
        let text_x = x + card_pad;
        let run_button_width = (self.sidebar_text_width(ui_font, "Run")?.ceil() as usize
            + SIDEBAR_INSET * 4)
            .max(SNIPPET_ACTION_BUTTON_MIN_WIDTH);
        let paste_button_width = (self.sidebar_text_width(ui_font, "Paste")?.ceil() as usize
            + SIDEBAR_INSET * 4)
            .max(SNIPPET_ACTION_BUTTON_MIN_WIDTH);
        let delete_button_size = SNIPPET_ACTION_BUTTON_HEIGHT;
        let action_area_width = if hovered {
            run_button_width + paste_button_width + delete_button_size + SIDEBAR_INSET * 2
        } else {
            0
        };
        let title_y = y + SIDEBAR_INSET * 2;
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
        let preview_y = y + SIDEBAR_INSET * 2 + ui_metrics.cell_size.height as usize + 8;
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
            let paste_x = delete_x.saturating_sub(SIDEBAR_INSET + paste_button_width);
            let run_x = paste_x.saturating_sub(SIDEBAR_INSET + run_button_width);
            let action_y = y + SIDEBAR_INSET * 2 - 4;
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
                SNIPPET_ACTION_BUTTON_HEIGHT,
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
                SNIPPET_ACTION_BUTTON_HEIGHT,
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
                SIDEBAR_ROW_RADIUS + 4.0
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

        let text_pad = SIDEBAR_INSET + 2;
        let mut text_x = x + text_pad;
        if let Some(icon) = icon {
            let icon_size = (ui_metrics.cell_size.height as usize + 2).clamp(18, 22);
            self.paint_sidebar_icon(
                layers,
                icon,
                text_x,
                y + (height.saturating_sub(icon_size)) / 2,
                icon_size,
                muted_fg,
            )?;
            text_x += icon_size + SIDEBAR_ICON_GAP;
        }

        let text_color = if input.text.is_empty() && !focused {
            muted_fg.mul_alpha(0.72)
        } else {
            chrome.text
        };
        let text = if input.text.is_empty() && !focused {
            placeholder
        } else {
            input.text.as_str()
        };
        if multiline {
            let line_height = ui_metrics.cell_size.height as usize + 4;
            let max_lines = height.saturating_sub(SIDEBAR_INSET * 2).max(1) / line_height.max(1);
            let mut line_y = y + SIDEBAR_INSET + 2;
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
                        SNIPPET_CARET_WIDTH,
                        (ui_metrics.cell_size.height as f32).max(1.0),
                    ),
                    chrome.text,
                )
                .context("right sidebar snippet body caret")?;
            }
        } else {
            let text_area_width = width.saturating_sub((text_x - x) + text_pad);
            let baseline_y = y + (height.saturating_sub(ui_metrics.cell_size.height as usize)) / 2;

            if input.text.is_empty() && !focused {
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
                let chars: Vec<char> = input.text.chars().collect();
                let cursor = input.cursor.min(chars.len());
                let avail = text_area_width as f32;
                let caret_margin = SNIPPET_CARET_WIDTH + 2.0;

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
                                    ui_metrics.cell_size.height as f32 + 4.0,
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
                            SNIPPET_CARET_WIDTH,
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
            .map(|_| (ui_metrics.cell_size.height as usize + 2).clamp(18, 22))
            .unwrap_or(0);
        let horizontal_pad = SIDEBAR_INSET * 2;
        let available_label_width = width.saturating_sub(horizontal_pad * 2 + icon_size);
        let text_width =
            (self.sidebar_text_width(ui_font, label)?.ceil() as usize).min(available_label_width);
        let icon_label_gap = if icon.is_some() && text_width > 0 {
            SIDEBAR_ICON_GAP
        } else {
            0
        };
        let total_width = icon_size + icon_label_gap + text_width;
        let start_x =
            x + horizontal_pad + width.saturating_sub(horizontal_pad * 2 + total_width) / 2;
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
            width.saturating_sub(text_x.saturating_sub(x) + SIDEBAR_INSET),
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
        let icon_size = (size * 58 / 100).max(16);
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

lazy_static::lazy_static! {
    static ref FILE_PREVIEW_SYNTAX_SET: SyntaxSet = SyntaxSet::load_defaults_newlines();
    static ref FILE_PREVIEW_THEME_SET: ThemeSet = ThemeSet::load_defaults();
}

fn preview_lines_from_text(
    path: &Path,
    text: &str,
    use_dark_theme: bool,
) -> Vec<RightSidebarFilePreviewLine> {
    let Some(syntax) = FILE_PREVIEW_SYNTAX_SET
        .find_syntax_for_file(path)
        .ok()
        .flatten()
    else {
        return preview_plain_lines_from_text(text);
    };
    let Some(theme) = preview_syntax_theme(use_dark_theme) else {
        return preview_plain_lines_from_text(text);
    };

    let default_color = theme
        .settings
        .foreground
        .map(syntect_color_to_linear)
        .unwrap_or_else(|| LinearRgba::with_components(1.0, 1.0, 1.0, 1.0));

    let mut highlighter = HighlightLines::new(syntax, theme);
    let raw_lines: Vec<&str> = if text.is_empty() {
        vec![""]
    } else {
        text.lines().collect()
    };
    let mut lines = Vec::with_capacity(raw_lines.len());
    for line in raw_lines {
        // Highlight only the head of very long lines to bound syntect cost, but
        // keep the tail verbatim (rendered in the default colour) so no content
        // is lost.
        let (head, tail) = match line
            .char_indices()
            .nth(FILE_PREVIEW_HIGHLIGHT_CHAR_LIMIT)
            .map(|(idx, _)| idx)
        {
            Some(split) => (&line[..split], &line[split..]),
            None => (line, ""),
        };
        let ranges = match highlighter.highlight_line(head, &FILE_PREVIEW_SYNTAX_SET) {
            Ok(ranges) => ranges,
            Err(err) => {
                log::warn!("failed to highlight file preview line: {err:#}");
                return preview_plain_lines_from_text(text);
            }
        };
        lines.push(preview_line_from_highlighted_ranges(
            ranges,
            tail,
            default_color,
        ));
    }
    lines
}

fn preview_syntax_theme(use_dark_theme: bool) -> Option<&'static Theme> {
    let dark_theme_names = [
        "base16-eighties.dark",
        "Solarized (dark)",
        "base16-ocean.dark",
    ];
    let light_theme_names = ["base16-ocean.light", "Solarized (light)", "InspiredGitHub"];
    let names = if use_dark_theme {
        &dark_theme_names[..]
    } else {
        &light_theme_names[..]
    };
    names
        .iter()
        .find_map(|name| FILE_PREVIEW_THEME_SET.themes.get(*name))
        .or_else(|| FILE_PREVIEW_THEME_SET.themes.values().next())
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

fn preview_line_from_highlighted_ranges(
    ranges: Vec<(SyntectStyle, &str)>,
    tail: &str,
    default_color: LinearRgba,
) -> RightSidebarFilePreviewLine {
    let mut plain = String::new();
    let mut char_count = 0usize;
    let mut spans = Vec::new();

    for (style, text) in ranges {
        if text.is_empty() {
            continue;
        }
        let color = syntect_color_to_linear(style.foreground);
        let span_char_count = text.chars().count();
        plain.push_str(text);
        char_count = char_count.saturating_add(span_char_count);
        spans.push(RightSidebarFilePreviewSpan {
            text: text.to_string(),
            char_count: span_char_count,
            color,
        });
    }

    // The un-highlighted remainder of a very long line is kept verbatim so no
    // content is dropped; it just renders in the editor's default colour.
    if !tail.is_empty() {
        let span_char_count = tail.chars().count();
        plain.push_str(tail);
        char_count = char_count.saturating_add(span_char_count);
        spans.push(RightSidebarFilePreviewSpan {
            text: tail.to_string(),
            char_count: span_char_count,
            color: default_color,
        });
    }

    RightSidebarFilePreviewLine {
        plain,
        char_count,
        spans,
    }
}

fn syntect_color_to_linear(color: SyntectColor) -> LinearRgba {
    LinearRgba::with_srgba(color.r, color.g, color.b, color.a)
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
) -> usize {
    number_digits
        .saturating_mul(cell_width.max(1))
        .saturating_add(SIDEBAR_ICON_GAP * 2)
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
    let cell_height = ui_metrics.cell_size.height as usize;
    let row_height = cell_height.saturating_add(18).clamp(38, 58);
    let icon_size = cell_height
        .saturating_add(8)
        .clamp(22, row_height.saturating_sub(8));
    let chevron_size = (icon_size * 72 / 100).clamp(14, 24);
    let indent_step = (icon_size * 58 / 100).clamp(14, 22);
    let icon_gap = (icon_size / 3).clamp(8, 14);
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

fn load_right_sidebar_file_preview(
    path: &Path,
    use_dark_syntax_theme: bool,
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
        preview_lines_from_text(path, &text, use_dark_syntax_theme)
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
            )
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
        build_right_sidebar_file_index, full_line_colors_by_byte, image_pixels_within_preview_budget,
        load_file_preview, load_file_preview_image, naturalish_cmp, path_key, preview_line_count,
        preview_lines_from_text, preview_plain_lines_from_text,
        preview_text_range, preview_visible_colored, preview_visible_line_range,
        right_sidebar_file_browse_rows_from_index, right_sidebar_file_row_metrics,
        open_with_candidate_allowed, right_sidebar_open_with_cache_key,
        search_right_sidebar_file_index, snippet_cursor_visible,
        snippet_run_buffer, sorted_open_with_candidates, visible_file_row_range,
        wrap_snippet_text_for_width, FILE_PREVIEW_HIGHLIGHT_CHAR_LIMIT, FILE_PREVIEW_MAX_BYTES,
    };
    use crate::termwindow::{RightSidebarFilePreviewLine, RightSidebarFilePreviewSpan};
    use crate::utilsprites::RenderMetrics;
    use std::collections::HashSet;
    use std::fs;
    use std::io::Cursor;
    use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
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

        assert_eq!(small.row_height, 38);
        assert_eq!(small.icon_size, 26);
        assert!(normal.row_height > small.row_height);
        assert!(normal.icon_size > small.icon_size);
        assert!(normal.indent_step >= small.indent_step);
        assert!(normal.icon_gap >= small.icon_gap);
        assert_eq!(large.row_height, 58);
        assert_eq!(large.icon_size, 50);
        assert!(large.chevron_size <= 24);
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
            cand("/Applications/Visual Studio Code.app", "Visual Studio Code", false),
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
        let line = "a".repeat(FILE_PREVIEW_HIGHLIGHT_CHAR_LIMIT * 2);

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
