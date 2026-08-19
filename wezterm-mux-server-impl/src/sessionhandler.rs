use crate::PKI;
use anyhow::{anyhow, Context};
use codec::*;
use config::keyassignment::SpawnTabDomain;
use config::TermConfig;
use mux::client::ClientId;
use mux::command_spec::CommandSpecExt;
use mux::domain::SplitSource;
use mux::pane::{CachePolicy, Pane, PaneId};
use mux::renderable::{RenderableDimensions, StableCursorPosition};
use mux::tab::TabId;
use mux::{
    ClientRegistrationId, FrontendAccessMode as MuxFrontendAccessMode,
    FrontendAccessState as MuxFrontendAccessState, FrontendPaneViewport, FrontendViewport,
    FrontendViewportState, Mux, PaletteSessionId,
};
use promise::spawn::spawn_into_main_thread;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use termwiz::surface::SequenceNo;
use url::Url;
use wezterm_term::color::ColorPalette;
use wezterm_term::terminal::Alert;
use wezterm_term::StableRowIndex;

lazy_static::lazy_static! {
    /// Serializes the authoritative tree decision with the live-workspace
    /// check and spawn. Without this, two clients opening the same empty
    /// Thread concurrently can each create its first tab.
    static ref THINKTERM_MATERIALIZE: futures::lock::Mutex<()> =
        futures::lock::Mutex::new(());
}

#[derive(Clone)]
pub struct PduSender {
    func: Arc<dyn Fn(DecodedPdu) -> anyhow::Result<()> + Send + Sync>,
}

impl PduSender {
    pub fn send(&self, pdu: DecodedPdu) -> anyhow::Result<()> {
        (self.func)(pdu)
    }

    pub fn new<T>(f: T) -> Self
    where
        T: Fn(DecodedPdu) -> anyhow::Result<()> + Send + Sync + 'static,
    {
        Self { func: Arc::new(f) }
    }
}

#[derive(Default, Debug)]
pub(crate) struct PerPane {
    cursor_position: StableCursorPosition,
    title: String,
    working_dir: Option<Url>,
    dimensions: RenderableDimensions,
    mouse_grabbed: bool,
    alt_screen: bool,
    /// Outer None means that this connection has never received application
    /// palette state for the pane. Inner None is an explicit reset to the
    /// client's own configured palette.
    last_sent_application_palette: Option<Option<ColorPalette>>,
    seqno: SequenceNo,
    pub(crate) notifications: Vec<Alert>,
}

impl PerPane {
    fn needs_application_palette(&self, palette: &Option<ColorPalette>) -> bool {
        self.last_sent_application_palette.as_ref() != Some(palette)
    }

    fn record_application_palette(&mut self, palette: Option<ColorPalette>) {
        self.last_sent_application_palette = Some(palette);
    }

    fn compute_changes(
        &mut self,
        pane: &Arc<dyn Pane>,
        force_with_input_serial: Option<InputSerial>,
    ) -> Option<GetPaneRenderChangesResponse> {
        let mut changed = false;
        let mouse_grabbed = pane.is_mouse_grabbed();
        if mouse_grabbed != self.mouse_grabbed {
            changed = true;
        }

        let alt_screen = pane.is_alt_screen_active();
        if alt_screen != self.alt_screen {
            changed = true;
        }

        let dims = pane.get_dimensions();
        if dims != self.dimensions {
            changed = true;
        }

        let cursor_position = pane.get_cursor_position();
        if cursor_position != self.cursor_position {
            changed = true;
        }

        let title = pane.get_title();
        if title != self.title {
            changed = true;
        }

        let working_dir = pane.get_current_working_dir(CachePolicy::AllowStale);
        if working_dir != self.working_dir {
            changed = true;
        }

        let old_seqno = self.seqno;
        self.seqno = pane.get_current_seqno();
        let mut all_dirty_lines = pane.get_changed_since(
            0..dims.physical_top + dims.viewport_rows as StableRowIndex,
            old_seqno,
        );
        if !all_dirty_lines.is_empty() {
            changed = true;
        }

        if !changed && !force_with_input_serial.is_some() {
            return None;
        }

        // Figure out what we're going to send as dirty lines vs bonus lines
        let viewport_range =
            dims.physical_top..dims.physical_top + dims.viewport_rows as StableRowIndex;

        let (first_line, lines) = pane.get_lines(viewport_range);
        let mut bonus_lines = lines
            .into_iter()
            .enumerate()
            .filter_map(|(idx, mut line)| {
                let stable_row = first_line + idx as StableRowIndex;
                if all_dirty_lines.contains(stable_row) {
                    all_dirty_lines.remove(stable_row);
                    line.compress_for_scrollback();
                    Some((stable_row, line))
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();

        // Always send the cursor's row, as that tends to the busiest and we don't
        // have a sequencing concept for our idea of the remote state.
        let (cursor_line_idx, mut lines) = pane.get_lines(cursor_position.y..cursor_position.y + 1);
        let mut cursor_line = lines.remove(0);
        cursor_line.compress_for_scrollback();
        bonus_lines.push((cursor_line_idx, cursor_line));

        self.cursor_position = cursor_position;
        self.title = title.clone();
        self.working_dir = working_dir.clone();
        self.dimensions = dims;
        self.mouse_grabbed = mouse_grabbed;
        self.alt_screen = alt_screen;

        let bonus_lines = bonus_lines.into();
        Some(GetPaneRenderChangesResponse {
            pane_id: pane.pane_id(),
            mouse_grabbed,
            alt_screen,
            dirty_lines: all_dirty_lines.iter().cloned().collect(),
            dimensions: dims,
            cursor_position,
            title,
            bonus_lines,
            working_dir: working_dir.map(Into::into),
            input_serial: force_with_input_serial,
            seqno: self.seqno,
        })
    }
}

fn maybe_push_pane_changes(
    pane: &Arc<dyn Pane>,
    sender: PduSender,
    per_pane: Arc<Mutex<PerPane>>,
) -> anyhow::Result<()> {
    let mut per_pane = per_pane.lock().unwrap();

    // Application palette provenance must arrive even when the state is
    // `None`: that explicit reset prevents a server's configured/advisory
    // palette from taking over client rendering. Send it before line changes
    // so a real application override is installed before those lines paint.
    let application_palette = pane.palette_override();
    if per_pane.needs_application_palette(&application_palette) {
        sender.send(DecodedPdu {
            pdu: Pdu::SetApplicationPalette(SetApplicationPalette {
                pane_id: pane.pane_id(),
                palette: application_palette.clone(),
            }),
            serial: 0,
        })?;
        per_pane.record_application_palette(application_palette);
    }

    if let Some(resp) = per_pane.compute_changes(pane, None) {
        sender.send(DecodedPdu {
            pdu: Pdu::GetPaneRenderChangesResponse(resp),
            serial: 0,
        })?;
    }

    let notifications: Vec<Alert> = per_pane.notifications.drain(..).collect();
    for alert in notifications {
        match alert {
            // The current application state was sent (and de-duplicated)
            // above. Never forward this alert as a configured palette.
            Alert::PaletteChanged => {}
            alert => {
                sender.send(DecodedPdu {
                    pdu: Pdu::NotifyAlert(NotifyAlert {
                        pane_id: pane.pane_id(),
                        alert,
                    }),
                    serial: 0,
                })?;
            }
        }
    }
    Ok(())
}

fn apply_client_palette(pane: &Arc<dyn Pane>, palette: Option<ColorPalette>) -> anyhow::Result<()> {
    match pane.get_config() {
        Some(config) => match config.downcast_ref::<TermConfig>() {
            Some(tc) => match palette {
                Some(palette) => tc.set_client_palette(palette),
                None => tc.clear_client_palette(),
            },
            None => {
                log::error!(
                    "pane {} doesn't have TermConfig as its config; ignoring client palette update",
                    pane.pane_id()
                );
            }
        },
        None => {
            if let Some(palette) = palette {
                let config = TermConfig::new();
                config.set_client_palette(palette);
                pane.set_config(Arc::new(config));
            }
        }
    }
    Ok(())
}

pub(crate) fn codec_viewport_state(state: FrontendViewportState) -> ClientViewportState {
    ClientViewportState {
        tab_id: state.tab_id,
        owner: state.owner,
        canonical_size: state.canonical_size,
        view: state.view.map(|view| codec::ClientView {
            scroll: view
                .scroll
                .into_iter()
                .map(|(pane_id, lines_from_bottom)| codec::ClientPaneScroll {
                    pane_id,
                    lines_from_bottom,
                })
                .collect(),
        }),
        generation: state.generation,
        access: codec_access_state(state.access),
    }
}

pub(crate) fn codec_access_state(state: MuxFrontendAccessState) -> FrontendAccessState {
    FrontendAccessState {
        mode: codec_access_mode(state.mode),
        owner: state.owner,
        generation: state.generation,
    }
}

fn codec_access_mode(mode: MuxFrontendAccessMode) -> FrontendAccessMode {
    match mode {
        MuxFrontendAccessMode::TmuxLatest => FrontendAccessMode::TmuxLatest,
        MuxFrontendAccessMode::Handoff => FrontendAccessMode::Handoff,
    }
}

fn mux_access_mode(mode: FrontendAccessMode) -> MuxFrontendAccessMode {
    match mode {
        FrontendAccessMode::TmuxLatest => MuxFrontendAccessMode::TmuxLatest,
        FrontendAccessMode::Handoff => MuxFrontendAccessMode::Handoff,
    }
}

fn mux_view(view: codec::ClientView) -> mux::FrontendView {
    mux::FrontendView {
        scroll: view
            .scroll
            .into_iter()
            .map(|entry| (entry.pane_id, entry.lines_from_bottom))
            .collect(),
    }
}

fn mux_viewport(viewport: ClientViewport) -> FrontendViewport {
    match viewport {
        ClientViewport::CellGrid { size } => FrontendViewport::CellGrid { size },
        ClientViewport::Native { size, panes } => FrontendViewport::Native {
            size,
            panes: panes
                .into_iter()
                .map(|pane| FrontendPaneViewport {
                    pane_id: pane.pane_id,
                    size: pane.size,
                    frame: pane.frame,
                })
                .collect(),
        },
    }
}

fn claim_viewport_for_pane(
    mux: &Mux,
    client_id: Option<&Arc<ClientId>>,
    registration: Option<ClientRegistrationId>,
    pane_id: PaneId,
) -> anyhow::Result<TabId> {
    let client_id =
        client_id.ok_or_else(|| anyhow!("terminal input requires an identified client"))?;
    let registration = registration
        .ok_or_else(|| anyhow!("terminal input requires a live client registration"))?;
    if mux
        .registered_client_has_frontend_access(client_id, registration)
        .is_none()
    {
        anyhow::bail!("terminal input came from a superseded client connection");
    }
    let (_domain_id, _window_id, tab_id) = mux
        .resolve_pane_id(pane_id)
        .ok_or_else(|| anyhow!("no such pane {pane_id}"))?;
    if !mux.registered_client_had_tab_input(client_id, registration, tab_id) {
        match mux.registered_client_has_frontend_access(client_id, registration) {
            None => anyhow::bail!("client connection was superseded"),
            Some(false) => anyhow::bail!("terminal is being operated on another device"),
            Some(true) => anyhow::bail!("unable to acquire the frontend layout"),
        }
    }
    Ok(tab_id)
}

fn require_registered_frontend_access(
    mux: &Mux,
    client_id: Option<&Arc<ClientId>>,
    registration: Option<ClientRegistrationId>,
) -> anyhow::Result<()> {
    let client_id = client_id.ok_or_else(|| anyhow!("request requires an identified client"))?;
    let registration =
        registration.ok_or_else(|| anyhow!("request requires a live client registration"))?;
    match mux.registered_client_has_frontend_access(client_id, registration) {
        None => anyhow::bail!("client connection was superseded"),
        Some(false) => anyhow::bail!("terminal is being operated on another device"),
        Some(true) => Ok(()),
    }
}

/// Mutations which can be reached through tab/menu chrome must still obey B's
/// global owner, but checking them must not itself claim A's layout lease.
fn requires_existing_frontend_access(pdu: &Pdu) -> bool {
    matches!(
        pdu,
        Pdu::EraseScrollbackRequest(_)
            | Pdu::KillPane(_)
            | Pdu::SetPaneZoomed(_)
            | Pdu::SpawnV2(_)
            | Pdu::SplitPane(_)
            | Pdu::SpawnPaneInStack(_)
            | Pdu::ActivatePaneInStack(_)
            | Pdu::MovePaneToStack(_)
            | Pdu::MovePaneToNewTab(_)
            | Pdu::AdjustPaneSize(_)
    )
}

fn activate_client_palette(
    mux: &Mux,
    pane: &Arc<dyn Pane>,
    palette_session_id: Option<PaletteSessionId>,
) -> anyhow::Result<()> {
    if let Some(palette_session_id) = palette_session_id {
        if let Some(palette) = mux.activate_client_palette(palette_session_id, pane.pane_id()) {
            apply_client_palette(pane, Some(palette))?;
        }
    }
    Ok(())
}

fn schedule_palette_session_cleanup(session_id: PaletteSessionId, reason: &'static str) {
    let mux = Mux::get();
    if !mux.deactivate_palette_session(session_id) {
        return;
    }
    // Deactivation above closes the stale-handler gate immediately. Applying
    // fallback here, behind work already queued by this connection, closes the
    // TOCTOU where a handler had fetched its palette just before disconnect
    // and would otherwise apply it after synchronous cleanup.
    spawn_into_main_thread(async move {
        let mux = Mux::get();
        for change in mux.unregister_palette_session(session_id) {
            if let Some(pane) = mux.get_pane(change.pane_id) {
                if let Err(err) = apply_client_palette(&pane, change.palette) {
                    log::error!(
                        "applying palette fallback for pane {} after {reason}: {err:#}",
                        change.pane_id
                    );
                }
            }
        }
    })
    .detach();
}

pub struct SessionHandler {
    to_write_tx: PduSender,
    per_pane: HashMap<TabId, Arc<Mutex<PerPane>>>,
    client_id: Option<Arc<ClientId>>,
    client_registration: Option<ClientRegistrationId>,
    palette_session_id: Option<PaletteSessionId>,
    proxy_client_id: Option<ClientId>,
}

impl Drop for SessionHandler {
    fn drop(&mut self) {
        if let Some(session_id) = self.palette_session_id.take() {
            schedule_palette_session_cleanup(session_id, "client disconnect");
        }
        if let (Some(client_id), Some(registration)) =
            (self.client_id.take(), self.client_registration.take())
        {
            Mux::get().unregister_client(&client_id, registration);
        }
    }
}

impl SessionHandler {
    pub fn new(to_write_tx: PduSender) -> Self {
        Self {
            to_write_tx,
            per_pane: HashMap::new(),
            client_id: None,
            client_registration: None,
            palette_session_id: None,
            proxy_client_id: None,
        }
    }

    pub(crate) fn per_pane(&mut self, pane_id: PaneId) -> Arc<Mutex<PerPane>> {
        Arc::clone(
            self.per_pane
                .entry(pane_id)
                .or_insert_with(|| Arc::new(Mutex::new(PerPane::default()))),
        )
    }

    pub fn schedule_pane_push(&mut self, pane_id: PaneId) {
        let sender = self.to_write_tx.clone();
        let per_pane = self.per_pane(pane_id);
        spawn_into_main_thread(async move {
            let mux = Mux::get();
            let pane = mux
                .get_pane(pane_id)
                .ok_or_else(|| anyhow!("no such pane {}", pane_id))?;
            maybe_push_pane_changes(&pane, sender, per_pane)?;
            Ok::<(), anyhow::Error>(())
        })
        .detach();
    }

    pub fn process_one(&mut self, decoded: DecodedPdu) {
        let start = Instant::now();
        let sender = self.to_write_tx.clone();
        let serial = decoded.serial;
        let palette_session_id = self.palette_session_id;
        log::trace!("recv {} {}", serial, decoded.pdu.pdu_name());

        if let (Some(client_id), Some(registration)) = (&self.client_id, self.client_registration) {
            if decoded.pdu.is_user_input() {
                let mux = Mux::get();
                mux.registered_client_had_input(client_id, registration);
            }
        }

        let send_response = move |result: anyhow::Result<Pdu>| {
            let pdu = match result {
                Ok(pdu) => pdu,
                Err(err) => Pdu::ErrorResponse(ErrorResponse {
                    reason: format!("Error: {err:#}"),
                }),
            };
            log::trace!("{} processing time {:?}", serial, start.elapsed());
            sender.send(DecodedPdu { pdu, serial }).ok();
        };

        if requires_existing_frontend_access(&decoded.pdu) {
            if let Err(err) = require_registered_frontend_access(
                &Mux::get(),
                self.client_id.as_ref(),
                self.client_registration,
            ) {
                send_response(Err(err));
                return;
            }
        }

        fn catch<F, SND>(f: F, send_response: SND)
        where
            F: FnOnce() -> anyhow::Result<Pdu>,
            SND: Fn(anyhow::Result<Pdu>),
        {
            send_response(f());
        }

        match decoded.pdu {
            Pdu::Ping(Ping {}) => send_response(Ok(Pdu::Pong(Pong {}))),
            Pdu::SetWindowWorkspace(SetWindowWorkspace {
                window_id,
                workspace,
            }) => {
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            let mut window = mux
                                .get_window_mut(window_id)
                                .ok_or_else(|| anyhow!("window {} is invalid", window_id))?;
                            window.set_workspace(&workspace);
                            Ok(Pdu::UnitResponse(UnitResponse {}))
                        },
                        send_response,
                    )
                })
                .detach();
            }
            Pdu::SetClientId(SetClientId {
                mut client_id,
                is_proxy,
            }) => {
                if is_proxy {
                    if self.proxy_client_id.is_none() {
                        // Copy proxy identity, but don't assign it to the mux;
                        // we'll use it to annotate the actual clients own
                        // identity when they send it
                        self.proxy_client_id.replace(client_id);
                    }
                } else {
                    // If this session is a proxy, override the incoming id with
                    // the proxy information so that it is clear what is going
                    // on from the `thinkterm cli list-clients` information
                    if let Some(proxy_id) = &self.proxy_client_id {
                        client_id.ssh_auth_sock = proxy_id.ssh_auth_sock.clone();
                        // Note that this `via proxy pid` string is coupled
                        // with the logic in mux/src/ssh_agent
                        client_id.hostname =
                            format!("{} (via proxy pid {})", client_id.hostname, proxy_id.pid);
                    }

                    let client_id = Arc::new(client_id);
                    if let Some(old_session_id) = self.palette_session_id.take() {
                        schedule_palette_session_cleanup(old_session_id, "client identity change");
                    }
                    self.palette_session_id =
                        Some(Mux::get().register_palette_session(client_id.as_ref()));
                    if let (Some(old_client), Some(old_registration)) =
                        (self.client_id.take(), self.client_registration.take())
                    {
                        Mux::get().unregister_client(&old_client, old_registration);
                    }
                    let registration = Mux::get().register_client(client_id.clone());
                    self.client_id.replace(client_id);
                    self.client_registration.replace(registration);
                }
                send_response(Ok(Pdu::UnitResponse(UnitResponse {})))
            }
            Pdu::SetFocusedPane(SetFocusedPane {
                pane_id,
                configured_palette,
            }) => {
                let client_id = self.client_id.clone();
                let registration = self.client_registration;
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            let _identity = mux.with_identity(client_id.clone());

                            let pane = mux
                                .get_pane(pane_id)
                                .ok_or_else(|| anyhow::anyhow!("pane {pane_id} not found"))?;
                            require_registered_frontend_access(
                                &mux,
                                client_id.as_ref(),
                                registration,
                            )?;

                            if let Some(palette) = configured_palette {
                                if let Some(palette) = mux.advise_client_palette(
                                    palette_session_id.ok_or_else(|| {
                                        anyhow!(
                                            "palette-bearing focus requires a live client session"
                                        )
                                    })?,
                                    pane_id,
                                    palette,
                                ) {
                                    apply_client_palette(&pane, Some(palette))?;
                                }
                            }
                            // Switch the OSC query base before focus reporting
                            // can provoke an immediate query from the app.
                            activate_client_palette(&mux, &pane, palette_session_id)?;

                            let (_domain_id, window_id, tab_id) = mux
                                .resolve_pane_id(pane_id)
                                .ok_or_else(|| anyhow::anyhow!("pane {pane_id} not found"))?;
                            {
                                let mut window =
                                    mux.get_window_mut(window_id).ok_or_else(|| {
                                        anyhow::anyhow!("window {window_id} not found")
                                    })?;
                                let tab_idx = window.idx_by_id(tab_id).ok_or_else(|| {
                                    anyhow::anyhow!(
                                        "tab {tab_id} isn't really in window {window_id}!?"
                                    )
                                })?;
                                window.save_and_then_set_active(tab_idx);
                            }
                            let tab = mux
                                .get_tab(tab_id)
                                .ok_or_else(|| anyhow::anyhow!("tab {tab_id} not found"))?;
                            tab.set_active_pane(&pane);

                            mux.record_focus_for_current_identity(pane_id);
                            mux.notify(mux::MuxNotification::PaneFocused(pane_id));

                            Ok(Pdu::UnitResponse(UnitResponse {}))
                        },
                        send_response,
                    )
                })
                .detach();
            }
            Pdu::GetClientList(GetClientList) => {
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            let clients = mux.iter_clients();
                            Ok(Pdu::GetClientListResponse(GetClientListResponse {
                                clients,
                            }))
                        },
                        send_response,
                    )
                })
                .detach();
            }
            Pdu::ListPanes(ListPanes {}) => {
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            let mut tabs = vec![];
                            let mut tab_titles = vec![];
                            let mut window_titles = HashMap::new();
                            for window_id in mux.iter_windows().into_iter() {
                                let window = mux.get_window(window_id).unwrap();
                                window_titles.insert(window_id, window.get_title().to_string());
                                for tab in window.iter() {
                                    tabs.push(tab.codec_pane_tree());
                                    tab_titles.push(tab.get_title());
                                }
                            }
                            log::trace!("ListPanes {tabs:#?} {tab_titles:?}");
                            Ok(Pdu::ListPanesResponse(ListPanesResponse {
                                tabs,
                                tab_titles,
                                window_titles,
                            }))
                        },
                        send_response,
                    )
                })
                .detach();
            }

            Pdu::RenameWorkspace(RenameWorkspace {
                old_workspace,
                new_workspace,
            }) => {
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            mux.rename_workspace(&old_workspace, &new_workspace);
                            Ok(Pdu::UnitResponse(UnitResponse {}))
                        },
                        send_response,
                    );
                })
                .detach();
            }

            Pdu::WriteToPane(WriteToPane { pane_id, data }) => {
                let sender = self.to_write_tx.clone();
                let per_pane = self.per_pane(pane_id);
                let client_id = self.client_id.clone();
                let registration = self.client_registration;
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            let _identity = mux.with_identity(client_id.clone());
                            let pane = mux
                                .get_pane(pane_id)
                                .ok_or_else(|| anyhow!("no such pane {}", pane_id))?;
                            claim_viewport_for_pane(
                                &mux,
                                client_id.as_ref(),
                                registration,
                                pane_id,
                            )?;
                            activate_client_palette(&mux, &pane, palette_session_id)?;
                            pane.writer().write_all(&data)?;
                            maybe_push_pane_changes(&pane, sender, per_pane)?;
                            Ok(Pdu::UnitResponse(UnitResponse {}))
                        },
                        send_response,
                    );
                })
                .detach();
            }
            Pdu::EraseScrollbackRequest(EraseScrollbackRequest {
                pane_id,
                erase_mode,
            }) => {
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            let pane = mux
                                .get_pane(pane_id)
                                .ok_or_else(|| anyhow!("no such pane {}", pane_id))?;
                            pane.erase_scrollback(erase_mode);
                            Ok(Pdu::UnitResponse(UnitResponse {}))
                        },
                        send_response,
                    );
                })
                .detach();
            }
            Pdu::KillPane(KillPane { pane_id }) => {
                let sender = self.to_write_tx.clone();
                let per_pane = self.per_pane(pane_id);
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            let pane = mux
                                .get_pane(pane_id)
                                .ok_or_else(|| anyhow!("no such pane {}", pane_id))?;
                            pane.kill();
                            mux.remove_pane(pane_id);
                            maybe_push_pane_changes(&pane, sender, per_pane)?;
                            Ok(Pdu::UnitResponse(UnitResponse {}))
                        },
                        send_response,
                    );
                })
                .detach();
            }
            Pdu::SendPaste(SendPaste { pane_id, data }) => {
                let sender = self.to_write_tx.clone();
                let per_pane = self.per_pane(pane_id);
                let client_id = self.client_id.clone();
                let registration = self.client_registration;
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            let _identity = mux.with_identity(client_id.clone());
                            let pane = mux
                                .get_pane(pane_id)
                                .ok_or_else(|| anyhow!("no such pane {}", pane_id))?;
                            claim_viewport_for_pane(
                                &mux,
                                client_id.as_ref(),
                                registration,
                                pane_id,
                            )?;
                            activate_client_palette(&mux, &pane, palette_session_id)?;
                            pane.send_paste(&data)?;
                            maybe_push_pane_changes(&pane, sender, per_pane)?;
                            Ok(Pdu::UnitResponse(UnitResponse {}))
                        },
                        send_response,
                    )
                })
                .detach();
            }

            Pdu::SearchScrollbackRequest(SearchScrollbackRequest {
                pane_id,
                pattern,
                range,
                limit,
            }) => {
                use mux::pane::Pattern;

                async fn do_search(
                    pane_id: TabId,
                    pattern: Pattern,
                    range: std::ops::Range<StableRowIndex>,
                    limit: Option<u32>,
                ) -> anyhow::Result<Pdu> {
                    let mux = Mux::get();
                    let pane = mux
                        .get_pane(pane_id)
                        .ok_or_else(|| anyhow!("no such pane {}", pane_id))?;

                    pane.search(pattern, range, limit).await.map(|results| {
                        Pdu::SearchScrollbackResponse(SearchScrollbackResponse { results })
                    })
                }

                spawn_into_main_thread(async move {
                    promise::spawn::spawn(async move {
                        let result = do_search(pane_id, pattern, range, limit).await;
                        send_response(result);
                    })
                    .detach();
                })
                .detach();
            }

            Pdu::SetPaneZoomed(SetPaneZoomed {
                containing_tab_id,
                pane_id,
                zoomed,
            }) => {
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            let pane = mux
                                .get_pane(pane_id)
                                .ok_or_else(|| anyhow!("no such pane {}", pane_id))?;
                            let tab = mux
                                .get_tab(containing_tab_id)
                                .ok_or_else(|| anyhow!("no such tab {}", containing_tab_id))?;
                            activate_client_palette(&mux, &pane, palette_session_id)?;
                            mux::zoom_trace!(
                                "srv.zoom.recv tab={containing_tab_id} pane={pane_id} \
                                 want_zoomed={zoomed} | {}",
                                tab.geometry_trace()
                            );
                            match tab.get_zoomed_pane() {
                                Some(p) => {
                                    let is_zoomed = p.pane_id() == pane_id;
                                    if is_zoomed != zoomed {
                                        tab.set_zoomed(false);
                                        if zoomed {
                                            tab.set_active_pane(&pane);
                                            tab.set_zoomed(zoomed);
                                        }
                                    }
                                }
                                None => {
                                    if zoomed {
                                        tab.set_active_pane(&pane);
                                        tab.set_zoomed(zoomed);
                                    }
                                }
                            }
                            mux::zoom_trace!(
                                "srv.zoom.done tab={containing_tab_id} pane={pane_id} \
                                 want_zoomed={zoomed} | {}",
                                tab.geometry_trace()
                            );
                            Ok(Pdu::UnitResponse(UnitResponse {}))
                        },
                        send_response,
                    )
                })
                .detach();
            }

            Pdu::GetPaneDirection(GetPaneDirection { pane_id, direction }) => {
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            let (_domain_id, _window_id, tab_id) = mux
                                .resolve_pane_id(pane_id)
                                .ok_or_else(|| anyhow!("no such pane {}", pane_id))?;
                            let tab = mux
                                .get_tab(tab_id)
                                .ok_or_else(|| anyhow!("no such tab {}", tab_id))?;
                            let panes = tab.iter_panes_ignoring_zoom();
                            let pane_id = tab
                                .get_pane_direction(direction, true)
                                .map(|pane_index| panes[pane_index].pane.pane_id());

                            Ok(Pdu::GetPaneDirectionResponse(GetPaneDirectionResponse {
                                pane_id,
                            }))
                        },
                        send_response,
                    )
                })
                .detach();
            }

            Pdu::ActivatePaneDirection(ActivatePaneDirection { pane_id, direction }) => {
                let client_id = self.client_id.clone();
                let registration = self.client_registration;
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            claim_viewport_for_pane(
                                &mux,
                                client_id.as_ref(),
                                registration,
                                pane_id,
                            )?;
                            let (_domain_id, _window_id, tab_id) = mux
                                .resolve_pane_id(pane_id)
                                .ok_or_else(|| anyhow!("no such pane {}", pane_id))?;
                            let tab = mux
                                .get_tab(tab_id)
                                .ok_or_else(|| anyhow!("no such tab {}", tab_id))?;
                            // A direction request is a no-op while zoomed when
                            // the configured policy forbids unzooming. Do not
                            // transfer ownership to a pane that won't receive
                            // focus in that case.
                            if tab.get_zoomed_pane().is_none()
                                || config::configuration().unzoom_on_switch_pane
                            {
                                let panes = tab.iter_panes_ignoring_zoom();
                                if let Some(pane_index) = tab.get_pane_direction(direction, true) {
                                    let target = &panes[pane_index].pane;
                                    activate_client_palette(&mux, target, palette_session_id)?;
                                }
                            }
                            tab.activate_pane_direction(direction);
                            Ok(Pdu::UnitResponse(UnitResponse {}))
                        },
                        send_response,
                    )
                })
                .detach();
            }

            Pdu::Resize(Resize {
                containing_tab_id,
                pane_id,
                size,
            }) => {
                let client_id = self.client_id.clone();
                let registration = self.client_registration;
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            if let (Some(client_id), Some(registration)) =
                                (client_id.as_ref(), registration)
                            {
                                match mux.registered_client_may_resize_tab(
                                    client_id,
                                    registration,
                                    containing_tab_id,
                                ) {
                                    None => anyhow::bail!("client connection was superseded"),
                                    Some(false) => {
                                        return Ok(Pdu::UnitResponse(UnitResponse {}));
                                    }
                                    Some(true) => {}
                                }
                            }
                            let pane = mux
                                .get_pane(pane_id)
                                .ok_or_else(|| anyhow!("no such pane {}", pane_id))?;
                            let tab = mux
                                .get_tab(containing_tab_id)
                                .ok_or_else(|| anyhow!("no such tab {}", containing_tab_id))?;
                            if !tab.contains_pane(pane_id) {
                                anyhow::bail!("pane {pane_id} is not in tab {containing_tab_id}");
                            }
                            pane.resize(size)?;
                            // A legacy single-pane Resize carries no pane
                            // frame. It may update the PTY surface, but it
                            // cannot safely redefine split geometry: font
                            // scaling and frontend chrome make the surface
                            // smaller than its containing rectangle. Complete
                            // Native viewports carry exact frames and are the
                            // sole source for divider reconstruction.
                            Ok(Pdu::UnitResponse(UnitResponse {}))
                        },
                        send_response,
                    )
                })
                .detach();
            }

            Pdu::SendKeyDown(SendKeyDown {
                pane_id,
                event,
                input_serial,
            }) => {
                let sender = self.to_write_tx.clone();
                let per_pane = self.per_pane(pane_id);
                let client_id = self.client_id.clone();
                let registration = self.client_registration;
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            let _identity = mux.with_identity(client_id.clone());
                            let pane = mux
                                .get_pane(pane_id)
                                .ok_or_else(|| anyhow!("no such pane {}", pane_id))?;
                            claim_viewport_for_pane(
                                &mux,
                                client_id.as_ref(),
                                registration,
                                pane_id,
                            )?;
                            activate_client_palette(&mux, &pane, palette_session_id)?;
                            pane.key_down(event.key, event.modifiers)?;

                            // For a key press, we want to always send back the
                            // cursor position so that the predictive echo doesn't
                            // leave the cursor in the wrong place
                            let mut per_pane = per_pane.lock().unwrap();
                            if let Some(resp) = per_pane.compute_changes(&pane, Some(input_serial))
                            {
                                sender.send(DecodedPdu {
                                    pdu: Pdu::GetPaneRenderChangesResponse(resp),
                                    serial: 0,
                                })?;
                            }
                            Ok(Pdu::UnitResponse(UnitResponse {}))
                        },
                        send_response,
                    )
                })
                .detach();
            }
            Pdu::SendMouseEvent(SendMouseEvent { pane_id, event }) => {
                let sender = self.to_write_tx.clone();
                let per_pane = self.per_pane(pane_id);
                let client_id = self.client_id.clone();
                let registration = self.client_registration;
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            let _identity = mux.with_identity(client_id.clone());
                            let pane = mux
                                .get_pane(pane_id)
                                .ok_or_else(|| anyhow!("no such pane {}", pane_id))?;
                            claim_viewport_for_pane(
                                &mux,
                                client_id.as_ref(),
                                registration,
                                pane_id,
                            )?;
                            activate_client_palette(&mux, &pane, palette_session_id)?;
                            // The client coalesces rapid wheel motion into a single
                            // event with an accumulated amount, but the terminal
                            // emits one report per event regardless of the amount;
                            // replay it per notch so mouse-mode apps scroll the
                            // full distance.
                            use wezterm_term::MouseButton as MB;
                            let notches = match event.button {
                                MB::WheelUp(n) if n > 1 => Some((MB::WheelUp(1), n)),
                                MB::WheelDown(n) if n > 1 => Some((MB::WheelDown(1), n)),
                                MB::WheelLeft(n) if n > 1 => Some((MB::WheelLeft(1), n)),
                                MB::WheelRight(n) if n > 1 => Some((MB::WheelRight(1), n)),
                                _ => None,
                            };
                            match notches {
                                Some((notch, n)) => {
                                    let mut single = event;
                                    single.button = notch;
                                    for _ in 0..n {
                                        pane.mouse_event(single.clone())?;
                                    }
                                }
                                None => pane.mouse_event(event)?,
                            }
                            maybe_push_pane_changes(&pane, sender, per_pane)?;
                            Ok(Pdu::UnitResponse(UnitResponse {}))
                        },
                        send_response,
                    )
                })
                .detach();
            }

            Pdu::SpawnV2(spawn) => {
                spawn_into_main_thread(async move {
                    schedule_domain_spawn_v2(spawn, send_response);
                })
                .detach();
            }

            Pdu::SplitPane(split) => {
                spawn_into_main_thread(async move {
                    schedule_split_pane(split, send_response);
                })
                .detach();
            }

            Pdu::SpawnPaneInStack(request) => {
                spawn_into_main_thread(async move {
                    schedule_spawn_pane_in_stack(request, send_response);
                })
                .detach();
            }

            Pdu::ActivatePaneInStack(request) => {
                let client_id = self.client_id.clone();
                let registration = self.client_registration;
                spawn_into_main_thread(async move {
                    let mux = Mux::get();
                    let _identity = mux.with_identity(client_id.clone());
                    let result = (|| {
                        let pane = mux
                            .get_pane(request.pane_id)
                            .ok_or_else(|| anyhow!("no such pane {}", request.pane_id))?;
                        claim_viewport_for_pane(
                            &mux,
                            client_id.as_ref(),
                            registration,
                            request.pane_id,
                        )?;
                        let (_domain_id, _window_id, tab_id) = mux
                            .resolve_pane_id(request.pane_id)
                            .ok_or_else(|| anyhow!("no such pane {}", request.pane_id))?;
                        let tab = mux
                            .get_tab(tab_id)
                            .ok_or_else(|| anyhow!("no such tab {}", tab_id))?;
                        if let Some(zoomed) = tab.get_zoomed_pane() {
                            let zoomed_stack = tab.pane_stack_id(zoomed.pane_id());
                            let target_stack = tab.pane_stack_id(request.pane_id);
                            if zoomed_stack.is_none() || zoomed_stack != target_stack {
                                anyhow::bail!("cannot switch pane tab while zoomed");
                            }
                        }
                        activate_client_palette(&mux, &pane, palette_session_id)?;
                        mux.activate_pane_in_stack(request.pane_id)
                    })();
                    send_response(result.map(|()| Pdu::UnitResponse(UnitResponse {})));
                })
                .detach();
            }

            Pdu::MovePaneToStack(request) => {
                spawn_into_main_thread(async move {
                    schedule_move_pane_to_stack(request, send_response);
                })
                .detach();
            }

            Pdu::GetThinkTermTree(_) => {
                send_response(Ok(Pdu::ThinkTermTreeState(ThinkTermTreeState {
                    tree: crate::thinkterm_tree::snapshot(),
                })));
            }

            Pdu::GetThinkTermSessionState(_) => {
                send_response(crate::thinkterm_session::snapshot().map(Pdu::ThinkTermSessionState));
            }

            Pdu::EnsureThinkTermThread(request) => {
                spawn_into_main_thread(async move {
                    schedule_ensure_thinkterm_thread(request, send_response);
                })
                .detach();
            }

            Pdu::SetClientViewport(SetClientViewport { tab_id, viewport }) => {
                let client_id = self.client_id.clone();
                let registration = self.client_registration;
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let client_id = client_id.ok_or_else(|| {
                                anyhow!("SetClientViewport requires an identified client")
                            })?;
                            let registration = registration.ok_or_else(|| {
                                anyhow!("SetClientViewport requires a live client registration")
                            })?;
                            let mux = Mux::get();
                            let state = mux
                                .set_registered_client_viewport(
                                    &client_id,
                                    registration,
                                    tab_id,
                                    mux_viewport(viewport),
                                )?
                                .ok_or_else(|| anyhow!("client connection was superseded"))?;
                            Ok(Pdu::ClientViewportState(codec_viewport_state(state)))
                        },
                        send_response,
                    )
                })
                .detach();
            }

            // Offering a view is not a claim: a renderer that is not driving is
            // ignored, so this can be sent freely without stealing the lease.
            Pdu::SetClientView(codec::SetClientView { tab_id, view }) => {
                let client_id = self.client_id.clone();
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let client_id = client_id.ok_or_else(|| {
                                anyhow!("SetClientView requires an identified client")
                            })?;
                            Mux::get().set_client_view(&client_id, tab_id, mux_view(view));
                            Ok(Pdu::UnitResponse(UnitResponse {}))
                        },
                        send_response,
                    )
                })
                .detach();
            }
            Pdu::ClaimClientViewport(ClaimClientViewport { tab_id, viewport }) => {
                let client_id = self.client_id.clone();
                let registration = self.client_registration;
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let client_id = client_id.ok_or_else(|| {
                                anyhow!("ClaimClientViewport requires an identified client")
                            })?;
                            let registration = registration.ok_or_else(|| {
                                anyhow!("ClaimClientViewport requires a live client registration")
                            })?;
                            let state = Mux::get()
                                .claim_registered_client_viewport(
                                    &client_id,
                                    registration,
                                    tab_id,
                                    mux_viewport(viewport),
                                )?
                                .ok_or_else(|| anyhow!("client connection was superseded"))?;
                            Ok(Pdu::ClientViewportState(codec_viewport_state(state)))
                        },
                        send_response,
                    )
                })
                .detach();
            }

            Pdu::SetFrontendAccessMode(SetFrontendAccessMode {
                mode,
                tab_id,
                viewport,
            }) => {
                let client_id = self.client_id.clone();
                let registration = self.client_registration;
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let client_id = client_id.ok_or_else(|| {
                                anyhow!("SetFrontendAccessMode requires an identified client")
                            })?;
                            let registration = registration.ok_or_else(|| {
                                anyhow!("SetFrontendAccessMode requires a live client registration")
                            })?;
                            let mux = Mux::get();
                            let target_mode = mux_access_mode(mode);
                            let viewport = mux_viewport(viewport);
                            mux.validate_registered_frontend_access_mode_change(
                                &client_id,
                                registration,
                                target_mode,
                                tab_id,
                                &viewport,
                            )?;
                            let prior_mode = mux.frontend_access_state().mode;
                            crate::thinkterm_access::persist_mode(target_mode)?;
                            match mux.set_registered_frontend_access_mode(
                                &client_id,
                                registration,
                                target_mode,
                                tab_id,
                                viewport,
                            ) {
                                Ok(state) => {
                                    Ok(Pdu::FrontendAccessState(codec_access_state(state)))
                                }
                                Err(err) => {
                                    if prior_mode != target_mode {
                                        if let Err(rollback) =
                                            crate::thinkterm_access::persist_mode(prior_mode)
                                        {
                                            log::error!(
                                                "failed to roll back persisted frontend mode: \
                                                 {rollback:#}"
                                            );
                                        }
                                    }
                                    Err(err)
                                }
                            }
                        },
                        send_response,
                    )
                })
                .detach();
            }

            Pdu::MutateThinkTermTree(MutateThinkTermTree { ops }) => {
                // mutate() broadcasts to every connection when the batch
                // changed something; the direct response here is what lets the
                // caller reconcile even when it did not.
                send_response(
                    crate::thinkterm_tree::mutate(&ops)
                        .map(|tree| Pdu::ThinkTermTreeState(ThinkTermTreeState { tree })),
                );
            }

            Pdu::MovePaneToNewTab(request) => {
                let client_id = self.client_id.clone();
                spawn_into_main_thread(async move {
                    schedule_move_pane(request, send_response, client_id);
                })
                .detach();
            }

            Pdu::GetPaneRenderableDimensions(GetPaneRenderableDimensions { pane_id }) => {
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            let pane = mux
                                .get_pane(pane_id)
                                .ok_or_else(|| anyhow!("no such pane {}", pane_id))?;
                            let cursor_position = pane.get_cursor_position();
                            let dimensions = pane.get_dimensions();
                            Ok(Pdu::GetPaneRenderableDimensionsResponse(
                                GetPaneRenderableDimensionsResponse {
                                    pane_id,
                                    cursor_position,
                                    dimensions,
                                },
                            ))
                        },
                        send_response,
                    )
                })
                .detach();
            }

            Pdu::GetPaneRenderChanges(GetPaneRenderChanges { pane_id, .. }) => {
                let sender = self.to_write_tx.clone();
                let per_pane = self.per_pane(pane_id);
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            let is_alive = match mux.get_pane(pane_id) {
                                Some(pane) => {
                                    maybe_push_pane_changes(&pane, sender, per_pane)?;
                                    true
                                }
                                None => false,
                            };
                            Ok(Pdu::LivenessResponse(LivenessResponse {
                                pane_id,
                                is_alive,
                            }))
                        },
                        send_response,
                    )
                })
                .detach();
            }

            Pdu::GetLines(GetLines { pane_id, lines }) => {
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            let pane = mux
                                .get_pane(pane_id)
                                .ok_or_else(|| anyhow!("no such pane {}", pane_id))?;
                            let mut lines_and_indices = vec![];

                            for range in lines {
                                let (first_row, lines) = pane.get_lines(range);
                                for (idx, mut line) in lines.into_iter().enumerate() {
                                    let stable_row = first_row + idx as StableRowIndex;
                                    line.compress_for_scrollback();
                                    lines_and_indices.push((stable_row, line));
                                }
                            }
                            Ok(Pdu::GetLinesResponse(GetLinesResponse {
                                pane_id,
                                lines: lines_and_indices.into(),
                            }))
                        },
                        send_response,
                    )
                })
                .detach();
            }

            Pdu::GetImageCell(GetImageCell {
                pane_id,
                line_idx,
                cell_idx,
                data_hash,
            }) => {
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            let mut data = None;

                            let pane = mux
                                .get_pane(pane_id)
                                .ok_or_else(|| anyhow!("no such pane {}", pane_id))?;

                            let (_, lines) = pane.get_lines(line_idx..line_idx + 1);
                            'found_data: for line in lines {
                                if let Some(cell) = line.get_cell(cell_idx) {
                                    if let Some(images) = cell.attrs().images() {
                                        for im in images {
                                            if im.image_data().hash() == data_hash {
                                                data.replace(im.image_data().clone());
                                                break 'found_data;
                                            }
                                        }
                                    }
                                }
                            }
                            Ok(Pdu::GetImageCellResponse(GetImageCellResponse {
                                pane_id,
                                data,
                            }))
                        },
                        send_response,
                    )
                })
                .detach();
            }

            Pdu::GetCodecVersion(_) => {
                match std::env::current_exe().context("resolving current_exe") {
                    Err(err) => send_response(Err(err)),
                    Ok(executable_path) => {
                        send_response(Ok(Pdu::GetCodecVersionResponse(GetCodecVersionResponse {
                            codec_vers: CODEC_VERSION,
                            version_string: config::wezterm_version().to_owned(),
                            server_id: Mux::get().runtime_server_id().to_string(),
                            executable_path,
                            config_file_path: std::env::var_os("WEZTERM_CONFIG_FILE")
                                .map(Into::into),
                        })))
                    }
                }
            }

            Pdu::GetTlsCreds(_) => {
                catch(
                    move || {
                        let client_cert_pem = PKI.generate_client_cert()?;
                        let ca_cert_pem = PKI.ca_pem_string()?;
                        Ok(Pdu::GetTlsCredsResponse(GetTlsCredsResponse {
                            client_cert_pem,
                            ca_cert_pem,
                        }))
                    },
                    send_response,
                );
            }
            Pdu::WindowTitleChanged(WindowTitleChanged { window_id, title }) => {
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            let mut window = mux
                                .get_window_mut(window_id)
                                .ok_or_else(|| anyhow!("no such window {window_id}"))?;

                            window.set_title(&title);

                            Ok(Pdu::UnitResponse(UnitResponse {}))
                        },
                        send_response,
                    )
                })
                .detach();
            }
            Pdu::TabTitleChanged(TabTitleChanged { tab_id, title }) => {
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            let tab = mux
                                .get_tab(tab_id)
                                .ok_or_else(|| anyhow!("no such tab {tab_id}"))?;

                            tab.set_title(&title);

                            Ok(Pdu::UnitResponse(UnitResponse {}))
                        },
                        send_response,
                    )
                })
                .detach();
            }
            Pdu::SetPalette(SetPalette { pane_id, palette }) => {
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            let pane = mux
                                .get_pane(pane_id)
                                .ok_or_else(|| anyhow!("no such pane {}", pane_id))?;
                            let palette_session_id = palette_session_id.ok_or_else(|| {
                                anyhow!("palette advisory requires a live client session")
                            })?;

                            // Advice from a background client is stored only.
                            // If this client already owns the pane, a config
                            // reload updates the OSC query base immediately.
                            if let Some(palette) =
                                mux.advise_client_palette(palette_session_id, pane_id, palette)
                            {
                                apply_client_palette(&pane, Some(palette))?;
                            }

                            Ok(Pdu::UnitResponse(UnitResponse {}))
                        },
                        send_response,
                    )
                })
                .detach();
            }

            Pdu::AdjustPaneSize(AdjustPaneSize {
                pane_id,
                direction,
                amount,
            }) => {
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            let (_pane_domain_id, _window_id, tab_id) = mux
                                .resolve_pane_id(pane_id)
                                .ok_or_else(|| anyhow!("pane_id {} invalid", pane_id))?;

                            let tab = match mux.get_tab(tab_id) {
                                Some(tab) => tab,
                                None => {
                                    return Err(anyhow!(
                                        "Failed to retrieve tab with ID {}",
                                        tab_id
                                    ));
                                }
                            };

                            tab.adjust_pane_size(direction, amount);
                            Ok(Pdu::UnitResponse(UnitResponse {}))
                        },
                        send_response,
                    )
                })
                .detach();
            }

            Pdu::Invalid { .. } => send_response(Err(anyhow!("invalid PDU {:?}", decoded.pdu))),
            Pdu::Pong { .. }
            | Pdu::ListPanesResponse { .. }
            | Pdu::SetApplicationPalette { .. }
            | Pdu::SetClipboard { .. }
            | Pdu::NotifyAlert { .. }
            | Pdu::SpawnResponse { .. }
            | Pdu::GetPaneRenderChangesResponse { .. }
            | Pdu::UnitResponse { .. }
            | Pdu::LivenessResponse { .. }
            | Pdu::GetPaneDirectionResponse { .. }
            | Pdu::SearchScrollbackResponse { .. }
            | Pdu::GetLinesResponse { .. }
            | Pdu::GetCodecVersionResponse { .. }
            | Pdu::WindowWorkspaceChanged { .. }
            | Pdu::GetTlsCredsResponse { .. }
            | Pdu::GetClientListResponse { .. }
            | Pdu::PaneRemoved { .. }
            | Pdu::PaneFocused { .. }
            | Pdu::TabResized { .. }
            | Pdu::GetImageCellResponse { .. }
            | Pdu::MovePaneToNewTabResponse { .. }
            | Pdu::TabAddedToWindow { .. }
            | Pdu::GetPaneRenderableDimensionsResponse { .. }
            | Pdu::ThinkTermTreeState { .. }
            | Pdu::ThinkTermSessionState { .. }
            | Pdu::EnsureThinkTermThreadResponse { .. }
            | Pdu::ClientViewportState { .. }
            | Pdu::FrontendAccessState { .. }
            | Pdu::ErrorResponse { .. } => {
                send_response(Err(anyhow!("expected a request, got {:?}", decoded.pdu)))
            }
        }
    }
}

// Dancing around a little bit here; we can't directly spawn_into_main_thread the domain_spawn
// function below because the compiler thinks that all of its locals then need to be Send.
// We need to shimmy through this helper to break that aspect of the compiler flow
// analysis and allow things to compile.
fn schedule_domain_spawn_v2<SND>(spawn: SpawnV2, send_response: SND)
where
    SND: Fn(anyhow::Result<Pdu>) + 'static,
{
    promise::spawn::spawn(async move { send_response(domain_spawn_v2(spawn).await) }).detach();
}

fn schedule_ensure_thinkterm_thread<SND>(request: EnsureThinkTermThread, send_response: SND)
where
    SND: Fn(anyhow::Result<Pdu>) + 'static,
{
    promise::spawn::spawn(async move { send_response(ensure_thinkterm_thread(request).await) })
        .detach();
}

async fn ensure_thinkterm_thread(request: EnsureThinkTermThread) -> anyhow::Result<Pdu> {
    let _materialize = THINKTERM_MATERIALIZE.lock().await;
    let mux = Mux::get();
    // No identity is installed here on purpose: every workspace name on this
    // path is explicit (`landing.workspace`), and a guard held across the
    // awaits below would leak the requesting identity to unrelated
    // main-thread work. See `Mux::with_identity`.
    let landing = crate::thinkterm_tree::ensure_landing(request.preferred_thread_id.as_deref())?;

    let has_live_pane = mux
        .iter_windows_in_workspace(&landing.workspace)
        .into_iter()
        .filter_map(|window_id| mux.get_window(window_id))
        .any(|window| window.iter().any(|tab| !tab.iter_all_panes().is_empty()));

    let spawned = if has_live_pane {
        false
    } else {
        crate::thinkterm_layout::begin_restore(&landing.workspace);
        let restored = crate::thinkterm_layout::restore_thread_layout(
            &landing.thread_id,
            &landing.workspace,
            &landing.project_path,
            request.size,
        )
        .await;

        let (materialized, preserve_saved_layout) = match restored {
            Ok(true) => (Ok(()), false),
            Ok(false) => (
                spawn_default_thinkterm_thread(&mux, &landing, request.size).await,
                false,
            ),
            Err(err) => {
                log::warn!(
                    "failed to restore ThinkTerm layout for thread {}: {err:#}; \
                     opening one default shell",
                    landing.thread_id
                );
                (
                    spawn_default_thinkterm_thread(&mux, &landing, request.size)
                        .await
                        .context("spawn fallback shell after layout restore failure"),
                    true,
                )
            }
        };

        match materialized {
            Ok(()) => {
                crate::thinkterm_layout::finish_restore(&landing.workspace, !preserve_saved_layout);
                crate::thinkterm_session::publish_changed();
                true
            }
            Err(err) => {
                crate::thinkterm_layout::finish_restore(&landing.workspace, false);
                return Err(err);
            }
        }
    };

    Ok(Pdu::EnsureThinkTermThreadResponse(
        EnsureThinkTermThreadResponse {
            thread_id: landing.thread_id,
            workspace: landing.workspace,
            spawned,
        },
    ))
}

/// NOTE: this runs with *no* identity installed (see `ensure_thinkterm_thread`).
/// If layout restore or this fallback ever gains a "send a command to the new
/// shell" step, that write would reach `record_input_for_current_identity`
/// and, in `TmuxLatest`, claim the tab's viewport for the ambient identity
/// (the GUI's, in a GUI-hosted mux) — resolve the requesting identity
/// explicitly at that point instead of re-adding a `with_identity` guard.
async fn spawn_default_thinkterm_thread(
    mux: &Mux,
    landing: &crate::thinkterm_tree::LandingRecord,
    size: wezterm_term::TerminalSize,
) -> anyhow::Result<()> {
    let command_dir = match landing.project_path.trim() {
        "" => None,
        path if path.starts_with("wezterm-mux://") => None,
        path => Some(path.to_string()),
    };
    mux.spawn_tab_or_window(
        None,
        SpawnTabDomain::DefaultDomain,
        None,
        command_dir,
        size,
        None,
        landing.workspace.clone(),
        None,
    )
    .await?;
    Ok(())
}

fn schedule_split_pane<SND>(split: SplitPane, send_response: SND)
where
    SND: Fn(anyhow::Result<Pdu>) + 'static,
{
    promise::spawn::spawn(async move { send_response(split_pane(split).await) }).detach();
}

fn schedule_spawn_pane_in_stack<SND>(request: SpawnPaneInStack, send_response: SND)
where
    SND: Fn(anyhow::Result<Pdu>) + 'static,
{
    promise::spawn::spawn(async move { send_response(spawn_pane_in_stack(request).await) })
        .detach();
}

fn schedule_move_pane_to_stack<SND>(request: MovePaneToStack, send_response: SND)
where
    SND: Fn(anyhow::Result<Pdu>) + 'static,
{
    promise::spawn::spawn(async move {
        let mux = Mux::get();
        // No identity: `move_pane_to_stack` reads none, and a guard held
        // across the await would leak the identity to unrelated work.
        send_response(
            mux.move_pane_to_stack(request.source_pane_id, request.target_pane_id)
                .await
                .map(|_| Pdu::UnitResponse(UnitResponse {})),
        );
    })
    .detach();
}

async fn spawn_pane_in_stack(request: SpawnPaneInStack) -> anyhow::Result<Pdu> {
    let mux = Mux::get();
    // No identity: `spawn_pane_in_stack` reads none, and a guard held across
    // the await would leak the identity to unrelated main-thread work.

    let (_pane_domain_id, window_id, tab_id) = mux
        .resolve_pane_id(request.pane_id)
        .ok_or_else(|| anyhow!("pane_id {} invalid", request.pane_id))?;

    // The new pane joins the stack occupying the same rect as the base
    // pane, so it inherits the base pane's current size.
    let base = mux
        .get_pane(request.pane_id)
        .ok_or_else(|| anyhow!("pane_id {} invalid", request.pane_id))?;
    let dims = base.get_dimensions();
    let size = ::wezterm_term::TerminalSize {
        rows: dims.viewport_rows,
        cols: dims.cols,
        pixel_width: dims.pixel_width,
        pixel_height: dims.pixel_height,
        dpi: dims.dpi,
    };

    let pane = mux
        .spawn_pane_in_stack(
            request.pane_id,
            request.domain,
            request.command.map(|c| c.into_command_builder()),
            request.command_dir,
            size,
        )
        .await?;

    Ok::<Pdu, anyhow::Error>(Pdu::SpawnResponse(SpawnResponse {
        pane_id: pane.pane_id(),
        tab_id,
        window_id,
        size,
    }))
}

async fn split_pane(split: SplitPane) -> anyhow::Result<Pdu> {
    let mux = Mux::get();
    // No identity: `Mux::split_pane` reads none, and a guard held across the
    // await would leak the identity to unrelated main-thread work.

    let (_pane_domain_id, window_id, tab_id) = mux
        .resolve_pane_id(split.pane_id)
        .ok_or_else(|| anyhow!("pane_id {} invalid", split.pane_id))?;

    let source = if let Some(move_pane_id) = split.move_pane_id {
        SplitSource::MovePane(move_pane_id)
    } else {
        SplitSource::Spawn {
            command: split.command.map(|c| c.into_command_builder()),
            command_dir: split.command_dir,
        }
    };

    let (pane, size) = mux
        .split_pane(split.pane_id, split.split_request, source, split.domain)
        .await?;

    Ok::<Pdu, anyhow::Error>(Pdu::SpawnResponse(SpawnResponse {
        pane_id: pane.pane_id(),
        tab_id,
        window_id,
        size,
    }))
}

async fn domain_spawn_v2(spawn: SpawnV2) -> anyhow::Result<Pdu> {
    let mux = Mux::get();
    // No identity: `spawn.workspace` is an explicit non-optional string on
    // the wire, so `spawn_tab_or_window` never falls back to the ambient
    // identity's workspace. A guard held across the await would leak the
    // identity to unrelated main-thread work.

    let (tab, pane, window_id) = mux
        .spawn_tab_or_window(
            spawn.window_id,
            spawn.domain,
            spawn.command.map(|c| c.into_command_builder()),
            spawn.command_dir,
            spawn.size,
            None, // optional current pane_id
            spawn.workspace,
            None, // optional gui window position
        )
        .await?;

    Ok::<Pdu, anyhow::Error>(Pdu::SpawnResponse(SpawnResponse {
        pane_id: pane.pane_id(),
        tab_id: tab.tab_id(),
        window_id,
        size: tab.get_size(),
    }))
}

fn schedule_move_pane<SND>(
    request: MovePaneToNewTab,
    send_response: SND,
    client_id: Option<Arc<ClientId>>,
) where
    SND: Fn(anyhow::Result<Pdu>) + 'static,
{
    promise::spawn::spawn(async move { send_response(move_pane(request, client_id).await) })
        .detach();
}

/// `Mux::move_pane_to_new_tab` falls back to the *global identity's*
/// workspace when it has to create a window and nobody named one. Resolve
/// that name here, synchronously and per-client, so no identity has to
/// survive the await.
///
/// Only the new-window case is filled in: with a `window_id` the workspace
/// is unused locally, and leaving it `None` keeps the PDU forwarded to a
/// nested mux byte-identical to before.
fn workspace_for_moved_pane(
    mux: &Mux,
    requested: Option<String>,
    window_id: Option<mux::window::WindowId>,
    client_id: Option<&Arc<ClientId>>,
) -> Option<String> {
    if requested.is_some() || window_id.is_some() {
        return requested;
    }
    Some(mux.active_workspace_for_optional_client(client_id))
}

async fn move_pane(
    request: MovePaneToNewTab,
    client_id: Option<Arc<ClientId>>,
) -> anyhow::Result<Pdu> {
    let mux = Mux::get();
    // No identity is installed here: the one identity-derived value on this
    // path (the fallback workspace for a new window) is resolved explicitly
    // below. A guard held across the await would leak the identity to
    // unrelated main-thread work.
    let workspace = workspace_for_moved_pane(
        &mux,
        request.workspace_for_new_window,
        request.window_id,
        client_id.as_ref(),
    );

    let (tab, window_id) = mux
        .move_pane_to_new_tab(request.pane_id, request.window_id, workspace)
        .await?;

    Ok::<Pdu, anyhow::Error>(Pdu::MovePaneToNewTabResponse(MovePaneToNewTabResponse {
        tab_id: tab.tab_id(),
        window_id,
    }))
}

#[cfg(test)]
mod tests {
    use super::{
        claim_viewport_for_pane, requires_existing_frontend_access, workspace_for_moved_pane,
        PerPane,
    };
    use codec::{EnsureThinkTermThread, Pdu};
    use mux::client::ClientId;
    use mux::Mux;
    use std::sync::Arc;
    use wezterm_term::color::ColorPalette;
    use wezterm_term::TerminalSize;

    #[test]
    fn cold_thread_materialization_does_not_require_an_existing_owner() {
        let request = Pdu::EnsureThinkTermThread(EnsureThinkTermThread {
            preferred_thread_id: Some("thread-main".to_string()),
            size: TerminalSize::default(),
        });

        assert!(!requires_existing_frontend_access(&request));
    }

    #[test]
    fn terminal_input_requires_identity_and_a_live_registration() {
        let mux = Mux::new(None);
        let missing_identity = claim_viewport_for_pane(&mux, None, None, usize::MAX)
            .unwrap_err()
            .to_string();
        assert!(missing_identity.contains("identified client"));

        let client = Arc::new(mux::client::generate_client_id());
        let missing_registration = claim_viewport_for_pane(&mux, Some(&client), None, usize::MAX)
            .unwrap_err()
            .to_string();
        assert!(missing_registration.contains("live client registration"));

        let stale_registration = mux.register_client(Arc::clone(&client));
        mux.unregister_client(&client, stale_registration);
        let superseded =
            claim_viewport_for_pane(&mux, Some(&client), Some(stale_registration), usize::MAX)
                .unwrap_err()
                .to_string();
        assert!(superseded.contains("superseded client connection"));
    }

    #[test]
    fn moved_pane_workspace_is_resolved_from_the_requesting_client() {
        config::use_test_configuration();
        let mux = Mux::new(None);
        let client = Arc::new(mux::client::generate_client_id());
        mux.register_client(Arc::clone(&client));
        mux.set_active_workspace_for_client(&client, "space-2");

        // An explicitly requested workspace passes through untouched.
        assert_eq!(
            workspace_for_moved_pane(&mux, Some("named".to_string()), None, Some(&client)),
            Some("named".to_string())
        );
        // An existing-window move keeps the field empty: it is unused locally
        // and the forwarded nested-mux PDU stays byte-identical.
        assert_eq!(
            workspace_for_moved_pane(&mux, None, Some(7), Some(&client)),
            None
        );
        // A new window with no name lands in the requesting client's
        // workspace...
        assert_eq!(
            workspace_for_moved_pane(&mux, None, None, Some(&client)),
            Some("space-2".to_string())
        );
        // ...and in the default workspace when the request is anonymous.
        assert_eq!(
            workspace_for_moved_pane(&mux, None, None, None),
            Some("default".to_string())
        );
    }

    #[test]
    fn application_palette_delivery_distinguishes_unsent_from_reset() {
        let mut state = PerPane::default();
        assert!(state.needs_application_palette(&None));

        state.record_application_palette(None);
        assert!(!state.needs_application_palette(&None));

        let palette = Some(ColorPalette::default());
        assert!(state.needs_application_palette(&palette));
        state.record_application_palette(palette.clone());
        assert!(!state.needs_application_palette(&palette));

        assert!(state.needs_application_palette(&None));
    }
}
