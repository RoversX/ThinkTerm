//! Pure decisions a pane makes about resizing, palettes and its watchdog,
//! off the network so the ordering rules are unit-testable. Moved from the
//! desktop client's `clientpane.rs`; every client makes these the same way.
use thinkterm_proto::RenderableDimensions;
use wezterm_term::color::ColorPalette;
use wezterm_term::TerminalSize;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResizeDecision {
    pub converge_local_surface: bool,
    pub send_rpc: bool,
}

pub fn render_watchdog_should_run(dead: bool) -> bool {
    !dead
}

#[derive(Debug, PartialEq)]
pub struct ApplicationPaletteTransition {
    pub palette: ColorPalette,
    pub application_palette: bool,
    pub palette_changed: bool,
    pub provenance_changed: bool,
}

pub fn application_palette_transition(
    current: &ColorPalette,
    configured: &ColorPalette,
    was_application_palette: bool,
    update: Option<ColorPalette>,
) -> ApplicationPaletteTransition {
    let (palette, application_palette) = match update {
        Some(palette) => (palette, true),
        None => (configured.clone(), false),
    };
    ApplicationPaletteTransition {
        palette_changed: current != &palette,
        provenance_changed: was_application_palette != application_palette,
        palette,
        application_palette,
    }
}

pub fn render_dimensions_match_size(dimensions: RenderableDimensions, size: TerminalSize) -> bool {
    dimensions.cols == size.cols
        && dimensions.viewport_rows == size.rows
        && dimensions.pixel_width == size.pixel_width
        && dimensions.pixel_height == size.pixel_height
        && dimensions.dpi == size.dpi
}

pub fn decide_resize(
    last_requested: Option<TerminalSize>,
    dimensions: RenderableDimensions,
    target: TerminalSize,
) -> ResizeDecision {
    let converge_local_surface = !render_dimensions_match_size(dimensions, target);
    ResizeDecision {
        converge_local_surface,
        send_rpc: last_requested != Some(target) && converge_local_surface,
    }
}

pub fn decide_resize_for_viewport(
    owns_viewport: Option<bool>,
    last_requested: Option<TerminalSize>,
    dimensions: RenderableDimensions,
    target: TerminalSize,
) -> ResizeDecision {
    if owns_viewport != Some(true) {
        // A passive renderer shows the canonical server grid.  Do not first
        // reflow locally and then get snapped back by the next server push.
        // `None` is not permission: it is the attach-time interval before the
        // server has answered the first viewport advertisement.
        return ResizeDecision {
            converge_local_surface: false,
            send_rpc: false,
        };
    }
    decide_resize(last_requested, dimensions, target)
}

/// What `requested_size` holds after a resize decision. It dedupes the resize
/// RPC, so it may only record a size the server was actually told about: a
/// target latched on the passive branch would dedupe away the corrective RPC
/// for that same size once this renderer becomes the owner.
///
/// A passive renderer clears the latch outright rather than preserving it:
/// whatever it remembers was sent under a previous ownership, and the server
/// may have been driven elsewhere since - a retained key that happens to
/// equal the takeover target would dedupe away the very RPC that reclaims
/// the geometry. Clearing can never produce a spurious RPC, because
/// `send_rpc` still requires the local surface to disagree with the target.
pub fn next_requested_size(
    owns_viewport: Option<bool>,
    prior: Option<TerminalSize>,
    decision: ResizeDecision,
    target: TerminalSize,
) -> Option<TerminalSize> {
    if owns_viewport != Some(true) {
        return None;
    }
    if decision.send_rpc {
        Some(target)
    } else {
        prior
    }
}

/// Delivery state of the palette advisory RPC.
///
/// A single worker task owns all sending for a pane, and it always sends the
/// LATEST desired palette — that serialization is what makes stale overwrites
/// impossible (two concurrent send tasks with retries could land an old color
/// on the server after a newer one). The state transitions live here, off the
/// network, so the ordering rules are unit-testable.
#[derive(Default)]
pub struct PaletteDelivery {
    /// The palette we want the server to hold.
    desired: Option<ColorPalette>,
    /// The palette the server last confirmed receiving. `None` means
    /// unknown: never confirmed, failed, or invalidated by a reconnect.
    delivered: Option<ColorPalette>,
    /// Whether the sender worker is alive. At most one ever runs.
    sender_running: bool,
}

impl PaletteDelivery {
    /// Adopt a new target. Returns true when the caller must start the
    /// worker (none is running); an already-running worker will pick the
    /// new target up on its next loop.
    pub fn adopt_target(&mut self, palette: ColorPalette) -> bool {
        self.desired = Some(palette);
        if self.sender_running {
            false
        } else {
            self.sender_running = true;
            true
        }
    }

    /// What the worker should send next, or `None` when it is done — in
    /// which case the worker slot is released.
    pub fn next_to_send(&mut self) -> Option<ColorPalette> {
        match &self.desired {
            Some(target) if self.delivered.as_ref() != Some(target) => Some(target.clone()),
            _ => {
                self.sender_running = false;
                None
            }
        }
    }

    pub fn record_success(&mut self, sent: ColorPalette) {
        self.delivered = Some(sent);
    }

    /// The worker gave up (repeated RPC failures). `delivered` stays
    /// whatever it was — importantly NOT the desired value — so the next
    /// set_config or resync restarts the worker instead of assuming the
    /// server heard us.
    pub fn give_up_if_still_desired(&mut self, attempted: &ColorPalette) -> bool {
        if self.desired.as_ref() == Some(attempted) {
            self.sender_running = false;
            true
        } else {
            // A newer target arrived during the final failed RPC. Keep the
            // worker slot and let it send that target instead of stranding it
            // until an unrelated config update or resync happens.
            false
        }
    }

    /// A reconnect happened: whatever we previously confirmed, the server
    /// may be a fresh process that knows nothing.
    pub fn invalidate_delivery(&mut self) {
        self.delivered = None;
    }

    /// The palette the server last confirmed, if any.
    pub fn delivered(&self) -> Option<&ColorPalette> {
        self.delivered.as_ref()
    }
}

pub fn remote_server_identity_matches(created: Option<&str>, current: Option<&str>) -> bool {
    created == current
}

pub fn finish_preview_request(
    requested: &mut Option<TerminalSize>,
    size: TerminalSize,
    succeeded: bool,
    finished: bool,
) {
    if finished && !succeeded && requested.as_ref() == Some(&size) {
        requested.take();
    }
}

#[cfg(test)]
mod test {
    use super::*;

    fn size(cols: usize, rows: usize, dpi: u32) -> TerminalSize {
        TerminalSize {
            rows,
            cols,
            pixel_width: cols * 10,
            pixel_height: rows * 20,
            dpi,
        }
    }

    fn dimensions(size: TerminalSize) -> RenderableDimensions {
        RenderableDimensions {
            cols: size.cols,
            viewport_rows: size.rows,
            scrollback_rows: size.rows,
            dpi: size.dpi,
            pixel_width: size.pixel_width,
            pixel_height: size.pixel_height,
            ..RenderableDimensions::default()
        }
    }

    #[test]
    fn remote_pane_ids_are_reusable_only_within_the_same_mux_runtime() {
        assert!(remote_server_identity_matches(
            Some("devbox:100:runtime-a"),
            Some("devbox:100:runtime-a")
        ));
        assert!(!remote_server_identity_matches(
            Some("devbox:100:runtime-a"),
            Some("devbox:200:runtime-b")
        ));
        assert!(!remote_server_identity_matches(
            None,
            Some("devbox:200:runtime-b")
        ));
    }

    #[test]
    fn resize_decision_separates_local_convergence_from_rpc() {
        let target = size(120, 40, 96);
        let old = size(100, 30, 96);

        assert_eq!(
            decide_resize(Some(target), dimensions(target), target),
            ResizeDecision {
                converge_local_surface: false,
                send_rpc: false,
            }
        );
        assert_eq!(
            decide_resize(Some(target), dimensions(old), target),
            ResizeDecision {
                converge_local_surface: true,
                send_rpc: false,
            },
            "a delayed server surface must be repaired locally without repeating the RPC"
        );
        assert_eq!(
            decide_resize(Some(old), dimensions(target), target),
            ResizeDecision {
                converge_local_surface: false,
                send_rpc: false,
            },
            "a server that already reached the target needs no RPC"
        );
        assert_eq!(
            decide_resize(Some(old), dimensions(old), target),
            ResizeDecision {
                converge_local_surface: true,
                send_rpc: true,
            },
            "a genuine target change sends exactly one RPC"
        );
    }

    #[test]
    fn resize_decision_includes_dpi() {
        let target = size(120, 40, 144);
        let mut advertised = dimensions(target);
        advertised.dpi = 96;

        assert_eq!(
            decide_resize(Some(target), advertised, target),
            ResizeDecision {
                converge_local_surface: true,
                send_rpc: false,
            }
        );
    }

    #[test]
    fn passive_different_sized_client_keeps_the_canonical_surface_stable() {
        let canonical = size(132, 40, 96);
        let passive_window = size(80, 24, 96);

        assert_eq!(
            decide_resize_for_viewport(
                Some(false),
                Some(passive_window),
                dimensions(canonical),
                passive_window,
            ),
            ResizeDecision {
                converge_local_surface: false,
                send_rpc: false,
            },
            "a non-owner must neither publish nor locally reflow to its different window size"
        );
    }

    #[test]
    fn unknown_viewport_ownership_waits_for_the_server() {
        let canonical = size(132, 40, 96);
        let attaching_window = size(80, 24, 96);

        assert_eq!(
            decide_resize_for_viewport(None, None, dimensions(canonical), attaching_window,),
            ResizeDecision {
                converge_local_surface: false,
                send_rpc: false,
            }
        );
    }

    /// The disconnect-takeover regression: a stale dedupe key from the
    /// passive period must never suppress the corrective RPC once the lease
    /// lands here. Both halves matter: a target latched while passive (never
    /// sent), and a key retained from a PREVIOUS ownership that happens to
    /// equal the takeover target - the common case for a renderer whose
    /// window never changed while another device drove the tab.
    #[test]
    fn a_passive_resize_does_not_dedupe_away_the_owners_first_rpc() {
        let canonical = size(132, 40, 96);
        let target = size(120, 40, 96);

        let passive = decide_resize_for_viewport(Some(false), None, dimensions(canonical), target);
        assert_eq!(
            passive,
            ResizeDecision {
                converge_local_surface: false,
                send_rpc: false,
            }
        );
        let requested = next_requested_size(Some(false), None, passive, target);
        assert_eq!(requested, None, "nothing was sent, so nothing may latch");

        // A key left over from when this renderer last owned the tab is
        // cleared by any passive round, even though it equals the target.
        assert_eq!(
            next_requested_size(Some(false), Some(target), passive, target),
            None,
            "a passive renderer's dedupe key is meaningless and must clear"
        );

        let takeover =
            decide_resize_for_viewport(Some(true), requested, dimensions(canonical), target);
        assert!(takeover.send_rpc, "the corrective RPC must still go out");
        assert_eq!(
            next_requested_size(Some(true), requested, takeover, target),
            Some(target)
        );

        let repeat =
            decide_resize_for_viewport(Some(true), Some(target), dimensions(canonical), target);
        assert!(!repeat.send_rpc, "a sent size still dedupes the next call");
        assert_eq!(
            next_requested_size(Some(true), Some(target), repeat, target),
            Some(target),
            "an owner's deduped call keeps its key"
        );
    }

    #[test]
    fn stale_preview_completion_cannot_clear_a_newer_requested_size() {
        let geometry = size(120, 40, 96);
        let mut requested = Some(geometry);

        finish_preview_request(&mut requested, geometry, false, false);
        assert_eq!(requested, Some(geometry));

        finish_preview_request(&mut requested, geometry, false, true);
        assert_eq!(requested, None);
    }

    #[test]
    fn dead_panes_do_not_run_the_render_watchdog() {
        assert!(render_watchdog_should_run(false));
        assert!(!render_watchdog_should_run(true));
    }
}

#[cfg(test)]
mod palette_delivery_tests {
    use super::{application_palette_transition, PaletteDelivery};
    use wezterm_term::color::ColorPalette;

    fn palette(fg: f32) -> ColorPalette {
        let mut palette = ColorPalette::default();
        palette.foreground = (fg, fg, fg, 1.0).into();
        palette
    }

    #[test]
    fn a_send_is_confirmed_and_the_worker_stops() {
        let mut state = PaletteDelivery::default();
        assert!(
            state.adopt_target(palette(0.5)),
            "first adopt starts a worker"
        );
        let sent = state.next_to_send().expect("something to send");
        state.record_success(sent);
        assert!(state.next_to_send().is_none(), "delivered == desired: done");
        assert!(!state.sender_running, "the worker slot must be released");
    }

    /// The bug this design removes: a retry of an OLD palette must never be
    /// the last thing the server hears. The worker always re-reads the
    /// latest desired value, so a success for A while B is pending simply
    /// leads to sending B next.
    #[test]
    fn a_stale_color_can_never_be_the_last_one_standing() {
        let mut state = PaletteDelivery::default();
        assert!(state.adopt_target(palette(0.1)));
        let first = state.next_to_send().expect("A to send");

        // B arrives while A's RPC is in flight: same worker, no new task.
        assert!(!state.adopt_target(palette(0.9)), "worker already running");

        state.record_success(first);
        let second = state.next_to_send().expect("B still owed to the server");
        assert_eq!(second, palette(0.9), "the newest target wins");
        state.record_success(second);
        assert!(state.next_to_send().is_none());
    }

    /// The retry path of the same bug: a send FAILS, and while the worker
    /// waits to retry, a newer palette arrives. The retry must send the
    /// newer palette — the failed old one is simply abandoned.
    #[test]
    fn a_retry_after_failure_sends_the_newest_target() {
        let mut state = PaletteDelivery::default();
        assert!(state.adopt_target(palette(0.1)));
        let _failed = state.next_to_send().expect("A to send");
        // The RPC for A fails: no record_success. B arrives during the
        // retry backoff.
        assert!(!state.adopt_target(palette(0.9)), "worker still running");
        assert_eq!(
            state.next_to_send(),
            Some(palette(0.9)),
            "the retry must carry the newest target, not replay the old one"
        );
    }

    /// Give-up leaves delivery unconfirmed so the next advisory restarts a
    /// worker rather than assuming the server heard us.
    #[test]
    fn giving_up_allows_a_later_restart_with_the_same_value() {
        let mut state = PaletteDelivery::default();
        assert!(state.adopt_target(palette(0.5)));
        let _ = state.next_to_send().expect("initial send");
        assert!(state.give_up_if_still_desired(&palette(0.5)));
        assert!(
            state.adopt_target(palette(0.5)),
            "same value must restart the worker after a give-up"
        );
        assert_eq!(state.next_to_send(), Some(palette(0.5)));
    }

    /// A newer target arriving during the final failed RPC must keep the
    /// worker alive; otherwise no caller remains to start delivery for it.
    #[test]
    fn giving_up_an_obsolete_target_keeps_the_worker_for_the_newest_value() {
        let mut state = PaletteDelivery::default();
        let old = palette(0.1);
        let new = palette(0.9);
        assert!(state.adopt_target(old.clone()));
        assert_eq!(state.next_to_send(), Some(old.clone()));
        assert!(!state.adopt_target(new.clone()));
        assert!(!state.give_up_if_still_desired(&old));
        assert!(state.sender_running);
        assert_eq!(state.next_to_send(), Some(new));
    }

    /// A reconnect invalidates old confirmations: the server may be a fresh
    /// process that never heard the palette we once delivered.
    #[test]
    fn a_reconnect_invalidates_the_confirmation() {
        let mut state = PaletteDelivery::default();
        assert!(state.adopt_target(palette(0.5)));
        let sent = state.next_to_send().unwrap();
        state.record_success(sent);
        assert!(state.next_to_send().is_none());

        state.invalidate_delivery();
        assert!(
            state.adopt_target(palette(0.5)),
            "resync restarts the worker"
        );
        assert_eq!(state.next_to_send(), Some(palette(0.5)));
    }

    #[test]
    fn explicit_no_override_restores_the_configured_palette() {
        let configured = palette(1.0);
        let transition = application_palette_transition(&palette(0.7), &configured, true, None);
        assert_eq!(transition.palette, configured);
        assert!(!transition.application_palette);
        assert!(transition.palette_changed);
        assert!(transition.provenance_changed);
    }

    #[test]
    fn default_gray_is_honored_when_it_is_explicit_application_state() {
        let configured = palette(1.0);
        let gray = ColorPalette::default();
        let transition =
            application_palette_transition(&configured, &configured, false, Some(gray.clone()));
        assert_eq!(transition.palette, gray);
        assert!(transition.application_palette);
        assert!(transition.palette_changed);
        assert!(transition.provenance_changed);
    }

    #[test]
    fn equal_colors_still_preserve_application_provenance_without_repaint() {
        let configured = palette(1.0);
        let transition = application_palette_transition(
            &configured,
            &configured,
            false,
            Some(configured.clone()),
        );
        assert_eq!(transition.palette, configured);
        assert!(transition.application_palette);
        assert!(!transition.palette_changed);
        assert!(transition.provenance_changed);
    }
}
