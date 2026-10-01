use crate::domain::ClientInner;
use crate::pane::events::DesktopHost;
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
use mux::Mux;
use parking_lot::{MappedMutexGuard, Mutex, MutexGuard};
use rangeset::RangeSet;
use std::collections::{BTreeMap, HashMap};
use std::ops::Range;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use termwiz::input::KeyboardEncoding;
use termwiz::surface::SequenceNo;
use thinkterm_session::clock::Clock;
use thinkterm_session::decide::{
    application_palette_transition, decide_resize_for_viewport, finish_preview_request,
    next_requested_size, remote_server_identity_matches, PaletteDelivery,
};
use thinkterm_session::host::{SessionEvents, SessionHost};
use thinkterm_session::input::PaneInput;
use thinkterm_session::lines::FrontendPreviewPolicy;
use thinkterm_session::mouse;
use thinkterm_session::pane::PaneSession;
use thinkterm_session::SessionConfig;
use url::Url;
use wezterm_dynamic::Value;
use wezterm_term::color::ColorPalette;
use wezterm_term::{
    Alert, Clipboard, KeyCode, KeyModifiers, Line, MouseEvent, Progress, StableRowIndex,
    TerminalConfiguration, TerminalSize,
};

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
    /// This process as the session sees it: the mux, the clocks, the
    /// connection.
    host: Arc<DesktopHost>,
    /// The mirrored pane itself: rows, cursor, geometry, pushes, fetches.
    session: Arc<PaneSession<DesktopHost>>,
    clipboard: Mutex<Option<Arc<dyn Clipboard>>>,
    requested_size: Mutex<Option<TerminalSize>>,
    ignore_next_kill: Mutex<bool>,
    user_vars: Mutex<HashMap<String, String>>,
    config: Mutex<Option<Arc<dyn TerminalConfiguration>>>,
    unseen_output: Mutex<bool>,
    progress: Mutex<Progress>,
    agent_status: Mutex<Option<thinkterm_proto::AgentStatus>>,
    foreground_program: Mutex<Option<thinkterm_proto::ForegroundProgram>>,
    kitty_relay: Mutex<Option<Arc<super::kitty::Relay>>>,
}

impl ClientPane {
    /// Store a status delivered outside the unilateral push path (the
    /// cold-start fetch on attach/resync). Quiet: the caller decides
    /// whether a repaint is warranted.
    pub fn set_agent_status(&self, status: Option<thinkterm_proto::AgentStatus>) {
        *self.agent_status.lock() = status.filter(|s| s.within_budget());
    }

    /// The same, for the foreground program.
    pub fn set_foreground_program(&self, program: Option<thinkterm_proto::ForegroundProgram>) {
        *self.foreground_program.lock() = program.filter(|program| program.within_budget());
    }

    pub fn reconnect_kitty_frames(&self) {
        let relay = self.kitty_relay.lock().clone();
        if let Some(relay) = relay {
            relay.reconnected();
        }
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

        let session_config = SessionConfig {
            scrollback_lines: configuration().scrollback_lines,
            local_echo_threshold_ms: client.local_echo_threshold_ms,
            overlay_lag_indicator: client.overlay_lag_indicator,
        };
        let host = Arc::new(DesktopHost::new(Arc::clone(client)));
        let session = PaneSession::new(
            Arc::clone(&host),
            crate::pane::events::shared_image_store(),
            session_config,
            remote_pane_id,
            Arc::clone(&remote_tab_id),
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
            alt_screen,
        );
        let writer = PaneWriter {
            client: Arc::clone(client),
            session: Arc::clone(&session),
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
            host,
            session,
            remote_pane_id,
            local_pane_id,
            remote_tab_id,
            application_palette: Mutex::new(false),
            writer: Mutex::new(writer),
            configured_palette: Arc::new(Mutex::new(palette.clone())),
            delivered_palette,
            palette_rpc_lock,
            palette: Mutex::new(palette),
            clipboard: Mutex::new(None),
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
            foreground_program: Mutex::new(client.remote_foreground_program(remote_pane_id)),
            kitty_relay: Mutex::new(None),
        }
    }

    pub async fn process_unilateral(&self, pdu: Pdu) -> anyhow::Result<()> {
        match pdu {
            Pdu::KittyFrameSelections(state) => {
                let relay = self.kitty_relay.lock().clone();
                if let Some(relay) = relay {
                    relay.receive(state)?;
                }
            }
            Pdu::GetPaneRenderChangesResponse(delta) => {
                // Queued, and applied one at a time in arrival order by a
                // single task. Each push used to be its own task that
                // awaited the images it named; several in flight at once
                // finished in whatever order their fetches did, so an older
                // push could land after a newer one and roll the rows back,
                // and a program streaming pictures had every push fetching
                // a frame that was already stale.
                self.session.queue_render_delta(delta);
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
                    self.session.make_all_stale();
                }

                // Provenance changes must propagate through chained muxes even
                // when the effective colors happen to be identical. Only an
                // actual color change invalidates the render surface above.
                if transition.palette_changed || transition.provenance_changed {
                    self.host
                        .events()
                        .alert(self.local_pane_id, Alert::PaletteChanged);
                }
            }
            Pdu::NotifyAlert(NotifyAlert { mut alert, .. }) => {
                if let Alert::SetUserVar { name, value } = &mut alert {
                    wezterm_term::agent_contract::sanitize_agent_user_var(name, value);
                }
                match &alert {
                    Alert::SetUserVar { name, value } => {
                        self.user_vars.lock().insert(name.clone(), value.clone());
                    }
                    Alert::OutputSinceFocusLost => {
                        *self.unseen_output.lock() = true;
                        self.host
                            .events()
                            .alert(self.local_pane_id, Alert::OutputSinceFocusLost);
                    }
                    Alert::Progress(progress) => {
                        *self.progress.lock() = progress.clone();
                        self.host
                            .events()
                            .alert(self.local_pane_id, Alert::Progress(progress.clone()));
                    }
                    _ => {}
                }
                self.host.events().alert(self.local_pane_id, alert);
            }
            Pdu::AgentStatusChanged(codec::AgentStatusChanged { status, .. }) => {
                let status = status.filter(|s| s.within_budget());
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
                    self.host.events().agent_status_changed(self.local_pane_id);
                }
            }
            Pdu::ForegroundProgramChanged(codec::ForegroundProgramChanged { program, .. }) => {
                let program = program.filter(|program| program.within_budget());
                // Coalesced at send time like the agent status: only a real
                // change is worth a repaint, or a forward one hop on.
                let changed = {
                    let mut slot = self.foreground_program.lock();
                    if *slot == program {
                        false
                    } else {
                        *slot = program;
                        true
                    }
                };
                if changed {
                    self.host
                        .events()
                        .foreground_program_changed(self.local_pane_id);
                }
            }
            Pdu::PaneRemoved(PaneRemoved { pane_id }) => {
                log::trace!("remote pane {} has been removed", pane_id);
                self.session.set_dead(true);
                // The prune below can be deferred (activity in flight, or
                // the windows lock contended); the dead mirror must not
                // keep reporting an agent to the panel meanwhile.
                *self.agent_status.lock() = None;
                *self.foreground_program.lock() = None;
                self.host.events().pane_removed(self.local_pane_id);
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
                self.host.events().pane_focused(self.local_pane_id);
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
        self.session.render_looks_stalled(viewport_top)
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
        let changed = self.session.apply_local_resize(size);
        if changed {
            self.session.update_last_send();
        }
        changed
    }

    /// Send `size` again, as a plain per-pane Resize, once the server has
    /// kept this pane at another size for a while. Returns how long until
    /// the caller should check again while that is pending: a pane that
    /// stopped drawing may not paint by itself.
    ///
    /// Adoption only reshapes the local surface and leaves the server to the
    /// viewport report, which the server skips when it repeats the last one.
    /// A size the server changed on its own since was then never put back,
    /// and each push and paint flipped the surface between the two sizes.
    pub fn resend_frontend_geometry_if_ignored(
        &self,
        size: TerminalSize,
    ) -> Option<std::time::Duration> {
        let (resend, recheck_in) = self.session.server_geometry_repair(size);
        if !resend {
            return recheck_in;
        }
        let client = Arc::clone(&self.client);
        let remote_pane_id = self.remote_pane_id;
        let remote_tab_id = self.remote_tab_id();
        log::info!(
            "the server kept pane {} at another size than {}x{}; sending it again",
            self.local_pane_id,
            size.cols,
            size.rows
        );
        promise::spawn::spawn(async move {
            if let Err(err) = client
                .client
                .resize(Resize {
                    containing_tab_id: remote_tab_id,
                    pane_id: remote_pane_id,
                    size,
                })
                .await
            {
                log::warn!("resending the size of remote pane {remote_pane_id}: {err:#}");
            }
            Ok::<(), anyhow::Error>(())
        })
        .detach();
        recheck_in
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
        let changed = self.session.begin_frontend_preview(epoch, size, policy);
        if changed {
            self.session.update_last_send();
        }
        changed
    }

    pub fn server_geometry_matches(&self, size: TerminalSize) -> bool {
        self.session.server_geometry_matches(size)
    }

    pub fn finish_frontend_geometry_preview(
        &self,
        epoch: u64,
        size: TerminalSize,
        succeeded: bool,
    ) -> bool {
        let finished = self.session.end_frontend_preview(epoch, succeeded);
        let mut requested = self.requested_size.lock();
        finish_preview_request(&mut requested, size, succeeded, finished);
        finished
    }

    pub fn is_remote_tardy(&self) -> bool {
        self.session.is_tardy()
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
        self.session.prime_frontend_geometry(size)
    }

    /// Which geometry fields still block a takeover from settling, or None
    /// once the server agrees.  Diagnostic only; see `mux::geometrytrace`.
    pub fn frontend_geometry_mismatch(&self, size: TerminalSize) -> Option<String> {
        self.session.frontend_geometry_mismatch(size)
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

impl ClientPane {
    /// Queue `input` behind everything this pane was given before it.
    fn queue_input(&self, input: PaneInput) -> anyhow::Result<()> {
        if self.session.queue_input(input)? {
            start_input_drain(&self.session);
        }
        self.session.update_last_send();
        Ok(())
    }
}

#[async_trait(?Send)]
impl Pane for ClientPane {
    fn subscribe_kitty_frames(&self) -> Option<Box<dyn mux::pane::KittyFrameSubscription>> {
        let relay = Arc::clone(self.kitty_relay.lock().get_or_insert_with(|| super::kitty::Relay::new(
            &self.client, self.local_pane_id, self.remote_pane_id,
        )));
        Some(relay.subscribe())
    }

    fn kitty_frame_selections(&self, known: Option<u64>) -> Option<(u64, u64, Vec<wezterm_term::KittyFrameSelection>)> {
        match self.kitty_relay.lock().as_ref() {
            Some(relay) => relay.snapshot(known),
            None => (known != Some(0)).then(|| (0, 0, Vec::new())),
        }
    }

    fn get_kitty_image(&self, request: codec::GetKittyImage) -> mux::pane::KittyImageFuture {
        let relay = self.kitty_relay.lock().clone();
        let client = Arc::clone(&self.client);
        let remote_pane = self.remote_pane_id;
        Box::pin(async move {
            super::kitty::fetch_image(client, relay, remote_pane, request).await
        })
    }

    fn pane_id(&self) -> PaneId {
        self.local_pane_id
    }

    fn get_metadata(&self) -> Value {
        let mut map: BTreeMap<Value, Value> = BTreeMap::new();
        map.insert(
            Value::String("is_tardy".to_string()),
            Value::Bool(self.session.is_tardy()),
        );
        map.insert(
            Value::String("since_last_response_ms".to_string()),
            Value::U64(self.session.since_last_response().as_millis() as u64),
        );

        Value::Object(map.into())
    }

    fn get_cursor_position(&self) -> StableCursorPosition {
        self.session.cursor_position()
    }

    fn get_dimensions(&self) -> RenderableDimensions {
        self.session.dimensions()
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
        self.session.get_lines(lines)
    }

    fn get_logical_lines(&self, lines: Range<StableRowIndex>) -> Vec<LogicalLine> {
        mux::pane::impl_get_logical_lines_via_get_lines(self, lines)
    }

    fn get_current_seqno(&self) -> SequenceNo {
        self.session.current_seqno()
    }

    fn get_changed_since(
        &self,
        lines: Range<StableRowIndex>,
        seqno: SequenceNo,
    ) -> RangeSet<StableRowIndex> {
        self.session.changed_since(lines, seqno)
    }

    fn set_clipboard(&self, clipboard: &Arc<dyn Clipboard>) {
        self.clipboard.lock().replace(Arc::clone(clipboard));
    }

    fn get_title(&self) -> String {
        self.session.title()
    }

    fn get_progress(&self) -> Progress {
        self.progress.lock().clone()
    }

    fn agent_status(&self) -> Option<thinkterm_proto::AgentStatus> {
        self.agent_status.lock().clone()
    }

    fn foreground_program(&self) -> Option<thinkterm_proto::ForegroundProgram> {
        self.foreground_program.lock().clone()
    }

    fn send_paste(&self, text: &str) -> anyhow::Result<()> {
        if self.client.remote_tab_input_is_blocked() {
            return Ok(());
        }
        self.host.events().input_recorded();
        if self.session.paste(text)? {
            start_input_drain(&self.session);
        }
        self.session.update_last_send();
        Ok(())
    }

    fn reader(&self) -> anyhow::Result<Option<Box<dyn std::io::Read + Send>>> {
        Ok(None)
    }

    fn writer(&self) -> MappedMutexGuard<'_, dyn std::io::Write> {
        // Kitty-protocol and win32-input-mode keys arrive here rather than
        // through key_down, so this is where they count as input: for the
        // client list's idle time, and for the laggy-link indicator.
        self.host.events().input_recorded();
        self.session.update_last_send();
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
        let client = Arc::clone(&self.client);
        // Invalidate any cached rows on a resize
        self.session.make_all_stale();
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
        self.session.update_last_send();
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
            let advertised = self.session.dimensions();
            let owns_viewport = self.client.owns_remote_viewport(self.remote_tab_id());
            let decision =
                decide_resize_for_viewport(owns_viewport, prior_requested, advertised, size);
            // A passive renderer displays the server's canonical grid. It
            // must neither reshape its local RenderableDimensions nor send a
            // resize that the server will reject; doing the former alone
            // creates a local/server invalidate loop and visible flicker.
            *requested = next_requested_size(owns_viewport, prior_requested, decision, size);

            if decision.converge_local_surface {
                self.session.apply_local_resize(size);
            }
            if decision.send_rpc {
                self.session.update_last_send();
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
        self.host.events().input_recorded();
        let input_serial = InputSerial::from_millis(self.host.clock().wall_millis());
        if self.session.key_down(input_serial, key, mods)? {
            start_input_drain(&self.session);
        }
        self.session.update_last_send();
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
        // The local session host's terminals are this machine's: a kill is
        // not held back by the frontend lease, which is still being settled
        // for the first moments of every launch. Dropping the kill there
        // left the pane running in the server while its mirror was gone,
        // and the next resync mirrored it back as a new window.
        let host = self.client.client.is_local_session_host();
        if !host && self.client.remote_tab_input_is_blocked() {
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
            client.note_pending_kill(remote_pane_id);
            promise::spawn::spawn(async move {
                let result = async {
                    if host || client.prepare_remote_tab_input(remote_tab_id).await? {
                        client
                            .client
                            .kill_pane(KillPane {
                                pane_id: remote_pane_id,
                            })
                            .await?;
                    }
                    Ok::<(), anyhow::Error>(())
                }
                .await;
                client.forget_pending_kill(remote_pane_id);
                if let Err(err) = &result {
                    if host {
                        log::warn!(
                            "the session server refused to close pane {remote_pane_id}: {err:#}; \
                             it stays running and comes back at the next resync"
                        );
                    }
                }
                result
            })
            .detach();
        }
    }

    fn mouse_event(&self, event: MouseEvent) -> anyhow::Result<()> {
        if self.client.remote_tab_input_is_blocked() {
            return Ok(());
        }
        self.host.events().input_recorded();
        self.queue_input(PaneInput::Mouse(mouse::normalize_wheel(event)))
    }

    fn is_dead(&self) -> bool {
        self.session.is_dead()
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
        self.session.is_mouse_grabbed()
    }

    fn is_alt_screen_active(&self) -> bool {
        self.session.is_alt_screen()
    }

    fn get_keyboard_encoding(&self) -> KeyboardEncoding {
        self.session.keyboard_encoding()
    }

    fn get_current_working_dir(&self, _policy: CachePolicy) -> Option<Url> {
        self.session.working_dir()
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
            log::warn!(
                "erase scrollback of remote pane {}: {err:#}",
                self.remote_pane_id
            );
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
        // A scheme the window is showing rather than choosing stops here: it
        // is adopted for rendering below, but the server is not told and the
        // configured palette -- what this pane falls back to when an
        // application clears its own -- is left as it was. Otherwise every
        // arrow key through the colour-scheme list would spend a round trip
        // per remote pane and leave an abandoned scheme installed on the
        // server as the answer to its OSC colour queries.
        let is_preview = config
            .downcast_ref::<config::TermConfig>()
            .is_some_and(|term_config| term_config.client_palette_is_preview());
        // Skip the send only when the SERVER is known to hold this exact
        // palette. "The value didn't change locally" is not that: the
        // initial advisory can be lost in attach races, and comparing
        // against our own memory would then skip the correction forever —
        // leaving the server answering OSC color queries from its bare
        // defaults. Identical re-sends stay flicker-free because both the
        // server and the receiving side de-duplicate them; the skip here
        // only avoids per-config-bump RPC noise once delivery is confirmed.
        let send = self.delivered_palette.lock().delivered() != Some(&palette);

        // If the application running in the pane hasn't changed the
        // palette through escape sequences, speculatively adopt the
        // new palette so that it updates with the lowest latency.
        if !*self.application_palette.lock() {
            *self.palette.lock() = palette.clone();
        }
        if !is_preview {
            *self.configured_palette.lock() = palette.clone();
        }

        if send && !is_preview {
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
    session: Arc<PaneSession<DesktopHost>>,
}

/// Run the pane's input drain on the main thread executor, which needs a
/// `Send` future: the desktop link hands out `Send` futures, so it is.
fn start_input_drain(session: &Arc<PaneSession<DesktopHost>>) {
    promise::spawn::spawn_into_main_thread(Arc::clone(session).drain_inputs()).detach();
}

impl std::io::Write for PaneWriter {
    fn write(&mut self, data: &[u8]) -> Result<usize, std::io::Error> {
        if self.client.remote_tab_input_is_blocked() {
            return Ok(data.len());
        }
        let start = self
            .session
            .write_bytes(data)
            .map_err(|full| std::io::Error::new(std::io::ErrorKind::WouldBlock, full))?;
        if start {
            start_input_drain(&self.session);
        }
        Ok(data.len())
    }

    fn flush(&mut self) -> Result<(), std::io::Error> {
        Ok(())
    }
}
