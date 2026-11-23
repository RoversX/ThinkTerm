//! Pi: ThinkTerm's extension, inside the Pi that runs in the pane, listens
//! on a Unix socket and leaves a note saying where (one per Pi process, in
//! the chat folder). The bridge connects and passes lines both ways: the
//! extension speaks this crate's protocol itself, so nothing is translated
//! here. Without the extension there is nothing to connect to, and the
//! client is offered to put it in.

use super::{read_line, read_ops, same_path, Input, Out, Target};
use crate::{Done, Ev, Need, Op};
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;

/// The extension, as it is put into Pi.
pub const EXTENSION: &str = include_str!("../../pi/thinkterm-chat.ts");
/// Its file in Pi's extension folder.
pub const EXTENSION_FILE: &str = "thinkterm-chat.ts";
/// How every version of the extension begins: a file of that name that
/// does not is someone else's, and left alone.
const MARK: &str = "// ThinkTerm chat extension";

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}

/// Where the extensions leave their notes and sockets.
pub fn chat_dir() -> PathBuf {
    home().join(".local").join("state").join("thinkterm").join("agent-chat")
}

/// The agent's name, for the detail lines (in English).
fn named(agent: &str) -> &'static str {
    if agent == "omp" {
        "OMP"
    } else {
        "Pi"
    }
}

/// What Pi and OMP run as: Node or Bun, under their own names or not.
const PROGRAMS: &[&str] = &["pi", "omp", "node", "bun"];

/// The agent's own folder: Pi's, or that of OMP (Oh My Pi), the fork of
/// Pi that runs the same extensions from a folder of its own. Both take
/// `PI_CODING_AGENT_DIR` in place of it: as the agent in the pane has it,
/// where that can be seen (Linux), else as the bridge has it.
pub fn agent_dir(target: &Target) -> PathBuf {
    let set = super::pane_env(target, PROGRAMS, "PI_CODING_AGENT_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("PI_CODING_AGENT_DIR").map(PathBuf::from));
    match set.filter(|dir| !dir.as_os_str().is_empty()) {
        Some(dir) => dir,
        None if target.agent == "omp" => home().join(".omp").join("agent"),
        None => home().join(".pi").join("agent"),
    }
}

pub fn extension_path(target: &Target) -> PathBuf {
    agent_dir(target).join("extensions").join(EXTENSION_FILE)
}

/// Whether ThinkTerm's extension is in the agent's folder, of any version.
pub fn installed(target: &Target) -> bool {
    std::fs::read_to_string(extension_path(target)).is_ok_and(|text| text.starts_with(MARK))
}

/// Puts the extension into the agent's folder, or brings it up to date. The
/// agent takes it in when it next starts, or on `/reload`.
pub fn install(target: &Target) -> Result<PathBuf> {
    let dir = agent_dir(target);
    if !dir.is_dir() {
        bail!("{} is not there: is {} installed?", dir.display(), target.agent);
    }
    let extensions = dir.join("extensions");
    std::fs::create_dir_all(&extensions).with_context(|| format!("creating {}", extensions.display()))?;
    let file = extensions.join(EXTENSION_FILE);
    if let Ok(text) = std::fs::read_to_string(&file) {
        if !text.starts_with(MARK) {
            bail!("{} is someone else's file", file.display());
        }
        if text == EXTENSION {
            return Ok(file);
        }
    }
    let staged = extensions.join(format!(".{EXTENSION_FILE}.{}", std::process::id()));
    std::fs::write(&staged, EXTENSION).with_context(|| format!("writing {}", staged.display()))?;
    std::fs::rename(&staged, &file).with_context(|| format!("putting {} in place", file.display()))?;
    Ok(file)
}

/// The version an extension's text says it is (`EXTENSION_VERSION`); 0
/// for one from before versions.
fn version_of(text: &str) -> u32 {
    text.lines()
        .take(60)
        .find_map(|line| {
            let rest = line.trim().strip_prefix("const EXTENSION_VERSION = ")?;
            rest.trim_end_matches(';').trim().parse().ok()
        })
        .unwrap_or(0)
}

/// Puts this ThinkTerm's extension in place of an older one of its own, so
/// fixes reach a Pi that has it: taken in when the agent next starts or
/// reloads. Someone else's file, or a newer one, is left as it is.
fn refresh(target: &Target) {
    let Ok(text) = std::fs::read_to_string(extension_path(target)) else {
        return;
    };
    if text.starts_with(MARK) && version_of(&text) < version_of(EXTENSION) {
        let _ = install(target);
    }
}

/// The note an extension leaves.
#[derive(Debug, Deserialize)]
struct Note {
    pid: u32,
    pane: u64,
    #[serde(default)]
    mux_socket: Option<String>,
    socket: PathBuf,
}

/// The note of the Pi running in the pane, if one is alive there.
fn find(dir: &Path, target: &Target) -> Option<Note> {
    let mut found: Option<(std::time::SystemTime, Note)> = None;
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !(name.starts_with("pi-") && name.ends_with(".json")) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        let Ok(note) = serde_json::from_str::<Note>(&text) else {
            continue;
        };
        if note.pane != target.pane || !alive(note.pid) {
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

fn alive(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    // Signal 0 checks without sending; EPERM is a process that is there.
    let sent = unsafe { libc::kill(pid, 0) };
    sent == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

pub fn run(target: &Target, input: impl BufRead + Send + 'static, out: Out) -> Result<()> {
    refresh(target);
    let (tx, rx) = mpsc::channel::<Input<Piece>>();
    read_ops(input, tx.clone());
    let note = match find(&chat_dir(), target) {
        Some(note) => note,
        None => match offer(target, &rx, &out)? {
            Some(note) => note,
            None => return Ok(()),
        },
    };
    relay(target, note, tx, rx, out)
}

/// How often a pane whose Pi has no extension running is looked at again.
const LOOK_AGAIN: Duration = Duration::from_millis(500);

/// No extension answers in this pane: say what would help, put it in when
/// the client asks, and go on to the chat once the agent runs it (on its
/// `/reload`, or started again).
fn offer(target: &Target, rx: &mpsc::Receiver<Input<Piece>>, out: &Out) -> Result<Option<Note>> {
    let need = if installed(target) { Need::PiRestart } else { Need::PiExtension };
    out.error(Some(need), format!("no ThinkTerm extension is running in this {}", named(&target.agent)))?;
    loop {
        match rx.recv_timeout(LOOK_AGAIN) {
            Ok(Input::Op(Op::Install)) => match install(target) {
                Ok(_) => out.send(&Ev::Done { what: Done::Installed })?,
                Err(err) => out.refused(format!("{err:#}"))?,
            },
            Ok(Input::Op(_)) => out.refused(format!("{}'s chat is not running", named(&target.agent)))?,
            Ok(Input::ClientGone | Input::AgentGone(_)) | Err(RecvTimeoutError::Disconnected) => return Ok(None),
            Ok(Input::Agent(_)) | Err(RecvTimeoutError::Timeout) => {}
        }
        if let Some(note) = find(&chat_dir(), target) {
            return Ok(Some(note));
        }
    }
}

/// What comes from the extension, by which connection to it.
enum Piece {
    Line(u64, String),
    Gone(u64, String),
}

/// How long a Pi that switched sessions (`/new`, `/resume`, `/reload`) is
/// waited for to listen again: its extension stops and starts again with
/// the session.
const SWITCH_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

fn listen(note: &Note, serial: u64, who: &'static str, tx: &mpsc::Sender<Input<Piece>>) -> std::io::Result<UnixStream> {
    let stream = UnixStream::connect(&note.socket)?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let tx = tx.clone();
    std::thread::spawn(move || {
        let why = loop {
            match read_line(&mut reader) {
                Ok(Some(line)) => {
                    if tx.send(Input::Agent(Piece::Line(serial, line))).is_err() {
                        return;
                    }
                }
                Ok(None) => break format!("{who}'s chat ended"),
                Err(err) => break err.to_string(),
            }
        };
        let _ = tx.send(Input::Agent(Piece::Gone(serial, why)));
    });
    Ok(stream)
}

/// The Pi in the pane listening again after its session changed, if it
/// does within [`SWITCH_WAIT`].
fn listen_again(target: &Target, serial: u64, tx: &mpsc::Sender<Input<Piece>>) -> Option<UnixStream> {
    let until = std::time::Instant::now() + SWITCH_WAIT;
    while std::time::Instant::now() < until {
        if let Some(note) = find(&chat_dir(), target) {
            if let Ok(stream) = listen(&note, serial, named(&target.agent), tx) {
                return Some(stream);
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    None
}

fn relay(target: &Target, note: Note, tx: mpsc::Sender<Input<Piece>>, rx: mpsc::Receiver<Input<Piece>>, out: Out) -> Result<()> {
    let mut serial = 0u64;
    let mut writer = match listen(&note, serial, named(&target.agent), &tx) {
        Ok(stream) => stream,
        Err(err) => {
            out.error(Some(Need::PiRestart), format!("connecting to {}: {err}", note.socket.display()))?;
            return Ok(());
        }
    };
    while let Ok(input) = rx.recv() {
        let gone = match input {
            // Only what is a line of the protocol is passed on.
            Input::Agent(Piece::Line(from, line)) => {
                if from == serial && serde_json::from_str::<serde_json::Value>(&line).is_ok_and(|v| v.get("ev").is_some()) {
                    out.raw(&super::thumb::fill_line(&line))?;
                }
                None
            }
            Input::Agent(Piece::Gone(from, why)) => (from == serial).then_some(why),
            Input::Op(Op::Install) => {
                match install(target) {
                    Ok(_) => out.send(&Ev::Done { what: Done::Installed })?,
                    Err(err) => out.refused(format!("{err:#}"))?,
                }
                None
            }
            Input::Op(op) => {
                let mut line = serde_json::to_string(&op)?;
                line.push('\n');
                writer.write_all(line.as_bytes()).err().map(|err| err.to_string())
            }
            Input::ClientGone => break,
            Input::AgentGone(why) => Some(why),
        };
        if let Some(why) = gone {
            // A Pi that switched sessions listens again in a moment: the
            // chat goes on with the session it switched to.
            let _ = writer.shutdown(std::net::Shutdown::Both);
            serial += 1;
            match listen_again(target, serial, &tx) {
                Some(stream) => writer = stream,
                None => {
                    out.error(None, why)?;
                    return Ok(());
                }
            }
        }
    }
    let _ = writer.shutdown(std::net::Shutdown::Both);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_extension_carries_its_mark() {
        assert!(EXTENSION.starts_with(MARK));
    }

    #[test]
    fn a_note_is_found_for_its_pane_while_its_pi_lives() {
        let dir = tempfile::tempdir().unwrap();
        let me = std::process::id();
        let note = |pid: u32, pane: u64, mux: &str| {
            format!(r#"{{"agent":"pi","pid":{pid},"pane":{pane},"mux_socket":"{mux}","socket":"/x/{pid}.sock"}}"#)
        };
        std::fs::write(dir.path().join("pi-1.json"), note(me, 7, "/run/a")).unwrap();
        // Long gone, though its note was left behind.
        std::fs::write(dir.path().join("pi-2.json"), note(u32::MAX - 7, 8, "/run/a")).unwrap();
        std::fs::write(dir.path().join("other.json"), note(me, 9, "/run/a")).unwrap();

        let target = |pane: u64, mux: Option<&str>| Target {
            pane,
            agent: "pi".into(),
            cwd: None,
            mux_socket: mux.map(str::to_string),
        };
        assert_eq!(find(dir.path(), &target(7, None)).map(|n| n.pid), Some(me));
        assert_eq!(find(dir.path(), &target(7, Some("/run/a"))).map(|n| n.pid), Some(me));
        assert!(find(dir.path(), &target(7, Some("/run/b"))).is_none(), "another mux's pane 7");
        assert!(find(dir.path(), &target(8, None)).is_none(), "its Pi is gone");
        assert!(find(dir.path(), &target(9, None)).is_none(), "not a note's name");
    }

    #[test]
    fn install_puts_the_extension_in_and_leaves_others_alone() {
        let dir = tempfile::tempdir().unwrap();
        // The variable is the process's: this test is the only one to set it.
        std::env::set_var("PI_CODING_AGENT_DIR", dir.path());
        // A pane of a mux that is not there: no process of the person's
        // own can be taken for it, and their folder is never touched.
        let target = |agent: &str| Target {
            pane: u64::MAX - 1,
            agent: agent.into(),
            cwd: None,
            mux_socket: Some(dir.path().join("no-mux").display().to_string()),
        };
        let file = install(&target("pi")).unwrap();
        assert!(file.starts_with(dir.path()));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), EXTENSION);
        assert!(installed(&target("pi")) && installed(&target("omp")), "one folder for both when it is set");
        // An older one of ours is brought up to date; a newer one is kept.
        assert!(version_of(EXTENSION) >= 2);
        std::fs::write(&file, format!("{MARK}\nconst EXTENSION_VERSION = 1;\n")).unwrap();
        refresh(&target("pi"));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), EXTENSION);
        let newer = format!("{MARK}\nconst EXTENSION_VERSION = {};\n", version_of(EXTENSION) + 1);
        std::fs::write(&file, &newer).unwrap();
        refresh(&target("pi"));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), newer);
        std::fs::write(&file, "// someone else's\n").unwrap();
        refresh(&target("pi"));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "// someone else's\n");
        assert!(install(&target("pi")).is_err());
        assert!(!installed(&target("pi")));
        std::env::remove_var("PI_CODING_AGENT_DIR");
    }
}
