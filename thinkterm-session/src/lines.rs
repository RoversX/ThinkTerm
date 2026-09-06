//! The per-pane line cache and its fetch bookkeeping: which rows are
//! current, which are on their way, when a fetch or a poll has stalled,
//! and how a cached row survives a resize. Pure over `Timestamp`.
use crate::clock::Timestamp;
use lru::LruCache;
use rangeset::RangeSet;
use std::ops::Range;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use thinkterm_proto::RenderableDimensions;
use wezterm_term::{Line, StableRowIndex};

// 30s, not shorter: poll is a fallback, and every poll costs the server a
// full-scrollback get_changed_since scan under the terminal lock, per
// painted mirror pane. Stall recovery is the watchdog's job — it resets
// the interval to BASE the moment a pane looks stuck.
pub const MAX_POLL_INTERVAL: Duration = Duration::from_secs(30);
pub const BASE_POLL_INTERVAL: Duration = Duration::from_millis(20);
pub const FETCH_STALL_BASE: Duration = Duration::from_secs(30);
pub const POLL_STALL_BASE: Duration = Duration::from_secs(15);

pub fn stall_timeout(base: Duration, attempt: u32) -> Duration {
    let multiplier = 1u32.checked_shl(attempt).unwrap_or(u32::MAX);
    base.checked_mul(multiplier).unwrap_or(Duration::MAX)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FetchToken {
    pub epoch: u64,
    pub started_at: Timestamp,
    pub stall_attempt: u32,
}

impl FetchToken {
    pub fn new(epoch: u64, started_at: Timestamp) -> Self {
        Self {
            epoch,
            started_at,
            stall_attempt: 0,
        }
    }

    pub fn retry(self, started_at: Timestamp) -> Self {
        Self {
            epoch: self.epoch,
            started_at,
            stall_attempt: self.stall_attempt.saturating_add(1),
        }
    }
}

pub fn fetch_should_be_retried(token: FetchToken, now: Timestamp) -> bool {
    now.saturating_duration_since(token.started_at)
        > stall_timeout(FETCH_STALL_BASE, token.stall_attempt)
}

pub fn poll_should_be_retried(started_at: Timestamp, attempt: u32, now: Timestamp) -> bool {
    now.saturating_duration_since(started_at) > stall_timeout(POLL_STALL_BASE, attempt)
}

pub fn claim_poll_completion(in_flight: &AtomicU64, generation: u64) -> bool {
    in_flight
        .compare_exchange(generation, 0, Ordering::SeqCst, Ordering::SeqCst)
        .is_ok()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrontendPreviewGeometry {
    pub epoch: u64,
    pub size: wezterm_term::TerminalSize,
    pub policy: FrontendPreviewPolicy,
}

/// How an optimistic frontend geometry should treat rows that were fetched for
/// the preceding grid.  Takeover previews are hidden by an opaque frontend
/// surface, while a live divider resize is visible and must never expose a row
/// whose cell storage still has the old width.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrontendPreviewPolicy {
    PreserveRows,
    LiveResize,
}

#[derive(Debug)]
pub enum LineEntry {
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
pub struct FetchRetryBatch {
    pub token: FetchToken,
    pub rows: RangeSet<StableRowIndex>,
}

#[derive(Debug, Default)]
pub struct LineWatchdogDecision {
    pub repaint: bool,
    pub retries: Vec<FetchRetryBatch>,
}

impl LineWatchdogDecision {
    pub fn retry(&mut self, row: StableRowIndex, token: FetchToken) {
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

pub fn line_watchdog_decision(
    lines: &mut LruCache<StableRowIndex, LineEntry>,
    range: Range<StableRowIndex>,
    now: Timestamp,
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
    pub fn kind(&self) -> (&'static str, Option<FetchToken>) {
        match self {
            Self::Line(_) => ("Line", None),
            Self::Fetching(token) => ("Fetching", Some(*token)),
            Self::LineAndFetching(_, token) => ("LineAndFetching", Some(*token)),
            Self::Stale(_) => ("Stale", None),
        }
    }
}

pub fn invalidate_line_entries(
    lines: &mut LruCache<StableRowIndex, LineEntry>,
    preserve_lines: bool,
) {
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
pub fn resize_stale_line_entries(lines: &mut LruCache<StableRowIndex, LineEntry>, cols: usize) {
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

pub fn render_geometry_changed(current: RenderableDimensions, next: RenderableDimensions) -> bool {
    current.cols != next.cols
        || current.viewport_rows != next.viewport_rows
        || current.pixel_width != next.pixel_width
        || current.pixel_height != next.pixel_height
        || current.dpi != next.dpi
}

pub fn render_dimensions_match_terminal_size(
    dimensions: RenderableDimensions,
    size: wezterm_term::TerminalSize,
) -> bool {
    dimensions.cols == size.cols
        && dimensions.viewport_rows == size.rows
        && dimensions.pixel_width == size.pixel_width
        && dimensions.pixel_height == size.pixel_height
        && dimensions.dpi == size.dpi
}

pub fn confirmed_frontend_visible_range(
    dimensions: RenderableDimensions,
    size: wezterm_term::TerminalSize,
) -> Option<Range<StableRowIndex>> {
    if !render_dimensions_match_terminal_size(dimensions, size) {
        return None;
    }
    let top = dimensions.physical_top;
    Some(top..top.saturating_add(size.rows as StableRowIndex))
}

pub fn release_unreturned_fetches(
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

pub fn resolve_server_geometry(
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

pub fn fetch_token_is_current(token: FetchToken, epoch: u64) -> bool {
    token.epoch == epoch
}

#[cfg(test)]
mod test {
    use super::*;
    use std::num::NonZeroUsize;
    use termwiz::surface::SEQ_ZERO;

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
        let token = FetchToken::new(7, Timestamp::ZERO);
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
        let token = FetchToken::new(9, Timestamp::ZERO);
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
        let token = FetchToken::new(4, Timestamp::ZERO);
        assert!(fetch_token_is_current(token, 4));
        assert!(!fetch_token_is_current(token, 5));
    }

    #[test]
    fn healthy_slow_fetch_keeps_its_original_token() {
        let started = Timestamp::ZERO;
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
        let started = Timestamp::ZERO;
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
        let now = Timestamp::ZERO;
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
        let now = Timestamp::ZERO;
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
        let started = Timestamp::ZERO;
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
