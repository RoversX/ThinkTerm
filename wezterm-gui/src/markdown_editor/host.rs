use super::{
    build_visual_document, DocumentSnapshot, EditorViewState, MarkdownDocumentSession,
    MarkdownProjection, NoteSpellingIssue, ProjectedObject, SaveState, SelectionGranularity,
    SourcePosition, SourceSelection, VaultDocument, VisualDocument, VisualWrapCache,
};
use parking_lot::Mutex;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::ops::Range;
use std::sync::Arc;
use std::time::SystemTime;
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
enum NoteDisplaySource {
    Published,
    Live,
    Frozen(DocumentSnapshot),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AutosaveWakeAction {
    Idle,
    Reschedule(Duration),
    SaveNow,
}

#[derive(Debug, Clone)]
pub(crate) struct NoteRunLayout {
    pub source: Range<usize>,
    pub x: f32,
    pub width: f32,
    pub hit_x: f32,
    pub hit_width: f32,
    /// Visual x offsets paired with the source byte position reached at that
    /// boundary. Contains both the leading and trailing edge.
    pub boundaries: Vec<(f32, usize)>,
    pub atomic: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct NoteLineLayout {
    pub source: Range<usize>,
    pub y: f32,
    pub height: f32,
    pub runs: Vec<NoteRunLayout>,
}

#[derive(Debug, Clone)]
pub(crate) struct NoteCodeBlockLayout {
    pub source_start: usize,
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    pub max_horizontal_scroll: f32,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct NoteLineGeometry {
    pub top: f32,
    pub height: f32,
    pub gap: f32,
}

type NoteWrapLayoutKey = (u64, super::EditorMode, usize, usize);

/// Identifies one exact background wrap request. `layout` describes the
/// document revision/caret/width while `visual_identity` prevents revisions
/// from different documents from colliding. `generation` makes an A -> B -> A
/// resize reject the first A even though its layout key is equal to the latest
/// request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct NoteBackgroundWrapKey {
    layout: NoteWrapLayoutKey,
    visual_identity: usize,
    generation: u64,
}

pub(crate) struct NoteHostState {
    pub session: Option<Arc<Mutex<MarkdownDocumentSession>>>,
    pub document: Option<VaultDocument>,
    pub view: EditorViewState,
    pub projection: MarkdownProjection,
    pub visual: Arc<VisualDocument>,
    wrapped_visual: Arc<VisualDocument>,
    pub wrap_cache: VisualWrapCache,
    pub projection_revision: Option<u64>,
    pub visual_key: Option<(u64, super::EditorMode, usize)>,
    wrapped_key: Option<(u64, super::EditorMode, usize, usize)>,
    pub line_layouts: Vec<NoteLineLayout>,
    pub line_geometry_key: Option<u64>,
    pub line_geometry: Arc<Vec<NoteLineGeometry>>,
    pub viewport_height: f32,
    pub content_height: f32,
    pub load_error: Option<String>,
    display_source: NoteDisplaySource,
    autosave_deadline: Option<Instant>,
    autosave_timer_scheduled: bool,
    pub reveal_caret: bool,
    pub drag_selection_active: bool,
    pub selection_granularity: SelectionGranularity,
    pub drag_selection_base: Option<Range<usize>>,
    pub drag_autoscroll_scheduled: bool,
    /// Ordered so paint can hash them each frame without sorting a copy.
    pub collapsed_code_blocks: BTreeSet<usize>,
    pub code_horizontal_offsets: BTreeMap<usize, f32>,
    pub code_block_layouts: Vec<NoteCodeBlockLayout>,
    pub spelling_issues: Arc<Vec<NoteSpellingIssue>>,
    pub spelling_revision: Option<u64>,
    pub spelling_context: Option<Range<usize>>,
    pub spellcheck_scheduled_revision: Option<u64>,
    pub spellcheck_in_flight_revision: Option<u64>,
    /// (revision, selection start, selection end, wrap key). Scroll position
    /// is deliberately NOT part of the key: rebuilding the snapshot per
    /// scrolled frame copied up to 64 KB of text and thousands of hit rects.
    pub native_text_input_snapshot_key: Option<(u64, usize, usize, usize)>,
    /// Scroll offset (bits) the current snapshot's hit rects were built at,
    /// so a settle refresh can catch them up after scrolling stops.
    pub native_text_input_snapshot_scroll: Option<u32>,
    /// Last time the Note body scroll offset actually changed.
    pub last_scroll_change: Option<Instant>,
    pub native_text_input_token: u64,
    pub parse_requested_revision: Option<u64>,
    pub parse_in_flight_revision: Option<u64>,
    /// The current projection reused cached wiki-link data but at least one
    /// link had no cached resolution; a background parse round trip owes us
    /// the authoritative vault lookup.
    links_resolution_pending: bool,
    /// Memoized `caret_reveal_start` for the current projection, keyed by
    /// (revision, caret). `active_syntax` is a linear scan over every syntax
    /// node and refresh runs once per paint; a pure scroll frame must not pay
    /// for it.
    reveal_key_cache: Option<(u64, usize, usize)>,
    background_wrap_preferred: bool,
    /// `estimated_wrap_work_bytes()` of the current visual document, stored
    /// at rebuild so per-frame decisions never re-walk every line. This
    /// estimate deliberately excludes code/table/image/rule component lines,
    /// so it must never be the only input to the sync-vs-background wrap
    /// decision: the cached wrap still hashes and shifts EVERY visual line.
    wrap_work_estimate: usize,
    /// Length of the full displayed snapshot's source (for progressive
    /// previews this is the complete document, not the seeded slice).
    displayed_source_bytes: usize,
    /// Line count of the current visual document — what the cached wrap's
    /// reuse pass actually iterates.
    visual_line_count: usize,
    background_wrap_requested_key: Option<NoteBackgroundWrapKey>,
    background_wrap_generation: u64,
    pub background_wrap_in_flight_key: Option<NoteBackgroundWrapKey>,
}

const BACKGROUND_PARSE_THRESHOLD_BYTES: usize = 64 * 1024;
/// Seed enough parsed content for the first screen while a large document is
/// parsed in full on a worker.  Keep this below the background-wrap threshold
/// so opening a multi-megabyte note can paint useful text immediately without
/// performing an exact large-note wrap on the UI thread.
const PROGRESSIVE_PREVIEW_BYTES: usize = 6 * 1024;
/// Exact font measurement and wrapping is expensive well before parsing is.
/// Keep small notes immediate, but move ordinary multi-page notes (including
/// the 40 KiB regression fixture) off the UI thread.
const BACKGROUND_WRAP_THRESHOLD_BYTES: usize = 8 * 1024;
/// Upper bounds for the primed-cache synchronous wrap: beyond any of these
/// the O(all lines) reuse pass itself is too expensive for a keystroke.
const PRIMED_SYNC_WRAP_MAX_SOURCE_BYTES: usize = 256 * 1024;
const PRIMED_SYNC_WRAP_MAX_LINES: usize = 4096;
const PRIMED_SYNC_WRAP_MAX_WORK_BYTES: usize = 256 * 1024;

impl Default for NoteHostState {
    fn default() -> Self {
        Self {
            session: None,
            document: None,
            view: EditorViewState::default(),
            projection: MarkdownProjection::default(),
            visual: Arc::new(VisualDocument::default()),
            wrapped_visual: Arc::new(VisualDocument::default()),
            wrap_cache: VisualWrapCache::default(),
            projection_revision: None,
            visual_key: None,
            wrapped_key: None,
            line_layouts: vec![],
            line_geometry_key: None,
            line_geometry: Arc::new(vec![]),
            viewport_height: 0.0,
            content_height: 0.0,
            load_error: None,
            display_source: NoteDisplaySource::Published,
            autosave_deadline: None,
            autosave_timer_scheduled: false,
            reveal_caret: true,
            drag_selection_active: false,
            selection_granularity: SelectionGranularity::Character,
            drag_selection_base: None,
            drag_autoscroll_scheduled: false,
            collapsed_code_blocks: BTreeSet::new(),
            code_horizontal_offsets: BTreeMap::new(),
            code_block_layouts: vec![],
            spelling_issues: Arc::new(vec![]),
            spelling_revision: None,
            spelling_context: None,
            spellcheck_scheduled_revision: None,
            spellcheck_in_flight_revision: None,
            native_text_input_snapshot_key: None,
            native_text_input_snapshot_scroll: None,
            last_scroll_change: None,
            native_text_input_token: 0,
            parse_requested_revision: None,
            parse_in_flight_revision: None,
            links_resolution_pending: false,
            reveal_key_cache: None,
            background_wrap_preferred: false,
            wrap_work_estimate: 0,
            displayed_source_bytes: 0,
            visual_line_count: 0,
            background_wrap_requested_key: None,
            background_wrap_generation: 0,
            background_wrap_in_flight_key: None,
        }
    }
}

impl NoteHostState {
    /// Switch this host to a Vault document. The document registry guarantees
    /// that another window opening the same path supplies the same Arc.
    pub(crate) fn bind_document(&mut self, document: VaultDocument) -> bool {
        if self
            .session
            .as_ref()
            .is_some_and(|session| Arc::ptr_eq(session, &document.session))
        {
            self.document = Some(document);
            self.load_error = None;
            return false;
        }

        self.session = Some(Arc::clone(&document.session));
        self.document = Some(document);
        self.view = EditorViewState::default();
        self.projection = MarkdownProjection::default();
        self.visual = Arc::new(VisualDocument::default());
        self.wrapped_visual = Arc::new(VisualDocument::default());
        self.wrap_cache = VisualWrapCache::default();
        self.projection_revision = None;
        self.visual_key = None;
        self.wrapped_key = None;
        self.line_layouts.clear();
        self.line_geometry_key = None;
        self.line_geometry = Arc::new(vec![]);
        self.content_height = 0.0;
        self.load_error = None;
        self.display_source = NoteDisplaySource::Published;
        self.autosave_deadline = None;
        self.autosave_timer_scheduled = false;
        self.collapsed_code_blocks.clear();
        self.code_horizontal_offsets.clear();
        self.code_block_layouts.clear();
        self.spelling_issues = Arc::new(vec![]);
        self.spelling_revision = None;
        self.spelling_context = None;
        self.spellcheck_scheduled_revision = None;
        self.spellcheck_in_flight_revision = None;
        self.native_text_input_snapshot_key = None;
        self.parse_requested_revision = None;
        self.parse_in_flight_revision = None;
        self.links_resolution_pending = false;
        self.reveal_key_cache = None;
        self.reset_wrap_work_metrics();
        self.background_wrap_requested_key = None;
        let snapshot = self
            .session
            .as_ref()
            .map(|session| session.lock().published_snapshot());
        if snapshot
            .as_ref()
            .is_some_and(|snapshot| snapshot.source.len() <= BACKGROUND_PARSE_THRESHOLD_BYTES)
        {
            self.refresh_projection();
        } else if let Some(snapshot) = snapshot {
            self.seed_progressive_preview(&snapshot);
        }
        true
    }

    fn seed_progressive_preview(&mut self, snapshot: &DocumentSnapshot) {
        let end = progressive_preview_end(&snapshot.source, PROGRESSIVE_PREVIEW_BYTES);
        let source = &snapshot.source[..end];
        self.projection = MarkdownProjection::parse(source);
        // Deliberately uncached: this is a partial seed projection that the
        // full background parse replaces.
        self.reveal_key_cache = None;
        let active_start = self.projection.caret_reveal_start(0);
        self.visual = Arc::new(build_visual_document(
            source,
            &self.projection,
            self.view.mode,
            0,
        ));
        // Record the FULL document size (the gate metrics must not lie about
        // what the display represents), but keep the seed itself on the
        // synchronous wrap path: this visual is deliberately tiny so a
        // multi-megabyte note paints useful first-screen text immediately.
        // The full background parse installs honest metrics right after.
        self.refresh_wrap_work_metrics(snapshot.source.len());
        self.background_wrap_preferred = self.wrap_work_estimate >= BACKGROUND_WRAP_THRESHOLD_BYTES;
        self.visual_key = Some((snapshot.revision, self.view.mode, active_start));
        // This is deliberately not the complete revision.  The regular
        // background parse path will replace it with the full projection.
        self.projection_revision = None;
        self.parse_requested_revision = Some(snapshot.revision);
    }

    pub(crate) fn clear_document(&mut self, error: Option<String>) {
        self.session = None;
        self.document = None;
        self.load_error = error;
        self.projection_revision = None;
        self.visual_key = None;
        self.wrapped_key = None;
        self.line_layouts.clear();
        self.line_geometry_key = None;
        self.line_geometry = Arc::new(vec![]);
        self.parse_requested_revision = None;
        self.parse_in_flight_revision = None;
        self.links_resolution_pending = false;
        self.reveal_key_cache = None;
        self.reset_wrap_work_metrics();
        self.background_wrap_requested_key = None;
    }

    pub(crate) fn apply_external_snapshot(
        &mut self,
        modified: SystemTime,
        len: u64,
        source: String,
    ) -> bool {
        let Some(session) = self.session.as_ref().cloned() else {
            return false;
        };
        if !session
            .lock()
            .apply_external_snapshot(&mut self.view, modified, len, source)
        {
            return false;
        }
        self.display_source = NoteDisplaySource::Published;
        self.autosave_deadline = None;
        self.autosave_timer_scheduled = false;
        self.projection_revision = None;
        self.visual_key = None;
        self.wrapped_key = None;
        self.wrap_cache = VisualWrapCache::default();
        self.line_geometry_key = None;
        self.line_geometry = Arc::new(vec![]);
        self.spelling_revision = None;
        self.spelling_context = None;
        self.spelling_issues = Arc::new(vec![]);
        self.refresh_projection();
        true
    }

    /// The exact snapshot this host currently displays. Background work
    /// (link resolution, large-note parsing) must operate on this snapshot —
    /// not the live session — or a Published host sharing a dirty session
    /// would parse text it is not showing.
    fn display_snapshot(&self) -> Option<DocumentSnapshot> {
        let session = self.session.as_ref()?;
        Some(match &self.display_source {
            NoteDisplaySource::Published => session.lock().published_snapshot(),
            NoteDisplaySource::Live => session.lock().current_snapshot(),
            NoteDisplaySource::Frozen(snapshot) => snapshot.clone(),
        })
    }

    pub(crate) fn refresh_projection(&mut self) {
        let Some(session) = self.session.as_ref().cloned() else {
            return;
        };
        match self.display_source.clone() {
            NoteDisplaySource::Published => {
                let snapshot = session.lock().published_snapshot();
                self.refresh_projection_from_source(snapshot.revision, &snapshot.source);
            }
            NoteDisplaySource::Live => {
                let session = session.lock();
                self.refresh_projection_from_source(session.revision(), session.source());
            }
            NoteDisplaySource::Frozen(snapshot) => {
                self.refresh_projection_from_source(snapshot.revision, &snapshot.source);
            }
        }
    }

    fn refresh_projection_from_source(&mut self, revision: u64, source: &str) {
        let mut projection_stage = None;
        if self.projection_revision != Some(revision) {
            if source.len() > BACKGROUND_PARSE_THRESHOLD_BYTES {
                self.parse_requested_revision = Some(revision);
                return;
            }
            projection_stage = Some(crate::input_diagnostics::StageTimer::begin(
                "note_projection",
            ));
            let cached_links = self
                .projection
                .objects
                .iter()
                .filter_map(|object| match object {
                    ProjectedObject::WikiLink {
                        target,
                        embed,
                        resolved_path,
                        ambiguous_paths,
                        rendered_lines,
                        ..
                    } => Some((
                        (target.clone(), *embed),
                        (
                            resolved_path.clone(),
                            ambiguous_paths.clone(),
                            rendered_lines.clone(),
                        ),
                    )),
                    _ => None,
                })
                .collect::<HashMap<_, _>>();
            let mut projection = MarkdownProjection::parse(source);
            // Never walk the vault (or read embedded notes) on the UI thread:
            // reuse the previous projection's resolutions for immediate paint
            // and let the background parse worker do the authoritative lookup.
            let mut links_missing_resolution = false;
            let mut any_links = false;
            for object in &mut projection.objects {
                let ProjectedObject::WikiLink {
                    target,
                    embed,
                    resolved_path,
                    ambiguous_paths,
                    rendered_lines,
                    ..
                } = object
                else {
                    continue;
                };
                any_links = true;
                if let Some((cached_path, cached_ambiguous_paths, cached_lines)) =
                    cached_links.get(&(target.clone(), *embed))
                {
                    *resolved_path = cached_path.clone();
                    *ambiguous_paths = cached_ambiguous_paths.clone();
                    *rendered_lines = cached_lines.clone();
                } else {
                    links_missing_resolution = true;
                }
            }
            // Published projections used to re-resolve on every reparse, which
            // is what picks up created/renamed targets and edited embeds; keep
            // that freshness, just on the worker instead of the UI thread.
            // Live/Frozen keep the old cache-authoritative behavior and only
            // owe a round trip for links the cache has never seen.
            self.links_resolution_pending = links_missing_resolution
                || (any_links && matches!(self.display_source, NoteDisplaySource::Published));
            self.projection = projection;
            self.reveal_key_cache = None;
            let code_starts = self
                .projection
                .objects
                .iter()
                .filter_map(|object| match object {
                    ProjectedObject::CodeBlock(code) => Some(code.source.start),
                    _ => None,
                })
                .collect::<HashSet<_>>();
            self.collapsed_code_blocks
                .retain(|source_start| code_starts.contains(source_start));
            self.code_horizontal_offsets
                .retain(|source_start, _| code_starts.contains(source_start));
            self.projection_revision = Some(revision);
            if self.spelling_revision != Some(revision) {
                self.spelling_issues = Arc::new(vec![]);
                self.spelling_context = None;
            }
            self.visual_key = None;
        }
        let caret = self.view.selection.focus.byte;
        let active_start = match self.reveal_key_cache {
            Some((cached_revision, cached_caret, result))
                if cached_revision == revision && cached_caret == caret =>
            {
                result
            }
            _ => {
                let result = self.projection.caret_reveal_start(caret);
                self.reveal_key_cache = Some((revision, caret, result));
                result
            }
        };
        let key = (revision, self.view.mode, active_start);
        if self.visual_key != Some(key) {
            if projection_stage.is_none() {
                projection_stage = Some(crate::input_diagnostics::StageTimer::begin(
                    "note_projection",
                ));
            }
            self.visual = Arc::new(build_visual_document(
                source,
                &self.projection,
                self.view.mode,
                self.view.selection.focus.byte,
            ));
            self.refresh_wrap_work_metrics(source.len());
            self.visual_key = Some(key);
            self.wrapped_key = None;
        }
        if let Some(stage) = projection_stage {
            stage.finish(true);
        }
    }

    pub(crate) fn background_parse_request(
        &mut self,
    ) -> Option<(
        u64,
        Arc<str>,
        super::EditorMode,
        usize,
        Option<(std::path::PathBuf, String)>,
    )> {
        let snapshot = self.display_snapshot()?;
        let needs_full_parse = snapshot.source.len() > BACKGROUND_PARSE_THRESHOLD_BYTES
            && self.projection_revision != Some(snapshot.revision);
        if !needs_full_parse && !self.links_resolution_pending {
            return None;
        }
        self.parse_requested_revision = Some(snapshot.revision);
        if self.parse_in_flight_revision.is_some() {
            return None;
        }
        self.parse_in_flight_revision = Some(snapshot.revision);
        let document = self
            .document
            .as_ref()
            .map(|document| (document.vault_root.clone(), document.relative_path.clone()));
        Some((
            snapshot.revision,
            snapshot.source,
            self.view.mode,
            self.view.selection.focus.byte,
            document,
        ))
    }

    pub(crate) fn apply_background_parse(
        &mut self,
        revision: u64,
        mode: super::EditorMode,
        caret: usize,
        projection: MarkdownProjection,
        visual: VisualDocument,
    ) -> bool {
        if self.parse_in_flight_revision != Some(revision) {
            return false;
        }
        self.parse_in_flight_revision = None;
        let display_revision = self.display_snapshot().map(|snapshot| snapshot.revision);
        if display_revision != Some(revision)
            || self.view.mode != mode
            || self.view.selection.focus.byte != caret
        {
            return false;
        }
        let display_source_len = self
            .display_snapshot()
            .map(|snapshot| snapshot.source.len())
            .unwrap_or(0);
        let active_start = projection.caret_reveal_start(caret);
        self.projection = projection;
        self.reveal_key_cache = Some((revision, caret, active_start));
        // The worker performed the authoritative vault-link resolution.
        self.links_resolution_pending = false;
        self.visual = Arc::new(visual);
        self.refresh_wrap_work_metrics(display_source_len);
        self.projection_revision = Some(revision);
        self.visual_key = Some((revision, mode, active_start));
        self.wrapped_key = None;
        self.spelling_revision = None;
        self.spelling_context = None;
        self.line_geometry_key = None;
        self.line_geometry = Arc::new(vec![]);
        true
    }

    pub(crate) fn background_wrap_request(
        &mut self,
        wrap_key: usize,
    ) -> Option<(NoteBackgroundWrapKey, Arc<VisualDocument>, VisualWrapCache)> {
        let (revision, mode, active_start) = self.visual_key?;
        let layout = (revision, mode, active_start, wrap_key);
        let visual_identity = Arc::as_ptr(&self.visual) as usize;
        let request_changed = self.background_wrap_requested_key.is_none_or(|requested| {
            requested.layout != layout || requested.visual_identity != visual_identity
        });
        if request_changed {
            self.background_wrap_generation = self.background_wrap_generation.wrapping_add(1);
            self.background_wrap_requested_key = Some(NoteBackgroundWrapKey {
                layout,
                visual_identity,
                generation: self.background_wrap_generation,
            });
        }
        let key = self.background_wrap_requested_key?;
        if self.wrapped_key == Some(layout) || self.background_wrap_in_flight_key.is_some() {
            return None;
        }
        if !self.background_wrap_preferred {
            return None;
        }
        self.background_wrap_in_flight_key = Some(key);
        Some((
            key,
            Arc::clone(&self.visual),
            std::mem::take(&mut self.wrap_cache),
        ))
    }

    pub(crate) fn prefers_background_wrap(&self) -> bool {
        self.background_wrap_preferred
    }

    /// Whether the paint path should hand this wrap to the background worker
    /// (accepting one provisional stale frame) instead of wrapping
    /// synchronously.
    ///
    /// A primed cache makes the synchronous wrap re-measure only changed
    /// lines, so typing stays on the immediate path with no per-key flash —
    /// but the reuse pass still hashes and shifts every line (O(total
    /// lines)), so truly huge notes keep the background path even when
    /// primed.
    pub(crate) fn should_background_wrap(&self, wrap_key: usize) -> bool {
        if !self.background_wrap_preferred {
            return false;
        }
        !(self.wrap_cache.is_primed_for(wrap_key)
            && self.displayed_source_bytes <= PRIMED_SYNC_WRAP_MAX_SOURCE_BYTES
            && self.visual_line_count <= PRIMED_SYNC_WRAP_MAX_LINES
            && self.wrap_work_estimate <= PRIMED_SYNC_WRAP_MAX_WORK_BYTES)
    }

    /// Recompute the wrap-decision inputs after installing a new visual
    /// document. `background_wrap_preferred` must trigger on total size and
    /// line count as well: `estimated_wrap_work_bytes` excludes code/table/
    /// image/rule lines, but the cached wrap's reuse pass iterates every
    /// line, so a huge code-only note with a tiny wrap-work estimate must
    /// still stay off the UI thread.
    fn refresh_wrap_work_metrics(&mut self, displayed_source_bytes: usize) {
        self.wrap_work_estimate = self.visual.estimated_wrap_work_bytes();
        self.displayed_source_bytes = displayed_source_bytes;
        self.visual_line_count = self.visual.lines.len();
        self.background_wrap_preferred = self.wrap_work_estimate >= BACKGROUND_WRAP_THRESHOLD_BYTES
            || self.displayed_source_bytes > PRIMED_SYNC_WRAP_MAX_SOURCE_BYTES
            || self.visual_line_count > PRIMED_SYNC_WRAP_MAX_LINES;
    }

    fn reset_wrap_work_metrics(&mut self) {
        self.wrap_work_estimate = 0;
        self.displayed_source_bytes = 0;
        self.visual_line_count = 0;
        self.background_wrap_preferred = false;
    }

    pub(crate) fn apply_background_wrap(
        &mut self,
        key: NoteBackgroundWrapKey,
        wrapped: Arc<VisualDocument>,
        wrap_cache: VisualWrapCache,
    ) -> bool {
        if self.background_wrap_in_flight_key != Some(key) {
            return false;
        }
        self.background_wrap_in_flight_key = None;
        let current_visual_identity = Arc::as_ptr(&self.visual) as usize;
        if self.background_wrap_requested_key != Some(key)
            || self.visual_key != Some((key.layout.0, key.layout.1, key.layout.2))
            || current_visual_identity != key.visual_identity
        {
            return false;
        }
        self.wrap_cache = wrap_cache;
        self.wrapped_visual = wrapped;
        self.wrapped_key = Some(key.layout);
        self.line_geometry_key = None;
        self.line_geometry = Arc::new(vec![]);
        true
    }

    pub(crate) fn background_parse_pending(&self) -> bool {
        if self.links_resolution_pending {
            return self.session.is_some();
        }
        self.display_snapshot()
            .is_some_and(|snapshot| self.projection_revision != Some(snapshot.revision))
    }

    pub(crate) fn begin_live_editing(&mut self) {
        if matches!(self.display_source, NoteDisplaySource::Live) {
            return;
        }
        self.display_source = NoteDisplaySource::Live;
        self.projection_revision = None;
        self.visual_key = None;
        self.wrapped_key = None;
        if let Some(session) = self.session.as_ref().cloned() {
            session.lock().clamp_view(&mut self.view);
        }
        self.refresh_projection();
    }

    pub(crate) fn freeze_live_source(&mut self) {
        if !matches!(self.display_source, NoteDisplaySource::Live) {
            return;
        }
        let Some(session) = self.session.as_ref() else {
            return;
        };
        self.display_source = NoteDisplaySource::Frozen(session.lock().current_snapshot());
    }

    pub(crate) fn apply_published_snapshot(&mut self, saved_revision: u64) -> bool {
        let can_publish = match &self.display_source {
            NoteDisplaySource::Published => true,
            NoteDisplaySource::Frozen(snapshot) => snapshot.revision <= saved_revision,
            // A successful autosave must not kick the focused editor out of
            // Live mode. That transition forced a full rebuild on the next
            // key and was visible as a brief post-save flash.
            NoteDisplaySource::Live => false,
        };
        if !can_publish {
            return false;
        }
        let changed = !matches!(self.display_source, NoteDisplaySource::Published)
            || self.projection_revision != Some(saved_revision);
        self.display_source = NoteDisplaySource::Published;
        if changed {
            self.projection_revision = None;
            self.visual_key = None;
            self.wrapped_key = None;
        }
        changed
    }

    pub(crate) fn display_save_state(&self) -> SaveState {
        let Some(session) = self.session.as_ref() else {
            return SaveState::Saved;
        };
        let session = session.lock();
        match &self.display_source {
            NoteDisplaySource::Published => SaveState::Saved,
            NoteDisplaySource::Live => session.save_state().clone(),
            NoteDisplaySource::Frozen(snapshot)
                if session.published_snapshot().revision >= snapshot.revision =>
            {
                SaveState::Saved
            }
            NoteDisplaySource::Frozen(_) => session.save_state().clone(),
        }
    }

    pub(crate) fn note_edited(&mut self, now: Instant, delay: Duration) -> bool {
        self.autosave_deadline = Some(now + delay);
        if self.autosave_timer_scheduled {
            false
        } else {
            self.autosave_timer_scheduled = true;
            true
        }
    }

    pub(crate) fn autosave_woke(&mut self, now: Instant) -> AutosaveWakeAction {
        match self.autosave_deadline {
            None => {
                self.autosave_timer_scheduled = false;
                AutosaveWakeAction::Idle
            }
            Some(deadline) if deadline > now => {
                AutosaveWakeAction::Reschedule(deadline.duration_since(now))
            }
            Some(_) => {
                self.autosave_deadline = None;
                self.autosave_timer_scheduled = false;
                AutosaveWakeAction::SaveNow
            }
        }
    }

    pub(crate) fn cancel_autosave_deadline(&mut self) {
        self.autosave_deadline = None;
    }

    pub(crate) fn cached_wrapped_visual(&self, wrap_key: usize) -> Option<Arc<VisualDocument>> {
        let Some((revision, mode, active_start)) = self.visual_key else {
            return None;
        };
        (self.wrapped_key == Some((revision, mode, active_start, wrap_key)))
            .then(|| Arc::clone(&self.wrapped_visual))
    }

    pub(crate) fn cache_wrapped_visual(
        &mut self,
        wrap_key: usize,
        visual: Arc<VisualDocument>,
    ) -> Arc<VisualDocument> {
        self.wrapped_visual = visual;
        if let Some((revision, mode, active_start)) = self.visual_key {
            self.wrapped_key = Some((revision, mode, active_start, wrap_key));
        }
        Arc::clone(&self.wrapped_visual)
    }

    pub(crate) fn provisional_wrapped_visual(&self) -> Arc<VisualDocument> {
        Arc::clone(&self.wrapped_visual)
    }

    pub(crate) fn source_position_for_point(&self, x: f32, y: f32) -> SourcePosition {
        let Some(line) = self
            .line_layouts
            .iter()
            .min_by(|a, b| distance_to_line(a, y).total_cmp(&distance_to_line(b, y)))
        else {
            return SourcePosition::new(0);
        };
        if line.runs.is_empty() {
            return SourcePosition::new(line.source.start);
        }
        let run = line
            .runs
            .iter()
            .min_by(|a, b| distance_to_run(a, x).total_cmp(&distance_to_run(b, x)))
            .expect("non-empty runs");
        if run.atomic {
            let midpoint = run.x + run.width / 2.0;
            return SourcePosition {
                byte: if x < midpoint {
                    run.source.start
                } else {
                    run.source.end
                },
                affinity: if x < midpoint {
                    super::Affinity::Before
                } else {
                    super::Affinity::After
                },
            };
        }
        run.boundaries
            .iter()
            .min_by(|(ax, _), (bx, _)| (x - *ax).abs().total_cmp(&(x - *bx).abs()))
            .map(|(_, byte)| SourcePosition::new(*byte))
            .unwrap_or_else(|| SourcePosition::new(run.source.start))
    }

    pub(crate) fn selection_range_for_granularity(
        &self,
        position: SourcePosition,
        granularity: SelectionGranularity,
    ) -> Range<usize> {
        let Some(session) = self.session.as_ref() else {
            return position.byte..position.byte;
        };
        let session = session.lock();
        match granularity {
            SelectionGranularity::Character => position.byte..position.byte,
            SelectionGranularity::Word => session.word_range_at(position.byte),
            SelectionGranularity::MarkdownBlock => {
                let block = self
                    .projection
                    .blocks
                    .iter()
                    .filter(|block| {
                        block.source.start <= position.byte && position.byte <= block.source.end
                    })
                    .min_by_key(|block| {
                        let priority = match block.kind {
                            super::BlockKind::ListItem
                            | super::BlockKind::Heading(_)
                            | super::BlockKind::Quote => 0usize,
                            super::BlockKind::Paragraph => 1,
                            super::BlockKind::CodeBlock | super::BlockKind::Table => 2,
                            super::BlockKind::List => 3,
                            super::BlockKind::Properties
                            | super::BlockKind::Callout
                            | super::BlockKind::Embed
                            | super::BlockKind::Other => 4,
                        };
                        (
                            priority,
                            block.source.end.saturating_sub(block.source.start),
                        )
                    });
                match block.map(|block| block.kind) {
                    Some(super::BlockKind::CodeBlock | super::BlockKind::Table) | None => {
                        session.source_line_selection_range(position.byte)
                    }
                    Some(_) => block
                        .map(|block| block.source.clone())
                        .unwrap_or(position.byte..position.byte),
                }
            }
        }
    }

    pub(crate) fn begin_selection(
        &mut self,
        position: SourcePosition,
        granularity: SelectionGranularity,
        extend: bool,
    ) {
        let range = self.selection_range_for_granularity(position, granularity);
        self.selection_granularity = granularity;
        if extend {
            self.view.selection.focus = SourcePosition::new(match granularity {
                SelectionGranularity::Character => position.byte,
                SelectionGranularity::Word | SelectionGranularity::MarkdownBlock => {
                    if position.byte < self.view.selection.anchor.byte {
                        range.start
                    } else {
                        range.end
                    }
                }
            });
            self.drag_selection_base = Some(self.view.selection.range());
        } else {
            self.view.selection = if granularity == SelectionGranularity::Character {
                SourceSelection::caret(position.byte)
            } else {
                SourceSelection {
                    anchor: SourcePosition::new(range.start),
                    focus: SourcePosition::new(range.end),
                }
            };
            self.drag_selection_base = Some(range);
        }
        self.view.preferred_column = None;
    }

    pub(crate) fn extend_selection_to(&mut self, position: SourcePosition) {
        let base = self
            .drag_selection_base
            .clone()
            .unwrap_or_else(|| self.view.selection.range());
        let target = self.selection_range_for_granularity(position, self.selection_granularity);
        match self.selection_granularity {
            SelectionGranularity::Character => self.view.selection.focus = position,
            SelectionGranularity::Word | SelectionGranularity::MarkdownBlock => {
                if target.end <= base.start {
                    self.view.selection.anchor = SourcePosition::new(base.end);
                    self.view.selection.focus = SourcePosition::new(target.start);
                } else if target.start >= base.end {
                    self.view.selection.anchor = SourcePosition::new(base.start);
                    self.view.selection.focus = SourcePosition::new(target.end);
                } else {
                    self.view.selection.anchor = SourcePosition::new(base.start);
                    self.view.selection.focus = SourcePosition::new(base.end);
                }
            }
        }
        self.view.preferred_column = None;
    }

    /// Move between painted visual rows (including soft wraps) while retaining
    /// the preferred horizontal pixel. The controller falls back to physical
    /// source-line navigation when the target row is outside the current
    /// viewport and therefore intentionally has no shaped layout.
    pub(crate) fn visual_vertical_target(&self, delta: isize) -> Option<(SourcePosition, usize)> {
        let caret = self.view.selection.focus.byte;
        let current = self.line_layouts.iter().position(|line| {
            line.runs
                .iter()
                .any(|run| run.source.start <= caret && caret <= run.source.end)
                || (line.runs.is_empty() && line.source.start == caret)
        })?;
        let target = if delta.is_negative() {
            current.checked_sub(delta.unsigned_abs())?
        } else {
            current.checked_add(delta as usize)?
        };
        let target_line = self.line_layouts.get(target)?;
        let current_line = &self.line_layouts[current];
        let current_x = current_line
            .runs
            .iter()
            .find(|run| run.source.start <= caret && caret <= run.source.end)
            .and_then(|run| {
                run.boundaries
                    .iter()
                    .min_by_key(|(_, source)| source.abs_diff(caret))
                    .map(|(x, _)| *x)
            })
            .or_else(|| current_line.runs.first().map(|run| run.x))
            .unwrap_or(0.0);
        let preferred = self
            .view
            .preferred_column
            .unwrap_or(current_x.max(0.0) as usize);
        let position = self
            .source_position_for_point(preferred as f32, target_line.y + target_line.height / 2.0);
        Some((position, preferred))
    }

    pub(crate) fn atomic_source_for_point(&self, x: f32, y: f32) -> Option<Range<usize>> {
        self.line_layouts
            .iter()
            .find(|line| y >= line.y && y <= line.y + line.height)?
            .runs
            .iter()
            .find(|run| run.atomic && x >= run.x && x <= run.x + run.width)
            .map(|run| run.source.clone())
    }

    pub(crate) fn external_link_target_for_point(&self, x: f32, y: f32) -> Option<String> {
        let run = self
            .line_layouts
            .iter()
            .find(|line| y >= line.y && y <= line.y + line.height)?
            .runs
            .iter()
            .find(|run| x >= run.x && x <= run.x + run.width)?;
        let hit_source = run
            .boundaries
            .windows(2)
            .find_map(|pair| {
                let (left_x, left_byte) = pair[0];
                let (right_x, right_byte) = pair[1];
                let min_x = left_x.min(right_x);
                let max_x = left_x.max(right_x);
                (x >= min_x && x < max_x)
                    .then(|| left_byte.min(right_byte)..left_byte.max(right_byte))
            })
            .unwrap_or_else(|| run.source.clone());
        self.projection
            .text
            .iter()
            .filter_map(|span| {
                let target = span.link_target.as_ref()?;
                (span.source.start < hit_source.end && hit_source.start < span.source.end)
                    .then_some(target)
            })
            .find(|target| {
                url::Url::parse(target)
                    .is_ok_and(|url| matches!(url.scheme(), "http" | "https" | "mailto"))
            })
            .cloned()
    }

    pub(crate) fn table_cell_target(&self, delta: isize) -> Option<usize> {
        let caret = self.view.selection.focus.byte;
        let cells: Vec<_> = self
            .projection
            .objects
            .iter()
            .filter_map(|object| match object {
                ProjectedObject::Table(table) => Some(table),
                _ => None,
            })
            .flat_map(|table| table.rows.iter().flat_map(|row| row.iter()))
            .collect();
        let current = cells
            .iter()
            .position(|cell| cell.source.start <= caret && caret <= cell.source.end)?;
        let target = if delta.is_negative() {
            current.checked_sub(delta.unsigned_abs())?
        } else {
            current.checked_add(delta as usize)?
        };
        cells.get(target).map(|cell| cell.source.start)
    }

    pub(crate) fn clamp_scroll(&mut self) {
        let max = (self.content_height - self.viewport_height).max(0.0);
        self.view.scroll_offset = self.view.scroll_offset.clamp(0.0, max);
    }

    pub(crate) fn scroll_by(&mut self, delta: f32) -> bool {
        let old = self.view.scroll_offset;
        self.view.scroll_offset += delta;
        self.clamp_scroll();
        let changed = (old - self.view.scroll_offset).abs() > f32::EPSILON;
        if changed {
            self.last_scroll_change = Some(Instant::now());
        }
        changed
    }

    pub(crate) fn toggle_code_block(&mut self, source_start: usize) -> bool {
        if !self.collapsed_code_blocks.remove(&source_start) {
            self.collapsed_code_blocks.insert(source_start);
            true
        } else {
            false
        }
    }

    pub(crate) fn scroll_code_block_at(&mut self, x: f32, y: f32, delta: f32) -> bool {
        let Some(layout) = self.code_block_layouts.iter().find(|layout| {
            x >= layout.x
                && x <= layout.x + layout.width
                && y >= layout.y
                && y <= layout.y + layout.height
                && layout.max_horizontal_scroll > 0.0
        }) else {
            return false;
        };
        let offset = self
            .code_horizontal_offsets
            .entry(layout.source_start)
            .or_default();
        let old = *offset;
        *offset = (*offset + delta).clamp(0.0, layout.max_horizontal_scroll);
        (old - *offset).abs() > f32::EPSILON
    }
}

fn progressive_preview_end(source: &str, target: usize) -> usize {
    if source.len() <= target {
        return source.len();
    }
    let mut end = target.min(source.len());
    while end > 0 && !source.is_char_boundary(end) {
        end -= 1;
    }
    if let Some(newline) = source[..end].rfind('\n') {
        if newline >= target / 2 {
            return newline + 1;
        }
    }
    end
}

#[cfg(test)]
mod state_tests {
    use super::*;
    use std::path::PathBuf;

    fn host_with_source(source: &str) -> NoteHostState {
        let mut host = NoteHostState::default();
        host.session = Some(Arc::new(Mutex::new(MarkdownDocumentSession::new(
            "doc".into(),
            PathBuf::from("Inbox.md"),
            source.into(),
        ))));
        host.refresh_projection();
        host
    }

    #[test]
    fn large_document_seeds_first_screen_before_full_background_parse() {
        let paragraph = "First-screen text with **formatting** and enough words to wrap.\n\n";
        let source = paragraph.repeat(BACKGROUND_PARSE_THRESHOLD_BYTES / paragraph.len() + 8);
        let session = Arc::new(Mutex::new(MarkdownDocumentSession::new(
            "large".into(),
            PathBuf::from("Large.md"),
            source,
        )));
        let document = VaultDocument {
            vault_root: PathBuf::from("vault"),
            relative_path: "Large.md".into(),
            document_path: PathBuf::from("vault/Large.md"),
            session,
        };
        let mut host = NoteHostState::default();

        assert!(host.bind_document(document));
        assert!(!host.visual.lines.is_empty());
        assert_eq!(host.projection_revision, None);
        assert!(!host.prefers_background_wrap());
        assert!(host.background_parse_request().is_some());
    }

    #[test]
    fn reveal_cache_tracks_caret_moves_within_one_revision() {
        let source = "A **bold** word";
        let mut host = host_with_source(source);
        let key_outside = host.visual_key;

        host.view.selection = SourceSelection::caret(source.find("bold").unwrap());
        host.refresh_projection();
        assert_ne!(host.visual_key, key_outside, "caret inside strong reveals");

        host.view.selection = SourceSelection::caret(0);
        host.refresh_projection();
        assert_eq!(
            host.visual_key, key_outside,
            "memoized reveal key must not stick to the previous caret"
        );
    }

    #[test]
    fn small_note_wiki_links_resolve_via_background_parse_not_ui_thread() {
        let source = "See [[Other Note]] for details.";
        let mut host = host_with_source(source);
        // The synchronous refresh reused (empty) cached link data, so it owes
        // a background round trip for the authoritative vault lookup.
        assert!(host.background_parse_pending());
        let (revision, request_source, mode, caret, _) =
            host.background_parse_request().expect("resolution request");
        assert_eq!(request_source.as_ref(), source);

        // Single flight while the worker runs.
        assert!(host.background_parse_request().is_none());

        let resolved = MarkdownProjection::parse(source);
        let visual = build_visual_document(source, &resolved, mode, caret);
        assert!(host.apply_background_parse(revision, mode, caret, resolved, visual));
        assert!(!host.background_parse_pending());
        assert!(host.background_parse_request().is_none());
    }

    #[test]
    fn published_host_resolves_links_for_the_displayed_snapshot_not_the_live_one() {
        let mut host = host_with_source("See [[Other]]");
        let session = host.session.as_ref().unwrap().clone();
        let mut other_view = EditorViewState::default();
        let end = session.lock().source().len();
        session.lock().set_caret(&mut other_view, end, false);
        assert!(session
            .lock()
            .insert_text(&mut other_view, " and [[Extra]]"));

        // The Published host must request its displayed (published) snapshot,
        // not the newer dirty session text.
        let (revision, source, mode, caret, _) =
            host.background_parse_request().expect("resolution request");
        assert_eq!(source.as_ref(), "See [[Other]]");
        assert_eq!(revision, 0);

        let resolved = MarkdownProjection::parse(&source);
        let visual = build_visual_document(&source, &resolved, mode, caret);
        assert!(host.apply_background_parse(revision, mode, caret, resolved, visual));
        // Settled: no repeated background parsing while the session stays
        // dirty and the display stays Published.
        assert!(!host.background_parse_pending());
        assert!(host.background_parse_request().is_none());
    }

    #[test]
    fn published_reparse_rechecks_vault_links_even_on_cache_hits() {
        let mut host = host_with_source("See [[Other]]");
        let (revision, source, mode, caret, _) =
            host.background_parse_request().expect("initial resolution");
        let resolved = MarkdownProjection::parse(&source);
        let visual = build_visual_document(&source, &resolved, mode, caret);
        assert!(host.apply_background_parse(revision, mode, caret, resolved, visual));
        assert!(!host.background_parse_pending());

        // An external writer changed the note; the link target may have
        // changed on disk too, so a cache hit must not suppress the
        // background re-resolution the old synchronous path performed.
        let external = "Also see [[Other]].".to_string();
        let len = external.len() as u64;
        assert!(host.apply_external_snapshot(SystemTime::now(), len, external));
        assert!(host.background_parse_pending());
        assert!(host.background_parse_request().is_some());
    }

    #[test]
    fn published_host_ignores_unsaved_shared_revision() {
        let mut host = host_with_source("saved");
        let session = host.session.as_ref().unwrap().clone();
        let mut view = EditorViewState::default();
        session.lock().set_caret(&mut view, 5, false);
        assert!(session.lock().insert_text(&mut view, " draft"));

        host.refresh_projection();
        assert_eq!(host.projection_revision, Some(0));

        let (revision, _, source) = session.lock().snapshot_for_save();
        session.lock().finish_save(revision, source, Ok(()));
        assert!(host.apply_published_snapshot(revision));
        host.refresh_projection();
        assert_eq!(host.projection_revision, Some(revision));
    }

    #[test]
    fn newer_live_revision_is_not_replaced_by_older_publish() {
        let mut host = host_with_source("a");
        host.begin_live_editing();
        let session = host.session.as_ref().unwrap().clone();
        let mut view = EditorViewState::default();
        session.lock().set_caret(&mut view, 1, false);
        assert!(session.lock().insert_text(&mut view, "b"));
        host.refresh_projection();
        assert_eq!(host.projection_revision, Some(1));
        assert!(!host.apply_published_snapshot(0));
        assert_eq!(host.projection_revision, Some(1));
    }

    #[test]
    fn successful_autosave_keeps_the_focused_host_live() {
        let mut host = host_with_source("a");
        host.begin_live_editing();
        let session = host.session.as_ref().unwrap().clone();
        let mut view = EditorViewState::default();
        session.lock().set_caret(&mut view, 1, false);
        assert!(session.lock().insert_text(&mut view, "b"));
        host.refresh_projection();
        let (revision, _, source) = session.lock().snapshot_for_save();
        session.lock().finish_save(revision, source, Ok(()));

        assert!(!host.apply_published_snapshot(revision));
        assert!(matches!(host.display_source, NoteDisplaySource::Live));
        assert_eq!(host.projection_revision, Some(revision));
        assert_eq!(host.display_save_state(), SaveState::Saved);
    }

    #[test]
    fn autosave_uses_one_reschedulable_wakeup() {
        let mut host = NoteHostState::default();
        let start = Instant::now();
        let delay = Duration::from_millis(500);
        assert!(host.note_edited(start, delay));
        assert!(!host.note_edited(start + Duration::from_millis(100), delay));
        assert_eq!(
            host.autosave_woke(start + delay),
            AutosaveWakeAction::Reschedule(Duration::from_millis(100))
        );
        assert_eq!(
            host.autosave_woke(start + Duration::from_millis(600)),
            AutosaveWakeAction::SaveNow
        );
        assert!(host.note_edited(start + Duration::from_millis(700), delay));
        host.cancel_autosave_deadline();
        assert_eq!(
            host.autosave_woke(start + Duration::from_millis(1200)),
            AutosaveWakeAction::Idle
        );
    }

    #[test]
    fn primed_wrap_cache_keeps_typing_synchronous_but_not_for_huge_notes() {
        let wrap_key = 7usize;
        let measure = |_: super::super::BlockKind, _: super::super::InlineStyle, text: &str| {
            Ok::<f32, ()>(text.chars().count() as f32)
        };

        // Ordinary multi-page note: background-preferred, but once the wrap
        // cache is primed for this width, typing wraps synchronously.
        let mut host = host_with_source(&"ordinary prose that wraps ".repeat(1_000));
        assert!(host.prefers_background_wrap());
        assert!(host.should_background_wrap(wrap_key), "first layout");
        super::super::wrap_visual_document_by_width_cached(
            &Arc::clone(&host.visual),
            120.0,
            wrap_key,
            &mut host.wrap_cache,
            measure,
        )
        .unwrap();
        assert!(!host.should_background_wrap(wrap_key), "primed → sync");
        assert!(
            host.should_background_wrap(wrap_key + 1),
            "other width is not primed"
        );

        // Huge note: even a primed cache pays O(total lines) per reuse pass,
        // so it stays on the background path. Sources this large parse on the
        // worker, so install the visual through the background-parse flow.
        let mut huge = host_with_source(&"prose ".repeat(60_000));
        let (revision, source, mode, caret, _) =
            huge.background_parse_request().expect("huge parse request");
        let projection = MarkdownProjection::parse(&source);
        let visual = build_visual_document(&source, &projection, mode, caret);
        assert!(huge.apply_background_parse(revision, mode, caret, projection, visual));
        super::super::wrap_visual_document_by_width_cached(
            &Arc::clone(&huge.visual),
            120.0,
            wrap_key,
            &mut huge.wrap_cache,
            measure,
        )
        .unwrap();
        assert!(huge.should_background_wrap(wrap_key));
    }

    #[test]
    fn component_heavy_notes_stay_on_background_wrap_despite_tiny_work_estimate() {
        let wrap_key = 3usize;
        let measure = |_: super::super::BlockKind, _: super::super::InlineStyle, text: &str| {
            Ok::<f32, ()>(text.chars().count() as f32)
        };

        // ~50 KiB code-only note: estimated_wrap_work_bytes excludes code
        // lines, so it is tiny — but the reuse pass still walks every visual
        // line, so the line count must force the background path.
        let code_source = format!("```text\n{}```", "code line\n".repeat(5_000));
        let mut code_host = host_with_source(&code_source);
        assert!(
            code_host.wrap_work_estimate < BACKGROUND_WRAP_THRESHOLD_BYTES,
            "estimate ignores code lines"
        );
        assert!(code_host.visual_line_count > PRIMED_SYNC_WRAP_MAX_LINES);
        assert!(code_host.prefers_background_wrap());
        super::super::wrap_visual_document_by_width_cached(
            &Arc::clone(&code_host.visual),
            120.0,
            wrap_key,
            &mut code_host.wrap_cache,
            measure,
        )
        .unwrap();
        assert!(
            code_host.should_background_wrap(wrap_key),
            "primed cache must not pull a huge code note onto the UI thread"
        );

        // Table-heavy note: same rule via the table rows.
        let table_source = format!("| A | B |\n| - | - |\n{}", "| aa | bb |\n".repeat(5_000));
        let mut table_host = host_with_source(&table_source);
        assert!(table_host.visual_line_count > PRIMED_SYNC_WRAP_MAX_LINES);
        assert!(table_host.prefers_background_wrap());
        super::super::wrap_visual_document_by_width_cached(
            &Arc::clone(&table_host.visual),
            120.0,
            wrap_key,
            &mut table_host.wrap_cache,
            measure,
        )
        .unwrap();
        assert!(table_host.should_background_wrap(wrap_key));
    }

    #[test]
    fn wrap_metrics_reset_on_clear_and_track_full_source_for_previews() {
        // Small doc: metrics reflect the synchronous build...
        let mut host = host_with_source("plain prose");
        assert!(host.displayed_source_bytes > 0);
        assert!(host.visual_line_count > 0);
        // ...and clear_document zeroes everything.
        host.clear_document(None);
        assert_eq!(host.displayed_source_bytes, 0);
        assert_eq!(host.visual_line_count, 0);
        assert_eq!(host.wrap_work_estimate, 0);
        assert!(!host.prefers_background_wrap());

        // Progressive preview seeds only the first screen but must gate on
        // the FULL document size.
        let paragraph = "First-screen text with **formatting** and enough words to wrap.\n\n";
        let source = paragraph.repeat(BACKGROUND_PARSE_THRESHOLD_BYTES / paragraph.len() + 8);
        let full_len = source.len();
        let session = Arc::new(Mutex::new(MarkdownDocumentSession::new(
            "large".into(),
            PathBuf::from("Large.md"),
            source,
        )));
        let document = VaultDocument {
            vault_root: PathBuf::from("vault"),
            relative_path: "Large.md".into(),
            document_path: PathBuf::from("vault/Large.md"),
            session,
        };
        let mut host = NoteHostState::default();
        assert!(host.bind_document(document));
        assert_eq!(
            host.displayed_source_bytes, full_len,
            "the metric reports the full document, not the seeded slice"
        );
        // The tiny seed itself still paints via the synchronous wrap so the
        // first screen appears immediately (matches the seed test above).
        assert!(!host.prefers_background_wrap());
    }

    #[test]
    fn nontrivial_note_wrap_is_single_flight_and_latest_width_wins() {
        let source = "ordinary proportional-font prose ".repeat(1_300);
        assert!(source.len() < BACKGROUND_PARSE_THRESHOLD_BYTES);
        let mut host = host_with_source(&source);
        assert!(host.prefers_background_wrap());

        let (old_key, _, old_cache) = host.background_wrap_request(7).expect("wrap request");
        assert!(host.background_wrap_request(8).is_none());
        assert!(!host.apply_background_wrap(
            old_key,
            Arc::new(VisualDocument::default()),
            old_cache,
        ));

        let (new_key, _, new_cache) = host.background_wrap_request(8).expect("latest wrap");
        assert_ne!(new_key.generation, old_key.generation);
        assert_eq!(new_key.layout.3, 8);
        assert!(host.apply_background_wrap(
            new_key,
            Arc::new(VisualDocument::default()),
            new_cache,
        ));
    }

    #[test]
    fn repeated_paint_keeps_current_wrap_request_valid() {
        let mut host = host_with_source(&"text that needs wrapping ".repeat(500));
        let (key, _, cache) = host.background_wrap_request(42).expect("wrap request");
        assert!(host.background_wrap_request(42).is_none());
        assert_eq!(host.background_wrap_requested_key, Some(key));
        assert!(host.apply_background_wrap(key, Arc::new(VisualDocument::default()), cache,));
    }

    #[test]
    fn resize_aba_rejects_the_first_matching_width() {
        let mut host = host_with_source(&"text that needs wrapping ".repeat(500));
        let (first_a, _, first_cache) = host.background_wrap_request(10).expect("first A request");
        assert!(host.background_wrap_request(11).is_none());
        assert!(host.background_wrap_request(10).is_none());
        assert_eq!(host.background_wrap_requested_key.unwrap().layout.3, 10);

        assert!(!host.apply_background_wrap(
            first_a,
            Arc::new(VisualDocument::default()),
            first_cache,
        ));
        let (latest_a, _, _) = host
            .background_wrap_request(10)
            .expect("latest A follows single flight");
        assert_ne!(latest_a.generation, first_a.generation);
    }

    #[test]
    fn word_and_markdown_block_drag_keep_semantic_boundaries() {
        let mut host = host_with_source("First paragraph.\n\n- list item\n- second item\n");
        host.begin_live_editing();
        host.begin_selection(SourcePosition::new(2), SelectionGranularity::Word, false);
        host.extend_selection_to(SourcePosition::new(10));
        assert_eq!(
            host.session
                .as_ref()
                .unwrap()
                .lock()
                .selected_text(&host.view),
            Some("First paragraph")
        );

        let list = "First paragraph.\n\n".len();
        host.begin_selection(
            SourcePosition::new(list + 3),
            SelectionGranularity::MarkdownBlock,
            false,
        );
        let selected = host
            .session
            .as_ref()
            .unwrap()
            .lock()
            .selected_text(&host.view)
            .unwrap()
            .to_string();
        assert!(selected.contains("list item"));
        assert!(!selected.contains("second item"));
    }

    #[test]
    #[ignore = "manual Note projection/wrap performance probe"]
    fn note_projection_and_wrap_performance_probe() {
        fn fixture(target_bytes: usize) -> String {
            const PARAGRAPH: &str = "## Heading\n\nA paragraph with **bold**, *italic*, and [link](https://example.com). It contains enough prose to exercise wrapping without making every block a code block.\n\n- [ ] task\n- list item\n\n";
            const CODE: &str = "```rust\nfn value(input: usize) -> usize { input + 1 }\n```\n\n";
            let mut source = String::with_capacity(target_bytes + PARAGRAPH.len() + CODE.len());
            let paragraph_target = (target_bytes / PARAGRAPH.len()).max(1);
            let code_interval = (paragraph_target / 10).max(1);
            let mut paragraph_index = 0usize;
            while source.len() < target_bytes {
                source.push_str(PARAGRAPH);
                if paragraph_index % code_interval == 0 {
                    source.push_str(CODE);
                }
                paragraph_index += 1;
            }
            source
        }

        fn p95(mut durations: Vec<Duration>) -> Duration {
            durations.sort_unstable();
            durations[(durations.len() * 95 / 100).min(durations.len() - 1)]
        }

        for target in [250 * 1024, 1024 * 1024, 5 * 1024 * 1024] {
            let source = fixture(target);
            let mut parse_times = Vec::new();
            let mut visual_times = Vec::new();
            let mut wrap_times = Vec::new();
            for _ in 0..12 {
                let started = Instant::now();
                let projection = MarkdownProjection::parse(&source);
                parse_times.push(started.elapsed());
                let started = Instant::now();
                let visual = build_visual_document(
                    &source,
                    &projection,
                    super::super::EditorMode::LivePreview,
                    source.len(),
                );
                visual_times.push(started.elapsed());

                let started = Instant::now();
                let wrapped =
                    super::super::wrap_visual_document_by_width(&visual, 640.0, |_, _, text| {
                        Ok::<f32, ()>(text.chars().count() as f32 * 8.0)
                    })
                    .unwrap();
                wrap_times.push(started.elapsed());
                assert!(!wrapped.lines.is_empty());
            }
            eprintln!(
                "note_probe bytes={} parse_p95={:?} visual_p95={:?} wrap_p95={:?} total_p95={:?}",
                source.len(),
                p95(parse_times.clone()),
                p95(visual_times.clone()),
                p95(wrap_times.clone()),
                p95(parse_times
                    .into_iter()
                    .zip(visual_times)
                    .zip(wrap_times)
                    .map(|((parse, visual), wrap)| parse + visual + wrap)
                    .collect())
            );

            let projection = MarkdownProjection::parse(&source);
            let visual = build_visual_document(
                &source,
                &projection,
                super::super::EditorMode::LivePreview,
                source.len(),
            );
            let mut wrap_cache = super::super::VisualWrapCache::default();
            super::super::wrap_visual_document_by_width_cached(
                &visual,
                640.0,
                1,
                &mut wrap_cache,
                |_, _, text| Ok::<f32, ()>(text.chars().count() as f32 * 8.0),
            )
            .unwrap();
            let mut edited_source = source;
            let edit_at = edited_source.len() / 2;
            let mut incremental_parse = Vec::new();
            let mut incremental_visual = Vec::new();
            let mut incremental_wrap = Vec::new();
            for _ in 0..12 {
                edited_source.insert(edit_at, 'x');
                let started = Instant::now();
                let projection = MarkdownProjection::parse(&edited_source);
                incremental_parse.push(started.elapsed());
                let started = Instant::now();
                let visual = build_visual_document(
                    &edited_source,
                    &projection,
                    super::super::EditorMode::LivePreview,
                    edit_at + 1,
                );
                incremental_visual.push(started.elapsed());
                let started = Instant::now();
                let wrapped = super::super::wrap_visual_document_by_width_cached(
                    &visual,
                    640.0,
                    1,
                    &mut wrap_cache,
                    |_, _, text| Ok::<f32, ()>(text.chars().count() as f32 * 8.0),
                )
                .unwrap();
                incremental_wrap.push(started.elapsed());
                assert!(!wrapped.lines.is_empty());
            }
            eprintln!(
                "note_probe_incremental bytes={} parse_p95={:?} visual_p95={:?} wrap_p95={:?} total_p95={:?}",
                edited_source.len(),
                p95(incremental_parse.clone()),
                p95(incremental_visual.clone()),
                p95(incremental_wrap.clone()),
                p95(incremental_parse
                    .into_iter()
                    .zip(incremental_visual)
                    .zip(incremental_wrap)
                    .map(|((parse, visual), wrap)| parse + visual + wrap)
                    .collect())
            );
        }
    }
}

fn distance_to_line(line: &NoteLineLayout, y: f32) -> f32 {
    if y < line.y {
        line.y - y
    } else if y > line.y + line.height {
        y - (line.y + line.height)
    } else {
        0.0
    }
}

fn distance_to_run(run: &NoteRunLayout, x: f32) -> f32 {
    if x < run.hit_x {
        run.hit_x - x
    } else if x > run.hit_x + run.hit_width {
        x - (run.hit_x + run.hit_width)
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(source: Range<usize>, glyph_x: f32, cell_x: f32) -> NoteRunLayout {
        NoteRunLayout {
            boundaries: vec![(glyph_x, source.start), (glyph_x + 10.0, source.end)],
            source,
            x: glyph_x,
            width: 10.0,
            hit_x: cell_x,
            hit_width: 100.0,
            atomic: false,
        }
    }

    #[test]
    fn table_cell_whitespace_hits_its_own_source_range() {
        let mut host = NoteHostState::default();
        host.line_layouts.push(NoteLineLayout {
            source: 0..20,
            y: 10.0,
            height: 30.0,
            runs: vec![run(1..2, 10.0, 0.0), run(11..12, 110.0, 100.0)],
        });

        assert_eq!(host.source_position_for_point(90.0, 20.0).byte, 2);
        assert_eq!(host.source_position_for_point(110.0, 20.0).byte, 11);
        assert_eq!(host.source_position_for_point(190.0, 20.0).byte, 12);
    }

    #[test]
    fn point_selection_chooses_the_nearest_visual_row() {
        let mut host = NoteHostState::default();
        host.line_layouts = vec![
            NoteLineLayout {
                source: 0..1,
                y: 10.0,
                height: 20.0,
                runs: vec![run(0..1, 0.0, 0.0)],
            },
            NoteLineLayout {
                source: 2..3,
                y: 40.0,
                height: 20.0,
                runs: vec![run(2..3, 0.0, 0.0)],
            },
        ];

        assert_eq!(host.source_position_for_point(0.0, 5.0).byte, 0);
        assert_eq!(host.source_position_for_point(0.0, 65.0).byte, 2);
    }

    #[test]
    fn external_link_hit_testing_uses_painted_run_bounds() {
        let source = "Read [docs](https://example.test/guide).";
        let projection = MarkdownProjection::parse(source);
        let link_source = projection
            .text
            .iter()
            .find(|span| span.link_target.is_some())
            .unwrap()
            .source
            .clone();
        let mut host = NoteHostState {
            projection,
            ..NoteHostState::default()
        };
        host.line_layouts.push(NoteLineLayout {
            source: 0..source.len(),
            y: 10.0,
            height: 20.0,
            runs: vec![NoteRunLayout {
                source: 0..source.len(),
                x: 30.0,
                width: 40.0,
                hit_x: 30.0,
                hit_width: 40.0,
                boundaries: vec![
                    (30.0, 0),
                    (40.0, link_source.start),
                    (60.0, link_source.end),
                    (70.0, source.len()),
                ],
                atomic: false,
            }],
        });

        assert_eq!(
            host.external_link_target_for_point(50.0, 20.0).as_deref(),
            Some("https://example.test/guide")
        );
        assert_eq!(host.external_link_target_for_point(31.0, 20.0), None);
        assert_eq!(host.external_link_target_for_point(80.0, 20.0), None);
    }

    #[test]
    fn code_block_view_state_is_per_block_and_bounded() {
        let mut host = NoteHostState::default();
        assert!(host.toggle_code_block(10));
        assert!(host.collapsed_code_blocks.contains(&10));
        assert!(!host.toggle_code_block(10));
        assert!(!host.collapsed_code_blocks.contains(&10));

        host.code_block_layouts.push(NoteCodeBlockLayout {
            source_start: 20,
            x: 100.0,
            y: 50.0,
            width: 300.0,
            height: 120.0,
            max_horizontal_scroll: 80.0,
        });
        assert!(!host.scroll_code_block_at(20.0, 60.0, 30.0));
        assert!(host.scroll_code_block_at(200.0, 80.0, 30.0));
        assert_eq!(host.code_horizontal_offsets.get(&20), Some(&30.0));
        assert!(host.scroll_code_block_at(200.0, 80.0, 100.0));
        assert_eq!(host.code_horizontal_offsets.get(&20), Some(&80.0));
        assert!(!host.scroll_code_block_at(200.0, 80.0, 100.0));
    }
}
