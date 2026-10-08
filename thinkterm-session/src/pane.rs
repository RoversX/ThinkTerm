//! One mirrored pane: its state behind one lock, the pushes it applies in
//! order, the rows it fetches and the polls it makes through the host.
//! Detached tasks hold a `Weak` to it; a pane that is gone by the time an
//! answer arrives is simply not updated.
use crate::clock::Clock;
use crate::delta_queue::RenderDeltaQueue;
use crate::host::{request, HostConfig, HostPaneId, PduLink, SessionEvents, SessionHost, Spawner};
use crate::hydrate::hydrate_lines;
use crate::images::ImageStore;
use crate::input::{drain_pane_inputs, push_input, InputQueue, InputQueueFull, PaneInput};
use crate::lines::*;
use crate::pane_state::PaneState;
use crate::{Lock, SessionConfig};
use codec::{GetLines, GetPaneRenderChanges, GetPaneRenderChangesResponse, InputSerial, Pdu};
use rangeset::RangeSet;
use std::ops::Range;
use std::sync::atomic::{AtomicUsize, Ordering};
use thinkterm_proto::TabId;
use std::sync::{Arc, Weak};
use std::time::Duration;
use termwiz::cell::CellAttributes;
use termwiz::color::AnsiColor;
use termwiz::input::{KeyEvent, KeyboardEncoding};
use termwiz::surface::{SequenceNo, SEQ_ZERO};
use thinkterm_proto::ScrollbackEraseMode;
use thinkterm_proto::{PaneId, RenderableDimensions, StableCursorPosition};
use url::Url;
use wezterm_term::{KeyCode, KeyModifiers, Line, MouseEvent, StableRowIndex, TerminalSize};

type SceneImageKey = (crate::host::ImageDomainKey, [u8; 32]);

/// How long a scene picture that could not be fetched waits before it is
/// asked for again, unless its metadata changes first. A busy pane or a
/// dropped link is not the last word, and a static placement's metadata
/// may never change.
const SCENE_RETRY_AFTER: std::time::Duration = std::time::Duration::from_secs(2);

#[derive(Default)]
struct SceneImages {
    pending: std::collections::HashSet<SceneImageKey>,
    failed: Vec<(SceneImageKey, (u64, u64), crate::clock::Timestamp)>,
}

pub struct PaneSession<H: SessionHost> {
    host: Arc<H>,
    images: Arc<Lock<ImageStore>>,
    scene_images: Lock<SceneImages>,
    state: Lock<PaneState>,
    render_deltas: Lock<RenderDeltaQueue>,
    /// Everything this pane was given and the server has not answered,
    /// in the order it was given; see `PaneInput`. Shared with the drain's
    /// guards, which settle it after this pane may be gone.
    inputs: Arc<Lock<InputQueue>>,
    remote_pane_id: PaneId,
    /// The tab the server currently files the pane under; a resync moves
    /// it. Shared with the host so it can update it in place.
    remote_tab_id: Arc<AtomicUsize>,
    host_pane_id: HostPaneId,
}

/// The size a session starts from, as the server listed it. Grids and row
/// caches are built from it before any render delta could be refused, so an
/// impossible one is brought into bounds here: each client drops such a pane
/// when it can, and this is the floor under all of them.
fn plausible_dimensions(dimensions: RenderableDimensions) -> RenderableDimensions {
    if dimensions.is_plausible() {
        return dimensions;
    }
    log::warn!("a pane was listed with impossible dimensions; starting it from bounded ones");
    use thinkterm_proto::layout::{MAX_PANE_CELLS, MAX_PANE_PIXELS};
    let viewport_rows = dimensions.viewport_rows.min(MAX_PANE_CELLS);
    RenderableDimensions {
        cols: dimensions.cols.min(MAX_PANE_CELLS),
        viewport_rows,
        scrollback_rows: viewport_rows,
        physical_top: 0,
        scrollback_top: 0,
        pixel_width: dimensions.pixel_width.min(MAX_PANE_PIXELS),
        pixel_height: dimensions.pixel_height.min(MAX_PANE_PIXELS),
        ..dimensions
    }
}

impl<H: SessionHost> PaneSession<H> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        host: Arc<H>,
        images: Arc<Lock<ImageStore>>,
        config: SessionConfig,
        remote_pane_id: PaneId,
        remote_tab_id: Arc<AtomicUsize>,
        host_pane_id: HostPaneId,
        dimensions: RenderableDimensions,
        title: &str,
        alt_screen: bool,
    ) -> Arc<Self> {
        let dimensions = plausible_dimensions(dimensions);
        let state = PaneState::new(
            config,
            dimensions,
            title,
            alt_screen,
            host.config().fetch_rate_per_second(),
            host.clock().now(),
        );
        Arc::new(Self {
            host,
            images,
            scene_images: Default::default(),
            state: Lock::new(state),
            render_deltas: Lock::new(RenderDeltaQueue::default()),
            inputs: Default::default(),
            remote_pane_id,
            remote_tab_id,
            host_pane_id,
        })
    }

    fn state(&self) -> crate::LockGuard<'_, PaneState> {
        self.state.lock()
    }

    fn now(&self) -> crate::clock::Timestamp {
        self.host.clock().now()
    }

    /// Canonical pictures used by scene placements outside the row cache.
    /// Four outstanding requests per pane bound work; each completion wakes
    /// the painter, which asks for any visible pictures still missing.
    pub fn scene_image(self: &Arc<Self>, image_id: u32, hash: [u8; 32], generation: u64, revision: u64) -> Option<Arc<termwiz::image::ImageData>> {
        let domain = self.host.image_domain();
        let held = {
            let mut images = self.images.lock();
            images.touch(self.now());
            images.get(&(domain, hash))
        };
        if held.as_ref().is_some_and(|data| data.generation() >= generation) || self.is_dead() { return held; }
        let request = codec::GetImageCell { pane_id: self.remote_pane_id, line_idx: 0, cell_idx: 0,
            data_hash: hash, data_generation: generation,
            have_frames: held.as_ref().map_or(0, |data| crate::images::frame_count(&data.data())) };
        if !matches!(self.host.image_request(request.clone(), Some(image_id)), Pdu::GetKittyImage(_)) { return held; }
        {
            let now = self.now();
            let mut pending = self.scene_images.lock();
            pending.failed.retain(|(key, g, at)| key.0 == domain && (*key != (domain, hash) || *g == (generation, revision))
                && now.saturating_duration_since(*at) < SCENE_RETRY_AFTER);
            if pending.failed.iter().any(|(key, g, _)| *key == (domain, hash) && *g == (generation, revision))
                || pending.pending.len() >= 4 || !pending.pending.insert((domain, hash)) { return held; }
        }
        let host = Arc::clone(&self.host);
        let weak = Arc::downgrade(self);
        let pending = PendingSceneImage { pane: weak.clone(), key: (domain, hash) };
        let old = held.clone();
        self.host.spawner().spawn_detached(Box::pin(async move {
            if host.image_domain() != domain || weak.upgrade().is_none_or(|pane| pane.is_dead()) { return; }
            let data = crate::hydrate::fetch_image(&*host, old, request, Some(image_id)).await;
            let pane = weak.upgrade();
            if let Some(pane) = &pane {
                if host.image_domain() == domain && !pane.is_dead() {
                    let wanted = data.as_ref().is_some_and(|data| data.hash() == hash && data.generation() >= generation);
                    // A newer picture than the one asked for is kept as well:
                    // the metadata naming it is on its way.
                    if let Some(data) = data.filter(|data| data.hash() != hash || data.generation() >= generation) {
                        crate::images::file_image(&pane.images, domain, data);
                    }
                    if !wanted {
                        let now = pane.now();
                        let mut state = pane.scene_images.lock();
                        if state.failed.len() == crate::images::MAX_IMAGES { state.failed.remove(0); }
                        state.failed.push(((domain, hash), (generation, revision), now));
                    }
                }
            }
            drop(pending);
            if let Some(pane) = pane { host.events().pane_output(pane.host_pane_id); }
        }));
        held
    }

    pub fn remote_pane_id(&self) -> PaneId {
        self.remote_pane_id
    }

    /// The tab the server files the pane under, as last told.
    pub fn remote_tab_id(&self) -> TabId {
        self.remote_tab_id.load(Ordering::Relaxed)
    }

    pub fn host_pane_id(&self) -> HostPaneId {
        self.host_pane_id
    }

    // ---- what the renderer reads --------------------------------------

    pub fn dimensions(&self) -> RenderableDimensions {
        self.state().dimensions
    }

    /// The dimensions the server last reported, while `dimensions` may
    /// still be a frontend preview of another size.
    pub fn server_dimensions(&self) -> RenderableDimensions {
        self.state().server_dimensions
    }

    pub fn cursor_position(&self) -> StableCursorPosition {
        self.state().cursor_position
    }

    /// Whether a render delta has arrived: until then the pane has no
    /// picture of its own, only placeholders.
    pub fn has_received(&self) -> bool {
        self.state().received
    }

    pub fn current_seqno(&self) -> SequenceNo {
        self.state().seqno
    }

    pub fn title(&self) -> String {
        self.state().title.clone()
    }

    pub fn working_dir(&self) -> Option<Url> {
        self.state().working_dir.clone()
    }

    pub fn is_dead(&self) -> bool {
        self.state().dead
    }

    pub fn set_dead(&self, dead: bool) {
        self.state().dead = dead;
    }

    pub fn is_alt_screen(&self) -> bool {
        self.state().alt_screen
    }

    pub fn is_mouse_grabbed(&self) -> bool {
        self.state().mouse_grabbed
    }

    pub fn keyboard_encoding(&self) -> KeyboardEncoding {
        self.state().keyboard_encoding
    }

    pub fn is_tardy(&self) -> bool {
        let now = self.now();
        self.state().is_tardy(now)
    }

    pub fn since_last_response(&self) -> Duration {
        let now = self.now();
        self.state().since_last_response(now)
    }

    pub fn make_all_stale(&self) {
        self.state().make_all_stale();
    }

    pub fn update_last_send(&self) {
        let now = self.now();
        self.state().update_last_send(now);
    }

    /// A key went out with `serial`: remember it for cursor reconciliation
    /// and predict its echo, in that order.
    pub fn predict_from_key_event(&self, serial: InputSerial, key: KeyCode, mods: KeyModifiers) {
        let mut st = self.state();
        st.input_serial = serial;
        st.predict_from_key_event(key, mods);
    }

    pub fn predict_from_paste(&self, text: &str) {
        self.state().predict_from_paste(text);
    }

    // ---- input ----------------------------------------------------------
    //
    // Everything goes through one queue per pane, in the order it was
    // given; `Ok(true)` means nothing is draining it and the host must
    // spawn `drain_inputs`. Over the limit an input is refused, not
    // dropped: the caller is told, and what reaches the server is either
    // all of an input or none of it.

    /// Queue `input` behind everything given before it.
    pub fn queue_input(&self, input: PaneInput) -> Result<bool, InputQueueFull> {
        let what = input.describe();
        push_input(&self.inputs, input).map_err(|full| {
            log::warn!(
                "refusing {what} for remote pane {}: {full}",
                self.remote_pane_id
            );
            full
        })
    }

    /// A key, sent with `serial` and predicted locally right after it is
    /// queued: the serial first, so the answer to this very key is the
    /// one allowed to move the cursor.
    pub fn key_down(
        &self,
        serial: InputSerial,
        key: KeyCode,
        mods: KeyModifiers,
    ) -> Result<bool, InputQueueFull> {
        let start = self.queue_input(PaneInput::Key {
            event: KeyEvent {
                key,
                modifiers: mods,
            },
            input_serial: serial,
        })?;
        self.predict_from_key_event(serial, key, mods);
        Ok(start)
    }

    /// A paste, predicted locally right after it is queued.
    pub fn paste(&self, text: &str) -> Result<bool, InputQueueFull> {
        let start = self.queue_input(PaneInput::Paste(text.to_owned()))?;
        self.predict_from_paste(text);
        Ok(start)
    }

    /// Bytes the host encoded itself (kitty-protocol keys, IME text).
    pub fn write_bytes(&self, data: &[u8]) -> Result<bool, InputQueueFull> {
        self.queue_input(PaneInput::Bytes(data.to_vec()))
    }

    pub fn mouse_event(&self, event: MouseEvent) -> Result<bool, InputQueueFull> {
        self.queue_input(PaneInput::Mouse(crate::mouse::normalize_wheel(event)))
    }

    pub fn erase_scrollback(&self, mode: ScrollbackEraseMode) -> Result<bool, InputQueueFull> {
        self.queue_input(PaneInput::EraseScrollback(mode))
    }

    /// Send the queued input in order until the queue is empty. The host
    /// spawns this when a queue call returned `Ok(true)`; it is a plain
    /// `async fn` rather than a `Spawner` task so a host whose link hands
    /// out `Send` futures gets a `Send` future here too.
    pub async fn drain_inputs(self: Arc<Self>) {
        drain_pane_inputs(
            self.host.link(),
            self.remote_pane_id,
            Arc::clone(&self.remote_tab_id),
            Arc::clone(&self.inputs),
        )
        .await
    }

    // ---- geometry -------------------------------------------------------

    pub fn apply_local_resize(&self, size: TerminalSize) -> bool {
        self.state().apply_local_resize(size)
    }

    pub fn begin_frontend_preview(
        &self,
        epoch: u64,
        size: TerminalSize,
        policy: FrontendPreviewPolicy,
    ) -> bool {
        self.state().begin_frontend_preview(epoch, size, policy)
    }

    pub fn end_frontend_preview(&self, epoch: u64, succeeded: bool) -> bool {
        self.state().end_frontend_preview(epoch, succeeded)
    }

    pub fn server_geometry_matches(&self, size: TerminalSize) -> bool {
        self.state().server_geometry_matches(size)
    }

    /// Whether the server has kept this pane at another size than `target`
    /// for long enough that the frontend should send `target` again, and
    /// how long until it should look again while that is pending; see
    /// `GeometryRepair`.
    pub fn server_geometry_repair(&self, target: TerminalSize) -> (bool, Option<Duration>) {
        let now = self.now();
        let mut st = self.state();
        let server = st.server_dimensions;
        let resend = st.geometry_repair.should_resend(server, target, now);
        (resend, st.geometry_repair.next_check_in(now))
    }

    /// Which fields or render rows block a takeover from settling on `size`.
    pub fn frontend_geometry_mismatch(&self, size: TerminalSize) -> Option<String> {
        let mut st = self.state();
        let dims = st.server_dimensions;
        if !render_dimensions_match_terminal_size(dims, size) {
            let mut fields = Vec::new();
            if dims.cols != size.cols {
                fields.push(format!("cols {}!={}", dims.cols, size.cols));
            }
            if dims.viewport_rows != size.rows {
                fields.push(format!("rows {}!={}", dims.viewport_rows, size.rows));
            }
            if dims.pixel_width != size.pixel_width {
                fields.push(format!("px_w {}!={}", dims.pixel_width, size.pixel_width));
            }
            if dims.pixel_height != size.pixel_height {
                fields.push(format!("px_h {}!={}", dims.pixel_height, size.pixel_height));
            }
            if dims.dpi != size.dpi {
                fields.push(format!("dpi {}!={}", dims.dpi, size.dpi));
            }
            return Some(fields.join(","));
        }

        let top = st.server_dimensions.physical_top;
        let end = top.saturating_add(size.rows as StableRowIndex);
        let mut total = 0usize;
        let mut sample = Vec::new();
        for row in top..end {
            let kind = match st.lines.get(&row) {
                Some(LineEntry::Line(_)) => continue,
                Some(LineEntry::Fetching(token)) => format!("Fetching/e{}", token.epoch),
                Some(LineEntry::LineAndFetching(_, token)) => {
                    format!("LineAndFetching/e{}", token.epoch)
                }
                Some(LineEntry::Stale(_)) => "Stale".to_string(),
                None => "Missing".to_string(),
            };
            total += 1;
            if sample.len() < 8 {
                sample.push(format!("{row}:{kind}"));
            }
        }
        (total > 0).then(|| {
            format!(
                "render rows {top}..{end} pending {total} [{}], cache_epoch={}",
                sample.join(" "),
                st.line_cache_epoch
            )
        })
    }

    /// Drive a render poll and populate every currently visible line while a
    /// takeover overlay is still opaque.  Returns true only after the server
    /// has confirmed `size` and none of those rows is stale or in flight.
    pub fn prime_frontend_geometry(self: &Arc<Self>, size: TerminalSize) -> bool {
        // One lock for the whole call: the row fetch it schedules goes
        // through `get_lines_locked`, never back through the public
        // `get_lines`, so this cannot deadlock on its own lock.
        let mut st = self.state();
        // Cancel any backoff this pane had settled into, but leave
        // `last_poll` alone so `poll` still rate-limits itself. Backdating
        // it forced a fresh render-changes RPC on *every* call, and the
        // reply emits PaneOutput, which marks the frontend dirty, which
        // draws, which calls this again: on a local socket that loop ran
        // thousands of times a second for the whole takeover window
        // instead of the ~50 the base interval allows.
        st.poll_interval = BASE_POLL_INTERVAL;
        self.poll(&mut st);
        let Some(mut visible) = confirmed_frontend_visible_range(st.server_dimensions, size) else {
            return false;
        };

        // This both returns any retained rows and schedules missing/stale
        // rows. Fetch completion emits PaneOutput and drives another overlay
        // frame, where the readiness check below can finally pass.
        let _ = self.get_lines_locked(&mut st, visible.clone());

        if confirmed_frontend_visible_range(st.server_dimensions, size).as_ref() != Some(&visible) {
            return false;
        }
        visible.all(|row| matches!(st.lines.get(&row), Some(LineEntry::Line(_))))
    }

    // ---- the render surface ---------------------------------------------

    pub fn get_lines(
        self: &Arc<Self>,
        lines: Range<StableRowIndex>,
    ) -> (StableRowIndex, Vec<Line>) {
        let mut st = self.state();
        self.get_lines_locked(&mut st, lines)
    }

    fn get_lines_locked(
        self: &Arc<Self>,
        st: &mut PaneState,
        lines: Range<StableRowIndex>,
    ) -> (StableRowIndex, Vec<Line>) {
        let mut result = vec![];
        let mut to_fetch = RangeSet::new();
        let now = self.now();
        let fetch_token = st.fetch_token(now);

        for idx in lines.clone() {
            let entry = match st.lines.pop(&idx) {
                Some(LineEntry::Line(line)) => {
                    result.push(line.clone());
                    if line.changed_since(st.seqno) {
                        to_fetch.add(idx);
                        LineEntry::Stale(line)
                    } else {
                        LineEntry::Line(line)
                    }
                }
                Some(LineEntry::LineAndFetching(line, then)) => {
                    result.push(line.clone());
                    LineEntry::LineAndFetching(line, then)
                }
                Some(LineEntry::Fetching(then)) => {
                    result.push(Line::with_width(st.dimensions.cols, SEQ_ZERO));
                    LineEntry::Fetching(then)
                }
                Some(LineEntry::Stale(line)) => {
                    result.push(line.clone());
                    to_fetch.add(idx);
                    LineEntry::LineAndFetching(line, fetch_token)
                }
                None => {
                    result.push(Line::with_width(st.dimensions.cols, SEQ_ZERO));
                    to_fetch.add(idx);
                    LineEntry::Fetching(fetch_token)
                }
            };

            if st.config.overlay_lag_indicator && idx == st.dimensions.physical_top {
                if st.is_tardy(now) {
                    let status = format!(
                        "ThinkTerm: {:.0?}⏳since last response",
                        st.since_last_response(now)
                    );
                    // Right align it in the tab
                    let col = st
                        .dimensions
                        .cols
                        .saturating_sub(wezterm_term::unicode_column_width(&status, None));

                    let mut attr = CellAttributes::default();
                    attr.set_foreground(AnsiColor::White);
                    attr.set_background(AnsiColor::Blue);

                    result
                        .last_mut()
                        .unwrap()
                        .overlay_text_with_attribute(col, &status, attr, SEQ_ZERO);
                }
            }

            st.lines.put(idx, entry);
        }

        log::trace!(
            "get_lines: {:?}, num result lines={}, will fetch {:?}",
            lines,
            result.len(),
            to_fetch
        );

        self.schedule_fetch_lines(st.dead, to_fetch, fetch_token);
        self.fetch_ahead(st, &lines, fetch_token);
        (lines.start, result)
    }

    /// Rows a scroll is about to reveal are asked for before they are
    /// painted: a row fetched on demand arrives a frame after it became
    /// visible, and a fast scroll paints a fresh band of blank rows every
    /// frame. Uncached rows around `painted` are tagged Fetching and
    /// requested separately, so the painted rows' fetch is not held up;
    /// once per cache epoch, the whole scrollback is fetched the same way
    /// where the host asks for it (the session server of this machine).
    ///
    /// Two limits keep this speculation harmless. Rows are only tagged
    /// while the cache can hold them beside the painted range: an insert
    /// past the capacity would evict a painted row, whose fetch would then
    /// be discarded and re-issued on every paint. And each speculative
    /// batch is admitted by the host's fetch rate limit first, so a slow
    /// link is not flooded; a refused batch leaves its rows untagged for a
    /// later paint to try again.
    fn fetch_ahead(
        self: &Arc<Self>,
        st: &mut PaneState,
        painted: &Range<StableRowIndex>,
        fetch_token: FetchToken,
    ) {
        if st.dead || !st.received {
            return;
        }
        let config = self.host.config();
        let dims = st.dimensions;
        let lowest = dims.scrollback_top;
        let highest = dims.physical_top + dims.viewport_rows as StableRowIndex;
        let painted_rows = painted.end.saturating_sub(painted.start).max(0) as usize;
        let mut budget = st.lines.cap().get().saturating_sub(painted_rows);
        let rate = config.fetch_rate_per_second();
        let now = self.now();

        // Uncached rows among the `limit` rows of `rows` nearest its
        // `nearest` end. The limit bounds the region, cached rows included:
        // bounding only the misses would let a band wider than the cache
        // fetch a different slice on every paint, each evicting the last.
        let uncached =
            |st: &mut PaneState, rows: Range<StableRowIndex>, limit: usize, nearest: Nearest| {
                let mut set = RangeSet::new();
                let mut seen = 0;
                let mut walk = rows.clone();
                while seen < limit {
                    let idx = match nearest {
                        Nearest::End => match walk.next_back() {
                            Some(idx) => idx,
                            None => break,
                        },
                        Nearest::Start => match walk.next() {
                            Some(idx) => idx,
                            None => break,
                        },
                    };
                    if idx < lowest || idx >= highest {
                        continue;
                    }
                    seen += 1;
                    if !st.lines.contains(&idx) {
                        set.add(idx);
                    }
                }
                set
            };
        let tag = |st: &mut PaneState, set: &RangeSet<StableRowIndex>| {
            for r in set.iter() {
                for idx in r.clone() {
                    st.lines.put(idx, LineEntry::Fetching(fetch_token));
                }
            }
        };

        let band =
            (dims.viewport_rows.max(1) * config.scrollback_lookahead_screens()) as StableRowIndex;
        if band > 0 && budget > 0 {
            let mut ahead = uncached(
                st,
                painted.start - band..painted.start,
                budget / 2,
                Nearest::End,
            );
            let below = uncached(
                st,
                painted.end..painted.end + band,
                budget / 2,
                Nearest::Start,
            );
            for r in below.iter() {
                ahead.add_range(r.clone());
            }
            if !ahead.is_empty() {
                if !st.fetch_limiter.admit(rate, 1, now) {
                    log::trace!("fetch ahead of {:?} refused by the rate limit", painted);
                    return;
                }
                let count: usize = ahead.iter().map(|r| r.len()).sum();
                budget = budget.saturating_sub(count);
                tag(st, &ahead);
                log::trace!("fetching ahead {:?} around {:?}", ahead, painted);
                self.schedule_fetch_lines(st.dead, ahead, fetch_token);
            }
        }

        let grew = dims.physical_top - st.warmed_top >= dims.viewport_rows as StableRowIndex;
        if config.warm_scrollback()
            && budget > 0
            && (st.warmed_epoch != Some(st.line_cache_epoch) || grew)
        {
            // Speculation must not scale to the user's entire history limit.
            // Bound the region as well as misses, so later paints do not
            // gradually warm another 3,500 rows outside this recent window.
            let warm = uncached(st, lowest..highest, budget.min(3500), Nearest::End);
            if !warm.is_empty() {
                if !st.fetch_limiter.admit(rate, 1, now) {
                    log::trace!("scrollback warm refused by the rate limit");
                    return;
                }
                tag(st, &warm);
                log::trace!("warming scrollback {:?}", warm);
                self.schedule_fetch_lines(st.dead, warm, fetch_token);
            }
            st.warmed_epoch = Some(st.line_cache_epoch);
            st.warmed_top = dims.physical_top;
        }
    }

    pub fn changed_since(
        self: &Arc<Self>,
        lines: Range<StableRowIndex>,
        seqno: SequenceNo,
    ) -> RangeSet<StableRowIndex> {
        let mut st = self.state();
        self.poll(&mut st);

        let mut result = RangeSet::new();
        for r in lines {
            match st.lines.get(&r) {
                None => {
                    result.add(r);
                }
                Some(
                    LineEntry::Line(line)
                    | LineEntry::Stale(line)
                    | LineEntry::LineAndFetching(line, _),
                ) if line.changed_since(seqno) => {
                    result.add(r);
                }
                _ => {}
            }
        }

        // If we're behind receiving an update, invalidate the top row so
        // that the indicator will update in a more timely fashion
        let now = self.now();
        if st.is_tardy(now) {
            // ... but take care to avoid always reporting it as dirty, so
            // that we don't end up busy looping just to repaint it
            if now.saturating_duration_since(st.last_late_dirty) >= Duration::from_secs(1) {
                result.add(st.dimensions.physical_top);
                st.last_late_dirty = now;
            }
        }

        if !result.is_empty() {
            log::trace!("get_changed_since: {} -> {:?}", seqno, result);
        }

        result
    }

    /// True when a row the host is actually displaying is waiting on work
    /// that only a paint performs. `viewport_top` is the displayed viewport
    /// origin (None = following the tail): scoping to the painted range --
    /// not the live screen -- is what keeps this from latching, and Stale
    /// entries for rows outside it are normal and must not count.
    ///
    /// Checking also repairs a fetch whose detached future may have been
    /// lost. Its retry deadline doubles after every supersession, so a
    /// healthy but consistently slow request eventually gets a window long
    /// enough to complete instead of losing forever to a fixed watchdog.
    pub fn render_looks_stalled(self: &Arc<Self>, viewport_top: Option<StableRowIndex>) -> bool {
        let st = self.state();
        let top = viewport_top.unwrap_or(st.dimensions.physical_top);
        let range = top..top.saturating_add(st.dimensions.viewport_rows as StableRowIndex);
        drop(st);
        self.render_looks_stalled_in(range)
    }

    /// The same check over exactly the rows a client shows. A client whose
    /// view is shorter than the pane's screen must not ask about rows past
    /// its bottom: those may not exist, and a row that does not exist
    /// reads as "repaint" forever.
    pub fn render_looks_stalled_in(self: &Arc<Self>, range: Range<StableRowIndex>) -> bool {
        let mut st = self.state();
        if !crate::decide::render_watchdog_should_run(st.dead) {
            return false;
        }
        let decision = line_watchdog_decision(&mut st.lines, range, self.now());
        if decision.repaint {
            st.poll_interval = BASE_POLL_INTERVAL;
        }
        for batch in decision.retries {
            self.schedule_fetch_lines(st.dead, batch.rows, batch.token);
        }
        decision.repaint
    }

    // ---- pushes ---------------------------------------------------------

    /// Queue a render push; it is applied by the drain, in order.
    pub fn queue_render_delta(self: &Arc<Self>, delta: GetPaneRenderChangesResponse) {
        if self.render_deltas.lock().push(delta) {
            let weak = Arc::downgrade(self);
            self.host
                .spawner()
                .spawn_detached(Box::pin(Self::drain_render_deltas(weak)));
        }
    }

    async fn drain_render_deltas(weak: Weak<Self>) {
        let _draining = DrainingDeltas(weak.clone());
        loop {
            let Some(me) = weak.upgrade() else {
                return;
            };
            let Some((delta, has_newer)) = me.render_deltas.lock().take() else {
                return;
            };
            // A push with a newer one already behind it is applied without
            // asking for pictures: the newer push names the current ones, and
            // rows whose pictures are missing keep showing the previous frame.
            let started = me.now();
            let fetched_pictures = !has_newer;
            me.apply_render_delta(delta, has_newer).await;
            log::debug!(
                "render push for pane {} applied in {:?} (fetched pictures: {})",
                me.host_pane_id,
                me.now().saturating_duration_since(started),
                fetched_pictures
            );
        }
    }

    /// Apply one push. With `has_newer` set, newer pushes wait behind this
    /// one and it is applied without fetching pictures; a row it then has
    /// to leave out is marked dirty unless one of those pushes brings it,
    /// since the server sent it as a bonus row and nothing else would ever
    /// fetch it again.
    async fn apply_render_delta(
        self: &Arc<Self>,
        mut delta: GetPaneRenderChangesResponse,
        has_newer: bool,
    ) {
        let bonus_lines = std::mem::take(&mut delta.bonus_lines);
        let (bonus_lines, left_out) = hydrate_lines(
            &*self.host,
            &self.images,
            delta.pane_id,
            bonus_lines,
            !has_newer,
        )
        .await;
        // A newer push can supersede image rows, and an image-domain change
        // during hydration can discard them even without another push. In
        // either case, recover any rows that the queue does not already carry.
        if !left_out.is_empty() {
            let carried = self.render_deltas.lock().rows_carried();
            for row in left_out {
                if !carried.contains(row) {
                    delta.dirty_lines.push(row..row + 1);
                }
            }
        }

        let mut st = self.state();
        self.apply_changes_to_surface(&mut st, delta, bonus_lines);
    }

    fn apply_changes_to_surface(
        self: &Arc<Self>,
        st: &mut PaneState,
        delta: GetPaneRenderChangesResponse,
        bonus_lines: Vec<(StableRowIndex, Line)>,
    ) {
        // Reject impossible geometry before it reaches cache seeding or any
        // frontend. Clamping only dirty ranges leaves those other walks open.
        if !delta.dimensions.is_plausible() {
            log::warn!("ignoring render update with invalid pane dimensions");
            return;
        }
        log::trace!(
            "apply_changes_to_surface local={} remote={}",
            self.host_pane_id,
            self.remote_pane_id
        );
        let now = self.now();
        st.poll_interval = BASE_POLL_INTERVAL;
        st.last_recv_time = now;
        st.received = true;

        if delta.alt_screen != st.alt_screen {
            st.alt_screen = delta.alt_screen;
            // Rows cached from the other screen are stamped with seqnos
            // the server will never move past, so nothing else would ever
            // refetch them; the alternate screen's rows in particular sit
            // at the stable indices of the primary's oldest scrollback.
            st.invalidate_line_cache(true);
        }
        st.mouse_grabbed = delta.mouse_grabbed;
        st.keyboard_encoding = delta.keyboard_encoding.into();

        let live_preview_accepts_snapshot = st.frontend_preview.is_none_or(|preview| {
            preview.policy != FrontendPreviewPolicy::LiveResize
                || render_dimensions_match_terminal_size(delta.dimensions, preview.size)
        });

        // Every row of every dirty range is walked below under the lock,
        // and the ranges come off the wire, so the walk has to be bounded
        // by something the wire does not set. Below the viewport's top the
        // bound is a viewport taller than any screen; above it, the walk
        // only marks cached rows stale, and past as many rows as the cache
        // holds that is the same work as marking the whole cache stale, so
        // the walk stops there and the one sweep takes over.
        const MAX_VIEWPORT_ROWS: usize = thinkterm_proto::layout::MAX_PANE_CELLS;
        let physical_top = delta.dimensions.physical_top;
        let row_end = physical_top.saturating_add(
            delta.dimensions.viewport_rows.min(MAX_VIEWPORT_ROWS) as StableRowIndex,
        );
        let cache_rows = st.lines.cap().get() as StableRowIndex;
        let walk_start = physical_top
            .saturating_sub(cache_rows)
            .max(delta.dimensions.scrollback_top);
        let mut dirty = RangeSet::new();
        let mut sweep_cache = false;
        for r in delta.dirty_lines {
            let lo = r.start.max(delta.dimensions.scrollback_top);
            let hi = r.end.min(row_end);
            if lo >= hi {
                continue;
            }
            if lo < walk_start {
                sweep_cache = true;
            }
            let lo = lo.max(walk_start);
            if lo < hi {
                dirty.add_range(lo..hi);
            }
        }
        if sweep_cache {
            invalidate_line_entries(&mut st.lines, true);
        }
        if delta.cursor_position != st.cursor_position {
            dirty.add(st.cursor_position.y);
            // But note that the server may have sent this in bonus_lines;
            // we'll address that below
            dirty.add(delta.cursor_position.y);
        }

        // Keep track of the approximate round trip time by recording how
        // long it took for this response to come back
        if let Some(serial) = delta.input_serial {
            st.last_input_rtt = serial.elapsed_millis_since(self.host.clock().wall_millis());
        }

        // When it comes to updating the cursor position, if the update was tagged
        // with keyboard input, we'll only take the position if the update comes from
        // the most recent key event.  This helps to prevent the cursor wiggling if the
        // user is typing more than one character per roundtrip interval--the wiggle
        // manifests because we may have already predicted a local cursor move forwards
        // by one character, and we may receive the response to the prior update after
        // we have rendered that, and then shortly receive the most recent response.
        // The result of that is that the cursor moves right one, left one and then
        // finally right one in quick succession.
        // If the delta was not from an input event then we trust it; this is most
        // like due to a unilateral movement by the application on the other end.
        if live_preview_accepts_snapshot
            && (delta.input_serial.is_none()
                || delta.input_serial.unwrap_or(InputSerial::empty()) >= st.input_serial)
        {
            st.cursor_position = delta.cursor_position;
        }
        let prior_server_dimensions = st.server_dimensions;
        st.server_dimensions = delta.dimensions;
        let (visible_dimensions, invalidate) = resolve_server_geometry(
            st.dimensions,
            prior_server_dimensions,
            delta.dimensions,
            st.frontend_preview.is_some(),
        );
        st.dimensions = visible_dimensions;
        match invalidate {
            // During a preview, retain old rows while marking them stale. This
            // prevents a blank flash as a full-screen application redraws.
            Some(true) => st.invalidate_line_cache(true),
            // A width this surface did not have keeps them too: clearing them
            // blanked the whole pane until the refetch landed, on every push
            // while the server and the frontend disagreed about the width.
            Some(false) => st.invalidate_line_cache_for_width(visible_dimensions.cols),
            None => {}
        }
        // Retaining rows is not enough when the resize rewrapped the
        // scrollback: the viewport's stable range moved and the retained
        // rows are keyed by where it was. Before the bonus rows land (they
        // replace seeds), carry the old rows over by screen position so the
        // step draws them instead of blank. See `seed_moved_viewport_rows`.
        if render_geometry_changed(prior_server_dimensions, delta.dimensions) {
            seed_moved_viewport_rows(
                &mut st.lines,
                prior_server_dimensions.physical_top,
                delta.dimensions.physical_top,
                st.dimensions.viewport_rows,
            );
        }
        st.title = delta.title;
        st.working_dir = delta.working_dir.map(Into::into);
        log::trace!(
            "server says: seqno from {} -> {} for local_pane_id={}",
            st.seqno,
            delta.seqno,
            self.host_pane_id
        );
        st.seqno = delta.seqno;

        let rules = self.host.config().hyperlink_rules();
        for (stable_row, line) in bonus_lines {
            // A live preview can have several acknowledged server grids in
            // flight over its lifetime. Rows bundled with an intermediate
            // grid are authoritative for that grid only; accepting them into
            // the final-width cache is exactly how old-width fragments survive
            // after the divider stops.
            if !live_preview_accepts_snapshot {
                // ...but not dropped either. The seqno below advances past
                // this grid whether or not its rows are kept, so the rows it
                // rewrapped are in no later delta's dirty set: dropped, they
                // were a blank row each until the paint's own refetch came
                // back -- one blank frame per intermediate step of a fast
                // drag. Kept as Stale they are drawn meanwhile and still
                // refetched by the next paint, which is when the final grid
                // answers; and the row stays in `dirty` for the same reason.
                st.put_line(stable_row, line, &rules, None);
                st.make_stale(stable_row);
                continue;
            }
            log::trace!("bonus line {} seqno={}", stable_row, line.current_seqno());
            st.put_line(stable_row, line, &rules, None);
            dirty.remove(stable_row);
        }

        log::trace!(
            "apply_changes_to_surface: Generate PaneOutput event for local={}",
            self.host_pane_id
        );
        // Under the lock, as it always was: the host must not re-enter.
        self.host.events().pane_output(self.host_pane_id);

        let mut to_fetch = RangeSet::new();
        log::trace!("dirty as of seq {} -> {:?}", delta.seqno, dirty);
        for r in dirty.iter() {
            for stable_row in r.clone() {
                // If a line is in the (probable) viewport region,
                // then we'll likely want to fetch it.
                // If it is outside that region, remove it from our cache
                // so that we'll fetch it on demand later.
                let fetchable = stable_row >= delta.dimensions.physical_top;
                let prior = st.lines.pop(&stable_row);
                let prior_kind = prior.as_ref().map(|e| e.kind());
                if !fetchable {
                    // Above the server's viewport: keep what we have, as
                    // Stale, and let the paint refetch it when it is next
                    // shown. The entry is already popped, so it goes back
                    // here -- `make_stale` after the pop found nothing and
                    // dropped the row, which is what left a scrolled-up pane
                    // (or a preview drawn from the old top) blank across
                    // every step of a divider drag: a rewrap dirties the
                    // whole scrollback, and every row of it fell out.
                    log::trace!("make {} stale bcos not fetchable", stable_row);
                    match prior {
                        Some(LineEntry::Stale(line))
                        | Some(LineEntry::Line(line))
                        | Some(LineEntry::LineAndFetching(line, _)) => {
                            st.lines.put(stable_row, LineEntry::Stale(line));
                        }
                        Some(LineEntry::Fetching(_)) | None => {}
                    }
                    continue;
                }
                to_fetch.add(stable_row);
                let token = st.fetch_token(now);
                let entry = match prior {
                    Some(LineEntry::Fetching(_)) | None => LineEntry::Fetching(token),
                    Some(LineEntry::LineAndFetching(old, ..))
                    | Some(LineEntry::Stale(old))
                    | Some(LineEntry::Line(old)) => LineEntry::LineAndFetching(old, token),
                };
                log::trace!(
                    "row {} {:?} -> {:?} due to dirty and IN viewport",
                    stable_row,
                    prior_kind,
                    entry.kind()
                );
                st.lines.put(stable_row, entry);
            }
        }
        if !to_fetch.is_empty() {
            let rate = self.host.config().fetch_rate_per_second();
            if st.fetch_limiter.admit(rate, 1, now) {
                let token = st.fetch_token(now);
                self.schedule_fetch_lines(st.dead, to_fetch, token);
            } else {
                log::warn!(
                    "exceeded fetch throttle, drop {:?} and mark stale",
                    to_fetch
                );
                for r in to_fetch.iter() {
                    for stable_row in r.clone() {
                        st.make_stale(stable_row);
                    }
                }
            }
        }
    }

    // ---- fetching and polling -------------------------------------------

    fn schedule_fetch_lines(
        self: &Arc<Self>,
        dead: bool,
        to_fetch: RangeSet<StableRowIndex>,
        fetch_token: FetchToken,
    ) {
        if to_fetch.is_empty() || dead {
            return;
        }

        log::trace!(
            "will fetch lines {:?} for remote tab id {} at {:?}",
            to_fetch,
            self.remote_pane_id,
            fetch_token,
        );

        let host = Arc::clone(&self.host);
        let images = Arc::clone(&self.images);
        let remote_pane_id = self.remote_pane_id;
        let weak = Arc::downgrade(self);

        self.host.spawner().spawn_detached(Box::pin(async move {
            let result = request(
                host.link(),
                Pdu::GetLines(GetLines {
                    pane_id: remote_pane_id,
                    lines: to_fetch.clone().into(),
                }),
                |pdu| match pdu {
                    Pdu::GetLinesResponse(response) => Ok(response),
                    other => Err(other),
                },
            )
            .await;

            let result = match result {
                Ok(result) => {
                    // A reply may already belong to a dropped pane or an
                    // obsolete screen. Reject it before fetching its images,
                    // without holding the pane or its lock across hydration.
                    // apply_lines checks again after that await.
                    if !weak
                        .upgrade()
                        .is_some_and(|pane| pane.lock_current_line_fetch(fetch_token).is_some())
                    {
                        return;
                    }
                    let (lines, _) =
                        hydrate_lines(&*host, &images, remote_pane_id, result.lines, true).await;
                    Ok(lines)
                }
                Err(err) => Err(err),
            };
            Self::apply_lines(&weak, result, to_fetch, fetch_token);
        }));
    }

    /// Reject obsolete work at either side of image hydration. The same
    /// epoch notification is used at both gates so an early discard still
    /// schedules the repaint needed to refetch stale rows, once per epoch.
    fn lock_current_line_fetch(
        &self,
        fetch_token: FetchToken,
    ) -> Option<crate::LockGuard<'_, PaneState>> {
        let mut st = self.state();
        // A retained pane may become dead while a reply is in flight. Apply
        // that reply as before; only a dropped pane or obsolete epoch discards it.
        if fetch_token_is_current(fetch_token, st.line_cache_epoch) {
            return Some(st);
        }
        log::trace!(
            "discarding line fetch for pane {} from epoch {} because current epoch is {}",
            self.host_pane_id,
            fetch_token.epoch,
            st.line_cache_epoch
        );
        let already_notified = st.epoch_discard_notified == st.line_cache_epoch;
        st.epoch_discard_notified = st.line_cache_epoch;
        drop(st);
        if !already_notified {
            self.host.events().pane_output(self.host_pane_id);
        }
        None
    }

    fn apply_lines(
        weak: &Weak<Self>,
        result: anyhow::Result<Vec<(StableRowIndex, Line)>>,
        to_fetch: RangeSet<StableRowIndex>,
        fetch_token: FetchToken,
    ) {
        // The pane is gone: nothing to update, nobody to tell.
        let Some(me) = weak.upgrade() else {
            return;
        };
        let mut notify_pane_output = true;
        {
            let Some(mut st) = me.lock_current_line_fetch(fetch_token) else {
                return;
            };

            match result {
                Ok(lines) => {
                    let rules = me.host.config().hyperlink_rules();
                    let mut returned = RangeSet::new();

                    log::trace!("fetch complete for {:?} with {:?}", to_fetch, fetch_token);
                    for (stable_row, line) in lines.into_iter() {
                        returned.add(stable_row);
                        st.put_line(stable_row, line, &rules, Some(fetch_token));
                    }
                    // The terminal can scroll or resize while GetLines is in
                    // flight, so a successful response is allowed to omit a
                    // row that no longer exists in its stable range. Leaving
                    // that row tagged Fetching would suppress every future
                    // request for it and can hold an opaque takeover mask up
                    // forever.
                    release_unreturned_fetches(&mut st.lines, &to_fetch, &returned, fetch_token);
                }
                Err(err) => {
                    log::error!("get_lines failed: {}", err);
                    // No PaneOutput for a failure: notifying would repaint,
                    // the repaint would re-fetch the Stale rows, and a
                    // persistent error would spin that loop at RPC rate.
                    // The rows stay Stale; the next natural paint (input,
                    // or the ~1s render watchdog) retries them instead.
                    notify_pane_output = false;
                    for r in to_fetch.iter() {
                        for stable_row in r.clone() {
                            let entry = match st.lines.pop(&stable_row) {
                                Some(LineEntry::Fetching(then)) if then == fetch_token => {
                                    // leave it popped
                                    continue;
                                }
                                Some(LineEntry::LineAndFetching(line, then))
                                    if then == fetch_token =>
                                {
                                    // Stale, not Line: the local copy's seqno
                                    // is already <= the pane's, so a Line
                                    // would never satisfy changed_since and
                                    // the row would keep its old content
                                    // forever. Stale rows are re-fetched by
                                    // the next paint.
                                    LineEntry::Stale(line)
                                }
                                Some(entry) => entry,
                                None => continue,
                            };
                            st.lines.put(stable_row, entry);
                        }
                    }
                }
            }
        }
        if notify_pane_output {
            log::trace!(
                "Generate PaneOutput event for local_pane_id={}",
                me.host_pane_id
            );
            me.host.events().pane_output(me.host_pane_id);
        }
    }

    fn poll(self: &Arc<Self>, st: &mut PaneState) {
        let now = self.now();
        // A paint of any pane of this connection is the occasion to let a
        // stopped stream's frames go; the store is shared across them.
        self.images
            .lock()
            .release_idle(now, crate::images::IDLE_IMAGE_RELEASE);
        let mut forced_retry = false;
        let in_flight = st.poll_in_flight.load(Ordering::SeqCst);
        if in_flight != 0 {
            let deadline = stall_timeout(POLL_STALL_BASE, st.poll_stall_attempt);
            if poll_should_be_retried(st.last_poll, st.poll_stall_attempt, now) {
                // A detached completion may have been lost. Increase the
                // next deadline instead of using a fixed threshold: a slow
                // but healthy poll must eventually be allowed to win.
                log::warn!(
                    "pane {} poll generation {} exceeded {:?} (attempt {}); replacing it",
                    self.host_pane_id,
                    in_flight,
                    deadline,
                    st.poll_stall_attempt,
                );
                if !claim_poll_completion(&st.poll_in_flight, in_flight) {
                    return;
                }
                st.poll_stall_attempt = st.poll_stall_attempt.saturating_add(1);
                forced_retry = true;
            } else {
                // We have a poll in progress
                return;
            }
        }

        if !forced_retry {
            if now.saturating_duration_since(st.last_poll) < st.poll_interval {
                return;
            }
            st.poll_stall_attempt = 0;
        }

        let interval = st.poll_interval;
        let interval = (interval + interval).min(MAX_POLL_INTERVAL);
        st.poll_interval = interval;

        st.last_poll = now;
        st.poll_gen = st.poll_gen.wrapping_add(1).max(1);
        let gen = st.poll_gen;
        st.poll_in_flight.store(gen, Ordering::SeqCst);
        let poll_in_flight = Arc::clone(&st.poll_in_flight);
        let remote_pane_id = self.remote_pane_id;
        let host = Arc::clone(&self.host);
        let weak = Arc::downgrade(self);
        self.host.spawner().spawn_detached(Box::pin(async move {
            let alive = match request(
                host.link(),
                Pdu::GetPaneRenderChanges(GetPaneRenderChanges {
                    pane_id: remote_pane_id,
                }),
                |pdu| match pdu {
                    Pdu::LivenessResponse(response) => Ok(response),
                    other => Err(other),
                },
            )
            .await
            {
                Ok(resp) => resp.is_alive,
                // if we got a timeout on a reconnectable, don't
                // consider the tab to be dead; that helps to
                // avoid having a tab get shuffled around
                Err(_) => host.link().is_reconnectable(),
            };

            // Cleared through the shared handle before anything that can
            // bail: if the pane is gone, a flag left set would silence every
            // future poll. Generation checked: a poll the watchdog already
            // superseded must not clear its successor's latch or apply its
            // stale answer.
            if !claim_poll_completion(&poll_in_flight, gen) {
                return;
            }

            let Some(me) = weak.upgrade() else {
                return;
            };
            let mut st = me.state();
            st.dead = !alive;
            st.last_recv_time = host.clock().now();
            st.poll_stall_attempt = 0;
        }));
    }
}

/// Marks the drain over when its task ends, however it ends, so a task
/// dropped without finishing cannot leave every later push waiting.
struct DrainingDeltas<H: SessionHost>(Weak<PaneSession<H>>);

struct PendingSceneImage<H: SessionHost> {
    pane: Weak<PaneSession<H>>,
    key: SceneImageKey,
}

impl<H: SessionHost> Drop for PendingSceneImage<H> {
    fn drop(&mut self) {
        if let Some(pane) = self.pane.upgrade() {
            let mut pending = pane.scene_images.lock();
            pending.pending.remove(&self.key);
            if pending.pending.is_empty() { pending.pending = Default::default(); }
            if pending.failed.is_empty() { pending.failed = Vec::new(); }
        }
    }
}

impl<H: SessionHost> Drop for DrainingDeltas<H> {
    fn drop(&mut self) {
        if let Some(pane) = self.0.upgrade() {
            pane.render_deltas.lock().end_drain();
        }
    }
}

/// Which end of a range `fetch_ahead` takes rows from first.
#[derive(Clone, Copy)]
enum Nearest {
    Start,
    End,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_impossible_listed_size_starts_in_bounds() {
        use thinkterm_proto::layout::MAX_PANE_CELLS;
        let wild = RenderableDimensions {
            cols: usize::MAX,
            viewport_rows: 100_000,
            scrollback_rows: 100_000,
            physical_top: StableRowIndex::MAX,
            scrollback_top: 0,
            dpi: 72,
            pixel_width: usize::MAX,
            pixel_height: 800,
            reverse_video: false,
        };
        let tamed = plausible_dimensions(wild);
        assert!(tamed.is_plausible());
        assert_eq!((tamed.cols, tamed.viewport_rows), (MAX_PANE_CELLS, MAX_PANE_CELLS));
        assert_eq!(tamed.physical_top, 0);
        // An ordinary size passes through untouched.
        let ordinary = RenderableDimensions {
            cols: 80,
            viewport_rows: 24,
            scrollback_rows: 24,
            physical_top: 0,
            ..wild
        };
        let ordinary = RenderableDimensions {
            pixel_width: 640,
            ..ordinary
        };
        assert_eq!(plausible_dimensions(ordinary), ordinary);
    }
    use crate::host::{DetachedFuture, HostConfig, LinkError};
    use crate::input::{LocalFuture, PaneLink};
    use codec::{GetImageCellResponse, GetLinesResponse, UnitResponse};
    use std::cell::{Cell, RefCell};
    use std::future::Future;
    use std::pin::Pin;
    use std::rc::Rc;
    use std::task::{Context, Poll, Waker};
    use termwiz::cell::CellAttributes;
    use termwiz::image::{ImageCell, ImageData, TextureCoordinate};
    use thinkterm_proto::TabId;

    /// Poll to completion with a no-op waker: every future here is ready
    /// after a poll or two, nothing waits on the outside.
    fn spin_on<F: Future>(mut fut: F) -> F::Output {
        let mut fut = unsafe { Pin::new_unchecked(&mut fut) };
        let mut cx = Context::from_waker(Waker::noop());
        for _ in 0..1000 {
            if let Poll::Ready(out) = fut.as_mut().poll(&mut cx) {
                return out;
            }
        }
        panic!("a test future never became ready");
    }

    struct TestClock(Cell<u64>);
    impl Clock for TestClock {
        fn now(&self) -> crate::clock::Timestamp {
            crate::clock::Timestamp::from_micros(self.0.get())
        }
        fn wall_millis(&self) -> u64 {
            5_000
        }
    }

    /// Tasks run when the test says so, so a fetch can be "in flight"
    /// across an epoch bump.
    #[derive(Default)]
    struct TestSpawner(RefCell<Vec<DetachedFuture>>);
    impl Spawner for TestSpawner {
        fn spawn_detached(&self, fut: DetachedFuture) {
            self.0.borrow_mut().push(fut);
        }
    }
    impl TestSpawner {
        fn run_all(&self) {
            for _ in 0..100 {
                let tasks = std::mem::take(&mut *self.0.borrow_mut());
                if tasks.is_empty() {
                    return;
                }
                for task in tasks {
                    spin_on(task);
                }
            }
            panic!("tasks kept spawning tasks");
        }
    }

    #[derive(Default)]
    struct TestEvents {
        outputs: Cell<u32>,
    }
    impl SessionEvents for TestEvents {
        fn pane_output(&self, _pane: HostPaneId) {
            self.outputs.set(self.outputs.get() + 1);
        }
        fn alert(&self, _pane: HostPaneId, _alert: wezterm_term::Alert) {}
        fn agent_status_changed(&self, _pane: HostPaneId) {}
        fn foreground_program_changed(&self, _pane: HostPaneId) {}
        fn pane_removed(&self, _pane: HostPaneId) {}
        fn pane_focused(&self, _pane: HostPaneId) {}
        fn input_recorded(&self) {}
    }

    struct TestConfig {
        lookahead: Cell<usize>,
        warm: Cell<bool>,
        rate: Cell<u32>,
    }
    impl Default for TestConfig {
        fn default() -> Self {
            Self {
                lookahead: Cell::new(0),
                warm: Cell::new(false),
                rate: Cell::new(100),
            }
        }
    }
    impl HostConfig for TestConfig {
        fn hyperlink_rules(&self) -> Arc<Vec<termwiz::hyperlink::Rule>> {
            Arc::new(Vec::new())
        }
        fn fetch_rate_per_second(&self) -> u32 {
            self.rate.get()
        }
        fn scrollback_lookahead_screens(&self) -> usize {
            self.lookahead.get()
        }
        fn warm_scrollback(&self) -> bool {
            self.warm.get()
        }
    }

    /// Answers GetLines from a table of rows, everything else with
    /// UnitResponse.
    #[derive(Default)]
    struct TestLink {
        rows: RefCell<Vec<(StableRowIndex, String)>>,
        asked: RefCell<Vec<&'static str>>,
        rows_asked: Cell<usize>,
        image: RefCell<Option<Arc<ImageData>>>,
        image_frames_from: Cell<u32>,
        refuse_by_id: Cell<bool>,
        line_reply_ready: RefCell<Option<Rc<Cell<bool>>>>,
        image_reply_ready: RefCell<Option<Rc<Cell<bool>>>>,
    }
    impl PduLink for TestLink {
        type Request = LocalFuture<Result<Pdu, LinkError>>;
        fn request(&self, pdu: Pdu) -> Self::Request {
            self.asked.borrow_mut().push(pdu.pdu_name());
            let ready = match &pdu {
                Pdu::GetLines(_) => self.line_reply_ready.borrow().clone(),
                Pdu::GetImageCell(_) | Pdu::GetKittyImage(_) => self.image_reply_ready.borrow().clone(),
                _ => None,
            };
            let answer = match pdu {
                Pdu::GetLines(req) => {
                    self.rows_asked.set(
                        self.rows_asked.get() + req.lines.iter().map(|r| r.len()).sum::<usize>(),
                    );
                    let table = self.rows.borrow();
                    let mut lines: Vec<(StableRowIndex, Line)> = req
                        .lines
                        .iter()
                        .flat_map(|range| range.clone())
                        .filter_map(|row| {
                            table.iter().find(|(r, _)| *r == row).map(|(r, text)| {
                                (
                                    *r,
                                    Line::from_text(
                                        text,
                                        &CellAttributes::default(),
                                        SEQ_ZERO,
                                        None,
                                    ),
                                )
                            })
                        })
                        .collect();
                    if let Some(image) = self.image.borrow().as_ref() {
                        for (_, line) in &mut lines {
                            line.cells_mut()[0]
                                .attrs_mut()
                                .attach_image(Box::new(ImageCell::new(
                                    TextureCoordinate::new_f32(0.0, 0.0),
                                    TextureCoordinate::new_f32(1.0, 1.0),
                                    Arc::clone(image),
                                )));
                        }
                    }
                    Pdu::GetLinesResponse(GetLinesResponse {
                        pane_id: req.pane_id,
                        lines: lines.into(),
                    })
                }
                Pdu::GetPaneRenderChanges(req) => Pdu::LivenessResponse(codec::LivenessResponse {
                    pane_id: req.pane_id,
                    is_alive: true,
                }),
                Pdu::GetKittyImage(_) if self.refuse_by_id.get() => Pdu::ErrorResponse(codec::ErrorResponse {
                    reason: "unknown request".to_string(),
                }),
                Pdu::GetKittyImage(req) => Pdu::GetImageCellResponse(GetImageCellResponse {
                    pane_id: req.pane_id, data: self.image.borrow().clone(),
                    data_generation: self.image.borrow().as_ref().map_or(0, |image| image.generation()),
                    frames_from: self.image_frames_from.get(),
                }),
                Pdu::GetImageCell(req) => Pdu::GetImageCellResponse(GetImageCellResponse {
                    pane_id: req.pane_id,
                    data: self.image.borrow().clone(),
                    data_generation: req.data_generation,
                    frames_from: self.image_frames_from.get(),
                }),
                _ => Pdu::UnitResponse(UnitResponse {}),
            };
            Box::pin(async move {
                if let Some(ready) = ready {
                    std::future::poll_fn(|_| {
                        if ready.get() {
                            Poll::Ready(())
                        } else {
                            Poll::Pending
                        }
                    })
                    .await;
                }
                Ok(answer)
            })
        }
        fn is_reconnectable(&self) -> bool {
            true
        }
        fn connection_generation(&self) -> u64 {
            1
        }
    }
    impl PaneLink for TestLink {
        type Prepare = LocalFuture<anyhow::Result<bool>>;
        fn prepare(&self, _remote_tab_id: TabId) -> Self::Prepare {
            Box::pin(async { Ok(true) })
        }
    }

    struct TestHost {
        canonical_images: Cell<bool>,
        image_domain: Cell<usize>,
        clock: TestClock,
        spawner: TestSpawner,
        events: TestEvents,
        link: TestLink,
        config: TestConfig,
    }
    impl SessionHost for TestHost {
        type Clock = TestClock;
        type Spawner = TestSpawner;
        type Events = TestEvents;
        type Link = TestLink;
        type Config = TestConfig;
        fn clock(&self) -> &TestClock {
            &self.clock
        }
        fn spawner(&self) -> &TestSpawner {
            &self.spawner
        }
        fn events(&self) -> &TestEvents {
            &self.events
        }
        fn link(&self) -> &TestLink {
            &self.link
        }
        fn config(&self) -> &TestConfig {
            &self.config
        }
        fn image_request(&self, request: codec::GetImageCell, image_id: Option<u32>) -> Pdu {
            if let Some(image_id) = image_id.filter(|_| self.canonical_images.get()) {
                return Pdu::GetKittyImage(codec::GetKittyImage {
                    pane_id: request.pane_id, image_id, data_hash: request.data_hash,
                    have_frames: request.have_frames, image_epoch: None,
                });
            }
            Pdu::GetImageCell(request)
        }
        fn image_domain(&self) -> crate::host::ImageDomainKey {
            self.image_domain.get()
        }
    }

    fn scene_picture(pixel: u8) -> Arc<ImageData> {
        Arc::new(ImageData::with_data(termwiz::image::ImageDataType::new_single_frame_content_hashed(1, 1, vec![pixel; 4])))
    }

    #[test]
    fn scene_fetches_share_pixels_bound_pending_work_and_release_cancelled_requests() {
        let (host, pane) = session(&[]);
        let picture = scene_picture(7);
        *host.link.image.borrow_mut() = Some(picture.clone());
        assert!(pane.scene_image(2, picture.hash(), 0, 1).is_none());
        assert!(host.spawner.0.borrow().is_empty(), "legacy peers cannot fetch a scene image by cell zero");
        host.canonical_images.set(true);
        for _ in 0..10 { assert!(pane.scene_image(2, picture.hash(), 0, 1).is_none()); }
        assert_eq!(host.spawner.0.borrow().len(), 1);
        host.spawner.run_all();
        let held = pane.scene_image(2, picture.hash(), 0, 1).unwrap();
        assert!(Arc::ptr_eq(&picture, &held));
        assert_eq!(host.events.outputs.get(), 1);
        assert_eq!(pane.scene_images.lock().pending.capacity(), 0);
        for id in 10..20 { pane.scene_image(id, [id as u8; 32], 0, 1); }
        assert_eq!(host.spawner.0.borrow().len(), 4);
        assert_eq!(pane.scene_images.lock().pending.len(), 4);
        host.spawner.0.borrow_mut().clear();
        assert_eq!(pane.scene_images.lock().pending.capacity(), 0);
        pane.scene_image(10, [10; 32], 0, 1);
        assert_eq!(host.spawner.0.borrow().len(), 1);
    }

    #[test]
    fn absent_or_wrong_scene_pixels_wait_before_they_are_fetched_again() {
        for response in [None, Some(scene_picture(8))] {
            let (host, pane) = session(&[]);
            host.canonical_images.set(true);
            let picture = scene_picture(7);
            *host.link.image.borrow_mut() = response.clone();
            pane.scene_image(2, picture.hash(), 0, 1);
            host.spawner.run_all();
            let requests = host.link.asked.borrow().len();
            for _ in 0..10 { assert!(pane.scene_image(2, picture.hash(), 0, 1).is_none()); }
            assert!(host.spawner.0.borrow().is_empty());
            assert_eq!(host.link.asked.borrow().len(), requests);
            if let Some(newer) = &response {
                assert!(pane.images.lock().get(&(host.image_domain(), newer.hash())).is_some());
            }
            // Not for good: a failure is asked about again after a while.
            host.clock.0.set(SCENE_RETRY_AFTER.as_micros() as u64);
            pane.scene_image(2, picture.hash(), 0, 1);
            assert_eq!(host.spawner.0.borrow().len(), 1);
            host.spawner.run_all();
            *host.link.image.borrow_mut() = Some(picture.clone());
            pane.scene_image(2, picture.hash(), 0, 2);
            host.spawner.run_all();
            assert!(pane.scene_image(2, picture.hash(), 0, 2).is_some());
            assert_eq!(pane.scene_images.lock().failed.capacity(), 0);
        }
    }

    #[test]
    fn a_refused_fetch_by_id_asks_by_the_cell() {
        let (host, pane) = session(&[]);
        host.canonical_images.set(true);
        host.link.refuse_by_id.set(true);
        let picture = scene_picture(7);
        *host.link.image.borrow_mut() = Some(picture.clone());
        let request = codec::GetImageCell { pane_id: 0, line_idx: 0, cell_idx: 0, data_hash: picture.hash(), data_generation: 0, have_frames: 0 };
        let fetched = spin_on(crate::hydrate::fetch_image(&*host, None, request, Some(2)));
        assert!(fetched.is_some_and(|data| data.hash() == picture.hash()));
        let asked = host.link.asked.borrow();
        assert_eq!(asked[asked.len() - 2..], ["GetKittyImage", "GetImageCell"]);
        drop(asked);
        drop(pane);
    }

    #[test]
    fn old_scene_work_cannot_start_or_install_pixels_after_reconnect_or_pane_drop() {
        for start in [false, true] {
            for dropped in [false, true] {
                let (host, pane) = session(&[]);
                host.canonical_images.set(true);
                let picture = scene_picture(7);
                *host.link.image.borrow_mut() = Some(picture.clone());
                let ready = Rc::new(Cell::new(false));
                *host.link.image_reply_ready.borrow_mut() = Some(ready.clone());
                pane.scene_image(2, picture.hash(), 0, 1);
                let mut task = host.spawner.0.borrow_mut().pop().unwrap();
                if start { assert!(task.as_mut().poll(&mut Context::from_waker(Waker::noop())).is_pending()); }
                let images = pane.images.clone();
                let weak = Arc::downgrade(&pane);
                if dropped { drop(pane); assert!(weak.upgrade().is_none()); }
                else { host.image_domain.set(2); }
                ready.set(true);
                spin_on(task);
                assert!(images.lock().get(&(1, picture.hash())).is_none());
                assert!(images.lock().get(&(2, picture.hash())).is_none());
                if !start { assert!(host.link.asked.borrow().is_empty()); }
                if let Some(pane) = weak.upgrade() { assert_eq!(pane.scene_images.lock().pending.capacity(), 0); }
            }
        }
    }

    #[test]
    fn a_late_canonical_image_reply_cannot_rewind_pixels_and_legacy_fetch_behavior_is_unchanged() {
        use termwiz::image::ImageDataType;
        let (host, _) = session(&[]);
        let held = Arc::new(ImageData::with_data(ImageDataType::new_single_frame_content_hashed(1, 1, vec![1; 4])));
        held.set_generation(9);
        let fresh = Arc::new(ImageData::with_data_and_hash(ImageDataType::new_single_frame_content_hashed(1, 1, vec![2; 4]), held.hash()));
        fresh.set_generation(8);
        *host.link.image.borrow_mut() = Some(fresh);
        let request = codec::GetImageCell { pane_id: 9, line_idx: 0, cell_idx: 0, data_hash: held.hash(), data_generation: 8, have_frames: 0 };
        host.canonical_images.set(true);
        let result = spin_on(crate::hydrate::fetch_image(&*host, Some(held.clone()), request.clone(), Some(4))).unwrap();
        assert!(Arc::ptr_eq(&held, &result));
        assert_eq!(held.generation(), 9);
        assert!(matches!(&*held.data(), ImageDataType::Rgba8 { data, .. } if data == &[1; 4]));
        host.canonical_images.set(false);
        spin_on(crate::hydrate::fetch_image(&*host, Some(held.clone()), request, Some(4))).unwrap();
        assert_eq!(held.generation(), 8);
        assert!(matches!(&*held.data(), ImageDataType::Rgba8 { data, .. } if data == &[2; 4]));
    }

    fn dims() -> RenderableDimensions {
        RenderableDimensions {
            cols: 80,
            viewport_rows: 24,
            scrollback_rows: 24,
            physical_top: 0,
            scrollback_top: 0,
            dpi: 96,
            pixel_width: 800,
            pixel_height: 480,
            reverse_video: false,
        }
    }

    fn size() -> TerminalSize {
        TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 480,
            dpi: 96,
        }
    }

    fn session(rows: &[(StableRowIndex, &str)]) -> (Arc<TestHost>, Arc<PaneSession<TestHost>>) {
        session_with_cache(rows, 256)
    }

    fn session_with_cache(
        rows: &[(StableRowIndex, &str)],
        scrollback_lines: usize,
    ) -> (Arc<TestHost>, Arc<PaneSession<TestHost>>) {
        let host = Arc::new(TestHost {
            canonical_images: Cell::new(false),
            image_domain: Cell::new(1),
            clock: TestClock(Cell::new(0)),
            spawner: TestSpawner::default(),
            events: TestEvents::default(),
            link: TestLink::default(),
            config: TestConfig::default(),
        });
        *host.link.rows.borrow_mut() = rows.iter().map(|(r, t)| (*r, t.to_string())).collect();
        let session = PaneSession::new(
            Arc::clone(&host),
            Arc::new(Lock::new(ImageStore::default())),
            SessionConfig {
                scrollback_lines,
                local_echo_threshold_ms: None,
                overlay_lag_indicator: false,
            },
            9,
            Arc::new(AtomicUsize::new(0)),
            1,
            dims(),
            "test",
            false,
        );
        (host, session)
    }

    fn delta(seqno: usize, alt_screen: bool, mouse_grabbed: bool) -> GetPaneRenderChangesResponse {
        GetPaneRenderChangesResponse {
            pane_id: 9,
            mouse_grabbed,
            alt_screen,
            keyboard_encoding: Default::default(),
            cursor_position: Default::default(),
            dimensions: dims(),
            dirty_lines: vec![],
            title: String::new(),
            working_dir: None,
            bonus_lines: Vec::new().into(),
            input_serial: None,
            seqno,
        }
    }

    #[test]
    fn render_updates_reject_unbounded_geometry_and_bound_dirty_ranges() {
        let (host, session) = session(&[]);
        let mut huge = delta(1, false, false);
        huge.dimensions.physical_top = 1;
        huge.dimensions.viewport_rows = usize::MAX;
        session.queue_render_delta(huge);
        host.spawner.run_all();
        assert_eq!(session.state().dimensions, dims());
        let mut dirty = delta(2, false, false);
        dirty.dirty_lines = vec![0..isize::MAX];
        session.queue_render_delta(dirty);
        host.spawner.run_all();
        assert_eq!(session.state().seqno, 2);
        assert!(session.state().lines.len() <= 256);
    }

    #[test]
    fn skipped_image_rows_are_recovered_unless_a_later_push_carries_them() {
        let (host, session) = session(&[(1, "recovered"), (2, "must not fetch")]);
        let picture = Arc::new(ImageData::with_raw_data(vec![1, 2, 3]));
        let image_line = || {
            let mut line = Line::from_text("picture", &CellAttributes::default(), 1, None);
            line.cells_mut()[0]
                .attrs_mut()
                .attach_image(Box::new(ImageCell::new(
                    TextureCoordinate::new_f32(0., 0.),
                    TextureCoordinate::new_f32(1., 1.),
                    Arc::clone(&picture),
                )));
            line
        };
        let mut first = delta(1, false, false);
        first.bonus_lines = vec![(1, image_line()), (2, image_line())].into();
        let mut next = delta(2, false, false);
        next.bonus_lines = vec![(
            2,
            Line::from_text("latest", &CellAttributes::default(), 2, None),
        )]
        .into();
        session.queue_render_delta(first);
        session.queue_render_delta(next);
        host.spawner.run_all();
        assert_eq!(*host.link.asked.borrow(), ["GetLines"]);
        assert_eq!(
            host.link.rows_asked.get(),
            1,
            "only the uncarried image row is fetched"
        );
        let st = session.state();
        assert!(
            matches!(st.lines.peek(&1), Some(LineEntry::Line(line)) if line.as_str() == "recovered")
        );
        assert!(
            matches!(st.lines.peek(&2), Some(LineEntry::Line(line)) if line.as_str() == "latest")
        );
        assert_eq!(st.seqno, 2);
    }

    #[test]
    fn queued_text_updates_keep_sparse_rows_and_apply_the_final_state() {
        let (host, session) = session(&[]);
        for seqno in 1..=100 {
            let mut update = delta(seqno, false, seqno % 2 == 0);
            let row = (seqno % 24) as StableRowIndex;
            update.bonus_lines = vec![(
                row,
                Line::from_text(
                    &format!("update {seqno}"),
                    &CellAttributes::default(),
                    seqno,
                    None,
                ),
            )]
            .into();
            update.title = format!("title {seqno}");
            session.queue_render_delta(update);
        }
        host.spawner.run_all();
        let st = session.state();
        assert_eq!(st.seqno, 100);
        assert_eq!(st.title, "title 100");
        assert!(st.mouse_grabbed);
        for seqno in 77..=100 {
            let row = (seqno % 24) as StableRowIndex;
            assert!(
                matches!(st.lines.peek(&row), Some(LineEntry::Line(line)) if line.as_str() == format!("update {seqno}"))
            );
        }
        assert!(
            host.link.asked.borrow().is_empty(),
            "text needs no recovery fetch"
        );
        assert_eq!(host.events.outputs.get(), 100);
    }

    #[test]
    fn image_hydration_accepts_animation_tails_and_rejects_bad_pixels() {
        use termwiz::image::ImageDataType;
        let (host, _) = session(&[]);
        let held = Arc::new(ImageData::with_data(
            ImageDataType::new_single_frame_content_hashed(1, 1, vec![1; 4]),
        ));
        let first_hash = match &*held.data() {
            ImageDataType::Rgba8 { hash, .. } => *hash,
            _ => unreachable!(),
        };
        let request = codec::GetImageCell {
            pane_id: 9, line_idx: 0, cell_idx: 0, data_hash: held.hash(),
            data_generation: 7, have_frames: 1,
        };
        for (from, frames) in [(1, vec![vec![2; 4]]), (2, vec![])] {
            *host.link.image.borrow_mut() = Some(Arc::new(ImageData::with_data_and_hash(
                ImageDataType::AnimRgba8 {
                    width: 1, height: 1,
                    durations: vec![std::time::Duration::from_millis(40); 2],
                    frames, hashes: vec![first_hash, [2; 32]],
                }, held.hash(),
            )));
            host.link.image_frames_from.set(from);
            let fetched = spin_on(crate::hydrate::fetch_image(&*host, Some(held.clone()), request.clone(), None)).unwrap();
            assert!(Arc::ptr_eq(&held, &fetched));
            assert_eq!(crate::images::frame_count(&held.data()), 2);
        }
        let bad = Arc::new(ImageData::with_data_and_hash(
            ImageDataType::Rgba8 { width: 512, height: 511, data: vec![0; 4], hash: [0; 32] },
            held.hash(),
        ));
        *host.link.image.borrow_mut() = Some(bad);
        host.link.image_frames_from.set(0);
        assert!(spin_on(crate::hydrate::fetch_image(&*host, Some(held.clone()), request, None)).is_none());
        assert!(held.data().is_well_formed());
    }

    /// R1: `prime_frontend_geometry` fetches through the same lock it
    /// holds. It must neither deadlock nor report ready before the rows
    /// arrived, and report ready once they have.
    #[test]
    fn priming_an_empty_cache_takes_one_lock_and_settles_after_the_fetch() {
        let (host, session) = session(&(0..24).map(|r| (r, "row")).collect::<Vec<_>>());
        // A server seqno first: rows stamped with SEQ_ZERO always read as
        // changed, exactly as they would before the first push.
        session.queue_render_delta(delta(1, false, false));
        host.spawner.run_all();
        assert!(
            !session.prime_frontend_geometry(size()),
            "nothing fetched yet"
        );
        assert!(!host.spawner.0.borrow().is_empty(), "a fetch was scheduled");
        host.spawner.run_all();
        assert!(host.link.asked.borrow().contains(&"GetLines"));
        assert!(
            session.prime_frontend_geometry(size()),
            "every visible row is a Line now"
        );
    }

    /// R5: a poll completing after the pane is gone still clears the
    /// latch it shares with the task, generation-checked.
    #[test]
    fn a_poll_answered_after_the_pane_is_gone_still_clears_its_latch() {
        let (host, session) = session(&[]);
        // past the poll interval, so changed_since polls; the task is queued, not run
        host.clock.0.set(1_000_000);
        let _ = session.changed_since(0..1, SEQ_ZERO);
        let latch = Arc::clone(&session.state().poll_in_flight);
        assert_ne!(latch.load(Ordering::SeqCst), 0, "a poll is in flight");
        drop(session);
        host.spawner.run_all();
        assert_eq!(
            latch.load(Ordering::SeqCst),
            0,
            "cleared before the upgrade failed"
        );
    }

    /// R6: fetches discarded because the epoch moved notify once per
    /// epoch, not once per fetch and not never.
    #[test]
    fn discarded_fetches_notify_once_per_epoch() {
        let (host, session) = session(&[(0, "a"), (1, "b")]);
        let mut size = size();
        for epoch in 1..=2u64 {
            // two fetches queued for this epoch, neither run yet
            let _ = session.get_lines(0..1);
            let _ = session.get_lines(1..2);
            // the epoch moves under them
            size.cols += 1;
            assert!(session.apply_local_resize(size));
            assert_eq!(session.state().line_cache_epoch, epoch);
            host.spawner.run_all();
            assert_eq!(
                host.events.outputs.get(),
                epoch as u32,
                "exactly one repaint per epoch of discards"
            );
        }
    }

    fn image_session() -> (Arc<TestHost>, Arc<PaneSession<TestHost>>) {
        let (host, session) = session(&[(0, "image")]);
        *host.link.image.borrow_mut() = Some(Arc::new(ImageData::with_raw_data(vec![1, 2, 3])));
        (host, session)
    }

    fn pending_image_line_fetch(
        host: &TestHost,
        session: &Arc<PaneSession<TestHost>>,
    ) -> (DetachedFuture, Rc<Cell<bool>>) {
        let ready = Rc::new(Cell::new(false));
        *host.link.line_reply_ready.borrow_mut() = Some(Rc::clone(&ready));
        session.get_lines(0..1);
        let mut task = host
            .spawner
            .0
            .borrow_mut()
            .pop()
            .expect("scheduled line fetch");
        assert!(task
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending());
        assert_eq!(*host.link.asked.borrow(), ["GetLines"]);
        (task, ready)
    }

    #[test]
    fn obsolete_image_line_reply_skips_images_and_notifies_once() {
        let (host, session) = image_session();
        let (task, ready) = pending_image_line_fetch(&host, &session);
        let mut resized = size();
        resized.cols += 1;
        session.apply_local_resize(resized);
        ready.set(true);
        spin_on(task);
        assert_eq!(*host.link.asked.borrow(), ["GetLines"]);
        assert_eq!(session.images.lock().footprint(), (0, 0));
        assert_eq!(
            host.events.outputs.get(),
            1,
            "discard still prompts a repaint"
        );

        // That repaint must be able to fetch and attach the current image.
        session.get_lines(0..1);
        host.spawner.run_all();
        assert_eq!(
            *host.link.asked.borrow(),
            ["GetLines", "GetLines", "GetImageCell"]
        );
        assert!(
            matches!(session.state().lines.peek(&0), Some(LineEntry::Line(line))
            if line.visible_cells().next().unwrap().attrs().images().is_some())
        );
    }

    #[test]
    fn image_line_reply_after_pane_drop_skips_images() {
        let (host, session) = image_session();
        let images = Arc::clone(&session.images);
        let (task, ready) = pending_image_line_fetch(&host, &session);
        let weak = Arc::downgrade(&session);
        drop(session);
        assert!(
            weak.upgrade().is_none(),
            "the fetch does not retain the pane"
        );
        ready.set(true);
        spin_on(task);
        assert_eq!(*host.link.asked.borrow(), ["GetLines"]);
        assert_eq!(images.lock().footprint(), (0, 0));
        assert_eq!(host.events.outputs.get(), 0);
    }

    #[test]
    fn image_line_reply_after_pane_death_still_applies_current_lines() {
        let (host, session) = image_session();
        let (task, ready) = pending_image_line_fetch(&host, &session);
        assert!(matches!(
            session.state().lines.peek(&0),
            Some(LineEntry::Fetching(_))
        ));
        session.set_dead(true);
        ready.set(true);
        spin_on(task);
        assert_eq!(*host.link.asked.borrow(), ["GetLines", "GetImageCell"]);
        assert!(matches!(
            session.state().lines.peek(&0),
            Some(LineEntry::Line(line)) if line.as_str().trim_end() == "image"
                && line.visible_cells().next().unwrap().attrs().images().is_some()
        ));
        assert_eq!(host.events.outputs.get(), 1);
    }

    #[test]
    fn dead_pane_still_rejects_an_obsolete_line_reply() {
        let (host, session) = image_session();
        let (task, ready) = pending_image_line_fetch(&host, &session);
        let mut resized = size();
        resized.cols += 1;
        session.apply_local_resize(resized);
        session.set_dead(true);
        ready.set(true);
        spin_on(task);
        assert_eq!(*host.link.asked.borrow(), ["GetLines"]);
        assert!(session.state().lines.peek(&0).is_none());
        assert_eq!(session.images.lock().footprint(), (0, 0));
        assert_eq!(host.events.outputs.get(), 1);
    }

    #[test]
    fn image_line_reply_is_rechecked_after_hydration() {
        let (host, session) = image_session();
        let image_ready = Rc::new(Cell::new(false));
        *host.link.image_reply_ready.borrow_mut() = Some(Rc::clone(&image_ready));
        let (mut task, ready) = pending_image_line_fetch(&host, &session);
        ready.set(true);
        assert!(task
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending());
        assert_eq!(*host.link.asked.borrow(), ["GetLines", "GetImageCell"]);
        let mut resized = size();
        resized.cols += 1;
        session.apply_local_resize(resized);
        image_ready.set(true);
        spin_on(task);
        assert!(!matches!(
            session.state().lines.peek(&0),
            Some(LineEntry::Line(_))
        ));
        assert_eq!(host.events.outputs.get(), 1);
    }

    #[test]
    fn an_image_reply_from_before_reconnect_cannot_repopulate_the_store() {
        let (host, session) = image_session();
        let image_ready = Rc::new(Cell::new(false));
        *host.link.image_reply_ready.borrow_mut() = Some(Rc::clone(&image_ready));
        let (mut task, ready) = pending_image_line_fetch(&host, &session);
        ready.set(true);
        assert!(task.as_mut().poll(&mut Context::from_waker(Waker::noop())).is_pending());
        assert_eq!(*host.link.asked.borrow(), ["GetLines", "GetImageCell"]);
        host.image_domain.set(2);
        session.images.lock().clear();
        image_ready.set(true);
        spin_on(task);
        assert_eq!(session.images.lock().footprint(), (0, 0));
        assert!(!matches!(session.state().lines.peek(&0), Some(LineEntry::Line(_))));
    }

    #[test]
    fn image_domain_change_during_a_push_recovers_rows_and_keeps_the_drain_running() {
        let (host, session) = image_session();
        let ready = Rc::new(Cell::new(false));
        *host.link.image_reply_ready.borrow_mut() = Some(Rc::clone(&ready));
        let mut line = Line::from_text("image", &CellAttributes::default(), 1, None);
        line.cells_mut()[0].attrs_mut().attach_image(Box::new(ImageCell::new(
            TextureCoordinate::new_f32(0., 0.),
            TextureCoordinate::new_f32(1., 1.),
            host.link.image.borrow().as_ref().unwrap().clone(),
        )));
        let mut update = delta(1, false, false);
        update.bonus_lines = vec![(0, line)].into();
        session.queue_render_delta(update);
        let mut task = host.spawner.0.borrow_mut().pop().unwrap();
        assert!(task.as_mut().poll(&mut Context::from_waker(Waker::noop())).is_pending());
        host.image_domain.set(2);
        ready.set(true);
        spin_on(task);
        assert_eq!(session.current_seqno(), 1);
        assert_eq!(session.images.lock().footprint(), (0, 0));
        host.spawner.run_all();
        assert_eq!(*host.link.asked.borrow(), ["GetImageCell", "GetLines", "GetImageCell"]);
        assert!(matches!(session.state().lines.peek(&0), Some(LineEntry::Line(line))
            if line.visible_cells().next().unwrap().attrs().images().is_some()));
        session.queue_render_delta(delta(2, false, true));
        host.spawner.run_all();
        assert_eq!(session.current_seqno(), 2);
        assert!(session.is_mouse_grabbed());
        session.get_lines(0..1);
        assert!(!session.render_looks_stalled_in(0..1));
        assert!(host.spawner.0.borrow().is_empty(), "no repeated fetch after recovery");
    }

    /// S12: the screen switch, the mouse grab and the cache flush land in
    /// the same locked update.
    #[test]
    fn a_delta_switching_screens_flushes_the_cache_in_the_same_update() {
        let (host, session) = session(&[(0, "a")]);
        let _ = session.get_lines(0..1);
        host.spawner.run_all();
        let epoch_before = session.state().line_cache_epoch;
        session.queue_render_delta(delta(3, true, true));
        host.spawner.run_all();
        assert!(session.is_alt_screen());
        assert!(session.is_mouse_grabbed());
        assert_eq!(session.current_seqno(), 3);
        assert!(
            session.state().line_cache_epoch > epoch_before,
            "the other screen's rows are stale"
        );
    }

    /// A push carrying a width this surface did not have keeps the cached
    /// rows drawable: clearing them blanked the whole pane on every push
    /// while the server and the frontend disagreed about the width.
    #[test]
    fn a_push_with_another_width_keeps_the_rows_drawable() {
        let (host, session) = session(&[(0, "kept")]);
        session.queue_render_delta(delta(1, false, false));
        host.spawner.run_all();
        let _ = session.get_lines(0..1);
        host.spawner.run_all();
        session.apply_local_resize(TerminalSize {
            cols: 60,
            pixel_width: 600,
            ..size()
        });

        session.queue_render_delta(delta(2, false, false));
        host.spawner.run_all();

        let st = session.state();
        assert_eq!(st.dimensions.cols, 80, "the server's width is shown");
        match st.lines.peek(&0) {
            Some(LineEntry::Stale(line)) => {
                assert_eq!(line.as_str().trim_end(), "kept");
                assert_eq!(line.len(), 80, "normalized to the new width");
            }
            other => panic!(
                "row 0 must stay drawable, found {:?}",
                other.map(|entry| entry.kind())
            ),
        }
    }

    fn rows_requested(host: &TestHost) -> usize {
        host.link.rows_asked.get()
    }

    /// Rows just outside a painted range are fetched with it, tagged so a
    /// later paint does not ask twice, and only within the scrollback.
    #[test]
    fn a_paint_fetches_the_rows_a_scroll_would_reveal_next() {
        let (host, session) = session(&(-24..24).map(|r| (r, "row")).collect::<Vec<_>>());
        host.config.lookahead.set(1);
        // dims(): 24 viewport rows, physical_top 0, scrollback_top 0.
        session.queue_render_delta(delta(1, false, false));
        host.spawner.run_all();
        host.link.rows_asked.set(0);

        let _ = session.get_lines(10..14);
        host.spawner.run_all();
        // 4 painted + up to 24 on each side, clamped to rows 0..24: 0..10
        // below and 14..24 above, none beyond the scrollback top.
        assert_eq!(rows_requested(&host), 24);
        let st = session.state();
        assert!(matches!(st.lines.peek(&0), Some(LineEntry::Line(_))));
        assert!(matches!(st.lines.peek(&23), Some(LineEntry::Line(_))));
        assert!(st.lines.peek(&-1).is_none(), "nothing above the scrollback top");
        drop(st);

        host.link.rows_asked.set(0);
        let _ = session.get_lines(10..14);
        host.spawner.run_all();
        assert_eq!(rows_requested(&host), 0, "everything around is cached now");
    }

    /// The whole scrollback is fetched once per cache epoch when the host
    /// asks for it, and again after the epoch moves.
    #[test]
    fn a_warming_host_fetches_the_scrollback_once_per_epoch() {
        let (host, session) = session(&(0..24).map(|r| (r, "row")).collect::<Vec<_>>());
        host.config.warm.set(true);
        session.queue_render_delta(delta(1, false, false));
        host.spawner.run_all();
        host.link.rows_asked.set(0);

        let _ = session.get_lines(20..24);
        host.spawner.run_all();
        assert_eq!(rows_requested(&host), 24, "the painted rows plus the rest");
        let _ = session.get_lines(0..4);
        host.spawner.run_all();
        assert_eq!(rows_requested(&host), 24, "already warm");

        let mut size = size();
        size.cols += 1;
        assert!(session.apply_local_resize(size));
        let _ = session.get_lines(20..24);
        host.spawner.run_all();
        assert!(rows_requested(&host) > 24, "a new epoch warms again");
    }

    #[test]
    fn automatic_warming_is_bounded_without_limiting_requested_rows() {
        let rows: Vec<_> = (96_500..100_000).map(|r| (r, "row")).collect();
        let (host, session) = session_with_cache(&rows, 100_000);
        host.config.warm.set(true);
        let mut tall = delta(1, false, false);
        tall.dimensions.physical_top = 99_976;
        tall.dimensions.scrollback_rows = 100_000;
        session.queue_render_delta(tall);
        host.spawner.run_all();

        let _ = session.get_lines(99_976..100_000);
        host.spawner.run_all();
        assert_eq!(rows_requested(&host), 3500, "only the recent region is warmed");
        assert_eq!(
            session.state().lines.cap().get(),
            100_000,
            "history capacity is unchanged"
        );
        let _ = session.get_lines(99_976..100_000);
        host.spawner.run_all();
        assert_eq!(
            rows_requested(&host),
            3500,
            "another paint does not warm older rows"
        );

        host.link.rows_asked.set(0);
        let _ = session.get_lines(0..4000);
        host.spawner.run_all();
        assert_eq!(
            rows_requested(&host),
            4000,
            "explicit requests are not capped by warming"
        );
    }

    /// Output that scrolled past between two pushes was never sent; once
    /// the physical top has moved a viewport, the scrollback is warmed
    /// again for the rows that appeared.
    #[test]
    fn scrollback_that_grew_a_viewport_is_warmed_again() {
        let (host, session) = session(&(0..72).map(|r| (r, "row")).collect::<Vec<_>>());
        host.config.warm.set(true);
        session.queue_render_delta(delta(1, false, false));
        host.spawner.run_all();
        let _ = session.get_lines(0..24);
        host.spawner.run_all();
        host.link.rows_asked.set(0);

        let mut grown = delta(2, false, false);
        grown.dimensions.physical_top = 48;
        grown.dimensions.scrollback_rows = 72;
        session.queue_render_delta(grown);
        host.spawner.run_all();
        let _ = session.get_lines(48..72);
        host.spawner.run_all();
        // The painted 48..72 plus the never-pushed 24..48; 0..24 is cached.
        assert_eq!(rows_requested(&host), 48);
    }

    /// Speculation never pushes a painted row out of the cache: with a
    /// cache barely larger than the viewport, the lookahead is cut to what
    /// fits beside the painted rows, which all come back as lines.
    #[test]
    fn speculative_fetches_leave_the_painted_rows_in_the_cache() {
        // scrollback_lines below the floor: the cache holds 128 rows.
        let (host, session) =
            session_with_cache(&(0..200).map(|r| (r, "row")).collect::<Vec<_>>(), 100);
        host.config.lookahead.set(3);
        let mut tall = delta(1, false, false);
        tall.dimensions.physical_top = 176;
        tall.dimensions.scrollback_rows = 200;
        session.queue_render_delta(tall);
        host.spawner.run_all();
        host.link.rows_asked.set(0);

        let _ = session.get_lines(176..200);
        host.spawner.run_all();
        let st = session.state();
        for row in 176..200 {
            assert!(
                matches!(st.lines.peek(&row), Some(LineEntry::Line(_))),
                "painted row {row} survived the speculation"
            );
        }
        assert!(st.lines.len() <= 128);
        // 24 painted plus at most the 104 the cache has room for.
        assert!(rows_requested(&host) <= 128, "{}", rows_requested(&host));
        assert!(rows_requested(&host) > 24, "some lookahead happened");
    }

    /// A band wider than the cache settles: the second paint of the same
    /// range finds the bounded region cached and asks for nothing, rather
    /// than fetching a fresh slice that evicts the last one every frame.
    #[test]
    fn speculation_wider_than_the_cache_settles_on_the_second_paint() {
        let (host, session) =
            session_with_cache(&(0..168).map(|r| (r, "row")).collect::<Vec<_>>(), 100);
        host.config.lookahead.set(3);
        let mut tall = delta(1, false, false);
        tall.dimensions.physical_top = 128;
        tall.dimensions.scrollback_rows = 168;
        session.queue_render_delta(tall);
        host.spawner.run_all();
        host.link.rows_asked.set(0);

        let _ = session.get_lines(64..104);
        host.spawner.run_all();
        let first = rows_requested(&host);
        assert!(first > 40, "painted rows plus a bounded lookahead");

        for _ in 0..3 {
            let _ = session.get_lines(64..104);
            host.spawner.run_all();
        }
        assert_eq!(rows_requested(&host), first, "nothing more on repeated paints");
    }

    /// A speculative batch that the fetch rate limit refuses leaves its
    /// rows untagged, so a later paint can ask for them once the limit
    /// allows.
    #[test]
    fn speculative_fetches_respect_the_fetch_rate_limit() {
        let (host, session) = session(&(0..72).map(|r| (r, "row")).collect::<Vec<_>>());
        host.config.lookahead.set(1);
        host.config.rate.set(1);
        let mut tall = delta(1, false, false);
        tall.dimensions.physical_top = 48;
        tall.dimensions.scrollback_rows = 72;
        session.queue_render_delta(tall);
        host.spawner.run_all();

        // One batch is admitted: rows 36..60 and 64..72 around the paint.
        let _ = session.get_lines(60..64);
        host.spawner.run_all();
        assert!(matches!(
            session.state().lines.peek(&40),
            Some(LineEntry::Line(_))
        ));

        // The next, in the same second, is refused: nothing is tagged.
        let _ = session.get_lines(30..34);
        host.spawner.run_all();
        assert!(session.state().lines.peek(&10).is_none(), "left for later");

        // A second later the limit allows it again.
        host.clock.0.set(1_000_000);
        let _ = session.get_lines(30..34);
        host.spawner.run_all();
        assert!(matches!(
            session.state().lines.peek(&10),
            Some(LineEntry::Line(_))
        ));
    }

    /// The input path: queue, drain through the link in order, settle.
    #[test]
    fn queued_input_leaves_in_order_and_settles() {
        let (host, session) = session(&[]);
        assert!(
            session.write_bytes(b"ni").unwrap(),
            "the first input starts the drain"
        );
        assert!(
            !session.write_bytes(b"hao").unwrap(),
            "the second rides along"
        );
        assert!(!session
            .key_down(
                InputSerial::from_millis(1),
                KeyCode::Enter,
                KeyModifiers::NONE
            )
            .unwrap());
        spin_on(Arc::clone(&session).drain_inputs());
        assert_eq!(*host.link.asked.borrow(), ["WriteToPane", "SendKeyDown"]);
        assert!(
            session.write_bytes(b"x").unwrap(),
            "drained: the next input starts a new drain"
        );
    }
}
