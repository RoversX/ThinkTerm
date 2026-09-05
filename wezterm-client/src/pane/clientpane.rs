use crate::domain::ClientInner;
use crate::pane::mousestate;
use crate::pane::renderable::{
    hydrate_lines, FrontendPreviewPolicy, RenderableInner, RenderableState,
};
use anyhow::bail;
use async_trait::async_trait;
use codec::*;
use config::configuration;
use config::keyassignment::ScrollbackEraseMode;
use futures::future::{BoxFuture, FutureExt};
use futures::lock::Mutex as AsyncMutex;
use futures::stream::{FuturesUnordered, StreamExt};
use mux::domain::DomainId;
use mux::pane::{
    alloc_pane_id, CachePolicy, CloseReason, ForEachPaneLogicalLine, LogicalLine, Pane, PaneId,
    Pattern, SearchResult, WithPaneLines,
};
use mux::renderable::{RenderableDimensions, StableCursorPosition};
use mux::tab::TabId;
use mux::{Mux, MuxNotification};
use parking_lot::{MappedMutexGuard, Mutex, MutexGuard};
use rangeset::RangeSet;
use ratelim::RateLimiter;
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::ops::Range;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use termwiz::input::{KeyEvent, KeyboardEncoding};
use termwiz::surface::SequenceNo;
use url::Url;
use wezterm_dynamic::Value;
use wezterm_term::color::ColorPalette;
use wezterm_term::{
    Alert, Clipboard, KeyCode, KeyModifiers, Line, MouseEvent, Progress, StableRowIndex,
    TerminalConfiguration, TerminalSize,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ResizeDecision {
    converge_local_surface: bool,
    send_rpc: bool,
}

fn render_watchdog_should_run(dead: bool) -> bool {
    !dead
}

#[derive(Debug, PartialEq)]
struct ApplicationPaletteTransition {
    palette: ColorPalette,
    application_palette: bool,
    palette_changed: bool,
    provenance_changed: bool,
}

fn application_palette_transition(
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

fn render_dimensions_match_size(dimensions: RenderableDimensions, size: TerminalSize) -> bool {
    dimensions.cols == size.cols
        && dimensions.viewport_rows == size.rows
        && dimensions.pixel_width == size.pixel_width
        && dimensions.pixel_height == size.pixel_height
        && dimensions.dpi == size.dpi
}

fn decide_resize(
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

fn decide_resize_for_viewport(
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
fn next_requested_size(
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
struct PaletteDelivery {
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
    fn adopt_target(&mut self, palette: ColorPalette) -> bool {
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
    fn next_to_send(&mut self) -> Option<ColorPalette> {
        match &self.desired {
            Some(target) if self.delivered.as_ref() != Some(target) => Some(target.clone()),
            _ => {
                self.sender_running = false;
                None
            }
        }
    }

    fn record_success(&mut self, sent: ColorPalette) {
        self.delivered = Some(sent);
    }

    /// The worker gave up (repeated RPC failures). `delivered` stays
    /// whatever it was — importantly NOT the desired value — so the next
    /// set_config or resync restarts the worker instead of assuming the
    /// server heard us.
    fn give_up_if_still_desired(&mut self, attempted: &ColorPalette) -> bool {
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
    fn invalidate_delivery(&mut self) {
        self.delivered = None;
    }
}

pub(crate) fn remote_server_identity_matches(created: Option<&str>, current: Option<&str>) -> bool {
    created == current
}

pub struct ClientPane {
    client: Arc<ClientInner>,
    /// The mux runtime that allocated `remote_pane_id`.
    ///
    /// Pane ids are process-local and restart from small values when a mux
    /// server is replaced.  Keeping the allocating runtime here prevents a
    /// replacement resync from mistaking a disconnected, same-numbered pane
    /// for the fresh server's pane.
    remote_server_id: Option<String>,
    local_pane_id: PaneId,
    pub remote_pane_id: PaneId,
    remote_tab_id: Arc<AtomicUsize>,
    pub renderable: Mutex<RenderableState>,
    configured_palette: Arc<Mutex<ColorPalette>>,
    /// Delivery state for the palette advisory: what we want the server to
    /// hold and what it last confirmed. A single worker task drains it; see
    /// [`PaletteDelivery`].
    delivered_palette: Arc<Mutex<PaletteDelivery>>,
    /// Serializes the two RPCs that can mutate this client's advisory on the
    /// server: SetPalette and the palette-bearing SetFocusedPane. Without
    /// this, an older focus task can race a config reload and land last.
    palette_rpc_lock: Arc<AsyncMutex<()>>,
    palette: Mutex<ColorPalette>,
    application_palette: Mutex<bool>,
    writer: Mutex<PaneWriter>,
    /// Everything this pane was given and the server has not answered,
    /// in the order it was given; see [`PaneInput`].
    inputs: Arc<Mutex<InputQueue>>,
    clipboard: Mutex<Option<Arc<dyn Clipboard>>>,
    mouse_grabbed: Mutex<bool>,
    alt_screen: Mutex<bool>,
    /// As last reported by the server. The GUI's key encoders consult it
    /// before every key: a pane left at Xterm never gets kitty-protocol or
    /// win32-input-mode bytes, whatever the remote program asked for.
    keyboard_encoding: Mutex<KeyboardEncoding>,
    /// Render pushes waiting to be applied, in the order they arrived.
    render_deltas: Mutex<RenderDeltaQueue>,
    requested_size: Mutex<Option<TerminalSize>>,
    ignore_next_kill: Mutex<bool>,
    user_vars: Mutex<HashMap<String, String>>,
    config: Mutex<Option<Arc<dyn TerminalConfiguration>>>,
    unseen_output: Mutex<bool>,
    progress: Mutex<Progress>,
    agent_status: Mutex<Option<thinkterm_proto::AgentStatus>>,
}

impl ClientPane {
    /// Store a status delivered outside the unilateral push path (the
    /// cold-start fetch on attach/resync). Quiet: the caller decides
    /// whether a repaint is warranted.
    pub fn set_agent_status(&self, status: Option<thinkterm_proto::AgentStatus>) {
        *self.agent_status.lock() = status;
    }

    /// Ask the (single) sender worker to bring the server to `palette`.
    /// Starts the worker when none is running; a running worker picks the
    /// new target up by itself. Re-sends of identical palettes are
    /// flicker-free: both the server and the receiving side de-duplicate.
    fn advise_server_palette(
        client: Arc<ClientInner>,
        remote_pane_id: PaneId,
        palette: ColorPalette,
        delivery: Arc<Mutex<PaletteDelivery>>,
        rpc_lock: Arc<AsyncMutex<()>>,
    ) {
        if !delivery.lock().adopt_target(palette) {
            return;
        }
        promise::spawn::spawn(async move {
            let mut failures = 0u32;
            let mut failed_target: Option<ColorPalette> = None;
            loop {
                let Some(target) = delivery.lock().next_to_send() else {
                    // Delivered everything we wanted; the slot is released.
                    return;
                };
                let result = {
                    let _guard = rpc_lock.lock().await;
                    client
                        .client
                        .set_configured_palette_for_pane(SetPalette {
                            pane_id: remote_pane_id,
                            palette: target.clone(),
                        })
                        .await
                };
                match result {
                    Ok(_) => {
                        failures = 0;
                        failed_target = None;
                        // Desired may have moved on while this was in
                        // flight; the next loop sends the newer target, so
                        // an old color can never be the last one standing.
                        delivery.lock().record_success(target);
                    }
                    Err(err) => {
                        if failed_target.as_ref() == Some(&target) {
                            failures += 1;
                        } else {
                            // A new desired palette gets its own retry budget;
                            // failures of the superseded value do not count.
                            failed_target = Some(target.clone());
                            failures = 1;
                        }
                        log::warn!(
                            "advising palette for remote pane {remote_pane_id} \
                             (attempt {failures}): {err:#}"
                        );
                        if failures >= 3 {
                            let gave_up = delivery.lock().give_up_if_still_desired(&target);
                            if gave_up {
                                // Leave delivery unconfirmed: the next
                                // set_config or resync restarts the worker
                                // rather than assuming the server heard us.
                                return;
                            }
                            failures = 0;
                            failed_target = None;
                            continue;
                        }
                        smol::Timer::after(std::time::Duration::from_secs(2)).await;
                    }
                }
            }
        })
        .detach();
    }

    /// Re-advise the server of the configured palette. Called after a
    /// resync reattaches this pane: the server may be a fresh process that
    /// never received the original advisory, and its bare default palette
    /// is what OSC color queries would keep answering. Previous delivery
    /// confirmations are meaningless across a reconnect, so they are
    /// invalidated first.
    pub fn resend_palette_to_server(&self) {
        let palette = self.configured_palette.lock().clone();
        self.delivered_palette.lock().invalidate_delivery();
        Self::advise_server_palette(
            Arc::clone(&self.client),
            self.remote_pane_id,
            palette,
            Arc::clone(&self.delivered_palette),
            Arc::clone(&self.palette_rpc_lock),
        );
    }

    pub fn new(
        client: &Arc<ClientInner>,
        remote_tab_id: TabId,
        remote_pane_id: PaneId,
        size: TerminalSize,
        title: &str,
        alt_screen: bool,
    ) -> Self {
        let local_pane_id = alloc_pane_id();
        let remote_tab_id = Arc::new(AtomicUsize::new(remote_tab_id));
        let inputs: Arc<Mutex<InputQueue>> = Default::default();
        let writer = PaneWriter {
            client: Arc::clone(client),
            remote_pane_id,
            remote_tab_id: Arc::clone(&remote_tab_id),
            inputs: Arc::clone(&inputs),
        };

        let fetch_limiter =
            RateLimiter::new(|config| config.ratelimit_mux_line_prefetches_per_second);

        let render = RenderableState {
            inner: RefCell::new(RenderableInner::new(
                client,
                remote_pane_id,
                local_pane_id,
                RenderableDimensions {
                    cols: size.cols as _,
                    viewport_rows: size.rows as _,
                    scrollback_rows: size.rows as _,
                    physical_top: 0,
                    scrollback_top: 0,
                    dpi: size.dpi,
                    pixel_width: size.pixel_width,
                    pixel_height: size.pixel_height,
                    reverse_video: false,
                },
                title,
                fetch_limiter,
            )),
        };

        let config = configuration();
        let palette: ColorPalette = config.resolved_palette.clone().into();

        // Advise the server of our palette preference. Delivery is tracked:
        // if this send is lost (attach races, transient RPC failure), the
        // server would otherwise answer OSC color queries from its bare
        // defaults forever — and an application that queries-then-restores
        // the foreground would then paint the pane in those defaults.
        let delivered_palette = Arc::new(Mutex::new(PaletteDelivery::default()));
        let palette_rpc_lock = Arc::new(AsyncMutex::new(()));
        Self::advise_server_palette(
            Arc::clone(client),
            remote_pane_id,
            palette.clone(),
            Arc::clone(&delivered_palette),
            Arc::clone(&palette_rpc_lock),
        );

        Self {
            client: Arc::clone(client),
            remote_server_id: client.client.remote_server_id(),
            inputs,
            remote_pane_id,
            local_pane_id,
            remote_tab_id,
            application_palette: Mutex::new(false),
            renderable: Mutex::new(render),
            writer: Mutex::new(writer),
            configured_palette: Arc::new(Mutex::new(palette.clone())),
            delivered_palette,
            palette_rpc_lock,
            palette: Mutex::new(palette),
            clipboard: Mutex::new(None),
            mouse_grabbed: Mutex::new(false),
            alt_screen: Mutex::new(alt_screen),
            keyboard_encoding: Mutex::new(KeyboardEncoding::Xterm),
            render_deltas: Mutex::new(RenderDeltaQueue::default()),
            requested_size: Mutex::new(Some(size)),
            ignore_next_kill: Mutex::new(false),
            unseen_output: Mutex::new(false),
            user_vars: Mutex::new(HashMap::new()),
            config: Mutex::new(None),
            progress: Mutex::new(Progress::default()),
            // Seed from the domain's remote-keyed snapshot: this pane's
            // status may have been fetched or pushed before the mirror
            // existed, and nothing re-delivers it until the agent next
            // changes state.
            agent_status: Mutex::new(client.remote_agent_status(remote_pane_id)),
        }
    }

    /// Apply one push. With `carried` given, newer pushes wait behind this
    /// one and it is applied without fetching pictures; a row it then has
    /// to leave out is marked dirty unless one of those pushes brings it,
    /// since the server sent it as a bonus row and nothing else would ever
    /// fetch it again.
    async fn apply_render_delta(
        &self,
        mut delta: GetPaneRenderChangesResponse,
        carried: Option<RowsCarried>,
    ) {
        *self.mouse_grabbed.lock() = delta.mouse_grabbed;
        *self.alt_screen.lock() = delta.alt_screen;
        *self.keyboard_encoding.lock() = delta.keyboard_encoding.into();

        let bonus_lines = std::mem::take(&mut delta.bonus_lines);
        let client = { Arc::clone(&self.renderable.lock().inner.borrow().client) };
        let (bonus_lines, left_out) =
            hydrate_lines(client, delta.pane_id, bonus_lines, carried.is_none()).await;
        if let Some(carried) = carried {
            for row in left_out {
                if !carried.contains(row) {
                    delta.dirty_lines.push(row..row + 1);
                }
            }
        }

        self.renderable
            .lock()
            .inner
            .borrow_mut()
            .apply_changes_to_surface(delta, bonus_lines);
    }

    pub async fn process_unilateral(&self, pdu: Pdu) -> anyhow::Result<()> {
        match pdu {
            Pdu::GetPaneRenderChangesResponse(delta) => {
                // Queued, and applied one at a time in arrival order by a
                // single task. Each push used to be its own task that
                // awaited the images it named; several in flight at once
                // finished in whatever order their fetches did, so an older
                // push could land after a newer one and roll the rows back,
                // and a program streaming pictures had every push fetching
                // a frame that was already stale.
                if self.render_deltas.lock().push(delta) {
                    let local_pane_id = self.local_pane_id;
                    promise::spawn::spawn(drain_render_deltas(local_pane_id)).detach();
                }
            }
            Pdu::SetClipboard(SetClipboard {
                clipboard,
                selection,
                ..
            }) => match self.clipboard.lock().as_ref() {
                Some(clip) => {
                    log::debug!(
                        "Pdu::SetClipboard pane={} remote={} {:?} {:?}",
                        self.local_pane_id,
                        self.remote_pane_id,
                        selection,
                        clipboard
                    );
                    clip.set_contents(selection, clipboard)?;
                }
                None => {
                    log::error!("ClientPane: Ignoring SetClipboard request {:?}", clipboard);
                }
            },
            Pdu::SetApplicationPalette(SetApplicationPalette { palette, .. }) => {
                let current = self.palette.lock().clone();
                let configured = self.configured_palette.lock().clone();
                let was_application_palette = *self.application_palette.lock();
                let transition = application_palette_transition(
                    &current,
                    &configured,
                    was_application_palette,
                    palette,
                );

                *self.application_palette.lock() = transition.application_palette;
                if transition.palette_changed {
                    *self.palette.lock() = transition.palette;
                    self.renderable.lock().inner.borrow_mut().make_all_stale();
                }

                // Provenance changes must propagate through chained muxes even
                // when the effective colors happen to be identical. Only an
                // actual color change invalidates the render surface above.
                if transition.palette_changed || transition.provenance_changed {
                    let mux = Mux::get();
                    mux.notify(MuxNotification::Alert {
                        pane_id: self.local_pane_id,
                        alert: Alert::PaletteChanged,
                    });
                }
            }
            Pdu::NotifyAlert(NotifyAlert { alert, .. }) => {
                let mux = Mux::get();
                match &alert {
                    Alert::SetUserVar { name, value } => {
                        self.user_vars.lock().insert(name.clone(), value.clone());
                    }
                    Alert::OutputSinceFocusLost => {
                        *self.unseen_output.lock() = true;
                        mux.notify(MuxNotification::Alert {
                            pane_id: self.local_pane_id,
                            alert: Alert::OutputSinceFocusLost,
                        });
                    }
                    Alert::Progress(progress) => {
                        *self.progress.lock() = progress.clone();
                        mux.notify(MuxNotification::Alert {
                            pane_id: self.local_pane_id,
                            alert: Alert::Progress(progress.clone()),
                        });
                    }
                    _ => {}
                }
                mux.notify(MuxNotification::Alert {
                    pane_id: self.local_pane_id,
                    alert,
                });
            }
            Pdu::AgentStatusChanged(codec::AgentStatusChanged { status, .. }) => {
                // Read-at-send-time coalescing means a backlog of queued
                // notifications all carry the same current value; only a
                // real change is worth a re-notify (each one repaints and
                // re-scans thread work downstream).
                let changed = {
                    let mut slot = self.agent_status.lock();
                    if *slot == status {
                        false
                    } else {
                        *slot = status;
                        true
                    }
                };
                if changed {
                    // Re-notify locally: repaints this GUI, and when this
                    // process is itself a mux server for further clients,
                    // its dispatch forwards the status one hop on (same
                    // reason the Progress alert above re-notifies).
                    Mux::get().notify(MuxNotification::AgentStatusChanged(self.local_pane_id));
                }
            }
            Pdu::PaneRemoved(PaneRemoved { pane_id }) => {
                log::trace!("remote pane {} has been removed", pane_id);
                self.renderable.lock().inner.borrow_mut().dead = true;
                // The prune below can be deferred (activity in flight, or
                // the windows lock contended); the dead mirror must not
                // keep reporting an agent to the panel meanwhile.
                *self.agent_status.lock() = None;
                let mux = Mux::get();
                mux.prune_dead_windows();

                self.client.expire_stale_mappings();
            }
            Pdu::PaneFocused(PaneFocused { pane_id }) => {
                // We get here whenever the pane focus is changed on the
                // server. That might be an echo of a focus change we
                // advised ourselves, or a "remote" `thinkterm cli
                // activate-pane-direction` style call from some other
                // actor. Applying it yanks both the window's active tab
                // and the pane stack's active pane, so a STALE echo (of an
                // advisory older than the user's latest selection) must be
                // discarded or rapid tab/stack switching visibly flips
                // between the old and new selections. An echo that matches
                // our latest advisory is applied: it is normally a no-op,
                // and it heals a resync that carried a pre-switch snapshot.
                let advised = *self.client.focused_remote_pane_id.lock().unwrap();
                let advised_recently = self
                    .client
                    .focus_advised_at
                    .lock()
                    .unwrap()
                    .map_or(false, |at| at.elapsed() < std::time::Duration::from_secs(3));
                if advised != Some(self.remote_pane_id) && advised_recently {
                    log::trace!(
                        "ignoring stale remote pane focus {pane_id}: \
                         newer local advisory for {advised:?} is in flight"
                    );
                    return Ok(());
                }
                log::trace!("advised of remote pane focus: {pane_id}");

                let mux = Mux::get();
                if let Err(err) = mux.focus_pane_and_containing_tab(self.local_pane_id) {
                    log::error!("Error reconciling remote PaneFocused notification: {err:#}");
                } else if let Some((_domain, window_id, _tab)) =
                    mux.resolve_pane_id(self.local_pane_id)
                {
                    // The reconcile flips tab/stack selection silently (to
                    // avoid focus advisory loops); nudge the GUI to repaint.
                    mux.notify(MuxNotification::WindowInvalidated(window_id));
                }
            }
            _ => bail!("unhandled unilateral pdu: {:?}", pdu),
        };
        Ok(())
    }

    pub(crate) fn belongs_to_remote_server(&self, server_id: Option<&str>) -> bool {
        remote_server_identity_matches(self.remote_server_id.as_deref(), server_id)
    }

    /// The mux runtime that allocated this mirror's remote pane id, as
    /// recorded at construction. `None` when the connection had not yet
    /// learned a server identity.
    pub(crate) fn created_remote_server_id(&self) -> Option<&str> {
        self.remote_server_id.as_deref()
    }

    pub fn remote_pane_id(&self) -> PaneId {
        self.remote_pane_id
    }

    pub fn remote_tab_id(&self) -> TabId {
        self.remote_tab_id.load(Ordering::Relaxed)
    }

    pub fn remote_viewport_state(&self) -> Option<codec::ClientViewportState> {
        self.client.remote_viewport_state(self.remote_tab_id())
    }

    pub fn owns_remote_viewport(&self) -> Option<bool> {
        self.client.owns_remote_viewport(self.remote_tab_id())
    }

    pub fn remote_access_state(&self) -> Option<codec::FrontendAccessState> {
        self.client.remote_access_state()
    }

    pub fn has_remote_access(&self) -> Option<bool> {
        self.client.has_remote_access()
    }

    pub fn remote_frontend_gate(&self) -> crate::domain::RemoteFrontendGate {
        self.client.remote_frontend_gate()
    }

    pub(crate) fn set_remote_tab_id(&self, remote_tab_id: TabId) {
        self.remote_tab_id.store(remote_tab_id, Ordering::Relaxed);
    }

    pub(crate) fn belongs_to_client(&self, client: &Arc<ClientInner>) -> bool {
        Arc::ptr_eq(&self.client, client)
    }

    /// Ask the server to make this pane the visible (active) pane of the
    /// stack that contains it, mirroring a local level-2 tab switch. Without
    /// this, the next resync would flip the local stack back to the server's
    /// stale active pane.
    pub fn activate_in_stack_on_server(&self) {
        if self.client.remote_tab_input_is_blocked() {
            return;
        }
        let client = Arc::clone(&self.client);
        let remote_pane_id = self.remote_pane_id;
        let remote_tab_id = self.remote_tab_id();
        promise::spawn::spawn(async move {
            match client.prepare_remote_tab_input(remote_tab_id).await {
                Ok(true) => {
                    if let Err(err) = client
                        .client
                        .activate_pane_in_stack(codec::ActivatePaneInStack {
                            pane_id: remote_pane_id,
                        })
                        .await
                    {
                        log::error!("remote stack activation failed: {err:#}");
                    }
                }
                Ok(false) => {}
                Err(err) => log::error!("remote stack activation claim failed: {err:#}"),
            }
        })
        .detach();
    }

    /// Arrange to suppress the next Pane::kill call.
    ///
    /// ThinkTerm uses this when it intentionally discards a local mirror
    /// (for example, Disconnect or Delete Space) while leaving the pane alive
    /// on the mux server.  Native GUI closure can keep the whole mux window in
    /// the background instead, so it does not need this one-shot suppression.
    pub fn ignore_next_kill(&self) {
        *self.ignore_next_kill.lock() = true;
    }

    /// True when the renderable is sitting on displayed content the GUI
    /// has not painted: rows tagged Stale awaiting a re-fetch, or fetches
    /// in flight far longer than any healthy round trip (those are
    /// repaired so the triggered paint re-requests them, and the poll
    /// backoff is reset). `viewport_top` is the GUI's displayed viewport
    /// origin for this pane (None = following the tail).
    ///
    /// The mirror render loop (push → PaneOutput → invalidate → paint →
    /// poll/fetch) has no pulse of its own; the GUI's watchdog uses this to
    /// restart it instead of leaving the pane frozen until user input.
    pub fn render_looks_stalled(&self, viewport_top: Option<StableRowIndex>) -> bool {
        let renderable = self.renderable.lock();
        let mut inner = renderable.inner.borrow_mut();
        if !render_watchdog_should_run(inner.dead) {
            return false;
        }
        inner.watchdog_check_displayed_rows(viewport_top)
    }

    /// Adopt geometry that is carried by a complete frontend viewport RPC.
    ///
    /// The GUI has already computed the tab root and every split pane from a
    /// single layout pass.  Updating the local render surface here makes that
    /// geometry visible atomically with the viewport request, while recording
    /// the requested size prevents the normal `Pane::resize` path from
    /// following up with a duplicate per-pane Resize RPC.
    pub fn adopt_frontend_geometry(&self, size: TerminalSize) -> bool {
        *self.requested_size.lock() = Some(size);
        let render = self.renderable.lock();
        let mut inner = render.inner.borrow_mut();
        let changed = inner.apply_local_resize(size);
        if changed {
            inner.update_last_send();
        }
        changed
    }

    /// Pin the local render surface to a divider preview epoch. Unlike a
    /// normal adoption this deliberately preserves cached rows and ignores
    /// older server dimensions until the final full viewport is confirmed.
    pub fn preview_frontend_geometry(&self, epoch: u64, size: TerminalSize) -> bool {
        self.preview_frontend_geometry_with_policy(epoch, size, FrontendPreviewPolicy::PreserveRows)
    }

    /// Preview a divider that remains visible while it is moving. Retained
    /// rows are normalized to the new width and refetched rather than being
    /// interpreted as though their old cell storage already matched it.
    pub fn preview_live_frontend_geometry(&self, epoch: u64, size: TerminalSize) -> bool {
        self.preview_frontend_geometry_with_policy(epoch, size, FrontendPreviewPolicy::LiveResize)
    }

    fn preview_frontend_geometry_with_policy(
        &self,
        epoch: u64,
        size: TerminalSize,
        policy: FrontendPreviewPolicy,
    ) -> bool {
        *self.requested_size.lock() = Some(size);
        let render = self.renderable.lock();
        let mut inner = render.inner.borrow_mut();
        let changed = inner.begin_frontend_preview(epoch, size, policy);
        if changed {
            inner.update_last_send();
        }
        changed
    }

    pub fn server_geometry_matches(&self, size: TerminalSize) -> bool {
        self.renderable
            .lock()
            .inner
            .borrow()
            .server_geometry_matches(size)
    }

    pub fn finish_frontend_geometry_preview(
        &self,
        epoch: u64,
        size: TerminalSize,
        succeeded: bool,
    ) -> bool {
        let finished = self
            .renderable
            .lock()
            .inner
            .borrow_mut()
            .end_frontend_preview(epoch, succeeded);
        let mut requested = self.requested_size.lock();
        finish_preview_request(&mut requested, size, succeeded, finished);
        finished
    }

    pub fn is_remote_tardy(&self) -> bool {
        self.renderable.lock().inner.borrow().is_tardy()
    }

    /// Forget a geometry adoption when the complete viewport RPC failed.
    /// A later authoritative resync can then establish the server's actual
    /// dimensions instead of having the failed request remain a dedupe key.
    pub fn forget_frontend_geometry(&self, size: TerminalSize) {
        let mut requested = self.requested_size.lock();
        if requested.as_ref() == Some(&size) {
            requested.take();
        }
    }

    /// Prime and verify the remote render snapshot for an optimistically
    /// adopted frontend size.  GUI takeover uses this while its opaque mask is
    /// still present, so a full-screen application cannot expose its old grid
    /// or incremental redraw after the ownership RPC completes.
    pub fn prime_frontend_geometry(&self, size: TerminalSize) -> bool {
        self.renderable.lock().prime_frontend_geometry(size)
    }

    /// Which geometry fields still block a takeover from settling, or None
    /// once the server agrees.  Diagnostic only; see `mux::geometrytrace`.
    pub fn frontend_geometry_mismatch(&self, size: TerminalSize) -> Option<String> {
        self.renderable.lock().frontend_geometry_mismatch(size)
    }

    /// End the remote pane and wait for the mux server to acknowledge it.
    /// Destructive compound workflows use this instead of `Pane::kill`, whose
    /// fire-and-forget task can lose a race with detaching the last window.
    pub async fn kill_remote_and_wait(&self) -> anyhow::Result<()> {
        if !self
            .client
            .prepare_remote_tab_input(self.remote_tab_id())
            .await?
        {
            anyhow::bail!("terminal is being operated on another device");
        }
        self.client
            .client
            .kill_pane(KillPane {
                pane_id: self.remote_pane_id,
            })
            .await?;
        Ok(())
    }
}

fn finish_preview_request(
    requested: &mut Option<TerminalSize>,
    size: TerminalSize,
    succeeded: bool,
    finished: bool,
) {
    if finished && !succeeded && requested.as_ref() == Some(&size) {
        requested.take();
    }
}

impl ClientPane {
    /// Queue `input` behind everything this pane was given before it.
    fn queue_input(&self, input: PaneInput) -> anyhow::Result<()> {
        queue_pane_input(
            &self.client,
            self.remote_pane_id,
            &self.remote_tab_id,
            &self.inputs,
            input,
        )?;
        self.renderable.lock().inner.borrow_mut().update_last_send();
        Ok(())
    }
}

#[async_trait(?Send)]
impl Pane for ClientPane {
    fn pane_id(&self) -> PaneId {
        self.local_pane_id
    }

    fn get_metadata(&self) -> Value {
        let renderable = self.renderable.lock();
        let inner = renderable.inner.borrow();

        let mut map: BTreeMap<Value, Value> = BTreeMap::new();
        map.insert(
            Value::String("is_tardy".to_string()),
            Value::Bool(inner.is_tardy()),
        );
        map.insert(
            Value::String("since_last_response_ms".to_string()),
            Value::U64(inner.last_recv_time.elapsed().as_millis() as u64),
        );

        Value::Object(map.into())
    }

    fn get_cursor_position(&self) -> StableCursorPosition {
        self.renderable.lock().get_cursor_position()
    }

    fn get_dimensions(&self) -> RenderableDimensions {
        self.renderable.lock().get_dimensions()
    }

    fn with_lines_mut(&self, lines: Range<StableRowIndex>, with_lines: &mut dyn WithPaneLines) {
        mux::pane::impl_with_lines_via_get_lines(self, lines, with_lines);
    }

    fn for_each_logical_line_in_stable_range_mut(
        &self,
        lines: Range<StableRowIndex>,
        for_line: &mut dyn ForEachPaneLogicalLine,
    ) {
        mux::pane::impl_for_each_logical_line_via_get_logical_lines(self, lines, for_line);
    }

    fn get_lines(&self, lines: Range<StableRowIndex>) -> (StableRowIndex, Vec<Line>) {
        self.renderable.lock().get_lines(lines)
    }

    fn get_logical_lines(&self, lines: Range<StableRowIndex>) -> Vec<LogicalLine> {
        mux::pane::impl_get_logical_lines_via_get_lines(self, lines)
    }

    fn get_current_seqno(&self) -> SequenceNo {
        self.renderable.lock().get_current_seqno()
    }

    fn get_changed_since(
        &self,
        lines: Range<StableRowIndex>,
        seqno: SequenceNo,
    ) -> RangeSet<StableRowIndex> {
        self.renderable.lock().get_changed_since(lines, seqno)
    }

    fn set_clipboard(&self, clipboard: &Arc<dyn Clipboard>) {
        self.clipboard.lock().replace(Arc::clone(clipboard));
    }

    fn get_title(&self) -> String {
        let renderable = self.renderable.lock();
        let inner = renderable.inner.borrow();
        inner.title.clone()
    }

    fn get_progress(&self) -> Progress {
        self.progress.lock().clone()
    }

    fn agent_status(&self) -> Option<thinkterm_proto::AgentStatus> {
        self.agent_status.lock().clone()
    }

    fn send_paste(&self, text: &str) -> anyhow::Result<()> {
        if self.client.remote_tab_input_is_blocked() {
            return Ok(());
        }
        Mux::get().record_input_for_current_identity();
        self.queue_input(PaneInput::Paste(text.to_owned()))?;
        self.renderable
            .lock()
            .inner
            .borrow_mut()
            .predict_from_paste(text);
        Ok(())
    }

    fn reader(&self) -> anyhow::Result<Option<Box<dyn std::io::Read + Send>>> {
        Ok(None)
    }

    fn writer(&self) -> MappedMutexGuard<'_, dyn std::io::Write> {
        // Kitty-protocol and win32-input-mode keys arrive here rather than
        // through key_down, so this is where they count as input: for the
        // client list's idle time, and for the laggy-link indicator.
        Mux::get().record_input_for_current_identity();
        self.renderable.lock().inner.borrow_mut().update_last_send();
        MutexGuard::map(self.writer.lock(), |writer| {
            let w: &mut dyn std::io::Write = writer;
            w
        })
    }

    fn set_zoomed(&self, zoomed: bool) {
        let local_pane_id = self.local_pane_id;
        let remote_pane_id = self.remote_pane_id;
        let remote_tab_id = self.remote_tab_id();
        if self.client.remote_tab_input_is_blocked() {
            mux::zoom_trace!(
                "gui.zoom.skip pane={local_pane_id}/r{remote_pane_id} rtab={remote_tab_id} \
                 zoomed={zoomed} reason=input_blocked"
            );
            return;
        }
        let render = self.renderable.lock();
        let mut inner = render.inner.borrow_mut();
        let client = Arc::clone(&self.client);
        // Invalidate any cached rows on a resize
        inner.make_all_stale();
        promise::spawn::spawn(async move {
            // Traced separately from the send below: this await can be a full
            // claim round-trip (or refuse outright) while SetClientViewport
            // has no such gate, which is the suspected source of the zoom
            // state and the zoom geometry reaching the server out of order.
            let prepared = client.prepare_remote_tab_input(remote_tab_id).await;
            mux::zoom_trace!(
                "gui.zoom.prepared pane={local_pane_id}/r{remote_pane_id} rtab={remote_tab_id} \
                 zoomed={zoomed} gen={} owns={}",
                client.client.connection_generation(),
                match &prepared {
                    Ok(owns) => owns.to_string(),
                    Err(err) => format!("err({err:#})"),
                }
            );
            if prepared? {
                mux::zoom_trace!(
                    "gui.zoom.send pane={local_pane_id}/r{remote_pane_id} \
                     rtab={remote_tab_id} zoomed={zoomed}"
                );
                let result = client
                    .client
                    .set_zoomed(SetPaneZoomed {
                        containing_tab_id: remote_tab_id,
                        pane_id: remote_pane_id,
                        zoomed,
                    })
                    .await;
                mux::zoom_trace!(
                    "gui.zoom.ack pane={local_pane_id}/r{remote_pane_id} rtab={remote_tab_id} \
                     zoomed={zoomed} ok={}",
                    result.is_ok()
                );
                result?;
            }
            Ok::<(), anyhow::Error>(())
        })
        .detach();
        inner.update_last_send();
    }

    fn resize(&self, size: TerminalSize) -> anyhow::Result<()> {
        let (decision, prior_requested, advertised) = {
            // Dedupe against the last size WE requested, not against the
            // server-advertised dimensions: mid-split (or any server-side
            // relayout) the advertised dims legitimately disagree with the
            // GUI's still-stale layout for a moment, and re-asserting the
            // stale size would revert the server's pane resize and bake a
            // stale terminal surface. Split geometry is committed only by a
            // complete Native viewport carrying exact pane frames.
            let mut requested = self.requested_size.lock();
            let prior_requested = *requested;
            let render = self.renderable.lock();
            let mut inner = render.inner.borrow_mut();
            let advertised = inner.dimensions;
            let owns_viewport = self.client.owns_remote_viewport(self.remote_tab_id());
            let decision =
                decide_resize_for_viewport(owns_viewport, prior_requested, advertised, size);
            // A passive renderer displays the server's canonical grid. It
            // must neither reshape its local RenderableDimensions nor send a
            // resize that the server will reject; doing the former alone
            // creates a local/server invalidate loop and visible flicker.
            *requested = next_requested_size(owns_viewport, prior_requested, decision, size);

            if decision.converge_local_surface {
                inner.apply_local_resize(size);
            }
            if decision.send_rpc {
                inner.update_last_send();
            }
            (decision, prior_requested, advertised)
        };

        log::trace!(
            "pane {} resize {:?}: requested={:?} advertised={:?} target={:?}",
            self.local_pane_id,
            decision,
            prior_requested,
            advertised,
            size
        );

        if decision.send_rpc {
            let client = Arc::clone(&self.client);
            let remote_pane_id = self.remote_pane_id;
            let remote_tab_id = self.remote_tab_id();
            promise::spawn::spawn(async move {
                client
                    .client
                    .resize(Resize {
                        containing_tab_id: remote_tab_id,
                        pane_id: remote_pane_id,
                        size,
                    })
                    .await
            })
            .detach();
        }
        Ok(())
    }

    async fn search(
        &self,
        pattern: Pattern,
        range: Range<StableRowIndex>,
        limit: Option<u32>,
    ) -> anyhow::Result<Vec<SearchResult>> {
        match self
            .client
            .client
            .search_scrollback(SearchScrollbackRequest {
                pane_id: self.remote_pane_id,
                pattern,
                range,
                limit,
            })
            .await
        {
            Ok(SearchScrollbackResponse { results }) => Ok(results),
            Err(e) => Err(e),
        }
    }

    fn key_down(&self, key: KeyCode, mods: KeyModifiers) -> anyhow::Result<()> {
        if self.client.remote_tab_input_is_blocked() {
            return Ok(());
        }
        Mux::get().record_input_for_current_identity();
        let input_serial = InputSerial::now();
        self.queue_input(PaneInput::Key {
            event: KeyEvent {
                key,
                modifiers: mods,
            },
            input_serial,
        })?;
        let renderable = self.renderable.lock();
        let mut inner = renderable.inner.borrow_mut();
        inner.input_serial = input_serial;
        inner.predict_from_key_event(key, mods);
        Ok(())
    }

    fn key_up(&self, _key: KeyCode, _mods: KeyModifiers) -> anyhow::Result<()> {
        // Nothing to send, and that matches a local pane: the xterm-style
        // encoders emit no bytes for a release. The protocols that do
        // report releases (kitty, win32-input-mode) are encoded by the GUI
        // and written through `writer()`, once `get_keyboard_encoding`
        // tells it the remote program asked for them.
        Ok(())
    }

    fn kill(&self) {
        let mut ignore = self.ignore_next_kill.lock();
        if *ignore {
            *ignore = false;
            return;
        }
        if self.client.remote_tab_input_is_blocked() {
            return;
        }
        let client = Arc::clone(&self.client);
        let remote_pane_id = self.remote_pane_id;
        let remote_tab_id = self.remote_tab_id();
        let local_domain_id = self.client.local_domain_id;

        // We only want to ask the server to kill the pane if the user
        // explicitly requested it to die.
        // Domain detaching can implicitly call Pane::kill on the panes
        // in the domain, so we need to check here whether the domain is
        // in the detached state; if so then we must skip sending the
        // kill to the server.
        let mut send_kill = true;

        {
            let mux = Mux::get();
            if let Some(client_domain) = mux.get_domain(local_domain_id) {
                if client_domain.state() == mux::domain::DomainState::Detached {
                    send_kill = false;
                }
            }
        }

        if send_kill {
            promise::spawn::spawn(async move {
                if client.prepare_remote_tab_input(remote_tab_id).await? {
                    client
                        .client
                        .kill_pane(KillPane {
                            pane_id: remote_pane_id,
                        })
                        .await?;
                }
                Ok::<(), anyhow::Error>(())
            })
            .detach();
        }
    }

    fn mouse_event(&self, event: MouseEvent) -> anyhow::Result<()> {
        if self.client.remote_tab_input_is_blocked() {
            return Ok(());
        }
        Mux::get().record_input_for_current_identity();
        self.queue_input(PaneInput::Mouse(mousestate::normalize_wheel(event)))
    }

    fn is_dead(&self) -> bool {
        self.renderable.lock().inner.borrow().dead
    }

    fn palette(&self) -> ColorPalette {
        self.palette.lock().clone()
    }

    fn palette_override(&self) -> Option<ColorPalette> {
        if *self.application_palette.lock() {
            Some(self.palette.lock().clone())
        } else {
            None
        }
    }

    fn domain_id(&self) -> DomainId {
        self.client.local_domain_id
    }

    fn is_mouse_grabbed(&self) -> bool {
        *self.mouse_grabbed.lock()
    }

    fn is_alt_screen_active(&self) -> bool {
        *self.alt_screen.lock()
    }

    fn get_keyboard_encoding(&self) -> KeyboardEncoding {
        *self.keyboard_encoding.lock()
    }

    fn get_current_working_dir(&self, _policy: CachePolicy) -> Option<Url> {
        self.renderable.lock().inner.borrow().working_dir.clone()
    }

    fn focus_changed(&self, focused: bool) {
        if focused {
            self.advise_focus();
            *self.unseen_output.lock() = false;
        }
    }

    fn is_remote_mirror(&self) -> bool {
        true
    }

    fn erase_scrollback(&self, erase_mode: ScrollbackEraseMode) {
        if self.client.remote_tab_input_is_blocked() {
            return;
        }
        // In the queue with the keys: the server applies it in arrival
        // order, so the client sends it in the order it was asked.
        if let Err(err) = self.queue_input(PaneInput::EraseScrollback(erase_mode)) {
            log::warn!("erase scrollback of remote pane {}: {err:#}", self.remote_pane_id);
        }
    }

    fn advise_focus(&self) {
        if self.client.remote_tab_input_is_blocked() {
            return;
        }
        let mut focused_pane = self.client.focused_remote_pane_id.lock().unwrap();
        if *focused_pane != Some(self.remote_pane_id) {
            focused_pane.replace(self.remote_pane_id);
            self.client
                .focus_advised_at
                .lock()
                .unwrap()
                .replace(std::time::Instant::now());
            let client = Arc::clone(&self.client);
            let remote_pane_id = self.remote_pane_id;
            let configured_palette = Arc::clone(&self.configured_palette);
            let palette_rpc_lock = Arc::clone(&self.palette_rpc_lock);
            promise::spawn::spawn(async move {
                // SetPalette and this palette-bearing focus request both
                // mutate the same server-side advisory. Serialize them and
                // sample the palette only after acquiring the lock so an old
                // focus task cannot overwrite a newer config reload.
                let _guard = palette_rpc_lock.lock().await;
                let configured_palette = configured_palette.lock().clone();
                client
                    .client
                    .set_focused_pane_id(SetFocusedPane {
                        pane_id: remote_pane_id,
                        configured_palette: Some(configured_palette),
                    })
                    .await
            })
            .detach();
        }
    }

    fn has_unseen_output(&self) -> bool {
        *self.unseen_output.lock()
    }

    fn can_close_without_prompting(&self, reason: CloseReason) -> bool {
        match reason {
            CloseReason::Window => true,
            CloseReason::Tab => false,
            CloseReason::Pane => false,
        }
    }

    fn copy_user_vars(&self) -> HashMap<String, String> {
        self.user_vars.lock().clone()
    }

    fn set_config(&self, config: Arc<dyn TerminalConfiguration>) {
        let palette = config.color_palette();
        // Skip the send only when the SERVER is known to hold this exact
        // palette. "The value didn't change locally" is not that: the
        // initial advisory can be lost in attach races, and comparing
        // against our own memory would then skip the correction forever —
        // leaving the server answering OSC color queries from its bare
        // defaults. Identical re-sends stay flicker-free because both the
        // server and the receiving side de-duplicate them; the skip here
        // only avoids per-config-bump RPC noise once delivery is confirmed.
        let send = self.delivered_palette.lock().delivered.as_ref() != Some(&palette);

        // If the application running in the pane hasn't changed the
        // palette through escape sequences, speculatively adopt the
        // new palette so that it updates with the lowest latency.
        if !*self.application_palette.lock() {
            *self.palette.lock() = palette.clone();
        }
        *self.configured_palette.lock() = palette.clone();

        if send {
            Self::advise_server_palette(
                Arc::clone(&self.client),
                self.remote_pane_id,
                palette,
                Arc::clone(&self.delivered_palette),
                Arc::clone(&self.palette_rpc_lock),
            );
        }
        self.config.lock().replace(config);
    }

    fn get_config(&self) -> Option<Arc<dyn TerminalConfiguration>> {
        self.config.lock().clone()
    }
}

#[derive(Default)]
struct RenderDeltaQueue {
    pending: std::collections::VecDeque<GetPaneRenderChangesResponse>,
    draining: bool,
}

/// The rows named by the pushes still waiting in the queue: as bonus rows,
/// or as dirty ranges the client will fetch on its own.
#[derive(Default)]
struct RowsCarried {
    rows: std::collections::HashSet<StableRowIndex>,
    ranges: Vec<Range<StableRowIndex>>,
}

impl RowsCarried {
    fn contains(&self, row: StableRowIndex) -> bool {
        self.rows.contains(&row) || self.ranges.iter().any(|range| range.contains(&row))
    }
}

impl RenderDeltaQueue {
    /// Queue `delta`; true when the caller has to start the drain.
    fn push(&mut self, delta: GetPaneRenderChangesResponse) -> bool {
        self.pending.push_back(delta);
        !std::mem::replace(&mut self.draining, true)
    }

    /// The next push to apply, with the rows the pushes behind it carry
    /// when there are any, or None once the drain is over.
    fn take(&mut self) -> Option<(GetPaneRenderChangesResponse, Option<RowsCarried>)> {
        let delta = match self.pending.pop_front() {
            Some(delta) => delta,
            None => {
                self.draining = false;
                return None;
            }
        };
        if self.pending.is_empty() {
            return Some((delta, None));
        }
        let mut carried = RowsCarried::default();
        for newer in &self.pending {
            carried.rows.extend(newer.bonus_lines.rows());
            carried.ranges.extend(newer.dirty_lines.iter().cloned());
        }
        Some((delta, Some(carried)))
    }
}

/// Marks the drain over when its task ends, however it ends, so a task
/// dropped without finishing cannot leave every later push waiting.
struct DrainingDeltas(PaneId);

impl Drop for DrainingDeltas {
    fn drop(&mut self) {
        let Some(mux) = Mux::try_get() else {
            return;
        };
        if let Some(pane) = mux.get_pane(self.0) {
            if let Some(pane) = pane.downcast_ref::<ClientPane>() {
                pane.render_deltas.lock().draining = false;
            }
        }
    }
}

async fn drain_render_deltas(local_pane_id: PaneId) {
    let _draining = DrainingDeltas(local_pane_id);
    loop {
        let Some(pane) = Mux::get().get_pane(local_pane_id) else {
            return;
        };
        let Some(pane) = pane.downcast_ref::<ClientPane>() else {
            return;
        };
        let Some((delta, carried)) = pane.render_deltas.lock().take() else {
            return;
        };
        // A push with a newer one already behind it is applied without
        // asking for pictures: the newer push names the current ones, and
        // rows whose pictures are missing keep showing the previous frame.
        let started = std::time::Instant::now();
        let fetched_pictures = carried.is_none();
        pane.apply_render_delta(delta, carried).await;
        log::debug!(
            "render push for pane {local_pane_id} applied in {:?} (fetched pictures: {})",
            started.elapsed(),
            fetched_pictures
        );
    }
}

struct PaneWriter {
    client: Arc<ClientInner>,
    remote_pane_id: PaneId,
    remote_tab_id: Arc<AtomicUsize>,
    inputs: Arc<Mutex<InputQueue>>,
}

/// One thing the user gave a remote pane.
///
/// Keys, pastes and mouse reports travel as their own requests; the pty
/// bytes the GUI encodes itself (kitty-protocol and win32-input-mode keys,
/// text from the input method, SendString) go as WriteToPane. The four
/// used to leave by separate paths, and the byte path waited for each
/// answer before sending more, so a key pressed while a write was still
/// unanswered overtook the text queued behind that write: Enter before
/// the sentence it was meant to end. Everything now goes through one
/// queue per pane, in the order it was given.
enum PaneInput {
    Bytes(Vec<u8>),
    Key {
        event: KeyEvent,
        input_serial: InputSerial,
    },
    Paste(String),
    Mouse(MouseEvent),
    EraseScrollback(ScrollbackEraseMode),
}

/// What an input weighs against `INPUT_QUEUE_LIMIT` when it is all
/// bookkeeping.
const INPUT_ITEM_FLOOR: usize = 64;

impl PaneInput {
    fn weight(&self) -> usize {
        match self {
            PaneInput::Bytes(data) => data.len().max(INPUT_ITEM_FLOOR),
            PaneInput::Paste(text) => text.len().max(INPUT_ITEM_FLOOR),
            PaneInput::Key { .. } | PaneInput::Mouse(_) | PaneInput::EraseScrollback(_) => {
                INPUT_ITEM_FLOOR
            }
        }
    }

    fn describe(&self) -> String {
        match self {
            PaneInput::Bytes(data) => format!("{} bytes", data.len()),
            PaneInput::Key { .. } => "a key".to_string(),
            PaneInput::Paste(text) => format!("a paste of {} bytes", text.len()),
            PaneInput::Mouse(_) => "a mouse report".to_string(),
            PaneInput::EraseScrollback(_) => "a scrollback erase".to_string(),
        }
    }

    /// Fold `next` into this one when the two can travel as a single
    /// request: bytes after bytes, a mouse report over the one before it.
    /// Nothing folds across a key or a paste, so the order the user gave
    /// is the order the pty sees. Hands `next` back when it has to stay
    /// its own request.
    fn absorb(&mut self, next: PaneInput) -> Option<PaneInput> {
        match (self, next) {
            (PaneInput::Bytes(data), PaneInput::Bytes(more)) => {
                data.extend_from_slice(&more);
                None
            }
            (PaneInput::Mouse(last), PaneInput::Mouse(event)) => {
                mousestate::coalesce(last, event).map(PaneInput::Mouse)
            }
            (_, next) => Some(next),
        }
    }

    fn into_pdu(self, pane_id: PaneId) -> Pdu {
        match self {
            PaneInput::Bytes(data) => Pdu::WriteToPane(WriteToPane { pane_id, data }),
            PaneInput::Key {
                event,
                input_serial,
            } => Pdu::SendKeyDown(SendKeyDown {
                pane_id,
                event,
                input_serial,
            }),
            PaneInput::Paste(data) => Pdu::SendPaste(SendPaste { pane_id, data }),
            PaneInput::Mouse(event) => Pdu::SendMouseEvent(SendMouseEvent { pane_id, event }),
            PaneInput::EraseScrollback(erase_mode) => {
                Pdu::EraseScrollbackRequest(EraseScrollbackRequest {
                    pane_id,
                    erase_mode,
                })
            }
        }
    }
}

/// Input for a remote pane the server has not answered: what waits to be
/// sent, and how much is on the wire.
#[derive(Default)]
struct InputQueue {
    pending: VecDeque<PaneInput>,
    /// The weight of `pending`.
    queued: usize,
    /// The weight sent and not yet answered.
    in_flight: usize,
    draining: bool,
}

/// How much input may wait for the link, queued or sent and unanswered,
/// before more is refused: a paste, not a runaway.
const INPUT_QUEUE_LIMIT: usize = 4 * 1024 * 1024;

/// The queue is full: the link has answered nothing for a while and
/// `INPUT_QUEUE_LIMIT` bytes of input already wait for it.
#[derive(Debug)]
struct InputQueueFull {
    waiting: usize,
}

impl std::fmt::Display for InputQueueFull {
    fn fmt(&self, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            fmt,
            "{} bytes of input are already waiting for the link to the remote pane",
            self.waiting
        )
    }
}

impl std::error::Error for InputQueueFull {}

impl InputQueue {
    /// Queue `input` behind what already waits; true when the caller has
    /// to start the drain. Over the limit the input is refused rather than
    /// dropped and reported as sent: the caller is told, and what reaches
    /// the remote pane is either all of an input or none of it.
    fn push(&mut self, input: PaneInput) -> Result<bool, InputQueueFull> {
        let waiting = self.queued + self.in_flight;
        if waiting + input.weight() > INPUT_QUEUE_LIMIT {
            return Err(InputQueueFull { waiting });
        }
        let input = match self.pending.back_mut() {
            Some(last) => {
                let before = last.weight();
                let left = last.absorb(input);
                self.queued = self.queued - before + last.weight();
                left
            }
            None => Some(input),
        };
        if let Some(input) = input {
            self.queued += input.weight();
            self.pending.push_back(input);
        }
        Ok(!std::mem::replace(&mut self.draining, true))
    }

    /// The next input to send, counted as in flight from here on, or None
    /// once nothing waits: the drain is over and the next push starts a
    /// new one. The check and the hand-back happen under the one lock, so
    /// no push can slip between them and find the queue neither drained
    /// nor draining.
    fn take(&mut self) -> Option<PaneInput> {
        match self.pending.pop_front() {
            Some(input) => {
                let weight = input.weight();
                self.queued -= weight;
                self.in_flight += weight;
                Some(input)
            }
            None => {
                self.draining = false;
                None
            }
        }
    }

    /// `weight` of input was answered, or never went: it no longer waits.
    fn settle(&mut self, weight: usize) {
        self.in_flight = self.in_flight.saturating_sub(weight);
    }
}

/// Marks the drain over if its task ends before it reaches the end of the
/// queue (a panic caught by the executor, a scheduler torn down with it
/// queued); every later input would otherwise wait for a drain that never
/// comes. A drain that did reach the end handed the queue back itself and
/// disarms this, so it cannot undo a drain the next push already started.
struct InputDraining {
    queue: Arc<Mutex<InputQueue>>,
    done: bool,
}

impl Drop for InputDraining {
    fn drop(&mut self) {
        if !self.done {
            self.queue.lock().draining = false;
        }
    }
}

/// Settles an input's weight when its answer arrives, or when the answer
/// is given up on: dropped with the future, it settles all the same.
struct Settling {
    queue: Arc<Mutex<InputQueue>>,
    weight: usize,
}

impl Drop for Settling {
    fn drop(&mut self) {
        self.queue.lock().settle(self.weight);
    }
}

/// Where a drain sends: the connection, or a recorder under test.
trait InputLink: Send + Sync + 'static {
    /// Whether this client may drive the tab right now; a claim may have
    /// to reach the server first.
    fn prepare(&self, remote_tab_id: TabId) -> BoxFuture<'_, anyhow::Result<bool>>;

    /// Put the request on the wire now, behind everything sent before it;
    /// the future is its answer.
    fn send(&self, pdu: Pdu) -> BoxFuture<'static, anyhow::Result<Pdu>>;
}

impl InputLink for ClientInner {
    fn prepare(&self, remote_tab_id: TabId) -> BoxFuture<'_, anyhow::Result<bool>> {
        self.prepare_remote_tab_input(remote_tab_id).boxed()
    }

    fn send(&self, pdu: Pdu) -> BoxFuture<'static, anyhow::Result<Pdu>> {
        self.client.send_pdu_pipelined(pdu).boxed()
    }
}

/// Sends the pane's queued input in order, each request the moment the
/// one before it is on the wire, then waits for the answers. No request
/// waits for an answer before the next is sent: the wire keeps the order,
/// and the server applies one connection's input to a pane in the order
/// it arrives. The answers only settle what counts against
/// `INPUT_QUEUE_LIMIT`, so a stalled link ends in refused input, not in
/// input sent out of order.
async fn drain_pane_inputs<L: InputLink>(
    link: Arc<L>,
    remote_pane_id: PaneId,
    remote_tab_id: Arc<AtomicUsize>,
    queue: Arc<Mutex<InputQueue>>,
) {
    let mut answers = FuturesUnordered::new();
    let mut draining = InputDraining {
        queue: Arc::clone(&queue),
        done: false,
    };
    loop {
        let Some(input) = queue.lock().take() else {
            draining.done = true;
            break;
        };
        let what = input.describe();
        let settling = Settling {
            queue: Arc::clone(&queue),
            weight: input.weight(),
        };
        let remote_tab_id = remote_tab_id.load(Ordering::Relaxed);
        match link.prepare(remote_tab_id).await {
            Ok(true) => {}
            Ok(false) => {
                log::warn!(
                    "dropping {what} for remote pane {remote_pane_id}: this client may \
                     not drive the tab right now"
                );
                continue;
            }
            Err(err) => {
                log::error!("dropping {what} for remote pane {remote_pane_id}: {err:#}");
                continue;
            }
        }
        let answer = link.send(input.into_pdu(remote_pane_id));
        answers.push(async move {
            let _settling = settling;
            if let Err(err) = answered(answer.await) {
                log::error!("sending {what} to remote pane {remote_pane_id}: {err:#}");
            }
        });
    }
    drop(draining);
    while answers.next().await.is_some() {}
}

fn answered(answer: anyhow::Result<Pdu>) -> anyhow::Result<()> {
    match answer? {
        Pdu::UnitResponse(_) => Ok(()),
        Pdu::ErrorResponse(err) => bail!(err.reason),
        other => bail!("unexpected response {other:?}"),
    }
}

/// Queue `input` for the remote pane behind everything given before it,
/// and start the drain when none runs.
fn queue_pane_input(
    client: &Arc<ClientInner>,
    remote_pane_id: PaneId,
    remote_tab_id: &Arc<AtomicUsize>,
    inputs: &Arc<Mutex<InputQueue>>,
    input: PaneInput,
) -> Result<(), InputQueueFull> {
    let what = input.describe();
    let start_drain = inputs.lock().push(input).map_err(|full| {
        log::warn!("refusing {what} for remote pane {remote_pane_id}: {full}");
        full
    })?;
    if start_drain {
        promise::spawn::spawn_into_main_thread(drain_pane_inputs(
            Arc::clone(client),
            remote_pane_id,
            Arc::clone(remote_tab_id),
            Arc::clone(inputs),
        ))
        .detach();
    }
    Ok(())
}

impl std::io::Write for PaneWriter {
    fn write(&mut self, data: &[u8]) -> Result<usize, std::io::Error> {
        if self.client.remote_tab_input_is_blocked() {
            return Ok(data.len());
        }
        queue_pane_input(
            &self.client,
            self.remote_pane_id,
            &self.remote_tab_id,
            &self.inputs,
            PaneInput::Bytes(data.to_vec()),
        )
        .map_err(|full| std::io::Error::new(std::io::ErrorKind::WouldBlock, full))?;
        Ok(data.len())
    }

    fn flush(&mut self) -> Result<(), std::io::Error> {
        Ok(())
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

    #[test]
    fn render_pushes_are_taken_in_order_and_know_when_another_waits() {
        let mut queue = super::RenderDeltaQueue::default();
        let delta = |seqno| codec::GetPaneRenderChangesResponse {
            pane_id: 1,
            mouse_grabbed: false,
            alt_screen: false,
            keyboard_encoding: Default::default(),
            cursor_position: Default::default(),
            dimensions: Default::default(),
            dirty_lines: vec![],
            title: String::new(),
            working_dir: None,
            bonus_lines: Vec::new().into(),
            input_serial: None,
            seqno,
        };
        assert!(queue.push(delta(1)), "the first push starts the drain");
        assert!(!queue.push(delta(2)), "the second rides along");
        let (first, carried) = queue.take().unwrap();
        assert_eq!(first.seqno, 1);
        assert!(
            carried.is_some(),
            "so the first is applied without fetching pictures"
        );
        let (second, carried) = queue.take().unwrap();
        assert_eq!(second.seqno, 2);
        assert!(carried.is_none(), "the latest one fetches");
        assert!(queue.take().is_none(), "and the drain ends");
    }

    /// A push applied without pictures leaves rows out; the rows the
    /// pushes behind it name are the ones something else will bring.
    #[test]
    fn a_push_applied_without_pictures_knows_which_rows_the_newer_ones_carry() {
        use termwiz::surface::Line;
        let mut queue = super::RenderDeltaQueue::default();
        let delta =
            |seqno,
             bonus: Vec<wezterm_term::StableRowIndex>,
             dirty: Vec<std::ops::Range<wezterm_term::StableRowIndex>>| {
                codec::GetPaneRenderChangesResponse {
                    pane_id: 1,
                    mouse_grabbed: false,
                    alt_screen: false,
                    keyboard_encoding: Default::default(),
                    cursor_position: Default::default(),
                    dimensions: Default::default(),
                    dirty_lines: dirty,
                    title: String::new(),
                    working_dir: None,
                    bonus_lines: bonus
                        .into_iter()
                        .map(|row| (row, Line::with_width(1, 0)))
                        .collect::<Vec<_>>()
                        .into(),
                    input_serial: None,
                    seqno,
                }
            };
        queue.push(delta(1, vec![3, 4], vec![]));
        queue.push(delta(2, vec![4], vec![10..12]));
        queue.push(delta(3, vec![7], vec![]));
        let (_, carried) = queue.take().unwrap();
        let carried = carried.expect("two pushes wait behind the first");
        assert!(carried.contains(4), "a bonus row of a later push");
        assert!(carried.contains(7));
        assert!(carried.contains(11), "a dirty row of a later push");
        assert!(
            !carried.contains(3),
            "row 3 is only in the push being applied: left out, it must be marked dirty"
        );
        assert!(!carried.contains(12), "ranges are half-open");
    }

}

#[cfg(test)]
mod input_queue_tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use wezterm_term::{MouseButton, MouseEventKind};

    fn bytes(text: &str) -> PaneInput {
        PaneInput::Bytes(text.as_bytes().to_vec())
    }

    fn enter() -> PaneInput {
        PaneInput::Key {
            event: KeyEvent {
                key: KeyCode::Enter,
                modifiers: KeyModifiers::NONE,
            },
            input_serial: InputSerial::empty(),
        }
    }

    fn mouse(kind: MouseEventKind, button: MouseButton, x: usize) -> PaneInput {
        PaneInput::Mouse(MouseEvent {
            kind,
            x,
            y: 0,
            x_pixel_offset: 0,
            y_pixel_offset: 0,
            button,
            modifiers: KeyModifiers::NONE,
        })
    }

    fn tag(input: &PaneInput) -> String {
        match input {
            PaneInput::Bytes(data) => format!("bytes:{}", String::from_utf8_lossy(data)),
            PaneInput::Key { .. } => "key".to_string(),
            PaneInput::Paste(text) => format!("paste:{text}"),
            PaneInput::EraseScrollback(_) => "erase".to_string(),
            PaneInput::Mouse(event) => match (&event.kind, &event.button) {
                (MouseEventKind::Move, _) => format!("mouse:move@{}", event.x),
                (_, MouseButton::WheelDown(n)) => format!("mouse:wheeldown{n}"),
                _ => "mouse".to_string(),
            },
        }
    }

    fn pdu_tag(pdu: &Pdu) -> String {
        match pdu {
            Pdu::WriteToPane(write) => {
                format!("bytes:{}", String::from_utf8_lossy(&write.data))
            }
            Pdu::SendKeyDown(_) => "key".to_string(),
            Pdu::SendPaste(paste) => format!("paste:{}", paste.data),
            Pdu::SendMouseEvent(_) => "mouse".to_string(),
            other => format!("{other:?}"),
        }
    }

    /// A link that records what went on the wire and answers only when
    /// the test says so.
    #[derive(Default)]
    struct RecordingLink {
        wire: Mutex<Vec<String>>,
        answers: Mutex<Vec<smol::channel::Sender<anyhow::Result<Pdu>>>>,
        refuse: AtomicBool,
    }

    impl RecordingLink {
        fn answer_everything(&self) {
            for answer in self.answers.lock().drain(..) {
                answer
                    .try_send(Ok(Pdu::UnitResponse(UnitResponse {})))
                    .unwrap();
            }
        }
    }

    impl InputLink for RecordingLink {
        fn prepare(&self, _remote_tab_id: TabId) -> BoxFuture<'_, anyhow::Result<bool>> {
            let allowed = !self.refuse.load(Ordering::SeqCst);
            async move { Ok(allowed) }.boxed()
        }

        fn send(&self, pdu: Pdu) -> BoxFuture<'static, anyhow::Result<Pdu>> {
            self.wire.lock().push(pdu_tag(&pdu));
            let (tx, rx) = smol::channel::bounded(1);
            self.answers.lock().push(tx);
            async move {
                rx.recv()
                    .await
                    .map_err(|_| anyhow::anyhow!("the answer was dropped"))?
            }
            .boxed()
        }
    }

    /// The scenario that was wrong: text from the input method is on the
    /// wire and unanswered, more text arrives, then Enter. Enter must leave
    /// after that text, and nothing may wait for the first answer.
    #[test]
    fn enter_leaves_behind_the_text_queued_before_it_without_waiting_for_an_answer() {
        let link = Arc::new(RecordingLink::default());
        let queue: Arc<Mutex<InputQueue>> = Default::default();
        let tab = Arc::new(AtomicUsize::new(0));
        let ex = smol::LocalExecutor::new();
        let drain = || {
            ex.spawn(drain_pane_inputs(
                Arc::clone(&link),
                7,
                Arc::clone(&tab),
                Arc::clone(&queue),
            ))
        };

        assert!(queue.lock().push(bytes("ni")).unwrap());
        let first = drain();
        while ex.try_tick() {}
        assert_eq!(*link.wire.lock(), ["bytes:ni"], "sent at once");
        assert_eq!(queue.lock().in_flight, INPUT_ITEM_FLOOR, "and unanswered");
        assert!(
            !queue.lock().draining,
            "an unanswered request does not hold the drain"
        );

        assert!(queue.lock().push(bytes("hao")).unwrap(), "a new drain starts");
        assert!(!queue.lock().push(enter()).unwrap(), "and Enter rides along");
        let second = drain();
        while ex.try_tick() {}
        assert_eq!(
            *link.wire.lock(),
            ["bytes:ni", "bytes:hao", "key"],
            "Enter left after the text, while ni is still unanswered"
        );
        assert_eq!(queue.lock().in_flight, 3 * INPUT_ITEM_FLOOR);

        link.answer_everything();
        smol::block_on(ex.run(async {
            first.await;
            second.await;
        }));
        assert_eq!(queue.lock().in_flight, 0, "every answer settled its input");
    }

    #[test]
    fn input_this_client_may_not_send_is_dropped_and_settled() {
        let link = Arc::new(RecordingLink::default());
        link.refuse.store(true, Ordering::SeqCst);
        let queue: Arc<Mutex<InputQueue>> = Default::default();
        assert!(queue.lock().push(enter()).unwrap());
        let ex = smol::LocalExecutor::new();
        smol::block_on(ex.run(drain_pane_inputs(
            Arc::clone(&link),
            7,
            Arc::new(AtomicUsize::new(0)),
            Arc::clone(&queue),
        )));
        assert!(link.wire.lock().is_empty(), "nothing went");
        let queue = queue.lock();
        assert_eq!(queue.in_flight, 0, "and nothing is counted as waiting");
        assert!(!queue.draining, "the drain is over");
    }

    #[test]
    fn only_neighbours_of_the_same_kind_travel_together() {
        let mut queue = InputQueue::default();
        queue.push(bytes("a")).unwrap();
        queue.push(bytes("b")).unwrap();
        queue.push(enter()).unwrap();
        queue.push(bytes("c")).unwrap();
        queue
            .push(mouse(MouseEventKind::Move, MouseButton::None, 1))
            .unwrap();
        queue
            .push(mouse(MouseEventKind::Move, MouseButton::None, 2))
            .unwrap();
        queue.push(PaneInput::Paste("p".to_string())).unwrap();
        queue
            .push(mouse(MouseEventKind::Press, MouseButton::WheelDown(3), 0))
            .unwrap();
        queue
            .push(mouse(MouseEventKind::Press, MouseButton::WheelDown(1), 0))
            .unwrap();
        assert_eq!(
            queue.queued,
            queue.pending.iter().map(PaneInput::weight).sum::<usize>(),
            "folding keeps the account right"
        );
        let sent: Vec<String> = std::iter::from_fn(|| queue.take())
            .map(|input| tag(&input))
            .collect();
        assert_eq!(
            sent,
            [
                "bytes:ab",
                "key",
                "bytes:c",
                "mouse:move@2",
                "paste:p",
                "mouse:wheeldown4"
            ]
        );
    }

    #[test]
    fn input_on_the_wire_counts_until_it_is_answered() {
        let mut queue = InputQueue::default();
        assert!(
            queue
                .push(PaneInput::Bytes(vec![b'x'; INPUT_QUEUE_LIMIT]))
                .is_ok(),
            "the limit itself fits"
        );
        let refused = queue.push(enter()).expect_err("a key on top of it does not");
        assert_eq!(refused.waiting, INPUT_QUEUE_LIMIT);
        assert_eq!(
            queue.take().map(|input| input.weight()),
            Some(INPUT_QUEUE_LIMIT),
            "what was queued is intact; the refused key is not in it"
        );
        assert!(
            queue.push(enter()).is_err(),
            "on the wire and unanswered, it still counts"
        );
        queue.settle(INPUT_QUEUE_LIMIT);
        assert!(queue.push(enter()).is_ok(), "answered, it makes room");
    }

    #[test]
    fn a_drain_that_ends_early_gives_the_queue_back() {
        let queue: Arc<Mutex<InputQueue>> = Default::default();
        assert!(queue.lock().push(bytes("a")).unwrap());
        drop(InputDraining {
            queue: Arc::clone(&queue),
            done: false,
        });
        assert!(!queue.lock().draining, "the slot is free again");
        assert_eq!(
            tag(&queue.lock().take().unwrap()),
            "bytes:a",
            "and the bytes still wait for the next drain"
        );
    }

    #[test]
    fn a_drain_that_reached_the_end_does_not_undo_the_next_one() {
        let queue: Arc<Mutex<InputQueue>> = Default::default();
        assert!(queue.lock().push(bytes("a")).unwrap());
        assert!(queue.lock().take().is_some());
        assert!(queue.lock().take().is_none(), "handed back under the lock");
        assert!(
            queue.lock().push(bytes("b")).unwrap(),
            "the next push starts a new drain"
        );
        drop(InputDraining {
            queue: Arc::clone(&queue),
            done: true,
        });
        assert!(queue.lock().draining, "the finished drain's guard is disarmed");
    }
}
