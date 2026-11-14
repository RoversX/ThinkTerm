//! `thinkterm plugin`: the plugin host from the command line, for looking
//! at plugins and trying one out while writing it
//! (docs/thinkterm/plugins.md).

use anyhow::{anyhow, Context};
use clap::{Parser, Subcommand};
use serde_json::Value;
use std::time::Duration;
use thinkterm_plugin_channel::client::{Host, Session, CALL_TIMEOUT};
use thinkterm_plugin_channel::registry::{self as api, Does, Info, State};
use thinkterm_plugin_panel::{At, Terminal};

#[derive(Debug, Parser, Clone)]
pub struct PluginCommand {
    #[command(subcommand)]
    sub: Sub,
}

#[derive(Debug, Subcommand, Clone)]
enum Sub {
    /// List the plugins and their states
    List,
    /// Turn a plugin on. A new one is let run from where it is installed,
    /// which nothing else does
    Enable { id: String },
    /// Turn a plugin off
    Disable { id: String },
    /// Stop a plugin and read its manifest again; every installed plugin
    /// when no id is given
    Reload { id: Option<String> },
    /// Send a plugin a call, and print its answer
    Call {
        id: String,
        #[arg(value_name = "JSON")]
        body: String,
    },
    /// Do one of a plugin's actions, as the command palette and a key
    /// bound to it do; `thinkterm plugin list` shows each plugin's
    Do {
        /// The plugin, by id
        plugin: String,
        /// The action, by its id in the plugin's manifest
        action: String,
        /// The pane the action is asked for at.
        /// The default is to use the current pane based on the
        /// environment variable WEZTERM_PANE.
        #[arg(long)]
        pane_id: Option<u64>,
    },
    /// Print the directory plugins are installed in
    Dir,
}

impl PluginCommand {
    pub fn run(&self) -> anyhow::Result<()> {
        if let Sub::Dir = self.sub {
            println!(
                "{}",
                thinkterm_plugin_channel::paths::plugins_dir().display()
            );
            return Ok(());
        }
        let host = Host::for_this_build().context("finding the plugin host")?;
        let session = Session::start(host, |_| {}).context("starting a plugin session")?;
        match &self.sub {
            Sub::List => {
                let listed = manage(&session, api::Request::List { locale: locale() })?;
                let listed: Vec<Info> = serde_json::from_value(listed)?;
                print_list(&listed).map_err(crate::stdout_error)?;
            }
            Sub::Enable { id } | Sub::Disable { id } => {
                let enabled = matches!(self.sub, Sub::Enable { .. });
                manage(
                    &session,
                    api::Request::SetEnabled {
                        id: id.clone(),
                        enabled,
                    },
                )?;
                println!("{id} is {}", if enabled { "on" } else { "off" });
            }
            Sub::Reload { id } => {
                manage(&session, api::Request::Reload { id: id.clone() })?;
                println!(
                    "reloaded {}",
                    id.as_deref().unwrap_or("every installed plugin")
                );
            }
            Sub::Call { id, body } => {
                let body: Value = serde_json::from_str(body).context("the call is not JSON")?;
                let answer = ask(&session, id, body, CALL_TIMEOUT)?;
                println!("{}", serde_json::to_string_pretty(&answer)?);
            }
            Sub::Do {
                plugin,
                action,
                pane_id,
            } => {
                let listed = manage(&session, api::Request::List { locale: locale() })?;
                let listed: Vec<Info> = serde_json::from_value(listed)?;
                let info = listed
                    .iter()
                    .find(|info| info.id == *plugin)
                    .ok_or_else(|| anyhow!("there is no plugin {plugin:?}"))?;
                let found = info
                    .actions
                    .iter()
                    .find(|known| known.id == *action)
                    .ok_or_else(|| anyhow!("the plugin {plugin:?} has no action {action:?}"))?;
                match &found.does {
                    Does::Call(body) => {
                        let body = at(*pane_id).added_to(body.clone());
                        let answer = ask(&session, plugin, body, CALL_TIMEOUT)?;
                        println!("{}", serde_json::to_string_pretty(&answer)?);
                    }
                    Does::Page(args) => open_page(info, args, *pane_id)?,
                }
            }
            Sub::Dir => unreachable!("answered above"),
        }
        Ok(())
    }
}

/// Where an action asked for here is asked for at: this directory, and the
/// pane named, else the one this runs in.
fn at(pane_id: Option<u64>) -> At {
    let pane = pane_id.or_else(|| std::env::var("WEZTERM_PANE").ok()?.parse().ok());
    At {
        cwd: std::env::current_dir()
            .ok()
            .map(|dir| dir.to_string_lossy().into_owned()),
        remote: None,
        terminal: pane.map(|pane| Terminal {
            pane,
            socket: std::env::var("WEZTERM_UNIX_SOCKET")
                .ok()
                .filter(|socket| !socket.is_empty()),
        }),
        // Asked for in a terminal: no page is in focus.
        page: None,
    }
}

/// Opens plugin `info`'s page with `args`, as `thinkterm cli open-page`
/// does, which finds the ThinkTerm to open it in.
fn open_page(info: &Info, args: &Value, pane_id: Option<u64>) -> anyhow::Result<()> {
    let program = std::env::current_exe().context("finding this program")?;
    let mut command = std::process::Command::new(program);
    command
        .args(["cli", "open-page", &info.id, "--title", &info.name])
        .args(["--args", &args.to_string()]);
    if let Some(page) = &info.page {
        command.args(["--icon", &page.icon]);
    }
    if let Some(pane_id) = pane_id {
        command.args(["--pane-id", &pane_id.to_string()]);
    }
    let status = command.status().context("running `thinkterm cli open-page`")?;
    anyhow::ensure!(status.success(), "the page was not opened");
    Ok(())
}

fn ask(session: &Session, plugin: &str, body: Value, wait: Duration) -> anyhow::Result<Value> {
    let (tx, rx) = std::sync::mpsc::channel();
    session.call_within(plugin, body, wait, move |answer| {
        let _ = tx.send(answer);
    });
    rx.recv()
        .context("the plugin session ended")?
        .map_err(|why| anyhow!(why))
}

fn manage(session: &Session, request: api::Request) -> anyhow::Result<Value> {
    ask(
        session,
        api::PLUGIN,
        serde_json::to_value(request)?,
        CALL_TIMEOUT,
    )
}

/// The language plugins name themselves in, from the environment:
/// `zh_CN.UTF-8` is `zh-CN`.
fn locale() -> String {
    ["LC_ALL", "LC_MESSAGES", "LANG"]
        .iter()
        .filter_map(|name| std::env::var(name).ok())
        .find(|value| !value.is_empty())
        .map(|value| {
            let tag = value.split(['.', '@']).next().unwrap_or_default();
            match tag {
                "C" | "POSIX" => String::new(),
                tag => tag.replace('_', "-"),
            }
        })
        .unwrap_or_default()
}

fn state_label(info: &Info) -> &'static str {
    match info.state {
        State::New => "new",
        State::Off => "off",
        State::Idle => "idle",
        State::Starting => "starting",
        State::Running => "running",
        State::Crashed { .. } => "crashed",
        State::Failed { .. } => "failed",
        State::Invalid { .. } => "invalid",
        State::Unsupported { .. } => "unsupported",
    }
}

fn print_list(listed: &[Info]) -> std::io::Result<()> {
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    let id_width = listed
        .iter()
        .map(|info| info.id.chars().count())
        .max()
        .unwrap_or(0)
        .max(2);
    writeln!(
        out,
        "{:id_width$}  {:11}  {:10}  NAME",
        "ID", "STATE", "VERSION"
    )?;
    for info in listed {
        let version = if info.builtin {
            "built in"
        } else if info.version.is_empty() {
            "-"
        } else {
            &info.version
        };
        writeln!(
            out,
            "{:id_width$}  {:11}  {:10}  {}",
            info.id,
            state_label(info),
            version,
            info.name
        )?;
        if let Some(reason) = info.state.reason() {
            writeln!(out, "{:id_width$}  {reason}", "")?;
        }
        if info.state == State::New {
            writeln!(
                out,
                "{:id_width$}  never let run: `thinkterm plugin enable {}` lets it",
                "", info.id
            )?;
        }
        for action in &info.actions {
            writeln!(
                out,
                "{:id_width$}  does {}: {}",
                "", action.id, action.title
            )?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_locale_comes_from_the_environment_as_a_tag() {
        // Only the shape: the variables belong to whoever runs the tests.
        let locale = super::locale();
        assert!(!locale.contains('_') && !locale.contains('.'), "{}", locale);
    }
}
