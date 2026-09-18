//! What a client knows about one remote pane: the rows it holds and their
//! fetch state, the cursor, the geometry the server confirmed and the
//! geometry being shown, and the timing that drives polls, fetch stalls
//! and predictive echo. Pure over the host's `Timestamp`; the session in
//! `pane.rs` does the talking.
use crate::clock::{RateLimiter, Timestamp};
use crate::lines::*;
use crate::SessionConfig;
use codec::InputSerial;
use lru::LruCache;
use std::num::NonZeroUsize;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use std::time::Duration;
use termwiz::cell::{Cell, CellAttributes, Underline};
use termwiz::input::KeyboardEncoding;
use termwiz::surface::{SequenceNo, SEQ_ZERO};
use thinkterm_proto::{RenderableDimensions, StableCursorPosition};
use url::Url;
use wezterm_term::{KeyCode, KeyModifiers, Line, StableRowIndex, TerminalSize};

pub struct PaneState {
    pub(crate) config: SessionConfig,
    pub(crate) last_poll: Timestamp,
    pub(crate) dead: bool,
    /// Generation of the poll currently in flight; 0 means none. Shared
    /// with the poll task so completion can clear it without finding this
    /// pane again (a pane that left the mux must not stay latched), and
    /// generation-checked so a watchdog-superseded poll completing late
    /// can neither clear its successor's latch nor apply a stale answer.
    pub(crate) poll_in_flight: Arc<AtomicU64>,
    pub(crate) poll_gen: u64,
    pub(crate) poll_stall_attempt: u32,
    pub(crate) poll_interval: Duration,

    pub(crate) cursor_position: StableCursorPosition,
    /// Whether the remote pane showed its alternate screen at the last
    /// push. The two screens share one stable-row space on the wire, so a
    /// switch changes what every cached row means without any row being
    /// resent: the cache is cleared on the switch.
    pub(crate) alt_screen: bool,
    pub(crate) mouse_grabbed: bool,
    pub(crate) keyboard_encoding: KeyboardEncoding,
    pub(crate) dimensions: RenderableDimensions,
    /// The dimensions most recently confirmed by the remote pane.  A GUI
    /// takeover may resize its local surface optimistically; keeping the
    /// server value separate lets it remain masked until a post-resize render
    /// snapshot has actually arrived.
    pub(crate) server_dimensions: RenderableDimensions,
    /// While a native GUI divider is moving, this is the authoritative render
    /// surface. Remote snapshots continue updating `server_dimensions` and
    /// line contents, but cannot bounce the visible surface back to an older
    /// grid between mouse-move frames.
    pub(crate) frontend_preview: Option<FrontendPreviewGeometry>,

    pub(crate) lines: LruCache<StableRowIndex, LineEntry>,
    pub(crate) line_cache_epoch: u64,
    /// The cache epoch whose scrollback was last fetched whole; a flush
    /// (screen switch, resize) moves the epoch and the next paint warms
    /// the cache again.
    pub(crate) warmed_epoch: Option<u64>,
    /// Where the physical top was when the scrollback was last warmed;
    /// rows that scrolled past since were never pushed, so a viewport of
    /// growth warms again.
    pub(crate) warmed_top: StableRowIndex,
    /// The epoch for which a discarded-fetch PaneOutput has already been
    /// emitted; during a live resize every frame bumps the epoch and can
    /// discard several in-flight fetches, and one repaint per epoch is
    /// enough to re-issue them.
    pub(crate) epoch_discard_notified: u64,
    pub(crate) title: String,
    pub(crate) working_dir: Option<Url>,
    pub(crate) seqno: SequenceNo,

    pub(crate) fetch_limiter: RateLimiter,

    pub(crate) last_send_time: Timestamp,
    pub(crate) last_recv_time: Timestamp,
    /// Whether the server has said anything about this pane yet: before
    /// that its cursor and rows are placeholders, not a picture.
    pub(crate) received: bool,
    pub(crate) last_late_dirty: Timestamp,
    pub(crate) last_input_rtt: u64,

    pub(crate) input_serial: InputSerial,
}

impl PaneState {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: SessionConfig,
        dimensions: RenderableDimensions,
        title: &str,
        alt_screen: bool,
        fetch_rate_per_second: u32,
        now: Timestamp,
    ) -> Self {
        Self {
            config,
            last_poll: now,
            dead: false,
            poll_in_flight: Arc::new(AtomicU64::new(0)),
            poll_gen: 0,
            poll_stall_attempt: 0,
            poll_interval: BASE_POLL_INTERVAL,
            cursor_position: StableCursorPosition::default(),
            dimensions,
            alt_screen,
            mouse_grabbed: false,
            keyboard_encoding: KeyboardEncoding::Xterm,
            server_dimensions: dimensions,
            frontend_preview: None,
            lines: LruCache::new(NonZeroUsize::new(config.scrollback_lines.max(128)).unwrap()),
            line_cache_epoch: 0,
            warmed_epoch: None,
            warmed_top: 0,
            epoch_discard_notified: 0,
            title: title.to_string(),
            working_dir: None,
            fetch_limiter: RateLimiter::new(fetch_rate_per_second, now),
            last_send_time: now,
            last_recv_time: now,
            received: false,
            last_late_dirty: now,
            last_input_rtt: 0,
            input_serial: InputSerial::empty(),
            seqno: SEQ_ZERO,
        }
    }

    /// How long since the server was last heard from for this pane.
    pub fn since_last_response(&self, now: Timestamp) -> Duration {
        now.saturating_duration_since(self.last_recv_time)
    }

    /// Returns true if we think we should display the laggy connection
    /// indicator.  If we're past our poll interval and more recently
    /// tried to send something than receive something, the UI is worth
    /// showing.
    pub fn is_tardy(&self, now: Timestamp) -> bool {
        let elapsed = self.since_last_response(now);
        if elapsed > self.poll_interval.max(Duration::from_secs(3)) {
            self.last_send_time > self.last_recv_time
        } else {
            false
        }
    }

    /// Predictive echo can be noisy when the link is working well,
    /// so we only employ it when it looks like the latency is high.
    fn should_predict(&self) -> bool {
        self.config
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

    pub fn update_last_send(&mut self, now: Timestamp) {
        self.last_send_time = now;
        self.poll_interval = BASE_POLL_INTERVAL;
    }

    pub fn make_all_stale(&mut self) {
        self.invalidate_line_cache(true);
    }

    pub(crate) fn fetch_token(&self, started_at: Timestamp) -> FetchToken {
        FetchToken::new(self.line_cache_epoch, started_at)
    }

    pub(crate) fn invalidate_line_cache(&mut self, preserve_lines: bool) {
        self.line_cache_epoch = self.line_cache_epoch.wrapping_add(1);
        invalidate_line_entries(&mut self.lines, preserve_lines);
    }

    /// Converge the locally rendered surface to the geometry requested by the
    /// GUI. This is deliberately separate from deciding whether a Resize RPC
    /// is needed: a delayed server update can temporarily restore old
    /// dimensions after the desired size was already requested.
    pub fn apply_local_resize(&mut self, size: TerminalSize) -> bool {
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

    pub fn begin_frontend_preview(
        &mut self,
        epoch: u64,
        size: TerminalSize,
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

    pub fn server_geometry_matches(&self, size: TerminalSize) -> bool {
        render_dimensions_match_terminal_size(self.server_dimensions, size)
    }

    pub fn end_frontend_preview(&mut self, epoch: u64, succeeded: bool) -> bool {
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

    pub(crate) fn make_stale(&mut self, stable_row: StableRowIndex) {
        match self.lines.pop(&stable_row) {
            Some(LineEntry::Stale(old))
            | Some(LineEntry::Line(old))
            | Some(LineEntry::LineAndFetching(old, _)) => {
                self.lines.put(stable_row, LineEntry::Stale(old));
            }
            Some(LineEntry::Fetching(_)) | None => {}
        }
    }

    pub(crate) fn put_line(
        &mut self,
        stable_row: StableRowIndex,
        mut line: Line,
        rules: &[termwiz::hyperlink::Rule],
        fetch_token: Option<FetchToken>,
    ) {
        line.scan_and_create_hyperlinks(rules);

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
}
