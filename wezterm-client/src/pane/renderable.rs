use crate::domain::ClientInner;
use crate::pane::clientpane::ClientPane;
use anyhow::anyhow;
use codec::*;
use config::{configuration, ConfigHandle};
use lru::LruCache;
use mux::pane::PaneId;
use mux::renderable::{RenderableDimensions, StableCursorPosition};
use mux::Mux;
use promise::BrokenPromise;
use rangeset::*;
use ratelim::RateLimiter;
use std::cell::RefCell;
use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::ops::Range;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use termwiz::cell::{Cell, CellAttributes, Underline};
use termwiz::color::AnsiColor;
use termwiz::image::{ImageCell, ImageData};
use termwiz::surface::{SequenceNo, SEQ_ZERO};
use url::Url;
use wezterm_term::{KeyCode, KeyModifiers, Line, StableRowIndex};

// 30s, not shorter: poll is a fallback, and every poll costs the server a
// full-scrollback get_changed_since scan under the terminal lock, per
// painted mirror pane. Stall recovery is the watchdog's job — it resets
// the interval to BASE the moment a pane looks stuck.
const MAX_POLL_INTERVAL: Duration = Duration::from_secs(30);
const BASE_POLL_INTERVAL: Duration = Duration::from_millis(20);
const FETCH_STALL_BASE: Duration = Duration::from_secs(30);
const POLL_STALL_BASE: Duration = Duration::from_secs(15);

fn stall_timeout(base: Duration, attempt: u32) -> Duration {
    let multiplier = 1u32.checked_shl(attempt).unwrap_or(u32::MAX);
    base.checked_mul(multiplier).unwrap_or(Duration::MAX)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FetchToken {
    epoch: u64,
    started_at: Instant,
    stall_attempt: u32,
}

impl FetchToken {
    fn new(epoch: u64, started_at: Instant) -> Self {
        Self {
            epoch,
            started_at,
            stall_attempt: 0,
        }
    }

    fn retry(self, started_at: Instant) -> Self {
        Self {
            epoch: self.epoch,
            started_at,
            stall_attempt: self.stall_attempt.saturating_add(1),
        }
    }
}

fn fetch_should_be_retried(token: FetchToken, now: Instant) -> bool {
    now.saturating_duration_since(token.started_at)
        > stall_timeout(FETCH_STALL_BASE, token.stall_attempt)
}

fn poll_should_be_retried(started_at: Instant, attempt: u32, now: Instant) -> bool {
    now.saturating_duration_since(started_at) > stall_timeout(POLL_STALL_BASE, attempt)
}

fn claim_poll_completion(in_flight: &AtomicU64, generation: u64) -> bool {
    in_flight
        .compare_exchange(generation, 0, Ordering::SeqCst, Ordering::SeqCst)
        .is_ok()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FrontendPreviewGeometry {
    epoch: u64,
    size: wezterm_term::TerminalSize,
    policy: FrontendPreviewPolicy,
}

/// How an optimistic frontend geometry should treat rows that were fetched for
/// the preceding grid.  Takeover previews are hidden by an opaque frontend
/// surface, while a live divider resize is visible and must never expose a row
/// whose cell storage still has the old width.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FrontendPreviewPolicy {
    PreserveRows,
    LiveResize,
}

#[derive(Debug)]
enum LineEntry {
    // Up to date wrt. server and has been rendered at least once
    Line(Line),
    // Currently being downloaded from the server
    Fetching(FetchToken),
    // We have a version of the line locally and are treating it
    // as needing rendering because we are also in the process of
    // downloading a newer version from the server
    LineAndFetching(Line, FetchToken),
    // We have a local copy but it is stale and will need to be
    // fetched again
    Stale(Line),
}

#[derive(Debug)]
struct FetchRetryBatch {
    token: FetchToken,
    rows: RangeSet<StableRowIndex>,
}

#[derive(Debug, Default)]
struct LineWatchdogDecision {
    repaint: bool,
    retries: Vec<FetchRetryBatch>,
}

impl LineWatchdogDecision {
    fn retry(&mut self, row: StableRowIndex, token: FetchToken) {
        self.repaint = true;
        if let Some(batch) = self.retries.iter_mut().find(|batch| batch.token == token) {
            batch.rows.add(row);
            return;
        }
        let mut rows = RangeSet::new();
        rows.add(row);
        self.retries.push(FetchRetryBatch { token, rows });
    }
}

fn line_watchdog_decision(
    lines: &mut LruCache<StableRowIndex, LineEntry>,
    range: Range<StableRowIndex>,
    now: Instant,
) -> LineWatchdogDecision {
    let mut decision = LineWatchdogDecision::default();
    for row in range {
        let entry = match lines.pop(&row) {
            Some(LineEntry::Stale(line)) => {
                decision.repaint = true;
                LineEntry::Stale(line)
            }
            Some(LineEntry::Fetching(token)) => {
                if fetch_should_be_retried(token, now) {
                    let replacement = token.retry(now);
                    decision.retry(row, replacement);
                    LineEntry::Fetching(replacement)
                } else {
                    LineEntry::Fetching(token)
                }
            }
            Some(LineEntry::LineAndFetching(line, token)) => {
                if fetch_should_be_retried(token, now) {
                    let replacement = token.retry(now);
                    decision.retry(row, replacement);
                    LineEntry::LineAndFetching(line, replacement)
                } else {
                    LineEntry::LineAndFetching(line, token)
                }
            }
            Some(entry) => entry,
            None => {
                decision.repaint = true;
                continue;
            }
        };
        lines.put(row, entry);
    }
    decision
}

impl LineEntry {
    fn kind(&self) -> (&'static str, Option<FetchToken>) {
        match self {
            Self::Line(_) => ("Line", None),
            Self::Fetching(token) => ("Fetching", Some(*token)),
            Self::LineAndFetching(_, token) => ("LineAndFetching", Some(*token)),
            Self::Stale(_) => ("Stale", None),
        }
    }
}

fn invalidate_line_entries(lines: &mut LruCache<StableRowIndex, LineEntry>, preserve_lines: bool) {
    if !preserve_lines {
        lines.clear();
        return;
    }

    // Keep the original bound: an unbounded rebuild left the cache free to
    // grow with every fetched line for the rest of the pane's life.
    let mut stale = LruCache::new(lines.cap());
    while let Some((stable_row, entry)) = lines.pop_lru() {
        match entry {
            LineEntry::Stale(line)
            | LineEntry::Line(line)
            | LineEntry::LineAndFetching(line, _) => {
                stale.put(stable_row, LineEntry::Stale(line));
            }
            LineEntry::Fetching(_) => {}
        }
    }
    *lines = stale;
}

/// Keep a stable visual fallback while a visible divider is moving, but make
/// every retained row structurally valid for the new grid before it can be
/// painted.  Cropping/padding a prior complete row is preferable to either a
/// blank flash or mixing old-width storage with newly fetched cells.
fn resize_stale_line_entries(lines: &mut LruCache<StableRowIndex, LineEntry>, cols: usize) {
    let mut stale = LruCache::new(lines.cap());
    while let Some((stable_row, entry)) = lines.pop_lru() {
        let line = match entry {
            LineEntry::Stale(line)
            | LineEntry::Line(line)
            | LineEntry::LineAndFetching(line, _) => line,
            LineEntry::Fetching(_) => continue,
        };
        let seqno = line.current_seqno();
        let mut line = line;
        line.resize(cols, seqno);
        line.set_last_cell_was_wrapped(false, seqno);
        stale.put(stable_row, LineEntry::Stale(line));
    }
    *lines = stale;
}

fn render_geometry_changed(current: RenderableDimensions, next: RenderableDimensions) -> bool {
    current.cols != next.cols
        || current.viewport_rows != next.viewport_rows
        || current.pixel_width != next.pixel_width
        || current.pixel_height != next.pixel_height
        || current.dpi != next.dpi
}

fn render_dimensions_match_terminal_size(
    dimensions: RenderableDimensions,
    size: wezterm_term::TerminalSize,
) -> bool {
    dimensions.cols == size.cols
        && dimensions.viewport_rows == size.rows
        && dimensions.pixel_width == size.pixel_width
        && dimensions.pixel_height == size.pixel_height
        && dimensions.dpi == size.dpi
}

fn confirmed_frontend_visible_range(
    dimensions: RenderableDimensions,
    size: wezterm_term::TerminalSize,
) -> Option<Range<StableRowIndex>> {
    if !render_dimensions_match_terminal_size(dimensions, size) {
        return None;
    }
    let top = dimensions.physical_top;
    Some(top..top.saturating_add(size.rows as StableRowIndex))
}

fn release_unreturned_fetches(
    lines: &mut LruCache<StableRowIndex, LineEntry>,
    requested: &RangeSet<StableRowIndex>,
    returned: &RangeSet<StableRowIndex>,
    fetch_token: FetchToken,
) {
    for range in requested.iter() {
        for row in range.clone() {
            if returned.contains(row) {
                continue;
            }
            match lines.pop(&row) {
                Some(LineEntry::Fetching(token)) if token == fetch_token => {}
                Some(LineEntry::LineAndFetching(line, token)) if token == fetch_token => {
                    lines.put(row, LineEntry::Stale(line));
                }
                Some(entry) => {
                    lines.put(row, entry);
                }
                None => {}
            }
        }
    }
}

fn resolve_server_geometry(
    visible: RenderableDimensions,
    previous_server: RenderableDimensions,
    next_server: RenderableDimensions,
    preview_active: bool,
) -> (RenderableDimensions, Option<bool>) {
    if preview_active {
        let invalidation = render_geometry_changed(previous_server, next_server).then_some(true);
        (visible, invalidation)
    } else if render_geometry_changed(visible, next_server) {
        (next_server, Some(visible.cols == next_server.cols))
    } else {
        (next_server, None)
    }
}

fn fetch_token_is_current(token: FetchToken, epoch: u64) -> bool {
    token.epoch == epoch
}

pub struct RenderableInner {
    pub client: Arc<ClientInner>,
    remote_pane_id: PaneId,
    local_pane_id: PaneId,
    last_poll: Instant,
    pub dead: bool,
    /// Generation of the poll currently in flight; 0 means none. Shared
    /// with the poll task so completion can clear it without finding this
    /// pane again (a pane that left the mux must not stay latched), and
    /// generation-checked so a watchdog-superseded poll completing late
    /// can neither clear its successor's latch nor apply a stale answer.
    poll_in_flight: Arc<AtomicU64>,
    poll_gen: u64,
    poll_stall_attempt: u32,
    poll_interval: Duration,

    cursor_position: StableCursorPosition,
    pub dimensions: RenderableDimensions,
    /// The dimensions most recently confirmed by the remote pane.  A GUI
    /// takeover may resize its local surface optimistically; keeping the
    /// server value separate lets it remain masked until a post-resize render
    /// snapshot has actually arrived.
    server_dimensions: RenderableDimensions,
    /// While a native GUI divider is moving, this is the authoritative render
    /// surface. Remote snapshots continue updating `server_dimensions` and
    /// line contents, but cannot bounce the visible surface back to an older
    /// grid between mouse-move frames.
    frontend_preview: Option<FrontendPreviewGeometry>,

    lines: LruCache<StableRowIndex, LineEntry>,
    line_cache_epoch: u64,
    /// The epoch for which a discarded-fetch PaneOutput has already been
    /// emitted; during a live resize every frame bumps the epoch and can
    /// discard several in-flight fetches, and one repaint per epoch is
    /// enough to re-issue them.
    epoch_discard_notified: u64,
    pub title: String,
    pub working_dir: Option<Url>,
    pub seqno: SequenceNo,

    fetch_limiter: RateLimiter,

    last_send_time: Instant,
    pub last_recv_time: Instant,
    last_late_dirty: Instant,
    last_input_rtt: u64,

    pub input_serial: InputSerial,
}

pub struct RenderableState {
    pub inner: RefCell<RenderableInner>,
}

impl RenderableInner {
    pub fn new(
        client: &Arc<ClientInner>,
        remote_pane_id: PaneId,
        local_pane_id: PaneId,
        dimensions: RenderableDimensions,
        title: &str,
        fetch_limiter: RateLimiter,
    ) -> Self {
        let now = Instant::now();

        Self {
            client: Arc::clone(client),
            remote_pane_id,
            local_pane_id,
            last_poll: now,
            dead: false,
            poll_in_flight: Arc::new(AtomicU64::new(0)),
            poll_gen: 0,
            poll_stall_attempt: 0,
            poll_interval: BASE_POLL_INTERVAL,
            cursor_position: StableCursorPosition::default(),
            dimensions,
            server_dimensions: dimensions,
            frontend_preview: None,
            lines: LruCache::new(
                NonZeroUsize::new(configuration().scrollback_lines.max(128)).unwrap(),
            ),
            line_cache_epoch: 0,
            epoch_discard_notified: 0,
            title: title.to_string(),
            working_dir: None,
            fetch_limiter,
            last_send_time: now,
            last_recv_time: now,
            last_late_dirty: now,
            last_input_rtt: 0,
            input_serial: InputSerial::empty(),
            seqno: SEQ_ZERO,
        }
    }

    /// Returns true if we think we should display the laggy connection
    /// indicator.  If we're past our poll interval and more recently
    /// tried to send something than receive something, the UI is worth
    /// showing.
    pub fn is_tardy(&self) -> bool {
        let elapsed = self.last_recv_time.elapsed();
        if elapsed > self.poll_interval.max(Duration::from_secs(3)) {
            self.last_send_time > self.last_recv_time
        } else {
            false
        }
    }

    /// Predictive echo can be noisy when the link is working well,
    /// so we only employ it when it looks like the latency is high.
    fn should_predict(&self) -> bool {
        self.client
            .local_echo_threshold_ms
            .map(|thresh| self.last_input_rtt >= thresh)
            .unwrap_or(false)
    }

    /// Compute a "prediction" and apply it to the line data that we
    /// have available, marking it as dirty so that it gets rendered.
    /// The prediction is basically just local echo.
    /// Open questions:
    /// how do we tell if the intent is to suppress local echo during eg:
    ///  * password prompt?  One option is to look back and see if the line
    ///                      looks like a password prompt.
    ///  * normal mode in vim: letter presses are typically movement or
    ///                        other editor commands
    /// There are bound to be a number of other edge cases that we should
    /// handle.
    fn apply_prediction(&mut self, c: KeyCode, line: &mut Line) {
        let text = line.as_str();
        if text.contains("sword") {
            // This line might be a password prompt.  Don't force
            // on local echo here, as we don't want to reveal content
            // from their password
            return;
        }

        match c {
            KeyCode::Enter => {
                self.cursor_position.x = 0;
                self.cursor_position.y += 1;
            }
            KeyCode::UpArrow => {
                self.cursor_position.y = self.cursor_position.y.saturating_sub(1);
            }
            KeyCode::DownArrow => {
                self.cursor_position.y += 1;
            }
            KeyCode::RightArrow => {
                self.cursor_position.x += 1;
            }
            KeyCode::LeftArrow => {
                self.cursor_position.x = self.cursor_position.x.saturating_sub(1);
            }
            KeyCode::Delete => {
                line.erase_cell(self.cursor_position.x, SEQ_ZERO);
            }
            KeyCode::Backspace => {
                if self.cursor_position.x > 0 {
                    line.erase_cell(self.cursor_position.x - 1, SEQ_ZERO);
                    self.cursor_position.x -= 1;
                }
            }
            KeyCode::Char(c) => {
                let cell = Cell::new(
                    c,
                    CellAttributes::default()
                        .set_underline(Underline::Double)
                        .clone(),
                );

                let width = cell.width();
                line.set_cell(self.cursor_position.x, cell, SEQ_ZERO);
                // Adjust the cursor to reflect the width of this new cell
                self.cursor_position.x += width;
            }
            _ => {}
        }
    }

    /// Based on a keypress, apply a "prediction" of what the terminal
    /// content will look like once we receive the response from the
    /// remote system.  The prediction helps to reduce perceived latency
    /// when a user is typing at any reasonable velocity.
    pub fn predict_from_key_event(&mut self, key: KeyCode, mods: KeyModifiers) {
        if !self.should_predict() {
            return;
        }

        let c = match key {
            KeyCode::LeftArrow
            | KeyCode::RightArrow
            | KeyCode::UpArrow
            | KeyCode::DownArrow
            | KeyCode::Delete
            | KeyCode::Backspace
            | KeyCode::Enter
            | KeyCode::Char(_) => key,
            _ => return,
        };
        if mods != KeyModifiers::NONE && mods != KeyModifiers::SHIFT {
            return;
        }

        let row = self.cursor_position.y;
        match self.lines.pop(&row) {
            // A Stale row stays Stale: promoting it to Line would cancel
            // the pending re-fetch (its seqno can never satisfy
            // changed_since again) and freeze the row on predicted text.
            Some(LineEntry::Stale(mut line)) => {
                self.apply_prediction(c, &mut line);
                self.lines.put(row, LineEntry::Stale(line));
            }
            Some(LineEntry::Line(mut line)) => {
                self.apply_prediction(c, &mut line);
                self.lines.put(row, LineEntry::Line(line));
            }
            Some(LineEntry::LineAndFetching(mut line, instant)) => {
                self.apply_prediction(c, &mut line);
                self.lines
                    .put(row, LineEntry::LineAndFetching(line, instant));
            }
            Some(entry) => {
                self.lines.put(row, entry);
            }
            None => {}
        }
    }

    fn apply_paste_prediction(&mut self, row: usize, text: &str, line: &mut Line) {
        let attrs = CellAttributes::default()
            .set_underline(Underline::Double)
            .clone();

        let text_line = Line::from_text(text, &attrs, SEQ_ZERO, None);

        if row == 0 {
            for cell in text_line.visible_cells() {
                line.set_cell(self.cursor_position.x, cell.as_cell(), SEQ_ZERO);
                self.cursor_position.x += cell.width();
            }
        } else {
            // The pasted line replaces the data for the existing line
            line.resize_and_clear(0, SEQ_ZERO, CellAttributes::default());
            line.append_line(text_line, SEQ_ZERO);
            self.cursor_position.x = line.len();
        }
    }

    pub fn predict_from_paste(&mut self, text: &str) {
        if !self.should_predict() {
            return;
        }

        let text = textwrap::fill(text, self.dimensions.cols);
        let lines: Vec<&str> = text.split("\n").collect();

        for (idx, paste_line) in lines.iter().enumerate() {
            let row = self.cursor_position.y + idx as StableRowIndex;

            match self.lines.pop(&row) {
                // Stale stays Stale; see predict_from_key_event.
                Some(LineEntry::Stale(mut line)) => {
                    self.apply_paste_prediction(idx, paste_line, &mut line);
                    self.lines.put(row, LineEntry::Stale(line));
                }
                Some(LineEntry::Line(mut line)) => {
                    self.apply_paste_prediction(idx, paste_line, &mut line);
                    self.lines.put(row, LineEntry::Line(line));
                }
                Some(LineEntry::LineAndFetching(mut line, instant)) => {
                    self.apply_paste_prediction(idx, paste_line, &mut line);
                    self.lines
                        .put(row, LineEntry::LineAndFetching(line, instant));
                }
                Some(entry) => {
                    self.lines.put(row, entry);
                }
                None => {}
            }
        }
        self.cursor_position.y += lines.len().saturating_sub(1) as StableRowIndex;
    }

    pub fn update_last_send(&mut self) {
        self.last_send_time = Instant::now();
        self.poll_interval = BASE_POLL_INTERVAL;
    }

    pub fn apply_changes_to_surface(
        &mut self,
        delta: GetPaneRenderChangesResponse,
        bonus_lines: Vec<(StableRowIndex, Line)>,
    ) {
        log::trace!(
            "apply_changes_to_surface local={} remote={}",
            self.local_pane_id,
            self.remote_pane_id
        );
        let now = Instant::now();
        self.poll_interval = BASE_POLL_INTERVAL;
        self.last_recv_time = now;

        let live_preview_accepts_snapshot = self.frontend_preview.is_none_or(|preview| {
            preview.policy != FrontendPreviewPolicy::LiveResize
                || render_dimensions_match_terminal_size(delta.dimensions, preview.size)
        });

        let mut dirty = RangeSet::new();
        for r in delta.dirty_lines {
            dirty.add_range(r.clone());
        }
        if delta.cursor_position != self.cursor_position {
            dirty.add(self.cursor_position.y);
            // But note that the server may have sent this in bonus_lines;
            // we'll address that below
            dirty.add(delta.cursor_position.y);
        }

        // Keep track of the approximate round trip time by recording how
        // long it took for this response to come back
        if let Some(serial) = delta.input_serial {
            self.last_input_rtt = serial.elapsed_millis();
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
                || delta.input_serial.unwrap_or(InputSerial::empty()) >= self.input_serial)
        {
            self.cursor_position = delta.cursor_position;
        }
        let prior_server_dimensions = self.server_dimensions;
        self.server_dimensions = delta.dimensions;
        let (visible_dimensions, invalidate) = resolve_server_geometry(
            self.dimensions,
            prior_server_dimensions,
            delta.dimensions,
            self.frontend_preview.is_some(),
        );
        self.dimensions = visible_dimensions;
        if let Some(preserve_lines) = invalidate {
            // During a preview, retain old rows while marking them stale. This
            // prevents a blank flash as a full-screen application redraws.
            self.invalidate_line_cache(preserve_lines);
        }
        self.title = delta.title;
        self.working_dir = delta.working_dir.map(Into::into);
        log::trace!(
            "server says: seqno from {} -> {} for local_pane_id={}",
            self.seqno,
            delta.seqno,
            self.local_pane_id
        );
        self.seqno = delta.seqno;

        let config = configuration();
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
            self.put_line(stable_row, line, &config, None);
            dirty.remove(stable_row);
        }

        log::trace!(
            "apply_changes_to_surface: Generate PaneOutput event for local={}",
            self.local_pane_id
        );
        Mux::get().notify(mux::MuxNotification::PaneOutput(self.local_pane_id));

        let mut to_fetch = RangeSet::new();
        log::trace!("dirty as of seq {} -> {:?}", delta.seqno, dirty);
        for r in dirty.iter() {
            for stable_row in r.clone() {
                // If a line is in the (probable) viewport region,
                // then we'll likely want to fetch it.
                // If it is outside that region, remove it from our cache
                // so that we'll fetch it on demand later.
                let fetchable = stable_row >= delta.dimensions.physical_top;
                let prior = self.lines.pop(&stable_row);
                let prior_kind = prior.as_ref().map(|e| e.kind());
                if !fetchable {
                    log::trace!("make {} stale bcos not fetchable", stable_row);
                    self.make_stale(stable_row);
                    continue;
                }
                to_fetch.add(stable_row);
                let token = self.fetch_token(now);
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
                self.lines.put(stable_row, entry);
            }
        }
        if !to_fetch.is_empty() {
            if self.fetch_limiter.non_blocking_admittance_check(1) {
                self.schedule_fetch_lines(to_fetch, self.fetch_token(now));
            } else {
                log::warn!(
                    "exceeded fetch throttle, drop {:?} and mark stale",
                    to_fetch
                );
                for r in to_fetch.iter() {
                    for stable_row in r.clone() {
                        self.make_stale(stable_row);
                    }
                }
            }
        }
    }

    pub fn make_all_stale(&mut self) {
        self.invalidate_line_cache(true);
    }

    /// True when a row the GUI is actually displaying is waiting on work
    /// that only a paint performs. `viewport_top` is the GUI's displayed
    /// viewport origin (None = following the tail): scoping to the painted
    /// range — not the live screen — is what keeps this from latching, and
    /// Stale entries for rows outside it are normal and must not count.
    ///
    /// Checking also repairs a fetch whose detached future may have been
    /// lost. Its retry deadline doubles after every supersession, so a
    /// healthy but consistently slow request eventually gets a window long
    /// enough to complete instead of losing forever to a fixed watchdog.
    pub(crate) fn watchdog_check_displayed_rows(
        &mut self,
        viewport_top: Option<StableRowIndex>,
    ) -> bool {
        let top = viewport_top.unwrap_or(self.dimensions.physical_top);
        let range = top..top.saturating_add(self.dimensions.viewport_rows as StableRowIndex);
        // Keep the line-table borrow inside the pure decision helper. Only
        // after it ends do we schedule RPCs through `self`, avoiding a
        // renderable/RefCell re-entry while inspecting the cache.
        let decision = line_watchdog_decision(&mut self.lines, range, Instant::now());
        if decision.repaint {
            self.poll_interval = BASE_POLL_INTERVAL;
        }
        for batch in decision.retries {
            self.schedule_fetch_lines(batch.rows, batch.token);
        }
        decision.repaint
    }

    fn fetch_token(&self, started_at: Instant) -> FetchToken {
        FetchToken::new(self.line_cache_epoch, started_at)
    }

    fn invalidate_line_cache(&mut self, preserve_lines: bool) {
        self.line_cache_epoch = self.line_cache_epoch.wrapping_add(1);
        invalidate_line_entries(&mut self.lines, preserve_lines);
    }

    /// Converge the locally rendered surface to the geometry requested by the
    /// GUI. This is deliberately separate from deciding whether a Resize RPC
    /// is needed: a delayed server update can temporarily restore old
    /// dimensions after the desired size was already requested.
    pub(crate) fn apply_local_resize(&mut self, size: wezterm_term::TerminalSize) -> bool {
        if self.dimensions.cols == size.cols
            && self.dimensions.viewport_rows == size.rows
            && self.dimensions.pixel_width == size.pixel_width
            && self.dimensions.pixel_height == size.pixel_height
            && self.dimensions.dpi == size.dpi
        {
            return false;
        }

        let width_changed = self.dimensions.cols != size.cols;
        self.dimensions.cols = size.cols;
        self.dimensions.viewport_rows = size.rows;
        self.dimensions.pixel_width = size.pixel_width;
        self.dimensions.pixel_height = size.pixel_height;
        self.dimensions.dpi = size.dpi;
        // Keep showing the old rows while the refetch is in flight, exactly
        // as the LiveResize preview does: dropping them here painted the
        // whole pane blank for a server round trip every time chrome (the
        // sidebars) changed the terminal's width. A width change normalizes
        // the retained cell storage so an old-width row can never be
        // interpreted as already matching the new grid.
        self.line_cache_epoch = self.line_cache_epoch.wrapping_add(1);
        if width_changed {
            resize_stale_line_entries(&mut self.lines, size.cols);
        } else {
            invalidate_line_entries(&mut self.lines, true);
        }
        true
    }

    pub(crate) fn begin_frontend_preview(
        &mut self,
        epoch: u64,
        size: wezterm_term::TerminalSize,
        policy: FrontendPreviewPolicy,
    ) -> bool {
        if self
            .frontend_preview
            .is_some_and(|preview| preview.epoch > epoch)
        {
            return false;
        }
        self.frontend_preview = Some(FrontendPreviewGeometry {
            epoch,
            size,
            policy,
        });
        if render_dimensions_match_terminal_size(self.dimensions, size) {
            return false;
        }
        let width_changed = self.dimensions.cols != size.cols;
        self.dimensions.cols = size.cols;
        self.dimensions.viewport_rows = size.rows;
        self.dimensions.pixel_width = size.pixel_width;
        self.dimensions.pixel_height = size.pixel_height;
        self.dimensions.dpi = size.dpi;
        if policy == FrontendPreviewPolicy::LiveResize {
            self.line_cache_epoch = self.line_cache_epoch.wrapping_add(1);
            if width_changed {
                resize_stale_line_entries(&mut self.lines, size.cols);
            } else {
                invalidate_line_entries(&mut self.lines, true);
            }
        }
        true
    }

    pub(crate) fn server_geometry_matches(&self, size: wezterm_term::TerminalSize) -> bool {
        render_dimensions_match_terminal_size(self.server_dimensions, size)
    }

    pub(crate) fn end_frontend_preview(&mut self, epoch: u64, succeeded: bool) -> bool {
        let Some(preview) = self.frontend_preview else {
            return false;
        };
        if preview.epoch != epoch {
            return false;
        }
        self.frontend_preview = None;
        if succeeded && self.server_geometry_matches(preview.size) {
            self.dimensions = self.server_dimensions;
            return true;
        }
        if !succeeded && render_geometry_changed(self.dimensions, self.server_dimensions) {
            let preserve_lines = self.dimensions.cols == self.server_dimensions.cols;
            self.dimensions = self.server_dimensions;
            self.invalidate_line_cache(preserve_lines);
        }
        true
    }

    fn make_stale(&mut self, stable_row: StableRowIndex) {
        match self.lines.pop(&stable_row) {
            Some(LineEntry::Stale(old))
            | Some(LineEntry::Line(old))
            | Some(LineEntry::LineAndFetching(old, _)) => {
                self.lines.put(stable_row, LineEntry::Stale(old));
            }
            Some(LineEntry::Fetching(_)) | None => {}
        }
    }

    fn put_line(
        &mut self,
        stable_row: StableRowIndex,
        mut line: Line,
        config: &ConfigHandle,
        fetch_token: Option<FetchToken>,
    ) {
        line.scan_and_create_hyperlinks(&config.hyperlink_rules);

        let entry = if let Some(fetch_token) = fetch_token {
            // If we're completing a fetch, only replace entries that were
            // set to fetching as part of our fetch.  If they are now longer
            // tagged that way, then someone came along after us and changed
            // the state, so we should leave it alone

            match self.lines.pop(&stable_row) {
                Some(LineEntry::LineAndFetching(_, then)) | Some(LineEntry::Fetching(then))
                    if fetch_token == then =>
                {
                    log::trace!(
                        "row {} fetch done -> Line seq={} vs self.seq={}",
                        stable_row,
                        line.current_seqno(),
                        self.seqno
                    );
                    line.update_last_change_seqno(self.seqno);
                    LineEntry::Line(line)
                }
                Some(e) => {
                    // It changed since we started: leave it alone!
                    log::trace!(
                        "row {} {:?} changed since fetch started at {:?}, so leave it be",
                        stable_row,
                        e.kind(),
                        fetch_token
                    );
                    self.lines.put(stable_row, e);
                    return;
                }
                None => return,
            }
        } else {
            LineEntry::Line(line)
        };
        self.lines.put(stable_row, entry);
    }

    fn schedule_fetch_lines(
        &mut self,
        to_fetch: RangeSet<StableRowIndex>,
        fetch_token: FetchToken,
    ) {
        if to_fetch.is_empty() || self.dead {
            return;
        }

        let local_pane_id = self.local_pane_id;
        log::trace!(
            "will fetch lines {:?} for remote tab id {} at {:?}",
            to_fetch,
            self.remote_pane_id,
            fetch_token,
        );

        let client = Arc::clone(&self.client);
        let remote_pane_id = self.remote_pane_id;

        promise::spawn::spawn(async move {
            let result = client
                .client
                .get_lines(GetLines {
                    pane_id: remote_pane_id,
                    lines: to_fetch.clone().into(),
                })
                .await;

            let result = match result {
                Ok(result) => {
                    let lines =
                        hydrate_lines(Arc::clone(&client), remote_pane_id, result.lines).await;
                    Ok(lines)
                }
                Err(err) => Err(err),
            };
            Self::apply_lines(local_pane_id, result, to_fetch, fetch_token)
        })
        .detach();
    }

    fn apply_lines(
        local_pane_id: PaneId,
        result: anyhow::Result<Vec<(StableRowIndex, Line)>>,
        to_fetch: RangeSet<StableRowIndex>,
        fetch_token: FetchToken,
    ) -> anyhow::Result<()> {
        let mux = Mux::get();
        let pane = mux
            .get_pane(local_pane_id)
            .ok_or_else(|| anyhow!("no such tab {}", local_pane_id))?;
        let mut notify_pane_output = true;
        if let Some(client_tab) = pane.downcast_ref::<ClientPane>() {
            let renderable = client_tab.renderable.lock();
            let mut inner = renderable.inner.borrow_mut();

            if !fetch_token_is_current(fetch_token, inner.line_cache_epoch) {
                log::trace!(
                    "discarding line fetch for pane {} from epoch {} because current epoch is {}",
                    local_pane_id,
                    fetch_token.epoch,
                    inner.line_cache_epoch
                );
                // The rows this fetch covered were re-tagged Stale when the
                // epoch moved, and only a paint re-fetches Stale rows. A
                // paint is only scheduled by PaneOutput, so returning
                // without notifying can leave the pane frozen until the
                // user interacts with it. Once per epoch is enough: a live
                // resize discards several in-flight fetches per frame.
                let already_notified = inner.epoch_discard_notified == inner.line_cache_epoch;
                inner.epoch_discard_notified = inner.line_cache_epoch;
                drop(inner);
                drop(renderable);
                if !already_notified {
                    mux.notify(mux::MuxNotification::PaneOutput(local_pane_id));
                }
                return Ok(());
            }

            match result {
                Ok(lines) => {
                    let config = configuration();
                    let mut returned = RangeSet::new();

                    log::trace!("fetch complete for {:?} with {:?}", to_fetch, fetch_token);
                    for (stable_row, line) in lines.into_iter() {
                        returned.add(stable_row);
                        inner.put_line(stable_row, line, &config, Some(fetch_token));
                    }
                    // The terminal can scroll or resize while GetLines is in
                    // flight, so a successful response is allowed to omit a
                    // row that no longer exists in its stable range. Leaving
                    // that row tagged Fetching would suppress every future
                    // request for it and can hold an opaque takeover mask up
                    // forever.
                    release_unreturned_fetches(&mut inner.lines, &to_fetch, &returned, fetch_token);
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
                            let entry = match inner.lines.pop(&stable_row) {
                                Some(LineEntry::Fetching(then)) if then == fetch_token => {
                                    // leave it popped
                                    continue;
                                }
                                Some(LineEntry::LineAndFetching(line, then))
                                    if then == fetch_token =>
                                {
                                    // Stale, not Line: the local copy's seqno
                                    // is already <= inner.seqno, so a Line
                                    // would never satisfy changed_since and
                                    // the row would keep its old content
                                    // forever. Stale rows are re-fetched by
                                    // the next paint.
                                    LineEntry::Stale(line)
                                }
                                Some(entry) => entry,
                                None => continue,
                            };
                            inner.lines.put(stable_row, entry);
                        }
                    }
                }
            }
        }
        if notify_pane_output {
            log::trace!(
                "Generate PaneOutput event for local_pane_id={}",
                local_pane_id
            );
            mux.notify(mux::MuxNotification::PaneOutput(local_pane_id));
        }
        Ok(())
    }

    fn poll(&mut self) -> anyhow::Result<()> {
        let now = Instant::now();
        let mut forced_retry = false;
        let in_flight = self.poll_in_flight.load(Ordering::SeqCst);
        if in_flight != 0 {
            let deadline = stall_timeout(POLL_STALL_BASE, self.poll_stall_attempt);
            if poll_should_be_retried(self.last_poll, self.poll_stall_attempt, now) {
                // A detached completion may have been lost. Increase the
                // next deadline instead of using a fixed threshold: a slow
                // but healthy poll must eventually be allowed to win.
                log::warn!(
                    "pane {} poll generation {} exceeded {:?} (attempt {}); replacing it",
                    self.local_pane_id,
                    in_flight,
                    deadline,
                    self.poll_stall_attempt,
                );
                if !claim_poll_completion(&self.poll_in_flight, in_flight) {
                    return Ok(());
                }
                self.poll_stall_attempt = self.poll_stall_attempt.saturating_add(1);
                forced_retry = true;
            } else {
                // We have a poll in progress
                return Ok(());
            }
        }

        if !forced_retry {
            if now.saturating_duration_since(self.last_poll) < self.poll_interval {
                return Ok(());
            }
            self.poll_stall_attempt = 0;
        }

        let interval = self.poll_interval;
        let interval = (interval + interval).min(MAX_POLL_INTERVAL);
        self.poll_interval = interval;

        self.last_poll = now;
        self.poll_gen = self.poll_gen.wrapping_add(1).max(1);
        let gen = self.poll_gen;
        self.poll_in_flight.store(gen, Ordering::SeqCst);
        let poll_in_flight = Arc::clone(&self.poll_in_flight);
        let remote_pane_id = self.remote_pane_id;
        let local_pane_id = self.local_pane_id;
        let client = Arc::clone(&self.client);
        promise::spawn::spawn(async move {
            let alive = match client
                .client
                .get_pane_render_changes(GetPaneRenderChanges {
                    pane_id: remote_pane_id,
                })
                .await
            {
                Ok(resp) => resp.is_alive,
                // if we got a timeout on a reconnectable, don't
                // consider the tab to be dead; that helps to
                // avoid having a tab get shuffled around
                Err(_) => client.client.is_reconnectable,
            };

            // Cleared through the shared handle before anything that can
            // bail: if the pane has left the mux (or the downcast fails),
            // a flag left set would silence every future poll. Generation
            // checked: a poll the watchdog already superseded must not
            // clear its successor's latch or apply its stale answer.
            if !claim_poll_completion(&poll_in_flight, gen) {
                return Ok(());
            }

            let mux = Mux::get();
            let tab = mux
                .get_pane(local_pane_id)
                .ok_or_else(|| anyhow!("no such tab {}", local_pane_id))?;
            if let Some(client_tab) = tab.downcast_ref::<ClientPane>() {
                let renderable = client_tab.renderable.lock();
                let mut inner = renderable.inner.borrow_mut();

                inner.dead = !alive;
                inner.last_recv_time = Instant::now();
                inner.poll_stall_attempt = 0;
            }
            Ok::<(), anyhow::Error>(())
        })
        .detach();
        Ok(())
    }
}

lazy_static::lazy_static! {
    static ref IMAGE_LRU: Mutex<LruCache<[u8;32], Arc<ImageData>>> = Mutex::new(LruCache::new(NonZeroUsize::new(128).unwrap()));
}

pub(crate) async fn hydrate_lines(
    client: Arc<ClientInner>,
    pane_id: PaneId,
    serialized_lines: SerializedLines,
) -> Vec<(StableRowIndex, Line)> {
    let (lines, image_cells) = serialized_lines.extract_data();

    if image_cells.is_empty() {
        return lines;
    }

    let mut requests = HashMap::new();
    let mut data_by_hash = HashMap::new();
    for im in &image_cells {
        let held = IMAGE_LRU.lock().unwrap().get(&im.data_hash).cloned();
        match held {
            // A copy at or past the generation the cell was sent with is
            // current. An animation grows behind an unchanging hash, so
            // the hash alone would say "have it" forever.
            Some(data) if data.generation() >= im.data_generation => {
                data_by_hash.insert(im.data_hash, data);
            }
            held => {
                requests.entry(im.data_hash).or_insert_with(|| {
                    let have_frames = held
                        .as_ref()
                        .map(|data| super::images::frame_count(&data.data()))
                        .unwrap_or(0);
                    (
                        held,
                        GetImageCell {
                            pane_id,
                            line_idx: im.line_idx,
                            cell_idx: im.cell_idx,
                            data_hash: im.data_hash,
                            data_generation: im.data_generation,
                            have_frames,
                        },
                    )
                });
            }
        }
    }

    // Concurrently, not one at a time: these are independent round trips, so
    // awaiting them serially cost a line with N distinct images N times the
    // latency.
    let fetched = futures::future::join_all(
        requests
            .into_values()
            .map(|(held, request)| fetch_image(&client, held, request)),
    )
    .await;

    for data in fetched.into_iter().flatten() {
        IMAGE_LRU
            .lock()
            .unwrap()
            .put(data.hash(), Arc::clone(&data));
        data_by_hash.insert(data.hash(), data);
    }

    let mut line_by_idx = HashMap::new();
    for (line_idx, line) in lines {
        line_by_idx.insert(line_idx, line);
    }

    for im in image_cells {
        if let Some(data) = data_by_hash.get(&im.data_hash) {
            if let Some(line) = line_by_idx.get_mut(&im.line_idx) {
                if let Some(cell) = line.cells_mut_for_attr_changes_only().get_mut(im.cell_idx) {
                    cell.attrs_mut()
                        .attach_image(Box::new(ImageCell::with_z_index(
                            im.top_left,
                            im.bottom_right,
                            Arc::clone(data),
                            im.z_index,
                            im.padding_left,
                            im.padding_top,
                            im.padding_right,
                            im.padding_bottom,
                            im.image_id,
                            im.placement_id,
                        )));
                }
            }
        }
    }

    line_by_idx.into_iter().collect()
}

/// Fetch the image `request` names and bring `held`, the copy already
/// filed under that hash, up to date in place; the Arc to file is returned.
/// A delta the copy cannot take (its leading frames no longer match) is
/// followed by one fetch of the whole image; a whole image the copy cannot
/// take is adopted as a fresh Arc, and lines hydrated from now on point at
/// that one.
async fn fetch_image(
    client: &Arc<ClientInner>,
    held: Option<Arc<ImageData>>,
    request: GetImageCell,
) -> Option<Arc<ImageData>> {
    let whole = GetImageCell {
        have_frames: 0,
        ..request
    };
    let asked_for_delta = request.have_frames > 0;
    let mut response = client.client.get_image_cell(request).await;
    for _ in 0..2 {
        match response {
            Ok(GetImageCellResponse {
                data: Some(fresh),
                data_generation,
                frames_from,
                ..
            }) => {
                let Some(held) = &held else {
                    fresh.set_generation(data_generation);
                    return Some(fresh);
                };
                if super::images::merge_into(held, &fresh, frames_from, data_generation) {
                    return Some(Arc::clone(held));
                }
                if frames_from > 0 && asked_for_delta {
                    log::debug!("image delta did not fit the copy held; fetching the whole image");
                    response = client.client.get_image_cell(GetImageCell { ..whole }).await;
                    continue;
                }
                fresh.set_generation(data_generation);
                return Some(fresh);
            }
            Ok(GetImageCellResponse { data: None, .. }) => {
                // Not an error: the image has been let go of on the server
                // by the time the request lands, which is the ordinary
                // outcome for a pane streaming frames faster than the round
                // trip. This cell renders without it and the next frame
                // supersedes it.
                log::debug!("image cell no longer holds the requested hash");
                return None;
            }
            Err(err) => {
                log::error!("failed to retrieve image {err:#}");
                return None;
            }
        }
    }
    None
}

impl RenderableState {
    pub fn get_cursor_position(&self) -> StableCursorPosition {
        self.inner.borrow().cursor_position
    }

    pub fn get_lines(&self, lines: Range<StableRowIndex>) -> (StableRowIndex, Vec<Line>) {
        let mut inner = self.inner.borrow_mut();
        let mut result = vec![];
        let mut to_fetch = RangeSet::new();
        let now = Instant::now();
        let fetch_token = inner.fetch_token(now);

        for idx in lines.clone() {
            let entry = match inner.lines.pop(&idx) {
                Some(LineEntry::Line(line)) => {
                    result.push(line.clone());
                    if line.changed_since(inner.seqno) {
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
                    result.push(Line::with_width(inner.dimensions.cols, SEQ_ZERO));
                    LineEntry::Fetching(then)
                }
                Some(LineEntry::Stale(line)) => {
                    result.push(line.clone());
                    to_fetch.add(idx);
                    LineEntry::LineAndFetching(line, fetch_token)
                }
                None => {
                    result.push(Line::with_width(inner.dimensions.cols, SEQ_ZERO));
                    to_fetch.add(idx);
                    LineEntry::Fetching(fetch_token)
                }
            };

            if inner.client.overlay_lag_indicator && idx == inner.dimensions.physical_top {
                if inner.is_tardy() {
                    let status = format!(
                        "ThinkTerm: {:.0?}⏳since last response",
                        inner.last_recv_time.elapsed()
                    );
                    // Right align it in the tab
                    let col = inner
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

            inner.lines.put(idx, entry);
        }

        log::trace!(
            "get_lines: {:?}, num result lines={}, will fetch {:?}",
            lines,
            result.len(),
            to_fetch
        );

        inner.schedule_fetch_lines(to_fetch, fetch_token);
        (lines.start, result)
    }

    pub fn get_current_seqno(&self) -> SequenceNo {
        self.inner.borrow().seqno
    }

    pub fn get_changed_since(
        &self,
        lines: Range<StableRowIndex>,
        seqno: SequenceNo,
    ) -> RangeSet<StableRowIndex> {
        let mut inner = self.inner.borrow_mut();
        if let Err(err) = inner.poll() {
            // We allow for BrokenPromise here for now; for a TLS backed
            // session it indicates that we'll retry.  For a local unix
            // domain session it is terminal... but we will detect that
            // terminal condition elsewhere
            if let Err(err) = err.downcast::<BrokenPromise>() {
                log::error!("remote tab poll failed: {}, marking as dead", err);
                inner.dead = true;
            }
        }

        let mut result = RangeSet::new();
        for r in lines {
            match inner.lines.get(&r) {
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
        if inner.is_tardy() {
            // ... but take care to avoid always reporting it as dirty, so
            // that we don't end up busy looping just to repaint it
            if inner.last_late_dirty.elapsed() >= Duration::from_secs(1) {
                result.add(inner.dimensions.physical_top);
                inner.last_late_dirty = Instant::now();
            }
        }

        if !result.is_empty() {
            log::trace!("get_changed_since: {} -> {:?}", seqno, result);
        }

        result
    }

    pub fn get_dimensions(&self) -> RenderableDimensions {
        self.inner.borrow().dimensions
    }

    /// Which fields or render rows block a takeover from settling on `size`.
    pub(crate) fn frontend_geometry_mismatch(
        &self,
        size: wezterm_term::TerminalSize,
    ) -> Option<String> {
        let mut inner = self.inner.borrow_mut();
        let dims = inner.server_dimensions;
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

        let top = inner.server_dimensions.physical_top;
        let end = top.saturating_add(size.rows as StableRowIndex);
        let mut total = 0usize;
        let mut sample = Vec::new();
        for row in top..end {
            let kind = match inner.lines.get(&row) {
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
                inner.line_cache_epoch
            )
        })
    }

    /// Drive a render poll and populate every currently visible line while a
    /// takeover overlay is still opaque.  Returns true only after the server
    /// has confirmed `size` and none of those rows is stale or in flight.
    pub(crate) fn prime_frontend_geometry(&self, size: wezterm_term::TerminalSize) -> bool {
        let mut visible = {
            let mut inner = self.inner.borrow_mut();
            // Cancel any backoff this pane had settled into, but leave
            // `last_poll` alone so `poll` still rate-limits itself. Backdating
            // it forced a fresh render-changes RPC on *every* call, and the
            // reply emits PaneOutput, which marks the frontend dirty, which
            // draws, which calls this again: on a local socket that loop ran
            // thousands of times a second for the whole takeover window
            // instead of the ~50 the base interval allows.
            inner.poll_interval = BASE_POLL_INTERVAL;
            if let Err(err) = inner.poll() {
                log::trace!("polling takeover geometry: {err:#}");
            }
            let Some(visible) = confirmed_frontend_visible_range(inner.server_dimensions, size)
            else {
                return false;
            };
            visible
        };

        // This both returns any retained rows and schedules missing/stale
        // rows. Fetch completion emits PaneOutput and drives another overlay
        // frame, where the readiness check below can finally pass.
        let _ = self.get_lines(visible.clone());

        let mut inner = self.inner.borrow_mut();
        if confirmed_frontend_visible_range(inner.server_dimensions, size).as_ref()
            != Some(&visible)
        {
            return false;
        }
        visible.all(|row| matches!(inner.lines.get(&row), Some(LineEntry::Line(_))))
    }
}

#[cfg(test)]
mod test {
    use super::*;

    fn line(width: usize) -> Line {
        Line::with_width(width, SEQ_ZERO)
    }

    fn dimensions(cols: usize, rows: usize, dpi: u32) -> RenderableDimensions {
        RenderableDimensions {
            cols,
            viewport_rows: rows,
            scrollback_rows: rows,
            dpi,
            pixel_width: cols * 10,
            pixel_height: rows * 20,
            ..RenderableDimensions::default()
        }
    }

    #[test]
    fn preserving_invalidation_cancels_fetches_and_keeps_stale_lines() {
        let mut lines = LruCache::new(NonZeroUsize::new(8).unwrap());
        let token = FetchToken::new(7, Instant::now());
        lines.put(1, LineEntry::Line(line(80)));
        lines.put(2, LineEntry::LineAndFetching(line(80), token));
        lines.put(3, LineEntry::Fetching(token));

        invalidate_line_entries(&mut lines, true);

        assert!(matches!(lines.get(&1), Some(LineEntry::Stale(_))));
        assert!(matches!(lines.get(&2), Some(LineEntry::Stale(_))));
        assert!(
            lines.get(&3).is_none(),
            "an in-flight fetch without a line must be canceled"
        );
    }

    #[test]
    fn invalidation_and_live_resize_keep_the_cache_bounded() {
        let mut lines = LruCache::new(NonZeroUsize::new(8).unwrap());
        lines.put(1, LineEntry::Line(line(80)));

        invalidate_line_entries(&mut lines, true);
        assert_eq!(lines.cap().get(), 8);

        resize_stale_line_entries(&mut lines, 37);
        assert_eq!(lines.cap().get(), 8);
    }

    #[test]
    fn width_changing_invalidation_clears_every_cached_line() {
        let mut lines = LruCache::new(NonZeroUsize::new(8).unwrap());
        lines.put(1, LineEntry::Line(line(80)));
        lines.put(2, LineEntry::Stale(line(80)));

        invalidate_line_entries(&mut lines, false);

        assert!(lines.is_empty());
    }

    #[test]
    fn live_resize_normalizes_retained_rows_to_the_preview_width() {
        let mut lines = LruCache::new(NonZeroUsize::new(8).unwrap());
        let mut wrapped = line(80);
        wrapped.set_last_cell_was_wrapped(true, SEQ_ZERO);
        lines.put(1, LineEntry::Line(wrapped));
        lines.put(2, LineEntry::Stale(line(80)));

        resize_stale_line_entries(&mut lines, 37);

        for row in [1, 2] {
            let Some(LineEntry::Stale(line)) = lines.get(&row) else {
                panic!("row {} was not retained as stale", row);
            };
            assert_eq!(line.len(), 37);
            assert!(!line.last_cell_was_wrapped());
        }
    }

    #[test]
    fn geometry_epoch_ignores_scrollback_motion_but_detects_render_size() {
        let current = dimensions(80, 24, 96);
        let mut scrollback_only = current;
        scrollback_only.scrollback_rows += 1;
        scrollback_only.physical_top += 1;
        assert!(!render_geometry_changed(current, scrollback_only));

        let mut new_width = current;
        new_width.cols += 1;
        assert!(render_geometry_changed(current, new_width));

        let mut new_dpi = current;
        new_dpi.dpi += 1;
        assert!(render_geometry_changed(current, new_dpi));
    }

    #[test]
    fn server_updates_cannot_replace_a_pinned_preview_grid() {
        let server = dimensions(80, 24, 96);
        let preview = dimensions(140, 42, 96);
        let next_server = dimensions(100, 30, 96);

        let (visible, invalidation) = resolve_server_geometry(preview, server, next_server, true);
        assert_eq!(visible, preview);
        assert_eq!(invalidation, Some(true));

        let (visible, invalidation) = resolve_server_geometry(server, server, next_server, false);
        assert_eq!(visible, next_server);
        assert_eq!(invalidation, Some(false));
    }

    #[test]
    fn server_geometry_confirmation_requires_the_complete_terminal_size() {
        let expected = wezterm_term::TerminalSize {
            rows: 40,
            cols: 120,
            pixel_width: 1200,
            pixel_height: 800,
            dpi: 144,
        };
        let confirmed = dimensions(120, 40, 144);
        assert!(render_dimensions_match_terminal_size(confirmed, expected));

        let mut stale_rows = confirmed;
        stale_rows.viewport_rows = 24;
        assert!(!render_dimensions_match_terminal_size(stale_rows, expected));

        let mut stale_pixels = confirmed;
        stale_pixels.pixel_height = 768;
        assert!(!render_dimensions_match_terminal_size(
            stale_pixels,
            expected
        ));
    }

    #[test]
    fn takeover_fetches_the_authoritative_server_rows_after_bottom_anchored_resize() {
        let size = wezterm_term::TerminalSize {
            rows: 27,
            cols: 120,
            pixel_width: 1200,
            pixel_height: 540,
            dpi: 96,
        };
        let mut server = dimensions(120, 27, 96);
        server.pixel_width = size.pixel_width;
        server.pixel_height = size.pixel_height;
        server.physical_top = 69;

        assert_eq!(
            confirmed_frontend_visible_range(server, size),
            Some(69..96),
            "a preview's stale top row must not shift the request to 70..97"
        );
    }

    #[test]
    fn a_partial_fetch_response_does_not_leave_an_unreturned_row_in_flight_forever() {
        let token = FetchToken::new(9, Instant::now());
        let mut lines = LruCache::new(NonZeroUsize::new(8).unwrap());
        lines.put(70, LineEntry::Fetching(token));
        lines.put(71, LineEntry::LineAndFetching(line(80), token));
        let mut requested = RangeSet::new();
        requested.add_range(70..72);
        let returned = RangeSet::new();

        release_unreturned_fetches(&mut lines, &requested, &returned, token);

        assert!(lines.get(&70).is_none());
        assert!(matches!(lines.get(&71), Some(LineEntry::Stale(_))));
    }

    #[test]
    fn fetch_token_from_prior_epoch_is_not_current() {
        let token = FetchToken::new(4, Instant::now());
        assert!(fetch_token_is_current(token, 4));
        assert!(!fetch_token_is_current(token, 5));
    }

    #[test]
    fn healthy_slow_fetch_keeps_its_original_token() {
        let started = Instant::now();
        let token = FetchToken::new(7, started);
        let mut lines = LruCache::new(NonZeroUsize::new(8).unwrap());
        lines.put(10, LineEntry::Fetching(token));

        let decision = line_watchdog_decision(&mut lines, 10..11, started + Duration::from_secs(3));
        assert!(!decision.repaint);
        assert!(decision.retries.is_empty());
        assert!(matches!(lines.get(&10), Some(LineEntry::Fetching(current)) if *current == token));
    }

    #[test]
    fn fetch_stall_deadline_grows_until_a_stable_slow_rtt_can_complete() {
        let started = Instant::now();
        let first = FetchToken::new(11, started);
        let mut lines = LruCache::new(NonZeroUsize::new(8).unwrap());
        lines.put(20, LineEntry::Fetching(first));

        let retry_started = started + Duration::from_secs(31);
        let decision = line_watchdog_decision(&mut lines, 20..21, retry_started);
        assert!(decision.repaint);
        assert_eq!(decision.retries.len(), 1);
        let replacement = decision.retries[0].token;
        assert_eq!(replacement.epoch, first.epoch);
        assert_eq!(replacement.stall_attempt, 1);

        // A stable 40s RTT lost to the first 30s deadline, but the replacement
        // keeps the exact token for its 60s window and can be accepted.
        let decision =
            line_watchdog_decision(&mut lines, 20..21, retry_started + Duration::from_secs(40));
        assert!(!decision.repaint);
        assert!(decision.retries.is_empty());
        assert!(matches!(
            lines.get(&20),
            Some(LineEntry::Fetching(current)) if *current == replacement
        ));
    }

    #[test]
    fn displayed_missing_rows_retry_but_offscreen_pending_rows_do_not_latch() {
        let now = Instant::now();
        let mut missing = LruCache::new(NonZeroUsize::new(8).unwrap());
        missing.put(10, LineEntry::Line(line(80)));
        let decision = line_watchdog_decision(&mut missing, 10..12, now);
        assert!(decision.repaint, "displayed row 11 is missing");
        assert!(decision.retries.is_empty());

        let mut offscreen = LruCache::new(NonZeroUsize::new(8).unwrap());
        offscreen.put(5, LineEntry::Stale(line(80)));
        offscreen.put(10, LineEntry::Line(line(80)));
        offscreen.put(11, LineEntry::Line(line(80)));
        let decision = line_watchdog_decision(&mut offscreen, 10..12, now);
        assert!(!decision.repaint);
        assert!(decision.retries.is_empty());
    }

    #[test]
    fn epoch_bump_resets_fetch_stall_attempt() {
        let now = Instant::now();
        let old = FetchToken::new(3, now).retry(now).retry(now);
        assert_eq!(old.stall_attempt, 2);

        let mut lines = LruCache::new(NonZeroUsize::new(8).unwrap());
        lines.put(1, LineEntry::LineAndFetching(line(80), old));
        invalidate_line_entries(&mut lines, true);
        assert!(matches!(lines.get(&1), Some(LineEntry::Stale(_))));

        let next = FetchToken::new(old.epoch + 1, now);
        assert_eq!(next.stall_attempt, 0);
        assert!(!fetch_token_is_current(old, next.epoch));
        assert!(fetch_token_is_current(next, next.epoch));
    }

    #[test]
    fn poll_stall_retry_is_generation_safe_and_uses_a_growing_deadline() {
        let started = Instant::now();
        assert!(poll_should_be_retried(
            started,
            0,
            started + Duration::from_secs(20)
        ));
        assert!(!poll_should_be_retried(
            started,
            1,
            started + Duration::from_secs(20)
        ));
        assert_eq!(stall_timeout(POLL_STALL_BASE, 2), Duration::from_secs(60));
        assert_eq!(stall_timeout(FETCH_STALL_BASE, 2), Duration::from_secs(120));

        let in_flight = AtomicU64::new(2);
        assert!(!claim_poll_completion(&in_flight, 1));
        assert_eq!(in_flight.load(Ordering::SeqCst), 2);
        assert!(claim_poll_completion(&in_flight, 2));
        assert_eq!(in_flight.load(Ordering::SeqCst), 0);
    }
}
