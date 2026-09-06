use crate::scripting::guiwin::GuiWin;
use crate::spawn::SpawnWhere;
use crate::termwindow::TermWindowNotif;
use crate::TermWindow;
use ::window::*;
use anyhow::{anyhow, Context, Error};
use config::keyassignment::{KeyAssignment, SpawnCommand, SpawnTabDomain};
use config::{ConfigSubscription, NotificationHandling};
use mux::client::ClientId;
use mux::domain::DomainId;
use mux::tab::TabId;
use mux::window::WindowId as MuxWindowId;
use mux::{Mux, MuxNotification};
use promise::{Future, Promise};
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;
use wezterm_client::domain::{ClientDomain, FrontendRecoverySlot, ThinkTermFrontendRecoveryTarget};
use wezterm_term::{Alert, ClipboardSelection};
use wezterm_toast_notification::*;

/// Which mux windows in the active workspace get a GUI window of their
/// own. Local windows always; a remote domain's only in its thread
/// workspaces (its plain windows are background). A window of the local
/// session host is the other way round: its thread windows are the
/// threads of the local Spaces, opened by their Space window through the
/// thread store (making one here would open every live thread of every
/// Space), and only a startup window in a plain workspace opens on its own.
/// `no_gui_window_yet`: this process shows nothing so far. A host window
/// outside a thread workspace is the startup tab, whose window restores
/// the thread; once one window is up, further ones there are leftovers
/// the server accumulated (a startup tab whose close never landed, a
/// `cli spawn --new-window`) and stay in the background.
fn should_spawn_reconciled_gui_window(
    is_domain_owned: bool,
    is_host: bool,
    workspace: &str,
    no_gui_window_yet: bool,
) -> bool {
    let thread_workspace = crate::workspace_threads::is_thread_workspace_name(workspace);
    if is_host {
        return !thread_workspace && no_gui_window_yet;
    }
    !is_domain_owned || thread_workspace
}

fn frontend_recovery_tab(
    current_generation: Option<u64>,
    requested_generation: u64,
    slot: FrontendRecoverySlot,
    recovery_targets: &[ThinkTermFrontendRecoveryTarget],
) -> Option<TabId> {
    if current_generation != Some(requested_generation) {
        return None;
    }
    recovery_targets
        .iter()
        .find(|target| target.slot == slot)
        .map(|target| target.tab_id)
}

/// Run exactly once after the client has installed a replacement mux
/// topology and armed its frontend recovery barrier.  The normal
/// TabAddedToWindow notification is too early to serve as that barrier's
/// trigger: it is emitted while the replacement is still being assembled.
pub fn request_thinkterm_frontend_recovery(
    domain_id: DomainId,
    connection_generation: u64,
    recovery_targets: Vec<ThinkTermFrontendRecoveryTarget>,
) {
    promise::spawn::spawn_into_main_thread(async move {
        let Some(frontend) = try_front_end() else {
            return;
        };

        // Address each retained native window explicitly. A missing window is
        // a failed recovery target, not a reason to leave the domain parked in
        // Syncing forever.
        for target in recovery_targets {
            // A mux WindowId is process-local and can change when the remote
            // server is replaced. Locate the retained native window through
            // its stable frontend slot, then rebind that window to the fresh
            // mux id below. Looking it up by target.window_id here caused a
            // failed recovery to abort and retry forever while allocating a
            // new mux window on every generation.
            let Some(gui_window) = frontend.gui_window_for_recovery_slot(target.slot) else {
                if let Some(domain) = Mux::get().get_domain(domain_id) {
                    if let Some(client) = domain.downcast_ref::<ClientDomain>() {
                        client.fail_frontend_recovery(
                            target.slot,
                            target.tab_id,
                            connection_generation,
                            format!(
                                "GUI frontend slot {:?} disappeared during mux recovery",
                                target.slot
                            ),
                        );
                    }
                }
                continue;
            };
            gui_window
                .window
                .notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                    let mux = Mux::get();
                    let current_generation = mux.get_domain(domain_id).and_then(|domain| {
                        domain
                            .downcast_ref::<ClientDomain>()
                            .and_then(ClientDomain::connection_generation)
                    });
                    let tab_id = frontend_recovery_tab(
                        current_generation,
                        connection_generation,
                        term_window.frontend_recovery_slot(),
                        &[target],
                    );
                    if let Some(tab_id) = tab_id {
                        let target_is_live = mux
                            .get_window(target.window_id)
                            .is_some_and(|window| window.idx_by_id(tab_id).is_some());
                        if !target_is_live {
                            if let Some(domain) = mux.get_domain(domain_id) {
                                if let Some(client) = domain.downcast_ref::<ClientDomain>() {
                                    client.fail_frontend_recovery(
                                        target.slot,
                                        tab_id,
                                        connection_generation,
                                        format!(
                                            "replacement mux window {} no longer contains tab {}",
                                            target.window_id, tab_id
                                        ),
                                    );
                                }
                            }
                            return;
                        }
                        if term_window.mux_window_id != target.window_id {
                            term_window.switch_to_mux_window(target.window_id);
                        }
                        if !term_window.sync_replacement_frontend_geometry(
                            tab_id,
                            target.slot,
                            connection_generation,
                        ) {
                            log::warn!(
                                "cannot restore GUI frontend geometry for mux window {} tab {}",
                                term_window.mux_window_id,
                                tab_id
                            );
                            if let Some(domain) = mux.get_domain(domain_id) {
                                if let Some(client) = domain.downcast_ref::<ClientDomain>() {
                                    client.fail_frontend_recovery(
                                        target.slot,
                                        tab_id,
                                        connection_generation,
                                        "GUI recovery target could not start geometry sync",
                                    );
                                }
                            }
                        }
                    } else if let Some(domain) = mux.get_domain(domain_id) {
                        if let Some(client) = domain.downcast_ref::<ClientDomain>() {
                            client.fail_frontend_recovery(
                                target.slot,
                                target.tab_id,
                                connection_generation,
                                "GUI recovery target no longer matches its native window",
                            );
                        }
                    }
                })));
        }
    })
    .detach();
}

pub struct GuiFrontEnd {
    connection: Rc<Connection>,
    // Depth counter: > 0 means at least one window is mid-switch (materializing
    // a workspace it is about to adopt). While non-zero the additive reconcile
    // is suppressed so it can't spawn a duplicate window for the mux window
    // being adopted. A counter (rather than a bool) lets concurrent switches /
    // Dock "New Window" clicks nest without one clobbering another's guard.
    switching_workspaces: RefCell<usize>,
    spawned_mux_window: RefCell<HashSet<MuxWindowId>>,
    known_windows: RefCell<BTreeMap<Window, MuxWindowId>>,
    /// Whether this process has shown a GUI window yet. AppKit asks a
    /// freshly launched app with no windows to "open an untitled file";
    /// before the first window exists that request is the launch itself,
    /// which startup already answers with a window of its own.
    any_window_recorded: Cell<bool>,
    /// Startup has run its course (the first window is up, or startup
    /// produced none): a new-window request is honoured from here on.
    startup_settled: Cell<bool>,
    /// A new-window request arrived during startup and was held; it is
    /// answered if startup ends with no window at all.
    new_window_request_held: Cell<bool>,
    /// Stable native-window lookup used across replacement mux runtimes.
    /// Unlike `known_windows`, this is keyed by the frontend-owned slot and
    /// therefore remains valid while remote WindowIds are being replaced.
    recovery_windows: RefCell<BTreeMap<FrontendRecoverySlot, Window>>,
    /// Set when the LAST GUI window closed and we detached whatever client
    /// domains were attached at that moment. A domain whose attach was still
    /// in flight shows as Detached then, escapes that sweep, and completes
    /// later — this flag lets the WindowCreated handler catch that late
    /// arrival and finish the job instead of leaving an invisible process.
    /// Cleared as soon as any GUI window exists again.
    detach_when_windowless: std::cell::Cell<bool>,
    client_id: Arc<ClientId>,
    config_subscription: RefCell<Option<ConfigSubscription>>,
}

impl Drop for GuiFrontEnd {
    fn drop(&mut self) {
        ::window::shutdown();
    }
}

impl GuiFrontEnd {
    pub fn try_new() -> anyhow::Result<Rc<GuiFrontEnd>> {
        let connection = Connection::init()?;
        connection.set_event_handler(Self::app_event_handler);
        crate::native_settings::apply_to_app(&crate::native_settings::load());
        spawn_malloc_pressure_relief_thread();

        let mux = Mux::get();
        // Agent detection schedules its safety tick and pane evaluations on
        // the GUI executor. Connection::init above installs that executor;
        // initializing it while the mux is first assembled is too early.
        mux::agent_status::initialize_mux(&mux);
        let client_id = mux.active_identity().expect("to have set my own id");

        let front_end = Rc::new(GuiFrontEnd {
            connection,
            switching_workspaces: RefCell::new(0),
            spawned_mux_window: RefCell::new(HashSet::new()),
            known_windows: RefCell::new(BTreeMap::new()),
            any_window_recorded: Cell::new(false),
            startup_settled: Cell::new(false),
            new_window_request_held: Cell::new(false),
            recovery_windows: RefCell::new(BTreeMap::new()),
            detach_when_windowless: std::cell::Cell::new(false),
            client_id: client_id.clone(),
            config_subscription: RefCell::new(None),
        });

        mux.subscribe(move |n| {
            match n {
                MuxNotification::WorkspaceRenamed {
                    old_workspace,
                    new_workspace,
                } => {
                    let mux = Mux::get();
                    let active = mux.active_workspace();
                    if active == old_workspace || active == new_workspace {
                        let switcher = WorkspaceSwitcher::new(&new_workspace);
                        promise::spawn::spawn_into_main_thread(async move {
                            drop(switcher);
                        })
                        .detach();
                    }
                }
                MuxNotification::WindowCreated(window_id) => {
                    promise::spawn::spawn_into_main_thread(async move {
                        let fe = crate::frontend::front_end();
                        if fe.reap_windowless_late_attach() {
                            return;
                        }
                        if fe.spawned_mux_window.borrow().contains(&window_id) {
                            return;
                        }
                        if !fe.is_switching_workspace() {
                            fe.reconcile_workspace();
                        }
                    })
                    .detach();
                }
                MuxNotification::WindowWorkspaceChanged(_)
                | MuxNotification::ActiveWorkspaceChanged(_)
                | MuxNotification::WindowRemoved(_) => {
                    promise::spawn::spawn_into_main_thread(async move {
                        let fe = crate::frontend::front_end();
                        if !fe.is_switching_workspace() {
                            fe.reconcile_workspace();
                        }
                    })
                    .detach();
                }
                MuxNotification::PaneFocused(pane_id) => {
                    promise::spawn::spawn_into_main_thread(async move {
                        let mux = Mux::get();
                        if mux.get_pane(pane_id).is_none() {
                            log::debug!(
                                "Ignoring stale PaneFocused notification for missing pane {pane_id}"
                            );
                            return;
                        }
                        if let Err(err) = mux.focus_pane_and_containing_tab(pane_id) {
                            log::error!("Error reconciling PaneFocused notification: {err:#}");
                        }
                    })
                    .detach();
                }
                MuxNotification::TabTitleChanged { .. } => {}
                MuxNotification::WindowTitleChanged { .. } => {}
                MuxNotification::TabResized(_) => {}
                // Server-side only: a GUI mux owns no ThinkTerm tree. The
                // remote tree arrives as a pushed PDU, not as a notification.
                MuxNotification::ThinkTermTreeChanged => {}
                MuxNotification::ThinkTermSessionChanged => {}
                MuxNotification::FrontendLeaseChanged(_) => {}
                MuxNotification::FrontendAccessChanged(_) => {}
                MuxNotification::TabAddedToWindow { .. } => {}
                MuxNotification::PaneRemoved(_) => {}
                MuxNotification::WindowInvalidated(_) => {}
                MuxNotification::PaneOutput(_) => {}
                MuxNotification::AgentStatusChanged(_) => {}
                MuxNotification::PaneAdded(_) => {}
                MuxNotification::Alert {
                    pane_id,
                    alert:
                        Alert::ToastNotification {
                            title,
                            body,
                            focus: _,
                        },
                } => {
                    let mux = Mux::get();

                    if let Some((_domain, window_id, tab_id)) = mux.resolve_pane_id(pane_id) {
                        let config = config::configuration();

                        if let Some((_fdomain, f_window, f_tab, f_pane)) =
                            mux.resolve_focused_pane(&client_id)
                        {
                            let show = match config.notification_handling {
                                NotificationHandling::NeverShow => false,
                                NotificationHandling::AlwaysShow => true,
                                NotificationHandling::SuppressFromFocusedPane => f_pane != pane_id,
                                NotificationHandling::SuppressFromFocusedTab => f_tab != tab_id,
                                NotificationHandling::SuppressFromFocusedWindow => {
                                    f_window != window_id
                                }
                            };

                            if show {
                                let message = if title.is_none() { "" } else { &body };
                                let title = title.as_ref().unwrap_or(&body);
                                // FIXME: if notification.focus is true, we should do
                                // something here to arrange to focus pane_id when the
                                // notification is clicked
                                persistent_toast_notification(title, message);
                            }
                        }
                    }
                }
                MuxNotification::Alert {
                    pane_id: _,
                    alert: Alert::Bell | Alert::Progress(_),
                } => {
                    // Handled via TermWindowNotif; NOP it here.
                }
                MuxNotification::Alert {
                    pane_id: _,
                    alert:
                        Alert::OutputSinceFocusLost
                        | Alert::PaletteChanged
                        | Alert::CurrentWorkingDirectoryChanged
                        | Alert::WindowTitleChanged(_)
                        | Alert::TabTitleChanged(_)
                        | Alert::IconTitleChanged(_)
                        | Alert::SetUserVar { .. },
                } => {}
                MuxNotification::Empty => {
                    if config::configuration().quit_when_all_windows_are_closed {
                        promise::spawn::spawn_into_main_thread(async move {
                            if mux::activity::Activity::count() == 0 {
                                log::trace!("Mux is now empty, terminate gui");
                                Connection::get().unwrap().terminate_message_loop();
                            }
                        })
                        .detach();
                    }
                }
                MuxNotification::SaveToDownloads { name, data } => {
                    if !config::configuration().allow_download_protocols {
                        log::error!(
                            "Ignoring download request for {:?}, \
                                 as allow_download_protocols=false",
                            name
                        );
                    } else if let Err(err) = crate::download::save_to_downloads(name, &*data) {
                        log::error!("save_to_downloads: {:#}", err);
                    }
                }
                MuxNotification::AssignClipboard {
                    pane_id,
                    selection,
                    clipboard,
                } => {
                    promise::spawn::spawn_into_main_thread(async move {
                        let fe = crate::frontend::front_end();
                        log::trace!(
                            "set clipboard in pane {} {:?} {:?}",
                            pane_id,
                            selection,
                            clipboard
                        );
                        if let Some(window) = fe.known_windows.borrow().keys().next() {
                            window.set_clipboard(
                                match selection {
                                    ClipboardSelection::Clipboard => Clipboard::Clipboard,
                                    ClipboardSelection::PrimarySelection => {
                                        Clipboard::PrimarySelection
                                    }
                                },
                                clipboard.unwrap_or_else(String::new),
                            );
                        } else {
                            log::error!("Cannot assign clipboard as there are no windows");
                        };
                    })
                    .detach();
                }
            }
            true
        });
        // Re-evaluate the config so that folks that are using
        // `wezterm.gui.get_appearance()` can have that take effect
        // before any windows are created
        config::reload();

        // And build the initial menu bar.
        // TODO: arrange for this to happen on config reload.
        crate::commands::CommandDef::recreate_menubar(&config::configuration());

        Ok(front_end)
    }

    fn app_event_handler(event: ApplicationEvent) {
        log::trace!("Got app event {event:?}");
        match event {
            ApplicationEvent::OpenCommandScript(file_name) => {
                let quoted_file_name = match shlex::try_quote(&file_name) {
                    Ok(name) => name.to_owned().to_string(),
                    Err(_) => {
                        log::error!(
                            "OpenCommandScript: {file_name} has embedded NUL bytes and
                             cannot be launched via the shell"
                        );
                        return;
                    }
                };
                promise::spawn::spawn(async move {
                    use config::keyassignment::SpawnTabDomain;
                    use wezterm_term::TerminalSize;

                    // We send the script to execute to the shell on stdin, rather than ask the
                    // shell to execute it directly, so that we start the shell and read in the
                    // user's rc files before running the script.  Without this, wezterm on macOS
                    // is launched with a default and very anemic path, and that is frustrating for
                    // users.

                    let mux = Mux::get();
                    let window_id = None;
                    let pane_id = None;
                    let cmd = None;
                    let cwd = None;
                    let workspace = mux.active_workspace();

                    match mux
                        .spawn_tab_or_window(
                            window_id,
                            crate::local_sessions::local_spawn_domain(),
                            cmd,
                            cwd,
                            TerminalSize::default(),
                            pane_id,
                            workspace,
                            None, // optional position
                        )
                        .await
                    {
                        Ok((_tab, pane, _window_id)) => {
                            log::trace!("Spawned {file_name} as pane_id {}", pane.pane_id());
                            let mut writer = pane.writer();
                            write!(writer, "{quoted_file_name} ; exit\n").ok();
                        }
                        Err(err) => {
                            log::error!("Failed to spawn {file_name}: {err:#?}");
                        }
                    };
                })
                .detach();
            }
            ApplicationEvent::PerformKeyAssignment(action) => {
                // We should only get here when there are no windows open
                // and the user picks an action from the menubar.
                // This is not currently possible, but could be in the
                // future.

                fn spawn_command(spawn: &SpawnCommand, spawn_where: SpawnWhere) {
                    let config = config::configuration();
                    let dpi = config.dpi.unwrap_or_else(|| ::window::default_dpi());
                    let size =
                        config.initial_size(dpi as u32, crate::cell_pixel_dims(&config, dpi).ok());
                    let term_config = Arc::new(config::TermConfig::with_config(config));

                    crate::spawn::spawn_command_impl(
                        spawn,
                        spawn_where,
                        size,
                        None,
                        term_config,
                        None,
                        None,
                        None,
                    )
                }

                match action {
                    KeyAssignment::QuitApplication => {
                        // If we get here, there are no windows that could have received
                        // the QuitApplication command, therefore it must be ok to quit
                        // immediately
                        Connection::get().unwrap().terminate_message_loop();
                    }
                    KeyAssignment::SpawnWindow => {
                        // macOS sends this at launch for an app that has no
                        // window yet. In process the startup window is there
                        // before the request arrives; with the local session
                        // host it comes after a round trip, and honouring the
                        // request opened a second Space window beside it.
                        let front_end = front_end();
                        if !front_end.any_window_recorded.get() && !front_end.startup_settled.get()
                        {
                            log::debug!(
                                "holding the launch-time new-window request: startup opens the first window"
                            );
                            front_end.new_window_request_held.set(true);
                            return;
                        }
                        front_end.spawn_space_window();
                    }
                    KeyAssignment::SpawnTab(spawn_where) => {
                        spawn_command(
                            &SpawnCommand {
                                domain: spawn_where,
                                ..Default::default()
                            },
                            SpawnWhere::NewWindow,
                        );
                    }
                    KeyAssignment::SpawnCommandInNewTab(spawn) => {
                        spawn_command(&spawn, SpawnWhere::NewTab);
                    }
                    KeyAssignment::SpawnCommandInNewWindow(spawn) => {
                        spawn_command(&spawn, SpawnWhere::NewWindow);
                    }
                    _ => {
                        log::warn!("unhandled perform: {action:?}");
                    }
                }
            }
        }
    }

    pub fn run_forever(&self) -> anyhow::Result<()> {
        self.connection
            .run_message_loop()
            .context("running message loop")
    }

    pub fn gui_windows(&self) -> Vec<GuiWin> {
        let windows = self.known_windows.borrow();
        let mut windows: Vec<GuiWin> = windows
            .iter()
            .map(|(window, &mux_window_id)| GuiWin {
                mux_window_id,
                window: window.clone(),
            })
            .collect();
        windows.sort_by(|a, b| a.window.cmp(&b.window));
        windows
    }

    pub fn reconcile_workspace(&self) -> Future<()> {
        let mut promise = Promise::new();
        let mux = Mux::get();

        // Each GUI window is pinned to its own mux window (its Space). We do
        // NOT force every window onto a single active workspace any more, since
        // ThinkTerm intentionally lets multiple windows live in different
        // Spaces (workspaces) at the same time. Reconcile is therefore additive
        // and only does two non-destructive things:
        //   1. Close GUI windows whose mux window has gone away (e.g. the last
        //      pane in that window exited and the mux killed the window).
        //   2. Create GUI windows for mux windows in the active workspace that
        //      don't have a GUI window yet (startup's first window, and the
        //      native SpawnWindow path that adds a mux window to the current
        //      workspace). Windows pinned to live mux windows in *other*
        //      workspaces are never touched.

        // 1. Handle GUI windows whose mux window no longer exists: give the
        //    TermWindow a chance to fall back to another thread of its Space
        //    first. Closing it outright meant an `exit` in a thread's only
        //    pane took the whole window down — and the app with it, when it
        //    was the last window. The TermWindow either adopts a new mux
        //    window (updating known_windows via rebind) or closes itself.
        let known_windows = std::mem::take(&mut *self.known_windows.borrow_mut());
        let mut windows = BTreeMap::new();
        for (window, window_id) in known_windows.into_iter() {
            if mux.get_window(window_id).is_some() {
                windows.insert(window, window_id);
            } else {
                self.spawned_mux_window.borrow_mut().remove(&window_id);
                window.notify(TermWindowNotif::Apply(Box::new(|term_window| {
                    term_window.recover_from_dead_mux_window();
                })));
                windows.insert(window, window_id);
            }
        }
        *self.known_windows.borrow_mut() = windows;

        // 2. Spawn GUI windows for active-workspace mux windows that lack one.
        let workspace = mux.active_workspace_for_client(&self.client_id);
        let mux_windows = mux.iter_windows_in_workspace(&workspace);
        log::debug!(
            "reconcile: active_ws={} mux_in_active={:?} known={:?} spawned={:?}",
            workspace,
            mux_windows,
            self.known_windows
                .borrow()
                .values()
                .copied()
                .collect::<Vec<_>>(),
            self.spawned_mux_window
                .borrow()
                .iter()
                .copied()
                .collect::<Vec<_>>(),
        );

        let future = promise.get_future().unwrap();
        promise::spawn::spawn(async move {
            for mux_window_id in mux_windows {
                if front_end().has_mux_window(mux_window_id)
                    || front_end()
                        .spawned_mux_window
                        .borrow()
                        .contains(&mux_window_id)
                {
                    continue;
                }

                let mux = Mux::get();
                let (is_domain_owned, is_host, window_workspace) = {
                    let Some(mux_window) = mux.get_window(mux_window_id) else {
                        continue;
                    };
                    // A window of the local session host is a local window:
                    // shown like one, restored like one.
                    (
                        mux_window
                            .origin_domain()
                            .is_some_and(|id| !crate::local_sessions::is_host_domain_id(id)),
                        mux_window
                            .origin_domain()
                            .is_some_and(crate::local_sessions::is_host_domain_id),
                        mux_window.get_workspace().to_string(),
                    )
                };
                // `get_window` holds a read guard for the mux's complete
                // window map. Drop it before awaiting native window creation:
                // startup content views register a UI surface, which needs the
                // write side of the same lock.

                // Client domains mirror every remote mux window locally when
                // they attach. That includes the server's own startup
                // `default` window and other background workspaces that do not
                // belong to a ThinkTerm Space/thread. They must stay available
                // in the mux for protocol bookkeeping, but automatically
                // giving them a GUI window creates the stray "local Space with
                // a remote terminal" window and tangles its lifecycle with the
                // real thread window.
                let no_gui_window_yet = front_end().known_windows.borrow().is_empty()
                    && front_end().spawned_mux_window.borrow().is_empty();
                if !should_spawn_reconciled_gui_window(
                    is_domain_owned,
                    is_host,
                    &window_workspace,
                    no_gui_window_yet,
                ) {
                    log::debug!(
                        "reconcile: leaving background domain window {} in workspace {:?} hidden",
                        mux_window_id,
                        window_workspace,
                    );
                    continue;
                }

                front_end()
                    .spawned_mux_window
                    .borrow_mut()
                    .insert(mux_window_id);
                log::trace!("Creating TermWindow for mux_window_id={}", mux_window_id);
                // Domain-owned windows (remote mux windows, tmux) must not be
                // adopted into a saved workspace thread: the restore would
                // re-point the GUI window at a different mux window and orphan
                // the one the domain just created (killing e.g. the in-window
                // ConnectionUI mid-authentication).
                let created = if is_domain_owned {
                    TermWindow::new_window_without_restore(mux_window_id).await
                } else {
                    TermWindow::new_window(mux_window_id).await
                };
                if let Err(err) = created {
                    log::error!("Failed to create window: {:#}", err);
                    let mux = Mux::get();
                    mux.kill_window(mux_window_id);
                    front_end()
                        .spawned_mux_window
                        .borrow_mut()
                        .remove(&mux_window_id);
                }
            }
            // Note: reconcile does not touch the switch-depth counter; only the
            // paired switch operations (which increment on entry) decrement it.
            promise.ok(());
        })
        .detach();
        future
    }

    /// Startup's spawn has returned; its window, if any, follows within
    /// moments. Once those have passed, a new-window request held during
    /// startup is answered if startup showed nothing, and later requests
    /// are answered as they come.
    pub(crate) fn startup_settled_soon(&self) {
        promise::spawn::spawn(async move {
            smol::Timer::after(std::time::Duration::from_secs(2)).await;
            let front_end = front_end();
            front_end.startup_settled.set(true);
            if front_end.new_window_request_held.replace(false)
                && !front_end.any_window_recorded.get()
            {
                log::info!("startup opened no window; answering the held new-window request");
                front_end.spawn_space_window();
            }
        })
        .detach();
    }

    pub(crate) fn spawn_space_window(&self) {
        // Each Dock "New Window" gets its own Space and its own GUI window, and
        // must work regardless of whatever else is in flight. We claim a
        // distinct (unoccupied) Space synchronously so rapid repeated clicks
        // each land on a different Space, then suppress reconcile only while we
        // materialize + create this window.
        let space_owner_id = crate::workspace_threads::next_space_owner_id();
        let active_space_id = crate::workspace_threads::claim_space_for_new_window(space_owner_id);
        self.set_switching_workspaces(true);

        promise::spawn::spawn(async move {
            let result = async {
                let (mux_window_id, created_mux_window) =
                    Self::materialize_space_window_for_app(&active_space_id).await?;
                front_end()
                    .spawned_mux_window
                    .borrow_mut()
                    .insert(mux_window_id);
                if let Err(err) = TermWindow::new_window_with_claimed_space(
                    mux_window_id,
                    space_owner_id,
                    active_space_id.clone(),
                )
                .await
                {
                    if created_mux_window {
                        Mux::get().kill_window(mux_window_id);
                    }
                    return Err(err);
                }
                Ok(())
            }
            .await;

            if let Err(err) = result {
                crate::workspace_threads::release_window_space(space_owner_id);
                log::error!("failed to create ThinkTerm Space window: {err:#}");
            }
            front_end().set_switching_workspaces(false);
        })
        .detach();
    }

    async fn materialize_space_window_for_app(
        space_id: &str,
    ) -> anyhow::Result<(MuxWindowId, bool)> {
        let mux = Mux::get();
        let thread_id = crate::workspace_threads::ensure_active_thread_for_space(space_id)
            .ok_or_else(|| anyhow!("failed to ensure active thread for Space {space_id}"))?;
        let live_workspaces = mux.iter_workspaces();
        let plan =
            crate::workspace_threads::activate_thread_record(&thread_id, &live_workspaces, true)
                .ok_or_else(|| {
                    anyhow!("failed to activate thread {thread_id} for Space {space_id}")
                })?;
        let created_mux_window = plan.needs_materialize;

        if plan.needs_materialize {
            let config = config::configuration();
            let dpi = config.dpi.unwrap_or_else(|| ::window::default_dpi());
            let size = config.initial_size(dpi as u32, crate::cell_pixel_dims(&config, dpi).ok());
            let term_config: Arc<dyn wezterm_term::TerminalConfiguration> =
                Arc::new(config::TermConfig::with_config(config.clone()));
            let layout = crate::workspace_threads::thread_layout(&plan.thread_id);
            let remote_spec = crate::ssh_hosts::host_spec(
                crate::workspace_threads::remote_host_id_for_project_id(&plan.project_id),
            );
            let (initial_cwd, default_domain) = if let Some(spec) = remote_spec.as_ref() {
                let domain_name = crate::ssh_hosts::ensure_ssh_domain_registered(spec)?;
                (None, SpawnTabDomain::DomainName(domain_name))
            } else {
                (
                    plan.project_path.to_str().map(|path| path.to_string()),
                    SpawnTabDomain::DefaultDomain,
                )
            };

            // Strict first, so a refused Project directory does not quietly
            // become `$HOME`. If it is refused, park the problem for the window
            // that is about to be built and materialize leniently anyway: this
            // call is what *creates* that window, so failing here would open no
            // window at all -- a worse outcome than the wrong directory, and
            // one the user could not even see to fix.
            if let Err(err) = crate::workspace_threads::materialize_thread(
                plan.workspace_name.clone(),
                layout.clone(),
                initial_cwd.clone(),
                size,
                None,
                term_config.clone(),
                default_domain.clone(),
                true,
            )
            .await
            {
                let Some(failure) =
                    crate::termwindow::ui::folder_problem::ProjectRootUnavailable::from_error_chain(
                        &err,
                    )
                else {
                    return Err(err);
                };
                log::warn!("Space {space_id} starts in the home directory: {failure}");
                // The failed strict attempt can leave a tab-less mux window
                // behind; the lenient retry would then report AlreadyLive and
                // hand back a window with no tabs at all.
                let mux = Mux::get();
                for window_id in mux.iter_windows_in_workspace(&plan.workspace_name) {
                    if mux
                        .get_window(window_id)
                        .is_some_and(|window| window.is_empty())
                    {
                        mux.kill_window(window_id);
                    }
                }
                crate::termwindow::project_root_view::queue_project_root_problem(
                    space_id.to_string(),
                    plan.thread_id.clone(),
                    crate::workspace_threads::thread_display_name(
                        &plan.project_id,
                        &plan.thread_id,
                    ),
                    failure,
                );
                crate::workspace_threads::materialize_thread(
                    plan.workspace_name.clone(),
                    layout,
                    initial_cwd,
                    size,
                    None,
                    term_config,
                    default_domain,
                    false,
                )
                .await?;
                // What just materialized is a shell in `$HOME`, not the Thread
                // the user asked for. Mark it so the next deliberate open adds
                // the Project terminal instead of handing this one back.
                crate::workspace_threads::mark_denied_project_fallback(plan.workspace_name.clone());
            }
        }

        crate::workspace_threads::window_to_show_in_workspace(&plan.workspace_name)
            .map(|window_id| (window_id, created_mux_window))
            .ok_or_else(|| anyhow!("Space workspace {} has no mux window", plan.workspace_name))
    }

    pub fn has_mux_window(&self, mux_window_id: MuxWindowId) -> bool {
        for &mux_id in self.known_windows.borrow().values() {
            if mux_id == mux_window_id {
                return true;
            }
        }
        false
    }

    /// Pre-claim a mux window that the caller is about to create a GUI window
    /// for explicitly (e.g. the `thinkterm connect` flow), so that neither the
    /// WindowCreated handler nor the additive reconcile spawns a duplicate.
    pub fn claim_spawned_mux_window(&self, mux_window_id: MuxWindowId) {
        self.spawned_mux_window.borrow_mut().insert(mux_window_id);
    }

    pub fn switch_workspace(&self, workspace: &str) {
        let mux = Mux::get();
        mux.set_active_workspace_for_client(&self.client_id, workspace);
        self.set_switching_workspaces(false);
        self.reconcile_workspace();
    }

    pub fn record_known_window(
        &self,
        window: Window,
        mux_window_id: MuxWindowId,
        recovery_slot: FrontendRecoverySlot,
    ) {
        // A GUI window exists again: any pending last-window cleanup is moot.
        self.detach_when_windowless.set(false);
        self.any_window_recorded.set(true);
        // Mark this mux window as having a GUI window so the additive reconcile
        // never re-creates a window for it (e.g. after the user closes it while
        // its mux window keeps running in the background).
        self.spawned_mux_window.borrow_mut().insert(mux_window_id);
        self.recovery_windows
            .borrow_mut()
            .insert(recovery_slot, window.clone());
        self.known_windows
            .borrow_mut()
            .insert(window, mux_window_id);
        if !self.is_switching_workspace() {
            self.reconcile_workspace();
        }
    }

    pub fn forget_known_window(&self, window: &Window) {
        self.recovery_windows
            .borrow_mut()
            .retain(|_, candidate| candidate != window);
        let now_empty = {
            let mut windows = self.known_windows.borrow_mut();
            windows.remove(window);
            windows.is_empty()
        };
        // The last GUI window closing is the user's "I'm done": detach any
        // client domains so their panes drop, the mux empties, and the Empty
        // notification can actually terminate the process.
        // Mux::remove_window_internal deliberately no longer detaches while
        // other windows still reference a domain (deleting one thread must
        // not sever the connection its siblings are using), so without this
        // the background workspaces' panes would keep an invisible process
        // alive after the last window is gone. Gated on the same setting
        // that governs quitting, so a platform that idles without windows
        // (macOS) also keeps its connections.
        if now_empty && config::configuration().quit_when_all_windows_are_closed {
            self.detach_attached_client_domains();
            // A domain whose attach is still in flight shows as Detached and
            // escapes the sweep above; remember to finish the job when its
            // windows materialize (see the WindowCreated handler).
            self.detach_when_windowless.set(true);
        }
        if !self.is_switching_workspace() {
            self.reconcile_workspace();
        }
    }

    fn detach_attached_client_domains(&self) {
        let mux = Mux::get();
        for domain in mux.iter_domains() {
            if domain.state() != mux::domain::DomainState::Attached {
                continue;
            }
            // The local session host is not detachable by the mux or by an
            // action, but the last window closing is this connection's
            // end: its mirrors must go so the mux empties and the process
            // exits (its terminals live on in the server).
            if let Some(client) = domain.downcast_ref::<ClientDomain>() {
                if client.is_local_session_host() {
                    client.perform_detach();
                    continue;
                }
            }
            if domain.detachable() {
                if let Err(err) = domain.detach() {
                    log::error!(
                        "while detaching domain {} after the last window closed: {err:#}",
                        domain.domain_name()
                    );
                }
            }
        }
    }

    /// A mux window appeared while no GUI window exists and the last-window
    /// cleanup already ran: this is a connection that was still attaching
    /// when the user closed everything. Finish what forget_known_window
    /// started rather than resurrecting an unwanted session. Returns true
    /// when the event was consumed this way.
    pub fn reap_windowless_late_attach(&self) -> bool {
        if !self.detach_when_windowless.get() {
            return false;
        }
        if !self.known_windows.borrow().is_empty() {
            self.detach_when_windowless.set(false);
            return false;
        }
        self.detach_attached_client_domains();
        true
    }

    /// Re-point an existing GUI window at a different mux window. Used when a
    /// window switches the Space it is showing in place, so that
    /// `known_windows` stays accurate without relying on reconcile to rebuild
    /// the whole mux <-> gui mapping.
    pub fn rebind_known_window(&self, window: &Window, mux_window_id: MuxWindowId) {
        self.known_windows
            .borrow_mut()
            .insert(window.clone(), mux_window_id);
        // Mark the adopted mux window as spawned so reconcile won't create a
        // duplicate for it. We deliberately do NOT un-mark the window's
        // *previous* mux window: after an in-window Space switch it keeps
        // running in the background, and must stay protected so the additive
        // reconcile never resurrects it as a stray "different" window (e.g.
        // after another window is closed). A mux window only leaves the set
        // when it actually dies (cleanup in `reconcile_workspace`).
        self.spawned_mux_window.borrow_mut().insert(mux_window_id);
    }

    pub fn invalidate_all_windows(&self) {
        for window in self.known_windows.borrow().keys() {
            window.invalidate();
        }
    }

    pub fn is_switching_workspace(&self) -> bool {
        *self.switching_workspaces.borrow() > 0
    }

    /// Enter (true) or leave (false) a workspace-switch critical section,
    /// during which the additive reconcile is suppressed so it doesn't spawn a
    /// duplicate window for the mux window being adopted. Calls must be paired;
    /// the underlying depth counter lets concurrent switches nest safely.
    pub fn set_switching_workspaces(&self, value: bool) {
        let mut depth = self.switching_workspaces.borrow_mut();
        if value {
            *depth += 1;
        } else {
            *depth = depth.saturating_sub(1);
        }
    }

    pub fn gui_window_for_mux_window(&self, mux_window_id: MuxWindowId) -> Option<GuiWin> {
        let windows = self.known_windows.borrow();
        for (window, v) in windows.iter() {
            if *v == mux_window_id {
                return Some(GuiWin {
                    mux_window_id,
                    window: window.clone(),
                });
            }
        }
        None
    }

    pub fn gui_window_for_recovery_slot(&self, slot: FrontendRecoverySlot) -> Option<GuiWin> {
        let window = self.recovery_windows.borrow().get(&slot)?.clone();
        let mux_window_id = self.known_windows.borrow().get(&window).copied()?;
        Some(GuiWin {
            mux_window_id,
            window,
        })
    }
}

thread_local! {
    static FRONT_END: RefCell<Option<Rc<GuiFrontEnd>>> = RefCell::new(None);
}

pub fn try_front_end() -> Option<Rc<GuiFrontEnd>> {
    FRONT_END.with(|f| f.borrow().as_ref().map(Rc::clone))
}

pub fn front_end() -> Rc<GuiFrontEnd> {
    FRONT_END
        .with(|f| f.borrow().as_ref().map(Rc::clone))
        .expect("to be called on gui thread")
}

pub struct WorkspaceSwitcher {
    new_name: String,
}

impl WorkspaceSwitcher {
    pub fn new(new_name: &str) -> Self {
        front_end().set_switching_workspaces(true);
        Self {
            new_name: new_name.to_string(),
        }
    }
}

impl Drop for WorkspaceSwitcher {
    fn drop(&mut self) {
        front_end().switch_workspace(&self.new_name);
    }
}

pub fn shutdown() {
    FRONT_END.with(|f| drop(f.borrow_mut().take()));
}

/// Hand the allocator's empty-but-dirty pages back to the kernel on a slow
/// cadence. Rendering churns multi-megabyte buffers (vertex staging growth,
/// card extraction), and macOS malloc holds the freed pages dirty instead of
/// returning them -- measured at 300MB of phys footprint that no live object
/// accounts for. Runs on its own thread so the walk over the zones (a few ms
/// on a large heap) never lands inside a frame.
#[cfg(target_os = "macos")]
fn spawn_malloc_pressure_relief_thread() {
    extern "C" {
        fn malloc_zone_pressure_relief(zone: *mut std::ffi::c_void, goal: usize) -> usize;
    }
    std::thread::Builder::new()
        .name("malloc-relief".to_string())
        .spawn(|| loop {
            std::thread::sleep(std::time::Duration::from_secs(30));
            let freed = unsafe { malloc_zone_pressure_relief(std::ptr::null_mut(), 0) };
            if freed > 0 {
                log::debug!("malloc_zone_pressure_relief returned {freed} bytes to the kernel");
            }
        })
        .expect("spawn malloc-relief thread");
}

#[cfg(not(target_os = "macos"))]
fn spawn_malloc_pressure_relief_thread() {}

pub fn try_new() -> Result<Rc<GuiFrontEnd>, Error> {
    let front_end = GuiFrontEnd::try_new()?;
    FRONT_END.with(|f| *f.borrow_mut() = Some(Rc::clone(&front_end)));

    let config_subscription = config::subscribe_to_config_reload({
        move || {
            promise::spawn::spawn_into_main_thread(async {
                crate::commands::CommandDef::recreate_menubar(&config::configuration());
            })
            .detach();
            true
        }
    });
    front_end
        .config_subscription
        .borrow_mut()
        .replace(config_subscription);

    Ok(front_end)
}

#[cfg(test)]
mod tests {
    use super::{frontend_recovery_tab, should_spawn_reconciled_gui_window};
    use wezterm_client::domain::{FrontendRecoverySlot, ThinkTermFrontendRecoveryTarget};

    #[test]
    fn reconcile_keeps_local_windows_visible() {
        assert!(should_spawn_reconciled_gui_window(false, false, "default", true));
        assert!(should_spawn_reconciled_gui_window(false, false, "user-created-workspace", true));
    }

    #[test]
    fn reconcile_hides_background_domain_windows() {
        assert!(!should_spawn_reconciled_gui_window(true, false, "default", true));
        assert!(!should_spawn_reconciled_gui_window(true, false, "unmanaged-remote-workspace", true));
        assert!(!should_spawn_reconciled_gui_window(true, false, "thinkterm:not-a-thread-workspace", true));
    }

    #[test]
    fn reconcile_shows_thinkterm_domain_thread_windows() {
        assert!(should_spawn_reconciled_gui_window(true, false, "thinkterm:muxdomain-host::space::space-1:thread-2", true));
        assert!(should_spawn_reconciled_gui_window(true, false, "thinkterm:muxdomain-host::space::space-1:thread-2:remote-default", true));
    }

    #[test]
    fn reconcile_opens_only_the_hosts_startup_window() {
        // The host's thread windows belong to the local Spaces and are
        // opened through the thread store, never one per live thread.
        assert!(should_spawn_reconciled_gui_window(false, true, "default", true));
        // A second host window in the startup workspace stays hidden once
        // a window is up: it is a leftover, not a window to show.
        assert!(!should_spawn_reconciled_gui_window(false, true, "default", false));
        assert!(!should_spawn_reconciled_gui_window(false, true, "thinkterm:project-1:thread-2", true));
    }

    #[test]
    fn replacement_recovery_targets_the_stable_slot_in_the_current_generation() {
        let recovery_targets = [
            ThinkTermFrontendRecoveryTarget {
                slot: FrontendRecoverySlot::Window(70),
                window_id: 7,
                tab_id: 41,
            },
            ThinkTermFrontendRecoveryTarget {
                slot: FrontendRecoverySlot::Window(80),
                window_id: 8,
                tab_id: 42,
            },
        ];
        assert_eq!(
            frontend_recovery_tab(
                Some(7),
                7,
                FrontendRecoverySlot::Window(80),
                &recovery_targets,
            ),
            Some(42)
        );
        assert_eq!(
            frontend_recovery_tab(
                Some(8),
                7,
                FrontendRecoverySlot::Window(80),
                &recovery_targets,
            ),
            None
        );
        assert_eq!(
            frontend_recovery_tab(None, 7, FrontendRecoverySlot::Window(80), &recovery_targets,),
            None
        );
        assert_eq!(
            frontend_recovery_tab(
                Some(7),
                7,
                FrontendRecoverySlot::Window(70),
                &recovery_targets,
            ),
            Some(41)
        );
    }
}
