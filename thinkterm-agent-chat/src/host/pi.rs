//! Pi: ThinkTerm's extension, inside the Pi that runs in the pane, listens
//! on a Unix socket and leaves a note saying where (one per Pi process, in
//! the chat folder). The bridge connects and passes lines both ways: the
//! extension speaks this crate's protocol itself, so nothing is translated
//! here. Without the extension there is nothing to connect to: the bridge
//! puts it in as a chat opens, and an agent started before then is offered
//! to take it in at a tap (its reload command, typed into its pane).

use super::{read_line, read_ops, same_path, AgentHere, Input, Out, PaneControl, Target};
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

/// An agent ThinkTerm talks to through an extension of its own: what it
/// is called. Where the extension goes is its own folder (`agent_dir`);
/// how a running one takes it in anew, `crate::reload_command`.
struct Harness {
    agent: &'static str,
    name: &'static str,
}

impl Harness {
    fn reload(&self) -> &'static str {
        crate::reload_command(self.agent)
    }
}

/// Pi, and OMP (Oh My Pi), Pi's fork, which runs the same extension from
/// a folder of its own.
const HARNESSES: [Harness; 2] = [Harness { agent: "pi", name: "Pi" }, Harness { agent: "omp", name: "OMP" }];

fn harness(agent: &str) -> &'static Harness {
    HARNESSES.iter().find(|h| h.agent == agent).unwrap_or(&HARNESSES[0])
}

/// The agent's name, for the detail lines (in English).
fn named(agent: &str) -> &'static str {
    harness(agent).name
}

/// What Pi and OMP run as: Node or Bun, under their own names or not.
const PROGRAMS: &[&str] = &["pi", "omp", "node", "bun"];

/// The agent's own folder: Pi's, or that of OMP (Oh My Pi), the fork of
/// Pi that runs the same extensions from a folder of its own. Both take
/// `PI_CODING_AGENT_DIR` in place of it: as the agent in the pane has it,
/// where that can be seen (Linux), else as the bridge has it.
pub fn agent_dir(target: &Target) -> PathBuf {
    agent_dir_in(&home(), target)
}

fn agent_dir_in(home: &Path, target: &Target) -> PathBuf {
    let set = super::pane_env(target, PROGRAMS, "PI_CODING_AGENT_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("PI_CODING_AGENT_DIR").map(PathBuf::from));
    match set.filter(|dir| !dir.as_os_str().is_empty()) {
        Some(dir) => dir,
        None if target.agent == "omp" => home.join(".omp").join("agent"),
        None => home.join(".pi").join("agent"),
    }
}

pub fn extension_path(target: &Target) -> PathBuf {
    agent_dir(target).join("extensions").join(EXTENSION_FILE)
}

/// Whether ThinkTerm's extension is in the agent's folder, of any version.
pub fn installed(target: &Target) -> bool {
    std::fs::read_to_string(extension_path(target)).is_ok_and(|text| text.starts_with(MARK))
}

/// Puts the extension into the agent's folder, or brings an older one of
/// ours up to date; one as new or newer is left as it is. The agent takes
/// it in when it next starts, or on its reload command.
pub fn install(target: &Target) -> Result<PathBuf> {
    install_in(&home(), target)
}

/// Readies every agent that runs the extension as one chat opens: the
/// target's, and the others' (Pi, OMP) whose folders are there. Only the
/// target's failing is said.
fn ensure_in(home: &Path, target: &Target) -> Result<PathBuf> {
    let own = install_in(home, target);
    for other in HARNESSES.iter().filter(|h| h.agent != target.agent) {
        let other = Target {
            agent: other.agent.into(),
            ..target.clone()
        };
        if agent_dir_in(home, &other) != agent_dir_in(home, target) {
            let _ = install_in(home, &other);
        }
    }
    own
}

fn install_in(home: &Path, target: &Target) -> Result<PathBuf> {
    let dir = agent_dir_in(home, target);
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
        // A newer one of ours is kept; one as new but not as written (a
        // copy changed by hand, a build that did not raise the version) is
        // put right.
        if version_of(&text) > version_of(EXTENSION) || text == EXTENSION {
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

/// The note an extension leaves.
#[derive(Debug, Deserialize)]
struct Note {
    pid: u32,
    pane: u64,
    #[serde(default)]
    mux_socket: Option<String>,
    socket: PathBuf,
    /// The extension's `EXTENSION_VERSION`; 0 for one from before it said.
    #[serde(default)]
    version: u32,
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

pub fn run(target: &Target, input: impl BufRead + Send + 'static, out: Out, pane: Option<Box<dyn PaneControl>>) -> Result<()> {
    // Put in where it is not, brought up to date where it is older: an
    // agent started from now on has it.
    let _ = ensure_in(&home(), target);
    let pane = pane.as_deref();
    let (tx, rx) = mpsc::channel::<Input<Piece>>();
    read_ops(input, tx.clone(), out.clone(), super::Place::new(target.cwd.clone()));
    let note = match find(&chat_dir(), target) {
        Some(note) => note,
        None => match offer(target, pane, &rx, &out)? {
            Some(note) => note,
            None => return Ok(()),
        },
    };
    relay(target, note, tx, rx, out, pane)
}

/// Has the agent in the pane take its extensions in: its reload command,
/// typed as a person would -- only while it rests, and only into an empty
/// field, as the command would be added to what is typed there.
fn load(target: &Target, pane: &dyn PaneControl) -> std::result::Result<(), String> {
    let h = harness(&target.agent);
    match pane.agent() {
        // What is left of its screen is not it: nothing is typed into what
        // runs there now.
        AgentHere::Gone => return Err(format!("{} is not running in this pane any more", h.name)),
        AgentHere::Working => return Err(format!("{} is working: load it once it rests", h.name)),
        AgentHere::Resting | AgentHere::Unknown => {}
    }
    match pane.screen().as_deref().and_then(field_empty) {
        Some(true) => pane.type_line(h.reload()).map_err(|err| format!("{err:#}")),
        Some(false) => Err(format!("something is typed in {}'s field: send or clear it, then load", h.name)),
        None => Err(format!("type {} in {}", h.reload(), h.name)),
    }
}

/// How long a load typed into the pane is waited on before another is
/// typed: a second tap while the agent reloads types nothing.
const LOADING: Duration = Duration::from_secs(10);

/// The version of ours in the agent's folder, which a reload would take in.
fn version_in_folder(target: &Target) -> u32 {
    std::fs::read_to_string(extension_path(target))
        .ok()
        .filter(|text| text.starts_with(MARK))
        .map_or(0, |text| version_of(&text))
}

/// Whether the agent's field is empty, as its screen shows it: the rows
/// between the last two rules (Pi draws its field between two lines of
/// `─`). None where the screen shows no such field.
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
    Some(rows[above + 1..below].iter().all(|row| row.trim().is_empty()))
}

/// How often a pane whose Pi has no extension running is looked at again.
const LOOK_AGAIN: Duration = Duration::from_millis(500);

/// No extension answers in this pane: say what would help, put it in when
/// the client asks, and go on to the chat once the agent runs it (on its
/// `/reload`, or started again).
fn offer(target: &Target, pane: Option<&dyn PaneControl>, rx: &mpsc::Receiver<Input<Piece>>, out: &Out) -> Result<Option<Note>> {
    let message = format!("no ThinkTerm extension is running in this {}", named(&target.agent));
    // In place but not taken in: what can have the agent take it in says so.
    let say = |out: &Out| -> std::io::Result<()> {
        let need = if installed(target) { Need::PiRestart } else { Need::PiExtension };
        out.send(&Ev::Error {
            need: Some(need),
            message: message.clone(),
            can_load: need == Need::PiRestart && pane.is_some(),
        })
    };
    say(out)?;
    // When a load was last typed.
    let mut loading: Option<std::time::Instant> = None;
    loop {
        match rx.recv_timeout(LOOK_AGAIN) {
            Ok(Input::Op(Op::Install)) => match install(target) {
                // Where the bridge can load it, the card says so: a notice
                // to type the command would say otherwise.
                Ok(_) if pane.is_some() => say(out)?,
                Ok(_) => {
                    out.send(&Ev::Done { what: Done::Installed })?;
                    say(out)?;
                }
                Err(err) => out.refused(format!("{err:#}"))?,
            },
            // A second tap while the agent reloads types nothing more.
            Ok(Input::Op(Op::Reload)) if loading.is_some_and(|at| at.elapsed() < LOADING) => {}
            Ok(Input::Op(Op::Reload)) => {
                let loaded = match pane {
                    Some(pane) => load(target, pane),
                    None => Err(format!("type {} in {}", harness(&target.agent).reload(), named(&target.agent))),
                };
                match loaded {
                    Ok(()) => loading = Some(std::time::Instant::now()),
                    Err(why) => out.refused(why)?,
                }
            }
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
/// How long an agent asked from here to take its extension in is waited
/// for: OMP's `/restart` starts its whole process again.
const RELOAD_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

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
fn listen_again(target: &Target, serial: u64, tx: &mpsc::Sender<Input<Piece>>, wait: Duration) -> Option<(UnixStream, u32)> {
    let until = std::time::Instant::now() + wait;
    while std::time::Instant::now() < until {
        if let Some(note) = find(&chat_dir(), target) {
            if let Ok(stream) = listen(&note, serial, named(&target.agent), tx) {
                return Some((stream, note.version));
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    None
}

fn relay(
    target: &Target,
    note: Note,
    tx: mpsc::Sender<Input<Piece>>,
    rx: mpsc::Receiver<Input<Piece>>,
    out: Out,
    pane: Option<&dyn PaneControl>,
) -> Result<()> {
    let mut serial = 0u64;
    // The running extension's version: one older than the one in the
    // agent's folder is said to be, after each greeting of its (which a
    // client takes as all is current).
    let mut version = note.version;
    // The running extension offers `/reload` itself (it lists it).
    let mut reloads_itself = false;
    // A reload was asked for from here: the agent is waited for longer.
    let mut reloading = false;
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
                let ev = serde_json::from_str::<serde_json::Value>(&line)
                    .ok()
                    .and_then(|v| v.get("ev").and_then(|ev| ev.as_str()).map(str::to_string));
                if from == serial && ev.is_some() {
                    out.raw(&super::thumb::fill_line(&line))?;
                    if ev.as_deref() == Some("hello") && version < version_in_folder(target) {
                        out.send(&Ev::Outdated)?;
                    }
                    if ev.as_deref() == Some("commands") {
                        reloads_itself = serde_json::from_str::<Ev>(&line)
                            .is_ok_and(|ev| matches!(ev, Ev::Commands { commands } if commands.iter().any(|c| c.name == "/reload")));
                    }
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
            // Nothing newer to take in (a second tap, after the first was
            // taken in): nothing is done.
            Input::Op(Op::Reload) if version >= version_in_folder(target) => None,
            // An extension that offers `/reload` runs it itself; for one that
            // does not (older, or OMP's), the command is typed for it.
            Input::Op(Op::Reload) if reloads_itself => {
                reloading = true;
                let line = "{\"op\":\"send\",\"text\":\"/reload\"}\n";
                writer.write_all(line.as_bytes()).err().map(|err| err.to_string())
            }
            Input::Op(Op::Reload) => {
                let loaded = match pane {
                    Some(pane) => load(target, pane),
                    None => Err(format!("type {} in {}", harness(&target.agent).reload(), named(&target.agent))),
                };
                match loaded {
                    Ok(()) => reloading = true,
                    Err(why) => out.refused(why)?,
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
            let wait = if std::mem::take(&mut reloading) { RELOAD_WAIT } else { SWITCH_WAIT };
            match listen_again(target, serial, &tx, wait) {
                Some((stream, now)) => {
                    writer = stream;
                    version = now;
                    reloads_itself = false;
                }
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
        install(&target("pi")).unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), EXTENSION);
        let newer = format!("{MARK}\nconst EXTENSION_VERSION = {};\n", version_of(EXTENSION) + 1);
        std::fs::write(&file, &newer).unwrap();
        install(&target("pi")).unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), newer);
        std::fs::write(&file, "// someone else's\n").unwrap();
        assert!(install(&target("pi")).is_err());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "// someone else's\n");
        assert!(!installed(&target("pi")));
        std::env::remove_var("PI_CODING_AGENT_DIR");

        // Without the variable, each agent has its own folder: a chat
        // opened with one readies the others whose folders are there, and
        // makes none that is not.
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join(".pi/agent")).unwrap();
        std::fs::create_dir_all(home.path().join(".omp/agent")).unwrap();
        ensure_in(home.path(), &target("pi")).unwrap();
        for agent in [".pi", ".omp"] {
            let file = home.path().join(agent).join("agent/extensions").join(EXTENSION_FILE);
            assert_eq!(std::fs::read_to_string(&file).unwrap(), EXTENSION, "{agent}");
        }
        let lone = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(lone.path().join(".omp/agent")).unwrap();
        ensure_in(lone.path(), &target("omp")).unwrap();
        assert!(!lone.path().join(".pi").exists(), "no folder made for an agent that is not there");
        assert!(ensure_in(lone.path(), &target("pi")).is_err(), "the target's own failing is said");
    }

    /// A pane as the tests have it: whether its agent rests, its screen,
    /// and what was typed into it.
    struct FakePane {
        agent: AgentHere,
        screen: Vec<&'static str>,
        typed: std::sync::Mutex<Vec<String>>,
    }

    impl PaneControl for FakePane {
        fn agent(&self) -> AgentHere {
            self.agent
        }
        fn screen(&self) -> Option<Vec<String>> {
            Some(self.screen.iter().map(|row| row.to_string()).collect())
        }
        fn type_line(&self, line: &str) -> anyhow::Result<()> {
            self.typed.lock().unwrap().push(line.to_string());
            Ok(())
        }
    }

    #[test]
    fn the_reload_is_typed_only_into_an_empty_field_of_an_agent_at_rest() {
        let rule = "──────────────────────────────";
        let screen = |field: &'static str| vec!["what it said", "", rule, field, rule, "~/work", "0.0%/32k"];
        let pane = |agent, field| FakePane {
            agent,
            screen: screen(field),
            typed: Default::default(),
        };
        let target = |agent: &str| Target {
            pane: 1,
            agent: agent.into(),
            cwd: None,
            mux_socket: None,
        };

        let resting = pane(AgentHere::Resting, "   ");
        assert_eq!(load(&target("pi"), &resting), Ok(()));
        assert_eq!(*resting.typed.lock().unwrap(), ["/reload"]);
        let omp = pane(AgentHere::Unknown, "");
        assert_eq!(load(&target("omp"), &omp), Ok(()));
        assert_eq!(*omp.typed.lock().unwrap(), ["/restart"]);

        let working = pane(AgentHere::Working, "");
        assert!(load(&target("pi"), &working).unwrap_err().contains("working"));
        let typing = pane(AgentHere::Resting, "half a sentence");
        assert!(load(&target("pi"), &typing).unwrap_err().contains("typed"));
        let unknown = FakePane {
            agent: AgentHere::Resting,
            screen: vec!["no field drawn here"],
            typed: Default::default(),
        };
        assert_eq!(load(&target("pi"), &unknown).unwrap_err(), "type /reload in Pi");
        // Pi quit; its last screen is still there, the shell's prompt under
        // it: nothing is typed into the shell.
        let gone = pane(AgentHere::Gone, "");
        assert!(load(&target("pi"), &gone).unwrap_err().contains("not running"));
        for pane in [working, typing, unknown, gone] {
            assert!(pane.typed.lock().unwrap().is_empty(), "nothing typed");
        }
    }
}
