//! `thinkterm plugin`: the plugin host from the command line, for looking
//! at plugins and trying one out while writing it
//! (docs/thinkterm/plugins.md).

use anyhow::{anyhow, Context};
use clap::{Parser, Subcommand};
use serde_json::{json, Value};
use std::path::Path;
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
    /// Make a plugin to start from in the plugins directory, and turn it
    /// on: a panel and a page drawn by a Python script, main.py, which the
    /// Python on this machine runs
    New {
        /// Its id, which names its directory: a-z, 0-9 and "-"
        id: String,
        /// What it is called; made from the id when not given
        #[arg(long)]
        name: Option<String>,
        /// Leave it off: it runs once `thinkterm plugin enable` turns it on
        #[arg(long)]
        off: bool,
    },
    /// Draw a plugin's panel or page, open in ThinkTerm, into a PNG of that
    /// view alone, as ThinkTerm shows it, and print where the PNG is
    Shot {
        /// The plugin, by id
        id: String,
        /// Its page, when its panel is open too
        #[arg(long, conflicts_with = "panel")]
        page: bool,
        /// Its panel, when a page of it is open too
        #[arg(long)]
        panel: bool,
    },
    /// Print what a plugin's program wrote to standard error -- a Python
    /// script's tracebacks, say -- in its last runs
    Logs {
        /// The plugin, by id
        id: String,
        /// The last this many lines
        #[arg(long, short = 'n', default_value_t = 200)]
        lines: usize,
    },
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
        let host_log = host.log.clone();
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
            Sub::New { id, name, off } => new_plugin(&session, id, name.as_deref(), *off)?,
            Sub::Logs { id, lines } => print_logs(&session, &host_log, id, *lines)?,
            Sub::Shot { id, page, panel } => {
                let page = match (page, panel) {
                    (true, _) => Some(true),
                    (_, true) => Some(false),
                    _ => None,
                };
                let request = api::Request::Picture {
                    id: id.clone(),
                    page,
                };
                // The host gives the view's client its own while to draw it.
                let kept = ask(
                    &session,
                    api::PLUGIN,
                    serde_json::to_value(request)?,
                    SHOT_TIMEOUT,
                )?;
                let kept: api::Picture = serde_json::from_value(kept)?;
                println!("{}", kept.path);
                eprintln!(
                    "{} by {} pixels: {id}'s {}, as ThinkTerm shows it",
                    kept.width,
                    kept.height,
                    if kept.page { "page" } else { "panel" }
                );
            }
            Sub::Dir => unreachable!("answered above"),
        }
        Ok(())
    }
}

/// How long `shot` waits: longer than the host gives a client to draw.
const SHOT_TIMEOUT: Duration = Duration::from_secs(15);
/// How long `new` waits for the plugin it made to answer: its first start
/// may wait for Python to be looked for, a login shell's PATH and all.
const FIRST_CALL_TIMEOUT: Duration = Duration::from_secs(40);

/// The plugin `thinkterm plugin new` makes, as plugins/python-template has
/// it: its manifest is given the new plugin's id and name.
const TEMPLATE_MANIFEST: &str = include_str!("../../plugins/python-template/plugin.toml");
const TEMPLATE_MAIN: &str = include_str!("../../plugins/python-template/main.py");
const TEMPLATE_HELPER: &str = include_str!("../../plugins/python-template/thinkterm.py");

/// Makes plugin `id` from the template in the plugins directory, in a
/// directory of its own made for it, and -- unless `off` -- turns on that
/// one and calls it once, to say whether it runs.
fn new_plugin(session: &Session, id: &str, name: Option<&str>, off: bool) -> anyhow::Result<()> {
    anyhow::ensure!(
        api::valid_id(id) && id != api::PLUGIN,
        "{id:?} cannot name a plugin: an id is {}",
        api::id_rule()
    );
    let name = name
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| name_from(id));
    let listed = manage(session, api::Request::List { locale: locale() })?;
    let listed: Vec<Info> = serde_json::from_value(listed)?;
    if let Some(taken) = listed.iter().find(|info| info.id == id) {
        let place = match (&taken.dir, taken.builtin) {
            (_, true) => ", built into ThinkTerm".to_string(),
            (Some(dir), _) => format!(", in {dir}"),
            (None, _) => String::new(),
        };
        anyhow::bail!("there is a plugin {id:?} already{place}: pick another id");
    }
    let plugins = thinkterm_plugin_channel::paths::plugins_dir();
    std::fs::create_dir_all(&plugins).with_context(|| format!("making {}", plugins.display()))?;
    let dir = plugins.join(id);
    // Made here and now, or not at all: never into what is there already.
    std::fs::create_dir(&dir).with_context(|| {
        format!(
            "making {}: pick another id, or move what is there away",
            dir.display()
        )
    })?;
    let write = |file: &str, text: &str| {
        std::fs::write(dir.join(file), text).with_context(|| format!("writing {file}"))
    };
    write("main.py", TEMPLATE_MAIN)?;
    write("thinkterm.py", TEMPLATE_HELPER)?;
    // The manifest last, and whole: the host never finds half a plugin.
    write(".plugin.toml.partial", &manifest_for(id, &name))?;
    std::fs::rename(dir.join(".plugin.toml.partial"), dir.join("plugin.toml"))
        .context("writing plugin.toml")?;
    println!("Made {name}, {id}, in {}", dir.display());
    if off {
        // Off whatever a plugin of its id, there before, was left as.
        manage(
            session,
            api::Request::SetEnabled {
                id: id.to_string(),
                enabled: false,
            },
        )
        .map_err(|why| anyhow!("it is made, and could not be turned off: {why:#}"))?;
        println!("It is off: `thinkterm plugin enable {id}` lets it run.");
        return Ok(());
    }
    // Only the plugin just made: not another that claims its id meanwhile.
    manage(
        session,
        api::Request::Allow {
            id: id.to_string(),
            dir: dir.display().to_string(),
        },
    )
    .map_err(|why| anyhow!("it is made, and not turned on: {why:#}"))?;
    // Its first start looks for Python too, which can take a while.
    if let Err(why) = ask(session, id, json!({}), FIRST_CALL_TIMEOUT) {
        anyhow::bail!(
            "it is on, and does not run: {why:#}\n\
             Once that is mended, `thinkterm plugin reload {id}` starts it again, \
             and `thinkterm plugin logs {id}` shows what it said."
        );
    }
    println!(
        "It is on, and runs: its panel is in the right sidebar.\n\n\
         \x20 thinkterm plugin do {id} open    opens its page\n\
         \x20 thinkterm plugin call {id} '{{}}'  calls it\n\
         \x20 thinkterm plugin logs {id}       what it printed to standard error\n\n\
         Edit main.py: it starts again once a .py file in its directory changes, \
         or with `thinkterm plugin reload {id}`."
    );
    Ok(())
}

/// The template's manifest for plugin `id`, called `name`.
fn manifest_for(id: &str, name: &str) -> String {
    let mut manifest: String = TEMPLATE_MANIFEST
        .lines()
        .map(|line| match line {
            "id = \"hello-python\"" => format!("id = {}", toml_string(id)),
            "name = \"Hello Python\"" => format!("name = {}", toml_string(name)),
            "title = \"Open Hello Python\"" => {
                format!("title = {}", toml_string(&format!("Open {name}")))
            }
            line => line.to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n");
    manifest.push('\n');
    manifest
}

/// `text` as a TOML basic string.
fn toml_string(text: &str) -> String {
    let mut quoted = String::with_capacity(text.len() + 2);
    quoted.push('"');
    for c in text.chars() {
        match c {
            '"' => quoted.push_str("\\\""),
            '\\' => quoted.push_str("\\\\"),
            c if c.is_control() => quoted.push_str(&format!("\\u{:04X}", c as u32)),
            c => quoted.push(c),
        }
    }
    quoted.push('"');
    quoted
}

/// A name for plugin `id`: its words, capitalized.
fn name_from(id: &str) -> String {
    id.split('-')
        .filter(|word| !word.is_empty())
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().chain(chars).collect::<String>(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Prints the last `last` lines plugin `id`'s programs wrote to standard
/// error: what the host kept of them, or -- from a host started since,
/// which kept none -- what the host's log, `log`, still has.
fn print_logs(session: &Session, log: &Path, id: &str, last: usize) -> anyhow::Result<()> {
    let kept = manage(session, api::Request::Logs { id: id.to_string() });
    let (mut shown, cut) = match kept {
        Ok(value) => {
            let logs: api::Logs = serde_json::from_value(value)?;
            (logs.lines, logs.cut)
        }
        // A host from before it kept them.
        Err(why) if unknown_to_the_host(&why) => (Vec::new(), false),
        Err(why) => return Err(why),
    };
    if shown.iter().all(|line| is_run_mark(line)) {
        let logged = from_host_log(log, id);
        if !logged.is_empty() {
            shown = logged;
        }
    }
    if shown.iter().all(|line| is_run_mark(line)) {
        println!("{id} has written nothing to standard error lately.");
        return Ok(());
    }
    let skipped = shown.len().saturating_sub(last);
    if cut || skipped > 0 {
        println!("(older lines are left out)");
    }
    for line in &shown[skipped..] {
        println!("{line}");
    }
    Ok(())
}

/// Whether the host said it does not know what it was asked: one from
/// before that was a thing to ask.
fn unknown_to_the_host(why: &anyhow::Error) -> bool {
    why.to_string()
        .contains("not a request the plugin host knows")
}

/// Whether `line` says when a run started or why it stopped, rather than
/// being what it wrote.
fn is_run_mark(line: &str) -> bool {
    line.starts_with("-- ") && line.ends_with(" --")
}

/// `text` without what a terminal would act on: an older host logged a
/// plugin's standard error as it came.
fn plain(text: &str) -> String {
    let mut plain = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => match chars.next() {
                Some('[') => {
                    for c in chars.by_ref() {
                        if ('\u{40}'..='\u{7e}').contains(&c) {
                            break;
                        }
                    }
                }
                Some(']' | 'P' | 'X' | '^' | '_') => {
                    while let Some(c) = chars.next() {
                        if c == '\u{7}' {
                            break;
                        }
                        if c == '\u{1b}' && chars.peek() == Some(&'\\') {
                            chars.next();
                            break;
                        }
                    }
                }
                _ => {}
            },
            '\t' => plain.push(c),
            c if c.is_control() => {}
            c => plain.push(c),
        }
    }
    plain
}

/// What the host's log, at `log`, says plugin `id`'s programs wrote to
/// standard error, with when each started and why it stopped.
fn from_host_log(log: &Path, id: &str) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(log) else {
        return Vec::new();
    };
    let said = format!("plugin {id}: ");
    let stopped = format!("plugin {id} stopped: ");
    let started = format!("started plugin {id} (");
    text.lines()
        .filter_map(|line| {
            // "<seconds> <pid> [LEVEL] <message>"
            let (_, message) = line.split_once("] ")?;
            if let Some(rest) = message.strip_prefix(&said) {
                Some(plain(rest))
            } else if let Some(why) = message.strip_prefix(&stopped) {
                Some(format!("-- stopped: {} --", plain(why)))
            } else if message.starts_with(&started) {
                Some("-- started --".to_string())
            } else {
                None
            }
        })
        .collect()
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
    use super::*;

    #[test]
    fn a_new_plugin_gets_its_id_and_name_in_the_templates_manifest() {
        let manifest = manifest_for("stock-ticker", "Stocks \"live\"");
        assert!(
            manifest.contains("\nid = \"stock-ticker\"\n"),
            "{}",
            manifest
        );
        assert!(
            manifest.contains("\nname = \"Stocks \\\"live\\\"\"\n"),
            "{}",
            manifest
        );
        assert!(
            manifest.contains("\ntitle = \"Open Stocks \\\"live\\\"\"\n"),
            "{}",
            manifest
        );
        assert!(!manifest.contains("hello-python") && !manifest.contains("Hello Python"));
        assert!(manifest.contains("python = \"main.py\""));
        assert!(TEMPLATE_HELPER.contains("HELPER = 1"));
        assert!(TEMPLATE_MAIN.contains("thinkterm.run("));
    }

    #[test]
    fn a_name_is_made_from_an_id_and_quoted_for_toml() {
        assert_eq!(name_from("stock-ticker"), "Stock Ticker");
        assert_eq!(name_from("x2"), "X2");
        assert_eq!(toml_string("a\\b\"c\td 中"), "\"a\\\\b\\\"c\\u0009d 中\"");
    }

    #[test]
    fn the_hosts_log_has_what_a_plugin_said_when_its_host_kept_none() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("plugins-log");
        std::fs::write(
            &log,
            "1 7 [INFO] started plugin hello (/usr/bin/python3), pid 9\n\
             2 7 [INFO] plugin hello: Traceback (most recent call last):\n\
             2 7 [INFO] plugin other: not this one\n\
             3 7 [INFO] plugin hello: NameError: \u{1b}[31mx\u{1b}[0m\n\
             3 7 [WARN] plugin hello stopped: it exited with status 1\n",
        )
        .unwrap();
        assert_eq!(
            from_host_log(&log, "hello"),
            [
                "-- started --",
                "Traceback (most recent call last):",
                "NameError: x",
                "-- stopped: it exited with status 1 --"
            ]
        );
        assert!(from_host_log(&dir.path().join("none"), "hello").is_empty());
        assert!(is_run_mark("-- started at 01:02:03 UTC --"));
        assert!(is_run_mark("-- started --"));
        assert!(is_run_mark("-- stopped: it exited with status 1 --"));
        assert!(!is_run_mark("started"));
    }

    #[test]
    fn a_locale_comes_from_the_environment_as_a_tag() {
        // Only the shape: the variables belong to whoever runs the tests.
        let locale = super::locale();
        assert!(!locale.contains('_') && !locale.contains('.'), "{}", locale);
    }
}
