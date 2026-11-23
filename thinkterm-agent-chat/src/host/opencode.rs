//! OpenCode 2: its background service, which the TUI in the pane is
//! itself a client of -- or the private server a TUI started with
//! `--standalone` runs for itself, or the one it was pointed at with
//! `--server`. The bridge is one more client of it: it finds the pane's
//! session, follows the service's events, and may do what the TUI does --
//! say something (a turn of its own, or more for the running one), stop
//! the turn, answer an approval, switch the model.
//!
//! The service speaks HTTP and JSON on the loopback, behind a password,
//! and says where it is in a file in OpenCode's state folder
//! (`~/.local/state/opencode/service.json`). OpenCode 1 has no such
//! service; the person is told to update.

use super::http::{self, Endpoint};
use super::{head, now, read_ops, tail, Input, Out, PaneControl, PaneProc, Target};
use crate::{
    AgentState, Ask, AskKind, Attachment, Choice, CommandInfo, ContextInfo, ContextPart, Ev, FileEdit, Item, Look, LookKind,
    ModelChoice, Need, NoteKind, Op, PartKind, Status, Usage,
};
use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

/// How long one call to the service may take.
const CALL_WAIT: Duration = Duration::from_secs(20);
/// How long a probe of where the service may be may take.
const PROBE_WAIT: Duration = Duration::from_secs(3);
/// The event stream says something every 15 s: a minute of nothing is a
/// service gone.
const QUIET: Duration = Duration::from_secs(60);
/// The longest event taken in, as OpenCode's own client takes them.
const MAX_EVENT: usize = 16 << 20;
/// Events read and not yet taken, at most: the service sends every
/// session's, and the chat may be slow to take them.
const MAX_BACKLOG: usize = 10_000;
/// Messages of history asked for at a time.
const MESSAGES_AT_ONCE: u32 = 80;
/// Sessions asked for at a time, looking for the pane's.
const SESSIONS_LOOKED_AT: u32 = 100;
/// Pages of a folder's sessions looked through, at most.
const SESSION_PAGES: usize = 20;
/// How often the pane's title is looked at, to follow the TUI when it
/// opens another session.
const FOLLOW_EVERY: Duration = Duration::from_secs(2);
/// How long the bridge tries to find the service again once its events
/// stop (it restarts when OpenCode updates).
const RECONNECT_FOR: Duration = Duration::from_secs(15);
/// What is kept of a command's output and a file's diff.
const OUTPUT_KEPT: usize = 6 << 10;
const DIFF_KEPT: usize = 24 << 10;
/// Files of one change whose diff is kept.
const DIFFS_KEPT: usize = 12;
/// What is kept of what the agent said or thought, each, and of a command.
const TEXT_KEPT: usize = 256 << 10;
/// What is shown of a command asked about: its start, said to go on.
const ASK_KEPT: usize = 16 << 10;
/// What is shown of a tool's input, or a command's description.
const DETAIL_KEPT: usize = 300;
/// The small pictures of one message, at most, in bytes.
const THUMBS_KEPT: usize = 2 << 20;
/// How often what the agent is writing is sent as it grows.
const WRITING_EVERY: Duration = Duration::from_millis(150);
/// The names OpenCode's processes go by: its TUI calls itself `opencode`,
/// its servers keep the name of their file (`opencode.exe`, as npm puts it).
const PROGRAMS: &[&str] = &["opencode", "opencode.exe", "opencode2"];
/// The user the service takes its password for.
const USER: &str = "opencode";
/// The prefix of the terminal title OpenCode's TUI sets for a session:
/// `OC | <its name>`, cut to 40 characters with an ellipsis.
const TITLE_PREFIX: &str = "OC | ";
/// The events the bridge follows; the service has many more.
const EVENTS: &[&str] = &[
    "session.created",
    "session.deleted",
    "session.renamed",
    "session.model.selected",
    "session.usage.updated",
    "session.inbox.enqueued",
    "session.inbox.delivered",
    "session.inbox.cancelled",
    "session.skill.activated",
    "session.execution.started",
    "session.execution.succeeded",
    "session.execution.failed",
    "session.execution.interrupted",
    "session.step.ended",
    "session.step.failed",
    "session.text.delta",
    "session.text.ended",
    "session.reasoning.delta",
    "session.reasoning.ended",
    "session.tool.input.started",
    "session.tool.called",
    "session.tool.success",
    "session.tool.failed",
    "session.shell.started",
    "session.shell.ended",
    "session.compaction.ended",
    "session.compaction.failed",
    "permission.asked",
    "permission.replied",
    "model.updated",
    "provider.updated",
    "form.created",
    "form.replied",
    "form.cancelled",
    "command.updated",
];
/// OpenCode's own commands the chat runs: name, what it does, what follows.
const OWN_COMMANDS: [(&str, &str, Option<&str>); 2] = [
    ("/compact", "Summarize the conversation to free up context", None),
    ("/rename", "Rename the session", Some("<name>")),
];
/// The TUI's own commands, which need it: said so, rather than sent to the
/// model as words.
const TERMINAL_COMMANDS: [&str; 22] = [
    "new", "clear", "sessions", "resume", "continue", "models", "agents", "themes", "help", "exit", "quit", "q", "undo",
    "redo", "share", "unshare", "editor", "export", "details", "settings", "mcps", "status",
];

/// The service, as the bridge talks to it.
struct Api {
    endpoint: Endpoint,
    /// Where the TUI runs: what is asked of the service is about it.
    directory: Option<String>,
}

impl Api {
    fn call(&self, method: &str, path: &str, body: Option<&Value>) -> Result<Value> {
        let body = body.map(|b| b.to_string());
        let answer = http::request(&self.endpoint, method, path, body.as_deref().map(str::as_bytes), CALL_WAIT)
            .with_context(|| format!("{method} {path}"))?;
        if !answer.ok() {
            return Err(anyhow!("{method} {path}: {} {}", answer.status, refusal(&answer.text())));
        }
        if answer.body.iter().all(u8::is_ascii_whitespace) {
            return Ok(Value::Null);
        }
        serde_json::from_slice(&answer.body).with_context(|| format!("{method} {path}: not JSON"))
    }

    fn get(&self, path: &str) -> Result<Value> {
        self.call("GET", path, None)
    }

    fn post(&self, path: &str, body: &Value) -> Result<Value> {
        self.call("POST", path, Some(body))
    }

    /// `?location[directory]=…`, which places a question about the
    /// configuration (models, commands) where the TUI runs.
    fn location(&self) -> String {
        match &self.directory {
            Some(dir) => format!("location%5Bdirectory%5D={}", encode(dir)),
            None => String::new(),
        }
    }
}

/// What a refusal says, from the service's JSON error or as it came.
fn refusal(text: &str) -> String {
    let said = serde_json::from_str::<Value>(text).ok().and_then(|value| {
        value["message"]
            .as_str()
            .or(value["error"]["message"].as_str())
            .or(value["data"]["message"].as_str())
            .or(value["error"].as_str())
            .map(str::to_string)
    });
    head(&said.unwrap_or_else(|| text.trim().to_string()), 400)
}

/// `text` as a URL takes it in a path's part or a query's value.
fn encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.~".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// The TUI in the pane, as far as its process tells (Linux).
#[derive(Debug, Default)]
struct Tui {
    /// A private server it runs for itself (`--standalone`).
    private: Option<Endpoint>,
    /// The server it was pointed at (`--server`), as given, and as the
    /// bridge can reach it (none for one it cannot: `https`).
    server_url: Option<String>,
    server: Option<Endpoint>,
    /// It runs a private server of its own (`--standalone`).
    standalone: bool,
    /// The session it was asked to open (`--session`).
    session: Option<String>,
    /// OpenCode's state folder, as the TUI has it.
    state_home: Option<PathBuf>,
    home: Option<PathBuf>,
    /// Its program, to ask its version of when no service answers.
    exe: Option<PathBuf>,
    /// The folder it works in: the one it was given, else its own.
    directory: Option<PathBuf>,
    pid: Option<u32>,
    /// The ports it is connected to: its server's among them.
    ports: Vec<u16>,
}

fn tui_of(procs: &[PaneProc]) -> Tui {
    let mut tui = Tui::default();
    let serves = |proc: &PaneProc| proc.argv.iter().skip(1).any(|arg| arg == "serve");
    if let Some(proc) = procs.iter().find(|proc| !serves(proc)) {
        let own_dir = std::fs::read_link(format!("/proc/{}/cwd", proc.pid)).ok();
        let mut given = None;
        let mut args = proc.argv.iter().skip(1);
        while let Some(arg) = args.next() {
            let value = |flag: &str, args: &mut dyn Iterator<Item = &String>| -> Option<String> {
                if arg == flag {
                    args.next().cloned()
                } else {
                    arg.strip_prefix(&format!("{flag}=")).map(str::to_string)
                }
            };
            if let Some(url) = value("--server", &mut args) {
                // As OpenCode reads them: OPENCODE_PASSWORD first.
                let password = proc.var("OPENCODE_PASSWORD").or_else(|| proc.var("OPENCODE_SERVER_PASSWORD"));
                tui.server = Endpoint::parse(&url, password.map(|p| Endpoint::basic(USER, &p)));
                tui.server_url = Some(shown_url(&url));
            } else if arg == "--standalone" {
                tui.standalone = true;
            } else if let Some(id) = value("--session", &mut args).or_else(|| value("-s", &mut args)) {
                tui.session = Some(id);
            } else if value("--prompt", &mut args).is_some() || value("--log-level", &mut args).is_some() {
                // Their values are no folder.
            } else if !arg.starts_with('-') {
                // The folder it was asked to start in, as it was started.
                let dir = match &own_dir {
                    Some(own) => own.join(arg),
                    None => PathBuf::from(arg),
                };
                if dir.is_dir() {
                    given = Some(dir);
                }
            }
        }
        tui.directory = given.or(own_dir).map(|dir| std::fs::canonicalize(&dir).unwrap_or(dir));
        tui.state_home = proc.var("XDG_STATE_HOME").filter(|dir| !dir.is_empty()).map(PathBuf::from);
        tui.home = proc.var("HOME").filter(|dir| !dir.is_empty()).map(PathBuf::from);
        tui.exe = std::fs::read_link(format!("/proc/{}/exe", proc.pid)).ok();
        tui.pid = Some(proc.pid);
        tui.ports = super::connected_ports(proc.pid);
        // A private server is the TUI's own child, told its password.
        let private = procs.iter().find(|server| {
            serves(server) && server.argv.iter().any(|arg| arg == "--stdio") && server.parent() == Some(proc.pid)
        });
        if let Some(server) = private {
            let password = server.var("OPENCODE_PASSWORD").or_else(|| server.var("OPENCODE_SERVER_PASSWORD"));
            // Each port it listens on, until one answers as it.
            tui.private = super::listening_ports(server.pid)
                .into_iter()
                .map(|port| Endpoint {
                    authority: format!("127.0.0.1:{port}"),
                    base: String::new(),
                    auth: password.as_deref().map(|p| Endpoint::basic(USER, p)),
                })
                .find(|endpoint| probe(endpoint, Some(server.pid)).is_some());
            tui.standalone = true;
        }
    }
    tui
}

/// A URL as it may be shown or logged: without a user, a password or a
/// query, any of which may hold a secret.
fn shown_url(url: &str) -> String {
    let url = url.trim();
    let (scheme, rest) = match url.split_once("://") {
        Some((scheme, rest)) => (format!("{scheme}://"), rest),
        None => (String::new(), url),
    };
    let rest = rest.split(['?', '#']).next().unwrap_or_default();
    let (authority, path) = match rest.find('/') {
        Some(at) => (&rest[..at], &rest[at..]),
        None => (rest, ""),
    };
    let host = authority.rsplit('@').next().unwrap_or(authority);
    format!("{scheme}{host}{path}")
}

/// Whether the process `pid` is the one listening on `port`, where that
/// can be told. A registration a service left behind may name a port
/// another program listens on now, which is never sent the password.
#[cfg(target_os = "linux")]
fn listens(pid: u32, port: u16) -> Option<bool> {
    if !Path::new(&format!("/proc/{pid}")).exists() {
        return Some(false);
    }
    Some(super::listening_ports(pid).contains(&port))
}

#[cfg(not(target_os = "linux"))]
fn listens(pid: u32, port: u16) -> Option<bool> {
    // As OpenCode itself asks who holds its port.
    let out = std::process::Command::new("lsof")
        .args(["-nP", &format!("-iTCP:{port}"), "-sTCP:LISTEN", "-Fp"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    let pid = pid.to_string();
    Some(
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|line| line.strip_prefix('p'))
            .any(|found| found.trim() == pid),
    )
}

/// What the service's registration file says.
fn registered(file: &Path) -> Option<(Endpoint, Option<u32>)> {
    let info: Value = serde_json::from_str(&std::fs::read_to_string(file).ok()?).ok()?;
    let auth = info["password"].as_str().filter(|p| !p.is_empty()).map(|p| Endpoint::basic(USER, p));
    let endpoint = Endpoint::parse(info["url"].as_str()?, auth)?;
    Some((endpoint, info["pid"].as_u64().map(|pid| pid as u32)))
}

/// The files a service may have registered itself in: the release's,
/// then any other channel's.
fn registrations(tui: &Tui) -> Vec<PathBuf> {
    let home = tui
        .home
        .clone()
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("/"));
    let state = tui
        .state_home
        .clone()
        .or_else(|| std::env::var_os("XDG_STATE_HOME").filter(|d| !d.is_empty()).map(PathBuf::from))
        .unwrap_or_else(|| home.join(".local").join("state"))
        .join("opencode");
    let mut files = vec![state.join("service.json")];
    if let Ok(entries) = std::fs::read_dir(&state) {
        let mut others: Vec<PathBuf> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name().and_then(|n| n.to_str()).is_some_and(|name| {
                    name.starts_with("service-") && name.ends_with(".json") && !name.contains(".pty-handoff")
                })
            })
            .collect();
        others.sort();
        files.extend(others);
    }
    files
}

/// Whether OpenCode's service answers at `endpoint`, as the pid it
/// registered with when that is known: its version if so.
fn probe(endpoint: &Endpoint, pid: Option<u32>) -> Option<String> {
    let answer = http::request(endpoint, "GET", "/api/info", None, PROBE_WAIT).ok()?;
    if !answer.ok() {
        return None;
    }
    let info: Value = serde_json::from_slice(&answer.body).ok()?;
    if pid.is_some_and(|pid| info["pid"].as_u64() != Some(pid as u64)) {
        return None;
    }
    Some(info["version"].as_str().unwrap_or_default().to_string())
}

/// Where the pane's OpenCode may be served: the one server when that is
/// known (the TUI's own, the one it was given, the one it is connected
/// to), else every service that answers.
struct Located {
    tui: Tui,
    endpoints: Vec<Endpoint>,
}

/// The service the pane's TUI talks to: the server it runs or was pointed
/// at, else the background service.
fn locate(target: &Target) -> std::result::Result<Located, (Need, String)> {
    let tui = tui_of(&super::pane_procs(target, PROGRAMS));
    log::info!("agent chat: OpenCode in pane {}: {tui:?}", target.pane);
    // A server the TUI was pointed at, or runs for itself: that one, or
    // none -- never another, which would hold other conversations.
    if let Some(url) = tui.server_url.clone() {
        return match tui.server.clone() {
            Some(endpoint) if probe(&endpoint, None).is_some() => {
                log::info!("agent chat: OpenCode's given server at {}", endpoint.authority);
                Ok(Located {
                    tui,
                    endpoints: vec![endpoint],
                })
            }
            Some(_) => Err((Need::OpenCodeService, format!("the server this OpenCode was given ({url}) does not answer"))),
            None => Err((Need::OpenCodeService, format!("this OpenCode talks to {url}, which the chat cannot reach"))),
        };
    }
    if tui.standalone || tui.private.is_some() {
        return match tui.private.clone() {
            Some(endpoint) if probe(&endpoint, None).is_some() => {
                log::info!("agent chat: OpenCode's own server at {}", endpoint.authority);
                Ok(Located {
                    tui,
                    endpoints: vec![endpoint],
                })
            }
            _ => Err((Need::OpenCodeService, "this OpenCode's private server (--standalone) does not answer".into())),
        };
    }
    let mut answering = Vec::new();
    for file in registrations(&tui) {
        let Some((endpoint, pid)) = registered(&file) else {
            continue;
        };
        // Its password goes to the process that registered it, or nowhere.
        if let (Some(pid), Some(port)) = (pid, port_of(&endpoint)) {
            if listens(pid, port) == Some(false) {
                log::info!("agent chat: {} is not OpenCode's any more ({})", endpoint.authority, file.display());
                continue;
            }
        }
        if probe(&endpoint, pid).is_some() {
            log::info!("agent chat: OpenCode's service at {} ({})", endpoint.authority, file.display());
            // The TUI talks to it: it is its service.
            if port_of(&endpoint).is_some_and(|port| tui.ports.contains(&port)) {
                return Ok(Located {
                    tui,
                    endpoints: vec![endpoint],
                });
            }
            answering.push(endpoint);
        }
    }
    // OpenCode 1 has no service at all, though a 2 may have left one
    // running: it is told apart by its version.
    if let Some(version) = tui.exe.as_deref().and_then(version_of) {
        if version.split('.').next().and_then(|major| major.trim_start_matches('v').parse::<u32>().ok()).is_some_and(|major| major < 2) {
            return Err((Need::OpenCodeUpgrade, format!("OpenCode {version} has no background service")));
        }
    }
    // The TUI's connections are known, and none is to a service found: it
    // talks to another, which the bridge does not know.
    if !answering.is_empty() && tui.pid.is_some() && !tui.ports.is_empty() {
        return Err((
            Need::OpenCodeService,
            "this OpenCode talks to a server other than the one registered".into(),
        ));
    }
    if answering.is_empty() {
        return Err((
            Need::OpenCodeService,
            "no OpenCode service answers: none is registered, or it does not answer".into(),
        ));
    }
    Ok(Located {
        tui,
        endpoints: answering,
    })
}

/// The pane's session, on the one service that has it: on several that
/// answer, the one where it is found surely, or none (which is said)
/// rather than one where it was guessed.
fn pick(
    endpoints: Vec<Endpoint>,
    directory: Option<String>,
    cwd: Option<&Path>,
    title: Option<&str>,
    asked: Option<&str>,
    screen: Option<&[String]>,
) -> std::result::Result<(Api, Found), (Need, String)> {
    let place = cwd.map_or("any folder".into(), |cwd| cwd.display().to_string());
    let not_found = || match shown_title(title) {
        Some(Some(name)) => format!("the session the pane shows ({name}) is not among OpenCode's in {place}"),
        _ => format!("no OpenCode session in {place}"),
    };
    let several = endpoints.len() > 1;
    let mut sure = Vec::new();
    for endpoint in endpoints {
        let api = Api {
            endpoint,
            directory: directory.clone(),
        };
        match find_session(&api, cwd, title, asked, screen) {
            Ok(Some(found)) if !several => return Ok((api, found)),
            Ok(Some(found)) if !found.guessed => sure.push((api, found)),
            Ok(_) => {}
            Err(err) if !several => return Err((Need::OpenCodeService, format!("{err:#}"))),
            Err(err) => log::info!("agent chat: an OpenCode service did not list its sessions: {err:#}"),
        }
    }
    if !several {
        return Err((Need::OpenCodeSession, not_found()));
    }
    match sure.len() {
        1 => Ok(sure.remove(0)),
        0 => Err((Need::OpenCodeSession, not_found())),
        _ => Err((
            Need::OpenCodeService,
            "several OpenCode services run, and which one this pane's is cannot be told".into(),
        )),
    }
}

/// The port an endpoint is at.
fn port_of(endpoint: &Endpoint) -> Option<u16> {
    endpoint.authority.rsplit(':').next()?.parse().ok()
}

/// What `program --version` says, if it says it soon.
fn version_of(program: &Path) -> Option<String> {
    let mut child = std::process::Command::new(program)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    let mut said = String::new();
    std::io::Read::read_to_string(&mut child.stdout.take()?, &mut said).ok()?;
    said.split_whitespace()
        .find(|word| word.trim_start_matches('v').starts_with(|c: char| c.is_ascii_digit()))
        .map(str::to_string)
}

/// The events of the service, on a thread of their own: each as JSON.
fn follow(endpoint: &Endpoint, tx: mpsc::Sender<Input<Value>>, backlog: Arc<AtomicUsize>) -> Result<()> {
    let mut path = String::from("/api/event?");
    path.push_str(&EVENTS.iter().map(|t| format!("types={t}")).collect::<Vec<_>>().join("&"));
    let (mut status, mut body) = http::stream(endpoint, &path, CALL_WAIT, QUIET)?;
    if status == 400 {
        // A service that does not know one of the kinds: all of them.
        (status, body) = http::stream(endpoint, "/api/event", CALL_WAIT, QUIET)?;
    }
    if status != 200 {
        return Err(anyhow!("the event stream: {status}"));
    }
    std::thread::spawn(move || {
        let mut data = String::new();
        let mut raw = Vec::new();
        loop {
            raw.clear();
            // An event is never longer than this, nor a line of it.
            match http::read_line_within(&mut body, &mut raw, MAX_EVENT) {
                Ok(0) => {
                    let _ = tx.send(Input::AgentGone("its events ended".into()));
                    return;
                }
                Ok(_) => {}
                Err(err) => {
                    let _ = tx.send(Input::AgentGone(err.to_string()));
                    return;
                }
            }
            let line = String::from_utf8_lossy(&raw);
            let line = line.trim_end_matches(['\r', '\n']);
            if line.is_empty() {
                if !data.is_empty() {
                    match serde_json::from_str::<Value>(&data) {
                        Ok(event) => {
                            // Too many not taken yet: let the stream go; the
                            // chat finds the service again and reads all anew.
                            if backlog.fetch_add(1, Ordering::SeqCst) >= MAX_BACKLOG {
                                let _ = tx.send(Input::AgentGone("too far behind its events".into()));
                                return;
                            }
                            if tx.send(Input::Agent(event)).is_err() {
                                return;
                            }
                        }
                        Err(err) => log::warn!("agent chat: an OpenCode event that is not JSON: {err}"),
                    }
                    data.clear();
                }
            } else if let Some(more) = line.strip_prefix("data:") {
                let more = more.strip_prefix(' ').unwrap_or(more);
                if data.len() + more.len() + 1 > MAX_EVENT {
                    let _ = tx.send(Input::AgentGone("an event too long".into()));
                    return;
                }
                if !data.is_empty() {
                    data.push('\n');
                }
                data.push_str(more);
            }
            // Comments (heartbeats), and the `event:`/`id:` fields: nothing
            // to take.
        }
    });
    Ok(())
}

/// The pane's session, as found.
#[derive(Clone, Debug)]
struct Found {
    info: Value,
    guessed: bool,
}

impl Found {
    fn id(&self) -> &str {
        self.info["id"].as_str().unwrap_or_default()
    }
}

/// The name the TUI shows in the pane's title: `Some(None)` when it shows
/// none (its home, or a session without a name), `None` when the title
/// is not the TUI's.
fn shown_title(title: Option<&str>) -> Option<Option<String>> {
    let title = title?.trim();
    if let Some(name) = title.strip_prefix(TITLE_PREFIX) {
        return Some(Some(name.to_string()));
    }
    (title == "OpenCode").then_some(None)
}

/// Whether a session's name is the one the title shows, which the TUI
/// cuts to 40 characters (37 and an ellipsis), its control characters
/// made spaces.
fn same_title(name: &str, shown: &str) -> bool {
    let name: String = name.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    if name == shown {
        return true;
    }
    match shown.strip_suffix('\u{2026}') {
        Some(start) if !start.is_empty() => name.starts_with(start),
        _ => false,
    }
}

/// The pane's session: one started where the pane is, not a sub-agent's;
/// the one the title names if it names one, else among several the one
/// working, else the one last updated.
fn find_session(
    api: &Api,
    cwd: Option<&Path>,
    title: Option<&str>,
    asked: Option<&str>,
    screen: Option<&[String]>,
) -> Result<Option<Found>> {
    let shown = shown_title(title);
    let here = |session: &Value| -> bool {
        session["parentID"].is_null()
            && session["time"]["archived"].is_null()
            && match cwd {
                Some(cwd) => session_dir(session).is_some_and(|dir| same_dir(&dir, cwd)),
                None => true,
            }
    };
    let named = |session: &Value| -> bool {
        match (&shown, session["title"].as_str()) {
            (Some(Some(shown)), Some(name)) => same_title(name, shown),
            (Some(None), name) => name.is_none_or(|name| name.trim().is_empty() || default_title(name)),
            _ => false,
        }
    };
    // All of the folder's (the service lists them by folder), and of those
    // the ones whose name has the title's words when it names one: every
    // page, so that a name found once is the only one so named.
    let mut query = format!("limit={SESSIONS_LOOKED_AT}&parentID=null&archived=false");
    if let Some(cwd) = cwd {
        query.push_str(&format!("&directory={}", encode(&cwd.display().to_string())));
    }
    if let Some(Some(name)) = &shown {
        let words = name.trim_end_matches('\u{2026}').trim();
        if !words.is_empty() {
            query.push_str(&format!("&search={}", encode(words)));
        }
    }
    let mut sessions: Vec<Value> = Vec::new();
    let mut cursor: Option<String> = None;
    let mut complete = false;
    for _ in 0..SESSION_PAGES {
        let mut path = format!("/api/session?{query}");
        // A cursor holds its order, which may not be given again.
        match &cursor {
            Some(cursor) => path.push_str(&format!("&cursor={}", encode(cursor))),
            None => path.push_str("&order=desc"),
        }
        let listed = api.get(&path)?;
        let page = listed["data"].as_array().cloned().unwrap_or_default();
        let full = page.len() as u32 >= SESSIONS_LOOKED_AT;
        sessions.extend(page.into_iter().filter(|session| here(session)));
        cursor = listed["cursor"]["next"].as_str().filter(|_| full).map(str::to_string);
        if cursor.is_none() {
            complete = true;
            break;
        }
    }
    let active: HashSet<String> = api
        .get("/api/session/active")
        .ok()
        .and_then(|active| active["data"].as_object().map(|map| map.keys().cloned().collect()))
        .unwrap_or_default();
    // The one the TUI was opened on (`--session`), looked up by itself when
    // it is older than those listed, while the title does not say another.
    if let Some(asked) = asked {
        let session = match sessions.iter().find(|s| s["id"].as_str() == Some(asked)) {
            Some(session) => Some(session.clone()),
            None => api
                .get(&format!("/api/session/{}", encode(asked)))
                .ok()
                .map(|info| info.get("data").cloned().unwrap_or(info))
                .filter(|session| session["id"].as_str() == Some(asked)),
        };
        if let Some(session) = session {
            if shown.is_none() || named(&session) {
                return Ok(Some(Found {
                    info: session,
                    guessed: false,
                }));
            }
        }
    }
    let by_name: Vec<Value> = sessions.iter().filter(|s| named(s)).cloned().collect();
    let (mut pool, sure) = match (&shown, by_name.len()) {
        // Named, and only one has that name: sure once all were looked at.
        (Some(Some(_)), 1) => (by_name, complete),
        // Named, and none has that name: not another one in its place.
        (Some(Some(_)), 0) => return Ok(None),
        (Some(_), n) if n > 0 => (by_name, false),
        _ => (sessions, false),
    };
    let working = |s: &Value| s["id"].as_str().is_some_and(|id| active.contains(id));
    let updated = |s: &Value| s["time"]["updated"].as_i64().unwrap_or(0);
    pool.sort_by(|a, b| working(b).cmp(&working(a)).then(updated(b).cmp(&updated(a))));
    if !sure && pool.len() > 1 {
        // The one whose words the TUI shows.
        if let Some(at) = screen.and_then(|screen| shown_on_screen(api, &pool, screen)) {
            return Ok(Some(Found {
                info: pool.swap_remove(at),
                guessed: false,
            }));
        }
    }
    let guessed = match &shown {
        Some(Some(_)) => !sure,
        // The TUI names none: its home, or a session not named yet.
        Some(None) => true,
        None => pool.len() > 1 || cwd.is_none(),
    };
    Ok(pool.into_iter().next().map(|info| Found { info, guessed }))
}

/// Sessions looked at for their words on the screen, at most.
const SCREEN_CANDIDATES: usize = 8;

/// Among sessions that could each be the pane's, the one more of whose
/// recent words are on the TUI's screen than any other's.
fn shown_on_screen(api: &Api, pool: &[Value], screen: &[String]) -> Option<usize> {
    let squeeze = |text: &str| text.split_whitespace().collect::<Vec<_>>().join(" ");
    let shown = squeeze(&screen.join("\n"));
    let mut best: Option<(usize, usize)> = None;
    let mut tied = false;
    for (at, session) in pool.iter().enumerate().take(SCREEN_CANDIDATES) {
        let Some(id) = session["id"].as_str() else {
            continue;
        };
        let Ok(listed) = api.get(&format!("/api/session/{}/message?limit=12&order=desc", encode(id))) else {
            continue;
        };
        let mut score = 0;
        for message in listed["data"].as_array().into_iter().flatten() {
            // What was said, as typed: the TUI shows it as it is.
            let Some(text) = message["text"].as_str().filter(|_| message["type"].as_str() == Some("user")) else {
                continue;
            };
            let first = squeeze(text.lines().find(|line| !line.trim().is_empty()).unwrap_or_default());
            let probe: String = first.chars().take(40).collect();
            if probe.chars().count() >= 4 && shown.contains(&probe) {
                score += 1;
            }
        }
        match best {
            Some((_, high)) if score < high => {}
            Some((_, high)) if score == high => tied = true,
            _ => {
                best = Some((at, score));
                tied = false;
            }
        }
    }
    let (at, score) = best?;
    (score > 0 && !tied).then_some(at)
}

/// The folder a session works in: its location's, and the part of it it
/// was started in.
fn session_dir(session: &Value) -> Option<PathBuf> {
    let dir = PathBuf::from(session["location"]["directory"].as_str()?);
    match session["subpath"].as_str().filter(|sub| !sub.is_empty()) {
        Some(sub) => Some(dir.join(sub)),
        None => Some(dir),
    }
}

fn same_dir(a: &Path, b: &Path) -> bool {
    let canonical = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    canonical(a) == canonical(b)
}

/// A model as OpenCode names one: its provider, and its own id.
fn model_id(model: &Value) -> Option<String> {
    Some(format!("{}/{}", model["providerID"].as_str()?, model["id"].as_str()?))
}

/// What the bridge holds of the session it chats about.
struct Chat {
    session: String,
    title: Option<String>,
    guessed: bool,
    state: AgentState,
    /// When the running turn started (unix ms, the service's clock).
    started: Option<u64>,
    /// The cursor to the messages before those sent.
    older: Option<String>,
    /// History left out of the last page to fit it in a line: sent first
    /// when more is asked for.
    held: Vec<Item>,
    /// Approvals and questions asked, by id: whether each is a question
    /// (a form, answered in the terminal).
    asks: HashMap<String, Ask>,
    /// The tools called, by id: name and input, to say what an approval
    /// is for and what a finished one did.
    tools: HashMap<String, (String, Value)>,
    /// The model the session talks to, and its variant.
    model: Option<String>,
    effort: Option<String>,
    models: Vec<ModelChoice>,
    /// The models' context windows, by id.
    windows: HashMap<String, u64>,
    /// OpenCode's commands where the pane is.
    commands: Vec<CommandInfo>,
    /// What the agent is writing now, as far as it has come.
    writing: Option<Writing>,
    /// What the last step took of the context, and the session's totals.
    used: Option<u64>,
    totals: Usage,
    /// The pane's title as last seen, and when it is next looked at.
    pane_title: Option<String>,
    follow_at: Instant,
    cwd: Option<PathBuf>,
    /// How long the turns read worked (`Ev::Worked`): each said after the
    /// page holding what it follows.
    worked: Vec<Ev>,
    /// The last item sent, which a turn that ends is said to have worked
    /// after.
    last_item: Option<String>,
    /// The pane shows a session not found yet: nothing is sent until it
    /// is.
    unresolved: bool,
    /// The step that was being written when the chat opened: its words
    /// so far are not kept by the service (it keeps each part once it is
    /// written), so it is shown whole once written, not from the middle.
    joined: Option<String>,
    /// Sessions its sub-agents run in, the newest first: what they ask
    /// waits on the turn.
    children: Vec<String>,
    /// The session each ask is of, when it is a sub-agent's.
    ask_sessions: HashMap<String, String>,
    /// When it last began working (unix ms), as the service says.
    busy: Option<u64>,
    /// The pane was seen without OpenCode at the last look.
    gone_seen: bool,
    /// The end of a turn whose start is on a page not read yet, said once
    /// that page is.
    open_end: Option<OpenEnd>,
}

struct Writing {
    id: String,
    thinking: bool,
    text: String,
    sent: Instant,
    owed: bool,
}

impl Chat {
    fn new(found: &Found, cwd: Option<PathBuf>, pane_title: Option<String>) -> Self {
        let mut chat = Self {
            session: found.id().to_string(),
            title: None,
            guessed: found.guessed,
            state: AgentState::Idle,
            started: None,
            older: None,
            held: Vec::new(),
            asks: HashMap::new(),
            tools: HashMap::new(),
            model: None,
            effort: None,
            models: Vec::new(),
            windows: HashMap::new(),
            commands: Vec::new(),
            writing: None,
            used: None,
            totals: Usage::default(),
            pane_title,
            follow_at: Instant::now() + FOLLOW_EVERY,
            cwd,
            worked: Vec::new(),
            last_item: None,
            unresolved: false,
            joined: None,
            children: Vec::new(),
            ask_sessions: HashMap::new(),
            busy: None,
            gone_seen: false,
            open_end: None,
        };
        chat.took(&found.info);
        chat
    }

    /// What a session's info says of its name, model and totals.
    fn took(&mut self, info: &Value) {
        self.title = own_title(&info["title"]);
        self.busy = info["time"]["busy"].as_u64();
        if let Some(model) = model_id(&info["model"]) {
            self.model = Some(model);
            self.effort = variant_of(&info["model"]);
        }
        self.totals = usage_of(&info["tokens"], info["cost"].as_f64());
    }

    fn hello(&self) -> Ev {
        Ev::Hello {
            protocol: crate::PROTOCOL,
            agent: "opencode".into(),
            title: self.title.clone(),
            guessed: self.guessed,
        }
    }

    fn models_ev(&self) -> Ev {
        Ev::Models {
            current: self.model.clone(),
            effort: self.effort.clone(),
            models: self.models.clone(),
        }
    }

    fn commands_ev(&self) -> Ev {
        let own = OWN_COMMANDS.iter().map(|(name, description, args)| CommandInfo {
            name: name.to_string(),
            description: description.to_string(),
            args: args.map(str::to_string),
        });
        Ev::Commands {
            commands: own.chain(self.commands.iter().cloned()).collect(),
        }
    }

    fn context_ev(&self, parts: Vec<ContextPart>) -> Ev {
        Ev::Context {
            context: ContextInfo {
                used: self.used,
                window: self.model.as_ref().and_then(|model| self.windows.get(model).copied()),
                parts,
                compacts_at: None,
                totals: self.totals.clone(),
            },
        }
    }

    /// More of what the agent writes (or thinks): sent as it grows, a few
    /// times a second.
    fn wrote(&mut self, out: &Out, id: String, thinking: bool, delta: &str) -> Result<()> {
        match &mut self.writing {
            Some(writing) if writing.id == id => writing.text.push_str(delta),
            _ => {
                // Another part began: the last one goes as far as it came.
                self.send_writing(out)?;
                self.writing = Some(Writing {
                    id,
                    thinking,
                    text: delta.to_string(),
                    // The first words go at once.
                    sent: Instant::now().checked_sub(WRITING_EVERY).unwrap_or_else(Instant::now),
                    owed: false,
                });
            }
        }
        let due = self.writing.as_ref().is_some_and(|w| w.sent.elapsed() >= WRITING_EVERY);
        if due {
            self.send_writing(out)?;
        } else if let Some(writing) = &mut self.writing {
            writing.owed = true;
        }
        Ok(())
    }

    /// How long until what was written and not sent is due.
    fn writing_due(&self) -> Option<Duration> {
        self.writing
            .as_ref()
            .filter(|w| w.owed)
            .map(|w| WRITING_EVERY.saturating_sub(w.sent.elapsed()))
    }

    fn send_writing(&mut self, out: &Out) -> Result<()> {
        let Some(writing) = &mut self.writing else {
            return Ok(());
        };
        writing.sent = Instant::now();
        writing.owed = false;
        if writing.text.trim().is_empty() {
            return Ok(());
        }
        let (id, text) = (writing.id.clone(), head(&writing.text, TEXT_KEPT));
        let item = if writing.thinking {
            Item::Thinking { id, text }
        } else {
            Item::Text { id, text }
        };
        self.send_item(out, item)
    }

    /// An item new or changed, which the turn's time is then said after.
    fn send_item(&mut self, out: &Out, item: Item) -> Result<()> {
        self.last_item = Some(item.id().to_string());
        out.send(&Ev::Item { item })?;
        Ok(())
    }

    /// A part written whole: the words as they came are done with.
    fn written(&mut self, out: &Out, id: String, thinking: bool, text: &str) -> Result<()> {
        if self.writing.as_ref().is_some_and(|w| w.id == id) {
            self.writing = None;
        }
        if text.trim().is_empty() {
            return Ok(());
        }
        let text = head(text, TEXT_KEPT);
        self.send_item(out, if thinking { Item::Thinking { id, text } } else { Item::Text { id, text } })
    }

    /// A page of history, then how long its turns worked.
    fn send_page(&mut self, out: &Out, items: Vec<Item>, reset: bool) -> Result<()> {
        let (items, more) = self.page(items);
        let ids: HashSet<&str> = items.iter().map(Item::id).collect();
        let worked: Vec<&Ev> = self
            .worked
            .iter()
            .filter(|ev| matches!(ev, Ev::Worked { after, .. } if ids.contains(after.as_str())))
            .collect();
        out.send(&Ev::History {
            items: items.clone(),
            more,
            reset,
        })?;
        for ev in worked {
            out.send(ev)?;
        }
        Ok(())
    }

    /// A page of history as one line holds it, the rest held back for the
    /// next ask: `(items, more)`.
    fn page(&mut self, items: Vec<Item>) -> (Vec<Item>, bool) {
        let (held, page) = super::split_history(items);
        self.held = held;
        let more = !self.held.is_empty() || self.older.is_some();
        (page, more)
    }

    fn state_now(&self) -> AgentState {
        if !self.asks.is_empty() {
            AgentState::Blocked
        } else if self.started.is_some() {
            AgentState::Working
        } else {
            AgentState::Idle
        }
    }

    fn set_state(&mut self, out: &Out) -> Result<()> {
        let state = self.state_now();
        if self.state != state {
            self.state = state;
            out.send(&Ev::State {
                state,
                since: Some(now()),
            })?;
        }
        Ok(())
    }

    /// Whether `when` is due: the pane's title is to be looked at.
    fn follow_due(&self) -> Duration {
        self.follow_at.saturating_duration_since(Instant::now())
    }

    /// How long the loop may wait for its next input.
    fn wait(&self) -> Duration {
        let mut wait = self.follow_due();
        if let Some(due) = self.writing_due() {
            wait = wait.min(due);
        }
        wait
    }
}

/// The variant a model reference names, unless it is the default one.
fn variant_of(model: &Value) -> Option<String> {
    model["variant"].as_str().filter(|v| !v.is_empty() && *v != "default").map(str::to_string)
}

fn usage_of(tokens: &Value, cost: Option<f64>) -> Usage {
    let n = |v: &Value| v.as_u64();
    Usage {
        input: n(&tokens["input"]),
        output: n(&tokens["output"]),
        cache_read: n(&tokens["cache"]["read"]),
        cache_write: n(&tokens["cache"]["write"]),
        reasoning: n(&tokens["reasoning"]),
        cost,
    }
}

/// What a step's model call took of the context: all it read, and what
/// it wrote, which the next call reads.
fn context_used(tokens: &Value) -> Option<u64> {
    let n = |v: &Value| v.as_u64().unwrap_or(0);
    let used = n(&tokens["input"]) + n(&tokens["cache"]["read"]) + n(&tokens["cache"]["write"]) + n(&tokens["output"]);
    (used > 0).then_some(used)
}

pub fn run(target: &Target, input: impl BufRead + Send + 'static, out: Out, pane: Option<Box<dyn PaneControl>>) -> Result<()> {
    let Located { tui, endpoints } = match locate(target) {
        Ok(located) => located,
        Err((need, message)) => {
            out.error(Some(need), message)?;
            return Ok(());
        }
    };
    // Where the TUI works: the folder it was started in, or given.
    let cwd = tui.directory.clone().or_else(|| target.cwd.clone());
    let directory = cwd.as_ref().map(|cwd| cwd.display().to_string());
    let pane_title = pane.as_ref().and_then(|pane| pane.title());
    let screen = pane.as_ref().and_then(|pane| pane.screen());
    let (mut api, found) = match pick(
        endpoints,
        directory,
        cwd.as_deref(),
        pane_title.as_deref(),
        tui.session.as_deref(),
        screen.as_deref(),
    ) {
        Ok(picked) => picked,
        Err((need, message)) => {
            out.error(Some(need), message)?;
            return Ok(());
        }
    };
    let (tx, rx) = mpsc::channel();
    let backlog = Arc::new(AtomicUsize::new(0));
    if let Err(err) = follow(&api.endpoint, tx.clone(), Arc::clone(&backlog)) {
        out.error(Some(Need::OpenCodeService), format!("{err:#}"))?;
        return Ok(());
    }
    read_ops(input, tx.clone());
    let mut chat = Chat::new(&found, cwd, pane_title);
    if !reopen(&api, &mut chat, &out)? {
        return Ok(());
    }

    loop {
        let input = match rx.recv_timeout(chat.wait()) {
            Ok(input) => Some(input),
            Err(mpsc::RecvTimeoutError::Timeout) => None,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        // Whatever came: what was written and not sent yet, once due, and a
        // look at the pane, once due (a busy service never lets the wait
        // run out).
        if chat.writing_due().is_some_and(|due| due.is_zero()) {
            chat.send_writing(&out)?;
        }
        if chat.follow_due().is_zero() {
            chat.follow_at = Instant::now() + FOLLOW_EVERY;
            if let Some(pane) = &pane {
                if !follow_pane(&api, &mut chat, &out, pane.as_ref())? {
                    break;
                }
            }
        }
        let Some(input) = input else {
            continue;
        };
        match input {
            Input::ClientGone => break,
            // The service restarts as OpenCode updates: it is looked for
            // again, wherever it is now, and what happened meanwhile is
            // read anew.
            Input::AgentGone(why) => match reconnect(target, &tx, &backlog, &api.endpoint) {
                Some(endpoint) => {
                    log::info!("agent chat: OpenCode's events stopped ({why}); found it again");
                    api.endpoint = endpoint;
                    if !reopen(&api, &mut chat, &out)? {
                        break;
                    }
                }
                None => {
                    out.error(Some(Need::OpenCodeService), format!("OpenCode's service went away: {why}"))?;
                    break;
                }
            },
            Input::Op(op) => {
                // What acts on a session goes to the one the pane shows now:
                // looked at again first, and refused when that is not the
                // one the chat was showing (the person sees the other one
                // and says it again), or is not found yet.
                let acts = matches!(op, Op::Send { .. } | Op::Interrupt | Op::Answer { .. } | Op::SetModel { .. });
                if acts {
                    let before = chat.session.clone();
                    if let Some(pane) = &pane {
                        if !follow_pane(&api, &mut chat, &out, pane.as_ref())? {
                            break;
                        }
                    }
                    if chat.unresolved {
                        out.refused("the pane shows a session that is not found yet")?;
                        continue;
                    }
                    if chat.session != before {
                        out.refused("the pane shows another session now: say it again there")?;
                        continue;
                    }
                }
                handle_op(&api, &mut chat, &out, op)?;
            }
            Input::Agent(event) => {
                backlog.fetch_sub(1, Ordering::SeqCst);
                if !handle_event(&api, &mut chat, &out, &event)? {
                    break;
                }
            }
        }
    }
    Ok(())
}

/// The service, found again and followed, within a while: the one it was,
/// wherever it listens now, or the only one there is.
fn reconnect(target: &Target, tx: &mpsc::Sender<Input<Value>>, backlog: &Arc<AtomicUsize>, was: &Endpoint) -> Option<Endpoint> {
    let until = Instant::now() + RECONNECT_FOR;
    while Instant::now() < until {
        std::thread::sleep(Duration::from_millis(500));
        let Ok(Located { endpoints, .. }) = locate(target) else {
            continue;
        };
        let endpoint = match endpoints.iter().find(|e| e.authority == was.authority && e.base == was.base) {
            Some(same) => same.clone(),
            None if endpoints.len() == 1 => endpoints[0].clone(),
            None => return None,
        };
        if follow(&endpoint, tx.clone(), Arc::clone(backlog)).is_ok() {
            return Some(endpoint);
        }
    }
    None
}

/// Says all of the session to the client: its name, history, what it does
/// and asks, its model, commands and context.
fn open(api: &Api, chat: &mut Chat, out: &Out) -> Result<()> {
    if let Ok(info) = api.get(&format!("/api/session/{}", encode(&chat.session))) {
        chat.took(info.get("data").unwrap_or(&info));
    }
    chat.children = children_of(api, &chat.session);
    out.send(&chat.hello())?;
    chat.writing = None;
    chat.held.clear();
    let listed = history(api, &chat.session, None)?;
    chat.older = listed.older;
    // The count read now: newer than one held from before a reconnect.
    if listed.used.is_some() {
        chat.used = listed.used;
    }
    remember_tools(chat, &listed.tools);
    chat.worked = listed.worked;
    chat.open_end = listed.open_end.clone();
    chat.last_item = listed.items.last().map(|item| item.id().to_string());
    chat.send_page(out, listed.items, true)?;
    chat.started = api
        .get("/api/session/active")
        .ok()
        .filter(|active| !active["data"][chat.session.as_str()].is_null())
        .map(|_| chat.busy.or(listed.turn_started).unwrap_or_else(now_ms));
    // A step being written: its words come whole once written.
    chat.joined = chat.started.and(listed.last_assistant.clone());
    chat.asks.clear();
    chat.ask_sessions.clear();
    for (ask, of) in pending_asks(api, chat) {
        chat.ask_sessions.insert(ask.id.clone(), of);
        chat.asks.insert(ask.id.clone(), ask.clone());
        out.send(&Ev::Ask { ask })?;
    }
    chat.state = chat.state_now();
    out.send(&Ev::State {
        state: chat.state,
        since: None,
    })?;
    match api.get(&format!("/api/model?{}", api.location())) {
        Ok(listed) => {
            (chat.models, chat.windows) = models_of(&listed);
        }
        Err(err) => log::info!("agent chat: no OpenCode models: {err:#}"),
    }
    out.send(&chat.models_ev())?;
    list_commands(api, chat);
    out.send(&chat.commands_ev())?;
    send_context(api, chat, out)?;
    Ok(())
}

fn now_ms() -> u64 {
    now() * 1000
}

fn list_commands(api: &Api, chat: &mut Chat) {
    match api.get(&format!("/api/command?{}", api.location())) {
        Ok(listed) => {
            chat.commands = listed["data"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|command| {
                    let name = command["name"].as_str()?;
                    Some(CommandInfo {
                        name: format!("/{name}"),
                        description: head(
                            &command["description"].as_str().unwrap_or_default().split_whitespace().collect::<Vec<_>>().join(" "),
                            DETAIL_KEPT,
                        ),
                        // What it is about follows it, if anything.
                        args: Some(String::new()),
                    })
                })
                .filter(|command| !OWN_COMMANDS.iter().any(|(own, _, _)| *own == command.name))
                .collect();
        }
        Err(err) => log::info!("agent chat: no OpenCode commands: {err:#}"),
    }
}

/// Sub-agents' sessions asked about at most, the newest.
const CHILDREN_ASKED: usize = 10;

/// The approvals and questions the session and its sub-agents wait on
/// now, each with the session it is of: the sub-agents working first,
/// then the newest.
fn pending_asks(api: &Api, chat: &Chat) -> Vec<(Ask, String)> {
    let mut asks = Vec::new();
    let active: HashSet<String> = api
        .get("/api/session/active")
        .ok()
        .and_then(|active| active["data"].as_object().map(|map| map.keys().cloned().collect()))
        .unwrap_or_default();
    let mut children = chat.children.clone();
    // Stable: the newest first among each.
    children.sort_by_key(|child| !active.contains(child));
    let sessions = std::iter::once(chat.session.clone()).chain(children.into_iter().take(CHILDREN_ASKED));
    for session in sessions {
        let encoded = encode(&session);
        if let Ok(listed) = api.get(&format!("/api/session/{encoded}/permission")) {
            for request in listed["data"].as_array().into_iter().flatten() {
                asks.push((permission_ask(chat, request), session.clone()));
            }
        }
        if let Ok(listed) = api.get(&format!("/api/session/{encoded}/form")) {
            for form in listed["data"].as_array().into_iter().flatten() {
                asks.extend(form_ask(form).map(|ask| (ask, session.clone())));
            }
        }
    }
    asks
}

/// The sessions the session's sub-agents run in, the newest first.
fn children_of(api: &Api, session: &str) -> Vec<String> {
    api.get(&format!("/api/session?parentID={}&limit={SESSIONS_LOOKED_AT}&order=desc", encode(session)))
        .ok()
        .and_then(|listed| {
            listed["data"].as_array().map(|data| {
                data.iter()
                    .filter_map(|child| child["id"].as_str().map(str::to_string))
                    .collect()
            })
        })
        .unwrap_or_default()
}

/// Says all of the session again, or that it cannot: false then, and the
/// chat is over.
fn reopen(api: &Api, chat: &mut Chat, out: &Out) -> Result<bool> {
    match open(api, chat, out) {
        Ok(()) => Ok(true),
        Err(err) => {
            out.error(Some(Need::OpenCodeService), format!("{err:#}"))?;
            Ok(false)
        }
    }
}

/// A session's own name: none while it has OpenCode's first one.
fn own_title(title: &Value) -> Option<String> {
    title
        .as_str()
        .filter(|t| !t.trim().is_empty() && !default_title(t))
        .map(str::to_string)
}

/// Whether a name is the one OpenCode gives a session before it has its
/// own (`New session - 2026-11-24T08:00:00.000Z`): the TUI shows none.
fn default_title(title: &str) -> bool {
    let Some(stamp) = title.strip_prefix("New session - ").or_else(|| title.strip_prefix("Child session - ")) else {
        return false;
    };
    let shape = "0000-00-00T00:00:00.000Z";
    stamp.len() == shape.len()
        && stamp.bytes().zip(shape.bytes()).all(|(c, s)| if s == b'0' { c.is_ascii_digit() } else { c == s })
}

/// Follows the TUI to another session, which its title names. While the
/// pane shows one not found yet (the service is busy, or has not listed
/// it), `chat.unresolved` holds and it is looked for again at the next
/// look.
fn follow_pane(api: &Api, chat: &mut Chat, out: &Out, pane: &dyn PaneControl) -> Result<bool> {
    // OpenCode left the pane (its service may stay): the chat is over, once
    // that is seen twice in a row.
    if pane.agent() == super::AgentHere::Gone {
        if chat.gone_seen {
            out.error(None, "OpenCode is not running in the pane any more")?;
            return Ok(false);
        }
        chat.gone_seen = true;
    } else {
        chat.gone_seen = false;
    }
    let title = pane.title();
    if title == chat.pane_title && !chat.unresolved {
        return Ok(true);
    }
    let shown = shown_title(title.as_deref());
    let settled = |chat: &mut Chat| {
        chat.pane_title = title.clone();
        chat.unresolved = false;
    };
    match &shown {
        // Not the TUI's title (it quit, or it does not set one): the chat
        // stays where it is.
        None => {
            settled(chat);
            return Ok(true);
        }
        // The session's own name: it was renamed, here or there.
        Some(Some(name)) if chat.title.as_deref().is_some_and(|own| same_title(own, name)) => {
            settled(chat);
            return Ok(true);
        }
        _ => {}
    }
    let screen = pane.screen();
    match find_session(api, chat.cwd.as_deref(), title.as_deref(), None, screen.as_deref()) {
        Ok(Some(found)) if found.id() == chat.session => settled(chat),
        // Its home, or a session not yet named (which it names once it says
        // something): no reason to leave.
        Ok(Some(found)) if found.guessed && shown == Some(None) => settled(chat),
        // Several so named, none told apart: not this one, nor a guess --
        // held, and looked for again.
        Ok(Some(found)) if found.guessed => chat.unresolved = true,
        Ok(Some(found)) => {
            *chat = Chat::new(&found, chat.cwd.clone(), title);
            return reopen(api, chat, out);
        }
        Ok(None) | Err(_) => {
            chat.unresolved = matches!(shown, Some(Some(_)));
            if !chat.unresolved {
                settled(chat);
            }
        }
    }
    Ok(true)
}

/// Messages before `cursor` (the latest without one): as items oldest
/// first, and what else they tell.
fn history(api: &Api, session: &str, cursor: Option<&str>) -> Result<Messages> {
    let mut path = format!("/api/session/{}/message?limit={MESSAGES_AT_ONCE}", encode(session));
    // A cursor holds its order, which may not be given again.
    match cursor {
        Some(cursor) => path.push_str(&format!("&cursor={}", encode(cursor))),
        None => path.push_str("&order=desc"),
    }
    let listed = api.get(&path)?;
    let mut messages: Vec<Value> = listed["data"].as_array().cloned().unwrap_or_default();
    // Newest first as listed.
    messages.reverse();
    let older = listed["cursor"]["next"]
        .as_str()
        .filter(|_| messages.len() as u32 >= MESSAGES_AT_ONCE)
        .map(str::to_string);
    Ok(messages_to_items(&messages, older, api.directory.as_deref()))
}

/// The end of a turn that began on an older page: how long it worked is
/// said once that page is read.
#[derive(Clone, Debug, PartialEq)]
struct OpenEnd {
    /// The `Ev::Worked` id, as the turn's idle message names it.
    id: String,
    /// When it ended (unix ms).
    end: u64,
    /// What it worked after, when that is on the newer page.
    after: Option<String>,
}

struct Messages {
    items: Vec<Item>,
    older: Option<String>,
    /// What the newest step took of the context.
    used: Option<u64>,
    /// The tools called, as remembered for approvals.
    tools: Vec<(String, String, Value)>,
    /// When the last turn began, if it has not ended.
    turn_started: Option<u64>,
    /// How long each turn that ended worked, after its last item.
    worked: Vec<Ev>,
    /// The newest step's message.
    last_assistant: Option<String>,
    /// The first turn's end, when its start is on an older page.
    open_end: Option<OpenEnd>,
}

fn remember_tools(chat: &mut Chat, tools: &[(String, String, Value)]) {
    for (id, name, input) in tools {
        chat.tools.insert(id.clone(), (name.clone(), input.clone()));
    }
}

fn messages_to_items(messages: &[Value], older: Option<String>, cwd: Option<&str>) -> Messages {
    let mut items = Vec::new();
    let mut used = None;
    let mut tools = Vec::new();
    let mut turn_started = None;
    let mut worked = Vec::new();
    let mut last_assistant = None;
    let mut open_end = None;
    let mut said = false;
    for message in messages {
        let created = message["time"]["created"].as_u64();
        match message["type"].as_str() {
            Some("user") => {
                // What is added to a running turn does not start another.
                if turn_started.is_none() {
                    turn_started = created;
                }
                said = true;
                items.extend(user_item(message));
            }
            Some("assistant") => {
                last_assistant = message["id"].as_str().map(str::to_string);
                if let Some(tokens) = message.get("tokens").and_then(context_used) {
                    used = Some(tokens);
                }
                for part in message["content"].as_array().into_iter().flatten() {
                    // Only calls not done: one done needs its input no more.
                    let done = matches!(part["state"]["status"].as_str(), Some("completed") | Some("error"));
                    if part["type"].as_str() == Some("tool") && !done {
                        if let (Some(id), Some(name)) = (part["id"].as_str(), part["name"].as_str()) {
                            tools.push((id.to_string(), name.to_string(), part["state"]["input"].clone()));
                        }
                    }
                }
                items.extend(assistant_items(message, cwd));
            }
            Some("shell") => items.extend(shell_item(message)),
            Some("compaction") => items.extend(compaction_note(message)),
            Some("idle") => {
                // Why it failed, as said when it did.
                if message["outcome"].as_str() == Some("failed") {
                    if let (Some(id), Some(why)) = (message["id"].as_str(), message["error"]["message"].as_str()) {
                        items.push(Item::Note {
                            id: format!("{id}-failed"),
                            note: NoteKind::Failed,
                            text: why.to_string(),
                        });
                    }
                }
                let last = items.last().map(|item: &Item| item.id().to_string());
                match (turn_started.take(), created, message["id"].as_str()) {
                    (Some(start), Some(end), Some(id)) => {
                        if let Some(after) = last {
                            worked.push(Ev::Worked {
                                id: format!("{id}-worked"),
                                after,
                                seconds: end.saturating_sub(start) / 1000,
                            });
                        }
                    }
                    // A turn that began on an older page.
                    (None, Some(end), Some(id)) if !said && open_end.is_none() => {
                        open_end = Some(OpenEnd {
                            id: format!("{id}-worked"),
                            end,
                            after: last,
                        });
                    }
                    _ => {}
                }
            }
            Some("synthetic") => items.extend(synthetic_item(message)),
            Some("skill") => items.push(Item::Tool {
                id: message["id"].as_str().unwrap_or_default().to_string(),
                name: "skill".into(),
                detail: message["name"].as_str().unwrap_or_default().to_string(),
                status: Status::Done,
            }),
            // Switches of agent, model or folder, and what the agent was
            // told on the way: nothing said.
            _ => {}
        }
    }
    Messages {
        items,
        older,
        used,
        tools,
        turn_started,
        worked,
        last_assistant,
        open_end,
    }
}

/// What OpenCode put into the conversation itself, where it is something
/// to show: a sub-agent's result, as that sub-agent's row.
fn synthetic_item(message: &Value) -> Option<Item> {
    let meta = &message["metadata"];
    if meta["source"].as_str() != Some("subagent") {
        return None;
    }
    Some(Item::Tool {
        id: message["id"].as_str()?.to_string(),
        name: "agent".into(),
        detail: head(
            message["description"].as_str().or(meta["agent"].as_str()).unwrap_or_default(),
            DETAIL_KEPT,
        ),
        status: match meta["state"].as_str() {
            Some("completed") => Status::Done,
            Some("running") | Some("pending") => Status::Running,
            _ => Status::Failed,
        },
    })
}

/// What the person said, and the pictures and files they sent with it.
fn user_item(message: &Value) -> Option<Item> {
    if message["type"].as_str().is_some_and(|kind| kind != "user") {
        return None;
    }
    let id = message["id"].as_str()?.to_string();
    let text = message["text"].as_str().unwrap_or_default();
    Some(Item::User {
        id,
        text: head(text, TEXT_KEPT),
        attachments: attachments_of(&message["files"]),
    })
}

fn attachments_of(files: &Value) -> Vec<Attachment> {
    // Small pictures of one message, at most this much: the rest are named.
    let mut budget = THUMBS_KEPT;
    files
        .as_array()
        .into_iter()
        .flatten()
        .map(|file| {
            let mime = file["mime"].as_str().unwrap_or_default();
            let uri = file["source"]["uri"].as_str().or(file["uri"].as_str()).unwrap_or_default();
            let path = uri.strip_prefix("file://").map(percent_decoded);
            let image = mime.starts_with("image/");
            let thumb = if image {
                path.as_deref()
                    .and_then(super::thumb::of_file)
                    .or_else(|| {
                        file["data"]
                            .as_str()
                            .and_then(|data| super::thumb::of_data_url(&format!("data:{mime};base64,{data}")))
                    })
            } else {
                None
            };
            let thumb = thumb.filter(|thumb| {
                let fits = thumb.len() <= budget;
                budget = if fits { budget - thumb.len() } else { 0 };
                fits
            });
            let name = file["name"]
                .as_str()
                .map(str::to_string)
                .or_else(|| path.as_deref().map(|p| p.rsplit('/').next().unwrap_or(p).to_string()))
                .unwrap_or_else(|| "file".into());
            Attachment { name, image, path, thumb }
        })
        .collect()
}

/// A `file://` URL's path, its `%xx` read back.
fn percent_decoded(text: &str) -> String {
    let bytes = text.as_bytes();
    let hex = |b: u8| (b as char).to_digit(16);
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(high), Some(low)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push((high * 16 + low) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// What the agent did in one step: what it thought and said, the tools
/// it called, and how the step ended when it did not simply end.
fn assistant_items(message: &Value, cwd: Option<&str>) -> Vec<Item> {
    let Some(id) = message["id"].as_str() else {
        return Vec::new();
    };
    let mut items = Vec::new();
    let (mut texts, mut thoughts) = (0, 0);
    for part in message["content"].as_array().into_iter().flatten() {
        match part["type"].as_str() {
            Some("text") => {
                let text = part["text"].as_str().unwrap_or_default();
                if !text.trim().is_empty() {
                    items.push(Item::Text {
                        id: part_id(id, false, texts),
                        text: head(text, TEXT_KEPT),
                    });
                }
                texts += 1;
            }
            Some("reasoning") => {
                let text = part["text"].as_str().unwrap_or_default();
                if !text.trim().is_empty() {
                    items.push(Item::Thinking {
                        id: part_id(id, true, thoughts),
                        text: head(text, TEXT_KEPT),
                    });
                }
                thoughts += 1;
            }
            Some("tool") => items.extend(tool_item(part, cwd)),
            _ => {}
        }
    }
    if let Some(error) = message.get("error").filter(|e| e.is_object()) {
        let note = if error["type"].as_str() == Some("aborted") {
            NoteKind::Interrupted
        } else {
            NoteKind::Failed
        };
        items.push(Item::Note {
            id: format!("{id}-end"),
            note,
            text: if note == NoteKind::Failed {
                error["message"].as_str().unwrap_or_default().to_string()
            } else {
                String::new()
            },
        });
    }
    items
}

/// A text or reasoning part's id: its message's, and its place among the
/// parts of its kind (as the service's events number them).
fn part_id(message: &str, thinking: bool, ordinal: u64) -> String {
    format!("{message}:{}:{ordinal}", if thinking { "thinking" } else { "text" })
}

/// A tool part of a message as an item.
fn tool_item(part: &Value, cwd: Option<&str>) -> Option<Item> {
    let id = part["id"].as_str()?.to_string();
    let name = part["name"].as_str().unwrap_or_default();
    let state = &part["state"];
    Some(tool_as_item(id, name, &state["input"], state, cwd))
}

/// A tool call as an item: from what it was asked (`input`) and how it
/// went (`state`: its status, what came of it, what it told of itself).
fn tool_as_item(id: String, name: &str, input: &Value, state: &Value, cwd: Option<&str>) -> Item {
    let status = tool_status(state);
    let output = output_of(&state["content"]);
    let path = |key: &str| input[key].as_str().unwrap_or_default().to_string();
    let file = || {
        let p = path("path");
        shown(if p.is_empty() { path("filePath") } else { p }, cwd)
    };
    match name {
        "shell" | "bash" => Item::Command {
            id,
            command: head(input["command"].as_str().unwrap_or_default(), TEXT_KEPT),
            status,
            output: tail(&output, OUTPUT_KEPT),
            exit: state["metadata"]["exit"].as_i64().map(fit),
            looks: Vec::new(),
        },
        "read" => look(id, format!("read {}", file()), LookKind::Read, file(), status),
        "grep" => {
            let pattern = path("pattern");
            look(id, format!("grep {pattern}"), LookKind::Search, pattern, status)
        }
        "glob" => {
            let pattern = path("pattern");
            look(id, format!("glob {pattern}"), LookKind::Search, pattern, status)
        }
        "list" | "ls" => {
            let dir = if file().is_empty() { ".".to_string() } else { file() };
            look(id, format!("ls {dir}"), LookKind::List, dir, status)
        }
        "edit" | "write" | "patch" | "apply_patch" | "multiedit" => {
            let files = edited_files(name, input, &state["metadata"], cwd);
            if files.is_empty() {
                Item::Tool {
                    id,
                    name: name.to_string(),
                    detail: String::new(),
                    status,
                }
            } else {
                Item::Edit { id, status, files }
            }
        }
        "websearch" | "web_search" => Item::Tool {
            id,
            name: "web_search".into(),
            detail: input["query"].as_str().unwrap_or_default().to_string(),
            status,
        },
        "subagent" | "task" => Item::Tool {
            id,
            name: "agent".into(),
            detail: head(
                input["description"]
                    .as_str()
                    .or(input["prompt"].as_str())
                    .unwrap_or_default(),
                DETAIL_KEPT,
            ),
            status,
        },
        "question" => Item::Tool {
            id,
            name: "question".into(),
            detail: head(&questions_of(&input["questions"]), DETAIL_KEPT),
            status,
        },
        _ => Item::Tool {
            id,
            name: name.to_string(),
            detail: brief(input),
            status,
        },
    }
}

fn look(id: String, command: String, kind: LookKind, target: String, status: Status) -> Item {
    Item::Command {
        id,
        command,
        status,
        output: String::new(),
        exit: None,
        looks: vec![Look { kind, target }],
    }
}

fn tool_status(state: &Value) -> Status {
    match state["status"].as_str() {
        Some("completed") => {
            if state["metadata"]["exit"].as_i64().is_some_and(|code| code != 0) {
                Status::Failed
            } else {
                Status::Done
            }
        }
        Some("error") => {
            let kind = state["error"]["type"].as_str().unwrap_or_default().to_ascii_lowercase();
            if kind.contains("reject") || kind.contains("denied") || kind.contains("permission") {
                Status::Declined
            } else {
                Status::Failed
            }
        }
        _ => Status::Running,
    }
}

/// What a tool's content says, as text: its texts, without the exit code
/// it adds at the end (said apart).
fn output_of(content: &Value) -> String {
    let texts: Vec<&str> = content
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|part| part["text"].as_str())
        .filter(|text| !is_exit_line(text))
        .collect();
    texts.join("\n")
}

fn is_exit_line(text: &str) -> bool {
    text.trim()
        .strip_prefix("Exit code: ")
        .is_some_and(|code| code.trim_start_matches('-').chars().all(|c| c.is_ascii_digit()))
}

/// An exit code as the chat holds it, whatever its size.
fn fit(code: i64) -> i32 {
    code as i32
}

/// The files a change touched: as the tool tells once it is done (its
/// patches), else made up from what it was asked.
fn edited_files(name: &str, input: &Value, metadata: &Value, cwd: Option<&str>) -> Vec<FileEdit> {
    let told: Vec<FileEdit> = metadata["files"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|file| {
            let path = file["file"].as_str().or(file["path"].as_str())?;
            let patch = file["patch"].as_str().or(file["diff"].as_str()).unwrap_or_default();
            Some(file_edit(&shown(path.to_string(), cwd), patch))
        })
        .collect();
    if !told.is_empty() {
        return capped(told);
    }
    let path = input["path"].as_str().or(input["filePath"].as_str()).unwrap_or_default();
    if path.is_empty() {
        return Vec::new();
    }
    let lines = |text: &str| -> Vec<String> { text.lines().map(str::to_string).collect() };
    let diff = if name == "write" {
        // All of it is new, as far as the chat knows: a bare `@@`.
        let body: Vec<String> = lines(input["content"].as_str().unwrap_or_default())
            .into_iter()
            .map(|line| format!("+{line}"))
            .collect();
        format!("@@\n{}", body.join("\n"))
    } else {
        let old = lines(input["oldString"].as_str().or(input["old_string"].as_str()).unwrap_or_default());
        let new = lines(input["newString"].as_str().or(input["new_string"].as_str()).unwrap_or_default());
        let body: Vec<String> = old
            .into_iter()
            .map(|l| format!("-{l}"))
            .chain(new.into_iter().map(|l| format!("+{l}")))
            .collect();
        format!("@@\n{}", body.join("\n"))
    };
    vec![file_edit(&shown(path.to_string(), cwd), &diff)]
}

/// A path as the chat shows it: from the folder the agent works in, when
/// it is in it.
fn shown(path: String, cwd: Option<&str>) -> String {
    let Some(cwd) = cwd.map(|cwd| cwd.trim_end_matches('/')).filter(|cwd| !cwd.is_empty()) else {
        return path;
    };
    match path.strip_prefix(cwd).and_then(|rest| rest.strip_prefix('/')) {
        Some(rest) if !rest.is_empty() => rest.to_string(),
        _ => path,
    }
}

fn file_edit(path: &str, patch: &str) -> FileEdit {
    // From the first hunk on: the file's header names it already.
    let diff = match patch.find("\n@@") {
        Some(at) if !patch.starts_with("@@") => &patch[at + 1..],
        _ => patch,
    };
    let (added, removed) = crate::diff::count(diff);
    FileEdit {
        path: path.to_string(),
        added,
        removed,
        diff: head(diff, DIFF_KEPT),
    }
}

/// A change of many files keeps the diffs of the first few; the rest are
/// counted.
fn capped(mut files: Vec<FileEdit>) -> Vec<FileEdit> {
    for file in files.iter_mut().skip(DIFFS_KEPT) {
        file.diff.clear();
    }
    files
}

/// A tool's input, short: its words, or its first value that is one.
fn brief(input: &Value) -> String {
    let said = match input {
        Value::Object(map) => ["description", "query", "url", "prompt", "code", "name", "command"]
            .iter()
            .find_map(|key| map.get(*key).and_then(Value::as_str))
            .map(str::to_string)
            .or_else(|| map.values().find_map(|v| v.as_str().map(str::to_string))),
        Value::String(s) => Some(s.clone()),
        _ => None,
    };
    head(
        said.unwrap_or_default().lines().next().unwrap_or_default().trim(),
        DETAIL_KEPT,
    )
}

/// The questions a `question` tool asks, one a line with its choices.
fn questions_of(questions: &Value) -> String {
    let mut text = String::new();
    for question in questions.as_array().into_iter().flatten() {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(question["question"].as_str().unwrap_or_default());
        for (n, option) in question["options"].as_array().into_iter().flatten().enumerate() {
            text.push_str(&format!("\n{}. {}", n + 1, option["label"].as_str().unwrap_or_default()));
        }
    }
    text
}

/// A command the person ran in the TUI (`!cmd`).
fn shell_item(message: &Value) -> Option<Item> {
    let id = message["id"].as_str()?.to_string();
    let exit = message["exit"].as_i64().map(fit);
    let status = match message["status"].as_str() {
        Some("running") => Status::Running,
        Some("exited") if exit.is_none_or(|code| code == 0) => Status::Done,
        _ => Status::Failed,
    };
    Some(Item::Command {
        id,
        command: head(message["command"].as_str().unwrap_or_default(), TEXT_KEPT),
        status,
        output: tail(message["output"]["output"].as_str().unwrap_or_default(), OUTPUT_KEPT),
        exit,
        looks: Vec::new(),
    })
}

/// A summing up of earlier turns, once it is done or failed.
fn compaction_note(message: &Value) -> Option<Item> {
    let id = message["id"].as_str()?.to_string();
    match message["status"].as_str()? {
        "completed" => Some(Item::Note {
            id,
            note: NoteKind::Compacted,
            text: String::new(),
        }),
        "failed" => Some(Item::Note {
            id,
            note: NoteKind::Failed,
            text: message["error"]["message"].as_str().unwrap_or_default().to_string(),
        }),
        _ => None,
    }
}

/// An approval the session asks for.
fn permission_ask(chat: &Chat, request: &Value) -> Ask {
    let id = request["id"].as_str().unwrap_or_default().to_string();
    let action = request["action"].as_str().unwrap_or_default();
    let resources: Vec<&str> = request["resources"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
    let called = request["source"]["id"].as_str().and_then(|call| chat.tools.get(call));
    let kind = match action {
        "shell" | "bash" => AskKind::Command,
        "edit" | "write" | "patch" => AskKind::Edit,
        _ => AskKind::Tool,
    };
    let body = match (kind, called) {
        // The command whole, as it was asked to run.
        (AskKind::Command, Some((_, input))) if input["command"].is_string() => input["command"].as_str().unwrap_or_default().to_string(),
        (AskKind::Command, _) | (AskKind::Edit, _) => resources.join("\n"),
        (_, _) => {
            let what = called.map_or(action, |(name, _)| name.as_str());
            if resources.is_empty() || resources == ["*"] {
                what.to_string()
            } else {
                format!("{what}: {}", resources.join(", "))
            }
        }
    };
    Ask {
        id,
        kind,
        body: cut(&body, ASK_KEPT),
        reason: request["message"].as_str().filter(|m| !m.is_empty()).map(str::to_string),
        cwd: chat.cwd.as_ref().map(|cwd| cwd.display().to_string()),
        // OpenCode's "always" is no answer for this session: it saves a
        // rule for the whole project, of wider patterns than the command
        // shown. That is answered in its terminal, which says so.
        choices: vec![Choice::Accept, Choice::Decline],
    }
}

/// A question the agent asks (a form): answered in the terminal, which
/// shows its choices.
fn form_ask(form: &Value) -> Option<Ask> {
    let id = form["id"].as_str()?.to_string();
    let mut body = String::new();
    for field in form["fields"].as_array().into_iter().flatten() {
        if !body.is_empty() {
            body.push('\n');
        }
        body.push_str(
            field["description"]
                .as_str()
                .filter(|d| !d.is_empty())
                .or(field["title"].as_str())
                .unwrap_or_default(),
        );
        for (n, option) in field["options"].as_array().into_iter().flatten().enumerate() {
            body.push_str(&format!("\n{}. {}", n + 1, option["label"].as_str().unwrap_or_default()));
        }
    }
    if body.is_empty() {
        body = form["title"].as_str().unwrap_or_default().to_string();
    }
    Some(Ask {
        id,
        kind: AskKind::Tool,
        body: cut(&body, ASK_KEPT),
        reason: form["title"].as_str().map(str::to_string),
        cwd: None,
        choices: Vec::new(),
    })
}

/// The start of `text`, and a word that it goes on when cut.
fn cut(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    format!("{}\n…", head(text, max))
}

/// The chat models OpenCode offers where the pane is, and their windows.
fn models_of(listed: &Value) -> (Vec<ModelChoice>, HashMap<String, u64>) {
    let mut models = Vec::new();
    let mut windows = HashMap::new();
    let all: Vec<&Value> = listed["data"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|model| model["type"].is_null() || model["type"].as_str() == Some("chat"))
        .filter(|model| !matches!(model["status"].as_str(), Some("deprecated") | Some("disabled")))
        .collect();
    for model in &all {
        let Some(id) = model_id(model) else {
            continue;
        };
        let name = model["name"].as_str().unwrap_or(model["id"].as_str().unwrap_or_default());
        // A name two providers both give says which one.
        let twice = all.iter().filter(|other| other["name"].as_str() == Some(name)).count() > 1;
        if let Some(window) = model["limit"]["context"].as_u64() {
            windows.insert(id.clone(), window);
        }
        models.push(ModelChoice {
            name: if twice {
                format!("{name} ({})", model["providerID"].as_str().unwrap_or_default())
            } else {
                name.to_string()
            },
            efforts: model["variants"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|variant| variant["id"].as_str().map(str::to_string))
                .filter(|variant| variant != "default")
                .collect(),
            id,
        });
    }
    (models, windows)
}

/// How full the context is: the last step's count, and what the session
/// holds now, estimated part by part.
fn send_context(api: &Api, chat: &mut Chat, out: &Out) -> Result<()> {
    let parts = match api.get(&format!("/api/session/{}/context", encode(&chat.session))) {
        Ok(held) => context_parts(held.get("data").unwrap_or(&held), chat.used),
        Err(err) => {
            log::info!("agent chat: no OpenCode context: {err:#}");
            Vec::new()
        }
    };
    out.send(&chat.context_ev(parts))?;
    Ok(())
}

/// What the messages in the context are made of, in tokens: estimated
/// from their text (four characters a token, a wide one a token each),
/// scaled to what the last step counted, the rest of which is the agent's
/// own instructions and tools.
fn context_parts(messages: &Value, used: Option<u64>) -> Vec<ContextPart> {
    let mut counted: Vec<(PartKind, u64)> = Vec::new();
    let mut add = |kind: PartKind, text: &str| {
        let tokens = estimate(text);
        if tokens == 0 {
            return;
        }
        match counted.iter_mut().find(|(k, _)| *k == kind) {
            Some((_, n)) => *n += tokens,
            None => counted.push((kind, tokens)),
        }
    };
    for message in messages.as_array().into_iter().flatten() {
        match message["type"].as_str() {
            Some("user") | Some("synthetic") => add(PartKind::User, message["text"].as_str().unwrap_or_default()),
            Some("assistant") => {
                for part in message["content"].as_array().into_iter().flatten() {
                    match part["type"].as_str() {
                        Some("text") => add(PartKind::Assistant, part["text"].as_str().unwrap_or_default()),
                        Some("reasoning") => add(PartKind::Thinking, part["text"].as_str().unwrap_or_default()),
                        Some("tool") => {
                            add(PartKind::ToolUse, &part["state"]["input"].to_string());
                            add(PartKind::ToolUse, &output_of(&part["state"]["content"]));
                        }
                        _ => {}
                    }
                }
            }
            Some("compaction") => add(PartKind::Summary, message["summary"].as_str().unwrap_or_default()),
            Some("shell") => add(PartKind::ToolUse, message["output"]["output"].as_str().unwrap_or_default()),
            _ => {}
        }
    }
    let estimated: u64 = counted.iter().map(|(_, n)| n).sum();
    if let Some(used) = used.filter(|&used| used > 0) {
        if estimated > used {
            // Scaled down to what was counted.
            for (_, n) in counted.iter_mut() {
                *n = *n * used / estimated.max(1);
            }
        } else {
            counted.insert(0, (PartKind::System, used - estimated));
        }
    }
    counted
        .into_iter()
        .filter(|(_, tokens)| *tokens > 0)
        .map(|(kind, tokens)| ContextPart { kind, tokens })
        .collect()
}

/// Tokens in `text`, roughly: four characters each, a wide character one.
fn estimate(text: &str) -> u64 {
    let (mut narrow, mut wide) = (0u64, 0u64);
    for c in text.chars() {
        if (c as u32) < 0x2E80 {
            narrow += 1;
        } else {
            wide += 1;
        }
    }
    narrow.div_ceil(4) + wide
}

fn handle_op(api: &Api, chat: &mut Chat, out: &Out, op: Op) -> Result<()> {
    let session = encode(&chat.session);
    match op {
        Op::Send { text, attachments } => {
            if text.trim().is_empty() && attachments.is_empty() {
                return Ok(());
            }
            if attachments.is_empty() && own_command(api, chat, out, &text)? {
                return Ok(());
            }
            let mut words = text.clone();
            for file in attachments.iter().filter(|file| !file.image) {
                if let Some(path) = &file.path {
                    if !words.is_empty() {
                        words.push('\n');
                    }
                    words.push_str(path);
                }
            }
            let files: Vec<Value> = attachments
                .iter()
                .filter(|file| file.image)
                .filter_map(|picture| {
                    let path = picture.path.as_deref()?;
                    Some(json!({ "uri": format!("file://{}", path_in_url(path)), "name": picture.name }))
                })
                .collect();
            let mut body = json!({ "text": words, "delivery": "steer" });
            if !files.is_empty() {
                body["files"] = Value::Array(files);
            }
            if let Err(err) = api.post(&format!("/api/session/{session}/prompt"), &body) {
                out.refused(format!("{err:#}"))?;
            }
        }
        Op::Interrupt => match api.post(&format!("/api/session/{session}/interrupt"), &json!({})) {
            Ok(answer) if answer["interrupted"].as_bool() == Some(false) => out.refused("nothing is running")?,
            Ok(_) => {}
            Err(err) => out.refused(format!("{err:#}"))?,
        },
        Op::Answer { ask, choice } => match chat.asks.get(&ask) {
            Some(asked) if asked.choices.is_empty() => out.refused("this question is answered in the terminal")?,
            Some(asked) if !asked.choices.contains(&choice) => {
                out.refused("OpenCode keeps \"always\" for the whole project: answer that in its terminal")?
            }
            Some(_) => {
                let decision = match choice {
                    Choice::Decline => "reject",
                    _ => "once",
                };
                // A sub-agent's is answered in its own session.
                let of = chat.ask_sessions.get(&ask).map_or(session.clone(), |of| encode(of));
                match api.post(
                    &format!("/api/session/{of}/permission/{}/reply", encode(&ask)),
                    &json!({ "decision": decision }),
                ) {
                    Ok(_) => {
                        chat.asks.remove(&ask);
                        chat.ask_sessions.remove(&ask);
                        out.send(&Ev::Resolved { id: ask })?;
                        chat.set_state(out)?;
                    }
                    Err(err) => out.refused(format!("{err:#}"))?,
                }
            }
            None => out.refused("that question is no longer asked")?,
        },
        // What the last page left out goes first: it is newer than the
        // messages before it.
        Op::Older if !chat.held.is_empty() => {
            let held = std::mem::take(&mut chat.held);
            chat.send_page(out, held, false)?;
        }
        Op::Older => match chat.older.take() {
            Some(cursor) => match history(api, &chat.session, Some(&cursor)) {
                Ok(listed) => {
                    chat.older = listed.older;
                    remember_tools(chat, &listed.tools);
                    chat.worked.extend(listed.worked);
                    // A turn that ended on the newer page and began on this
                    // one: said now, after what it ended with.
                    let mut across = None;
                    match (listed.turn_started, chat.open_end.take()) {
                        (Some(start), Some(end)) => {
                            let after = end.after.or_else(|| listed.items.last().map(|item| item.id().to_string()));
                            if let Some(after) = after {
                                across = Some(Ev::Worked {
                                    id: end.id,
                                    after,
                                    seconds: end.end.saturating_sub(start) / 1000,
                                });
                            }
                            chat.open_end = listed.open_end;
                        }
                        // It began further back still.
                        (_, kept) => chat.open_end = kept.or(listed.open_end),
                    }
                    let on_page = |ev: &Ev| matches!(ev, Ev::Worked { after, .. } if listed.items.iter().any(|item| item.id() == after));
                    // What follows an item on this page goes with it; what
                    // follows one the client holds already goes now.
                    let now = across.clone().filter(|ev| !on_page(ev));
                    chat.worked.extend(across);
                    chat.send_page(out, listed.items, false)?;
                    if let Some(ev) = now {
                        out.send(&ev)?;
                    }
                }
                Err(err) => {
                    chat.older = Some(cursor);
                    out.refused(format!("{err:#}"))?;
                }
            },
            None => out.send(&Ev::History {
                items: Vec::new(),
                more: false,
                reset: false,
            })?,
        },
        Op::SetModel { model, effort } => {
            let Some((provider, id)) = model.split_once('/') else {
                out.refused(format!("not a model OpenCode names: {model}"))?;
                return Ok(());
            };
            if !chat.models.is_empty() && !chat.models.iter().any(|known| known.id == model) {
                out.refused(format!("OpenCode has no model {model} here"))?;
                return Ok(());
            }
            let mut reference = json!({ "id": id, "providerID": provider });
            if let Some(effort) = &effort {
                reference["variant"] = json!(effort);
            }
            match api.post(&format!("/api/session/{session}/model"), &json!({ "model": reference })) {
                Ok(_) => {
                    chat.model = Some(model);
                    chat.effort = effort;
                    out.send(&chat.models_ev())?;
                    send_context(api, chat, out)?;
                }
                Err(err) => out.refused(format!("{err:#}"))?,
            }
        }
        Op::Install => out.refused("OpenCode needs nothing installed")?,
        Op::Reload => out.refused("OpenCode has no extension to load")?,
    }
    Ok(())
}

/// A path as a `file://` URL holds it.
fn path_in_url(path: &str) -> String {
    path.split('/').map(encode).collect::<Vec<_>>().join("/")
}

/// One of the commands OpenCode runs, sent as words: run as the TUI would,
/// or said to need it. False for words to send on.
fn own_command(api: &Api, chat: &mut Chat, out: &Out, text: &str) -> Result<bool> {
    let text = text.trim();
    let Some(rest) = text.strip_prefix('/') else {
        return Ok(false);
    };
    let (name, args) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    let args = args.trim();
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == ':') {
        return Ok(false);
    }
    let session = encode(&chat.session);
    let done = match name {
        "compact" | "summarize" => api.post(&format!("/api/session/{session}/compact"), &json!({ "delivery": "steer" })).map(|_| ()),
        "rename" => {
            if args.is_empty() {
                out.refused("/rename needs a name")?;
                return Ok(true);
            }
            api.call("PATCH", &format!("/api/session/{session}"), Some(&json!({ "title": args })))
                .map(|_| ())
        }
        "model" => {
            if args.is_empty() {
                out.refused("/model takes a model's id: pick one from /model in the list")?;
            } else {
                let (model, effort) = match args.split_once(char::is_whitespace) {
                    Some((model, effort)) => (model.to_string(), Some(effort.trim().to_string())),
                    None => (args.to_string(), None),
                };
                handle_op(api, chat, out, Op::SetModel { model, effort })?;
            }
            return Ok(true);
        }
        _ if chat.commands.iter().any(|command| command.name == format!("/{name}")) => api
            .post(
                &format!("/api/session/{session}/command"),
                &json!({ "name": name, "text": args, "delivery": "steer" }),
            )
            .map(|_| ()),
        _ if TERMINAL_COMMANDS.contains(&name) => {
            out.refused(format!("/{name} works in the terminal only"))?;
            return Ok(true);
        }
        _ => return Ok(false),
    };
    if let Err(err) = done {
        out.refused(format!("{err:#}"))?;
    }
    Ok(true)
}

/// Returns whether the chat goes on.
fn handle_event(api: &Api, chat: &mut Chat, out: &Out, event: &Value) -> Result<bool> {
    let kind = event["type"].as_str().unwrap_or_default();
    let data = &event["data"];
    // Events of every session come: only this one's are taken, but for the
    // commands and sessions of where the pane is.
    let ours = data["sessionID"].as_str() == Some(chat.session.as_str());
    // What its sub-agents ask waits on its turn too.
    let (own, children) = (chat.session.clone(), chat.children.clone());
    let mine = |of: Option<&str>| of.is_some_and(|of| of == own || children.iter().any(|child| child == of));
    match kind {
        "session.created" => {
            let parent = data["parentID"].as_str().or(data["parent"].as_str());
            if let (true, Some(child)) = (mine(parent), data["sessionID"].as_str()) {
                if !chat.children.iter().any(|known| known == child) {
                    chat.children.insert(0, child.to_string());
                }
            }
        }
        // Models came or went (a provider connected, or finished starting).
        "model.updated" | "provider.updated" => {
            if let Ok(listed) = api.get(&format!("/api/model?{}", api.location())) {
                (chat.models, chat.windows) = models_of(&listed);
                out.send(&chat.models_ev())?;
            }
        }
        // Taken back before it reached the agent: the chat is said anew,
        // without it (nothing takes one item out).
        "session.inbox.cancelled" if ours => {
            if !reopen(api, chat, out)? {
                return Ok(false);
            }
        }
        "session.skill.activated" if ours => {
            // As the service keeps it: a message named for the event.
            let id = event["id"].as_str().unwrap_or_default().replacen("evt_", "msg_", 1);
            chat.send_item(
                out,
                Item::Tool {
                    id,
                    name: "skill".into(),
                    detail: data["name"].as_str().unwrap_or_default().to_string(),
                    status: Status::Done,
                },
            )?;
        }
        "command.updated" => {
            list_commands(api, chat);
            out.send(&chat.commands_ev())?;
        }
        "session.renamed" if ours => {
            chat.title = own_title(&data["title"]);
            out.send(&chat.hello())?;
        }
        "session.deleted" if ours => {
            out.error(Some(Need::OpenCodeSession), "the session was deleted")?;
            return Ok(false);
        }
        "session.model.selected" if ours => {
            if let Some(model) = model_id(&data["model"]) {
                chat.model = Some(model);
                chat.effort = variant_of(&data["model"]);
                out.send(&chat.models_ev())?;
            }
        }
        // The session's own totals, every step's.
        "session.usage.updated" if ours => {
            chat.totals = usage_of(&data["tokens"], data["cost"].as_f64());
        }
        "session.inbox.enqueued" if ours => {
            let item = &data["item"];
            if item["type"].as_str() == Some("user") {
                if let Some(id) = data["inboxID"].as_str() {
                    chat.send_item(
                        out,
                        Item::User {
                            id: id.to_string(),
                            text: head(item["payload"]["text"].as_str().unwrap_or_default(), TEXT_KEPT),
                            attachments: attachments_of(&item["payload"]["files"]),
                        },
                    )?;
                }
            }
        }
        "session.inbox.delivered" if ours => {
            // As it is kept now: its pictures as the agent took them.
            if let Some(id) = data["inboxID"].as_str() {
                if let Ok(message) = api.get(&format!("/api/session/{}/message/{}", encode(&chat.session), encode(id))) {
                    let message = message.get("data").unwrap_or(&message);
                    if let Some(item) = user_item(message).or_else(|| synthetic_item(message)) {
                        chat.send_item(out, item)?;
                    }
                }
            }
        }
        "session.execution.started" if ours => {
            chat.started = Some(event["created"].as_u64().unwrap_or_else(now_ms));
            chat.set_state(out)?;
        }
        "session.execution.succeeded" | "session.execution.failed" | "session.execution.interrupted" if ours => {
            chat.send_writing(out)?;
            chat.writing = None;
            if kind == "session.execution.failed" {
                if let Some(why) = data["error"]["message"].as_str() {
                    // As the idle message the service keeps names it.
                    let id = event["id"].as_str().unwrap_or_default().replacen("evt_", "msg_", 1);
                    chat.send_item(
                        out,
                        Item::Note {
                            id: format!("{id}-failed"),
                            note: NoteKind::Failed,
                            text: why.to_string(),
                        },
                    )?;
                }
            }
            if let (Some(start), Some(after)) = (chat.started.take(), chat.last_item.clone()) {
                let end = event["created"].as_u64().unwrap_or_else(now_ms);
                // The idle message the service keeps has the event's id.
                let id = event["id"].as_str().unwrap_or_default().replacen("evt_", "msg_", 1);
                let worked = Ev::Worked {
                    id: format!("{id}-worked"),
                    after,
                    seconds: end.saturating_sub(start) / 1000,
                };
                out.send(&worked)?;
                chat.worked.push(worked);
            }
            // What it asked is no longer asked; what its sub-agents ask, run
            // on their own, still is.
            let own = chat.session.clone();
            let ended: Vec<String> = chat
                .asks
                .keys()
                .filter(|id| chat.ask_sessions.get(*id).is_none_or(|of| *of == own))
                .cloned()
                .collect();
            for id in ended {
                chat.asks.remove(&id);
                chat.ask_sessions.remove(&id);
                out.send(&Ev::Resolved { id })?;
            }
            chat.set_state(out)?;
            if let Ok(info) = api.get(&format!("/api/session/{}", encode(&chat.session))) {
                let info = info.get("data").unwrap_or(&info);
                chat.totals = usage_of(&info["tokens"], info["cost"].as_f64());
            }
            send_context(api, chat, out)?;
        }
        // The step being written when the chat opened: its start is kept
        // nowhere, so it is not shown from the middle; it comes whole once
        // written (`.ended`, and the step's end).
        "session.text.delta" | "session.reasoning.delta" if ours && data["assistantMessageID"].as_str() == chat.joined.as_deref() => {}
        "session.text.delta" | "session.reasoning.delta" if ours => {
            let thinking = kind == "session.reasoning.delta";
            if let (Some(message), Some(delta)) = (data["assistantMessageID"].as_str(), data["delta"].as_str()) {
                let id = part_id(message, thinking, data["ordinal"].as_u64().unwrap_or(0));
                chat.wrote(out, id, thinking, delta)?;
            }
        }
        "session.text.ended" | "session.reasoning.ended" if ours => {
            let thinking = kind == "session.reasoning.ended";
            if let Some(message) = data["assistantMessageID"].as_str() {
                let id = part_id(message, thinking, data["ordinal"].as_u64().unwrap_or(0));
                chat.written(out, id, thinking, data["text"].as_str().unwrap_or_default())?;
            }
        }
        "session.tool.input.started" if ours => {
            if let (Some(id), Some(name)) = (data["id"].as_str(), data["name"].as_str()) {
                chat.tools.insert(id.to_string(), (name.to_string(), Value::Null));
            }
        }
        "session.tool.called" | "session.tool.success" | "session.tool.failed" if ours => {
            let Some(id) = data["id"].as_str() else {
                return Ok(true);
            };
            if kind == "session.tool.called" {
                let name = chat.tools.get(id).map(|(name, _)| name.clone()).unwrap_or_default();
                chat.tools.insert(id.to_string(), (name, data["input"].clone()));
            }
            // A call done needs its input no more: it is let go.
            let held = if kind == "session.tool.called" {
                chat.tools.get(id).cloned()
            } else {
                chat.tools.remove(id)
            };
            let Some((name, input)) = held else {
                return Ok(true);
            };
            if name.is_empty() {
                return Ok(true);
            }
            let state = match kind {
                "session.tool.called" => json!({ "status": "running" }),
                "session.tool.success" => json!({ "status": "completed", "content": data["content"], "metadata": data["metadata"] }),
                _ => json!({ "status": "error", "error": data["error"], "content": data["content"], "metadata": data["metadata"] }),
            };
            chat.send_item(out, tool_as_item(id.to_string(), &name, &input, &state, api.directory.as_deref()))?;
        }
        // A step's end: its message as kept, which corrects what was sent
        // as it came.
        "session.step.ended" | "session.step.failed" if ours => {
            chat.send_writing(out)?;
            chat.writing = None;
            if data["assistantMessageID"].as_str() == chat.joined.as_deref() {
                chat.joined = None;
            }
            if let Some(message) = data["assistantMessageID"].as_str() {
                if let Ok(kept) = api.get(&format!("/api/session/{}/message/{}", encode(&chat.session), encode(message))) {
                    for item in assistant_items(kept.get("data").unwrap_or(&kept), api.directory.as_deref()) {
                        chat.send_item(out, item)?;
                    }
                }
            }
            if let Some(used) = context_used(&data["tokens"]) {
                chat.used = Some(used);
            }
        }
        "session.shell.started" | "session.shell.ended" if ours => {
            let shell = &data["shell"];
            if let Some(id) = shell["id"].as_str() {
                let exit = shell["exit"].as_i64().map(fit);
                let running = kind == "session.shell.started";
                chat.send_item(
                    out,
                    Item::Command {
                        id: id.to_string(),
                        command: head(shell["command"].as_str().unwrap_or_default(), TEXT_KEPT),
                        status: if running {
                            Status::Running
                        } else if exit.is_some_and(|code| code != 0) {
                            Status::Failed
                        } else {
                            Status::Done
                        },
                        output: tail(data["output"]["output"].as_str().unwrap_or_default(), OUTPUT_KEPT),
                        exit,
                        looks: Vec::new(),
                    },
                )?;
            }
        }
        "session.compaction.ended" | "session.compaction.failed" if ours => {
            chat.send_item(
                out,
                Item::Note {
                    id: format!("{}-compacted", event["id"].as_str().unwrap_or_default()),
                    note: if kind == "session.compaction.ended" {
                        NoteKind::Compacted
                    } else {
                        NoteKind::Failed
                    },
                    text: data["error"]["message"].as_str().unwrap_or_default().to_string(),
                },
            )?;
            chat.used = None;
            send_context(api, chat, out)?;
        }
        "permission.asked" if mine(data["sessionID"].as_str()) => {
            let ask = permission_ask(chat, data);
            if let Some(of) = data["sessionID"].as_str() {
                chat.ask_sessions.insert(ask.id.clone(), of.to_string());
            }
            chat.asks.insert(ask.id.clone(), ask.clone());
            out.send(&Ev::Ask { ask })?;
            chat.set_state(out)?;
        }
        "permission.replied" if mine(data["sessionID"].as_str()) => {
            if let Some(id) = data["requestID"].as_str() {
                chat.ask_sessions.remove(id);
                if chat.asks.remove(id).is_some() {
                    out.send(&Ev::Resolved { id: id.to_string() })?;
                }
            }
            chat.set_state(out)?;
        }
        "form.created" => {
            let form = &data["form"];
            if mine(form["sessionID"].as_str()) {
                if let Some(ask) = form_ask(form) {
                    if let Some(of) = form["sessionID"].as_str() {
                        chat.ask_sessions.insert(ask.id.clone(), of.to_string());
                    }
                    chat.asks.insert(ask.id.clone(), ask.clone());
                    out.send(&Ev::Ask { ask })?;
                    chat.set_state(out)?;
                }
            }
        }
        "form.replied" | "form.cancelled" if mine(data["sessionID"].as_str()) => {
            if let Some(id) = data["id"].as_str() {
                chat.ask_sessions.remove(id);
                if chat.asks.remove(id).is_some() {
                    out.send(&Ev::Resolved { id: id.to_string() })?;
                }
            }
            chat.set_state(out)?;
        }
        _ => {}
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(json: &str) -> Value {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn titles_name_the_sessions_they_show() {
        assert_eq!(shown_title(Some("OC | Fix the demo check")), Some(Some("Fix the demo check".into())));
        assert_eq!(shown_title(Some("OpenCode")), Some(None));
        assert_eq!(shown_title(Some("bash")), None);
        assert_eq!(shown_title(None), None);
        assert!(same_title("Fix the demo check", "Fix the demo check"));
        let long = "A very long session name that goes past forty characters";
        let shown = format!("{}\u{2026}", long.chars().take(37).collect::<String>());
        assert!(same_title(long, &shown));
        assert!(!same_title("Another one entirely", &shown));
        assert!(same_title("tab\there", "tab here"), "control characters show as spaces");
        assert!(!same_title("x", "\u{2026}"));
    }

    #[test]
    fn a_step_becomes_thoughts_words_and_tools() {
        let step = message(
            r#"{"id":"msg_1","type":"assistant","content":[
                {"type":"reasoning","text":"thinking it over"},
                {"type":"text","text":"I'll look."},
                {"type":"tool","id":"call_1","name":"read","state":{"status":"completed","input":{"path":"/w/demo.txt"},"content":[{"type":"text","text":"1: alpha"}]}},
                {"type":"tool","id":"call_2","name":"shell","state":{"status":"completed","input":{"command":"sh check.sh"},
                  "content":[{"type":"text","text":"FAIL title\n"},{"type":"text","text":"Exit code: 1"}],"metadata":{"exit":1}}},
                {"type":"tool","id":"call_3","name":"edit","state":{"status":"completed","input":{"path":"/w/demo.txt","oldString":"alpha","newString":"ALPHA"},
                  "metadata":{"files":[{"file":"demo.txt","patch":"Index: demo.txt\n===\n--- demo.txt\n+++ demo.txt\n@@ -1,2 +1,2 @@\n-alpha\n+ALPHA\n beta\n"}]}}},
                {"type":"text","text":"Done."}
            ],"error":{"type":"aborted","message":"Step interrupted"}}"#,
        );
        let items = assistant_items(&step, Some("/w"));
        assert_eq!(items.len(), 7, "{items:#?}");
        assert!(matches!(&items[0], Item::Thinking { id, .. } if id == "msg_1:thinking:0"));
        assert!(matches!(&items[1], Item::Text { id, text } if id == "msg_1:text:0" && text == "I'll look."));
        assert!(matches!(&items[2], Item::Command { looks, command, .. } if looks[0].kind == LookKind::Read && looks[0].target == "demo.txt" && command == "read demo.txt"));
        match &items[3] {
            Item::Command { command, status, output, exit, .. } => {
                assert_eq!(command, "sh check.sh");
                assert_eq!(*status, Status::Failed);
                assert_eq!(output, "FAIL title\n");
                assert_eq!(*exit, Some(1));
            }
            other => panic!("{other:?}"),
        }
        match &items[4] {
            Item::Edit { files, status, .. } => {
                assert_eq!(*status, Status::Done);
                assert_eq!(files[0].path, "demo.txt");
                assert_eq!((files[0].added, files[0].removed), (1, 1));
                assert!(files[0].diff.starts_with("@@ -1,2 +1,2 @@"), "{}", files[0].diff);
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(&items[5], Item::Text { id, .. } if id == "msg_1:text:1"));
        assert!(matches!(&items[6], Item::Note { note: NoteKind::Interrupted, .. }));
    }

    #[test]
    fn an_edit_not_yet_done_is_made_up_from_what_was_asked() {
        let files = edited_files("edit", &json!({"path": "/w/a.txt", "oldString": "a\nb", "newString": "c"}), &Value::Null, Some("/w/"));
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "a.txt");
        assert_eq!((files[0].added, files[0].removed), (1, 2));
        let files = edited_files("write", &json!({"filePath": "/elsewhere/n.txt", "content": "one\ntwo\n"}), &Value::Null, Some("/w"));
        assert_eq!(files[0].path, "/elsewhere/n.txt", "outside the folder: whole");
        assert_eq!((files[0].added, files[0].removed), (2, 0));
        assert!(edited_files("edit", &json!({}), &Value::Null, None).is_empty());
    }

    #[test]
    fn history_says_who_said_what_and_how_long_turns_took() {
        let messages = message(
            r#"[
              {"id":"msg_u","type":"user","time":{"created":1000},"text":"look","files":[{"data":"","mime":"text/plain","source":{"type":"uri","uri":"file:///home/me/ThinkTerm_Uploads/a%20b.txt"},"name":"a b.txt"}]},
              {"id":"msg_a","type":"assistant","time":{"created":1500},"content":[{"type":"text","text":"ok"}],"tokens":{"input":900,"output":100,"reasoning":0,"cache":{"read":0,"write":0}}},
              {"id":"msg_s","type":"model-switched","time":{"created":1600}},
              {"id":"msg_i","type":"idle","time":{"created":4200},"outcome":"succeeded"},
              {"id":"msg_c","type":"compaction","time":{"created":5000},"status":"completed","reason":"manual","summary":"so far"},
              {"id":"msg_f","type":"compaction","time":{"created":6000},"status":"failed","reason":"manual","error":{"type":"x","message":"no"}},
              {"id":"msg_h","type":"shell","time":{"created":7000},"shellID":"sh","command":"ls","status":"exited","exit":2,"output":{"output":"nope","cursor":0,"size":4,"truncated":false}},
              {"id":"msg_y","type":"synthetic","time":{"created":8000},"text":"<background-result>…</background-result>","description":"Child check","metadata":{"source":"subagent","state":"completed"}},
              {"id":"msg_z","type":"synthetic","time":{"created":9000},"text":"context"}
            ]"#,
        );
        let listed = messages_to_items(messages.as_array().unwrap(), Some("next".into()), None);
        assert_eq!(listed.used, Some(1000));
        assert_eq!(listed.older.as_deref(), Some("next"));
        assert!(listed.turn_started.is_none(), "the turn ended");
        let kinds: Vec<String> = listed.items.iter().map(|item| item.id().to_string()).collect();
        assert_eq!(kinds, ["msg_u", "msg_a:text:0", "msg_c", "msg_f", "msg_h", "msg_y"]);
        assert!(matches!(&listed.items[5], Item::Tool { name, detail, status: Status::Done, .. } if name == "agent" && detail == "Child check"));
        assert_eq!(
            listed.worked,
            [Ev::Worked {
                id: "msg_i-worked".into(),
                after: "msg_a:text:0".into(),
                seconds: 3
            }]
        );
        match &listed.items[0] {
            Item::User { attachments, .. } => {
                assert_eq!(attachments[0].path.as_deref(), Some("/home/me/ThinkTerm_Uploads/a b.txt"));
                assert!(!attachments[0].image);
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(&listed.items[3], Item::Note { note: NoteKind::Failed, text, .. } if text == "no"));
        assert!(matches!(&listed.items[4], Item::Command { status: Status::Failed, exit: Some(2), .. }));
    }

    #[test]
    fn a_turn_split_across_pages_is_timed_once_its_start_is_read() {
        // The newer page: the turn's last step and its end.
        let newer = message(
            r#"[
              {"id":"msg_a","type":"assistant","time":{"created":5000},"content":[{"type":"text","text":"done"}]},
              {"id":"msg_i","type":"idle","time":{"created":9000},"outcome":"succeeded"},
              {"id":"msg_u2","type":"user","time":{"created":10000},"text":"next"}
            ]"#,
        );
        let listed = messages_to_items(newer.as_array().unwrap(), Some("older".into()), None);
        assert!(listed.worked.is_empty(), "its start is not here");
        assert_eq!(
            listed.open_end,
            Some(OpenEnd {
                id: "msg_i-worked".into(),
                end: 9000,
                after: Some("msg_a:text:0".into()),
            })
        );
        assert_eq!(listed.turn_started, Some(10000), "the running turn's start");
        // The older page: where it began.
        let older = message(r#"[{"id":"msg_u1","type":"user","time":{"created":1000},"text":"go"}]"#);
        let listed = messages_to_items(older.as_array().unwrap(), None, None);
        assert_eq!(listed.turn_started, Some(1000));
        assert_eq!(listed.open_end, None);
    }

    #[test]
    fn approvals_say_what_they_are_for() {
        let chat_with = |tools: &[(&str, &str, Value)]| {
            let found = Found {
                info: json!({"id": "ses_1"}),
                guessed: false,
            };
            let mut chat = Chat::new(&found, Some(PathBuf::from("/w")), None);
            for (id, name, input) in tools {
                chat.tools.insert(id.to_string(), (name.to_string(), input.clone()));
            }
            chat
        };
        let chat = chat_with(&[("call_1", "shell", json!({"command": "printf 'one\\ntwo\\n'; exit 3"}))]);
        let ask = permission_ask(
            &chat,
            &json!({"id": "per_1", "action": "shell", "resources": ["printf 'one\\ntwo\\n'", "exit 3"], "source": {"id": "call_1"}}),
        );
        assert_eq!(ask.kind, AskKind::Command);
        assert_eq!(ask.body, "printf 'one\\ntwo\\n'; exit 3");
        // OpenCode's "always" is a rule for the whole project: not offered.
        assert_eq!(ask.choices, [Choice::Accept, Choice::Decline]);
        assert_eq!(ask.cwd.as_deref(), Some("/w"));
        let ask = permission_ask(&chat, &json!({"id": "per_2", "action": "edit", "resources": ["demo.txt"]}));
        assert_eq!((ask.kind, ask.body.as_str()), (AskKind::Edit, "demo.txt"));
        let ask = permission_ask(&chat, &json!({"id": "per_3", "action": "webfetch", "resources": ["https://x"]}));
        assert_eq!((ask.kind, ask.body.as_str()), (AskKind::Tool, "webfetch: https://x"));

        let form = form_ask(&json!({"id": "frm_1", "title": "Questions", "fields": [
            {"key": "q0", "title": "方式", "description": "用哪种方式修？", "options": [{"label": "改文件"}, {"label": "改检查"}]}
        ]}))
        .unwrap();
        assert!(form.choices.is_empty(), "answered in the terminal");
        assert_eq!(form.body, "用哪种方式修？\n1. 改文件\n2. 改检查");
    }

    #[test]
    fn models_are_named_by_provider_and_keep_their_efforts() {
        let listed = json!({"data": [
            {"id": "mock-1", "providerID": "mock", "type": null, "name": "Mock 1", "variants": [{"id": "low"}, {"id": "high"}], "limit": {"context": 200000}},
            {"id": "m", "providerID": "a", "type": "chat", "name": "Same", "variants": []},
            {"id": "m", "providerID": "b", "type": "chat", "name": "Same", "variants": [{"id": "default"}]},
            {"id": "img", "providerID": "a", "type": "image", "name": "Pictures"}
        ]});
        let (models, windows) = models_of(&listed);
        let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["mock/mock-1", "a/m", "b/m"]);
        assert_eq!(models[0].efforts, ["low", "high"]);
        assert_eq!(models[1].name, "Same (a)");
        assert!(models[2].efforts.is_empty());
        assert_eq!(windows.get("mock/mock-1"), Some(&200000));
        assert_eq!(variant_of(&json!({"variant": "default"})), None);
        assert_eq!(variant_of(&json!({"variant": "high"})).as_deref(), Some("high"));
    }

    #[test]
    fn the_context_is_split_by_what_fills_it() {
        let held = json!([
            {"type": "user", "text": "abcdefgh"},
            {"type": "assistant", "content": [{"type": "text", "text": "abcd"}, {"type": "reasoning", "text": "思考"}]}
        ]);
        let parts = context_parts(&held, Some(100));
        let of = |kind: PartKind| parts.iter().find(|p| p.kind == kind).map(|p| p.tokens);
        assert_eq!(of(PartKind::User), Some(2));
        assert_eq!(of(PartKind::Assistant), Some(1));
        assert_eq!(of(PartKind::Thinking), Some(2));
        assert_eq!(of(PartKind::System), Some(95), "the rest is the agent's own");
        assert_eq!(context_used(&json!({"input": 10, "output": 2, "cache": {"read": 5, "write": 1}})), Some(18));
        assert_eq!(context_used(&json!({})), None);
    }

    #[test]
    fn a_turn_that_ends_leaves_its_sub_agents_asks() {
        let service = Service::start(&[]);
        let found = Found {
            info: json!({"id": "ses_p"}),
            guessed: false,
        };
        let mut chat = Chat::new(&found, None, None);
        let ask = |id: &str| Ask {
            id: id.into(),
            kind: AskKind::Command,
            body: "ls".into(),
            reason: None,
            cwd: None,
            choices: vec![Choice::Accept, Choice::Decline],
        };
        chat.asks.insert("per_own".into(), ask("per_own"));
        chat.asks.insert("per_child".into(), ask("per_child"));
        chat.ask_sessions.insert("per_own".into(), "ses_p".into());
        chat.ask_sessions.insert("per_child".into(), "ses_c".into());
        chat.children.push("ses_c".into());
        let out = Out::new(Box::new(std::io::sink()));
        let ended = json!({"id": "evt_1", "created": 2000, "type": "session.execution.succeeded", "data": {"sessionID": "ses_p"}});
        assert!(handle_event(&service.api, &mut chat, &out, &ended).unwrap());
        assert!(!chat.asks.contains_key("per_own"), "the turn's own ask ended with it");
        assert!(chat.asks.contains_key("per_child"), "the sub-agent still waits");
        assert_eq!(chat.state_now(), AgentState::Blocked);
    }

    #[test]
    fn a_given_server_is_shown_without_its_secrets() {
        assert_eq!(shown_url("http://me:pw@127.0.0.1:4096/oc?token=x#y"), "http://127.0.0.1:4096/oc");
        assert_eq!(shown_url("https://host"), "https://host");
    }

    #[test]
    fn paths_and_urls_are_written_both_ways() {
        assert_eq!(encode("/home/a b/ü"), "%2Fhome%2Fa%20b%2F%C3%BC");
        assert_eq!(path_in_url("/home/a b/x.png"), "/home/a%20b/x.png");
        assert_eq!(percent_decoded("/home/a%20b/%C3%BC.png"), "/home/a b/ü.png");
        assert_eq!(percent_decoded("100%"), "100%");
        assert_eq!(refusal(r#"{"message":"busy"}"#), "busy");
        assert_eq!(refusal("plain"), "plain");
    }

    /// A stand-in for OpenCode's service: each GET answered with the body
    /// of the first route its path starts with, else an empty list; the
    /// routes may change while it runs.
    struct Service {
        api: Api,
        routes: std::sync::Arc<std::sync::Mutex<Vec<(String, String)>>>,
    }

    impl Service {
        fn start(routes: &[(&str, Value)]) -> Self {
            use std::io::{Read, Write};
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let api = Api {
                endpoint: Endpoint {
                    authority: listener.local_addr().unwrap().to_string(),
                    base: String::new(),
                    auth: None,
                },
                directory: None,
            };
            let routes = std::sync::Arc::new(std::sync::Mutex::new(
                routes.iter().map(|(path, body)| (path.to_string(), body.to_string())).collect::<Vec<_>>(),
            ));
            let held = std::sync::Arc::clone(&routes);
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(mut stream) = stream else { return };
                    let mut asked = Vec::new();
                    let mut buf = [0u8; 4096];
                    while !asked.windows(4).any(|w| w == b"\r\n\r\n") {
                        match stream.read(&mut buf) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => asked.extend_from_slice(&buf[..n]),
                        }
                    }
                    let asked = String::from_utf8_lossy(&asked).to_string();
                    let path = asked.split_whitespace().nth(1).unwrap_or_default().to_string();
                    let body = held
                        .lock()
                        .unwrap()
                        .iter()
                        .find(|(route, _)| path.starts_with(route.as_str()))
                        .map(|(_, body)| body.clone())
                        .unwrap_or_else(|| r#"{"data":[]}"#.to_string());
                    let _ = stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}", body.len()).as_bytes());
                }
            });
            Self { api, routes }
        }

        fn route(&self, path: &str, body: Value) {
            let mut routes = self.routes.lock().unwrap();
            routes.retain(|(route, _)| route != path);
            routes.insert(0, (path.to_string(), body.to_string()));
        }
    }

    fn session(id: &str, title: Option<&str>, updated: i64) -> Value {
        json!({"id": id, "title": title, "time": {"updated": updated}, "location": {"directory": "/w"}})
    }

    #[test]
    fn the_session_the_title_names_or_none() {
        let page = |sessions: Vec<Value>, next: Option<&str>| json!({"data": sessions, "cursor": {"next": next}});
        let service = Service::start(&[("/api/session?", page(vec![session("ses_a", Some("Alpha"), 2), session("ses_b", None, 1)], None))]);
        let find = |title: &str, asked: Option<&str>| find_session(&service.api, None, Some(title), asked, None).unwrap();

        let found = find("OC | Alpha", None).unwrap();
        assert_eq!((found.id(), found.guessed), ("ses_a", false));
        // A default name is no name: the TUI shows "OpenCode" for it.
        assert!(default_title("New session - 2026-11-24T08:00:00.000Z"));
        assert!(default_title("Child session - 2026-11-24T08:00:00.000Z"));
        assert!(!default_title("New session - tomorrow"));
        assert_eq!(own_title(&json!("New session - 2026-11-24T08:00:00.000Z")), None);
        // Named, but no session has that name: none, not another.
        assert!(find("OC | Gamma", None).is_none());
        // The TUI names none: a guess, the unnamed one first.
        let found = find("OpenCode", None).unwrap();
        assert_eq!((found.id(), found.guessed), ("ses_b", true));

        // A session older than the first page is looked for on the next.
        let full: Vec<Value> = (0..SESSIONS_LOOKED_AT).map(|n| session(&format!("ses_{n}"), Some("Other"), 1000 + n as i64)).collect();
        service.route("/api/session?limit", page(full, Some("page2")));
        service.route(
            &format!("/api/session?limit={SESSIONS_LOOKED_AT}&parentID=null&archived=false&search=Old%20one&cursor=page2"),
            page(vec![session("ses_old", Some("Old one"), 1)], None),
        );
        let found = find("OC | Old one", None).unwrap();
        assert_eq!((found.id(), found.guessed), ("ses_old", false));

        // The one it was opened on, looked up by its id when not listed.
        service.route("/api/session/ses_given", json!({"data": session("ses_given", Some("Given"), 1)}));
        let found = find("bash", Some("ses_given")).unwrap();
        assert_eq!((found.id(), found.guessed), ("ses_given", false));
    }

    /// A pane whose title the test sets.
    struct Pane(std::sync::Arc<std::sync::Mutex<Option<String>>>);

    impl PaneControl for Pane {
        fn agent(&self) -> super::super::AgentHere {
            super::super::AgentHere::Resting
        }
        fn screen(&self) -> Option<Vec<String>> {
            None
        }
        fn type_line(&self, _line: &str) -> Result<()> {
            Ok(())
        }
        fn title(&self) -> Option<String> {
            self.0.lock().unwrap().clone()
        }
    }

    #[test]
    fn a_switch_not_found_yet_is_tried_again_and_holds_what_is_sent() {
        let page = |sessions: Vec<Value>| json!({"data": sessions, "cursor": {"next": null}});
        let service = Service::start(&[("/api/session?", page(vec![session("ses_a", Some("Alpha"), 2)]))]);
        // No sub-agents.
        service.route("/api/session?parentID=ses", page(vec![]));
        let title = std::sync::Arc::new(std::sync::Mutex::new(Some("OC | Alpha".to_string())));
        let pane = Pane(std::sync::Arc::clone(&title));
        let found = find_session(&service.api, None, pane.title().as_deref(), None, None).unwrap().unwrap();
        let mut chat = Chat::new(&found, None, pane.title());
        let out = Out::new(Box::new(std::io::sink()));

        // The TUI opens a session the service does not list yet.
        *title.lock().unwrap() = Some("OC | Beta".to_string());
        assert!(follow_pane(&service.api, &mut chat, &out, &pane).unwrap());
        assert_eq!(chat.session, "ses_a");
        assert!(chat.unresolved, "held until it is found");
        assert_eq!(chat.pane_title.as_deref(), Some("OC | Alpha"), "the title is looked at again");

        // Now listed: the chat goes there.
        service.route("/api/session?", page(vec![session("ses_b", Some("Beta"), 3), session("ses_a", Some("Alpha"), 2)]));
        service.route("/api/session/ses_b", json!({"data": session("ses_b", Some("Beta"), 3)}));
        service.route("/api/session?parentID=ses", page(vec![]));
        assert!(follow_pane(&service.api, &mut chat, &out, &pane).unwrap());
        assert_eq!(chat.session, "ses_b");
        assert!(!chat.unresolved);
        assert_eq!(chat.pane_title.as_deref(), Some("OC | Beta"));
    }
}
