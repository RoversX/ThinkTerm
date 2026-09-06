//! One mirrored pane: its state behind one lock, the pushes it applies in
//! order, the rows it fetches and the polls it makes through the host.
//! Detached tasks hold a `Weak` to it; a pane that is gone by the time an
//! answer arrives is simply not updated.
use crate::clock::Clock;
use crate::delta_queue::{RenderDeltaQueue, RowsCarried};
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

pub struct PaneSession<H: SessionHost> {
    host: Arc<H>,
    images: Arc<Lock<ImageStore>>,
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

    pub fn remote_pane_id(&self) -> PaneId {
        self.remote_pane_id
    }

    pub fn host_pane_id(&self) -> HostPaneId {
        self.host_pane_id
    }

    // ---- what the renderer reads --------------------------------------

    pub fn dimensions(&self) -> RenderableDimensions {
        self.state().dimensions
    }

    pub fn cursor_position(&self) -> StableCursorPosition {
        self.state().cursor_position
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
        (lines.start, result)
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
        let mut st = self.state();
        if !crate::decide::render_watchdog_should_run(st.dead) {
            return false;
        }
        let top = viewport_top.unwrap_or(st.dimensions.physical_top);
        let range = top..top.saturating_add(st.dimensions.viewport_rows as StableRowIndex);
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
            let Some((delta, carried)) = me.render_deltas.lock().take() else {
                return;
            };
            // A push with a newer one already behind it is applied without
            // asking for pictures: the newer push names the current ones, and
            // rows whose pictures are missing keep showing the previous frame.
            let started = me.now();
            let fetched_pictures = carried.is_none();
            me.apply_render_delta(delta, carried).await;
            log::debug!(
                "render push for pane {} applied in {:?} (fetched pictures: {})",
                me.host_pane_id,
                me.now().saturating_duration_since(started),
                fetched_pictures
            );
        }
    }

    /// Apply one push. With `carried` given, newer pushes wait behind this
    /// one and it is applied without fetching pictures; a row it then has
    /// to leave out is marked dirty unless one of those pushes brings it,
    /// since the server sent it as a bonus row and nothing else would ever
    /// fetch it again.
    async fn apply_render_delta(
        self: &Arc<Self>,
        mut delta: GetPaneRenderChangesResponse,
        carried: Option<RowsCarried>,
    ) {
        let bonus_lines = std::mem::take(&mut delta.bonus_lines);
        let (bonus_lines, left_out) = hydrate_lines(
            &*self.host,
            &self.images,
            delta.pane_id,
            bonus_lines,
            carried.is_none(),
        )
        .await;
        if let Some(carried) = carried {
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
        log::trace!(
            "apply_changes_to_surface local={} remote={}",
            self.host_pane_id,
            self.remote_pane_id
        );
        let now = self.now();
        st.poll_interval = BASE_POLL_INTERVAL;
        st.last_recv_time = now;

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

        let mut dirty = RangeSet::new();
        for r in delta.dirty_lines {
            dirty.add_range(r.clone());
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
        if let Some(preserve_lines) = invalidate {
            // During a preview, retain old rows while marking them stale. This
            // prevents a blank flash as a full-screen application redraws.
            st.invalidate_line_cache(preserve_lines);
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
                    log::trace!("make {} stale bcos not fetchable", stable_row);
                    st.make_stale(stable_row);
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
                    let (lines, _) =
                        hydrate_lines(&*host, &images, remote_pane_id, result.lines, true).await;
                    Ok(lines)
                }
                Err(err) => Err(err),
            };
            Self::apply_lines(&weak, result, to_fetch, fetch_token);
        }));
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
            let mut st = me.state();

            if !fetch_token_is_current(fetch_token, st.line_cache_epoch) {
                log::trace!(
                    "discarding line fetch for pane {} from epoch {} because current epoch is {}",
                    me.host_pane_id,
                    fetch_token.epoch,
                    st.line_cache_epoch
                );
                // The rows this fetch covered were re-tagged Stale when the
                // epoch moved, and only a paint re-fetches Stale rows. A
                // paint is only scheduled by PaneOutput, so returning
                // without notifying can leave the pane frozen until the
                // user interacts with it. Once per epoch is enough: a live
                // resize discards several in-flight fetches per frame.
                let already_notified = st.epoch_discard_notified == st.line_cache_epoch;
                st.epoch_discard_notified = st.line_cache_epoch;
                drop(st);
                if !already_notified {
                    me.host.events().pane_output(me.host_pane_id);
                }
                return;
            }

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

impl<H: SessionHost> Drop for DrainingDeltas<H> {
    fn drop(&mut self) {
        if let Some(pane) = self.0.upgrade() {
            pane.render_deltas.lock().end_drain();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::{DetachedFuture, HostConfig, LinkError};
    use crate::input::{LocalFuture, PaneLink};
    use codec::{GetLinesResponse, UnitResponse};
    use std::cell::{Cell, RefCell};
    use std::future::Future;
    use std::pin::Pin;
    use std::task::{Context, Poll, Waker};
    use termwiz::cell::CellAttributes;
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
        fn pane_removed(&self, _pane: HostPaneId) {}
        fn pane_focused(&self, _pane: HostPaneId) {}
        fn input_recorded(&self) {}
    }

    struct TestConfig;
    impl HostConfig for TestConfig {
        fn hyperlink_rules(&self) -> Arc<Vec<termwiz::hyperlink::Rule>> {
            Arc::new(Vec::new())
        }
        fn fetch_rate_per_second(&self) -> u32 {
            100
        }
    }

    /// Answers GetLines from a table of rows, everything else with
    /// UnitResponse.
    #[derive(Default)]
    struct TestLink {
        rows: RefCell<Vec<(StableRowIndex, String)>>,
        asked: RefCell<Vec<&'static str>>,
    }
    impl PduLink for TestLink {
        type Request = LocalFuture<Result<Pdu, LinkError>>;
        fn request(&self, pdu: Pdu) -> Self::Request {
            self.asked.borrow_mut().push(pdu.pdu_name());
            let answer = match pdu {
                Pdu::GetLines(req) => {
                    let table = self.rows.borrow();
                    let lines: Vec<(StableRowIndex, Line)> = req
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
                    Pdu::GetLinesResponse(GetLinesResponse {
                        pane_id: req.pane_id,
                        lines: lines.into(),
                    })
                }
                Pdu::GetPaneRenderChanges(req) => Pdu::LivenessResponse(codec::LivenessResponse {
                    pane_id: req.pane_id,
                    is_alive: true,
                }),
                _ => Pdu::UnitResponse(UnitResponse {}),
            };
            Box::pin(async move { Ok(answer) })
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
        fn image_domain(&self) -> crate::host::ImageDomainKey {
            1
        }
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
        let host = Arc::new(TestHost {
            clock: TestClock(Cell::new(0)),
            spawner: TestSpawner::default(),
            events: TestEvents::default(),
            link: TestLink::default(),
            config: TestConfig,
        });
        *host.link.rows.borrow_mut() = rows.iter().map(|(r, t)| (*r, t.to_string())).collect();
        let session = PaneSession::new(
            Arc::clone(&host),
            Arc::new(Lock::new(ImageStore::default())),
            SessionConfig {
                scrollback_lines: 256,
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
