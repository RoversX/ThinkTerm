//! Claude Code: ThinkTerm's mod (a plugin's hooks module, `claude/` in
//! this crate), inside the Claude Code that runs in the pane, leaves a note
//! in the chat folder (one per Claude process) and looks there once a
//! second for a bridge listening for it. A mod cannot listen; it can only
//! ask, over HTTP, so the bridge listens on a Unix socket of its own and
//! the mod talks to it: it posts what happens in the session, long-polls
//! for what to do, and, while a permission prompt or a question waits,
//! for the answer. The mod tells the session's own shapes; they are turned
//! into this crate's here.
//!
//! The mod goes into Claude Code's skills folder, from where Claude Code
//! loads it as a plugin: a session started from then on has it, and one
//! running already takes it in on `/reload-plugins`.

use super::opencode::{brief, capped, file_edit, look, questions_of, shown};
use super::{head, read_ops, same_path, split_history, tail, AgentHere, Input, Out, PaneControl, Target};
use crate::{
    AgentState, Ask, AskKind, Attachment, Choice, CommandInfo, CommandKind, ContextInfo, Done, Ev, Item, LookKind, ModelChoice,
    Need, NoteKind, Op, Question, QuestionOption, Status, PROTOCOL,
};
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// The mod's files, as they are put into Claude Code: its manifest, its
/// hooks list and its hooks module.
/// How long a page of history asked for is waited on before it may be
/// asked for again.
const HISTORY_WAIT: Duration = Duration::from_secs(20);

const MOD_FILES: [(&str, &str); 3] = [
    (
        ".claude-plugin/plugin.json",
        include_str!("../../claude/thinkterm-chat/.claude-plugin/plugin.json"),
    ),
    ("hooks/hooks.json", include_str!("../../claude/thinkterm-chat/hooks/hooks.json")),
    ("hooks/register.ts", MODULE),
];
const MODULE: &str = include_str!("../../claude/thinkterm-chat/hooks/register.ts");
/// The mod's folder in Claude Code's skills folder.
const MOD_NAME: &str = "thinkterm-chat";
/// How every version of the hooks module begins: a folder whose module
/// does not is someone else's, and left alone.
const MARK: &str = "// ThinkTerm chat mod";
/// What Claude Code runs as: its own program, or Node or Bun.
const PROGRAMS: &[&str] = &["claude", "claude.exe", "node", "bun"];

/// How long a mod is waited for to link, once its bridge listens: it looks
/// once a second.
const HELLO_WAIT: Duration = Duration::from_secs(8);
/// How long a mod that linked may go without a word before it is taken
/// for gone: it asks for work at least every 20 seconds.
const QUIET: Duration = Duration::from_secs(60);
/// How long an op the mod asks for is held, waiting for one.
const OPS_WAIT: Duration = Duration::from_secs(20);
/// How long Claude Code started again in the pane is waited for.
const SWITCH_WAIT: Duration = Duration::from_secs(5);
/// How long a load typed into the pane is waited on before another is typed.
const LOADING: Duration = Duration::from_secs(10);
/// How often words still being written are sent as far as they go.
const WRITING_EVERY: Duration = Duration::from_millis(150);
/// The most a request from the mod may carry.
const MAX_BODY: usize = 48 << 20;
/// The most of its head a request may have.
const MAX_HEAD: usize = 64 << 10;
/// Rows of history read when a chat opens, and the most ever read.
const ROWS_FIRST: usize = 400;
const ROWS_MOST: usize = 4096;

const OUTPUT_KEPT: usize = 6 << 10;
const TEXT_KEPT: usize = 256 << 10;
const DETAIL_KEPT: usize = 300;
const ASK_KEPT: usize = 16 << 10;

/// How hard Claude can think, in `/effort`'s words.
const EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

/// Commands that open something only the terminal shows (a picker, a
/// panel, a dialog) or that act on the terminal itself: the phone does
/// not offer them, and says so when one is typed.
const TERMINAL_COMMANDS: [&str; 46] = [
    "add-dir",
    "agents",
    "background",
    "branch",
    "bug",
    "cd",
    "color",
    "config",
    "copy",
    "doctor",
    "exit",
    "export",
    "feedback",
    "focus",
    "fork",
    "heapdump",
    "help",
    "hooks",
    "ide",
    "keybindings",
    "list-agents",
    "login",
    "logout",
    "mcp",
    "memory",
    "mobile",
    "output-style",
    "passes",
    "permissions",
    "plugin",
    "powerup",
    "pro-trial-expired",
    "radio",
    "resume",
    "rewind",
    "sandbox",
    "scroll-speed",
    "setup-bedrock",
    "setup-vertex",
    "status",
    "statusline",
    "stickers",
    "tasks",
    "terminal-setup",
    "theme",
    "tui",
];

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}

/// Claude Code's own folder: `CLAUDE_CONFIG_DIR` as the Claude in the pane
/// has it, where that can be seen (Linux), else as the bridge has it.
fn claude_dir(target: &Target) -> PathBuf {
    let set = super::pane_env(target, PROGRAMS, "CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from));
    match set.filter(|dir| !dir.as_os_str().is_empty()) {
        Some(dir) => dir,
        None => home().join(".claude"),
    }
}

/// Where the mod goes: a plugin in Claude Code's skills folder.
pub fn mod_dir(target: &Target) -> PathBuf {
    claude_dir(target).join("skills").join(MOD_NAME)
}

/// Whether the mod is in place, of any version.
pub fn installed(target: &Target) -> bool {
    module_in(&mod_dir(target)).is_some_and(|text| text.starts_with(MARK))
}

fn module_in(dir: &Path) -> Option<String> {
    std::fs::read_to_string(dir.join("hooks").join("register.ts")).ok()
}

/// Puts the mod into Claude Code, or brings an older one of ours up to
/// date; one as new or newer is left as it is. A Claude started from now
/// on has it; a running one takes it in on `/reload-plugins`.
pub fn install(target: &Target) -> Result<PathBuf> {
    install_in(&claude_dir(target))
}

fn install_in(claude: &Path) -> Result<PathBuf> {
    if !claude.is_dir() {
        bail!("{} is not there: is Claude Code installed?", claude.display());
    }
    let dir = claude.join("skills").join(MOD_NAME);
    match module_in(&dir) {
        Some(text) if !text.starts_with(MARK) => bail!("{} is someone else's", dir.display()),
        // A newer one of ours is kept; one as new but not as written (a copy
        // changed by hand, a build that did not raise the version) is put
        // right.
        Some(text) if version_of(&text) > version_of(MODULE) => return Ok(dir),
        Some(_) => {}
        None if dir.read_dir().is_ok_and(|mut entries| entries.next().is_some()) => {
            bail!("{} is someone else's", dir.display())
        }
        None => {}
    }
    for (name, text) in MOD_FILES {
        let file = dir.join(name);
        if std::fs::read_to_string(&file).is_ok_and(|now| now == text) {
            continue;
        }
        let parent = file.parent().context("a file with no folder")?;
        std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        let staged = parent.join(format!(".{}.{}", name.rsplit('/').next().unwrap_or(name), std::process::id()));
        std::fs::write(&staged, text).with_context(|| format!("writing {}", staged.display()))?;
        std::fs::rename(&staged, &file).with_context(|| format!("putting {} in place", file.display()))?;
    }
    Ok(dir)
}

/// The version a hooks module's text says it is (`MOD_VERSION`); 0 for
/// none.
fn version_of(text: &str) -> u32 {
    text.lines()
        .take(40)
        .find_map(|line| {
            let rest = line.trim().strip_prefix("const MOD_VERSION = ")?;
            rest.trim_end_matches(';').trim().parse().ok()
        })
        .unwrap_or(0)
}

/// The version of ours in Claude Code's folder, which a reload would take in.
fn version_in_folder(target: &Target) -> u32 {
    module_in(&mod_dir(target))
        .filter(|text| text.starts_with(MARK))
        .map_or(0, |text| version_of(&text))
}

/// The note a mod leaves.
#[derive(Clone, Debug, Deserialize)]
struct Note {
    pid: u32,
    pane: u64,
    #[serde(default)]
    mux_socket: Option<String>,
    /// Claude Code's version.
    #[serde(default)]
    claude: String,
}

/// The note of the Claude running in the pane, if one is alive there: the
/// newest, should several be.
fn find(dir: &Path, target: &Target) -> Option<Note> {
    let mut found: Option<(std::time::SystemTime, Note)> = None;
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !(name.starts_with("claude-") && name.ends_with(".json")) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        let Ok(note) = serde_json::from_str::<Note>(&text) else {
            continue;
        };
        if note.pane != target.pane || !alive(note.pid) || !in_pane(note.pid, note.pane) {
            continue;
        }
        // Pane ids are a mux's own: with both sockets known, they must be one.
        if let (Some(ours), Some(theirs)) = (&target.mux_socket, &note.mux_socket) {
            if !same_path(ours, theirs) {
                continue;
            }
        }
        let modified = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(std::time::UNIX_EPOCH);
        if found.as_ref().is_none_or(|(newest, _)| modified > *newest) {
            found = Some((modified, note));
        }
    }
    found.map(|(_, note)| note)
}

/// Whether process `pid` runs in pane `pane`, as its environment says
/// where that can be read (Linux): a note left by a Claude long gone whose
/// pid another process took is not followed.
fn in_pane(pid: u32, pane: u64) -> bool {
    match std::fs::read(format!("/proc/{pid}/environ")) {
        Ok(environ) => {
            let want = format!("WEZTERM_PANE={pane}");
            environ.split(|&b| b == 0).any(|var| var == want.as_bytes())
        }
        Err(_) => true,
    }
}

fn alive(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    // Signal 0 checks without sending; EPERM is a process that is there.
    let sent = unsafe { libc::kill(pid, 0) };
    sent == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

pub fn run(target: &Target, input: impl BufRead + Send + 'static, out: Out, pane: Option<Box<dyn PaneControl>>) -> Result<()> {
    // Put in where it is not, brought up to date where it is older: a
    // Claude started from now on has it.
    let _ = install(target);
    let pane = pane.as_deref();
    let (tx, rx) = mpsc::channel::<Input<Piece>>();
    // Where Claude works, once its mod says: what the paths it is shown
    // are made relative to.
    let place = super::Place::new(target.cwd.clone());
    read_ops(input, tx.clone(), out.clone(), place.clone());
    let dir = super::pi::chat_dir();
    let mut note = match find(&dir, target) {
        Some(note) => note,
        None => match offer(target, pane, &rx, &out)? {
            Some(note) => note,
            None => return Ok(()),
        },
    };
    loop {
        match relay(target, &place, &dir, &note, &tx, &rx, &out, pane)? {
            Ended::Client => return Ok(()),
            Ended::Agent(why) => {
                // Started again in the pane (`claude --resume`): the chat
                // goes on with it.
                let until = Instant::now() + SWITCH_WAIT;
                let again = loop {
                    if let Some(found) = find(&dir, target).filter(|found| found.pid != note.pid) {
                        break Some(found);
                    }
                    if Instant::now() >= until {
                        break None;
                    }
                    std::thread::sleep(Duration::from_millis(250));
                };
                match again {
                    Some(found) => note = found,
                    None => {
                        out.error(None, why)?;
                        return Ok(());
                    }
                }
            }
        }
    }
}

/// Has Claude in the pane take its plugins in anew: `/reload-plugins`,
/// typed as a person would -- only while it rests, and only into an empty
/// field, as the command would be added to what is typed there.
fn load(pane: &dyn PaneControl) -> std::result::Result<(), String> {
    let command = crate::reload_command("claude");
    match pane.agent() {
        AgentHere::Gone => return Err("Claude is not running in this pane any more".into()),
        AgentHere::Working => return Err("Claude is working: load it once it rests".into()),
        AgentHere::Resting | AgentHere::Unknown => {}
    }
    match pane.screen().as_deref().and_then(field_empty) {
        Some(true) => pane.type_line(command).map_err(|err| format!("{err:#}")),
        Some(false) => Err("something is typed in Claude's field: send or clear it, then load".into()),
        None => Err(format!("type {command} in Claude")),
    }
}

/// Whether Claude's field is empty, as its screen shows it: the rows
/// between the last two rules (it draws its field between two lines of
/// `─`), but for its `❯` sign. None where the screen shows no such field.
fn field_empty(rows: &[String]) -> Option<bool> {
    let rule = |row: &String| {
        let row = row.trim();
        row.chars().count() >= 8 && row.chars().all(|c| c == '─')
    };
    let rules: Vec<usize> = rows.iter().enumerate().filter(|(_, row)| rule(row)).map(|(at, _)| at).collect();
    let [.., above, below] = rules[..] else {
        return None;
    };
    if below <= above + 1 {
        return None;
    }
    let field = &rows[above + 1..below];
    if !field.first()?.trim_start().starts_with(['❯', '>']) {
        return None;
    }
    Some(field.iter().all(|row| row.trim().trim_start_matches(['❯', '>']).trim().is_empty()))
}

/// How often a pane whose Claude runs no mod is looked at again.
const LOOK_AGAIN: Duration = Duration::from_millis(500);

/// No mod answers in this pane: say what would help, put it in when the
/// client asks, and go on to the chat once Claude runs it (on its
/// `/reload-plugins`, or started again).
fn offer(target: &Target, pane: Option<&dyn PaneControl>, rx: &mpsc::Receiver<Input<Piece>>, out: &Out) -> Result<Option<Note>> {
    let message = "no ThinkTerm plugin is running in this Claude Code (it needs Claude Code 2.1.300 or newer, with plugins allowed)";
    let say = |out: &Out| -> std::io::Result<()> {
        let need = if installed(target) { Need::PiRestart } else { Need::PiExtension };
        out.send(&Ev::Error {
            need: Some(need),
            message: message.into(),
            can_load: need == Need::PiRestart && pane.is_some(),
        })
    };
    say(out)?;
    let mut loading: Option<Instant> = None;
    loop {
        match rx.recv_timeout(LOOK_AGAIN) {
            Ok(Input::Op(Op::Install)) => match install(target) {
                Ok(_) if pane.is_some() => say(out)?,
                Ok(_) => {
                    out.send(&Ev::Done { what: Done::Installed })?;
                    say(out)?;
                }
                Err(err) => out.refused(format!("{err:#}"))?,
            },
            Ok(Input::Op(Op::Reload)) if loading.is_some_and(|at| at.elapsed() < LOADING) => {}
            Ok(Input::Op(Op::Reload)) => {
                let loaded = match pane {
                    Some(pane) => load(pane),
                    None => Err(format!("type {} in Claude", crate::reload_command("claude"))),
                };
                match loaded {
                    Ok(()) => loading = Some(Instant::now()),
                    Err(why) => out.refused(why)?,
                }
            }
            Ok(Input::Op(_)) => out.refused("Claude's chat is not running")?,
            Ok(Input::ClientGone | Input::AgentGone(_)) | Err(RecvTimeoutError::Disconnected) => return Ok(None),
            Ok(Input::Agent(_)) | Err(RecvTimeoutError::Timeout) => {}
        }
        if let Some(note) = find(&super::pi::chat_dir(), target) {
            return Ok(Some(note));
        }
    }
}

/// What comes from the mod: the events it posted, by the copy of the
/// module that posted them (a reload of the mod is another copy).
enum Piece {
    Events(String, Vec<Value>),
}

enum Ended {
    Client,
    Agent(String),
}

/// One op waiting for the mod, and the copy of the module it went to.
struct Slot {
    seq: u64,
    op: Value,
    to: Option<String>,
}

/// What the bridge and the mod's requests share.
#[derive(Default)]
struct Shared {
    ops: Vec<Slot>,
    seq: u64,
    /// The copies of the module that asked, in the order they first did: the
    /// last is the one loaded; the others a reload replaced.
    copies: Vec<String>,
    /// Answers the phone gave, by ask, until the mod takes them.
    answers: HashMap<String, Value>,
    /// Asks answered or dropped elsewhere: a mod holding one lets go.
    gone: HashSet<String>,
    /// When the mod was last heard from.
    seen: Option<Instant>,
    closed: bool,
}

type Link = Arc<(Mutex<Shared>, Condvar)>;

fn lock(link: &Link) -> std::sync::MutexGuard<'_, Shared> {
    link.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Hands the mod an op, in order: it takes it on its next ask.
fn enqueue(link: &Link, op: Value) {
    let mut shared = lock(link);
    shared.seq += 1;
    let seq = shared.seq;
    shared.ops.push(Slot { seq, op, to: None });
    link.1.notify_all();
}

/// The ops for `copy`, once there are any or `wait` is over; none (`None`)
/// for a copy a newer one replaced, which asks no more. What it
/// acknowledged (`ack`) is done with; what went to an older copy, which a
/// reload ended before it said it took it, goes again.
fn take_ops(link: &Link, copy: &str, ack: u64, wait: Duration) -> Option<Vec<Value>> {
    let mut shared = lock(link);
    shared.seen = Some(Instant::now());
    if !shared.copies.iter().any(|known| known == copy) {
        shared.copies.push(copy.to_string());
    }
    if shared.copies.last().map(String::as_str) != Some(copy) {
        return None;
    }
    shared.ops.retain(|slot| !(slot.seq <= ack && slot.to.as_deref() == Some(copy)));
    // What went out and was not acknowledged never came (an answer lost,
    // or another copy's): it goes again.
    for slot in shared.ops.iter_mut() {
        slot.to = None;
    }
    // A newer copy, waiting meanwhile, is woken to look again.
    link.1.notify_all();
    let until = Instant::now() + wait;
    loop {
        if shared.copies.last().map(String::as_str) != Some(copy) {
            return None;
        }
        if shared.closed {
            return Some(Vec::new());
        }
        let fresh: Vec<Value> = shared
            .ops
            .iter_mut()
            .filter(|slot| slot.to.is_none())
            .map(|slot| {
                slot.to = Some(copy.to_string());
                let mut op = slot.op.clone();
                op["seq"] = json!(slot.seq);
                op
            })
            .collect();
        if !fresh.is_empty() {
            return Some(fresh);
        }
        let now = Instant::now();
        if now >= until {
            return Some(Vec::new());
        }
        shared = link.1.wait_timeout(shared, until - now).unwrap_or_else(|p| p.into_inner()).0;
    }
}

/// The phone's answer to `key`, once it gives one, the ask is gone, or
/// `wait` is over.
fn take_answer(link: &Link, key: &str, wait: Duration) -> Value {
    let mut shared = lock(link);
    shared.seen = Some(Instant::now());
    let until = Instant::now() + wait;
    loop {
        if let Some(answer) = shared.answers.remove(key) {
            return json!({ "answer": answer });
        }
        if shared.gone.contains(key) || shared.closed {
            return json!({ "gone": true });
        }
        let now = Instant::now();
        if now >= until {
            return json!({});
        }
        shared = link.1.wait_timeout(shared, until - now).unwrap_or_else(|p| p.into_inner()).0;
    }
}

/// A request from the mod: one to a connection.
#[derive(Debug, Default)]
struct Request {
    method: String,
    path: String,
    query: HashMap<String, String>,
    body: Vec<u8>,
}

fn read_request(reader: &mut impl BufRead) -> std::io::Result<Request> {
    let bad = |what: &str| std::io::Error::new(std::io::ErrorKind::InvalidData, what.to_string());
    let mut head = 0usize;
    let mut line = String::new();
    reader.read_line(&mut line)?;
    head += line.len();
    let mut words = line.split_whitespace();
    let method = words.next().ok_or_else(|| bad("no method"))?.to_string();
    let target = words.next().ok_or_else(|| bad("no path"))?.to_string();
    let mut length = 0usize;
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            return Err(bad("cut short"));
        }
        head += line.len();
        if head > MAX_HEAD {
            return Err(bad("head too long"));
        }
        let header = line.trim_end();
        if header.is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':') {
            if name.trim().eq_ignore_ascii_case("content-length") {
                length = value.trim().parse().map_err(|_| bad("bad length"))?;
            }
        }
    }
    if length > MAX_BODY {
        std::io::copy(&mut reader.take(length as u64), &mut std::io::sink())?;
        return Err(std::io::Error::new(std::io::ErrorKind::FileTooLarge, "body too long"));
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body)?;
    let (path, query) = match target.split_once('?') {
        Some((path, query)) => (path.to_string(), query),
        None => (target.clone(), ""),
    };
    let query = query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .map(|(k, v)| (k.to_string(), unescape(v)))
        .collect();
    Ok(Request {
        method,
        path,
        query,
        body,
    })
}

/// A query value with its `%XX` escapes undone.
fn unescape(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        let hex = |b: u8| (b as char).to_digit(16);
        match bytes[at] {
            b'%' if at + 2 < bytes.len() => match (hex(bytes[at + 1]), hex(bytes[at + 2])) {
                (Some(hi), Some(lo)) => {
                    out.push((hi * 16 + lo) as u8);
                    at += 3;
                    continue;
                }
                _ => out.push(b'%'),
            },
            b'+' => out.push(b' '),
            b => out.push(b),
        }
        at += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn respond(stream: &mut UnixStream, status: u16, body: &Value) -> std::io::Result<()> {
    let body = body.to_string();
    let reason = match status {
        200 => "OK",
        413 => "Payload Too Large",
        _ => "Not Found",
    };
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )?;
    stream.flush()
}

/// Answers one request of the mod's.
fn answer(mut stream: UnixStream, link: &Link, tx: &mpsc::Sender<Input<Piece>>) -> std::io::Result<()> {
    let request = match read_request(&mut BufReader::new(stream.try_clone()?)) {
        Ok(request) => request,
        Err(err) if err.kind() == std::io::ErrorKind::FileTooLarge => {
            return respond(&mut stream, 413, &json!({ "too_large": true }));
        }
        Err(err) => return Err(err),
    };
    let copy = request.query.get("copy").cloned().unwrap_or_default();
    match (request.method.as_str(), request.path.as_str()) {
        ("POST", "/ev") => {
            let posted: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
            let copy = posted["copy"].as_str().unwrap_or_default().to_string();
            let events = posted["events"].as_array().cloned().unwrap_or_default();
            lock(link).seen = Some(Instant::now());
            let _ = tx.send(Input::Agent(Piece::Events(copy, events)));
            respond(&mut stream, 200, &json!({}))
        }
        ("GET", "/ops") => {
            let ack = request.query.get("ack").and_then(|n| n.parse().ok()).unwrap_or(0);
            match take_ops(link, &copy, ack, OPS_WAIT) {
                Some(ops) => respond(&mut stream, 200, &json!({ "ops": ops })),
                None => respond(&mut stream, 200, &json!({ "gone": true })),
            }
        }
        ("GET", "/answer") => {
            let key = request.query.get("key").cloned().unwrap_or_default();
            let wait = request.query.get("wait").and_then(|n| n.parse().ok()).unwrap_or(20u64).min(30);
            let answer = take_answer(link, &key, Duration::from_secs(wait));
            respond(&mut stream, 200, &answer)
        }
        _ => respond(&mut stream, 404, &json!({ "unknown": request.path })),
    }
}

/// The bridge's socket, gone with it.
struct Listening {
    path: PathBuf,
    link: Link,
}

impl Drop for Listening {
    fn drop(&mut self) {
        lock(&self.link).closed = true;
        self.link.1.notify_all();
        // The listening thread waits for a connection: one wakes it, to
        // find it is closed and end, letting its socket go.
        let _ = UnixStream::connect(&self.path);
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Listens for the mod of the Claude whose note this is: a socket of its
/// own in the chat folder, which the mod finds by its name.
fn listen(dir: &Path, note: &Note, tx: &mpsc::Sender<Input<Piece>>) -> Result<Listening> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    forget_stale(dir);
    let path = dir.join(format!("claude-{}-{}.sock", note.pid, std::process::id()));
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path).with_context(|| format!("listening on {}", path.display()))?;
    let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    let link: Link = Arc::new((Mutex::new(Shared::default()), Condvar::new()));
    let (served, tx) = (link.clone(), tx.clone());
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            if lock(&served).closed {
                return;
            }
            let Ok(stream) = stream else {
                continue;
            };
            let (link, tx) = (served.clone(), tx.clone());
            std::thread::spawn(move || {
                if let Err(err) = answer(stream, &link, &tx) {
                    log::debug!("claude chat: a request of the mod's: {err}");
                }
            });
        }
    });
    Ok(Listening { path, link })
}

/// Sockets left by bridges that are gone (killed), taken away so no mod
/// keeps trying them; and the notes of Claude processes that are gone.
fn forget_stale(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if let Some(pid) = name.strip_prefix("claude-").and_then(|n| n.strip_suffix(".json")) {
            if pid.parse::<u32>().is_ok_and(|pid| !alive(pid)) {
                let _ = std::fs::remove_file(entry.path());
            }
            continue;
        }
        let Some(rest) = name.strip_prefix("claude-").and_then(|n| n.strip_suffix(".sock")) else {
            continue;
        };
        let Some(bridge) = rest.rsplit('-').next().and_then(|pid| pid.parse::<u32>().ok()) else {
            continue;
        };
        if bridge != std::process::id() && !alive(bridge) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn relay(
    target: &Target,
    place: &super::Place,
    dir: &Path,
    note: &Note,
    tx: &mpsc::Sender<Input<Piece>>,
    rx: &mpsc::Receiver<Input<Piece>>,
    out: &Out,
    pane: Option<&dyn PaneControl>,
) -> Result<Ended> {
    let listening = match listen(dir, note, tx) {
        Ok(listening) => listening,
        Err(err) => {
            out.error(None, format!("{err:#}"))?;
            return Ok(Ended::Client);
        }
    };
    let mut chat = Chat::new(out.clone(), listening.link.clone(), target.cwd.as_ref().map(|p| p.display().to_string()));
    chat.place = place.clone();
    let started = Instant::now();
    let mut told_silent = false;
    let mut loading: Option<Instant> = None;
    loop {
        let wait = if chat.writing.is_empty() { Duration::from_secs(1) } else { WRITING_EVERY };
        match rx.recv_timeout(wait) {
            Ok(Input::Op(Op::Install)) => match install(target) {
                Ok(_) => out.send(&Ev::Done { what: Done::Installed })?,
                Err(err) => out.refused(format!("{err:#}"))?,
            },
            // Nothing newer to take in, or a load typed a moment ago.
            Ok(Input::Op(Op::Reload)) if chat.hello && chat.version >= version_in_folder(target) => {}
            Ok(Input::Op(Op::Reload)) if loading.is_some_and(|at| at.elapsed() < LOADING) => {}
            Ok(Input::Op(Op::Reload)) => {
                let loaded = match pane {
                    Some(pane) => load(pane),
                    None => Err(format!("type {} in Claude", crate::reload_command("claude"))),
                };
                match loaded {
                    Ok(()) => loading = Some(Instant::now()),
                    Err(why) => out.refused(why)?,
                }
            }
            Ok(Input::Op(op)) => chat.op(op)?,
            Ok(Input::Agent(Piece::Events(copy, events))) => {
                for event in &events {
                    chat.event(&copy, event, target)?;
                }
            }
            Ok(Input::ClientGone) | Err(RecvTimeoutError::Disconnected) => return Ok(Ended::Client),
            Ok(Input::AgentGone(why)) => return Ok(Ended::Agent(why)),
            Err(RecvTimeoutError::Timeout) => {}
        }
        chat.write_on()?;
        if !alive(note.pid) {
            return Ok(Ended::Agent("Claude Code quit".into()));
        }
        let seen = lock(&listening.link).seen;
        match seen {
            // The mod reloads (a new copy links in a second or two) or was
            // taken out: after a while without a word, it is gone.
            Some(at) if at.elapsed() > QUIET => {
                out.error(Some(Need::PiRestart), "the ThinkTerm plugin in Claude Code stopped answering")?;
                return Ok(Ended::Client);
            }
            None if !told_silent && started.elapsed() > HELLO_WAIT => {
                told_silent = true;
                out.send(&Ev::Error {
                    need: Some(Need::PiRestart),
                    message: format!(
                        "the ThinkTerm plugin in Claude Code {} does not answer",
                        if note.claude.is_empty() { "?" } else { &note.claude }
                    ),
                    can_load: pane.is_some(),
                })?;
            }
            _ => {}
        }
    }
}

/// Words being written, as far as they came.
struct Writing {
    id: String,
    thinking: bool,
    text: String,
    /// When they were last sent as far as they went.
    sent: Option<Instant>,
}

/// A permission prompt or a question, as shown, and what the mod needs
/// to answer it.
struct Waiting {
    ask: Ask,
    /// The questions as asked, for the answers to name them.
    questions: Vec<String>,
    /// The mod holds it, so the phone's answer counts.
    held: bool,
}

struct Chat {
    out: Out,
    link: Link,
    cwd: Option<String>,
    /// The mod said hello.
    hello: bool,
    /// The copy of the module that said hello last.
    copy: String,
    /// The running mod's version.
    version: u32,
    /// Since when the turn has been running.
    since: Option<u64>,
    state: Option<AgentState>,
    asks: Vec<Waiting>,
    /// Words being written, by `(turn, step, block)`.
    writing: HashMap<(String, u64, u64), Writing>,
    /// The blocks of the step being answered, in the order they came:
    /// what the rows kept for them are matched to, so a row's words
    /// replace the ones written as they came.
    blocks: VecDeque<(u64, &'static str)>,
    step: Option<(String, u64)>,
    /// Calls of this chat's time, by id: their tool and what they were asked.
    tools: HashMap<String, (String, Value)>,
    /// What the tool kept of a call, and its error, as the mod told it.
    results: HashMap<String, (Value, Option<String>)>,
    /// A call's result as the model reads it, and whether it is an error.
    ended: HashMap<String, (String, bool)>,
    /// The pictures a call gave back (a screenshot a tool took), kept as
    /// files, by call id.
    shots: HashMap<String, Vec<Attachment>>,
    /// Where Claude works, for the files a client asks to see.
    place: super::Place,
    last_item: Option<String>,
    /// History older than what was sent, for the next ask.
    held: Vec<Item>,
    /// Rows the last history read asked for.
    asked: usize,
    /// The session may hold more rows than were read.
    more: bool,
    /// The oldest item sent.
    oldest: Option<String>,
    /// An older page is being read.
    paging: bool,
    /// History asked for, not come yet: what happens meanwhile waits.
    reading: Option<Instant>,
    compactions: u64,
    /// The Claude Code model row's choices, and the model in use.
    models: Option<(Vec<String>, String, String)>,
    effort: Option<String>,
    /// What every turn used, together.
    totals: crate::Usage,
    /// Things said here not taken by Claude yet, shown under these ids.
    waiting_words: VecDeque<(String, String)>,
    sent: u64,
}

impl Chat {
    fn new(out: Out, link: Link, cwd: Option<String>) -> Self {
        Self {
            out,
            link,
            cwd,
            hello: false,
            copy: String::new(),
            version: 0,
            since: None,
            state: None,
            asks: Vec::new(),
            writing: HashMap::new(),
            blocks: VecDeque::new(),
            step: None,
            tools: HashMap::new(),
            results: HashMap::new(),
            ended: HashMap::new(),
            shots: HashMap::new(),
            place: super::Place::default(),
            last_item: None,
            held: Vec::new(),
            asked: 0,
            more: false,
            oldest: None,
            paging: false,
            reading: None,
            compactions: 0,
            models: None,
            effort: None,
            totals: crate::Usage::default(),
            waiting_words: VecDeque::new(),
            sent: 0,
        }
    }

    fn cwd(&self) -> Option<&str> {
        self.cwd.as_deref()
    }

    /// An item new or changed, which a turn's time is then said after.
    fn send_item(&mut self, item: Item) -> Result<()> {
        self.last_item = Some(item.id().to_string());
        self.send_line(&Ev::item(item))
    }

    /// A line, with small pictures for the pictures in it.
    fn send_line(&self, ev: &Ev) -> Result<()> {
        let line = serde_json::to_string(ev)?;
        self.out.raw(&super::thumb::fill_line(&line))?;
        Ok(())
    }

    fn state_now(&self) -> AgentState {
        if self.asks.iter().any(|waiting| waiting.held) || !self.asks.is_empty() {
            AgentState::Blocked
        } else if self.since.is_some() {
            AgentState::Working
        } else {
            AgentState::Idle
        }
    }

    fn tell_state(&mut self) -> Result<()> {
        let now = self.state_now();
        if self.state != Some(now) {
            self.state = Some(now);
            self.out.send(&Ev::State {
                state: now,
                since: if now == AgentState::Idle { None } else { self.since.or(Some(super::now())) },
            })?;
        }
        Ok(())
    }

    fn read_history(&mut self, last: usize, paging: bool) {
        self.asked = last;
        self.paging = paging;
        self.reading = Some(Instant::now());
        enqueue(&self.link, json!({ "op": "history", "last": last }));
    }

    /// The client's ask.
    fn op(&mut self, op: Op) -> Result<()> {
        if !self.hello {
            self.out.refused("Claude's chat is starting")?;
            return Ok(());
        }
        match op {
            // These two open a picker in the terminal when given nothing:
            // the model menu is the phone's own.
            Op::Send { text, attachments } if attachments.is_empty() && matches!(text.trim(), "/model" | "/effort") => {
                self.out.refused("choose the model and how hard it thinks in the model menu")?;
            }
            Op::Send { text, attachments } => {
                let (pictures, files): (Vec<_>, Vec<_>) = attachments.into_iter().filter(|a| a.path.is_some()).partition(|a| a.image);
                let paths = |list: Vec<Attachment>| list.into_iter().filter_map(|a| a.path).collect::<Vec<_>>();
                let (pictures, files) = (paths(pictures), paths(files));
                // As the mod puts it to Claude: the paths after the words.
                let words: Vec<&str> = std::iter::once(text.as_str())
                    .chain(pictures.iter().map(String::as_str))
                    .chain(files.iter().map(String::as_str))
                    .filter(|part| !part.is_empty())
                    .collect();
                let words = words.join("\n");
                let typed_command = text.trim_start().starts_with('/') && pictures.is_empty() && files.is_empty();
                // Shown at once: Claude takes it when it rests, which may be
                // a while into its turn. The row it is kept as, once taken,
                // goes by the same id.
                if !typed_command && !words.trim().is_empty() {
                    self.sent += 1;
                    let id = format!("sent-{}-{}", super::now(), self.sent);
                    if let Some(Said::Item(Item::User { text, attachments, .. })) = said(&id, &words, 0, self.cwd()) {
                        self.send_item(Item::User {
                            id: id.clone(),
                            text,
                            attachments,
                        })?;
                        self.waiting_words.push_back((id, words.clone()));
                    }
                }
                enqueue(&self.link, json!({ "op": "send", "text": text, "pictures": pictures, "files": files }));
            }
            Op::Interrupt => enqueue(&self.link, json!({ "op": "interrupt" })),
            Op::SetModel { model, effort } => {
                let current = self.models.as_ref().map(|(_, value, _)| value.clone());
                let model = (Some(&model) != current.as_ref() && !model.is_empty()).then_some(model);
                enqueue(&self.link, json!({ "op": "model", "model": model, "effort": effort }));
            }
            Op::Answer { ask, choice, answers } => self.answer(&ask, choice, answers)?,
            Op::Older => self.older()?,
            // Open is answered where ops are read (`read_ops`).
            Op::Install | Op::Reload | Op::Open { .. } => {}
        }
        Ok(())
    }

    fn answer(&mut self, key: &str, choice: Choice, answers: Vec<String>) -> Result<()> {
        let Some(at) = self.asks.iter().position(|waiting| waiting.ask.id == key) else {
            self.out.refused("that was answered already")?;
            return Ok(());
        };
        if !self.asks[at].held {
            self.out.refused("answer it in the terminal")?;
            return Ok(());
        }
        let waiting = &self.asks[at];
        if !waiting.ask.choices.contains(&choice) {
            self.out.refused("that answer is not offered")?;
            return Ok(());
        }
        let said = match choice {
            Choice::Decline => json!({ "behavior": "deny", "message": "The person declined in the ThinkTerm app." }),
            Choice::AcceptForSession => json!({ "behavior": "allow", "session": true }),
            Choice::Accept if waiting.ask.kind == AskKind::Question => {
                if answers.len() != waiting.questions.len() || answers.iter().any(|a| a.trim().is_empty()) {
                    self.out.refused("answer every question")?;
                    return Ok(());
                }
                let named: serde_json::Map<String, Value> =
                    waiting.questions.iter().cloned().zip(answers.into_iter().map(Value::String)).collect();
                json!({ "behavior": "allow", "answers": named })
            }
            Choice::Accept => json!({ "behavior": "allow" }),
        };
        let mut shared = lock(&self.link);
        shared.answers.insert(key.to_string(), said);
        self.link.1.notify_all();
        Ok(())
    }

    fn older(&mut self) -> Result<()> {
        if !self.held.is_empty() {
            let (held, page) = split_history(std::mem::take(&mut self.held));
            self.held = held;
            return self.send_page(page, false);
        }
        // A page asked for and never come (too large to cross) is asked
        // for again, not waited on for good.
        let reading = self.reading.is_some_and(|at| at.elapsed() < HISTORY_WAIT);
        if self.more && !reading && self.asked < ROWS_MOST {
            self.read_history((self.asked * 4).min(ROWS_MOST), true);
            return Ok(());
        }
        self.out.send(&Ev::History {
            items: Vec::new(),
            more: false,
            reset: false,
        })?;
        Ok(())
    }

    fn send_page(&mut self, page: Vec<Item>, reset: bool) -> Result<()> {
        if let Some(first) = page.first() {
            self.oldest = Some(first.id().to_string());
        }
        let more = !self.held.is_empty() || (self.more && self.asked < ROWS_MOST);
        self.send_line(&Ev::History {
            items: page,
            more,
            reset,
        })
    }

    /// What the mod tells.
    fn event(&mut self, copy: &str, ev: &Value, target: &Target) -> Result<()> {
        let s = |key: &str| ev[key].as_str().unwrap_or_default();
        match s("t") {
            "hello" => {
                // The mod's first word, again after each reload of it.
                let again = self.hello;
                self.hello = true;
                self.copy = copy.to_string();
                self.version = ev["mod"].as_u64().unwrap_or(0) as u32;
                if let Some(cwd) = ev["cwd"].as_str().filter(|cwd| !cwd.is_empty()) {
                    self.cwd = Some(cwd.to_string());
                    self.place.set(cwd);
                }
                self.since = ev["working"].as_bool().unwrap_or(false).then(super::now);
                self.asks.clear();
                self.writing.clear();
                self.tools.clear();
                self.ended.clear();
                self.results.clear();
                self.shots.clear();
                self.state = None;
                self.out.send(&Ev::Hello {
                    protocol: PROTOCOL,
                    agent: "claude".into(),
                    title: None,
                    guessed: false,
                })?;
                if self.version < version_in_folder(target) {
                    self.out.send(&Ev::Outdated)?;
                }
                let _ = again;
                if let Some(mode) = ev["mode"].as_str().filter(|m| !m.is_empty()) {
                    self.out.send(&Ev::Mode { mode: mode.to_string() })?;
                }
                self.read_history(ROWS_FIRST, false);
                self.tell_state()?;
            }
            "rows" => self.rows(ev)?,
            "reset" => {
                self.held.clear();
                self.asks.clear();
                self.waiting_words.clear();
                self.tools.clear();
                self.ended.clear();
                self.results.clear();
                self.shots.clear();
                self.writing.clear();
                self.since = None;
                self.out.send(&Ev::History {
                    items: Vec::new(),
                    more: false,
                    reset: true,
                })?;
                self.read_history(ROWS_FIRST, false);
                self.tell_state()?;
            }
            "row" => self.row(ev)?,
            "chunk" => self.chunk(ev)?,
            "step" => {
                let key_turn = s("turn").to_string();
                let step = ev["step"].as_u64().unwrap_or(0);
                for (key, writing) in self.writing.iter_mut() {
                    if key.0 == key_turn && key.1 == step {
                        writing.sent = None;
                    }
                }
                self.write_on()?;
            }
            "turn" => self.turn(ev)?,
            "done" => self.done(ev)?,
            "ask" => self.ask(ev)?,
            "resolved" => {
                let key = s("key");
                if let Some(at) = self.asks.iter().position(|waiting| waiting.ask.id == key) {
                    self.asks.remove(at);
                    self.out.send(&Ev::Resolved { id: key.to_string() })?;
                }
                // A hook of the mod's still holding it lets go now.
                let mut shared = lock(&self.link);
                shared.answers.remove(key);
                shared.gone.insert(key.to_string());
                self.link.1.notify_all();
                drop(shared);
                self.tell_state()?;
            }
            "commands" => {
                let commands = ev["commands"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|c| {
                        let name = c["name"].as_str()?;
                        (!TERMINAL_COMMANDS.contains(&name) && !name.starts_with("__")).then(|| CommandInfo {
                            name: format!("/{name}"),
                            description: head(c["description"].as_str().unwrap_or_default(), DETAIL_KEPT),
                            args: None,
                            kind: Some(CommandKind::Command),
                        })
                    })
                    .collect();
                self.out.send(&Ev::Commands { commands })?;
            }
            "models" => self.models(ev)?,
            "usage" => {
                let context = ContextInfo {
                    used: ev["tokens"].as_u64(),
                    window: ev["window"].as_u64(),
                    totals: self.totals.clone(),
                    ..Default::default()
                };
                self.out.send(&Ev::Context { context })?;
            }
            "compacted" => {
                self.compactions += 1;
                let id = format!("compacted-{}-{}", super::now(), self.compactions);
                self.send_item(Item::Note {
                    id,
                    note: NoteKind::Compacted,
                    text: String::new(),
                })?;
            }
            "refused" => {
                // Something said here that Claude did not take: not shown
                // again as waiting.
                if ev["dropped"].as_bool() == Some(true) {
                    self.waiting_words.pop_front();
                }
                self.out.refused(s("message"))?
            }
            "mode" if !s("mode").is_empty() => self.out.send(&Ev::Mode { mode: s("mode").to_string() })?,
            // What this side does not know yet.
            _ => {}
        }
        Ok(())
    }

    fn rows(&mut self, ev: &Value) -> Result<()> {
        self.reading = None;
        let rows = ev["rows"].as_array().cloned().unwrap_or_default();
        let asked = ev["asked"].as_u64().unwrap_or(self.asked as u64) as usize;
        self.asked = asked;
        self.more = rows.len() >= asked && asked < ROWS_MOST;
        let items = history_items(&rows, self.cwd());
        if self.paging {
            self.paging = false;
            // What is older than what the client holds.
            let older = match self.oldest.as_deref().and_then(|oldest| items.iter().position(|i| i.id() == oldest)) {
                Some(at) => items.into_iter().take(at).collect(),
                None => Vec::new(),
            };
            if older.is_empty() {
                self.more = false;
            }
            let (held, page) = split_history(older);
            self.held = held;
            return self.send_page(page, false);
        }
        let (held, page) = split_history(items);
        self.held = held;
        self.last_item = page.last().map(|item| item.id().to_string());
        self.send_page(page, true)?;
        // History from the start drops what a client shows as asked: what
        // still waits is said again after it.
        for waiting in &self.asks {
            self.out.send(&Ev::Ask { ask: waiting.ask.clone() })?;
        }
        // And what was said here that Claude has not taken yet.
        let waiting: Vec<(String, String)> = self.waiting_words.iter().cloned().collect();
        for (id, words) in waiting {
            if let Some(Said::Item(item)) = said(&id, &words, 0, self.cwd()) {
                self.send_item(item)?;
            }
        }
        Ok(())
    }

    /// A row the session kept, as it was kept.
    fn row(&mut self, ev: &Value) -> Result<()> {
        let uuid = ev["uuid"].as_str().unwrap_or_default().to_string();
        // A row's content is its blocks, or for a plain prompt one string.
        let content = match &ev["content"] {
            Value::Array(blocks) => blocks.clone(),
            Value::String(text) => vec![json!({ "type": "text", "text": text })],
            _ => Vec::new(),
        };
        if ev["meta"].as_bool() == Some(true) || uuid.is_empty() {
            return Ok(());
        }
        match ev["door"].as_str().unwrap_or_default() {
            "prompt" | "command" => {
                let text = texts(&content);
                let pictures = content.iter().filter(|b| b["type"] == "image").count();
                // Said here and shown already: kept under the id it showed by.
                // The mod's prompts are taken in the order they were sent.
                let ours = ev["door"] == "prompt" && ev["ours"].as_bool() == Some(true);
                let id = match ours.then(|| self.waiting_words.pop_front()).flatten() {
                    Some((id, _)) => id,
                    None => uuid.clone(),
                };
                if let Some(item) = said(&id, &text, pictures, self.cwd()) {
                    self.said_item(item)?;
                }
            }
            "tool-result" => {
                // The pictures in the results, which the row's content no
                // longer holds: told beside it, by call.
                // A picture file read is kept as Claude saw it too: the file
                // may be written over (a screenshot taken again under its
                // name), and each row shows what was seen then.
                for told in ev["pictures"].as_array().into_iter().flatten() {
                    if let Some(id) = told["id"].as_str() {
                        let pictures = super::thumb::pictures_in(&told["images"]);
                        if !pictures.is_empty() {
                            self.shots.insert(id.to_string(), pictures);
                        }
                    }
                }
                for block in content.iter().filter(|b| b["type"] == "tool_result") {
                    let Some(id) = block["tool_use_id"].as_str() else {
                        continue;
                    };
                    let text = match &block["content"] {
                        Value::String(text) => text.clone(),
                        Value::Array(parts) => texts(parts),
                        _ => String::new(),
                    };
                    let error = block["is_error"].as_bool().unwrap_or(false);
                    self.ended.insert(id.to_string(), (text, error));
                    self.tool_ended(id)?;
                }
            }
            "notice" if ev["name"] == "compact_boundary" => {}
            "notice" if ev["name"] == "local_command" => {
                if let Some(item) = said(&uuid, &texts(&content), 0, self.cwd()) {
                    self.said_item(item)?;
                }
            }
            "response" => {
                for block in &content {
                    match block["type"].as_str().unwrap_or_default() {
                        "text" => {
                            let id = self.block_id("text", &uuid);
                            self.written(id, false, block["text"].as_str().unwrap_or_default())?;
                        }
                        "thinking" => {
                            let id = self.block_id("thinking", &uuid);
                            self.written(id, true, block["thinking"].as_str().unwrap_or_default())?;
                        }
                        "tool_use" => {
                            self.block_id("tool", &uuid);
                            if let (Some(id), Some(name)) = (block["id"].as_str(), block["name"].as_str()) {
                                self.tool_started(id, name, block["input"].clone())?;
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// An item of what was said: a command's output goes with the command
    /// before it.
    fn said_item(&mut self, item: Said) -> Result<()> {
        match item {
            Said::Item(item) => self.send_item(item),
            Said::Output(id, output) => self.send_item(Item::Text { id, text: output }),
        }
    }

    /// The id a kept block of the step goes by: the one its words were
    /// written under as they came, else the row's own.
    fn block_id(&mut self, kind: &'static str, uuid: &str) -> String {
        while let Some((index, came)) = self.blocks.pop_front() {
            if came != kind {
                continue;
            }
            if let Some((turn, step)) = &self.step {
                let key = (turn.clone(), *step, index);
                if let Some(writing) = self.writing.get(&key) {
                    return writing.id.clone();
                }
            }
            break;
        }
        uuid.to_string()
    }

    /// Words written whole: the ones as they came are done with.
    fn written(&mut self, id: String, thinking: bool, text: &str) -> Result<()> {
        self.writing.retain(|_, writing| writing.id != id);
        if text.trim().is_empty() {
            return Ok(());
        }
        let text = head(text, TEXT_KEPT);
        self.send_item(if thinking { Item::Thinking { id, text } } else { Item::Text { id, text } })
    }

    fn chunk(&mut self, ev: &Value) -> Result<()> {
        let turn = ev["turn"].as_str().unwrap_or_default().to_string();
        let step = ev["step"].as_u64().unwrap_or(0);
        let index = ev["index"].as_u64().unwrap_or(0);
        if self.step.as_ref() != Some(&(turn.clone(), step)) {
            self.step = Some((turn.clone(), step));
            self.blocks.clear();
        }
        let kind: &'static str = match ev["kind"].as_str() {
            Some("text") => "text",
            Some("thinking") => "thinking",
            Some("tool") => "tool",
            _ => return Ok(()),
        };
        if !self.blocks.iter().any(|(i, _)| *i == index) {
            self.blocks.push_back((index, kind));
        }
        if kind == "tool" {
            return Ok(());
        }
        let key = (turn.clone(), step, index);
        let writing = self.writing.entry(key).or_insert_with(|| Writing {
            id: format!("w-{turn}-{step}-{index}"),
            thinking: kind == "thinking",
            text: String::new(),
            sent: None,
        });
        writing.text.push_str(ev["text"].as_str().unwrap_or_default());
        Ok(())
    }

    /// Words still being written, sent as far as they go, every so often.
    fn write_on(&mut self) -> Result<()> {
        let mut due = Vec::new();
        for writing in self.writing.values_mut() {
            if writing.text.trim().is_empty() || writing.sent.is_some_and(|at| at.elapsed() < WRITING_EVERY) {
                continue;
            }
            writing.sent = Some(Instant::now());
            let text = head(&writing.text, TEXT_KEPT);
            let id = writing.id.clone();
            due.push(if writing.thinking { Item::Thinking { id, text } } else { Item::Text { id, text } });
        }
        for item in due {
            self.last_item = Some(item.id().to_string());
            self.out.send(&Ev::partial(item))?;
        }
        Ok(())
    }

    fn turn(&mut self, ev: &Value) -> Result<()> {
        let turn = ev["turn"].as_str().unwrap_or_default().to_string();
        if ev["phase"] == "start" {
            self.since = Some(super::now());
            return self.tell_state();
        }
        // Words that never came whole (the turn was stopped) are left as
        // far as they went.
        let left: Vec<Writing> = self.writing.drain().map(|(_, writing)| writing).collect();
        for writing in left {
            if !writing.text.trim().is_empty() {
                let text = head(&writing.text, TEXT_KEPT);
                let id = writing.id;
                self.send_item(if writing.thinking { Item::Thinking { id, text } } else { Item::Text { id, text } })?;
            }
        }
        self.blocks.clear();
        self.step = None;
        // What came of the turn's calls is all in: their ends are let go.
        self.tools.clear();
        self.ended.clear();
        self.results.clear();
        self.shots.clear();
        if ev["aborted"].as_bool() == Some(true) {
            self.send_item(Item::Note {
                id: format!("interrupted-{turn}"),
                note: NoteKind::Interrupted,
                text: String::new(),
            })?;
        }
        let ms = ev["ms"].as_u64().unwrap_or(0);
        if let Some(after) = self.last_item.clone().filter(|_| ms >= 1000) {
            self.out.send(&Ev::Worked {
                id: format!("worked-{turn}"),
                after,
                seconds: ms / 1000,
            })?;
        }
        self.since = None;
        self.tell_state()
    }

    /// A call the model asked for, about to be decided and run.
    fn tool_started(&mut self, id: &str, name: &str, input: Value) -> Result<()> {
        let item = tool_item(id, name, &input, None, self.cwd());
        self.tools.insert(id.to_string(), (name.to_string(), input));
        self.send_item(item)
    }

    /// What the tool kept of a call (its output, a change's patch), told
    /// before or after the row of its result.
    fn done(&mut self, ev: &Value) -> Result<()> {
        let Some(id) = ev["id"].as_str() else {
            return Ok(());
        };
        let mut result = ev["result"].clone();
        if ev["interrupted"].as_bool() == Some(true) {
            result = json!({ "interrupted": true });
        }
        self.results.insert(id.to_string(), (result, ev["failed"].as_str().map(str::to_string)));
        if self.ended.contains_key(id) {
            self.tool_ended(id)?;
        }
        Ok(())
    }

    /// A call's item once it ended: its result's row, with what the tool kept.
    fn tool_ended(&mut self, id: &str) -> Result<()> {
        let Some((name, input)) = self.tools.get(id).cloned() else {
            return Ok(());
        };
        let (text, error) = self.ended.get(id).cloned().unwrap_or_default();
        let (result, failed) = self.results.get(id).cloned().unwrap_or((Value::Null, None));
        let end = End {
            result: &result,
            text: if text.is_empty() { failed.as_deref().unwrap_or_default() } else { &text },
            error: error || failed.is_some(),
            deny: None,
        };
        let mut item = tool_item(id, &name, &input, Some(&end), self.cwd());
        // The pictures a call gave back, as Claude saw them: over the file
        // a picture read names, which may hold another since.
        if let (Item::Command { pictures, .. } | Item::Tool { pictures, .. }, Some(shots)) = (&mut item, self.shots.get(id)) {
            pictures.clone_from(shots);
        }
        // Whole: nothing more comes for it.
        if self.ended.contains_key(id) && self.results.contains_key(id) {
            self.tools.remove(id);
            self.ended.remove(id);
            self.results.remove(id);
            self.shots.remove(id);
        }
        self.send_item(item)
    }

    fn ask(&mut self, ev: &Value) -> Result<()> {
        let key = ev["key"].as_str().unwrap_or_default().to_string();
        if key.is_empty() {
            return Ok(());
        }
        let held = ev["held"].as_bool().unwrap_or(false);
        let tool = ev["tool"].as_str().unwrap_or_default();
        let input = &ev["input"];
        let (kind, body, questions) = match tool {
            "AskUserQuestion" => (AskKind::Question, String::new(), questions(&input["questions"])),
            "Bash" => (
                AskKind::Command,
                head(input["command"].as_str().unwrap_or_default(), ASK_KEPT),
                Vec::new(),
            ),
            "Edit" | "MultiEdit" | "Write" | "NotebookEdit" => {
                let path = input["file_path"].as_str().or(input["notebook_path"].as_str()).unwrap_or_default();
                (AskKind::Edit, shown(path.to_string(), self.cwd()), Vec::new())
            }
            _ => {
                let said = brief(input);
                let body = if said.is_empty() { tool_name(tool) } else { format!("{}: {said}", tool_name(tool)) };
                (AskKind::Tool, head(&body, ASK_KEPT), Vec::new())
            }
        };
        let choices = match (held, kind) {
            (false, _) => Vec::new(),
            (true, AskKind::Question) => vec![Choice::Accept, Choice::Decline],
            (true, _) if ev["remember"].as_bool() == Some(true) => vec![Choice::Accept, Choice::AcceptForSession, Choice::Decline],
            (true, _) => vec![Choice::Accept, Choice::Decline],
        };
        let asked: Vec<String> = questions.iter().map(|q| q.question.clone()).collect();
        let ask = Ask {
            id: key.clone(),
            kind,
            body,
            reason: None,
            // Where a command would run; a question is asked of no folder.
            cwd: (kind != AskKind::Question).then(|| self.cwd.clone()).flatten(),
            choices,
            questions,
        };
        self.asks.retain(|waiting| waiting.ask.id != key);
        self.asks.push(Waiting {
            ask: ask.clone(),
            questions: asked,
            held,
        });
        self.out.send(&Ev::Ask { ask })?;
        self.tell_state()
    }

    fn models(&mut self, ev: &Value) -> Result<()> {
        let options: Vec<String> = ev["options"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|o| o.as_str().map(str::to_string))
            .collect();
        let using = ev["model"].as_str().unwrap_or_default().to_string();
        let value = ev["value"].as_str().unwrap_or_default().to_string();
        if let Some(effort) = ev["effort"].as_str().filter(|e| !e.is_empty()) {
            self.effort = Some(effort.to_string());
        }
        // The row reads "Default (recommended)" until a model is picked.
        let current = if options.contains(&value) { value } else { "default".to_string() };
        let models = options
            .iter()
            .map(|id| ModelChoice {
                id: id.clone(),
                name: model_name(id, (id == &current).then_some(using.as_str())),
                efforts: if id.starts_with("haiku") { Vec::new() } else { EFFORTS.iter().map(|e| e.to_string()).collect() },
                description: String::new(),
            })
            .collect();
        self.models = Some((options, current.clone(), using));
        self.out.send(&Ev::Models {
            current: Some(current),
            effort: self.effort.clone(),
            models,
        })?;
        Ok(())
    }
}

/// A model of Claude Code's model row, as people know it: the one in use
/// says which it is when it is the default.
fn model_name(id: &str, using: Option<&str>) -> String {
    let (base, long) = match id.strip_suffix("[1m]") {
        Some(base) => (base, true),
        None => (id, false),
    };
    let mut name = match base {
        "default" => "Default".to_string(),
        "opusplan" => "Opus Plan".to_string(),
        other => {
            let mut chars = other.chars();
            chars.next().map(|first| first.to_uppercase().chain(chars).collect()).unwrap_or_default()
        }
    };
    if long {
        name.push_str(" (1M)");
    }
    if base == "default" {
        if let Some(model) = using.filter(|m| !m.is_empty()) {
            name = format!("{name} ({})", pretty_model(model));
        }
    }
    name
}

/// `claude-opus-5-5` as "Opus 5.5".
fn pretty_model(model: &str) -> String {
    let rest = model.strip_prefix("claude-").unwrap_or(model);
    let mut words = rest.split('-');
    let Some(family) = words.next() else {
        return model.to_string();
    };
    let numbers: Vec<&str> = words.take_while(|w| w.chars().all(|c| c.is_ascii_digit()) && w.len() <= 2).collect();
    let mut chars = family.chars();
    let family: String = chars.next().map(|first| first.to_uppercase().chain(chars).collect()).unwrap_or_default();
    if numbers.is_empty() {
        family
    } else {
        format!("{family} {}", numbers.join("."))
    }
}

/// A tool's name as the chat shows it: an MCP tool's as its server's.
fn tool_name(tool: &str) -> String {
    match tool.strip_prefix("mcp__").and_then(|rest| rest.split_once("__")) {
        Some((server, name)) => format!("{server} {name}"),
        None => tool.to_string(),
    }
}

fn questions(asked: &Value) -> Vec<Question> {
    asked
        .as_array()
        .into_iter()
        .flatten()
        .map(|q| Question {
            question: q["question"].as_str().unwrap_or_default().to_string(),
            header: q["header"].as_str().unwrap_or_default().to_string(),
            options: q["options"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|o| QuestionOption {
                    label: o["label"].as_str().unwrap_or_default().to_string(),
                    description: o["description"].as_str().unwrap_or_default().to_string(),
                })
                .collect(),
            multi: q["multiSelect"].as_bool().unwrap_or(false),
        })
        .collect()
}

/// The text blocks of a row, joined.
fn texts(content: &[Value]) -> String {
    content
        .iter()
        .filter(|b| b["type"] == "text")
        .filter_map(|b| b["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

/// What a row of what was said becomes.
enum Said {
    Item(Item),
    /// A command's output, shown as said back.
    Output(String, String),
}

/// The text between `<tag>` and `</tag>`, if `text` holds the tag.
fn tagged<'a>(text: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = text.find(&open)? + open.len();
    let end = text[start..].find(&close).map_or(text.len(), |at| start + at);
    Some(&text[start..end])
}

/// Escape sequences taken out of a command's output.
fn plain(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for c in chars.by_ref() {
                    if c.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

/// A row of what was said (a prompt, a command typed and what it printed)
/// as an item: none for what the person never saw as said.
fn said(id: &str, text: &str, pictures: usize, cwd: Option<&str>) -> Option<Said> {
    let _ = cwd;
    let trimmed = text.trim();
    let id = id.to_string();
    if trimmed.is_empty() && pictures == 0 {
        return None;
    }
    for hidden in ["<system-reminder>", "<local-command-caveat>", "<task-notification>", "<command-message>"] {
        if trimmed.starts_with(hidden) {
            return None;
        }
    }
    if let Some(name) = tagged(trimmed, "command-name") {
        let args = tagged(trimmed, "command-args").unwrap_or_default().trim();
        let name = name.trim();
        let name = if name.starts_with('/') { name.to_string() } else { format!("/{name}") };
        let text = if args.is_empty() { name } else { format!("{name} {args}") };
        return Some(Said::Item(Item::User {
            id,
            text,
            attachments: Vec::new(),
        }));
    }
    for tag in ["local-command-stdout", "local-command-stderr", "bash-stdout"] {
        if trimmed.starts_with(&format!("<{tag}>")) {
            let output = plain(tagged(trimmed, tag).unwrap_or_default()).trim().to_string();
            return (!output.is_empty()).then(|| Said::Output(id, tail(&output, OUTPUT_KEPT)));
        }
    }
    if let Some(command) = tagged(trimmed, "bash-input").filter(|_| trimmed.starts_with("<bash-input>")) {
        return Some(Said::Item(Item::Command {
            id,
            command: head(command.trim(), TEXT_KEPT),
            status: Status::Done,
            output: String::new(),
            exit: None,
            looks: Vec::new(),
            pictures: Vec::new(),
        }));
    }
    if trimmed.starts_with("[Request interrupted by user") {
        return Some(Said::Item(Item::Note {
            id,
            note: NoteKind::Interrupted,
            text: String::new(),
        }));
    }
    if trimmed.starts_with("This session is being continued from a previous conversation") {
        return Some(Said::Item(Item::Note {
            id,
            note: NoteKind::Compacted,
            text: String::new(),
        }));
    }
    let (words, mut attachments) = uploads(trimmed);
    for _ in 0..pictures {
        attachments.push(Attachment {
            name: "image".into(),
            image: true,
            path: None,
            thumb: None,
        });
    }
    Some(Said::Item(Item::User {
        id,
        text: head(&words, TEXT_KEPT),
        attachments,
    }))
}

/// What was said, and the files it ends with: the paths of files sent from
/// the phone, a line each, as Claude was told them.
fn uploads(text: &str) -> (String, Vec<Attachment>) {
    let mut lines: Vec<&str> = text.trim_end().lines().collect();
    let mut files = Vec::new();
    while let Some(last) = lines.last() {
        let path = last.trim();
        if !path.starts_with('/') || !path.contains("/ThinkTerm_Uploads/") {
            break;
        }
        let name = path.rsplit('/').next().unwrap_or(path).to_string();
        let lower = name.to_ascii_lowercase();
        let image = [".png", ".jpg", ".jpeg", ".gif", ".webp", ".heic"].iter().any(|ext| lower.ends_with(ext));
        files.push(Attachment {
            name,
            image,
            path: Some(path.to_string()),
            thumb: None,
        });
        lines.pop();
    }
    files.reverse();
    (lines.join("\n").trim_end().to_string(), files)
}

/// How a call ended, as the mod tells it.
struct End<'a> {
    result: &'a Value,
    text: &'a str,
    error: bool,
    deny: Option<&'a str>,
}

fn status_of(end: Option<&End>) -> Status {
    let Some(end) = end else {
        return Status::Running;
    };
    if end.deny.is_some() {
        return Status::Declined;
    }
    if end.error {
        let text = end.text.to_ascii_lowercase();
        let declined = text.contains("doesn't want to proceed") || text.contains("was rejected") || text.contains("declined");
        return if declined { Status::Declined } else { Status::Failed };
    }
    if end.result["interrupted"].as_bool() == Some(true) {
        return Status::Failed;
    }
    Status::Done
}

/// A tool call as an item, from what it was asked and, once it ended, how.
fn tool_item(id: &str, tool: &str, input: &Value, end: Option<&End>, cwd: Option<&str>) -> Item {
    let id = id.to_string();
    let status = status_of(end);
    let field = |key: &str| input[key].as_str().unwrap_or_default().to_string();
    let file = || shown(field("file_path"), cwd);
    match tool {
        "Bash" => {
            let output = match end {
                Some(end) => {
                    let out = end.result["stdout"].as_str().unwrap_or_default();
                    let err = end.result["stderr"].as_str().unwrap_or_default();
                    let joined = [out, err].iter().filter(|s| !s.trim().is_empty()).cloned().collect::<Vec<_>>().join("\n");
                    if joined.is_empty() {
                        end.text.to_string()
                    } else {
                        joined
                    }
                }
                None => String::new(),
            };
            let exit = end.filter(|end| end.error).and_then(|end| {
                end.text
                    .lines()
                    .find_map(|line| line.trim().strip_prefix("Exit code ").and_then(|n| n.trim().parse().ok()))
            });
            Item::Command {
                id,
                command: head(&field("command"), TEXT_KEPT),
                status,
                output: tail(&output, OUTPUT_KEPT),
                exit,
                looks: Vec::new(),
                pictures: Vec::new(),
            }
        }
        "Read" => {
            let path = field("file_path");
            let seen = status == Status::Done && super::thumb::is_picture_path(&path);
            let mut item = look(id, format!("read {}", file()), LookKind::Read, file(), status);
            if let Item::Command { pictures, looks, .. } = &mut item {
                // Shown from where the agent works; opened from where it is.
                if path.starts_with('/') {
                    if let Some(look) = looks.first_mut() {
                        look.path = Some(path.clone());
                    }
                }
                // A picture it read: shown under the line.
                if seen {
                    pictures.push(super::thumb::picture_file(&path));
                }
            }
            item
        }
        "Grep" => {
            let pattern = field("pattern");
            look(id, format!("grep {pattern}"), LookKind::Search, pattern, status)
        }
        "Glob" => {
            let pattern = field("pattern");
            look(id, format!("glob {pattern}"), LookKind::Search, pattern, status)
        }
        "LS" => {
            let dir = shown(field("path"), cwd);
            look(id, format!("ls {dir}"), LookKind::List, dir, status)
        }
        "Edit" | "MultiEdit" | "Write" | "NotebookEdit" => {
            let files = edited_files(tool, input, end, cwd);
            if files.is_empty() {
                Item::Tool {
                    id,
                    name: tool.to_string(),
                    detail: file(),
                    status,
                    pictures: Vec::new(),
                }
            } else {
                Item::Edit { id, status, files }
            }
        }
        "WebSearch" => Item::Tool {
            id,
            name: "web_search".into(),
            detail: head(&field("query"), DETAIL_KEPT),
            status,
            pictures: Vec::new(),
        },
        "WebFetch" => Item::Tool {
            id,
            name: "web_fetch".into(),
            detail: head(&field("url"), DETAIL_KEPT),
            status,
            pictures: Vec::new(),
        },
        "Task" | "Agent" => Item::Tool {
            id,
            name: "agent".into(),
            detail: head(
                input["description"].as_str().or(input["prompt"].as_str()).unwrap_or_default(),
                DETAIL_KEPT,
            ),
            status,
            pictures: Vec::new(),
        },
        "AskUserQuestion" => {
            let mut detail = questions_of(&input["questions"]);
            if let Some(answers) = end.and_then(|end| end.result["answers"].as_object()) {
                for answer in answers.values().filter_map(Value::as_str) {
                    detail.push_str(&format!("\n→ {answer}"));
                }
            }
            Item::Tool {
                id,
                name: "question".into(),
                detail: head(&detail, DETAIL_KEPT * 4),
                status,
                pictures: Vec::new(),
            }
        }
        "Skill" => Item::Tool {
            id,
            name: "skill".into(),
            detail: head(&field("skill"), DETAIL_KEPT),
            status,
            pictures: Vec::new(),
        },
        "TodoWrite" => {
            let todos = input["todos"].as_array().cloned().unwrap_or_default();
            let doing = todos
                .iter()
                .find(|t| t["status"] == "in_progress")
                .and_then(|t| t["activeForm"].as_str().or(t["content"].as_str()))
                .unwrap_or_default();
            Item::Tool {
                id,
                name: "todo".into(),
                detail: head(doing, DETAIL_KEPT),
                status,
                pictures: Vec::new(),
            }
        }
        "ExitPlanMode" => Item::Tool {
            id,
            name: "plan".into(),
            detail: head(field("plan").lines().find(|l| !l.trim().is_empty()).unwrap_or_default().trim(), DETAIL_KEPT),
            status,
            pictures: Vec::new(),
        },
        _ => Item::Tool {
            id,
            name: tool_name(tool),
            detail: brief(input),
            status,
            pictures: Vec::new(),
        },
    }
}

/// The files a change touched: as the tool tells once it is done (its
/// patch), else made up from what it was asked.
fn edited_files(tool: &str, input: &Value, end: Option<&End>, cwd: Option<&str>) -> Vec<crate::FileEdit> {
    let path = input["file_path"].as_str().or(input["notebook_path"].as_str()).unwrap_or_default();
    if path.is_empty() {
        return Vec::new();
    }
    let path = shown(path.to_string(), cwd);
    if let Some(hunks) = end.and_then(|end| end.result["structuredPatch"].as_array()).filter(|h| !h.is_empty()) {
        let diff = hunks
            .iter()
            .map(|hunk| {
                let n = |key: &str| hunk[key].as_u64().unwrap_or(0);
                let lines: Vec<&str> = hunk["lines"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
                format!(
                    "@@ -{},{} +{},{} @@\n{}",
                    n("oldStart"),
                    n("oldLines"),
                    n("newStart"),
                    n("newLines"),
                    lines.join("\n")
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        return capped(vec![file_edit(&path, &diff)]);
    }
    let lines = |text: &str, sign: char| -> Vec<String> { text.lines().map(|l| format!("{sign}{l}")).collect() };
    let change = |old: &str, new: &str| -> String {
        let mut body = lines(old, '-');
        body.extend(lines(new, '+'));
        format!("@@\n{}", body.join("\n"))
    };
    let diff = match tool {
        "Write" => format!("@@\n{}", lines(input["content"].as_str().unwrap_or_default(), '+').join("\n")),
        "Edit" => change(
            input["old_string"].as_str().unwrap_or_default(),
            input["new_string"].as_str().unwrap_or_default(),
        ),
        "MultiEdit" => input["edits"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|edit| {
                change(
                    edit["old_string"].as_str().unwrap_or_default(),
                    edit["new_string"].as_str().unwrap_or_default(),
                )
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => return Vec::new(),
    };
    vec![file_edit(&path, &diff)]
}

/// History, as the session keeps it (its rows: a prompt, a block of an
/// answer, a call's results), as items.
fn history_items(rows: &[Value], cwd: Option<&str>) -> Vec<Item> {
    let mut items: Vec<Item> = Vec::new();
    for row in rows {
        let uuid = row["uuid"].as_str().unwrap_or_default();
        let text = row["text"].as_str().unwrap_or_default();
        match row["role"].as_str() {
            Some("user") => {
                if text.trim().is_empty() {
                    continue;
                }
                match said(uuid, text, 0, cwd) {
                    Some(Said::Item(item)) => items.push(item),
                    Some(Said::Output(id, output)) => items.push(Item::Text { id, text: output }),
                    None => {}
                }
            }
            Some("assistant") => {
                if !text.trim().is_empty() && !uuid.is_empty() {
                    items.push(Item::Text {
                        id: uuid.to_string(),
                        text: head(text, TEXT_KEPT),
                    });
                }
                for call in row["tools"].as_array().into_iter().flatten() {
                    let Some(id) = call["id"].as_str() else {
                        continue;
                    };
                    let tool = call["tool"].as_str().unwrap_or_default();
                    let answered = !call["text"].is_null() || !call["result"].is_null();
                    let end = End {
                        result: &call["result"],
                        text: call["text"].as_str().unwrap_or_default(),
                        error: call["error"].as_bool().unwrap_or(false),
                        deny: None,
                    };
                    let mut item = tool_item(id, tool, &call["input"], answered.then_some(&end), cwd);
                    // The pictures the call gave back, the newest few: kept
                    // as files, as they are when they come live.
                    if let Item::Command { pictures, .. } | Item::Tool { pictures, .. } = &mut item {
                        let seen = super::thumb::pictures_in(&call["pictures"]);
                        if !seen.is_empty() {
                            *pictures = seen;
                        }
                    }
                    items.push(item);
                }
            }
            _ => {}
        }
    }
    items
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(pane: u64, mux: Option<&str>) -> Target {
        Target {
            pane,
            agent: "claude".into(),
            cwd: None,
            mux_socket: mux.map(str::to_string),
        }
    }

    #[test]
    fn the_module_carries_its_mark_and_version() {
        assert!(MODULE.starts_with(MARK));
        assert!(version_of(MODULE) >= 1);
    }

    #[test]
    fn install_puts_the_mod_in_and_leaves_others_alone() {
        let claude = tempfile::tempdir().unwrap();
        let dir = install_in(claude.path()).unwrap();
        assert_eq!(dir, claude.path().join("skills").join(MOD_NAME));
        for (name, text) in MOD_FILES {
            assert_eq!(std::fs::read_to_string(dir.join(name)).unwrap(), text, "{name}");
        }
        // An older one of ours is brought up to date; a newer one is kept.
        let module = dir.join("hooks/register.ts");
        std::fs::write(&module, format!("{MARK}\nconst MOD_VERSION = 0\n")).unwrap();
        install_in(claude.path()).unwrap();
        assert_eq!(std::fs::read_to_string(&module).unwrap(), MODULE);
        let newer = format!("{MARK}\nconst MOD_VERSION = {}\n", version_of(MODULE) + 1);
        std::fs::write(&module, &newer).unwrap();
        install_in(claude.path()).unwrap();
        assert_eq!(std::fs::read_to_string(&module).unwrap(), newer);
        // Someone else's folder of the same name is left as it is.
        std::fs::write(&module, "// someone else's\n").unwrap();
        assert!(install_in(claude.path()).is_err());
        assert_eq!(std::fs::read_to_string(&module).unwrap(), "// someone else's\n");
        let other = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(other.path().join("skills").join(MOD_NAME)).unwrap();
        std::fs::write(other.path().join("skills").join(MOD_NAME).join("SKILL.md"), "mine").unwrap();
        assert!(install_in(other.path()).is_err());
        // No Claude Code here: nothing is made.
        let none = claude.path().join("not-there");
        assert!(install_in(&none).is_err());
        assert!(!none.exists());
    }

    #[test]
    fn a_note_is_found_for_its_pane_while_its_claude_lives() {
        let dir = tempfile::tempdir().unwrap();
        // A process of pane 7's, as a Claude in it would be.
        let mut claude = std::process::Command::new("sleep").arg("30").env("WEZTERM_PANE", "7").spawn().unwrap();
        let me = claude.id();
        let note = |pid: u32, pane: u64, mux: &str| format!(r#"{{"agent":"claude","pid":{pid},"pane":{pane},"mux_socket":"{mux}","version":1}}"#);
        std::fs::write(dir.path().join(format!("claude-{me}.json")), note(me, 7, "/run/a")).unwrap();
        std::fs::write(dir.path().join("claude-2.json"), note(u32::MAX - 7, 8, "/run/a")).unwrap();
        std::fs::write(dir.path().join("pi-1.json"), note(me, 9, "/run/a")).unwrap();
        assert_eq!(find(dir.path(), &target(7, None)).map(|n| n.pid), Some(me));
        assert_eq!(find(dir.path(), &target(7, Some("/run/a"))).map(|n| n.pid), Some(me));
        assert!(find(dir.path(), &target(7, Some("/run/b"))).is_none(), "another mux's pane 7");
        assert!(find(dir.path(), &target(8, None)).is_none(), "its Claude is gone");
        assert!(find(dir.path(), &target(9, None)).is_none(), "Pi's note");
        // A note of pane 8's whose pid a process of another pane took.
        std::fs::write(dir.path().join(format!("claude-{me}.json")), note(me, 8, "/run/a")).unwrap();
        assert!(find(dir.path(), &target(8, None)).is_none(), "not that pane's process");
        let _ = claude.kill();
        let _ = claude.wait();
    }

    #[test]
    fn what_gone_processes_left_is_taken_away() {
        let dir = tempfile::tempdir().unwrap();
        let me = std::process::id();
        let gone = u32::MAX - 7;
        for name in [format!("claude-{me}.json"), format!("claude-{gone}.json"), format!("claude-{me}-{gone}.sock"), format!("claude-{me}-{me}.sock"), "pi-1.json".into()] {
            std::fs::write(dir.path().join(name), "").unwrap();
        }
        forget_stale(dir.path());
        let mut left: Vec<String> = std::fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        left.sort();
        let mut want = vec![format!("claude-{me}-{me}.sock"), format!("claude-{me}.json"), "pi-1.json".to_string()];
        want.sort();
        assert_eq!(left, want);
    }

    #[test]
    fn requests_are_read_whole() {
        let raw = b"POST /ev?copy=a%20b&ack=3 HTTP/1.1\r\nHost: bridge\r\nContent-Length: 4\r\n\r\n{\"x\"";
        let request = read_request(&mut std::io::Cursor::new(raw.to_vec())).unwrap();
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/ev");
        assert_eq!(request.query["copy"], "a b");
        assert_eq!(request.query["ack"], "3");
        assert_eq!(request.body, b"{\"x\"");
        let cut = b"GET /ops HTTP/1.1\r\nContent-Length: 10\r\n\r\nshort";
        assert!(read_request(&mut std::io::Cursor::new(cut.to_vec())).is_err());
        assert_eq!(unescape("%E6%B1%89%zz+"), "汉%zz ");
    }

    #[test]
    fn ops_go_to_the_copy_that_asks_and_again_after_a_reload() {
        let link: Link = Arc::new((Mutex::new(Shared::default()), Condvar::new()));
        enqueue(&link, json!({ "op": "send", "text": "one" }));
        let got = take_ops(&link, "a", 0, Duration::ZERO).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0]["seq"], 1);
        // Asked again without acknowledging it: the answer was lost (the mod
        // links anew), and it goes again; acknowledged, it is done with.
        assert_eq!(take_ops(&link, "a", 0, Duration::ZERO).unwrap()[0]["seq"], 1);
        assert_eq!(take_ops(&link, "a", 1, Duration::ZERO), Some(vec![]));
        enqueue(&link, json!({ "op": "send", "text": "one" }));
        let first = take_ops(&link, "a", 1, Duration::ZERO).unwrap();
        assert_eq!(first[0]["seq"], 2);
        // The module reloaded before it said it took it: the new copy gets it,
        // and the old one, still asking while it settles, gets nothing again.
        let again = take_ops(&link, "b", 0, Duration::ZERO).unwrap();
        assert_eq!(again.len(), 1);
        assert_eq!(again[0]["text"], "one");
        assert_eq!(take_ops(&link, "a", 0, Duration::ZERO), None);
        // Acknowledged: gone.
        enqueue(&link, json!({ "op": "interrupt" }));
        let next = take_ops(&link, "b", 2, Duration::ZERO).unwrap();
        assert_eq!(next.len(), 1);
        assert_eq!(next[0]["seq"], 3);
        assert!(lock(&link).ops.iter().all(|slot| slot.seq == 3));
    }

    #[test]
    fn an_answer_waits_for_its_taker() {
        let link: Link = Arc::new((Mutex::new(Shared::default()), Condvar::new()));
        assert_eq!(take_answer(&link, "k", Duration::ZERO), json!({}));
        lock(&link).answers.insert("k".into(), json!({ "behavior": "allow" }));
        assert_eq!(take_answer(&link, "k", Duration::ZERO), json!({ "answer": { "behavior": "allow" } }));
        lock(&link).gone.insert("k".into());
        assert_eq!(take_answer(&link, "k", Duration::ZERO), json!({ "gone": true }));
    }

    #[test]
    fn claudes_field_is_read_from_its_screen() {
        let rule = "────────────────────────────────";
        let screen = |field: &str| vec!["● said".to_string(), rule.into(), field.to_string(), rule.into(), "  ⏸ manual mode on".into()];
        assert_eq!(field_empty(&screen("❯ ")), Some(true));
        assert_eq!(field_empty(&screen("❯ half a sentence")), Some(false));
        assert_eq!(field_empty(&screen("no prompt sign")), None);
        assert_eq!(field_empty(&["nothing drawn".to_string()]), None);
    }

    /// The rows of a session as `$.session.messages` gives them, cut down
    /// as the mod sends them: taken from a sandboxed Claude Code 2.1.331.
    fn sample_rows() -> Vec<Value> {
        serde_json::from_value(json!([
            {"role": "user", "uuid": "u1", "text": "PROBE:hello", "tools": [], "results": 0},
            {"role": "assistant", "uuid": "a0", "text": "", "tools": [], "results": 0},
            {"role": "assistant", "uuid": "a1", "text": "Hello **there**.", "tools": [], "results": 0},
            {"role": "user", "uuid": "u2", "text": "run it\n/home/me/ThinkTerm_Uploads/shot.png", "tools": [], "results": 0},
            {"role": "assistant", "uuid": "a2", "text": "", "results": 0, "tools": [
                {"id": "t1", "tool": "Bash", "input": {"command": "echo hi", "description": "Say hi"},
                 "result": {"stdout": "hi", "stderr": "", "interrupted": false}, "text": "hi", "error": false}]},
            {"role": "user", "uuid": "u3", "text": "", "tools": [], "results": 1},
            {"role": "assistant", "uuid": "a3", "text": "", "results": 0, "tools": [
                {"id": "t2", "tool": "Edit", "input": {"file_path": "/w/probe.txt", "old_string": "two", "new_string": "TWO"},
                 "result": {"filePath": "/w/probe.txt", "structuredPatch": [{"oldStart": 1, "oldLines": 3, "newStart": 1, "newLines": 3,
                   "lines": [" one", "-two", "+TWO", " three"]}]}, "text": "The file was updated.", "error": false}]},
            {"role": "assistant", "uuid": "a4", "text": "", "results": 0, "tools": [
                {"id": "t3", "tool": "Write", "input": {"file_path": "/w/x.txt", "content": "a"},
                 "text": "The user doesn't want to proceed with this tool use.", "error": true}]},
            {"role": "user", "uuid": "u4", "text": "[Request interrupted by user for tool use]", "tools": [], "results": 0},
            {"role": "user", "uuid": "u5", "text": "<command-name>/effort</command-name>\n            <command-message>effort</command-message>\n            <command-args>high</command-args>", "tools": [], "results": 0},
            {"role": "user", "uuid": "u6", "text": "<local-command-stdout>Set effort level to \u{1b}[1mhigh\u{1b}[22m</local-command-stdout>", "tools": [], "results": 0},
            {"role": "user", "uuid": "u7", "text": "This session is being continued from a previous conversation that ran out of context.", "tools": [], "results": 0},
            {"role": "assistant", "uuid": "a5", "text": "", "results": 0, "tools": [
                {"id": "t4", "tool": "Read", "input": {"file_path": "/w/src/main.rs"}, "text": "fn main", "error": false},
                {"id": "t5", "tool": "mcp__docs__search", "input": {"query": "hooks"}}]}
        ]))
        .unwrap()
    }

    #[test]
    fn history_rows_become_items() {
        let items = history_items(&sample_rows(), Some("/w"));
        let ids: Vec<&str> = items.iter().map(Item::id).collect();
        assert_eq!(ids, ["u1", "a1", "u2", "t1", "t2", "t3", "u4", "u5", "u6", "u7", "t4", "t5"]);
        assert!(matches!(&items[0], Item::User { text, .. } if text == "PROBE:hello"));
        assert!(matches!(&items[2], Item::User { text, attachments, .. }
            if text == "run it" && attachments.len() == 1 && attachments[0].image && attachments[0].name == "shot.png"));
        assert!(matches!(&items[3], Item::Command { command, output, status: Status::Done, .. } if command == "echo hi" && output == "hi"));
        match &items[4] {
            Item::Edit { files, status: Status::Done, .. } => {
                assert_eq!(files[0].path, "probe.txt");
                assert_eq!((files[0].added, files[0].removed), (1, 1));
                assert!(files[0].diff.starts_with("@@ -1,3 +1,3 @@"));
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(&items[5], Item::Edit { status: Status::Declined, .. }));
        assert!(matches!(&items[6], Item::Note { note: NoteKind::Interrupted, .. }));
        assert!(matches!(&items[7], Item::User { text, .. } if text == "/effort high"));
        assert!(matches!(&items[8], Item::Text { text, .. } if text == "Set effort level to high"));
        assert!(matches!(&items[9], Item::Note { note: NoteKind::Compacted, .. }));
        assert!(matches!(&items[10], Item::Command { looks, status: Status::Done, .. } if looks[0].target == "src/main.rs"));
        assert!(matches!(&items[11], Item::Tool { name, status: Status::Running, .. } if name == "docs search"));
    }

    /// The lines a chat sent, as events.
    fn lines(buffer: &Arc<Mutex<Vec<u8>>>) -> Vec<Ev> {
        let text = String::from_utf8(buffer.lock().unwrap().clone()).unwrap();
        text.lines().map(|line| serde_json::from_str(line).unwrap()).collect()
    }

    struct Sink(Arc<Mutex<Vec<u8>>>);
    impl Write for Sink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn chat() -> (Chat, Arc<Mutex<Vec<u8>>>, Link) {
        let buffer = Arc::new(Mutex::new(Vec::new()));
        let link: Link = Arc::new((Mutex::new(Shared::default()), Condvar::new()));
        let chat = Chat::new(Out::new(Box::new(Sink(buffer.clone()))), link.clone(), Some("/w".into()));
        (chat, buffer, link)
    }

    #[test]
    fn a_turn_streams_then_keeps_its_words_under_one_id() {
        let (mut chat, buffer, link) = chat();
        let t = target(1, None);
        chat.event("c1", &json!({"t": "hello", "mod": 1, "cwd": "/w"}), &t).unwrap();
        assert_eq!(take_ops(&link, "c1", 0, Duration::ZERO).unwrap()[0]["op"], "history");
        chat.event("c1", &json!({"t": "rows", "asked": 400, "rows": sample_rows()}), &t).unwrap();
        chat.event("c1", &json!({"t": "turn", "phase": "start", "turn": "T"}), &t).unwrap();
        for word in ["Thinking ", "hard."] {
            chat.event("c1", &json!({"t": "chunk", "turn": "T", "step": 0, "index": 0, "kind": "thinking", "text": word}), &t).unwrap();
        }
        for word in ["Hello ", "world."] {
            chat.event("c1", &json!({"t": "chunk", "turn": "T", "step": 0, "index": 1, "kind": "text", "text": word}), &t).unwrap();
        }
        chat.write_on().unwrap();
        chat.event("c1", &json!({"t": "row", "door": "response", "uuid": "R1", "type": "assistant", "content": [{"type": "thinking", "thinking": "Thinking hard."}]}), &t).unwrap();
        chat.event("c1", &json!({"t": "row", "door": "response", "uuid": "R2", "type": "assistant", "content": [{"type": "text", "text": "Hello world."}]}), &t).unwrap();
        chat.event("c1", &json!({"t": "chunk", "turn": "T", "step": 0, "index": 2, "kind": "tool", "id": "toolu_1", "name": "Bash"}), &t).unwrap();
        chat.event("c1", &json!({"t": "row", "door": "response", "uuid": "R3", "type": "assistant",
            "content": [{"type": "tool_use", "id": "toolu_1", "name": "Bash", "input": {"command": "ls"}}]}), &t).unwrap();
        // What the tool kept comes before the result's row; either way round
        // the item ends with both.
        chat.event("c1", &json!({"t": "done", "id": "toolu_1", "tool": "Bash", "result": {"stdout": "a\nb", "stderr": ""}}), &t).unwrap();
        chat.event("c1", &json!({"t": "row", "door": "tool-result", "uuid": "R4", "type": "user",
            "content": [{"type": "tool_result", "tool_use_id": "toolu_1", "content": "a\nb", "is_error": false}]}), &t).unwrap();
        chat.event("c1", &json!({"t": "turn", "phase": "end", "turn": "T", "ms": 4200, "aborted": false}), &t).unwrap();
        let evs = lines(&buffer);
        // Partial words, then the same items whole, under the ids they came by.
        let partials: Vec<&Item> = evs.iter().filter_map(|ev| match ev { Ev::Item { item, partial: true } => Some(item), _ => None }).collect();
        assert!(partials.iter().any(|i| matches!(i, Item::Thinking { id, .. } if id == "w-T-0-0")));
        assert!(partials.iter().any(|i| matches!(i, Item::Text { id, text } if id == "w-T-0-1" && text == "Hello world.")));
        let whole: Vec<&Item> = evs.iter().filter_map(|ev| match ev { Ev::Item { item, partial: false } => Some(item), _ => None }).collect();
        assert!(matches!(whole[0], Item::Thinking { id, text } if id == "w-T-0-0" && text == "Thinking hard."));
        assert!(matches!(whole[1], Item::Text { id, text } if id == "w-T-0-1" && text == "Hello world."));
        assert!(matches!(whole[2], Item::Command { id, status: Status::Running, .. } if id == "toolu_1"));
        assert!(matches!(whole[3], Item::Command { id, status: Status::Done, output, .. } if id == "toolu_1" && output == "a\nb"));
        assert!(evs.iter().any(|ev| matches!(ev, Ev::Worked { after, seconds: 4, .. } if after == "toolu_1")));
        assert!(matches!(evs.last(), Some(Ev::State { state: AgentState::Idle, .. })));
        assert!(evs.iter().any(|ev| matches!(ev, Ev::History { reset: true, .. })));
    }

    #[test]
    fn a_picture_read_shows_under_its_line() {
        let (mut chat, buffer, _link) = chat();
        let t = target(1, None);
        chat.event("c1", &json!({"t": "hello", "mod": 2, "cwd": "/w"}), &t).unwrap();
        for (id, file) in [("toolu_p", "/w/shot.png"), ("toolu_t", "/w/notes.txt")] {
            chat.event("c1", &json!({"t": "row", "door": "response", "uuid": format!("R-{id}"), "type": "assistant",
                "content": [{"type": "tool_use", "id": id, "name": "Read", "input": {"file_path": file}}]}), &t).unwrap();
            chat.event("c1", &json!({"t": "done", "id": id, "tool": "Read", "result": {"type": "image"}}), &t).unwrap();
            chat.event("c1", &json!({"t": "row", "door": "tool-result", "uuid": format!("U-{id}"), "type": "user",
                "content": [{"type": "tool_result", "tool_use_id": id, "content": []}], "pictures": []}), &t).unwrap();
        }
        let done: Vec<(String, Vec<Option<String>>)> = lines(&buffer)
            .into_iter()
            .filter_map(|ev| match ev {
                Ev::Item { item: Item::Command { id, status: Status::Done, pictures, .. }, .. } => {
                    Some((id, pictures.into_iter().map(|p| p.path).collect()))
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            done,
            [("toolu_p".to_string(), vec![Some("/w/shot.png".to_string())]), ("toolu_t".to_string(), vec![])]
        );
    }

    #[test]
    fn a_question_is_answered_with_its_picks() {
        let (mut chat, buffer, link) = chat();
        let t = target(1, None);
        chat.event("c1", &json!({"t": "hello", "mod": 1}), &t).unwrap();
        chat.event("c1", &json!({"t": "ask", "key": "toolu_9", "tool": "AskUserQuestion", "held": true, "input": {"questions": [
            {"question": "Which color?", "header": "Color", "multiSelect": false,
             "options": [{"label": "Red", "description": "warm"}, {"label": "Blue", "description": "cool"}]}]}}), &t).unwrap();
        let ask = lines(&buffer).into_iter().find_map(|ev| match ev { Ev::Ask { ask } => Some(ask), _ => None }).unwrap();
        assert_eq!(ask.kind, AskKind::Question);
        assert_eq!(ask.questions[0].options[1].label, "Blue");
        assert_eq!(ask.choices, [Choice::Accept, Choice::Decline]);
        // Not every question answered: refused, nothing handed on.
        chat.op(Op::Answer { ask: "toolu_9".into(), choice: Choice::Accept, answers: vec![] }).unwrap();
        assert!(lock(&link).answers.is_empty());
        chat.op(Op::Answer { ask: "toolu_9".into(), choice: Choice::Accept, answers: vec!["Blue".into()] }).unwrap();
        assert_eq!(take_answer(&link, "toolu_9", Duration::ZERO), json!({"answer": {"behavior": "allow", "answers": {"Which color?": "Blue"}}}));
        chat.event("c1", &json!({"t": "resolved", "key": "toolu_9"}), &t).unwrap();
        assert!(lines(&buffer).iter().any(|ev| matches!(ev, Ev::Resolved { id } if id == "toolu_9")));
        assert!(chat.asks.is_empty());
    }

    #[test]
    fn a_prompt_only_the_terminal_holds_is_answered_there() {
        let (mut chat, buffer, link) = chat();
        let t = target(1, None);
        chat.event("c1", &json!({"t": "hello", "mod": 1}), &t).unwrap();
        chat.event("c1", &json!({"t": "ask", "key": "toolu_3", "tool": "Bash", "held": false, "input": {"command": "rm -rf build"}}), &t).unwrap();
        let ask = lines(&buffer).into_iter().find_map(|ev| match ev { Ev::Ask { ask } => Some(ask), _ => None }).unwrap();
        assert!(ask.choices.is_empty(), "no choices: the client points at the terminal");
        chat.op(Op::Answer { ask: "toolu_3".into(), choice: Choice::Accept, answers: vec![] }).unwrap();
        assert!(lock(&link).answers.is_empty());
        // A question takes no "for this session".
        chat.event("c1", &json!({"t": "ask", "key": "q1", "tool": "AskUserQuestion", "held": true, "input": {"questions": [
            {"question": "Which?", "options": [{"label": "A"}, {"label": "B"}]}]}}), &t).unwrap();
        chat.op(Op::Answer { ask: "q1".into(), choice: Choice::AcceptForSession, answers: vec![] }).unwrap();
        assert!(lock(&link).answers.is_empty());
        // Held, with rules to remember: three choices; a session yes says so.
        chat.event("c1", &json!({"t": "ask", "key": "toolu_4", "tool": "Bash", "held": true, "remember": true, "input": {"command": "make"}}), &t).unwrap();
        chat.op(Op::Answer { ask: "toolu_4".into(), choice: Choice::AcceptForSession, answers: vec![] }).unwrap();
        assert_eq!(take_answer(&link, "toolu_4", Duration::ZERO)["answer"], json!({"behavior": "allow", "session": true}));
    }

    #[test]
    fn what_waits_is_asked_again_after_history_and_older_rows_are_read_on_ask() {
        let (mut chat, buffer, link) = chat();
        let t = target(1, None);
        let row = |uuid: &str, text: &str| json!({"role": "user", "uuid": uuid, "text": text, "tools": [], "results": 0});
        chat.event("c1", &json!({"t": "hello", "mod": 1}), &t).unwrap();
        chat.event("c1", &json!({"t": "ask", "key": "k1", "tool": "Bash", "held": true, "input": {"command": "make"}}), &t).unwrap();
        chat.event("c1", &json!({"t": "rows", "asked": 2, "rows": [row("u3", "three"), row("u4", "four")]}), &t).unwrap();
        let evs = lines(&buffer);
        let history = evs.iter().position(|ev| matches!(ev, Ev::History { reset: true, .. })).unwrap();
        assert!(matches!(&evs[history], Ev::History { more: true, .. }), "as many rows as asked: there may be more");
        assert!(evs[history..].iter().any(|ev| matches!(ev, Ev::Ask { ask } if ask.id == "k1")));
        // Older: the session is read further back, and what the client
        // holds already is left out.
        take_ops(&link, "c1", 0, Duration::ZERO);
        chat.op(Op::Older).unwrap();
        let ops = take_ops(&link, "c1", 99, Duration::ZERO).unwrap();
        assert_eq!(ops[0]["op"], "history");
        assert_eq!(ops[0]["last"], 8);
        buffer.lock().unwrap().clear();
        chat.event("c1", &json!({"t": "rows", "asked": 8, "rows": [row("u1", "one"), row("u2", "two"), row("u3", "three"), row("u4", "four")]}), &t).unwrap();
        match &lines(&buffer)[..] {
            [Ev::History { items, more: false, reset: false }] => {
                assert_eq!(items.iter().map(Item::id).collect::<Vec<_>>(), ["u1", "u2"]);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn what_is_said_here_shows_at_once_and_keeps_its_id() {
        let (mut chat, buffer, link) = chat();
        let t = target(1, None);
        chat.event("c1", &json!({"t": "hello", "mod": 1}), &t).unwrap();
        take_ops(&link, "c1", 0, Duration::ZERO);
        let picture = Attachment {
            name: "a.png".into(),
            image: true,
            path: Some("/h/ThinkTerm_Uploads/a.png".into()),
            thumb: None,
        };
        chat.op(Op::Send { text: "look".into(), attachments: vec![picture] }).unwrap();
        let ops = take_ops(&link, "c1", 1, Duration::ZERO).unwrap();
        assert_eq!(ops[0], json!({"op": "send", "seq": 2, "text": "look", "pictures": ["/h/ThinkTerm_Uploads/a.png"], "files": []}));
        let shown = lines(&buffer).into_iter().find_map(|ev| match ev { Ev::Item { item: Item::User { id, text, attachments }, .. } => Some((id, text, attachments)), _ => None }).unwrap();
        assert!(shown.0.starts_with("sent-") && shown.1 == "look" && shown.2.len() == 1);
        // A prompt typed in the terminal meanwhile (its content one string)
        // shows under its own id and takes nothing of what waits.
        buffer.lock().unwrap().clear();
        chat.event("c1", &json!({"t": "row", "door": "prompt", "uuid": "T1", "type": "user", "ours": false,
            "content": "typed there"}), &t).unwrap();
        assert!(matches!(&lines(&buffer)[..], [Ev::Item { item: Item::User { id, text, .. }, .. }] if id == "T1" && text == "typed there"));
        // Taken: the row is kept under the id it showed by.
        buffer.lock().unwrap().clear();
        chat.event("c1", &json!({"t": "row", "door": "prompt", "uuid": "U9", "type": "user", "ours": true,
            "content": [{"type": "text", "text": "look\n/h/ThinkTerm_Uploads/a.png"}]}), &t).unwrap();
        assert!(matches!(&lines(&buffer)[..], [Ev::Item { item: Item::User { id, .. }, .. }] if *id == shown.0));
        // A command goes as typed, and shows once Claude runs it.
        buffer.lock().unwrap().clear();
        chat.op(Op::Send { text: "/compact".into(), attachments: vec![] }).unwrap();
        assert!(lines(&buffer).is_empty());
        chat.op(Op::Send { text: "/model".into(), attachments: vec![] }).unwrap();
        assert!(matches!(&lines(&buffer)[..], [Ev::Refused { .. }]));
    }

    #[test]
    fn models_are_the_model_rows_choices() {
        assert_eq!(model_name("default", Some("claude-opus-5-5")), "Default (Opus 5.5)");
        assert_eq!(model_name("sonnet[1m]", None), "Sonnet (1M)");
        assert_eq!(model_name("opusplan", None), "Opus Plan");
        assert_eq!(pretty_model("claude-haiku-5-5-20261101"), "Haiku 5.5");
        let (mut chat, buffer, _) = chat();
        chat.event("c1", &json!({"t": "models", "options": ["default", "sonnet", "haiku"], "value": "Default (recommended)",
            "model": "claude-opus-5-5", "effort": "high"}), &target(1, None)).unwrap();
        let models = lines(&buffer).into_iter().find_map(|ev| match ev { Ev::Models { current, effort, models } => Some((current, effort, models)), _ => None }).unwrap();
        assert_eq!(models.0.as_deref(), Some("default"));
        assert_eq!(models.1.as_deref(), Some("high"));
        assert_eq!(models.2[0].name, "Default (Opus 5.5)");
        assert!(models.2[2].efforts.is_empty(), "haiku takes no effort");
    }

    #[test]
    fn commands_the_terminal_alone_shows_are_left_out() {
        let (mut chat, buffer, _) = chat();
        chat.event("c1", &json!({"t": "commands", "commands": [
            {"name": "compact", "description": "Free up context"}, {"name": "config", "description": "Open settings"},
            {"name": "__remote-workflow", "description": "x"}, {"name": "review", "description": "Review"}]}), &target(1, None)).unwrap();
        let names: Vec<String> = lines(&buffer).into_iter().find_map(|ev| match ev { Ev::Commands { commands } => Some(commands), _ => None })
            .unwrap().into_iter().map(|c| c.name).collect();
        assert_eq!(names, ["/compact", "/review"]);
    }
}
