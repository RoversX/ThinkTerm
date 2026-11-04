use anyhow::{anyhow, Context};
use clap::Parser;
use std::ffi::OsString;
use std::io::{IsTerminal, Write};
use wezterm_client::client::Client;

mod activate_pane;
mod activate_pane_direction;
mod activate_tab;
mod adjust_pane_size;
mod agent;
mod color_schemes;
mod events;
mod get_pane_direction;
mod get_text;
mod kill_pane;
mod list;
mod list_clients;
mod move_pane_to_new_tab;
mod proxy;
mod rename_workspace;
mod restore_output;
mod reverse;
mod send_key;
mod send_text;
mod set_pane_label;
mod set_tab_title;
mod set_window_title;
mod spawn_command;
mod split_pane;
mod stop_server;
mod target;
mod tls_creds;
mod wait;
mod web_server;
mod web_token;
mod share;
mod share_watch;
mod zoom_pane;

#[derive(Debug, Parser, Clone, Copy)]
enum CliOutputFormatKind {
    #[command(name = "table", about = "multi line space separated table")]
    Table,
    #[command(name = "json", about = "JSON format")]
    Json,
}

impl std::str::FromStr for CliOutputFormatKind {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<CliOutputFormatKind, Self::Err> {
        match s {
            "json" => Ok(CliOutputFormatKind::Json),
            "table" => Ok(CliOutputFormatKind::Table),
            _ => Err(anyhow::anyhow!("unknown output format")),
        }
    }
}

#[derive(Debug, Parser, Clone, Copy)]
struct CliOutputFormat {
    /// Controls the output format.
    /// "table" and "json" are possible formats.
    #[arg(long = "format", default_value = "table")]
    format: CliOutputFormatKind,
}

/// What `--pane` named is not there, or could be more than one pane: the
/// command exits with status 3.
#[derive(Debug)]
pub struct NoSuchTarget(pub String);

impl std::fmt::Display for NoSuchTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for NoSuchTarget {}

/// The server could not be reached, or will not talk to this version: the
/// command exits with status 4.
#[derive(Debug)]
pub struct ServerUnreachable;

impl std::fmt::Display for ServerUnreachable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("could not reach the ThinkTerm server")
    }
}

impl std::error::Error for ServerUnreachable {}

/// The command line asks for something that cannot be: the command exits
/// with status 2, as when the arguments do not parse.
#[derive(Debug)]
pub struct UsageError(pub String);

impl std::fmt::Display for UsageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for UsageError {}

/// The status a command that failed with `err` exits with: 2 for a command
/// line that cannot be, 3 for a pane that is not there or not one, 4 for a
/// server out of reach, and 1 for anything else. A command that waits
/// exits with what it waited for instead.
pub fn exit_status(err: &anyhow::Error) -> i32 {
    if err.downcast_ref::<UsageError>().is_some() {
        2
    } else if err.downcast_ref::<NoSuchTarget>().is_some() || is_no_such_pane(err) {
        3
    } else if err.downcast_ref::<ServerUnreachable>().is_some() {
        4
    } else {
        1
    }
}

/// The error for a pane id the listing does not have.
pub(crate) fn no_such_pane(pane_id: mux::pane::PaneId) -> anyhow::Error {
    NoSuchTarget(format!("there is no pane {pane_id}")).into()
}

/// Whether `err` is the server's answer about a pane id it has no record
/// of, in any of the ways its handlers word it. Any other failure -- a
/// connection dropping, a GUI handing over -- is not.
pub(crate) fn is_no_such_pane(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        let text = cause.to_string();
        text.contains("no such pane")
            || pane_id_in(&text, "pane ", " not found")
            || pane_id_in(&text, "pane_id ", " invalid")
    })
}

/// Whether `text` holds `before`, a pane id, then `after`.
fn pane_id_in(text: &str, before: &str, after: &str) -> bool {
    text.match_indices(before).any(|(at, _)| {
        let rest = &text[at + before.len()..];
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        digits > 0 && rest[digits..].starts_with(after)
    })
}

/// Text a program in a pane chose (a title, a user var, a notification),
/// made safe to print to a terminal: control characters are written out
/// rather than sent, so none of them reaches the terminal running this.
pub(crate) fn printable(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if c.is_control() {
            out.extend(c.escape_default());
        } else {
            out.push(c);
        }
    }
    out
}

/// Print `value` as JSON: indented for a person at a terminal, on one line
/// for a program reading it.
pub(crate) fn print_json<T: serde::Serialize>(value: &T) -> anyhow::Result<()> {
    let out = std::io::stdout();
    let mut out = out.lock();
    if out.is_terminal() {
        serde_json::to_writer_pretty(&mut out, value)
    } else {
        serde_json::to_writer(&mut out, value)
    }
    .map_err(crate::stdout_error)?;
    writeln!(out).map_err(crate::stdout_error)
}

#[derive(Debug, Parser, Clone)]
pub struct CliCommand {
    /// Don't automatically start the server
    #[arg(long = "no-auto-start")]
    no_auto_start: bool,

    /// Prefer connecting to a background mux server.
    /// The default is to prefer connecting to a running
    /// ThinkTerm GUI instance
    #[arg(long = "prefer-mux")]
    prefer_mux: bool,

    /// When connecting to a gui instance, if you started the
    /// gui with `--class SOMETHING`, you should also pass
    /// that same value here in order for the client to find
    /// the correct gui instance.
    #[arg(long = "class")]
    class: Option<String>,

    #[command(subcommand)]
    sub: CliSubCommand,
}

#[derive(Debug, Parser, Clone)]
enum CliSubCommand {
    #[command(name = "list", about = "list windows, tabs and panes")]
    List(list::ListCommand),

    #[command(name = "list-clients", about = "list clients")]
    ListClients(list_clients::ListClientsCommand),

    #[command(name = "proxy", about = "start rpc proxy pipe")]
    Proxy(proxy::ProxyCommand),

    #[command(name = "connect-reverse-proxy", hide = true)]
    ConnectReverseProxy(reverse::Proxy),

    #[command(name = "tlscreds", about = "obtain tls credentials")]
    TlsCreds(tls_creds::TlsCredsCommand),

    /// Mint, list and revoke the tokens a browser presents at the
    /// server's web port
    #[command(name = "web-token", rename_all = "kebab")]
    WebToken(web_token::WebTokenCommand),

    /// Your shared panes, and the panes other accounts on this machine share
    /// with you. A pane is shared from the share button in ThinkTerm.
    #[command(name = "share", rename_all = "kebab")]
    Share(share::ShareCommand),

    /// Turn the browser listener on and off, and see where it is accepting
    #[command(name = "web-server", rename_all = "kebab")]
    WebServer(web_server::WebServerCommand),

    #[command(
        name = "move-pane-to-new-tab",
        rename_all = "kebab",
        about = "Move a pane into a new tab"
    )]
    MovePaneToNewTab(move_pane_to_new_tab::MovePaneToNewTab),

    #[command(
        name = "split-pane",
        rename_all = "kebab",
        trailing_var_arg = true,
        about = "split the current pane.
Outputs the pane-id for the newly created pane on success"
    )]
    SplitPane(split_pane::SplitPane),

    #[command(
        name = "spawn",
        trailing_var_arg = true,
        about = "Spawn a command into a new window or tab
Outputs the pane-id for the newly created pane on success"
    )]
    SpawnCommand(spawn_command::SpawnCommand),

    /// Send text to a pane as though it were pasted.
    /// If bracketed paste mode is enabled in the pane, then the
    /// text will be sent as a bracketed paste. No Enter is added:
    /// to submit the text, send a carriage return separately with
    /// `--no-paste`.
    #[command(name = "send-text", rename_all = "kebab")]
    SendText(send_text::SendText),

    /// Press keys in a pane by name -- `enter`, `ctrl+c`, `up` -- encoded
    /// the way the program there asked for keys, as if typed.
    /// With `--wait`, wait for the command they start in the pane's shell
    /// and exit with its status.
    #[command(name = "send-key", rename_all = "kebab")]
    SendKey(send_key::SendKey),

    /// Wait for a pane's program to end, or with `--command` for the
    /// command running in its shell, and exit with its status
    #[command(name = "wait", rename_all = "kebab")]
    Wait(wait::WaitCommand),

    /// Print what happens as it happens -- panes opening and closing,
    /// programs and commands finishing, bells, notifications, titles,
    /// agent status -- until interrupted
    #[command(name = "events", rename_all = "kebab")]
    Events(events::EventsCommand),

    /// Retrieves the textual content of a pane and output it to stdout
    #[command(name = "get-text", rename_all = "kebab")]
    GetText(get_text::GetText),

    /// Activate an adjacent pane in the specified direction.
    #[command(name = "activate-pane-direction", rename_all = "kebab")]
    ActivatePaneDirection(activate_pane_direction::ActivatePaneDirection),

    /// Determine the adjacent pane in the specified direction.
    ///
    /// Prints the pane id in that direction, or nothing if there
    /// is no pane in that direction.
    #[command(name = "get-pane-direction", rename_all = "kebab")]
    GetPaneDirection(get_pane_direction::GetPaneDirection),

    /// Kill a pane
    #[command(name = "kill-pane", rename_all = "kebab")]
    KillPane(kill_pane::KillPane),

    /// Activate (focus) a pane
    #[command(name = "activate-pane", rename_all = "kebab")]
    ActivatePane(activate_pane::ActivatePane),

    /// Adjust the size of a pane directionally
    #[command(name = "adjust-pane-size", rename_all = "kebab")]
    AdjustPaneSize(adjust_pane_size::CliAdjustPaneSize),

    /// Activate a tab
    #[command(name = "activate-tab", rename_all = "kebab")]
    ActivateTab(activate_tab::ActivateTab),

    /// Change the title of a tab
    #[command(name = "set-tab-title", rename_all = "kebab")]
    SetTabTitle(set_tab_title::SetTabTitle),

    /// Label a pane, so that `--pane LABEL` finds it; an empty label takes
    /// the label away
    #[command(name = "set-pane-label", rename_all = "kebab")]
    SetPaneLabel(set_pane_label::SetPaneLabel),

    /// Change the title of a window
    #[command(name = "set-window-title", rename_all = "kebab")]
    SetWindowTitle(set_window_title::SetWindowTitle),

    /// Rename a workspace
    #[command(name = "rename-workspace", rename_all = "kebab")]
    RenameWorkspace(rename_workspace::RenameWorkspace),

    /// Zoom, unzoom, or toggle zoom state
    #[command(name = "zoom-pane", rename_all = "kebab")]
    ZoomPane(zoom_pane::ZoomPane),

    /// Query agent statuses
    #[command(name = "agent")]
    Agent(agent::AgentCommand),

    /// List the built-in color schemes and their colors
    #[command(name = "color-schemes", rename_all = "kebab")]
    ColorSchemes(color_schemes::ColorSchemesCommand),

    /// Stop the background session server; the terminals in it end
    #[command(name = "stop-server")]
    StopServer(stop_server::StopServerCommand),

    /// Whether this machine keeps pane output to restore after its
    /// terminals stop
    #[command(name = "restore-output")]
    RestoreOutput(restore_output::RestoreOutputCommand),
}

/// The window class `thinkterm cli` was asked to find, for reaching the
/// window this machine has open as well as the server it asks.
static WINDOW_CLASS: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Whether the window's Settings let the programs here reach the terminals
/// of its servers at all, as its settings file says: unless they do, there
/// is nothing to ask it about them.
pub(crate) fn servers_terminals_reachable() -> bool {
    #[derive(serde::Deserialize, Default)]
    #[serde(default)]
    struct Agents {
        server_terminals: String,
    }
    #[derive(serde::Deserialize, Default)]
    #[serde(default)]
    struct Settings {
        agents: Agents,
    }
    std::fs::read(config::native_settings_path())
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Settings>(&bytes).ok())
        .is_some_and(|settings| {
            matches!(settings.agents.server_terminals.as_str(), "read" | "type")
        })
}

/// How long the window is waited on for what its servers have: a window
/// busy starting or reloading does not hold up what this machine has.
const WINDOW_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

/// `ask` of the window, given `WINDOW_WAIT` to answer; `None` past it.
pub(crate) async fn within_window_wait<T>(
    ask: impl std::future::Future<Output = Option<T>>,
) -> Option<T> {
    smol::future::or(ask, async {
        smol::Timer::after(WINDOW_WAIT).await;
        None
    })
    .await
}

/// The window this machine has open, which is connected to the servers whose
/// panes the programs here may reach as far as its Settings say; `None`
/// without one.
pub(crate) async fn window_client() -> Option<Client> {
    let class = WINDOW_CLASS
        .get()
        .map(String::as_str)
        .unwrap_or(wezterm_gui_subcommands::DEFAULT_WINDOW_CLASS);
    let socket_path = wezterm_client::discovery::resolve_gui_sock_path(class).ok()?;
    let domain = config::UnixDomain {
        socket_path: Some(socket_path),
        no_serve_automatically: true,
        ..Default::default()
    };
    let mut ui = mux::connui::ConnectionUI::new_headless();
    let client = Client::new_unix_domain(None, &domain, true, &mut ui, true).ok()?;
    client.verify_version_compat(&ui).await.ok()?;
    Some(client)
}

async fn run_cli_async(opts: &crate::Opt, cli: CliCommand) -> anyhow::Result<()> {
    if let Some(class) = &cli.class {
        let _ = WINDOW_CLASS.set(class.clone());
    }
    // The scheme table is compiled in; asking a server for it (or starting
    // one) would only get in the way.
    if let CliSubCommand::ColorSchemes(cmd) = &cli.sub {
        return cmd.run();
    }
    // Stopping must not start one: handled before a client is made.
    if let CliSubCommand::StopServer(cmd) = &cli.sub {
        return cmd.run(&crate::init_config(opts)?);
    }
    // A switch in a file on this machine, which the server reads when it
    // needs it: nothing to ask a server.
    if let CliSubCommand::RestoreOutput(cmd) = &cli.sub {
        return cmd.run();
    }
    // Watching asks the servers of the accounts that share, not our own.
    if let CliSubCommand::Share(cmd) = &cli.sub {
        if cmd.is_viewer() {
            return cmd.run_viewer().await;
        }
    }

    let mut ui = mux::connui::ConnectionUI::new_headless();
    let initial = true;

    // The browser port and its tokens belong to the mux server; a GUI's
    // in-process mux has neither. Asking the GUI (the default, for every
    // other command) answered "not accepting browser clients" while the
    // server was.
    let prefer_mux = cli.prefer_mux
        || matches!(
            cli.sub,
            CliSubCommand::WebServer(_) | CliSubCommand::WebToken(_) | CliSubCommand::Share(_)
        );
    // HTML takes its colours from the server's palette, which the server
    // sends unasked as the client registers: listened for before that.
    // Only for it, since one viewport push per tab goes out ahead of it.
    let palette_pushes = match &cli.sub {
        CliSubCommand::GetText(cmd) if cmd.wants_palette() => Some(
            wezterm_client::client::tap_unilateral_pdus_where(4, |pdu| {
                matches!(pdu, codec::Pdu::DefaultPalette(_))
            }),
        ),
        _ => None,
    };
    let client = Client::new_default_unix_domain(
        initial,
        &mut ui,
        cli.no_auto_start,
        prefer_mux,
        cli.class
            .as_deref()
            .unwrap_or(wezterm_gui_subcommands::DEFAULT_WINDOW_CLASS),
    )
    .context(ServerUnreachable)?;

    // Every ordinary RPC is deferred behind the client registration barrier
    // (see PduRegistrationBarrier) until the bootstrap handshake completes,
    // and nothing else on this path runs it -- without this, every
    // subcommand parks forever on a healthy server. The proxy is exempt: it
    // discards this client and registers its own raw connection with
    // `is_proxy: true`.
    if !matches!(cli.sub, CliSubCommand::Proxy(_)) {
        client
            .verify_version_compat(&ui)
            .await
            .context(ServerUnreachable)?;
    }

    match cli.sub {
        CliSubCommand::ListClients(cmd) => cmd.run(client).await,
        CliSubCommand::List(cmd) => cmd.run(client).await,
        CliSubCommand::MovePaneToNewTab(cmd) => cmd.run(client).await,
        CliSubCommand::SplitPane(cmd) => cmd.run(client).await,
        CliSubCommand::SendText(cmd) => cmd.run(client).await,
        CliSubCommand::SendKey(cmd) => cmd.run(client).await,
        CliSubCommand::Wait(cmd) => cmd.run(client).await,
        CliSubCommand::Events(cmd) => cmd.run(client).await,
        CliSubCommand::GetText(cmd) => cmd.run(client, palette_pushes).await,
        CliSubCommand::SpawnCommand(cmd) => cmd.run(client, &crate::init_config(opts)?).await,
        CliSubCommand::Proxy(cmd) => cmd.run(client, &crate::init_config(opts)?).await,
        CliSubCommand::ConnectReverseProxy(cmd) => cmd.run(client).await,
        CliSubCommand::TlsCreds(cmd) => cmd.run(client).await,
        CliSubCommand::WebServer(cmd) => cmd.run(client).await,
        CliSubCommand::WebToken(cmd) => cmd.run(client).await,
        CliSubCommand::Share(cmd) => cmd.run(client).await,
        CliSubCommand::ActivatePaneDirection(cmd) => cmd.run(client).await,
        CliSubCommand::GetPaneDirection(cmd) => cmd.run(client).await,
        CliSubCommand::KillPane(cmd) => cmd.run(client).await,
        CliSubCommand::ActivatePane(cmd) => cmd.run(client).await,
        CliSubCommand::AdjustPaneSize(cmd) => cmd.run(client).await,
        CliSubCommand::ActivateTab(cmd) => cmd.run(client).await,
        CliSubCommand::SetTabTitle(cmd) => cmd.run(client).await,
        CliSubCommand::SetPaneLabel(cmd) => cmd.run(client).await,
        CliSubCommand::SetWindowTitle(cmd) => cmd.run(client).await,
        CliSubCommand::RenameWorkspace(cmd) => cmd.run(client).await,
        CliSubCommand::ZoomPane(cmd) => cmd.run(client).await,
        CliSubCommand::Agent(cmd) => cmd.run(client).await,
        CliSubCommand::ColorSchemes(_)
        | CliSubCommand::StopServer(_)
        | CliSubCommand::RestoreOutput(_) => {
            unreachable!("handled without a client above")
        }
    }
}

pub fn run_cli(opts: &crate::Opt, cli: CliCommand) -> anyhow::Result<()> {
    let executor = promise::spawn::ScopedExecutor::new();
    match promise::spawn::block_on(executor.run(async move { run_cli_async(opts, cli).await })) {
        Ok(_) => Ok(()),
        Err(err) => crate::terminate_with_error(err),
    }
}

pub fn resolve_relative_cwd(cwd: Option<OsString>) -> anyhow::Result<Option<String>> {
    match cwd {
        None => Ok(None),
        Some(cwd) => Ok(Some(
            std::env::current_dir()?
                .join(cwd)
                .to_str()
                .ok_or_else(|| anyhow!("path is not representable as String"))?
                .to_string(),
        )),
    }
}
