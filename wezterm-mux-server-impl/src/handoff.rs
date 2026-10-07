//! One mux server hands everything it runs to another process.
//!
//! Upgrading used to mean killing the server, and with it every shell and
//! program in every pane, and the scrollback. Now the new binary starts
//! with `--takeover`, connects to the running server's handoff socket
//! (`<socket>.handoff`), and receives each pane's pty as a file
//! descriptor, its process id, and what the old process had parsed out of
//! the pty as a `TerminalSnapshot`; then the windows and tabs, under the
//! same ids; then the listening socket itself. The new server also
//! inherits the old one's runtime server id, so a client that reconnects
//! finds the same server and rebinds its panes by id -- not a replacement
//! to be restored from a saved layout.
//!
//! Nothing here is irreversible until `Commit`. The old server pauses its
//! pane readers before snapshotting and keeps them paused; the new server
//! only holds what it receives -- descriptors unopened, snapshots decoded
//! -- and opens, builds and registers everything at `Commit`, so nothing
//! reads or writes a pty in two places. If either side fails before the
//! new server answers `Owned`, the old server's readers resume and it
//! keeps serving; the new server exits. After `Owned` the old server
//! exits without touching the layout files, which the new one owns now.
//!
//! Left behind: panes that hold a channel rather than a pty (ssh-domain
//! and tmux-domain panes) and panes whose process already exited. They
//! are pruned from the topology the new server gets, and end with the old
//! process. Also left behind, by design: bytes of a partial escape
//! sequence at the pause boundary, and output held by an unfinished
//! synchronized-output frame (the next frame redraws).

use anyhow::{anyhow, bail, Context};
use std::convert::TryFrom;
use thinkterm_proto::PaneNode;
use mux::domain::DomainId;
use mux::localpane::LocalPane;
use mux::pane::{Pane, PaneId};
use mux::tab::{Tab, TabId};
use mux::window::{Window, WindowId};
use mux::{Mux, PausedReader};
use passfd::FdPassingExt;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::os::unix::io::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use wezterm_term::{TerminalSize, TerminalSnapshot};

/// Bumped whenever the messages below change shape. Both sides say theirs
/// first; a mismatch is refused before anything is paused.
pub const HANDOFF_VERSION: u32 = 1;

/// How long either side waits for the other's next message.
const STEP_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the old server waits for `Owned` after `Commit`: that one step
/// is the successor restoring every terminal and registering everything,
/// which grows with the panes and their scrollback. Past it the old server
/// carries on -- with both reading the same ptys if the successor was
/// merely slow, which is why it is generous.
const OWNED_TIMEOUT: Duration = Duration::from_secs(120);
/// How long the successor waits for `Hello`: the old server is pausing
/// and snapshotting every pane first, up to `PAUSE_TIMEOUT` each.
const HELLO_TIMEOUT: Duration = Duration::from_secs(60);
/// How long the old server waits for one pane's reader to park and its
/// parser to catch up.
const PAUSE_TIMEOUT: Duration = Duration::from_secs(2);
/// A message larger than this is not a message.
const MAX_MESSAGE: usize = 512 * 1024 * 1024;

/// For the test of the failure path: the new server gives up when it
/// reaches the named step (`pane`, `topology`, `listener`, `commit`), and
/// the old server must be seen to carry on.
fn fail_at(step: &str) -> anyhow::Result<()> {
    if std::env::var("THINKTERM_HANDOFF_FAIL_AT").as_deref() == Ok(step) {
        bail!("THINKTERM_HANDOFF_FAIL_AT={step}: giving up here for the test");
    }
    Ok(())
}

#[derive(Serialize, Deserialize, Debug)]
enum HandoffMessage {
    /// New → old, first.
    Ready { version: u32 },
    /// Old → new. Followed by the pid-file descriptor when `pid_file`.
    Hello(Hello),
    /// Old → new. Followed by the pty descriptor.
    Pane(HandoffPane),
    /// Old → new.
    Topology(Vec<HandoffWindow>),
    /// Old → new. Followed by the listening socket's descriptor.
    Listener,
    /// Old → new: everything is sent.
    Commit,
    /// New → old, after each of the above except `Commit`.
    Ack,
    /// New → old, after `Commit`: the panes are registered and the
    /// listener is running.
    Owned,
    /// Either side, before it drops the connection.
    Refused(String),
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Hello {
    pub version: u32,
    pub runtime_server_id: String,
    pub next_pane_id: PaneId,
    pub next_tab_id: TabId,
    pub next_window_id: WindowId,
    pub next_stack_id: usize,
    pub pid_file: bool,
    /// Panes that stay with the old server; for the log.
    pub left_behind: Vec<PaneId>,
}

#[derive(Serialize, Deserialize, Debug)]
struct HandoffPane {
    pane_id: PaneId,
    description: String,
    pid: u32,
    tty_name: Option<String>,
    /// varbincode of the `TerminalSnapshot`, zstd-compressed.
    snapshot: Vec<u8>,
}

#[derive(Serialize, Deserialize, Debug)]
struct HandoffWindow {
    window_id: WindowId,
    workspace: String,
    title: String,
    active_tab: usize,
    tabs: Vec<HandoffTab>,
}

#[derive(Serialize, Deserialize, Debug)]
struct HandoffTab {
    tab_id: TabId,
    title: String,
    size: TerminalSize,
    tree: PaneNode,
}

/// Where a server listens for its successor, next to its socket.
pub fn handoff_socket_path(socket_path: &Path) -> PathBuf {
    let mut name = socket_path
        .file_name()
        .map(|name| name.to_os_string())
        .unwrap_or_default();
    name.push(".handoff");
    socket_path.with_file_name(name)
}

// ---------------------------------------------------------------- framing

fn write_message(stream: &mut UnixStream, message: &HandoffMessage) -> anyhow::Result<()> {
    let mut bytes = Vec::new();
    let mut encode = varbincode::Serializer::new(&mut bytes);
    message
        .serialize(&mut encode)
        .context("encoding a handoff message")?;
    let len = u32::try_from(bytes.len()).context("a handoff message is too large")?;
    stream.write_all(&len.to_le_bytes())?;
    stream.write_all(&bytes)?;
    stream.flush()?;
    Ok(())
}

fn read_message(stream: &mut UnixStream) -> anyhow::Result<HandoffMessage> {
    let mut len = [0u8; 4];
    stream
        .read_exact(&mut len)
        .context("reading the next handoff message")?;
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_MESSAGE {
        bail!("a handoff message claims {len} bytes");
    }
    let mut bytes = vec![0u8; len];
    stream.read_exact(&mut bytes)?;
    let mut reader = &bytes[..];
    let mut decode = varbincode::Deserializer::new(&mut reader);
    HandoffMessage::deserialize(&mut decode).context("decoding a handoff message")
}

fn expect(stream: &mut UnixStream, what: &str) -> anyhow::Result<HandoffMessage> {
    match read_message(stream)? {
        HandoffMessage::Refused(why) => bail!("the other side refused: {why}"),
        message => {
            if std::mem::discriminant(&message) != std::mem::discriminant(&placeholder(what)) {
                bail!("expected {what}, got {message:?}");
            }
            Ok(message)
        }
    }
}

/// A value of the variant named by `what`, for comparing discriminants.
fn placeholder(what: &str) -> HandoffMessage {
    match what {
        "Ack" => HandoffMessage::Ack,
        "Owned" => HandoffMessage::Owned,
        "Ready" => HandoffMessage::Ready { version: 0 },
        other => panic!("no placeholder for {}", other),
    }
}

fn send_fd(stream: &UnixStream, fd: RawFd) -> anyhow::Result<()> {
    stream
        .as_raw_fd()
        .send_fd(fd)
        .context("passing a file descriptor")
}

fn recv_owned_fd(stream: &UnixStream) -> anyhow::Result<OwnedFd> {
    let fd = stream
        .as_raw_fd()
        .recv_fd()
        .context("receiving a file descriptor")?;
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

const KITTY_PLAYBACK_TAG: &[u8] = b"KGPA\x01";
const KITTY_VIRTUAL_TAG: &[u8] = b"KGPA\x02";
const KITTY_NUMBERS_TAG: &[u8] = b"KGPA\x03";
const KITTY_PLACEMENTS_TAG: &[u8] = b"KGPA\x04";
const KITTY_RELATIVES_TAG: &[u8] = b"KGPA\x05";

fn encode_snapshot(snapshot: &TerminalSnapshot, graphics: &wezterm_term::KittyGraphicsSnapshot) -> anyhow::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    snapshot.serialize(&mut varbincode::Serializer::new(&mut bytes))
        .context("encoding a terminal snapshot")?;
    // The predecessor's decoder stops after TerminalSnapshot. Keep that
    // prefix identical so an older successor can still adopt the terminal.
    let tag = if graphics.relatives.is_some() {
        Some(KITTY_RELATIVES_TAG)
    } else if graphics.placements.is_some() {
        Some(KITTY_PLACEMENTS_TAG)
    } else if !graphics.image_numbers.is_empty() {
        Some(KITTY_NUMBERS_TAG)
    } else if !graphics.virtual_images.is_empty() {
        Some(KITTY_VIRTUAL_TAG)
    } else if !graphics.playback.selections.is_empty() {
        Some(KITTY_PLAYBACK_TAG)
    } else {
        None
    };
    if let Some(tag) = tag {
        bytes.extend_from_slice(tag);
        graphics.playback.serialize(&mut varbincode::Serializer::new(&mut bytes))
            .context("encoding Kitty playback")?;
        if tag != KITTY_PLAYBACK_TAG {
            graphics.virtual_images.serialize(&mut varbincode::Serializer::new(&mut bytes))
                .context("encoding Kitty virtual placements")?;
        }
        if tag == KITTY_NUMBERS_TAG || tag == KITTY_PLACEMENTS_TAG || tag == KITTY_RELATIVES_TAG {
            graphics.image_numbers.serialize(&mut varbincode::Serializer::new(&mut bytes))
                .context("encoding Kitty image numbers")?;
        }
        if let Some(relatives) = &graphics.relatives {
            graphics.placements.serialize(&mut varbincode::Serializer::new(&mut bytes))
                .context("encoding Kitty placement identities")?;
            relatives.serialize(&mut varbincode::Serializer::new(&mut bytes))
                .context("encoding Kitty relative placements")?;
        } else if let Some(placements) = &graphics.placements {
            placements.serialize(&mut varbincode::Serializer::new(&mut bytes))
                .context("encoding Kitty placement identities")?;
        }
    }
    zstd::encode_all(&bytes[..], 3).context("compressing a terminal snapshot")
}

fn decode_snapshot(bytes: &[u8]) -> anyhow::Result<(TerminalSnapshot, Option<wezterm_term::KittyGraphicsSnapshot>)> {
    let bytes = zstd::decode_all(bytes).context("decompressing a terminal snapshot")?;
    let mut reader = &bytes[..];
    let snapshot = TerminalSnapshot::deserialize(&mut varbincode::Deserializer::new(&mut reader))
        .context("decoding a terminal snapshot")?;
    let graphics = if reader.is_empty() {
        None
    } else {
        let has_relatives = reader.starts_with(KITTY_RELATIVES_TAG);
        let has_placements = has_relatives || reader.starts_with(KITTY_PLACEMENTS_TAG);
        let has_numbers = has_placements || reader.starts_with(KITTY_NUMBERS_TAG);
        let has_virtuals = has_numbers || reader.starts_with(KITTY_VIRTUAL_TAG);
        anyhow::ensure!(has_virtuals || reader.starts_with(KITTY_PLAYBACK_TAG), "unknown terminal snapshot extension");
        reader = &reader[KITTY_PLAYBACK_TAG.len()..];
        let playback = wezterm_term::kitty_animation::KittyPlaybackSnapshot::deserialize(
            &mut varbincode::Deserializer::new(&mut reader),
        ).context("decoding Kitty playback")?;
        let virtual_images = if has_virtuals {
            Vec::<wezterm_term::kitty_virtual::VirtualImage>::deserialize(
                &mut varbincode::Deserializer::new(&mut reader),
            ).context("decoding Kitty virtual placements")?
        } else { Vec::new() };
        let image_numbers = if has_numbers {
            Vec::<(u32, u32)>::deserialize(&mut varbincode::Deserializer::new(&mut reader))
                .context("decoding Kitty image numbers")?
        } else { Vec::new() };
        let placements = if has_relatives {
            Option::<wezterm_term::KittyPlacementSnapshot>::deserialize(&mut varbincode::Deserializer::new(&mut reader))
                .context("decoding Kitty placement identities")?
        } else if has_placements {
            Some(wezterm_term::KittyPlacementSnapshot::deserialize(&mut varbincode::Deserializer::new(&mut reader))
                .context("decoding Kitty placement identities")?)
        } else { None };
        let relatives = if has_relatives {
            Some(wezterm_term::kitty_relative::RelativeSnapshot::deserialize(&mut varbincode::Deserializer::new(&mut reader))
                .context("decoding Kitty relative placements")?)
        } else { None };
        anyhow::ensure!(reader.is_empty(), "trailing bytes after Kitty snapshot extension");
        Some(wezterm_term::KittyGraphicsSnapshot { playback, virtual_images, image_numbers, placements, relatives })
    };
    Ok((snapshot, graphics))
}

// ------------------------------------------------------- the old server

static PID_FILE_FD: AtomicI32 = AtomicI32::new(-1);
static REPORT_FD: AtomicI32 = AtomicI32::new(-1);

/// The pid file this daemon holds locked, to pass on with everything else.
pub fn remember_pid_file_fd(fd: RawFd) {
    PID_FILE_FD.store(fd, Ordering::SeqCst);
}

/// The pipe back to the shell that ran `--daemonize --takeover`.
pub fn remember_report_fd(fd: RawFd) {
    REPORT_FD.store(fd, Ordering::SeqCst);
}

/// Tell the shell that ran `--daemonize --takeover` how it went, once;
/// nothing happens when it was not asked. The line is `ok` or
/// `error: <why>`, and the pipe closes with it.
pub fn report_takeover(outcome: Result<(), &str>) {
    let fd = REPORT_FD.swap(-1, Ordering::SeqCst);
    if fd < 0 {
        return;
    }
    let mut pipe = unsafe { std::fs::File::from_raw_fd(fd) };
    let line = match outcome {
        Ok(()) => "ok\n".to_string(),
        Err(why) => format!("error: {}\n", why.replace('\n', " ")),
    };
    pipe.write_all(line.as_bytes()).ok();
}

/// Listen for a successor at `<socket_path>.handoff`. One takeover at a
/// time; a failed one leaves this server as it was.
pub fn spawn_handoff_listener(socket_path: PathBuf) -> anyhow::Result<()> {
    let path = handoff_socket_path(&socket_path);
    crate::local::claim_socket_path_for_server(
        &path,
        &config::configuration().daemon_options.pid_file(),
    )?;
    let listener =
        UnixListener::bind(&path).with_context(|| format!("binding {}", path.display()))?;
    config::set_sticky_bit(&path);
    std::thread::Builder::new()
        .name("mux-handoff".to_string())
        .spawn(move || {
            for stream in listener.incoming() {
                match stream {
                    Ok(stream) => serve(stream, &socket_path),
                    Err(err) => {
                        log::error!("handoff listener: accept failed: {err:#}");
                        return;
                    }
                }
            }
        })
        .context("spawning the handoff listener thread")?;
    Ok(())
}

fn serve(mut stream: UnixStream, socket_path: &Path) {
    stream.set_read_timeout(Some(STEP_TIMEOUT)).ok();
    stream.set_write_timeout(Some(STEP_TIMEOUT)).ok();
    let started = Instant::now();
    match serve_inner(&mut stream, socket_path) {
        Ok(()) => {
            log::info!(
                "handed every pane to the successor in {:?}; this server exits",
                started.elapsed()
            );
            std::process::exit(0);
        }
        Err(err) => {
            log::error!("takeover failed after {:?}; this server keeps serving: {err:#}", started.elapsed());
            write_message(&mut stream, &HandoffMessage::Refused(format!("{err:#}"))).ok();
        }
    }
}

/// Everything captured on the main thread, with the readers still paused.
/// Dropping it before the successor owns the panes resumes them.
struct Capture {
    hello: Hello,
    panes: Vec<(HandoffPane, OwnedFd)>,
    topology: Vec<HandoffWindow>,
    paused: Vec<PausedReader>,
}

fn serve_inner(stream: &mut UnixStream, socket_path: &Path) -> anyhow::Result<()> {
    let HandoffMessage::Ready { version } = expect(stream, "Ready")? else {
        unreachable!()
    };
    if version != HANDOFF_VERSION {
        bail!("the successor speaks handoff version {version}, this server speaks {HANDOFF_VERSION}");
    }
    let unix_domains = config::configuration().unix_domains.len();
    if unix_domains > 1 {
        bail!(
            "this server listens for {unix_domains} unix domains and a takeover carries one; \
             stop and restart it instead"
        );
    }
    let listener_fd = crate::local::listener_fd_for(socket_path)
        .ok_or_else(|| anyhow!("no listening socket registered for {}", socket_path.display()))?;
    let pid_file_fd = PID_FILE_FD.load(Ordering::SeqCst);

    let capture = block_on_main(move || capture(pid_file_fd >= 0))?;
    log::info!(
        "handing {} panes, {} windows to the successor ({} left behind)",
        capture.panes.len(),
        capture.topology.len(),
        capture.hello.left_behind.len()
    );

    write_message(stream, &HandoffMessage::Hello(capture.hello.clone()))?;
    if pid_file_fd >= 0 {
        send_fd(stream, pid_file_fd)?;
    }
    expect(stream, "Ack")?;

    for (pane, fd) in &capture.panes {
        let pane_id = pane.pane_id;
        write_message(stream, &HandoffMessage::Pane(clone_pane(pane)))
            .with_context(|| format!("sending pane {pane_id}"))?;
        send_fd(stream, fd.as_raw_fd())
            .with_context(|| format!("sending pane {pane_id}'s pty"))?;
        expect(stream, "Ack").with_context(|| format!("pane {pane_id}"))?;
    }

    write_message(stream, &HandoffMessage::Topology(clone_topology(&capture.topology)))?;
    expect(stream, "Ack").context("topology")?;

    write_message(stream, &HandoffMessage::Listener)?;
    send_fd(stream, listener_fd).context("sending the listening socket")?;
    expect(stream, "Ack").context("listener")?;

    write_message(stream, &HandoffMessage::Commit)?;
    stream.set_read_timeout(Some(OWNED_TIMEOUT)).ok();
    expect(stream, "Owned")?;

    for paused in capture.paused {
        paused.keep();
    }
    Ok(())
}

fn clone_pane(pane: &HandoffPane) -> HandoffPane {
    HandoffPane {
        pane_id: pane.pane_id,
        description: pane.description.clone(),
        pid: pane.pid,
        tty_name: pane.tty_name.clone(),
        snapshot: pane.snapshot.clone(),
    }
}

fn clone_topology(windows: &[HandoffWindow]) -> Vec<HandoffWindow> {
    windows
        .iter()
        .map(|window| HandoffWindow {
            window_id: window.window_id,
            workspace: window.workspace.clone(),
            title: window.title.clone(),
            active_tab: window.active_tab,
            tabs: window
                .tabs
                .iter()
                .map(|tab| HandoffTab {
                    tab_id: tab.tab_id,
                    title: tab.title.clone(),
                    size: tab.size,
                    tree: clone_tree(&tab.tree),
                })
                .collect(),
        })
        .collect()
}

fn clone_tree(node: &PaneNode) -> PaneNode {
    match node {
        PaneNode::Empty => PaneNode::Empty,
        PaneNode::Leaf(entry) => PaneNode::Leaf(entry.clone()),
        PaneNode::Stack(stack) => PaneNode::Stack(stack.clone()),
        PaneNode::Split { left, right, node } => PaneNode::Split {
            left: Box::new(clone_tree(left)),
            right: Box::new(clone_tree(right)),
            node: node.clone(),
        },
    }
}

/// Run `f` on the main thread and wait for it here.
fn block_on_main<F, R>(f: F) -> R
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    smol::block_on(promise::spawn::spawn_into_main_thread(async move { f() }))
}

/// Pause every pane that can travel, snapshot it, and read the topology.
/// Runs on the main thread, where the mux is read.
fn capture(pid_file: bool) -> anyhow::Result<Capture> {
    let mux = Mux::get();
    let local_domain = mux.default_domain().domain_id();
    let mut paused = Vec::new();
    let mut panes = Vec::new();
    let mut carried = HashSet::new();
    let mut left_behind = Vec::new();

    for pane in mux.iter_panes() {
        let pane_id = pane.pane_id();
        let parts = match pane.downcast_ref::<LocalPane>() {
            Some(local) if pane.domain_id() == local_domain => local.handoff_parts(),
            _ => None,
        };
        let Some(parts) = parts else {
            left_behind.push(pane_id);
            continue;
        };
        let guard = mux::pause_pane_reader(pane_id, PAUSE_TIMEOUT)
            .with_context(|| format!("pausing pane {pane_id}"))?;
        let local = pane
            .downcast_ref::<LocalPane>()
            .expect("checked above");
        let (snapshot, graphics) = local.snapshot_terminal();
        paused.push(guard);
        panes.push((
            HandoffPane {
                pane_id,
                description: parts.description,
                pid: parts.pid,
                tty_name: parts.tty_name.map(|name| name.to_string_lossy().into_owned()),
                snapshot: encode_snapshot(&snapshot, &graphics)?,
            },
            parts.pty_fd,
        ));
        carried.insert(pane_id);
    }

    let mut topology = Vec::new();
    for window_id in mux.iter_windows() {
        let Some(window) = mux.get_window(window_id) else {
            continue;
        };
        let active_tab_id = window.get_active().map(|tab| tab.tab_id());
        let mut tabs = Vec::new();
        for tab in window.iter() {
            let tree = prune_tree(tab.codec_pane_tree(), &carried);
            if matches!(tree, PaneNode::Empty) {
                continue;
            }
            tabs.push(HandoffTab {
                tab_id: tab.tab_id(),
                title: tab.get_title(),
                size: tab.get_size(),
                tree,
            });
        }
        if tabs.is_empty() {
            continue;
        }
        let active_tab = active_tab_id
            .and_then(|id| tabs.iter().position(|tab| tab.tab_id == id))
            .unwrap_or(0);
        topology.push(HandoffWindow {
            window_id,
            workspace: window.get_workspace().to_string(),
            title: window.get_title().to_string(),
            active_tab,
            tabs,
        });
    }

    Ok(Capture {
        hello: Hello {
            version: HANDOFF_VERSION,
            runtime_server_id: mux.runtime_server_id().to_string(),
            next_pane_id: mux::pane::next_pane_id(),
            next_tab_id: mux::tab::next_tab_id(),
            next_window_id: mux::window::next_window_id(),
            next_stack_id: mux::tab::next_pane_stack_id(),
            pid_file,
            left_behind,
        },
        panes,
        topology,
        paused,
    })
}

/// The tree without the panes that stay behind: a stack keeps the rest of
/// its panes, a split with one empty side becomes the other side.
fn prune_tree(node: PaneNode, keep: &HashSet<PaneId>) -> PaneNode {
    match node {
        PaneNode::Empty => PaneNode::Empty,
        PaneNode::Leaf(entry) => {
            if keep.contains(&entry.pane_id) {
                PaneNode::Leaf(entry)
            } else {
                PaneNode::Empty
            }
        }
        PaneNode::Stack(mut stack) => {
            let active = stack.panes.get(stack.active).map(|entry| entry.pane_id);
            stack.panes.retain(|entry| keep.contains(&entry.pane_id));
            if stack.panes.is_empty() {
                return PaneNode::Empty;
            }
            stack.active = active
                .and_then(|id| stack.panes.iter().position(|entry| entry.pane_id == id))
                .unwrap_or(0);
            PaneNode::Stack(stack)
        }
        PaneNode::Split { left, right, node } => {
            let left = prune_tree(*left, keep);
            let right = prune_tree(*right, keep);
            match (left, right) {
                (PaneNode::Empty, PaneNode::Empty) => PaneNode::Empty,
                (PaneNode::Empty, kept) | (kept, PaneNode::Empty) => kept,
                (left, right) => PaneNode::Split {
                    left: Box::new(left),
                    right: Box::new(right),
                    node,
                },
            }
        }
    }
}

// ------------------------------------------------------- the new server

/// A takeover in progress on the new server: connected, introduced, and
/// holding what the old server said about itself.
pub struct Takeover {
    stream: UnixStream,
    pub hello: Hello,
    pub pid_file_fd: Option<OwnedFd>,
}

/// Connect to the running server's handoff socket and learn who it is.
/// Called before the mux exists: the runtime server id and the id
/// counters come from here.
pub fn begin(socket_path: &Path) -> anyhow::Result<Takeover> {
    let path = handoff_socket_path(socket_path);
    let mut stream = UnixStream::connect(&path).with_context(|| {
        format!(
            "connecting to {}: is a server that can hand over running?",
            path.display()
        )
    })?;
    stream.set_read_timeout(Some(HELLO_TIMEOUT))?;
    stream.set_write_timeout(Some(STEP_TIMEOUT))?;
    write_message(
        &mut stream,
        &HandoffMessage::Ready {
            version: HANDOFF_VERSION,
        },
    )?;
    let hello = match read_message(&mut stream)? {
        HandoffMessage::Hello(hello) => hello,
        HandoffMessage::Refused(why) => bail!("the running server refused: {why}"),
        other => bail!("expected Hello, got {other:?}"),
    };
    if hello.version != HANDOFF_VERSION {
        bail!(
            "the running server speaks handoff version {}, this one speaks {HANDOFF_VERSION}",
            hello.version
        );
    }
    let pid_file_fd = if hello.pid_file {
        Some(recv_owned_fd(&stream)?)
    } else {
        None
    };
    stream.set_read_timeout(Some(STEP_TIMEOUT))?;
    write_message(&mut stream, &HandoffMessage::Ack)?;
    Ok(Takeover {
        stream,
        hello,
        pid_file_fd,
    })
}

/// Panes made from what was received, on the way to being the mux's.
/// Dropped before `commit`, they release their processes and are then
/// leaked rather than dropped: a `LocalPane` sends SIGHUP on drop, and the
/// pty writer sends a newline and EOF, and the processes still belong to
/// the old server. This process is about to exit anyway.
struct Staged {
    panes: Vec<Arc<dyn Pane>>,
    committed: bool,
}

impl Staged {
    fn commit(mut self) -> Vec<Arc<dyn Pane>> {
        self.committed = true;
        std::mem::take(&mut self.panes)
    }
}

impl Drop for Staged {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        for pane in self.panes.drain(..) {
            if let Some(local) = pane.downcast_ref::<LocalPane>() {
                local.release_process();
            }
            std::mem::forget(pane);
        }
    }
}

/// A pane as received: its descriptor unopened and its snapshot decoded,
/// nothing built from them yet.
struct Received {
    pane: HandoffPane,
    fd: OwnedFd,
    snapshot: TerminalSnapshot,
    graphics: Option<wezterm_term::KittyGraphicsSnapshot>,
}

/// Receive the panes, the windows and the listening socket; only on
/// `Commit` open the ptys, build the panes, tabs and windows, register
/// them all at once, start accepting, and tell the old server it may go.
/// Until then nothing touches a pty: a failure closes duplicated
/// descriptors and no more. Returns the handoff stream: it reaches EOF
/// when the old server has exited, which is when its TCP listeners are
/// free to bind.
pub fn complete(
    takeover: Takeover,
    local_domain: DomainId,
    socket_path: &Path,
) -> anyhow::Result<UnixStream> {
    let mut stream = takeover.stream;
    let mut received: Vec<Received> = Vec::new();
    let mut windows: Vec<HandoffWindow> = Vec::new();
    let mut listener: Option<OwnedFd> = None;
    let started = Instant::now();

    loop {
        match read_message(&mut stream)? {
            HandoffMessage::Pane(pane) => {
                let pane_id = pane.pane_id;
                let fd = recv_owned_fd(&stream)?;
                fail_at("pane")?;
                let (snapshot, graphics) = decode_snapshot(&pane.snapshot)
                    .with_context(|| format!("pane {pane_id}'s snapshot"))?;
                received.push(Received { pane, fd, snapshot, graphics });
                write_message(&mut stream, &HandoffMessage::Ack)?;
            }
            HandoffMessage::Topology(list) => {
                fail_at("topology")?;
                let have: HashSet<PaneId> = received.iter().map(|r| r.pane.pane_id).collect();
                for window in &list {
                    for tab in &window.tabs {
                        let mut wanted = Vec::new();
                        pane_ids_in(&tab.tree, &mut wanted);
                        if let Some(missing) = wanted.iter().find(|id| !have.contains(id)) {
                            bail!(
                                "tab {} names pane {missing}, which was not handed over",
                                tab.tab_id
                            );
                        }
                    }
                }
                windows.extend(list);
                write_message(&mut stream, &HandoffMessage::Ack)?;
            }
            HandoffMessage::Listener => {
                listener = Some(recv_owned_fd(&stream)?);
                fail_at("listener")?;
                write_message(&mut stream, &HandoffMessage::Ack)?;
            }
            HandoffMessage::Commit => {
                fail_at("commit")?;
                let listener = listener.ok_or_else(|| anyhow!("Commit without a listener"))?;
                install(received, windows, listener, local_domain, socket_path)?;
                write_message(&mut stream, &HandoffMessage::Owned)?;
                log::info!("took over from the running server in {:?}", started.elapsed());
                return Ok(stream);
            }
            HandoffMessage::Refused(why) => bail!("the running server gave up: {why}"),
            other => bail!("unexpected handoff message {other:?}"),
        }
    }
}

/// The commit: build everything and make it the mux's.
fn install(
    received: Vec<Received>,
    windows: Vec<HandoffWindow>,
    listener: OwnedFd,
    local_domain: DomainId,
    socket_path: &Path,
) -> anyhow::Result<()> {
    let mut staged = Staged {
        panes: Vec::new(),
        committed: false,
    };
    let mut by_id: HashMap<PaneId, Arc<dyn Pane>> = HashMap::new();
    for item in received {
        let pane_id = item.pane.pane_id;
        let pane = adopt_pane(item, local_domain)
            .with_context(|| format!("adopting pane {pane_id}"))?;
        staged.panes.push(Arc::clone(&pane));
        by_id.insert(pane_id, pane);
    }
    let mut built = Vec::new();
    for window in windows {
        built.push(build_window(window, &by_id)?);
    }
    // The guard stays armed through registration: a failure there must
    // not drop what was adopted either.
    register(&staged.panes, built)?;
    staged.commit();

    unsafe { libc::fcntl(listener.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) };
    let listener = unsafe { wezterm_uds::UnixListener::from_raw_fd(listener.into_raw_fd()) };
    crate::local::remember_listener(socket_path.to_path_buf(), &listener);
    let mut listener = crate::local::LocalListener::new(listener);
    std::thread::Builder::new()
        .name("mux-listener".to_string())
        .spawn(move || listener.run())
        .context("spawning the listener thread")?;
    Ok(())
}

/// A session imported from another program keeps that program's O_NONBLOCK
/// until the next handoff. Ordinary panes read and write blocking.
fn ensure_blocking(fd: &OwnedFd) -> anyhow::Result<()> {
    let flags = rustix::fs::fcntl_getfl(fd)?;
    if flags.contains(rustix::fs::OFlags::NONBLOCK) {
        rustix::fs::fcntl_setfl(fd, flags.difference(rustix::fs::OFlags::NONBLOCK))?;
    }
    Ok(())
}

fn adopt_pane(item: Received, domain_id: DomainId) -> anyhow::Result<Arc<dyn Pane>> {
    let Received { pane, fd, snapshot, mut graphics } = item;
    ensure_blocking(&fd).context("making an adopted pty blocking")?;
    let master = portable_pty::unix::master_from_raw_fd(fd, pane.tty_name.map(PathBuf::from))?;
    let writer = master.take_writer()?;
    let snapshot_size = snapshot.size;
    let mut terminal = wezterm_term::Terminal::new(
        snapshot_size,
        Arc::new(config::TermConfig::new()),
        "ThinkTerm",
        config::wezterm_version(),
        writer,
    );
    terminal
        .restore_with_kitty_placements(snapshot, graphics.as_mut().and_then(|g| g.placements.take()))
        .context("restoring the terminal from its snapshot")?;
    if let Some(graphics) = graphics {
        // Kitty's extras accompany the snapshot. Each is checked whole before
        // it is applied, so a part that fails is left out and the pane is
        // adopted without it, rather than refusing every pane in the handoff.
        let mut restored = vec![
            terminal.restore_kitty_playback(graphics.playback).context("Kitty playback"),
            terminal.restore_kitty_virtual(graphics.virtual_images).context("Kitty virtual placements"),
        ];
        if !graphics.image_numbers.is_empty() {
            restored.push(terminal.restore_kitty_numbers(graphics.image_numbers).context("Kitty image numbers"));
        }
        if let Some(relatives) = graphics.relatives {
            restored.push(terminal.restore_kitty_relatives(relatives).context("Kitty relative placements"));
        }
        for err in restored.into_iter().filter_map(Result::err) {
            log::warn!("adopting a pane without its {err:#}");
        }
    }
    // The old server kept serving between the snapshot and Commit; a
    // resize in that window reached the pty but not the snapshot.
    if let Ok(size) = master.get_size() {
        let current = TerminalSize {
            rows: size.rows as usize,
            cols: size.cols as usize,
            pixel_width: size.pixel_width as usize,
            pixel_height: size.pixel_height as usize,
            dpi: snapshot_size.dpi,
        };
        if current.rows > 0
            && current.cols > 0
            && (current.rows != snapshot_size.rows || current.cols != snapshot_size.cols)
        {
            terminal.resize(current);
        }
    }
    let pane_writer = terminal.writer_handle();
    let local = LocalPane::new(
        pane.pane_id,
        terminal,
        portable_pty::adopted_child(pane.pid),
        master,
        pane_writer,
        domain_id,
        pane.description,
    );
    Ok(Arc::new(local))
}

fn build_window(
    window: HandoffWindow,
    staged: &HashMap<PaneId, Arc<dyn Pane>>,
) -> anyhow::Result<(Window, Vec<Arc<Tab>>, usize)> {
    let mut built = Window::new_with_id(window.window_id, window.workspace, None);
    built.set_title(&window.title);
    let mut tabs = Vec::new();
    for tab in window.tabs {
        let mut wanted = Vec::new();
        pane_ids_in(&tab.tree, &mut wanted);
        if let Some(missing) = wanted.iter().find(|id| !staged.contains_key(id)) {
            bail!(
                "tab {} names pane {missing}, which was not handed over",
                tab.tab_id
            );
        }
        let built_tab = Arc::new(Tab::new_with_id(&tab.size, tab.tab_id));
        built_tab.set_title(&tab.title);
        built_tab.sync_with_pane_tree(tab.size, tab.tree, |entry| {
            Arc::clone(&staged[&entry.pane_id])
        });
        tabs.push(built_tab);
    }
    Ok((built, tabs, window.active_tab))
}

fn pane_ids_in(node: &PaneNode, out: &mut Vec<PaneId>) {
    match node {
        PaneNode::Empty => {}
        PaneNode::Leaf(entry) => out.push(entry.pane_id),
        PaneNode::Stack(stack) => out.extend(stack.panes.iter().map(|entry| entry.pane_id)),
        PaneNode::Split { left, right, .. } => {
            pane_ids_in(left, out);
            pane_ids_in(right, out);
        }
    }
}

/// Make the staged panes, tabs and windows the mux's. This is the moment
/// the pane readers start, so it happens once, after everything arrived.
fn register(
    staged: &[Arc<dyn Pane>],
    windows: Vec<(Window, Vec<Arc<Tab>>, usize)>,
) -> anyhow::Result<()> {
    let mux = Mux::get();
    for pane in staged {
        mux.add_pane(pane)
            .with_context(|| format!("registering pane {}", pane.pane_id()))?;
    }
    for (window, tabs, active_tab) in windows {
        let builder = mux.insert_window(window);
        let window_id = *builder;
        for tab in &tabs {
            mux.add_tab_no_panes(tab);
            mux.add_tab_to_window(tab, window_id)?;
        }
        if let Some(mut window) = mux.get_window_mut(window_id) {
            window.set_active_without_saving(active_tab.min(tabs.len().saturating_sub(1)));
        }
    }
    Ok(())
}

/// Blocks until the old server has closed its end: it is gone.
pub fn wait_for_predecessor_exit(mut stream: UnixStream) {
    stream.set_read_timeout(None).ok();
    let mut buf = [0u8; 64];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => return,
            Ok(_) => {}
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_adopted_nonblocking_descriptor_becomes_blocking() {
        let (end, _peer) = UnixStream::pair().unwrap();
        end.set_nonblocking(true).unwrap();
        let fd = OwnedFd::from(end);
        ensure_blocking(&fd).unwrap();
        let flags = rustix::fs::fcntl_getfl(&fd).unwrap();
        assert!(!flags.contains(rustix::fs::OFlags::NONBLOCK));
    }
    use thinkterm_proto::{PaneEntry, PaneStackEntry, SplitDirection, SplitDirectionAndSize};

    fn entry(pane_id: PaneId) -> PaneEntry {
        PaneEntry {
            window_id: 0,
            tab_id: 0,
            pane_id,
            title: String::new(),
            size: TerminalSize::default(),
            working_dir: None,
            is_active_pane: false,
            is_zoomed_pane: false,
            alt_screen: false,
            workspace: String::new(),
            cursor_pos: Default::default(),
            physical_top: 0,
            top_row: 0,
            left_col: 0,
            tty_name: None,
        }
    }

    fn split(left: PaneNode, right: PaneNode) -> PaneNode {
        PaneNode::Split {
            left: Box::new(left),
            right: Box::new(right),
            node: SplitDirectionAndSize {
                direction: SplitDirection::Horizontal,
                first: TerminalSize::default(),
                second: TerminalSize::default(),
            },
        }
    }

    #[test]
    fn pruning_keeps_the_panes_that_travel_and_collapses_the_rest() {
        let tree = split(
            PaneNode::Leaf(entry(1)),
            split(
                PaneNode::Stack(PaneStackEntry {
                    active: 1,
                    panes: vec![entry(2), entry(3)],
                    pane_stack_id: Some(9),
                }),
                PaneNode::Leaf(entry(4)),
            ),
        );
        let keep: HashSet<PaneId> = [1usize, 3].iter().copied().collect();
        let pruned = prune_tree(tree, &keep);
        let mut ids = Vec::new();
        pane_ids_in(&pruned, &mut ids);
        assert_eq!(ids, [1, 3]);
        match &pruned {
            PaneNode::Split { right, .. } => match &**right {
                PaneNode::Stack(stack) => {
                    assert_eq!(stack.active, 0, "the active pane followed pane 3");
                    assert_eq!(stack.pane_stack_id, Some(9));
                }
                other => panic!("the right side should be the stack alone, got {:?}", other),
            },
            other => panic!("expected a split, got {:?}", other),
        }
        assert!(matches!(
            prune_tree(PaneNode::Leaf(entry(4)), &keep),
            PaneNode::Empty
        ));
    }

    #[test]
    fn messages_and_snapshots_survive_their_bytes() {
        let (mut a, mut b) = UnixStream::pair().unwrap();
        write_message(
            &mut a,
            &HandoffMessage::Ready {
                version: HANDOFF_VERSION,
            },
        )
        .unwrap();
        match read_message(&mut b).unwrap() {
            HandoffMessage::Ready { version } => assert_eq!(version, HANDOFF_VERSION),
            other => panic!("{:?}", other),
        }

        let mut term = wezterm_term::Terminal::new(
            TerminalSize {
                rows: 4,
                cols: 10,
                pixel_width: 80,
                pixel_height: 64,
                dpi: 96,
            },
            Arc::new(config::TermConfig::new()),
            "ThinkTerm",
            "test",
            Box::new(Vec::new()),
        );
        term.advance_bytes("hello");
        let snapshot = term.snapshot();
        let mut graphics = term.snapshot_kitty_graphics();
        let bytes = encode_snapshot(&snapshot, &graphics).unwrap();
        assert_eq!(decode_snapshot(&bytes).unwrap(), (snapshot, None));
        let snapshot = term.snapshot();
        graphics.playback.selections.push(wezterm_term::kitty_animation::KittyPlaybackEntry {
            image_id: 1,
            data_hash: [7; 32],
            animation: wezterm_term::kitty_animation::KittyAnimation::new([0, 40], 100),
        });
        let bytes = encode_snapshot(&snapshot, &graphics).unwrap();
        let decoded = decode_snapshot(&bytes).unwrap();
        assert_eq!(decoded, (term.snapshot(), Some(graphics.clone())));
        // Exactly the predecessor's decoder: it consumes the same prefix
        // and can adopt the terminal without understanding this extension.
        let raw = zstd::decode_all(&bytes[..]).unwrap();
        let mut reader = &raw[..];
        let old = TerminalSnapshot::deserialize(&mut varbincode::Deserializer::new(&mut reader)).unwrap();
        assert_eq!(old, snapshot);
        assert!(reader.starts_with(KITTY_PLAYBACK_TAG));
        let mut malformed = raw;
        malformed.push(0);
        assert!(decode_snapshot(&zstd::encode_all(&malformed[..], 3).unwrap()).is_err());

        let virtuals = vec![wezterm_term::kitty_virtual::VirtualImage {
            image_id: 7, data_hash: [4; 32],
            placements: vec![wezterm_term::kitty_virtual::VirtualPlacement {
                placement_id: 3, columns: 8, rows: 4,
            }],
        }];
        // Version 2 carries the same playback value plus virtual grids. Test
        // animated and static-only virtual images, and legacy prefix readers.
        for animated in [true, false] {
            if !animated { graphics.playback.selections.clear(); }
            graphics.virtual_images = virtuals.clone();
            let bytes = encode_snapshot(&snapshot, &graphics).unwrap();
            assert_eq!(decode_snapshot(&bytes).unwrap(), (term.snapshot(), Some(graphics.clone())));
            let raw = zstd::decode_all(&bytes[..]).unwrap();
            let mut reader = &raw[..];
            let old = TerminalSnapshot::deserialize(&mut varbincode::Deserializer::new(&mut reader)).unwrap();
            assert_eq!(old, snapshot);
            assert!(reader.starts_with(KITTY_VIRTUAL_TAG));
            let mut trailing = raw.clone(); trailing.push(0);
            assert!(decode_snapshot(&zstd::encode_all(&trailing[..], 3).unwrap()).is_err());
            assert!(decode_snapshot(&zstd::encode_all(&raw[..raw.len()-1], 3).unwrap()).is_err());
        }
        for with_virtuals in [true, false] {
            if !with_virtuals { graphics.virtual_images.clear(); }
            graphics.image_numbers = vec![(12, 1), (12, 2), (13, 3)];
            let bytes = encode_snapshot(&snapshot, &graphics).unwrap();
            assert_eq!(decode_snapshot(&bytes).unwrap(), (term.snapshot(), Some(graphics.clone())));
            let raw = zstd::decode_all(&bytes[..]).unwrap();
            let mut reader = &raw[..];
            let old = TerminalSnapshot::deserialize(&mut varbincode::Deserializer::new(&mut reader)).unwrap();
            assert_eq!(old, snapshot);
            assert!(reader.starts_with(KITTY_NUMBERS_TAG));
            let mut trailing = raw.clone(); trailing.push(0);
            assert!(decode_snapshot(&zstd::encode_all(&trailing[..], 3).unwrap()).is_err());
            assert!(decode_snapshot(&zstd::encode_all(&raw[..raw.len()-1], 3).unwrap()).is_err());
        }
        graphics.placements = Some(wezterm_term::KittyPlacementSnapshot {
            next_id: u64::from(u32::MAX) + 1,
            placements: Vec::new(),
            cell_tags: [Vec::new(), Vec::new()],
        });
        let bytes = encode_snapshot(&snapshot, &graphics).unwrap();
        assert_eq!(decode_snapshot(&bytes).unwrap(), (term.snapshot(), Some(graphics)));
        let raw = zstd::decode_all(&bytes[..]).unwrap();
        let mut reader = &raw[..];
        let old = TerminalSnapshot::deserialize(&mut varbincode::Deserializer::new(&mut reader)).unwrap();
        assert_eq!(old, snapshot);
        assert!(reader.starts_with(KITTY_PLACEMENTS_TAG));
        assert!(decode_snapshot(&zstd::encode_all(&raw[..raw.len()-1], 3).unwrap()).is_err());
        let mut trailing = raw; trailing.push(0);
        assert!(decode_snapshot(&zstd::encode_all(&trailing[..], 3).unwrap()).is_err());
    }

    #[test]
    fn relative_handoff_preserves_the_legacy_prefix_and_rebuilds_anonymous_parent_origins() {
        let make_term = || wezterm_term::Terminal::new(
            TerminalSize { rows: 8, cols: 20, pixel_width: 160, pixel_height: 128, dpi: 96 },
            Arc::new(config::TermConfig::new()), "ThinkTerm", "test", Box::new(Vec::new()));
        let mut terminal = make_term();
        terminal.advance_bytes("\x1b[2;4H\x1b_Ga=T,f=24,s=1,v=1,i=1,p=0,c=2,r=2,C=1;/wAA\x1b\\");
        terminal.advance_bytes("\x1b_Ga=t,f=24,s=1,v=1,i=2;AP8A\x1b\\");
        terminal.advance_bytes("\x1b_Ga=p,i=2,p=5,P=1,H=3,V=2,c=2,r=2\x1b\\");
        let expected = terminal.kitty_frame_selections(None).unwrap().1.into_iter()
            .find(|s| s.image_id == 2).unwrap().relative_placements;
        assert_eq!(expected.len(), 1);
        for _ in 0..2 {
            let snapshot = terminal.snapshot();
            let graphics = terminal.snapshot_kitty_graphics();
            let bytes = encode_snapshot(&snapshot, &graphics).unwrap();
            let (decoded, state) = decode_snapshot(&bytes).unwrap();
            assert_eq!(decoded, snapshot);
            assert_eq!(state.as_ref(), Some(&graphics));
            let raw = zstd::decode_all(&bytes[..]).unwrap();
            let mut reader = &raw[..];
            let legacy = TerminalSnapshot::deserialize(&mut varbincode::Deserializer::new(&mut reader)).unwrap();
            assert_eq!(legacy, snapshot);
            assert!(reader.starts_with(KITTY_RELATIVES_TAG));
            assert!(decode_snapshot(&zstd::encode_all(&raw[..raw.len() - 1], 3).unwrap()).is_err());
            let mut trailing = raw; trailing.push(0);
            assert!(decode_snapshot(&zstd::encode_all(&trailing[..], 3).unwrap()).is_err());
            let state = state.unwrap();
            let mut next = make_term();
            next.restore_with_kitty_placements(decoded, state.placements).unwrap();
            next.restore_kitty_relatives(state.relatives.unwrap()).unwrap();
            let actual = next.kitty_frame_selections(None).unwrap().1.into_iter()
                .find(|s| s.image_id == 2).unwrap().relative_placements;
            assert_eq!(actual, expected);
            terminal = next;
        }
        terminal.advance_bytes("\x1b_Ga=d,d=i,i=1\x1b\\");
        assert!(terminal.kitty_frame_selections(None).unwrap().1.iter().all(|s| s.image_id != 2));
    }

    #[test]
    fn the_handoff_socket_sits_next_to_the_socket() {
        assert_eq!(
            handoff_socket_path(Path::new("/run/user/1000/thinkterm/sock")),
            PathBuf::from("/run/user/1000/thinkterm/sock.handoff")
        );
    }
}
