use crate::domain::ClientInner;
use crate::pane::mousestate::MouseState;
use crate::pane::renderable::{
    hydrate_lines, FrontendPreviewPolicy, RenderableInner, RenderableState,
};
use anyhow::bail;
use async_trait::async_trait;
use codec::*;
use config::configuration;
use config::keyassignment::ScrollbackEraseMode;
use futures::lock::Mutex as AsyncMutex;
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
use std::collections::{BTreeMap, HashMap};
use std::ops::Range;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use termwiz::input::KeyEvent;
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

fn remote_server_identity_matches(created: Option<&str>, current: Option<&str>) -> bool {
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
    mouse: Arc<Mutex<MouseState>>,
    clipboard: Mutex<Option<Arc<dyn Clipboard>>>,
    mouse_grabbed: Mutex<bool>,
    alt_screen: Mutex<bool>,
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
        let writer = PaneWriter {
            client: Arc::clone(client),
            remote_pane_id,
            remote_tab_id: Arc::clone(&remote_tab_id),
        };

        let mouse = Arc::new(Mutex::new(MouseState::new(
            remote_pane_id,
            Arc::clone(client),
            Arc::clone(&remote_tab_id),
        )));

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
            mouse,
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

    pub async fn process_unilateral(&self, pdu: Pdu) -> anyhow::Result<()> {
        match pdu {
            Pdu::GetPaneRenderChangesResponse(mut delta) => {
                *self.mouse_grabbed.lock() = delta.mouse_grabbed;
                *self.alt_screen.lock() = delta.alt_screen;

                let bonus_lines = std::mem::take(&mut delta.bonus_lines);
                let client = { Arc::clone(&self.renderable.lock().inner.borrow().client) };
                let bonus_lines = hydrate_lines(client, delta.pane_id, bonus_lines).await;

                self.renderable
                    .lock()
                    .inner
                    .borrow_mut()
                    .apply_changes_to_surface(delta, bonus_lines);
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
        let client = Arc::clone(&self.client);
        let remote_pane_id = self.remote_pane_id;
        let remote_tab_id = self.remote_tab_id();
        self.renderable
            .lock()
            .inner
            .borrow_mut()
            .predict_from_paste(text);

        let data = text.to_owned();
        promise::spawn::spawn(async move {
            if client.prepare_remote_tab_input(remote_tab_id).await? {
                client
                    .client
                    .send_paste(SendPaste {
                        pane_id: remote_pane_id,
                        data,
                    })
                    .await?;
            }
            Ok::<(), anyhow::Error>(())
        })
        .detach();
        self.renderable.lock().inner.borrow_mut().update_last_send();
        Ok(())
    }

    fn reader(&self) -> anyhow::Result<Option<Box<dyn std::io::Read + Send>>> {
        Ok(None)
    }

    fn writer(&self) -> MappedMutexGuard<'_, dyn std::io::Write> {
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
            let decision = decide_resize_for_viewport(
                self.client.owns_remote_viewport(self.remote_tab_id()),
                prior_requested,
                advertised,
                size,
            );
            // A passive renderer displays the server's canonical grid. It
            // must neither reshape its local RenderableDimensions nor send a
            // resize that the server will reject; doing the former alone
            // creates a local/server invalidate loop and visible flicker.
            requested.replace(size);

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
        let input_serial;
        {
            let renderable = self.renderable.lock();
            let mut inner = renderable.inner.borrow_mut();
            inner.input_serial = InputSerial::now();
            input_serial = inner.input_serial;
            inner.predict_from_key_event(key, mods);
        }
        let client = Arc::clone(&self.client);
        let remote_pane_id = self.remote_pane_id;
        let remote_tab_id = self.remote_tab_id();
        promise::spawn::spawn(async move {
            if client.prepare_remote_tab_input(remote_tab_id).await? {
                client
                    .client
                    .key_down(SendKeyDown {
                        pane_id: remote_pane_id,
                        event: KeyEvent {
                            key,
                            modifiers: mods,
                        },
                        input_serial,
                    })
                    .await?;
            }
            Ok::<(), anyhow::Error>(())
        })
        .detach();
        self.renderable.lock().inner.borrow_mut().update_last_send();
        Ok(())
    }

    fn key_up(&self, _key: KeyCode, _mods: KeyModifiers) -> anyhow::Result<()> {
        // TODO: decide how to handle key_up for mux client
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
        self.mouse.lock().append(event);
        if MouseState::next(Arc::clone(&self.mouse)) {
            self.renderable.lock().inner.borrow_mut().update_last_send();
        }
        Ok(())
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
        let client = Arc::clone(&self.client);
        let remote_pane_id = self.remote_pane_id;
        let remote_tab_id = self.remote_tab_id();
        promise::spawn::spawn(async move {
            if client.prepare_remote_tab_input(remote_tab_id).await? {
                client
                    .client
                    .erase_scrollback(EraseScrollbackRequest {
                        pane_id: remote_pane_id,
                        erase_mode,
                    })
                    .await?;
            }
            Ok::<(), anyhow::Error>(())
        })
        .detach();
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

struct PaneWriter {
    client: Arc<ClientInner>,
    remote_pane_id: TabId,
    remote_tab_id: Arc<AtomicUsize>,
}

impl std::io::Write for PaneWriter {
    fn write(&mut self, data: &[u8]) -> Result<usize, std::io::Error> {
        if self.client.remote_tab_input_is_blocked() {
            return Ok(data.len());
        }
        let remote_tab_id = self.remote_tab_id.load(Ordering::Relaxed);
        let payload = data.to_vec();
        promise::spawn::block_on(async {
            if self.client.prepare_remote_tab_input(remote_tab_id).await? {
                self.client
                    .client
                    .write_to_pane(WriteToPane {
                        pane_id: self.remote_pane_id,
                        data: payload,
                    })
                    .await?;
            }
            Ok::<(), anyhow::Error>(())
        })
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, format!("{}", e)))?;
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

    #[test]
    fn stale_preview_completion_cannot_clear_a_newer_requested_size() {
        let geometry = size(120, 40, 96);
        let mut requested = Some(geometry);

        finish_preview_request(&mut requested, geometry, false, false);
        assert_eq!(requested, Some(geometry));

        finish_preview_request(&mut requested, geometry, false, true);
        assert_eq!(requested, None);
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
