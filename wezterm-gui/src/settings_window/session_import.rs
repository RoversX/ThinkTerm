use super::*;
use mux::domain::{Domain, DomainId, DomainState};
use std::sync::Arc;
use thinkterm_import::{ImportMode, Preview, Session};
use wezterm_mux_server_impl::session_import;

#[derive(Debug, Clone)]
pub(super) struct ImportUi {
    sessions: Vec<Session>,
    target: Option<ImportTarget>,
    origin_window: Option<MuxWindowId>,
    notes: Vec<String>,
    preview: Option<Preview>,
    result: Option<codec::ImportSessionResponse>,
    pending: Option<codec::ImportSessionRequest>,
    busy: bool,
    failed: bool,
    message: String,
}

thread_local! {
    // Keep the request while Settings is closed, until its outcome is known.
    static UNRESOLVED: RefCell<Option<ImportUi>> = const { RefCell::new(None) };
}

pub(super) fn remembered_source() -> Option<ImportSource> {
    UNRESOLVED.with(|slot| {
        let state = slot.borrow();
        let id = &state.as_ref()?.pending.as_ref()?.request.source;
        thinkterm_import::sources()
            .into_iter()
            .find(|s| s.id == id)
            .map(|s| ImportSource::Session(s.id))
    })
}

impl Default for ImportUi {
    fn default() -> Self {
        UNRESOLVED
            .with(|slot| slot.borrow().clone())
            .map(|mut state| {
                state.busy = false;
                state
            })
            .unwrap_or_else(|| Self {
                sessions: Vec::new(),
                target: None,
                origin_window: None,
                notes: Vec::new(),
                preview: None,
                result: None,
                pending: None,
                busy: false,
                failed: false,
                message: String::new(),
            })
    }
}

impl ImportUi {
    /// An import whose outcome is unknown blocks the page until a recovery
    /// attempt fails. Then the user may leave it behind (see `abandon`).
    pub(super) fn busy(&self) -> bool {
        self.busy || (self.pending.is_some() && !self.failed)
    }

    /// Stop tracking an import whose outcome could not be found out. Its
    /// receipt on the owner still keeps that request from running twice.
    pub(super) fn abandon(&mut self) {
        self.pending = None;
        UNRESOLVED.with(|slot| slot.borrow_mut().take());
    }

    pub(super) fn has_preview(&self) -> bool {
        self.preview.is_some()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ImportTarget {
    Local,
    Remote { name: String, domain_id: DomainId },
}

impl ImportTarget {
    /// The machine of the Space Settings was opened from.
    fn current() -> anyhow::Result<Self> {
        let space_id = OPENED_FROM_SPACE.with(|slot| slot.borrow().clone());
        let Some(space_id) = space_id else {
            return Ok(Self::Local);
        };
        let store = crate::workspace_threads::workspace_thread_store_snapshot();
        Self::for_host(origin_domain(&store, &space_id)?)
    }

    /// This machine for `None`, else the connection named `host`. A host
    /// not connected since launch is registered the way its Space would
    /// register it; `owner` connects it.
    fn for_host(host: Option<String>) -> anyhow::Result<Self> {
        let Some(name) = host else {
            return Ok(Self::Local);
        };
        let domain = match mux::Mux::get().get_domain_by_name(&name) {
            Some(domain) => domain,
            None => crate::connect_domain_from_ssh_host(&name)
                .context("The import connection is no longer available")?,
        };
        anyhow::ensure!(
            domain
                .downcast_ref::<wezterm_client::domain::ClientDomain>()
                .is_some(),
            "Session import requires a ThinkTerm connection"
        );
        Ok(Self::Remote {
            name,
            domain_id: domain.domain_id(),
        })
    }

    fn host(&self) -> Option<&str> {
        match self {
            Self::Local => None,
            Self::Remote { name, .. } => Some(name),
        }
    }

    fn location(&self, source: ImportSource) -> String {
        match self {
            Self::Local => source.text("session-import-location-local"),
            Self::Remote { name, .. } => {
                source.text_args("session-import-location-remote", &[("host", name.clone())])
            }
        }
    }

    async fn owner(&self) -> anyhow::Result<Option<Arc<dyn Domain>>> {
        let name = match self {
            Self::Local => crate::local_sessions::host_domain_name(),
            Self::Remote { name, .. } => Some(name.clone()),
        };
        let Some(name) = name else {
            return Ok(None);
        };
        let domain = mux::Mux::get()
            .get_domain_by_name(&name)
            .context("The import connection is no longer available")?;
        if let Self::Remote { domain_id, .. } = self {
            anyhow::ensure!(
                *domain_id == domain.domain_id(),
                "The import connection changed; inspect the session again"
            );
        }
        let client = domain
            .downcast_ref::<wezterm_client::domain::ClientDomain>()
            .context("Session import requires a ThinkTerm connection")?;
        if client.state() != DomainState::Attached {
            client
                .attach_with_ui(
                    None,
                    mux::connui::ConnectionUI::new_lazy(Default::default()),
                )
                .await?;
        }
        Ok(Some(domain))
    }

    async fn discover(&self, source: String) -> anyhow::Result<codec::ListImportSessionsResponse> {
        if let Some(domain) = self.owner().await? {
            domain
                .downcast_ref::<wezterm_client::domain::ClientDomain>()
                .unwrap()
                .list_import_sessions(source)
                .await
        } else {
            promise::spawn::spawn_into_new_thread(move || {
                session_import::discover(codec::ListImportSessions { source })
            })
            .await
        }
    }

    async fn preview(
        &self,
        source: String,
        session: String,
    ) -> anyhow::Result<codec::PreviewImportSessionResponse> {
        if let Some(domain) = self.owner().await? {
            domain
                .downcast_ref::<wezterm_client::domain::ClientDomain>()
                .unwrap()
                .preview_import_session(source, session)
                .await
        } else {
            promise::spawn::spawn_into_new_thread(move || {
                session_import::preview(codec::PreviewImportSession { source, session })
            })
            .await
        }
    }

    async fn recover(
        &self,
        request: codec::ImportSessionRequest,
    ) -> anyhow::Result<codec::ImportSessionStatus> {
        let status = if let Some(domain) = self.owner().await? {
            domain
                .downcast_ref::<wezterm_client::domain::ClientDomain>()
                .unwrap()
                .get_import_session_status(request.request_id.clone())
                .await?
        } else {
            session_import::status(request.request_id.clone()).await?
        };
        match status {
            codec::ImportSessionStatus::NotFound => self
                .import(request)
                .await
                .map(codec::ImportSessionStatus::Completed),
            status => Ok(status),
        }
    }

    async fn import(
        &self,
        request: codec::ImportSessionRequest,
    ) -> anyhow::Result<codec::ImportSessionResponse> {
        if let Some(domain) = self.owner().await? {
            domain
                .downcast_ref::<wezterm_client::domain::ClientDomain>()
                .unwrap()
                .import_session(request)
                .await
        } else {
            session_import::execute(request, crate::workspace_threads::persist_session_import).await
        }
    }
}

/// The machines an import can run on: this one, then each host a remote
/// Space comes from.
fn import_hosts() -> Vec<Option<String>> {
    std::iter::once(None)
        .chain(
            crate::workspace_threads::remote_space_domains()
                .into_iter()
                .map(Some),
        )
        .collect()
}

/// Domain names are never empty, so "" can stand for this machine.
fn host_key(host: Option<&str>) -> u64 {
    web_token_key(host.unwrap_or(""))
}

fn host_connected(name: &str) -> bool {
    mux::Mux::get()
        .get_domain_by_name(name)
        .is_some_and(|domain| domain.state() == DomainState::Attached)
}

fn source_id(source: ImportSource) -> Option<&'static str> {
    match source {
        ImportSource::Session(id) => Some(id),
        ImportSource::WezTerm | ImportSource::Editors => None,
    }
}

/// Note a finished import where the Import page will find it, whether or
/// not Settings is still open to show the result.
fn remember_import(
    target: &ImportTarget,
    request: &codec::ImportSessionRequest,
    status: &anyhow::Result<codec::ImportSessionStatus>,
) {
    if let Ok(codec::ImportSessionStatus::Completed(result)) = status {
        crate::workspace_threads::note_imported_session(
            &request.request.source,
            &request.request.session,
            target.host(),
            &result.space_id,
        );
    }
}

fn origin_domain(
    store: &crate::workspace_threads::WorkspaceThreadStore,
    space_id: &str,
) -> anyhow::Result<Option<String>> {
    let space = store
        .spaces
        .iter()
        .find(|space| space.id == space_id)
        .context("The import origin Space is no longer available")?;
    Ok(space.client_domain.clone())
}

fn localized_error(source: ImportSource, message: &str, fallback: &'static str) -> String {
    let generic = [
        (
            "An imported working directory is unavailable",
            "session-import-error-directory-missing",
        ),
        (
            "Import mode changed after preview",
            "session-import-error-changed",
        ),
        (
            "Session import requires an authenticated desktop connection",
            "session-import-error-local-user",
        ),
        (
            "Session import requires a local terminal owner",
            "session-import-error-local-user",
        ),
        (
            "Some imported panes have no live terminal",
            "session-import-error-terminal-missing",
        ),
    ]
    .iter()
    .find_map(|(reason, key)| message.ends_with(reason).then_some(*key));
    let known = generic.or_else(|| source.provider().ok()?.error_key(message));
    if let Some(key) = known.filter(|key| *key != "session-import-error-receiver") {
        return source.text(key);
    }
    format!(
        "{}\n\n{}",
        source.text(known.unwrap_or(fallback)),
        source.text_args(
            "session-import-error-details",
            &[("error", message.to_string())]
        )
    )
}

async fn open_import(
    target: ImportTarget,
    result: codec::ImportSessionResponse,
    origin_window: Option<MuxWindowId>,
) -> anyhow::Result<()> {
    if let Some(domain) = target.owner().await? {
        let client = domain
            .downcast_ref::<wezterm_client::domain::ClientDomain>()
            .unwrap();
        client.fetch_thinkterm_tree().await?;
        client.resync().await?;
    }
    let expected_domain = match &target {
        ImportTarget::Local => None,
        ImportTarget::Remote { name, .. } => Some(name.as_str()),
    };
    let store = crate::workspace_threads::workspace_thread_store_snapshot();
    anyhow::ensure!(
        store.spaces.iter().any(|space| space.id == result.space_id
            && space.client_domain.as_deref() == expected_domain),
        "The imported Space is not available on its original connection"
    );
    let frontend = crate::frontend::try_front_end().context("ThinkTerm frontend is unavailable")?;
    let windows = frontend.gui_windows();
    let mux = mux::Mux::get();
    let already_open = windows.iter().find(|gui| {
        mux.get_window(gui.mux_window_id).is_some_and(|window| {
            crate::workspace_threads::space_id_for_workspace(window.get_workspace()).as_deref()
                == Some(result.space_id.as_str())
        })
    });
    if let Some(gui) = already_open
        .or_else(|| {
            windows
                .iter()
                .find(|gui| Some(gui.mux_window_id) == origin_window)
        })
        .or_else(|| windows.first())
    {
        let window = gui.window.clone();
        gui.window
            .notify(crate::termwindow::TermWindowNotif::Apply(Box::new(
                move |tw| {
                    let thread = result
                        .tree
                        .projects
                        .iter()
                        .flat_map(|p| &p.threads)
                        .find(|t| {
                            t.materialized_workspace_name.as_deref()
                                == Some(result.workspace.as_str())
                        })
                        .map(|t| t.id.clone());
                    tw.switch_space_to_thread(result.space_id, thread, &window);
                    window.focus();
                },
            )));
        return Ok(());
    }
    let window_id = crate::workspace_threads::window_to_show_in_workspace(&result.workspace)
        .context("The imported session is not connected; reopen its Space after connecting to its session server")?;
    let owner = crate::workspace_threads::next_space_owner_id();
    anyhow::ensure!(
        crate::workspace_threads::switch_window_space(owner, &result.space_id),
        "The imported Space is already open"
    );
    frontend.claim_spawned_mux_window(window_id);
    frontend.set_switching_workspaces(true);
    let opened = crate::termwindow::TermWindow::new_window_with_claimed_space(
        window_id,
        owner,
        result.space_id,
        None,
    )
    .await;
    frontend.set_switching_workspaces(false);
    if opened.is_err() {
        crate::workspace_threads::release_window_space(owner);
    }
    opened
}

impl SettingsWindow {
    fn apply_import_status(
        &mut self,
        source: ImportSource,
        status: anyhow::Result<codec::ImportSessionStatus>,
    ) {
        use codec::ImportSessionStatus;
        let state = &mut self.ui.session_import;
        state.busy = false;
        match status {
            Ok(ImportSessionStatus::Completed(result)) => {
                state.pending = None;
                state.preview = None;
                state.sessions.clear();
                state.result = Some(result);
                state.failed = false;
                state.message.clear();
                self.ui.import_step = ImportStep::Result;
                self.ui.content_scroll.reset();
            }
            Ok(ImportSessionStatus::Running) => {
                state.failed = false;
                state.message = source.text("session-import-still-running");
            }
            Ok(ImportSessionStatus::Failed(error)) => {
                state.pending = None;
                state.failed = true;
                state.message = localized_error(source, &error, "session-import-error-import");
            }
            Ok(ImportSessionStatus::Interrupted { .. } | ImportSessionStatus::Expired) => {
                state.pending = None;
                state.failed = true;
                state.message = source.text("session-import-result-interrupted");
            }
            Ok(ImportSessionStatus::NotFound) => unreachable!("recovery resends the same request"),
            Err(err) => {
                state.failed = true;
                state.message = localized_error(
                    source,
                    &format!("{err:#}"),
                    "session-import-result-unavailable",
                );
            }
        }
        UNRESOLVED.with(|slot| {
            *slot.borrow_mut() = state.pending.as_ref().map(|_| state.clone());
        });
    }

    pub(super) fn perform_session_import_action(&mut self, action: SettingsAction) {
        if self.ui.session_import.busy {
            return;
        }
        if self.ui.session_import.pending.is_some()
            && !matches!(action, SettingsAction::SessionImportRecover)
        {
            if self.ui.session_import.busy() {
                return;
            }
            self.ui.session_import.abandon();
        }
        let selected_source = self.ui.import_source;
        let Ok(provider) = selected_source.provider() else {
            return;
        };
        let instance_id = self.instance_id;
        match action {
            SettingsAction::SessionImportDetect => {
                let target = match self
                    .ui
                    .session_import
                    .target
                    .clone()
                    .map(Ok)
                    .unwrap_or_else(ImportTarget::current)
                {
                    Ok(target) => target,
                    Err(err) => {
                        self.ui.session_import.failed = true;
                        self.ui.session_import.message = localized_error(
                            selected_source,
                            &format!("{err:#}"),
                            "session-import-error-inspect",
                        );
                        return;
                    }
                };
                self.ui.session_import = ImportUi {
                    target: Some(target.clone()),
                    origin_window: OPENED_FROM.with(|slot| slot.get()),
                    busy: true,
                    message: selected_source.text("session-import-detecting"),
                    ..ImportUi::default()
                };
                promise::spawn::spawn(async move {
                    let found = target.discover(provider.info().id.into()).await;
                    if let Some(settings) = settings_window_for_instance(instance_id) {
                        let mut settings = settings.borrow_mut();
                        if settings.ui.import_source != selected_source {
                            return;
                        }
                        settings.ui.session_import.busy = false;
                        match found {
                            Ok(found) => {
                                let sessions = found.sessions;
                                settings.ui.session_import.notes = found.notes;
                                settings.ui.session_import.message = if sessions.is_empty() {
                                    selected_source.text("session-import-none")
                                } else {
                                    String::new()
                                };
                                settings.ui.session_import.sessions = sessions;
                                if settings.ui.session_import.sessions.len() == 1 {
                                    let key =
                                        web_token_key(&settings.ui.session_import.sessions[0].name);
                                    settings.perform_session_import_action(
                                        SettingsAction::SessionImportPreview(key),
                                    );
                                }
                            }
                            Err(err) => {
                                settings.ui.session_import.failed = true;
                                settings.ui.session_import.message = localized_error(
                                    selected_source,
                                    &format!("{err:#}"),
                                    "session-import-error-inspect",
                                );
                            }
                        }
                        if let Some(window) = &settings.window {
                            window.invalidate();
                        }
                    }
                })
                .detach();
            }
            SettingsAction::SessionImportHost(key) => {
                let Some(host) = import_hosts()
                    .into_iter()
                    .find(|host| host_key(host.as_deref()) == key)
                else {
                    return;
                };
                match ImportTarget::for_host(host) {
                    Ok(target) => {
                        self.ui.session_import.target = Some(target);
                        self.perform_session_import_action(SettingsAction::SessionImportDetect);
                    }
                    Err(err) => {
                        self.ui.session_import.failed = true;
                        self.ui.session_import.message = localized_error(
                            selected_source,
                            &format!("{err:#}"),
                            "session-import-error-inspect",
                        );
                    }
                }
            }
            SettingsAction::SessionImportPreview(key) => {
                let Some(target) = self.ui.session_import.target.clone() else {
                    return;
                };
                let Some(session) = self
                    .ui
                    .session_import
                    .sessions
                    .iter()
                    .find(|session| web_token_key(&session.name) == key)
                    .cloned()
                else {
                    return;
                };
                self.ui.session_import.preview = None;
                self.ui.session_import.busy = true;
                self.ui.session_import.failed = false;
                self.ui.session_import.message = selected_source.text("session-import-detecting");
                promise::spawn::spawn(async move {
                    let preview = target
                        .preview(provider.info().id.into(), session.name)
                        .await;
                    if let Some(settings) = settings_window_for_instance(instance_id) {
                        let mut settings = settings.borrow_mut();
                        if settings.ui.import_source != selected_source {
                            return;
                        }
                        settings.ui.session_import.busy = false;
                        match preview {
                            Ok(response) => {
                                settings.ui.session_import.notes = response.notes;
                                settings.ui.session_import.preview = Some(response.preview);
                                settings.ui.session_import.message.clear();
                                settings.ui.content_scroll.reset();
                                #[cfg(debug_assertions)]
                                if std::env::var("THINKTERM_SETTINGS_EXPAND")
                                    .is_ok_and(|value| value == "import-preview")
                                {
                                    if let Some(offset) = initial_scroll() {
                                        settings.ui.content_scroll.offset = offset;
                                        settings.ui.content_scroll.target_offset = offset;
                                    }
                                }
                            }
                            Err(err) => {
                                settings.ui.session_import.failed = true;
                                settings.ui.session_import.message = localized_error(
                                    selected_source,
                                    &format!("{err:#}"),
                                    "session-import-error-inspect",
                                );
                            }
                        }
                        if let Some(window) = &settings.window {
                            window.invalidate();
                        }
                    }
                })
                .detach();
            }
            SettingsAction::SessionImportBack => {
                self.ui.session_import.preview = None;
                self.ui.session_import.failed = false;
                self.ui.session_import.message.clear();
            }
            SettingsAction::SessionImportConfirm => {
                let Some(target) = self.ui.session_import.target.clone() else {
                    return;
                };
                let Some(preview) = &self.ui.session_import.preview else {
                    return;
                };
                if preview.unavailable.is_some() || self.ui.session_import.failed {
                    return;
                }
                let request = codec::ImportSessionRequest {
                    request_id: format!(
                        "{}-{}",
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs(),
                        uuid::Uuid::new_v4()
                    ),
                    request: thinkterm_import::ImportRequest {
                        source: provider.info().id.into(),
                        session: preview.session.clone(),
                        mode: if preview.live {
                            ImportMode::Live
                        } else {
                            ImportMode::Layout
                        },
                        fingerprint: preview.fingerprint.clone(),
                        space_name: format!("{} — {}", selected_source.label(), preview.session),
                    },
                };
                self.ui.session_import.busy = true;
                self.ui.session_import.failed = false;
                self.ui.session_import.pending = Some(request.clone());
                self.ui.session_import.message = selected_source.text("session-import-importing");
                UNRESOLVED.with(|slot| *slot.borrow_mut() = Some(self.ui.session_import.clone()));
                promise::spawn::spawn(async move {
                    let imported = match target.import(request.clone()).await {
                        Ok(result) => Ok(codec::ImportSessionStatus::Completed(result)),
                        Err(_) => target.recover(request.clone()).await,
                    };
                    remember_import(&target, &request, &imported);
                    if let Some(settings) = settings_window_for_instance(instance_id) {
                        let mut settings = settings.borrow_mut();
                        if settings.ui.import_source != selected_source {
                            return;
                        }
                        settings.apply_import_status(selected_source, imported);
                        if let Some(window) = &settings.window {
                            window.invalidate();
                        }
                    }
                })
                .detach();
            }
            SettingsAction::SessionImportRecover => {
                let (Some(target), Some(request)) = (
                    self.ui.session_import.target.clone(),
                    self.ui.session_import.pending.clone(),
                ) else {
                    return;
                };
                self.ui.session_import.busy = true;
                promise::spawn::spawn(async move {
                    let status = target.recover(request.clone()).await;
                    remember_import(&target, &request, &status);
                    if let Some(settings) = settings_window_for_instance(instance_id) {
                        let mut settings = settings.borrow_mut();
                        settings.apply_import_status(selected_source, status);
                        if let Some(window) = &settings.window {
                            window.invalidate();
                        }
                    }
                })
                .detach();
            }
            SettingsAction::SessionImportOpen => {
                let Some(target) = self.ui.session_import.target.clone() else {
                    return;
                };
                let origin_window = self.ui.session_import.origin_window;
                let Some(result) = self.ui.session_import.result.clone() else {
                    return;
                };
                self.ui.session_import.busy = true;
                self.ui.session_import.failed = false;
                self.ui.session_import.message.clear();
                promise::spawn::spawn(async move {
                    let opened = open_import(target, result, origin_window).await;
                    if let Some(settings) = settings_window_for_instance(instance_id) {
                        let mut settings = settings.borrow_mut();
                        if settings.ui.import_source != selected_source {
                            return;
                        }
                        settings.ui.session_import.busy = false;
                        if let Err(err) = opened {
                            settings.ui.session_import.failed = true;
                            settings.ui.session_import.message = localized_error(
                                selected_source,
                                &format!("{err:#}"),
                                "session-import-error-open",
                            );
                        }
                        if let Some(window) = &settings.window {
                            window.invalidate();
                        }
                    }
                })
                .detach();
            }
            _ => {}
        }
    }

    pub(super) fn paint_session_import(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
    ) -> anyhow::Result<f32> {
        let selected_source = self.ui.import_source;
        let state = self.ui.session_import.clone();
        let palette = self.palette();
        let title = Rc::clone(&self.ui_font);
        let left = x;
        let inner = width;
        let gap = self.ui_px(20.0);
        let cell = self.metrics.cell_size.height as f32;
        let mut top = y;
        top = self.paint_import_source_heading(
            layers,
            left,
            top,
            inner,
            &selected_source.text("session-import-title"),
            &selected_source.text(if state.preview.is_some() {
                "session-import-preview-title"
            } else {
                "session-import-select-session"
            }),
        )? + self.ui_px(24.0);
        // The machine is chosen before a preview; an import in flight keeps
        // the one it was sent to.
        let hosts = if state.preview.is_none() && state.pending.is_none() {
            import_hosts()
        } else {
            Vec::new()
        };
        if hosts.len() > 1 {
            top = self.paint_import_hosts(
                layers,
                left,
                top,
                inner,
                &hosts,
                state.target.as_ref(),
                !state.busy,
            )? + gap;
        } else if let Some(target) = &state.target {
            top = self.paint_import_copy(
                layers,
                left,
                top,
                inner,
                &target.location(selected_source),
                palette.secondary_text,
            )? + gap;
        }
        let imported_into = |session: &str| {
            crate::workspace_threads::imported_session_space(
                source_id(selected_source)?,
                session,
                state.target.as_ref()?.host(),
            )
        };
        if let Some(preview) = &state.preview {
            let status = selected_source.text(if preview.live {
                "session-import-status-live"
            } else {
                "session-import-status-layout"
            });
            let status = match &preview.version {
                Some(version) => format!("{status} · {} {version}", selected_source.label()),
                None => status,
            };
            top = self.paint_import_copy(
                layers,
                left,
                top,
                inner,
                &status,
                TileColor::Teal.linear(),
            )? + gap;
        } else if state.result.is_none() {
            top = self.paint_import_copy(
                layers,
                left,
                top,
                inner,
                &selected_source.text("session-import-description"),
                palette.secondary_text,
            )? + gap;
        } else {
            top += gap;
        }

        let live = state.preview.as_ref().map(|preview| preview.live);
        let mut requirements = vec![selected_source.text(
            if matches!(state.target, Some(ImportTarget::Remote { .. })) {
                "session-import-requirements-remote"
            } else {
                "session-import-requirements-local"
            },
        )];
        if live != Some(true) {
            requirements.push(selected_source.text("session-import-requirements-layout"));
        }
        requirements.push(selected_source.text("session-import-requirements-refresh"));
        requirements.extend(state.notes.iter().map(|key| selected_source.text(key)));
        if let Some(reason) = state
            .preview
            .as_ref()
            .and_then(|preview| preview.unavailable.as_ref())
        {
            top = self.paint_import_notice(
                layers,
                left,
                top,
                inner,
                &localized_error(selected_source, reason, "session-import-error-inspect"),
                true,
            )? + gap;
        }

        if let Some(preview) = &state.preview {
            if let Some(space) = imported_into(&preview.session) {
                top = self.paint_import_notice(
                    layers,
                    left,
                    top,
                    inner,
                    &selected_source
                        .text_args("session-import-already-imported", &[("name", space)]),
                    true,
                )? + gap;
            }
            let destination = selected_source.text_args(
                "session-import-destination",
                &[(
                    "name",
                    format!("{} — {}", selected_source.label(), preview.session),
                )],
            );
            self.draw_svg_icon(layers, SvgIcon::Layers, left, top, cell, palette.muted_text)?;
            top = self.paint_import_copy(
                layers,
                left + cell + self.ui_px(10.0),
                top,
                inner - cell - self.ui_px(10.0),
                &destination,
                palette.text,
            )? + gap;
            let tabs = preview.projects.iter().map(|p| p.tabs).sum::<usize>();
            top = self.paint_session_counts(
                layers,
                left,
                top,
                inner,
                [preview.projects.len(), tabs, preview.pane_count()],
            )? + gap;
            top = self.paint_import_copy(
                layers,
                left,
                top,
                inner,
                &selected_source.text(if preview.live {
                    "session-import-live"
                } else {
                    "session-import-offline"
                }),
                palette.secondary_text,
            )? + gap;
            for project in &preview.projects {
                self.draw_rect(
                    layers,
                    0,
                    left,
                    top,
                    inner,
                    self.ui_px(1.0),
                    palette.separator,
                )?;
                top += self.ui_px(16.0);
                let icon = self.ui_px(24.0);
                self.draw_svg_icon(
                    layers,
                    SvgIcon::FolderOpen,
                    left,
                    top + (self.description_line_step() - icon) / 2.0,
                    icon,
                    palette.muted_text,
                )?;
                let text_x = left + icon + self.ui_px(14.0);
                let text_width = inner - icon - self.ui_px(14.0);
                let lines = self.wrap_settings_text(&title, &project.name, text_width);
                for line in lines {
                    self.draw_import_line(
                        layers,
                        &title,
                        text_x,
                        top,
                        text_width,
                        self.description_line_step(),
                        &line,
                        palette.text,
                    )?;
                    top += self.description_line_step();
                }
                let detail = selected_source.text_args(
                    "session-import-project-meta",
                    &[
                        ("threads", project.threads.to_string()),
                        ("tabs", project.tabs.to_string()),
                        ("panes", project.panes.to_string()),
                    ],
                );
                top = self.paint_import_copy(
                    layers,
                    text_x,
                    top + self.ui_px(4.0),
                    text_width,
                    &detail,
                    palette.secondary_text,
                )? + self.ui_px(12.0);
                let terminal_icon = self.ui_px(20.0);
                let terminal_x = text_x + terminal_icon + self.ui_px(10.0);
                let terminal_width = (left + inner - terminal_x).max(1.0);
                let line_height = self.description_line_step();
                for (index, terminal) in project.terminals.iter().enumerate() {
                    let label = terminal
                        .title
                        .as_deref()
                        .or(terminal.tab_name.as_deref())
                        .or_else(|| {
                            terminal.cwd.as_deref().and_then(|cwd| {
                                Path::new(cwd).file_name().and_then(|name| name.to_str())
                            })
                        })
                        .map(str::to_owned)
                        .unwrap_or_else(|| {
                            selected_source.text_args(
                                "session-import-terminal-untitled",
                                &[("number", (index + 1).to_string())],
                            )
                        });
                    self.draw_svg_icon(
                        layers,
                        SvgIcon::Terminal,
                        text_x,
                        top + (line_height - terminal_icon) / 2.0,
                        terminal_icon,
                        palette.muted_text,
                    )?;
                    self.draw_import_line(
                        layers,
                        &self.import_body_font,
                        terminal_x,
                        top,
                        terminal_width,
                        line_height,
                        &self.text_with_ellipsis(&self.import_body_font, &label, terminal_width),
                        palette.text,
                    )?;
                    top += line_height;
                    let mut details = Vec::new();
                    if let Some(tab) = &terminal.tab_name {
                        if tab != &label {
                            details.push(tab.clone());
                        }
                    }
                    if let Some(cwd) = &terminal.cwd {
                        details.push(home_relative(Path::new(cwd)));
                    }
                    if !details.is_empty() {
                        self.draw_import_line(
                            layers,
                            &self.import_body_font,
                            terminal_x,
                            top,
                            terminal_width,
                            line_height,
                            &self.text_with_ellipsis(
                                &self.import_body_font,
                                &details.join(" · "),
                                terminal_width,
                            ),
                            palette.secondary_text,
                        )?;
                        top += line_height;
                    }
                    top += self.ui_px(8.0);
                }
                top += self.ui_px(8.0);
            }
        } else {
            for session in &state.sessions {
                self.draw_rect(
                    layers,
                    0,
                    left,
                    top,
                    inner,
                    self.ui_px(1.0),
                    palette.separator,
                )?;
                top += self.ui_px(16.0);
                let label = selected_source.text("session-import-preview");
                let button_width = self.button_width_for_label(&label, 0.0).min(inner * 0.45);
                let height = self.ui_px(CONTROL_HEIGHT);
                self.draw_svg_icon(
                    layers,
                    SvgIcon::Terminal,
                    left,
                    top + (height - cell) / 2.0,
                    cell,
                    palette.muted_text,
                )?;
                let name_x = left + cell + self.ui_px(12.0);
                let name_y = self.control_text_y(top, height);
                let name_width =
                    (inner - (name_x - left) - button_width - self.ui_px(16.0)).max(1.0);
                self.draw_text(
                    layers,
                    &title,
                    name_x,
                    name_y,
                    &session.name,
                    palette.text,
                    name_width,
                )?;
                if imported_into(&session.name).is_some() {
                    self.paint_badge(
                        layers,
                        &selected_source.text("session-import-imported"),
                        TileColor::Teal.linear(),
                        name_x
                            + self
                                .measure_text_width(&title, &session.name)
                                .min(name_width)
                            + self.ui_px(14.0),
                        name_y,
                        name_x + name_width,
                    )?;
                }
                self.paint_import_actions(
                    layers,
                    left + inner - button_width,
                    top,
                    button_width,
                    &[ImportButton {
                        label,
                        action: SettingsAction::SessionImportPreview(web_token_key(&session.name)),
                        primary: false,
                        enabled: !state.busy,
                    }],
                )?;
                top += height + self.ui_px(16.0);
            }
        }
        if !state.message.is_empty() {
            top = if state.failed {
                self.paint_import_notice(layers, left, top, inner, &state.message, true)?
            } else {
                self.paint_import_copy(
                    layers,
                    left,
                    top,
                    inner,
                    &state.message,
                    palette.secondary_text,
                )?
            } + gap;
        }
        top = self.paint_session_notes(layers, left, top + gap, inner, &requirements)?;
        let buttons = if state.pending.is_some() {
            vec![ImportButton {
                label: selected_source.text("session-import-check-result"),
                action: SettingsAction::SessionImportRecover,
                primary: true,
                enabled: !state.busy,
            }]
        } else if let Some(preview) = &state.preview {
            vec![
                ImportButton {
                    label: selected_source.text("session-import-refresh-preview"),
                    action: SettingsAction::SessionImportPreview(web_token_key(&preview.session)),
                    primary: false,
                    enabled: !state.busy,
                },
                ImportButton {
                    label: selected_source.text(if preview.live {
                        "session-import-confirm-live"
                    } else {
                        "session-import-confirm-offline"
                    }),
                    action: SettingsAction::SessionImportConfirm,
                    primary: true,
                    enabled: !state.busy && !state.failed && preview.unavailable.is_none(),
                },
            ]
        } else {
            vec![ImportButton {
                label: selected_source.text("session-import-detect"),
                action: SettingsAction::SessionImportDetect,
                primary: true,
                enabled: !state.busy,
            }]
        };
        self.paint_import_footer(
            layers,
            left,
            top + gap,
            inner,
            Some(SettingsAction::ImportBack),
            &buttons,
        )
    }

    pub(super) fn paint_session_result(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
    ) -> anyhow::Result<f32> {
        let selected_source = self.ui.import_source;
        let provider = selected_source.provider()?;
        let state = self.ui.session_import.clone();
        let Some(result) = &state.result else {
            return Ok(y);
        };
        let message = selected_source.text_args(
            if result.live {
                "session-import-done-live"
            } else {
                "session-import-done-offline"
            },
            &[("count", result.pane_count.to_string())],
        );
        let mut bottom =
            self.paint_import_result(layers, x, y + self.ui_px(64.0), width, &message)?;
        if let Some(target) = &state.target {
            bottom = self.paint_import_centered_copy(
                layers,
                x,
                bottom + self.ui_px(16.0),
                width,
                &target.location(selected_source),
            )?;
        }
        if let Some(space) = result.tree.spaces.first() {
            bottom = self.paint_import_centered_copy(
                layers,
                x,
                bottom + self.ui_px(24.0),
                width,
                &selected_source.text_args(
                    "session-import-destination",
                    &[("name", space.name.clone())],
                ),
            )?;
        }
        bottom = self.paint_session_notes(
            layers,
            x,
            bottom + self.ui_px(40.0),
            width,
            &provider
                .result_notes()
                .iter()
                .map(|key| selected_source.text(key))
                .collect::<Vec<_>>(),
        )?;
        if !state.message.is_empty() {
            bottom = self.paint_import_notice(
                layers,
                x,
                bottom + self.ui_px(20.0),
                width,
                &state.message,
                state.failed,
            )?;
        }
        self.paint_import_footer(
            layers,
            x,
            bottom + self.ui_px(32.0),
            width,
            Some(SettingsAction::ImportStartOver),
            &[ImportButton {
                label: selected_source.text("session-import-open"),
                action: SettingsAction::SessionImportOpen,
                primary: true,
                enabled: !state.busy,
            }],
        )
    }

    /// One row per machine an import can run on. Choosing one detects the
    /// sessions there; the imported terminals stay on that machine.
    fn paint_import_hosts(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
        hosts: &[Option<String>],
        chosen: Option<&ImportTarget>,
        enabled: bool,
    ) -> anyhow::Result<f32> {
        let selected_source = self.ui.import_source;
        let palette = self.palette();
        let font = Rc::clone(&self.ui_font);
        let cell = self.metrics.cell_size.height as f32;
        let height = self.ui_px(CONTROL_HEIGHT);
        let pad = self.ui_px(14.0);
        let mut top = self.paint_import_copy(
            layers,
            x,
            y,
            width,
            &selected_source.text("session-import-hosts"),
            palette.text,
        )? + self.ui_px(8.0);
        for host in hosts {
            let action = SettingsAction::SessionImportHost(host_key(host.as_deref()));
            let selected = chosen.is_some_and(|target| target.host() == host.as_deref());
            if enabled {
                self.ui_context
                    .push(rect(x, top, width, height), WidgetKind::Button, action);
            }
            let bg = if selected {
                Some(palette.nav_selected_bg)
            } else if enabled && self.ui.interaction.pressed == Some(action) {
                Some(palette.control_pressed_bg)
            } else if enabled && self.ui.interaction.hovered == Some(action) {
                Some(palette.control_hover_bg)
            } else {
                None
            };
            if let Some(bg) = bg {
                self.draw_rounded_rect(layers, 0, x, top, width, height, bg, self.ui_px(9.0))?;
            }
            let (text, secondary) = if selected {
                (palette.selected_text, palette.selected_text)
            } else {
                (palette.text, palette.secondary_text)
            };
            let (icon, name, status) = match host {
                None => (
                    SvgIcon::Laptop,
                    selected_source.text("session-import-host-local"),
                    String::new(),
                ),
                Some(name) => (
                    SvgIcon::Server,
                    name.clone(),
                    selected_source.text(if host_connected(name) {
                        "session-import-host-connected"
                    } else {
                        "session-import-host-disconnected"
                    }),
                ),
            };
            self.draw_svg_icon(
                layers,
                icon,
                x + pad,
                top + (height - cell) / 2.0,
                cell,
                secondary,
            )?;
            let name_x = x + pad + cell + self.ui_px(12.0);
            let status_width = self.measure_text_width(&font, &status).min(width * 0.5);
            let status_x = x + width - pad - status_width;
            self.draw_text(
                layers,
                &font,
                name_x,
                self.control_text_y(top, height),
                &name,
                text,
                (status_x - name_x - self.ui_px(16.0)).max(1.0),
            )?;
            if !status.is_empty() {
                self.draw_text(
                    layers,
                    &font,
                    status_x,
                    self.control_text_y(top, height),
                    &status,
                    secondary,
                    status_width,
                )?;
            }
            top += height + self.ui_px(4.0);
        }
        Ok(top)
    }

    fn paint_session_notes(
        &self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
        notes: &[String],
    ) -> anyhow::Result<f32> {
        let selected_source = self.ui.import_source;
        let palette = self.palette();
        self.draw_rect(
            layers,
            0,
            x,
            y,
            width,
            self.ui_px(1.0),
            palette.separator.mul_alpha(0.7),
        )?;
        let mut top = y + self.ui_px(24.0);
        let icon = self.ui_px(20.0);
        let inset = self.ui_px(32.0);
        self.draw_svg_icon(
            layers,
            SvgIcon::Info,
            x,
            top + (self.import_line_step() - icon) / 2.0,
            icon,
            palette.muted_text,
        )?;
        let font = Rc::clone(&self.import_body_font);
        for line in self.wrap_settings_text(
            &font,
            &selected_source.text("session-import-notes-title"),
            width - inset,
        ) {
            self.draw_import_line(
                layers,
                &font,
                x + inset,
                top,
                width - inset,
                self.import_line_step(),
                &line,
                palette.text,
            )?;
            top += self.import_line_step();
        }
        top += self.ui_px(12.0);
        for note in notes {
            self.draw_rounded_rect(
                layers,
                0,
                x + self.ui_px(8.0),
                top + self.import_line_step() / 2.0,
                self.ui_px(4.0),
                self.ui_px(4.0),
                palette.muted_text,
                self.ui_px(2.0),
            )?;
            top = self.paint_import_copy(
                layers,
                x + inset,
                top,
                width - inset,
                note,
                palette.secondary_text,
            )? + self.ui_px(8.0);
        }
        Ok(top)
    }

    fn paint_session_counts(
        &self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
        counts: [usize; 3],
    ) -> anyhow::Result<f32> {
        let palette = self.palette();
        let gap = self.ui_px(12.0);
        let columns = if width >= self.ui_px(480.0) { 3 } else { 1 };
        let tile_width = (width - gap * (columns - 1) as f32) / columns as f32;
        let pad = self.ui_px(12.0);
        let labels = [
            "session-import-projects",
            "session-import-tabs",
            "session-import-terminals",
        ]
        .map(crate::i18n::tr);
        let font = Rc::clone(&self.ui_font);
        let lines: Vec<_> = counts
            .iter()
            .zip(labels.iter())
            .map(|(count, label)| {
                self.wrap_settings_text(&font, &format!("{count} {label}"), tile_width - pad * 2.0)
            })
            .collect();
        let height = pad * 2.0
            + self.import_line_step() * lines.iter().map(Vec::len).max().unwrap_or(1) as f32;
        for (index, lines) in lines.iter().enumerate() {
            let left = x + (index % columns) as f32 * (tile_width + gap);
            let top = y + (index / columns) as f32 * (height + gap);
            if columns > 1 && index > 0 {
                self.draw_rect(
                    layers,
                    0,
                    left - gap / 2.0,
                    top + pad,
                    self.ui_px(1.0),
                    height - pad * 2.0,
                    palette.separator,
                )?;
            }
            for (row, line) in lines.iter().enumerate() {
                self.draw_text(
                    layers,
                    &font,
                    left + pad,
                    top + pad + row as f32 * self.import_line_step(),
                    line,
                    palette.text,
                    tile_width - pad * 2.0,
                )?;
            }
        }
        let rows = 3 / columns;
        Ok(y + height * rows as f32 + gap * (rows - 1) as f32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failed_recovery_can_be_left_behind() {
        let mut state = ImportUi::default();
        state.pending = Some(codec::ImportSessionRequest {
            request_id: "1-example".into(),
            request: thinkterm_import::ImportRequest {
                source: "example".into(),
                session: "development".into(),
                mode: ImportMode::Layout,
                fingerprint: "example-fingerprint".into(),
                space_name: "Imported".into(),
            },
        });
        assert!(state.busy());
        state.failed = true;
        assert!(!state.busy());
        UNRESOLVED.with(|slot| *slot.borrow_mut() = Some(state.clone()));
        state.abandon();
        assert!(state.pending.is_none());
        assert!(ImportUi::default().pending.is_none());
    }

    #[test]
    fn import_follows_the_space_owner_and_never_falls_back_for_a_missing_space() {
        let store: crate::workspace_threads::WorkspaceThreadStore = serde_json::from_value(serde_json::json!({
            "spaces": [
                {"id": "local", "name": "Local", "active_project_id": null, "thread_refs": ["remote-thread"]},
                {"id": "remote", "name": "Remote", "active_project_id": null, "client_domain": "server-a"},
                {"id": "named-local", "name": "Remote", "active_project_id": null, "client_domain": "Local"}
            ], "projects": []
        })).unwrap();
        assert_eq!(origin_domain(&store, "local").unwrap(), None);
        assert_eq!(
            origin_domain(&store, "remote").unwrap().as_deref(),
            Some("server-a")
        );
        assert_eq!(
            origin_domain(&store, "named-local").unwrap().as_deref(),
            Some("Local")
        );
        assert!(origin_domain(&store, "missing").is_err());
    }

    #[test]
    fn this_machine_and_each_host_are_told_apart() {
        let keys = [
            host_key(None),
            host_key(Some("server-a")),
            host_key(Some("server-b")),
        ];
        assert_ne!(keys[0], keys[1]);
        assert_ne!(keys[1], keys[2]);
        assert_eq!(host_key(Some("server-a")), keys[1]);
        assert_eq!(ImportTarget::Local.host(), None);
        assert_eq!(
            ImportTarget::Remote {
                name: "server-a".into(),
                domain_id: 7,
            }
            .host(),
            Some("server-a")
        );
    }

    #[test]
    fn generic_errors_are_localized_and_unknown_errors_keep_details() {
        let source = ImportSource::Session("example");
        assert_eq!(
            localized_error(
                source,
                "Prepare import: An imported working directory is unavailable",
                "session-import-error-import"
            ),
            source.text("session-import-error-directory-missing")
        );
        let message = localized_error(
            source,
            "An unexpected import failure",
            "session-import-error-import",
        );
        assert!(message.contains("An unexpected import failure"));
        assert!(message.starts_with(&source.text("session-import-error-import")));
    }
}
