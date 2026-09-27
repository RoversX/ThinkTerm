//! `thinkterm plugin`: the plugin host from the command line, for looking
//! at plugins and trying one out while writing it
//! (docs/thinkterm/plugins.md).

use anyhow::{anyhow, Context};
use clap::{Parser, Subcommand};
use serde_json::Value;
use std::time::Duration;
use thinkterm_plugin_channel::client::{Host, Session, CALL_TIMEOUT};
use thinkterm_plugin_channel::registry::{self as api, Info, State};

#[derive(Debug, Parser, Clone)]
pub struct PluginCommand {
    #[command(subcommand)]
    sub: Sub,
}

#[derive(Debug, Subcommand, Clone)]
enum Sub {
    /// List the plugins and their states
    List,
    /// Turn a plugin on
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
                print_list(&listed);
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
            Sub::Dir => unreachable!("answered above"),
        }
        Ok(())
    }
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

fn print_list(listed: &[Info]) {
    let id_width = listed
        .iter()
        .map(|info| info.id.chars().count())
        .max()
        .unwrap_or(0)
        .max(2);
    println!("{:id_width$}  {:11}  {:10}  NAME", "ID", "STATE", "VERSION");
    for info in listed {
        let version = if info.builtin {
            "built in"
        } else if info.version.is_empty() {
            "-"
        } else {
            &info.version
        };
        println!(
            "{:id_width$}  {:11}  {:10}  {}",
            info.id,
            state_label(info),
            version,
            info.name
        );
        if let Some(reason) = info.state.reason() {
            println!("{:id_width$}  {reason}", "");
        }
    }
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
