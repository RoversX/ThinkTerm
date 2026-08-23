use crate::resize_increment_calculator::ResizeIncrementCalculator;
use crate::termwindow::TermWindowNotif;
use crate::ui::rescale_ui_usize;
use crate::utilsprites::RenderMetrics;
use ::window::{Dimensions, ResizeIncrement, Window, WindowOps, WindowState};
use config::{ConfigHandle, DimensionContext};
use mux::domain::Domain;
use mux::pane::{Pane, PaneId};
use mux::tab::PositionedPane;
use mux::Mux;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};
use wezterm_client::domain::{
    AutomaticRemotePaneResize, ClientDomain, FrontendRecoverySlot, RemoteFrontendGate,
};
use wezterm_client::pane::ClientPane;
use wezterm_font::FontConfiguration;
use wezterm_term::TerminalSize;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FrontendGeometryAction {
    Passive,
    Set { takeover: bool },
    Claim,
}

fn frontend_geometry_action(
    ownership: Option<bool>,
    collaborative: bool,
    previewing: bool,
) -> FrontendGeometryAction {
    match ownership {
        Some(false) if previewing && collaborative => FrontendGeometryAction::Claim,
        Some(false) => FrontendGeometryAction::Passive,
        Some(true) => FrontendGeometryAction::Set { takeover: false },
        None => FrontendGeometryAction::Set { takeover: true },
    }
}

fn visible_geometry_targets(
    adopted: &[(PaneId, TerminalSize)],
    visible_panes: &HashSet<PaneId>,
) -> Vec<(PaneId, TerminalSize)> {
    adopted
        .iter()
        .filter(|(pane_id, _)| visible_panes.contains(pane_id))
        .copied()
        .collect()
}

fn remote_divider_target_is_owed(
    in_flight: Option<&codec::ClientViewport>,
    acknowledged: Option<&codec::ClientViewport>,
    target: &codec::ClientViewport,
) -> bool {
    in_flight != Some(target) && acknowledged != Some(target)
}

fn remote_divider_can_pump(
    strategy: super::RemoteDividerResizeStrategy,
    finishing: bool,
    in_flight: bool,
    has_pending: bool,
) -> bool {
    !in_flight && has_pending && (strategy == super::RemoteDividerResizeStrategy::Live || finishing)
}

/// The tab geometry a local viewport publish was computed against.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LocalTabShape {
    size: TerminalSize,
    /// Sorted, so two snapshots of the same pane set always compare equal.
    panes: Vec<PaneId>,
}

impl LocalTabShape {
    fn of(tab: &Arc<mux::tab::Tab>) -> Self {
        let mut panes: Vec<PaneId> = tab
            .iter_all_panes()
            .into_iter()
            .map(|pane| pane.pane_id())
            .collect();
        panes.sort_unstable();
        Self {
            size: tab.get_size(),
            panes,
        }
    }
}

/// The last locally-published viewport the mux rejected, together with the
/// tab shape it was computed against. A rejected viewport is deterministic:
/// republishing it fails identically, and the failure revokes the lease and
/// notifies every renderer — which brings us straight back here at the 120ms
/// report cadence. Stay quiet until the geometry or the tab actually changes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RejectedLocalViewport {
    viewport: mux::FrontendViewport,
    shape: LocalTabShape,
}

/// A publish is worth attempting unless the exact same geometry was already
/// rejected against the exact same tab shape.
fn local_viewport_publish_is_worthwhile(
    rejected: Option<&RejectedLocalViewport>,
    candidate: &mux::FrontendViewport,
    shape: &LocalTabShape,
) -> bool {
    rejected.is_none_or(|prior| prior.viewport != *candidate || prior.shape != *shape)
}

// Full-screen TUIs handle SIGWINCH asynchronously after the server-side PTY
// resize has returned.  Keep two 125ms overlay frames of quiet time so a
// slightly delayed clear/redraw cannot become the first visible GUI frame.
const FRONTEND_GEOMETRY_SETTLE: Duration = Duration::from_millis(200);

fn geometry_confirmation_settled(
    ready: bool,
    now: std::time::Instant,
    ready_since: &mut Option<std::time::Instant>,
) -> bool {
    if !ready {
        *ready_since = None;
        return false;
    }

    let started = ready_since.get_or_insert(now);
    now.duration_since(*started) >= FRONTEND_GEOMETRY_SETTLE
}

#[derive(Debug, Clone, Copy)]
pub struct RowsAndCols {
    pub rows: usize,
    pub cols: usize,
}

#[derive(Debug)]
pub enum ScaleChange {
    Absolute(f64),
    Relative(f64),
}

impl super::TermWindow {
    pub(crate) fn frontend_recovery_slot(&self) -> FrontendRecoverySlot {
        FrontendRecoverySlot::Window(self.space_owner_id)
    }

    fn remember_gui_recovery_intent(
        &self,
        domain: &ClientDomain,
        tab_id: mux::tab::TabId,
        viewport: &codec::ClientViewport,
    ) {
        let slot = self.frontend_recovery_slot();
        let mux = Mux::get();
        for candidate in mux.iter_domains() {
            let Some(client) = candidate.downcast_ref::<ClientDomain>() else {
                continue;
            };
            if candidate.domain_id() != domain.domain_id() {
                client.clear_frontend_recovery_intent(slot);
            }
        }
        if let Err(err) = domain.set_frontend_recovery_intent(slot, tab_id, viewport) {
            log::trace!("recording GUI frontend recovery intent: {err:#}");
        }
    }

    pub(crate) fn clear_gui_recovery_intent(&self) {
        let slot = self.frontend_recovery_slot();
        let Some(mux) = Mux::try_get() else {
            return;
        };
        for candidate in mux.iter_domains() {
            if let Some(client) = candidate.downcast_ref::<ClientDomain>() {
                client.clear_frontend_recovery_intent(slot);
            }
        }
    }

    pub(crate) fn active_frontend_access_state(&self) -> Option<mux::FrontendAccessState> {
        let pane = Mux::get()
            .get_active_tab_for_window(self.mux_window_id)?
            .get_active_pane()?;
        if let Some(client) = pane.downcast_ref::<ClientPane>() {
            return client
                .remote_access_state()
                .map(|state| mux::FrontendAccessState {
                    mode: match state.mode {
                        codec::FrontendAccessMode::TmuxLatest => {
                            mux::FrontendAccessMode::TmuxLatest
                        }
                        codec::FrontendAccessMode::Handoff => mux::FrontendAccessMode::Handoff,
                    },
                    owner: state.owner,
                    generation: state.generation,
                });
        }
        Some(Mux::get().frontend_access_state())
    }

    pub(crate) fn frontend_terminal_gate(&self) -> RemoteFrontendGate {
        let Some(tab) = Mux::get().get_active_tab_for_window(self.mux_window_id) else {
            return RemoteFrontendGate::Visible;
        };
        let Some(pane) = tab.get_active_pane() else {
            return RemoteFrontendGate::Visible;
        };
        let gate = if let Some(client) = pane.downcast_ref::<ClientPane>() {
            client.remote_frontend_gate()
        } else {
            let state = Mux::get().frontend_access_state();
            if state.mode == mux::FrontendAccessMode::TmuxLatest
                || Mux::get()
                    .active_identity()
                    .as_deref()
                    .is_some_and(|identity| state.owner.as_ref() == Some(identity))
            {
                RemoteFrontendGate::Visible
            } else {
                RemoteFrontendGate::Claimable { owner: state.owner }
            }
        };
        let obscured_by_geometry = self
            .frontend_geometry_phases
            .get(&tab.tab_id())
            .copied()
            .is_some_and(super::FrontendGeometryPhase::obscures_terminal);
        if obscured_by_geometry
            && matches!(
                gate,
                RemoteFrontendGate::Visible
                    | RemoteFrontendGate::Claimable { .. }
                    | RemoteFrontendGate::Syncing
            )
        {
            RemoteFrontendGate::Syncing
        } else {
            // Connection health remains more informative than a geometry
            // wait if the transport drops during takeover.
            gate
        }
    }

    pub(crate) fn frontend_surface_blocked(&self) -> bool {
        self.frontend_terminal_gate().obscures_terminal()
    }

    pub(crate) fn frontend_takeover_claimable(&self) -> bool {
        self.frontend_terminal_gate().is_claimable()
    }

    pub(crate) fn active_remote_frontend_viewport_state(
        &self,
    ) -> Option<codec::ClientViewportState> {
        Mux::get()
            .get_active_tab_for_window(self.mux_window_id)?
            .get_active_pane()?
            .downcast_ref::<ClientPane>()?
            .remote_viewport_state()
    }

    fn tab_frontend_viewport_ownership(&self, tab: &Arc<mux::tab::Tab>) -> Option<bool> {
        if let Some(owns) = tab.get_active_pane().and_then(|pane| {
            pane.downcast_ref::<ClientPane>()
                .map(ClientPane::owns_remote_viewport)
        }) {
            return owns;
        }
        Some(Mux::get().current_identity_owns_frontend_lease(tab.tab_id()))
    }

    fn tab_owns_frontend_viewport(&self, tab: &Arc<mux::tab::Tab>) -> bool {
        self.tab_frontend_viewport_ownership(tab) == Some(true)
    }

    pub(crate) fn owns_frontend_viewport(&self) -> bool {
        let mux = Mux::get();
        mux.get_active_tab_for_window(self.mux_window_id)
            .map_or(true, |tab| self.tab_owns_frontend_viewport(&tab))
    }

    fn begin_frontend_geometry_epoch(
        &mut self,
        tab_id: mux::tab::TabId,
        takeover: bool,
    ) -> Option<u64> {
        if self
            .frontend_geometry_phases
            .get(&tab_id)
            .copied()
            .is_some_and(super::FrontendGeometryPhase::is_in_flight)
        {
            return None;
        }
        let epoch = self.next_frontend_geometry_epoch;
        self.next_frontend_geometry_epoch =
            self.next_frontend_geometry_epoch.wrapping_add(1).max(1);
        let phase = if takeover {
            super::FrontendGeometryPhase::TakeoverSyncing { epoch }
        } else {
            super::FrontendGeometryPhase::Committing { epoch }
        };
        self.frontend_geometry_phases.insert(tab_id, phase);
        self.invalidate_window();
        Some(epoch)
    }

    fn finish_frontend_geometry_epoch(
        &mut self,
        tab_id: mux::tab::TabId,
        epoch: u64,
        succeeded: bool,
        adopted: &[(PaneId, TerminalSize)],
    ) {
        let Some(phase) = self.frontend_geometry_phases.get(&tab_id).copied() else {
            return;
        };
        if phase.epoch() != epoch {
            return;
        }

        if !succeeded {
            if let Some(recovery) = self.frontend_recovery_geometry.remove(&tab_id) {
                if recovery.epoch == epoch {
                    if let Some(domain) = Mux::get().get_domain(recovery.domain_id) {
                        if let Some(client) = domain.downcast_ref::<ClientDomain>() {
                            client.fail_frontend_recovery(
                                recovery.slot,
                                tab_id,
                                recovery.generation,
                                format!(
                                    "GUI geometry recovery failed for tab {tab_id} generation {}",
                                    recovery.generation
                                ),
                            );
                        }
                    }
                } else {
                    self.frontend_recovery_geometry.insert(tab_id, recovery);
                }
            }
        }

        let mux = Mux::get();
        for (pane_id, size) in adopted {
            let Some(pane) = mux.get_pane(*pane_id) else {
                continue;
            };
            let Some(client) = pane.downcast_ref::<ClientPane>() else {
                continue;
            };
            if succeeded {
                // A resync notification can race the RPC completion and
                // temporarily put the advertised dimensions back. Validate
                // the surface against the exact acknowledged viewport before
                // allowing the first unmasked paint.
                client.adopt_frontend_geometry(*size);
            } else {
                client.forget_frontend_geometry(*size);
            }
        }

        if succeeded && phase.obscures_terminal() {
            // The RPC acknowledgement means that the server has issued the
            // PTY resize, not that the resized screen has reached this
            // renderer. Keep the opaque takeover state and actively fetch a
            // complete post-resize snapshot before revealing it.
            let visible_panes = self
                .get_panes_to_render()
                .into_iter()
                .map(|positioned| positioned.pane.pane_id())
                .collect::<HashSet<_>>();
            self.frontend_geometry_confirmations.insert(
                tab_id,
                super::FrontendGeometryConfirmation {
                    epoch,
                    panes: visible_geometry_targets(adopted, &visible_panes),
                    ready_since: None,
                },
            );
            self.advance_frontend_geometry_confirmation();
            self.update_title_post_status();
            self.invalidate_window();
            return;
        }

        self.complete_frontend_geometry_epoch(tab_id, epoch);
    }

    fn complete_frontend_geometry_epoch(&mut self, tab_id: mux::tab::TabId, epoch: u64) {
        let Some(phase) = self.frontend_geometry_phases.get(&tab_id).copied() else {
            return;
        };
        if phase.epoch() != epoch {
            return;
        }
        let keep_follow_up_obscured = phase.obscures_terminal();
        self.frontend_geometry_confirmations.remove(&tab_id);
        self.frontend_geometry_phases.remove(&tab_id);
        if let Some(recovery) = self.frontend_recovery_geometry.remove(&tab_id) {
            if recovery.epoch == epoch {
                if let Some(domain) = Mux::get().get_domain(recovery.domain_id) {
                    if let Some(client) = domain.downcast_ref::<ClientDomain>() {
                        client.acknowledge_frontend_recovery(
                            recovery.slot,
                            tab_id,
                            recovery.generation,
                        );
                    }
                }
            } else {
                self.frontend_recovery_geometry.insert(tab_id, recovery);
            }
        }
        // Only consume the owed resync if we are actually about to run it.
        // Removing it unconditionally discarded the geometry a background tab
        // was still waiting for: the flag was cleared, the sync never ran, and
        // nothing re-armed it, so that tab kept stale geometry until an
        // unrelated window resize happened to push it again.
        let needs_follow_up = self.frontend_geometry_resync_after_epoch.contains(&tab_id);
        let active = self.active_tab_is(tab_id);
        mux::zoom_trace!(
            "gui.sync.complete tab={tab_id} epoch={epoch} follow_up={needs_follow_up} \
             active={active}"
        );
        if needs_follow_up && active {
            self.frontend_geometry_resync_after_epoch.remove(&tab_id);
            mux::zoom_trace!("gui.sync.followup tab={tab_id} after_epoch={epoch}");
            // Starting the follow-up synchronously keeps the tab opaque: the
            // old epoch is replaced before this callback can paint.
            self.sync_active_tab_geometry_now();
            if keep_follow_up_obscured {
                if let Some(super::FrontendGeometryPhase::Committing { epoch }) =
                    self.frontend_geometry_phases.get(&tab_id).copied()
                {
                    self.frontend_geometry_phases.insert(
                        tab_id,
                        super::FrontendGeometryPhase::TakeoverSyncing { epoch },
                    );
                }
            }
        }
        self.update_title_post_status();
        self.invalidate_window();
    }

    /// Progress the active takeover without painting its old terminal grid.
    /// Each call forces a remote render poll and primes missing visible rows;
    /// PaneOutput and the overlay animation schedule subsequent checks.
    pub(crate) fn advance_frontend_geometry_confirmation(&mut self) {
        let Some(tab) = Mux::get().get_active_tab_for_window(self.mux_window_id) else {
            return;
        };
        let tab_id = tab.tab_id();
        // A confirmation owed to a tab the user has switched away from is
        // never advanced here, and its overlay is not painted either, so
        // nothing schedules the frames that would settle it.
        if mux::geometrytrace::trace_enabled() {
            for (pending_tab, pending) in &self.frontend_geometry_confirmations {
                if *pending_tab != tab_id {
                    mux::zoom_trace!(
                        "gui.confirm.stranded tab={pending_tab} epoch={} active_tab={tab_id}",
                        pending.epoch
                    );
                }
            }
        }
        let Some(confirmation) = self.frontend_geometry_confirmations.get(&tab_id).cloned() else {
            return;
        };
        if self
            .frontend_geometry_phases
            .get(&tab_id)
            .copied()
            .map(super::FrontendGeometryPhase::epoch)
            != Some(confirmation.epoch)
        {
            self.frontend_geometry_confirmations.remove(&tab_id);
            return;
        }

        let mux = Mux::get();
        let mut ready = !confirmation.panes.is_empty();
        let mut blockers = Vec::new();
        for (pane_id, size) in &confirmation.panes {
            let pane_ready = mux
                .get_pane(*pane_id)
                .and_then(|pane| {
                    pane.downcast_ref::<ClientPane>()
                        .map(|client| client.prime_frontend_geometry(*size))
                })
                .unwrap_or(false);
            if !pane_ready && mux::geometrytrace::trace_enabled() {
                // Distinguish "the server never agreed on this size" (a
                // permanent stall) from "rows are still being fetched" (which
                // resolves on its own).
                let why = mux
                    .get_pane(*pane_id)
                    .and_then(|pane| {
                        pane.downcast_ref::<ClientPane>()
                            .map(|client| client.frontend_geometry_mismatch(*size))
                    })
                    .flatten()
                    .unwrap_or_else(|| "rows_pending".to_string());
                blockers.push(format!(
                    "{pane_id}:want={} {why}",
                    mux::geometrytrace::size(size)
                ));
            }
            ready &= pane_ready;
        }

        let now = std::time::Instant::now();
        let settled = if let Some(pending) = self.frontend_geometry_confirmations.get_mut(&tab_id) {
            geometry_confirmation_settled(ready, now, &mut pending.ready_since)
        } else {
            false
        };
        mux::zoom_trace!(
            "gui.confirm.tick tab={tab_id} epoch={} ready={ready} settled={settled} \
             blockers=[{}]",
            confirmation.epoch,
            blockers.join("; ")
        );
        if settled {
            self.complete_frontend_geometry_epoch(tab_id, confirmation.epoch);
        }
    }

    pub(crate) fn active_tab_is(&self, tab_id: mux::tab::TabId) -> bool {
        Mux::get()
            .get_active_tab_for_window(self.mux_window_id)
            .is_some_and(|tab| tab.tab_id() == tab_id)
    }

    /// Reflow one active top-level tab and derive both the root viewport and
    /// every visible pane surface from that same final layout pass.
    fn prepare_client_frontend_geometry(
        &mut self,
        tab: &Arc<mux::tab::Tab>,
    ) -> Option<(
        Arc<dyn Domain>,
        codec::ClientViewport,
        Vec<(PaneId, TerminalSize)>,
    )> {
        self.prepare_client_frontend_geometry_with_preview(tab, None)
    }

    fn prepare_client_frontend_geometry_with_preview(
        &mut self,
        tab: &Arc<mux::tab::Tab>,
        preview_epoch: Option<u64>,
    ) -> Option<(
        Arc<dyn Domain>,
        codec::ClientViewport,
        Vec<(PaneId, TerminalSize)>,
    )> {
        if !self.active_tab_is(tab.tab_id()) {
            return None;
        }
        let active_pane = tab.get_active_pane()?;
        let client_pane = active_pane.downcast_ref::<ClientPane>()?;
        let domain_id = client_pane.domain_id();
        let domain = Mux::get().get_domain(domain_id)?;
        if !domain.is::<ClientDomain>() {
            return None;
        }

        tab.resize(self.terminal_size);
        self.reapply_collapsed_panes_for_tab(tab.tab_id());

        let mut panes = Vec::new();
        let mut adopted = Vec::new();
        for positioned in self.get_panes_to_render() {
            for member in self.positioned_panes_for_stack(tab, &positioned)? {
                let pane = member.pane.downcast_ref::<ClientPane>()?;
                if pane.domain_id() != domain_id {
                    return None;
                }
                let viewport = self.frontend_viewport_for_positioned_pane(&member)?;
                adopted.push((viewport.pane_id, viewport.size));
                panes.push(viewport);
            }
        }
        for (pane_id, size) in &adopted {
            let pane = Mux::get().get_pane(*pane_id)?;
            let pane = pane.downcast_ref::<ClientPane>()?;
            if let Some(epoch) = preview_epoch {
                pane.preview_frontend_geometry(epoch, *size);
            } else {
                pane.adopt_frontend_geometry(*size);
            }
        }

        Some((
            domain,
            codec::ClientViewport::Native {
                size: self.terminal_size,
                panes,
            },
            adopted,
        ))
    }

    /// Keep a remote split visually attached to its divider while it is being
    /// dragged. The selected policy is frozen at drag start: Live pumps at
    /// most one RPC plus one replaceable newest target; OnRelease only keeps
    /// the local pinned preview until the drag ends.
    pub(crate) fn preview_active_tab_geometry_now(&mut self) {
        if self.content_view_foreground() {
            return;
        }
        let mux = Mux::get();
        let Some(tab) = mux.get_active_tab_for_window(self.mux_window_id) else {
            return;
        };
        let tab_id = tab.tab_id();
        if tab
            .get_active_pane()
            .is_none_or(|pane| pane.downcast_ref::<ClientPane>().is_none())
        {
            self.force_sync_active_mux_tab_pane_sizes();
            return;
        }

        let mode = self.active_frontend_access_state().map(|state| state.mode);
        let ownership = self.tab_frontend_viewport_ownership(&tab);
        if mode == Some(mux::FrontendAccessMode::Handoff) && ownership != Some(true) {
            return;
        }
        let epoch = if let Some(stream) = self.remote_divider_resize_streams.get(&tab_id) {
            stream.epoch
        } else {
            let epoch = self.next_frontend_geometry_epoch;
            self.next_frontend_geometry_epoch =
                self.next_frontend_geometry_epoch.wrapping_add(1).max(1);
            epoch
        };
        let Some((domain, viewport, panes)) =
            self.prepare_client_frontend_geometry_with_preview(&tab, Some(epoch))
        else {
            return;
        };
        let Some(client_domain) = domain.downcast_ref::<ClientDomain>() else {
            return;
        };
        let Some(connection_generation) = client_domain.connection_generation() else {
            return;
        };
        let prepared = super::PreparedRemoteDividerGeometry { viewport, panes };

        if let Some(stream) = self.remote_divider_resize_streams.get_mut(&tab_id) {
            stream.latest = prepared.clone();
            stream.pending = remote_divider_target_is_owed(
                stream.in_flight.as_ref(),
                stream.last_acknowledged.as_ref(),
                &prepared.viewport,
            )
            .then_some(prepared);
        } else {
            let pane_is_tardy = self.get_panes_to_render().into_iter().any(|positioned| {
                positioned
                    .pane
                    .downcast_ref::<ClientPane>()
                    .is_some_and(ClientPane::is_remote_tardy)
            });
            let strategy = match crate::native_settings::remote_pane_resize_mode() {
                crate::native_settings::NativeRemotePaneResizeMode::Live => {
                    super::RemoteDividerResizeStrategy::Live
                }
                crate::native_settings::NativeRemotePaneResizeMode::OnRelease => {
                    super::RemoteDividerResizeStrategy::OnRelease
                }
                crate::native_settings::NativeRemotePaneResizeMode::Auto => {
                    match client_domain.automatic_remote_pane_resize(pane_is_tardy) {
                        AutomaticRemotePaneResize::Live => super::RemoteDividerResizeStrategy::Live,
                        AutomaticRemotePaneResize::OnRelease => {
                            super::RemoteDividerResizeStrategy::OnRelease
                        }
                    }
                }
            };
            let collaborative = mode == Some(mux::FrontendAccessMode::TmuxLatest);
            self.remote_divider_resize_streams.insert(
                tab_id,
                super::RemoteDividerResizeStream {
                    epoch,
                    strategy,
                    domain_id: domain.domain_id(),
                    connection_generation,
                    claim_first: collaborative && ownership == Some(false),
                    finishing: false,
                    latest: prepared.clone(),
                    pending: Some(prepared),
                    in_flight: None,
                    last_acknowledged: None,
                    final_acknowledged_at: None,
                },
            );
        }

        self.frontend_geometry_confirmations.remove(&tab_id);
        self.frontend_geometry_phases
            .insert(tab_id, super::FrontendGeometryPhase::Previewing { epoch });
        self.pump_remote_divider_resize(tab_id);
        self.invalidate_window();
    }

    fn pump_remote_divider_resize(&mut self, tab_id: mux::tab::TabId) {
        let Some(stream) = self.remote_divider_resize_streams.get_mut(&tab_id) else {
            return;
        };
        if !remote_divider_can_pump(
            stream.strategy,
            stream.finishing,
            stream.in_flight.is_some(),
            stream.pending.is_some(),
        ) {
            return;
        }
        let Some(prepared) = stream.pending.take() else {
            return;
        };
        let epoch = stream.epoch;
        let domain_id = stream.domain_id;
        let generation = stream.connection_generation;
        let claim = stream.claim_first;
        stream.claim_first = false;
        stream.in_flight = Some(prepared.viewport.clone());

        let Some(domain) = Mux::get().get_domain(domain_id) else {
            self.abort_remote_divider_resize(tab_id, epoch, true);
            return;
        };
        let Some(client_domain) = domain.downcast_ref::<ClientDomain>() else {
            self.abort_remote_divider_resize(tab_id, epoch, true);
            return;
        };
        if client_domain.connection_generation() != Some(generation) {
            self.abort_remote_divider_resize(tab_id, epoch, true);
            return;
        }
        self.remember_gui_recovery_intent(client_domain, tab_id, &prepared.viewport);
        let Some(window) = self.window.as_ref().cloned() else {
            self.abort_remote_divider_resize(tab_id, epoch, false);
            return;
        };
        let sent = prepared.viewport.clone();
        promise::spawn::spawn(async move {
            let succeeded = match domain.downcast_ref::<ClientDomain>() {
                Some(client_domain)
                    if client_domain.connection_generation() == Some(generation) =>
                {
                    let result = if claim {
                        client_domain
                            .claim_client_viewport(tab_id, sent.clone())
                            .await
                    } else {
                        client_domain
                            .set_client_viewport(tab_id, sent.clone())
                            .await
                    };
                    if let Err(err) = &result {
                        log::warn!("streaming remote divider resize: {err:#}");
                    }
                    result.is_ok() && client_domain.owns_remote_viewport(tab_id) == Some(true)
                }
                Some(_) | None => false,
            };
            window.notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                term_window
                    .remote_divider_resize_rpc_finished(tab_id, epoch, generation, sent, succeeded);
            })));
            Ok::<(), anyhow::Error>(())
        })
        .detach();
    }

    fn remote_divider_resize_rpc_finished(
        &mut self,
        tab_id: mux::tab::TabId,
        epoch: u64,
        generation: u64,
        sent: codec::ClientViewport,
        succeeded: bool,
    ) {
        let valid = self
            .remote_divider_resize_streams
            .get(&tab_id)
            .is_some_and(|stream| {
                stream.epoch == epoch
                    && stream.connection_generation == generation
                    && stream.in_flight.as_ref() == Some(&sent)
            });
        if !valid {
            return;
        }
        if !succeeded || !self.active_tab_is(tab_id) {
            self.abort_remote_divider_resize(tab_id, epoch, true);
            return;
        }

        let mut should_pump = false;
        let mut should_confirm = false;
        if let Some(stream) = self.remote_divider_resize_streams.get_mut(&tab_id) {
            stream.in_flight = None;
            stream.last_acknowledged = Some(sent);
            if stream.pending.is_some() {
                should_pump = true;
            } else if stream.finishing
                && stream.last_acknowledged.as_ref() == Some(&stream.latest.viewport)
            {
                stream
                    .final_acknowledged_at
                    .get_or_insert_with(Instant::now);
                should_confirm = true;
            }
        }
        if should_pump {
            self.pump_remote_divider_resize(tab_id);
        } else if should_confirm {
            self.confirm_remote_divider_resize(tab_id, epoch);
        }
    }

    pub(crate) fn finish_remote_split_drag(&mut self) {
        let Some(tab) = Mux::get().get_active_tab_for_window(self.mux_window_id) else {
            return;
        };
        let tab_id = tab.tab_id();
        let (epoch, should_confirm) = {
            let Some(stream) = self.remote_divider_resize_streams.get_mut(&tab_id) else {
                self.sync_active_tab_geometry_now();
                return;
            };
            stream.finishing = true;
            if remote_divider_target_is_owed(
                stream.in_flight.as_ref(),
                stream.last_acknowledged.as_ref(),
                &stream.latest.viewport,
            ) {
                stream.pending = Some(stream.latest.clone());
            }
            let should_confirm = stream.in_flight.is_none()
                && stream.pending.is_none()
                && stream.last_acknowledged.as_ref() == Some(&stream.latest.viewport);
            if should_confirm {
                stream
                    .final_acknowledged_at
                    .get_or_insert_with(Instant::now);
            }
            (stream.epoch, should_confirm)
        };
        self.pump_remote_divider_resize(tab_id);
        if should_confirm {
            self.confirm_remote_divider_resize(tab_id, epoch);
        }
    }

    fn confirm_remote_divider_resize(&mut self, tab_id: mux::tab::TabId, epoch: u64) {
        let Some(stream) = self.remote_divider_resize_streams.get(&tab_id) else {
            return;
        };
        if stream.epoch != epoch || !stream.finishing || stream.in_flight.is_some() {
            return;
        }
        let generation = stream.connection_generation;
        let domain_id = stream.domain_id;
        let panes = stream.latest.panes.clone();
        let started = stream.final_acknowledged_at.unwrap_or_else(Instant::now);
        let domain_generation_matches = Mux::get().get_domain(domain_id).and_then(|domain| {
            domain
                .downcast_ref::<ClientDomain>()
                .and_then(ClientDomain::connection_generation)
        }) == Some(generation);
        let mut ready = domain_generation_matches && !panes.is_empty();
        for (pane_id, size) in &panes {
            let pane_ready = Mux::get()
                .get_pane(*pane_id)
                .and_then(|pane| {
                    pane.downcast_ref::<ClientPane>().map(|client| {
                        let matches = client.server_geometry_matches(*size);
                        if !matches {
                            let _ = client.prime_frontend_geometry(*size);
                        }
                        matches
                    })
                })
                .unwrap_or(false);
            ready &= pane_ready;
        }
        if ready {
            self.complete_remote_divider_resize(tab_id, epoch);
            return;
        }
        if started.elapsed() >= Duration::from_secs(2) {
            log::warn!("remote divider resize did not converge for tab {tab_id}");
            self.abort_remote_divider_resize(tab_id, epoch, true);
            return;
        }
        let Some(window) = self.window.as_ref().cloned() else {
            self.abort_remote_divider_resize(tab_id, epoch, false);
            return;
        };
        promise::spawn::spawn(async move {
            smol::Timer::after(Duration::from_millis(25)).await;
            window.notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                term_window.confirm_remote_divider_resize(tab_id, epoch);
            })));
            Ok::<(), anyhow::Error>(())
        })
        .detach();
    }

    fn complete_remote_divider_resize(&mut self, tab_id: mux::tab::TabId, epoch: u64) {
        let Some(stream) = self.remote_divider_resize_streams.remove(&tab_id) else {
            return;
        };
        if stream.epoch != epoch {
            self.remote_divider_resize_streams.insert(tab_id, stream);
            return;
        }
        for (pane_id, size) in &stream.latest.panes {
            if let Some(pane) = Mux::get().get_pane(*pane_id) {
                if let Some(client) = pane.downcast_ref::<ClientPane>() {
                    client.finish_frontend_geometry_preview(epoch, *size, true);
                }
            }
        }
        if self
            .frontend_geometry_phases
            .get(&tab_id)
            .is_some_and(|phase| phase.epoch() == epoch)
        {
            self.frontend_geometry_phases.remove(&tab_id);
        }
        self.update_title_post_status();
        self.invalidate_window();
    }

    fn abort_remote_divider_resize(&mut self, tab_id: mux::tab::TabId, epoch: u64, resync: bool) {
        let Some(stream) = self.remote_divider_resize_streams.remove(&tab_id) else {
            return;
        };
        if stream.epoch != epoch {
            self.remote_divider_resize_streams.insert(tab_id, stream);
            return;
        }
        for (pane_id, size) in &stream.latest.panes {
            if let Some(pane) = Mux::get().get_pane(*pane_id) {
                if let Some(client) = pane.downcast_ref::<ClientPane>() {
                    client.finish_frontend_geometry_preview(epoch, *size, false);
                }
            }
        }
        if self
            .frontend_geometry_phases
            .get(&tab_id)
            .is_some_and(|phase| phase.epoch() == epoch)
        {
            self.frontend_geometry_phases.remove(&tab_id);
        }
        if resync {
            if let Some(domain) = Mux::get().get_domain(stream.domain_id) {
                promise::spawn::spawn(async move {
                    if let Some(client) = domain.downcast_ref::<ClientDomain>() {
                        if let Err(err) = client.resync().await {
                            log::warn!("resyncing after failed divider resize: {err:#}");
                        }
                    }
                    Ok::<(), anyhow::Error>(())
                })
                .detach();
            }
        }
        self.update_title_post_status();
        self.invalidate_window();
    }

    pub(crate) fn cancel_remote_divider_resizes_except(&mut self, keep: Option<mux::tab::TabId>) {
        let stale = self
            .remote_divider_resize_streams
            .iter()
            .filter_map(|(tab_id, stream)| {
                (Some(*tab_id) != keep).then_some((*tab_id, stream.epoch))
            })
            .collect::<Vec<_>>();
        for (tab_id, epoch) in stale {
            self.abort_remote_divider_resize(tab_id, epoch, true);
        }
    }

    /// Take the viewport because someone is using this window right now.
    ///
    /// The server hands the lease over on *input*, and a click in a pane that
    /// is not asking for mouse reporting never reaches the server at all — it
    /// is handled here. So a window sat down at and clicked in kept drawing at
    /// whatever size the phone that last typed had left it, until something
    /// incidental happened to re-report the geometry.
    ///
    /// Terminal-area clicks and scrolling claim.  Tab/sidebar/window chrome
    /// calls never reach this method, so merely navigating the surrounding UI
    /// cannot steal the terminal from another renderer.
    ///
    /// Nothing happens when this window already owns the viewport, which is
    /// the overwhelmingly common case, so an ordinary click costs one
    /// comparison.
    pub(crate) fn claim_frontend_viewport_for_interaction(&mut self) {
        if !matches!(
            self.frontend_terminal_gate(),
            RemoteFrontendGate::Visible | RemoteFrontendGate::Claimable { .. }
        ) {
            return;
        }
        let mux = Mux::get();
        let Some(tab) = mux.get_active_tab_for_window(self.mux_window_id) else {
            return;
        };
        let tab_id = tab.tab_id();
        if self
            .frontend_geometry_phases
            .get(&tab_id)
            .copied()
            .is_some_and(super::FrontendGeometryPhase::is_in_flight)
            || self.owns_frontend_viewport()
        {
            return;
        }

        if tab
            .get_active_pane()
            .is_some_and(|pane| pane.downcast_ref::<ClientPane>().is_none())
        {
            let Some(client_id) = mux.active_identity() else {
                return;
            };
            // An explicit user takeover re-arms the local publish path.
            self.forget_rejected_local_viewport(tab_id);
            let viewport = self.local_frontend_viewport_for_tab(&tab, true);
            if let Err(err) = mux.claim_local_frontend_viewport(&client_id, tab_id, viewport) {
                log::warn!("claiming local GUI frontend viewport: {err:#}");
                return;
            }
            self.resize_mux_tabs_to_current_terminal_size();
            self.invalidate_window();
            return;
        }

        let Some(epoch) = self.begin_frontend_geometry_epoch(tab_id, true) else {
            return;
        };
        let Some((domain, viewport, adopted)) = self.prepare_client_frontend_geometry(&tab) else {
            self.frontend_geometry_phases.remove(&tab_id);
            self.invalidate_window();
            return;
        };
        if let Some(client_domain) = domain.downcast_ref::<ClientDomain>() {
            self.remember_gui_recovery_intent(client_domain, tab_id, &viewport);
        }
        let Some(window) = self.window.as_ref().cloned() else {
            self.frontend_geometry_phases.remove(&tab_id);
            return;
        };
        promise::spawn::spawn(async move {
            let result = match domain.downcast_ref::<ClientDomain>() {
                Some(client_domain) => {
                    let result = client_domain.claim_client_viewport(tab_id, viewport).await;
                    if let Err(err) = &result {
                        log::warn!("claiming remote GUI frontend viewport: {err:#}");
                        if let Err(resync_err) = client_domain.resync().await {
                            log::warn!(
                                "resyncing after failed frontend viewport claim: {resync_err:#}"
                            );
                        }
                    }
                    result.map(|_| client_domain.owns_remote_viewport(tab_id) == Some(true))
                }
                None => Err(anyhow::anyhow!("frontend domain is not a client domain")),
            };
            window.notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                let succeeded = result.as_ref().is_ok_and(|owns| *owns);
                let is_still_active = term_window.active_tab_is(tab_id);
                term_window.finish_frontend_geometry_epoch(tab_id, epoch, succeeded, &adopted);
                if succeeded && is_still_active {
                    for pos in term_window.get_panes_to_render() {
                        term_window.scroll_to_bottom(&pos.pane);
                    }
                }
            })));
            Ok::<(), anyhow::Error>(())
        })
        .detach();
    }

    pub(crate) fn request_frontend_access_mode(&mut self, mode: codec::FrontendAccessMode) {
        let mux = Mux::get();
        let Some(tab) = mux.get_active_tab_for_window(self.mux_window_id) else {
            return;
        };
        let tab_id = tab.tab_id();
        let is_remote = tab
            .get_active_pane()
            .is_some_and(|pane| pane.downcast_ref::<ClientPane>().is_some());
        if is_remote {
            let Some((domain, viewport)) = self.client_viewport_for_tab(&tab, true) else {
                return;
            };
            let Some(window) = self.window.as_ref().cloned() else {
                return;
            };
            promise::spawn::spawn(async move {
                let result = match domain.downcast_ref::<ClientDomain>() {
                    Some(domain) => domain
                        .set_frontend_access_mode(tab_id, mode, viewport)
                        .await
                        .map(|_| ()),
                    None => Ok(()),
                };
                window.notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                    if let Err(err) = result {
                        log::warn!("changing frontend access mode: {err:#}");
                    } else {
                        for pos in term_window.get_panes_to_render() {
                            term_window.scroll_to_bottom(&pos.pane);
                        }
                        term_window.resize_mux_tabs_to_current_terminal_size();
                    }
                    term_window.update_title_post_status();
                    term_window.invalidate_window();
                })));
                Ok::<(), anyhow::Error>(())
            })
            .detach();
            return;
        }

        let Some(client_id) = mux.active_identity() else {
            return;
        };
        let target = match mode {
            codec::FrontendAccessMode::TmuxLatest => mux::FrontendAccessMode::TmuxLatest,
            codec::FrontendAccessMode::Handoff => mux::FrontendAccessMode::Handoff,
        };
        // A mode change is an explicit user action: re-arm the local publish.
        self.forget_rejected_local_viewport(tab_id);
        let viewport = self.local_frontend_viewport_for_tab(&tab, true);
        if let Err(err) =
            mux.validate_frontend_access_mode_change(&client_id, target, tab_id, &viewport)
        {
            log::warn!("changing local frontend access mode: {err:#}");
            return;
        }
        let prior = mux.frontend_access_state().mode;
        if let Err(err) = wezterm_mux_server_impl::thinkterm_access::persist_mode(target) {
            log::warn!("persisting local frontend access mode: {err:#}");
            return;
        }
        if let Err(err) = mux.set_frontend_access_mode(&client_id, target, tab_id, viewport) {
            if prior != target {
                if let Err(rollback) =
                    wezterm_mux_server_impl::thinkterm_access::persist_mode(prior)
                {
                    log::error!("rolling back local frontend mode: {rollback:#}");
                }
            }
            log::warn!("changing local frontend access mode: {err:#}");
            return;
        }
        for pos in self.get_panes_to_render() {
            self.scroll_to_bottom(&pos.pane);
        }
        self.resize_mux_tabs_to_current_terminal_size();
        self.update_title_post_status();
        self.invalidate_window();
    }

    /// How this window would describe itself for `tab`, when that tab is
    /// backed by a remote mux. `None` for a tab that is not.
    ///
    /// `include_panes` says whether the per-pane split is worth offering.
    /// It is false for any tab this window is not currently drawing — its
    /// split is not the one in force, and offering it would ask the server to
    /// act on a layout nobody is looking at. The overall grid is still said,
    /// because the server can only hand the viewport to a client whose
    /// geometry it already holds: a tab this window has never described is a
    /// tab it can never take back.
    fn client_viewport_for_tab(
        &self,
        tab: &Arc<mux::tab::Tab>,
        include_panes: bool,
    ) -> Option<(Arc<dyn Domain>, codec::ClientViewport)> {
        let mux = Mux::get();
        let active_pane = tab.get_active_pane()?;
        let client_pane = active_pane.downcast_ref::<ClientPane>()?;
        let domain_id = client_pane.domain_id();
        let domain = mux.get_domain(domain_id)?;
        if !domain.is::<ClientDomain>() {
            return None;
        }
        let panes = if include_panes {
            let mut panes = Vec::new();
            for positioned in self.get_panes_to_render() {
                for member in self.positioned_panes_for_stack(tab, &positioned)? {
                    let pane = member.pane.downcast_ref::<ClientPane>()?;
                    if pane.domain_id() != domain_id {
                        return None;
                    }
                    panes.push(self.frontend_viewport_for_positioned_pane(&member)?);
                }
            }
            panes
        } else {
            Vec::new()
        };
        Some((
            domain,
            codec::ClientViewport::Native {
                size: self.terminal_size,
                panes,
            },
        ))
    }

    /// Debounce resize-driven advertisements. Only the active tab has exact
    /// pane rectangles; background tabs are never overwritten with an empty
    /// native layout.
    pub(crate) fn report_frontend_viewport(&self) {
        if self
            .frontend_viewport_report_pending
            .swap(true, std::sync::atomic::Ordering::AcqRel)
        {
            return;
        }
        let Some(window) = self.window.as_ref().cloned() else {
            self.frontend_viewport_report_pending
                .store(false, std::sync::atomic::Ordering::Release);
            return;
        };
        let pending = Arc::clone(&self.frontend_viewport_report_pending);
        promise::spawn::spawn(async move {
            smol::Timer::after(Duration::from_millis(120)).await;
            window.notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                pending.store(false, std::sync::atomic::Ordering::Release);
                term_window.report_frontend_viewport_now();
            })));
            Ok::<(), anyhow::Error>(())
        })
        .detach();
    }

    fn report_frontend_viewport_now(&mut self) {
        let mux = Mux::get();
        // Rejections for tabs that no longer exist have nothing left to damp.
        self.rejected_local_viewports
            .retain(|tab_id, _| mux.get_tab(*tab_id).is_some());
        let Some(tab) = mux.get_active_tab_for_window(self.mux_window_id) else {
            return;
        };
        if let Some(phase) = self.frontend_geometry_phases.get(&tab.tab_id()).copied() {
            if phase.is_in_flight() {
                self.frontend_geometry_resync_after_epoch
                    .insert(tab.tab_id());
            }
            // Preview geometry is committed explicitly on divider release;
            // in-flight geometry schedules one newest follow-up above.
            return;
        }
        self.report_frontend_viewport_for_tab(&tab);
    }

    /// Immediately converge the active top-level tab. Discrete lifecycle
    /// events (activation, tab creation, split completion and divider release)
    /// use this instead of the window-resize debounce so the first paint is
    /// already based on the current GUI geometry.
    pub(crate) fn sync_replacement_frontend_geometry(
        &mut self,
        tab_id: mux::tab::TabId,
        slot: FrontendRecoverySlot,
        generation: u64,
    ) -> bool {
        if slot != self.frontend_recovery_slot() {
            return false;
        }
        let mux = Mux::get();
        let Some(mut window) = mux.get_window_mut(self.mux_window_id) else {
            return false;
        };
        let Some(tab_idx) = window.idx_by_id(tab_id) else {
            return false;
        };
        let changed = window
            .get_active()
            .is_none_or(|active| active.tab_id() != tab_id);
        if changed {
            window.save_and_then_set_active(tab_idx);
        }
        drop(window);

        if changed {
            if let Some(tab) = mux.get_tab(tab_id) {
                if let Some(pane) = tab.get_active_pane() {
                    pane.focus_changed(true);
                }
            }
            self.update_title();
            self.update_scrollbar();
        }

        let Some(tab) = mux.get_tab(tab_id) else {
            return false;
        };
        let Some(active_pane) = tab.get_active_pane() else {
            return false;
        };
        let Some(client_pane) = active_pane.downcast_ref::<ClientPane>() else {
            return false;
        };
        let Some(domain) = mux.get_domain(client_pane.domain_id()) else {
            return false;
        };
        let Some(client_domain) = domain.downcast_ref::<ClientDomain>() else {
            return false;
        };
        if client_domain.pending_frontend_recovery(slot, tab_id) != Some(generation) {
            return false;
        }

        // Another device already owns B. Confirm that state through a
        // dedicated recovery report, but do not reshape this hidden mirror.
        if client_domain.owns_remote_viewport(tab_id) == Some(false) {
            let Some((domain, viewport)) = self.client_viewport_for_tab(&tab, false) else {
                return false;
            };
            let Some(client_domain) = domain.downcast_ref::<ClientDomain>() else {
                return false;
            };
            self.remember_gui_recovery_intent(client_domain, tab_id, &viewport);
            let Some(window) = self.window.as_ref().cloned() else {
                return false;
            };
            let domain_id = client_domain.domain_id();
            let domain_for_task = Arc::clone(&domain);
            promise::spawn::spawn(async move {
                let Some(client_domain) = domain_for_task.downcast_ref::<ClientDomain>() else {
                    return Ok::<(), anyhow::Error>(());
                };
                let result = client_domain.set_client_viewport(tab_id, viewport).await;
                match result {
                    Ok(_) if client_domain.owns_remote_viewport(tab_id) == Some(false) => {
                        client_domain.acknowledge_frontend_recovery(slot, tab_id, generation);
                    }
                    Ok(_) => {
                        // The passive report made this client the first owner.
                        // Re-enter on the GUI thread and install Native geometry
                        // before acknowledging the recovery barrier.
                        window.notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                            if !term_window
                                .sync_replacement_frontend_geometry(tab_id, slot, generation)
                            {
                                if let Some(domain) = Mux::get().get_domain(domain_id) {
                                    if let Some(client) = domain.downcast_ref::<ClientDomain>() {
                                        client.fail_frontend_recovery(
                                            slot,
                                            tab_id,
                                            generation,
                                            "GUI could not converge replacement geometry",
                                        );
                                    }
                                }
                            }
                        })));
                    }
                    Err(err) => {
                        client_domain.fail_frontend_recovery(
                            slot,
                            tab_id,
                            generation,
                            format!("GUI replacement viewport failed: {err:#}"),
                        );
                    }
                }
                Ok::<(), anyhow::Error>(())
            })
            .detach();
            return true;
        }

        self.sync_active_tab_geometry_now();
        let Some(phase) = self.frontend_geometry_phases.get(&tab_id).copied() else {
            return false;
        };
        let epoch = phase.epoch();
        self.frontend_geometry_phases.insert(
            tab_id,
            super::FrontendGeometryPhase::TakeoverSyncing { epoch },
        );
        self.frontend_recovery_geometry.insert(
            tab_id,
            super::FrontendRecoveryGeometry {
                epoch,
                domain_id: client_pane.domain_id(),
                slot,
                generation,
            },
        );
        self.invalidate_window();
        true
    }

    pub(crate) fn sync_active_tab_geometry_now(&mut self) {
        if self.content_view_foreground() {
            return;
        }
        let mux = Mux::get();
        let Some(tab) = mux.get_active_tab_for_window(self.mux_window_id) else {
            return;
        };
        let tab_id = tab.tab_id();
        self.cancel_remote_divider_resizes_except(Some(tab_id));
        let Some(active_pane) = tab.get_active_pane() else {
            return;
        };

        if active_pane.downcast_ref::<ClientPane>().is_none() {
            tab.resize(self.terminal_size);
            self.reapply_collapsed_panes_for_tab(tab_id);
            self.force_sync_active_mux_tab_pane_sizes();
            self.report_frontend_viewport_for_tab(&tab);
            self.invalidate_window();
            return;
        }

        let ownership = self.tab_frontend_viewport_ownership(&tab);
        let previewing = matches!(
            self.frontend_geometry_phases.get(&tab_id),
            Some(super::FrontendGeometryPhase::Previewing { .. })
        );
        let collaborative = self
            .active_frontend_access_state()
            .is_some_and(|state| state.mode == mux::FrontendAccessMode::TmuxLatest);

        // A known passive renderer advertises availability without reshaping
        // its mirror. The sole exception is an explicit A-mode layout preview:
        // releasing its divider claims and commits that final geometry.
        let action = frontend_geometry_action(ownership, collaborative, previewing);
        if action == FrontendGeometryAction::Passive {
            mux::zoom_trace!("gui.sync.skip tab={tab_id} reason=passive");
            self.frontend_geometry_phases.remove(&tab_id);
            self.report_frontend_viewport_for_tab(&tab);
            self.invalidate_window();
            return;
        }
        if self
            .frontend_geometry_phases
            .get(&tab_id)
            .copied()
            .is_some_and(super::FrontendGeometryPhase::is_in_flight)
        {
            // The post-zoom (or post-unzoom) viewport is not sent here; it is
            // owed to complete_frontend_geometry_epoch. If a transition ends
            // mismatched, check whether the matching gui.sync.followup ever
            // ran.
            mux::zoom_trace!(
                "gui.sync.defer tab={tab_id} reason=epoch_in_flight phase={:?}",
                self.frontend_geometry_phases.get(&tab_id)
            );
            self.frontend_geometry_resync_after_epoch.insert(tab_id);
            return;
        }
        let takeover = action == FrontendGeometryAction::Set { takeover: true };
        let claim = action == FrontendGeometryAction::Claim;
        let Some(epoch) = self.begin_frontend_geometry_epoch(tab_id, takeover) else {
            mux::zoom_trace!("gui.sync.skip tab={tab_id} reason=epoch_denied");
            return;
        };
        let Some((domain, viewport, adopted)) = self.prepare_client_frontend_geometry(&tab) else {
            mux::zoom_trace!("gui.sync.skip tab={tab_id} epoch={epoch} reason=prepare_failed");
            self.frontend_geometry_phases.remove(&tab_id);
            self.invalidate_window();
            return;
        };
        mux::zoom_trace!(
            "gui.sync.begin tab={tab_id} epoch={epoch} rpc={} takeover={takeover} \
             local_zoom={} adopted=[{}]",
            if claim { "claim" } else { "set" },
            tab.get_zoomed_pane()
                .map(|pane| pane.pane_id().to_string())
                .unwrap_or_else(|| "-".to_string()),
            adopted
                .iter()
                .map(|(pane_id, size)| format!("{pane_id}:{}", mux::geometrytrace::size(size)))
                .collect::<Vec<_>>()
                .join(" ")
        );
        if let Some(client_domain) = domain.downcast_ref::<ClientDomain>() {
            self.remember_gui_recovery_intent(client_domain, tab_id, &viewport);
        }
        let Some(window) = self.window.as_ref().cloned() else {
            self.frontend_geometry_phases.remove(&tab_id);
            return;
        };

        promise::spawn::spawn(async move {
            let result = match domain.downcast_ref::<ClientDomain>() {
                Some(client_domain) => {
                    let result = if claim {
                        client_domain.claim_client_viewport(tab_id, viewport).await
                    } else {
                        client_domain.set_client_viewport(tab_id, viewport).await
                    };
                    if let Err(err) = &result {
                        log::warn!("synchronizing active GUI tab geometry: {err:#}");
                        if let Err(resync_err) = client_domain.resync().await {
                            log::warn!(
                                "resyncing after failed active tab geometry: {resync_err:#}"
                            );
                        }
                    }
                    // SetClientViewport is intentionally non-claiming.  It
                    // succeeds even when another frontend owns B mode, in
                    // which case the submitted geometry was only recorded
                    // and will never be reflected by the server pane.  Do
                    // not wait forever for that impossible confirmation.
                    result.map(|_| client_domain.owns_remote_viewport(tab_id) == Some(true))
                }
                None => Err(anyhow::anyhow!("frontend domain is not a client domain")),
            };
            window.notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                let geometry_applied = result.as_ref().is_ok_and(|owns| *owns);
                term_window.finish_frontend_geometry_epoch(
                    tab_id,
                    epoch,
                    geometry_applied,
                    &adopted,
                );
            })));
            Ok::<(), anyhow::Error>(())
        })
        .detach();
    }

    /// Allow the next local publish for `tab_id` even if an identical one was
    /// rejected: an explicit user action (a claim, a mode change) means a
    /// retry is meaningful again.
    pub(crate) fn forget_rejected_local_viewport(&mut self, tab_id: mux::tab::TabId) {
        self.rejected_local_viewports.remove(&tab_id);
    }

    fn report_frontend_viewport_for_tab(&mut self, tab: &Arc<mux::tab::Tab>) {
        let mux = Mux::get();
        let Some(active_pane) = tab.get_active_pane() else {
            return;
        };
        let tab_id = tab.tab_id();
        // Only the tab being drawn can offer a pane split, and only when it
        // holds the lease; anything else describes a layout nobody sees.
        let include_panes = self.active_tab_is(tab_id) && self.tab_owns_frontend_viewport(tab);

        if active_pane.downcast_ref::<ClientPane>().is_some() {
            let Some((domain, viewport)) = self.client_viewport_for_tab(tab, include_panes) else {
                return;
            };
            if let Some(client_domain) = domain.downcast_ref::<ClientDomain>() {
                self.remember_gui_recovery_intent(client_domain, tab_id, &viewport);
            }
            let adopted = match &viewport {
                codec::ClientViewport::Native { panes, .. } => panes
                    .iter()
                    .map(|pane| (pane.pane_id, pane.size))
                    .collect::<Vec<_>>(),
                codec::ClientViewport::CellGrid { .. } => Vec::new(),
            };
            let window = self.window.as_ref().cloned();
            promise::spawn::spawn(async move {
                let Some(client_domain) = domain.downcast_ref::<ClientDomain>() else {
                    return Ok::<(), anyhow::Error>(());
                };
                if let Err(err) = client_domain.set_client_viewport(tab_id, viewport).await {
                    log::warn!("publishing GUI frontend viewport: {err:#}");
                    if let Err(resync_err) = client_domain.resync().await {
                        log::warn!("resyncing after failed GUI viewport: {resync_err:#}");
                    }
                    if let Some(window) = window {
                        window.notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                            let mux = Mux::get();
                            for (pane_id, size) in &adopted {
                                if let Some(pane) = mux.get_pane(*pane_id) {
                                    if let Some(client) = pane.downcast_ref::<ClientPane>() {
                                        client.forget_frontend_geometry(*size);
                                    }
                                }
                            }
                            term_window.invalidate_window();
                        })));
                    }
                }
                Ok(())
            })
            .detach();
            return;
        }
        let owns = include_panes;

        // This GUI is itself hosting the authoritative mux. Register its
        // LocalPane viewport under the GUI's normal ClientId just like a
        // remote renderer. Otherwise the first attached TUI silently becomes
        // owner while the GUI continues resizing the same PTYs directly.
        let Some(client_id) = mux.active_identity() else {
            return;
        };
        let viewport = self.local_frontend_viewport_for_tab(tab, owns);
        let shape = LocalTabShape::of(tab);
        if !local_viewport_publish_is_worthwhile(
            self.rejected_local_viewports.get(&tab_id),
            &viewport,
            &shape,
        ) {
            mux::zoom_trace!("gui.viewport.skip tab={tab_id} reason=rejected");
            return;
        }
        match mux.set_client_viewport(&client_id, tab_id, viewport.clone()) {
            Ok(_) => {
                self.rejected_local_viewports.remove(&tab_id);
            }
            Err(err) => {
                log::warn!("cannot publish local GUI viewport: {err:#}");
                self.rejected_local_viewports
                    .insert(tab_id, RejectedLocalViewport { viewport, shape });
            }
        }
    }

    fn local_frontend_viewport_for_tab(
        &self,
        tab: &Arc<mux::tab::Tab>,
        include_panes: bool,
    ) -> mux::FrontendViewport {
        let panes = if include_panes {
            let mut panes = Vec::new();
            for positioned in self.get_panes_to_render() {
                let Some(members) = self.positioned_panes_for_stack(tab, &positioned) else {
                    continue;
                };
                for member in members {
                    if member.pane.is_remote_mirror() {
                        continue;
                    }
                    let Some(viewport) = self.frontend_viewport_for_positioned_pane(&member) else {
                        continue;
                    };
                    panes.push(mux::FrontendPaneViewport {
                        pane_id: viewport.pane_id,
                        size: viewport.size,
                        frame: viewport.frame,
                    });
                }
            }
            panes
        } else {
            Vec::new()
        };
        mux::FrontendViewport::Native {
            size: self.terminal_size,
            panes,
        }
    }

    fn normalized_font_scale_value(&self, font_scale: Option<f64>) -> Option<f64> {
        let global_scale = self.fonts.get_font_scale();
        font_scale
            .filter(|scale| scale.is_finite() && *scale > 0.0)
            .filter(|scale| scale.to_bits() != global_scale.to_bits())
    }

    fn persisted_font_scale_for_pane(&self, pane_id: PaneId) -> Option<f64> {
        let font_scale = self
            .pane_state
            .borrow()
            .get(&pane_id)
            .and_then(|state| state.font_scale);
        self.normalized_font_scale_value(font_scale)
    }

    pub(crate) fn snapshot_active_workspace_thread_layout(&self) {
        let mux = Mux::get();
        let Some(window) = mux.get_window(self.mux_window_id) else {
            return;
        };
        let space_id = self.space_id_for_layout_snapshot(window.get_workspace());
        crate::workspace_threads::snapshot_active_space_thread_layout_with_font_scales(
            &space_id,
            window.get_workspace(),
            self.mux_window_id,
            |pane_id| self.persisted_font_scale_for_pane(pane_id),
        );
    }

    /// The Space a layout snapshot should be recorded under. A window
    /// displaying a thread reference shows a workspace that belongs to
    /// another Space, and the snapshot store refuses a Space/workspace
    /// mismatch — so resolve the workspace's own Space; an unbound
    /// workspace falls back to the window's Space.
    fn space_id_for_layout_snapshot(&self, workspace: &str) -> String {
        crate::workspace_threads::space_id_for_workspace(workspace)
            .unwrap_or_else(|| self.active_space_id.clone())
    }

    fn persist_workspace_pane_font_scales(&self) {
        let mux = Mux::get();
        let Some(window) = mux.get_window(self.mux_window_id) else {
            return;
        };
        let space_id = self.space_id_for_layout_snapshot(window.get_workspace());
        crate::workspace_threads::snapshot_active_space_thread_layout_with_font_scales(
            &space_id,
            window.get_workspace(),
            self.mux_window_id,
            |pane_id| self.persisted_font_scale_for_pane(pane_id),
        );
    }

    fn apply_font_scales_to_mux_window_panes(
        &mut self,
        font_scales: HashMap<PaneId, Option<f64>>,
    ) -> bool {
        let mux = Mux::get();
        let Some(mux_window) = mux.get_window(self.mux_window_id) else {
            return false;
        };

        let mut changed = false;
        for tab in mux_window.iter() {
            for pos in tab.iter_panes_ignoring_zoom() {
                let pane_id = pos.pane.pane_id();
                let font_scale = self.normalized_font_scale_value(
                    font_scales.get(&pane_id).copied().unwrap_or(None),
                );
                let mut state = self.pane_state(pos.pane.pane_id());
                if state.font_scale != font_scale {
                    state.font_scale = font_scale;
                    changed = true;
                }
            }
        }
        if !changed {
            // Called per TabAddedToWindow during resyncs; skip the cache
            // flush when nothing actually changed.
            return false;
        }

        self.quad_generation += 1;
        self.shape_generation += 1;
        self.pane_font_cache.borrow_mut().clear();
        self.shape_cache.borrow_mut().clear();
        self.ui_shape_caches.borrow_mut().clear_all();
        self.publish_ui_shape_cache_diagnostics();
        self.line_to_ele_shape_cache.borrow_mut().clear();
        self.invalidate_fancy_tab_bar();
        self.invalidate_modal();
        if let Some(window) = self.window.as_ref() {
            window.invalidate();
        }
        true
    }

    /// Restore the destination thread's per-pane scale state without resizing
    /// any pane.  Adoption paths use this to stage every geometry input before
    /// issuing their single, final pane-size synchronization.
    pub(crate) fn stage_workspace_thread_font_scales(&mut self) -> bool {
        let mux = Mux::get();
        let Some(window) = mux.get_window(self.mux_window_id) else {
            return false;
        };
        let Some(font_scales) = crate::workspace_threads::workspace_pane_font_scales(
            window.get_workspace(),
            self.mux_window_id,
        ) else {
            return false;
        };
        self.apply_font_scales_to_mux_window_panes(font_scales)
    }

    pub(crate) fn apply_workspace_thread_font_scales(&mut self) {
        if self.stage_workspace_thread_font_scales() {
            self.sync_pane_font_sizes();
        }
    }

    pub(crate) fn apply_native_terminal_settings(&mut self) {
        let settings = crate::native_settings::load();
        let Some(font_size) = settings.terminal.font_size else {
            return;
        };
        if !font_size.is_finite() || font_size <= 0.0 || self.config.font_size <= 0.0 {
            return;
        }
        let font_scale = (font_size / self.config.font_size).clamp(0.25, 4.0);
        if let Some(window) = self.window.as_ref().cloned() {
            self.adjust_font_scale(font_scale, &window);
        }
    }

    pub(crate) fn resize_layout_for_dimensions(
        &self,
        dimensions: &Dimensions,
        scale_changed_cells: Option<RowsAndCols>,
    ) -> (TerminalSize, Dimensions, ResizeIncrementCalculator) {
        let config = &self.config;

        let tab_bar_height = if self.show_tab_bar {
            self.tab_bar_pixel_height().unwrap_or(0.)
        } else {
            0.
        };

        let border = self.get_os_border();

        if let Some(cell_dims) = scale_changed_cells {
            // Scaling preserves existing terminal dimensions, yielding a new
            // overall set of window dimensions
            let size = TerminalSize {
                rows: cell_dims.rows,
                cols: cell_dims.cols,
                pixel_height: cell_dims.rows * self.render_metrics.cell_size.height as usize,
                pixel_width: cell_dims.cols * self.render_metrics.cell_size.width as usize,
                dpi: dimensions.dpi as u32,
            };

            let rows = size.rows;
            let cols = size.cols;

            let h_context = DimensionContext {
                dpi: dimensions.dpi as f32,
                pixel_max: size.pixel_width as f32,
                pixel_cell: self.render_metrics.cell_size.width as f32,
            };
            let v_context = DimensionContext {
                dpi: dimensions.dpi as f32,
                pixel_max: size.pixel_height as f32,
                pixel_cell: self.render_metrics.cell_size.height as f32,
            };
            let padding_left = config.window_padding.left.evaluate_as_pixels(h_context) as usize
                + self.workspace_sidebar_width();
            let padding_top = config.window_padding.top.evaluate_as_pixels(v_context) as usize;
            let padding_bottom =
                config.window_padding.bottom.evaluate_as_pixels(v_context) as usize;
            let padding_right =
                effective_right_padding(&config, h_context) + self.right_sidebar_width();

            let pixel_height = (rows * self.render_metrics.cell_size.height as usize)
                + (padding_top + padding_bottom)
                + (border.top + border.bottom).get() as usize
                + tab_bar_height as usize;

            let pixel_width = (cols * self.render_metrics.cell_size.width as usize)
                + (padding_left + padding_right)
                + (border.left + border.right).get() as usize;

            let dims = Dimensions {
                pixel_width: pixel_width as usize,
                pixel_height: pixel_height as usize,
                dpi: dimensions.dpi,
            };

            let ri_calc = ResizeIncrementCalculator {
                x: self.render_metrics.cell_size.width as u16,
                y: self.render_metrics.cell_size.height as u16,
                padding_left: padding_left,
                padding_top: padding_top,
                padding_right: padding_right,
                padding_bottom: padding_bottom,
                border: border,
                tab_bar_height: tab_bar_height as usize,
            };

            (size, dims, ri_calc)
        } else {
            // Resize of the window dimensions may result in changed terminal dimensions

            let h_context = DimensionContext {
                dpi: dimensions.dpi as f32,
                pixel_max: self.terminal_size.pixel_width as f32,
                pixel_cell: self.render_metrics.cell_size.width as f32,
            };
            let v_context = DimensionContext {
                dpi: dimensions.dpi as f32,
                pixel_max: self.terminal_size.pixel_height as f32,
                pixel_cell: self.render_metrics.cell_size.height as f32,
            };
            let padding_left = config.window_padding.left.evaluate_as_pixels(h_context) as usize
                + self.workspace_sidebar_width();
            let padding_top = config.window_padding.top.evaluate_as_pixels(v_context) as usize;
            let padding_bottom =
                config.window_padding.bottom.evaluate_as_pixels(v_context) as usize;
            let padding_right =
                effective_right_padding(&config, h_context) + self.right_sidebar_width();

            let avail_width = dimensions.pixel_width.saturating_sub(
                (padding_left + padding_right) as usize
                    + (border.left + border.right).get() as usize,
            );
            let avail_height = dimensions
                .pixel_height
                .saturating_sub(
                    (padding_top + padding_bottom) as usize
                        + (border.top + border.bottom).get() as usize,
                )
                .saturating_sub(tab_bar_height as usize);

            let rows = avail_height / self.render_metrics.cell_size.height as usize;
            let cols = avail_width / self.render_metrics.cell_size.width as usize;

            let size = TerminalSize {
                rows,
                cols,
                // Take care to use the exact pixel dimensions of the cells, rather
                // than the available space, so that apps that are sensitive to
                // the pixels-per-cell have consistent values at a given font size.
                // https://github.com/wezterm/wezterm/issues/535
                pixel_height: rows * self.render_metrics.cell_size.height as usize,
                pixel_width: cols * self.render_metrics.cell_size.width as usize,
                dpi: dimensions.dpi as u32,
            };

            let ri_calc = ResizeIncrementCalculator {
                x: self.render_metrics.cell_size.width as u16,
                y: self.render_metrics.cell_size.height as u16,
                padding_left: padding_left,
                padding_top: padding_top,
                padding_right: padding_right,
                padding_bottom: padding_bottom,
                border: border,
                tab_bar_height: tab_bar_height as usize,
            };

            (size, *dimensions, ri_calc)
        }
    }

    /// Expand the currently visible member of a pane stack into every level-2
    /// tab that occupies the same split rectangle. Hidden members need their
    /// own PTY/render target before they become active; otherwise divider or
    /// window resize leaves them on their last visible geometry and the first
    /// frame after a level-2 switch visibly jumps.
    fn positioned_panes_for_stack(
        &self,
        tab: &Arc<mux::tab::Tab>,
        positioned: &PositionedPane,
    ) -> Option<Vec<PositionedPane>> {
        let stack_tabs = tab.pane_stack_tabs(positioned.pane.pane_id());
        if stack_tabs.is_empty() {
            return Some(vec![positioned.clone()]);
        }

        let mux = Mux::get();
        stack_tabs
            .into_iter()
            .map(|stack_tab| {
                let mut member = positioned.clone();
                member.pane = mux.get_pane(stack_tab.pane_id)?;
                member.is_active = positioned.is_active && stack_tab.is_active;
                member.is_zoomed = positioned.is_zoomed && stack_tab.is_active;
                Some(member)
            })
            .collect()
    }

    /// A stack shares a frame but not necessarily a terminal grid: pane-local
    /// font scaling means each member must convert that frame using its own
    /// cell metrics.
    fn frontend_viewport_for_positioned_pane(
        &self,
        positioned: &PositionedPane,
    ) -> Option<codec::ClientPaneViewport> {
        let font_scale = self.pane_font_scale(positioned.pane.pane_id());
        let metrics = if font_scale.to_bits() == self.fonts.get_font_scale().to_bits() {
            self.render_metrics
        } else {
            match self.pane_font_resources(font_scale) {
                Ok((_, metrics)) => metrics,
                Err(err) => {
                    log::warn!("cannot calculate pane-stack viewport: {err:#}");
                    return None;
                }
            }
        };
        Some(codec::ClientPaneViewport {
            pane_id: positioned.pane.pane_id(),
            size: self.terminal_size_for_positioned_pane(positioned, metrics),
            frame: self.frontend_frame_for_positioned_pane(positioned),
        })
    }

    pub(crate) fn terminal_size_for_positioned_pane(
        &self,
        pos: &PositionedPane,
        render_metrics: RenderMetrics,
    ) -> TerminalSize {
        let cell_width = render_metrics.cell_size.width.max(1) as usize;
        let cell_height = render_metrics.cell_size.height.max(1) as usize;
        let pane_nav_height = self
            .pane_nav_bar_height()
            .min(pos.pixel_height.saturating_sub(cell_height));
        let pixel_width = pos.pixel_width.max(cell_width);
        let pixel_height = pos
            .pixel_height
            .saturating_sub(pane_nav_height)
            .max(cell_height);
        let cols = (pixel_width / cell_width).max(1);
        let rows = (pixel_height / cell_height).max(1);

        TerminalSize {
            rows,
            cols,
            pixel_width: cols * cell_width,
            pixel_height: rows * cell_height,
            dpi: self.dimensions.dpi as u32,
        }
    }

    fn frontend_frame_for_positioned_pane(&self, pos: &PositionedPane) -> TerminalSize {
        TerminalSize {
            rows: pos.height,
            cols: pos.width,
            pixel_width: pos.pixel_width,
            pixel_height: pos.pixel_height,
            dpi: self.dimensions.dpi as u32,
        }
    }

    fn sync_positioned_pane_font_size(&self, pos: &PositionedPane) -> anyhow::Result<()> {
        let pane_id = pos.pane.pane_id();
        let font_scale = self.pane_font_scale(pane_id);
        let render_metrics = if font_scale.to_bits() == self.fonts.get_font_scale().to_bits() {
            self.render_metrics
        } else {
            self.pane_font_resources(font_scale)?.1
        };
        let target_size = self.terminal_size_for_positioned_pane(pos, render_metrics);
        let dims = pos.pane.get_dimensions();

        // Register the metrics behind target_size so mux-side split-tree
        // resizes (cascade during divider drags) compute this same size for
        // the pane instead of the raw root-cell size — the two writers must
        // agree or the PTY ping-pongs between their answers every frame.
        if pos.pane.downcast_ref::<ClientPane>().is_none() {
            mux::pane::set_frontend_cell_metrics(
                pane_id,
                Some(mux::pane::FrontendCellMetrics {
                    cell_width: render_metrics.cell_size.width.max(1) as usize,
                    cell_height: render_metrics.cell_size.height.max(1) as usize,
                    chrome_height: self.pane_nav_bar_height(),
                    dpi: self.dimensions.dpi as u32,
                }),
            );
        }

        if dims.cols != target_size.cols
            || dims.viewport_rows != target_size.rows
            || dims.pixel_width != target_size.pixel_width
            || dims.pixel_height != target_size.pixel_height
            || dims.dpi != target_size.dpi
        {
            if let Some(client) = pos.pane.downcast_ref::<ClientPane>() {
                client.adopt_frontend_geometry(target_size);
            } else {
                log::debug!(
                    target: "sizetrace",
                    "gui sync pane {} {}x{} -> {}x{} (scale {} pos {}x{}px nav {})",
                    pane_id,
                    dims.cols,
                    dims.viewport_rows,
                    target_size.cols,
                    target_size.rows,
                    font_scale,
                    pos.pixel_width,
                    pos.pixel_height,
                    self.pane_nav_bar_height(),
                );
                pos.pane.resize(target_size)?;
            }
        }

        Ok(())
    }

    fn sync_active_mux_tab_pane_sizes(&self) {
        if !self.owns_frontend_viewport() {
            // Another frontend holds this tab's lease; resizing its PTYs from
            // here would fight it. Logged because a *wrongly* closed gate is
            // invisible otherwise: it silently strands every pane at its old
            // size during divider drags.
            log::debug!("skipping pane size sync: frontend viewport not owned");
            return;
        }
        let Some(tab) = Mux::get().get_active_tab_for_window(self.mux_window_id) else {
            return;
        };
        for positioned in self.get_panes_to_render() {
            let Some(members) = self.positioned_panes_for_stack(&tab, &positioned) else {
                continue;
            };
            for pos in members {
                if let Err(err) = self.sync_positioned_pane_font_size(&pos) {
                    log::error!(
                        "failed to sync font-scaled pane size for pane {}: {:#}",
                        pos.pane.pane_id(),
                        err
                    );
                }
            }
        }
    }

    pub fn sync_pane_font_sizes(&self) {
        if self.content_view_foreground() {
            return;
        }
        self.sync_active_mux_tab_pane_sizes();
    }

    /// Synchronize the newly adopted mux window even if a connection content
    /// view is still foreground. Remote mirror panes are intentionally skipped
    /// by Tab::resize, so attach/switch paths must explicitly size each pane.
    pub(crate) fn force_sync_active_mux_tab_pane_sizes(&self) {
        self.sync_active_mux_tab_pane_sizes();
    }

    pub(crate) fn resize_mux_tabs_to_current_terminal_size(&mut self) {
        let mux = Mux::get();
        let Some(tab) = mux.get_active_tab_for_window(self.mux_window_id) else {
            return;
        };
        match self.tab_frontend_viewport_ownership(&tab) {
            None => {
                // This is the common first-open race: the remote topology is
                // already present but its access snapshot has not arrived.
                // A passive report can make this client the first owner while
                // leaving the old renderer geometry visible. Route it through
                // the opaque takeover epoch so root and pane surfaces are
                // converged before the first terminal frame.
                self.sync_active_tab_geometry_now();
                return;
            }
            Some(false) => {
                self.report_frontend_viewport();
                return;
            }
            Some(true) => {}
        }
        tab.resize(self.terminal_size);
        self.reapply_collapsed_panes_for_tab(tab.tab_id());
        self.force_sync_active_mux_tab_pane_sizes();
        self.report_frontend_viewport();
    }

    /// Repair a missed/deferred sidebar reflow before terminal geometry is
    /// consumed by paint.  Most size changes arrive through
    /// `apply_dimensions`, but a content-view handoff or remote mux resync can
    /// briefly leave the active tab carrying the prior full-width geometry.
    ///
    /// The comparison is important: `Tab::resize` emits `TabResized`, which a
    /// client domain answers with a resync.  Only repairing a real mismatch
    /// keeps this recovery path convergent rather than creating a resize loop.
    pub(crate) fn reconcile_active_mux_tab_size_before_paint(&mut self) {
        if self.content_view_foreground() {
            return;
        }
        if !self.owns_frontend_viewport() {
            return;
        }

        let mux = Mux::get();
        let Some(tab) = mux.get_active_tab_for_window(self.mux_window_id) else {
            return;
        };
        if tab.get_size() == self.terminal_size {
            return;
        }

        log::debug!(
            "repairing active tab {} geometry before paint: {:?} -> {:?}",
            tab.tab_id(),
            tab.get_size(),
            self.terminal_size
        );
        if !tab.resize(self.terminal_size) {
            return;
        }
        self.reapply_collapsed_panes_for_tab(tab.tab_id());
        self.force_sync_active_mux_tab_pane_sizes();
    }

    pub fn resize(
        &mut self,
        dimensions: Dimensions,
        window_state: WindowState,
        window: &Window,
        live_resizing: bool,
    ) {
        log::trace!(
            "resize event, live={} current cells: {:?}, current dims: {:?}, new dims: {:?} window_state:{:?}",
            live_resizing,
            self.current_cell_dimensions(),
            self.dimensions,
            dimensions,
            window_state,
        );
        if dimensions.pixel_width == 0 || dimensions.pixel_height == 0 {
            // on windows, this can happen when minimizing the window.
            // NOP!
            log::trace!("new dimensions are zero: NOP!");
            return;
        }
        let content_view_resize_state_changed = self
            .active_content_view_mut()
            .is_some_and(|view| view.set_live_resizing(live_resizing));
        if self.dimensions == dimensions && self.window_state == window_state {
            // It didn't really change
            log::trace!("dimensions didn't change NOP!");
            if content_view_resize_state_changed {
                window.invalidate();
            }
            return;
        }
        super::gpu_debug(format!(
            "main_window resize live={} {}x{} -> {}x{} dpi {} -> {} backend={}",
            live_resizing,
            self.dimensions.pixel_width,
            self.dimensions.pixel_height,
            dimensions.pixel_width,
            dimensions.pixel_height,
            self.dimensions.dpi,
            dimensions.dpi,
            if self.webgpu.is_some() {
                "WebGpu"
            } else if self.gl.is_some() {
                "OpenGL"
            } else {
                "none"
            }
        ));
        let last_state = self.window_state;
        self.window_state = window_state;
        self.quad_generation += 1;
        if last_state != self.window_state {
            self.load_os_parameters();
        }

        if let Some(webgpu) = self.webgpu.as_mut() {
            webgpu.resize(dimensions);
        }

        // For simple, user-interactive resizes where the dpi doesn't change,
        // skip our scaling recalculation
        if live_resizing && self.dimensions.dpi == dimensions.dpi {
            self.apply_dimensions(&dimensions, None, window);
        } else {
            self.scaling_changed(dimensions, self.fonts.get_font_scale(), window);
        }
        if let Some(modal) = self.get_modal() {
            modal.reconfigure(self);
        }
        self.emit_window_event("window-resized", None);
    }

    pub fn apply_pending_scale_changes(&mut self) {
        while self.resizes_pending == 0 {
            match self.pending_scale_changes.pop_front() {
                Some(ScaleChange::Relative(change)) => {
                    if let Some(window) = self.window.as_ref().map(|w| w.clone()) {
                        self.adjust_font_scale(self.fonts.get_font_scale() * change, &window);
                    }
                }
                Some(ScaleChange::Absolute(change)) => {
                    if let Some(window) = self.window.as_ref().map(|w| w.clone()) {
                        self.adjust_font_scale(change, &window);
                    }
                }
                None => break,
            }
        }
    }

    pub fn apply_scale_change(&mut self, dimensions: &Dimensions, font_scale: f64) {
        let config = &self.config;
        let font_size = config.font_size * font_scale;
        let theoretical_height = font_size * dimensions.dpi as f64 / 72.0;

        if theoretical_height < 2.0 {
            log::warn!(
                "refusing to go to an unreasonably small font scale {:?}
                       font_scale={} would yield font_height {}",
                dimensions,
                font_scale,
                theoretical_height
            );
            return;
        }

        let (prior_font, prior_dpi) = self.fonts.change_scaling(font_scale, dimensions.dpi);
        match RenderMetrics::new(&self.fonts) {
            Ok(metrics) => {
                self.render_metrics = metrics;
            }
            Err(err) => {
                log::error!(
                    "{:#} while attempting to scale font to {} with {:?}",
                    err,
                    font_scale,
                    dimensions
                );
                // Restore prior scaling factors
                self.fonts.change_scaling(prior_font, prior_dpi);
            }
        }

        if let Err(err) = self.recreate_texture_atlas(None) {
            log::error!("recreate_texture_atlas: {:#}", err);
        }
        self.invalidate_fancy_tab_bar();
        self.invalidate_modal();
    }

    pub fn apply_dimensions(
        &mut self,
        dimensions: &Dimensions,
        mut scale_changed_cells: Option<RowsAndCols>,
        window: &Window,
    ) {
        log::trace!(
            "apply_dimensions {:?} scale_changed_cells {:?}. window_state {:?}",
            dimensions,
            scale_changed_cells,
            self.window_state
        );
        let saved_dims = self.dimensions;
        self.dimensions = *dimensions;
        self.quad_generation += 1;

        if scale_changed_cells.is_some() && !self.window_state.can_resize() {
            log::warn!(
                "cannot resize window to match {:?} because window_state is {:?}",
                scale_changed_cells,
                self.window_state
            );
            scale_changed_cells.take();
        }

        // Technically speaking, we should compute the rows and cols
        // from the new dimensions and apply those to the tabs, and
        // then for the scaling changed case, try to re-apply the
        // original rows and cols, but if we do that we end up
        // double resizing the tabs, so we speculatively apply the
        // final size, which in that case should result in a NOP
        // change to the tab size.

        let (size, dims, ri_calc) =
            self.resize_layout_for_dimensions(dimensions, scale_changed_cells);

        log::trace!("apply_dimensions computed size {:?}, dims {:?}", size, dims);

        let terminal_size_changed = self.terminal_size != size;
        self.terminal_size = size;
        if terminal_size_changed {
            if self.content_view_foreground() {
                self.content_view_deferred_mux_resize = true;
                log::trace!("content view foreground; deferring mux tab resize");
            } else {
                self.resize_mux_tabs_to_current_terminal_size();
            }
        } else {
            self.report_frontend_viewport();
            log::trace!("terminal size unchanged; syncing active pane geometry");
            if !self.content_view_foreground() {
                self.sync_pane_font_sizes();
            }
        }

        self.resize_overlays();
        self.invalidate_fancy_tab_bar();
        self.update_title();

        window.set_resize_increments(if self.config.use_resize_increments {
            ri_calc.into()
        } else {
            ResizeIncrement::disabled()
        });

        // Queue up a speculative resize in order to preserve the number of rows+cols
        if let Some(cell_dims) = scale_changed_cells {
            // If we don't think the dimensions have changed, don't request
            // the window to change.  This seems to help on Wayland where
            // we won't know what size the compositor thinks we should have
            // when we're first opened, until after it sends us a configure event.
            // If we send this too early, it will trump that configure event
            // and we'll end up with weirdness where our window renders in the
            // middle of a larger region that the compositor thinks we live in.
            // Wayland is weird!
            if saved_dims != dims {
                log::trace!(
                    "scale changed so resize from {:?} to {:?} {:?} (event called with {:?})",
                    saved_dims,
                    dims,
                    cell_dims,
                    dimensions
                );
                // Stash this size pre-emptively. Without this, on Windows,
                // when the font scaling is changed we can end up not seeing
                // these dimensions and the scaling_changed logic ends up
                // comparing two dimensions that have the same DPI and recomputing
                // an adjusted terminal size.
                // eg: rather than a simple old-dpi -> new dpi transition, we'd
                // see old-dpi -> new dpi, call set_inner_size, then see a
                // new-dpi -> new-dpi adjustment with a slightly different
                // pixel geometry which is considered to be a user-driven resize.
                // Stashing the dimensions here avoids that misconception.
                self.dimensions = dims;
                self.set_inner_size(window, dims.pixel_width, dims.pixel_height);
            }
        }
    }

    pub fn current_cell_dimensions(&self) -> RowsAndCols {
        RowsAndCols {
            rows: self.terminal_size.rows as usize,
            cols: self.terminal_size.cols as usize,
        }
    }

    #[allow(clippy::float_cmp)]
    pub fn scaling_changed(&mut self, dimensions: Dimensions, font_scale: f64, window: &Window) {
        fn dpi_adjusted(n: usize, dpi: usize) -> f32 {
            n as f32 / dpi as f32
        }

        /// On Windows, scaling changes may adjust the pixel geometry by a few pixels,
        /// so this function checks if we're in a close-enough ballpark.
        fn close_enough(a: f32, b: f32) -> bool {
            let diff = (a - b).abs();
            diff < 10.
        }

        // Distinguish between eg: dpi being detected as double the initial dpi (where
        // the pixel dimensions don't change), and the dpi change being detected, but
        // where the window manager also decides to tile/resize the window.
        // In the latter case, we don't want to preserve the terminal rows/cols.
        let simple_dpi_change = dimensions.dpi != self.dimensions.dpi
            && ((close_enough(
                dpi_adjusted(dimensions.pixel_height, dimensions.dpi),
                dpi_adjusted(self.dimensions.pixel_height, self.dimensions.dpi),
            ) && close_enough(
                dpi_adjusted(dimensions.pixel_width, dimensions.dpi),
                dpi_adjusted(self.dimensions.pixel_width, self.dimensions.dpi),
            )) || (close_enough(
                dimensions.pixel_width as f32,
                self.dimensions.pixel_width as f32,
            ) && close_enough(
                dimensions.pixel_height as f32,
                self.dimensions.pixel_height as f32,
            )));

        if simple_dpi_change && cfg!(target_os = "macos") {
            // Spooky action at a distance: on macOS, NSWindow::isZoomed can falsely
            // return YES in situations such as the current screen changing.
            // That causes window_state to believe that we are MAXIMIZED.
            // We cannot easily detect that in the window layer, but at this
            // layer, if we realize that the dpi was the only thing that changed
            // then remove the MAXIMIZED state so that the can_resize check
            // in adjust_font_scale will not block us from adapting to the new
            // DPI. This is gross and it would be better handled at the macOS
            // layer.
            // <https://github.com/wezterm/wezterm/issues/3503>
            self.window_state -= WindowState::MAXIMIZED;
        }

        let dpi_changed = dimensions.dpi != self.dimensions.dpi;
        let font_scale_changed = font_scale != self.fonts.get_font_scale();
        let scale_changed = dpi_changed || font_scale_changed;

        log::trace!(
            "dpi_changed={}, font_scale_changed={} scale_changed={} simple_dpi_change={}",
            dpi_changed,
            font_scale_changed,
            scale_changed,
            simple_dpi_change
        );

        let cell_dims = self.current_cell_dimensions();

        if dpi_changed {
            let old_dpi = self.dimensions.dpi;
            let new_dpi = dimensions.dpi;
            self.workspace_sidebar_width =
                rescale_ui_usize(self.workspace_sidebar_width, old_dpi, new_dpi);
            self.right_sidebar_width = rescale_ui_usize(self.right_sidebar_width, old_dpi, new_dpi);
            self.right_sidebar_file_tree_width =
                rescale_ui_usize(self.right_sidebar_file_tree_width, old_dpi, new_dpi);
            self.right_sidebar_file_preview_width =
                rescale_ui_usize(self.right_sidebar_file_preview_width, old_dpi, new_dpi);
            self.right_sidebar_note_pane_width =
                rescale_ui_usize(self.right_sidebar_note_pane_width, old_dpi, new_dpi);
        }

        if scale_changed {
            self.apply_scale_change(&dimensions, font_scale);
        }

        let scale_changed_cells = if font_scale_changed || simple_dpi_change {
            Some(cell_dims)
        } else {
            None
        };

        log::trace!(
            "scaling_changed, follow with applying dimensions. scale_changed_cells={:?}",
            scale_changed_cells
        );
        self.apply_dimensions(&dimensions, scale_changed_cells, window);
    }

    /// Used for applying font size changes only; this takes into account
    /// the `adjust_window_size_when_changing_font_size` configuration and
    /// revises the scaling/resize change accordingly
    pub fn adjust_font_scale(&mut self, font_scale: f64, window: &Window) {
        let adjust_window_size_when_changing_font_size =
            match self.config.adjust_window_size_when_changing_font_size {
                Some(value) => value,
                None => {
                    let is_tiling = self
                        .config
                        .tiling_desktop_environments
                        .iter()
                        .any(|item| item.as_str() == self.connection_name.as_str());
                    !is_tiling
                }
            };

        if self.window_state.can_resize() && adjust_window_size_when_changing_font_size {
            self.scaling_changed(self.dimensions, font_scale, window);
        } else {
            let dimensions = self.dimensions;
            // Compute new font metrics
            self.apply_scale_change(&dimensions, font_scale);
            // Now revise the pty size to fit the window
            self.apply_dimensions(&dimensions, None, window);
        }
    }

    fn adjust_active_pane_font_scale(&mut self, font_scale: f64) {
        let pane_id = match self.get_active_pane_no_overlay() {
            Some(pane) => pane.pane_id(),
            None => {
                if let Some(window) = self.window.as_ref().map(|w| w.clone()) {
                    self.adjust_font_scale(font_scale, &window);
                }
                return;
            }
        };

        let font_size = self.config.font_size * font_scale;
        let theoretical_height = font_size * self.dimensions.dpi as f64 / 72.0;

        if theoretical_height < 2.0 {
            log::warn!(
                "refusing to go to an unreasonably small pane font scale {:?}
                       font_scale={} would yield font_height {}",
                self.dimensions,
                font_scale,
                theoretical_height
            );
            return;
        }

        let global_scale = self.fonts.get_font_scale();
        {
            let mut state = self.pane_state(pane_id);
            state.font_scale = if font_scale.to_bits() == global_scale.to_bits() {
                None
            } else {
                Some(font_scale)
            };
        }
        self.persist_workspace_pane_font_scales();

        // A pane-local font change leaves the tab root unchanged, but changes
        // the number of rows and columns that fit inside this pane.  Adopting
        // only the local ClientPane surface makes the renderer look resized
        // while the remote PTY keeps its previous geometry.  Converge the
        // complete active-tab viewport so Cmd/Ctrl +/-/0, menu actions and
        // split panes all update the authoritative PTY size as one layout.
        self.sync_active_tab_geometry_now();
        self.quad_generation += 1;
        self.shape_generation += 1;
        self.shape_cache.borrow_mut().clear();
        self.ui_shape_caches.borrow_mut().clear_all();
        self.publish_ui_shape_cache_diagnostics();
        self.line_to_ele_shape_cache.borrow_mut().clear();
        self.invalidate_fancy_tab_bar();
        self.invalidate_modal();

        if let Some(window) = self.window.as_ref() {
            window.invalidate();
        }
    }

    pub fn decrease_font_size(&mut self) {
        if let Some(pane) = self.get_active_pane_no_overlay() {
            let font_scale = self.pane_font_scale(pane.pane_id());
            self.adjust_active_pane_font_scale(font_scale * (1.0 / 1.1));
        } else {
            self.pending_scale_changes
                .push_back(ScaleChange::Relative(1.0 / 1.1));
            self.apply_pending_scale_changes();
        }
    }

    pub fn increase_font_size(&mut self) {
        if let Some(pane) = self.get_active_pane_no_overlay() {
            let font_scale = self.pane_font_scale(pane.pane_id());
            self.adjust_active_pane_font_scale(font_scale * 1.1);
        } else {
            self.pending_scale_changes
                .push_back(ScaleChange::Relative(1.1));
            self.apply_pending_scale_changes();
        }
    }

    pub fn reset_font_size(&mut self) {
        if self.get_active_pane_no_overlay().is_some() {
            self.adjust_active_pane_font_scale(self.fonts.get_font_scale());
        } else {
            self.pending_scale_changes
                .push_back(ScaleChange::Absolute(1.0));
            self.apply_pending_scale_changes();
        }
    }

    pub fn set_window_size(&mut self, size: TerminalSize, window: &Window) -> anyhow::Result<()> {
        let config = &self.config;
        let fontconfig = Rc::new(FontConfiguration::new(
            Some(config.clone()),
            self.dimensions.dpi,
        )?);
        let render_metrics = RenderMetrics::new(&fontconfig)?;

        let terminal_size = TerminalSize {
            rows: size.rows,
            cols: size.cols,
            pixel_width: (render_metrics.cell_size.width as usize * size.cols),
            pixel_height: (render_metrics.cell_size.height as usize * size.rows),
            dpi: size.dpi,
        };

        let show_tab_bar = config.enable_tab_bar && !config.hide_tab_bar_if_only_one_tab;
        let tab_bar_height = if show_tab_bar {
            self.tab_bar_pixel_height()? as usize
        } else {
            0
        };

        let h_context = DimensionContext {
            dpi: self.dimensions.dpi as f32,
            pixel_max: self.dimensions.pixel_width as f32,
            pixel_cell: render_metrics.cell_size.width as f32,
        };
        let v_context = DimensionContext {
            dpi: self.dimensions.dpi as f32,
            pixel_max: self.dimensions.pixel_height as f32,
            pixel_cell: render_metrics.cell_size.height as f32,
        };
        let padding_left = config.window_padding.left.evaluate_as_pixels(h_context) as usize;
        let padding_top = config.window_padding.top.evaluate_as_pixels(v_context) as usize;
        let padding_bottom = config.window_padding.bottom.evaluate_as_pixels(v_context) as usize;

        let dimensions = Dimensions {
            pixel_width: ((terminal_size.cols as usize * render_metrics.cell_size.width as usize)
                + padding_left
                + effective_right_padding(&config, h_context)),
            pixel_height: ((terminal_size.rows as usize * render_metrics.cell_size.height as usize)
                + padding_top
                + padding_bottom) as usize
                + tab_bar_height,
            dpi: self.dimensions.dpi,
        };

        self.apply_scale_change(&dimensions, 1.0);
        self.apply_dimensions(
            &dimensions,
            Some(RowsAndCols {
                rows: size.rows as usize,
                cols: size.cols as usize,
            }),
            window,
        );
        Ok(())
    }

    pub fn reset_font_and_window_size(&mut self, window: &Window) -> anyhow::Result<()> {
        let size = self.config.initial_size(
            self.dimensions.dpi as u32,
            Some(crate::cell_pixel_dims(
                &self.config,
                self.dimensions.dpi as f64,
            )?),
        );
        self.set_window_size(size, window)
    }

    pub fn effective_right_padding(&self, config: &ConfigHandle) -> usize {
        effective_right_padding(
            config,
            DimensionContext {
                pixel_cell: self.render_metrics.cell_size.width as f32,
                dpi: self.dimensions.dpi as f32,
                pixel_max: self.dimensions.pixel_width as f32,
            },
        )
    }
}

/// Computes the effective padding for the RHS.
/// This is needed because the default is 0, but if the user has
/// enabled the scroll bar then they will expect it to have a reasonable
/// size unless they've specified differently.
pub fn effective_right_padding(config: &ConfigHandle, context: DimensionContext) -> usize {
    if config.enable_scroll_bar && config.window_padding.right.is_zero() {
        context.pixel_cell as usize
    } else {
        config.window_padding.right.evaluate_as_pixels(context) as usize
    }
}

#[cfg(test)]
mod frontend_geometry_tests {
    use super::{
        frontend_geometry_action, geometry_confirmation_settled,
        local_viewport_publish_is_worthwhile, remote_divider_can_pump,
        remote_divider_target_is_owed, visible_geometry_targets, FrontendGeometryAction,
        LocalTabShape, RejectedLocalViewport, FRONTEND_GEOMETRY_SETTLE,
    };
    use crate::termwindow::{FrontendGeometryPhase, RemoteDividerResizeStrategy};
    use std::collections::HashSet;
    use std::time::{Duration, Instant};
    use wezterm_term::TerminalSize;

    fn test_size(cols: usize, rows: usize) -> TerminalSize {
        TerminalSize {
            cols,
            rows,
            pixel_width: cols * 8,
            pixel_height: rows * 16,
            dpi: 96,
        }
    }

    fn test_shape(cols: usize, rows: usize, panes: &[usize]) -> LocalTabShape {
        LocalTabShape {
            size: test_size(cols, rows),
            panes: panes.to_vec(),
        }
    }

    fn test_viewport(cols: usize, rows: usize) -> mux::FrontendViewport {
        mux::FrontendViewport::CellGrid {
            size: test_size(cols, rows),
        }
    }

    #[test]
    fn an_identical_rejected_local_viewport_is_not_republished() {
        let viewport = test_viewport(120, 40);
        let shape = test_shape(120, 40, &[1, 2]);
        assert!(local_viewport_publish_is_worthwhile(
            None, &viewport, &shape
        ));

        let rejected = RejectedLocalViewport {
            viewport: viewport.clone(),
            shape: shape.clone(),
        };
        assert!(!local_viewport_publish_is_worthwhile(
            Some(&rejected),
            &viewport,
            &shape
        ));
    }

    #[test]
    fn a_changed_viewport_or_tab_shape_re_arms_the_local_publish() {
        let rejected = RejectedLocalViewport {
            viewport: test_viewport(120, 40),
            shape: test_shape(120, 40, &[1, 2]),
        };

        // A different viewport is worth publishing...
        assert!(local_viewport_publish_is_worthwhile(
            Some(&rejected),
            &test_viewport(100, 30),
            &test_shape(120, 40, &[1, 2]),
        ));
        // ...as is the same viewport against a resized tab...
        assert!(local_viewport_publish_is_worthwhile(
            Some(&rejected),
            &test_viewport(120, 40),
            &test_shape(90, 40, &[1, 2]),
        ));
        // ...or against a changed pane set.
        assert!(local_viewport_publish_is_worthwhile(
            Some(&rejected),
            &test_viewport(120, 40),
            &test_shape(120, 40, &[1, 2, 3]),
        ));
    }

    #[test]
    fn only_takeover_geometry_obscures_the_terminal() {
        assert!(!FrontendGeometryPhase::Previewing { epoch: 1 }.obscures_terminal());
        assert!(!FrontendGeometryPhase::Committing { epoch: 2 }.obscures_terminal());
        assert!(FrontendGeometryPhase::TakeoverSyncing { epoch: 3 }.obscures_terminal());
    }

    #[test]
    fn geometry_action_separates_owner_updates_takeover_and_passive_tabs() {
        assert_eq!(
            frontend_geometry_action(Some(true), false, false),
            FrontendGeometryAction::Set { takeover: false }
        );
        assert_eq!(
            frontend_geometry_action(None, false, false),
            FrontendGeometryAction::Set { takeover: true }
        );
        assert_eq!(
            frontend_geometry_action(Some(false), false, false),
            FrontendGeometryAction::Passive
        );
        assert_eq!(
            frontend_geometry_action(Some(false), true, true),
            FrontendGeometryAction::Claim
        );
    }

    #[test]
    fn takeover_geometry_must_remain_ready_for_a_stable_interval() {
        let start = Instant::now();
        let mut ready_since = None;

        assert!(!geometry_confirmation_settled(
            true,
            start,
            &mut ready_since
        ));
        assert!(!geometry_confirmation_settled(
            true,
            start + FRONTEND_GEOMETRY_SETTLE - Duration::from_millis(1),
            &mut ready_since
        ));
        assert!(geometry_confirmation_settled(
            true,
            start + FRONTEND_GEOMETRY_SETTLE,
            &mut ready_since
        ));

        assert!(!geometry_confirmation_settled(
            false,
            start + FRONTEND_GEOMETRY_SETTLE,
            &mut ready_since
        ));
        assert_eq!(ready_since, None);
        assert!(!geometry_confirmation_settled(
            true,
            start + FRONTEND_GEOMETRY_SETTLE,
            &mut ready_since
        ));
    }

    #[test]
    fn hidden_stack_targets_do_not_delay_the_takeover_mask() {
        let size = |cols| wezterm_term::TerminalSize {
            cols,
            rows: 24,
            pixel_width: cols * 10,
            pixel_height: 480,
            dpi: 96,
        };
        let adopted = vec![(10, size(80)), (11, size(120))];
        let visible = HashSet::from([10]);

        assert_eq!(
            visible_geometry_targets(&adopted, &visible),
            vec![(10, size(80))]
        );
    }

    #[test]
    fn remote_divider_stream_has_one_in_flight_and_release_gates_slow_mode() {
        assert!(remote_divider_can_pump(
            RemoteDividerResizeStrategy::Live,
            false,
            false,
            true
        ));
        assert!(!remote_divider_can_pump(
            RemoteDividerResizeStrategy::Live,
            false,
            true,
            true
        ));
        assert!(!remote_divider_can_pump(
            RemoteDividerResizeStrategy::OnRelease,
            false,
            false,
            true
        ));
        assert!(remote_divider_can_pump(
            RemoteDividerResizeStrategy::OnRelease,
            true,
            false,
            true
        ));
    }

    #[test]
    fn remote_divider_latest_target_replaces_only_unsent_work() {
        let viewport = |cols| codec::ClientViewport::CellGrid {
            size: wezterm_term::TerminalSize {
                rows: 24,
                cols,
                pixel_width: cols * 10,
                pixel_height: 480,
                dpi: 96,
            },
        };
        let first = viewport(80);
        let latest = viewport(120);
        assert!(!remote_divider_target_is_owed(Some(&first), None, &first));
        assert!(remote_divider_target_is_owed(Some(&first), None, &latest));
        assert!(!remote_divider_target_is_owed(None, Some(&latest), &latest));
    }
}
