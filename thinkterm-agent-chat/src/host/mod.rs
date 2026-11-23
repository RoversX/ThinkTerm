//! The bridge, on the machine the pane lives on (`thinkterm cli agent
//! chat`): it finds the conversation of the agent in a pane, speaks that
//! agent's own interface, and tells the client what happens in this
//! crate's lines, reading the client's from stdin and writing its own to
//! stdout.

mod codex;
mod http;
mod opencode;
pub mod pi;
mod rollout;
mod thumb;
mod ws;

use crate::{Ev, Need, Op, MAX_LINE};
use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::sync::{mpsc, Arc, Mutex};

/// The pane the client asked about, as the mux that owns it sees it.
#[derive(Clone, Debug, Default)]
pub struct Target {
    pub pane: u64,
    /// The agent the mux found in it: `codex`, `pi`.
    pub agent: String,
    /// The pane's working directory, where the agent was started.
    pub cwd: Option<PathBuf>,
    /// The mux the pane belongs to, as its `WEZTERM_UNIX_SOCKET` names it.
    pub mux_socket: Option<String>,
}

/// What the bridge may do to the agent's pane, through the mux that owns
/// it: given by the CLI, which holds the connection to the mux.
pub trait PaneControl: Send {
    /// How the agent the chat is with is doing in the pane.
    fn agent(&self) -> AgentHere;
    /// The rows on the pane's screen, as text; none when they cannot be
    /// read.
    fn screen(&self) -> Option<Vec<String>>;
    /// Types `line` into the pane and presses Enter.
    fn type_line(&self, line: &str) -> anyhow::Result<()>;
    /// The pane's title, as the program in it last set it (OpenCode puts
    /// its conversation's name there); none when it cannot be read.
    fn title(&self) -> Option<String> {
        None
    }
}

/// How the agent the chat is with is doing in its pane, as the mux sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentHere {
    Resting,
    Working,
    /// There, doing it does not say what.
    Unknown,
    /// Not in the pane any more (it quit; the pane holds a shell, or
    /// another program).
    Gone,
}

/// Most of a history sent in one line: well under what either side takes
/// in ([`MAX_LINE`]), however long the conversation.
pub(crate) const HISTORY_BUDGET: usize = 4 << 20;

/// `items` as two: the older ones left out, and the newest that fit in
/// [`HISTORY_BUDGET`] as JSON (one at least, whatever its size: each part
/// of an item is cut short where it is made, so that history always goes
/// on).
pub(crate) fn split_history(items: Vec<crate::Item>) -> (Vec<crate::Item>, Vec<crate::Item>) {
    let keep = newest_fitting(&items);
    let mut items = items;
    let newest = items.split_off(items.len() - keep);
    (items, newest)
}

/// How many of the newest `items` fit in [`HISTORY_BUDGET`]: one at least.
fn newest_fitting(items: &[crate::Item]) -> usize {
    let mut size = 0;
    let mut keep = 0;
    for item in items.iter().rev() {
        let len = serde_json::to_string(item).map_or(0, |line| line.len() + 1);
        if size + len > HISTORY_BUDGET && keep > 0 {
            break;
        }
        size += len;
        keep += 1;
    }
    keep
}

/// The newest of `items` that fit in [`HISTORY_BUDGET`] as JSON, and
/// whether any older ones were left out to fit.
#[cfg(test)]
pub(crate) fn fit_history(items: Vec<crate::Item>) -> (Vec<crate::Item>, bool) {
    let (held, page) = split_history(items);
    (page, !held.is_empty())
}

/// A process of the pane's, as `/proc` tells it (Linux).
#[derive(Clone, Debug, Default)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) struct PaneProc {
    pub pid: u32,
    pub argv: Vec<String>,
    environ: Vec<u8>,
}

impl PaneProc {
    /// One of its environment variables.
    pub fn var(&self, name: &str) -> Option<String> {
        let want = format!("{name}=");
        self.environ
            .split(|&b| b == 0)
            .find_map(|v| v.strip_prefix(want.as_bytes()))
            .map(|v| String::from_utf8_lossy(v).into_owned())
    }

    /// Its parent's pid.
    pub fn parent(&self) -> Option<u32> {
        let stat = std::fs::read_to_string(format!("/proc/{}/stat", self.pid)).ok()?;
        // After the name, which is in parentheses and may hold anything.
        let rest = &stat[stat.rfind(')')? + 1..];
        rest.split_whitespace().nth(1)?.parse().ok()
    }
}

/// The user's processes named one of `programs` that run in the target's
/// pane (they say so in `WEZTERM_PANE`, and in `WEZTERM_UNIX_SOCKET`
/// which mux the pane is of), where that can be read (Linux).
#[cfg(target_os = "linux")]
pub(crate) fn pane_procs(target: &Target, programs: &[&str]) -> Vec<PaneProc> {
    use std::os::unix::fs::MetadataExt;
    let pane = target.pane.to_string();
    let me = unsafe { libc::getuid() };
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return found;
    };
    for entry in entries.flatten() {
        let Some(pid) = entry.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) else {
            continue;
        };
        let dir = entry.path();
        // Only the user's own processes are looked at.
        if std::fs::metadata(&dir).map_or(true, |meta| meta.uid() != me) {
            continue;
        }
        let Ok(comm) = std::fs::read_to_string(dir.join("comm")) else {
            continue;
        };
        if !programs.contains(&comm.trim()) {
            continue;
        }
        let Ok(environ) = std::fs::read(dir.join("environ")) else {
            continue;
        };
        let proc = PaneProc {
            pid,
            argv: std::fs::read(dir.join("cmdline"))
                .map(|cmdline| {
                    cmdline
                        .split(|&b| b == 0)
                        .filter(|arg| !arg.is_empty())
                        .map(|arg| String::from_utf8_lossy(arg).into_owned())
                        .collect()
                })
                .unwrap_or_default(),
            environ,
        };
        if proc.var("WEZTERM_PANE").as_deref() != Some(pane.as_str()) {
            continue;
        }
        if let Some(mux) = target.mux_socket.as_deref() {
            if !proc.var("WEZTERM_UNIX_SOCKET").is_some_and(|theirs| same_path(&theirs, mux)) {
                continue;
            }
        }
        found.push(proc);
    }
    found
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn pane_procs(_target: &Target, _programs: &[&str]) -> Vec<PaneProc> {
    Vec::new()
}

/// A variable as the agent in the pane has it, where that can be read
/// (Linux): the bridge runs with ssh's environment, not the pane's, and
/// an agent's own folder (`CODEX_HOME`) may be set only in the pane's
/// shell. `None` when no process of the pane's names it.
pub(crate) fn pane_env(target: &Target, programs: &[&str], name: &str) -> Option<String> {
    // Another program of the pane (a server run in it with node) may not
    // have it set: the agent's own is looked for on.
    pane_procs(target, programs).iter().find_map(|proc| proc.var(name))
}

/// The ports a process listens on for TCP, where that can be read
/// (Linux).
pub(crate) fn listening_ports(pid: u32) -> Vec<u16> {
    let mut ports = Vec::new();
    for socket in tcp_sockets(pid).into_iter().filter(|s| s.listening) {
        if !ports.contains(&socket.local) {
            ports.push(socket.local);
        }
    }
    ports
}

/// The ports a process is connected to over TCP (the other end's), where
/// that can be read (Linux).
pub(crate) fn connected_ports(pid: u32) -> Vec<u16> {
    let mut ports = Vec::new();
    for socket in tcp_sockets(pid).into_iter().filter(|s| s.connected) {
        if !ports.contains(&socket.remote) {
            ports.push(socket.remote);
        }
    }
    ports
}

/// A TCP socket of a process's.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
struct TcpSocket {
    listening: bool,
    connected: bool,
    local: u16,
    remote: u16,
}

/// A process's TCP sockets: its own, found among all the kernel lists.
#[cfg(target_os = "linux")]
fn tcp_sockets(pid: u32) -> Vec<TcpSocket> {
    let mut inodes = std::collections::HashSet::new();
    if let Ok(fds) = std::fs::read_dir(format!("/proc/{pid}/fd")) {
        for fd in fds.flatten() {
            let Ok(link) = std::fs::read_link(fd.path()) else {
                continue;
            };
            let link = link.to_string_lossy();
            if let Some(inode) = link.strip_prefix("socket:[").and_then(|l| l.strip_suffix(']')) {
                inodes.insert(inode.to_string());
            }
        }
    }
    let port = |address: &str| address.rsplit(':').next().and_then(|p| u16::from_str_radix(p, 16).ok());
    let mut sockets = Vec::new();
    for table in ["tcp", "tcp6"] {
        let Ok(text) = std::fs::read_to_string(format!("/proc/{pid}/net/{table}")) else {
            continue;
        };
        for line in text.lines().skip(1) {
            let fields: Vec<&str> = line.split_whitespace().collect();
            // local address, remote, state (0A listening, 01 connected), …, inode.
            let (Some(local), Some(remote), Some(state), Some(inode)) = (fields.get(1), fields.get(2), fields.get(3), fields.get(9))
            else {
                continue;
            };
            if !inodes.contains(*inode) {
                continue;
            }
            if let (Some(local), Some(remote)) = (port(local), port(remote)) {
                sockets.push(TcpSocket {
                    listening: *state == "0A",
                    connected: *state == "01",
                    local,
                    remote,
                });
            }
        }
    }
    sockets
}

#[cfg(not(target_os = "linux"))]
fn tcp_sockets(_pid: u32) -> Vec<TcpSocket> {
    Vec::new()
}

/// The same file, however its path is written.
pub(crate) fn same_path(a: &str, b: &str) -> bool {
    let canonical = |p: &str| std::fs::canonicalize(p).unwrap_or_else(|_| std::path::PathBuf::from(p));
    a == b || canonical(a) == canonical(b)
}

/// Where the bridge's lines go, one JSON object a line, each flushed.
#[derive(Clone)]
pub struct Out(Arc<Mutex<Box<dyn Write + Send>>>);

impl Out {
    pub fn new(write: Box<dyn Write + Send>) -> Self {
        Self(Arc::new(Mutex::new(write)))
    }

    pub fn stdout() -> Self {
        Self::new(Box::new(io::stdout()))
    }

    pub fn send(&self, ev: &Ev) -> io::Result<()> {
        let line = serde_json::to_string(ev).map_err(io::Error::other)?;
        self.raw(&line)
    }

    /// A line made elsewhere (by the Pi extension), passed on as it is.
    fn raw(&self, line: &str) -> io::Result<()> {
        let mut out = self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        out.write_all(line.as_bytes())?;
        out.write_all(b"\n")?;
        out.flush()
    }

    pub fn error(&self, need: Option<Need>, message: impl Into<String>) -> io::Result<()> {
        self.send(&Ev::Error {
            need,
            message: message.into(),
            can_load: false,
        })
    }

    pub fn refused(&self, message: impl Into<String>) -> io::Result<()> {
        self.send(&Ev::Refused {
            message: message.into(),
        })
    }
}

/// What reaches a bridge's loop: the client's side and the agent's.
pub(crate) enum Input<T> {
    Op(Op),
    ClientGone,
    Agent(T),
    AgentGone(String),
}

/// Reads the client's lines on a thread of their own. A line that is not
/// an op is skipped; the end of input ends the chat.
pub(crate) fn read_ops<T: Send + 'static>(mut reader: impl BufRead + Send + 'static, tx: mpsc::Sender<Input<T>>) {
    std::thread::spawn(move || {
        while let Ok(Some(line)) = read_line(&mut reader) {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<Op>(&line) {
                Ok(op) => {
                    if tx.send(Input::Op(op)).is_err() {
                        return;
                    }
                }
                Err(err) => log::warn!("agent chat: not an op: {err}"),
            }
        }
        let _ = tx.send(Input::ClientGone);
    });
}

/// One line, without its newline; `None` at the end. A line longer than
/// [`MAX_LINE`] is an error.
pub(crate) fn read_line(reader: &mut impl BufRead) -> io::Result<Option<String>> {
    let mut line = Vec::new();
    loop {
        let (done, used) = {
            let buf = reader.fill_buf()?;
            if buf.is_empty() {
                return Ok((!line.is_empty()).then(|| String::from_utf8_lossy(&line).into_owned()));
            }
            match buf.iter().position(|&b| b == b'\n') {
                Some(at) => {
                    line.extend_from_slice(&buf[..at]);
                    (true, at + 1)
                }
                None => {
                    line.extend_from_slice(buf);
                    (false, buf.len())
                }
            }
        };
        reader.consume(used);
        if line.len() > MAX_LINE {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "line too long"));
        }
        if done {
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            return Ok(Some(String::from_utf8_lossy(&line).into_owned()));
        }
    }
}

/// Runs the chat for `target` until the client goes or the agent does.
/// `pane`, where the CLI can give it, lets the bridge type into the
/// agent's pane (to have it take its extension in).
pub fn run(
    target: Target,
    input: impl BufRead + Send + 'static,
    out: Out,
    pane: Option<Box<dyn PaneControl>>,
) -> anyhow::Result<()> {
    match target.agent.as_str() {
        "codex" => codex::run(&target, input, out),
        // OMP (Oh My Pi) is a fork of Pi: the same extension speaks for it.
        "pi" | "omp" => pi::run(&target, input, out, pane),
        "opencode" => opencode::run(&target, input, out, pane),
        other => Ok(out.error(Some(Need::Unsupported), format!("no chat for the agent `{other}`"))?),
    }
}

/// Unix seconds now.
pub(crate) fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The tail of `text`: at most `max` bytes, cut at a line where it can.
pub(crate) fn tail(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut start = text.len() - max;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    let cut = &text[start..];
    match cut.find('\n') {
        Some(at) if at + 1 < cut.len() => cut[at + 1..].to_string(),
        _ => cut.to_string(),
    }
}

/// The head of `text`: at most `max` bytes, on a character boundary.
pub(crate) fn head(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_are_read_whole_and_bounded() {
        let mut input = io::Cursor::new(b"one\r\ntwo\nthree".to_vec());
        assert_eq!(read_line(&mut input).unwrap().as_deref(), Some("one"));
        assert_eq!(read_line(&mut input).unwrap().as_deref(), Some("two"));
        assert_eq!(read_line(&mut input).unwrap().as_deref(), Some("three"));
        assert_eq!(read_line(&mut input).unwrap(), None);

        let mut long = io::Cursor::new(vec![b'x'; MAX_LINE + 2]);
        assert!(read_line(&mut long).is_err());
    }

    #[test]
    fn a_history_too_long_keeps_its_newest_items() {
        let item = |n: usize| crate::Item::Text {
            id: n.to_string(),
            text: "x".repeat(1 << 20),
        };
        let (kept, dropped) = fit_history((0..6).map(item).collect());
        assert!(dropped);
        assert_eq!(kept.iter().map(|i| i.id().to_string()).collect::<Vec<_>>(), ["3", "4", "5"]);
        let (kept, dropped) = fit_history(vec![item(0)]);
        assert!(!dropped && kept.len() == 1);
        // One larger than all the room is sent still, alone: history goes on.
        let huge = crate::Item::Text {
            id: "huge".into(),
            text: "x".repeat(HISTORY_BUDGET + 1),
        };
        let (kept, dropped) = fit_history(vec![item(0), huge]);
        assert!(dropped);
        assert_eq!(kept.iter().map(|i| i.id().to_string()).collect::<Vec<_>>(), ["huge"]);
    }

    #[test]
    fn tails_and_heads_keep_to_characters_and_lines() {
        assert_eq!(tail("short", 10), "short");
        assert_eq!(tail("first\nsecond\nthird", 12), "third");
        assert_eq!(head("汉字汉字", 4), "汉");
        assert_eq!(tail("汉字汉字", 4), "字");
    }
}
