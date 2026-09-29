// Don't create a new standard console window when launched from the windows GUI.
#![cfg_attr(not(test), windows_subsystem = "windows")]

use crate::customglyph::BlockKey;
use crate::glyphcache::GlyphCache;
use crate::utilsprites::RenderMetrics;
use ::window::*;
use anyhow::{anyhow, Context};
use clap::builder::ValueParser;
use clap::{Parser, ValueHint};
use config::keyassignment::{SpawnCommand, SpawnTabDomain};
use config::{ConfigHandle, SerialDomain, SshDomain, SshMultiplexing};
use mux::activity::Activity;
use mux::domain::{Domain, LocalDomain};
use mux::Mux;
use mux_lua::MuxDomain;
use portable_pty::cmdbuilder::CommandBuilder;
use promise::spawn::block_on;
use std::borrow::Cow;
use std::collections::HashMap;
use std::env::current_dir;
use std::ffi::OsString;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use termwiz::cell::CellAttributes;
use termwiz::surface::{Line, SEQ_ZERO};
use unicode_normalization::UnicodeNormalization;
use wezterm_bidi::Direction;
pub(crate) use wezterm_client::domain::AttachRetryOutcome;
use wezterm_client::domain::{is_fatal_attach_error, ClientDomain, ClientDomainConfig};
use wezterm_font::shaper::PresentationWidth;
use wezterm_font::FontConfiguration;
use wezterm_gui_subcommands::*;
use wezterm_mux_server_impl::update_mux_domains;
use wezterm_toast_notification::*;

mod agent_status;
mod bottom_quotes;
mod bounded_file;
mod colorease;
mod commands;
mod customglyph;
mod download;
mod framedump;
mod frontend;
mod glyphcache;
mod i18n;
mod input_diagnostics;
mod inputmap;
mod main_window_placement;
mod markdown_editor;
mod local_sessions;
mod native_paths;
mod native_settings;
mod overlay;
mod perf;
mod plugins;
mod quad;
mod renderstate;
mod resize_increment_calculator;
mod scripting;
mod scrollbar;
mod secret;
mod selection;
mod settings_window;
mod web_settings;
mod shapecache;
mod shell_catalog;
mod snippets;
mod spawn;
mod ssh_hosts;
mod state_backup;
mod stats;
mod tabbar;
mod termwindow;
mod ui;
mod unicode_names;
mod uniforms;
mod update;
mod utilsprites;
mod workspace_threads;

#[cfg(feature = "dhat-heap")]
#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

pub use selection::SelectionMode;
pub use termwindow::{set_window_class, set_window_position, TermWindow, ICON_DATA};

#[derive(Debug, Parser)]
#[command(
    name = "thinkterm-gui",
    about = "ThinkTerm - a workspace-first terminal\nhttps://github.com/RoversX/thinkterm",
    version = config::wezterm_version()
)]
struct Opt {
    /// Skip loading the ThinkTerm configuration
    #[arg(long, short = 'n')]
    skip_config: bool,

    /// Specify the configuration file to use, overrides the normal
    /// configuration file resolution
    #[arg(
        long = "config-file",
        value_parser,
        conflicts_with = "skip_config",
        value_hint=ValueHint::FilePath,
    )]
    config_file: Option<OsString>,

    /// Override specific configuration values
    #[arg(
        long = "config",
        name = "name=value",
        value_parser=ValueParser::new(name_equals_value),
        number_of_values = 1)]
    config_override: Vec<(String, String)>,

    /// On Windows, whether to attempt to attach to the parent
    /// process console to display logging output
    #[arg(long = "attach-parent-console")]
    #[allow(dead_code)]
    attach_parent_console: bool,

    #[command(subcommand)]
    cmd: Option<SubCommand>,
}

#[derive(Debug, Parser, Clone)]
enum SubCommand {
    #[command(
        name = "start",
        about = "Start the GUI, optionally running an alternative program [aliases: -e]"
    )]
    Start(StartCommand),

    /// Start the GUI in blocking mode. You shouldn't see this, but you
    /// may see it in shell completions because of this open clap issue:
    /// <https://github.com/clap-rs/clap/issues/1335>
    #[command(short_flag_alias = 'e', hide = true)]
    BlockingStart(StartCommand),

    #[command(name = "ssh", about = "Establish an ssh session")]
    Ssh(SshCommand),

    #[command(name = "serial", about = "Open a serial port")]
    Serial(SerialCommand),

    #[command(name = "connect", about = "Connect to wezterm multiplexer")]
    Connect(ConnectCommand),

    #[command(name = "ls-fonts", about = "Display information about fonts")]
    LsFonts(LsFontsCommand),

    #[command(name = "show-keys", about = "Show key assignments")]
    ShowKeys(ShowKeysCommand),
}

async fn async_run_ssh(opts: SshCommand) -> anyhow::Result<()> {
    let mut ssh_option = HashMap::new();
    if opts.verbose {
        ssh_option.insert("wezterm_ssh_verbose".to_string(), "true".to_string());
    }
    for (k, v) in opts.config_override {
        ssh_option.insert(k.to_lowercase().to_string(), v);
    }

    let dom = SshDomain {
        name: format!("SSH to {}", opts.user_at_host_and_port),
        remote_address: opts.user_at_host_and_port.host_and_port.clone(),
        username: opts.user_at_host_and_port.username.clone(),
        multiplexing: SshMultiplexing::None,
        ssh_option,
        ..Default::default()
    };

    let start_command = StartCommand {
        always_new_process: true,
        class: opts.class,
        cwd: None,
        no_auto_connect: true,
        position: opts.position,
        workspace: None,
        prog: opts.prog.clone(),
        ..Default::default()
    };

    let cmd = if !opts.prog.is_empty() {
        let builder = CommandBuilder::from_argv(opts.prog);
        Some(builder)
    } else {
        None
    };

    let domain: Arc<dyn Domain> = Arc::new(mux::ssh::RemoteSshDomain::with_ssh_domain(&dom)?);
    let mux = Mux::get();
    mux.add_domain(&domain);
    mux.set_default_domain(&domain);

    let should_publish = false;
    async_run_terminal_gui(cmd, start_command, should_publish).await
}

fn run_ssh(opts: SshCommand) -> anyhow::Result<()> {
    crate::state_backup::enter_gui();
    if let Some(cls) = opts.class.as_ref() {
        crate::set_window_class(cls);
    }
    if let Some(pos) = opts.position.as_ref() {
        set_window_position(pos.clone());
    }

    build_initial_mux(&config::configuration(), None, None)?;

    let gui = crate::frontend::try_new()?;

    promise::spawn::spawn(async {
        if let Err(err) = async_run_ssh(opts).await {
            terminate_with_error(err);
        }
    })
    .detach();

    maybe_show_configuration_error_window();
    let outcome = gui.run_forever();
    crate::local_sessions::finish_stop_at_exit(&config::configuration());
    outcome
}

async fn async_run_serial(opts: SerialCommand) -> anyhow::Result<()> {
    let serial_domain = SerialDomain {
        name: format!("Serial Port {}", opts.port),
        port: Some(opts.port.clone()),
        baud: opts.baud,
    };

    let start_command = StartCommand {
        always_new_process: true,
        class: opts.class,
        cwd: None,
        no_auto_connect: true,
        position: opts.position,
        workspace: None,
        domain: Some(serial_domain.name.clone()),
        ..Default::default()
    };

    let cmd = None;

    let domain: Arc<dyn Domain> = Arc::new(LocalDomain::new_serial_domain(serial_domain)?);
    let mux = Mux::get();
    mux.add_domain(&domain);

    let should_publish = false;
    async_run_terminal_gui(cmd, start_command, should_publish).await
}

fn run_serial(config: config::ConfigHandle, opts: SerialCommand) -> anyhow::Result<()> {
    crate::state_backup::enter_gui();
    if let Some(cls) = opts.class.as_ref() {
        crate::set_window_class(cls);
    }
    if let Some(pos) = opts.position.as_ref() {
        set_window_position(pos.clone());
    }

    build_initial_mux(&config, None, None)?;

    let gui = crate::frontend::try_new()?;

    promise::spawn::spawn(async {
        if let Err(err) = async_run_serial(opts).await {
            terminate_with_error(err);
        }
    })
    .detach();

    maybe_show_configuration_error_window();
    let outcome = gui.run_forever();
    crate::local_sessions::finish_stop_at_exit(&config::configuration());
    outcome
}

fn have_panes_in_domain_and_ws(domain: &Arc<dyn Domain>, workspace: &Option<String>) -> bool {
    let mux = Mux::get();
    let have_panes_in_domain = mux
        .iter_panes()
        .iter()
        .any(|p| p.domain_id() == domain.domain_id());

    if !have_panes_in_domain {
        return false;
    }

    if let Some(ws) = &workspace {
        for window_id in mux.iter_windows_in_workspace(ws) {
            if let Some(win) = mux.get_window(window_id) {
                for t in win.iter() {
                    for p in t.iter_panes_ignoring_zoom() {
                        if p.pane.domain_id() == domain.domain_id() {
                            return true;
                        }
                    }
                }
            }
        }
        false
    } else {
        true
    }
}

/// Attach `domain`, retrying transient failures with exponential backoff.
/// ClientDomain owns the shared retry engine so generic Domain::attach calls
/// (including mux spawn/split paths) receive the same behavior. Other domain
/// kinds have no distinct attach operation and remain single-attempt.
pub(crate) async fn attach_domain_with_retry(
    domain: Arc<dyn Domain>,
    window_id: Option<mux::window::WindowId>,
    ui: mux::connui::ConnectionUI,
    mut keep_going: impl FnMut() -> bool,
    max_total: Option<std::time::Duration>,
) -> anyhow::Result<AttachRetryOutcome> {
    if let Some(client) = domain.downcast_ref::<ClientDomain>() {
        return client
            .attach_with_ui_retry(window_id, ui, keep_going, max_total)
            .await;
    }
    if !keep_going() {
        ui.close();
        return Ok(AttachRetryOutcome::Cancelled);
    }
    domain.attach(window_id).await?;
    ui.close();
    Ok(AttachRetryOutcome::Attached)
}

async fn attach_domain_in_window_with_retry(
    domain: Arc<dyn Domain>,
    window_id: mux::window::WindowId,
) -> anyhow::Result<AttachRetryOutcome> {
    // The local session host never shows connection progress in a window
    // (see ClientDomain::attach); here it is usually attached already, and
    // even then the tab would flash before the attach saw that.
    let is_host = domain
        .downcast_ref::<ClientDomain>()
        .is_some_and(|client| client.is_local_session_host());
    let ui = if is_host {
        mux::connui::ConnectionUI::new_headless()
    } else {
        mux::connui::ConnectionUI::with_params(mux::connui::ConnectionUIParams {
            window_id: Some(window_id),
            ..Default::default()
        })
    };
    attach_domain_with_retry(
        domain,
        Some(window_id),
        ui,
        move || Mux::get().get_window(window_id).is_some(),
        Some(std::time::Duration::from_secs(60)),
    )
    .await
}

/// Returns the domain id to tag domain-owned mux windows with, when the
/// domain is a ClientDomain (remote mux). Tagged windows are exempt from
/// the frontend's saved-thread restore/adoption and local layout snapshots.
fn client_domain_origin(domain: &Arc<dyn Domain>) -> Option<mux::domain::DomainId> {
    domain
        .downcast_ref::<ClientDomain>()
        .map(|_| domain.domain_id())
}

async fn spawn_tab_in_domain_if_mux_is_empty(
    cmd: Option<CommandBuilder>,
    is_connecting: bool,
    domain: Option<Arc<dyn Domain>>,
    workspace: Option<String>,
) -> anyhow::Result<()> {
    let mux = Mux::get();

    let domain = domain.unwrap_or_else(|| mux.default_domain());

    // The local session host was attached before this ran and its windows
    // are mirrored already (the attach installs the topology before it
    // returns). Its threads live in thread workspaces, which reconcile
    // leaves hidden, so "the domain has panes" is not "a window will open".
    // Judge the startup workspace alone: a startup tab there gets a window,
    // whose restore then adopts the live thread and cleans the tab up -- as
    // it does in process.
    let mut workspace = workspace;
    let is_host = domain
        .downcast_ref::<ClientDomain>()
        .is_some_and(|client| client.is_local_session_host());
    if is_host && workspace.is_none() {
        workspace = Some(mux.active_workspace());
    }

    if !is_connecting {
        if have_panes_in_domain_and_ws(&domain, &workspace) {
            return Ok(());
        }
    }

    let position = None;
    let builder = mux.new_empty_window_for_domain(
        workspace.clone(),
        position,
        client_domain_origin(&domain),
    );
    let window_id = *builder;
    // Dropping the builder tells the frontend about the window. For a domain
    // that still has to attach, that happens now, so the attach await below
    // does not hold the window back (it opens at the initial size and the
    // TabAddedToWindow notification adjusts it later). The host is attached
    // already, and its spawn is a round trip: shown empty, the window is
    // adopted by the thread restore and cleaned up before the shell lands,
    // and the shell then makes a window of its own -- a second one on
    // screen. So the host's window is announced once the tab is in it, the
    // order the in-process spawn has.
    let held_window = if is_host {
        Some(builder)
    } else {
        drop(builder);
        None
    };

    let config = config::configuration();
    config.update_ulimit()?;

    match attach_domain_in_window_with_retry(Arc::clone(&domain), window_id).await? {
        AttachRetryOutcome::Attached => {}
        AttachRetryOutcome::Cancelled => return Ok(()),
    }

    if have_panes_in_domain_and_ws(&domain, &workspace) {
        trigger_and_log_gui_attached(MuxDomain(domain.domain_id())).await;
        return Ok(());
    }

    let _config_subscription = config::subscribe_to_config_reload(move || {
        promise::spawn::spawn_into_main_thread(async move {
            if let Err(err) = update_mux_domains(&config::configuration()) {
                log::error!("Error updating mux domains: {:#}", err);
            }
        })
        .detach();
        true
    });

    let dpi = config.dpi.unwrap_or_else(|| ::window::default_dpi());
    let spawned = domain
        .spawn(
            config.initial_size(dpi as u32, Some(cell_pixel_dims(&config, dpi)?)),
            cmd,
            None,
            window_id,
        )
        .await;
    drop(held_window);
    spawned?;
    trigger_and_log_gui_attached(MuxDomain(domain.domain_id())).await;
    Ok(())
}

/// `thinkterm connect <domain>` routed through the ThinkTerm Space system:
/// find-or-create the domain's dedicated Space; if already connected in this
/// process, focus an existing window instead of duplicating; otherwise create
/// the connect window with an explicit Space claim so the frontend's
/// reconcile/restore can never hijack it away from the in-window ConnectionUI.
/// `connect <name>` fallback when the name is not a lua-configured domain:
/// resolve it against the ThinkTerm SSH host store and register a
/// multiplexing client domain built from that host, including its stored
/// credentials, so hosts added in the UI are connectable (and reconnect
/// silently) without a lua ssh_domains entry.
pub(crate) fn connect_domain_from_ssh_host(name: &str) -> anyhow::Result<Arc<dyn Domain>> {
    let entry = crate::ssh_hosts::list_all_hosts()
        .into_iter()
        .find(|entry| entry.spec.label == name || entry.id == name)
        .ok_or_else(|| {
            anyhow!("invalid domain {name}: not in ssh_domains and not a saved SSH host")
        })?;
    let mut dom = crate::ssh_hosts::build_ssh_domain(&entry.spec);
    dom.name = name.to_string();
    dom.multiplexing = SshMultiplexing::WezTerm;
    dom.stored_password = entry
        .spec
        .password
        .as_deref()
        .map(crate::secret::reveal)
        .filter(|p| !p.is_empty());
    let domain: Arc<dyn Domain> = Arc::new(ClientDomain::new(ClientDomainConfig::Ssh(dom)));
    Mux::get().add_domain(&domain);
    Ok(domain)
}

async fn current_gui_terminal_size(
    mux_window_id: mux::window::WindowId,
    fallback: wezterm_term::TerminalSize,
) -> wezterm_term::TerminalSize {
    let Some(gui_window) = crate::frontend::front_end().gui_window_for_mux_window(mux_window_id)
    else {
        return fallback;
    };
    let (tx, rx) = smol::channel::bounded(1);
    gui_window
        .window
        .notify(crate::termwindow::TermWindowNotif::GetTerminalSize(tx));
    rx.recv().await.unwrap_or(fallback)
}

async fn adopt_materialized_workspace_window(
    source_window_id: mux::window::WindowId,
    workspace: &str,
    domain_id: mux::domain::DomainId,
) -> anyhow::Result<()> {
    let mux = Mux::get();
    if mux.get_window(source_window_id).is_some_and(|window| {
        window.get_workspace() == workspace
            && window.iter().any(|tab| {
                tab.iter_all_panes()
                    .iter()
                    .any(|pane| pane.domain_id() == domain_id)
            })
    }) {
        // Initial attach may already have folded the restored remote window
        // into the claimed native window. Do not jump to a second mux window
        // in the same workspace and kill this one: that would turn local GUI
        // adoption into a remote pane close when a workspace temporarily has
        // multiple mux windows.
        return Ok(());
    }
    let Some(target_window_id) =
        mux.iter_windows_in_workspace(workspace)
            .into_iter()
            .find(|window_id| {
                *window_id != source_window_id
                    && mux.get_window(*window_id).is_some_and(|window| {
                        window.iter().any(|tab| {
                            tab.iter_all_panes()
                                .iter()
                                .any(|pane| pane.domain_id() == domain_id)
                        })
                    })
            })
    else {
        anyhow::bail!("restored workspace {workspace} has no live window for domain {domain_id}");
    };
    let Some(gui_window) = crate::frontend::front_end().gui_window_for_mux_window(source_window_id)
    else {
        return Ok(());
    };
    crate::frontend::front_end().set_switching_workspaces(true);
    let (tx, rx) = smol::channel::bounded(1);
    gui_window
        .window
        .notify(crate::termwindow::TermWindowNotif::Apply(Box::new(
            move |term_window| {
                let switched = if term_window.mux_window_id == source_window_id {
                    term_window.switch_to_mux_window(target_window_id);
                    term_window.mux_window_id == target_window_id
                } else {
                    false
                };
                let _ = tx.try_send(switched);
            },
        )));
    let switched = rx
        .recv()
        .await
        .context("waiting for GUI to adopt restored mux window");
    crate::frontend::front_end().set_switching_workspaces(false);
    let switched = switched?;
    if !switched
        || crate::frontend::front_end()
            .gui_window_for_mux_window(target_window_id)
            .is_none()
    {
        anyhow::bail!("GUI did not confirm adoption of restored mux window {target_window_id}");
    }
    mux.kill_window(source_window_id);
    Ok(())
}

/// Clear the first-open gate if authentication/materialization fails or the
/// async Connect request is cancelled. Success hands the one-shot intent to
/// the GUI, which waits for the selected remote tab before consuming it.
struct RemoteOpenRequest {
    window: Option<window::Window>,
    domain_id: mux::domain::DomainId,
}

impl RemoteOpenRequest {
    fn finish(mut self) {
        if let Some(window) = self.window.take() {
            let domain_id = self.domain_id;
            window.notify(crate::termwindow::TermWindowNotif::Apply(Box::new(move |tw| {
                tw.finish_remote_open(domain_id);
            })));
        }
    }
}

impl Drop for RemoteOpenRequest {
    fn drop(&mut self) {
        if let Some(window) = self.window.take() {
            let domain_id = self.domain_id;
            window.notify(crate::termwindow::TermWindowNotif::Apply(Box::new(move |tw| {
                tw.cancel_remote_open(domain_id);
            })));
        }
    }
}

pub(crate) async fn connect_domain_into_space(
    cmd: Option<CommandBuilder>,
    domain: Arc<dyn Domain>,
) -> anyhow::Result<()> {
    use mux::domain::DomainState;

    let mux = Mux::get();
    let domain_name = domain.domain_name().to_string();
    let plan = workspace_threads::ensure_mux_domain_space(&domain_name);

    // Already attached in this process: focus one of this domain's windows.
    if domain.state() == DomainState::Attached {
        for window_id in mux.iter_windows() {
            let owned = mux
                .get_window(window_id)
                .map_or(false, |w| w.origin_domain() == Some(domain.domain_id()));
            if !owned {
                continue;
            }
            if let Some(gui_window) =
                crate::frontend::front_end().gui_window_for_mux_window(window_id)
            {
                let domain_id = domain.domain_id();
                gui_window.window.notify(crate::termwindow::TermWindowNotif::Apply(Box::new(
                    move |tw| tw.claim_remote_viewport_for_explicit_open(domain_id),
                )));
                gui_window.window.focus();
                return Ok(());
            }
        }
    }

    // Claim the Space for the window we are about to create. If another
    // in-process window already occupies it, focus that window instead.
    let space_owner_id = workspace_threads::next_space_owner_id();
    if !workspace_threads::switch_window_space(space_owner_id, &plan.space_id) {
        for window_id in mux.iter_windows() {
            let owned = mux
                .get_window(window_id)
                .map_or(false, |w| w.origin_domain() == Some(domain.domain_id()));
            if !owned {
                continue;
            }
            if let Some(gui_window) =
                crate::frontend::front_end().gui_window_for_mux_window(window_id)
            {
                let domain_id = domain.domain_id();
                gui_window.window.notify(crate::termwindow::TermWindowNotif::Apply(Box::new(
                    move |tw| tw.claim_remote_viewport_for_explicit_open(domain_id),
                )));
                gui_window.window.focus();
                return Ok(());
            }
        }
        anyhow::bail!(
            "Space for domain {domain_name} is occupied by another window; \
             switch to it instead"
        );
    }

    // Create the connect mux window directly in the Space's main-thread
    // workspace so that thread switching can find it again by name (the
    // fold of the remote's primary window into this window goes by the
    // origin-domain claim, not by workspace name). Claim it before the
    // builder drops so the WindowCreated notification can never race a
    // reconcile-spawned duplicate.
    let thread_workspace = workspace_threads::ensure_mux_thread_workspace(&plan);
    let window_id = {
        let builder = mux.new_empty_window_for_domain(
            Some(thread_workspace.clone()),
            None,
            Some(domain.domain_id()),
        );
        let id = *builder;
        crate::frontend::front_end().claim_spawned_mux_window(id);
        id
    };

    TermWindow::new_window_with_claimed_space(
        window_id,
        space_owner_id,
        plan.space_id.clone(),
        Some(domain.domain_id()),
    )
    .await?;
    // Keep the native handle: adopting the restored workspace can replace
    // its mux-window id while this explicit connection is in progress.
    let window = crate::frontend::front_end()
        .gui_window_for_mux_window(window_id)
        .map(|gui| gui.window);
    if window.is_none() {
        // The window was recorded under this id with no await since; a
        // miss here means the intent armed inside it can never be released.
        log::warn!("remote open: no GUI window for mux window {window_id}; intent unreleased");
    }
    let remote_open = RemoteOpenRequest {
        window,
        domain_id: domain.domain_id(),
    };

    let config = config::configuration();
    config.update_ulimit()?;

    // Keep the claimed connect window alive from the authentication UI all
    // the way through creation of its first remote pane. ClientDomain::attach
    // has its own Activity, but drops it before returning; closing the
    // ConnectionUI can then prune this still-empty window before spawn()
    // installs the first tab.
    let connect_activity = mux::activity::Activity::new();

    // The ConnectionUI (auth prompts) appears as a tab inside this window.
    // Transient failures retry with backoff instead of terminating the
    // process; the user can bail out by closing the window. Fatal failures
    // (auth declined, version mismatch) leave the UI open showing the error
    // and skip the rest of the setup — the window closes itself once the
    // ConnectionUI's close delay runs out, or when the user closes it.
    {
        let ui = mux::connui::ConnectionUI::with_params(mux::connui::ConnectionUIParams {
            window_id: Some(window_id),
            ..Default::default()
        });
        match attach_domain_with_retry(
            domain.clone(),
            Some(window_id),
            ui.clone(),
            move || Mux::get().get_window(window_id).is_some(),
            None,
        )
        .await
        {
            Ok(AttachRetryOutcome::Cancelled) => return Ok(()),
            Ok(AttachRetryOutcome::Attached) => {
                // The transport can finish connecting just after the user
                // closes its GUI. Do not turn that late success into a
                // headless attached domain.
                if mux.get_window(window_id).is_none() {
                    ui.close();
                    if domain.state() == DomainState::Attached {
                        domain.detach()?;
                    }
                    return Ok(());
                }
            }
            Err(err) => {
                log::error!("attaching {domain_name} failed: {err:#}");
                return Ok(());
            }
        }
    }

    // The server owns the sidebar tree and only hands it over as part of the
    // attach, so the Space and thread picked above were a guess: a device
    // meeting this server for the first time has to invent a Space before it
    // can authenticate, and learns the real ones only now. Ask again against
    // the tree that has since landed, then push up whatever the server is
    // still missing (its tree can legitimately be empty — the last Space was
    // deleted from another device — in which case the Space we invented is
    // the right answer and wants seeding).
    let settled = workspace_threads::ensure_mux_domain_space(&domain_name);
    let settled_workspace = workspace_threads::ensure_mux_thread_workspace(&settled);
    workspace_threads::reconcile_remote_subtree(&domain_name);

    // Ordinary GUI startup always enters through EnsureThinkTermThread. It is
    // idempotent when the workspace is already live and is the only path that
    // can restore a saved multi-tab/split layout after a mux replacement.
    // In particular, do not let an unrelated server startup shell or an early
    // primary-window fold suppress recovery. An explicit custom command keeps
    // the traditional direct-spawn semantics below.
    if cmd.is_none() {
        let _config_subscription = config::subscribe_to_config_reload(move || {
            promise::spawn::spawn_into_main_thread(async move {
                if let Err(err) = update_mux_domains(&config::configuration()) {
                    log::error!("Error updating mux domains: {:#}", err);
                }
            })
            .detach();
            true
        });

        let dpi = config.dpi.unwrap_or_else(|| ::window::default_dpi());
        let fallback_size = config.initial_size(dpi as u32, Some(cell_pixel_dims(&config, dpi)?));
        let size = current_gui_terminal_size(window_id, fallback_size).await;
        let client = domain
            .downcast_ref::<ClientDomain>()
            .ok_or_else(|| anyhow!("{domain_name} is not a multiplexing client domain"))?;
        crate::frontend::front_end().set_switching_workspaces(true);
        let ensured = client
            .ensure_thinkterm_thread(Some(settled.thread_id.clone()), size)
            .await;
        crate::frontend::front_end().set_switching_workspaces(false);
        let response = ensured.context("restore remote ThinkTerm landing layout")?;
        if response.workspace != settled_workspace {
            anyhow::bail!(
                "remote restored workspace {}, expected {}",
                response.workspace,
                settled_workspace
            );
        }
        adopt_materialized_workspace_window(window_id, &response.workspace, domain.domain_id())
            .await?;
    } else {
        let thread_workspace_filter = Some(thread_workspace.clone());
        if !have_panes_in_domain_and_ws(&domain, &thread_workspace_filter) {
            let _config_subscription = config::subscribe_to_config_reload(move || {
                promise::spawn::spawn_into_main_thread(async move {
                    if let Err(err) = update_mux_domains(&config::configuration()) {
                        log::error!("Error updating mux domains: {:#}", err);
                    }
                })
                .detach();
                true
            });
            let dpi = config.dpi.unwrap_or_else(|| ::window::default_dpi());
            let size = config.initial_size(dpi as u32, Some(cell_pixel_dims(&config, dpi)?));
            let _tab = domain.spawn(size, cmd, None, window_id).await?;
        }
    }
    remote_open.finish();
    drop(connect_activity);
    trigger_and_log_gui_attached(MuxDomain(domain.domain_id())).await;

    // No explicit workspace rename AFTER attach: the window was created in
    // the thread workspace up front, and the client domain aligns the
    // server's workspace to it when the primary window folds in (see
    // process_pane_list). Renaming post-attach used to race the fold and
    // polluted the server's workspace names.
    Ok(())
}

async fn connect_to_auto_connect_domains() -> anyhow::Result<()> {
    let mux = Mux::get();
    for dom in mux.iter_domains() {
        let Some(client) = dom.downcast_ref::<ClientDomain>() else {
            continue;
        };
        if !client.connect_automatically() {
            continue;
        }
        // One UI (its own window, since no GUI window exists yet) hosts the
        // first attempt and any background retries for this domain. The
        // local session host connects headless: a unix socket asks nothing,
        // and the UI's own mux window would be taken for the startup window
        // by the thread store, which then starts a fresh thread when it
        // closes.
        let ui = if client.is_local_session_host() {
            mux::connui::ConnectionUI::new_headless()
        } else {
            mux::connui::ConnectionUI::with_params(Default::default())
        };
        // The first attempt stays synchronous so that, on the happy path,
        // auto-connected domains are live before the first GUI window opens
        // (their windows adopt/fold correctly instead of racing startup).
        match client.attach_with_ui(None, ui.clone()).await {
            // Only a server this process started (or took over) is the
            // setting's to stop later; one the user runs is left alone.
            Ok(()) if client.is_local_session_host()
                && wezterm_client::local_update::local_session_host_started_by_us() =>
            {
                crate::local_sessions::note_host_attached();
            }
            Ok(()) => {}
            // The session server of this machine must be there before the
            // first window spawns into it; without it this launch runs its
            // terminals in process rather than in a domain that is not
            // attached.
            Err(err) if client.is_local_session_host() => {
                crate::local_sessions::fall_back_to_in_process(&mux, &err);
                ui.close();
            }
            Err(err) if is_fatal_attach_error(&err) => {
                log::error!("auto-connect {}: {err:#}", dom.domain_name());
            }
            Err(err) => {
                // A domain that is unreachable at login (VPN routes not up
                // yet) must neither block GUI startup nor take the other
                // auto-connect domains down with it: keep retrying in the
                // background and let startup proceed.
                log::error!(
                    "auto-connect {}: {err:#}; retrying in the background",
                    dom.domain_name()
                );
                let domain = Arc::clone(&dom);
                let gate_domain = Arc::clone(&dom);
                promise::spawn::spawn(async move {
                    let name = domain.domain_name().to_string();
                    match attach_domain_with_retry(
                        domain,
                        None,
                        ui,
                        move || gate_domain.state() == mux::domain::DomainState::Detached,
                        Some(std::time::Duration::from_secs(60)),
                    )
                    .await
                    {
                        Ok(_) => {}
                        Err(err) => {
                            log::error!("auto-connect {name}: giving up: {err:#}");
                        }
                    }
                })
                .detach();
            }
        }
    }
    Ok(())
}

async fn trigger_gui_startup(
    lua: Option<Rc<mlua::Lua>>,
    spawn: Option<SpawnCommand>,
) -> anyhow::Result<()> {
    if let Some(lua) = lua {
        let args = lua.pack_multi(spawn)?;
        config::lua::emit_event(&lua, ("gui-startup".to_string(), args)).await?;
    }
    Ok(())
}

async fn trigger_and_log_gui_startup(spawn_command: Option<SpawnCommand>) {
    if let Err(err) =
        config::with_lua_config_on_main_thread(move |lua| trigger_gui_startup(lua, spawn_command))
            .await
    {
        let message = format!("while processing gui-startup event: {:#}", err);
        log::error!("{}", message);
        persistent_toast_notification("Error", &message);
    }
}

async fn trigger_gui_attached(lua: Option<Rc<mlua::Lua>>, domain: MuxDomain) -> anyhow::Result<()> {
    if let Some(lua) = lua {
        let args = lua.pack_multi(domain)?;
        config::lua::emit_event(&lua, ("gui-attached".to_string(), args)).await?;
    }
    Ok(())
}

async fn trigger_and_log_gui_attached(domain: MuxDomain) {
    if let Err(err) =
        config::with_lua_config_on_main_thread(move |lua| trigger_gui_attached(lua, domain)).await
    {
        let message = format!("while processing gui-attached event: {:#}", err);
        log::error!("{}", message);
        persistent_toast_notification("Error", &message);
    }
}

fn cell_pixel_dims(config: &ConfigHandle, dpi: f64) -> anyhow::Result<(usize, usize)> {
    let fontconfig = Rc::new(FontConfiguration::new(Some(config.clone()), dpi as usize)?);
    let render_metrics = RenderMetrics::new(&fontconfig)?;
    Ok((
        render_metrics.cell_size.width as usize,
        render_metrics.cell_size.height as usize,
    ))
}

async fn async_run_terminal_gui(
    cmd: Option<CommandBuilder>,
    opts: StartCommand,
    should_publish: bool,
) -> anyhow::Result<()> {
    config::create_user_owned_dirs(&*config::RUNTIME_DIR)?;
    config::create_user_owned_dirs(&*config::CACHE_DIR)?;

    let unix_socket_path =
        config::RUNTIME_DIR.join(format!("gui-sock-{}", unsafe { libc::getpid() }));
    std::env::set_var("WEZTERM_UNIX_SOCKET", unix_socket_path.clone());
    wezterm_blob_leases::register_storage(Arc::new(
        wezterm_blob_leases::simple_tempdir::SimpleTempDir::new_in(&*config::CACHE_DIR)?,
    ))?;
    if let Err(err) = spawn_mux_server(unix_socket_path, should_publish) {
        log::warn!("{:#}", err);
    }
    // For the terminals this process runs itself; the session server
    // sweeps its own.
    mux::spawn_idle_image_sweeper();

    // This process is the GUI now (a launch that handed its command to a
    // running GUI returned before this), so a server the setting no longer
    // wants can go.
    crate::local_sessions::stop_background_server_when_off(&config::configuration());
    if !opts.no_auto_connect {
        connect_to_auto_connect_domains().await?;
    }

    let spawn_command = match &cmd {
        Some(cmd) => Some(SpawnCommand::from_command_builder(cmd)?),
        None => None,
    };

    // Apply the domain to the command
    let spawn_command = match (spawn_command, &opts.domain) {
        (Some(spawn), Some(name)) => Some(SpawnCommand {
            domain: SpawnTabDomain::DomainName(name.to_string()),
            ..spawn
        }),
        (None, Some(name)) => Some(SpawnCommand {
            domain: SpawnTabDomain::DomainName(name.to_string()),
            ..SpawnCommand::default()
        }),
        (spawn, None) => spawn,
    };
    let mux = Mux::get();

    let domain = if let Some(name) = &opts.domain {
        let domain = match mux.get_domain_by_name(name) {
            Some(domain) => domain,
            None => connect_domain_from_ssh_host(name)?,
        };
        Some(domain)
    } else {
        None
    };

    if !opts.attach {
        trigger_and_log_gui_startup(spawn_command).await;
    }

    let is_connecting = opts.attach;

    if let Some(domain) = &domain {
        if !opts.attach {
            let window_id = {
                // Force the builder to notify the frontend early,
                // so that the attach await below doesn't block it.
                let workspace = None;
                let position = None;
                let builder = mux.new_empty_window_for_domain(
                    workspace,
                    position,
                    client_domain_origin(domain),
                );
                *builder
            };

            match attach_domain_in_window_with_retry(Arc::clone(domain), window_id).await? {
                AttachRetryOutcome::Attached => {}
                AttachRetryOutcome::Cancelled => return Ok(()),
            }
            let config = config::configuration();
            let dpi = config.dpi.unwrap_or_else(|| ::window::default_dpi());
            let tab = domain
                .spawn(
                    config.initial_size(dpi as u32, Some(cell_pixel_dims(&config, dpi)?)),
                    cmd.clone(),
                    None,
                    window_id,
                )
                .await?;
            let mut window = mux
                .get_window_mut(window_id)
                .ok_or_else(|| anyhow!("failed to get mux window id {window_id}"))?;
            if let Some(tab_idx) = window.idx_by_id(tab.tab_id()) {
                window.set_active_without_saving(tab_idx);
            }
            trigger_and_log_gui_attached(MuxDomain(domain.domain_id())).await;
        }
    }
    // `thinkterm connect` to a mux client domain goes through the Space system:
    // dedicated find-or-create Space, explicit window claim, no restore.
    if is_connecting {
        if let Some(domain) = &domain {
            if domain.downcast_ref::<ClientDomain>().is_some() {
                return connect_domain_into_space(cmd, Arc::clone(domain)).await;
            }
        }
    }
    let started =
        spawn_tab_in_domain_if_mux_is_empty(cmd, is_connecting, domain, opts.workspace).await;
    crate::frontend::front_end().startup_settled_soon();
    started
}

#[derive(Debug)]
enum Publish {
    TryPathOrPublish(PathBuf),
    NoConnectNoPublish,
    NoConnectButPublish,
}

impl Publish {
    pub fn resolve(mux: &Arc<Mux>, config: &ConfigHandle, always_new_process: bool) -> Self {
        // The local session host is the default domain of an ordinary
        // launch too: a second launch still hands its command to this GUI.
        let default_name = mux.default_domain().domain_name().to_string();
        let default_is_local = default_name == config.default_domain.as_deref().unwrap_or("local")
            || crate::local_sessions::is_host_domain_name(&default_name);
        if !default_is_local {
            return Self::NoConnectNoPublish;
        }

        if always_new_process {
            return Self::NoConnectNoPublish;
        }

        if config::is_config_overridden() {
            // They're using a specific config file: assume that it is
            // different from the running gui
            log::trace!("skip existing gui: config is different");
            return Self::NoConnectNoPublish;
        }

        match wezterm_client::discovery::resolve_gui_sock_path(
            &crate::termwindow::get_window_class(),
        ) {
            Ok(path) => Self::TryPathOrPublish(path),
            Err(_) => Self::NoConnectButPublish,
        }
    }

    pub fn should_publish(&self) -> bool {
        match self {
            Self::TryPathOrPublish(_) | Self::NoConnectButPublish => true,
            Self::NoConnectNoPublish => false,
        }
    }

    pub fn try_spawn(
        &mut self,
        cmd: Option<CommandBuilder>,
        config: &ConfigHandle,
        workspace: Option<&str>,
        domain: SpawnTabDomain,
        new_tab: bool,
    ) -> anyhow::Result<bool> {
        if let Publish::TryPathOrPublish(gui_sock) = &self {
            let dom = config::UnixDomain {
                socket_path: Some(gui_sock.clone()),
                no_serve_automatically: true,
                ..Default::default()
            };
            let mut ui = mux::connui::ConnectionUI::new_headless();
            match wezterm_client::client::Client::new_unix_domain(None, &dom, false, &mut ui, true)
            {
                Ok(client) => {
                    let executor = promise::spawn::ScopedExecutor::new();
                    let command = cmd.clone();
                    let res = block_on(executor.run(async move {
                        let vers = client.verify_version_compat(&mut ui).await?;

                        if vers.executable_path != std::env::current_exe().context("resolve executable path")? {
                            *self = Publish::NoConnectNoPublish;
                            anyhow::bail!(
                                "Running GUI is a different executable from us, will start a new one");
                        }
                        let config_file_path = std::env::var_os("THINKTERM_CONFIG_FILE")
                            .or_else(|| std::env::var_os("WEZTERM_CONFIG_FILE"))
                            .map(Into::into);
                        if vers.config_file_path != config_file_path {
                            *self = Publish::NoConnectNoPublish;
                            anyhow::bail!(
                                "Running GUI has different config from us, will start a new one"
                            );
                        }

                        let window_id = if new_tab || config.prefer_to_spawn_tabs {
                            if let Ok(pane_id) = client.resolve_pane_id(None).await {
                                let panes = client.list_panes().await?;

                                let mut window_id = None;
                                'outer: for tabroot in panes.tabs {
                                    let mut cursor = tabroot.into_tree().cursor();

                                    loop {
                                        if let Some(stack) = cursor.leaf_mut() {
                                            for entry in &stack.panes {
                                                if entry.pane_id == pane_id {
                                                    window_id.replace(entry.window_id);
                                                    break 'outer;
                                                }
                                            }
                                        }
                                        match cursor.preorder_next() {
                                            Ok(c) => cursor = c,
                                            Err(_) => break,
                                        }
                                    }
                                }
                                window_id

                            } else {
                                None
                            }
                        } else {
                            None
                        };

                        client
                            .spawn_v2(codec::SpawnV2 {
                                domain,
                                window_id,
                                command: command
                                    .as_ref()
                                    .map(mux::command_spec::CommandSpecExt::from_command_builder),
                                command_dir: None,
                                size: config.initial_size(0, None),
                                workspace: workspace.unwrap_or(
                                    config
                                        .default_workspace
                                        .as_deref()
                                        .unwrap_or(mux::DEFAULT_WORKSPACE)
                                ).to_string(),
                            })
                            .await
                    }));

                    match res {
                        Ok(res) => {
                            log::info!(
                                "Spawned your command via the existing ThinkTerm GUI instance. \
                             Use thinkterm start --always-new-process if you do not want this behavior. \
                             Result={:?}",
                                res
                            );
                            Ok(true)
                        }
                        Err(err) => {
                            log::trace!(
                                "while attempting to ask existing instance to spawn: {:#}",
                                err
                            );
                            Ok(false)
                        }
                    }
                }
                Err(err) => {
                    // Couldn't connect: it's probably a stale symlink.
                    // That's fine: we can continue with starting a fresh gui below.
                    log::trace!("{:#}", err);
                    Ok(false)
                }
            }
        } else {
            Ok(false)
        }
    }
}

fn spawn_mux_server(unix_socket_path: PathBuf, should_publish: bool) -> anyhow::Result<()> {
    let mut listener =
        wezterm_mux_server_impl::local::LocalListener::with_domain(&config::UnixDomain {
            socket_path: Some(unix_socket_path.clone()),
            ..Default::default()
        })?;
    std::thread::spawn(move || {
        let name_holder;
        if should_publish {
            name_holder = wezterm_client::discovery::publish_gui_sock_path(
                &unix_socket_path,
                &crate::termwindow::get_window_class(),
            );
            if let Err(err) = &name_holder {
                log::warn!("{:#}", err);
            }
        }

        listener.run();
        std::fs::remove_file(unix_socket_path).ok();
    });

    Ok(())
}

fn setup_mux(
    local_domain: Arc<dyn Domain>,
    config: &ConfigHandle,
    default_domain_name: Option<&str>,
    default_workspace_name: Option<&str>,
) -> anyhow::Result<Arc<Mux>> {
    let mux = Arc::new(mux::Mux::new(Some(local_domain.clone())));
    Mux::set_mux(&mux);
    // The user-facing Agents toggle governs this process's own detector as
    // well as the panel; headless mux servers follow the Lua option alone.
    mux::agent_status::set_process_preference(|| {
        crate::native_settings::agent_panel_enabled()
    });
    // The user-facing shell choice reaches every local spawn from here.
    // A headless mux server installs nothing, so this preference can only
    // ever decide what *this* process spawns, never what a remote server
    // runs for us.
    mux::default_prog::set_process_preference(crate::native_settings::default_shell);
    let client_id = Arc::new(mux::client::generate_client_id());
    mux.register_client(client_id.clone());
    mux.replace_identity(Some(client_id));
    let default_workspace_name = default_workspace_name.unwrap_or(
        config
            .default_workspace
            .as_deref()
            .unwrap_or(mux::DEFAULT_WORKSPACE),
    );
    mux.set_active_workspace(&default_workspace_name);
    crate::update::load_last_release_info();
    // Before the configured client domains: the host is the configured
    // unix domain with two flags set, registered under the same name.
    let session_host = crate::local_sessions::install(&mux, config);
    update_mux_domains(config)?;
    // Register ThinkTerm's saved SSH hosts as runtime mux domains so that
    // reconnecting / restoring remote sessions can resolve them by name.
    crate::ssh_hosts::register_saved_hosts();
    // Both sources of domain names are registered by now, so a Space naming
    // one that still does not exist belongs to a host that has been deleted.
    // Local removal only; the server keeps its copy.
    crate::ssh_hosts::forget_spaces_of_deleted_hosts();

    let default_name = default_domain_name.unwrap_or(
        session_host
            .as_deref()
            .or(config.default_domain.as_deref())
            .unwrap_or("local"),
    );

    let domain = match mux.get_domain_by_name(default_name) {
        Some(domain) => domain,
        // Not a lua-configured domain: try the ThinkTerm SSH host store
        // (register_saved_hosts above only registers direct-ssh domains;
        // this builds a multiplexing client domain for `connect`).
        None => connect_domain_from_ssh_host(default_name).with_context(|| {
            format!("desired default domain '{default_name}' was not found in mux")
        })?,
    };
    mux.set_default_domain(&domain);

    Ok(mux)
}

fn build_initial_mux(
    config: &ConfigHandle,
    default_domain_name: Option<&str>,
    default_workspace_name: Option<&str>,
) -> anyhow::Result<Arc<Mux>> {
    let domain: Arc<dyn Domain> = Arc::new(LocalDomain::new("local")?);
    setup_mux(domain, config, default_domain_name, default_workspace_name)
}

fn run_terminal_gui(opts: StartCommand, default_domain_name: Option<String>) -> anyhow::Result<()> {
    // Before anything reads the state: an import staged from Settings.
    crate::state_backup::enter_gui();
    if let Some(cls) = opts.class.as_ref() {
        crate::set_window_class(cls);
    }
    if let Some(pos) = opts.position.as_ref() {
        set_window_position(pos.clone());
    }

    // A mux server owns the sidebar tree for its Spaces; wezterm-client hands
    // every copy it receives to this sink, which merges it with the parts of
    // the view that belong to this device alone.
    wezterm_client::domain::set_thinkterm_tree_sink(|domain_name, tree| {
        crate::workspace_threads::ingest_remote_tree(domain_name, tree);
    });
    wezterm_client::domain::set_thinkterm_connect_sink(|domain_name, _connection_generation| {
        crate::workspace_threads::note_remote_connected(domain_name);
    });
    wezterm_client::domain::set_thinkterm_frontend_recovery_sink(
        crate::frontend::request_thinkterm_frontend_recovery,
    );
    wezterm_client::remote_update::set_keep_sessions_on_update(
        crate::native_settings::remote_update_keeps_sessions(),
    );
    wezterm_client::domain::set_local_session_host_replaced_sink(
        crate::local_sessions::on_host_replaced,
    );

    let config = config::configuration();
    let need_builder = !opts.prog.is_empty() || opts.cwd.is_some();

    let cmd = if need_builder {
        let prog = opts.prog.iter().map(|s| s.as_os_str()).collect::<Vec<_>>();
        // Deliberately still the Lua option, not the chosen shell: this
        // resolves before any domain exists and the command may be routed
        // to a remote/exec domain that never reaches the local tiers, so
        // suppressing it here would break `default_prog` for those. The
        // consequence is narrow and documented: with BOTH a Lua
        // `default_prog` and a chosen shell, `--cwd` startups follow the
        // Lua one. Every other spawn goes through LocalDomain::build_command
        // where the choice outranks it.
        let mut builder = config.build_prog(
            if prog.is_empty() { None } else { Some(prog) },
            config.default_prog.as_ref(),
            config.default_cwd.as_ref(),
        )?;
        if let Some(cwd) = &opts.cwd {
            builder.cwd(if cwd.is_relative() {
                current_dir()?.join(cwd).into_os_string().into()
            } else {
                Cow::Borrowed(cwd.as_ref())
            });
        }
        Some(builder)
    } else {
        None
    };

    let mux = build_initial_mux(
        &config,
        default_domain_name.as_deref(),
        opts.workspace.as_deref(),
    )?;
    // First, let's see if we can ask an already running wezterm to do this.
    // We must do this before we start the gui frontend as the scheduler
    // requirements are different.
    let mut publish = Publish::resolve(
        &mux,
        &config,
        opts.always_new_process || opts.position.is_some(),
    );
    log::trace!("{:?}", publish);
    if publish.try_spawn(
        cmd.clone(),
        &config,
        opts.workspace.as_deref(),
        match &opts.domain {
            Some(name) => SpawnTabDomain::DomainName(name.to_string()),
            None => SpawnTabDomain::DefaultDomain,
        },
        opts.new_tab,
    )? {
        return Ok(());
    }

    let gui = crate::frontend::try_new()?;
    let activity = Activity::new();

    promise::spawn::spawn(async move {
        if let Err(err) = async_run_terminal_gui(cmd, opts, publish.should_publish()).await {
            terminate_with_error(err);
        }
        drop(activity);
    })
    .detach();

    maybe_show_configuration_error_window();
    // Once the GUI can be handed work: the panels plugins add, if any
    // plugin is installed.
    crate::plugins::look_for_panels();

    // A debugging hook alongside THINKTERM_FRAME_DUMP: open the Settings
    // window on a given section once the GUI is up, so a page can be
    // captured from a scripted launch with no keyboard or mouse.
    if let Some(page) = std::env::var_os("THINKTERM_OPEN_SETTINGS") {
        promise::spawn::spawn_into_main_thread(async move {
            smol::Timer::after(std::time::Duration::from_millis(2500)).await;
            if page == "update" {
                crate::settings_window::show_update_page();
            } else {
                crate::settings_window::show();
            }
        })
        .detach();
    }
    // Likewise a plugin's panel, by the plugin's id: the right sidebar is
    // opened on it in every window.
    if let Some(plugin) = std::env::var_os("THINKTERM_OPEN_PANEL") {
        let plugin = plugin.to_string_lossy().into_owned();
        promise::spawn::spawn_into_main_thread(async move {
            smol::Timer::after(std::time::Duration::from_millis(2500)).await;
            if let Some(front_end) = crate::frontend::try_front_end() {
                for gui_window in front_end.gui_windows() {
                    let plugin = plugin.clone();
                    gui_window
                        .window
                        .notify(crate::termwindow::TermWindowNotif::Apply(Box::new(
                            move |term_window| term_window.show_plugin_panel(&plugin),
                        )));
                }
            }
        })
        .detach();
    }

    gui.run_forever()
}

fn fatal_toast_notification(title: &str, message: &str) {
    persistent_toast_notification(title, message);
    // We need a short delay otherwise the notification
    // will not show
    #[cfg(windows)]
    std::thread::sleep(std::time::Duration::new(2, 0));
}

fn notify_on_panic() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if env_bootstrap::panics_are_quiet() {
            return;
        }
        if let Some(s) = info.payload().downcast_ref::<&str>() {
            fatal_toast_notification("Wezterm panic", s);
        }
        default_hook(info);
    }));
}

fn terminate_with_error_message(err: &str) -> ! {
    log::error!("{}; terminating", err);
    fatal_toast_notification("Wezterm Error", &err);
    std::process::exit(1);
}

fn terminate_with_error(err: anyhow::Error) -> ! {
    let mut err_text = format!("{err:#}");

    let warnings = config::configuration_warnings_and_errors();
    if !warnings.is_empty() {
        let err = warnings.join("\n");
        err_text = format!("{err_text}\nConfiguration Error: {err}");
    }

    terminate_with_error_message(&err_text)
}

fn main() {
    #[cfg(feature = "dhat-heap")]
    let _profiler = dhat::Profiler::new_heap();

    config::designate_this_as_the_main_thread();
    config::assign_error_callback(mux::connui::show_configuration_error_message);
    notify_on_panic();
    i18n::install();
    if let Err(e) = run() {
        terminate_with_error(e);
    }
    Mux::shutdown();
    frontend::shutdown();
}

fn maybe_show_configuration_error_window() {
    let warnings = config::configuration_warnings_and_errors();
    if !warnings.is_empty() {
        let err = warnings.join("\n");
        mux::connui::show_configuration_error_message(&err);
    }
}

fn run_show_keys(config: config::ConfigHandle, cmd: &ShowKeysCommand) -> anyhow::Result<()> {
    let map = crate::inputmap::InputMap::new(&config);
    if cmd.lua {
        map.dump_config(cmd.key_table.as_deref());
    } else {
        map.show_keys();
    }
    Ok(())
}

pub fn run_ls_fonts(config: config::ConfigHandle, cmd: &LsFontsCommand) -> anyhow::Result<()> {
    use wezterm_font::parser::ParsedFont;

    if let Err(err) = config::configuration_result() {
        log::error!("{}", err);
        return Ok(());
    }

    // Disable the normal config error UI window, as we don't have
    // a fully baked GUI environment running
    config::assign_error_callback(|err| eprintln!("{}", err));

    let font_config = Rc::new(wezterm_font::FontConfiguration::new(
        Some(config.clone()),
        config.dpi.unwrap_or_else(|| ::window::default_dpi()) as usize,
    )?);

    let render_metrics = crate::utilsprites::RenderMetrics::new(&font_config)?;

    let bidi_hint = if config.bidi_enabled {
        Some(config.bidi_direction)
    } else {
        None
    };

    let unicode_version = config.unicode_version();

    let text = match (&cmd.text, &cmd.codepoints) {
        (Some(text), _) => Some(text.to_string()),
        (_, Some(codepoints)) => {
            let mut s = String::new();
            for cp in codepoints.split(",") {
                let cp = u32::from_str_radix(cp, 16)
                    .with_context(|| format!("{cp} is not a hex number"))?;
                let c = char::from_u32(cp)
                    .ok_or_else(|| anyhow!("{cp} is not a valid unicode codepoint value"))?;
                s.push(c);
            }
            Some(s)
        }
        _ => None,
    };

    if let Some(text) = &text {
        // Emulate the effect of output normalization
        let text = if config.normalize_output_to_unicode_nfc {
            text.nfc().collect()
        } else {
            text.to_string()
        };

        let line = Line::from_text(
            &text,
            &CellAttributes::default(),
            SEQ_ZERO,
            Some(&unicode_version),
        );
        let cell_clusters = line.cluster(bidi_hint);
        let ft_lib = wezterm_font::ftwrap::Library::new()?;

        let mut glyph_cache = GlyphCache::new_in_memory(&font_config, 256)?;

        for cluster in cell_clusters {
            let style = font_config.match_style(&config, &cluster.attrs);
            let font = font_config.resolve_font(style)?;
            let presentation_width = PresentationWidth::with_cluster(&cluster);
            let infos = font
                .blocking_shape(
                    &cluster.text,
                    Some(cluster.presentation),
                    cluster.direction,
                    None,
                    Some(&presentation_width),
                )
                .unwrap();

            // We must grab the handles after shaping, so that we get the
            // revised list that includes system fallbacks!
            let handles = font.clone_handles();
            let faces: Vec<_> = handles
                .iter()
                .map(|p| ft_lib.face_from_locator(&p.handle).ok())
                .collect();

            let mut iter = infos.iter().peekable();

            let mut byte_lens = vec![];
            for c in cluster.text.chars() {
                let len = c.len_utf8();
                for _ in 0..len {
                    byte_lens.push(len);
                }
            }
            println!("{:?}", cluster.direction);

            while let Some(info) = iter.next() {
                let idx = cluster.byte_to_cell_idx(info.cluster as usize);
                let followed_by_space = match line.get_cell(idx + 1) {
                    Some(cell) => cell.str() == " ",
                    None => false,
                };

                let text = if cluster.direction == Direction::LeftToRight {
                    if let Some(next) = iter.peek() {
                        line.columns_as_str(idx..cluster.byte_to_cell_idx(next.cluster as usize))
                    } else {
                        let last_idx = cluster.byte_to_cell_idx(cluster.text.len() - 1);
                        line.columns_as_str(idx..last_idx + 1)
                    }
                } else {
                    let info_len = byte_lens[info.cluster as usize];
                    let last_idx = cluster.byte_to_cell_idx(info.cluster as usize + info_len - 1);
                    line.columns_as_str(idx..last_idx + 1)
                };

                let parsed = &handles[info.font_idx];
                let escaped = format!("{}", text.escape_unicode());
                let mut is_custom = false;

                let cached_glyph = glyph_cache.cached_glyph(
                    &info,
                    &style,
                    followed_by_space,
                    &font,
                    &render_metrics,
                    info.num_cells,
                )?;

                let mut texture = cached_glyph.texture.clone();

                if config.custom_block_glyphs {
                    if let Some(block) = info.only_char.and_then(BlockKey::from_char) {
                        texture.replace(glyph_cache.cached_block(block, &render_metrics)?);
                        println!(
                            "{:2} {:4} {:12} drawn by wezterm because custom_block_glyphs=true: {:?}",
                            info.cluster, text, escaped, block
                        );
                        is_custom = true;
                    }
                }

                if !is_custom {
                    let glyph_name = faces[info.font_idx]
                        .as_ref()
                        .and_then(|face| {
                            face.get_glyph_name(info.glyph_pos)
                                .map(|name| format!("{},", name))
                        })
                        .unwrap_or_else(String::new);

                    println!(
                        "{:2} {:4} {:12} x_adv={:<2} cells={:<2} glyph={}{:<4} {}\n{:38}{}",
                        info.cluster,
                        text,
                        escaped,
                        cached_glyph.x_advance.get(),
                        info.num_cells,
                        glyph_name,
                        info.glyph_pos,
                        parsed.lua_name(),
                        "",
                        parsed.handle.diagnostic_string()
                    );
                }

                if cmd.rasterize_ascii {
                    let mut glyph = String::new();

                    if let Some(texture) = &cached_glyph.texture {
                        use ::window::bitmaps::ImageTexture;
                        if let Some(tex) = texture.texture.downcast_ref::<ImageTexture>() {
                            for y in texture.coords.min_y()..texture.coords.max_y() {
                                for &px in tex.image.borrow().horizontal_pixel_range(
                                    texture.coords.min_x() as usize,
                                    texture.coords.max_x() as usize,
                                    y as usize,
                                ) {
                                    let px = u32::from_be(px);
                                    let (b, g, r, a) = (
                                        (px >> 8) as u8,
                                        (px >> 16) as u8,
                                        (px >> 24) as u8,
                                        (px & 0xff) as u8,
                                    );
                                    // Use regular RGB for other terminals, but then
                                    // set RGBA for wezterm
                                    glyph.push_str(&format!(
                                "\x1b[38:2::{r}:{g}:{b}m\x1b[38:6::{r}:{g}:{b}:{a}m\u{2588}\x1b[0m"
                            ));
                                }
                                glyph.push('\n');
                            }
                        }
                    }

                    if !is_custom {
                        println!(
                            "bearing: x={} y={}, offset: x={} y={}",
                            cached_glyph.bearing_x.get(),
                            cached_glyph.bearing_y.get(),
                            cached_glyph.x_offset.get(),
                            cached_glyph.y_offset.get(),
                        );
                    }
                    println!("{glyph}");
                }
            }
        }
        return Ok(());
    }

    println!("Primary font:");
    let default_font = font_config.default_font()?;
    println!(
        "{}",
        ParsedFont::lua_fallback(&default_font.clone_handles())
    );
    println!();

    for rule in &config.font_rules {
        println!();

        let mut condition = "When".to_string();
        if let Some(intensity) = &rule.intensity {
            condition.push_str(&format!(" Intensity={:?}", intensity));
        }
        if let Some(underline) = &rule.underline {
            condition.push_str(&format!(" Underline={:?}", underline));
        }
        if let Some(italic) = &rule.italic {
            condition.push_str(&format!(" Italic={:?}", italic));
        }
        if let Some(blink) = &rule.blink {
            condition.push_str(&format!(" Blink={:?}", blink));
        }
        if let Some(rev) = &rule.reverse {
            condition.push_str(&format!(" Reverse={:?}", rev));
        }
        if let Some(strikethrough) = &rule.strikethrough {
            condition.push_str(&format!(" Strikethrough={:?}", strikethrough));
        }
        if let Some(invisible) = &rule.invisible {
            condition.push_str(&format!(" Invisible={:?}", invisible));
        }

        println!("{}:", condition);
        let font = font_config.resolve_font(&rule.font)?;
        println!("{}", ParsedFont::lua_fallback(&font.clone_handles()));
        println!();
    }

    println!("Title font:");
    let title_font = font_config.title_font()?;
    println!("{}", ParsedFont::lua_fallback(&title_font.clone_handles()));
    println!();

    if cmd.list_system {
        let font_dirs = font_config.list_fonts_in_font_dirs();
        println!(
            "{} fonts found in your font_dirs + built-in fonts:",
            font_dirs.len()
        );
        for font in font_dirs {
            let pixel_sizes = if font.pixel_sizes.is_empty() {
                "".to_string()
            } else {
                format!(" pixel_sizes={:?}", font.pixel_sizes)
            };
            println!(
                "{} -- {}{}{}",
                font.lua_name(),
                font.aka(),
                font.handle.diagnostic_string(),
                pixel_sizes
            );
        }

        match font_config.list_system_fonts() {
            Ok(sys_fonts) => {
                println!(
                    "{} system fonts found using {:?}:",
                    sys_fonts.len(),
                    config.font_locator
                );
                for font in sys_fonts {
                    let pixel_sizes = if font.pixel_sizes.is_empty() {
                        "".to_string()
                    } else {
                        format!(" pixel_sizes={:?}", font.pixel_sizes)
                    };
                    println!(
                        "{} -- {}{}{}",
                        font.lua_name(),
                        font.aka(),
                        font.handle.diagnostic_string(),
                        pixel_sizes
                    );
                }
            }
            Err(err) => log::error!("Unable to list system fonts: {}", err),
        }
    }

    Ok(())
}

fn run() -> anyhow::Result<()> {
    // Inform the system of our AppUserModelID.
    // Without this, our toast notifications won't be correctly
    // attributed to our application.
    #[cfg(windows)]
    {
        unsafe {
            ::windows::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID(
                ::windows::core::PCWSTR(wide_string("com.roversx.thinkterm").as_ptr()),
            )
            .unwrap();
        }
    }

    let opts = Opt::parse();

    // This is a bit gross.
    // In order to not to automatically open a standard windows console when
    // we run, we use the windows_subsystem attribute at the top of this
    // source file.  That comes at the cost of causing the help output
    // to disappear if we are actually invoked from a console.
    // This AttachConsole call will attach us to the console of the parent
    // in that situation, but since we were launched as a windows subsystem
    // application we will be running asynchronously from the shell in
    // the command window, which means that it will appear to the user
    // that we hung at the end, when in reality the shell is waiting for
    // input but didn't know to re-draw the prompt.
    #[cfg(windows)]
    unsafe {
        if opts.attach_parent_console {
            winapi::um::wincon::AttachConsole(winapi::um::wincon::ATTACH_PARENT_PROCESS);
        }
    };

    env_bootstrap::bootstrap();
    // window_funcs is not set up by env_bootstrap as window_funcs is
    // GUI environment specific and env_bootstrap is used to setup the
    // headless mux server.
    config::lua::add_context_setup_func(window_funcs::register);
    config::lua::add_context_setup_func(crate::scripting::register);
    config::lua::add_context_setup_func(crate::stats::register);

    stats::Stats::init()?;
    let _saver = umask::UmaskSaver::new();

    config::common_init(
        opts.config_file.as_ref(),
        &opts.config_override,
        opts.skip_config,
    )?;
    let config = config::configuration();
    if let Some(value) = &config.default_ssh_auth_sock {
        std::env::set_var("SSH_AUTH_SOCK", value);
    }

    let sub = match opts.cmd.as_ref().cloned() {
        Some(SubCommand::BlockingStart(start)) => {
            // Act as if the normal start subcommand was used,
            // except that we always start a new instance.
            // This is needed for compatibility, because many tools assume
            // that "$TERMINAL -e $COMMAND" blocks until the command finished.
            SubCommand::Start(StartCommand {
                always_new_process: true,
                ..start
            })
        }
        Some(sub) => sub,
        None => {
            // Need to fake an argv0
            let mut argv = vec!["thinkterm-gui".to_string()];
            for a in &config.default_gui_startup_args {
                argv.push(a.clone());
            }
            SubCommand::try_parse_from(&argv).with_context(|| {
                format!(
                    "parsing the default_gui_startup_args config: {:?}",
                    config.default_gui_startup_args
                )
            })?
        }
    };

    match sub {
        SubCommand::Start(start) => {
            log::trace!("Using configuration: {:#?}\nopts: {:#?}", config, opts);
            let res = run_terminal_gui(start, None);
            wezterm_blob_leases::clear_storage();
            res
        }
        SubCommand::BlockingStart(_) => unreachable!(),
        SubCommand::Ssh(ssh) => run_ssh(ssh),
        SubCommand::Serial(serial) => run_serial(config, serial),
        SubCommand::Connect(connect) => run_terminal_gui(
            StartCommand {
                domain: Some(connect.domain_name.clone()),
                class: connect.class,
                workspace: connect.workspace,
                position: connect.position,
                prog: connect.prog,
                new_tab: connect.new_tab,
                always_new_process: true,
                attach: true,
                _cmd: false,
                no_auto_connect: false,
                cwd: None,
            },
            Some(connect.domain_name),
        ),
        SubCommand::LsFonts(cmd) => run_ls_fonts(config, &cmd),
        SubCommand::ShowKeys(cmd) => run_show_keys(config, &cmd),
    }
}
